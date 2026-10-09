//! The app side of the elevated helper, over
//! [`strata_helper::client::HelperClient`].
//!
//! - [`HelperManager`]: finds `strata-helper.exe` next to the app
//!   executable, connects through the installed service when settings ask
//!   for service mode (falling back to an on-demand UAC launch when the
//!   service is not installed), and tracks the connection state (none,
//!   connected, declined, disconnected) for the UI's banners.
//! - [`start_watchdog`]: notices a crash or disconnect within half a second,
//!   emits `helper://changed`, uninstalls the privileged-delete backend and
//!   tells the live updater; the last index stays usable and the UI offers to
//!   reconnect (`helper_elevate`).
//! - [`scan_volume`]: runs the MFT scan stream into an [`Ingest`],
//!   forwarding the user's cancel.
//! - [`HelperBackend`]: the cleanup service's [`PrivilegedBackend`]: by-id
//!   deletes and delete on reboot through the helper.
//!
//! In debug builds the helper is found next to the dev executable in the
//! Cargo target directory (`pnpm tauri dev` builds it first) and is accepted
//! unsigned; release builds require the same signer as the app.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use strata_clean::CleanError;
use strata_clean::permanent::DeleteStats;
use strata_clean::privileged::PrivilegedDeleteRequest;
use strata_helper::client::{ClientConfig, ClientError, HelperClient, ScanEvent};
use strata_ipc::protocol::{DeleteRequest, ErrorCode, RebootDeleteRequest, ScanOptions, ScanStats};
use tauri::{AppHandle, Emitter, Runtime};

use crate::features::cleanup::{PrivilegedBackend, set_privileged_backend};
use crate::scan::{Ingest, ScanError, ScanObserver};
use crate::state::AppState;

pub use strata_helper::HELPER_EXE;

/// Event carrying the [`HelperStatus`] after a connect or disconnect.
pub const HELPER_CHANGED: &str = "helper://changed";

/// Why the helper cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperError {
    /// No helper binary in this build.
    Unavailable(String),
    /// The user declined the UAC prompt.
    Declined,
    /// The helper is not connected or went away.
    Disconnected,
    /// Anything else (launch failure, handshake rejected, protocol error).
    Failed(String),
}

impl std::fmt::Display for HelperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(m) | Self::Failed(m) => f.write_str(m),
            Self::Declined => f.write_str("elevation was declined; using the standard scan"),
            Self::Disconnected => f.write_str("the fast-scan helper disconnected"),
        }
    }
}

impl From<ClientError> for HelperError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Declined => Self::Declined,
            ClientError::Disconnected => Self::Disconnected,
            other => Self::Failed(other.to_string()),
        }
    }
}

/// Connection state reported to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperState {
    /// Never started.
    None,
    /// Connected and usable.
    Connected,
    /// The user declined UAC.
    Declined,
    /// It crashed or disconnected; the last index stays usable.
    Disconnected,
}

/// `HelperStatus` in `ui/src/lib/volumes.ts`, plus `state`, `available` and
/// `message` for the disconnected/declined banners.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HelperStatus {
    /// An elevated helper is connected.
    pub elevated: bool,
    /// `none` | `on_demand` | `service`.
    pub mode: &'static str,
    /// Connection state.
    pub state: HelperState,
    /// The helper binary exists in this build.
    pub available: bool,
    /// Last problem, for the banner.
    pub message: Option<String>,
}

#[derive(Debug)]
struct Conn {
    state: HelperState,
    client: Option<Arc<HelperClient>>,
    service: bool,
    message: Option<String>,
}

/// Owns the helper connection.
#[derive(Debug)]
pub struct HelperManager {
    exe: Option<PathBuf>,
    conn: Mutex<Conn>,
    /// Serializes connection attempts (one UAC prompt at a time).
    connecting: Mutex<()>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl HelperManager {
    /// Looks for the helper next to the current executable.
    #[must_use]
    pub fn new() -> Self {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join(HELPER_EXE)))
            .filter(|p| p.is_file());
        Self::with_exe(exe)
    }

    /// A manager for an explicit helper binary (tests).
    #[must_use]
    pub fn with_exe(exe: Option<PathBuf>) -> Self {
        Self {
            exe,
            conn: Mutex::new(Conn {
                state: HelperState::None,
                client: None,
                service: false,
                message: None,
            }),
            connecting: Mutex::new(()),
        }
    }

    /// Whether this build ships a helper binary.
    #[must_use]
    pub fn available(&self) -> bool {
        self.exe.is_some()
    }

    /// The live connection, if any.
    #[must_use]
    pub fn client(&self) -> Option<Arc<HelperClient>> {
        lock(&self.conn).client.clone().filter(|c| c.is_connected())
    }

    /// Current status.
    #[must_use]
    pub fn status(&self) -> HelperStatus {
        let g = lock(&self.conn);
        let conn = g.client.as_ref().filter(|c| c.is_connected());
        HelperStatus {
            elevated: conn.is_some_and(|c| c.welcome().elevated),
            mode: match (conn.is_some(), g.service) {
                (false, _) => "none",
                (true, false) => "on_demand",
                (true, true) => "service",
            },
            state: if conn.is_some() {
                HelperState::Connected
            } else if g.state == HelperState::Connected {
                HelperState::Disconnected
            } else {
                g.state
            },
            available: self.exe.is_some(),
            message: g.message.clone(),
        }
    }

    /// Connects: through the service when `prefer_service` is set and it is
    /// installed, otherwise, when `allow_prompt` is set, by launching the
    /// helper through UAC. Blocks while
    /// the consent prompt is shown. Returns the existing connection when one
    /// is up.
    ///
    /// # Errors
    ///
    /// [`HelperError::Unavailable`] without a helper binary,
    /// [`HelperError::Declined`] when the user says no, or a launch/connect
    /// failure.
    pub fn connect(
        &self,
        prefer_service: bool,
        allow_prompt: bool,
    ) -> Result<Arc<HelperClient>, HelperError> {
        let _one = lock(&self.connecting);
        if let Some(c) = self.client() {
            return Ok(c);
        }
        let Some(exe) = self.exe.clone() else {
            return Err(HelperError::Unavailable(
                "fast scan needs the Strata helper, which is not part of this build".into(),
            ));
        };
        let mut service = false;
        let result = if prefer_service {
            match HelperClient::connect(ClientConfig::service(&exe)) {
                Ok(c) => {
                    service = true;
                    Ok(c)
                }
                Err(ClientError::ServiceNotInstalled) if allow_prompt => {
                    HelperClient::connect(ClientConfig::on_demand(&exe))
                }
                Err(e) => Err(e),
            }
        } else if allow_prompt {
            HelperClient::connect(ClientConfig::on_demand(&exe))
        } else {
            Err(ClientError::ServiceNotInstalled)
        };
        let mut g = lock(&self.conn);
        match result {
            Ok(c) => {
                let c = Arc::new(c);
                *g = Conn {
                    state: HelperState::Connected,
                    client: Some(c.clone()),
                    service,
                    message: None,
                };
                Ok(c)
            }
            Err(e) => {
                let e = HelperError::from(e);
                if e == HelperError::Declined {
                    g.state = HelperState::Declined;
                }
                g.message = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// Records a disconnect; returns whether the state changed.
    pub fn mark_disconnected(&self) -> bool {
        let mut g = lock(&self.conn);
        if g.client.is_none() {
            return false;
        }
        g.client = None;
        g.state = HelperState::Disconnected;
        g.message = Some(HelperError::Disconnected.to_string());
        true
    }
}

impl Default for HelperManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Connects (see [`HelperManager::connect`]) using the helper mode from
/// settings, then installs the privileged-delete backend, switches
/// journal-capable volumes to live updates and resumes activity tracking when
/// it is enabled. Emits `helper://changed`.
///
/// # Errors
///
/// As [`HelperManager::connect`].
pub fn elevate<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
) -> Result<Arc<HelperClient>, HelperError> {
    connect_and_announce(app, state, true)
}

/// Connects through the installed service only, never showing a UAC
/// prompt (startup in service mode). Errors when the service is not
/// installed or not usable.
///
/// # Errors
///
/// As [`HelperManager::connect`].
pub fn connect_quietly<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
) -> Result<Arc<HelperClient>, HelperError> {
    connect_and_announce(app, state, false)
}

fn connect_and_announce<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
    allow_prompt: bool,
) -> Result<Arc<HelperClient>, HelperError> {
    let prefer_service = state
        .store()
        .and_then(|s| s.load_settings().ok())
        .is_some_and(|s| s.helper.mode == strata_store::HelperMode::Service);
    let was_connected = state.helper.client().is_some();
    let r = state.helper.connect(prefer_service, allow_prompt);
    if let Ok(c) = &r
        && !was_connected
    {
        set_privileged_backend(app, Some(Arc::new(HelperBackend(c.clone()))));
        crate::live::on_helper_connected(app, state);
        crate::activity::on_helper_connected(state);
    }
    let _ = app.emit(HELPER_CHANGED, state.helper.status());
    r
}

/// Watches the connection: a crash or disconnect flips the state, removes
/// the privileged backend and emits `helper://changed`.
pub fn start_watchdog<R: Runtime>(app: &AppHandle<R>, state: &Arc<AppState>) {
    let app = app.clone();
    let state = state.clone();
    let _ = std::thread::Builder::new()
        .name("strata-helper-watch".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let lost = lock(&state.helper.conn)
                    .client
                    .as_ref()
                    .is_some_and(|c| !c.is_connected());
                if lost && state.helper.mark_disconnected() {
                    set_privileged_backend(&app, None);
                    let _ = app.emit(HELPER_CHANGED, state.helper.status());
                }
            }
        });
}

/// Scans `volume` (GUID path) with the MFT scanner, streaming into
/// `ingest`. Returns the helper's statistics (`cancelled` set when `cancel`
/// fired).
///
/// # Errors
///
/// [`ScanError::Scanner`] when the helper refuses or disconnects; the caller
/// keeps the previous index.
pub fn scan_volume(
    client: &HelperClient,
    volume: &str,
    ingest: &mut Ingest,
    cancel: &AtomicBool,
    observer: &dyn ScanObserver,
) -> Result<ScanStats, ScanError> {
    let fail = |e: ClientError| ScanError::Scanner(format!("helper: {}", HelperError::from(e)));
    let mut stream = client
        .scan_volume(volume, ScanOptions::default())
        .map_err(fail)?;
    let mut cancel_sent = false;
    let mut last = Instant::now();
    let started = Instant::now();
    // The helper knows how much of the MFT it has read, a better measure of
    // progress than bytes accounted so far.
    let mut mft: Option<(u64, u64)> = None;
    loop {
        if cancel.load(Ordering::Acquire) && !cancel_sent {
            cancel_sent = true;
            let _ = stream.cancel();
        }
        match stream.next_timeout(Duration::from_millis(100)) {
            Some(Ok(ScanEvent::Batch(records))) => ingest.push(records, observer)?,
            Some(Ok(ScanEvent::Progress(p))) => {
                mft = p.bytes_total.filter(|&t| t > 0).map(|t| (p.bytes_read, t));
            }
            Some(Ok(ScanEvent::Audit(_))) => {}
            Some(Ok(ScanEvent::Done(stats))) => return Ok(stats),
            Some(Err(e)) => return Err(fail(e)),
            None if stream.is_done() => {
                return Err(ScanError::Scanner(HelperError::Disconnected.to_string()));
            }
            None => {}
        }
        if last.elapsed() >= Duration::from_millis(250) {
            last = Instant::now();
            let mut p = ingest.progress();
            if let Some((read, total)) = mft {
                let f = (read as f64 / total as f64).min(0.99);
                let elapsed = started.elapsed().as_secs_f64();
                p.fraction = Some(f);
                p.eta_secs = (f > 0.02 && elapsed > 1.0).then(|| elapsed * (1.0 - f) / f);
            }
            observer.progress(&p);
        }
    }
}

// -----------------------------------------------------------------------------
// Privileged backend
// -----------------------------------------------------------------------------

/// The cleanup service's elevated route over a connected helper.
#[derive(Debug, Clone)]
pub struct HelperBackend(pub Arc<HelperClient>);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Maps a helper failure for `path` onto the cleaner's error type.
#[must_use]
pub fn clean_error(path: &str, e: &ClientError) -> CleanError {
    match e.code() {
        Some(ErrorCode::NotFound) => CleanError::NotFound { path: path.into() },
        Some(ErrorCode::AccessDenied) => CleanError::AccessDenied { path: path.into() },
        _ => CleanError::Os {
            path: path.into(),
            code: 0,
            message: format!("the helper refused or failed: {e}"),
        },
    }
}

impl PrivilegedBackend for HelperBackend {
    fn delete_by_id(&self, request: &PrivilegedDeleteRequest) -> Result<DeleteStats, CleanError> {
        let req = DeleteRequest {
            volume: request.volume.clone(),
            file_ref: request.file_ref,
            expected_path: wide(&request.expected_path),
            expected_size: request.expected_size,
            expected_mtime: request.expected_mtime,
            is_dir: request.is_dir,
        };
        self.0
            .privileged_delete(req)
            .map(|a| DeleteStats {
                files: a.value.files,
                dirs: a.value.dirs,
                links: a.value.links,
                bytes: a.value.bytes,
            })
            .map_err(|e| clean_error(&request.expected_path, &e))
    }

    fn delete_on_reboot(&self, request: &PrivilegedDeleteRequest) -> Result<(), CleanError> {
        self.0
            .delete_on_reboot(RebootDeleteRequest {
                file_ref: request.file_ref,
                expected_path: wide(&request.expected_path),
                expected_size: request.expected_size,
                expected_mtime: request.expected_mtime,
            })
            .map(|_| ())
            .map_err(|e| clean_error(&request.expected_path, &e))
    }
}
