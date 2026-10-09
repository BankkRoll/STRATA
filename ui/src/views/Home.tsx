/**
 * Home screen: the volumes overview.
 *
 * Volume discovery lands in M4; until a volume list exists this renders the
 * designed first-run empty state.
 */
import type { AppInfo } from "../lib/backend";

/** Props for {@link Home}. */
export interface HomeProps {
  /** Backend app info, or `null` while it is still loading. */
  info: AppInfo | null;
}

/** Volumes overview / first-run empty state. */
export function Home({ info }: HomeProps) {
  return (
    <main className="home" aria-labelledby="home-title">
      <section className="empty-state">
        <svg className="empty-state__mark" viewBox="0 0 64 64" aria-hidden="true">
          <rect x="6" y="10" width="52" height="10" rx="3" />
          <rect x="6" y="27" width="34" height="10" rx="3" />
          <rect x="6" y="44" width="20" height="10" rx="3" />
        </svg>
        <h1 id="home-title">See everything on your drives</h1>
        <p>
          Strata maps what is using your disk, explains what each item is and which app put it
          there, and helps you clean up safely.
        </p>
        <p className="empty-state__hint">Drive scanning arrives in the next build.</p>
      </section>
      <footer className="status-bar" aria-label="App status">
        <span>Strata {info?.version ?? ""}</span>
        {info && info.windowsBuild > 0 && <span>Windows build {info.windowsBuild}</span>}
      </footer>
    </main>
  );
}
