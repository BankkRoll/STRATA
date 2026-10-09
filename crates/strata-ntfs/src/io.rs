//! Positioned reads from images and raw volumes.
//!
//! Responsibilities:
//! - [`ReadAt`]: the one I/O trait the parser layer needs.
//! - Implementations for in-memory images (`[u8]`, `Vec<u8>`), image files
//!   ([`std::fs::File`]) and raw Windows volumes ([`RawVolume`]).
//! - [`AlignedBuf`]: sector-aligned buffers for `FILE_FLAG_NO_BUFFERING`.
//!
//! Everything here is safe Rust: Windows open flags go through
//! `OpenOptionsExt` and reads through `FileExt::seek_read`.

use std::fs::File;
use std::io;
use std::path::Path;

/// Positioned, shared-reference reads. Implementations must be safe to call
/// from several threads at once when they are `Sync`.
pub trait ReadAt {
    /// Fills `buf` with the bytes at `offset`. A short read is an error
    /// ([`io::ErrorKind::UnexpectedEof`]).
    ///
    /// # Errors
    ///
    /// Any I/O failure, or the range extends past the end of the source.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Buffer address, offset and length alignment that lets reads skip
    /// bounce buffering. 1 means "no requirement".
    fn alignment(&self) -> usize {
        1
    }
}

impl<T: ReadAt + ?Sized> ReadAt for &T {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(offset, buf)
    }

    fn alignment(&self) -> usize {
        (**self).alignment()
    }
}

impl ReadAt for [u8] {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = usize::try_from(offset).map_err(|_| eof())?;
        let end = start.checked_add(buf.len()).ok_or_else(eof)?;
        let src = self.get(start..end).ok_or_else(eof)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

impl ReadAt for Vec<u8> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.as_slice().read_at(offset, buf)
    }
}

impl ReadAt for File {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        read_exact_at(self, offset, buf)
    }
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "read past end of image")
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        // NOTE: seek_read moves the file cursor, which is harmless here because
        // every read in this crate is positioned.
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(eof()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset = offset.checked_add(n as u64).ok_or_else(eof)?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_exact_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

/// A heap buffer whose usable slice starts at a given power-of-two alignment.
///
/// `FILE_FLAG_NO_BUFFERING` requires the buffer address, the file offset and
/// the length to be sector multiples. Rust has no safe aligned allocation for
/// `u8`, so this over-allocates by `align` bytes and slices at the first
/// aligned address.
///
/// # Example
///
/// ```
/// use strata_ntfs::AlignedBuf;
/// let mut b = AlignedBuf::new(8192, 4096);
/// assert_eq!(b.as_mut_slice().len(), 8192);
/// assert_eq!(b.as_slice().as_ptr() as usize % 4096, 0);
/// ```
#[derive(Debug)]
pub struct AlignedBuf {
    storage: Vec<u8>,
    start: usize,
    len: usize,
}

impl AlignedBuf {
    /// Allocates `len` zeroed bytes aligned to `align` (rounded up to a power of two).
    #[must_use]
    pub fn new(len: usize, align: usize) -> Self {
        let align = align.max(1).next_power_of_two();
        let storage = vec![0u8; len.saturating_add(align)];
        let start = match storage.as_ptr().align_offset(align) {
            off if off < align => off,
            _ => 0,
        };
        Self {
            storage,
            start,
            len,
        }
    }

    /// The aligned bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.storage[self.start..self.start + self.len]
    }

    /// The aligned bytes, mutably.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.storage[self.start..self.start + self.len]
    }

    /// Usable length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the usable length is zero.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// How a raw volume handle is opened. Both are offered so they can be
/// benchmarked against each other on real hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IoMode {
    /// `FILE_FLAG_NO_BUFFERING`: bypasses the cache; every read must be
    /// sector aligned. Unaligned requests go through a bounce buffer.
    #[default]
    NoBuffering,
    /// `FILE_FLAG_SEQUENTIAL_SCAN`: cached reads with aggressive read-ahead.
    Sequential,
}

/// Win32 `FILE_SHARE_READ | FILE_SHARE_WRITE`: other processes keep using the volume.
const SHARE_READ_WRITE: u32 = 0x1 | 0x2;
/// Win32 `FILE_FLAG_NO_BUFFERING`.
const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
/// Win32 `FILE_FLAG_SEQUENTIAL_SCAN`.
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

/// Alignment used for unbuffered reads. 4096 is a multiple of both 512-byte
/// and 4Kn sector sizes, so it is valid without querying the device.
pub const RAW_ALIGNMENT: usize = 4096;

/// A raw NTFS volume (`\\.\X:`) or any file opened with volume-style flags.
///
/// Opening a volume requires administrator rights.
#[derive(Debug)]
pub struct RawVolume {
    file: File,
    mode: IoMode,
    align: usize,
}

impl RawVolume {
    /// Opens `\\.\X:` for drive letter `letter`.
    ///
    /// # Errors
    ///
    /// `letter` is not ASCII alphabetic, or the open fails (typically
    /// access denied when not elevated).
    pub fn open_drive(letter: char, mode: IoMode) -> io::Result<Self> {
        if !letter.is_ascii_alphabetic() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "drive letter must be A-Z",
            ));
        }
        Self::open(format!(r"\\.\{}:", letter.to_ascii_uppercase()), mode)
    }

    /// Opens a device or file path with read access, read/write sharing and
    /// the flags for `mode`.
    ///
    /// # Errors
    ///
    /// The open fails.
    pub fn open(path: impl AsRef<Path>, mode: IoMode) -> io::Result<Self> {
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            opts.share_mode(SHARE_READ_WRITE);
            opts.custom_flags(match mode {
                IoMode::NoBuffering => FILE_FLAG_NO_BUFFERING,
                IoMode::Sequential => FILE_FLAG_SEQUENTIAL_SCAN,
            });
        }
        Ok(Self::from_file(opts.open(path)?, mode, RAW_ALIGNMENT))
    }

    /// Wraps an already-open handle. `align` is the sector alignment honoured
    /// in [`IoMode::NoBuffering`] mode.
    #[must_use]
    pub fn from_file(file: File, mode: IoMode, align: usize) -> Self {
        Self {
            file,
            mode,
            align: align.max(1).next_power_of_two(),
        }
    }

    /// The mode this volume was opened with.
    #[must_use]
    pub fn mode(&self) -> IoMode {
        self.mode
    }
}

impl ReadAt for RawVolume {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        match self.mode {
            IoMode::Sequential => read_exact_at(&self.file, offset, buf),
            IoMode::NoBuffering => aligned_read(self.align, offset, buf, |o, b| {
                read_exact_at(&self.file, o, b)
            }),
        }
    }

    fn alignment(&self) -> usize {
        match self.mode {
            IoMode::NoBuffering => self.align,
            IoMode::Sequential => 1,
        }
    }
}

/// Performs a read that honours `align` for offset, length and buffer
/// address, bouncing through an [`AlignedBuf`] when the request is not
/// already aligned.
pub(crate) fn aligned_read(
    align: usize,
    offset: u64,
    buf: &mut [u8],
    mut raw: impl FnMut(u64, &mut [u8]) -> io::Result<()>,
) -> io::Result<()> {
    let a = align as u64;
    let direct = offset.is_multiple_of(a)
        && buf.len().is_multiple_of(align)
        && buf.as_ptr().align_offset(align) == 0;
    if direct {
        return raw(offset, buf);
    }
    let start = offset - offset % a;
    let end = offset
        .checked_add(buf.len() as u64)
        .and_then(|e| e.checked_next_multiple_of(a))
        .ok_or_else(eof)?;
    let span = usize::try_from(end - start).map_err(|_| eof())?;
    let mut bounce = AlignedBuf::new(span, align);
    raw(start, bounce.as_mut_slice())?;
    let skip = (offset - start) as usize;
    buf.copy_from_slice(&bounce.as_slice()[skip..skip + buf.len()]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_reads_and_rejects_past_end() {
        let img: Vec<u8> = (0..=255).collect();
        let mut b = [0u8; 4];
        img.read_at(10, &mut b).unwrap();
        assert_eq!(b, [10, 11, 12, 13]);
        assert!(img.read_at(254, &mut b).is_err());
        assert!(img.read_at(u64::MAX, &mut b).is_err());
    }

    #[test]
    fn aligned_buffer_is_aligned() {
        for align in [1, 512, 4096] {
            let b = AlignedBuf::new(100, align);
            assert_eq!(b.as_slice().as_ptr() as usize % align, 0);
            assert_eq!(b.len(), 100);
        }
    }

    #[test]
    fn unaligned_requests_bounce_through_aligned_reads() {
        let img: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
        let mut calls = Vec::new();
        let mut out = vec![0u8; 700];
        aligned_read(512, 300, &mut out, |o, b| {
            calls.push((o, b.len(), b.as_ptr() as usize % 512));
            img.read_at(o, b)
        })
        .unwrap();
        assert_eq!(out, img[300..1000]);
        assert_eq!(calls, vec![(0, 1024, 0)]);
    }

    #[test]
    fn aligned_requests_read_directly() {
        let img = vec![7u8; 4096];
        let mut buf = AlignedBuf::new(1024, 512);
        let mut calls = 0;
        aligned_read(512, 512, buf.as_mut_slice(), |o, b| {
            calls += 1;
            img.read_at(o, b)
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert!(buf.as_slice().iter().all(|&x| x == 7));
    }
}
