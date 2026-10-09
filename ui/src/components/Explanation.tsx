/**
 * Renders a classifier explanation (`rules_explain`): result, deciding rule,
 * inheritance origin and the evaluation trace. Used by the settings path
 * tester and the detail panel's "Why?".
 */
import type { Explanation } from "../lib/settings";
import { categoryInfo } from "../lib/palette";
import { SafetyBadge } from "./feature";

/** Explanation of one path. */
export function ExplanationView({ e }: { e: Explanation }) {
  return (
    <div className="explain" aria-live="polite">
      <p>
        <strong>{categoryInfo(e.result.category).label}</strong> · <SafetyBadge tier={e.result.safety} />
        {e.result.regenerable && <span className="badge">regenerable</span>}
      </p>
      {e.rule ? (
        <p>
          Rule <code>{e.rule.id}</code> ({e.rule.source === "builtin" ? "built-in" : "yours"}, pack {e.rule.pack}): {e.rule.explain}
        </p>
      ) : (
        <p>No rule matched; Strata treats it as user data.</p>
      )}
      {e.originPath && (
        <p className="detail__muted">
          Inherited from <code>{e.originPath}</code>
        </p>
      )}
      {e.trace.length > 0 && (
        <details>
          <summary>How the rules were evaluated</summary>
          <ol className="explain__trace">
            {e.trace.map((t, i) => (
              <li key={i}>{t}</li>
            ))}
          </ol>
        </details>
      )}
    </div>
  );
}
