//! `search_open` / `search_query` / `search_close`.

use std::sync::Arc;

use tauri::State;
use tauri::ipc::Channel;

use super::layout::StreamHandle;
use crate::error::{CmdResult, CommandError};
use crate::search::{SearchBatch, SearchQuery, SearchStream, Target};
use crate::state::{AppState, lock};

/// Opens a search stream whose batches go to `on_results`.
#[tauri::command]
pub fn search_open(
    state: State<'_, Arc<AppState>>,
    on_results: Channel<SearchBatch>,
) -> StreamHandle {
    let id = state.stream_id();
    let stream = SearchStream::new(Arc::new(move |b| {
        let _ = on_results.send(b);
    }));
    lock(&state.searches).insert(id, Arc::new(stream));
    StreamHandle { stream_id: id }
}

/// Starts query `seq`, cancelling the stream's previous query. Batches carry
/// `seq`.
///
/// # Errors
///
/// Unknown stream.
#[tauri::command]
pub fn search_query(
    state: State<'_, Arc<AppState>>,
    stream_id: u32,
    seq: u32,
    query: SearchQuery,
) -> CmdResult<()> {
    let stream = lock(&state.searches)
        .get(&stream_id)
        .cloned()
        .ok_or_else(|| CommandError::not_found("search stream closed"))?;
    let targets: Vec<Target> = lock(&state.sessions)
        .iter()
        .map(|(id, s)| Target {
            volume_id: id.clone(),
            slot: s.data.clone(),
        })
        .collect();
    stream.query(seq, query, targets, state.engine.get());
    Ok(())
}

/// Cancels and releases a stream.
#[tauri::command]
pub fn search_close(state: State<'_, Arc<AppState>>, stream_id: u32) {
    if let Some(s) = lock(&state.searches).remove(&stream_id) {
        s.cancel();
    }
}
