//! Notification-area icon and low-space toasts (SPEC §19 "Tray icon").
//!
//! - The tray icon is optional (`tray.enabled`). Its menu shows a free-space
//!   glance per fixed volume, "Quick scan", "Open Strata" and "Quit".
//! - While the tray is on, closing the main window hides it to the tray.
//! - A monitor thread polls free space of fixed volumes once a minute
//!   (`GetDiskFreeSpaceExW` through the volume layer; no disk I/O beyond
//!   that) and raises a Windows toast when a volume drops below
//!   `tray.low_space_threshold_bytes`, once per crossing.
//!
//! "Quick scan" emits [`QUICK_SCAN`] with the system volume id; the scan
//! itself belongs to the scan commands.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::Serialize;
use strata_store::{Settings, SizeUnits};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Runtime, WindowEvent};
use tauri_plugin_notification::NotificationExt;

/// Event emitted by the tray's "Quick scan" with payload [`QuickScan`].
pub const QUICK_SCAN: &str = "tray://quick-scan";

/// Poll interval for free space.
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);

const TRAY_ID: &str = "strata-tray";
const MENU_QUICK_SCAN: &str = "tray.quick_scan";
const MENU_OPEN: &str = "tray.open";
const MENU_QUIT: &str = "tray.quit";

/// Payload of [`QUICK_SCAN`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickScan {
    /// Volume id (GUID path) of the system volume, when known.
    pub volume_id: Option<String>,
}

// -----------------------------------------------------------------------------
// Model (pure, unit-tested)
// -----------------------------------------------------------------------------

/// Free space of one volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeGlance {
    /// Volume id (GUID path).
    pub id: String,
    /// Short name (`C:` or the label).
    pub name: String,
    /// Free bytes.
    pub free: u64,
    /// Capacity.
    pub total: u64,
    /// Holds Windows.
    pub is_system: bool,
}

/// One tray menu row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayEntry {
    /// Disabled informational row.
    Info(String),
    /// Clickable row.
    Action {
        /// Menu id.
        id: &'static str,
        /// Label.
        label: &'static str,
    },
    /// Separator.
    Separator,
}

/// Formats bytes in the user's unit system with one decimal.
#[must_use]
pub fn format_bytes(bytes: u64, units: SizeUnits) -> String {
    let (base, names): (f64, [&str; 5]) = match units {
        SizeUnits::Binary => (1024.0, ["B", "KiB", "MiB", "GiB", "TiB"]),
        SizeUnits::Decimal => (1000.0, ["B", "kB", "MB", "GB", "TB"]),
    };
    #[allow(clippy::cast_precision_loss)]
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= base && i < names.len() - 1 {
        v /= base;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", names[i])
    }
}

/// The menu for `volumes`, system volume first.
#[must_use]
pub fn menu_model(volumes: &[VolumeGlance], units: SizeUnits) -> Vec<TrayEntry> {
    let mut sorted: Vec<&VolumeGlance> = volumes.iter().collect();
    sorted.sort_by(|a, b| b.is_system.cmp(&a.is_system).then(a.name.cmp(&b.name)));
    let mut out: Vec<TrayEntry> = sorted
        .iter()
        .map(|v| {
            TrayEntry::Info(format!(
                "{}  {} free of {}",
                v.name,
                format_bytes(v.free, units),
                format_bytes(v.total, units)
            ))
        })
        .collect();
    if out.is_empty() {
        out.push(TrayEntry::Info("No drives found".into()));
    }
    out.extend([
        TrayEntry::Separator,
        TrayEntry::Action {
            id: MENU_QUICK_SCAN,
            label: "Quick scan",
        },
        TrayEntry::Action {
            id: MENU_OPEN,
            label: "Open Strata",
        },
        TrayEntry::Separator,
        TrayEntry::Action {
            id: MENU_QUIT,
            label: "Quit Strata",
        },
    ]);
    out
}

/// Hover text: the system volume's free space.
#[must_use]
pub fn tooltip(volumes: &[VolumeGlance], units: SizeUnits) -> String {
    volumes
        .iter()
        .find(|v| v.is_system)
        .or_else(|| volumes.first())
        .map_or_else(
            || "Strata".into(),
            |v| format!("Strata: {} {} free", v.name, format_bytes(v.free, units)),
        )
}

/// A low-space toast to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LowSpaceAlert {
    /// Volume id.
    pub id: String,
    /// Title.
    pub title: String,
    /// Body.
    pub body: String,
}

/// Edge-triggered low-space detection with hysteresis, so a volume hovering
/// at the threshold does not toast every minute.
#[derive(Debug, Default)]
pub struct LowSpaceMonitor {
    /// Volume id → currently below the threshold (already alerted).
    below: HashMap<String, bool>,
}

impl LowSpaceMonitor {
    /// Hysteresis above the threshold before a volume re-arms.
    #[must_use]
    pub fn rearm_margin(threshold: u64) -> u64 {
        (threshold / 20).max(256 * 1024 * 1024)
    }

    /// Forgets every state, so the next observation alerts again (the
    /// threshold changed).
    pub fn reset(&mut self) {
        self.below.clear();
    }

    /// Feeds one poll; returns the toasts to show.
    pub fn observe(
        &mut self,
        volumes: &[VolumeGlance],
        threshold: u64,
        units: SizeUnits,
    ) -> Vec<LowSpaceAlert> {
        let mut alerts = Vec::new();
        for v in volumes {
            let was_below = self.below.get(&v.id).copied().unwrap_or(false);
            if v.free < threshold {
                if !was_below {
                    alerts.push(LowSpaceAlert {
                        id: v.id.clone(),
                        title: format!("{} is low on space", v.name),
                        body: format!(
                            "Only {} free (below {}). Open Strata to see what is using it.",
                            format_bytes(v.free, units),
                            format_bytes(threshold, units)
                        ),
                    });
                }
                self.below.insert(v.id.clone(), true);
            } else if v.free >= threshold.saturating_add(Self::rearm_margin(threshold)) {
                self.below.insert(v.id.clone(), false);
            }
        }
        alerts
    }
}

// -----------------------------------------------------------------------------
// Runtime
// -----------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Wake {
    pending: bool,
    reset: bool,
}

/// Managed state for the tray and the monitor.
#[derive(Debug, Default)]
pub struct TrayState {
    wake: Arc<(Mutex<Wake>, Condvar)>,
    last: Mutex<Vec<VolumeGlance>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn settings<R: Runtime>(app: &AppHandle<R>) -> Settings {
    super::store::handle(app)
        .and_then(|s| Ok(s.load_settings()?))
        .unwrap_or_default()
}

/// Shows, unminimizes and focuses the main window.
pub fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Current free space of fixed, ready volumes.
#[must_use]
pub fn poll_volumes() -> Vec<VolumeGlance> {
    let opts = strata_win::volume::DiscoveryOptions {
        include_network: false,
        query_bitlocker: false,
    };
    strata_win::volume::discover_volumes(opts)
        .unwrap_or_default()
        .into_iter()
        .filter(|v| v.ready && v.kind == strata_win::volume::DriveKind::Fixed)
        .filter_map(|v| {
            let name = v
                .drive_letter
                .map(|d| format!("{d}:"))
                .or_else(|| v.label.clone())
                .unwrap_or_else(|| v.id());
            Some(VolumeGlance {
                id: v.id(),
                name,
                free: v.free_bytes?,
                total: v.total_bytes?,
                is_system: v.is_system,
            })
        })
        .collect()
}

fn build_menu<R: Runtime>(app: &AppHandle<R>, model: &[TrayEntry]) -> tauri::Result<Menu<R>> {
    let menu = Menu::new(app)?;
    for e in model {
        match e {
            TrayEntry::Info(text) => {
                menu.append(&MenuItem::new(app, text, false, None::<&str>)?)?;
            }
            TrayEntry::Action { id, label } => {
                menu.append(&MenuItem::with_id(app, *id, *label, true, None::<&str>)?)?;
            }
            TrayEntry::Separator => menu.append(&PredefinedMenuItem::separator(app)?)?,
        }
    }
    Ok(menu)
}

fn on_menu<R: Runtime>(app: &AppHandle<R>, id: &str) {
    match id {
        MENU_OPEN => show_main_window(app),
        MENU_QUICK_SCAN => {
            show_main_window(app);
            let volume_id = app.try_state::<TrayState>().and_then(|s| {
                lock(&s.last)
                    .iter()
                    .find(|v| v.is_system)
                    .map(|v| v.id.clone())
            });
            let _ = app.emit(QUICK_SCAN, QuickScan { volume_id });
        }
        MENU_QUIT => app.exit(0),
        _ => {}
    }
}

fn create_icon<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<TrayIcon<R>> {
    let units = settings(app).appearance.units;
    let volumes = poll_volumes();
    let menu = build_menu(app, &menu_model(&volumes, units))?;
    let mut b = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(tooltip(&volumes, units))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, e| on_menu(app, e.id().as_ref()))
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        b = b.icon(icon.clone());
    }
    b.build(app)
}

/// Shows or removes the tray icon.
pub fn set_visible<R: Runtime>(app: &AppHandle<R>, on: bool) {
    if on {
        if app.tray_by_id(TRAY_ID).is_none() {
            let _ = create_icon(app);
        }
    } else if let Some(t) = app.remove_tray_by_id(TRAY_ID) {
        let _ = t.set_visible(false);
    }
}

fn refresh<R: Runtime>(app: &AppHandle<R>, volumes: &[VolumeGlance], units: SizeUnits) {
    if let Some(t) = app.tray_by_id(TRAY_ID) {
        if let Ok(menu) = build_menu(app, &menu_model(volumes, units)) {
            let _ = t.set_menu(Some(menu));
        }
        let _ = t.set_tooltip(Some(tooltip(volumes, units)));
    }
}

/// Re-arms every low-space alert (threshold changed) and polls now.
pub fn reset_alerts<R: Runtime>(app: &AppHandle<R>) {
    if let Some(s) = app.try_state::<TrayState>() {
        let (m, cv) = &*s.wake;
        let mut w = lock(m);
        w.reset = true;
        w.pending = true;
        cv.notify_one();
    }
}

/// Polls now instead of waiting for the next interval.
pub fn wake<R: Runtime>(app: &AppHandle<R>) {
    if let Some(s) = app.try_state::<TrayState>() {
        let (m, cv) = &*s.wake;
        lock(m).pending = true;
        cv.notify_one();
    }
}

/// Creates the tray when enabled, hides-to-tray on close, and starts the
/// monitor thread.
pub fn setup<R: Runtime>(app: &AppHandle<R>, settings: &Settings) {
    app.manage(TrayState::default());
    if settings.tray.enabled {
        set_visible(app, true);
    }
    if let Some(w) = app.get_webview_window("main") {
        let handle = app.clone();
        w.on_window_event(move |e| {
            if let WindowEvent::CloseRequested { api, .. } = e
                && handle.tray_by_id(TRAY_ID).is_some()
            {
                api.prevent_close();
                if let Some(w) = handle.get_webview_window("main") {
                    let _ = w.hide();
                }
            }
        });
    }
    let handle = app.clone();
    let spawned = std::thread::Builder::new()
        .name("strata-space-monitor".into())
        .spawn(move || monitor(&handle));
    drop(spawned);
}

fn monitor<R: Runtime>(app: &AppHandle<R>) {
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };
    let wake = state.wake.clone();
    let mut detector = LowSpaceMonitor::default();
    loop {
        let s = settings(app);
        let tray_on = app.tray_by_id(TRAY_ID).is_some();
        if tray_on || s.tray.low_space_notification {
            let volumes = poll_volumes();
            if tray_on {
                refresh(app, &volumes, s.appearance.units);
            }
            let alerts = detector.observe(
                &volumes,
                s.tray.low_space_threshold_bytes,
                s.appearance.units,
            );
            if s.tray.low_space_notification {
                for a in alerts {
                    let _ = app
                        .notification()
                        .builder()
                        .title(a.title)
                        .body(a.body)
                        .show();
                }
            }
            *lock(&state.last) = volumes;
        }
        let (m, cv) = &*wake;
        let mut w = lock(m);
        if !w.pending {
            w = cv
                .wait_timeout(w, POLL_INTERVAL)
                .map_or_else(|e| e.into_inner().0, |r| r.0);
        }
        if w.reset {
            detector.reset();
        }
        *w = Wake::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn vol(id: &str, name: &str, free: u64, system: bool) -> VolumeGlance {
        VolumeGlance {
            id: id.into(),
            name: name.into(),
            free,
            total: 500 * GIB,
            is_system: system,
        }
    }

    #[test]
    fn menu_lists_volumes_then_actions() {
        let m = menu_model(
            &[
                vol("d", "D:", 200 * GIB, false),
                vol("c", "C:", 20 * GIB, true),
            ],
            SizeUnits::Binary,
        );
        assert_eq!(
            m[0],
            TrayEntry::Info("C:  20.0 GiB free of 500.0 GiB".into())
        );
        assert_eq!(
            m[1],
            TrayEntry::Info("D:  200.0 GiB free of 500.0 GiB".into())
        );
        let ids: Vec<&str> = m
            .iter()
            .filter_map(|e| match e {
                TrayEntry::Action { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, [MENU_QUICK_SCAN, MENU_OPEN, MENU_QUIT]);
        assert!(matches!(
            m.last(),
            Some(TrayEntry::Action { id: MENU_QUIT, .. })
        ));
        let empty = menu_model(&[], SizeUnits::Decimal);
        assert_eq!(empty[0], TrayEntry::Info("No drives found".into()));
    }

    #[test]
    fn tooltip_and_units() {
        let v = [vol("c", "C:", 1_500_000_000, true)];
        assert_eq!(tooltip(&v, SizeUnits::Decimal), "Strata: C: 1.5 GB free");
        assert_eq!(tooltip(&[], SizeUnits::Decimal), "Strata");
        assert_eq!(format_bytes(512, SizeUnits::Binary), "512 B");
        assert_eq!(format_bytes(1536, SizeUnits::Binary), "1.5 KiB");
    }

    #[test]
    fn low_space_alerts_once_per_crossing() {
        let mut m = LowSpaceMonitor::default();
        let t = 10 * GIB;
        let u = SizeUnits::Binary;
        assert!(
            m.observe(&[vol("c", "C:", 50 * GIB, true)], t, u)
                .is_empty()
        );
        let a = m.observe(&[vol("c", "C:", 9 * GIB, true)], t, u);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].title, "C: is low on space");
        assert!(a[0].body.contains("9.0 GiB"));
        assert!(m.observe(&[vol("c", "C:", 8 * GIB, true)], t, u).is_empty());
        // Barely above the threshold: still armed off (hysteresis).
        assert!(m.observe(&[vol("c", "C:", t + 1, true)], t, u).is_empty());
        assert!(m.observe(&[vol("c", "C:", 9 * GIB, true)], t, u).is_empty());
        // Well above re-arms; the next drop alerts again.
        assert!(
            m.observe(&[vol("c", "C:", 20 * GIB, true)], t, u)
                .is_empty()
        );
        assert_eq!(m.observe(&[vol("c", "C:", 9 * GIB, true)], t, u).len(), 1);
        m.reset();
        assert_eq!(m.observe(&[vol("c", "C:", 9 * GIB, true)], t, u).len(), 1);
    }
}
