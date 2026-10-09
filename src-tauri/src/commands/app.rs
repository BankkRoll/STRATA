//! App-level commands: `app_info`, `app_capabilities`, updates and
//! licenses.

use serde::Serialize;
use tauri::{AppHandle, Runtime, State};

use crate::backdrop;
use crate::error::{CmdResult, CommandError};

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
pub struct WindowState {
    /// The applied backdrop.
    pub backdrop: backdrop::Backdrop,
}

/// Version, Windows build and backdrop.
#[tauri::command]
pub fn app_info(app: AppHandle, state: State<'_, WindowState>) -> AppInfo {
    AppInfo {
        version: app.package_info().version.to_string(),
        windows_build: backdrop::windows_build(),
        backdrop: state.backdrop,
    }
}

/// Names of the commands that work right now (see [`crate::capabilities`]).
/// The UI enables exactly these.
#[tauri::command]
pub fn app_capabilities<R: Runtime>(app: AppHandle<R>) -> Vec<&'static str> {
    crate::capabilities::available(&crate::capabilities::facts(&app))
}

/// `UpdateCheck` in `ui/src/lib/settings.ts`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheck {
    /// Running version.
    pub current: String,
    /// Newer version on the channel, if any.
    pub latest: Option<String>,
    /// A newer version exists.
    pub available: bool,
    /// Channel checked.
    pub channel: crate::updater::Channel,
}

/// Checks the configured update channel now.
///
/// # Errors
///
/// `unavailable` in builds without update checks; `io` when the release
/// feed cannot be read.
#[tauri::command]
pub async fn updates_check<R: Runtime>(app: AppHandle<R>) -> CmdResult<UpdateCheck> {
    if !crate::updater::configured(&app) {
        return Err(CommandError::unavailable(
            "Update checks are turned off in this build (development or unsigned builds).",
        ));
    }
    let found = crate::updater::check_now(&app)
        .await
        .map_err(CommandError::io)?;
    Ok(UpdateCheck {
        current: app.package_info().version.to_string(),
        available: found.is_some(),
        latest: found.map(|i| i.version),
        channel: crate::updater::policy(&app).channel,
    })
}

/// Installs the downloaded update and restarts into it ("Restart now").
///
/// # Errors
///
/// `not_found` when no update is waiting; `io` when the installer could
/// not start.
#[tauri::command]
pub fn updates_restart<R: Runtime>(app: AppHandle<R>) -> CmdResult<()> {
    if !crate::updater::is_ready(&app) {
        return Err(CommandError::not_found("no update has been downloaded yet"));
    }
    crate::updater::install_now(&app).map_err(|e| CommandError::io(e.to_string()))
}

/// One third-party component (`LicenseEntry` in `ui/src/lib/settings.ts`).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LicenseEntry {
    name: String,
    version: String,
    license: String,
    ecosystem: String,
    repository: Option<String>,
}

/// The license list the build script collected.
const LICENSES_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/licenses.json"));

/// Third-party components shipped in this build, sorted by name.
///
/// # Errors
///
/// `internal` if the embedded list is malformed.
#[tauri::command]
pub fn about_licenses() -> CmdResult<Vec<LicenseEntry>> {
    let mut list: Vec<LicenseEntry> =
        serde_json::from_str(LICENSES_JSON).map_err(|e| CommandError::internal(e.to_string()))?;
    list.sort_by_key(|l| l.name.to_lowercase());
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_license_list_parses() {
        let list = about_licenses().unwrap();
        assert!(
            list.iter()
                .all(|l| l.ecosystem == "cargo" || l.ecosystem == "npm")
        );
    }
}
