//! USN change journal FSCTLs.
//!
//! - `FSCTL_QUERY_USN_JOURNAL` → [`query_journal`] (`None` when inactive).
//! - `FSCTL_READ_USN_JOURNAL` → [`read_journal`]: a blocking read with
//!   `BytesToWaitFor`, issued overlapped so it can be cut short by the
//!   request's [`Cancel`] or by the helper's own timeout.
//! - `FSCTL_CREATE_USN_JOURNAL` → [`create_journal`] (user-confirmed in the
//!   app; it changes volume state).
//!
//! The returned bytes are exactly what the kernel produced; the app parses
//! them with `strata_ntfs::parse_usn_buffer`, so the elevated side does no
//! parsing of journal contents.

use std::ffi::c_void;
use std::time::Duration;

use strata_ipc::protocol::{ErrorCode, UsnJournalInfo};
use strata_win::OwnedHandle;
use windows::Win32::Foundation::{
    ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Ioctl::{
    CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL, FSCTL_QUERY_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V0,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};
use windows::core::PCWSTR;

use crate::cancel::Cancel;
use crate::error::HelperError;
use crate::privs::{PrivilegeScope, VOLUME_PRIVILEGES};

/// `ERROR_JOURNAL_DELETE_IN_PROGRESS`.
const ERROR_JOURNAL_DELETE_IN_PROGRESS: u32 = 1178;
/// `ERROR_JOURNAL_NOT_ACTIVE`.
const ERROR_JOURNAL_NOT_ACTIVE: u32 = 1179;
/// `ERROR_JOURNAL_ENTRY_DELETED`.
const ERROR_JOURNAL_ENTRY_DELETED: u32 = 1181;
/// `ERROR_OPERATION_ABORTED`.
const ERROR_OPERATION_ABORTED: u32 = 995;

/// Smallest read buffer (one large record fits).
pub const MIN_READ_BYTES: u32 = 4096;
/// Largest read buffer.
pub const MAX_READ_BYTES: u32 = 4 * 1024 * 1024;
/// Longest blocking wait a client may request.
pub const MAX_WAIT: Duration = Duration::from_secs(60);
/// Default journal size when the client passes 0 (what `fsutil usn
/// createjournal` documentation suggests for a system volume).
pub const DEFAULT_MAX_SIZE: u64 = 32 * 1024 * 1024;
/// Default allocation delta when the client passes 0.
pub const DEFAULT_ALLOCATION_DELTA: u64 = 8 * 1024 * 1024;

/// Opens a volume device for FSCTLs.
fn open_volume(device: &str, write: bool, overlapped: bool) -> Result<OwnedHandle, HelperError> {
    let wide: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
    let access = if write {
        GENERIC_READ.0 | GENERIC_WRITE.0
    } else {
        GENERIC_READ.0
    };
    let flags = if overlapped {
        FILE_FLAG_OVERLAPPED
    } else {
        FILE_FLAGS_AND_ATTRIBUTES(0)
    };
    let _privs = PrivilegeScope::acquire(VOLUME_PRIVILEGES);
    // SAFETY: `wide` is NUL-terminated; the handle is owned below.
    let h = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    }
    .map_err(|e| win_error("cannot open the volume", &e))?;
    // SAFETY: fresh handle from CreateFileW.
    unsafe { OwnedHandle::from_raw(h) }
        .ok_or_else(|| HelperError::internal("CreateFileW returned no handle"))
}

fn win_error(context: &str, e: &windows::core::Error) -> HelperError {
    let hr = e.code().0 as u32;
    let code = if hr & 0xFFFF_0000 == 0x8007_0000 {
        hr & 0xFFFF
    } else {
        hr
    };
    match code {
        ERROR_JOURNAL_ENTRY_DELETED => HelperError::new(
            ErrorCode::JournalWrapped,
            "the requested USN is no longer in the journal",
        ),
        ERROR_JOURNAL_NOT_ACTIVE => {
            HelperError::new(ErrorCode::JournalNotActive, "the USN journal is not active")
        }
        ERROR_JOURNAL_DELETE_IN_PROGRESS => HelperError::new(
            ErrorCode::JournalChanged,
            "the USN journal is being deleted",
        ),
        c => HelperError::from_win32(c, format!("{context}: {}", e.message().trim_end())),
    }
}

/// `FSCTL_QUERY_USN_JOURNAL` on an open handle; `None` when inactive or
/// being deleted.
fn query_handle(h: &OwnedHandle) -> Result<Option<UsnJournalInfo>, HelperError> {
    match query_raw(h) {
        Ok(info) => Ok(Some(info)),
        Err(e)
            if matches!(
                e.code,
                ErrorCode::JournalNotActive | ErrorCode::JournalChanged
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// `FSCTL_QUERY_USN_JOURNAL` on an open handle, with the journal errors
/// typed as [`win_error`] maps them.
fn query_raw(h: &OwnedHandle) -> Result<UsnJournalInfo, HelperError> {
    let mut data = USN_JOURNAL_DATA_V0::default();
    let mut returned = 0u32;
    // SAFETY: `data` is a writable USN_JOURNAL_DATA_V0 of the size passed;
    // synchronous call on a live handle.
    let r = unsafe {
        DeviceIoControl(
            h.raw(),
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some((&raw mut data).cast::<c_void>()),
            std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
            Some(&mut returned),
            None,
        )
    };
    r.map_err(|e| win_error("FSCTL_QUERY_USN_JOURNAL", &e))?;
    Ok(UsnJournalInfo {
        journal_id: data.UsnJournalID,
        first_usn: data.FirstUsn,
        next_usn: data.NextUsn,
        lowest_valid_usn: data.LowestValidUsn,
        max_usn: data.MaxUsn,
        maximum_size: data.MaximumSize,
        allocation_delta: data.AllocationDelta,
    })
}

/// Queries the journal of `device`. `Ok(None)` when it is not active.
///
/// # Errors
///
/// The volume cannot be opened (not elevated) or does not support journals.
pub fn query_journal(device: &str) -> Result<Option<UsnJournalInfo>, HelperError> {
    query_handle(&open_volume(device, false, false)?)
}

/// Creates (or resizes) the journal of `device`, then returns its state.
///
/// # Errors
///
/// The volume cannot be opened for writing or does not support journals.
pub fn create_journal(
    device: &str,
    maximum_size: u64,
    allocation_delta: u64,
) -> Result<UsnJournalInfo, HelperError> {
    let h = open_volume(device, true, false)?;
    let data = CREATE_USN_JOURNAL_DATA {
        MaximumSize: if maximum_size == 0 {
            DEFAULT_MAX_SIZE
        } else {
            maximum_size
        },
        AllocationDelta: if allocation_delta == 0 {
            DEFAULT_ALLOCATION_DELTA
        } else {
            allocation_delta
        },
    };
    let mut returned = 0u32;
    // SAFETY: `data` is a readable CREATE_USN_JOURNAL_DATA of the size
    // passed; synchronous call on a live handle.
    unsafe {
        DeviceIoControl(
            h.raw(),
            FSCTL_CREATE_USN_JOURNAL,
            Some((&raw const data).cast::<c_void>()),
            std::mem::size_of::<CREATE_USN_JOURNAL_DATA>() as u32,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| win_error("FSCTL_CREATE_USN_JOURNAL", &e))?;
    query_handle(&h)?
        .ok_or_else(|| HelperError::new(ErrorCode::Io, "the journal is not active after creation"))
}

/// Parameters of one journal read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadParams {
    /// Journal id the client expects.
    pub journal_id: u64,
    /// First USN to return.
    pub from: i64,
    /// Output buffer size (clamped to [`MIN_READ_BYTES`]..=[`MAX_READ_BYTES`]).
    pub max_bytes: u32,
    /// `BytesToWaitFor`: block until this many bytes of records exist.
    pub bytes_to_wait_for: u32,
    /// Upper bound on the wait (clamped to [`MAX_WAIT`]).
    pub timeout: Duration,
}

/// Reads journal records of `device` starting at `params.from`.
///
/// Returns `(next_usn, raw records)`: the kernel's output buffer with its
/// leading 8-byte next-USN split off. A wait that times out or is cancelled
/// returns `(from, [])`.
///
/// # Errors
///
/// [`ErrorCode::JournalChanged`] when the journal id differs or the journal
/// is being deleted, [`ErrorCode::JournalNotActive`] when the volume has no
/// active journal, [`ErrorCode::JournalWrapped`] when `from` was purged, or
/// an open/I/O error.
pub fn read_journal(
    device: &str,
    params: ReadParams,
    cancel: &Cancel,
) -> Result<(i64, Vec<u8>), HelperError> {
    let h = open_volume(device, false, true)?;
    let info = query_raw(&h)?;
    if info.journal_id != params.journal_id {
        return Err(HelperError::new(
            ErrorCode::JournalChanged,
            "the USN journal was recreated",
        ));
    }
    if params.from < info.first_usn {
        return Err(HelperError::new(
            ErrorCode::JournalWrapped,
            "the requested USN is no longer in the journal",
        ));
    }
    let request = READ_USN_JOURNAL_DATA_V1 {
        StartUsn: params.from,
        ReasonMask: u32::MAX,
        ReturnOnlyOnClose: 0,
        // NOTE: the kernel's own Timeout is left at 0 (no timeout); the
        // helper bounds the wait itself and cancels the I/O, which behaves
        // the same on every Windows build.
        Timeout: 0,
        BytesToWaitFor: u64::from(params.bytes_to_wait_for),
        UsnJournalID: params.journal_id,
        MinMajorVersion: 2,
        MaxMajorVersion: 4,
    };
    let len = params.max_bytes.clamp(MIN_READ_BYTES, MAX_READ_BYTES);
    let mut out = vec![0u8; len as usize];
    let timeout = params.timeout.min(MAX_WAIT);
    // SAFETY: unnamed manual-reset event; owned below.
    let ev = unsafe { CreateEventW(None, true, false, None) }
        .map_err(|e| win_error("CreateEventW", &e))?;
    // SAFETY: fresh handle.
    let ev = unsafe { OwnedHandle::from_raw(ev) }
        .ok_or_else(|| HelperError::internal("CreateEventW returned no handle"))?;
    let mut ov = OVERLAPPED {
        hEvent: ev.raw(),
        ..Default::default()
    };
    // SAFETY: `request`, `out` and `ov` outlive the operation: every path
    // below waits for completion (GetOverlappedResult with bWait = TRUE after
    // a cancel) before they go out of scope.
    let issued = unsafe {
        DeviceIoControl(
            h.raw(),
            FSCTL_READ_USN_JOURNAL,
            Some((&raw const request).cast::<c_void>()),
            std::mem::size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
            Some(out.as_mut_ptr().cast::<c_void>()),
            len,
            None,
            Some(&mut ov),
        )
    };
    match issued {
        Ok(()) => {}
        Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
            let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
            // SAFETY: both handles are live events.
            let w = unsafe { WaitForMultipleObjects(&[ev.raw(), cancel.event()], false, ms) };
            if w != WAIT_OBJECT_0 {
                if w != WAIT_TIMEOUT && w.0 != WAIT_OBJECT_0.0 + 1 {
                    // NOTE: still cancel and reap below before reporting.
                    crate::diag::diag!("USN wait returned {:#x}", w.0);
                }
                // SAFETY: cancels only our own operation on our handle.
                unsafe {
                    let _ = CancelIoEx(h.raw(), Some(&ov));
                }
            }
        }
        Err(e) => return Err(win_error("FSCTL_READ_USN_JOURNAL", &e)),
    }
    let mut got = 0u32;
    // SAFETY: waits for (or collects) the operation issued above; after
    // this the kernel no longer touches `out`, `request` or `ov`.
    let done = unsafe { GetOverlappedResult(h.raw(), &ov, &mut got, true) };
    match done {
        Ok(()) => split_output(&out[..(got as usize).min(out.len())], params.from),
        Err(e) => {
            let hr = e.code().0 as u32;
            if hr == 0x8007_0000 | ERROR_OPERATION_ABORTED {
                Ok((params.from, Vec::new()))
            } else {
                Err(win_error("FSCTL_READ_USN_JOURNAL", &e))
            }
        }
    }
}

/// Splits the kernel's buffer into the next USN and the record bytes.
fn split_output(buf: &[u8], from: i64) -> Result<(i64, Vec<u8>), HelperError> {
    if buf.len() < 8 {
        return Ok((from, Vec::new()));
    }
    let mut next = [0u8; 8];
    next.copy_from_slice(&buf[..8]);
    Ok((i64::from_le_bytes(next), buf[8..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_splits_next_usn() {
        let mut b = 77i64.to_le_bytes().to_vec();
        b.extend_from_slice(&[1, 2, 3]);
        assert_eq!(split_output(&b, 5).unwrap(), (77, vec![1, 2, 3]));
        assert_eq!(split_output(&[1, 2], 5).unwrap(), (5, vec![]));
    }

    #[test]
    fn journal_errors_are_typed() {
        let e = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(1181));
        assert_eq!(win_error("x", &e).code, ErrorCode::JournalWrapped);
        let e = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(1179));
        assert_eq!(win_error("x", &e).code, ErrorCode::JournalNotActive);
        let e = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(1178));
        assert_eq!(win_error("x", &e).code, ErrorCode::JournalChanged);
        let e = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(5));
        assert_eq!(win_error("x", &e).code, ErrorCode::AccessDenied);
    }

    #[test]
    fn unelevated_query_is_access_denied() {
        if strata_win::process::is_elevated().unwrap_or(false) {
            return;
        }
        let e = query_journal(r"\\.\C:").unwrap_err();
        assert_eq!(e.code, ErrorCode::AccessDenied, "{e}");
    }
}
