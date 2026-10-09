//! The helper-facing API: start tracking, receive results, stop.
//!
//! ```text
//! Monitor::start ─┬─ SessionGuard (fixed-name real-time session)
//!                 ├─ consumer thread: ProcessTrace → callback → Tracker
//!                 └─ timer thread:   every overhead_interval → guard sample
//!                                    every flush_interval    → Tracker::flush
//!                                      → MonitorMessage::Batch on the channel
//! ```
//!
//! The helper forwards [`MonitorMessage`]s to the app, which persists
//! batches with `Store::record_activity` / `Store::set_last_writers`.
//! [`Monitor::stop`] (or drop) stops the session first, so `ProcessTrace`
//! returns, joins both threads and sends a final batch and
//! [`MonitorMessage::Stopped`]. If the session is stopped from outside (for
//! example `logman stop`), the consumer thread notices and reports
//! [`StopReason::SessionEnded`].

use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};
use strata_store::{Clock, Timestamp};
use strata_win::path::DeviceMap;

use crate::aggregate::{ActivityBatch, DirTotal, Window, WriterSummary};
use crate::decode::Decoder;
use crate::error::EtwError;
use crate::evidence::{Evidence, EvidenceConfig};
use crate::ffi::{CallbackState, RealtimeConsumer, Win32Control, thread_cpu_time};
use crate::overhead::{OverheadConfig, OverheadGuard, OverheadReport};
use crate::processes::LiveProcesses;
use crate::session::{SessionConfig, SessionGuard, SessionStats};
use crate::tracker::{Tracker, TrackerStats};

/// Monitor settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorConfig {
    /// Session settings (name, buffers, providers).
    pub session: SessionConfig,
    /// How often rollups are flushed to the channel (default 10 s).
    pub flush_interval: Duration,
    /// How often consumer CPU is sampled (default 5 s).
    pub overhead_interval: Duration,
    /// Overhead guard settings (cap from the `activity.cpu_cap` setting).
    pub overhead: OverheadConfig,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            session: SessionConfig::default(),
            flush_interval: Duration::from_secs(10),
            overhead_interval: Duration::from_secs(5),
            overhead: OverheadConfig::default(),
        }
    }
}

/// Why tracking ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// [`Monitor::stop`] or drop.
    Requested,
    /// The session ended without a request (stopped externally, or
    /// `ProcessTrace` failed with the given status).
    SessionEnded(u32),
}

/// What the monitor sends to the helper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MonitorMessage {
    /// Rollups and last writers to persist.
    Batch(ActivityBatch),
    /// An overhead sample (sent every `overhead_interval`).
    Overhead(OverheadReport),
    /// Tracking ended; no further messages follow.
    Stopped {
        /// Why.
        reason: StopReason,
        /// Pipeline counters at the end.
        stats: TrackerStats,
        /// Kernel-side losses reported by the session.
        session: SessionStats,
        /// Events whose processing panicked (contained).
        panics: u64,
    },
}

/// A running activity monitor. Must live in the elevated helper.
#[derive(Debug)]
pub struct Monitor {
    tracker: Arc<Mutex<Tracker>>,
    session: Option<SessionGuard<Win32Control>>,
    consumer: Option<JoinHandle<u32>>,
    timer: Option<JoinHandle<()>>,
    timer_stop: Option<Sender<()>>,
    tx: Sender<MonitorMessage>,
    stopping: Arc<AtomicBool>,
    state: Arc<CallbackState>,
    manifest_errors: Vec<String>,
}

impl Monitor {
    /// Starts tracking. Stops a stale session left by a crash first.
    ///
    /// `devices` normalizes NT paths (pass [`DeviceMap::current`]; rebuild
    /// it with [`Monitor::set_devices`] on volume changes). `clock` drives
    /// the view windows and pruning.
    ///
    /// # Errors
    ///
    /// [`EtwError::NotElevated`] when the process is not elevated;
    /// [`EtwError::Win32`] when a trace call fails (no session is left
    /// running); [`EtwError::Thread`] when a thread cannot be spawned.
    pub fn start(
        cfg: MonitorConfig,
        devices: DeviceMap,
        clock: Arc<dyn Clock>,
    ) -> Result<(Self, Receiver<MonitorMessage>), EtwError> {
        if !strata_win::process::is_elevated().unwrap_or(false) {
            return Err(EtwError::NotElevated);
        }
        let (decoder, manifest_errors) = Decoder::with_installed_manifests();
        let tracker = Arc::new(Mutex::new(Tracker::new(
            decoder,
            devices,
            Box::new(LiveProcesses),
            clock,
        )));
        let session = SessionGuard::start(Win32Control, &cfg.session)?;
        let state = Arc::new(CallbackState {
            tracker: tracker.clone(),
            panics: AtomicU64::new(0),
            poisoned: AtomicBool::new(false),
        });
        // On error from here on, dropping `session` stops the session.
        let consumer = RealtimeConsumer::open(&cfg.session.name, state.clone())?;
        let (tx, rx) = unbounded();
        let stopping = Arc::new(AtomicBool::new(false));

        let c_tx = tx.clone();
        let c_stopping = stopping.clone();
        let c_tracker = tracker.clone();
        let c_state = state.clone();
        let consumer = std::thread::Builder::new()
            .name("strata-etw-consumer".into())
            .spawn(move || {
                let status = consumer.run();
                if !c_stopping.load(Ordering::SeqCst) {
                    // NOTE: nobody asked to stop; report the end ourselves so
                    // the helper can tell the user tracking is off.
                    let mut t = c_tracker.lock().unwrap_or_else(PoisonError::into_inner);
                    let batch = t.flush();
                    if !batch.is_empty() {
                        let _ = c_tx.send(MonitorMessage::Batch(batch));
                    }
                    let _ = c_tx.send(MonitorMessage::Stopped {
                        reason: StopReason::SessionEnded(status),
                        stats: t.stats(),
                        session: SessionStats::default(),
                        panics: c_state.panics.load(Ordering::Relaxed),
                    });
                }
                status
            })
            .map_err(|_| EtwError::Thread("consumer".into()))?;

        let thread_handle = consumer.as_raw_handle() as isize;
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let t_tracker = tracker.clone();
        let t_tx = tx.clone();
        let t_cfg = cfg.clone();
        let timer = std::thread::Builder::new()
            .name("strata-etw-timer".into())
            .spawn(move || timer_loop(&t_cfg, thread_handle, &t_tracker, &t_tx, &stop_rx));
        let timer = match timer {
            Ok(t) => t,
            Err(_) => {
                stopping.store(true, Ordering::SeqCst);
                drop(session);
                let _ = consumer.join();
                return Err(EtwError::Thread("timer".into()));
            }
        };
        Ok((
            Self {
                tracker,
                session: Some(session),
                consumer: Some(consumer),
                timer: Some(timer),
                timer_stop: Some(stop_tx),
                tx,
                stopping,
                state,
                manifest_errors,
            },
            rx,
        ))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Tracker> {
        self.tracker.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Top writers over `w` from memory ("now" and "last hour"; the store
    /// serves longer ranges).
    #[must_use]
    pub fn top_writers(&self, w: Window, limit: usize) -> Vec<WriterSummary> {
        self.lock().top_writers(w, limit)
    }

    /// Per-process, per-directory totals over `w`.
    #[must_use]
    pub fn dir_totals(&self, w: Window, limit: usize) -> Vec<DirTotal> {
        self.lock().dir_totals(w, limit)
    }

    /// Attribution evidence since `since` (at most 48 hours back).
    #[must_use]
    pub fn evidence(&self, since: Timestamp, cfg: &EvidenceConfig) -> Vec<Evidence> {
        self.lock().evidence(since, cfg)
    }

    /// Pipeline counters.
    #[must_use]
    pub fn stats(&self) -> TrackerStats {
        self.lock().stats()
    }

    /// TDH problems met while loading layouts (decoding falls back to the
    /// built-in tables).
    #[must_use]
    pub fn manifest_errors(&self) -> &[String] {
        &self.manifest_errors
    }

    /// Replaces the device map after a volume change.
    pub fn set_devices(&self, devices: DeviceMap) {
        self.lock().set_devices(devices);
    }

    /// Forgets all activity held in memory, including unflushed data.
    /// Pair with `Store::clear_activity` for "Clear activity data".
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Stops the session, joins the threads, sends the final batch and
    /// [`MonitorMessage::Stopped`].
    ///
    /// # Errors
    ///
    /// [`EtwError::Win32`] if the session could not be stopped (the guard
    /// retries on drop, and the next launch's `recover_orphaned_session`
    /// catches it).
    pub fn stop(mut self) -> Result<SessionStats, EtwError> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<SessionStats, EtwError> {
        let Some(mut session) = self.session.take() else {
            return Ok(SessionStats::default());
        };
        // Stop the timer first: it samples the consumer thread's handle,
        // which joining the consumer closes.
        drop(self.timer_stop.take());
        if let Some(t) = self.timer.take() {
            let _ = t.join();
        }
        self.stopping.store(true, Ordering::SeqCst);
        let result = session.stop();
        if result.is_ok()
            && let Some(c) = self.consumer.take()
        {
            let _ = c.join();
        }
        let stats = result.clone().unwrap_or_default();
        let mut t = self.lock();
        let batch = t.flush();
        let tracker_stats = t.stats();
        drop(t);
        if !batch.is_empty() {
            let _ = self.tx.send(MonitorMessage::Batch(batch));
        }
        let _ = self.tx.send(MonitorMessage::Stopped {
            reason: StopReason::Requested,
            stats: tracker_stats,
            session: stats,
            panics: self.state.panics.load(Ordering::Relaxed),
        });
        // NOTE: on a failed stop the guard is dropped here and tries again;
        // the consumer thread is detached rather than joined so this cannot
        // hang on a session that refuses to stop.
        drop(session);
        result
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn timer_loop(
    cfg: &MonitorConfig,
    consumer: isize,
    tracker: &Mutex<Tracker>,
    tx: &Sender<MonitorMessage>,
    stop: &Receiver<()>,
) {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    let mut guard = OverheadGuard::new(cfg.overhead);
    let tick = cfg.overhead_interval.max(Duration::from_millis(100));
    let mut last_cpu = thread_cpu_time(consumer).unwrap_or_default();
    let mut last_tick = Instant::now();
    let mut last_flush = Instant::now();
    loop {
        match stop.recv_timeout(tick) {
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            _ => return,
        }
        let now = Instant::now();
        if let Ok(cpu) = thread_cpu_time(consumer) {
            let report = guard.observe(cpu.saturating_sub(last_cpu), now - last_tick, cpus);
            last_cpu = cpu;
            let mut t = tracker.lock().unwrap_or_else(PoisonError::into_inner);
            if t.sample_rate() != report.sample_rate {
                t.set_sample_rate(report.sample_rate);
            }
            drop(t);
            let _ = tx.send(MonitorMessage::Overhead(report));
        }
        last_tick = now;
        if now - last_flush >= cfg.flush_interval {
            last_flush = now;
            let batch = tracker
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .flush();
            if !batch.is_empty() && tx.send(MonitorMessage::Batch(batch)).is_err() {
                return;
            }
        }
    }
}
