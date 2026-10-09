//! Entry ids as the UI sees them.
//!
//! The index's [`EntryId`]s change whenever a volume is re-indexed (a scan
//! preview is replaced by the final index, or a rescan finishes), while the
//! UI keeps ids in its navigation path, selection and caches. So every id on
//! the wire carries the 2-bit *generation* of the index it came from:
//!
//! ```text
//! bit 31..30  generation (index builds of this volume, mod 4)
//! bit 29..0   index EntryId
//! ```
//!
//! When an index is replaced, the backend keeps an `old id -> new id` remap
//! for the previous generation ([`Remap`]), so ids the UI still holds resolve
//! to the same file in the new index. Ids two or more generations old no
//! longer resolve and are reported as missing.
//!
//! The encoding limits one volume to 2^30 (about 1.07 billion) entries; the
//! scan refuses larger volumes with a clear error.

use strata_index::EntryId;

/// Bits of an id holding the index entry id.
pub const LOCAL_BITS: u32 = 30;
/// Mask of the local part.
pub const LOCAL_MASK: u32 = (1 << LOCAL_BITS) - 1;
/// Largest number of id slots an index may have to be addressable.
pub const MAX_SLOTS: usize = LOCAL_MASK as usize;
/// Remap value for entries that do not exist in the new generation.
pub const GONE: u32 = u32::MAX;

/// Encodes an index id of generation `generation` for the wire.
///
/// # Example
///
/// ```
/// use strata_app_lib::ids::{decode, encode};
/// use strata_index::EntryId;
/// let wire = encode(3, EntryId(7));
/// assert_eq!(decode(wire), (3, 7));
/// ```
#[must_use]
pub const fn encode(generation: u8, id: EntryId) -> u32 {
    ((generation as u32 & 3) << LOCAL_BITS) | (id.0 & LOCAL_MASK)
}

/// Splits a wire id into `(generation, local id)`.
#[must_use]
pub const fn decode(wire: u32) -> (u8, u32) {
    ((wire >> LOCAL_BITS) as u8, wire & LOCAL_MASK)
}

/// Translation of the previous generation's ids into the current index.
#[derive(Debug, Clone, Default)]
pub struct Remap {
    /// Generation the old ids carry.
    pub generation: u8,
    /// Old local id → new local id, or [`GONE`].
    pub map: Vec<u32>,
}

impl Remap {
    /// New local id of an old local id.
    #[must_use]
    pub fn get(&self, old: u32) -> Option<u32> {
        self.map.get(old as usize).copied().filter(|&v| v != GONE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_generation() {
        for g in 0..4u8 {
            for id in [0, 1, LOCAL_MASK] {
                assert_eq!(decode(encode(g, EntryId(id))), (g, id));
            }
        }
    }

    #[test]
    fn generation_wraps_mod_4() {
        assert_eq!(decode(encode(5, EntryId(9))), (1, 9));
    }

    #[test]
    fn remap_hides_gone_entries() {
        let r = Remap {
            generation: 1,
            map: vec![4, GONE],
        };
        assert_eq!(r.get(0), Some(4));
        assert_eq!(r.get(1), None);
        assert_eq!(r.get(2), None);
    }
}
