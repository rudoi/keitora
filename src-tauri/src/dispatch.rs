//! Manual board dispatch: a state-aware apply reserves the task always but
//! injects only into an idle worktree with a live PTY (`dispatch_apply`). The
//! human stays in the loop — Keitora only performs the mechanical injection, and
//! only into idle sessions.
//!
//! The IPC-facing command is thin; the testable core is pure functions:
//! `decide` (dispatch-vs-queue), `render_steps` (the ritual lines), and
//! `injection_outcome` (the write result → audit decision).

use std::time::Duration;

use serde::Serialize;
use tauri::State;

use keitora_tasks::dispatch::{interpolate, DispatchContext};
use keitora_tasks::{CliKind, DispatchConfig, ReserveOutcome};

use crate::backend::{
    background_set, map_write_line_result, PtyBackend, PtyBackendError, SessionKey, SharedBackend,
    WriteLineOutcome,
};

/// Serializes `dispatch_apply` so its idempotency check, reserve, PTY inject, and
/// `dispatched` log write run as one critical section. Without it, two concurrent
/// calls for the same task could both read the log before either writes its
/// `dispatched` row, both pass the guard, and both inject (the double-inject this
/// module exists to prevent). Dispatch is human-driven and rare, so a single
/// global lock held across the (bounded) inject is cheap.
///
/// Uses `tokio::sync::Mutex` because the gate is held across `.await` points after
/// migrating `dispatch_apply` to `async` (a `std::sync::Mutex` guard is `!Send`
/// across await boundaries).
#[derive(Default)]
pub struct DispatchGate(pub(crate) tokio::sync::Mutex<()>);

/// The result of `dispatch_apply`: whether the task was actually injected
/// (`dispatched`) or merely reserved (`queued`), plus a human-readable `reason`
/// when it was deferred.
#[derive(Debug, Clone, Serialize)]
pub struct DispatchOutcome {
    pub dispatched: bool,
    pub reason: Option<String>,
}

/// The dispatch-vs-defer decision (internal; never crosses IPC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Dispatched,
    Queued,
}

/// Pure: dispatch (inject now) iff the worktree is idle AND we hold a live PTY for
/// it; otherwise queue (reserve only). Encodes the safety rule "never write into a
/// busy PTY" — the only state we ever inject into is a live, idle session.
fn decide(state: &str, has_live_pty: bool) -> Decision {
    if state == "idle" && has_live_pty {
        Decision::Dispatched
    } else {
        Decision::Queued
    }
}

/// Pure: map the AUTHORITATIVE final `write_line` outcome (D6) of a dispatch's
/// injection sequence into the `(audit_decision, DispatchOutcome)` pair. Kept
/// separate so the never-Err-after-reserve mapping is unit-testable. `sent_any`
/// records whether at least one earlier step's text+submit reached the pane.
///
/// NB: this NEVER yields an `Err`; the task is already reserved, and a retry would
/// re-inject. A partial inject reports `dispatched` (the agent has input and may be
/// acting); a clean miss reports `queued` (safe to retry).
fn injection_outcome(
    final_outcome: WriteLineOutcome,
    sent_any: bool,
    worktree: &str,
    detail: &str,
) -> (&'static str, DispatchOutcome) {
    match final_outcome {
        WriteLineOutcome::Dispatched => (
            "dispatched",
            DispatchOutcome {
                dispatched: true,
                reason: None,
            },
        ),
        // Partial: earlier lines already reached the pane (the agent may be acting).
        // Report dispatched so the caller doesn't re-dispatch; record for diagnostics.
        WriteLineOutcome::Queued | WriteLineOutcome::Error if sent_any => (
            "dispatched",
            DispatchOutcome {
                dispatched: true,
                reason: Some(format!("{worktree}: partially injected ({detail})")),
            },
        ),
        // Nothing reached the pane → safe to leave reserved/queued for retry.
        WriteLineOutcome::Queued | WriteLineOutcome::Error => (
            "queued",
            DispatchOutcome {
                dispatched: false,
                reason: Some(format!(
                    "{worktree}: reserved — could not inject ({detail})"
                )),
            },
        ),
    }
}

/// Why a dispatch to `worktree` was only reserved: its session is busy, or the
/// user terminated it.
fn queued_reason(worktree: &str, terminated: bool) -> String {
    if terminated {
        format!("{worktree} is terminated — reserved")
    } else {
        format!("{worktree} busy — reserved")
    }
}

/// The detail recorded for an injection's final `write_line` result: empty on
/// success; for a missing session of a terminated worktree, how to recover.
fn injection_detail(result: &Result<(), PtyBackendError>, terminated: bool) -> String {
    match result {
        Ok(()) => String::new(),
        Err(PtyBackendError::NoSuchSession) if terminated => {
            "session is terminated; launch it to dispatch".to_string()
        }
        Err(e) => e.to_string(),
    }
}

/// Inject each ritual `step` as its own submitted line via the backend's atomic
/// `write_line` (text, pause `submit_delay_ms`, then `\r`). Returns the AUTHORITATIVE
/// final `write_line` result (D6) and whether at least one step's text+submit reached
/// the pane (`sent_any`). Stops at the first `Queued`/`Error` step — nothing more
/// should land. Extracted from `dispatch_apply` so the loop is testable against a
/// `FakeBackend` without a live daemon.
fn inject_steps(
    backend: &dyn PtyBackend,
    key: &SessionKey,
    steps: &[String],
    submit_delay_ms: u32,
    inter_step: Duration,
) -> (Result<(), PtyBackendError>, bool) {
    let mut result: Result<(), PtyBackendError> = Ok(());
    let mut sent_any = false;
    for (i, line) in steps.iter().enumerate() {
        result = backend.write_line(key, line, submit_delay_ms);
        match map_write_line_result(&result) {
            WriteLineOutcome::Dispatched => {
                sent_any = true; // text + submit reached the pane
            }
            // Queued / Error: stop — nothing more should land.
            WriteLineOutcome::Queued | WriteLineOutcome::Error => break,
        }
        if i + 1 < steps.len() {
            std::thread::sleep(inter_step);
        }
    }
    (result, sent_any)
}

/// Pause between a step's text and its submit byte, passed to the daemon's atomic
/// `WriteLine` RPC (which writes the text, sleeps this long, then writes the `\r`).
/// The pause matters — a contiguous `text\r` burst reads to the TUI as a paste
/// (embedded newline, no submit), so the Enter must arrive as a distinct, later
/// keystroke. Empirically tunable; raise it if the TUI still fails to submit.
const SUBMIT_DELAY: Duration = Duration::from_millis(60);

/// Delay between dispatch steps so the target TUI finishes processing one submitted
/// line before the next arrives. Empirically tunable.
const INTER_STEP_DELAY: Duration = Duration::from_millis(80);

/// Pure: map every control char (incl. ESC, BEL, newline, tab) to a space, THEN
/// collapse whitespace runs — so an interpolated title/description can't submit the
/// line early or smuggle an escape sequence. `split_whitespace` alone leaves
/// non-whitespace C0 controls like ESC intact, hence the explicit map first.
fn collapse(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pure: Keitora's built-in dispatch prompt, used when no custom `[dispatch].steps`
/// are configured. The submit byte is appended separately at injection time.
fn default_line(ctx: &DispatchContext) -> String {
    let id = ctx.task.id;
    format!(
        "Please pick up Keitora task #{id}. Run `./keitora task claim {id}` to claim it, then implement it: {title}. {desc}. When finished, run `./keitora task done {id}`.",
        title = ctx.task.title,
        desc = ctx.task.description,
    )
}

/// Pure: the ordered lines to inject for a dispatch, given the already-selected
/// per-CLI step list (see [`select_ritual_steps`]). This function itself carries
/// NO CLI-awareness — it just renders whichever step list its caller selected;
/// the per-CLI branch lives entirely in `select_ritual_steps`. Each step is
/// interpolated against `ctx`, then control-char/whitespace collapsed, and lines
/// that render empty are dropped (no blank submits). If nothing remains — no
/// steps configured, or every configured step rendered blank — falls back to the
/// built-in default line, so a dispatch ALWAYS injects at least one line (a
/// misconfigured all-blank list can't silently produce a no-op that still
/// reports success). The submit byte is appended per line at injection time, NOT
/// here.
fn render_steps(steps: &[String], ctx: &DispatchContext) -> Vec<String> {
    let rendered: Vec<String> = steps
        .iter()
        .map(|s| collapse(&interpolate(s, ctx)))
        .filter(|l| !l.is_empty())
        .collect();
    if rendered.is_empty() {
        vec![collapse(&default_line(ctx))]
    } else {
        rendered
    }
}

/// Pure: which `DispatchConfig` step list `render_steps` should render, chosen by
/// the TARGET WORKTREE's resolved [`CliKind`] — a Claude
/// worktree gets `claude_steps`, a Codex worktree gets `codex_steps`. This is the
/// ONE place the per-CLI branch lives; `render_steps` stays CLI-agnostic.
fn select_ritual_steps(cfg: &DispatchConfig, cli: CliKind) -> &[String] {
    match cli {
        CliKind::Claude => &cfg.claude_steps,
        CliKind::Codex => &cfg.codex_steps,
    }
}

/// State-aware apply: reserve the task for `worktree` always, then inject it iff
/// the worktree is idle with a live PTY (else leave it reserved/queued). The
/// backend is authoritative on the idle decision so we never write into a busy PTY.
#[tauri::command]
pub async fn dispatch_apply(
    worktree: String,
    task_id: i64,
    client: State<'_, SharedBackend>,
    gate: State<'_, DispatchGate>,
) -> Result<DispatchOutcome, String> {
    // Hold the dispatch gate for the whole command so the guard-check → reserve →
    // inject → log sequence is atomic vs. concurrent dispatch_apply calls.
    // `tokio::sync::Mutex::lock` is cancel-safe and does not poison, so the guard is
    // always acquired cleanly.
    let _gate = gate.0.lock().await;

    let (backend, config) = crate::tasks::open_backend()?;

    // Idempotency guard + reserve in one intent-level call. If this task is
    // already dispatched and still reserved to this worktree, `dispatch_reserve`
    // returns `AlreadyDispatched` and performs NO write, so a re-click / re-drag /
    // keyboard re-apply no-ops. Otherwise it commits the reserve and returns the
    // post-assign `Task`. Errors (blocked / terminal / assigned elsewhere) bubble up
    // from the reserve as before — but only BEFORE the write (never-Err-after-reserve).
    let task = match backend
        .dispatch_reserve(task_id, "keitora", &worktree)
        .await
        .map_err(|e| e.to_string())?
    {
        ReserveOutcome::AlreadyDispatched => {
            return Ok(DispatchOutcome {
                dispatched: true,
                reason: Some("already dispatched".into()),
            });
        }
        ReserveOutcome::Reserved(task) => task,
    };

    // Live state via the shared reconciled view, so the idle rule matches
    // `sessions_list`. `decide` is a pre-filter on the idle rule;
    // the AUTHORITATIVE liveness check is the daemon's `write_line` result below
    // (D6). A reconciled non-`offline` session is one the daemon holds live, so we
    // feed `state != "offline"` for `has_live_pty` — but the daemon is the final
    // word on whether the write actually landed.
    // Reuse the `config` already resolved by `open_backend` for both the project
    // name and the LocalStore open inside `reconciled_sessions` (FIX 6).
    let project = config.project.name.clone();
    let live = client.live_set_snapshot();
    let bg = background_set(client.inner().as_ref()); // on-demand, exact; empty on RPC error
    let sessions = crate::tasks::reconciled_sessions(&live, &bg, &project, &config)?;
    let state = sessions
        .iter()
        .find(|s| s.worktree == worktree)
        .map(|s| s.state.clone())
        .unwrap_or_else(|| "offline".to_string());
    let has_live_pty = state != "offline";

    match decide(&state, has_live_pty) {
        Decision::Dispatched => {
            let ctx = DispatchContext {
                task: &task,
                worktree: &worktree,
            };
            // Resolve the target worktree's CLI so the injected ritual matches it:
            // worktree entry `cli` → user `default_cli` →
            // `CliKind::Claude`. Dispatching INTO a Codex worktree is safe here
            // specifically because `decide` above already gated this branch on
            // idle + live state, and stage 030 landed Codex's own liveness signal
            // (`.codex/hooks.json`) — before 030, a Codex worktree's reconciled
            // state was always Claude-hook-derived (never meaningfully idle+live),
            // so this injection path was effectively unreachable for Codex until
            // that liveness signal landed.
            let user = keitora_tasks::UserConfig::load().unwrap_or_default();
            let entry = config
                .worktree_entries()
                .into_iter()
                .find(|e| e.name == worktree);
            let resolved_cli = keitora_tasks::resolve_cli(entry.as_ref(), &user);
            let steps = render_steps(select_ritual_steps(&config.dispatch, resolved_cli), &ctx);
            let key = SessionKey {
                project: project.clone(),
                worktree: worktree.clone(),
            };
            // Inject each step as its own submitted line via a single atomic daemon
            // `WriteLine` RPC: the daemon writes the text, pauses `SUBMIT_DELAY` ms,
            // then writes the `\r`. Folding the old two-step client-side
            // text→sleep→`\r` split into one server-side RPC removes the window where
            // a concurrent keystroke could land between a step's text and its Enter.
            // The `write_line` RESULT is AUTHORITATIVE for liveness (D6):
            //   * Ok            → Dispatched (text + `\r` reached the PTY).
            //   * Queued        → NoSuchSession / NotConnected: leave reserved, retry
            //                     later. NOT an Err (a retry would re-inject).
            //   * Error (other) → a real write failure; still NEVER an Err after
            //                     reserve — the partial/queued reporting preserves the
            //                     never-Err-after-reserve invariant.
            // The inter-step delay still runs on this thread so the TUI finishes one
            // submitted line before the next arrives — bounded and small for the
            // handful of steps a ritual holds.
            let (result, sent_any) = inject_steps(
                client.inner().as_ref(),
                &key,
                &steps,
                SUBMIT_DELAY.as_millis() as u32,
                INTER_STEP_DELAY,
            );
            // The task is already reserved. NEVER return Err after reserving (a retry
            // would re-inject). Map the authoritative final outcome to a (never-Err)
            // result + audit decision via the pure `injection_outcome` helper.
            let terminated = client.terminated_keys(&project).contains(&worktree);
            let detail = injection_detail(&result, terminated);
            let (decision, outcome) =
                injection_outcome(map_write_line_result(&result), sent_any, &worktree, &detail);
            // Best-effort audit: a failed log must not flip us to Err (which would
            // prompt a re-inject of an already-dispatched task).
            let _ = backend
                .dispatch_log_add(task_id, &worktree, decision, &detail)
                .await;
            Ok(outcome)
        }
        Decision::Queued => {
            // Best-effort audit, matching the Dispatched branch: the task is already
            // reserved, so a failed log must NOT flip us to Err (a retried
            // dispatch_apply would re-reserve / re-inject). never-Err-after-reserve.
            let _ = backend
                .dispatch_log_add(task_id, &worktree, "queued", "")
                .await;
            let terminated = client.terminated_keys(&project).contains(&worktree);
            Ok(DispatchOutcome {
                dispatched: false,
                reason: Some(queued_reason(&worktree, terminated)),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_truth_table() {
        // Only idle + live PTY dispatches; everything else queues.
        assert_eq!(decide("idle", true), Decision::Dispatched);
        assert_eq!(decide("idle", false), Decision::Queued);
        assert_eq!(decide("busy", true), Decision::Queued);
        assert_eq!(decide("waiting", true), Decision::Queued);
        assert_eq!(decide("offline", true), Decision::Queued);
        assert_eq!(decide("offline", false), Decision::Queued);
    }

    /// Gate test: a session that is `idle` per the DB but present in the background
    /// set reconciles to `busy`, and `decide("busy", true)` returns `Queued` — so
    /// the dispatcher never injects into a session running background work.
    #[test]
    fn idle_with_background_task_gates_dispatch() {
        use crate::backend::{apply_reconcile, SessionKey};
        use keitora_tasks::WorktreeSession;
        use std::collections::HashSet;

        // Build a session that the DB reports as idle.
        let session = WorktreeSession {
            project: String::new(),
            worktree: "sayo".to_string(),
            state: "idle".to_string(),
            current_task_id: Some(5),
            pid: Some(9999),
            last_heartbeat: "2026-06-22T00:00:00Z".to_string(),
        };

        let key = SessionKey {
            project: "proj".to_string(),
            worktree: "sayo".to_string(),
        };

        // Session is live (present in the daemon live-set) …
        let mut live: HashSet<SessionKey> = HashSet::new();
        live.insert(key.clone());

        // … and the daemon reports it is running a background task.
        let mut bg: HashSet<SessionKey> = HashSet::new();
        bg.insert(key.clone());

        // Stage-003 override: idle + bg in live-set → busy.
        let reconciled = apply_reconcile(&[session], &live, &bg, "proj");
        assert_eq!(
            reconciled[0].state, "busy",
            "idle+bg session must reconcile to busy"
        );

        // Stage-004 gate: busy → Queued, so the dispatcher never injects.
        assert_eq!(
            decide("busy", true),
            Decision::Queued,
            "decide must queue a busy session even with a live PTY"
        );
    }

    fn task_fixture(id: i64, title: &str, desc: &str) -> keitora_tasks::Task {
        keitora_tasks::Task {
            id,
            title: title.to_string(),
            description: desc.to_string(),
            status: "active".to_string(),
            priority: "high".to_string(),
            assigned_to: Some("sol".to_string()),
            created_by: None,
            tags: vec![],
            branch: None,
            pr_url: None,
            created_at: "2026-06-21".to_string(),
            updated_at: "2026-06-21".to_string(),
        }
    }

    #[test]
    fn render_steps_empty_config_uses_default_line() {
        let task = task_fixture(42, "Fix\tthe   parser", "It crashes\non  empty\n\ninput");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let steps = render_steps(&DispatchConfig::default().claude_steps, &ctx);
        assert_eq!(
            steps.len(),
            1,
            "empty config yields exactly the default line"
        );
        let line = &steps[0];
        // Embedded newlines/tabs/multi-spaces collapse to single spaces.
        assert!(
            line.contains("Fix the parser"),
            "title not collapsed: {line}"
        );
        assert!(
            line.contains("It crashes on empty input"),
            "desc not collapsed: {line}"
        );
        // Built-in claim/done guidance preserved.
        assert!(line.contains("./keitora task claim 42"));
        assert!(line.contains("./keitora task done 42"));
        // The submit byte is NOT carried in-band any more — it is written separately.
        assert!(
            !line.ends_with('\r'),
            "submit byte must not be in the line: {line:?}"
        );
        assert!(
            !line.chars().any(|c| c.is_control()),
            "no control byte: {line:?}"
        );
    }

    #[test]
    fn render_steps_default_line_strips_control_bytes() {
        // ESC/ANSI in the title and a BEL in the desc must both be neutralised.
        let task = task_fixture(7, "a\u{1b}[31mb", "x\x07y");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let line = &render_steps(&DispatchConfig::default().claude_steps, &ctx)[0];
        assert!(
            line.contains("a [31mb") || line.contains("a[31mb"),
            "title: {line:?}"
        );
        assert!(line.contains("x y"), "desc: {line:?}");
        assert!(
            !line.chars().any(|c| c.is_control()),
            "no control byte may survive: {line:?}"
        );
        assert!(!line.contains('\u{1b}'), "no ESC may survive: {line:?}");
    }

    #[test]
    fn render_steps_interpolates_configured_steps_in_order() {
        let task = task_fixture(19, "custom dispatch", "wire it up");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec![
                "/clear".to_string(),
                "/park".to_string(),
                "/workflow pick up #{task_number}: {task_title}".to_string(),
            ],
            codex_steps: vec![],
        };
        let steps = render_steps(&cfg.claude_steps, &ctx);
        assert_eq!(
            steps,
            vec![
                "/clear".to_string(),
                "/park".to_string(),
                "/workflow pick up #0019: custom dispatch".to_string(),
            ]
        );
    }

    #[test]
    fn render_steps_drops_blank_steps() {
        let task = task_fixture(1, "t", "d");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec!["/clear".to_string(), "   ".to_string(), "/park".to_string()],
            codex_steps: vec![],
        };
        let steps = render_steps(&cfg.claude_steps, &ctx);
        assert_eq!(steps, vec!["/clear".to_string(), "/park".to_string()]);
    }

    #[test]
    fn render_steps_all_blank_config_falls_back_to_default() {
        // A misconfigured all-blank `steps` must NOT yield an empty injection (which
        // dispatch_apply would report as a successful no-op) — it falls back to the
        // built-in default line so a dispatch always sends something.
        let task = task_fixture(42, "the title", "the desc");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec!["   ".to_string(), "\t".to_string()],
            codex_steps: vec![],
        };
        let steps = render_steps(&cfg.claude_steps, &ctx);
        assert_eq!(steps.len(), 1, "must fall back to one default line");
        assert!(
            steps[0].contains("./keitora task claim 42"),
            "default line: {:?}",
            steps[0]
        );
    }

    #[test]
    fn render_steps_collapses_interpolated_control_chars() {
        // A title carrying a newline/ESC, injected via a custom step, must collapse so
        // it can't submit early or smuggle an escape sequence.
        let task = task_fixture(5, "line1\nline2\u{1b}[0m", "d");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec!["go {task_title}".to_string()],
            codex_steps: vec![],
        };
        let line = &render_steps(&cfg.claude_steps, &ctx)[0];
        assert!(
            !line.chars().any(|c| c.is_control()),
            "no control byte: {line:?}"
        );
        assert!(line.starts_with("go line1 line2"), "collapsed: {line:?}");
    }

    // -----------------------------------------------------------------------
    // select_ritual_steps
    // -----------------------------------------------------------------------

    #[test]
    fn select_ritual_steps_picks_claude_steps_for_claude_cli() {
        let cfg = DispatchConfig {
            claude_steps: vec!["/clear".to_string(), "/park".to_string()],
            codex_steps: vec!["/codex-step".to_string()],
        };
        assert_eq!(
            select_ritual_steps(&cfg, CliKind::Claude),
            &cfg.claude_steps[..]
        );
    }

    #[test]
    fn select_ritual_steps_picks_codex_steps_for_codex_cli() {
        let cfg = DispatchConfig {
            claude_steps: vec!["/clear".to_string()],
            codex_steps: vec!["/codex-step".to_string(), "/codex-step-2".to_string()],
        };
        assert_eq!(
            select_ritual_steps(&cfg, CliKind::Codex),
            &cfg.codex_steps[..]
        );
    }

    #[test]
    fn select_ritual_steps_empty_codex_list_falls_back_to_default_via_render_steps() {
        // `select_ritual_steps` itself has no fallback logic — it is a pure field
        // selector. The fallback lives in `render_steps`, exercised here through
        // the full pipeline for a Codex worktree with no configured `codex_steps`,
        // mirroring `render_steps_all_blank_config_falls_back_to_default` but for
        // the Codex list specifically.
        let task = task_fixture(42, "the title", "the desc");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec!["/clear".to_string()],
            codex_steps: vec![],
        };
        let steps = render_steps(select_ritual_steps(&cfg, CliKind::Codex), &ctx);
        assert_eq!(
            steps.len(),
            1,
            "empty codex_steps falls back to one default line"
        );
        assert!(
            steps[0].contains("./keitora task claim 42"),
            "default line: {:?}",
            steps[0]
        );
    }

    // -----------------------------------------------------------------------
    // inject_steps — the injection loop against a FakeBackend.
    // -----------------------------------------------------------------------

    fn inject_key() -> SessionKey {
        SessionKey {
            project: "proj".to_string(),
            worktree: "sayo".to_string(),
        }
    }

    fn write_line_calls(
        fake: &crate::backend::fake::FakeBackend,
    ) -> Vec<(SessionKey, String, u32)> {
        use crate::backend::fake::Call;
        fake.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::WriteLine {
                    key,
                    text,
                    submit_delay_ms,
                } => Some((key, text, submit_delay_ms)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn inject_steps_all_ok_reports_sent() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        fake.push_write_line(Ok(()));
        fake.push_write_line(Ok(()));
        let key = inject_key();
        let steps = vec!["/clear".to_string(), "/go".to_string()];
        let (result, sent_any) = inject_steps(&fake, &key, &steps, 60, Duration::ZERO);
        assert!(result.is_ok());
        assert!(sent_any);
        // Both steps attempted, with the right key + submit delay.
        let writes = write_line_calls(&fake);
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].0, key);
        assert_eq!(writes[0].1, "/clear");
        assert_eq!(writes[0].2, 60);
        assert_eq!(writes[1].1, "/go");
    }

    #[test]
    fn inject_steps_stops_on_no_such_session_but_sent_any() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        fake.push_write_line(Ok(()));
        fake.push_write_line(Err(PtyBackendError::NoSuchSession));
        let key = inject_key();
        let steps = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (result, sent_any) = inject_steps(&fake, &key, &steps, 60, Duration::ZERO);
        // The failing step's result is authoritative and maps to Queued.
        assert!(matches!(result, Err(PtyBackendError::NoSuchSession)));
        assert_eq!(map_write_line_result(&result), WriteLineOutcome::Queued);
        assert!(sent_any, "step 1 reached the pane");
        // Stops after the failing step — step 3 never runs.
        assert_eq!(write_line_calls(&fake).len(), 2);
    }

    #[test]
    fn inject_steps_first_not_connected_sends_nothing() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        fake.push_write_line(Err(PtyBackendError::NotConnected));
        let key = inject_key();
        let steps = vec!["a".to_string(), "b".to_string()];
        let (result, sent_any) = inject_steps(&fake, &key, &steps, 60, Duration::ZERO);
        assert!(matches!(result, Err(PtyBackendError::NotConnected)));
        assert_eq!(map_write_line_result(&result), WriteLineOutcome::Queued);
        assert!(!sent_any);
        // No step runs after the first failure.
        assert_eq!(write_line_calls(&fake).len(), 1);
    }

    #[test]
    fn a_terminated_worktree_is_named_in_the_queued_reason() {
        assert_eq!(queued_reason("sayo", false), "sayo busy — reserved");
        assert_eq!(queued_reason("sayo", true), "sayo is terminated — reserved");
    }

    #[test]
    fn a_missing_session_of_a_terminated_worktree_says_so() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        let key = inject_key();
        fake.insert_terminated(key.clone());
        fake.push_write_line(Err(PtyBackendError::NoSuchSession));
        let (result, sent_any) = inject_steps(&fake, &key, &["a".to_string()], 60, Duration::ZERO);
        let terminated = fake.terminated_keys(&key.project).contains(&key.worktree);
        let detail = injection_detail(&result, terminated);
        assert_eq!(detail, "session is terminated; launch it to dispatch");
        let (decision, outcome) =
            injection_outcome(map_write_line_result(&result), sent_any, "sayo", &detail);
        assert_eq!(decision, "queued");
        assert!(!outcome.dispatched);
        assert!(outcome.reason.unwrap().contains("terminated"));
        // Not terminated, or another error: the error's own text.
        let missing = Err(PtyBackendError::NoSuchSession);
        assert_eq!(injection_detail(&missing, false), "no live session");
        let down = Err(PtyBackendError::NotConnected);
        assert_eq!(injection_detail(&down, true), "herdr unreachable");
        assert_eq!(injection_detail(&Ok(()), true), "");
    }
}
