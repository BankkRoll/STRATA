//! Entry data and entry actions: `entry_info`, `entry_path`,
//! `entry_detail`, `list_children`, `apps_brief`, `entry_action`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};
use tauri::ipc::Response;
use tauri::{AppHandle, State};

use super::volumes::volume_key;
use super::{blocking, with_data};
use crate::detail::{self, EntryDetail, EntryInfo, HistoryPoint};
use crate::error::{CmdResult, CommandError};
use crate::rows::{RowQuery, row_page};
use crate::shell;
use crate::state::AppState;

/// Largest `entry_info` batch.
pub const MAX_INFO_BATCH: usize = 512;

/// Infos for up to [`MAX_INFO_BATCH`] ids; unknown ids are omitted.
///
/// # Errors
///
/// Unknown volume or too many ids.
#[tauri::command]
pub async fn entry_info(
    state: State<'_, Arc<AppState>>,
    volume_id: String,
    ids: Vec<u32>,
) -> CmdResult<Vec<EntryInfo>> {
    if ids.len() > MAX_INFO_BATCH {
        return Err(CommandError::bad_request("at most 512 ids per call"));
    }
    let st = state.inner().clone();
    blocking(move || {
        let engine = st.engine.get();
        with_data(&st, &volume_id, |data| {
            Ok(ids
                .iter()
                .filter_map(|&w| data.resolve(w))
                .map(|id| detail::entry_info(data, engine.as_deref(), id))
                .collect())
        })
    })
    .await
}

/// Win32 path of an entry (no `\\?\`).
///
/// # Errors
///
/// Unknown volume or id.
#[tauri::command]
pub async fn entry_path(
    state: State<'_, Arc<AppState>>,
    volume_id: String,
    id: u32,
) -> CmdResult<String> {
    let st = state.inner().clone();
    blocking(move || {
        with_data(&st, &volume_id, |data| {
            let e = data
                .resolve(id)
                .ok_or_else(|| CommandError::not_found("that item is no longer in the index"))?;
            Ok(data.path(e))
        })
    })
    .await
}

/// Full detail of one entry, including live filesystem facts and history.
///
/// # Errors
///
/// Unknown volume or id.
#[tauri::command]
pub async fn entry_detail(
    state: State<'_, Arc<AppState>>,
    volume_id: String,
    id: u32,
) -> CmdResult<EntryDetail> {
    let st = state.inner().clone();
    blocking(move || {
        let engine = st.engine.get();
        let unreliable = st.access_unreliable.load(Ordering::Relaxed);
        let (mut d, dir_path, path) = with_data(&st, &volume_id, |data| {
            let e = data
                .resolve(id)
                .ok_or_else(|| CommandError::not_found("that item is no longer in the index"))?;
            let d = detail::entry_detail(data, engine.as_deref(), &volume_id, e, unreliable);
            Ok((d, data.index.is_dir(e).then(|| data.path(e)), data.path(e)))
        })?;
        if let (Some(path), Some(store), Some(key)) =
            (dir_path, st.store(), volume_key(&st, &volume_id))
        {
            let hash = strata_store::path_hash(&path);
            if let Ok(points) = store.dir_series(&key, hash, 30) {
                let h: Vec<HistoryPoint> = points
                    .iter()
                    .filter_map(|p| {
                        p.sizes.map(|s| HistoryPoint {
                            at_ms: p.at.0.saturating_mul(1000),
                            allocated: s.allocated,
                        })
                    })
                    .collect();
                d.history = (!h.is_empty()).then_some(h);
            }
        }
        if let Some(w) = st
            .store()
            .and_then(|s| s.last_writer(strata_store::path_hash(&path)).ok().flatten())
        {
            d.last_writer = Some(detail::LastWriter {
                process: w
                    .image
                    .rsplit(['\\', '/'])
                    .next()
                    .unwrap_or(&w.image)
                    .to_owned(),
                pid: w.pid.unwrap_or(0),
                at_ms: w.at.0.saturating_mul(1000),
            });
        }
        Ok(d)
    })
    .await
}

/// One binary `STRP` row page.
///
/// # Errors
///
/// Unknown volume or parent.
#[tauri::command]
pub async fn list_children(
    state: State<'_, Arc<AppState>>,
    query: RowQuery,
) -> CmdResult<Response> {
    let st = state.inner().clone();
    blocking(move || with_data(&st, &query.volume_id, |data| row_page(data, &query)))
        .await
        .map(Response::new)
}

/// `{ id, name }` of an app.
#[derive(Debug, Clone, Serialize)]
pub struct AppBrief {
    id: u32,
    name: String,
}

/// Every app label referenced by `owner_app` ids.
#[tauri::command]
pub fn apps_brief(state: State<'_, Arc<AppState>>) -> Vec<AppBrief> {
    state
        .engine
        .get()
        .map(|e| {
            e.apps
                .all()
                .into_iter()
                .map(|(id, name)| AppBrief { id, name })
                .collect()
        })
        .unwrap_or_default()
}

/// Shell actions on entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EntryAction {
    /// Open with the default handler.
    Open,
    /// Reveal in Explorer.
    Reveal,
    /// Windows Properties dialog.
    Properties,
    /// Terminal in the folder (the file's folder for files).
    OpenTerminal,
}

/// Runs a shell action on entries.
///
/// # Errors
///
/// Unknown entries, virtual entries, or the shell's failure.
#[tauri::command]
pub async fn entry_action(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    action: EntryAction,
    volume_id: String,
    ids: Vec<u32>,
) -> CmdResult<()> {
    let st = state.inner().clone();
    let targets: Vec<(PathBuf, bool)> = blocking(move || {
        with_data(&st, &volume_id, |data| {
            ids.iter()
                .map(|&w| {
                    let id = data.resolve(w).ok_or_else(|| {
                        CommandError::not_found("that item is no longer in the index")
                    })?;
                    if data
                        .index
                        .flags(id)
                        .contains(strata_core::EntryFlags::VIRTUAL)
                    {
                        return Err(CommandError::bad_request(
                            "this block is computed by Strata and has no file on disk",
                        ));
                    }
                    Ok((PathBuf::from(data.path(id)), data.index.is_dir(id)))
                })
                .collect()
        })
    })
    .await?;
    match action {
        EntryAction::Properties | EntryAction::Reveal => {
            // NOTE: shell dialogs and Explorer selection need an STA thread
            // with a message loop that outlives the call: the main thread.
            app.run_on_main_thread(move || {
                for (p, _) in &targets {
                    let _ = if action == EntryAction::Properties {
                        shell::properties(p)
                    } else {
                        shell::reveal(p)
                    };
                }
            })
            .map_err(|e| CommandError::internal(e.to_string()))
        }
        EntryAction::Open => {
            blocking(move || {
                for (p, _) in &targets {
                    shell::open(p).map_err(CommandError::io)?;
                }
                Ok(())
            })
            .await
        }
        EntryAction::OpenTerminal => {
            let (p, is_dir) = targets
                .first()
                .cloned()
                .ok_or_else(|| CommandError::bad_request("nothing selected"))?;
            let dir = if is_dir {
                p
            } else {
                p.parent().map(PathBuf::from).unwrap_or(p)
            };
            shell::open_terminal(&dir).map_err(CommandError::io)
        }
    }
}
