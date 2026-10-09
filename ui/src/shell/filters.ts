/**
 * Presentation of view filters as chips: one chip per active filter, each
 * removable on its own.
 */
import { formatBytes, type SizeUnits } from "../lib/format";
import { categoryInfo } from "../lib/palette";
import type { ViewFilters } from "../lib/types";

/** One removable filter chip. */
export interface FilterChip {
  id: "categories" | "minBytes" | "modified" | "excluded";
  label: string;
  /** Filters with this chip removed. */
  without: ViewFilters;
}

/** Minimum-size presets offered by the filter menu (bytes; 0 = off). */
export const MIN_SIZE_PRESETS: readonly number[] = [0, 1024 ** 2, 10 * 1024 ** 2, 100 * 1024 ** 2, 1024 ** 3, 10 * 1024 ** 3];

/** "Modified within" presets in days (`null` = any time). */
export const MODIFIED_PRESETS: readonly (number | null)[] = [null, 1, 7, 30, 90, 365];

/**
 * Describes a "modified within" preset.
 *
 * @param days - Days, or `null`.
 */
export function modifiedLabel(days: number | null): string {
  if (days === null) return "Any time";
  if (days === 1) return "Last 24 hours";
  if (days === 365) return "Last year";
  return `Last ${days} days`;
}

/**
 * Builds the chips for the active filters.
 *
 * @param f - Filters.
 * @param units - Size units for the minimum-size chip.
 * @returns Chips in a stable order.
 */
export function filterChips(f: ViewFilters, units: SizeUnits = "binary"): FilterChip[] {
  const out: FilterChip[] = [];
  if (f.categories.length > 0) {
    const names = f.categories.map((c) => categoryInfo(c).label);
    out.push({ id: "categories", label: names.length <= 2 ? names.join(", ") : `${names.length} categories`, without: { ...f, categories: [] } });
  }
  if (f.minBytes > 0) out.push({ id: "minBytes", label: `≥ ${formatBytes(f.minBytes, { units })}`, without: { ...f, minBytes: 0 } });
  if (f.modifiedWithinDays !== null) out.push({ id: "modified", label: `Modified: ${modifiedLabel(f.modifiedWithinDays).toLowerCase()}`, without: { ...f, modifiedWithinDays: null } });
  if (f.excluded.length > 0) out.push({ id: "excluded", label: `${f.excluded.length} hidden`, without: { ...f, excluded: [] } });
  return out;
}

/**
 * Plain-text summary of filters (saved filter tooltips).
 *
 * @param f - Filters.
 * @returns One phrase per active filter; `["No filters"]` when none.
 */
export function describeFilters(f: ViewFilters): string[] {
  const chips = filterChips(f);
  return chips.length > 0 ? chips.map((c) => c.label) : ["No filters"];
}
