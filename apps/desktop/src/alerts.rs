//! Notifications and UI sounds (ADR 0021, `docs/NOTIFICATIONS.md`): when the app makes a
//! sound, shows a desktop notification, flashes its taskbar button or shows the unread
//! badge. The policy lives here and is tested with a recording sink; the OS work sits
//! behind [`Sink`] ([`OsSink`]: `PlaySoundW`, tray notifications, `FlashWindowEx`,
//! taskbar overlay).
//!
//! Silent mode (`--no-activate`, i.e. every trial and smoke run, or
//! `TURBO_PANCAKE_SILENT=1`): nothing is played, shown or flashed; each alert that would
//! fire prints one line on stderr instead —
//! `TURBO_PANCAKE_ALERT kind=<kind> [conv=<id>|unread=<n>] actions=<list>`. Message text
//! never reaches a log (spec: no message content in logs).
//!
//! One process-wide [`Alerts`] ([`install`] / [`with`]); every method takes `&self` and
//! may be called from any thread (UI, call session, messenger), except the unread total
//! and the window handle, which belong to the UI thread (taskbar COM object).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::sounds::{self, Player, RateLimiter, Sound};
use crate::taskbar;
use crate::tray::TrayNotifier;

/// At most one message sound per this interval.
pub const MESSAGE_SOUND_INTERVAL: Duration = Duration::from_millis(700);
/// Message notifications within this window of each other merge into "N новых сообщений".
pub const COALESCE_WINDOW: Duration = Duration::from_secs(5);
/// "Connection lost" sounds at most once per this interval (flapping links).
pub const DISCONNECT_SOUND_INTERVAL: Duration = Duration::from_secs(30);
/// Characters of the message text shown in a notification.
pub const PREVIEW_CHARS: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prefs {
    pub notifications: bool,
    pub sounds: bool,
    /// 0–100.
    pub volume: u8,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            notifications: true,
            sounds: true,
            volume: 70,
        }
    }
}

/// What a click on a notification opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Conversation(String),
    /// The incoming call of this conversation.
    Call(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub text: String,
    pub target: Target,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Sound(Sound),
    /// Start (or with `None` stop) the looped sound.
    Loop(Option<Sound>),
    /// A desktop notification; a click reports `tag` (see [`Alerts::target`]).
    Notify(Notification, u64),
    Flash,
    Badge(u32),
}

impl Action {
    fn name(&self) -> String {
        match self {
            Action::Sound(s) => format!("sound:{}", s.name()),
            Action::Loop(Some(s)) => format!("loop:{}", s.name()),
            Action::Loop(None) => "loop:stop".into(),
            Action::Notify(..) => "notify".into(),
            Action::Flash => "flash".into(),
            Action::Badge(n) => format!("badge:{n}"),
        }
    }
}

/// Carries the alerts out (or reports them).
pub trait Sink: Send + Sync {
    /// The main window is the one the user works in (shown, not minimized, foreground).
    fn focused(&self, window: isize) -> bool;
    fn perform(&self, kind: &str, detail: &str, actions: Vec<Action>, window: isize);
    fn set_volume(&self, _volume: u8) {}
    fn set_tray(&self, _tray: TrayNotifier) {}
    /// The window was (re)created: put the badge on its new taskbar button (UI thread).
    fn reapply_badge(&self, _window: isize, _unread: u32) {}
}

/// The real thing: sounds, tray notifications, taskbar flash and badge.
pub struct OsSink {
    player: Player,
    tray: Mutex<Option<TrayNotifier>>,
}

impl OsSink {
    pub fn new(volume: u8) -> Self {
        Self {
            player: Player::new(sounds::os_audio(), volume),
            tray: Mutex::new(None),
        }
    }

    fn badge(&self, window: isize, unread: u32) {
        taskbar::set_overlay(window, unread);
        if let Some(t) = *self.tray.lock().unwrap_or_else(|e| e.into_inner()) {
            t.set_tooltip(&tooltip(unread));
        }
    }
}

impl Sink for OsSink {
    fn focused(&self, window: isize) -> bool {
        taskbar::is_foreground(window)
    }

    fn perform(&self, _kind: &str, _detail: &str, actions: Vec<Action>, window: isize) {
        for a in actions {
            match a {
                Action::Sound(s) => self.player.play(s),
                Action::Loop(s) => self.player.set_loop(s),
                Action::Notify(n, tag) => {
                    if let Some(t) = *self.tray.lock().unwrap_or_else(|e| e.into_inner()) {
                        t.balloon(&n.title, &n.text, tag);
                    }
                }
                Action::Flash => taskbar::flash(window),
                Action::Badge(n) => self.badge(window, n),
            }
        }
    }

    fn set_volume(&self, volume: u8) {
        self.player.set_volume(volume);
    }

    fn set_tray(&self, tray: TrayNotifier) {
        *self.tray.lock().unwrap_or_else(|e| e.into_inner()) = Some(tray);
    }

    fn reapply_badge(&self, window: isize, unread: u32) {
        if unread > 0 {
            self.badge(window, unread);
        }
    }
}

/// Silent mode: one stderr line per alert, nothing audible or visible.
pub struct LogSink;

impl Sink for LogSink {
    fn focused(&self, window: isize) -> bool {
        taskbar::is_foreground(window)
    }

    fn perform(&self, kind: &str, detail: &str, actions: Vec<Action>, _window: isize) {
        let list: Vec<String> = actions.iter().map(Action::name).collect();
        eprintln!(
            "TURBO_PANCAKE_ALERT kind={kind}{detail} actions={}",
            list.join(",")
        );
    }
}

/// Tray tooltip with the unread count.
pub fn tooltip(unread: u32) -> String {
    if unread == 0 {
        "turbo-pancake".into()
    } else {
        format!("turbo-pancake — непрочитанных: {unread}")
    }
}

/// "N новых сообщений" with the Russian plural.
pub fn new_messages(n: u32) -> String {
    let (n10, n100) = (n % 10, n % 100);
    let words = if n10 == 1 && n100 != 11 {
        "новое сообщение"
    } else if (2..=4).contains(&n10) && !(12..=14).contains(&n100) {
        "новых сообщения"
    } else {
        "новых сообщений"
    };
    format!("{n} {words}")
}

/// The start of a message for a notification: one line, at most [`PREVIEW_CHARS`].
pub fn preview(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= PREVIEW_CHARS {
        flat
    } else {
        let mut s: String = flat.chars().take(PREVIEW_CHARS - 1).collect();
        s.push('…');
        s
    }
}

/// A live new message from someone else.
#[derive(Clone, Debug)]
pub struct MessageAlert {
    pub conversation_id: String,
    pub message_id: String,
    /// Author (and group) as the notification title.
    pub title: String,
    pub text: String,
}

/// An incoming call waiting for our answer.
#[derive(Clone, Debug)]
pub struct IncomingCall {
    pub call_id: String,
    pub conversation_id: String,
    pub title: String,
}

fn push_bounded<T>(q: &mut VecDeque<T>, v: T, cap: usize) {
    if q.len() == cap {
        q.pop_front();
    }
    q.push_back(v);
}

#[derive(Default)]
struct Connection {
    seen_online: bool,
    down: bool,
    announced: bool,
    last_sound: Option<Instant>,
}

struct State {
    prefs: Prefs,
    message_sounds: RateLimiter,
    /// Message ids already alerted (at-least-once delivery).
    seen: VecDeque<String>,
    /// The last message notification and how many it covered.
    coalesce: Option<(Instant, u32)>,
    ringing: Option<String>,
    /// Calls answered or declined here: never ring again.
    answered: VecDeque<String>,
    ringback: Option<String>,
    room: Option<String>,
    connection: Connection,
    unread: u32,
    next_tag: u64,
    targets: VecDeque<(u64, Target)>,
}

pub struct Alerts {
    sink: Box<dyn Sink>,
    window: AtomicIsize,
    state: Mutex<State>,
}

impl Alerts {
    pub fn new(sink: Box<dyn Sink>, prefs: Prefs) -> Self {
        sink.set_volume(prefs.volume);
        Self {
            sink,
            window: AtomicIsize::new(0),
            state: Mutex::new(State {
                prefs,
                message_sounds: RateLimiter::new(MESSAGE_SOUND_INTERVAL),
                seen: VecDeque::new(),
                coalesce: None,
                ringing: None,
                answered: VecDeque::new(),
                ringback: None,
                room: None,
                connection: Connection::default(),
                unread: 0,
                next_tag: 1,
                targets: VecDeque::new(),
            }),
        }
    }

    /// The OS sink, or the logging one in silent mode.
    pub fn for_this_process(silent: bool, prefs: Prefs) -> Self {
        let sink: Box<dyn Sink> = if silent {
            Box::new(LogSink)
        } else {
            Box::new(OsSink::new(prefs.volume))
        };
        Self::new(sink, prefs)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn window(&self) -> isize {
        self.window.load(Ordering::Relaxed)
    }

    fn fire(&self, kind: &str, detail: &str, actions: Vec<Action>) {
        if !actions.is_empty() {
            self.sink.perform(kind, detail, actions, self.window());
        }
    }

    fn sounds_on(p: &Prefs) -> bool {
        p.sounds && p.volume > 0
    }

    fn tag(st: &mut State, target: Target) -> u64 {
        let tag = st.next_tag;
        st.next_tag += 1;
        push_bounded(&mut st.targets, (tag, target), 16);
        tag
    }

    // ---- Settings and wiring ----

    pub fn prefs(&self) -> Prefs {
        self.state().prefs
    }

    /// Settings panel: notifications on/off, sounds on/off, volume 0–100.
    pub fn set_prefs(&self, notifications: bool, sounds: bool, volume: u8) {
        let volume = volume.min(100);
        let stop = {
            let mut st = self.state();
            let was_on = Self::sounds_on(&st.prefs);
            st.prefs = Prefs {
                notifications,
                sounds,
                volume,
            };
            let looping = st.ringing.is_some() || st.ringback.is_some();
            was_on && !Self::sounds_on(&st.prefs) && looping
        };
        self.sink.set_volume(volume);
        if stop {
            self.fire("sounds_off", "", vec![Action::Loop(None)]);
        }
    }

    /// Settings panel "Проверить звук": the message sound at the current volume.
    pub fn test_sound(&self) {
        if self.state().prefs.volume > 0 {
            self.fire("test_sound", "", vec![Action::Sound(Sound::Message)]);
        }
    }

    /// The main window's native handle; call again whenever it may be a new window
    /// (after showing it from the tray). UI thread.
    pub fn set_main_window(&self, hwnd: isize) {
        self.window.store(hwnd, Ordering::Relaxed);
        let unread = self.state().unread;
        self.sink.reapply_badge(hwnd, unread);
    }

    pub fn set_tray(&self, tray: TrayNotifier) {
        self.sink.set_tray(tray);
    }

    /// What the notification with this tag (from [`crate::tray::TrayEvent`]) opens.
    pub fn target(&self, tag: u64) -> Option<Target> {
        self.state()
            .targets
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, target)| target.clone())
    }

    // ---- Unread badge (UI thread) ----

    pub fn unread_total(&self) -> u32 {
        self.state().unread
    }

    /// The sum of unread messages over all conversations changed (UI thread): taskbar
    /// badge, tray tooltip and the [`on_unread_changed`] listeners.
    pub fn set_unread_total(&self, unread: u32) {
        let changed = std::mem::replace(&mut self.state().unread, unread) != unread;
        if !changed {
            return;
        }
        self.fire(
            "badge",
            &format!(" unread={unread}"),
            vec![Action::Badge(unread)],
        );
        UNREAD_LISTENERS.with_borrow(|l| l.iter().for_each(|f| f(unread)));
    }

    // ---- Messages ----

    /// A live new message from someone else. `open` is the conversation on screen (none
    /// while the call view or nothing is shown).
    pub fn message(&self, m: &MessageAlert, open: Option<&str>) {
        let focused = self.sink.focused(self.window());
        self.message_at(m, open, focused, Instant::now());
    }

    fn message_at(&self, m: &MessageAlert, open: Option<&str>, focused: bool, now: Instant) {
        let actions = {
            let mut st = self.state();
            if !m.message_id.is_empty() {
                if st.seen.contains(&m.message_id) {
                    return;
                }
                push_bounded(&mut st.seen, m.message_id.clone(), 64);
            }
            // Being read right now: nothing.
            if focused && open == Some(m.conversation_id.as_str()) {
                return;
            }
            let mut actions = Vec::new();
            if Self::sounds_on(&st.prefs) && st.message_sounds.allow(now) {
                actions.push(Action::Sound(Sound::Message));
            }
            if !focused && st.prefs.notifications {
                let count = match st.coalesce {
                    Some((at, n)) if now.saturating_duration_since(at) < COALESCE_WINDOW => n + 1,
                    _ => 1,
                };
                st.coalesce = Some((now, count));
                let (title, text) = if count > 1 {
                    ("turbo-pancake".to_string(), new_messages(count))
                } else {
                    (m.title.clone(), preview(&m.text))
                };
                let target = Target::Conversation(m.conversation_id.clone());
                let tag = Self::tag(&mut st, target.clone());
                actions.push(Action::Notify(
                    Notification {
                        title,
                        text,
                        target,
                    },
                    tag,
                ));
                actions.push(Action::Flash);
            }
            actions
        };
        self.fire("message", &format!(" conv={}", m.conversation_id), actions);
    }

    // ---- Calls ----

    /// The incoming call to show now (`None`: none waits for us). Starts the ringtone,
    /// notification and flash for a new one; stops the ringtone when it is gone
    /// (accepted, declined, cancelled, ended, timed out, answered on another device).
    pub fn incoming_call(&self, call: Option<&IncomingCall>) {
        let focused = self.sink.focused(self.window());
        self.incoming_call_with(call, focused);
    }

    fn incoming_call_with(&self, call: Option<&IncomingCall>, focused: bool) {
        let mut st = self.state();
        let call = call.filter(|c| !st.answered.contains(&c.call_id));
        match call {
            Some(c) if st.ringing.as_deref() == Some(c.call_id.as_str()) => {}
            Some(c) => {
                st.ringing = Some(c.call_id.clone());
                let mut actions = Vec::new();
                if Self::sounds_on(&st.prefs) {
                    actions.push(Action::Loop(Some(Sound::IncomingRing)));
                }
                if !focused && st.prefs.notifications {
                    let target = Target::Call(c.conversation_id.clone());
                    let tag = Self::tag(&mut st, target.clone());
                    actions.push(Action::Notify(
                        Notification {
                            title: format!("Входящий звонок — {}", c.title),
                            text: "Откройте turbo-pancake, чтобы ответить".into(),
                            target,
                        },
                        tag,
                    ));
                    actions.push(Action::Flash);
                }
                drop(st);
                self.fire(
                    "incoming_ring",
                    &format!(" conv={}", c.conversation_id),
                    actions,
                );
            }
            None => {
                if st.ringing.take().is_some() {
                    let after = Self::after_ring(&st);
                    drop(st);
                    self.fire("ring_stop", "", vec![Action::Loop(after)]);
                }
            }
        }
    }

    /// What loops once the ringtone stops: the ringback of our own waiting call.
    fn after_ring(st: &State) -> Option<Sound> {
        (st.ringback.is_some() && Self::sounds_on(&st.prefs)).then_some(Sound::OutgoingRing)
    }

    /// Accept or decline here: stop ringing at once, never ring again for this call.
    pub fn call_answered(&self, call_id: &str) {
        let mut st = self.state();
        if !st.answered.iter().any(|c| c == call_id) {
            push_bounded(&mut st.answered, call_id.to_owned(), 16);
        }
        if st.ringing.as_deref() == Some(call_id) {
            st.ringing = None;
            let after = Self::after_ring(&st);
            drop(st);
            self.fire("ring_stop", "", vec![Action::Loop(after)]);
        }
    }

    /// Our own call waiting for the others to answer (`None`: not waiting).
    pub fn outgoing_call(&self, call_id: Option<&str>) {
        let mut st = self.state();
        match call_id {
            Some(id) if st.ringback.as_deref() == Some(id) => {}
            Some(id) => {
                st.ringback = Some(id.to_owned());
                if st.ringing.is_none() && Self::sounds_on(&st.prefs) {
                    drop(st);
                    self.fire(
                        "outgoing_ring",
                        "",
                        vec![Action::Loop(Some(Sound::OutgoingRing))],
                    );
                }
            }
            None => {
                if st.ringback.take().is_some() && st.ringing.is_none() {
                    drop(st);
                    self.fire("ringback_stop", "", vec![Action::Loop(None)]);
                }
            }
        }
    }

    fn sound(&self, kind: &str, s: Sound) {
        if Self::sounds_on(&self.state().prefs) {
            self.fire(kind, "", vec![Action::Sound(s)]);
        }
    }

    /// The server confirmed us in this call's room (a rejoin after a reconnect is silent).
    pub fn call_joined(&self, room: &str) {
        let new = {
            let mut st = self.state();
            let new = st.room.as_deref() != Some(room);
            st.room = Some(room.to_owned());
            new
        };
        if new {
            self.sound("call_join", Sound::CallJoin);
        }
    }

    /// We left the call (or it ended for us).
    pub fn call_left(&self) {
        if self.state().room.take().is_some() {
            self.sound("call_leave", Sound::CallLeave);
        }
    }

    /// Another member joined our call.
    pub fn peer_joined(&self) {
        if self.state().room.is_some() {
            self.sound("peer_join", Sound::PeerJoin);
        }
    }

    pub fn peer_left(&self) {
        if self.state().room.is_some() {
            self.sound("peer_leave", Sound::PeerLeave);
        }
    }

    pub fn mute_changed(&self, muted: bool) {
        if muted {
            self.sound("mute", Sound::Mute);
        } else {
            self.sound("unmute", Sound::Unmute);
        }
    }

    pub fn share_changed(&self, sharing: bool) {
        if sharing {
            self.sound("share_start", Sound::ShareStart);
        } else {
            self.sound("share_stop", Sound::ShareStop);
        }
    }

    // ---- Connection ----

    /// The connection banner's state: `problem` (offline / reconnecting) and whether a
    /// link is up now. Lost → a sound once the app was online at least once, at most once
    /// per [`DISCONNECT_SOUND_INTERVAL`]; restored → a sound only after an announced loss.
    pub fn connection(&self, problem: bool, online: bool) {
        self.connection_at(problem, online, Instant::now());
    }

    fn connection_at(&self, problem: bool, online: bool, now: Instant) {
        let fire = {
            let mut st = self.state();
            let sounds = Self::sounds_on(&st.prefs);
            let c = &mut st.connection;
            if online && !problem {
                c.seen_online = true;
            }
            if problem && !c.down && c.seen_online {
                c.down = true;
                let quiet = c
                    .last_sound
                    .is_some_and(|t| now.saturating_duration_since(t) < DISCONNECT_SOUND_INTERVAL);
                if !quiet {
                    c.last_sound = Some(now);
                    c.announced = true;
                    Some(("disconnected", Sound::Disconnected))
                } else {
                    None
                }
            } else if !problem && c.down {
                c.down = false;
                let announced = std::mem::take(&mut c.announced);
                (announced && online).then_some(("reconnected", Sound::Reconnected))
            } else {
                None
            }
            .filter(|_| sounds)
        };
        if let Some((kind, s)) = fire {
            self.fire(kind, "", vec![Action::Sound(s)]);
        }
    }
}

type UnreadListener = Box<dyn Fn(u32)>;

thread_local! {
    static UNREAD_LISTENERS: RefCell<Vec<UnreadListener>> = RefCell::default();
}

/// Calls `f` on this (UI) thread with the new unread total whenever it changes — e.g. to
/// set the window's `unread-total` property.
pub fn on_unread_changed(f: impl Fn(u32) + 'static) {
    UNREAD_LISTENERS.with_borrow_mut(|l| l.push(Box::new(f)));
}

static GLOBAL: OnceLock<Alerts> = OnceLock::new();

/// Makes `alerts` the process-wide instance (first call wins).
pub fn install(alerts: Alerts) -> &'static Alerts {
    let _ = GLOBAL.set(alerts);
    GLOBAL.get().expect("set above")
}

pub fn get() -> Option<&'static Alerts> {
    GLOBAL.get()
}

/// Runs `f` with the process-wide alerts; nothing before [`install`] (tests, tools).
pub fn with(f: impl FnOnce(&Alerts)) {
    if let Some(a) = GLOBAL.get() {
        f(a);
    }
}

/// Silent mode: trials (`--no-activate`) or `TURBO_PANCAKE_SILENT=1`.
pub fn silent_requested(no_activate: bool) -> bool {
    no_activate || std::env::var_os("TURBO_PANCAKE_SILENT").is_some_and(|v| v == "1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    type Log = Vec<(String, Vec<Action>)>;

    /// Records alerts; never touches the OS.
    #[derive(Clone, Default)]
    struct Rec {
        log: Arc<Mutex<Log>>,
        focused: Arc<AtomicBool>,
    }

    impl Sink for Rec {
        fn focused(&self, _window: isize) -> bool {
            self.focused.load(Ordering::Relaxed)
        }
        fn perform(&self, kind: &str, _detail: &str, actions: Vec<Action>, _window: isize) {
            self.log.lock().unwrap().push((kind.into(), actions));
        }
    }

    impl Rec {
        fn take(&self) -> Log {
            std::mem::take(&mut *self.log.lock().unwrap())
        }
        fn kinds(&self) -> Vec<String> {
            self.take().into_iter().map(|(k, _)| k).collect()
        }
    }

    fn alerts() -> (Alerts, Rec) {
        let rec = Rec::default();
        (Alerts::new(Box::new(rec.clone()), Prefs::default()), rec)
    }

    fn msg(conv: &str, id: &str) -> MessageAlert {
        MessageAlert {
            conversation_id: conv.into(),
            message_id: id.into(),
            title: "Алиса".into(),
            text: "привет\nкак дела".into(),
        }
    }

    fn names(actions: &[Action]) -> Vec<String> {
        actions.iter().map(Action::name).collect()
    }

    #[test]
    fn message_policy_follows_focus_and_open_conversation() {
        let (a, rec) = alerts();
        let t0 = Instant::now();
        let s = Duration::from_secs(1);
        // Focused, reading that conversation: nothing.
        a.message_at(&msg("c1", "m1"), Some("c1"), true, t0);
        assert!(rec.take().is_empty());
        // Focused, another conversation open: sound only.
        a.message_at(&msg("c1", "m2"), Some("c2"), true, t0 + s);
        let got = rec.take();
        assert_eq!(names(&got[0].1), ["sound:message"]);
        // Not focused (hidden, minimized, other app): sound, notification, flash.
        a.message_at(&msg("c1", "m3"), Some("c1"), false, t0 + 2 * s);
        let got = rec.take();
        assert_eq!(got[0].0, "message");
        assert_eq!(names(&got[0].1), ["sound:message", "notify", "flash"]);
        let Action::Notify(n, tag) = &got[0].1[1] else {
            panic!()
        };
        assert_eq!(n.title, "Алиса");
        assert_eq!(n.text, "привет как дела");
        assert_eq!(a.target(*tag), Some(Target::Conversation("c1".into())));
        // The same message again (at-least-once delivery): nothing.
        a.message_at(&msg("c1", "m3"), None, false, t0 + 9 * s);
        assert!(rec.take().is_empty());
    }

    #[test]
    fn message_sounds_are_rate_limited_and_notifications_coalesce() {
        let (a, rec) = alerts();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        a.message_at(&msg("c1", "a"), None, false, t0);
        a.message_at(&msg("c2", "b"), None, false, t0 + ms(200));
        a.message_at(&msg("c2", "c"), None, false, t0 + ms(900));
        let got = rec.take();
        assert_eq!(names(&got[0].1), ["sound:message", "notify", "flash"]);
        assert_eq!(
            names(&got[1].1),
            ["notify", "flash"],
            "no sound within 700 ms"
        );
        assert_eq!(names(&got[2].1), ["sound:message", "notify", "flash"]);
        let texts: Vec<String> = got
            .iter()
            .map(
                |(_, acts)| match &acts.iter().find(|x| x.name() == "notify") {
                    Some(Action::Notify(n, _)) => format!("{}|{}", n.title, n.text),
                    _ => unreachable!(),
                },
            )
            .collect();
        assert_eq!(
            texts,
            [
                "Алиса|привет как дела",
                "turbo-pancake|2 новых сообщения",
                "turbo-pancake|3 новых сообщения"
            ]
        );
        // After a quiet spell a single message is shown in full again.
        a.message_at(&msg("c1", "d"), None, false, t0 + ms(7000));
        let got = rec.take();
        let Some(Action::Notify(n, _)) = got[0].1.iter().find(|x| x.name() == "notify") else {
            panic!()
        };
        assert_eq!(n.title, "Алиса");
    }

    #[test]
    fn prefs_switch_parts_off() {
        let (a, rec) = alerts();
        let t0 = Instant::now();
        a.set_prefs(false, true, 70);
        a.message_at(&msg("c", "1"), None, false, t0);
        assert_eq!(names(&rec.take()[0].1), ["sound:message"]);
        a.set_prefs(true, false, 70);
        a.message_at(&msg("c", "2"), None, false, t0 + Duration::from_secs(1));
        assert_eq!(names(&rec.take()[0].1), ["notify", "flash"]);
        // Volume 0 is "no sound" too, but the test button stays silent then.
        a.set_prefs(false, true, 0);
        a.message_at(&msg("c", "3"), None, false, t0 + Duration::from_secs(2));
        a.test_sound();
        a.mute_changed(true);
        assert!(rec.take().is_empty());
        a.set_prefs(false, false, 40);
        a.test_sound();
        assert_eq!(rec.kinds(), ["test_sound"]);
        // Sounds switched off while the phone rings: the ringtone stops.
        a.set_prefs(false, true, 40);
        a.incoming_call_with(Some(&call("z")), true);
        rec.take();
        a.set_prefs(false, false, 40);
        assert_eq!(rec.kinds(), ["sounds_off"]);
    }

    fn call(id: &str) -> IncomingCall {
        IncomingCall {
            call_id: id.into(),
            conversation_id: format!("conv-{id}"),
            title: "Боб".into(),
        }
    }

    #[test]
    fn ringtone_starts_once_and_stops_with_the_call() {
        let (a, rec) = alerts();
        a.incoming_call_with(Some(&call("k1")), false);
        a.incoming_call_with(Some(&call("k1")), false);
        let got = rec.take();
        assert_eq!(got.len(), 1, "one start per call");
        assert_eq!(names(&got[0].1), ["loop:incoming_ring", "notify", "flash"]);
        let Action::Notify(n, tag) = &got[0].1[1] else {
            panic!()
        };
        assert_eq!(n.title, "Входящий звонок — Боб");
        assert_eq!(a.target(*tag), Some(Target::Call("conv-k1".into())));
        // Cancelled / ended / answered on another device: no longer incoming.
        a.incoming_call_with(None, false);
        assert_eq!(names(&rec.take()[0].1), ["loop:stop"]);
        a.incoming_call_with(None, false);
        assert!(rec.take().is_empty());
        // Focused: the in-app call card shows it; ringtone only.
        a.incoming_call_with(Some(&call("k2")), true);
        assert_eq!(names(&rec.take()[0].1), ["loop:incoming_ring"]);
        // Accepted here: stops at once and the stale list does not restart it.
        a.call_answered("k2");
        assert_eq!(rec.kinds(), ["ring_stop"]);
        a.incoming_call_with(Some(&call("k2")), true);
        assert!(rec.take().is_empty());
    }

    #[test]
    fn ringback_yields_to_the_ringtone_and_returns() {
        let (a, rec) = alerts();
        a.outgoing_call(Some("own"));
        a.outgoing_call(Some("own"));
        assert_eq!(names(&rec.take()[0].1), ["loop:outgoing_ring"]);
        a.incoming_call_with(Some(&call("k")), true);
        a.call_answered("k");
        let got = rec.take();
        assert_eq!(names(&got[1].1), ["loop:outgoing_ring"], "ringback resumes");
        a.outgoing_call(None);
        assert_eq!(names(&rec.take()[0].1), ["loop:stop"]);
    }

    #[test]
    fn call_room_sounds() {
        let (a, rec) = alerts();
        a.peer_joined();
        assert!(rec.take().is_empty(), "not in a call");
        a.call_joined("call_1");
        a.call_joined("call_1"); // rejoin after a reconnect
        a.peer_joined();
        a.peer_left();
        a.mute_changed(true);
        a.mute_changed(false);
        a.share_changed(true);
        a.share_changed(false);
        a.call_left();
        a.call_left();
        assert_eq!(
            rec.kinds(),
            [
                "call_join",
                "peer_join",
                "peer_leave",
                "mute",
                "unmute",
                "share_start",
                "share_stop",
                "call_leave"
            ]
        );
    }

    #[test]
    fn connection_sounds_need_a_prior_online_and_are_debounced() {
        let (a, rec) = alerts();
        let t0 = Instant::now();
        let s = Duration::from_secs;
        // Never online yet: an initial failure is silent.
        a.connection_at(true, false, t0);
        a.connection_at(false, true, t0 + s(1));
        assert!(rec.take().is_empty());
        a.connection_at(true, false, t0 + s(2));
        a.connection_at(true, false, t0 + s(3));
        a.connection_at(false, true, t0 + s(4));
        assert_eq!(rec.kinds(), ["disconnected", "reconnected"]);
        // Flapping: no new pair within 30 s.
        a.connection_at(true, false, t0 + s(5));
        a.connection_at(false, true, t0 + s(6));
        assert!(rec.take().is_empty());
        a.connection_at(true, false, t0 + s(40));
        a.connection_at(false, false, t0 + s(41)); // signed out, not restored
        assert_eq!(rec.kinds(), ["disconnected"]);
    }

    #[test]
    fn unread_total_updates_badge_and_listeners_on_change() {
        let (a, rec) = alerts();
        let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
        let s = seen.clone();
        on_unread_changed(move |n| s.borrow_mut().push(n));
        a.set_unread_total(3);
        a.set_unread_total(3);
        a.set_unread_total(0);
        assert_eq!(a.unread_total(), 0);
        let got = rec.take();
        assert_eq!(names(&got[0].1), ["badge:3"]);
        assert_eq!(names(&got[1].1), ["badge:0"]);
        assert_eq!(*seen.borrow(), [3, 0]);
    }

    #[test]
    fn texts() {
        assert_eq!(new_messages(2), "2 новых сообщения");
        assert_eq!(new_messages(5), "5 новых сообщений");
        assert_eq!(new_messages(11), "11 новых сообщений");
        assert_eq!(new_messages(21), "21 новое сообщение");
        assert_eq!(new_messages(24), "24 новых сообщения");
        assert_eq!(tooltip(0), "turbo-pancake");
        assert_eq!(tooltip(3), "turbo-pancake — непрочитанных: 3");
        let long = "слово ".repeat(40);
        let p = preview(&long);
        assert_eq!(p.chars().count(), PREVIEW_CHARS);
        assert!(p.ends_with('…'));
    }
}
