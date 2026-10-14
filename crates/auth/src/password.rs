//! The PBKDF2-HMAC-SHA256 password hash shared by the sshd and web user files.
//!
//! Both files store the same four fields —
//! `pbkdf2-sha256:<iterations>:<salt-hex>:<digest-hex>` — so one camera
//! password costs the same to check (about a second on the 400 MHz
//! ARM926EJ-S, which has no crypto acceleration) wherever it is used, and a
//! hash produced by one tool is valid in the other file.

use crate::hex;
use ring::pbkdf2;
use std::num::NonZeroU32;

/// PBKDF2 output length, matching the SHA-256 block this uses.
pub(crate) const DIGEST_LEN: usize = 32;
/// Salt length. 16 random bytes is well past the point where collisions matter.
pub(crate) const SALT_LEN: usize = 16;
/// The only algorithm tag either file accepts.
pub(crate) const ALGORITHM: &str = "pbkdf2-sha256";

/// Default PBKDF2 iteration count.
///
/// Deliberately far below the ~600k that current guidance suggests for server
/// hardware: this runs on a 400 MHz ARM926EJ-S with no crypto acceleration,
/// where even this costs roughly a second per login attempt. The salt is what
/// defeats precomputation; the iteration count only raises the cost of an
/// offline attack on a stolen file, and a camera that cannot complete a login
/// is not a useful trade. Raise it with `--iterations` if the device is doing
/// nothing else.
pub const DEFAULT_ITERATIONS: u32 = 20_000;

/// One stored password: iteration count, salt and derived digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PasswordHash {
    iterations: NonZeroU32,
    salt: Vec<u8>,
    digest: Vec<u8>,
}

impl PasswordHash {
    /// Parses the four hash fields of a line; the error is a human-readable
    /// reason that callers attach to a file and line number.
    pub(crate) fn parse(
        algorithm: &str,
        iterations: &str,
        salt: &str,
        digest: &str,
    ) -> Result<Self, String> {
        if algorithm != ALGORITHM {
            return Err(format!("unsupported algorithm {algorithm:?}"));
        }
        let iterations: u32 = iterations
            .parse()
            .map_err(|_| "iteration count is not a number".to_string())?;
        let iterations =
            NonZeroU32::new(iterations).ok_or_else(|| "iteration count must be > 0".to_string())?;
        let salt = hex::decode(salt).map_err(|e| format!("salt: {e}"))?;
        let digest = hex::decode(digest).map_err(|e| format!("digest: {e}"))?;
        if salt.len() < 8 {
            return Err("salt is shorter than 8 bytes".into());
        }
        if digest.len() != DIGEST_LEN {
            return Err(format!("digest must be {DIGEST_LEN} bytes"));
        }
        Ok(Self {
            iterations,
            salt,
            digest,
        })
    }

    /// Hashes `password` under a fresh random salt.
    ///
    /// `SystemRandom` reads the kernel CSPRNG; using ring's own source keeps
    /// this independent of which `rand` version the SSH stack happens to pull.
    pub(crate) fn derive(password: &str, iterations: NonZeroU32) -> Result<Self, &'static str> {
        use ring::rand::SecureRandom;

        let mut salt = [0u8; SALT_LEN];
        ring::rand::SystemRandom::new()
            .fill(&mut salt)
            .map_err(|_| "cannot read random bytes from the system")?;
        let mut digest = [0u8; DIGEST_LEN];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            iterations,
            &salt,
            password.as_bytes(),
            &mut digest,
        );
        Ok(Self {
            iterations,
            salt: salt.to_vec(),
            digest: digest.to_vec(),
        })
    }

    /// Checks `password` with ring's constant-time comparison.
    pub(crate) fn verify(&self, password: &str) -> bool {
        pbkdf2::verify(
            pbkdf2::PBKDF2_HMAC_SHA256,
            self.iterations,
            &self.salt,
            password.as_bytes(),
            &self.digest,
        )
        .is_ok()
    }

    /// Spends as long as a real check would, for a user that does not exist,
    /// so response time does not reveal which usernames are valid.
    pub(crate) fn burn(password: &str) {
        let iterations =
            NonZeroU32::new(DEFAULT_ITERATIONS).expect("DEFAULT_ITERATIONS is non-zero");
        let mut sink = [0u8; DIGEST_LEN];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            iterations,
            &[0u8; SALT_LEN],
            password.as_bytes(),
            &mut sink,
        );
    }
}

impl std::fmt::Display for PasswordHash {
    /// `pbkdf2-sha256:<iterations>:<salt-hex>:<digest-hex>`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{ALGORITHM}:{}:{}:{}",
            self.iterations,
            hex::encode(&self.salt),
            hex::encode(&self.digest)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cheap(password: &str) -> PasswordHash {
        PasswordHash::derive(password, NonZeroU32::new(10).unwrap()).unwrap()
    }

    #[test]
    fn derive_then_verify() {
        let hash = cheap("hunter2");
        assert!(hash.verify("hunter2"));
        assert!(!hash.verify("hunter3"));
        assert!(!hash.verify(""));
    }

    #[test]
    fn salts_differ() {
        assert_ne!(cheap("same"), cheap("same"));
    }

    #[test]
    fn display_parse_round_trip() {
        let hash = cheap("pw");
        let text = hash.to_string();
        let fields: Vec<&str> = text.split(':').collect();
        assert_eq!(fields.len(), 4);
        let parsed = PasswordHash::parse(fields[0], fields[1], fields[2], fields[3]).unwrap();
        assert_eq!(parsed, hash);
        assert!(parsed.verify("pw"));
    }

    #[test]
    fn parse_rejects_bad_fields() {
        let salt = "00".repeat(SALT_LEN);
        let digest = "00".repeat(DIGEST_LEN);
        assert!(PasswordHash::parse("md5", "1", &salt, &digest)
            .unwrap_err()
            .contains("unsupported algorithm"));
        assert!(PasswordHash::parse(ALGORITHM, "x", &salt, &digest).is_err());
        assert!(PasswordHash::parse(ALGORITHM, "0", &salt, &digest).is_err());
        assert!(PasswordHash::parse(ALGORITHM, "1", "00", &digest)
            .unwrap_err()
            .contains("shorter than 8"));
        assert!(PasswordHash::parse(ALGORITHM, "1", &salt, "0011")
            .unwrap_err()
            .contains("32 bytes"));
        assert!(PasswordHash::parse(ALGORITHM, "1", "zz", &digest).is_err());
    }
}
