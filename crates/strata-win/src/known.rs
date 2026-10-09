//! Known-folder resolution (SPEC §12.1, §21), populating
//! [`strata_core::known::KnownFolders`].
//!
//! - The current user's folders come from `SHGetKnownFolderPath`, so
//!   localized, redirected (Documents on `D:`) and OneDrive-backed folders are
//!   reported where they really are. Nothing is derived from English names.
//! - Other profiles come from the `ProfileList` registry key. Their folders
//!   are read from their `User Shell Folders` key when their hive is loaded
//!   and readable (signed in, or elevated), and otherwise derived from the
//!   profile path using the default on-disk layout. Each folder records its
//!   [`FolderSource`] so callers can label derived locations.
//! - System and service profiles (`S-1-5-18/19/20`, service SIDs) are skipped.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use strata_core::known::{KnownFolder, KnownFolders, UserFolders};
use windows::Win32::Storage::FileSystem::GetTempPathW;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, HKEY_USERS};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_LocalAppData,
    FOLDERID_LocalAppDataLow, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_Profile,
    FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, FOLDERID_Public,
    FOLDERID_RoamingAppData, FOLDERID_SavedGames, FOLDERID_UserProfiles, FOLDERID_Videos,
    FOLDERID_Windows, KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath,
};
use windows::core::{GUID, PCWSTR, s};

use crate::error::{Context, Result, WinError};
use crate::registry::RegKey;
use crate::wide::{from_pwstr, from_wide_nul};

const PROFILE_LIST: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList";
const USER_SHELL_FOLDERS: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders";

pub use strata_core::known::FolderSource;

/// Whether another user's registry hive could be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HiveAccess {
    /// The hive is loaded and was read.
    Read,
    /// The hive is not loaded (the user is not signed in).
    NotLoaded,
    /// The hive is loaded but not readable (needs elevation).
    Denied,
    /// Not applicable (the current user, resolved via the API).
    NotNeeded,
}

/// One profile's folders plus where the profile itself came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileFolders {
    /// The core view (SID, account, current flag, folders).
    pub user: UserFolders,
    /// Profile root from `ProfileList`, if listed.
    pub profile_path: Option<PathBuf>,
    /// Whether the profile root exists on disk (stale entries are common).
    pub profile_exists: bool,
    /// Hive access for this profile.
    pub hive: HiveAccess,
}

/// Machine folders plus every profile with provenance.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResolvedFolders {
    /// Machine-wide folders.
    pub machine: HashMap<KnownFolder, PathBuf>,
    /// Profiles; the current user first.
    pub profiles: Vec<ProfileFolders>,
}

impl ResolvedFolders {
    /// The plain-data view consumed by the classifier and the never-list.
    #[must_use]
    pub fn to_known_folders(&self) -> KnownFolders {
        KnownFolders {
            machine: self.machine.clone(),
            users: self.profiles.iter().map(|p| p.user.clone()).collect(),
        }
    }
}

/// Resolves machine folders, the current user, and every other user profile.
///
/// # Example
///
/// ```
/// use strata_core::known::KnownFolder;
/// let resolved = strata_win::known::resolve_known_folders().unwrap();
/// let kf = resolved.to_known_folders();
/// let me = kf.current_user().unwrap();
/// assert!(me.folders.contains_key(&KnownFolder::LocalAppData));
/// ```
pub fn resolve_known_folders() -> Result<ResolvedFolders> {
    let machine = machine_folders()?;
    let me = current_user_folders()?;
    let my_sid = me.user.sid.clone();
    let mut profiles = vec![me];
    for entry in profile_list()? {
        if Some(&entry.sid) == my_sid.as_ref() {
            if let Some(p) = profiles.first_mut() {
                p.profile_path = Some(entry.path.clone());
                p.profile_exists = entry.path.is_dir();
            }
            continue;
        }
        profiles.push(other_user_folders(&entry));
    }
    Ok(ResolvedFolders { machine, profiles })
}

/// Convenience: [`resolve_known_folders`] as the core type.
pub fn known_folders() -> Result<KnownFolders> {
    Ok(resolve_known_folders()?.to_known_folders())
}

fn folder_id(f: KnownFolder) -> Option<GUID> {
    Some(match f {
        KnownFolder::UserProfile => FOLDERID_Profile,
        KnownFolder::LocalAppData => FOLDERID_LocalAppData,
        KnownFolder::LocalAppDataLow => FOLDERID_LocalAppDataLow,
        KnownFolder::SavedGames => FOLDERID_SavedGames,
        KnownFolder::AppData => FOLDERID_RoamingAppData,
        KnownFolder::Temp => return None,
        KnownFolder::Downloads => FOLDERID_Downloads,
        KnownFolder::Documents => FOLDERID_Documents,
        KnownFolder::Desktop => FOLDERID_Desktop,
        KnownFolder::Pictures => FOLDERID_Pictures,
        KnownFolder::Music => FOLDERID_Music,
        KnownFolder::Videos => FOLDERID_Videos,
        KnownFolder::ProgramData => FOLDERID_ProgramData,
        KnownFolder::Windir => FOLDERID_Windows,
        KnownFolder::ProgramFiles => FOLDERID_ProgramFiles,
        KnownFolder::ProgramFilesX86 => FOLDERID_ProgramFilesX86,
        KnownFolder::UserProfiles => FOLDERID_UserProfiles,
        KnownFolder::Public => FOLDERID_Public,
    })
}

/// `SHGetKnownFolderPath` for the current user.
pub fn known_folder_path(id: &GUID) -> Result<PathBuf> {
    // NOTE: DONT_VERIFY so a folder that was deleted (e.g. no Music folder)
    // still resolves to where it would be instead of failing.
    // SAFETY: `id` is a valid GUID; the returned string is freed below.
    let p = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY, None) }
        .ctx("SHGetKnownFolderPath")?;
    // SAFETY: `p` is a NUL-terminated string allocated by the shell.
    let s = unsafe { from_pwstr(PCWSTR(p.0)) };
    // SAFETY: the string was allocated with CoTaskMemAlloc and is freed once.
    unsafe { CoTaskMemFree(Some(p.0.cast())) };
    Ok(PathBuf::from(s))
}

fn machine_folders() -> Result<HashMap<KnownFolder, PathBuf>> {
    let mut out = HashMap::new();
    for f in KnownFolder::ALL.into_iter().filter(|f| !f.is_per_user()) {
        if let Some(id) = folder_id(f) {
            match known_folder_path(&id) {
                Ok(p) => {
                    out.insert(f, p);
                }
                // COMPAT: ProgramFilesX86 does not exist on 32-bit-only
                // systems; skip any folder the shell does not know.
                Err(_) => continue,
            }
        }
    }
    Ok(out)
}

type GetTempPath2Fn = unsafe extern "system" fn(u32, *mut u16) -> u32;

/// The current user's temp directory, without the trailing backslash.
///
/// Uses `GetTempPath2W` when available: for SYSTEM processes it returns the
/// protected `C:\Windows\SystemTemp` instead of the world-writable
/// `C:\Windows\Temp`.
// COMPAT: GetTempPath2W exists on Windows 11 and Windows 10 21H2+ (backported
// in 2022). It is resolved at runtime so older Windows 10 builds still load.
pub fn temp_dir() -> Result<PathBuf> {
    let mut buf = [0u16; 261];
    // SAFETY: kernel32 is always loaded; the name is a static C string.
    let proc = unsafe { GetModuleHandleW(windows::core::w!("kernel32.dll")) }
        .ok()
        .and_then(|m| unsafe { GetProcAddress(m, s!("GetTempPath2W")) });
    let n = match proc {
        Some(f) => {
            // SAFETY: GetTempPath2W has exactly this signature
            // (DWORD BufferLength, LPWSTR Buffer).
            let f: GetTempPath2Fn = unsafe { std::mem::transmute(f) };
            // SAFETY: `buf` holds 261 units, the documented maximum + 1.
            unsafe { f(buf.len() as u32, buf.as_mut_ptr()) }
        }
        // SAFETY: `buf` is writable for its length.
        None => unsafe { GetTempPathW(Some(&mut buf)) },
    } as usize;
    if n == 0 || n > buf.len() {
        return Err(WinError::last("GetTempPath2W"));
    }
    Ok(trim_trailing_sep(PathBuf::from(from_wide_nul(&buf[..n]))))
}

fn trim_trailing_sep(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    // Keep roots like `C:\` intact.
    if s.len() > 3 && s.ends_with('\\') {
        PathBuf::from(s.trim_end_matches('\\'))
    } else {
        p
    }
}

fn current_user_folders() -> Result<ProfileFolders> {
    let account = crate::token::current_user()?;
    let mut folders = HashMap::new();
    let mut sources = HashMap::new();
    for f in KnownFolder::ALL.into_iter().filter(|f| f.is_per_user()) {
        let path = match folder_id(f) {
            Some(id) => known_folder_path(&id),
            None => temp_dir(),
        };
        if let Ok(p) = path {
            folders.insert(f, p);
            sources.insert(f, FolderSource::Api);
        }
    }
    Ok(ProfileFolders {
        profile_path: folders.get(&KnownFolder::UserProfile).cloned(),
        profile_exists: folders
            .get(&KnownFolder::UserProfile)
            .is_some_and(|p| p.is_dir()),
        user: UserFolders {
            sid: Some(account.sid),
            account: account.account,
            is_current: true,
            folders,
            sources,
        },
        hive: HiveAccess::NotNeeded,
    })
}

/// One `ProfileList` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEntry {
    /// SID string.
    pub sid: String,
    /// Expanded `ProfileImagePath`.
    pub path: PathBuf,
}

/// Whether a profile SID belongs to a real user: local/domain accounts
/// (`S-1-5-21-`) and Microsoft Entra ID accounts (`S-1-12-1-`). System,
/// LocalService, NetworkService and service SIDs are excluded.
#[must_use]
pub fn is_user_profile_sid(sid: &str) -> bool {
    sid.starts_with("S-1-5-21-") || sid.starts_with("S-1-12-1-")
}

/// Real-user entries of `HKLM\...\ProfileList` (readable unelevated).
pub fn profile_list() -> Result<Vec<ProfileEntry>> {
    let Some(list) = RegKey::open(HKEY_LOCAL_MACHINE, PROFILE_LIST)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for sid in list.subkeys()? {
        // NOTE: `.bak` suffixes mark profiles Windows is repairing; they
        // duplicate a real SID and are skipped with the other non-user keys.
        if !is_user_profile_sid(&sid) || sid.contains('.') {
            continue;
        }
        let Ok(Some(key)) = RegKey::open(HKEY_LOCAL_MACHINE, &format!(r"{PROFILE_LIST}\{sid}"))
        else {
            continue;
        };
        let Ok(Some(raw)) = key.query_string("ProfileImagePath") else {
            continue;
        };
        let path = if raw.expandable {
            match expand_for_profile(&raw.value, None, machine_var) {
                Some(p) => p,
                None => continue,
            }
        } else {
            PathBuf::from(raw.value)
        };
        out.push(ProfileEntry { sid, path });
    }
    Ok(out)
}

/// Machine-level environment variables, which are the same for every user.
fn machine_var(name: &str) -> Option<OsString> {
    const MACHINE: [&str; 7] = [
        "SYSTEMDRIVE",
        "SYSTEMROOT",
        "WINDIR",
        "PROGRAMDATA",
        "ALLUSERSPROFILE",
        "PUBLIC",
        "PROGRAMFILES",
    ];
    MACHINE
        .contains(&name.to_ascii_uppercase().as_str())
        .then(|| std::env::var_os(name))
        .flatten()
}

/// Expands `%VAR%` references for another user's profile.
///
/// `%USERPROFILE%` becomes `profile`; machine-level variables come from
/// `machine`. Any other variable (`%USERNAME%`, `%OneDrive%`, ...) would
/// expand to the *current* user's value, which is wrong for another profile,
/// so the whole value is rejected (`None`) and the caller falls back.
#[must_use]
pub fn expand_for_profile(
    raw: &std::ffi::OsStr,
    profile: Option<&Path>,
    machine: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let s = raw.to_str()?;
    let mut out = OsString::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find('%')?;
        let name = &after[..end];
        if name.eq_ignore_ascii_case("USERPROFILE") {
            out.push(profile?.as_os_str());
        } else {
            out.push(machine(name)?);
        }
        rest = &after[end + 1..];
    }
    out.push(rest);
    Some(PathBuf::from(out))
}

/// Registry value name under `User Shell Folders` for each per-user folder.
fn shell_folder_value(f: KnownFolder) -> Option<&'static str> {
    Some(match f {
        KnownFolder::Desktop => "Desktop",
        KnownFolder::Documents => "Personal",
        KnownFolder::Pictures => "My Pictures",
        KnownFolder::Music => "My Music",
        KnownFolder::Videos => "My Video",
        KnownFolder::Downloads => "{374DE290-123F-4565-9164-39C4925E467B}",
        KnownFolder::AppData => "AppData",
        KnownFolder::LocalAppData => "Local AppData",
        KnownFolder::LocalAppDataLow => "{A520A1A4-1780-4FF6-BD18-167343C5AF16}",
        KnownFolder::SavedGames => "{4C5C32FF-BB9D-43b0-B5B4-2D72E54EAAA4}",
        _ => return None,
    })
}

/// Default on-disk location relative to the profile root.
// NOTE: these are file-system names, which are English on every Windows
// language since Vista (the localized names are display names from
// desktop.ini). They are only used when the user's hive cannot be read.
fn default_relative(f: KnownFolder) -> Option<&'static str> {
    Some(match f {
        KnownFolder::Desktop => "Desktop",
        KnownFolder::Documents => "Documents",
        KnownFolder::Pictures => "Pictures",
        KnownFolder::Music => "Music",
        KnownFolder::Videos => "Videos",
        KnownFolder::Downloads => "Downloads",
        KnownFolder::AppData => r"AppData\Roaming",
        KnownFolder::LocalAppData => r"AppData\Local",
        KnownFolder::LocalAppDataLow => r"AppData\LocalLow",
        KnownFolder::SavedGames => "Saved Games",
        _ => return None,
    })
}

fn other_user_folders(entry: &ProfileEntry) -> ProfileFolders {
    let mut folders = HashMap::new();
    let mut sources = HashMap::new();
    folders.insert(KnownFolder::UserProfile, entry.path.clone());
    sources.insert(KnownFolder::UserProfile, FolderSource::ProfileList);

    let (hive, shell) =
        match RegKey::open(HKEY_USERS, &format!(r"{}\{USER_SHELL_FOLDERS}", entry.sid)) {
            Ok(Some(k)) => (HiveAccess::Read, Some(k)),
            Ok(None) => (HiveAccess::NotLoaded, None),
            Err(_) => (HiveAccess::Denied, None),
        };
    for f in KnownFolder::ALL.into_iter().filter(|f| f.is_per_user()) {
        if f == KnownFolder::UserProfile || f == KnownFolder::Temp {
            continue;
        }
        let from_hive = shell
            .as_ref()
            .zip(shell_folder_value(f))
            .and_then(|(k, name)| k.query_string(name).ok().flatten())
            .and_then(|v| {
                if v.expandable {
                    expand_for_profile(&v.value, Some(&entry.path), machine_var)
                } else {
                    Some(PathBuf::from(v.value))
                }
            });
        if let Some(p) = from_hive {
            folders.insert(f, p);
            sources.insert(f, FolderSource::Hive);
        } else if let Some(rel) = default_relative(f) {
            folders.insert(f, entry.path.join(rel));
            sources.insert(f, FolderSource::Derived);
        }
    }
    // Temp: the user's TEMP variable if the hive is readable, else the
    // default under LocalAppData.
    let temp = (hive == HiveAccess::Read)
        .then(|| {
            RegKey::open(HKEY_USERS, &format!(r"{}\Environment", entry.sid))
                .ok()
                .flatten()
        })
        .flatten()
        .and_then(|k| k.query_string("TEMP").ok().flatten())
        .and_then(|v| expand_for_profile(&v.value, Some(&entry.path), machine_var));
    match temp {
        Some(t) => {
            folders.insert(KnownFolder::Temp, t);
            sources.insert(KnownFolder::Temp, FolderSource::Hive);
        }
        None => {
            if let Some(local) = folders.get(&KnownFolder::LocalAppData).cloned() {
                folders.insert(KnownFolder::Temp, local.join("Temp"));
                sources.insert(KnownFolder::Temp, FolderSource::Derived);
            }
        }
    }
    ProfileFolders {
        user: UserFolders {
            sid: Some(entry.sid.clone()),
            account: crate::sid::account_name(&entry.sid),
            is_current: false,
            folders,
            sources,
        },
        profile_path: Some(entry.path.clone()),
        profile_exists: entry.path.is_dir(),
        hive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn machine(name: &str) -> Option<OsString> {
        match name.to_ascii_uppercase().as_str() {
            "SYSTEMDRIVE" => Some("C:".into()),
            _ => None,
        }
    }

    #[test]
    fn expansion_for_other_profiles() {
        let prof = Path::new(r"D:\Users\bob");
        assert_eq!(
            expand_for_profile(OsStr::new(r"%USERPROFILE%\Documents"), Some(prof), machine)
                .unwrap(),
            Path::new(r"D:\Users\bob\Documents")
        );
        assert_eq!(
            expand_for_profile(OsStr::new(r"%SystemDrive%\Users\bob"), None, machine).unwrap(),
            Path::new(r"C:\Users\bob")
        );
        assert_eq!(
            expand_for_profile(OsStr::new(r"E:\Docs"), Some(prof), machine).unwrap(),
            Path::new(r"E:\Docs")
        );
        assert!(expand_for_profile(OsStr::new(r"%OneDrive%\Docs"), Some(prof), machine).is_none());
        assert!(expand_for_profile(OsStr::new(r"%USERPROFILE%\x"), None, machine).is_none());
        assert!(expand_for_profile(OsStr::new(r"%unterminated"), Some(prof), machine).is_none());
    }

    #[test]
    fn profile_sid_filter() {
        assert!(!is_user_profile_sid("S-1-5-18"));
        assert!(!is_user_profile_sid("S-1-5-19"));
        assert!(!is_user_profile_sid("S-1-5-20"));
        assert!(!is_user_profile_sid("S-1-5-80-123"));
        assert!(is_user_profile_sid("S-1-5-21-1-2-3-1001"));
        assert!(is_user_profile_sid("S-1-12-1-1-2-3-4"));
    }

    #[test]
    fn derived_profile_layout() {
        let entry = ProfileEntry {
            sid: "S-1-5-21-0-0-0-424242".into(),
            path: PathBuf::from(r"C:\Users\nobody-strata-test"),
        };
        let p = other_user_folders(&entry);
        assert_eq!(p.hive, HiveAccess::NotLoaded);
        assert!(!p.profile_exists);
        assert_eq!(
            p.user.folders[&KnownFolder::LocalAppData],
            Path::new(r"C:\Users\nobody-strata-test\AppData\Local")
        );
        assert_eq!(
            p.user.folders[&KnownFolder::Temp],
            Path::new(r"C:\Users\nobody-strata-test\AppData\Local\Temp")
        );
        assert_eq!(p.user.sources[&KnownFolder::Temp], FolderSource::Derived);
        assert_eq!(
            p.user.sources[&KnownFolder::UserProfile],
            FolderSource::ProfileList
        );
        for f in KnownFolder::ALL.into_iter().filter(|f| f.is_per_user()) {
            assert!(p.user.folders.contains_key(&f), "{f:?}");
        }
    }

    #[test]
    fn resolves_this_machine() {
        let r = resolve_known_folders().unwrap();
        for f in KnownFolder::ALL.into_iter().filter(|f| !f.is_per_user()) {
            if f == KnownFolder::ProgramFilesX86 && cfg!(target_pointer_width = "32") {
                continue;
            }
            assert!(r.machine.contains_key(&f), "{f:?}");
        }
        let me = &r.profiles[0];
        assert!(me.user.is_current);
        assert!(me.user.sid.as_deref().unwrap().starts_with("S-1-"));
        for f in KnownFolder::ALL.into_iter().filter(|f| f.is_per_user()) {
            assert!(me.user.folders.contains_key(&f), "{f:?}");
            assert_eq!(me.user.sources[&f], FolderSource::Api);
        }
        let temp = &me.user.folders[&KnownFolder::Temp];
        assert!(!temp.to_string_lossy().ends_with('\\'));
        let kf = r.to_known_folders();
        assert_eq!(kf.users.iter().filter(|u| u.is_current).count(), 1);
        assert!(r.profiles.iter().skip(1).all(|p| !p.user.is_current));
        if std::env::var_os("STRATA_PRINT_KNOWN").is_some() {
            println!("{}", serde_json::to_string_pretty(&r).unwrap());
        }
    }
}
