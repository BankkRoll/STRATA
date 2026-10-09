//! Volumes, drive types, Recycle Bin settings, registry and machine names.

use std::io;
use std::mem::size_of;

use windows::Win32::Foundation::{ERROR_NO_MORE_FILES, ERROR_SUCCESS};
use windows::Win32::Storage::FileSystem::{
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW, GetDriveTypeW,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, GetVolumePathNamesForVolumeNameW,
    QueryDosDeviceW,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::SystemInformation::{
    COMPUTER_NAME_FORMAT, ComputerNameDnsFullyQualified, ComputerNameDnsHostname,
    ComputerNameNetBIOS, GetComputerNameExW,
};
use windows::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};
use windows::core::{PCWSTR, PWSTR};

use super::wide;

/// One volume as reported by the volume manager.
#[derive(Debug, Clone)]
pub(crate) struct RawVolume {
    /// `\\?\Volume{GUID}\`.
    pub guid_path: String,
    /// `\Device\HarddiskVolumeN`.
    pub device: Option<String>,
    /// Drive roots and folder mount points, each with a trailing `\`.
    pub mount_points: Vec<String>,
}

/// Enumerates every volume, its NT device and its mount points.
pub(crate) fn enumerate_volumes() -> io::Result<Vec<RawVolume>> {
    let mut name = [0u16; 64];
    // SAFETY: `name` is a valid buffer.
    let find = unsafe { FindFirstVolumeW(&mut name) }?;
    let mut out = Vec::new();
    loop {
        let guid_path = from_nul(&name);
        out.push(RawVolume {
            device: device_of(&guid_path),
            mount_points: mount_points_of(&guid_path).unwrap_or_default(),
            guid_path,
        });
        // SAFETY: `find` came from FindFirstVolumeW; `name` is valid.
        match unsafe { FindNextVolumeW(find, &mut name) } {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_NO_MORE_FILES.to_hresult() => break,
            Err(e) => {
                // SAFETY: closing the search handle we opened.
                let _ = unsafe { FindVolumeClose(find) };
                return Err(e.into());
            }
        }
    }
    // SAFETY: closing the search handle we opened.
    let _ = unsafe { FindVolumeClose(find) };
    Ok(out)
}

fn device_of(guid_path: &str) -> Option<String> {
    // QueryDosDevice wants `Volume{GUID}` without prefix or trailing slash.
    let key = guid_path.strip_prefix(r"\\?\")?.trim_end_matches('\\');
    let key = wide(key);
    let mut buf = [0u16; 512];
    // SAFETY: `key` is NUL-terminated and `buf` is valid.
    let n = unsafe { QueryDosDeviceW(PCWSTR(key.as_ptr()), Some(&mut buf)) } as usize;
    (n > 0).then(|| from_nul(&buf[..n]))
}

fn mount_points_of(guid_path: &str) -> io::Result<Vec<String>> {
    let g = wide(guid_path);
    let mut buf = vec![0u16; 1024];
    let mut needed = 0u32;
    loop {
        // SAFETY: `g` is NUL-terminated; `buf` and `needed` are valid.
        match unsafe {
            GetVolumePathNamesForVolumeNameW(PCWSTR(g.as_ptr()), Some(&mut buf), &mut needed)
        } {
            Ok(()) => break,
            Err(_) if needed as usize > buf.len() => buf.resize(needed as usize, 0),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(buf
        .split(|&u| u == 0)
        .take_while(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect())
}

/// The mount point (with trailing `\`) that contains `path`.
pub(crate) fn volume_path_of(path: &[u16]) -> io::Result<Vec<u16>> {
    let mut buf = vec![0u16; 32 * 1024];
    // SAFETY: `path` is NUL-terminated; `buf` is valid.
    unsafe { GetVolumePathNameW(PCWSTR(path.as_ptr()), &mut buf) }?;
    let n = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
    buf.truncate(n);
    Ok(buf)
}

/// `\\?\Volume{GUID}\` for a mount point such as `C:\`.
pub(crate) fn volume_guid_of_mount(mount: &[u16]) -> io::Result<String> {
    let mut m = mount.to_vec();
    if m.last() != Some(&0) {
        m.push(0);
    }
    let mut buf = [0u16; 64];
    // SAFETY: `m` is NUL-terminated; `buf` is valid.
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(m.as_ptr()), &mut buf) }?;
    Ok(from_nul(&buf))
}

/// `GetDriveTypeW` for a root such as `C:\` or `\\?\Volume{...}\`.
pub(crate) fn drive_type(root: &str) -> u32 {
    let r = wide(root);
    // SAFETY: `r` is NUL-terminated.
    unsafe { GetDriveTypeW(PCWSTR(r.as_ptr())) }
}

/// Total size of the volume holding `root`, in bytes.
pub(crate) fn volume_total_bytes(root: &str) -> io::Result<u64> {
    let r = wide(root);
    let mut total = 0u64;
    // SAFETY: `r` is NUL-terminated; `total` is a valid out pointer.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(r.as_ptr()), None, Some(&mut total), None) }?;
    Ok(total)
}

/// `SHQueryRecycleBinW`: bytes and item count in the bin for the
/// drive holding `root`. Fails when the volume has no Recycle Bin.
pub(crate) fn query_recycle_bin(root: &str) -> io::Result<(u64, u64)> {
    let r = wide(root);
    let mut info = SHQUERYRBINFO {
        cbSize: size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `r` is NUL-terminated; `info` is valid with cbSize set.
    unsafe { SHQueryRecycleBinW(PCWSTR(r.as_ptr()), &mut info) }?;
    Ok((info.i64Size.max(0) as u64, info.i64NumItems.max(0) as u64))
}

/// Like [`query_recycle_bin`], for one root or (with `None`) every drive.
pub(crate) fn query_recycle_bin_any(root: Option<&str>) -> io::Result<(u64, u64)> {
    match root {
        Some(r) => query_recycle_bin(r),
        None => {
            let mut info = SHQUERYRBINFO {
                cbSize: size_of::<SHQUERYRBINFO>() as u32,
                ..Default::default()
            };
            // SAFETY: a null root means "all Recycle Bins"; `info` is valid.
            unsafe { SHQueryRecycleBinW(PCWSTR::null(), &mut info) }?;
            Ok((info.i64Size.max(0) as u64, info.i64NumItems.max(0) as u64))
        }
    }
}

/// `SHEmptyRecycleBinW` without Shell UI. Only `tools::empty_recycle_bin`
/// calls this, after validating a user consent; tests never do.
pub(crate) fn empty_recycle_bin(root: Option<&str>) -> io::Result<()> {
    use windows::Win32::UI::Shell::{
        SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND, SHEmptyRecycleBinW,
    };
    let w = root.map(wide);
    let p = w.as_ref().map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr()));
    // SAFETY: `p` is null or a NUL-terminated root path that outlives the call.
    unsafe {
        SHEmptyRecycleBinW(
            None,
            p,
            SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND,
        )
    }?;
    Ok(())
}

/// Reads a `REG_DWORD` under `HKEY_CURRENT_USER`; `None` when absent.
pub(crate) fn hkcu_dword(subkey: &str, value: &str) -> Option<u32> {
    let k = wide(subkey);
    let v = wide(value);
    let mut data = 0u32;
    let mut len = size_of::<u32>() as u32;
    // SAFETY: strings are NUL-terminated; `data`/`len` are valid out params.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(k.as_ptr()),
            PCWSTR(v.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut data).cast()),
            Some(&mut len),
        )
    };
    (r == ERROR_SUCCESS).then_some(data)
}

/// NetBIOS, DNS host and fully qualified names of this machine.
pub(crate) fn computer_names() -> Vec<String> {
    let formats: [COMPUTER_NAME_FORMAT; 3] = [
        ComputerNameNetBIOS,
        ComputerNameDnsHostname,
        ComputerNameDnsFullyQualified,
    ];
    let mut out = Vec::new();
    for f in formats {
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        // SAFETY: `buf` is valid for `len` units.
        if unsafe { GetComputerNameExW(f, Some(PWSTR(buf.as_mut_ptr())), &mut len) }.is_ok() {
            let s = String::from_utf16_lossy(&buf[..len as usize]);
            if !s.is_empty() && !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

fn from_nul(buf: &[u16]) -> String {
    let n = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}
