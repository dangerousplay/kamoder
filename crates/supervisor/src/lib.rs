//! Library half of `supervisor`.
//!
//! The supervisor itself is a binary; this library only carries what other
//! crates need to talk to it — the control-socket protocol and a client — so
//! `web` uses the very types the server serialises instead of a copy
//! that could drift.

pub mod control;
