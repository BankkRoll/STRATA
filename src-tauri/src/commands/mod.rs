//! Index-backed Tauri commands, one module per area. Registration and the
//! capability list are in `lib.rs` (`app_commands!`).
//!
//! Every command that reads an index is `async` and runs its work on the
//! blocking pool ([`blocking`]), so the main thread never waits on an index
//! lock or a layout.

use std::sync::Arc;

pub(crate) use crate::error::blocking;
use crate::error::{CmdResult, CommandError};
use crate::model::VolumeData;
use crate::state::{AppState, read};

pub mod activity;
pub mod app;
pub mod dupes;
pub mod entries;
pub mod insights;
pub mod layout;
pub mod rules;
pub mod search;
pub mod volumes;

/// Runs `f` with the volume's published index under its read lock.
pub(crate) fn with_data<T>(
    state: &Arc<AppState>,
    volume_id: &str,
    f: impl FnOnce(&VolumeData) -> CmdResult<T>,
) -> CmdResult<T> {
    let session = state
        .existing_session(volume_id)
        .ok_or_else(|| CommandError::not_found("this volume has not been scanned"))?;
    let guard = read(&session.data);
    let data = guard
        .as_ref()
        .ok_or_else(|| CommandError::not_found("this volume has not been scanned"))?;
    f(data)
}
