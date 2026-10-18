import { beforeEach, describe, expect, it, vi } from "vitest";

const backend: string[] = [];
let releaseUnsubscribe: (() => void) | null = null;
let failUnsubscribe = false;

vi.mock("./ipc", () => ({
  perfSubscribe: vi.fn(async () => {
    backend.push("subscribe");
  }),
  // Settles only when the test says so, like a slow thread-pool command.
  perfUnsubscribe: vi.fn(
    () =>
      new Promise<void>((resolve, reject) => {
        releaseUnsubscribe = () => {
          backend.push("unsubscribe");
          if (failUnsubscribe) reject(new Error("backend gone"));
          else resolve();
        };
      }),
  ),
}));

import { releasePerfLease, subscribePerfLease } from "./perfLease";

/** Let queued promise callbacks run. */
async function flush(): Promise<void> {
  for (let i = 0; i < 5; i++) await Promise.resolve();
}

describe("perf lease call ordering", () => {
  beforeEach(() => {
    backend.length = 0;
    releaseUnsubscribe = null;
    failUnsubscribe = false;
  });

  it("holds a subscribe until the earlier unsubscribe has reached the backend", async () => {
    const released = releasePerfLease();
    const subscribed = subscribePerfLease();
    await flush();
    expect(backend).toEqual([]);
    releaseUnsubscribe?.();
    await Promise.all([released, subscribed]);
    expect(backend).toEqual(["unsubscribe", "subscribe"]);
  });

  it("keeps the chain going after a failed call", async () => {
    failUnsubscribe = true;
    const released = releasePerfLease();
    const subscribed = subscribePerfLease();
    await flush();
    releaseUnsubscribe?.();
    await expect(released).resolves.toBeUndefined();
    await subscribed;
    expect(backend).toEqual(["unsubscribe", "subscribe"]);
  });
});
