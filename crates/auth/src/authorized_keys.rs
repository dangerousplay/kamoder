//! `authorized_keys` for `sshd`: OpenSSH public-key lines, one per key.
//!
//! ```text
//! ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI… alice@laptop
//! ```
//!
//! Only the plain `type base64 [comment]` form is accepted. OpenSSH's
//! per-key options (`from=`, `command=`, …) would each need enforcing, so a
//! line that has them is refused rather than silently granted more than it
//! says. A key is matched by its wire-format blob, which is what the client
//! proves possession of; [`Key::fingerprint`] is the `SHA256:` form
//! `ssh-keygen -l` prints, used to name keys in the web UI.
//!
//! The web admin writes this file and `sshd` reads it on every login
//! attempt, so a key added in the UI works without restarting anything.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use snafu::{ensure, OptionExt, Snafu};
use std::path::Path;

/// Key types `sshd` (russh with ring) can verify.
pub const ALGORITHMS: &[&str] = &[
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "rsa-sha2-256",
    "rsa-sha2-512",
    "ssh-rsa",
];

/// Longest line accepted (a 16384-bit RSA key is about 2.8 KiB of base64).
const MAX_LINE: usize = 8 * 1024;
/// Keys kept; more is almost certainly a mistake.
pub const MAX_KEYS: usize = 32;

#[derive(Debug, Snafu, PartialEq, Eq)]
pub enum Error {
    #[snafu(display("empty key"))]
    Empty,
    #[snafu(display("key line is too long"))]
    TooLong,
    #[snafu(display(
        "unsupported key type {algorithm:?} (use ssh-ed25519, ecdsa-sha2-* or ssh-rsa)"
    ))]
    Algorithm { algorithm: String },
    #[snafu(display("key options such as from= or command= are not supported"))]
    Options,
    #[snafu(display("missing the base64 key after {algorithm:?}"))]
    MissingBlob { algorithm: String },
    #[snafu(display("the key is not valid base64"))]
    Base64,
    #[snafu(display("the key data says {inner:?}, but the line says {outer:?}"))]
    Mismatch { outer: String, inner: String },
    #[snafu(display("comment must be printable text without line breaks"))]
    Comment,
}

/// One public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub algorithm: String,
    /// The SSH wire encoding (the base64-decoded middle field).
    pub blob: Vec<u8>,
    pub comment: String,
}

/// First SSH `string` of a blob: its key type.
fn blob_algorithm(blob: &[u8]) -> Option<&str> {
    let len = u32::from_be_bytes(blob.get(..4)?.try_into().ok()?) as usize;
    std::str::from_utf8(blob.get(4..4 + len)?).ok()
}

impl Key {
    /// Parses one `type base64 [comment]` line.
    pub fn parse(line: &str) -> Result<Key, Error> {
        let line = line.trim();
        ensure!(!line.is_empty(), EmptySnafu);
        ensure!(line.len() <= MAX_LINE, TooLongSnafu);
        let mut fields = line.splitn(3, char::is_whitespace);
        let algorithm = fields.next().unwrap_or_default().to_owned();
        if !ALGORITHMS.contains(&algorithm.as_str()) {
            // `from="…",no-pty ssh-ed25519 …`: options first.
            ensure!(
                !ALGORITHMS.iter().any(|a| line.contains(&format!(" {a} "))),
                OptionsSnafu
            );
            return AlgorithmSnafu { algorithm }.fail();
        }
        let encoded = fields
            .next()
            .filter(|b| !b.is_empty())
            .context(MissingBlobSnafu {
                algorithm: &algorithm,
            })?;
        let blob = STANDARD.decode(encoded).map_err(|_| Error::Base64)?;
        let inner = blob_algorithm(&blob).context(Base64Snafu)?;
        // An RSA key's blob always says ssh-rsa, whatever signature type the
        // line names.
        let outer_family = if algorithm.starts_with("rsa-sha2-") {
            "ssh-rsa"
        } else {
            &algorithm
        };
        ensure!(
            inner == outer_family,
            MismatchSnafu {
                outer: &algorithm,
                inner
            }
        );
        let comment = fields.next().unwrap_or_default().trim().to_owned();
        ensure!(!comment.chars().any(char::is_control), CommentSnafu);
        Ok(Key {
            algorithm,
            blob,
            comment,
        })
    }

    /// `SHA256:…`, as `ssh-keygen -lf` prints it.
    pub fn fingerprint(&self) -> String {
        let digest = ring::digest::digest(&ring::digest::SHA256, &self.blob);
        format!("SHA256:{}", STANDARD_NO_PAD.encode(digest.as_ref()))
    }

    /// The `authorized_keys` line.
    pub fn line(&self) -> String {
        let mut line = format!("{} {}", self.algorithm, STANDARD.encode(&self.blob));
        if !self.comment.is_empty() {
            line.push(' ');
            line.push_str(&self.comment);
        }
        line
    }
}

/// Every valid key in `text`; blank lines and `#` comments are skipped,
/// invalid lines are skipped too (the caller decides whether to complain).
pub fn parse(text: &str) -> Vec<Key> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| Key::parse(l).ok())
        .take(MAX_KEYS)
        .collect()
}

/// Keys in the file at `path`; a missing or unreadable file has none.
pub fn load(path: &Path) -> Vec<Key> {
    std::fs::read_to_string(path)
        .map(|t| parse(&t))
        .unwrap_or_default()
}

/// File contents for `keys`.
pub fn render(keys: &[Key]) -> String {
    let mut out =
        String::from("# Managed by the Kamoder web admin; one OpenSSH public key per line.\n");
    for k in keys {
        out.push_str(&k.line());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // A throwaway key from `ssh-keygen -t ed25519`; `ssh-keygen -lf` prints
    // the fingerprint asserted below.
    const ED: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAFQSXCNBe1s8MCZkJsvOne+u5PdkPmIhGxUII9Rwk9S test@host";

    #[test]
    fn parses_and_fingerprints() {
        let k = Key::parse(ED).unwrap();
        assert_eq!(k.algorithm, "ssh-ed25519");
        assert_eq!(k.comment, "test@host");
        assert_eq!(k.blob.len(), 51);
        assert_eq!(
            k.fingerprint(),
            "SHA256:bohEl0PS8JNGiMPm7Sg0jWsqafu3AQGGhI9+adPWk8U"
        );
        assert_eq!(k.line(), ED);
    }

    #[test]
    fn rejects_bad_lines() {
        assert_eq!(Key::parse("  "), Err(Error::Empty));
        assert_eq!(
            Key::parse("ssh-dss AAAA x"),
            Err(Error::Algorithm {
                algorithm: "ssh-dss".into()
            })
        );
        assert_eq!(Key::parse(&format!("no-pty {ED}")), Err(Error::Options));
        assert_eq!(Key::parse("ssh-ed25519 !!!"), Err(Error::Base64));
        assert_eq!(
            Key::parse("ssh-ed25519"),
            Err(Error::MissingBlob {
                algorithm: "ssh-ed25519".into()
            })
        );
        let rsa_labelled = ED.replace("ssh-ed25519 ", "ssh-rsa ");
        assert!(matches!(
            Key::parse(&rsa_labelled),
            Err(Error::Mismatch { .. })
        ));
    }

    #[test]
    fn file_round_trip() {
        let text = format!("# comment\n\n{ED}\ngarbage line\n");
        let keys = parse(&text);
        assert_eq!(keys.len(), 1);
        assert_eq!(parse(&render(&keys)), keys);
    }
}
