//! Volume discovery (SPEC §5).
//!
//! Enumerates every local volume, including volumes with no drive letter
//! (folder mounts, recovery and EFI partitions) and, on request, mapped
//! network drives. Each [`VolumeInfo`] carries what the UI and the scanner
//! choice need: filesystem, sizes, cluster size, serial and label, BitLocker
//! state, Dev Drive state, drive kind, whether it holds Windows, and where it
//! is mounted inside another volume.
//!
//! Per-volume failures (a locked BitLocker volume, an empty card reader, an
//! unreadable recovery partition) are recorded on the volume instead of
//! failing the whole enumeration.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{ERROR_NO_MORE_FILES, HANDLE};
use windows::Win32::NetworkManagement::WNet::WNetGetConnectionW;
use windows::Win32::Storage::FileSystem::{
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW, GetDiskFreeSpaceW,
    GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToInt32;
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    FILE_FS_PERSISTENT_VOLUME_INFORMATION, FSCTL_QUERY_PERSISTENT_VOLUME_STATE,
    PERSISTENT_VOLUME_STATE_DEV_VOLUME, PERSISTENT_VOLUME_STATE_TRUSTED_VOLUME,
};
use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows::Win32::UI::Shell::PropertiesSystem::{
    GPS_DEFAULT, IPropertyStore, PSGetPropertyKeyFromName,
};
use windows::Win32::UI::Shell::{IShellItem2, SHCreateItemFromParsingName};
use windows::core::PWSTR;

use crate::error::{Context, Result, WinError};
use crate::path::{mount_points_for_volume, volume_guid_for_mount_point, volume_mount_root};
use crate::wide::{WideCString, from_wide_nul};

// Filesystem flag bits from `GetVolumeInformationW` (winnt.h).
const FILE_CASE_SENSITIVE_SEARCH: u32 = 0x0000_0001;
const FILE_CASE_PRESERVED_NAMES: u32 = 0x0000_0002;
const FILE_VOLUME_IS_COMPRESSED: u32 = 0x0000_8000;
const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;
const FILE_SUPPORTS_USN_JOURNAL: u32 = 0x0200_0000;

/// Facility code of BitLocker (FVE) HRESULTs, e.g. `FVE_E_LOCKED_VOLUME`.
const FACILITY_FVE: u32 = 0x31;

/// How Windows classifies the drive (`GetDriveTypeW`), plus network drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveKind {
    /// Internal disk.
    Fixed,
    /// USB stick, SD card, external drive reported as removable.
    Removable,
    /// Mapped network drive or share.
    Network,
    /// Optical drive.
    CdRom,
    /// RAM disk.
    RamDisk,
    /// `DRIVE_NO_ROOT_DIR` or `DRIVE_UNKNOWN`.
    Unknown,
}

impl DriveKind {
    fn from_drive_type(t: u32) -> Self {
        match t {
            2 => Self::Removable,
            3 => Self::Fixed,
            4 => Self::Network,
            5 => Self::CdRom,
            6 => Self::RamDisk,
            _ => Self::Unknown,
        }
    }
}

/// Filesystem family, parsed from the `GetVolumeInformationW` name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSystemKind {
    /// NTFS: MFT scanner when elevated.
    Ntfs,
    /// ReFS, including Dev Drives.
    Refs,
    /// FAT32.
    Fat32,
    /// FAT12/16.
    Fat,
    /// exFAT.
    ExFat,
    /// UDF or CDFS (optical).
    Optical,
    /// Anything else, or unknown (see `fs_name`).
    Other,
}

impl FileSystemKind {
    /// Classifies a filesystem name such as `"NTFS"`.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_uppercase().as_str() {
            "NTFS" => Self::Ntfs,
            "REFS" => Self::Refs,
            "FAT32" => Self::Fat32,
            "FAT" | "FAT12" | "FAT16" => Self::Fat,
            "EXFAT" => Self::ExFat,
            "UDF" | "CDFS" => Self::Optical,
            _ => Self::Other,
        }
    }
}

/// BitLocker state of a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BitLockerState {
    /// Not encrypted (or not BitLocker-capable).
    NotEncrypted,
    /// Encrypted (fully, partially, or with protection suspended) and
    /// readable right now.
    Unlocked,
    /// Encrypted and locked: unscannable until the user unlocks it.
    Locked,
    /// Could not be determined (e.g. no mount path, or the shell property is
    /// unavailable).
    Unknown,
}

impl BitLockerState {
    /// Maps the `System.Volume.BitLockerProtection` shell property value.
    ///
    /// The values are not formally documented. Observed meanings: 0 not
    /// encryptable/none, 1 on, 2 off, 3 encrypting, 4 decrypting, 5 protection
    /// suspended, 6 locked, 8 encrypted with a clear key ("waiting for
    /// activation", common with Device Encryption).
    #[must_use]
    pub fn from_shell_value(v: i32) -> Self {
        match v {
            0 | 2 => Self::NotEncrypted,
            1 | 3 | 4 | 5 | 8 => Self::Unlocked,
            6 => Self::Locked,
            _ => Self::Unknown,
        }
    }
}

/// Dev Drive state (Windows 11 23H2+).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevDriveState {
    /// A Dev Drive; `trusted` means antivirus runs in performance mode.
    DevDrive {
        /// Whether the volume is marked trusted.
        trusted: bool,
    },
    /// Not a Dev Drive.
    NotDevDrive,
    /// The query failed or is unsupported (older Windows, no mount path).
    Unknown,
}

/// A folder mount: this volume appears as a folder inside another volume.
///
/// The index treats the folder as a boundary: the inner volume's bytes are not
/// counted in the outer volume's totals (SPEC §5, §21).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NestedMount {
    /// The folder path, e.g. `D:\mnt\data\`.
    pub folder: PathBuf,
    /// GUID path of the volume that contains the folder, when resolvable.
    pub parent_volume: Option<String>,
}

/// Everything Strata knows about one volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// `\\?\Volume{...}\`; `None` for network drives.
    pub guid_path: Option<String>,
    /// NT device, e.g. `\Device\HarddiskVolume3`, when resolvable.
    pub device_path: Option<String>,
    /// All mount paths (drive letters and folder mounts), possibly empty.
    pub mount_paths: Vec<PathBuf>,
    /// The drive letter, if it has one.
    pub drive_letter: Option<char>,
    /// Folder mounts of this volume inside other volumes.
    pub nested_in: Vec<NestedMount>,
    /// Drive kind.
    pub kind: DriveKind,
    /// UNC target of a mapped network drive.
    pub remote_path: Option<String>,
    /// Raw filesystem name (`NTFS`, `ReFS`, `FAT32`, ...).
    pub fs_name: Option<String>,
    /// Parsed filesystem family.
    pub filesystem: FileSystemKind,
    /// Volume label (may be empty).
    pub label: Option<String>,
    /// Volume serial number.
    pub serial: Option<u32>,
    /// Raw `FILE_*` filesystem flags.
    pub fs_flags: u32,
    /// Maximum component length (255 on NTFS).
    pub max_component_len: Option<u32>,
    /// `FILE_SUPPORTS_USN_JOURNAL`.
    pub supports_usn_journal: bool,
    /// `FILE_CASE_SENSITIVE_SEARCH`.
    pub case_sensitive_search: bool,
    /// `FILE_CASE_PRESERVED_NAMES`.
    pub case_preserved_names: bool,
    /// `FILE_READ_ONLY_VOLUME`.
    pub read_only: bool,
    /// `FILE_VOLUME_IS_COMPRESSED`.
    pub compressed: bool,
    /// Bytes per cluster.
    pub cluster_size: Option<u32>,
    /// Bytes per sector.
    pub sector_size: Option<u32>,
    /// Total size in bytes.
    pub total_bytes: Option<u64>,
    /// Free bytes on the volume.
    pub free_bytes: Option<u64>,
    /// Free bytes available to this user (quotas).
    pub available_bytes: Option<u64>,
    /// Whether the Windows directory lives on this volume.
    pub is_system: bool,
    /// BitLocker state.
    pub bitlocker: BitLockerState,
    /// Dev Drive state.
    pub dev_drive: DevDriveState,
    /// Whether filesystem information was readable (false for locked, empty
    /// removable, or access-denied volumes).
    pub ready: bool,
    /// Why filesystem information could not be read, if it could not.
    pub error: Option<WinError>,
}

impl VolumeInfo {
    /// Used bytes (`total - free`), when both are known.
    #[must_use]
    pub fn used_bytes(&self) -> Option<u64> {
        Some(self.total_bytes?.saturating_sub(self.free_bytes?))
    }

    /// Path to use for root-relative APIs: the drive letter root, else the
    /// first mount path, else the GUID path.
    #[must_use]
    pub fn root_path(&self) -> Option<PathBuf> {
        self.mount_paths
            .iter()
            .find(|p| p.as_os_str().len() == 3)
            .or_else(|| self.mount_paths.first())
            .cloned()
            .or_else(|| self.guid_path.as_ref().map(PathBuf::from))
    }

    /// Stable identity for diffing snapshots (GUID path, or the network root).
    #[must_use]
    pub fn id(&self) -> String {
        self.guid_path.clone().unwrap_or_else(|| {
            self.mount_paths
                .first()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
    }
}

/// Which scanner handles a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScannerKind {
    /// Raw MFT read through the elevated helper.
    Mft,
    /// The unelevated directory walker.
    Walker,
    /// Not scannable right now (locked or not ready).
    None,
}

/// Picks the scanner per SPEC §5: NTFS on a local, readable volume with an
/// elevated helper → MFT; locked or unready → none; everything else
/// (ReFS, Dev Drive, FAT, exFAT, network, unelevated NTFS) → walker.
///
/// # Example
///
/// ```
/// use strata_win::volume::{scanner_choice, ScannerKind};
/// # fn demo(v: &strata_win::volume::VolumeInfo) {
/// let kind = scanner_choice(v, strata_win::process::is_elevated().unwrap_or(false));
/// assert_ne!(kind, ScannerKind::Mft, "never without elevation");
/// # }
/// ```
#[must_use]
pub fn scanner_choice(volume: &VolumeInfo, elevated: bool) -> ScannerKind {
    if !volume.ready || volume.bitlocker == BitLockerState::Locked {
        return ScannerKind::None;
    }
    let local = matches!(volume.kind, DriveKind::Fixed | DriveKind::Removable);
    if elevated && local && volume.filesystem == FileSystemKind::Ntfs {
        ScannerKind::Mft
    } else {
        ScannerKind::Walker
    }
}

/// Options for [`discover_volumes`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiscoveryOptions {
    /// Include mapped network drives. Off by default: querying a dead share
    /// can block for the SMB timeout (SPEC §5: network is opt-in).
    pub include_network: bool,
    /// Query the BitLocker shell property (initializes COM on the calling
    /// thread). Off means BitLocker is only inferred from query failures.
    pub query_bitlocker: bool,
}

impl DiscoveryOptions {
    /// Local volumes with BitLocker detection; network drives excluded.
    #[must_use]
    pub const fn local() -> Self {
        Self {
            include_network: false,
            query_bitlocker: true,
        }
    }
}

struct FindVolume(HANDLE);

impl Drop for FindVolume {
    fn drop(&mut self) {
        // SAFETY: the search handle came from FindFirstVolumeW; closed once.
        unsafe {
            let _ = FindVolumeClose(self.0);
        }
    }
}

/// GUID paths (`\\?\Volume{...}\`) of every volume known to the mount
/// manager, mounted or not.
pub fn volume_guid_paths() -> Result<Vec<String>> {
    let mut buf = [0u16; 64];
    // SAFETY: `buf` is writable for its length (GUID paths are 49 chars).
    let h = unsafe { FindFirstVolumeW(&mut buf) }.ctx("FindFirstVolumeW")?;
    let guard = FindVolume(h);
    let mut out = vec![from_wide_nul(&buf).to_string_lossy().into_owned()];
    loop {
        buf.fill(0);
        // SAFETY: `guard.0` is a live search handle; `buf` is writable.
        match unsafe { FindNextVolumeW(guard.0, &mut buf) } {
            Ok(()) => out.push(from_wide_nul(&buf).to_string_lossy().into_owned()),
            Err(e) if e.code() == ERROR_NO_MORE_FILES.to_hresult() => return Ok(out),
            Err(e) => return Err(WinError::new("FindNextVolumeW", &e)),
        }
    }
}

/// GUID path of the volume holding the Windows directory.
pub fn system_volume_guid() -> Result<String> {
    let mut buf = [0u16; 260];
    // SAFETY: `buf` is writable for its length.
    let n = unsafe { GetWindowsDirectoryW(Some(&mut buf)) } as usize;
    if n == 0 || n >= buf.len() {
        return Err(WinError::last("GetWindowsDirectoryW"));
    }
    let windir = PathBuf::from(from_wide_nul(&buf));
    volume_guid_for_mount_point(&volume_mount_root(&windir)?)
}

/// Enumerates volumes.
///
/// Never fails because of one bad volume; only a failure to enumerate at all
/// is an error.
///
/// # Example
///
/// ```no_run
/// use strata_win::volume::{discover_volumes, DiscoveryOptions};
/// for v in discover_volumes(DiscoveryOptions::local())? {
///     println!("{:?} {:?} {:?}", v.mount_paths, v.fs_name, v.total_bytes);
/// }
/// # Ok::<(), strata_win::WinError>(())
/// ```
pub fn discover_volumes(opts: DiscoveryOptions) -> Result<Vec<VolumeInfo>> {
    let system = system_volume_guid().ok();
    let mut out: Vec<VolumeInfo> = volume_guid_paths()?
        .into_iter()
        .map(|g| local_volume_info(&g, system.as_deref(), opts.query_bitlocker))
        .collect();
    if opts.include_network {
        out.extend(network_drives());
    }
    Ok(out)
}

/// Builds the [`VolumeInfo`] of one local volume. Failures are recorded on
/// the result.
#[must_use]
pub fn local_volume_info(
    guid_path: &str,
    system_guid: Option<&str>,
    bitlocker: bool,
) -> VolumeInfo {
    let mount_paths = mount_points_for_volume(guid_path).unwrap_or_default();
    let drive_letter = mount_paths.iter().find_map(|p| drive_letter_of(p));
    let device_path = guid_path
        .strip_prefix(r"\\?\")
        .map(|s| s.trim_end_matches('\\'))
        .and_then(|inner| crate::path::query_dos_device(inner).ok());
    let nested_in = mount_paths
        .iter()
        .filter(|p| drive_letter_of(p).is_none())
        .map(|folder| NestedMount {
            folder: folder.clone(),
            parent_volume: parent_volume_of(folder),
        })
        .collect();
    let root = WideCString::new(guid_path);
    // SAFETY: `root` is a NUL-terminated root path.
    let kind = DriveKind::from_drive_type(unsafe { GetDriveTypeW(root.as_pcwstr()) });

    let mut v = VolumeInfo {
        guid_path: Some(guid_path.to_owned()),
        device_path,
        mount_paths,
        drive_letter,
        nested_in,
        kind,
        remote_path: None,
        fs_name: None,
        filesystem: FileSystemKind::Other,
        label: None,
        serial: None,
        fs_flags: 0,
        max_component_len: None,
        supports_usn_journal: false,
        case_sensitive_search: false,
        case_preserved_names: false,
        read_only: false,
        compressed: false,
        cluster_size: None,
        sector_size: None,
        total_bytes: None,
        free_bytes: None,
        available_bytes: None,
        is_system: system_guid.is_some_and(|s| s.eq_ignore_ascii_case(guid_path)),
        bitlocker: BitLockerState::Unknown,
        dev_drive: DevDriveState::Unknown,
        ready: false,
        error: None,
    };
    fill_fs_info(&mut v, guid_path);
    if bitlocker
        && v.bitlocker == BitLockerState::Unknown
        && let Some(root) = v.mount_paths.first()
    {
        v.bitlocker = shell_bitlocker_state(root).unwrap_or(BitLockerState::Unknown);
    }
    if v.ready {
        v.dev_drive = dev_drive_state(guid_path, v.filesystem);
    }
    v
}

fn drive_letter_of(p: &Path) -> Option<char> {
    let s = p.to_str()?;
    let b = s.as_bytes();
    (b.len() == 3 && b[1] == b':' && b[2] == b'\\' && b[0].is_ascii_alphabetic())
        .then(|| char::from(b[0].to_ascii_uppercase()))
}

fn parent_volume_of(folder: &Path) -> Option<String> {
    let trimmed = PathBuf::from(folder.to_string_lossy().trim_end_matches('\\'));
    let parent = trimmed.parent()?;
    let root = volume_mount_root(parent).ok()?;
    volume_guid_for_mount_point(&root).ok()
}

fn fill_fs_info(v: &mut VolumeInfo, root_path: &str) {
    let root = WideCString::new(root_path);
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    let (mut serial, mut max_len, mut flags) = (0u32, 0u32, 0u32);
    // SAFETY: all buffers and out-pointers are valid for the call.
    let r = unsafe {
        GetVolumeInformationW(
            root.as_pcwstr(),
            Some(&mut label),
            Some(&mut serial),
            Some(&mut max_len),
            Some(&mut flags),
            Some(&mut fs),
        )
    };
    if let Err(e) = r {
        let err = WinError::new("GetVolumeInformationW", &e);
        if (err.hresult >> 16) & 0x7FF == FACILITY_FVE {
            v.bitlocker = BitLockerState::Locked;
        }
        v.error = Some(err);
        return;
    }
    let fs_name = from_wide_nul(&fs).to_string_lossy().into_owned();
    v.filesystem = FileSystemKind::from_name(&fs_name);
    v.fs_name = Some(fs_name);
    v.label = Some(from_wide_nul(&label).to_string_lossy().into_owned());
    v.serial = Some(serial);
    v.max_component_len = Some(max_len);
    v.fs_flags = flags;
    v.supports_usn_journal = flags & FILE_SUPPORTS_USN_JOURNAL != 0;
    v.case_sensitive_search = flags & FILE_CASE_SENSITIVE_SEARCH != 0;
    v.case_preserved_names = flags & FILE_CASE_PRESERVED_NAMES != 0;
    v.read_only = flags & FILE_READ_ONLY_VOLUME != 0;
    v.compressed = flags & FILE_VOLUME_IS_COMPRESSED != 0;
    v.ready = true;

    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: out-pointers are valid u64s.
    if unsafe {
        GetDiskFreeSpaceExW(
            root.as_pcwstr(),
            Some(&mut avail),
            Some(&mut total),
            Some(&mut free),
        )
    }
    .is_ok()
    {
        v.available_bytes = Some(avail);
        v.total_bytes = Some(total);
        v.free_bytes = Some(free);
    }
    let (mut spc, mut bps, mut fc, mut tc) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: out-pointers are valid u32s.
    if unsafe {
        GetDiskFreeSpaceW(
            root.as_pcwstr(),
            Some(&mut spc),
            Some(&mut bps),
            Some(&mut fc),
            Some(&mut tc),
        )
    }
    .is_ok()
    {
        v.cluster_size = spc.checked_mul(bps);
        v.sector_size = Some(bps);
    }
}

/// Reads `System.Volume.BitLockerProtection` through the shell. Works
/// unelevated, unlike the BitLocker WMI provider.
pub fn shell_bitlocker_state(root: &Path) -> Result<BitLockerState> {
    let _com = crate::com::ComApartment::sta()?;
    let mut key = Default::default();
    let name = WideCString::new("System.Volume.BitLockerProtection");
    // SAFETY: `name` is NUL-terminated; `key` is a valid out-pointer.
    unsafe { PSGetPropertyKeyFromName(name.as_pcwstr(), &mut key) }
        .ctx("PSGetPropertyKeyFromName")?;
    let path = WideCString::new(root);
    // SAFETY: COM is initialized on this thread; `path` is NUL-terminated.
    let item: IShellItem2 = unsafe { SHCreateItemFromParsingName(path.as_pcwstr(), None) }
        .ctx("SHCreateItemFromParsingName")?;
    // SAFETY: `item` is a live interface.
    let store: IPropertyStore =
        unsafe { item.GetPropertyStore(GPS_DEFAULT) }.ctx("IShellItem2::GetPropertyStore")?;
    // SAFETY: `store` is live and `key` a valid PROPERTYKEY.
    let value = unsafe { store.GetValue(&key) }.ctx("IPropertyStore::GetValue")?;
    // NOTE: volumes without any BitLocker protection report VT_EMPTY (seen
    // on Windows 11 Home); PropVariantToInt32 maps that to 0, the same value
    // Explorer and `Shell.Application` report for them.
    // SAFETY: `value` is a valid PROPVARIANT owned for the call.
    let value = unsafe { PropVariantToInt32(&value) }.ctx("PropVariantToInt32")?;
    Ok(BitLockerState::from_shell_value(value))
}

/// Queries `FSCTL_QUERY_PERSISTENT_VOLUME_STATE` for the Dev Drive bit.
///
/// Dev Drives are always ReFS, so other filesystems return
/// [`DevDriveState::NotDevDrive`] without a query. The control code is
/// accepted on a root directory handle opened with `FILE_READ_ATTRIBUTES`,
/// so it works unelevated.
// COMPAT: Windows 10 and Windows 11 before 22621 do not know the Dev Drive
// flags; the call fails or returns no flag, which maps to Unknown/NotDevDrive.
#[must_use]
pub fn dev_drive_state(root_path: &str, filesystem: FileSystemKind) -> DevDriveState {
    if filesystem != FileSystemKind::Refs {
        return DevDriveState::NotDevDrive;
    }
    let Ok(h) = crate::path::open_for_attributes(Path::new(root_path), false) else {
        return DevDriveState::Unknown;
    };
    // NOTE: the filesystem rejects narrower masks with ERROR_INVALID_PARAMETER
    // on query (observed on NTFS, Windows 11 26200); an all-ones mask asks
    // for every flag.
    let input = FILE_FS_PERSISTENT_VOLUME_INFORMATION {
        Version: 1,
        FlagMask: u32::MAX,
        ..Default::default()
    };
    let mut output = FILE_FS_PERSISTENT_VOLUME_INFORMATION::default();
    let size = std::mem::size_of::<FILE_FS_PERSISTENT_VOLUME_INFORMATION>() as u32;
    let mut returned = 0u32;
    // SAFETY: input and output point to correctly sized structs that live for
    // the duration of this synchronous call.
    let r = unsafe {
        DeviceIoControl(
            h.raw(),
            FSCTL_QUERY_PERSISTENT_VOLUME_STATE,
            Some((&raw const input).cast()),
            size,
            Some((&raw mut output).cast()),
            size,
            Some(&mut returned),
            None,
        )
    };
    if r.is_err() || returned < size {
        return DevDriveState::Unknown;
    }
    if output.VolumeFlags & PERSISTENT_VOLUME_STATE_DEV_VOLUME != 0 {
        DevDriveState::DevDrive {
            trusted: output.VolumeFlags & PERSISTENT_VOLUME_STATE_TRUSTED_VOLUME != 0,
        }
    } else {
        DevDriveState::NotDevDrive
    }
}

/// Mapped network drives (`DRIVE_REMOTE` letters), with their UNC targets.
///
/// Filesystem details are queried too, which can block for the SMB timeout
/// if the server is unreachable.
// NOTE: an elevated process sees a different set of mapped drives than the
// unelevated user session (drive mappings are per logon session) unless
// EnableLinkedConnections is set, so the UI process should do this, not the
// helper.
#[must_use]
pub fn network_drives() -> Vec<VolumeInfo> {
    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = char::from(b'A' + i);
        let root = format!("{letter}:\\");
        let wroot = WideCString::new(&root);
        // SAFETY: `wroot` is NUL-terminated.
        if DriveKind::from_drive_type(unsafe { GetDriveTypeW(wroot.as_pcwstr()) })
            != DriveKind::Network
        {
            continue;
        }
        let mut v = VolumeInfo {
            guid_path: None,
            device_path: None,
            mount_paths: vec![PathBuf::from(&root)],
            drive_letter: Some(letter),
            nested_in: Vec::new(),
            kind: DriveKind::Network,
            remote_path: wnet_connection(letter),
            fs_name: None,
            filesystem: FileSystemKind::Other,
            label: None,
            serial: None,
            fs_flags: 0,
            max_component_len: None,
            supports_usn_journal: false,
            case_sensitive_search: false,
            case_preserved_names: false,
            read_only: false,
            compressed: false,
            cluster_size: None,
            sector_size: None,
            total_bytes: None,
            free_bytes: None,
            available_bytes: None,
            is_system: false,
            bitlocker: BitLockerState::NotEncrypted,
            dev_drive: DevDriveState::NotDevDrive,
            ready: false,
            error: None,
        };
        fill_fs_info(&mut v, &root);
        out.push(v);
    }
    out
}

/// UNC target of a mapped drive letter (`WNetGetConnectionW`).
#[must_use]
pub fn wnet_connection(letter: char) -> Option<String> {
    let local = WideCString::new(format!("{letter}:"));
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` units; `local` is NUL-terminated.
    let status =
        unsafe { WNetGetConnectionW(local.as_pcwstr(), Some(PWSTR(buf.as_mut_ptr())), &mut len) };
    (status.0 == 0).then(|| from_wide_nul(&buf).to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> VolumeInfo {
        VolumeInfo {
            guid_path: Some(r"\\?\Volume{x}\".into()),
            device_path: None,
            mount_paths: vec![r"C:\".into()],
            drive_letter: Some('C'),
            nested_in: vec![],
            kind: DriveKind::Fixed,
            remote_path: None,
            fs_name: Some("NTFS".into()),
            filesystem: FileSystemKind::Ntfs,
            label: None,
            serial: None,
            fs_flags: 0,
            max_component_len: None,
            supports_usn_journal: true,
            case_sensitive_search: true,
            case_preserved_names: true,
            read_only: false,
            compressed: false,
            cluster_size: Some(4096),
            sector_size: Some(512),
            total_bytes: Some(100),
            free_bytes: Some(40),
            available_bytes: Some(40),
            is_system: true,
            bitlocker: BitLockerState::NotEncrypted,
            dev_drive: DevDriveState::NotDevDrive,
            ready: true,
            error: None,
        }
    }

    #[test]
    fn scanner_choice_rules() {
        let mut v = sample();
        assert_eq!(scanner_choice(&v, true), ScannerKind::Mft);
        assert_eq!(scanner_choice(&v, false), ScannerKind::Walker);
        v.filesystem = FileSystemKind::Refs;
        assert_eq!(scanner_choice(&v, true), ScannerKind::Walker);
        v.filesystem = FileSystemKind::Ntfs;
        v.kind = DriveKind::Network;
        assert_eq!(scanner_choice(&v, true), ScannerKind::Walker);
        v.kind = DriveKind::Removable;
        assert_eq!(scanner_choice(&v, true), ScannerKind::Mft);
        v.bitlocker = BitLockerState::Locked;
        assert_eq!(scanner_choice(&v, true), ScannerKind::None);
        v.bitlocker = BitLockerState::Unlocked;
        v.ready = false;
        assert_eq!(scanner_choice(&v, true), ScannerKind::None);
        assert_eq!(sample().used_bytes(), Some(60));
    }

    #[test]
    fn classification_tables() {
        assert_eq!(FileSystemKind::from_name("NTFS"), FileSystemKind::Ntfs);
        assert_eq!(FileSystemKind::from_name("ReFS"), FileSystemKind::Refs);
        assert_eq!(FileSystemKind::from_name("exFAT"), FileSystemKind::ExFat);
        assert_eq!(FileSystemKind::from_name("FAT32"), FileSystemKind::Fat32);
        assert_eq!(FileSystemKind::from_name("9P"), FileSystemKind::Other);
        assert_eq!(BitLockerState::from_shell_value(6), BitLockerState::Locked);
        assert_eq!(
            BitLockerState::from_shell_value(1),
            BitLockerState::Unlocked
        );
        assert_eq!(
            BitLockerState::from_shell_value(2),
            BitLockerState::NotEncrypted
        );
        assert_eq!(
            BitLockerState::from_shell_value(42),
            BitLockerState::Unknown
        );
        assert_eq!(drive_letter_of(Path::new(r"d:\")), Some('D'));
        assert_eq!(drive_letter_of(Path::new(r"D:\mnt\")), None);
    }

    #[test]
    fn discovers_the_system_volume() {
        let vols = discover_volumes(DiscoveryOptions::local()).unwrap();
        let sys: Vec<_> = vols.iter().filter(|v| v.is_system).collect();
        assert_eq!(sys.len(), 1, "{vols:#?}");
        let sys = sys[0];
        assert!(sys.ready);
        assert!(sys.drive_letter.is_some());
        assert_eq!(sys.kind, DriveKind::Fixed);
        assert!(sys.total_bytes.unwrap() > 0);
        assert!(sys.cluster_size.unwrap().is_power_of_two());
        assert!(sys.device_path.as_deref().unwrap().starts_with(r"\Device\"));
        assert_ne!(sys.bitlocker, BitLockerState::Locked);
        let json = serde_json::to_string(sys).unwrap();
        let back: VolumeInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, sys);
        if std::env::var_os("STRATA_PRINT_VOLUMES").is_some() {
            println!("{}", serde_json::to_string_pretty(&vols).unwrap());
        }
    }

    #[test]
    fn network_enumeration_does_not_panic() {
        for v in network_drives() {
            assert_eq!(v.kind, DriveKind::Network);
            assert!(v.guid_path.is_none());
        }
    }
}
