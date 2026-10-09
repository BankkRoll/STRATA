//! Reference-counted privilege scopes (SPEC §4: enable privileges only
//! while they are needed).
//!
//! Privileges live on the process token, so they are shared by every
//! thread. A plain `PrivilegeGuard` per operation is wrong with concurrent
//! requests: when a scan's guard drops it restores "disabled" while another
//! worker is mid-open. [`PrivilegeScope`] counts holders per privilege and
//! enables on the first, disables on the last.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use strata_win::process::{PrivilegeError, PrivilegeGuard, enable_privilege};

use crate::diag::diag;

/// The privileges the helper keeps after startup (everything else is
/// removed from the token by `drop_privileges`).
///
/// `SeRestorePrivilege` is not kept: deletes run with ordinary
/// administrator access checks, so a file that denies administrators stays
/// undeletable rather than being bypassed.
pub const KEPT_PRIVILEGES: &[&str] = &[
    strata_win::process::SE_BACKUP,
    strata_win::process::SE_MANAGE_VOLUME,
    strata_win::process::SE_CHANGE_NOTIFY,
];

/// Privileges for opening a raw volume and issuing volume FSCTLs.
pub const VOLUME_PRIVILEGES: &[&str] = &[
    strata_win::process::SE_BACKUP,
    strata_win::process::SE_MANAGE_VOLUME,
];

#[derive(Debug, Default)]
struct Slot {
    holders: usize,
    guard: Option<PrivilegeGuard>,
}

static SLOTS: Mutex<Option<HashMap<&'static str, Slot>>> = Mutex::new(None);

fn slots() -> MutexGuard<'static, Option<HashMap<&'static str, Slot>>> {
    SLOTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Holds `names` enabled until dropped.
///
/// Best effort: a privilege the token does not hold (unelevated, or removed)
/// is skipped, and the operation then fails with the OS's own access-denied
/// error, which is what the client should see.
#[derive(Debug)]
#[must_use = "privileges are disabled again when the scope drops"]
pub struct PrivilegeScope {
    names: Vec<&'static str>,
}

impl PrivilegeScope {
    /// Enables every privilege in `names` (reference counted).
    pub fn acquire(names: &[&'static str]) -> Self {
        let mut map = slots();
        let map = map.get_or_insert_with(HashMap::new);
        for &name in names {
            let slot = map.entry(name).or_default();
            if slot.holders == 0 {
                slot.guard = match enable_privilege(name) {
                    Ok(g) => Some(g),
                    Err(PrivilegeError::NotHeld(_)) => None,
                    Err(e) => {
                        diag!("could not enable {name}: {e}");
                        None
                    }
                };
            }
            slot.holders += 1;
        }
        Self {
            names: names.to_vec(),
        }
    }

    /// Whether `name` is currently enabled by some scope.
    #[must_use]
    pub fn is_enabled(name: &str) -> bool {
        slots()
            .as_ref()
            .and_then(|m| m.get(name))
            .is_some_and(|s| s.guard.is_some())
    }

    /// How many scopes hold `name`.
    #[must_use]
    pub fn holders(name: &str) -> usize {
        slots()
            .as_ref()
            .and_then(|m| m.get(name))
            .map_or(0, |s| s.holders)
    }
}

impl Drop for PrivilegeScope {
    fn drop(&mut self) {
        let mut map = slots();
        let Some(map) = map.as_mut() else {
            return;
        };
        for name in &self.names {
            if let Some(slot) = map.get_mut(name) {
                slot.holders = slot.holders.saturating_sub(1);
                if slot.holders == 0 {
                    slot.guard = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_are_reference_counted() {
        // NOTE: a privilege a normal user holds (disabled by default) and
        // that nothing else in this test binary touches.
        const P: &str = "SeTimeZonePrivilege";
        let a = PrivilegeScope::acquire(&[P]);
        let held = PrivilegeScope::is_enabled(P);
        let b = PrivilegeScope::acquire(&[P]);
        assert_eq!(PrivilegeScope::holders(P), 2);
        drop(a);
        assert_eq!(PrivilegeScope::holders(P), 1);
        assert_eq!(PrivilegeScope::is_enabled(P), held);
        drop(b);
        assert_eq!(PrivilegeScope::holders(P), 0);
        assert!(!PrivilegeScope::is_enabled(P));
    }

    #[test]
    fn missing_privileges_are_skipped() {
        let s = PrivilegeScope::acquire(&["SeCreateTokenPrivilege"]);
        assert!(!PrivilegeScope::is_enabled("SeCreateTokenPrivilege"));
        drop(s);
    }
}
