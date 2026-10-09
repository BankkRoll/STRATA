//! The allocation pass: one attribute-only open per file.
//!
//! Each file is opened with `FILE_READ_ATTRIBUTES | SYNCHRONIZE` and
//! `FILE_OPEN_REPARSE_POINT | FILE_OPEN_FOR_BACKUP_INTENT | FILE_OPEN_NO_RECALL`
//! (relative to the already-open directory handle when there is one), and
//! the handle is queried for:
//! - `FileStandardInfo`: allocation, end of file, hardlink count;
//! - `FileCompressionInfo`: real allocation of compressed, sparse and WOF
//!   files (the handle-based form of `GetCompressedFileSizeW`);
//! - `FileIdInfo`: the file id, when the listing did not report one;
//! - `FileStreamInfo`: alternate data streams with their allocation.
//!
//! Content that is not local (`RECALL_ON_*`, `OFFLINE`) only gets the
//! standard-info query: no stream or compression queries are sent to the
//! cloud filter, and nothing ever requests data access.

use std::io;
use std::os::windows::io::OwnedHandle;

use strata_core::{EntryFlags, ReparseKind, ScanRecord, win32};

use crate::CancelToken;
use crate::parse::{StreamEntry, parse_streams};
use crate::record::{BuildCtx, RECALL_BITS, apply_streams};
use crate::sys::{self, AlignedBuf, OpenMode};

/// What to probe for one file.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Query<'a> {
    /// Name relative to the directory handle, or an absolute NT path when
    /// there is no handle.
    pub name: &'a [u16],
    pub attributes: u32,
    pub kind: ReparseKind,
    /// Whether to query the file id (the listing did not provide one).
    pub want_id: bool,
}

/// Facts read from one file handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileFacts {
    pub logical: u64,
    pub allocated: u64,
    pub links: u32,
    pub id: Option<u128>,
    pub streams: Vec<StreamEntry>,
}

/// Probes one file.
pub(crate) fn probe(
    dir: Option<&OwnedHandle>,
    q: &Query<'_>,
    buf: &mut AlignedBuf,
) -> io::Result<FileFacts> {
    let h = sys::nt_open(dir, q.name, OpenMode::Attributes, false)?;
    let std = sys::standard_info(&h)?;
    let remote = q.attributes & RECALL_BITS != 0;
    let mut allocated = std.allocation;
    let packed =
        q.attributes & (win32::FILE_ATTRIBUTE_COMPRESSED | win32::FILE_ATTRIBUTE_SPARSE_FILE) != 0
            || q.kind == ReparseKind::Wof;
    if packed
        && !remote
        && let Ok(c) = sys::compressed_size(&h)
    {
        allocated = c;
    }
    let id = if q.want_id {
        sys::file_id(&h).ok()
    } else {
        None
    };
    let streams = if remote {
        Vec::new()
    } else {
        sys::stream_info(&h, buf)
            .map(parse_streams)
            .unwrap_or_default()
    };
    Ok(FileFacts {
        logical: std.end_of_file,
        allocated,
        links: std.links,
        id,
        streams,
    })
}

/// Probes a batch. Entries after cancellation are `None`.
pub(crate) fn probe_all(
    dir: Option<&OwnedHandle>,
    queries: &[Query<'_>],
    cancel: &CancelToken,
) -> Vec<Option<io::Result<FileFacts>>> {
    let mut buf = AlignedBuf::new(4096);
    let mut out = Vec::with_capacity(queries.len());
    for (i, q) in queries.iter().enumerate() {
        if i % 64 == 0 && cancel.is_cancelled() {
            out.resize_with(queries.len(), || None);
            break;
        }
        out.push(Some(probe(dir, q, &mut buf)));
    }
    out
}

/// Applies probe results to a record.
pub(crate) fn apply(rec: &mut ScanRecord, facts: FileFacts, cx: BuildCtx) {
    rec.sizes.logical = facts.logical;
    rec.sizes.allocated = cx.on_disk(facts.allocated);
    rec.flags.set(EntryFlags::ALLOC_ESTIMATED, false);
    apply_streams(rec, facts.streams, cx);
}
