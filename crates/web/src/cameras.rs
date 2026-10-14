//! Per-camera details: what the kernel reports about each image sensor
//! (`/api/hardware`) merged with what the `camera` service measures (state,
//! frame rate, formats) and the RTSP path that serves it.
//!
//! The camera service's `/status` sits next to its snapshot URL on loopback;
//! when the service is down the sensors are still listed, with `state`
//! `unavailable` and the reason.

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::hardware;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Largest `/status` reply accepted.
const MAX_STATUS: u64 = 64 * 1024;

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/cameras", get(cameras))
}

/// The sensor's H.264 stream, when the camera service encodes.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct StreamInfo {
    pub codec: String,
    pub profile: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub gop: u32,
    pub target_kbps: u32,
    pub kbps: f32,
    pub encoder: String,
}

/// One sensor as the camera service reports it.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Live {
    pub index: u32,
    pub state: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub encoding: String,
    #[serde(default)]
    pub frames: u64,
    #[serde(default)]
    pub fps: f32,
    #[serde(default)]
    pub sensor_width: u32,
    #[serde(default)]
    pub sensor_height: u32,
    pub sensor_id: Option<String>,
    pub tuning: Option<String>,
    #[serde(default)]
    pub stream: Option<StreamInfo>,
    pub error: Option<String>,
}

/// `api.ts` `CameraDetails`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CameraDetails {
    pub index: u32,
    pub sensor: String,
    pub bus: Option<String>,
    pub channels: u32,
    /// RTSP path on the camera's port 554 (`rtsp://<host>/<path>`).
    pub rtsp_path: String,
    pub live: Live,
}

/// `/snapshot.jpg` URL → `/status` URL on the same service.
fn status_url(snapshot_url: &str) -> Option<String> {
    let rest = snapshot_url.strip_prefix("http://")?;
    let host = rest.split('/').next()?;
    Some(format!("http://{host}/status"))
}

async fn fetch_status(url: &str) -> Result<Vec<Live>, String> {
    let (address, host, path) = crate::video::parse_url(url)?;
    let mut stream = TcpStream::connect(&address)
        .await
        .map_err(|e| format!("camera service unreachable: {e}"))?;
    let request = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("camera service request failed: {e}"))?;
    let mut data = Vec::new();
    (&mut stream)
        .take(MAX_STATUS)
        .read_to_end(&mut data)
        .await
        .map_err(|e| format!("camera service read failed: {e}"))?;
    let end = data
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed reply from the camera service")?;
    serde_json::from_slice(&data[end + 4..]).map_err(|e| format!("bad camera status: {e}"))
}

/// Merges sensors and live status; sensors without status are `unavailable`.
pub fn merge(sensors: &[hardware::Camera], live: Result<Vec<Live>, String>) -> Vec<CameraDetails> {
    let (live, reason) = match live {
        Ok(l) => (l, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let mut indices: Vec<u32> = sensors.iter().map(|c| c.index).collect();
    indices.extend(live.iter().map(|l| l.index));
    indices.sort_unstable();
    indices.dedup();
    indices
        .into_iter()
        .map(|index| {
            let hw = sensors.iter().find(|c| c.index == index);
            let live = live
                .iter()
                .find(|l| l.index == index)
                .cloned()
                .unwrap_or_else(|| Live {
                    index,
                    state: "unavailable".into(),
                    error: reason
                        .clone()
                        .or_else(|| Some("not captured by the camera service".into())),
                    ..Live::default()
                });
            CameraDetails {
                index,
                sensor: hw.map_or_else(|| "unknown".into(), |c| c.sensor.clone()),
                bus: hw.and_then(|c| c.bus.clone()),
                channels: hw.map_or(0, |c| c.channels),
                rtsp_path: format!("vs{index}"),
                live,
            }
        })
        .collect()
}

/// `GET /api/cameras`.
async fn cameras(
    State(state): State<AppState>,
    _: AuthUser,
) -> Result<Json<Vec<CameraDetails>>, ApiError> {
    let root = state.config.sys_root.clone();
    let hw = tokio::task::spawn_blocking(move || hardware::read(&root))
        .await
        .map_err(ApiError::internal)?;
    let live = match status_url(&state.config.snapshot_url) {
        Some(url) => tokio::time::timeout(Duration::from_secs(2), fetch_status(&url))
            .await
            .unwrap_or_else(|_| Err("camera service did not answer".into())),
        None => Err("no camera service URL configured".into()),
    };
    Ok(Json(merge(&hw.cameras, live)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_url_from_snapshot_url() {
        assert_eq!(
            status_url("http://127.0.0.1:3000/snapshot.jpg").as_deref(),
            Some("http://127.0.0.1:3000/status")
        );
        assert_eq!(status_url("https://x/y"), None);
    }

    #[test]
    fn merges_sensors_and_status() {
        let sensors = vec![
            hardware::Camera {
                index: 0,
                sensor: "sc2331".into(),
                bus: Some("0-0030".into()),
                channels: 3,
            },
            hardware::Camera {
                index: 1,
                sensor: "sc2331".into(),
                bus: Some("0-0030".into()),
                channels: 3,
            },
        ];
        let live = vec![Live {
            index: 1,
            state: "running".into(),
            fps: 15.0,
            ..Live::default()
        }];
        let merged = merge(&sensors, Ok(live));
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].live.state, "unavailable");
        assert_eq!(merged[1].live.fps, 15.0);
        assert_eq!(merged[1].rtsp_path, "vs1");
        let down = merge(&sensors, Err("down".into()));
        assert_eq!(down[0].live.error.as_deref(), Some("down"));
    }
}
