//! The connection banner over the content (S7 step 4, spec §9 "автоматическое
//! переподключение … информирование о состоянии соединения"): one line for the messenger
//! link and the call's media link — offline with the next attempt, reconnecting, restored.
//!
//! Pure state machine fed with the link states and an explicit `now`; the UI thread owns
//! one [`Tracker`] and re-renders while [`Tracker::needs_tick`] (countdown, the short
//! "restored" notice).

use std::time::{Duration, Instant};

/// How long "связь восстановлена" stays after a recovery.
pub const RESTORED_FOR: Duration = Duration::from_secs(4);

/// The messenger's server link as the UI sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessengerLink {
    /// No account signed in: the messenger part shows nothing.
    SignedOut,
    Connecting,
    Online,
    /// Lost; the next attempt is due at this instant.
    Offline {
        retry_at: Instant,
    },
}

/// The media link of the call we are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallLink {
    /// Not in a call (or it ended).
    None,
    Connecting,
    Connected,
    /// Signaling or media lost; the session is recovering.
    Reconnecting,
}

/// Severity order: a higher kind wins when both links report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Kind {
    #[default]
    None = 0,
    Restored = 1,
    Reconnecting = 2,
    Offline = 3,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Banner {
    pub kind: Kind,
    pub text: String,
}

impl Kind {
    /// The value of the window's `banner-kind` property.
    pub fn ui(self) -> i32 {
        self as i32
    }

    /// For the `TURBO_PANCAKE_BANNER` log line.
    pub fn name(self) -> &'static str {
        match self {
            Kind::None => "none",
            Kind::Restored => "restored",
            Kind::Reconnecting => "reconnecting",
            Kind::Offline => "offline",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Tracker {
    messenger: MessengerLink,
    call: CallLink,
    /// The messenger was offline since it was last online.
    messenger_lost: bool,
    /// The call lost its link since it was last connected.
    call_lost: bool,
    /// The current call's media was connected at least once: only then is a
    /// disconnected link a loss (joining reuses a session whose old link was down).
    call_up: bool,
    messenger_restored_until: Option<Instant>,
    call_restored_until: Option<Instant>,
}

impl Default for Tracker {
    fn default() -> Self {
        Self {
            messenger: MessengerLink::SignedOut,
            call: CallLink::None,
            messenger_lost: false,
            call_lost: false,
            call_up: false,
            messenger_restored_until: None,
            call_restored_until: None,
        }
    }
}

impl Tracker {
    pub fn set_messenger(&mut self, link: MessengerLink, now: Instant) {
        match link {
            MessengerLink::SignedOut => {
                self.messenger_lost = false;
                self.messenger_restored_until = None;
            }
            MessengerLink::Offline { .. } => {
                self.messenger_lost = true;
                self.messenger_restored_until = None;
            }
            MessengerLink::Online if self.messenger_lost => {
                self.messenger_lost = false;
                self.messenger_restored_until = Some(now + RESTORED_FOR);
            }
            MessengerLink::Online | MessengerLink::Connecting => {}
        }
        self.messenger = link;
    }

    pub fn set_call(&mut self, link: CallLink, now: Instant) {
        match link {
            CallLink::None => {
                // Ended (or left) while recovering: the call view says why; no "restored".
                self.call_lost = false;
                self.call_up = false;
                self.call_restored_until = None;
            }
            CallLink::Reconnecting if !self.call_up => {}
            CallLink::Reconnecting => {
                self.call_lost = true;
                self.call_restored_until = None;
            }
            CallLink::Connected if self.call_lost => {
                self.call_lost = false;
                self.call_restored_until = Some(now + RESTORED_FOR);
            }
            CallLink::Connected | CallLink::Connecting => {}
        }
        self.call_up |= link == CallLink::Connected;
        self.call = link;
    }

    /// Link names for trial logs: `(messenger, call)`.
    pub fn link_names(&self) -> (&'static str, &'static str) {
        let messenger = match self.messenger {
            MessengerLink::SignedOut => "signed_out",
            MessengerLink::Connecting => "connecting",
            MessengerLink::Online => "online",
            MessengerLink::Offline { .. } => "offline",
        };
        let call = match self.call {
            CallLink::None => "none",
            CallLink::Connecting => "connecting",
            CallLink::Connected => "connected",
            CallLink::Reconnecting => "reconnecting",
        };
        (messenger, call)
    }

    /// True while the banner changes with time alone (countdown, restored notice).
    pub fn needs_tick(&self, now: Instant) -> bool {
        matches!(self.messenger, MessengerLink::Offline { .. })
            || self.messenger_restored_until.is_some_and(|t| t > now)
            || self.call_restored_until.is_some_and(|t| t > now)
    }

    pub fn banner(&self, now: Instant) -> Banner {
        let mut problems: Vec<(Kind, String)> = Vec::new();
        match self.messenger {
            MessengerLink::Offline { retry_at } => {
                let secs = retry_at
                    .saturating_duration_since(now)
                    .as_millis()
                    .div_ceil(1000);
                let when = if secs == 0 {
                    "повтор сейчас".to_string()
                } else {
                    format!("повтор через {secs} с")
                };
                problems.push((
                    Kind::Offline,
                    format!(
                        "Нет связи с сервером, {when}. Переписка доступна из кэша, введённый текст сохранён"
                    ),
                ));
            }
            MessengerLink::Connecting if self.messenger_lost => {
                problems.push((Kind::Reconnecting, "Переподключение к серверу…".into()));
            }
            _ => {}
        }
        match self.call {
            CallLink::Reconnecting if self.call_lost => problems.push((
                Kind::Reconnecting,
                "Звонок: связь потеряна, восстановление…".into(),
            )),
            CallLink::Connecting if self.call_lost => {
                problems.push((Kind::Reconnecting, "Звонок: переподключение…".into()))
            }
            _ => {}
        }
        if let Some(kind) = problems.iter().map(|(k, _)| *k).max() {
            let text: Vec<String> = problems.into_iter().map(|(_, t)| t).collect();
            return Banner {
                kind,
                text: text.join(" · "),
            };
        }
        let mut restored = Vec::new();
        if self.messenger_restored_until.is_some_and(|t| t > now) {
            restored.push("Связь с сервером восстановлена");
        }
        if self.call_restored_until.is_some_and(|t| t > now) {
            restored.push("Звонок восстановлен");
        }
        if restored.is_empty() {
            Banner::default()
        } else {
            Banner {
                kind: Kind::Restored,
                text: restored.join(" · "),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn first_connect_and_online_show_nothing() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        assert_eq!(t.banner(t0).kind, Kind::None);
        t.set_messenger(MessengerLink::Connecting, t0);
        assert_eq!(t.banner(t0), Banner::default());
        t.set_messenger(MessengerLink::Online, t0);
        assert_eq!(t.banner(t0), Banner::default());
        assert!(!t.needs_tick(t0));
    }

    #[test]
    fn outage_counts_down_then_reconnects_and_restores_briefly() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        t.set_messenger(MessengerLink::Online, t0);
        t.set_messenger(
            MessengerLink::Offline {
                retry_at: t0 + ms(2000),
            },
            t0,
        );
        let b = t.banner(t0);
        assert_eq!(b.kind, Kind::Offline);
        assert!(b.text.contains("повтор через 2 с"), "{}", b.text);
        assert!(b.text.contains("введённый текст сохранён"));
        assert!(t.needs_tick(t0));
        assert!(t.banner(t0 + ms(1500)).text.contains("через 1 с"));
        assert!(t.banner(t0 + ms(2500)).text.contains("повтор сейчас"));

        t.set_messenger(MessengerLink::Connecting, t0 + ms(2000));
        assert_eq!(t.banner(t0 + ms(2000)).kind, Kind::Reconnecting);

        let back = t0 + ms(3000);
        t.set_messenger(MessengerLink::Online, back);
        let b = t.banner(back);
        assert_eq!(b.kind, Kind::Restored);
        assert_eq!(b.text, "Связь с сервером восстановлена");
        assert!(t.needs_tick(back));
        assert_eq!(t.banner(back + RESTORED_FOR).kind, Kind::None);
        assert!(!t.needs_tick(back + RESTORED_FOR));
        // An ordinary Connecting later (no outage) shows nothing.
        t.set_messenger(MessengerLink::Connecting, back + RESTORED_FOR);
        assert_eq!(t.banner(back + RESTORED_FOR).kind, Kind::None);
    }

    #[test]
    fn server_down_at_start_is_offline_too() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        t.set_messenger(MessengerLink::Connecting, t0);
        t.set_messenger(MessengerLink::Offline { retry_at: t0 }, t0);
        assert_eq!(t.banner(t0).kind, Kind::Offline);
    }

    #[test]
    fn sign_out_clears_the_messenger_part() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        t.set_messenger(MessengerLink::Offline { retry_at: t0 }, t0);
        t.set_messenger(MessengerLink::SignedOut, t0);
        assert_eq!(t.banner(t0).kind, Kind::None);
        t.set_messenger(MessengerLink::Online, t0);
        assert_eq!(
            t.banner(t0).kind,
            Kind::None,
            "no restored after a sign-out"
        );
    }

    #[test]
    fn call_recovery_and_call_end_while_recovering() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        t.set_call(CallLink::Connecting, t0);
        assert_eq!(
            t.banner(t0).kind,
            Kind::None,
            "first media connect is the call view's"
        );
        // Joining on a session whose earlier link was down (found in the native trial).
        t.set_call(CallLink::Reconnecting, t0);
        assert_eq!(
            t.banner(t0).kind,
            Kind::None,
            "not a loss before the first connect"
        );
        t.set_call(CallLink::Connecting, t0);
        assert_eq!(t.banner(t0).kind, Kind::None);
        t.set_call(CallLink::Connected, t0);
        t.set_call(CallLink::Reconnecting, t0);
        let b = t.banner(t0);
        assert_eq!(b.kind, Kind::Reconnecting);
        assert!(b.text.starts_with("Звонок: связь потеряна"));
        t.set_call(CallLink::Connecting, t0);
        assert_eq!(t.banner(t0).text, "Звонок: переподключение…");
        t.set_call(CallLink::Connected, t0 + ms(100));
        assert_eq!(t.banner(t0 + ms(100)).text, "Звонок восстановлен");

        // Lost again, then the call ended (server restart ends live calls, ADR 0015).
        t.set_call(CallLink::Reconnecting, t0 + ms(200));
        t.set_call(CallLink::None, t0 + ms(300));
        assert_eq!(t.banner(t0 + ms(300)).kind, Kind::None);
        // The next call starts fresh: its first link-down is again not a loss.
        t.set_call(CallLink::Reconnecting, t0 + ms(400));
        assert_eq!(t.banner(t0 + ms(400)).kind, Kind::None);
    }

    #[test]
    fn both_links_join_with_the_worst_kind() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        t.set_messenger(MessengerLink::Online, t0);
        t.set_call(CallLink::Connected, t0);
        t.set_call(CallLink::Reconnecting, t0);
        t.set_messenger(
            MessengerLink::Offline {
                retry_at: t0 + ms(4000),
            },
            t0,
        );
        let b = t.banner(t0);
        assert_eq!(b.kind, Kind::Offline);
        assert!(b.text.contains(" · Звонок: связь потеряна"), "{}", b.text);
        // Messenger back while the call still recovers: the problem wins over "restored".
        t.set_messenger(MessengerLink::Online, t0 + ms(10));
        let b = t.banner(t0 + ms(10));
        assert_eq!(b.kind, Kind::Reconnecting);
        t.set_call(CallLink::Connected, t0 + ms(20));
        let b = t.banner(t0 + ms(20));
        assert_eq!(b.kind, Kind::Restored);
        assert_eq!(
            b.text,
            "Связь с сервером восстановлена · Звонок восстановлен"
        );
        assert_eq!(Kind::Offline.ui(), 3);
        assert_eq!(Kind::Restored.name(), "restored");
    }
}
