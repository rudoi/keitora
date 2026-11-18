import { DndContext } from "@dnd-kit/core";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import type { CardEnv } from "./cardHelpers";
import { LaneLayout } from "./LaneLayout";
import type { Lane } from "./useBoardLanes";

function mkLane(key: string): Lane {
  return { key, kind: "worktree", label: key, tasks: [], count: 0, isEmptyOffline: false };
}

const noop = () => {};
const env: CardEnv = {
  states: new Map(),
  pool: new Set(["alpha", "bravo", "charlie"]),
  laneCtx: { sessions: new Map(), tasks: [], sent: new Set() },
  sessions: new Map(),
  actor: null,
  selectedId: null,
  worktreeNames: ["alpha", "bravo", "charlie"],
  flyIn: null,
  menuTaskId: null,
  onMenuOpenChange: noop,
  onSelect: noop,
  onAction: noop,
  onDispatch: noop,
  onAutoDispatch: noop,
  onRelease: noop,
  onBacklog: noop,
  onReopen: noop,
};

/** Names of the worktree rows marked current, in render order. */
function currentRows(currentWorktree: string | null): string[] {
  const html = renderToStaticMarkup(
    <DndContext>
      <LaneLayout
        lanes={["alpha", "bravo", "charlie"].map(mkLane)}
        decisions={new Map()}
        dragging={false}
        env={env}
        currentWorktree={currentWorktree}
        showDone={false}
        backlog={{ shown: false, count: 0, onToggle: noop }}
      />
    </DndContext>,
  );
  // Each row opens with its own div, so the first name after a marker is that row's.
  return html
    .split('aria-current="true"')
    .slice(1)
    .map((rest) => /font-semibold[^"]*">([^<]*)</.exec(rest)?.[1] ?? "");
}

describe("LaneLayout current worktree", () => {
  it("marks only the current worktree's row", () => {
    expect(currentRows("bravo")).toEqual(["bravo"]);
  });

  it("marks no row when there is no current worktree or it has no lane", () => {
    expect(currentRows(null)).toEqual([]);
    expect(currentRows("other")).toEqual([]);
  });
});
