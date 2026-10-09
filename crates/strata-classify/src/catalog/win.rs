//! Windows sources for the installed-apps catalog: registry Uninstall keys
//! (machine, WOW64, current user and other loaded user hives) and AppX/MSIX
//! packages via `Windows.Management.Deployment.PackageManager`.
//!
//! Read-only. Unreadable keys and packages are skipped and reported as
//! warnings; nothing here fails the whole catalog.

use std::path::{Path, PathBuf};

use windows::Management::Deployment::PackageManager;
use windows::core::HSTRING;
use windows_registry::{CURRENT_USER, Key, LOCAL_MACHINE, USERS};

use super::{AppSource, InstalledApp};

const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
const UNINSTALL_WOW: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";

fn string(key: &Key, name: &str) -> Option<String> {
    key.get_string(name)
        .ok()
        .map(|s| s.trim().trim_matches('"').trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_key(
    root: &Key,
    path: &str,
    source: &AppSource,
    out: &mut Vec<InstalledApp>,
    warnings: &mut Vec<String>,
) {
    let Ok(base) = root.open(path) else { return };
    let names: Vec<String> = match base.keys() {
        Ok(k) => k.collect(),
        Err(e) => {
            warnings.push(format!("cannot enumerate {path}: {e}"));
            return;
        }
    };
    for name in names {
        let Ok(k) = base.open(&name) else {
            warnings.push(format!("cannot open {path}\\{name}"));
            continue;
        };
        let Some(display) = string(&k, "DisplayName") else {
            continue;
        };
        // Updates and patches point at their parent product.
        if string(&k, "ParentKeyName").is_some()
            || k.get_u32("IsMinorUpgrade").is_ok_and(|v| v == 1)
        {
            continue;
        }
        out.push(InstalledApp {
            name: display,
            publisher: string(&k, "Publisher"),
            version: string(&k, "DisplayVersion"),
            install_location: string(&k, "InstallLocation").map(PathBuf::from),
            display_icon: string(&k, "DisplayIcon"),
            estimated_size: k
                .get_u32("EstimatedSize")
                .ok()
                .map(|kb| u64::from(kb) * 1024),
            uninstall_string: string(&k, "UninstallString")
                .or_else(|| string(&k, "QuietUninstallString")),
            install_date: string(&k, "InstallDate"),
            source: source.clone(),
            key: name,
            package_family: None,
            data_dirs: Vec::new(),
            system_component: k.get_u32("SystemComponent").is_ok_and(|v| v == 1),
        });
    }
}

/// Reads every Uninstall key Strata can see.
pub(super) fn read_uninstall_entries(warnings: &mut Vec<String>) -> Vec<InstalledApp> {
    let mut out = Vec::new();
    read_key(
        LOCAL_MACHINE,
        UNINSTALL,
        &AppSource::Hklm,
        &mut out,
        warnings,
    );
    read_key(
        LOCAL_MACHINE,
        UNINSTALL_WOW,
        &AppSource::HklmWow64,
        &mut out,
        warnings,
    );
    read_key(
        CURRENT_USER,
        UNINSTALL,
        &AppSource::Hkcu,
        &mut out,
        warnings,
    );
    if let Ok(users) = USERS.keys() {
        for sid in users {
            // Service accounts and the `_Classes` companions of loaded hives
            // carry no per-user installs.
            let skip =
                sid.ends_with("_Classes") || !sid.starts_with("S-1-5-21-") || sid == ".DEFAULT";
            if skip {
                continue;
            }
            let Ok(hive) = USERS.open(&sid) else { continue };
            let mut found = Vec::new();
            read_key(
                &hive,
                UNINSTALL,
                &AppSource::UserHive { sid: sid.clone() },
                &mut found,
                warnings,
            );
            out.append(&mut found);
        }
    }
    // The current user's hive also appears under HKU\<sid>; drop those
    // copies but keep other users' installs of the same app.
    let current: std::collections::HashSet<_> = out
        .iter()
        .filter(|a| a.source == AppSource::Hkcu)
        .map(|a| (a.key.clone(), a.name.clone(), a.install_location.clone()))
        .collect();
    out.retain(|a| {
        !matches!(a.source, AppSource::UserHive { .. })
            || !current.contains(&(a.key.clone(), a.name.clone(), a.install_location.clone()))
    });
    out
}

/// Reads the current user's AppX/MSIX packages.
pub(super) fn read_appx_packages(
    local_appdata: Option<&Path>,
) -> windows::core::Result<Vec<InstalledApp>> {
    let pm = PackageManager::new()?;
    // An empty SID means "the current user"; no elevation needed.
    let pkgs = pm.FindPackagesByUserSecurityId(&HSTRING::new())?;
    let mut out = Vec::new();
    for p in pkgs {
        let Ok(id) = p.Id() else { continue };
        let Ok(family) = id.FamilyName().map(|h| h.to_string_lossy()) else {
            continue;
        };
        let full = id
            .FullName()
            .map(|h| h.to_string_lossy())
            .unwrap_or_else(|_| family.clone());
        let name = p
            .DisplayName()
            .map(|h| h.to_string_lossy())
            .ok()
            .filter(|s| !s.is_empty() && !s.starts_with("ms-resource:"))
            .or_else(|| id.Name().ok().map(|h| h.to_string_lossy()))
            .unwrap_or_else(|| family.clone());
        let publisher = p
            .PublisherDisplayName()
            .ok()
            .map(|h| h.to_string_lossy())
            .filter(|s| !s.is_empty() && !s.starts_with("ms-resource:"));
        let version = id
            .Version()
            .ok()
            .map(|v| format!("{}.{}.{}.{}", v.Major, v.Minor, v.Build, v.Revision));
        let install = p
            .InstalledPath()
            .ok()
            .map(|h| PathBuf::from(h.to_string_lossy()));
        let system = p.IsFramework().unwrap_or(false)
            || p.IsResourcePackage().unwrap_or(false)
            || p.SignatureKind()
                .is_ok_and(|k| k == windows::ApplicationModel::PackageSignatureKind::System);
        let data_dirs = local_appdata
            .map(|l| l.join("Packages").join(&family))
            .into_iter()
            .collect();
        out.push(InstalledApp {
            name,
            publisher,
            version,
            install_location: install,
            display_icon: None,
            estimated_size: None,
            uninstall_string: None,
            install_date: None,
            source: AppSource::Appx,
            key: full,
            package_family: Some(family),
            data_dirs,
            system_component: system,
        });
    }
    Ok(out)
}
