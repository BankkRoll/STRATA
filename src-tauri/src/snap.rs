//! Windows 11 Snap Layouts for the custom title bar.
//!
//! Windows shows the Snap Layouts flyout only when the window under the
//! pointer answers `WM_NCHITTEST` with `HTMAXBUTTON`. The HTML title bar
//! lives inside the WebView2 child window, which never does, so a
//! transparent native child window is laid over the page's maximize button.
//! It reports itself as the maximize button, toggles maximize on click, and
//! tells the page when the pointer enters or leaves so the HTML button can
//! show its hover state.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use tauri::{Emitter, Manager, Runtime, WebviewWindow};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, HBRUSH, NULL_BRUSH, ValidateRect};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, GetParent, HTMAXBUTTON, HWND_TOP, IDC_ARROW,
    IsZoomed, LoadCursorW, RegisterClassW, SW_MAXIMIZE, SW_RESTORE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SetWindowPos, ShowWindow, WINDOW_EX_STYLE, WM_DPICHANGED, WM_ERASEBKGND, WM_NCHITTEST,
    WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE,
    WM_PAINT, WM_SIZE, WNDCLASSW, WS_CHILD, WS_CLIPSIBLINGS, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

/// Title bar height in CSS pixels; keep in step with `ui/src/shell/window.ts`.
const TITLEBAR_HEIGHT: f64 = 32.0;
/// Caption button width in CSS pixels; keep in step with `ui/src/shell/window.ts`.
const BUTTON_WIDTH: f64 = 46.0;
/// Event the page listens to for the maximize button's hover state.
const HOVER_EVENT: &str = "titlebar://maximize-hover";

type HoverSink = Box<dyn Fn(bool) + Send + Sync>;

/// Forwards hover changes to the page; set once when the overlay is installed.
static HOVER: OnceLock<HoverSink> = OnceLock::new();
/// The overlay window, stored as a raw handle value (0 before install).
static OVERLAY: AtomicIsize = AtomicIsize::new(0);
/// Whether a non-client mouse-leave notification is armed.
static TRACKING: AtomicBool = AtomicBool::new(false);

/// Installs the overlay on `window`. Call once, after the window exists and
/// only when it is undecorated.
///
/// # Errors
///
/// Fails if the window handle is unavailable or the overlay window cannot be
/// created; the title bar then still works, without the Snap Layouts flyout.
pub fn install<R: Runtime>(window: &WebviewWindow<R>) -> Result<(), String> {
    let app = window.app_handle().clone();
    let _ = HOVER.set(Box::new(move |hovered| {
        let _ = app.emit(HOVER_EVENT, hovered);
    }));
    // NOTE: Tauri may build against a different `windows` crate version, so
    // the handle crosses over as its raw pointer value.
    let parent = HWND(window.hwnd().map_err(|e| e.to_string())?.0 as _);

    // SAFETY: the class name and window procedure are 'static; the parent
    // handle is the live top-level window this setup hook was called for.
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(overlay_proc),
            hInstance: instance.into(),
            lpszClassName: w!("StrataSnapOverlay"),
            hbrBackground: HBRUSH(GetStockObject(NULL_BRUSH).0),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        // NOTE: a zero return also means "already registered", which is fine.
        RegisterClassW(&class);
        // NOTE: WS_EX_LAYERED / WS_EX_TRANSPARENT would make the window
        // invisible to hit testing, so the overlay stays a plain child
        // window that paints nothing.
        let overlay = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("StrataSnapOverlay"),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            0,
            0,
            0,
            0,
            Some(parent),
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| e.to_string())?;
        OVERLAY.store(overlay.0 as isize, Ordering::Relaxed);
        place(parent, overlay);
        if !SetWindowSubclass(parent, Some(parent_proc), 1, 0).as_bool() {
            return Err("could not watch the main window for resizes".into());
        }
    }
    Ok(())
}

/// Moves the overlay over the maximize button: second from the right, at the
/// top, scaled to the window's DPI, and above the WebView.
///
/// # Safety
///
/// `parent` and `overlay` must be valid window handles.
unsafe fn place(parent: HWND, overlay: HWND) {
    let mut rc = RECT::default();
    // SAFETY: guaranteed by the caller; `rc` is a valid out pointer.
    let _ = unsafe { GetClientRect(parent, &mut rc) };
    // SAFETY: `parent` is a valid window handle per the caller.
    let scale = f64::from(unsafe { GetDpiForWindow(parent) }) / 96.0;
    let width = (BUTTON_WIDTH * scale).round() as i32;
    let height = (TITLEBAR_HEIGHT * scale).round() as i32;
    // SAFETY: both handles are valid per the caller.
    let _ = unsafe {
        SetWindowPos(
            overlay,
            Some(HWND_TOP),
            rc.right - 2 * width,
            0,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    };
}

/// Keeps the overlay in place when the main window resizes or changes DPI.
unsafe extern "system" fn parent_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_SIZE || msg == WM_DPICHANGED {
        let overlay = OVERLAY.load(Ordering::Relaxed);
        if overlay != 0 {
            // SAFETY: `hwnd` is the window this subclass is attached to and
            // the overlay is its child, alive as long as the parent.
            unsafe { place(hwnd, HWND(overlay as _)) };
        }
    }
    // SAFETY: forwarding the unmodified message to the next handler.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// The overlay's window procedure: claims to be the maximize button.
unsafe extern "system" fn overlay_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let hover = |on: bool| {
        if let Some(sink) = HOVER.get() {
            sink(on);
        }
    };
    match msg {
        WM_NCHITTEST => LRESULT(HTMAXBUTTON as isize),
        WM_NCMOUSEMOVE => {
            if !TRACKING.swap(true, Ordering::Relaxed) {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE | TME_NONCLIENT,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                // SAFETY: `track` is a fully initialized struct for this window.
                let _ = unsafe { TrackMouseEvent(&mut track) };
                hover(true);
            }
            LRESULT(0)
        }
        WM_NCMOUSELEAVE => {
            TRACKING.store(false, Ordering::Relaxed);
            hover(false);
            LRESULT(0)
        }
        // NOTE: swallow the press so Windows does not run its own caption
        // button logic; the toggle happens on release, like a real button.
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK => LRESULT(0),
        WM_NCLBUTTONUP => {
            // SAFETY: `hwnd` is the overlay; its parent is the main window.
            unsafe {
                if let Ok(parent) = GetParent(hwnd) {
                    let state = if IsZoomed(parent).as_bool() {
                        SW_RESTORE
                    } else {
                        SW_MAXIMIZE
                    };
                    let _ = ShowWindow(parent, state);
                }
            }
            LRESULT(0)
        }
        WM_PAINT => {
            // SAFETY: marks the overlay's own client area as painted.
            let _ = unsafe { ValidateRect(Some(hwnd), None) };
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        // SAFETY: default handling for every other message.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
