//! The never-list applied to what a path *really* names.
//!
//! Lexical checks cannot see 8.3 aliases, junctions, symlinks, mount points,
//! `subst` drives or mapped drives. The guard therefore checks, and refuses
//! if any of them is protected:
//!
//! 1. the literal request, canonicalized;
//! 2. its long-name expansion (`GetLongPathNameW`) when it contains `~`;
//! 3. the final path of a handle opened on the item itself
//!    (`FILE_FLAG_OPEN_REPARSE_POINT`, so links in the *parent chain* are
//!    resolved but the item is not), in both DOS and volume-GUID form;
//! 4. whether that handle is a volume root (`VOLUME_NAME_NONE` is `\`);
//! 5. the item's file identity against the identities of every protected
//!    location and its ancestors (catches hardlinked or aliased spellings
//!    the path forms miss);
//! 6. for a symlink, junction or mount point, the target it points to.
//!
//! Every action re-runs 3-6 on the handle it is about to delete through, so
//! a path swapped after pre-flight is caught at the last moment.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use strata_core::FileTime;
use strata_core::known::KnownFolders;

use crate::canon::CanonicalPath;
use crate::error::CleanError;
use crate::never::{NeverList, NeverListConfig, ProtectedKind, Refusal, RefusalReason, Relation};
use crate::win::handle::{
    self, ACCESS_READ_ATTRIBUTES, Follow, HandleInfo, OwnedHandle, SHARE_ALL, VOLUME_NAME_DOS,
    VOLUME_NAME_GUID, VOLUME_NAME_NONE,
};

/// Reparse tags with this bit name another file (symlinks, junctions).
const NAME_SURROGATE_BIT: u32 = 0x2000_0000;

/// Stable identity of a file: volume serial plus file id.
///
/// NTFS ids fit in 64 bits (the [`strata_core::FileRef`]); ReFS ids use all
/// 128. `file_index` is the 64-bit index `GetFileInformationByHandle`
/// reports, which is what the fallback walker records on ReFS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileIdentity {
    /// 64-bit volume serial number.
    pub volume_serial: u64,
    /// 128-bit file id.
    pub file_id: u128,
    /// 64-bit file index.
    pub file_index: u64,
}

impl FileIdentity {
    /// Whether this identity is the file `r` names.
    ///
    /// Synthetic walker ids never match: they were not read from the file.
    #[must_use]
    pub fn matches(&self, r: strata_core::FileRef) -> bool {
        if r.is_synthetic() {
            return false;
        }
        if self.file_id <= u128::from(u64::MAX) {
            self.file_id == u128::from(r.0)
        } else {
            self.file_index == r.0
        }
    }
}

/// Facts read from the item's handle. Never reads file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemFacts {
    /// Identity.
    pub identity: FileIdentity,
    /// Win32 attributes.
    pub attributes: u32,
    /// Reparse tag (0 when not a reparse point).
    pub reparse_tag: u32,
    /// Logical size of the unnamed stream.
    pub size: u64,
    /// Last-write time.
    pub modified: FileTime,
    /// Hardlink count.
    pub links: u32,
}

impl ItemFacts {
    /// Whether the item is a directory.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.attributes & strata_core::win32::FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// Whether the item is a reparse point.
    #[must_use]
    pub fn is_reparse(&self) -> bool {
        self.attributes & strata_core::win32::FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    pub(crate) fn from_info(i: &HandleInfo) -> Self {
        Self {
            identity: FileIdentity {
                volume_serial: i.volume_serial,
                file_id: i.file_id,
                file_index: i.file_index,
            },
            attributes: i.attributes,
            reparse_tag: i.reparse_tag,
            size: i.size,
            modified: i.modified,
            links: i.links,
        }
    }
}

/// An item that passed every guard check at the moment it was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedItem {
    /// The request, canonicalized.
    pub literal: CanonicalPath,
    /// The handle-resolved path (DOS form when available).
    pub resolved: CanonicalPath,
    /// Facts from the handle.
    pub facts: ItemFacts,
}

/// Inputs for [`SafetyGuard::new`].
#[derive(Debug, Clone, Default)]
pub struct GuardConfig {
    /// Resolved known folders (from the Windows layer).
    pub known: KnownFolders,
    /// Strata's install folder(s).
    pub install_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct IdRule {
    kind: ProtectedKind,
    relation: Relation,
    display: String,
}

/// The never-list plus file-system resolution. Cheap to share by reference.
#[derive(Debug, Clone)]
pub struct SafetyGuard {
    list: NeverList,
    ids: HashMap<(u64, u128), IdRule>,
}

impl SafetyGuard {
    /// Builds the guard for this machine: resolves volumes and machine names,
    /// compiles the never-list and records the identity of every protected
    /// location.
    ///
    /// # Errors
    ///
    /// Fails when volumes cannot be enumerated or the never-list cannot be
    /// compiled (missing core known folders).
    pub fn new(config: GuardConfig) -> Result<Self, GuardError> {
        let volumes =
            crate::volume::resolve_volume_map().map_err(|e| GuardError::Volumes(e.to_string()))?;
        let list = NeverList::new(NeverListConfig {
            known: config.known,
            install_dirs: config.install_dirs,
            volumes,
            local_hosts: crate::volume::local_host_names(),
        })
        .map_err(|e| GuardError::NeverList(e.to_string()))?;
        Ok(Self::from_never_list(list))
    }

    /// Wraps an already-compiled list and records protected identities.
    #[must_use]
    pub fn from_never_list(list: NeverList) -> Self {
        let mut ids = HashMap::new();
        for (path, kind, _) in list.absolute_locations() {
            let display = path.to_string();
            let wide = path.to_verbatim_wide();
            for follow in [Follow::NoFollow, Follow::Follow] {
                if let Some(i) = identity_of(&wide, follow) {
                    ids.insert(
                        i,
                        IdRule {
                            kind,
                            relation: Relation::Itself,
                            display: display.clone(),
                        },
                    );
                }
            }
            let mut ancestor = path.parent();
            while let Some(a) = ancestor {
                if a.is_root() {
                    break;
                }
                if let Some(i) = identity_of(&a.to_verbatim_wide(), Follow::Follow) {
                    ids.entry(i).or_insert_with(|| IdRule {
                        kind,
                        relation: Relation::Contains,
                        display: display.clone(),
                    });
                }
                ancestor = a.parent();
            }
        }
        Self { list, ids }
    }

    /// The compiled never-list.
    #[must_use]
    pub fn never_list(&self) -> &NeverList {
        &self.list
    }

    /// Runs every check on `path` (see the module docs).
    ///
    /// # Errors
    ///
    /// [`CleanError::Refused`] when any form is protected,
    /// [`CleanError::NotFound`] when the item is gone, or another
    /// [`CleanError`] when it cannot be opened.
    pub fn check_path(&self, path: &Path) -> Result<CheckedItem, CleanError> {
        self.open_checked(path, ACCESS_READ_ATTRIBUTES, SHARE_ALL)
            .map(|(_, item)| item)
    }

    /// Opens `path` itself (never its link target) with `access`, then runs
    /// every check on that handle. The caller acts through the returned
    /// handle, so nothing can be swapped between the check and the action.
    pub(crate) fn open_checked(
        &self,
        path: &Path,
        access: u32,
        share: windows::Win32::Storage::FileSystem::FILE_SHARE_MODE,
    ) -> Result<(OwnedHandle, CheckedItem), CleanError> {
        let display = path.display().to_string();
        let literal = self.list.check_str(path.as_os_str())?;
        if literal
            .components()
            .iter()
            .any(|c| c.orig().contains(&u16::from(b'~')))
            && let Ok(long) = handle::long_path_name(&literal.to_verbatim_wide())
        {
            let long = CanonicalPath::parse_wide(&long)
                .map_err(|e| Refusal::invalid(display.clone(), e))?;
            self.list.check_as(&long, &display)?;
        }
        self.check_parent_chain(&literal, &display)?;
        let probe = handle::open(
            &literal.to_verbatim_wide(),
            ACCESS_READ_ATTRIBUTES,
            SHARE_ALL,
            Follow::NoFollow,
        )
        .map_err(|e| CleanError::from_io(&display, &e))?;
        let (resolved, facts) = self.check_handle(&probe, &display)?;
        let h = if access & !ACCESS_READ_ATTRIBUTES == 0 && share == SHARE_ALL {
            probe
        } else {
            self.upgrade(&probe, access, share, &facts, &display)?
        };
        Ok((
            h,
            CheckedItem {
                literal,
                resolved,
                facts,
            },
        ))
    }

    /// Reopens a checked object with the access an action needs. Checks run
    /// on a read-only handle first, so a protected item is refused rather
    /// than reported as "access denied", and delete access is never held on
    /// anything unchecked.
    pub(crate) fn upgrade(
        &self,
        probe: &OwnedHandle,
        access: u32,
        share: windows::Win32::Storage::FileSystem::FILE_SHARE_MODE,
        facts: &ItemFacts,
        display: &str,
    ) -> Result<OwnedHandle, CleanError> {
        let h = handle::reopen(probe, access | ACCESS_READ_ATTRIBUTES, share)
            .map_err(|e| CleanError::from_io(display, &e))?;
        let again = handle::info(&h).map_err(|e| CleanError::from_io(display, &e))?;
        if (again.volume_serial, again.file_id)
            != (facts.identity.volume_serial, facts.identity.file_id)
        {
            return Err(
                Refusal::unverifiable(display, "the item changed while being opened").into(),
            );
        }
        Ok(h)
    }

    /// Checks 3-6 on an open handle. Used again right before every action.
    pub(crate) fn check_handle(
        &self,
        h: &OwnedHandle,
        display: &str,
    ) -> Result<(CanonicalPath, ItemFacts), CleanError> {
        let info = handle::info(h).map_err(|e| CleanError::from_io(display, &e))?;
        let resolved = self.check_resolved(h, display, info.attributes)?;
        self.check_identity(&info, display)?;

        if info.is_reparse() && info.reparse_tag & NAME_SURROGATE_BIT != 0 {
            self.check_link_target(&resolved, display)?;
        }
        Ok((resolved, ItemFacts::from_info(&info)))
    }

    /// Resolves the parent through any links (handle opened *with*
    /// following) and checks `resolved parent + leaf`. This catches items
    /// that cannot be opened themselves, like an in-use `pagefile.sys`
    /// reached through a junction.
    fn check_parent_chain(&self, literal: &CanonicalPath, display: &str) -> Result<(), CleanError> {
        let (Some(parent), Some(leaf)) = (literal.parent(), literal.file_name()) else {
            return Ok(());
        };
        let Ok(p) = handle::open(
            &parent.to_verbatim_wide(),
            ACCESS_READ_ATTRIBUTES,
            SHARE_ALL,
            Follow::Follow,
        ) else {
            // The item's own handle check still runs; an unopenable parent
            // means the item cannot be opened either.
            return Ok(());
        };
        for flags in [VOLUME_NAME_DOS, VOLUME_NAME_GUID] {
            if let Some(resolved) = handle::final_path(&p, flags)
                .ok()
                .and_then(|w| CanonicalPath::parse_wide(&w).ok())
            {
                self.list.check_as(&resolved.join(leaf.clone()), display)?;
            }
        }
        Ok(())
    }

    fn check_resolved(
        &self,
        h: &OwnedHandle,
        display: &str,
        attributes: u32,
    ) -> Result<CanonicalPath, CleanError> {
        if let Ok(rel) = handle::final_path(h, VOLUME_NAME_NONE)
            && (rel.is_empty() || rel == [u16::from(b'\\')])
        {
            return Err(Refusal::protected(
                display,
                ProtectedKind::VolumeRoot,
                Relation::Itself,
                display,
            )
            .into());
        }
        let parse = |flags| {
            handle::final_path(h, flags)
                .ok()
                .and_then(|w| CanonicalPath::parse_wide(&w).ok())
        };
        let dos = parse(VOLUME_NAME_DOS);
        let guid = parse(VOLUME_NAME_GUID);
        for p in [&dos, &guid].into_iter().flatten() {
            self.list.check_as(p, display)?;
            self.list.check_attributes_as(p, attributes, display)?;
        }
        dos.or(guid).ok_or_else(|| {
            Refusal::unverifiable(display, "Windows could not resolve its final path").into()
        })
    }

    pub(crate) fn check_identity(
        &self,
        info: &HandleInfo,
        display: &str,
    ) -> Result<(), CleanError> {
        if let Some(rule) = self.ids.get(&(info.volume_serial, info.file_id)) {
            return Err(Refusal::protected(
                display,
                rule.kind,
                rule.relation,
                rule.display.clone(),
            )
            .into());
        }
        Ok(())
    }

    fn check_link_target(&self, link: &CanonicalPath, display: &str) -> Result<(), CleanError> {
        let Ok(t) = handle::open(
            &link.to_verbatim_wide(),
            ACCESS_READ_ATTRIBUTES,
            SHARE_ALL,
            Follow::Follow,
        ) else {
            // A dangling link: deleting it cannot touch anything else.
            return Ok(());
        };
        let as_link_target = |e: CleanError| match e {
            CleanError::Refused { mut refusal } => {
                if let RefusalReason::Protected { relation, .. } = &mut refusal.reason {
                    *relation = Relation::LinkTarget;
                }
                CleanError::Refused { refusal }
            }
            other => other,
        };
        let info = handle::info(&t).map_err(|e| CleanError::from_io(display, &e))?;
        self.check_resolved(&t, display, 0)
            .map_err(as_link_target)?;
        self.check_identity(&info, display)
            .map_err(as_link_target)?;
        Ok(())
    }
}

fn identity_of(wide: &[u16], follow: Follow) -> Option<(u64, u128)> {
    let h = handle::open(wide, ACCESS_READ_ATTRIBUTES, SHARE_ALL, follow).ok()?;
    let i = handle::info(&h).ok()?;
    Some((i.volume_serial, i.file_id))
}

/// Errors building a [`SafetyGuard`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardError {
    /// Volume enumeration failed.
    #[error("could not enumerate volumes: {0}")]
    Volumes(String),
    /// The never-list could not be compiled.
    #[error("could not build the never-list: {0}")]
    NeverList(String),
}
