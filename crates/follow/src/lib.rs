//! PTZ follow movement: keeps a moving subject centred using the ISP motion
//! grid.
//!
//! The AV100 has no NPU and a soft-float ARM926, but its ISP already reduces
//! each frame to a 32×24 grid of motion energies (`video-proto::motion`,
//! served by `camerad`). The tracker works only on that grid, which costs
//! microseconds per sample, and moves the head through the `ptz` socket
//! (`ptz-proto`), since `onvif` owns the motors. It is a separate service so
//! the supervisor can restart it without touching video or PTZ (design
//! option B).
//!
//! The logic is split so that everything decisive is pure and host-tested:
//!
//! - [`detect`]: integer-only detector, grid → persistent target
//!   (FOL-04..08).
//! - [`control`]: state machine, target + PTZ status + time → one action
//!   (FOL-09..13).
//!
//! The service around them:
//!
//! - [`store`]: the switch across restarts (`etc/follow.json`).
//! - [`server`]: the control socket (`/tmp/kamoder-follow.sock`) and the
//!   state shared with the loop.
//! - [`runtime`]: the loop, motion socket in, PTZ socket out, with
//!   reconnection.

pub mod control;
pub mod detect;
pub mod runtime;
pub mod server;
pub mod store;

#[cfg(test)]
pub(crate) mod test_support;

/// Registers the descriptions of every metric this crate emits.
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge};
    describe_gauge!(
        "kamoder_follow_state",
        "1 for the current follow state (label state: idle, tracking, settling, returning, paused, disabled, unavailable)"
    );
    describe_gauge!("kamoder_follow_enabled", "1 while following is switched on");
    describe_counter!(
        "kamoder_follow_moves_total",
        "Moves (goto or home) the PTZ owner accepted from the tracker"
    );
    describe_counter!(
        "kamoder_follow_targets_total",
        "Targets acquired (a persistent blob after a grid without one)"
    );
    describe_counter!(
        "kamoder_follow_ptz_errors_total",
        "Failed or refused requests on the PTZ socket"
    );
    describe_counter!(
        "kamoder_follow_motion_errors_total",
        "Undecodable or over-long lines on the motion socket"
    );
    describe_counter!(
        "kamoder_follow_motion_reconnects_total",
        "Motion socket reconnection attempts"
    );
    describe_counter!(
        "kamoder_follow_socket_requests_total",
        "Requests on the follow socket, by op and result"
    );
    describe_counter!(
        "kamoder_follow_socket_refused_total",
        "Follow socket connections refused because all slots were busy"
    );
}
