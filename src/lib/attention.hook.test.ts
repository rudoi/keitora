// useAttention driven through a minimal hook runner: React's hooks are replaced by slot
// storage that persists across "renders" and runs effects synchronously, so the hook's
// latch, badge and notification paths run without a DOM.
import { beforeEach, describe, expect, it, vi } from "vitest";

const h = vi.hoisted(() => {
  const slots: { current?: unknown; value?: unknown }[] = [];
  let cursor = 0;
  return {
    slots,
    badges: [] as (number | undefined)[],
    notified: [] as string[],
    reset() {
      slots.length = 0;
      cursor = 0;
      this.badges.length = 0;
      this.notified.length = 0;
    },
    begin() {
      cursor = 0;
    },
    slot<T extends object>(init: () => T): T {
      const k = cursor++;
      if (slots[k] === undefined) slots[k] = init();
      return slots[k] as T;
    },
  };
});

vi.mock("react", () => ({
  useRef: (init: unknown) => h.slot(() => ({ current: init })),
  useState: (init: unknown) => {
    const box = h.slot(() => ({ value: init }));
    const set = (u: unknown) => {
      box.value = typeof u === "function" ? (u as (p: unknown) => unknown)(box.value) : u;
    };
    return [box.value, set];
  },
  useEffect: (fn: () => void) => {
    h.slot(() => ({}));
    fn();
  },
}));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    setBadgeCount: (n: number | undefined) => {
      h.badges.push(n);
      return Promise.resolve();
    },
  }),
}));

vi.mock("@tauri-apps/plugin-notification", () => ({
  isPermissionGranted: async () => true,
  requestPermission: async () => "granted",
}));

vi.mock("./ipc", () => ({
  notifySession: async (target: { worktree: string }) => {
    h.notified.push(target.worktree);
  },
}));

import { type AttentionKind, useAttention } from "./attention";
import type { SessionState, WorktreeSession } from "./sessions";

function ws(state: SessionState, extra: Partial<WorktreeSession> = {}): WorktreeSession {
  return {
    project: "proj",
    worktree: "bravo",
    state,
    current_task_id: null,
    pid: null,
    last_heartbeat: "",
    ...extra,
  };
}

/** One render of the hook over `sessions` (board view: nothing is focused). */
function render(...sessions: WorktreeSession[]): Map<string, AttentionKind> {
  h.begin();
  // biome-ignore lint/correctness/useHookAtTopLevel: React's hooks are mocked by the runner above; each call is one render
  return useAttention(
    new Map(sessions.map((s) => [s.worktree, s])),
    null,
    "board",
    null,
    false,
    () => {},
    "proj",
  );
}

const flush = () => new Promise((r) => setTimeout(r, 0));

describe("useAttention latch guard", () => {
  beforeEach(() => h.reset());

  it("a rowless idle agent shown twice needs no input: no badge, no notification", async () => {
    // The engine synthesizes this row from an agent present in herdr with no hook row.
    const idle = ws("idle", { agent: "present", herdr_status: "idle" });
    render(idle);
    render(idle);
    await flush();
    expect(h.notified).toEqual([]);
    expect(h.badges.every((b) => b === undefined)).toBe(true);
    expect(h.slots.some((s) => s.value instanceof Map && s.value.size > 0)).toBe(false);
  });

  it("an offline -> idle lift needs no input", async () => {
    render(ws("offline", { agent: "absent" }));
    render(ws("idle", { agent: "present" }));
    await flush();
    expect(h.notified).toEqual([]);
    expect(h.badges.every((b) => b === undefined)).toBe(true);
  });

  it("a launch that becomes ready is not a finished turn", async () => {
    const starting = ws("busy", { agent: "starting" });
    render(starting);
    render(starting);
    render(ws("idle", { agent: "present" }));
    await flush();
    expect(h.notified).toEqual([]);
    expect(h.badges.every((b) => b === undefined)).toBe(true);
  });

  it("a real busy -> idle of a started agent notifies once and badges", async () => {
    render(ws("busy", { agent: "starting" }));
    render(ws("busy", { agent: "present" }));
    render(ws("idle", { agent: "present" }));
    // A render returns what the previous render committed.
    const shown = render(ws("idle", { agent: "present" }));
    await flush();
    expect(h.notified).toEqual(["bravo"]);
    expect(h.badges[h.badges.length - 1]).toBe(1);
    expect(shown.get("bravo")).toBe("finished");
  });

  it("a stale fallback row between fresh waiting views causes no second notification", async () => {
    render(ws("busy", { agent: "present" }));
    render(ws("waiting", { agent: "present" }));
    render(ws("busy", { agent: "present", stale: true }));
    render(ws("waiting", { agent: "present" }));
    await flush();
    expect(h.notified).toEqual(["bravo"]);
  });
});
