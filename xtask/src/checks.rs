//! Source checks that need no cross toolchain: `./x check-boards` and
//! `./x check-literals`. Both run in `./x ci` (BRD-04, PKG-04) and are cheap
//! enough for any CI runner.

use crate::tasks::workspace_root;
use eyre::{bail, Result, WrapErr};
use std::path::{Path, PathBuf};

/// Every `boards/*.toml`, sorted.
fn board_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in
        std::fs::read_dir(dir).wrap_err_with(|| format!("cannot read {}", dir.display()))?
    {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "toml") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Loads and validates every shipped profile with the `board` crate the
/// services use, then makes sure detection can tell them apart: no two
/// profiles have the same `[match]` rules, and on a fixture board that
/// satisfies exactly one profile's rules, `board::resolve` picks that profile
/// (a tie or a shadowing profile fails). The fallback profile must exist.
pub fn check_boards(_extra: &[String]) -> Result<()> {
    let dir = workspace_root().join("boards");
    let files = board_files(&dir)?;
    if files.is_empty() {
        bail!("no board profiles in {}", dir.display());
    }

    let mut problems = Vec::new();
    let mut profiles = Vec::new();
    for path in &files {
        match board::load(path) {
            Ok(profile) => profiles.push(profile),
            Err(e) => problems.push(e.to_string()),
        }
    }
    if !problems.is_empty() {
        bail!("invalid board profiles:\n  {}", problems.join("\n  "));
    }

    if !profiles.iter().any(|p| p.name == board::FALLBACK) {
        problems.push(format!(
            "the fallback profile {}.toml is missing",
            board::FALLBACK
        ));
    }
    for (i, a) in profiles.iter().enumerate() {
        for b in &profiles[i + 1..] {
            if a.match_rules.count() > 0 && same_rules(&a.match_rules, &b.match_rules) {
                problems.push(format!(
                    "{} and {} have identical [match] rules; detection would tie",
                    a.name, b.name
                ));
            }
        }
    }

    // Detection on a synthetic board per profile.
    let scratch = workspace_root().join("target/check-boards");
    for profile in profiles.iter().filter(|p| p.match_rules.count() > 0) {
        let root = scratch.join(&profile.name);
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
        }
        fixture_root(&root, &profile.match_rules)?;
        match board::resolve(std::slice::from_ref(&dir), None, &root) {
            Ok(resolved) if resolved.name == profile.name => {}
            Ok(resolved) => problems.push(format!(
                "a board matching only {}'s rules resolves to {} ({})",
                profile.name, resolved.name, resolved.reason
            )),
            Err(e) => problems.push(format!(
                "a board matching only {}'s rules: {e}",
                profile.name
            )),
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);

    if !problems.is_empty() {
        bail!("board profile check failed:\n  {}", problems.join("\n  "));
    }
    println!("board profiles ok:");
    for (path, profile) in files.iter().zip(&profiles) {
        let rules = profile.match_rules.count();
        println!(
            "  {:<24} {rules} match rule(s)  {}",
            profile.name,
            path.strip_prefix(workspace_root())
                .unwrap_or(path)
                .display()
        );
    }
    Ok(())
}

fn same_rules(a: &board::MatchRules, b: &board::MatchRules) -> bool {
    let mut pa = a.path_exists.clone();
    let mut pb = b.path_exists.clone();
    pa.sort();
    pb.sort();
    a.cpuinfo_hardware == b.cpuinfo_hardware
        && a.dt_compatible == b.dt_compatible
        && a.dt_model == b.dt_model
        && pa == pb
}

/// A `/`-like tree where exactly `rules` hold.
fn fixture_root(root: &Path, rules: &board::MatchRules) -> Result<()> {
    let proc = root.join("proc");
    std::fs::create_dir_all(proc.join("device-tree"))?;
    if let Some(hw) = &rules.cpuinfo_hardware {
        std::fs::write(
            proc.join("cpuinfo"),
            format!("Processor\t: ARM926EJ-S\nHardware\t: {hw}\n"),
        )?;
    }
    if let Some(compatible) = &rules.dt_compatible {
        std::fs::write(
            proc.join("device-tree/compatible"),
            format!("{compatible}\0"),
        )?;
    }
    if let Some(model) = &rules.dt_model {
        std::fs::write(proc.join("device-tree/model"), format!("{model}\0"))?;
    }
    for path in &rules.path_exists {
        let path = root.join(path.strip_prefix("/").unwrap_or(path));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, "")?;
    }
    Ok(())
}

/// Crates that must work on every board: hardware facts come from the board
/// profile, not from literals in their code (PKG-04).
const GENERIC_CRATES: &[&str] = &[
    "rtsp",
    "web",
    "onvif",
    "supervisor",
    "lights",
    "lights-proto",
    "wifi",
    "camera",
    "video-proto",
    "board",
    "auth",
    "sshd",
    "telemetry",
    "os",
    "ptz-proto",
];

/// Matched case-insensitively: board and SoC names.
const BOARD_NAMES: &[&str] = &["av100", "ev200", "ev300", "ak3918"];
/// Matched exactly: interfaces, device nodes, drivers.
const HARDWARE_LITERALS: &[&str] = &[
    "wlan0",
    "/dev/venc",
    "/dev/preset_motor",
    "/dev/ak-motor",
    "/sys/user-gpio",
    "ssv6x5x",
    "presetmotor.ko",
];
/// Lines carrying this marker are deliberate (say why after a colon).
const ALLOW_MARKER: &str = "board-literal-ok";

/// Fails on board literals in the non-test code of [`GENERIC_CRATES`].
///
/// Skipped: comment lines, `tests/` and `benches/`, `#[cfg(test)]` items and
/// modules (inline, or a `mod x;` file), and lines marked
/// `// board-literal-ok: <reason>`.
pub fn check_literals(_extra: &[String]) -> Result<()> {
    let root = workspace_root();
    let mut hits = Vec::new();
    let mut allowed = 0;
    for name in GENERIC_CRATES {
        let dir = root.join("crates").join(name);
        if !dir.is_dir() {
            bail!("generic crate {} does not exist", dir.display());
        }
        let mut files = Vec::new();
        rust_files(&dir, &mut files)?;
        files.sort();
        let test_only = test_only_files(&files)?;
        for file in files.iter().filter(|f| !test_only.contains(*f)) {
            let text = std::fs::read_to_string(file)
                .wrap_err_with(|| format!("cannot read {}", file.display()))?;
            for (line_no, line) in production_lines(&text) {
                let Some(literal) = board_literal(line) else {
                    continue;
                };
                if line.contains(ALLOW_MARKER) {
                    allowed += 1;
                    continue;
                }
                hits.push(format!(
                    "  {}:{line_no}: {literal:?}: {}",
                    file.strip_prefix(&root).unwrap_or(file).display(),
                    line.trim()
                ));
            }
        }
    }
    if !hits.is_empty() {
        bail!(
            "board literals in generic crates (move them to boards/*.toml, or mark a \
             deliberate fallback with `// {ALLOW_MARKER}: <reason>`):\n{}",
            hits.join("\n")
        );
    }
    println!(
        "no board literals in {} generic crates ({allowed} marked {ALLOW_MARKER})",
        GENERIC_CRATES.len()
    );
    Ok(())
}

/// `.rs` files under `dir`, without `tests/`, `benches/` and `target/`.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            let name = entry.file_name();
            if !matches!(name.to_str(), Some("tests" | "benches" | "target")) {
                rust_files(&path, out)?;
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn is_test_cfg(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("#[cfg(test)]")
        || line.starts_with("#[cfg(any(test,")
        || line.starts_with("#[cfg(any(test ,")
}

/// Files declared as `#[cfg(test)] mod x;` somewhere among `files`.
fn test_only_files(files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(file)?;
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !is_test_cfg(line) {
                continue;
            }
            let Some(item) = lines[i + 1..]
                .iter()
                .map(|l| l.trim())
                .find(|l| !l.is_empty() && !l.starts_with("#[") && !l.starts_with("//"))
            else {
                continue;
            };
            let Some(name) = module_decl(item)
                .filter(|(_, inline)| !inline)
                .map(|(n, _)| n)
            else {
                continue;
            };
            let dir = file.parent().expect("a file has a directory");
            // `mod x;` in lib.rs/main.rs/mod.rs is a sibling; elsewhere it
            // is in the directory named after the declaring file.
            let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let base = if matches!(stem, "lib" | "main" | "mod") {
                dir.to_path_buf()
            } else {
                dir.join(stem)
            };
            out.push(base.join(format!("{name}.rs")));
            out.push(base.join(name).join("mod.rs"));
        }
    }
    Ok(out)
}

/// `mod x;` / `pub(crate) mod x {` → (`x`, inline?).
fn module_decl(item: &str) -> Option<(&str, bool)> {
    let rest = item
        .strip_prefix("pub ")
        .or_else(|| item.strip_prefix("pub(crate) "))
        .or_else(|| item.strip_prefix("pub(super) "))
        .unwrap_or(item);
    let rest = rest.strip_prefix("mod ")?;
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    let (name, tail) = rest.split_at(end);
    let tail = tail.trim_start();
    if tail.starts_with(';') {
        Some((name, false))
    } else if tail.starts_with('{') {
        Some((name, true))
    } else {
        None
    }
}

/// Non-comment lines outside `#[cfg(test)]` items, numbered from 1. A test
/// module ends the file (they come last by convention); any other test-only
/// item is skipped up to its closing brace or semicolon.
fn production_lines(text: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_test_cfg(line) {
            // Skip further attributes, then the item.
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().starts_with("#[") {
                j += 1;
            }
            if j < lines.len() {
                if let Some((_, true)) = module_decl(lines[j].trim()) {
                    break;
                }
            }
            let mut depth = 0i32;
            let mut opened = false;
            while j < lines.len() {
                for c in lines[j].chars() {
                    match c {
                        '{' => {
                            depth += 1;
                            opened = true;
                        }
                        '}' => depth -= 1,
                        _ => {}
                    }
                }
                let ends =
                    (opened && depth <= 0) || (!opened && lines[j].trim_end().ends_with(';'));
                j += 1;
                if ends {
                    break;
                }
            }
            i = j;
            continue;
        }
        let trimmed = line.trim_start();
        if !trimmed.starts_with("//") && !trimmed.is_empty() {
            out.push((i + 1, line));
        }
        i += 1;
    }
    out
}

/// The first board literal in the code part of `line`, if any.
fn board_literal(line: &str) -> Option<String> {
    // Ignore a trailing `// comment` (a `//` inside a string such as a URL
    // is rare in these crates and only makes the check stricter).
    let code = match line.find("//") {
        // An even number of quotes before it: the `//` is not in a string.
        Some(at) if line[..at].matches('"').count().is_multiple_of(2) => &line[..at],
        _ => line,
    };
    let lower = code.to_ascii_lowercase();
    if let Some(name) = BOARD_NAMES.iter().find(|n| lower.contains(*n)) {
        return Some((*name).to_owned());
    }
    if let Some(lit) = HARDWARE_LITERALS.iter().find(|l| code.contains(*l)) {
        return Some((*lit).to_owned());
    }
    gpio_number(&lower)
}

/// `gpio = 19`, `gpio: 98`, `power_gpio=98` …: a GPIO number in code.
fn gpio_number(lower: &str) -> Option<String> {
    let mut search = lower;
    while let Some(at) = search.find("gpio") {
        let rest = search[at + 4..].trim_start();
        if let Some(rest) = rest.strip_prefix([':', '=']) {
            let rest = rest.trim_start();
            if rest.starts_with(|c: char| c.is_ascii_digit()) {
                let number: String = rest.chars().take_while(char::is_ascii_digit).collect();
                return Some(format!("gpio {number}"));
            }
        }
        search = &search[at + 4..];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_literals() {
        assert_eq!(
            board_literal(r#"let x = "wlan0";"#).as_deref(),
            Some("wlan0")
        );
        assert_eq!(
            board_literal("hw == \"AK3918AV100\"").as_deref(),
            Some("av100")
        );
        assert_eq!(board_literal("power_gpio: 98,").as_deref(), Some("gpio 98"));
        assert_eq!(board_literal("let a = 1; // wlan0 by default"), None);
        assert_eq!(board_literal("let gpio = line.gpio;"), None);
    }

    #[test]
    fn skips_tests_and_comments() {
        let text = "fn a() {}\n// wlan0\n#[cfg(test)]\nfn helper() {\n  \"wlan0\";\n}\nfn b() {}\n#[cfg(test)]\nmod tests {\n  \"wlan0\";\n}\n";
        let lines: Vec<usize> = production_lines(text).into_iter().map(|(n, _)| n).collect();
        assert_eq!(lines, vec![1, 7]);
    }

    #[test]
    fn module_declarations() {
        assert_eq!(module_decl("mod fake;"), Some(("fake", false)));
        assert_eq!(module_decl("pub mod tests {"), Some(("tests", true)));
        assert_eq!(module_decl("fn x() {"), None);
    }
}
