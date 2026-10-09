//! Recycle Bin deletes and restore.
//!
//! **Delete.** Each item is opened and fully guard-checked, then verified
//! against the scan. Its ancestors are held open without `FILE_SHARE_DELETE`
//! so no ancestor can be renamed or swapped for a junction while the Shell
//! works, and the Shell is given the handle-resolved path (no links in it).
//! The batch runs through `IFileOperation` on a dedicated STA thread. A
//! progress sink records each item's result and the Recycle Bin location it
//! landed in, and aborts any item the Shell would destroy instead of
//! recycling. Afterwards the item's own handle must resolve inside
//! `$Recycle.Bin`; if something else was recycled in its place, that thing
//! is moved straight back and the item is reported as changed.
//!
//! **Restore.** We parse the `$I` metadata file and move the `$R` payload
//! back ourselves instead of invoking the Shell's "undelete" verb: the `$I`
//! format is documented and stable (version 2 since Windows 10), the move is
//! a same-volume rename with no UI, conflicts are detected rather than
//! prompted, and it works the same in tests as in the app. The opaque
//! [`RestoreTicket`] carries everything needed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use strata_core::FileTime;

use crate::canon::CanonicalPath;
use crate::error::{Change, CleanError, win32_code};
use crate::expect::{CancelToken, Expected};
use crate::guard::{FileIdentity, SafetyGuard};
use crate::volume::{RecycleBinSupport, volume_info};
use crate::win::handle::{
    self, ACCESS_READ_ATTRIBUTES, Follow, OwnedHandle, SHARE_ALL, SHARE_NO_DELETE, VOLUME_NAME_DOS,
};
use crate::win::shell;

/// Version of the [`RestoreTicket`] blob layout.
pub const RESTORE_TICKET_VERSION: u32 = 1;

/// Everything needed to restore one recycled item. Store it as an opaque
/// blob ([`RestoreTicket::to_blob`]); its layout is versioned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreTicket {
    /// Blob layout version.
    pub version: u32,
    /// Where the item was.
    pub original_path: String,
    /// The `$R...` payload in the Recycle Bin.
    pub recycled_path: String,
    /// The `$I...` metadata file next to it.
    pub info_path: String,
    /// Deletion time recorded by Windows.
    pub deleted_at: FileTime,
    /// Size recorded by Windows.
    pub size: u64,
    /// Identity of the recycled object (survives the rename).
    pub identity: Option<FileIdentity>,
}

impl RestoreTicket {
    /// Serializes for the undo log.
    #[must_use]
    pub fn to_blob(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Parses a blob from the undo log.
    ///
    /// # Errors
    ///
    /// [`RestoreError::BadTicket`] for unknown versions or corrupt data.
    pub fn from_blob(blob: &[u8]) -> Result<Self, RestoreError> {
        let t: Self = serde_json::from_slice(blob).map_err(|_| RestoreError::BadTicket)?;
        if t.version != RESTORE_TICKET_VERSION {
            return Err(RestoreError::BadTicket);
        }
        Ok(t)
    }
}

/// Parsed `$I` metadata file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoRecord {
    /// Format version (1: Vista-8.1, 2: Windows 10+).
    pub version: u64,
    /// Original size in bytes.
    pub size: u64,
    /// Deletion time.
    pub deleted_at: FileTime,
    /// Original full path.
    pub original_path: String,
}

/// Parses a `$I` file. Never panics; returns `None` for anything malformed.
///
/// Layout: `u64 version, u64 size, FILETIME deleted`, then for version 1 a
/// fixed 260-unit path, for version 2 a `u32` length in units (including the
/// NUL) followed by the path.
#[must_use]
pub fn parse_info_file(b: &[u8]) -> Option<InfoRecord> {
    let u64_at = |o: usize| {
        b.get(o..o + 8)
            .map(|s| u64::from_le_bytes(s.try_into().unwrap_or([0; 8])))
    };
    let version = u64_at(0)?;
    let size = u64_at(8)?;
    let deleted_at = FileTime(u64_at(16)?);
    let units: &[u8] = match version {
        1 => b.get(24..24 + 520)?,
        2 => {
            let len = u32::from_le_bytes(b.get(24..28)?.try_into().ok()?) as usize;
            if len == 0 || len > 32 * 1024 {
                return None;
            }
            b.get(28..28 + len * 2)?
        }
        _ => return None,
    };
    let wide: Vec<u16> = units
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    if wide.is_empty() {
        return None;
    }
    Some(InfoRecord {
        version,
        size,
        deleted_at,
        original_path: String::from_utf16_lossy(&wide),
    })
}

/// Why a restore failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RestoreError {
    /// The ticket blob is corrupt or from an unknown version.
    #[error("the undo record is unreadable")]
    BadTicket,
    /// The item is no longer in the Recycle Bin (emptied or restored).
    #[error("the item is no longer in the Recycle Bin")]
    NotInRecycleBin,
    /// The Recycle Bin entry now describes a different item.
    #[error("the Recycle Bin entry no longer matches this item")]
    Mismatch,
    /// Something already exists at the original location; never overwritten.
    #[error("something already exists at {path}")]
    DestinationExists {
        /// Original path.
        path: String,
    },
    /// OS failure.
    #[error("restore failed: {message} (error {code})")]
    Os {
        /// Win32 error code.
        code: i32,
        /// System message.
        message: String,
    },
}

fn os_err(e: &std::io::Error) -> RestoreError {
    RestoreError::Os {
        code: e.raw_os_error().map_or(-1, win32_code),
        message: e.to_string(),
    }
}

/// Moves a recycled item back to where it was.
///
/// Missing parent folders are recreated. An existing item at the original
/// path is never overwritten.
///
/// # Errors
///
/// See [`RestoreError`].
pub fn restore(ticket: &RestoreTicket) -> Result<PathBuf, RestoreError> {
    let info_bytes = std::fs::read(&ticket.info_path).map_err(|_| RestoreError::NotInRecycleBin)?;
    let info = parse_info_file(&info_bytes).ok_or(RestoreError::Mismatch)?;
    let same_path = match (
        CanonicalPath::parse(&info.original_path),
        CanonicalPath::parse(&ticket.original_path),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same_path || info.deleted_at != ticket.deleted_at {
        return Err(RestoreError::Mismatch);
    }
    let recycled =
        CanonicalPath::parse(&ticket.recycled_path).map_err(|_| RestoreError::Mismatch)?;
    let h = handle::open(
        &recycled.to_verbatim_wide(),
        ACCESS_READ_ATTRIBUTES,
        SHARE_ALL,
        Follow::NoFollow,
    )
    .map_err(|_| RestoreError::NotInRecycleBin)?;
    if let Some(id) = ticket.identity {
        let i = handle::info(&h).map_err(|e| os_err(&e))?;
        if (i.volume_serial, i.file_id) != (id.volume_serial, id.file_id) {
            return Err(RestoreError::Mismatch);
        }
    }
    drop(h);
    let dest = CanonicalPath::parse(&info.original_path).map_err(|_| RestoreError::Mismatch)?;
    let dest_path = PathBuf::from(dest.to_string());
    if std::fs::symlink_metadata(&dest_path).is_ok() {
        return Err(RestoreError::DestinationExists {
            path: dest_path.display().to_string(),
        });
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(PathBuf::from(parent.to_string())).map_err(|e| os_err(&e))?;
    }
    crate::win::fileops::move_no_replace(&recycled.to_verbatim_wide(), &dest.to_verbatim_wide())
        .map_err(|e| match e.raw_os_error().map(win32_code) {
            Some(80 | 183) => RestoreError::DestinationExists {
                path: dest_path.display().to_string(),
            },
            _ => os_err(&e),
        })?;
    // The payload is back; a leftover `$I` would show a ghost entry in the
    // Recycle Bin, but the restore itself succeeded.
    let _ = std::fs::remove_file(&ticket.info_path);
    Ok(dest_path)
}

/// The `$I` file that belongs to a `$R` payload path.
#[must_use]
pub fn info_path_for(recycled: &str) -> Option<String> {
    let p = Path::new(recycled);
    let name = p.file_name()?.to_str()?;
    let rest = name
        .strip_prefix("$R")
        .or_else(|| name.strip_prefix("$r"))?;
    Some(p.with_file_name(format!("$I{rest}")).display().to_string())
}

/// Finds Recycle Bin entries of the current user whose original path is
/// `original`, newest first. Used when the Shell did not report the
/// recycled item, and to recover tickets after a crash.
///
/// # Errors
///
/// Fails when the volume's Recycle Bin folder cannot be read.
pub fn find_in_recycle_bin(original: &Path) -> std::io::Result<Vec<RestoreTicket>> {
    let want = CanonicalPath::parse(original)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    let vi = volume_info(&want)?;
    let sid = crate::win::process::current_user_sid()
        .ok_or_else(|| std::io::Error::other("cannot read the current user's SID"))?;
    let bin = PathBuf::from(&vi.mount_point)
        .join("$Recycle.Bin")
        .join(sid);
    let mut out = Vec::new();
    for e in std::fs::read_dir(&bin)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("$I") {
            continue;
        }
        let Ok(bytes) = std::fs::read(e.path()) else {
            continue;
        };
        let Some(info) = parse_info_file(&bytes) else {
            continue;
        };
        if CanonicalPath::parse(&info.original_path).ok().as_ref() != Some(&want) {
            continue;
        }
        let recycled = e.path().with_file_name(format!("$R{}", &name[2..]));
        if std::fs::symlink_metadata(&recycled).is_err() {
            continue;
        }
        let identity = identity_of(&recycled);
        out.push(RestoreTicket {
            version: RESTORE_TICKET_VERSION,
            original_path: want.to_string(),
            recycled_path: recycled.display().to_string(),
            info_path: e.path().display().to_string(),
            deleted_at: info.deleted_at,
            size: info.size,
            identity,
        });
    }
    out.sort_by_key(|t| std::cmp::Reverse(t.deleted_at));
    Ok(out)
}

fn identity_of(p: &Path) -> Option<FileIdentity> {
    let c = CanonicalPath::parse(p).ok()?;
    let h = handle::open(
        &c.to_verbatim_wide(),
        ACCESS_READ_ATTRIBUTES,
        SHARE_ALL,
        Follow::NoFollow,
    )
    .ok()?;
    let i = handle::info(&h).ok()?;
    Some(FileIdentity {
        volume_serial: i.volume_serial,
        file_id: i.file_id,
        file_index: i.file_index,
    })
}

/// One item to recycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecycleItem {
    /// Path from the queue.
    pub path: PathBuf,
    /// What the scan saw.
    pub expected: Expected,
}

struct Prepared {
    index: usize,
    display: String,
    resolved: CanonicalPath,
    item_handle: OwnedHandle,
    _pins: Vec<OwnedHandle>,
    identity: FileIdentity,
}

fn pin_ancestors(resolved: &CanonicalPath) -> Vec<OwnedHandle> {
    let mut pins = Vec::new();
    let mut a = resolved.parent();
    while let Some(p) = a {
        if p.is_root() {
            break;
        }
        if let Ok(h) = handle::open(
            &p.to_verbatim_wide(),
            ACCESS_READ_ATTRIBUTES,
            SHARE_NO_DELETE,
            Follow::NoFollow,
        ) {
            pins.push(h);
        }
        a = p.parent();
    }
    pins
}

fn prepare(guard: &SafetyGuard, index: usize, item: &RecycleItem) -> Result<Prepared, CleanError> {
    let display = item.path.display().to_string();
    let (h, checked) = guard.open_checked(&item.path, ACCESS_READ_ATTRIBUTES, SHARE_ALL)?;
    item.expected
        .verify(&checked.facts)
        .map_err(|change| CleanError::Changed {
            path: display.clone(),
            change,
        })?;
    if !fits_shell_path_limit(&checked.resolved) {
        return Err(CleanError::RecycleBinUnavailable {
            path: display,
            reason: crate::volume::RecycleUnavailable::PathTooLong,
        });
    }
    let vi = volume_info(&checked.resolved).map_err(|e| CleanError::from_io(&display, &e))?;
    match &vi.recycle_bin {
        RecycleBinSupport::Unavailable { reason } => {
            return Err(CleanError::RecycleBinUnavailable {
                path: display,
                reason: *reason,
            });
        }
        RecycleBinSupport::Available {
            capacity: Some(capacity),
            ..
        } if item.expected.size > *capacity => {
            return Err(CleanError::TooLargeForRecycleBin {
                path: display,
                size: item.expected.size,
                capacity: *capacity,
            });
        }
        RecycleBinSupport::Available { .. } => {}
    }
    let pins = pin_ancestors(&checked.resolved);
    Ok(Prepared {
        index,
        display,
        resolved: checked.resolved,
        item_handle: h,
        _pins: pins,
        identity: checked.facts.identity,
    })
}

/// The Shell cannot recycle items whose path reaches `MAX_PATH` (260 units
/// including the NUL); it deletes them permanently instead.
#[must_use]
pub fn fits_shell_path_limit(p: &CanonicalPath) -> bool {
    p.to_wide().len() < 260
}

fn is_in_recycle_bin(p: &CanonicalPath) -> bool {
    p.components().first().is_some_and(|c| c.is("$RECYCLE.BIN"))
}

/// Recycles a batch. Results are index-aligned with `items`.
///
/// Items that fail a check never reach the Shell. Items the Shell did not
/// get to (because an earlier item failed and the operation stopped early)
/// are retried in a follow-up operation, so one locked file does not block
/// the rest.
#[must_use]
pub fn recycle(
    guard: &SafetyGuard,
    items: &[RecycleItem],
    cancel: &CancelToken,
) -> Vec<Result<RestoreTicket, CleanError>> {
    let mut results: Vec<Option<Result<RestoreTicket, CleanError>>> = vec![None; items.len()];
    let mut pending: Vec<usize> = (0..items.len()).collect();
    // Each round settles at least one item, so this terminates.
    for _round in 0..=items.len() {
        if pending.is_empty() {
            break;
        }
        if cancel.is_cancelled() {
            for &i in &pending {
                results[i] = Some(Err(CleanError::Cancelled {
                    path: items[i].path.display().to_string(),
                }));
            }
            break;
        }
        let mut prepared = Vec::new();
        for &i in &pending {
            match prepare(guard, i, &items[i]) {
                Ok(p) => prepared.push(p),
                Err(e) => results[i] = Some(Err(e)),
            }
        }
        if prepared.is_empty() {
            break;
        }
        let paths: Vec<String> = prepared.iter().map(|p| p.resolved.to_string()).collect();
        let token = cancel.clone();
        let shell_results = std::thread::spawn(move || shell::recycle_paths_on_sta(&paths, &token))
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("recycle thread panicked")));
        let shell_results = match shell_results {
            Ok(r) => r,
            Err(e) => {
                for p in &prepared {
                    results[p.index] = Some(Err(CleanError::from_io(&p.display, &e)));
                }
                break;
            }
        };
        let mut next = Vec::new();
        let mut settled = 0usize;
        for (p, r) in prepared.into_iter().zip(shell_results) {
            if let Some((i, outcome)) = settle(p, &r, cancel) {
                results[i] = Some(outcome);
                settled += 1;
            }
        }
        // Items with no result yet were not attempted.
        for &i in &pending {
            if results[i].is_none() {
                next.push(i);
            }
        }
        if settled == 0 {
            for &i in &next {
                results[i] = Some(Err(CleanError::Os {
                    path: items[i].path.display().to_string(),
                    code: -1,
                    message: "the Recycle Bin operation did not run".into(),
                }));
            }
            break;
        }
        pending = next;
    }
    results
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            r.unwrap_or_else(|| {
                Err(CleanError::Cancelled {
                    path: items[i].path.display().to_string(),
                })
            })
        })
        .collect()
}

/// Turns one Shell result into an outcome, or `None` when not attempted.
fn settle(
    p: Prepared,
    r: &shell::ShellItemResult,
    cancel: &CancelToken,
) -> Option<(usize, Result<RestoreTicket, CleanError>)> {
    if r.refused_permanent {
        return Some((
            p.index,
            Err(CleanError::WouldDeletePermanently { path: p.display }),
        ));
    }
    let hr = r.hr?;
    if hr < 0 {
        let code = win32_code(hr);
        // E_ABORT for items cut short by an earlier failure or cancel.
        if hr == 0x8000_4004_u32 as i32 {
            if cancel.is_cancelled() {
                return Some((p.index, Err(CleanError::Cancelled { path: p.display })));
            }
            return None;
        }
        let err = CleanError::from_io(&p.display, &std::io::Error::from_raw_os_error(code));
        return Some((p.index, Err(err)));
    }
    // SECURITY: the object we verified must be the one now in the bin.
    let now = handle::final_path(&p.item_handle, VOLUME_NAME_DOS)
        .ok()
        .and_then(|w| CanonicalPath::parse_wide(&w).ok());
    let moved = now.as_ref().is_some_and(is_in_recycle_bin);
    let recycled_path = r.recycled_path.clone().or_else(|| {
        now.as_ref()
            .filter(|n| is_in_recycle_bin(n))
            .map(ToString::to_string)
    });
    if !moved {
        // Something else was recycled in its place; put it back.
        if let Some(rp) = &recycled_path
            && let Some(ip) = info_path_for(rp)
            && let Ok(bytes) = std::fs::read(&ip)
            && let Some(info) = parse_info_file(&bytes)
        {
            let _ = restore(&RestoreTicket {
                version: RESTORE_TICKET_VERSION,
                original_path: info.original_path,
                recycled_path: rp.clone(),
                info_path: ip,
                deleted_at: info.deleted_at,
                size: info.size,
                identity: None,
            });
        }
        return Some((
            p.index,
            Err(CleanError::Changed {
                path: p.display,
                change: Change::Identity {
                    expected: strata_core::FileRef(p.identity.file_index),
                    found: 0,
                },
            }),
        ));
    }
    let ticket = recycled_path
        .and_then(|rp| {
            let ip = info_path_for(&rp)?;
            let info = parse_info_file(&std::fs::read(&ip).ok()?)?;
            Some(RestoreTicket {
                version: RESTORE_TICKET_VERSION,
                original_path: info.original_path,
                recycled_path: rp,
                info_path: ip,
                deleted_at: info.deleted_at,
                size: info.size,
                identity: Some(p.identity),
            })
        })
        .or_else(|| {
            find_in_recycle_bin(Path::new(&p.resolved.to_string()))
                .ok()?
                .into_iter()
                .find(|t| t.identity.is_some_and(|i| i.file_id == p.identity.file_id))
        });
    Some((
        p.index,
        ticket.ok_or_else(|| CleanError::Os {
            path: p.display.clone(),
            code: -1,
            message: "recycled, but its Recycle Bin entry could not be found for Restore".into(),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2(path: &str, size: u64, ft: u64) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(2u64.to_le_bytes());
        b.extend(size.to_le_bytes());
        b.extend(ft.to_le_bytes());
        let w: Vec<u16> = path.encode_utf16().chain([0]).collect();
        b.extend((w.len() as u32).to_le_bytes());
        for u in w {
            b.extend(u.to_le_bytes());
        }
        b
    }

    #[test]
    fn parses_version_2() {
        let r = parse_info_file(&v2(r"D:\x\a.txt", 42, 7)).unwrap();
        assert_eq!(r.version, 2);
        assert_eq!(r.size, 42);
        assert_eq!(r.deleted_at, FileTime(7));
        assert_eq!(r.original_path, r"D:\x\a.txt");
    }

    #[test]
    fn parses_version_1() {
        let mut b = Vec::new();
        b.extend(1u64.to_le_bytes());
        b.extend(5u64.to_le_bytes());
        b.extend(9u64.to_le_bytes());
        let mut path = [0u16; 260];
        for (i, u) in r"C:\old.txt".encode_utf16().enumerate() {
            path[i] = u;
        }
        for u in path {
            b.extend(u.to_le_bytes());
        }
        let r = parse_info_file(&b).unwrap();
        assert_eq!(
            (r.version, r.size, r.original_path.as_str()),
            (1, 5, r"C:\old.txt")
        );
    }

    #[test]
    fn malformed_info_files_are_rejected() {
        assert!(parse_info_file(&[]).is_none());
        assert!(parse_info_file(&[2, 0, 0]).is_none());
        let mut b = v2(r"C:\a", 1, 1);
        b[0] = 3;
        assert!(parse_info_file(&b).is_none());
        let mut b = v2(r"C:\a", 1, 1);
        b.truncate(30);
        assert!(parse_info_file(&b).is_none());
        let mut b = v2(r"C:\a", 1, 1);
        b[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_info_file(&b).is_none());
        let mut b = v2("", 1, 1);
        b[24..28].copy_from_slice(&1u32.to_le_bytes());
        assert!(parse_info_file(&b).is_none());
    }

    fn unit_dir(tag: &str) -> PathBuf {
        let base = if Path::new(r"D:\").exists() {
            PathBuf::from(r"D:\strata-clean-tests")
        } else {
            std::env::temp_dir().join("strata-clean-tests")
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let d = base.join(format!("unit-{tag}-{}-{nanos:x}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Bypasses pre-flight to prove the sink itself stops a permanent
    /// delete the Shell chose on its own, and that a normal recycle carries
    /// the recycle flag the check relies on.
    #[test]
    fn sink_stops_shell_permanent_delete() {
        let d = unit_dir("sink");
        let mut long = d.clone();
        for _ in 0..28 {
            long.push("abcdefghij");
        }
        std::fs::create_dir_all(&long).unwrap();
        let lf = long.join("nuke-me-not.txt");
        std::fs::write(&lf, b"keep").unwrap();
        let normal = d.join("normal.txt");
        std::fs::write(&normal, b"n").unwrap();

        let paths = vec![lf.display().to_string()];
        let r =
            std::thread::spawn(move || shell::recycle_paths_on_sta(&paths, &CancelToken::new()))
                .join()
                .unwrap()
                .unwrap();
        assert!(r[0].refused_permanent, "{:?}", r[0]);
        assert_eq!(std::fs::read(&lf).unwrap(), b"keep");

        let paths = vec![normal.display().to_string()];
        let r =
            std::thread::spawn(move || shell::recycle_paths_on_sta(&paths, &CancelToken::new()))
                .join()
                .unwrap()
                .unwrap();
        let flags = r[0].pre_flags.unwrap();
        assert_ne!(
            flags & 0x80,
            0,
            "TSF_DELETE_RECYCLE_IF_POSSIBLE missing: {flags:#x}"
        );
        let rp = r[0].recycled_path.clone().expect("recycled path reported");
        assert!(rp.to_uppercase().contains(r"\$RECYCLE.BIN\"));
        // Purge exactly the entry this test created.
        std::fs::remove_file(&rp).unwrap();
        std::fs::remove_file(info_path_for(&rp).unwrap()).unwrap();

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn info_path_mapping() {
        assert_eq!(
            info_path_for(r"C:\$Recycle.Bin\S-1\$RABC12.txt").as_deref(),
            Some(r"C:\$Recycle.Bin\S-1\$IABC12.txt")
        );
        assert!(info_path_for(r"C:\x\file.txt").is_none());
    }

    #[test]
    fn ticket_blob_round_trip() {
        let t = RestoreTicket {
            version: RESTORE_TICKET_VERSION,
            original_path: r"D:\a".into(),
            recycled_path: r"D:\$Recycle.Bin\S\$R1".into(),
            info_path: r"D:\$Recycle.Bin\S\$I1".into(),
            deleted_at: FileTime(3),
            size: 4,
            identity: None,
        };
        assert_eq!(RestoreTicket::from_blob(&t.to_blob()).unwrap(), t);
        assert_eq!(
            RestoreTicket::from_blob(b"junk"),
            Err(RestoreError::BadTicket)
        );
        let mut v = t;
        v.version = 99;
        assert_eq!(
            RestoreTicket::from_blob(&v.to_blob()),
            Err(RestoreError::BadTicket)
        );
    }
}
