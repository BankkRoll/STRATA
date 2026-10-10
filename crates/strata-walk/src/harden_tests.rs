//! Adversarial end-to-end tests: cloud placeholders, cycles, path limits,
//! races, access control, extreme sizes and stream counts.
//!
//! Every fixture lives in a directory the test creates
//! (`%TEMP%\strata-walk-harden-*`, or `D:\strata-walk-harden\*` for the huge
//! sparse file) and is removed by an RAII guard even when an assertion fails.
//! Fixtures that need something the machine may lack (symlink privilege, a
//! cloud-files sync root, a second NTFS volume, WOF) skip with a printed
//! reason instead of failing.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use strata_core::{EntryFlags, FileRef, ReparseKind, ScanRecord, win32};

use crate::sys::{self, OpenMode};
use crate::{CancelToken, ErrorKind, ListingMethod, WalkOptions, WalkStats, Walker};

// -----------------------------------------------------------------------------
// Fixtures
// -----------------------------------------------------------------------------

/// A unique-enough suffix for fixture directories: process id, clock and a
/// per-process counter.
fn unique() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!(
        "{}-{nanos:08x}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// A fixture directory removed on drop, after running registered restore
/// steps (ACL resets, sync-root teardown) in reverse order.
struct Tree {
    /// Plain path, for tools such as `mklink` and `icacls`.
    plain: PathBuf,
    /// `\\?\` path, for names Win32 would otherwise normalise.
    verbatim: PathBuf,
    restore: Vec<Box<dyn FnOnce()>>,
}

impl Tree {
    /// `%TEMP%\strata-walk-harden-<tag>-<unique>`.
    fn new(tag: &str) -> Self {
        Self::at(std::env::temp_dir().join(format!("strata-walk-harden-{tag}-{}", unique())))
            .expect("create temp fixture")
    }

    /// A fixture at exactly `plain`, which must not exist yet.
    fn at(plain: PathBuf) -> std::io::Result<Self> {
        fs::create_dir(&plain)?;
        let verbatim = fs::canonicalize(&plain)?;
        let plain = PathBuf::from(
            verbatim
                .to_string_lossy()
                .strip_prefix(r"\\?\")
                .expect("verbatim")
                .to_owned(),
        );
        Ok(Self {
            plain,
            verbatim,
            restore: Vec::new(),
        })
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.verbatim.join(rel)
    }

    fn dir(&self, rel: &str) -> PathBuf {
        let p = self.p(rel);
        fs::create_dir_all(&p).expect("create dir");
        p
    }

    fn file(&self, rel: &str, len: usize) -> PathBuf {
        let p = self.p(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(&p, vec![b'x'; len]).expect("write file");
        p
    }

    fn on_drop(&mut self, f: impl FnOnce() + 'static) {
        self.restore.push(Box::new(f));
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        for r in self.restore.drain(..).rev() {
            r();
        }
        if let Err(e) = fs::remove_dir_all(&self.verbatim)
            && self.verbatim.exists()
        {
            eprintln!("warning: fixture {} not removed: {e}", self.plain.display());
        }
    }
}

fn opts(listing: ListingMethod, allocation_pass: bool) -> WalkOptions {
    WalkOptions {
        threads: 4,
        listing,
        allocation_pass,
        ..WalkOptions::default()
    }
}

const MODES: [(ListingMethod, bool); 4] = [
    (ListingMethod::DirectoryInfo, true),
    (ListingMethod::DirectoryInfo, false),
    (ListingMethod::FindFirstFile, true),
    (ListingMethod::FindFirstFile, false),
];

fn walk_with(walker: &Walker) -> (Vec<ScanRecord>, WalkStats) {
    let mut recs = Vec::new();
    let stats = walker.run(&mut recs, &CancelToken::new()).expect("walk");
    (recs, stats)
}

fn walk(root: &Path, o: WalkOptions) -> (Vec<ScanRecord>, WalkStats) {
    walk_with(&Walker::new(root, o).expect("walker"))
}

fn hooked(
    root: &Path,
    o: WalkOptions,
    hooks: crate::walker::Hooks,
) -> (Vec<ScanRecord>, WalkStats) {
    let mut walker = Walker::new(root, o).expect("walker");
    walker.hooks = hooks;
    walk_with(&walker)
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().collect()
}

/// Structural invariants every walk must satisfy, checked without panicking
/// so stress loops can report the first violation with context.
fn check_structure(recs: &[ScanRecord]) -> Result<(), String> {
    let mut ids = HashSet::new();
    for r in recs {
        if !ids.insert(r.id) {
            return Err(format!(
                "duplicate id {:?} ({})",
                r.id,
                r.links[0].name.to_string_lossy()
            ));
        }
    }
    let roots = recs
        .iter()
        .filter(|r| r.links.iter().any(|l| l.parent == r.id))
        .count();
    if roots != 1 {
        return Err(format!("{roots} self-linked roots"));
    }
    for r in recs {
        if r.links.is_empty() {
            return Err(format!("record {:?} has no links", r.id));
        }
        for l in &r.links {
            if !ids.contains(&l.parent) {
                return Err(format!("dangling parent for {}", l.name.to_string_lossy()));
            }
        }
    }
    Ok(())
}

/// Indexed view over a walk's output; construction asserts the structural
/// invariants.
struct View<'a> {
    children: HashMap<FileRef, Vec<(&'a [u16], &'a ScanRecord)>>,
    root: &'a ScanRecord,
}

impl<'a> View<'a> {
    fn new(recs: &'a [ScanRecord]) -> Self {
        if let Err(e) = check_structure(recs) {
            panic!("{e}");
        }
        let root = recs
            .iter()
            .find(|r| r.links.iter().any(|l| l.parent == r.id))
            .expect("root");
        let mut children: HashMap<FileRef, Vec<(&[u16], &ScanRecord)>> = HashMap::new();
        for r in recs {
            for l in r.links.iter().filter(|l| l.parent != r.id) {
                children
                    .entry(l.parent)
                    .or_default()
                    .push((l.name.units(), r));
            }
        }
        Self { children, root }
    }

    fn children(&self, id: FileRef) -> &[(&'a [u16], &'a ScanRecord)] {
        self.children.get(&id).map_or(&[], Vec::as_slice)
    }

    fn child(&self, parent: FileRef, name: &[u16]) -> Option<&'a ScanRecord> {
        self.children(parent)
            .iter()
            .find(|(n, _)| *n == name)
            .map(|&(_, r)| r)
    }

    fn try_get(&self, rel: &str) -> Option<&'a ScanRecord> {
        let mut cur = self.root;
        for comp in rel.split('\\').filter(|c| !c.is_empty()) {
            cur = self.child(cur.id, &w(comp))?;
        }
        Some(cur)
    }

    fn get(&self, rel: &str) -> &'a ScanRecord {
        self.try_get(rel)
            .unwrap_or_else(|| panic!("{rel} not found in walk"))
    }
}

fn run_cmd(program: &str, args: &[&std::ffi::OsStr]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn mklink_junction(link: &Path, target: &Path) -> Result<String, String> {
    run_cmd(
        "cmd",
        &[
            "/C".as_ref(),
            "mklink".as_ref(),
            "/J".as_ref(),
            link.as_os_str(),
            target.as_os_str(),
        ],
    )
}

/// Opens a fixture the way the allocation pass does: attributes only, never
/// recalling, never following reparse points.
fn handle_of(p: &Path) -> std::os::windows::io::OwnedHandle {
    sys::nt_open(
        None,
        &crate::path::to_nt(&crate::path::to_extended(&wide(p))),
        OpenMode::Attributes,
        false,
    )
    .expect("open fixture")
}

fn attributes_of(p: &Path) -> u32 {
    sys::basic_info(&handle_of(p)).expect("basic info").1
}

fn set_attributes(p: &Path, attrs: u32) -> std::io::Result<()> {
    use windows::Win32::Storage::FileSystem::{FILE_FLAGS_AND_ATTRIBUTES, SetFileAttributesW};
    let mut name = wide(p);
    name.push(0);
    // SAFETY: `name` is a NUL-terminated wide string that outlives the call.
    unsafe {
        SetFileAttributesW(
            windows::core::PCWSTR(name.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(attrs),
        )
    }
    .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
}

fn write_stream(base: &Path, stream: &str, len: usize) {
    let mut name = base.as_os_str().to_owned();
    name.push(format!(":{stream}"));
    fs::write(PathBuf::from(name), vec![b'q'; len]).expect("write stream");
}

fn current_user() -> String {
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    let user = std::env::var("USERNAME").expect("USERNAME");
    if domain.is_empty() {
        user
    } else {
        format!(r"{domain}\{user}")
    }
}

/// Adds a deny ACE for the current user on `target` and registers its
/// removal with the fixture's guard.
fn deny(t: &mut Tree, target: &Path, rights: &str) {
    let user = current_user();
    run_cmd(
        "icacls",
        &[
            target.as_os_str(),
            "/deny".as_ref(),
            format!("{user}:{rights}").as_ref(),
        ],
    )
    .expect("icacls deny");
    let target = target.to_owned();
    t.on_drop(move || {
        let _ = run_cmd(
            "icacls",
            &[target.as_os_str(), "/remove:d".as_ref(), user.as_ref()],
        );
    });
}

// -----------------------------------------------------------------------------
// Cloud placeholders
// -----------------------------------------------------------------------------

/// A throwaway cloud-files sync root whose provider counts every request and
/// fails it without supplying data.
mod cloud {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use windows::Win32::Foundation::NTSTATUS;
    use windows::Win32::Storage::CloudFilters::{
        CF_CALLBACK_INFO, CF_CALLBACK_PARAMETERS, CF_CALLBACK_REGISTRATION,
        CF_CALLBACK_TYPE_FETCH_DATA, CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS, CF_CALLBACK_TYPE_NONE,
        CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO, CF_CONNECTION_KEY, CF_CREATE_FLAG_NONE,
        CF_FS_METADATA, CF_HYDRATION_POLICY, CF_HYDRATION_POLICY_FULL, CF_OPERATION_INFO,
        CF_OPERATION_PARAMETERS, CF_OPERATION_PARAMETERS_0, CF_OPERATION_PARAMETERS_0_0,
        CF_OPERATION_PARAMETERS_0_4, CF_OPERATION_TYPE, CF_OPERATION_TYPE_TRANSFER_DATA,
        CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS,
        CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION,
        CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC, CF_PLACEHOLDER_CREATE_FLAG_NONE,
        CF_PLACEHOLDER_CREATE_FLAGS, CF_PLACEHOLDER_CREATE_INFO, CF_POPULATION_POLICY,
        CF_POPULATION_POLICY_FULL, CF_REGISTER_FLAG_DISABLE_ON_DEMAND_POPULATION_ON_ROOT,
        CF_REGISTER_FLAG_MARK_IN_SYNC_ON_ROOT, CF_SYNC_POLICIES, CF_SYNC_REGISTRATION,
        CfConnectSyncRoot, CfCreatePlaceholders, CfDisconnectSyncRoot, CfExecute,
        CfRegisterSyncRoot, CfUnregisterSyncRoot,
    };
    use windows::Win32::Storage::FileSystem::FILE_BASIC_INFO;
    use windows::core::{GUID, PCWSTR};

    /// Provider requests seen from one process.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(super) struct Requests {
        pub fetch_data: usize,
        pub fetch_placeholders: usize,
    }

    /// Requests by requesting process id. Other processes (the indexer,
    /// antivirus) may touch the fixture too, so assertions look only at the
    /// process under test.
    static REQUESTS: Mutex<Option<HashMap<u32, Requests>>> = Mutex::new(None);

    const STATUS_UNSUCCESSFUL: NTSTATUS = NTSTATUS(0xC000_0001_u32 as i32);

    fn requests() -> MutexGuard<'static, Option<HashMap<u32, Requests>>> {
        REQUESTS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Requests made so far by process `pid`.
    pub(super) fn requests_from(pid: u32) -> Requests {
        requests()
            .as_ref()
            .and_then(|m| m.get(&pid).copied())
            .unwrap_or_default()
    }

    /// Total requests from every process.
    pub(super) fn all_requests() -> usize {
        requests().as_ref().map_or(0, |m| {
            m.values()
                .map(|r| r.fetch_data + r.fetch_placeholders)
                .sum()
        })
    }

    fn wz(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    fn wpath(p: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        p.as_os_str().encode_wide().chain([0]).collect()
    }

    /// Counts the request against the requesting process.
    ///
    /// # Safety
    ///
    /// `info` must be the callback info cfapi passed to the callback.
    unsafe fn count(info: &CF_CALLBACK_INFO, data: bool) {
        // SAFETY: with CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO, ProcessInfo is
        // either null or valid for the duration of the callback.
        let pid = unsafe { info.ProcessInfo.as_ref() }.map_or(0, |p| p.ProcessId);
        let mut m = requests();
        let r = m.get_or_insert_with(HashMap::new).entry(pid).or_default();
        if data {
            r.fetch_data += 1;
        } else {
            r.fetch_placeholders += 1;
        }
    }

    /// Completes a request with a failure status so the requester gets an
    /// error at once instead of waiting for the platform timeout.
    ///
    /// # Safety
    ///
    /// `info` must be the callback info cfapi passed to the callback.
    unsafe fn fail(
        info: &CF_CALLBACK_INFO,
        op: CF_OPERATION_TYPE,
        params: CF_OPERATION_PARAMETERS,
    ) {
        let op_info = CF_OPERATION_INFO {
            StructSize: size_of::<CF_OPERATION_INFO>() as u32,
            Type: op,
            ConnectionKey: info.ConnectionKey,
            TransferKey: info.TransferKey,
            CorrelationVector: info.CorrelationVector,
            SyncStatus: std::ptr::null(),
            RequestKey: info.RequestKey,
        };
        let mut params = params;
        // SAFETY: both structures are fully initialised and outlive the call.
        let _ = unsafe { CfExecute(&op_info, &mut params) };
    }

    unsafe extern "system" fn on_fetch_data(
        info: *const CF_CALLBACK_INFO,
        params: *const CF_CALLBACK_PARAMETERS,
    ) {
        // SAFETY: cfapi passes valid pointers for the duration of the call.
        let (info, params) = unsafe { (&*info, &*params) };
        // SAFETY: `info` came from cfapi; for FETCH_DATA the union holds
        // FetchData.
        let fetch = unsafe {
            count(info, true);
            params.Anonymous.FetchData
        };
        let p = CF_OPERATION_PARAMETERS {
            ParamSize: (std::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
                + size_of::<CF_OPERATION_PARAMETERS_0_0>()) as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 {
                TransferData: CF_OPERATION_PARAMETERS_0_0 {
                    CompletionStatus: STATUS_UNSUCCESSFUL,
                    Buffer: std::ptr::null(),
                    Offset: fetch.RequiredFileOffset,
                    Length: fetch.RequiredLength,
                    ..Default::default()
                },
            },
        };
        // SAFETY: `info` came from cfapi.
        unsafe { fail(info, CF_OPERATION_TYPE_TRANSFER_DATA, p) };
    }

    unsafe extern "system" fn on_fetch_placeholders(
        info: *const CF_CALLBACK_INFO,
        _params: *const CF_CALLBACK_PARAMETERS,
    ) {
        // SAFETY: cfapi passes a valid pointer for the duration of the call.
        let info = unsafe { &*info };
        // SAFETY: `info` came from cfapi.
        unsafe { count(info, false) };
        let p = CF_OPERATION_PARAMETERS {
            ParamSize: (std::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
                + size_of::<CF_OPERATION_PARAMETERS_0_4>()) as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 {
                TransferPlaceholders: CF_OPERATION_PARAMETERS_0_4 {
                    CompletionStatus: STATUS_UNSUCCESSFUL,
                    ..Default::default()
                },
            },
        };
        // SAFETY: `info` came from cfapi.
        unsafe { fail(info, CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS, p) };
    }

    /// A registered and connected sync root. Dropping it disconnects and
    /// unregisters, in that order.
    pub(super) struct SyncRoot {
        path: PathBuf,
        key: Option<CF_CONNECTION_KEY>,
        registered: bool,
    }

    impl SyncRoot {
        /// Registers `path` (an existing empty directory) as a sync root with
        /// full hydration and population policies, then connects the
        /// counting provider.
        pub(super) fn register(path: &Path) -> Result<Self, String> {
            let mut root = Self {
                path: path.to_owned(),
                key: None,
                registered: false,
            };
            let p = wpath(path);
            let name = wz("strata-walk-test");
            let version = wz("1");
            let identity = b"strata-walk-test-root";
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let reg = CF_SYNC_REGISTRATION {
                StructSize: size_of::<CF_SYNC_REGISTRATION>() as u32,
                ProviderName: PCWSTR(name.as_ptr()),
                ProviderVersion: PCWSTR(version.as_ptr()),
                SyncRootIdentity: identity.as_ptr().cast(),
                SyncRootIdentityLength: identity.len() as u32,
                FileIdentity: std::ptr::null(),
                FileIdentityLength: 0,
                ProviderId: GUID::from_u128(nanos ^ (u128::from(std::process::id()) << 96)),
            };
            let policies = CF_SYNC_POLICIES {
                StructSize: size_of::<CF_SYNC_POLICIES>() as u32,
                Hydration: CF_HYDRATION_POLICY {
                    Primary: CF_HYDRATION_POLICY_FULL,
                    ..Default::default()
                },
                Population: CF_POPULATION_POLICY {
                    Primary: CF_POPULATION_POLICY_FULL,
                    ..Default::default()
                },
                ..Default::default()
            };
            // SAFETY: every pointer in `reg` and `p` outlives the call.
            unsafe {
                CfRegisterSyncRoot(
                    PCWSTR(p.as_ptr()),
                    &reg,
                    &policies,
                    CF_REGISTER_FLAG_DISABLE_ON_DEMAND_POPULATION_ON_ROOT
                        | CF_REGISTER_FLAG_MARK_IN_SYNC_ON_ROOT,
                )
            }
            .map_err(|e| format!("CfRegisterSyncRoot: {e}"))?;
            root.registered = true;

            let table = [
                CF_CALLBACK_REGISTRATION {
                    Type: CF_CALLBACK_TYPE_FETCH_DATA,
                    Callback: Some(on_fetch_data),
                },
                CF_CALLBACK_REGISTRATION {
                    Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
                    Callback: Some(on_fetch_placeholders),
                },
                CF_CALLBACK_REGISTRATION {
                    Type: CF_CALLBACK_TYPE_NONE,
                    Callback: None,
                },
            ];
            // SAFETY: the table is terminated by CF_CALLBACK_TYPE_NONE and
            // cfapi copies it; the callbacks are 'static functions.
            let key = unsafe {
                CfConnectSyncRoot(
                    PCWSTR(p.as_ptr()),
                    table.as_ptr(),
                    None,
                    CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO,
                )
            }
            .map_err(|e| format!("CfConnectSyncRoot: {e}"))?;
            root.key = Some(key);
            Ok(root)
        }

        /// Creates placeholders under `dir` (the sync root or a placeholder
        /// directory inside it): `(name, logical size, is_dir, populated)`.
        pub(super) fn placeholders(
            &self,
            dir: &Path,
            items: &[(&str, u64, bool, bool)],
        ) -> Result<(), String> {
            let names: Vec<Vec<u16>> = items.iter().map(|i| wz(i.0)).collect();
            let identities: Vec<Vec<u8>> = items
                .iter()
                .map(|i| format!("id:{}", i.0).into_bytes())
                .collect();
            let now = filetime_now();
            let mut infos: Vec<CF_PLACEHOLDER_CREATE_INFO> = items
                .iter()
                .enumerate()
                .map(|(n, &(_, size, is_dir, populated))| {
                    let mut flags: CF_PLACEHOLDER_CREATE_FLAGS =
                        CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC;
                    if is_dir && populated {
                        flags |= CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION;
                    } else if !is_dir {
                        flags |= CF_PLACEHOLDER_CREATE_FLAG_NONE;
                    }
                    CF_PLACEHOLDER_CREATE_INFO {
                        RelativeFileName: PCWSTR(names[n].as_ptr()),
                        FsMetadata: CF_FS_METADATA {
                            BasicInfo: FILE_BASIC_INFO {
                                CreationTime: now,
                                LastAccessTime: now,
                                LastWriteTime: now,
                                ChangeTime: now,
                                FileAttributes: if is_dir {
                                    strata_core::win32::FILE_ATTRIBUTE_DIRECTORY
                                } else {
                                    0x80 // FILE_ATTRIBUTE_NORMAL
                                },
                            },
                            FileSize: if is_dir { 0 } else { size as i64 },
                        },
                        FileIdentity: identities[n].as_ptr().cast(),
                        FileIdentityLength: identities[n].len() as u32,
                        Flags: flags,
                        ..Default::default()
                    }
                })
                .collect();
            let base = wpath(dir);
            let mut processed = 0u32;
            // SAFETY: names, identities and `base` outlive the call; the
            // array is writable (cfapi stores per-entry results in it).
            unsafe {
                CfCreatePlaceholders(
                    PCWSTR(base.as_ptr()),
                    &mut infos,
                    CF_CREATE_FLAG_NONE,
                    Some(&mut processed),
                )
            }
            .map_err(|e| format!("CfCreatePlaceholders: {e}"))?;
            for (i, info) in infos.iter().enumerate() {
                info.Result
                    .ok()
                    .map_err(|e| format!("placeholder {}: {e}", items[i].0))?;
            }
            Ok(())
        }
    }

    fn filetime_now() -> i64 {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        (unix as i64 + 11_644_473_600) * 10_000_000
    }

    impl Drop for SyncRoot {
        fn drop(&mut self) {
            if let Some(key) = self.key.take() {
                // SAFETY: the key came from a successful CfConnectSyncRoot
                // and is disconnected exactly once.
                let _ = unsafe { CfDisconnectSyncRoot(key) };
            }
            if self.registered {
                let p = wpath(&self.path);
                // SAFETY: `p` is NUL-terminated and outlives the call.
                if let Err(e) = unsafe { CfUnregisterSyncRoot(PCWSTR(p.as_ptr())) } {
                    eprintln!("warning: CfUnregisterSyncRoot failed: {e}");
                }
            }
        }
    }
}

const MIB: u64 = 1 << 20;

/// Dehydrated placeholder files of the cloud fixture, relative to the sync
/// root, with their logical sizes.
const CLOUD_FILES: [(&str, u64); 4] = [
    ("a.bin", MIB),
    ("b.bin", MIB),
    ("c.bin", 3 * MIB),
    (r"listed-dir\inner.bin", 2 * MIB),
];

/// Environment variable naming the fixture for [`cloud_walk_child`].
const CLOUD_CHILD_ENV: &str = "STRATA_WALK_HARDEN_CLOUD_FIXTURE";

/// Walks the cloud fixture at `base` (the directory holding `sync`) in every
/// mode and checks what the records say about the placeholders.
fn assert_cloud_walk(base: &Path) {
    let sync = base.join("sync");
    for (method, alloc) in MODES {
        let started = Instant::now();
        let (recs, stats) = walk(base, opts(method, alloc));
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{method:?}/{alloc}: walk stalled on the provider"
        );
        let v = View::new(&recs);
        assert_eq!(
            stats.errors.total(),
            0,
            "{method:?}/{alloc}: {:?}",
            stats.errors
        );
        for (f, size) in CLOUD_FILES {
            let r = v.get(&format!(r"sync\{f}"));
            assert_eq!(r.sizes.logical, size, "{f} {method:?}/{alloc}");
            assert_eq!(
                r.sizes.allocated, 0,
                "{f} {method:?}/{alloc}: nothing is local"
            );
            assert_eq!(
                r.flags.reparse(),
                ReparseKind::Cloud,
                "{f} {method:?}/{alloc}"
            );
            assert!(
                win32::is_cloud_tag(r.reparse.as_ref().map_or(0, |p| p.tag)),
                "{f}: real cloud tag reported"
            );
            assert_eq!(
                r.flags.cloud(),
                strata_core::CloudState::OnlineOnly,
                "{f} {method:?}/{alloc}"
            );
            assert!(r.flags.contains(EntryFlags::OFFLINE), "{f}");
            assert!(r.ads.is_empty());
        }
        let listed = v.get(r"sync\listed-dir");
        assert!(listed.is_dir());
        assert!(!listed.flags.contains(EntryFlags::PARTIAL));
        let online = v.get(r"sync\online-dir");
        assert!(online.is_dir());
        assert_eq!(online.flags.reparse(), ReparseKind::Cloud);
        assert!(
            online.flags.contains(EntryFlags::PARTIAL),
            "{method:?}/{alloc}: online-only directory flagged partial"
        );
        assert!(v.children(online.id).is_empty(), "never descended");
        assert!(
            stats.partial && stats.partial_dirs == 1,
            "{method:?}/{alloc}"
        );

        // An online-only directory given as the root is not listed either.
        let (recs, stats) = walk(&sync.join("online-dir"), opts(method, alloc));
        assert_eq!(recs.len(), 1);
        assert!(recs[0].flags.contains(EntryFlags::PARTIAL));
        assert!(stats.partial);
    }
}

#[test]
#[ignore = "run in a child process by cloud_placeholders_are_never_hydrated"]
fn cloud_walk_child() {
    let Some(base) = std::env::var_os(CLOUD_CHILD_ENV) else {
        return;
    };
    assert_cloud_walk(Path::new(&base));
}

#[test]
fn cloud_placeholders_are_never_hydrated() {
    let mut t = Tree::new("cloud");
    let root_dir = t.dir("sync");
    let sync = match cloud::SyncRoot::register(&t.plain.join("sync")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping: cannot register a cloud-files sync root: {e}");
            return;
        }
    };
    if let Err(e) = sync.placeholders(
        &t.plain.join("sync"),
        &[
            ("a.bin", MIB, false, false),
            ("b.bin", MIB, false, false),
            ("c.bin", 3 * MIB, false, false),
            ("online-dir", 0, true, false),
            ("listed-dir", 0, true, true),
        ],
    ) {
        eprintln!("skipping: cannot create cloud placeholders: {e}");
        return;
    }
    sync.placeholders(
        &t.plain.join(r"sync\listed-dir"),
        &[("inner.bin", 2 * MIB, false, false)],
    )
    .expect("placeholder inside a populated placeholder directory");
    // The provider must be gone before the directory is deleted.
    t.on_drop(move || drop(sync));

    for (f, _) in CLOUD_FILES {
        let a = attributes_of(&root_dir.join(f));
        assert_ne!(
            a & crate::record::RECALL_BITS,
            0,
            "{f}: fixture is dehydrated ({a:#x})"
        );
    }

    // NOTE: cfapi never asks a provider to populate directories for its own
    // process, so an in-process walk cannot prove listings stay local. The
    // walk runs in-process for hydration and again in a child process,
    // which the provider treats like any other application.
    let me = std::process::id();
    assert_cloud_walk(&t.plain);
    assert_eq!(
        cloud::requests_from(me),
        cloud::Requests::default(),
        "in-process walk asked the provider for data"
    );

    let child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "harden_tests::cloud_walk_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CLOUD_CHILD_ENV, &t.plain)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn child walk");
    let child_pid = child.id();
    let out = child.wait_with_output().expect("child walk");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    eprintln!("{text}");
    assert!(out.status.success(), "child walk failed:\n{text}");
    assert!(text.contains("1 passed"), "child walk did not run:\n{text}");
    assert_eq!(
        cloud::requests_from(child_pid),
        cloud::Requests::default(),
        "child-process walk asked the provider for data or listings"
    );

    for (f, _) in CLOUD_FILES {
        let p = root_dir.join(f);
        let a = attributes_of(&p);
        assert_ne!(
            a & crate::record::RECALL_BITS,
            0,
            "{f} still dehydrated ({a:#x})"
        );
        let alloc = sys::standard_info(&handle_of(&p)).expect("std").allocation;
        assert_eq!(alloc, 0, "{f}: no clusters were filled in");
    }

    // Positive controls: real data access and a foreign directory listing
    // both reach the provider, which fails them, so the counters work.
    let started = Instant::now();
    assert!(
        fs::read(root_dir.join("a.bin")).is_err(),
        "the provider never supplies data"
    );
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(
        cloud::requests_from(me).fetch_data > 0,
        "a deliberate read must reach FETCH_DATA"
    );
    assert_ne!(
        attributes_of(&root_dir.join("a.bin")) & crate::record::RECALL_BITS,
        0
    );
    let mut dir = Command::new("cmd")
        .args(["/C", "dir"])
        .arg(t.plain.join(r"sync\online-dir"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn dir");
    let dir_pid = dir.id();
    let _ = dir.wait();
    assert!(
        cloud::requests_from(dir_pid).fetch_placeholders > 0,
        "a foreign listing of an online-only directory must reach FETCH_PLACEHOLDERS"
    );
    eprintln!(
        "cloud: {} provider requests in total",
        cloud::all_requests()
    );
}

#[test]
fn exclusively_held_file_is_probed_without_data_access() {
    let t = Tree::new("held");
    let p = t.file("held.bin", 10_000);
    write_stream(&p, "side", 5000);
    let _held = {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&p)
            .expect("open without sharing")
    };
    assert_eq!(
        fs::read(&p).map_err(|e| e.raw_os_error()).err(),
        Some(Some(32)),
        "fixture holds the file exclusively"
    );
    for (method, alloc) in MODES {
        let (recs, stats) = walk(&t.plain, opts(method, alloc));
        assert_eq!(
            stats.errors.total(),
            0,
            "{method:?}/{alloc}: {:?}",
            stats.errors
        );
        assert!(!stats.partial);
        let r = View::new(&recs).get("held.bin");
        assert_eq!(r.sizes.logical, 10_000);
        if method == ListingMethod::DirectoryInfo || alloc {
            assert_eq!(r.sizes.allocated, 12_288, "{method:?}/{alloc}");
        }
        if alloc {
            assert_eq!((r.ads.len(), r.sizes.ads_logical), (1, 5000));
        }
    }
}

#[test]
fn offline_file_streams_are_not_queried() {
    let t = Tree::new("offline");
    let hsm = t.file("hsm.bin", 9000);
    let local = t.file("local.bin", 9000);
    for f in [&hsm, &local] {
        write_stream(f, "meta", 7000);
    }
    if let Err(e) = set_attributes(&hsm, win32::FILE_ATTRIBUTE_OFFLINE) {
        eprintln!("skipping: cannot set FILE_ATTRIBUTE_OFFLINE: {e}");
        return;
    }
    assert_ne!(attributes_of(&hsm) & win32::FILE_ATTRIBUTE_OFFLINE, 0);
    for method in [ListingMethod::DirectoryInfo, ListingMethod::FindFirstFile] {
        let (recs, stats) = walk(&t.plain, opts(method, true));
        assert_eq!(stats.errors.total(), 0);
        let v = View::new(&recs);
        let r = v.get("hsm.bin");
        assert!(r.flags.contains(EntryFlags::OFFLINE), "{method:?}");
        assert_eq!(r.flags.cloud(), strata_core::CloudState::None);
        assert!(
            r.ads.is_empty(),
            "{method:?}: streams of offline content are never queried"
        );
        assert!(!r.flags.contains(EntryFlags::HAS_ADS));
        assert_eq!((r.sizes.ads_logical, r.sizes.ads_allocated), (0, 0));
        assert_eq!(r.sizes.logical, 9000);
        assert_eq!(r.sizes.allocated, 12_288, "standard info is still read");
        assert!(!r.flags.contains(EntryFlags::PARTIAL));

        let c = v.get("local.bin");
        assert!(!c.flags.contains(EntryFlags::OFFLINE));
        assert_eq!(
            c.ads.len(),
            1,
            "control: the same stream is found when local"
        );
    }
}

// -----------------------------------------------------------------------------
// Cycles
// -----------------------------------------------------------------------------

fn target_of(r: &ScanRecord) -> Option<String> {
    r.reparse
        .as_ref()
        .and_then(|p| p.target.as_ref())
        .map(strata_core::WideName::to_string_lossy)
}

#[test]
fn directory_symlink_to_ancestor_is_not_followed() {
    let t = Tree::new("symup");
    t.file(r"a\b\leaf.txt", 3);
    if let Err(e) = std::os::windows::fs::symlink_dir(&t.plain, t.plain.join(r"a\b\up")) {
        eprintln!("skipping: cannot create directory symlinks: {e}");
        return;
    }
    std::os::windows::fs::symlink_dir(r"..\..", t.plain.join(r"a\b\up-relative"))
        .expect("relative symlink");
    for (method, alloc) in MODES {
        let started = Instant::now();
        let (recs, stats) = walk(&t.plain, opts(method, alloc));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(stats.errors.total(), 0);
        assert_eq!(
            recs.len(),
            6,
            "{method:?}/{alloc}: root, a, b, leaf, 2 links"
        );
        let v = View::new(&recs);
        for (name, target) in [
            (r"a\b\up", t.plain.to_string_lossy().into_owned()),
            (r"a\b\up-relative", r"..\..".to_owned()),
        ] {
            let l = v.get(name);
            assert!(l.is_dir());
            assert_eq!(l.flags.reparse(), ReparseKind::Symlink, "{name}");
            assert_eq!(target_of(l), Some(target), "{name}");
            assert!(v.children(l.id).is_empty(), "{name} is not followed");
        }
    }
}

#[test]
fn symlink_loops_are_recorded() {
    let t = Tree::new("symloop");
    use std::os::windows::fs::{symlink_dir, symlink_file};
    if let Err(e) = symlink_file("b", t.plain.join("a")) {
        eprintln!("skipping: cannot create symlinks: {e}");
        return;
    }
    symlink_file("a", t.plain.join("b")).expect("b -> a");
    symlink_dir("db", t.plain.join("da")).expect("da -> db");
    symlink_dir("da", t.plain.join("db")).expect("db -> da");
    symlink_file("self", t.plain.join("self")).expect("self -> self");
    for (method, alloc) in MODES {
        let (recs, stats) = walk(&t.plain, opts(method, alloc));
        assert_eq!(
            stats.errors.total(),
            0,
            "{method:?}/{alloc}: {:?}",
            stats.errors
        );
        assert_eq!(recs.len(), 6);
        let v = View::new(&recs);
        for (name, target, dir) in [
            ("a", "b", false),
            ("b", "a", false),
            ("da", "db", true),
            ("db", "da", true),
            ("self", "self", false),
        ] {
            let l = v.get(name);
            assert_eq!(l.is_dir(), dir, "{name}");
            assert_eq!(l.flags.reparse(), ReparseKind::Symlink, "{name}");
            assert_eq!(target_of(l).as_deref(), Some(target), "{name}");
            assert_eq!(l.sizes.logical, 0);
            assert!(v.children(l.id).is_empty());
        }
    }

    // A loop given as the root cannot be resolved: a typed error or one
    // flagged record, never a hang.
    for root in ["da", "a", "self"] {
        let started = Instant::now();
        let mut recs = Vec::new();
        let res = Walker::new(t.plain.join(root), WalkOptions::default())
            .expect("walker")
            .run(&mut recs, &CancelToken::new());
        assert!(started.elapsed() < Duration::from_secs(10));
        match res {
            Err(e) => eprintln!("loop root {root}: {e}"),
            Ok(stats) => {
                assert_eq!(recs.len(), 1, "{root}");
                // NOTE: file symlinks are recorded as themselves and never
                // followed, so only the directory loop leaves a partial result.
                if root == "da" {
                    assert!(stats.partial, "{root}");
                    assert!(recs[0].flags.contains(EntryFlags::PARTIAL), "{root}");
                }
            }
        }
    }
}

/// The unprivileged counterpart of [`symlink_loops_are_recorded`]:
/// junctions need no privilege.
#[test]
fn junction_loops_are_recorded() {
    let t = Tree::new("jloop");
    let (a, b) = (t.plain.join("ja"), t.plain.join("jb"));
    mklink_junction(&a, &b).expect("ja -> jb");
    mklink_junction(&b, &a).expect("jb -> ja");
    for (method, alloc) in MODES {
        let (recs, stats) = walk(&t.plain, opts(method, alloc));
        assert_eq!(
            stats.errors.total(),
            0,
            "{method:?}/{alloc}: {:?}",
            stats.errors
        );
        assert_eq!(recs.len(), 3);
        let v = View::new(&recs);
        for (name, target) in [("ja", &b), ("jb", &a)] {
            let j = v.get(name);
            assert_eq!(j.flags.reparse(), ReparseKind::MountPoint);
            assert_eq!(target_of(j), Some(target.to_string_lossy().into_owned()));
            assert!(v.children(j.id).is_empty());
        }
    }
    let started = Instant::now();
    let mut recs = Vec::new();
    let res = Walker::new(&a, WalkOptions::default())
        .expect("walker")
        .run(&mut recs, &CancelToken::new());
    assert!(started.elapsed() < Duration::from_secs(10));
    match res {
        Err(e) => eprintln!("junction loop root: {e}"),
        Ok(stats) => {
            assert_eq!(recs.len(), 1);
            assert!(stats.partial && recs[0].flags.contains(EntryFlags::PARTIAL));
        }
    }
}

#[test]
fn junction_to_volume_root_is_not_followed() {
    let t = Tree::new("volroot");
    let drive: PathBuf = t.plain.components().take(2).collect();
    assert!(drive.has_root(), "{}", drive.display());
    let drive_s = drive.to_string_lossy().into_owned();
    mklink_junction(&t.plain.join("whole-volume"), &drive).expect("mklink /J");
    t.file("marker.txt", 1);
    for (method, alloc) in MODES {
        let started = Instant::now();
        let (recs, stats) = walk(&t.plain, opts(method, alloc));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{method:?}/{alloc}: the volume was walked"
        );
        assert_eq!(recs.len(), 3, "{method:?}/{alloc}: root, junction, marker");
        assert_eq!(stats.errors.total(), 0);
        let v = View::new(&recs);
        let j = v.get("whole-volume");
        assert_eq!(j.flags.reparse(), ReparseKind::MountPoint);
        assert_eq!(target_of(j).as_deref(), Some(drive_s.as_str()));
        assert!(v.children(j.id).is_empty());
    }
}

// -----------------------------------------------------------------------------
// Paths
// -----------------------------------------------------------------------------

/// NT paths are `UNICODE_STRING`s: at most 32,767 UTF-16 units.
const NT_PATH_MAX: usize = 32_767;

#[test]
fn paths_near_the_nt_length_limit() {
    let mut t = Tree::new("ntmax");
    let mut cur = t.verbatim.clone();
    let mut comps: Vec<String> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut len = 250usize;
    // Grow the deepest directory until even a one-character component no
    // longer fits, leaving a path within a few units of the NT limit. Each
    // level also gets a file while one still fits.
    while len > 0 {
        let name = "n".repeat(len);
        let next = cur.join(&name);
        if wide(&next).len() < NT_PATH_MAX && fs::create_dir(&next).is_ok() {
            let f = next.join("f");
            if fs::write(&f, b"leaf").is_ok() {
                files.push(f);
            }
            dirs.push(next.clone());
            cur = next;
            comps.push(name);
        } else {
            len /= 2;
        }
    }
    {
        // NOTE: `remove_dir_all` fails with ERROR_FILENAME_EXCED_RANGE this
        // deep, so the guard removes the chain bottom-up first.
        let (files, dirs) = (files.clone(), dirs.clone());
        t.on_drop(move || {
            for f in &files {
                let _ = fs::remove_file(f);
            }
            for d in dirs.iter().rev() {
                let _ = fs::remove_dir(d);
            }
        });
    }
    let deepest = wide(&cur).len();
    // NOTE: some volumes refuse to create a directory a few dozen units
    // short of the limit (GitHub's runners stop at 32,747).
    assert!(
        deepest > NT_PATH_MAX - 64,
        "deepest directory path is {deepest} units"
    );
    let longest_file = files.last().map_or(0, |f| wide(f).len());
    assert!(longest_file > NT_PATH_MAX - 260, "{longest_file}");
    eprintln!(
        "deepest directory: {deepest} units over {} levels; {} files, longest {longest_file} units",
        comps.len(),
        files.len()
    );

    for (method, alloc) in MODES {
        let started = Instant::now();
        let (recs, stats) = walk(&t.plain, opts(method, alloc));
        assert!(started.elapsed() < Duration::from_secs(30));
        let v = View::new(&recs);
        let mut node = v.root;
        let mut depth = 0;
        let mut found_files = 0;
        for c in &comps {
            match v.child(node.id, &w(c)) {
                Some(n) => {
                    node = n;
                    depth += 1;
                    found_files += usize::from(v.child(n.id, &w("f")).is_some());
                }
                None => break,
            }
        }
        assert_eq!(
            depth,
            comps.len(),
            "{method:?}/{alloc}: every level is recorded ({:?})",
            stats.errors
        );
        assert!(!node.flags.contains(EntryFlags::PARTIAL));
        assert_eq!(found_files, files.len(), "{method:?}/{alloc}");
        assert_eq!(
            stats.errors.total(),
            0,
            "{method:?}/{alloc}: {:?}",
            stats.errors
        );
        assert!(!stats.partial);
    }

    // `FindFirstFileExW` needs `<dir>\*` plus a terminator, which does not
    // fit for the deepest directory; the listing falls back to its handle.
    // NOTE: on volumes that stop short of the limit the search pattern still
    // fits, so there is no fallback to check.
    let ext = crate::path::to_extended(&wide(&cur));
    let mut entries = Vec::new();
    let Err(err) = sys::find_list(&ext, &CancelToken::new(), &mut entries) else {
        return;
    };
    assert_eq!(err.raw_os_error(), Some(sys::ERROR_FILENAME_EXCED_RANGE));
    let l = crate::walker::list_dir(
        &ext,
        ListingMethod::FindFirstFile,
        crate::parse::DirInfoClass::IdExtd,
        false,
        false,
        &CancelToken::new(),
    )
    .expect("listing falls back to the directory handle");
    assert!(l.complete && l.handle.is_some());
}

#[test]
fn dot_and_space_names_survive_verbatim() {
    let t = Tree::new("dots");
    let files = ["...", "..a", " ", "a..", ". "];
    for (i, n) in files.iter().enumerate() {
        fs::write(t.verbatim.join(n), vec![b'd'; i + 1]).expect("create dot name");
    }
    let d = t.verbatim.join("..dir.");
    fs::create_dir(&d).expect("dot dir");
    fs::write(d.join(" inner "), b"12345").expect("inner");
    let sp = t.verbatim.join("  ");
    fs::create_dir(&sp).expect("space dir");
    fs::write(sp.join("..."), b"123").expect("dots in space dir");

    for (method, alloc) in MODES {
        let (recs, stats) = walk(&t.plain, opts(method, alloc));
        assert_eq!(
            stats.errors.total(),
            0,
            "{method:?}/{alloc}: {:?}",
            stats.errors
        );
        let v = View::new(&recs);
        for (i, n) in files.iter().enumerate() {
            let r = v
                .child(v.root.id, &w(n))
                .unwrap_or_else(|| panic!("{n:?} missing ({method:?}/{alloc})"));
            assert_eq!(r.sizes.logical, i as u64 + 1, "{n:?}");
        }
        let dd = v.child(v.root.id, &w("..dir.")).expect("..dir.");
        assert!(dd.is_dir());
        assert_eq!(
            v.child(dd.id, &w(" inner ")).expect("inner").sizes.logical,
            5
        );
        let sd = v.child(v.root.id, &w("  ")).expect("space dir");
        assert_eq!(v.child(sd.id, &w("...")).expect("...").sizes.logical, 3);
        assert_eq!(recs.len(), 1 + 5 + 2 + 2);
    }
}

// -----------------------------------------------------------------------------
// Races
// -----------------------------------------------------------------------------

#[test]
fn file_replaced_by_directory_before_probe() {
    for (method, alloc) in MODES {
        let t = Tree::new("file2dir");
        t.file(r"d\turncoat.bin", 4321);
        t.file(r"d\steady.bin", 77);
        let root = t.verbatim.clone();
        let hooks = crate::walker::Hooks {
            before_refine: Some(std::sync::Arc::new(move |p: &[u16]| {
                if p.ends_with(&w(r"\d")) {
                    let f = root.join(r"d\turncoat.bin");
                    fs::remove_file(&f).expect("remove file");
                    fs::create_dir(&f).expect("replace with directory");
                    fs::write(f.join("inside.txt"), b"x").expect("fill directory");
                }
            })),
            ..Default::default()
        };
        let (recs, stats) = hooked(&t.plain, opts(method, alloc), hooks);
        let v = View::new(&recs);
        assert_eq!(v.get(r"d\steady.bin").sizes.logical, 77);
        let r = v.get(r"d\turncoat.bin");
        // The listing saw a file; the probe (when there is one) opens what is
        // there now with attribute access only, which succeeds on a
        // directory and reports no data. The entry is kept once, as a file.
        assert!(!r.is_dir());
        if alloc {
            assert_eq!(r.sizes.logical, 0, "{method:?}: probed the directory");
        } else {
            assert_eq!(r.sizes.logical, 4321);
        }
        assert_eq!(
            stats.errors.total(),
            0,
            "{method:?}/{alloc}: {:?}",
            stats.errors
        );
        assert!(v.try_get(r"d\turncoat.bin\inside.txt").is_none());
    }
}

/// xorshift64*: deterministic, dependency-free randomness for the churn
/// thread.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn walks_stay_consistent_under_concurrent_churn() {
    const DIRS: usize = 12;
    let t = Tree::new("churn");
    for d in 0..DIRS {
        for f in 0..40 {
            t.file(&format!(r"d{d:02}\f{f:03}.txt"), f);
        }
        t.file(&format!(r"d{d:02}\sub\s.txt"), 5);
    }
    let base = t.verbatim.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ops = std::sync::Arc::new(AtomicUsize::new(0));
    let churn = {
        let (stop, ops, base) = (stop.clone(), ops.clone(), base.clone());
        std::thread::spawn(move || {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                n += 1;
                let d = base.join(format!("d{:02}", rng.below(DIRS)));
                let e = base.join(format!("d{:02}", rng.below(DIRS)));
                let f = |dir: &Path, i: usize| dir.join(format!("f{i:03}.txt"));
                // Every outcome is acceptable: the walker races the same
                // operations, so failures here are expected and ignored.
                let _ = match rng.below(8) {
                    0 => fs::write(f(&d, rng.below(60)), b"churn"),
                    1 => fs::remove_file(f(&d, rng.below(60))),
                    2 => fs::rename(f(&d, rng.below(60)), f(&d, rng.below(60))),
                    3 => fs::rename(f(&d, rng.below(60)), f(&e, rng.below(60))),
                    4 => fs::create_dir_all(d.join(format!("new{}\\deeper", rng.below(4)))),
                    5 => fs::remove_dir_all(d.join(format!("new{}", rng.below(4)))),
                    6 => fs::rename(d.join("sub"), e.join(format!("moved{n}"))),
                    _ => fs::create_dir(d.join("sub"))
                        .and_then(|()| fs::write(d.join(r"sub\s.txt"), b"again")),
                };
                ops.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    let started = Instant::now();
    let mut walks = 0;
    let mut failure = None;
    while started.elapsed() < Duration::from_secs(4) || walks < 8 {
        let (method, alloc) = MODES[walks % MODES.len()];
        let (recs, _) = walk(&t.plain, opts(method, alloc));
        if let Err(e) = check_structure(&recs) {
            failure = Some(format!("{method:?}/{alloc} walk {walks}: {e}"));
            break;
        }
        walks += 1;
    }
    stop.store(true, Ordering::Relaxed);
    churn.join().expect("churn thread");
    eprintln!(
        "churn: {walks} walks against {} filesystem operations",
        ops.load(Ordering::Relaxed)
    );
    if let Some(f) = failure {
        panic!("{f}");
    }
}

// -----------------------------------------------------------------------------
// Access denied
// -----------------------------------------------------------------------------

#[test]
fn directory_denied_read_and_execute_is_flagged() {
    let mut t = Tree::new("denyrx");
    t.file(r"locked\secret.txt", 50);
    t.file(r"locked\sub\deeper.txt", 50);
    t.file(r"open\visible.txt", 7);
    let locked = t.plain.join("locked");
    deny(&mut t, &locked, "(RX)");
    for (method, alloc) in MODES {
        let (recs, stats) = walk(&t.plain, opts(method, alloc));
        let v = View::new(&recs);
        let l = v.get("locked");
        assert!(
            l.flags.contains(EntryFlags::ACCESS_DENIED),
            "{method:?}/{alloc}"
        );
        assert!(v.children(l.id).is_empty());
        assert_eq!(v.get(r"open\visible.txt").sizes.logical, 7);
        assert_eq!(stats.access_denied_dirs, 1);
        assert_eq!(stats.errors.get(ErrorKind::AccessDenied), 1);
        assert!(stats.partial);
        assert_eq!(recs.len(), 4);
    }
}

#[test]
fn denied_walk_root_is_flagged_not_fatal() {
    for rights in ["(RD)", "(RX)", "(F)"] {
        let mut t = Tree::new("denyroot");
        t.file(r"root\inside.txt", 9);
        let root = t.plain.join("root");
        deny(&mut t, &root, rights);
        for (method, alloc) in MODES {
            let started = Instant::now();
            let mut recs = Vec::new();
            let res = Walker::new(&root, opts(method, alloc))
                .expect("walker")
                .run(&mut recs, &CancelToken::new());
            assert!(started.elapsed() < Duration::from_secs(10));
            let stats = res.unwrap_or_else(|e| panic!("{rights} {method:?}/{alloc}: {e}"));
            assert_eq!(recs.len(), 1, "{rights} {method:?}/{alloc}");
            assert!(
                recs[0].flags.contains(EntryFlags::ACCESS_DENIED),
                "{rights} {method:?}/{alloc}"
            );
            assert!(recs[0].is_dir());
            assert_eq!(stats.access_denied_dirs, 1);
            assert!(stats.partial);
        }
    }
}

#[test]
fn denied_file_is_counted_and_walk_continues() {
    let mut t = Tree::new("denyfile");
    let secret = t.file(r"d\secret.bin", 5000);
    write_stream(&secret, "s", 100);
    t.file(r"d\visible.bin", 6000);
    t.file(r"e\other.bin", 7000);
    let secret_plain = t.plain.join(r"d\secret.bin");
    deny(&mut t, &secret_plain, "(F)");
    let probe = sys::nt_open(
        None,
        &crate::path::to_nt(&crate::path::to_extended(&wide(&secret))),
        OpenMode::Attributes,
        false,
    );
    eprintln!(
        "attribute open of the denied file: {:?}",
        probe.as_ref().err()
    );
    drop(probe);
    for (method, alloc) in MODES {
        let (recs, stats) = walk(&t.plain, opts(method, alloc));
        let v = View::new(&recs);
        assert_eq!(v.get(r"d\visible.bin").sizes.logical, 6000);
        assert_eq!(v.get(r"e\other.bin").sizes.logical, 7000);
        let r = v.get(r"d\secret.bin");
        assert_eq!(r.sizes.logical, 5000, "listing values are kept");
        let denied = stats.errors.get(ErrorKind::AccessDenied);
        if alloc && denied > 0 {
            assert_eq!(denied, 1, "{method:?}");
            assert!(r.flags.contains(EntryFlags::PARTIAL));
            assert_eq!(stats.partial_files, 1);
            assert!(stats.partial);
        } else {
            assert_eq!(
                stats.errors.total(),
                0,
                "{method:?}/{alloc}: {:?}",
                stats.errors
            );
            assert!(!stats.partial);
            if alloc {
                assert_eq!(
                    r.ads.len(),
                    1,
                    "read-attributes access comes from the parent"
                );
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Sizes
// -----------------------------------------------------------------------------

#[test]
fn hundred_gib_sparse_file_on_second_volume() {
    let base = Path::new(r"D:\strata-walk-harden");
    let facts = sys::volume_facts(&crate::path::to_extended(&w(r"D:\")));
    match &facts {
        Ok(f) if f.filesystem.eq_ignore_ascii_case("NTFS") => {}
        Ok(f) => {
            eprintln!("skipping: D: is {}, not NTFS", f.filesystem);
            return;
        }
        Err(e) => {
            eprintln!("skipping: no D: volume: {e}");
            return;
        }
    }
    if let Err(e) = fs::create_dir_all(base) {
        eprintln!("skipping: cannot create {}: {e}", base.display());
        return;
    }
    let t = match Tree::at(base.join(unique())) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping: cannot create a fixture on D: {e}");
            return;
        }
    };
    const SIZE: u64 = 100 << 30;
    let p = t.p("huge.bin");
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&p)
            .expect("create");
        sys::fixture::set_sparse(&f).expect("FSCTL_SET_SPARSE");
        f.seek(SeekFrom::Start(SIZE - 1)).expect("seek");
        f.write_all(b"z").expect("write");
        // NOTE: the cluster behind the last byte is allocated only when the
        // cache flushes it; until then the file reports 0 allocated.
        f.sync_all().expect("flush");
    }
    let on_disk = sys::compressed_size(&handle_of(&p)).expect("compressed size");
    assert!(on_disk > 0 && on_disk <= 1 << 20, "{on_disk}");
    for (method, alloc) in MODES {
        let (recs, stats) = walk(&t.plain, opts(method, alloc));
        assert_eq!(stats.errors.total(), 0);
        let r = View::new(&recs).get("huge.bin");
        assert!(r.flags.contains(EntryFlags::SPARSE));
        assert_eq!(r.sizes.logical, SIZE, "{method:?}/{alloc}");
        if method == ListingMethod::DirectoryInfo || alloc {
            assert_eq!(r.sizes.allocated, on_disk, "{method:?}/{alloc}");
            assert!(stats.totals.allocated_bytes < 1 << 20);
        } else {
            // Without a handle the real allocation is unknown; the estimate
            // is the rounded logical size and says so.
            assert!(r.flags.contains(EntryFlags::ALLOC_ESTIMATED));
        }
    }
    drop(t);
    let _ = fs::remove_dir(base);
}

#[test]
fn wof_lzx_file_reports_real_allocation() {
    let t = Tree::new("lzx");
    let p = t.p("tool.exe");
    let body = b"strata walker lzx fixture ".repeat(120_000);
    fs::write(&p, &body).expect("write");
    let plain = t.plain.join("tool.exe");
    if let Err(e) = run_cmd(
        "compact",
        &["/c".as_ref(), "/exe:lzx".as_ref(), plain.as_os_str()],
    ) {
        eprintln!("skipping: compact /exe:lzx failed: {}", e.trim());
        return;
    }
    // NOTE: the WOF filter hides its reparse point from handle queries too,
    // so compression shows only in the allocation.
    let logical = body.len() as u64;
    let on_disk = sys::standard_info(&handle_of(&p)).expect("std").allocation;
    if on_disk == 0 || on_disk >= logical / 4 {
        eprintln!("skipping: compact /exe:lzx did not compress ({on_disk}/{logical})");
        return;
    }
    for method in [ListingMethod::DirectoryInfo, ListingMethod::FindFirstFile] {
        let (recs, _) = walk(&t.plain, opts(method, true));
        let r = View::new(&recs).get("tool.exe");
        assert_eq!(r.sizes.logical, logical);
        assert_eq!(r.sizes.allocated, on_disk, "{method:?}");
        assert!(r.ads.is_empty());
    }
}

// -----------------------------------------------------------------------------
// Alternate data streams
// -----------------------------------------------------------------------------

#[test]
fn thousands_of_streams_grow_the_buffer() {
    const N: usize = 2000;
    let t = Tree::new("manyads");
    let f = t.file("host.bin", 10);
    for i in 0..N {
        write_stream(&f, &format!("stream-with-a-longer-name-{i:05}"), 1);
    }
    write_stream(&f, "big", 9000);
    let d = t.dir("dirhost");
    for i in 0..300 {
        write_stream(&d, &format!("d{i:04}"), 2);
    }
    let mut buf = crate::sys::AlignedBuf::new(4096);
    let raw = sys::stream_info(&handle_of(&f), &mut buf)
        .expect("stream info")
        .len();
    assert!(
        raw > 64 * 1024,
        "the list needs several buffer doublings ({raw} bytes)"
    );

    for method in [ListingMethod::DirectoryInfo, ListingMethod::FindFirstFile] {
        let (recs, stats) = walk(&t.plain, opts(method, true));
        assert_eq!(stats.errors.total(), 0);
        assert!(!stats.partial);
        let v = View::new(&recs);
        let r = v.get("host.bin");
        assert_eq!(r.ads.len(), N + 1, "{method:?}");
        assert_eq!(r.sizes.ads_logical, N as u64 + 9000);
        assert_eq!(r.sizes.ads_allocated, 12_288);
        assert!(!r.flags.contains(EntryFlags::PARTIAL));
        let dr = v.get("dirhost");
        assert_eq!(dr.ads.len(), 300);
        assert_eq!(dr.sizes.ads_logical, 600);
    }
}

#[test]
fn stream_list_overflow_is_reported_not_dropped() {
    let t = Tree::new("adscap");
    let f = t.file("host.bin", 10);
    // More than the probe's initial 4 KiB buffer holds.
    for i in 0..200 {
        write_stream(&f, &format!("s{i:03}"), 3);
    }
    t.file("plain.bin", 20);

    let h = handle_of(&f);
    let mut buf = crate::sys::AlignedBuf::new(64);
    let err = sys::stream_info_capped(&h, &mut buf, 256).expect_err("cap below the list size");
    assert_eq!(err.raw_os_error(), Some(234), "ERROR_MORE_DATA");
    let mut buf = crate::sys::AlignedBuf::new(64);
    let full =
        crate::parse::parse_streams(sys::stream_info_capped(&h, &mut buf, 1 << 20).expect("fits"));
    assert_eq!(full.len(), 200);

    for method in [ListingMethod::DirectoryInfo, ListingMethod::FindFirstFile] {
        let hooks = crate::walker::Hooks {
            stream_info_cap: Some(256),
            ..Default::default()
        };
        let (recs, stats) = hooked(&t.plain, opts(method, true), hooks);
        let v = View::new(&recs);
        let r = v.get("host.bin");
        assert!(r.flags.contains(EntryFlags::PARTIAL), "{method:?}");
        assert!(r.ads.is_empty());
        assert_eq!(r.sizes.logical, 10);
        assert_eq!(
            stats.errors.get(ErrorKind::Other),
            1,
            "{method:?}: {:?}",
            stats.errors
        );
        assert_eq!(stats.partial_files, 1);
        assert!(stats.partial);
        let p = v.get("plain.bin");
        assert!(
            !p.flags.contains(EntryFlags::PARTIAL),
            "small lists still fit"
        );
    }
}

// -----------------------------------------------------------------------------
// Filesystem identity
// -----------------------------------------------------------------------------

#[test]
fn only_local_ntfs_and_refs_ids_are_trusted() {
    use crate::walker::trusts_file_ids;
    assert!(trusts_file_ids(false, Some("NTFS")));
    assert!(trusts_file_ids(false, Some("REFS")));
    for fs in ["FAT", "FAT32", "EXFAT", "UDF", "CDFS", "", "CSVFS"] {
        assert!(!trusts_file_ids(false, Some(fs)), "{fs}");
    }
    assert!(!trusts_file_ids(false, None));
    assert!(
        !trusts_file_ids(true, Some("NTFS")),
        "SMB servers may synthesise ids"
    );
    assert!(!trusts_file_ids(true, Some("REFS")));
}

#[test]
fn unusual_name_round_trip_through_listing() {
    // Names that differ only in what Win32 would normalise away must stay
    // distinct records.
    let t = Tree::new("normalise");
    let names: Vec<Vec<u16>> = ["x", "x.", "x ", "x. ", "x.."]
        .iter()
        .map(|s| w(s))
        .collect();
    for (i, n) in names.iter().enumerate() {
        fs::write(t.verbatim.join(OsString::from_wide(n)), vec![0u8; i + 1]).expect("create");
    }
    for (method, alloc) in MODES {
        let (recs, _) = walk(&t.plain, opts(method, alloc));
        let v = View::new(&recs);
        for (i, n) in names.iter().enumerate() {
            let r = v.child(v.root.id, n).expect("present");
            assert_eq!(r.sizes.logical, i as u64 + 1, "{method:?}/{alloc}");
        }
    }
}
