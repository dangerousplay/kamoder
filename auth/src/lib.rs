//! Authentication shared by the camera's network services.
//!
//! - [`credentials`] — the `sshd` passwd file (`user:pbkdf2-sha256:…`).
//! - [`web_users`] — the `web` users file: the same hash plus a role
//!   and a generation counter, with atomic rewrites and a last-admin guard.
//! - [`jwt`] — HS256 session tokens and their key file.
//! - [`onvif_secret`] — `etc/onvif.secret`, the plaintext `user:password`
//!   account that ONVIF WS-UsernameToken and RTSP Digest both need.
//! - [`atomic`] — crash-safe file replacement, [`perms`] — the "is this secret
//!   readable by others" check that knows FAT has no modes, [`hex`].
//!
//! Everything is built on `ring` only: aws-lc, the usual alternative backend,
//! does not build for the ARMv5TE target, and a single crypto stack keeps the
//! binaries small on a 32 MB device. Password checks are PBKDF2 at 20 000
//! iterations — about a second on the 400 MHz ARM926EJ-S — so they happen
//! once per login; per-request checks are an HMAC and a table lookup.
//!
//! None of this touches the vendor SDK, so it is unit-tested on the host.

pub mod atomic;
pub mod credentials;
pub mod hex;
pub mod jwt;
pub mod onvif_secret;
mod password;
pub mod perms;
pub mod web_users;

pub use credentials::{hash_password, Credentials};
pub use jwt::{load_or_create_key, Claims, Jwt};
pub use password::DEFAULT_ITERATIONS;
pub use web_users::{Role, UserSummary, WebUsers};
