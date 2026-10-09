//! `layout_open` / `layout_request` / `layout_close`.

use std::sync::Arc;

use serde::Serialize;
use tauri::State;
use tauri::ipc::{Channel, InvokeResponseBody};

use super::{blocking, with_data};
use crate::error::{CmdResult, CommandError};
use crate::layout_pipe::{LayoutRequest, LayoutStream};
use crate::state::{AppState, lock};

/// `{ streamId }`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamHandle {
    /// Stream id.
    pub stream_id: u32,
}

/// Opens a layout stream whose frames go to `on_frame`.
#[tauri::command]
pub fn layout_open(
    state: State<'_, Arc<AppState>>,
    on_frame: Channel<InvokeResponseBody>,
) -> StreamHandle {
    let id = state.stream_id();
    let stream = LayoutStream::new(Box::new(move |bytes| {
        on_frame
            .send(InvokeResponseBody::Raw(bytes))
            .map_err(|e| e.to_string())
    }));
    lock(&state.layouts).insert(id, Arc::new(stream));
    StreamHandle { stream_id: id }
}

/// Lays out and sends the frame for `seq`; resolves after it was sent, or
/// at once when a newer request superseded it.
///
/// # Errors
///
/// Unknown stream or volume, bad viewport, stale root id.
#[tauri::command]
pub async fn layout_request(
    state: State<'_, Arc<AppState>>,
    stream_id: u32,
    seq: u32,
    request: LayoutRequest,
) -> CmdResult<()> {
    let st = state.inner().clone();
    let stream = lock(&st.layouts)
        .get(&stream_id)
        .cloned()
        .ok_or_else(|| CommandError::not_found("layout stream closed"))?;
    stream.announce(seq);
    blocking(move || {
        with_data(&st, &request.volume_id, |data| {
            stream.serve(data, &request, seq).map(|_| ())
        })
    })
    .await
}

/// Releases a stream.
#[tauri::command]
pub fn layout_close(state: State<'_, Arc<AppState>>, stream_id: u32) {
    lock(&state.layouts).remove(&stream_id);
}
