//! Non-English, redirected and relocated known folders across several
//! profiles: the classifier and the never-list, built from the same
//! synthetic `KnownFolders`, agree. No known-folder root is ever offered as
//! safe, every protected root is refused by the never-list, and ordinary
//! temporary files under the relocated folders stay cleanable.

use std::path::PathBuf;

use strata_classify::{Classifier, DynamicRoots, Entry, RuleSet};
use strata_clean::never::{NeverList, NeverListConfig};
use strata_core::Safety;
use strata_core::known::{KnownFolder, KnownFolders, UserFolders};

fn user(sid: &str, current: bool, folders: &[(KnownFolder, &str)]) -> UserFolders {
    let mut u = UserFolders {
        sid: Some(sid.into()),
        account: None,
        is_current: current,
        ..Default::default()
    };
    for (k, v) in folders {
        u.folders.insert(*k, PathBuf::from(*v));
    }
    u
}

/// A German system with Windows on E:, profiles on D:, and two users whose
/// folders are redirected, localized and partly in OneDrive.
fn known() -> KnownFolders {
    let mut kf = KnownFolders::default();
    for (k, v) in [
        (KnownFolder::Windir, r"E:\WINNT"),
        (KnownFolder::ProgramFiles, r"C:\Programme"),
        (KnownFolder::ProgramFilesX86, r"C:\Programme (x86)"),
        (KnownFolder::ProgramData, r"C:\ProgramData"),
        (KnownFolder::UserProfiles, r"D:\Benutzer"),
        (KnownFolder::Public, r"D:\Benutzer\Öffentlich"),
    ] {
        kf.machine.insert(k, PathBuf::from(v));
    }
    kf.users.push(user(
        "S-1-5-21-9-1001",
        true,
        &[
            (KnownFolder::UserProfile, r"D:\Benutzer\hans"),
            (KnownFolder::LocalAppData, r"D:\Benutzer\hans\AppData\Local"),
            (KnownFolder::AppData, r"D:\Benutzer\hans\AppData\Roaming"),
            (KnownFolder::Temp, r"D:\Benutzer\hans\AppData\Local\Temp"),
            (KnownFolder::Documents, r"D:\Benutzer\hans\Dokumente"),
            (KnownFolder::Downloads, r"F:\Téléchargements"),
            (
                KnownFolder::Desktop,
                r"D:\Benutzer\hans\OneDrive - Firma\Schreibtisch",
            ),
            (KnownFolder::Pictures, r"D:\Benutzer\hans\OneDrive\Bilder"),
            (KnownFolder::Music, r"G:\Музыка"),
            (KnownFolder::Videos, r"D:\Benutzer\hans\Vidéos"),
        ],
    ));
    kf.users.push(user(
        "S-1-5-21-9-1002",
        false,
        &[
            (KnownFolder::UserProfile, r"D:\Benutzer\Ærøskøbing"),
            (
                KnownFolder::LocalAppData,
                r"D:\Benutzer\Ærøskøbing\AppData\Local",
            ),
            (
                KnownFolder::Temp,
                r"D:\Benutzer\Ærøskøbing\AppData\Local\Temp",
            ),
            (KnownFolder::Documents, r"H:\مستندات"),
            (KnownFolder::Desktop, r"D:\Benutzer\Ærøskøbing\桌面"),
        ],
    ));
    kf
}

fn setup() -> (Classifier, NeverList, KnownFolders) {
    let kf = known();
    let rules = RuleSet::builtin().unwrap();
    let c = Classifier::new(&rules, &kf, &DynamicRoots::default()).unwrap();
    let never = NeverList::new(NeverListConfig {
        known: kf.clone(),
        ..Default::default()
    })
    .unwrap();
    (c, never, kf)
}

fn roots(kf: &KnownFolders) -> Vec<(KnownFolder, Option<usize>, PathBuf)> {
    let mut v: Vec<_> = kf
        .machine
        .iter()
        .map(|(k, p)| (*k, None, p.clone()))
        .collect();
    for (i, u) in kf.users.iter().enumerate() {
        v.extend(u.folders.iter().map(|(k, p)| (*k, Some(i), p.clone())));
    }
    v
}

#[test]
fn known_folder_roots_are_never_safe_and_always_refused() {
    let (c, never, kf) = setup();
    for (folder, owner, path) in roots(&kf) {
        let s = path.to_str().unwrap();
        let cls = c.classify_path(s, &Entry::dir("x"));
        if folder.root_is_protected() {
            assert!(never.check_str(s).is_err(), "{folder:?} {s} allowed");
            assert!(
                !matches!(cls.safety, Safety::Safe | Safety::Probably),
                "{folder:?} {s} classified {:?}",
                cls.safety
            );
        }
        // Folders inside a profile are attributed to that profile.
        if let Some(i) = owner {
            let home = &kf.users[i].folders[&KnownFolder::UserProfile];
            if path.starts_with(home) {
                assert_eq!(cls.user, Some(i as u16), "{s}");
            }
        }
    }
}

#[test]
fn temporary_files_under_relocated_folders_stay_cleanable() {
    let (c, never, kf) = setup();
    for u in &kf.users {
        let temp = u.folders[&KnownFolder::Temp].to_str().unwrap().to_string();
        let file = format!(r"{temp}\~setup4711.tmp");
        assert!(never.check_str(&file).is_ok(), "{file}");
        let cls = c.classify_path(&file, &Entry::file("x").with_size(10_000));
        assert!(
            matches!(cls.safety, Safety::Safe | Safety::Probably),
            "{file}: {:?}",
            cls.safety
        );
    }
    let win_temp = r"E:\WINNT\Temp\cab_1234.tmp";
    assert!(never.check_str(win_temp).is_ok());
    let cls = c.classify_path(win_temp, &Entry::file("x").with_size(10_000));
    assert!(matches!(cls.safety, Safety::Safe | Safety::Probably));
    // The relocated Windows directory itself stays protected inside.
    assert!(never.check_str(r"E:\WINNT\System32\kernel32.dll").is_err());
    assert!(never.check_str(r"C:\Windows\System32\kernel32.dll").is_ok());
}

#[test]
fn user_data_inside_localized_folders_is_never_safe() {
    let (c, _, kf) = setup();
    for u in &kf.users {
        for folder in [KnownFolder::Documents, KnownFolder::Desktop] {
            let Some(root) = u.folders.get(&folder) else {
                continue;
            };
            let file = format!(r"{}\Bericht 2026.docx", root.display());
            let cls = c.classify_path(&file, &Entry::file("x").with_size(50_000));
            assert!(
                !matches!(cls.safety, Safety::Safe | Safety::Probably),
                "{file}: {:?}",
                cls.safety
            );
        }
    }
}
