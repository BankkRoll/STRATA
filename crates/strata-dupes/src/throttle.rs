//! Shared I/O rate limit (token bucket) across hashing threads.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use strata_clean::CancelToken;

/// Limits the combined read rate of every hashing thread.
#[derive(Debug)]
pub(crate) struct Throttle {
    rate: Option<u64>,
    state: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Longest single sleep, so cancellation stays responsive.
const SLICE: Duration = Duration::from_millis(50);

impl Throttle {
    /// `bytes_per_sec = None` (or 0) means unlimited.
    pub(crate) fn new(bytes_per_sec: Option<u64>) -> Self {
        Self {
            rate: bytes_per_sec.filter(|&r| r > 0),
            state: Mutex::new(Bucket {
                tokens: 0.0,
                last: Instant::now(),
            }),
        }
    }

    /// Accounts for `bytes` about to be read, sleeping as needed. Returns
    /// `false` when cancelled while waiting.
    pub(crate) fn acquire(&self, bytes: u64, cancel: &CancelToken) -> bool {
        let Some(rate) = self.rate else {
            return !cancel.is_cancelled();
        };
        let rate = rate as f64;
        // A quarter second of burst smooths the per-read granularity without
        // letting an idle period turn into a long full-speed spike.
        let burst = rate / 4.0;
        let wait = {
            let mut b = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = Instant::now();
            let refill = now.duration_since(b.last).as_secs_f64() * rate;
            b.tokens = (b.tokens + refill).min(burst);
            b.last = now;
            b.tokens -= bytes as f64;
            if b.tokens < 0.0 {
                Duration::from_secs_f64(-b.tokens / rate)
            } else {
                Duration::ZERO
            }
        };
        let deadline = Instant::now() + wait;
        loop {
            if cancel.is_cancelled() {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            std::thread::sleep((deadline - now).min(SLICE));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_sleeps() {
        let t = Throttle::new(None);
        let start = Instant::now();
        for _ in 0..1000 {
            assert!(t.acquire(1 << 30, &CancelToken::new()));
        }
        assert!(start.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn limited_rate_is_respected() {
        let t = Throttle::new(Some(10 << 20));
        let start = Instant::now();
        // 5 MiB at 10 MiB/s is about 0.5 s (minus the empty initial bucket).
        for _ in 0..5 {
            assert!(t.acquire(1 << 20, &CancelToken::new()));
        }
        let e = start.elapsed();
        assert!(e >= Duration::from_millis(400), "{e:?}");
        assert!(e < Duration::from_millis(1500), "{e:?}");
    }

    #[test]
    fn cancel_interrupts_the_wait() {
        let t = Throttle::new(Some(1));
        let c = CancelToken::new();
        let c2 = c.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            c2.cancel();
        });
        let start = Instant::now();
        assert!(!t.acquire(1 << 20, &c));
        assert!(start.elapsed() < Duration::from_secs(2));
        h.join().unwrap();
    }
}
