//! Never-list bypass attempts: loopback UNC spellings, default shares onto
//! the Windows directory, very long verbatim paths, trailing dots and
//! spaces, reserved device names, unpaired surrogates and case variants.

mod common;
mod harden_support;

use std::path::Path;

use common::{guard, synthetic_list};
use harden_support::{HardenDir, lan_address, via_admin_share};
use strata_clean::CleanError;
use strata_clean::never::{NeverList, RefusalReason};

fn refused(l: &NeverList, s: &str) -> RefusalReason {
    match l.check_str(s) {
        Ok(c) => panic!("{s:?} was allowed as {c}"),
        Err(r) => r.reason,
    }
}

fn allowed(l: &NeverList, s: &str) {
    if let Err(r) = l.check_str(s) {
        panic!("{s:?} was refused: {}", r.message());
    }
}

#[test]
fn loopback_unc_spellings_are_refused() {
    let l = synthetic_list();
    for host in [
        "localhost",
        "LOCALHOST.",
        "localhost..",
        "foo.localhost",
        "127.0.0.1",
        "127.1",
        "127.0.1",
        "127.255.255.254",
        "2130706433",
        "0x7f000001",
        "0x7f.1",
        "0X7F.0.0.1",
        "0177.0.0.1",
        "017700000001",
        "0",
        "0.0.0.0",
        "[::1]",
        "::1",
        "[0:0:0:0:0:0:0:1]",
        "0--1.ipv6-literal.net",
        "0--1.IPV6-LITERAL.NET",
        "0--1s1.ipv6-literal.net",
        "--1.ipv6-literal.net",
        "[::ffff:127.0.0.1]",
        "[::ffff:7f00:1]",
        "[::]",
    ] {
        for s in [
            format!(r"\\{host}\share\x"),
            format!(r"\\?\UNC\{host}\share\x"),
            format!(r"\\{host}\C$\Users\me\notes.txt"),
        ] {
            let reason = refused(&l, &s);
            // A literal IPv6 server contains `:`, which is refused as a
            // stream name before the host is even considered.
            let colon_refusal =
                host.contains(':') && matches!(reason, RefusalReason::InvalidPath { .. });
            assert!(
                reason == RefusalReason::LoopbackShare || colon_refusal,
                "{s}: {reason:?}"
            );
        }
    }
}

#[test]
fn configured_host_names_match_with_trailing_dots_and_domains() {
    let mut cfg = strata_clean::never::NeverListConfig {
        local_hosts: vec!["DEVBOX".into(), "devbox.corp.example".into()],
        ..Default::default()
    };
    cfg.known = {
        let mut k = strata_core::known::KnownFolders::default();
        use strata_core::known::KnownFolder;
        k.machine.insert(KnownFolder::Windir, r"C:\Windows".into());
        k.machine
            .insert(KnownFolder::ProgramFiles, r"C:\Program Files".into());
        k.machine
            .insert(KnownFolder::UserProfiles, r"C:\Users".into());
        k
    };
    let l = NeverList::new(cfg).unwrap();
    for s in [
        r"\\devbox\data\x",
        r"\\DEVBOX.\data\x",
        r"\\devbox.corp.example.\data\x",
        r"\\devbox.other.example\data\x",
        r"\\?\UNC\DevBox\data\x",
    ] {
        assert_eq!(refused(&l, s), RefusalReason::LoopbackShare, "{s}");
    }
    allowed(&l, r"\\devbox2\data\x");
    allowed(&l, r"\\fileserver.corp.example\data\x");
}

#[test]
fn remote_hosts_are_not_mistaken_for_loopback() {
    let l = synthetic_list();
    for s in [
        r"\\fileserver\public\old.iso",
        r"\\10.0.0.5\share\x",
        r"\\128.0.0.1\share\x",
        r"\\126.255.255.255\share\x",
        r"\\0x80.1\share\x",
        r"\\fe80--2.ipv6-literal.net\share\x",
        r"\\localhostx\share\x",
        r"\\1.2.3.4.5\share\x",
    ] {
        allowed(&l, s);
    }
}

#[test]
fn default_shares_map_onto_the_windows_directory() {
    let l = synthetic_list();
    for s in [
        r"\\fileserver\ADMIN$",
        r"\\fileserver\admin$\System32",
        r"\\fileserver\ADMIN$\System32\drivers\etc\hosts",
        r"\\?\UNC\fileserver\Admin$\WinSxS\x\y.dll",
        r"\\fileserver\PRINT$\x64\3\driver.dll",
        r"\\fileserver\print$",
    ] {
        assert!(
            matches!(refused(&l, s), RefusalReason::Protected { .. }),
            "{s}"
        );
    }
    // The Windows directory's own allow-list still applies.
    allowed(&l, r"\\fileserver\ADMIN$\Temp\setup.log");
    allowed(
        &l,
        r"\\fileserver\ADMIN$\SoftwareDistribution\Download\x.cab",
    );
    allowed(&l, r"\\fileserver\IPCX$\x");
}

#[test]
fn long_verbatim_paths_inside_protected_trees_are_refused() {
    let l = synthetic_list();
    let deep = "abcdefghij\\".repeat(3_000);
    for base in [
        r"\\?\C:\Windows\",
        r"\\?\c:\windows\System32\",
        r"\??\C:\Program Files\WindowsApps\",
        r"\\?\UNC\fileserver\C$\Windows\",
    ] {
        let s = format!("{base}{deep}leaf.dll");
        assert!(s.len() > 32_767);
        assert!(
            matches!(refused(&l, &s), RefusalReason::Protected { .. }),
            "{base}"
        );
    }
    allowed(&l, &format!(r"\\?\D:\scratch\{deep}leaf.bin"));
}

#[test]
fn trailing_dots_and_spaces_never_unprotect() {
    let l = synthetic_list();
    for s in [
        r"C:\Windows.",
        r"C:\Windows. . .",
        r"C:\Windows .\System32",
        r"C:\Windows.\System32\x.dll",
        r"\\?\C:\Windows.",
        r"\\?\C:\Windows \System32",
        r"\\?\C:\Windows. \System32\x",
        r"C:\Program Files .",
        r"\\?\C:\Program Files.",
        r"C:\Users\me .",
        r"\\?\C:\Users\me.",
        r"C:\$Recycle.Bin.\x",
        r"\\?\C:\pagefile.sys.",
    ] {
        assert!(
            matches!(refused(&l, s), RefusalReason::Protected { .. }),
            "{s}"
        );
    }
}

#[test]
fn reserved_device_names_are_refused_outside_verbatim_paths() {
    let l = synthetic_list();
    for s in [
        r"D:\scratch\CON",
        r"D:\scratch\nul.txt",
        r"D:\scratch\AUX .log",
        r"D:\scratch\COM1",
        "D:\\scratch\\COM\u{00B9}",
        "D:\\scratch\\lpt\u{00B3}.txt",
        r"D:\scratch\CONIN$",
        r"D:\scratch\con\x",
    ] {
        assert!(
            matches!(refused(&l, s), RefusalReason::InvalidPath { .. }),
            "{s}"
        );
    }
    // Verbatim paths name the NTFS file literally, not the device.
    allowed(&l, r"\\?\D:\scratch\CON");
    // Device names are refused before any rule could be consulted.
    assert!(matches!(
        refused(&l, r"C:\Windows\NUL"),
        RefusalReason::InvalidPath { .. }
    ));
}

#[test]
fn unpaired_surrogates_do_not_escape_protection() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let l = synthetic_list();
    let wide = |s: &str, tail: &[u16]| {
        let mut w: Vec<u16> = s.encode_utf16().collect();
        w.extend_from_slice(tail);
        OsString::from_wide(&w)
    };
    for tail in [
        &[0xD800u16][..],
        &[0xDFFF],
        &[0xDC00, 0xD800],
        &[0x41, 0xDBFF],
    ] {
        let inside = wide(r"C:\Windows\System32\", tail);
        assert!(l.check_str(&inside).is_err(), "{inside:?}");
        let child = wide(r"C:\Users\", tail);
        assert!(l.check_str(&child).is_err(), "profile {child:?}");
        let ok = wide(r"D:\scratch\", tail);
        assert!(l.check_str(&ok).is_ok(), "{ok:?}");
    }
}

#[test]
fn case_variants_fold_like_ntfs() {
    let l = synthetic_list();
    for s in [
        r"c:\WINDOWS\system32",
        r"C:\wInDoWs\SyStEm32\x",
        "C:\\w\u{0131}ndows\\System32",
        r"c:\program files (X86)",
        r"c:\USERS\ME\appdata\LOCAL",
        r"d:\DOKUMENTE",
    ] {
        assert!(
            matches!(refused(&l, s), RefusalReason::Protected { .. }),
            "{s}"
        );
    }
}

// -----------------------------------------------------------------------------
// Through the guard, on real files
// -----------------------------------------------------------------------------

fn assert_loopback(p: &Path) {
    match guard().check_path(p) {
        Err(CleanError::Refused { refusal }) => {
            assert_eq!(
                refusal.reason,
                RefusalReason::LoopbackShare,
                "{}",
                p.display()
            );
        }
        other => panic!("{}: {other:?}", p.display()),
    }
}

#[test]
fn guard_refuses_loopback_spellings_of_a_real_file() {
    let t = HardenDir::new("never-loop");
    let f = t.file("victim.txt", b"keep me");
    for host in [
        "localhost.",
        "127.1",
        "2130706433",
        "0x7f.1",
        "0--1.ipv6-literal.net",
    ] {
        assert_loopback(&via_admin_share(host, &f));
    }
    assert!(f.exists());
}

#[test]
fn guard_refuses_a_share_of_this_machine_reached_by_lan_address() {
    let Some(ip) = lan_address() else {
        eprintln!("skipped: no LAN address");
        return;
    };
    let host = match ip {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => format!(
            "{}.ipv6-literal.net",
            v6.to_string().replace(':', "-").replace('%', "s")
        ),
    };
    let t = HardenDir::new("never-lan");
    let f = t.file("victim.txt", b"keep me");
    let unc = via_admin_share(&host, &f);
    if std::fs::metadata(&unc).is_err() {
        eprintln!("skipped: {} is not reachable", unc.display());
        return;
    }
    assert_loopback(&unc);
    let hosts = format!(r"\\{host}\ADMIN$\System32\drivers\etc\hosts");
    assert!(matches!(
        guard().check_path(Path::new(&hosts)),
        Err(CleanError::Refused { .. })
    ));
    assert!(f.exists());
}

#[test]
fn junction_cycles_resolve_without_looping_and_delete_only_the_link() {
    let t = HardenDir::new("never-cycle");
    let outer = t.dir("outer");
    let keep = t.file(r"outer\keep.txt", b"keep");
    let lp = outer.join("loop");
    common::junction(&lp, &outer);
    // Twenty trips round the cycle still name `outer\keep.txt`.
    let mut deep = lp.clone();
    for _ in 0..20 {
        deep.push("loop");
    }
    let checked = guard().check_path(&deep.join("keep.txt")).unwrap();
    assert!(checked.resolved.to_string().ends_with(r"outer\keep.txt"));

    // Deleting the folder that holds the cycle unlinks the junction without
    // walking it, so the walk terminates and each object goes exactly once.
    let item = guard().check_path(&outer).unwrap();
    let stats = strata_clean::permanent::delete_permanently(
        guard(),
        &outer,
        &strata_clean::Expected::from_facts(&item.facts),
        &strata_clean::CancelToken::new(),
    )
    .unwrap();
    assert!(!outer.exists());
    assert!(!keep.exists());
    assert_eq!((stats.files, stats.dirs, stats.links), (1, 1, 1));
}

#[test]
fn symlink_cycles_are_dangling_links_not_loops() {
    let t = HardenDir::new("never-symcycle");
    let a = t.path.join("a");
    let b = t.path.join("b");
    // Symlinks need Developer Mode or elevation; skip without them.
    if std::os::windows::fs::symlink_dir(&b, &a).is_err() {
        eprintln!("skipped: cannot create symlinks");
        return;
    }
    std::os::windows::fs::symlink_dir(&a, &b).unwrap();
    for p in [&a, &b] {
        assert!(guard().check_path(p).unwrap().facts.is_reparse());
    }
    let item = guard().check_path(&a).unwrap();
    strata_clean::permanent::delete_permanently(
        guard(),
        &a,
        &strata_clean::Expected::from_facts(&item.facts),
        &strata_clean::CancelToken::new(),
    )
    .unwrap();
    assert!(std::fs::symlink_metadata(&a).is_err());
    assert!(std::fs::symlink_metadata(&b).is_ok());
}
