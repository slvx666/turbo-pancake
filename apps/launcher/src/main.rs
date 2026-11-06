//! `turbo-pancake.exe` in the install root (ADR 0016). Starts
//! `versions/<current>/turbo-pancake-desktop.exe` with the given arguments. A pending
//! update gets one trial: the launcher waits until the new client confirms itself in
//! `state.json`; if it exits first (or the wait times out) the version is marked bad
//! and the previous one is started instead.
//!
//! `--wait-pid <pid>` — first wait (≤ 30 s) for that process to exit: the client that
//! restarts for an update releases its single-instance lock and files.
//! Prints `TURBO_PANCAKE_LAUNCHER …` lines (inherited stdout, trials).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::{Child, Command};
use std::time::{Duration, Instant};

use turbo_pancake_update::{Install, LaunchPlan};

/// How long a trial may take to confirm before it counts as failed.
const TRIAL_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(windows)]
#[allow(unsafe_code)]
fn wait_for_pid(pid: u32, timeout: Duration) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    // SAFETY: plain Win32 calls on a handle we own and close.
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let _ = WaitForSingleObject(h, timeout.as_millis() as u32);
            let _ = CloseHandle(h);
        }
    }
}

#[cfg(not(windows))]
fn wait_for_pid(_pid: u32, _timeout: Duration) {}

fn start(install: &Install, version: &str, args: &[String]) -> std::io::Result<Child> {
    let exe = install.exe(version);
    Command::new(&exe)
        .args(args)
        .current_dir(install.version_dir(version))
        .spawn()
}

fn run() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let root = exe.parent().ok_or("no install directory")?;
    let install = Install::new(root);
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--wait-pid") {
        let pid = args.get(1).and_then(|p| p.parse().ok());
        args.drain(..2.min(args.len()));
        if let Some(pid) = pid {
            wait_for_pid(pid, Duration::from_secs(30));
        }
    }
    if args.first().map(String::as_str) == Some("--uninstall") {
        args.remove(0);
        return uninstall(&exe, args);
    }
    let plan = install.launch_plan().map_err(|e| e.to_string())?;
    match plan {
        LaunchPlan::Run { version } => {
            println!("TURBO_PANCAKE_LAUNCHER plan=run version={version}");
            start(&install, &version, &args).map_err(|e| format!("start {version}: {e}"))?;
            Ok(())
        }
        LaunchPlan::Trial { version, fallback } => {
            println!("TURBO_PANCAKE_LAUNCHER plan=trial version={version} fallback={fallback}");
            trial(&install, &version, &fallback, &args)
        }
    }
}

/// `--uninstall` (ADR 0017): the launcher copies itself to the temporary directory and
/// runs the copy (`--uninstall-copy`), which can then delete the install directory.
fn uninstall(exe: &std::path::Path, mut args: Vec<String>) -> Result<(), String> {
    let copy_mode = args.first().map(String::as_str) == Some("--uninstall-copy");
    if copy_mode {
        args.remove(0);
    }
    let opts = turbo_pancake_setup::Options::take(&mut args);
    if !copy_mode {
        let copy = std::env::temp_dir().join(format!(
            "turbo-pancake-uninstall-{}.exe",
            std::process::id()
        ));
        std::fs::copy(exe, &copy).map_err(|e| format!("copy: {e}"))?;
        // Without an explicit directory the copy must still know where we are.
        let mut opts = opts;
        if opts.install_dir.is_none() {
            opts.install_dir = exe.parent().map(Into::into);
        }
        Command::new(&copy)
            .arg("--wait-pid")
            .arg(std::process::id().to_string())
            .arg("--uninstall")
            .arg("--uninstall-copy")
            .args(opts.to_args())
            .current_dir(std::env::temp_dir())
            .spawn()
            .map_err(|e| format!("start uninstaller: {e}"))?;
        return Ok(());
    }
    let result = remove_everything(&opts);
    match &result {
        Ok(purged) => {
            println!("TURBO_PANCAKE_UNINSTALL done=true purge={purged}");
            #[cfg(windows)]
            if !opts.quiet {
                turbo_pancake_setup::win::info("turbo-pancake удалён.");
            }
        }
        Err(e) => {
            println!("TURBO_PANCAKE_UNINSTALL done=false error=\"{e}\"");
            #[cfg(windows)]
            if !opts.quiet && e != "cancelled" {
                turbo_pancake_setup::win::error(&format!("Не удалось удалить turbo-pancake: {e}"));
            }
        }
    }
    delete_self_later(exe);
    result.map(|_| ())
}

/// Returns whether profile data was removed too.
fn remove_everything(opts: &turbo_pancake_setup::Options) -> Result<bool, String> {
    let target = opts.target()?;
    let mut purge = opts.purge;
    #[cfg(windows)]
    if !opts.quiet {
        use turbo_pancake_setup::win::{Ask, ask};
        if !ask("Удалить turbo-pancake с этого компьютера?", Ask::OkCancel) {
            return Err("cancelled".into());
        }
        purge = ask(
            "Удалить также локальную копию переписки, настройки и сохранённый вход?              Переписка на сервере не затрагивается.",
            Ask::YesNo,
        );
    }
    // Files first: a running client keeps them, and then nothing else is touched.
    turbo_pancake_setup::remove_install(&target.root).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&target.shortcut);
    #[cfg(windows)]
    turbo_pancake_setup::win::unregister_uninstall(&target.registry_key)?;
    if purge {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            turbo_pancake_setup::remove_profile_data(std::path::Path::new(&local))
                .map_err(|e| e.to_string())?;
        }
        #[cfg(windows)]
        turbo_pancake_setup::win::delete_credentials(&target.credential_prefix);
    }
    Ok(purge)
}

/// The temporary uninstaller copy cannot delete itself while it runs.
fn delete_self_later(exe: &std::path::Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = Command::new("cmd.exe")
            .args([
                "/d",
                "/c",
                "ping",
                "-n",
                "3",
                "127.0.0.1",
                ">nul",
                "&",
                "del",
                "/f",
                "/q",
            ])
            .arg(exe)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(windows))]
    let _ = exe;
}

fn trial(install: &Install, version: &str, fallback: &str, args: &[String]) -> Result<(), String> {
    let confirmed = |i: &Install| i.read_state().is_ok_and(|s| s.current == version);
    let failed = match start(install, version, args) {
        Err(e) => Some(format!("start failed: {e}")),
        Ok(mut child) => {
            let until = Instant::now() + TRIAL_TIMEOUT;
            loop {
                if confirmed(install) {
                    println!("TURBO_PANCAKE_LAUNCHER confirmed version={version}");
                    break None;
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        // It may have confirmed just before a normal exit.
                        if confirmed(install) {
                            println!("TURBO_PANCAKE_LAUNCHER confirmed version={version}");
                            break None;
                        }
                        break Some(format!("exited with {status} before confirming"));
                    }
                    Ok(None) if Instant::now() > until => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Some("no confirmation in time".into());
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                    Err(e) => break Some(e.to_string()),
                }
            }
        }
    };
    if let Some(why) = failed {
        println!(
            "TURBO_PANCAKE_LAUNCHER rollback version={version} to={fallback} reason=\"{why}\""
        );
        install.fail_trial(version).map_err(|e| e.to_string())?;
        start(install, fallback, args).map_err(|e| format!("start {fallback}: {e}"))?;
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("turbo-pancake: {e}");
        std::process::exit(1);
    }
}
