/**
 * Wording for scan progress, shared by the sidebar, the status bar and the
 * workspace notice so every surface says the same thing.
 */

/**
 * Whole-percent progress; never shows 100% before the scan finishes.
 *
 * @param fraction - Progress in `[0, 1]`.
 * @example
 * scanPercent(0.426) // "42%"
 */
export function scanPercent(fraction: number): string {
  return `${Math.min(99, Math.floor(Math.max(0, fraction) * 100))}%`;
}

/**
 * Remaining-time phrase for an ETA.
 *
 * @param secs - Estimated seconds left.
 * @example
 * etaText(150) // "about 3 min left"
 */
export function etaText(secs: number): string {
  if (secs < 60) return "less than a minute left";
  const m = Math.round(secs / 60);
  return m < 60 ? `about ${m} min left` : `about ${Math.round(m / 60)} h left`;
}
