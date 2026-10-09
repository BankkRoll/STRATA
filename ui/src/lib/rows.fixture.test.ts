// @vitest-environment node
/// <reference types="node" />
/**
 * Decodes the row page written by the backend's encoder
 * (`src-tauri/src/rows.rs`, test `page_matches_shared_ui_fixture`), so the
 * Rust encoder and {@link decodeRowPage} are checked against the same bytes.
 */
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { epoch2000ToMs } from "./format";
import { EntryFlag, decodeRowPage } from "./rows";

describe("backend row page fixture", () => {
  it("decodes every field", () => {
    const page = decodeRowPage(new Uint8Array(readFileSync(join(process.cwd(), "src/lib/__fixtures__/rowpage.bin"))));
    expect(page.parent).toBe(0x4000_0001);
    expect(page.total).toBe(12);
    expect(page.offset).toBe(10);
    expect(page.rows).toHaveLength(2);
    const [dir, file] = page.rows;
    expect(dir).toEqual({
      id: 0x4000_0010,
      parent: 0x4000_0001,
      name: "node_modules",
      flags: EntryFlag.DIR | EntryFlag.PARTIAL,
      isDir: true,
      category: 5,
      safety: "safe",
      allocated: 3 * 2 ** 40,
      logical: 123_456_789,
      items: 42,
      childCount: 7,
      modifiedMs: epoch2000ToMs(800_000_000),
      createdMs: epoch2000ToMs(700_000_000),
      accessedMs: null,
      appId: 3,
    });
    expect(file?.name).toBe("é�!");
    expect(file?.safety).toBeNull();
    expect(file?.flags).toBe(EntryFlag.HIDDEN);
    expect(file?.isDir).toBe(false);
  });
});
