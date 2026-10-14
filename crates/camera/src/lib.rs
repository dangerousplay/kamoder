//! Vendor-free camera core: the [`backend`] traits a board's video hardware
//! implements, the [`service`] (snapshots, `/status`, the video socket) that
//! drives them, [`jpeg`] encoding, and backends that need no hardware
//! ([`pattern`], and `fake` for tests). Hardware backends live in their own
//! crates (`anyka-av100`); the `camera` binary that picks one from the board
//! profile is `camerad`.

pub mod backend;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod jpeg;
pub mod pattern;
pub mod service;
