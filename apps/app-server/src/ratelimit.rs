//! In-memory token buckets (spec §8, §12: limits on registration, login, lookups, chat
//! creation and messages). One server process (spec §2), so nothing is shared.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

use uuid::Uuid;

/// `burst` requests at once, refilled at `per_minute`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate {
    pub burst: u32,
    pub per_minute: u32,
}

impl Rate {
    pub const fn new(burst: u32, per_minute: u32) -> Self {
        Self { burst, per_minute }
    }
}

/// Limits of the messenger API; tests loosen or tighten them.
#[derive(Clone, Debug)]
pub struct Limits {
    pub register_per_ip: Rate,
    /// Failed and successful logins alike.
    pub login_per_ip: Rate,
    pub lookup_per_user: Rate,
    pub create_conversation_per_user: Rate,
    /// Sends, edits and deletes.
    pub message_writes_per_user: Rate,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            register_per_ip: Rate::new(5, 5),
            login_per_ip: Rate::new(10, 10),
            lookup_per_user: Rate::new(30, 30),
            create_conversation_per_user: Rate::new(20, 20),
            message_writes_per_user: Rate::new(30, 120),
        }
    }
}

impl Limits {
    /// Generous limits for tests that create many users from one address.
    pub fn relaxed() -> Self {
        let r = Rate::new(100_000, 100_000);
        Self {
            register_per_ip: r,
            login_per_ip: r,
            lookup_per_user: r,
            create_conversation_per_user: r,
            message_writes_per_user: r,
        }
    }
}

/// The limiters of one server.
pub struct Limiters {
    pub register: Limiter<IpAddr>,
    pub login: Limiter<IpAddr>,
    pub lookup: Limiter<Uuid>,
    pub create_conversation: Limiter<Uuid>,
    pub message_writes: Limiter<Uuid>,
}

impl Limiters {
    pub fn new(l: &Limits) -> Self {
        Self {
            register: Limiter::new(l.register_per_ip),
            login: Limiter::new(l.login_per_ip),
            lookup: Limiter::new(l.lookup_per_user),
            create_conversation: Limiter::new(l.create_conversation_per_user),
            message_writes: Limiter::new(l.message_writes_per_user),
        }
    }
}

pub struct Limiter<K> {
    rate: Rate,
    buckets: Mutex<HashMap<K, (f64, Instant)>>,
}

/// Map size at which full (idle) buckets are dropped.
const PRUNE_AT: usize = 10_000;

impl<K: Eq + Hash + Clone> Limiter<K> {
    pub fn new(rate: Rate) -> Self {
        Self {
            rate,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one token for `key`; `false` = over the limit.
    pub fn check(&self, key: &K) -> bool {
        let now = Instant::now();
        let burst = f64::from(self.rate.burst.max(1));
        let per_sec = f64::from(self.rate.per_minute) / 60.0;
        let mut map = self.buckets.lock().expect("rate limiter lock");
        if map.len() >= PRUNE_AT {
            map.retain(|_, (tokens, at)| {
                *tokens + now.duration_since(*at).as_secs_f64() * per_sec < burst
            });
        }
        let (tokens, at) = map.entry(key.clone()).or_insert((burst, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * per_sec).min(burst);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_refuse_per_key() {
        let l = Limiter::new(Rate::new(3, 1));
        assert!((0..3).all(|_| l.check(&"a")));
        assert!(!l.check(&"a"));
        assert!(l.check(&"b"));
    }
}
