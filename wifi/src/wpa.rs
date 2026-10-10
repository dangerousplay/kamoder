//! `wpa_supplicant0.conf` and the wpa_supplicant control socket.

use snafu::Snafu;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One WPA-PSK network.
#[derive(Clone, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    /// Passphrase, or 64 hex digits when the config stores a raw key.
    pub psk: String,
}

// Hand-written so a `{:?}` in a log line can never print the key.
impl std::fmt::Debug for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Network")
            .field("ssid", &self.ssid)
            .field("psk", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Snafu, PartialEq, Eq)]
pub enum InvalidNetwork {
    #[snafu(display("SSID is empty"))]
    EmptySsid,
    #[snafu(display("SSID is {len} bytes; the limit is 32"))]
    SsidTooLong { len: usize },
    #[snafu(display("passphrase is {len} characters; WPA-PSK needs 8-63"))]
    PskLength { len: usize },
    #[snafu(display("{field} contains a quote, backslash or control character"))]
    Unquotable { field: &'static str },
}

impl Network {
    /// Rejects values that wpa_supplicant would refuse or misparse. The
    /// config quotes both fields, so an embedded quote would end the string
    /// early and silently configure a different network.
    pub fn validate(&self) -> Result<(), InvalidNetwork> {
        if self.ssid.is_empty() {
            return Err(InvalidNetwork::EmptySsid);
        }
        if self.ssid.len() > 32 {
            return Err(InvalidNetwork::SsidTooLong {
                len: self.ssid.len(),
            });
        }
        let len = self.psk.chars().count();
        if !(8..=63).contains(&len) {
            return Err(InvalidNetwork::PskLength { len });
        }
        let unquotable = |s: &str| s.chars().any(|c| c == '"' || c == '\\' || c.is_control());
        if unquotable(&self.ssid) {
            return Err(InvalidNetwork::Unquotable { field: "SSID" });
        }
        if unquotable(&self.psk) {
            return Err(InvalidNetwork::Unquotable {
                field: "passphrase",
            });
        }
        Ok(())
    }

    /// The config file this firmware's own tools write, byte for byte in
    /// shape, so the vendor scripts keep reading it.
    pub fn render(&self, ctrl_dir: &Path) -> String {
        format!(
            "ctrl_interface={}\n\
             update_config=1\n\
             network={{\n\
             \x20   ssid=\"{}\"\n\
             \x20   scan_ssid=1\n\
             \x20   key_mgmt=WPA-PSK\n\
             \x20   psk=\"{}\"\n\
             }}\n",
            ctrl_dir.display(),
            self.ssid,
            self.psk
        )
    }
}

/// The first network block's SSID and key, or `None` if there is none.
///
/// Accepts `psk="passphrase"` and the unquoted 64-hex-digit form.
pub fn parse_config(text: &str) -> Option<Network> {
    let mut ssid = None;
    let mut psk = None;
    let mut in_block = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("network=") {
            in_block = true;
            continue;
        }
        if !in_block {
            continue;
        }
        if line.starts_with('}') {
            break;
        }
        if let Some(value) = line.strip_prefix("ssid=") {
            ssid = Some(strip_quotes(value));
        } else if let Some(value) = line.strip_prefix("psk=") {
            psk = Some(strip_quotes(value));
        }
    }
    Some(Network {
        ssid: ssid?,
        psk: psk.unwrap_or_default(),
    })
}

fn strip_quotes(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
        .to_owned()
}

/// What wpa_supplicant reports about the link.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// `COMPLETED` once associated and keyed.
    pub state: Option<String>,
    pub ssid: Option<String>,
    pub bssid: Option<String>,
}

impl Status {
    pub fn parse(reply: &str) -> Self {
        let mut status = Status::default();
        for line in reply.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "wpa_state" => status.state = Some(value.to_owned()),
                "ssid" => status.ssid = Some(value.to_owned()),
                "bssid" => status.bssid = Some(value.to_owned()),
                _ => {}
            }
        }
        status
    }

    pub fn completed(&self) -> bool {
        self.state.as_deref() == Some("COMPLETED")
    }
}

/// Queries wpa_supplicant over its control socket.
///
/// The protocol is one datagram per request on a Unix socket; the client must
/// bind its own path so the daemon has somewhere to reply. `wpa_cli` is not on
/// every firmware, so this speaks it directly.
pub fn query(ctrl_dir: &Path, iface: &str, command: &str) -> std::io::Result<String> {
    let server = ctrl_dir.join(iface);
    let local = client_path();
    let _ = std::fs::remove_file(&local);
    let socket = UnixDatagram::bind(&local)?;
    // Remove the path whatever happens below.
    let _guard = RemoveOnDrop(local);
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;
    socket.connect(&server)?;
    socket.send(command.as_bytes())?;
    let mut buf = vec![0u8; 4096];
    let len = socket.recv(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf[..len]).into_owned())
}

fn client_path() -> PathBuf {
    PathBuf::from(format!("/tmp/anyka-wifi-{}.ctrl", std::process::id()))
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(ssid: &str, psk: &str) -> Network {
        Network {
            ssid: ssid.into(),
            psk: psk.into(),
        }
    }

    #[test]
    fn render_then_parse_round_trips() {
        let original = net("Home Net", "correct horse");
        let text = original.render(Path::new("/etc/Wireless"));
        assert!(text.starts_with("ctrl_interface=/etc/Wireless\n"));
        assert_eq!(parse_config(&text), Some(original));
    }

    #[test]
    fn parses_the_firmware_layout_and_hex_keys() {
        let text = "ctrl_interface=/etc/Wireless\nupdate_config=1\nnetwork={\n\tssid=\"Lab\"\n\tscan_ssid=1\n\tkey_mgmt=WPA-PSK\n\tpsk=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n}\n";
        let parsed = parse_config(text).unwrap();
        assert_eq!(parsed.ssid, "Lab");
        assert_eq!(parsed.psk.len(), 64);
    }

    #[test]
    fn scan_ssid_is_not_mistaken_for_ssid() {
        let text = "network={\n scan_ssid=1\n ssid=\"Real\"\n psk=\"12345678\"\n}\n";
        assert_eq!(parse_config(text).unwrap().ssid, "Real");
    }

    #[test]
    fn no_network_block_is_none() {
        assert_eq!(parse_config("ctrl_interface=/etc/Wireless\n"), None);
    }

    #[test]
    fn validation() {
        assert!(net("Home", "12345678").validate().is_ok());
        assert_eq!(
            net("", "12345678").validate(),
            Err(InvalidNetwork::EmptySsid)
        );
        assert_eq!(
            net("Home", "short").validate(),
            Err(InvalidNetwork::PskLength { len: 5 })
        );
        assert!(matches!(
            net("Ho\"me", "12345678").validate(),
            Err(InvalidNetwork::Unquotable { field: "SSID" })
        ));
        assert!(matches!(
            net("Home", "1234\n5678").validate(),
            Err(InvalidNetwork::Unquotable { .. })
        ));
        assert!(matches!(
            net(&"x".repeat(33), "12345678").validate(),
            Err(InvalidNetwork::SsidTooLong { len: 33 })
        ));
    }

    #[test]
    fn debug_never_shows_the_key() {
        let shown = format!("{:?}", net("Home", "supersecret"));
        assert!(!shown.contains("supersecret"), "{shown}");
        assert!(shown.contains("Home"));
    }

    #[test]
    fn status_reply() {
        let status = Status::parse(
            "bssid=aa:bb:cc:dd:ee:ff\nfreq=2437\nssid=Home\nid=0\nmode=station\npairwise_cipher=CCMP\nkey_mgmt=WPA2-PSK\nwpa_state=COMPLETED\nip_address=192.168.20.187\n",
        );
        assert!(status.completed());
        assert_eq!(status.ssid.as_deref(), Some("Home"));
        assert_eq!(status.bssid.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
        assert!(!Status::parse("wpa_state=SCANNING\n").completed());
    }
}
