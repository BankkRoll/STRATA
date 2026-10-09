//! Live file-activity attribution for Strata: which process wrote, created
//! and deleted what, where.
//!
//! The elevated helper runs one real-time ETW session (the
//! `Microsoft-Windows-Kernel-File` and `Microsoft-Windows-Kernel-Process`
//! providers), decodes the events, maps them to (process, file) and
//! aggregates them. The app persists the results with `strata-store` and
//! feeds attribution evidence to `strata-classify`.
//!
//! Responsibilities:
//! - [`session`]: the fixed-name session, stale-session takeover, stop on
//!   drop, [`recover_orphaned_session`].
//! - [`decode`] / [`layout`] / [`tdh`]: payload layouts (built in, checked
//!   against the installed manifests via TDH) and bounds-checked decoding.
//! - [`processes`]: (pid, start time) → image, robust to pid reuse.
//! - [`files`] / [`paths`]: file object / file key → name, NT device paths →
//!   drive paths, directory interning.
//! - [`aggregate`]: now / last hour / today windows, hourly rollups shaped
//!   for `Store::record_activity`, last writers for `Store::set_last_writers`.
//! - [`evidence`]: prefix → app evidence with weights from write share.
//! - [`overhead`]: consumer CPU guard and write sampling.
//! - [`tracker`]: the FFI-free pipeline tying the above together.
//! - [`monitor`]: the helper-facing API (session + consumer thread + timer,
//!   results on a channel).
//! - [`ffi`]: every Win32 call.
//!
//! Everything stays on the machine. Clearing is one call:
//! [`Monitor::clear`] for memory, `Store::clear_activity` for disk.

pub mod aggregate;
pub mod decode;
pub mod error;
pub mod evidence;
pub mod ffi;
pub mod files;
pub mod layout;
pub mod monitor;
pub mod overhead;
pub mod paths;
pub mod processes;
pub mod session;
pub mod tdh;
pub mod tracker;

pub use aggregate::{ActivityBatch, Counts, DirTotal, Window, WriterSummary};
pub use error::EtwError;
pub use evidence::{DirActivity, Evidence, EvidenceConfig};
pub use monitor::{Monitor, MonitorConfig, MonitorMessage, StopReason};
pub use overhead::{OverheadConfig, OverheadReport};
pub use session::{SESSION_NAME, recover_orphaned_session};
pub use tracker::{Tracker, TrackerStats};

use windows::core::GUID;

/// `Microsoft-Windows-Kernel-File` provider id.
pub const KERNEL_FILE_PROVIDER: GUID = GUID::from_u128(0xedd08927_9cc4_4e65_b970_c2560fb5c289);

/// `Microsoft-Windows-Kernel-Process` provider id.
pub const KERNEL_PROCESS_PROVIDER: GUID = GUID::from_u128(0x22fb2cd6_0e7b_422b_a0c7_2fad1fd0e716);
