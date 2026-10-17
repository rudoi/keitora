// The "↓ live" indicator's state: whether a pane's viewport is scrolled up into
// history. It asks the backend for the offset a short while after a scroll up goes
// out, or after output while the pill shows (a history erase or a snap-back may have
// brought the viewport live), never on a timer while hidden and idle. It forgets the
// state on any input, because herdr snaps the viewport back to the live tail then,
// and on a new session or attach.

import { trailingDebounce } from "./debounce";
import { MAX_SCROLL_LINES, type ScrollDirection } from "./ipc";

/** How long after the last scroll (or output, while shown) the offset is queried. */
export const LIVE_TAIL_DEBOUNCE_MS = 150;

export interface LiveTailOptions {
  /** Lines above the live tail, or null when the backend cannot tell. */
  query(): Promise<number | null>;
  /** Called whenever the pill should show or hide. */
  onChange(visible: boolean): void;
  delayMs?: number;
}

export interface LiveTail {
  /**
   * A scroll was sent: query the offset once the burst settles. Scrolling down while
   * the pill is hidden is already at the live tail, so it asks nothing.
   */
  noteScroll(direction: ScrollDirection): void;
  /** Output arrived: while the pill shows, re-check once the output settles. */
  noteOutput(): void;
  /** Input was sent: the viewport is back at the live tail. */
  noteInput(): void;
  /** A new session or attach: hide the pill and drop any query in flight. */
  reset(): void;
  /**
   * The pill was clicked (or End pressed). Hides it and returns the lines to scroll
   * down: MAX_SCROLL_LINES, since herdr stops its history scroll at the live tail
   * (and an app that takes the wheel itself gets a single event). Null when the pill
   * is not showing. The offset is queried again afterwards in case it fell short.
   */
  jumpToLive(): number | null;
  readonly visible: boolean;
  dispose(): void;
}

export function createLiveTail(opts: LiveTailOptions): LiveTail {
  let visible = false;
  // Bumped by anything that makes an in-flight answer stale (an input, a jump, a reset).
  let epoch = 0;
  let disposed = false;

  const show = (next: boolean) => {
    if (disposed || next === visible) return;
    visible = next;
    opts.onChange(next);
  };

  const query = trailingDebounce(() => {
    const asked = epoch;
    opts.query().then(
      (lines) => {
        if (disposed || asked !== epoch) return;
        // null: the backend cannot tell, so there is nothing to indicate.
        show((lines ?? 0) > 0);
      },
      () => {
        /* best-effort; the next scroll asks again */
      },
    );
  }, opts.delayMs ?? LIVE_TAIL_DEBOUNCE_MS);

  const reset = () => {
    epoch++;
    query.cancel();
    show(false);
  };

  return {
    noteScroll(direction) {
      if (disposed) return;
      if (direction === "up" || visible) query.call();
    },
    noteOutput() {
      if (!disposed && visible) query.call();
    },
    noteInput: reset,
    reset,
    jumpToLive() {
      if (!visible || disposed) return null;
      epoch++;
      show(false);
      query.call();
      return MAX_SCROLL_LINES;
    },
    get visible() {
      return visible;
    },
    dispose() {
      disposed = true;
      query.cancel();
    },
  };
}
