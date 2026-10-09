//! The two Win32 calls the CLI needs: token elevation and volume free space.
//!
//! This is the only module with `unsafe`; each call is a thin FFI wrapper.

#![allow(unsafe_code)]

use std::io;

#[cfg(windows)]
fn io_err(e: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(e.code().0 & 0xFFFF)
}

/// Whether the current process token is elevated (UAC "run as administrator").
///
/// # Errors
///
/// The process token cannot be opened or queried.
#[cfg(windows)]
pub fn is_elevated() -> io::Result<bool> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing,
    // and `token` is a valid out pointer for the opened token handle.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.map_err(io_err)?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: the buffer is a live TOKEN_ELEVATION whose exact size is passed,
    // and `token` was opened with TOKEN_QUERY above.
    let queried = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    // SAFETY: `token` is a handle we own; it is closed exactly once.
    let _ = unsafe { CloseHandle(token) };
    queried.map_err(io_err)?;
    Ok(elevation.TokenIsElevated != 0)
}

/// Elevation cannot be determined off Windows; raw volumes are unsupported there.
///
/// # Errors
///
/// Always.
#[cfg(not(windows))]
pub fn is_elevated() -> io::Result<bool> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Windows only"))
}

/// `(total, free)` bytes of the volume whose root is `root` (e.g. `C:\`).
///
/// # Errors
///
/// `GetDiskFreeSpaceExW` fails (no such drive, not ready, ...).
#[cfg(windows)]
pub fn disk_space(root: &str) -> io::Result<(u64, u64)> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows::core::PCWSTR;

    let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
    let mut total = 0u64;
    let mut free = 0u64;
    // SAFETY: `wide` is NUL-terminated and outlives the call; the out
    // pointers reference live u64s.
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            None,
            Some(&mut total),
            Some(&mut free),
        )
    }
    .map_err(io_err)?;
    Ok((total, free))
}

/// Unsupported off Windows.
///
/// # Errors
///
/// Always.
#[cfg(not(windows))]
pub fn disk_space(_root: &str) -> io::Result<(u64, u64)> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Windows only"))
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn elevation_query_succeeds() {
        assert!(super::is_elevated().is_ok());
    }

    #[test]
    fn system_drive_has_space_info() {
        let (total, free) = super::disk_space(r"C:\").unwrap();
        assert!(total > 0 && free <= total);
        assert!(super::disk_space(r"\\?\no-such-volume\").is_err());
    }
}
