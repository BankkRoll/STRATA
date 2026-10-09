//! Live index updates for one volume.
//!
//! Responsibilities:
//! - [`Tailer`]: tails the USN change journal through a [`JournalSource`],
//!   coalesces records per file within a tick, re-reads changed files
//!   through a [`RecordSource`] and applies them to a
//!   [`strata_index::Index`], emitting one merged
//!   [`strata_index::ChangeSet`] per tick ([`LiveEvent::Tick`]).
//! - Edge cases as typed outcomes ([`Halt`]): journal wrapped or recreated
//!   ([`RescanReason`]), journal disabled, source disconnected or volume
//!   dismounted ([`StaleReason`]), bursts ([`LiveStatus::CatchingUp`]),
//!   resume from sleep ([`Tailer::resume`]).
//! - Cache and catch-up ([`cache`]): atomic saves with the applied journal
//!   position, validation and replay on launch.
//! - Volumes without a journal ([`watch`]): a subtree watcher port, rescan
//!   planning and reconciliation of a rescan into the index.
//!
//! The crate performs no OS calls besides the cache file; the helper and
//! app implement the source traits.

#![forbid(unsafe_code)]

pub mod cache;
mod coalesce;
mod merge;
pub mod reason;
mod source;
mod status;
mod tailer;
pub mod watch;

pub use cache::{CacheFile, CachePolicy, LoadError, index_position};
pub use merge::{ChangeMerger, merge_change_sets};
pub use source::{
    Clock, Fetched, JournalInfo, JournalSource, RecordSource, SourceError, SystemClock,
};
pub use status::{Halt, JournalPosition, RescanReason, StaleReason, check_position};
pub use tailer::{IndexAccess, LiveEvent, LiveStatus, TailStats, Tailer, TailerConfig, TickReport};
pub use watch::{
    RescanPlanner, RescanTarget, SubtreeWatcher, WatchBatch, WatchChange, WatchKind,
    reconcile_subtree, resolve_relative,
};
