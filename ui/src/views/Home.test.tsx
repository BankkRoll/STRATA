import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { Home } from "./Home";

describe("Home", () => {
  it("renders the empty state heading", () => {
    render(<Home info={null} />);
    expect(screen.getByRole("heading", { level: 1 }).textContent).toBe(
      "See everything on your drives",
    );
  });

  it("shows version and Windows build when known", () => {
    render(<Home info={{ version: "1.2.3", windowsBuild: 22631, backdrop: "mica" }} />);
    expect(screen.getByText("Strata 1.2.3")).toBeTruthy();
    expect(screen.getByText("Windows build 22631")).toBeTruthy();
  });
});
