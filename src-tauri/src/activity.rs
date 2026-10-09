//! File-activity tracking (ETW), opt-in from settings.
//!
//! The elevated helper runs the `strata-etw` monitor (protocol v3
//! `StartActivity`); this module drains its stream on a background thread:
//! hourly rollups and last writers go to the store, overhead samples set the
//! throttled state, and every hour the helper's attribution evidence (which
//! process writes where) refines the app catalog. Tracking runs only while
//! `activity.enabled` is set and a helper is connected; it stops when the
//! setting is turned off, the helper goes away, or the app exits.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use strata_helper::client::{ActivityEvent, HelperClient};
use strata_ipc::protocol::{ActivityRow, LastWriteRow};
use strata_store::{ActivitySample, LastWrite, Timestamp};

use crate::classify::Engine;
use crate::state::{AppState, lock};

/// How often attribution evidence is pulled from the helper.
const EVIDENCE_EVERY: Duration = Duration::from_secs(60 * 60);
/// Evidence covers this much recent activity.
const EVIDENCE_WINDOW_SECS: i64 = 48 * 3600;

/// The latest overhead sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Health {
    /// Consumer CPU in hundredths of a percent of the machine.
    pub cpu_centi_percent: u32,
    /// Writes sampled 1 in N.
    pub sample_rate: u32,
    /// The overhead guard suggests turning tracking off.
    pub suggest_disable: bool,
}

#[derive(Debug, Default)]
struct Inner {
    client: Option<Arc<HelperClient>>,
    thread: Option<JoinHandle<()>>,
    stop: Option<Arc<AtomicBool>>,
    health: Option<Health>,
    started_ms: Option<i64>,
    /// The catalog before evidence was applied; evidence accumulates, so it
    /// is always applied to this base.
    base_catalog: Option<strata_classify::catalog::AppCatalog>,
}

/// Tracking state in [`AppState`].
#[derive(Debug, Default)]
pub struct ActivityState(Mutex<Inner>);

impl ActivityState {
    /// Whether the helper is tracking for this app.
    #[must_use]
    pub fn running(&self) -> bool {
        lock(&self.0)
            .thread
            .as_ref()
            .is_some_and(|t| !t.is_finished())
    }

    /// The latest overhead sample.
    #[must_use]
    pub fn health(&self) -> Option<Health> {
        lock(&self.0).health
    }

    /// When tracking started (Unix ms).
    #[must_use]
    pub fn started_ms(&self) -> Option<i64> {
        lock(&self.0).started_ms.filter(|_| self.running())
    }

    /// The client tracking runs on, while it runs.
    #[must_use]
    pub fn client(&self) -> Option<Arc<HelperClient>> {
        if self.running() {
            lock(&self.0).client.clone()
        } else {
            None
        }
    }
}

/// Whether settings ask for tracking.
#[must_use]
pub fn enabled(state: &AppState) -> bool {
    state
        .store()
        .and_then(|s| s.load_settings().ok())
        .is_some_and(|s| s.activity.enabled)
}

fn samples(rows: Vec<ActivityRow>) -> Vec<ActivitySample> {
    rows.into_iter()
        .map(|r| ActivitySample {
            at: Timestamp(r.hour),
            image: r.image,
            dir_hash: r.dir_hash,
            bytes_written: r.bytes_written,
            files_created: r.files_created,
            files_deleted: r.files_deleted,
        })
        .collect()
}

fn last_writes(rows: Vec<LastWriteRow>) -> Vec<LastWrite> {
    rows.into_iter()
        .map(|r| LastWrite {
            path_hash: r.path_hash,
            image: r.image,
            pid: r.pid,
            at: Timestamp(r.at),
        })
        .collect()
}

/// Applies the helper's attribution evidence to a fresh copy of the base
/// catalog.
fn refresh_evidence(state: &AppState, engine: &Engine, client: &HelperClient) {
    let now = crate::model::unix_ms() / 1000;
    let Ok(evidence) = client.activity_evidence(now - EVIDENCE_WINDOW_SECS, 2000) else {
        return;
    };
    let base = {
        let mut g = lock(&state.activity.0);
        if g.base_catalog.is_none() {
            g.base_catalog = engine.catalog.get().map(|c| (*c).clone());
        }
        g.base_catalog.clone()
    };
    let Some(mut catalog) = base else { return };
    for e in &evidence {
        catalog.add_evidence(&e.prefix, &e.app, e.weight_milli as f32 / 1000.0);
    }
    engine.catalog.set(catalog);
}

/// Starts tracking through the connected helper when settings ask for it.
/// Does nothing without a helper that offers activity tracking.
pub fn start(state: &Arc<AppState>) {
    if !enabled(state) || state.activity.running() {
        return;
    }
    let Some(client) = state
        .helper
        .client()
        .filter(|c| c.welcome().capabilities.activity)
    else {
        return;
    };
    let cap = state
        .store()
        .and_then(|s| s.load_settings().ok())
        .map_or(2.0, |s| s.activity.cpu_cap_percent as f32);
    let Ok(stream) = client.start_activity(cap) else {
        return;
    };
    let stop = Arc::new(AtomicBool::new(false));
    let st = state.clone();
    let c2 = client.clone();
    let stop2 = stop.clone();
    let thread = std::thread::Builder::new()
        .name("strata-activity".into())
        .spawn(move || drain(&st, &c2, stream, &stop2));
    let Ok(thread) = thread else { return };
    let mut g = lock(&state.activity.0);
    g.client = Some(client);
    g.thread = Some(thread);
    g.stop = Some(stop);
    g.health = None;
    g.started_ms = Some(crate::model::unix_ms());
}

fn drain(
    state: &AppState,
    client: &HelperClient,
    mut stream: strata_helper::client::ActivityStream,
    stop: &AtomicBool,
) {
    let mut evidence_at = Instant::now();
    while !stop.load(Ordering::Acquire) {
        match stream.next_timeout(Duration::from_millis(500)) {
            Some(Ok(ActivityEvent::Batch {
                rows,
                last_writes: lw,
            })) => {
                if let Some(store) = state.store() {
                    let _ = store.record_activity(&samples(rows));
                    let _ = store.set_last_writers(&last_writes(lw));
                }
            }
            Some(Ok(ActivityEvent::Health {
                cpu_centi_percent,
                sample_rate,
                suggest_disable,
            })) => {
                lock(&state.activity.0).health = Some(Health {
                    cpu_centi_percent,
                    sample_rate,
                    suggest_disable,
                });
            }
            Some(Ok(ActivityEvent::Stopped { .. }) | Err(_)) => break,
            None if stream.is_done() => break,
            None => {}
        }
        if evidence_at.elapsed() >= EVIDENCE_EVERY
            && let Some(engine) = state.engine.get()
        {
            evidence_at = Instant::now();
            refresh_evidence(state, &engine, client);
        }
    }
    if stop.load(Ordering::Acquire) {
        let _ = client.stop_activity();
    }
}

/// Stops tracking and waits for the stream to end.
pub fn stop(state: &AppState) {
    let (thread, stop) = {
        let mut g = lock(&state.activity.0);
        (g.thread.take(), g.stop.take())
    };
    if let Some(s) = stop {
        s.store(true, Ordering::Release);
    }
    if let Some(t) = thread {
        let _ = t.join();
    }
}

/// A helper connected: resume tracking if settings ask for it.
pub fn on_helper_connected(state: &Arc<AppState>) {
    start(state);
}

/// App exit: stop tracking cleanly.
pub fn shutdown(state: &AppState) {
    stop(state);
}

/// Forgets the helper's in-memory activity (stored activity is cleared by
/// the caller).
pub fn clear_helper(state: &AppState) {
    if let Some(c) = state.activity.client() {
        let _ = c.clear_activity();
    }
}

/// `TIME_ZONE_ID_DAYLIGHT`: daylight saving time is in effect.
const TIME_ZONE_ID_DAYLIGHT: u32 = 2;

/// Unix seconds of the most recent local midnight.
#[must_use]
pub fn local_midnight() -> i64 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut tzi = TIME_ZONE_INFORMATION::default();
    // SAFETY: plain query into a stack struct of the right type.
    let id = unsafe { GetTimeZoneInformation(&raw mut tzi) };
    // Bias is "UTC = local + bias", in minutes.
    let bias = i64::from(tzi.Bias)
        + if id == TIME_ZONE_ID_DAYLIGHT {
            i64::from(tzi.DaylightBias)
        } else {
            i64::from(tzi.StandardBias)
        };
    let now = crate::model::unix_ms() / 1000;
    let local = now - bias * 60;
    let midnight_local = local - local.rem_euclid(86_400);
    midnight_local + bias * 60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_convert_to_store_types() {
        let s = samples(vec![ActivityRow {
            hour: 3600,
            image: "tool.exe".into(),
            dir_hash: 7,
            bytes_written: 10,
            files_created: 1,
            files_deleted: 2,
        }]);
        assert_eq!(s[0].at, Timestamp(3600));
        assert_eq!((s[0].bytes_written, s[0].files_deleted), (10, 2));
        let w = last_writes(vec![LastWriteRow {
            path_hash: 9,
            image: "tool.exe".into(),
            pid: Some(4),
            at: 5,
        }]);
        assert_eq!((w[0].path_hash, w[0].at), (9, Timestamp(5)));
    }

    #[test]
    fn midnight_is_within_the_last_day() {
        let now = crate::model::unix_ms() / 1000;
        let m = local_midnight();
        assert!(m <= now && now - m < 86_400 + 3600);
    }
}
