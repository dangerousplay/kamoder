//! `manifest.json` types, structural validation and deterministic output.
//!
//! The manifest is the only file of a `.kup` the device trusts directly (via
//! its minisign signature); everything else is trusted through the sizes and
//! SHA-256 digests it lists (OTA-02). The shape follows design §4:
//!
//! ```text
//! {
//!   "schema": 1,
//!   "version": "0.2.0",
//!   "channel": "stable",
//!   "seq": 1760700000,
//!   "security_floor": "0.1.0",
//!   "min_updater": "0.1.0",
//!   "boards": ["..."],
//!   "target": "armv5te-unknown-linux-uclibceabi",
//!   "created": "2026-10-17T12:00:00Z",
//!   "components": [
//!     {"name": "slot", "kind": "sd-tree", "path": "payload.tar.gz", "size": 123,
//!      "sha256": "<64 lowercase hex>",
//!      "files": [{"path": "bin/web", "size": 45, "sha256": "...", "mode": 493}]},
//!     {"name": "kernel-a", "kind": "mtd-image", "partition": "KERNEL_A", ...}
//!   ]
//! }
//! ```
//!
//! # Compatibility rules
//!
//! - **Unknown fields are ignored** at the top level and in components, so a
//!   newer builder may add informational fields (a changelog digest, a build
//!   id) without locking out older updaters. Anything an older updater must
//!   not misread is a breaking change and bumps [`SCHEMA`]; the verifier
//!   reads `schema` first and refuses other values before parsing the rest.
//! - **File entries deny unknown fields**: a file entry describes what lands
//!   on disk, and a new attribute there (an owner, a link target) would change
//!   install semantics silently. That needs a schema bump.
//! - **Unknown component kinds fail to parse**: the updater cannot install
//!   what it does not understand.
//!
//! # Determinism
//!
//! The builder (`xtask kup`, host) signs the exact bytes of
//! [`Manifest::to_json`]: struct field order is fixed, `notes_url` is
//! omitted when absent, and `files` are always emitted sorted by path, so the
//! same inputs give the same signed bytes.

use semver::Version;
use serde::{Deserialize, Serialize, Serializer};
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;

/// The manifest schema this crate reads and writes. Bumped only for changes
/// an older updater would misinterpret (see the module docs).
pub const SCHEMA: u32 = 1;

/// Longest accepted file path in a component, bytes. Far above anything in a
/// slot tree; bounds memory and log lines for hostile input.
pub const MAX_PATH_LEN: usize = 1024;

/// Highest accepted file mode: permission bits only, no setuid/setgid/sticky,
/// no file-type bits (the SD card is vfat; modes only matter if a slot ever
/// lands on a POSIX filesystem).
pub const MAX_MODE: u32 = 0o777;

/// A signed update manifest (design §4, OTA-02).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Always [`SCHEMA`].
    pub schema: u32,
    /// Release version, semver ("0.2.0", "0.2.0-rc.1").
    pub version: Version,
    /// Release channel the manifest was published on.
    pub channel: Channel,
    /// Monotonic sequence number (the tag commit's unix time). The device
    /// refuses anything not newer than the highest it has seen (anti-replay).
    pub seq: u64,
    /// Lowest version that may still be installed once this manifest has been
    /// seen; raised by the maintainer after a security fix.
    pub security_floor: Version,
    /// Lowest updater version that understands this package.
    pub min_updater: Version,
    /// Board profile names (`boards/*.toml` `name`) the package supports.
    pub boards: Vec<String>,
    /// Rust target triple the binaries were built for.
    pub target: String,
    /// Build time, RFC 3339 UTC ("2026-10-17T12:00:00Z"). Informational.
    pub created: String,
    /// Release notes link shown in the web UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<String>,
    /// Installable parts of the package.
    pub components: Vec<Component>,
}

/// Release channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Tagged releases (`vX.Y.Z`), GitHub "latest".
    Stable,
    /// Pre-releases (`release/**` branches, `-rc.N`).
    Beta,
}

/// One installable part of a package.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    /// Unique name within the manifest ("slot", "boot", "kernel-a").
    pub name: String,
    /// What it is and where it goes; serialized inline as `"kind": ...`.
    #[serde(flatten)]
    pub kind: ComponentKind,
    /// Member of the `.kup` tar holding the payload ("payload.tar.gz").
    pub path: String,
    /// Payload size, bytes.
    pub size: u64,
    /// SHA-256 of the payload.
    pub sha256: Digest,
    /// Every file inside the payload (`sd-tree`, `boot-shim`); empty and
    /// omitted for `mtd-image`. Always serialized sorted by path.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "sorted_files"
    )]
    pub files: Vec<FileEntry>,
}

/// Component kind, tagged by the `kind` field.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ComponentKind {
    /// gzip tarball of a whole slot tree (`slots/<x>/`).
    SdTree,
    /// gzip tarball of the boot shim (`boot/bin/kboot`, `Factory/config.sh`).
    BootShim,
    /// Raw image for a NOR flash partition, found by name in `/proc/mtd`.
    MtdImage {
        /// MTD partition name ("KERNEL_A").
        partition: String,
    },
}

impl ComponentKind {
    /// The wire name ("sd-tree", "boot-shim", "mtd-image").
    pub fn as_str(&self) -> &'static str {
        match self {
            ComponentKind::SdTree => "sd-tree",
            ComponentKind::BootShim => "boot-shim",
            ComponentKind::MtdImage { .. } => "mtd-image",
        }
    }

    /// Whether the payload is an archive whose files the manifest lists.
    pub fn has_files(&self) -> bool {
        !matches!(self, ComponentKind::MtdImage { .. })
    }
}

/// One file of an archive component.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    /// Relative path inside the slot (see [`validate_rel_path`]).
    pub path: String,
    /// Size, bytes.
    pub size: u64,
    /// SHA-256 of the contents.
    pub sha256: Digest,
    /// Permission bits (`0o755` = 493), at most [`MAX_MODE`].
    pub mode: u32,
}

fn sorted_files<S: Serializer>(files: &[FileEntry], s: S) -> Result<S::Ok, S::Error> {
    let mut sorted: Vec<&FileEntry> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    s.collect_seq(sorted)
}

impl Manifest {
    /// The first component of `kind` (partition-sensitive for `mtd-image`).
    pub fn component(&self, kind: &ComponentKind) -> Option<&Component> {
        self.components.iter().find(|c| &c.kind == kind)
    }

    /// The component named `name`.
    pub fn component_named(&self, name: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.name == name)
    }

    /// The canonical signed bytes: pretty JSON, fixed field order, files
    /// sorted, trailing newline. `xtask kup` signs exactly this.
    pub fn to_json(&self) -> Vec<u8> {
        // Serializing plain structs, strings and integers cannot fail.
        let mut out = serde_json::to_vec_pretty(self).expect("manifest serializes");
        out.push(b'\n');
        out
    }

    /// Checks everything that does not depend on the device: schema, paths,
    /// unique names and paths, files present exactly for archive kinds, file
    /// modes, `created` shape. [`crate::verify`] calls it after the
    /// signature check; the builder should call it before signing.
    pub fn validate(&self) -> Result<(), Invalid> {
        if self.schema != SCHEMA {
            return Err(Invalid::new(format!(
                "schema {} (this updater reads {SCHEMA})",
                self.schema
            )));
        }
        if self.boards.is_empty() || self.boards.iter().any(|b| b.trim().is_empty()) {
            return Err(Invalid::new("boards must be a non-empty list of names"));
        }
        if self.target.trim().is_empty() {
            return Err(Invalid::new("target is empty"));
        }
        if !looks_like_rfc3339(&self.created) {
            return Err(Invalid::new(format!(
                "created {:?} is not an RFC 3339 timestamp",
                self.created
            )));
        }
        if self.components.is_empty() {
            return Err(Invalid::new("no components"));
        }
        let mut names = BTreeSet::new();
        for c in &self.components {
            if c.name.trim().is_empty() {
                return Err(Invalid::new("component with an empty name"));
            }
            if !names.insert(c.name.as_str()) {
                return Err(Invalid::new(format!("duplicate component {:?}", c.name)));
            }
            check_path(&c.path)?;
            if let ComponentKind::MtdImage { partition } = &c.kind {
                if partition.trim().is_empty() {
                    return Err(Invalid::new(format!(
                        "component {:?}: empty partition name",
                        c.name
                    )));
                }
            }
            match (c.kind.has_files(), c.files.is_empty()) {
                (true, true) => {
                    return Err(Invalid::new(format!(
                        "{} component {:?} lists no files",
                        c.kind.as_str(),
                        c.name
                    )))
                }
                (false, false) => {
                    return Err(Invalid::new(format!(
                        "{} component {:?} must not list files",
                        c.kind.as_str(),
                        c.name
                    )))
                }
                _ => {}
            }
            let mut paths = BTreeSet::new();
            for f in &c.files {
                check_path(&f.path)?;
                if !paths.insert(f.path.as_str()) {
                    return Err(Invalid::new(format!(
                        "component {:?}: duplicate file {:?}",
                        c.name, f.path
                    )));
                }
                if f.mode > MAX_MODE {
                    return Err(Invalid::new(format!(
                        "file {:?}: mode {:o} is more than permission bits",
                        f.path, f.mode
                    )));
                }
            }
        }
        Ok(())
    }
}

/// A structurally invalid manifest (see [`Manifest::validate`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalid {
    /// What is wrong, for logs and the web UI.
    pub reason: String,
    /// The offending path, when a path rule failed.
    pub path: Option<(String, PathError)>,
}

impl Invalid {
    fn new(reason: impl Into<String>) -> Self {
        Invalid {
            reason: reason.into(),
            path: None,
        }
    }
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for Invalid {}

fn check_path(path: &str) -> Result<(), Invalid> {
    validate_rel_path(path).map_err(|e| Invalid {
        reason: format!("bad path {path:?}: {e}"),
        path: Some((path.to_owned(), e)),
    })
}

/// Why a path in the manifest is unsafe to join onto a slot directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathError {
    /// The empty string.
    Empty,
    /// Longer than [`MAX_PATH_LEN`].
    TooLong,
    /// Starts with `/`.
    Absolute,
    /// Contains `\` (a Windows separator would be a plain character on Linux
    /// but a separator to whoever unpacks the card on a PC).
    Backslash,
    /// Contains a NUL or other control character.
    Control,
    /// `a//b`, or a leading/trailing `/`.
    EmptyComponent,
    /// A `.` component.
    Dot,
    /// A `..` component (path traversal).
    DotDot,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PathError::Empty => "empty path",
            PathError::TooLong => "path too long",
            PathError::Absolute => "absolute path",
            PathError::Backslash => "backslash in path",
            PathError::Control => "control character in path",
            PathError::EmptyComponent => "empty path component",
            PathError::Dot => "`.` path component",
            PathError::DotDot => "`..` path component",
        })
    }
}

impl std::error::Error for PathError {}

/// Accepts only plain relative paths: `/`-separated, no empty, `.` or `..`
/// components, no leading `/`, no backslash or control characters. Such a
/// path joined onto a slot directory can never leave it, whatever the
/// installer does with it afterwards.
pub fn validate_rel_path(path: &str) -> Result<(), PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if path.len() > MAX_PATH_LEN {
        return Err(PathError::TooLong);
    }
    if path.starts_with('/') {
        return Err(PathError::Absolute);
    }
    if path.contains('\\') {
        return Err(PathError::Backslash);
    }
    if path.chars().any(char::is_control) {
        return Err(PathError::Control);
    }
    for part in path.split('/') {
        match part {
            "" => return Err(PathError::EmptyComponent),
            "." => return Err(PathError::Dot),
            ".." => return Err(PathError::DotDot),
            _ => {}
        }
    }
    Ok(())
}

/// `YYYY-MM-DDTHH:MM:SS` followed by a fraction and/or zone (`Z`, `+hh:mm`).
/// A shape check only: the field is informational, and pulling in a date
/// crate for it is not worth the bytes on the camera.
fn looks_like_rfc3339(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 20 {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && matches!(b[10], b'T' | b't')
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19)
        && matches!(b[19], b'Z' | b'z' | b'+' | b'-' | b'.')
}

/// A SHA-256 digest, on the wire as exactly 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(pub [u8; 32]);

/// A string that is not 64 lowercase hex digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DigestError;

impl fmt::Display for DigestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sha256 must be 64 lowercase hex digits")
    }
}

impl std::error::Error for DigestError {}

impl Digest {
    /// Parses 64 lowercase hex digits. Uppercase is refused so every digest
    /// has exactly one spelling (the signed bytes stay canonical).
    pub fn parse(s: &str) -> Result<Self, DigestError> {
        let b = s.as_bytes();
        if b.len() != 64 {
            return Err(DigestError);
        }
        let nibble = |c: u8| match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => Err(DigestError),
        };
        let mut out = [0u8; 32];
        for (i, [hi, lo]) in b.as_chunks::<2>().0.iter().enumerate() {
            out[i] = nibble(*hi)? << 4 | nibble(*lo)?;
        }
        Ok(Digest(out))
    }

    /// The raw 32 bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({self})")
    }
}

impl std::str::FromStr for Digest {
    type Err = DigestError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Digest::parse(s)
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Digest::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Semver §11 precedence (pre-releases sort before the release, build
/// metadata ignored). `Version`'s own `Ord` also orders build metadata,
/// which is wrong for update decisions.
pub fn compare_versions(a: &Version, b: &Version) -> Ordering {
    a.cmp_precedence(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::sample;

    #[test]
    fn digest_round_trip_and_rejects() {
        let hex = "00ff10a0".repeat(8);
        let d = Digest::parse(&hex).unwrap();
        assert_eq!(d.to_string(), hex);
        assert_eq!(d.0[1], 0xff);
        assert!(Digest::parse(&hex.to_uppercase()).is_err());
        assert!(Digest::parse(&hex[..62]).is_err());
        assert!(Digest::parse(&format!("{hex}00")).is_err());
        assert!(Digest::parse(&"g".repeat(64)).is_err());
        assert!(Digest::parse("").is_err());
        assert!(serde_json::from_str::<Digest>("\"ABC\"").is_err());
    }

    #[test]
    fn path_rules() {
        for ok in [
            "bin/web",
            "a",
            "lib/sys/libc.so.0",
            "www/.well-known/x",
            "a..b/c",
        ] {
            assert_eq!(validate_rel_path(ok), Ok(()), "{ok}");
        }
        let cases = [
            ("", PathError::Empty),
            ("/etc/passwd", PathError::Absolute),
            ("../x", PathError::DotDot),
            ("bin/../../x", PathError::DotDot),
            ("bin/..", PathError::DotDot),
            ("./bin", PathError::Dot),
            ("bin//web", PathError::EmptyComponent),
            ("bin/", PathError::EmptyComponent),
            ("bin\\web", PathError::Backslash),
            ("..\\x", PathError::Backslash),
            ("bin/w\0eb", PathError::Control),
            ("bin/w\neb", PathError::Control),
        ];
        for (path, err) in cases {
            assert_eq!(validate_rel_path(path), Err(err), "{path:?}");
        }
        assert_eq!(
            validate_rel_path(&"a/".repeat(600)),
            Err(PathError::TooLong)
        );
    }

    #[test]
    fn serialization_is_deterministic_and_sorted() {
        let m = sample();
        let json = String::from_utf8(m.to_json()).unwrap();
        assert!(json.ends_with("}\n"));
        let bin = json.find("\"bin/web\"").unwrap();
        let www = json.find("\"www/index.html\"").unwrap();
        assert!(bin < www, "files sorted by path");
        assert!(!json.contains("notes_url"));
        // mtd-image: no files key, partition inline after kind.
        assert!(json.contains("\"kind\": \"mtd-image\",\n      \"partition\": \"KERNEL_A\""));
        let order = [
            "\"schema\"",
            "\"version\"",
            "\"channel\"",
            "\"seq\"",
            "\"components\"",
        ];
        let pos: Vec<usize> = order.iter().map(|k| json.find(k).unwrap()).collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]));
        // Round trip: parsing and re-serializing gives the same bytes.
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.to_json(), json.as_bytes());
    }

    #[test]
    fn unknown_fields_policy() {
        let mut v: serde_json::Value = serde_json::from_slice(&sample().to_json()).unwrap();
        v["future_field"] = serde_json::json!({"x": 1});
        v["components"][0]["future"] = serde_json::json!(true);
        let m: Manifest = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(m.components.len(), 2);

        v["components"][0]["files"][0]["owner"] = serde_json::json!("root");
        assert!(serde_json::from_value::<Manifest>(v.clone()).is_err());

        let mut v: serde_json::Value = serde_json::from_slice(&sample().to_json()).unwrap();
        v["components"][1]["kind"] = serde_json::json!("ubi-volume");
        assert!(serde_json::from_value::<Manifest>(v).is_err());
    }

    #[test]
    fn component_lookup() {
        let m = sample();
        assert_eq!(m.component(&ComponentKind::SdTree).unwrap().name, "slot");
        assert!(m.component(&ComponentKind::BootShim).is_none());
        let kernel = ComponentKind::MtdImage {
            partition: "KERNEL_A".into(),
        };
        assert_eq!(m.component(&kernel).unwrap().name, "kernel-a");
        assert_eq!(m.component_named("kernel-a").unwrap().size, 4);
    }

    #[test]
    fn validate_accepts_sample() {
        sample().validate().unwrap();
    }

    #[test]
    fn validate_rejects() {
        let check = |f: &dyn Fn(&mut Manifest), needle: &str| {
            let mut m = sample();
            f(&mut m);
            let err = m.validate().unwrap_err();
            assert!(err.reason.contains(needle), "{needle:?} not in {err}");
        };
        check(&|m| m.schema = 2, "schema 2");
        check(&|m| m.boards.clear(), "boards");
        check(&|m| m.target.clear(), "target");
        check(&|m| m.created = "yesterday".into(), "RFC 3339");
        check(&|m| m.components.clear(), "no components");
        check(
            &|m| m.components[1].name = "slot".into(),
            "duplicate component",
        );
        check(&|m| m.components[0].files.clear(), "lists no files");
        check(
            &|m| m.components[1].files = m.components[0].files.clone(),
            "must not list files",
        );
        check(
            &|m| m.components[0].files[0].path = "../../etc/shadow".into(),
            "`..`",
        );
        check(&|m| m.components[0].path = "/tmp/x".into(), "absolute");
        check(
            &|m| m.components[0].files[1].path = "www/index.html".into(),
            "duplicate file",
        );
        check(&|m| m.components[0].files[0].mode = 0o4755, "mode");
        check(
            &|m| {
                m.components[1].kind = ComponentKind::MtdImage {
                    partition: " ".into(),
                }
            },
            "partition",
        );

        let mut m = sample();
        m.components[0].files[0].path = "a/../b".into();
        let err = m.validate().unwrap_err();
        assert_eq!(err.path, Some(("a/../b".into(), PathError::DotDot)));
    }

    #[test]
    fn version_precedence() {
        let v = |s: &str| Version::parse(s).unwrap();
        let ordered = [
            "0.1.0",
            "0.2.0-alpha",
            "0.2.0-alpha.1",
            "0.2.0-beta",
            "0.2.0-rc.1",
            "0.2.0-rc.2",
            "0.2.0-rc.10",
            "0.2.0",
            "0.2.1",
            "0.10.0",
            "1.0.0",
        ];
        for pair in ordered.windows(2) {
            assert_eq!(
                compare_versions(&v(pair[0]), &v(pair[1])),
                Ordering::Less,
                "{} < {}",
                pair[0],
                pair[1]
            );
        }
        // Build metadata does not count for precedence.
        assert_eq!(
            compare_versions(&v("0.2.0+abc"), &v("0.2.0+def")),
            Ordering::Equal
        );
        assert!(Version::parse("v0.2.0").is_err());
        assert!(Version::parse("0.2").is_err());
    }

    #[test]
    fn rfc3339_shape() {
        assert!(looks_like_rfc3339("2026-10-17T12:00:00Z"));
        assert!(looks_like_rfc3339("2026-10-17T12:00:00.5+02:00"));
        assert!(!looks_like_rfc3339("2026-10-17 12:00:00"));
        assert!(!looks_like_rfc3339("2026-10-17T12:00:00"));
    }
}
