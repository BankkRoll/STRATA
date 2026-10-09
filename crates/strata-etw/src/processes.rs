//! Process identity: (pid, start time) → image.
//!
//! Windows reuses process ids, so an id alone does not name a process. The
//! table keys every process by (pid, creation time), learned from
//! Kernel-Process start events, and resolves an event's pid against the
//! process that was alive at the event's timestamp.
//!
//! Processes that were already running when tracking started have no start
//! event. The first event from such a pid asks a [`ProcessSource`] (live:
//! `OpenProcess` via [`strata_win::process::ProcessInfo`]); the answer is
//! accepted only if that process started before the event, otherwise the pid
//! has already been reused and the event stays unattributed rather than being
//! blamed on the wrong program.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use strata_core::FileTime;

/// One process.
#[derive(Debug, PartialEq, Eq)]
pub struct ProcInfo {
    /// Process id.
    pub pid: u32,
    /// Creation time.
    pub start: FileTime,
    /// Full image path (DOS path when known).
    pub image: Arc<str>,
}

impl ProcInfo {
    /// The image file name (`app.exe`), used as the attribution label.
    #[must_use]
    pub fn file_name(&self) -> &str {
        self.image.rsplit(['\\', '/']).next().unwrap_or(&self.image)
    }
}

/// Shared handle to a [`ProcInfo`]; equality and hashing by identity, which
/// the table makes equivalent to (pid, start time) identity.
#[derive(Debug, Clone)]
pub struct Proc(pub Arc<ProcInfo>);

impl PartialEq for Proc {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for Proc {}
impl Hash for Proc {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(Arc::as_ptr(&self.0) as usize);
    }
}

/// Looks up a running process that has no start event.
pub trait ProcessSource: Send + std::fmt::Debug {
    /// Image path and creation time of the process now using `pid`.
    fn lookup(&mut self, pid: u32) -> Option<(String, FileTime)>;
}

/// [`ProcessSource`] over the live system (`OpenProcess` +
/// `QueryFullProcessImageNameW` + `GetProcessTimes` from one handle).
#[derive(Debug, Default, Clone, Copy)]
pub struct LiveProcesses;

impl ProcessSource for LiveProcesses {
    fn lookup(&mut self, pid: u32) -> Option<(String, FileTime)> {
        let p = strata_win::process::ProcessInfo::of(pid).ok()?;
        Some((p.image.to_string_lossy().into_owned(), p.start_time))
    }
}

/// Image label for pid 4.
pub const SYSTEM_IMAGE: &str = "System";

const TICKS_PER_SEC: u64 = 10_000_000;

#[derive(Debug)]
struct Slot {
    proc: Proc,
    exit: Option<FileTime>,
}

/// The (pid, start time) table.
#[derive(Debug)]
pub struct ProcessTable {
    by_pid: HashMap<u32, Vec<Slot>>,
    /// pid → event time of a failed lookup, so a pid that cannot be opened is
    /// not retried on every event.
    failed: HashMap<u32, FileTime>,
    source: Box<dyn ProcessSource>,
    system: Proc,
    grace: u64,
}

impl ProcessTable {
    /// Seconds an exited process still matches late events (real-time ETW
    /// delivers per-CPU buffers, so events can trail the stop event).
    pub const EXIT_GRACE_SECS: u64 = 10;
    /// Seconds before a failed lookup is retried.
    pub const RETRY_SECS: u64 = 30;

    /// An empty table that falls back to `source` for unknown pids.
    #[must_use]
    pub fn new(source: Box<dyn ProcessSource>) -> Self {
        Self {
            by_pid: HashMap::new(),
            failed: HashMap::new(),
            source,
            system: Proc(Arc::new(ProcInfo {
                pid: 4,
                start: FileTime(0),
                image: SYSTEM_IMAGE.into(),
            })),
            grace: Self::EXIT_GRACE_SECS * TICKS_PER_SEC,
        }
    }

    /// Records a process start. A live entry for the same pid with an older
    /// start time is closed at `start` (the id was reused).
    pub fn start(&mut self, pid: u32, start: FileTime, image: String) -> Proc {
        let slots = self.by_pid.entry(pid).or_default();
        if let Some(s) = slots.iter().find(|s| s.proc.0.start == start) {
            return s.proc.clone();
        }
        for s in slots.iter_mut() {
            if s.exit.is_none() && s.proc.0.start < start {
                s.exit = Some(start);
            }
        }
        let proc = Proc(Arc::new(ProcInfo {
            pid,
            start,
            image: image.into(),
        }));
        slots.push(Slot {
            proc: proc.clone(),
            exit: None,
        });
        slots.sort_by_key(|s| s.proc.0.start);
        self.failed.remove(&pid);
        proc
    }

    /// Records a process exit at `at`.
    pub fn stop(&mut self, pid: u32, start: FileTime, at: FileTime) {
        if let Some(s) = self
            .by_pid
            .get_mut(&pid)
            .and_then(|v| v.iter_mut().find(|s| s.proc.0.start == start))
        {
            s.exit = Some(at);
        }
    }

    /// The process that owned `pid` at `at`, if known. Pid 0 (idle) is never
    /// attributed; pid 4 is [`SYSTEM_IMAGE`].
    pub fn resolve(&mut self, pid: u32, at: FileTime) -> Option<Proc> {
        match pid {
            0 => return None,
            4 => return Some(self.system.clone()),
            _ => {}
        }
        if let Some(p) = self.match_slot(pid, at) {
            return Some(p);
        }
        if let Some(&t) = self.failed.get(&pid)
            && at.0.saturating_sub(t.0) < Self::RETRY_SECS * TICKS_PER_SEC
        {
            return None;
        }
        match self.source.lookup(pid) {
            Some((image, start)) => {
                let known = self
                    .by_pid
                    .get(&pid)
                    .is_some_and(|v| v.iter().any(|s| s.proc.0.start == start));
                if !known {
                    self.start(pid, start, image);
                }
                // The live process may be a newer one that reused the id.
                let p = self.match_slot(pid, at);
                if p.is_none() {
                    self.failed.insert(pid, at);
                }
                p
            }
            None => {
                self.failed.insert(pid, at);
                None
            }
        }
    }

    fn match_slot(&self, pid: u32, at: FileTime) -> Option<Proc> {
        self.by_pid.get(&pid)?.iter().rev().find_map(|s| {
            let alive = s.proc.0.start <= at && s.exit.is_none_or(|e| at.0 <= e.0 + self.grace);
            alive.then(|| s.proc.clone())
        })
    }

    /// Drops processes that exited more than the grace period before `now`
    /// and stale lookup failures.
    pub fn prune(&mut self, now: FileTime) {
        let grace = self.grace;
        self.by_pid.retain(|_, v| {
            v.retain(|s| s.exit.is_none_or(|e| e.0 + grace >= now.0));
            !v.is_empty()
        });
        self.failed
            .retain(|_, t| now.0.saturating_sub(t.0) < Self::RETRY_SECS * TICKS_PER_SEC);
    }

    /// Number of tracked processes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_pid.values().map(Vec::len).sum()
    }

    /// Whether no process is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_pid.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct Fake(HashMap<u32, (String, FileTime)>, u32);

    impl ProcessSource for Fake {
        fn lookup(&mut self, pid: u32) -> Option<(String, FileTime)> {
            self.1 += 1;
            self.0.get(&pid).cloned()
        }
    }

    const S: u64 = TICKS_PER_SEC;

    #[test]
    fn pid_reuse_resolves_by_time() {
        let mut t = ProcessTable::new(Box::new(Fake::default()));
        t.start(100, FileTime(10 * S), r"C:\a\first.exe".into());
        t.stop(100, FileTime(10 * S), FileTime(20 * S));
        t.start(100, FileTime(50 * S), r"C:\b\second.exe".into());
        let img = |t: &mut ProcessTable, s| {
            t.resolve(100, FileTime(s * S))
                .map(|p| p.0.image.to_string())
        };
        assert_eq!(img(&mut t, 15).as_deref(), Some(r"C:\a\first.exe"));
        // Late event inside the exit grace still goes to the first process.
        assert_eq!(img(&mut t, 25).as_deref(), Some(r"C:\a\first.exe"));
        assert_eq!(img(&mut t, 40), None);
        assert_eq!(img(&mut t, 60).as_deref(), Some(r"C:\b\second.exe"));
        t.prune(FileTime(100 * S));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn start_closes_previous_owner_of_the_pid() {
        let mut t = ProcessTable::new(Box::new(Fake::default()));
        t.start(7, FileTime(S), "old.exe".into());
        // Stop event lost; the new start implies the old one exited.
        t.start(7, FileTime(1000 * S), "new.exe".into());
        let p = t.resolve(7, FileTime(2000 * S)).unwrap();
        assert_eq!(p.0.file_name(), "new.exe");
        let p = t.resolve(7, FileTime(500 * S)).unwrap();
        assert_eq!(p.0.file_name(), "old.exe");
    }

    #[test]
    fn snapshot_fallback_rejects_reused_pid() {
        let mut f = Fake::default();
        f.0.insert(9, (r"C:\x\pre.exe".into(), FileTime(5 * S)));
        f.0.insert(11, (r"C:\x\newer.exe".into(), FileTime(500 * S)));
        let mut t = ProcessTable::new(Box::new(f));
        assert_eq!(
            t.resolve(9, FileTime(100 * S)).unwrap().0.file_name(),
            "pre.exe"
        );
        // pid 11 now belongs to a process that started after the event.
        assert!(t.resolve(11, FileTime(100 * S)).is_none());
        assert_eq!(
            t.resolve(11, FileTime(600 * S)).unwrap().0.file_name(),
            "newer.exe"
        );
        assert!(t.resolve(0, FileTime(1)).is_none());
        assert_eq!(&*t.resolve(4, FileTime(1)).unwrap().0.image, SYSTEM_IMAGE);
    }

    #[test]
    fn failed_lookups_are_not_retried_per_event() {
        #[derive(Debug, Default)]
        struct Count(std::sync::Arc<std::sync::atomic::AtomicU32>);
        impl ProcessSource for Count {
            fn lookup(&mut self, _: u32) -> Option<(String, FileTime)> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                None
            }
        }
        let c = Count::default();
        let n = c.0.clone();
        let mut t = ProcessTable::new(Box::new(c));
        for i in 0..100 {
            assert!(t.resolve(77, FileTime(S + i)).is_none());
        }
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(t.resolve(77, FileTime(S * 40)).is_none());
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
