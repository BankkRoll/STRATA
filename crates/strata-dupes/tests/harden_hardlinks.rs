//! A file with 1023 hardlinks spread over many folders is one file: with a
//! separate copy it forms one group of two, every other name is counted as
//! the same file, and the lowest path represents the link set.

mod common;

use std::path::{Path, PathBuf};

use common::{bytes, candidate, file_info};
use strata_clean::CancelToken;
use strata_dupes::*;

/// A self-deleting folder under `D:\strata-harden-tests` (or `%TEMP%`).
struct HardenDir(PathBuf);

impl HardenDir {
    fn new(tag: &str) -> Self {
        let leaf = format!("{tag}-{}", std::process::id());
        let p = if Path::new(r"D:\").exists() {
            PathBuf::from(r"D:\strata-harden-tests").join(leaf)
        } else {
            std::env::temp_dir().join(format!("strata-harden-{leaf}"))
        };
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for HardenDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_thousand_links_are_one_file() {
    let d = HardenDir::new("dupes-links");
    let data = bytes(77, 256 * 1024 + 3);
    let first = d.0.join(r"d00\original.bin");
    std::fs::create_dir_all(first.parent().unwrap()).unwrap();
    std::fs::write(&first, &data).unwrap();
    let mut names = vec![first.clone()];
    for i in 1..1023 {
        let p = d.0.join(format!(r"d{:02}\link-{i:04}.bin", i % 37));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::hard_link(&first, &p).unwrap();
        names.push(p);
    }
    let (_, _, links) = file_info(&first);
    assert_eq!(links, 1023);
    let copy = d.0.join(r"zz\copy.bin");
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    std::fs::write(&copy, &data).unwrap();

    let mut cands: Vec<Candidate> = names.iter().map(|p| candidate(p)).collect();
    cands.push(candidate(&copy));
    // Offer them in reverse so the result cannot depend on arrival order.
    cands.reverse();
    let cfg = ScanConfig {
        min_size: 1024,
        ..ScanConfig::default()
    };
    let r = match find_duplicates(
        cands,
        &cfg,
        &MemoryHashCache::new(),
        &CancelToken::new(),
        &|_| {},
    ) {
        ScanOutcome::Completed(r) => r,
        ScanOutcome::Cancelled(_) => panic!("cancelled"),
    };
    assert_eq!(r.groups.len(), 1);
    assert_eq!(r.groups[0].files.len(), 2);
    assert_eq!(r.stats.excluded[&Exclusion::SameFile], 1022);
    let lowest = names.iter().min().unwrap();
    let paths: Vec<&PathBuf> = r.groups[0].files.iter().map(|f| &f.path).collect();
    assert!(paths.contains(&lowest), "{paths:?}");
    assert!(paths.contains(&&copy));
    // Only the two distinct files were read.
    assert_eq!(r.stats.measured, 2);
    assert!(r.stats.bytes_read <= 2 * data.len() as u64 * 2);
}
