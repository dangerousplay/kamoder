//! Failed-login throttling per client IP.
//!
//! Five failures within five minutes block that IP for five minutes (429).
//! The table is in memory and bounded: a flood from many addresses evicts
//! the oldest windows rather than growing without limit on a 32 MB board.
//! A restart forgets everything, which is fine — the PBKDF2 cost (one guess
//! per second, one at a time) is the real brake; this only stops a single
//! client from monopolising it.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Failures allowed within [`WINDOW`] before blocking.
pub const MAX_FAILURES: u32 = 5;
/// Counting window, and also how long a block lasts.
pub const WINDOW: Duration = Duration::from_secs(5 * 60);
/// Distinct IPs tracked at once.
pub const MAX_TRACKED: usize = 256;

#[derive(Debug, Clone, Copy)]
struct Window {
    started: Instant,
    failures: u32,
    blocked_until: Option<Instant>,
}

impl Window {
    fn expired(&self, now: Instant) -> bool {
        let block_over = self.blocked_until.is_none_or(|until| now >= until);
        block_over && now.duration_since(self.started) >= WINDOW
    }
}

/// Tracks recent login failures per IP.
#[derive(Debug, Default)]
pub struct RateLimiter {
    windows: HashMap<IpAddr, Window>,
}

impl RateLimiter {
    /// If `ip` is blocked, how long until it may try again.
    pub fn blocked(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let until = self.windows.get(&ip)?.blocked_until?;
        (now < until).then(|| until - now)
    }

    /// Records a failed login; returns whether `ip` is now blocked.
    pub fn record_failure(&mut self, ip: IpAddr, now: Instant) -> bool {
        if !self.windows.contains_key(&ip) && self.windows.len() >= MAX_TRACKED {
            self.evict(now);
        }
        let window = self.windows.entry(ip).or_insert(Window {
            started: now,
            failures: 0,
            blocked_until: None,
        });
        if window.expired(now) {
            *window = Window {
                started: now,
                failures: 0,
                blocked_until: None,
            };
        }
        window.failures += 1;
        if window.failures >= MAX_FAILURES {
            window.blocked_until = Some(now + WINDOW);
        }
        window.blocked_until.is_some()
    }

    /// Forgets `ip` after a successful login.
    pub fn clear(&mut self, ip: IpAddr) {
        self.windows.remove(&ip);
    }

    /// Drops expired windows; if none were, the oldest one.
    fn evict(&mut self, now: Instant) {
        self.windows.retain(|_, window| !window.expired(now));
        if self.windows.len() >= MAX_TRACKED {
            let oldest = self
                .windows
                .iter()
                .min_by_key(|(_, window)| window.started)
                .map(|(ip, _)| *ip);
            if let Some(ip) = oldest {
                self.windows.remove(&ip);
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.windows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u32) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(n))
    }

    #[test]
    fn fifth_failure_blocks_for_the_window() {
        let mut limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES - 1 {
            assert!(!limiter.record_failure(ip(1), t0));
        }
        assert!(limiter.blocked(ip(1), t0).is_none());
        assert!(limiter.record_failure(ip(1), t0));
        assert_eq!(limiter.blocked(ip(1), t0), Some(WINDOW));
        assert!(limiter.blocked(ip(2), t0).is_none(), "other IPs unaffected");
        assert!(limiter.blocked(ip(1), t0 + WINDOW).is_none(), "block lifts");
    }

    #[test]
    fn failures_outside_the_window_do_not_add_up() {
        let mut limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES - 1 {
            limiter.record_failure(ip(1), t0);
        }
        assert!(!limiter.record_failure(ip(1), t0 + WINDOW));
    }

    #[test]
    fn success_clears() {
        let mut limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES - 1 {
            limiter.record_failure(ip(1), t0);
        }
        limiter.clear(ip(1));
        assert!(!limiter.record_failure(ip(1), t0));
    }

    #[test]
    fn table_is_bounded() {
        let mut limiter = RateLimiter::default();
        let t0 = Instant::now();
        for n in 0..(MAX_TRACKED as u32 * 2) {
            limiter.record_failure(ip(n), t0 + Duration::from_millis(n.into()));
        }
        assert_eq!(limiter.len(), MAX_TRACKED);
        // The newest addresses survive.
        assert!(limiter
            .windows
            .contains_key(&ip(MAX_TRACKED as u32 * 2 - 1)));
    }
}
