/**
 * Small React hooks over browser state and the entry-info cache.
 */
import { useSyncExternalStore } from "react";
import { useSettings } from "../store/settings";
import type { EntryInfo, EntryInfoProvider } from "./entries";

/**
 * Tracks a media query.
 *
 * @param query - CSS media query.
 * @returns Whether it matches (false where `matchMedia` is unavailable).
 */
export function useMediaQuery(query: string): boolean {
  return useSyncExternalStore(
    (cb) => {
      if (typeof window.matchMedia !== "function") return () => undefined;
      const mq = window.matchMedia(query);
      mq.addEventListener("change", cb);
      return () => {
        mq.removeEventListener("change", cb);
      };
    },
    () => (typeof window.matchMedia === "function" ? window.matchMedia(query).matches : false),
  );
}

/** Whether the user asked the OS to reduce motion. */
export function useReducedMotion(): boolean {
  return useMediaQuery("(prefers-reduced-motion: reduce)");
}

/** Whether the effective theme is dark (setting, else the Windows preference). */
export function useDark(): boolean {
  const theme = useSettings((s) => s.theme);
  const systemDark = useMediaQuery("(prefers-color-scheme: dark)");
  return theme === "dark" || (theme === "system" && systemDark);
}

/**
 * Reads one entry's info, re-rendering when it arrives.
 *
 * @param provider - Provider for the volume, or `null`.
 * @param id - Entry id, or `null`.
 * @returns The info, or `undefined` while loading / unknown.
 */
export function useEntryInfo(provider: EntryInfoProvider | null, id: number | null): EntryInfo | undefined {
  return useSyncExternalStore(
    (cb) => provider?.subscribe(cb) ?? (() => undefined),
    () => (provider && id !== null ? provider.get(id) : undefined),
  );
}
