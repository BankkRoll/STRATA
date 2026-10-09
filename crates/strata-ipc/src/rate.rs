//! Per-connection request rate limiting.

use std::time::{Duration, Instant};

/// Token-bucket parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// Bucket capacity: requests allowed in a burst.
    pub burst: u32,
    /// Sustained requests per second.
    pub per_second: u32,
}

impl Default for RateLimit {
    /// 200-request bursts, 100 requests/s sustained. The app issues a handful
    /// of requests per scan plus one `ReadRecords` per USN tick (250 ms), so
    /// this only trips on a misbehaving or hostile client.
    fn default() -> Self {
        Self {
            burst: 200,
            per_second: 100,
        }
    }
}

/// A token bucket. Time is passed in explicitly so tests are deterministic.
///
/// # Example
///
/// ```
/// use std::time::{Duration, Instant};
/// use strata_ipc::rate::{RateLimit, TokenBucket};
/// let t0 = Instant::now();
/// let mut b = TokenBucket::new(RateLimit { burst: 2, per_second: 1 }, t0);
/// assert!(b.try_take(t0).is_ok());
/// assert!(b.try_take(t0).is_ok());
/// assert!(b.try_take(t0).is_err());
/// assert!(b.try_take(t0 + Duration::from_secs(1)).is_ok());
/// ```
#[derive(Debug, Clone)]
pub struct TokenBucket {
    limit: RateLimit,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// A full bucket.
    #[must_use]
    pub fn new(limit: RateLimit, now: Instant) -> Self {
        Self {
            limit,
            tokens: f64::from(limit.burst),
            last: now,
        }
    }

    /// Takes one token, or returns how long until one is available.
    pub fn try_take(&mut self, now: Instant) -> Result<(), Duration> {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = self.last.max(now);
        let rate = f64::from(self.limit.per_second);
        self.tokens = (self.tokens + elapsed * rate).min(f64::from(self.limit.burst));
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Ok(());
        }
        if rate <= 0.0 {
            return Err(Duration::MAX);
        }
        Err(Duration::from_secs_f64((1.0 - self.tokens) / rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refills_at_the_configured_rate() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(
            RateLimit {
                burst: 10,
                per_second: 100,
            },
            t0,
        );
        for _ in 0..10 {
            b.try_take(t0).unwrap();
        }
        let wait = b.try_take(t0).unwrap_err();
        assert!(
            wait <= Duration::from_millis(10) && wait > Duration::ZERO,
            "{wait:?}"
        );
        assert!(b.try_take(t0 + Duration::from_millis(10)).is_ok());
        // Long idle never exceeds the burst.
        let later = t0 + Duration::from_secs(3600);
        for _ in 0..10 {
            b.try_take(later).unwrap();
        }
        assert!(b.try_take(later).is_err());
    }

    #[test]
    fn clock_going_backwards_is_harmless() {
        let t0 = Instant::now() + Duration::from_secs(10);
        let mut b = TokenBucket::new(
            RateLimit {
                burst: 1,
                per_second: 1,
            },
            t0,
        );
        b.try_take(t0).unwrap();
        assert!(b.try_take(t0 - Duration::from_secs(5)).is_err());
    }

    #[test]
    fn zero_rate_never_refills() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(
            RateLimit {
                burst: 0,
                per_second: 0,
            },
            t0,
        );
        assert_eq!(b.try_take(t0), Err(Duration::MAX));
    }
}
