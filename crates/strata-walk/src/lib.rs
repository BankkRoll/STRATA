//! Strata's fallback walker: a parallel, unelevated directory traversal that
//! emits the same [`strata_core::ScanRecord`]s as the MFT scanner.
//!
//! Used for everything the MFT scanner cannot read: NTFS without elevation,
//! ReFS and Dev Drive, FAT/exFAT, and network shares.
//!
//! Responsibilities:
//! - Work-stealing traversal ([`Walker`]) with per-directory listing through
//!   [`ListingMethod::DirectoryInfo`] (default) or
//!   [`ListingMethod::FindFirstFile`].
//! - `\\?\` paths everywhere; names stay raw UTF-16.
//! - Reparse points are never followed; symlink, junction and mount-point
//!   targets are read for display.
//! - The allocation pass: one attribute-only, never-recalling open per file
//!   for exact allocation, compressed/sparse/WOF size, hardlink count and
//!   alternate data streams.
//! - Cloud placeholders: seen undisguised (real tag, recall and offline
//!   bits), never hydrated, and online-only directories are recorded
//!   `PARTIAL` instead of listed, because listing them makes the provider
//!   fetch their contents.
//! - Hardlink merging into one record per file id with all links found.
//! - Access-denied, vanished and replaced entries handled without failing.
//! - Network paths: bounded concurrency, per-request timeouts with
//!   `CancelSynchronousIo`, cancellation.
//! - Cancellation at directory, listing-buffer and file granularity, with
//!   `PARTIAL` flags on incomplete directories.
//!
//! # Identity
//!
//! On NTFS and ReFS (local), record ids are the filesystem's 64-bit file ids,
//! so they match the MFT scanner's references. Elsewhere, and for anything
//! whose id was never read, ids are synthetic ([`strata_core::FileRef::SYNTHETIC_BIT`]).
//! The root record links to itself and is named with the root's display path.
//!
//! # Hardlinks
//!
//! With the allocation pass on, a file with `NumberOfLinks > 1` is held back
//! until every link has been met, then emitted once with all of its links;
//! files with links outside the walked tree are emitted at the end of the
//! walk with the links that were found. With the pass off, link counts are
//! unknown: the first sighting keeps the real id and later sightings get a
//! synthetic id and [`strata_core::EntryFlags::HARDLINK_SECONDARY`].
//!
//! # Example
//!
//! ```no_run
//! use strata_walk::{CancelToken, FnSink, WalkOptions, Walker};
//!
//! let walker = Walker::new(r"D:\projects", WalkOptions::default())?;
//! let mut files = 0usize;
//! let mut sink = FnSink::new(|batch: Vec<strata_core::ScanRecord>| {
//!     files += batch.iter().filter(|r| !r.is_dir()).count();
//! });
//! let stats = walker.run(&mut sink, &CancelToken::new())?;
//! assert_eq!(stats.totals.files as usize, files);
//! # Ok::<(), strata_walk::WalkError>(())
//! ```

mod alloc;
mod cancel;
mod hardlink;
mod options;
mod parse;
mod path;
mod record;
mod sink;
mod stats;
mod sys;
mod timed;
mod walker;

#[cfg(test)]
mod harden_tests;
#[cfg(test)]
mod tests;

pub use cancel::CancelToken;
pub use options::{ListingMethod, WalkOptions};
pub use sink::{ChannelSink, FnSink, WalkEvent, WalkSink};
pub use stats::{ErrorCounts, ErrorKind, Progress, VolumeStats, WalkStats};
pub use walker::{WalkError, Walker};
