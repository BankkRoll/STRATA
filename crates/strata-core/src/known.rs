//! Resolved Windows known folders, as plain data.
//!
//! Resolution (`SHGetKnownFolderPath`, per-profile enumeration when elevated)
//! lives in the Windows layer. Consumers such as the classifier and the
//! cleanup never-list take a [`KnownFolders`] value instead of calling Win32,
//! so they stay pure and testable with synthetic layouts. Paths are never
//! hard-coded English names; localized or redirected folders come from the API.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A known folder that rules and safety checks can refer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum KnownFolder {
    /// `FOLDERID_Profile` (per user).
    UserProfile,
    /// `FOLDERID_LocalAppData` (per user).
    LocalAppData,
    /// `FOLDERID_LocalAppDataLow` (per user; low-integrity app data).
    LocalAppDataLow,
    /// `FOLDERID_SavedGames` (per user).
    SavedGames,
    /// `FOLDERID_RoamingAppData` (per user).
    AppData,
    /// The user's temp directory (`%TEMP%`, per user).
    Temp,
    /// `FOLDERID_Downloads` (per user).
    Downloads,
    /// `FOLDERID_Documents` (per user).
    Documents,
    /// `FOLDERID_Desktop` (per user).
    Desktop,
    /// `FOLDERID_Pictures` (per user).
    Pictures,
    /// `FOLDERID_Music` (per user).
    Music,
    /// `FOLDERID_Videos` (per user).
    Videos,
    /// `FOLDERID_ProgramData` (machine).
    ProgramData,
    /// `FOLDERID_Windows` (machine).
    Windir,
    /// `FOLDERID_ProgramFiles` (machine; native architecture).
    ProgramFiles,
    /// `FOLDERID_ProgramFilesX86` (machine).
    ProgramFilesX86,
    /// `FOLDERID_UserProfiles`, e.g. `C:\Users` (machine).
    UserProfiles,
    /// `FOLDERID_Public` (machine).
    Public,
}

impl KnownFolder {
    /// Every known folder.
    pub const ALL: [Self; 18] = [
        Self::UserProfile,
        Self::LocalAppData,
        Self::LocalAppDataLow,
        Self::SavedGames,
        Self::AppData,
        Self::Temp,
        Self::Downloads,
        Self::Documents,
        Self::Desktop,
        Self::Pictures,
        Self::Music,
        Self::Videos,
        Self::ProgramData,
        Self::Windir,
        Self::ProgramFiles,
        Self::ProgramFilesX86,
        Self::UserProfiles,
        Self::Public,
    ];

    /// Whether the folder differs per user profile.
    #[must_use]
    pub const fn is_per_user(self) -> bool {
        matches!(
            self,
            Self::UserProfile
                | Self::LocalAppData
                | Self::LocalAppDataLow
                | Self::SavedGames
                | Self::AppData
                | Self::Temp
                | Self::Downloads
                | Self::Documents
                | Self::Desktop
                | Self::Pictures
                | Self::Music
                | Self::Videos
        )
    }

    /// Rule-pack token, e.g. `{LOCALAPPDATA}`.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::UserProfile => "{USERPROFILE}",
            Self::LocalAppData => "{LOCALAPPDATA}",
            Self::LocalAppDataLow => "{LOCALAPPDATALOW}",
            Self::SavedGames => "{SAVEDGAMES}",
            Self::AppData => "{APPDATA}",
            Self::Temp => "{TEMP}",
            Self::Downloads => "{DOWNLOADS}",
            Self::Documents => "{DOCUMENTS}",
            Self::Desktop => "{DESKTOP}",
            Self::Pictures => "{PICTURES}",
            Self::Music => "{MUSIC}",
            Self::Videos => "{VIDEOS}",
            Self::ProgramData => "{PROGRAMDATA}",
            Self::Windir => "{WINDIR}",
            Self::ProgramFiles => "{PROGRAMFILES}",
            Self::ProgramFilesX86 => "{PROGRAMFILES_X86}",
            Self::UserProfiles => "{USERPROFILES}",
            Self::Public => "{PUBLIC}",
        }
    }

    /// Parses a rule-pack token (case-sensitive, braces included).
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.token() == token)
    }

    /// Whether the folder's root is protected from deletion (its contents
    /// may still be deleted individually).
    #[must_use]
    pub const fn root_is_protected(self) -> bool {
        !matches!(self, Self::Temp)
    }
}

/// Where a resolved per-user folder path came from.
///
/// Consumers use this to label derived locations (e.g. another user's
/// profile read without their registry hive) as less certain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderSource {
    /// `SHGetKnownFolderPath` / `GetTempPath2W` (authoritative).
    Api,
    /// The user's `User Shell Folders` registry value.
    Hive,
    /// `ProfileList\<SID>\ProfileImagePath`.
    ProfileList,
    /// Derived from the profile path; may be wrong if the folder is redirected.
    Derived,
}

/// Known folders of one user profile.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UserFolders {
    /// Account SID string (`S-1-5-21-...`), when known.
    pub sid: Option<String>,
    /// Display account name, when known.
    pub account: Option<String>,
    /// Whether this is the interactive user running Strata.
    pub is_current: bool,
    /// Resolved per-user folders.
    pub folders: HashMap<KnownFolder, PathBuf>,
    /// Provenance of each entry in `folders`; a missing entry means [`FolderSource::Api`].
    #[serde(default)]
    pub sources: HashMap<KnownFolder, FolderSource>,
}

impl UserFolders {
    /// Provenance of `folder`, defaulting to [`FolderSource::Api`].
    #[must_use]
    pub fn source(&self, folder: KnownFolder) -> FolderSource {
        self.sources
            .get(&folder)
            .copied()
            .unwrap_or(FolderSource::Api)
    }
}

/// All known folders on the machine: machine-wide plus one entry per profile.
///
/// # Example
///
/// ```
/// use strata_core::known::{KnownFolder, KnownFolders, UserFolders};
/// let mut kf = KnownFolders::default();
/// kf.machine.insert(KnownFolder::Windir, r"C:\Windows".into());
/// let mut me = UserFolders { is_current: true, ..Default::default() };
/// me.folders.insert(KnownFolder::Temp, r"C:\Users\me\AppData\Local\Temp".into());
/// kf.users.push(me);
/// assert_eq!(kf.resolve_all(KnownFolder::Windir).count(), 1);
/// assert_eq!(kf.current_user().unwrap().folders.len(), 1);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KnownFolders {
    /// Machine-wide folders.
    pub machine: HashMap<KnownFolder, PathBuf>,
    /// Per-profile folders. Contains only the current user unless elevated.
    pub users: Vec<UserFolders>,
}

impl KnownFolders {
    /// The interactive user's profile, if resolved.
    #[must_use]
    pub fn current_user(&self) -> Option<&UserFolders> {
        self.users.iter().find(|u| u.is_current)
    }

    /// Every resolved location of `folder`: one for machine folders, one per
    /// profile for per-user folders. Paired with the owning profile, if any.
    pub fn resolve_all(
        &self,
        folder: KnownFolder,
    ) -> impl Iterator<Item = (Option<&UserFolders>, &Path)> + '_ {
        let machine = (!folder.is_per_user())
            .then(|| self.machine.get(&folder))
            .flatten()
            .map(|p| (None, p.as_path()));
        let users = self.users.iter().filter_map(move |u| {
            folder
                .is_per_user()
                .then(|| u.folders.get(&folder))
                .flatten()
                .map(|p| (Some(u), p.as_path()))
        });
        machine.into_iter().chain(users)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip() {
        for f in KnownFolder::ALL {
            assert_eq!(KnownFolder::from_token(f.token()), Some(f));
        }
        assert_eq!(KnownFolder::from_token("{localappdata}"), None);
    }

    #[test]
    fn per_user_resolves_for_every_profile() {
        let mut kf = KnownFolders::default();
        for name in ["a", "b"] {
            let mut u = UserFolders::default();
            u.folders.insert(
                KnownFolder::LocalAppData,
                format!(r"C:\Users\{name}\AppData\Local").into(),
            );
            kf.users.push(u);
        }
        kf.machine
            .insert(KnownFolder::LocalAppData, r"C:\bogus".into());
        let paths: Vec<_> = kf.resolve_all(KnownFolder::LocalAppData).collect();
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().all(|(u, _)| u.is_some()));
    }
}
