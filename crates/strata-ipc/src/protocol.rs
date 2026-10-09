//! Protocol messages (SPEC §4, §6.4, §10, §15.7).
//!
//! Every frame carries a request id in its header (see [`crate::frame`]).
//! The client picks ids for its requests (non-zero); every response and event
//! the helper sends about that request carries the same id. Id 0 is reserved
//! for the handshake and for unsolicited messages.
//!
//! Compatibility: messages are postcard-encoded, so variants are identified
//! by position. Any change to a message type (adding, removing or reordering
//! variants or fields) must bump [`PROTOCOL_VERSION`]; the handshake rejects
//! mismatched peers, so the helper and the app always run the same version.

use serde::{Deserialize, Serialize};
use strata_core::{FileRef, FileTime, ScanRecord};
use strata_win::volume::VolumeInfo;

/// The wire protocol version. Both sides must match exactly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Request id reserved for the handshake and unsolicited messages.
pub const HANDSHAKE_ID: u32 = 0;

/// First message from the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The client's [`PROTOCOL_VERSION`].
    pub protocol: u32,
    /// Client build string (e.g. `0.1.0+abc123`), for logs and diagnostics.
    pub build: String,
    /// The client's process id. The helper checks it against the pipe's
    /// actual client process (`GetNamedPipeClientProcessId`).
    pub client_pid: u32,
}

/// What the helper can do in this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    /// Raw MFT scans (`ScanVolume` with the MFT scanner).
    pub mft_scan: bool,
    /// USN journal queries and reads.
    pub usn_journal: bool,
    /// Reading individual records by file reference.
    pub read_records: bool,
    /// Validated privileged deletes.
    pub privileged_delete: bool,
}

/// The helper's reply to a valid [`Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The helper's [`PROTOCOL_VERSION`] (equal to the client's).
    pub protocol: u32,
    /// Helper build string.
    pub helper_build: String,
    /// Whether the helper runs elevated.
    pub elevated: bool,
    /// Enabled features.
    pub capabilities: Capabilities,
}

/// Why the helper refused a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum HandshakeReject {
    /// Protocol versions differ; the app must restart a matching helper.
    #[error("protocol version mismatch: helper {helper}, client {client}")]
    VersionMismatch {
        /// Helper version.
        helper: u32,
        /// Client version.
        client: u32,
    },
    /// The client process failed image-path or signature verification.
    /// Details are logged by the helper, never sent to the client.
    #[error("client is not trusted")]
    Untrusted,
    /// `Hello.client_pid` does not match the pipe's client process.
    #[error("client pid does not match the pipe client")]
    PidMismatch,
    /// The first frame was not a valid `Hello`.
    #[error("malformed handshake")]
    Malformed,
}

/// Scan parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanOptions {
    /// Records per `ScanBatch` (the helper clamps to a sane range).
    pub batch_size: u32,
    /// Interval between `ScanProgress` events, in milliseconds.
    pub progress_interval_ms: u32,
    /// Include NTFS metadata files (`$MFT`, `$LogFile`, ...).
    pub include_metadata: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            batch_size: 8192,
            progress_interval_ms: 100,
            include_metadata: true,
        }
    }
}

/// A delete request, validated by the helper before acting (SPEC §15.7).
///
/// The helper reopens the file by id with `FILE_FLAG_OPEN_REPARSE_POINT`,
/// and refuses unless the path, size and modification time all still match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteRequest {
    /// Volume GUID path (`\\?\Volume{...}\`).
    pub volume: String,
    /// File reference (record + sequence).
    pub file_ref: FileRef,
    /// Path the UI showed the user, as raw UTF-16 (lossless).
    pub expected_path: Vec<u16>,
    /// Logical size the UI showed.
    pub expected_size: u64,
    /// Last-modified time the UI showed.
    pub expected_mtime: FileTime,
}

/// Client → helper requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Liveness check; answered with [`Response::Pong`].
    Ping,
    /// List volumes as the helper sees them.
    ListVolumes,
    /// Scan a volume; streams `ScanProgress`/`ScanBatch` then `ScanDone`.
    ScanVolume {
        /// Volume GUID path.
        volume: String,
        /// Scan parameters.
        options: ScanOptions,
    },
    /// Cancel an in-flight request (e.g. a scan) by its id.
    Cancel {
        /// The request to cancel.
        request_id: u32,
    },
    /// Query a volume's USN journal state.
    QueryUsnJournal {
        /// Volume GUID path.
        volume: String,
    },
    /// Read raw USN records starting at `from`.
    ReadUsn {
        /// Volume GUID path.
        volume: String,
        /// Journal id the client expects; a changed id means a full rescan.
        journal_id: u64,
        /// First USN to read.
        from: i64,
        /// Maximum bytes of records to return.
        max_bytes: u32,
    },
    /// Re-read individual records (after USN changes).
    ReadRecords {
        /// Volume GUID path.
        volume: String,
        /// Records to read.
        file_refs: Vec<FileRef>,
    },
    /// Delete a file or directory with elevated rights after validation.
    PrivilegedDelete(DeleteRequest),
    /// Ask the helper to exit.
    Shutdown,
}

/// Scan progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScanProgress {
    /// Records emitted so far.
    pub records: u64,
    /// MFT bytes read so far.
    pub bytes_read: u64,
    /// Total MFT bytes to read, when known.
    pub bytes_total: Option<u64>,
}

/// Final scan statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScanStats {
    /// Records emitted.
    pub records: u64,
    /// Records skipped as corrupt (bad signature or fixup mismatch).
    pub corrupt: u64,
    /// Wall time in milliseconds.
    pub elapsed_ms: u64,
    /// Whether the scan was cancelled (totals are partial).
    pub cancelled: bool,
}

/// USN journal state (`FSCTL_QUERY_USN_JOURNAL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsnJournalInfo {
    /// Journal id; changes when the journal is recreated.
    pub journal_id: u64,
    /// Oldest USN still in the journal.
    pub first_usn: i64,
    /// USN the next change will get.
    pub next_usn: i64,
    /// Lowest valid USN (records below were purged).
    pub lowest_valid_usn: i64,
    /// Largest possible USN.
    pub max_usn: i64,
    /// Maximum journal size in bytes.
    pub maximum_size: u64,
    /// Allocation delta in bytes.
    pub allocation_delta: u64,
}

/// Error categories sent to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorCode {
    /// The request was malformed or invalid in this state.
    BadRequest,
    /// The volume is unknown, locked, or gone.
    UnknownVolume,
    /// The helper does not support this request (see capabilities).
    NotSupported,
    /// Access denied by the OS.
    AccessDenied,
    /// The file id no longer exists.
    NotFound,
    /// A delete was refused because path, size or mtime no longer match.
    Mismatch,
    /// A delete was refused by the helper's never-list.
    Protected,
    /// Too many requests; retry after `retry_after_ms`.
    RateLimited,
    /// The request was cancelled.
    Cancelled,
    /// Another operation holds the resource.
    Busy,
    /// I/O error.
    Io,
    /// Helper bug or unexpected state.
    Internal,
}

/// A failed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorReply {
    /// Category.
    pub code: ErrorCode,
    /// Human-readable detail (never includes data from other users).
    pub message: String,
    /// For [`ErrorCode::RateLimited`]: when to retry.
    pub retry_after_ms: Option<u32>,
}

/// Helper → client responses and events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// Reply to [`Request::Ping`].
    Pong,
    /// Reply to [`Request::ListVolumes`].
    Volumes {
        /// Volumes as seen by the helper.
        volumes: Vec<VolumeInfo>,
    },
    /// Periodic scan progress.
    ScanProgress(ScanProgress),
    /// A batch of scan records.
    ScanBatch {
        /// Records, in scanner order.
        records: Vec<ScanRecord>,
    },
    /// The scan finished (or was cancelled; see `stats.cancelled`).
    ScanDone {
        /// Totals.
        stats: ScanStats,
    },
    /// Acknowledges [`Request::Cancel`] (sent with the cancel's own id).
    CancelAck {
        /// Whether the target request was still running.
        was_running: bool,
    },
    /// Reply to [`Request::QueryUsnJournal`]; `None` when the journal is not
    /// active on the volume.
    UsnJournal(Option<UsnJournalInfo>),
    /// Reply to [`Request::ReadUsn`]: raw `USN_RECORD_V2/V3/V4` bytes as
    /// returned by `FSCTL_READ_USN_JOURNAL` (after the leading next-USN),
    /// parsed by `strata-ntfs`.
    UsnRecords {
        /// USN to pass as `from` next time.
        next_usn: i64,
        /// Raw record bytes.
        raw: Vec<u8>,
    },
    /// Reply to [`Request::ReadRecords`].
    Records {
        /// Records that still exist.
        records: Vec<ScanRecord>,
        /// References that no longer exist (or are stale).
        missing: Vec<FileRef>,
    },
    /// A privileged delete succeeded.
    Deleted {
        /// What was deleted.
        file_ref: FileRef,
    },
    /// Reply to [`Request::Shutdown`]; the helper exits after sending it.
    ShuttingDown,
    /// The request failed.
    Error(ErrorReply),
}

/// Any message on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Client handshake.
    Hello(Hello),
    /// Helper handshake reply.
    Welcome(Welcome),
    /// Helper handshake refusal.
    Reject(HandshakeReject),
    /// Client request.
    Request(Request),
    /// Helper response or event.
    Response(Response),
}

/// A message plus the request id from its frame header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Request id (see the module docs).
    pub request_id: u32,
    /// The message.
    pub message: Message,
}
