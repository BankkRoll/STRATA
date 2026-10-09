//! Settings commands (SPEC §19).
//!
//! The settings model, its validation, versioning and export format live in
//! `strata-store`; this module adds the app-level checks, the file dialogs,
//! the `settings://changed` event and the side effects of a change (launch
//! at login, the tray icon, the low-space monitor).

use std::path::Path;

use serde::Serialize;
use strata_store::{Settings, SettingsIssue, Store};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_dialog::DialogExt;

use super::error::{ErrorKind, FeatureError, FeatureResult, blocking};

/// Event emitted with the full [`Settings`] after every successful change.
pub const SETTINGS_CHANGED: &str = "settings://changed";

/// Largest settings file accepted by import.
const MAX_IMPORT_BYTES: u64 = 1024 * 1024;

/// Smallest useful low-space threshold.
pub const MIN_LOW_SPACE_BYTES: u64 = 100 * 1024 * 1024;

/// Store validation plus the checks only the app knows about.
#[must_use]
pub fn validate(settings: &Settings) -> Vec<SettingsIssue> {
    let mut issues = settings.validate();
    if settings.tray.low_space_threshold_bytes < MIN_LOW_SPACE_BYTES {
        issues.push(SettingsIssue {
            key: "tray.low_space_threshold_bytes".into(),
            message: format!("must be at least {MIN_LOW_SPACE_BYTES} bytes"),
        });
    }
    if settings.startup.start_minimized_to_tray && !settings.tray.enabled {
        issues.push(SettingsIssue {
            key: "startup.start_minimized_to_tray".into(),
            message: "needs the tray icon (tray.enabled)".into(),
        });
    }
    issues
}

/// Something the app must do because a setting changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Register or unregister launch at login.
    LaunchAtLogin(bool),
    /// Show or hide the tray icon.
    Tray(bool),
    /// Re-evaluate free space now (threshold or notification toggled).
    RecheckSpace,
    /// Repaint the tray glance (units changed).
    RefreshTray,
}

/// The effects of going from `old` to `new`.
#[must_use]
pub fn effects(old: &Settings, new: &Settings) -> Vec<Effect> {
    let mut out = Vec::new();
    if old.startup.launch_at_login != new.startup.launch_at_login {
        out.push(Effect::LaunchAtLogin(new.startup.launch_at_login));
    }
    if old.tray.enabled != new.tray.enabled {
        out.push(Effect::Tray(new.tray.enabled));
    }
    if old.tray.low_space_notification != new.tray.low_space_notification
        || old.tray.low_space_threshold_bytes != new.tray.low_space_threshold_bytes
    {
        out.push(Effect::RecheckSpace);
    }
    if old.appearance.units != new.appearance.units {
        out.push(Effect::RefreshTray);
    }
    out
}

/// Validates and saves, returning the stored settings and their effects.
///
/// # Errors
///
/// [`ErrorKind::InvalidSettings`] (nothing written) or store errors.
pub fn save(store: &Store, new: &Settings) -> FeatureResult<(Settings, Vec<Effect>)> {
    let issues = validate(new);
    if !issues.is_empty() {
        return Err(FeatureError::settings(issues));
    }
    let old = store.load_settings()?;
    store.save_settings(new)?;
    let saved = store.load_settings()?;
    let fx = effects(&old, &saved);
    Ok((saved, fx))
}

/// Writes the export JSON to `path`.
///
/// # Errors
///
/// Store or file errors.
pub fn export_to(store: &Store, path: &Path) -> FeatureResult<()> {
    let json = store.export_settings()?;
    std::fs::write(path, json)
        .map_err(|e| FeatureError::io("could not write the settings file", &e))
}

/// Reads, validates and applies a settings file. Nothing is written when
/// any value is invalid.
///
/// # Errors
///
/// Unreadable, oversized, foreign or invalid files.
pub fn import_from(store: &Store, path: &Path) -> FeatureResult<(Settings, Vec<Effect>)> {
    let len = std::fs::metadata(path)
        .map_err(|e| FeatureError::io("could not read the settings file", &e))?
        .len();
    if len > MAX_IMPORT_BYTES {
        return Err(FeatureError::invalid(
            "that file is too large to be a Strata settings export",
        ));
    }
    let json = std::fs::read_to_string(path)
        .map_err(|e| FeatureError::io("could not read the settings file", &e))?;
    let old = store.load_settings()?;
    // Check the app-level rules on the decoded file before anything is
    // written: parse it into a scratch store first.
    let scratch = tempdir_store()?;
    let candidate = scratch.0.import_settings(&json)?;
    let issues = validate(&candidate);
    if !issues.is_empty() {
        return Err(FeatureError::settings(issues));
    }
    let imported = store.import_settings(&json)?;
    let fx = effects(&old, &imported);
    Ok((imported, fx))
}

/// A throwaway store in a unique temp directory, removed on drop.
struct ScratchStore(Store, std::path::PathBuf);

impl Drop for ScratchStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn tempdir_store() -> FeatureResult<ScratchStore> {
    let dir = std::env::temp_dir()
        .join("strata-settings-import")
        .join(super::consent::random_token()?);
    let store = Store::open(&dir)?;
    Ok(ScratchStore(store, dir))
}

/// Applies effects and tells every window about the new settings.
pub fn announce<R: Runtime>(app: &AppHandle<R>, settings: &Settings, fx: &[Effect]) {
    for e in fx {
        match *e {
            Effect::LaunchAtLogin(on) => {
                let _ = super::startup::set_launch_at_login(app, on);
            }
            Effect::Tray(on) => super::tray::set_visible(app, on),
            Effect::RecheckSpace => super::tray::reset_alerts(app),
            Effect::RefreshTray => {}
        }
    }
    if !fx.is_empty() {
        super::tray::wake(app);
    }
    let _ = app.emit(SETTINGS_CHANGED, settings);
}

/// Current settings (store types; snake_case keys as in the store).
#[tauri::command]
pub async fn settings_load<R: Runtime>(app: AppHandle<R>) -> FeatureResult<Settings> {
    let store = super::store::handle(&app)?;
    blocking(move || Ok(store.load_settings()?)).await
}

/// Validation issues for a draft, without saving (empty = valid).
#[tauri::command]
pub fn settings_validate(settings: Settings) -> Vec<SettingsIssue> {
    validate(&settings)
}

/// Validates and saves; emits `settings://changed`.
#[tauri::command]
pub async fn settings_save<R: Runtime>(
    app: AppHandle<R>,
    settings: Settings,
) -> FeatureResult<Settings> {
    let store = super::store::handle(&app)?;
    let (saved, fx) = blocking(move || save(&store, &settings)).await?;
    announce(&app, &saved, &fx);
    Ok(saved)
}

/// Response of `settings_export` / `settings_import`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsFileResult {
    /// The chosen file, or `null` when the user cancelled the dialog.
    pub path: Option<String>,
    /// The imported settings (import only).
    pub settings: Option<Settings>,
}

/// Asks where to save and writes the settings export there.
#[tauri::command]
pub async fn settings_export<R: Runtime>(app: AppHandle<R>) -> FeatureResult<SettingsFileResult> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        let mut dialog = app
            .dialog()
            .file()
            .add_filter("Strata settings", &["json"])
            .set_file_name("strata-settings.json");
        if let Some(w) = app.get_webview_window("main") {
            dialog = dialog.set_parent(&w);
        }
        let Some(file) = dialog.blocking_save_file() else {
            return Ok(SettingsFileResult {
                path: None,
                settings: None,
            });
        };
        let path = file
            .into_path()
            .map_err(|e| FeatureError::new(ErrorKind::Io, format!("unusable file path: {e}")))?;
        export_to(&store, &path)?;
        Ok(SettingsFileResult {
            path: Some(path.display().to_string()),
            settings: None,
        })
    })
    .await
}

/// Asks for a settings export and applies it; emits `settings://changed`.
#[tauri::command]
pub async fn settings_import<R: Runtime>(app: AppHandle<R>) -> FeatureResult<SettingsFileResult> {
    let store = super::store::handle(&app)?;
    let app2 = app.clone();
    let result = blocking(move || {
        let mut dialog = app2
            .dialog()
            .file()
            .add_filter("Strata settings", &["json"]);
        if let Some(w) = app2.get_webview_window("main") {
            dialog = dialog.set_parent(&w);
        }
        let Some(file) = dialog.blocking_pick_file() else {
            return Ok(None);
        };
        let path = file
            .into_path()
            .map_err(|e| FeatureError::new(ErrorKind::Io, format!("unusable file path: {e}")))?;
        let (settings, fx) = import_from(&store, &path)?;
        Ok(Some((path, settings, fx)))
    })
    .await?;
    Ok(match result {
        None => SettingsFileResult {
            path: None,
            settings: None,
        },
        Some((path, settings, fx)) => {
            announce(&app, &settings, &fx);
            SettingsFileResult {
                path: Some(path.display().to_string()),
                settings: Some(settings),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn validation_adds_app_rules_to_store_rules() {
        let mut s = Settings::default();
        assert!(validate(&s).is_empty(), "defaults are valid");
        s.tray.low_space_threshold_bytes = 1;
        s.startup.start_minimized_to_tray = true;
        s.live.update_tick_ms = 1;
        let keys: Vec<String> = validate(&s).into_iter().map(|i| i.key).collect();
        assert!(keys.contains(&"tray.low_space_threshold_bytes".to_string()));
        assert!(keys.contains(&"startup.start_minimized_to_tray".to_string()));
        assert!(keys.contains(&"live.update_tick_ms".to_string()));
    }

    #[test]
    fn save_rejects_invalid_and_reports_effects() {
        let (_d, store) = store();
        let mut bad = Settings::default();
        bad.history.retention_days = 1;
        let e = save(&store, &bad).unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidSettings);
        assert_eq!(store.load_settings().unwrap(), Settings::default());

        let mut s = Settings::default();
        s.startup.launch_at_login = true;
        s.tray.enabled = true;
        s.tray.low_space_threshold_bytes = 5 * 1024 * 1024 * 1024;
        let (saved, fx) = save(&store, &s).unwrap();
        assert_eq!(saved, s);
        assert_eq!(
            fx,
            [
                Effect::LaunchAtLogin(true),
                Effect::Tray(true),
                Effect::RecheckSpace
            ]
        );
        let (_, fx) = save(&store, &s).unwrap();
        assert!(fx.is_empty(), "no change, no effects");
    }

    #[test]
    fn export_then_import_round_trips_through_a_file() {
        let (_d, a) = store();
        let mut s = Settings::default();
        s.appearance.compact_density = true;
        s.tray.enabled = true;
        save(&a, &s).unwrap();
        let file_dir = tempfile::tempdir().unwrap();
        let file = file_dir.path().join("strata-settings.json");
        export_to(&a, &file).unwrap();

        let (_d2, b) = store();
        let (imported, fx) = import_from(&b, &file).unwrap();
        assert_eq!(imported, s);
        assert_eq!(b.load_settings().unwrap(), s);
        assert_eq!(fx, [Effect::Tray(true)]);
    }

    #[test]
    fn import_rejects_foreign_invalid_and_huge_files() {
        let (_d, store) = store();
        let dir = tempfile::tempdir().unwrap();
        let foreign = dir.path().join("x.json");
        std::fs::write(&foreign, r#"{"format":"something-else"}"#).unwrap();
        assert!(import_from(&store, &foreign).is_err());

        // A valid store export that breaks an app-level rule.
        let (_d2, other) = store_with(|s| {
            s.tray.low_space_threshold_bytes = 1;
        });
        let file = dir.path().join("low.json");
        std::fs::write(&file, other.export_settings().unwrap()).unwrap();
        let e = import_from(&store, &file).unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidSettings);
        assert_eq!(
            store.load_settings().unwrap(),
            Settings::default(),
            "nothing written"
        );

        let huge = dir.path().join("huge.json");
        std::fs::write(&huge, vec![b' '; (MAX_IMPORT_BYTES + 1) as usize]).unwrap();
        assert_eq!(
            import_from(&store, &huge).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
    }

    /// A store whose saved settings break only app-level rules (the store's
    /// own validation does not know them).
    fn store_with(f: impl FnOnce(&mut Settings)) -> (tempfile::TempDir, Store) {
        let (d, s) = store();
        let mut settings = Settings::default();
        f(&mut settings);
        s.save_settings(&settings).unwrap();
        (d, s)
    }
}
