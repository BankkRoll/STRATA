use std::time::Duration;

/// How each directory is listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ListingMethod {
    /// `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` on a directory
    /// handle with a 64 KiB buffer. Reports the file id, allocation size and
    /// reparse tag per entry, so most records need no per-file open. Falls
    /// back to `FileIdBothDirectoryInfo` and then `FileFullDirectoryInfo` on
    /// filesystems that reject the newer class.
    #[default]
    DirectoryInfo,
    /// `FindFirstFileExW(FindExInfoBasic, FIND_FIRST_EX_LARGE_FETCH)`. No file
    /// ids or allocation sizes: allocation is estimated until the allocation
    /// pass fills it.
    FindFirstFile,
}

/// Configuration for one walk.
///
/// # Example
///
/// ```
/// use strata_walk::{ListingMethod, WalkOptions};
/// let opts = WalkOptions {
///     listing: ListingMethod::FindFirstFile,
///     allocation_pass: false,
///     ..WalkOptions::default()
/// };
/// assert!(opts.threads > 0);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkOptions {
    /// Worker threads for local volumes.
    pub threads: usize,
    /// Worker threads (and therefore in-flight requests) for network paths.
    pub network_concurrency: usize,
    /// Per-request timeout on network paths. A request is one directory
    /// listing, one allocation-pass chunk or one reparse-target read. Local
    /// paths are never timed out.
    pub timeout: Duration,
    /// Open every file (attributes only, never data, never recalling cloud
    /// content) to read exact allocation, hardlink count, compressed size and
    /// alternate data streams. When off, allocation comes from the listing
    /// (exact for [`ListingMethod::DirectoryInfo`], estimated otherwise),
    /// streams are not reported and hardlinks are not merged.
    pub allocation_pass: bool,
    /// Directory listing method.
    pub listing: ListingMethod,
    /// Target number of records per sink batch.
    pub batch_size: usize,
    /// Minimum interval between progress callbacks.
    pub progress_interval: Duration,
    /// Directories this many levels below the root are reported but not
    /// listed (`Some(1)`: the root's direct entries only). `None` walks the
    /// whole tree. Used to rescan one folder after a change notification.
    pub max_depth: Option<u32>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(4, usize::from);
        Self {
            // PERF: listing is syscall-bound, not CPU-bound; oversubscribing
            // keeps the filesystem queue busy while threads wait in the kernel.
            threads: (cpus * 2).clamp(4, 64),
            network_concurrency: 8,
            timeout: Duration::from_secs(30),
            allocation_pass: true,
            listing: ListingMethod::default(),
            batch_size: 4096,
            progress_interval: Duration::from_millis(100),
            max_depth: None,
        }
    }
}
