//! Command-line parsing (hand-rolled: one subcommand, a handful of flags).

use std::path::PathBuf;

use strata_ntfs::IoMode;

/// Usage text printed by `--help` and on usage errors.
pub const USAGE: &str = "\
Usage: strata-cli scan <X:|path-to-image> [options]

Scans an NTFS volume's MFT (drive letter, needs an elevated terminal) or an
NTFS image file, then prints scan stats, totals, the largest files and a
reconciliation of used space against the sum of allocations.

Options:
  --json <out.json>   Also write golden-format JSON (per-path sizes and flags)
  --top <N>           Number of largest files to list (default 50)
  --chunk-mib <N>     MFT read size in MiB (default 4)
  --io-depth <N>      MFT reads kept in flight at once (default 8; 1 = one at a time)
  --no-buffering      Open with FILE_FLAG_NO_BUFFERING (default for drives)
  --sequential        Open with FILE_FLAG_SEQUENTIAL_SCAN (default for images)
  --mft-bitmap        Skip records that $MFT:$BITMAP marks unused (default)
  --no-mft-bitmap     Read and parse every MFT record
  -h, --help          Show this help";

/// What to scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A drive letter (`C:`), opened as `\\.\C:`.
    Drive(char),
    /// An image file.
    Image(PathBuf),
}

/// Options of the `scan` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanArgs {
    /// Volume or image.
    pub target: Target,
    /// Golden JSON output path.
    pub json: Option<PathBuf>,
    /// Largest-files count.
    pub top: usize,
    /// Read size in MiB.
    pub chunk_mib: usize,
    /// Reads kept in flight.
    pub io_depth: usize,
    /// Explicit I/O mode, if given.
    pub mode: Option<IoMode>,
    /// Use `$MFT:$BITMAP` to skip unused records.
    pub mft_bitmap: bool,
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `scan`.
    Scan(ScanArgs),
    /// `--help`.
    Help,
}

/// Parses arguments (without the program name).
///
/// # Errors
///
/// A message describing the usage error.
///
/// # Example
///
/// ```
/// use strata_cli::args::{parse_args, Command, Target};
/// let cmd = parse_args(["scan", "D:", "--top", "5"].map(String::from)).unwrap();
/// let Command::Scan(a) = cmd else { panic!() };
/// assert_eq!((a.target, a.top), (Target::Drive('D'), 5));
/// ```
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut it = args.into_iter();
    match it.next().as_deref() {
        None | Some("-h" | "--help" | "help") => return Ok(Command::Help),
        Some("scan") => {}
        Some(other) => return Err(format!("unknown command `{other}`")),
    }
    let mut target = None;
    let mut out = ScanArgs {
        target: Target::Drive('C'),
        json: None,
        top: 50,
        chunk_mib: strata_ntfs::DEFAULT_CHUNK_BYTES / (1024 * 1024),
        io_depth: strata_ntfs::DEFAULT_IO_DEPTH,
        mode: None,
        mft_bitmap: true,
    };
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--json" => out.json = Some(PathBuf::from(value("--json")?)),
            "--top" => out.top = parse_num(&value("--top")?, "--top", 0)?,
            "--chunk-mib" => out.chunk_mib = parse_num(&value("--chunk-mib")?, "--chunk-mib", 1)?,
            "--no-buffering" => set_mode(&mut out.mode, IoMode::NoBuffering)?,
            "--sequential" => set_mode(&mut out.mode, IoMode::Sequential)?,
            "--io-depth" => out.io_depth = parse_num(&value("--io-depth")?, "--io-depth", 1)?,
            "--mft-bitmap" => out.mft_bitmap = true,
            "--no-mft-bitmap" => out.mft_bitmap = false,
            flag if flag.starts_with("--") => return Err(format!("unknown option `{flag}`")),
            positional => {
                if target.is_some() {
                    return Err(format!("unexpected argument `{positional}`"));
                }
                target = Some(parse_target(positional));
            }
        }
    }
    out.target = target.ok_or("scan needs a target: a drive letter (C:) or an image path")?;
    Ok(Command::Scan(out))
}

fn set_mode(slot: &mut Option<IoMode>, mode: IoMode) -> Result<(), String> {
    match slot {
        Some(m) if *m != mode => Err("--no-buffering and --sequential are exclusive".into()),
        _ => {
            *slot = Some(mode);
            Ok(())
        }
    }
}

fn parse_num(s: &str, name: &str, min: usize) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(n) if n >= min && n <= 1 << 20 => Ok(n),
        _ => Err(format!("{name} expects a whole number >= {min}, got `{s}`")),
    }
}

/// `C:`, `c:` and `C:\` are drives; anything else is an image path.
fn parse_target(s: &str) -> Target {
    let b = s.as_bytes();
    let drive = matches!(b, [l, b':'] | [l, b':', b'\\'] if l.is_ascii_alphabetic());
    if drive {
        Target::Drive(char::from(b[0]).to_ascii_uppercase())
    } else {
        Target::Image(PathBuf::from(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &[&str]) -> Result<Command, String> {
        parse_args(s.iter().map(|x| (*x).to_owned()))
    }

    #[test]
    fn parses_all_options() {
        let Command::Scan(a) = parse(&[
            "scan",
            r"img\vol.img",
            "--json",
            "o.json",
            "--top",
            "7",
            "--chunk-mib",
            "4",
            "--sequential",
            "--no-mft-bitmap",
            "--io-depth",
            "3",
        ])
        .unwrap() else {
            panic!("not a scan")
        };
        assert_eq!(a.target, Target::Image(PathBuf::from(r"img\vol.img")));
        assert_eq!(a.json, Some(PathBuf::from("o.json")));
        assert_eq!(
            (a.top, a.chunk_mib, a.io_depth, a.mode, a.mft_bitmap),
            (7, 4, 3, Some(IoMode::Sequential), false)
        );
        let Command::Scan(d) = parse(&["scan", "C:"]).unwrap() else {
            panic!("not a scan")
        };
        assert!(d.mft_bitmap);
        assert_eq!(d.io_depth, strata_ntfs::DEFAULT_IO_DEPTH);
    }

    #[test]
    fn drive_forms() {
        for s in ["c:", "C:", r"C:\"] {
            let Command::Scan(a) = parse(&["scan", s]).unwrap() else {
                panic!()
            };
            assert_eq!(a.target, Target::Drive('C'));
        }
        let Command::Scan(a) = parse(&["scan", "C:x"]).unwrap() else {
            panic!()
        };
        assert_eq!(a.target, Target::Image(PathBuf::from("C:x")));
    }

    #[test]
    fn usage_errors() {
        assert_eq!(parse(&[]).unwrap(), Command::Help);
        assert_eq!(parse(&["scan", "C:", "--help"]).unwrap(), Command::Help);
        assert!(parse(&["scan"]).is_err());
        assert!(parse(&["frob"]).is_err());
        assert!(parse(&["scan", "C:", "D:"]).is_err());
        assert!(parse(&["scan", "C:", "--top"]).is_err());
        assert!(parse(&["scan", "C:", "--chunk-mib", "0"]).is_err());
        assert!(parse(&["scan", "C:", "--io-depth", "0"]).is_err());
        assert!(parse(&["scan", "C:", "--top", "-1"]).is_err());
        assert!(parse(&["scan", "C:", "--no-buffering", "--sequential"]).is_err());
        assert!(parse(&["scan", "C:", "--bogus"]).is_err());
    }
}
