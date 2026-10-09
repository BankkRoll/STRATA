//! Path helpers with Unicode and long paths, and this machine's known
//! folders as the never-list consumes them. Nothing machine-specific is
//! asserted beyond structure.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::Path;

use strata_win::path::{DeviceMap, eq_ignore_case, strip_verbatim, to_verbatim};

#[test]
fn ordinal_case_folding_matches_ntfs() {
    for (a, b, eq) in [
        ("ÄRGER.txt", "ärger.TXT", true),
        ("Σ", "σ", true),
        ("ß", "SS", false),
        ("ı", "I", false),
        ("İ", "i", false),
        ("🦀", "🦀", true),
        ("a", "a ", false),
    ] {
        assert_eq!(eq_ignore_case(OsStr::new(a), OsStr::new(b)), eq, "{a} {b}");
    }
    let lone = OsString::from_wide(&[0x61, 0xD800]);
    let lone_upper = OsString::from_wide(&[0x41, 0xD800]);
    assert!(eq_ignore_case(&lone, &lone_upper));
}

#[test]
fn verbatim_round_trips_long_unicode_paths() {
    let deep: String = (0..3000).map(|i| format!(r"\Ærø🦀{i}")).collect();
    let p = format!(r"D:{deep}\leaf.bin");
    let len = p.encode_utf16().count();
    assert!(len > 20_000 && len < 32_000, "{len}");
    let v = to_verbatim(Path::new(&p)).unwrap();
    assert!(v.to_string_lossy().starts_with(r"\\?\D:\Ærø🦀0\"));
    assert_eq!(strip_verbatim(&v), Path::new(&p));

    let unc = r"\\srv\Freigabe\Документы\x";
    let v = to_verbatim(Path::new(unc)).unwrap();
    assert_eq!(v, Path::new(r"\\?\UNC\srv\Freigabe\Документы\x"));
    assert_eq!(strip_verbatim(&v), Path::new(unc));
}

#[test]
fn device_map_keeps_unicode_and_surrogates() {
    let map = DeviceMap::from_entries([(
        r"\Device\HarddiskVolume4".to_owned(),
        r"C:\mnt\Données".into(),
    )]);
    let mut nt: Vec<u16> = r"\Device\HarddiskVolume4\🦀\".encode_utf16().collect();
    nt.push(0xDC00);
    let dos = map.to_dos(&OsString::from_wide(&nt)).unwrap();
    let mut want: Vec<u16> = r"C:\mnt\Données\🦀\".encode_utf16().collect();
    want.push(0xDC00);
    use std::os::windows::ffi::OsStrExt;
    assert_eq!(dos.as_os_str().encode_wide().collect::<Vec<_>>(), want);
}

#[test]
fn this_machines_known_folders_are_absolute_and_usable() {
    let kf = strata_win::known::known_folders().unwrap();
    assert!(!kf.users.is_empty());
    assert_eq!(kf.users.iter().filter(|u| u.is_current).count(), 1);
    for p in kf.machine.values() {
        assert!(p.is_absolute(), "{}", p.display());
    }
    for u in &kf.users {
        for p in u.folders.values() {
            assert!(p.is_absolute(), "{}", p.display());
            assert!(
                !p.to_string_lossy().contains('%'),
                "unexpanded {}",
                p.display()
            );
        }
    }
}
