//! Update sequence array (fixup) verification and removal.
//!
//! NTFS writes a 16-bit update sequence number into the last two bytes of
//! every 512-byte stride of a multi-sector record and saves the displaced
//! bytes in the update sequence array. If any stride's tail does not match,
//! the record was torn by an interrupted write.

use crate::le::u16_at;

/// Fixup stride. Always 512 bytes, regardless of the device sector size.
pub const FIXUP_STRIDE: usize = 512;

/// Why fixups could not be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixupError {
    /// The USA offset/count disagree with the record size.
    BadArray,
    /// A stride's tail does not match the update sequence number: torn write.
    Mismatch {
        /// Index (1-based) of the offending stride.
        stride: usize,
    },
}

/// Verifies every stride and restores the original bytes in place.
///
/// `rec` must be exactly one record. On error the buffer may be partially
/// modified and must be discarded.
///
/// # Errors
///
/// [`FixupError::BadArray`] when `usa_count - 1 != rec.len() / 512` or the
/// array does not fit in the first stride; [`FixupError::Mismatch`] for a
/// torn record.
pub fn apply_fixups(rec: &mut [u8]) -> Result<(), FixupError> {
    let usa_off = usize::from(u16_at(rec, 4).ok_or(FixupError::BadArray)?);
    let usa_count = usize::from(u16_at(rec, 6).ok_or(FixupError::BadArray)?);
    if rec.is_empty()
        || !rec.len().is_multiple_of(FIXUP_STRIDE)
        || usa_count != rec.len() / FIXUP_STRIDE + 1
    {
        return Err(FixupError::BadArray);
    }
    // The array must lie inside the first stride, clear of the stride's own tail.
    let usa_end = usa_count
        .checked_mul(2)
        .and_then(|n| n.checked_add(usa_off))
        .ok_or(FixupError::BadArray)?;
    if usa_off < 8 || usa_end > FIXUP_STRIDE - 2 {
        return Err(FixupError::BadArray);
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_count {
        let tail = i * FIXUP_STRIDE - 2;
        if rec[tail..tail + 2] != usn {
            return Err(FixupError::Mismatch { stride: i });
        }
        let saved = usa_off + 2 * i;
        let orig = [rec[saved], rec[saved + 1]];
        rec[tail..tail + 2].copy_from_slice(&orig);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protected(size: usize, usn: u16) -> (Vec<u8>, Vec<u8>) {
        let mut rec: Vec<u8> = (0..size).map(|i| (i * 7 % 256) as u8).collect();
        rec[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        let count = size / FIXUP_STRIDE + 1;
        rec[6..8].copy_from_slice(&(count as u16).to_le_bytes());
        let original = rec.clone();
        rec[0x30..0x32].copy_from_slice(&usn.to_le_bytes());
        for i in 1..count {
            let tail = i * FIXUP_STRIDE - 2;
            let (a, b) = (rec[tail], rec[tail + 1]);
            rec[0x30 + 2 * i] = a;
            rec[0x30 + 2 * i + 1] = b;
            rec[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
        }
        (rec, original)
    }

    #[test]
    fn restores_original_tails() {
        for size in [512, 1024, 4096] {
            let (mut rec, orig) = protected(size, 0x0102);
            apply_fixups(&mut rec).unwrap();
            for i in 1..=size / FIXUP_STRIDE {
                let t = i * FIXUP_STRIDE - 2;
                assert_eq!(rec[t..t + 2], orig[t..t + 2]);
            }
        }
    }

    #[test]
    fn detects_torn_stride() {
        let (mut rec, _) = protected(4096, 0x0102);
        rec[3 * FIXUP_STRIDE - 1] ^= 0xFF;
        assert_eq!(
            apply_fixups(&mut rec),
            Err(FixupError::Mismatch { stride: 3 })
        );
    }

    #[test]
    fn rejects_inconsistent_arrays() {
        let (mut rec, _) = protected(1024, 1);
        rec[6] = 5;
        assert_eq!(apply_fixups(&mut rec), Err(FixupError::BadArray));
        let (mut rec, _) = protected(1024, 1);
        rec[4..6].copy_from_slice(&600u16.to_le_bytes());
        assert_eq!(apply_fixups(&mut rec), Err(FixupError::BadArray));
        assert_eq!(apply_fixups(&mut []), Err(FixupError::BadArray));
        assert_eq!(apply_fixups(&mut [0u8; 100]), Err(FixupError::BadArray));
    }
}
