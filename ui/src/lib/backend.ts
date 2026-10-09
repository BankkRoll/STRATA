/**
 * Typed bridge to the Tauri backend.
 *
 * Every backend call goes through this module so the rest of the UI never
 * touches raw command names, and so components can render outside Tauri
 * (unit tests, plain `vite` dev server) with a well-defined fallback.
 */
import { invoke, isTauri } from "@tauri-apps/api/core";

/** Which window backdrop the backend managed to apply. */
export type Backdrop = "mica" | "solid";

/** Static facts about the running app, fetched once at startup. */
export interface AppInfo {
  /** Semantic version of the app build. */
  version: string;
  /** Windows build number (e.g. 22631), or 0 when not running on Windows. */
  windowsBuild: number;
  /** Backdrop applied to the main window; `solid` means the UI must paint its own background. */
  backdrop: Backdrop;
}

const BROWSER_FALLBACK: AppInfo = { version: "dev", windowsBuild: 0, backdrop: "solid" };

/**
 * Fetches static app info from the backend.
 *
 * @returns The backend's view of the app, or a solid-backdrop fallback when
 *   running outside the Tauri shell.
 */
export async function getAppInfo(): Promise<AppInfo> {
  if (!isTauri()) return BROWSER_FALLBACK;
  return invoke<AppInfo>("app_info");
}
