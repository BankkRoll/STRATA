//! The parallel traversal.
//!
//! Each directory is one rayon task. Rayon gives every worker its own deque
//! and steals from the others when idle, so a directory's subdirectories are
//! explored depth-first by the thread that found them until another thread
//! runs dry. A task:
//! 1. opens and lists its directory (one blocking request);
//! 2. emits its own record and any leaf directories (reparse points);
//! 3. spawns a task per traversable subdirectory;
//! 4. runs the allocation pass over its files in chunks (large directories
//!    are split into parallel chunk tasks) and emits them.
//!
//! Records flow through a bounded channel to the thread that called
//! [`Walker::run`], which batches them for the sink and reports progress.

use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{RecvTimeoutError, Sender, bounded};
use strata_core::{
    EntryFlags, FileRef, FileTime, NameLink, Reparse, ReparseKind, ScanRecord, Sizes, Times,
    WideName, win32,
};

use crate::alloc::{self, Query};
use crate::hardlink::{HardlinkTable, SeenIds};
use crate::parse::{DirInfoClass, RawEntry, StreamEntry, parse_reparse_target, parse_streams};
use crate::path;
use crate::record::{self, BuildCtx, RECALL_BITS};
use crate::stats::AtomicErrorCounts;
use crate::sys::{self, AlignedBuf, DirChunk, OpenMode};
use crate::timed;
use crate::{
    CancelToken, ErrorKind, ListingMethod, Progress, VolumeStats, WalkOptions, WalkSink, WalkStats,
};

/// Files per allocation-pass chunk on local volumes.
const CHUNK: usize = 1024;
/// Files per allocation-pass chunk on network paths, so one timed request
/// stays well inside the timeout.
const NETWORK_CHUNK: usize = 64;
/// Directory listing buffer. 64 KiB is also the SMB2 maximum per request.
const LIST_BUF: usize = 64 * 1024;
/// Pending sink batches before workers block.
const CHANNEL_DEPTH: usize = 1024;
/// Longest a record waits in the collector before the sink sees it.
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Error starting a walk. Errors inside the tree never fail the walk; they
/// are flagged on records and counted in [`WalkStats::errors`].
#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    /// The root does not exist.
    #[error("walk root not found: {0}")]
    RootNotFound(String),
    /// The root exists but is a file.
    #[error("walk root is not a directory: {0}")]
    NotADirectory(String),
    /// The root path could not be made absolute.
    #[error("invalid walk root {path}: {source}")]
    InvalidRoot {
        /// The path as given.
        path: String,
        /// Underlying error.
        source: io::Error,
    },
    /// The worker pool could not be created.
    #[error("failed to start walker threads: {0}")]
    Threads(String),
}

/// A configured walk of one directory tree.
///
/// # Example
///
/// ```no_run
/// use strata_walk::{CancelToken, WalkOptions, Walker};
///
/// let walker = Walker::new(r"C:\Users", WalkOptions::default())?;
/// let mut records = Vec::new();
/// let stats = walker.run(&mut records, &CancelToken::new())?;
/// println!("{} files, {} bytes", stats.totals.files, stats.totals.allocated_bytes);
/// # Ok::<(), strata_walk::WalkError>(())
/// ```
#[derive(Debug, Clone)]
pub struct Walker {
    root: Vec<u16>,
    opts: WalkOptions,
    #[cfg(test)]
    pub(crate) hooks: Hooks,
}

/// Callback receiving a directory's extended path.
#[cfg(test)]
pub(crate) type PathHook = Arc<dyn Fn(&[u16]) + Send + Sync>;

/// Test seams called at fixed points of a walk so tests can change the tree
/// deterministically between a directory being listed and being used.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Hooks {
    /// Called with a directory's path just before it is listed.
    pub before_list: Option<PathHook>,
    /// Called with a directory's path just before its files are probed.
    pub before_refine: Option<PathHook>,
    /// Replaces [`sys::STREAM_INFO_MAX`] for file probes.
    pub stream_info_cap: Option<usize>,
}

#[cfg(test)]
impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks").finish_non_exhaustive()
    }
}

impl Walker {
    /// Prepares a walk of `root`. Relative paths are resolved against the
    /// current directory; the result is converted to `\\?\` form. Nothing is
    /// opened until [`Walker::run`].
    ///
    /// # Errors
    ///
    /// [`WalkError::InvalidRoot`] if the path cannot be made absolute.
    pub fn new(root: impl AsRef<Path>, opts: WalkOptions) -> Result<Self, WalkError> {
        use std::os::windows::ffi::OsStrExt;
        let given = root.as_ref();
        let abs = std::path::absolute(given).map_err(|source| WalkError::InvalidRoot {
            path: given.display().to_string(),
            source,
        })?;
        let wide: Vec<u16> = abs.as_os_str().encode_wide().collect();
        Ok(Self {
            root: path::to_extended(&wide),
            opts,
            #[cfg(test)]
            hooks: Hooks::default(),
        })
    }

    /// The root in display form (no `\\?\` prefix).
    #[must_use]
    pub fn root_display(&self) -> String {
        String::from_utf16_lossy(&path::display(&self.root))
    }

    /// The options this walker was created with.
    #[must_use]
    pub const fn options(&self) -> &WalkOptions {
        &self.opts
    }

    /// Walks the tree, streaming records to `sink` on the calling thread.
    ///
    /// Returns when the walk finishes or, after `cancel` fires, as soon as
    /// in-flight requests return. Every record produced before cancellation
    /// is delivered; directories whose contents are incomplete carry
    /// [`EntryFlags::PARTIAL`].
    ///
    /// # Errors
    ///
    /// Only for problems with the root itself or thread creation.
    ///
    /// # Panics
    ///
    /// A panic in sink cancels cancel, waits for the workers to stop and
    /// then propagates.
    pub fn run<S: WalkSink + ?Sized>(
        &self,
        sink: &mut S,
        cancel: &CancelToken,
    ) -> Result<WalkStats, WalkError> {
        let started = Instant::now();
        let display = self.root_display();
        let volume = sys::volume_facts(&self.root).ok();
        let network = path::is_unc(&self.root) || volume.as_ref().is_some_and(|v| v.is_remote);
        let fs = volume.as_ref().map(|v| v.filesystem.to_ascii_uppercase());
        let real_ids = trusts_file_ids(network, fs.as_deref());

        let root_task = self.root_task(&display)?;

        let (tx, rx) = bounded::<Msg>(CHANNEL_DEPTH);
        let ctx = Ctx {
            opts: self.opts.clone(),
            cancel: cancel.clone(),
            tx,
            next_synthetic: AtomicU64::new(1),
            real_ids,
            network,
            build: BuildCtx {
                cluster: volume.as_ref().map_or(4096, |v| v.cluster_size.max(1)),
                now: now_filetime(),
                ntfs: fs.as_deref() == Some("NTFS"),
            },
            dir_class: AtomicU8::new(DirInfoClass::IdExtd as u8),
            dir_fallback: AtomicBool::new(false),
            hardlinks: HardlinkTable::new(),
            // NOTE: one id per record seen, so this grows with the walk;
            // real ids are only unique per record if every one is checked.
            seen: real_ids.then(SeenIds::new),
            errors: AtomicErrorCounts::default(),
            access_denied_dirs: AtomicU64::new(0),
            partial_dirs: AtomicU64::new(0),
            partial_files: AtomicU64::new(0),
            #[cfg(not(test))]
            stream_cap: sys::STREAM_INFO_MAX,
            #[cfg(test)]
            stream_cap: self.hooks.stream_info_cap.unwrap_or(sys::STREAM_INFO_MAX),
            #[cfg(test)]
            hooks: self.hooks.clone(),
        };
        let threads = if network {
            self.opts.network_concurrency.min(self.opts.threads)
        } else {
            self.opts.threads
        }
        .max(1);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("strata-walk-{i}"))
            .start_handler(|_| std::mem::forget(sys::expose_placeholders()))
            .build()
            .map_err(|e| WalkError::Threads(e.to_string()))?;

        let mut collector = Collector::new(&self.opts, started);
        std::thread::scope(|s| {
            let ctx = &ctx;
            s.spawn(move || {
                pool.scope(|sc| sc.spawn(move |sc| process_dir(sc, ctx, root_task)));
                let rest = ctx.hardlinks.drain();
                if !rest.is_empty() {
                    ctx.emit(rest);
                }
                let _ = ctx.tx.send(Msg::Done);
            });
            collector.drive(rx, sink, &ctx.errors);
        });

        let errors = ctx.errors.snapshot();
        let access_denied_dirs = ctx.access_denied_dirs.load(Ordering::Relaxed);
        let partial_dirs = ctx.partial_dirs.load(Ordering::Relaxed);
        let partial_files = ctx.partial_files.load(Ordering::Relaxed);
        let cancelled = cancel.is_cancelled();
        Ok(WalkStats {
            totals: collector.progress,
            listing: self.opts.listing,
            dir_info_fallback: ctx.dir_fallback.load(Ordering::Relaxed),
            access_denied_dirs,
            partial_dirs,
            partial_files,
            estimated_allocations: collector.estimated,
            hardlinks_merged: ctx.hardlinks.merged(),
            errors,
            cancelled,
            partial: cancelled || partial_dirs > 0 || access_denied_dirs > 0 || partial_files > 0,
            volume: volume.map(|v| VolumeStats {
                mount: String::from_utf16_lossy(&path::display(&v.mount)),
                filesystem: v.filesystem,
                cluster_size: v.cluster_size,
                total_bytes: v.total,
                free_bytes: v.free,
                is_network: network,
            }),
        })
    }

    fn root_task(&self, display: &str) -> Result<DirTask, WalkError> {
        let nt = path::to_nt(&self.root);
        let _exposed = sys::expose_placeholders();
        // NOTE: the root is the one path we follow if it is a link: the user
        // chose it explicitly.
        let (times, attributes) = match sys::nt_open(None, &nt, OpenMode::Attributes, true) {
            Ok(h) => {
                sys::basic_info(&h).unwrap_or((Times::default(), win32::FILE_ATTRIBUTE_DIRECTORY))
            }
            Err(e) => match ErrorKind::classify(&e) {
                ErrorKind::Vanished => return Err(WalkError::RootNotFound(display.to_owned())),
                // Listing will report the real problem (and flag the root).
                _ => (Times::default(), win32::FILE_ATTRIBUTE_DIRECTORY),
            },
        };
        if attributes & win32::FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(WalkError::NotADirectory(display.to_owned()));
        }
        Ok(DirTask {
            path: self.root.clone().into(),
            parent: None,
            name: WideName::from_str_lossless(display),
            // The followed root is reported as a plain directory.
            attributes: attributes & !win32::FILE_ATTRIBUTE_REPARSE_POINT,
            reparse_tag: 0,
            times,
            listed_id: None,
            follow: true,
        })
    }
}

/// Whether filesystem file ids can serve as record ids on a volume.
///
/// Only local NTFS and ReFS ids are stable, unique and shared by all links
/// of a file. FAT and exFAT ids encode the directory-entry position (they
/// change on rename and can be reused), and SMB servers may synthesise
/// them, so those volumes get synthetic ids. `filesystem` is the
/// upper-cased name from `GetVolumeInformationW`, or `None` when it could
/// not be read.
pub(crate) fn trusts_file_ids(network: bool, filesystem: Option<&str>) -> bool {
    !network && matches!(filesystem, Some("NTFS" | "REFS"))
}

fn now_filetime() -> FileTime {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    FileTime::from_unix_secs(i64::try_from(secs).unwrap_or(i64::MAX))
}

// -----------------------------------------------------------------------------
// Shared walk state
// -----------------------------------------------------------------------------

enum Msg {
    Records(Vec<ScanRecord>),
    Done,
}

struct Ctx {
    opts: WalkOptions,
    cancel: CancelToken,
    tx: Sender<Msg>,
    next_synthetic: AtomicU64,
    real_ids: bool,
    network: bool,
    build: BuildCtx,
    /// Directory information class in use (a `DirInfoClass`
    /// discriminant); downgraded once if the filesystem rejects it.
    dir_class: AtomicU8,
    dir_fallback: AtomicBool,
    hardlinks: HardlinkTable,
    seen: Option<SeenIds>,
    errors: AtomicErrorCounts,
    access_denied_dirs: AtomicU64,
    partial_dirs: AtomicU64,
    partial_files: AtomicU64,
    /// Stream-list buffer cap for file probes.
    stream_cap: usize,
    #[cfg(test)]
    hooks: Hooks,
}

impl Ctx {
    fn synthetic(&self) -> FileRef {
        FileRef(FileRef::SYNTHETIC_BIT | self.next_synthetic.fetch_add(1, Ordering::Relaxed))
    }

    /// The `FileRef` for a filesystem id, if ids are trusted on this volume
    /// and it fits: 128-bit ReFS ids with a non-zero high half, or ids that
    /// would collide with the synthetic range, are not representable.
    fn real_ref(&self, id: Option<u128>) -> Option<FileRef> {
        let id = id.filter(|_| self.real_ids)?;
        let low = u64::try_from(id).ok()?;
        (low & FileRef::SYNTHETIC_BIT == 0).then_some(FileRef(low))
    }

    fn id_for(&self, id: Option<u128>) -> FileRef {
        self.real_ref(id).unwrap_or_else(|| self.synthetic())
    }

    /// Reserves a record id. Returns `false` when a record with this real id
    /// was already produced: the entry was moved between two directory
    /// listings and must not be reported (or descended into) twice.
    /// Synthetic ids are unique by construction.
    fn claim(&self, id: FileRef) -> bool {
        id.is_synthetic()
            || self
                .seen
                .as_ref()
                .is_none_or(|s| s.insert(u128::from(id.0)))
    }

    fn count(&self, e: &io::Error) -> ErrorKind {
        let k = ErrorKind::classify(e);
        self.errors.add(k);
        k
    }

    fn emit(&self, batch: Vec<ScanRecord>) {
        if !batch.is_empty() && self.tx.send(Msg::Records(batch)).is_err() {
            // The collector is gone (the sink panicked): stop the walk.
            self.cancel.cancel();
        }
    }

    fn class(&self) -> DirInfoClass {
        match self.dir_class.load(Ordering::Relaxed) {
            0 => DirInfoClass::IdExtd,
            1 => DirInfoClass::IdBoth,
            _ => DirInfoClass::Full,
        }
    }

    fn note_class(&self, used: DirInfoClass) {
        if used != DirInfoClass::IdExtd {
            self.dir_fallback.store(true, Ordering::Relaxed);
        }
        self.dir_class.fetch_max(used as u8, Ordering::Relaxed);
    }

    /// Runs a blocking filesystem request: inline on local volumes, on the
    /// worker's timed I/O thread on network paths.
    fn blocking<T, F>(&self, f: F) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> io::Result<T> + Send + 'static,
    {
        if self.network {
            timed::run_timed(self.opts.timeout, &self.cancel, f).map_err(|e| e.to_io())?
        } else {
            f()
        }
    }
}

// -----------------------------------------------------------------------------
// Directory tasks
// -----------------------------------------------------------------------------

struct DirTask {
    /// Extended (`\\?\`) path.
    path: Arc<[u16]>,
    /// Parent id; `None` for the root, which links to itself.
    parent: Option<FileRef>,
    name: WideName,
    attributes: u32,
    reparse_tag: u32,
    times: Times,
    /// Id reported by the parent's listing, used when the directory itself
    /// cannot be opened.
    listed_id: Option<u128>,
    /// Follow a reparse point at this path (root only).
    follow: bool,
}

impl DirTask {
    fn record(&self, id: FileRef, now: FileTime) -> ScanRecord {
        let flags = record::flags_for(self.attributes, self.reparse_tag, &self.times, now);
        ScanRecord {
            id,
            links: vec![NameLink {
                parent: self.parent.unwrap_or(id),
                name: self.name.clone(),
            }],
            attributes: self.attributes,
            flags,
            times: self.times,
            fn_created: None,
            sizes: Sizes::default(),
            reparse: (self.attributes & win32::FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(
                Reparse {
                    tag: self.reparse_tag,
                    target: None,
                },
            ),
            ads: Vec::new(),
        }
    }
}

/// Everything learned from listing one directory.
pub(crate) struct Listing {
    /// Directory handle, kept open for relative opens in the allocation pass.
    pub handle: Option<Arc<OwnedHandle>>,
    /// The directory's own file id.
    pub own_id: Option<u128>,
    /// Allocation of the directory index (`$INDEX_ALLOCATION`).
    pub overhead: u64,
    /// Named streams on the directory itself.
    pub streams: Vec<StreamEntry>,
    /// Why the directory's stream list could not be read.
    pub streams_error: Option<io::Error>,
    pub entries: Vec<RawEntry>,
    /// False when cancellation cut the listing short.
    pub complete: bool,
    /// Information class actually used (after any fallback).
    pub class: DirInfoClass,
}

/// Lists one directory (extended path). `class` is the information class to
/// try first; `want_handle` keeps a handle for `FindFirstFile` listings too
/// (the allocation pass needs the directory's id, index size and streams).
pub(crate) fn list_dir(
    path: &[u16],
    method: ListingMethod,
    class: DirInfoClass,
    want_handle: bool,
    follow: bool,
    cancel: &CancelToken,
) -> io::Result<Listing> {
    let nt = path::to_nt(path);
    let mut handle = if method == ListingMethod::DirectoryInfo || want_handle {
        match sys::nt_open(None, &nt, OpenMode::ListDir, follow) {
            Ok(h) => Some(h),
            Err(e) if method == ListingMethod::DirectoryInfo => return Err(e),
            Err(e) if ErrorKind::classify(&e) == ErrorKind::ReplacedByFile => return Err(e),
            // FindFirstFileExW may still succeed (it needs only list access).
            Err(_) => None,
        }
    } else {
        None
    };
    let mut l = Listing {
        own_id: handle.as_ref().and_then(|h| sys::file_id(h).ok()),
        overhead: handle
            .as_ref()
            .and_then(|h| sys::standard_info(h).ok())
            .map_or(0, |s| s.allocation),
        streams: Vec::new(),
        streams_error: None,
        entries: Vec::new(),
        complete: true,
        class,
        handle: None,
    };
    if want_handle && let Some(h) = &handle {
        let mut buf = AlignedBuf::new(1024);
        match sys::stream_info(h, &mut buf) {
            Ok(b) => l.streams = parse_streams(b),
            Err(e) => l.streams_error = Some(e),
        }
    }
    match (method, &handle) {
        (ListingMethod::DirectoryInfo, Some(h)) => query_all(h, &mut l, cancel)?,
        _ => match sys::find_list(path, cancel, &mut l.entries) {
            Ok(complete) => l.complete = complete,
            // NOTE: FindFirstFileExW needs `<dir>\*`, which can exceed the NT
            // path limit when the directory itself still fits; Win32 then
            // reports PATH_NOT_FOUND, as if the directory had vanished. A
            // failed find is retried through a directory handle, and the
            // original error stands only if that cannot be opened either.
            Err(e) => {
                let h = match handle.take() {
                    Some(h) => h,
                    None => match sys::nt_open(None, &nt, OpenMode::ListDir, follow) {
                        Ok(h) => h,
                        Err(_) => return Err(e),
                    },
                };
                l.entries.clear();
                l.complete = true;
                query_all(&h, &mut l, cancel)?;
                handle = Some(h);
            }
        },
    }
    l.handle = handle.map(Arc::new);
    Ok(l)
}

/// Reads every directory-information buffer of `h` into `l`, downgrading
/// the information class once if the filesystem rejects it.
fn query_all(h: &OwnedHandle, l: &mut Listing, cancel: &CancelToken) -> io::Result<()> {
    let mut buf = AlignedBuf::new(LIST_BUF);
    let mut first = true;
    loop {
        if cancel.is_cancelled() {
            l.complete = false;
            return Ok(());
        }
        match sys::query_dir(h, l.class, &mut buf)? {
            DirChunk::Data(n) => {
                crate::parse::parse_dir_info(buf.bytes(n), l.class, &mut l.entries);
            }
            DirChunk::End => return Ok(()),
            DirChunk::Unsupported if first && l.class != DirInfoClass::Full => {
                l.class = match l.class {
                    DirInfoClass::IdExtd => DirInfoClass::IdBoth,
                    _ => DirInfoClass::Full,
                };
                continue;
            }
            DirChunk::Unsupported => return Err(io::Error::from_raw_os_error(50)),
        }
        first = false;
    }
}

fn process_dir<'s>(scope: &rayon::Scope<'s>, ctx: &'s Ctx, task: DirTask) {
    // Children with recall bits are never spawned; this catches a root that
    // is itself an online-only cloud directory.
    if ctx.cancel.is_cancelled() || task.attributes & RECALL_BITS != 0 {
        let id = ctx.id_for(task.listed_id);
        if !ctx.claim(id) {
            return;
        }
        let mut rec = task.record(id, ctx.build.now);
        rec.flags |= EntryFlags::PARTIAL;
        ctx.partial_dirs.fetch_add(1, Ordering::Relaxed);
        ctx.emit(vec![rec]);
        return;
    }

    #[cfg(test)]
    if let Some(h) = &ctx.hooks.before_list {
        h(&task.path);
    }
    let listing = {
        let p = Arc::clone(&task.path);
        let (method, class, follow) = (ctx.opts.listing, ctx.class(), task.follow);
        let want_handle = ctx.opts.allocation_pass;
        let cancel = ctx.cancel.clone();
        ctx.blocking(move || list_dir(&p, method, class, want_handle, follow, &cancel))
    };
    let listing = match listing {
        Ok(l) => l,
        Err(e) => {
            let id = ctx.id_for(task.listed_id);
            let mut rec = task.record(id, ctx.build.now);
            match ctx.count(&e) {
                ErrorKind::AccessDenied => {
                    rec.flags |= EntryFlags::ACCESS_DENIED;
                    ctx.access_denied_dirs.fetch_add(1, Ordering::Relaxed);
                }
                ErrorKind::Vanished => return,
                ErrorKind::ReplacedByFile => return replaced_by_file(ctx, &task),
                _ => {
                    rec.flags |= EntryFlags::PARTIAL;
                    ctx.partial_dirs.fetch_add(1, Ordering::Relaxed);
                }
            }
            if ctx.claim(id) {
                ctx.emit(vec![rec]);
            }
            return;
        }
    };
    ctx.note_class(listing.class);

    let own_id = ctx
        .real_ref(listing.own_id)
        .unwrap_or_else(|| ctx.id_for(task.listed_id));
    if !ctx.claim(own_id) {
        // Moved during the walk and already walked at its other path.
        return;
    }
    let mut own = task.record(own_id, ctx.build.now);
    own.sizes.dir_overhead = ctx.build.on_disk(listing.overhead);
    record::apply_streams(&mut own, listing.streams, ctx.build);
    if let Some(e) = &listing.streams_error {
        ctx.count(e);
    }
    if !listing.complete || listing.streams_error.is_some() {
        own.flags |= EntryFlags::PARTIAL;
        ctx.partial_dirs.fetch_add(1, Ordering::Relaxed);
    }

    let mut out = vec![own];
    let mut files: Vec<(ScanRecord, Option<u128>)> = Vec::new();
    let mut targets: Vec<usize> = Vec::new();
    for e in listing.entries {
        let kind = record::reparse_kind(e.attributes, e.reparse_tag);
        let wants_target = matches!(kind, ReparseKind::Symlink | ReparseKind::MountPoint);
        if e.is_dir() && !kind.blocks_traversal() {
            if e.attributes & RECALL_BITS != 0 {
                // NOTE: listing a not-yet-populated cloud directory makes the
                // provider fetch its contents. cfapi marks such directories
                // RECALL_ON_DATA_ACCESS (or RECALL_ON_OPEN), and the open
                // flags cannot prevent it. Record it, flagged partial,
                // without descending.
                let id = ctx.id_for(e.file_id);
                if ctx.claim(id) {
                    let mut rec = record::build(e, id, own_id, ctx.build);
                    rec.flags |= EntryFlags::PARTIAL;
                    ctx.partial_dirs.fetch_add(1, Ordering::Relaxed);
                    out.push(rec);
                }
                continue;
            }
            let child = DirTask {
                path: path::join(&task.path, &e.name).into(),
                parent: Some(own_id),
                name: WideName::from_units(e.name),
                attributes: e.attributes,
                reparse_tag: e.reparse_tag,
                times: e.times,
                listed_id: e.file_id,
                follow: false,
            };
            scope.spawn(move |s| process_dir(s, ctx, child));
        } else if e.is_dir() {
            let id = ctx.id_for(e.file_id);
            if !ctx.claim(id) {
                continue;
            }
            if wants_target {
                targets.push(out.len());
            }
            out.push(record::build(e, id, own_id, ctx.build));
        } else {
            let key = e.file_id;
            let id = ctx.id_for(key);
            let rec = record::build(e, id, own_id, ctx.build);
            if wants_target {
                if ctx.claim(id) {
                    targets.push(out.len());
                    out.push(rec);
                }
            } else {
                files.push((rec, key));
            }
        }
    }

    if !targets.is_empty() {
        read_targets(ctx, listing.handle.as_ref(), &task.path, &mut out, &targets);
    }
    ctx.emit(out);

    if files.is_empty() {
        return;
    }
    if !ctx.opts.allocation_pass {
        return ctx.emit(dedup_without_link_counts(ctx, files));
    }
    let chunk = if ctx.network { NETWORK_CHUNK } else { CHUNK };
    while files.len() > chunk {
        let tail = files.split_off(files.len() - chunk);
        let (h, p) = (listing.handle.clone(), Arc::clone(&task.path));
        scope.spawn(move |_| refine_chunk(ctx, h, p, tail));
    }
    refine_chunk(ctx, listing.handle, task.path, files);
}

/// A directory listed by its parent turned out to be a file when opened:
/// report what is there now.
fn replaced_by_file(ctx: &Ctx, task: &DirTask) {
    let nt = path::to_nt(&task.path);
    let probed = ctx.blocking(move || {
        let h = sys::nt_open(None, &nt, OpenMode::Attributes, false)?;
        let (times, attributes) = sys::basic_info(&h)?;
        let tag = sys::attribute_tag(&h).map_or(0, |(_, t)| t);
        let std = sys::standard_info(&h)?;
        let id = sys::file_id(&h).ok();
        Ok((times, attributes, tag, std, id))
    });
    match probed {
        Ok((times, attributes, tag, std, id)) => {
            let e = RawEntry {
                name: task.name.units().to_vec(),
                attributes,
                reparse_tag: tag,
                times,
                logical: std.end_of_file,
                allocated: Some(std.allocation),
                file_id: id,
            };
            let id = ctx.id_for(id);
            if !ctx.claim(id) {
                return;
            }
            ctx.emit(vec![record::build(
                e,
                id,
                task.parent.unwrap_or(id),
                ctx.build,
            )]);
        }
        Err(e) => {
            ctx.count(&e);
        }
    }
}

fn read_targets(
    ctx: &Ctx,
    handle: Option<&Arc<OwnedHandle>>,
    dir: &Arc<[u16]>,
    out: &mut [ScanRecord],
    idx: &[usize],
) {
    for &i in idx {
        let name: Vec<u16> = out[i].links[0].name.units().to_vec();
        let (h, d) = (handle.cloned(), Arc::clone(dir));
        let read = ctx.blocking(move || {
            let file = match &h {
                Some(h) => sys::nt_open(Some(h), &name, OpenMode::Attributes, false)?,
                None => sys::nt_open(
                    None,
                    &path::to_nt(&path::join(&d, &name)),
                    OpenMode::Attributes,
                    false,
                )?,
            };
            let mut buf = AlignedBuf::new(16 * 1024);
            let data = sys::reparse_data(&file, &mut buf)?;
            Ok(parse_reparse_target(data))
        });
        match read {
            Ok(Some((tag, target))) => {
                out[i].reparse = Some(Reparse {
                    tag,
                    target: target.map(WideName::from_units),
                });
            }
            Ok(None) => {}
            Err(e) => {
                ctx.count(&e);
            }
        }
    }
}

/// Without the allocation pass there are no link counts, so hardlinks
/// cannot be held back for merging. Ids must still be unique: the first
/// sighting keeps the real id, later ones get a synthetic id and the
/// `HARDLINK_SECONDARY` flag (their bytes are counted at the first path).
fn dedup_without_link_counts(ctx: &Ctx, files: Vec<(ScanRecord, Option<u128>)>) -> Vec<ScanRecord> {
    files
        .into_iter()
        .map(|(mut rec, _)| {
            if !ctx.claim(rec.id) {
                rec.id = ctx.synthetic();
                rec.flags |= EntryFlags::HARDLINK_SECONDARY;
            }
            rec
        })
        .collect()
}

/// Allocation pass over one chunk of a directory's files.
fn refine_chunk(
    ctx: &Ctx,
    handle: Option<Arc<OwnedHandle>>,
    dir: Arc<[u16]>,
    chunk: Vec<(ScanRecord, Option<u128>)>,
) {
    #[cfg(test)]
    if let Some(h) = &ctx.hooks.before_refine {
        h(&dir);
    }
    let want_id = |r: &ScanRecord| ctx.real_ids && r.id.is_synthetic();
    let cap = ctx.stream_cap;
    let mut chunk_failed = false;
    let results = if ctx.network {
        let owned: Vec<(Vec<u16>, u32, ReparseKind, bool)> = chunk
            .iter()
            .map(|(r, _)| {
                let name = query_name(handle.is_some(), &dir, r);
                (name, r.attributes, r.flags.reparse(), want_id(r))
            })
            .collect();
        let (h, cancel, n) = (handle.clone(), ctx.cancel.clone(), chunk.len());
        ctx.blocking(move || {
            let qs: Vec<Query<'_>> = owned
                .iter()
                .map(|(name, attributes, kind, want_id)| Query {
                    name,
                    attributes: *attributes,
                    kind: *kind,
                    want_id: *want_id,
                })
                .collect();
            Ok(alloc::probe_all(h.as_deref(), &qs, &cancel, cap))
        })
        .unwrap_or_else(|e| {
            ctx.count(&e);
            chunk_failed = !ctx.cancel.is_cancelled();
            (0..n).map(|_| None).collect()
        })
    } else {
        let names: Vec<Vec<u16>> = if handle.is_some() {
            Vec::new()
        } else {
            chunk
                .iter()
                .map(|(r, _)| query_name(false, &dir, r))
                .collect()
        };
        let qs: Vec<Query<'_>> = chunk
            .iter()
            .enumerate()
            .map(|(i, (r, _))| Query {
                name: names.get(i).map_or(r.links[0].name.units(), Vec::as_slice),
                attributes: r.attributes,
                kind: r.flags.reparse(),
                want_id: want_id(r),
            })
            .collect();
        alloc::probe_all(handle.as_deref(), &qs, &ctx.cancel, cap)
    };

    // A file the pass could not fully read keeps its listing values, but its
    // streams (and, without a listing allocation, its real size) are
    // unknown: flag it so totals are not mistaken for complete ones.
    let partial = |rec: &mut ScanRecord| {
        rec.flags |= EntryFlags::PARTIAL;
        ctx.partial_files.fetch_add(1, Ordering::Relaxed);
    };
    let mut out = Vec::with_capacity(chunk.len());
    for ((mut rec, key), res) in chunk.into_iter().zip(results) {
        match res {
            None => {
                if ctx.claim(rec.id) {
                    if chunk_failed {
                        partial(&mut rec);
                    }
                    out.push(rec);
                }
            }
            Some(Err(e)) => {
                if ctx.count(&e) != ErrorKind::Vanished && ctx.claim(rec.id) {
                    partial(&mut rec);
                    out.push(rec);
                }
            }
            Some(Ok(mut facts)) => {
                let links = facts.links;
                let key = key.or(facts.id);
                if rec.id.is_synthetic()
                    && let Some(r) = ctx.real_ref(facts.id)
                {
                    rec.id = r;
                }
                if let Some(e) = facts.streams_error.take() {
                    ctx.count(&e);
                    partial(&mut rec);
                }
                alloc::apply(&mut rec, facts, ctx.build);
                match key {
                    Some(k) if links > 1 && ctx.real_ids => {
                        if let Some(merged) = ctx.hardlinks.add(k, rec, links) {
                            out.push(merged);
                        }
                    }
                    // A single-link file met twice was moved between two
                    // directory listings; it is kept at the first path.
                    _ if !ctx.claim(rec.id) => {}
                    _ => out.push(rec),
                }
            }
        }
    }
    ctx.emit(out);
}

/// Name to open: the bare component when a directory handle is available,
/// otherwise the absolute NT path.
fn query_name(relative: bool, dir: &[u16], r: &ScanRecord) -> Vec<u16> {
    let name = r.links[0].name.units();
    if relative {
        name.to_vec()
    } else {
        path::to_nt(&path::join(dir, name))
    }
}

// -----------------------------------------------------------------------------
// Collector
// -----------------------------------------------------------------------------

struct Collector {
    batch_size: usize,
    progress_interval: Duration,
    started: Instant,
    progress: Progress,
    estimated: u64,
    pending: Vec<ScanRecord>,
}

impl Collector {
    fn new(opts: &WalkOptions, started: Instant) -> Self {
        Self {
            batch_size: opts.batch_size.max(1),
            progress_interval: opts.progress_interval,
            started,
            progress: Progress::default(),
            estimated: 0,
            pending: Vec::new(),
        }
    }

    fn account(&mut self, batch: &[ScanRecord]) {
        for r in batch {
            if r.is_dir() {
                self.progress.dirs += 1;
            } else {
                self.progress.files += 1;
            }
            if r.flags.contains(EntryFlags::ALLOC_ESTIMATED) {
                self.estimated += 1;
            }
            self.progress.logical_bytes = self
                .progress
                .logical_bytes
                .saturating_add(r.sizes.total_logical());
            self.progress.allocated_bytes = self
                .progress
                .allocated_bytes
                .saturating_add(r.sizes.total_allocated());
        }
    }

    fn flush<S: WalkSink + ?Sized>(&mut self, sink: &mut S) {
        if !self.pending.is_empty() {
            sink.records(std::mem::take(&mut self.pending));
        }
    }

    fn drive<S: WalkSink + ?Sized>(
        &mut self,
        // NOTE: taken by value so a panicking sink drops the receiver while
        // unwinding; blocked workers then see a closed channel instead of
        // deadlocking the scope join.
        rx: crossbeam_channel::Receiver<Msg>,
        sink: &mut S,
        errors: &AtomicErrorCounts,
    ) {
        let mut last_flush = Instant::now();
        let mut last_progress = Instant::now();
        loop {
            match rx.recv_timeout(FLUSH_INTERVAL) {
                Ok(Msg::Records(batch)) => {
                    self.account(&batch);
                    if self.pending.is_empty() {
                        self.pending = batch;
                    } else {
                        self.pending.extend(batch);
                    }
                }
                Ok(Msg::Done) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            if self.pending.len() >= self.batch_size || last_flush.elapsed() >= FLUSH_INTERVAL {
                self.flush(sink);
                last_flush = Instant::now();
            }
            if last_progress.elapsed() >= self.progress_interval {
                self.progress.errors = errors.snapshot().total();
                self.progress.elapsed = self.started.elapsed();
                sink.progress(&self.progress);
                last_progress = Instant::now();
            }
        }
        self.flush(sink);
        self.progress.errors = errors.snapshot().total();
        self.progress.elapsed = self.started.elapsed();
        sink.progress(&self.progress);
    }
}
