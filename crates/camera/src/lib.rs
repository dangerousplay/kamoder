//! Pure-Rust video capture for the AK3918AV100: the ISP tuning loader, the
//! camera host driver's V4L2 interface, and JPEG snapshots — no vendor
//! library. See `device` for the bring-up sequence and `conf` for the tuning
//! file format.

pub mod conf;
pub mod device;
pub mod jpeg;
pub mod service;
pub mod v4l2;
pub mod venc;
