/**
 * File types: breakdown by extension (bars + full table) and by
 * sniffed content type. Choosing a type opens Largest files filtered to it.
 */
import { BarList } from "../components/charts";
import { LoadState, Unavailable, ViewFrame, useCapability, useLoad } from "../components/feature";
import { useFeatures } from "../features";
import { formatBytes, formatCount, formatPercent } from "../lib/format";
import { NO_LARGEST_FILTERS } from "../lib/insights";
import { useApp } from "../store/app";
import { useInsights } from "../store/insights";
import { useSettings } from "../store/settings";

const BARS = 15;

/** Display label of an extension ("" = no extension). */
export function extensionLabel(ext: string): string {
  return ext === "" ? "(no extension)" : `.${ext}`;
}

function filterBy(ext: string) {
  useInsights.getState().setLargest({ ...NO_LARGEST_FILTERS, extensions: [ext] });
  useApp.getState().setView("largest");
}

/** File types view. */
export function FileTypesView() {
  const features = useFeatures();
  const has = useCapability("insights_file_types");
  const volumeId = useApp((s) => s.volumeId) as string;
  const root = useApp((s) => s.path[s.path.length - 1] ?? null);
  const sizeMode = useApp((s) => s.sizeMode);
  const units = useSettings((s) => s.units);
  const [load, reload] = useLoad(() => features.insights.fetchFileTypes(volumeId, root, sizeMode), [features, volumeId, root, sizeMode], has);
  if (!has) {
    return (
      <ViewFrame title="File types">
        <Unavailable feature="File types" command="insights_file_types">
          Space by extension and by detected content type.
        </Unavailable>
      </ViewFrame>
    );
  }
  return (
    <ViewFrame title="File types" lead="Under the current folder. Choose a type to list its largest files.">
      <LoadState load={load} feature="File types" command="insights_file_types" onRetry={reload}>
        {(d) => {
          const rows = [...d.byExtension].sort((a, b) => b.bytes - a.bytes);
          if (rows.length === 0) {
            return (
              <div className="state state--quiet">
                <h2>No files here</h2>
              </div>
            );
          }
          return (
            <>
              <BarList
                label="Largest file types"
                selectHint="List largest files of type"
                bars={rows.slice(0, BARS).map((r) => ({
                  key: r.extension,
                  label: extensionLabel(r.extension),
                  value: r.bytes,
                  valueText: `${formatBytes(r.bytes, { units })} · ${formatPercent(d.totalBytes === 0 ? 0 : r.bytes / d.totalBytes)}`,
                }))}
                onSelect={filterBy}
              />
              <table className="table">
                <caption>All extensions</caption>
                <thead>
                  <tr>
                    <th scope="col">Extension</th>
                    <th scope="col">Group</th>
                    <th scope="col" className="num">
                      Files
                    </th>
                    <th scope="col" className="num">
                      Size
                    </th>
                    <th scope="col" className="num">
                      Content differs
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((r) => (
                    <tr key={r.extension}>
                      <th scope="row">
                        <button
                          type="button"
                          className="linkish"
                          onClick={() => {
                            filterBy(r.extension);
                          }}
                        >
                          {extensionLabel(r.extension)}
                        </button>
                      </th>
                      <td>{r.group}</td>
                      <td className="num">{formatCount(r.files)}</td>
                      <td className="num">{formatBytes(r.bytes, { units })}</td>
                      <td className="num">{r.mismatched > 0 ? formatCount(r.mismatched) : "—"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
              <h2 className="section-title">By detected content</h2>
              {d.byDetectedType.length === 0 ? (
                <p className="detail__muted">No files have been content-sniffed yet (large files are checked in the background).</p>
              ) : (
                <table className="table">
                  <caption className="visually-hidden">By detected content type</caption>
                  <thead>
                    <tr>
                      <th scope="col">Detected type</th>
                      <th scope="col" className="num">
                        Files
                      </th>
                      <th scope="col" className="num">
                        Size
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {d.byDetectedType.map((r) => (
                      <tr key={r.label}>
                        <th scope="row">{r.label}</th>
                        <td className="num">{formatCount(r.files)}</td>
                        <td className="num">{formatBytes(r.bytes, { units })}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              )}
            </>
          );
        }}
      </LoadState>
    </ViewFrame>
  );
}
