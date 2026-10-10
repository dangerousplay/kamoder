//! Bakes build identity into the binary for the boot banner (`src/banner.rs`):
//! commit, build date, compiler, target, profile and Cargo features.
//!
//! No `vergen`/`chrono`: everything here comes from `git`, `rustc -V` and the
//! environment Cargo already provides, so the build needs no extra crates.

use std::env;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Re-run when the commit or the index moves, and when this crate's
    // sources change, so the hash and date follow real rebuilds.
    let git_dir = git(&["rev-parse", "--git-dir"]);
    if let Some(dir) = &git_dir {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/index");
    }
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let commit = git(&["rev-parse", "--short=10", "HEAD"]).unwrap_or_else(|| "unknown".into());
    // Untracked files are ignored: the Rust tree itself is untracked today, so
    // counting it would mark every build dirty.
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|out| !out.is_empty());
    let commit = if dirty {
        format!("{commit}-dirty")
    } else {
        commit
    };

    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let rustc_version = Command::new(rustc)
        .arg("-V")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "unknown".into());

    // Honour reproducible-build timestamps when set.
    let epoch = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });

    let mut features: Vec<String> = env::vars()
        .filter_map(|(key, _)| key.strip_prefix("CARGO_FEATURE_").map(str::to_owned))
        .map(|f| f.to_lowercase().replace('_', "-"))
        .collect();
    features.sort();

    let set = |key: &str, value: &str| println!("cargo:rustc-env={key}={value}");
    set("ANYKA_BUILD_COMMIT", &commit);
    set("ANYKA_BUILD_DATE", &format_utc(epoch));
    set("ANYKA_BUILD_RUSTC", &rustc_version);
    set(
        "ANYKA_BUILD_TARGET",
        &env::var("TARGET").unwrap_or_default(),
    );
    set(
        "ANYKA_BUILD_PROFILE",
        &env::var("PROFILE").unwrap_or_default(),
    );
    set("ANYKA_BUILD_FEATURES", &features.join(","));
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `YYYY-MM-DD HH:MM:SS UTC` from a Unix timestamp (Howard Hinnant's
/// days-to-civil algorithm).
fn format_utc(epoch: u64) -> String {
    let days = (epoch / 86_400) as i64;
    let secs = epoch % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}
