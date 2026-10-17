//! Test helpers: temp dirs (hand-rolled, like `bootctl`), raw tar writing
//! (so tests can produce entries a well-behaved builder refuses: `../`,
//! symlinks), gzip, and signed `.kup` fixtures. Signing uses the committed
//! TEST-ONLY key through the pure-Rust `minisign` crate.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ota_manifest::{
    trusted_comment, Channel, Component, ComponentKind, Digest, FileEntry, Manifest, Version,
};
use ring::digest::{digest, SHA256};

/// The TEST-ONLY secret key (unencrypted; `keys/test/README` in ota-manifest).
const TEST_SECRET_KEY: &str = include_str!("../../ota-manifest/keys/test/test.key");

/// A uniquely named directory under the system temp dir, removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("ota-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn sha256(data: &[u8]) -> Digest {
    let mut out = [0u8; 32];
    out.copy_from_slice(digest(&SHA256, data).as_ref());
    Digest(out)
}

/// Tar writer that puts names into headers verbatim.
#[derive(Default)]
pub(crate) struct RawTar {
    out: Vec<u8>,
}

impl RawTar {
    fn header(name: &str, kind: tar::EntryType, size: u64, mode: u32) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        let bytes = name.as_bytes();
        assert!(bytes.len() < 100, "test names stay short");
        h.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
        h.set_entry_type(kind);
        h.set_size(size);
        h.set_mode(mode);
        h.set_mtime(1_760_700_000);
        h.set_cksum();
        h
    }

    fn push(&mut self, h: &tar::Header, data: &[u8]) {
        self.out.extend_from_slice(h.as_bytes());
        self.out.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        self.out.extend(std::iter::repeat_n(0u8, pad));
    }

    pub(crate) fn file(&mut self, name: &str, data: &[u8], mode: u32) -> &mut Self {
        let h = Self::header(name, tar::EntryType::Regular, data.len() as u64, mode);
        self.push(&h, data);
        self
    }

    pub(crate) fn dir(&mut self, name: &str) -> &mut Self {
        let h = Self::header(name, tar::EntryType::Directory, 0, 0o755);
        self.push(&h, &[]);
        self
    }

    pub(crate) fn symlink(&mut self, name: &str, target: &str) -> &mut Self {
        let mut h = Self::header(name, tar::EntryType::Symlink, 0, 0o777);
        h.set_link_name(target).unwrap();
        h.set_cksum();
        self.push(&h, &[]);
        self
    }

    pub(crate) fn finish(&mut self) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.out);
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    pub(crate) fn finish_to(&mut self, path: &Path) {
        std::fs::write(path, self.finish()).unwrap();
    }
}

pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// An uncompressed tar of plain files.
pub(crate) fn write_tar(path: &Path, members: &[(&str, &[u8])]) {
    let mut t = RawTar::default();
    for (name, data) in members {
        t.file(name, data, 0o644);
    }
    t.finish_to(path);
}

/// A file of a fixture: path, contents, mode.
pub(crate) type Spec = (String, Vec<u8>, u32);

pub(crate) fn specs(files: &[(&str, &[u8], u32)]) -> Vec<Spec> {
    files
        .iter()
        .map(|(p, d, m)| (p.to_string(), d.to_vec(), *m))
        .collect()
}

/// A gzip tar holding exactly `files` (directories implied).
pub(crate) fn tar_gz(files: &[Spec]) -> Vec<u8> {
    let mut t = RawTar::default();
    for (p, d, m) in files {
        t.file(p, d, *m);
    }
    gzip(&t.finish())
}

fn entries(files: &[Spec]) -> Vec<FileEntry> {
    files
        .iter()
        .map(|(p, d, m)| FileEntry {
            path: p.clone(),
            size: d.len() as u64,
            sha256: sha256(d),
            mode: *m,
        })
        .collect()
}

/// One component of a fixture: the files the manifest lists and the
/// member bytes actually in the package (normally `tar_gz(files)`).
pub(crate) struct Part {
    pub name: String,
    pub kind: ComponentKind,
    pub member: String,
    pub files: Vec<Spec>,
    pub payload: Vec<u8>,
}

/// A package under construction.
pub(crate) struct Fixture {
    pub version: &'static str,
    /// Manifest `seq` (anti-replay counter).
    pub seq: u64,
    pub parts: Vec<Part>,
}

impl Fixture {
    pub(crate) fn tree(files: &[(&str, &[u8], u32)]) -> Fixture {
        let files = specs(files);
        Fixture {
            version: "0.2.0",
            seq: 1_760_700_000,
            parts: vec![Part {
                name: "slot".into(),
                kind: ComponentKind::SdTree,
                member: "payload.tar.gz".into(),
                payload: tar_gz(&files),
                files,
            }],
        }
    }

    /// A small slot tree.
    pub(crate) fn standard() -> Fixture {
        Fixture::tree(&[
            ("bin/web", b"\x7fELF web binary".as_slice(), 0o755),
            ("lib/libfoo.so", b"\x7fELF lib".as_slice(), 0o755),
            ("www/index.html", b"<html></html>".as_slice(), 0o644),
            ("supervisor.toml", b"[services]\n".as_slice(), 0o644),
            ("init.sh", b"#!/bin/sh\n".as_slice(), 0o755),
        ])
    }

    pub(crate) fn with_boot(mut self, files: &[(&str, &[u8], u32)]) -> Fixture {
        let files = specs(files);
        self.parts.push(Part {
            name: "boot".into(),
            kind: ComponentKind::BootShim,
            member: "boot.tar.gz".into(),
            payload: tar_gz(&files),
            files,
        });
        self
    }

    pub(crate) fn with_image(mut self, name: &str, partition: &str, image: &[u8]) -> Fixture {
        self.parts.push(Part {
            name: name.into(),
            kind: ComponentKind::MtdImage {
                partition: partition.into(),
            },
            member: format!("{name}.img"),
            files: Vec::new(),
            payload: image.to_vec(),
        });
        self
    }

    /// The manifest; component size/sha256 follow the actual member bytes.
    pub(crate) fn manifest(&self) -> Manifest {
        let v: Version = self.version.parse().unwrap();
        Manifest {
            schema: 1,
            version: v,
            channel: Channel::Stable,
            seq: self.seq,
            security_floor: "0.1.0".parse().unwrap(),
            min_updater: "0.1.0".parse().unwrap(),
            boards: vec!["test-board".into()],
            target: "armv5te-unknown-linux-uclibceabi".into(),
            created: "2026-10-17T12:00:00Z".into(),
            notes_url: None,
            components: self
                .parts
                .iter()
                .map(|p| Component {
                    name: p.name.clone(),
                    kind: p.kind.clone(),
                    path: p.member.clone(),
                    size: p.payload.len() as u64,
                    sha256: sha256(&p.payload),
                    files: entries(&p.files),
                })
                .collect(),
        }
    }

    /// Writes `update.kup` into `dir` (manifest signed with the test key).
    pub(crate) fn write_kup(&self, dir: &Path) -> PathBuf {
        let manifest = self.manifest();
        let json = manifest.to_json();
        let sig = sign(&json, &trusted_comment(&manifest));
        let mut t = RawTar::default();
        t.file(crate::kup::MANIFEST_NAME, &json, 0o644);
        t.file(crate::kup::SIGNATURE_NAME, sig.as_bytes(), 0o644);
        for p in &self.parts {
            t.file(&p.member, &p.payload, 0o644);
        }
        let path = dir.join("update.kup");
        t.finish_to(&path);
        path
    }
}

/// minisign signature of `data` with the TEST-ONLY key.
///
/// The committed key is unencrypted with an all-zero checksum field (the C
/// `minisign` CLI does not check it for unencrypted keys, the crate's
/// `into_unencrypted_secret_key` does), so the raw key bytes are parsed
/// directly.
pub(crate) fn sign(data: &[u8], comment: &str) -> String {
    use base64::Engine;
    let line = TEST_SECRET_KEY.lines().nth(1).expect("key line");
    let raw = base64::engine::general_purpose::STANDARD
        .decode(line.trim())
        .unwrap();
    let sk = minisign::SecretKey::from_bytes(&raw).unwrap();
    minisign::sign(None, &sk, data, Some(comment), Some("ota test"))
        .unwrap()
        .into_string()
}
