//! File-activity tracking requests (`StartActivity`, `StopActivity`,
//! `ClearActivity`, `QueryActivity`, `ActivityEvidence`).
//!
//! One [`strata_etw::Monitor`] runs per helper process, owned by the
//! connection that started it ([`ActivityHub`]). The `StartActivity` worker
//! thread forwards the monitor's messages as events on its request id until
//! the monitor reports that it stopped. It stops the monitor when the
//! request is cancelled (`StopActivity`, `Cancel`, disconnect, `Shutdown`,
//! helper exit) or when the client can no longer be reached.
//!
//! The other requests act only on the requesting connection's own session;
//! another connection sees "nothing runs", so one user never reads another
//! user's activity in service mode.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;
use strata_etw::{
    EtwError, EvidenceConfig, Monitor, MonitorConfig, MonitorMessage, OverheadConfig,
    OverheadReport, StopReason, Window,
};
use strata_ipc::protocol::{
    ActivityRow, ActivityWindow, DirBytes, ErrorCode, EvidenceRow, LastWriteRow, Response,
    WriterRow,
};
use strata_store::{SystemClock, Timestamp};
use strata_win::path::DeviceMap;

use super::RequestCtx;
use crate::cancel::Cancel;
use crate::error::HelperError;

/// How often the forwarding loop checks for cancellation.
const TICK: Duration = Duration::from_millis(250);
/// How long `StopActivity` waits for the session to wind down.
const STOP_WAIT: Duration = Duration::from_secs(15);
/// Largest `limit` honored by queries.
const MAX_ROWS: u32 = 1000;
/// Directories listed per writer in `QueryActivity`.
const TOP_DIRS: usize = 5;
/// Lowest accepted CPU cap, in hundredths of a percent.
const MIN_CAP_CENTI: u32 = 10;
/// Highest accepted CPU cap, in hundredths of a percent.
const MAX_CAP_CENTI: u32 = 10_000;

/// The process-wide tracking slot.
#[derive(Debug, Default)]
pub struct ActivityHub {
    slot: Mutex<Option<Session>>,
    ended: Condvar,
}

#[derive(Debug)]
struct Session {
    connection: u64,
    cancel: Cancel,
    /// `None` while starting and after the forwarder took it to stop it.
    monitor: Option<Monitor>,
}

impl ActivityHub {
    fn lock(&self) -> MutexGuard<'_, Option<Session>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `connection` owns the running session.
    #[must_use]
    pub fn is_running(&self, connection: u64) -> bool {
        self.lock()
            .as_ref()
            .is_some_and(|s| s.connection == connection)
    }

    fn claim(&self, connection: u64, cancel: Cancel) -> Result<Claim<'_>, HelperError> {
        let mut slot = self.lock();
        if slot.is_some() {
            return Err(HelperError::new(
                ErrorCode::Busy,
                "activity tracking is already running",
            ));
        }
        *slot = Some(Session {
            connection,
            cancel,
            monitor: None,
        });
        Ok(Claim { hub: self })
    }

    /// Runs `f` on `connection`'s monitor; `None` when it has none.
    fn with_monitor<T>(&self, connection: u64, f: impl FnOnce(&Monitor) -> T) -> Option<T> {
        let slot = self.lock();
        slot.as_ref()
            .filter(|s| s.connection == connection)
            .and_then(|s| s.monitor.as_ref())
            .map(f)
    }

    /// Cancels `connection`'s session and waits up to `wait` for it to end.
    /// Returns whether it still runs.
    fn stop(&self, connection: u64, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        let mut slot = self.lock();
        loop {
            match slot.as_ref() {
                Some(s) if s.connection == connection => s.cancel.cancel(),
                _ => return false,
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            slot = self
                .ended
                .wait_timeout(slot, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// Holds the slot for one `StartActivity`; releasing it stops the monitor.
struct Claim<'a> {
    hub: &'a ActivityHub,
}

impl Claim<'_> {
    fn install(&self, monitor: Monitor) {
        if let Some(s) = self.hub.lock().as_mut() {
            s.monitor = Some(monitor);
        }
    }

    /// Takes the monitor out so it can be stopped without holding the lock.
    fn take(&self) -> Option<Monitor> {
        self.hub.lock().as_mut().and_then(|s| s.monitor.take())
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let session = self.hub.lock().take();
        // NOTE: dropping a monitor stops its ETW session and joins its
        // threads; do it outside the lock so queries never wait on that.
        drop(session);
        self.hub.ended.notify_all();
    }
}

/// Handles `StartActivity`: runs tracking until cancelled, streaming events.
///
/// # Errors
///
/// [`ErrorCode::Busy`] when a session already runs, [`ErrorCode::AccessDenied`]
/// when the helper is not elevated, or the start failure.
pub fn start_activity(ctx: &RequestCtx<'_>, cpu_cap_centi_percent: u32) -> Result<(), HelperError> {
    let claim = ctx
        .shared
        .activity
        .claim(ctx.connection, ctx.cancel.clone())?;
    if !strata_win::process::is_elevated().unwrap_or(false) {
        return Err(not_elevated());
    }
    let devices = DeviceMap::current().map_err(HelperError::from)?;
    let (monitor, rx) = Monitor::start(
        monitor_config(cpu_cap_centi_percent),
        devices,
        Arc::new(SystemClock),
    )
    .map_err(etw_error)?;
    claim.install(monitor);
    let mut stopping = false;
    loop {
        if !stopping && ctx.cancel.is_cancelled() {
            stopping = true;
            // NOTE: `stop` pushes the final batch and `Stopped` onto `rx`,
            // which the loop then forwards like any other message.
            if let Some(m) = claim.take() {
                let _ = m.stop();
            }
        }
        let message = match rx.recv_timeout(TICK) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return ctx.send(Response::ActivityStopped {
                    requested: stopping,
                    status: 0,
                    events_lost: 0,
                });
            }
        };
        match message {
            MonitorMessage::Batch(batch) => ctx.send(Response::ActivityBatch {
                rows: batch.samples.into_iter().map(activity_row).collect(),
                last_writes: batch.last_writes.into_iter().map(last_write_row).collect(),
            })?,
            MonitorMessage::Overhead(report) => ctx.send(health(report))?,
            MonitorMessage::Stopped {
                reason, session, ..
            } => {
                let (requested, status) = match reason {
                    StopReason::Requested => (true, 0),
                    StopReason::SessionEnded(status) => (false, status),
                };
                return ctx.send(Response::ActivityStopped {
                    requested,
                    status,
                    events_lost: u64::from(session.events_lost),
                });
            }
        }
    }
}

/// Handles `StopActivity`.
///
/// # Errors
///
/// The reply could not be sent.
pub fn stop_activity(ctx: &RequestCtx<'_>) -> Result<(), HelperError> {
    let running = ctx.shared.activity.stop(ctx.connection, STOP_WAIT);
    ctx.send(Response::ActivityState { running })
}

/// Handles `ClearActivity`.
///
/// # Errors
///
/// The reply could not be sent.
pub fn clear_activity(ctx: &RequestCtx<'_>) -> Result<(), HelperError> {
    let hub = &ctx.shared.activity;
    let _ = hub.with_monitor(ctx.connection, Monitor::clear);
    ctx.send(Response::ActivityState {
        running: hub.is_running(ctx.connection),
    })
}

/// Handles `QueryActivity`.
///
/// # Errors
///
/// The reply could not be sent.
pub fn query_activity(
    ctx: &RequestCtx<'_>,
    window: ActivityWindow,
    limit: u32,
) -> Result<(), HelperError> {
    let w = match window {
        ActivityWindow::Now => Window::Now,
        ActivityWindow::LastHour => Window::LastHour,
        ActivityWindow::Since { unix_secs } => Window::Since(Timestamp(unix_secs)),
    };
    let limit = limit.min(MAX_ROWS) as usize;
    let writers = ctx
        .shared
        .activity
        .with_monitor(ctx.connection, |m| {
            let writers = m.top_writers(w, limit);
            let dirs = if writers.is_empty() {
                Vec::new()
            } else {
                m.dir_totals(w, usize::MAX)
            };
            (writers, dirs)
        })
        .map(|(writers, dirs)| writer_rows(writers, dirs))
        .unwrap_or_default();
    ctx.send(Response::ActivityTop { writers })
}

/// Handles `ActivityEvidence`.
///
/// # Errors
///
/// The reply could not be sent.
pub fn activity_evidence(
    ctx: &RequestCtx<'_>,
    since_unix: i64,
    limit: u32,
) -> Result<(), HelperError> {
    let mut evidence: Vec<EvidenceRow> = ctx
        .shared
        .activity
        .with_monitor(ctx.connection, |m| {
            m.evidence(Timestamp(since_unix), &EvidenceConfig::default())
        })
        .unwrap_or_default()
        .into_iter()
        .map(|e| EvidenceRow {
            prefix: e.prefix,
            app: e.app,
            image: e.image,
            weight_milli: milli(e.weight),
            share_milli: milli(e.share),
        })
        .collect();
    evidence.sort_by(|a, b| {
        b.weight_milli
            .cmp(&a.weight_milli)
            .then_with(|| a.prefix.cmp(&b.prefix))
    });
    evidence.truncate(limit.min(MAX_ROWS) as usize);
    ctx.send(Response::ActivityEvidence { evidence })
}

fn not_elevated() -> HelperError {
    HelperError::new(
        ErrorCode::AccessDenied,
        "activity tracking needs an elevated helper",
    )
}

fn etw_error(e: EtwError) -> HelperError {
    match e {
        EtwError::NotElevated => not_elevated(),
        EtwError::Win32 { .. } => HelperError::new(ErrorCode::Io, e.to_string()),
        EtwError::Thread(_) => HelperError::internal(e.to_string()),
    }
}

fn monitor_config(cpu_cap_centi_percent: u32) -> MonitorConfig {
    let mut cfg = MonitorConfig::default();
    if cpu_cap_centi_percent != 0 {
        let centi = cpu_cap_centi_percent.clamp(MIN_CAP_CENTI, MAX_CAP_CENTI);
        cfg.overhead = OverheadConfig {
            cap_percent: centi as f32 / 100.0,
            ..cfg.overhead
        };
    }
    cfg
}

fn activity_row(s: strata_store::ActivitySample) -> ActivityRow {
    ActivityRow {
        hour: s.at.0,
        image: s.image,
        dir_hash: s.dir_hash,
        bytes_written: s.bytes_written,
        files_created: s.files_created,
        files_deleted: s.files_deleted,
    }
}

fn last_write_row(w: strata_store::LastWrite) -> LastWriteRow {
    LastWriteRow {
        path_hash: w.path_hash,
        image: w.image,
        pid: w.pid,
        at: w.at.0,
    }
}

fn health(r: OverheadReport) -> Response {
    Response::ActivityHealth {
        cpu_centi_percent: centi(r.cpu_percent),
        sample_rate: r.sample_rate,
        suggest_disable: r.suggest_disable,
    }
}

/// `percent` × 100, rounded; negative and NaN become 0.
fn centi(percent: f32) -> u32 {
    let v = (f64::from(percent) * 100.0).round();
    if v.is_nan() || v <= 0.0 {
        0
    } else if v >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        v as u32
    }
}

/// A `0..=1` fraction in thousandths, rounded and clamped.
fn milli(fraction: f32) -> u32 {
    let v = (f64::from(fraction) * 1000.0).round();
    if v.is_nan() || v <= 0.0 {
        0
    } else {
        (v as u32).min(1000)
    }
}

fn writer_rows(
    writers: Vec<strata_etw::WriterSummary>,
    dirs: Vec<strata_etw::DirTotal>,
) -> Vec<WriterRow> {
    let mut by_image: HashMap<String, Vec<DirBytes>> = HashMap::new();
    for d in dirs {
        if d.counts.bytes_written == 0 {
            continue;
        }
        by_image.entry(d.image).or_default().push(DirBytes {
            dir: d.dir,
            bytes_written: d.counts.bytes_written,
        });
    }
    writers
        .into_iter()
        .map(|w| {
            let mut top_dirs = by_image.remove(&w.image).unwrap_or_default();
            top_dirs.sort_by(|a, b| {
                b.bytes_written
                    .cmp(&a.bytes_written)
                    .then_with(|| a.dir.cmp(&b.dir))
            });
            top_dirs.truncate(TOP_DIRS);
            WriterRow {
                image: w.image,
                bytes_written: w.counts.bytes_written,
                files_created: w.counts.files_created,
                files_deleted: w.counts.files_deleted,
                dirs: w.dirs,
                top_dirs,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_etw::{Counts, DirTotal, WriterSummary};

    #[test]
    fn fractions_round_and_clamp() {
        assert_eq!(milli(1.0), 1000);
        assert_eq!(milli(1.2), 1000);
        assert_eq!(milli(0.3334), 333);
        assert_eq!(milli(0.0005), 1);
        assert_eq!(milli(-0.5), 0);
        assert_eq!(milli(f32::NAN), 0);
        assert_eq!(centi(2.0), 200);
        assert_eq!(centi(0.374), 37);
        assert_eq!(centi(-1.0), 0);
        assert_eq!(centi(f32::INFINITY), u32::MAX);
    }

    #[test]
    fn cpu_cap_is_clamped() {
        let default = MonitorConfig::default().overhead.cap_percent;
        assert_eq!(monitor_config(0).overhead.cap_percent, default);
        assert_eq!(monitor_config(150).overhead.cap_percent, 1.5);
        assert_eq!(monitor_config(1).overhead.cap_percent, 0.1);
        assert_eq!(monitor_config(u32::MAX).overhead.cap_percent, 100.0);
    }

    #[test]
    fn writers_get_their_largest_directories() {
        let writers = vec![
            WriterSummary {
                image: "a.exe".into(),
                counts: Counts::write(100),
                dirs: 7,
            },
            WriterSummary {
                image: "b.exe".into(),
                counts: Counts::created(),
                dirs: 1,
            },
        ];
        let mut dirs: Vec<DirTotal> = (0..7u64)
            .map(|i| DirTotal {
                image: "a.exe".into(),
                dir: format!(r"C:\d\{i}"),
                counts: Counts::write(i),
            })
            .collect();
        dirs.push(DirTotal {
            image: "b.exe".into(),
            dir: r"C:\d\b".into(),
            counts: Counts::created(),
        });
        let rows = writer_rows(writers, dirs);
        assert_eq!(rows.len(), 2);
        let a: Vec<u64> = rows[0].top_dirs.iter().map(|d| d.bytes_written).collect();
        assert_eq!(a, [6, 5, 4, 3, 2]);
        assert_eq!(rows[0].dirs, 7);
        assert_eq!(rows[0].bytes_written, 100);
        assert!(rows[1].top_dirs.is_empty(), "no bytes written");
        assert_eq!(rows[1].files_created, 1);
    }

    #[test]
    fn hub_is_exclusive_and_per_connection() {
        let hub = ActivityHub::default();
        let cancel = Cancel::new().unwrap();
        let claim = hub.claim(1, cancel.clone()).unwrap();
        assert!(hub.is_running(1));
        assert!(!hub.is_running(2));
        assert_eq!(
            hub.claim(2, Cancel::new().unwrap()).err().map(|e| e.code),
            Some(ErrorCode::Busy)
        );
        assert!(!hub.stop(2, Duration::ZERO), "not the owner");
        assert!(!cancel.is_cancelled());
        std::thread::scope(|s| {
            let stopper = s.spawn(|| hub.stop(1, Duration::from_secs(10)));
            while !cancel.is_cancelled() {
                std::thread::yield_now();
            }
            drop(claim);
            assert!(!stopper.join().unwrap());
        });
        assert!(!hub.is_running(1));
        drop(hub.claim(2, Cancel::new().unwrap()).unwrap());
    }
}
