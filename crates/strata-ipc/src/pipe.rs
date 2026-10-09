//! Named-pipe transport.
//!
//! Design: one overlapped pipe handle per connection, driven by blocking
//! calls. Every read and write is issued as overlapped I/O and then waited on
//! by the calling thread, which gives:
//!
//! - Full duplex across threads. A synchronous pipe handle serializes all
//!   I/O on it, so a reader blocked in `ReadFile` would stall a writer; with
//!   overlapped I/O one thread can stream `ScanBatch`es while another waits
//!   for `Cancel`.
//! - Timeouts. A pending operation is cancelled with `CancelIoEx` and then
//!   always reaped with `GetOverlappedResult(.., TRUE)` before its buffer and
//!   `OVERLAPPED` go out of scope, so the kernel never writes to freed memory.
//!
//! Reads are serialized by one mutex and writes by another, so frames are
//! never interleaved. The helper serves a single client per session: the
//! pipe has exactly one instance, reused across reconnects with
//! `DisconnectNamedPipe`/`ConnectNamedPipe`, so the name never disappears
//! (no window for a squatter) and no second client can connect concurrently.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use strata_win::{OwnedHandle, WinError};
use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
    HANDLE, WAIT_TIMEOUT,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::PCWSTR;

use crate::error::IpcError;
use crate::frame::{LEN_SIZE, decode_frame, encode_frame, peek_len};
use crate::protocol::{
    Capabilities, Envelope, ErrorCode, ErrorReply, HANDSHAKE_ID, HandshakeReject, Hello, Message,
    PROTOCOL_VERSION, Request, Response, Welcome,
};
use crate::rate::{RateLimit, TokenBucket};
use crate::security::{
    CLIENT_PIPE_ACCESS, PeerIdentity, PeerVerifier, SecurityDescriptor, TrustError,
    is_valid_pipe_name,
};

/// Kernel buffer size per direction.
const PIPE_BUFFER: u32 = 1024 * 1024;
/// Smallest read issued.
const MIN_READ: usize = 64 * 1024;
/// Largest single read issued (bounded by the frame being read).
const MAX_READ: usize = 4 * 1024 * 1024;
/// How long a rejected peer is given to read the rejection before the pipe
/// is disconnected.
const REJECT_LINGER: Duration = Duration::from_secs(2);

// -----------------------------------------------------------------------------
// Overlapped I/O core
// -----------------------------------------------------------------------------

fn new_event() -> Result<OwnedHandle, WinError> {
    // SAFETY: anonymous manual-reset event; the handle is owned below.
    let h = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .map_err(|e| WinError::new("CreateEventW", &e))?;
    // SAFETY: fresh handle from CreateEventW.
    unsafe { OwnedHandle::from_raw(h) }.ok_or_else(|| WinError::from_win32("CreateEventW", 6))
}

fn is_disconnect(e: &windows::core::Error) -> bool {
    [ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED]
        .iter()
        .any(|c| e.code() == c.to_hresult())
}

fn map_io(op: &'static str, e: &windows::core::Error) -> IpcError {
    if is_disconnect(e) {
        IpcError::Disconnected
    } else {
        IpcError::Win(WinError::new(op, e))
    }
}

/// Runs one overlapped operation to completion.
///
/// Returns `Ok(Some(bytes))` when it completed, `Ok(None)` when `timeout`
/// elapsed and the operation was cancelled.
///
/// # Safety
///
/// `start` must issue at most one overlapped operation on `handle` using the
/// `OVERLAPPED` it is given, with buffers that stay valid until this function
/// returns. This function guarantees the operation has fully completed
/// (successfully, with an error, or cancelled) before it returns.
unsafe fn run_overlapped(
    handle: HANDLE,
    event: HANDLE,
    timeout: Option<Duration>,
    start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
) -> windows::core::Result<Option<u32>> {
    let mut ov = OVERLAPPED {
        hEvent: event,
        ..Default::default()
    };
    match start(&mut ov) {
        Ok(()) => {}
        Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {}
        // NOTE: the operation never started, so nothing references `ov`.
        Err(e) => return Err(e),
    }
    if let Some(t) = timeout {
        let ms = u32::try_from(t.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: `event` is a live event handle.
        if unsafe { WaitForSingleObject(event, ms) } == WAIT_TIMEOUT {
            // SAFETY: cancels only the operation identified by `ov`.
            unsafe {
                let _ = CancelIoEx(handle, Some(&ov));
            }
        }
    }
    let mut n = 0u32;
    // IMPORTANT: always wait for completion, even after a cancel, so the
    // kernel is done with `ov` and the caller's buffer before they are freed.
    // SAFETY: `ov` is the OVERLAPPED of the operation started above.
    match unsafe { GetOverlappedResult(handle, &ov, &mut n, true) } {
        Ok(()) => Ok(Some(n)),
        Err(e) if e.code() == ERROR_OPERATION_ABORTED.to_hresult() => Ok(None),
        Err(e) => Err(e),
    }
}

#[derive(Debug)]
struct Reader {
    event: OwnedHandle,
    buf: Vec<u8>,
}

#[derive(Debug)]
struct Writer {
    event: OwnedHandle,
    buf: Vec<u8>,
}

/// One end of a connected pipe.
#[derive(Debug)]
struct PipeIo {
    handle: OwnedHandle,
    reader: Mutex<Reader>,
    writer: Mutex<Writer>,
    /// Set after a framing error or a partial write: the stream is out of
    /// sync and every further operation fails.
    poisoned: AtomicBool,
    write_timeout: Duration,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // NOTE: a panic while holding the lock leaves at worst a partial buffer,
    // which the poison flag already covers.
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl PipeIo {
    fn new(handle: OwnedHandle, write_timeout: Duration) -> Result<Self, WinError> {
        Ok(Self {
            handle,
            reader: Mutex::new(Reader {
                event: new_event()?,
                buf: Vec::new(),
            }),
            writer: Mutex::new(Writer {
                event: new_event()?,
                buf: Vec::new(),
            }),
            poisoned: AtomicBool::new(false),
            write_timeout,
        })
    }

    fn raw(&self) -> HANDLE {
        self.handle.raw()
    }

    fn poison(&self) {
        self.poisoned.store(true, Ordering::SeqCst);
    }

    fn check(&self) -> Result<(), IpcError> {
        if self.poisoned.load(Ordering::SeqCst) {
            Err(IpcError::Disconnected)
        } else {
            Ok(())
        }
    }

    /// Clears per-connection state before a server instance is reused.
    fn reset(&self) {
        lock(&self.reader).buf.clear();
        self.poisoned.store(false, Ordering::SeqCst);
    }

    /// Receives one frame. `Ok(None)` when `timeout` elapses first; partial
    /// data stays buffered for the next call.
    fn recv(&self, timeout: Option<Duration>) -> Result<Option<Envelope>, IpcError> {
        self.check()?;
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut r = lock(&self.reader);
        loop {
            match decode_frame(&r.buf) {
                Ok(Some((env, used))) => {
                    r.buf.drain(..used);
                    return Ok(Some(env));
                }
                Ok(None) => {}
                Err(e) => {
                    self.poison();
                    return Err(e.into());
                }
            }
            // NOTE: the frame length is validated before its body is read, so
            // a hostile length can make us buffer at most MAX_FRAME_LEN.
            let want = match peek_len(&r.buf) {
                Ok(Some(len)) => (LEN_SIZE + len as usize)
                    .saturating_sub(r.buf.len())
                    .clamp(MIN_READ, MAX_READ),
                _ => MIN_READ,
            };
            let remaining = match deadline {
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(None);
                    }
                    Some(left)
                }
                None => None,
            };
            let old = r.buf.len();
            r.buf.resize(old + want, 0);
            let event = r.event.raw();
            let handle = self.raw();
            let slice = &mut r.buf[old..];
            // SAFETY: one ReadFile into `slice`, which lives (and is not
            // reallocated) until run_overlapped has reaped the operation.
            let result = unsafe {
                run_overlapped(handle, event, remaining, |ov| {
                    ReadFile(handle, Some(slice), None, Some(ov))
                })
            };
            match result {
                Ok(Some(n)) => r.buf.truncate(old + n as usize),
                Ok(None) => {
                    r.buf.truncate(old);
                    return Ok(None);
                }
                Err(e) => {
                    r.buf.truncate(old);
                    return Err(map_io("ReadFile", &e));
                }
            }
        }
    }

    /// Sends one frame, all or nothing from the caller's view: a partial
    /// write poisons the connection.
    fn send(&self, env: &Envelope) -> Result<(), IpcError> {
        self.check()?;
        let mut w = lock(&self.writer);
        let mut buf = std::mem::take(&mut w.buf);
        buf.clear();
        let result = encode_frame(env, &mut buf)
            .map_err(IpcError::from)
            .and_then(|()| self.write_all(w.event.raw(), &buf));
        w.buf = buf;
        result
    }

    fn write_all(&self, event: HANDLE, bytes: &[u8]) -> Result<(), IpcError> {
        let mut off = 0;
        while off < bytes.len() {
            let chunk = &bytes[off..];
            let handle = self.raw();
            // SAFETY: one WriteFile from `chunk`, which outlives the call.
            let result = unsafe {
                run_overlapped(handle, event, Some(self.write_timeout), |ov| {
                    WriteFile(handle, Some(chunk), None, Some(ov))
                })
            };
            match result {
                Ok(Some(0)) | Ok(None) => {
                    self.poison();
                    return Err(IpcError::Timeout);
                }
                Ok(Some(n)) => off += n as usize,
                Err(e) => {
                    self.poison();
                    return Err(map_io("WriteFile", &e));
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn write_raw(&self, bytes: &[u8]) -> Result<(), IpcError> {
        let w = lock(&self.writer);
        self.write_all(w.event.raw(), bytes)
    }

    /// Reads and discards until the peer disconnects or `timeout` elapses.
    fn linger(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut scratch = vec![0u8; 4096];
        let reader = lock(&self.reader);
        let event = reader.event.raw();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            let handle = self.raw();
            let slice = &mut scratch[..];
            // SAFETY: one ReadFile into `scratch`, reaped before returning.
            let r = unsafe {
                run_overlapped(handle, event, Some(left), |ov| {
                    ReadFile(handle, Some(slice), None, Some(ov))
                })
            };
            if !matches!(r, Ok(Some(n)) if n > 0) {
                return;
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Server
// -----------------------------------------------------------------------------

/// Server (helper) configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Pipe name from [`crate::security::session_pipe_name`].
    pub pipe_name: String,
    /// The interactive user's SID: the only non-SYSTEM principal granted
    /// access in the pipe's DACL.
    pub user_sid: String,
    /// Client verification (image path + signer).
    pub verifier: Arc<dyn PeerVerifier>,
    /// Helper build string sent in `Welcome`.
    pub helper_build: String,
    /// Whether the helper is elevated (sent in `Welcome`).
    pub elevated: bool,
    /// Capabilities sent in `Welcome`.
    pub capabilities: Capabilities,
    /// Per-connection request rate limit.
    pub rate_limit: RateLimit,
    /// How long the client has to send `Hello` after connecting.
    pub handshake_timeout: Duration,
    /// How long a write may block on a client that is not reading.
    pub write_timeout: Duration,
}

impl ServerConfig {
    /// Defaults: 5 s handshake, 30 s write timeout, [`RateLimit::default`].
    pub fn new(
        pipe_name: impl Into<String>,
        user_sid: impl Into<String>,
        verifier: Arc<dyn PeerVerifier>,
    ) -> Self {
        Self {
            pipe_name: pipe_name.into(),
            user_sid: user_sid.into(),
            verifier,
            helper_build: env!("CARGO_PKG_VERSION").to_owned(),
            elevated: false,
            capabilities: Capabilities::default(),
            rate_limit: RateLimit::default(),
            handshake_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(30),
        }
    }
}

/// The helper's end of the pipe: a single, reusable instance.
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
/// use strata_ipc::pipe::{PipeServer, ServerConfig};
/// use strata_ipc::protocol::{Request, Response};
/// use strata_ipc::security::TrustPolicy;
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let name = std::env::args().nth(1).unwrap();
/// let app = TrustPolicy::sibling_of_current_exe("strata.exe")?;
/// let sid = strata_win::process::current_user()?.sid;
/// let config = ServerConfig::new(name, sid, Arc::new(TrustPolicy::signed(app)?));
/// let mut server = PipeServer::create(config)?;
/// let conn = server.accept(None)?;
/// while let Ok(Some((id, req))) = conn.recv_request(None) {
///     if req == Request::Ping {
///         conn.send(id, Response::Pong)?;
///     }
/// }
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct PipeServer {
    io: Arc<PipeIo>,
    connect_event: OwnedHandle,
    config: ServerConfig,
    // NOTE: kept alive for the pipe's lifetime for clarity; the kernel copies
    // the descriptor at creation.
    _security: SecurityDescriptor,
}

impl PipeServer {
    /// Creates the pipe (first and only instance).
    ///
    /// Fails if the name is malformed or already exists (someone squatted it,
    /// or another helper is running).
    pub fn create(config: ServerConfig) -> Result<Self, IpcError> {
        // SECURITY: the name arrives on the helper's command line; refuse
        // anything that is not our own format.
        if !is_valid_pipe_name(&config.pipe_name) {
            return Err(IpcError::Win(WinError::from_win32(
                "invalid pipe name",
                123,
            )));
        }
        let security = SecurityDescriptor::for_pipe(&config.user_sid)?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.as_ptr(),
            bInheritHandle: false.into(),
        };
        let name: Vec<u16> = config
            .pipe_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SECURITY: FIRST_PIPE_INSTANCE fails if the name already exists, so
        // a pre-created (squatted) pipe is detected instead of joined;
        // REJECT_REMOTE_CLIENTS keeps the pipe off the network; one instance
        // means a second client gets ERROR_PIPE_BUSY.
        // SAFETY: `name` is NUL-terminated and `sa` points to a valid
        // descriptor; both outlive the call.
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                Some(&sa),
            )
        };
        // SAFETY: CreateNamedPipeW returned a handle we own (or invalid).
        let handle = unsafe { OwnedHandle::from_raw(h) }
            .ok_or_else(|| WinError::last("CreateNamedPipeW"))?;
        Ok(Self {
            io: Arc::new(PipeIo::new(handle, config.write_timeout)?),
            connect_event: new_event()?,
            config,
            _security: security,
        })
    }

    /// The pipe handle (for inspection, e.g. reading back its DACL).
    #[must_use]
    pub fn raw_handle(&self) -> HANDLE {
        self.io.raw()
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Waits for a client, verifies it, and performs the handshake.
    ///
    /// On any failure the client is disconnected and the pipe is ready for
    /// the next `accept`. Fails with [`IpcError::Busy`] while a previous
    /// [`ServerConnection`] is still alive.
    pub fn accept(&mut self, timeout: Option<Duration>) -> Result<ServerConnection, IpcError> {
        if Arc::strong_count(&self.io) > 1 {
            return Err(IpcError::Busy);
        }
        self.io.reset();
        self.wait_for_client(timeout)?;
        match self.handshake() {
            Ok((peer, hello)) => Ok(ServerConnection {
                io: Arc::clone(&self.io),
                peer,
                hello,
                bucket: Mutex::new(TokenBucket::new(self.config.rate_limit, Instant::now())),
            }),
            Err(e) => {
                disconnect(&self.io);
                Err(e)
            }
        }
    }

    fn wait_for_client(&self, timeout: Option<Duration>) -> Result<(), IpcError> {
        let handle = self.io.raw();
        // SAFETY: one ConnectNamedPipe on our pipe, reaped by run_overlapped.
        let r = unsafe {
            run_overlapped(handle, self.connect_event.raw(), timeout, |ov| {
                ConnectNamedPipe(handle, Some(ov))
            })
        };
        match r {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(IpcError::Timeout),
            // NOTE: the client connected between CreateNamedPipe and
            // ConnectNamedPipe; that is a success.
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => Ok(()),
            Err(e) => Err(IpcError::Win(WinError::new("ConnectNamedPipe", &e))),
        }
    }

    fn handshake(&self) -> Result<(PeerIdentity, Hello), IpcError> {
        let mut pid = 0u32;
        // SAFETY: `pid` is a valid out-pointer; the pipe is connected.
        unsafe { GetNamedPipeClientProcessId(self.io.raw(), &mut pid) }
            .map_err(|e| WinError::new("GetNamedPipeClientProcessId", &e))?;
        // SECURITY: verify the peer before parsing a single byte from it.
        let verified = PeerIdentity::of_pid(pid)
            .map_err(TrustError::Inspect)
            .and_then(|peer| self.config.verifier.verify(&peer).map(|()| peer));
        let peer = match verified {
            Ok(p) => p,
            Err(e) => {
                self.reject(HandshakeReject::Untrusted);
                return Err(IpcError::Untrusted(e));
            }
        };
        let hello = match self.io.recv(Some(self.config.handshake_timeout)) {
            Ok(Some(Envelope {
                message: Message::Hello(h),
                ..
            })) => h,
            Ok(None) => return Err(IpcError::Timeout),
            Ok(Some(_)) | Err(IpcError::Frame(_)) => {
                self.reject(HandshakeReject::Malformed);
                return Err(IpcError::Rejected(HandshakeReject::Malformed));
            }
            Err(e) => return Err(e),
        };
        if hello.protocol != PROTOCOL_VERSION {
            let r = HandshakeReject::VersionMismatch {
                helper: PROTOCOL_VERSION,
                client: hello.protocol,
            };
            self.reject(r.clone());
            return Err(IpcError::Rejected(r));
        }
        // SECURITY: the PID in Hello is informational; the kernel-reported
        // PID is authoritative. A mismatch means something is relaying.
        if hello.client_pid != pid {
            self.reject(HandshakeReject::PidMismatch);
            return Err(IpcError::Rejected(HandshakeReject::PidMismatch));
        }
        self.io.send(&Envelope {
            request_id: HANDSHAKE_ID,
            message: Message::Welcome(Welcome {
                protocol: PROTOCOL_VERSION,
                helper_build: self.config.helper_build.clone(),
                elevated: self.config.elevated,
                capabilities: self.config.capabilities,
            }),
        })?;
        Ok((peer, hello))
    }

    fn reject(&self, reason: HandshakeReject) {
        let sent = self.io.send(&Envelope {
            request_id: HANDSHAKE_ID,
            message: Message::Reject(reason),
        });
        if sent.is_ok() {
            // NOTE: DisconnectNamedPipe discards unread data, so give the
            // client a moment to read the rejection and hang up first.
            self.io.linger(REJECT_LINGER);
        }
    }
}

fn disconnect(io: &PipeIo) {
    io.poison();
    // SAFETY: plain call on our pipe handle; failure means already
    // disconnected.
    unsafe {
        let _ = DisconnectNamedPipe(io.raw());
    }
}

/// An accepted, verified, handshaken client. Dropping it disconnects the
/// client and frees the pipe for the next [`PipeServer::accept`].
///
/// `recv_request` and `send` may be called from different threads at the
/// same time (share it with `Arc` or scoped threads).
#[derive(Debug)]
pub struct ServerConnection {
    io: Arc<PipeIo>,
    peer: PeerIdentity,
    hello: Hello,
    bucket: Mutex<TokenBucket>,
}

impl ServerConnection {
    /// The verified client process.
    #[must_use]
    pub fn peer(&self) -> &PeerIdentity {
        &self.peer
    }

    /// The client's `Hello`.
    #[must_use]
    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Receives the next request. `Ok(None)` on timeout.
    ///
    /// A request over the rate limit is answered with
    /// [`ErrorCode::RateLimited`] here and reported as
    /// [`IpcError::RateLimited`]; the connection stays usable. Request id 0
    /// is reserved and answered with [`ErrorCode::BadRequest`].
    pub fn recv_request(
        &self,
        timeout: Option<Duration>,
    ) -> Result<Option<(u32, Request)>, IpcError> {
        let Some(env) = self.io.recv(timeout)? else {
            return Ok(None);
        };
        let Message::Request(req) = env.message else {
            // SECURITY: a client never sends handshake or response frames
            // after the handshake; treat it as hostile and hang up.
            disconnect(&self.io);
            return Err(IpcError::Unexpected("non-request frame from client"));
        };
        if let Err(wait) = lock(&self.bucket).try_take(Instant::now()) {
            let retry_ms = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX).max(1);
            self.send(
                env.request_id,
                Response::Error(ErrorReply {
                    code: ErrorCode::RateLimited,
                    message: "too many requests".into(),
                    retry_after_ms: Some(retry_ms),
                }),
            )?;
            return Err(IpcError::RateLimited {
                request_id: env.request_id,
                retry_after: wait,
            });
        }
        if env.request_id == HANDSHAKE_ID {
            self.send(
                HANDSHAKE_ID,
                Response::Error(ErrorReply {
                    code: ErrorCode::BadRequest,
                    message: "request id 0 is reserved".into(),
                    retry_after_ms: None,
                }),
            )?;
            return Err(IpcError::Unexpected("request id 0"));
        }
        Ok(Some((env.request_id, req)))
    }

    /// Sends a response or event for `request_id`.
    pub fn send(&self, request_id: u32, response: Response) -> Result<(), IpcError> {
        self.io.send(&Envelope {
            request_id,
            message: Message::Response(response),
        })
    }

    /// Disconnects after giving the client up to `linger` to read what was
    /// sent (e.g. after `ShuttingDown`).
    pub fn close(self, linger: Duration) {
        self.io.linger(linger);
    }
}

impl Drop for ServerConnection {
    fn drop(&mut self) {
        disconnect(&self.io);
    }
}

// -----------------------------------------------------------------------------
// Client
// -----------------------------------------------------------------------------

/// Client (app) options.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Client build string sent in `Hello`.
    pub build: String,
    /// Total time to wait for the pipe to appear and accept the connection.
    pub connect_timeout: Duration,
    /// Time to wait for `Welcome` after `Hello`.
    pub handshake_timeout: Duration,
    /// How long a write may block on a helper that is not reading.
    pub write_timeout: Duration,
    /// Verifies the server process (the mutual half of the check). `None`
    /// skips it; the app always sets it.
    pub server_verifier: Option<Arc<dyn PeerVerifier>>,
    /// Protocol version to announce. Leave at [`PROTOCOL_VERSION`]; other
    /// values exist to exercise the mismatch path.
    pub protocol: u32,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            build: env!("CARGO_PKG_VERSION").to_owned(),
            connect_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(30),
            server_verifier: None,
            protocol: PROTOCOL_VERSION,
        }
    }
}

/// The app's end of the pipe.
///
/// # Example
///
/// ```no_run
/// use strata_ipc::pipe::{ClientOptions, PipeClient};
/// use strata_ipc::protocol::{Request, Response};
/// # fn main() -> Result<(), strata_ipc::IpcError> {
/// let client = PipeClient::connect(r"\\.\pipe\strata-helper-...", ClientOptions::default())?;
/// assert_eq!(client.call(Request::Ping, None)?, Response::Pong);
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct PipeClient {
    io: PipeIo,
    welcome: Welcome,
    server: Option<PeerIdentity>,
    next_id: AtomicU32,
    pending: Mutex<std::collections::VecDeque<(u32, Response)>>,
}

impl PipeClient {
    /// Connects, verifies the server if configured, and handshakes.
    pub fn connect(name: &str, opts: ClientOptions) -> Result<Self, IpcError> {
        let deadline = Instant::now() + opts.connect_timeout;
        let handle = open_client(name, deadline)?;
        let io = PipeIo::new(handle, opts.write_timeout)?;
        let server = match &opts.server_verifier {
            Some(v) => {
                let mut pid = 0u32;
                // SAFETY: `pid` is a valid out-pointer; the pipe is open.
                unsafe { GetNamedPipeServerProcessId(io.raw(), &mut pid) }
                    .map_err(|e| WinError::new("GetNamedPipeServerProcessId", &e))?;
                let peer = PeerIdentity::of_pid(pid)
                    .map_err(|e| IpcError::Untrusted(TrustError::Inspect(e)))?;
                v.verify(&peer).map_err(IpcError::Untrusted)?;
                Some(peer)
            }
            None => None,
        };
        io.send(&Envelope {
            request_id: HANDSHAKE_ID,
            message: Message::Hello(Hello {
                protocol: opts.protocol,
                build: opts.build.clone(),
                client_pid: std::process::id(),
            }),
        })?;
        let welcome = match io.recv(Some(opts.handshake_timeout))? {
            Some(Envelope {
                message: Message::Welcome(w),
                ..
            }) => w,
            Some(Envelope {
                message: Message::Reject(r),
                ..
            }) => return Err(IpcError::Rejected(r)),
            Some(_) => return Err(IpcError::Unexpected("non-handshake frame from helper")),
            None => return Err(IpcError::Timeout),
        };
        if welcome.protocol != opts.protocol {
            return Err(IpcError::Rejected(HandshakeReject::VersionMismatch {
                helper: welcome.protocol,
                client: opts.protocol,
            }));
        }
        Ok(Self {
            io,
            welcome,
            server,
            next_id: AtomicU32::new(1),
            pending: Mutex::new(std::collections::VecDeque::new()),
        })
    }

    /// The helper's `Welcome`.
    #[must_use]
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// The verified helper process, when server verification was enabled.
    #[must_use]
    pub fn server(&self) -> Option<&PeerIdentity> {
        self.server.as_ref()
    }

    /// Sends a request and returns its id.
    pub fn send(&self, request: Request) -> Result<u32, IpcError> {
        let mut id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if id == HANDSHAKE_ID {
            id = self.next_id.fetch_add(1, Ordering::Relaxed);
        }
        self.io.send(&Envelope {
            request_id: id,
            message: Message::Request(request),
        })?;
        Ok(id)
    }

    /// Receives the next response or event. `Ok(None)` on timeout.
    ///
    /// A rate-limit refusal is returned as [`IpcError::RateLimited`]; other
    /// request failures arrive as [`Response::Error`]. A helper crash or exit
    /// surfaces as [`IpcError::Disconnected`].
    pub fn recv(&self, timeout: Option<Duration>) -> Result<Option<(u32, Response)>, IpcError> {
        if let Some(p) = lock(&self.pending).pop_front() {
            return Self::classify(p).map(Some);
        }
        match self.io.recv(timeout)? {
            Some(Envelope {
                request_id,
                message: Message::Response(r),
            }) => Self::classify((request_id, r)).map(Some),
            Some(_) => {
                self.io.poison();
                Err(IpcError::Unexpected("non-response frame from helper"))
            }
            None => Ok(None),
        }
    }

    fn classify((id, r): (u32, Response)) -> Result<(u32, Response), IpcError> {
        match r {
            Response::Error(ErrorReply {
                code: ErrorCode::RateLimited,
                retry_after_ms,
                ..
            }) => Err(IpcError::RateLimited {
                request_id: id,
                retry_after: Duration::from_millis(u64::from(retry_after_ms.unwrap_or(1))),
            }),
            other => Ok((id, other)),
        }
    }

    /// Sends `request` and waits for the first response with its id.
    /// Responses to other requests that arrive meanwhile are kept for
    /// [`PipeClient::recv`].
    pub fn call(&self, request: Request, timeout: Option<Duration>) -> Result<Response, IpcError> {
        let id = self.send(request)?;
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            let left = match deadline {
                Some(d) => {
                    let l = d.saturating_duration_since(Instant::now());
                    if l.is_zero() {
                        return Err(IpcError::Timeout);
                    }
                    Some(l)
                }
                None => None,
            };
            let Some(env) = self.io.recv(left)? else {
                return Err(IpcError::Timeout);
            };
            match env.message {
                Message::Response(r) if env.request_id == id => {
                    return Self::classify((id, r)).map(|(_, r)| r);
                }
                Message::Response(r) => lock(&self.pending).push_back((env.request_id, r)),
                _ => {
                    self.io.poison();
                    return Err(IpcError::Unexpected("non-response frame from helper"));
                }
            }
        }
    }

    #[cfg(test)]
    fn write_raw(&self, bytes: &[u8]) -> Result<(), IpcError> {
        self.io.write_raw(bytes)
    }
}

fn open_client(name: &str, deadline: Instant) -> Result<OwnedHandle, IpcError> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    loop {
        // SECURITY: SECURITY_IDENTIFICATION limits a (possibly impostor)
        // server to identifying us; it cannot impersonate the app's token.
        // SAFETY: `wide` is NUL-terminated; the handle is owned below.
        let r = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                CLIENT_PIPE_ACCESS,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                None,
            )
        };
        let err = match r {
            Ok(h) => {
                // SAFETY: fresh handle from CreateFileW.
                return unsafe { OwnedHandle::from_raw(h) }
                    .ok_or_else(|| IpcError::Win(WinError::from_win32("CreateFileW", 6)));
            }
            Err(e) => e,
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if err.code() == ERROR_PIPE_BUSY.to_hresult() {
            if left.is_zero() {
                return Err(IpcError::Busy);
            }
            let ms = u32::try_from(left.as_millis())
                .unwrap_or(u32::MAX - 1)
                .max(1);
            // SAFETY: `wide` is NUL-terminated.
            if !unsafe { WaitNamedPipeW(PCWSTR(wide.as_ptr()), ms) }.as_bool() {
                return Err(IpcError::Busy);
            }
        } else if err.code() == ERROR_FILE_NOT_FOUND.to_hresult() {
            // NOTE: the helper may still be starting (UAC prompt, process
            // start); WaitNamedPipeW returns immediately for a missing pipe,
            // so poll.
            if left.is_zero() {
                return Err(IpcError::Timeout);
            }
            std::thread::sleep(left.min(Duration::from_millis(20)));
        } else {
            return Err(IpcError::Win(WinError::new("CreateFileW", &err)));
        }
    }
}

#[cfg(test)]
#[path = "pipe_tests.rs"]
mod tests;
