//! `./x itest` — system tests of the SD card on a simulated camera.
//!
//! Builds the card (`./x sd`) and the test-support tool, assembles the VM
//! images with `itest/build-images.sh`, and runs the pytest suite in
//! `itest/tests` under labgrid, which boots the VM and drives its console.
//! See `itest/env/qemu.yaml` and `itest/guest/firmware-init` for what is
//! simulated.
//!
//! The test gets its own `camera.conf` with throwaway SSH, web admin and
//! ONVIF passwords (passed to pytest as `LG_ITEST_PASSWORD`,
//! `LG_ITEST_WEB_PASSWORD`, `LG_ITEST_ONVIF_PASSWORD`); the real card's
//! `camera.conf` is never copied into the images.
//!
//!   ./x itest                    build everything, run the whole suite
//!   ./x itest --no-build         reuse the last card and support build
//!   ./x itest -- -k wifi -x      anything after `--` goes to pytest

use crate::tasks;
use eyre::{bail, Result, WrapErr};
use std::io::Read;
use std::path::Path;
use std::process::Command;

pub fn itest(extra: &[String]) -> Result<()> {
    let (ours, pytest_args) = match extra.iter().position(|a| a == "--") {
        Some(i) => (&extra[..i], &extra[i + 1..]),
        None => (extra, &[][..]),
    };
    let skip_build = ours.iter().any(|a| a == "--no-build");

    let root = tasks::workspace_root();
    let vm = tasks::vm_out_dir();
    let kernel = vm.join("zImage");
    for (path, hint) in [
        (
            vm.join("zImage"),
            "build it with tools/qemu-armv5/run.sh build",
        ),
        (
            vm.join("rootfs.pristine"),
            "run tools/qemu-armv5/run.sh extract && run.sh rootfs",
        ),
        (
            vm.join("app.squashfs"),
            "run tools/qemu-armv5/run.sh extract",
        ),
    ] {
        if !path.exists() {
            bail!("{} not found; {hint}", path.display());
        }
    }
    for tool in [
        "qemu-system-arm",
        "fakeroot",
        "cpio",
        "sfdisk",
        "mkfs.vfat",
        "mcopy",
        "sshpass",
        "uv",
    ] {
        if tasks::which(tool).is_none() {
            bail!("{tool} is required for ./x itest");
        }
    }

    let work = root.join("target/itest");
    std::fs::create_dir_all(&work)?;
    let secrets = Secrets {
        ssh: random_password()?,
        web: random_password()?,
        onvif: random_password()?,
    };

    if !skip_build {
        tasks::sd(&[])?;
        tasks::run_cargo(
            "build",
            &[
                "--release".into(),
                "-p".into(),
                "anyka-itest-support".into(),
            ],
        )?;
    }
    let conf = work.join("camera.conf");
    write_test_conf(&root.join("tools/camera.conf.example"), &conf, &secrets)?;
    let status = Command::new(root.join("itest/build-images.sh"))
        .arg(vm.join("rootfs.pristine"))
        .arg(vm.join("app.squashfs"))
        .arg(tasks::sd_contents_dir()?)
        .arg(root.join("target").join(tasks::TARGET).join("release"))
        .arg(&conf)
        .arg(&work)
        .status()
        .wrap_err("cannot run itest/build-images.sh")?;
    if !status.success() {
        bail!("build-images.sh failed with {status}");
    }

    let status = Command::new("uv")
        .args([
            "run",
            "--project",
            "itest",
            "pytest",
            "itest/tests",
            "--lg-env",
            "itest/env/qemu.yaml",
        ])
        // Every console byte, for when a boot hangs before the shell.
        .arg("--lg-log")
        .arg(work.join("console"))
        .args(pytest_args)
        .current_dir(&root)
        .env("LG_KERNEL", &kernel)
        .env("LG_ITEST_DIR", &work)
        .env("LG_ITEST_PASSWORD", &secrets.ssh)
        .env("LG_ITEST_WEB_PASSWORD", &secrets.web)
        .env("LG_ITEST_ONVIF_PASSWORD", &secrets.onvif)
        .status()
        .wrap_err("cannot run pytest")?;
    if !status.success() {
        bail!("integration tests failed");
    }
    Ok(())
}

/// Throwaway passwords for one run.
struct Secrets {
    ssh: String,
    web: String,
    onvif: String,
}

/// The example config with the simulated world's values: HomeNet desired,
/// fresh SSH, web admin and ONVIF passwords, the rescue telnetd on.
fn write_test_conf(example: &Path, out: &Path, secrets: &Secrets) -> Result<()> {
    let text = std::fs::read_to_string(example)?;
    let mut conf = String::new();
    for line in text.lines() {
        let line = match line.split_once('=') {
            Some(("wifi_ssid", _)) => "wifi_ssid='HomeNet'".to_owned(),
            Some(("wifi_password", _)) => "wifi_password='homepass1'".to_owned(),
            Some(("ssh_password", _)) => format!("ssh_password='{}'", secrets.ssh),
            Some(("web_admin_user", _)) => "web_admin_user='admin'".to_owned(),
            Some(("web_admin_password", _)) => {
                format!("web_admin_password='{}'", secrets.web)
            }
            Some(("onvif_user", _)) => "onvif_user='onvif'".to_owned(),
            Some(("onvif_password", _)) => format!("onvif_password='{}'", secrets.onvif),
            Some(("start_telnetd", _)) => "start_telnetd=1".to_owned(),
            _ => line.to_owned(),
        };
        conf.push_str(&line);
        conf.push('\n');
    }
    std::fs::write(out, conf)?;
    Ok(())
}

/// A throwaway password per run, from the kernel CSPRNG. 64 symbols, so the
/// modulo is unbiased.
fn random_password() -> Result<String> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789-_.@%+=";
    let mut bytes = [0u8; 20];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect())
}
