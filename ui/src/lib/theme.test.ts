import { describe, expect, it } from "vitest";
import { applyBackdrop, applyTheme } from "./theme";

describe("applyTheme", () => {
  it("sets an explicit theme and clears it for system", () => {
    const root = document.createElement("html");
    applyTheme(root, "dark");
    expect(root.getAttribute("data-theme")).toBe("dark");
    applyTheme(root, "system");
    expect(root.hasAttribute("data-theme")).toBe(false);
  });
});

describe("applyBackdrop", () => {
  it("records the backdrop", () => {
    const root = document.createElement("html");
    applyBackdrop(root, "mica");
    expect(root.getAttribute("data-backdrop")).toBe("mica");
  });
});
