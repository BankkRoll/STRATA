//! Verified opens, the partial and full hashes, and byte comparison.
//!
//! Every read goes through [`open_checked`], which verifies identity and
//! the placeholder gate on the handle before returning it, and every hash
//! re-reads the handle facts after the last byte so a file written while it
//! was being read is reported as changed instead of producing a hash.

use std::fs::File;
use std::io::Read;
use std::os::windows::fs::FileExt;
use std::path::Path;

use strata_clean::CancelToken;
use strata_core::{FileRef, FileTime};
use xxhash_rust::xxh3::Xxh3;

use crate::gate::{SkipReason, check_handle_attributes};
use crate::throttle::Throttle;
use crate::win::{self, Access, Facts};

/// Bytes read from each of the three partial-hash windows.
pub const PARTIAL_WINDOW: u64 = 64 * 1024;

/// Why a hash was not produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HashFail {
    Cancelled,
    Skip(SkipReason),
}

impl From<SkipReason> for HashFail {
    fn from(r: SkipReason) -> Self {
        Self::Skip(r)
    }
}

/// What the caller expects the handle to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Expect {
    pub file_ref: FileRef,
    /// `(size, mtime)` from an earlier handle observation, if any.
    pub state: Option<(u64, FileTime)>,
    pub allow_wof_and_dedup: bool,
}

/// Opens `path` (no recall, no link following), then verifies the
/// placeholder gate, identity and, when given, size and mtime.
pub(crate) fn open_checked(
    path: &Path,
    access: Access,
    expect: &Expect,
) -> Result<(File, Facts), SkipReason> {
    let file = win::open(path, access).map_err(|e| SkipReason::from_io(&e))?;
    let facts = win::facts(&file).map_err(|e| SkipReason::from_io(&e))?;
    check_handle_attributes(
        facts.attributes,
        facts.reparse_tag,
        expect.allow_wof_and_dedup,
    )?;
    if facts.index != expect.file_ref.0 {
        return Err(SkipReason::IdMismatch {
            expected: expect.file_ref.0,
            found: facts.index,
        });
    }
    if let Some((size, mtime)) = expect.state
        && (facts.size != size || facts.mtime != mtime)
    {
        return Err(SkipReason::Changed);
    }
    Ok((file, facts))
}

/// Fails with [`SkipReason::Changed`] unless the handle still shows `before`.
fn recheck(file: &File, before: &Facts) -> Result<(), SkipReason> {
    let after = win::facts(file).map_err(|e| SkipReason::from_io(&e))?;
    if after.size != before.size || after.mtime != before.mtime || after.index != before.index {
        return Err(SkipReason::Changed);
    }
    Ok(())
}

/// Offsets of the partial-hash windows for a file of `size` bytes.
///
/// Files up to three windows long are read whole. Otherwise the first,
/// middle (4 KiB aligned) and last 64 KiB are read; the middle window is
/// what separates files that share a header and a trailer, such as media
/// with fixed container metadata.
pub(crate) fn partial_windows(size: u64) -> Vec<(u64, u64)> {
    if size <= 3 * PARTIAL_WINDOW {
        return vec![(0, size)];
    }
    let mid = ((size / 2).saturating_sub(PARTIAL_WINDOW / 2)) & !4095;
    vec![
        (0, PARTIAL_WINDOW),
        (mid, PARTIAL_WINDOW),
        (size - PARTIAL_WINDOW, PARTIAL_WINDOW),
    ]
}

/// xxh3 of the size and the partial windows.
pub(crate) fn partial_hash(
    file: &File,
    facts: &Facts,
    buf: &mut Vec<u8>,
    throttle: &Throttle,
    cancel: &CancelToken,
) -> Result<u64, HashFail> {
    let mut h = Xxh3::new();
    h.update(&facts.size.to_le_bytes());
    for (off, len) in partial_windows(facts.size) {
        let len = len as usize;
        if buf.len() < len {
            buf.resize(len, 0);
        }
        if !throttle.acquire(len as u64, cancel) {
            return Err(HashFail::Cancelled);
        }
        read_exact_at(file, &mut buf[..len], off)?;
        h.update(&buf[..len]);
    }
    recheck(file, facts)?;
    Ok(h.digest())
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut off: u64) -> Result<(), SkipReason> {
    while !buf.is_empty() {
        match file.seek_read(buf, off) {
            // Shorter than the handle said a moment ago: it shrank.
            Ok(0) => return Err(SkipReason::Changed),
            Ok(n) => {
                buf = &mut buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(SkipReason::from_io(&e)),
        }
    }
    Ok(())
}

/// Streams the whole unnamed stream through BLAKE3.
pub(crate) fn full_hash(
    file: &File,
    facts: &Facts,
    buf: &mut [u8],
    throttle: &Throttle,
    cancel: &CancelToken,
    on_bytes: &dyn Fn(u64),
) -> Result<[u8; 32], HashFail> {
    let mut h = blake3::Hasher::new();
    let mut reader = file;
    let mut total = 0u64;
    loop {
        if cancel.is_cancelled() {
            return Err(HashFail::Cancelled);
        }
        let want = buf.len().min(
            usize::try_from(facts.size - total)
                .unwrap_or(usize::MAX)
                .max(1),
        );
        if !throttle.acquire(want as u64, cancel) {
            return Err(HashFail::Cancelled);
        }
        let n = match reader.read(&mut buf[..want]) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(SkipReason::from_io(&e).into()),
        };
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > facts.size {
            return Err(SkipReason::Changed.into());
        }
        h.update(&buf[..n]);
        on_bytes(n as u64);
    }
    if total != facts.size {
        return Err(SkipReason::Changed.into());
    }
    recheck(file, facts)?;
    Ok(*h.finalize().as_bytes())
}

/// Compares two open files byte for byte. Both must show `size` bytes.
pub(crate) fn same_bytes(
    a: (&File, &Facts),
    b: (&File, &Facts),
    buf: &mut [u8],
    throttle: &Throttle,
    cancel: &CancelToken,
) -> Result<bool, HashFail> {
    if a.1.size != b.1.size {
        return Ok(false);
    }
    let half = (buf.len() / 2).max(1);
    let (ba, bb) = buf.split_at_mut(half);
    let mut off = 0u64;
    while off < a.1.size {
        if cancel.is_cancelled() {
            return Err(HashFail::Cancelled);
        }
        let n = ba
            .len()
            .min(usize::try_from(a.1.size - off).unwrap_or(usize::MAX));
        if !throttle.acquire(2 * n as u64, cancel) {
            return Err(HashFail::Cancelled);
        }
        read_exact_at(a.0, &mut ba[..n], off)?;
        read_exact_at(b.0, &mut bb[..n], off)?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
        off += n as u64;
    }
    recheck(a.0, a.1)?;
    recheck(b.0, b.1)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_cover_small_files_whole() {
        assert_eq!(partial_windows(1), vec![(0, 1)]);
        assert_eq!(
            partial_windows(3 * PARTIAL_WINDOW),
            vec![(0, 3 * PARTIAL_WINDOW)]
        );
    }

    #[test]
    fn windows_are_first_middle_last() {
        let size = 10 * 1024 * 1024 + 3;
        let w = partial_windows(size);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0], (0, PARTIAL_WINDOW));
        assert_eq!(w[1].0 % 4096, 0);
        assert!(w[1].0 > PARTIAL_WINDOW && w[1].0 + PARTIAL_WINDOW < size - PARTIAL_WINDOW);
        assert_eq!(w[2], (size - PARTIAL_WINDOW, PARTIAL_WINDOW));
        for (off, len) in partial_windows(3 * PARTIAL_WINDOW + 1) {
            assert!(off + len <= 3 * PARTIAL_WINDOW + 1);
        }
    }
}
