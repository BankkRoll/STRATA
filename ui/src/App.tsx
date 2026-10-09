import { useEffect, useState } from "react";
import { AppShell } from "./components/AppShell";
import { getAppInfo, type AppInfo } from "./lib/backend";
import { applyBackdrop, applyTheme } from "./lib/theme";
import { ServicesContext, type Services } from "./services";
import { useSettings } from "./store/settings";

/** Props for {@link App}. */
export interface AppProps {
  /** Backend services (Tauri in the app, fixtures in the dev harness). */
  services: Services;
}

/** Root component: wires theme/backdrop to the document and renders the shell. */
export function App({ services }: AppProps) {
  const theme = useSettings((s) => s.theme);
  const [info, setInfo] = useState<AppInfo | null>(null);

  useEffect(() => {
    applyTheme(document.documentElement, theme);
  }, [theme]);

  useEffect(() => {
    let cancelled = false;
    getAppInfo()
      .then((next) => {
        if (cancelled) return;
        applyBackdrop(document.documentElement, next.backdrop);
        setInfo(next);
      })
      .catch((err: unknown) => {
        console.error("app_info failed", err);
        applyBackdrop(document.documentElement, "solid");
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <ServicesContext value={services}>
      <AppShell info={info} />
    </ServicesContext>
  );
}
