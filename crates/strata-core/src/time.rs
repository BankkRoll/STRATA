use serde::{Deserialize, Serialize};

/// Windows FILETIME: 100-ns intervals since 1601-01-01 UTC.
///
/// This is the full-precision form used in scan records and detail fetches.
/// The index stores the compact [`EpochSecs`] instead.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct FileTime(pub u64);

/// FILETIME ticks per second.
const TICKS_PER_SEC: u64 = 10_000_000;

/// FILETIME of 2000-01-01T00:00:00Z, the [`EpochSecs`] epoch.
const EPOCH_2000_FILETIME: u64 = 125_911_584_000_000_000;

/// FILETIME of 1970-01-01T00:00:00Z.
const UNIX_EPOCH_FILETIME: u64 = 116_444_736_000_000_000;

/// FILETIME of 1990-01-01T00:00:00Z; earlier timestamps are suspicious.
const SUSPICIOUS_BEFORE_FILETIME: u64 = 122_756_256_000_000_000;

impl FileTime {
    /// Seconds since the Unix epoch (negative before 1970).
    #[must_use]
    pub fn to_unix_secs(self) -> i64 {
        // NOTE: u64 FILETIME / 1e7 fits comfortably in i64.
        (self.0 / TICKS_PER_SEC) as i64 - (UNIX_EPOCH_FILETIME / TICKS_PER_SEC) as i64
    }

    /// Builds a FILETIME from Unix seconds, clamping pre-1601 values to 0.
    #[must_use]
    pub fn from_unix_secs(secs: i64) -> Self {
        let base = (UNIX_EPOCH_FILETIME / TICKS_PER_SEC) as i64;
        let s = secs.saturating_add(base).max(0) as u64;
        Self(s.saturating_mul(TICKS_PER_SEC))
    }

    /// Whether this timestamp is implausible: before 1990, or more than a day
    /// after `now`. Such values are flagged rather than trusted for sorting.
    #[must_use]
    pub fn is_suspicious(self, now: FileTime) -> bool {
        const DAY: u64 = 86_400 * TICKS_PER_SEC;
        self.0 < SUSPICIOUS_BEFORE_FILETIME || self.0 > now.0.saturating_add(DAY)
    }
}

/// Compact timestamp stored in the index: whole seconds since 2000-01-01 UTC.
///
/// A `u32` covers 2000 through 2136. Earlier times clamp to 0 and later ones
/// to `u32::MAX`; full precision is fetched on demand from the record.
///
/// # Example
///
/// ```
/// use strata_core::{EpochSecs, FileTime};
/// let t = FileTime(125_911_584_000_000_000 + 10 * 10_000_000);
/// assert_eq!(EpochSecs::from_filetime(t).0, 10);
/// ```
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct EpochSecs(pub u32);

impl EpochSecs {
    /// Converts with clamping at both ends.
    #[must_use]
    pub fn from_filetime(t: FileTime) -> Self {
        let secs = t.0.saturating_sub(EPOCH_2000_FILETIME) / TICKS_PER_SEC;
        Self(u32::try_from(secs).unwrap_or(u32::MAX))
    }

    /// Converts back to a FILETIME (second precision).
    #[must_use]
    pub fn to_filetime(self) -> FileTime {
        FileTime(EPOCH_2000_FILETIME + u64::from(self.0) * TICKS_PER_SEC)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_round_trip() {
        let t = FileTime::from_unix_secs(1_700_000_000);
        assert_eq!(t.to_unix_secs(), 1_700_000_000);
        assert_eq!(FileTime(UNIX_EPOCH_FILETIME).to_unix_secs(), 0);
    }

    #[test]
    fn pre_1601_clamps() {
        assert_eq!(FileTime::from_unix_secs(i64::MIN), FileTime(0));
    }

    #[test]
    fn epoch_secs_clamps() {
        assert_eq!(EpochSecs::from_filetime(FileTime(0)).0, 0);
        assert_eq!(EpochSecs::from_filetime(FileTime(u64::MAX)).0, u32::MAX);
        let t = FileTime(EPOCH_2000_FILETIME + 42 * TICKS_PER_SEC);
        assert_eq!(EpochSecs::from_filetime(t).to_filetime(), t);
    }

    #[test]
    fn suspicious_detection() {
        let now = FileTime::from_unix_secs(1_760_000_000);
        assert!(FileTime::from_unix_secs(0).is_suspicious(now));
        assert!(FileTime::from_unix_secs(1_760_000_000 + 2 * 86_400).is_suspicious(now));
        assert!(!FileTime::from_unix_secs(1_700_000_000).is_suspicious(now));
    }
}
