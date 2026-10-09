//! The helper's pipe server: accept loop and per-connection request loop
//! (SPEC §4).
//!
//! Concurrency: one client per pipe at a time (the pipe has a single
//! instance). For each connection the calling thread is the *reader*: it
//! receives requests and answers `Ping`, `Cancel` and `Shutdown` itself, so
//! they stay responsive during a scan. Every other request runs on its own
//! scoped worker thread that sends its own responses; frames never
//! interleave because the connection serializes writes. When the
//! connection ends, every in-flight request is cancelled and joined before
//! the next client is accepted.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use strata_ipc::IpcError;
use strata_ipc::pipe::{PipeServer, ServerConfig, ServerConnection};
use strata_ipc::protocol::{ErrorCode, Request, Response};

use crate::cancel::Cancel;
use crate::diag::diag;
use crate::error::HelperError;
use crate::ops::{self, RequestCtx, Shared};
use crate::watch::ProcessWatch;

/// How often the reader wakes up to check the parent, the stop flag and the
/// idle timer.
const POLL: Duration = Duration::from_millis(250);
/// How long a client gets to read `ShuttingDown` before the pipe closes.
const SHUTDOWN_LINGER: Duration = Duration::from_secs(2);

/// Activity shared across the pipes of one process (service idle exit).
#[derive(Debug)]
pub struct Activity {
    last: Mutex<Instant>,
    connected: AtomicUsize,
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            last: Mutex::new(Instant::now()),
            connected: AtomicUsize::new(0),
        }
    }
}

impl Activity {
    /// Records activity now.
    pub fn touch(&self) {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    /// Whether nothing happened for `idle` and no client is connected.
    #[must_use]
    pub fn idle_for(&self, idle: Duration) -> bool {
        self.connected.load(Ordering::SeqCst) == 0
            && self
                .last
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .elapsed()
                >= idle
    }
}

/// Everything one pipe server needs.
#[derive(Debug)]
pub struct HelperConfig {
    /// Pipe name, user SID, client verifier, capabilities, rate limit.
    pub server: ServerConfig,
    /// Exit after the first client disconnects (on-demand mode).
    pub exit_on_disconnect: bool,
    /// Exit when no client connects within this time.
    pub accept_timeout: Option<Duration>,
    /// Close the connection (and exit, with `exit_on_disconnect`) after this
    /// long without requests while nothing is in flight.
    pub idle_timeout: Option<Duration>,
    /// Exit when this process exits (the launching app).
    pub parent: Option<ProcessWatch>,
    /// Requests running at once per connection; more get `Busy`.
    pub max_in_flight: usize,
    /// External stop request (service stop, tests).
    pub stop: Arc<AtomicBool>,
    /// Shared activity tracking (service idle exit).
    pub activity: Option<Arc<Activity>>,
}

impl HelperConfig {
    /// On-demand defaults around `server`: exit on disconnect, 60 s to
    /// connect, 30 min idle, 16 requests in flight.
    #[must_use]
    pub fn on_demand(server: ServerConfig) -> Self {
        Self {
            server,
            exit_on_disconnect: true,
            accept_timeout: Some(Duration::from_secs(60)),
            idle_timeout: Some(Duration::from_secs(30 * 60)),
            parent: None,
            max_in_flight: 16,
            stop: Arc::new(AtomicBool::new(false)),
            activity: None,
        }
    }
}

/// Why [`serve`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// The client disconnected (or the connection broke) in on-demand mode.
    ClientDisconnected,
    /// The client sent `Shutdown`.
    Shutdown,
    /// Nobody connected within the accept timeout.
    NoClient,
    /// The connection was idle for the idle timeout.
    Idle,
    /// The watched parent process exited.
    ParentExited,
    /// The stop flag was set.
    Stopped,
}

/// Creates the pipe and serves clients until an exit condition.
///
/// # Errors
///
/// The pipe cannot be created (malformed or squatted name, invalid SID).
pub fn serve(config: HelperConfig, shared: &Shared) -> Result<ExitReason, HelperError> {
    let mut server = PipeServer::create(config.server.clone()).map_err(HelperError::from)?;
    serve_on(&mut server, &config, shared)
}

/// [`serve`] on an existing pipe.
///
/// # Errors
///
/// An unrecoverable pipe error while waiting for clients.
pub fn serve_on(
    server: &mut PipeServer,
    config: &HelperConfig,
    shared: &Shared,
) -> Result<ExitReason, HelperError> {
    let mut waiting_since = Instant::now();
    loop {
        if let Some(reason) = should_exit(config) {
            return Ok(reason);
        }
        if config
            .accept_timeout
            .is_some_and(|t| waiting_since.elapsed() >= t)
        {
            return Ok(ExitReason::NoClient);
        }
        let conn = match server.accept(Some(POLL)) {
            Ok(c) => c,
            Err(IpcError::Timeout) => continue,
            Err(e @ (IpcError::Untrusted(_) | IpcError::Rejected(_))) => {
                diag!("refused a client: {e}");
                continue;
            }
            Err(e @ (IpcError::Disconnected | IpcError::Frame(_) | IpcError::Unexpected(_))) => {
                diag!("handshake failed: {e}");
                continue;
            }
            Err(e) => return Err(HelperError::from(e)),
        };
        if let Some(a) = &config.activity {
            a.connected.fetch_add(1, Ordering::SeqCst);
            a.touch();
        }
        let reason = Connection::new(&conn, config, shared).run();
        if let Some(a) = &config.activity {
            a.connected.fetch_sub(1, Ordering::SeqCst);
            a.touch();
        }
        match reason {
            ConnEnd::Shutdown => {
                conn.close(SHUTDOWN_LINGER);
                return Ok(ExitReason::Shutdown);
            }
            ConnEnd::Exit(r) => return Ok(r),
            ConnEnd::Closed(r) => {
                drop(conn);
                if config.exit_on_disconnect {
                    return Ok(r);
                }
                waiting_since = Instant::now();
            }
        }
    }
}

fn should_exit(config: &HelperConfig) -> Option<ExitReason> {
    if config.stop.load(Ordering::SeqCst) {
        return Some(ExitReason::Stopped);
    }
    if config.parent.as_ref().is_some_and(|p| !p.is_alive()) {
        return Some(ExitReason::ParentExited);
    }
    None
}

/// How a connection ended.
enum ConnEnd {
    /// `Shutdown` was requested; close with linger and exit.
    Shutdown,
    /// Exit immediately (parent gone, stop flag).
    Exit(ExitReason),
    /// The connection is over; with `exit_on_disconnect`, exit with this.
    Closed(ExitReason),
}

struct Connection<'a> {
    conn: &'a ServerConnection,
    config: &'a HelperConfig,
    shared: &'a Shared,
    in_flight: Mutex<HashMap<u32, Cancel>>,
    scanning: AtomicBool,
}

impl<'a> Connection<'a> {
    fn new(conn: &'a ServerConnection, config: &'a HelperConfig, shared: &'a Shared) -> Self {
        Self {
            conn,
            config,
            shared,
            in_flight: Mutex::new(HashMap::new()),
            scanning: AtomicBool::new(false),
        }
    }

    fn in_flight(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Cancel>> {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn run(&self) -> ConnEnd {
        std::thread::scope(|s| {
            let mut last_request = Instant::now();
            let end = loop {
                if let Some(r) = should_exit(self.config) {
                    break ConnEnd::Exit(r);
                }
                let (id, request) = match self.conn.recv_request(Some(POLL)) {
                    Ok(Some(x)) => x,
                    Ok(None) => {
                        let idle = self
                            .config
                            .idle_timeout
                            .is_some_and(|t| last_request.elapsed() >= t);
                        if idle && self.in_flight().is_empty() {
                            break ConnEnd::Closed(ExitReason::Idle);
                        }
                        continue;
                    }
                    Err(IpcError::RateLimited { .. }) => continue,
                    Err(e) if !e.is_fatal() => continue,
                    Err(e) => {
                        if e != IpcError::Disconnected {
                            diag!("connection closed: {e}");
                        }
                        break ConnEnd::Closed(ExitReason::ClientDisconnected);
                    }
                };
                last_request = Instant::now();
                if let Some(a) = &self.config.activity {
                    a.touch();
                }
                match request {
                    Request::Ping => {
                        if self.conn.send(id, Response::Pong).is_err() {
                            break ConnEnd::Closed(ExitReason::ClientDisconnected);
                        }
                    }
                    Request::Cancel { request_id } => {
                        let target = self.in_flight().get(&request_id).cloned();
                        if let Some(c) = &target {
                            c.cancel();
                        }
                        let ack = Response::CancelAck {
                            was_running: target.is_some(),
                        };
                        if self.conn.send(id, ack).is_err() {
                            break ConnEnd::Closed(ExitReason::ClientDisconnected);
                        }
                    }
                    Request::Shutdown => {
                        let _ = self.conn.send(id, Response::ShuttingDown);
                        break ConnEnd::Shutdown;
                    }
                    other => {
                        if let Err(e) = self.start(s, id, other)
                            && self.conn.send(id, Response::Error(e.to_reply())).is_err()
                        {
                            break ConnEnd::Closed(ExitReason::ClientDisconnected);
                        }
                    }
                }
            };
            for c in self.in_flight().values() {
                c.cancel();
            }
            end
        })
    }

    fn start<'s>(
        &'s self,
        scope: &'s std::thread::Scope<'s, '_>,
        id: u32,
        request: Request,
    ) -> Result<(), HelperError> {
        let is_scan = matches!(request, Request::ScanVolume { .. });
        let cancel = Cancel::new()?;
        {
            let mut map = self.in_flight();
            if map.contains_key(&id) {
                return Err(HelperError::bad_request("request id already in flight"));
            }
            if map.len() >= self.config.max_in_flight {
                return Err(HelperError::new(
                    ErrorCode::Busy,
                    "too many requests in flight",
                ));
            }
            // NOTE: one scan at a time: a scan holds large buffers and the
            // disk's full sequential bandwidth.
            if is_scan && self.scanning.swap(true, Ordering::SeqCst) {
                return Err(HelperError::new(
                    ErrorCode::Busy,
                    "a scan is already running",
                ));
            }
            map.insert(id, cancel.clone());
        }
        let spawned = std::thread::Builder::new()
            .name(format!("strata-helper-req-{id}"))
            .spawn_scoped(scope, move || {
                let ctx = RequestCtx {
                    id,
                    conn: self.conn,
                    cancel,
                    client_pid: self.conn.peer().pid,
                    shared: self.shared,
                };
                if let Err(e) = ops::handle(&ctx, request)
                    && !e.disconnected
                {
                    let _ = ctx.send(Response::Error(e.to_reply()));
                }
                self.in_flight().remove(&id);
                if is_scan {
                    self.scanning.store(false, Ordering::SeqCst);
                }
            });
        if let Err(e) = spawned {
            self.in_flight().remove(&id);
            if is_scan {
                self.scanning.store(false, Ordering::SeqCst);
            }
            return Err(HelperError::internal(format!("cannot start a worker: {e}")));
        }
        Ok(())
    }
}
