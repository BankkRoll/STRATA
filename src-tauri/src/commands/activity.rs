//! File-activity commands (`ui/src/lib/activity.ts`).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use strata_ipc::protocol::ActivityWindow;
use strata_store::{Timestamp, WriterTotal, path_hash};
use tauri::{AppHandle, State};

use super::{blocking, with_data};
use crate::error::{CmdResult, CommandError};
use crate::state::AppState;

/// Tracking state (`ActivityStatus` in the UI).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityStatus {
    /// `activity.enabled` (opt-in, off by default).
    pub enabled: bool,
    /// The ETW session runs.
    pub running: bool,
    /// Tracking needs the elevated helper and none is connected.
    pub needs_helper: bool,
    /// The overhead guard is sampling writes.
    pub throttled: bool,
    /// Recent CPU share of tracing, in percent of the machine.
    pub cpu_percent: Option<f64>,
    /// When tracking started (Unix ms).
    pub since_ms: Option<i64>,
    /// Days of activity kept.
    pub retention_days: u32,
}

fn status(state: &AppState) -> ActivityStatus {
    let settings = state.store().and_then(|s| s.load_settings().ok());
    let enabled = settings.as_ref().is_some_and(|s| s.activity.enabled);
    let running = state.activity.running();
    let health = state.activity.health();
    ActivityStatus {
        enabled,
        running,
        needs_helper: enabled
            && !running
            && !state
                .helper
                .client()
                .is_some_and(|c| c.welcome().capabilities.activity),
        throttled: health.is_some_and(|h| h.suggest_disable || h.sample_rate > 1),
        cpu_percent: health.map(|h| f64::from(h.cpu_centi_percent) / 100.0),
        since_ms: state.activity.started_ms(),
        retention_days: settings.map_or(30, |s| s.activity.retention_days),
    }
}

/// Tracking state (`activity_status`).
#[tauri::command]
pub fn activity_status(state: State<'_, Arc<AppState>>) -> ActivityStatus {
    status(&state)
}

/// Turns tracking on or off and remembers the choice
/// (`activity_set_enabled`). Turning it on connects the helper first, which
/// may show the UAC prompt.
///
/// # Errors
///
/// The settings could not be saved.
#[tauri::command]
pub async fn activity_set_enabled(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    enabled: bool,
) -> CmdResult<ActivityStatus> {
    let st = state.inner().clone();
    blocking(move || {
        let store = crate::features::store::handle(&app)?;
        let mut s = store.load_settings()?;
        if s.activity.enabled != enabled {
            s.activity.enabled = enabled;
            let (saved, fx) = crate::features::settings::save(&store, &s)?;
            crate::features::settings::announce(&app, &saved, &fx);
        }
        if enabled {
            if st.helper.client().is_none() {
                // NOTE: a declined prompt leaves tracking enabled but not
                // running; the status reports that it needs the helper.
                let _ = crate::helper::elevate(&app, &st);
            }
            crate::activity::start(&st);
        } else {
            crate::activity::stop(&st);
        }
        Ok(status(&st))
    })
    .await
}

/// Time window for top writers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Window {
    /// The last minute or two (helper memory).
    Now,
    /// The last hour (helper memory).
    Hour,
    /// Since local midnight (store).
    Today,
}

/// One directory a process wrote to.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirBytes {
    /// Directory.
    pub path: String,
    /// Bytes written there.
    pub bytes_written: u64,
}

/// One process's writes (`WriterRow` in the UI).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriterRow {
    /// Full image path.
    pub image: String,
    /// Executable name.
    pub name: String,
    /// Bytes written.
    pub bytes_written: u64,
    /// Files created.
    pub files_created: u64,
    /// Files deleted.
    pub files_deleted: u64,
    /// Directories it wrote most to.
    pub top_dirs: Vec<DirBytes>,
}

fn exe_name(image: &str) -> String {
    image.rsplit(['\\', '/']).next().unwrap_or(image).to_owned()
}

fn from_store(t: WriterTotal) -> WriterRow {
    WriterRow {
        name: exe_name(&t.image),
        image: t.image,
        bytes_written: t.bytes_written,
        files_created: t.files_created,
        files_deleted: t.files_deleted,
        top_dirs: Vec::new(),
    }
}

/// Top writers in a window (`activity_top`): now and the last hour from the
/// helper's memory, today from the store.
///
/// # Errors
///
/// Store errors.
#[tauri::command]
pub async fn activity_top(
    state: State<'_, Arc<AppState>>,
    window: Window,
    limit: u32,
) -> CmdResult<Vec<WriterRow>> {
    let st = state.inner().clone();
    blocking(move || {
        let limit = limit.clamp(1, 500);
        match window {
            Window::Now | Window::Hour => {
                let Some(client) = st.activity.client() else {
                    return Ok(Vec::new());
                };
                let w = if window == Window::Now {
                    ActivityWindow::Now
                } else {
                    ActivityWindow::LastHour
                };
                let rows = client
                    .query_activity(w, limit)
                    .map_err(|e| CommandError::io(e.to_string()))?;
                Ok(rows
                    .into_iter()
                    .map(|r| WriterRow {
                        name: exe_name(&r.image),
                        image: r.image,
                        bytes_written: r.bytes_written,
                        files_created: r.files_created,
                        files_deleted: r.files_deleted,
                        top_dirs: r
                            .top_dirs
                            .into_iter()
                            .map(|d| DirBytes {
                                path: d.dir,
                                bytes_written: d.bytes_written,
                            })
                            .collect(),
                    })
                    .collect())
            }
            Window::Today => {
                let store = st
                    .store()
                    .ok_or_else(|| CommandError::unavailable("activity history is unavailable"))?;
                Ok(store
                    .top_writers(Timestamp(crate::activity::local_midnight()), limit as usize)?
                    .into_iter()
                    .map(from_store)
                    .collect())
            }
        }
    })
    .await
}

/// Writers under one folder over the last `days` (`activity_dir_writers`).
///
/// # Errors
///
/// Unknown volume or entry, or store errors.
#[tauri::command]
pub async fn activity_dir_writers(
    state: State<'_, Arc<AppState>>,
    volume_id: String,
    id: u32,
    days: u32,
) -> CmdResult<Vec<WriterRow>> {
    let st = state.inner().clone();
    blocking(move || {
        let path = with_data(&st, &volume_id, |d| {
            d.resolve(id)
                .map(|e| d.path(e))
                .ok_or_else(|| CommandError::not_found("that folder is no longer in the index"))
        })?;
        let store = st
            .store()
            .ok_or_else(|| CommandError::unavailable("activity history is unavailable"))?;
        let since = crate::model::unix_ms() / 1000 - i64::from(days.clamp(1, 365)) * 86_400;
        Ok(store
            .dir_writers(path_hash(&path), Timestamp(since), 20)?
            .into_iter()
            .map(from_store)
            .collect())
    })
    .await
}

/// Deletes all activity data, stored and in the helper (`activity_clear`).
///
/// # Errors
///
/// Store errors.
#[tauri::command]
pub async fn activity_clear(state: State<'_, Arc<AppState>>) -> CmdResult<()> {
    let st = state.inner().clone();
    blocking(move || {
        if let Some(store) = st.store() {
            store.clear_activity()?;
        }
        crate::activity::clear_helper(&st);
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_names() {
        assert_eq!(exe_name(r"C:\Tools\builder.exe"), "builder.exe");
        assert_eq!(exe_name("System"), "System");
    }
}
