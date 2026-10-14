//! Vendor-free video capture: the camera core (`backend` traits, the
//! `service` with snapshots, `/status` and the video socket, `jpeg`) and,
//! until it moves to its own crate, the AK3918AV100 backend — the ISP
//! tuning loader, the camera host driver's V4L2 interface and the hardware
//! H.264 encoder, no vendor library. See `device` for the bring-up sequence,
//! `conf` for the tuning file format and `av100` for how it plugs into the
//! core.

pub mod av100;
pub mod backend;
pub mod conf;
pub mod device;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod jpeg;
pub mod service;
pub mod v4l2;
pub mod venc;
