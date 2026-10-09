//! Classification throughput on a synthetic ~5M-entry tree.
//!
//! Run with `cargo run --release -p strata-classify --example bench_classify`.
//! The tree mixes the shapes that matter for the engine: deep dependency
//! folders (node_modules), package caches, browser profiles, a large Windows
//! tree, a Steam library and plain user data on a second drive.

use std::time::Instant;

use rayon::prelude::*;
use strata_classify::{ChildRef, Classifier, DirScope, Entry, Name, RuleSet};
use strata_core::known::{KnownFolder, KnownFolders, UserFolders};
use strata_core::{EntryFlags, FileTime};

struct Tree {
    names: Vec<String>,
    is_dir: Vec<bool>,
    children: Vec<Vec<u32>>,
}

impl Tree {
    fn new() -> Self {
        Self {
            names: vec![String::new()],
            is_dir: vec![true],
            children: vec![Vec::new()],
        }
    }

    fn add(&mut self, parent: u32, name: String, dir: bool) -> u32 {
        let id = u32::try_from(self.names.len()).unwrap();
        self.names.push(name);
        self.is_dir.push(dir);
        self.children.push(Vec::new());
        self.children[parent as usize].push(id);
        id
    }

    fn path(&mut self, parent: u32, parts: &[&str]) -> u32 {
        let mut cur = parent;
        for p in parts {
            let existing = self.children[cur as usize]
                .iter()
                .copied()
                .find(|&c| self.names[c as usize].eq_ignore_ascii_case(p));
            cur = match existing {
                Some(c) => c,
                None => self.add(cur, (*p).to_string(), true),
            };
        }
        cur
    }

    fn len(&self) -> usize {
        self.names.len() - 1
    }
}

fn fill(t: &mut Tree, dir: u32, depth: u32, fanout: u32, files: u32, prefix: &str, ext: &str) {
    for i in 0..files {
        t.add(dir, format!("{prefix}{i}.{ext}"), false);
    }
    if depth == 0 {
        return;
    }
    for i in 0..fanout {
        let d = t.add(dir, format!("{prefix}d{i}"), true);
        fill(t, d, depth - 1, fanout, files, prefix, ext);
    }
}

fn build() -> Tree {
    let mut t = Tree::new();
    let c = t.add(0, "C:".into(), true);
    let d = t.add(0, "D:".into(), true);

    // ~1.1M under Windows.
    let sys = t.path(c, &["Windows", "System32"]);
    fill(&mut t, sys, 4, 9, 15, "sys", "dll");
    let sxs = t.path(c, &["Windows", "WinSxS"]);
    fill(&mut t, sxs, 3, 20, 20, "amd64_", "manifest");

    // ~2.4M in 240 projects with node_modules.
    let src = t.path(c, &["Users", "alice", "source"]);
    for p in 0..240 {
        let proj = t.add(src, format!("proj{p}"), true);
        t.add(proj, "package.json".into(), false);
        t.add(proj, "README.md".into(), false);
        let srcdir = t.add(proj, "src".into(), true);
        fill(&mut t, srcdir, 1, 5, 10, "mod", "ts");
        let nm = t.add(proj, "node_modules".into(), true);
        for k in 0..120 {
            let pkg = t.add(nm, format!("pkg-{k}"), true);
            t.add(pkg, "package.json".into(), false);
            let lib = t.add(pkg, "lib".into(), true);
            fill(&mut t, lib, 1, 3, 15, "f", "js");
        }
        if p % 3 == 0 {
            let dist = t.add(proj, "dist".into(), true);
            fill(&mut t, dist, 1, 4, 20, "chunk", "js");
        }
    }

    // ~0.5M in caches.
    let npm = t.path(
        c,
        &[
            "Users",
            "alice",
            "AppData",
            "Local",
            "npm-cache",
            "_cacache",
            "content-v2",
        ],
    );
    fill(&mut t, npm, 2, 50, 70, "x", "bin");
    let chrome = t.path(
        c,
        &[
            "Users",
            "alice",
            "AppData",
            "Local",
            "Google",
            "Chrome",
            "User Data",
        ],
    );
    for prof in ["Default", "Profile 1", "Profile 2"] {
        let pr = t.path(chrome, &[prof]);
        for sub in [
            "Cache",
            "Code Cache",
            "GPUCache",
            "Local Storage",
            "IndexedDB",
        ] {
            let s = t.add(pr, sub.into(), true);
            fill(&mut t, s, 1, 10, 600, "data_", "bin");
        }
        t.add(pr, "History".into(), false);
        t.add(pr, "Preferences".into(), false);
    }

    // ~0.4M in a Steam library.
    let common = t.path(d, &["SteamLibrary", "steamapps", "common"]);
    for g in 0..40 {
        let game = t.add(common, format!("Game {g}"), true);
        fill(&mut t, game, 2, 12, 60, "asset", "pak");
    }

    // ~0.9M of plain user data.
    let data = t.path(d, &["data"]);
    fill(&mut t, data, 4, 8, 200, "photo", "jpg");
    t
}

fn known() -> KnownFolders {
    let mut kf = KnownFolders::default();
    kf.machine.insert(KnownFolder::Windir, r"C:\Windows".into());
    kf.machine
        .insert(KnownFolder::ProgramFiles, r"C:\Program Files".into());
    kf.machine.insert(
        KnownFolder::ProgramFilesX86,
        r"C:\Program Files (x86)".into(),
    );
    kf.machine
        .insert(KnownFolder::ProgramData, r"C:\ProgramData".into());
    let mut u = UserFolders {
        is_current: true,
        ..Default::default()
    };
    for (k, v) in [
        (KnownFolder::UserProfile, r"C:\Users\alice"),
        (KnownFolder::LocalAppData, r"C:\Users\alice\AppData\Local"),
        (KnownFolder::AppData, r"C:\Users\alice\AppData\Roaming"),
        (KnownFolder::Temp, r"C:\Users\alice\AppData\Local\Temp"),
        (KnownFolder::Downloads, r"C:\Users\alice\Downloads"),
        (KnownFolder::Documents, r"C:\Users\alice\Documents"),
    ] {
        u.folders.insert(k, v.into());
    }
    kf.users.push(u);
    kf
}

fn entry<'a>(t: &'a Tree, id: u32) -> Entry<'a> {
    Entry {
        name: Name::Str(&t.names[id as usize]),
        flags: if t.is_dir[id as usize] {
            EntryFlags::DIR
        } else {
            EntryFlags::EMPTY
        },
        size: 4096,
        newest_mtime: Some(FileTime::from_unix_secs(1_700_000_000)),
        magic: None,
    }
}

fn kids(t: &Tree, id: u32) -> impl Iterator<Item = ChildRef<'_>> {
    t.children[id as usize].iter().map(move |&c| ChildRef {
        name: Name::Str(&t.names[c as usize]),
        is_dir: t.is_dir[c as usize],
    })
}

/// Classifies `dir`'s subtree; returns (entries, safe entries).
fn walk(c: &Classifier, t: &Tree, scope: &DirScope, dir: u32, parallel: bool) -> (u64, u64) {
    let kids_of = &t.children[dir as usize];
    let visit = |&k: &u32| -> (u64, u64) {
        let e = entry(t, k);
        if t.is_dir[k as usize] {
            let s = c.enter_dir(scope, &e, kids(t, k));
            let safe = u64::from(s.classification().safety == strata_core::Safety::Safe);
            let (n, sf) = walk(c, t, &s, k, parallel);
            (n + 1, sf + safe)
        } else {
            let cls = c.classify_file(scope, &e);
            (1, u64::from(cls.safety == strata_core::Safety::Safe))
        }
    };
    if parallel && kids_of.len() > 8 {
        kids_of
            .par_iter()
            .map(visit)
            .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    } else {
        kids_of
            .iter()
            .map(visit)
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    }
}

fn run(c: &Classifier, t: &Tree, parallel: bool) -> (u64, u64) {
    let mut total = (0, 0);
    for &vol in &t.children[0] {
        let scope = c.root(&t.names[vol as usize], kids(t, vol));
        let r = walk(c, t, &scope, vol, parallel);
        total = (total.0 + r.0, total.1 + r.1);
    }
    total
}

fn main() {
    let t0 = Instant::now();
    let tree = build();
    println!("tree: {} entries built in {:?}", tree.len(), t0.elapsed());

    let t0 = Instant::now();
    let rules = RuleSet::builtin().unwrap();
    let c = Classifier::new(&rules, &known(), &Default::default()).unwrap();
    println!("compile: {} rules in {:?}", c.rules().len(), t0.elapsed());

    // The dev machine is shared, so report the best of several runs.
    let best = |parallel: bool| {
        (0..5)
            .map(|_| {
                let t0 = Instant::now();
                let r = run(&c, &tree, parallel);
                (t0.elapsed(), r)
            })
            .min_by_key(|(d, _)| *d)
            .unwrap()
    };
    let (single, (n, safe)) = best(false);
    #[allow(clippy::cast_precision_loss)]
    let ns = single.as_nanos() as f64 / n as f64;
    println!("single-threaded: {n} entries ({safe} safe) in {single:?} = {ns:.0} ns/entry");
    #[allow(clippy::cast_precision_loss)]
    let five_m = ns * 5_000_000.0 / 1e9;
    println!("  extrapolated to 5M entries: {five_m:.2} s");

    let (par, (n2, _)) = best(true);
    println!(
        "parallel ({} threads): {n2} entries in {par:?}",
        rayon::current_num_threads()
    );
}
