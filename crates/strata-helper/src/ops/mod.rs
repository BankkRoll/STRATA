//! Request handlers.
//!
//! Each handler runs on its own worker thread (see [`crate::server`]) and
//! sends every response and event for its request itself, ending with
//! exactly one final response. Failures are returned as [`HelperError`] and
//! sent by the server as `Response::Error`.
//!
//! - [`scan`] handles `ScanVolume` (streamed batches with backpressure).
//! - [`records`] handles `ReadRecords`.
//! - [`usn`] wraps the USN journal FSCTLs.
//! - [`delete`] handles `PrivilegedDelete` and `DeleteOnReboot`.
//! - [`activity`] handles file-activity tracking and its queries.

pub mod activity;
pub mod delete;
pub mod records;
pub mod scan;
pub mod usn;

use std::time::Duration;

use strata_ipc::pipe::ServerConnection;
use strata_ipc::protocol::{AuditOp, AuditPhase, Request, Response};

use crate::audit::{AuditSubject, AuditTrail};
use crate::cancel::Cancel;
use crate::error::HelperError;
use crate::source::Volumes;

/// State shared by every request of a helper process.
#[derive(Debug)]
pub struct Shared {
    /// Volume sources and the open-volume cache.
    pub volumes: Volumes,
    /// Audit numbering.
    pub audit: AuditTrail,
    /// The never-list guard for deletes (built on first use).
    pub deletes: delete::DeleteGuard,
    /// MFT bytes per scanner read (a multiple of the record size; the
    /// default is `strata_ntfs::DEFAULT_CHUNK_BYTES`).
    pub scan_chunk_bytes: usize,
    /// The process-wide file-activity tracking session.
    pub activity: activity::ActivityHub,
}

impl Shared {
    /// Shared state over `volumes`.
    #[must_use]
    pub fn new(volumes: Volumes) -> Self {
        Self {
            volumes,
            audit: AuditTrail::new(),
            deletes: delete::DeleteGuard::default(),
            scan_chunk_bytes: strata_ntfs::DEFAULT_CHUNK_BYTES,
            activity: activity::ActivityHub::default(),
        }
    }
}

/// One in-flight request.
#[derive(Debug)]
pub struct RequestCtx<'a> {
    /// The request id.
    pub id: u32,
    /// The connection to answer on.
    pub conn: &'a ServerConnection,
    /// Identifies the connection within this helper process.
    pub connection: u64,
    /// This request's cancellation.
    pub cancel: Cancel,
    /// Verified client process id (for audit records).
    pub client_pid: u32,
    /// Process-wide state.
    pub shared: &'a Shared,
}

impl RequestCtx<'_> {
    /// Sends `response` for this request.
    ///
    /// # Errors
    ///
    /// The client is gone ([`HelperError::disconnected`]) or the pipe failed.
    pub fn send(&self, response: Response) -> Result<(), HelperError> {
        self.conn.send(self.id, response).map_err(HelperError::from)
    }

    /// Records and sends one audit entry.
    ///
    /// # Errors
    ///
    /// As [`RequestCtx::send`]. A failed `Started` record aborts the action
    /// (write-ahead: nothing is touched that the client could not log).
    pub fn audit(
        &self,
        subject: &AuditSubject,
        phase: AuditPhase,
        detail: impl Into<String>,
    ) -> Result<(), HelperError> {
        let entry = self
            .shared
            .audit
            .record(self.client_pid, subject, phase, detail);
        self.send(Response::Audit(entry))
    }

    /// Fails with [`strata_ipc::protocol::ErrorCode::Cancelled`] once
    /// cancellation was requested.
    ///
    /// # Errors
    ///
    /// When cancelled.
    pub fn check_cancel(&self) -> Result<(), HelperError> {
        if self.cancel.is_cancelled() {
            Err(HelperError::new(
                strata_ipc::protocol::ErrorCode::Cancelled,
                "cancelled",
            ))
        } else {
            Ok(())
        }
    }
}

/// Runs one worker request. `Ping`, `Cancel` and `Shutdown` are answered by
/// the connection's reader thread and never reach this function.
///
/// # Errors
///
/// The request failed; the caller sends the error.
pub fn handle(ctx: &RequestCtx<'_>, request: Request) -> Result<(), HelperError> {
    match request {
        Request::ListVolumes => {
            let volumes = ctx.shared.volumes.list()?;
            ctx.send(Response::Volumes { volumes })
        }
        Request::ScanVolume { volume, options } => scan::scan_volume(ctx, &volume, options),
        Request::QueryUsnJournal { volume } => {
            let device = ctx.shared.volumes.device(&volume)?;
            ctx.send(Response::UsnJournal(usn::query_journal(&device)?))
        }
        Request::ReadUsn {
            volume,
            journal_id,
            from,
            max_bytes,
            bytes_to_wait_for,
            timeout_ms,
        } => {
            let device = ctx.shared.volumes.device(&volume)?;
            let (next_usn, raw) = usn::read_journal(
                &device,
                usn::ReadParams {
                    journal_id,
                    from,
                    max_bytes,
                    bytes_to_wait_for,
                    timeout: Duration::from_millis(u64::from(timeout_ms)),
                },
                &ctx.cancel,
            )?;
            ctx.send(Response::UsnRecords { next_usn, raw })
        }
        Request::CreateUsnJournal {
            volume,
            maximum_size,
            allocation_delta,
        } => create_journal(ctx, volume, maximum_size, allocation_delta),
        Request::ReadRecords { volume, file_refs } => {
            records::read_records(ctx, &volume, &file_refs)
        }
        Request::PrivilegedDelete(req) => delete::privileged_delete(ctx, req),
        Request::DeleteOnReboot(req) => delete::delete_on_reboot(ctx, req),
        Request::StartActivity {
            cpu_cap_centi_percent,
        } => activity::start_activity(ctx, cpu_cap_centi_percent),
        Request::StopActivity => activity::stop_activity(ctx),
        Request::ClearActivity => activity::clear_activity(ctx),
        Request::QueryActivity { window, limit } => activity::query_activity(ctx, window, limit),
        Request::ActivityEvidence { since_unix, limit } => {
            activity::activity_evidence(ctx, since_unix, limit)
        }
        Request::Ping | Request::Cancel { .. } | Request::Shutdown => {
            Err(HelperError::internal("control request routed to a worker"))
        }
    }
}

fn create_journal(
    ctx: &RequestCtx<'_>,
    volume: String,
    maximum_size: u64,
    allocation_delta: u64,
) -> Result<(), HelperError> {
    let device = ctx.shared.volumes.device(&volume)?;
    let subject = AuditSubject {
        op: AuditOp::CreateUsnJournal,
        volume,
        file_ref: None,
        path: None,
    };
    ctx.audit(&subject, AuditPhase::Started, "")?;
    match usn::create_journal(&device, maximum_size, allocation_delta) {
        Ok(info) => {
            ctx.audit(
                &subject,
                AuditPhase::Succeeded,
                format!(
                    "journal {:#x}, maximum size {} bytes",
                    info.journal_id, info.maximum_size
                ),
            )?;
            ctx.send(Response::UsnJournal(Some(info)))
        }
        Err(e) => {
            ctx.audit(&subject, AuditPhase::Failed, e.message.clone())?;
            Err(e)
        }
    }
}
