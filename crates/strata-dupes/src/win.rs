//! Thin Win32 layer: opening files without recalling cloud content, reading
//! handle facts, and a rename that never replaces. Every `unsafe` block of
//! the crate lives here.
//!
//! # Why opening a file here cannot trigger a download
//!
//! Cloud placeholders (OneDrive Files On-Demand and any other Cloud Files
//! provider) are hydrated by the Cloud Files minifilter in two situations:
//! when a handle is opened on a file marked `RECALL_ON_OPEN`, and when data
//! is read from a range that is not on disk (`RECALL_ON_DATA_ACCESS`). Legacy
//! HSM products recall `OFFLINE` files the same ways. This module:
//!
//! 1. Never opens a candidate the index already reports as cloud, offline or
//!    a non-content reparse point (checked by the caller before any open).
//! 2. Opens every file with `FILE_FLAG_OPEN_NO_RECALL`, which tells the
//!    filter that the data must stay remote, so an open is never a recall
//!    trigger; and with `FILE_FLAG_OPEN_REPARSE_POINT`, so a symlink or
//!    junction swapped in since the scan is opened itself, not followed.
//! 3. Re-reads the attributes and reparse tag **from the open handle** and
//!    drops the file on any recall, offline, pinned/unpinned bit or a cloud
//!    tag before the first byte is requested. Only plain local files (and
//!    WOF/dedup files, whose data is on this volume) ever see a read.
//!
//! Reads therefore only ever reach files whose data is local; a placeholder
//! that slipped past the index is caught by step 3, and the no-recall open
//! means even that open did not start a download.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use strata_core::FileTime;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_TAG_INFO, FileAttributeTagInfo,
    GetFileInformationByHandle, GetFileInformationByHandleEx, MOVE_FILE_FLAGS, MoveFileExW,
};
use windows::core::PCWSTR;

const FILE_READ_DATA: u32 = 0x0001;
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const SYNCHRONIZE: u32 = 0x0010_0000;
const SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
const SHARE_READ_DELETE: u32 = 0x1 | 0x4;
const FILE_FLAG_OPEN_NO_RECALL: u32 = 0x0010_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
const ATTR_REPARSE: u32 = strata_core::win32::FILE_ATTRIBUTE_REPARSE_POINT;

/// How a file is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// Attributes only; no data access is requested.
    Metadata,
    /// Sequential data reads; other processes may still read, write and
    /// delete, so hashing never gets in anyone's way.
    Read,
    /// Data reads while denying writers, so the content cannot change while
    /// a hardlink is created from it.
    ReadDenyWrite,
}

/// Facts read from an open handle. Never reads content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Facts {
    /// 64-bit file index (the NTFS file reference).
    pub index: u64,
    /// 32-bit volume serial number.
    pub volume_serial: u32,
    /// Win32 attributes.
    pub attributes: u32,
    /// Reparse tag, 0 when not a reparse point.
    pub reparse_tag: u32,
    /// Logical size of the unnamed stream.
    pub size: u64,
    /// Last-write time.
    pub mtime: FileTime,
    /// Hardlink count.
    pub links: u32,
}

/// Opens `path` without following links and without recalling content.
pub(crate) fn open(path: &Path, access: Access) -> io::Result<File> {
    let (rights, share, extra) = match access {
        Access::Metadata => (FILE_READ_ATTRIBUTES | SYNCHRONIZE, SHARE_ALL, 0),
        Access::Read => (
            FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            SHARE_ALL,
            FILE_FLAG_SEQUENTIAL_SCAN,
        ),
        Access::ReadDenyWrite => (
            FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            SHARE_READ_DELETE,
            FILE_FLAG_SEQUENTIAL_SCAN,
        ),
    };
    OpenOptions::new()
        .access_mode(rights)
        .share_mode(share)
        .custom_flags(FILE_FLAG_OPEN_NO_RECALL | FILE_FLAG_OPEN_REPARSE_POINT | extra)
        .open(path)
}

/// Reads [`Facts`] from an open handle.
pub(crate) fn facts(file: &File) -> io::Result<Facts> {
    let h = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `h` is a live handle owned by `file` for the duration of the
    // call, and `info` is a properly sized, writable out-parameter.
    unsafe { GetFileInformationByHandle(h, &raw mut info) }.map_err(win_err)?;
    let mut tag = 0;
    if info.dwFileAttributes & ATTR_REPARSE != 0 {
        let mut t = FILE_ATTRIBUTE_TAG_INFO::default();
        // SAFETY: as above; the buffer is a FILE_ATTRIBUTE_TAG_INFO and its
        // exact size is passed.
        unsafe {
            GetFileInformationByHandleEx(
                h,
                FileAttributeTagInfo,
                (&raw mut t).cast(),
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        }
        .map_err(win_err)?;
        tag = t.ReparseTag;
    }
    let ft = info.ftLastWriteTime;
    Ok(Facts {
        index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        volume_serial: info.dwVolumeSerialNumber,
        attributes: info.dwFileAttributes,
        reparse_tag: tag,
        size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        mtime: FileTime((u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)),
        links: info.nNumberOfLinks,
    })
}

/// Renames `from` to `to`, failing if `to` exists (`std::fs::rename`
/// replaces on Windows, which would destroy whatever took the name).
pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let f = wide(from);
    let t = wide(to);
    // SAFETY: both buffers are NUL-terminated UTF-16 strings that outlive
    // the call; flags 0 means "fail if the target exists, same volume only".
    unsafe { MoveFileExW(PCWSTR(f.as_ptr()), PCWSTR(t.as_ptr()), MOVE_FILE_FLAGS(0)) }
        .map_err(win_err)
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn win_err(e: windows::core::Error) -> io::Error {
    // HRESULT_FROM_WIN32 keeps the Win32 code in the low 16 bits.
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((hr & 0xFFFF) as i32)
    } else {
        io::Error::other(e)
    }
}
