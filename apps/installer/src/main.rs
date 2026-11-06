//! `turbo-pancake-setup.exe` — per-user installer (ADR 0017). Installs the embedded
//! package into `%LOCALAPPDATA%\Programs\turbo-pancake` without administrator rights,
//! creates the Start-menu shortcut and the uninstall entry, then starts the client.
//!
//! `--quiet` — no message boxes; `--no-launch` — do not start the client.
//! Hidden (trials): `--install-dir <dir>`, `--shortcut-dir <dir>`, `--trial-id <id>`.
//! Prints `TURBO_PANCAKE_SETUP action=… version=… bytes=…` (inherited stdout).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use turbo_pancake_setup::{Options, Target, command_line, install_package};

static PACKAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/payload.bin"));

fn report_error(quiet: bool, text: &str) {
    eprintln!("turbo-pancake-setup: {text}");
    #[cfg(windows)]
    if !quiet {
        turbo_pancake_setup::win::error(&format!("Не удалось установить turbo-pancake: {text}"));
    }
    #[cfg(not(windows))]
    let _ = quiet;
}

fn integrate(target: &Target, version: &str, bytes: u64) -> Result<(), String> {
    #[cfg(windows)]
    {
        use turbo_pancake_setup::win;
        let launcher = target.launcher();
        win::create_shortcut(&target.shortcut, &launcher, &target.root, "turbo-pancake")?;
        let args = target.uninstall_args();
        let mut quiet = args.clone();
        quiet.push("--quiet".into());
        win::register_uninstall(&win::UninstallEntry {
            key: &target.registry_key,
            display_name: &target.display_name,
            version,
            install_location: &target.root,
            icon: &launcher,
            uninstall: &command_line(&launcher, &args),
            quiet_uninstall: &command_line(&launcher, &quiet),
            size_kb: (bytes / 1024).min(u32::MAX as u64) as u32,
        })?;
    }
    #[cfg(not(windows))]
    let _ = (target, version, bytes);
    Ok(())
}

fn run(opts: &Options) -> Result<(), String> {
    let target = opts.target()?;
    let outcome = install_package(&target.root, PACKAGE).map_err(|e| e.to_string())?;
    // The entry shows the version that is actually current.
    let current = turbo_pancake_update::Install::new(&target.root)
        .read_state()
        .map(|s| s.current)
        .unwrap_or_else(|_| outcome.version.clone());
    integrate(&target, &current, outcome.installed_bytes)?;
    println!(
        "TURBO_PANCAKE_SETUP action={} version={} current={current} bytes={} root={}",
        outcome.action.name(),
        outcome.version,
        outcome.installed_bytes,
        target.root.display()
    );
    if !opts.no_launch {
        std::process::Command::new(target.launcher())
            .current_dir(&target.root)
            .spawn()
            .map_err(|e| format!("start: {e}"))?;
    }
    Ok(())
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let opts = Options::take(&mut args);
    if let Some(a) = args.first() {
        report_error(opts.quiet, &format!("unknown option {a}"));
        std::process::exit(2);
    }
    if let Err(e) = run(&opts) {
        report_error(opts.quiet, &e);
        std::process::exit(1);
    }
}
