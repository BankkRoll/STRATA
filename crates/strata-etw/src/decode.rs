//! Typed decoding of the Kernel-File and Kernel-Process events Strata uses.
//!
//! Per-event decoding walks the payload with a [`Layout`] instead of calling
//! TDH (`TdhGetEventInformation` + `TdhGetProperty`) for every event. TDH
//! re-resolves the schema and copies each property on every call; the
//! hand walk reads a handful of fixed offsets with no allocation for writes,
//! which are the bulk of the stream. TDH is still the source of truth for the
//! layouts: [`crate::tdh::load_layouts`] reads them from the installed
//! manifest, and the built-in tables below are checked against it in tests.
//!
//! Layouts transcribed from the manifests (Windows 11; all `Pointer` fields
//! are 4 or 8 bytes per the event header):
//!
//! | Event | v | Fields used |
//! |---|---|---|
//! | Kernel-File 10 NameCreate, 11 NameDelete | 0 | `FileKey`, `FileName` |
//! | Kernel-File 12 Create, 30 CreateNewFile | 0, 1 | `FileObject`, `CreateOptions`, `FileName` |
//! | Kernel-File 16 Write | 0, 1 | `FileObject`, `FileKey`, `IOSize`, `IOFlags` |
//! | Kernel-File 26 DeletePath, 27 RenamePath | 0, 1 | `FileObject`, `FileKey`, `ExtraInformation`, `InfoClass`, `FilePath` |
//! | Kernel-Process 1 ProcessStart | 0-4 | `ProcessID`, `CreateTime`, `ImageName` (NT path) |
//! | Kernel-Process 2 ProcessStop | 0-2 | `ProcessID`, `CreateTime` |

use serde::{Deserialize, Serialize};
use strata_core::FileTime;

use crate::layout::{FieldType as T, Layout, LayoutTable, Provider};

/// Kernel-File event ids.
pub mod file_id {
    /// A file name was associated with a file key.
    pub const NAME_CREATE: u16 = 10;
    /// A file key's name was released.
    pub const NAME_DELETE: u16 = 11;
    /// `IRP_MJ_CREATE` (open or create).
    pub const CREATE: u16 = 12;
    /// `IRP_MJ_WRITE`.
    pub const WRITE: u16 = 16;
    /// Delete disposition set, with the file path.
    pub const DELETE_PATH: u16 = 26;
    /// Rename, with a file path.
    pub const RENAME_PATH: u16 = 27;
    /// A create that made a new file.
    pub const CREATE_NEW_FILE: u16 = 30;
}

/// Kernel-Process event ids.
pub mod process_id {
    /// Process start.
    pub const START: u16 = 1;
    /// Process stop.
    pub const STOP: u16 = 2;
}

/// Every (provider, event id) this crate decodes.
pub const DECODED_EVENTS: &[(Provider, u16)] = &[
    (Provider::KernelFile, file_id::NAME_CREATE),
    (Provider::KernelFile, file_id::NAME_DELETE),
    (Provider::KernelFile, file_id::CREATE),
    (Provider::KernelFile, file_id::WRITE),
    (Provider::KernelFile, file_id::DELETE_PATH),
    (Provider::KernelFile, file_id::RENAME_PATH),
    (Provider::KernelFile, file_id::CREATE_NEW_FILE),
    (Provider::KernelProcess, process_id::START),
    (Provider::KernelProcess, process_id::STOP),
];

/// Highest event version probed when loading layouts from TDH.
pub const MAX_PROBED_VERSION: u8 = 8;

/// The built-in layouts (see the module table).
#[must_use]
pub fn builtin_layouts() -> LayoutTable {
    use Provider::{KernelFile as F, KernelProcess as P};
    let mut t = LayoutTable::default();
    let name = Layout::of(&[("FileKey", T::Pointer), ("FileName", T::UnicodeString)]);
    t.insert(F, file_id::NAME_CREATE, 0, name.clone());
    t.insert(F, file_id::NAME_DELETE, 0, name);
    let create0 = Layout::of(&[
        ("Irp", T::Pointer),
        ("ThreadId", T::Pointer),
        ("FileObject", T::Pointer),
        ("CreateOptions", T::Int32),
        ("CreateAttributes", T::Int32),
        ("ShareAccess", T::Int32),
        ("FileName", T::UnicodeString),
    ]);
    let create1 = Layout::of(&[
        ("Irp", T::Pointer),
        ("FileObject", T::Pointer),
        ("IssuingThreadId", T::Int32),
        ("CreateOptions", T::Int32),
        ("CreateAttributes", T::Int32),
        ("ShareAccess", T::Int32),
        ("FileName", T::UnicodeString),
    ]);
    for id in [file_id::CREATE, file_id::CREATE_NEW_FILE] {
        t.insert(F, id, 0, create0.clone());
        t.insert(F, id, 1, create1.clone());
    }
    t.insert(
        F,
        file_id::WRITE,
        0,
        Layout::of(&[
            ("ByteOffset", T::Int64),
            ("Irp", T::Pointer),
            ("ThreadId", T::Pointer),
            ("FileObject", T::Pointer),
            ("FileKey", T::Pointer),
            ("IOSize", T::Int32),
            ("IOFlags", T::Int32),
        ]),
    );
    t.insert(
        F,
        file_id::WRITE,
        1,
        Layout::of(&[
            ("ByteOffset", T::Int64),
            ("Irp", T::Pointer),
            ("FileObject", T::Pointer),
            ("FileKey", T::Pointer),
            ("IssuingThreadId", T::Int32),
            ("IOSize", T::Int32),
            ("IOFlags", T::Int32),
            ("ExtraFlags", T::Int32),
        ]),
    );
    let path0 = Layout::of(&[
        ("Irp", T::Pointer),
        ("ThreadId", T::Pointer),
        ("FileObject", T::Pointer),
        ("FileKey", T::Pointer),
        ("ExtraInformation", T::Pointer),
        ("InfoClass", T::Int32),
        ("FilePath", T::UnicodeString),
    ]);
    let path1 = Layout::of(&[
        ("Irp", T::Pointer),
        ("FileObject", T::Pointer),
        ("FileKey", T::Pointer),
        ("ExtraInformation", T::Pointer),
        ("IssuingThreadId", T::Int32),
        ("InfoClass", T::Int32),
        ("FilePath", T::UnicodeString),
    ]);
    for id in [file_id::DELETE_PATH, file_id::RENAME_PATH] {
        t.insert(F, id, 0, path0.clone());
        t.insert(F, id, 1, path1.clone());
    }

    let start_head = [
        ("ProcessID", T::Int32),
        ("CreateTime", T::FileTime),
        ("ParentProcessID", T::Int32),
        ("SessionID", T::Int32),
    ];
    let tail2 = [
        ("ImageChecksum", T::Int32),
        ("TimeDateStamp", T::Int32),
        ("PackageFullName", T::UnicodeString),
        ("PackageRelativeAppId", T::UnicodeString),
    ];
    let mut v0 = start_head.to_vec();
    v0.push(("ImageName", T::UnicodeString));
    let mut v1 = start_head.to_vec();
    v1.extend([("Flags", T::Int32), ("ImageName", T::UnicodeString)]);
    let mut v2 = v1.clone();
    v2.extend(tail2);
    let mut v3 = vec![
        ("ProcessID", T::Int32),
        ("ProcessSequenceNumber", T::Int64),
        ("CreateTime", T::FileTime),
        ("ParentProcessID", T::Int32),
        ("ParentProcessSequenceNumber", T::Int64),
        ("SessionID", T::Int32),
        ("Flags", T::Int32),
        ("ProcessTokenElevationType", T::Int32),
        ("ProcessTokenIsElevated", T::Int32),
        ("MandatoryLabel", T::Sid),
        ("ImageName", T::UnicodeString),
    ];
    v3.extend(tail2);
    let mut v4 = v3.clone();
    v4.push(("SecurityMitigations", T::Int32));
    for (v, l) in [(0, v0), (1, v1), (2, v2), (3, v3), (4, v4)] {
        t.insert(P, process_id::START, v, Layout::of(&l));
    }

    let stop_mid = [
        ("CreateTime", T::FileTime),
        ("ExitTime", T::FileTime),
        ("ExitCode", T::Int32),
        ("TokenElevationType", T::Int32),
        ("HandleCount", T::Int32),
        ("CommitCharge", T::Int64),
        ("CommitPeak", T::Int64),
    ];
    let io = [
        ("CPUCycleCount", T::Int64),
        ("ReadOperationCount", T::Int32),
        ("WriteOperationCount", T::Int32),
        ("ReadTransferKiloBytes", T::Int32),
        ("WriteTransferKiloBytes", T::Int32),
        ("HardFaultCount", T::Int32),
    ];
    let mut s0 = vec![("ProcessID", T::Int32)];
    s0.extend(stop_mid);
    let mut s1 = s0.clone();
    s0.push(("ImageName", T::AnsiString));
    s1.extend(io);
    s1.push(("ImageName", T::AnsiString));
    let mut s2 = vec![("ProcessID", T::Int32), ("ProcessSequenceNumber", T::Int64)];
    s2.extend(stop_mid);
    s2.extend(io);
    s2.push(("ImageName", T::AnsiString));
    for (v, l) in [(0, s0), (1, s1), (2, s2)] {
        t.insert(P, process_id::STOP, v, Layout::of(&l));
    }
    t
}

/// The parts of an `EVENT_RECORD` the decoder needs, borrowed from the
/// event buffer.
#[derive(Debug, Clone, Copy)]
pub struct RawEvent<'a> {
    /// Which provider logged it.
    pub provider: Provider,
    /// `EventDescriptor.Id`.
    pub id: u16,
    /// `EventDescriptor.Version`.
    pub version: u8,
    /// `EventHeader.ProcessId`: the process the I/O was issued in.
    pub pid: u32,
    /// `EventHeader.TimeStamp` as a FILETIME (the session uses system time).
    pub timestamp: FileTime,
    /// 4 or 8, from `EVENT_HEADER_FLAG_32_BIT_HEADER` / `_64_BIT_HEADER`.
    pub pointer_size: usize,
    /// `UserData`.
    pub data: &'a [u8],
}

/// A decoded event of interest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    /// `FileKey` now names `name` (NT path).
    NameCreate {
        /// File key (per-stream context pointer).
        key: u64,
        /// NT path, UTF-16.
        name: Vec<u16>,
    },
    /// `FileKey` no longer names `name`.
    NameDelete {
        /// File key.
        key: u64,
        /// NT path, UTF-16.
        name: Vec<u16>,
    },
    /// A file object was opened on `name`.
    Create {
        /// File object pointer.
        file_object: u64,
        /// `CreateOptions` (disposition in the top byte).
        options: u32,
        /// NT path, UTF-16.
        name: Vec<u16>,
    },
    /// A create that made a new file.
    CreateNewFile {
        /// File object pointer.
        file_object: u64,
        /// NT path, UTF-16.
        name: Vec<u16>,
    },
    /// A write request.
    Write {
        /// File object pointer.
        file_object: u64,
        /// File key.
        key: u64,
        /// Bytes requested.
        size: u32,
        /// IRP flags (`IRP_PAGING_IO` and friends).
        io_flags: u32,
    },
    /// Delete disposition set on a file.
    DeletePath {
        /// File object pointer.
        file_object: u64,
        /// File key.
        key: u64,
        /// `FILE_INFORMATION_CLASS` (13 disposition, 64 disposition-ex).
        info_class: u32,
        /// Disposition value (`DeleteFile` flag or `FILE_DISPOSITION_*`).
        extra: u64,
        /// Path, UTF-16 (NT path).
        path: Vec<u16>,
    },
    /// A rename; `path` is one of the two names (see [`crate::files`]).
    RenamePath {
        /// File object pointer.
        file_object: u64,
        /// File key.
        key: u64,
        /// Path, UTF-16 (NT path).
        path: Vec<u16>,
    },
    /// A process started.
    ProcessStart {
        /// Process id.
        pid: u32,
        /// Creation time.
        create_time: FileTime,
        /// Image as an NT path, UTF-16.
        image: Vec<u16>,
    },
    /// A process exited.
    ProcessStop {
        /// Process id.
        pid: u32,
        /// Creation time (identifies which process with this id).
        create_time: FileTime,
    },
}

/// A decoded event with its header fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Issuing process id from the header.
    pub pid: u32,
    /// When it was logged.
    pub timestamp: FileTime,
    /// What happened.
    pub kind: EventKind,
}

/// Why an event could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// No layout for this event version.
    #[error("no layout for {provider:?} event {id} version {version}")]
    UnknownLayout {
        /// Provider.
        provider: Provider,
        /// Event id.
        id: u16,
        /// Event version.
        version: u8,
    },
    /// A field the decoder needs is missing or truncated.
    #[error("{provider:?} event {id}: field `{field}` missing or truncated")]
    MissingField {
        /// Provider.
        provider: Provider,
        /// Event id.
        id: u16,
        /// Field name.
        field: &'static str,
    },
}

/// Decodes events with a layout table.
#[derive(Debug, Clone)]
pub struct Decoder {
    table: LayoutTable,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new(builtin_layouts())
    }
}

impl Decoder {
    /// A decoder over `table`.
    #[must_use]
    pub const fn new(table: LayoutTable) -> Self {
        Self { table }
    }

    /// Built-in layouts plus any extra versions found in the installed
    /// manifests. Manifest layouts replace built-ins of the same version.
    /// Returns the TDH errors, if any (decoding still works from the
    /// built-ins).
    #[must_use]
    pub fn with_installed_manifests() -> (Self, Vec<String>) {
        let mut table = builtin_layouts();
        let (installed, errors) = crate::tdh::load_layouts(DECODED_EVENTS, MAX_PROBED_VERSION);
        for (p, id, v) in installed.keys().collect::<Vec<_>>() {
            if let Some(l) = installed.get(p, id, v) {
                table.insert(p, id, v, l.clone());
            }
        }
        (Self::new(table), errors)
    }

    /// The layout table in use.
    #[must_use]
    pub const fn layouts(&self) -> &LayoutTable {
        &self.table
    }

    /// Whether `(provider, id)` is an event this decoder handles. Cheap; the
    /// consumer checks it before anything else.
    #[must_use]
    pub fn wants(provider: Provider, id: u16) -> bool {
        DECODED_EVENTS.contains(&(provider, id))
    }

    /// Decodes one event. `Ok(None)` for events this crate ignores, and for
    /// delete-disposition events that clear the flag (undelete).
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the version is unknown or a needed field is
    /// missing.
    pub fn decode(&self, raw: &RawEvent<'_>) -> Result<Option<Event>, DecodeError> {
        if !Self::wants(raw.provider, raw.id) {
            return Ok(None);
        }
        let layout = self.table.get(raw.provider, raw.id, raw.version).ok_or(
            DecodeError::UnknownLayout {
                provider: raw.provider,
                id: raw.id,
                version: raw.version,
            },
        )?;
        let f = layout.read(raw.data, raw.pointer_size);
        let missing = |field| DecodeError::MissingField {
            provider: raw.provider,
            id: raw.id,
            field,
        };
        let u64f = |n: &'static str| f.u64(n).ok_or_else(|| missing(n));
        let u32f = |n: &'static str| f.u32(n).ok_or_else(|| missing(n));
        let strf = |n: &'static str| f.utf16(n).ok_or_else(|| missing(n));
        use file_id as fi;
        let kind = match (raw.provider, raw.id) {
            (Provider::KernelFile, fi::NAME_CREATE) => EventKind::NameCreate {
                key: u64f("FileKey")?,
                name: strf("FileName")?,
            },
            (Provider::KernelFile, fi::NAME_DELETE) => EventKind::NameDelete {
                key: u64f("FileKey")?,
                name: strf("FileName")?,
            },
            (Provider::KernelFile, fi::CREATE) => EventKind::Create {
                file_object: u64f("FileObject")?,
                options: u32f("CreateOptions")?,
                name: strf("FileName")?,
            },
            (Provider::KernelFile, fi::CREATE_NEW_FILE) => EventKind::CreateNewFile {
                file_object: u64f("FileObject")?,
                name: strf("FileName")?,
            },
            (Provider::KernelFile, fi::WRITE) => EventKind::Write {
                file_object: u64f("FileObject")?,
                key: u64f("FileKey")?,
                size: u32f("IOSize")?,
                io_flags: u32f("IOFlags")?,
            },
            (Provider::KernelFile, fi::DELETE_PATH) => {
                let info_class = u32f("InfoClass")?;
                let extra = u64f("ExtraInformation")?;
                if !is_delete_disposition(info_class, extra) {
                    return Ok(None);
                }
                EventKind::DeletePath {
                    file_object: u64f("FileObject")?,
                    key: u64f("FileKey")?,
                    info_class,
                    extra,
                    path: strf("FilePath")?,
                }
            }
            (Provider::KernelFile, fi::RENAME_PATH) => EventKind::RenamePath {
                file_object: u64f("FileObject")?,
                key: u64f("FileKey")?,
                path: strf("FilePath")?,
            },
            (Provider::KernelProcess, process_id::START) => EventKind::ProcessStart {
                pid: u32f("ProcessID")?,
                create_time: FileTime(u64f("CreateTime")?),
                image: strf("ImageName")?,
            },
            (Provider::KernelProcess, process_id::STOP) => EventKind::ProcessStop {
                pid: u32f("ProcessID")?,
                create_time: FileTime(u64f("CreateTime")?),
            },
            _ => return Ok(None),
        };
        Ok(Some(Event {
            pid: raw.pid,
            timestamp: raw.timestamp,
            kind,
        }))
    }
}

/// `FileDispositionInformation` (13): `ExtraInformation` is the `DeleteFile`
/// BOOLEAN. `FileDispositionInformationEx` (64): `FILE_DISPOSITION_DELETE`
/// (bit 0) set means delete, clear means undelete. Other classes are treated
/// as deletes because the provider only logs this event for delete paths.
#[must_use]
pub const fn is_delete_disposition(info_class: u32, extra: u64) -> bool {
    match info_class {
        13 => extra & 0xFF != 0,
        64 => extra & 1 != 0,
        _ => true,
    }
}

/// `FILE_DELETE_ON_CLOSE` in `CreateOptions`.
pub const FILE_DELETE_ON_CLOSE: u32 = 0x0000_1000;

/// `IRP_PAGING_IO`: a cache-manager or memory-manager write, issued by the
/// system on behalf of an earlier cached write.
pub const IRP_PAGING_IO: u32 = 0x0000_0002;

/// `IRP_SYNCHRONOUS_PAGING_IO`.
pub const IRP_SYNCHRONOUS_PAGING_IO: u32 = 0x0000_0040;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table_covers_every_decoded_event() {
        let t = builtin_layouts();
        for &(p, id) in DECODED_EVENTS {
            assert!(t.get(p, id, 0).is_some(), "{p:?} {id}");
        }
        assert_eq!(
            t.get(Provider::KernelProcess, 1, 4).unwrap().fields.len(),
            16
        );
        assert_eq!(
            t.get(Provider::KernelProcess, 2, 2).unwrap().fields.len(),
            16
        );
    }

    #[test]
    fn disposition_rules() {
        assert!(is_delete_disposition(13, 1));
        assert!(!is_delete_disposition(13, 0));
        assert!(is_delete_disposition(64, 0x13));
        assert!(!is_delete_disposition(64, 0x10));
    }
}
