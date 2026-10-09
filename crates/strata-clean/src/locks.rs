//! "Why can't I delete this?": Restart Manager lock detection
//! and polite close.
//!
//! Closing is always polite: `WM_CLOSE` to the app's top-level windows, or a
//! Restart Manager shutdown request without `RmForceShutdown`. Both need a
//! [`Consent`]. Nothing here ever terminates a process.

use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::consent::{CloseApp, Consent, ConsentError};
use crate::win::process;
use crate::win::rm::{RmProcess, RmSession};
use crate::win::wide_os;

/// Default bound on files registered for a folder.
pub const DEFAULT_MAX_FILES: usize = 1000;

/// Restart Manager registration batch size.
const RM_BATCH: usize = 256;

/// Restart Manager's application type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppKind {
    /// Unknown.
    Unknown,
    /// App with a main window.
    MainWindow,
    /// App with other windows only.
    OtherWindow,
    /// Windows service.
    Service,
    /// Explorer.
    Explorer,
    /// Console app.
    Console,
    /// Critical system process; cannot be closed.
    Critical,
}

impl AppKind {
    fn from_raw(t: i32) -> Self {
        match t {
            1 => Self::MainWindow,
            2 => Self::OtherWindow,
            3 => Self::Service,
            4 => Self::Explorer,
            5 => Self::Console,
            1000 => Self::Critical,
            _ => Self::Unknown,
        }
    }
}

/// A process holding a file open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockHolder {
    /// Process id.
    pub pid: u32,
    /// Process start time (FILETIME), disambiguating reused PIDs.
    pub start_time: u64,
    /// Friendly name from Restart Manager (e.g. "Discord").
    pub app_name: String,
    /// Executable path, when we may query it.
    pub exe_path: Option<String>,
    /// Service short name, for services.
    pub service: Option<String>,
    /// Application type.
    pub kind: AppKind,
    /// Whether the app registered for restart.
    pub restartable: bool,
}

impl LockHolder {
    fn from_rm(p: RmProcess) -> Self {
        let exe_path = process::image_path(p.pid);
        let app_name = if p.app_name.is_empty() {
            exe_path
                .as_deref()
                .and_then(|e| Path::new(e).file_name())
                .map_or_else(
                    || format!("PID {}", p.pid),
                    |n| n.to_string_lossy().into_owned(),
                )
        } else {
            p.app_name
        };
        Self {
            pid: p.pid,
            start_time: p.start_time,
            app_name,
            exe_path,
            service: (!p.service.is_empty()).then_some(p.service),
            kind: AppKind::from_raw(p.app_type),
            restartable: p.restartable,
        }
    }

    /// The consent prompt for closing this app.
    #[must_use]
    pub fn close_request(&self) -> CloseApp {
        CloseApp {
            pid: self.pid,
            start_time: self.start_time,
            app_name: self.app_name.clone(),
        }
    }
}

/// Files Restart Manager should check for `path`: the file itself, or up to
/// `max_files` files inside a folder. Never descends into reparse points
/// and never opens file content.
pub(crate) fn lockable_files(path: &Path, max_files: usize) -> Vec<PathBuf> {
    use strata_core::win32::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Vec::new();
    };
    let attrs = meta.file_attributes();
    if attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return vec![path.to_path_buf()];
    }
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(m) = entry.metadata() else { continue };
            let a = m.file_attributes();
            if a & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                continue;
            }
            if a & FILE_ATTRIBUTE_DIRECTORY != 0 {
                stack.push(entry.path());
            } else {
                out.push(entry.path());
                if out.len() >= max_files {
                    return out;
                }
            }
        }
    }
    out
}

/// Processes holding `path` (or files inside it, up to `max_files`) open.
///
/// # Errors
///
/// Fails when Restart Manager cannot start a session.
///
/// # Example
///
/// ```no_run
/// let holders = strata_clean::locks::who_locks(
///     std::path::Path::new(r"C:\Users\me\AppData\Local\Discord\Cache"),
///     strata_clean::locks::DEFAULT_MAX_FILES,
/// )?;
/// for h in holders {
///     println!("In use by: {} (PID {})", h.app_name, h.pid);
/// }
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn who_locks(path: &Path, max_files: usize) -> io::Result<Vec<LockHolder>> {
    who_locks_files(&lockable_files(path, max_files))
}

pub(crate) fn who_locks_files(files: &[PathBuf]) -> io::Result<Vec<LockHolder>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let session = RmSession::start()?;
    for chunk in files.chunks(RM_BATCH) {
        let wides: Vec<Vec<u16>> = chunk.iter().map(|f| wide_os(f.as_os_str())).collect();
        session.register_files(&wides)?;
    }
    let mut holders: Vec<LockHolder> = session
        .list()?
        .into_iter()
        .map(LockHolder::from_rm)
        .collect();
    holders.sort_by_key(|h| h.pid);
    holders.dedup_by_key(|h| h.pid);
    Ok(holders)
}

/// What a polite close did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CloseOutcome {
    /// `WM_CLOSE` was posted to this many windows; the app decides.
    AskedWindows {
        /// Window count.
        windows: usize,
    },
    /// Restart Manager asked the app (or service) to shut down and it did.
    ShutDown,
}

/// Why a polite close did not happen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CloseError {
    /// The consent was stale or expired.
    #[error(transparent)]
    Consent(ConsentError),
    /// The PID now belongs to a different process.
    #[error("that program already closed or its process id was reused")]
    ProcessChanged,
    /// A critical system process; Windows would not survive closing it.
    #[error("this is a critical system process and cannot be closed")]
    Critical,
    /// The app declined or Restart Manager failed.
    #[error("the program did not close: {0}")]
    Declined(String),
}

/// Asks `holder` to close, politely. Never kills.
///
/// GUI apps get `WM_CLOSE` on their top-level windows; services and
/// window-less apps get a Restart Manager shutdown request without force.
///
/// # Errors
///
/// See [`CloseError`].
pub fn close_politely(
    holder: &LockHolder,
    consent: Consent<CloseApp>,
) -> Result<CloseOutcome, CloseError> {
    let action = consent.redeem().map_err(CloseError::Consent)?;
    if action.pid != holder.pid || action.start_time != holder.start_time {
        return Err(CloseError::Consent(ConsentError::Stale));
    }
    if holder.kind == AppKind::Critical {
        return Err(CloseError::Critical);
    }
    if holder.kind != AppKind::Service {
        let windows = process::post_close_to_windows(holder.pid);
        if windows > 0 {
            return Ok(CloseOutcome::AskedWindows { windows });
        }
    }
    let session = RmSession::start().map_err(|e| CloseError::Declined(e.to_string()))?;
    // Registration by (pid, start time) fails if the PID was reused.
    session
        .register_process(holder.pid, holder.start_time)
        .map_err(|_| CloseError::ProcessChanged)?;
    session
        .shutdown_politely()
        .map_err(|e| CloseError::Declined(e.to_string()))?;
    Ok(CloseOutcome::ShutDown)
}
