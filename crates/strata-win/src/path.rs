//! Path conversions: verbatim (`\\?\`) paths, final paths of handles, NT
//! device paths to DOS paths, and volume GUID paths to mount points.
//!
//! These are shared by the scanners, the ETW tracer (which reports NT device
//! paths such as `\Device\HarddiskVolume3\x`) and cleanup (which reopens files
//! by verbatim path to bypass `MAX_PATH` and Win32 name normalization).

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_MORE_DATA, HANDLE};
use windows::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GETFINALPATHNAMEBYHANDLE_FLAGS, GetFinalPathNameByHandleW, GetFullPathNameW,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, GetVolumePathNamesForVolumeNameW,
    OPEN_EXISTING, QueryDosDeviceW, VOLUME_NAME_DOS,
};

use crate::error::{Context, Result, WinError};
use crate::handle::OwnedHandle;
use crate::wide::{WideCString, encode, from_wide_nul, split_multi_sz};

const VERBATIM: &str = r"\\?\";
const VERBATIM_UNC: &str = r"\\?\UNC\";
const NT_GLOBAL: &str = r"\??\";
const DEVICE_NS: &str = r"\\.\";

/// Converts `path` to a verbatim path (`\\?\C:\x` or `\\?\UNC\server\share\x`).
///
/// Relative paths are made absolute first with `GetFullPathNameW`, which also
/// applies Win32 normalization (`.`/`..`, `/` to `\`, and trailing dots and
/// spaces stripped from components). Already-verbatim, NT (`\??\`) and device
/// namespace (`\\.\`) paths are returned unchanged.
///
/// # Example
///
/// ```
/// use std::path::Path;
/// let v = strata_win::path::to_verbatim(Path::new(r"C:\Windows\..\Users")).unwrap();
/// assert_eq!(v, Path::new(r"\\?\C:\Users"));
/// let u = strata_win::path::to_verbatim(Path::new(r"\\server\share\x")).unwrap();
/// assert_eq!(u, Path::new(r"\\?\UNC\server\share\x"));
/// ```
pub fn to_verbatim(path: &Path) -> Result<PathBuf> {
    let units = encode(path);
    if starts_with(&units, VERBATIM)
        || starts_with(&units, NT_GLOBAL)
        || starts_with(&units, DEVICE_NS)
    {
        return Ok(path.to_path_buf());
    }
    let full = full_path(path)?;
    Ok(PathBuf::from(OsString::from_wide(&verbatim_from_absolute(
        &full,
    ))))
}

/// Strips a `\\?\` or `\\?\UNC\` prefix for display (`\\?\C:\x` → `C:\x`).
///
/// Volume GUID paths (`\\?\Volume{...}\`) are left as they are because they
/// have no shorter form.
#[must_use]
pub fn strip_verbatim(path: &Path) -> PathBuf {
    let units = encode(path);
    if starts_with(&units, VERBATIM_UNC) {
        let mut out = encode(r"\\");
        out.extend_from_slice(&units[VERBATIM_UNC.len()..]);
        return PathBuf::from(OsString::from_wide(&out));
    }
    if starts_with(&units, VERBATIM) {
        let rest = &units[VERBATIM.len()..];
        if rest.len() >= 2 && rest[1] == u16::from(b':') {
            return PathBuf::from(OsString::from_wide(rest));
        }
    }
    path.to_path_buf()
}

fn verbatim_from_absolute(full: &[u16]) -> Vec<u16> {
    if starts_with(full, r"\\") {
        let mut out = encode(VERBATIM_UNC);
        out.extend_from_slice(&full[2..]);
        out
    } else {
        let mut out = encode(VERBATIM);
        out.extend_from_slice(full);
        out
    }
}

fn starts_with(units: &[u16], prefix: &str) -> bool {
    let p: Vec<u16> = prefix.encode_utf16().collect();
    units.len() >= p.len() && units[..p.len()] == p[..]
}

fn full_path(path: &Path) -> Result<Vec<u16>> {
    let wide = WideCString::new(path);
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: `buf` is writable for its full length; the file-part
        // out-pointer is not requested.
        let n = unsafe { GetFullPathNameW(wide.as_pcwstr(), Some(&mut buf), None) } as usize;
        if n == 0 {
            return Err(WinError::last("GetFullPathNameW"));
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(buf);
        }
        buf.resize(n + 1, 0);
    }
}

/// The normalized DOS path of an open handle (`GetFinalPathNameByHandleW`),
/// with symlinks and junctions resolved, as a verbatim path (`\\?\C:\x`).
///
/// # Safety-relevant note
///
/// The handle is borrowed; it must stay open for the duration of the call.
pub fn final_path(handle: HANDLE) -> Result<PathBuf> {
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: `buf` is writable for its full length and the caller keeps
        // `handle` open.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                &mut buf,
                GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0),
            )
        } as usize;
        if n == 0 {
            return Err(WinError::last("GetFinalPathNameByHandleW"));
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(PathBuf::from(OsString::from_wide(&buf)));
        }
        buf.resize(n + 1, 0);
    }
}

/// Opens `path` for attribute reads only, without following a final reparse
/// point when `no_follow` is set. Directories are supported.
pub fn open_for_attributes(path: &Path, no_follow: bool) -> Result<OwnedHandle> {
    let wide = WideCString::new(path);
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if no_follow {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    // SAFETY: `wide` is NUL-terminated; the returned handle is owned below.
    let h = unsafe {
        CreateFileW(
            wide.as_pcwstr(),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    }
    .ctx("CreateFileW")?;
    // SAFETY: CreateFileW returned a fresh handle that we now own.
    unsafe { OwnedHandle::from_raw(h) }.ok_or_else(|| WinError::from_win32("CreateFileW", 6))
}

/// The final (link-resolved) verbatim path of `path`.
pub fn final_path_of(path: &Path) -> Result<PathBuf> {
    let h = open_for_attributes(path, false)?;
    final_path(h.raw())
}

/// Case-insensitive ordinal comparison, as NTFS compares names on
/// case-insensitive directories (`CompareStringOrdinal` with `bIgnoreCase`).
#[must_use]
pub fn eq_ignore_case(a: &OsStr, b: &OsStr) -> bool {
    let a: Vec<u16> = a.encode_wide().collect();
    let b: Vec<u16> = b.encode_wide().collect();
    // SAFETY: both slices are valid for their lengths.
    unsafe { CompareStringOrdinal(&a, &b, true) == CSTR_EQUAL }
}

/// The volume GUID path (`\\?\Volume{...}\`) of a mount point (`C:\`,
/// `D:\mnt\data\`). A trailing backslash is added if missing.
pub fn volume_guid_for_mount_point(mount: &Path) -> Result<String> {
    let mut m = encode(mount);
    if m.last() != Some(&u16::from(b'\\')) {
        m.push(u16::from(b'\\'));
    }
    m.push(0);
    let mut buf = [0u16; 64];
    // SAFETY: `m` is NUL-terminated; `buf` holds the documented 50 characters.
    unsafe { GetVolumeNameForVolumeMountPointW(windows::core::PCWSTR(m.as_ptr()), &mut buf) }
        .ctx("GetVolumeNameForVolumeMountPointW")?;
    Ok(from_wide_nul(&buf).to_string_lossy().into_owned())
}

/// Every mount path of a volume: drive letters (`C:\`) and folder mount
/// points (`D:\mnt\data\`), as reported by `GetVolumePathNamesForVolumeNameW`.
pub fn mount_points_for_volume(guid_path: &str) -> Result<Vec<PathBuf>> {
    let name = WideCString::new(guid_path);
    let mut buf = vec![0u16; 256];
    loop {
        let mut needed = 0u32;
        // SAFETY: `buf` is writable for its length; `needed` receives the
        // required size on ERROR_MORE_DATA.
        let r = unsafe {
            GetVolumePathNamesForVolumeNameW(name.as_pcwstr(), Some(&mut buf), &mut needed)
        };
        match r {
            Ok(()) => {
                return Ok(split_multi_sz(&buf)
                    .into_iter()
                    .map(PathBuf::from)
                    .collect());
            }
            Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {
                buf.resize(needed.max(buf.len() as u32 * 2) as usize, 0);
            }
            Err(e) => return Err(WinError::new("GetVolumePathNamesForVolumeNameW", &e)),
        }
    }
}

/// The mount root of the volume containing `path` (`GetVolumePathNameW`):
/// `C:\` for `C:\Users`, or `D:\mnt\data\` inside a folder-mounted volume.
pub fn volume_mount_root(path: &Path) -> Result<PathBuf> {
    let wide = WideCString::new(path);
    let mut buf = vec![0u16; encode(path).len() + 64];
    // SAFETY: `buf` is at least as long as the input path, as documented.
    unsafe { GetVolumePathNameW(wide.as_pcwstr(), &mut buf) }.ctx("GetVolumePathNameW")?;
    Ok(PathBuf::from(from_wide_nul(&buf)))
}

/// `QueryDosDeviceW` for one DOS device name (`C:`, `Volume{...}`): the first
/// NT target, e.g. `\Device\HarddiskVolume3`.
pub fn query_dos_device(name: &str) -> Result<String> {
    let wide = WideCString::new(name);
    let mut buf = vec![0u16; 1024];
    // SAFETY: `buf` is writable for its full length.
    let n = unsafe { QueryDosDeviceW(wide.as_pcwstr(), Some(&mut buf)) };
    if n == 0 {
        return Err(WinError::last("QueryDosDeviceW"));
    }
    Ok(split_multi_sz(&buf[..n as usize])
        .into_iter()
        .next()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned())
}

/// Maps NT device paths to DOS paths.
///
/// Built from every local volume (device → preferred mount path: a drive
/// letter when it has one, else its first folder mount point) plus mapped
/// network drives. Matching is by longest device prefix on a component
/// boundary, so `\Device\HarddiskVolume1` never matches
/// `\Device\HarddiskVolume10\x`.
///
/// # Example
///
/// ```
/// use std::ffi::OsStr;
/// use std::path::Path;
/// use strata_win::path::DeviceMap;
/// let map = DeviceMap::from_entries([
///     (r"\Device\HarddiskVolume3".to_owned(), r"C:\".into()),
///     (r"\Device\HarddiskVolume7".to_owned(), r"C:\mnt\data\".into()),
/// ]);
/// assert_eq!(
///     map.to_dos(OsStr::new(r"\Device\HarddiskVolume7\models\a.bin")).unwrap(),
///     Path::new(r"C:\mnt\data\models\a.bin"),
/// );
/// ```
#[derive(Debug, Clone, Default)]
pub struct DeviceMap {
    /// (device path without trailing `\`, DOS root with trailing `\`).
    entries: Vec<(Vec<u16>, PathBuf)>,
}

impl DeviceMap {
    /// Builds a map from explicit (device, DOS root) pairs.
    pub fn from_entries(entries: impl IntoIterator<Item = (String, PathBuf)>) -> Self {
        let mut map = Self::default();
        for (dev, root) in entries {
            map.insert(&dev, root);
        }
        map
    }

    fn insert(&mut self, device: &str, root: PathBuf) {
        let mut dev: Vec<u16> = device.encode_utf16().collect();
        while dev.last() == Some(&u16::from(b'\\')) {
            dev.pop();
        }
        if dev.is_empty() {
            return;
        }
        let mut r = encode(&root);
        if r.last() != Some(&u16::from(b'\\')) {
            r.push(u16::from(b'\\'));
        }
        self.entries
            .push((dev, PathBuf::from(OsString::from_wide(&r))));
        // NOTE: longest device first so prefix matching picks the most
        // specific entry.
        self.entries.sort_by_key(|e| std::cmp::Reverse(e.0.len()));
    }

    /// Enumerates the current machine's volumes and mapped drives.
    pub fn current() -> Result<Self> {
        let mut map = Self::default();
        for guid in crate::volume::volume_guid_paths()? {
            let Some(inner) = guid
                .strip_prefix(VERBATIM)
                .map(|s| s.trim_end_matches('\\'))
            else {
                continue;
            };
            let Ok(device) = query_dos_device(inner) else {
                continue;
            };
            let mounts = mount_points_for_volume(&guid).unwrap_or_default();
            let preferred = mounts
                .iter()
                .find(|m| encode(m).len() == 3)
                .or_else(|| mounts.first());
            if let Some(root) = preferred {
                map.insert(&device, root.clone());
            }
        }
        // NOTE: mapped network drives and SUBST drives are not volumes. Network
        // drives resolve under `\Device\LanmanRedirector\;Z:...\server\share` or
        // `\Device\Mup\...`, which the generic UNC rule below handles.
        Ok(map)
    }

    /// Translates an NT path. `\Device\Mup\server\share\x` becomes
    /// `\\server\share\x`; unknown devices return `None`.
    #[must_use]
    pub fn to_dos(&self, nt_path: &OsStr) -> Option<PathBuf> {
        let units: Vec<u16> = nt_path.encode_wide().collect();
        for (dev, root) in &self.entries {
            if units.len() < dev.len() || !ascii_eq_ignore_case(&units[..dev.len()], dev) {
                continue;
            }
            let rest = &units[dev.len()..];
            if rest.is_empty() {
                return Some(root.clone());
            }
            if rest[0] != u16::from(b'\\') {
                continue;
            }
            let mut out = encode(root);
            out.extend_from_slice(&rest[1..]);
            return Some(PathBuf::from(OsString::from_wide(&out)));
        }
        let mup: Vec<u16> = r"\Device\Mup\".encode_utf16().collect();
        if units.len() > mup.len() && ascii_eq_ignore_case(&units[..mup.len()], &mup) {
            let mut out = encode(r"\\");
            out.extend_from_slice(&units[mup.len()..]);
            return Some(PathBuf::from(OsString::from_wide(&out)));
        }
        None
    }
}

fn ascii_eq_ignore_case(a: &[u16], b: &[u16]) -> bool {
    let lower = |c: u16| {
        if (u16::from(b'A')..=u16::from(b'Z')).contains(&c) {
            c + 32
        } else {
            c
        }
    };
    a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| lower(x) == lower(y))
}

/// Translates one NT device path using a freshly built [`DeviceMap`].
///
/// Callers translating many paths should build the map once.
pub fn device_to_dos_path(nt_path: &OsStr) -> Result<Option<PathBuf>> {
    Ok(DeviceMap::current()?.to_dos(nt_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn verbatim_conversion_rules() {
        assert_eq!(verbatim_from_absolute(&w(r"C:\a")), w(r"\\?\C:\a"));
        assert_eq!(
            verbatim_from_absolute(&w(r"\\srv\share\a")),
            w(r"\\?\UNC\srv\share\a")
        );
        for p in [r"\\?\C:\a.", r"\??\C:\x", r"\\.\PhysicalDrive0"] {
            assert_eq!(to_verbatim(Path::new(p)).unwrap(), Path::new(p));
        }
    }

    #[test]
    fn verbatim_normalizes_relative() {
        let v = to_verbatim(Path::new(".")).unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(strip_verbatim(&v), cwd);
    }

    #[test]
    fn strip_rules() {
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:\x")), Path::new(r"C:\x"));
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\s\sh\x")),
            Path::new(r"\\s\sh\x")
        );
        let g = r"\\?\Volume{0000}\x";
        assert_eq!(strip_verbatim(Path::new(g)), Path::new(g));
    }

    #[test]
    fn device_map_prefers_component_boundaries() {
        let map = DeviceMap::from_entries([
            (r"\Device\HarddiskVolume1".to_owned(), PathBuf::from(r"E:\")),
            (
                r"\Device\HarddiskVolume10\".to_owned(),
                PathBuf::from(r"C:\mnt\ten"),
            ),
        ]);
        assert_eq!(
            map.to_dos(OsStr::new(r"\Device\HarddiskVolume10\x"))
                .unwrap(),
            Path::new(r"C:\mnt\ten\x")
        );
        assert_eq!(
            map.to_dos(OsStr::new(r"\device\harddiskvolume1\y"))
                .unwrap(),
            Path::new(r"E:\y")
        );
        assert_eq!(
            map.to_dos(OsStr::new(r"\Device\HarddiskVolume1")).unwrap(),
            Path::new(r"E:\")
        );
        assert!(
            map.to_dos(OsStr::new(r"\Device\HarddiskVolume100\z"))
                .is_none()
        );
        assert_eq!(
            map.to_dos(OsStr::new(r"\Device\Mup\srv\share\f")).unwrap(),
            Path::new(r"\\srv\share\f")
        );
    }

    #[test]
    fn real_system_drive_round_trips() {
        let windir = std::env::var_os("windir").unwrap();
        let root = volume_mount_root(Path::new(&windir)).unwrap();
        let guid = volume_guid_for_mount_point(&root).unwrap();
        assert!(guid.starts_with(r"\\?\Volume{"), "{guid}");
        let mounts = mount_points_for_volume(&guid).unwrap();
        assert!(
            mounts
                .iter()
                .any(|m| eq_ignore_case(m.as_os_str(), root.as_os_str()))
        );

        let letter = &root.to_string_lossy()[..2];
        let device = query_dos_device(letter).unwrap();
        assert!(device.starts_with(r"\Device\"), "{device}");
        let map = DeviceMap::current().unwrap();
        let nt = format!(r"{device}\Windows\System32");
        let dos = map.to_dos(OsStr::new(&nt)).unwrap();
        assert!(eq_ignore_case(
            dos.as_os_str(),
            root.join(r"Windows\System32").as_os_str()
        ));
    }

    #[test]
    fn final_path_resolves_real_directory() {
        let windir = PathBuf::from(std::env::var_os("windir").unwrap());
        let fp = final_path_of(&windir).unwrap();
        assert!(eq_ignore_case(
            strip_verbatim(&fp).as_os_str(),
            windir.as_os_str()
        ));
    }

    #[test]
    fn ordinal_case_folding() {
        assert!(eq_ignore_case(OsStr::new("ÄbC"), OsStr::new("äBc")));
        assert!(!eq_ignore_case(OsStr::new("a"), OsStr::new("b")));
    }
}
