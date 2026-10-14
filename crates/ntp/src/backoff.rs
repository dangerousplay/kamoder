//! Retry delays after a failed sync round (NTP-02): 5 s, doubling, capped at
//! 5 min. With Wi-Fi down that is one round of DNS + UDP every 5 minutes, no
//! busy loop; when the network comes back the clock is set within one delay.

use std::time::Duration;

/// First retry delay.
pub const INITIAL: Duration = Duration::from_secs(5);
/// Longest retry delay.
pub const MAX: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff { next: INITIAL }
    }
}

impl Backoff {
    /// The delay before the next attempt; each call doubles the following one.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (delay * 2).min(MAX);
        delay
    }

    /// Back to [`INITIAL`] after a success.
    pub fn reset(&mut self) {
        self.next = INITIAL;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_from_5_s_to_5_min() {
        let mut backoff = Backoff::default();
        let secs: Vec<u64> = (0..9).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(secs, [5, 10, 20, 40, 80, 160, 300, 300, 300]);
    }

    #[test]
    fn reset_starts_over() {
        let mut backoff = Backoff::default();
        backoff.next_delay();
        backoff.next_delay();
        backoff.reset();
        assert_eq!(backoff.next_delay(), INITIAL);
    }
}
