//! Writes a large synthetic NTFS image dominated by attribute-list files,
//! for benchmarks of the scan's completion stage.
//!
//! ```text
//! cargo run --release -p strata-ntfs --features test-image --example perf_links -- \
//!     <out.img> [records (default 9000000)] [non-resident list % (default 40)]
//! ```
//!
//! The image resembles a large system volume full of component-store
//! hardlinks: about 5.3M records in use out of 9M, two large all-free
//! regions plus scattered free records, and in every 100-record block four
//! hardlinked files whose names and data spill into one to three extension
//! records through an attribute list (about 500k extension records in all).
//! Some lists are stored non-resident in their own cluster; some groups put
//! their extension records before the base record. The rest of the mix is
//! directories, resident and non-resident files, alternate streams, two-name
//! hardlinks and symlinks. Scan it with
//! `strata-cli scan <out.img> --no-buffering --top 0`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use strata_core::FileRef;
use strata_ntfs::test_image::{
    AttrListSpec, AttrValue, Geometry, ImageBuilder, NonResidentSpec, ROOT, RecordBuilder,
    attr_list_value, symlink_reparse,
};
use strata_ntfs::{
    AT_ATTRIBUTE_LIST, AT_DATA, AT_FILE_NAME, AT_STANDARD_INFORMATION, NS_WIN32, Run,
};

/// First record the generator fills; lower ones belong to NTFS metadata.
const FIRST: u64 = 24;
/// Every record whose number is a multiple of this is a directory.
const DIR_EVERY: u64 = 13;
/// Volume size in clusters (464 GiB at 4 KiB clusters).
const VOLUME_CLUSTERS: u64 = 121_634_816;
/// Offsets within each 100-record block where a link group starts. Each
/// group spans up to four records (base plus one to three extensions).
const GROUPS: [u64; 4] = [40, 46, 52, 58];
/// Records per group window.
const WINDOW: u64 = 4;

/// SplitMix64: a cheap, well-mixed hash so the layout is deterministic.
fn mix(n: u64) -> u64 {
    let mut z = n.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One hardlinked file whose attributes span several records.
#[derive(Clone, Copy)]
struct Group {
    base: u64,
    /// Extension records, in record order.
    holders: [u64; 3],
    count: usize,
    /// Names held by each extension record.
    names_per_holder: u64,
}

impl Group {
    fn holders(&self) -> &[u64] {
        &self.holders[..self.count]
    }

    fn records(&self) -> impl Iterator<Item = u64> + '_ {
        std::iter::once(self.base).chain(self.holders().iter().copied())
    }
}

struct Shape {
    records: u64,
    holes: [(u64, u64); 2],
    nonresident_pct: u64,
}

impl Shape {
    fn new(records: u64, nonresident_pct: u64) -> Self {
        let at = |f: f64| (records as f64 * f) as u64;
        Self {
            records,
            holes: [(at(0.30), at(0.45)), (at(0.70), at(0.85))],
            nonresident_pct,
        }
    }

    fn in_hole(&self, n: u64) -> bool {
        n < FIRST || n >= self.records || self.holes.iter().any(|&(a, b)| (a..b).contains(&n))
    }

    fn last_dir(&self) -> u64 {
        self.holes[0].0 / DIR_EVERY
    }

    fn in_group_window(n: u64) -> Option<u64> {
        let off = n % 100;
        GROUPS
            .iter()
            .find(|&&g| (g..g + WINDOW).contains(&off))
            .map(|&g| n - off + g)
    }

    /// The group whose window starts at `start`, if the whole group lies
    /// outside the free regions.
    fn group(&self, start: u64) -> Option<Group> {
        let h = mix(start ^ 0x6C69_6E6B);
        let count = 1 + (h % 3) as usize;
        // Every third group puts its extension records before the base.
        let (base, first_holder) = if h.is_multiple_of(3) {
            (start + count as u64, start)
        } else {
            (start, start + 1)
        };
        let mut holders = [0; 3];
        for (i, slot) in holders.iter_mut().enumerate().take(count) {
            *slot = first_holder + i as u64;
        }
        let g = Group {
            base,
            holders,
            count,
            names_per_holder: 2 + (h >> 8) % 2,
        };
        g.records().all(|r| !self.in_hole(r)).then_some(g)
    }

    fn nonresident_list(&self, g: &Group) -> bool {
        mix(g.base ^ 0x4C49_5354) % 100 < self.nonresident_pct
    }

    fn is_dir(&self, n: u64) -> bool {
        n.is_multiple_of(DIR_EVERY)
            && n / DIR_EVERY < self.last_dir()
            && Self::in_group_window(n).is_none()
    }

    fn parent(&self, n: u64) -> FileRef {
        let below = (n / DIR_EVERY).min(self.last_dir());
        if below <= 2 {
            return ROOT;
        }
        let mut k = 2 + mix(n ^ 0x5555) % (below - 2);
        while Self::in_group_window(k * DIR_EVERY).is_some() {
            k -= 1;
        }
        if k < 2 {
            return ROOT;
        }
        FileRef::from_parts(k * DIR_EVERY, 1)
    }

    fn long_name(n: u64, i: u64) -> String {
        format!(
            "amd64_microsoft-windows-comp-{:x}_31bf3856ad364e35_10.0.{}.{}_none_{:016x}_{i}.dll",
            mix(n) & 0xFFFF,
            19041 + n % 7,
            mix(n ^ i) % 5000,
            mix(n.wrapping_add(i)),
        )
    }

    fn group_list(g: &Group) -> Vec<u8> {
        let me = FileRef::from_parts(g.base, 1);
        let mut entries = vec![
            AttrListSpec::new(AT_STANDARD_INFORMATION, 0, me),
            AttrListSpec::new(AT_FILE_NAME, 0, me),
        ];
        for &h in g.holders() {
            for _ in 0..g.names_per_holder {
                entries.push(AttrListSpec::new(
                    AT_FILE_NAME,
                    0,
                    FileRef::from_parts(h, 1),
                ));
            }
        }
        let last = *g.holders().last().unwrap_or(&g.base);
        entries.push(AttrListSpec::new(AT_DATA, 0, FileRef::from_parts(last, 1)));
        attr_list_value(&entries)
    }

    fn group_record(&self, g: &Group, n: u64, list_lcn: Option<u64>) -> RecordBuilder {
        let parent = self.parent(n);
        if n == g.base {
            let f = RecordBuilder::file(1, parent, &Self::long_name(n, 0));
            let list = Self::group_list(g);
            return match list_lcn {
                Some(lcn) => f.attr(
                    AT_ATTRIBUTE_LIST,
                    "",
                    0,
                    AttrValue::NonResident(NonResidentSpec::new(
                        vec![Run {
                            vcn: 0,
                            lcn: Some(lcn),
                            len: 1,
                        }],
                        list.len() as u64,
                        4096,
                    )),
                ),
                None => f.attr(AT_ATTRIBUTE_LIST, "", 0, AttrValue::Resident(list)),
            };
        }
        let mut r = RecordBuilder::new(1).extension_of(FileRef::from_parts(g.base, 1));
        for i in 0..g.names_per_holder {
            r = r.name(
                self.parent(n ^ (i + 1)),
                &Self::long_name(n, i + 1),
                NS_WIN32,
            );
        }
        if g.holders().last() == Some(&n) {
            r = r.data_nonresident("", 0, data_spec(mix(n), 1 + n % 3));
        }
        r
    }

    fn record(&self, n: u64, lists: &HashMap<u64, u64>) -> Option<RecordBuilder> {
        if self.in_hole(n) {
            return None;
        }
        if let Some(start) = Self::in_group_window(n) {
            let g = self.group(start)?;
            if !g.records().any(|r| r == n) {
                return None;
            }
            return Some(self.group_record(&g, n, lists.get(&g.base).copied()));
        }
        let parent = self.parent(n);
        if self.is_dir(n) {
            return Some(RecordBuilder::dir(1, parent, &format!("Folder {n:x}")));
        }
        if mix(n) % 100 >= 84 {
            return None;
        }
        let h = mix(n.wrapping_mul(31));
        let name = format!(
            "{}_{n}.{}",
            ["lib", "Microsoft.Windows", "data", "x"][(h % 4) as usize],
            ["dll", "dat", "json", "mui", "png"][(h >> 8) as usize % 5]
        );
        let f = RecordBuilder::file(1, parent, &name);
        Some(match h % 100 {
            0..=29 => f.data_nonresident("", 0, data_spec(h, 1 + (h >> 20) % 3)),
            30..=34 => f.data("", &[0u8; 120]).data("Zone.Identifier", &[0u8; 26]),
            35..=37 => f
                .name(self.parent(n ^ 0xABCD), &format!("link_{n}.dll"), NS_WIN32)
                .data_nonresident("", 0, data_spec(h, 1)),
            38..=39 => f
                .reparse(symlink_reparse(
                    r"\??\C:\Windows\target",
                    r"C:\Windows\target",
                    false,
                ))
                .data("", &[]),
            _ => f.data("", &[0u8; 60]),
        })
    }
}

/// A non-resident stream of `pieces` runs scattered over the volume.
fn data_spec(h: u64, pieces: u64) -> NonResidentSpec {
    let mut runs = Vec::new();
    let mut vcn = 0;
    for i in 0..pieces {
        let len = 1 + mix(h ^ i) % 64;
        let lcn = 2_000_000 + mix(h.wrapping_add(i)) % (VOLUME_CLUSTERS - 3_000_000);
        runs.push(Run {
            vcn,
            lcn: Some(lcn),
            len,
        });
        vcn += len;
    }
    NonResidentSpec::new(runs, vcn * 4096 - mix(h) % 4096, 4096)
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(out) = args.next() else {
        eprintln!("usage: perf_links <out.img> [records] [non-resident list %]");
        std::process::exit(64);
    };
    let records: u64 = args
        .next()
        .map_or(9_000_000, |a| a.parse().expect("records"));
    let pct: u64 = args.next().map_or(40, |a| a.parse().expect("percent"));
    let started = Instant::now();
    let shape = Shape::new(records, pct);
    let mut builder = ImageBuilder::new(Geometry::default())
        .with_system_files()
        .mft_fragments(96)
        .mft_data_in_extension(15)
        .mft_bitmap_in_extension()
        .min_records(records)
        .claim_clusters(VOLUME_CLUSTERS);

    let mut lists = HashMap::new();
    let (mut groups, mut extensions) = (0u64, 0u64);
    for block in (0..records).step_by(100) {
        for off in GROUPS {
            let Some(g) = shape.group(block + off) else {
                continue;
            };
            groups += 1;
            extensions += g.count as u64;
            if shape.nonresident_list(&g) {
                let runs = builder.alloc(1);
                builder.write_runs(&runs, &Shape::group_list(&g));
                lists.insert(g.base, runs[0].lcn.unwrap_or(0));
            }
        }
    }
    let nonresident = lists.len();
    let lists = Arc::new(lists);
    let shape = Arc::new(shape);
    let in_use = {
        let (s, l) = (Arc::clone(&shape), Arc::clone(&lists));
        (0..records).filter(|&n| s.record(n, &l).is_some()).count()
    };
    let gen_shape = Arc::clone(&shape);
    let builder = builder.generate(move |n| gen_shape.record(n, &lists));
    let file = std::fs::File::create(&out)?;
    let len = builder.write_to(&file)?;
    file.sync_all()?;
    println!(
        "{out}: {records} records ({in_use} generated in use), {groups} attribute-list files \
         ({nonresident} non-resident lists), {extensions} extension records, {:.2} GiB, {:.1} s",
        len as f64 / f64::from(1u32 << 30),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
