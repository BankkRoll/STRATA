/**
 * Window chrome: whether Strata draws its own title bar, and the window
 * controls behind it.
 *
 * The custom title bar needs the main window to run without native
 * decorations. If the window is decorated anyway, the native frame stays and
 * the in-app bar shows no window controls, so there are never two sets of
 * caption buttons. The decision is:
 *
 * 1. `?titlebar=custom|native` in the URL, or the `strata.flags.titleBar`
 *    storage flag, forces a mode (development and support).
 * 2. Otherwise Strata asks the window: undecorated means custom.
 *
 * Snap Layouts on the maximize button come from a native overlay window in
 * the shell process that answers `WM_NCHITTEST` with `HTMAXBUTTON`; it
 * reports hover through the `titlebar://maximize-hover` event so the HTML
 * button can paint its hover state while the overlay owns the mouse.
 */
import { isTauri } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";

/** Title bar flavour. */
export type TitleBarMode = "native" | "custom";

/** Storage key of the title bar override flag. */
export const TITLE_BAR_FLAG = "strata.flags.titleBar";

/** Title bar height in CSS px; the native snap overlay uses the same value. */
export const TITLE_BAR_HEIGHT = 32;

/** Width of one caption button in CSS px; the native snap overlay uses the same value. */
export const CAPTION_BUTTON_WIDTH = 46;

/** Event the native snap overlay emits with `true` / `false` on hover changes. */
export const MAXIMIZE_HOVER_EVENT = "titlebar://maximize-hover";

/**
 * Reads a forced title bar mode from the URL or storage.
 *
 * @returns The override, or `null` to follow the window.
 */
export function titleBarOverride(): TitleBarMode | null {
  try {
    const q = new URLSearchParams(window.location.search).get("titlebar");
    const v = q ?? window.localStorage.getItem(TITLE_BAR_FLAG);
    return v === "custom" || v === "native" ? v : null;
  } catch {
    return null;
  }
}

async function currentWindow() {
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  return getCurrentWindow();
}

/**
 * Decides the title bar mode once at startup.
 *
 * @returns `custom` when forced or when the window has no native frame.
 */
export async function detectTitleBarMode(): Promise<TitleBarMode> {
  const forced = titleBarOverride();
  if (forced) return forced;
  if (!isTauri()) return "native";
  try {
    return (await (await currentWindow()).isDecorated()) ? "native" : "custom";
  } catch {
    return "native";
  }
}

/** Window control actions. Each is a no-op outside the Strata shell. */
export const windowControls = {
  async minimize(): Promise<void> {
    if (isTauri()) await (await currentWindow()).minimize();
  },
  async toggleMaximize(): Promise<void> {
    if (isTauri()) await (await currentWindow()).toggleMaximize();
  },
  async close(): Promise<void> {
    if (isTauri()) await (await currentWindow()).close();
  },
};

/** Live window facts for the title bar. */
export interface WindowChrome {
  mode: TitleBarMode;
  maximized: boolean;
  /** The native snap overlay reports the pointer over the maximize button. */
  maximizeHover: boolean;
}

/**
 * Tracks the title bar mode, the maximized state and the snap overlay hover.
 *
 * @returns Current chrome state (`native` until detection finishes).
 */
export function useWindowChrome(): WindowChrome {
  const [chrome, setChrome] = useState<WindowChrome>(() => ({ mode: titleBarOverride() ?? "native", maximized: false, maximizeHover: false }));
  useEffect(() => {
    const live = { cancelled: false };
    const cleanups: (() => void)[] = [];
    // Listeners can resolve after unmount; those are released immediately.
    const track = (f: () => void) => {
      if (live.cancelled) f();
      else cleanups.push(f);
    };
    void detectTitleBarMode().then(async (mode) => {
      if (live.cancelled) return;
      setChrome((c) => ({ ...c, mode }));
      if (mode !== "custom" || !isTauri()) return;
      try {
        const win = await currentWindow();
        const refresh = () => {
          void win.isMaximized().then((maximized) => {
            if (!live.cancelled) setChrome((c) => (c.maximized === maximized ? c : { ...c, maximized }));
          });
        };
        refresh();
        track(await win.onResized(refresh));
        const { listen } = await import("@tauri-apps/api/event");
        track(
          await listen<boolean>(MAXIMIZE_HOVER_EVENT, (e) => {
            if (!live.cancelled) setChrome((c) => ({ ...c, maximizeHover: e.payload }));
          }),
        );
      } catch (err) {
        console.error("window chrome", err);
      }
    });
    return () => {
      live.cancelled = true;
      for (const f of cleanups.splice(0)) f();
    };
  }, []);
  useEffect(() => {
    document.documentElement.toggleAttribute("data-custom-titlebar", chrome.mode === "custom");
  }, [chrome.mode]);
  return chrome;
}
