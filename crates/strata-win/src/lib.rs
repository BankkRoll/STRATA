//! The shared Win32 layer for Strata.
//!
//! Every Windows system query that more than one crate needs lives here, so
//! no other crate duplicates FFI. `unsafe` is confined to thin wrappers, each
//! block carrying a `// SAFETY:` justification, and every kernel handle is
//! owned by an RAII type.
//!
//! Responsibilities:
//! - [`volume`]: volume discovery (SPEC §5): mount points, folder mounts,
//!   filesystem, sizes, BitLocker, Dev Drive, network drives, and the
//!   scanner choice.
//! - [`watcher`]: volume hot-plug notifications.
//! - [`known`]: known folders for the current user and every other profile,
//!   populating [`strata_core::known::KnownFolders`] (SPEC §12.1).
//! - [`process`]: elevation, privileges, launching the helper through UAC,
//!   and process identity (SPEC §4).
//! - [`signature`]: Authenticode verification and signer comparison for the
//!   helper/app mutual check.
//! - [`path`]: verbatim paths, final paths, NT device → DOS paths, volume
//!   GUID paths ↔ mount points.
//! - [`last_access`]: the NTFS last-access update policy (SPEC §13).
//! - [`shadow`]: Volume Shadow Copy storage per volume (SPEC §7.5).
//! - [`sid`]: SID parsing and account lookup.

#![cfg(windows)]

mod com;
mod error;
mod handle;
pub mod known;
pub mod last_access;
pub mod path;
pub mod process;
mod registry;
pub mod shadow;
pub mod sid;
pub mod signature;
mod token;
pub mod volume;
pub mod watcher;
mod wide;

pub use error::{Result, WinError};
pub use handle::OwnedHandle;
