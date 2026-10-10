//! Library-hygiene guards: source-scan regression tests so the classes of
//! problems fixed in v0.3.0 cannot silently reappear. Each guard inspects
//! only the production (pre-`#[cfg(test)]`) portion of each source file.

use std::fs;
use std::path::PathBuf;

fn production_sources() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).expect("read src dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let content = fs::read_to_string(&path).expect("read source");
        let prod = match content.find("#[cfg(test)]") {
            Some(idx) => content[..idx].to_string(),
            None => content,
        };
        out.push((
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_string(),
            prod,
        ));
    }
    out
}

/// No direct stdout/stderr printing in library code — the `log` facade only.
#[test]
fn no_direct_print_macros_in_library_code() {
    for (name, src) in production_sources() {
        assert!(
            !src.contains("println!(") && !src.contains("eprintln!("),
            "{name} uses println!/eprintln! — use the log facade instead"
        );
    }
}

/// No origin-hardware branding hardcoded into library code outside the
/// documented DeviceConfig defaults (`config.rs` describes the origin
/// hardware as its serde default and is the deliberate exception).
#[test]
fn no_origin_hardware_branding_in_library_code() {
    const BANNED: &[&str] = &["PiCameraV1", "\"Pi Camera\"", "OV5647"];
    for (name, src) in production_sources() {
        if name == "config.rs" {
            continue; // documented origin-hardware serde defaults
        }
        for banned in BANNED {
            assert!(
                !src.contains(banned),
                "{name} contains hardcoded origin-hardware string {banned:?} — take it from configuration"
            );
        }
    }
}

/// No lab/example IPs in library code — endpoints come from configuration.
#[test]
fn no_private_lab_ips_in_library_code() {
    for (name, src) in production_sources() {
        assert!(
            !src.contains("192.168."),
            "{name} contains a 192.168.x address — take it from configuration"
        );
    }
}

/// Production paths must be free of panic-capable unwrap/expect (#15):
/// every request-facing code path answers errors as faults, never panics.
/// Serializer writes to the in-memory quick-xml buffer use
/// `.unwrap_or_default()` (provably infallible Vec writes — the release
/// behavior discards nothing real and never panics), and lock acquisition
/// goes through the poison-tolerant helpers in `ptz_state.rs`.
#[test]
fn production_code_has_no_unwrap_or_expect() {
    let mut violations = Vec::new();
    for (name, prod) in production_sources() {
        // Strip comments first — doc examples may legitimately unwrap.
        let no_comments = strip_comments(&prod);
        for (i, line) in no_comments.lines().enumerate() {
            let t = line.trim();
            if t.contains(".unwrap()") || t.contains(".expect(") {
                violations.push(format!("{name}:{}: {}", i + 1, t));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "production unwrap/expect found (see #15):\n{}",
        violations.join("\n")
    );
}

/// Removes // line comments (naive but sufficient — the sources keep
/// strings on their own lines and contain no // inside string literals
/// on unwrap-bearing lines).
fn strip_comments(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}
