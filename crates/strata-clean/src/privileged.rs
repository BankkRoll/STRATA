//! Requests the elevated helper accepts.
//!
//! The helper never trusts a path from the client. A
//! [`PrivilegedDeleteRequest`] names the file by `(volume, file reference)`;
//! the helper opens it **by id**, so no path lookup (and no link in a path)
//! is involved, then checks that the object's current path is the one the
//! user saw, that size and mtime match the scan, and that the never-list
//! allows that resolved path. Only then does it delete, by handle, exactly
//! like [`crate::permanent`]. This defeats path-swap and symlink races.
//!
//! [`DelayedDeleteRequest`] schedules delete-on-reboot. Windows deletes by
//! *path* at boot, so it is restricted to plain files with a single link and
//! requires explicit consent.

use std::path::Path;

use serde::{Deserialize, Serialize};
use strata_core::{FileRef, FileTime};

use crate::canon::{CanonicalPath, Root};
use crate::consent::{Consent, ConsentError, DeleteOnReboot};
use crate::error::CleanError;
use crate::expect::{CancelToken, Expected};
use crate::guard::{CheckedItem, SafetyGuard};
use crate::never::Refusal;
use crate::permanent::{DeleteStats, delete_verified};
use crate::win::handle::{
    self, ACCESS_DELETE, ACCESS_LIST_DIRECTORY, ACCESS_READ_ATTRIBUTES, ACCESS_SYNCHRONIZE, Follow,
    SHARE_ALL, SHARE_NO_DELETE, VOLUME_NAME_GUID,
};

/// A delete request as the helper receives it over the pipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivilegedDeleteRequest {
    /// Volume: a braced GUID (`{...}`) or a mount point (`C:\`).
    pub volume: String,
    /// File reference from the scan.
    pub file_ref: FileRef,
    /// Path the user saw.
    pub expected_path: String,
    /// Logical size from the scan (files only).
    pub expected_size: u64,
    /// Last-write time from the scan (files only).
    pub expected_mtime: FileTime,
    /// Whether the scan saw a directory.
    pub is_dir: bool,
}

impl PrivilegedDeleteRequest {
    /// Structural validation that needs no file-system access.
    ///
    /// # Errors
    ///
    /// A [`CleanError`] describing the first problem.
    pub fn validate(
        &self,
        guard: &SafetyGuard,
    ) -> Result<(CanonicalPath, CanonicalPath), CleanError> {
        if self.file_ref.is_synthetic() || self.file_ref.0 == 0 {
            return Err(CleanError::Changed {
                path: self.expected_path.clone(),
                change: crate::error::Change::SyntheticReference,
            });
        }
        let expected = guard.never_list().check_str(&self.expected_path)?;
        let volume_root = parse_volume(&self.volume)
            .ok_or_else(|| Refusal::unverifiable(&self.expected_path, "the volume is malformed"))?;
        Ok((expected, volume_root))
    }
}

fn parse_volume(v: &str) -> Option<CanonicalPath> {
    let spelled = if v.starts_with('{') {
        format!(r"\\?\Volume{v}\")
    } else {
        v.to_string()
    };
    CanonicalPath::parse(spelled)
        .ok()
        .filter(CanonicalPath::is_root)
}

/// Opens the file by id, verifies everything, and deletes it by handle.
///
/// # Errors
///
/// A typed [`CleanError`]; nothing is deleted unless every check passes.
pub fn verify_and_delete_by_id(
    guard: &SafetyGuard,
    req: &PrivilegedDeleteRequest,
    cancel: &CancelToken,
) -> Result<DeleteStats, CleanError> {
    let display = req.expected_path.clone();
    let (expected_path, volume_root) = req.validate(guard)?;
    if cancel.is_cancelled() {
        return Err(CleanError::Cancelled { path: display });
    }
    let hint = handle::open(
        &volume_root.to_verbatim_wide(),
        ACCESS_READ_ATTRIBUTES,
        SHARE_ALL,
        Follow::Follow,
    )
    .map_err(|e| CleanError::from_io(&req.volume, &e))?;
    let mut access = ACCESS_DELETE | ACCESS_READ_ATTRIBUTES | ACCESS_SYNCHRONIZE;
    if req.is_dir {
        access |= ACCESS_LIST_DIRECTORY;
    }
    let share = if req.is_dir {
        SHARE_NO_DELETE
    } else {
        SHARE_ALL
    };
    let probe = handle::open_by_id(
        &hint,
        u128::from(req.file_ref.0),
        ACCESS_READ_ATTRIBUTES,
        SHARE_ALL,
    )
    .map_err(|e| CleanError::from_io(&display, &e))?;
    let (resolved, facts) = guard.check_handle(&probe, &display)?;
    // SECURITY: the object must still live at the path the user approved.
    if resolved != expected_path {
        return Err(CleanError::Changed {
            path: display,
            change: crate::error::Change::Identity {
                expected: req.file_ref,
                found: facts.identity.file_id,
            },
        });
    }
    let same_volume = match volume_root.root() {
        Root::Volume(_) => handle::final_path(&probe, VOLUME_NAME_GUID)
            .ok()
            .and_then(|w| CanonicalPath::parse_wide(&w).ok())
            .is_some_and(|g| g.root() == volume_root.root()),
        _ => volume_root.contains(&resolved),
    };
    if !same_volume {
        return Err(CleanError::Changed {
            path: display,
            change: crate::error::Change::Volume,
        });
    }
    Expected {
        file_ref: req.file_ref,
        is_dir: req.is_dir,
        size: req.expected_size,
        modified: req.expected_mtime,
    }
    .verify(&facts)
    .map_err(|change| CleanError::Changed {
        path: display.clone(),
        change,
    })?;
    // NOTE: a handle opened by id has no name, and NTFS refuses to delete
    // through it (STATUS_INVALID_PARAMETER: it cannot know which link to
    // remove). Open the verified path, re-run the guard on that handle, and
    // require the very same object before deleting.
    let (h, item) = guard.open_checked(Path::new(&resolved.to_string()), access, share)?;
    if (
        item.facts.identity.volume_serial,
        item.facts.identity.file_id,
    ) != (facts.identity.volume_serial, facts.identity.file_id)
    {
        return Err(CleanError::Changed {
            path: display,
            change: crate::error::Change::Identity {
                expected: req.file_ref,
                found: item.facts.identity.file_id,
            },
        });
    }
    drop(probe);
    delete_verified(guard, h, &item, &display, cancel)
}

/// A delete-on-reboot request (helper only; needs elevation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelayedDeleteRequest {
    /// The file.
    pub path: String,
    /// File reference from the scan.
    pub file_ref: FileRef,
    /// Logical size from the scan.
    pub expected_size: u64,
    /// Last-write time from the scan.
    pub expected_mtime: FileTime,
}

impl DelayedDeleteRequest {
    /// Every check except scheduling: guard, identity, plain file, one link.
    ///
    /// # Errors
    ///
    /// A typed [`CleanError`].
    pub fn validate(&self, guard: &SafetyGuard) -> Result<CheckedItem, CleanError> {
        let item = guard.check_path(Path::new(&self.path))?;
        Expected {
            file_ref: self.file_ref,
            is_dir: false,
            size: self.expected_size,
            modified: self.expected_mtime,
        }
        .verify(&item.facts)
        .map_err(|change| CleanError::Changed {
            path: self.path.clone(),
            change,
        })?;
        // SECURITY: Windows deletes by path at boot, after our checks.
        // Restrict to the simplest objects so a later swap cannot widen the
        // damage: no directories, no links, no extra hardlink names.
        if item.facts.is_dir() || item.facts.is_reparse() || item.facts.links != 1 {
            return Err(Refusal::unverifiable(
                &self.path,
                "only plain files with a single name can be deleted at restart",
            )
            .into());
        }
        Ok(item)
    }
}

/// Validates and schedules a delete at the next restart.
///
/// # Errors
///
/// A typed [`CleanError`] (access denied when not elevated), or a consent
/// error mapped to [`CleanError::NeedsAcknowledgement`].
pub fn schedule_delete_on_reboot(
    guard: &SafetyGuard,
    req: &DelayedDeleteRequest,
    consent: Consent<DeleteOnReboot>,
) -> Result<(), CleanError> {
    let action = consent.redeem().map_err(|e| consent_error(&req.path, &e))?;
    let same = match (
        CanonicalPath::parse(&action.path),
        CanonicalPath::parse(&req.path),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same {
        return Err(consent_error(&req.path, &ConsentError::Stale));
    }
    let item = req.validate(guard)?;
    crate::win::fileops::delete_on_reboot(&item.resolved.to_verbatim_wide())
        .map_err(|e| CleanError::from_io(&req.path, &e))
}

fn consent_error(path: &str, e: &ConsentError) -> CleanError {
    CleanError::Refused {
        refusal: Refusal::unverifiable(path, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::never::tests::sample;

    fn req(path: &str) -> PrivilegedDeleteRequest {
        PrivilegedDeleteRequest {
            volume: r"C:\".into(),
            file_ref: FileRef(123),
            expected_path: path.into(),
            expected_size: 1,
            expected_mtime: FileTime(1),
            is_dir: false,
        }
    }

    #[test]
    fn validation_refuses_protected_and_malformed() {
        let g = SafetyGuard::from_never_list(sample());
        assert!(matches!(
            req(r"C:\Windows\System32\x.dll").validate(&g),
            Err(CleanError::Refused { .. })
        ));
        assert!(matches!(
            req(r"C:\Users").validate(&g),
            Err(CleanError::Refused { .. })
        ));
        assert!(matches!(
            req("relative").validate(&g),
            Err(CleanError::Refused { .. })
        ));
        let mut r = req(r"D:\x\y.txt");
        r.file_ref = FileRef(FileRef::SYNTHETIC_BIT | 5);
        assert!(matches!(r.validate(&g), Err(CleanError::Changed { .. })));
        let mut r = req(r"D:\x\y.txt");
        r.volume = r"C:\Windows".into();
        assert!(matches!(r.validate(&g), Err(CleanError::Refused { .. })));
        let mut r = req(r"D:\x\y.txt");
        r.volume = "{11111111-1111-4111-8111-111111111111}".into();
        assert!(r.validate(&g).is_ok());
    }

    #[test]
    fn request_round_trips_through_json() {
        let r = req(r"D:\a.txt");
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(
            serde_json::from_str::<PrivilegedDeleteRequest>(&s).unwrap(),
            r
        );
    }

    /// The id of a real system folder, with a harmless expected path: the
    /// handle resolves into Windows and must be refused before any check of
    /// the expected path could matter.
    #[test]
    fn ids_of_protected_objects_are_refused() {
        let g = SafetyGuard::from_never_list(sample());
        for target in [
            r"C:\Windows",
            r"C:\Windows\System32",
            r"C:\Windows\explorer.exe",
            r"C:\Windows\System32\drivers\etc\hosts",
            r"C:\Users",
            r"C:\Users\Public",
            r"C:\Program Files",
            r"C:\ProgramData",
        ] {
            let p = CanonicalPath::parse(target).unwrap();
            let Ok(h) = handle::open(
                &p.to_verbatim_wide(),
                ACCESS_READ_ATTRIBUTES,
                SHARE_ALL,
                Follow::Follow,
            ) else {
                continue;
            };
            let info = handle::info(&h).unwrap();
            drop(h);
            for expected_path in [target, r"D:\scratch\harmless.txt"] {
                let r = PrivilegedDeleteRequest {
                    volume: r"C:\".into(),
                    file_ref: FileRef(info.file_index),
                    expected_path: expected_path.into(),
                    expected_size: info.size,
                    expected_mtime: info.modified,
                    is_dir: info.attributes & 0x10 != 0,
                };
                let e = verify_and_delete_by_id(&g, &r, &CancelToken::new()).unwrap_err();
                assert!(
                    matches!(e, CleanError::Refused { .. }),
                    "{target} as {expected_path}: {e:?}"
                );
            }
            assert!(Path::new(target).exists());
        }
    }

    #[test]
    fn delayed_delete_validation() {
        let g = SafetyGuard::from_never_list(sample());
        let r = DelayedDeleteRequest {
            path: r"C:\Windows\System32\drivers\etc\hosts".into(),
            file_ref: FileRef(1),
            expected_size: 0,
            expected_mtime: FileTime(0),
        };
        assert!(matches!(r.validate(&g), Err(CleanError::Refused { .. })));
    }
}
