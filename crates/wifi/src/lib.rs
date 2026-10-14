//! Wi-Fi station reconciler for the AK3918AV100.
//!
//! The camera is configured by hand and has no other way back onto the
//! network once it falls off: no Ethernet, no UART adapter attached, and the
//! vendor `/ipc/ipc` process rewrites the Wi-Fi configuration if it ever runs.
//! So instead of a one-shot setup script, this is a control loop:
//!
//! ```text
//!   desired  (camera.conf)  ─┐
//!   observed (/rom config,   ├─> reconcile::decide ─> Action ─> system::*
//!             wpa_supplicant │
//!             STATUS, wlan0, │
//!             gateway ARP)  ─┘
//! ```
//!
//! Its one promise is **the camera stays reachable**. It converges on the
//! network `camera.conf` asks for — including undoing a different network
//! that something else configured — but if that network does not come up it
//! rolls back to the last configuration that was *observed* working, rather
//! than stranding the camera on a network nobody can reach it from.
//!
//! Every decision is made by [`reconcile::decide`], a pure function over
//! plain values, so the whole policy is unit-tested on the host without a
//! radio.
//!
//! When restarting the station is not enough — the SSV6x5x USB driver can
//! wedge under heavy traffic — [`recovery`] escalates to reloading the driver
//! and power-cycling the module (on the AV100, GPIO 98), behind a trait so
//! those sequences are unit-tested too. Interface, driver and power GPIO come
//! from the board profile ([`hardware`]).

pub mod hardware;
pub mod net;
pub mod reconcile;
pub mod recovery;
pub mod settings;
pub mod system;
pub mod wpa;
