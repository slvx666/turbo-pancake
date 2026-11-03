//! Per-viewer layer choice for the watched share (S4, spec §5.1/§5.3, ADR 0011).
//!
//! A share has two layers: the high (main) one and a low preview (≈ 320 px, ≈ 10 fps).
//! A viewer's main picture carries one of them; which one follows from what the viewer
//! asked for, whether its downlink loses packets ([`NetworkPolicy`]) and whether the
//! server's budget for high-quality streams allows another one (admission control).

use std::time::{Duration, Instant};

/// What a viewer asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LayerChoice {
    /// The server picks (high unless the network or the budget is in the way).
    #[default]
    Auto,
    High,
    Low,
}

/// What flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    High,
    Low,
}

/// Why a viewer gets the layer it gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerReason {
    Requested,
    /// Auto: the viewer's downlink loses packets.
    Network,
    /// The high-stream budget of the server is used up.
    Admission,
    /// The sharer sends one layer only.
    SingleLayer,
}

/// The layer a viewer gets. `net_ok`: its network allows the high layer; `admitted`: the
/// high-stream budget has room for it.
pub fn choose(
    requested: LayerChoice,
    has_low: bool,
    net_ok: bool,
    admitted: bool,
) -> (Layer, LayerReason) {
    if !has_low {
        return (Layer::High, LayerReason::SingleLayer);
    }
    match requested {
        LayerChoice::Low => (Layer::Low, LayerReason::Requested),
        LayerChoice::High if admitted => (Layer::High, LayerReason::Requested),
        LayerChoice::High => (Layer::Low, LayerReason::Admission),
        LayerChoice::Auto if !net_ok => (Layer::Low, LayerReason::Network),
        LayerChoice::Auto if !admitted => (Layer::Low, LayerReason::Admission),
        LayerChoice::Auto => (Layer::High, LayerReason::Requested),
    }
}

/// Decides from the viewer's receiver reports (loss of the forwarded main stream, about
/// once a second) whether its network carries the high layer.
///
/// Down: two reports in a row with ≥ 8 % loss, or one with ≥ 20 %. Up: after the hold
/// time, 8 reports in a row with ≤ 2 % loss. On the low layer the link has room, so a
/// clean low layer does not prove the high one fits: an upgrade that falls back within
/// [`NetworkPolicy::PROBE_FAIL`] counts as failed and doubles the hold (up to 64 s).
#[derive(Debug, Clone)]
pub struct NetworkPolicy {
    high_ok: bool,
    bad_reports: u32,
    good_reports: u32,
    hold: Duration,
    hold_until: Option<Instant>,
    upgraded_at: Option<Instant>,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkPolicy {
    pub const DOWN_LOSS: f32 = 0.08;
    pub const DOWN_LOSS_NOW: f32 = 0.20;
    pub const UP_LOSS: f32 = 0.02;
    pub const UP_REPORTS: u32 = 8;
    pub const FIRST_HOLD: Duration = Duration::from_secs(8);
    pub const MAX_HOLD: Duration = Duration::from_secs(64);
    pub const PROBE_FAIL: Duration = Duration::from_secs(10);

    pub fn new() -> Self {
        Self {
            high_ok: true,
            bad_reports: 0,
            good_reports: 0,
            hold: Self::FIRST_HOLD,
            hold_until: None,
            upgraded_at: None,
        }
    }

    pub fn allows_high(&self) -> bool {
        self.high_ok
    }

    /// One receiver report window; returns true if the decision changed.
    pub fn report(&mut self, now: Instant, loss: f32) -> bool {
        if self.high_ok {
            self.bad_reports = if loss >= Self::DOWN_LOSS {
                self.bad_reports + 1
            } else {
                0
            };
            if loss >= Self::DOWN_LOSS_NOW || self.bad_reports >= 2 {
                let failed_probe = self
                    .upgraded_at
                    .is_some_and(|t| now.saturating_duration_since(t) < Self::PROBE_FAIL);
                self.hold = if failed_probe {
                    (self.hold * 2).min(Self::MAX_HOLD)
                } else {
                    Self::FIRST_HOLD
                };
                self.high_ok = false;
                self.bad_reports = 0;
                self.good_reports = 0;
                self.hold_until = Some(now + self.hold);
                self.upgraded_at = None;
                return true;
            }
            return false;
        }
        self.good_reports = if loss <= Self::UP_LOSS {
            self.good_reports + 1
        } else {
            0
        };
        let held = self.hold_until.is_none_or(|t| now >= t);
        if held && self.good_reports >= Self::UP_REPORTS {
            self.high_ok = true;
            self.good_reports = 0;
            self.upgraded_at = Some(now);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_follows_request_network_and_budget() {
        use Layer::*;
        use LayerChoice as C;
        use LayerReason as R;
        assert_eq!(choose(C::Auto, true, true, true), (High, R::Requested));
        assert_eq!(choose(C::Auto, true, false, true), (Low, R::Network));
        assert_eq!(choose(C::Auto, true, true, false), (Low, R::Admission));
        assert_eq!(choose(C::High, true, false, true), (High, R::Requested));
        assert_eq!(choose(C::High, true, true, false), (Low, R::Admission));
        assert_eq!(choose(C::Low, true, true, true), (Low, R::Requested));
        // One layer only: that one, whatever was asked.
        for c in [C::Auto, C::High, C::Low] {
            assert_eq!(choose(c, false, false, false), (High, R::SingleLayer));
        }
    }

    #[test]
    fn network_policy_goes_down_on_loss_and_probes_up_with_backoff() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_secs(n);
        let mut p = NetworkPolicy::new();
        assert!(p.allows_high());
        // One bad report is noise.
        assert!(!p.report(s(1), 0.10));
        assert!(!p.report(s(2), 0.0));
        assert!(p.allows_high());
        // Two in a row: down.
        assert!(!p.report(s(3), 0.10));
        assert!(p.report(s(4), 0.10));
        assert!(!p.allows_high());
        // Clean reports, but the hold (8 s from t=4) is not over before t=12.
        for n in 5..12 {
            assert!(!p.report(s(n), 0.0), "t={n}");
        }
        assert!(p.report(s(12), 0.0), "8 clean reports and the hold is over");
        assert!(p.allows_high());
        // The probe fails at once (heavy loss): down, hold doubles to 16 s.
        assert!(p.report(s(13), 0.30));
        for n in 14..29 {
            assert!(!p.report(s(n), 0.0), "t={n}");
        }
        assert!(p.report(s(29), 0.0));
        // Stays up for longer than PROBE_FAIL: the next drop starts from the first hold.
        assert!(!p.report(s(45), 0.0));
        assert!(p.report(s(46), 0.25));
        for n in 47..54 {
            assert!(!p.report(s(n), 0.0), "t={n}");
        }
        assert!(p.report(s(54), 0.0));
        // Moderate loss resets the clean streak.
        let mut q = NetworkPolicy::new();
        q.report(s(0), 0.5);
        for n in 1..9 {
            q.report(s(n), if n == 5 { 0.05 } else { 0.0 });
        }
        assert!(!q.allows_high(), "streak broken at t=5");
    }
}
