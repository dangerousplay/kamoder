//! Wi-Fi status and reboot (T14, CFG-03/05).
//!
//! `/api/wifi` computes what `wifi status` prints, with the same
//! library calls, instead of running the binary: no fork of a second
//! multi-megabyte process on a 32 MB board. What only the daemon's memory
//! knows (retry counters, the rollback hold timer) is not visible from here,
//! so `rollback_in_s` is always `null` and `state` is derived from what is
//! observable: the desired, configured, last-good and associated networks.

use crate::auth::{AdminUser, AuthUser};
use crate::error::ApiError;
use crate::state::{AppState, Config};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Mutex;
use wifi::reconcile::Observed;
use wifi::{net, settings, system, wpa};

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/wifi", get(wifi))
        .route("/system/reboot", post(reboot))
}

/// `api.ts` `WifiStatus`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WifiStatus {
    pub iface: String,
    /// wpa_supplicant reports `COMPLETED`.
    pub connected: bool,
    /// The network currently associated.
    pub ssid: Option<String>,
    pub ip: Option<String>,
    pub gateway: Option<String>,
    pub signal_dbm: Option<i32>,
    /// `healthy`, `switching` (desired network not applied yet), `rolled_back`
    /// (on the last-good network instead of the desired one), `reconnecting`.
    pub state: &'static str,
    /// Associated, addressed and the gateway answers ARP.
    pub healthy: bool,
    /// `camera.conf` `wifi_ssid`; `None` = keep the current network.
    pub target_ssid: Option<String>,
    /// The network in the station config (`/rom/wpa_supplicant0.conf`).
    pub configured_ssid: Option<String>,
    pub last_good_ssid: Option<String>,
    /// Not observable outside the wifi process; always `None`.
    pub rollback_in_s: Option<u64>,
}

/// wpa_supplicant's control socket client path is per process; one query at
/// a time.
static WPA: Mutex<()> = Mutex::new(());

/// Gathers the status. Blocking: control-socket round trips and, when the
/// gateway is not already in the ARP cache, up to ~3 s of ARP probing.
pub fn wifi_status(camera_conf: &std::path::Path, paths: &system::Paths) -> WifiStatus {
    let target = settings::load(camera_conf).ok().and_then(|d| d.network);
    let configured = system::read_configured(paths);
    let last_good = system::load_last_good(paths);
    let (status, signal) = {
        let _guard = crate::state::lock(&WPA);
        let status = wpa::query(&paths.ctrl_dir, &paths.iface, "STATUS")
            .map(|reply| wpa::Status::parse(&reply))
            .unwrap_or_default();
        let signal = if status.completed() {
            wpa::query(&paths.ctrl_dir, &paths.iface, "SIGNAL_POLL")
                .ok()
                .and_then(|reply| rssi(&reply))
        } else {
            None
        };
        (status, signal)
    };
    let ip = net::ipv4_address(&paths.iface);
    let gateway = net::default_gateway(&paths.iface);
    let gateway_reachable = match (status.completed(), ip, gateway) {
        (true, Some(_), Some(gw)) => Some(
            std::fs::read_to_string("/proc/net/arp")
                .is_ok_and(|table| net::arp_complete(&table, &paths.iface, gw))
                || net::gateway_reachable(&paths.iface, gw),
        ),
        _ => None,
    };
    let observed = Observed {
        configured: configured.clone(),
        associated: status.completed(),
        associated_ssid: status.ssid.clone(),
        has_address: ip.is_some(),
        gateway_reachable,
    };
    let healthy = observed.healthy();
    let ssid_of = |n: &Option<wpa::Network>| n.as_ref().map(|n| n.ssid.clone());
    let target_ssid = ssid_of(&target);
    let configured_ssid = ssid_of(&configured);
    let last_good_ssid = ssid_of(&last_good);
    let on_target = target_ssid.is_none() || target_ssid == configured_ssid;
    let state = if !healthy {
        "reconnecting"
    } else if on_target {
        "healthy"
    } else if configured_ssid.is_some() && configured_ssid == last_good_ssid {
        "rolled_back"
    } else {
        "switching"
    };
    WifiStatus {
        iface: paths.iface.clone(),
        connected: status.completed(),
        ssid: status.ssid,
        ip: ip.map(|ip| ip.to_string()),
        gateway: gateway.map(|gw| gw.to_string()),
        signal_dbm: signal,
        state,
        healthy,
        target_ssid,
        configured_ssid,
        last_good_ssid,
        rollback_in_s: None,
    }
}

/// `RSSI=-52` out of a `SIGNAL_POLL` reply.
fn rssi(reply: &str) -> Option<i32> {
    reply
        .lines()
        .find_map(|line| line.strip_prefix("RSSI="))
        .and_then(|v| v.trim().parse().ok())
}

/// `GET /api/wifi` → `WifiStatus`.
async fn wifi(State(state): State<AppState>, _: AuthUser) -> Result<Json<WifiStatus>, ApiError> {
    let conf = state.config.camera_conf.clone();
    let paths = state.config.wifi.clone();
    let status = tokio::task::spawn_blocking(move || wifi_status(&conf, &paths))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(status))
}

/// `POST /api/system/reboot` (admin) → 202, then after
/// [`Tuning::reboot_delay`](crate::state::Tuning) the reboot starts.
async fn reboot(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> (StatusCode, Json<Value>) {
    tracing::warn!(user = %user.user, "reboot requested");
    let delay = state.tuning.reboot_delay;
    let reboot = state.tuning.reboot;
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        reboot(&state.config);
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({ "rebooting": true, "message": "stopping services, then rebooting" })),
    )
}

/// The real reboot: `sh init.sh --stop; sync; reboot`, in a detached shell.
///
/// Detached (own process group, no pipes to us) because `init.sh --stop`
/// stops the supervisor, which stops this very process; the shell has to
/// outlive it to get to `sync` and `reboot`. Stopping the supervisor first
/// lets every service shut down and flush its log (CFG-05).
pub fn reboot_now(config: &Config) {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let script =
        std::fs::canonicalize(&config.init_script).unwrap_or_else(|_| config.init_script.clone());
    tracing::warn!(init = %script.display(), "rebooting");
    let spawned = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"if [ -f "$1" ]; then sh "$1" --stop; fi; sync; sleep 1; reboot || reboot -f"#)
        .arg("sh")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    if let Err(error) = spawned {
        tracing::error!(%error, "cannot start the reboot shell; not rebooting");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rssi_parsing() {
        assert_eq!(rssi("RSSI=-52\nLINKSPEED=65\n"), Some(-52));
        assert_eq!(rssi("FAIL\n"), None);
    }

    #[test]
    fn status_without_a_radio() {
        let dir = std::env::temp_dir().join(format!("anyka-web-wifi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("backup")).unwrap();
        std::fs::write(
            dir.join("camera.conf"),
            "wifi_ssid='Home Net'\nwifi_password=secret123\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("wpa.conf"),
            "network={\n\tssid=\"Old\"\n\tpsk=\"password1\"\n}\n",
        )
        .unwrap();
        let paths = system::Paths {
            wpa_conf: dir.join("wpa.conf"),
            ctrl_dir: dir.join("ctrl"),
            iface: "anykatest0".into(),
            dhcp_script: dir.join("none"),
            state_dir: dir.join("backup"),
        };
        let status = wifi_status(&dir.join("camera.conf"), &paths);
        assert!(!status.connected);
        assert!(!status.healthy);
        assert_eq!(status.state, "reconnecting");
        assert_eq!(status.target_ssid.as_deref(), Some("Home Net"));
        assert_eq!(status.configured_ssid.as_deref(), Some("Old"));
        assert_eq!(status.last_good_ssid, None);
        assert_eq!(status.ip, None);
    }
}
