//! `camera.conf` over the API (T14, CFG-01/02).
//!
//! The file is shell: `Factory/config.sh` and `init.sh` source it with
//! BusyBox sh, and `wifi` parses it (every 10 s) with
//! [`wifi::settings::parse`], which is also what reads it here. So a
//! write has to satisfy both:
//!
//! - only **known keys** are accepted, each with a type ([`KEYS`]);
//! - values are written bare when they are made of harmless characters and
//!   in single quotes otherwise. Nothing is special inside single quotes in
//!   sh, and `wifi` takes a quoted value literally, so the only values
//!   that cannot be written are those containing a single quote or a line
//!   break — they are refused;
//! - only the lines of **changed** keys are rewritten; comments, unknown keys
//!   and order are kept, missing keys are appended;
//! - the file is replaced atomically (`auth::atomic`, mode 0600) — it
//!   holds the Wi-Fi and SSH passwords.
//!
//! Secrets (`*_password`) are write-only: `GET` reports only whether each is
//! set. The ONVIF account lives in `etc/onvif.secret`, not here; its user
//! name and whether it is set are folded into `GET` for the settings page,
//! and it is changed through `PUT /api/onvif/credentials`.

use crate::auth::{AdminUser, AuthUser};
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/config", get(read).put(update))
}

/// What a key holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `0` or `1`.
    Flag,
    /// A tracing filter: a level or a directive list.
    LogLevel,
    /// `''` or `textfile:<dir>`.
    Metrics,
    /// Up to 32 bytes; empty keeps the current network.
    Ssid,
    /// Empty, or 8-63 characters.
    WifiPassword,
    /// A Unix user name.
    User,
    /// Any printable text.
    Password,
}

/// One known `camera.conf` key.
#[derive(Debug, Clone, Copy)]
struct Key {
    name: &'static str,
    kind: Kind,
    /// Never returned; `secrets_set` says whether it has a value.
    secret: bool,
    /// Accepted by `PUT`. The others are consumed once by `init.sh` and
    /// changed elsewhere (web users, `/api/onvif/credentials`).
    writable: bool,
    /// Takes effect only when the stack restarts (reboot).
    needs_reboot: bool,
}

const fn key(name: &'static str, kind: Kind, needs_reboot: bool) -> Key {
    Key {
        name,
        kind,
        secret: false,
        writable: true,
        needs_reboot,
    }
}

/// Every key the API knows, in `camera.conf.example` order.
const KEYS: &[Key] = &[
    key("run_rust", Kind::Flag, true),
    key("start_telnetd", Kind::Flag, true),
    key("stop_vendor_ipc", Kind::Flag, true),
    key("log_level", Kind::LogLevel, true),
    key("metrics", Kind::Metrics, true),
    // wifi re-reads these every 10 s.
    key("wifi_ssid", Kind::Ssid, false),
    Key {
        secret: true,
        ..key("wifi_password", Kind::WifiPassword, false)
    },
    key("ssh_user", Kind::User, true),
    Key {
        secret: true,
        ..key("ssh_password", Kind::Password, true)
    },
    Key {
        secret: true,
        writable: false,
        ..key("web_admin_password", Kind::Password, true)
    },
    Key {
        writable: false,
        ..key("onvif_user", Kind::User, true)
    },
    Key {
        secret: true,
        writable: false,
        ..key("onvif_password", Kind::Password, true)
    },
];

fn known(name: &str) -> Option<&'static Key> {
    KEYS.iter().find(|key| key.name == name)
}

/// `api.ts` `ConfigResponse`.
#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    pub values: Map<String, Value>,
    pub secrets_set: Vec<&'static str>,
}

/// `api.ts` `ConfigUpdate`.
#[derive(Debug, Deserialize)]
pub struct ConfigUpdate {
    pub values: BTreeMap<String, Value>,
}

/// `api.ts` `ConfigUpdateResult`.
#[derive(Debug, Serialize)]
pub struct ConfigUpdateResult {
    /// Keys whose value changed (and whose lines were rewritten).
    pub changed: Vec<String>,
    /// Services restarted to apply the change: none — `wifi` picks up
    /// Wi-Fi changes by itself and the rest applies at the next boot.
    pub restart: Vec<String>,
    /// Some changed key only takes effect after a reboot.
    pub reboot_required: bool,
}

/// Serializes read-modify-write cycles of `camera.conf`.
static WRITE: Mutex<()> = Mutex::new(());

/// `GET /api/config` → known keys' values, secrets omitted.
async fn read(
    State(state): State<AppState>,
    _: AuthUser,
) -> Result<Json<ConfigResponse>, ApiError> {
    let conf = state.config.camera_conf.clone();
    let secret = state.config.onvif_secret.clone();
    let response = tokio::task::spawn_blocking(move || -> std::io::Result<ConfigResponse> {
        let text = read_text(&conf)?;
        Ok(render(&text, crate::onvif::load(&secret).ok().flatten()))
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(|error| ApiError::internal(format!("cannot read camera.conf: {error}")))?;
    Ok(Json(response))
}

/// The file's text; missing is empty.
fn read_text(path: &Path) -> std::io::Result<String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
}

fn render(text: &str, onvif: Option<crate::onvif::Credentials>) -> ConfigResponse {
    let parsed = wifi::settings::parse(text);
    let mut values = Map::new();
    let mut secrets_set = Vec::new();
    for key in KEYS {
        let Some(value) = parsed.get(key.name) else {
            continue;
        };
        if key.secret {
            if !value.is_empty() {
                secrets_set.push(key.name);
            }
            continue;
        }
        let json = match key.kind {
            Kind::Flag => value
                .parse::<u8>()
                .map_or_else(|_| Value::from(value.clone()), Value::from),
            _ => Value::from(value.clone()),
        };
        values.insert(key.name.to_owned(), json);
    }
    // The live ONVIF account wins over the seed values in camera.conf.
    if let Some(onvif) = onvif {
        values.insert("onvif_user".to_owned(), Value::from(onvif.user));
        if !secrets_set.contains(&"onvif_password") {
            secrets_set.push("onvif_password");
        }
    }
    ConfigResponse {
        values,
        secrets_set,
    }
}

/// `PUT /api/config` (admin) `{values}` → `{changed, restart, reboot_required}`.
async fn update(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    crate::error::ApiJson(update): crate::error::ApiJson<ConfigUpdate>,
) -> Result<Json<ConfigUpdateResult>, ApiError> {
    let mut wanted = Vec::with_capacity(update.values.len());
    for (name, value) in &update.values {
        let key =
            known(name).ok_or_else(|| ApiError::bad_request(format!("unknown key {name:?}")))?;
        if !key.writable {
            return Err(ApiError::bad_request(match key.name {
                "onvif_user" | "onvif_password" => {
                    format!("{name} is set with PUT /api/onvif/credentials")
                }
                _ => format!("{name} is not settable here; manage web users on the Users page"),
            }));
        }
        let value = validate(key, value)
            .map_err(|reason| ApiError::bad_request(format!("{name}: {reason}")))?;
        wanted.push((key, value));
    }
    let path = state.config.camera_conf.clone();
    let changed = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<&'static Key>> {
        let _guard = crate::state::lock(&WRITE);
        let text = read_text(&path)?;
        let (next, changed) = rewrite(&text, &wanted);
        if !changed.is_empty() {
            ::auth::atomic::write(&path, next.as_bytes(), 0o600)?;
        }
        Ok(changed)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(|error| ApiError::write_failed("camera.conf", &error))?;

    let names: Vec<String> = changed.iter().map(|key| key.name.to_owned()).collect();
    tracing::info!(user = %user.user, changed = ?names, "camera.conf updated");
    Ok(Json(ConfigUpdateResult {
        reboot_required: changed.iter().any(|key| key.needs_reboot),
        changed: names,
        restart: Vec::new(),
    }))
}

/// Checks and normalizes one value to the text that goes in the file.
fn validate(key: &Key, value: &Value) -> Result<String, String> {
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) if key.kind == Kind::Flag => u8::from(*b).to_string(),
        Value::Number(n) if n.is_u64() || n.is_i64() => n.to_string(),
        _ => return Err("expected a string".into()),
    };
    if text.contains(['\n', '\r', '\0']) {
        return Err("line breaks are not allowed".into());
    }
    if text.contains('\'') {
        return Err("single quotes cannot be stored in camera.conf".into());
    }
    if text.chars().any(char::is_control) {
        return Err("control characters are not allowed".into());
    }
    if text.len() > 256 {
        return Err("longer than 256 bytes".into());
    }
    match key.kind {
        Kind::Flag => match text.as_str() {
            "0" | "1" => {}
            "true" => return Ok("1".into()),
            "false" => return Ok("0".into()),
            _ => return Err("must be 0 or 1".into()),
        },
        Kind::LogLevel => {
            // A level or an EnvFilter directive list; anything else would
            // only be ignored by every binary, so catch typos early.
            let ok = !text.is_empty()
                && text.split(',').all(|directive| {
                    !directive.is_empty()
                        && directive
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_=:.-[]{}*\"".contains(&b))
                });
            if !ok {
                return Err(
                    "expected error, warn, info, debug, trace or a directive list like 'info,web=debug'"
                        .into(),
                );
            }
        }
        Kind::Metrics => {
            let ok = text.is_empty()
                || text
                    .strip_prefix("textfile:")
                    .is_some_and(|dir| !dir.trim().is_empty());
            if !ok {
                return Err("expected empty or textfile:<directory>".into());
            }
        }
        Kind::Ssid => {
            if text.len() > 32 {
                return Err(format!("{} bytes; the limit is 32", text.len()));
            }
            if text.contains(['"', '\\']) {
                return Err(
                    "quotes and backslashes are not supported by wpa_supplicant's config".into(),
                );
            }
        }
        Kind::WifiPassword => {
            let len = text.chars().count();
            if !(len == 0 || (8..=63).contains(&len)) {
                return Err(format!("{len} characters; WPA-PSK needs 8-63"));
            }
            if text.contains(['"', '\\']) {
                return Err(
                    "quotes and backslashes are not supported by wpa_supplicant's config".into(),
                );
            }
        }
        Kind::User => {
            let ok = !text.is_empty()
                && text.len() <= 32
                && text
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
            if !ok {
                return Err("1-32 letters, digits, '.', '_' or '-'".into());
            }
        }
        Kind::Password => {}
    }
    Ok(text)
}

/// `value` as sh source: bare if harmless, otherwise single-quoted.
/// [`validate`] has already refused single quotes and line breaks.
fn quote(value: &str) -> String {
    let bare = value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_./:,=@%+-".contains(&b));
    if bare {
        value.to_owned()
    } else {
        format!("'{value}'")
    }
}

/// The key assigned on `line`, if it is a `key=value` line.
fn assigned_key(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return None;
    }
    let (key, _) = trimmed.split_once('=')?;
    (!key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')).then_some(key)
}

/// Applies `wanted` to the file text: every assignment of a changed key is
/// rewritten in place, keys not in the file are appended, everything else is
/// kept byte for byte. Returns the new text and the keys that changed.
fn rewrite(text: &str, wanted: &[(&'static Key, String)]) -> (String, Vec<&'static Key>) {
    let current: HashMap<String, String> = wifi::settings::parse(text);
    let changed: Vec<(&'static Key, &String)> = wanted
        .iter()
        .filter(|(key, value)| current.get(key.name) != Some(value))
        .map(|(key, value)| (*key, value))
        .collect();
    if changed.is_empty() {
        return (text.to_owned(), Vec::new());
    }
    let line_for = |key: &Key, value: &str| format!("{}={}", key.name, quote(value));

    let mut out = String::with_capacity(text.len() + 64);
    let mut written = Vec::new();
    for line in text.split_inclusive('\n') {
        let replacement =
            assigned_key(line).and_then(|name| changed.iter().find(|(key, _)| key.name == name));
        match replacement {
            Some((key, value)) => {
                out.push_str(&line_for(key, value));
                out.push_str(if line.ends_with("\r\n") {
                    "\r\n"
                } else if line.ends_with('\n') {
                    "\n"
                } else {
                    ""
                });
                written.push(key.name);
            }
            None => out.push_str(line),
        }
    }
    for (key, value) in &changed {
        if !written.contains(&key.name) {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&line_for(key, value));
            out.push('\n');
        }
    }
    (out, changed.into_iter().map(|(key, _)| key).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn want(pairs: &[(&str, &str)]) -> Vec<(&'static Key, String)> {
        pairs
            .iter()
            .map(|(name, value)| (known(name).unwrap(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn rewrite_keeps_comments_order_and_unknown_lines() {
        let text = "# header\nrun_rust=1\n\n# level\nlog_level=info   # note\ncustom_thing=\"x y\"\nwifi_ssid=\n";
        let (next, changed) = rewrite(
            text,
            &want(&[
                ("log_level", "debug"),
                ("run_rust", "1"),
                ("wifi_password", "p@ss word;x"),
            ]),
        );
        let names: Vec<_> = changed.iter().map(|k| k.name).collect();
        assert_eq!(names, ["log_level", "wifi_password"]);
        assert_eq!(
            next,
            "# header\nrun_rust=1\n\n# level\nlog_level=debug\ncustom_thing=\"x y\"\nwifi_ssid=\nwifi_password='p@ss word;x'\n"
        );
        // What wifi and sh will read back.
        let parsed = wifi::settings::parse(&next);
        assert_eq!(parsed["wifi_password"], "p@ss word;x");
        assert_eq!(parsed["log_level"], "debug");
    }

    #[test]
    fn no_change_is_no_rewrite_and_missing_newline_is_handled() {
        let (next, changed) = rewrite("log_level=info", &want(&[("log_level", "info")]));
        assert!(changed.is_empty());
        assert_eq!(next, "log_level=info");
        let (next, _) = rewrite("log_level=info", &want(&[("metrics", "textfile:/tmp/m")]));
        assert_eq!(next, "log_level=info\nmetrics=textfile:/tmp/m\n");
        let (next, _) = rewrite("a=1\r\nlog_level=info\r\n", &want(&[("log_level", "warn")]));
        assert_eq!(next, "a=1\r\nlog_level=warn\r\n");
    }

    #[test]
    fn validation() {
        let v = |name: &str, value: Value| validate(known(name).unwrap(), &value);
        assert_eq!(v("run_rust", json!(true)).unwrap(), "1");
        assert_eq!(v("run_rust", json!(0)).unwrap(), "0");
        assert!(v("run_rust", json!(2)).is_err());
        assert!(v("run_rust", json!("yes")).is_err());
        assert!(v("log_level", json!("info,web=debug")).is_ok());
        assert!(v("log_level", json!("info; reboot")).is_err());
        assert!(v("log_level", json!("")).is_err());
        assert!(v("metrics", json!("")).is_ok());
        assert!(v("metrics", json!("textfile:/mnt/m")).is_ok());
        assert!(v("metrics", json!("http:0.0.0.0:9000")).is_err());
        assert!(v("wifi_ssid", json!("x".repeat(33))).is_err());
        assert!(v("wifi_ssid", json!("")).is_ok());
        assert!(v("wifi_password", json!("short")).is_err());
        assert!(v("wifi_password", json!("")).is_ok());
        assert!(v("wifi_password", json!("long enough")).is_ok());
        assert!(v("ssh_password", json!("it's")).is_err());
        assert!(v("ssh_password", json!("a\nb")).is_err());
        assert!(v("ssh_user", json!("root")).is_ok());
        assert!(v("ssh_user", json!("ro ot")).is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote(""), "");
        assert_eq!(quote("info,x=debug"), "info,x=debug");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("$(reboot)"), "'$(reboot)'");
    }
}
