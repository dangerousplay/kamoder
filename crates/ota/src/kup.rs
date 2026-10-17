//! `.kup` update package reader (design §4).
//!
//! A `.kup` is an **uncompressed** tar holding `manifest.json`, its minisign
//! signature `manifest.json.minisig`, and one member per component
//! (`payload.tar.gz`, optional `boot.tar.gz`, `*.img` for MTD partitions):
//!
//! ```text
//! update.kup (tar)
//! ├── manifest.json            signed; lists every component and file with size + sha256
//! ├── manifest.json.minisig
//! ├── payload.tar.gz           sd-tree    -> slots/<x>/
//! ├── boot.tar.gz              boot-shim  -> boot/, Factory/
//! └── kernel.img               mtd-image  -> /dev/mtdN by partition name
//! ```
//!
//! The tar is not extracted: [`Kup::open`] walks the headers once (seeking
//! over the data) and remembers where each member's bytes are. Readers then
//! `pread` straight out of the file, so nothing is copied to a temp file and
//! RAM use is one buffer, whatever the package size (32 MB of RAM, the
//! package can be 10+ MB).
//!
//! Trust: nothing but the two manifest members is read before the signature
//! checks out ([`Kup::verify`]); a component is then read through a
//! [`ComponentReader`] that hashes every byte and fails at the end of the
//! member if size or SHA-256 differ from the manifest. Consumers that stop
//! before the end (gzip and tar both stop at their own end markers) call
//! [`ComponentReader::finish`] to drain and check the rest.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use ota_manifest::{
    validate_rel_path, Component, Digest, Manifest, PathError, Policy, Rejection, TrustedKeys,
    MAX_MANIFEST_BYTES,
};
use ring::digest::{Context, SHA256};
use snafu::{ensure, OptionExt, ResultExt, Snafu};

/// Member holding the signed manifest.
pub const MANIFEST_NAME: &str = "manifest.json";
/// Member holding the minisign signature of [`MANIFEST_NAME`].
pub const SIGNATURE_NAME: &str = "manifest.json.minisig";
/// Largest accepted signature file (a minisign signature is ~300 bytes).
pub const MAX_SIGNATURE_BYTES: u64 = 4096;
/// Most members a package may have; real ones have 3 to 6.
pub const MAX_ENTRIES: usize = 256;

/// Errors reading a package.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("cannot open {}: {source}", path.display()))]
    Open { path: PathBuf, source: io::Error },
    #[snafu(display("{} is not a valid package (tar): {source}", path.display()))]
    Scan { path: PathBuf, source: io::Error },
    #[snafu(display("package has more than {max} members"))]
    TooManyEntries { max: usize },
    #[snafu(display("package member {name:?} has an unsafe name: {source}"))]
    EntryName { name: String, source: PathError },
    #[snafu(display("package member {name:?} appears twice"))]
    DuplicateEntry { name: String },
    #[snafu(display("package member {name:?} extends past the end of the file (truncated?)"))]
    EntryBeyondEnd { name: String },
    #[snafu(display("package has no member {name:?}"))]
    Missing { name: String },
    #[snafu(display("package member {name:?} is {size} bytes, more than {max}"))]
    TooLarge { name: String, size: u64, max: u64 },
    #[snafu(display("cannot read package member {name:?}: {source}"))]
    Read { name: String, source: io::Error },
    #[snafu(display("signature is not UTF-8 text"))]
    SignatureText,
    #[snafu(display("manifest rejected: {source}"))]
    Rejected { source: Rejection },
    #[snafu(display(
        "component {component:?}: member {member:?} is {found} bytes, manifest says {expected}"
    ))]
    SizeMismatch {
        component: String,
        member: String,
        expected: u64,
        found: u64,
    },
    #[snafu(display("component {component:?}: payload ends after {found} of {expected} bytes"))]
    Truncated {
        component: String,
        expected: u64,
        found: u64,
    },
    #[snafu(display(
        "component {component:?}: sha256 {found} does not match manifest {expected}"
    ))]
    ComponentDigest {
        component: String,
        expected: Digest,
        found: Digest,
    },
}

impl Error {
    /// Whether the package content (not the device) is at fault.
    pub fn is_bad_package(&self) -> bool {
        !matches!(self, Error::Open { .. } | Error::Read { .. })
    }
}

/// Result alias for this module.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// One regular-file member of the package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KupEntry {
    /// Member name (`payload.tar.gz`).
    pub name: String,
    /// Offset of the first data byte in the package file.
    pub offset: u64,
    /// Data size, bytes.
    pub size: u64,
}

/// A manifest whose signature checked out, with the exact signed bytes (the
/// installer copies them into the slot, so the running slot can re-verify
/// itself later, design §6 health check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedManifest {
    /// The parsed, validated manifest.
    pub manifest: Manifest,
    /// `manifest.json` bytes as signed.
    pub json: Vec<u8>,
    /// `manifest.json.minisig` text.
    pub signature: String,
}

/// An opened `.kup` with its member index.
#[derive(Debug)]
pub struct Kup {
    path: PathBuf,
    file: File,
    entries: Vec<KupEntry>,
}

impl Kup {
    /// Opens `path` and indexes its members (headers only, data is skipped
    /// with seeks). Only regular files are indexed; names must be plain
    /// relative paths (a leading `./` is dropped) and unique.
    pub fn open(path: impl AsRef<Path>) -> Result<Kup> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).context(OpenSnafu { path: &path })?;
        let len = file.metadata().context(OpenSnafu { path: &path })?.len();
        let mut entries: Vec<KupEntry> = Vec::new();
        {
            let mut archive = tar::Archive::new(&file);
            let iter = archive
                .entries_with_seek()
                .context(ScanSnafu { path: &path })?;
            for entry in iter {
                let entry = entry.context(ScanSnafu { path: &path })?;
                if !entry.header().entry_type().is_file() {
                    tracing::debug!(kind = ?entry.header().entry_type(), "skipping non-file package member");
                    continue;
                }
                let raw = entry.path().context(ScanSnafu { path: &path })?;
                let name = raw
                    .to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| Error::EntryName {
                        name: raw.to_string_lossy().into_owned(),
                        source: PathError::Control,
                    })?;
                let name = name.strip_prefix("./").unwrap_or(&name).to_owned();
                validate_rel_path(&name).context(EntryNameSnafu { name: &name })?;
                ensure!(
                    entries.iter().all(|e| e.name != name),
                    DuplicateEntrySnafu { name }
                );
                ensure!(
                    entries.len() < MAX_ENTRIES,
                    TooManyEntriesSnafu { max: MAX_ENTRIES }
                );
                let offset = entry.raw_file_position();
                let size = entry.size();
                ensure!(
                    offset.checked_add(size).is_some_and(|end| end <= len),
                    EntryBeyondEndSnafu { name }
                );
                entries.push(KupEntry { name, offset, size });
            }
        }
        Ok(Kup {
            path,
            file,
            entries,
        })
    }

    /// The package file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every indexed member, in archive order.
    pub fn entries(&self) -> &[KupEntry] {
        &self.entries
    }

    /// The member called `name`.
    pub fn entry(&self, name: &str) -> Option<&KupEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Reads a whole small member, refusing anything larger than `max`.
    pub fn read_entry(&self, name: &str, max: u64) -> Result<Vec<u8>> {
        let e = self.entry(name).context(MissingSnafu { name })?;
        ensure!(
            e.size <= max,
            TooLargeSnafu {
                name,
                size: e.size,
                max
            }
        );
        let mut buf = vec![0u8; e.size as usize];
        self.file
            .read_exact_at(&mut buf, e.offset)
            .context(ReadSnafu { name })?;
        Ok(buf)
    }

    /// Reads and verifies the manifest against `keys` (signature, schema,
    /// trusted comment, structure; OTA-02/03).
    pub fn verify(&self, keys: &TrustedKeys) -> Result<VerifiedManifest> {
        let json = self.read_entry(MANIFEST_NAME, MAX_MANIFEST_BYTES as u64)?;
        let sig = self.read_entry(SIGNATURE_NAME, MAX_SIGNATURE_BYTES)?;
        let signature = String::from_utf8(sig).map_err(|_| Error::SignatureText)?;
        let manifest = ota_manifest::verify(&json, &signature, keys).context(RejectedSnafu)?;
        Ok(VerifiedManifest {
            manifest,
            json,
            signature,
        })
    }

    /// [`Kup::verify`] then the device policy (OTA-04).
    pub fn verify_and_check(
        &self,
        keys: &TrustedKeys,
        policy: &Policy,
    ) -> Result<VerifiedManifest> {
        let v = self.verify(keys)?;
        policy.check(&v.manifest).context(RejectedSnafu)?;
        Ok(v)
    }

    /// A reader over `component`'s member that checks size and SHA-256 at
    /// the end. The member must exist with exactly the manifest's size.
    pub fn component_reader(&self, component: &Component) -> Result<ComponentReader<'_>> {
        let e = self.entry(&component.path).context(MissingSnafu {
            name: &component.path,
        })?;
        ensure!(
            e.size == component.size,
            SizeMismatchSnafu {
                component: &component.name,
                member: &e.name,
                expected: component.size,
                found: e.size,
            }
        );
        Ok(ComponentReader {
            file: &self.file,
            component: component.name.clone(),
            offset: e.offset,
            size: component.size,
            expected: component.sha256,
            pos: 0,
            ctx: Some(Context::new(&SHA256)),
            fault: None,
        })
    }
}

/// Why a [`ComponentReader`] stopped with an error (kept so the caller can
/// report the precise cause even when a decoder on top only saw an
/// `io::Error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    Truncated { found: u64 },
    Digest { found: Digest },
}

/// Reads one component member, hashing as it goes (see [`Kup::component_reader`]).
pub struct ComponentReader<'a> {
    file: &'a File,
    component: String,
    offset: u64,
    size: u64,
    expected: Digest,
    pos: u64,
    ctx: Option<Context>,
    fault: Option<Fault>,
}

impl std::fmt::Debug for ComponentReader<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentReader")
            .field("component", &self.component)
            .field("pos", &self.pos)
            .field("size", &self.size)
            .field("fault", &self.fault)
            .finish_non_exhaustive()
    }
}

impl ComponentReader<'_> {
    /// Component name, for messages.
    pub fn component(&self) -> &str {
        &self.component
    }

    /// Bytes read so far.
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Total member size.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The integrity failure seen so far, if any, as an [`Error`].
    pub fn failure(&self) -> Option<Error> {
        self.fault.map(|f| self.fault_error(f))
    }

    /// Reads to the end of the member and checks size and SHA-256 against
    /// the manifest. Idempotent: once at the end it only re-reports the
    /// outcome.
    pub fn finish(&mut self) -> Result<()> {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match self.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(source) => {
                    return Err(self.failure().unwrap_or(Error::Read {
                        name: self.component.clone(),
                        source,
                    }))
                }
            }
        }
        match self.failure() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn fault_error(&self, fault: Fault) -> Error {
        match fault {
            Fault::Truncated { found } => Error::Truncated {
                component: self.component.clone(),
                expected: self.size,
                found,
            },
            Fault::Digest { found } => Error::ComponentDigest {
                component: self.component.clone(),
                expected: self.expected,
                found,
            },
        }
    }

    fn fail(&mut self, fault: Fault) -> io::Error {
        self.fault = Some(fault);
        io::Error::new(io::ErrorKind::InvalidData, self.fault_error(fault))
    }
}

impl Read for ComponentReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(f) = self.fault {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                self.fault_error(f),
            ));
        }
        let remaining = self.size - self.pos;
        if remaining == 0 {
            if let Some(ctx) = self.ctx.take() {
                let found = digest_of(ctx);
                if found != self.expected {
                    return Err(self.fail(Fault::Digest { found }));
                }
            }
            return Ok(0);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let want = buf
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let n = self
            .file
            .read_at(&mut buf[..want], self.offset + self.pos)?;
        if n == 0 {
            let found = self.pos;
            return Err(self.fail(Fault::Truncated { found }));
        }
        if let Some(ctx) = self.ctx.as_mut() {
            ctx.update(&buf[..n]);
        }
        self.pos += n as u64;
        Ok(n)
    }
}

/// Finishes a SHA-256 context into a [`Digest`].
pub(crate) fn digest_of(ctx: Context) -> Digest {
    let d = ctx.finish();
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    Digest(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, sha256, Fixture, TempDir};

    #[test]
    fn indexes_members_and_verifies_manifest() {
        let tmp = TempDir::new("kup-open");
        let fx = Fixture::standard();
        let kup_path = fx.write_kup(tmp.path());
        let kup = Kup::open(&kup_path).unwrap();
        let names: Vec<_> = kup.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, [MANIFEST_NAME, SIGNATURE_NAME, "payload.tar.gz"]);
        let v = kup.verify(&TrustedKeys::test_key()).unwrap();
        // Files come back sorted (canonical form), so compare serialized.
        assert_eq!(v.manifest.to_json(), fx.manifest().to_json());
        assert_eq!(v.json, fx.manifest().to_json());
    }

    #[test]
    fn untrusted_key_is_rejected() {
        let tmp = TempDir::new("kup-key");
        let kup = Kup::open(Fixture::standard().write_kup(tmp.path())).unwrap();
        let err = kup.verify(&TrustedKeys::empty()).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Rejected {
                    source: Rejection::NoTrustedKey
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn component_reader_checks_digest_at_eof() {
        let tmp = TempDir::new("kup-digest");
        let fx = Fixture::standard();
        let kup = Kup::open(fx.write_kup(tmp.path())).unwrap();
        let manifest = fx.manifest();
        let comp = &manifest.components[0];
        let mut r = kup.component_reader(comp).unwrap();
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        assert_eq!(sha256(&all), comp.sha256);
        r.finish().unwrap();

        let mut bad = comp.clone();
        bad.sha256 = Digest([7; 32]);
        let mut r = kup.component_reader(&bad).unwrap();
        assert!(matches!(
            r.finish().unwrap_err(),
            Error::ComponentDigest { .. }
        ));

        bad.size += 1;
        assert!(matches!(
            kup.component_reader(&bad).unwrap_err(),
            Error::SizeMismatch { .. }
        ));
    }

    #[test]
    fn truncated_package_is_detected() {
        let tmp = TempDir::new("kup-trunc");
        let path = Fixture::standard().write_kup(tmp.path());
        let bytes = std::fs::read(&path).unwrap();
        // Cut into the payload data (the last member), keep a whole header.
        let kup = Kup::open(&path).unwrap();
        let payload = kup.entry("payload.tar.gz").unwrap().clone();
        std::fs::write(
            &path,
            &bytes[..(payload.offset + payload.size / 2) as usize],
        )
        .unwrap();
        let err = Kup::open(&path).unwrap_err();
        assert!(
            matches!(err, Error::EntryBeyondEnd { .. } | Error::Scan { .. }),
            "{err}"
        );
    }

    #[test]
    fn duplicate_and_unsafe_member_names_are_rejected() {
        let tmp = TempDir::new("kup-names");
        let dup = tmp.path().join("dup.kup");
        test_support::write_tar(&dup, &[("a", b"1".as_slice()), ("a", b"2".as_slice())]);
        assert!(matches!(
            Kup::open(&dup).unwrap_err(),
            Error::DuplicateEntry { .. }
        ));

        let evil = tmp.path().join("evil.kup");
        let mut tar = test_support::RawTar::default();
        tar.file("../manifest.json", b"{}", 0o644);
        tar.finish_to(&evil);
        assert!(matches!(
            Kup::open(&evil).unwrap_err(),
            Error::EntryName {
                source: PathError::DotDot,
                ..
            }
        ));
    }

    #[test]
    fn missing_manifest() {
        let tmp = TempDir::new("kup-missing");
        let p = tmp.path().join("x.kup");
        test_support::write_tar(&p, &[("payload.tar.gz", b"xx".as_slice())]);
        let kup = Kup::open(&p).unwrap();
        assert!(matches!(
            kup.verify(&TrustedKeys::test_key()).unwrap_err(),
            Error::Missing { .. }
        ));
    }
}
