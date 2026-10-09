//! Packed encoding of one snapshot's directory aggregates.
//!
//! A snapshot stores its rows as a single blob instead of one SQL row per
//! directory; the benchmark in `tests/bench.rs` measured 12 bytes per
//! directory against 29 for the best `WITHOUT ROWID` table layout.
//!
//! Format (codec 1), all integers unsigned LEB128:
//!
//! ```text
//! count
//! repeat count times, rows sorted by path id ascending:
//!     id - previous id         (previous starts at 0; must be >= 1)
//!     allocated tag            (allocated / 4096 * 2 when cluster-aligned,
//!                               else allocated * 2 + 1)
//!     zigzag(logical - allocated)
//!     files
//! ```
//!
//! Path ids are dense `paths.id` values, so sorted deltas are mostly 1 byte.
//! Allocated sizes are almost always multiples of 4 KiB, so dividing saves
//! about 1.5 bytes per row, and logical is usually close to allocated.
//!
//! Decoding is fully bounds-checked: a damaged blob yields [`CodecError`],
//! never a panic or an unbounded allocation.

use crate::snapshot::DirSizes;

/// Codec id stored in `snapshot_dirs.codec`.
pub(crate) const CODEC_V1: i64 = 1;

const CLUSTER: u128 = 4096;

/// The blob is malformed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodecError(pub &'static str);

fn put_varint(out: &mut Vec<u8>, mut v: u128) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn varint(&mut self) -> Result<u128, CodecError> {
        let mut v: u128 = 0;
        let mut shift = 0u32;
        loop {
            let byte = *self
                .data
                .get(self.pos)
                .ok_or(CodecError("truncated varint"))?;
            self.pos += 1;
            if shift >= 128 || (shift == 126 && byte > 0x03) {
                return Err(CodecError("varint overflow"));
            }
            v |= u128::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    fn u64(&mut self) -> Result<u64, CodecError> {
        u64::try_from(self.varint()?).map_err(|_| CodecError("value exceeds u64"))
    }
}

fn zigzag(v: i128) -> u128 {
    ((v << 1) ^ (v >> 127)) as u128
}

fn unzigzag(v: u128) -> i128 {
    ((v >> 1) as i128) ^ -((v & 1) as i128)
}

/// Encodes rows. `rows` must be sorted by id ascending with unique ids >= 1.
pub(crate) fn encode(rows: &[(i64, DirSizes)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rows.len() * 10 + 4);
    put_varint(&mut out, rows.len() as u128);
    let mut prev = 0i64;
    for (id, s) in rows {
        put_varint(&mut out, (id - prev) as u128);
        prev = *id;
        let alloc = u128::from(s.allocated);
        let tag = if alloc % CLUSTER == 0 {
            alloc / CLUSTER * 2
        } else {
            alloc * 2 + 1
        };
        put_varint(&mut out, tag);
        put_varint(
            &mut out,
            zigzag(i128::from(s.logical) - i128::from(s.allocated)),
        );
        put_varint(&mut out, u128::from(s.files));
    }
    out
}

/// Decodes every row, calling `f(id, sizes)` in id order. Stops early when
/// `f` returns `false`.
pub(crate) fn for_each(
    data: &[u8],
    mut f: impl FnMut(i64, DirSizes) -> bool,
) -> Result<(), CodecError> {
    let mut r = Reader { data, pos: 0 };
    let count = r.u64()?;
    let mut prev: i64 = 0;
    for _ in 0..count {
        let delta = r.u64()?;
        if delta == 0 {
            return Err(CodecError("ids not strictly increasing"));
        }
        let id = i64::try_from(delta)
            .ok()
            .and_then(|d| prev.checked_add(d))
            .ok_or(CodecError("id overflow"))?;
        prev = id;
        let tag = r.varint()?;
        let allocated = if tag & 1 == 0 {
            (tag >> 1).checked_mul(CLUSTER)
        } else {
            Some(tag >> 1)
        }
        .and_then(|a| u64::try_from(a).ok())
        .ok_or(CodecError("allocated exceeds u64"))?;
        let logical = u64::try_from(i128::from(allocated) + unzigzag(r.varint()?))
            .map_err(|_| CodecError("logical out of range"))?;
        let files = r.u64()?;
        if !f(
            id,
            DirSizes {
                allocated,
                logical,
                files,
            },
        ) {
            return Ok(());
        }
    }
    if r.pos != data.len() {
        return Err(CodecError("trailing bytes"));
    }
    Ok(())
}

/// Decodes all rows.
pub(crate) fn decode(data: &[u8]) -> Result<Vec<(i64, DirSizes)>, CodecError> {
    // Each row takes at least 4 bytes, which bounds the preallocation for a
    // hostile count prefix.
    let mut rows = Vec::with_capacity(data.len() / 4);
    for_each(data, |id, s| {
        rows.push((id, s));
        true
    })?;
    Ok(rows)
}

/// Finds one id without materializing the rest.
pub(crate) fn find(data: &[u8], wanted: i64) -> Result<Option<DirSizes>, CodecError> {
    let mut found = None;
    for_each(data, |id, s| {
        if id == wanted {
            found = Some(s);
        }
        id < wanted
    })?;
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sizes(allocated: u64, logical: u64, files: u64) -> DirSizes {
        DirSizes {
            allocated,
            logical,
            files,
        }
    }

    #[test]
    fn round_trips_edge_values() {
        let rows = vec![
            (1, sizes(0, 0, 0)),
            (2, sizes(4096, 1, 1)),
            (300, sizes(u64::MAX, 0, u64::MAX)),
            (301, sizes(0, u64::MAX, 7)),
            (i64::MAX, sizes(u64::MAX - 4095, u64::MAX, 0)),
        ];
        let blob = encode(&rows);
        assert_eq!(decode(&blob).unwrap(), rows);
        assert_eq!(find(&blob, 300).unwrap(), Some(rows[2].1));
        assert_eq!(find(&blob, 299).unwrap(), None);
    }

    #[test]
    fn empty_blob() {
        assert_eq!(decode(&encode(&[])).unwrap(), vec![]);
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn rejects_damage() {
        let blob = encode(&[(1, sizes(8192, 8000, 2)), (2, sizes(1, 1, 1))]);
        for cut in 0..blob.len() {
            assert!(decode(&blob[..cut]).is_err(), "cut at {cut}");
        }
        let mut extra = blob.clone();
        extra.push(0);
        assert!(decode(&extra).is_err());
        assert!(decode(&[0xFF; 40]).is_err());
        // count = 2 but second id delta is 0
        assert!(decode(&[2, 1, 0, 0, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn typical_rows_are_compact() {
        let rows: Vec<_> = (1..=1000)
            .map(|i| (i, sizes(i as u64 * 1_048_576, i as u64 * 1_000_000, 40)))
            .collect();
        let blob = encode(&rows);
        assert!(blob.len() < 1000 * 10, "{} bytes", blob.len());
    }

    proptest! {
        #[test]
        fn round_trips(mut rows in prop::collection::vec(
            (1i64..1_000_000, any::<u64>(), any::<u64>(), any::<u64>()), 0..200)
        ) {
            rows.sort_by_key(|r| r.0);
            rows.dedup_by_key(|r| r.0);
            let rows: Vec<_> = rows.into_iter().map(|(id, a, l, f)| (id, sizes(a, l, f))).collect();
            prop_assert_eq!(decode(&encode(&rows)).unwrap(), rows);
        }

        #[test]
        fn never_panics_on_garbage(data in prop::collection::vec(any::<u8>(), 0..256)) {
            let _ = decode(&data);
            let _ = find(&data, 5);
        }
    }
}
