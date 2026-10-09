//! Writes a large synthetic NTFS image for I/O benchmarks of the MFT scan.
//!
//! ```text
//! cargo run --release -p strata-ntfs --features test-image --example perf_image -- \
//!     <out.img> [records (default 5000000)] [mft fragments (default 96)]
//! ```
//!
//! The image resembles a large, nearly full system volume: a fragmented MFT
//! whose `$DATA` and `$MFT:$BITMAP` both continue in an extension record,
//! about 70% of records in use, two large all-free regions plus scattered
//! free records, and a file mix of directories, resident and non-resident
//! data, alternate streams, hardlinks, symlinks and attribute-list records
//! with extension records. Scan it with
//! `strata-cli scan <out.img> --no-buffering --top 0`.

use std::time::Instant;

use strata_core::FileRef;
use strata_ntfs::test_image::{
    AttrListSpec, Geometry, ImageBuilder, NonResidentSpec, ROOT, RecordBuilder, symlink_reparse,
};
use strata_ntfs::{AT_DATA, AT_FILE_NAME, AT_STANDARD_INFORMATION, NS_WIN32, Run};

/// First record the generator fills; lower ones belong to NTFS metadata.
const FIRST: u64 = 24;
/// Every record whose number is a multiple of this is a directory.
const DIR_EVERY: u64 = 13;
/// Volume size in clusters (464 GiB at 4 KiB clusters).
const VOLUME_CLUSTERS: u64 = 121_634_816;

/// SplitMix64: a cheap, well-mixed hash so the layout is deterministic.
fn mix(n: u64) -> u64 {
    let mut z = n.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

struct Shape {
    records: u64,
    /// All-free record ranges.
    holes: [(u64, u64); 2],
}

impl Shape {
    fn new(records: u64) -> Self {
        let at = |f: f64| (records as f64 * f) as u64;
        Self {
            records,
            holes: [(at(0.41), at(0.49)), (at(0.86), at(0.92))],
        }
    }

    fn last_dir(&self) -> u64 {
        self.holes[0].0 / DIR_EVERY
    }

    fn in_use(&self, n: u64) -> bool {
        if n < FIRST || n >= self.records || self.holes.iter().any(|&(a, b)| (a..b).contains(&n)) {
            return false;
        }
        if self.is_dir(n) {
            return true;
        }
        // Base + extension pairs stay together.
        if matches!(n % 1000, 507 | 508) {
            return self.in_use_scattered(n - n % 1000 + 507);
        }
        self.in_use_scattered(n)
    }

    fn is_dir(&self, n: u64) -> bool {
        n.is_multiple_of(DIR_EVERY)
            && n / DIR_EVERY < self.last_dir()
            && !matches!(n % 1000, 507 | 508)
    }

    fn in_use_scattered(&self, n: u64) -> bool {
        mix(n) % 100 < 81
    }

    /// A directory at a lower record number than `n` (the root for early records).
    fn parent(&self, n: u64) -> FileRef {
        let below = (n / DIR_EVERY).min(self.last_dir());
        if below <= 2 {
            return ROOT;
        }
        let k = 2 + mix(n ^ 0x5555) % (below - 2);
        FileRef::from_parts(k * DIR_EVERY, 1)
    }

    fn record(&self, n: u64) -> Option<RecordBuilder> {
        if !self.in_use(n) {
            return None;
        }
        let h = mix(n.wrapping_mul(31));
        let parent = self.parent(n);
        if self.is_dir(n) {
            return Some(RecordBuilder::dir(1, parent, &format!("Folder {n:x}")));
        }
        let name = format!(
            "{}_{n}.{}",
            ["lib", "Microsoft.Windows", "data", "x"][(h % 4) as usize],
            ["dll", "dat", "json", "mui", "png"][(h >> 8) as usize % 5]
        );
        match n % 1000 {
            507 => {
                let me = FileRef::from_parts(n, 1);
                let ext = FileRef::from_parts(n + 1, 1);
                return Some(RecordBuilder::file(1, parent, &name).attr_list(&[
                    AttrListSpec::new(AT_STANDARD_INFORMATION, 0, me),
                    AttrListSpec::new(AT_FILE_NAME, 0, me),
                    AttrListSpec::new(AT_DATA, 0, ext),
                ]));
            }
            508 => {
                return Some(
                    RecordBuilder::new(1)
                        .extension_of(FileRef::from_parts(n - 1, 1))
                        .data_nonresident("", 0, data_spec(h, 7)),
                );
            }
            _ => {}
        }
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
        eprintln!("usage: perf_image <out.img> [records] [fragments]");
        std::process::exit(64);
    };
    let records: u64 = args
        .next()
        .map_or(5_000_000, |a| a.parse().expect("records"));
    let fragments: usize = args.next().map_or(96, |a| a.parse().expect("fragments"));
    let started = Instant::now();
    let shape = Shape::new(records);
    let in_use = (0..records).filter(|&n| shape.in_use(n)).count();
    let builder = ImageBuilder::new(Geometry::default())
        .with_system_files()
        .mft_fragments(fragments)
        .mft_data_in_extension(15)
        .mft_bitmap_in_extension()
        .min_records(records)
        .claim_clusters(VOLUME_CLUSTERS)
        .generate(move |n| shape.record(n));
    let file = std::fs::File::create(&out)?;
    let len = builder.write_to(&file)?;
    file.sync_all()?;
    println!(
        "{out}: {records} records ({in_use} generated in use), {fragments} MFT fragments, {:.2} GiB, {:.1} s",
        len as f64 / f64::from(1u32 << 30),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
