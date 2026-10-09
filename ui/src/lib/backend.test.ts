import { describe, expect, it } from "vitest";
import { asCommandFailure, errorMessage } from "./backend";

describe("backend command failures", () => {
  it("recognizes the backend's { code, message } errors", () => {
    expect(asCommandFailure({ code: "not_found", message: "gone" })).toEqual({ code: "not_found", message: "gone" });
    expect(asCommandFailure("plain")).toBeNull();
    expect(asCommandFailure({ code: 1, message: "x" })).toBeNull();
    expect(asCommandFailure(null)).toBeNull();
  });

  it("shows the backend's message", () => {
    expect(errorMessage({ code: "io", message: "Explorer could not show it" })).toBe("Explorer could not show it");
  });
});
