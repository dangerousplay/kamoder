//! Choosing the profile: detection, scoring, override and fallback
//! (BRD-01, BRD-02).
//!
//! All paths are read relative to `root` so tests can point it at a fixture
//! tree; on the camera it is `/`.

use crate::profile::{load, MatchRules, Profile};
use crate::{ReadSnafu, Result, TieSnafu, UnknownBoardSnafu};
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use std::path::{Path, PathBuf};

/// The profile used when nothing matches. It has no `[match]` rules, so it
/// is never chosen by score, only by name or by falling back.
pub const FALLBACK: &str = "generic-v4l2";

/// What the kernel says about the board.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detected {
    /// `Hardware` line of `/proc/cpuinfo` (ARM kernels without a device
    /// tree, like the AK3918's 3.4).
    pub hardware: Option<String>,
    /// `/proc/device-tree/compatible`, split at the NULs.
    pub compatible: Vec<String>,
    /// `/proc/device-tree/model`.
    pub model: Option<String>,
}

/// Reads [`Detected`] from `root`. Missing files leave fields empty.
pub fn detect(root: &Path) -> Detected {
    let read = |rel: &str| std::fs::read_to_string(root.join(rel)).ok();
    let hardware = read("proc/cpuinfo").and_then(|cpuinfo| {
        cpuinfo.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            (k.trim() == "Hardware").then(|| v.trim().to_owned())
        })
    });
    let compatible = read("proc/device-tree/compatible")
        .map(|text| {
            text.split('\0')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let model = read("proc/device-tree/model")
        .map(|m| m.trim_end_matches('\0').trim().to_owned())
        .filter(|m| !m.is_empty());
    Detected {
        hardware: hardware.filter(|h| !h.is_empty()),
        compatible,
        model,
    }
}

/// The chosen profile and why it was chosen. This is also the handoff file
/// format, so field names are a contract between supervisor and services.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolved {
    pub name: String,
    /// The profile file it was loaded from.
    pub path: PathBuf,
    /// Human-readable, shown in logs and `/api/hardware`.
    pub reason: String,
    pub profile: Profile,
}

/// Picks the profile for this board from the `*.toml` files in `dirs`
/// (earlier directories shadow later ones by name).
///
/// - `override_name` set: that profile, or [`crate::Error::UnknownBoard`].
/// - otherwise the profile whose rules all hold with the highest score; a
///   tie at the top is [`crate::Error::Tie`].
/// - nothing matched: [`FALLBACK`], with the SoC in `reason`.
///
/// Any invalid profile in `dirs` is an error, even one that would not have
/// matched: a broken file should fail loudly on every board, not only on
/// the one it was written for.
pub fn resolve(dirs: &[PathBuf], override_name: Option<&str>, root: &Path) -> Result<Resolved> {
    if let Some(name) = override_name {
        let (path, profile) = find(dirs, name)?;
        tracing::info!(board = name, path = %path.display(), "board chosen by override");
        return Ok(Resolved {
            name: profile.name.clone(),
            path,
            reason: format!("BOARD={name} override"),
            profile,
        });
    }

    let detected = detect(root);
    let mut best: Vec<(usize, PathBuf, Profile)> = Vec::new();
    for (path, profile) in scan(dirs)? {
        let Some(score) = score(&profile.match_rules, &detected, root) else {
            continue;
        };
        match best.first().map(|(s, ..)| *s) {
            Some(top) if score < top => {}
            Some(top) if score == top => best.push((score, path, profile)),
            _ => best = vec![(score, path, profile)],
        }
    }

    if best.len() > 1 {
        let score = best[0].0;
        let names: Vec<_> = best.into_iter().map(|(_, _, p)| p.name).collect();
        return TieSnafu { names, score }.fail();
    }
    if let Some((score, path, profile)) = best.pop() {
        let reason = format!("matched {} (score {score})", describe(&profile.match_rules));
        tracing::info!(board = %profile.name, path = %path.display(), %reason, "board detected");
        return Ok(Resolved {
            name: profile.name.clone(),
            path,
            reason,
            profile,
        });
    }

    let soc = detected
        .hardware
        .or(detected.model)
        .or_else(|| detected.compatible.first().cloned())
        .unwrap_or_else(|| "unknown".to_owned());
    let reason = format!("no profile matched (SoC: {soc})");
    let (path, profile) = find(dirs, FALLBACK)?;
    tracing::warn!(board = FALLBACK, %reason, "falling back to the generic board profile");
    Ok(Resolved {
        name: profile.name.clone(),
        path,
        reason,
        profile,
    })
}

/// Loads `<name>.toml` from the first directory that has it.
fn find(dirs: &[PathBuf], name: &str) -> Result<(PathBuf, Profile)> {
    let file = format!("{name}.toml");
    // A name with a separator would escape the profile directories.
    let plain = !name.is_empty() && Path::new(&file).file_name() == Some(file.as_ref());
    if let Some(path) = dirs
        .iter()
        .map(|dir| dir.join(&file))
        .find(|path| plain && path.is_file())
    {
        let profile = load(&path)?;
        return Ok((path, profile));
    }
    let dirs = dirs
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    UnknownBoardSnafu { name, dirs }.fail()
}

/// Every profile in `dirs`, first directory winning on a duplicate name.
/// A directory that does not exist is skipped.
fn scan(dirs: &[PathBuf]) -> Result<Vec<(PathBuf, Profile)>> {
    let mut out: Vec<(PathBuf, Profile)> = Vec::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(source).context(ReadSnafu { path: dir }),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let path = entry.context(ReadSnafu { path: dir })?.path();
            if path.extension().is_some_and(|e| e == "toml") {
                paths.push(path);
            }
        }
        // read_dir order is unspecified; sort so ties report stably.
        paths.sort();
        for path in paths {
            let profile = load(&path)?;
            if out.iter().all(|(_, p)| p.name != profile.name) {
                out.push((path, profile));
            }
        }
    }
    Ok(out)
}

/// The rule count when every rule holds, else `None`. A profile without
/// rules never scores: it would match every board.
fn score(rules: &MatchRules, detected: &Detected, root: &Path) -> Option<usize> {
    let count = rules.count();
    if count == 0 {
        return None;
    }
    let hardware_ok = rules
        .cpuinfo_hardware
        .as_ref()
        .is_none_or(|want| detected.hardware.as_ref() == Some(want));
    let compatible_ok = rules.dt_compatible.as_ref().is_none_or(|want| {
        detected
            .compatible
            .iter()
            .any(|c| c.contains(want.as_str()))
    });
    let model_ok = rules.dt_model.as_ref().is_none_or(|want| {
        detected
            .model
            .as_ref()
            .is_some_and(|m| m.contains(want.as_str()))
    });
    let paths_ok = rules.path_exists.iter().all(|p| {
        // Profile paths are absolute; re-anchor them under `root`.
        let rel = p.strip_prefix("/").unwrap_or(p);
        root.join(rel).exists()
    });
    (hardware_ok && compatible_ok && model_ok && paths_ok).then_some(count)
}

/// `cpuinfo_hardware=AK3918AV100, path_exists=/dev/x`, for the reason.
fn describe(rules: &MatchRules) -> String {
    let mut parts = Vec::new();
    if let Some(v) = &rules.cpuinfo_hardware {
        parts.push(format!("cpuinfo_hardware={v}"));
    }
    if let Some(v) = &rules.dt_compatible {
        parts.push(format!("dt_compatible={v}"));
    }
    if let Some(v) = &rules.dt_model {
        parts.push(format!("dt_model={v}"));
    }
    for p in &rules.path_exists {
        parts.push(format!("path_exists={}", p.display()));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use crate::Error;

    /// A minimal valid profile with the given `[match]` body.
    fn profile(name: &str, rules: &str) -> String {
        format!(
            "name = \"{name}\"\n[match]\n{rules}\n[video]\nbackend = \"test-pattern\"\n\
             sensors = [0]\n[video.stream]\nwidth = 640\nheight = 360\nfps = 15\n\
             kbps = 1024\ngop = 30\nprofile = \"main\"\n"
        )
    }

    struct Fixture {
        dir: TempDir,
    }

    impl Fixture {
        /// A profile directory with the fallback, and an empty fake root.
        fn new(tag: &str) -> Self {
            let dir = TempDir::new(tag);
            std::fs::create_dir_all(dir.path().join("boards")).unwrap();
            std::fs::create_dir_all(dir.path().join("root")).unwrap();
            let f = Self { dir };
            f.board(FALLBACK, "");
            f
        }

        fn board(&self, name: &str, rules: &str) {
            let path = self.boards().join(format!("{name}.toml"));
            std::fs::write(path, profile(name, rules)).unwrap();
        }

        fn file(&self, rel: &str, body: &str) {
            let path = self.root().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }

        fn boards(&self) -> PathBuf {
            self.dir.path().join("boards")
        }

        fn root(&self) -> PathBuf {
            self.dir.path().join("root")
        }

        fn resolve(&self, override_name: Option<&str>) -> Result<Resolved> {
            resolve(&[self.boards()], override_name, &self.root())
        }
    }

    const AV100_CPUINFO: &str = "processor\t: 0\nmodel name\t: ARM926EJ-S rev 5 (v5l)\n\
                                 BogoMIPS\t: 441.34\nHardware\t: AK3918AV100\nRevision\t: 0000\n";

    #[test]
    fn detect_reads_cpuinfo_and_device_tree() {
        let f = Fixture::new("detect");
        f.file("proc/cpuinfo", AV100_CPUINFO);
        f.file("proc/device-tree/compatible", "acme,cam-v2\0acme,soc\0");
        f.file("proc/device-tree/model", "Acme Cam v2\0");
        let d = detect(&f.root());
        assert_eq!(d.hardware.as_deref(), Some("AK3918AV100"));
        assert_eq!(d.compatible, ["acme,cam-v2", "acme,soc"]);
        assert_eq!(d.model.as_deref(), Some("Acme Cam v2"));
        assert_eq!(detect(&f.root().join("missing")), Detected::default());
    }

    #[test]
    fn av100_root_matches_by_cpuinfo() {
        let f = Fixture::new("av100");
        f.board("av100", "cpuinfo_hardware = \"AK3918AV100\"");
        f.board("other", "cpuinfo_hardware = \"AK3918EV300\"");
        f.file("proc/cpuinfo", AV100_CPUINFO);
        let r = f.resolve(None).unwrap();
        assert_eq!(r.name, "av100");
        assert_eq!(r.path, f.boards().join("av100.toml"));
        assert_eq!(r.reason, "matched cpuinfo_hardware=AK3918AV100 (score 1)");
    }

    #[test]
    fn more_rules_win_and_all_must_hold() {
        let f = Fixture::new("score");
        f.file("proc/cpuinfo", AV100_CPUINFO);
        f.file("proc/device-tree/compatible", "acme,cam-v2\0");
        f.file("dev/preset_motor", "");
        f.board("soc", "cpuinfo_hardware = \"AK3918AV100\"");
        f.board(
            "ptz",
            "cpuinfo_hardware = \"AK3918AV100\"\npath_exists = [\"/dev/preset_motor\"]",
        );
        // Three rules, but one fails: not a candidate.
        f.board(
            "wrong",
            "cpuinfo_hardware = \"AK3918AV100\"\ndt_compatible = \"cam-v2\"\ndt_model = \"Nope\"",
        );
        assert_eq!(f.resolve(None).unwrap().name, "ptz");
    }

    #[test]
    fn unknown_soc_falls_back_with_reason() {
        let f = Fixture::new("fallback");
        f.board("av100", "cpuinfo_hardware = \"AK3918AV100\"");
        f.file("proc/cpuinfo", "Hardware\t: SOMETHING9000\n");
        let r = f.resolve(None).unwrap();
        assert_eq!(r.name, FALLBACK);
        assert_eq!(r.reason, "no profile matched (SoC: SOMETHING9000)");
    }

    #[test]
    fn missing_fallback_is_unknown_board() {
        let f = Fixture::new("nofallback");
        std::fs::remove_file(f.boards().join(format!("{FALLBACK}.toml"))).unwrap();
        let err = f.resolve(None).unwrap_err();
        assert!(matches!(err, Error::UnknownBoard { ref name, .. } if name == FALLBACK));
    }

    #[test]
    fn override_picks_named_profile_even_if_it_does_not_match() {
        let f = Fixture::new("override");
        f.board("av100", "cpuinfo_hardware = \"AK3918AV100\"");
        let r = f.resolve(Some("av100")).unwrap();
        assert_eq!(r.name, "av100");
        assert_eq!(r.reason, "BOARD=av100 override");
    }

    #[test]
    fn missing_override_is_an_error_not_a_fallback() {
        let f = Fixture::new("override-missing");
        for name in ["nope", "../boards/generic-v4l2", ""] {
            let err = f.resolve(Some(name)).unwrap_err();
            assert!(matches!(err, Error::UnknownBoard { .. }), "{name}: {err:?}");
        }
    }

    #[test]
    fn equal_scores_are_a_tie() {
        let f = Fixture::new("tie");
        f.file("proc/cpuinfo", AV100_CPUINFO);
        f.board("a", "cpuinfo_hardware = \"AK3918AV100\"");
        f.board("b", "cpuinfo_hardware = \"AK3918AV100\"");
        match f.resolve(None) {
            Err(Error::Tie { names, score }) => {
                assert_eq!(names, ["a", "b"]);
                assert_eq!(score, 1);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn invalid_profile_anywhere_fails_resolution() {
        let f = Fixture::new("broken");
        std::fs::write(f.boards().join("broken.toml"), "name = \"broken\"\n").unwrap();
        assert!(matches!(f.resolve(None), Err(Error::Parse { .. })));
    }

    /// The shipped `boards/` directory at the repository root.
    fn shipped_boards() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../boards")
    }

    #[test]
    fn every_shipped_profile_loads_and_validates() {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(shipped_boards()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "toml") {
                let p = load(&path).unwrap_or_else(|e| panic!("{e}"));
                names.push(p.name);
            }
        }
        assert!(names.iter().any(|n| n == FALLBACK), "{names:?}");
        assert!(names.iter().any(|n| n == "anyka-av100-ptz"), "{names:?}");
    }

    #[test]
    fn av100_fixture_resolves_to_shipped_av100_profile() {
        let f = Fixture::new("shipped-av100");
        f.file("proc/cpuinfo", AV100_CPUINFO);
        let r = resolve(&[shipped_boards()], None, &f.root()).unwrap();
        assert_eq!(r.name, "anyka-av100-ptz");
        let p = &r.profile;
        assert_eq!(p.video.backend, "anyka-av100");
        assert_eq!(p.video.sensors, [0, 1]);
        assert_eq!(p.wifi.as_ref().unwrap().power_gpio, Some(98));

        // And an unknown SoC gets the generic profile.
        f.file("proc/cpuinfo", "Hardware\t: Unknown\n");
        let r = resolve(&[shipped_boards()], None, &f.root()).unwrap();
        assert_eq!(r.name, FALLBACK);
        assert_eq!(r.profile.video.backend, "test-pattern");
        assert!(r.profile.lights.is_none() && r.profile.wifi.is_none() && r.profile.ptz.is_none());
    }
}
