/**
 * Applies theme and backdrop choices to the document root.
 *
 * Colors are CSS custom properties keyed off `data-theme` and `data-backdrop`
 * on `<html>`, so switching never re-renders React.
 */
import type { Backdrop } from "./backend";
import type { ThemePreference } from "../store/settings";

/**
 * Sets the theme attribute on `root`.
 *
 * @param root - Element carrying the theme attributes (normally `<html>`).
 * @param theme - `system` removes the override so `prefers-color-scheme` decides.
 */
export function applyTheme(root: HTMLElement, theme: ThemePreference): void {
  if (theme === "system") {
    root.removeAttribute("data-theme");
  } else {
    root.setAttribute("data-theme", theme);
  }
}

/**
 * Sets the backdrop attribute on `root`.
 *
 * @param root - Element carrying the theme attributes (normally `<html>`).
 * @param backdrop - `mica` lets the window material show through the app background.
 */
export function applyBackdrop(root: HTMLElement, backdrop: Backdrop): void {
  root.setAttribute("data-backdrop", backdrop);
}
