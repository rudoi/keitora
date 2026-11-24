// Owns the dependency state for the whole app: seeds it from `deps_status`, follows the
// `deps://status` and `deps://progress` events, and re-pulls the snapshot when herdr
// recovery reports herdr found or lost. The onboarding step, the rail card, the
// upgrade dialog and the Settings section all read it through `useDeps()`.

import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
} from "react";
import {
  type DepsState,
  depsReducer,
  INITIAL_DEPS,
  isBelowFloor,
  type OverwriteChoice,
  overwriteInstall,
} from "../lib/deps";
import { type DepsTier, depsInstall, depsStatus, onDepsProgress, onDepsStatus } from "../lib/ipc";
import { HERDR_NOT_FOUND } from "../lib/recovery";
import { useRecovery } from "../lib/sessions";
import { errorText } from "../lib/updates";
import { useConfirmDialog } from "./ConfirmDialog";
import { useToast } from "./Toast";

interface DepsCtx {
  state: DepsState;
  /** Open the upgrade dialog even when the backend's prompt was snoozed. */
  upgradeOpen: boolean;
  setUpgradeOpen: (open: boolean) => void;
  /** Re-pull the snapshot. */
  refresh: () => void;
}

const Ctx = createContext<DepsCtx>({
  state: INITIAL_DEPS,
  upgradeOpen: false,
  setUpgradeOpen: () => {},
  refresh: () => {},
});

export function useDeps(): DepsCtx {
  return useContext(Ctx);
}

export function DepsProvider({ children }: { children: ReactNode }) {
  const [state, dispatch] = useReducer(depsReducer, INITIAL_DEPS);
  const [upgradeOpen, setUpgradeOpen] = useState(false);
  const recovery = useRecovery();

  const refresh = useCallback(() => {
    void depsStatus()
      .then((snapshot) => dispatch({ type: "status", snapshot }))
      .catch(() => dispatch({ type: "load_failed" }));
  }, []);

  useEffect(() => {
    let cancelled = false;
    const unlisten: Array<() => void> = [];
    const keep = (p: Promise<() => void>) =>
      void p.then((fn) => (cancelled ? fn() : unlisten.push(fn))).catch(() => {});
    keep(onDepsStatus((snapshot) => dispatch({ type: "status", snapshot })));
    keep(onDepsProgress((event) => dispatch({ type: "progress", event })));
    // Subscribed first, so an event landing before the snapshot is not lost to it.
    void depsStatus()
      .then((snapshot) => {
        if (!cancelled) dispatch({ type: "status", snapshot });
      })
      .catch(() => {
        if (!cancelled) dispatch({ type: "load_failed" });
      });
    return () => {
      cancelled = true;
      for (const fn of unlisten) fn();
    };
  }, []);

  // herdr found or lost changes `herdr_installed`, which the backend reads on demand.
  const herdrMissing = recovery.reason_code === HERDR_NOT_FOUND;
  const firstMissing = useRef(true);
  // biome-ignore lint/correctness/useExhaustiveDependencies: re-pull only when herdr is found or lost
  useEffect(() => {
    if (firstMissing.current) {
      firstMissing.current = false;
      return;
    }
    refresh();
  }, [herdrMissing]);

  // The dialog closes itself once there is nothing left to upgrade.
  const floor = state.snapshot?.floor;
  const belowFloor = floor !== undefined && isBelowFloor(floor);
  useEffect(() => {
    if (upgradeOpen && !belowFloor) setUpgradeOpen(false);
  }, [belowFloor, upgradeOpen]);

  const value = useMemo(
    () => ({ state, upgradeOpen, setUpgradeOpen, refresh }),
    [state, upgradeOpen, refresh],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

const CHOICE: Record<"confirm" | "alt" | "cancel", OverwriteChoice> = {
  confirm: "replace",
  alt: "keep",
  cancel: "cancel",
};

/**
 * Starts an install, asking first when it would replace a herdr Keitora didn't install:
 * Replace, keep it and install a separate Keitora-managed copy, or Cancel (also Esc or
 * an outside click), which installs nothing. Failures to start toast; how the run goes
 * arrives as events.
 */
export function useDepsInstall(): (tier?: DepsTier | null) => Promise<void> {
  const { state } = useDeps();
  const { choiceDialog } = useConfirmDialog();
  const { showToast } = useToast();
  const target = state.snapshot?.overwrite_target ?? null;
  return useCallback(
    async (tier: DepsTier | null = null) => {
      let request: { tier: DepsTier | null; confirmOverwrite: boolean } | null = {
        tier,
        confirmOverwrite: false,
      };
      if (target !== null && tier !== "direct") {
        const choice = await choiceDialog({
          title: "Replace existing herdr?",
          body: `Keitora didn't install the herdr at ${target}. Replace it, or keep it and install a separate copy for Keitora.`,
          confirmLabel: "Replace",
          altLabel: "Keep it, install a copy",
          cancelLabel: "Cancel",
        });
        request = overwriteInstall(CHOICE[choice], tier);
      }
      if (request === null) return;
      try {
        await depsInstall(request.tier, request.confirmOverwrite);
      } catch (err) {
        showToast(`Could not start the install: ${errorText(err)}`);
      }
    },
    [target, choiceDialog, showToast],
  );
}
