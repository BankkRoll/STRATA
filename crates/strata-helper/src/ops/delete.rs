//! `PrivilegedDelete` and `DeleteOnReboot` (SPEC §15.3, §15.7).
//!
//! The helper never trusts the client's path. Every request is checked
//! against the never-list here first, independently of the app and of
//! `strata-clean`; then `strata_clean::privileged` reopens the object by
//! file id, verifies path, volume, size and mtime, re-runs the never-list on
//! the handle-resolved path and deletes by handle without following
//! reparse points. Each request produces `Started` and an outcome audit
//! record.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use strata_clean::consent::{DeleteOnReboot as RebootConsent, Prompt};
use strata_clean::privileged::{
    DelayedDeleteRequest, PrivilegedDeleteRequest, schedule_delete_on_reboot,
    verify_and_delete_by_id,
};
use strata_clean::{CleanError, GuardConfig, SafetyGuard};
use strata_ipc::protocol::{
    AuditOp, AuditPhase, DeleteRequest, DeleteSummary, ErrorCode, RebootDeleteRequest, Response,
};

use super::RequestCtx;
use crate::audit::AuditSubject;
use crate::error::HelperError;
use crate::source::{VolumeTarget, parse_volume_id};

/// The helper's own never-list guard, built on first use.
///
/// Building it enumerates volumes and resolves known folders for every
/// profile, which takes a moment, so it is deferred until the first delete
/// and then reused for the helper's lifetime.
#[derive(Debug, Default)]
pub struct DeleteGuard {
    guard: Mutex<Option<Arc<SafetyGuard>>>,
}

impl DeleteGuard {
    /// A cell pre-filled with `guard`.
    #[must_use]
    pub fn with(guard: SafetyGuard) -> Self {
        Self {
            guard: Mutex::new(Some(Arc::new(guard))),
        }
    }

    /// The guard, building it on first use.
    ///
    /// # Errors
    ///
    /// Known folders or volumes could not be resolved; nothing can be
    /// deleted without a complete never-list.
    pub fn get(&self) -> Result<Arc<SafetyGuard>, HelperError> {
        let mut slot = self.guard.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(g) = slot.as_ref() {
            return Ok(Arc::clone(g));
        }
        let known = strata_win::known::known_folders()
            .map_err(|e| HelperError::internal(format!("cannot resolve known folders: {e}")))?;
        let install_dirs: Vec<PathBuf> = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from))
            .into_iter()
            .collect();
        let guard = SafetyGuard::new(GuardConfig {
            known,
            install_dirs,
        })
        .map_err(|e| HelperError::internal(format!("cannot build the never-list: {e}")))?;
        let guard = Arc::new(guard);
        *slot = Some(Arc::clone(&guard));
        Ok(guard)
    }
}

fn utf16_path(units: &[u16]) -> Result<String, HelperError> {
    String::from_utf16(units).map_err(|_| {
        HelperError::bad_request("paths with unpaired surrogates cannot be deleted by the helper")
    })
}

/// `strata-clean` takes a braced GUID or a mount point.
fn clean_volume(volume: &str) -> Result<String, HelperError> {
    match parse_volume_id(volume)? {
        VolumeTarget::Device(d) => {
            if let Some(guid) = d.strip_prefix(r"\\?\Volume") {
                Ok(guid.to_owned())
            } else {
                // `\\.\X:` → `X:\`
                Ok(format!("{}\\", &d[4..]))
            }
        }
        VolumeTarget::Image => Err(HelperError::new(
            ErrorCode::NotSupported,
            "image volumes cannot be modified",
        )),
    }
}

/// Whether a failure happened before anything was touched.
fn is_refusal(e: &CleanError) -> bool {
    matches!(
        e,
        CleanError::Refused { .. }
            | CleanError::NeverTier { .. }
            | CleanError::Changed { .. }
            | CleanError::NotFound { .. }
            | CleanError::Cancelled { .. }
    )
}

fn fail(ctx: &RequestCtx<'_>, subject: &AuditSubject, e: &CleanError) -> HelperError {
    let phase = if is_refusal(e) {
        AuditPhase::Refused
    } else {
        AuditPhase::Failed
    };
    let err = HelperError::from(e);
    if let Err(send) = ctx.audit(subject, phase, err.message.clone()) {
        return send;
    }
    err
}

/// Handles `PrivilegedDelete`.
///
/// # Errors
///
/// [`ErrorCode::Protected`] (never-list), [`ErrorCode::Mismatch`] (the
/// object changed), or another typed failure. Nothing is deleted unless
/// every check passes.
pub fn privileged_delete(ctx: &RequestCtx<'_>, req: DeleteRequest) -> Result<(), HelperError> {
    let subject = AuditSubject {
        op: AuditOp::PrivilegedDelete,
        volume: req.volume.clone(),
        file_ref: Some(req.file_ref),
        path: Some(req.expected_path.clone()),
    };
    ctx.audit(&subject, AuditPhase::Started, "")?;
    let prepared = utf16_path(&req.expected_path)
        .and_then(|p| Ok((p, clean_volume(&req.volume)?)))
        .and_then(|(p, v)| Ok((p, v, ctx.shared.deletes.get()?)));
    let (path, volume, guard) = match prepared {
        Ok(x) => x,
        Err(e) => {
            ctx.audit(&subject, AuditPhase::Refused, e.message.clone())?;
            return Err(e);
        }
    };
    // SECURITY: the helper's own never-list check, before strata-clean's.
    // A compromised or buggy app cannot get a protected path past it.
    if let Err(refusal) = guard.never_list().check_str(&path) {
        return Err(fail(ctx, &subject, &CleanError::from(refusal)));
    }
    let request = PrivilegedDeleteRequest {
        volume,
        file_ref: req.file_ref,
        expected_path: path,
        expected_size: req.expected_size,
        expected_mtime: req.expected_mtime,
        is_dir: req.is_dir,
    };
    match verify_and_delete_by_id(&guard, &request, ctx.cancel.token()) {
        Ok(stats) => {
            ctx.audit(
                &subject,
                AuditPhase::Succeeded,
                format!(
                    "{} files, {} folders, {} links, {} bytes",
                    stats.files, stats.dirs, stats.links, stats.bytes
                ),
            )?;
            ctx.send(Response::Deleted {
                file_ref: req.file_ref,
                summary: DeleteSummary {
                    files: stats.files,
                    dirs: stats.dirs,
                    links: stats.links,
                    bytes: stats.bytes,
                },
            })
        }
        Err(e) => Err(fail(ctx, &subject, &e)),
    }
}

/// Handles `DeleteOnReboot`.
///
/// The app shows the restart-delete prompt before sending the request, so
/// the request from the verified app is the user's confirmation; the helper
/// mints the consent value from it and redeems it immediately.
///
/// # Errors
///
/// As [`privileged_delete`]; also refused for directories, links and files
/// with more than one name, and access denied when not elevated.
pub fn delete_on_reboot(ctx: &RequestCtx<'_>, req: RebootDeleteRequest) -> Result<(), HelperError> {
    let subject = AuditSubject {
        op: AuditOp::DeleteOnReboot,
        volume: String::new(),
        file_ref: Some(req.file_ref),
        path: Some(req.expected_path.clone()),
    };
    ctx.audit(&subject, AuditPhase::Started, "")?;
    let prepared = utf16_path(&req.expected_path).and_then(|p| Ok((p, ctx.shared.deletes.get()?)));
    let (path, guard) = match prepared {
        Ok(x) => x,
        Err(e) => {
            ctx.audit(&subject, AuditPhase::Refused, e.message.clone())?;
            return Err(e);
        }
    };
    // SECURITY: helper-level never-list check, as for by-id deletes.
    if let Err(refusal) = guard.never_list().check_str(&path) {
        return Err(fail(ctx, &subject, &CleanError::from(refusal)));
    }
    let consent = Prompt::new(RebootConsent { path: path.clone() }).confirm();
    let request = DelayedDeleteRequest {
        path,
        file_ref: req.file_ref,
        expected_size: req.expected_size,
        expected_mtime: req.expected_mtime,
    };
    match schedule_delete_on_reboot(&guard, &request, consent) {
        Ok(()) => {
            ctx.audit(
                &subject,
                AuditPhase::Succeeded,
                "scheduled for the next restart",
            )?;
            ctx.send(Response::RebootScheduled {
                file_ref: req.file_ref,
            })
        }
        Err(e) => Err(fail(ctx, &subject, &e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volumes_are_converted_for_strata_clean() {
        assert_eq!(
            clean_volume(r"\\?\Volume{11111111-1111-4111-8111-111111111111}\").unwrap(),
            "{11111111-1111-4111-8111-111111111111}"
        );
        assert_eq!(clean_volume("d:").unwrap(), r"D:\");
        assert!(clean_volume(r"C:\Windows").is_err());
        assert!(clean_volume(crate::source::IMAGE_VOLUME).is_err());
    }

    #[test]
    fn unpaired_surrogates_are_refused() {
        assert!(utf16_path(&[0xD800, 0x41]).is_err());
        assert_eq!(utf16_path(&[0x43, 0x3A]).unwrap(), "C:");
    }
}
