//! Reads the desired network from `camera.conf`.
//!
//! That file is shell — `init.sh` and `Factory/config.sh` source it — so this
//! parses the subset of sh assignment syntax the file documents: `key=value`,
//! optionally single- or double-quoted, comments on their own lines. Anything
//! fancier (expansions, escapes) is deliberately not interpreted: a password
//! is taken literally, not evaluated.

use crate::wpa::Network;
use std::collections::HashMap;
use std::path::Path;

/// The subset of `camera.conf` this program acts on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Desired {
    /// `None` means "keep whatever is configured" — the reconciler still keeps
    /// the link up and still rolls back to the last good network, it just
    /// never writes a new one.
    pub network: Option<Network>,
}

/// Parses `camera.conf`. A missing file is not an error: it means no desired
/// network, which is the safe default.
pub fn load(path: &Path) -> std::io::Result<Desired> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(from_values(&parse(&text))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Desired::default()),
        Err(err) => Err(err),
    }
}

/// Builds the desired state from parsed values.
pub fn from_values(values: &HashMap<String, String>) -> Desired {
    let ssid = values.get("wifi_ssid").map(String::as_str).unwrap_or("");
    let psk = values
        .get("wifi_password")
        .map(String::as_str)
        .unwrap_or("");
    if ssid.is_empty() {
        return Desired::default();
    }
    Desired {
        network: Some(Network {
            ssid: ssid.to_owned(),
            psk: psk.to_owned(),
        }),
    }
}

/// Parses sh-style `key=value` lines.
pub fn parse(text: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        values.insert(key.to_owned(), unquote(raw));
    }
    values
}

/// `'x y'` → `x y`, `"x y"` → `x y`, `x # note` → `x`. An unterminated quote
/// takes the rest of the line rather than failing, matching what a human
/// editing the file most likely meant.
fn unquote(raw: &str) -> String {
    let raw = raw.trim_end_matches(['\r', '\n']);
    for quote in ['\'', '"'] {
        if let Some(rest) = raw.strip_prefix(quote) {
            return match rest.find(quote) {
                Some(end) => rest[..end].to_owned(),
                None => rest.to_owned(),
            };
        }
    }
    // Unquoted: sh ends the word at whitespace; anything after is a comment
    // or a mistake, and either way not part of the value.
    raw.split_whitespace().next().unwrap_or("").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_quoted_and_commented_values() {
        let values = parse(
            "# comment\n\
             wifi_ssid=Home\n\
             wifi_password='p@ss word;x'\n\
             ssh_user=\"root\"\n\
             log_level=info   # trailing note\n\
             metrics=\n\
             not a line\n\
             bad-key=1\n",
        );
        assert_eq!(values["wifi_ssid"], "Home");
        assert_eq!(values["wifi_password"], "p@ss word;x");
        assert_eq!(values["ssh_user"], "root");
        assert_eq!(values["log_level"], "info");
        assert_eq!(values["metrics"], "");
        assert!(!values.contains_key("bad-key"));
    }

    #[test]
    fn crlf_from_a_windows_editor_is_ignored() {
        let values = parse("wifi_ssid=Home\r\nwifi_password=secret123\r\n");
        assert_eq!(values["wifi_ssid"], "Home");
        assert_eq!(values["wifi_password"], "secret123");
    }

    #[test]
    fn empty_ssid_means_keep_current() {
        let desired = from_values(&parse("wifi_ssid=\nwifi_password=whatever1\n"));
        assert_eq!(desired.network, None);
    }

    #[test]
    fn ssid_and_password_become_the_desired_network() {
        let desired = from_values(&parse("wifi_ssid='My Net'\nwifi_password=hunter22\n"));
        assert_eq!(
            desired.network,
            Some(Network {
                ssid: "My Net".into(),
                psk: "hunter22".into()
            })
        );
    }

    #[test]
    fn missing_file_is_no_desired_network() {
        let desired = load(Path::new("/nonexistent/camera.conf")).unwrap();
        assert_eq!(desired, Desired::default());
    }
}
