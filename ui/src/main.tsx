import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { createTauriServices } from "./services";
import "./styles.css";

const root = document.getElementById("root");
if (!root) throw new Error("#root missing from index.html");

async function boot(el: HTMLElement): Promise<void> {
  // NOTE: `import.meta.env.DEV` is a compile-time constant, so production
  // builds drop this branch and the fixture harness chunk entirely.
  if (import.meta.env.DEV && new URLSearchParams(location.search).has("fixture")) {
    const { mountFixtureHarness } = await import("./dev/FixtureHarness");
    mountFixtureHarness(el);
    return;
  }
  const { services } = createTauriServices();
  createRoot(el).render(
    <StrictMode>
      <App services={services} />
    </StrictMode>,
  );
}

void boot(root);
