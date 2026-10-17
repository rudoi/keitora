import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { trailingDebounce } from "./debounce";

describe("trailingDebounce", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("runs once, after the window, for a burst of calls", () => {
    const fn = vi.fn();
    const d = trailingDebounce(fn, 150);
    d.call();
    vi.advanceTimersByTime(100);
    d.call();
    d.call();
    vi.advanceTimersByTime(149);
    expect(fn).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(fn).toHaveBeenCalledTimes(1);
  });

  it("runs again for a later, separate call", () => {
    const fn = vi.fn();
    const d = trailingDebounce(fn, 150);
    d.call();
    vi.advanceTimersByTime(150);
    d.call();
    vi.advanceTimersByTime(150);
    expect(fn).toHaveBeenCalledTimes(2);
  });

  it("drops a pending run on cancel", () => {
    const fn = vi.fn();
    const d = trailingDebounce(fn, 150);
    d.call();
    d.cancel();
    vi.advanceTimersByTime(500);
    expect(fn).not.toHaveBeenCalled();
  });
});
