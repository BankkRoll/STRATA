//! Strata desktop app: the unelevated UI process.
//!
//! Responsibilities:
//! - Owns the main window (backdrop, single-instance enforcement).
//! - Exposes Tauri commands to the WebView frontend.
//! - Later milestones wire in the index, layout, classifier, store and the
//!   pipe client for the elevated helper.

mod backdrop;

use serde::Serialize;
use tauri::Manager;

/// Static facts about the running app, returned by [`app_info`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    /// Semantic version of this build.
    pub version: String,
    /// Windows build number, or 0 on other platforms.
    pub windows_build: u32,
    /// Backdrop applied to the main window.
    pub backdrop: backdrop::Backdrop,
}

/// Managed state recording which backdrop was applied at startup.
#[derive(Debug)]
struct WindowState {
    backdrop: backdrop::Backdrop,
}

#[tauri::command]
fn app_info(app: tauri::AppHandle, state: tauri::State<'_, WindowState>) -> AppInfo {
    AppInfo {
        version: app.package_info().version.to_string(),
        windows_build: backdrop::windows_build(),
        backdrop: state.backdrop,
    }
}

/// Builds and runs the Tauri application. Blocks until the app exits.
///
/// # Panics
///
/// Panics if the Tauri runtime fails to start (e.g. WebView2 is missing and
/// could not be bootstrapped); there is no UI to report the error through.
pub fn run() {
    tauri::Builder::default()
        // NOTE: must be registered first so a second launch exits before
        // creating any windows of its own.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .setup(|app| {
            let window = app
                .get_webview_window("main")
                .ok_or("main window missing from tauri.conf.json")?;
            let applied = backdrop::apply(&window);
            app.manage(WindowState { backdrop: applied });
            // The window starts hidden so users never see a transparent frame
            // before the backdrop or the CSS background is in place.
            window.show()?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![app_info])
        .run(tauri::generate_context!())
        .expect("failed to run Strata");
}
