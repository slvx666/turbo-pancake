//! The connection banner on the UI thread (S7 step 4): the messenger status and the
//! call's media link feed one [`Tracker`]; a timer re-renders it while it counts down or
//! shows "восстановлена". Trials: `--chat-log` prints `TURBO_PANCAKE_BANNER …` on every
//! change and `--snapshot-banner-dir` saves an own-window snapshot of each state.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use slint::ComponentHandle;
use turbo_pancake_desktop::banner::{Banner, CallLink, Kind, MessengerLink, Tracker};
use turbo_pancake_messenger_client::Status;

use crate::MainWindow;

#[derive(Default)]
struct State {
    tracker: Tracker,
    shown: Banner,
    timer: Option<slint::Timer>,
    log: bool,
    snapshot_dir: Option<PathBuf>,
    snapshots: u32,
    /// Kind of the last snapshot taken (one per kind change).
    shown_kind_snapshot: Option<Kind>,
    /// `--trial-input`: the text whose survival every log line reports.
    trial_input: Option<String>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn configure(log: bool, snapshot_dir: Option<PathBuf>, trial_input: Option<String>) {
    if let Some(dir) = &snapshot_dir
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        eprintln!("--snapshot-banner-dir: {e}");
    }
    STATE.with_borrow_mut(|s| {
        s.log = log;
        s.snapshot_dir = snapshot_dir;
        s.trial_input = trial_input;
    });
}

pub fn messenger(w: &MainWindow, status: &Status) {
    let now = Instant::now();
    let link = match status {
        Status::SignedOut => MessengerLink::SignedOut,
        Status::Connecting => MessengerLink::Connecting,
        Status::Online => MessengerLink::Online,
        Status::Reconnecting { retry_in_ms } => MessengerLink::Offline {
            retry_at: now + Duration::from_millis(*retry_in_ms),
        },
    };
    STATE.with_borrow_mut(|s| s.tracker.set_messenger(link, now));
    refresh(w);
}

pub fn call(w: &MainWindow, link: CallLink) {
    let now = Instant::now();
    let changed = STATE.with_borrow_mut(|s| {
        let before = s.tracker.banner(now);
        s.tracker.set_call(link, now);
        s.tracker.banner(now) != before
    });
    // Media snapshots arrive many times a second; only a change needs the UI.
    if changed {
        refresh(w);
    }
}

fn refresh(w: &MainWindow) {
    let now = Instant::now();
    let (banner, changed, tick) = STATE.with_borrow_mut(|s| {
        let b = s.tracker.banner(now);
        let changed = b != s.shown;
        s.shown = b.clone();
        (b, changed, s.tracker.needs_tick(now))
    });
    if changed {
        w.set_banner_kind(banner.kind.ui());
        w.set_banner_text(banner.text.as_str().into());
        report(w, &banner);
    }
    // The timer is kept and only stopped: this also runs inside its own callback.
    STATE.with_borrow_mut(|s| {
        let timer = s.timer.get_or_insert_with(slint::Timer::default);
        if tick && !timer.running() {
            let weak = w.as_weak();
            timer.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(250),
                move || {
                    if let Some(w) = weak.upgrade() {
                        refresh(&w);
                    }
                },
            );
        } else if !tick && timer.running() {
            timer.stop();
        }
    });
}

/// Log line and snapshot for trials; only the kind matters for snapshots (not every
/// second of the countdown).
fn report(w: &MainWindow, banner: &Banner) {
    let (log, input, dir, (messenger, call)) = STATE.with_borrow(|s| {
        (
            s.log,
            s.trial_input.clone(),
            s.snapshot_dir.clone(),
            s.tracker.link_names(),
        )
    });
    if log {
        let kept = input.as_ref().map_or(String::new(), |t| {
            let kept = w.get_compose_text() == t.as_str() && w.get_room_text() == t.as_str();
            format!(" input_kept={kept}")
        });
        println!(
            "TURBO_PANCAKE_BANNER kind={} messenger={messenger} call={call}{kept} text=\"{}\"",
            banner.kind.name(),
            banner.text.replace('"', "'")
        );
    }
    let Some(dir) = dir else { return };
    let kind = banner.kind;
    let n = STATE.with_borrow_mut(|s| {
        let last = s.shown_kind_snapshot;
        if last == Some(kind) {
            return None;
        }
        s.shown_kind_snapshot = Some(kind);
        s.snapshots += 1;
        Some(s.snapshots)
    });
    let Some(n) = n else { return };
    let path = dir.join(format!("banner-{n:02}-{}.ppm", kind.name()));
    let weak = w.as_weak();
    // After the next frame has the new banner.
    slint::Timer::single_shot(Duration::from_millis(400), move || {
        if let Some(w) = weak.upgrade() {
            let ok = crate::write_snapshot(&w, &path);
            println!(
                "TURBO_PANCAKE_BANNER_SNAPSHOT kind={} success={ok}",
                kind.name()
            );
        }
    });
}
