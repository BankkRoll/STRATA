//! The volume registry and the `VolumeInfo` wire type.
//!
//! [`Registry`] holds every volume `strata_win::volume::discover_volumes`
//! found, updated by the hot-plug watcher, plus each volume's scan status.
//! Removed volumes that have an index stay listed (`present: false`, state
//! `stale`); others disappear. [`VolumeDto`] is the UI's `VolumeInfo`
//! (`ui/src/lib/volumes.ts`).

use std::collections::BTreeMap;

use serde::Serialize;
use strata_win::volume::{BitLockerState, DevDriveState, DriveKind, FileSystemKind, VolumeInfo};

use crate::model::{ScannerUsed, VolumeData};
use crate::scan::ScanProgress;

/// Scan state of one volume.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanStatus {
    /// A scan is running.
    pub running: bool,
    /// Its progress.
    pub progress: Option<ScanProgress>,
    /// Last scan failure, shown once.
    pub error: Option<String>,
    /// Why live updates stopped (journal lost or turned off), until the
    /// next scan.
    pub notice: Option<String>,
    /// Changes still being replayed while catching up with the journal.
    pub catching_up: Option<usize>,
}

/// One known volume.
#[derive(Debug, Clone)]
pub struct VolumeEntry {
    /// Platform facts.
    pub info: VolumeInfo,
    /// Currently mounted.
    pub present: bool,
    /// Scan state.
    pub status: ScanStatus,
}

/// Every known volume by id (GUID path).
#[derive(Debug, Default)]
pub struct Registry {
    /// Volumes by id.
    pub volumes: BTreeMap<String, VolumeEntry>,
}

impl Registry {
    /// Replaces the volume list with a fresh enumeration. Volumes that
    /// vanished stay (absent) when `keep` says they have an index.
    pub fn replace_all(&mut self, list: Vec<VolumeInfo>, keep: impl Fn(&str) -> bool) {
        let mut next = BTreeMap::new();
        for info in list {
            let id = info.id();
            let status = self
                .volumes
                .remove(&id)
                .map(|e| e.status)
                .unwrap_or_default();
            next.insert(
                id,
                VolumeEntry {
                    info,
                    present: true,
                    status,
                },
            );
        }
        for (id, mut e) in std::mem::take(&mut self.volumes) {
            if keep(&id) {
                e.present = false;
                next.insert(id, e);
            }
        }
        self.volumes = next;
    }

    /// Updates or inserts one volume.
    pub fn upsert(&mut self, info: VolumeInfo) {
        let id = info.id();
        let status = self
            .volumes
            .remove(&id)
            .map(|e| e.status)
            .unwrap_or_default();
        self.volumes.insert(
            id,
            VolumeEntry {
                info,
                present: true,
                status,
            },
        );
    }

    /// Marks a volume removed (kept when it has an index).
    pub fn remove(&mut self, id: &str, has_index: bool) {
        if has_index {
            if let Some(e) = self.volumes.get_mut(id) {
                e.present = false;
            }
        } else {
            self.volumes.remove(id);
        }
    }
}

/// `VolumeInfo.scan` on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanDto {
    /// `never` | `scanning` | `live` | `stale` | `partial`.
    pub state: &'static str,
    /// Progress while scanning.
    pub progress: Option<ScanProgress>,
    /// Unix ms of the last completed scan.
    pub last_scan_ms: Option<i64>,
    /// `mft` | `walker`.
    pub scanner: Option<ScannerUsed>,
    /// Root wire id once an index (or preview) exists.
    pub root_id: Option<u32>,
    /// Unix ms of the last change applied after the scan (live updates or
    /// cleanup), or `None`. Views re-request their layout when it moves.
    pub changed_ms: Option<i64>,
    /// Last failure message.
    pub error: Option<String>,
    /// Why live updates stopped, until the next scan.
    pub notice: Option<String>,
    /// Changes left to replay while catching up with the journal.
    pub catching_up: Option<usize>,
}

/// `VolumeInfo` on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeDto {
    /// GUID path (or network root).
    pub id: String,
    /// Mount points (`C:\`, folder mounts).
    pub mount_points: Vec<String>,
    /// Label.
    pub label: String,
    /// Filesystem display name.
    pub filesystem: &'static str,
    /// Dev Drive.
    pub dev_drive: bool,
    /// Drive kind.
    pub kind: &'static str,
    /// Hosts Windows.
    pub is_system: bool,
    /// Capacity.
    pub total_bytes: u64,
    /// Free bytes.
    pub free_bytes: u64,
    /// Cluster size.
    pub cluster_size: u32,
    /// Serial as `XXXX-XXXX`.
    pub serial: String,
    /// `none` | `unlocked` | `locked`.
    pub bitlocker: &'static str,
    /// Mounted now.
    pub present: bool,
    /// Scan state.
    pub scan: ScanDto,
    /// Allocated bytes per category id once scanned.
    pub category_bytes: Option<BTreeMap<String, u64>>,
}

/// Builds the wire form of one volume.
#[must_use]
pub fn to_dto(e: &VolumeEntry, data: Option<&VolumeData>, live: bool) -> VolumeDto {
    let i = &e.info;
    let state = if e.status.running || data.is_some_and(|d| d.preview) {
        "scanning"
    } else {
        match data {
            None => "never",
            Some(d) if d.partial => "partial",
            Some(_) if live => "live",
            // A complete index nothing keeps current is a point-in-time
            // snapshot.
            Some(_) => "stale",
        }
    };
    let category_bytes = data
        .filter(|d| !d.preview)
        .map(|d| d.category_bytes.clone());
    VolumeDto {
        id: i.id(),
        mount_points: i
            .mount_paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        label: i.label.clone().unwrap_or_default(),
        filesystem: if i.kind == DriveKind::Network {
            "network"
        } else {
            match i.filesystem {
                FileSystemKind::Ntfs => "NTFS",
                FileSystemKind::Refs => "ReFS",
                FileSystemKind::Fat32 => "FAT32",
                FileSystemKind::ExFat => "exFAT",
                FileSystemKind::Fat => "FAT",
                FileSystemKind::Optical | FileSystemKind::Other => "other",
            }
        },
        dev_drive: matches!(i.dev_drive, DevDriveState::DevDrive { .. }),
        kind: match i.kind {
            DriveKind::Fixed => "fixed",
            DriveKind::Removable => "removable",
            DriveKind::Network => "network",
            DriveKind::CdRom => "cdrom",
            DriveKind::RamDisk => "ramdisk",
            DriveKind::Unknown => "unknown",
        },
        is_system: i.is_system,
        total_bytes: i.total_bytes.unwrap_or(0),
        free_bytes: i.free_bytes.unwrap_or(0),
        cluster_size: i.cluster_size.unwrap_or(0),
        serial: i
            .serial
            .map(|s| format!("{:04X}-{:04X}", s >> 16, s & 0xFFFF))
            .unwrap_or_default(),
        bitlocker: match i.bitlocker {
            BitLockerState::Locked => "locked",
            BitLockerState::Unlocked => "unlocked",
            BitLockerState::NotEncrypted | BitLockerState::Unknown => "none",
        },
        present: e.present,
        scan: ScanDto {
            state,
            progress: e.status.progress.filter(|_| e.status.running),
            last_scan_ms: data.filter(|d| !d.preview).map(|d| d.scanned_at_ms),
            scanner: data.map(|d| d.scanner),
            root_id: data.map(VolumeData::root_wire),
            changed_ms: data.map(|d| d.changed_ms).filter(|&ms| ms > 0),
            error: e.status.error.clone(),
            notice: e.status.notice.clone(),
            catching_up: e.status.catching_up,
        },
        category_bytes,
    }
}
