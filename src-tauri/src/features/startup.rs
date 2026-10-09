//! Startup behaviour (SPEC §19 "Startup", §21 multiple instances).
//!
//! - Launch at login through the autostart plugin (`HKCU\...\Run`), off by
//!   default; the registry entry passes [`AUTOSTART_FLAG`] so a login start
//!   can open minimized to the tray.
//! - Command-line arguments: an optional path to open, from the first
//!   launch ([`app_launch_request`]) and from later launches, which the
//!   single-instance plugin forwards here ([`on_second_instance`]).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_autostart::ManagerExt;

use super::error::{ErrorKind, FeatureError, FeatureResult};

/// Argument the login entry passes.
pub const AUTOSTART_FLAG: &str = "--autostart";

/// Event emitted when another launch forwards its arguments.
pub const SECOND_INSTANCE: &str = "app://second-instance";

/// What a launch asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRequest {
    /// Started by the login entry.
    pub autostart: bool,
    /// An absolute path to open (e.g. "Open in Strata" or a drag onto the
    /// shortcut), when one was given.
    pub open_path: Option<String>,
    /// Every argument after the executable, as received.
    pub args: Vec<String>,
}

impl LaunchRequest {
    /// Parses `argv` (including the executable at index 0). Relative paths
    /// are resolved against `cwd`; flags other than ours are ignored.
    #[must_use]
    pub fn parse(argv: &[String], cwd: &Path) -> Self {
        let args: Vec<String> = argv.iter().skip(1).cloned().collect();
        let autostart = args.iter().any(|a| a == AUTOSTART_FLAG);
        let open_path = args
            .iter()
            .find(|a| !a.starts_with('-') && !a.trim().is_empty())
            .map(|a| {
                let p = PathBuf::from(a.trim_matches('"'));
                if p.is_absolute() { p } else { cwd.join(p) }
            })
            .map(|p| p.display().to_string());
        Self {
            autostart,
            open_path,
            args,
        }
    }
}

/// Managed state: the first launch's request until the UI takes it.
#[derive(Debug, Default)]
pub struct InitialLaunch(pub Mutex<Option<LaunchRequest>>);

/// The first launch's request (once; later calls return `null`). The UI
/// calls this after it starts listening for [`SECOND_INSTANCE`].
#[tauri::command]
pub fn app_launch_request<R: Runtime>(app: AppHandle<R>) -> Option<LaunchRequest> {
    app.try_state::<InitialLaunch>()
        .and_then(|s| s.0.lock().ok().and_then(|mut g| g.take()))
}

/// Single-instance callback: focus the existing window and forward the new
/// launch's arguments as [`SECOND_INSTANCE`].
pub fn on_second_instance<R: Runtime>(app: &AppHandle<R>, argv: Vec<String>, cwd: String) {
    super::tray::show_main_window(app);
    let request = LaunchRequest::parse(&argv, Path::new(&cwd));
    let _ = app.emit(SECOND_INSTANCE, request);
}

/// Registers or removes launch at login.
///
/// # Errors
///
/// When the autostart entry cannot be written.
pub fn set_launch_at_login<R: Runtime>(app: &AppHandle<R>, on: bool) -> FeatureResult<()> {
    let m = app.autolaunch();
    let r = if on { m.enable() } else { m.disable() };
    r.map_err(|e| FeatureError::new(ErrorKind::Io, format!("launch at login: {e}")))
}

/// Brings the login entry in line with the setting (it can be removed by
/// the user in Task Manager, or left behind by an old version).
pub fn sync_launch_at_login<R: Runtime>(app: &AppHandle<R>, wanted: bool) {
    if let Ok(actual) = app.autolaunch().is_enabled()
        && actual != wanted
    {
        // NOTE: if Task Manager disabled the entry, re-enabling here would
        // fight the user; only remove a stale entry, never add one silently.
        if !wanted {
            let _ = set_launch_at_login(app, false);
        }
    }
}

/// Response of `startup_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupStatus {
    /// Whether the login entry exists right now.
    pub launch_at_login: bool,
}

/// The actual launch-at-login state (it can differ from the setting when
/// the user disabled it in Task Manager).
#[tauri::command]
pub fn startup_status<R: Runtime>(app: AppHandle<R>) -> FeatureResult<StartupStatus> {
    let launch_at_login = app
        .autolaunch()
        .is_enabled()
        .map_err(|e| FeatureError::new(ErrorKind::Io, format!("launch at login: {e}")))?;
    Ok(StartupStatus { launch_at_login })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        std::iter::once("strata.exe")
            .chain(a.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn parses_autostart_and_paths() {
        let cwd = Path::new(r"C:\Users\me");
        let r = LaunchRequest::parse(&argv(&[]), cwd);
        assert_eq!(r, LaunchRequest::default());

        let r = LaunchRequest::parse(&argv(&[AUTOSTART_FLAG]), cwd);
        assert!(r.autostart);
        assert_eq!(r.open_path, None);

        let r = LaunchRequest::parse(&argv(&[r"D:\Projects"]), cwd);
        assert_eq!(r.open_path.as_deref(), Some(r"D:\Projects"));
        assert!(!r.autostart);

        let r = LaunchRequest::parse(&argv(&["--flag", "Downloads"]), cwd);
        assert_eq!(r.open_path.as_deref(), Some(r"C:\Users\me\Downloads"));
        assert_eq!(r.args, ["--flag", "Downloads"]);
    }
}
