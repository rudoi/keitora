// Ordered access to the backend's performance sampling lease. Subscribe and unsubscribe are
// async commands run on a thread pool, so two calls issued back to back can land in either
// order — a stale unsubscribe from an unmount could overtake the next mount's subscribe and
// kill its lease. Every lease call is therefore chained behind the previous one and reaches
// the backend in the order it was issued.

import { perfSubscribe, perfUnsubscribe } from "./ipc";

let chain: Promise<void> = Promise.resolve();

/** Run `call` once every earlier lease call has settled; its failure is swallowed. */
function enqueue(call: () => Promise<void>): Promise<void> {
  chain = chain.then(call).catch(() => {});
  return chain;
}

/** Take or renew the sampling lease, after any earlier lease call. */
export function subscribePerfLease(): Promise<void> {
  return enqueue(perfSubscribe);
}

/** Release the sampling lease, after any earlier lease call. */
export function releasePerfLease(): Promise<void> {
  return enqueue(perfUnsubscribe);
}
