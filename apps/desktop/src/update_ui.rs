//! The updater on the UI thread (S7 step 5): status into the settings panel and the
//! badge, the action button, restart through the launcher, and the confirmation of a
//! freshly installed version. `--trial-update` installs and restarts without input and
//! prints `TURBO_PANCAKE_UPDATE …` lines (also with `--chat-log`).

use std::cell::RefCell;
use std::time::Duration;

use slint::ComponentHandle;
use turbo_pancake_desktop::updater::{self, UpdateCommand, UpdateStatus, Updater, UpdaterConfig};
use turbo_pancake_update::Install;

use crate::MainWindow;

/// A confirmed start: the window was shown and the client kept running this long.
const CONFIRM_AFTER: Duration = Duration::from_secs(4);

#[derive(Default)]
struct State {
    updater: Option<Updater>,
    status: Option<UpdateStatus>,
    trial: bool,
    log: bool,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn start(
    window: &MainWindow,
    http_base: String,
    tls: std::sync::Arc<turbo_pancake_tls::rustls::ClientConfig>,
    trial: bool,
    log: bool,
) {
    let exe = std::env::current_exe().ok();
    let install = exe.as_deref().and_then(Install::from_exe);
    STATE.with_borrow_mut(|s| {
        s.trial = trial;
        s.log = log || trial;
    });
    if let Some(exe) = exe {
        // Confirm a pending trial version once it has proven to start and run.
        slint::Timer::single_shot(CONFIRM_AFTER, move || {
            match updater::confirm_if_pending(&exe) {
                Ok(Some(v)) => println!("TURBO_PANCAKE_UPDATE confirmed=true version={v}"),
                Ok(None) => {}
                Err(e) => eprintln!("update confirmation failed: {e}"),
            }
        });
    }
    let weak = window.as_weak();
    let cfg = UpdaterConfig {
        http_base,
        tls,
        install,
        keys: updater::pinned_keys(),
        build_version: updater::build_version().to_owned(),
        check_every: Duration::from_secs(6 * 3600),
    };
    let started = Updater::start(cfg, move |status| {
        let _ = weak.upgrade_in_event_loop(move |w| apply(&w, status));
    });
    match started {
        Ok(u) => STATE.with_borrow_mut(|s| s.updater = Some(u)),
        Err(e) => window.set_update_text(format!("Не удалось обновить: {e}").into()),
    }
    let weak = window.as_weak();
    window.on_update_action(move || {
        let Some(w) = weak.upgrade() else { return };
        let status = STATE.with_borrow(|s| s.status.clone());
        match status {
            Some(UpdateStatus::Available {
                installable: true, ..
            }) => send(UpdateCommand::Install),
            Some(UpdateStatus::Ready { .. }) => restart(&w),
            _ => send(UpdateCommand::Check),
        }
    });
}

fn send(c: UpdateCommand) {
    STATE.with_borrow(|s| {
        if let Some(u) = &s.updater {
            u.send(c);
        }
    });
}

fn apply(w: &MainWindow, status: UpdateStatus) {
    w.set_update_text(status.text().into());
    w.set_update_button(status.button().into());
    w.set_update_available(status.highlighted());
    let (trial, log) = STATE.with_borrow(|s| (s.trial, s.log));
    if log {
        let (version, error) = match &status {
            UpdateStatus::UpToDate { version }
            | UpdateStatus::Available { version, .. }
            | UpdateStatus::Downloading { version }
            | UpdateStatus::Ready { version } => (version.as_str(), ""),
            UpdateStatus::Failed(e) => ("", e.as_str()),
            _ => ("", ""),
        };
        println!(
            "TURBO_PANCAKE_UPDATE status={} version={version} error=\"{}\"",
            status.name(),
            error.replace('"', "'")
        );
    }
    let user_asked_install = matches!(
        STATE.with_borrow(|s| s.status.clone()),
        Some(UpdateStatus::Downloading { .. })
    );
    STATE.with_borrow_mut(|s| s.status = Some(status.clone()));
    match status {
        UpdateStatus::Available {
            installable: true, ..
        } if trial => send(UpdateCommand::Install),
        // "Установить и перезапустить": the restart follows the verified download.
        UpdateStatus::Ready { .. } if trial || user_asked_install => restart(w),
        _ => {}
    }
}

/// Starts the launcher (it waits for this process to exit) and quits.
fn restart(w: &MainWindow) {
    let Some((install, _)) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Install::from_exe)
    else {
        return;
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let spawned = std::process::Command::new(install.launcher())
        .arg("--wait-pid")
        .arg(std::process::id().to_string())
        .args(&args)
        .current_dir(install.root())
        .spawn();
    match spawned {
        Ok(_) => {
            println!("TURBO_PANCAKE_UPDATE restart=true");
            let _ = w.hide();
            let _ = slint::quit_event_loop();
        }
        Err(e) => w.set_update_text(format!("Не удалось перезапустить: {e}").into()),
    }
}
