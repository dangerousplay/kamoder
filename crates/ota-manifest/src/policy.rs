//! Device-side install policy (OTA-04), a pure function of the verified
//! manifest and what the device knows about itself.
//!
//! Order (design §4): target → board → seq → security floor → updater
//! version → installable component. The signature and schema were already
//! checked by [`crate::verify`]; [`Policy::check`] re-checks the schema so a
//! `Manifest` built in code cannot skip it.
//!
//! - **seq** is the anti-replay counter: a manifest must be newer than the
//!   highest the device has accepted. `force` (an admin's explicit
//!   reinstall) admits the *same* seq, never an older one.
//! - **security floor**: the effective floor is the higher of the device's
//!   remembered floor (from manifests it accepted before; survives losing
//!   `state.json` through the running slot's own manifest) and the
//!   manifest's own `security_floor`. `force` does not bypass it.
//! - A lower **version** with a higher seq is allowed: that is how the
//!   maintainer ships a deliberate rollback release.
//!   [`Policy::is_downgrade`] lets the UI say so.

use crate::manifest::{compare_versions, ComponentKind, Manifest, SCHEMA};
use crate::Rejection;
use semver::Version;
use std::cmp::Ordering;

/// What the device knows when deciding whether to install a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// This device's board profile name (`board::current()`).
    pub board: String,
    /// The target triple this updater was built for.
    pub target: String,
    /// Version of the running slot.
    pub current_version: Version,
    /// Version of the updater doing the install (compared to `min_updater`).
    pub updater_version: Version,
    /// Highest manifest `seq` accepted so far (0 = none).
    pub highest_seq: u64,
    /// Highest `security_floor` accepted so far (`0.0.0` = none).
    pub security_floor: Version,
    /// Admin asked to reinstall: admits `seq == highest_seq`.
    pub force: bool,
}

impl Policy {
    /// Accepts the manifest or says why not.
    pub fn check(&self, m: &Manifest) -> Result<(), Rejection> {
        if m.schema != SCHEMA {
            return Err(Rejection::Schema { found: m.schema });
        }
        if m.target != self.target {
            return Err(Rejection::Target {
                expected: self.target.clone(),
                found: m.target.clone(),
            });
        }
        if !m.boards.contains(&self.board) {
            return Err(Rejection::Board {
                board: self.board.clone(),
                supported: m.boards.clone(),
            });
        }
        let seq_ok = m.seq > self.highest_seq || (self.force && m.seq == self.highest_seq);
        if !seq_ok {
            return Err(Rejection::Replay {
                seq: m.seq,
                highest: self.highest_seq,
            });
        }
        let floor = match compare_versions(&self.security_floor, &m.security_floor) {
            Ordering::Greater => &self.security_floor,
            _ => &m.security_floor,
        };
        if compare_versions(&m.version, floor) == Ordering::Less {
            return Err(Rejection::BelowFloor {
                version: m.version.clone(),
                floor: floor.clone(),
            });
        }
        if compare_versions(&self.updater_version, &m.min_updater) == Ordering::Less {
            return Err(Rejection::UpdaterTooOld {
                required: m.min_updater.clone(),
                have: self.updater_version.clone(),
            });
        }
        if m.component(&ComponentKind::SdTree).is_none() {
            return Err(Rejection::NoComponent);
        }
        Ok(())
    }

    /// Whether installing `m` goes to a lower version than the running one.
    pub fn is_downgrade(&self, m: &Manifest) -> bool {
        compare_versions(&m.version, &self.current_version) == Ordering::Less
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::sample;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn policy() -> Policy {
        Policy {
            board: "test-board".into(),
            target: "armv5te-unknown-linux-uclibceabi".into(),
            current_version: v("0.1.0"),
            updater_version: v("0.1.0"),
            highest_seq: 100,
            security_floor: v("0.0.0"),
            force: false,
        }
    }

    #[test]
    fn accepts_good_manifest() {
        policy().check(&sample()).unwrap();
        assert!(!policy().is_downgrade(&sample()));
    }

    #[test]
    fn wrong_schema() {
        let mut m = sample();
        m.schema = 2;
        assert!(matches!(
            policy().check(&m),
            Err(Rejection::Schema { found: 2 })
        ));
    }

    #[test]
    fn wrong_target() {
        let mut m = sample();
        m.target = "x86_64-unknown-linux-gnu".into();
        assert!(matches!(policy().check(&m), Err(Rejection::Target { .. })));
    }

    #[test]
    fn wrong_board() {
        let p = Policy {
            board: "other-board".into(),
            ..policy()
        };
        let err = p.check(&sample()).unwrap_err();
        assert!(matches!(&err, Rejection::Board { board, .. } if board == "other-board"));
        assert!(err.to_string().contains("other-board"));
    }

    #[test]
    fn replay_and_force() {
        let mut m = sample();
        m.seq = 100;
        assert!(matches!(
            policy().check(&m),
            Err(Rejection::Replay {
                seq: 100,
                highest: 100
            })
        ));
        let forced = Policy {
            force: true,
            ..policy()
        };
        forced.check(&m).unwrap();
        m.seq = 99;
        assert!(matches!(
            forced.check(&m),
            Err(Rejection::Replay {
                seq: 99,
                highest: 100
            })
        ));
        m.seq = 101;
        policy().check(&m).unwrap();
    }

    #[test]
    fn security_floor() {
        // The manifest's own floor.
        let mut m = sample();
        m.version = v("0.2.0-rc.1");
        m.security_floor = v("0.2.0");
        assert!(matches!(
            policy().check(&m),
            Err(Rejection::BelowFloor { .. })
        ));
        // The device's remembered floor, even with force.
        let mut m = sample();
        m.version = v("0.1.5");
        let p = Policy {
            security_floor: v("0.1.6"),
            force: true,
            highest_seq: m.seq,
            ..policy()
        };
        let err = p.check(&m).unwrap_err();
        assert!(
            matches!(&err, Rejection::BelowFloor { floor, .. } if *floor == v("0.1.6")),
            "{err}"
        );
        // Equal to the floor is fine.
        m.version = v("0.1.6");
        p.check(&m).unwrap();
    }

    #[test]
    fn updater_too_old() {
        let mut m = sample();
        m.min_updater = v("0.3.0");
        assert!(matches!(
            policy().check(&m),
            Err(Rejection::UpdaterTooOld { .. })
        ));
        // Pre-release updater is older than the release it precedes.
        m.min_updater = v("0.3.0");
        let p = Policy {
            updater_version: v("0.3.0-rc.1"),
            ..policy()
        };
        assert!(p.check(&m).is_err());
        let p = Policy {
            updater_version: v("0.3.0"),
            ..policy()
        };
        p.check(&m).unwrap();
    }

    #[test]
    fn no_slot_component() {
        let mut m = sample();
        m.components.retain(|c| c.kind != ComponentKind::SdTree);
        assert!(matches!(policy().check(&m), Err(Rejection::NoComponent)));
    }

    #[test]
    fn downgrade_with_newer_seq_is_allowed_but_flagged() {
        let p = Policy {
            current_version: v("0.3.0"),
            ..policy()
        };
        let m = sample();
        p.check(&m).unwrap();
        assert!(p.is_downgrade(&m));
    }
}
