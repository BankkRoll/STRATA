//! Session lifecycle: one fixed-name real-time session, never leaked.
//!
//! The rules, all enforced by [`SessionGuard`] over any [`TraceControl`]
//! (the Win32 one is [`crate::ffi::Win32Control`]; tests use a fake):
//!
//! 1. The session has a fixed name ([`SESSION_NAME`]), so a session left
//!    behind by a crashed helper is found by name.
//! 2. If starting fails because the name exists, the stale session is
//!    stopped and the start retried once.
//! 3. The guard exists from the moment the session does. If enabling a
//!    provider fails, or the caller panics or returns early, dropping the
//!    guard stops the session.
//! 4. [`recover_orphaned_session`] stops a leftover session at the next
//!    launch, before anything else runs.
//!
//! ETW sessions are kernel objects that outlive the process that created
//! them, so these rules are the only thing that keeps a crash from leaving
//! kernel file tracing running until reboot.

use serde::{Deserialize, Serialize};

use crate::error::{EtwError, code};

/// The session name.
pub const SESSION_NAME: &str = "Strata-FileActivity";

/// `Microsoft-Windows-Kernel-File` keywords enabled: FILENAME (0x10),
/// CREATE (0x80), WRITE (0x200), DELETE_PATH (0x400),
/// RENAME_SETLINK_PATH (0x800), CREATE_NEW_FILE (0x1000). FILEIO (0x20) and
/// READ (0x100) are left off: they add cleanup/close/read/query/directory
/// events on every open file and would multiply the event rate.
pub const KERNEL_FILE_KEYWORDS: u64 = 0x10 | 0x80 | 0x200 | 0x400 | 0x800 | 0x1000;

/// `Microsoft-Windows-Kernel-Process` keyword `WINEVENT_KEYWORD_PROCESS`
/// (process start/stop).
pub const KERNEL_PROCESS_KEYWORDS: u64 = 0x10;

/// `TRACE_LEVEL_INFORMATION`; every event used is logged at this level.
pub const LEVEL_INFORMATION: u8 = 4;

/// A provider to enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSpec {
    /// Provider GUID as a `u128`.
    pub guid: u128,
    /// Maximum level.
    pub level: u8,
    /// `MatchAnyKeyword`.
    pub keywords: u64,
}

/// Session buffer settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfig {
    /// Session name.
    pub name: String,
    /// Buffer size in KiB (default 64).
    pub buffer_kb: u32,
    /// Minimum buffers (default 4).
    pub min_buffers: u32,
    /// Maximum buffers (default 64): bursts beyond this many unread buffers
    /// are dropped by the kernel and reported as lost events.
    pub max_buffers: u32,
    /// Seconds between forced buffer flushes (default 1), bounding the
    /// latency of the "now" window.
    pub flush_secs: u32,
    /// Providers to enable, in order.
    pub providers: Vec<ProviderSpec>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            name: SESSION_NAME.to_owned(),
            buffer_kb: 64,
            min_buffers: 4,
            max_buffers: 64,
            flush_secs: 1,
            providers: vec![
                ProviderSpec {
                    guid: crate::KERNEL_PROCESS_PROVIDER.to_u128(),
                    level: LEVEL_INFORMATION,
                    keywords: KERNEL_PROCESS_KEYWORDS,
                },
                ProviderSpec {
                    guid: crate::KERNEL_FILE_PROVIDER.to_u128(),
                    level: LEVEL_INFORMATION,
                    keywords: KERNEL_FILE_KEYWORDS,
                },
            ],
        }
    }
}

/// Counters reported when a session stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionStats {
    /// Events the kernel dropped (buffers full).
    pub events_lost: u32,
    /// Real-time buffers dropped (consumer too slow).
    pub realtime_buffers_lost: u32,
}

/// The trace-control calls the guard needs. Errors are Win32 status codes.
pub trait TraceControl {
    /// `StartTraceW` for a real-time session; returns its control handle.
    fn start(&mut self, cfg: &SessionConfig) -> Result<u64, u32>;
    /// `EnableTraceEx2(ENABLE_PROVIDER)`.
    fn enable(&mut self, handle: u64, provider: &ProviderSpec) -> Result<(), u32>;
    /// `ControlTraceW(STOP)` by name.
    fn stop(&mut self, name: &str) -> Result<SessionStats, u32>;
    /// `ControlTraceW(QUERY)` by name: whether the session exists.
    fn exists(&mut self, name: &str) -> Result<bool, u32>;
}

/// Owns a running session; stops it on drop.
#[derive(Debug)]
pub struct SessionGuard<C: TraceControl> {
    control: C,
    name: String,
    handle: u64,
    live: bool,
}

impl<C: TraceControl> SessionGuard<C> {
    /// Starts the session (stopping a stale one with the same name first)
    /// and enables every provider. On any failure the session is stopped
    /// before returning.
    ///
    /// # Errors
    ///
    /// [`EtwError::NotElevated`] for access denied, else
    /// [`EtwError::Win32`] naming the failing call.
    pub fn start(mut control: C, cfg: &SessionConfig) -> Result<Self, EtwError> {
        let handle = match control.start(cfg) {
            Ok(h) => h,
            Err(code::ALREADY_EXISTS) => {
                match control.stop(&cfg.name) {
                    Ok(_) | Err(code::NOT_FOUND) => {}
                    Err(e) => return Err(EtwError::win32("ControlTraceW(STOP stale)", e)),
                }
                control
                    .start(cfg)
                    .map_err(|e| EtwError::win32("StartTraceW", e))?
            }
            Err(e) => return Err(EtwError::win32("StartTraceW", e)),
        };
        let mut guard = Self {
            control,
            name: cfg.name.clone(),
            handle,
            live: true,
        };
        for p in &cfg.providers {
            guard
                .control
                .enable(guard.handle, p)
                .map_err(|e| EtwError::win32("EnableTraceEx2", e))?;
        }
        Ok(guard)
    }

    /// Session name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the session is still owned (not yet stopped).
    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.live
    }

    /// Stops the session now. Idempotent; a session already stopped by
    /// someone else counts as stopped.
    ///
    /// # Errors
    ///
    /// [`EtwError::Win32`] when `ControlTraceW` fails for another reason;
    /// the guard then still tries again on drop.
    pub fn stop(&mut self) -> Result<SessionStats, EtwError> {
        if !self.live {
            return Ok(SessionStats::default());
        }
        match self.control.stop(&self.name) {
            Ok(s) => {
                self.live = false;
                Ok(s)
            }
            Err(code::NOT_FOUND) => {
                self.live = false;
                Ok(SessionStats::default())
            }
            Err(e) => Err(EtwError::win32("ControlTraceW(STOP)", e)),
        }
    }
}

impl<C: TraceControl> Drop for SessionGuard<C> {
    fn drop(&mut self) {
        // NOTE: errors cannot be reported from drop; the next launch's
        // recover_orphaned_session catches anything left behind.
        let _ = self.stop();
    }
}

/// Stops a session named `name` if one exists. Returns whether one was
/// stopped.
///
/// # Errors
///
/// [`EtwError::NotElevated`] or [`EtwError::Win32`].
pub fn recover_orphaned_session_with<C: TraceControl>(
    control: &mut C,
    name: &str,
) -> Result<bool, EtwError> {
    match control.exists(name) {
        Ok(false) | Err(code::NOT_FOUND) => Ok(false),
        Ok(true) => match control.stop(name) {
            Ok(_) => Ok(true),
            Err(code::NOT_FOUND) => Ok(false),
            Err(e) => Err(EtwError::win32("ControlTraceW(STOP)", e)),
        },
        Err(e) => Err(EtwError::win32("ControlTraceW(QUERY)", e)),
    }
}

/// Stops a session left behind by a crashed helper ([`SESSION_NAME`]).
/// Call it when the helper starts, before anything else. Returns whether a
/// session was found and stopped.
///
/// # Errors
///
/// [`EtwError::NotElevated`] when not elevated (an unelevated process can
/// neither see nor stop the session), or [`EtwError::Win32`].
pub fn recover_orphaned_session() -> Result<bool, EtwError> {
    recover_orphaned_session_with(&mut crate::ffi::Win32Control, SESSION_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct Fake {
        running: Vec<String>,
        log: Vec<String>,
        fail_enable_at: Option<usize>,
        enabled: usize,
        start_error: Option<u32>,
        stop_error: Option<u32>,
    }

    impl TraceControl for &mut Fake {
        fn start(&mut self, cfg: &SessionConfig) -> Result<u64, u32> {
            self.log.push(format!("start {}", cfg.name));
            if let Some(e) = self.start_error.take() {
                return Err(e);
            }
            if self.running.contains(&cfg.name) {
                return Err(code::ALREADY_EXISTS);
            }
            self.running.push(cfg.name.clone());
            Ok(42)
        }
        fn enable(&mut self, handle: u64, p: &ProviderSpec) -> Result<(), u32> {
            assert_eq!(handle, 42);
            self.log.push(format!("enable {:x}", p.keywords));
            self.enabled += 1;
            if self.fail_enable_at == Some(self.enabled) {
                return Err(87);
            }
            Ok(())
        }
        fn stop(&mut self, name: &str) -> Result<SessionStats, u32> {
            self.log.push(format!("stop {name}"));
            if let Some(e) = self.stop_error.take() {
                return Err(e);
            }
            let before = self.running.len();
            self.running.retain(|n| n != name);
            if before == self.running.len() {
                Err(code::NOT_FOUND)
            } else {
                Ok(SessionStats {
                    events_lost: 3,
                    realtime_buffers_lost: 0,
                })
            }
        }
        fn exists(&mut self, name: &str) -> Result<bool, u32> {
            self.log.push(format!("query {name}"));
            Ok(self.running.iter().any(|n| n == name))
        }
    }

    #[test]
    fn start_enable_stop() {
        let mut f = Fake::default();
        {
            let mut g = SessionGuard::start(&mut f, &SessionConfig::default()).unwrap();
            assert!(g.is_live());
            assert_eq!(g.stop().unwrap().events_lost, 3);
            assert!(!g.is_live());
            // Idempotent, and drop does not stop again.
            assert_eq!(g.stop().unwrap(), SessionStats::default());
        }
        assert!(f.running.is_empty());
        assert_eq!(
            f.log,
            [
                "start Strata-FileActivity",
                "enable 10",
                "enable 1e90",
                "stop Strata-FileActivity"
            ]
        );
    }

    #[test]
    fn stale_session_is_stopped_then_restarted() {
        let mut f = Fake {
            running: vec![SESSION_NAME.into()],
            ..Default::default()
        };
        let g = SessionGuard::start(&mut f, &SessionConfig::default()).unwrap();
        drop(g);
        assert!(f.running.is_empty());
        assert_eq!(
            &f.log[..3],
            [
                "start Strata-FileActivity",
                "stop Strata-FileActivity",
                "start Strata-FileActivity"
            ]
        );
    }

    #[test]
    fn enable_failure_stops_the_session() {
        let mut f = Fake {
            fail_enable_at: Some(2),
            ..Default::default()
        };
        let err = SessionGuard::start(&mut f, &SessionConfig::default()).unwrap_err();
        assert_eq!(
            err,
            EtwError::Win32 {
                op: "EnableTraceEx2".into(),
                code: 87
            }
        );
        assert!(f.running.is_empty(), "session leaked: {:?}", f.log);
        assert_eq!(f.log.last().unwrap(), "stop Strata-FileActivity");
    }

    #[test]
    fn access_denied_maps_to_not_elevated_and_starts_nothing() {
        let mut f = Fake {
            start_error: Some(code::ACCESS_DENIED),
            ..Default::default()
        };
        let err = SessionGuard::start(&mut f, &SessionConfig::default()).unwrap_err();
        assert_eq!(err, EtwError::NotElevated);
        assert!(f.running.is_empty());
        assert_eq!(f.log, ["start Strata-FileActivity"]);
    }

    #[test]
    fn panic_while_owning_the_guard_stops_the_session() {
        let mut f = Fake::default();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = SessionGuard::start(&mut f, &SessionConfig::default()).unwrap();
            panic!("consumer failed");
        }));
        assert!(r.is_err());
        assert!(f.running.is_empty());
    }

    #[test]
    fn failed_stop_is_retried_on_drop_and_external_stop_is_fine() {
        let mut f = Fake::default();
        {
            let mut g = SessionGuard::start(&mut f, &SessionConfig::default()).unwrap();
            g.control.stop_error = Some(6);
            assert!(g.stop().is_err());
            assert!(g.is_live());
        }
        assert!(f.running.is_empty());

        let mut f = Fake::default();
        let mut g = SessionGuard::start(&mut f, &SessionConfig::default()).unwrap();
        g.control.running.clear();
        assert_eq!(g.stop().unwrap(), SessionStats::default());
    }

    #[test]
    fn orphan_recovery() {
        let mut f = Fake {
            running: vec![SESSION_NAME.into(), "Other".into()],
            ..Default::default()
        };
        assert!(recover_orphaned_session_with(&mut &mut f, SESSION_NAME).unwrap());
        assert!(!recover_orphaned_session_with(&mut &mut f, SESSION_NAME).unwrap());
        assert_eq!(f.running, ["Other"]);
    }
}
