// Left-anchored, full-height settings drawer. Edits the active project's
// keitora.toml (Project section) and the user keitora.toml + registry (User section).
// Re-seeds its form from a fresh getSettings() each time it opens (mirrors
// CreateWorktreeDialog). Saves are surgical on the backend; the app refreshes
// via the project://changed event the save commands emit.

import * as Dialog from "@radix-ui/react-dialog";
import { AlertTriangle, Loader2, X } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";
import type { CliKind, DiffBase, ProjectSummary } from "../lib/ipc";
import { daemonRestart, daemonShutdown } from "../lib/ipc";
import { LAYER_MODAL, MODAL_OVERLAY_CLS } from "../lib/layers";
import { RESTART_FALLBACK_MS, restartOutcome } from "../lib/recovery";
import { useDaemonStatus, useRecovery } from "../lib/sessions";
import {
  commitSettingsToProject,
  getSettings,
  removeProject,
  type SettingsSnapshot,
  saveProjectSettings,
  saveUserSettings,
  setSkipPermissions,
} from "../lib/settings";
import { THEME_MODES, useTheme } from "../lib/theme";
import { RestartDaemonDialog } from "./RestartDaemonDialog";
import { useToast } from "./Toast";
import { WorktreePicker } from "./WorktreePicker";

const field =
  "w-full bg-bg-3 border border-line rounded-chip px-3 py-2 text-tx-1 text-[13px] outline-none focus:border-accent/60";
const fieldRo = "w-full bg-bg-2 border border-line/60 rounded-chip px-3 py-2 text-tx-3 text-[13px]";
const label = "text-[10.5px] uppercase tracking-wide text-tx-3";
const sectionTitle = "text-[12px] font-semibold uppercase tracking-wide text-tx-2";
const btnPrimary =
  "rounded-chip border border-accent bg-accent px-3.5 py-1.5 text-[11.5px] font-semibold text-[var(--color-on-accent)] disabled:opacity-60";
const btnGhost =
  "rounded-chip border border-line bg-bg-3 px-3 py-1.5 text-[11.5px] text-tx-1 disabled:opacity-50";

/** Build the project-default `DiffBase` from the drawer's kind select + ref/parent text. Always
 *  returns an explicit base (never null): `auto`, or a `ref`/`forkPoint` with empty text, resolve
 *  to an explicit `{kind:"auto"}` so saving can override a committed non-Auto `[project].diff_base`
 *  rather than merely clearing the (empty) registry override. Exported for unit testing. */
export function buildDiffBase(kind: DiffBase["kind"], text: string): DiffBase {
  const t = text.trim();
  switch (kind) {
    case "upstream":
      return { kind: "upstream" };
    case "ref":
      return t ? { kind: "ref", name: t } : { kind: "auto" };
    case "forkPoint":
      return t ? { kind: "forkPoint", parent: t } : { kind: "auto" };
    default:
      return { kind: "auto" };
  }
}

// ── DaemonSection ─────────────────────────────────────────────────────────────

/**
 * Daemon section in the Settings drawer: the daemon ACTIONS (reveal log, restart,
 * stop). The live status read-out (state / PID / uptime / version / sessions) now
 * lives in the rail daemon card; this panel keeps only the controls. Uses
 * `useDaemonStatus()` internally so the action enable/disable stays live while the
 * drawer is open.
 */
function DaemonSection() {
  const info = useDaemonStatus();
  const recovery = useRecovery();
  const { showToast } = useToast();
  const [confirmStop, setConfirmStop] = useState(false);
  // confirmRestart: whether the restart confirm dialog is open.
  const [confirmRestart, setConfirmRestart] = useState(false);
  // restarting: true while the restart is in flight (from daemonRestart() call
  // until recovery reports an outcome, or until the fallback timer fires).
  const [restarting, setRestarting] = useState(false);
  // The daemon pid and recovery seq captured when we initiate a restart. Success
  // is recovery back to Healthy with a DIFFERENT pid (a new server); failure is a
  // Down/Paused snapshot newer than the call (see `restartOutcome`).
  const startPidRef = useRef<number | null>(null);
  const startSeqRef = useRef(0);
  const startedAtRef = useRef(0);
  // Timer ref for the restart fallback. Cleaned up on unmount and whenever the
  // restart concludes (success or failure).
  const restartTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Guard against setState after unmount (mirrors QuitConfirmModal pattern).
  const mountedRef = useRef(true);

  useEffect(() => {
    return () => {
      mountedRef.current = false;
      if (restartTimerRef.current) clearTimeout(restartTimerRef.current);
    };
  }, []);

  const state = info.state;
  const isConnected = state === "Connected";
  // Restart also recovers an unavailable herdr; only the pre-connect state has nothing to restart.
  const canRestart = state !== "NeverStarted";

  const revealLog = async () => {
    if (!info.log_path) return;
    try {
      const { openPath } = await import("@tauri-apps/plugin-opener");
      await openPath(info.log_path);
    } catch (e) {
      showToast(`Could not reveal log: ${String(e)}`);
    }
  };

  // Called when the user confirms in the restart dialog.
  const handleRestart = async () => {
    setConfirmRestart(false);
    // Remember which daemon instance we're replacing; success = a different pid.
    startPidRef.current = info.pid;
    startSeqRef.current = recovery.seq;
    startedAtRef.current = Date.now();
    setRestarting(true);
    // Arm the fallback: a restart that reports nothing for this long is slow.
    if (restartTimerRef.current) clearTimeout(restartTimerRef.current);
    restartTimerRef.current = setTimeout(() => {
      if (!mountedRef.current) return;
      setRestarting(false);
      showToast("Daemon restart is taking longer than expected");
    }, RESTART_FALLBACK_MS);
    try {
      await daemonRestart();
    } catch (e) {
      if (!mountedRef.current) return;
      if (restartTimerRef.current) clearTimeout(restartTimerRef.current);
      setRestarting(false);
      showToast(`Could not restart daemon: ${String(e)}`);
    }
  };

  // Completion detection, driven by recovery state: success is recovery back to
  // Healthy with a new pid (keying off the pid survives a missed/coalesced "down"
  // emit); failure is recovery landing in Down/Paused after the call.
  // biome-ignore lint/correctness/useExhaustiveDependencies: re-evaluate only when the pid/recovery/restarting change; showToast is stable
  useEffect(() => {
    if (!restarting) return;
    const outcome = restartOutcome({
      startPid: startPidRef.current,
      startSeq: startSeqRef.current,
      pid: info.pid,
      recovery,
      elapsedMs: Date.now() - startedAtRef.current,
    });
    if (outcome.kind === "pending" || outcome.kind === "slow") return;
    if (restartTimerRef.current) clearTimeout(restartTimerRef.current);
    setRestarting(false);
    showToast(
      outcome.kind === "success"
        ? "Daemon restarted"
        : `Daemon restart failed: ${outcome.reason ?? "see log"}`,
    );
  }, [info.pid, recovery.phase, recovery.seq, restarting]);

  const handleStop = async () => {
    try {
      await daemonShutdown();
      setConfirmStop(false);
    } catch (e) {
      showToast(`Could not stop daemon: ${String(e)}`);
      setConfirmStop(false);
    }
  };

  return (
    <section className="flex flex-col gap-3 border-t border-line pt-6">
      <div className={sectionTitle}>Daemon</div>
      <p className="text-[11px] text-tx-3">
        Live status, CPU, memory, and uptime are shown in the side rail.
      </p>
      {info.reason && <p className="text-[11px] text-busy">{info.reason}</p>}

      <div className="flex flex-wrap gap-2">
        {info.log_path && (
          <button
            className={btnGhost}
            onClick={() => void revealLog()}
            title="Reveal log in Finder"
          >
            Reveal log in Finder
          </button>
        )}
        {/* Restart daemon button: opens the confirm dialog (or shows in-flight state). */}
        <button
          className={btnGhost}
          disabled={!canRestart || restarting}
          onClick={() => {
            if (!restarting) setConfirmRestart(true);
          }}
          title={canRestart ? "Restart the daemon" : "Daemon not connected"}
        >
          {restarting ? (
            <span className="flex items-center gap-1.5">
              <Loader2 size={12} className="animate-spin" />
              Restarting…
            </span>
          ) : (
            "Restart daemon"
          )}
        </button>
        {confirmStop ? (
          <div className="flex flex-none gap-1.5">
            <button className={btnGhost} onClick={() => void handleStop()}>
              Confirm
            </button>
            <button className={btnGhost} onClick={() => setConfirmStop(false)}>
              Cancel
            </button>
          </div>
        ) : (
          <button
            className={btnGhost}
            disabled={!isConnected}
            onClick={() => setConfirmStop(true)}
            title={
              isConnected
                ? `Stop daemon — ${info.session_count} session(s) will be lost`
                : "Daemon not connected"
            }
          >
            Stop daemon
          </button>
        )}
      </div>
      {confirmStop && (
        <p className="text-[11px] text-urgent">
          Confirm stop — {info.session_count} session(s) will be lost
        </p>
      )}

      {/* Restart confirm dialog (the rail's Restart asks the same question). */}
      <RestartDaemonDialog
        open={confirmRestart}
        onOpenChange={setConfirmRestart}
        sessionCount={info.session_count}
        onConfirm={() => void handleRestart()}
      />
    </section>
  );
}

export function SettingsDrawer({
  open,
  onOpenChange,
  onSwitchProject,
  onAddProject: _onAddProject,
  dispatchEditor,
  codexDispatchEditor,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Shared optimistic project-switch path (App.tsx → switchProject). Settings
   * has only the name (no ProjectSummary), so it calls without the optimistic
   * summary; reconciliation still flows through project://changed. */
  onSwitchProject: (name: string, summary?: ProjectSummary) => Promise<void>;
  /** Opens the add-project dialog (single mount in App.tsx). */
  onAddProject?: () => void;
  /** Mounts the Claude dispatch-steps editor here. Receives the current steps
   * and an onChange to update them; until then a placeholder renders. */
  dispatchEditor?: (steps: string[], onChange: (steps: string[]) => void) => ReactNode;
  /** Mounts the Codex dispatch-steps editor — identical signature to
   * `dispatchEditor`, edits the Codex group's own step list. */
  codexDispatchEditor?: (steps: string[], onChange: (steps: string[]) => void) => ReactNode;
}) {
  const { showToast } = useToast();
  const [snap, setSnap] = useState<SettingsSnapshot | null>(null);
  const [loadErr, setLoadErr] = useState<string | null>(null);
  // Theme preference — shared with the app shell (AppInner) via the localStorage store.
  const { mode: themeMode, setMode: setThemeMode } = useTheme();

  // Worktree picker overlay state.
  const [pickerOpen, setPickerOpen] = useState(false);

  // Project form state.
  const [worktreesDir, setWorktreesDir] = useState("");
  const [archiveDays, setArchiveDays] = useState("30");
  const [logRetentionDays, setLogRetentionDays] = useState("90");
  const [projClaude, setProjClaude] = useState("");
  const [projCodex, setProjCodex] = useState("");
  const [dispatchSteps, setDispatchSteps] = useState<string[]>([]);
  const [codexDispatchSteps, setCodexDispatchSteps] = useState<string[]>([]);
  // Project default diff base: a kind select + the ref/parent text for ref/forkPoint.
  const [diffBaseKind, setDiffBaseKind] = useState<DiffBase["kind"]>("auto");
  const [diffBaseText, setDiffBaseText] = useState("");
  const [savingProject, setSavingProject] = useState(false);
  // Commit-to-file affordance: in-flight flag while commitSettingsToProject() runs.
  const [committingFile, setCommittingFile] = useState(false);

  // User form state.
  const [userClaude, setUserClaude] = useState("");
  const [userCodex, setUserCodex] = useState("");
  const [defaultCli, setDefaultCli] = useState<CliKind>("claude");
  const [savingUser, setSavingUser] = useState(false);
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null);

  // Per-CLI launch autonomy toggles. `skipRestart`
  // is local-only: it surfaces a non-destructive restart prompt after a toggle
  // and is intentionally NOT seeded from the snapshot so a settings reload can't
  // dismiss it unexpectedly. One shared restart prompt covers either toggle since
  // both are cleared by the same daemon restart.
  const [skipPerms, setSkipPerms] = useState(false);
  const [codexAutonomy, setCodexAutonomy] = useState(false);
  const [skipRestart, setSkipRestart] = useState(false);

  const seedProject = (s: SettingsSnapshot) => {
    setWorktreesDir(s.project.worktrees_dir ?? "");
    setArchiveDays(String(s.project.archive_days));
    setLogRetentionDays(String(s.project.log_retention_days));
    setProjClaude(s.project.claude_path ?? "");
    setProjCodex(s.project.codex_path ?? "");
    setDispatchSteps(s.project.dispatch_steps);
    setCodexDispatchSteps(s.project.codex_dispatch_steps);
    const db = s.project.diff_base;
    setDiffBaseKind(db?.kind ?? "auto");
    setDiffBaseText(db?.kind === "ref" ? db.name : db?.kind === "forkPoint" ? db.parent : "");
  };
  const seedUser = (s: SettingsSnapshot) => {
    setUserClaude(s.user.claude_path ?? "");
    setUserCodex(s.user.codex_path ?? "");
    setDefaultCli(s.user.default_cli);
    setConfirmRemove(null);
    // Seed the autonomy toggles; clear any stale restart prompt on (re)open.
    setSkipPerms(s.user.skip_permissions);
    setCodexAutonomy(s.user.codex_autonomy);
    setSkipRestart(false);
  };

  // Refresh from the backend, re-seeding only the requested form section(s).
  // `snap` (read-only displays + the registry list) always updates; the form
  // fields of an un-requested section are left as-is so a save in one section
  // never discards unsaved edits in the other.
  const refresh = async (which: { project?: boolean; user?: boolean }) => {
    try {
      const s = await getSettings();
      setLoadErr(null);
      setSnap(s);
      if (which.project) seedProject(s);
      if (which.user) seedUser(s);
    } catch (e) {
      setLoadErr(String(e));
    }
  };

  // Re-seed both sections each time the drawer opens.
  // biome-ignore lint/correctness/useExhaustiveDependencies: re-seed only when drawer opens; stable refresh excluded from deps
  useEffect(() => {
    if (!open) return;
    void refresh({ project: true, user: true });
  }, [open]);

  const nz = (s: string): string | null => {
    const t = s.trim();
    return t === "" ? null : t;
  };

  // Strict positive-integer parse: rejects "", floats ("1.5"→NaN check), and
  // non-numerics — parseInt would silently truncate "1.5" to 1.
  const posInt = (s: string): number | null => {
    const n = Number(s.trim());
    return Number.isInteger(n) && n >= 1 ? n : null;
  };

  const saveProject = async () => {
    if (savingProject) return;
    const a = posInt(archiveDays);
    const l = posInt(logRetentionDays);
    if (a === null || l === null) {
      showToast("Archive days and log retention must be whole numbers ≥ 1.");
      return;
    }
    setSavingProject(true);
    try {
      await saveProjectSettings({
        archiveDays: a,
        logRetentionDays: l,
        worktreesDir: nz(worktreesDir),
        claudePath: nz(projClaude),
        codexPath: nz(projCodex),
        dispatchSteps: dispatchSteps.map((s) => s.trim()).filter((s) => s !== ""),
        codexDispatchSteps: codexDispatchSteps.map((s) => s.trim()).filter((s) => s !== ""),
        diffBase: buildDiffBase(diffBaseKind, diffBaseText),
      });
      showToast("Project settings saved.");
      await refresh({ project: true });
    } catch (e) {
      showToast(String(e));
    } finally {
      setSavingProject(false);
    }
  };

  const saveUser = async () => {
    if (savingUser) return;
    setSavingUser(true);
    try {
      await saveUserSettings({
        claudePath: nz(userClaude),
        codexPath: nz(userCodex),
        defaultCli,
      });
      showToast("User settings saved.");
      await refresh({ user: true });
    } catch (e) {
      showToast(String(e));
    } finally {
      setSavingUser(false);
    }
  };

  // Commit the active project's current settings to a keitora.toml in the project root.
  // Mirrors the save flow: in-flight flag + toast + refresh project snapshot.
  const commitToFile = async () => {
    if (committingFile) return;
    setCommittingFile(true);
    try {
      await commitSettingsToProject();
      showToast("Settings committed to keitora.toml.");
      await refresh({ project: true });
    } catch (e) {
      showToast(String(e));
    } finally {
      setCommittingFile(false);
    }
  };

  const switchActive = async (name: string) => {
    // Ignore the empty-placeholder value and no-op re-selection of the active.
    if (!snap || !name || name === snap.user.active) return;
    // No ProjectSummary available in Settings (we only have the name), so this is
    // the degraded path: no optimistic flip. switchProject still drives App-level
    // project state via the config-file-watcher reconcile, and toasts on success/
    // error. Reseed the drawer's own snapshot afterward so its form stays in sync.
    await onSwitchProject(name);
    await refresh({ project: true, user: true });
  };

  const doRemove = async (name: string) => {
    try {
      await removeProject(name);
      showToast(`Removed ${name} from the registry.`);
      await refresh({ project: true, user: true });
    } catch (e) {
      showToast(String(e));
    }
  };

  // Generalized autonomy toggle: `cli` selects which provider's flag is
  // flipped, backed by the leading-discriminator `setSkipPermissions(cli, enabled)`.
  const toggleAutonomy = async (cli: CliKind) => {
    const current = cli === "claude" ? skipPerms : codexAutonomy;
    const seeded =
      cli === "claude"
        ? (snap?.user.skip_permissions ?? false)
        : (snap?.user.codex_autonomy ?? false);
    const next = !current;
    try {
      await setSkipPermissions(cli, next);
      if (cli === "claude") setSkipPerms(next);
      else setCodexAutonomy(next);
      // New sessions pick this up automatically; prompt to restart so running
      // sessions respawn with the new args — but only when the persisted value
      // actually changed (toggling back to the seeded value needs no restart).
      setSkipRestart(next !== seeded);
    } catch (e) {
      showToast(String(e));
    }
  };

  const restartForSkip = async () => {
    try {
      await daemonRestart();
      // Clear the prompt only after a successful restart so the in-context retry
      // stays available if it fails.
      setSkipRestart(false);
      showToast("Restarting herdr — sessions will resume");
    } catch (e) {
      showToast(`Could not restart daemon: ${String(e)}`);
    }
  };

  const dirChanged = snap != null && nz(worktreesDir) !== (snap.project.worktrees_dir ?? null);

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className={MODAL_OVERLAY_CLS} />
        <Dialog.Content
          className={`fixed left-0 top-0 bottom-0 ${LAYER_MODAL} flex w-[420px] max-w-[92vw] flex-col border-r border-line bg-bg-1 shadow-[8px_0_30px_rgba(0,0,0,.4)]`}
          aria-describedby={undefined}
        >
          {/* Header */}
          <div className="flex items-center justify-between border-b border-line px-5 py-3.5">
            <Dialog.Title className="text-[15px] font-semibold text-tx-1">Settings</Dialog.Title>
            <Dialog.Close asChild>
              <button className="text-tx-3 hover:text-tx-1" aria-label="Close settings">
                <X size={16} />
              </button>
            </Dialog.Close>
          </div>

          {/* Worktree picker overlay — rendered on top of the drawer when open */}
          {pickerOpen && snap && (
            <div className="absolute inset-0 z-10 flex items-center justify-center bg-black/60 p-6">
              <WorktreePicker
                variant="modal"
                projectRoot={snap.project.root}
                onDone={() => {
                  setPickerOpen(false);
                  // The picker persisted a new selection — re-read the snapshot so
                  // the drawer's read-only "Worktrees" field reflects it (the
                  // rail/board refresh on their own via `worktrees://changed`).
                  void refresh({ project: true });
                }}
                onSkip={() => setPickerOpen(false)}
              />
            </div>
          )}

          {/* Body */}
          <div className="flex-1 overflow-y-auto px-5 py-4">
            {loadErr && <p className="text-[11px] text-urgent">{loadErr}</p>}
            {!snap && !loadErr && <p className="text-[12px] text-tx-3">Loading…</p>}

            {snap && (
              <div className="flex flex-col gap-7">
                {/* ── Project ─────────────────────────────────────────────── */}
                <section className="flex flex-col gap-3">
                  <div className={sectionTitle}>Project · {snap.project.name}</div>

                  <label className="flex flex-col gap-1.5">
                    <span className={label}>Name (read-only)</span>
                    <input className={fieldRo} value={snap.project.name} readOnly tabIndex={-1} />
                  </label>

                  <label className="flex flex-col gap-1.5">
                    <span className={label}>Task DB path (read-only)</span>
                    <input
                      className={fieldRo}
                      value={snap.project.db_path}
                      readOnly
                      tabIndex={-1}
                    />
                  </label>

                  <label className="flex flex-col gap-1.5">
                    <span className={label}>Worktrees (read-only — managed from the rail)</span>
                    <input
                      className={fieldRo}
                      value={snap.project.worktrees.join(", ") || "—"}
                      readOnly
                      tabIndex={-1}
                    />
                  </label>

                  <div className="flex flex-col gap-1.5">
                    <p className="text-[11px] text-tx-3">
                      Scan this project's git worktrees and choose which to track.
                    </p>
                    <button className={btnGhost} onClick={() => setPickerOpen(true)}>
                      Re-discover worktrees
                    </button>
                  </div>

                  <label className="flex flex-col gap-1.5">
                    <span className={label}>Worktrees directory</span>
                    <input
                      className={field}
                      value={worktreesDir}
                      onChange={(e) => setWorktreesDir(e.target.value)}
                      placeholder="~/.keitora/<project> (default)"
                    />
                    {dirChanged && (
                      <span className="flex items-start gap-1.5 text-[11px] text-busy">
                        <AlertTriangle size={13} className="mt-0.5 flex-none" />
                        Changing this re-points where Keitora resolves every tracked worktree — the
                        current worktrees will immediately appear disconnected (their files stay
                        under the old path).
                      </span>
                    )}
                  </label>

                  <div className="flex gap-3">
                    <label className="flex flex-1 flex-col gap-1.5">
                      <span className={label}>Archive days</span>
                      <input
                        className={field}
                        type="number"
                        min={1}
                        value={archiveDays}
                        onChange={(e) => setArchiveDays(e.target.value)}
                      />
                    </label>
                    <label className="flex flex-1 flex-col gap-1.5">
                      <span className={label}>Log retention days</span>
                      <input
                        className={field}
                        type="number"
                        min={1}
                        value={logRetentionDays}
                        onChange={(e) => setLogRetentionDays(e.target.value)}
                      />
                    </label>
                  </div>

                  <label className="flex flex-col gap-1.5">
                    <span className={label}>Diff base (default)</span>
                    <div className="flex gap-2">
                      <select
                        className={field}
                        value={diffBaseKind}
                        onChange={(e) => setDiffBaseKind(e.target.value as DiffBase["kind"])}
                      >
                        <option value="auto">Auto (origin/main → main)</option>
                        <option value="upstream">Upstream (@&#123;upstream&#125;)</option>
                        <option value="ref">Ref tip…</option>
                        <option value="forkPoint">Fork point of…</option>
                      </select>
                      {(diffBaseKind === "ref" || diffBaseKind === "forkPoint") && (
                        <input
                          className={field}
                          value={diffBaseText}
                          onChange={(e) => setDiffBaseText(e.target.value)}
                          placeholder={diffBaseKind === "ref" ? "origin/release-5.2" : "develop"}
                        />
                      )}
                    </div>
                    <span className="text-[11px] text-tx-3">
                      What the diff view compares against. A per-worktree override (set from the
                      diff header) wins over this.
                    </span>
                  </label>

                  {/* Claude/Codex path overrides + dispatch rituals now render inline in
                      the per-CLI provider groups below (Claude, Codex) — this button
                      still saves them, since they're the same form state. */}
                  <div className="flex justify-end">
                    <button
                      className={btnPrimary}
                      onClick={() => void saveProject()}
                      disabled={savingProject}
                    >
                      {savingProject ? "Saving…" : "Save project"}
                    </button>
                  </div>

                  {/* Commit-to-file affordance: surface when settings are stored only in
                      the user registry (no keitora.toml in the project root). Committing
                      writes a keitora.toml so settings are shareable with the repo. */}
                  <div className="flex flex-col gap-1.5 rounded-chip border border-line bg-bg-2 px-3 py-2.5">
                    {snap.project.has_committed_file ? (
                      <p className="text-[11px] text-tx-3">
                        Settings are stored in a <code>keitora.toml</code> in this project's root —
                        they can be committed to the repo.
                      </p>
                    ) : (
                      <p className="text-[11px] text-tx-3">
                        Settings for this project are stored in your user config. Commit a{" "}
                        <code>keitora.toml</code> to share them with the repo.
                      </p>
                    )}
                    <div className="flex">
                      <button
                        className={btnGhost}
                        onClick={() => void commitToFile()}
                        disabled={committingFile}
                      >
                        {committingFile
                          ? "Committing…"
                          : snap.project.has_committed_file
                            ? "Update project file"
                            : "Commit to project file"}
                      </button>
                    </div>
                  </div>
                </section>

                {/* ── Appearance ──────────────────────────────────────────── */}
                <section className="flex flex-col gap-3 border-t border-line pt-6">
                  <div className={sectionTitle}>Appearance</div>

                  <div className="flex items-center justify-between gap-3">
                    <span className={label}>Theme</span>
                    <div className="flex gap-1.5" role="group" aria-label="Theme">
                      {THEME_MODES.map((m) => (
                        <button
                          key={m}
                          className={themeMode === m ? btnPrimary : btnGhost}
                          aria-pressed={themeMode === m}
                          onClick={() => setThemeMode(m)}
                        >
                          {m === "light" ? "Light" : m === "dark" ? "Dark" : "System"}
                        </button>
                      ))}
                    </div>
                  </div>
                  <p className="text-[11px] text-tx-3">
                    System follows your OS light/dark setting.
                  </p>
                </section>

                {/* ── User ────────────────────────────────────────────────── */}
                <section className="flex flex-col gap-3 border-t border-line pt-6">
                  <div className={sectionTitle}>User</div>

                  <div className="flex items-center justify-between gap-3">
                    <span className={label}>Default CLI</span>
                    <div className="flex gap-1.5" role="group" aria-label="Default CLI">
                      <button
                        className={defaultCli === "claude" ? btnPrimary : btnGhost}
                        aria-pressed={defaultCli === "claude"}
                        onClick={() => setDefaultCli("claude")}
                      >
                        Claude
                      </button>
                      <button
                        className={defaultCli === "codex" ? btnPrimary : btnGhost}
                        aria-pressed={defaultCli === "codex"}
                        onClick={() => setDefaultCli("codex")}
                      >
                        Codex
                      </button>
                    </div>
                  </div>
                  <p className="text-[11px] text-tx-3">
                    Which CLI a worktree launches when it has no explicit CLI of its own.
                  </p>

                  {/* Claude/Codex user-default paths now render inline in the per-CLI
                      provider groups below — this button still saves them, since
                      they're the same form state (along with Default CLI above). */}
                  <div className="flex justify-end">
                    <button
                      className={btnPrimary}
                      onClick={() => void saveUser()}
                      disabled={savingUser}
                    >
                      {savingUser ? "Saving…" : "Save user"}
                    </button>
                  </div>

                  <label className="mt-2 flex flex-col gap-1.5">
                    <span className={label}>Active project</span>
                    <select
                      className={field}
                      value={
                        snap.user.projects.some((p) => p.name === snap.user.active)
                          ? // biome-ignore lint/style/noNonNullAssertion: active is non-null: guarded by .some() check in the value expression above
                            snap.user.active!
                          : ""
                      }
                      onChange={(e) => void switchActive(e.target.value)}
                    >
                      {/* Placeholder when there is no active project, or the
                          active one isn't in the registry list — keeps the
                          select's value matched to a real option so React can't
                          silently select (and switch to) the first project. */}
                      {!snap.user.projects.some((p) => p.name === snap.user.active) && (
                        <option value="" disabled>
                          (none)
                        </option>
                      )}
                      {snap.user.projects.map((p) => (
                        <option key={p.name} value={p.name}>
                          {p.name}
                        </option>
                      ))}
                    </select>
                  </label>

                  <div className="flex flex-col gap-1.5">
                    <span className={label}>Registered projects</span>
                    <div className="flex flex-col gap-1">
                      {snap.user.projects.map((p) => (
                        <div
                          key={p.name}
                          className="flex items-center gap-2 rounded-chip border border-line bg-bg-2 px-3 py-2"
                        >
                          <div className="min-w-0 flex-1">
                            <div className="flex items-center gap-2 text-[12.5px] text-tx-1">
                              {p.name}
                              {p.active && (
                                <span className="rounded-chip bg-accent-dim px-1.5 py-0.5 text-[9.5px] uppercase tracking-wide text-accent-text">
                                  active
                                </span>
                              )}
                            </div>
                            <div className="truncate text-[10.5px] text-tx-3" title={p.root}>
                              {p.root}
                            </div>
                          </div>
                          {confirmRemove === p.name ? (
                            <div className="flex flex-none gap-1.5">
                              <button className={btnGhost} onClick={() => void doRemove(p.name)}>
                                Confirm
                              </button>
                              <button className={btnGhost} onClick={() => setConfirmRemove(null)}>
                                Cancel
                              </button>
                            </div>
                          ) : (
                            <button
                              className={btnGhost}
                              onClick={() => setConfirmRemove(p.name)}
                              disabled={p.active}
                              title={
                                p.active
                                  ? "Switch to another project first"
                                  : "Remove from registry"
                              }
                            >
                              Remove
                            </button>
                          )}
                        </div>
                      ))}
                    </div>
                    {/* Add project — same opener as the rail; on completion the
                        drawer's getSettings() refresh (on project://changed) updates the list. */}
                    {_onAddProject && (
                      <button className={btnGhost} onClick={_onAddProject}>
                        Add project…
                      </button>
                    )}
                  </div>
                </section>

                {/* ── Claude ─────────────────────────────────────────────── */}
                <section className="flex flex-col gap-3 border-t border-line pt-6">
                  <div className={sectionTitle}>Claude</div>

                  <label className="flex flex-col gap-1.5">
                    <span className={label}>Claude path (user default)</span>
                    <input
                      className={field}
                      value={userClaude}
                      onChange={(e) => setUserClaude(e.target.value)}
                      placeholder="(auto-discovered when empty)"
                    />
                  </label>

                  <label className="flex flex-col gap-1.5">
                    <span className={label}>Claude path (project override)</span>
                    <input
                      className={snap.project.has_committed_file ? field : fieldRo}
                      value={projClaude}
                      onChange={(e) => setProjClaude(e.target.value)}
                      placeholder="(uses the user default when empty)"
                      disabled={!snap.project.has_committed_file}
                      readOnly={!snap.project.has_committed_file}
                      tabIndex={snap.project.has_committed_file ? undefined : -1}
                    />
                    {!snap.project.has_committed_file && (
                      <span className="text-[11px] text-tx-3">
                        A project-level Claude override requires a committed{" "}
                        <code>keitora.toml</code>. Commit a file below, or set the user-level
                        default above.
                      </span>
                    )}
                  </label>

                  <div className="flex flex-col gap-1.5">
                    <span className={label}>Dispatch ritual</span>
                    {dispatchEditor ? (
                      dispatchEditor(dispatchSteps, setDispatchSteps)
                    ) : (
                      <p className="text-[11px] text-tx-3">
                        {dispatchSteps.length === 0
                          ? "Using Keitora's built-in dispatch prompt."
                          : `${dispatchSteps.length} step(s) configured.`}
                      </p>
                    )}
                  </div>

                  <div className="flex items-center justify-between gap-3">
                    <span className={label}>Skip permission prompts</span>
                    <button
                      className={skipPerms ? btnPrimary : btnGhost}
                      onClick={() => void toggleAutonomy("claude")}
                    >
                      {skipPerms ? "Enabled" : "Disabled"}
                    </button>
                  </div>
                  {/* Icon + a single text child: keep the sentence (incl. <code>)
                      out of the flex layout so it flows as one paragraph. */}
                  <div className="flex items-start gap-1.5 text-[11px] text-tx-3">
                    <AlertTriangle size={13} className="mt-0.5 flex-none" />
                    <span>
                      Launches Claude with <code>--dangerously-skip-permissions</code>, bypassing
                      its per-action approval prompts. Only enable if you understand the risk.
                    </span>
                  </div>

                  {snap?.project.skip_permissions && (
                    <div className="flex items-start gap-1.5 text-[11px] text-busy">
                      <AlertTriangle size={13} className="mt-0.5 flex-none" />
                      <span>
                        This project's <code>keitora.toml</code> enables skip-permissions, so its
                        sessions launch with the flag regardless of the toggle above.
                      </span>
                    </div>
                  )}
                </section>

                {/* ── Codex ──────────────────────────────────────── */}
                <section className="flex flex-col gap-3 border-t border-line pt-6">
                  <div className={sectionTitle}>Codex</div>

                  {!snap.user.codex_available && (
                    <p className="text-[11px] text-tx-3">
                      Codex not found on PATH — install it, then reopen Settings.
                    </p>
                  )}

                  <label className="flex flex-col gap-1.5">
                    <span className={label}>Codex path (user default)</span>
                    <input
                      className={snap.user.codex_available ? field : fieldRo}
                      value={userCodex}
                      onChange={(e) => setUserCodex(e.target.value)}
                      placeholder="(auto-discovered when empty)"
                      disabled={!snap.user.codex_available}
                      readOnly={!snap.user.codex_available}
                      tabIndex={snap.user.codex_available ? undefined : -1}
                    />
                  </label>

                  <label className="flex flex-col gap-1.5">
                    <span className={label}>Codex path (project override)</span>
                    <input
                      className={
                        snap.user.codex_available && snap.project.has_committed_file
                          ? field
                          : fieldRo
                      }
                      value={projCodex}
                      onChange={(e) => setProjCodex(e.target.value)}
                      placeholder="(uses the user default when empty)"
                      disabled={!snap.user.codex_available || !snap.project.has_committed_file}
                      readOnly={!snap.user.codex_available || !snap.project.has_committed_file}
                      tabIndex={
                        snap.user.codex_available && snap.project.has_committed_file
                          ? undefined
                          : -1
                      }
                    />
                    {snap.user.codex_available && !snap.project.has_committed_file && (
                      <span className="text-[11px] text-tx-3">
                        A project-level Codex override requires a committed{" "}
                        <code>keitora.toml</code>. Commit a file below, or set the user-level
                        default above.
                      </span>
                    )}
                  </label>

                  <div className="flex flex-col gap-1.5">
                    <span className={label}>Dispatch ritual</span>
                    {codexDispatchEditor && snap.user.codex_available ? (
                      codexDispatchEditor(codexDispatchSteps, setCodexDispatchSteps)
                    ) : (
                      <p className="text-[11px] text-tx-3">
                        {codexDispatchSteps.length === 0
                          ? "Using Keitora's built-in dispatch prompt."
                          : `${codexDispatchSteps.length} step(s) configured.`}
                      </p>
                    )}
                  </div>

                  <div className="flex items-center justify-between gap-3">
                    <span className={label}>Bypass approvals &amp; sandbox (danger)</span>
                    <button
                      className={codexAutonomy ? btnPrimary : btnGhost}
                      onClick={() => void toggleAutonomy("codex")}
                      disabled={!snap.user.codex_available}
                    >
                      {codexAutonomy ? "Enabled" : "Disabled"}
                    </button>
                  </div>
                  <div className="flex items-start gap-1.5 text-[11px] text-tx-3">
                    <AlertTriangle size={13} className="mt-0.5 flex-none" />
                    <span>
                      Launches Codex with <code>--dangerously-bypass-approvals-and-sandbox</code>,
                      bypassing both its sandbox and its per-action approval prompts — broader than
                      Claude's flag. Only enable if you understand the risk.
                    </span>
                  </div>

                  {snap.project.codex_autonomy && (
                    <div className="flex items-start gap-1.5 text-[11px] text-busy">
                      <AlertTriangle size={13} className="mt-0.5 flex-none" />
                      <span>
                        This project's <code>keitora.toml</code> enables Codex autonomy, so its
                        sessions launch with the flag regardless of the toggle above.
                      </span>
                    </div>
                  )}
                </section>

                {/* Shared restart prompt: either autonomy toggle above can trigger it,
                    since both persist immediately and both need the same daemon
                    restart to reach already-running sessions. */}
                {skipRestart && (
                  <div className="flex flex-col gap-2 rounded-chip border border-line bg-bg-2 px-3 py-2.5">
                    <span className="flex items-start gap-1.5 text-[11px] text-busy">
                      <AlertTriangle size={13} className="mt-0.5 flex-none" />
                      New sessions use this setting automatically. To apply it to sessions that are
                      already running, restart the daemon — running agents are interrupted mid-turn
                      and resume with the new setting (their transcript is kept).
                    </span>
                    <div className="flex gap-2">
                      <button className={btnPrimary} onClick={() => void restartForSkip()}>
                        Restart daemon
                      </button>
                      <button className={btnGhost} onClick={() => setSkipRestart(false)}>
                        Later
                      </button>
                    </div>
                  </div>
                )}

                {/* ── Daemon ──────────────────────────────────────────────── */}
                <DaemonSection />
              </div>
            )}
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
