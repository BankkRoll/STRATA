//! Safe deletion for Strata.
//!
//! Wrong deletes are unacceptable, so every layer here assumes the layer
//! above it made a mistake:
//!
//! - [`canon`]: lexical canonicalization of every Win32 path spelling.
//! - [`never`](mod@never): the hard-coded never-delete list, independent of rule packs.
//! - [`guard`]: the never-list applied to literal, 8.3-expanded and
//!   handle-resolved forms (junctions, symlinks, mount points, volume GUIDs).
//! - [`preflight`]: TOCTOU re-verification, locks, Recycle Bin capacity.
//! - [`locks`]: Restart Manager lock detection and polite close.
//! - [`recycle`]: `IFileOperation` Recycle Bin deletes and restore.
//! - [`permanent`]: handle-based permanent delete that never follows links.
//! - [`privileged`]: requests the elevated helper accepts (by file id).
//! - [`tools`]: built-in Windows tool launchers and the uninstaller.
//! - [`apps`]: running-app awareness ("close Chrome first").
//! - [`audit`]: the write-ahead audit log contract.
//! - [`flow`]: plan, pre-flight and execute, the API the app calls.
//!
//! `unsafe` is confined to the `win` module.

pub mod apps;
pub mod audit;
pub mod canon;
pub mod consent;
pub mod error;
pub mod expect;
pub mod flow;
pub mod guard;
pub mod locks;
pub mod never;
pub mod permanent;
pub mod preflight;
pub mod privileged;
pub mod recycle;
pub mod tools;
pub mod volume;

mod win;

pub use error::{Change, CleanError};
pub use expect::{CancelToken, Expected};
pub use guard::{CheckedItem, FileIdentity, GuardConfig, ItemFacts, SafetyGuard};
