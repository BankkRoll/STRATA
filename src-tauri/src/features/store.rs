//! Store lifecycle (SPEC §18, §21).
//!
//! Responsibilities:
//! - Open `strata-store` in the per-user app data directory at startup and
//!   keep the handle in managed state ([`AppStore`]).
//! - Report database health and offer the two resets.
//! - Daily maintenance: snapshot retention and activity pruning.
//!
//! Every other track should reach the store through [`handle`] rather than
//! opening a second one.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use strata_store::{DbHealth, Store, StoreHealth};
use tauri::{AppHandle, Manager, Runtime};

use super::error::{ErrorKind, FeatureError, FeatureResult, blocking};

/// Name of the store directory under the app's local data directory.
pub const STORE_DIR_NAME: &str = "store";

/// How often maintenance runs.
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// How often the maintenance thread wakes up to check whether a run is due.
const MAINTENANCE_TICK: Duration = Duration::from_secs(60 * 60);

/// Managed state: the opened store, or why it could not be opened.
#[derive(Debug)]
pub struct AppStore {
    store: Result<Store, String>,
    dir: PathBuf,
}

impl AppStore {
    /// Opens the store in `dir`. Never fails: an unusable directory is
    /// recorded and reported by every store-backed command.
    #[must_use]
    pub fn open(dir: PathBuf) -> Self {
        let store = Store::open(&dir).map_err(|e| e.to_string());
        Self { store, dir }
    }

    /// A store that could not even be located (no app data folder).
    #[must_use]
    pub fn unavailable(reason: String) -> Self {
        Self {
            store: Err(reason),
            dir: PathBuf::new(),
        }
    }

    /// The store handle.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::StoreUnavailable`] when the store directory could not be
    /// created.
    pub fn get(&self) -> FeatureResult<Store> {
        self.store.clone().map_err(|e| {
            FeatureError::new(
                ErrorKind::StoreUnavailable,
                format!("Strata's data folder could not be opened: {e}"),
            )
        })
    }

    /// Directory holding the database files.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// The store handle from managed state.
///
/// # Errors
///
/// [`ErrorKind::StoreUnavailable`] when the store is not open.
pub fn handle<R: Runtime>(app: &AppHandle<R>) -> FeatureResult<Store> {
    app.try_state::<AppStore>()
        .ok_or_else(|| FeatureError::new(ErrorKind::StoreUnavailable, "the store is not open"))?
        .get()
}

/// Where the store lives for this user (`%LOCALAPPDATA%\<identifier>\store`).
///
/// # Errors
///
/// Fails when Windows cannot report the local app data folder.
pub fn store_dir<R: Runtime>(app: &AppHandle<R>) -> FeatureResult<PathBuf> {
    app.path()
        .app_local_data_dir()
        .map(|d| d.join(STORE_DIR_NAME))
        .map_err(|e| FeatureError::new(ErrorKind::Io, format!("no local app data folder: {e}")))
}

/// Health of one database, for the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbHealthDto {
    /// `ok`, `corrupt`, `too_new` or `unavailable`.
    pub status: &'static str,
    /// Details for anything but `ok`.
    pub detail: Option<String>,
    /// Whether a reset would fix it.
    pub resettable: bool,
}

impl From<&DbHealth> for DbHealthDto {
    fn from(h: &DbHealth) -> Self {
        match h {
            DbHealth::Ok => Self {
                status: "ok",
                detail: None,
                resettable: false,
            },
            DbHealth::Corrupt { detail } => Self {
                status: "corrupt",
                detail: Some(detail.clone()),
                resettable: true,
            },
            DbHealth::TooNew { found, supported } => Self {
                status: "too_new",
                detail: Some(format!(
                    "written by a newer Strata (schema {found}, this build reads up to {supported})"
                )),
                resettable: true,
            },
            DbHealth::Unavailable { detail } => Self {
                status: "unavailable",
                detail: Some(detail.clone()),
                resettable: true,
            },
        }
    }
}

/// Response of `store_health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreHealthDto {
    /// Whether the store directory could be opened at all.
    pub open: bool,
    /// Why not, when `open` is false.
    pub error: Option<String>,
    /// `history.db` (snapshots, activity, caches; safe to reset).
    pub history: Option<DbHealthDto>,
    /// `state.db` (settings, undo log; reset loses them).
    pub state: Option<DbHealthDto>,
}

impl StoreHealthDto {
    fn from_health(h: &StoreHealth) -> Self {
        Self {
            open: true,
            error: None,
            history: Some((&h.history).into()),
            state: Some((&h.state).into()),
        }
    }
}

/// Response of the reset commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetDto {
    /// Where the damaged file was moved, if there was one.
    pub moved_to: Option<String>,
    /// Health after the reset.
    pub health: StoreHealthDto,
}

/// What one maintenance pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MaintenanceReport {
    /// Snapshots removed by retention.
    pub deleted_snapshots: u64,
    /// Activity rows removed.
    pub pruned_activity_rows: u64,
}

/// One maintenance pass: retention per the user's history settings and
/// activity pruning per the activity settings.
///
/// # Errors
///
/// Store errors; a damaged history database fails here and is reported by
/// `store_health`.
pub fn run_maintenance(store: &Store) -> FeatureResult<MaintenanceReport> {
    let settings = store.load_settings()?;
    let retention = store.apply_retention(&settings.history.retention_policy())?;
    let activity = store.prune_activity(settings.activity.retention_days)?;
    Ok(MaintenanceReport {
        deleted_snapshots: retention.deleted_snapshots,
        pruned_activity_rows: activity.hourly_rows + activity.last_writer_rows,
    })
}

/// Whether a maintenance pass is due.
#[must_use]
pub fn maintenance_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|l| now.saturating_duration_since(l) >= MAINTENANCE_INTERVAL)
}

/// Starts the daily maintenance thread. The first pass runs right away
/// (after crash recovery, which the caller orders before this).
pub fn spawn_maintenance(store: Store) {
    let spawned = std::thread::Builder::new()
        .name("strata-maintenance".into())
        .spawn(move || {
            let mut last = None;
            loop {
                let now = Instant::now();
                if maintenance_due(last, now) {
                    // NOTE: a failed pass (damaged history.db) is retried on
                    // the next interval; health is surfaced by store_health.
                    let _ = run_maintenance(&store);
                    last = Some(now);
                }
                std::thread::sleep(MAINTENANCE_TICK);
            }
        });
    // NOTE: if the OS refuses a thread, retention simply runs next launch.
    drop(spawned);
}

fn health_dto<R: Runtime>(app: &AppHandle<R>) -> StoreHealthDto {
    match app.try_state::<AppStore>().map(|s| s.get()) {
        Some(Ok(store)) => StoreHealthDto::from_health(&store.health()),
        Some(Err(e)) => StoreHealthDto {
            open: false,
            error: Some(e.message),
            history: None,
            state: None,
        },
        None => StoreHealthDto {
            open: false,
            error: Some("the store is not open".into()),
            history: None,
            state: None,
        },
    }
}

/// Health of both databases.
#[tauri::command]
pub async fn store_health<R: Runtime>(app: AppHandle<R>) -> FeatureResult<StoreHealthDto> {
    blocking(move || Ok(health_dto(&app))).await
}

/// Moves a damaged `history.db` aside and starts a fresh one. Settings and
/// the undo log are untouched.
#[tauri::command]
pub async fn store_reset_history<R: Runtime>(app: AppHandle<R>) -> FeatureResult<ResetDto> {
    blocking(move || {
        let report = handle(&app)?.reset_history()?;
        Ok(ResetDto {
            moved_to: report.moved_to.map(|p| p.display().to_string()),
            health: health_dto(&app),
        })
    })
    .await
}

/// Moves a damaged `state.db` aside and starts a fresh one. This loses
/// settings and the undo log, so it is refused while `state.db` is healthy.
#[tauri::command]
pub async fn store_reset_state<R: Runtime>(app: AppHandle<R>) -> FeatureResult<ResetDto> {
    blocking(move || {
        let store = handle(&app)?;
        if store.health().state == DbHealth::Ok {
            return Err(FeatureError::invalid(
                "the settings database is healthy; resetting it would lose settings and the undo log",
            ));
        }
        let report = store.reset_state()?;
        // A fresh state.db has no interrupted actions; cleanup can start.
        if let Some(cleanup) = app.try_state::<super::cleanup::CleanupState>() {
            cleanup.service().set_ready();
        }
        Ok(ResetDto {
            moved_to: report.moved_to.map(|p| p.display().to_string()),
            health: health_dto(&app),
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_runs_on_a_fresh_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = AppStore::open(dir.path().join("s")).get().unwrap();
        assert_eq!(
            run_maintenance(&store).unwrap(),
            MaintenanceReport::default()
        );
    }

    #[test]
    fn maintenance_is_due_daily() {
        let now = Instant::now();
        assert!(maintenance_due(None, now));
        assert!(!maintenance_due(Some(now), now + Duration::from_secs(3600)));
        assert!(maintenance_due(Some(now), now + MAINTENANCE_INTERVAL));
    }

    #[test]
    fn health_dto_reports_resettable_states() {
        let ok: DbHealthDto = (&DbHealth::Ok).into();
        assert_eq!((ok.status, ok.resettable), ("ok", false));
        let bad: DbHealthDto = (&DbHealth::Corrupt { detail: "x".into() }).into();
        assert_eq!((bad.status, bad.resettable), ("corrupt", true));
    }
}
