use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use sha1::Digest;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::types::UsernameToken;

/// Verify a WS-UsernameToken against expected credentials.
///
/// Supports two modes:
/// - **plaintext**: direct comparison when `nonce` is empty.
/// - **digest**: `base64(SHA1(base64_decode(Nonce) + Created + Password))`
///   when `nonce` is non-empty.
pub fn verify_username_token(
    token: &UsernameToken,
    expected_user: &str,
    expected_pass: &str,
) -> bool {
    if token.username != expected_user {
        return false;
    }

    if token.nonce.is_empty() {
        // Plaintext mode
        constant_time_eq(token.password.as_bytes(), expected_pass.as_bytes())
    } else {
        // Digest mode
        let computed = compute_password_digest(&token.nonce, &token.created, expected_pass);
        constant_time_eq(token.password.as_bytes(), computed.as_bytes())
    }
}

/// Compute the ONVIF password digest:
/// `BASE64(SHA1(base64_decode(Nonce) + Created + Password))`
pub fn compute_password_digest(nonce: &str, created: &str, password: &str) -> String {
    let nonce_bytes = BASE64
        .decode(nonce)
        .unwrap_or_else(|_| nonce.as_bytes().to_vec());

    let mut hasher = sha1::Sha1::new();
    hasher.update(&nonce_bytes);
    hasher.update(created.as_bytes());
    hasher.update(password.as_bytes());
    let result = hasher.finalize();

    BASE64.encode(result)
}

/// Constant-time byte comparison to prevent timing side-channels on
/// password validation.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result: u8 = 0;
    for (x, y) in a.iter().zip(b) {
        result |= x ^ y;
    }
    result == 0
}

// ---------------------------------------------------------------------------
// Replay guard (issue #16): a captured digest UsernameToken must not be
// replayable — nonces are remembered and Created must be fresh.
// ---------------------------------------------------------------------------

/// Remembers seen nonces (bounded map) and enforces a Created freshness
/// window. `window_secs == 0` disables both checks (tests only).
pub struct ReplayGuard {
    window_secs: u64,
    seen: Mutex<HashMap<String, Instant>>,
}

const MAX_REMEMBERED_NONCES: usize = 1024;

impl ReplayGuard {
    pub fn new(window_secs: u64) -> Self {
        Self {
            window_secs,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Accepts a (nonce, created) pair exactly once: the Created timestamp
    /// must sit inside the freshness window and the nonce must not have
    /// been seen before.
    pub fn check_and_remember(&self, nonce: &str, created: &str) -> bool {
        if self.window_secs == 0 {
            return true;
        }
        if !created_is_fresh(created, self.window_secs) {
            return false;
        }
        let mut seen = match self.seen.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if seen.len() >= MAX_REMEMBERED_NONCES {
            prune_stale(&mut seen);
        }
        // The nonce (not its decoded bytes) is the identity — the wire
        // value is what an attacker replays byte-for-byte.
        seen.insert(nonce.to_string(), Instant::now()).is_none()
    }
}

fn prune_stale(seen: &mut HashMap<String, Instant>) {
    // One full window back covers every accepted token.
    seen.retain(|_, at| at.elapsed() < Duration::from_secs(600));
    if seen.len() >= MAX_REMEMBERED_NONCES {
        // Still full (degenerate all-at-once replay): drop the oldest
        // quarter instead of growing without bound.
        let mut times: Vec<(String, Instant)> = seen.iter().map(|(k, v)| (k.clone(), *v)).collect();
        times.sort_by_key(|(_, at)| *at);
        for (k, _) in times.into_iter().take(MAX_REMEMBERED_NONCES / 4) {
            seen.remove(&k);
        }
    }
}

/// Parses an RFC3339-ish `YYYY-MM-DDTHH:MM:SS[.fff]Z` Created value and
/// checks it lies within ±window of now. Unparseable timestamps are stale
/// (fail closed).
fn created_is_fresh(created: &str, window_secs: u64) -> bool {
    let Some(unix) = parse_created_unix(created) else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (unix - now).abs() <= window_secs as i64
}

/// Minimal civil-time parser for the ONVIF Created format (no chrono dep):
/// Howard Hinnant's days-from-civil over the date part.
pub fn parse_created_unix(created: &str) -> Option<i64> {
    let bytes = created.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |a: usize, b: usize| -> Option<i64> { created.get(a..b)?.parse::<i64>().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy.rem_euclid(400);
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + s)
}

// ---------------------------------------------------------------------------
// HTTP Digest transport auth (issue #54): RFC 7616 subset — MD5,
// qop="auth" only (md5-sess / auth-int / SHA-2 are never offered and are
// rejected). 802.1X (GetDot1XConfiguration & friends) is deliberately
// out of scope for this library: an EAP supplicant is host
// infrastructure, not SOAP device protocol — such actions stay
// unregistered and answer the generic unsupported-action fault.
// ---------------------------------------------------------------------------

/// Outcome of verifying an HTTP Digest `Authorization` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestResult {
    /// Credentials verified; the nonce was consumed at the presented
    /// nonce-count.
    Ok,
    /// The nonce is unknown or expired — credentials were not evaluated;
    /// re-challenge the client with `stale=TRUE` (RFC 7616 §3.3).
    Stale,
    /// Malformed header, wrong credentials, unsupported algorithm/qop,
    /// URI mismatch, or an nc replay — answer with a fresh challenge
    /// (`stale=FALSE`).
    Invalid,
}

/// A server-side `WWW-Authenticate: Digest ...` challenge.
///
/// Serialized shape (byte-stable):
/// `Digest realm="...", nonce="...", qop="auth", algorithm=MD5,
/// stale=FALSE, opaque="..."` — `stale=TRUE` on the re-challenge that
/// follows an unknown/expired nonce.
#[derive(Debug, Clone)]
pub struct HttpDigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub opaque: String,
    pub stale: bool,
}

impl HttpDigestChallenge {
    pub fn new(realm: &str, nonce: &str, opaque: &str, stale: bool) -> Self {
        Self {
            realm: realm.to_string(),
            nonce: nonce.to_string(),
            opaque: opaque.to_string(),
            stale,
        }
    }

    /// Serialize to a `WWW-Authenticate` header value.
    pub fn to_header_value(&self) -> String {
        format!(
            "Digest realm=\"{}\", nonce=\"{}\", qop=\"auth\", algorithm=MD5, stale={}, opaque=\"{}\"",
            self.realm,
            self.nonce,
            if self.stale { "TRUE" } else { "FALSE" },
            self.opaque
        )
    }
}

/// Bounded server-side nonce store for HTTP Digest (issue #54): nonces
/// issued by [`NonceGuard::generate`] expire after the freshness window
/// and track the highest accepted nonce-count per nonce, so a captured
/// `Authorization` header cannot be replayed with the same nc. Bounded
/// to 64 slots with expiry pruning plus oldest-first eviction — a client
/// flooding the server with challenge requests cannot grow the map
/// without bound. `ttl_secs == 0` disables expiry (tests only) while nc
/// replay protection stays active.
pub struct NonceGuard {
    ttl_secs: u64,
    inner: Mutex<HashMap<String, NonceEntry>>,
}

struct NonceEntry {
    issued: Instant,
    last_nc: u32,
}

impl NonceEntry {
    fn fresh() -> Self {
        Self {
            issued: Instant::now(),
            last_nc: 0,
        }
    }
}

const MAX_DIGEST_NONCES: usize = 64;

impl Default for NonceGuard {
    /// Window disabled (`ttl_secs == 0`) — the AuthState default-derive
    /// path; the server constructor always installs a real window.
    fn default() -> Self {
        Self::new(0)
    }
}

impl NonceGuard {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            ttl_secs,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Generate and register a fresh nonce (16 random bytes, hex).
    pub fn generate(&self) -> String {
        use rand::RngCore;

        let mut bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut bytes);
        let nonce = hex::encode(bytes);
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.ttl_secs != 0 {
            inner.retain(|_, e| e.issued.elapsed().as_secs() < self.ttl_secs);
        }
        if inner.len() >= MAX_DIGEST_NONCES {
            // Still full after expiry pruning: evict the oldest-issued
            // quarter (bounded round-robin-style eviction, the
            // ReplayGuard pattern).
            let mut issued: Vec<(String, Instant)> =
                inner.iter().map(|(k, v)| (k.clone(), v.issued)).collect();
            issued.sort_by_key(|(_, at)| *at);
            for (k, _) in issued.into_iter().take(MAX_DIGEST_NONCES / 4) {
                inner.remove(&k);
            }
        }
        inner.insert(nonce.clone(), NonceEntry::fresh());
        nonce
    }

    /// Whether the nonce is known and, when a window is configured, not
    /// expired. With the window disabled (`ttl_secs == 0`, tests only)
    /// any nonce is accepted as fresh.
    fn probe(&self, nonce: &str) -> bool {
        if self.ttl_secs == 0 {
            return true;
        }
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match inner.get(nonce) {
            Some(e) => e.issued.elapsed().as_secs() < self.ttl_secs,
            None => false,
        }
    }

    /// Consume a nonce-count on a *verified* response: the nonce must
    /// still be known and fresh (any known nonce with the window
    /// disabled — entries register lazily), and `nc` must strictly
    /// exceed the last accepted count. Only called after the hash
    /// verified, so a failed password attempt never burns the client's
    /// nc.
    fn consume(&self, nonce: &str, nc: u32) -> bool {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.ttl_secs == 0 {
            // Window disabled (tests only): replay protection stays on.
            let e = inner
                .entry(nonce.to_string())
                .or_insert_with(NonceEntry::fresh);
            if nc > e.last_nc {
                e.last_nc = nc;
                return true;
            }
            return false;
        }
        match inner.get_mut(nonce) {
            Some(e) => {
                if e.issued.elapsed().as_secs() >= self.ttl_secs {
                    false
                } else if nc > e.last_nc {
                    e.last_nc = nc;
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }
}

/// Verify an `Authorization: Digest` header (RFC 7616 subset: MD5,
/// qop="auth" — exactly what [`HttpDigestChallenge`] offers).
///
/// * `uri` is the effective request URI; the header's digest-uri must
///   equal it (RFC 7616 §3.4.3 recommendation).
/// * `realm` is the expected (server-issued) realm.
/// * `nonce_store` supplies nonce freshness + nc replay protection.
///
/// `response = MD5(HA1:nonce:nc:cnonce:qop:HA2)` with
/// `HA1 = MD5(username:realm:password)`, `HA2 = MD5(method:uri)`,
/// compared in constant time (same pattern as [`verify_username_token`]).
/// A verified response consumes the nonce count; a wrong password does
/// not.
pub fn verify_http_digest(
    header_value: &str,
    method: &str,
    uri: &str,
    realm: &str,
    username: &str,
    password: &str,
    nonce_store: &NonceGuard,
) -> DigestResult {
    let Some(p) = parse_digest_header(header_value) else {
        return DigestResult::Invalid;
    };
    if p.username != username || p.realm != realm {
        return DigestResult::Invalid;
    }
    // Only MD5 — the challenge never offers md5-sess/SHA-2 (case-
    // insensitive token per RFC 7616 §3.3).
    if let Some(alg) = &p.algorithm {
        if !alg.eq_ignore_ascii_case("MD5") {
            return DigestResult::Invalid;
        }
    }
    // qop="auth" is the only quality-of-protection offered; auth-int
    // (hashing the entity body) is out of scope by design.
    match p.qop.as_deref() {
        Some(q) if q.eq_ignore_ascii_case("auth") => {}
        _ => return DigestResult::Invalid,
    }
    // The digest-uri must equal the effective request URI (RFC 7616
    // §3.4.3 recommendation — strict here, both are ours to know).
    if p.uri != uri {
        return DigestResult::Invalid;
    }
    let nc_trimmed = p.nc.trim();
    let Ok(nc) = u32::from_str_radix(nc_trimmed, 16) else {
        return DigestResult::Invalid;
    };
    if !nonce_store.probe(&p.nonce) {
        return DigestResult::Stale;
    }

    // response = MD5(HA1:nonce:nc:cnonce:qop:HA2), hashed exactly with
    // the client's field bytes (nc formatting included) — a client
    // sending "1" hashed a different string than one sending
    // "00000001", and we verify what was sent.
    let ha1 = md5_hex(format!("{username}:{realm}:{password}").as_bytes());
    let ha2 = md5_hex(format!("{method}:{}", p.uri).as_bytes());
    let expected = md5_hex(
        format!(
            "{}:{}:{}:{}:auth:{}",
            ha1, p.nonce, nc_trimmed, p.cnonce, ha2
        )
        .as_bytes(),
    );
    let given = p.response.to_ascii_lowercase();
    if !constant_time_eq(given.as_bytes(), expected.as_bytes()) {
        return DigestResult::Invalid;
    }
    // Credentials verified — consume the nc (rejects same-nc replays).
    if !nonce_store.consume(&p.nonce, nc) {
        return DigestResult::Invalid;
    }
    DigestResult::Ok
}

/// Parsed parameters of an `Authorization: Digest` header.
#[derive(Default)]
struct DigestParams {
    username: String,
    realm: String,
    nonce: String,
    uri: String,
    qop: Option<String>,
    nc: String,
    cnonce: String,
    response: String,
    algorithm: Option<String>,
}

/// Parse `Authorization: Digest k=v, ...` (RFC 7616 §3.4). Returns
/// `None` for a non-Digest scheme or a structurally malformed header.
/// Quoted values may contain commas and `\"` escapes; keys are
/// case-insensitive; unknown keys (opaque, userhash, ...) are carried
/// or ignored, never fatal.
fn parse_digest_header(header: &str) -> Option<DigestParams> {
    let header = header.trim();
    let (scheme, rest) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Digest") {
        return None;
    }
    let mut params = DigestParams::default();
    for part in split_digest_params(rest) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = unquote_digest_value(value.trim());
        match key.as_str() {
            "username" => params.username = value,
            "realm" => params.realm = value,
            "nonce" => params.nonce = value,
            "uri" => params.uri = value,
            "qop" => params.qop = Some(value),
            "nc" => params.nc = value,
            "cnonce" => params.cnonce = value,
            "response" => params.response = value,
            "algorithm" => params.algorithm = Some(value),
            _ => {}
        }
    }
    if params.username.is_empty()
        || params.realm.is_empty()
        || params.nonce.is_empty()
        || params.uri.is_empty()
        || params.response.is_empty()
        || params.qop.is_none()
        || params.nc.is_empty()
        || params.cnonce.is_empty()
    {
        return None;
    }
    Some(params)
}

/// Split a Digest parameter list on top-level commas — quoted strings
/// may legally contain commas.
fn split_digest_params(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in s.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' && in_quotes {
            current.push(ch);
            escaped = true;
        } else if ch == '"' {
            in_quotes = !in_quotes;
            current.push(ch);
        } else if ch == ',' && !in_quotes {
            parts.push(current.clone());
            current.clear();
        } else {
            current.push(ch);
        }
    }
    parts.push(current);
    parts
}

/// Strip surrounding double quotes, resolving `\"` escapes inside. The
/// quote bytes are ASCII, so the slice boundaries are char boundaries.
fn unquote_digest_value(v: &str) -> String {
    let inner = if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        &v[1..v.len() - 1]
    } else {
        v
    };
    let mut out = String::with_capacity(inner.len());
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    out
}

/// Lowercase hex MD5 of `data` (the RFC 7616 request-digest primitive).
fn md5_hex(data: &[u8]) -> String {
    use md5::Md5;

    let mut hasher = Md5::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    mod proptests {
        //! Property tests: `parse_created_unix` parses the untrusted
        //! Created field of a UsernameToken — arbitrary strings must
        //! surface as None, never a panic.
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn parse_created_unix_never_panics(input in "\\PC{0,64}") {
                let _ = parse_created_unix(&input);
            }
        }
    }

    use super::*;

    // Test vector for digest computation.
    // Nonce (base64): "dGhpcyBpcyBhIG5vbmNl" → bytes: "this is a nonce"
    const TEST_NONCE: &str = "dGhpcyBpcyBhIG5vbmNl";
    const TEST_CREATED: &str = "2025-01-01T00:00:00Z";
    const TEST_PASSWORD: &str = "test123";

    fn expected_digest() -> String {
        compute_password_digest(TEST_NONCE, TEST_CREATED, TEST_PASSWORD)
    }

    // ------------------------------------------------------------------
    // Plaintext
    // ------------------------------------------------------------------

    #[test]
    fn test_auth_plaintext_valid() {
        let token = UsernameToken {
            username: "admin".into(),
            password: "secret".into(),
            nonce: String::new(),
            created: String::new(),
        };
        assert!(verify_username_token(&token, "admin", "secret"));
    }

    #[test]
    fn test_auth_plaintext_valid_empty_password() {
        let token = UsernameToken {
            username: "admin".into(),
            password: String::new(),
            nonce: String::new(),
            created: String::new(),
        };
        assert!(verify_username_token(&token, "admin", ""));
    }

    #[test]
    fn test_auth_plaintext_invalid() {
        let token = UsernameToken {
            username: "admin".into(),
            password: "wrong".into(),
            nonce: String::new(),
            created: String::new(),
        };
        assert!(!verify_username_token(&token, "admin", "secret"));
    }

    #[test]
    fn test_auth_plaintext_username_mismatch() {
        let token = UsernameToken {
            username: "nobody".into(),
            password: "secret".into(),
            nonce: String::new(),
            created: String::new(),
        };
        assert!(!verify_username_token(&token, "admin", "secret"));
    }

    // ------------------------------------------------------------------
    // Digest
    // ------------------------------------------------------------------

    #[test]
    fn test_auth_digest_valid() {
        let token = UsernameToken {
            username: "admin".into(),
            password: expected_digest(),
            nonce: TEST_NONCE.into(),
            created: TEST_CREATED.into(),
        };
        assert!(verify_username_token(&token, "admin", TEST_PASSWORD));
    }

    #[test]
    fn test_auth_digest_invalid() {
        let token = UsernameToken {
            username: "admin".into(),
            password: "AAAAAAAA".into(),
            nonce: TEST_NONCE.into(),
            created: TEST_CREATED.into(),
        };
        assert!(!verify_username_token(&token, "admin", TEST_PASSWORD));
    }

    #[test]
    fn test_auth_digest_username_mismatch() {
        let token = UsernameToken {
            username: "nobody".into(),
            password: expected_digest(),
            nonce: TEST_NONCE.into(),
            created: TEST_CREATED.into(),
        };
        assert!(!verify_username_token(&token, "admin", TEST_PASSWORD));
    }

    #[test]
    fn test_auth_digest_wrong_password() {
        let token = UsernameToken {
            username: "admin".into(),
            password: expected_digest(),
            nonce: TEST_NONCE.into(),
            created: TEST_CREATED.into(),
        };
        assert!(!verify_username_token(&token, "admin", "wrong_password"));
    }

    // ------------------------------------------------------------------
    // Digest computation
    // ------------------------------------------------------------------

    #[test]
    fn test_compute_digest_deterministic() {
        let a = compute_password_digest(TEST_NONCE, TEST_CREATED, TEST_PASSWORD);
        let b = compute_password_digest(TEST_NONCE, TEST_CREATED, TEST_PASSWORD);
        assert_eq!(a, b);
    }

    #[test]
    fn test_compute_digest_different_inputs() {
        let a = compute_password_digest(TEST_NONCE, TEST_CREATED, TEST_PASSWORD);
        let b = compute_password_digest(TEST_NONCE, TEST_CREATED, "other");
        assert_ne!(a, b);
    }

    #[test]
    fn test_compute_digest_correct_format() {
        let digest = compute_password_digest(TEST_NONCE, TEST_CREATED, TEST_PASSWORD);
        // Digest must be valid base64
        assert!(
            BASE64.decode(&digest).is_ok(),
            "digest must be valid base64"
        );
        // Decoded must be 20 bytes (SHA-1 output)
        assert_eq!(
            BASE64.decode(&digest).unwrap().len(),
            20,
            "SHA-1 digest must be 20 bytes"
        );
    }

    // ------------------------------------------------------------------
    // Constant-time helper
    // ------------------------------------------------------------------

    #[test]
    fn test_constant_time_eq_same() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    #[test]
    fn test_constant_time_eq_diff() {
        assert!(!constant_time_eq(b"hello", b"world"));
    }

    #[test]
    fn test_constant_time_eq_diff_len() {
        assert!(!constant_time_eq(b"hello", b"helloo"));
    }

    #[test]
    fn test_constant_time_eq_empty() {
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"", b"a"));
    }

    // ------------------------------------------------------------------
    // HTTP Digest (issue #54, RFC 7616 subset)
    // ------------------------------------------------------------------

    use md5::Md5;

    /// Client-side digest computation (the mirror of
    /// `verify_http_digest`): builds a full `Authorization` header the
    /// way an RFC 7616 client would.
    #[allow(clippy::too_many_arguments)] // faithful mirror of the RFC field set
    fn client_digest_header(
        username: &str,
        realm: &str,
        password: &str,
        method: &str,
        uri: &str,
        nonce: &str,
        nc: &str,
        cnonce: &str,
    ) -> String {
        fn md5hex(s: &str) -> String {
            use sha1::Digest as _;
            let mut h = Md5::new();
            h.update(s.as_bytes());
            hex::encode(h.finalize())
        }
        let ha1 = md5hex(&format!("{username}:{realm}:{password}"));
        let ha2 = md5hex(&format!("{method}:{uri}"));
        let response = md5hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));
        format!(
            "Digest username=\"{username}\", realm=\"{realm}\", nonce=\"{nonce}\", \
             uri=\"{uri}\", qop=auth, nc={nc}, cnonce=\"{cnonce}\", \
             response=\"{response}\", opaque=\"5ccc069c\", algorithm=MD5"
        )
    }

    const RFC_REALM: &str = "testrealm@host.com";

    /// RFC 2617 §3.5 worked example — the canonical MD5 digest vector
    /// (carried into RFC 7616): golden response hex must verify.
    #[test]
    fn test_digest_rfc2617_example_vector() {
        let guard = NonceGuard::new(0); // tests only: nonce never expires
        let header = "Digest username=\"Mufasa\", realm=\"testrealm@host.com\", \
                      nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", \
                      uri=\"/dir/index.html\", qop=auth, nc=00000001, \
                      cnonce=\"0a4f113b\", \
                      response=\"6629fae49393a05397450978507c4ef1\", \
                      opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"";
        assert_eq!(
            verify_http_digest(
                header,
                "GET",
                "/dir/index.html",
                RFC_REALM,
                "Mufasa",
                "Circle Of Life",
                &guard
            ),
            DigestResult::Ok
        );
    }

    #[test]
    fn test_digest_wrong_password_invalid() {
        let guard = NonceGuard::new(0);
        let header = client_digest_header(
            "admin",
            "onvif",
            "secret",
            "POST",
            "/onvif/device_service",
            "nonce-1",
            "00000001",
            "cnonce-1",
        );
        assert_eq!(
            verify_http_digest(
                &header,
                "POST",
                "/onvif/device_service",
                "onvif",
                "admin",
                "WRONG",
                &guard
            ),
            DigestResult::Invalid
        );
    }

    #[test]
    fn test_digest_wrong_username_invalid() {
        let guard = NonceGuard::new(0);
        let header = client_digest_header(
            "admin",
            "onvif",
            "secret",
            "POST",
            "/onvif/device_service",
            "nonce-1",
            "00000001",
            "cnonce-1",
        );
        assert_eq!(
            verify_http_digest(
                &header,
                "POST",
                "/onvif/device_service",
                "onvif",
                "other",
                "secret",
                &guard
            ),
            DigestResult::Invalid
        );
    }

    #[test]
    fn test_digest_realm_mismatch_invalid() {
        let guard = NonceGuard::new(0);
        let header = client_digest_header(
            "admin",
            "elsewhere",
            "secret",
            "POST",
            "/onvif/device_service",
            "nonce-1",
            "00000001",
            "cnonce-1",
        );
        assert_eq!(
            verify_http_digest(
                &header,
                "POST",
                "/onvif/device_service",
                "onvif",
                "admin",
                "secret",
                &guard
            ),
            DigestResult::Invalid
        );
    }

    #[test]
    fn test_digest_algorithm_md5_sess_rejected() {
        let guard = NonceGuard::new(0);
        let mut header = client_digest_header(
            "admin",
            "onvif",
            "secret",
            "POST",
            "/onvif/device_service",
            "nonce-1",
            "00000001",
            "cnonce-1",
        );
        // Same formula, algorithm=MD5-sess — never offered by this server.
        header = header.replace("algorithm=MD5", "algorithm=MD5-sess");
        assert_eq!(
            verify_http_digest(
                &header,
                "POST",
                "/onvif/device_service",
                "onvif",
                "admin",
                "secret",
                &guard
            ),
            DigestResult::Invalid
        );
    }

    #[test]
    fn test_digest_algorithm_lowercase_md5_accepted() {
        let guard = NonceGuard::new(0);
        let header = client_digest_header(
            "admin",
            "onvif",
            "secret",
            "POST",
            "/onvif/device_service",
            "nonce-1",
            "00000001",
            "cnonce-1",
        )
        .replace("algorithm=MD5", "algorithm=md5");
        assert_eq!(
            verify_http_digest(
                &header,
                "POST",
                "/onvif/device_service",
                "onvif",
                "admin",
                "secret",
                &guard
            ),
            DigestResult::Ok
        );
    }

    #[test]
    fn test_digest_missing_qop_invalid() {
        let guard = NonceGuard::new(0);
        let header = client_digest_header(
            "admin",
            "onvif",
            "secret",
            "POST",
            "/onvif/device_service",
            "nonce-1",
            "00000001",
            "cnonce-1",
        )
        .replace("qop=auth, ", "");
        assert_eq!(
            verify_http_digest(
                &header,
                "POST",
                "/onvif/device_service",
                "onvif",
                "admin",
                "secret",
                &guard
            ),
            DigestResult::Invalid
        );
    }

    #[test]
    fn test_digest_qop_auth_int_invalid() {
        let guard = NonceGuard::new(0);
        // qop=auth-int requires hashing the entity body — never offered.
        let header = client_digest_header(
            "admin",
            "onvif",
            "secret",
            "POST",
            "/onvif/device_service",
            "nonce-1",
            "00000001",
            "cnonce-1",
        )
        .replace("qop=auth,", "qop=\"auth-int\",");
        assert_eq!(
            verify_http_digest(
                &header,
                "POST",
                "/onvif/device_service",
                "onvif",
                "admin",
                "secret",
                &guard
            ),
            DigestResult::Invalid
        );
    }

    #[test]
    fn test_digest_uri_mismatch_invalid() {
        let guard = NonceGuard::new(0);
        let header = client_digest_header(
            "admin",
            "onvif",
            "secret",
            "POST",
            "/onvif/other_service",
            "nonce-1",
            "00000001",
            "cnonce-1",
        );
        assert_eq!(
            verify_http_digest(
                &header,
                "POST",
                "/onvif/device_service",
                "onvif",
                "admin",
                "secret",
                &guard
            ),
            DigestResult::Invalid
        );
    }

    #[test]
    fn test_digest_malformed_headers_invalid() {
        let guard = NonceGuard::new(0);
        for header in [
            "",
            "Basic dXNlcjpwYXNz",
            "Digest",
            "Digest garbage",
            "Digest realm=\"onvif\"",
            "Digest username=\"a\", realm=\"onvif\", nonce=\"n\", uri=\"/x\", nc=zz, cnonce=\"c\", qop=auth, response=\"deadbeef\"",
        ] {
            assert_eq!(
                verify_http_digest(header, "POST", "/x", "onvif", "a", "p", &guard),
                DigestResult::Invalid,
                "header {header:?} must be Invalid"
            );
        }
    }

    #[test]
    fn test_digest_unknown_nonce_stale() {
        let guard = NonceGuard::new(300);
        let header = client_digest_header(
            "admin",
            "onvif",
            "secret",
            "POST",
            "/onvif/device_service",
            "never-issued",
            "00000001",
            "cnonce-1",
        );
        assert_eq!(
            verify_http_digest(
                &header,
                "POST",
                "/onvif/device_service",
                "onvif",
                "admin",
                "secret",
                &guard
            ),
            DigestResult::Stale
        );
    }

    #[test]
    fn test_digest_generated_nonce_roundtrip() {
        let guard = NonceGuard::new(300);
        let nonce = guard.generate();
        let header = client_digest_header(
            "admin",
            "onvif",
            "secret",
            "POST",
            "/onvif/device_service",
            &nonce,
            "00000001",
            "cnonce-1",
        );
        assert_eq!(
            verify_http_digest(
                &header,
                "POST",
                "/onvif/device_service",
                "onvif",
                "admin",
                "secret",
                &guard
            ),
            DigestResult::Ok
        );
    }

    #[test]
    fn test_digest_replay_same_nc_rejected() {
        let guard = NonceGuard::new(0);
        let header = client_digest_header(
            "admin",
            "onvif",
            "secret",
            "POST",
            "/onvif/device_service",
            "nonce-1",
            "00000001",
            "cnonce-1",
        );
        assert_eq!(
            verify_http_digest(
                &header,
                "POST",
                "/onvif/device_service",
                "onvif",
                "admin",
                "secret",
                &guard
            ),
            DigestResult::Ok
        );
        // Byte-identical replay: nc 1 is no longer > last accepted 1.
        assert_eq!(
            verify_http_digest(
                &header,
                "POST",
                "/onvif/device_service",
                "onvif",
                "admin",
                "secret",
                &guard
            ),
            DigestResult::Invalid,
            "captured header must not be replayable"
        );
    }

    #[test]
    fn test_digest_failed_password_does_not_burn_nc() {
        let guard = NonceGuard::new(0);
        let wrong = client_digest_header(
            "admin",
            "onvif",
            "WRONG",
            "POST",
            "/onvif/device_service",
            "nonce-1",
            "00000001",
            "cnonce-1",
        );
        assert_eq!(
            verify_http_digest(
                &wrong,
                "POST",
                "/onvif/device_service",
                "onvif",
                "admin",
                "secret",
                &guard
            ),
            DigestResult::Invalid
        );
        // Client retries with the right password at the SAME nc — must
        // succeed (failed attempts never consumed the count).
        let right = client_digest_header(
            "admin",
            "onvif",
            "secret",
            "POST",
            "/onvif/device_service",
            "nonce-1",
            "00000001",
            "cnonce-1",
        );
        assert_eq!(
            verify_http_digest(
                &right,
                "POST",
                "/onvif/device_service",
                "onvif",
                "admin",
                "secret",
                &guard
            ),
            DigestResult::Ok
        );
    }

    #[test]
    fn test_digest_nc_advance_accepted() {
        let guard = NonceGuard::new(0);
        for nc in ["00000001", "00000002", "00000042"] {
            let header = client_digest_header(
                "admin",
                "onvif",
                "secret",
                "POST",
                "/onvif/device_service",
                "nonce-1",
                nc,
                "cnonce-x",
            );
            assert_eq!(
                verify_http_digest(
                    &header,
                    "POST",
                    "/onvif/device_service",
                    "onvif",
                    "admin",
                    "secret",
                    &guard
                ),
                DigestResult::Ok,
                "nc {nc} must be accepted"
            );
        }
    }

    // ------------------------------------------------------------------
    // Challenge serialization (golden byte shape)
    // ------------------------------------------------------------------

    #[test]
    fn test_challenge_header_shape() {
        let c = HttpDigestChallenge::new("onvif", "abc123", "opaque1", false);
        assert_eq!(
            c.to_header_value(),
            "Digest realm=\"onvif\", nonce=\"abc123\", qop=\"auth\", \
             algorithm=MD5, stale=FALSE, opaque=\"opaque1\""
        );
    }

    #[test]
    fn test_challenge_header_shape_stale() {
        let c = HttpDigestChallenge::new("onvif", "abc123", "opaque1", true);
        assert_eq!(
            c.to_header_value(),
            "Digest realm=\"onvif\", nonce=\"abc123\", qop=\"auth\", \
             algorithm=MD5, stale=TRUE, opaque=\"opaque1\""
        );
    }

    // ------------------------------------------------------------------
    // NonceGuard
    // ------------------------------------------------------------------

    #[test]
    fn test_nonce_generate_fresh_hex_unique() {
        let guard = NonceGuard::new(300);
        let a = guard.generate();
        let b = guard.generate();
        assert_eq!(a.len(), 32, "16 bytes hex-encoded");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn test_nonce_store_bounded_no_panic() {
        let guard = NonceGuard::new(300);
        let mut last = String::new();
        for _ in 0..200 {
            last = guard.generate();
        }
        // The most recent nonce is still usable.
        let header = client_digest_header(
            "admin", "onvif", "secret", "POST", "/x", &last, "00000001", "c",
        );
        assert_eq!(
            verify_http_digest(&header, "POST", "/x", "onvif", "admin", "secret", &guard),
            DigestResult::Ok
        );
    }

    /// A quoted username containing a comma must not break parameter
    /// splitting (RFC 7616 quoted-string).
    #[test]
    fn test_digest_quoted_comma_username_parses() {
        let guard = NonceGuard::new(0);
        let header = client_digest_header(
            "Ad, min",
            "onvif",
            "secret",
            "POST",
            "/onvif/device_service",
            "nonce-1",
            "00000001",
            "cnonce-1",
        );
        assert_eq!(
            verify_http_digest(
                &header,
                "POST",
                "/onvif/device_service",
                "onvif",
                "Ad, min",
                "secret",
                &guard
            ),
            DigestResult::Ok
        );
    }

    /// Property test: the Digest header parser faces untrusted header
    /// bytes — arbitrary input must surface as `Invalid` or `Stale`,
    /// never `Ok`, and never panic.
    mod digest_proptests {
        use super::super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn digest_header_never_panics_or_accepts(input in "\\PC{0,128}") {
                let guard = NonceGuard::new(300);
                let verdict = verify_http_digest(
                    &input, "POST", "/x", "onvif", "admin", "secret", &guard,
                );
                prop_assert_ne!(verdict, DigestResult::Ok);
            }
        }
    }
}
