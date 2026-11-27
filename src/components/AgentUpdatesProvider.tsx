// Owns the agent CLI update state for the whole app: seeds it from `agent_updates_status`,
// follows the `agent-updates://…` events, and keeps this run's "later" choices plus the
// per-pane marks (sessions to restart, stopped for an update, parked on an update prompt).
// The banner, the dialog and the settings section read it through `useAgentUpdates()`;
// terminal panes read the per-pane marks through `useAgentUpdatePane()`, a separate
// context so a status change doesn't re-render every pane.

import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  type LaterMap,
  sessionKey,
  sessionKeys,
  withCliKeys,
  withoutCliKey,
} from "../lib/agentUpdates";
import {
  type AgentUpdatesSnapshot,
  agentUpdateRestart,
  agentUpdateSkip,
  agentUpdatesCheck,
  agentUpdatesStatus,
  type CliKind,
  onAgentUpdatePrompt,
  onAgentUpdateRestart,
  onAgentUpdateRun,
  onAgentUpdateStop,
  onAgentUpdatesStatus,
  type RunOutcome,
} from "../lib/ipc";
import { CLI_KINDS } from "../lib/providers";
import { errorText } from "../lib/updates";
import { useToast } from "./Toast";

/** The latest update run's outcome; `seq` tells runs apart. */
export interface RunResult {
  cli: CliKind;
  outcome: RunOutcome;
  seq: number;
}

interface AgentUpdatesCtx {
  snapshot: AgentUpdatesSnapshot | null;
  /** A user-requested check is in flight. */
  checking: boolean;
  runResult: RunResult | null;
  /** The banner's post-update row was dismissed. */
  outcomeDismissed: boolean;
  /** The CLI the update dialog is open for. */
  dialogCli: CliKind | null;
  later: LaterMap;
  openDialog: (cli: CliKind) => void;
  closeDialog: () => void;
  putOffUntilLater: (cli: CliKind) => void;
  skip: (cli: CliKind, version: string) => void;
  check: () => void;
  /** Restart `cli`'s stopped and running sessions (the panes do it). */
  restartSessions: (cli: CliKind) => void;
  dismissOutcome: () => void;
}

interface AgentUpdatePaneCtx {
  /** Sessions to relaunch on the new version of a CLI, and which CLI. A pane mounted
   *  when the user asked restarts once its attach settles; one that mounts later only
   *  offers the restart (see `pendingRestartAction`). */
  pendingRestart: ReadonlyMap<string, CliKind>;
  /** The installed version of each CLI, as last checked. */
  versions: Partial<Record<CliKind, string>>;
  /** Sessions stopped so their CLI could be updated, and which CLI. */
  stopped: ReadonlyMap<string, CliKind>;
  /** Sessions whose launch is parked on the CLI's update screen. */
  prompted: ReadonlyMap<string, CliKind>;
  openDialog: (cli: CliKind) => void;
  /** Remove `key` from the pending restarts; whether it was there. */
  takePendingRestart: (key: string) => boolean;
  /** The pane at `key` launched a new session: drop its restart and stopped marks. */
  settle: (key: string) => void;
  /** Drop `key`'s update-prompt hint. */
  clearPrompt: (key: string) => void;
  /** Check now, quietly (a launch just hit an update prompt). */
  checkQuietly: () => void;
}

const noop = () => {};
const Ctx = createContext<AgentUpdatesCtx>({
  snapshot: null,
  checking: false,
  runResult: null,
  outcomeDismissed: false,
  dialogCli: null,
  later: {},
  openDialog: noop,
  closeDialog: noop,
  putOffUntilLater: noop,
  skip: noop,
  check: noop,
  restartSessions: noop,
  dismissOutcome: noop,
});
const PaneCtx = createContext<AgentUpdatePaneCtx>({
  pendingRestart: new Map(),
  versions: {},
  stopped: new Map(),
  prompted: new Map(),
  openDialog: noop,
  takePendingRestart: () => false,
  settle: noop,
  clearPrompt: noop,
  checkQuietly: noop,
});

export function useAgentUpdates(): AgentUpdatesCtx {
  return useContext(Ctx);
}

export function useAgentUpdatePane(): AgentUpdatePaneCtx {
  return useContext(PaneCtx);
}

export function AgentUpdatesProvider({ children }: { children: ReactNode }) {
  const { showToast } = useToast();
  const toastRef = useRef(showToast);
  toastRef.current = showToast;

  const [snapshot, setSnapshot] = useState<AgentUpdatesSnapshot | null>(null);
  const [checking, setChecking] = useState(false);
  const [runResult, setRunResult] = useState<RunResult | null>(null);
  const [outcomeDismissed, setOutcomeDismissed] = useState(false);
  const [dialogCli, setDialogCli] = useState<CliKind | null>(null);
  const [later, setLater] = useState<LaterMap>({});
  const [pendingRestart, setPendingRestart] = useState<ReadonlyMap<string, CliKind>>(
    () => new Map(),
  );
  const [stopped, setStopped] = useState<ReadonlyMap<string, CliKind>>(() => new Map());
  const [prompted, setPrompted] = useState<ReadonlyMap<string, CliKind>>(() => new Map());
  const seqRef = useRef(0);
  // Mirrors pendingRestart for takePendingRestart, which answers synchronously.
  const pendingRef = useRef(pendingRestart);
  pendingRef.current = pendingRestart;
  const snapshotRef = useRef(snapshot);
  snapshotRef.current = snapshot;

  const checkQuietly = useCallback(() => {
    void agentUpdatesCheck(false)
      .then(setSnapshot)
      .catch(() => {});
  }, []);

  useEffect(() => {
    let cancelled = false;
    const unlisten: Array<() => void> = [];
    const keep = (p: Promise<() => void>) =>
      void p.then((fn) => (cancelled ? fn() : unlisten.push(fn))).catch(() => {});

    keep(onAgentUpdatesStatus((s) => setSnapshot(s)));
    keep(
      onAgentUpdateRun(({ cli, outcome }) => {
        seqRef.current += 1;
        setRunResult({ cli, outcome, seq: seqRef.current });
        setOutcomeDismissed(false);
      }),
    );
    keep(
      onAgentUpdateStop(({ cli, keys }) => {
        setStopped((m) => withCliKeys(m, sessionKeys(keys), cli));
      }),
    );
    keep(
      onAgentUpdateRestart(({ cli, keys }) => {
        setPendingRestart((m) => withCliKeys(m, sessionKeys(keys), cli));
      }),
    );
    keep(
      onAgentUpdatePrompt(({ project, worktree, cli }) => {
        setPrompted((m) => withCliKeys(m, [sessionKey(project, worktree)], cli));
        checkQuietly();
      }),
    );
    // Subscribed first, so an event landing before the snapshot is not lost to it.
    void agentUpdatesStatus()
      .then((s) => {
        if (cancelled) return;
        setSnapshot((cur) => cur ?? s);
        // Sessions stopped before a webview reload still read as stopped.
        for (const st of s.stopped) {
          setStopped((m) => withCliKeys(m, [sessionKey(st.project, st.worktree)], st.cli));
        }
      })
      .catch(() => {});

    return () => {
      cancelled = true;
      for (const fn of unlisten) fn();
    };
  }, [checkQuietly]);

  const check = useCallback(() => {
    setChecking(true);
    void agentUpdatesCheck(true)
      .then(setSnapshot)
      .catch((err: unknown) => toastRef.current(`Agent update check failed: ${errorText(err)}`))
      .finally(() => setChecking(false));
  }, []);

  const openDialog = useCallback((cli: CliKind) => setDialogCli(cli), []);
  const closeDialog = useCallback(() => setDialogCli(null), []);

  const putOffUntilLater = useCallback((cli: CliKind) => {
    const state = snapshotRef.current?.agents[cli]?.state;
    if (state?.type !== "available") return;
    setLater((l) => ({ ...l, [cli]: state.latest }));
  }, []);

  const skip = useCallback((cli: CliKind, version: string) => {
    void agentUpdateSkip(cli, version)
      .then(setSnapshot)
      .catch((err: unknown) => toastRef.current(`Could not skip: ${errorText(err)}`));
  }, []);

  const restartSessions = useCallback((cli: CliKind) => {
    setOutcomeDismissed(true);
    void agentUpdateRestart(cli)
      .then((keys) => {
        const n = keys.length;
        toastRef.current(
          n === 0
            ? `No ${cli} sessions to restart`
            : `Restarting ${n} ${cli} session${n === 1 ? "" : "s"}`,
        );
      })
      .catch((err: unknown) => toastRef.current(`Could not restart: ${errorText(err)}`));
  }, []);

  const dismissOutcome = useCallback(() => setOutcomeDismissed(true), []);

  const takePendingRestart = useCallback((key: string) => {
    if (!pendingRef.current.has(key)) return false;
    pendingRef.current = withoutCliKey(pendingRef.current, key);
    setPendingRestart((m) => withoutCliKey(m, key));
    return true;
  }, []);

  const settle = useCallback((key: string) => {
    if (pendingRef.current.has(key)) {
      pendingRef.current = withoutCliKey(pendingRef.current, key);
      setPendingRestart((m) => withoutCliKey(m, key));
    }
    setStopped((m) => withoutCliKey(m, key));
  }, []);

  const clearPrompt = useCallback((key: string) => {
    setPrompted((m) => withoutCliKey(m, key));
  }, []);

  const value = useMemo(
    () => ({
      snapshot,
      checking,
      runResult,
      outcomeDismissed,
      dialogCli,
      later,
      openDialog,
      closeDialog,
      putOffUntilLater,
      skip,
      check,
      restartSessions,
      dismissOutcome,
    }),
    [
      snapshot,
      checking,
      runResult,
      outcomeDismissed,
      dialogCli,
      later,
      openDialog,
      closeDialog,
      putOffUntilLater,
      skip,
      check,
      restartSessions,
      dismissOutcome,
    ],
  );
  // Keyed on the installed versions alone, so a status change that leaves them alone
  // does not re-render every pane.
  const versionKey = CLI_KINDS.map((c) => snapshot?.agents[c]?.installed ?? "").join(" ");
  const versions = useMemo(() => {
    const installed = versionKey.split(" ");
    return Object.fromEntries(CLI_KINDS.map((c, i) => [c, installed[i] || undefined])) as Partial<
      Record<CliKind, string>
    >;
  }, [versionKey]);
  const paneValue = useMemo(
    () => ({
      pendingRestart,
      versions,
      stopped,
      prompted,
      openDialog,
      takePendingRestart,
      settle,
      clearPrompt,
      checkQuietly,
    }),
    [
      pendingRestart,
      versions,
      stopped,
      prompted,
      openDialog,
      takePendingRestart,
      settle,
      clearPrompt,
      checkQuietly,
    ],
  );
  return (
    <Ctx.Provider value={value}>
      <PaneCtx.Provider value={paneValue}>{children}</PaneCtx.Provider>
    </Ctx.Provider>
  );
}
