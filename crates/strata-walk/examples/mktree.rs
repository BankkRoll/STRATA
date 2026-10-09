//! Builds a large synthetic tree for walker benchmarks.
//!
//! ```text
//! cargo run --release -p strata-walk --example mktree -- <root> <files> [files_per_dir]
//! ```
//!
//! Files are spread over a three-level directory fan-out with sizes from 0
//! to ~16 KiB (deterministic pseudo-random), so the tree mixes resident and
//! non-resident files like a real source/cache tree.

use std::path::PathBuf;

use rayon::prelude::*;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("root"));
    let files: usize = args.next().expect("file count").parse().expect("number");
    let per_dir: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(100);
    let dirs = files.div_ceil(per_dir);
    (0..dirs).into_par_iter().for_each(|d| {
        let dir = root
            .join(format!("l1-{:03}", d / 1000))
            .join(format!("l2-{:03}", (d / 50) % 20))
            .join(format!("leaf-{d:06}"));
        std::fs::create_dir_all(&dir).expect("create dir");
        for f in 0..per_dir.min(files - d * per_dir) {
            let n = (d * per_dir + f) as u64;
            let len = (n.wrapping_mul(2_654_435_761) >> 7) % 16_384;
            let len = if n.is_multiple_of(3) { len % 600 } else { len };
            std::fs::write(
                dir.join(format!("file-{f:04}.dat")),
                vec![b'a'; len as usize],
            )
            .expect("write file");
        }
    });
    println!(
        "created {files} files in {dirs} leaf directories under {}",
        root.display()
    );
}
