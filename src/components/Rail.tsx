// Left rail: every registered project as a collapsible group of worktree lanes, in
// registry order. Any number of groups can be open; opening one is a peek, and clicking a
// lane focuses that worktree (switching project when it belongs to another one). Each lane
// has a status-light block carrying the worktree's state colour; only a blocking,
// ball-in-your-court state (waiting on a permission prompt) switches it to its attention
// colour and pulses (static glow under reduced-motion). A finished worktree reads as steady
// green — the dock badge and notification carry that signal instead. The active project's
// header looks selected, and the focused lane carries the rail's single accent focus bar.

import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { Activity, HelpCircle, Settings } from "lucide-react";
import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { formatBytes, formatCpu, formatUptime } from "../lib/format";
import type { CliKind, DaemonInfo } from "../lib/ipc";
import { daemonRestart, daemonStatus } from "../lib/ipc";
import { laneMenuAction, type WorktreeMenuItem, worktreeMenuItems } from "../lib/laneState";
import type { PendingAction } from "../lib/pendingIntent";
import { isMac, isWindows, modKeyLabel } from "../lib/platform";
import {
  applyExpanded,
  collapseOthers,
  ensureExpanded,
  expandAll,
  forget,
  isStoredExpanded,
  toggle,
} from "../lib/railExpanded";
import type { GroupModel } from "../lib/railGroups";
import { canRestartNow, showRailRestart } from "../lib/recovery";
import { useDaemonStats, useRecovery } from "../lib/sessions";
import { removeProject } from "../lib/settings";
import { useLocalStorageJson } from "../lib/useLocalStorageState";
import { usePrefersReducedMotion } from "../lib/usePrefersReducedMotion";
import { useConfirmDialog } from "./ConfirmDialog";
import { RestartDaemonDialog } from "./RestartDaemonDialog";
import { RailProjects } from "./rail/RailProjects";
import { useToast } from "./Toast";

// ── DaemonCard ─────────────────────────────────────────────────────────────────

/**
 * The Connected-state rail daemon card: a compact two-row readout of the herdr daemon —
 * identity + uptime on top, CPU / memory / live-session count below. Replaces the old
 * tooltip-only green dot. Mounted ONLY while Connected (by DaemonStatusBadge), so the
 * `daemon://stats` subscription in {@link useDaemonStats} is naturally scoped and ends on
 * disconnect.
 *
 * Numbers use tabular figures (see `.rail-daemon-card` in App.css) and fixed-width value
 * slots so they don't jitter as they tick. Before the first sample arrives, CPU/MEM render
 * `—` placeholders at the same widths (no layout shift).
 */
function DaemonCard({
  daemonInfo,
  fallbackUptime,
}: {
  daemonInfo: DaemonInfo;
  fallbackUptime: string;
}) {
  const stats = useDaemonStats(true);

  // The stats card is herdr-only Until the
  // first sample resolves (or while the stats report not running), fall back to the muted
  // tooltip dot rather than a card with placeholder numbers.
  if (!stats?.running) {
    return (
      <div
        className="rail-daemon-badge rail-daemon-connected"
        title={`Daemon running — ${daemonInfo.session_count} session${daemonInfo.session_count === 1 ? "" : "s"} · PID ${daemonInfo.pid ?? "—"} · up ${fallbackUptime}`}
      >
        <span className="dot online" style={{ opacity: 0.4 }} />
      </div>
    );
  }

  const version = stats.version ?? null;
  // Prefer the live sampled uptime; fall back to the status-derived value (herdr's
  // started_at is null today, so fallbackUptime is usually "—" until the first sample).
  const uptime =
    stats?.uptime_secs != null ? formatUptime(stats.uptime_secs * 1000) : fallbackUptime;
  const cpu = stats?.cpu_percent != null ? formatCpu(stats.cpu_percent) : "—";
  const mem = stats?.mem_bytes != null ? formatBytes(stats.mem_bytes) : "—";
  const count = daemonInfo.session_count;
  const exactMem = stats?.mem_bytes != null ? `${stats.mem_bytes.toLocaleString()} bytes` : "—";

  return (
    <div className="rail-daemon-badge-wrap">
      <div
        className="rail-daemon-card"
        title={`herdr daemon${version ? ` ${version}` : ""} — ${count} session${count === 1 ? "" : "s"} · PID ${daemonInfo.pid ?? "—"} · up ${uptime} · mem ${exactMem}`}
      >
        <div className="rail-daemon-card-row">
          <span className="dot online" />
          <span className="rail-daemon-name">herdr{version ? ` ${version}` : ""}</span>
          <span className="rail-daemon-uptime" aria-label={`uptime ${uptime}`}>
            up {uptime}
          </span>
        </div>
        <div className="rail-daemon-card-row rail-daemon-metrics">
          <span className="rail-daemon-metric">
            <span className="rail-daemon-metric-label">CPU</span>
            <span className="rail-daemon-metric-value">{cpu}</span>
          </span>
          <span className="rail-daemon-metric">
            <span className="rail-daemon-metric-label">MEM</span>
            <span className="rail-daemon-metric-value">{mem}</span>
          </span>
          <span className="rail-daemon-sessions">{count} sess</span>
        </div>
      </div>
    </div>
  );
}

// ── DaemonStatusBadge ─────────────────────────────────────────────────────────

/**
 * Rail-footer daemon status indicator. Renders nothing when NeverStarted (pre-
 * connect). Shows the daemon info card when Connected and an offline badge (with the
 * backend's reason, when it gave one) when Disconnected.
 *
 * Placement: just above the Settings gear button.
 */
export function DaemonStatusBadge({ daemonInfo }: { daemonInfo: DaemonInfo }) {
  const { state } = daemonInfo;

  // Pre-connect: render nothing.
  if (state === "NeverStarted") {
    return null;
  }

  // Connected: the daemon info card.
  if (state === "Connected") {
    const uptime = daemonInfo.started_at
      ? formatUptime(Date.now() - daemonInfo.started_at * 1000)
      : "—";

    // Connected: the daemon info card. Mounted only on this branch, so its 2s stats poll is scoped to the Connected state.
    return <DaemonCard daemonInfo={daemonInfo} fallbackUptime={uptime} />;
  }

  // Disconnected: herdr is unreachable. The backend's reason (why it is unavailable and how to
  // fix it) replaces the generic label when set.
  if (state === "Disconnected") {
    return <OfflineCard daemonInfo={daemonInfo} />;
  }

  return null;
}

/**
 * The offline card: the click-to-retry-status badge with the reason, and a Restart button
 * beside it (a sibling, so no control is nested in another) when a restart can help. The
 * button asks first, with the dialog Settings uses: "offline" can also be a status probe that
 * timed out over a server whose agents are still working. It stays disabled ("Restarting…")
 * for as long as the restart runs, which recovery reports, not just for the request. A failed
 * restart toasts like Settings does; the card keeps the reason, which the backend re-emits.
 */
function OfflineCard({ daemonInfo }: { daemonInfo: DaemonInfo }) {
  const { showToast } = useToast();
  const recovery = useRecovery();
  const [confirming, setConfirming] = useState(false);
  const [restarting, setRestarting] = useState(false);
  const mounted = useRef(true);
  useEffect(
    () => () => {
      mounted.current = false;
    },
    [],
  );
  const reason = daemonInfo.reason;

  const canRestart = canRestartNow(recovery, restarting);

  const restart = async () => {
    setConfirming(false);
    setRestarting(true);
    try {
      await daemonRestart();
    } catch (e) {
      showToast(`Could not restart daemon: ${String(e)}`);
    } finally {
      if (mounted.current) setRestarting(false);
    }
  };

  return (
    <div className="rail-daemon-offline">
      <div
        className="rail-daemon-badge rail-daemon-error"
        title={reason ?? "Daemon offline — click to retry"}
        role="button"
        tabIndex={0}
        onClick={() => {
          void daemonStatus();
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            void daemonStatus();
          }
        }}
      >
        <span className="dot offline" />
        <span className="rail-daemon-label">{reason ?? "Daemon offline"}</span>
      </div>
      {showRailRestart(daemonInfo) && (
        <button
          type="button"
          className="rail-daemon-restart"
          disabled={!canRestart}
          onClick={() => setConfirming(true)}
        >
          {canRestart ? "Restart" : "Restarting…"}
        </button>
      )}
      <RestartDaemonDialog
        open={confirming && canRestart}
        onOpenChange={setConfirming}
        sessionCount={daemonInfo.session_count}
        onConfirm={() => void restart()}
      />
    </div>
  );
}

// ── Menus ──────────────────────────────────────────────────────────────────────
// Right-click context menus for a worktree row and a project header. There's no native
// context-menu primitive installed (`@radix-ui/react-context-menu` is NOT a dependency) —
// both reuse the already-installed dropdown-menu package as a controlled, cursor-positioned
// menu, the same pattern DispatchMenu.tsx uses for its "Dispatch to…" popover (portal,
// controlled `open`, outside-click/Escape dismiss, shared item classNames). The trigger is
// an invisible 0×0 anchor pinned at the click coordinates instead of a visible button, so
// the menu pops up exactly where the user right-clicked rather than next to a fixed element.

const menuItemCls =
  "flex cursor-pointer items-center gap-2 rounded-chip px-2 py-1.5 text-[11.5px] outline-none data-[highlighted]:bg-bg-3 data-[disabled]:cursor-not-allowed data-[disabled]:opacity-45";
const menuContentCls =
  "z-50 min-w-[160px] rounded-card border border-line bg-bg-2 p-1 shadow-[0_8px_24px_rgba(0,0,0,.45)]";
const menuLabelCls =
  "max-w-[220px] truncate px-2 pb-1 pt-1 text-[10px] uppercase tracking-wide text-tx-3";
const menuSepCls = "my-1 h-px bg-line";

/** The invisible 0×0 trigger a menu anchors to, at viewport coordinates. */
function MenuAnchor({ x, y }: { x: number; y: number }) {
  return (
    <DropdownMenu.Trigger asChild>
      <button
        type="button"
        aria-hidden="true"
        tabIndex={-1}
        className="pointer-events-none fixed h-0 w-0 border-0 bg-transparent p-0"
        style={{ left: x, top: y }}
      />
    </DropdownMenu.Trigger>
  );
}

// A live session offers "Restart session", "Start fresh conversation", "Transfer to {other
// CLI}" and "Terminate session"; a terminated one offers only "Launch" and "Launch fresh"
// (`worktreeMenuItems`). A lane of a project other than the active one leads with "Focus",
// under a label naming the lane, and its actions switch to that project first.
type WorktreeMenuState = {
  name: string;
  /** Registry key of the lane's project. */
  projectKey: string;
  /** The lane's project was not the active one when the menu opened. Shapes the menu
   *  only; a pick re-checks against the current groups. */
  inactive: boolean;
  x: number;
  y: number;
  cli: CliKind;
  terminated: boolean;
};

const WORKTREE_ITEM_LABEL: Record<Exclude<WorktreeMenuItem, "transfer">, string> = {
  focus: "Focus",
  restart: "Restart session",
  fresh: "Start fresh conversation",
  terminate: "Terminate session",
  launch: "Launch",
  "launch-fresh": "Launch fresh",
};

function WorktreeMenu({
  state,
  onOpenChange,
  onPick,
  codexAvailable,
}: {
  state: WorktreeMenuState | null;
  onOpenChange: (open: boolean) => void;
  onPick: (state: WorktreeMenuState, item: WorktreeMenuItem, targetCli: CliKind) => void;
  codexAvailable: boolean;
}) {
  const targetCli: CliKind = state?.cli === "codex" ? "claude" : "codex";
  // Claude is never surfaced as unavailable here — it's the default agent and a
  // hard requirement of the app today (pane launches fall back to a bare `claude`
  // when no path is configured or discovered), so only a Codex *target* can be
  // disabled.
  const codexMissing = targetCli === "codex" && !codexAvailable;
  const items = worktreeMenuItems(state?.terminated ?? false, { inactive: state?.inactive });
  const item = (kind: WorktreeMenuItem) => {
    const off = kind === "transfer" && codexMissing;
    return (
      <Fragment key={kind}>
        <DropdownMenu.Item
          disabled={off}
          title={off ? "Codex isn't installed" : undefined}
          onSelect={() => {
            if (state && !off) onPick(state, kind, targetCli);
          }}
          className={`${menuItemCls} ${kind === "focus" ? "font-medium text-tx-1" : "text-tx-2"}`}
        >
          {kind === "transfer"
            ? `Transfer to ${targetCli === "codex" ? "Codex" : "Claude"}`
            : WORKTREE_ITEM_LABEL[kind]}
        </DropdownMenu.Item>
        {kind === "focus" && <DropdownMenu.Separator className={menuSepCls} />}
      </Fragment>
    );
  };
  return (
    <DropdownMenu.Root open={state !== null} onOpenChange={onOpenChange}>
      <MenuAnchor x={state?.x ?? 0} y={state?.y ?? 0} />
      <DropdownMenu.Portal>
        <DropdownMenu.Content align="start" sideOffset={2} className={menuContentCls}>
          {state?.inactive && (
            <DropdownMenu.Label
              className={menuLabelCls}
              title={`${state.projectKey} / ${state.name}`}
            >
              {state.projectKey} / {state.name}
            </DropdownMenu.Label>
          )}
          {items.map(item)}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

// The project header's menu (right-click on the header, or its ⋯ button).
type ProjectMenuState = {
  /** Registry key of the project. */
  key: string;
  x: number;
  y: number;
};

const REVEAL_LABEL = isMac
  ? "Reveal in Finder"
  : isWindows
    ? "Show in Explorer"
    : "Reveal in file manager";

function ProjectMenu({
  state,
  group,
  onOpenChange,
  onCloseAutoFocus,
  onNewWorktree,
  onReveal,
  onCollapseOthers,
  onExpandAll,
  onRemove,
}: {
  state: ProjectMenuState | null;
  /** The menu's project; null while closed or once the project is gone. */
  group: GroupModel | null;
  onOpenChange: (open: boolean) => void;
  onCloseAutoFocus: (e: Event) => void;
  onNewWorktree: (g: GroupModel) => void;
  onReveal: (g: GroupModel) => void;
  onCollapseOthers: (g: GroupModel) => void;
  onExpandAll: () => void;
  onRemove: (g: GroupModel) => void;
}) {
  const run = (fn: (g: GroupModel) => void) => () => {
    if (group) fn(group);
  };
  return (
    <DropdownMenu.Root open={state !== null && group !== null} onOpenChange={onOpenChange}>
      <MenuAnchor x={state?.x ?? 0} y={state?.y ?? 0} />
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="start"
          sideOffset={2}
          className={menuContentCls}
          onCloseAutoFocus={onCloseAutoFocus}
        >
          {group && (
            <>
              <DropdownMenu.Label className={menuLabelCls} title={group.summary.root}>
                {group.key}
              </DropdownMenu.Label>
              <DropdownMenu.Item
                onSelect={run(onNewWorktree)}
                className={`${menuItemCls} text-tx-2`}
              >
                New worktree…
              </DropdownMenu.Item>
              <DropdownMenu.Item onSelect={run(onReveal)} className={`${menuItemCls} text-tx-2`}>
                {REVEAL_LABEL}
              </DropdownMenu.Item>
              <DropdownMenu.Separator className={menuSepCls} />
              <DropdownMenu.Item
                onSelect={run(onCollapseOthers)}
                className={`${menuItemCls} text-tx-2`}
              >
                Collapse others
              </DropdownMenu.Item>
              <DropdownMenu.Item onSelect={onExpandAll} className={`${menuItemCls} text-tx-2`}>
                Expand all
              </DropdownMenu.Item>
              <DropdownMenu.Separator className={menuSepCls} />
              <DropdownMenu.Item
                disabled={group.isActive}
                title={group.isActive ? "Switch to another project first" : undefined}
                onSelect={run(onRemove)}
                className={`${menuItemCls} text-urgent data-[highlighted]:bg-urgent/10`}
              >
                Remove project…
              </DropdownMenu.Item>
            </>
          )}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

// ── Rail ──────────────────────────────────────────────────────────────────────

// Upper bound on waiting for a group body's open transition, so a missed `transitionend`
// never leaves the focused lane unrevealed.
const GROUP_OPEN_SAFETY_MS = 500;

// Whether `el` animates its `grid-template-rows` (the group body's open/close).
function animatesRows(el: HTMLElement): boolean {
  const cs = window.getComputedStyle(el);
  const props = cs.transitionProperty.split(",").map((p) => p.trim());
  const durations = cs.transitionDuration.split(",").map((d) => Number.parseFloat(d) || 0);
  return props.some(
    (p, i) => (p === "grid-template-rows" || p === "all") && durations[i % durations.length] > 0,
  );
}

/**
 * Run `fn` once `el`'s open transition has ended, so a lane inside has its full height:
 * on the next frame when there is no transition to wait for, and after
 * `GROUP_OPEN_SAFETY_MS` at the latest. Returns a cancel.
 */
function afterGroupOpens(el: HTMLElement | null, fn: () => void): () => void {
  if (!el || !animatesRows(el)) {
    const raf = window.requestAnimationFrame(fn);
    return () => window.cancelAnimationFrame(raf);
  }
  const finish = () => {
    cancel();
    fn();
  };
  const onEnd = (e: TransitionEvent) => {
    if (e.target === el && e.propertyName === "grid-template-rows") finish();
  };
  const timer = window.setTimeout(finish, GROUP_OPEN_SAFETY_MS);
  const cancel = () => {
    el.removeEventListener("transitionend", onEnd);
    window.clearTimeout(timer);
  };
  el.addEventListener("transitionend", onEnd);
  return cancel;
}

export function Rail({
  groups,
  selected,
  onSelectLane,
  onOpenDiff,
  onLaneAction,
  onNewWorktree,
  onReorder,
  onAddProject,
  onOpenSettings,
  onOpenHelp,
  perfOpen,
  onTogglePerf,
  daemonInfo,
  hintVisible,
  codexAvailable,
}: {
  groups: GroupModel[];
  /** The active project's selected worktree. */
  selected: string | null;
  onSelectLane: (g: GroupModel, worktree: string) => void;
  onOpenDiff: (g: GroupModel, worktree: string) => void;
  /** A worktree-menu action on a lane; switches to the lane's project first unless it is
   *  the active one and ready. */
  onLaneAction: (g: GroupModel, worktree: string, action: PendingAction) => void;
  onNewWorktree: (g: GroupModel) => void;
  /** Persist a new project order (registry keys). */
  onReorder: (keys: string[]) => Promise<void>;
  onAddProject: () => void;
  onOpenSettings: () => void;
  onOpenHelp: () => void;
  /** Whether the performance overlay is showing (drives the toggle's pressed state). */
  perfOpen: boolean;
  onTogglePerf: () => void;
  daemonInfo: DaemonInfo;
  hintVisible: boolean;
  codexAvailable: boolean;
}) {
  // Right-click context menu: which worktree row it's anchored to, and the
  // click coordinates it should pop up at. Null = closed.
  const [menu, setMenu] = useState<WorktreeMenuState | null>(null);
  // The project header menu, and the element to hand focus back to when it closes (the
  // ⋯ button that opened it; null for a right-click).
  const [projectMenu, setProjectMenu] = useState<ProjectMenuState | null>(null);
  const projectMenuReturn = useRef<HTMLElement | null>(null);
  const { showToast } = useToast();
  const { confirmDialog } = useConfirmDialog();
  const reduceMotion = usePrefersReducedMotion();
  const lanesRef = useRef<HTMLDivElement>(null);

  // Open groups, persisted as registry keys; null = never stored (first run).
  const [stored, setStored] = useLocalStorageJson<string[] | null>(
    "keitora:railExpanded",
    null,
    isStoredExpanded,
  );
  const expanded = useMemo(() => new Set(stored ?? []), [stored]);
  const updateExpanded = useCallback(
    (update: (s: ReadonlySet<string>) => ReadonlySet<string>) =>
      setStored((prev) => applyExpanded(prev, update)),
    [setStored],
  );

  const activeKey = groups.find((g) => g.isActive)?.key ?? null;
  const activeOpen = activeKey !== null && expanded.has(activeKey);

  // First run: open the active project's group.
  useEffect(() => {
    if (stored === null && activeKey !== null) setStored([activeKey]);
  }, [stored, activeKey, setStored]);

  // Open the active project's group when the active project changes. The first key seen
  // only primes the ref, so a reload does not re-open a group the user collapsed.
  const prevActiveKey = useRef<string | null>(null);
  useEffect(() => {
    if (activeKey === null) return;
    const prev = prevActiveKey.current;
    prevActiveKey.current = activeKey;
    if (prev !== null && prev !== activeKey) updateExpanded((s) => ensureExpanded(s, activeKey));
  }, [activeKey, updateExpanded]);

  // Keep the focused lane in view as the selection or active project changes. Only when
  // its group is open: a collapsed group's lanes have no height to scroll to. A group that
  // just opened is still animating, so wait for it to finish.
  const prevActiveOpen = useRef(activeOpen);
  // biome-ignore lint/correctness/useExhaustiveDependencies: selected/activeKey are the triggers
  useEffect(() => {
    const justOpened = activeOpen && !prevActiveOpen.current;
    prevActiveOpen.current = activeOpen;
    if (!activeOpen) return;
    const reveal = () =>
      lanesRef.current
        ?.querySelector(".rail-group.active .lane-row.sel")
        ?.scrollIntoView({ block: "nearest", behavior: reduceMotion ? "auto" : "smooth" });
    if (justOpened && !reduceMotion) {
      return afterGroupOpens(
        lanesRef.current?.querySelector<HTMLElement>(".rail-group.active .rail-group-body") ?? null,
        reveal,
      );
    }
    const raf = window.requestAnimationFrame(reveal);
    return () => window.cancelAnimationFrame(raf);
  }, [selected, activeKey, activeOpen, reduceMotion]);

  // A worktree-menu pick. Every action goes through `onLaneAction` with the group as it
  // is now (not as it was when the menu opened), which runs it in place only once the
  // active project is ready and otherwise switches first, so a pick during a cold switch
  // can't land on the previous project's same-named worktree.
  const pickLaneItem = (m: WorktreeMenuState, item: WorktreeMenuItem, targetCli: CliKind) => {
    setMenu(null);
    const g = groups.find((x) => x.key === m.projectKey);
    if (!g) return;
    if (item === "focus") {
      onSelectLane(g, m.name);
      return;
    }
    const action = laneMenuAction(item, targetCli);
    if (action !== null) onLaneAction(g, m.name, action);
  };

  // Opens the project root in the OS file manager (the parent folder, with the project
  // selected). Reveal needs no path scope in the opener plugin's default capability.
  const reveal = async (g: GroupModel) => {
    try {
      const { revealItemInDir } = await import("@tauri-apps/plugin-opener");
      await revealItemInDir(g.summary.root);
    } catch (e) {
      showToast(String(e));
    }
  };

  // Registry only; the list refresh follows project://changed. The removed project's
  // expanded state is forgotten here rather than whenever the list lacks it: a project
  // can drop out of the list transiently (its keitora.toml mid-save).
  const remove = async (g: GroupModel) => {
    const ok = await confirmDialog({
      title: `Remove ${g.key}?`,
      body: "It is removed from Keitora's registry only. Files on disk are not touched.",
      confirmLabel: "Remove",
      danger: true,
    });
    if (!ok) return;
    try {
      await removeProject(g.key);
      updateExpanded((s) => forget(s, g.key));
      showToast(`Removed ${g.key} from the registry.`);
    } catch (e) {
      showToast(String(e));
    }
  };

  // Stable, so the memoized groups and lanes skip re-rendering when nothing they show changed.
  const toggleGroup = useCallback(
    (key: string) => updateExpanded((s) => toggle(s, key)),
    [updateExpanded],
  );
  const openLaneMenu = useCallback((g: GroupModel, name: string, x: number, y: number) => {
    const lane = g.lanes.find((l) => l.name === name);
    if (!lane) return;
    setMenu({
      name,
      projectKey: g.key,
      inactive: !g.isActive,
      x,
      y,
      cli: lane.cli,
      terminated: lane.terminated,
    });
  }, []);
  const openProjectMenu = useCallback(
    (g: GroupModel, x: number, y: number, from: HTMLElement | null) => {
      projectMenuReturn.current = from;
      setProjectMenu({ key: g.key, x, y });
    },
    [],
  );

  const menuGroup = projectMenu ? (groups.find((g) => g.key === projectMenu.key) ?? null) : null;

  return (
    <nav className="rail">
      <div className="rail-label-row">
        <div className="rail-label">Projects</div>
        <div className="rail-label-actions">
          {hintVisible && <span className="rail-cycle-hint">{modKeyLabel} ← →</span>}
          <button
            type="button"
            className="rail-add-btn"
            onClick={onAddProject}
            title="Add project"
            aria-label="Add project"
          >
            +
          </button>
        </div>
      </div>
      <div className="rail-lanes" ref={lanesRef}>
        <RailProjects
          groups={groups}
          expanded={expanded}
          menuKey={menuGroup?.key ?? null}
          selected={selected}
          hintVisible={hintVisible}
          reduceMotion={reduceMotion}
          onToggle={toggleGroup}
          onSelectLane={onSelectLane}
          onOpenDiff={onOpenDiff}
          onLaneContextMenu={openLaneMenu}
          onNewWorktree={onNewWorktree}
          onOpenMenu={openProjectMenu}
          onReorder={onReorder}
        />
      </div>
      <div className="rail-dock">
        <DaemonStatusBadge daemonInfo={daemonInfo} />
        <div className="rail-footer">
          <button className="rail-settings" onClick={onOpenSettings} title="Settings">
            <Settings size={14} />
            <span>settings</span>
          </button>
          <button
            className="rail-perf"
            aria-pressed={perfOpen}
            title={`Performance (${modKeyLabel}+Shift+P)`}
            onClick={onTogglePerf}
          >
            <Activity size={14} />
            <span>perf</span>
          </button>
          <button className="rail-shortcuts" onClick={onOpenHelp} title="Keyboard shortcuts (?)">
            <HelpCircle size={14} />
            <span>shortcuts</span>
          </button>
        </div>
      </div>
      <WorktreeMenu
        state={menu}
        onOpenChange={(open) => {
          if (!open) setMenu(null);
        }}
        onPick={pickLaneItem}
        codexAvailable={codexAvailable}
      />
      <ProjectMenu
        state={projectMenu}
        group={menuGroup}
        onOpenChange={(open) => {
          if (!open) setProjectMenu(null);
        }}
        onCloseAutoFocus={(e) => {
          // Back to the ⋯ button that opened it rather than the hidden anchor.
          const el = projectMenuReturn.current;
          projectMenuReturn.current = null;
          if (el?.isConnected) {
            e.preventDefault();
            el.focus();
          }
        }}
        onNewWorktree={onNewWorktree}
        onReveal={(g) => void reveal(g)}
        onCollapseOthers={(g) => updateExpanded(() => collapseOthers(g.key))}
        onExpandAll={() =>
          updateExpanded((s) =>
            expandAll(
              s,
              groups.map((x) => x.key),
            ),
          )
        }
        onRemove={(g) => void remove(g)}
      />
    </nav>
  );
}
