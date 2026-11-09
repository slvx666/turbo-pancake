//! Throw-away PostgreSQL for tests (S5, ADR 0013).
//!
//! The first call of [`database_url`] in a test process creates a cluster under the
//! build directory (`<target>/<profile>/pg-test/pg-<pid>`), starts it on a free loopback
//! port with durability off, and returns a fresh database; later calls return another
//! fresh database in the same cluster, so tests stay isolated and run in parallel.
//!
//! Binaries: `TURBO_PANCAKE_PG_BIN` (directory with `initdb` and `postgres`), else
//! `PGBIN` (set on GitHub's Windows runners), else the first `initdb` on `PATH`.
//! On Windows the test process puts itself into a job object that kills its children
//! when it exits, so a test run never leaves a PostgreSQL server behind.
//!
//! Without binaries the call panics with instructions, unless `TURBO_PANCAKE_SKIP_PG=1`
//! is set: then it returns `None` and the caller skips the test (and says so).

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

struct Cluster {
    port: u16,
    bin: PathBuf,
    dir: PathBuf,
    options: String,
}

/// Started once per process; a failure is remembered so every test fails at once with
/// the same message instead of retrying the start.
static CLUSTER: OnceLock<Result<Option<Cluster>, String>> = OnceLock::new();
static NEXT_DB: AtomicU32 = AtomicU32::new(1);

/// URL of a new, empty database (`postgres://postgres@127.0.0.1:<port>/<name>`), or `None`
/// when PostgreSQL is unavailable and `TURBO_PANCAKE_SKIP_PG` is set.
pub fn database_url() -> Option<String> {
    let cluster = match CLUSTER.get_or_init(start) {
        Ok(c) => c.as_ref()?,
        Err(e) => panic!("{e}"),
    };
    let name = format!(
        "t{}_{}",
        std::process::id(),
        NEXT_DB.fetch_add(1, Ordering::Relaxed)
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = Command::new(cluster.bin.join(exe("createdb")))
            .args(["-h", "127.0.0.1", "-U", "postgres", "-p"])
            .arg(cluster.port.to_string())
            .arg(&name)
            .stdin(Stdio::null())
            .output()
            .expect("run createdb");
        if out.status.success() {
            break;
        }
        // Right after start the server answers "the database system is starting up".
        assert!(
            Instant::now() < deadline,
            "createdb failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Some(format!(
        "postgres://postgres@127.0.0.1:{}/{name}",
        cluster.port
    ))
}

/// Port of the running cluster (after the first [`database_url`]).
pub fn port() -> Option<u16> {
    match CLUSTER.get() {
        Some(Ok(Some(c))) => Some(c.port),
        _ => None,
    }
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn bin_dir() -> Option<PathBuf> {
    let has_initdb = |p: &Path| p.join(exe("initdb")).is_file();
    for var in ["TURBO_PANCAKE_PG_BIN", "PGBIN"] {
        if let Some(v) = std::env::var_os(var) {
            let p = PathBuf::from(v);
            if has_initdb(&p) {
                return Some(p);
            }
        }
    }
    std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).find(|p| has_initdb(p)))
}

fn start() -> Result<Option<Cluster>, String> {
    let Some(bin) = bin_dir() else {
        if std::env::var_os("TURBO_PANCAKE_SKIP_PG").is_some() {
            eprintln!("PostgreSQL binaries not found: test skipped (TURBO_PANCAKE_SKIP_PG)");
            return Ok(None);
        }
        return Err(
            "PostgreSQL binaries not found: set TURBO_PANCAKE_PG_BIN to a directory with \
             initdb and postgres (see docs/TESTING.md), or TURBO_PANCAKE_SKIP_PG=1 to skip"
                .into(),
        );
    };
    kill_children_on_exit();

    // <target>/<profile>/deps/<test>.exe -> <target>/<profile>/pg-test
    let exe_path = std::env::current_exe().expect("current exe");
    let base = exe_path
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("."))
        .join("pg-test");
    std::fs::create_dir_all(&base).expect("create pg-test dir");
    remove_stale(&base);
    let dir = base.join(format!("pg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let out = Command::new(bin.join(exe("initdb")))
        .arg("-D")
        .arg(&dir)
        .args([
            "-U",
            "postgres",
            "-A",
            "trust",
            "-E",
            "UTF8",
            "--no-locale",
            "--no-sync",
            "--no-instructions",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("run initdb: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "initdb failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }

    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("free port")
        .port();
    // pg_ctl, not postgres directly: started by an administrator (CI runners) postgres
    // refuses to run, pg_ctl drops the privileges with a restricted token. The server
    // stays in this process's job object, so it still dies with the test process.
    let options = format!(
        "-h 127.0.0.1 -p {port} -c fsync=off -c synchronous_commit=off \
         -c full_page_writes=off -c max_connections=300"
    );
    pg_start(&bin, &dir, port, &options)?;
    Ok(Some(Cluster {
        port,
        bin,
        dir,
        options,
    }))
}

fn pg_start(bin: &Path, dir: &Path, port: u16, options: &str) -> Result<(), String> {
    let log = dir.join("server.log");
    let ctl_log = dir.join("pg_ctl.log");
    let out = Command::new(bin.join(exe("pg_ctl")))
        .arg("start")
        .arg("-D")
        .arg(dir)
        .arg("-l")
        .arg(&log)
        .args(["-w", "-t", "60", "-o", options])
        // Files, not pipes: the server inherits pg_ctl's handles, so a pipe would never
        // reach end-of-file and `output()` would wait forever.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(
            std::fs::File::create(&ctl_log)
                .map(Stdio::from)
                .map_err(|e| format!("pg_ctl log: {e}"))?,
        )
        .status()
        .map_err(|e| format!("run pg_ctl: {e}"))?;
    let failed = |why: &str| {
        format!(
            "postgres did not start: {why} {}\n--- server.log ---\n{}",
            std::fs::read_to_string(&ctl_log).unwrap_or_default().trim(),
            std::fs::read_to_string(&log).unwrap_or_default()
        )
    };
    if !out.success() {
        return Err(failed("pg_ctl failed"));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        if Instant::now() >= deadline {
            return Err(failed("port not listening"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// Stops this process's cluster (`pg_ctl stop -m fast`): a database outage for tests.
/// Every database of the process goes away with it, so only a test binary whose tests
/// all expect that may call this; [`restart`] brings it back.
pub fn stop() {
    let Some(Ok(Some(c))) = CLUSTER.get() else {
        panic!("pg-test: no running cluster to stop");
    };
    let status = Command::new(c.bin.join(exe("pg_ctl")))
        .arg("stop")
        .arg("-D")
        .arg(&c.dir)
        .args(["-m", "fast", "-w", "-t", "60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run pg_ctl stop");
    assert!(status.success(), "pg_ctl stop failed");
}

/// Starts the cluster stopped by [`stop`] again, same port and data.
pub fn restart() {
    let Some(Ok(Some(c))) = CLUSTER.get() else {
        panic!("pg-test: no cluster to restart");
    };
    pg_start(&c.bin, &c.dir, c.port, &c.options).unwrap_or_else(|e| panic!("{e}"));
}

/// Deletes clusters of earlier runs. A directory still in use cannot be renamed on
/// Windows (open files), so a running cluster of a parallel test process is left alone.
fn remove_stale(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let ours = format!("pg-{}", std::process::id());
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("pg-") || name.starts_with("stale-")) || name == ours {
            continue;
        }
        let trash = base.join(format!("stale-{name}"));
        let target = if name.starts_with("stale-") {
            e.path()
        } else if std::fs::rename(e.path(), &trash).is_ok() {
            trash
        } else {
            continue;
        };
        let _ = std::fs::remove_dir_all(target);
    }
}

/// Puts this process into a job object that kills every process in it when the last
/// handle closes, i.e. when this process exits however it exits.
#[cfg(windows)]
#[allow(unsafe_code)]
fn kill_children_on_exit() {
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    use windows::core::PCWSTR;

    // SAFETY: plain Win32 calls with valid arguments; the job handle is deliberately
    // never closed (it must live as long as the process).
    unsafe {
        let Ok(job) = CreateJobObjectW(None, PCWSTR::null()) else {
            eprintln!("pg-test: CreateJobObjectW failed; PostgreSQL may outlive the test");
            return;
        };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if set.is_err() || AssignProcessToJobObject(job, GetCurrentProcess()).is_err() {
            eprintln!("pg-test: job object setup failed; PostgreSQL may outlive the test");
        }
    }
}

#[cfg(not(windows))]
fn kill_children_on_exit() {
    // S9: PR_SET_PDEATHSIG on Linux, a watchdog on macOS.
}

#[cfg(test)]
mod tests {
    #[test]
    fn two_databases_in_one_cluster() {
        let Some(a) = super::database_url() else {
            return;
        };
        let b = super::database_url().unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("postgres://postgres@127.0.0.1:"));
    }
}
