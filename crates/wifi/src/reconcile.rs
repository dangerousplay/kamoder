//! The reconcile policy: observed state + desired state → one action.
//!
//! Pure: no I/O, and time is passed in, so every scenario — a foreign network
//! configured behind our back, a typo in the password, a router that went
//! away — is a unit test rather than an afternoon with a camera.
//!
//! The rules, in priority order:
//!
//! 1. **Remember what works — first.** A healthy link records its
//!    configuration as last-known-good, persistently, *before* anything moves
//!    the camera off it. Otherwise the very first switch to a new network
//!    would have nothing to roll back to.
//! 2. **Converge.** If the configured network is not the desired one, write
//!    the desired one. This is what undoes a network someone else configured.
//!    A configured network that is not up *yet* first gets the grace period,
//!    so that a working one is seen working — and recorded — before it is
//!    replaced.
//! 3. **Give it time.** An unhealthy link gets a grace period before anything
//!    is touched — association plus DHCP takes several seconds, and the
//!    firmware may still be doing it at boot.
//! 4. **Retry with backoff, escalating.** Doubling the wait each time (capped):
//!    first a gentle `Renew` while still associated, then full restarts, and
//!    — on a network known to work, where the radio rather than the network
//!    is the suspect — a driver reload and a USB power cycle of the module.
//!    The ladder repeats for as long as the link stays down.
//! 5. **Never strand the camera.** After `max_attempts` failed retries, if a
//!    different last-known-good network exists, roll back to it and hold the
//!    desired network off for `rollback_hold` before trying it again.
//!
//! Health itself is debounced: a gateway that misses one ARP probe on an
//! otherwise up link is tolerated until it misses `gateway_failures` in a row.
//! Losing the association or the address is not debounced.

use crate::wpa::Network;
use std::time::{Duration, Instant};

/// Everything observed in one pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// The network in the config file wpa_supplicant reads.
    pub configured: Option<Network>,
    /// wpa_supplicant reports `COMPLETED`.
    pub associated: bool,
    /// SSID wpa_supplicant is actually on.
    pub associated_ssid: Option<String>,
    /// wlan0 has an IPv4 address.
    pub has_address: bool,
    /// `None`: no default route to test, or not tested.
    pub gateway_reachable: Option<bool>,
}

impl Observed {
    /// Up, on the configured network, addressed, and the gateway answers.
    pub fn healthy(&self) -> bool {
        let Some(configured) = &self.configured else {
            return false;
        };
        self.associated
            && self.associated_ssid.as_deref() == Some(configured.ssid.as_str())
            && self.has_address
            && self.gateway_reachable != Some(false)
    }

    /// wpa_supplicant is `COMPLETED` on the configured SSID — whatever DHCP
    /// and the gateway say. The supplicant is worth keeping.
    pub fn associated_on_configured(&self) -> bool {
        let Some(configured) = &self.configured else {
            return false;
        };
        self.associated && self.associated_ssid.as_deref() == Some(configured.ssid.as_str())
    }

    /// Up in every respect except that the gateway did not answer: the one
    /// failure that is debounced.
    pub fn gateway_silent(&self) -> bool {
        self.associated_on_configured() && self.has_address && self.gateway_reachable == Some(false)
    }
}

/// Tunables.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub grace: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    pub max_attempts: u32,
    pub rollback_hold: Duration,
    /// Consecutive gateway misses on an otherwise up link before it counts
    /// as unhealthy. 0 and 1 both mean "the first miss".
    pub gateway_failures: u32,
    /// Failed full restarts before a driver reset. 0: never touch the driver
    /// or the module's power.
    pub driver_reset_after: u32,
    /// Failed driver resets before a USB power cycle. 0: never cycle power.
    pub power_cycle_after: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(45),
            backoff_initial: Duration::from_secs(10),
            backoff_max: Duration::from_secs(300),
            max_attempts: 4,
            rollback_hold: Duration::from_secs(30 * 60),
            gateway_failures: 3,
            driver_reset_after: 2,
            power_cycle_after: 1,
        }
    }
}

/// The longest wait between two attempts, whatever `backoff_max` says: the
/// camera has no other way back onto the network, so it must keep trying.
pub const BACKOFF_CEILING: Duration = Duration::from_secs(300);

impl Policy {
    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.backoff_initial
            .saturating_mul(factor)
            .min(self.backoff_max)
    }
}

/// What the loop should do now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Healthy, or waiting out the grace period or a backoff.
    Wait,
    /// Healthy and the configuration differs from the recorded good one.
    RecordGood(Network),
    /// Write this network to the config and reassociate.
    Apply(Network),
    /// Keep wpa_supplicant, ask it to reassociate, and renew the DHCP lease.
    /// The gentle first step while still associated.
    Renew,
    /// Reassociate with the configuration as it is.
    Reassociate,
    /// Reload the Wi-Fi driver module, then restart the station.
    ResetDriver,
    /// Power-cycle the USB Wi-Fi module (GPIO 98), reload the driver, then
    /// restart the station.
    PowerCycle,
    /// Give up on the current network and restore this one.
    RollBack(Network),
}

/// State carried between passes.
#[derive(Debug, Clone, Default)]
pub struct Memory {
    pub unhealthy_since: Option<Instant>,
    /// Reassociations since the link was last healthy.
    pub attempts: u32,
    pub next_attempt_at: Option<Instant>,
    /// Persisted by the caller; loaded at startup.
    pub last_good: Option<Network>,
    /// While set and in the future, the desired network is not re-applied.
    /// A desired network that failed and was rolled back from, and until when
    /// it is not re-applied. Only *that* network is held: editing camera.conf
    /// to a different one takes effect immediately.
    pub held: Option<(Network, Instant)>,
    /// Consecutive observations with the gateway silent on an up link.
    pub gateway_failures: u32,
    /// `Renew` already tried in this unhealthy episode.
    pub renewed: bool,
    /// Full restarts since the last hardware step (or since healthy).
    pub failed_restarts: u32,
    /// Driver resets since the last power cycle (or since healthy).
    pub failed_driver_resets: u32,
}

impl Memory {
    /// Updates the health bookkeeping and returns the debounced health. Call
    /// before [`decide`] each pass.
    pub fn observe(&mut self, observed: &Observed, policy: &Policy, now: Instant) -> bool {
        if observed.gateway_silent() {
            self.gateway_failures = self.gateway_failures.saturating_add(1);
        } else {
            self.gateway_failures = 0;
        }
        let healthy = self.healthy(observed, policy);
        if observed.healthy() {
            // Only a link proven end to end resets the ladder. A tolerated
            // gateway miss does not: a wedged radio that associates but moves
            // no traffic would otherwise reset its own escalation forever.
            self.unhealthy_since = None;
            self.attempts = 0;
            self.next_attempt_at = None;
            self.renewed = false;
            self.failed_restarts = 0;
            self.failed_driver_resets = 0;
        } else if !healthy {
            self.unhealthy_since.get_or_insert(now);
        }
        healthy
    }

    /// Health as the policy sees it: strict, except that a gateway that
    /// stopped answering on an otherwise up link is given
    /// `policy.gateway_failures` observations before it counts.
    pub fn healthy(&self, observed: &Observed, policy: &Policy) -> bool {
        observed.healthy()
            || (observed.gateway_silent() && self.gateway_failures < policy.gateway_failures)
    }

    /// Records that `action` was carried out.
    ///
    /// `failing` is the configured network at the time of the decision — for
    /// a rollback, the one being given up on.
    pub fn performed(
        &mut self,
        action: &Action,
        failing: Option<&Network>,
        policy: &Policy,
        now: Instant,
    ) {
        match action {
            Action::Wait => {}
            Action::RecordGood(network) => self.last_good = Some(network.clone()),
            Action::Apply(_) => {
                // A new network gets a fresh grace period and retry budget.
                self.attempts = 0;
                self.renewed = false;
                self.next_attempt_at = None;
                self.unhealthy_since = Some(now);
            }
            Action::Renew => {
                // Not counted towards the rollback budget: the network is
                // associated, so it is not the network that is wrong.
                self.renewed = true;
                self.next_attempt_at = Some(now + policy.backoff_initial);
            }
            Action::Reassociate => {
                self.attempts += 1;
                self.failed_restarts += 1;
                self.next_attempt_at = Some(now + policy.backoff(self.attempts));
            }
            Action::ResetDriver => {
                self.attempts += 1;
                self.failed_restarts = 0;
                self.failed_driver_resets += 1;
                self.next_attempt_at = Some(now + policy.backoff(self.attempts));
            }
            Action::PowerCycle => {
                // Top of the ladder: start it again from the bottom.
                self.attempts += 1;
                self.failed_restarts = 0;
                self.failed_driver_resets = 0;
                self.next_attempt_at = Some(now + policy.backoff(self.attempts));
            }
            Action::RollBack(_) => {
                self.attempts = 0;
                self.renewed = false;
                self.next_attempt_at = None;
                self.unhealthy_since = Some(now);
                self.held = failing.map(|network| (network.clone(), now + policy.rollback_hold));
            }
        }
    }

    /// The next rung of the ladder. See [`decide`], rule 4.
    fn retry(&self, observed: &Observed, policy: &Policy) -> Action {
        if !self.renewed && self.attempts == 0 && observed.associated_on_configured() {
            return Action::Renew;
        }
        // The radio is only the suspect on a network that is known to work
        // (or when nothing is known). A network that has never been seen
        // working is more likely just wrong — a typo, out of range — and
        // the rollback deals with that, without reloading drivers first.
        let trusted = self.last_good.is_none() || observed.configured == self.last_good;
        if trusted && policy.driver_reset_after > 0 {
            if policy.power_cycle_after > 0 && self.failed_driver_resets >= policy.power_cycle_after
            {
                return Action::PowerCycle;
            }
            if self.failed_restarts >= policy.driver_reset_after {
                return Action::ResetDriver;
            }
        }
        Action::Reassociate
    }

    fn holding(&self, desired: &Network, now: Instant) -> bool {
        self.held
            .as_ref()
            .is_some_and(|(network, until)| network == desired && now < *until)
    }
}

/// Chooses the next action. See the module docs for the rules.
pub fn decide(
    desired: Option<&Network>,
    observed: &Observed,
    memory: &Memory,
    policy: &Policy,
    now: Instant,
) -> Action {
    let healthy = memory.healthy(observed, policy);

    // 1. Healthy on something not yet recorded: record it before anything
    //    else, so there is always a way back. Strictly healthy: a link whose
    //    gateway is being given the benefit of the doubt is not proven.
    if observed.healthy() {
        if let Some(configured) = &observed.configured {
            if memory.last_good.as_ref() != Some(configured) {
                return Action::RecordGood(configured.clone());
            }
        }
    }

    // 2. Converge on the desired network, unless it was just rolled back
    //    from and is being held off — and not before the configured network
    //    has had its grace period. A network that was *just* switched to is
    //    usually still associating; replacing it at once means it is never
    //    seen working, never recorded, and if the desired network then fails
    //    there is nothing to roll back to. Found by the qemu tests: the camera
    //    sat offline retrying a dead network next to a working one.
    if let Some(desired) = desired {
        let unproven = !healthy
            && memory
                .unhealthy_since
                .is_some_and(|since| now.saturating_duration_since(since) < policy.grace);
        if observed.configured.as_ref() != Some(desired)
            && !memory.holding(desired, now)
            && !unproven
        {
            return Action::Apply(desired.clone());
        }
    }

    if healthy {
        return Action::Wait;
    }

    // 3. Grace period. Not for a gateway that went silent on an up link:
    //    the guard has already waited, and nothing is still being set up.
    let since = memory.unhealthy_since.unwrap_or(now);
    if !observed.gateway_silent() && now.saturating_duration_since(since) < policy.grace {
        return Action::Wait;
    }

    // 5. Retries exhausted: fall back to what is known to work, if that is
    //    something other than what is failing now.
    if memory.attempts >= policy.max_attempts {
        if let Some(good) = &memory.last_good {
            if observed.configured.as_ref() != Some(good) {
                return Action::RollBack(good.clone());
            }
        }
    }

    // 4. Retry with backoff, one rung of the ladder at a time. Also the path
    //    when there is nothing to roll back to: keep trying, slowly, since
    //    there is no better option.
    match memory.next_attempt_at {
        Some(at) if now < at => Action::Wait,
        _ => memory.retry(observed, policy),
    }
}

/// Where an action sits on the recovery ladder, for the
/// `kamoder_wifi_escalation_level` gauge. `None` for actions that are not
/// a recovery step.
pub fn escalation_level(action: &Action) -> Option<u8> {
    match action {
        Action::Renew => Some(1),
        Action::Reassociate => Some(2),
        Action::ResetDriver => Some(3),
        Action::PowerCycle => Some(4),
        Action::Wait | Action::RecordGood(_) | Action::Apply(_) | Action::RollBack(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(ssid: &str) -> Network {
        Network {
            ssid: ssid.into(),
            psk: format!("{ssid}-password"),
        }
    }

    fn healthy_on(network: &Network) -> Observed {
        Observed {
            configured: Some(network.clone()),
            associated: true,
            associated_ssid: Some(network.ssid.clone()),
            has_address: true,
            gateway_reachable: Some(true),
        }
    }

    fn down_on(network: &Network) -> Observed {
        Observed {
            configured: Some(network.clone()),
            ..Observed::default()
        }
    }

    fn policy() -> Policy {
        Policy {
            grace: Duration::from_secs(30),
            backoff_initial: Duration::from_secs(10),
            backoff_max: Duration::from_secs(80),
            max_attempts: 3,
            rollback_hold: Duration::from_secs(600),
            gateway_failures: 3,
            driver_reset_after: 2,
            power_cycle_after: 1,
        }
    }

    /// Drives the loop the way `main` does: observe, decide, perform.
    fn step(
        desired: Option<&Network>,
        observed: &Observed,
        memory: &mut Memory,
        now: Instant,
    ) -> Action {
        step_with(&policy(), desired, observed, memory, now)
    }

    fn step_with(
        policy: &Policy,
        desired: Option<&Network>,
        observed: &Observed,
        memory: &mut Memory,
        now: Instant,
    ) -> Action {
        memory.observe(observed, policy, now);
        let action = decide(desired, observed, memory, policy, now);
        memory.performed(&action, observed.configured.as_ref(), policy, now);
        action
    }

    #[test]
    fn health_needs_every_signal() {
        let home = net("home");
        assert!(healthy_on(&home).healthy());
        assert!(!Observed {
            associated: false,
            ..healthy_on(&home)
        }
        .healthy());
        assert!(!Observed {
            has_address: false,
            ..healthy_on(&home)
        }
        .healthy());
        assert!(!Observed {
            gateway_reachable: Some(false),
            ..healthy_on(&home)
        }
        .healthy());
        // No default route to test is not a failure on its own.
        assert!(Observed {
            gateway_reachable: None,
            ..healthy_on(&home)
        }
        .healthy());
        // Associated, but to something other than the configured network.
        assert!(!Observed {
            associated_ssid: Some("other".into()),
            ..healthy_on(&home)
        }
        .healthy());
    }

    #[test]
    fn a_foreign_network_is_replaced_with_the_desired_one() {
        let (home, foreign) = (net("home"), net("foreign"));
        let mut memory = Memory::default();
        let now = Instant::now();
        // A *working* foreign network is recorded first — it is the way back
        // if `home` fails — and then replaced: the desired one wins.
        let action = step(Some(&home), &healthy_on(&foreign), &mut memory, now);
        assert_eq!(action, Action::RecordGood(foreign.clone()));
        let action = step(Some(&home), &healthy_on(&foreign), &mut memory, now);
        assert_eq!(action, Action::Apply(home));
    }

    #[test]
    fn healthy_desired_network_is_recorded_once() {
        let home = net("home");
        let mut memory = Memory::default();
        let t0 = Instant::now();
        assert_eq!(
            step(Some(&home), &healthy_on(&home), &mut memory, t0),
            Action::RecordGood(home.clone())
        );
        assert_eq!(memory.last_good, Some(home.clone()));
        assert_eq!(
            step(Some(&home), &healthy_on(&home), &mut memory, t0),
            Action::Wait
        );
    }

    #[test]
    fn no_desired_network_adopts_whatever_works() {
        let manual = net("manual");
        let mut memory = Memory::default();
        let t0 = Instant::now();
        assert_eq!(
            step(None, &healthy_on(&manual), &mut memory, t0),
            Action::RecordGood(manual.clone())
        );
        // And never writes a config of its own.
        assert_eq!(
            step(None, &healthy_on(&manual), &mut memory, t0),
            Action::Wait
        );
    }

    #[test]
    fn grace_period_then_backoff() {
        let home = net("home");
        let mut memory = Memory {
            last_good: Some(home.clone()),
            ..Memory::default()
        };
        let t0 = Instant::now();
        let down = down_on(&home);

        assert_eq!(step(Some(&home), &down, &mut memory, t0), Action::Wait);
        assert_eq!(
            step(
                Some(&home),
                &down,
                &mut memory,
                t0 + Duration::from_secs(29)
            ),
            Action::Wait
        );
        let t1 = t0 + Duration::from_secs(30);
        assert_eq!(
            step(Some(&home), &down, &mut memory, t1),
            Action::Reassociate
        );
        // 10 s backoff after the first attempt, 20 s after the second.
        assert_eq!(
            step(Some(&home), &down, &mut memory, t1 + Duration::from_secs(9)),
            Action::Wait
        );
        let t2 = t1 + Duration::from_secs(10);
        assert_eq!(
            step(Some(&home), &down, &mut memory, t2),
            Action::Reassociate
        );
        assert_eq!(
            step(
                Some(&home),
                &down,
                &mut memory,
                t2 + Duration::from_secs(19)
            ),
            Action::Wait
        );
        assert_eq!(
            step(
                Some(&home),
                &down,
                &mut memory,
                t2 + Duration::from_secs(20)
            ),
            // Two failed restarts on a network known to work: the driver.
            Action::ResetDriver
        );
    }

    #[test]
    fn backoff_is_capped() {
        let p = policy();
        assert_eq!(p.backoff(1), Duration::from_secs(10));
        assert_eq!(p.backoff(4), Duration::from_secs(80));
        assert_eq!(p.backoff(40), Duration::from_secs(80));
    }

    /// The scenario this crate exists for: the desired network is wrong (bad
    /// password, out of range, typo) and the camera must not be stranded.
    #[test]
    fn a_failing_desired_network_rolls_back_to_last_good_and_retries_later() {
        let (good, broken) = (net("good"), net("broken"));
        let mut memory = Memory {
            last_good: Some(good.clone()),
            ..Memory::default()
        };
        let mut now = Instant::now();

        // Healthy on the good network; camera.conf now asks for `broken`.
        assert_eq!(
            step(Some(&broken), &healthy_on(&good), &mut memory, now),
            Action::Apply(broken.clone())
        );

        // `broken` never comes up: grace, then three spaced retries.
        let down = down_on(&broken);
        let mut reassociations = 0;
        let rolled_back = loop {
            now += Duration::from_secs(5);
            match step(Some(&broken), &down, &mut memory, now) {
                Action::Wait => {}
                Action::Reassociate => reassociations += 1,
                Action::RollBack(to) => break to,
                other => panic!("unexpected {other:?}"),
            }
            assert!(reassociations <= 3, "never rolled back");
        };
        assert_eq!(reassociations, 3);
        assert_eq!(rolled_back, good);

        // Back on the good network, the desired one is held off...
        now += Duration::from_secs(20);
        assert_eq!(
            step(Some(&broken), &healthy_on(&good), &mut memory, now),
            Action::Wait
        );
        now += Duration::from_secs(500);
        assert_eq!(
            step(Some(&broken), &healthy_on(&good), &mut memory, now),
            Action::Wait
        );

        // ...until the hold expires, when it is tried again.
        now += Duration::from_secs(100);
        assert_eq!(
            step(Some(&broken), &healthy_on(&good), &mut memory, now),
            Action::Apply(broken)
        );
    }

    /// First boot with a new `wifi_ssid` and no record yet: the current,
    /// working network must be recorded before the switch, or a bad password
    /// in camera.conf would leave nothing to roll back to.
    #[test]
    fn first_boot_switch_can_still_roll_back() {
        let (current, typo) = (net("current"), net("typo"));
        let mut memory = Memory::default();
        let mut now = Instant::now();
        assert_eq!(
            step(Some(&typo), &healthy_on(&current), &mut memory, now),
            Action::RecordGood(current.clone())
        );
        assert_eq!(
            step(Some(&typo), &healthy_on(&current), &mut memory, now),
            Action::Apply(typo.clone())
        );
        let down = down_on(&typo);
        let restored = loop {
            now += Duration::from_secs(5);
            match step(Some(&typo), &down, &mut memory, now) {
                Action::RollBack(to) => break to,
                Action::Wait | Action::Reassociate => {}
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(restored, current);
    }

    /// The hold is for the network that failed, not for whatever camera.conf
    /// says next: fixing a typo by switching to another network must not wait
    /// out the hold.
    #[test]
    fn a_different_desired_network_is_not_held() {
        let (good, broken, other) = (net("good"), net("broken"), net("other"));
        let mut memory = Memory {
            last_good: Some(good.clone()),
            ..Memory::default()
        };
        let mut now = Instant::now();
        step(Some(&broken), &healthy_on(&good), &mut memory, now);
        let down = down_on(&broken);
        loop {
            now += Duration::from_secs(5);
            if let Action::RollBack(_) = step(Some(&broken), &down, &mut memory, now) {
                break;
            }
        }
        now += Duration::from_secs(5);
        // `broken` is held...
        assert_eq!(
            step(Some(&broken), &healthy_on(&good), &mut memory, now),
            Action::Wait
        );
        // ...but a new choice is applied straight away.
        assert_eq!(
            step(Some(&other), &healthy_on(&good), &mut memory, now),
            Action::Apply(other)
        );
    }

    /// Switched by someone else to a network that is still associating: it
    /// gets the grace period, is seen working, recorded — and only then
    /// replaced by the desired one.
    #[test]
    fn a_just_switched_network_is_given_time_to_prove_itself() {
        let (home, other) = (net("home"), net("other"));
        let mut memory = Memory {
            last_good: Some(home.clone()),
            ..Memory::default()
        };
        let t0 = Instant::now();
        // Still associating.
        assert_eq!(
            step(Some(&home), &down_on(&other), &mut memory, t0),
            Action::Wait
        );
        assert_eq!(
            step(
                Some(&home),
                &down_on(&other),
                &mut memory,
                t0 + Duration::from_secs(10)
            ),
            Action::Wait
        );
        // Up: recorded first, then replaced.
        let t1 = t0 + Duration::from_secs(15);
        assert_eq!(
            step(Some(&home), &healthy_on(&other), &mut memory, t1),
            Action::RecordGood(other.clone())
        );
        assert_eq!(
            step(Some(&home), &healthy_on(&other), &mut memory, t1),
            Action::Apply(home)
        );
    }

    /// The scenario the grace period exists for: the desired AP is down when
    /// someone moves the camera to a working network. Without the grace the
    /// working network was replaced unseen and the camera stranded.
    #[test]
    fn desired_ap_down_after_an_external_switch_rolls_back_to_the_switched_network() {
        let (home, other) = (net("home"), net("other"));
        let mut memory = Memory {
            last_good: Some(home.clone()),
            ..Memory::default()
        };
        let mut now = Instant::now();
        step(Some(&home), &down_on(&other), &mut memory, now);
        now += Duration::from_secs(10);
        step(Some(&home), &healthy_on(&other), &mut memory, now); // recorded
        assert_eq!(
            step(Some(&home), &healthy_on(&other), &mut memory, now),
            Action::Apply(home.clone())
        );
        let restored = loop {
            now += Duration::from_secs(5);
            match step(Some(&home), &down_on(&home), &mut memory, now) {
                Action::RollBack(to) => break to,
                Action::Wait | Action::Reassociate => {}
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(restored, other);
    }

    /// A configured network that stays broken past the grace period is
    /// replaced — keeping it would only keep the camera offline.
    #[test]
    fn a_broken_configured_network_is_replaced_after_the_grace() {
        let (home, broken) = (net("home"), net("broken"));
        let mut memory = Memory::default();
        let t0 = Instant::now();
        assert_eq!(
            step(Some(&home), &down_on(&broken), &mut memory, t0),
            Action::Wait
        );
        assert_eq!(
            step(
                Some(&home),
                &down_on(&broken),
                &mut memory,
                t0 + Duration::from_secs(30)
            ),
            Action::Apply(home)
        );
    }

    #[test]
    fn someone_else_rewrites_the_config_with_no_desired_network() {
        // camera.conf leaves Wi-Fi alone, and /ipc/ipc (say) rewrites it to a
        // network that does not work. The reconciler restores the last good.
        let (manual, clobbered) = (net("manual"), net("clobbered"));
        let mut memory = Memory::default();
        let mut now = Instant::now();
        step(None, &healthy_on(&manual), &mut memory, now);
        assert_eq!(memory.last_good, Some(manual.clone()));

        let down = down_on(&clobbered);
        let restored = loop {
            now += Duration::from_secs(5);
            match step(None, &down, &mut memory, now) {
                Action::RollBack(to) => break to,
                Action::Wait | Action::Reassociate => {}
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(restored, manual);
    }

    #[test]
    fn nothing_to_roll_back_to_keeps_retrying() {
        let home = net("home");
        let mut memory = Memory::default();
        let mut now = Instant::now();
        let down = down_on(&home);
        let mut retries = 0;
        for _ in 0..200 {
            now += Duration::from_secs(5);
            match step(Some(&home), &down, &mut memory, now) {
                Action::Reassociate | Action::ResetDriver | Action::PowerCycle => retries += 1,
                Action::Wait => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        // 1000 s: grace, then 10+20+40+80+80... — keeps going, slowly.
        assert!((10..=16).contains(&retries), "{retries}");
    }

    #[test]
    fn last_good_equal_to_the_failing_network_is_not_a_rollback() {
        let home = net("home");
        let mut memory = Memory {
            last_good: Some(home.clone()),
            ..Memory::default()
        };
        let mut now = Instant::now();
        let down = down_on(&home);
        for _ in 0..100 {
            now += Duration::from_secs(5);
            let action = step(Some(&home), &down, &mut memory, now);
            assert!(
                matches!(
                    action,
                    Action::Wait | Action::Reassociate | Action::ResetDriver | Action::PowerCycle
                ),
                "{action:?}"
            );
        }
    }

    #[test]
    fn recovery_resets_the_retry_budget() {
        let home = net("home");
        let mut memory = Memory::default();
        let t0 = Instant::now();
        step(Some(&home), &down_on(&home), &mut memory, t0);
        step(
            Some(&home),
            &down_on(&home),
            &mut memory,
            t0 + Duration::from_secs(30),
        );
        assert_eq!(memory.attempts, 1);
        step(
            Some(&home),
            &healthy_on(&home),
            &mut memory,
            t0 + Duration::from_secs(40),
        );
        assert_eq!(memory.attempts, 0);
        assert_eq!(memory.unhealthy_since, None);
    }

    fn gateway_silent_on(network: &Network) -> Observed {
        Observed {
            gateway_reachable: Some(false),
            ..healthy_on(network)
        }
    }

    /// WFR-01: one lost ARP probe on a working link is not an outage.
    #[test]
    fn a_single_gateway_miss_is_tolerated() {
        let home = net("home");
        let mut memory = Memory {
            last_good: Some(home.clone()),
            ..Memory::default()
        };
        let mut now = Instant::now();
        for _ in 0..10 {
            // Miss, miss, answer — never three in a row.
            for observed in [
                gateway_silent_on(&home),
                gateway_silent_on(&home),
                healthy_on(&home),
            ] {
                now += Duration::from_secs(10);
                assert!(memory.observe(&observed, &policy(), now));
                assert_eq!(
                    decide(Some(&home), &observed, &memory, &policy(), now),
                    Action::Wait
                );
            }
        }
        assert_eq!(memory.unhealthy_since, None);
    }

    /// WFR-01 + WFR-02: three misses in a row make it unhealthy, and the
    /// first remedy is the gentle one — straight away, without the grace.
    #[test]
    fn a_silent_gateway_is_renewed_after_the_guard_without_a_grace() {
        let home = net("home");
        let mut memory = Memory {
            last_good: Some(home.clone()),
            ..Memory::default()
        };
        let silent = gateway_silent_on(&home);
        let t0 = Instant::now();
        assert_eq!(step(Some(&home), &silent, &mut memory, t0), Action::Wait);
        assert_eq!(
            step(
                Some(&home),
                &silent,
                &mut memory,
                t0 + Duration::from_secs(10)
            ),
            Action::Wait
        );
        assert_eq!(
            step(
                Some(&home),
                &silent,
                &mut memory,
                t0 + Duration::from_secs(20)
            ),
            Action::Renew
        );
        assert_eq!(memory.attempts, 0, "renew is not a counted attempt");
        // Then the restarts, after the first backoff.
        assert_eq!(
            step(
                Some(&home),
                &silent,
                &mut memory,
                t0 + Duration::from_secs(25)
            ),
            Action::Wait
        );
        assert_eq!(
            step(
                Some(&home),
                &silent,
                &mut memory,
                t0 + Duration::from_secs(30)
            ),
            Action::Reassociate
        );
    }

    #[test]
    fn a_guard_of_one_is_the_old_behaviour() {
        let home = net("home");
        let policy = Policy {
            gateway_failures: 1,
            ..policy()
        };
        let mut memory = Memory::default();
        assert!(!memory.observe(&gateway_silent_on(&home), &policy, Instant::now()));
    }

    /// Association loss is not debounced, and is not renewed: there is no
    /// association to keep.
    #[test]
    fn association_loss_skips_the_guard_and_the_renew() {
        let home = net("home");
        let mut memory = Memory {
            last_good: Some(home.clone()),
            ..Memory::default()
        };
        let t0 = Instant::now();
        assert!(!memory.observe(&down_on(&home), &policy(), t0));
        assert_eq!(
            decide(
                Some(&home),
                &down_on(&home),
                &memory,
                &policy(),
                t0 + Duration::from_secs(30)
            ),
            Action::Reassociate
        );
    }

    /// The field incident: a wedged radio that sometimes associates but
    /// never passes traffic. Tolerated misses in between must not reset the
    /// ladder, or it never gets past the restarts.
    #[test]
    fn tolerated_misses_do_not_reset_the_ladder() {
        let home = net("home");
        let mut memory = Memory {
            last_good: Some(home.clone()),
            ..Memory::default()
        };
        let mut now = Instant::now();
        let mut seen = Vec::new();
        for i in 0..200 {
            now += Duration::from_secs(5);
            // Alternates between "not associated" and "associated, silent".
            let observed = if i % 3 == 0 {
                gateway_silent_on(&home)
            } else {
                down_on(&home)
            };
            let action = step(Some(&home), &observed, &mut memory, now);
            if action != Action::Wait {
                seen.push(action);
            }
        }
        assert!(seen.contains(&Action::ResetDriver), "{seen:?}");
        assert!(seen.contains(&Action::PowerCycle), "{seen:?}");
    }

    /// WFR-03: the order of the rungs, and that the ladder starts again
    /// after a power cycle.
    #[test]
    fn the_ladder_escalates_and_cycles() {
        let home = net("home");
        let mut memory = Memory {
            last_good: Some(home.clone()),
            ..Memory::default()
        };
        let mut now = Instant::now();
        // Associated but silent, then gone entirely after the renew.
        let mut observed = gateway_silent_on(&home);
        let mut seen = Vec::new();
        while seen.len() < 9 {
            now += Duration::from_secs(5);
            let action = step(Some(&home), &observed, &mut memory, now);
            if action != Action::Wait {
                seen.push(action);
                observed = down_on(&home);
            }
        }
        use Action::*;
        assert_eq!(
            seen,
            [
                Renew,
                Reassociate,
                Reassociate,
                ResetDriver,
                PowerCycle,
                Reassociate,
                Reassociate,
                ResetDriver,
                PowerCycle
            ]
        );
    }

    #[test]
    fn hardware_steps_can_be_disabled() {
        let home = net("home");
        for (policy, banned) in [
            (
                Policy {
                    driver_reset_after: 0,
                    ..policy()
                },
                [Action::ResetDriver, Action::PowerCycle].as_slice(),
            ),
            (
                Policy {
                    power_cycle_after: 0,
                    ..policy()
                },
                [Action::PowerCycle].as_slice(),
            ),
        ] {
            let mut memory = Memory::default();
            let mut now = Instant::now();
            let mut seen = Vec::new();
            for _ in 0..300 {
                now += Duration::from_secs(5);
                seen.push(step_with(
                    &policy,
                    Some(&home),
                    &down_on(&home),
                    &mut memory,
                    now,
                ));
            }
            for action in banned {
                assert!(!seen.contains(action), "{action:?} with {policy:?}");
            }
        }
    }

    /// A network never seen working is more likely wrong than the radio:
    /// no driver reloads on the way to the rollback. (The VM tests rely on
    /// this sequence.)
    #[test]
    fn an_untrusted_network_rolls_back_without_hardware_steps() {
        let (good, broken) = (net("good"), net("broken"));
        let mut memory = Memory {
            last_good: Some(good.clone()),
            ..Memory::default()
        };
        let mut now = Instant::now();
        step(Some(&broken), &healthy_on(&good), &mut memory, now);
        let mut seen = Vec::new();
        loop {
            now += Duration::from_secs(5);
            let action = step(Some(&broken), &down_on(&broken), &mut memory, now);
            if action != Action::Wait {
                seen.push(action.clone());
            }
            if matches!(action, Action::RollBack(_)) {
                break;
            }
        }
        use Action::*;
        assert_eq!(
            seen,
            [Reassociate, Reassociate, Reassociate, RollBack(good)]
        );
    }

    /// WFR-04: down for ten hours with nowhere to go — it never stops, and
    /// never waits longer than the ceiling between attempts.
    #[test]
    fn retries_forever_within_the_ceiling() {
        let home = net("home");
        let policy = Policy {
            backoff_max: BACKOFF_CEILING,
            ..Policy::default()
        };
        let mut memory = Memory::default();
        let start = Instant::now();
        let mut now = start;
        let mut last = None;
        let mut longest = Duration::ZERO;
        let mut power_cycles = 0;
        while now < start + Duration::from_secs(10 * 3600) {
            now += Duration::from_secs(10);
            let action = step_with(&policy, Some(&home), &down_on(&home), &mut memory, now);
            if action == Action::Wait {
                continue;
            }
            if action == Action::PowerCycle {
                power_cycles += 1;
            }
            if let Some(prev) = last {
                longest = longest.max(now - prev);
            }
            last = Some(now);
        }
        assert!(
            longest <= BACKOFF_CEILING + Duration::from_secs(10),
            "{longest:?}"
        );
        assert!(now - last.unwrap() <= BACKOFF_CEILING + Duration::from_secs(10));
        assert!(power_cycles >= 10, "{power_cycles}");
    }

    #[test]
    fn renew_is_once_per_episode() {
        let home = net("home");
        let mut memory = Memory {
            last_good: Some(home.clone()),
            ..Memory::default()
        };
        let mut now = Instant::now();
        let mut renews = 0;
        for _ in 0..100 {
            now += Duration::from_secs(5);
            if step(Some(&home), &gateway_silent_on(&home), &mut memory, now) == Action::Renew {
                renews += 1;
            }
        }
        assert_eq!(renews, 1);
        // Healthy again: the next episode may renew again.
        step(Some(&home), &healthy_on(&home), &mut memory, now);
        assert!(!memory.renewed);
    }

    #[test]
    fn a_link_only_tolerated_is_never_recorded_as_good() {
        let home = net("home");
        let mut memory = Memory::default();
        assert_eq!(
            step(
                Some(&home),
                &gateway_silent_on(&home),
                &mut memory,
                Instant::now()
            ),
            Action::Wait
        );
        assert_eq!(memory.last_good, None);
    }
}
