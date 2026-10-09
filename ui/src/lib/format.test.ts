// @vitest-environment node
import { describe, expect, it } from "vitest";
import {
  EPOCH_2000_MS,
  describeTimestamp,
  epoch2000ToMs,
  formatBytes,
  formatBytesExact,
  formatCount,
  formatDate,
  formatDateTime,
  formatPercent,
  formatRelative,
  isSuspiciousTimestamp,
} from "./format";

const en = { locale: "en-US" };

describe("formatBytes", () => {
  it.each([
    [0, "0 B"],
    [1, "1 B"],
    [1023, "1,023 B"],
    [1024, "1.00 KB"],
    [1536, "1.50 KB"],
    [10 * 1024, "10.0 KB"],
    [100 * 1024, "100 KB"],
    [1023.7 * 1024, "1.00 MB"],
    [9.996 * 1024 ** 3, "10.0 GB"],
    [1.45 * 1024 ** 3, "1.45 GB"],
    [3 * 1024 ** 4, "3.00 TB"],
    [2 ** 60, "1.00 EB"],
  ])("binary %d → %s", (bytes, text) => {
    expect(formatBytes(bytes, en)).toBe(text);
  });

  it("uses SI units with kB", () => {
    expect(formatBytes(1500, { ...en, units: "si" })).toBe("1.50 kB");
    expect(formatBytes(1_500_000, { ...en, units: "si" })).toBe("1.50 MB");
    expect(formatBytes(999, { ...en, units: "si" })).toBe("999 B");
  });

  it("formats negative deltas and rejects non-finite input", () => {
    expect(formatBytes(-2048, en)).toBe("-2.00 KB");
    expect(formatBytes(Number.NaN, en)).toBe("—");
    expect(formatBytes(Number.POSITIVE_INFINITY, en)).toBe("—");
  });

  it("follows the locale's separators", () => {
    expect(formatBytes(1.5 * 1024 ** 3, { locale: "de-DE" })).toBe("1,50 GB");
    expect(formatBytes(1000, { locale: "de-DE" })).toBe("1.000 B");
  });
});

describe("counts and percentages", () => {
  it("groups counts per locale", () => {
    expect(formatCount(1234567, en)).toBe("1,234,567");
    expect(formatCount(1234567, { locale: "de-DE" })).toBe("1.234.567");
    expect(formatBytesExact(1, en)).toBe("1 byte");
    expect(formatBytesExact(4096, en)).toBe("4,096 bytes");
  });

  it("never shows a non-zero share as 0%", () => {
    expect(formatPercent(0, en)).toBe("0.0%");
    expect(formatPercent(0.00001, en)).toBe("<0.1%");
    expect(formatPercent(0.1234, en)).toBe("12.3%");
    expect(formatPercent(2, en)).toBe("100.0%");
    expect(formatPercent(Number.NaN, en)).toBe("—");
  });
});

describe("timestamps", () => {
  const now = Date.UTC(2026, 9, 9, 12, 0, 0);

  it("converts the index epoch", () => {
    expect(epoch2000ToMs(0)).toBeNull();
    expect(epoch2000ToMs(86400)).toBe(EPOCH_2000_MS + 86_400_000);
  });

  it("flags pre-1990 and future timestamps", () => {
    expect(isSuspiciousTimestamp(Date.UTC(1989, 11, 31), now)).toBe(true);
    expect(isSuspiciousTimestamp(Date.UTC(1990, 0, 2), now)).toBe(false);
    expect(isSuspiciousTimestamp(now + 2 * 86_400_000, now)).toBe(true);
    expect(isSuspiciousTimestamp(now + 3_600_000, now)).toBe(false);
    expect(isSuspiciousTimestamp(Number.NaN, now)).toBe(true);
  });

  it("formats absolute times in the given zone (UTC stored, local shown)", () => {
    const t = Date.UTC(2025, 2, 4, 13, 5);
    expect(formatDateTime(t, { ...en, timeZone: "UTC" })).toBe("Mar 4, 2025, 1:05 PM");
    expect(formatDateTime(t, { ...en, timeZone: "Asia/Tokyo" })).toBe("Mar 4, 2025, 10:05 PM");
    expect(formatDate(t, { ...en, timeZone: "UTC" })).toBe("Mar 4, 2025");
    expect(formatDateTime(null, en)).toBe("Unknown");
  });

  it("handles DST transitions in local display", () => {
    // 2025-03-09 is the US spring-forward day: 06:59Z is 1:59 EST, 07:00Z is 3:00 EDT.
    expect(formatDateTime(Date.UTC(2025, 2, 9, 6, 59), { ...en, timeZone: "America/New_York" })).toBe("Mar 9, 2025, 1:59 AM");
    expect(formatDateTime(Date.UTC(2025, 2, 9, 7, 0), { ...en, timeZone: "America/New_York" })).toBe("Mar 9, 2025, 3:00 AM");
  });

  it("formats relative times", () => {
    expect(formatRelative(now - 30_000, now, en)).toBe("30 seconds ago");
    expect(formatRelative(now - 3 * 86_400_000, now, en)).toBe("3 days ago");
    expect(formatRelative(now - 86_400_000, now, en)).toBe("yesterday");
    expect(formatRelative(now + 2 * 3_600_000, now, en)).toBe("in 2 hours");
    expect(formatRelative(now - 400 * 86_400_000, now, en)).toBe("last year");
    expect(formatRelative(null, now, en)).toBe("Unknown");
  });

  it("describes timestamps with the suspicious flag", () => {
    const d = describeTimestamp(Date.UTC(1980, 0, 1), now, { ...en, timeZone: "UTC" });
    expect(d.suspicious).toBe(true);
    expect(d.absolute).toBe("Jan 1, 1980, 12:00 AM");
    expect(describeTimestamp(null, now).suspicious).toBe(false);
  });
});
