//! Boot banner written to the serial console when the supervisor starts, so
//! anyone watching the UART can see the camera is running Kamoder, exactly
//! which build, and on what hardware. Build identity comes from `build.rs`,
//! hardware from [`crate::hardware`].

use crate::config::Config;
use std::path::Path;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const COMMIT: &str = env!("ANYKA_BUILD_COMMIT");
pub const DATE: &str = env!("ANYKA_BUILD_DATE");
pub const RUSTC: &str = env!("ANYKA_BUILD_RUSTC");
pub const TARGET: &str = env!("ANYKA_BUILD_TARGET");
pub const PROFILE: &str = env!("ANYKA_BUILD_PROFILE");
pub const FEATURES: &str = env!("ANYKA_BUILD_FEATURES");

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
/// Rust orange (xterm-256 colour 208).
const RUST: &str = "\x1b[38;5;208m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";

/// Minimum inner width of the title box, in characters (the art's width).
const MIN_WIDTH: usize = 66;

/// Project name. Vendor-neutral on purpose: the AK3918 is the first target,
/// not the only one.
pub const PROJECT: &str = "Kamoder";
pub const TAGLINE: &str = "Open camera firmware in Rust, for any vendor";

/// A camera, drawn beside the wordmark; each row is padded to the same width.
const CAMERA: [&str; 5] = [
    r" ___________     ",
    r"|  .-----.  |__  ",
    r"|  | (O) |  |  |>",
    r"|  '-----'  |__| ",
    r"|___________|    ",
];

const LOGO: [&str; 5] = [
    r" _                                   _",
    r"| | __  __ _  _ __ ___    ___    __| |  ___  _ __",
    r"| |/ / / _` || '_ ` _ \  / _ \  / _` | / _ \| '__|",
    r"|   < | (_| || | | | | || (_) || (_| ||  __/| |",
    r"|_|\_\ \__,_||_| |_| |_| \___/  \__,_| \___||_|",
];

/// Renders the banner. Lines end in `\r\n` so it lines up on a raw serial
/// console as well as a cooked tty.
pub fn render(config: &Config, config_path: &Path, start: &crate::ready::StartTime) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push_str(RESET);
        out.push_str("\r\n");
    };

    line(String::new());
    for (camera, word) in CAMERA.iter().zip(LOGO) {
        let camera = camera.replace("(O)", &format!("({RED}{BOLD}O{RESET}{BOLD})"));
        line(format!("  {BOLD}{camera}{RESET}  {RUST}{BOLD}{word}"));
    }

    let title = format!("{PROJECT} v{VERSION} :: {TAGLINE}");
    let status = "Supervisor up: services below are Rust binaries";
    // Wide enough for the longest line, so a longer version never breaks it.
    let width = [title.as_str(), status]
        .iter()
        .map(|text| text.chars().count() + 2)
        .max()
        .unwrap_or(0)
        .max(MIN_WIDTH);
    let rule = "═".repeat(width);
    let boxed = |text: &str, style: &str| {
        let pad = width - (text.chars().count() + 2);
        format!(
            "  {RUST}║{RESET} {style}{text}{RESET}{} {RUST}║",
            " ".repeat(pad)
        )
    };
    line(format!("  {RUST}╔{rule}╗"));
    line(boxed(&title, BOLD));
    line(boxed(status, GREEN));
    line(format!("  {RUST}╚{rule}╝"));

    let field = |name: &str, value: &str| format!("    {CYAN}{BOLD}{name:<10}{RESET} {value}");
    let features = if FEATURES.is_empty() {
        "default (none)"
    } else {
        FEATURES
    };
    line(format!("  {RUST}{BOLD}Build"));
    line(field(
        "Version",
        &format!("{BOLD}{PROJECT} v{VERSION}{RESET}"),
    ));
    line(field("Built", &format!("{DATE} {DIM}({PROFILE}){RESET}")));
    line(field("Commit", &format!("{YELLOW}{COMMIT}")));
    line(field("Rust", RUSTC));
    line(field("Target", TARGET));
    line(field("Features", features));
    line(field("Services", &services(config)));
    line(String::new());
    line(format!("  {RUST}{BOLD}Hardware"));
    for (label, value) in crate::hardware::detect(Path::new("/")) {
        line(field(label, &value));
    }
    line(field("Kernel", &system_line()));
    for (label, value) in start.banner_rows() {
        line(field(label, &value));
    }
    line(field("Config", &format!("{DIM}{}", config_path.display())));
    line(String::new());
    out
}

/// `● name` (green) for enabled services, `○ name` (dim) for disabled ones.
fn services(config: &Config) -> String {
    config
        .services
        .iter()
        .map(|s| {
            if s.enabled {
                format!("{GREEN}●{RESET} {}", s.name)
            } else {
                format!("{DIM}○ {}{RESET}", s.name)
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn system_line() -> String {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let release = read("/proc/sys/kernel/osrelease");
    let uptime = read("/proc/uptime");
    let uptime = uptime.split_whitespace().next().unwrap_or("?");
    format!("Linux {}  {DIM}up {uptime} s{RESET}", release.trim())
}
