//! RTSP Digest authentication (RFC 2617 as RTSP 1.0 uses it; MD5, qop=auth).
//!
//! The challenge, the `Authorization` parser and the response check are
//! `ttpkit-auth`'s. What it leaves to the server is the nonce: ours are
//! stateless, `<issued>-<mac>` with `mac = MD5(key:issued)` under a random
//! per-process key, so any nonce we issued can be recognised without a
//! table, and its age is known.
//!
//! A nonce older than [`NONCE_LIFETIME`] no longer opens a new session, but is
//! still accepted on requests for a session that already exists: clients
//! (retina, ffmpeg) keep one nonce for the whole connection and treat a 401 on
//! a keepalive as fatal, so expiring it would cut long-running streams.
//!
//! Not checked: `nc` replay and the `uri` parameter. Both would only matter
//! against someone already able to read the plaintext RTSP connection, who
//! can read the session id and the video anyway.

use msf_rtsp::header::HeaderFieldValue;
use std::time::{Duration, Instant};
use ttpkit_auth::digest::{DigestAlgorithm, DigestChallenge, DigestResponse, QualityOfProtection};
use ttpkit_auth::AuthChallenge;

/// How long a nonce can start new sessions.
pub const NONCE_LIFETIME: Duration = Duration::from_secs(600);

/// The outcome of checking an `Authorization` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Right credentials and a nonce within its lifetime.
    Valid,
    /// Right credentials, but on a nonce past its lifetime.
    Stale,
    /// Missing, malformed, or wrong.
    Invalid,
}

/// One account, one realm.
pub struct DigestAuth {
    realm: String,
    user: String,
    /// `MD5(user:realm:password)`, all `verify` needs.
    ha1: String,
    key: String,
    epoch: Instant,
}

impl DigestAuth {
    pub fn new(realm: &str, user: &str, password: &str) -> Self {
        let ha1 = DigestAlgorithm::Md5.digest(format!("{user}:{realm}:{password}").as_bytes());
        Self {
            realm: realm.to_owned(),
            user: user.to_owned(),
            ha1,
            key: format!("{:032x}", rand::random::<u128>()),
            epoch: Instant::now(),
        }
    }

    /// A `WWW-Authenticate` value with a fresh nonce.
    pub fn challenge(&self) -> DigestChallenge {
        DigestChallenge::builder(self.realm.as_str())
            .nonce(self.nonce(self.epoch.elapsed().as_secs()))
            .qops([QualityOfProtection::Auth])
            .build()
    }

    /// Checks `authorization` for `method`.
    pub fn verify(&self, method: &str, authorization: Option<&HeaderFieldValue>) -> Verdict {
        let Some(header) = authorization else {
            return Verdict::Invalid;
        };
        let Ok(text) = header.to_str() else {
            return Verdict::Invalid;
        };
        let Ok(response) = text.parse::<DigestResponse>() else {
            return Verdict::Invalid;
        };
        // `DigestResponse` does not expose the nonce; the header has the same
        // `scheme param=value, …` grammar as a challenge, so read it from there.
        let nonce = AuthChallenge::parse(header).ok().and_then(|parsed| {
            parsed.into_iter().find_map(|item| {
                item.params()
                    .iter()
                    .find(|param| param.name().eq_ignore_ascii_case("nonce"))
                    .map(|param| param.value().to_owned())
            })
        });
        let Some(issued) = nonce.as_deref().and_then(|nonce| self.issued(nonce)) else {
            return Verdict::Invalid;
        };
        let correct = response.algorithm() == DigestAlgorithm::Md5
            && response.username() == self.user
            && response.realm() == self.realm
            && response.verify(method, &self.ha1);
        if !correct {
            Verdict::Invalid
        } else if self.epoch.elapsed().as_secs().saturating_sub(issued) > NONCE_LIFETIME.as_secs() {
            Verdict::Stale
        } else {
            Verdict::Valid
        }
    }

    fn nonce(&self, issued: u64) -> String {
        format!("{issued:x}-{}", self.mac(issued))
    }

    fn mac(&self, issued: u64) -> String {
        DigestAlgorithm::Md5.digest(format!("{}:{issued}", self.key).as_bytes())
    }

    /// When `nonce` was issued, if we issued it.
    fn issued(&self, nonce: &str) -> Option<u64> {
        let (issued, mac) = nonce.split_once('-')?;
        let issued = u64::from_str_radix(issued, 16).ok()?;
        (self.mac(issued) == mac).then_some(issued)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ttpkit_auth::digest::DigestChallenge;

    fn answer(
        challenge: &DigestChallenge,
        user: &str,
        password: &str,
        method: &str,
    ) -> HeaderFieldValue {
        let response = challenge
            .response_builder()
            .cnonce("0a4f113b")
            .nc(1)
            .build(method, "rtsp://cam/vs0", None, user, password)
            .unwrap();
        HeaderFieldValue::from(response.to_string())
    }

    #[test]
    fn accepts_the_right_password_only() {
        let auth = DigestAuth::new("cam", "admin", "hunter2");
        let challenge = auth.challenge();
        let text = challenge.to_string();
        assert!(text.starts_with("Digest realm=\"cam\""), "{text}");
        assert!(text.contains("qop=\"auth\""), "{text}");
        assert!(text.contains("algorithm=MD5"), "{text}");

        let good = answer(&challenge, "admin", "hunter2", "DESCRIBE");
        assert_eq!(auth.verify("DESCRIBE", Some(&good)), Verdict::Valid);
        // Bound to the method.
        assert_eq!(auth.verify("SETUP", Some(&good)), Verdict::Invalid);
        let wrong = answer(&challenge, "admin", "hunter3", "DESCRIBE");
        assert_eq!(auth.verify("DESCRIBE", Some(&wrong)), Verdict::Invalid);
        let other = answer(&challenge, "root", "hunter2", "DESCRIBE");
        assert_eq!(auth.verify("DESCRIBE", Some(&other)), Verdict::Invalid);
        assert_eq!(auth.verify("DESCRIBE", None), Verdict::Invalid);
        let basic = HeaderFieldValue::from("Basic YWRtaW46aHVudGVyMg==");
        assert_eq!(auth.verify("DESCRIBE", Some(&basic)), Verdict::Invalid);
    }

    #[test]
    fn rejects_nonces_it_did_not_issue() {
        let auth = DigestAuth::new("cam", "admin", "hunter2");
        // Same realm and password, another process's key.
        let other = DigestAuth::new("cam", "admin", "hunter2");
        let forged = answer(&other.challenge(), "admin", "hunter2", "DESCRIBE");
        assert_eq!(auth.verify("DESCRIBE", Some(&forged)), Verdict::Invalid);
        let made_up = DigestChallenge::builder("cam")
            .nonce("0-00")
            .qops([QualityOfProtection::Auth])
            .build();
        let guessed = answer(&made_up, "admin", "hunter2", "DESCRIBE");
        assert_eq!(auth.verify("DESCRIBE", Some(&guessed)), Verdict::Invalid);
    }

    #[test]
    fn old_nonces_are_stale() {
        let mut auth = DigestAuth::new("cam", "admin", "hunter2");
        let header = answer(&auth.challenge(), "admin", "hunter2", "OPTIONS");
        // Rewind the epoch so the nonce, issued "now", reads as issued past
        // its lifetime ago.
        let Some(earlier) = auth
            .epoch
            .checked_sub(NONCE_LIFETIME + Duration::from_secs(5))
        else {
            return; // the monotonic clock is too close to its origin
        };
        auth.epoch = earlier;
        assert_eq!(auth.verify("OPTIONS", Some(&header)), Verdict::Stale);
        // A wrong password on a stale nonce is still just wrong.
        let wrong = answer(&auth.challenge(), "admin", "nope", "OPTIONS");
        assert_eq!(auth.verify("OPTIONS", Some(&wrong)), Verdict::Invalid);
    }
}
