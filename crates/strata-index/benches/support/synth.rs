//! Deterministic synthetic volumes with realistic names and shapes.
//!
//! The mix approximates a developer's Windows system drive: a `node_modules`
//! heavy dev tree (many small directories of short JS names), a Windows-like
//! tree (System32 files, WinSxS component directories with hardlinks into
//! System32), a media library (dated photo folders, music albums, videos),
//! and profile clutter (documents, app caches with hex names).

use std::collections::VecDeque;

use strata_core::{EntryFlags, FileRef, FileTime, NameLink, ScanRecord, Sizes, Times, WideName};

/// The root reference every generated volume uses.
pub const ROOT: FileRef = FileRef::from_parts(5, 5);

/// "Now" for generated timestamps and the index (2025-10-09).
pub fn now() -> FileTime {
    FileTime::from_unix_secs(1_760_000_000)
}

const WORDS: [&str; 64] = [
    "react",
    "core",
    "utils",
    "lodash",
    "parser",
    "render",
    "async",
    "stream",
    "buffer",
    "crypto",
    "events",
    "path",
    "babel",
    "plugin",
    "loader",
    "config",
    "server",
    "client",
    "router",
    "state",
    "store",
    "theme",
    "style",
    "color",
    "chalk",
    "debug",
    "glob",
    "minimatch",
    "semver",
    "yargs",
    "commander",
    "webpack",
    "vite",
    "rollup",
    "esbuild",
    "jest",
    "mocha",
    "types",
    "schema",
    "json",
    "yaml",
    "markdown",
    "image",
    "video",
    "audio",
    "network",
    "shell",
    "kernel",
    "graphics",
    "input",
    "storage",
    "update",
    "security",
    "print",
    "speech",
    "search",
    "photo",
    "camera",
    "music",
    "sync",
    "cloud",
    "model",
    "cache",
    "index",
];

struct Gen<'a> {
    rng: u64,
    next_rec: u64,
    count: usize,
    limit: usize,
    emit: &'a mut dyn FnMut(ScanRecord),
}

impl Gen<'_> {
    fn rand(&mut self) -> u64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.rand() % n.max(1)
    }

    fn between(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    fn word(&mut self) -> &'static str {
        WORDS[self.below(WORDS.len() as u64) as usize]
    }

    fn full(&self) -> bool {
        self.count >= self.limit
    }

    fn new_ref(&mut self) -> FileRef {
        let r = self.next_rec;
        // Leave occasional holes, like free MFT records.
        self.next_rec += 1 + u64::from(self.below(10) == 0);
        FileRef::from_parts(r, 1 + (r % 13) as u16)
    }

    fn times(&mut self) -> Times {
        let age = self.below(5 * 365 * 86_400) as i64;
        let t = FileTime::from_unix_secs(1_760_000_000 - age);
        Times {
            created: t,
            modified: t,
            accessed: t,
            changed: t,
        }
    }

    /// Log-uniform size between 2^lo and 2^hi bytes.
    fn size(&mut self, lo: u32, hi: u32) -> u64 {
        let bits = self.between(u64::from(lo), u64::from(hi)) as u32;
        let base = 1u64 << bits;
        base + self.below(base)
    }

    fn dir(&mut self, parent: FileRef, name: &str) -> Option<FileRef> {
        if self.full() {
            return None;
        }
        let id = self.new_ref();
        let rec = ScanRecord {
            id,
            links: vec![link(parent, name)],
            attributes: 0x10,
            flags: EntryFlags::DIR,
            times: self.times(),
            fn_created: None,
            sizes: Sizes {
                dir_overhead: if self.below(4) == 0 { 4096 } else { 0 },
                ..Sizes::default()
            },
            reparse: None,
            ads: vec![],
        };
        self.count += 1;
        (self.emit)(rec);
        Some(id)
    }

    fn file(&mut self, links: Vec<NameLink>, logical: u64) {
        if self.full() {
            return;
        }
        let allocated = if logical < 700 {
            0
        } else {
            logical.next_multiple_of(4096)
        };
        let n = links.len();
        let id = self.new_ref();
        let rec = ScanRecord {
            id,
            links,
            attributes: 0x20,
            flags: EntryFlags::EMPTY,
            times: self.times(),
            fn_created: None,
            sizes: Sizes {
                logical,
                allocated,
                ..Sizes::default()
            },
            reparse: None,
            ads: vec![],
        };
        self.count += n;
        (self.emit)(rec);
    }

    // ---- areas --------------------------------------------------------------

    fn dev(&mut self, parent: FileRef, budget: usize) {
        let stop = (self.count + budget).min(self.limit);
        let Some(dev) = self.dir(parent, "dev") else {
            return;
        };
        let mut p = 0;
        while self.count < stop {
            let Some(proj) = self.dir(dev, &format!("project-{p}")) else {
                return;
            };
            p += 1;
            for f in ["package.json", "README.md", "tsconfig.json", ".gitignore"] {
                let s = self.size(8, 12);
                self.file(vec![link(proj, f)], s);
            }
            let Some(nm) = self.dir(proj, "node_modules") else {
                return;
            };
            let packages = self.between(150, 600);
            let mut queue: VecDeque<(FileRef, u32)> = VecDeque::new();
            for k in 0..packages {
                if self.count >= stop {
                    return;
                }
                let name = format!("{}-{}{k}", self.word(), self.word());
                let Some(pkg) = self.dir(nm, &name) else {
                    return;
                };
                queue.push_back((pkg, 0));
            }
            while let Some((pkg, depth)) = queue.pop_front() {
                if self.count >= stop {
                    return;
                }
                for f in ["package.json", "README.md", "LICENSE", "index.js"] {
                    let s = self.size(9, 14);
                    self.file(vec![link(pkg, f)], s);
                }
                for sub in ["lib", "dist", "src", "types", "esm", "cjs", "test"] {
                    if self.below(3) == 0 {
                        continue;
                    }
                    let Some(d) = self.dir(pkg, sub) else { return };
                    for _ in 0..self.between(2, 12) {
                        let w = self.word();
                        let ext =
                            ["js", "d.ts", "js.map", "mjs", "cjs", "json"][self.below(6) as usize];
                        let s = self.size(8, 16);
                        self.file(vec![link(d, &format!("{w}.{ext}"))], s);
                    }
                }
                if depth == 0 && self.below(8) == 0 {
                    let Some(inner) = self.dir(pkg, "node_modules") else {
                        return;
                    };
                    for k in 0..self.between(1, 4) {
                        let name = format!("{}{k}", self.word());
                        if let Some(d) = self.dir(inner, &name) {
                            queue.push_back((d, 1));
                        }
                    }
                }
            }
        }
    }

    fn windows(&mut self, parent: FileRef, budget: usize) {
        let stop = (self.count + budget).min(self.limit);
        let Some(win) = self.dir(parent, "Windows") else {
            return;
        };
        let Some(sys) = self.dir(win, "System32") else {
            return;
        };
        let Some(sxs) = self.dir(win, "WinSxS") else {
            return;
        };
        let mut n = 0u64;
        while self.count < stop {
            n += 1;
            let w = self.word();
            let comp = format!(
                "amd64_microsoft-windows-{w}-{}_31bf3856ad364e35_10.0.19041.{}_none_{:016x}",
                self.word(),
                n % 5000,
                self.rand()
            );
            let Some(c) = self.dir(sxs, &comp) else {
                return;
            };
            for k in 0..self.between(1, 6) {
                let ext = ["dll", "exe", "mui", "manifest", "sys", "cat"][self.below(6) as usize];
                let name = format!("{w}{k}.{ext}");
                let s = self.size(10, 22);
                let mut links = vec![link(c, &name)];
                if self.below(3) == 0 {
                    links.push(link(sys, &format!("{w}{n}_{k}.{ext}")));
                }
                self.file(links, s);
            }
            if n.is_multiple_of(40) {
                let Some(loc) = self.dir(sys, &format!("{w}-{n}")) else {
                    return;
                };
                for k in 0..self.between(5, 40) {
                    let s = self.size(10, 20);
                    let name = format!("{}{k}.dll.mui", self.word());
                    self.file(vec![link(loc, &name)], s);
                }
            }
        }
    }

    fn media(&mut self, parent: FileRef, budget: usize) {
        let stop = (self.count + budget).min(self.limit);
        let Some(pics) = self.dir(parent, "Pictures") else {
            return;
        };
        let Some(music) = self.dir(parent, "Music") else {
            return;
        };
        let Some(videos) = self.dir(parent, "Videos") else {
            return;
        };
        let mut img = 0u64;
        let mut year = 2010;
        while self.count < stop {
            let Some(y) = self.dir(pics, &year.to_string()) else {
                return;
            };
            for m in 1..=12 {
                let Some(md) = self.dir(y, &format!("{year}-{m:02}")) else {
                    return;
                };
                for _ in 0..self.between(20, 300) {
                    img += 1;
                    let name = if self.below(2) == 0 {
                        format!("IMG_{img:04}.JPG")
                    } else {
                        format!("DSC{img:05}.NEF")
                    };
                    let s = self.size(20, 25);
                    self.file(vec![link(md, &name)], s);
                }
            }
            let name = format!("{} {}", self.word(), self.word());
            let Some(artist) = self.dir(music, &name) else {
                return;
            };
            for a in 0..self.between(1, 5) {
                let name = format!("Album {a} - {}", self.word());
                let Some(album) = self.dir(artist, &name) else {
                    return;
                };
                for t in 1..self.between(8, 16) {
                    let s = self.size(22, 24);
                    let name = format!("{t:02} - {}.mp3", self.word());
                    self.file(vec![link(album, &name)], s);
                }
            }
            for v in 0..self.between(2, 10) {
                let s = self.size(28, 32);
                let name = format!("{}-{year}-{v}.mp4", self.word());
                self.file(vec![link(videos, &name)], s);
            }
            year += 1;
        }
    }

    fn profile(&mut self, parent: FileRef, budget: usize) {
        let stop = (self.count + budget).min(self.limit);
        let Some(docs) = self.dir(parent, "Documents") else {
            return;
        };
        let Some(local) = self.dir(parent, "AppData") else {
            return;
        };
        let mut k = 0u64;
        while self.count < stop {
            k += 1;
            let name = format!("{} notes {k}", self.word());
            let Some(d) = self.dir(docs, &name) else {
                return;
            };
            for i in 0..self.between(3, 30) {
                let ext = ["docx", "pdf", "xlsx", "txt", "pptx"][self.below(5) as usize];
                let s = self.size(10, 23);
                let name = format!("{} report {i}.{ext}", self.word());
                self.file(vec![link(d, &name)], s);
            }
            let name = format!("{}{k}", self.word());
            let Some(app) = self.dir(local, &name) else {
                return;
            };
            let Some(cache) = self.dir(app, "Cache") else {
                return;
            };
            for _ in 0..self.between(20, 200) {
                let s = self.size(9, 18);
                let name = format!("{:016x}", self.rand());
                self.file(vec![link(cache, &name)], s);
            }
        }
    }
}

fn link(parent: FileRef, name: &str) -> NameLink {
    NameLink {
        parent,
        name: WideName::from_str_lossless(name),
    }
}

/// Emits about `n` entries (one per name link) of a synthetic volume, root
/// first, deterministically for a given `seed`.
pub fn generate(n: usize, seed: u64, emit: &mut dyn FnMut(ScanRecord)) {
    emit(ScanRecord {
        id: ROOT,
        links: vec![link(ROOT, "")],
        attributes: 0x10,
        flags: EntryFlags::DIR,
        times: Times::default(),
        fn_created: None,
        sizes: Sizes::default(),
        reparse: None,
        ads: vec![],
    });
    let mut g = Gen {
        rng: seed | 1,
        next_rec: 64,
        count: 1,
        limit: n,
        emit,
    };
    let Some(users) = g.dir(ROOT, "Users") else {
        return;
    };
    let Some(me) = g.dir(users, "me") else { return };
    g.dev(ROOT, n * 45 / 100);
    g.windows(ROOT, n * 30 / 100);
    g.media(me, n * 15 / 100);
    while !g.full() {
        let budget = n - g.count;
        g.profile(me, budget);
    }
}

/// All records of [`generate`] in a vector.
pub fn records(n: usize, seed: u64) -> Vec<ScanRecord> {
    let mut v = Vec::with_capacity(n);
    generate(n, seed, &mut |r| v.push(r));
    v
}
