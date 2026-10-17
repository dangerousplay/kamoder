//! Host tests for the boot shell scripts: `card/Factory/config.sh` (slot
//! choice, OTA-09/12/13) and `tools/init.sh` (code from the slot, data from
//! the data root, OTA-05).
//!
//! They run the real scripts with the host's `sh` against a fake card in a
//! temporary directory, with stub `kboot`, `init.sh` and `supervisor`
//! scripts that record how they were called. The camera runs BusyBox ash;
//! the host `sh` (dash or bash) is close enough for path logic, not a
//! substitute for a boot on the device. `config.sh` takes two test-only
//! variables for this: `KAMODER_SD_ROOTS` (card search list, no mount) and
//! `KAMODER_TMP` (instead of `/tmp`).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A directory removed on drop (no tempfile crate in the workspace).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kamoder-boot-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Canonical, so paths the scripts print (`cd && pwd`) compare equal.
        TempDir(fs::canonicalize(&dir).unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn write_exe(path: &Path, text: &str) {
    write(path, text);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// config.sh
// ---------------------------------------------------------------------------

/// A fake card: `<tmp>/sd/anyka_rust` with camera.conf (telnetd off).
struct Card {
    tmp: TempDir,
}

impl Card {
    fn new(tag: &str) -> Self {
        let card = Card {
            tmp: TempDir::new(tag),
        };
        write(&card.rust().join("camera.conf"), "start_telnetd=0\n");
        fs::create_dir_all(card.tmp.0.join("tmp")).unwrap();
        card
    }

    fn rust(&self) -> PathBuf {
        self.tmp.0.join("sd/anyka_rust")
    }

    fn ran(&self) -> PathBuf {
        self.tmp.0.join("ran")
    }

    /// A stub init.sh in `dir` that records `$0|KAMODER_DATA|KAMODER_SLOT`.
    fn init_in(&self, dir: &Path) {
        write(
            &dir.join("init.sh"),
            &format!(
                "printf '%s|%s|%s\\n' \"$0\" \"$KAMODER_DATA\" \"$KAMODER_SLOT\" >> '{}'\n",
                self.ran().display()
            ),
        );
    }

    /// A stub kboot: `select` prints `output` and exits `status`; `guard`
    /// records its arguments.
    fn kboot(&self, output: &str, status: i32) {
        let guard = self.tmp.0.join("guard");
        write_exe(
            &self.rust().join("boot/bin/kboot"),
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n  select) printf '%s\\n' \"{output}\"; exit {status} ;;\n  guard) echo \"$*\" > '{}' ;;\nesac\n",
                guard.display()
            ),
        );
    }

    fn last(&self, slot: &str) {
        write(&self.rust().join("boot/last"), &format!("{slot}\n"));
    }

    fn run(&self) -> std::process::Output {
        let output = Command::new("sh")
            .arg(repo().join("card/Factory/config.sh"))
            .env("KAMODER_SD_ROOTS", self.tmp.0.join("sd"))
            .env("KAMODER_TMP", self.tmp.0.join("tmp"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}\n{}", self.boot_log());
        output
    }

    /// The single init.sh run: (script, data, slot).
    fn ran_once(&self) -> (PathBuf, PathBuf, PathBuf) {
        let ran = read(&self.ran());
        let lines: Vec<_> = ran.lines().collect();
        assert_eq!(lines.len(), 1, "init.sh runs: {ran:?}\n{}", self.boot_log());
        let parts: Vec<_> = lines[0].split('|').map(PathBuf::from).collect();
        (parts[0].clone(), parts[1].clone(), parts[2].clone())
    }

    fn boot_log(&self) -> String {
        read(&self.rust().join("logs/boot.log"))
    }

    fn guard(&self) -> Option<String> {
        // The guard is backgrounded; give it a moment.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = read(&self.tmp.0.join("guard"));
            if !text.is_empty() {
                return Some(text.trim().to_owned());
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn assert_ran(&self, slot_dir: &Path) {
        let (script, data, slot) = self.ran_once();
        assert_eq!(script, slot_dir.join("init.sh"));
        assert_eq!(data, self.rust());
        assert_eq!(slot, slot_dir);
    }
}

/// OTA-12: no boot/ at all, the legacy tree boots as before.
#[test]
fn config_sh_boots_a_legacy_tree() {
    let card = Card::new("legacy");
    card.init_in(&card.rust());
    card.run();
    card.assert_ran(&card.rust());
    let log = card.boot_log();
    assert!(log.contains("no "), "{log}");
    assert!(log.contains("legacy tree"), "{log}");
    assert!(card.guard().is_none());
}

#[test]
fn config_sh_boots_the_slot_kboot_chooses() {
    let card = Card::new("kboot-b");
    card.init_in(&card.rust());
    let b = card.rust().join("slots/b");
    card.init_in(&b);
    card.kboot(&format!("KSLOT=b KSLOT_DIR={} KTRIAL=0", b.display()), 0);
    card.run();
    card.assert_ran(&b);
    assert!(card.boot_log().contains("kboot chose slot b"));
    assert!(read(&card.tmp.0.join("guard")).is_empty());
}

/// OTA-10: a trial boot starts `kboot guard`, detached.
#[test]
fn config_sh_starts_the_guard_on_a_trial_boot() {
    let card = Card::new("trial");
    let b = card.rust().join("slots/b");
    card.init_in(&b);
    card.kboot(&format!("KSLOT=b KSLOT_DIR={} KTRIAL=1", b.display()), 0);
    card.run();
    card.assert_ran(&b);
    assert_eq!(
        card.guard().as_deref(),
        Some(format!("guard --root {} --window 600", card.rust().display()).as_str())
    );
}

/// OTA-13: kboot missing, boot/last names a usable slot.
#[test]
fn config_sh_falls_back_to_boot_last() {
    let card = Card::new("last");
    card.init_in(&card.rust());
    let b = card.rust().join("slots/b");
    card.init_in(&b);
    card.last("b");
    card.run();
    card.assert_ran(&b);
    assert!(card.boot_log().contains("boot/last says slot b"));
}

/// OTA-13: kboot fails and there is no boot/last: the legacy tree.
#[test]
fn config_sh_falls_back_to_legacy_when_kboot_fails() {
    let card = Card::new("kboot-fails");
    card.init_in(&card.rust());
    card.init_in(&card.rust().join("slots/b"));
    card.kboot("KSLOT=b", 1);
    card.run();
    card.assert_ran(&card.rust());
    assert!(card.boot_log().contains("kboot select failed"));
}

/// Garbage (here a shell syntax error) from kboot must not end config.sh.
#[test]
fn config_sh_survives_unparseable_kboot_output() {
    let card = Card::new("garbage");
    card.init_in(&card.rust());
    let b = card.rust().join("slots/b");
    card.init_in(&b);
    card.last("b");
    card.kboot("KSLOT='b", 0);
    card.run();
    card.assert_ran(&b);
    assert!(card.boot_log().contains("kboot output unusable"));
}

/// A slot without init.sh is never started, whoever names it.
#[test]
fn config_sh_skips_a_slot_without_init_sh() {
    let card = Card::new("empty-slot");
    card.init_in(&card.rust());
    let b = card.rust().join("slots/b");
    fs::create_dir_all(&b).unwrap();
    card.kboot(&format!("KSLOT=b KSLOT_DIR={} KTRIAL=0", b.display()), 0);
    card.last("b");
    card.run();
    card.assert_ran(&card.rust());
    let log = card.boot_log();
    assert!(log.contains("kboot output unusable"), "{log}");
    assert!(log.contains("names no runnable slot"), "{log}");
}

/// Slot a is the legacy tree until slots/a exists (design D-O2).
#[test]
fn config_sh_maps_slot_a() {
    let card = Card::new("slot-a-legacy");
    card.init_in(&card.rust());
    card.last("a");
    card.run();
    card.assert_ran(&card.rust());

    let card = Card::new("slot-a-dir");
    card.init_in(&card.rust());
    let a = card.rust().join("slots/a");
    card.init_in(&a);
    card.last("a");
    card.run();
    card.assert_ran(&a);
}

/// After the legacy tree is gone, a slot still boots without kboot.
#[test]
fn config_sh_last_resort_is_any_runnable_slot() {
    let card = Card::new("no-legacy");
    let b = card.rust().join("slots/b");
    card.init_in(&b);
    card.run();
    card.assert_ran(&b);
}

#[test]
fn config_sh_honours_run_rust_0() {
    let card = Card::new("run-rust-0");
    write(
        &card.rust().join("camera.conf"),
        "start_telnetd=0\nrun_rust=0\n",
    );
    card.init_in(&card.rust());
    card.run();
    assert!(read(&card.ran()).is_empty());
}

// ---------------------------------------------------------------------------
// init.sh
// ---------------------------------------------------------------------------

/// A fake stack in `dir`: init.sh, a stub supervisor that records its
/// arguments, supervisor.toml and the runtime library stubs.
fn stack(dir: &Path, calls: &Path) {
    fs::create_dir_all(dir).unwrap();
    fs::copy(repo().join("tools/init.sh"), dir.join("init.sh")).unwrap();
    write_exe(
        &dir.join("bin/supervisor"),
        &format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$1\" in\n  board) printf \"KAMODER_BOARD_NAME='test'\\nVENDOR_PROCESSES=''\\nWATCHDOG_WAIT=0\\n\" ;;\nesac\nexit 0\n",
            calls.display()
        ),
    );
    write(&dir.join("supervisor.toml"), "");
    for lib in [
        "libpthread.so.0",
        "libm.so.0",
        "librt.so.0",
        "libdl.so.0",
        "libgcc_s.so.1",
        "libstdc++.so.6",
    ] {
        write(&dir.join("lib/sys").join(lib), "");
    }
}

fn init_sh(script: &Path, env: &[(&str, &Path)], mode: &str) -> std::process::Output {
    let mut command = Command::new("sh");
    command
        .arg(script)
        .arg(mode)
        .env_remove("KAMODER_DATA")
        .env_remove("KAMODER_SLOT");
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    output
}

fn check_call(calls: &Path) -> String {
    read(calls)
        .lines()
        .find(|line| line.ends_with("--check"))
        .unwrap_or_default()
        .to_owned()
}

/// Legacy tree: the supervisor is called exactly as before slots existed.
#[test]
fn init_sh_legacy_tree_is_unchanged() {
    let tmp = TempDir::new("init-legacy");
    let root = tmp.0.join("anyka_rust");
    let calls = tmp.0.join("calls");
    stack(&root, &calls);
    init_sh(&root.join("init.sh"), &[], "--check");
    assert_eq!(
        check_call(&calls),
        format!("--config {}/supervisor.toml --check", root.display())
    );
    assert!(root.join("logs/init.log").exists() || root.join("logs").is_dir());
}

/// Booted as a slot: code from the slot, data (logs) in the data root.
#[test]
fn init_sh_slot_mode_splits_code_and_data() {
    let tmp = TempDir::new("init-slot");
    let root = tmp.0.join("anyka_rust");
    let slot = root.join("slots/b");
    let calls = tmp.0.join("calls");
    stack(&slot, &calls);
    init_sh(
        &slot.join("init.sh"),
        &[("KAMODER_DATA", &root), ("KAMODER_SLOT", &slot)],
        "--check",
    );
    assert_eq!(
        check_call(&calls),
        format!(
            "--config {s}/supervisor.toml --slot-dir {s} --data-dir {d} --check",
            s = slot.display(),
            d = root.display()
        )
    );
    assert!(root.join("logs").is_dir());
    assert!(!slot.join("logs").exists());
}

/// Run by hand from slots/<x>/ without the variables, the data root is
/// still found, so camera.conf and the logs are the real ones.
#[test]
fn init_sh_run_from_a_slot_finds_the_data_root() {
    let tmp = TempDir::new("init-hand");
    let root = tmp.0.join("anyka_rust");
    let slot = root.join("slots/a");
    let calls = tmp.0.join("calls");
    stack(&slot, &calls);
    init_sh(&slot.join("init.sh"), &[], "--check");
    assert!(
        check_call(&calls).contains(&format!("--data-dir {} --check", root.display())),
        "{}",
        read(&calls)
    );
    assert!(root.join("logs").is_dir());
}

/// `--stop` needs no slot: it works from the PID files in /tmp and logs to
/// the data root. Skipped if a real stack's PID files are present on this
/// host, which a stop would signal.
#[test]
fn init_sh_stop_works_from_a_slot() {
    if [
        "/tmp/anyka_rust.pid",
        "/tmp/anyka_rust.loop.pid",
        "/tmp/anyka_rust.guard.pid",
    ]
    .iter()
    .any(|p| Path::new(p).exists())
    {
        eprintln!("skipped: /tmp/anyka_rust*.pid present");
        return;
    }
    let tmp = TempDir::new("init-stop");
    let root = tmp.0.join("anyka_rust");
    let slot = root.join("slots/b");
    let calls = tmp.0.join("calls");
    stack(&slot, &calls);
    init_sh(
        &slot.join("init.sh"),
        &[("KAMODER_DATA", &root), ("KAMODER_SLOT", &slot)],
        "--stop",
    );
    assert!(read(&root.join("logs/init.log")).contains("init: stopped"));
}
