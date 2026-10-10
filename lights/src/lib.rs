//! Owner of the camera's lights and speaker amp.
//!
//! One process drives GPIO 19 (IR-cut), 67 (IR LEDs), 68 (white), 48
//! (red/blue) and 37 (speaker amp) through sysfs, like `onvif` owns the PTZ
//! motor: `web` and `onvif` ask it over `/tmp/kamoder-lights.sock`
//! ([`lights_proto`]) instead of writing the pins themselves, so two writers
//! never fight over a light.
//!
//! - [`gpio`]: the sysfs layer and its allowlist. No other GPIO is reachable.
//! - [`control`]: state → levels, in the order that keeps IR light off the
//!   filter, and the safe state (day, lights off, amp muted) on drop.
//! - [`store`]: the user-set state across restarts (`etc/lights.json`).
//! - [`owner`]: the lock, the blink task and the socket server.

pub mod control;
pub mod gpio;
pub mod owner;
pub mod store;

pub use control::Lights;
pub use gpio::{Gpio, Line, Sysfs};
pub use owner::Owner;

/// Registers the descriptions of every metric this crate emits.
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge};
    describe_gauge!(
        "kamoder_lights_night",
        "1 in night mode (IR-cut out, IR LEDs on), 0 in day mode"
    );
    describe_counter!(
        "kamoder_lights_mode_transitions_total",
        "Day/night switches, by the mode switched to"
    );
    describe_gauge!("kamoder_lights_white_on", "1 while the white light is on");
    describe_gauge!(
        "kamoder_lights_alarm",
        "Red/blue lights: 0 off, 1 on, 2 blinking"
    );
    describe_gauge!(
        "kamoder_lights_amp_muted",
        "1 while the speaker amp is muted"
    );
    describe_counter!(
        "kamoder_lights_gpio_errors_total",
        "Failed GPIO writes, by line"
    );
    describe_counter!(
        "kamoder_lights_save_errors_total",
        "Failed writes of the saved light state"
    );
    describe_counter!(
        "kamoder_lights_socket_requests_total",
        "Requests on the lights socket, by op and result"
    );
    describe_counter!(
        "kamoder_lights_socket_refused_total",
        "Lights socket connections refused because all slots were busy"
    );
}
