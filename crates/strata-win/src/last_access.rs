//! Whether NTFS updates last-access times.
//!
//! Read from `HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\
//! NtfsDisableLastAccessUpdate`, the value `fsutil behavior query
//! disablelastaccess` reports. The UI shows a notice whenever access times
//! are not maintained, and never presents them as authoritative.

use serde::{Deserialize, Serialize};
use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;

use crate::error::Result;
use crate::registry::RegKey;

const KEY: &str = r"SYSTEM\CurrentControlSet\Control\FileSystem";
const VALUE: &str = "NtfsDisableLastAccessUpdate";

/// Last-access update policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "policy")]
pub enum LastAccessPolicy {
    /// Updates are on.
    Enabled {
        /// Windows chose this (high bit set), typically based on volume size
        /// at install time, rather than the user.
        system_managed: bool,
    },
    /// Updates are off: access times are stale.
    Disabled {
        /// Windows chose this rather than the user.
        system_managed: bool,
    },
    /// The value is absent. Windows 10 1803+ then manages it itself.
    NotSet,
    /// An unrecognized value (raw value kept).
    Unknown(u32),
}

impl LastAccessPolicy {
    /// Decodes the registry value.
    ///
    /// The high bit (`0x8000_0000`) marks "system managed". The low bits
    /// follow `fsutil`: 0 user-managed enabled, 1 user-managed disabled,
    /// 2 system-managed enabled, 3 system-managed disabled. Windows writes
    /// both `0x8000_0000|1` and `0x8000_0000|3` forms, so either system flag
    /// counts.
    ///
    /// # Example
    ///
    /// ```
    /// use strata_win::last_access::LastAccessPolicy;
    /// assert_eq!(
    ///     LastAccessPolicy::from_raw(Some(0x8000_0002)),
    ///     LastAccessPolicy::Enabled { system_managed: true }
    /// );
    /// assert_eq!(
    ///     LastAccessPolicy::from_raw(Some(1)),
    ///     LastAccessPolicy::Disabled { system_managed: false }
    /// );
    /// ```
    #[must_use]
    pub const fn from_raw(raw: Option<u32>) -> Self {
        let Some(v) = raw else {
            return Self::NotSet;
        };
        let low = v & 0x7FFF_FFFF;
        if low > 3 {
            return Self::Unknown(v);
        }
        let system_managed = v & 0x8000_0000 != 0 || low >= 2;
        if low & 1 == 1 {
            Self::Disabled { system_managed }
        } else {
            Self::Enabled { system_managed }
        }
    }

    /// Whether access times should be treated as unreliable.
    #[must_use]
    pub const fn access_times_unreliable(self) -> bool {
        !matches!(self, Self::Enabled { .. })
    }
}

/// Reads the current policy.
pub fn last_access_policy() -> Result<LastAccessPolicy> {
    let raw = match RegKey::open(HKEY_LOCAL_MACHINE, KEY)? {
        Some(k) => k.query_dword(VALUE)?,
        None => None,
    };
    Ok(LastAccessPolicy::from_raw(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_table() {
        use LastAccessPolicy::*;
        assert_eq!(LastAccessPolicy::from_raw(None), NotSet);
        assert_eq!(
            LastAccessPolicy::from_raw(Some(0)),
            Enabled {
                system_managed: false
            }
        );
        assert_eq!(
            LastAccessPolicy::from_raw(Some(1)),
            Disabled {
                system_managed: false
            }
        );
        assert_eq!(
            LastAccessPolicy::from_raw(Some(2)),
            Enabled {
                system_managed: true
            }
        );
        assert_eq!(
            LastAccessPolicy::from_raw(Some(3)),
            Disabled {
                system_managed: true
            }
        );
        assert_eq!(
            LastAccessPolicy::from_raw(Some(0x8000_0000)),
            Enabled {
                system_managed: true
            }
        );
        assert_eq!(
            LastAccessPolicy::from_raw(Some(0x8000_0001)),
            Disabled {
                system_managed: true
            }
        );
        assert_eq!(
            LastAccessPolicy::from_raw(Some(0x8000_0003)),
            Disabled {
                system_managed: true
            }
        );
        assert_eq!(LastAccessPolicy::from_raw(Some(7)), Unknown(7));
        assert!(
            Disabled {
                system_managed: true
            }
            .access_times_unreliable()
        );
        assert!(NotSet.access_times_unreliable());
        assert!(
            !Enabled {
                system_managed: false
            }
            .access_times_unreliable()
        );
    }

    #[test]
    fn reads_this_machine() {
        let p = last_access_policy().unwrap();
        assert!(!matches!(p, LastAccessPolicy::Unknown(_)), "{p:?}");
    }
}
