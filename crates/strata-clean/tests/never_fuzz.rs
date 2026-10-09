//! Fuzzes path spellings against the canonicalizer and never-list: no
//! spelling of a protected path may ever be allowed.

mod common;

use proptest::prelude::*;

const PROTECTED: &[&str] = &[
    r"C:\",
    r"C:\Windows",
    r"C:\Windows\System32",
    r"C:\Windows\System32\drivers\etc\hosts",
    r"C:\Windows\WinSxS\amd64_x",
    r"C:\Windows\Temp",
    r"C:\Windows\SoftwareDistribution\Download",
    r"C:\Program Files",
    r"C:\Program Files (x86)",
    r"C:\Program Files (Arm)",
    r"C:\Program Files\Strata\strata.exe",
    r"C:\Program Files\WindowsApps\pkg\a.dll",
    r"C:\Users",
    r"C:\Users\me",
    r"C:\Users\someone",
    r"C:\Users\me\AppData",
    r"C:\Users\me\AppData\Local",
    r"C:\Users\me\AppData\LocalLow",
    r"C:\Users\me\AppData\Roaming",
    r"C:\Users\me\Desktop",
    r"D:\Dokumente",
    r"D:\",
    r"C:\ProgramData",
    r"C:\$Recycle.Bin\S-1-5-21\$RABC.txt",
    r"C:\System Volume Information",
    r"C:\pagefile.sys",
    r"E:\hiberfil.sys",
    r"C:\Boot\BCD",
    r"C:\EFI\Microsoft",
    r"C:\$MFT",
    r"C:\$Extend\$UsnJrnl",
];

fn components(p: &str) -> (char, Vec<String>) {
    let drive = p.chars().next().unwrap();
    let rest = &p[3..];
    let comps = rest
        .split('\\')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    (drive, comps)
}

#[derive(Debug, Clone)]
struct Spelling {
    prefix: usize,
    drive_lower: bool,
    case_flips: Vec<bool>,
    seps: Vec<usize>,
    detours: Vec<Option<String>>,
    trailing: Vec<usize>,
    final_sep: bool,
    admin_share: bool,
}

fn spelling(n: usize) -> impl Strategy<Value = Spelling> {
    (
        0usize..5,
        any::<bool>(),
        prop::collection::vec(any::<bool>(), 64),
        prop::collection::vec(0usize..6, n + 1),
        prop::collection::vec(prop::option::weighted(0.2, "[a-zA-Z0-9 _-]{1,8}"), n + 1),
        prop::collection::vec(0usize..5, n + 1),
        any::<bool>(),
        prop::bool::weighted(0.1),
    )
        .prop_map(
            |(prefix, drive_lower, case_flips, seps, detours, trailing, final_sep, admin_share)| {
                Spelling {
                    prefix,
                    drive_lower,
                    case_flips,
                    seps,
                    detours,
                    trailing,
                    final_sep,
                    admin_share,
                }
            },
        )
}

const SEPS: [&str; 6] = ["\\", "/", "\\\\", "/\\", "\\.\\", "\\./"];
const TRAILING: [&str; 5] = ["", ".", " ", ". .", ".."];
const PREFIXES: [&str; 5] = ["", r"\\?\", r"\\.\", r"\??\", "//?/"];

fn render(target: &str, s: &Spelling) -> String {
    let (drive, comps) = components(target);
    let mut out = String::new();
    let drive = if s.drive_lower {
        drive.to_ascii_lowercase()
    } else {
        drive
    };
    if s.admin_share {
        out.push_str(&format!(r"\\Server\{drive}$"));
    } else {
        out.push_str(PREFIXES[s.prefix]);
        out.push(drive);
        out.push(':');
    }
    let mut flip = s.case_flips.iter().cycle();
    for (i, c) in comps.iter().enumerate() {
        out.push_str(SEPS[s.seps[i]]);
        if let Some(d) = &s.detours[i] {
            out.push_str(d);
            out.push_str("\\..\\");
        }
        let cased: String = c
            .chars()
            .map(|ch| {
                if *flip.next().unwrap() {
                    if ch.is_lowercase() {
                        ch.to_ascii_uppercase()
                    } else {
                        ch.to_ascii_lowercase()
                    }
                } else {
                    ch
                }
            })
            .collect();
        out.push_str(&cased);
        out.push_str(TRAILING[s.trailing[i]]);
    }
    if comps.is_empty() || s.final_sep {
        out.push('\\');
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn no_spelling_of_a_protected_path_is_allowed(
        idx in 0..PROTECTED.len(),
        s in spelling(8),
    ) {
        let list = common::synthetic_list();
        let target = PROTECTED[idx];
        let spelled = render(target, &s);
        prop_assert!(
            list.check_str(&spelled).is_err(),
            "{} spelled as {} was allowed",
            target,
            spelled
        );
    }

    #[test]
    fn arbitrary_strings_never_panic(s in "\\PC{0,80}") {
        let list = common::synthetic_list();
        let _ = list.check_str(&s);
    }

    #[test]
    fn random_paths_under_protected_roots_are_refused(
        root in prop::sample::select(vec![
            r"C:\Windows\System32",
            r"C:\Windows\WinSxS",
            r"C:\Program Files\WindowsApps",
            r"C:\Program Files\Strata",
            r"C:\$Recycle.Bin",
            r"C:\System Volume Information",
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\Windows",
            r"\Device\HarddiskVolume77\Windows",
        ]),
        tail in prop::collection::vec("[a-zA-Z0-9_-][a-zA-Z0-9 ._-]{0,11}", 0..5),
    ) {
        let list = common::synthetic_list();
        let p = format!("{root}\\{}", tail.join("\\"));
        prop_assert!(list.check_str(&p).is_err(), "{} was allowed", p);
    }

    #[test]
    fn ordinary_data_paths_are_allowed(
        tail in prop::collection::vec("[a-zA-Z0-9_-]{1,12}", 1..5),
    ) {
        let list = common::synthetic_list();
        let p = format!(r"D:\scratch\{}", tail.join("\\"));
        prop_assert!(list.check_str(&p).is_ok(), "{} was refused", p);
        let t = format!(r"C:\Windows\Temp\{}", tail.join("\\"));
        prop_assert!(list.check_str(&t).is_ok(), "{} was refused", t);
    }
}
