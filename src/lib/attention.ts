import { getCurrentWindow } from "@tauri-apps/api/window";
import { isPermissionGranted, requestPermission } from "@tauri-apps/plugin-notification";
import { useEffect, useRef, useState } from "react";
import type { View } from "../components/TopBar";
import { notifySession } from "./ipc";
import type { SessionState, WorktreeSession } from "./sessions";
import type { TaskWithDeps } from "./tasks";

// macOS silently routes a *foreground* app's OS notifications to Notification Center
// (no banner) unless the app registers a `willPresent` delegate — so when Keitora is
// focused we surface attention in-app instead. Windows/Linux DO show foreground OS
// notifications, so there we keep the OS path even when focused. Detect from the
// webview UA to stay frontend-only (avoids pulling in @tauri-apps/plugin-os + its
// Rust plugin/capability).
const IS_MACOS = typeof navigator !== "undefined" && /Mac/i.test(navigator.userAgent);

/**
 * Pure attention logic shared by the OS-banner and dock-badge features.
 *
 * A worktree "needs input" when the ball is in the user's court: it is blocked on a
 * permission prompt (`waiting`), or it just finished a turn and is sitting idle awaiting
 * the next instruction. The latter can't be read from the current state alone — `idle` is
 * also the resting/fresh state — so we latch it from the busy/waiting→idle transition.
 *
 * The React hook that drives these (`useAttention`) lives below the pure functions; it is a
 * thin wrapper so the decision logic here stays DOM-free and unit-testable.
 */

/**
 * Latch: did this worktree just finish working (was busy/waiting, now idle)?
 * - resumes (`→busy`) or disappears (`→offline`) clear the latch
 * - `busy→idle` / `waiting→idle` set it (the agent was working and is now awaiting you)
 * - a fresh idle (no prior, or `idle→idle`) leaves the latch unchanged
 */
export function latchIdleFromTurn(
  prev: SessionState | undefined,
  next: SessionState,
  was: boolean,
): boolean {
  if (next === "busy" || next === "offline") return false;
  if (next === "idle" && (prev === "busy" || prev === "waiting")) return true;
  return was;
}

/** The ball is in the user's court for this worktree. */
export function needsInput(state: SessionState, idleFromTurn: boolean): boolean {
  return state === "waiting" || (state === "idle" && idleFromTurn);
}

/**
 * Which kind of attention a worktree needs. Both drive the dock badge + notification; only
 * `waiting` also paints/animates the rail block (a `finished` worktree reads as plain idle).
 */
export type AttentionKind = "waiting" | "finished";

/**
 * Render-facing classification of a worktree's need for attention, split by the action
 * the user must take. Consistent with `needsInput`: returns non-null exactly when
 * `needsInput(state, idleFromTurn)` is true.
 *  - `"waiting"`  — blocked on a permission prompt (`state === "waiting"`).
 *  - `"finished"` — the agent finished its turn and is awaiting the next instruction
 *    (`state === "idle"`, latched from a busy/waiting→idle transition).
 */
export function classifyAttention(
  state: SessionState,
  idleFromTurn: boolean,
): AttentionKind | null {
  if (state === "waiting") return "waiting";
  if (state === "idle" && idleFromTurn) return "finished";
  return null;
}

/** Shallow value-equality for the attention map, to avoid pointless re-renders. */
function attentionMapsEqual(a: Map<string, AttentionKind>, b: Map<string, AttentionKind>): boolean {
  if (a.size !== b.size) return false;
  for (const [k, v] of a) if (b.get(k) !== v) return false;
  return true;
}

/**
 * The single worktree the user is actively looking at. Grid view renders many panes, but
 * only `selected` has focus; board view shows no terminal at all and so doesn't count as
 * "focused", else notifications for the selected worktree would be silently suppressed.
 */
export function isWorktreeFocused(view: View, selected: string | null, worktree: string): boolean {
  return (view === "focus" || view === "grid") && selected === worktree;
}

/** Suppress a banner only for the worktree the user is actively viewing. */
export function shouldSuppressNotification(
  appHasFocus: boolean,
  view: View,
  selected: string | null,
  worktree: string,
): boolean {
  return appHasFocus && isWorktreeFocused(view, selected, worktree);
}

/**
 * Per-worktree acknowledgement: has the user "seen" this worktree's current need for input?
 * An acknowledged worktree is excluded from the dock badge and from banner firing until it
 * re-arms. This is what lets the badge clear by simply viewing a finished worktree.
 * - new turn (`→busy`) or gone (`→offline`) → re-arm (false)
 * - no longer needs input → drop (false)
 * - the user is looking at it (`focused`) → acknowledged (true)
 * - otherwise unchanged
 */
export function nextAcknowledged(
  prevAck: boolean,
  state: SessionState,
  needsInputNow: boolean,
  focused: boolean,
): boolean {
  if (state === "busy" || state === "offline") return false;
  if (!needsInputNow) return false;
  if (focused) return true;
  return prevAck;
}

/** True when the active project changed and attention tracking must reset+reseed. */
export function projectChanged(prev: string | null, next: string | null): boolean {
  return prev !== next;
}

/**
 * Drive the two attention signals off the live session map:
 *  - fire a native banner when a NON-focused worktree newly needs input, and
 *  - keep the dock badge equal to the count of worktrees that currently need input.
 *
 * `needsInput` for an idle worktree depends on whether it just finished a turn, which the
 * snapshot can't express, so we latch it per worktree across renders (`latchRef`). A worktree
 * the user views is "acknowledged" (`ackRef`) and drops out of the badge until it re-arms on
 * its next turn — that is how the badge clears without an explicit dismiss.
 *
 * Returns the per-worktree `AttentionKind` map consumed by the rail. Only a `waiting` entry
 * paints/animates the rail block (a permission prompt — ball in your court now); a `finished`
 * entry is carried in the map but the rail renders it as plain idle (steady green), since its
 * signal already lives in the dock badge + notification — derived from the *same* latch so the
 * two can never disagree. The map is referentially stable (only a real change of kinds produces
 * a new identity, via `attentionMapsEqual`). Note the rail is NOT acknowledgement-gated: viewing
 * a worktree clears the badge but the lane keeps its colour.
 *
 * @param sessions    - live map from useSessions() (Map<worktreeName, WorktreeSession>)
 * @param tasks       - live list from useTasks().tasks (may be null before first load)
 * @param view        - current view ("board" | "focus" | "grid")
 * @param selected    - currently selected worktree name, or null
 * @param appFocused       - whether the Keitora window has OS focus (from useAppFocus)
 * @param showInAppBanner  - emit an in-app banner (used when Keitora is focused, since macOS
 *                           hides a foreground app's OS notification banners)
 * @param activeProject    - the currently active project name (or null); when this changes
 *                           all tracking refs are reset so no stale latch bleeds across.
 */
export function useAttention(
  sessions: Map<string, WorktreeSession>,
  tasks: TaskWithDeps[] | null,
  view: View,
  selected: string | null,
  appFocused: boolean,
  showInAppBanner: (worktree: string, body: string) => void,
  activeProject: string | null = null,
): Map<string, AttentionKind> {
  // Previous state per worktree; null until the first non-empty snapshot is seeded.
  const prevRef = useRef<Map<string, SessionState> | null>(null);
  // idleFromTurn latch per worktree (see latchIdleFromTurn).
  const latchRef = useRef<Map<string, boolean>>(new Map());
  // Worktrees the user has acknowledged by viewing (see nextAcknowledged).
  const ackRef = useRef<Set<string>>(new Set());
  // Tracks the last seen active project so we can detect a switch.
  const prevProjectRef = useRef<string | null>(null);
  // Resolve the OS permission once; concurrent fires share the in-flight promise so we
  // never pop two permission dialogs.
  const permRef = useRef<Promise<boolean> | null>(null);
  // Per-worktree attention kind for rendering, kept referentially stable across renders.
  const [attention, setAttention] = useState<Map<string, AttentionKind>>(new Map());

  // biome-ignore lint/correctness/useExhaustiveDependencies: mount-only attention tracker; all refs and stable setters intentionally excluded
  useEffect(() => {
    const latch = latchRef.current;
    const acknowledged = ackRef.current;

    // ── Project switch: reset tracking so no stale latch bleeds across projects ──
    // Fall through so the existing seed/empty branch runs and reseeds for the new project.
    if (projectChanged(prevProjectRef.current, activeProject)) {
      prevProjectRef.current = activeProject;
      prevRef.current = null;
      latch.clear();
      acknowledged.clear();
    }

    // Commit a freshly-computed kinds map, preserving identity when nothing changed.
    const commit = (kinds: Map<string, AttentionKind>) =>
      setAttention((prev) => (attentionMapsEqual(prev, kinds) ? prev : kinds));

    // Recompute the dock badge: worktrees that need input AND aren't acknowledged.
    // undefined/0 clears it.
    const updateBadge = () => {
      let count = 0;
      for (const [name, s] of sessions) {
        if (needsInput(s.state, latch.get(name) ?? false) && !acknowledged.has(name)) count++;
      }
      void getCurrentWindow().setBadgeCount(count > 0 ? count : undefined);
    };

    // ── No sessions ──────────────────────────────────────────────────────────
    // Initial load, or the set emptied (e.g. project switch). Clear the badge so
    // a stale count can't linger, and reset tracking so the next non-empty
    // snapshot re-seeds without firing banners for pre-existing states.
    if (sessions.size === 0) {
      void getCurrentWindow().setBadgeCount(undefined);
      prevRef.current = null;
      latch.clear();
      acknowledged.clear();
      commit(new Map());
      return;
    }

    // ── Seed on first load ───────────────────────────────────────────────────
    // Record states and reset latches. A pre-existing `waiting` still badges (it genuinely
    // needs input), but we fire no banners on launch — only on later transitions. With the
    // latch at false, only `waiting` classifies as attention here (a pre-existing finished
    // turn can't be known from a single snapshot — same as the badge).
    if (prevRef.current === null) {
      const seed = new Map<string, SessionState>();
      const kinds = new Map<string, AttentionKind>();
      for (const [name, s] of sessions) {
        seed.set(name, s.state);
        latch.set(name, false);
        const kind = classifyAttention(s.state, false);
        if (kind) kinds.set(name, kind);
      }
      prevRef.current = seed;
      updateBadge();
      commit(kinds);
      return;
    }

    const prev = prevRef.current;

    const ensurePermission = (): Promise<boolean> => {
      if (permRef.current === null) {
        permRef.current = (async () => {
          if (await isPermissionGranted()) return true;
          return (await requestPermission()) === "granted";
        })();
      }
      return permRef.current;
    };

    const fire = async (worktreeName: string) => {
      // Resolve a human-readable body from the active task; fall back when unknown.
      // biome-ignore lint/style/noNonNullAssertion: worktreeName is known to be present in sessions: derived from sessions iteration
      const session = sessions.get(worktreeName)!;
      let body = "needs attention";
      if (session.current_task_id !== null && tasks !== null) {
        const task = tasks.find((t) => t.id === session.current_task_id);
        if (task) body = task.title;
      }

      // Hybrid routing. `fire` only runs for non-focused worktrees (suppressed upstream),
      // so an in-app banner here is always for a worktree the user is NOT viewing.
      // On macOS a foreground app's OS banner is swallowed, so show it in-app (no OS
      // permission needed); elsewhere the OS notification shows fine even when focused.
      if (appFocused && IS_MACOS) {
        showInAppBanner(worktreeName, body);
        return;
      }

      if (!(await ensurePermission())) return;
      // Through the backend so a click on the notification lands on this worktree.
      await notifySession(
        { project: activeProject ?? "", worktree: worktreeName },
        `Keitora — ${worktreeName}`,
        body,
      ).catch(() => {});
    };

    // ── Detect transitions ───────────────────────────────────────────────────
    const promises: Promise<void>[] = [];
    const kinds = new Map<string, AttentionKind>();
    for (const [name, s] of sessions) {
      const prevState = prev.get(name);

      // A worktree appearing after the seed (config add, incremental load) has no
      // prior state: record its first observed state as a baseline and don't fire —
      // a baseline is not a transition. It still counts toward the badge below.
      if (prevState === undefined) {
        prev.set(name, s.state);
        latch.set(name, false);
        acknowledged.delete(name);
        const kind = classifyAttention(s.state, false);
        if (kind) kinds.set(name, kind);
        continue;
      }

      const wasLatch = latch.get(name) ?? false;
      const before = needsInput(prevState, wasLatch);
      const nextLatch = latchIdleFromTurn(prevState, s.state, wasLatch);
      const now = needsInput(s.state, nextLatch);

      latch.set(name, nextLatch);
      prev.set(name, s.state);

      const kind = classifyAttention(s.state, nextLatch);
      if (kind) kinds.set(name, kind);

      // Acknowledge the worktree the user is actively viewing (and re-arm it on its
      // next turn). Suppression and acknowledgement share the `focused` predicate:
      // the worktree you are looking at neither badges nor banners.
      const focused = appFocused && isWorktreeFocused(view, selected, name);
      if (nextAcknowledged(acknowledged.has(name), s.state, now, focused)) {
        acknowledged.add(name);
      } else {
        acknowledged.delete(name);
      }

      if (
        !before &&
        now &&
        !shouldSuppressNotification(appFocused, view, selected, name) &&
        !acknowledged.has(name)
      ) {
        promises.push(fire(name));
      }
    }

    // Prune tracking for worktrees no longer present so a stale state/latch can't
    // resurface if the same name is later re-added.
    for (const name of prev.keys()) {
      if (!sessions.has(name)) prev.delete(name);
    }
    for (const name of latch.keys()) {
      if (!sessions.has(name)) latch.delete(name);
    }
    for (const name of acknowledged) {
      if (!sessions.has(name)) acknowledged.delete(name);
    }

    updateBadge();
    commit(kinds);

    // Fire-and-forget — these are short-lived; cleanup doesn't need to cancel them.
    if (promises.length > 0) {
      void Promise.all(promises);
    }
  }, [sessions, tasks, view, selected, appFocused, activeProject]);

  return attention;
}
