//! Public keys the updater trusts, compiled into the binary (OTA-03).
//!
//! Two slots, so the signing key can be rotated without bricking updates:
//!
//! | slot      | compile-time env              | fallback file            |
//! |-----------|-------------------------------|--------------------------|
//! | `current` | `KAMODER_UPDATE_PUBKEY`       | `keys/kamoder.pub`       |
//! | `next`    | `KAMODER_UPDATE_PUBKEY_NEXT`  | `keys/kamoder-next.pub`  |
//!
//! The env variable (read with `option_env!`, so cargo rebuilds when it
//! changes) wins over the file. Either holds a minisign public key: the
//! `RW...` base64 line alone, or the whole `.pub` file. Lines starting with
//! `#` are ignored, and the committed files are `#`-only **placeholders**
//! because the project key does not exist yet. With no key built in, every
//! manifest is rejected with "no trusted update key built in": an updater
//! that trusts nothing is safe, one that trusts a key nobody holds is not
//! worse, and one that silently skipped verification would be.
//!
//! Rotation: put the new key in `next`, ship a release trusting both, start
//! signing with the new key, then move it to `current` and clear `next`.
//!
//! The TEST-ONLY key under `keys/test/` (its secret key is committed, so it
//! protects nothing) is available as [`TrustedKeys::test_key`] in this
//! crate's tests and, through the `test-key` feature, in other crates' tests.
//! [`TrustedKeys::builtin`] never includes it.
//!
//! # Maintainer steps (once)
//!
//! ```text
//! minisign -G -p kamoder.pub -s kamoder.key          # choose a password
//! cp kamoder.pub crates/ota-manifest/keys/kamoder.pub  # commit it
//! # GitHub repo -> Settings -> Secrets -> Actions:
//! #   MINISIGN_SECRET_KEY = contents of kamoder.key
//! #   MINISIGN_PASSWORD   = its password (used by the release workflow, T8)
//! # keep kamoder.key offline; never commit it
//! ```

use minisign_verify::{PublicKey, Signature};
use snafu::Snafu;

/// Compiled-in key texts, `(slot, text)`.
const BUILTIN: [(&str, &str); 2] = [
    (
        "current",
        match option_env!("KAMODER_UPDATE_PUBKEY") {
            Some(key) => key,
            None => include_str!("../keys/kamoder.pub"),
        },
    ),
    (
        "next",
        match option_env!("KAMODER_UPDATE_PUBKEY_NEXT") {
            Some(key) => key,
            None => include_str!("../keys/kamoder-next.pub"),
        },
    ),
];

/// The committed TEST-ONLY public key (secret: `keys/test/test.key`).
#[cfg(any(test, feature = "test-key"))]
pub const TEST_PUBLIC_KEY: &str = include_str!("../keys/test/test.pub");

/// A compiled-in or supplied key text that is not a minisign public key.
#[derive(Debug, Snafu)]
#[snafu(display("update key {label:?} is not a minisign public key: {source}"))]
pub struct KeyError {
    /// Which key ("current", "next", or the caller's label).
    pub label: String,
    /// What the parser said.
    pub source: minisign_verify::Error,
}

/// The set of public keys a manifest signature may come from.
#[derive(Clone)]
pub struct TrustedKeys {
    keys: Vec<(String, PublicKey)>,
}

/// How a signature relates to a key set.
pub(crate) enum Check {
    /// Verified by one of the keys.
    Good,
    /// The set is empty.
    NoKeys,
    /// No key in the set has the signature's key id.
    UnknownKey,
    /// A key with the right id rejected the signature (tampered data or
    /// comment, or a legacy non-prehashed signature).
    Bad(minisign_verify::Error),
}

impl TrustedKeys {
    /// The keys compiled into this build (see the module docs). Empty while
    /// the placeholders are in place; an error only if a provided key text
    /// is malformed (a unit test makes that a build failure in `./x test`).
    pub fn builtin() -> Result<Self, KeyError> {
        let mut keys = TrustedKeys::empty();
        for (label, text) in BUILTIN {
            keys.add_text(label, text)?;
        }
        Ok(keys)
    }

    /// The committed TEST-ONLY key. Never use outside tests.
    #[cfg(any(test, feature = "test-key"))]
    pub fn test_key() -> Self {
        let mut keys = TrustedKeys::empty();
        keys.add_text("test", TEST_PUBLIC_KEY)
            .expect("committed test key parses");
        keys
    }

    /// A set trusting nothing.
    pub fn empty() -> Self {
        TrustedKeys { keys: Vec::new() }
    }

    /// Adds the key in `text` (a `.pub` file or the bare `RW...` line);
    /// `#` lines are ignored, and a text with no key line adds nothing.
    pub fn add_text(&mut self, label: &str, text: &str) -> Result<&mut Self, KeyError> {
        if let Some(line) = key_line(text) {
            let key = PublicKey::from_base64(line).map_err(|source| KeyError {
                label: label.to_owned(),
                source,
            })?;
            self.keys.push((label.to_owned(), key));
        }
        Ok(self)
    }

    /// Builds a set from `(label, text)` pairs (see [`TrustedKeys::add_text`]).
    pub fn from_texts<'a>(
        texts: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, KeyError> {
        let mut keys = TrustedKeys::empty();
        for (label, text) in texts {
            keys.add_text(label, text)?;
        }
        Ok(keys)
    }

    /// Whether no key is trusted.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Labels of the trusted keys, in order ("current", "next").
    pub fn labels(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(|(label, _)| label.as_str())
    }

    pub(crate) fn check(&self, data: &[u8], sig: &Signature) -> Check {
        if self.keys.is_empty() {
            return Check::NoKeys;
        }
        for (_, key) in &self.keys {
            match key.verify(data, sig, false) {
                Ok(()) => return Check::Good,
                Err(minisign_verify::Error::UnexpectedKeyId) => continue,
                Err(e) => return Check::Bad(e),
            }
        }
        Check::UnknownKey
    }
}

impl std::fmt::Debug for TrustedKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.labels()).finish()
    }
}

/// The base64 key line: the last line that is neither blank, a `#` comment,
/// nor minisign's `untrusted comment:` header.
fn key_line(text: &str) -> Option<&str> {
    text.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("untrusted comment:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_keys_parse() {
        // Fails `./x test` if someone commits a malformed kamoder.pub or
        // builds with a malformed KAMODER_UPDATE_PUBKEY.
        TrustedKeys::builtin().unwrap();
    }

    #[test]
    fn builtin_never_contains_test_key() {
        let builtin = TrustedKeys::builtin().unwrap();
        let test_line = key_line(TEST_PUBLIC_KEY).unwrap();
        for (_, text) in BUILTIN {
            assert_ne!(key_line(text), Some(test_line));
        }
        assert!(builtin.labels().all(|l| l == "current" || l == "next"));
    }

    #[test]
    fn placeholder_text_adds_nothing() {
        let keys = TrustedKeys::from_texts([("x", "# PLACEHOLDER\n#\n\n")]).unwrap();
        assert!(keys.is_empty());
    }

    #[test]
    fn bare_line_and_file_forms() {
        let line = key_line(TEST_PUBLIC_KEY).unwrap();
        let keys = TrustedKeys::from_texts([("file", TEST_PUBLIC_KEY), ("line", line)]).unwrap();
        assert_eq!(keys.labels().collect::<Vec<_>>(), ["file", "line"]);
    }

    #[test]
    fn malformed_key_is_an_error() {
        let err = TrustedKeys::from_texts([("current", "RWnotbase64!!")]).unwrap_err();
        assert_eq!(err.label, "current");
    }
}
