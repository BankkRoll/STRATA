//! The app's client for the helper (SPEC §4): launch → connect → handshake
//! → typed requests.
//!
//! ```no_run
//! use strata_helper::client::{ClientConfig, ClientError, HelperClient, ScanEvent};
//! use strata_ipc::protocol::ScanOptions;
//! # fn main() -> Result<(), ClientError> {
//! let helper = strata_ipc::security::TrustPolicy::sibling_of_current_exe("strata-helper.exe")
//!     .map_err(|e| ClientError::Launch(e.to_string()))?;
//! let mut client = match HelperClient::connect(ClientConfig::on_demand(helper)) {
//!     Ok(c) => c,
//!     Err(ClientError::Declined) => return Ok(()), // UAC declined: use the walker
//!     Err(e) => return Err(e),
//! };
//! for event in client.scan_volume(r"\\?\Volume{11111111-1111-4111-8111-111111111111}\", ScanOptions::default())? {
//!     match event {
//!         Ok(ScanEvent::Batch(records)) => { /* index.ingest(records) */ }
//!         Ok(ScanEvent::Done(stats)) => println!("{} records", stats.records),
//!         Ok(_) => {}
//!         Err(ClientError::Disconnected) => { client.reconnect()?; break }
//!         Err(e) => return Err(e),
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! Design:
//!
//! - One dispatcher thread reads every frame and routes it by request id to
//!   a per-request bounded channel, so a scan stream, pings and USN reads
//!   can be used from different threads at once.
//! - Backpressure is end to end: a scan consumer that stops draining its
//!   [`ScanStream`] fills its channel, the dispatcher stops reading the
//!   pipe, and the helper's scan blocks. A scan that is never drained also
//!   delays other requests' replies, so consume scans promptly (or drop the
//!   stream, which cancels the scan).
//! - A helper crash, exit or broken pipe ends the dispatcher; every pending
//!   and later call returns [`ClientError::Disconnected`]. Call
//!   [`HelperClient::reconnect`] to start over with the same launch mode.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender, bounded};
use strata_core::{FileRef, ScanRecord};
use strata_ipc::IpcError;
use strata_ipc::pipe::{ClientOptions, PipeClient};
use strata_ipc::protocol::{
    AuditEntry, DeleteRequest, DeleteSummary, ErrorCode, ErrorReply, HandshakeReject,
    RebootDeleteRequest, Request, Response, ScanOptions, ScanProgress, ScanStats, UsnJournalInfo,
    Welcome,
};
use strata_ipc::security::{PeerVerifier, TrustError, session_pipe_name};
use strata_win::process::{ElevatedChild, LaunchError, launch_elevated};
use strata_win::volume::VolumeInfo;

/// Per-request channel capacity (scan batches buffered client side).
const ROUTE_CAPACITY: usize = 8;
/// How often blocked internal loops check for shutdown.
const TICK: Duration = Duration::from_millis(200);

// -----------------------------------------------------------------------------
// Configuration
// -----------------------------------------------------------------------------

/// How the client reaches a helper.
#[derive(Debug, Clone)]
pub enum LaunchMode {
    /// Start `helper_exe` elevated through UAC for this app session.
    OnDemand {
        /// Path of `strata-helper.exe`.
        helper_exe: PathBuf,
    },
    /// Use the installed service (start it if needed), no UAC prompt.
    Service {
        /// Path of the installed `strata-helper.exe`, for server
        /// verification.
        helper_exe: PathBuf,
    },
    /// Start `helper_exe` unelevated (development and tests; with `--image`
    /// in debug builds).
    Spawn {
        /// Path of `strata-helper.exe`.
        helper_exe: PathBuf,
        /// Extra arguments, e.g. `["--image", "<file>"]`.
        extra_args: Vec<OsString>,
    },
    /// Connect to an already running helper's pipe.
    Pipe {
        /// The pipe name.
        name: String,
    },
}

/// Client configuration.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// How to reach the helper.
    pub mode: LaunchMode,
    /// Pipe options. When `server_verifier` is `None` and the mode names a
    /// helper binary, the helper is verified against that binary.
    pub options: ClientOptions,
    /// Default timeout for request/response calls (not scan streams).
    pub request_timeout: Duration,
}

impl ClientConfig {
    fn with_mode(mode: LaunchMode) -> Self {
        Self {
            mode,
            options: ClientOptions {
                // NOTE: covers the UAC prompt and process start; the pipe
                // appears only after the user consents.
                connect_timeout: Duration::from_secs(30),
                ..ClientOptions::default()
            },
            request_timeout: Duration::from_secs(60),
        }
    }

    /// On-demand elevation of `helper_exe` (one UAC prompt per launch).
    #[must_use]
    pub fn on_demand(helper_exe: impl Into<PathBuf>) -> Self {
        Self::with_mode(LaunchMode::OnDemand {
            helper_exe: helper_exe.into(),
        })
    }

    /// The installed service.
    #[must_use]
    pub fn service(helper_exe: impl Into<PathBuf>) -> Self {
        Self::with_mode(LaunchMode::Service {
            helper_exe: helper_exe.into(),
        })
    }

    /// An unelevated child process (development, tests).
    #[must_use]
    pub fn spawn(helper_exe: impl Into<PathBuf>, extra_args: Vec<OsString>) -> Self {
        Self::with_mode(LaunchMode::Spawn {
            helper_exe: helper_exe.into(),
            extra_args,
        })
    }

    /// An existing pipe.
    #[must_use]
    pub fn pipe(name: impl Into<String>) -> Self {
        Self::with_mode(LaunchMode::Pipe { name: name.into() })
    }
}

// -----------------------------------------------------------------------------
// Errors and results
// -----------------------------------------------------------------------------

/// A request the helper refused or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteError {
    /// Category.
    pub code: ErrorCode,
    /// Detail from the helper.
    pub message: String,
    /// Audit records the helper sent before failing (privileged requests);
    /// store them like successful ones.
    pub audit: Vec<AuditEntry>,
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

/// Everything a client call can fail with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The user declined the UAC prompt; fall back to the walker.
    #[error("the user declined the elevation prompt")]
    Declined,
    /// The helper could not be started.
    #[error("could not start the helper: {0}")]
    Launch(String),
    /// The helper process exited before accepting the connection.
    #[error("the helper exited early (exit code {0:?})")]
    HelperExited(Option<u32>),
    /// Service mode was requested but the service is not installed.
    #[error("the helper service is not installed")]
    ServiceNotInstalled,
    /// The helper is gone (crash, exit, broken pipe). Show the banner and
    /// offer [`HelperClient::reconnect`].
    #[error("the helper disconnected")]
    Disconnected,
    /// No answer in time.
    #[error("timed out waiting for the helper")]
    Timeout,
    /// Too many requests; retry after the delay.
    #[error("rate limited; retry after {retry_after:?}")]
    RateLimited {
        /// When a retry may succeed.
        retry_after: Duration,
    },
    /// The helper refused the handshake (e.g. version mismatch: restart a
    /// matching helper).
    #[error("handshake rejected: {0}")]
    Rejected(HandshakeReject),
    /// The process at the other end is not the expected helper.
    #[error("the helper is not trusted: {0}")]
    Untrusted(TrustError),
    /// The request failed in the helper.
    #[error("{0}")]
    Remote(RemoteError),
    /// The helper answered with a response that does not fit the request.
    #[error("unexpected response: {0}")]
    Unexpected(String),
    /// Another transport failure.
    #[error(transparent)]
    Ipc(IpcError),
}

impl ClientError {
    /// The helper's error code, for [`ClientError::Remote`].
    #[must_use]
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Remote(r) => Some(r.code),
            Self::RateLimited { .. } => Some(ErrorCode::RateLimited),
            _ => None,
        }
    }
}

impl From<IpcError> for ClientError {
    fn from(e: IpcError) -> Self {
        match e {
            IpcError::Disconnected => Self::Disconnected,
            IpcError::Timeout => Self::Timeout,
            IpcError::RateLimited { retry_after, .. } => Self::RateLimited { retry_after },
            IpcError::Rejected(r) => Self::Rejected(r),
            IpcError::Untrusted(t) => Self::Untrusted(t),
            other => Self::Ipc(other),
        }
    }
}

/// A result together with the helper's audit records for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audited<T> {
    /// The result.
    pub value: T,
    /// Audit records to store in the undo/audit log.
    pub audit: Vec<AuditEntry>,
}

/// Raw journal bytes from [`HelperClient::read_usn`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnChunk {
    /// USN to read from next time.
    pub next_usn: i64,
    /// `USN_RECORD_V2/V3/V4` records as the kernel returned them.
    pub raw: Vec<u8>,
}

impl UsnChunk {
    /// The buffer in `FSCTL_READ_USN_JOURNAL` layout (leading next-USN),
    /// ready for `strata_ntfs::parse_usn_buffer`.
    #[must_use]
    pub fn to_fsctl_buffer(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(8 + self.raw.len());
        b.extend_from_slice(&self.next_usn.to_le_bytes());
        b.extend_from_slice(&self.raw);
        b
    }
}

/// Parameters for [`HelperClient::read_usn`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnRead {
    /// Volume GUID path.
    pub volume: String,
    /// Journal id from `query_usn_journal`.
    pub journal_id: u64,
    /// First USN to read.
    pub from: i64,
    /// Output buffer size.
    pub max_bytes: u32,
    /// Block until this many bytes of records exist (0: return at once).
    pub bytes_to_wait_for: u32,
    /// Longest wait.
    pub wait: Duration,
}

/// Records from [`HelperClient::read_records`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordsReply {
    /// Records that still exist with the requested sequence numbers.
    pub records: Vec<ScanRecord>,
    /// References that no longer exist or were reused.
    pub missing: Vec<FileRef>,
}

// -----------------------------------------------------------------------------
// Connection internals
// -----------------------------------------------------------------------------

#[derive(Debug)]
enum HelperProcess {
    Elevated(ElevatedChild),
    Spawned(std::process::Child),
}

impl HelperProcess {
    fn pid(&self) -> u32 {
        match self {
            Self::Elevated(c) => c.pid(),
            Self::Spawned(c) => c.id(),
        }
    }

    fn exit_code(&mut self) -> Option<Option<u32>> {
        match self {
            Self::Elevated(c) => c.try_exit_code().ok().flatten().map(Some),
            Self::Spawned(c) => c
                .try_wait()
                .ok()
                .flatten()
                .map(|s| s.code().map(|c| c as u32)),
        }
    }
}

#[derive(Debug, Default)]
struct Routes {
    senders: HashMap<u32, Sender<Response>>,
    early: HashMap<u32, Vec<Response>>,
    max_registered: u32,
}

#[derive(Debug)]
struct Inner {
    /// `None` once closed, so a closed client really releases the pipe even
    /// while `ScanStream`s still hold this state.
    pipe: RwLock<Option<PipeClient>>,
    routes: Mutex<Routes>,
    /// Serializes "send + register" so routes are registered in id order.
    send_lock: Mutex<()>,
    closing: AtomicBool,
    connected: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_terminal(r: &Response) -> bool {
    !matches!(
        r,
        Response::ScanProgress(_) | Response::ScanBatch { .. } | Response::Audit(_)
    )
}

impl Inner {
    fn with_pipe<T>(
        &self,
        f: impl FnOnce(&PipeClient) -> Result<T, IpcError>,
    ) -> Result<T, IpcError> {
        let pipe = self.pipe.read().unwrap_or_else(PoisonError::into_inner);
        match pipe.as_ref() {
            Some(p) => f(p),
            None => Err(IpcError::Disconnected),
        }
    }

    /// Closes the pipe handle; the helper sees the disconnect.
    fn close_pipe(&self) {
        let pipe = self
            .pipe
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        drop(pipe);
    }

    fn dispatch(self: &Arc<Self>) {
        while !self.closing.load(Ordering::SeqCst) {
            match self.with_pipe(|p| p.recv(Some(TICK))) {
                Ok(None) => {}
                Ok(Some((id, r))) => self.route(id, r),
                Err(IpcError::RateLimited {
                    request_id,
                    retry_after,
                }) => {
                    let ms = u32::try_from(retry_after.as_millis()).unwrap_or(u32::MAX);
                    self.route(
                        request_id,
                        Response::Error(ErrorReply {
                            code: ErrorCode::RateLimited,
                            message: "too many requests".into(),
                            retry_after_ms: Some(ms),
                        }),
                    );
                }
                Err(e) if !e.is_fatal() => {}
                Err(_) => break,
            }
        }
        self.connected.store(false, Ordering::SeqCst);
        let mut routes = lock(&self.routes);
        routes.senders.clear();
        routes.early.clear();
    }

    fn route(&self, id: u32, r: Response) {
        let terminal = is_terminal(&r);
        let tx = {
            let mut routes = lock(&self.routes);
            match routes.senders.get(&id) {
                Some(tx) => {
                    let tx = tx.clone();
                    if terminal {
                        routes.senders.remove(&id);
                    }
                    tx
                }
                None if id > routes.max_registered => {
                    routes.early.entry(id).or_default().push(r);
                    return;
                }
                // NOTE: a request whose caller gave up (timeout, dropped
                // stream); its late responses are discarded.
                None => return,
            }
        };
        let mut r = r;
        loop {
            match tx.send_timeout(r, TICK) {
                Ok(()) => return,
                Err(SendTimeoutError::Timeout(back)) => {
                    if self.closing.load(Ordering::SeqCst) {
                        return;
                    }
                    r = back;
                }
                Err(SendTimeoutError::Disconnected(_)) => {
                    lock(&self.routes).senders.remove(&id);
                    return;
                }
            }
        }
    }

    fn request(&self, request: Request) -> Result<(u32, Receiver<Response>), ClientError> {
        if !self.connected.load(Ordering::SeqCst) {
            return Err(ClientError::Disconnected);
        }
        let _order = lock(&self.send_lock);
        let id = self.with_pipe(|p| p.send(request))?;
        let mut routes = lock(&self.routes);
        let early = routes.early.remove(&id).unwrap_or_default();
        let (tx, rx) = bounded(ROUTE_CAPACITY.max(early.len()));
        let mut finished = false;
        for r in early {
            finished |= is_terminal(&r);
            let _ = tx.try_send(r);
        }
        if !finished {
            routes.senders.insert(id, tx);
        }
        routes.max_registered = routes.max_registered.max(id);
        drop(routes);
        if !self.connected.load(Ordering::SeqCst) {
            self.unregister(id);
            return Err(ClientError::Disconnected);
        }
        Ok((id, rx))
    }

    fn unregister(&self, id: u32) {
        lock(&self.routes).senders.remove(&id);
    }

    fn call(
        &self,
        request: Request,
        timeout: Duration,
    ) -> Result<(Response, Vec<AuditEntry>), ClientError> {
        let (id, rx) = self.request(request)?;
        let deadline = Instant::now() + timeout;
        let mut audit = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(Response::Audit(a)) => audit.push(a),
                Ok(Response::Error(e)) => return Err(remote(e, audit)),
                Ok(r) => return Ok((r, audit)),
                Err(RecvTimeoutError::Timeout) => {
                    self.unregister(id);
                    return Err(ClientError::Timeout);
                }
                Err(RecvTimeoutError::Disconnected) => return Err(ClientError::Disconnected),
            }
        }
    }

    /// Sends `request` and discards whatever comes back.
    fn fire(&self, request: Request) {
        if let Ok((id, rx)) = self.request(request) {
            drop(rx);
            self.unregister(id);
        }
    }
}

fn remote(e: ErrorReply, audit: Vec<AuditEntry>) -> ClientError {
    if e.code == ErrorCode::RateLimited {
        return ClientError::RateLimited {
            retry_after: Duration::from_millis(u64::from(e.retry_after_ms.unwrap_or(1))),
        };
    }
    ClientError::Remote(RemoteError {
        code: e.code,
        message: e.message,
        audit,
    })
}

fn unexpected(r: &Response) -> ClientError {
    let name = format!("{r:?}");
    ClientError::Unexpected(name.chars().take(80).collect())
}

// -----------------------------------------------------------------------------
// HelperClient
// -----------------------------------------------------------------------------

/// A connection to the helper. See the module docs.
#[derive(Debug)]
pub struct HelperClient {
    config: ClientConfig,
    inner: Arc<Inner>,
    welcome: Welcome,
    dispatcher: Option<JoinHandle<()>>,
    process: Option<HelperProcess>,
}

impl HelperClient {
    /// Launches (per `config.mode`), connects, verifies the helper and
    /// handshakes.
    ///
    /// # Errors
    ///
    /// [`ClientError::Declined`] when the user declines UAC (fall back to
    /// the walker), [`ClientError::ServiceNotInstalled`],
    /// [`ClientError::HelperExited`], [`ClientError::Rejected`] (version
    /// mismatch), [`ClientError::Untrusted`], or a transport error.
    pub fn connect(config: ClientConfig) -> Result<Self, ClientError> {
        let mut options = config.options.clone();
        if options.server_verifier.is_none() {
            options.server_verifier = helper_verifier(&config.mode)?;
        }
        let (name, mut process) = start(&config.mode)?;
        let pipe = match PipeClient::connect(&name, options) {
            Ok(p) => p,
            Err(e) => {
                let early = process.as_mut().and_then(HelperProcess::exit_code);
                return Err(match (e, early) {
                    (IpcError::Timeout | IpcError::Disconnected, Some(code)) => {
                        ClientError::HelperExited(code)
                    }
                    (e, _) => e.into(),
                });
            }
        };
        let welcome = pipe.welcome().clone();
        let inner = Arc::new(Inner {
            pipe: RwLock::new(Some(pipe)),
            routes: Mutex::default(),
            send_lock: Mutex::new(()),
            closing: AtomicBool::new(false),
            connected: AtomicBool::new(true),
        });
        let d = Arc::clone(&inner);
        let dispatcher = std::thread::Builder::new()
            .name("strata-helper-client".into())
            .spawn(move || d.dispatch())
            .map_err(|e| ClientError::Launch(e.to_string()))?;
        Ok(Self {
            config,
            inner,
            welcome,
            dispatcher: Some(dispatcher),
            process,
        })
    }

    /// Drops the current connection and connects again with the same
    /// configuration (relaunching an on-demand helper, which shows UAC
    /// again).
    ///
    /// # Errors
    ///
    /// As [`HelperClient::connect`].
    pub fn reconnect(&mut self) -> Result<(), ClientError> {
        self.close();
        let fresh = Self::connect(self.config.clone())?;
        *self = fresh;
        Ok(())
    }

    /// Whether the connection is still up.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::SeqCst)
    }

    /// The helper's handshake reply (elevation, capabilities, build).
    #[must_use]
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// The helper process id, when this client started it.
    #[must_use]
    pub fn helper_pid(&self) -> Option<u32> {
        self.process.as_ref().map(HelperProcess::pid)
    }

    /// Round trip; returns the latency.
    ///
    /// # Errors
    ///
    /// Transport errors.
    pub fn ping(&self) -> Result<Duration, ClientError> {
        let t = Instant::now();
        match self
            .inner
            .call(Request::Ping, self.config.request_timeout)?
        {
            (Response::Pong, _) => Ok(t.elapsed()),
            (r, _) => Err(unexpected(&r)),
        }
    }

    /// Volumes as the helper sees them.
    ///
    /// # Errors
    ///
    /// Transport or helper errors.
    pub fn list_volumes(&self) -> Result<Vec<VolumeInfo>, ClientError> {
        match self
            .inner
            .call(Request::ListVolumes, self.config.request_timeout)?
        {
            (Response::Volumes { volumes }, _) => Ok(volumes),
            (r, _) => Err(unexpected(&r)),
        }
    }

    /// Starts an MFT scan and returns its event stream.
    ///
    /// # Errors
    ///
    /// The request could not be sent. Failures during the scan arrive
    /// through the stream.
    pub fn scan_volume(
        &self,
        volume: impl Into<String>,
        options: ScanOptions,
    ) -> Result<ScanStream, ClientError> {
        let (id, rx) = self.inner.request(Request::ScanVolume {
            volume: volume.into(),
            options,
        })?;
        Ok(ScanStream {
            inner: Arc::clone(&self.inner),
            id,
            rx,
            done: false,
        })
    }

    /// Cancels request `request_id`; returns whether it was still running.
    ///
    /// # Errors
    ///
    /// Transport errors.
    pub fn cancel(&self, request_id: u32) -> Result<bool, ClientError> {
        match self
            .inner
            .call(Request::Cancel { request_id }, self.config.request_timeout)?
        {
            (Response::CancelAck { was_running }, _) => Ok(was_running),
            (r, _) => Err(unexpected(&r)),
        }
    }

    /// USN journal state; `None` when the journal is not active.
    ///
    /// # Errors
    ///
    /// Transport or helper errors.
    pub fn query_usn_journal(
        &self,
        volume: impl Into<String>,
    ) -> Result<Option<UsnJournalInfo>, ClientError> {
        let req = Request::QueryUsnJournal {
            volume: volume.into(),
        };
        match self.inner.call(req, self.config.request_timeout)? {
            (Response::UsnJournal(info), _) => Ok(info),
            (r, _) => Err(unexpected(&r)),
        }
    }

    /// Creates the USN journal (call only after the user confirmed enabling
    /// live updates). 0 sizes use the helper's defaults.
    ///
    /// # Errors
    ///
    /// Transport or helper errors (audit records in [`RemoteError`]).
    pub fn create_usn_journal(
        &self,
        volume: impl Into<String>,
        maximum_size: u64,
        allocation_delta: u64,
    ) -> Result<Audited<UsnJournalInfo>, ClientError> {
        let req = Request::CreateUsnJournal {
            volume: volume.into(),
            maximum_size,
            allocation_delta,
        };
        match self.inner.call(req, self.config.request_timeout)? {
            (Response::UsnJournal(Some(value)), audit) => Ok(Audited { value, audit }),
            (r, _) => Err(unexpected(&r)),
        }
    }

    /// Reads journal records (blocking up to `read.wait` for new ones).
    ///
    /// # Errors
    ///
    /// `Remote` with [`ErrorCode::JournalChanged`] or
    /// [`ErrorCode::JournalWrapped`] means "rescan the volume".
    pub fn read_usn(&self, read: &UsnRead) -> Result<UsnChunk, ClientError> {
        let req = Request::ReadUsn {
            volume: read.volume.clone(),
            journal_id: read.journal_id,
            from: read.from,
            max_bytes: read.max_bytes,
            bytes_to_wait_for: read.bytes_to_wait_for,
            timeout_ms: u32::try_from(read.wait.as_millis()).unwrap_or(u32::MAX),
        };
        match self
            .inner
            .call(req, self.config.request_timeout + read.wait)?
        {
            (Response::UsnRecords { next_usn, raw }, _) => Ok(UsnChunk { next_usn, raw }),
            (r, _) => Err(unexpected(&r)),
        }
    }

    /// Re-reads MFT records by reference.
    ///
    /// # Errors
    ///
    /// Transport or helper errors.
    pub fn read_records(
        &self,
        volume: impl Into<String>,
        file_refs: Vec<FileRef>,
    ) -> Result<RecordsReply, ClientError> {
        let req = Request::ReadRecords {
            volume: volume.into(),
            file_refs,
        };
        match self.inner.call(req, self.config.request_timeout)? {
            (Response::Records { records, missing }, _) => Ok(RecordsReply { records, missing }),
            (r, _) => Err(unexpected(&r)),
        }
    }

    /// Deletes a file or folder by id after the helper re-verifies it.
    ///
    /// # Errors
    ///
    /// `Remote` with [`ErrorCode::Protected`] (never-list),
    /// [`ErrorCode::Mismatch`] (changed since the scan) and others; the
    /// error carries the audit records.
    pub fn privileged_delete(
        &self,
        request: DeleteRequest,
    ) -> Result<Audited<DeleteSummary>, ClientError> {
        match self.inner.call(
            Request::PrivilegedDelete(request),
            self.config.request_timeout,
        )? {
            (Response::Deleted { summary, .. }, audit) => Ok(Audited {
                value: summary,
                audit,
            }),
            (r, _) => Err(unexpected(&r)),
        }
    }

    /// Schedules a file for deletion at the next restart (after the user
    /// confirmed the restart-delete prompt).
    ///
    /// # Errors
    ///
    /// As [`HelperClient::privileged_delete`].
    pub fn delete_on_reboot(
        &self,
        request: RebootDeleteRequest,
    ) -> Result<Audited<FileRef>, ClientError> {
        match self.inner.call(
            Request::DeleteOnReboot(request),
            self.config.request_timeout,
        )? {
            (Response::RebootScheduled { file_ref }, audit) => Ok(Audited {
                value: file_ref,
                audit,
            }),
            (r, _) => Err(unexpected(&r)),
        }
    }

    /// Asks the helper to exit and closes the connection.
    ///
    /// # Errors
    ///
    /// Transport errors (a helper that is already gone is not an error).
    pub fn shutdown(mut self) -> Result<(), ClientError> {
        let r = match self
            .inner
            .call(Request::Shutdown, self.config.request_timeout)
        {
            Ok((Response::ShuttingDown, _)) | Err(ClientError::Disconnected) => Ok(()),
            Ok((r, _)) => Err(unexpected(&r)),
            Err(e) => Err(e),
        };
        self.close();
        r
    }

    fn close(&mut self) {
        self.inner.closing.store(true, Ordering::SeqCst);
        if let Some(d) = self.dispatcher.take() {
            let _ = d.join();
        }
        self.inner.connected.store(false, Ordering::SeqCst);
        self.inner.close_pipe();
    }
}

impl Drop for HelperClient {
    fn drop(&mut self) {
        self.close();
    }
}

fn helper_verifier(mode: &LaunchMode) -> Result<Option<Arc<dyn PeerVerifier>>, ClientError> {
    let exe = match mode {
        LaunchMode::OnDemand { helper_exe }
        | LaunchMode::Service { helper_exe }
        | LaunchMode::Spawn { helper_exe, .. } => helper_exe,
        LaunchMode::Pipe { .. } => return Ok(None),
    };
    let policy =
        crate::verify::trust_policy(exe).map_err(|e| ClientError::Launch(e.to_string()))?;
    Ok(Some(Arc::new(policy)))
}

fn on_demand_args(name: &str) -> Result<Vec<OsString>, ClientError> {
    let me = std::env::current_exe().map_err(|e| ClientError::Launch(e.to_string()))?;
    Ok(vec![
        "--pipe".into(),
        name.into(),
        "--client-image".into(),
        me.into_os_string(),
        "--client-pid".into(),
        std::process::id().to_string().into(),
        "--parent-pid".into(),
    ])
}

fn user_sid() -> Result<String, ClientError> {
    strata_win::process::current_user()
        .map(|u| u.sid)
        .map_err(|e| ClientError::Launch(e.to_string()))
}

fn start(mode: &LaunchMode) -> Result<(String, Option<HelperProcess>), ClientError> {
    match mode {
        LaunchMode::Pipe { name } => Ok((name.clone(), None)),
        LaunchMode::OnDemand { helper_exe } => {
            let name =
                session_pipe_name(&user_sid()?).map_err(|e| ClientError::Launch(e.to_string()))?;
            let child =
                launch_elevated(helper_exe, &on_demand_args(&name)?).map_err(|e| match e {
                    LaunchError::Declined => ClientError::Declined,
                    LaunchError::Failed(w) => ClientError::Launch(w.to_string()),
                })?;
            Ok((name, Some(HelperProcess::Elevated(child))))
        }
        LaunchMode::Spawn {
            helper_exe,
            extra_args,
        } => {
            let name =
                session_pipe_name(&user_sid()?).map_err(|e| ClientError::Launch(e.to_string()))?;
            let child = std::process::Command::new(helper_exe)
                .args(on_demand_args(&name)?)
                .args(extra_args)
                .spawn()
                .map_err(|e| ClientError::Launch(e.to_string()))?;
            Ok((name, Some(HelperProcess::Spawned(child))))
        }
        LaunchMode::Service { .. } => {
            crate::service::start_service().map_err(|e| match e {
                crate::service::ServiceError::NotInstalled => ClientError::ServiceNotInstalled,
                other => ClientError::Launch(other.to_string()),
            })?;
            let name = crate::service::service_pipe_name(&user_sid()?)
                .map_err(|e| ClientError::Launch(e.to_string()))?;
            Ok((name, None))
        }
    }
}

// -----------------------------------------------------------------------------
// Scan streams
// -----------------------------------------------------------------------------

/// One event of a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanEvent {
    /// Progress.
    Progress(ScanProgress),
    /// Records, in scanner order.
    Batch(Vec<ScanRecord>),
    /// An audit record for the scan.
    Audit(AuditEntry),
    /// The scan ended (check `stats.cancelled`). Always the last event.
    Done(ScanStats),
}

/// The events of one scan, as an iterator. Ends after `Done` or an error.
///
/// Dropping an unfinished stream cancels the scan.
#[derive(Debug)]
pub struct ScanStream {
    inner: Arc<Inner>,
    id: u32,
    rx: Receiver<Response>,
    done: bool,
}

impl ScanStream {
    /// The scan's request id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Asks the helper to stop; the stream then ends with
    /// `Done { cancelled: true }`.
    ///
    /// # Errors
    ///
    /// The request could not be sent.
    pub fn cancel(&self) -> Result<(), ClientError> {
        let (id, rx) = self.inner.request(Request::Cancel {
            request_id: self.id,
        })?;
        drop(rx);
        self.inner.unregister(id);
        Ok(())
    }

    /// Like `next`, but gives up after `timeout` (`None` then means "no
    /// event yet", not "finished"; check [`ScanStream::is_done`]).
    pub fn next_timeout(&mut self, timeout: Duration) -> Option<Result<ScanEvent, ClientError>> {
        if self.done {
            return None;
        }
        match self.rx.recv_timeout(timeout) {
            Ok(r) => Some(self.convert(r)),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                self.done = true;
                Some(Err(ClientError::Disconnected))
            }
        }
    }

    /// Whether the stream has ended.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.done
    }

    fn convert(&mut self, r: Response) -> Result<ScanEvent, ClientError> {
        match r {
            Response::ScanProgress(p) => Ok(ScanEvent::Progress(p)),
            Response::ScanBatch { records } => Ok(ScanEvent::Batch(records)),
            Response::Audit(a) => Ok(ScanEvent::Audit(a)),
            Response::ScanDone { stats } => {
                self.done = true;
                Ok(ScanEvent::Done(stats))
            }
            Response::Error(e) => {
                self.done = true;
                Err(remote(e, Vec::new()))
            }
            other => {
                self.done = true;
                Err(unexpected(&other))
            }
        }
    }
}

impl Iterator for ScanStream {
    type Item = Result<ScanEvent, ClientError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.rx.recv() {
            Ok(r) => Some(self.convert(r)),
            Err(_) => {
                self.done = true;
                Some(Err(ClientError::Disconnected))
            }
        }
    }
}

impl Drop for ScanStream {
    fn drop(&mut self) {
        self.inner.unregister(self.id);
        if !self.done && self.inner.connected.load(Ordering::SeqCst) {
            self.inner.fire(Request::Cancel {
                request_id: self.id,
            });
        }
    }
}
