//! Minimal read-only registry access.

use std::ffi::OsString;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS};
use windows::Win32::System::Registry::{
    HKEY, KEY_READ, KEY_WOW64_64KEY, REG_DWORD, REG_EXPAND_SZ, REG_SZ, REG_VALUE_TYPE, RegCloseKey,
    RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW,
};
use windows::core::PWSTR;

use crate::error::{Result, WinError, check};
use crate::wide::{WideCString, from_wide_nul};

/// An open registry key, closed on drop.
#[derive(Debug)]
pub(crate) struct RegKey(HKEY);

/// A string value and whether it was `REG_EXPAND_SZ` (unexpanded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegString {
    pub value: OsString,
    pub expandable: bool,
}

impl RegKey {
    /// Opens `subkey` under `root` for reading, in the 64-bit view.
    ///
    /// Returns `Ok(None)` when the key does not exist.
    pub(crate) fn open(root: HKEY, subkey: &str) -> Result<Option<Self>> {
        let name = WideCString::new(subkey);
        let mut out = HKEY::default();
        // SAFETY: `name` is NUL-terminated and outlives the call; `out` is a
        // valid out-pointer.
        let status = unsafe {
            RegOpenKeyExW(
                root,
                name.as_pcwstr(),
                None,
                KEY_READ | KEY_WOW64_64KEY,
                &mut out,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check("RegOpenKeyExW", status)?;
        Ok(Some(Self(out)))
    }

    /// Reads raw value bytes and type. `Ok(None)` when the value is absent.
    fn query_raw(&self, name: &str) -> Result<Option<(REG_VALUE_TYPE, Vec<u8>)>> {
        let name = WideCString::new(name);
        let mut buf = vec![0u8; 512];
        loop {
            let mut ty = REG_VALUE_TYPE::default();
            let mut len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
            // SAFETY: `buf` has `len` writable bytes; all pointers are valid
            // for the duration of the call.
            let status = unsafe {
                RegQueryValueExW(
                    self.0,
                    name.as_pcwstr(),
                    None,
                    Some(&mut ty),
                    Some(buf.as_mut_ptr()),
                    Some(&mut len),
                )
            };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            if status == ERROR_MORE_DATA {
                // NOTE: values can grow between calls; loop until it fits.
                buf.resize(len as usize + 2, 0);
                continue;
            }
            check("RegQueryValueExW", status)?;
            buf.truncate(len as usize);
            return Ok(Some((ty, buf)));
        }
    }

    /// Reads a `REG_SZ` or `REG_EXPAND_SZ` value without expanding it.
    pub(crate) fn query_string(&self, name: &str) -> Result<Option<RegString>> {
        let Some((ty, bytes)) = self.query_raw(name)? else {
            return Ok(None);
        };
        if ty != REG_SZ && ty != REG_EXPAND_SZ {
            return Err(WinError::from_win32("RegQueryValueExW (type)", 13));
        }
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Ok(Some(RegString {
            value: from_wide_nul(&units),
            expandable: ty == REG_EXPAND_SZ,
        }))
    }

    /// Reads a `REG_DWORD` value.
    pub(crate) fn query_dword(&self, name: &str) -> Result<Option<u32>> {
        let Some((ty, bytes)) = self.query_raw(name)? else {
            return Ok(None);
        };
        if ty != REG_DWORD || bytes.len() < 4 {
            return Err(WinError::from_win32("RegQueryValueExW (type)", 13));
        }
        Ok(Some(u32::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3],
        ])))
    }

    /// Names of the immediate subkeys.
    pub(crate) fn subkeys(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut index = 0;
        loop {
            // NOTE: registry key names are limited to 255 characters.
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            // SAFETY: `name` has `len` writable units; other pointers are
            // optional and passed as None.
            let status = unsafe {
                RegEnumKeyExW(
                    self.0,
                    index,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if status == ERROR_NO_MORE_ITEMS {
                return Ok(out);
            }
            check("RegEnumKeyExW", status)?;
            out.push(String::from_utf16_lossy(&name[..len as usize]));
            index += 1;
        }
    }
}

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: the key was opened by us and is closed exactly once.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
