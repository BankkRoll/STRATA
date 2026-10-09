/**
 * Typed bridge to the Tauri backend.
 *
 * Every backend call goes through this module so the rest of the UI never
 * touches raw `invoke`, and so components render outside Tauri (unit tests,
 * the plain `vite` dev server) with a well-defined failure instead of a crash.
 *
 * Responsibilities:
 * - {@link call}: `invoke` wrapper that turns "not in Tauri" and "command not
 *   registered" into {@link BackendUnavailableError}.
 * - {@link getAppInfo} and {@link getCapabilities}: startup facts.
 *
 * Each command's arguments and result types are documented where it is wrapped.
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

/**
 * A backend command the UI needs does not exist in this build (or the UI is
 * not running inside Tauri). Features show a designed "not available"
 * state with {@link BackendUnavailableError.reason} instead of faking data.
 */
export class BackendUnavailableError extends Error {
  override name = "BackendUnavailableError";

  /**
   * @param command - The Tauri command name.
   * @param reason - User-facing explanation.
   */
  constructor(
    readonly command: string,
    readonly reason: string,
  ) {
    super(`${command}: ${reason}`);
  }
}

/** Whether the UI runs inside the Strata shell (false under tests and plain Vite). */
export function inTauri(): boolean {
  return isTauri();
}

/**
 * Invokes a backend command.
 *
 * @param command - Tauri command name.
 * @param args - Command arguments (camelCase keys, per Tauri's convention).
 * @returns The command's response.
 * @throws {BackendUnavailableError} Outside Tauri, or when the command is not
 *   registered in this build.
 */
export async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri()) throw new BackendUnavailableError(command, "Strata's engine is not running (browser preview).");
  try {
    return await invoke<T>(command, args);
  } catch (err) {
    // NOTE: Tauri rejects unregistered commands with a plain string like
    // "command foo not found"; anything else is a real error from the command.
    if (typeof err === "string" && /command .* not found/i.test(err)) {
      throw new BackendUnavailableError(command, "This build does not include that feature yet.");
    }
    const failure = asCommandFailure(err);
    if (failure?.code === "unavailable") throw new BackendUnavailableError(command, failure.message);
    throw err;
  }
}

/**
 * A command failure as the backend serializes it (`CommandError` in
 * `src-tauri/src/error.rs`).
 */
export interface CommandFailure {
  /** `unavailable`, `not_found`, `bad_request`, `busy`, `declined`, `io` or `internal`. */
  code: string;
  /** User-facing explanation. */
  message: string;
}

/**
 * Recognizes a backend {@link CommandFailure}.
 *
 * @param err - A rejected `invoke` value.
 * @returns The failure, or `null` for anything else.
 */
export function asCommandFailure(err: unknown): CommandFailure | null {
  if (typeof err !== "object" || err === null) return null;
  const { code, message } = err as Record<string, unknown>;
  return typeof code === "string" && typeof message === "string" ? { code, message } : null;
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

/**
 * Fetches the list of backend command names this build implements
 * (`app_capabilities`). Missing command → empty list, so every
 * backend-dependent action shows as disabled with a reason.
 *
 * @returns Supported command names.
 */
export async function getCapabilities(): Promise<ReadonlySet<string>> {
  try {
    return new Set(await call<string[]>("app_capabilities"));
  } catch (err) {
    if (err instanceof BackendUnavailableError) return new Set();
    throw err;
  }
}

/**
 * Turns any thrown value into a user-facing message.
 *
 * @param err - Caught value.
 * @returns A readable message.
 */
export function errorMessage(err: unknown): string {
  if (err instanceof BackendUnavailableError) return err.reason;
  if (err instanceof Error) return err.message;
  if (typeof err === "string") return err;
  const failure = asCommandFailure(err);
  if (failure) return failure.message;
  return "Unexpected error";
}
