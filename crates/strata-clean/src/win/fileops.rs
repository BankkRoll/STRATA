//! Moves and delete-on-reboot.

use std::io;

use windows::Win32::Storage::FileSystem::{
    MOVE_FILE_FLAGS, MOVEFILE_DELAY_UNTIL_REBOOT, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

/// Renames `from` to `to` (both NUL-terminated), failing if `to` exists.
pub(crate) fn move_no_replace(from: &[u16], to: &[u16]) -> io::Result<()> {
    // SAFETY: both paths are NUL-terminated and outlive the call.
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
    }?;
    Ok(())
}

/// Schedules `path` (NUL-terminated) for deletion at the next boot. Needs
/// write access to `HKLM\...\Session Manager`, i.e. elevation.
pub(crate) fn delete_on_reboot(path: &[u16]) -> io::Result<()> {
    // SAFETY: `path` is NUL-terminated; a null destination means delete.
    unsafe {
        MoveFileExW(
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            MOVE_FILE_FLAGS(MOVEFILE_DELAY_UNTIL_REBOOT.0),
        )
    }?;
    Ok(())
}
