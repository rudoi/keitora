// Owns the updater state for the whole app: seeds it from `update_status`, follows the
// `updater://status` and `updater://progress` events, and toasts manual check results.
// The banner and the Updates settings section both read it through `useUpdates()`.

import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useReducer,
  useRef,
} from "react";
import {
  onUpdaterProgress,
  onUpdaterStatus,
  updateApply,
  updateCheck,
  updateDownload,
  updateStatus,
} from "../lib/ipc";
import {
  errorText,
  INITIAL_UPDATES,
  installUpdate,
  toastFor,
  type UpdatesState,
  updatesReducer,
} from "../lib/updates";
import { useToast } from "./Toast";

interface UpdatesCtx {
  state: UpdatesState;
  /** Check now; the result arrives as a status event. */
  check: () => void;
  /** Download then apply; the app restarts on success. */
  install: () => void;
  dismiss: () => void;
}

const Ctx = createContext<UpdatesCtx>({
  state: INITIAL_UPDATES,
  check: () => {},
  install: () => {},
  dismiss: () => {},
});

export function useUpdates(): UpdatesCtx {
  return useContext(Ctx);
}

export function UpdatesProvider({ children }: { children: ReactNode }) {
  const { showToast } = useToast();
  const [state, dispatch] = useReducer(updatesReducer, INITIAL_UPDATES);
  const toastRef = useRef(showToast);
  toastRef.current = showToast;
  // One install at a time, however many buttons call install().
  const installingRef = useRef(false);

  useEffect(() => {
    let cancelled = false;
    const unlisten: Array<() => void> = [];
    const keep = (p: Promise<() => void>) =>
      void p.then((fn) => (cancelled ? fn() : unlisten.push(fn))).catch(() => {});

    keep(
      onUpdaterStatus((event) => {
        dispatch({ type: "status", event });
        const msg = toastFor(event);
        if (msg) toastRef.current(msg);
      }),
    );
    keep(onUpdaterProgress((event) => dispatch({ type: "progress", event })));
    // Subscribed first, so an event landing before the snapshot is not lost to it.
    void updateStatus()
      .then((snapshot) => {
        if (!cancelled) dispatch({ type: "seed", snapshot });
      })
      .catch(() => {});

    return () => {
      cancelled = true;
      for (const fn of unlisten) fn();
    };
  }, []);

  const check = useCallback(() => {
    dispatch({ type: "check_start" });
    // State and the toast come from the status event; the return value only ends the spinner.
    void updateCheck()
      .catch((err: unknown) => toastRef.current(`Update check failed: ${errorText(err)}`))
      .finally(() => dispatch({ type: "check_end" }));
  }, []);

  const install = useCallback(() => {
    if (installingRef.current) return;
    installingRef.current = true;
    dispatch({ type: "install_start" });
    void installUpdate({ download: updateDownload, apply: updateApply })
      .then((r) => dispatch({ type: "install_end", error: r.ok ? null : r.error }))
      .finally(() => {
        installingRef.current = false;
      });
  }, []);

  const dismiss = useCallback(() => dispatch({ type: "dismiss" }), []);

  const value = useMemo(
    () => ({ state, check, install, dismiss }),
    [state, check, install, dismiss],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}
