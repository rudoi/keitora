// Keitora app shell: a worktree rail + a Focus/Grid stage of embedded Claude
// terminals. Worktrees come from keitora.toml (via the backend) and update
// live. Every available worktree gets a persistent TerminalPane; the view
// controls which are on-screen.

import * as Tooltip from "@radix-ui/react-tooltip";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AddProjectDialog } from "./components/AddProjectDialog";
import { AttentionBannersProvider, useAttentionBanners } from "./components/AttentionBanners";
import { Board } from "./components/Board";
import { ConfirmDialogProvider, useConfirmDialog } from "./components/ConfirmDialog";
import { CreateWorktreeDialog } from "./components/CreateWorktreeDialog";
import { DiffView } from "./components/DiffView";
import { DispatchStepsEditor } from "./components/DispatchStepsEditor";
import { DispatchStrip } from "./components/DispatchStrip";
import { FontSizeHud, type FontSizeHudHandle } from "./components/FontSizeHud";
import { Inspector } from "./components/Inspector";
import { KeybindingHelp } from "./components/KeybindingHelp";
import { Onboarding } from "./components/Onboarding";
import { PerfOverlay } from "./components/PerfOverlay";
import { QuitConfirmModal } from "./components/QuitConfirmModal";
import { Rail } from "./components/Rail";
import { RecoveryBanner } from "./components/RecoveryBanner";
import { SettingsDrawer } from "./components/SettingsDrawer";
import { TerminalPane } from "./components/TerminalPane";
import { ToastProvider, useToast } from "./components/Toast";
import { TopBar, type View } from "./components/TopBar";
import { WorktreePicker } from "./components/WorktreePicker";
import { useAttention } from "./lib/attention";
import { dispatchSuggest, type Proposal } from "./lib/dispatch";
import {
  clampFontSize,
  getTerminalFontSize,
  setTerminalFontSize,
  TERM_FONT_DEFAULT,
} from "./lib/fontSize";
import {
  type CliKind,
  createWorktree,
  currentProject,
  injectHandoffSummary,
  listWorktrees,
  onProjectChanged,
  type ProjectSummary,
  previewClaudeTransfer,
  reorderProjects,
  takeXdgLeakNotice,
  transferWorktree,
} from "./lib/ipc";
import { type KeyCtx, useKeybindings } from "./lib/keybindings";
import { isPaneVisible } from "./lib/paneVisibility";
import {
  canConsume,
  intentForSwitch,
  type PendingAction,
  type PendingIntent,
  type SettleState,
  type SwitchOpts,
  shouldDrop,
} from "./lib/pendingIntent";
import { useProjectsOverview } from "./lib/projectsOverview";
import { buildGroups, type GroupModel, shareGroups } from "./lib/railGroups";
import { latestWins } from "./lib/railOrder";
import { nextRestartSignal, type RestartSignal } from "./lib/resumeNotice";
import { countsByProject } from "./lib/sessionCounts";
import {
  useDaemonSessions,
  useDaemonStatus,
  useRecovery,
  useSessions,
  useWorktreeActivity,
} from "./lib/sessions";
import { getSettings } from "./lib/settings";
import { switchProject, switchTarget } from "./lib/switchProject";
import { useTasks } from "./lib/tasks";
import {
  cssPointFromPhysical,
  FILE_DROP_EVENT,
  type PaneHit,
  paneAtPoint,
  paneElementAtPoint,
} from "./lib/terminalFileDrop";
import { useTheme } from "./lib/theme";
import { useAppFocus } from "./lib/useAppFocus";
import { useKeitoraCliInstall } from "./lib/useKeitoraCliInstall";
import { useLiveResource } from "./lib/useLiveResource";
import { useLocalStorageState } from "./lib/useLocalStorageState";
import { useModifierHint } from "./lib/useModifierHint";
import { useProjectList } from "./lib/useProjectList";
import {
  activeProject,
  emptyWarmState,
  rememberSelection,
  rememberView,
  sameWorktrees,
  setActive,
  type WarmState,
  warmProject,
} from "./lib/warmProjects";
import { xdgLeakNoticeBody } from "./lib/xdgNotice";
import "./App.css";

// Inner component so useToast() can access the ToastProvider above it.
// Drops persist one at a time and in order, so a slow save can't land after a later one.
const persistProjectOrder = latestWins(reorderProjects);

function AppInner() {
  // First-run nudge to put the `keitora` CLI on PATH (so session hooks work anywhere).
  useKeitoraCliInstall();

  // Own the app theme for its lifetime: applies data-theme to <html> and, in System
  // mode, tracks the OS light/dark preference live. SettingsDrawer shares this state.
  useTheme();

  // Worktrees from task-config.json; refetched live on `worktrees://changed`.
  const {
    data: worktrees,
    error,
    reload: reloadWorktrees,
  } = useLiveResource(listWorktrees, "worktrees://changed");
  const [selected, setSelected] = useState<string | null>(null);
  const [view, setView] = useState<View>("board");
  // Worktree whose full-window diff view is open (bare name; active project implicit), or
  // null. Mounted as an overlay inside `.body` so the terminal panes stay alive underneath.
  const [diffTarget, setDiffTarget] = useState<string | null>(null);
  // The pane a file drag is currently hovering, or null. Drives the drop
  // affordance only — the drop itself is handed to the pane via FILE_DROP_EVENT.
  const [dropTarget, setDropTarget] = useState<PaneHit | null>(null);

  // Warm-project registry: every project visited this app session stays
  // "warm" — its TerminalPanes stay mounted + streaming so switching back is a pure
  // visibility flip, not an unmount/remount/re-attach. The daemon already keeps all
  // projects' PTYs alive; this is the frontend side. A ref mirrors it for the switch
  // handler (which must not take `warm` as a dep).
  const [warm, setWarm] = useState<WarmState>(emptyWarmState);
  const warmRef = useRef(warm);
  warmRef.current = warm;
  // The active project's worktrees, sourced from its registry entry so the whole
  // active-project UI (panes, rail, keymap) flips instantly on a warm switch; falls
  // back to the live `worktrees` before the project has been warmed.
  const activeWorktrees = useMemo(
    () => activeProject(warm)?.worktrees ?? worktrees ?? [],
    [warm, worktrees],
  );
  const [selectedTaskId, setSelectedTaskId] = useState<number | null>(null);
  // Persisted *preference* for the inspector's collapsed state.
  const [collapsedPref, setCollapsedPref] = useLocalStorageState(
    "keitora:inspectorCollapsed",
    false,
  );
  // Effective collapsed state the Inspector actually renders. Selecting a card opens
  // the inspector transiently (without touching the saved preference); deselecting
  // reverts to the preference. A manual toggle updates both (below).
  const [inspectorCollapsed, setInspectorCollapsed] = useState(collapsedPref);
  // Keep the latest preference reachable from the selection effect without making it a
  // dependency (depending on it would re-open the inspector whenever the pref changes).
  const collapsedPrefRef = useRef(collapsedPref);
  collapsedPrefRef.current = collapsedPref;
  useEffect(() => {
    setInspectorCollapsed(selectedTaskId !== null ? false : collapsedPrefRef.current);
  }, [selectedTaskId]);
  const [helpOpen, setHelpOpen] = useState(false);
  const [perfOpen, setPerfOpen] = useState(false);
  const [wtOpen, setWtOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // Add-project dialog — single instance lifted here so both the rail and
  // SettingsDrawer share one mount (avoids duplicate flow state).
  const [addProjectOpen, setAddProjectOpen] = useState(false);
  // Whether a `codex` binary was found on this machine — gates the Rail's
  // "Transfer to Codex" menu item. Re-fetched on project switch below (once
  // `activeProjectName` exists), matching the SettingsDrawer re-seed trigger.
  const [codexAvailable, setCodexAvailable] = useState(false);

  // Registry project list — the single source for both the rail's project groups
  // and the keymap's cycle order (project.prev/next), so the visible group order and
  // the keyboard cycle order can never disagree. One
  // fetch + one subscription via useProjectList.
  const registryProjects = useProjectList();
  // Worktrees + session states of every registered project, for the rail's groups of
  // projects other than the active one (null until the first load).
  const projectsOverview = useProjectsOverview();
  // Mirrors `registryProjects` for the switch handler, which resolves a registry key to
  // its summary without taking the list as a dep.
  const registryProjectsRef = useRef(registryProjects);
  registryProjectsRef.current = registryProjects;

  // Terminal font-size shortcuts. The value is read/written imperatively (via
  // fontSize.ts) rather than subscribed here, so a shortcut never re-renders the
  // app shell — only the mounted TerminalPanes react, through their own
  // useTerminalFontSize subscription. The HUD owns its own visibility/timer; we
  // just flash it via its imperative handle. Both callbacks are dependency-free and
  // therefore stable, so they don't churn the keymap ctx.
  const fontHudRef = useRef<FontSizeHudHandle>(null);
  const adjustFontSize = useCallback((delta: number) => {
    const next = clampFontSize(getTerminalFontSize() + delta);
    setTerminalFontSize(next);
    fontHudRef.current?.show(next);
  }, []);
  const resetFontSize = useCallback(() => {
    setTerminalFontSize(TERM_FONT_DEFAULT);
    fontHudRef.current?.show(TERM_FONT_DEFAULT);
  }, []);

  // Board hands up a stable callback that toggles the selected card's dispatch
  // menu; the `board.dispatch` (d) binding calls it. Held in a ref so it stays
  // out of the keymap ctx deps until it's actually registered.
  const openCardMenuRef = useRef<() => void>(() => {});
  const registerOpenCardMenu = useCallback((fn: () => void) => {
    openCardMenuRef.current = fn;
  }, []);

  // Per-worktree restart signal: the Rail's "Restart session" context-menu item
  // (and a completed CLI transfer) bumps the entry keyed
  // `${project}/${worktree}` — matching the TerminalPane React-key convention below
  // (`:704`) — and the matching mounted pane's prop-driven effect reacts by running
  // its own restart(). The Rail only ever shows the ACTIVE project's worktrees
  // (`:593`), so the target project is always the current `warm.active`, read via `warmRef`
  // so this stays fully stable (no deps) rather than churning on every switch.
  const [restartNonces, setRestartNonces] = useState<Map<string, RestartSignal>>(() => new Map());
  // Shared by onRestartSession and onTransfer — bumps the nonce the mounted
  // TerminalPane's prop-driven effect reacts to (kill → spawn → attach). Extracted
  // so a completed transfer converges on identical restart semantics to a manual
  // restart, rather than attempting its own respawn path (contract C2 — never a
  // backend same-SessionKey respawn).
  // `project` may be passed explicitly by callers that captured the active project
  // BEFORE an await (e.g. onTransfer — the active project can change during the
  // transfer); it defaults to the current active project for the synchronous
  // restart path (onRestartSession), which never awaits.
  // `fresh` asks the pane to start a new, empty conversation instead of resuming.
  const bumpRestartNonce = useCallback(
    (worktree: string, project?: string | null, fresh = false) => {
      // Use the explicitly-passed project (even if null) when provided; otherwise the
      // current active project for the synchronous restart path.
      const proj = project === undefined ? warmRef.current.active : project;
      const key = `${proj}/${worktree}`;
      setRestartNonces((prev) => {
        const next = new Map(prev);
        next.set(key, nextRestartSignal(next.get(key), fresh));
        return next;
      });
    },
    [],
  );
  const onRestartSession = useCallback(
    (worktree: string, opts?: { fresh?: boolean }) => {
      // Make the target pane visible FIRST. The restart confirm surface lives inside the
      // pane's own container, which is `display:none` while inactive — so restarting a
      // non-focused worktree would set `confirmKill` on a hidden pane and appear to do
      // nothing. Focus it (same as the Rail's onSelect) so the confirm renders where
      // the user can see it; these setState calls batch with the nonce bump into one render.
      setSelected(worktree);
      setView("focus");
      bumpRestartNonce(worktree, undefined, opts?.fresh ?? false);
    },
    [bumpRestartNonce],
  );

  // Per-worktree terminate / launch signals from the Rail's menu, keyed and bumped like
  // `restartNonces` (the terminate signal's `fresh` is unused). The matching mounted pane
  // does the work: it confirms a terminate of a live session, and only it knows the
  // terminal size a launch spawns at.
  const [terminateNonces, setTerminateNonces] = useState<Map<string, RestartSignal>>(
    () => new Map(),
  );
  const [launchNonces, setLaunchNonces] = useState<Map<string, RestartSignal>>(() => new Map());
  // Like onRestartSession: focus the target pane first so its confirm / overlay renders
  // where the user can see it.
  const onTerminateSession = useCallback((worktree: string) => {
    setSelected(worktree);
    setView("focus");
    const key = `${warmRef.current.active}/${worktree}`;
    setTerminateNonces((prev) => new Map(prev).set(key, nextRestartSignal(prev.get(key), false)));
  }, []);
  const onLaunchSession = useCallback((worktree: string, opts?: { fresh?: boolean }) => {
    setSelected(worktree);
    setView("focus");
    const key = `${warmRef.current.active}/${worktree}`;
    const fresh = opts?.fresh ?? false;
    setLaunchNonces((prev) => new Map(prev).set(key, nextRestartSignal(prev.get(key), fresh)));
  }, []);

  // Project-level gate: "loading" = not yet fetched; null = no project → onboarding;
  // ProjectSummary = active project → main app.
  const [project, setProject] = useState<ProjectSummary | null | "loading">("loading");
  // Error from resolving the active project (distinct from the worktree-fetch
  // error owned by useLiveResource).
  const [projectError, setProjectError] = useState<string | null>(null);
  // Name of the project being switched to, while the switch is in flight. Drives a
  // full-screen blocking overlay so the user knows not to act mid-transition. Set
  // when a switch starts; cleared once the BACKEND confirms the new active project
  // (loadProject resolves with the target) — NOT on the optimistic flip.
  const [switchingTo, setSwitchingTo] = useState<string | null>(null);
  // A lane focus/action parked until a project switch settles (see `pendingIntent.ts`).
  // State, not a ref, so the consuming effect below re-runs as the switch progresses.
  const [pending, setPending] = useState<PendingIntent | null>(null);

  // The active project's name (null → onboarding / loading). Drives project-scoped
  // live data (tasks + sessions) so a stale response from a project the user switched
  // away from can't corrupt the active board/lights.
  const activeProjectName = typeof project === "object" && project !== null ? project.name : null;
  // The active project's registry key. `project.name` is the resolved `[project].name`,
  // while the registry list (and so the keymap's cycle order) is keyed by registry key;
  // the two are usually equal but need not be.
  const activeProjectKey = useMemo(
    () =>
      registryProjects.find((p) => p.resolved_name === activeProjectName)?.name ??
      activeProjectName,
    [registryProjects, activeProjectName],
  );
  // The diff view resolves its worktree against the ACTIVE project, so a project switch would
  // leave it showing the old project's diff keyed to a name that no longer matches. Close it.
  // biome-ignore lint/correctness/useExhaustiveDependencies: close on project change only.
  useEffect(() => {
    setDiffTarget(null);
  }, [activeProjectName]);
  // Backend kind of the active project ("sqlite" | "github"), driving the board's
  // safety-net poll cadence (github polls far less often — its push is dead so the
  // gated poll is primary, but each probe is cheap).
  const activeBackend = typeof project === "object" && project !== null ? project.backend : null;
  // Whether Codex is installed — gates the Rail's "Transfer to Codex" menu
  // item. Codex availability is machine-global (PATH-based discovery), so probe once
  // on mount; the Settings drawer re-seeds it via getSettings() whenever it opens.
  useEffect(() => {
    void getSettings()
      .then((s) => setCodexAvailable(s.user.codex_available))
      .catch(() => {}); // non-fatal — the transfer item just stays disabled
  }, []);
  // Shared task data for the Board + Inspector (one list, one live subscription),
  // scoped to the active project.
  const {
    tasks,
    reservations: dispatched,
    reload: reloadTasks,
  } = useTasks(activeProjectName, activeBackend);
  // Bumps whenever the task list reloads, so the open inspector refetches in step.
  const [taskRev, setTaskRev] = useState(0);
  // biome-ignore lint/correctness/useExhaustiveDependencies: mount-only increment; stable setter intentionally omitted
  useEffect(() => setTaskRev((r) => r + 1), [tasks]);

  // Load the active project from the registry. null → onboarding.
  const loadProject = useCallback(async () => {
    try {
      const p = await currentProject();
      setProjectError(null);
      setProject(p); // null → onboarding; ProjectSummary → main app
    } catch (e) {
      setProjectError(String(e));
      setProject(null);
    }
  }, []);
  // Live worktree session states (idle/busy/waiting/offline) for Rail + Board dots.
  // `sessions` is cache-seeded for instant lights on switch-back; `liveSessions`
  // is the uncached live map that drives notifications (so a stale cache seed can't
  // manufacture a false busy→idle "finished" banner on switch-back).
  const { sessions, liveSessions, reload: reloadSessions } = useSessions(activeProjectName);
  // All-projects worktree activity (stage · branch · build bar · working), polled once and
  // kept warm across project switches so the active project's lanes never blank out on a
  // switch. Not gated on the active project — the backend enumerates every project.
  const worktreeActivity = useWorktreeActivity();
  // Mirrors `warmRef` above: lets the injection poll below (§ onTransfer) read the
  // latest live session state from inside a plain async function without making it
  // a dependency of anything.
  const liveSessionsRef = useRef(liveSessions);
  liveSessionsRef.current = liveSessions;
  // Live daemon connection status — used by Rail, TerminalPane, and SettingsDrawer.
  const daemonInfo = useDaemonStatus();
  // Live herdr server recovery state: drives the recovery banner.
  const recovery = useRecovery();
  // Live list of all daemon sessions across all projects (refreshes on sessions://changed).
  const daemonSessions = useDaemonSessions();
  // Whether the Keitora window has OS focus (drives banner routing + acknowledgement).
  const appFocused = useAppFocus();
  // In-app attention banner; clicking it jumps to (and so acknowledges) the worktree.
  const { showAttention } = useAttentionBanners();
  const showInAppBanner = useCallback(
    (worktree: string, body: string) => {
      showAttention({
        title: `Keitora — ${worktree}`,
        body,
        onClick: () => {
          setView((v) => (v === "board" ? "grid" : v)); // ensure a terminal view
          setSelected(worktree); // focus → effect re-runs → acknowledged → badge clears
        },
      });
    },
    [showAttention],
  );
  // ── Per-project live session counts ────────────────────────────────────────
  // Map from project name → count of live daemon sessions in that project,
  // derived from the live `useDaemonSessions()` hint list. Drives the
  // rail project-group session counts; refreshes whenever the hint list changes.
  const projectSessionCounts = useMemo(() => countsByProject(daemonSessions), [daemonSessions]);

  // Dispatched reservations ((taskId, worktree) pairs already injected) now ride the
  // same single `board_snapshot` fetch as the board tasks (see `useTasks`), rather
  // than a second co-fired `dispatched_reservations` subscription.
  // Attention: banner non-focused worktrees that need input + dock-badge the count, and
  // surface the per-worktree attention kind for the rail's status block (waiting / finished).
  const attention = useAttention(
    liveSessions,
    tasks ?? null,
    view,
    selected,
    appFocused,
    showInAppBanner,
    activeProjectName,
  );

  // Load + subscribe to project changes (config watcher + project switcher).
  useEffect(() => {
    void loadProject();
    let cancelled = false;
    let un: (() => void) | undefined;
    void onProjectChanged(() => void loadProject()).then((u) => {
      if (cancelled) u();
      else un = u;
    });
    return () => {
      cancelled = true;
      un?.();
    };
  }, [loadProject]);

  // ── Dispatcher state ────────────────────────────────────────────────────────
  const [proposals, setProposals] = useState<Map<number, Proposal>>(new Map());
  const [suggestLoading, setSuggestLoading] = useState(false);
  const [suggestError, setSuggestError] = useState<string | null>(null);
  const { showToast } = useToast();
  const { confirmDialog } = useConfirmDialog();

  // Once per launch: explain that pane tools moved to the user's normal folders.
  // The backend marks the notice shown when it hands it out, so the dialog's
  // answer does not matter.
  // biome-ignore lint/correctness/useExhaustiveDependencies: run once on mount
  useEffect(() => {
    takeXdgLeakNotice()
      .then((dirs) => {
        if (dirs === null || dirs.length === 0) return;
        void confirmDialog({
          title: "Panes now use your normal tool folders",
          body: xdgLeakNoticeBody(dirs),
          confirmLabel: "Got it",
          cancelLabel: "Dismiss",
        });
      })
      .catch(() => {});
  }, []);

  // Best-effort Codex → Claude handoff-summary injection:
  // poll the worktree's live session state (the same `sessions_list`-derived
  // source the rail/board already subscribe to via `useSessions`) until it
  // reports `idle` — i.e. the freshly-restarted Claude session has finished
  // booting and is ready for input — then inject. Bounded (15s) and silently
  // gives up on timeout/error: this is a nice-to-have on top of a transfer that
  // already succeeded, never something the user should be blocked on or see fail.
  const injectHandoffWhenIdle = useCallback((worktree: string, project: string | null) => {
    const deadline = Date.now() + 15_000;
    const tick = () => {
      // Abandon quietly if the user switched projects since the transfer: both the
      // live-session map read below and the backend injection resolve the CURRENT
      // active project, so proceeding would target the wrong project.
      if (warmRef.current.active !== project) return;
      const state = liveSessionsRef.current.get(worktree)?.state;
      if (state === "idle") {
        void injectHandoffSummary(worktree).catch(() => {});
        return;
      }
      if (Date.now() < deadline) {
        window.setTimeout(tick, 400);
      }
      // Timeout: give up quietly. The summary file is left in place (best-effort —
      // not removed), so nothing is lost; the user can retry the transfer's
      // restart manually if they notice the injection never landed.
    };
    // A short initial delay lets the restart's kill→spawn actually begin before
    // the first check — otherwise a session that was already idle a moment ago
    // (the pre-transfer state, not yet reflecting the restart) could pass the
    // check instantly and inject into the dying old session.
    window.setTimeout(tick, 800);
  }, []);

  // Which worktree is mid-transfer, and to which CLI. Drives the
  // matching TerminalPane's "Transferring to Claude/Codex…" indicator in place of
  // the recovery overlay (Codex→Claude: the backend kills the live PTY BEFORE the
  // summary step, so the mounted pane would otherwise flash "session ended") and
  // suppresses that pane's confirm-kill prompt on the restart-nonce bump (the user
  // already confirmed the transfer in the rail dialog below).
  const [transferring, setTransferring] = useState<{ worktree: string; to: CliKind } | null>(null);
  // Tracks whether the transferring worktree's session has been observed offline
  // (or absent) since `transferring` was last set — see the clearing effect below.
  const transferSawOfflineRef = useRef(false);
  // Clears `transferring` once the transferred worktree's session is confirmed live
  // again post-restart, using the SAME live-session source the rail/
  // injectHandoffWhenIdle key on (`liveSessions`/`liveSessionsRef`). A worktree's
  // OLD session is typically still live (non-offline) at the instant `transferring`
  // is set — before `transferWorktree`'s kill/respawn ever runs — so clearing on the
  // first non-offline reading would dismiss the indicator immediately, before the
  // transfer even starts. Instead this only clears once the session has been
  // observed offline (or absent from the map) at least once since `transferring`
  // was set, and THEN reports live again — i.e. an actual post-kill respawn. A
  // bounded fallback timeout (below) guards against a session that never comes
  // back (e.g. a downstream spawn failure) so the indicator can't get stuck.
  useEffect(() => {
    if (!transferring) {
      transferSawOfflineRef.current = false;
      return;
    }
    const state = liveSessions.get(transferring.worktree)?.state;
    if (state === undefined || state === "offline") {
      transferSawOfflineRef.current = true;
      return;
    }
    if (transferSawOfflineRef.current) {
      setTransferring(null);
    }
  }, [transferring, liveSessions]);
  // Safety-net: never let "Transferring…" get stuck showing if the session never
  // reports live again after the observed offline gap above.
  useEffect(() => {
    if (!transferring) return;
    const timer = window.setTimeout(() => setTransferring(null), 20_000);
    return () => window.clearTimeout(timer);
  }, [transferring]);

  // Mirrors `onRestartSession`: focuses the target worktree first (the
  // confirm dialog / toast must render on a visible pane), then drives the
  // direction-specific confirm → transfer → restart sequence.
  // Confirmation always runs BEFORE any backend side effect (Risk 7 / C2).
  const onTransfer = useCallback(
    (worktree: string, targetCli: CliKind) => {
      setSelected(worktree);
      setView("focus");
      // Capture the active project NOW, before the transfer await. bumpRestartNonce
      // + injectHandoffWhenIdle below both key on the active project; if the user
      // switches projects during the await, reading it post-await would target the
      // wrong project.
      const activeProject = warmRef.current.active;
      const label = targetCli === "codex" ? "Codex" : "Claude";
      void (async () => {
        try {
          if (targetCli === "codex") {
            // Claude → Codex is a full, IRREVERSIBLE live import (it writes
            // Codex's ledger). Show the lock-free preview and require explicit
            // confirmation before anything imports.
            const preview = await previewClaudeTransfer(worktree);
            const ok = await confirmDialog({
              title: `Transfer ${worktree} to Codex?`,
              body:
                `Codex will import this Claude session and it cannot be undone:\n` +
                `• ${preview.filename} (modified ${preview.mtime})\n` +
                `“${preview.first_user_line}”\n\n` +
                `If this isn't the session you expect, cancel and check the worktree first.`,
              confirmLabel: "Transfer to Codex",
              danger: true,
            });
            if (!ok) return; // no side effects have happened yet
          } else {
            // Codex → Claude is a lossy SUMMARY handoff (Risk 7), not a full-
            // history transfer — the copy must say so, distinctly from the
            // Claude→Codex irreversible-import warning above.
            const ok = await confirmDialog({
              title: `Transfer ${worktree} to Claude?`,
              body:
                `Claude can't import a Codex thread directly, so this carries a generated ` +
                `summary (goal, decisions, next steps, open files) — not the full ` +
                `conversation. Detail will be lost.`,
              confirmLabel: "Transfer to Claude",
            });
            if (!ok) return;
          }
          showToast(`Transferring ${worktree} to ${label}…`);
          // Set AFTER the user confirms, BEFORE the backend transfer starts, so the
          // pane's overlay/confirm-kill suppression is in place
          // before `transferWorktree` can kill the live PTY (Codex→Claude) or the
          // restart-nonce bump can pop a confirm (Claude→Codex). Left set on
          // success — the clearing effect above dismisses it once the respawned
          // session is live again, not immediately here, so the indicator survives
          // across the respawn.
          setTransferring({ worktree, to: targetCli });
          await transferWorktree(worktree, targetCli);
          bumpRestartNonce(worktree, activeProject, false);
          showToast(`Transferred ${worktree} to ${label}.`);
          if (targetCli === "claude") {
            // Best-effort: inject the Codex→Claude handoff summary once the fresh
            // session is ready. No-ops (false) for a Claude→Codex transfer target
            // is impossible here (gated by the branch above) — this path only
            // ever runs after a Codex→Claude transfer.
            injectHandoffWhenIdle(worktree, activeProject);
          }
        } catch (e) {
          setTransferring(null);
          showToast(String(e));
        }
      })();
    },
    [bumpRestartNonce, showToast, confirmDialog, injectHandoffWhenIdle],
  );

  // Set of task ids that are currently active — used to gate the Rail chip.
  // Active task per worktree, keyed by claim owner (assigned_to). Drives the rail card's
  // task line so a worktree shows what it's working on even with no live session.
  const activeTaskByWorktree = useMemo(() => {
    const m = new Map<string, { id: number; title: string }>();
    for (const t of tasks ?? []) {
      if (t.status !== "active" || !t.assigned_to) continue;
      // A worktree should have one active task, but if two ever collide, prefer the
      // lowest id — matching the backend's `active_task_id` (ORDER BY id LIMIT 1) so the
      // rail can't disagree with it, instead of last-in-array-wins.
      const cur = m.get(t.assigned_to);
      if (!cur || t.id < cur.id) m.set(t.assigned_to, { id: t.id, title: t.title });
    }
    return m;
  }, [tasks]);

  // Whether the active project's rich sources (worktrees, sessions, attention, tasks)
  // belong to it yet; on a cold switch they still hold the previous project's until it warms.
  const activeReady = activeProjectName !== null && warm.active === activeProjectName;
  // One rail group per registered project, in registry order. Rebuilt on every activity
  // or session tick; `shareGroups` hands back the previous objects for whatever did not
  // change, so the memoized groups and lanes skip re-rendering.
  const prevRailGroups = useRef<GroupModel[] | null>(null);
  const railGroups = useMemo(() => {
    const next = shareGroups(
      prevRailGroups.current,
      buildGroups({
        projects: registryProjects,
        activeResolvedName: activeProjectName,
        activeReady,
        activeWorktrees,
        sessions,
        attention,
        activeTaskByWorktree,
        overview: projectsOverview,
        activity: worktreeActivity,
        liveCounts: projectSessionCounts,
        selected,
        maxDots: 8,
      }),
    );
    prevRailGroups.current = next;
    return next;
  }, [
    registryProjects,
    activeProjectName,
    activeReady,
    activeWorktrees,
    sessions,
    attention,
    activeTaskByWorktree,
    projectsOverview,
    worktreeActivity,
    projectSessionCounts,
    selected,
  ]);

  // Count of idle worktrees — pre-check before calling dispatch_suggest.
  const idleCount = Array.from(sessions.values()).filter((s) => s.state === "idle").length;

  const onSuggest = useCallback(async () => {
    if (suggestLoading) return;
    // Frontend pre-check: skip the subprocess if nothing is idle.
    if (idleCount === 0) {
      // DispatchStrip will show the "No idle worktrees" note via idleCount.
      return;
    }
    setSuggestLoading(true);
    setSuggestError(null);
    try {
      const list = await dispatchSuggest();
      const map = new Map<number, Proposal>();
      for (const p of list) {
        map.set(p.task_id, p);
      }
      setProposals(map);
    } catch (e) {
      const msg = String(e);
      setSuggestError(msg);
      showToast(msg);
    } finally {
      setSuggestLoading(false);
    }
  }, [suggestLoading, idleCount, showToast]);

  const onProposalSpent = useCallback((taskId: number) => {
    setProposals((prev) => {
      const next = new Map(prev);
      next.delete(taskId);
      return next;
    });
  }, []);

  // Run a lane action in the active project, through the same handlers as the rail's
  // worktree menu. Lane actions need a worktree; "new-worktree" is project-level.
  const runPendingAction = useCallback(
    (action: PendingAction, worktree: string | null) => {
      if (action.kind === "new-worktree") {
        setWtOpen(true);
        return;
      }
      if (worktree === null) return;
      switch (action.kind) {
        case "restart":
          onRestartSession(worktree, { fresh: action.fresh });
          break;
        case "transfer":
          onTransfer(worktree, action.to);
          break;
        case "terminate":
          onTerminateSession(worktree);
          break;
        case "launch":
          onLaunchSession(worktree, { fresh: action.fresh });
          break;
        case "diff":
          setDiffTarget(worktree);
          break;
      }
    },
    [onRestartSession, onTransfer, onTerminateSession, onLaunchSession],
  );
  // Read through a ref by the rail's lane handlers, so they stay stable as the session
  // handlers change.
  const runPendingActionRef = useRef(runPendingAction);
  runPendingActionRef.current = runPendingAction;

  // Shared optimistic project-switch path, by registry key. The rail passes the
  // known ProjectSummary; without one (Settings) it is looked up in the registry list,
  // so both get the instant optimistic flip. `opts` lets a lane click focus a worktree
  // and/or run a lane action once the switch settles, and silence the success toast.
  // Reconciliation flows through the existing project://changed → loadProject() chain
  // (driven by set_active_project's config write), so no explicit refresh is needed here.
  // biome-ignore lint/correctness/useExhaustiveDependencies: stable callback; project/showToast/setProject captured via refs pattern, not deps
  const handleSwitchProject = useCallback(
    async (name: string, summary?: ProjectSummary, opts?: SwitchOpts) => {
      // `name` is the registry key, which is what the backend switches by. App's active
      // project, the warm registry and the overlay are keyed by the resolved name.
      const target = switchTarget(name, summary, registryProjectsRef.current);
      const resolved = target.resolvedName;
      // A new switch supersedes whatever an earlier one parked.
      setPending(null);
      if (typeof project === "object" && project?.name === resolved) {
        // Already active: nothing to switch. Park the lane focus/action; the consuming
        // effect below applies it at once when this project is settled, or once a switch
        // to it still in flight settles.
        setPending(intentForSwitch(resolved, opts, false));
        return;
      }
      // Warm target: the project's panes are already mounted + streaming, so
      // switching is a pure visibility flip — flip the active project NOW (no overlay),
      // sourcing panes/rail from its stored registry entry. The overlay is gated to
      // not-yet-warm (cold) targets only, so this is instant.
      // Snapshot warm state so a failed switch can revert the optimistic warm flip
      // below — switchProject() reverts its own `setProject`, but does not own ours.
      const prevWarm = warmRef.current;
      const isWarm = resolved in prevWarm.projects;
      if (isWarm) {
        setWarm((w) => setActive(w, resolved));
        if (opts?.select) {
          // A lane was picked: focus it instead of the remembered focus.
          setSelected(opts.select);
          setView("focus");
        } else {
          // Restore this project's remembered focus + view so a switch-back
          // feels like we never left it, rather than resetting to the first worktree.
          const entry = prevWarm.projects[resolved];
          if (entry) {
            setSelected(entry.selected);
            setView(entry.view);
          }
        }
      }
      // A warm target's selection was applied above (parking it would hold it until the
      // backend confirms the switch, showing the remembered focus first). Cold targets get
      // their selection only through this slot: until the target is warm the rail still
      // shows the previous project's worktrees, and the "keep selected valid" effect
      // would reset an early selection.
      setPending(intentForSwitch(resolved, opts, isWarm));
      setSwitchingTo(resolved);
      // Safety net: never trap the user behind the (cold-only) overlay if a reseed hangs.
      window.setTimeout(() => setSwitchingTo((cur) => (cur === resolved ? null : cur)), 4000);
      try {
        const ok = await switchProject(name, {
          setProject,
          currentProject: project,
          showToast,
          summary: target.optimistic,
          silent: opts?.silent,
        });
        if (!ok) {
          // Backend rejected the switch. switchProject already reverted `project`; undo
          // our optimistic warm flip + focus restore so panes/rail don't strand on the
          // target while project/backend stay on the previous one. Drop this switch's
          // parked intent (but not one a later switch parked).
          setPending((cur) => (cur?.project === resolved ? null : cur));
          if (isWarm) {
            setWarm(prevWarm);
            const back = prevWarm.active ? prevWarm.projects[prevWarm.active] : undefined;
            if (back) {
              setSelected(back.selected);
              setView(back.view);
            }
          }
          return;
        }
        // Reload project + worktrees + sessions. reloadSessions() lands the new project's
        // status lights now instead of waiting on the 3s background poll. Once
        // `switchingTo` clears below, the settled-warm effect seeds/refreshes this
        // project's registry entry from the freshly-reloaded worktrees (keeping any
        // remembered focus/view). Panes never key new-project/old-worktree because each
        // project's panes render from its OWN stored worktree list.
        await Promise.all([loadProject(), reloadWorktrees(), reloadSessions()]);
      } finally {
        setSwitchingTo((cur) => (cur === resolved ? null : cur));
      }
    },
    [project, setProject, showToast, loadProject, reloadWorktrees, reloadSessions],
  );

  // Rail lane paths. A lane of the active project acts directly; any other project's lane
  // (or the active one's while its switch is still settling) goes through the project
  // switch, which focuses the worktree and runs the action once the switch settles. Stable
  // (changing values are read through refs), so the memoized rail groups and lanes hold.
  const handleSwitchProjectRef = useRef(handleSwitchProject);
  handleSwitchProjectRef.current = handleSwitchProject;
  const activeReadyRef = useRef(activeReady);
  activeReadyRef.current = activeReady;
  const selectLane = useCallback((g: GroupModel, w: string) => {
    if (g.isActive && activeReadyRef.current) {
      setSelected(w);
      setView("focus");
    } else {
      void handleSwitchProjectRef.current(g.key, g.summary, { select: w, silent: true });
    }
  }, []);
  // Focus a worktree picked in the performance overlay, switching project when needed.
  const focusPerfSession = useCallback(
    (project: string, worktree: string) => {
      const g = railGroups.find((x) => x.resolvedName === project);
      if (g) selectLane(g, worktree);
    },
    [railGroups, selectLane],
  );
  const closePerf = useCallback(() => setPerfOpen(false), []);
  const togglePerf = useCallback(() => setPerfOpen((v) => !v), []);
  const openLaneDiff = useCallback((g: GroupModel, w: string) => {
    if (g.isActive && activeReadyRef.current) setDiffTarget(w);
    else
      void handleSwitchProjectRef.current(g.key, g.summary, {
        select: w,
        silent: true,
        action: { kind: "diff" },
      });
  }, []);
  const laneAction = useCallback((g: GroupModel, w: string, action: PendingAction) => {
    if (g.isActive && activeReadyRef.current) runPendingActionRef.current(action, w);
    else void handleSwitchProjectRef.current(g.key, g.summary, { select: w, silent: true, action });
  }, []);
  const newWorktreeIn = useCallback((g: GroupModel) => {
    if (g.isActive && activeReadyRef.current) setWtOpen(true);
    else
      void handleSwitchProjectRef.current(g.key, g.summary, {
        silent: true,
        action: { kind: "new-worktree" },
      });
  }, []);

  // Stable wrapper for keybinding ctx: switches project by name, passing the known
  // ProjectSummary from the registry list (enables warm optimistic flip).
  const switchToProject = useCallback(
    (name: string) => {
      const s = registryProjects.find((p) => p.name === name);
      void handleSwitchProject(name, s);
    },
    [registryProjects, handleSwitchProject],
  );

  // ── Central keymap ──────────────────────────────────────────────────────────
  // Ordered list of available worktree names; drives Cmd+Left/Right cycling.
  const availableWorktrees = useMemo(
    () => activeWorktrees.filter((w) => w.status === "available").map((w) => w.name),
    [activeWorktrees],
  );
  // Ordered project names for the cycle chords. Memoized so the keymap ctx stays
  // stable across project-list refreshes that don't change the names.
  const projectNames = useMemo(() => registryProjects.map((p) => p.name), [registryProjects]);
  const keyCtx: KeyCtx = useMemo(
    () => ({
      view,
      selected,
      availableWorktrees,
      hasSelectedCard: selectedTaskId !== null,
      toggleHelp: () => setHelpOpen((o) => !o),
      togglePerf,
      openDiff: (name: string) => setDiffTarget(name),
      setView,
      setSelected,
      onSuggest: () => void onSuggest(),
      openCardMenu: () => openCardMenuRef.current(),
      adjustFontSize,
      resetFontSize,
      projectNames,
      activeProjectName: activeProjectKey ?? "",
      switchToProject,
    }),
    [
      view,
      selected,
      availableWorktrees,
      selectedTaskId,
      onSuggest,
      adjustFontSize,
      resetFontSize,
      projectNames,
      activeProjectKey,
      switchToProject,
      togglePerf,
    ],
  );
  useKeybindings(keyCtx);

  // Hold-modifier hint overlay: reveals number badges on rail rows.
  const hintVisible = useModifierHint();

  // Keep the active project warm: seed/refresh its registry entry from the
  // live worktree list once project + worktrees have SETTLED. Never mid-switch
  // (`switchingTo` set), when `worktrees` may still belong to the previous project —
  // warming with that stale list would mis-key the new project's panes. On a
  // cold switch this seeds the project (defaults focus/view); on a refresh it keeps
  // the remembered focus/view and just updates summary + worktrees.
  useEffect(() => {
    if (typeof project !== "object" || project === null) return;
    if (!worktrees) return;
    if (switchingTo) return;
    setWarm((w) => {
      // Skip the re-warm (and its render cascade) when nothing changed — the 30s live
      // resync hands back a fresh `worktrees` array with identical contents.
      const existing = w.projects[project.name];
      if (existing && w.active === project.name && sameWorktrees(existing.worktrees, worktrees)) {
        return w;
      }
      return warmProject(w, { summary: project, worktrees });
    });
  }, [project, worktrees, switchingTo]);

  // OS file drops: a dragged file's paths are visible only to the webview
  // drag-drop event — Tauri strips them from HTML5 drop events — so there is one
  // app-level listener rather than per-pane handlers. It hit-tests the cursor and
  // hands the paths to the pane underneath (FILE_DROP_EVENT), which does the actual
  // injection so the drop obeys that pane's input gate. Registered once for the app's
  // lifetime; `disposed` covers an unmount racing the listener's own registration.
  useEffect(() => {
    let un: (() => void) | undefined;
    let disposed = false;
    void getCurrentWebview()
      .onDragDropEvent((ev) => {
        const p = ev.payload;
        if (p.type === "leave") {
          setDropTarget(null);
          return;
        }
        // `position` is physical pixels; the DOM hit-test needs CSS pixels.
        const { x, y } = cssPointFromPhysical(p.position, window.devicePixelRatio);
        if (p.type === "drop") {
          setDropTarget(null);
          if (p.paths.length === 0) return;
          paneElementAtPoint(x, y)?.dispatchEvent(
            new CustomEvent(FILE_DROP_EVENT, { detail: { paths: p.paths } }),
          );
          return;
        }
        // "enter" / "over" — track which pane would receive the drop. These fire on
        // every pointer move, and paneAtPoint allocates a fresh object each call, so
        // keep the previous reference when the pane hasn't changed: otherwise every
        // move re-renders the whole stage (and every mounted xterm) mid-drag.
        const hit = paneAtPoint(x, y);
        setDropTarget((cur) =>
          cur?.project === hit?.project && cur?.name === hit?.name ? cur : hit,
        );
      })
      .then((f) => {
        if (disposed) f();
        else un = f;
      })
      .catch(() => {
        /* no webview drag-drop here (non-Tauri context) — drops just do
           nothing, rather than throwing an unhandled rejection at the window. */
      });
    return () => {
      disposed = true;
      un?.();
    };
  }, []);

  // Keep the selected worktree valid as the (active project's) worktree list changes.
  useEffect(() => {
    if (!worktrees) return;
    const avail = activeWorktrees.filter((w) => w.status === "available").map((w) => w.name);
    setSelected((cur) => (cur && avail.includes(cur) ? cur : (avail[0] ?? null)));
  }, [activeWorktrees, worktrees]);

  // Persist the active project's focus + view into its registry entry, so a
  // later switch-back can restore them. The registry is the per-project memory; global
  // `selected`/`view` stay the live driver for the active project. Skips mid-switch
  // (those values still belong to the previous project until the switch settles).
  useEffect(() => {
    if (switchingTo) return;
    const active = warm.active;
    if (!active) return;
    const entry = warmRef.current.projects[active];
    // Skip when the registry already matches — avoids a setWarm + render on every
    // selection/view change that doesn't actually move the stored value.
    if (!entry || (entry.selected === selected && entry.view === view)) return;
    setWarm((w) =>
      active in w.projects ? rememberView(rememberSelection(w, active, selected), active, view) : w,
    );
  }, [selected, view, warm.active, switchingTo]);

  // Fire a parked lane focus/action once its project switch has settled (the target is
  // active, the switch is done, and its panes are mounted), or drop it when the switch
  // ended elsewhere (the failure toast already told the user). Declared after the two
  // effects above so its selection wins over their reset in the same commit.
  useEffect(() => {
    if (!pending) return;
    const settle: SettleState = {
      activeProjectName,
      switchingTo,
      warmActive: warm.active,
      warmHas: pending.project in warm.projects,
    };
    if (canConsume(pending, settle)) {
      if (pending.worktree !== null) {
        setSelected(pending.worktree);
        setView("focus");
      }
      if (pending.action) runPendingAction(pending.action, pending.worktree);
      setPending(null);
    } else if (shouldDrop(pending, settle)) {
      setPending(null);
    }
  }, [pending, activeProjectName, switchingTo, warm, runPendingAction]);

  // 1. Project still loading.
  if (project === "loading") {
    return (
      <div className="keitora shell-msg">
        <div className="msg-card">Loading…</div>
      </div>
    );
  }

  // 2. No project configured → onboarding.
  if (project === null) {
    return (
      <div className="keitora shell-msg">
        <Onboarding onComplete={() => void loadProject()} />
      </div>
    );
  }

  // 3. Error (config or worktree fetch).
  if (error || projectError) {
    return (
      <div className="keitora shell-msg">
        <div className="msg-card">
          <b>Config error</b>
          <p className="mono">{projectError ?? error}</p>
          <button
            className="pane-btn"
            onClick={() => {
              void loadProject();
              void reloadWorktrees();
            }}
          >
            Retry
          </button>
        </div>
      </div>
    );
  }

  // 4. Worktrees still loading.
  if (worktrees === null) {
    return (
      <div className="keitora shell-msg">
        <div className="msg-card">Loading worktrees…</div>
      </div>
    );
  }

  // 5. No worktrees (valid but empty project — offer discovery).
  if (worktrees.length === 0) {
    return (
      <div className="keitora shell-msg">
        <WorktreePicker
          projectRoot={project.root}
          onDone={() => {
            void reloadWorktrees();
          }}
        />
      </div>
    );
  }

  // 6. Main app view.
  // `selected` is the actor for claim/done — the worktree the user has focused.
  // Fallback: if no selection yet (selected === null), pass "" to the backend.
  // The board's "mine" check (assigned_to === actor) will match no tasks, so
  // Done/Unclaim buttons won't appear until the user picks a lane.
  const actor = selected;

  const worktreeNames = activeWorktrees.map((w) => w.name);

  return (
    <div className="keitora">
      <TopBar
        view={view}
        onView={setView}
        projectLabel={activeProjectKey}
        worktreeLabel={activeReady ? selected : undefined}
      />
      <RecoveryBanner info={recovery} />
      <div className="body" style={{ position: "relative" }}>
        <Rail
          groups={railGroups}
          selected={selected}
          onSelectLane={selectLane}
          onOpenDiff={openLaneDiff}
          onLaneAction={laneAction}
          onNewWorktree={newWorktreeIn}
          onReorder={persistProjectOrder}
          onAddProject={() => setAddProjectOpen(true)}
          onOpenSettings={() => setSettingsOpen(true)}
          onOpenHelp={() => setHelpOpen(true)}
          perfOpen={perfOpen}
          onTogglePerf={togglePerf}
          daemonInfo={daemonInfo}
          hintVisible={hintVisible}
          codexAvailable={codexAvailable}
        />
        <SettingsDrawer
          open={settingsOpen}
          onOpenChange={setSettingsOpen}
          onSwitchProject={handleSwitchProject}
          onAddProject={() => setAddProjectOpen(true)}
          dispatchEditor={(steps, onChange) => (
            <DispatchStepsEditor steps={steps} onChange={onChange} />
          )}
          codexDispatchEditor={(steps, onChange) => (
            <DispatchStepsEditor steps={steps} onChange={onChange} />
          )}
        />
        {/* Single add-project dialog mount — shared by the rail + SettingsDrawer. */}
        <AddProjectDialog open={addProjectOpen} onOpenChange={setAddProjectOpen} />
        <CreateWorktreeDialog
          open={wtOpen}
          onOpenChange={setWtOpen}
          onSubmit={async (name) => {
            await createWorktree(name);
            setSelected(name);
            setView("focus");
          }}
        />

        {/* Board surface — center stage + inspector. Only mounted in board view. */}
        {view === "board" && (
          <>
            {/* DispatchStrip lives above the Board, sibling to it in App. */}
            <div className="flex min-h-0 min-w-0 flex-1 flex-col bg-bg-0">
              <DispatchStrip
                proposals={proposals}
                loading={suggestLoading}
                error={suggestError}
                idleCount={idleCount}
                onSuggest={() => void onSuggest()}
                selectedId={selectedTaskId}
                onSelect={setSelectedTaskId}
                tasks={tasks ?? []}
                sessions={sessions}
              />
              <Board
                tasks={tasks ?? []}
                worktreeNames={worktreeNames}
                sessions={sessions}
                actor={actor}
                selectedId={selectedTaskId}
                onSelect={setSelectedTaskId}
                reload={reloadTasks}
                registerOpenCardMenu={registerOpenCardMenu}
                dispatched={dispatched ?? []}
              />
            </div>
            <Inspector
              taskId={selectedTaskId}
              revision={taskRev}
              actor={actor}
              reload={reloadTasks}
              onClose={() => setSelectedTaskId(null)}
              sessions={sessions}
              proposals={proposals}
              onProposalSpent={onProposalSpent}
              collapsed={inspectorCollapsed}
              onToggleCollapsed={() => {
                // Derive both from the same next value so a manual toggle stays
                // consistent even when auto-open has desynced effective vs. pref
                // (effective open while the saved pref is collapsed).
                const next = !inspectorCollapsed;
                setInspectorCollapsed(next);
                setCollapsedPref(next);
              }}
            />
          </>
        )}

        {/* Terminal stage — always mounted so PTY sessions survive view switches;
            hidden (not unmounted) while the board is showing. */}
        <div className={`stage ${view}`} style={view === "board" ? { display: "none" } : undefined}>
          {/* Panes for EVERY warm project are mounted at once; only the active
              project's in-view panes are visible (active=true → display:flex), the rest
              are display:none but still mounted + streaming. A project only enters
              `warm.projects` once it has been activated (the settled-warm effect), so
              panes always first-mount while their project is visible — term.open() never
              runs on a hidden host. Each project's panes render from its OWN stored
              worktree list, so a switch can never key new-project/old-worktree. */}
          {Object.values(warm.projects).flatMap((wp) =>
            wp.worktrees
              .filter((w) => w.status === "available")
              .map((w) => (
                <TerminalPane
                  key={`${wp.summary.name}/${w.name}`}
                  name={w.name}
                  path={w.path}
                  projectName={wp.summary.name}
                  active={wp.summary.name === warm.active && isPaneVisible(view, selected, w.name)}
                  daemonInfo={daemonInfo}
                  recovery={recovery}
                  isDropTarget={
                    dropTarget?.project === wp.summary.name && dropTarget?.name === w.name
                  }
                  restartNonce={restartNonces.get(`${wp.summary.name}/${w.name}`)?.n ?? 0}
                  restartFresh={restartNonces.get(`${wp.summary.name}/${w.name}`)?.fresh ?? false}
                  transferringTo={transferring?.worktree === w.name ? transferring.to : null}
                  terminated={w.terminated}
                  terminateNonce={terminateNonces.get(`${wp.summary.name}/${w.name}`)?.n ?? 0}
                  launchNonce={launchNonces.get(`${wp.summary.name}/${w.name}`)?.n ?? 0}
                  launchFresh={launchNonces.get(`${wp.summary.name}/${w.name}`)?.fresh ?? false}
                />
              )),
          )}
        </div>
        {/* Diff view — full-window overlay inside .body (covers the stage, TopBar stays).
            The terminal stage underneath is never unmounted/hidden, so PTYs + xterm sizing
            survive; DiffView is opaque (bg-bg-1) and grabs focus. */}
        {diffTarget && (
          <div className="absolute inset-0 z-40">
            {/* key on the worktree so switching targets remounts with fresh selection/state */}
            <DiffView key={diffTarget} worktree={diffTarget} onClose={() => setDiffTarget(null)} />
          </div>
        )}
        {/* Performance overlay: floats over the stage (never the rail); mounted only while
            open so its sort and expansion state reset on close. */}
        {perfOpen && <PerfOverlay onClose={closePerf} onFocusSession={focusPerfSession} />}
      </div>
      {helpOpen && <KeybindingHelp onClose={() => setHelpOpen(false)} />}
      <FontSizeHud ref={fontHudRef} />
      {switchingTo && !(switchingTo in warm.projects) && (
        <div
          className="switch-overlay"
          role="alertdialog"
          aria-busy="true"
          aria-label={`Switching to ${switchingTo}`}
        >
          <div className="switch-overlay-card">
            <div className="switch-spinner" aria-hidden="true" />
            <div className="switch-overlay-title">Switching to {switchingTo}…</div>
            <div className="switch-overlay-sub">Loading project</div>
          </div>
        </div>
      )}
    </div>
  );
}

function App() {
  return (
    <ToastProvider>
      <ConfirmDialogProvider>
        <AttentionBannersProvider>
          <Tooltip.Provider>
            <AppInner />
            <QuitConfirmModal />
          </Tooltip.Provider>
        </AttentionBannersProvider>
      </ConfirmDialogProvider>
    </ToastProvider>
  );
}

export default App;
