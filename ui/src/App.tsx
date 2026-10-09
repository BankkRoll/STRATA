import { useEffect, useState } from "react";
import { getAppInfo, type AppInfo } from "./lib/backend";
import { applyBackdrop, applyTheme } from "./lib/theme";
import { useSettings } from "./store/settings";
import { Home } from "./views/Home";

/** Root component: wires theme/backdrop to the document and renders the current view. */
export function App() {
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

  return <Home info={info} />;
}
