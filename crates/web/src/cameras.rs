//! Per-camera details: what the kernel reports about each image sensor
//! (`/api/hardware`) merged with what the `camera` service measures (state,
//! frame rate, formats) and the RTSP path that serves it.
//!
//! The camera service's `/status` ([`video_proto::Status`]) sits next to its
//! snapshot URL on loopback; when the service is down the sensors are still
//! listed, with `state` `unavailable` and the reason. Sensors the board
//! profile expects but nothing found are listed as `missing`.

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::hardware;
use crate::state::AppState;
use axum::extract::State;
use axum::http::{header, HeaderMap};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use video_proto::Status;

/// Largest `/status` reply accepted.
const MAX_STATUS: u64 = 64 * 1024;

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/cameras", get(cameras))
}

/// `api.ts` `CameraDetails`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CameraDetails {
    pub index: u32,
    pub sensor: String,
    pub bus: Option<String>,
    pub channels: u32,
    /// RTSP path, [`video_proto::stream_path`] (`vs0`, ...).
    pub rtsp_path: String,
    /// Port the `rtsp` service listens on.
    pub rtsp_port: u16,
    /// `rtsp://<host>[:port]/<path>` with the host the browser used to
    /// reach this server; `None` when the request named no usable host.
    pub rtsp_url: Option<String>,
    pub live: Status,
}

/// Where the RTSP streams are reachable, for [`merge`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rtsp {
    /// Host name or address, no port.
    pub host: Option<String>,
    pub port: u16,
}

impl Rtsp {
    fn url(&self, path: &str) -> Option<String> {
        let host = self.host.as_deref()?;
        Some(if self.port == video_proto::DEFAULT_RTSP_PORT {
            format!("rtsp://{host}/{path}")
        } else {
            format!("rtsp://{host}:{}/{path}", self.port)
        })
    }
}

/// The host part of a `Host` header: `cam.lan:8080` → `cam.lan`,
/// `[fe80::1]:80` → `[fe80::1]`. `None` when empty or when it holds
/// anything a host name or address cannot, so a forged header cannot inject
/// a path or credentials into the URL.
pub fn host_name(header: &str) -> Option<String> {
    let header = header.trim();
    let host = if header.starts_with('[') {
        &header[..=header.find(']')?]
    } else {
        header.split(':').next()?
    };
    let allowed =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '[' | ']' | ':');
    (!host.is_empty() && host.chars().all(allowed)).then(|| host.to_owned())
}

/// `/snapshot.jpg` URL → `/status` URL on the same service.
fn status_url(snapshot_url: &str) -> Option<String> {
    let rest = snapshot_url.strip_prefix("http://")?;
    let host = rest.split('/').next()?;
    Some(format!("http://{host}/status"))
}

async fn fetch_status(url: &str) -> Result<Vec<Status>, String> {
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

/// Merges sensors and live status; sensors without status are
/// `unavailable`. `expected` are the sensors the board profile lists: one
/// neither the kernel nor the camera service knows is still listed, as
/// `missing`, so a dead sensor shows up as a failure instead of vanishing.
pub fn merge(
    sensors: &[hardware::Camera],
    expected: &[u32],
    live: Result<Vec<Status>, String>,
    rtsp: &Rtsp,
) -> Vec<CameraDetails> {
    let (live, reason) = match live {
        Ok(l) => (l, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let mut indices: Vec<u32> = sensors.iter().map(|c| c.index).collect();
    indices.extend(live.iter().map(|l| l.index));
    indices.extend_from_slice(expected);
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
                .unwrap_or_else(|| match hw {
                    None => Status {
                        index,
                        state: "missing".into(),
                        error: Some(
                            "expected by the board profile, not found by the kernel".into(),
                        ),
                        ..Status::default()
                    },
                    Some(_) => Status {
                        index,
                        state: "unavailable".into(),
                        error: reason
                            .clone()
                            .or_else(|| Some("not captured by the camera service".into())),
                        ..Status::default()
                    },
                });
            let rtsp_path = video_proto::stream_path(index);
            CameraDetails {
                index,
                sensor: hw.map_or_else(|| "unknown".into(), |c| c.sensor.clone()),
                bus: hw.and_then(|c| c.bus.clone()),
                channels: hw.map_or(0, |c| c.channels),
                rtsp_url: rtsp.url(&rtsp_path),
                rtsp_port: rtsp.port,
                rtsp_path,
                live,
            }
        })
        .collect()
}

/// `GET /api/cameras`.
async fn cameras(
    State(state): State<AppState>,
    _: AuthUser,
    headers: HeaderMap,
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
    let expected = state
        .config
        .board
        .as_ref()
        .map_or(&[][..], |b| &b.profile.video.sensors[..]);
    let rtsp = Rtsp {
        host: headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .and_then(host_name),
        port: state.config.rtsp_port,
    };
    Ok(Json(merge(&hw.cameras, expected, live, &rtsp)))
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

    fn sc2331(index: u32) -> hardware::Camera {
        hardware::Camera {
            index,
            sensor: "sc2331".into(),
            bus: Some("0-0030".into()),
            channels: 3,
        }
    }

    fn rtsp(host: Option<&str>, port: u16) -> Rtsp {
        Rtsp {
            host: host.map(str::to_owned),
            port,
        }
    }

    #[test]
    fn merges_sensors_and_status() {
        let sensors = vec![sc2331(0), sc2331(1)];
        let live = vec![Status {
            index: 1,
            state: "running".into(),
            fps: 15.0,
            ..Status::default()
        }];
        let at = rtsp(Some("cam.lan"), video_proto::DEFAULT_RTSP_PORT);
        let merged = merge(&sensors, &[], Ok(live), &at);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].live.state, "unavailable");
        assert_eq!(merged[1].live.fps, 15.0);
        assert_eq!(merged[1].rtsp_path, "vs1");
        assert_eq!(merged[1].rtsp_port, 554);
        assert_eq!(merged[1].rtsp_url.as_deref(), Some("rtsp://cam.lan/vs1"));
        let down = merge(&sensors, &[], Err("down".into()), &at);
        assert_eq!(down[0].live.error.as_deref(), Some("down"));
    }

    #[test]
    fn expected_sensors_are_listed_even_when_missing() {
        let at = rtsp(None, 554);
        // Sensor 1 neither probed nor captured: listed as missing.
        let merged = merge(&[sc2331(0)], &[0, 1], Err("down".into()), &at);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].live.state, "unavailable");
        assert_eq!(merged[1].index, 1);
        assert_eq!(merged[1].live.state, "missing");
        assert_eq!(merged[1].sensor, "unknown");
        assert!(merged[1].live.error.is_some());
        // Captured though the kernel listing lacks it: the live state wins.
        let live = vec![Status {
            index: 1,
            state: "running".into(),
            ..Status::default()
        }];
        let merged = merge(&[], &[1], Ok(live), &at);
        assert_eq!(merged[0].live.state, "running");
        assert_eq!(merged[0].rtsp_url, None);
    }

    #[test]
    fn rtsp_url_from_host_and_port() {
        assert_eq!(
            rtsp(Some("10.0.0.5"), 554).url("vs0").as_deref(),
            Some("rtsp://10.0.0.5/vs0")
        );
        assert_eq!(
            rtsp(Some("cam"), 8554).url("vs1").as_deref(),
            Some("rtsp://cam:8554/vs1")
        );
        assert_eq!(rtsp(None, 554).url("vs0"), None);
    }

    #[test]
    fn host_header_loses_its_port() {
        assert_eq!(host_name("10.0.0.5").as_deref(), Some("10.0.0.5"));
        assert_eq!(host_name("cam.lan:8080").as_deref(), Some("cam.lan"));
        assert_eq!(host_name("[fe80::1]:80").as_deref(), Some("[fe80::1]"));
        assert_eq!(host_name("[::1]").as_deref(), Some("[::1]"));
        assert_eq!(host_name(""), None);
        assert_eq!(host_name("[::1"), None);
        assert_eq!(host_name("evil/x"), None);
        assert_eq!(host_name("user@cam"), None);
    }

    #[test]
    fn producer_status_parses_as_video_proto() {
        let body = r#"[{"index":0,"state":"running","width":640,"height":360,"format":"NV12","encoding":"JPEG snapshots (software)","frames":1,"fps":15.0,"sensor_width":1920,"sensor_height":1080,"sensor_id":null,"tuning":null,"stream":null,"error":null}]"#;
        let parsed: Vec<Status> = serde_json::from_str(body).unwrap();
        assert_eq!(parsed[0].state, "running");
        assert_eq!(parsed[0].sensor_width, 1920);
    }
}
