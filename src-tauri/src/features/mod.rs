//! App-side features beyond scanning: store, settings, cleanup, tools,
//! history, tray and startup.
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
//! Wiring in `lib.rs` is two calls: [`register`] on the builder and
//! [`setup`] in the setup hook; commands are registered in `app_commands!`.

pub mod audit;
pub mod cleanup;
pub mod consent;
pub mod error;
pub mod helper;
pub mod history;
pub mod links;
pub mod locks;
pub mod queue;
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
    app.manage(queue::CleanupQueue::default());
    app.manage(crate::insights::RecommendationCache::default());

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
