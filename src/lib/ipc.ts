// Typed bridge to the Rust backend: worktree listing, PTY control, and event
// subscriptions. Output arrives base64-encoded (see src-tauri pty batching).

import type { UnlistenFn } from "@tauri-apps/api/event";
import type { Dims } from "./termFit";
import { getTransport } from "./transport";

// Route every backend call through the active transport (localTransport —
// a verbatim Tauri invoke/listen pass-through). `decodeBase64` is re-exported for
// TerminalPane.
function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return getTransport().invoke<T>(cmd, args);
}
function listen<T>(channel: string, cb: (payload: T) => void): Promise<UnlistenFn> {
  return getTransport().listen<T>(channel, cb);
}

export type WorktreeStatus = "available" | "missing" | "not_dir" | "not_git";

/** Which coding CLI drives a worktree's session. The single place this
 *  union is declared in TS — other modules import it from here. */
export type CliKind = "claude" | "codex";

// ── Daemon status types ────────────────────────────────────────────────────────

/**
 * Discriminated union matching the `daemon://status` event payload.
 * Values match the backend's `StatusPayload` state strings.
 */
export type DaemonState =
  | "NeverStarted" // synthesized client-side; before the first status arrives
  | "Connected" // herdr reachable and healthy
  | "Disconnected"; // herdr unavailable (see `reason`) or the connection was lost

/**
 * Snapshot of daemon connection state + metadata.
 * Mirrors the backend `StatusPayload` struct.
 *
 * Field notes:
 *   - `started_at`: u64 unix-epoch seconds from Rust → number here.
 *   - `daemon_version`: present when state is "Connected".
 *   - `session_count`: present when state is "Connected" (count of live daemon sessions).
 *   - `log_path`: absolute path of herdr's log; null or empty when unknown.
 *   - `reason`: why herdr is unavailable (with the fix), when the backend says. While herdr
 *     cannot be found the recovery payload carries the same text with
 *     `reason_code: "herdr_not_found"`.
 *   - `restartable`: true when pressing Restart can help (the herdr backend); false for the
 *     unavailable backend, which has to be relaunched. Absent in older payloads, so false.
 *   - `pid`: present when state is "Connected".
 */
export interface DaemonInfo {
  state: DaemonState;
  pid: number | null;
  started_at: number | null; // unix epoch seconds (u64 from Rust)
  session_count: number; // 0 when not Connected
  log_path: string | null; // absolute path; null or empty when unknown
  daemon_version: number | null; // non-null when Connected
  reason?: string; // why herdr is unavailable, when the backend gives one
  restartable: boolean; // whether a restart can help; false when the payload omits it
}

// Raw shape coming off the wire: optional fields may be absent (skip_serializing_if).
interface RawStatusPayload {
  state: DaemonState;
  pid?: number;
  started_at?: number;
  session_count?: number;
  log_path?: string;
  daemon_version?: number;
  reason?: string;
  restartable?: boolean;
}

/**
 * Normalize the raw StatusPayload from Rust into DaemonInfo.
 * The backend uses skip_serializing_if for optional fields, so absent fields
 * are missing from the JSON object. Fill them in as null/0 for TS consumers.
 */
function normalizeStatusPayload(raw: RawStatusPayload): DaemonInfo {
  return {
    state: raw.state,
    pid: raw.pid ?? null,
    started_at: raw.started_at ?? null,
    session_count: raw.session_count ?? 0,
    log_path: raw.log_path ?? null,
    daemon_version: raw.daemon_version ?? null,
    reason: raw.reason,
    restartable: raw.restartable ?? false,
  };
}

/** Snapshot the current daemon connection state + metadata. */
export function daemonStatus(): Promise<DaemonInfo> {
  return invoke<RawStatusPayload>("daemon_status").then(normalizeStatusPayload);
}

/** Subscribe to daemon connection-state changes (`daemon://status` event). */
export function onDaemonStatus(cb: (info: DaemonInfo) => void): Promise<UnlistenFn> {
  return listen<RawStatusPayload>("daemon://status", (payload) =>
    cb(normalizeStatusPayload(payload)),
  );
}

// ── Server recovery ────────────────────────────────────────────────────────────

/** Where herdr server recovery stands (the backend's phase names). */
export type RecoveryPhase =
  | "Healthy"
  | "Down"
  | "AwaitHeadroom"
  | "Restarting"
  | "Reconciling"
  | "Resuming"
  | "Paused";

/** Where one queued session stands. */
export type RecoveryItemState = "queued" | "resuming" | "suspended";

/** One session owed a resume. */
export interface RecoveryQueueItem {
  project: string;
  worktree: string;
  state: RecoveryItemState;
}

/** A session identified by project and worktree. */
export interface RecoverySessionRef {
  project: string;
  worktree: string;
}

/**
 * The `daemon://recovery` event body and the `daemon_recovery` answer. Every
 * field is always present ("none" is `null`). `seq` grows with every snapshot.
 */
export interface RecoveryInfo {
  phase: RecoveryPhase;
  reason: string | null;
  reason_code: string | null;
  queue: RecoveryQueueItem[];
  /** Sessions whose resume succeeded in this episode, sorted by project then worktree. */
  settled: RecoverySessionRef[];
  resumed: number;
  failed: number;
  total: number;
  generation: number;
  can_resume_now: boolean;
  seq: number;
}

/** Nothing to recover (also the value before the first snapshot arrives). */
export const HEALTHY_RECOVERY: RecoveryInfo = {
  phase: "Healthy",
  reason: null,
  reason_code: null,
  queue: [],
  settled: [],
  resumed: 0,
  failed: 0,
  total: 0,
  generation: 0,
  can_resume_now: false,
  seq: 0,
};

/** Snapshot the server recovery state. */
export function recoveryStatus(): Promise<RecoveryInfo> {
  return invoke<RecoveryInfo>("daemon_recovery");
}

/**
 * The one-time "panes now use your normal tool folders" notice: the old
 * isolated tool folders left in place, or null when there is nothing to say (or
 * it was already shown — taking it marks it shown).
 */
export function takeXdgLeakNotice(): Promise<string[] | null> {
  return invoke<string[] | null>("take_xdg_leak_notice");
}

/** The banner's "Resume now" / "Resume all": resume what recovery is holding back. */
export function recoveryResumeNow(): Promise<void> {
  return invoke("recovery_resume_now");
}

/** Resume this suspended session next (it became active, or Resume was clicked). */
export function recoveryFocus(project: string, worktree: string): Promise<void> {
  return invoke("recovery_focus", { project, worktree });
}

/**
 * Start this recovering or suspended session with a fresh conversation in
 * recovery's own slot. `false` when recovery holds no entry for it: start it
 * fresh the ordinary way instead.
 */
export function recoveryStartFresh(project: string, worktree: string): Promise<boolean> {
  return invoke<boolean>("recovery_start_fresh", { project, worktree });
}

/** Stop the herdr server. Sessions are not respawned until the next spawn/attach. */
export function daemonShutdown(): Promise<void> {
  return invoke("daemon_shutdown");
}

/** Restart the daemon (stop, then bring the server back up). */
export function daemonRestart(): Promise<void> {
  return invoke("daemon_restart");
}

/**
 * Live process stats for the daemon, rendered in the rail daemon card. Mirrors the
 * `DaemonStats` struct (backend/mod.rs) and the `daemon://stats` event payload. Only the
 * herdr backend fills it; others report `running: false`. Distinct from
 * {@link DaemonInfo}: `version` is the daemon's own version STRING (not the protocol
 * number), and cpu/mem/uptime are sampled by the backend's status loop.
 */
export interface DaemonStats {
  running: boolean;
  pid: number | null;
  uptime_secs: number | null;
  cpu_percent: number | null;
  mem_bytes: number | null;
  version: string | null;
}

interface RawDaemonStats {
  running: boolean;
  pid?: number;
  uptime_secs?: number;
  cpu_percent?: number;
  mem_bytes?: number;
  version?: string;
}

function normalizeDaemonStats(raw: RawDaemonStats): DaemonStats {
  return {
    running: raw.running,
    pid: raw.pid ?? null,
    uptime_secs: raw.uptime_secs ?? null,
    cpu_percent: raw.cpu_percent ?? null,
    mem_bytes: raw.mem_bytes ?? null,
    version: raw.version ?? null,
  };
}

/**
 * The daemon's last sampled process stats (cpu/memory/uptime/version): a cheap read of
 * the backend's cache, not polled; updates arrive as `daemon://stats` events.
 */
export function daemonStats(): Promise<DaemonStats> {
  return invoke<RawDaemonStats>("daemon_stats").then(normalizeDaemonStats);
}

// ── Worktree activity (rail lanes) ──────────────────────────────────────────────
// Mirrors `activity::WorktreeActivity` (src-tauri/src/activity/mod.rs): each project's
// worktrees, with workflow stage, git branch, UBT build progress, and a working flag.
// No process/agent counts — internal Claude Task subagents are OS-invisible.

/** A worktree's `/workflow` state (subset of the on-disk `WORKFLOW_STATE.json`). */
export interface WorkflowState {
  stage: string;
  stageOrdinal: number | null;
  stageTotal: number;
  stageDetail: string | null;
  workNumber: string | null;
  branch: string | null;
  branchMismatch: boolean;
  done: number | null;
  total: number | null;
  prUrl: string | null;
  complexity: string | null;
  fileMtime: number;
  stale: boolean;
  unreadable: boolean;
}

/** UBT build progress for a worktree. */
export interface BuildProgress {
  adapter: string;
  label: string;
  done: number;
  total: number;
  phase: string | null;
  state: "running" | "succeeded" | "failed";
  startedAt: number;
  etaSecs: number | null;
  percent: number;
  message: string | null;
}

/** One worktree's current activity, rendered on its rail lane. */
export interface WorktreeActivity {
  project: string;
  worktree: string;
  cli: string;
  branch: string | null;
  workflow: WorkflowState | null;
  build: BuildProgress | null;
  working: boolean;
}

interface RawWorkflowState {
  stage: string;
  stage_ordinal?: number;
  stage_total: number;
  stage_detail?: string;
  work_number?: string;
  branch?: string;
  branch_mismatch: boolean;
  done?: number;
  total?: number;
  pr_url?: string;
  complexity?: string;
  file_mtime: number;
  stale: boolean;
  unreadable: boolean;
}

interface RawBuildProgress {
  adapter: string;
  label: string;
  done: number;
  total: number;
  phase?: string;
  state: "running" | "succeeded" | "failed";
  started_at: number;
  eta_secs?: number;
  percent: number;
  message?: string;
}

interface RawWorktreeActivity {
  project: string;
  worktree: string;
  cli: string;
  branch?: string;
  workflow?: RawWorkflowState;
  build?: RawBuildProgress;
  working: boolean;
}

function normalizeWorkflow(raw: RawWorkflowState): WorkflowState {
  return {
    stage: raw.stage,
    stageOrdinal: raw.stage_ordinal ?? null,
    stageTotal: raw.stage_total,
    stageDetail: raw.stage_detail ?? null,
    workNumber: raw.work_number ?? null,
    branch: raw.branch ?? null,
    branchMismatch: raw.branch_mismatch,
    done: raw.done ?? null,
    total: raw.total ?? null,
    prUrl: raw.pr_url ?? null,
    complexity: raw.complexity ?? null,
    fileMtime: raw.file_mtime,
    stale: raw.stale,
    unreadable: raw.unreadable,
  };
}

function normalizeBuild(raw: RawBuildProgress): BuildProgress {
  return {
    adapter: raw.adapter,
    label: raw.label,
    done: raw.done,
    total: raw.total,
    phase: raw.phase ?? null,
    state: raw.state,
    startedAt: raw.started_at,
    etaSecs: raw.eta_secs ?? null,
    percent: raw.percent,
    message: raw.message ?? null,
  };
}

function normalizeWorktreeActivity(raw: RawWorktreeActivity): WorktreeActivity {
  return {
    project: raw.project,
    worktree: raw.worktree,
    cli: raw.cli,
    branch: raw.branch ?? null,
    workflow: raw.workflow ? normalizeWorkflow(raw.workflow) : null,
    build: raw.build ? normalizeBuild(raw.build) : null,
    working: raw.working,
  };
}

/** Activity for every project's worktree (rail lanes). */
export function worktreeActivity(): Promise<WorktreeActivity[]> {
  return invoke<RawWorktreeActivity[]>("worktree_activity").then((rows) =>
    rows.map(normalizeWorktreeActivity),
  );
}

/** A single live session hint returned by `daemon_list_sessions`. */
export interface DaemonSessionHint {
  project: string;
  worktree: string;
  gen: number;
}

/** List all live daemon sessions across all projects. */
export function daemonListSessions(): Promise<DaemonSessionHint[]> {
  return invoke<DaemonSessionHint[]>("daemon_list_sessions");
}

// ── App-quit confirmation ───────────────────────────────────────────────

/**
 * Fires when the window close was intercepted because the daemon holds live
 * sessions (`app://close-requested`). The payload is the live-session count; the
 * frontend drives the quit-confirm modal off it.
 */
export function onAppCloseRequested(cb: (sessionCount: number) => void): Promise<UnlistenFn> {
  // The seam's listen passes the payload directly (localTransport unwraps e.payload).
  return listen<number>("app://close-requested", (count) => cb(count));
}

/** Quit the GUI but leave the daemon + its live sessions running. */
export function appQuitKeep(): Promise<void> {
  return invoke("app_quit_keep");
}

/** Stop all daemon sessions, then quit the GUI. */
export function appQuitStop(): Promise<void> {
  return invoke("app_quit_stop");
}

// ── Attach / Detach types ──────────────────────────────────────────────────────

/**
 * Result of `ptyAttach`. Matches the `AttachPayload` struct in host.rs.
 * scrollback is base64-encoded raw VT bytes (the backend already base64-encoded
 * the raw bytes — write directly to xterm via `term.write(decodeBase64(scrollback))`).
 */
export interface AttachPayload {
  gen: number;
  scrollback: string; // base64-encoded raw VT bytes
  /** Pass to `ptyDetach` so a late detach cannot end a newer attach. */
  attach_id: number;
  /**
   * Whether `ptyRepaint` really repaints this session. When it cannot, a hidden
   * pane must never drop output it has not written.
   */
  repaints: boolean;
}

/**
 * One message on a pane's output channel. Matches `OutputMsg` in host.rs:
 * `data` is base64 VT bytes; `full` marks a whole-viewport repaint.
 */
export type OutputMsg = { type: "frame"; data: string; full: boolean } | { type: "exit" };

/** A takeover size as the host's `rows`/`cols` params: both `null` when unmeasured. */
function sizeArgs(size: Dims | null): { rows: number | null; cols: number | null } {
  return { rows: size?.rows ?? null, cols: size?.cols ?? null };
}

/**
 * Attach to an existing daemon session for `id` (bare worktree name).
 * Returns scrollback bytes (already base64-encoded by the backend), the
 * daemon-side gen, a truncation flag and the attach id. From then on the
 * session's output and exit arrive, in order, on `onOutput` (a channel of this
 * attach only; a later attach replaces it).
 *
 * `size` is the pane's measured size, or `null` when unmeasured (the backend
 * then picks one).
 *
 * Rust param names: project, id, rows, cols, on_output  (rows before cols in
 * host.rs pty_attach). `project` qualifies the session so same-named worktrees
 * in different projects never collide.
 */
export function ptyAttach(
  project: string,
  id: string,
  size: Dims | null,
  onOutput: (msg: OutputMsg) => void,
): Promise<AttachPayload> {
  const channel = getTransport().channel<OutputMsg>(onOutput);
  return invoke<AttachPayload>("pty_attach", { project, id, ...sizeArgs(size), onOutput: channel });
}

/**
 * Detach this GUI from a PTY session — daemon keeps the session live + buffering.
 * Called on pane unmount and project switch. NOT called on user-initiated kill.
 * With `attachId` only that attach is detached: a newer attach is left alone.
 *
 * Rust param names: project, id, attach_id
 */
export function ptyDetach(project: string, id: string, attachId?: number): Promise<void> {
  return invoke("pty_detach", { project, id, attachId: attachId ?? null });
}

export interface WorktreeInfo {
  name: string;
  path: string;
  status: WorktreeStatus;
  /** Resolved default CLI for this worktree: registry entry `cli`, else
   *  the user default, else `claude`. Drives the rail's provider glyph. */
  cli: CliKind;
  /** The user terminated this worktree's session: it stays down (no resume,
   *  no respawn) until launched again. */
  terminated: boolean;
}

/** List configured worktrees, each resolved to a path + filesystem status. */
export function listWorktrees(): Promise<WorktreeInfo[]> {
  return invoke<WorktreeInfo[]>("worktrees");
}

/**
 * Spawn the session for `id` in `cwd` under `project`. `command` overrides the
 * default Claude launcher. `project` is the daemon namespace. The default launch
 * resumes the worktree's conversation; `fresh` starts a new, empty one instead.
 * `size` is `null` when the pane is unmeasured (the backend picks one).
 */
export function ptySpawn(
  project: string,
  id: string,
  cwd: string,
  size: Dims | null,
  command?: string,
  fresh?: boolean,
): Promise<number> {
  return invoke<number>("pty_spawn", {
    project,
    id,
    cwd,
    ...sizeArgs(size),
    command: command ?? null,
    fresh: fresh ?? false,
  });
}

/** Reap the backend session for `id` (under `project`) at generation `gen` (no-op if superseded). */
export function ptyKill(project: string, id: string, gen: number): Promise<void> {
  return invoke("pty_kill", { project, id, gen });
}

/**
 * Terminate the session for `id` under `project`: mark it terminated (so nothing
 * resumes or respawns it) and kill it at `gen`, or at its live generation when
 * `gen` is omitted. Rejects only when the mark could not be stored or the kill
 * failed; a kill refused while herdr recovers still succeeds.
 *
 * Rust param names: project, id, gen
 */
export function sessionTerminate(project: string, id: string, gen?: number): Promise<void> {
  return invoke("session_terminate", { project, id, gen: gen ?? null });
}

/**
 * Launch a terminated session again: clear the mark, then spawn it like
 * `ptySpawn` (resuming its conversation unless `fresh`). Resolves to the new
 * generation; spawn refusals come back with `ptySpawn`'s tokens.
 *
 * Rust param names: project, id, cwd, rows, cols, fresh
 */
export function sessionLaunch(
  project: string,
  id: string,
  cwd: string,
  size: Dims | null,
  fresh?: boolean,
): Promise<number> {
  return invoke<number>("session_launch", {
    project,
    id,
    cwd,
    ...sizeArgs(size),
    fresh: fresh ?? false,
  });
}

/** `t0` is the latency probe's keystroke stamp (epoch ms); omitted when the probe is off. */
export function ptyWrite(project: string, id: string, data: string, t0?: number): Promise<void> {
  return invoke("pty_write", t0 === undefined ? { project, id, data } : { project, id, data, t0 });
}

/** Whether the backend latency probe is on (read once at startup). */
export function latencyProbeEnabled(): Promise<boolean> {
  return invoke("latency_probe_enabled");
}

/**
 * Save pasted clipboard image bytes to a temp file; resolves to its absolute path.
 * Bytes travel as base64 (like PTY output) — a number[] would make a 4K screenshot
 * tens of MB of JSON to serialize on the UI thread.
 */
export function saveClipboardImage(bytesB64: string, ext: string): Promise<string> {
  return invoke("save_clipboard_image", { bytesB64, ext });
}

export function openTerminalFile(project: string, worktree: string, path: string): Promise<void> {
  return invoke("open_terminal_file", { project, worktree, path });
}

// ── Diff view ────────────────────────────────────────────────────────────────
// Read-only worktree diff (merge-base of main → working tree). The backend emits
// camelCase (serde rename_all), so these need no normalization. Diff base, caching,
// cancellation and whitespace handling all live in the Rust `diff` module.

export type DiffStatus = "added" | "modified" | "deleted" | "renamed" | "untracked";

/** One changed file in the summary (cheap, tree-populating payload). */
export interface DiffFileSummary {
  path: string;
  /** Present for renames/copies — the pre-rename path. */
  oldPath?: string;
  status: DiffStatus;
  added: number;
  deleted: number;
  binary: boolean;
}

export type DiffLineKind = "context" | "add" | "del";

/** One syntax-highlight run within a line: `len` code points carrying class id `cls` (a 0-based
 *  index into the backend's class table; absent → no class, render as plain text). Spans cover the
 *  line's `text` in order. */
export interface DiffSpan {
  len: number;
  cls?: number;
}

export interface DiffLine {
  kind: DiffLineKind;
  /** 1-based line number on the old side (absent for additions). */
  oldNo?: number;
  /** 1-based line number on the new side (absent for deletions). */
  newNo?: number;
  text: string;
  /** Syntax-highlight spans covering `text`; absent when the file isn't highlighted. */
  spans?: DiffSpan[];
}

export interface DiffHunk {
  oldStart: number;
  oldLines: number;
  newStart: number;
  newLines: number;
  header: string;
  lines: DiffLine[];
}

export interface FileDiff {
  binary: boolean;
  /** The diff exceeded the size cap and was not fully rendered. */
  truncated: boolean;
  hunks: DiffHunk[];
}

/** Full changed-file summary (committed + uncommitted + untracked). One working-tree scan
 *  server-side; reuses a cached result while the filesystem watcher sees no change. */
export function worktreeDiffSummary(
  worktree: string,
  ignoreWs: boolean,
): Promise<DiffFileSummary[]> {
  return invoke<DiffFileSummary[]>("worktree_diff_summary", { worktree, ignoreWs });
}

/** Committed changes only (`merge-base..HEAD`) — no working-tree scan, for instant first
 *  paint while the full summary computes. */
export function worktreeDiffCommitted(
  worktree: string,
  ignoreWs: boolean,
): Promise<DiffFileSummary[]> {
  return invoke<DiffFileSummary[]>("worktree_diff_committed", { worktree, ignoreWs });
}

/** Structured diff for one file (lazy on selection; cancellable backend-side). `untracked`
 *  lets the backend skip the probe that distinguishes an untracked file from a no-op diff. */
export function worktreeFileDiff(
  worktree: string,
  path: string,
  oldPath: string | undefined,
  ignoreWs: boolean,
  untracked: boolean,
): Promise<FileDiff> {
  return invoke<FileDiff>("worktree_file_diff", {
    worktree,
    path,
    oldPath: oldPath ?? null,
    ignoreWs,
    untracked,
  });
}

/** Scan-free liveness token (`"{watcherGen}:{head}:{mergeBase}"`) for the open view. */
export function worktreeDiffRevision(worktree: string): Promise<string> {
  return invoke<string>("worktree_diff_revision", { worktree });
}

/** What the diff view compares the working tree against. Discriminated on `kind`:
 *  - `auto` — origin/main → main → origin/HEAD, via merge-base (the default).
 *  - `ref` — the tip of `name` (every difference from that ref as it is now).
 *  - `upstream` — the branch's `@{upstream}`, via merge-base.
 *  - `forkPoint` — merge-base of `parent` and HEAD. */
export type DiffBase =
  | { kind: "auto" }
  | { kind: "ref"; name: string }
  | { kind: "upstream" }
  | { kind: "forkPoint"; parent: string };

/** How HEAD is compared against the resolved base ref. */
export type DiffCompare = "mergeBase" | "tip";

/** The resolved diff base for a worktree, for the header label + error banner. */
export interface DiffBaseInfo {
  /** The concrete ref being compared against (e.g. `origin/main`). */
  refName: string;
  compare: DiffCompare;
  /** Set when a configured ref could not be resolved; the diff is then empty. */
  error?: string;
}

/** Resolve the effective diff base for a worktree (per-worktree override → project default →
 *  Auto). Cheap: served from a short-TTL backend cache. */
export function worktreeDiffBase(worktree: string): Promise<DiffBaseInfo> {
  return invoke<DiffBaseInfo>("worktree_diff_base", { worktree });
}

/** Set (or clear, with `base: null`) the per-worktree diff-base override. Emits
 *  `project://changed`; the backend invalidates its base cache so the next fetch reflects it. */
export function setWorktreeDiffBase(worktree: string, base: DiffBase | null): Promise<void> {
  return invoke("set_worktree_diff_base", { worktree, base });
}

/** Ref++ the worktree's filesystem watcher while a diff view is open on it. */
export function worktreeDiffWatchStart(worktree: string): Promise<void> {
  return invoke("worktree_diff_watch_start", { worktree });
}

/** Ref-- the worktree's filesystem watcher when the diff view closes. */
export function worktreeDiffWatchStop(worktree: string): Promise<void> {
  return invoke("worktree_diff_watch_stop", { worktree });
}

export function ptyResize(project: string, id: string, rows: number, cols: number): Promise<void> {
  return invoke("pty_resize", { project, id, rows, cols });
}

/** Which way a pane scroll moves: `up` is back into history. */
export type ScrollDirection = "up" | "down";

/**
 * What a pane scroll came from. herdr routes it to an app that handles scrolling
 * itself as one wheel report (or arrow key) for `wheel`, one PageUp/PageDown key for
 * `page_key`; over a plain shell both scroll its own history.
 */
export type ScrollSource = "wheel" | "page_key";

/** The most lines one `ptyScroll` can carry (the wire field is a u16). */
export const MAX_SCROLL_LINES = 0xffff;

/**
 * Scroll a pane's viewport by `lines` (a whole number, 1..MAX_SCROLL_LINES) from `source`,
 * with the pointer at the 0-based cell `column`,`row`. herdr routes it: to the app when it
 * reports the mouse, uses alternate scroll or takes page keys, otherwise through its own
 * history.
 */
export function ptyScroll(
  project: string,
  id: string,
  direction: ScrollDirection,
  source: ScrollSource,
  lines: number,
  column: number,
  row: number,
): Promise<void> {
  return invoke("pty_scroll", { project, id, direction, source, lines, column, row });
}

/**
 * How many lines above the live tail a pane's viewport is scrolled (0 = live), or
 * null when the backend cannot tell. One call per ask: it runs a CLI command, so
 * never poll it.
 */
export function ptyScrollOffset(project: string, id: string): Promise<number | null> {
  return invoke("pty_scroll_offset", { project, id });
}

/**
 * Ask the backend to push one fresh repaint frame for a session. Called once the
 * terminal's renderer is live so an idle restored pane paints on boot instead of showing
 * a blank terminal with a blinking block cursor.
 */
export function ptyRepaint(project: string, id: string): Promise<void> {
  return invoke("pty_repaint", { project, id });
}

// Re-exported from the shared module so existing `import { decodeBase64 } from "./ipc"`
// call sites keep working (the impl lives in ./base64 to avoid an import cycle).
export { decodeBase64 } from "./base64";

/** How the backend brought a session's agent back (see `pty://resume/...`). */
export interface PtyResumeEvent {
  outcome: string;
  fell_back: boolean;
}

/**
 * Subscribe to the notice the backend sends when it resumed a session's
 * conversation (or had to fall back). Best-effort: a transport that does not
 * carry the event resolves to a no-op unlisten instead of failing.
 */
export function onPtyResume(
  project: string,
  id: string,
  cb: (e: PtyResumeEvent) => void,
): Promise<UnlistenFn> {
  const noop: UnlistenFn = () => {};
  try {
    return listen<PtyResumeEvent>(`pty://resume/${project}/${id}`, cb).catch(() => noop);
  } catch {
    return Promise.resolve(noop);
  }
}

/** Fires when task-config.json changes (worktrees added/removed/renamed). */
export function onWorktreesChanged(cb: () => void): Promise<UnlistenFn> {
  return listen("worktrees://changed", () => cb());
}

export interface ProjectSummary {
  name: string;
  resolved_name: string; // [project].name; the session/activity namespace
  root: string; // absolute path to the project root (dir containing keitora.toml)
  active: boolean; // whether this is the currently active project
  db_path: string; // absolute path to the SQLite task DB
  worktrees: string[]; // configured worktree names
  backend: string; // backend kind from [tasks] backend (e.g. "sqlite" | "github")
}

/** Return the active project, or null if no project is configured. */
export function currentProject(): Promise<ProjectSummary | null> {
  return invoke<ProjectSummary | null>("current_project");
}

/** Return all registered projects. */
export function listProjects(): Promise<ProjectSummary[]> {
  return invoke<ProjectSummary[]>("list_projects");
}

/** Switch the active project by name. */
export function setActiveProject(name: string): Promise<void> {
  return invoke("set_active_project", { name });
}

/** Persist the project display order (a permutation of the registry keys). */
export function reorderProjects(order: string[]): Promise<void> {
  return invoke("reorder_projects", { order });
}

/**
 * Register and activate a project at the given folder path.
 * If the folder has no keitora.toml the backend scaffolds one.
 * Throws a descriptive string on error (not a git repo, etc.).
 */
export function onboardProject(path: string): Promise<ProjectSummary> {
  return invoke<ProjectSummary>("onboard_project", { path });
}

/**
 * Detect a legacy task-config.json WITHOUT migrating it. Returns its path, or
 * null if none found — so onboarding can offer import instead of writing silently.
 */
export function detectLegacy(): Promise<string | null> {
  return invoke<string | null>("detect_legacy");
}

/**
 * Run the legacy migration (task-config.json → keitora.toml).
 * Returns the migrated ProjectSummary, or null if no legacy config was found.
 */
export function migrateLegacy(): Promise<ProjectSummary | null> {
  return invoke<ProjectSummary | null>("migrate_legacy");
}

/** Fires when the active project changes (config watcher or project switcher). */
export function onProjectChanged(cb: () => void): Promise<UnlistenFn> {
  return listen("project://changed", () => cb());
}

export interface WorktreeCandidate {
  name: string;
  path: string;
  branch: string | null;
  already_tracked: boolean;
}

/**
 * Discover git worktrees under the given project root that are not yet
 * tracked in keitora.toml.
 */
export function discoverWorktrees(projectRoot: string): Promise<WorktreeCandidate[]> {
  // Tauri v2 maps camelCase JS arg keys to the snake_case Rust param `project_root`.
  return invoke<WorktreeCandidate[]>("discover_worktrees", { projectRoot });
}

export interface WorktreeEntry {
  name: string;
  path: string;
}

/**
 * Write a new worktree list to the active project's keitora.toml.
 * Emits `project://changed` and `worktrees://changed` after saving.
 */
export function setWorktrees(entries: WorktreeEntry[]): Promise<void> {
  return invoke("set_worktrees", { entries });
}

/**
 * Create a new git worktree with the given name and register it in keitora.toml.
 * Emits `worktrees://changed` and `project://changed` after creation.
 * Returns the resolved worktree info (path + status).
 */
export function createWorktree(name: string): Promise<WorktreeInfo> {
  return invoke<WorktreeInfo>("worktree_create", { name });
}

/**
 * Confirm-preview payload for a Claude→Codex transfer.
 * The rail shows this before the irreversible import and only proceeds
 * on explicit user confirm. `first_user_line` is truncated for display.
 */
export interface ConfirmPreview {
  path: string; // full JSONL path (post-normalization, containment-checked)
  filename: string; // basename only, i.e. <uuid>.jsonl
  mtime: string; // RFC 3339 last-modified time (the "newest" key)
  first_user_line: string; // first real (non-meta) user line, truncated
}

/**
 * Phase A: resolve the worktree's live Claude session and return a preview.
 * Lock-free and side-effect-free (no import, no PTY teardown).
 */
export function previewClaudeTransfer(worktree: string): Promise<ConfirmPreview> {
  return invoke<ConfirmPreview>("preview_claude_transfer", { worktree });
}

/**
 * Phase B: transfer the worktree to `targetCli` — a live Claude→Codex import
 * (`codex`) or a Codex→Claude summary handoff (`claude`). Persists the new CLI /
 * thread state on success; the caller then drives the PTY restart via
 * `restartNonces` (contract C2). Resolves once persistence is committed.
 */
export function transferWorktree(worktree: string, targetCli: CliKind): Promise<void> {
  return invoke("transfer_worktree", { worktree, targetCli });
}

/**
 * Best-effort Codex → Claude handoff-summary injection.
 * Called once the freshly-restarted worktree's session reports `idle`. Resolves
 * `true` when a pending handoff summary was found and injected, `false` when
 * there was nothing to inject (a Claude→Codex transfer, or it was already
 * injected) — NOT an error either way.
 */
export function injectHandoffSummary(worktree: string): Promise<boolean> {
  return invoke<boolean>("inject_handoff_summary", { worktree });
}

export interface CliStatus {
  // "on_path" | "installable" | "missing" | "installed"
  state: "on_path" | "installable" | "missing" | "installed";
  path: string | null; // resolved path, located source, or created link
  note: string | null; // e.g. "~/.local/bin is not on your PATH"
}

/** Detect whether the `keitora` CLI is on PATH and, if not, whether it can be linked. */
export function keitoraCliStatus(): Promise<CliStatus> {
  return invoke<CliStatus>("keitora_cli_status");
}

/** Symlink a located `keitora` binary onto PATH (~/.local/bin). Idempotent. */
export function installKeitoraCli(): Promise<CliStatus> {
  return invoke<CliStatus>("install_keitora_cli");
}
