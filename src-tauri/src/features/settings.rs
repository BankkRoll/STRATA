//! Settings commands.
//!
//! The settings model, its validation, versioning and export format live in
//! `strata-store`; this module adds the app-level checks, the file dialogs,
//! the `settings://changed` event and the side effects of a change (launch
//! at login, the tray icon, the low-space monitor).

use serde::Serialize;
use strata_store::{Settings, SettingsIssue, Store};
use tauri::{AppHandle, Emitter, Runtime};

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

/// Validates and applies a settings export. Nothing is written when any
/// value is invalid, including the app-level rules.
///
/// # Errors
///
/// Oversized, foreign or invalid exports (`invalid_settings` lists the
/// issues in `detail`).
pub fn import_json(store: &Store, json: &str) -> FeatureResult<(Settings, Vec<Effect>)> {
    if json.len() as u64 > MAX_IMPORT_BYTES {
        return Err(FeatureError::invalid(
            "that file is too large to be a Strata settings export",
        ));
    }
    let old = store.load_settings()?;
    // Check the app-level rules on the decoded export before anything is
    // written: parse it into a scratch store first.
    let scratch = tempdir_store()?;
    let candidate = scratch.0.import_settings(json)?;
    let issues = validate(&candidate);
    if !issues.is_empty() {
        return Err(FeatureError::settings(issues));
    }
    let imported = store.import_settings(json)?;
    let fx = effects(&old, &imported);
    Ok((imported, fx))
}

// A throwaway store in a unique temp directory, removed on drop.
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

/// Applies the settings the running app reads continuously (the update
/// policy). Called at startup and after every change.
pub fn apply_runtime<R: Runtime>(app: &AppHandle<R>, settings: &Settings) {
    crate::updater::set_policy(
        app,
        crate::updater::Policy {
            channel: match settings.updates.channel {
                strata_store::UpdateChannel::Stable => crate::updater::Channel::Stable,
                strata_store::UpdateChannel::Beta => crate::updater::Channel::Beta,
            },
            auto_download: settings.updates.auto_download,
        },
    );
}

/// Applies effects and tells every window about the new settings.
pub fn announce<R: Runtime>(app: &AppHandle<R>, settings: &Settings, fx: &[Effect]) {
    apply_runtime(app, settings);
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

/// Result of saving or importing (`SettingsResult` in the UI).
#[derive(Debug, Clone, Serialize)]
pub struct SettingsResult {
    /// The settings as stored, or the current ones when nothing was written.
    pub settings: Settings,
    /// Validation issues; non-empty means nothing was written.
    pub issues: Vec<SettingsIssue>,
}

/// Validates and saves (`settings_save`); applies side effects and emits
/// `settings://changed`. Invalid settings come back as `issues`, not as an
/// error, and nothing is written.
#[tauri::command]
pub async fn settings_save<R: Runtime>(
    app: AppHandle<R>,
    settings: Settings,
) -> FeatureResult<SettingsResult> {
    let store = super::store::handle(&app)?;
    let issues = validate(&settings);
    if !issues.is_empty() {
        let current = blocking(move || Ok(store.load_settings()?)).await?;
        return Ok(SettingsResult {
            settings: current,
            issues,
        });
    }
    let (saved, fx) = blocking(move || save(&store, &settings)).await?;
    announce(&app, &saved, &fx);
    Ok(SettingsResult {
        settings: saved,
        issues: Vec::new(),
    })
}

/// The settings as a `strata-settings` JSON export (`settings_export`).
#[tauri::command]
pub async fn settings_export<R: Runtime>(app: AppHandle<R>) -> FeatureResult<String> {
    let store = super::store::handle(&app)?;
    blocking(move || Ok(store.export_settings()?)).await
}

/// Validates then applies an export (`settings_import`); writes nothing
/// when any value is invalid.
#[tauri::command]
pub async fn settings_import<R: Runtime>(
    app: AppHandle<R>,
    json: String,
) -> FeatureResult<SettingsResult> {
    let store = super::store::handle(&app)?;
    let result = blocking(move || {
        Ok(match import_json(&store, &json) {
            Ok((settings, fx)) => (
                SettingsResult {
                    settings,
                    issues: Vec::new(),
                },
                Some(fx),
            ),
            Err(e) if e.code == ErrorKind::InvalidSettings => (
                SettingsResult {
                    settings: store.load_settings()?,
                    issues: e
                        .detail
                        .and_then(|d| serde_json::from_value(d).ok())
                        .unwrap_or_default(),
                },
                None,
            ),
            Err(e) => return Err(e),
        })
    })
    .await?;
    if let (r, Some(fx)) = &result {
        announce(&app, &r.settings, fx);
    }
    Ok(result.0)
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
        assert_eq!(e.code, ErrorKind::InvalidSettings);
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
    fn export_then_import_round_trips() {
        let (_d, a) = store();
        let mut s = Settings::default();
        s.appearance.compact_density = true;
        s.tray.enabled = true;
        save(&a, &s).unwrap();
        let json = a.export_settings().unwrap();

        let (_d2, b) = store();
        let (imported, fx) = import_json(&b, &json).unwrap();
        assert_eq!(imported, s);
        assert_eq!(b.load_settings().unwrap(), s);
        assert_eq!(fx, [Effect::Tray(true)]);
    }

    #[test]
    fn import_rejects_foreign_invalid_and_huge_exports() {
        let (_d, store) = store();
        assert!(import_json(&store, r#"{"format":"something-else"}"#).is_err());

        // A valid store export that breaks an app-level rule.
        let (_d2, other) = store_with(|s| {
            s.tray.low_space_threshold_bytes = 1;
        });
        let e = import_json(&store, &other.export_settings().unwrap()).unwrap_err();
        assert_eq!(e.code, ErrorKind::InvalidSettings);
        assert_eq!(
            store.load_settings().unwrap(),
            Settings::default(),
            "nothing written"
        );

        let huge = " ".repeat(MAX_IMPORT_BYTES as usize + 1);
        assert_eq!(
            import_json(&store, &huge).unwrap_err().code,
            ErrorKind::BadRequest
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
