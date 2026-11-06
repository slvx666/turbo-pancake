//! turbo-pancake benchmark harness.
//!
//! Every scenario writes a self-describing JSON report (host, parameters, raw
//! samples, summary) into `benchmarks/results/`. See `benchmarks/README.md`.

mod mem;
mod messenger;
mod sfu;
mod sfu_screen;

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;
use turbo_pancake_protocol::v1::{self, client_envelope::Body as C, server_envelope::Body as S};

#[derive(Parser)]
#[command(about = "turbo-pancake benchmark harness")]
struct Cli {
    /// Directory for JSON reports.
    #[arg(long, default_value = "benchmarks/results")]
    out_dir: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Cold/warm start to first rendered frame + idle memory of the desktop client.
    DesktopStartup {
        #[arg(long)]
        exe: PathBuf,
        #[arg(long, default_value_t = 5)]
        runs: u32,
        /// Time after the first frame before memory is sampled.
        #[arg(long, default_value_t = 3000)]
        settle_ms: u64,
        /// Free-form label stored in the report (e.g. "femtovg", "software").
        #[arg(long, default_value = "default")]
        label: String,
        /// Extra environment for the client, KEY=VALUE (repeatable).
        #[arg(long = "env", value_parser = parse_kv)]
        envs: Vec<(String, String)>,
        /// Extra argument passed to the client (repeatable), e.g.
        /// `--client-arg=--window-pos --client-arg=2020,100 --client-arg=--no-activate`.
        #[arg(long = "client-arg", allow_hyphen_values = true)]
        client_args: Vec<String>,
    },
    /// Signaling relay latency, server RTT and memory with idle connections.
    Signaling {
        #[arg(long)]
        server_exe: PathBuf,
        #[arg(long, default_value_t = 2000)]
        messages: u32,
        #[arg(long, default_value_t = 500)]
        idle_connections: u32,
    },
    /// S8: N signed-in WebSocket sessions with presence, groups and real PostgreSQL.
    Messenger {
        #[arg(long)]
        server_exe: PathBuf,
        #[arg(long, default_value_t = 500)]
        users: usize,
        /// Members per group (≤ 100); every user is in one group.
        #[arg(long, default_value_t = 100)]
        group_size: usize,
        /// Users that disconnect and reconnect once each (presence fan-out).
        #[arg(long, default_value_t = 20)]
        churn: usize,
        /// Messages posted to the largest group (fan-out to every other member).
        #[arg(long, default_value_t = 20)]
        messages: usize,
        /// Idle hold with heartbeats; server memory/CPU sampled every second.
        #[arg(long, default_value_t = 30)]
        hold_seconds: u64,
        /// Connections that stop sending (heartbeat timeout must mark them offline).
        #[arg(long, default_value_t = 10)]
        silent: usize,
        /// Parallel registrations (each costs one Argon2id hash on the server).
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        /// PostgreSQL of the server; default: a throw-away cluster (`crates/pg-test`,
        /// `TURBO_PANCAKE_PG_BIN`). Use a dedicated empty database.
        #[arg(long, env = "TURBO_PANCAKE_BENCH_DATABASE_URL", hide_env_values = true)]
        database_url: Option<String>,
    },
    /// SFU forwarding: N media members, server CPU and throughput per number of talkers.
    Sfu {
        #[arg(long)]
        server_exe: PathBuf,
        #[arg(long, default_value_t = 20)]
        members: usize,
        /// Members talking at once, one measured phase each (comma-separated).
        #[arg(long, value_delimiter = ',', default_value = "1,3,10,20")]
        talkers: Vec<usize>,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        /// Payload bytes per 20 ms frame (100 ≈ Opus voice at 40 kbit/s).
        #[arg(long, default_value_t = 100)]
        payload: usize,
    },
    /// S4 screen share load: `publishers` (N share, 2 watch + preview all), `fanout`
    /// (1 shares, N watch high), `grid` (N share, each previews all others).
    SfuScreen {
        #[arg(long)]
        server_exe: PathBuf,
        #[arg(long, value_enum)]
        scenario: sfu_screen::Scenario,
        #[arg(long, default_value_t = 50)]
        n: usize,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long, default_value_t = 2000)]
        high_kbps: u32,
        #[arg(long, default_value_t = 30)]
        high_fps: u32,
        #[arg(long, default_value_t = 150)]
        preview_kbps: u32,
        #[arg(long, default_value_t = 10)]
        preview_fps: u32,
    },
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .ok_or_else(|| format!("expected KEY=VALUE, got {s}"))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let report = match cli.cmd {
        Cmd::DesktopStartup {
            exe,
            runs,
            settle_ms,
            label,
            envs,
            client_args,
        } => desktop_startup(&exe, runs, settle_ms, &label, &envs, &client_args)?,
        Cmd::Signaling {
            server_exe,
            messages,
            idle_connections,
        } => tokio::runtime::Runtime::new()?.block_on(signaling(
            &server_exe,
            messages,
            idle_connections,
        ))?,
        Cmd::Messenger {
            server_exe,
            users,
            group_size,
            churn,
            messages,
            hold_seconds,
            silent,
            concurrency,
            database_url,
        } => tokio::runtime::Runtime::new()?.block_on(messenger::messenger(
            &server_exe,
            messenger::Params {
                users,
                group_size,
                churn,
                messages,
                hold_seconds,
                silent,
                concurrency,
                database_url,
            },
        ))?,
        Cmd::Sfu {
            server_exe,
            members,
            talkers,
            seconds,
            payload,
        } => tokio::runtime::Runtime::new()?.block_on(sfu::sfu(
            &server_exe,
            members,
            &talkers,
            seconds,
            payload,
        ))?,
        Cmd::SfuScreen {
            server_exe,
            scenario,
            n,
            seconds,
            high_kbps,
            high_fps,
            preview_kbps,
            preview_fps,
        } => tokio::runtime::Runtime::new()?.block_on(sfu_screen::sfu_screen(
            &server_exe,
            sfu_screen::Params {
                scenario,
                n,
                seconds,
                high_kbps,
                high_fps,
                preview_kbps,
                preview_fps,
            },
        ))?,
    };
    std::fs::create_dir_all(&cli.out_dir)?;
    let scenario = report["scenario"].as_str().unwrap_or("unknown").to_owned();
    let label = report["label"].as_str().unwrap_or("").to_owned();
    let name = format!(
        "{scenario}{}{}-{}-{}.json",
        if label.is_empty() { "" } else { "-" },
        label,
        std::env::consts::OS,
        unix_now()
    );
    let path = cli.out_dir.join(name);
    std::fs::write(&path, serde_json::to_string_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report["summary"])?);
    println!("report written to {}", path.display());
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn host_info() -> serde_json::Value {
    use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
    let sys = System::new_with_specifics(
        RefreshKind::nothing()
            .with_cpu(CpuRefreshKind::nothing())
            .with_memory(MemoryRefreshKind::everything()),
    )
    .ok();
    let git = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "os_version": System::long_os_version().ok(),
        "cpu": sys.as_ref().and_then(|s| s.cpus().first()).map(|c| c.brand().trim().to_owned()),
        "logical_cpus": sys.as_ref().map(|s| s.cpus().len()),
        "total_memory_bytes": sys.as_ref().map(System::total_memory),
        "git_commit": git,
    })
}

#[derive(Serialize, Clone, Copy)]
struct Stats {
    min: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
    n: usize,
}

fn stats(mut v: Vec<f64>) -> Option<Stats> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let pick = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    Some(Stats {
        min: v[0],
        p50: pick(0.50),
        p95: pick(0.95),
        p99: pick(0.99),
        max: v[v.len() - 1],
        n: v.len(),
    })
}

const MIB: f64 = 1024.0 * 1024.0;

/// Last lines of a child's stderr, kept for diagnostics when it misbehaves.
type StderrTail = Arc<Mutex<VecDeque<String>>>;

const STDERR_TAIL_LINES: usize = 40;

/// Spawns a child with piped stdout/stderr; stdout lines are forwarded over a
/// channel, the tail of stderr is retained for error reports.
fn spawn_lines(mut cmd: Command) -> anyhow::Result<(Child, mpsc::Receiver<String>, StderrTail)> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {cmd:?}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let tail: StderrTail = Arc::default();
    let tail_w = tail.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut t = tail_w.lock().expect("stderr tail lock");
            if t.len() == STDERR_TAIL_LINES {
                t.pop_front();
            }
            t.push_back(line);
        }
    });
    Ok((child, rx, tail))
}

fn wait_line(
    rx: &mpsc::Receiver<String>,
    prefix: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(l) if l.starts_with(prefix) => return Ok(l),
            Ok(_) => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => bail!("timed out waiting for `{prefix}`"),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("process closed stdout (exited?) before printing `{prefix}`")
            }
        }
    }
}

/// Adds exit status and stderr tail of `child` to an error.
fn with_child_diag(err: anyhow::Error, child: &mut Child, tail: &StderrTail) -> anyhow::Error {
    // Give the stderr reader a moment to drain after the process died.
    std::thread::sleep(Duration::from_millis(200));
    let status = match child.try_wait() {
        Ok(Some(s)) => s.to_string(),
        Ok(None) => "still running".to_owned(),
        Err(e) => format!("unknown ({e})"),
    };
    let stderr = tail
        .lock()
        .map(|t| {
            t.iter().cloned().collect::<Vec<_>>().join(
                "
",
            )
        })
        .unwrap_or_default();
    err.context(format!(
        "child status: {status}
--- child stderr (tail) ---
{stderr}"
    ))
}

fn desktop_startup(
    exe: &Path,
    runs: u32,
    settle_ms: u64,
    label: &str,
    envs: &[(String, String)],
    client_args: &[String],
) -> anyhow::Result<serde_json::Value> {
    let exe_bytes = std::fs::metadata(exe)
        .with_context(|| {
            format!(
                "{} not found; build with `cargo build --release -p turbo-pancake-desktop`",
                exe.display()
            )
        })?
        .len();
    let mut samples = Vec::new();
    for run in 0..runs {
        let mut cmd = Command::new(exe);
        cmd.arg("--smoke-exit-after-ms")
            .arg((settle_ms + 1500).to_string())
            .args(client_args)
            .envs(envs.iter().cloned());
        let t0 = Instant::now();
        let (mut child, rx, tail) = spawn_lines(cmd)?;
        let line = match wait_line(&rx, "TURBO_PANCAKE_FIRST_FRAME", Duration::from_secs(30)) {
            Ok(l) => l,
            Err(e) => {
                let err = with_child_diag(e, &mut child, &tail);
                let _ = child.kill();
                return Err(err);
            }
        };
        let first_frame_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut parts = line.split_whitespace().skip(1);
        let in_app_ms: f64 = parts
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(f64::NAN);
        let renderer = parts.next().unwrap_or("?").to_owned();

        std::thread::sleep(Duration::from_millis(settle_ms));
        let m = mem::sample(child.id())?;
        let exit_seen = wait_line(&rx, "TURBO_PANCAKE_EXIT", Duration::from_secs(15)).is_ok();
        let status = child.wait()?;
        println!(
            "run {run}: first_frame={first_frame_ms:.0}ms (in-app {in_app_ms:.0}ms, {renderer}) ws={:.1}MiB private={}",
            m.working_set_bytes as f64 / MIB,
            m.private_bytes
                .map_or("n/a".into(), |p| format!("{:.1}MiB", p as f64 / MIB)),
        );
        samples.push(json!({
            "run": run,
            "first_frame_ms": first_frame_ms,
            "in_app_first_frame_ms": in_app_ms,
            "renderer": renderer,
            "memory": m,
            "clean_exit": exit_seen && status.success(),
        }));
    }
    let col = |f: &dyn Fn(&serde_json::Value) -> Option<f64>| -> Option<Stats> {
        stats(samples.iter().filter_map(f).collect())
    };
    let summary = json!({
        "first_frame_ms": col(&|s| s["first_frame_ms"].as_f64()),
        "working_set_mib": col(&|s| s["memory"]["working_set_bytes"].as_f64().map(|b| b / MIB)),
        "private_mib": col(&|s| s["memory"]["private_bytes"].as_f64().map(|b| b / MIB)),
        "exe_mib": exe_bytes as f64 / MIB,
        "all_clean_exit": samples.iter().all(|s| s["clean_exit"] == true),
        // The first run after a build is the closest thing to a cold start we can
        // automate without flushing the OS file cache; later runs are warm.
        "first_run_ms": samples.first().map(|s| s["first_frame_ms"].clone()),
    });
    Ok(json!({
        "scenario": "desktop-startup",
        "label": label,
        "host": host_info(),
        "params": { "exe": exe.display().to_string(), "runs": runs, "settle_ms": settle_ms, "env": envs, "client_args": client_args },
        "samples": samples,
        "summary": summary,
    }))
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_send(ws: &mut Ws, rid: u64, body: C) -> anyhow::Result<()> {
    let bytes = turbo_pancake_protocol::encode(&turbo_pancake_protocol::client(rid, body));
    ws.send(Message::Binary(bytes.into())).await?;
    Ok(())
}

async fn ws_recv(ws: &mut Ws) -> anyhow::Result<S> {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .context("timeout")?
            .context("closed")??;
        if let Message::Binary(b) = msg {
            return turbo_pancake_protocol::decode_server(&b)?
                .body
                .context("empty body");
        }
    }
}

async fn ws_hello(addr: &str, name: &str) -> anyhow::Result<(Ws, String)> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/signal")).await?;
    ws_send(
        &mut ws,
        1,
        C::Hello(v1::Hello {
            client_version: "turbo-pancake-bench".into(),
            display_name: name.into(),
            ..Default::default()
        }),
    )
    .await?;
    match ws_recv(&mut ws).await? {
        S::Welcome(w) => Ok((ws, w.peer_id)),
        other => bail!("expected Welcome, got {other:?}"),
    }
}

async fn signaling(
    server_exe: &Path,
    messages: u32,
    idle_connections: u32,
) -> anyhow::Result<serde_json::Value> {
    let mut cmd = Command::new(server_exe);
    cmd.args(["--bind", "127.0.0.1:0"]).env("RUST_LOG", "warn");
    let (mut child, rx, tail) = spawn_lines(cmd)?;
    let result = signaling_inner(&child, &rx, messages, idle_connections).await;
    let result = result.map_err(|e| with_child_diag(e, &mut child, &tail));
    let _ = child.kill();
    let _ = child.wait();
    let (summary, raw) = result?;
    Ok(json!({
        "scenario": "signaling",
        "label": "",
        "host": host_info(),
        "params": { "messages": messages, "idle_connections": idle_connections },
        "raw": raw,
        "summary": summary,
    }))
}

async fn signaling_inner(
    child: &Child,
    rx: &mpsc::Receiver<String>,
    messages: u32,
    idle_connections: u32,
) -> anyhow::Result<(serde_json::Value, serde_json::Value)> {
    let line = wait_line(rx, "TURBO_PANCAKE_LISTENING", Duration::from_secs(10))?;
    let addr = line
        .split_whitespace()
        .nth(1)
        .context("no addr")?
        .to_owned();
    let pid = child.id();
    let mem_start = mem::sample(pid)?;

    let (mut a, _) = ws_hello(&addr, "bench-a").await?;
    let (mut b, b_id) = ws_hello(&addr, "bench-b").await?;
    for ws in [&mut a, &mut b] {
        ws_send(
            ws,
            2,
            C::JoinRoom(v1::JoinRoom {
                room_id: "bench".into(),
            }),
        )
        .await?;
        loop {
            if let S::RoomJoined(_) = ws_recv(ws).await? {
                break;
            }
        }
    }
    // a receives PeerJoined for b
    ws_recv(&mut a).await?;

    let clock = Instant::now();
    let mut relay_us = Vec::with_capacity(messages as usize);
    let mut rtt_us = Vec::with_capacity(messages as usize);
    for i in 0..messages {
        let sent = clock.elapsed().as_micros() as u64;
        ws_send(
            &mut a,
            10 + u64::from(i),
            C::Signal(v1::Signal {
                room_id: "bench".into(),
                to_peer_id: b_id.clone(),
                payload: Some(v1::signal::Payload::IceCandidate(v1::IceCandidate {
                    candidate: format!("candidate:{sent} 1 udp 1 127.0.0.1 9 typ host"),
                    sdp_mid: "0".into(),
                    sdp_mline_index: 0,
                })),
            }),
        )
        .await?;
        match ws_recv(&mut b).await? {
            S::Signal(_) => relay_us.push((clock.elapsed().as_micros() as u64 - sent) as f64),
            other => bail!("unexpected {other:?}"),
        }

        let sent = clock.elapsed().as_micros() as u64;
        ws_send(
            &mut a,
            1,
            C::Ping(v1::Ping {
                client_time_us: sent,
            }),
        )
        .await?;
        match ws_recv(&mut a).await? {
            S::Pong(p) => {
                rtt_us.push((clock.elapsed().as_micros() as u64 - p.client_time_us) as f64)
            }
            other => bail!("unexpected {other:?}"),
        }
    }

    let mem_two_peers = mem::sample(pid)?;
    let t_idle = Instant::now();
    let mut idle = Vec::with_capacity(idle_connections as usize);
    for i in 0..idle_connections {
        idle.push(ws_hello(&addr, &format!("idle-{i}")).await?.0);
    }
    let connect_all_ms = t_idle.elapsed().as_secs_f64() * 1000.0;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mem_idle = mem::sample(pid)?;
    // The relay must still be responsive with all idle sockets attached.
    let sent = clock.elapsed().as_micros() as u64;
    ws_send(
        &mut a,
        1,
        C::Ping(v1::Ping {
            client_time_us: sent,
        }),
    )
    .await?;
    let rtt_loaded_us = match ws_recv(&mut a).await? {
        S::Pong(p) => (clock.elapsed().as_micros() as u64 - p.client_time_us) as f64,
        other => bail!("unexpected {other:?}"),
    };
    drop(idle);

    let per_conn = (mem_idle.working_set_bytes as f64 - mem_two_peers.working_set_bytes as f64)
        / f64::from(idle_connections.max(1));
    let summary = json!({
        "relay_latency_us": stats(relay_us.clone()),
        "server_rtt_us": stats(rtt_us.clone()),
        "rtt_with_idle_connections_us": rtt_loaded_us,
        "idle_connections": idle_connections,
        "connect_all_idle_ms": connect_all_ms,
        "server_working_set_mib": {
            "start": mem_start.working_set_bytes as f64 / MIB,
            "two_peers": mem_two_peers.working_set_bytes as f64 / MIB,
            "with_idle": mem_idle.working_set_bytes as f64 / MIB,
        },
        "server_bytes_per_idle_connection": per_conn,
    });
    Ok((
        summary,
        json!({ "relay_latency_us": relay_us, "server_rtt_us": rtt_us }),
    ))
}
