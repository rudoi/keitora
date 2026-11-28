// A small `!` badge for the health monitor's warn/error findings: amber for `warn`,
// red for `error`, nothing for `info` or none. On a lane it is a button that opens
// Settings at the Health section; in a project header (itself a button) it only shows.

import { glyphLabel, glyphView, requestHealthSettings } from "../../lib/health";
import type { HealthFinding } from "../../lib/ipc";

export function HealthGlyph({
  findings,
  scope,
}: {
  findings: readonly HealthFinding[];
  scope: "lane" | "project";
}) {
  const view = glyphView(findings);
  if (view === null) return null;
  const cls = `health-glyph ${view.tone}`;
  if (scope === "project") {
    return (
      <span className={cls} role="img" aria-label={glyphLabel(view.count)} title={view.title}>
        !
      </span>
    );
  }
  return (
    <button
      type="button"
      className={cls}
      aria-label={glyphLabel(view.count)}
      title={view.title}
      onClick={(e) => {
        e.stopPropagation();
        requestHealthSettings();
      }}
    >
      !
    </button>
  );
}
