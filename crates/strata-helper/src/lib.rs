//! The elevated Strata helper and the app's client for it.
//!
//! The unelevated app never opens raw volumes or deletes with elevated
//! rights itself; it asks this helper over a named pipe secured by
//! `strata-ipc`. The binary (`strata-helper.exe`) is a thin wrapper over
//! this library so the request loop can be tested in-process.
//!
//! Responsibilities:
//! - [`args`]: command-line parsing for the launch modes (on-demand,
//!   service, install/uninstall).
//! - [`server`]: the per-pipe accept loop and per-connection request loop
//!   (reader thread plus worker threads, cancellation, idle and parent-death
//!   exit).
//! - [`ops`]: request handlers (volumes, MFT scan streaming, USN journal,
//!   record re-reads, validated privileged deletes).
//! - [`source`]: volume id validation and raw-volume / image readers.
//! - [`verify`]: client verification policies layered on
//!   `strata_ipc::security::TrustPolicy`.
//! - [`privs`]: reference-counted privilege scopes (`SeBackupPrivilege`,
//!   `SeManageVolumePrivilege`) around the operations that need them.
//! - [`audit`]: audit records for every privileged action.
//! - [`service`]: Windows service mode, install/uninstall and the per-user
//!   service pipe names.
//! - [`client`]: [`client::HelperClient`], what the app backend calls.
//! - [`run`]: process entry points used by `main`.

#![cfg(windows)]

pub mod args;
pub mod audit;
pub mod cancel;
pub mod client;
mod diag;
pub mod error;
pub mod ops;
pub mod privs;
pub mod run;
pub mod server;
pub mod service;
pub mod source;
pub mod verify;
pub mod watch;

pub use error::HelperError;

/// File name of the helper binary, next to the app's executable.
pub const HELPER_EXE: &str = "strata-helper.exe";
