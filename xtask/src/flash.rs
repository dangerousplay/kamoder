//! `./x flash` — write the SD card tree onto a physical card.
//!
//! Runs `./x sd`, mounts the card through udisks (no root needed), then
//! copies `autorun.sh`, `Factory/` and `anyka_rust/` from the SD card tree
//! (`dist/sdcard`, or `$ANYKA_SD_TREE`) to the root of the card. Everything else on the card
//! (anyka_happy, vendor files) is left alone.
//!
//! Inside `anyka_rust/`, the camera-owned entries (`SD_PRESERVED`) are never
//! overwritten: the camera blanks `ssh_password` from its `camera.conf` after
//! hashing it, and keeps keys, logs and backups next to it. They are only
//! seeded when the card does not have them yet.
//!
//!   ./x flash                        build, then write to the card labelled ANYKA
//!   ./x flash --no-build             write the last `./x sd` output
//!   ./x flash --device /dev/sdc1     pick the partition explicitly
//!   ./x flash --keep-mounted         leave the card mounted afterwards

use crate::tasks;
use eyre::{bail, eyre, Result, WrapErr};
use std::path::{Path, PathBuf};
use std::process::Command;

const DEVICE_ENV: &str = "ANYKA_SD_DEVICE";
const DEFAULT_LABEL: &str = "ANYKA";

pub fn flash(extra: &[String]) -> Result<()> {
    let mut skip_build = false;
    let mut keep_mounted = false;
    let mut device = std::env::var(DEVICE_ENV).ok().map(PathBuf::from);
    let mut args = extra.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--no-build" => skip_build = true,
            "--keep-mounted" => keep_mounted = true,
            "--device" => {
                let value = args.next().ok_or_else(|| eyre!("--device needs a path"))?;
                device = Some(PathBuf::from(value));
            }
            other => bail!("unknown flash option {other:?}"),
        }
    }

    // Resolve and vet the card before the (slow) build, so a missing card
    // fails in a second rather than after a full cross-compile.
    let device = match device {
        Some(device) => device,
        None => {
            let by_label = Path::new("/dev/disk/by-label").join(DEFAULT_LABEL);
            if !by_label.exists() {
                bail!(
                    "no partition labelled {DEFAULT_LABEL}; insert the card, \
                     or pass --device /dev/sdXN (or set {DEVICE_ENV})"
                );
            }
            by_label
        }
    };
    let device = device
        .canonicalize()
        .wrap_err_with(|| format!("cannot resolve {}", device.display()))?;
    ensure_removable_vfat(&device)?;

    if !skip_build {
        tasks::sd(&[])?;
    }

    let source = tasks::sd_contents_dir()?;
    if !source.join("anyka_rust/bin").is_dir() {
        bail!("{} is not staged; run ./x sd first", source.display());
    }

    let (mount, mounted_here) = mount(&device)?;
    println!("\nwriting to {} ({})", mount.display(), device.display());

    copy_file(&source.join("autorun.sh"), &mount.join("autorun.sh"))?;
    replace_dir(&source.join("Factory"), &mount.join("Factory"))?;
    sync_anyka_rust(&source.join("anyka_rust"), &mount.join("anyka_rust"))?;

    tasks::run("sync", &[])?;
    if mounted_here && !keep_mounted {
        tasks::run("udisksctl", &["unmount", "-b", &device.to_string_lossy()])?;
        println!("\ncard written and unmounted; safe to remove");
    } else {
        println!(
            "\ncard written and synced; still mounted at {}",
            mount.display()
        );
    }
    Ok(())
}

/// Refuses anything that is not a vfat partition on a removable disk, so a
/// typo in `--device` cannot clobber a system drive.
fn ensure_removable_vfat(device: &Path) -> Result<()> {
    let output = Command::new("lsblk")
        .args(["-ndo", "RM,FSTYPE,TYPE"])
        .arg(device)
        .output()
        .wrap_err("cannot run lsblk")?;
    if !output.status.success() {
        bail!("lsblk cannot inspect {}", device.display());
    }
    let info = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = info.split_whitespace().collect();
    match fields.as_slice() {
        ["1", "vfat", "part"] => Ok(()),
        _ => bail!(
            "{} is not a vfat partition on a removable disk (lsblk: {:?}); refusing to write",
            device.display(),
            info.trim()
        ),
    }
}

/// Returns the card's mountpoint, mounting it through udisks if needed. The
/// flag says whether this call did the mounting.
fn mount(device: &Path) -> Result<(PathBuf, bool)> {
    if let Some(target) = mountpoint(device)? {
        return Ok((target, false));
    }
    tasks::run("udisksctl", &["mount", "-b", &device.to_string_lossy()])?;
    let target = mountpoint(device)?.ok_or_else(|| {
        eyre!(
            "udisksctl reported success but {} is not mounted",
            device.display()
        )
    })?;
    Ok((target, true))
}

fn mountpoint(device: &Path) -> Result<Option<PathBuf>> {
    let output = Command::new("findmnt")
        .args(["-no", "TARGET", "--source"])
        .arg(device)
        .output()
        .wrap_err("cannot run findmnt")?;
    // findmnt exits 1 when the device is not mounted.
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from))
}

/// Replaces the build-owned parts of `anyka_rust/` and seeds the camera-owned
/// ones only where the card lacks them.
fn sync_anyka_rust(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(target)? {
        let path = entry?.path();
        if !is_preserved(&path) {
            remove(&path)?;
        }
    }
    for entry in std::fs::read_dir(source)? {
        let from = entry?.path();
        let to = target.join(from.file_name().ok_or_else(|| eyre!("unnamed entry"))?);
        if is_preserved(&from) && to.exists() {
            println!(
                "  kept   anyka_rust/{}",
                to.file_name().unwrap_or_default().to_string_lossy()
            );
            continue;
        }
        copy_tree(&from, &to)?;
    }
    Ok(())
}

fn is_preserved(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| tasks::SD_PRESERVED.contains(&name))
}

fn replace_dir(source: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        remove(target)?;
    }
    copy_tree(source, target)
}

fn remove(path: &Path) -> Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .wrap_err_with(|| format!("cannot remove {}", path.display()))
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    if source.is_dir() {
        std::fs::create_dir_all(target)?;
        for entry in std::fs::read_dir(source)? {
            let from = entry?.path();
            let name = from.file_name().ok_or_else(|| eyre!("unnamed entry"))?;
            copy_tree(&from, &target.join(name))?;
        }
        Ok(())
    } else {
        copy_file(source, target)
    }
}

/// Copies contents only. `std::fs::copy` also copies the mode, and vfat
/// rejects most chmods with EPERM; the camera runs everything through `sh`,
/// so the mode is irrelevant there anyway.
fn copy_file(source: &Path, target: &Path) -> Result<()> {
    let mut reader = std::fs::File::open(source)
        .wrap_err_with(|| format!("cannot open {}", source.display()))?;
    let mut writer = std::fs::File::create(target)
        .wrap_err_with(|| format!("cannot create {}", target.display()))?;
    std::io::copy(&mut reader, &mut writer)
        .wrap_err_with(|| format!("cannot copy {} to {}", source.display(), target.display()))?;
    Ok(())
}
