//! Volume discovery for the guard and Recycle Bin capability checks.
//!
//! Removable and network drives normally have no Recycle Bin, and a volume
//! can be set to "Don't move files to the Recycle Bin" (`NukeOnDelete`).
//! Either way a "recycle" would really be a permanent delete, so the plan
//! reports it before anything runs (SPEC §15.2 step 4, §21).

use std::io;

use serde::{Deserialize, Serialize};

use crate::canon::{CanonicalPath, Root};
use crate::never::{VolumeEntry, VolumeMap};
use crate::win::vol;

const BITBUCKET_VOLUME: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume";

/// Resolves every volume's GUID, NT device and mount points.
///
/// # Errors
///
/// Fails when the volume manager cannot be enumerated.
pub fn resolve_volume_map() -> io::Result<VolumeMap> {
    let volumes = vol::enumerate_volumes()?
        .into_iter()
        .filter_map(|v| {
            let guid = v
                .guid_path
                .strip_prefix(r"\\?\Volume")?
                .trim_end_matches('\\')
                .to_string();
            Some(VolumeEntry {
                guid,
                device: v
                    .device
                    .as_deref()
                    .and_then(|d| d.strip_prefix(r"\Device\"))
                    .map(str::to_string),
                mount_points: v.mount_points.into_iter().map(Into::into).collect(),
            })
        })
        .collect();
    Ok(VolumeMap { volumes })
}

/// Names of this machine, for loopback UNC detection.
#[must_use]
pub fn local_host_names() -> Vec<String> {
    vol::computer_names()
}

/// `GetDriveTypeW` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveType {
    /// Unknown.
    Unknown,
    /// The root path is invalid.
    NoRootDir,
    /// Removable media (USB sticks, SD cards).
    Removable,
    /// Fixed disk (including most external USB disks).
    Fixed,
    /// Network drive.
    Remote,
    /// Optical drive.
    CdRom,
    /// RAM disk.
    RamDisk,
}

impl DriveType {
    fn from_raw(t: u32) -> Self {
        match t {
            1 => Self::NoRootDir,
            2 => Self::Removable,
            3 => Self::Fixed,
            4 => Self::Remote,
            5 => Self::CdRom,
            6 => Self::RamDisk,
            _ => Self::Unknown,
        }
    }
}

/// Why a volume has no usable Recycle Bin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecycleUnavailable {
    /// Removable drive.
    RemovableDrive,
    /// Network drive or UNC path.
    NetworkDrive,
    /// Optical drive.
    OpticalDrive,
    /// RAM disk.
    RamDisk,
    /// The user set this drive to delete immediately (`NukeOnDelete`).
    DisabledForVolume,
    /// The Shell reports no Recycle Bin for this volume.
    NoRecycleBin,
    /// The volume could not be identified.
    UnknownVolume,
    /// The path is too long for the Shell to recycle (it would delete it
    /// permanently instead).
    PathTooLong,
}

impl RecycleUnavailable {
    /// Short reason for the UI.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::RemovableDrive => "removable drives have no Recycle Bin",
            Self::NetworkDrive => "network locations have no Recycle Bin",
            Self::OpticalDrive => "optical drives have no Recycle Bin",
            Self::RamDisk => "RAM disks have no Recycle Bin",
            Self::DisabledForVolume => {
                "this drive is set to delete files immediately instead of recycling"
            }
            Self::NoRecycleBin => "Windows reports no Recycle Bin on this drive",
            Self::UnknownVolume => "the drive could not be identified",
            Self::PathTooLong => "its path is too long for the Recycle Bin",
        }
    }
}

/// Recycle Bin state of one volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RecycleBinSupport {
    /// Items can be recycled.
    Available {
        /// Maximum size from the per-volume `MaxCapacity` setting, in bytes;
        /// `None` when Windows has not written the setting yet.
        capacity: Option<u64>,
        /// Bytes currently in this drive's Recycle Bin.
        used: u64,
    },
    /// Items would be permanently deleted.
    Unavailable {
        /// Why.
        reason: RecycleUnavailable,
    },
}

/// What the plan needs to know about the volume of an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// Mount point holding the item (`C:\`, `C:\mnt\data\`, `\\server\share\`).
    pub mount_point: String,
    /// Braced volume GUID, when local.
    pub guid: Option<String>,
    /// Drive type.
    pub drive_type: DriveType,
    /// Total size in bytes, when known.
    pub total_bytes: Option<u64>,
    /// Recycle Bin support.
    pub recycle_bin: RecycleBinSupport,
}

impl VolumeInfo {
    /// Whether an item of `size` bytes fits in this volume's Recycle Bin.
    /// `None` when unknown (no capacity setting yet); the delete sink still
    /// refuses any item the Shell would not recycle.
    #[must_use]
    pub fn fits(&self, size: u64) -> Option<bool> {
        match &self.recycle_bin {
            RecycleBinSupport::Available {
                capacity: Some(c), ..
            } => Some(size <= *c),
            RecycleBinSupport::Available { capacity: None, .. } => None,
            RecycleBinSupport::Unavailable { .. } => Some(false),
        }
    }
}

/// Inspects the volume that holds `path`.
///
/// # Errors
///
/// Fails when the mount point of `path` cannot be found.
pub fn volume_info(path: &CanonicalPath) -> io::Result<VolumeInfo> {
    let verbatim = path.to_verbatim_wide();
    let mount = vol::volume_path_of(&verbatim)?;
    let mount_str = String::from_utf16_lossy(&mount);
    let mount_display = mount_str
        .strip_prefix(r"\\?\UNC\")
        .map(|s| format!(r"\\{s}"))
        .or_else(|| mount_str.strip_prefix(r"\\?\").map(str::to_string))
        .unwrap_or_else(|| mount_str.clone());
    let mount_wide: Vec<u16> = mount_display.encode_utf16().collect();
    let guid = vol::volume_guid_of_mount(&mount_wide).ok().and_then(|g| {
        g.strip_prefix(r"\\?\Volume")
            .map(|s| s.trim_end_matches('\\').to_string())
    });
    let unc = matches!(path.root(), Root::Unc { .. }) || mount_display.starts_with(r"\\");
    let drive_type = if unc {
        DriveType::Remote
    } else {
        DriveType::from_raw(vol::drive_type(&mount_display))
    };
    let total_bytes = vol::volume_total_bytes(&mount_display).ok();
    let recycle_bin = recycle_support(drive_type, guid.as_deref(), &mount_display);
    Ok(VolumeInfo {
        mount_point: mount_display,
        guid,
        drive_type,
        total_bytes,
        recycle_bin,
    })
}

fn recycle_support(drive_type: DriveType, guid: Option<&str>, mount: &str) -> RecycleBinSupport {
    let unavailable = |reason| RecycleBinSupport::Unavailable { reason };
    match drive_type {
        DriveType::Removable => return unavailable(RecycleUnavailable::RemovableDrive),
        DriveType::Remote => return unavailable(RecycleUnavailable::NetworkDrive),
        DriveType::CdRom => return unavailable(RecycleUnavailable::OpticalDrive),
        DriveType::RamDisk => return unavailable(RecycleUnavailable::RamDisk),
        DriveType::Unknown | DriveType::NoRootDir => {
            return unavailable(RecycleUnavailable::UnknownVolume);
        }
        DriveType::Fixed => {}
    }
    let Some(guid) = guid else {
        return unavailable(RecycleUnavailable::UnknownVolume);
    };
    let key = format!(r"{BITBUCKET_VOLUME}\{guid}");
    if vol::hkcu_dword(&key, "NukeOnDelete").unwrap_or(0) != 0 {
        return unavailable(RecycleUnavailable::DisabledForVolume);
    }
    let Ok((used, _)) = vol::query_recycle_bin(mount) else {
        return unavailable(RecycleUnavailable::NoRecycleBin);
    };
    // MaxCapacity is stored in megabytes.
    let capacity = vol::hkcu_dword(&key, "MaxCapacity").map(|mb| u64::from(mb) * 1024 * 1024);
    RecycleBinSupport::Available { capacity, used }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_map_contains_the_system_drive() {
        let map = resolve_volume_map().unwrap();
        let c = map
            .volumes
            .iter()
            .find(|v| v.mount_points.iter().any(|m| m.as_os_str() == r"C:\"))
            .expect("C: volume");
        assert!(
            c.guid.starts_with('{') && c.guid.ends_with('}'),
            "{}",
            c.guid
        );
        assert!(
            c.device
                .as_deref()
                .is_some_and(|d| d.starts_with("HarddiskVolume"))
        );
    }

    #[test]
    fn system_drive_is_fixed_with_a_recycle_bin() {
        let info = volume_info(&CanonicalPath::parse(r"C:\Windows").unwrap()).unwrap();
        assert_eq!(info.drive_type, DriveType::Fixed);
        assert_eq!(info.mount_point, r"C:\");
        assert!(info.guid.is_some());
        assert!(info.total_bytes.unwrap_or(0) > 0);
        assert!(
            matches!(info.recycle_bin, RecycleBinSupport::Available { .. }),
            "{:?}",
            info.recycle_bin
        );
    }

    #[test]
    fn fit_rules() {
        let mut v = VolumeInfo {
            mount_point: r"C:\".into(),
            guid: None,
            drive_type: DriveType::Fixed,
            total_bytes: None,
            recycle_bin: RecycleBinSupport::Available {
                capacity: Some(100),
                used: 0,
            },
        };
        assert_eq!(v.fits(100), Some(true));
        assert_eq!(v.fits(101), Some(false));
        v.recycle_bin = RecycleBinSupport::Available {
            capacity: None,
            used: 0,
        };
        assert_eq!(v.fits(u64::MAX), None);
        v.recycle_bin = RecycleBinSupport::Unavailable {
            reason: RecycleUnavailable::RemovableDrive,
        };
        assert_eq!(v.fits(0), Some(false));
    }

    #[test]
    fn removable_and_network_have_no_recycle_bin() {
        assert_eq!(
            recycle_support(DriveType::Removable, Some("{x}"), r"E:\"),
            RecycleBinSupport::Unavailable {
                reason: RecycleUnavailable::RemovableDrive
            }
        );
        assert_eq!(
            recycle_support(DriveType::Remote, None, r"\\s\x\"),
            RecycleBinSupport::Unavailable {
                reason: RecycleUnavailable::NetworkDrive
            }
        );
        assert_eq!(
            recycle_support(DriveType::Fixed, None, r"C:\"),
            RecycleBinSupport::Unavailable {
                reason: RecycleUnavailable::UnknownVolume
            }
        );
    }
}
