//! Duplicate finder commands (`ui/src/lib/dupes.ts`).

use std::sync::Arc;

use serde::Serialize;
use strata_clean::CancelToken;
use strata_clean::consent::Prompt;
use strata_core::Safety;
use strata_dupes::hardlink::{self, HardlinkRefusal, LinkConfig};
use tauri::{AppHandle, Manager, State};

use super::blocking;
use crate::dupes::{self, DupeGroups, DupeScanStatus, DupeSelection};
use crate::error::{CmdResult, CommandError};
use crate::features::audit::{ItemMeta, StoreAuditLog, VolumeCache};
use crate::features::queue::{QueueAddResult, QueueSource, add_to_queue, candidates};
use crate::state::AppState;

/// The finder's state (`dupes_status`).
#[tauri::command]
pub fn dupes_status(state: State<'_, Arc<AppState>>) -> DupeScanStatus {
    state.dupes.status()
}

/// Starts (or resumes) a scan over volumes (`dupes_start`); progress
/// arrives on `dupes://status`.
///
/// # Errors
///
/// `busy` while a scan runs.
#[tauri::command]
pub fn dupes_start(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    volume_ids: Vec<String>,
    min_bytes: u64,
) -> CmdResult<()> {
    dupes::start(&app, state.inner(), &volume_ids, min_bytes)
        .map_err(|m| CommandError::new(crate::error::ErrorCode::Busy, m))
}

/// Cancels the scan (`dupes_cancel`).
#[tauri::command]
pub fn dupes_cancel(state: State<'_, Arc<AppState>>) {
    dupes::cancel(&state);
}

/// A page of groups by wasted bytes (`dupes_groups`).
///
/// # Errors
///
/// Never; empty before the first scan.
#[tauri::command]
pub async fn dupes_groups(
    state: State<'_, Arc<AppState>>,
    offset: usize,
    limit: usize,
) -> CmdResult<DupeGroups> {
    let st = state.inner().clone();
    blocking(move || Ok(dupes::groups(&st, offset, limit))).await
}

fn report(st: &AppState) -> CmdResult<(strata_dupes::DuplicateReport, dupes::VolumeMap)> {
    dupes::report_and_volumes(st)
        .ok_or_else(|| CommandError::not_found("run a duplicate scan first"))
}

/// Queues the selected copies (`dupes_queue`), refusing any selection that
/// covers every copy of a group.
///
/// # Errors
///
/// No report, an outdated group, or a selection covering every copy.
#[tauri::command]
pub async fn dupes_queue(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    selections: Vec<DupeSelection>,
) -> CmdResult<QueueAddResult> {
    let st = state.inner().clone();
    blocking(move || {
        let (rep, volumes) = report(&st)?;
        let sel = dupes::select(&rep, &selections).map_err(CommandError::bad_request)?;
        let mut cands = Vec::new();
        let mut refused = Vec::new();
        for (volume_id, ids) in dupes::selected_entries(&st, &rep, &volumes, &sel) {
            let (mut c, mut r) = candidates(&st, &volume_id, &ids);
            cands.append(&mut c);
            refused.append(&mut r);
        }
        add_to_queue(&app, cands, refused, QueueSource::Duplicates)
    })
    .await
}

/// The consent prompt for replacing copies with hardlinks
/// (`HardlinkPrompt` in the UI).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HardlinkPrompt {
    /// Handle for `dupes_hardlink`.
    pub prompt_id: u64,
    /// Warning text shown verbatim.
    pub message: String,
    /// Copies that will become links.
    pub files: u64,
    /// Bytes freed.
    pub bytes_saved: u64,
    /// Selected copies refused because they are on another volume than the
    /// kept copy.
    pub refused_cross_volume: u64,
    /// Unix ms after which the prompt is void.
    pub expires_ms: i64,
}

/// Prepares replacing selected copies with hardlinks to the kept copy
/// (`dupes_hardlink_prompt`). Changes nothing.
///
/// # Errors
///
/// No report, a selection covering every copy, or nothing that can be
/// linked.
#[tauri::command]
pub async fn dupes_hardlink_prompt(
    state: State<'_, Arc<AppState>>,
    selections: Vec<DupeSelection>,
) -> CmdResult<HardlinkPrompt> {
    let st = state.inner().clone();
    blocking(move || {
        let (rep, _) = report(&st)?;
        let sel = dupes::select(&rep, &selections).map_err(CommandError::bad_request)?;
        let mut prompts = Vec::new();
        let mut refused_cross_volume = 0u64;
        let mut message = String::new();
        for s in &selections {
            match hardlink::plan(&rep, &sel, s.group_id) {
                Ok(action) => {
                    let p = Prompt::new(action);
                    message.push_str(&p.text());
                    message.push('\n');
                    prompts.push(p);
                }
                Err(HardlinkRefusal::DifferentVolume { .. }) => {
                    refused_cross_volume += s.file_ids.len() as u64;
                }
                Err(HardlinkRefusal::NothingMarked) => {}
                Err(e) => return Err(CommandError::bad_request(e.to_string())),
            }
        }
        if prompts.is_empty() {
            return Err(CommandError::bad_request(
                "none of the selected copies can be replaced with hardlinks",
            ));
        }
        let files = prompts
            .iter()
            .map(|p| p.action().replace.len() as u64)
            .sum();
        let bytes_saved = prompts.iter().map(|p| p.action().bytes_saved).sum();
        let (prompt_id, expires_ms) = dupes::offer_hardlinks(&st, prompts)?;
        Ok(HardlinkPrompt {
            prompt_id,
            message: message.trim_end().to_owned(),
            files,
            bytes_saved,
            refused_cross_volume,
            expires_ms,
        })
    })
    .await
}

/// One copy that could not be replaced.
#[derive(Debug, Clone, Serialize)]
pub struct HardlinkFailure {
    /// The copy.
    pub path: String,
    /// Why.
    pub message: String,
}

/// Result of `dupes_hardlink`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HardlinkResult {
    /// Copies replaced.
    pub replaced: u64,
    /// Bytes freed.
    pub bytes_saved: u64,
    /// Copies left alone, with the reason.
    pub failed: Vec<HardlinkFailure>,
}

/// Replaces the copies after the user confirmed the prompt
/// (`dupes_hardlink`). Each original goes to the Recycle Bin through the
/// cleaner and is logged as a `duplicates` action.
///
/// # Errors
///
/// An unknown, expired or too-fast confirmation, or the safety guard could
/// not start.
#[tauri::command]
pub async fn dupes_hardlink(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    prompt_id: u64,
) -> CmdResult<HardlinkResult> {
    let st = state.inner().clone();
    let store = crate::features::store::handle(&app)?;
    let protected: Vec<_> = app.path().app_local_data_dir().ok().into_iter().collect();
    blocking(move || {
        let prompts = dupes::take_hardlinks(&st, prompt_id)?;
        let guard = crate::features::cleanup::machine_guard(&protected)?;
        let mut volumes = VolumeCache::default();
        let mut result = HardlinkResult {
            replaced: 0,
            bytes_saved: 0,
            failed: Vec::new(),
        };
        for prompt in prompts {
            let action = prompt.action().clone();
            let meta = action
                .replace
                .iter()
                .map(|(i, f)| {
                    (
                        strata_dupes::queue_item_id(action.group, *i),
                        ItemMeta {
                            mtime: f.mtime,
                            volume: volumes.key_for(&f.path),
                            method: strata_store::DeleteMethod::Recycle,
                            rule_id: None,
                        },
                    )
                })
                .collect();
            let mut log =
                StoreAuditLog::new(store.clone(), strata_store::ActionKind::Duplicates, meta);
            // This handler is the confirmation click; the consent is minted
            // and redeemed on this thread.
            let outcomes = hardlink::replace_with_hardlinks(
                &guard,
                prompt.confirm(),
                |_| Safety::Careful,
                &mut log,
                &LinkConfig::default(),
                &CancelToken::new(),
            )
            .map_err(|e| CommandError::new(crate::error::ErrorCode::Consent, e.to_string()))?;
            for o in outcomes {
                match o.result {
                    Ok(_) => {
                        result.replaced += 1;
                        result.bytes_saved += action.keeper.size;
                    }
                    Err(e) => result.failed.push(HardlinkFailure {
                        path: o.path.display().to_string(),
                        message: e.to_string(),
                    }),
                }
            }
        }
        Ok(result)
    })
    .await
}
