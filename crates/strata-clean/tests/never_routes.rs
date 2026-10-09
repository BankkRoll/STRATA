//! Safety tests: never-list paths are refused by every route, even
//! when the classifier wrongly calls them safe and every confirmation is
//! given.

mod common;

use std::path::PathBuf;

use common::guard;
use strata_clean::audit::{ItemOutcome, MemoryAuditLog};
use strata_clean::flow::{
    Acknowledgements, CleanupConfig, Decision, DeleteMethod, Plan, QueueItem, execute, plan,
};
use strata_clean::permanent::delete_permanently;
use strata_clean::privileged::{DelayedDeleteRequest, PrivilegedDeleteRequest};
use strata_clean::recycle::{RecycleItem, recycle};
use strata_clean::{CancelToken, CleanError, Expected};
use strata_core::{FileRef, FileTime, Safety};

fn protected_paths() -> Vec<PathBuf> {
    let env = |v: &str| PathBuf::from(std::env::var_os(v).unwrap());
    let mut v: Vec<PathBuf> = [
        r"C:\",
        r"C:\Windows",
        r"C:\Windows\System32",
        r"C:\Windows\System32\drivers\etc\hosts",
        r"C:\Windows\WinSxS",
        r"C:\Windows\Temp",
        r"C:\Users",
        r"C:\Users\Public",
        r"C:\Program Files",
        r"C:\Program Files (x86)",
        r"C:\ProgramData",
        r"C:\$Recycle.Bin",
        r"C:\System Volume Information",
        r"C:\pagefile.sys",
        r"c:/windows/explorer.exe",
        r"\\?\C:\Windows\System32\config",
        r"\\localhost\C$\Windows",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    v.extend([
        env("USERPROFILE"),
        env("LOCALAPPDATA"),
        env("APPDATA"),
        env("USERPROFILE").join("Desktop"),
        env("USERPROFILE").join("AppData"),
    ]);
    v
}

fn any_expected(is_dir: bool) -> Expected {
    Expected {
        file_ref: FileRef(5),
        is_dir,
        size: 0,
        modified: FileTime(0),
    }
}

fn is_refusal(e: &CleanError) -> bool {
    matches!(e, CleanError::Refused { .. })
}

#[test]
fn permanent_route_refuses() {
    for p in protected_paths() {
        // NOTE: not every machine has every path (CI runners keep the page file
        // on D:), so only paths present beforehand must survive.
        let existed = p.exists();
        for dir in [true, false] {
            let e = delete_permanently(guard(), &p, &any_expected(dir), &CancelToken::new())
                .unwrap_err();
            assert!(is_refusal(&e), "{}: {e:?}", p.display());
        }
        assert!(!existed || p.exists(), "{} vanished", p.display());
    }
}

#[test]
fn recycle_route_refuses() {
    let items: Vec<RecycleItem> = protected_paths()
        .into_iter()
        .map(|path| RecycleItem {
            path,
            expected: any_expected(true),
        })
        .collect();
    for (i, r) in recycle(guard(), &items, &CancelToken::new())
        .into_iter()
        .enumerate()
    {
        let e = r.unwrap_err();
        assert!(is_refusal(&e), "{}: {e:?}", items[i].path.display());
    }
}

#[test]
fn flow_refuses_even_with_every_confirmation() {
    let queue: Vec<QueueItem> = protected_paths()
        .into_iter()
        .enumerate()
        .map(|(i, path)| QueueItem {
            id: i as u64,
            path,
            expected: any_expected(true),
            safety: Safety::Safe,
        })
        .collect();
    let planned = plan(guard(), queue.clone());
    assert!(planned.items.is_empty(), "plan kept {:?}", planned.items);

    // A plan forged without `plan()` still cannot get through `execute`.
    let forged = Plan {
        items: queue,
        totals: Vec::new(),
        volumes: Vec::new(),
        warnings: Vec::new(),
    };
    let all: Acknowledgements = Acknowledgements {
        careful: forged.items.iter().map(|i| i.id).collect(),
        permanent: true,
        large_permanent: true,
        permanent_instead_of_recycle: forged.items.iter().map(|i| i.id).collect(),
    };
    for method in [DeleteMethod::RecycleBin, DeleteMethod::Permanent] {
        let mut log = MemoryAuditLog::new();
        let report = execute(
            guard(),
            &forged,
            &Decision {
                method,
                acks: all.clone(),
            },
            &CleanupConfig::default(),
            &mut log,
            &mut |_| {},
            &CancelToken::new(),
        );
        assert_eq!(report.summary.succeeded, 0);
        for r in &report.results {
            match &r.outcome {
                ItemOutcome::Failed { error } => {
                    assert!(is_refusal(error), "{}: {error:?}", r.path)
                }
                other => panic!("{}: {other:?}", r.path),
            }
        }
    }
}

#[test]
fn helper_requests_refuse() {
    for p in protected_paths() {
        let r = PrivilegedDeleteRequest {
            volume: r"C:\".into(),
            file_ref: FileRef(5),
            expected_path: p.display().to_string(),
            expected_size: 0,
            expected_mtime: FileTime(0),
            is_dir: true,
        };
        assert!(
            matches!(r.validate(guard()), Err(CleanError::Refused { .. })),
            "{}",
            p.display()
        );
        let d = DelayedDeleteRequest {
            path: p.display().to_string(),
            file_ref: FileRef(5),
            expected_size: 0,
            expected_mtime: FileTime(0),
        };
        assert!(
            matches!(d.validate(guard()), Err(CleanError::Refused { .. })),
            "{}",
            p.display()
        );
    }
}
