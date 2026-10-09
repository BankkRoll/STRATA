//! Volume hot-plug notifications.
//!
//! [`VolumeWatcher`] owns a thread with a message-only window. On every
//! device change it re-enumerates volumes and diffs against the previous
//! snapshot, so it reports drive-letter volumes, folder mounts, letterless
//! volumes and BitLocker unlocks alike. Events arrive on a channel.
//!
//! Message-only windows do not receive broadcast messages, and volume
//! `DBT_DEVICEARRIVAL` with `DBT_DEVTYP_VOLUME` is a broadcast to top-level
//! windows. The watcher therefore also registers for
//! `GUID_DEVINTERFACE_VOLUME` device-interface notifications, which are
//! delivered to message-only windows. A volume's mount (drive letter) can lag
//! its interface arrival, so each change also schedules a delayed rescan.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Ioctl::GUID_DEVINTERFACE_VOLUME;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE, DBT_DEVTYP_DEVICEINTERFACE,
    DBT_DEVTYP_VOLUME, DEV_BROADCAST_DEVICEINTERFACE_W, DEV_BROADCAST_HDR, DEV_BROADCAST_VOLUME,
    DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    HDEVNOTIFY, HWND_MESSAGE, KillTimer, MSG, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterDeviceNotificationW, SetTimer, UnregisterDeviceNotification, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WM_DEVICECHANGE, WM_TIMER, WNDCLASSEXW,
};
use windows::core::w;

use crate::error::{Context, Result, WinError};
use crate::volume::{DiscoveryOptions, VolumeInfo, discover_volumes};

/// Delay before the follow-up rescan after a device change.
const SETTLE_DELAY: Duration = Duration::from_millis(1500);
const TIMER_ID: usize = 1;
/// Posted by [`VolumeWatcher::rescan`].
const WM_RESCAN: u32 = WM_APP + 1;

/// A change in the set of volumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum VolumeEvent {
    /// A volume appeared (inserted, mounted, or newly enumerable).
    Arrived {
        /// Its current information.
        volume: Box<VolumeInfo>,
    },
    /// A volume disappeared (yanked, ejected, dismounted). The index for it
    /// becomes stale.
    Removed {
        /// The last information seen before removal.
        volume: Box<VolumeInfo>,
    },
    /// A volume's mount paths, readiness, lock state, filesystem or label
    /// changed (e.g. BitLocker unlocked, letter reassigned, media inserted).
    Changed {
        /// Information after the change.
        volume: Box<VolumeInfo>,
    },
    /// Raw `DBT_DEVTYP_VOLUME` notification with the affected drive letters,
    /// when Windows delivers one. Always followed by diff events.
    DriveLetters {
        /// `true` for arrival, `false` for removal.
        arrived: bool,
        /// Letters from the unit mask.
        letters: Vec<char>,
    },
}

/// Options for [`VolumeWatcher::start`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatcherOptions {
    /// Enumeration options used for every rescan.
    pub discovery: DiscoveryOptions,
}

impl Default for WatcherOptions {
    fn default() -> Self {
        Self {
            discovery: DiscoveryOptions::local(),
        }
    }
}

/// Watches for volume arrival and removal on a dedicated thread.
///
/// # Example
///
/// ```no_run
/// use strata_win::watcher::{VolumeWatcher, WatcherOptions};
/// let w = VolumeWatcher::start(WatcherOptions::default())?;
/// while let Ok(ev) = w.events().recv() {
///     println!("{ev:?}");
/// }
/// w.stop()?;
/// # Ok::<(), strata_win::WinError>(())
/// ```
#[derive(Debug)]
pub struct VolumeWatcher {
    hwnd: isize,
    thread: Option<JoinHandle<()>>,
    events: Receiver<VolumeEvent>,
    initial: Vec<VolumeInfo>,
}

struct ThreadState {
    tx: Sender<VolumeEvent>,
    opts: DiscoveryOptions,
    snapshot: BTreeMap<String, VolumeInfo>,
}

thread_local! {
    // NOTE: each watcher owns its thread, so a thread-local holds that
    // watcher's state without passing raw pointers through the window.
    static STATE: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

impl VolumeWatcher {
    /// Starts the watcher thread. Returns once the window exists and the
    /// initial snapshot has been taken (see [`VolumeWatcher::initial`]).
    pub fn start(opts: WatcherOptions) -> Result<Self> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) =
            crossbeam_channel::bounded::<Result<(isize, Vec<VolumeInfo>)>>(1);
        let thread = std::thread::Builder::new()
            .name("strata-volume-watcher".into())
            .spawn(move || run(tx, opts.discovery, &ready_tx))
            .map_err(|_| WinError::from_win32("spawn watcher thread", 8))?;
        match ready_rx.recv() {
            Ok(Ok((hwnd, initial))) => Ok(Self {
                hwnd,
                thread: Some(thread),
                events: rx,
                initial,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(WinError::from_win32("watcher thread exited", 1067))
            }
        }
    }

    /// Volume events. Disconnected after [`VolumeWatcher::stop`].
    #[must_use]
    pub fn events(&self) -> &Receiver<VolumeEvent> {
        &self.events
    }

    /// The volumes seen when the watcher started.
    #[must_use]
    pub fn initial(&self) -> &[VolumeInfo] {
        &self.initial
    }

    /// Requests an immediate re-enumeration (e.g. after the user unlocked a
    /// BitLocker volume from the app).
    pub fn rescan(&self) -> Result<()> {
        // SAFETY: PostMessageW is thread-safe; the window lives until stop.
        unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut c_void)),
                WM_RESCAN,
                WPARAM(0),
                LPARAM(0),
            )
        }
        .ctx("PostMessageW")
    }

    /// Closes the window and joins the thread.
    pub fn stop(mut self) -> Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        // SAFETY: posting to a window owned by our thread; if it already
        // died the post fails harmlessly and join returns immediately.
        let posted = unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut c_void)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            )
        };
        let joined = thread.join();
        posted.ctx("PostMessageW(WM_CLOSE)")?;
        joined.map_err(|_| WinError::from_win32("watcher thread panicked", 1067))
    }
}

impl Drop for VolumeWatcher {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

type Ready = Result<(isize, Vec<VolumeInfo>)>;

fn run(tx: Sender<VolumeEvent>, opts: DiscoveryOptions, ready: &Sender<Ready>) {
    let setup = || -> Result<(HWND, HDEVNOTIFY)> {
        // SAFETY: querying our own module handle.
        let hinstance = unsafe { GetModuleHandleW(None) }.ctx("GetModuleHandleW")?;
        let class = w!("StrataVolumeWatcher");
        if !CLASS_REGISTERED.swap(true, Ordering::SeqCst) {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            // SAFETY: `wc` is fully initialized; the class name is static.
            if unsafe { RegisterClassExW(&wc) } == 0 {
                CLASS_REGISTERED.store(false, Ordering::SeqCst);
                return Err(WinError::last("RegisterClassExW"));
            }
        }
        // SAFETY: the class is registered; HWND_MESSAGE makes it message-only.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("Strata volume watcher"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .ctx("CreateWindowExW")?;
        let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
            dbcc_size: std::mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
            dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE.0,
            dbcc_classguid: GUID_DEVINTERFACE_VOLUME,
            ..Default::default()
        };
        // SAFETY: `filter` is a valid DEV_BROADCAST_DEVICEINTERFACE_W and
        // `hwnd` is our window.
        let notify = unsafe {
            RegisterDeviceNotificationW(
                HANDLE(hwnd.0),
                (&raw const filter).cast(),
                DEVICE_NOTIFY_WINDOW_HANDLE,
            )
        };
        match notify {
            Ok(n) => Ok((hwnd, n)),
            Err(e) => {
                // SAFETY: destroying the window we just created.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                Err(WinError::new("RegisterDeviceNotificationW", &e))
            }
        }
    };
    let (hwnd, notify) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let initial = discover_volumes(opts).unwrap_or_default();
    STATE.with(|s| {
        *s.borrow_mut() = Some(ThreadState {
            tx,
            opts,
            snapshot: initial.iter().map(|v| (v.id(), v.clone())).collect(),
        });
    });
    let _ = ready.send(Ok((hwnd.0 as isize, initial)));

    let mut msg = MSG::default();
    // SAFETY: standard message loop on the thread that owns `hwnd`.
    // GetMessageW returns 0 on WM_QUIT and -1 on error; both end the loop.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        // SAFETY: `msg` was filled by GetMessageW.
        unsafe { DispatchMessageW(&msg) };
    }
    // SAFETY: the registration belongs to this thread's window.
    unsafe {
        let _ = UnregisterDeviceNotification(notify);
    }
    STATE.with(|s| s.borrow_mut().take());
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DEVICECHANGE => {
            let event = wparam.0 as u32;
            if event == DBT_DEVICEARRIVAL || event == DBT_DEVICEREMOVECOMPLETE {
                // SAFETY: for these events lParam points to a
                // DEV_BROADCAST_HDR (or is null) valid during the call.
                if let Some(letters) = unsafe { volume_letters(lparam) } {
                    send(VolumeEvent::DriveLetters {
                        arrived: event == DBT_DEVICEARRIVAL,
                        letters,
                    });
                }
                rescan();
                // SAFETY: our own window; replaces any pending settle timer.
                unsafe { SetTimer(Some(hwnd), TIMER_ID, SETTLE_DELAY.as_millis() as u32, None) };
            }
            LRESULT(1)
        }
        WM_TIMER if wparam.0 == TIMER_ID => {
            // SAFETY: our own window and timer.
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_ID);
            }
            rescan();
            LRESULT(0)
        }
        WM_RESCAN => {
            rescan();
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: destroying our own window from its thread.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default processing with the arguments we received.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Drive letters of a `DBT_DEVTYP_VOLUME` broadcast, if `lparam` is one.
///
/// # Safety
///
/// `lparam` must be null or point to a valid `DEV_BROADCAST_HDR`.
unsafe fn volume_letters(lparam: LPARAM) -> Option<Vec<char>> {
    let hdr = lparam.0 as *const DEV_BROADCAST_HDR;
    if hdr.is_null() {
        return None;
    }
    // SAFETY: caller contract.
    let hdr_ref = unsafe { &*hdr };
    if hdr_ref.dbch_devicetype != DBT_DEVTYP_VOLUME
        || (hdr_ref.dbch_size as usize) < std::mem::size_of::<DEV_BROADCAST_VOLUME>()
    {
        return None;
    }
    // SAFETY: the header says this is a DEV_BROADCAST_VOLUME of sufficient size.
    let vol = unsafe { &*hdr.cast::<DEV_BROADCAST_VOLUME>() };
    Some(letters_from_unit_mask(vol.dbcv_unitmask))
}

/// Decodes a `dbcv_unitmask` (bit 0 = `A:`).
#[must_use]
pub fn letters_from_unit_mask(mask: u32) -> Vec<char> {
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| char::from(b'A' + i))
        .collect()
}

fn send(ev: VolumeEvent) {
    STATE.with(|s| {
        if let Ok(guard) = s.try_borrow()
            && let Some(st) = guard.as_ref()
        {
            let _ = st.tx.send(ev);
        }
    });
}

// IMPORTANT: no RefCell borrow may be held across `discover_volumes`. The
// BitLocker query makes STA COM calls, which can pump messages and re-enter
// the window procedure; a held borrow would then panic inside an
// `extern "system"` callback and abort the process.
fn rescan() {
    let Some(opts) = STATE.with(|s| s.try_borrow().ok()?.as_ref().map(|st| st.opts)) else {
        return;
    };
    let Ok(now) = discover_volumes(opts) else {
        return;
    };
    let now: BTreeMap<String, VolumeInfo> = now.into_iter().map(|v| (v.id(), v)).collect();
    STATE.with(|s| {
        let Ok(mut guard) = s.try_borrow_mut() else {
            return;
        };
        let Some(st) = guard.as_mut() else {
            return;
        };
        for ev in diff_volumes(&st.snapshot, &now) {
            let _ = st.tx.send(ev);
        }
        st.snapshot = now;
    });
}

/// Fields whose change is worth an event. Free space is deliberately
/// excluded: it changes constantly.
fn identity(v: &VolumeInfo) -> impl PartialEq + '_ {
    (
        &v.mount_paths,
        v.ready,
        v.bitlocker,
        &v.fs_name,
        &v.label,
        v.kind,
        v.total_bytes,
    )
}

/// Diffs two snapshots keyed by [`VolumeInfo::id`].
#[must_use]
pub fn diff_volumes(
    before: &BTreeMap<String, VolumeInfo>,
    after: &BTreeMap<String, VolumeInfo>,
) -> Vec<VolumeEvent> {
    let mut out = Vec::new();
    for (id, old) in before {
        match after.get(id) {
            None => out.push(VolumeEvent::Removed {
                volume: Box::new(old.clone()),
            }),
            Some(new) if identity(old) != identity(new) => out.push(VolumeEvent::Changed {
                volume: Box::new(new.clone()),
            }),
            Some(_) => {}
        }
    }
    for (id, new) in after {
        if !before.contains_key(id) {
            out.push(VolumeEvent::Arrived {
                volume: Box::new(new.clone()),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::{BitLockerState, DevDriveState, DriveKind, FileSystemKind};

    fn vol(id: &str, letter: Option<&str>) -> VolumeInfo {
        VolumeInfo {
            guid_path: Some(id.into()),
            device_path: None,
            mount_paths: letter.map(Into::into).into_iter().collect(),
            drive_letter: None,
            nested_in: vec![],
            kind: DriveKind::Removable,
            remote_path: None,
            fs_name: Some("exFAT".into()),
            filesystem: FileSystemKind::ExFat,
            label: None,
            serial: None,
            fs_flags: 0,
            max_component_len: None,
            supports_usn_journal: false,
            case_sensitive_search: false,
            case_preserved_names: true,
            read_only: false,
            compressed: false,
            cluster_size: None,
            sector_size: None,
            total_bytes: Some(10),
            free_bytes: Some(5),
            available_bytes: Some(5),
            is_system: false,
            bitlocker: BitLockerState::NotEncrypted,
            dev_drive: DevDriveState::NotDevDrive,
            ready: true,
            error: None,
        }
    }

    fn map(vs: &[VolumeInfo]) -> BTreeMap<String, VolumeInfo> {
        vs.iter().map(|v| (v.id(), v.clone())).collect()
    }

    #[test]
    fn diff_reports_arrival_removal_and_changes() {
        let a = vol("a", Some(r"E:\"));
        let b = vol("b", None);
        let mut b2 = b.clone();
        b2.mount_paths = vec![r"C:\mnt\b\".into()];
        let mut a_free = a.clone();
        a_free.free_bytes = Some(1);
        let c = vol("c", Some(r"F:\"));

        assert!(diff_volumes(&map(std::slice::from_ref(&a)), &map(&[a_free])).is_empty());
        let evs = diff_volumes(&map(&[a.clone(), b]), &map(&[b2.clone(), c.clone()]));
        assert_eq!(
            evs,
            vec![
                VolumeEvent::Removed {
                    volume: Box::new(a)
                },
                VolumeEvent::Changed {
                    volume: Box::new(b2)
                },
                VolumeEvent::Arrived {
                    volume: Box::new(c)
                },
            ]
        );
        let mut locked = vol("d", Some(r"G:\"));
        locked.bitlocker = BitLockerState::Locked;
        let unlocked = vol("d", Some(r"G:\"));
        assert!(matches!(
            diff_volumes(&map(&[locked]), &map(&[unlocked]))[..],
            [VolumeEvent::Changed { .. }]
        ));
    }

    #[test]
    fn unit_mask_letters() {
        assert_eq!(letters_from_unit_mask(0b101), vec!['A', 'C']);
        assert_eq!(letters_from_unit_mask(1 << 25), vec!['Z']);
        assert!(letters_from_unit_mask(0).is_empty());
        assert_eq!(letters_from_unit_mask(u32::MAX).len(), 26);
    }

    #[test]
    fn start_rescan_stop() {
        let w = VolumeWatcher::start(WatcherOptions::default()).unwrap();
        assert!(w.initial().iter().any(|v| v.is_system));
        w.rescan().unwrap();
        // A stable machine produces no events from a forced rescan.
        let ev = w.events().recv_timeout(Duration::from_millis(500));
        assert!(ev.is_err(), "unexpected event {ev:?}");
        let rx = w.events().clone();
        w.stop().unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn two_watchers_and_drop() {
        let a = VolumeWatcher::start(WatcherOptions::default()).unwrap();
        let b = VolumeWatcher::start(WatcherOptions::default()).unwrap();
        let rx = b.events().clone();
        drop(b);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        ));
        a.stop().unwrap();
    }
}
