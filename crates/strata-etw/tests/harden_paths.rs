//! NT-to-DOS path mapping and directory interning with unusual input:
//! verbatim UNC prefixes, unpaired surrogates, astral characters, very long
//! paths, device-number prefixes and volume roots.

use strata_etw::paths::PathMapper;
use strata_win::path::DeviceMap;

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn mapper() -> PathMapper {
    PathMapper::new(DeviceMap::from_entries([
        (r"\Device\HarddiskVolume3".to_owned(), r"C:\".into()),
        (r"\Device\HarddiskVolume30".to_owned(), r"E:\".into()),
        (r"\Device\HarddiskVolume7".to_owned(), r"C:\mnt\data".into()),
    ]))
}

#[test]
fn verbatim_unc_prefixes_become_unc_paths() {
    let m = mapper();
    assert_eq!(
        m.to_dos(&w(r"\??\UNC\srv\share\a.txt")),
        r"\\srv\share\a.txt"
    );
    assert_eq!(
        m.to_dos(&w(r"\\?\UNC\srv\share\a.txt")),
        r"\\srv\share\a.txt"
    );
    assert_eq!(m.to_dos(&w(r"\??\unc\srv\share")), r"\\srv\share");
    assert_eq!(m.to_dos(&w(r"\??\C:\x")), r"C:\x");
    assert_eq!(
        m.to_dos(&w(r"\Device\Mup\srv\share\a.txt")),
        r"\\srv\share\a.txt"
    );
}

#[test]
fn device_numbers_never_match_by_prefix() {
    let m = mapper();
    assert_eq!(m.to_dos(&w(r"\Device\HarddiskVolume3\a")), r"C:\a");
    assert_eq!(m.to_dos(&w(r"\Device\HarddiskVolume30\a")), r"E:\a");
    assert_eq!(
        m.to_dos(&w(r"\Device\HarddiskVolume7\models\x")),
        r"C:\mnt\data\models\x"
    );
    assert_eq!(
        m.to_dos(&w(r"\Device\HarddiskVolume300\a")),
        r"\Device\HarddiskVolume300\a"
    );
    assert_eq!(m.to_dos(&w(r"\Device\HarddiskVolume3")), r"C:\");
}

#[test]
fn unicode_and_long_paths_survive() {
    let m = mapper();
    let mut nt = w(r"\Device\HarddiskVolume3\Ærø\🦀\");
    nt.push(0xD800);
    nt.extend(w("x.txt"));
    let dos = m.to_dos(&nt);
    assert_eq!(dos, "C:\\Ærø\\🦀\\\u{FFFD}x.txt");

    let deep: String = (0..5000).map(|i| format!(r"\dir{i}")).collect();
    let nt = w(&format!(r"\Device\HarddiskVolume3{deep}\leaf.bin"));
    assert!(nt.len() > 32_767);
    let dos = m.to_dos(&nt);
    assert!(dos.starts_with(r"C:\dir0\dir1\"));
    assert!(dos.ends_with(r"\dir4999\leaf.bin"));
}

#[test]
fn directory_interning_handles_roots_and_multibyte_names() {
    let mut m = mapper();
    let a = m.file_from_dos(r"C:\a.txt".into());
    assert_eq!(&*a.dir.0.path, r"C:\");
    let b = m.file_from_dos("C:\\Ærø\\b.txt".into());
    assert_eq!(&*b.dir.0.path, "C:\\Ærø");
    let c = m.file_from_dos("C:\\Ærø\\c.txt".into());
    assert_eq!(b.dir, c.dir);
    let unc = m.file(&w(r"\??\UNC\srv\share\d.txt"));
    assert_eq!(&*unc.path, r"\\srv\share\d.txt");
    assert_eq!(&*unc.dir.0.path, r"\\srv\share");
    let odd = m.file_from_dos("🦀".into());
    assert_eq!(&*odd.dir.0.path, "🦀");
    drop((a, b, c, unc, odd));
    m.sweep();
    assert_eq!(m.interned_dirs(), 0);
}
