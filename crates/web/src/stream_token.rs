//! Short-lived tokens for URLs the browser fetches without headers.
//!
//! `<img src>` and `EventSource` cannot send `Authorization`, so the log
//! stream, snapshot and MJPEG routes also accept `?token=`. Putting the
//! session JWT there would leak a 12-hour credential into URLs (history,
//! logs, referrers), so they take only these instead: 128 random bits, valid
//! for 60 s, issued by `POST /api/logs/stream-token` to an authenticated
//! user. A token may be reused within its lifetime (an `<img>` refreshing a
//! snapshot, an `EventSource` reconnecting) and is tied to the user's
//! generation, so deleting or demoting the user ends it like a session.
//!
//! In memory only, and bounded: a handful of browser tabs never needs more
//! than [`MAX_TOKENS`].

use ring::rand::{SecureRandom, SystemRandom};
use std::time::Duration;
use subtle::ConstantTimeEq;

/// How long a stream token is accepted.
pub const TTL: Duration = Duration::from_secs(60);
/// Live tokens kept at once; the oldest is dropped beyond this.
pub const MAX_TOKENS: usize = 32;

#[derive(Debug, Clone)]
struct Entry {
    token: String,
    user: String,
    gen: u32,
    exp: u64,
}

/// Issued stream tokens.
#[derive(Debug, Default)]
pub struct StreamTokens {
    entries: Vec<Entry>,
}

impl StreamTokens {
    /// Issues a token for `user` at generation `gen`; returns it and its
    /// expiry (Unix seconds).
    pub fn issue(&mut self, user: &str, gen: u32, now: u64) -> Option<(String, u64)> {
        let mut bytes = [0u8; 16];
        SystemRandom::new().fill(&mut bytes).ok()?;
        let token = base16ct::lower::encode_string(&bytes);
        let exp = now + TTL.as_secs();
        self.entries.retain(|entry| entry.exp > now);
        if self.entries.len() >= MAX_TOKENS {
            self.entries.remove(0);
        }
        self.entries.push(Entry {
            token: token.clone(),
            user: user.to_owned(),
            gen,
            exp,
        });
        Some((token, exp))
    }

    /// The user and generation behind `token`, if it is live.
    pub fn redeem(&self, token: &str, now: u64) -> Option<(String, u32, u64)> {
        // Compare every entry without early exit so timing does not reveal a
        // matching prefix.
        let mut found = None;
        for entry in &self.entries {
            if bool::from(entry.token.as_bytes().ct_eq(token.as_bytes())) && entry.exp > now {
                found = Some((entry.user.clone(), entry.gen, entry.exp));
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_tokens_redeem_until_expiry() {
        let mut tokens = StreamTokens::default();
        let (token, exp) = tokens.issue("alice", 3, 1000).unwrap();
        assert_eq!(exp, 1000 + TTL.as_secs());
        assert_eq!(token.len(), 32);
        assert_eq!(
            tokens.redeem(&token, 1001),
            Some(("alice".to_owned(), 3, exp))
        );
        assert_eq!(tokens.redeem(&token, exp), None);
        assert_eq!(tokens.redeem("nope", 1001), None);
    }

    #[test]
    fn store_is_bounded() {
        let mut tokens = StreamTokens::default();
        let (first, _) = tokens.issue("a", 1, 0).unwrap();
        for _ in 0..MAX_TOKENS {
            tokens.issue("a", 1, 0).unwrap();
        }
        assert_eq!(tokens.entries.len(), MAX_TOKENS);
        assert_eq!(tokens.redeem(&first, 0), None);
    }
}
