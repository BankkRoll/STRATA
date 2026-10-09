//! UTF-16 string conversions for Win32 calls.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};

use windows::core::PCWSTR;

/// A NUL-terminated UTF-16 buffer that can be passed as `PCWSTR`.
#[derive(Debug, Clone)]
pub(crate) struct WideCString(Vec<u16>);

impl WideCString {
    /// Encodes `s`. Interior NULs truncate the string as Win32 would see it,
    /// so callers that accept untrusted input must reject NULs first.
    pub(crate) fn new(s: impl AsRef<OsStr>) -> Self {
        Self(s.as_ref().encode_wide().chain(std::iter::once(0)).collect())
    }

    /// Pointer valid for as long as `self` lives.
    pub(crate) fn as_pcwstr(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }
}

/// Decodes a buffer up to its first NUL (or the whole buffer if none).
pub(crate) fn from_wide_nul(buf: &[u16]) -> OsString {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..len])
}

/// Splits a `REG_MULTI_SZ`-style buffer (strings separated by NUL, ending
/// with an empty string) into its parts.
pub(crate) fn split_multi_sz(buf: &[u16]) -> Vec<OsString> {
    buf.split(|&c| c == 0)
        .take_while(|s| !s.is_empty())
        .map(OsString::from_wide)
        .collect()
}

/// Reads a NUL-terminated string owned by the system.
///
/// # Safety
///
/// `p` must be null or point to a NUL-terminated UTF-16 string that stays
/// valid for the duration of the call.
pub(crate) unsafe fn from_pwstr(p: PCWSTR) -> OsString {
    if p.is_null() {
        return OsString::new();
    }
    // SAFETY: the caller guarantees `p` is a valid NUL-terminated string.
    OsString::from_wide(unsafe { p.as_wide() })
}

/// Encodes without a terminator (for comparisons).
pub(crate) fn encode(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nul_handling() {
        let w = WideCString::new("ab");
        assert_eq!(w.0, vec![97, 98, 0]);
        assert_eq!(from_wide_nul(&[97, 0, 98]), OsString::from("a"));
        assert_eq!(from_wide_nul(&[97, 98]), OsString::from("ab"));
    }

    #[test]
    fn multi_sz() {
        let buf: Vec<u16> = "C:\\\0D:\\mnt\\\0\0".encode_utf16().collect();
        assert_eq!(
            split_multi_sz(&buf),
            vec![OsString::from("C:\\"), OsString::from("D:\\mnt\\")]
        );
        assert!(split_multi_sz(&[0, 0]).is_empty());
        assert!(split_multi_sz(&[]).is_empty());
    }
}
