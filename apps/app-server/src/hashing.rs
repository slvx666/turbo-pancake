//! Bounded password hashing (S8 step 4, issue 45). Every Argon2id hash or verification
//! takes 19 MiB and ≈ 20–40 ms of CPU; without a bound a burst of logins could run one
//! per blocking thread (up to 512, ≈ 9.5 GiB). At most `concurrency` run at once; up to
//! `queue` more wait (at most `max_wait`); beyond that the request is refused at once
//! with `503 Unavailable` + `Retry-After`, never with a wrong-password answer.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::api::{ApiError, ApiResult, Code};

pub struct Hasher {
    slots: Semaphore,
    concurrency: usize,
    queue: usize,
    max_wait: Duration,
    waiting: AtomicUsize,
    running: AtomicUsize,
    peak_running: AtomicUsize,
    refused: AtomicU64,
    done: AtomicU64,
}

/// Snapshot for `/v1/server-stats`.
#[derive(Debug, Clone, Copy)]
pub struct HasherStats {
    pub concurrency: usize,
    pub queue: usize,
    pub running: usize,
    pub waiting: usize,
    pub peak_running: usize,
    pub refused: u64,
    pub done: u64,
}

fn busy() -> ApiError {
    ApiError::new(Code::Unavailable, "server busy, try again")
}

impl Hasher {
    pub fn new(concurrency: usize, queue: usize, max_wait: Duration) -> Self {
        let concurrency = concurrency.max(1);
        Self {
            slots: Semaphore::new(concurrency),
            concurrency,
            queue,
            max_wait,
            waiting: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            peak_running: AtomicUsize::new(0),
            refused: AtomicU64::new(0),
            done: AtomicU64::new(0),
        }
    }

    /// Runs `f` on the blocking pool once a slot is free.
    pub async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> ApiResult<T> {
        let permit = match self.slots.try_acquire() {
            Ok(p) => p,
            Err(_) => {
                // Count ourselves in the queue first: two callers can never both slip in
                // as the last waiter.
                if self.waiting.fetch_add(1, Ordering::SeqCst) >= self.queue {
                    self.waiting.fetch_sub(1, Ordering::SeqCst);
                    self.refused.fetch_add(1, Ordering::Relaxed);
                    return Err(busy());
                }
                let got = tokio::time::timeout(self.max_wait, self.slots.acquire()).await;
                self.waiting.fetch_sub(1, Ordering::SeqCst);
                match got {
                    Ok(Ok(p)) => p,
                    _ => {
                        self.refused.fetch_add(1, Ordering::Relaxed);
                        return Err(busy());
                    }
                }
            }
        };
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_running.fetch_max(now, Ordering::SeqCst);
        let out = tokio::task::spawn_blocking(f).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        self.done.fetch_add(1, Ordering::Relaxed);
        drop(permit);
        out.map_err(|_| ApiError::internal())
    }

    pub fn stats(&self) -> HasherStats {
        HasherStats {
            concurrency: self.concurrency,
            queue: self.queue,
            running: self.running.load(Ordering::Relaxed),
            waiting: self.waiting.load(Ordering::Relaxed),
            peak_running: self.peak_running.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            done: self.done.load(Ordering::Relaxed),
        }
    }
}

/// Default number of hashes at once: half the CPUs, 1..=4 (4 × 19 MiB = 76 MiB).
pub fn default_concurrency() -> usize {
    std::thread::available_parallelism()
        .map_or(2, |n| n.get() / 2)
        .clamp(1, 4)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::Hasher;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bounded_running_and_refuses_beyond_the_queue() {
        let h = Arc::new(Hasher::new(2, 3, Duration::from_secs(5)));
        let tasks: Vec<_> = (0..12)
            .map(|_| {
                let h = h.clone();
                tokio::spawn(async move {
                    h.run(|| std::thread::sleep(Duration::from_millis(150)))
                        .await
                        .is_ok()
                })
            })
            .collect();
        let mut ok = 0;
        for t in tasks {
            ok += usize::from(t.await.unwrap());
        }
        let s = h.stats();
        // 2 running + 3 waiting are served; the other 7 are refused at once.
        assert_eq!(ok, 5, "{s:?}");
        assert_eq!(s.refused, 7, "{s:?}");
        assert_eq!(s.peak_running, 2, "{s:?}");
        assert_eq!((s.running, s.waiting), (0, 0), "{s:?}");
    }

    #[tokio::test]
    async fn a_waiter_gives_up_after_max_wait() {
        let h = Arc::new(Hasher::new(1, 8, Duration::from_millis(50)));
        let slow = {
            let h = h.clone();
            tokio::spawn(async move {
                h.run(|| std::thread::sleep(Duration::from_millis(400)))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let late = h.run(|| ()).await;
        assert!(late.is_err());
        assert!(slow.await.unwrap().is_ok());
        assert_eq!(h.stats().refused, 1);
    }
}
