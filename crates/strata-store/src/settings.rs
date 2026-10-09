//! Typed application settings (SPEC §19), persisted as versioned key-value
//! rows in `state.db`.
//!
//! Every leaf field is its own row, keyed `section.field` (for example
//! `live.update_tick_ms`), holding a JSON value and a per-key schema version.
//! Storing leaves rather than one JSON document gives forward and backward
//! compatibility for free:
//!
//! - **Missing keys** (an older database, or a setting added since) take
//!   their default.
//! - **Unknown keys** (written by a newer Strata) are never read, never
//!   deleted, and survive `save_settings`, so a downgrade followed by an
//!   upgrade loses nothing.
//! - **Bad values** (wrong type, out of range) fall back to the default for
//!   that key alone; one damaged setting never resets the others.
//! - **Newer-versioned keys** are used when they still decode. `save_settings`
//!   leaves such a row untouched unless the user actually changed the value,
//!   so an older build does not clobber a newer format.
//!
//! Export/import uses the same leaf map wrapped in a small JSON envelope, and
//! import validates everything before writing anything.

use std::collections::{BTreeMap, HashMap};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use strata_core::SizeMode;

use crate::Store;
use crate::error::{Result, StoreError};
use crate::retention::RetentionPolicy;
use crate::snapshot::SnapshotOptions;

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

// -----------------------------------------------------------------------------
// Settings types
// -----------------------------------------------------------------------------

/// Every user setting, with defaults.
///
/// # Example
///
/// ```
/// use strata_store::{Settings, Store};
/// let dir = tempfile::tempdir().unwrap();
/// let store = Store::open(dir.path()).unwrap();
/// let mut s = store.load_settings().unwrap();
/// assert!(!s.activity.enabled, "ETW is off by default");
/// s.appearance.compact_density = true;
/// store.save_settings(&s).unwrap();
/// assert_eq!(store.load_settings().unwrap(), s);
/// ```
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Scanning.
    pub scan: ScanSettings,
    /// USN live updates.
    pub live: LiveSettings,
    /// ETW activity tracking.
    pub activity: ActivitySettings,
    /// Elevated helper.
    pub helper: HelperSettings,
    /// Cleanup and duplicates.
    pub cleanup: CleanupSettings,
    /// Snapshots and retention.
    pub history: HistorySettings,
    /// Look and feel.
    pub appearance: AppearanceSettings,
    /// Classifier rules.
    pub rules: RulesSettings,
    /// Launch behaviour.
    pub startup: StartupSettings,
    /// Notification-area icon.
    pub tray: TraySettings,
    /// Auto-update.
    pub updates: UpdateSettings,
    /// Telemetry consent.
    pub privacy: PrivacySettings,
}

/// Scan settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanSettings {
    /// Size every view shows by default.
    pub default_size_mode: SizeMode,
    /// Glob patterns excluded from scans.
    pub exclude_globs: Vec<String>,
    /// Offer mapped network drives on the home screen.
    pub include_network_drives: bool,
    /// Scan the system volume on launch.
    pub auto_scan_on_launch: bool,
    /// Scan removable drives when they are plugged in.
    pub auto_scan_removable: bool,
    /// Fallback walker threads; 0 picks automatically.
    pub walker_concurrency: u32,
    /// Show the "Strata never follows junctions or symlinks" notice.
    pub show_follow_policy: bool,
}

impl Default for ScanSettings {
    fn default() -> Self {
        Self {
            default_size_mode: SizeMode::Allocated,
            exclude_globs: Vec::new(),
            include_network_drives: false,
            auto_scan_on_launch: true,
            auto_scan_removable: false,
            walker_concurrency: 0,
            show_follow_policy: true,
        }
    }
}

/// Live-update settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveSettings {
    /// Tail the USN journal to keep the index current.
    pub usn_enabled: bool,
    /// How often coalesced changes are pushed to the UI, in ms.
    pub update_tick_ms: u32,
    /// Rescan automatically when the journal wraps or is recreated.
    pub auto_rescan_on_journal_loss: bool,
}

impl Default for LiveSettings {
    fn default() -> Self {
        Self {
            usn_enabled: true,
            update_tick_ms: 1000,
            auto_rescan_on_journal_loss: true,
        }
    }
}

/// ETW activity-tracking settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActivitySettings {
    /// Activity tracking is opt-in.
    pub enabled: bool,
    /// Days of hourly rollups to keep.
    pub retention_days: u32,
    /// Sustained CPU share above which tracing throttles, in percent.
    pub cpu_cap_percent: f64,
}

impl Default for ActivitySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            retention_days: 30,
            cpu_cap_percent: 2.0,
        }
    }
}

/// How the elevated helper runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperMode {
    /// Launched with a UAC prompt when needed.
    #[default]
    OnDemand,
    /// Installed as a Windows service.
    Service,
}

/// Helper settings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HelperSettings {
    /// On-demand or service mode.
    pub mode: HelperMode,
}

/// Default delete method offered by the cleanup review screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupMethod {
    /// Move to the Recycle Bin.
    #[default]
    RecycleBin,
    /// Delete permanently.
    Permanent,
}

/// Cleanup settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CleanupSettings {
    /// Method preselected on the review screen.
    pub default_method: CleanupMethod,
    /// Permanent deletes of items above this many bytes need a second
    /// confirmation.
    pub large_delete_confirm_bytes: u64,
    /// `node_modules` untouched for this many days counts as stale.
    pub stale_node_modules_days: u32,
    /// Installers untouched for this many days count as stale.
    pub stale_installers_days: u32,
    /// Files smaller than this are ignored by the duplicate finder.
    pub duplicates_min_bytes: u64,
}

impl Default for CleanupSettings {
    fn default() -> Self {
        Self {
            default_method: CleanupMethod::RecycleBin,
            large_delete_confirm_bytes: 10 * GIB,
            stale_node_modules_days: 90,
            stale_installers_days: 60,
            duplicates_min_bytes: MIB,
        }
    }
}

/// Snapshot and retention settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistorySettings {
    /// Hours between automatic snapshots while live.
    pub snapshot_interval_hours: u32,
    /// Days of snapshots to keep.
    pub retention_days: u32,
    /// Snapshots older than this are thinned to one per week.
    pub thin_after_days: u32,
    /// Directories smaller than this are not stored in snapshots.
    pub min_dir_bytes: u64,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            snapshot_interval_hours: 24,
            retention_days: 90,
            thin_after_days: 30,
            min_dir_bytes: SnapshotOptions::default().min_dir_bytes,
        }
    }
}

impl HistorySettings {
    /// The retention policy these settings describe.
    #[must_use]
    pub const fn retention_policy(&self) -> RetentionPolicy {
        RetentionPolicy {
            keep_days: self.retention_days,
            thin_after_days: self.thin_after_days,
        }
    }

    /// Snapshot options these settings describe.
    #[must_use]
    pub const fn snapshot_options(&self) -> SnapshotOptions {
        SnapshotOptions {
            min_dir_bytes: self.min_dir_bytes,
        }
    }
}

/// Theme preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// Follow Windows.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

/// What treemap colors encode by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorMode {
    /// Top-level category.
    #[default]
    Category,
    /// Last-modified age.
    Age,
    /// File extension / type.
    FileType,
    /// Cleanup safety tier.
    Safety,
    /// Owning application.
    App,
}

/// Treemap rendering style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreemapStyle {
    /// Flat fills.
    Flat,
    /// Cushion shading.
    #[default]
    Cushion,
}

/// Size unit system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeUnits {
    /// KiB/MiB/GiB (1024), as Explorer computes them.
    #[default]
    Binary,
    /// kB/MB/GB (1000).
    Decimal,
}

/// Appearance settings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    /// Light/dark preference.
    pub theme: Theme,
    /// Default color mode.
    pub color_mode: ColorMode,
    /// Treemap style.
    pub treemap_style: TreemapStyle,
    /// Unit system.
    pub units: SizeUnits,
    /// Compact row density.
    pub compact_density: bool,
}

/// Classifier rule settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RulesSettings {
    /// Load rule packs from the user rules folder.
    pub user_rules_enabled: bool,
    /// Custom user rules folder; `None` uses the default under `%APPDATA%`.
    pub user_rules_dir: Option<String>,
}

impl Default for RulesSettings {
    fn default() -> Self {
        Self {
            user_rules_enabled: true,
            user_rules_dir: None,
        }
    }
}

/// Startup settings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StartupSettings {
    /// Start with Windows.
    pub launch_at_login: bool,
    /// Start hidden in the notification area.
    pub start_minimized_to_tray: bool,
}

/// Tray icon settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TraySettings {
    /// Show the notification-area icon.
    pub enabled: bool,
    /// Toast when a fixed volume's free space drops below the threshold.
    pub low_space_notification: bool,
    /// Free-space threshold for the toast, in bytes.
    pub low_space_threshold_bytes: u64,
}

impl Default for TraySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            low_space_notification: true,
            low_space_threshold_bytes: 10 * GIB,
        }
    }
}

/// Update channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    /// Released builds.
    #[default]
    Stable,
    /// Pre-release builds.
    Beta,
}

/// Update settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    /// Channel to follow.
    pub channel: UpdateChannel,
    /// Download updates in the background (applied on restart).
    pub auto_download: bool,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            channel: UpdateChannel::Stable,
            auto_download: true,
        }
    }
}

/// Privacy settings. Telemetry is off unless the user opts in.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PrivacySettings {
    /// Send crash reports (never paths, names or scan data).
    pub crash_reports_opt_in: bool,
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

/// One validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsIssue {
    /// Leaf key, e.g. `live.update_tick_ms`.
    pub key: String,
    /// What is wrong.
    pub message: String,
}

fn issue(key: &str, message: impl Into<String>) -> SettingsIssue {
    SettingsIssue {
        key: key.to_owned(),
        message: message.into(),
    }
}

fn check_range<T: PartialOrd + std::fmt::Display>(
    out: &mut Vec<SettingsIssue>,
    key: &str,
    v: T,
    min: T,
    max: T,
) {
    if v < min || v > max {
        out.push(issue(key, format!("{v} is outside {min}..={max}")));
    }
}

impl Settings {
    /// Checks ranges and cross-field constraints. Empty means valid.
    #[must_use]
    pub fn validate(&self) -> Vec<SettingsIssue> {
        let mut out = Vec::new();
        let globs = &self.scan.exclude_globs;
        if globs.len() > 1000 {
            out.push(issue("scan.exclude_globs", "more than 1000 patterns"));
        }
        if globs.iter().any(|g| g.trim().is_empty() || g.len() > 1024) {
            out.push(issue(
                "scan.exclude_globs",
                "patterns must be non-empty and at most 1024 bytes",
            ));
        }
        check_range(
            &mut out,
            "scan.walker_concurrency",
            self.scan.walker_concurrency,
            0,
            256,
        );
        check_range(
            &mut out,
            "live.update_tick_ms",
            self.live.update_tick_ms,
            100,
            60_000,
        );
        check_range(
            &mut out,
            "activity.retention_days",
            self.activity.retention_days,
            1,
            365,
        );
        let cap = self.activity.cpu_cap_percent;
        if !cap.is_finite() || !(0.1..=50.0).contains(&cap) {
            out.push(issue("activity.cpu_cap_percent", "must be within 0.1..=50"));
        }
        check_range(
            &mut out,
            "cleanup.large_delete_confirm_bytes",
            self.cleanup.large_delete_confirm_bytes,
            1,
            u64::MAX,
        );
        check_range(
            &mut out,
            "cleanup.stale_node_modules_days",
            self.cleanup.stale_node_modules_days,
            1,
            3650,
        );
        check_range(
            &mut out,
            "cleanup.stale_installers_days",
            self.cleanup.stale_installers_days,
            1,
            3650,
        );
        check_range(
            &mut out,
            "cleanup.duplicates_min_bytes",
            self.cleanup.duplicates_min_bytes,
            1,
            u64::MAX,
        );
        check_range(
            &mut out,
            "history.snapshot_interval_hours",
            self.history.snapshot_interval_hours,
            1,
            24 * 30,
        );
        check_range(
            &mut out,
            "history.retention_days",
            self.history.retention_days,
            7,
            3650,
        );
        if self.history.thin_after_days > self.history.retention_days {
            out.push(issue(
                "history.thin_after_days",
                "must not exceed history.retention_days",
            ));
        }
        if self
            .rules
            .user_rules_dir
            .as_deref()
            .is_some_and(|d| d.trim().is_empty())
        {
            out.push(issue("rules.user_rules_dir", "must not be empty"));
        }
        out
    }
}

// -----------------------------------------------------------------------------
// Leaf mapping
// -----------------------------------------------------------------------------

/// Schema version of a settings key. Every key is at version 1; when a key's
/// JSON shape changes incompatibly, give it a new version here and teach
/// [`resolve`] to upgrade older rows.
fn key_version(_key: &str) -> u32 {
    1
}

/// Largest JSON value accepted for one key (export/import hardening).
const MAX_VALUE_BYTES: usize = 64 * 1024;

fn is_valid_key(key: &str) -> bool {
    key.len() <= 128
        && key.split('.').count() >= 2
        && key.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

type Leaves = BTreeMap<String, Value>;

fn to_leaves(s: &Settings) -> Result<Leaves> {
    let mut out = BTreeMap::new();
    if let Value::Object(sections) = serde_json::to_value(s)? {
        for (section, fields) in sections {
            if let Value::Object(fields) = fields {
                for (field, v) in fields {
                    out.insert(format!("{section}.{field}"), v);
                }
            }
        }
    }
    Ok(out)
}

fn from_leaves(leaves: &Leaves) -> serde_json::Result<Settings> {
    let mut root = Map::new();
    for (key, v) in leaves {
        if let Some((section, field)) = key.split_once('.') {
            let entry = root
                .entry(section.to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(m) = entry {
                m.insert(field.to_owned(), v.clone());
            }
        }
    }
    serde_json::from_value(Value::Object(root))
}

fn default_leaves() -> Leaves {
    // Serializing a plain struct of primitives cannot fail; an empty map would
    // just make every stored key "unknown", which is safe.
    to_leaves(&Settings::default()).unwrap_or_default()
}

/// A stored row.
struct Row {
    key: String,
    version: u32,
    value: String,
}

const SQL_SELECT_SETTINGS: &str = "
SELECT key, version, value FROM settings ORDER BY key";

const SQL_UPSERT_SETTING: &str = "
INSERT INTO settings (key, version, value, updated_at)
VALUES (?1, ?2, ?3, ?4)
ON CONFLICT (key) DO UPDATE SET
    version = excluded.version,
    value = excluded.value,
    updated_at = excluded.updated_at";

fn load_rows(conn: &Connection) -> Result<Vec<Row>> {
    let rows = conn
        .prepare_cached(SQL_SELECT_SETTINGS)?
        .query_map([], |r| {
            Ok(Row {
                key: r.get(0)?,
                version: u32::try_from(r.get::<_, i64>(1)?).unwrap_or(u32::MAX),
                value: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Builds effective settings from stored rows, falling back to the default
/// for every key that is missing, undecodable or invalid.
fn resolve(rows: &[Row]) -> Settings {
    let defaults = default_leaves();
    let mut current = defaults.clone();
    for row in rows {
        if !defaults.contains_key(&row.key) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&row.value) else {
            continue;
        };
        let previous = current.insert(row.key.clone(), v);
        if from_leaves(&current).is_err()
            && let Some(p) = previous
        {
            current.insert(row.key.clone(), p);
        }
    }
    // Revert flagged keys first. A cross-field rule can flag a key that is
    // already at its default (thin_after_days = 30 vs retention_days = 14);
    // then the whole section reverts. Both steps strictly move towards the
    // (valid) defaults, so this terminates.
    for _ in 0..=defaults.len() {
        let Ok(settings) = from_leaves(&current) else {
            break;
        };
        let issues = settings.validate();
        if issues.is_empty() {
            return settings;
        }
        let mut progressed = false;
        for i in &issues {
            if let Some(d) = defaults.get(&i.key)
                && current.get(&i.key) != Some(d)
            {
                current.insert(i.key.clone(), d.clone());
                progressed = true;
            }
        }
        if !progressed {
            for i in &issues {
                let section = i.key.split('.').next().unwrap_or_default();
                for (k, d) in &defaults {
                    if k.split('.').next() == Some(section) {
                        current.insert(k.clone(), d.clone());
                    }
                }
            }
        }
    }
    Settings::default()
}

// -----------------------------------------------------------------------------
// Export format
// -----------------------------------------------------------------------------

/// `format` tag of a settings export.
const EXPORT_FORMAT: &str = "strata-settings";
/// Envelope version of a settings export.
const EXPORT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct ExportFile {
    format: String,
    format_version: u32,
    #[serde(default, skip_deserializing)]
    exported_at: String,
    settings: BTreeMap<String, ExportEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ExportEntry {
    version: u32,
    value: Value,
}

// -----------------------------------------------------------------------------
// Store API
// -----------------------------------------------------------------------------

impl Store {
    /// Loads settings. Never fails because of bad stored values; see the
    /// module docs for the fallback rules.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn load_settings(&self) -> Result<Settings> {
        self.state().read(|c| Ok(resolve(&load_rows(c)?)))
    }

    /// Validates and saves settings. Unknown keys already stored are kept.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidSettings`] when validation fails (nothing is
    /// written), or database errors.
    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        let issues = settings.validate();
        if !issues.is_empty() {
            return Err(StoreError::InvalidSettings(issues));
        }
        let new = to_leaves(settings)?;
        let now = self.now();
        self.state().write(|tx| {
            let rows = load_rows(tx)?;
            let loaded = to_leaves(&resolve(&rows))?;
            let stored: HashMap<&str, &Row> = rows.iter().map(|r| (r.key.as_str(), r)).collect();
            let mut upsert = tx.prepare_cached(SQL_UPSERT_SETTING)?;
            for (key, value) in &new {
                let version = key_version(key);
                let json = serde_json::to_string(value)?;
                if let Some(row) = stored.get(key.as_str()) {
                    let newer_untouched = row.version > version && loaded.get(key) == Some(value);
                    let unchanged = row.version == version && row.value == json;
                    if newer_untouched || unchanged {
                        continue;
                    }
                }
                upsert.execute(params![key, i64::from(version), json, now.0])?;
            }
            Ok(())
        })
    }

    /// Exports the effective settings, plus any unknown keys preserved from
    /// newer versions, as pretty JSON.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn export_settings(&self) -> Result<String> {
        let now = self.now();
        let rows = self.state().read(load_rows)?;
        let effective = to_leaves(&resolve(&rows))?;
        let mut settings = BTreeMap::new();
        for row in &rows {
            if effective.contains_key(&row.key) {
                continue;
            }
            if let Ok(value) = serde_json::from_str(&row.value) {
                settings.insert(
                    row.key.clone(),
                    ExportEntry {
                        version: row.version,
                        value,
                    },
                );
            }
        }
        for (key, value) in effective {
            let version = key_version(&key);
            settings.insert(key, ExportEntry { version, value });
        }
        let file = ExportFile {
            format: EXPORT_FORMAT.into(),
            format_version: EXPORT_VERSION,
            exported_at: now.to_string(),
            settings,
        };
        Ok(serde_json::to_string_pretty(&file)?)
    }

    /// Validates an export produced by [`export_settings`](Self::export_settings)
    /// and replaces the current settings with it. Known keys missing from the
    /// file are reset to defaults; unknown keys are stored as-is.
    ///
    /// # Example
    ///
    /// ```
    /// # use strata_store::Store;
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let store = Store::open(dir.path()).unwrap();
    /// let json = store.export_settings().unwrap();
    /// let settings = store.import_settings(&json).unwrap();
    /// assert_eq!(settings, store.load_settings().unwrap());
    /// assert!(store.import_settings("{\"format\":\"other\"}").is_err());
    /// ```
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidInput`] for a malformed or foreign file,
    /// [`StoreError::InvalidSettings`] when any value fails validation.
    /// Nothing is written on error.
    pub fn import_settings(&self, json: &str) -> Result<Settings> {
        let file: ExportFile = serde_json::from_str(json)
            .map_err(|e| StoreError::InvalidInput(format!("not a settings export: {e}")))?;
        if file.format != EXPORT_FORMAT {
            return Err(StoreError::InvalidInput(format!(
                "unexpected format {:?}",
                file.format
            )));
        }
        if file.format_version > EXPORT_VERSION {
            return Err(StoreError::InvalidInput(format!(
                "export format version {} is newer than supported {EXPORT_VERSION}",
                file.format_version
            )));
        }

        let defaults = default_leaves();
        let mut leaves = defaults.clone();
        let mut unknown = Vec::new();
        let mut issues = Vec::new();
        for (key, entry) in file.settings {
            if !is_valid_key(&key) {
                issues.push(issue(&key, "malformed key"));
                continue;
            }
            let json = serde_json::to_string(&entry.value)?;
            if json.len() > MAX_VALUE_BYTES {
                issues.push(issue(&key, "value too large"));
                continue;
            }
            if defaults.contains_key(&key) {
                let previous = leaves.insert(key.clone(), entry.value);
                if from_leaves(&leaves).is_err() {
                    issues.push(issue(&key, "value has the wrong type"));
                    if let Some(p) = previous {
                        leaves.insert(key, p);
                    }
                }
            } else {
                unknown.push((key, entry.version, json));
            }
        }
        let settings = from_leaves(&leaves)?;
        issues.extend(settings.validate());
        if !issues.is_empty() {
            return Err(StoreError::InvalidSettings(issues));
        }

        let now = self.now();
        self.state().write(|tx| {
            let mut upsert = tx.prepare_cached(SQL_UPSERT_SETTING)?;
            for (key, value) in &leaves {
                let json = serde_json::to_string(value)?;
                upsert.execute(params![key, i64::from(key_version(key)), json, now.0])?;
            }
            for (key, version, json) in &unknown {
                upsert.execute(params![key, i64::from(*version), json, now.0])?;
            }
            Ok(())
        })?;
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, version: u32, value: &str) -> Row {
        Row {
            key: key.into(),
            version,
            value: value.into(),
        }
    }

    #[test]
    fn defaults_are_valid_and_match_spec() {
        let s = Settings::default();
        assert!(s.validate().is_empty());
        assert!(!s.activity.enabled);
        assert_eq!(s.activity.retention_days, 30);
        assert!(!s.privacy.crash_reports_opt_in);
        assert!(!s.startup.launch_at_login);
        assert!(s.live.usn_enabled);
        assert_eq!(s.cleanup.default_method, CleanupMethod::RecycleBin);
        assert_eq!(s.history.retention_policy(), RetentionPolicy::default());
        assert_eq!(s.cleanup.duplicates_min_bytes, MIB);
    }

    #[test]
    fn every_leaf_key_is_well_formed() {
        let leaves = default_leaves();
        assert!(leaves.len() > 30);
        for k in leaves.keys() {
            assert!(is_valid_key(k), "{k}");
        }
    }

    #[test]
    fn leaves_round_trip() {
        let mut s = Settings::default();
        s.scan.exclude_globs = vec!["**/.git".into()];
        s.rules.user_rules_dir = Some(r"D:\rules".into());
        assert_eq!(from_leaves(&to_leaves(&s).unwrap()).unwrap(), s);
    }

    #[test]
    fn bad_values_fall_back_per_key() {
        let s = resolve(&[
            row("live.update_tick_ms", 1, "\"fast\""),
            row("live.usn_enabled", 1, "false"),
            row("scan.walker_concurrency", 1, "100000"),
            row("appearance.theme", 1, "not json"),
            row("appearance.units", 1, "\"decimal\""),
        ]);
        assert_eq!(s.live.update_tick_ms, 1000);
        assert!(!s.live.usn_enabled);
        assert_eq!(s.scan.walker_concurrency, 0);
        assert_eq!(s.appearance.theme, Theme::System);
        assert_eq!(s.appearance.units, SizeUnits::Decimal);
    }

    #[test]
    fn cross_field_violation_converges() {
        let s = resolve(&[
            row("history.retention_days", 1, "14"),
            row("history.thin_after_days", 1, "60"),
        ]);
        // thin_after_days falls back to 30, which still exceeds 14, so the
        // section reverts as a whole.
        assert!(s.validate().is_empty());
        assert_eq!(s.history, HistorySettings::default());

        let s = resolve(&[
            row("history.retention_days", 1, "60"),
            row("history.thin_after_days", 1, "61"),
        ]);
        assert_eq!(s.history.retention_days, 60);
        assert_eq!(s.history.thin_after_days, 30);
    }

    #[test]
    fn newer_version_value_is_used_when_it_decodes() {
        let s = resolve(&[row("tray.enabled", 7, "true")]);
        assert!(s.tray.enabled);
    }
}
