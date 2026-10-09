//! Privileged deletes through the helper's pipe, unelevated, on files this
//! test creates in its own scratch directory. Protected objects are only
//! ever *requested*; the helper must refuse them before touching anything.

mod common;

use std::os::windows::fs::MetadataExt;
use std::path::Path;

use common::{HelperSetup, TestDir, TestHelper};
use strata_core::{FileRef, FileTime};
use strata_helper::client::{ClientError, HelperClient};
use strata_ipc::protocol::{AuditOp, AuditPhase, DeleteRequest, ErrorCode, RebootDeleteRequest};

/// Identity of a file as the scan would have reported it.
struct Facts {
    file_ref: FileRef,
    size: u64,
    mtime: FileTime,
    is_dir: bool,
}

fn facts(p: &Path) -> Facts {
    let m = std::fs::metadata(p).unwrap();
    Facts {
        file_ref: FileRef(file_index(p)),
        size: if m.is_dir() { 0 } else { m.len() },
        mtime: FileTime(m.last_write_time()),
        is_dir: m.is_dir(),
    }
}

fn file_index(p: &Path) -> u64 {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_FLAG_BACKUP_SEMANTICS so directories open too; read-attributes
    // only (0x80).
    let f = std::fs::OpenOptions::new()
        .access_mode(0x80)
        .custom_flags(0x0200_0000)
        .open(p)
        .unwrap();
    let mut info = windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION::default();
    use std::os::windows::io::AsRawHandle;
    // SAFETY: a live handle and a writable out-struct.
    unsafe {
        windows::Win32::Storage::FileSystem::GetFileInformationByHandle(
            windows::Win32::Foundation::HANDLE(f.as_raw_handle()),
            &mut info,
        )
    }
    .unwrap();
    (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)
}

fn request(p: &Path) -> DeleteRequest {
    let f = facts(p);
    let s = p.display().to_string();
    DeleteRequest {
        volume: s[..2].to_string(),
        file_ref: f.file_ref,
        expected_path: s.encode_utf16().collect(),
        expected_size: f.size,
        expected_mtime: f.mtime,
        is_dir: f.is_dir,
    }
}

fn remote(e: ClientError) -> (ErrorCode, Vec<AuditPhase>) {
    match e {
        ClientError::Remote(r) => {
            assert!(
                r.audit
                    .iter()
                    .all(|a| a.op == AuditOp::PrivilegedDelete || a.op == AuditOp::DeleteOnReboot)
            );
            (r.code, r.audit.iter().map(|a| a.phase).collect())
        }
        other => panic!("expected a helper error, got {other:?}"),
    }
}

fn client(helper: &TestHelper) -> HelperClient {
    helper.connect()
}

#[test]
fn identity_mismatch_is_refused_and_a_match_deletes() {
    let dir = TestDir::new("del");
    let helper = TestHelper::start(HelperSetup::default());
    let c = client(&helper);
    let f = dir.file("victim.tmp", b"abc");
    let good = request(&f);

    let mut wrong_size = good.clone();
    wrong_size.expected_size = 99;
    let (code, phases) = remote(c.privileged_delete(wrong_size).unwrap_err());
    assert_eq!(code, ErrorCode::Mismatch);
    assert_eq!(phases, [AuditPhase::Started, AuditPhase::Refused]);
    assert!(f.exists());

    let mut wrong_time = good.clone();
    wrong_time.expected_mtime = FileTime(good.expected_mtime.0 - 10_000_000);
    assert_eq!(
        remote(c.privileged_delete(wrong_time).unwrap_err()).0,
        ErrorCode::Mismatch
    );
    assert!(f.exists());

    // Another file's id with this file's path: the by-id object is not at
    // the path the user approved.
    let other = dir.file("other.tmp", b"abc");
    let mut wrong_id = good.clone();
    wrong_id.file_ref = request(&other).file_ref;
    let (code, _) = remote(c.privileged_delete(wrong_id).unwrap_err());
    assert_eq!(code, ErrorCode::Mismatch);
    assert!(f.exists() && other.exists());

    let mut wrong_path = good.clone();
    wrong_path.expected_path = other.display().to_string().encode_utf16().collect();
    assert!(c.privileged_delete(wrong_path).is_err());
    assert!(f.exists() && other.exists());

    let done = c.privileged_delete(good.clone()).unwrap();
    assert_eq!(done.value.files, 1);
    assert_eq!(done.value.bytes, 3);
    assert_eq!(
        done.audit.iter().map(|a| a.phase).collect::<Vec<_>>(),
        [AuditPhase::Started, AuditPhase::Succeeded]
    );
    assert!(done.audit.windows(2).all(|w| w[0].seq < w[1].seq));
    assert!(
        done.audit
            .iter()
            .all(|a| a.file_ref == Some(good.file_ref) && a.client_pid == std::process::id())
    );
    assert!(!f.exists());
    assert!(other.exists());

    // Gone now: opening the stale id fails, so nothing else can be hit.
    // NOTE: strata-clean reports the by-id open of a freed id as an OS
    // error (ERROR_INVALID_PARAMETER), which maps to Io.
    let (code, _) = remote(c.privileged_delete(good).unwrap_err());
    assert!(
        matches!(
            code,
            ErrorCode::NotFound | ErrorCode::Mismatch | ErrorCode::Io
        ),
        "{code:?}"
    );
    assert!(other.exists());
}

#[test]
fn a_directory_is_deleted_recursively_by_id() {
    let dir = TestDir::new("deldir");
    let helper = TestHelper::start(HelperSetup::default());
    let c = client(&helper);
    let d = dir.path.join("tree");
    std::fs::create_dir_all(d.join("a")).unwrap();
    std::fs::write(d.join("a").join("x.bin"), b"12345").unwrap();
    std::fs::write(d.join("y.bin"), b"1").unwrap();
    let done = c.privileged_delete(request(&d)).unwrap();
    assert_eq!((done.value.files, done.value.dirs), (2, 2));
    assert!(!d.exists());
}

#[test]
fn the_never_list_is_enforced_by_the_helper() {
    let dir = TestDir::new("never");
    let helper = TestHelper::start(HelperSetup::default());
    let c = client(&helper);

    // Real protected files with their real ids. Files only: no request in
    // this suite names a protected folder by its real id; folders are
    // covered by path claims below.
    for target in [
        r"C:\Windows\System32\drivers\etc\hosts",
        r"C:\Windows\explorer.exe",
    ] {
        let p = Path::new(target);
        if !p.exists() {
            continue;
        }
        let (code, phases) = remote(c.privileged_delete(request(p)).unwrap_err());
        assert_eq!(code, ErrorCode::Protected, "{target}");
        assert_eq!(
            phases,
            [AuditPhase::Started, AuditPhase::Refused],
            "{target}"
        );
        assert!(p.exists());
    }

    // A harmless file's id with a protected path claim: refused by the
    // helper's own check before any lookup.
    let f = dir.file("harmless.tmp", b"h");
    let mut claim = request(&f);
    claim.expected_path = r"C:\Windows\System32\kernel32.dll".encode_utf16().collect();
    claim.volume = "C:".into();
    assert_eq!(
        remote(c.privileged_delete(claim).unwrap_err()).0,
        ErrorCode::Protected
    );
    for folder in [r"C:\Users", r"C:\Windows\System32", r"C:\Program Files"] {
        let mut claim = request(&f);
        claim.expected_path = folder.encode_utf16().collect();
        claim.is_dir = true;
        claim.volume = "C:".into();
        assert_eq!(
            remote(c.privileged_delete(claim).unwrap_err()).0,
            ErrorCode::Protected,
            "{folder}"
        );
    }
    assert!(f.exists());

    // The id of a protected object with a harmless path claim: the handle
    // resolves into Windows and is refused.
    let hosts = Path::new(r"C:\Windows\System32\drivers\etc\hosts");
    if hosts.exists() {
        let mut sneaky = request(hosts);
        sneaky.expected_path = f.display().to_string().encode_utf16().collect();
        let (code, _) = remote(c.privileged_delete(sneaky).unwrap_err());
        assert!(
            matches!(code, ErrorCode::Protected | ErrorCode::Mismatch),
            "{code:?}"
        );
        assert!(hosts.exists() && f.exists());
    }

    // Malformed requests never reach the file system.
    let mut bad_volume = request(&f);
    bad_volume.volume = r"C:\Windows".into();
    assert_eq!(
        remote(c.privileged_delete(bad_volume).unwrap_err()).0,
        ErrorCode::BadRequest
    );
    let mut surrogate = request(&f);
    surrogate.expected_path = vec![0xD800, 0x41];
    assert_eq!(
        remote(c.privileged_delete(surrogate).unwrap_err()).0,
        ErrorCode::BadRequest
    );
    assert!(f.exists());
}

#[test]
fn delete_on_reboot_is_validated_by_the_helper() {
    let dir = TestDir::new("reboot");
    let helper = TestHelper::start(HelperSetup::default());
    let c = client(&helper);
    let reboot = |p: &Path| {
        let r = request(p);
        RebootDeleteRequest {
            file_ref: r.file_ref,
            expected_path: r.expected_path,
            expected_size: r.expected_size,
            expected_mtime: r.expected_mtime,
        }
    };
    let hosts = Path::new(r"C:\Windows\System32\drivers\etc\hosts");
    if hosts.exists() {
        let (code, phases) = remote(c.delete_on_reboot(reboot(hosts)).unwrap_err());
        assert_eq!(code, ErrorCode::Protected);
        assert_eq!(phases, [AuditPhase::Started, AuditPhase::Refused]);
    }
    let sub = dir.path.join("folder");
    std::fs::create_dir(&sub).unwrap();
    let (code, _) = remote(c.delete_on_reboot(reboot(&sub)).unwrap_err());
    assert!(
        matches!(code, ErrorCode::Protected | ErrorCode::Mismatch),
        "directories are never scheduled: {code:?}"
    );
    assert!(sub.exists());
    let f = dir.file("stale.tmp", b"s");
    let mut stale = reboot(&f);
    stale.expected_size = 42;
    assert_eq!(
        remote(c.delete_on_reboot(stale).unwrap_err()).0,
        ErrorCode::Mismatch
    );
    if !strata_win::process::is_elevated().unwrap_or(false) {
        // Scheduling writes PendingFileRenameOperations (HKLM): refused
        // without elevation, after every validation passed.
        let (code, phases) = remote(c.delete_on_reboot(reboot(&f)).unwrap_err());
        assert_eq!(code, ErrorCode::AccessDenied);
        assert_eq!(phases, [AuditPhase::Started, AuditPhase::Failed]);
    }
    assert!(f.exists());
}
