/**
 * Color modes, the packed `color_key` encoding, and the palettes behind them.
 *
 * Responsibilities:
 * - {@link decodeColorKey} / {@link encodeColorKey}: the bit layout of the
 *   `u32` color key every layout record carries (documented in
 *   `docs/tracks/ui.md`). All modes are packed into one key, so switching
 *   color mode is a shader uniform change, never a relayout.
 * - Fixed accessible palettes with light and dark variants for categories
 *   (SPEC §12.5) and safety tiers, plus a pattern per category for
 *   color-blind users.
 * - {@link buildPaletteTexture}: the RGBA lookup texture the renderers sample.
 */

/** What block colors encode (SPEC §16.1). */
export type ColorMode = "category" | "fileType" | "age" | "app" | "safety" | "recent";

/** Every color mode with its label, in menu order. */
export const COLOR_MODES: readonly { id: ColorMode; label: string }[] = [
  { id: "category", label: "Category" },
  { id: "fileType", label: "File type" },
  { id: "age", label: "Age" },
  { id: "app", label: "Owning app" },
  { id: "safety", label: "Safety tier" },
  { id: "recent", label: "Changed recently" },
];

/** Shader index of each color mode. */
export const COLOR_MODE_INDEX: Readonly<Record<ColorMode, number>> = {
  category: 0,
  safety: 1,
  age: 2,
  fileType: 3,
  app: 4,
  recent: 5,
};

/** Fields packed into a color key. */
export interface ColorKeyFields {
  /** Category id (`strata_core::Category` discriminant, 0–14). */
  category: number;
  /** Safety tier: 0 unclassified, 1 safe, 2 probably, 3 careful, 4 never. */
  safety: number;
  /** Age bucket (see {@link AGE_BUCKETS}); 0 unknown, 31 suspicious timestamp. */
  age: number;
  /** File-type slot (0 = directory / none, 1–255 per the backend's type table). */
  fileType: number;
  /** Owning-app slot (0 = unattributed, 1–1023 per the backend's app table). */
  app: number;
  /** Changed by a live update within the decay window. */
  recent: boolean;
}

/**
 * Unpacks a color key.
 *
 * Bits: `category 0–3 | safety 4–6 | age 7–11 | fileType 12–19 | app 20–29 | recent 30`.
 *
 * @param key - The u32 from a node record.
 * @returns The decoded fields.
 */
export function decodeColorKey(key: number): ColorKeyFields {
  return {
    category: key & 0xf,
    safety: (key >>> 4) & 0x7,
    age: (key >>> 7) & 0x1f,
    fileType: (key >>> 12) & 0xff,
    app: (key >>> 20) & 0x3ff,
    recent: ((key >>> 30) & 1) === 1,
  };
}

/**
 * Packs fields into a color key (the inverse of {@link decodeColorKey}).
 *
 * @param f - Fields; out-of-range values are masked.
 * @returns The u32 key.
 */
export function encodeColorKey(f: ColorKeyFields): number {
  return (
    ((f.category & 0xf) |
      ((f.safety & 0x7) << 4) |
      ((f.age & 0x1f) << 7) |
      ((f.fileType & 0xff) << 12) |
      ((f.app & 0x3ff) << 20) |
      ((f.recent ? 1 : 0) << 30)) >>>
    0
  );
}

// -----------------------------------------------------------------------------
// Categories
// -----------------------------------------------------------------------------

/** Fill patterns for color-blind mode; index is the shader pattern id. */
export const PATTERNS = ["none", "diagonal", "antidiagonal", "horizontal", "vertical", "dots", "crosshatch", "grid"] as const;

/** A fill pattern name. */
export type Pattern = (typeof PATTERNS)[number];

/** One top-level category with its fixed colors. */
export interface CategoryInfo {
  /** `strata_core::Category` discriminant. */
  id: number;
  /** Serde name used by the backend (`snake_case`). */
  key: string;
  /** Display label. */
  label: string;
  /** Fill color on light backgrounds. */
  light: string;
  /** Fill color on dark backgrounds. */
  dark: string;
  /** Pattern shown in color-blind mode. */
  pattern: Pattern;
}

/**
 * The 15 categories in display order (SPEC §12.5). Hues are spaced so
 * neighbors differ in lightness as well as hue, and each also carries a
 * distinct pattern, so no category relies on hue alone.
 */
export const CATEGORIES: readonly CategoryInfo[] = [
  { id: 1, key: "system", label: "System", light: "#5b6b7f", dark: "#8a9bb0", pattern: "horizontal" },
  { id: 2, key: "apps", label: "Apps", light: "#3b6fd8", dark: "#6e9bf0", pattern: "none" },
  { id: 3, key: "games", label: "Games", light: "#8e44c9", dark: "#b587e8", pattern: "dots" },
  { id: 4, key: "ai_models", label: "AI models", light: "#d6338a", dark: "#ee7db8", pattern: "crosshatch" },
  { id: 5, key: "dev_build", label: "Dev & build", light: "#e07b00", dark: "#f5a54a", pattern: "diagonal" },
  { id: 6, key: "caches", label: "Caches", light: "#2a9d8f", dark: "#4fc4b5", pattern: "antidiagonal" },
  { id: 7, key: "temp", label: "Temp", light: "#b89600", dark: "#e3c84a", pattern: "vertical" },
  { id: 8, key: "downloads", label: "Downloads", light: "#1f8fd1", dark: "#5db6ec", pattern: "grid" },
  { id: 9, key: "documents", label: "Documents", light: "#4a8f3a", dark: "#7dbe6a", pattern: "none" },
  { id: 10, key: "media", label: "Media", light: "#c2410c", dark: "#eb7a4e", pattern: "dots" },
  { id: 11, key: "archives", label: "Archives & disk images", light: "#7a5c3e", dark: "#b08a64", pattern: "grid" },
  { id: 12, key: "cloud", label: "Cloud placeholders", light: "#5f9cc0", dark: "#9cc7e0", pattern: "horizontal" },
  { id: 13, key: "recycle_bin", label: "Recycle Bin", light: "#7f7f30", dark: "#b5b56a", pattern: "crosshatch" },
  { id: 14, key: "ntfs_metadata", label: "NTFS metadata", light: "#6d6d6d", dark: "#9a9a9a", pattern: "vertical" },
  { id: 0, key: "unknown", label: "Unknown", light: "#a0a4ab", dark: "#5e636b", pattern: "antidiagonal" },
];

const CATEGORY_BY_ID = new Map(CATEGORIES.map((c) => [c.id, c]));

/**
 * Looks up a category by id; unknown ids map to "Unknown".
 *
 * @param id - Category discriminant.
 * @returns The category entry.
 */
export function categoryInfo(id: number): CategoryInfo {
  return CATEGORY_BY_ID.get(id) ?? (CATEGORY_BY_ID.get(0) as CategoryInfo);
}

/** Safety tiers in key order (index = safety bits). */
export const SAFETY_TIERS: readonly { id: number; key: string; label: string; light: string; dark: string }[] = [
  { id: 0, key: "unclassified", label: "Not classified", light: "#a0a4ab", dark: "#5e636b" },
  // Okabe–Ito: distinguishable under the common color-vision deficiencies.
  { id: 1, key: "safe", label: "Safe", light: "#009e73", dark: "#2bc79a" },
  { id: 2, key: "probably", label: "Probably safe", light: "#3d9ad1", dark: "#56b4e9" },
  { id: 3, key: "careful", label: "Careful", light: "#d18f00", dark: "#e69f00" },
  { id: 4, key: "never", label: "Never", light: "#c25400", dark: "#e3712a" },
];

/** Age buckets: upper bound in seconds (exclusive) and label; index = age bits. */
export const AGE_BUCKETS: readonly { maxSecs: number; label: string }[] = [
  { maxSecs: 0, label: "Unknown" },
  { maxSecs: 3600, label: "Last hour" },
  { maxSecs: 6 * 3600, label: "Last 6 hours" },
  { maxSecs: 86400, label: "Today" },
  { maxSecs: 3 * 86400, label: "Last 3 days" },
  { maxSecs: 7 * 86400, label: "This week" },
  { maxSecs: 14 * 86400, label: "Last 2 weeks" },
  { maxSecs: 30 * 86400, label: "This month" },
  { maxSecs: 60 * 86400, label: "Last 2 months" },
  { maxSecs: 91 * 86400, label: "Last 3 months" },
  { maxSecs: 182 * 86400, label: "Last 6 months" },
  { maxSecs: 274 * 86400, label: "Last 9 months" },
  { maxSecs: 365 * 86400, label: "This year" },
  { maxSecs: 548 * 86400, label: "Last 18 months" },
  { maxSecs: 730 * 86400, label: "Last 2 years" },
  { maxSecs: 1095 * 86400, label: "Last 3 years" },
  { maxSecs: 1461 * 86400, label: "Last 4 years" },
  { maxSecs: 1826 * 86400, label: "Last 5 years" },
  { maxSecs: 2557 * 86400, label: "Last 7 years" },
  { maxSecs: 3652 * 86400, label: "Last 10 years" },
  { maxSecs: Number.POSITIVE_INFINITY, label: "Older" },
];

/** Age bucket for a suspicious timestamp (SPEC §13). */
export const AGE_SUSPICIOUS = 31;

/**
 * Maps an age to its bucket, the way the backend fills the key's age bits.
 *
 * @param ageSecs - Seconds since last modification (negative = future).
 * @param suspicious - Whether the timestamp is implausible.
 * @returns Bucket index 1–20, or {@link AGE_SUSPICIOUS}.
 */
export function ageBucket(ageSecs: number, suspicious = false): number {
  if (suspicious || !Number.isFinite(ageSecs)) return AGE_SUSPICIOUS;
  const a = Math.max(0, ageSecs);
  for (let i = 1; i < AGE_BUCKETS.length; i++) if (a < (AGE_BUCKETS[i]?.maxSecs ?? 0)) return i;
  return AGE_BUCKETS.length - 1;
}

// -----------------------------------------------------------------------------
// Color math
// -----------------------------------------------------------------------------

/** RGB triple in 0–255. */
export type Rgb = readonly [number, number, number];

/**
 * Parses `#rrggbb`.
 *
 * @param hex - Six-digit hex color.
 * @returns RGB triple.
 */
export function hexToRgb(hex: string): Rgb {
  const n = Number.parseInt(hex.slice(1), 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

function hslToRgb(h: number, s: number, l: number): Rgb {
  const k = (n: number) => (n + h / 30) % 12;
  const a = s * Math.min(l, 1 - l);
  const f = (n: number) => l - a * Math.max(-1, Math.min(k(n) - 3, Math.min(9 - k(n), 1)));
  return [Math.round(f(0) * 255), Math.round(f(8) * 255), Math.round(f(4) * 255)];
}

/** Golden-angle hue for slot `i`, so consecutive slots never look alike. */
function slotColor(i: number, offset: number, dark: boolean): Rgb {
  const hue = (offset + i * 137.508) % 360;
  const band = i % 3;
  const l = dark ? 0.62 + band * 0.07 : 0.42 + band * 0.08;
  return hslToRgb(hue, 0.55, l);
}

/** Viridis-like ramp, perceptually ordered and color-blind safe. */
const AGE_RAMP: readonly Rgb[] = [
  [253, 231, 37],
  [181, 222, 43],
  [110, 206, 88],
  [53, 183, 121],
  [31, 158, 137],
  [38, 130, 142],
  [49, 104, 142],
  [62, 73, 137],
  [72, 40, 120],
  [68, 1, 84],
];

function ageColor(bucket: number, dark: boolean): Rgb {
  if (bucket === 0) return dark ? [94, 99, 107] : [160, 164, 171];
  if (bucket === AGE_SUSPICIOUS) return [214, 39, 40];
  const t = Math.min(1, (bucket - 1) / (AGE_BUCKETS.length - 2));
  const x = t * (AGE_RAMP.length - 1);
  const i = Math.min(AGE_RAMP.length - 2, Math.floor(x));
  const a = AGE_RAMP[i] as Rgb;
  const b = AGE_RAMP[i + 1] as Rgb;
  const u = x - i;
  const lerp = (k: 0 | 1 | 2) => Math.round(a[k] + (b[k] - a[k]) * u);
  return [lerp(0), lerp(1), lerp(2)];
}

/**
 * Color of a key in a mode, as CSS `rgb()` (for legends, the list and the
 * detail panel; the GPU uses {@link buildPaletteTexture}).
 *
 * @param mode - Color mode.
 * @param key - Packed color key.
 * @param dark - Whether the dark palette applies.
 * @returns CSS color.
 */
export function cssColorFor(mode: ColorMode, key: number, dark: boolean): string {
  const [r, g, b] = rgbFor(mode, key, dark);
  return `rgb(${r} ${g} ${b})`;
}

/** RGB of a key in a mode (static part; the recent-mode pulse is GPU-only). */
export function rgbFor(mode: ColorMode, key: number, dark: boolean): Rgb {
  const f = decodeColorKey(key);
  switch (mode) {
    case "category": {
      const c = categoryInfo(f.category);
      return hexToRgb(dark ? c.dark : c.light);
    }
    case "safety": {
      const s = SAFETY_TIERS[f.safety] ?? SAFETY_TIERS[0];
      return hexToRgb(dark ? (s?.dark ?? "#5e636b") : (s?.light ?? "#a0a4ab"));
    }
    case "age":
      return ageColor(f.age, dark);
    case "fileType":
      return f.fileType === 0 ? (dark ? [80, 84, 92] : [176, 180, 188]) : slotColor(f.fileType, 20, dark);
    case "app":
      return f.app === 0 ? (dark ? [80, 84, 92] : [176, 180, 188]) : slotColor(f.app, 200, dark);
    case "recent":
      return f.recent ? (dark ? [255, 196, 0] : [230, 120, 0]) : dark ? [70, 74, 82] : [196, 200, 206];
  }
}

/** Width of the palette texture (the widest slot space, apps). */
export const PALETTE_WIDTH = 1024;
/** Rows: 0 category, 1 safety, 2 age, 3 file type, 4 app, 5 recent. */
export const PALETTE_ROWS = 6;

/**
 * Builds the RGBA8 palette lookup texture the renderers sample with
 * `texelFetch(palette, ivec2(slot, row))`. Category texels carry the pattern
 * id in alpha.
 *
 * @param dark - Dark-theme variants.
 * @returns `PALETTE_WIDTH × PALETTE_ROWS × 4` bytes.
 */
export function buildPaletteTexture(dark: boolean): Uint8Array {
  const out = new Uint8Array(PALETTE_WIDTH * PALETTE_ROWS * 4);
  const put = (row: number, slot: number, rgb: Rgb, a = 255) => {
    const o = (row * PALETTE_WIDTH + slot) * 4;
    out[o] = rgb[0];
    out[o + 1] = rgb[1];
    out[o + 2] = rgb[2];
    out[o + 3] = a;
  };
  for (let i = 0; i < 16; i++) {
    const c = categoryInfo(i);
    put(0, i, hexToRgb(dark ? c.dark : c.light), PATTERNS.indexOf(c.pattern));
  }
  for (let i = 0; i < 8; i++) put(1, i, rgbFor("safety", encodeColorKey({ ...ZERO, safety: i }), dark));
  for (let i = 0; i < 32; i++) put(2, i, ageColor(i, dark));
  for (let i = 0; i < 256; i++) put(3, i, rgbFor("fileType", encodeColorKey({ ...ZERO, fileType: i }), dark));
  for (let i = 0; i < 1024; i++) put(4, i, rgbFor("app", encodeColorKey({ ...ZERO, app: i }), dark));
  put(5, 0, rgbFor("recent", 0, dark));
  put(5, 1, rgbFor("recent", 1 << 30, dark));
  return out;
}

const ZERO: ColorKeyFields = { category: 0, safety: 0, age: 0, fileType: 0, app: 0, recent: false };
