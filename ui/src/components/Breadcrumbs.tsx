/**
 * Breadcrumb bar: the path from the volume root to the current layout root.
 * Every ancestor is a button that jumps there (SPEC §16.1).
 */
import { useEntryInfo } from "../lib/hooks";
import type { EntryInfoProvider } from "../lib/entries";
import { volumeName } from "../lib/volumes";
import { useApp } from "../store/app";
import { useVolumes } from "../store/volumes";
import { useServices } from "../services";
import { Icon } from "./icons";

function Crumb({ provider, id, index, last, rootLabel }: { provider: EntryInfoProvider; id: number; index: number; last: boolean; rootLabel: string }) {
  const info = useEntryInfo(provider, index === 0 ? null : id);
  const jumpTo = useApp((s) => s.jumpTo);
  const label = index === 0 ? rootLabel : (info?.name ?? "…");
  return (
    <li className="crumbs__item">
      {index > 0 && <Icon name="chevron" size={12} className="crumbs__sep" />}
      {last ? (
        <span className="crumbs__current" aria-current="page">
          {label}
        </span>
      ) : (
        <button
          type="button"
          className="crumbs__link"
          onClick={() => {
            jumpTo(index);
          }}
        >
          {label}
        </button>
      )}
    </li>
  );
}

/** The breadcrumb navigation for the current root. */
export function Breadcrumbs() {
  const services = useServices();
  const volumeId = useApp((s) => s.volumeId);
  const path = useApp((s) => s.path);
  const volumes = useVolumes((s) => s.volumes);
  if (!volumeId || path.length === 0) {
    return (
      <nav className="crumbs" aria-label="Breadcrumb">
        <ol>
          <li className="crumbs__item">
            <span className="crumbs__current" aria-current="page">
              Volumes
            </span>
          </li>
        </ol>
      </nav>
    );
  }
  const vol = volumes?.find((v) => v.id === volumeId);
  const provider = services.entryInfo(volumeId);
  return (
    <nav className="crumbs" aria-label="Breadcrumb">
      <ol>
        {path.map((id, i) => (
          <Crumb
            key={`${i}:${id}`}
            provider={provider}
            id={id}
            index={i}
            last={i === path.length - 1}
            rootLabel={vol ? volumeName(vol) : "Volume"}
          />
        ))}
      </ol>
    </nav>
  );
}
