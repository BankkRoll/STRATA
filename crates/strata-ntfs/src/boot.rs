//! NTFS boot sector (`$Boot`, sector 0) parsing and validation.

use crate::error::{NtfsError, Result};
use crate::le::{i64_at, u8_at, u16_at, u64_at};

/// Largest cluster size NTFS supports (Windows 10 1709+).
pub const MAX_CLUSTER_SIZE: u64 = 2 * 1024 * 1024;

/// Geometry decoded from the NTFS boot sector.
///
/// # Example
///
/// ```
/// use strata_ntfs::BootSector;
/// let mut s = [0u8; 512];
/// s[3..11].copy_from_slice(b"NTFS    ");
/// s[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
/// s[0x0D] = 8; // 4 KiB clusters
/// s[0x28..0x30].copy_from_slice(&80_000u64.to_le_bytes());
/// s[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
/// s[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
/// s[0x40] = 0xF6; // -10: 1024-byte records
/// s[0x44] = 1;
/// s[0x1FE] = 0x55;
/// s[0x1FF] = 0xAA;
/// let b = BootSector::parse(&s).unwrap();
/// assert_eq!(b.cluster_size, 4096);
/// assert_eq!(b.record_size, 1024);
/// assert_eq!(b.total_clusters, 10_000);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootSector {
    /// Bytes per sector (256..=4096, power of two).
    pub bytes_per_sector: u32,
    /// Bytes per cluster (sector size .. 2 MiB, power of two).
    pub cluster_size: u64,
    /// Total sectors in the volume, as recorded at 0x28.
    pub total_sectors: u64,
    /// Total clusters (`total_sectors / sectors_per_cluster`).
    pub total_clusters: u64,
    /// First cluster of `$MFT`.
    pub mft_lcn: u64,
    /// First cluster of `$MFTMirr`.
    pub mft_mirror_lcn: u64,
    /// Bytes per MFT record (512..=65536, power of two).
    pub record_size: u32,
    /// Bytes per index record, if the encoded value is sane. Unused by the
    /// scanner, so an odd value does not reject the volume.
    pub index_record_size: Option<u32>,
    /// Volume serial number.
    pub serial: u64,
}

impl BootSector {
    /// Bytes needed to parse a boot sector.
    pub const LEN: usize = 512;

    /// Parses and validates a boot sector.
    ///
    /// # Errors
    ///
    /// [`NtfsError::Boot`] when the buffer is short, the OEM id is not
    /// `"NTFS    "`, or any geometry field is out of range.
    pub fn parse(b: &[u8]) -> Result<Self> {
        let bad = NtfsError::Boot;
        if b.len() < Self::LEN {
            return Err(bad("buffer shorter than 512 bytes"));
        }
        if b.get(3..11) != Some(b"NTFS    ".as_slice()) {
            return Err(bad("OEM id is not \"NTFS    \""));
        }
        if b.get(0x1FE..0x200) != Some([0x55, 0xAA].as_slice()) {
            return Err(bad("missing 0x55AA end-of-sector marker"));
        }
        let bps = u32::from(u16_at(b, 0x0B).ok_or(bad("short"))?);
        if !(256..=4096).contains(&bps) || !bps.is_power_of_two() {
            return Err(bad("bytes per sector must be a power of two in 256..=4096"));
        }
        let spc_raw = u8_at(b, 0x0D).ok_or(bad("short"))?;
        let spc: u64 = match spc_raw {
            0 => return Err(bad("sectors per cluster is zero")),
            1..=0x80 => u64::from(spc_raw),
            // Values above 0x80 encode 2^(256-n), used for clusters larger than 64 KiB.
            n => {
                let shift = 256 - u32::from(n);
                if shift > 31 {
                    return Err(bad("sectors per cluster exponent too large"));
                }
                1u64 << shift
            }
        };
        if !spc.is_power_of_two() {
            return Err(bad("sectors per cluster must be a power of two"));
        }
        let cluster_size = spc * u64::from(bps);
        if cluster_size > MAX_CLUSTER_SIZE {
            return Err(bad("cluster size exceeds 2 MiB"));
        }
        let total_sectors = u64_at(b, 0x28).ok_or(bad("short"))?;
        let total_clusters = total_sectors / spc;
        if total_clusters == 0 {
            return Err(bad("volume has no clusters"));
        }
        let mft_lcn = u64_at(b, 0x30).ok_or(bad("short"))?;
        let mft_mirror_lcn = u64_at(b, 0x38).ok_or(bad("short"))?;
        if mft_lcn >= total_clusters || mft_mirror_lcn >= total_clusters {
            return Err(bad("$MFT or $MFTMirr LCN is past the end of the volume"));
        }
        let record_size = decode_record_size(u8_at(b, 0x40).ok_or(bad("short"))?, cluster_size)
            .ok_or(bad("MFT record size encoding is invalid"))?;
        // NOTE: the update sequence stride is fixed at 512 bytes, so a record
        // smaller than 512 bytes could not carry a valid fixup array.
        if !(512..=65536).contains(&record_size) || !record_size.is_power_of_two() {
            return Err(bad("MFT record size must be a power of two in 512..=65536"));
        }
        let index_record_size =
            decode_record_size(u8_at(b, 0x44).ok_or(bad("short"))?, cluster_size)
                .filter(|s| (256..=65536).contains(s) && s.is_power_of_two());
        let serial = i64_at(b, 0x48).ok_or(bad("short"))? as u64;
        Ok(Self {
            bytes_per_sector: bps,
            cluster_size,
            total_sectors,
            total_clusters,
            mft_lcn,
            mft_mirror_lcn,
            record_size,
            index_record_size,
            serial,
        })
    }

    /// Byte offset of `$MFT` on the volume.
    #[must_use]
    pub fn mft_offset(&self) -> u64 {
        self.mft_lcn.saturating_mul(self.cluster_size)
    }

    /// Volume size in bytes as recorded in the boot sector.
    #[must_use]
    pub fn volume_bytes(&self) -> u64 {
        self.total_clusters.saturating_mul(self.cluster_size)
    }
}

/// Decodes the signed "clusters per record" byte: positive values count
/// clusters, negative values mean `2^|n|` bytes.
fn decode_record_size(raw: u8, cluster_size: u64) -> Option<u32> {
    let n = raw as i8;
    if n > 0 {
        u32::try_from(u64::from(n.unsigned_abs()).checked_mul(cluster_size)?).ok()
    } else if n < 0 && n.unsigned_abs() <= 31 {
        Some(1u32 << n.unsigned_abs())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sector(bps: u16, spc: u8, cpr: u8) -> [u8; 512] {
        let mut s = [0u8; 512];
        s[3..11].copy_from_slice(b"NTFS    ");
        s[0x0B..0x0D].copy_from_slice(&bps.to_le_bytes());
        s[0x0D] = spc;
        s[0x28..0x30].copy_from_slice(&1_000_000u64.to_le_bytes());
        s[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        s[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
        s[0x40] = cpr;
        s[0x44] = 1;
        s[0x48..0x50].copy_from_slice(&0xDEAD_BEEFu64.to_le_bytes());
        s[0x1FE] = 0x55;
        s[0x1FF] = 0xAA;
        s
    }

    #[test]
    fn decodes_common_geometries() {
        let b = BootSector::parse(&sector(512, 8, 0xF6)).unwrap();
        assert_eq!((b.cluster_size, b.record_size), (4096, 1024));
        assert_eq!(b.index_record_size, Some(4096));
        assert_eq!(b.serial, 0xDEAD_BEEF);
        assert_eq!(b.total_clusters, 125_000);

        // 512-byte clusters with 1 KiB records: positive clusters-per-record.
        let b = BootSector::parse(&sector(512, 1, 2)).unwrap();
        assert_eq!((b.cluster_size, b.record_size), (512, 1024));

        // 4Kn: 4096-byte sectors and records.
        let b = BootSector::parse(&sector(4096, 1, 0xF4)).unwrap();
        assert_eq!((b.cluster_size, b.record_size), (4096, 4096));
    }

    #[test]
    fn large_cluster_exponent_encoding() {
        // 0xF4 = 2^(256-244) = 4096 sectors of 512 bytes = 2 MiB.
        let b = BootSector::parse(&sector(512, 0xF4, 0xF6)).unwrap();
        assert_eq!(b.cluster_size, 2 * 1024 * 1024);
        // 4 MiB clusters are rejected.
        assert!(BootSector::parse(&sector(512, 0xF3, 0xF6)).is_err());
    }

    #[test]
    fn rejects_bad_fields() {
        assert!(BootSector::parse(&[0u8; 100]).is_err());
        let mut s = sector(512, 8, 0xF6);
        s[3] = b'X';
        assert!(BootSector::parse(&s).is_err());
        assert!(BootSector::parse(&sector(500, 8, 0xF6)).is_err());
        assert!(BootSector::parse(&sector(8192, 1, 0xF6)).is_err());
        assert!(BootSector::parse(&sector(512, 0, 0xF6)).is_err());
        assert!(BootSector::parse(&sector(512, 3, 0xF6)).is_err());
        assert!(BootSector::parse(&sector(512, 8, 0)).is_err());
        assert!(BootSector::parse(&sector(512, 8, 0xF7)).is_ok()); // 512-byte records
        assert!(BootSector::parse(&sector(512, 8, 0xF8)).is_err()); // 256-byte records
        assert!(BootSector::parse(&sector(512, 8, 0x80)).is_err());
        assert!(BootSector::parse(&sector(512, 8, 0xE0)).is_err()); // 2^32
        let mut s = sector(512, 8, 0xF6);
        s[0x30..0x38].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(BootSector::parse(&s).is_err());
        let mut s = sector(512, 8, 0xF6);
        s[0x1FE] = 0;
        assert!(BootSector::parse(&s).is_err());
    }
}
