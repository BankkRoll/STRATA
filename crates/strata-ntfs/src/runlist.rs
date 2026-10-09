//! Runlist (mapping pairs) decoding and encoding.
//!
//! A non-resident attribute stores its clusters as a sequence of runs. Each
//! run starts with a header byte: the low nibble is the size of the length
//! field, the high nibble the size of the LCN offset field. The offset is
//! signed and relative to the previous run's LCN; an offset size of 0 is a
//! sparse run. A zero header byte ends the list.

use crate::error::RunlistError;

/// Upper bound on runs in one attribute instance. A 64 KiB record holds at
/// most ~32k two-byte runs, so anything larger is corrupt.
pub const MAX_RUNS: usize = 65_536;

/// One contiguous extent of an attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// First virtual cluster of the run within the attribute.
    pub vcn: u64,
    /// First logical cluster on the volume; `None` for a sparse run.
    pub lcn: Option<u64>,
    /// Length in clusters (never zero).
    pub len: u64,
}

impl Run {
    /// One past the last VCN of the run.
    #[must_use]
    pub fn end_vcn(&self) -> u64 {
        self.vcn.saturating_add(self.len)
    }
}

/// Decodes a mapping pairs array.
///
/// `start_vcn` is the attribute instance's first VCN; `total_clusters`
/// bounds every LCN so a corrupt run can never point past the volume.
///
/// # Errors
///
/// Any [`RunlistError`]: truncated input, field sizes above 8, zero or
/// negative lengths, LCNs outside `0..total_clusters`, overflow, or more
/// than [`MAX_RUNS`] runs.
///
/// # Example
///
/// ```
/// use strata_ntfs::{decode_runlist, Run};
/// // 0x21: 1-byte length, 2-byte offset. 0x01: sparse run of 4 clusters.
/// let bytes = [0x21, 0x08, 0x00, 0x10, 0x01, 0x04, 0x00];
/// let runs = decode_runlist(&bytes, 0, 100_000).unwrap();
/// assert_eq!(runs, vec![
///     Run { vcn: 0, lcn: Some(0x1000), len: 8 },
///     Run { vcn: 8, lcn: None, len: 4 },
/// ]);
/// ```
pub fn decode_runlist(
    bytes: &[u8],
    start_vcn: u64,
    total_clusters: u64,
) -> Result<Vec<Run>, RunlistError> {
    let mut runs = Vec::new();
    let mut pos = 0usize;
    let mut vcn = start_vcn;
    let mut lcn: i64 = 0;
    loop {
        let header = *bytes.get(pos).ok_or(RunlistError::Truncated)?;
        if header == 0 {
            return Ok(runs);
        }
        if runs.len() >= MAX_RUNS {
            return Err(RunlistError::TooManyRuns(MAX_RUNS));
        }
        let len_size = header & 0x0F;
        let off_size = header >> 4;
        if len_size == 0 || len_size > 8 {
            return Err(RunlistError::FieldSize(len_size));
        }
        if off_size > 8 {
            return Err(RunlistError::FieldSize(off_size));
        }
        pos += 1;
        let len_bytes = bytes
            .get(pos..pos + usize::from(len_size))
            .ok_or(RunlistError::Truncated)?;
        let len = read_signed(len_bytes);
        if len <= 0 {
            return Err(RunlistError::BadLength);
        }
        let len = len as u64;
        pos += usize::from(len_size);
        let run_lcn = if off_size == 0 {
            None
        } else {
            let off_bytes = bytes
                .get(pos..pos + usize::from(off_size))
                .ok_or(RunlistError::Truncated)?;
            pos += usize::from(off_size);
            lcn = lcn
                .checked_add(read_signed(off_bytes))
                .ok_or(RunlistError::Overflow)?;
            let in_volume = u64::try_from(lcn)
                .ok()
                .and_then(|l| l.checked_add(len))
                .is_some_and(|end| end <= total_clusters);
            if !in_volume {
                return Err(RunlistError::OutOfVolume { lcn, len });
            }
            Some(lcn as u64)
        };
        runs.push(Run {
            vcn,
            lcn: run_lcn,
            len,
        });
        vcn = vcn.checked_add(len).ok_or(RunlistError::Overflow)?;
    }
}

/// Sign-extends a little-endian integer of 1..=8 bytes.
fn read_signed(b: &[u8]) -> i64 {
    let mut buf = [0u8; 8];
    buf[..b.len()].copy_from_slice(b);
    if b.last().is_some_and(|&x| x & 0x80 != 0) {
        for x in &mut buf[b.len()..] {
            *x = 0xFF;
        }
    }
    i64::from_le_bytes(buf)
}

/// Encodes runs as a mapping pairs array, including the terminating zero.
///
/// Runs must be in VCN order; their `vcn` fields are not stored (they are
/// implied by the order and the attribute's start VCN).
///
/// # Example
///
/// ```
/// use strata_ntfs::{decode_runlist, encode_runlist, Run};
/// let runs = vec![
///     Run { vcn: 0, lcn: Some(5000), len: 3 },
///     Run { vcn: 3, lcn: None, len: 10 },
///     Run { vcn: 13, lcn: Some(100), len: 300 },
/// ];
/// let bytes = encode_runlist(&runs);
/// assert_eq!(decode_runlist(&bytes, 0, 10_000).unwrap(), runs);
/// ```
#[must_use]
pub fn encode_runlist(runs: &[Run]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev: i64 = 0;
    for r in runs {
        let len = signed_bytes(r.len as i64);
        let off = match r.lcn {
            None => Vec::new(),
            Some(l) => {
                let delta = (l as i64).wrapping_sub(prev);
                prev = l as i64;
                signed_bytes(delta)
            }
        };
        out.push(((off.len() as u8) << 4) | len.len() as u8);
        out.extend_from_slice(&len);
        out.extend_from_slice(&off);
    }
    out.push(0);
    out
}

/// Minimal little-endian two's complement encoding of `v` (at least 1 byte).
fn signed_bytes(v: i64) -> Vec<u8> {
    let full = v.to_le_bytes();
    let mut n = 8;
    while n > 1 {
        let top = full[n - 1];
        let next_sign = full[n - 2] & 0x80 != 0;
        if (top == 0x00 && !next_sign) || (top == 0xFF && next_sign) {
            n -= 1;
        } else {
            break;
        }
    }
    full[..n].to_vec()
}

/// Number of clusters actually on disk (sparse runs excluded).
#[must_use]
pub fn allocated_clusters(runs: &[Run]) -> u64 {
    runs.iter()
        .filter(|r| r.lcn.is_some())
        .fold(0u64, |acc, r| acc.saturating_add(r.len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_offsets_move_backwards() {
        // 0x11 len 4 off +0x40; 0x11 len 2 off -0x10 (0xF0).
        let runs = decode_runlist(&[0x11, 4, 0x40, 0x11, 2, 0xF0, 0], 7, 1000).unwrap();
        assert_eq!(
            runs,
            vec![
                Run {
                    vcn: 7,
                    lcn: Some(0x40),
                    len: 4
                },
                Run {
                    vcn: 11,
                    lcn: Some(0x30),
                    len: 2
                },
            ]
        );
    }

    #[test]
    fn rejects_malformed_lists() {
        assert_eq!(decode_runlist(&[], 0, 10), Err(RunlistError::Truncated));
        assert_eq!(
            decode_runlist(&[0x11, 1], 0, 10),
            Err(RunlistError::Truncated)
        );
        assert_eq!(
            decode_runlist(&[0x10, 1, 0], 0, 10),
            Err(RunlistError::FieldSize(0))
        );
        assert_eq!(
            decode_runlist(&[0x19], 0, 10),
            Err(RunlistError::FieldSize(9))
        );
        assert_eq!(
            decode_runlist(&[0x91, 1], 0, 10),
            Err(RunlistError::FieldSize(9))
        );
        assert_eq!(
            decode_runlist(&[0x11, 0, 1, 0], 0, 10),
            Err(RunlistError::BadLength)
        );
        assert_eq!(
            decode_runlist(&[0x11, 0xFF, 1, 0], 0, 10),
            Err(RunlistError::BadLength)
        );
        assert!(matches!(
            decode_runlist(&[0x11, 5, 8, 0], 0, 10),
            Err(RunlistError::OutOfVolume { .. })
        ));
        assert!(matches!(
            decode_runlist(&[0x11, 1, 0xFF, 0], 0, 10),
            Err(RunlistError::OutOfVolume { lcn: -1, .. })
        ));
        assert_eq!(
            decode_runlist(&[0x01, 1, 0], u64::MAX, 10),
            Err(RunlistError::Overflow)
        );
    }

    #[test]
    fn round_trips_wide_values() {
        let runs = vec![
            Run {
                vcn: 0,
                lcn: Some(0x7F),
                len: 0x80,
            },
            Run {
                vcn: 0x80,
                lcn: Some(0x1_0000_0000),
                len: 1,
            },
            Run {
                vcn: 0x81,
                lcn: Some(1),
                len: 0x7FFF_FFFF,
            },
            Run {
                vcn: 0x8000_0080,
                lcn: None,
                len: 0xFF,
            },
        ];
        let enc = encode_runlist(&runs);
        assert_eq!(decode_runlist(&enc, 0, u64::MAX).unwrap(), runs);
        assert_eq!(allocated_clusters(&runs), 0x80 + 1 + 0x7FFF_FFFF);
    }

    #[test]
    fn too_many_runs_is_rejected() {
        let mut bytes = Vec::new();
        for _ in 0..=MAX_RUNS {
            bytes.extend_from_slice(&[0x01, 1]);
        }
        bytes.push(0);
        assert_eq!(
            decode_runlist(&bytes, 0, 10),
            Err(RunlistError::TooManyRuns(MAX_RUNS))
        );
    }
}
