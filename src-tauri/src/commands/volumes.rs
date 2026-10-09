//! Volumes, scanning and helper commands.

use std::sync::Arc;

use tauri::{AppHandle, State};

use super::blocking;
use crate::error::{CmdResult, CommandError, ErrorCode};
use crate::helper::{HelperError, HelperStatus};
use crate::jobs::{self, ScanMode};
use crate::state::{AppState, lock};
use crate::volumes::VolumeDto;

/// Every known volume with its scan state.
#[tauri::command]
pub async fn list_volumes(state: State<'_, Arc<AppState>>) -> CmdResult<Vec<VolumeDto>> {
    let st = state.inner().clone();
    blocking(move || Ok(jobs::volume_list(&st))).await
}

/// Elevation and helper connection state.
#[tauri::command]
pub fn helper_status(state: State<'_, Arc<AppState>>) -> HelperStatus {
    state.helper.status()
}

/// Connects the elevated helper (service, or a UAC prompt) or reconnects
/// after a crash.
///
/// # Errors
///
/// `unavailable` without a helper binary, `declined` when the user says no.
#[tauri::command]
pub async fn helper_elevate(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> CmdResult<HelperStatus> {
    let st = state.inner().clone();
    blocking(move || match crate::helper::elevate(&app, &st) {
        Ok(_) => Ok(st.helper.status()),
        Err(HelperError::Unavailable(m)) => Err(CommandError::unavailable(m)),
        Err(HelperError::Declined) => Err(CommandError::new(
            ErrorCode::Declined,
            HelperError::Declined.to_string(),
        )),
        Err(e) => Err(CommandError::io(e.to_string())),
    })
    .await
}

/// Starts a scan; progress arrives through `volumes://changed`.
///
/// # Errors
///
/// Unknown, absent, locked or busy volumes.
#[tauri::command]
pub fn scan_start(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    volume_id: String,
    mode: ScanMode,
) -> CmdResult<()> {
    jobs::start(&app, state.inner(), &volume_id, mode)
}

/// Cancels a running scan; the partial result stays browsable.
#[tauri::command]
pub fn scan_cancel(state: State<'_, Arc<AppState>>, volume_id: String) {
    if let Some(s) = state.existing_session(&volume_id)
        && let Some(c) = lock(&s.cancel).as_ref()
    {
        c.cancel();
    }
}

/// The store key of a volume.
#[must_use]
pub fn volume_key(state: &AppState, volume_id: &str) -> Option<strata_store::VolumeKey> {
    let reg = lock(&state.registry);
    let e = reg.volumes.get(volume_id)?;
    Some(strata_store::VolumeKey {
        serial: u64::from(e.info.serial.unwrap_or(0)),
        guid_path: e.info.guid_path.clone()?,
    })
}
