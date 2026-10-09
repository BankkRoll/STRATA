//! Settings persistence: round trip, forward compatibility, export/import.

mod common;

use common::*;
use strata_core::SizeMode;
use strata_store::*;

fn raw(dir: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.join("state.db")).unwrap()
}

fn customized() -> Settings {
    let mut s = Settings::default();
    s.scan.default_size_mode = SizeMode::Logical;
    s.scan.exclude_globs = vec!["**/node_modules/.cache".into(), r"D:\VMs\**".into()];
    s.live.update_tick_ms = 250;
    s.activity.enabled = true;
    s.activity.cpu_cap_percent = 1.5;
    s.helper.mode = HelperMode::Service;
    s.cleanup.default_method = CleanupMethod::Permanent;
    s.history.retention_days = 180;
    s.appearance.color_mode = ColorMode::Age;
    s.rules.user_rules_dir = Some(r"D:\strata-rules".into());
    s.startup.launch_at_login = true;
    s.tray.enabled = true;
    s.updates.channel = UpdateChannel::Beta;
    s
}

#[test]
fn fresh_store_loads_defaults() {
    let (_d, store, _c) = store_at(t0());
    assert_eq!(store.load_settings().unwrap(), Settings::default());
}

#[test]
fn round_trip() {
    let (d, store, _c) = store_at(t0());
    let s = customized();
    store.save_settings(&s).unwrap();
    assert_eq!(store.load_settings().unwrap(), s);
    drop(store);
    let store = Store::open(d.path()).unwrap();
    assert_eq!(store.load_settings().unwrap(), s);
}

#[test]
fn invalid_settings_are_rejected_without_writing() {
    let (_d, store, _c) = store_at(t0());
    let mut s = Settings::default();
    s.live.update_tick_ms = 5;
    s.activity.cpu_cap_percent = f64::INFINITY;
    match store.save_settings(&s) {
        Err(StoreError::InvalidSettings(issues)) => {
            let keys: Vec<_> = issues.iter().map(|i| i.key.as_str()).collect();
            assert_eq!(
                keys,
                vec!["live.update_tick_ms", "activity.cpu_cap_percent"]
            );
        }
        other => panic!("expected InvalidSettings, got {other:?}"),
    }
    assert_eq!(store.load_settings().unwrap(), Settings::default());
}

#[test]
fn unknown_keys_survive_save() {
    let (d, store, _c) = store_at(t0());
    raw(d.path())
        .execute(
            "INSERT INTO settings (key, version, value, updated_at)
             VALUES ('future.shiny_feature', 3, '{\"level\":11}', 0)",
            [],
        )
        .unwrap();
    store.save_settings(&customized()).unwrap();
    let (version, value): (i64, String) = raw(d.path())
        .query_row(
            "SELECT version, value FROM settings WHERE key = 'future.shiny_feature'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((version, value.as_str()), (3, "{\"level\":11}"));
    assert_eq!(store.load_settings().unwrap(), customized());
}

#[test]
fn missing_keys_default_and_bad_rows_fall_back() {
    let (d, store, _c) = store_at(t0());
    store.save_settings(&customized()).unwrap();
    let conn = raw(d.path());
    conn.execute("DELETE FROM settings WHERE key = 'live.update_tick_ms'", [])
        .unwrap();
    conn.execute(
        "UPDATE settings SET value = '\"purple\"' WHERE key = 'appearance.color_mode'",
        [],
    )
    .unwrap();
    let s = store.load_settings().unwrap();
    assert_eq!(s.live.update_tick_ms, 1000);
    assert_eq!(s.appearance.color_mode, ColorMode::Category);
    assert_eq!(s.helper.mode, HelperMode::Service, "others unaffected");
}

#[test]
fn newer_versioned_row_is_not_clobbered_unless_changed() {
    let (d, store, _c) = store_at(t0());
    // A newer build stored tray.low_space_threshold_bytes in a format this
    // build cannot decode.
    raw(d.path())
        .execute(
            "INSERT INTO settings (key, version, value, updated_at)
             VALUES ('tray.low_space_threshold_bytes', 2, '{\"percent\":5}', 0)",
            [],
        )
        .unwrap();
    let mut s = store.load_settings().unwrap();
    assert_eq!(s.tray.low_space_threshold_bytes, 10 * GIB);
    s.appearance.compact_density = true;
    store.save_settings(&s).unwrap();
    let value: String = raw(d.path())
        .query_row(
            "SELECT value FROM settings WHERE key = 'tray.low_space_threshold_bytes'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(value, "{\"percent\":5}", "untouched value is preserved");

    s.tray.low_space_threshold_bytes = 20 * GIB;
    store.save_settings(&s).unwrap();
    let (version, value): (i64, String) = raw(d.path())
        .query_row(
            "SELECT version, value FROM settings WHERE key = 'tray.low_space_threshold_bytes'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((version, value), (1, (20 * GIB).to_string()));
}

#[test]
fn export_import_round_trip_across_stores() {
    let (d1, a, _c) = store_at(t0());
    a.save_settings(&customized()).unwrap();
    raw(d1.path())
        .execute(
            "INSERT INTO settings (key, version, value, updated_at)
             VALUES ('future.thing', 4, '[1,2,3]', 0)",
            [],
        )
        .unwrap();
    let json = a.export_settings().unwrap();
    assert!(json.contains("\"format\": \"strata-settings\""));
    assert!(json.contains("future.thing"));
    assert!(json.contains("2026-10-09T12:00:00Z"));

    let (d2, b, _c) = store_at(t0());
    let imported = b.import_settings(&json).unwrap();
    assert_eq!(imported, customized());
    assert_eq!(b.load_settings().unwrap(), customized());
    let v: i64 = raw(d2.path())
        .query_row(
            "SELECT version FROM settings WHERE key = 'future.thing'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, 4);
}

#[test]
fn import_validates_everything_first() {
    let (_d, store, _c) = store_at(t0());
    store.save_settings(&customized()).unwrap();

    let bad_value = r#"{"format":"strata-settings","format_version":1,"settings":{
        "live.update_tick_ms":{"version":1,"value":"soon"},
        "history.retention_days":{"version":1,"value":2}}}"#;
    match store.import_settings(bad_value) {
        Err(StoreError::InvalidSettings(issues)) => {
            let keys: Vec<_> = issues.iter().map(|i| i.key.as_str()).collect();
            assert!(keys.contains(&"live.update_tick_ms"));
            assert!(keys.contains(&"history.retention_days"));
        }
        other => panic!("expected InvalidSettings, got {other:?}"),
    }
    let bad_key = r#"{"format":"strata-settings","format_version":1,"settings":{
        "Robert'); DROP TABLE settings;--":{"version":1,"value":1}}}"#;
    assert!(matches!(
        store.import_settings(bad_key),
        Err(StoreError::InvalidSettings(_))
    ));
    for bad in [
        "",
        "[]",
        r#"{"format":"strata-settings","format_version":99,"settings":{}}"#,
        r#"{"format":"something-else","format_version":1,"settings":{}}"#,
    ] {
        assert!(
            matches!(store.import_settings(bad), Err(StoreError::InvalidInput(_))),
            "{bad}"
        );
    }
    assert_eq!(
        store.load_settings().unwrap(),
        customized(),
        "nothing written"
    );
}

#[test]
fn import_of_partial_file_resets_missing_keys() {
    let (_d, store, _c) = store_at(t0());
    store.save_settings(&customized()).unwrap();
    let partial = r#"{"format":"strata-settings","format_version":1,"settings":{
        "appearance.theme":{"version":1,"value":"dark"}}}"#;
    let s = store.import_settings(partial).unwrap();
    let mut expected = Settings::default();
    expected.appearance.theme = Theme::Dark;
    assert_eq!(s, expected);
    assert_eq!(store.load_settings().unwrap(), expected);
}

#[test]
fn settings_drive_history_policy() {
    let mut s = Settings::default();
    s.history.retention_days = 120;
    s.history.thin_after_days = 14;
    s.history.min_dir_bytes = 64 * MIB;
    assert_eq!(
        s.history.retention_policy(),
        RetentionPolicy {
            keep_days: 120,
            thin_after_days: 14
        }
    );
    assert_eq!(s.history.snapshot_options().min_dir_bytes, 64 * MIB);
}

#[test]
fn retired_keys_from_older_stores_are_ignored() {
    let (d, store, _c) = store_at(t0());
    raw(d.path())
        .execute(
            "INSERT INTO settings (key, version, value, updated_at)
             VALUES ('privacy.crash_reports_opt_in', 1, 'true', 0)",
            [],
        )
        .unwrap();
    assert_eq!(store.load_settings().unwrap(), Settings::default());
}
