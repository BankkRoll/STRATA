//! Strata desktop app: the unelevated UI process.
//!
//! Responsibilities:
//! - Owns the main window (backdrop, custom title bar with Snap Layouts,
//!   single-instance enforcement, tray, launch at login, updates).
//! - Discovers volumes and watches hot-plug ([`volumes`], `volumes://changed`).
//! - Scans with the walker or the elevated helper into an index, classifies
//!   it and publishes it progressively ([`scan`], [`jobs`], [`helper`]), then
//!   keeps it current with the USN journal or folder watching ([`live`]).
//! - Serves the UI: binary layout frames ([`layout_pipe`]), binary row pages
//!   ([`rows`]), entry info and detail ([`detail`]), search ([`search`]) and
//!   shell actions ([`shell`]), analysis views ([`insights`]), duplicates
//!   ([`dupes`]) and file activity ([`activity`]).
//! - Settings, cleanup, tools, history, tray and startup ([`features`]).
//!
//! Module map: `commands/` holds the index-backed Tauri commands (one file
//! per area), `features/` the store-backed ones; [`state`] documents the
//! threading model. Every command is registered in one place,
//! [`app_commands!`] below, which also produces the list `app_capabilities`
//! filters.

#[cfg(not(windows))]
compile_error!("the Strata app targets Windows only");

pub mod activity;
mod backdrop;
pub mod capabilities;
pub mod classify;
pub mod commands;
pub mod detail;
pub mod dirwatch;
pub mod dupes;
pub mod error;
pub mod features;
pub mod frame;
pub mod helper;
pub mod ids;
pub mod insights;
pub mod jobs;
pub mod layout_pipe;
pub mod live;
pub mod model;
pub mod rows;
pub mod scan;
pub mod search;
pub mod shell;
mod snap;
pub mod state;
pub mod updater;
pub mod volumes;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tauri::Manager;

use crate::classify::Engine;
use crate::state::{AppState, lock};

/// Command modules by short name, for [`app_commands!`].
mod handlers {
    pub use crate::commands::{
        activity, app, dupes, entries, insights, layout, rules, search, volumes,
    };
    pub use crate::features::{cleanup, history, queue, settings, startup, store, tools};
    pub use crate::features::{helper as service, links, locks};
}

/// Declares every command once: the `invoke_handler` registration and the
/// names [`capabilities`] filters for `app_capabilities`. New commands are
/// added here (and given a requirement in [`capabilities::requirement`]).
macro_rules! app_commands {
    ($($module:ident :: $cmd:ident),* $(,)?) => {
        /// Names of every registered command.
        pub const COMMAND_NAMES: &[&str] = &[$(stringify!($cmd)),*];

        fn invoke_handler() -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static {
            tauri::generate_handler![$(handlers::$module::$cmd),*]
        }
    };
}

app_commands![
    // App and startup.
    app::app_info,
    app::app_capabilities,
    app::updates_check,
    app::updates_restart,
    app::about_licenses,
    links::open_url,
    links::report_issue,
    startup::app_launch_request,
    startup::startup_status,
    store::store_health,
    store::store_reset_history,
    store::store_reset_state,
    store::data_clear,
    // Volumes, scanning and the helper.
    volumes::list_volumes,
    volumes::helper_status,
    volumes::helper_elevate,
    volumes::scan_start,
    volumes::scan_cancel,
    service::helper_service_install,
    service::helper_service_uninstall,
    service::helper_service_status,
    // The index.
    layout::layout_open,
    layout::layout_request,
    layout::layout_close,
    entries::entry_info,
    entries::entry_path,
    entries::entry_detail,
    entries::list_children,
    entries::apps_brief,
    entries::entry_action,
    search::search_open,
    search::search_query,
    search::search_close,
    // Analysis.
    insights::insights_largest,
    insights::insights_file_types,
    insights::insights_categories,
    insights::apps_footprint,
    insights::apps_orphans,
    insights::apps_queue_caches,
    insights::recommendations_list,
    insights::recommendations_preview,
    insights::recommendations_queue,
    rules::rules_list,
    rules::rules_open_folder,
    rules::rules_reload,
    rules::rules_explain,
    // History.
    history::history_snapshots,
    history::history_usage,
    history::history_diff,
    history::history_dir_series,
    history::history_since_last_scan,
    // Settings.
    settings::settings_load,
    settings::settings_save,
    settings::settings_export,
    settings::settings_import,
    // Cleanup.
    cleanup::cleanup_status,
    queue::cleanup_queue_list,
    queue::cleanup_queue_add,
    queue::cleanup_queue_remove,
    queue::cleanup_queue_clear,
    queue::cleanup_plan,
    queue::cleanup_preflight,
    queue::cleanup_close_prompt,
    queue::cleanup_close_app,
    queue::cleanup_execute,
    queue::cleanup_execute_elevated,
    queue::cleanup_cancel,
    queue::cleanup_retry_plan,
    queue::cleanup_delete_on_reboot,
    queue::cleanup_history,
    queue::cleanup_restore,
    locks::locks_query,
    // Tools.
    tools::tools_status,
    tools::tools_prepare,
    tools::tools_run,
    tools::recycle_bin_empty,
    tools::recycle_bin_info,
    // Duplicates.
    dupes::dupes_status,
    dupes::dupes_start,
    dupes::dupes_cancel,
    dupes::dupes_groups,
    dupes::dupes_queue,
    dupes::dupes_hardlink_prompt,
    dupes::dupes_hardlink,
    // Activity.
    activity::activity_status,
    activity::activity_set_enabled,
    activity::activity_top,
    activity::activity_dir_writers,
    activity::activity_clear,
];

/// Loads rules and the app catalog, then keeps the volume list current.
/// Runs on background threads; the UI is usable meanwhile.
fn start_background(app: &tauri::AppHandle, state: &Arc<AppState>) {
    let data_dir = app.path().app_local_data_dir().ok();
    let st = state.clone();
    let handle = app.clone();
    let _ = std::thread::Builder::new()
        .name("strata-startup".into())
        .spawn(move || {
            if let Ok(p) = strata_win::last_access::last_access_policy() {
                st.access_unreliable
                    .store(p.access_times_unreliable(), Ordering::Relaxed);
            }
            let kf = strata_win::known::known_folders().unwrap_or_default();
            let rules_dir = rules_dir(&st, data_dir.as_deref());
            let engine = Engine::new(&kf, rules_dir.as_deref()).map(Arc::new);
            st.engine.set(engine.clone());
            if let Ok(e) = engine {
                e.load_catalog(&kf);
            }
            live::restore_cached(&handle, &st);
        });

    let st = state.clone();
    let handle = app.clone();
    let _ = std::thread::Builder::new()
        .name("strata-volumes".into())
        .spawn(move || watch_volumes(&handle, &st));
}

/// The user rules folder: the one from settings when user rules are on and
/// a folder is set, else `<app data>\rules`.
pub(crate) fn rules_dir(
    state: &AppState,
    data_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    let settings = state.store().and_then(|s| s.load_settings().ok());
    match settings {
        Some(s) if !s.rules.user_rules_enabled => None,
        Some(s) if s.rules.user_rules_dir.is_some() => s.rules.user_rules_dir.map(Into::into),
        _ => data_dir.map(|d| d.join("rules")),
    }
}

fn watch_volumes(app: &tauri::AppHandle, state: &Arc<AppState>) {
    use strata_win::watcher::{VolumeEvent, VolumeWatcher, WatcherOptions};
    let watcher = match VolumeWatcher::start(WatcherOptions::default()) {
        Ok(w) => w,
        Err(_) => {
            // NOTE: without hot-plug notifications the list is still correct
            // at startup; it just will not follow later arrivals.
            if let Ok(list) =
                strata_win::volume::discover_volumes(strata_win::volume::DiscoveryOptions::local())
            {
                lock(&state.registry).replace_all(list, |_| false);
                jobs::emit_volumes(app, state);
            }
            return;
        }
    };
    let initial = watcher.initial().to_vec();
    lock(&state.registry).replace_all(initial, |id| state.has_index(id));
    jobs::emit_volumes(app, state);
    while let Ok(ev) = watcher.events().recv() {
        {
            let mut reg = lock(&state.registry);
            match ev {
                VolumeEvent::Arrived { volume } | VolumeEvent::Changed { volume } => {
                    reg.upsert(*volume);
                }
                VolumeEvent::Removed { volume } => {
                    let id = volume.id();
                    let has = state.has_index(&id);
                    reg.remove(&id, has);
                    drop(reg);
                    live::stop(state, &id);
                    jobs::emit_volumes(app, state);
                    continue;
                }
                VolumeEvent::DriveLetters { .. } => continue,
            }
        }
        jobs::emit_volumes(app, state);
    }
}

/// Builds and runs the Tauri application. Blocks until the app exits.
///
/// # Panics
///
/// Panics if the Tauri runtime fails to start (e.g. WebView2 is missing and
/// could not be bootstrapped); there is no UI to report the error through.
pub fn run() {
    let state = Arc::new(AppState::default());
    // NOTE: single-instance must be registered first so a second launch
    // exits before creating any windows of its own.
    let builder =
        tauri::Builder::default().plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            features::on_second_instance(app, argv, cwd);
        }));
    let app = features::register(builder)
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(updater::init())
        .manage(state.clone())
        .setup(move |app| {
            let window = app
                .get_webview_window("main")
                .ok_or("main window missing from tauri.conf.json")?;
            let applied = backdrop::apply(&window);
            // NOTE: a failed overlay only loses the Snap Layouts flyout; the
            // custom title bar keeps working, so it never blocks startup.
            if !window.is_decorated().unwrap_or(true) {
                let _ = snap::install(&window);
            }
            app.manage(commands::app::WindowState { backdrop: applied });
            let launch = features::setup(app);
            // The backend shares the store the shell features opened.
            let _ = state.store.set(features::store::handle(app.handle()).ok());
            if let Some(settings) = state.store().and_then(|s| s.load_settings().ok()) {
                features::settings::apply_runtime(app.handle(), &settings);
            }
            helper::start_watchdog(app.handle(), &state);
            // The window starts hidden so users never see a transparent frame
            // before the backdrop or the CSS background is in place.
            if launch.show_window {
                window.show()?;
            }
            start_background(app.handle(), &state);
            Ok(())
        })
        .invoke_handler(invoke_handler())
        .build(tauri::generate_context!())
        .expect("failed to build Strata");
    app.run(|handle, event| {
        if let tauri::RunEvent::Exit = event
            && let Some(st) = handle.try_state::<Arc<AppState>>()
        {
            live::shutdown(&st);
            activity::shutdown(&st);
        }
    });
}
