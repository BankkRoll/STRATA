//! Background updates and rollback, built on `tauri-plugin-updater`.
//!
//! Responsibilities:
//! - Checks the release feed for the configured [`Channel`] shortly after
//!   launch and then daily. Release builds only, and only when a real updater
//!   public key is configured.
//! - Downloads and verifies the signed installer in the background, then runs
//!   it when the app exits ("apply on restart").
//! - Detects a freshly installed version that never finished loading its main
//!   page and offers to reinstall the previous version, verified with the same
//!   updater signature.
//!
//! The rollback state lives in `%LOCALAPPDATA%\<identifier>\update-state.json`.
//! It only records version strings.

use std::{fs, path::PathBuf, sync::Mutex, thread, time::Duration};

use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Emitter, Manager, RunEvent, Runtime, Url,
    plugin::{Builder as PluginBuilder, TauriPlugin},
    webview::PageLoadEvent,
};
use tauri_plugin_updater::{Update, UpdaterExt};

/// Event emitted when an update is downloaded and will install on exit.
/// The payload is an [`UpdateInfo`].
pub const UPDATE_READY_EVENT: &str = "strata://update-ready";

/// Event emitted when an update exists but [`Policy::auto_download`] is off.
/// The payload is an [`UpdateInfo`].
pub const UPDATE_AVAILABLE_EVENT: &str = "strata://update-available";

const FIRST_CHECK_DELAY: Duration = Duration::from_secs(20);
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const STATE_FILE: &str = "update-state.json";
/// Marks the unconfigured key shipped in `tauri.conf.json`.
const PLACEHOLDER_KEY_PREFIX: &str = "REPLACE_WITH";
/// Rolling pre-release whose `latest.json` the release workflow repoints at
/// every published release, stable or pre-release.
const BETA_TAG: &str = "updater-beta";
const STABLE_SUFFIX: &str = "/latest/download/latest.json";

/// Release channel to follow. Mirrors `strata_store::UpdateChannel`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Published, non-pre-release versions only.
    #[default]
    Stable,
    /// The newest published release, including pre-releases.
    Beta,
}

/// How the background updater behaves. Mirrors `strata_store::UpdateSettings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Channel to follow.
    pub channel: Channel,
    /// Download in the background and install on exit. When off, the app only
    /// emits [`UPDATE_AVAILABLE_EVENT`].
    pub auto_download: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            channel: Channel::Stable,
            auto_download: true,
        }
    }
}

/// Payload of [`UPDATE_READY_EVENT`] and [`UPDATE_AVAILABLE_EVENT`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    /// Version that will be installed.
    pub version: String,
    /// Release notes from the manifest, if any.
    pub notes: Option<String>,
}

struct UpdaterState {
    policy: Mutex<Policy>,
    ready: Mutex<Option<(Update, Vec<u8>)>>,
}

/// A version that was just installed and has not yet reached first page load.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
struct Pending {
    from: String,
    to: String,
    launches: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    pending: Option<Pending>,
}

#[derive(Debug, PartialEq, Eq)]
enum LaunchAction {
    Nothing,
    Clear,
    Record(Pending),
    OfferRollback(Pending),
}

/// Decides what a launch means for an in-flight update.
///
/// The first launch of the new version is recorded; reaching it a second time
/// without the page-load marker having cleared it means the previous launch
/// failed. Running any other version means the update never applied (UAC was
/// declined) or a rollback already happened, so the record is stale.
fn on_launch(pending: Option<Pending>, current: &str) -> LaunchAction {
    match pending {
        None => LaunchAction::Nothing,
        Some(p) if p.to == current && p.launches >= 1 => LaunchAction::OfferRollback(p),
        Some(p) if p.to == current => LaunchAction::Record(Pending {
            launches: p.launches + 1,
            ..p
        }),
        Some(_) => LaunchAction::Clear,
    }
}

/// `https://github.com/o/r/releases/latest/download/latest.json` → `https://github.com/o/r/releases`.
fn releases_base(stable_endpoint: &str) -> Option<&str> {
    stable_endpoint
        .strip_suffix(STABLE_SUFFIX)
        .filter(|b| b.ends_with("/releases"))
}

fn channel_endpoint(base: &str, channel: Channel) -> String {
    match channel {
        Channel::Stable => format!("{base}{STABLE_SUFFIX}"),
        Channel::Beta => format!("{base}/download/{BETA_TAG}/latest.json"),
    }
}

fn version_endpoint(base: &str, version: &str) -> String {
    format!("{base}/download/v{version}/latest.json")
}

/// Release feed base URL, or `None` when updates are disabled for this build.
fn feed<R: Runtime>(app: &AppHandle<R>) -> Option<String> {
    if cfg!(debug_assertions) {
        return None;
    }
    let config = app.config().plugins.0.get("updater")?;
    let pubkey = config.get("pubkey")?.as_str()?;
    if pubkey.trim().is_empty() || pubkey.starts_with(PLACEHOLDER_KEY_PREFIX) {
        return None;
    }
    let endpoint = config.get("endpoints")?.get(0)?.as_str()?;
    releases_base(endpoint).map(str::to_owned)
}

fn state_path<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    app.path()
        .app_local_data_dir()
        .ok()
        .map(|d| d.join(STATE_FILE))
}

fn load_pending<R: Runtime>(app: &AppHandle<R>) -> Option<Pending> {
    let bytes = fs::read(state_path(app)?).ok()?;
    serde_json::from_slice::<StateFile>(&bytes).ok()?.pending
}

fn save_pending<R: Runtime>(app: &AppHandle<R>, pending: Option<Pending>) {
    let Some(path) = state_path(app) else { return };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec(&StateFile { pending }) {
        let _ = fs::write(path, json);
    }
}

fn current_version<R: Runtime>(app: &AppHandle<R>) -> String {
    app.package_info().version.to_string()
}

/// Whether update checks are enabled for this build (a release build with a
/// real updater public key).
pub fn configured<R: Runtime>(app: &AppHandle<R>) -> bool {
    feed(app).is_some()
}

/// Whether a downloaded update is waiting to be installed.
pub fn is_ready<R: Runtime>(app: &AppHandle<R>) -> bool {
    app.try_state::<UpdaterState>()
        .and_then(|s| s.ready.lock().ok().map(|r| r.is_some()))
        .unwrap_or(false)
}

/// The policy in effect.
pub fn policy<R: Runtime>(app: &AppHandle<R>) -> Policy {
    app.try_state::<UpdaterState>()
        .and_then(|s| s.policy.lock().ok().map(|p| *p))
        .unwrap_or_default()
}

/// Checks the configured channel now. With auto-download on, a newer
/// version is downloaded and [`UPDATE_READY_EVENT`] follows; otherwise
/// [`UPDATE_AVAILABLE_EVENT`] is emitted. Returns the newer version, if any.
///
/// # Errors
///
/// Updates are not configured in this build, or the feed could not be read.
pub async fn check_now<R: Runtime>(app: &AppHandle<R>) -> Result<Option<UpdateInfo>, String> {
    let base = feed(app).ok_or_else(|| "updates are not configured in this build".to_owned())?;
    check_once(app, &base).await.map_err(|e| e.to_string())
}

/// Replaces the update policy, e.g. after the settings store loads or changes.
/// Takes effect at the next check.
pub fn set_policy<R: Runtime>(app: &AppHandle<R>, policy: Policy) {
    if let Some(state) = app.try_state::<UpdaterState>()
        && let Ok(mut current) = state.policy.lock()
    {
        *current = policy;
    }
}

/// Installs a downloaded update now and restarts into it. On Windows this
/// exits the process; it returns only if nothing was ready or the installer
/// failed to launch.
///
/// # Errors
///
/// Returns the updater error if the installer could not be started.
pub fn install_now<R: Runtime>(app: &AppHandle<R>) -> Result<(), tauri_plugin_updater::Error> {
    install_ready(app, true)
}

fn install_ready<R: Runtime>(
    app: &AppHandle<R>,
    restart: bool,
) -> Result<(), tauri_plugin_updater::Error> {
    let Some(state) = app.try_state::<UpdaterState>() else {
        return Ok(());
    };
    let Some((update, bytes)) = state.ready.lock().ok().and_then(|mut r| r.take()) else {
        return Ok(());
    };
    save_pending(
        app,
        Some(Pending {
            from: current_version(app),
            to: update.version.clone(),
            launches: 0,
        }),
    );
    let result = update.restart_after_install(restart).install(bytes);
    if result.is_err() {
        save_pending(app, None);
    }
    result
}

async fn check_once<R: Runtime>(
    app: &AppHandle<R>,
    base: &str,
) -> tauri_plugin_updater::Result<Option<UpdateInfo>> {
    let policy = app
        .try_state::<UpdaterState>()
        .and_then(|s| s.policy.lock().ok().map(|p| *p))
        .unwrap_or_default();
    let url = Url::parse(&channel_endpoint(base, policy.channel))?;
    let Some(update) = app
        .updater_builder()
        .endpoints(vec![url])?
        .build()?
        .check()
        .await?
    else {
        return Ok(None);
    };
    let info = UpdateInfo {
        version: update.version.clone(),
        notes: update.body.clone(),
    };
    if !policy.auto_download {
        let _ = app.emit(UPDATE_AVAILABLE_EVENT, &info);
        return Ok(Some(info));
    }
    let bytes = update.download(|_, _| {}, || {}).await?;
    if let Some(state) = app.try_state::<UpdaterState>()
        && let Ok(mut ready) = state.ready.lock()
    {
        *ready = Some((update, bytes));
    }
    let _ = app.emit(UPDATE_READY_EVENT, &info);
    Ok(Some(info))
}

fn spawn_checks<R: Runtime>(app: AppHandle<R>, base: String) {
    thread::spawn(move || {
        thread::sleep(FIRST_CHECK_DELAY);
        loop {
            let already_ready = app
                .try_state::<UpdaterState>()
                .and_then(|s| s.ready.lock().ok().map(|r| r.is_some()))
                .unwrap_or(false);
            if !already_ready
                && let Err(e) = tauri::async_runtime::block_on(check_once(&app, &base))
            {
                // NOTE: offline machines and GitHub hiccups are normal; never
                // surface them, just try again tomorrow.
                eprintln!("update check failed: {e}");
            }
            thread::sleep(CHECK_INTERVAL);
        }
    });
}

fn offer_rollback<R: Runtime>(app: &AppHandle<R>, base: &str, pending: &Pending) {
    let releases = base.to_owned();
    let question = format!(
        "Strata {} did not start correctly last time.\n\n\
         Reinstall the previous version ({})?",
        pending.to, pending.from
    );
    if !native::confirm("Strata update problem", &question) {
        return;
    }
    let app = app.clone();
    let from = pending.from.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = reinstall(&app, &releases, &from).await {
            native::inform(
                "Strata update problem",
                &format!(
                    "Strata {from} could not be reinstalled ({e}).\n\n\
                     Download it from {releases}"
                ),
            );
        }
    });
}

/// Downloads and runs the installer of `version` from its own release, signed
/// like any update. Exits the app on success.
async fn reinstall<R: Runtime>(
    app: &AppHandle<R>,
    base: &str,
    version: &str,
) -> tauri_plugin_updater::Result<()> {
    let url = Url::parse(&version_endpoint(base, version))?;
    let wanted = version.to_owned();
    let update = app
        .updater_builder()
        .endpoints(vec![url])?
        // The manifest is the previous release's own `latest.json`, so accept
        // exactly that version even though it is older than the running one.
        .version_comparator(move |_, remote| remote.version.to_string() == wanted)
        .build()?
        .check()
        .await?
        .ok_or(tauri_plugin_updater::Error::ReleaseNotFound)?;
    update.download_and_install(|_, _| {}, || {}).await
}

/// Builds the plugin that drives background updates and rollback. Register it
/// after `tauri_plugin_updater`.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    PluginBuilder::new("strata-updater")
        .setup(|app, _api| {
            app.manage(UpdaterState {
                policy: Mutex::new(Policy::default()),
                ready: Mutex::new(None),
            });
            let Some(base) = feed(app) else {
                return Ok(());
            };
            match on_launch(load_pending(app), &current_version(app)) {
                LaunchAction::Nothing => {}
                LaunchAction::Clear => save_pending(app, None),
                LaunchAction::Record(p) => save_pending(app, Some(p)),
                LaunchAction::OfferRollback(p) => {
                    // Cleared first so a failed or declined rollback never nags twice.
                    save_pending(app, None);
                    offer_rollback(app, &base, &p);
                }
            }
            spawn_checks(app.clone(), base);
            Ok(())
        })
        .on_page_load(|webview, payload| {
            // NOTE: "page finished loading" is the best first-paint signal
            // available without the frontend's cooperation.
            if webview.label() != "main" || payload.event() != PageLoadEvent::Finished {
                return;
            }
            let app = webview.app_handle();
            if let Some(p) = load_pending(app)
                && p.to == current_version(app)
            {
                save_pending(app, None);
            }
        })
        .on_event(|app, event| {
            if let RunEvent::Exit = event
                && let Err(e) = install_ready(app, false)
            {
                eprintln!("update install failed: {e}");
            }
        })
        .build()
}

#[cfg(windows)]
mod native {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_YESNO,
        MESSAGEBOX_STYLE, MessageBoxW,
    };
    use windows::core::HSTRING;

    // NOTE: a native box, not a webview dialog, because a broken frontend is
    // exactly what the rollback prompt has to work around.
    fn show(title: &str, text: &str, style: MESSAGEBOX_STYLE) -> bool {
        // SAFETY: both strings are valid, NUL-terminated HSTRINGs that outlive
        // the call, and a null owner window is allowed.
        let result = unsafe {
            MessageBoxW(
                None,
                &HSTRING::from(text),
                &HSTRING::from(title),
                style | MB_SETFOREGROUND,
            )
        };
        result == IDYES
    }

    pub(super) fn confirm(title: &str, text: &str) -> bool {
        show(title, text, MB_YESNO | MB_ICONWARNING)
    }

    pub(super) fn inform(title: &str, text: &str) {
        show(title, text, MB_OK | MB_ICONINFORMATION);
    }
}

#[cfg(not(windows))]
mod native {
    pub(super) fn confirm(_title: &str, _text: &str) -> bool {
        false
    }

    pub(super) fn inform(_title: &str, _text: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(to: &str, launches: u32) -> Pending {
        Pending {
            from: "0.1.0".into(),
            to: to.into(),
            launches,
        }
    }

    #[test]
    fn first_launch_of_new_version_is_recorded() {
        assert_eq!(
            on_launch(Some(pending("0.2.0", 0)), "0.2.0"),
            LaunchAction::Record(pending("0.2.0", 1))
        );
    }

    #[test]
    fn second_unhealthy_launch_offers_rollback() {
        assert_eq!(
            on_launch(Some(pending("0.2.0", 1)), "0.2.0"),
            LaunchAction::OfferRollback(pending("0.2.0", 1))
        );
    }

    #[test]
    fn update_that_never_applied_is_cleared() {
        assert_eq!(
            on_launch(Some(pending("0.2.0", 0)), "0.1.0"),
            LaunchAction::Clear
        );
    }

    #[test]
    fn nothing_pending_does_nothing() {
        assert_eq!(on_launch(None, "0.2.0"), LaunchAction::Nothing);
    }

    #[test]
    fn endpoints_derive_from_the_stable_feed() {
        let base = releases_base("https://github.com/o/r/releases/latest/download/latest.json")
            .expect("github feed");
        assert_eq!(base, "https://github.com/o/r/releases");
        assert_eq!(
            channel_endpoint(base, Channel::Beta),
            "https://github.com/o/r/releases/download/updater-beta/latest.json"
        );
        assert_eq!(
            version_endpoint(base, "0.1.0"),
            "https://github.com/o/r/releases/download/v0.1.0/latest.json"
        );
        assert_eq!(
            channel_endpoint(base, Channel::Stable),
            format!("{base}{STABLE_SUFFIX}")
        );
    }

    #[test]
    fn non_github_feed_is_rejected() {
        assert_eq!(releases_base("https://example.com/latest.json"), None);
        assert_eq!(
            releases_base("https://example.com/latest/download/latest.json"),
            None
        );
    }

    #[test]
    fn policy_round_trips_with_store_names() {
        let json = r#"{"channel":"beta","auto_download":false}"#;
        let policy: Policy = serde_json::from_str(json).expect("valid policy");
        assert_eq!(
            policy,
            Policy {
                channel: Channel::Beta,
                auto_download: false
            }
        );
    }
}
