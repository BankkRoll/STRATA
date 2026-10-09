//! App-side features beyond scanning (SPEC §15, §18, §19).
//!
//! Responsibilities:
//! - [`store`]: open `strata-store` per user, health, resets, daily
//!   retention.
//! - [`audit`]: the write-ahead `AuditLog` adapter and crash recovery.
//! - [`settings`]: load/save/validate, export/import, `settings://changed`.
//! - [`cleanup`]: plan / pre-flight / execute / retry / restore, the
//!   never-list again at this layer, and the privileged-delete hook.
//! - [`locks`]: lock holders and polite close behind a consent round trip.
//! - [`consent`]: the confirmation round trip itself.
//! - [`tools`]: built-in tools, Empty Recycle Bin, Recycle Bin facts.
//! - [`history`]: usage series, snapshots, diffs, sparklines, since last scan.
//! - [`tray`]: tray icon, free-space glance, low-space toasts.
//! - [`startup`]: launch at login, launch arguments, second instance.
//! - [`helper`]: helper service install/uninstall/status.
//!
//! Wiring in `lib.rs` is three calls: [`register`] on the builder,
//! [`setup`] in the setup hook, and [`invoke_handler!`] for the commands.

pub mod audit;
pub mod cleanup;
pub mod consent;
pub mod error;
pub mod helper;
pub mod history;
pub mod links;
pub mod locks;
pub mod settings;
pub mod startup;
pub mod store;
pub mod tools;
pub mod tray;

use std::sync::Mutex;

use tauri::{App, AppHandle, Manager, Runtime};

pub use startup::on_second_instance;

/// Adds the plugins these features need. Call after the single-instance
/// plugin (which must stay first).
#[must_use]
pub fn register<R: Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .arg(startup::AUTOSTART_FLAG)
                .build(),
        )
}

/// What `lib.rs` should do with the main window after [`setup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Launch {
    /// Show the window now; `false` when starting minimized to the tray.
    pub show_window: bool,
}

/// Opens the store, starts recovery and maintenance, the tray and the
/// low-space monitor. Never fails because of a damaged store; that is
/// reported through `store_health`.
pub fn setup<R: Runtime>(app: &mut App<R>) -> Launch {
    let handle = app.handle().clone();
    let data_dir = handle.path().app_local_data_dir().ok();
    let app_store = match store::store_dir(&handle) {
        Ok(dir) => store::AppStore::open(dir),
        Err(e) => store::AppStore::unavailable(e.message),
    };
    let opened = app_store.get().ok();
    app.manage(app_store);

    let protected: Vec<_> = data_dir.into_iter().collect();
    app.manage(cleanup::CleanupState::new(cleanup::CleanupService::new(
        Box::new(move || cleanup::machine_guard(&protected)),
    )));
    app.manage(locks::KnownHolders::default());
    app.manage(locks::PendingCloses::default());
    app.manage(tools::PendingTools::default());
    app.manage(Capabilities::default());

    let cwd = std::env::current_dir().unwrap_or_default();
    let argv: Vec<String> = std::env::args().collect();
    let request = startup::LaunchRequest::parse(&argv, &cwd);
    let autostart = request.autostart;
    app.manage(startup::InitialLaunch(Mutex::new(Some(request))));

    let settings = opened
        .as_ref()
        .and_then(|s| s.load_settings().ok())
        .unwrap_or_default();
    startup::sync_launch_at_login(&handle, settings.startup.launch_at_login);
    tray::setup(&handle, &settings);
    start_recovery(&handle, opened);

    Launch {
        show_window: !(autostart
            && settings.startup.start_minimized_to_tray
            && settings.tray.enabled),
    }
}

/// Crash recovery of the undo log, then cleanup is enabled and daily
/// maintenance starts. Runs off the main thread: recovery may look into
/// Recycle Bin folders.
fn start_recovery<R: Runtime>(app: &AppHandle<R>, store: Option<strata_store::Store>) {
    let service = app.state::<cleanup::CleanupState>().service();
    let Some(store) = store else {
        service.set_unavailable("Strata's data folder could not be opened".into());
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("strata-recovery".into())
        .spawn(move || {
            if store.health().state == strata_store::DbHealth::Ok {
                match audit::recover(&store, &audit::FsProbe) {
                    Ok(_) => service.set_ready(),
                    Err(e) => {
                        service.set_unavailable(format!("the undo log could not be checked: {e}"))
                    }
                }
            } else {
                service.set_unavailable(
                    "the settings and undo database is damaged; reset it in Settings > Data".into(),
                );
            }
            store::spawn_maintenance(store);
        });
    drop(spawned);
}

// -----------------------------------------------------------------------------
// Capabilities
// -----------------------------------------------------------------------------

/// Command names other modules registered for `app_capabilities`.
#[derive(Debug, Default)]
pub struct Capabilities(Mutex<Vec<String>>);

/// Adds command names (for example the scan and layout commands) to the
/// list `app_capabilities` returns.
pub fn register_capabilities<R: Runtime>(app: &AppHandle<R>, names: &[&str]) {
    if let Some(c) = app.try_state::<Capabilities>()
        && let Ok(mut v) = c.0.lock()
    {
        v.extend(names.iter().map(|s| (*s).to_string()));
    }
}

/// Store-backed commands.
const STORE_COMMANDS: &[&str] = &[
    "settings_load",
    "settings_save",
    "settings_export",
    "settings_import",
    "history_volumes",
    "history_series",
    "history_snapshots",
    "history_diff",
    "history_dir_series",
    "history_since_last_scan",
    "cleanup_history",
    "cleanup_action",
    "cleanup_restorable",
    "cleanup_restore",
    "tools_run",
    "recycle_bin_empty",
    "helper_service_status",
];

/// Commands that act on files once recovery is done.
const CLEANUP_COMMANDS: &[&str] = &[
    "cleanup_plan",
    "cleanup_preflight",
    "cleanup_execute",
    "cleanup_cancel",
    "cleanup_retry",
    "cleanup_discard",
];

/// Commands that work without the store.
const ALWAYS_COMMANDS: &[&str] = &[
    "app_info",
    "app_capabilities",
    "app_launch_request",
    "store_health",
    "store_reset_history",
    "store_reset_state",
    "settings_validate",
    "cleanup_status",
    "locks_query",
    "locks_close_prepare",
    "locks_close_politely",
    "locks_close_cancel",
    "tools_list",
    "tools_prepare",
    "tools_cancel",
    "recycle_bin_info",
    "startup_status",
    "open_url",
    "report_issue",
];

/// Which shell commands work right now: store-backed ones need an open
/// store, cleanup needs a usable undo log, the elevated route needs a
/// connected helper, service install needs the helper binary.
#[must_use]
pub fn available_commands<R: Runtime>(app: &AppHandle<R>) -> Vec<String> {
    let mut out: Vec<String> = ALWAYS_COMMANDS.iter().map(|s| (*s).to_string()).collect();
    let store_ok = store::handle(app).is_ok();
    if store_ok {
        out.extend(STORE_COMMANDS.iter().map(|s| (*s).to_string()));
    }
    let svc = app
        .try_state::<cleanup::CleanupState>()
        .map(|s| s.service());
    let cleanup_ok = store_ok
        && svc
            .as_ref()
            .is_some_and(|s| !matches!(s.readiness(), cleanup::Readiness::Unavailable { .. }));
    if cleanup_ok {
        out.extend(CLEANUP_COMMANDS.iter().map(|s| (*s).to_string()));
        if svc.as_ref().is_some_and(|s| s.privileged().is_some()) {
            out.push("cleanup_execute_elevated".into());
        }
    }
    if store_ok && helper::helper_present() {
        out.push("helper_service_install".into());
        out.push("helper_service_uninstall".into());
    }
    if let Some(c) = app.try_state::<Capabilities>()
        && let Ok(v) = c.0.lock()
    {
        out.extend(v.iter().cloned());
    }
    out.sort();
    out.dedup();
    out
}

/// Names of the backend commands that work in this build right now. The
/// UI enables exactly these.
#[tauri::command]
pub fn app_capabilities<R: Runtime>(app: AppHandle<R>) -> Vec<String> {
    available_commands(&app)
}

/// `tauri::generate_handler!` with every shell-feature command, followed by
/// the commands passed in (`invoke_handler![app_info, scan::scan_start]`).
macro_rules! invoke_handler {
    ($($cmd:tt)*) => {
        ::tauri::generate_handler![
            crate::features::app_capabilities,
            crate::features::startup::app_launch_request,
            crate::features::startup::startup_status,
            crate::features::store::store_health,
            crate::features::store::store_reset_history,
            crate::features::store::store_reset_state,
            crate::features::settings::settings_load,
            crate::features::settings::settings_validate,
            crate::features::settings::settings_save,
            crate::features::settings::settings_export,
            crate::features::settings::settings_import,
            crate::features::cleanup::cleanup_status,
            crate::features::cleanup::cleanup_plan,
            crate::features::cleanup::cleanup_preflight,
            crate::features::cleanup::cleanup_execute,
            crate::features::cleanup::cleanup_execute_elevated,
            crate::features::cleanup::cleanup_cancel,
            crate::features::cleanup::cleanup_retry,
            crate::features::cleanup::cleanup_discard,
            crate::features::cleanup::cleanup_history,
            crate::features::cleanup::cleanup_action,
            crate::features::cleanup::cleanup_restorable,
            crate::features::cleanup::cleanup_restore,
            crate::features::locks::locks_query,
            crate::features::locks::locks_close_prepare,
            crate::features::locks::locks_close_politely,
            crate::features::locks::locks_close_cancel,
            crate::features::tools::tools_list,
            crate::features::tools::tools_prepare,
            crate::features::tools::tools_run,
            crate::features::tools::tools_cancel,
            crate::features::tools::recycle_bin_empty,
            crate::features::tools::recycle_bin_info,
            crate::features::history::history_volumes,
            crate::features::history::history_series,
            crate::features::history::history_snapshots,
            crate::features::history::history_diff,
            crate::features::history::history_dir_series,
            crate::features::history::history_since_last_scan,
            crate::features::helper::helper_service_install,
            crate::features::helper::helper_service_uninstall,
            crate::features::helper::helper_service_status,
            crate::features::links::open_url,
            crate::features::links::report_issue,
            $($cmd)*
        ]
    };
}
pub(crate) use invoke_handler;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_lists_are_disjoint_and_complete() {
        let mut all: Vec<&str> = ALWAYS_COMMANDS
            .iter()
            .chain(STORE_COMMANDS)
            .chain(CLEANUP_COMMANDS)
            .copied()
            .chain([
                "cleanup_execute_elevated",
                "helper_service_install",
                "helper_service_uninstall",
            ])
            .collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n, "a command is listed twice");
        // Every command in the handler macro, plus app_info from lib.rs.
        let source = include_str!("mod.rs");
        let start = source.find("::tauri::generate_handler![").unwrap();
        let end = source[start..].find("$($cmd)*").unwrap() + start;
        let mut handled: Vec<&str> = source[start..end]
            .lines()
            .filter_map(|l| l.trim().strip_prefix("crate::features::"))
            .map(|l| l.trim_end_matches(',').rsplit("::").next().unwrap())
            .chain(["app_info"])
            .collect();
        handled.sort_unstable();
        assert_eq!(handled, all);
    }
}
