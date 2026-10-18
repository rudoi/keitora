// One embedded terminal bound to a worktree's PTY. The xterm `Terminal` lives
// for the component's lifetime so the session + scrollback persist across view
// switches; the WebGL context is released while the pane is off-screen (bounding
// live GPU contexts) and we never fit/resize while hidden (avoids 0×0 corruption).

import type { UnlistenFn } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { type ITheme, Terminal } from "@xterm/xterm";
import { useCallback, useEffect, useRef, useState } from "react";
import { classifyAttachError } from "../lib/attachErrors";
import { useTerminalFontSize } from "../lib/fontSize";
import { createFrameCoalescer, type FrameCoalescer } from "../lib/frameCoalescer";
import { appOwnsScroll } from "../lib/historySearch";
import {
  type CliKind,
  type DaemonInfo,
  daemonStatus,
  decodeBase64,
  MAX_SCROLL_LINES,
  onPtyResume,
  openTerminalFile,
  ptyAttach,
  ptyDetach,
  ptyKill,
  ptyRepaint,
  ptyResize,
  ptyScroll,
  ptyScrollOffset,
  ptySpawn,
  ptyWrite,
  type RecoveryInfo,
  recoveryFocus,
  recoveryStartFresh,
  type ScrollDirection,
  type ScrollSource,
  saveClipboardImage,
  sessionLaunch,
  sessionTerminate,
} from "../lib/ipc";
import {
  isFindChord,
  isGlobalAppChord,
  isScrollToLiveKey,
  paneScrollDirection,
} from "../lib/keybindings";
import { markPending, onFrame, stamp } from "../lib/latencyProbe";
import { createLiveTail, type LiveTail } from "../lib/liveTail";
import { createPaneAttachments } from "../lib/paneAttach";
import { createOutputGate, type OutputGate } from "../lib/paneOutput";
import {
  afterRecoveringRefusal,
  inputBlocked,
  nextPaneStatus,
  type Status,
} from "../lib/paneStatus";
import { isWindows } from "../lib/platform";
import { paneRecoveryState, recoveryItem, recoveryPaneMessage } from "../lib/recovery";
import {
  dimLine,
  restartConfirmCopy,
  resumeNoticeLine,
  terminateConfirmCopy,
} from "../lib/resumeNotice";
import {
  accumulateWheelLines,
  type Cell,
  clampPtySize,
  cursorCell,
  type Dims,
  dimsAreSane,
  dimsEqual,
  sizeToSend,
  wheelCell,
} from "../lib/termFit";
import { clipboardActionForKey, suppressNativePaste } from "../lib/terminalClipboard";
import { suppressesCompositionDoubleCommit } from "../lib/terminalComposition";
import {
  FILE_DROP_EVENT,
  type FileDropDetail,
  injectionTextForPaths,
  quotePathForInjection,
} from "../lib/terminalFileDrop";
import { installTerminalLinks } from "../lib/terminalLinks";
import { type ResolvedTheme, useResolvedTheme } from "../lib/theme";
import { type HistoryView, TerminalSearch } from "./TerminalSearch";
import { useToast } from "./Toast";
import "@xterm/xterm/css/xterm.css";

// Keitora ANSI palettes — the shell + Claude render in the app's own theme. One per
// resolved theme; the active one is chosen from useTheme() and swapped live when the
// theme changes (see the theme effect below). Keep in spirit with the CSS palettes in
// index.css / App.css.
const THEMES: Record<ResolvedTheme, ITheme> = {
  dark: {
    background: "#0a0e0d",
    foreground: "#e9efed",
    cursor: "#f0c665",
    cursorAccent: "#0a0e0d",
    selectionBackground: "rgba(240,198,101,.28)",
    black: "#151c1a",
    red: "#ff5d5d",
    green: "#46d18a",
    yellow: "#e9b13a",
    blue: "#5b9dff",
    magenta: "#b18cff",
    cyan: "#4fd0b4",
    white: "#9bb0aa",
    brightBlack: "#637872",
    brightRed: "#ff7b7b",
    brightGreen: "#6ee0a3",
    brightYellow: "#f5c462",
    brightBlue: "#7fb4ff",
    brightMagenta: "#c7a6ff",
    brightCyan: "#5fe6cf",
    brightWhite: "#e9efed",
  },
  light: {
    background: "#ffffff",
    foreground: "#14201d",
    cursor: "#9a6b12",
    cursorAccent: "#ffffff",
    selectionBackground: "rgba(240,198,101,.35)",
    black: "#14201d",
    red: "#c0392b",
    green: "#1f9d63",
    yellow: "#b5791a",
    blue: "#2f6fe0",
    magenta: "#8a4fd0",
    cyan: "#1a9e82",
    // ANSI "white" (7) is a mid gray on light backgrounds so default-light output stays
    // visible; foreground (dark) carries normal text. brightWhite (15) → darker for contrast.
    white: "#657b75",
    brightBlack: "#7b908a",
    brightRed: "#d83b3b",
    brightGreen: "#178a56",
    brightYellow: "#c67d10",
    brightBlue: "#1f5fd0",
    brightMagenta: "#7a3fc0",
    brightCyan: "#0f8a72",
    brightWhite: "#3a4a45",
  },
};

/**
 * Base64-encode a clipboard image for the save_clipboard_image IPC.
 *
 * FileReader rather than btoa(String.fromCharCode(...bytes)): the latter needs manual
 * chunking to avoid blowing the argument limit, and does the conversion on the UI
 * thread for what can be a multi-megabyte screenshot.
 */
function blobToBase64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      // readAsDataURL yields "data:<mime>;base64,<payload>" — we want the payload.
      const url = String(reader.result);
      resolve(url.slice(url.indexOf(",") + 1));
    };
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(blob);
  });
}

/** The history view while no terminal is wired up: does nothing. */
const IDLE_HISTORY_VIEW: HistoryView = {
  scrollBy: async () => false,
  returnToLive: () => {},
};

// The fit addon's proposal for its terminal's host; undefined when it cannot measure.
function proposeFit(fit: FitAddon): Dims | undefined {
  try {
    return fit.proposeDimensions();
  } catch {
    return undefined;
  }
}

// Size `term` to an already-measured proposal (a no-op when it already is).
function sizeTerminal(term: Terminal, d: Dims) {
  if (!dimsEqual(d, { rows: term.rows, cols: term.cols })) term.resize(d.cols, d.rows);
}

/** The per-pane status union (declared with its reducer in `lib/paneStatus`). */
export type { Status };

export function TerminalPane({
  name,
  path,
  projectName,
  active,
  daemonInfo,
  recovery,
  isDropTarget = false,
  restartNonce,
  restartFresh = false,
  transferringTo,
  terminated,
  terminateNonce,
  launchNonce,
  launchFresh = false,
}: {
  name: string;
  path: string;
  projectName: string;
  active: boolean;
  daemonInfo: DaemonInfo;
  /** herdr server recovery state: the pane shows `recovering` while the
   * backend resumes its session, or `suspended` while recovery holds it back. */
  recovery: RecoveryInfo;
  /** A file drag is hovering this pane — draws the drop affordance. */
  isDropTarget?: boolean;
  restartNonce: number;
  /** Whether the latest `restartNonce` bump asked for a new, empty conversation
   * instead of resuming the current one. */
  restartFresh?: boolean;
  /** Set while this worktree is mid CLI-transfer — the target CLI
   * being transferred TO. Swaps the recovery overlay for a "Transferring…"
   * indicator and skips the restart-nonce confirm (already confirmed in the rail). */
  transferringTo?: CliKind | null;
  /** The user terminated this worktree's session (from the worktree list). The single
   * source of truth for the terminated state: the pane never attaches, spawns or
   * restarts on its own while it is set. */
  terminated: boolean;
  /** Bumped by the rail's "Terminate session" item (confirmed here when live). */
  terminateNonce: number;
  /** Bumped by the rail's "Launch" / "Launch fresh" items. */
  launchNonce: number;
  /** Whether the latest `launchNonce` bump asked for a new, empty conversation. */
  launchFresh?: boolean;
}) {
  const paneRef = useRef<HTMLDivElement | null>(null);
  const { showToast } = useToast();
  const linkContextRef = useRef({ project: projectName, worktree: name, active });
  linkContextRef.current = { project: projectName, worktree: name, active };
  const linksRef = useRef<ReturnType<typeof installTerminalLinks> | null>(null);
  const hostRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const webglRef = useRef<WebglAddon | null>(null);
  // `active` for callbacks that must not hold a GPU context for an off-screen pane.
  const activeRef = useRef(active);
  activeRef.current = active;
  const searchOpenRef = useRef(false); // mirrors searchVisible state for use inside the key handler
  const openedRef = useRef(false);
  // A real fit has been committed to this Terminal (by refitAndResize or the fit before
  // an attach/spawn). Until then it holds xterm's default 80×24, which must never reach
  // herdr as a takeover size: attaches and spawns send no size instead.
  const measuredRef = useRef(false);
  // True while an IME composition is in flight in this pane's textarea. Read by the custom
  // key handler to suppress xterm's double-commit on the 中/英 (Caps Lock) toggle.
  const isComposingRef = useRef(false);
  const attachingRef = useRef(false); // guards doAttach against concurrent re-entry
  const genRef = useRef<number | null>(null); // generation of this pane's current backend session
  // The backend attaches this pane holds (each scopes a `ptyDetach`): the live one
  // feeding its output channel, and one a re-attach in flight would replace. Plus
  // the live channel's gate. A new attach replaces both.
  const attachmentsRef = useRef(createPaneAttachments());
  const outputGateRef = useRef<OutputGate | null>(null);
  // Joins channel frames into one term.write per animation frame, and holds them
  // while the pane is hidden. Sits between the gate and the terminal.
  const coalescerRef = useRef<FrameCoalescer | null>(null);
  const restartBtnRef = useRef<HTMLButtonElement | null>(null);
  const refitRafRef = useRef<number | null>(null); // pending refit animation frame
  const refitGenRef = useRef(0); // supersedes any older refit retry loop
  // Set after a (re)attach: the one frame captured at attach may have landed before the
  // renderer was warm, so the first activation asks the backend for a fresh viewport frame.
  // Consumed once per attach so ordinary pane switches don't re-take-over the herdr bridge
  // (size-driven redraws are already handled by refitAndResize → resize).
  const pendingRepaintRef = useRef(false);
  // Last restartNonce seen by the prop-driven restart effect below. Seeded to the initial
  // prop value so that effect does not fire on mount — only when the nonce increments past
  // whatever we've already reacted to.
  const lastSeenRestartNonceRef = useRef(restartNonce);
  // Same seeding for the terminate / launch signals.
  const lastSeenTerminateNonceRef = useRef(terminateNonce);
  const lastSeenLaunchNonceRef = useRef(launchNonce);
  // Mirrors the `terminated` prop for callbacks: attach and spawn return at once while
  // it is set, so a terminated session is never brought back except by a launch.
  const terminatedRef = useRef(terminated);
  terminatedRef.current = terminated;
  const wheelRemainderRef = useRef(0);

  const [status, setStatus] = useState<Status>(terminated ? "terminated" : "attaching");
  // statusRef mirrors status so the onData closure doesn't capture a stale value.
  const statusRef = useRef<Status>(terminated ? "terminated" : "attaching");
  const updateStatus = useCallback((s: Status) => {
    statusRef.current = s;
    setStatus(s);
  }, []);

  const [searchVisible, setSearchVisible] = useState(false);
  const setSearchOpen = useCallback((open: boolean) => {
    searchOpenRef.current = open;
    setSearchVisible(open);
  }, []);
  const [showSpinner, setShowSpinner] = useState(false);
  const [fontSize] = useTerminalFontSize();
  // Resolved app theme (light|dark). Chosen at construction and swapped live below.
  const resolvedTheme = useResolvedTheme();
  const [showReattachedPill, setShowReattachedPill] = useState(false);
  // "↓ live" pill: the viewport is scrolled up into herdr's history.
  const [showLivePill, setShowLivePill] = useState(false);
  const liveTailRef = useRef<LiveTail | null>(null);
  const scrollToLiveRef = useRef<() => void>(() => {});
  // What the terminal search drives: herdr's history, scrolled by a line delta and brought
  // back to the live tail. Wired up with the terminal below.
  const historyViewRef = useRef<HistoryView>(IDLE_HISTORY_VIEW);
  // The open confirm surface, by restart flavour or a terminate (`false` = closed).
  const [confirmKill, setConfirmKill] = useState<false | "resume" | "fresh" | "terminate">(false);

  // Mirrors the `transferringTo` prop so the restart-nonce effect
  // below can read it live without adding a churny prop dependency.
  const transferringRef = useRef(transferringTo ?? null);
  transferringRef.current = transferringTo ?? null;

  // The latest daemon state and recovery state, for async callbacks that
  // finish after either changed.
  const daemonStateRef = useRef(daemonInfo.state);
  daemonStateRef.current = daemonInfo.state;
  const recNowRef = useRef(paneRecoveryState(recovery, projectName, name));
  recNowRef.current = paneRecoveryState(recovery, projectName, name);
  // The current doAttach, for a retry scheduled from a callback declared before it.
  const doAttachRef = useRef<(() => Promise<void>) | null>(null);

  // The backend refused because herdr is recovering. Recovery may have let go
  // of the pane while that call was in flight (and the re-attach it triggered
  // was skipped): then try again instead of waiting quietly for good.
  const settleRecoveringRefusal = useCallback(() => {
    const next = afterRecoveringRefusal(daemonStateRef.current, recNowRef.current);
    updateStatus(next.status);
    if (next.attach) window.setTimeout(() => void doAttachRef.current?.(), 250);
  }, [updateStatus]);

  // Reset confirmKill whenever status changes.
  // biome-ignore lint/correctness/useExhaustiveDependencies: collapse confirmKill on status change; stable setter excluded from deps
  useEffect(() => {
    setConfirmKill(false);
  }, [status]);

  // Cancel any in-flight refit retry frame.
  const cancelRefit = useCallback(() => {
    if (refitRafRef.current != null) {
      cancelAnimationFrame(refitRafRef.current);
      refitRafRef.current = null;
    }
  }, []);

  // Reliable fit + PTY resize. The old single-rAF fit fired before the host was
  // laid out / the freshly-recreated WebGL renderer had measured its cell size, so
  // FitAddon.proposeDimensions() returned undefined, fit.fit() no-op'd, and xterm's
  // default 80×24 was shipped to the daemon — the "wrong size until I swap worktrees"
  // bug. Here we retry across frames until the proposal is sane AND stable across two
  // frames, then commit once. On exhaustion we commit the last sane proposal seen, or
  // nothing at all: the terminal's own size may still be the unmeasured default. After
  // committing we refresh (resyncs a recreated WebGL viewport).
  const refitAndResize = useCallback(() => {
    const term = termRef.current;
    const host = hostRef.current;
    const fit = fitRef.current;
    if (!term || !host || !fit) return;

    cancelRefit();
    // Claim a generation: any later refitAndResize() invalidates this loop, so even
    // if a tick is somehow already in flight it bails instead of double-committing.
    const myGen = ++refitGenRef.current;

    const MAX_FIT_FRAMES = 8;
    let frames = 0;
    let lastSane: Dims | null = null;

    // Sizes xterm to the proposal already measured (no second layout read) and
    // sends it.
    // With no proposal (the loop gave up) only the refresh runs: xterm's own size
    // may be the unmeasured default.
    const commit = (d: Dims | null) => {
      try {
        if (d) sizeTerminal(term, d);
      } catch {
        /* host not measurable */
      }
      // Only a size xterm actually took counts as measured.
      if (d && dimsEqual(d, { rows: term.rows, cols: term.cols })) {
        measuredRef.current = true;
        const sized = clampPtySize(d);
        void ptyResize(projectName, name, sized.rows, sized.cols);
      }
      try {
        term.refresh(0, Math.max(0, term.rows - 1));
      } catch {
        /* ignore */
      }
    };

    const tick = () => {
      refitRafRef.current = null;
      // Bail if a newer refit superseded us, or the pane was torn down / swapped out.
      if (refitGenRef.current !== myGen) return;
      if (termRef.current !== term || fitRef.current !== fit) return;
      // A hidden / 0×0 host must never ship a bogus size.
      if (host.clientWidth === 0 || host.clientHeight === 0) return;

      frames += 1;
      const proposed = proposeFit(fit);
      if (dimsAreSane(proposed)) {
        if (lastSane && dimsEqual(lastSane, proposed)) {
          commit(proposed);
          return;
        }
        lastSane = proposed;
      }
      if (frames >= MAX_FIT_FRAMES) {
        commit(lastSane);
        return;
      }
      refitRafRef.current = requestAnimationFrame(tick);
    };

    refitRafRef.current = requestAnimationFrame(tick);
  }, [name, projectName, cancelRefit]);

  // Hold a WebGL context for an opened, on-screen terminal (once).
  const loadWebgl = useCallback((term: Terminal) => {
    if (webglRef.current) return;
    try {
      const w = new WebglAddon();
      term.loadAddon(w);
      webglRef.current = w;
    } catch {
      /* WebGL unavailable — xterm falls back to its canvas/DOM renderer */
    }
  }, []);

  // Fit the terminal before an attach or spawn, so herdr takes the pane over at its
  // real size rather than xterm's default. Only a laid-out host is touched: it is
  // opened (proposeDimensions needs the terminal's element) and fitted when the
  // renderer can measure a cell. A hidden (0×0) host is left alone, so the pane stays
  // unmeasured and sends no size; its first fit on show sends the real one. No
  // ptyResize here: the attach or spawn that follows carries the size.
  const fitBeforeTakeover = useCallback(() => {
    const term = termRef.current;
    const host = hostRef.current;
    const fit = fitRef.current;
    if (!term || !host || !fit) return;
    if (host.clientWidth === 0 || host.clientHeight === 0) return;
    if (!openedRef.current) {
      term.open(host);
      openedRef.current = true;
    }
    // Measure with the renderer the pane will paint with: a cell measured by the
    // DOM renderer can differ from WebGL's, and the refit that follows would then
    // shrink the pane it just took over.
    if (activeRef.current) loadWebgl(term);
    const proposed = proposeFit(fit);
    // Renderer cell metrics not ready yet: the post-attach refit sends the size.
    if (!dimsAreSane(proposed)) return;
    try {
      sizeTerminal(term, proposed);
      measuredRef.current = true;
    } catch {
      /* host not measurable */
    }
  }, [loadWebgl]);

  // The gate for a new attach's output channel, writing into `term`. Closes the
  // previous attach's gate (its late messages are dropped); the previous attach
  // stays registered until the new one replaces it (or fails and releases it).
  const newOutputGate = useCallback(
    (term: Terminal) => {
      outputGateRef.current?.close();
      attachmentsRef.current.begin();
      // Output still queued from the previous attach is superseded by this one's scrollback.
      coalescerRef.current?.reset();
      // A new attach paints the live viewport: whatever the pill said is stale.
      liveTailRef.current?.reset();
      const gate = createOutputGate({
        write: (bytes, full) => {
          if (termRef.current !== term) return;
          coalescerRef.current?.push(bytes, onFrame(`${projectName}/${name}`), full);
          // While the pill shows, output may mean history was erased or the
          // viewport snapped back: re-check once it settles.
          liveTailRef.current?.noteOutput();
        },
        // The output before the exit reaches the terminal first (hidden or not),
        // then the pane shows it ended. A newer attach or a restart resets the
        // coalescer meanwhile, which cancels the deferred "ended".
        exit: () => {
          if (termRef.current !== term) return;
          // A terminated pane stays terminated when its killed session's exit lands.
          const ended = () => {
            if (termRef.current === term && statusRef.current !== "terminated") {
              updateStatus("ended");
            }
          };
          if (coalescerRef.current) coalescerRef.current.end(ended);
          else ended();
        },
      });
      outputGateRef.current = gate;
      return gate;
    },
    [name, projectName, updateStatus],
  );

  // A new attach succeeded: it feeds the pane, and the backend replaced any earlier one.
  const attachSucceeded = useCallback((result: { attach_id: number; repaints: boolean }) => {
    attachmentsRef.current.succeeded(result.attach_id);
    coalescerRef.current?.setRepaintable(result.repaints);
  }, []);

  // A new attach failed: nothing replaced the earlier attach, so release it here
  // (its gate is already closed; left registered, nobody would ever detach it).
  const attachFailed = useCallback(() => {
    const stale = attachmentsRef.current.failed();
    if (stale != null) void ptyDetach(projectName, name, stale).catch(() => {});
  }, [name, projectName]);

  // Output reaches a pane only over an attach's channel. A session spawned while
  // this pane has none (the mount's attach found no session) is attached right
  // after, so its output has somewhere to go; a restart keeps the existing attach.
  const attachAfterSpawn = useCallback(
    async (term: Terminal) => {
      const gate = newOutputGate(term);
      try {
        // The spawn just sent the same size (or none); this takeover keeps it.
        const result = await ptyAttach(
          projectName,
          name,
          sizeToSend(measuredRef.current, term),
          gate.onMessage,
        );
        if (termRef.current !== term) {
          // Torn down meanwhile: release just this attach.
          gate.close();
          void ptyDetach(projectName, name, result.attach_id);
          return;
        }
        attachSucceeded(result);
        if (result.scrollback.length > 0) term.write(decodeBase64(result.scrollback));
        gate.open();
      } catch (e) {
        gate.close();
        attachFailed();
        console.warn("[keitora] attach after spawn failed; the pane shows no output", e);
      }
    },
    [name, projectName, newOutputGate, attachSucceeded, attachFailed],
  );

  // `via: "launch"` brings a terminated session back (`sessionLaunch` clears the mark,
  // then spawns); a plain spawn never runs for a terminated pane.
  const spawn = useCallback(
    async (fresh = false, via: "spawn" | "launch" = "spawn") => {
      const term = termRef.current;
      if (!term) return;
      if (via === "spawn" && terminatedRef.current) return;
      try {
        // An unmeasured pane spawns with no size; the backend picks one, and its
        // first fit sends the real size.
        const size = sizeToSend(measuredRef.current, term);
        const gen =
          via === "launch"
            ? await sessionLaunch(projectName, name, path, size, fresh)
            : await ptySpawn(projectName, name, path, size, undefined, fresh);
        genRef.current = gen;
        updateStatus("running");
        // A fit may have changed the size while the spawn was in flight: send the
        // new one. Still unmeasured, or unchanged: nothing to send.
        const sizeNow = sizeToSend(measuredRef.current, term);
        if (sizeNow && !(size && dimsEqual(size, sizeNow))) {
          void ptyResize(projectName, name, sizeNow.rows, sizeNow.cols);
        }
        if (attachmentsRef.current.live == null) await attachAfterSpawn(term);
      } catch (e) {
        // A spawn refused because herdr is recovering is not a launch failure:
        // the backend resumes the session itself.
        const msg = String(e);
        if (msg.includes("Recovering")) settleRecoveringRefusal();
        else if (msg.includes("Terminated")) {
          // The backend refused: the session is terminated (the worktree list may lag).
          genRef.current = null;
          updateStatus("terminated");
        } else updateStatus("spawn-failed");
      }
    },
    [name, path, projectName, updateStatus, settleRecoveringRefusal, attachAfterSpawn],
  );

  // biome-ignore lint/correctness/useExhaustiveDependencies: stable callback; name/path/projectName/updateStatus identity-stable
  const doAttach = useCallback(async () => {
    const term = termRef.current;
    const host = hostRef.current;
    const fit = fitRef.current;
    if (!term || !host || !fit) return;
    // A terminated session stays down: no attach, and so no fallback spawn either.
    if (terminatedRef.current) return;

    // Concurrency guard: only one attach may run at a time. Two daemon-status
    // transitions (or a mount + a reconnect) could otherwise drive two
    // concurrent attaches, doubling scrollback/output. Early-return while one is
    // in flight; the flag clears in the finally below.
    if (attachingRef.current) return;
    attachingRef.current = true;

    updateStatus("attaching");

    // Show spinner only after 500ms if attach is still in flight.
    const spinnerId = window.setTimeout(() => setShowSpinner(true), 500);

    try {
      // The daemon client connects asynchronously a moment AFTER the pane mounts,
      // so the first attach can race ahead of the connection and return
      // NotConnected. That is NOT a "no live session" — retry until the client is
      // connected (the daemon is the authority), and only fall back to spawn on a
      // definitive NoLiveSession miss. Bounded so a genuinely-down daemon still
      // spawns rather than hanging. The herdr backend returns NotConnected until its
      // one-shot ensure_server + reconcile completes (a cold start can spawn the
      // server, ~10s, then reconcile, ~3s), so the window must comfortably exceed that
      // or a slow first boot would fall through to a duplicate spawn.
      const deadline = Date.now() + 20000;
      for (;;) {
        // Each try has its own channel; frames that beat the reply wait in the gate
        // until the scrollback below is written.
        const gate = newOutputGate(term);
        try {
          // Fit first (a visible pane opens here, under the "attaching" overlay), so
          // the takeover happens at the pane's real size. Unmeasured: send no size and
          // let the backend keep the pane's own. Re-run per try: the host may have been
          // shown or resized while a retry waited.
          fitBeforeTakeover();
          const result = await ptyAttach(
            projectName,
            name,
            sizeToSend(measuredRef.current, term),
            gate.onMessage,
          );
          window.clearTimeout(spinnerId);
          setShowSpinner(false);
          if (termRef.current !== term) {
            // The pane was torn down while the attach was in flight: release just
            // this attach, never a newer one.
            gate.close();
            void ptyDetach(projectName, name, result.attach_id);
            return;
          }
          attachSucceeded(result);

          // Open the terminal (unless the fit above already did) and write scrollback
          // before making it visible.
          if (!openedRef.current) {
            term.open(host);
            openedRef.current = true;
          }
          if (result.scrollback.length > 0) {
            term.write(decodeBase64(result.scrollback));
          }
          gate.open();
          refitAndResize();
          // Terminated while the attach was in flight: the pane stays terminated.
          if (statusRef.current === "terminated") return;

          genRef.current = result.gen;
          // Ask for one fresh frame on the upcoming activation (the attach frame may predate
          // a warm renderer). Switch-backs after this won't repaint.
          pendingRepaintRef.current = true;
          updateStatus("running");

          // Show the "↩ reattached" pill for 2.5s if there was scrollback.
          if (result.scrollback.length > 0) {
            setShowReattachedPill(true);
            window.setTimeout(() => setShowReattachedPill(false), 2500);
          }
          return;
        } catch (e: unknown) {
          gate.close();
          attachFailed();
          // `pty_attach` returns a STABLE leading token from `attach_err_kind`
          // (host.rs); `classifyAttachError` matches those exact tokens — the
          // DaemonError Display prose ("daemon unreachable") is NOT matchable.
          const action = classifyAttachError(String(e), Date.now(), deadline);
          if (action === "ended-while-away") {
            window.clearTimeout(spinnerId);
            setShowSpinner(false);
            updateStatus("ended-while-away");
            return;
          }
          if (action === "recovering") {
            // herdr is being recovered: wait quietly (no retry loop, never a
            // spawn); the pane re-attaches when recovery reports Healthy, or
            // at once if it already did while this attach was in flight.
            window.clearTimeout(spinnerId);
            setShowSpinner(false);
            settleRecoveringRefusal();
            return;
          }
          if (action === "terminated" || terminatedRef.current) {
            // Terminated (by the backend's refusal, or meanwhile by the user): stay
            // down until launched, never falling through to a spawn.
            window.clearTimeout(spinnerId);
            setShowSpinner(false);
            genRef.current = null;
            updateStatus("terminated");
            return;
          }
          if (action === "retry") {
            // Client still connecting to the daemon — wait and retry attach. This
            // is the live retry loop; it engages on the NotConnected token only.
            await new Promise((r) => window.setTimeout(r, 250));
            continue;
          }
          // NoLiveSession (authoritative miss from a connected daemon), a
          // NotConnected that outlasted the deadline, or any other error: spawn a
          // fresh session.
          window.clearTimeout(spinnerId);
          setShowSpinner(false);
          await spawn();
          return;
        }
      }
    } finally {
      attachingRef.current = false;
    }
  }, [
    name,
    path,
    projectName,
    spawn,
    updateStatus,
    refitAndResize,
    settleRecoveringRefusal,
    newOutputGate,
    attachSucceeded,
    attachFailed,
    fitBeforeTakeover,
  ]);
  doAttachRef.current = doAttach;

  const restart = useCallback(
    async (fresh = false) => {
      // Kill any existing session before spawning again (resuming its conversation
      // unless `fresh`).
      if (genRef.current != null) {
        const gen = genRef.current;
        genRef.current = null;
        try {
          await ptyKill(projectName, name, gen);
        } catch {
          /* best-effort */
        }
      }
      // The killed session's queued output and any "ended" still waiting on its
      // write belong to the old session, not the one spawned next. So does its
      // scroll position.
      coalescerRef.current?.reset();
      liveTailRef.current?.reset();
      termRef.current?.clear();
      // Re-open the terminal if it was closed.
      const term = termRef.current;
      const host = hostRef.current;
      if (term && host && !openedRef.current) {
        term.open(host);
        openedRef.current = true;
      }
      // A visible pane spawns at its real size; a hidden one sends none.
      fitBeforeTakeover();
      await spawn(fresh);
    },
    [name, projectName, spawn, fitBeforeTakeover],
  );

  // Terminate: like restart's kill, but nothing is spawned after it. The mark is
  // stored before the kill, so recovery never resumes the session. The terminal's
  // scrollback stays visible behind the "Session terminated" overlay.
  const terminate = useCallback(async () => {
    const gen = genRef.current;
    try {
      await sessionTerminate(projectName, name, gen ?? undefined);
    } catch (e) {
      // Not stored (nothing was done), or stored but the kill failed: then the
      // worktree list reports it terminated and the prop effect below takes over.
      showToast(`Could not terminate ${name}: ${String(e)}`);
      return;
    }
    genRef.current = null;
    // The killed session's queued output and any "ended" still waiting on its write,
    // and its scroll position.
    coalescerRef.current?.reset();
    liveTailRef.current?.reset();
    updateStatus("terminated");
  }, [name, projectName, showToast, updateStatus]);

  // Launch a terminated session again: `spawn` through `sessionLaunch`, after
  // clearing the old session's output like a restart does.
  const launch = useCallback(
    async (fresh = false) => {
      coalescerRef.current?.reset();
      liveTailRef.current?.reset();
      termRef.current?.clear();
      const term = termRef.current;
      const host = hostRef.current;
      if (term && host && !openedRef.current) {
        term.open(host);
        openedRef.current = true;
      }
      fitBeforeTakeover();
      updateStatus("attaching");
      await spawn(fresh, "launch");
    },
    [spawn, updateStatus, fitBeforeTakeover],
  );

  // This pane's place in recovery (from `daemon://recovery`, never the error token).
  const rec = paneRecoveryState(recovery, projectName, name);
  const recItem = recoveryItem(recovery, projectName, name);

  // SINGLE owner of daemon-connectivity reactions (FIX #5). The parent `App`
  // subscribes via `useDaemonStatus` and passes `daemonInfo` down, so this
  // prop-driven effect is the one place that reacts to connect/disconnect — no
  // separate `onDaemonStatus` listener (which would double-fire doAttach).
  //
  // The reaction itself is `nextPaneStatus`, recovery first:
  // - recovery holds the session back → `suspended`; herdr recovering → a live or
  //   frozen pane goes quiet (`recovering`);
  // - recovery lets go + daemon Connected while daemon-down/recovering/suspended →
  //   re-attach (attach-first: the resumed session is adopted; NoLiveSession falls
  //   back to a gated spawn);
  // - daemon drops while running/attaching/detached-alive → freeze (daemon-down,
  //   NOT ended; do not kill).
  // A pane never restarts itself: the backend owns recovery and resumes sessions.
  // doAttach's own attachingRef guard prevents a concurrent attach.
  useEffect(() => {
    const s = statusRef.current;
    const next = nextPaneStatus(s, daemonInfo.state, rec);
    if (next.attach) void doAttach();
    else if (next.status !== s) updateStatus(next.status);
  }, [daemonInfo.state, rec, doAttach, updateStatus]);

  // The `terminated` prop. Becoming set (terminated from the rail, possibly while this
  // pane was busy) parks the pane; becoming clear (launched) re-attaches a pane still
  // parked, and replays the daemon/recovery reaction for one whose attach was skipped
  // while the mark was still set.
  // biome-ignore lint/correctness/useExhaustiveDependencies: reacts to the prop only; doAttach/updateStatus are stable and the daemon/recovery state is read live through refs
  useEffect(() => {
    if (terminated) {
      genRef.current = null;
      if (statusRef.current !== "terminated") updateStatus("terminated");
      return;
    }
    if (statusRef.current === "terminated") {
      void doAttach();
      return;
    }
    const next = nextPaneStatus(statusRef.current, daemonStateRef.current, recNowRef.current);
    if (next.attach) void doAttach();
  }, [terminated]);

  // Resume-on-focus: a suspended session comes back when its pane is shown.
  useEffect(() => {
    if (active && status === "suspended") void recoveryFocus(projectName, name);
  }, [active, status, projectName, name]);

  // "Start fresh conversation" while recovery holds the session: recovery
  // launches it fresh in its own turn; when it holds nothing for this pane, the
  // ordinary fresh restart does it.
  const startFreshFromRecovery = useCallback(async () => {
    try {
      if (await recoveryStartFresh(projectName, name)) return;
    } catch {
      /* fall back to the ordinary path */
    }
    await restart(true);
  }, [projectName, name, restart]);

  // Prop-driven restart signal. The Rail's "Restart session" menu bumps a per-worktree
  // nonce (in App) to reach THIS mounted pane regardless of status — including a wedged pane
  // still reporting "running", where the overlay (gated on `status !== "running"`, below) never
  // renders and so has no button to drive restart(). Mirrors the "single owner of daemon
  // reactions" prop-driven effect above: react only when the prop actually changes, tracking
  // the last-seen value in a ref (seeded on mount) so this never fires on initial mount, only
  // when restartNonce increments past what we've already handled. Same confirm-or-restart
  // decision as the existing "Kill & Restart" button (handleKillRestart, below): needsConfirm
  // gates on the CURRENT status via statusRef, not a stale closure. `restartFresh` says which
  // flavour the bump asked for; it changes together with the nonce.
  // biome-ignore lint/correctness/useExhaustiveDependencies: needsConfirm is a pure inline predicate (no stable identity to track); statusRef/transferringRef read live; restartFresh only matters alongside a nonce change
  useEffect(() => {
    if (restartNonce <= lastSeenRestartNonceRef.current) return;
    lastSeenRestartNonceRef.current = restartNonce;
    // A terminated session is only brought back by a launch.
    if (terminatedRef.current || statusRef.current === "terminated") return;
    // A CLI transfer was already confirmed in the rail dialog
    // before this nonce bump — never re-prompt with the "Kill & Restart?" confirm
    // even though the session is still `running` (Claude→Codex transfer target).
    if (transferringRef.current) {
      void restart(false);
    } else if (needsConfirm(statusRef.current)) {
      setConfirmKill(restartFresh ? "fresh" : "resume");
    } else {
      void restart(restartFresh);
    }
  }, [restartNonce, restartFresh, restart]);

  // Prop-driven terminate / launch signals from the rail menu, handled like the restart
  // signal above (react only past the seeded last-seen nonce). A terminate of a live
  // session asks first; a launch never does, and is ignored unless the pane is terminated.
  // biome-ignore lint/correctness/useExhaustiveDependencies: needsConfirm is a pure inline predicate; statusRef/terminatedRef read live
  useEffect(() => {
    if (terminateNonce <= lastSeenTerminateNonceRef.current) return;
    lastSeenTerminateNonceRef.current = terminateNonce;
    if (terminatedRef.current || statusRef.current === "terminated") return;
    if (needsConfirm(statusRef.current)) setConfirmKill("terminate");
    else void terminate();
  }, [terminateNonce, terminate]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: launchFresh only matters alongside a nonce change; statusRef/terminatedRef read live
  useEffect(() => {
    if (launchNonce <= lastSeenLaunchNonceRef.current) return;
    lastSeenLaunchNonceRef.current = launchNonce;
    if (!terminatedRef.current && statusRef.current !== "terminated") return;
    void launch(launchFresh);
  }, [launchNonce, launch]);

  // Lifetime: create the Terminal, wire I/O, attach or spawn the session. Disposed on unmount.
  // biome-ignore lint/correctness/useExhaustiveDependencies: mount-only terminal setup; all deps are stable refs, intentionally excluded
  useEffect(() => {
    const term = new Terminal({
      fontFamily: 'ui-monospace, "SF Mono", "JetBrains Mono", Menlo, monospace',
      fontSize: fontSize,
      cursorBlink: true,
      // herdr owns the pane's history (the wheel scrolls it there, see below); xterm
      // only paints herdr's viewport, so it keeps none: rows it scrolled off would be
      // stale, and a selection drag or scrollbar could move its viewport onto them.
      scrollback: 0,
      theme: THEMES[resolvedTheme],
    });
    const links = installTerminalLinks(term, {
      context: () => linkContextRef.current,
      openUrl,
      openFile: openTerminalFile,
      showError: showToast,
    });
    linksRef.current = links;
    const fit = new FitAddon();
    term.loadAddon(fit);

    // Track IME composition state off the host: composition events bubble from xterm's
    // internal textarea, which term.open() creates inside the host later (and re-creates
    // across open calls), so listening on the stable parent catches them without having to
    // chase term.textarea. Feeds suppressesCompositionDoubleCommit below.
    const compositionHost = hostRef.current;
    const onCompositionStart = () => {
      isComposingRef.current = true;
    };
    const onCompositionEnd = () => {
      isComposingRef.current = false;
    };
    compositionHost?.addEventListener("compositionstart", onCompositionStart);
    compositionHost?.addEventListener("compositionend", onCompositionEnd);

    // Paste: Claude takes an image by *path*, so when the clipboard holds
    // image data we write it to a temp file and inject the quoted path — the same
    // shape a real terminal produces when you drag an image in. Anything else (or
    // any failure reading an image) falls back to the plain text paste.
    //
    // Both branches go through term.paste() rather than ptyWrite() so they respect
    // bracketed-paste mode and the onData input gate below (attaching).
    const pasteClipboard = async () => {
      try {
        // clipboard.read() is absent in some webviews; readText() is the baseline.
        if (typeof navigator.clipboard.read === "function") {
          const items = await navigator.clipboard.read();
          // Text wins whenever the clipboard carries any. Office apps (Excel, Word,
          // Outlook) put a bitmap on the clipboard *alongside* copied cells/rich text,
          // and WebView2 surfaces that as image/png — taking the image first would turn
          // an ordinary text paste into a temp-file path, a regression against the plain
          // readText() this replaced. Ctrl+Shift+V is also "paste", so there'd be no
          // chord left to force text.
          const hasText = items.some((i) => i.types.includes("text/plain"));
          if (!hasText) {
            for (const item of items) {
              const type = item.types.find((t) => t.startsWith("image/"));
              if (!type) continue;
              const blob = await item.getType(type);
              const path = await saveClipboardImage(
                await blobToBase64(blob),
                type.slice("image/".length),
              );
              // The pane may have unmounted (term disposed) while the read was in
              // flight — termRef is nulled on cleanup, so this skips a dead terminal.
              if (termRef.current === term) term.paste(`${quotePathForInjection(path)} `);
              return;
            }
          }
        }
      } catch {
        /* no readable image / denied — fall through to the text paste */
      }
      try {
        const text = await navigator.clipboard.readText();
        if (text && termRef.current === term) term.paste(text);
      } catch {
        /* clipboard unavailable / denied — silent */
      }
    };

    term.attachCustomKeyEventHandler((e: KeyboardEvent) => {
      // We only ever act on keydown; let xterm handle keypress/keyup unchanged.
      if (e.type !== "keydown") return true;
      if (isFindChord(e)) {
        // Returning false only stops xterm; preventDefault keeps the webview's native
        // find bar from opening too.
        e.preventDefault();
        setSearchOpen(true);
        return false;
      }
      if (e.key === "Escape" && searchOpenRef.current) {
        setSearchOpen(false);
        return false;
      }
      // Let app-navigation chords (view switch, worktree cycle) win over the
      // terminal: don't consume them, so they bubble to the window-level keymap
      // instead of being sent to the PTY.
      if (isGlobalAppChord(e)) return false;
      // End returns to the live tail while the "↓ live" pill is up (and is not sent).
      if (isScrollToLiveKey(e) && liveTailRef.current?.visible) {
        e.preventDefault();
        scrollToLiveRef.current();
        return false;
      }
      // Shift+PageUp/PageDown page through herdr's history and never reach the app.
      const pageDir = paneScrollDirection(e);
      if (pageDir) {
        e.preventDefault();
        queueScroll(pageDir, "page_key", Math.max(1, term.rows - 1), cursorAt());
        flushScroll();
        return false;
      }
      // Windows clipboard: WebView2 DOES deliver a native paste to xterm's textarea, but
      // that path is opaque to our attaching gate, so we own Ctrl+V explicitly
      // here and suppress the native duplicate at the host container (see the capture-
      // phase paste listener below). term.paste() respects bracketed-paste mode
      // and flows through the onData gate below. No-op off Windows (clipboardActionForKey
      // returns null).
      const action = clipboardActionForKey(e, isWindows);
      if (action === "paste") {
        void pasteClipboard();
        return false;
      }
      if (action === "copy") {
        const sel = term.getSelection();
        if (sel) {
          // Clear the selection only after the copy succeeds, so a denied write
          // doesn't drop the selection with nothing on the clipboard. With the
          // selection gone, the next Ctrl+C sends SIGINT (Windows Terminal behavior).
          navigator.clipboard
            .writeText(sel)
            .then(() => {
              if (termRef.current === term) term.clearSelection();
            })
            .catch(() => {
              /* clipboard write denied — keep the selection, nothing copied */
            });
          return false;
        }
        // no selection → fall through so xterm sends \x03 (SIGINT)
      }
      // The 中/英 (Caps Lock) IME toggle mid-composition double-commits in xterm 5.5.0:
      // this keydown finalizes the composition AND the native compositionend re-sends it.
      // Swallow the keydown (this handler runs before CompositionHelper.keydown) so only
      // compositionend emits — 你好, not 你好你好. Returning false here doesn't
      // preventDefault, so Caps Lock still toggles the IME.
      if (suppressesCompositionDoubleCommit(e, isComposingRef.current)) return false;
      return true;
    });

    termRef.current = term;
    fitRef.current = fit;
    // A new Terminal starts at xterm's default size, not a measured one.
    measuredRef.current = false;
    coalescerRef.current = createFrameCoalescer({
      visible: linkContextRef.current.active,
      write: (bytes, done) => term.write(bytes, done),
      // The hidden buffer overflowed and was dropped: ask for a fresh viewport frame.
      onNeedsRepaint: () => {
        void ptyRepaint(projectName, name).catch(() => {
          /* best-effort; the bridge still streams any real output */
        });
      },
    });

    // Input gating: suppress keystrokes while attaching, detached-alive, daemon-down,
    // while recovery holds the session (recovering / suspended), or once terminated.
    const dataDisp = term.onData((d) => {
      if (inputBlocked(statusRef.current)) return;
      // herdr snaps the viewport back to the live tail on any input.
      liveTailRef.current?.noteInput();
      const t0 = stamp();
      markPending(`${projectName}/${name}`, t0);
      void ptyWrite(projectName, name, d, t0);
    });

    // Windows double-paste: WebView2 fires a native `paste` on xterm's helper
    // textarea IN ADDITION to our explicit Ctrl+V handler above, so xterm would paste a
    // second time. Suppress the native paste at the host container — an ANCESTOR of the
    // textarea, so this capture-phase listener runs before xterm's target-phase handlers
    // (a same-node listener would lose on registration order). We own paste via the
    // explicit path. No-op off Windows, where the native textarea paste IS the paste path.
    const host = hostRef.current;
    const onNativePaste = (e: ClipboardEvent) => {
      if (!suppressNativePaste(isWindows)) return;
      e.preventDefault(); // stop native insertion into the helper textarea
      e.stopImmediatePropagation(); // stop xterm's textarea + element paste handlers
    };
    host?.addEventListener("paste", onNativePaste, true);

    // Scrolling belongs to herdr: it routes each scroll to the app (mouse reporting),
    // to arrow keys (alternate scroll) or to the pane's history. So every vertical wheel
    // tick becomes a ptyScroll, and xterm neither scrolls its own viewport nor emulates
    // arrows. Ticks are batched per animation frame; a direction change sends the
    // pending batch first so the order of movement is kept.
    let wheelDebug = false;
    try {
      wheelDebug = !!localStorage.getItem("keitora:debug-wheel");
    } catch {
      /* localStorage unavailable */
    }

    const liveTail = createLiveTail({
      query: () => ptyScrollOffset(projectName, name),
      onChange: setShowLivePill,
    });
    liveTailRef.current = liveTail;
    setShowLivePill(false);

    let pendingScroll:
      | ({ direction: ScrollDirection; source: ScrollSource; lines: number } & Cell)
      | null = null;
    let scrollRaf: number | null = null;

    // The cursor's cell: where a key-driven scroll (page keys, back to live) points.
    function cursorAt(): Cell {
      const buf = term.buffer.active;
      return cursorCell(buf.cursorX, buf.cursorY, term.cols, term.rows);
    }

    function flushScroll() {
      if (scrollRaf !== null) {
        cancelAnimationFrame(scrollRaf);
        scrollRaf = null;
      }
      const batch = pendingScroll;
      pendingScroll = null;
      if (!batch || termRef.current !== term || inputBlocked(statusRef.current)) return;
      if (wheelDebug) console.log("[keitora wheel] ptyScroll", batch);
      const { direction, source, lines, column, row } = batch;
      ptyScroll(projectName, name, direction, source, lines, column, row).catch((err) => {
        // Expected while herdr recovers; the next tick tries again.
        if (wheelDebug) console.debug("[keitora wheel] ptyScroll failed", err);
      });
      // Once the burst settles, ask whether the viewport ended up in history.
      liveTail.noteScroll(direction);
    }

    // The pill's click / End: one wheel scroll down far enough to reach the live tail
    // (herdr stops there). Queued like any other scroll, so a pending "up" batch goes
    // out before it.
    scrollToLiveRef.current = () => {
      const lines = liveTail.jumpToLive();
      if (lines === null) return;
      queueScroll("down", "wheel", lines, cursorAt());
      flushScroll();
      term.focus();
    };

    // The search's scrolls move herdr's history only. A wheel scroll goes to the app
    // instead when it owns the scroll (alternate screen, mouse tracking), where a jump's
    // many lines would become as many key presses or mouse reports, so none is sent then.
    const historyScrollable = () =>
      !appOwnsScroll(term.buffer.active.type, term.modes.mouseTrackingMode);

    // The search's jumps: one wheel scroll of the given size, after any pending batch.
    // Resolves true once herdr has taken it, false when nothing was sent; rejects when
    // the scroll failed.
    const scrollBy = async (lines: number): Promise<boolean> => {
      flushScroll();
      if (
        lines === 0 ||
        termRef.current !== term ||
        inputBlocked(statusRef.current) ||
        !historyScrollable()
      ) {
        return false;
      }
      const direction: ScrollDirection = lines > 0 ? "up" : "down";
      const { column, row } = cursorAt();
      const n = Math.min(MAX_SCROLL_LINES, Math.abs(lines));
      try {
        await ptyScroll(projectName, name, direction, "wheel", n, column, row);
      } finally {
        liveTail.noteScroll(direction);
      }
      return true;
    };
    historyViewRef.current = {
      scrollBy,
      // Scrolls down by the offset herdr reports, never blindly by the maximum.
      returnToLive: () => {
        if (!historyScrollable()) return;
        if (liveTail.visible) {
          scrollToLiveRef.current();
          return;
        }
        void ptyScrollOffset(projectName, name)
          .then((offset) => (offset ? scrollBy(-offset) : false))
          .catch(() => {});
      },
    };

    function queueScroll(
      direction: ScrollDirection,
      source: ScrollSource,
      lines: number,
      cell: Cell,
    ) {
      // herdr routes a wheel and a page key differently, so they never share a batch.
      if (
        pendingScroll &&
        (pendingScroll.direction !== direction || pendingScroll.source !== source)
      ) {
        flushScroll();
      }
      if (pendingScroll) {
        pendingScroll.lines = Math.min(MAX_SCROLL_LINES, pendingScroll.lines + lines);
        pendingScroll.column = cell.column;
        pendingScroll.row = cell.row;
      } else {
        pendingScroll = { direction, source, lines: Math.min(MAX_SCROLL_LINES, lines), ...cell };
      }
      if (scrollRaf === null) {
        scrollRaf = requestAnimationFrame(() => {
          scrollRaf = null;
          flushScroll();
        });
      }
    }

    // The painted screen, looked up on the first wheel event (it exists once the
    // terminal is open, which any wheel event over it implies) and kept. Its rect is
    // measured on the first wheel event of an animation frame and reused until the
    // frame fires, so a fast wheel does not force a layout read per event.
    let screen: Element | null = null;
    let screenRect: DOMRect | null = null;
    let screenRectRaf: number | null = null;
    function measureScreen(): DOMRect | null {
      if (screenRect) return screenRect;
      screen ??= term.element?.querySelector(".xterm-screen") ?? null;
      screenRect = screen?.getBoundingClientRect() ?? null;
      screenRectRaf ??= requestAnimationFrame(() => {
        screenRectRaf = null;
        screenRect = null;
      });
      return screenRect;
    }

    term.attachCustomWheelEventHandler((e: WheelEvent) => {
      // Ctrl+wheel is left to the webview (zoom); xterm still mustn't act on it.
      if (e.ctrlKey) return false;
      // Horizontal-only movement has nothing to scroll.
      if (e.deltaY === 0) return false;
      e.preventDefault();
      if (inputBlocked(statusRef.current)) {
        wheelRemainderRef.current = 0;
        return false;
      }

      // Cell size comes from the painted screen, so it tracks font size and zoom.
      // An unmeasured screen or empty grid gives a non-finite or zero cell size,
      // which wheelCell maps to cell 0 and accumulateWheelLines to a fallback row.
      const rect = measureScreen();
      const cols = term.cols;
      const rows = term.rows;
      const cellW = (rect?.width ?? 0) / cols;
      const cellH = (rect?.height ?? 0) / rows;
      const cell = wheelCell(
        e.clientX - (rect?.left ?? 0),
        e.clientY - (rect?.top ?? 0),
        cellW,
        cellH,
        cols,
        rows,
      );

      const { lines, remainder } = accumulateWheelLines(
        e.deltaY,
        cellH,
        wheelRemainderRef.current,
        e.deltaMode,
        rows,
      );
      wheelRemainderRef.current = remainder;
      const direction = lines < 0 ? "up" : "down";

      if (wheelDebug) {
        console.log("[keitora wheel]", {
          deltaMode: e.deltaMode,
          deltaY: e.deltaY,
          lines,
          cell,
          direction: lines === 0 ? null : direction,
        });
      }

      if (lines !== 0) queueScroll(direction, "wheel", Math.abs(lines), cell);
      return false;
    });

    let cancelled = false;
    let unResume: UnlistenFn | undefined;
    (async () => {
      // Output and exit arrive on each attach's own channel (see doAttach).
      // The backend says how it brought the agent back (resumed / continued / fell back).
      const resume = await onPtyResume(projectName, name, (n) => {
        const line = resumeNoticeLine(n);
        if (!cancelled && line) term.writeln(dimLine(line));
      });
      if (cancelled) return void resume();
      unResume = resume;

      // Always attach-first: the daemon is the source of truth for whether a live
      // session exists. doAttach() waits out the brief post-mount connecting
      // window and falls back to spawn() only on an authoritative NoLiveSession.
      // An attach that completes after this pane unmounted releases itself (by its
      // attach id), so a newer pane on the same session keeps its attach.
      await doAttach();
    })();

    return () => {
      cancelled = true;
      cancelRefit();
      compositionHost?.removeEventListener("compositionstart", onCompositionStart);
      compositionHost?.removeEventListener("compositionend", onCompositionEnd);
      dataDisp.dispose();
      host?.removeEventListener("paste", onNativePaste, true);
      if (scrollRaf !== null) cancelAnimationFrame(scrollRaf);
      if (screenRectRaf !== null) cancelAnimationFrame(screenRectRaf);
      pendingScroll = null;
      liveTail.dispose();
      liveTailRef.current = null;
      scrollToLiveRef.current = () => {};
      historyViewRef.current = IDLE_HISTORY_VIEW;
      unResume?.();
      webglRef.current?.dispose();
      webglRef.current = null;
      links.dispose();
      linksRef.current = null;
      term.dispose();
      // Detach instead of kill: daemon keeps session alive for reattach. Every
      // attach this pane still holds is released (the live one, and one a re-attach
      // in flight would have replaced), whatever the session's state. Each detach is
      // scoped, so a newer pane already attached to the session (this detach can
      // land after its attach) keeps it.
      outputGateRef.current?.close();
      outputGateRef.current = null;
      coalescerRef.current?.dispose();
      coalescerRef.current = null;
      for (const attachId of attachmentsRef.current.release()) {
        void ptyDetach(projectName, name, attachId).catch(() => {});
      }
      termRef.current = null;
      openedRef.current = false;
    };
  }, [name, path, projectName]);

  // (FIX #5) The direct `onDaemonStatus(...)` listener was removed: the
  // prop-driven `daemonInfo.state` effect above is now the single owner of
  // daemon-connectivity reactions, so two effects can't race two concurrent
  // doAttach() calls.

  // Output is written only while the pane is on screen (the same signal that holds the
  // WebGL context below); a hidden pane buffers it and catches up on show.
  useEffect(() => {
    coalescerRef.current?.setVisible(active);
  }, [active]);

  // Activation: open on first show (only when not still attaching), hold a WebGL context
  // only while visible, and fit/resize only when visible (a hidden host measures 0×0).
  // biome-ignore lint/correctness/useExhaustiveDependencies: re-arm on active/name/status; stable callbacks cancelRefit/refitAndResize intentionally excluded
  useEffect(() => {
    if (!active) linksRef.current?.hide();
    const term = termRef.current;
    const host = hostRef.current;
    const fit = fitRef.current;
    if (!term || !host || !fit) return;

    if (active) {
      // Open the terminal only when not in "attaching" state (the attach flow
      // opens it itself: before the attach when the host is laid out, else after
      // the scrollback is ready).
      if (!openedRef.current && status !== "attaching") {
        term.open(host);
        openedRef.current = true;
      }
      if (openedRef.current) loadWebgl(term);
      if (openedRef.current) {
        // Reliable multi-frame fit replaces the old single rAF that could
        // measure before the renderer was warm and ship xterm's default 80×24.
        refitAndResize();
        // Don't re-grab terminal focus for a dead session — the overlay's button
        // owns focus while the pane is not running.
        if (status === "running") {
          const raf = requestAnimationFrame(() => {
            if (statusRef.current !== "running") return;
            termRef.current?.focus();
            // The renderer (xterm + WebGL) is now live. An idle restored pane streams no
            // new output, so the one frame captured at attach may have been written before
            // the renderer was ready → blank terminal + block cursor. Ask the backend to
            // push a fresh viewport frame now so it lands in a renderer that can paint it.
            // Only right after a (re)attach — a plain switch back to an already-painted pane
            // keeps its xterm buffer, and a stale size is corrected by refitAndResize above,
            // so re-taking-over the herdr bridge on every activation is just churn.
            if (pendingRepaintRef.current) {
              pendingRepaintRef.current = false;
              void ptyRepaint(projectName, name).catch(() => {
                /* best-effort; the bridge still streams any real output */
              });
            }
          });
          return () => {
            cancelRefit();
            cancelAnimationFrame(raf);
          };
        }
        return () => cancelRefit();
      }
    } else {
      // Off-screen: stop any in-flight refit (its next tick would otherwise still
      // commit a fit/ptyResize during the brief window before the host measures 0×0)
      // and release the GPU context.
      cancelRefit();
      webglRef.current?.dispose();
      webglRef.current = null;
    }
  }, [active, name, status]);

  // File drops: App.tsx watches the webview drag-drop event (only it sees OS
  // file paths) and dispatches the paths at the pane under the cursor. We inject here
  // via term.paste() rather than letting App call ptyWrite, so a drop obeys the same
  // bracketed-paste + onData gate as typing (attaching / daemon-down).
  useEffect(() => {
    const el = paneRef.current;
    if (!el) return;
    const onFileDrop = (ev: Event) => {
      // Only a live session can receive a drop. The onData gate below covers
      // attaching/detached-alive/daemon-down but not ended/lost/spawn-failed — and on a
      // dead pane the focus() call would pull focus off the "Kill & Restart" button
      // (which the status effect deliberately focuses) onto a terminal going nowhere.
      if (statusRef.current !== "running") return;
      const { paths } = (ev as CustomEvent<FileDropDetail>).detail ?? { paths: [] };
      const text = injectionTextForPaths(paths);
      if (!text) return;
      termRef.current?.paste(text);
      // Match a real terminal: the path lands at the prompt with focus, ready to submit.
      termRef.current?.focus();
    };
    el.addEventListener(FILE_DROP_EVENT, onFileDrop);
    return () => el.removeEventListener(FILE_DROP_EVENT, onFileDrop);
  }, []);

  // Resize follows the host, but only while visible; debounced.
  // biome-ignore lint/correctness/useExhaustiveDependencies: re-arm on active/name/projectName; stable refitAndResize excluded from deps
  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    let t: number | undefined;
    const ro = new ResizeObserver(() => {
      if (!active) return;
      window.clearTimeout(t);
      t = window.setTimeout(() => {
        refitAndResize();
      }, 16);
    });
    ro.observe(host);
    return () => {
      ro.disconnect();
      window.clearTimeout(t);
    };
  }, [active, name, projectName]);

  // When the session is not running, move focus off the dead terminal onto the
  // relevant overlay button so it's reachable by keyboard.
  useEffect(() => {
    if (status === "running" || status === "attaching") return;
    termRef.current?.blur();
    const raf = requestAnimationFrame(() => restartBtnRef.current?.focus());
    return () => cancelAnimationFrame(raf);
  }, [status]);

  // Apply font size changes live and refit (driven by the Cmd +/- shortcut).
  // biome-ignore lint/correctness/useExhaustiveDependencies: apply font size on fontSize/active/name/projectName change; stable refitAndResize excluded
  useEffect(() => {
    const term = termRef.current;
    const fit = fitRef.current;
    if (!term || !fit || !active) return;
    term.options.fontSize = fontSize;
    refitAndResize();
  }, [fontSize, active, name, projectName]);

  // Swap the terminal palette live when the app theme changes, so every open pane
  // flips together (no reopen). Assigning options.theme updates xterm's colors; the
  // refresh forces the WebGL renderer to repaint the existing buffer.
  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    term.options.theme = THEMES[resolvedTheme];
    // Only force a repaint once the terminal is actually opened (element attached);
    // refreshing an unopened pane has no renderer to drive. Opened panes repaint so the
    // swap is immediate even for the WebGL renderer.
    if (term.element) term.refresh(0, term.rows - 1);
  }, [resolvedTheme]);

  const retryConnection = useCallback(() => {
    // Calling daemonStatus() triggers the client to check connection state.
    void daemonStatus();
  }, []);

  // Determine if "Kill & Restart" needs an inline confirm step.
  const needsConfirm = (s: Status): boolean =>
    s !== "ended" &&
    s !== "ended-while-away" &&
    s !== "lost" &&
    s !== "spawn-failed" &&
    s !== "terminated";

  const handleKillRestart = (fresh = false) => {
    if (needsConfirm(statusRef.current)) {
      setConfirmKill(fresh ? "fresh" : "resume");
    } else {
      void restart(fresh);
    }
  };
  const confirmCopy =
    confirmKill === "terminate"
      ? terminateConfirmCopy()
      : restartConfirmCopy(confirmKill === "fresh" ? "fresh" : "resume");

  return (
    // data-project/data-name are how a file drop finds this pane's PTY identity
    // (App.tsx hit-tests with elementFromPoint — see terminalFileDrop.ts).
    <div
      className={isDropTarget ? "pane drop-target" : "pane"}
      style={{ display: active ? "flex" : "none" }}
      ref={paneRef}
      data-project={projectName}
      data-name={name}
    >
      <div className="term-host" ref={hostRef} />
      {searchVisible && termRef.current && (
        <TerminalSearch
          term={termRef.current}
          project={projectName}
          id={name}
          history={historyViewRef}
          onClose={() => {
            setSearchOpen(false);
            termRef.current?.focus();
          }}
        />
      )}

      {/* "↩ reattached" pill */}
      {showReattachedPill && <div className="pane-reattached-pill">↩ reattached</div>}

      {/* "↓ live" pill: shown while the viewport is scrolled into history */}
      {showLivePill && (
        <button
          type="button"
          className="pane-btn pane-live-pill"
          title="Back to the live output (End)"
          // Keep focus in the terminal: a click must not steal it.
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => scrollToLiveRef.current()}
        >
          ↓ live
        </button>
      )}

      {/* Per-state overlay — matches the full table in 04-frontend-ux.md §4.2 */}
      {status !== "running" && (
        // A suspended pane resumes on a click anywhere; its Resume button is the
        // keyboard path.
        <div
          className="pane-overlay"
          onClick={
            status === "suspended" && !transferringTo
              ? () => void recoveryFocus(projectName, name)
              : undefined
          }
        >
          {transferringTo ? (
            // CLI transfer in progress: the backend may have
            // already killed this pane's live session (Codex→Claude, before the
            // seconds-long summary step) — show an intentional "Transferring…"
            // indicator instead of the alarming recovery overlay/"Kill & Restart"
            // the underlying status would otherwise render.
            <>
              <div className="pane-spinner" aria-label="Transferring" />
              <div className="pane-msg">
                Transferring to {transferringTo === "codex" ? "Codex" : "Claude"}…
              </div>
            </>
          ) : (
            <>
              {status === "attaching" && showSpinner && (
                <>
                  <div className="pane-spinner" aria-label="Connecting" />
                  <div className="pane-msg">Connecting to session…</div>
                </>
              )}

              {status === "detached-alive" && (
                <div className="pane-msg">Session active — reattaching…</div>
              )}

              {status === "recovering" && (
                <>
                  <div className="pane-spinner" aria-label="Recovering" />
                  <div className="pane-msg">{recoveryPaneMessage(recovery, recItem)}</div>
                  {recItem && (
                    <div className="pane-btn-row">
                      <button
                        type="button"
                        className="pane-btn pane-btn-secondary"
                        onClick={() => void startFreshFromRecovery()}
                      >
                        Start fresh conversation
                      </button>
                    </div>
                  )}
                </>
              )}

              {status === "suspended" && (
                <>
                  <div className="pane-msg">Suspended after herdr crash — click to resume</div>
                  <div className="pane-btn-row">
                    <button
                      type="button"
                      ref={restartBtnRef}
                      className="pane-btn"
                      onClick={(e) => {
                        e.stopPropagation();
                        void recoveryFocus(projectName, name);
                      }}
                    >
                      Resume
                    </button>
                    <button
                      type="button"
                      className="pane-btn pane-btn-secondary"
                      onClick={(e) => {
                        e.stopPropagation();
                        void startFreshFromRecovery();
                      }}
                    >
                      Start fresh conversation
                    </button>
                  </div>
                </>
              )}

              {status === "daemon-down" && (
                <>
                  <div className="pane-msg">Session paused — daemon unreachable</div>
                  <button ref={restartBtnRef} className="pane-btn" onClick={retryConnection}>
                    Retry connection
                  </button>
                </>
              )}

              {status === "terminated" && (
                <>
                  <div className="pane-msg">Session terminated</div>
                  <div className="pane-btn-row">
                    <button ref={restartBtnRef} className="pane-btn" onClick={() => void launch()}>
                      Launch
                    </button>
                    <button
                      className="pane-btn pane-btn-secondary"
                      onClick={() => void launch(true)}
                    >
                      Launch fresh
                    </button>
                  </div>
                </>
              )}

              {(status === "spawn-failed" ||
                status === "ended" ||
                status === "ended-while-away" ||
                status === "lost") && (
                <>
                  <div className="pane-msg">
                    {status === "spawn-failed" && "Failed to launch claude — is it on PATH?"}
                    {status === "ended" && "session ended"}
                    {status === "ended-while-away" &&
                      "Session ended while away — restart to continue"}
                    {status === "lost" && "Session lost (daemon restarted) — start a new session"}
                  </div>
                  {/* Confirm surface (when needed) is hoisted below, outside this
                      status !== "running" gate — see the standalone `confirmKill` block. Hide this
                      button while it's open so the two don't render on top of each other. */}
                  {!confirmKill && (
                    <div className="pane-btn-row">
                      <button
                        ref={restartBtnRef}
                        className="pane-btn"
                        onClick={() => handleKillRestart()}
                      >
                        Kill &amp; Restart
                      </button>
                      <button
                        className="pane-btn pane-btn-secondary"
                        onClick={() => handleKillRestart(true)}
                      >
                        Start fresh conversation
                      </button>
                    </div>
                  )}
                </>
              )}
            </>
          )}
        </div>
      )}

      {/* Standalone confirm surface: hoisted OUT of the `status !== "running"` overlay
          gate above so it can render for a still-"running" (wedged) pane too — the case the
          overlay never covers. Gated only on `confirmKill`, driven by BOTH the error-state
          "Kill & Restart" button above and the prop-driven restartNonce effect. Wrapped in its
          OWN `.pane-overlay` because `.pane-confirm` has no standalone positioning — outside the
          overlay it renders as an invisible zero-context block. */}
      {confirmKill && !transferringTo && (
        <div className="pane-overlay">
          <div className="pane-confirm">
            <div className="pane-msg">{confirmCopy.message}</div>
            <div className="pane-btn-row">
              <button
                className="pane-btn"
                onClick={() => {
                  const choice = confirmKill;
                  setConfirmKill(false);
                  if (choice === "terminate") void terminate();
                  else void restart(choice === "fresh");
                }}
              >
                {confirmCopy.confirmLabel}
              </button>
              <button className="pane-btn pane-btn-secondary" onClick={() => setConfirmKill(false)}>
                Cancel
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
