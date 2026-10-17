//! HS256 session tokens: `jsonwebtoken` for the token format, `ring` for the
//! HMAC.
//!
//! `jsonwebtoken` 10+ splits its crypto behind a `CryptoProvider`. Its two
//! built-in backends are both unusable here: `aws_lc_rs` does not build for
//! ARMv5TE, and `rust_crypto` pulls `rsa` (RUSTSEC-2023-0071, unfixed) plus
//! the P-256/P-384/Ed25519 stacks, none of which a single-key HS256 session
//! needs. So the crate is built with no backend and [`RING_PROVIDER`] supplies
//! HS256 on `ring::hmac`, the same crypto stack as the rest of this crate.
//!
//! What this accepts:
//!
//! - only `alg: HS256` — the header is checked before anything else, which
//!   rules out `alg: none` and algorithm confusion;
//! - the signature is checked with `ring::hmac::verify` (constant time)
//!   *before* the claims are parsed, so unauthenticated claims JSON never
//!   reaches serde (the small header object does, to read `alg`);
//! - base64url must be unpadded;
//! - `exp` is compared against a caller-supplied `now`, keeping the check
//!   deterministic in tests; `jsonwebtoken`'s own clock check is turned off.
//!
//! Checking a token costs one HMAC-SHA256 over ~200 bytes — negligible even
//! on the 400 MHz ARM926EJ-S, unlike the PBKDF2 login it stands in for.
//!
//! The 32-byte key lives in a 0600 file (`etc/web-jwt.key`) created on first
//! use; deleting it invalidates every session.

use crate::web_users::Role;
use jsonwebtoken::crypto::{CryptoProvider, JwtSigner, JwtVerifier, KeyUtils};
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::signature::{self, Signer, Verifier};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use ring::hmac;
use serde::{Deserialize, Serialize};
use snafu::{ensure, ResultExt, Snafu};
use std::path::{Path, PathBuf};

/// Signing key length: the HMAC-SHA256 output size, as RFC 7518 recommends.
pub const KEY_LEN: usize = 32;

/// Why a token was refused. Callers answer 401 for all of them; the variants
/// exist for logs and tests.
#[derive(Debug, Snafu, PartialEq, Eq)]
pub enum VerifyError {
    #[snafu(display("token is not three base64url segments"))]
    Malformed,

    #[snafu(display("token signature does not match"))]
    BadSignature,

    #[snafu(display("token header is not HS256"))]
    UnsupportedHeader,

    #[snafu(display("token payload is not valid claims"))]
    BadClaims,

    #[snafu(display("token expired"))]
    Expired,
}

#[derive(Debug, Snafu)]
pub enum KeyError {
    #[snafu(display("cannot read {}: {source}", path.display()))]
    ReadKey {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("cannot write {}: {source}", path.display()))]
    WriteKey {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display(
        "{} is accessible to other users (mode {mode:o}); run: chmod 600 {}",
        path.display(),
        path.display()
    ))]
    KeyPermissions { path: PathBuf, mode: u32 },

    #[snafu(display(
        "{} holds {len} bytes, expected {KEY_LEN}; delete it to generate a new key (this ends all sessions)",
        path.display()
    ))]
    KeyLength { path: PathBuf, len: usize },

    #[snafu(display("cannot read random bytes from the system"))]
    Random,
}

/// What a session token asserts. Times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// Username.
    pub sub: String,
    pub role: Role,
    /// The user's generation when the token was issued; see
    /// [`crate::web_users`].
    pub gen: u32,
    pub iat: u64,
    pub exp: u64,
}

/// HS256 on `ring::hmac`, the only algorithm this provider offers.
///
/// Installed process-wide by [`Jwt::new`]. Asking it for anything but HS256
/// with an HMAC secret is an error, and the JWK helpers are left
/// unimplemented: tokens here never carry or need a JWK.
pub static RING_PROVIDER: CryptoProvider = CryptoProvider {
    signer_factory: |algorithm, key| {
        ensure_hs256(algorithm)?;
        Ok(Box::new(RingHs256(hmac::Key::new(
            hmac::HMAC_SHA256,
            key.as_bytes(),
        ))))
    },
    verifier_factory: |algorithm, key| {
        ensure_hs256(algorithm)?;
        Ok(Box::new(RingHs256(hmac::Key::new(
            hmac::HMAC_SHA256,
            key.try_get_as_bytes()?,
        ))))
    },
    key_utils: KeyUtils::new_unimplemented(),
};

fn ensure_hs256(algorithm: &Algorithm) -> jsonwebtoken::errors::Result<()> {
    match algorithm {
        Algorithm::HS256 => Ok(()),
        _ => Err(ErrorKind::UnsupportedAlgorithm.into()),
    }
}

struct RingHs256(hmac::Key);

impl Signer<Vec<u8>> for RingHs256 {
    fn try_sign(&self, message: &[u8]) -> Result<Vec<u8>, signature::Error> {
        Ok(hmac::sign(&self.0, message).as_ref().to_vec())
    }
}

impl Verifier<Vec<u8>> for RingHs256 {
    fn verify(&self, message: &[u8], tag: &Vec<u8>) -> Result<(), signature::Error> {
        hmac::verify(&self.0, message, tag).map_err(|_| signature::Error::new())
    }
}

impl JwtSigner for RingHs256 {
    fn algorithm(&self) -> Algorithm {
        Algorithm::HS256
    }
}

impl JwtVerifier for RingHs256 {
    fn algorithm(&self) -> Algorithm {
        Algorithm::HS256
    }
}

/// Signs and verifies tokens with one HMAC key.
#[derive(Clone)]
pub struct Jwt {
    encoding: EncodingKey,
    decoding: DecodingKey,
    validation: Validation,
}

impl std::fmt::Debug for Jwt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Jwt { key: <redacted> }")
    }
}

impl Jwt {
    /// Uses `key` (normally the 32 bytes from [`load_or_create_key`]), and
    /// installs [`RING_PROVIDER`] as `jsonwebtoken`'s backend if nothing is
    /// installed yet.
    pub fn new(key: &[u8]) -> Self {
        if let Err(installed) = RING_PROVIDER.install_default() {
            assert!(
                std::ptr::eq(installed, &RING_PROVIDER),
                "another jsonwebtoken CryptoProvider is installed"
            );
        }
        let mut validation = Validation::new(Algorithm::HS256);
        // `verify` compares `exp` with the caller's clock instead.
        validation.validate_exp = false;
        validation.leeway = 0;
        Self {
            encoding: EncodingKey::from_secret(key),
            decoding: DecodingKey::from_secret(key),
            validation,
        }
    }

    /// Issues a token for `claims`.
    pub fn sign(&self, claims: &Claims) -> String {
        jsonwebtoken::encode(&Header::new(Algorithm::HS256), claims, &self.encoding)
            .expect("HS256 signing with ring cannot fail")
    }

    /// Checks a token and returns its claims if it is authentic and
    /// `now < exp`.
    ///
    /// Whether the user still exists with the same role and generation is the
    /// caller's check, against [`crate::web_users::WebUsers::lookup`].
    pub fn verify(&self, token: &str, now: u64) -> Result<Claims, VerifyError> {
        // Read the header separately so a bad header and bad claims (both
        // JSON errors inside `decode`) are told apart.
        let header = jsonwebtoken::decode_header(token).map_err(|e| match e.kind() {
            ErrorKind::Json(_) => VerifyError::UnsupportedHeader,
            _ => VerifyError::Malformed,
        })?;
        ensure!(header.alg == Algorithm::HS256, UnsupportedHeaderSnafu);
        let claims = jsonwebtoken::decode::<Claims>(token, &self.decoding, &self.validation)
            .map_err(|e| match e.kind() {
                ErrorKind::InvalidSignature => VerifyError::BadSignature,
                ErrorKind::InvalidAlgorithm | ErrorKind::UnsupportedAlgorithm => {
                    VerifyError::UnsupportedHeader
                }
                ErrorKind::Json(_)
                | ErrorKind::MissingRequiredClaim(_)
                | ErrorKind::InvalidClaimFormat(_) => VerifyError::BadClaims,
                _ => VerifyError::Malformed,
            })?
            .claims;
        ensure!(now < claims.exp, ExpiredSnafu);
        Ok(claims)
    }
}

/// Reads the signing key at `path`, generating and storing a new random one
/// (mode 0600, written atomically) if the file does not exist.
///
/// An existing key of the wrong length, or one other users can read (off
/// FAT), is an error rather than silently replaced: either points at a
/// problem someone should look at.
pub fn load_or_create_key(path: &Path) -> Result<[u8; KEY_LEN], KeyError> {
    match std::fs::metadata(path) {
        Ok(metadata) => {
            if let Some(mode) = crate::perms::too_open(path, &metadata) {
                return KeyPermissionsSnafu { path, mode }.fail();
            }
            let bytes = std::fs::read(path).context(ReadKeySnafu { path })?;
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| KeyError::KeyLength {
                    path: path.to_owned(),
                    len: bytes.len(),
                })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            use ring::rand::SecureRandom;

            let mut key = [0u8; KEY_LEN];
            ring::rand::SystemRandom::new()
                .fill(&mut key)
                .map_err(|_| KeyError::Random)?;
            crate::atomic::write(path, &key, 0o600).context(WriteKeySnafu { path })?;
            tracing::info!(path = %path.display(), "generated a new web session key");
            Ok(key)
        }
        Err(source) => Err(KeyError::ReadKey {
            path: path.to_owned(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use std::os::unix::fs::PermissionsExt;

    /// The header the previous hand-rolled signer pinned; tokens it issued
    /// must still verify after the switch to `jsonwebtoken`.
    const LEGACY_HEADER: &[u8] = br#"{"alg":"HS256","typ":"JWT"}"#;

    const NOW: u64 = 1_800_000_000;

    fn claims() -> Claims {
        Claims {
            sub: "admin".into(),
            role: Role::Admin,
            gen: 3,
            iat: NOW,
            exp: NOW + 12 * 3600,
        }
    }

    fn jwt() -> Jwt {
        Jwt::new(&[7u8; KEY_LEN])
    }

    fn b64(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Re-signs arbitrary header and payload bytes with the test key.
    fn forge(header: &[u8], payload: &[u8], key: &[u8]) -> String {
        let signed = format!("{}.{}", b64(header), b64(payload));
        let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), signed.as_bytes());
        format!("{signed}.{}", b64(tag.as_ref()))
    }

    #[test]
    fn valid_token_round_trips() {
        let token = jwt().sign(&claims());
        assert_eq!(token.split('.').count(), 3);
        assert_eq!(jwt().verify(&token, NOW), Ok(claims()));
        assert_eq!(jwt().verify(&token, NOW + 12 * 3600 - 1), Ok(claims()));
    }

    #[test]
    fn header_is_hs256() {
        let token = jwt().sign(&claims());
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert_eq!(header.alg, Algorithm::HS256);
        assert_eq!(header.typ.as_deref(), Some("JWT"));
    }

    #[test]
    fn legacy_tokens_still_verify() {
        let payload = serde_json::to_vec(&claims()).unwrap();
        let token = forge(LEGACY_HEADER, &payload, &[7u8; KEY_LEN]);
        assert_eq!(jwt().verify(&token, NOW), Ok(claims()));
    }

    #[test]
    fn expired_token_is_refused() {
        let token = jwt().sign(&claims());
        assert_eq!(
            jwt().verify(&token, NOW + 12 * 3600),
            Err(VerifyError::Expired)
        );
    }

    #[test]
    fn tampered_payload_is_refused() {
        let token = jwt().sign(&claims());
        let parts: Vec<&str> = token.split('.').collect();
        let mut evil = claims();
        evil.sub = "root".into();
        evil.exp += 1_000_000;
        let forged = format!(
            "{}.{}.{}",
            parts[0],
            b64(&serde_json::to_vec(&evil).unwrap()),
            parts[2]
        );
        assert_eq!(jwt().verify(&forged, NOW), Err(VerifyError::BadSignature));
    }

    #[test]
    fn tampered_signature_is_refused() {
        let token = jwt().sign(&claims());
        let (signed, sig) = token.rsplit_once('.').unwrap();
        let mut sig = URL_SAFE_NO_PAD.decode(sig).unwrap();
        sig[0] ^= 1;
        let forged = format!("{signed}.{}", b64(&sig));
        assert_eq!(jwt().verify(&forged, NOW), Err(VerifyError::BadSignature));
        // Truncated signature.
        let short = format!("{signed}.{}", b64(&sig[..16]));
        assert_eq!(jwt().verify(&short, NOW), Err(VerifyError::BadSignature));
    }

    #[test]
    fn wrong_key_is_refused() {
        let token = Jwt::new(&[8u8; KEY_LEN]).sign(&claims());
        assert_eq!(jwt().verify(&token, NOW), Err(VerifyError::BadSignature));
    }

    #[test]
    fn alg_none_is_refused() {
        let payload = serde_json::to_vec(&claims()).unwrap();
        let unsigned = format!(
            "{}.{}.",
            b64(br#"{"alg":"none","typ":"JWT"}"#),
            b64(&payload)
        );
        assert_eq!(
            jwt().verify(&unsigned, NOW),
            Err(VerifyError::UnsupportedHeader)
        );
        // Even correctly MACed, any algorithm but HS256 is refused.
        for header in [
            &br#"{"alg":"none","typ":"JWT"}"#[..],
            br#"{"alg":"HS512","typ":"JWT"}"#,
            br#"{"alg":"RS256","typ":"JWT"}"#,
            br#"{"typ":"JWT"}"#,
            b"not json",
        ] {
            let token = forge(header, &payload, &[7u8; KEY_LEN]);
            assert_eq!(
                jwt().verify(&token, NOW),
                Err(VerifyError::UnsupportedHeader),
                "{}",
                String::from_utf8_lossy(header)
            );
        }
        // Only `alg` is pinned: other header fields are fine under a valid MAC,
        // which only the key holder can produce.
        let token = forge(
            br#"{"typ":"JWT","alg":"HS256","kid":"x"}"#,
            &payload,
            &[7u8; KEY_LEN],
        );
        assert_eq!(jwt().verify(&token, NOW), Ok(claims()));
    }

    #[test]
    fn malformed_tokens_are_refused() {
        let token = jwt().sign(&claims());
        for bad in [
            "",
            "abc",
            "a.b",
            "a.b.c.d",
            &format!("{token}.x"),
            &format!("{token}="),
            &token.replace('.', ".."),
        ] {
            assert!(jwt().verify(bad, NOW).is_err(), "{bad:?}");
        }
        // Authentic but not our claims.
        let token = forge(LEGACY_HEADER, br#"{"sub":"a"}"#, &[7u8; KEY_LEN]);
        assert_eq!(jwt().verify(&token, NOW), Err(VerifyError::BadClaims));
    }

    #[test]
    fn claims_json_shape() {
        let value = serde_json::to_value(claims()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "sub": "admin", "role": "admin", "gen": 3,
                "iat": NOW, "exp": NOW + 12 * 3600
            })
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("anyka-auth-jwt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn key_is_created_once_and_private() {
        let dir = scratch("key");
        let path = dir.join("web-jwt.key");
        let first = load_or_create_key(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), first);
        assert_eq!(load_or_create_key(&path).unwrap(), first);

        // Deleting the key yields a new one, invalidating old tokens.
        let token = Jwt::new(&first).sign(&claims());
        std::fs::remove_file(&path).unwrap();
        let second = load_or_create_key(&path).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            Jwt::new(&second).verify(&token, NOW),
            Err(VerifyError::BadSignature)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bad_key_files_are_refused() {
        let dir = scratch("badkey");
        let path = dir.join("web-jwt.key");
        crate::atomic::write(&path, &[1u8; 5], 0o600).unwrap();
        assert!(matches!(
            load_or_create_key(&path),
            Err(KeyError::KeyLength { len: 5, .. })
        ));
        crate::atomic::write(&path, &[1u8; KEY_LEN], 0o600).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            load_or_create_key(&path),
            Err(KeyError::KeyPermissions { mode: 0o644, .. })
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
