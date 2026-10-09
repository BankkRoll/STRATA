//! Bounds-checked little-endian field readers.
//!
//! Every on-disk structure in this crate is read through these helpers so a
//! truncated or hostile buffer yields `None` instead of a panic.

/// Returns `len` bytes starting at `off`, or `None` if out of range.
#[inline]
pub(crate) fn slice(b: &[u8], off: usize, len: usize) -> Option<&[u8]> {
    b.get(off..off.checked_add(len)?)
}

#[inline]
pub(crate) fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}

#[inline]
pub(crate) fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(slice(b, off, 2)?.try_into().ok()?))
}

#[inline]
pub(crate) fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(slice(b, off, 4)?.try_into().ok()?))
}

#[inline]
pub(crate) fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(slice(b, off, 8)?.try_into().ok()?))
}

#[inline]
pub(crate) fn i64_at(b: &[u8], off: usize) -> Option<i64> {
    Some(i64::from_le_bytes(slice(b, off, 8)?.try_into().ok()?))
}

/// Reads `count` UTF-16LE code units starting at `off`.
pub(crate) fn utf16_at(b: &[u8], off: usize, count: usize) -> Option<Vec<u16>> {
    let bytes = slice(b, off, count.checked_mul(2)?)?;
    Some(
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_range_reads_are_none() {
        let b = [1u8, 2, 3];
        assert_eq!(u16_at(&b, 1), Some(0x0302));
        assert_eq!(u16_at(&b, 2), None);
        assert_eq!(u32_at(&b, 0), None);
        assert_eq!(u64_at(&b, usize::MAX), None);
        assert_eq!(utf16_at(&b, 0, usize::MAX), None);
        assert_eq!(slice(&b, usize::MAX, 2), None);
    }
}
