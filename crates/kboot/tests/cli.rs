//! End-to-end tests of the `kboot` binary against fake data roots.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A uniquely named directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("kboot-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn init(&self, rel: &str) {
        let dir = self.0.join(rel);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("init.sh"), "#!/bin/sh\n").unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn kboot(args: &[&str], root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kboot"))
        .args(args)
        .arg("--root")
        .arg(root)
        .output()
        .unwrap()
}

fn ok(out: Output) -> String {
    assert!(
        out.status.success(),
        "status {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// `kboot select` output as (KSLOT, KSLOT_DIR, KTRIAL, KVERSION), parsed by sh.
fn select(root: &Path) -> (String, String, String, String) {
    let out = ok(kboot(&["select"], root));
    for line in out.lines() {
        assert!(
            ["KSLOT=", "KSLOT_DIR='", "KTRIAL=", "KVERSION='"]
                .iter()
                .any(|p| line.starts_with(p)),
            "unexpected stdout line {line:?}"
        );
    }
    let sh = Command::new("sh")
        .arg("-c")
        .arg(r#"eval "$KOUT"; printf '%s\n%s\n%s\n%s\n' "$KSLOT" "$KSLOT_DIR" "$KTRIAL" "$KVERSION""#)
        .env("KOUT", &out)
        .output()
        .unwrap();
    assert!(sh.status.success());
    let text = String::from_utf8(sh.stdout).unwrap();
    let v: Vec<&str> = text.lines().collect();
    assert_eq!(v.len(), 4, "{text:?}");
    (v[0].into(), v[1].into(), v[2].into(), v[3].into())
}

fn status_json(root: &Path) -> String {
    ok(kboot(&["status", "--json"], root))
}

fn last(root: &Path) -> String {
    fs::read_to_string(root.join("boot/last")).unwrap()
}

fn activate_b(root: &Path) {
    ok(kboot(
        &["activate", "--slot", "b", "--version", "v2.0.0"],
        root,
    ));
}

#[test]
fn legacy_only_tree_boots_root_as_slot_a() {
    let t = TempDir::new("legacy");
    t.init("");
    let (slot, dir, trial, version) = select(t.path());
    assert_eq!(slot, "a");
    assert_eq!(dir, t.path().display().to_string());
    assert_eq!(trial, "0");
    assert_eq!(version, "legacy");
    assert_eq!(last(t.path()), "a\n");
    assert!(t.path().join("boot/sel.0").is_file());
    let log = fs::read_to_string(t.path().join("logs/boot.log")).unwrap();
    assert!(log.contains("kboot: booting slot a"), "{log}");
}

#[test]
fn activated_b_boots_as_trial_and_counts_tries() {
    let t = TempDir::new("trial");
    t.init("");
    t.init("slots/b");
    select(t.path());
    activate_b(t.path());
    let (slot, dir, trial, version) = select(t.path());
    assert_eq!((slot.as_str(), trial.as_str()), ("b", "1"));
    assert_eq!(dir, t.path().join("slots/b").display().to_string());
    assert_eq!(version, "v2.0.0");
    assert_eq!(last(t.path()), "b\n");
    let js = status_json(t.path());
    assert!(
        js.contains("\"b\":{\"bootable\":true,\"successful\":false,\"tries_left\":2"),
        "{js}"
    );

    // Committed: no longer a trial, tries stop counting.
    ok(kboot(&["mark-good"], t.path()));
    let (slot, _, trial, _) = select(t.path());
    assert_eq!((slot.as_str(), trial.as_str()), ("b", "0"));

    // Manual rollback brings a back.
    ok(kboot(&["rollback"], t.path()));
    let (slot, _, trial, _) = select(t.path());
    assert_eq!((slot.as_str(), trial.as_str()), ("a", "0"));
    assert_eq!(last(t.path()), "a\n");
}

#[test]
fn trial_never_committed_rolls_back_on_fourth_boot() {
    let t = TempDir::new("rollback");
    t.init("");
    t.init("slots/b");
    activate_b(t.path());
    for _ in 0..3 {
        let (slot, _, trial, _) = select(t.path());
        assert_eq!((slot.as_str(), trial.as_str()), ("b", "1"));
    }
    let out = kboot(&["select"], t.path());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = ok(out);
    assert!(
        stdout.contains("KSLOT=a\n") && stdout.contains("KTRIAL=0\n"),
        "{stdout}"
    );
    assert!(
        stderr.contains("rollback: slot b failed its trial"),
        "{stderr}"
    );
    let log = fs::read_to_string(t.path().join("logs/boot.log")).unwrap();
    assert!(log.contains("rollback: slot b"), "{log}");
    assert!(status_json(t.path()).contains("\"b\":{\"bootable\":false"));
    assert_eq!(last(t.path()), "a\n");
}

#[test]
fn slot_without_init_falls_back_to_a() {
    let t = TempDir::new("noinit");
    t.init("");
    fs::create_dir_all(t.path().join("slots/b/bin")).unwrap();
    activate_b(t.path());
    let out = kboot(&["select"], t.path());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    ok(out);
    assert!(stderr.contains("slot b has no init.sh"), "{stderr}");
    let (slot, dir, trial, _) = select(t.path());
    assert_eq!((slot.as_str(), trial.as_str()), ("a", "0"));
    assert_eq!(dir, t.path().display().to_string());
    assert!(status_json(t.path()).contains("\"b\":{\"bootable\":false"));
}

#[test]
fn slots_a_preferred_over_legacy_root() {
    let t = TempDir::new("slota");
    t.init("");
    t.init("slots/a");
    let (slot, dir, _, _) = select(t.path());
    assert_eq!(slot, "a");
    assert_eq!(dir, t.path().join("slots/a").display().to_string());
}

#[test]
fn nothing_bootable_fails_without_stdout() {
    let t = TempDir::new("empty");
    let out = kboot(&["select"], t.path());
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn path_with_space_survives_eval() {
    let t = TempDir::new("space dir");
    t.init("");
    let (_, dir, _, _) = select(t.path());
    assert_eq!(dir, t.path().display().to_string());
}

#[test]
fn boot_last_rewritten_only_on_change() {
    let t = TempDir::new("last");
    t.init("");
    t.init("slots/b");
    select(t.path());
    let m1 = fs::metadata(t.path().join("boot/last"))
        .unwrap()
        .modified()
        .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    select(t.path());
    let m2 = fs::metadata(t.path().join("boot/last"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(m1, m2, "unchanged slot must not rewrite boot/last");
    activate_b(t.path());
    select(t.path());
    assert_eq!(last(t.path()), "b\n");
    assert!(!t.path().join("boot/last.tmp").exists());
}

#[test]
fn guard_exits_early_when_marked_good() {
    let t = TempDir::new("guard-good");
    t.init("");
    t.init("slots/b");
    activate_b(t.path());
    select(t.path());
    let root = t.path().to_path_buf();
    let marker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        ok(kboot(&["mark-good", "--slot", "b"], &root));
    });
    let start = Instant::now();
    let out = kboot(
        &["guard", "--window", "5", "--poll", "1", "--dry-run"],
        t.path(),
    );
    marker.join().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{stderr}");
    assert!(out.stdout.is_empty());
    assert!(stderr.contains("committed"), "{stderr}");
    assert!(start.elapsed() < Duration::from_secs(4));
}

#[test]
fn guard_hits_deadline_without_commit() {
    let t = TempDir::new("guard-deadline");
    t.init("");
    t.init("slots/b");
    activate_b(t.path());
    select(t.path());
    let start = Instant::now();
    let out = kboot(
        &["guard", "--window", "2", "--poll", "1", "--dry-run"],
        t.path(),
    );
    let elapsed = start.elapsed();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(3), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "reboot (dry run)\n");
    assert!(
        stderr.contains("not committed within 2 s; rebooting"),
        "{stderr}"
    );
    assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");
}

#[test]
fn guard_on_committed_slot_exits_at_once() {
    let t = TempDir::new("guard-none");
    t.init("");
    select(t.path());
    let out = kboot(&["guard", "--window", "60", "--dry-run"], t.path());
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
}

#[test]
fn status_human_and_json() {
    let t = TempDir::new("status");
    t.init("");
    let human = ok(kboot(&["status"], t.path()));
    assert!(human.contains("none valid (legacy defaults)"), "{human}");
    assert!(human.contains("(legacy root)"), "{human}");
    select(t.path());
    let js = status_json(t.path());
    assert!(js.starts_with("{\"root\":"), "{js}");
    assert!(js.contains("\"fresh\":false"), "{js}");
    assert!(js.contains("\"last_booted\":\"a\""), "{js}");
    assert!(js.contains("\"legacy\":true"), "{js}");
    assert!(
        js.contains("\"next\":{\"slot\":\"a\",\"trial\":false}"),
        "{js}"
    );
}

#[test]
fn usage_errors_exit_2() {
    let out = Command::new(env!("CARGO_BIN_EXE_kboot"))
        .arg("select")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let out = Command::new(env!("CARGO_BIN_EXE_kboot"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("kboot "));
}
