//! Safety tests: the guard against real protected paths on this machine,
//! through every spelling, junction and namespace we can construct unelevated.

mod common;

use std::path::{Path, PathBuf};

use common::{TestDir, guard, junction};
use strata_clean::CleanError;
use strata_clean::never::{ProtectedKind, RefusalReason, Relation};

fn refusal(path: &str) -> (ProtectedKind, Relation) {
    match guard().check_path(Path::new(path)) {
        Err(CleanError::Refused { refusal }) => match refusal.reason {
            RefusalReason::Protected {
                protected,
                relation,
                ..
            } => (protected, relation),
            other => panic!("{path}: unexpected refusal {other:?}"),
        },
        other => panic!("{path}: expected a refusal, got {other:?}"),
    }
}

fn assert_refused(path: &str) {
    match guard().check_path(Path::new(path)) {
        Err(CleanError::Refused { .. }) => {}
        other => panic!("{path}: expected a refusal, got {other:?}"),
    }
}

fn c_volume() -> (String, String) {
    let map = strata_clean::volume::resolve_volume_map().unwrap();
    let v = map
        .volumes
        .into_iter()
        .find(|v| v.mount_points.iter().any(|m| m.as_os_str() == r"C:\"))
        .unwrap();
    (v.guid, v.device.unwrap())
}

#[test]
fn system_paths_refused_in_every_spelling() {
    let (guid, device) = c_volume();
    let spellings = [
        r"C:\Windows".to_string(),
        r"c:\windows".into(),
        r"C:/Windows/".into(),
        r"C:\\Windows\\".into(),
        r"C:\Windows.".into(),
        r"C:\Windows. . ".into(),
        r"C:\Windows\.".into(),
        r"C:\Users\..\Windows".into(),
        r"\\?\C:\Windows".into(),
        r"\\?\c:\WINDOWS\".into(),
        r"\??\C:\Windows".into(),
        r"\\.\C:\Windows".into(),
        r"//?/C:/Windows".into(),
        r"C:\Windows\System32".into(),
        r"C:\Windows\System32\drivers\etc\hosts".into(),
        r"C:\Windows\System32\kernel32.dll".into(),
        r"C:\WINDOWS\SYSTEM32\CONFIG".into(),
        r"C:\Windows\WinSxS".into(),
        r"C:\Windows\Temp".into(),
        format!(r"\\?\Volume{guid}\Windows"),
        format!(r"\\?\Volume{}\Windows\System32", guid.to_uppercase()),
        format!(r"\\?\GLOBALROOT\Device\{device}\Windows"),
        format!(r"\Device\{device}\Windows\System32"),
        r"C:\".into(),
        r"\\?\C:\".into(),
        format!(r"\\?\Volume{guid}\"),
        r"C:\Users".into(),
        r"C:\Program Files".into(),
        r"C:\Program Files (x86)".into(),
        r"C:\ProgramData".into(),
        r"C:\$Recycle.Bin".into(),
        r"C:\System Volume Information".into(),
        r"C:\Windows\explorer.exe".into(),
        r"C:\Windows\Installer".into(),
    ];
    for s in &spellings {
        assert_refused(s);
    }
}

#[test]
fn profile_and_known_folders_refused() {
    let profile = std::env::var("USERPROFILE").unwrap();
    let local = std::env::var("LOCALAPPDATA").unwrap();
    let roaming = std::env::var("APPDATA").unwrap();
    assert_eq!(
        refusal(&profile),
        (ProtectedKind::UserProfileRoot, Relation::Itself)
    );
    assert_eq!(
        refusal(&profile.to_uppercase()),
        (ProtectedKind::UserProfileRoot, Relation::Itself)
    );
    assert_refused(&local);
    assert_refused(&roaming);
    assert_refused(&format!(r"{profile}\AppData"));
    assert_refused(&format!(r"{profile}\Downloads"));
    assert_refused(&format!(r"{profile}\Desktop"));
    assert_refused(&format!(r"\\?\{profile}"));
    assert_refused(&format!(r"{}\..", local));
}

#[test]
fn short_names_are_expanded() {
    // 8.3 names exist on the system drive for these folders on default
    // installs; when generation is disabled the lookup fails, and the handle
    // check still refuses.
    for s in [
        r"C:\PROGRA~1",
        r"C:\PROGRA~2",
        r"C:\PROGRA~3",
        r"C:\Users\..\PROGRA~1\.\",
    ] {
        if Path::new(s).exists() {
            assert_refused(s);
        }
    }
    let profile = std::env::var("USERPROFILE").unwrap();
    let local_short = format!(r"{profile}\AppData\Local\..\..\AppData");
    assert_refused(&local_short);
}

#[test]
fn junction_into_windows_is_refused() {
    let t = TestDir::new("junction-win");
    let link = t.path.join("link");
    junction(&link, Path::new(r"C:\Windows"));

    // The link itself points at a protected folder.
    assert_eq!(
        refusal(link.to_str().unwrap()),
        (ProtectedKind::WindowsDirectory, Relation::LinkTarget)
    );
    // Anything reached through it is inside C:\Windows.
    for rel in [
        r"System32",
        r"System32\drivers",
        r"System32\kernel32.dll",
        r"WinSxS",
        r"Temp",
        r"System32\..\System32",
    ] {
        let p = format!(r"{}\{rel}", link.display());
        assert_eq!(
            refusal(&p),
            (ProtectedKind::WindowsDirectory, Relation::Inside),
            "{p}"
        );
    }
    // Verbatim and mixed spellings through the link too.
    assert_refused(&format!(r"\\?\{}\System32", link.display()));
    assert_refused(&format!(r"{}/system32/", link.display()).replace('\\', "/"));
}

#[test]
fn junction_chain_into_protected_is_refused() {
    let t = TestDir::new("junction-chain");
    let a = t.path.join("a");
    let b = t.path.join("b");
    junction(&a, Path::new(r"C:\Users"));
    junction(&b, &a);
    let me = std::env::var("USERNAME").unwrap();
    assert_refused(b.to_str().unwrap());
    assert_refused(&format!(r"{}\{me}", b.display()));
    assert_refused(&format!(r"{}\{me}\AppData", b.display()));
}

#[test]
fn junction_to_volume_root_is_refused() {
    let t = TestDir::new("junction-root");
    let link = t.path.join("root");
    junction(&link, Path::new(r"C:\"));
    assert_eq!(
        refusal(link.to_str().unwrap()),
        (ProtectedKind::VolumeRoot, Relation::LinkTarget)
    );
    assert_refused(&format!(r"{}\Windows", link.display()));
    assert_refused(&format!(r"{}\pagefile.sys", link.display()));
}

#[test]
fn symlinks_into_protected_are_refused_when_creatable() {
    let t = TestDir::new("symlink");
    let link = t.path.join("sl");
    // Symlinks need Developer Mode or elevation; skip quietly without them.
    if std::os::windows::fs::symlink_dir(r"C:\Windows", &link).is_err() {
        return;
    }
    assert_refused(link.to_str().unwrap());
    assert_refused(&format!(r"{}\System32", link.display()));
}

#[test]
fn loopback_admin_share_is_refused() {
    for s in [
        r"\\localhost\C$\Windows",
        r"\\127.0.0.1\c$\Windows\System32",
        r"\\?\UNC\localhost\C$\Users",
    ] {
        match guard().check_path(Path::new(s)) {
            Err(CleanError::Refused { .. }) => {}
            other => panic!("{s}: {other:?}"),
        }
    }
    let host = std::env::var("COMPUTERNAME").unwrap();
    assert!(matches!(
        guard().check_path(Path::new(&format!(r"\\{host}\Users\x"))),
        Err(CleanError::Refused { .. })
    ));
}

#[test]
fn streams_and_devices_refused() {
    let t = TestDir::new("ads");
    let f = t.file("a.txt", b"x");
    for s in [
        format!("{}::$DATA", f.display()),
        format!("{}:hidden", f.display()),
        format!(r"{}:$I30:$INDEX_ALLOCATION", t.path.display()),
        format!(r"{}\NUL", t.path.display()),
        r"\\.\PhysicalDrive0".to_string(),
        r"\\.\pipe\x".to_string(),
        r"Windows\System32".to_string(),
    ] {
        match guard().check_path(Path::new(&s)) {
            Err(CleanError::Refused { refusal }) => {
                assert!(
                    matches!(refusal.reason, RefusalReason::InvalidPath { .. }),
                    "{s}"
                );
            }
            other => panic!("{s}: {other:?}"),
        }
    }
}

#[test]
fn ordinary_temp_items_pass_with_facts() {
    let t = TestDir::new("ok");
    let f = t.file(r"sub\data.bin", &[7u8; 1234]);
    let checked = guard().check_path(&f).unwrap();
    assert_eq!(checked.facts.size, 1234);
    assert!(!checked.facts.is_dir());
    assert_eq!(
        checked.resolved.to_string().to_lowercase(),
        f.display().to_string().to_lowercase()
    );
    let d = guard().check_path(&t.path.join("sub")).unwrap();
    assert!(d.facts.is_dir());
    // Same file, different spelling: same identity.
    let alt = PathBuf::from(format!(r"\\?\{}", f.display()).to_uppercase());
    let checked2 = guard().check_path(&alt).unwrap();
    assert_eq!(checked.facts.identity, checked2.facts.identity);
    assert!(matches!(
        guard().check_path(&t.path.join("missing")),
        Err(CleanError::NotFound { .. })
    ));
}

#[test]
fn junction_to_unprotected_dir_is_allowed() {
    let t = TestDir::new("junction-ok");
    let target = t.dir("target");
    std::fs::write(target.join("sentinel.txt"), b"keep").unwrap();
    let link = t.path.join("link");
    junction(&link, &target);
    let c = guard().check_path(&link).unwrap();
    assert!(c.facts.is_reparse());
}
