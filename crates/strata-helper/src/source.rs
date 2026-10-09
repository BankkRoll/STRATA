//! Where MFT bytes come from: raw volumes, or (debug builds only) an image
//! file.
//!
//! SECURITY: volume ids arrive from the client. They are validated against
//! a strict grammar ([`parse_volume_id`]) and turned into a device path the
//! helper builds itself, so a client can never make the elevated helper open
//! an arbitrary file or device.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use strata_ntfs::{IoMode, NtfsVolume, RawVolume, ReadAt};
use strata_win::volume::VolumeInfo;

use crate::error::HelperError;
use crate::privs::{PrivilegeScope, VOLUME_PRIVILEGES};

/// The volume id that names the `--image` file (debug builds only).
pub const IMAGE_VOLUME: &str = "strata-image";

/// A validated volume, as the helper will open it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VolumeTarget {
    /// A device path the helper built: `\\?\Volume{GUID}` or `\\.\X:`.
    Device(String),
    /// The development image file.
    Image,
}

/// Validates a client-supplied volume id.
///
/// Accepted forms: `\\?\Volume{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}\` (the
/// trailing backslash is optional), `X:` and `X:\`, and [`IMAGE_VOLUME`].
///
/// # Errors
///
/// [`strata_ipc::protocol::ErrorCode::BadRequest`] for anything else.
///
/// # Example
///
/// ```
/// use strata_helper::source::{parse_volume_id, VolumeTarget};
/// assert_eq!(parse_volume_id(r"c:\").unwrap(), VolumeTarget::Device(r"\\.\C:".into()));
/// assert!(parse_volume_id(r"C:\Windows").is_err());
/// ```
pub fn parse_volume_id(id: &str) -> Result<VolumeTarget, HelperError> {
    if id == IMAGE_VOLUME {
        return Ok(VolumeTarget::Image);
    }
    let bad = || HelperError::bad_request("volume must be a volume GUID path or a drive letter");
    let bytes = id.as_bytes();
    if (bytes.len() == 2 || (bytes.len() == 3 && bytes[2] == b'\\'))
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
    {
        return Ok(VolumeTarget::Device(format!(
            r"\\.\{}:",
            char::from(bytes[0].to_ascii_uppercase())
        )));
    }
    let rest = id.strip_prefix(r"\\?\Volume{").ok_or_else(bad)?;
    let rest = rest.strip_suffix('\\').unwrap_or(rest);
    let guid = rest.strip_suffix('}').ok_or_else(bad)?;
    if !is_guid(guid) {
        return Err(bad());
    }
    Ok(VolumeTarget::Device(format!(
        r"\\?\Volume{{{}}}",
        guid.to_ascii_lowercase()
    )))
}

fn is_guid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A boxed positioned reader, so raw volumes and image files share one
/// `NtfsVolume` type.
pub struct DynReader(Box<dyn ReadAt + Send + Sync>);

impl std::fmt::Debug for DynReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DynReader")
    }
}

impl ReadAt for DynReader {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.0.read_at(offset, buf)
    }

    fn alignment(&self) -> usize {
        self.0.alignment()
    }

    fn queued(&self) -> Option<&strata_ntfs::QueuedReader> {
        self.0.queued()
    }
}

/// An opened NTFS volume.
pub type MftVolume = NtfsVolume<DynReader>;

/// Opens volumes for requests and caches them for record re-reads.
#[derive(Debug, Default)]
pub struct Volumes {
    image: Option<PathBuf>,
    cache: Mutex<HashMap<VolumeTarget, Arc<MftVolume>>>,
}

impl Volumes {
    /// Raw volumes only (release behavior).
    #[must_use]
    pub fn raw() -> Self {
        Self::default()
    }

    /// Raw volumes plus `image` under the id [`IMAGE_VOLUME`]. Debug builds
    /// only: an elevated helper must never read client-chosen files.
    #[cfg(debug_assertions)]
    #[must_use]
    pub fn with_image(image: PathBuf) -> Self {
        Self {
            image: Some(image),
            cache: Mutex::default(),
        }
    }

    /// Whether an image source is configured.
    #[must_use]
    pub fn has_image(&self) -> bool {
        self.image.is_some()
    }

    /// Validates `volume` and checks the target is available.
    ///
    /// # Errors
    ///
    /// Malformed id, or the image id without an image configured.
    pub fn target(&self, volume: &str) -> Result<VolumeTarget, HelperError> {
        let t = parse_volume_id(volume)?;
        if t == VolumeTarget::Image && self.image.is_none() {
            return Err(HelperError::new(
                strata_ipc::protocol::ErrorCode::UnknownVolume,
                "no image source is configured",
            ));
        }
        Ok(t)
    }

    /// Opens `volume` from scratch (a scan must see the current MFT layout)
    /// and caches it for [`Volumes::cached`].
    ///
    /// # Errors
    ///
    /// Invalid id, open failure (access denied when unelevated), or not NTFS.
    pub fn open_fresh(&self, volume: &str) -> Result<Arc<MftVolume>, HelperError> {
        let target = self.target(volume)?;
        let reader = self.open_reader(&target)?;
        let v = Arc::new(NtfsVolume::open(reader)?);
        self.lock().insert(target, Arc::clone(&v));
        Ok(v)
    }

    /// The cached volume, opened on first use.
    ///
    /// # Errors
    ///
    /// As [`Volumes::open_fresh`].
    pub fn cached(&self, volume: &str) -> Result<Arc<MftVolume>, HelperError> {
        let target = self.target(volume)?;
        if let Some(v) = self.lock().get(&target) {
            return Ok(Arc::clone(v));
        }
        self.open_fresh(volume)
    }

    /// Drops a cached volume (after an I/O error or MFT growth).
    pub fn invalidate(&self, volume: &str) {
        if let Ok(t) = parse_volume_id(volume) {
            self.lock().remove(&t);
        }
    }

    /// The device path for volume FSCTLs (USN journal).
    ///
    /// # Errors
    ///
    /// Invalid id, or the image source (which has no journal).
    pub fn device(&self, volume: &str) -> Result<String, HelperError> {
        match self.target(volume)? {
            VolumeTarget::Device(d) => Ok(d),
            VolumeTarget::Image => Err(HelperError::new(
                strata_ipc::protocol::ErrorCode::NotSupported,
                "image volumes have no USN journal",
            )),
        }
    }

    /// Volumes as the helper sees them, plus a synthetic entry for the image
    /// source when one is configured.
    ///
    /// # Errors
    ///
    /// Volume enumeration failed.
    pub fn list(&self) -> Result<Vec<VolumeInfo>, HelperError> {
        let mut out =
            strata_win::volume::discover_volumes(strata_win::volume::DiscoveryOptions::local())?;
        if let Some(image) = &self.image {
            out.push(image_volume_info(image));
        }
        Ok(out)
    }

    fn open_reader(&self, target: &VolumeTarget) -> Result<DynReader, HelperError> {
        match target {
            VolumeTarget::Device(path) => {
                // NOTE: privileges are checked when the handle is opened, so
                // the scope only needs to cover the open.
                let _privs = PrivilegeScope::acquire(VOLUME_PRIVILEGES);
                let raw = RawVolume::open(path, IoMode::NoBuffering)
                    .map_err(|e| HelperError::from_io("cannot open the volume", &e))?;
                Ok(DynReader(Box::new(raw)))
            }
            VolumeTarget::Image => {
                let path = self
                    .image
                    .as_ref()
                    .ok_or_else(|| HelperError::internal("image target without an image source"))?;
                let f = File::open(path)
                    .map_err(|e| HelperError::from_io("cannot open the image", &e))?;
                Ok(DynReader(Box::new(f)))
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<VolumeTarget, Arc<MftVolume>>> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn image_volume_info(image: &std::path::Path) -> VolumeInfo {
    use strata_win::volume::{BitLockerState, DevDriveState, DriveKind, FileSystemKind};
    let len = std::fs::metadata(image).map(|m| m.len()).ok();
    VolumeInfo {
        guid_path: Some(IMAGE_VOLUME.into()),
        device_path: None,
        mount_paths: Vec::new(),
        drive_letter: None,
        nested_in: Vec::new(),
        kind: DriveKind::Fixed,
        remote_path: None,
        fs_name: Some("NTFS".into()),
        filesystem: FileSystemKind::Ntfs,
        label: Some("image".into()),
        serial: None,
        fs_flags: 0,
        max_component_len: Some(255),
        supports_usn_journal: false,
        case_sensitive_search: true,
        case_preserved_names: true,
        read_only: true,
        compressed: false,
        cluster_size: None,
        sector_size: None,
        total_bytes: len,
        free_bytes: None,
        available_bytes: None,
        is_system: false,
        bitlocker: BitLockerState::NotEncrypted,
        dev_drive: DevDriveState::NotDevDrive,
        ready: true,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_ids_are_strict() {
        assert_eq!(
            parse_volume_id(r"\\?\Volume{11111111-1111-4111-8111-111111111111}\").unwrap(),
            VolumeTarget::Device(r"\\?\Volume{11111111-1111-4111-8111-111111111111}".into())
        );
        assert_eq!(
            parse_volume_id(r"\\?\Volume{AAAAAAAA-1111-4111-8111-111111111111}").unwrap(),
            VolumeTarget::Device(r"\\?\Volume{aaaaaaaa-1111-4111-8111-111111111111}".into())
        );
        assert_eq!(
            parse_volume_id("d:").unwrap(),
            VolumeTarget::Device(r"\\.\D:".into())
        );
        assert_eq!(parse_volume_id(IMAGE_VOLUME).unwrap(), VolumeTarget::Image);
        for bad in [
            "",
            "C",
            r"C:\x",
            r"\\.\C:",
            r"\\?\C:\",
            r"\\?\Volume{11111111-1111-4111-8111-111111111111}\Windows",
            r"\\?\Volume{11111111-1111-4111-8111-11111111111}\",
            r"\\?\Volume{11111111-1111-4111-8111-11111111111g}\",
            r"\\?\Volume{11111111-1111-4111-8111-111111111111}\..\x",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1",
            r"..\..\secret.img",
            "1:",
        ] {
            assert!(parse_volume_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn image_id_needs_an_image_source() {
        assert!(Volumes::raw().target(IMAGE_VOLUME).is_err());
        assert!(Volumes::raw().device("C:").is_ok());
    }
}
