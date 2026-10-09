//! Duplicate file finder for Strata.
//!
//! Responsibilities:
//! - [`find_duplicates`]: the pipeline (size groups, handle measurement,
//!   partial xxh3, full BLAKE3), parallel, I/O-throttled, cancellable and
//!   resumable through a [`HashCache`], with [`Progress`] events and ETA.
//! - Never hydrating cloud files: candidates flagged as cloud, offline or
//!   non-content reparse points are never opened, and every open uses
//!   `FILE_FLAG_OPEN_NO_RECALL` and re-checks the attributes on the handle
//!   before reading (see the `win` module docs for the full argument).
//! - Hardlinks are one file: secondary links and repeated file references
//!   are excluded, and file ids are verified on every handle.
//! - [`suggest_keep`]: a pure, table-tested keep suggestion with a reason.
//! - [`Selection`]: a deletion selection that cannot mark every copy of a
//!   group, producing `strata-clean` [`QueueItem`](strata_clean::flow::QueueItem)s
//!   so deletes go through the cleaner's pre-flight, audit log and Recycle
//!   Bin flow. [`verify_selection`] re-checks copies (hash or bytes) first.
//! - [`hardlink`]: the optional "replace duplicates with hardlinks" action.
//!
//! `unsafe` is confined to the private `win` module.

mod cache;
mod candidate;
mod gate;
pub mod hardlink;
mod hash;
mod keep;
mod report;
mod scan;
mod select;
mod throttle;
mod verify;
mod win;

pub use cache::{CacheError, CachedHash, HashCache, HashKey, MemoryHashCache};
pub use candidate::{Candidate, VolumeKey};
pub use gate::{Exclusion, PLACEHOLDER_ATTRIBUTES, SkipReason, check_handle_attributes};
pub use hash::PARTIAL_WINDOW;
pub use keep::{
    KeepContext, KeepInput, KeepReason, KeepRule, KeepSuggestion, Preference, suggest_keep,
};
pub use report::{
    DupFile, DuplicateGroup, DuplicateReport, Phase, Progress, ScanOutcome, ScanStats, SkippedFile,
};
pub use scan::{DEFAULT_MIN_SIZE, ScanConfig, SizeGroup, find_duplicates, group_by_size};
pub use select::{
    GroupSelection, MAX_GROUP_FILES, Selection, SelectionError, SelectionRequest,
    parse_queue_item_id, queue_item_id,
};
pub use verify::{
    VerifyConfig, VerifyError, VerifyFailure, VerifyMode, VerifyProblem, verify_selection,
};
