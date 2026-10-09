//! In-memory index of one volume, its live updates, queries, name search and
//! cache file.
//!
//! Responsibilities:
//! - [`Index`]: struct-of-arrays storage with `u32` [`EntryId`]s, under 64
//!   bytes per entry excluding names (layout in [`index`](crate::Index)).
//! - [`IndexBuilder`]: builds from [`strata_core::ScanRecord`]s arriving in any
//!   order, resolving parents, stale references, hardlinks, reparse points,
//!   orphans, cycles and NTFS metadata grouping, then aggregates bottom-up in
//!   parallel.
//! - Live updates: [`Index::upsert`], [`Index::remove`], [`Index::apply`]
//!   return a [`ChangeSet`] after O(depth) aggregate maintenance.
//! - Queries: sorted child pages, paths, top-N, filters and breakdowns
//!   ([`query`] module items re-exported here).
//! - [`search`]: query language and parallel streaming name search.
//! - Cache file: [`Index::save`], [`Index::load`] with per-section checksums.
//!
//! The crate is pure (no I/O besides the cache file) and forbids `unsafe`.

#![forbid(unsafe_code)]

mod agg;
mod build;
pub mod cache;
mod check;
mod compact;
mod ext;
mod fold;
#[cfg(test)]
mod harden_tests;
mod index;
mod live;
mod mem;
mod names;
pub mod query;
pub mod search;
mod wtf8;

pub use build::{BuildStats, IndexBuilder, METADATA_NODE_NAME, ORPHANS_NODE_NAME};
pub use cache::CacheError;
pub use compact::IdRemap;
pub use index::{
    Children, DirAggregate, EntryId, EntryTimes, Index, IndexOptions, MAX_ENTRIES, MemoryReport,
    VolumeInfo,
};
pub use live::{ChangeSet, Update};
pub use query::{Breakdown, ChildQuery, EntryKind, Filter, SortKey};

/// Errors from building or updating an index.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IndexError {
    /// The volume has more entries than a `u32`-id index can hold.
    #[error("too many entries for the index: {0} (limit {MAX_ENTRIES})")]
    TooManyEntries(u64),
    /// The names would exceed the 4 GiB name buffer (`u32` offsets).
    #[error("too many name bytes for the index: {0}")]
    NameStoreFull(u64),
    /// The file reference `u64::MAX` is reserved for virtual nodes.
    #[error("file reference {0:#x} is reserved")]
    ReservedFileRef(u64),
    /// The update would remove or replace the volume root.
    #[error("the volume root cannot be removed or replaced")]
    RootRemoval,
    /// The id does not name a live virtual block.
    #[error("entry {0} is not a virtual block")]
    NotVirtualBlock(u32),
}
