/**
 * The keyboard map: every shortcut the app understands, in one table that
 * drives both the global key handler and the `?` cheat sheet, so the two can
 * never disagree.
 *
 * Shortcuts with a `command` are dispatched by the shell; the rest are
 * handled by the focused component (the map, the list, menus) and are listed
 * for reference only.
 */
import { ENTRY_ACTIONS } from "../lib/commands";
import { AREAS, LIST_VIEW_SHORTCUT, VISUAL_VIEWS } from "./areas";

/** Commands the shell dispatches from the keyboard. */
export type ShellCommand =
  | "palette"
  | "fileSearch"
  | "settings"
  | "editPath"
  | "toggleSidebar"
  | "toggleInspector"
  | "toggleList"
  | "cheatSheet"
  | "nextRegion"
  | "prevRegion"
  | "goUp"
  | "newTab"
  | "closeTab"
  | "nextTab"
  | "prevTab"
  | "moveTabLeft"
  | "moveTabRight"
  | "listMode"
  | `area:${string}`
  | `view:${string}`;

/** Cheat sheet groups, in display order. */
export const SHORTCUT_GROUPS = ["General", "Navigation", "Workspace", "Views", "Selection", "Map and list"] as const;

/** A cheat sheet group. */
export type ShortcutGroup = (typeof SHORTCUT_GROUPS)[number];

/** One shortcut. */
export interface Shortcut {
  /** Key combinations, e.g. `Ctrl+Shift+PageUp`; any of them triggers it. */
  keys: readonly string[];
  description: string;
  group: ShortcutGroup;
  /** Dispatched by the shell; absent for component-level keys. */
  command?: ShellCommand;
  /** Also fires while typing in a text field (Ctrl/Alt chords only). */
  whileTyping?: boolean;
}

/** Every shortcut, in cheat sheet order. */
export const SHORTCUTS: readonly Shortcut[] = [
  { keys: ["Ctrl+K", "Ctrl+P"], description: "Search files or run a command", group: "General", command: "palette", whileTyping: true },
  { keys: ["Ctrl+F"], description: "Search files by name", group: "General", command: "fileSearch", whileTyping: true },
  { keys: ["Ctrl+,"], description: "Open settings", group: "General", command: "settings", whileTyping: true },
  { keys: ["?", "Ctrl+/"], description: "Show keyboard shortcuts", group: "General", command: "cheatSheet" },
  { keys: ["F6"], description: "Move focus to the next region", group: "General", command: "nextRegion", whileTyping: true },
  { keys: ["Shift+F6"], description: "Move focus to the previous region", group: "General", command: "prevRegion", whileTyping: true },
  { keys: ["Esc"], description: "Close a menu, dialog or overlay", group: "General" },
  ...AREAS.filter((a) => a.id !== "settings").map((a): Shortcut => ({ keys: [a.shortcut], description: `Go to ${a.label}`, group: "Navigation", command: `area:${a.id}`, whileTyping: true })),
  { keys: ["Ctrl+L"], description: "Edit the path", group: "Navigation", command: "editPath", whileTyping: true },
  { keys: ["Alt+ArrowUp"], description: "Go up one folder", group: "Navigation", command: "goUp" },
  { keys: ["Ctrl+T"], description: "Open the current folder in a new tab", group: "Workspace", command: "newTab", whileTyping: true },
  { keys: ["Ctrl+W"], description: "Close the tab", group: "Workspace", command: "closeTab", whileTyping: true },
  { keys: ["Ctrl+Tab", "Ctrl+PageDown"], description: "Next tab", group: "Workspace", command: "nextTab", whileTyping: true },
  { keys: ["Ctrl+Shift+Tab", "Ctrl+PageUp"], description: "Previous tab", group: "Workspace", command: "prevTab", whileTyping: true },
  { keys: ["Ctrl+Shift+PageUp"], description: "Move the tab left", group: "Workspace", command: "moveTabLeft", whileTyping: true },
  { keys: ["Ctrl+Shift+PageDown"], description: "Move the tab right", group: "Workspace", command: "moveTabRight", whileTyping: true },
  { keys: ["Ctrl+B"], description: "Show or hide the sidebar", group: "Workspace", command: "toggleSidebar", whileTyping: true },
  { keys: ["Ctrl+J"], description: "Show or hide the list pane", group: "Workspace", command: "toggleList", whileTyping: true },
  { keys: ["Ctrl+Alt+B"], description: "Show or hide the inspector", group: "Workspace", command: "toggleInspector", whileTyping: true },
  ...VISUAL_VIEWS.map((v): Shortcut => ({ keys: [v.shortcut], description: `Show the ${v.label.toLowerCase()}`, group: "Views", command: `view:${v.id}`, whileTyping: true })),
  { keys: [LIST_VIEW_SHORTCUT], description: "Show the list only", group: "Views", command: "listMode", whileTyping: true },
  ...ENTRY_ACTIONS.filter((a) => a.shortcut).map((a): Shortcut => ({ keys: [a.shortcut === "Del" ? "Delete" : (a.shortcut as string)], description: a.label, group: "Selection" })),
  { keys: ["↑ ↓ ← →"], description: "Move between items", group: "Map and list" },
  { keys: ["Enter"], description: "Open the folder", group: "Map and list" },
  { keys: ["Backspace"], description: "Go up (map)", group: "Map and list" },
  { keys: ["Space", "Ctrl+Space"], description: "Select, or add to the selection", group: "Map and list" },
  { keys: ["+", "-", "0"], description: "Zoom in, out, reset (map)", group: "Map and list" },
  { keys: ["Shift+F10"], description: "Open the actions menu", group: "Map and list" },
  { keys: ["Ctrl+C"], description: "Copy the selected rows (list)", group: "Map and list" },
];

/** A parsed key combination. */
export interface Combo {
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  /** Lower-cased `KeyboardEvent.key`. */
  key: string;
}

const KEY_ALIASES: Readonly<Record<string, string>> = { esc: "escape", space: " ", del: "delete" };

/**
 * Parses `Ctrl+Shift+PageUp`-style text.
 *
 * @param text - Display form.
 * @returns The combination.
 * @example
 * parseCombo("Ctrl+,") // { ctrl: true, alt: false, shift: false, key: "," }
 */
export function parseCombo(text: string): Combo {
  // "+" is itself a key, so split on "+" only when it joins two names.
  const parts = text === "+" ? ["+"] : text.split(/\+(?=.)/);
  const combo: Combo = { ctrl: false, alt: false, shift: false, key: "" };
  for (const p of parts) {
    const l = p.toLowerCase();
    if (l === "ctrl") combo.ctrl = true;
    else if (l === "alt") combo.alt = true;
    else if (l === "shift") combo.shift = true;
    else combo.key = KEY_ALIASES[l] ?? l;
  }
  return combo;
}

/** Keys whose character already implies Shift on common layouts. */
const SHIFTED = new Set(["?", "+", "<", ">", ":", "\"", "{", "}", "|", "_", "~", "!", "@", "#", "$", "%", "^", "&", "*", "(", ")"]);

/**
 * Whether a keyboard event matches a combination.
 *
 * @param e - The event (only modifier flags, `key` and `code` are read).
 * @param combo - Parsed combination.
 */
export function matchesCombo(e: Pick<KeyboardEvent, "ctrlKey" | "altKey" | "shiftKey" | "metaKey" | "key" | "code">, combo: Combo): boolean {
  if (e.metaKey || e.ctrlKey !== combo.ctrl || e.altKey !== combo.alt) return false;
  let key = e.key.toLowerCase();
  // Alt+digit and Ctrl+digit report layout-specific characters on some
  // keyboards (AZERTY), so fall back to the physical digit key.
  if (/^digit\d$/i.test(e.code) && /^\d$/.test(combo.key)) key = e.code.slice(5);
  if (key !== combo.key) return false;
  return SHIFTED.has(combo.key) || e.shiftKey === combo.shift;
}

const PARSED: readonly { shortcut: Shortcut; combos: Combo[] }[] = SHORTCUTS.filter((s) => s.command).map((shortcut) => ({
  shortcut,
  combos: shortcut.keys.map(parseCombo),
}));

/**
 * Finds the shell command for a key event.
 *
 * @param e - Key event.
 * @param typing - Focus is in a text field.
 * @returns The command, or `null`.
 */
export function commandFor(e: Pick<KeyboardEvent, "ctrlKey" | "altKey" | "shiftKey" | "metaKey" | "key" | "code">, typing: boolean): ShellCommand | null {
  for (const { shortcut, combos } of PARSED) {
    if (typing && !shortcut.whileTyping) continue;
    if (combos.some((c) => matchesCombo(e, c))) return shortcut.command ?? null;
  }
  return null;
}

/**
 * Splits a combination into display keycaps.
 *
 * @param text - `Ctrl+Shift+PageUp`.
 * @returns `["Ctrl", "Shift", "PgUp"]`.
 */
export function keycaps(text: string): string[] {
  const names: Readonly<Record<string, string>> = { ArrowUp: "↑", ArrowDown: "↓", ArrowLeft: "←", ArrowRight: "→", PageUp: "PgUp", PageDown: "PgDn", Escape: "Esc", Delete: "Del" };
  return (text === "+" ? ["+"] : text.split(/\+(?=.)/)).map((k) => names[k] ?? k);
}

/**
 * The display shortcut of a shell command (first binding), for tooltips.
 *
 * @param command - Shell command.
 * @returns E.g. `Ctrl+B`, or `undefined`.
 */
export function shortcutOf(command: ShellCommand): string | undefined {
  return SHORTCUTS.find((s) => s.command === command)?.keys[0];
}

/** `aria-keyshortcuts` form of a display combination (`Ctrl` → `Control`). */
export function ariaKeys(text: string | undefined): string | undefined {
  return text?.replace(/\bCtrl\b/g, "Control");
}
