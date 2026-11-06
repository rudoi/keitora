import { describe, expect, it, vi } from "vitest";
import { createFrameCoalescer } from "./frameCoalescer";
import { createOutputGate } from "./paneOutput";

/** A target that records what the pane would have shown, as text. */
function recorder() {
  const log: string[] = [];
  const fulls: boolean[] = [];
  return {
    log,
    fulls,
    target: {
      write: (bytes: Uint8Array, full: boolean) => {
        log.push(new TextDecoder().decode(bytes));
        fulls.push(full);
      },
      exit: () => log.push("<exit>"),
    },
  };
}

const frame = (text: string, full = false) => ({ type: "frame", data: btoa(text), full }) as const;

describe("createOutputGate", () => {
  it("holds frames until the scrollback is written, then flushes them in order", () => {
    const { log, target } = recorder();
    const gate = createOutputGate(target);
    gate.onMessage(frame("one"));
    gate.onMessage(frame("two", true));
    expect(log).toEqual([]);

    log.push("<scrollback>");
    gate.open();
    expect(log).toEqual(["<scrollback>", "one", "two"]);

    gate.onMessage(frame("three"));
    expect(log).toEqual(["<scrollback>", "one", "two", "three"]);
  });

  it("delivers an exit in its place in the stream", () => {
    const { log, target } = recorder();
    const gate = createOutputGate(target);
    gate.onMessage(frame("bye"));
    gate.onMessage({ type: "exit" });
    gate.open();
    expect(log).toEqual(["bye", "<exit>"]);
  });

  it("drops everything once closed, buffered or not", () => {
    const { log, target } = recorder();
    const gate = createOutputGate(target);
    gate.onMessage(frame("stale"));
    gate.close();
    gate.open();
    gate.onMessage(frame("late"));
    gate.onMessage({ type: "exit" });
    expect(log).toEqual([]);
  });

  it("passes each frame's full flag on", () => {
    const { log, fulls, target } = recorder();
    const gate = createOutputGate(target);
    gate.onMessage(frame("diff"));
    gate.open();
    gate.onMessage(frame("repaint", true));
    expect(log).toEqual(["diff", "repaint"]);
    expect(fulls).toEqual([false, true]);
  });

  it("opens only once", () => {
    const { log, target } = recorder();
    const gate = createOutputGate(target);
    gate.onMessage(frame("a"));
    gate.open();
    gate.open();
    expect(log).toEqual(["a"]);
  });
});

/**
 * The pane's wiring: frames go through a coalescer, and an exit ends the pane once
 * the output before it is written. A restart resets the coalescer.
 */
function paneWiring() {
  let next = 1;
  const frames = new Map<number, () => void>();
  const timers = new Map<number, () => void>();
  const dones: (() => void)[] = [];
  const ended = vi.fn();
  const coalescer = createFrameCoalescer({
    write: (_bytes, done) => dones.push(done),
    onNeedsRepaint: () => {},
    visible: true,
    scheduler: {
      request: (cb) => {
        frames.set(next, cb);
        return next++;
      },
      cancel: (h) => {
        frames.delete(h);
      },
      timeout: (cb) => {
        timers.set(next, cb);
        return next++;
      },
      clearTimeout: (h) => {
        timers.delete(h);
      },
    },
  });
  const gate = createOutputGate({
    write: (bytes, full) => coalescer.push(bytes, undefined, full),
    exit: () => coalescer.end(ended),
  });
  const settle = () => {
    // Run scheduled flushes, then complete every write they issued.
    const pending = [...frames.values(), ...timers.values()];
    frames.clear();
    timers.clear();
    for (const cb of pending) cb();
    while (dones.length) dones.shift()?.();
  };
  return { gate, coalescer, ended, settle };
}

describe("exit around a restart", () => {
  it("exit before the restart's reset does not end the pane", () => {
    const { gate, coalescer, ended, settle } = paneWiring();
    gate.open();
    gate.onMessage(frame("output"));
    gate.onMessage({ type: "exit" });
    coalescer.reset();
    settle();
    expect(ended).not.toHaveBeenCalled();
  });

  // The backend stops an old session's bridge (joining its reader) before the kill
  // returns, so its exit is sent before the replacement starts. The one remaining
  // window is transport delivery order between the kill's reply and the output
  // channel, which the backend cannot rule out; an exit landing after the reset is
  // indistinguishable from the new session's own, so it ends the pane. A session
  // generation on the exit message would close this, but is not added without
  // evidence that the late delivery happens.
  it("exit that arrives after the restart's reset ends the pane (known gap)", () => {
    const { gate, coalescer, ended, settle } = paneWiring();
    gate.open();
    coalescer.reset();
    gate.onMessage({ type: "exit" });
    settle();
    expect(ended).toHaveBeenCalledTimes(1);
  });
});
