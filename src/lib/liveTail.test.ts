import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MAX_SCROLL_LINES } from "./ipc";
import { createLiveTail, LIVE_TAIL_DEBOUNCE_MS } from "./liveTail";

function harness() {
  const changes: boolean[] = [];
  const pending: { resolve: (n: number | null) => void; reject: (e: unknown) => void }[] = [];
  let queries = 0;
  const tail = createLiveTail({
    query: () => {
      queries++;
      return new Promise<number | null>((resolve, reject) => pending.push({ resolve, reject }));
    },
    onChange: (v) => changes.push(v),
  });
  return {
    tail,
    changes,
    timers: () => vi.getTimerCount(),
    queries: () => queries,
    fire() {
      vi.runOnlyPendingTimers();
    },
    async answer(n: number | null, i = pending.length - 1) {
      pending[i].resolve(n);
      await Promise.resolve();
      await Promise.resolve();
    },
    async fail(i = pending.length - 1) {
      pending[i].reject(new Error("down"));
      await Promise.resolve();
      await Promise.resolve();
    },
    /** Scroll up, let the query run, and answer it with `n`. */
    async scrolledTo(n: number | null) {
      tail.noteScroll("up");
      vi.runOnlyPendingTimers();
      await this.answer(n);
    },
  };
}

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("createLiveTail", () => {
  it("asks nothing until a scroll up goes out, then once after the debounce", () => {
    const h = harness();
    expect(h.timers()).toBe(0);
    h.tail.noteScroll("up");
    vi.advanceTimersByTime(LIVE_TAIL_DEBOUNCE_MS - 1);
    expect(h.queries()).toBe(0);
    vi.advanceTimersByTime(1);
    expect(h.queries()).toBe(1);
  });

  it("restarts the debounce on each scroll of a burst", () => {
    const h = harness();
    h.tail.noteScroll("up");
    h.tail.noteScroll("up");
    h.tail.noteScroll("up");
    expect(h.timers()).toBe(1);
    h.fire();
    expect(h.queries()).toBe(1);
  });

  it("asks nothing for a scroll down while the pill is hidden", () => {
    const h = harness();
    h.tail.noteScroll("down");
    expect(h.timers()).toBe(0);
    expect(h.queries()).toBe(0);
  });

  it("asks after a scroll down while the pill shows", async () => {
    const h = harness();
    await h.scrolledTo(10);
    h.tail.noteScroll("down");
    h.fire();
    expect(h.queries()).toBe(2);
    await h.answer(0);
    expect(h.tail.visible).toBe(false);
  });

  it("shows the pill for a positive offset and hides it at zero", async () => {
    const h = harness();
    await h.scrolledTo(40);
    expect(h.tail.visible).toBe(true);
    await h.scrolledTo(0);
    expect(h.tail.visible).toBe(false);
    expect(h.changes).toEqual([true, false]);
  });

  it("does not show for an unknown offset", async () => {
    const h = harness();
    await h.scrolledTo(null);
    expect(h.tail.visible).toBe(false);
    expect(h.changes).toEqual([]);
  });

  it("keeps the state when a query fails", async () => {
    const h = harness();
    await h.scrolledTo(5);
    h.tail.noteScroll("up");
    h.fire();
    await h.fail();
    expect(h.tail.visible).toBe(true);
  });

  it("ignores output while the pill is hidden", () => {
    const h = harness();
    h.tail.noteOutput();
    h.tail.noteOutput();
    expect(h.timers()).toBe(0);
    expect(h.queries()).toBe(0);
  });

  it("re-checks once output settles while the pill shows, and hides on a snap-back", async () => {
    const h = harness();
    await h.scrolledTo(12);
    h.tail.noteOutput();
    vi.advanceTimersByTime(LIVE_TAIL_DEBOUNCE_MS - 1);
    h.tail.noteOutput();
    expect(h.timers()).toBe(1);
    vi.advanceTimersByTime(LIVE_TAIL_DEBOUNCE_MS - 1);
    expect(h.queries()).toBe(1);
    vi.advanceTimersByTime(1);
    expect(h.queries()).toBe(2);
    await h.answer(0);
    expect(h.tail.visible).toBe(false);
    // Hidden again: output asks nothing more.
    h.tail.noteOutput();
    expect(h.timers()).toBe(0);
  });

  it("hides on input and cancels a query still waiting", () => {
    const h = harness();
    h.tail.noteScroll("up");
    h.tail.noteInput();
    expect(h.timers()).toBe(0);
    expect(h.queries()).toBe(0);
  });

  it("ignores an answer that was in flight when input arrived", async () => {
    const h = harness();
    h.tail.noteScroll("up");
    h.fire();
    h.tail.noteInput();
    await h.answer(30);
    expect(h.tail.visible).toBe(false);
    expect(h.changes).toEqual([]);
  });

  it("hides on input after the pill showed", async () => {
    const h = harness();
    await h.scrolledTo(30);
    h.tail.noteInput();
    expect(h.tail.visible).toBe(false);
    expect(h.changes).toEqual([true, false]);
  });

  it("reset hides the pill, cancels a waiting query and drops one in flight", async () => {
    const h = harness();
    await h.scrolledTo(30);
    h.tail.noteScroll("up");
    h.tail.reset();
    expect(h.tail.visible).toBe(false);
    expect(h.timers()).toBe(0);
    h.tail.noteScroll("up");
    h.fire();
    h.tail.reset();
    await h.answer(8);
    expect(h.tail.visible).toBe(false);
    expect(h.changes).toEqual([true, false]);
  });

  it("jumps all the way down, hides, and re-checks", async () => {
    const h = harness();
    await h.scrolledTo(120);
    expect(h.tail.jumpToLive()).toBe(MAX_SCROLL_LINES);
    expect(h.tail.visible).toBe(false);
    expect(h.timers()).toBe(1);
    h.fire();
    await h.answer(0);
    expect(h.changes).toEqual([true, false]);
  });

  it("shows the pill again when the jump fell short", async () => {
    const h = harness();
    await h.scrolledTo(100000);
    expect(h.tail.jumpToLive()).toBe(MAX_SCROLL_LINES);
    h.fire();
    await h.answer(34465);
    expect(h.tail.visible).toBe(true);
  });

  it("ignores an answer that was in flight when the jump went out", async () => {
    const h = harness();
    await h.scrolledTo(50);
    h.tail.noteScroll("up");
    h.fire();
    h.tail.jumpToLive();
    await h.answer(60, 1);
    expect(h.tail.visible).toBe(false);
  });

  it("has nothing to jump to while the pill is hidden", () => {
    const h = harness();
    expect(h.tail.jumpToLive()).toBeNull();
    expect(h.timers()).toBe(0);
  });

  it("does nothing after dispose", async () => {
    const h = harness();
    h.tail.noteScroll("up");
    h.fire();
    h.tail.dispose();
    await h.answer(9);
    h.tail.noteScroll("up");
    h.tail.noteOutput();
    expect(h.tail.visible).toBe(false);
    expect(h.timers()).toBe(0);
  });
});
