import type { UnlistenFn } from "@tauri-apps/api/event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  DEFAULT_RESYNC_MS,
  listenAlso,
  newestSettledGate,
  type SubscribeResult,
  subscribeLive,
} from "./useLiveResource";

// A controllable fake `listen`: it records the handler so a test can fire events,
// and lets the test decide when the listen() promise resolves (so we can assert the
// listen-then-load ordering deterministically).
function makeFakeListen() {
  let handler: ((event: { payload: unknown }) => void) | undefined;
  let resolveListen: ((u: UnlistenFn) => void) | undefined;
  let rejectListen: ((e: unknown) => void) | undefined;
  let attached = false;
  // Models Tauri: after unlisten, the handler no longer receives events.
  const unlisten = vi.fn(() => {
    attached = false;
  });

  const listen = vi.fn((_event: string, h: (event: { payload: unknown }) => void) => {
    handler = h;
    attached = true;
    return new Promise<UnlistenFn>((resolve, reject) => {
      resolveListen = resolve;
      rejectListen = reject;
    });
  }) as unknown as typeof import("@tauri-apps/api/event").listen;

  return {
    listen,
    unlisten,
    // Deliver a change event. `payload` defaults to `null` (a unit payload, like
    // worktrees://changed) so revision-less firing exercises the always-run path;
    // pass `{ revision }` to drive the revision gate.
    fire: (payload: unknown = null) => {
      if (attached) handler?.({ payload });
    },
    isAttached: () => attached,
    /** Resolve the listen() promise (attach completes). */
    settle: async () => {
      resolveListen?.(unlisten as unknown as UnlistenFn);
      await Promise.resolve();
      await Promise.resolve();
    },
    /** Reject the listen() promise (attach failed). */
    failAttach: async (e: unknown) => {
      attached = false;
      rejectListen?.(e);
      await Promise.resolve();
      await Promise.resolve();
    },
  };
}

describe("subscribeLive", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("attaches the listener before the initial load (no miss window)", async () => {
    const fake = makeFakeListen();
    const load = vi.fn().mockResolvedValue("v");
    const results: SubscribeResult<unknown>[] = [];

    subscribeLive(load, "ev", (r) => results.push(r), { listen: fake.listen });

    // Listener is registered synchronously on subscribe; the initial load has NOT
    // run yet because it is chained after listen() resolves.
    expect(fake.isAttached()).toBe(true);
    expect(load).not.toHaveBeenCalled();

    await fake.settle();

    // Initial load runs only after attach completed.
    expect(load).toHaveBeenCalledTimes(1);
    expect(results).toEqual([{ data: "v" }]);
  });

  it("reloads when the change event fires", async () => {
    const fake = makeFakeListen();
    const load = vi.fn().mockResolvedValue("v");
    const results: SubscribeResult<unknown>[] = [];

    subscribeLive(load, "ev", (r) => results.push(r), { listen: fake.listen });
    await fake.settle();
    expect(load).toHaveBeenCalledTimes(1);

    fake.fire();
    await Promise.resolve();
    await Promise.resolve();

    expect(load).toHaveBeenCalledTimes(2);
    expect(results[results.length - 1]).toEqual({ data: "v" });
  });

  it("re-syncs on the default 30s poll interval", async () => {
    const fake = makeFakeListen();
    const load = vi.fn().mockResolvedValue("v");

    subscribeLive(load, "ev", () => {}, { listen: fake.listen });
    await fake.settle();
    expect(load).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(DEFAULT_RESYNC_MS);
    expect(load).toHaveBeenCalledTimes(2);

    await vi.advanceTimersByTimeAsync(DEFAULT_RESYNC_MS);
    expect(load).toHaveBeenCalledTimes(3);
  });

  it("disables polling when intervalMs <= 0", async () => {
    const fake = makeFakeListen();
    const load = vi.fn().mockResolvedValue("v");

    subscribeLive(load, "ev", () => {}, { listen: fake.listen, intervalMs: 0 });
    await fake.settle();
    expect(load).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(DEFAULT_RESYNC_MS * 3);
    expect(load).toHaveBeenCalledTimes(1); // only the initial load
  });

  it("with intervalMs 0 only events reload, however long it runs", async () => {
    const fake = makeFakeListen();
    const load = vi.fn().mockResolvedValue("v");

    subscribeLive(load, "daemon://stats", () => {}, { listen: fake.listen, intervalMs: 0 });
    await fake.settle();
    expect(load).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(5 * 60_000);
    expect(load).toHaveBeenCalledTimes(1); // no timer

    fake.fire();
    await Promise.resolve();
    expect(load).toHaveBeenCalledTimes(2); // exactly one reload per event
  });

  it("teardown detaches the listener and stops polling", async () => {
    const fake = makeFakeListen();
    const load = vi.fn().mockResolvedValue("v");
    const results: SubscribeResult<unknown>[] = [];

    const teardown = subscribeLive(load, "ev", (r) => results.push(r), { listen: fake.listen });
    await fake.settle();
    expect(load).toHaveBeenCalledTimes(1);

    teardown();
    expect(fake.unlisten).toHaveBeenCalledTimes(1);

    // No further loads from polling...
    await vi.advanceTimersByTimeAsync(DEFAULT_RESYNC_MS * 2);
    // ...and a late event after teardown is a no-op on results.
    fake.fire();
    await Promise.resolve();
    await Promise.resolve();

    expect(load).toHaveBeenCalledTimes(1);
    expect(results).toEqual([{ data: "v" }]);
  });

  it("detaches the listener if unmounted before listen() resolves", async () => {
    const fake = makeFakeListen();
    const load = vi.fn().mockResolvedValue("v");

    const teardown = subscribeLive(load, "ev", () => {}, { listen: fake.listen });
    // Tear down while listen() is still pending.
    teardown();
    await fake.settle();

    // The resolved unlisten must still be invoked, and no load should have run.
    expect(fake.unlisten).toHaveBeenCalledTimes(1);
    expect(load).not.toHaveBeenCalled();
  });

  it("surfaces a listen() failure but still runs the initial load", async () => {
    const fake = makeFakeListen();
    const load = vi.fn().mockResolvedValue("v");
    const results: SubscribeResult<unknown>[] = [];

    subscribeLive(load, "ev", (r) => results.push(r), { listen: fake.listen });
    await fake.failAttach(new Error("ipc down"));

    // The listen error is reported, and the initial load still runs (view isn't
    // left permanently empty).
    expect(results).toContainEqual({ error: "Error: ipc down" });
    expect(load).toHaveBeenCalledTimes(1);
    expect(results[results.length - 1]).toEqual({ data: "v" });
  });

  it("reports errors through onResult", async () => {
    const fake = makeFakeListen();
    const load = vi.fn().mockRejectedValue(new Error("boom"));
    const results: SubscribeResult<unknown>[] = [];

    subscribeLive(load, "ev", (r) => results.push(r), { listen: fake.listen });
    await fake.settle();

    expect(results).toEqual([{ error: "Error: boom" }]);
  });

  // ── Revision-gated event reloads (§A) ──────────────────────────────────────
  describe("revision-gated events", () => {
    it("skips a reload when the event revision is unchanged", async () => {
      const fake = makeFakeListen();
      const load = vi.fn().mockResolvedValue("v");

      subscribeLive(load, "ev", () => {}, { listen: fake.listen });
      await fake.settle();
      expect(load).toHaveBeenCalledTimes(1); // initial load

      fake.fire({ revision: "r1" });
      await Promise.resolve();
      await Promise.resolve();
      expect(load).toHaveBeenCalledTimes(2); // new revision → reload

      fake.fire({ revision: "r1" }); // same revision → gated
      await Promise.resolve();
      await Promise.resolve();
      expect(load).toHaveBeenCalledTimes(2); // no extra reload
    });

    it("reloads when the event revision changes", async () => {
      const fake = makeFakeListen();
      const load = vi.fn().mockResolvedValue("v");

      subscribeLive(load, "ev", () => {}, { listen: fake.listen });
      await fake.settle();

      fake.fire({ revision: "r1" });
      await Promise.resolve();
      await Promise.resolve();
      fake.fire({ revision: "r2" });
      await Promise.resolve();
      await Promise.resolve();

      expect(load).toHaveBeenCalledTimes(3); // initial + r1 + r2
    });

    it("always reloads a unit payload (no revision → worktrees path)", async () => {
      const fake = makeFakeListen();
      const load = vi.fn().mockResolvedValue("v");

      subscribeLive(load, "ev", () => {}, { listen: fake.listen });
      await fake.settle();

      fake.fire(); // null payload, no revision
      await Promise.resolve();
      await Promise.resolve();
      fake.fire(); // fires again — never gated
      await Promise.resolve();
      await Promise.resolve();

      expect(load).toHaveBeenCalledTimes(3); // initial + 2 unconditional reloads
    });
  });

  // ── Revision-gated periodic tick (§B) ──────────────────────────────────────
  describe("revision-gated tick", () => {
    it("probes the revision and skips the reload when unchanged", async () => {
      const fake = makeFakeListen();
      const load = vi.fn().mockResolvedValue("v");
      const revision = vi.fn().mockResolvedValue("r1");

      subscribeLive(load, "ev", () => {}, { listen: fake.listen, intervalMs: 1000, revision });
      await fake.settle();
      expect(load).toHaveBeenCalledTimes(1); // initial

      // First tick: lastRevision undefined → "r1" differs → reload, remember r1.
      await vi.advanceTimersByTimeAsync(1000);
      expect(revision).toHaveBeenCalledTimes(1);
      expect(load).toHaveBeenCalledTimes(2);

      // Second tick: probe returns "r1" again → unchanged → no reload.
      await vi.advanceTimersByTimeAsync(1000);
      expect(revision).toHaveBeenCalledTimes(2);
      expect(load).toHaveBeenCalledTimes(2);
    });

    it("reloads once when the tick revision changes", async () => {
      const fake = makeFakeListen();
      const load = vi.fn().mockResolvedValue("v");
      const revision = vi.fn().mockResolvedValueOnce("r1").mockResolvedValueOnce("r2");

      subscribeLive(load, "ev", () => {}, { listen: fake.listen, intervalMs: 1000, revision });
      await fake.settle();
      expect(load).toHaveBeenCalledTimes(1);

      await vi.advanceTimersByTimeAsync(1000); // undefined → r1 → reload
      await vi.advanceTimersByTimeAsync(1000); // r1 → r2 → reload
      expect(load).toHaveBeenCalledTimes(3);
    });

    it("falls through to a full reload when the revision probe rejects", async () => {
      const fake = makeFakeListen();
      const load = vi.fn().mockResolvedValue("v");
      const revision = vi.fn().mockRejectedValue(new Error("revision probe unavailable"));

      subscribeLive(load, "ev", () => {}, { listen: fake.listen, intervalMs: 1000, revision });
      await fake.settle();
      expect(load).toHaveBeenCalledTimes(1);

      await vi.advanceTimersByTimeAsync(1000);
      expect(revision).toHaveBeenCalledTimes(1);
      expect(load).toHaveBeenCalledTimes(2); // probe failed → blind reload (old behavior)
    });

    it("retries after a failed load — a rejected load does NOT poison the gate", async () => {
      // Regression: the gate must advance `lastRevision` only after a SUCCESSFUL
      // reload. If it advanced before the load, a transient failure would wedge the
      // board — the safety-net poll would see the same revision and skip forever.
      const fake = makeFakeListen();
      const load = vi
        .fn()
        .mockResolvedValueOnce("v0") // initial load ok
        .mockRejectedValueOnce(new Error("transient blip")) // tick 1 load FAILS
        .mockResolvedValue("v1"); // tick 2 onward ok
      const revision = vi.fn().mockResolvedValue("r1"); // revision never changes

      subscribeLive(load, "ev", () => {}, { listen: fake.listen, intervalMs: 1000, revision });
      await fake.settle();
      expect(load).toHaveBeenCalledTimes(1); // initial

      // Tick 1: probe "r1" != lastRevision(undefined) → run() → load rejects. Because
      // the load failed, lastRevision must stay undefined (gate NOT poisoned).
      await vi.advanceTimersByTimeAsync(1000);
      expect(load).toHaveBeenCalledTimes(2);

      // Tick 2: probe "r1" still != lastRevision(undefined) → run() retries and heals.
      await vi.advanceTimersByTimeAsync(1000);
      expect(load).toHaveBeenCalledTimes(3);

      // Tick 3: the successful reload committed "r1"; now the gate holds → skip.
      await vi.advanceTimersByTimeAsync(1000);
      expect(load).toHaveBeenCalledTimes(3);
    });

    it("does not overlap: a slow probe blocks later ticks until it resolves", async () => {
      // On a large repo the revision probe/load can take several interval periods. The
      // tick must not fire a second probe while the first is still outstanding, or
      // scans would pile up (the original "minutes / never loads" failure).
      const fake = makeFakeListen();
      const load = vi.fn().mockResolvedValue("v");
      let releaseProbe: ((r: string) => void) | undefined;
      const revision = vi.fn(
        () =>
          new Promise<string>((resolve) => {
            releaseProbe = resolve;
          }),
      );

      subscribeLive(load, "ev", () => {}, { listen: fake.listen, intervalMs: 1000, revision });
      await fake.settle();
      expect(load).toHaveBeenCalledTimes(1); // initial

      // First tick starts a probe that hasn't resolved yet.
      await vi.advanceTimersByTimeAsync(1000);
      expect(revision).toHaveBeenCalledTimes(1);

      // Several more intervals pass while the probe is still in flight → no new probes.
      await vi.advanceTimersByTimeAsync(3000);
      expect(revision).toHaveBeenCalledTimes(1);

      // Resolve the probe; the busy flag clears and the next tick may probe again.
      releaseProbe?.("r1");
      await Promise.resolve();
      await vi.advanceTimersByTimeAsync(1000);
      expect(revision).toHaveBeenCalledTimes(2);
    });
  });
});

describe("listenAlso", () => {
  type Handler = (event: { payload: unknown }) => void;
  function recordingListen(fail: ReadonlySet<string> = new Set()) {
    const handlers = new Map<string, Handler>();
    const unlistened: string[] = [];
    const listen = vi.fn(async (event: string, h: Handler) => {
      if (fail.has(event)) throw new Error(`no ${event}`);
      handlers.set(event, h);
      return () => {
        unlistened.push(event);
        handlers.delete(event);
      };
    }) as unknown as typeof import("@tauri-apps/api/event").listen;
    return { listen, handlers, unlistened };
  }

  it("delivers every listed event to the one handler and detaches them all", async () => {
    const rec = recordingListen();
    const seen: unknown[] = [];
    const un = await listenAlso(["b://x"], rec.listen)("a://x", (e) => seen.push(e.payload));
    rec.handlers.get("a://x")?.({ payload: 1 });
    rec.handlers.get("b://x")?.({ payload: 2 });
    expect(seen).toEqual([1, 2]);
    un();
    expect(rec.unlistened.sort()).toEqual(["a://x", "b://x"]);
  });

  it("undoes a partial attach and rethrows when one listen fails", async () => {
    const rec = recordingListen(new Set(["b://x"]));
    await expect(listenAlso(["b://x"], rec.listen)("a://x", () => {})).rejects.toThrow("no b://x");
    expect(rec.unlistened).toEqual(["a://x"]);
  });
});

describe("subscribeLive ordering", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  // A load whose calls each return a promise the test resolves or rejects by index.
  function deferredLoad() {
    const calls: { resolve: (v: string) => void; reject: (e: unknown) => void }[] = [];
    const load = vi.fn(
      () =>
        new Promise<string>((resolve, reject) => {
          calls.push({ resolve, reject });
        }),
    );
    return { load, calls };
  }

  const flush = async () => {
    for (let i = 0; i < 5; i++) await Promise.resolve();
  };

  async function start() {
    const fake = makeFakeListen();
    const { load, calls } = deferredLoad();
    const results: SubscribeResult<unknown>[] = [];
    const sub = subscribeLive(load, "ev", (r) => results.push(r), {
      listen: fake.listen,
      intervalMs: 0,
    });
    await fake.settle();
    await flush();
    return { fake, load, calls, results, sub };
  }

  it("drops an older load that settles after a newer one", async () => {
    const { fake, calls, results } = await start();
    fake.fire(); // load #1
    expect(calls).toHaveLength(2);

    calls[1].resolve("new");
    await flush();
    calls[0].resolve("old");
    await flush();

    expect(results).toEqual([{ data: "new" }]);
  });

  it("applies an older load while a newer one is still in flight", async () => {
    const { fake, calls, results } = await start();
    fake.fire();

    calls[0].resolve("first");
    await flush();
    expect(results).toEqual([{ data: "first" }]);

    calls[1].resolve("second");
    await flush();
    expect(results).toEqual([{ data: "first" }, { data: "second" }]);
  });

  it("drops an older error that settles after a newer result", async () => {
    const { fake, calls, results } = await start();
    fake.fire();

    calls[1].resolve("new");
    await flush();
    calls[0].reject(new Error("stale failure"));
    await flush();

    expect(results).toEqual([{ data: "new" }]);
  });

  it("a dropped result does not advance the revision gate", async () => {
    const { fake, load, calls, results } = await start();
    calls[0].resolve("initial");
    await flush();

    fake.fire({ revision: "r1" }); // load #1, will be superseded
    fake.fire({ revision: "r2" }); // load #2
    calls[2].resolve("r2-data");
    await flush();
    calls[1].resolve("r1-data");
    await flush();
    expect(results).toEqual([{ data: "initial" }, { data: "r2-data" }]);

    // r2 was applied, so a repeat of r2 is gated; r1 was dropped, so r1 still reloads.
    fake.fire({ revision: "r2" });
    expect(load).toHaveBeenCalledTimes(3);
    fake.fire({ revision: "r1" });
    expect(load).toHaveBeenCalledTimes(4);
  });

  it("orders a manual reload with event loads", async () => {
    const { fake, calls, results, sub } = await start();
    calls[0].resolve("initial");
    await flush();

    const manual = sub.reload(); // load #1
    fake.fire(); // load #2
    calls[2].resolve("event");
    await flush();
    calls[1].resolve("manual");
    await manual;
    await flush();

    expect(results).toEqual([{ data: "initial" }, { data: "event" }]);
  });

  it("applies nothing after teardown", async () => {
    const { calls, results, sub } = await start();
    sub();
    calls[0].resolve("late");
    await flush();
    expect(results).toEqual([]);
  });
});

describe("newestSettledGate", () => {
  it("rejects an older load that settles after a newer one", () => {
    const gate = newestSettledGate();
    const older = gate.begin();
    const newer = gate.begin();
    expect(newer()).toBe(true);
    expect(older()).toBe(false);
  });

  it("accepts loads that settle in start order", () => {
    const gate = newestSettledGate();
    const first = gate.begin();
    const second = gate.begin();
    expect(first()).toBe(true);
    expect(second()).toBe(true);
  });

  it("accepts a load that begins after a settled one", () => {
    const gate = newestSettledGate();
    expect(gate.begin()()).toBe(true);
    expect(gate.begin()()).toBe(true);
  });
});
