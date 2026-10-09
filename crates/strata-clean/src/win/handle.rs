//! File handles: open, identify, resolve, delete.

use std::io;
use std::mem::{size_of, zeroed};

use strata_core::FileTime;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER,
    ERROR_NOT_SUPPORTED, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_TAG_INFO,
    FILE_BASIC_INFO, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_NO_RECALL, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FileAttributeTagInfo, FileBasicInfo,
    FileDispositionInfo, FileDispositionInfoEx, FileIdInfo, GETFINALPATHNAMEBYHANDLE_FLAGS,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    GetLongPathNameW, OPEN_EXISTING, ReOpenFile, SetFileInformationByHandle,
};
use windows::core::PCWSTR;

pub(crate) use windows::Win32::Storage::FileSystem::{
    VOLUME_NAME_DOS, VOLUME_NAME_GUID, VOLUME_NAME_NONE,
};

/// `DELETE` standard access right.
pub(crate) const ACCESS_DELETE: u32 = 0x0001_0000;
/// `SYNCHRONIZE` standard access right.
pub(crate) const ACCESS_SYNCHRONIZE: u32 = 0x0010_0000;
/// `FILE_READ_ATTRIBUTES`.
pub(crate) const ACCESS_READ_ATTRIBUTES: u32 = 0x0080;
/// `FILE_LIST_DIRECTORY` (same bit as `FILE_READ_DATA`).
pub(crate) const ACCESS_LIST_DIRECTORY: u32 = 0x0001;

/// Share mode that never blocks other openers.
pub(crate) const SHARE_ALL: FILE_SHARE_MODE =
    FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0);
/// Share mode that stops others renaming or deleting the object while held.
pub(crate) const SHARE_NO_DELETE: FILE_SHARE_MODE =
    FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0);

/// An owned Win32 handle, closed on drop.
#[derive(Debug)]
pub(crate) struct OwnedHandle(HANDLE);

impl OwnedHandle {
    /// Takes ownership of a raw handle.
    ///
    /// # Safety
    ///
    /// `h` must be a valid handle that nothing else will close.
    pub(crate) unsafe fn from_raw(h: HANDLE) -> Self {
        Self(h)
    }

    pub(crate) fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// SAFETY: kernel handles are process-wide and usable from any thread.
unsafe impl Send for OwnedHandle {}

/// How [`open`] treats a reparse point at the final component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Follow {
    /// Open the link itself (`FILE_FLAG_OPEN_REPARSE_POINT`).
    NoFollow,
    /// Open whatever the link points to.
    Follow,
}

/// Opens an existing file or directory.
///
/// `path` must be NUL-terminated (normally a verbatim path). Directories are
/// opened with `FILE_FLAG_BACKUP_SEMANTICS`; cloud placeholders are never
/// recalled.
pub(crate) fn open(
    path: &[u16],
    access: u32,
    share: FILE_SHARE_MODE,
    follow: Follow,
) -> io::Result<OwnedHandle> {
    debug_assert_eq!(path.last(), Some(&0));
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_NO_RECALL.0;
    if follow == Follow::NoFollow {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT.0;
    }
    // SAFETY: `path` is NUL-terminated and outlives the call.
    let h = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            access,
            share,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(flags),
            None,
        )
    }?;
    // SAFETY: CreateFileW succeeded, so `h` is a fresh handle we own.
    Ok(unsafe { OwnedHandle::from_raw(h) })
}

/// Opens a file by id on the volume `hint` belongs to, never following a
/// reparse point and never recalling cloud content.
pub(crate) fn open_by_id(
    hint: &OwnedHandle,
    file_id: u128,
    access: u32,
    share: FILE_SHARE_MODE,
) -> io::Result<OwnedHandle> {
    use windows::Win32::Storage::FileSystem::{
        ExtendedFileIdType, FILE_ID_128, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FileIdType,
        OpenFileById,
    };
    let desc = if let Ok(small) = i64::try_from(file_id) {
        FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: small },
        }
    } else {
        FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: ExtendedFileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 {
                ExtendedFileId: FILE_ID_128 {
                    Identifier: file_id.to_le_bytes(),
                },
            },
        }
    };
    // SAFETY: `desc` is fully initialized and outlives the call.
    let h = unsafe {
        OpenFileById(
            hint.raw(),
            &desc,
            access,
            share,
            None,
            FILE_FLAGS_AND_ATTRIBUTES(
                FILE_FLAG_BACKUP_SEMANTICS.0
                    | FILE_FLAG_OPEN_REPARSE_POINT.0
                    | FILE_FLAG_OPEN_NO_RECALL.0,
            ),
        )
    }?;
    // SAFETY: OpenFileById succeeded, so `h` is a fresh handle we own.
    Ok(unsafe { OwnedHandle::from_raw(h) })
}

/// A second handle to the same object with different access and sharing.
/// No path lookup happens, so the object cannot have been swapped.
pub(crate) fn reopen(
    h: &OwnedHandle,
    access: u32,
    share: FILE_SHARE_MODE,
) -> io::Result<OwnedHandle> {
    // NOTE: not ReOpenFile: it fails on directory handles with access denied
    // and rejects FILE_FLAG_OPEN_NO_RECALL (both seen on Windows 11 26200).
    // An NtCreateFile relative to the handle with an empty name reopens the
    // same object with the options we need.
    super::ntdir::open_relative(h, &[], access, share)
}

/// Result of [`final_path`]: UTF-16 units without the NUL.
pub(crate) fn final_path(
    h: &OwnedHandle,
    flags: GETFINALPATHNAMEBYHANDLE_FLAGS,
) -> io::Result<Vec<u16>> {
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: the buffer slice is valid for its length.
        let n = unsafe { GetFinalPathNameByHandleW(h.raw(), &mut buf, flags) } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(buf);
        }
        // `n` is the required size including the NUL.
        buf.resize(n + 1, 0);
    }
}

/// Identity and metadata of an open handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HandleInfo {
    pub attributes: u32,
    pub reparse_tag: u32,
    pub size: u64,
    pub modified: FileTime,
    pub links: u32,
    pub volume_serial: u64,
    pub file_id: u128,
    pub file_index: u64,
}

impl HandleInfo {
    pub(crate) fn is_reparse(&self) -> bool {
        self.attributes & strata_core::win32::FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
}

/// Queries identity, size, times and attributes. Never reads file data.
pub(crate) fn info(h: &OwnedHandle) -> io::Result<HandleInfo> {
    // SAFETY: plain-old-data out parameter.
    let mut bhfi: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    // SAFETY: `bhfi` is a valid out pointer.
    unsafe { GetFileInformationByHandle(h.raw(), &mut bhfi) }?;
    let file_index = (u64::from(bhfi.nFileIndexHigh) << 32) | u64::from(bhfi.nFileIndexLow);
    let modified = FileTime(
        (u64::from(bhfi.ftLastWriteTime.dwHighDateTime) << 32)
            | u64::from(bhfi.ftLastWriteTime.dwLowDateTime),
    );

    // SAFETY: plain-old-data out parameter.
    let mut id: FILE_ID_INFO = unsafe { zeroed() };
    // SAFETY: `id` is valid for `size_of::<FILE_ID_INFO>()` bytes.
    let (volume_serial, file_id) = match unsafe {
        GetFileInformationByHandleEx(
            h.raw(),
            FileIdInfo,
            (&raw mut id).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } {
        Ok(()) => (
            id.VolumeSerialNumber,
            u128::from_le_bytes(id.FileId.Identifier),
        ),
        // FAT and some network redirectors have no 128-bit ids.
        Err(_) => (u64::from(bhfi.dwVolumeSerialNumber), u128::from(file_index)),
    };

    // SAFETY: plain-old-data out parameter.
    let mut tag: FILE_ATTRIBUTE_TAG_INFO = unsafe { zeroed() };
    // SAFETY: `tag` is valid for its size.
    let reparse_tag = match unsafe {
        GetFileInformationByHandleEx(
            h.raw(),
            FileAttributeTagInfo,
            (&raw mut tag).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } {
        Ok(()) => tag.ReparseTag,
        Err(_) => 0,
    };

    Ok(HandleInfo {
        attributes: bhfi.dwFileAttributes,
        reparse_tag,
        size: (u64::from(bhfi.nFileSizeHigh) << 32) | u64::from(bhfi.nFileSizeLow),
        modified,
        links: bhfi.nNumberOfLinks,
        volume_serial,
        file_id,
        file_index,
    })
}

/// Marks the object behind `h` for deletion.
///
/// Uses POSIX semantics (the name disappears immediately, even if others
/// hold it open with `FILE_SHARE_DELETE`) and ignores the read-only bit.
/// Falls back to classic disposition on file systems or Windows builds
/// without `FileDispositionInfoEx`, clearing read-only first if needed.
pub(crate) fn delete_by_handle(h: &OwnedHandle) -> io::Result<()> {
    let ex = FILE_DISPOSITION_INFO_EX {
        Flags: windows::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO_EX_FLAGS(
            FILE_DISPOSITION_FLAG_DELETE.0
                | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS.0
                | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE.0,
        ),
    };
    // SAFETY: `ex` is valid for its size for the duration of the call.
    let r = unsafe {
        SetFileInformationByHandle(
            h.raw(),
            FileDispositionInfoEx,
            (&raw const ex).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    match r {
        Ok(()) => return Ok(()),
        Err(e)
            if ![
                ERROR_INVALID_PARAMETER.to_hresult(),
                ERROR_NOT_SUPPORTED.to_hresult(),
                ERROR_INVALID_FUNCTION.to_hresult(),
            ]
            .contains(&e.code()) =>
        {
            return Err(e.into());
        }
        Err(_) => {}
    }
    match legacy_dispose(h) {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) => {
            clear_readonly(h)?;
            legacy_dispose(h)
        }
        other => other,
    }
}

fn legacy_dispose(h: &OwnedHandle) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: `info` is valid for its size for the duration of the call.
    unsafe {
        SetFileInformationByHandle(
            h.raw(),
            FileDispositionInfo,
            (&raw const info).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }?;
    Ok(())
}

fn clear_readonly(h: &OwnedHandle) -> io::Result<()> {
    // SAFETY: `h` is valid; ReOpenFile returns a new handle to the same
    // object (no path lookup, so no race).
    let w = unsafe {
        ReOpenFile(
            h.raw(),
            FILE_WRITE_ATTRIBUTES.0,
            SHARE_ALL,
            FILE_FLAGS_AND_ATTRIBUTES(
                FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0,
            ),
        )
    }?;
    // SAFETY: ReOpenFile succeeded, so `w` is a fresh handle we own.
    let w = unsafe { OwnedHandle::from_raw(w) };
    // SAFETY: plain-old-data out parameter.
    let mut basic: FILE_BASIC_INFO = unsafe { zeroed() };
    // SAFETY: `basic` is valid for its size.
    unsafe {
        GetFileInformationByHandleEx(
            w.raw(),
            FileBasicInfo,
            (&raw mut basic).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    }?;
    if basic.FileAttributes & FILE_ATTRIBUTE_READONLY.0 == 0 {
        return Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED.0 as i32));
    }
    basic.FileAttributes &= !FILE_ATTRIBUTE_READONLY.0;
    // Zero timestamps mean "leave unchanged".
    basic.CreationTime = 0;
    basic.LastAccessTime = 0;
    basic.LastWriteTime = 0;
    basic.ChangeTime = 0;
    // SAFETY: `basic` is valid for its size.
    unsafe {
        SetFileInformationByHandle(
            w.raw(),
            FileBasicInfo,
            (&raw const basic).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    }?;
    Ok(())
}

/// Expands 8.3 short names. `path` must be NUL-terminated.
pub(crate) fn long_path_name(path: &[u16]) -> io::Result<Vec<u16>> {
    debug_assert_eq!(path.last(), Some(&0));
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: `path` is NUL-terminated; `buf` is valid for its length.
        let n = unsafe { GetLongPathNameW(PCWSTR(path.as_ptr()), Some(&mut buf)) } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(buf);
        }
        buf.resize(n + 1, 0);
    }
}
