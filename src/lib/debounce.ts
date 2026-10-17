// Timer helpers shared by hooks and pane logic.

/**
 * Wrap `fn` so a burst of calls runs it once, `ms` after the last call. `cancel` drops a
 * pending run (for teardown).
 */
export function trailingDebounce(
  fn: () => void,
  ms: number,
): { call: () => void; cancel: () => void } {
  let timer: ReturnType<typeof setTimeout> | undefined;
  return {
    call: () => {
      if (timer !== undefined) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = undefined;
        fn();
      }, ms);
    },
    cancel: () => {
      if (timer !== undefined) clearTimeout(timer);
      timer = undefined;
    },
  };
}
