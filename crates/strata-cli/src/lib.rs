//! `strata-cli`: run the MFT scanner against a volume or image and report.
//!
//! Responsibilities:
//! - [`args`]: command-line parsing.
//! - [`run`]: open the target (elevation check for drives), scan, print
//!   stats, totals, the largest files and a used-space reconciliation, and
//!   optionally write golden JSON ([`golden`]).
//! - [`paths`]: path reconstruction from parent references for display.
//!
//! Exit codes: 0 success, 1 failure, 2 not elevated (drive targets),
//! 64 usage error.

pub mod args;
pub mod golden;
pub mod paths;
pub mod report;
mod win;

pub use win::{disk_space, is_elevated};

use std::io::Write;

use strata_ntfs::{IoMode, NtfsVolume, RawVolume, ScanOptions};

use crate::args::{Command, ScanArgs, Target, USAGE};
use crate::report::{Collector, Reconciliation};

/// Exit code: success.
pub const EXIT_OK: i32 = 0;
/// Exit code: the scan or an I/O operation failed.
pub const EXIT_FAILED: i32 = 1;
/// Exit code: a drive was given but the process is not elevated.
pub const EXIT_NOT_ELEVATED: i32 = 2;
/// Exit code: bad command line (`EX_USAGE`).
pub const EXIT_USAGE: i32 = 64;

/// Message printed when a drive scan is attempted without elevation.
pub const NOT_ELEVATED_MESSAGE: &str =
    "Run from an elevated terminal: MFT scanning needs administrator rights.";

/// Runs the CLI with `argv` (without the program name), writing the report
/// to `out` and diagnostics to `err`. Returns the process exit code.
pub fn run(
    argv: impl IntoIterator<Item = String>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let args = match args::parse_args(argv) {
        Ok(Command::Help) => {
            let _ = writeln!(out, "{USAGE}");
            return EXIT_OK;
        }
        Ok(Command::Scan(a)) => a,
        Err(e) => {
            let _ = writeln!(err, "error: {e}\n\n{USAGE}");
            return EXIT_USAGE;
        }
    };
    if let Target::Drive(_) = args.target {
        match win::is_elevated() {
            Ok(true) => {}
            Ok(false) => {
                let _ = writeln!(err, "{NOT_ELEVATED_MESSAGE}");
                return EXIT_NOT_ELEVATED;
            }
            Err(e) => {
                let _ = writeln!(err, "error: cannot determine elevation: {e}");
                return EXIT_FAILED;
            }
        }
    }
    match scan(&args, out) {
        Ok(()) => EXIT_OK,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            EXIT_FAILED
        }
    }
}

fn scan(args: &ScanArgs, out: &mut dyn Write) -> Result<(), String> {
    let (reader, path_prefix, live) = match &args.target {
        Target::Drive(l) => {
            let mode = args.mode.unwrap_or(IoMode::NoBuffering);
            let v = RawVolume::open_drive(*l, mode).map_err(|e| format!("cannot open {l}: {e}"))?;
            (v, format!("{l}:"), true)
        }
        Target::Image(p) => {
            let mode = args.mode.unwrap_or(IoMode::Sequential);
            let v = RawVolume::open(p, mode)
                .map_err(|e| format!("cannot open {}: {e}", p.display()))?;
            (v, String::new(), false)
        }
    };
    let volume = NtfsVolume::open(reader).map_err(|e| e.to_string())?;
    let opts = ScanOptions {
        chunk_bytes: args.chunk_mib * 1024 * 1024,
        use_mft_bitmap: args.mft_bitmap,
        ..ScanOptions::default()
    };
    let mut collector = Collector::new(args.top, args.json.is_some());
    let stats = volume
        .scan(&opts, |batch| {
            for r in batch {
                collector.add(r);
            }
        })
        .map_err(|e| e.to_string())?;

    let cs = volume.boot().cluster_size;
    let (used, used_source) = match &args.target {
        Target::Drive(l) => match win::disk_space(&format!("{l}:\\")) {
            Ok((total, free)) => (
                Some(total.saturating_sub(free)),
                "GetDiskFreeSpaceExW".to_owned(),
            ),
            Err(e) => (None, format!("GetDiskFreeSpaceExW failed: {e}")),
        },
        Target::Image(_) => match volume.count_used_clusters() {
            Ok(c) => (Some(c.saturating_mul(cs)), "$Bitmap".to_owned()),
            Err(e) => (None, format!("$Bitmap unreadable: {e}")),
        },
    };
    let recon = Reconciliation {
        used,
        used_source,
        files_allocated: collector.totals.sum_allocated(),
        attr_overhead: collector.totals.attr_overhead,
        live_volume: live,
    };
    let text = report::render(&stats, &mut collector, &recon, &path_prefix);
    out.write_all(text.as_bytes()).map_err(|e| e.to_string())?;

    if let Some(json_path) = &args.json {
        let records = collector.all.take().unwrap_or_default();
        let doc = golden::Golden {
            format: golden::FORMAT.to_owned(),
            source: "mft".to_owned(),
            volume: golden::VolumeInfo {
                cluster_size: cs,
                record_size: volume.boot().record_size,
                total_bytes: volume.boot().volume_bytes(),
                used_bytes: used,
                other_attr_allocated: stats.other_attr_allocated,
                serial: format!("{:016X}", volume.boot().serial),
            },
            totals: collector.totals,
            entries: golden::entries(&records, &mut collector.paths),
        };
        let json = serde_json::to_vec_pretty(&doc).map_err(|e| e.to_string())?;
        std::fs::write(json_path, json)
            .map_err(|e| format!("cannot write {}: {e}", json_path.display()))?;
        let _ = writeln!(out, "\nWrote {}", json_path.display());
    }
    Ok(())
}
