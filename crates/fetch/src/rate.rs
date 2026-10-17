//! Token bucket that caps the download rate.
//!
//! The camera's SSV6x5x Wi-Fi drops off the network under sustained full-speed
//! transfers. Reading the socket no faster than the cap lets TCP flow control
//! slow the sender, so the radio never sees a burst bigger than the bucket.
//!
//! Debt model: a chunk is always taken in full (it has already been read), the
//! balance may go negative, and the caller sleeps until it is back at zero.
//! The burst is a quarter second of rate (at least 16 KiB) so one read of up
//! to 64 KiB does not stall for long at low caps.

use std::time::{Duration, Instant};

/// Smallest burst, so tiny caps still allow a TLS record per wake-up.
const MIN_BURST: f64 = 16.0 * 1024.0;

#[derive(Debug)]
pub(crate) struct TokenBucket {
    rate: f64,
    capacity: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub(crate) fn new(bytes_per_sec: u32, now: Instant) -> Self {
        let rate = f64::from(bytes_per_sec.max(1));
        let capacity = (rate / 4.0).max(MIN_BURST);
        Self {
            rate,
            capacity,
            tokens: capacity,
            last: now,
        }
    }

    /// Accounts `n` bytes just read and returns how long to wait before the
    /// next read.
    pub(crate) fn take(&mut self, n: usize, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.tokens -= n as f64;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / self.rate)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_paced() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(64 * 1024, t0);
        // Burst is 16 KiB: the first 16 KiB are free.
        assert_eq!(b.take(16 * 1024, t0), Duration::ZERO);
        // The next 32 KiB cost half a second at 64 KiB/s.
        let wait = b.take(32 * 1024, t0);
        assert!((wait.as_secs_f64() - 0.5).abs() < 1e-6, "{wait:?}");
        // After sleeping that long the debt is cleared.
        let t1 = t0 + wait;
        assert_eq!(b.take(0, t1), Duration::ZERO);
    }

    #[test]
    fn idle_refill_is_capped() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(1024 * 1024, t0);
        let _ = b.take(256 * 1024, t0);
        // A long idle refills only up to the 256 KiB burst.
        let t1 = t0 + Duration::from_secs(60);
        assert_eq!(b.take(256 * 1024, t1), Duration::ZERO);
        assert!(b.take(1024 * 1024, t1) >= Duration::from_millis(999));
    }
}
