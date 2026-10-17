//! Signed update manifest: types, minisign verification and install policy.
//!
//! An update package (`.kup`, design §4) carries `manifest.json` and its
//! minisign signature `manifest.json.minisig`. The manifest lists every
//! payload and every file with its size and SHA-256, so once the manifest is
//! trusted, everything else can be checked against it while streaming. This
//! crate is that trust step and nothing more: no I/O, no async, no download.
//! The camera-side `ota` crate and the host-side `xtask kup` both use it.
//!
//! ```text
//!  manifest.json ──┐
//!  .minisig ───────┼─> verify() ── signature (current/next key, OTA-03)
//!  TrustedKeys ────┘       │       schema == 1, then full parse
//!                          │       trusted comment == "kamoder <version> seq=<seq>"
//!                          │       structure (paths, digests, unique names)
//!                          v
//!                      Manifest ──> Policy::check() ── target, board, seq,
//!                                                       floor, min_updater (OTA-04)
//! ```
//!
//! # Why minisign
//!
//! Ed25519 with a tiny, dependency-free verifier (`minisign-verify`), a
//! one-line public key that can be compiled in, and a CLI the release
//! workflow can call. Sigstore/cosign (already used for release assets) needs
//! an online transparency log and a far larger verifier than fits the
//! camera's budget.
//!
//! # Binding the signature to the metadata
//!
//! The minisign *trusted comment* is covered by the global signature. The
//! builder sets it to [`trusted_comment`] (`kamoder 0.2.0 seq=1760700000`)
//! and [`verify`] requires it to match the parsed manifest exactly, so the
//! version and seq a log or UI shows from the signature line are the ones
//! the manifest carries.
//!
//! # Key handling
//!
//! See [`keys`]: the project key is compiled in from `KAMODER_UPDATE_PUBKEY`
//! or `keys/kamoder.pub` (placeholder until the maintainer generates it; with
//! no key every manifest is rejected with [`Rejection::NoTrustedKey`]). The
//! committed TEST-ONLY key is reachable only from tests.

pub mod keys;
pub mod manifest;
pub mod policy;

pub use keys::{KeyError, TrustedKeys};
pub use manifest::{
    compare_versions, validate_rel_path, Channel, Component, ComponentKind, Digest, DigestError,
    FileEntry, Invalid, Manifest, PathError, SCHEMA,
};
pub use policy::Policy;
pub use semver::Version;

use keys::Check;
use minisign_verify::Signature;
use serde::Deserialize;
use snafu::Snafu;

/// Largest accepted `manifest.json`, bytes. A slot tree lists a few hundred
/// files (~150 bytes each); the cap keeps hostile input from eating the
/// camera's RAM before the signature is even checked.
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

/// Prefix of the trusted comment, before `<version> seq=<seq>`.
pub const COMMENT_PREFIX: &str = "kamoder";

/// Why a manifest was refused. Every variant is a clear, user-facing reason
/// (logged by the updater, shown by the web UI with [`Rejection::code`]).
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Rejection {
    /// `manifest.json` is larger than [`MAX_MANIFEST_BYTES`].
    #[snafu(display("manifest is {size} bytes, more than the {max} allowed"))]
    TooLarge { size: usize, max: usize },
    /// The `.minisig` text does not parse.
    #[snafu(display("signature is not a minisign signature: {source}"))]
    SignatureFormat { source: minisign_verify::Error },
    /// This build has no update key (placeholders in `keys/`).
    #[snafu(display("no trusted update key built in"))]
    NoTrustedKey,
    /// Signed by a key this build does not trust.
    #[snafu(display("signed by an unknown key"))]
    UnknownKey,
    /// The signature does not match the manifest bytes or trusted comment.
    #[snafu(display("bad signature: {source}"))]
    BadSignature { source: minisign_verify::Error },
    /// A manifest schema this updater does not read.
    #[snafu(display("manifest schema {found} is not supported (expected {SCHEMA})"))]
    Schema { found: u32 },
    /// Not valid manifest JSON (after the signature check).
    #[snafu(display("malformed manifest: {source}"))]
    Malformed { source: serde_json::Error },
    /// The trusted comment does not name this manifest's version and seq.
    #[snafu(display("signature comment {found:?} does not match manifest ({expected:?})"))]
    CommentMismatch { expected: String, found: String },
    /// Structurally invalid (paths, duplicates, files on the wrong kind).
    #[snafu(display("invalid manifest: {source}"))]
    Invalid { source: Invalid },
    /// Built for another target triple.
    #[snafu(display("package is for target {found}, this device runs {expected}"))]
    Target { expected: String, found: String },
    /// This board is not in the manifest's list.
    #[snafu(display("package does not support board {board:?} (supports {supported:?})"))]
    Board {
        board: String,
        supported: Vec<String>,
    },
    /// `seq` not newer than the highest accepted (replay / rollback attack).
    #[snafu(display(
        "package seq {seq} is not newer than {highest} (already installed or older)"
    ))]
    Replay { seq: u64, highest: u64 },
    /// Version below the security floor.
    #[snafu(display("version {version} is below the security floor {floor}"))]
    BelowFloor { version: Version, floor: Version },
    /// This updater is older than the package requires.
    #[snafu(display("package needs updater {required} or newer, this is {have}"))]
    UpdaterTooOld { required: Version, have: Version },
    /// No `sd-tree` (slot) component to install.
    #[snafu(display("package has no slot (sd-tree) component"))]
    NoComponent,
}

impl Rejection {
    /// Stable snake_case code for APIs and metrics labels.
    pub fn code(&self) -> &'static str {
        match self {
            Rejection::TooLarge { .. } => "too_large",
            Rejection::SignatureFormat { .. } => "signature_format",
            Rejection::NoTrustedKey => "no_trusted_key",
            Rejection::UnknownKey => "unknown_key",
            Rejection::BadSignature { .. } => "bad_signature",
            Rejection::Schema { .. } => "schema",
            Rejection::Malformed { .. } => "malformed",
            Rejection::CommentMismatch { .. } => "comment_mismatch",
            Rejection::Invalid { .. } => "invalid",
            Rejection::Target { .. } => "target",
            Rejection::Board { .. } => "board",
            Rejection::Replay { .. } => "replay",
            Rejection::BelowFloor { .. } => "below_floor",
            Rejection::UpdaterTooOld { .. } => "updater_too_old",
            Rejection::NoComponent => "no_component",
        }
    }
}

/// The trusted comment the builder signs with and [`verify`] requires:
/// `kamoder <version> seq=<seq>`.
pub fn trusted_comment(m: &Manifest) -> String {
    format!("{COMMENT_PREFIX} {} seq={}", m.version, m.seq)
}

/// Verifies `sig_text` (a `.minisig` file) over `manifest_bytes` against
/// `keys`, then parses and validates the manifest. No field is looked at
/// before the signature checks out (OTA-03).
pub fn verify(
    manifest_bytes: &[u8],
    sig_text: &str,
    keys: &TrustedKeys,
) -> Result<Manifest, Rejection> {
    if manifest_bytes.len() > MAX_MANIFEST_BYTES {
        return Err(Rejection::TooLarge {
            size: manifest_bytes.len(),
            max: MAX_MANIFEST_BYTES,
        });
    }
    let sig =
        Signature::decode(sig_text).map_err(|source| Rejection::SignatureFormat { source })?;
    match keys.check(manifest_bytes, &sig) {
        Check::Good => {}
        Check::NoKeys => return Err(Rejection::NoTrustedKey),
        Check::UnknownKey => return Err(Rejection::UnknownKey),
        Check::Bad(source) => return Err(Rejection::BadSignature { source }),
    }

    // Schema first, so a future layout is reported as such rather than as
    // whatever field the old parser trips over.
    #[derive(Deserialize)]
    struct Probe {
        schema: u32,
    }
    let probe: Probe =
        serde_json::from_slice(manifest_bytes).map_err(|source| Rejection::Malformed { source })?;
    if probe.schema != SCHEMA {
        return Err(Rejection::Schema {
            found: probe.schema,
        });
    }
    let manifest: Manifest =
        serde_json::from_slice(manifest_bytes).map_err(|source| Rejection::Malformed { source })?;

    let expected = trusted_comment(&manifest);
    if sig.trusted_comment() != expected {
        return Err(Rejection::CommentMismatch {
            expected,
            found: sig.trusted_comment().to_owned(),
        });
    }
    manifest
        .validate()
        .map_err(|source| Rejection::Invalid { source })?;
    Ok(manifest)
}

/// [`verify`] then [`Policy::check`].
pub fn verify_and_check(
    manifest_bytes: &[u8],
    sig_text: &str,
    keys: &TrustedKeys,
    policy: &Policy,
) -> Result<Manifest, Rejection> {
    let manifest = verify(manifest_bytes, sig_text, keys)?;
    policy.check(&manifest)?;
    Ok(manifest)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn digest(byte: u8) -> Digest {
        Digest([byte; 32])
    }

    /// Valid for the policy in `policy::tests` (board "test-board",
    /// highest_seq 100).
    pub(crate) fn sample() -> Manifest {
        let v = |s: &str| Version::parse(s).unwrap();
        Manifest {
            schema: SCHEMA,
            version: v("0.2.0"),
            channel: Channel::Stable,
            seq: 1_760_700_000,
            security_floor: v("0.1.0"),
            min_updater: v("0.1.0"),
            boards: vec!["test-board".into()],
            target: "armv5te-unknown-linux-uclibceabi".into(),
            created: "2026-10-17T12:00:00Z".into(),
            notes_url: None,
            components: vec![
                Component {
                    name: "slot".into(),
                    kind: ComponentKind::SdTree,
                    path: "payload.tar.gz".into(),
                    size: 10,
                    sha256: digest(1),
                    files: vec![
                        FileEntry {
                            path: "www/index.html".into(),
                            size: 3,
                            sha256: digest(2),
                            mode: 0o644,
                        },
                        FileEntry {
                            path: "bin/web".into(),
                            size: 7,
                            sha256: digest(3),
                            mode: 0o755,
                        },
                    ],
                },
                Component {
                    name: "kernel-a".into(),
                    kind: ComponentKind::MtdImage {
                        partition: "KERNEL_A".into(),
                    },
                    path: "kernel.bin".into(),
                    size: 4,
                    sha256: digest(4),
                    files: vec![],
                },
            ],
        }
    }
}

/// Signature tests over fixtures signed with the committed TEST-ONLY keys.
/// Regenerate with `tests/data/sign-fixtures.sh` after changing a fixture.
#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &[u8] = include_bytes!("../tests/data/manifest.json");
    const SIG: &str = include_str!("../tests/data/manifest.json.minisig");
    /// Same bytes, signed by the unrelated `keys/test/other` key.
    const SIG_OTHER_KEY: &str = include_str!("../tests/data/manifest.json.other.minisig");
    /// Same bytes, test key, trusted comment naming another seq.
    const SIG_WRONG_COMMENT: &str = include_str!("../tests/data/manifest.json.badcomment.minisig");
    /// `schema: 2`, signed correctly with the test key.
    const SCHEMA2: &[u8] = include_bytes!("../tests/data/schema2.json");
    const SCHEMA2_SIG: &str = include_str!("../tests/data/schema2.json.minisig");
    /// A traversal path, signed correctly with the test key.
    const TRAVERSAL: &[u8] = include_bytes!("../tests/data/traversal.json");
    const TRAVERSAL_SIG: &str = include_str!("../tests/data/traversal.json.minisig");
    const OTHER_PUB: &str = include_str!("../keys/test/other.pub");

    fn keys() -> TrustedKeys {
        TrustedKeys::test_key()
    }

    #[test]
    fn good_fixture_verifies() {
        let m = verify(MANIFEST, SIG, &keys()).unwrap();
        assert_eq!(m.version, Version::parse("0.2.0").unwrap());
        assert_eq!(m.channel, Channel::Stable);
        assert_eq!(trusted_comment(&m), "kamoder 0.2.0 seq=1760700000");
        assert!(m.component(&ComponentKind::SdTree).is_some());
        assert!(m.component(&ComponentKind::BootShim).is_some());
    }

    #[test]
    fn fixture_is_canonical() {
        // The builder signs `to_json()`; the fixture must be exactly that.
        let m: Manifest = serde_json::from_slice(MANIFEST).unwrap();
        assert_eq!(
            String::from_utf8(m.to_json()).unwrap(),
            std::str::from_utf8(MANIFEST).unwrap()
        );
    }

    #[test]
    fn verifies_with_either_rotation_slot() {
        let keys =
            TrustedKeys::from_texts([("current", OTHER_PUB), ("next", keys::TEST_PUBLIC_KEY)])
                .unwrap();
        verify(MANIFEST, SIG, &keys).unwrap();
        verify(MANIFEST, SIG_OTHER_KEY, &keys).unwrap();
    }

    #[test]
    fn every_tampered_byte_fails() {
        let mut data = MANIFEST.to_vec();
        for i in (0..data.len()).step_by(7) {
            data[i] ^= 0x01;
            let err = verify(&data, SIG, &keys()).unwrap_err();
            assert!(
                matches!(err, Rejection::BadSignature { .. }),
                "byte {i}: {err}"
            );
            data[i] ^= 0x01;
        }
        let mut longer = MANIFEST.to_vec();
        longer.push(b' ');
        assert!(matches!(
            verify(&longer, SIG, &keys()),
            Err(Rejection::BadSignature { .. })
        ));
    }

    #[test]
    fn tampered_trusted_comment_fails() {
        let forged = SIG.replace("seq=1760700000", "seq=1760700001");
        assert_ne!(forged, SIG);
        assert!(matches!(
            verify(MANIFEST, &forged, &keys()),
            Err(Rejection::BadSignature { .. })
        ));
    }

    #[test]
    fn wrong_key_fails() {
        let err = verify(MANIFEST, SIG_OTHER_KEY, &keys()).unwrap_err();
        assert!(matches!(err, Rejection::UnknownKey), "{err}");
        assert_eq!(err.code(), "unknown_key");
    }

    #[test]
    fn no_keys_fails() {
        let err = verify(MANIFEST, SIG, &TrustedKeys::empty()).unwrap_err();
        assert!(matches!(err, Rejection::NoTrustedKey));
        assert_eq!(err.to_string(), "no trusted update key built in");
    }

    #[test]
    fn production_keys_do_not_trust_test_signature() {
        let builtin = TrustedKeys::builtin().unwrap();
        let err = verify(MANIFEST, SIG, &builtin).unwrap_err();
        assert!(
            matches!(err, Rejection::NoTrustedKey | Rejection::UnknownKey),
            "{err}"
        );
    }

    #[test]
    fn comment_mismatch_fails() {
        let err = verify(MANIFEST, SIG_WRONG_COMMENT, &keys()).unwrap_err();
        match err {
            Rejection::CommentMismatch { expected, found } => {
                assert_eq!(expected, "kamoder 0.2.0 seq=1760700000");
                assert_eq!(found, "kamoder 0.2.0 seq=1");
            }
            other => panic!("{other}"),
        }
    }

    #[test]
    fn garbage_signature_fails() {
        for sig in ["", "untrusted comment: x\nnot base64\n", "hello"] {
            assert!(matches!(
                verify(MANIFEST, sig, &keys()),
                Err(Rejection::SignatureFormat { .. })
            ));
        }
    }

    #[test]
    fn schema_bump_is_reported_as_schema() {
        let err = verify(SCHEMA2, SCHEMA2_SIG, &keys()).unwrap_err();
        assert!(matches!(err, Rejection::Schema { found: 2 }), "{err}");
    }

    #[test]
    fn signed_traversal_is_still_invalid() {
        let err = verify(TRAVERSAL, TRAVERSAL_SIG, &keys()).unwrap_err();
        match &err {
            Rejection::Invalid { source } => {
                assert_eq!(source.path.as_ref().unwrap().1, PathError::DotDot)
            }
            other => panic!("{other}"),
        }
    }

    #[test]
    fn too_large_is_refused_before_anything() {
        let big = vec![b' '; MAX_MANIFEST_BYTES + 1];
        assert!(matches!(
            verify(&big, "", &keys()),
            Err(Rejection::TooLarge { .. })
        ));
    }

    #[test]
    fn verify_and_check_applies_policy() {
        let p = Policy {
            board: "test-board".into(),
            target: "armv5te-unknown-linux-uclibceabi".into(),
            current_version: Version::new(0, 1, 0),
            updater_version: Version::new(0, 1, 0),
            highest_seq: 0,
            security_floor: Version::new(0, 0, 0),
            force: false,
        };
        verify_and_check(MANIFEST, SIG, &keys(), &p).unwrap();
        let p = Policy {
            highest_seq: 1_760_700_000,
            ..p
        };
        assert!(matches!(
            verify_and_check(MANIFEST, SIG, &keys(), &p),
            Err(Rejection::Replay { .. })
        ));
    }
}
