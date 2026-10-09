//! The hard-coded never-delete list. Pure: no file-system access.
//!
//! This list is independent of the classifier's rule packs: even if a rule
//! wrongly marks `C:\Windows` as "safe", these checks refuse it. They run in
//! the app backend and again inside the elevated helper.
//!
//! A path is refused when it **is**, **contains** (is an ancestor of), or for
//! subtree rules **is inside** a protected location. Every spelling of the
//! request is checked: the canonical form, its trailing-dot/space alias, the
//! drive form of administrative shares (`\\host\C$`), and every mount point of
//! a volume GUID or NT device root. When a volume root cannot be mapped to a
//! mount point, rules are matched on the volume-relative components alone, so
//! an unknown root can never smuggle `\Windows` past the list.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use strata_core::known::{KnownFolder, KnownFolders};

use crate::canon::{CanonicalPath, Name, PathError, Root};

/// What kind of protected location a refusal is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProtectedKind {
    /// A volume root (`C:\`, a mounted-folder volume, a share root).
    VolumeRoot,
    /// The Windows directory, outside its allow-listed temp/cache folders.
    WindowsDirectory,
    /// A `Program Files*` root or `Common Files`.
    ProgramFiles,
    /// `Program Files\WindowsApps` (system-managed packages).
    WindowsApps,
    /// The profiles root (`C:\Users`).
    UserProfilesRoot,
    /// A user profile root (`C:\Users\name`).
    UserProfileRoot,
    /// A known folder's root (Documents, Desktop, AppData, ...).
    KnownFolderRoot {
        /// Which folder.
        folder: KnownFolder,
    },
    /// `AppData\LocalLow` (no `KnownFolder` variant yet).
    AppDataRoot,
    /// `System Volume Information` (shadow copies, indexer, restore points).
    SystemVolumeInformation,
    /// The Recycle Bin store (`$Recycle.Bin`); empty it through the Shell.
    RecycleBinStore,
    /// NTFS metadata files (`$MFT`, `$Extend`, ...).
    NtfsMetadata,
    /// Paging, hibernation or swap file.
    SystemFile,
    /// Boot manager files and folders (`Boot`, `EFI`, `bootmgr`, `Recovery`).
    BootFiles,
    /// A top-level item with both the system and hidden attributes.
    TopLevelSystemHidden,
    /// Strata's own install folder.
    StrataInstall,
}

impl ProtectedKind {
    /// Short human-readable description for the UI.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::VolumeRoot => "the root of a drive or share".into(),
            Self::WindowsDirectory => "part of Windows".into(),
            Self::ProgramFiles => "a Program Files folder".into(),
            Self::WindowsApps => "a system-managed app package folder".into(),
            Self::UserProfilesRoot => "the folder that holds every user profile".into(),
            Self::UserProfileRoot => "a user profile folder".into(),
            Self::KnownFolderRoot { folder } => {
                format!("a Windows known folder ({})", known_folder_label(folder))
            }
            Self::AppDataRoot => "an application data root folder".into(),
            Self::SystemVolumeInformation => "System Volume Information (restore points)".into(),
            Self::RecycleBinStore => "the Recycle Bin store (use Empty Recycle Bin)".into(),
            Self::NtfsMetadata => "NTFS file-system metadata".into(),
            Self::SystemFile => "a Windows paging, swap or hibernation file".into(),
            Self::BootFiles => "needed to start Windows".into(),
            Self::TopLevelSystemHidden => "a hidden system item at the top of the drive".into(),
            Self::StrataInstall => "Strata's own install folder".into(),
        }
    }
}

fn known_folder_label(f: KnownFolder) -> &'static str {
    match f {
        KnownFolder::UserProfile => "user profile",
        KnownFolder::LocalAppData => "AppData\\Local",
        KnownFolder::LocalAppDataLow => "AppData\\LocalLow",
        KnownFolder::SavedGames => "Saved Games",
        KnownFolder::AppData => "AppData\\Roaming",
        KnownFolder::Temp => "Temp",
        KnownFolder::Downloads => "Downloads",
        KnownFolder::Documents => "Documents",
        KnownFolder::Desktop => "Desktop",
        KnownFolder::Pictures => "Pictures",
        KnownFolder::Music => "Music",
        KnownFolder::Videos => "Videos",
        KnownFolder::ProgramData => "ProgramData",
        KnownFolder::Windir => "Windows",
        KnownFolder::ProgramFiles => "Program Files",
        KnownFolder::ProgramFilesX86 => "Program Files (x86)",
        KnownFolder::UserProfiles => "Users",
        KnownFolder::Public => "Public",
    }
}

/// How the requested path relates to the protected location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// The request is the protected item.
    Itself,
    /// The request is inside a protected subtree.
    Inside,
    /// The request contains the protected item.
    Contains,
    /// The request is a link whose target is protected.
    LinkTarget,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RefusalReason {
    /// Matches the never-list.
    Protected {
        /// Kind of protected location.
        protected: ProtectedKind,
        /// How the request relates to it.
        relation: Relation,
        /// Display form of the protected location.
        protected_path: String,
    },
    /// The path is malformed or uses a refused spelling.
    InvalidPath {
        /// The parse failure.
        error: PathError,
    },
    /// A UNC path that points back at this machine; use the local path.
    LoopbackShare,
    /// The path could not be resolved well enough to prove it is safe.
    Unverifiable {
        /// What failed.
        detail: String,
    },
}

/// A refused request: the path and a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{}", self.message())]
pub struct Refusal {
    /// The request as given (or as resolved, for link/handle checks).
    pub path: String,
    /// Why.
    pub reason: RefusalReason,
}

impl Refusal {
    /// Sentence for the UI.
    #[must_use]
    pub fn message(&self) -> String {
        match &self.reason {
            RefusalReason::Protected {
                protected,
                relation,
                protected_path,
            } => {
                let what = protected.describe();
                match relation {
                    Relation::Itself => {
                        format!("{} is {what}. Strata never deletes it.", self.path)
                    }
                    Relation::Inside => format!(
                        "{} is inside {protected_path}, which is {what}. Strata never deletes it.",
                        self.path
                    ),
                    Relation::Contains => format!(
                        "{} contains {protected_path}, which is {what}. Delete items inside it individually.",
                        self.path
                    ),
                    Relation::LinkTarget => format!(
                        "{} is a link to {protected_path}, which is {what}. Strata refuses links into protected locations.",
                        self.path
                    ),
                }
            }
            RefusalReason::InvalidPath { error } => {
                format!("{} cannot be deleted: {error}.", self.path)
            }
            RefusalReason::LoopbackShare => format!(
                "{} is a network path back to this computer. Use the local path instead.",
                self.path
            ),
            RefusalReason::Unverifiable { detail } => format!(
                "{} could not be verified as safe to delete ({detail}).",
                self.path
            ),
        }
    }

    pub(crate) fn protected(
        path: impl Into<String>,
        protected: ProtectedKind,
        relation: Relation,
        protected_path: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            reason: RefusalReason::Protected {
                protected,
                relation,
                protected_path: protected_path.into(),
            },
        }
    }

    pub(crate) fn invalid(path: impl Into<String>, error: PathError) -> Self {
        Self {
            path: path.into(),
            reason: RefusalReason::InvalidPath { error },
        }
    }

    pub(crate) fn unverifiable(path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            reason: RefusalReason::Unverifiable {
                detail: detail.into(),
            },
        }
    }
}

/// One volume and where it is mounted. Plain data, resolved by the caller
/// (see [`crate::volume::resolve_volume_map`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VolumeEntry {
    /// Braced GUID, e.g. `{11111111-1111-4111-8111-111111111111}`.
    pub guid: String,
    /// NT device name without the `\Device\` prefix, e.g. `HarddiskVolume3`.
    pub device: Option<String>,
    /// DOS mount points: drive roots (`C:\`) and mounted folders.
    pub mount_points: Vec<PathBuf>,
}

/// Every volume on the machine and its mount points.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VolumeMap {
    /// Volumes.
    pub volumes: Vec<VolumeEntry>,
}

/// Inputs for [`NeverList::new`].
#[derive(Debug, Clone, Default)]
pub struct NeverListConfig {
    /// Resolved known folders for the machine and every profile.
    pub known: KnownFolders,
    /// Strata's install folder(s).
    pub install_dirs: Vec<PathBuf>,
    /// Volume GUID / device / mount point map.
    pub volumes: VolumeMap,
    /// Names and addresses of this machine, for loopback UNC detection.
    /// `localhost`, `127.*` and `::1` are always included.
    pub local_hosts: Vec<String>,
}

/// Errors building the never-list.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NeverListError {
    /// A folder the list cannot work without was not resolved.
    #[error("known folder {0:?} is not resolved")]
    MissingKnownFolder(KnownFolder),
    /// A configured path could not be canonicalized.
    #[error("configured path {path} is invalid: {error}")]
    InvalidConfiguredPath {
        /// The path.
        path: String,
        /// Why.
        error: PathError,
    },
}

#[derive(Debug, Clone)]
enum Anchor {
    /// A fixed absolute path.
    Absolute(CanonicalPath),
    /// Relative to the root of whatever volume the request is on.
    RootRelative(Vec<Name>),
    /// Every direct child of a fixed directory.
    ChildOf(CanonicalPath),
    /// Every top-level item whose folded name starts with this prefix.
    RootChildPrefix(Vec<u16>),
}

#[derive(Debug, Clone)]
struct Allow {
    rel: Vec<Name>,
    contents_only: bool,
}

#[derive(Debug, Clone)]
enum Scope {
    /// The item itself (and, like every rule, its ancestors).
    Exact,
    /// The item and everything inside it, minus the allow-list.
    Subtree(Vec<Allow>),
}

#[derive(Debug, Clone)]
struct Rule {
    anchor: Anchor,
    scope: Scope,
    kind: ProtectedKind,
}

/// One entry of the never-list in documentation form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeverListEntry {
    /// Display form of the location (`<volume>\` prefix for per-volume rules).
    pub location: String,
    /// Whether everything inside is protected too.
    pub subtree: bool,
    /// Kind.
    pub kind: ProtectedKind,
    /// Allow-listed exceptions inside a subtree.
    pub exceptions: Vec<String>,
}

/// Paths under `{WINDIR}` that cleanup rules may delete. Each
/// entry is `(relative path, contents_only)`; the folders themselves stay
/// protected.
const WINDIR_ALLOW: &[(&str, bool)] = &[
    ("Temp", true),
    ("SoftwareDistribution\\Download", true),
    ("SoftwareDistribution\\DeliveryOptimization", true),
    (
        "ServiceProfiles\\NetworkService\\AppData\\Local\\Microsoft\\Windows\\DeliveryOptimization\\Cache",
        true,
    ),
    ("MEMORY.DMP", false),
    ("Minidump", true),
    ("LiveKernelReports", true),
    ("Logs\\CBS", true),
    ("Logs\\DISM", true),
    ("Prefetch", true),
    (
        "ServiceProfiles\\LocalService\\AppData\\Local\\FontCache",
        true,
    ),
];

/// Per-volume items whose whole subtree is protected.
const ROOT_SUBTREES: &[(&str, ProtectedKind)] = &[
    (
        "System Volume Information",
        ProtectedKind::SystemVolumeInformation,
    ),
    ("$Recycle.Bin", ProtectedKind::RecycleBinStore),
    ("RECYCLER", ProtectedKind::RecycleBinStore),
    ("RECYCLED", ProtectedKind::RecycleBinStore),
    ("$MFT", ProtectedKind::NtfsMetadata),
    ("$MFTMirr", ProtectedKind::NtfsMetadata),
    ("$LogFile", ProtectedKind::NtfsMetadata),
    ("$Volume", ProtectedKind::NtfsMetadata),
    ("$AttrDef", ProtectedKind::NtfsMetadata),
    ("$Bitmap", ProtectedKind::NtfsMetadata),
    ("$Boot", ProtectedKind::NtfsMetadata),
    ("$BadClus", ProtectedKind::NtfsMetadata),
    ("$Secure", ProtectedKind::NtfsMetadata),
    ("$UpCase", ProtectedKind::NtfsMetadata),
    ("$Extend", ProtectedKind::NtfsMetadata),
    ("Boot", ProtectedKind::BootFiles),
    ("EFI", ProtectedKind::BootFiles),
    ("Recovery", ProtectedKind::BootFiles),
];

/// Per-volume items protected themselves (not their contents).
const ROOT_EXACT: &[(&str, ProtectedKind)] = &[
    ("pagefile.sys", ProtectedKind::SystemFile),
    ("hiberfil.sys", ProtectedKind::SystemFile),
    ("swapfile.sys", ProtectedKind::SystemFile),
    ("bootmgr", ProtectedKind::BootFiles),
    ("BOOTNXT", ProtectedKind::BootFiles),
    ("BOOTSECT.BAK", ProtectedKind::BootFiles),
    ("boot.ini", ProtectedKind::BootFiles),
    ("ntldr", ProtectedKind::BootFiles),
    ("Documents and Settings", ProtectedKind::UserProfilesRoot),
];

/// The compiled never-list.
///
/// # Example
///
/// ```
/// use strata_clean::canon::CanonicalPath;
/// use strata_clean::never::{NeverList, NeverListConfig};
/// use strata_core::known::{KnownFolder, KnownFolders};
///
/// let mut known = KnownFolders::default();
/// known.machine.insert(KnownFolder::Windir, r"C:\Windows".into());
/// known.machine.insert(KnownFolder::ProgramFiles, r"C:\Program Files".into());
/// known.machine.insert(KnownFolder::UserProfiles, r"C:\Users".into());
/// let list = NeverList::new(NeverListConfig { known, ..Default::default() }).unwrap();
///
/// let sys = CanonicalPath::parse(r"c:/windows/system32/").unwrap();
/// assert!(list.check(&sys).is_err());
/// let tmp = CanonicalPath::parse(r"C:\Windows\Temp\setup.log").unwrap();
/// assert!(list.check(&tmp).is_ok());
/// ```
#[derive(Debug, Clone)]
pub struct NeverList {
    rules: Vec<Rule>,
    windir: CanonicalPath,
    volume_mounts: Vec<(Name, Option<Name>, Vec<CanonicalPath>)>,
    local_hosts: Vec<Box<[u16]>>,
}

fn names(rel: &str) -> Vec<Name> {
    rel.split('\\').map(Name::from_str_name).collect()
}

fn canon_config(p: &Path) -> Result<CanonicalPath, NeverListError> {
    CanonicalPath::parse(p).map_err(|error| NeverListError::InvalidConfiguredPath {
        path: p.display().to_string(),
        error,
    })
}

impl NeverList {
    /// Compiles the list.
    ///
    /// # Errors
    ///
    /// Fails when `{WINDIR}`, `{PROGRAMFILES}` or `{USERPROFILES}` is not
    /// resolved (the list would be incomplete), or a configured path is not
    /// fully qualified.
    pub fn new(config: NeverListConfig) -> Result<Self, NeverListError> {
        let mut rules = Vec::new();
        let machine = |f: KnownFolder| config.known.machine.get(&f);

        for (name, kind) in ROOT_SUBTREES {
            rules.push(Rule {
                anchor: Anchor::RootRelative(names(name)),
                scope: Scope::Subtree(Vec::new()),
                kind: *kind,
            });
        }
        for (name, kind) in ROOT_EXACT {
            rules.push(Rule {
                anchor: Anchor::RootRelative(names(name)),
                scope: Scope::Exact,
                kind: *kind,
            });
        }
        // Covers `Program Files (Arm)` and installs on secondary drives,
        // which have no known-folder token.
        rules.push(Rule {
            anchor: Anchor::RootChildPrefix(Name::from_str_name("PROGRAM FILES").folded().to_vec()),
            scope: Scope::Exact,
            kind: ProtectedKind::ProgramFiles,
        });

        let windir = machine(KnownFolder::Windir)
            .ok_or(NeverListError::MissingKnownFolder(KnownFolder::Windir))?;
        let windir = canon_config(windir)?;
        rules.push(Rule {
            anchor: Anchor::Absolute(windir.clone()),
            scope: Scope::Subtree(
                WINDIR_ALLOW
                    .iter()
                    .map(|(rel, contents_only)| Allow {
                        rel: names(rel),
                        contents_only: *contents_only,
                    })
                    .collect(),
            ),
            kind: ProtectedKind::WindowsDirectory,
        });

        if machine(KnownFolder::ProgramFiles).is_none() {
            return Err(NeverListError::MissingKnownFolder(
                KnownFolder::ProgramFiles,
            ));
        }
        for f in [KnownFolder::ProgramFiles, KnownFolder::ProgramFilesX86] {
            if let Some(pf) = machine(f) {
                let pf = canon_config(pf)?;
                rules.push(Rule {
                    anchor: Anchor::Absolute(pf.join_all(&names("WindowsApps"))),
                    scope: Scope::Subtree(Vec::new()),
                    kind: ProtectedKind::WindowsApps,
                });
                rules.push(Rule {
                    anchor: Anchor::Absolute(pf.join_all(&names("Common Files"))),
                    scope: Scope::Exact,
                    kind: ProtectedKind::ProgramFiles,
                });
                rules.push(Rule {
                    anchor: Anchor::Absolute(pf),
                    scope: Scope::Exact,
                    kind: ProtectedKind::ProgramFiles,
                });
            }
        }

        let profiles = machine(KnownFolder::UserProfiles).ok_or(
            NeverListError::MissingKnownFolder(KnownFolder::UserProfiles),
        )?;
        let profiles = canon_config(profiles)?;
        rules.push(Rule {
            anchor: Anchor::Absolute(profiles.clone()),
            scope: Scope::Exact,
            kind: ProtectedKind::UserProfilesRoot,
        });
        // Every child of the profiles root is a profile (including ones we
        // could not resolve unelevated, `Public`, `Default` and `All Users`).
        rules.push(Rule {
            anchor: Anchor::ChildOf(profiles),
            scope: Scope::Exact,
            kind: ProtectedKind::UserProfileRoot,
        });

        for folder in KnownFolder::ALL {
            if !folder.root_is_protected()
                || matches!(
                    folder,
                    KnownFolder::Windir
                        | KnownFolder::ProgramFiles
                        | KnownFolder::ProgramFilesX86
                        | KnownFolder::UserProfiles
                )
            {
                continue;
            }
            let kind = if folder == KnownFolder::UserProfile {
                ProtectedKind::UserProfileRoot
            } else {
                ProtectedKind::KnownFolderRoot { folder }
            };
            for (_, path) in config.known.resolve_all(folder) {
                let path = canon_config(path)?;
                let local_low = (folder == KnownFolder::LocalAppData)
                    .then(|| path.parent())
                    .flatten()
                    .map(|appdata| appdata.join_all(&names("LocalLow")));
                rules.push(Rule {
                    anchor: Anchor::Absolute(path),
                    scope: Scope::Exact,
                    kind,
                });
                if let Some(local_low) = local_low {
                    rules.push(Rule {
                        anchor: Anchor::Absolute(local_low),
                        scope: Scope::Exact,
                        kind: ProtectedKind::AppDataRoot,
                    });
                }
            }
        }

        for dir in &config.install_dirs {
            rules.push(Rule {
                anchor: Anchor::Absolute(canon_config(dir)?),
                scope: Scope::Subtree(Vec::new()),
                kind: ProtectedKind::StrataInstall,
            });
        }

        let mut volume_mounts = Vec::new();
        for v in &config.volumes.volumes {
            let guid = Name::from_str_name(&v.guid);
            let device = v.device.as_deref().map(Name::from_str_name);
            let mounts: Vec<CanonicalPath> = v
                .mount_points
                .iter()
                .filter_map(|m| CanonicalPath::parse(m).ok())
                .collect();
            // A folder mount point is the root of another volume.
            for m in mounts.iter().filter(|m| !m.is_root()) {
                rules.push(Rule {
                    anchor: Anchor::Absolute(m.clone()),
                    scope: Scope::Exact,
                    kind: ProtectedKind::VolumeRoot,
                });
            }
            volume_mounts.push((guid, device, mounts));
        }

        let mut local_hosts: Vec<Box<[u16]>> = ["localhost", "::1", "0:0:0:0:0:0:0:1", "[::1]"]
            .iter()
            .map(|h| Name::from_str_name(h).folded().into())
            .collect();
        for h in &config.local_hosts {
            local_hosts.push(Name::from_str_name(h).folded().into());
        }

        Ok(Self {
            rules,
            windir,
            volume_mounts,
            local_hosts,
        })
    }

    /// Checks a canonical path against every rule.
    ///
    /// # Errors
    ///
    /// Returns the first [`Refusal`] found.
    pub fn check(&self, path: &CanonicalPath) -> Result<(), Refusal> {
        self.check_as(path, &path.to_string())
    }

    /// Like [`NeverList::check`], but refusals name `display` (the path the
    /// user asked for) rather than the resolved form being checked.
    ///
    /// # Errors
    ///
    /// Returns the first [`Refusal`] found.
    pub fn check_as(&self, path: &CanonicalPath, display: &str) -> Result<(), Refusal> {
        for (interp, agnostic) in self.interpretations(path, display)? {
            self.check_one(&interp, agnostic, display)?;
        }
        Ok(())
    }

    /// Checks a raw path string: canonicalizes, then [`NeverList::check`].
    ///
    /// # Errors
    ///
    /// Returns a [`Refusal`] for malformed paths too.
    pub fn check_str(&self, path: impl AsRef<std::ffi::OsStr>) -> Result<CanonicalPath, Refusal> {
        let raw = path.as_ref();
        let canon =
            CanonicalPath::parse(raw).map_err(|e| Refusal::invalid(raw.to_string_lossy(), e))?;
        self.check(&canon)?;
        Ok(canon)
    }

    /// Refuses a top-level item that carries both the system and hidden
    /// attributes, the pair Windows uses to mark protected operating-system
    /// files at a volume root (e.g. `pagefile.sys`).
    ///
    /// # Errors
    ///
    /// Returns a [`Refusal`] when the rule applies.
    pub fn check_attributes(&self, path: &CanonicalPath, attributes: u32) -> Result<(), Refusal> {
        self.check_attributes_as(path, attributes, &path.to_string())
    }

    pub(crate) fn check_attributes_as(
        &self,
        path: &CanonicalPath,
        attributes: u32,
        display: &str,
    ) -> Result<(), Refusal> {
        use strata_core::win32::{FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM};
        let both = FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM;
        if path.components().len() == 1 && attributes & both == both {
            return Err(Refusal::protected(
                display,
                ProtectedKind::TopLevelSystemHidden,
                Relation::Itself,
                path.to_string(),
            ));
        }
        Ok(())
    }

    /// Absolute protected locations, for identity-based checks by the guard.
    pub(crate) fn absolute_locations(
        &self,
    ) -> impl Iterator<Item = (&CanonicalPath, ProtectedKind, bool)> {
        self.rules.iter().filter_map(|r| match &r.anchor {
            Anchor::Absolute(p) => Some((p, r.kind, matches!(r.scope, Scope::Subtree(_)))),
            _ => None,
        })
    }

    /// The whole list in documentation form.
    #[must_use]
    pub fn entries(&self) -> Vec<NeverListEntry> {
        self.rules
            .iter()
            .map(|r| {
                let location = match &r.anchor {
                    Anchor::Absolute(p) => p.to_string(),
                    Anchor::RootRelative(rel) => format!(
                        "<volume>\\{}",
                        rel.iter().map(Name::display).collect::<Vec<_>>().join("\\")
                    ),
                    Anchor::ChildOf(p) => format!("{p}\\*"),
                    Anchor::RootChildPrefix(prefix) => {
                        format!("<volume>\\{}*", String::from_utf16_lossy(prefix))
                    }
                };
                let (subtree, exceptions) = match &r.scope {
                    Scope::Exact => (false, Vec::new()),
                    Scope::Subtree(allow) => (
                        true,
                        allow
                            .iter()
                            .map(|a| {
                                let rel = a.rel.iter().map(Name::display).collect::<Vec<_>>();
                                if a.contents_only {
                                    format!("{}\\*", rel.join("\\"))
                                } else {
                                    rel.join("\\")
                                }
                            })
                            .collect(),
                    ),
                };
                NeverListEntry {
                    location,
                    subtree,
                    kind: r.kind,
                    exceptions,
                }
            })
            .collect()
    }

    /// Whether a UNC server name may address this machine.
    ///
    /// The SMB client resolves names through DNS and `inet_aton`-style
    /// parsing, so `localhost.`, `127.1`, `2130706433`, `0x7f.1`, `[::1]`
    /// and `0--1.ipv6-literal.net` all reach the local host. Any spelling
    /// that could be this machine is treated as loopback; over-refusing a
    /// remote host is harmless, missing a local one is not.
    fn is_loopback(&self, server: &Name) -> bool {
        let raw = String::from_utf16_lossy(server.folded()).to_ascii_lowercase();
        let host = raw.trim_end_matches('.');
        if host.is_empty() || host == "localhost" || host.ends_with(".localhost") {
            return true;
        }
        if ip_is_local(host) {
            return true;
        }
        let first_label = host.split('.').next().unwrap_or(host);
        self.local_hosts.iter().any(|h| {
            let h = String::from_utf16_lossy(h).to_ascii_lowercase();
            let h = h.trim_end_matches('.');
            h == host || (!h.contains('.') && h == first_label)
        })
    }

    /// Every configured mount point of every local volume.
    pub(crate) fn mount_points(&self) -> impl Iterator<Item = &CanonicalPath> {
        self.volume_mounts.iter().flat_map(|(_, _, m)| m.iter())
    }

    fn mounts_for_guid(&self, guid: &Name) -> Option<&[CanonicalPath]> {
        self.volume_mounts
            .iter()
            .find(|(g, _, _)| g == guid)
            .map(|(_, _, m)| m.as_slice())
            .filter(|m| !m.is_empty())
    }

    fn mounts_for_device(&self, device: &Name) -> Option<&[CanonicalPath]> {
        self.volume_mounts
            .iter()
            .find(|(_, d, _)| d.as_ref() == Some(device))
            .map(|(_, _, m)| m.as_slice())
            .filter(|m| !m.is_empty())
    }

    /// Every spelling of `path` worth checking, flagged when rules must be
    /// matched root-agnostically.
    fn interpretations(
        &self,
        path: &CanonicalPath,
        display: &str,
    ) -> Result<Vec<(CanonicalPath, bool)>, Refusal> {
        let mut base = vec![path.clone()];
        base.extend(path.trimmed_alias());
        let mut out = Vec::new();
        for q in base {
            match q.root() {
                Root::Drive(_) => out.push((q, false)),
                Root::Unc { server, share } => {
                    if self.is_loopback(server) {
                        return Err(Refusal {
                            path: display.to_string(),
                            reason: RefusalReason::LoopbackShare,
                        });
                    }
                    let s = share.folded();
                    if s.len() == 2
                        && s[1] == u16::from(b'$')
                        && u8::try_from(s[0]).is_ok_and(|b| b.is_ascii_uppercase())
                    {
                        // Administrative share: `\\host\C$\x` is `C:\x` on
                        // that host, which may be this one under another name.
                        let drive = CanonicalPath::from_parts(
                            Root::Drive(s[0] as u8),
                            q.components().to_vec(),
                        );
                        out.push((drive, false));
                    }
                    // Default shares onto the Windows directory, whatever the
                    // server is called: `ADMIN$` is `%WINDIR%` and `PRINT$`
                    // is the spooler's driver store inside it.
                    let into_windir: Option<&[&str]> = if share.is("ADMIN$") {
                        Some(&[])
                    } else if share.is("PRINT$") {
                        Some(&["System32", "spool", "drivers"])
                    } else {
                        None
                    };
                    if let Some(prefix) = into_windir {
                        let mut parts: Vec<Name> =
                            prefix.iter().map(|s| Name::from_str_name(s)).collect();
                        parts.extend_from_slice(q.components());
                        out.push((self.windir.join_all(&parts), false));
                    }
                    out.push((q, false));
                }
                Root::Volume(guid) => {
                    let mounts = self.mounts_for_guid(guid);
                    Self::push_mounted(&mut out, &q, mounts);
                }
                Root::Device(dev) => {
                    let mounts = self.mounts_for_device(dev);
                    Self::push_mounted(&mut out, &q, mounts);
                }
            }
        }
        Ok(out)
    }

    fn push_mounted(
        out: &mut Vec<(CanonicalPath, bool)>,
        q: &CanonicalPath,
        mounts: Option<&[CanonicalPath]>,
    ) {
        match mounts {
            Some(mounts) => {
                for m in mounts {
                    out.push((m.join_all(q.components()), false));
                }
                out.push((q.clone(), false));
            }
            None => out.push((q.clone(), true)),
        }
    }

    fn check_one(&self, p: &CanonicalPath, agnostic: bool, display: &str) -> Result<(), Refusal> {
        if p.is_root() {
            return Err(Refusal::protected(
                display,
                ProtectedKind::VolumeRoot,
                Relation::Itself,
                p.to_string(),
            ));
        }
        let rebase = |r: &CanonicalPath| -> CanonicalPath {
            if agnostic {
                CanonicalPath::from_parts(p.root().clone(), r.components().to_vec())
            } else {
                r.clone()
            }
        };
        for rule in &self.rules {
            let concrete = match &rule.anchor {
                Anchor::Absolute(r) => rebase(r),
                Anchor::RootRelative(rel) => {
                    CanonicalPath::from_parts(p.root().clone(), rel.clone())
                }
                Anchor::ChildOf(parent) => {
                    let parent = rebase(parent);
                    if parent.is_ancestor_of(p) {
                        parent.join(p.components()[parent.components().len()].clone())
                    } else {
                        // `p` can only contain a child of `parent` by
                        // containing `parent`, which its own Exact rule covers.
                        continue;
                    }
                }
                Anchor::RootChildPrefix(prefix) => {
                    let first = &p.components()[0];
                    if first.folded().starts_with(prefix) {
                        CanonicalPath::from_parts(p.root().clone(), vec![first.clone()])
                    } else {
                        continue;
                    }
                }
            };
            evaluate(rule, &concrete, p, display)?;
        }
        Ok(())
    }
}

/// Whether `host` (lowercase, trailing dots removed) is an IPv4 or IPv6
/// literal for the loopback or unspecified address, in any spelling the
/// Windows resolver accepts.
fn ip_is_local(host: &str) -> bool {
    use std::net::IpAddr;
    let local_v4 = |a: u32| a >> 24 == 127 || a == 0;
    if let Some(a) = parse_inet_aton(host) {
        return local_v4(a);
    }
    let v6 = host
        .strip_suffix(".ipv6-literal.net")
        .map(|s| s.replace('-', ":").replace('s', "%"))
        .unwrap_or_else(|| host.to_string());
    let v6 = v6.trim_start_matches('[').trim_end_matches(']');
    let v6 = v6.split('%').next().unwrap_or(v6);
    match v6.parse::<IpAddr>() {
        Ok(IpAddr::V6(a)) => {
            a.is_loopback()
                || a.is_unspecified()
                || a.to_ipv4().is_some_and(|v4| local_v4(u32::from(v4)))
        }
        Ok(IpAddr::V4(a)) => local_v4(u32::from(a)),
        Err(_) => false,
    }
}

/// `inet_aton` parsing: one to four dot-separated parts, each decimal, octal
/// (leading `0`) or hex (`0x`); the last part fills the remaining bytes.
fn parse_inet_aton(s: &str) -> Option<u32> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut nums = Vec::with_capacity(parts.len());
    for p in &parts {
        let n = if let Some(h) = p.strip_prefix("0x") {
            if h.is_empty() {
                0
            } else {
                u64::from_str_radix(h, 16).ok()?
            }
        } else if p.len() > 1 && p.starts_with('0') {
            u64::from_str_radix(&p[1..], 8).ok()?
        } else if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) {
            p.parse::<u64>().ok()?
        } else {
            return None;
        };
        nums.push(n);
    }
    let (last, head) = nums.split_last()?;
    let mut addr: u64 = 0;
    for (i, &n) in head.iter().enumerate() {
        if n > 0xFF {
            return None;
        }
        addr |= n << (24 - 8 * i);
    }
    let rest_bits = 32 - 8 * head.len() as u32;
    if *last >= 1u64 << rest_bits {
        return None;
    }
    u32::try_from(addr | last).ok()
}

fn evaluate(
    rule: &Rule,
    concrete: &CanonicalPath,
    p: &CanonicalPath,
    display: &str,
) -> Result<(), Refusal> {
    let refuse = |relation| {
        Err(Refusal::protected(
            display,
            rule.kind,
            relation,
            concrete.to_string(),
        ))
    };
    if concrete == p {
        return refuse(Relation::Itself);
    }
    if p.is_ancestor_of(concrete) {
        return refuse(Relation::Contains);
    }
    if concrete.is_ancestor_of(p)
        && let Scope::Subtree(allow) = &rule.scope
    {
        let allowed = allow.iter().any(|a| {
            let target = concrete.join_all(&a.rel);
            if a.contents_only {
                target.is_ancestor_of(p)
            } else {
                target == *p
            }
        });
        if !allowed {
            return refuse(Relation::Inside);
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use strata_core::known::UserFolders;

    /// A synthetic machine: two profiles, Documents redirected to D:, a
    /// localized Pictures folder, a volume mounted at `C:\mnt\data`.
    pub(crate) fn sample_config() -> NeverListConfig {
        let mut known = KnownFolders::default();
        known
            .machine
            .insert(KnownFolder::Windir, r"C:\Windows".into());
        known
            .machine
            .insert(KnownFolder::ProgramFiles, r"C:\Program Files".into());
        known.machine.insert(
            KnownFolder::ProgramFilesX86,
            r"C:\Program Files (x86)".into(),
        );
        known
            .machine
            .insert(KnownFolder::UserProfiles, r"C:\Users".into());
        known
            .machine
            .insert(KnownFolder::ProgramData, r"C:\ProgramData".into());
        known
            .machine
            .insert(KnownFolder::Public, r"C:\Users\Public".into());
        for (name, docs, pics) in [
            ("alice", r"D:\Docs\alice", r"C:\Users\alice\Bilder"),
            ("bob", r"C:\Users\bob\Documents", r"C:\Users\bob\Pictures"),
        ] {
            let base = format!(r"C:\Users\{name}");
            let mut u = UserFolders {
                is_current: name == "alice",
                ..Default::default()
            };
            u.folders
                .insert(KnownFolder::UserProfile, base.clone().into());
            u.folders.insert(
                KnownFolder::LocalAppData,
                format!(r"{base}\AppData\Local").into(),
            );
            u.folders.insert(
                KnownFolder::AppData,
                format!(r"{base}\AppData\Roaming").into(),
            );
            u.folders.insert(
                KnownFolder::Temp,
                format!(r"{base}\AppData\Local\Temp").into(),
            );
            u.folders.insert(KnownFolder::Documents, docs.into());
            u.folders.insert(KnownFolder::Pictures, pics.into());
            u.folders
                .insert(KnownFolder::Downloads, format!(r"{base}\Downloads").into());
            u.folders.insert(
                KnownFolder::Desktop,
                format!(r"{base}\OneDrive\Desktop").into(),
            );
            known.users.push(u);
        }
        NeverListConfig {
            known,
            install_dirs: vec![r"C:\Program Files\Strata".into()],
            volumes: VolumeMap {
                volumes: vec![
                    VolumeEntry {
                        guid: "{11111111-1111-4111-8111-111111111111}".into(),
                        device: Some("HarddiskVolume3".into()),
                        mount_points: vec![r"C:\".into()],
                    },
                    VolumeEntry {
                        guid: "{22222222-2222-4222-8222-222222222222}".into(),
                        device: Some("HarddiskVolume5".into()),
                        mount_points: vec![r"D:\".into(), r"C:\mnt\data\".into()],
                    },
                    VolumeEntry {
                        guid: "{33333333-3333-4333-8333-333333333333}".into(),
                        device: Some("HarddiskVolume1".into()),
                        mount_points: vec![],
                    },
                ],
            },
            local_hosts: vec!["DEVBOX".into(), "devbox.example.lan".into()],
        }
    }

    pub(crate) fn sample() -> NeverList {
        NeverList::new(sample_config()).unwrap()
    }

    fn refused(list: &NeverList, s: &str) -> Refusal {
        match list.check_str(s) {
            Ok(_) => panic!("{s} was allowed"),
            Err(r) => r,
        }
    }

    fn allowed(list: &NeverList, s: &str) {
        if let Err(r) = list.check_str(s) {
            panic!("{s} was refused: {}", r.message());
        }
    }

    fn kind_of(r: &Refusal) -> (ProtectedKind, Relation) {
        match r.reason {
            RefusalReason::Protected {
                protected,
                relation,
                ..
            } => (protected, relation),
            ref other => panic!("not a protected refusal: {other:?}"),
        }
    }

    #[test]
    fn requires_core_folders() {
        let mut c = sample_config();
        c.known.machine.remove(&KnownFolder::Windir);
        assert_eq!(
            NeverList::new(c).unwrap_err(),
            NeverListError::MissingKnownFolder(KnownFolder::Windir)
        );
    }

    #[test]
    fn volume_roots() {
        let l = sample();
        for s in [
            r"C:\",
            r"D:\",
            r"c:",
            r"\\?\C:\",
            r"\\?\Volume{33333333-3333-4333-8333-333333333333}\",
            r"\Device\HarddiskVolume9",
            r"\\nas\share",
            r"\\nas\share\",
            r"C:\mnt\data",
            r"Z:\",
        ] {
            if CanonicalPath::parse(s).is_err() {
                continue;
            }
            let r = refused(&l, s);
            assert!(
                matches!(kind_of(&r), (_, Relation::Itself | Relation::Contains)),
                "{s}"
            );
        }
        assert_eq!(kind_of(&refused(&l, r"E:\")).0, ProtectedKind::VolumeRoot);
    }

    #[test]
    fn windows_directory_and_allow_list() {
        let l = sample();
        for s in [
            r"C:\Windows",
            r"C:\Windows\System32",
            r"C:\Windows\System32\drivers\etc\hosts",
            r"C:\Windows\WinSxS",
            r"C:\Windows\Installer\x.msi",
            r"C:\Windows\Temp",
            r"C:\Windows\SoftwareDistribution",
            r"C:\Windows\SoftwareDistribution\Download",
            r"C:\Windows\SoftwareDistribution\DataStore\x.edb",
            r"C:\Windows\Logs",
            r"C:\Windows\Prefetch",
            r"C:\Windows\MEMORY.DMP\x",
            r"C:\Windows\Temporary",
        ] {
            refused(&l, s);
        }
        for s in [
            r"C:\Windows\Temp\x.tmp",
            r"C:\Windows\Temp\sub\dir",
            r"c:\windows\temp\X.TMP",
            r"C:\Windows\SoftwareDistribution\Download\abc",
            r"C:\Windows\MEMORY.DMP",
            r"C:\Windows\Minidump\1.dmp",
            r"C:\Windows\Logs\CBS\CBS.log",
            r"C:\Windows\Prefetch\APP.pf",
            r"C:\Windows\LiveKernelReports\x.dmp",
        ] {
            allowed(&l, s);
        }
    }

    #[test]
    fn allow_list_cannot_be_reached_by_dot_tricks() {
        let l = sample();
        // A verbatim `Temp.` folder is a different directory from `Temp`.
        refused(&l, r"\\?\C:\Windows\Temp.\x");
        refused(&l, r"\\?\C:\Windows\Temp \x");
        // Non-final trailing space is significant to Win32, so this is the
        // `Temp ` directory, which is not allow-listed.
        refused(&l, r"C:\Windows\Temp \x");
    }

    #[test]
    fn profiles_and_known_folders() {
        let l = sample();
        let cases = [
            (
                r"C:\Users",
                ProtectedKind::UserProfilesRoot,
                Relation::Itself,
            ),
            (
                r"C:\Users\alice",
                ProtectedKind::UserProfileRoot,
                Relation::Itself,
            ),
            (
                r"C:\Users\carol",
                ProtectedKind::UserProfileRoot,
                Relation::Itself,
            ),
            (
                r"C:\Users\Default",
                ProtectedKind::UserProfileRoot,
                Relation::Itself,
            ),
            (
                r"C:\Users\alice\AppData",
                ProtectedKind::KnownFolderRoot {
                    folder: KnownFolder::LocalAppData,
                },
                Relation::Contains,
            ),
            (
                r"C:\Users\alice\AppData\Local",
                ProtectedKind::KnownFolderRoot {
                    folder: KnownFolder::LocalAppData,
                },
                Relation::Itself,
            ),
            (
                r"C:\Users\alice\AppData\LocalLow",
                ProtectedKind::AppDataRoot,
                Relation::Itself,
            ),
            (
                r"C:\Users\bob\AppData\Roaming",
                ProtectedKind::KnownFolderRoot {
                    folder: KnownFolder::AppData,
                },
                Relation::Itself,
            ),
            (
                r"D:\Docs\alice",
                ProtectedKind::KnownFolderRoot {
                    folder: KnownFolder::Documents,
                },
                Relation::Itself,
            ),
            (
                r"D:\Docs",
                ProtectedKind::KnownFolderRoot {
                    folder: KnownFolder::Documents,
                },
                Relation::Contains,
            ),
            (
                r"C:\Users\alice\Bilder",
                ProtectedKind::KnownFolderRoot {
                    folder: KnownFolder::Pictures,
                },
                Relation::Itself,
            ),
            (
                r"C:\Users\alice\OneDrive",
                ProtectedKind::KnownFolderRoot {
                    folder: KnownFolder::Desktop,
                },
                Relation::Contains,
            ),
            (
                r"C:\ProgramData",
                ProtectedKind::KnownFolderRoot {
                    folder: KnownFolder::ProgramData,
                },
                Relation::Itself,
            ),
        ];
        for (s, kind, rel) in cases {
            assert_eq!(kind_of(&refused(&l, s)), (kind, rel), "{s}");
        }
        for s in [
            r"C:\Users\alice\AppData\Local\Temp",
            r"C:\Users\alice\AppData\Local\Temp\x",
            r"C:\Users\alice\AppData\Local\Google\Chrome\User Data\Default\Cache",
            r"D:\Docs\alice\report.docx",
            r"C:\Users\alice\Downloads\setup.exe",
            r"C:\Users\bob\Pictures\cat.jpg",
            r"C:\Users\alice\Documents",
            r"C:\ProgramData\Microsoft\Windows\WER\ReportQueue",
            r"D:\Users\x",
        ] {
            allowed(&l, s);
        }
    }

    #[test]
    fn program_files() {
        let l = sample();
        for s in [
            r"C:\Program Files",
            r"C:\Program Files (x86)",
            r"C:\Program Files (Arm)",
            r"D:\Program Files",
            r"C:\Program Files\Common Files",
            r"C:\Program Files\WindowsApps",
            r"C:\Program Files\WindowsApps\Microsoft.Foo_1.0\x.dll",
            r"C:\Program Files\Strata",
            r"C:\Program Files\Strata\strata.exe",
        ] {
            refused(&l, s);
        }
        allowed(&l, r"C:\Program Files\SomeApp\cache\x");
    }

    #[test]
    fn per_volume_system_items() {
        let l = sample();
        for s in [
            r"C:\System Volume Information",
            r"D:\System Volume Information\x",
            r"C:\$Recycle.Bin",
            r"C:\$RECYCLE.BIN\S-1-5-21\$R1.txt",
            r"C:\$MFT",
            r"C:\$Extend\$UsnJrnl",
            r"C:\pagefile.sys",
            r"D:\hiberfil.sys",
            r"C:\swapfile.sys",
            r"C:\Boot\BCD",
            r"C:\EFI",
            r"C:\bootmgr",
            r"C:\BOOTNXT",
            r"C:\Recovery\WindowsRE",
            r"C:\Documents and Settings",
            r"\\?\Volume{33333333-3333-4333-8333-333333333333}\EFI\Microsoft",
            r"\\nas\share\$RECYCLE.BIN",
        ] {
            refused(&l, s);
        }
        allowed(&l, r"C:\pagefile.sys.bak");
        allowed(&l, r"C:\NVIDIA\installer");
    }

    #[test]
    fn volume_guid_and_device_roots_map_to_mounts() {
        let l = sample();
        refused(
            &l,
            r"\\?\Volume{11111111-1111-4111-8111-111111111111}\Windows\System32",
        );
        refused(&l, r"\Device\HarddiskVolume3\Windows");
        refused(&l, r"\\?\GLOBALROOT\Device\HarddiskVolume3\Users\bob");
        allowed(
            &l,
            r"\\?\Volume{11111111-1111-4111-8111-111111111111}\Windows\Temp\x",
        );
        allowed(
            &l,
            r"\\?\Volume{22222222-2222-4222-8222-222222222222}\Windows\x",
        );
        // D: volume through its folder mount: `C:\mnt\data\Docs\alice` is
        // `D:\Docs\alice` (Documents root).
        refused(
            &l,
            r"\\?\Volume{22222222-2222-4222-8222-222222222222}\Docs\alice",
        );
        // Unmapped volumes are matched root-agnostically.
        refused(
            &l,
            r"\\?\Volume{00000000-0000-0000-0000-000000000000}\Windows\x",
        );
        refused(&l, r"\Device\HarddiskVolumeShadowCopy7\Windows\System32");
    }

    #[test]
    fn unc_admin_and_loopback_shares() {
        let l = sample();
        refused(&l, r"\\fileserver\C$\Windows\System32");
        refused(&l, r"\\?\UNC\fileserver\c$\Users\bob");
        let r = refused(&l, r"\\localhost\share\x");
        assert_eq!(r.reason, RefusalReason::LoopbackShare);
        refused(&l, r"\\127.0.0.2\share\x");
        refused(&l, r"\\devbox\data\x");
        refused(&l, r"\\DEVBOX.EXAMPLE.LAN\data\x");
        allowed(&l, r"\\fileserver\public\old\x.iso");
        allowed(&l, r"\\fileserver\D$\scratch\x");
    }

    #[test]
    fn top_level_system_hidden() {
        let l = sample();
        let p = CanonicalPath::parse(r"C:\Config.Msi").unwrap();
        assert!(l.check_attributes(&p, 0x6).is_err());
        assert!(l.check_attributes(&p, 0x2).is_ok());
        let deep = CanonicalPath::parse(r"C:\a\b").unwrap();
        assert!(l.check_attributes(&deep, 0x6).is_ok());
    }

    #[test]
    fn messages_are_human_readable() {
        let l = sample();
        let m = refused(&l, r"C:\Users").message();
        assert!(m.contains("every user profile"), "{m}");
        let m = refused(&l, r"C:\Windows\System32").message();
        assert!(m.contains("inside C:\\Windows"), "{m}");
        let m = refused(&l, r"C:\a\b:c").message();
        assert!(m.contains("alternate data stream"), "{m}");
    }

    #[test]
    fn entries_document_the_list() {
        let e = sample().entries();
        assert!(e.iter().any(|x| x.location == r"C:\Windows"
            && x.subtree
            && x.exceptions.contains(&r"Temp\*".to_string())));
        assert!(e.iter().any(|x| x.location == r"<volume>\pagefile.sys"));
        assert!(e.iter().any(|x| x.location == r"C:\Users\*"));
    }
}
