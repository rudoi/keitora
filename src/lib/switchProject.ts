// Shared, optimistic project-switch path behind App.tsx's `handleSwitchProject`,
// which every project switcher in the UI (and the keyboard cycle) goes through.
//
// Why this lives in its own file (not `ipc.ts`): it needs the React `setProject`
// state setter so it can flip the App-level `project` state immediately. It is a
// plain async function, NOT a hook — the caller passes `setProject`, the current
// project value, and a `showToast` callback as parameters. That keeps it usable
// from any callback.
//
// Mechanism: when the caller already holds the target `ProjectSummary`, flip the
// UI optimistically before the backend round-trip. `set_active_project` writes
// the user registry and emits `project://changed` itself, which drives
// `loadProject()` → `setProject` again with the reconciled value (same data — no
// flicker). On error, revert.

import { type ProjectSummary, setActiveProject } from "./ipc";

type ProjectState = ProjectSummary | null | "loading";

/**
 * What a switch to registry key `projectKey` targets. `summary` is the caller's, else the
 * registry entry for the key; `resolvedName` is its `[project].name` (the key itself when
 * the project is unknown), which is what App's active project, the warm registry and the
 * switch overlay are keyed by. `optimistic` is the summary to flip App's `project` state
 * to: App holds the resolved name in `name`, the same as `current_project` returns.
 */
export function switchTarget(
  projectKey: string,
  summary: ProjectSummary | undefined,
  registry: readonly ProjectSummary[],
): {
  summary: ProjectSummary | undefined;
  resolvedName: string;
  optimistic: ProjectSummary | undefined;
} {
  const s = summary ?? registry.find((p) => p.name === projectKey);
  return {
    summary: s,
    resolvedName: s?.resolved_name ?? projectKey,
    optimistic: s && { ...s, name: s.resolved_name },
  };
}

export async function switchProject(
  name: string,
  opts: {
    setProject: React.Dispatch<React.SetStateAction<ProjectState>>;
    currentProject: ProjectState;
    showToast: (msg: string) => void;
    summary?: ProjectSummary;
    /** Skip the "Switched to …" toast (the error toast still shows). */
    silent?: boolean;
  },
): Promise<boolean> {
  const { setProject, currentProject, showToast, summary, silent } = opts;

  // 1. Optimistic flip: if the caller has the summary, update the UI now.
  if (summary) {
    setProject(summary);
  }

  try {
    // 2. Backend round-trip. set_active_project writes the user registry and emits
    //    project://changed → loadProject(). That reconciliation sets project state
    //    again to the same value — no flicker.
    //    The backend also restarts the tasks/sessions watcher on the new DB, so
    //    tasks://changed and sessions://changed begin firing for the new project.
    await setActiveProject(name);
    if (!silent) showToast(`Switched to ${name}.`);
    return true;
  } catch (e) {
    // 3. Error: revert the optimistic update so the UI matches the backend. Returns
    //    false so the caller can also revert any of its own optimistic state (e.g. the
    //    warm-active visibility flip, which this function doesn't own).
    if (summary) {
      setProject(currentProject);
    }
    showToast(String(e));
    return false;
  }
}
