//! End-to-end tests on real trees built in `%TEMP%`. Nothing here needs
//! elevation; fixtures that need a privilege the machine lacks (symlinks
//! without Developer Mode, case-sensitive directories without WSL, the
//! `\\localhost\C$` admin share) are skipped with a printed reason.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use strata_core::{EntryFlags, FileRef, ReparseKind, ScanRecord, win32};

use crate::sys::{self, OpenMode};
use crate::walker::Hooks;
use crate::{
    CancelToken, ChannelSink, ErrorKind, FnSink, ListingMethod, WalkError, WalkEvent, WalkOptions,
    WalkStats, Walker,
};

// -----------------------------------------------------------------------------
// Fixtures
// -----------------------------------------------------------------------------

/// A temporary directory removed (after running registered restore steps,
/// e.g. ACL resets) on drop.
struct TempTree {
    /// Plain path, for tools such as `mklink` and `icacls`.
    plain: PathBuf,
    /// `\\?\` path, for creating names Win32 would otherwise normalise.
    verbatim: PathBuf,
    restore: Vec<Box<dyn FnOnce()>>,
}

impl TempTree {
    fn new(tag: &str) -> Self {
        let plain = std::env::temp_dir().join(format!("strata-walk-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&plain);
        fs::create_dir_all(&plain).expect("create temp root");
        let verbatim = fs::canonicalize(&plain).expect("canonicalize");
        let plain = PathBuf::from(
            verbatim
                .to_string_lossy()
                .strip_prefix(r"\\?\")
                .expect("verbatim")
                .to_owned(),
        );
        Self {
            plain,
            verbatim,
            restore: Vec::new(),
        }
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
}

impl Drop for TempTree {
    fn drop(&mut self) {
        for r in self.restore.drain(..).rev() {
            r();
        }
        let _ = fs::remove_dir_all(&self.verbatim);
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

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Indexed view over a walk's output. Construction checks the structural
/// invariants every walk must satisfy: unique ids, one self-linked root, and
/// every link's parent present.
struct View<'a> {
    by_id: HashMap<FileRef, &'a ScanRecord>,
    children: HashMap<FileRef, Vec<(&'a [u16], &'a ScanRecord)>>,
    root: &'a ScanRecord,
}

impl<'a> View<'a> {
    fn new(recs: &'a [ScanRecord]) -> Self {
        let mut by_id = HashMap::new();
        for r in recs {
            assert!(by_id.insert(r.id, r).is_none(), "duplicate id {:?}", r.id);
        }
        let roots: Vec<_> = recs
            .iter()
            .filter(|r| r.links.iter().any(|l| l.parent == r.id))
            .collect();
        assert_eq!(roots.len(), 1, "exactly one self-linked root");
        let mut children: HashMap<FileRef, Vec<(&[u16], &ScanRecord)>> = HashMap::new();
        for r in recs {
            assert!(!r.links.is_empty());
            for l in &r.links {
                assert!(
                    by_id.contains_key(&l.parent),
                    "dangling parent for {:?}",
                    l.name
                );
                if l.parent != r.id {
                    children
                        .entry(l.parent)
                        .or_default()
                        .push((l.name.units(), r));
                }
            }
        }
        Self {
            by_id,
            children,
            root: roots[0],
        }
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

fn cluster(stats: &WalkStats) -> u64 {
    stats.volume.as_ref().expect("volume facts").cluster_size
}

fn handle_of(p: &Path) -> std::os::windows::io::OwnedHandle {
    let wide: Vec<u16> = p.as_os_str().encode_wide().collect();
    sys::nt_open(
        None,
        &crate::path::to_nt(&crate::path::to_extended(&wide)),
        OpenMode::Attributes,
        false,
    )
    .expect("open fixture")
}

// -----------------------------------------------------------------------------
// Structure, sizes and identity
// -----------------------------------------------------------------------------

#[test]
fn nested_tree_has_exact_records_in_every_mode() {
    let t = TempTree::new("nested");
    t.file(r"a\b\c\deep.bin", 100);
    t.file(r"a\mid.bin", 5000);
    t.file(r"a\b\twelve.bin", 12288);
    t.file("top.txt", 0);
    t.dir("empty");

    for (method, alloc) in MODES {
        let (recs, stats) = walk(&t.plain, opts(method, alloc));
        let v = View::new(&recs);
        let c = cluster(&stats);
        assert_eq!(c, 4096, "fixtures assume 4 KiB clusters");
        assert_eq!(recs.len(), 9, "{method:?}/{alloc}");
        assert_eq!((stats.totals.dirs, stats.totals.files), (5, 4));
        assert!(!stats.partial && !stats.cancelled);
        assert_eq!(stats.errors.total(), 0, "{:?}", stats.errors);

        assert!(v.root.is_dir());
        assert_eq!(
            v.root.links[0].name.to_string_lossy(),
            t.plain.to_string_lossy()
        );
        for d in ["a", r"a\b", r"a\b\c", "empty"] {
            assert!(v.get(d).is_dir(), "{d}");
        }
        assert!(v.children(v.get("empty").id).is_empty());

        let exact = method == ListingMethod::DirectoryInfo || alloc;
        let expect = [
            (r"a\b\c\deep.bin", 100, if exact { 0 } else { 4096 }),
            (r"a\mid.bin", 5000, 8192),
            (r"a\b\twelve.bin", 12288, 12288),
            ("top.txt", 0, 0),
        ];
        for (path, logical, allocated) in expect {
            let r = v.get(path);
            assert!(!r.is_dir());
            assert_eq!(r.sizes.logical, logical, "{path} {method:?}/{alloc}");
            assert_eq!(r.sizes.allocated, allocated, "{path} {method:?}/{alloc}");
            assert_eq!(
                r.flags.contains(EntryFlags::ALLOC_ESTIMATED),
                !exact,
                "{path} {method:?}/{alloc}"
            );
            assert!(r.ads.is_empty());
        }
        assert_eq!(stats.estimated_allocations, if exact { 0 } else { 4 });
        assert_eq!(
            stats.totals.logical_bytes,
            100 + 5000 + 12288,
            "logical totals"
        );

        let real_ids = method == ListingMethod::DirectoryInfo || alloc;
        for r in &recs {
            if r.is_dir() && method == ListingMethod::FindFirstFile && !alloc {
                assert!(r.id.is_synthetic());
            } else {
                assert_eq!(!r.id.is_synthetic(), real_ids, "{:?}", r.links[0].name);
            }
            if r.is_dir() {
                assert!(r.sizes.dir_overhead.is_multiple_of(c));
            }
        }
        if real_ids {
            let mid = v.get(r"a\mid.bin");
            let id = sys::file_id(&handle_of(&t.p(r"a\mid.bin"))).expect("id");
            assert_eq!(u128::from(mid.id.0), id, "record id is the NTFS file id");
        }
    }
}

#[test]
fn directory_index_overhead_is_reported() {
    let t = TempTree::new("overhead");
    for i in 0..2000 {
        t.file(&format!(r"big\a-reasonably-long-file-name-{i:05}.txt"), 1);
    }
    let (recs, stats) = walk(&t.plain, opts(ListingMethod::DirectoryInfo, true));
    let v = View::new(&recs);
    let big = v.get("big");
    assert!(
        big.sizes.dir_overhead >= 64 * 1024,
        "{}",
        big.sizes.dir_overhead
    );
    assert!(big.sizes.dir_overhead.is_multiple_of(cluster(&stats)));
    assert_eq!(big.sizes.total_allocated(), big.sizes.dir_overhead);
}

#[test]
fn long_paths_and_deep_nesting() {
    let t = TempTree::new("deep");
    let mut long = String::new();
    for i in 0..15 {
        long.push_str(&format!(r"component-number-{i:02}-xxxxx\"));
    }
    t.file(&format!("{long}leaf.txt"), 3);
    assert!(t.p(&long).as_os_str().len() > 400);

    let mut deep = t.p("d");
    for _ in 1..1100 {
        deep.push("d");
    }
    fs::create_dir_all(&deep).expect("deep dirs");
    fs::write(deep.join("bottom.txt"), b"1234").expect("bottom");

    for method in [ListingMethod::DirectoryInfo, ListingMethod::FindFirstFile] {
        let (recs, stats) = walk(&t.plain, opts(method, true));
        assert_eq!(stats.errors.total(), 0, "{method:?} {:?}", stats.errors);
        let v = View::new(&recs);
        assert_eq!(v.get(&format!("{long}leaf.txt")).sizes.logical, 3);
        let mut cur = v.root;
        let mut depth = 0;
        while let Some(next) = v.child(cur.id, &w("d")) {
            cur = next;
            depth += 1;
        }
        assert_eq!(depth, 1100, "{method:?}");
        let bottom = v.child(cur.id, &w("bottom.txt")).expect("bottom.txt");
        assert_eq!(bottom.sizes.logical, 4);
    }
}

#[test]
fn unusual_names_survive_verbatim() {
    let t = TempTree::new("names");
    let names: Vec<Vec<u16>> = [
        "trailing.",
        "trailing ",
        "CON",
        "NUL.txt",
        "AUX",
        "COM1",
        "lpt1.log",
        " leading",
        "emoji 📁😀",
        "RTL שלום عربي",
        "Mixed.Case",
    ]
    .iter()
    .map(|s| w(s))
    .chain([vec![u16::from(b'a'), 0xD800, u16::from(b'b')], vec![0xDC00]])
    .collect();
    for (i, n) in names.iter().enumerate() {
        let p = t.verbatim.join(OsString::from_wide(n));
        fs::write(&p, vec![0u8; i + 1]).expect("create unusual name");
    }
    let con_dir = t.verbatim.join("PRN");
    fs::create_dir(&con_dir).expect("PRN dir");
    fs::write(con_dir.join("inside."), b"ok").expect("file in PRN");

    for (method, alloc) in MODES {
        let (recs, stats) = walk(&t.plain, opts(method, alloc));
        assert_eq!(stats.errors.total(), 0, "{method:?} {:?}", stats.errors);
        let v = View::new(&recs);
        for (i, n) in names.iter().enumerate() {
            let r = v.child(v.root.id, n).unwrap_or_else(|| {
                panic!("{:?} missing ({method:?})", String::from_utf16_lossy(n))
            });
            assert_eq!(r.sizes.logical, i as u64 + 1);
            assert!(!r.flags.contains(EntryFlags::ALLOC_ESTIMATED) || !alloc);
        }
        let prn = v.child(v.root.id, &w("PRN")).expect("PRN");
        assert!(prn.is_dir());
        assert_eq!(
            v.child(prn.id, &w("inside."))
                .expect("inside.")
                .sizes
                .logical,
            2
        );
        assert!(
            v.child(v.root.id, &[0xDC00]).expect("lone").links[0]
                .name
                .has_unpaired_surrogate()
        );
    }
}

#[test]
fn case_sensitive_directory_keeps_both_names() {
    let t = TempTree::new("case");
    let d = t.dir("cs");
    let plain = t.plain.join("cs");
    if let Err(e) = run_cmd(
        "fsutil.exe",
        &[
            "file".as_ref(),
            "setCaseSensitiveInfo".as_ref(),
            plain.as_os_str(),
            "enable".as_ref(),
        ],
    ) {
        eprintln!(
            "skipping: cannot enable per-directory case sensitivity: {}",
            e.trim()
        );
        return;
    }
    fs::write(d.join("A.txt"), b"1").expect("A.txt");
    fs::write(d.join("a.txt"), b"22").expect("a.txt");
    for (method, alloc) in MODES {
        let (recs, _) = walk(&t.plain, opts(method, alloc));
        let v = View::new(&recs);
        assert_eq!(v.get(r"cs\A.txt").sizes.logical, 1, "{method:?}/{alloc}");
        assert_eq!(v.get(r"cs\a.txt").sizes.logical, 2, "{method:?}/{alloc}");
    }
}

// -----------------------------------------------------------------------------
// Reparse points
// -----------------------------------------------------------------------------

#[test]
fn junctions_are_recorded_not_followed_even_in_cycles() {
    let t = TempTree::new("junction");
    t.file(r"target\inside.txt", 10);
    let mk = |link: &str, target: &Path| {
        run_cmd(
            "cmd",
            &[
                "/C".as_ref(),
                "mklink".as_ref(),
                "/J".as_ref(),
                t.plain.join(link).as_os_str(),
                target.as_os_str(),
            ],
        )
        .expect("mklink /J");
    };
    mk("loop", &t.plain);
    mk(r"target\up", &t.plain);
    mk("tojunk", &t.plain.join("target"));

    for (method, alloc) in MODES {
        let (recs, stats) = walk(&t.plain, opts(method, alloc));
        let v = View::new(&recs);
        assert_eq!(
            recs.len(),
            6,
            "{method:?}/{alloc}: root, target, inside, 3 junctions"
        );
        assert_eq!(stats.errors.total(), 0);
        for (name, target) in [
            ("loop", &t.plain),
            (r"target\up", &t.plain),
            ("tojunk", &t.plain.join("target")),
        ] {
            let j = v.get(name);
            assert!(j.is_dir());
            assert_eq!(j.flags.reparse(), ReparseKind::MountPoint, "{name}");
            let rp = j.reparse.as_ref().expect("reparse info");
            assert_eq!(rp.tag, win32::IO_REPARSE_TAG_MOUNT_POINT);
            assert_eq!(
                rp.target.as_ref().map(|t| t.to_string_lossy()),
                Some(target.to_string_lossy().into_owned()),
                "{name}"
            );
            assert!(v.children(j.id).is_empty(), "{name} must not be traversed");
        }
    }
}

#[test]
fn symlinks_are_recorded_not_followed() {
    let t = TempTree::new("symlink");
    t.file(r"real\data.bin", 5000);
    let file_link = t.plain.join("file-link");
    let dir_link = t.plain.join("dir-link");
    if let Err(e) = std::os::windows::fs::symlink_file(t.plain.join(r"real\data.bin"), &file_link) {
        eprintln!(
            "skipping: cannot create symlinks without Developer Mode or SeCreateSymbolicLinkPrivilege: {e}"
        );
        return;
    }
    std::os::windows::fs::symlink_dir(t.plain.join("real"), &dir_link).expect("dir symlink");

    for (method, alloc) in MODES {
        let (recs, _) = walk(&t.plain, opts(method, alloc));
        let v = View::new(&recs);
        let f = v.get("file-link");
        assert_eq!(f.flags.reparse(), ReparseKind::Symlink);
        assert_eq!(f.sizes.logical, 0, "a symlink has no data of its own");
        assert_eq!(
            f.reparse
                .as_ref()
                .and_then(|r| r.target.as_ref())
                .map(|n| n.to_string_lossy()),
            Some(
                t.plain
                    .join(r"real\data.bin")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        let d = v.get("dir-link");
        assert!(d.is_dir());
        assert_eq!(d.flags.reparse(), ReparseKind::Symlink);
        assert!(v.children(d.id).is_empty());
    }
}

// -----------------------------------------------------------------------------
// Hardlinks
// -----------------------------------------------------------------------------

#[test]
fn hardlinks_merge_into_one_record() {
    let t = TempTree::new("hardlink");
    let outside = TempTree::new("hardlink-outside");
    let original = t.file(r"A\f.bin", 10_000);
    t.dir("B");
    t.dir("C");
    t.dir("D");
    fs::hard_link(&original, t.p(r"B\f2.bin")).expect("link 2");
    fs::hard_link(&original, t.p(r"C\f3.bin")).expect("link 3");
    let ext = outside.file("ext.bin", 3000);
    fs::hard_link(&ext, t.p(r"D\ext-link.bin")).expect("external link");

    for method in [ListingMethod::DirectoryInfo, ListingMethod::FindFirstFile] {
        let (recs, stats) = walk(&t.plain, opts(method, true));
        let v = View::new(&recs);
        assert_eq!(stats.totals.files, 2, "{method:?}");
        assert_eq!(stats.hardlinks_merged, 2);
        let f = v.get(r"A\f.bin");
        assert!(!f.id.is_synthetic());
        assert_eq!(v.get(r"B\f2.bin").id, f.id);
        assert_eq!(v.get(r"C\f3.bin").id, f.id);
        let mut names: Vec<String> = f.links.iter().map(|l| l.name.to_string_lossy()).collect();
        names.sort();
        assert_eq!(names, ["f.bin", "f2.bin", "f3.bin"]);
        let parents: HashSet<FileRef> = f.links.iter().map(|l| l.parent).collect();
        let expected: HashSet<FileRef> = ["A", "B", "C"].iter().map(|d| v.get(d).id).collect();
        assert_eq!(parents, expected);
        assert_eq!(f.sizes.logical, 10_000);
        assert_eq!(f.sizes.allocated, 12_288);

        let e = v.get(r"D\ext-link.bin");
        assert_eq!(e.links.len(), 1, "the outside link is never seen");
        assert_eq!(e.sizes.logical, 3000);
    }

    let (recs, stats) = walk(&t.plain, opts(ListingMethod::DirectoryInfo, false));
    let v = View::new(&recs);
    assert_eq!(stats.totals.files, 4);
    let sightings = [r"A\f.bin", r"B\f2.bin", r"C\f3.bin"].map(|p| v.get(p));
    let primary: Vec<_> = sightings.iter().filter(|r| !r.id.is_synthetic()).collect();
    assert_eq!(primary.len(), 1, "one sighting keeps the real id");
    assert!(!primary[0].flags.contains(EntryFlags::HARDLINK_SECONDARY));
    let secondary: Vec<_> = sightings.iter().filter(|r| r.id.is_synthetic()).collect();
    assert_eq!(secondary.len(), 2);
    assert!(
        secondary
            .iter()
            .all(|r| r.flags.contains(EntryFlags::HARDLINK_SECONDARY))
    );
}

// -----------------------------------------------------------------------------
// Sparse, compressed, WOF and alternate data streams
// -----------------------------------------------------------------------------

#[test]
fn sparse_file_reports_real_allocation() {
    let t = TempTree::new("sparse");
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(t.p("sparse.bin"))
            .expect("create");
        sys::fixture::set_sparse(&f).expect("FSCTL_SET_SPARSE");
        f.seek(SeekFrom::Start(100 << 20)).expect("seek");
        f.write_all(b"z").expect("write");
    }
    for (method, alloc) in MODES {
        let (recs, _) = walk(&t.plain, opts(method, alloc));
        let r = View::new(&recs).get("sparse.bin");
        assert!(r.flags.contains(EntryFlags::SPARSE));
        assert_eq!(r.sizes.logical, (100 << 20) + 1);
        if method == ListingMethod::DirectoryInfo || alloc {
            assert_eq!(
                r.sizes.allocated,
                64 * 1024,
                "one 64 KiB sparse unit, {method:?}/{alloc}"
            );
        } else {
            assert!(r.flags.contains(EntryFlags::ALLOC_ESTIMATED));
        }
    }
}

#[test]
fn compressed_file_reports_compressed_allocation() {
    let t = TempTree::new("compressed");
    let p = t.p("packed.txt");
    {
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&p)
            .expect("create");
        sys::fixture::set_compressed(&f).expect("FSCTL_SET_COMPRESSION");
        let chunk = b"strata walker compression fixture line\r\n".repeat(26_000);
        (&f).write_all(&chunk).expect("write");
        f.sync_all().expect("flush");
    }
    let on_disk = sys::compressed_size(&handle_of(&p)).expect("compressed size");
    let logical = fs::metadata(&p).expect("meta").len();
    assert!(
        on_disk > 0 && on_disk < logical / 4,
        "fixture compresses: {on_disk}/{logical}"
    );
    for (method, alloc) in MODES {
        let (recs, _) = walk(&t.plain, opts(method, alloc));
        let r = View::new(&recs).get("packed.txt");
        assert!(r.flags.contains(EntryFlags::COMPRESSED));
        assert_eq!(r.sizes.logical, logical);
        if method == ListingMethod::DirectoryInfo || alloc {
            assert_eq!(r.sizes.allocated, on_disk, "{method:?}/{alloc}");
        }
    }
}

#[test]
fn wof_compressed_file_reports_real_allocation() {
    let t = TempTree::new("wof");
    let p = t.p("app.exe");
    fs::write(&p, b"walker wof fixture ".repeat(60_000)).expect("write");
    let plain = t.plain.join("app.exe");
    if let Err(e) = run_cmd(
        "compact",
        &["/c".as_ref(), "/exe:xpress4k".as_ref(), plain.as_os_str()],
    ) {
        eprintln!("skipping: compact /exe failed: {}", e.trim());
        return;
    }
    let logical = fs::metadata(&p).expect("meta").len();
    let on_disk = sys::standard_info(&handle_of(&p)).expect("std").allocation;
    assert!(
        on_disk > 0 && on_disk < logical / 4,
        "fixture is WOF-compressed: {on_disk}/{logical}"
    );

    for method in [ListingMethod::DirectoryInfo, ListingMethod::FindFirstFile] {
        let (recs, _) = walk(&t.plain, opts(method, true));
        let r = View::new(&recs).get("app.exe");
        assert_eq!(r.sizes.logical, logical);
        assert_eq!(r.sizes.allocated, on_disk, "{method:?}");
        assert!(!r.flags.contains(EntryFlags::ALLOC_ESTIMATED));
        assert!(r.ads.is_empty(), "WofCompressedData is never an ADS");
    }
    let (recs, _) = walk(&t.plain, opts(ListingMethod::DirectoryInfo, false));
    let r = View::new(&recs).get("app.exe");
    assert!(
        r.flags.contains(EntryFlags::ALLOC_ESTIMATED),
        "listings hide WOF allocation; without the pass it is an estimate"
    );
}

#[test]
fn alternate_data_streams_are_enumerated() {
    let t = TempTree::new("ads");
    let f = t.file("host.txt", 4);
    let stream = |base: &Path, s: &str, len: usize| {
        let mut name = base.as_os_str().to_owned();
        name.push(format!(":{s}"));
        fs::write(PathBuf::from(name), vec![b'q'; len]).expect("write stream");
    };
    stream(&f, "big", 5000);
    stream(&f, "empty", 0);
    stream(&f, "Zone.Identifier", 26);
    let d = t.dir("dir-with-stream");
    stream(&d, "dirstream", 9000);

    for method in [ListingMethod::DirectoryInfo, ListingMethod::FindFirstFile] {
        let (recs, _) = walk(&t.plain, opts(method, true));
        let v = View::new(&recs);
        let r = v.get("host.txt");
        assert!(r.flags.contains(EntryFlags::HAS_ADS));
        let mut ads: Vec<(String, u64, u64)> = r
            .ads
            .iter()
            .map(|a| (a.name.to_string_lossy(), a.logical, a.allocated))
            .collect();
        ads.sort();
        assert_eq!(
            ads,
            [
                ("Zone.Identifier".to_owned(), 26, 0),
                ("big".to_owned(), 5000, 8192),
                ("empty".to_owned(), 0, 0),
            ],
            "{method:?}"
        );
        assert_eq!((r.sizes.ads_logical, r.sizes.ads_allocated), (5026, 8192));
        assert_eq!(
            r.sizes.total_allocated(),
            8192,
            "4-byte main stream is resident"
        );

        let dr = v.get("dir-with-stream");
        assert_eq!(dr.ads.len(), 1);
        assert_eq!(
            (dr.sizes.ads_logical, dr.sizes.ads_allocated),
            (9000, 12_288)
        );
    }
    let (recs, _) = walk(&t.plain, opts(ListingMethod::DirectoryInfo, false));
    assert!(View::new(&recs).get("host.txt").ads.is_empty());
}

// -----------------------------------------------------------------------------
// Errors and races
// -----------------------------------------------------------------------------

fn current_user() -> String {
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    let user = std::env::var("USERNAME").expect("USERNAME");
    if domain.is_empty() {
        user
    } else {
        format!(r"{domain}\{user}")
    }
}

#[test]
fn access_denied_directory_is_flagged_and_walk_continues() {
    let mut t = TempTree::new("denied");
    t.file(r"locked\secret.txt", 50);
    t.file(r"locked\sub\more.txt", 50);
    t.file(r"open\visible.txt", 7);
    let locked = t.plain.join("locked");
    let grant = format!("{}:(RD)", current_user());
    run_cmd(
        "icacls",
        &[locked.as_os_str(), "/deny".as_ref(), grant.as_ref()],
    )
    .expect("icacls deny");
    let (l2, user) = (locked.clone(), current_user());
    t.restore.push(Box::new(move || {
        let _ = run_cmd(
            "icacls",
            &[l2.as_os_str(), "/remove:d".as_ref(), user.as_ref()],
        );
    }));

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
        assert!(stats.partial && !stats.cancelled);
        assert_eq!(recs.len(), 4);
    }
}

fn hooked(root: &Path, o: WalkOptions, hooks: Hooks) -> (Vec<ScanRecord>, WalkStats) {
    let mut walker = Walker::new(root, o).expect("walker");
    walker.hooks = hooks;
    walk_with(&walker)
}

fn ends_with(path: &[u16], suffix: &str) -> bool {
    path.ends_with(&w(suffix))
}

#[test]
fn entries_changing_mid_scan_are_handled() {
    for (method, alloc) in MODES {
        let t = TempTree::new("race");
        t.file(r"victim\child.txt", 1);
        t.file(r"gone\a\b.txt", 1);
        t.file(r"mixed\keep.txt", 11);
        t.file(r"mixed\del.txt", 12);
        t.file("stays.txt", 13);
        let root = t.verbatim.clone();
        let r2 = root.clone();
        let hooks = Hooks {
            before_list: Some(Arc::new(move |p: &[u16]| {
                if ends_with(p, r"\victim") {
                    fs::remove_dir_all(r2.join("victim")).expect("remove victim");
                    fs::write(r2.join("victim"), vec![0u8; 777]).expect("replace with file");
                } else if ends_with(p, r"\gone") {
                    fs::remove_dir_all(r2.join("gone")).expect("remove gone");
                }
            })),
            before_refine: Some(Arc::new(move |p: &[u16]| {
                if ends_with(p, r"\mixed") {
                    let _ = fs::remove_file(root.join(r"mixed\del.txt"));
                }
            })),
        };
        let (recs, stats) = hooked(&t.plain, opts(method, alloc), hooks);
        let v = View::new(&recs);
        let victim = v.get("victim");
        assert!(
            !victim.is_dir(),
            "{method:?}/{alloc}: replaced directory reported as file"
        );
        assert_eq!(victim.sizes.logical, 777);
        assert_eq!(stats.errors.get(ErrorKind::ReplacedByFile), 1);
        assert!(v.try_get("gone").is_none(), "vanished directory is dropped");
        assert!(stats.errors.get(ErrorKind::Vanished) >= 1);
        assert_eq!(v.get(r"mixed\keep.txt").sizes.logical, 11);
        assert_eq!(v.get("stays.txt").sizes.logical, 13);
        if alloc {
            assert!(
                v.try_get(r"mixed\del.txt").is_none(),
                "file deleted before probing is dropped"
            );
        }
    }
}

#[test]
fn root_errors() {
    let t = TempTree::new("rooterr");
    let file = t.file("plain.txt", 1);
    let missing = t.plain.join("nope");
    let mut recs = Vec::new();
    let cancel = CancelToken::new();
    assert!(matches!(
        Walker::new(&missing, WalkOptions::default())
            .expect("w")
            .run(&mut recs, &cancel),
        Err(WalkError::RootNotFound(_))
    ));
    assert!(matches!(
        Walker::new(&file, WalkOptions::default())
            .expect("w")
            .run(&mut recs, &cancel),
        Err(WalkError::NotADirectory(_))
    ));
    assert!(recs.is_empty());
}

// -----------------------------------------------------------------------------
// Cancellation, progress and sinks
// -----------------------------------------------------------------------------

fn wide_tree(tag: &str, dirs: usize, files: usize) -> TempTree {
    let t = TempTree::new(tag);
    for d in 0..dirs {
        for f in 0..files {
            t.file(&format!(r"d{d:03}\f{f:03}.txt"), f);
        }
    }
    t
}

#[test]
fn cancellation_flags_partial_and_keeps_structure() {
    let t = wide_tree("cancel", 60, 20);
    for method in [ListingMethod::DirectoryInfo, ListingMethod::FindFirstFile] {
        let cancel = CancelToken::new();
        let seen = Arc::new(AtomicUsize::new(0));
        let (c2, s2) = (cancel.clone(), Arc::clone(&seen));
        let mut walker = Walker::new(&t.plain, opts(method, true)).expect("walker");
        walker.hooks.before_list = Some(Arc::new(move |_: &[u16]| {
            if s2.fetch_add(1, Ordering::SeqCst) == 10 {
                c2.cancel();
            }
        }));
        let mut recs = Vec::new();
        let stats = walker.run(&mut recs, &cancel).expect("walk");
        let v = View::new(&recs);
        assert!(stats.cancelled && stats.partial, "{method:?}");
        assert!(stats.partial_dirs > 0);
        assert!(
            recs.iter()
                .any(|r| r.is_dir() && r.flags.contains(EntryFlags::PARTIAL))
        );
        assert!(stats.totals.files < 1200, "walk stopped early");
        assert_eq!(
            v.children(v.root.id).len(),
            60,
            "the root listing itself completed"
        );
    }

    let cancel = CancelToken::new();
    cancel.cancel();
    let mut recs = Vec::new();
    let stats = Walker::new(&t.plain, WalkOptions::default())
        .expect("walker")
        .run(&mut recs, &cancel)
        .expect("walk");
    assert_eq!(recs.len(), 1);
    assert!(recs[0].flags.contains(EntryFlags::PARTIAL));
    assert!(stats.cancelled);
}

#[test]
fn sinks_receive_batches_and_progress() {
    let t = wide_tree("sink", 30, 30);
    let o = WalkOptions {
        batch_size: 50,
        progress_interval: std::time::Duration::ZERO,
        ..opts(ListingMethod::DirectoryInfo, true)
    };
    let walker = Walker::new(&t.plain, o).expect("walker");

    let (mut batches, mut total, mut progress) = (0usize, 0usize, Vec::new());
    let mut sink = FnSink::with_progress(
        |b: Vec<ScanRecord>| {
            batches += 1;
            total += b.len();
        },
        |p: &crate::Progress| progress.push(*p),
    );
    let stats = walker.run(&mut sink, &CancelToken::new()).expect("walk");
    assert_eq!(total, 1 + 30 + 900);
    assert!(batches > 1);
    assert_eq!(progress.last().expect("final progress").files, 900);
    assert_eq!(stats.totals.files, 900);
    assert_eq!(stats.totals.dirs, 31);
    assert!(progress.windows(2).all(|p| p[0].files <= p[1].files));

    let (tx, rx) = crossbeam_channel::unbounded();
    walker
        .run(&mut ChannelSink::new(tx), &CancelToken::new())
        .expect("walk");
    let n: usize = rx
        .try_iter()
        .map(|e| match e {
            WalkEvent::Records(b) => b.len(),
            WalkEvent::Progress(_) => 0,
        })
        .sum();
    assert_eq!(n, 931);
}

#[test]
fn listing_methods_agree_after_allocation_pass() {
    let t = wide_tree("agree", 10, 25);
    t.file(r"x\y\z\deep.bin", 70_000);
    let key = |r: &ScanRecord, v: &View<'_>| {
        let mut path = Vec::new();
        let mut cur = r;
        while cur.id != v.root.id {
            path.push(cur.links[0].name.to_string_lossy());
            cur = v.by_id[&cur.links[0].parent];
        }
        path.reverse();
        (
            path.join("\\"),
            r.sizes,
            r.flags.0 & !EntryFlags::ALLOC_ESTIMATED.0,
            r.id,
        )
    };
    let collect = |m| {
        let (recs, _) = walk(&t.plain, opts(m, true));
        let v = View::new(&recs);
        let mut k: Vec<_> = recs.iter().map(|r| key(r, &v)).collect();
        k.sort_by(|a, b| a.0.cmp(&b.0));
        k
    };
    let a = collect(ListingMethod::DirectoryInfo);
    let b = collect(ListingMethod::FindFirstFile);
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.0, y.0);
        assert_eq!(x.1, y.1, "{}", x.0);
        assert_eq!(x.2, y.2, "{}", x.0);
        if !x.0.is_empty() {
            assert_eq!(x.3, y.3, "{}: both methods report the NTFS file id", x.0);
        }
    }
}

#[test]
fn network_share_walk_uses_bounded_timed_requests() {
    let t = wide_tree("unc", 5, 5);
    let local = t.plain.to_string_lossy().into_owned();
    let Some(rest) = local.strip_prefix("C:\\") else {
        eprintln!("skipping: temp dir is not on C:");
        return;
    };
    let unc = PathBuf::from(format!(r"\\localhost\C$\{rest}"));
    if fs::metadata(&unc).is_err() {
        eprintln!("skipping: the \\\\localhost\\C$ admin share is not reachable unelevated");
        return;
    }
    let o = WalkOptions {
        network_concurrency: 2,
        timeout: std::time::Duration::from_secs(20),
        ..opts(ListingMethod::DirectoryInfo, true)
    };
    let (recs, stats) = walk(&unc, o);
    let v = View::new(&recs);
    assert!(stats.volume.as_ref().is_some_and(|v| v.is_network));
    assert_eq!(stats.totals.files, 25);
    assert!(
        recs.iter().all(|r| r.id.is_synthetic()),
        "SMB ids are not trusted"
    );
    assert_eq!(v.get(r"d004\f004.txt").sizes.logical, 4);
}

#[test]
fn every_directory_information_class_parses_real_listings() {
    use crate::parse::DirInfoClass;
    let t = TempTree::new("classes");
    t.file("a.txt", 10);
    t.file(r"sub\b.bin", 5000);
    t.file("emoji 😀.dat", 3);
    run_cmd(
        "cmd",
        &[
            "/C".as_ref(),
            "mklink".as_ref(),
            "/J".as_ref(),
            t.plain.join("junc").as_os_str(),
            t.plain.join("sub").as_os_str(),
        ],
    )
    .expect("mklink /J");
    let ext = crate::path::to_extended(&t.verbatim.as_os_str().encode_wide().collect::<Vec<_>>());
    let list = |class| {
        let l = crate::walker::list_dir(
            &ext,
            ListingMethod::DirectoryInfo,
            class,
            false,
            false,
            &CancelToken::new(),
        )
        .expect("list");
        assert_eq!(l.class, class, "NTFS supports every class");
        assert!(l.complete);
        let mut e = l.entries;
        e.sort_by(|a, b| a.name.cmp(&b.name));
        e
    };
    let extd = list(DirInfoClass::IdExtd);
    let both = list(DirInfoClass::IdBoth);
    let full = list(DirInfoClass::Full);
    assert_eq!(extd.len(), 4);
    for ((x, b), f) in extd.iter().zip(&both).zip(&full) {
        for other in [b, f] {
            assert_eq!(x.name, other.name);
            assert_eq!(x.attributes, other.attributes);
            assert_eq!(x.reparse_tag, other.reparse_tag);
            assert_eq!((x.logical, x.allocated), (other.logical, other.allocated));
            assert_eq!(x.times, other.times);
        }
        assert_eq!(x.file_id.map(|i| i as u64), b.file_id.map(|i| i as u64));
        assert!(x.file_id.is_some());
        assert_eq!(f.file_id, None);
    }
    let junc = extd.iter().find(|e| e.name == w("junc")).expect("junc");
    assert_eq!(junc.reparse_tag, win32::IO_REPARSE_TAG_MOUNT_POINT);
}

#[test]
fn junction_given_as_root_is_followed() {
    let t = TempTree::new("rootjunc");
    t.file(r"real\inside.txt", 10);
    t.file(r"real\sub\deeper.txt", 20);
    let link = t.plain.join("link");
    run_cmd(
        "cmd",
        &[
            "/C".as_ref(),
            "mklink".as_ref(),
            "/J".as_ref(),
            link.as_os_str(),
            t.plain.join("real").as_os_str(),
        ],
    )
    .expect("mklink /J");
    for (method, alloc) in MODES {
        let (recs, _) = walk(&link, opts(method, alloc));
        let v = View::new(&recs);
        assert_eq!(recs.len(), 4, "{method:?}/{alloc}");
        assert_eq!(v.root.flags.reparse(), ReparseKind::None);
        assert_eq!(v.get(r"sub\deeper.txt").sizes.logical, 20);
    }
}
