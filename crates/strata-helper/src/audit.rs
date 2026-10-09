//! Audit records for privileged actions.
//!
//! The helper has no database. Each privileged request sends its records to
//! the client as `Response::Audit` events with the request's id: `Started`
//! before anything is touched, then exactly one of `Succeeded`, `Refused` or
//! `Failed`. The app stores them in its undo/audit log; a `Started` without
//! an outcome after a helper crash marks the action "interrupted".

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use strata_core::{FileRef, FileTime};
use strata_ipc::protocol::{AuditEntry, AuditOp, AuditPhase};

/// 100 ns intervals between 1601-01-01 and 1970-01-01.
const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;

/// The current time as a FILETIME.
#[must_use]
pub fn now() -> FileTime {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let ticks = u64::try_from(since.as_nanos() / 100).unwrap_or(u64::MAX);
    FileTime(UNIX_EPOCH_AS_FILETIME.saturating_add(ticks))
}

/// What an audited action is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditSubject {
    /// The action.
    pub op: AuditOp,
    /// Volume as requested.
    pub volume: String,
    /// File reference for per-file actions.
    pub file_ref: Option<FileRef>,
    /// Claimed path for per-file actions.
    pub path: Option<Vec<u16>>,
}

/// Numbers audit records for one helper process.
#[derive(Debug, Default)]
pub struct AuditTrail {
    seq: AtomicU64,
}

impl AuditTrail {
    /// A trail starting at sequence 1.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the next record.
    #[must_use]
    pub fn record(
        &self,
        client_pid: u32,
        subject: &AuditSubject,
        phase: AuditPhase,
        detail: impl Into<String>,
    ) -> AuditEntry {
        AuditEntry {
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            time: now(),
            client_pid,
            op: subject.op,
            phase,
            volume: subject.volume.clone(),
            file_ref: subject.file_ref,
            path: subject.path.clone(),
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_numbered_and_timestamped() {
        let t = AuditTrail::new();
        let s = AuditSubject {
            op: AuditOp::PrivilegedDelete,
            volume: r"C:\".into(),
            file_ref: Some(FileRef(7)),
            path: Some("C:\\x".encode_utf16().collect()),
        };
        let a = t.record(1, &s, AuditPhase::Started, "");
        let b = t.record(1, &s, AuditPhase::Succeeded, "1 file");
        assert_eq!((a.seq, b.seq), (1, 2));
        assert!(b.time >= a.time);
        // 2020-01-01 as FILETIME.
        assert!(a.time.0 > 132_223_104_000_000_000);
        assert_eq!(b.detail, "1 file");
    }
}
