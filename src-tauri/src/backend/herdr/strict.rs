//! `strict` — the verified launch a spawn that follows a transfer to Codex uses.
//!
//! The default launch ([`Spawner::spawn_agent_locked`](super::Spawner)) trusts
//! `agent.start`: once it returns the session counts as running, and an agent
//! that exits early falls back resume → continue → fresh. A transfer cannot
//! afford either: `continue` (`resume --last`) may land in an unrelated thread,
//! and a pane whose agent never came up must not be reported as running.
//!
//! The strict launch walks a short chain instead — resume the imported thread,
//! then fresh, then fresh without the optional hook-trust flag — and confirms
//! each step by watching the pane (`pane get`) until herdr reports the agent
//! (and, for a resume, the expected session id). It never issues `continue`.
//! Every step and every change in what the pane reports goes to a per-key trail
//! the caller drains into the transfer diagnostics.
//!
//! The pure parts (chain, timing, observation state machine, error text) are
//! unit-tested here; the end-to-end behaviour is tested over the fake herdr in
//! the parent module's tests.

use std::time::{Duration, Instant};

use keitora_tasks::{is_safe_claude_session_id, CliKind};
use serde_json::{json, Value};

use crate::backend::{AgentLaunch, LaunchTrailEntry, PtyBackendError, SessionKey};
use crate::lifecycle::{self, LifecycleEvent, ResumeResult};
use crate::transfer_registry::TrailStage;

use super::cli::{AgentSessionKind, PaneIdentity, PaneInfo, PaneProcessInfo};
use super::ops::HerdrError;
use super::resume::ResumeDecision;
use super::{
    agent, env, session_event, start_bridge_in, Spawner, AGENT_START_TIMEOUT_MS,
    SHELL_READY_DELAY_MS,
};

/// Most trail entries kept per key; the oldest are dropped beyond it.
pub(super) const TRAIL_CAP: usize = 200;

/// The cli the strict launch applies to (and names in its final error).
const STRICT_CLI: CliKind = CliKind::Codex;

/// Longest `last_reason` (in chars) the final error quotes.
const REASON_CAP: usize = 300;

/// Longest pane tail (in bytes) a failed step records.
const PANE_TAIL_CAP: usize = 1024;

/// How long each part of a strict launch may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StrictTiming {
    /// Pause after `workspace.create` before `agent.start`.
    pub shell_ready: Duration,
    /// Interval between two `pane get` observations.
    pub poll: Duration,
    /// How long a step watches the pane after `agent.start` returned `Ok`.
    pub verify_window: Duration,
    /// Hard cap per step, including `agent.start` and a start-timeout extension.
    pub step_cap: Duration,
    /// Hard total budget: each step's cap is `min(step_cap, budget remaining)`,
    /// and no step starts with less than one poll of budget left.
    pub chain_budget: Duration,
    /// Pauses before the first and second re-issue of `agent.start`.
    pub reissue_backoffs: [Duration; 2],
}

impl StrictTiming {
    /// The production timing.
    pub(super) fn production() -> StrictTiming {
        StrictTiming {
            shell_ready: Duration::from_millis(SHELL_READY_DELAY_MS),
            poll: Duration::from_millis(250),
            verify_window: Duration::from_secs(10),
            step_cap: Duration::from_secs(45),
            chain_budget: Duration::from_secs(100),
            reissue_backoffs: [Duration::from_millis(300), Duration::from_millis(800)],
        }
    }

    /// Tiny values for unit tests (the fake herdr answers at once).
    pub(super) fn fast() -> StrictTiming {
        StrictTiming {
            shell_ready: Duration::ZERO,
            poll: Duration::from_millis(2),
            verify_window: Duration::from_millis(60),
            step_cap: Duration::from_millis(200),
            chain_budget: Duration::from_secs(1),
            reissue_backoffs: [Duration::from_millis(1), Duration::from_millis(2)],
        }
    }
}

/// The timing a strict launch runs with: [`StrictTiming::fast`] under test.
pub(super) fn strict_timing() -> StrictTiming {
    if cfg!(test) {
        StrictTiming::fast()
    } else {
        StrictTiming::production()
    }
}

/// Why a strict step did not start its agent.
#[derive(Debug)]
enum StepError {
    /// Another pane holds the agent's name (the chain may recover once).
    NameTaken(HerdrError),
    /// Any other failure, as the trail and final error quote it.
    Failed(String),
}

impl StepError {
    /// The failure reason the trail, log and final error carry. A name-taken
    /// error is reduced to its code: its message lists the holders' cwds.
    fn reason(self) -> String {
        match self {
            StepError::NameTaken(e) => e.code().unwrap_or("agent_name_taken").to_string(),
            StepError::Failed(reason) => reason,
        }
    }
}

/// One step of the strict chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StrictStep {
    pub decision: ResumeDecision,
    /// Whether the optional `--dangerously-bypass-hook-trust` flag is passed.
    pub hook_trust: bool,
    /// How the step reads in the trail and the final error.
    pub label: String,
}

/// The steps a strict launch tries, in order: `resume <thread>` (only when the
/// launch is not fresh and carries a safe thread id), `fresh`, then `fresh`
/// without the optional flag. Never contains [`ResumeDecision::Continue`]: an
/// unsafe id would make the args degrade to `resume --last`, so it is dropped.
pub(super) fn strict_chain(launch: &AgentLaunch) -> Vec<StrictStep> {
    let resume_id = if launch.fresh {
        None
    } else {
        launch
            .resume_target
            .as_deref()
            .filter(|id| is_safe_claude_session_id(id))
    };
    let mut steps = Vec::new();
    if let Some(id) = resume_id {
        steps.push(StrictStep {
            decision: ResumeDecision::Resume {
                cli: STRICT_CLI,
                id: id.to_string(),
            },
            hook_trust: true,
            label: format!("resume {id}"),
        });
    }
    steps.push(StrictStep {
        decision: ResumeDecision::Fresh,
        hook_trust: true,
        label: "fresh".into(),
    });
    steps.push(StrictStep {
        decision: ResumeDecision::Fresh,
        hook_trust: false,
        label: "fresh without optional flags".into(),
    });
    steps
}

/// What a step's verification waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Expect {
    /// codex reporting this thread id as its session.
    Resume(String),
    /// codex reported on two consecutive observations.
    Fresh,
}

impl Expect {
    fn of(decision: &ResumeDecision) -> Expect {
        match decision {
            ResumeDecision::Resume { id, .. } => Expect::Resume(id.clone()),
            // The chain never holds `Continue`; treat it like fresh if it did.
            ResumeDecision::Continue | ResumeDecision::Fresh => Expect::Fresh,
        }
    }
}

/// How a step's verification ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The pane reports what the step expected.
    Confirmed,
    /// The agent is up but herdr never confirmed the session: accepted.
    PresentUnknown,
    /// The pane reported another thread id for a resume.
    Mismatch(String),
    /// The pane itself went away.
    Gone,
    /// The agent was seen, then the pane fell back to a bare shell.
    Vanished,
    /// No agent was ever seen (before the process check, or after it found
    /// nothing but the shell).
    NeverAppeared,
}

impl Verdict {
    /// The step's failure reason; `None` when the verdict is a success.
    pub(super) fn failure(&self) -> Option<String> {
        match self {
            Verdict::Confirmed | Verdict::PresentUnknown => None,
            Verdict::Mismatch(v) => Some(format!("session mismatch: pane reports {v}")),
            Verdict::Gone => Some("pane closed during startup".into()),
            Verdict::Vanished => Some("agent vanished after start".into()),
            Verdict::NeverAppeared => Some("agent never appeared".into()),
        }
    }
}

/// What one observation showed, for "only trail when it changes".
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Pane {
        agent: Option<String>,
        status: Option<String>,
        session: Option<(String, &'static str, String)>,
    },
    Error(String),
    Gone,
}

impl Seen {
    fn detail(&self) -> Value {
        match self {
            Seen::Pane {
                agent,
                status,
                session,
            } => json!({
                "agent": agent,
                "status": status,
                "session": session
                    .as_ref()
                    .map(|(agent, kind, value)| json!({ "agent": agent, "kind": kind, "value": value })),
            }),
            Seen::Error(e) => json!({ "error": e }),
            Seen::Gone => json!({ "gone": true }),
        }
    }
}

/// The verification state carried across one step's observations.
#[derive(Debug, Default)]
pub(super) struct VerifyState {
    pub seen_agent: bool,
    pub consecutive_present: u32,
    pub last_mismatch: Option<String>,
    pub transient_errors: u32,
    last_logged: Option<Seen>,
}

/// The result of one observation: a verdict that ends the step early, and a
/// trail detail when what the pane reports changed.
#[derive(Debug, Default)]
pub(super) struct Classified {
    pub verdict: Option<Verdict>,
    pub trail: Option<Value>,
}

/// Fold one `pane get` answer into `state`.
pub(super) fn classify_observation(
    state: &mut VerifyState,
    expect: &Expect,
    obs: &Result<Option<PaneInfo>, HerdrError>,
) -> Classified {
    let (seen, verdict) = match obs {
        Err(e) if e.is_not_found() => (Seen::Gone, Some(Verdict::Gone)),
        Ok(None) => (Seen::Gone, Some(Verdict::Gone)),
        Err(e) => {
            state.transient_errors += 1;
            (Seen::Error(e.to_string()), None)
        }
        Ok(Some(p)) => {
            let seen = Seen::Pane {
                agent: p.agent.clone(),
                status: p.agent_status.clone(),
                session: p.agent_session.as_ref().map(|s| {
                    let kind = match s.kind {
                        AgentSessionKind::Id => "id",
                        AgentSessionKind::Path => "path",
                    };
                    (s.agent.clone(), kind, s.value.clone())
                }),
            };
            let verdict = match &p.agent {
                Some(agent) => {
                    state.seen_agent = true;
                    state.consecutive_present += 1;
                    let is_codex = agent == agent::herdr_kind(STRICT_CLI);
                    match expect {
                        Expect::Resume(thread) => match &p.agent_session {
                            Some(s)
                                if is_codex
                                    && s.agent == agent::herdr_kind(STRICT_CLI)
                                    && s.kind == AgentSessionKind::Id
                                    && &s.value == thread =>
                            {
                                Some(Verdict::Confirmed)
                            }
                            Some(s) if s.kind == AgentSessionKind::Id => {
                                state.last_mismatch = Some(s.value.clone());
                                None
                            }
                            _ => None,
                        },
                        Expect::Fresh => (is_codex && state.consecutive_present >= 2)
                            .then_some(Verdict::Confirmed),
                    }
                }
                None => {
                    state.consecutive_present = 0;
                    state.seen_agent.then_some(Verdict::Vanished)
                }
            };
            (seen, verdict)
        }
    };
    let trail = if state.last_logged.as_ref() != Some(&seen) {
        let detail = seen.detail();
        state.last_logged = Some(seen);
        Some(detail)
    } else {
        None
    };
    Classified { verdict, trail }
}

/// The verdict once a step's window ends without one: a resume that only ever
/// saw another thread is a mismatch; an agent that was seen is present
/// (unconfirmed, accepted); otherwise [`Verdict::NeverAppeared`], which the
/// caller settles with [`classify_never_appeared`].
pub(super) fn classify_window_end(state: &VerifyState, expect: &Expect) -> Verdict {
    match (&state.last_mismatch, expect) {
        (Some(value), Expect::Resume(_)) => Verdict::Mismatch(value.clone()),
        _ if state.seen_agent => Verdict::PresentUnknown,
        _ => Verdict::NeverAppeared,
    }
}

/// Settle "no agent was ever seen" from the pane's processes: a foreground
/// process other than the shell means the agent runs but herdr never labelled
/// the pane (accepted); no such process, or no answer, is a real failure.
pub(super) fn classify_never_appeared(info: &Result<PaneProcessInfo, HerdrError>) -> Verdict {
    match info {
        Ok(info) if !info.foreground_pids.is_empty() => Verdict::PresentUnknown,
        _ => Verdict::NeverAppeared,
    }
}

/// Lowercase the two exact tokens the pane reads as spawn-error kinds
/// (`Recovering`, `Terminated`), so free text can never be mistaken for them.
pub(super) fn sanitize_ui_tokens(text: &str) -> String {
    text.replace("Recovering", "recovering")
        .replace("Terminated", "terminated")
}

/// The error a strict launch returns once every step failed:
/// `codex did not start: <reason> (tried: <a> -> <b> ...)`. The reason is
/// whitespace-collapsed, capped and kept clear of the pane's error tokens.
pub(super) fn format_final_error(last_reason: &str, tried: &[String]) -> String {
    let collapsed = last_reason.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = collapsed.chars().take(REASON_CAP).collect();
    format!(
        "{} did not start: {} (tried: {})",
        agent::herdr_kind(STRICT_CLI),
        sanitize_ui_tokens(&capped),
        tried.join(" -> ")
    )
}

/// Whether an `agent.start` error is herdr's startup timeout (the agent may
/// still be loading): the CLI call ran out of time, herdr's `timeout` code, or
/// its timeout text (the same classification the resume path uses).
pub(super) fn is_agent_start_timeout(e: &HerdrError) -> bool {
    matches!(e, HerdrError::Timeout)
        || e.code() == Some("timeout")
        || super::is_timeout_text(&e.to_string())
}

/// The text of an early-exit error for the step's failure reason.
fn early_exit_reason(e: &HerdrError) -> String {
    match e {
        HerdrError::Api { code, message } => format!("agent exited early ({code}): {message}"),
        other => format!("agent exited early: {other}"),
    }
}

impl Spawner {
    /// Append one entry to `key`'s launch trail (oldest dropped past the cap).
    pub(super) fn push_trail(&self, key: &SessionKey, stage: TrailStage, detail: Value) {
        log::debug!("herdr: strict launch {key} [{stage:?}] {detail}");
        let mut trails = self.trails.lock().unwrap();
        let entries = trails.entry(key.clone()).or_default();
        if entries.len() >= TRAIL_CAP {
            entries.pop_front();
        }
        entries.push_back(LaunchTrailEntry { stage, detail });
    }

    /// Launch `key`'s Codex agent through the strict chain under the key's
    /// lock. Returns the session generation, the outcome, and whether the
    /// launch decision got weaker than the first step (resume → fresh).
    pub(super) fn spawn_agent_transfer_strict(
        &self,
        key: &SessionKey,
        cwd: String,
        cols: u16,
        rows: u16,
        launch: &AgentLaunch,
    ) -> Result<(u64, ResumeResult, bool), PtyBackendError> {
        self.spawn_agent_transfer_strict_with(
            key,
            cwd,
            cols,
            rows,
            launch,
            &strict_timing(),
            std::env::var("CODEX_HOME").ok(),
        )
    }

    /// [`Self::spawn_agent_transfer_strict`] with the timing and the
    /// `CODEX_HOME` value injected.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn_agent_transfer_strict_with(
        &self,
        key: &SessionKey,
        cwd: String,
        cols: u16,
        rows: u16,
        launch: &AgentLaunch,
        timing: &StrictTiming,
        codex_home: Option<String>,
    ) -> Result<(u64, ResumeResult, bool), PtyBackendError> {
        let lock = self.lock_for(key);
        let _held = lock.lock().unwrap();
        self.trails.lock().unwrap().remove(key);

        let chain = strict_chain(launch);
        let first = chain[0].decision.clone();
        if !launch.fresh && first == ResumeDecision::Fresh {
            log::warn!(
                "herdr: strict launch of {key} has no usable thread id ({:?}); starting fresh",
                launch.resume_target
            );
        }
        let kind = agent::herdr_kind(STRICT_CLI);
        let name = agent::agent_name(kind, key);
        let mut extra = crate::codex_env::strict_pane_extra_env(codex_home);
        extra.push(env::agent_marker());
        let pane_env = env::pane_env_with(
            &launch.path,
            &self.ops.env().original_xdg,
            self.ops.env().path(),
            &extra,
        );
        let chain_start = Instant::now();
        let mut tried: Vec<String> = Vec::new();
        let mut last_reason = String::new();
        // One `agent_name_taken` per launch is recovered from: the holders are
        // closed and the same step runs again in a fresh pane.
        let mut name_recovered = false;
        self.evict_key_panes(key, &cwd);
        let mut index = 0;
        while let Some(step) = chain.get(index) {
            let remaining = timing.chain_budget.saturating_sub(chain_start.elapsed());
            if index > 0 && remaining < timing.poll.max(Duration::from_millis(1)) {
                self.push_trail(
                    key,
                    TrailStage::Launch,
                    json!({ "budgetExhausted": true, "index": index }),
                );
                break;
            }
            self.drop_mapped_pane(key);
            let id = match self.ops.workspace_create(&cwd, &pane_env) {
                Ok(id) => id,
                Err(e) => {
                    self.push_trail(
                        key,
                        TrailStage::Launch,
                        json!({ "index": index, "workspaceError": e.to_string() }),
                    );
                    return Err(e.into());
                }
            };
            if !timing.shell_ready.is_zero() {
                std::thread::sleep(timing.shell_ready);
            }
            let args = agent::build_agent_args_opts(
                STRICT_CLI,
                launch.autonomy,
                &step.decision,
                step.hook_trust,
            );
            tried.push(step.label.clone());
            self.push_trail(
                key,
                TrailStage::Launch,
                json!({ "index": index, "label": step.label, "args": args, "paneId": id.pane_id }),
            );
            let step_cap = timing
                .step_cap
                .min(timing.chain_budget.saturating_sub(chain_start.elapsed()));
            let step_deadline = Instant::now() + step_cap;
            let outcome =
                match self.strict_step(key, &name, &id.pane_id, &args, step, timing, step_deadline)
                {
                    Err(StepError::NameTaken(e)) if !name_recovered => {
                        match self.recover_name_taken(key, &name, &cwd, &id.pane_id) {
                            Some(closed) => {
                                self.push_trail(
                                    key,
                                    TrailStage::Launch,
                                    json!({ "index": index, "nameTaken": closed }),
                                );
                                name_recovered = true;
                                // The same step runs again; it is listed once.
                                tried.pop();
                                continue;
                            }
                            // `agent list` failed: nothing was closed; the step fails.
                            None => Err(StepError::NameTaken(e)),
                        }
                    }
                    other => other,
                };
            match outcome {
                Ok(()) => {
                    let (gen, result, fell_back) = self.finish_strict_launch(
                        key,
                        cwd,
                        cols,
                        rows,
                        &id,
                        &step.decision,
                        &first,
                    );
                    self.push_trail(
                        key,
                        TrailStage::Launch,
                        json!({ "index": index, "started": step.label, "fellBack": fell_back }),
                    );
                    return Ok((gen, result, fell_back));
                }
                Err(failure) => {
                    let name_taken = matches!(failure, StepError::NameTaken(_));
                    let reason = failure.reason();
                    let tail = self
                        .ops
                        .pane_read(&id.pane_id, "recent", "text", Some(20))
                        .unwrap_or_default();
                    self.push_trail(
                        key,
                        TrailStage::Verify,
                        json!({
                            "index": index,
                            "failed": reason,
                            "paneTail": super::truncate_at_char_boundary(&tail, PANE_TAIL_CAP),
                        }),
                    );
                    self.close_quietly(key, &id.pane_id);
                    lifecycle::record(LifecycleEvent::ResumeOutcome {
                        key: key.clone(),
                        outcome: ResumeResult::Failed {
                            reason: format!("{} failed: {reason}", step.label),
                        },
                    });
                    log::warn!(
                        "herdr: strict launch of {key}: {} failed: {reason}",
                        step.label
                    );
                    last_reason = reason;
                    // Every later step would start under the same name.
                    if name_taken {
                        break;
                    }
                }
            }
            index += 1;
        }
        Err(PtyBackendError::Other(format_final_error(
            &last_reason,
            &tried,
        )))
    }

    /// Run one step on `pane_id`: `agent.start` (re-issued on unexpected errors,
    /// but not on `agent_name_taken`, which only closing the holder clears),
    /// then watch the pane.
    #[allow(clippy::too_many_arguments)]
    fn strict_step(
        &self,
        key: &SessionKey,
        name: &str,
        pane_id: &str,
        args: &[String],
        step: &StrictStep,
        timing: &StrictTiming,
        step_deadline: Instant,
    ) -> Result<(), StepError> {
        let kind = agent::herdr_kind(STRICT_CLI);
        let mut reissues = 0usize;
        let window_end = loop {
            match self
                .ops
                .agent_start(name, kind, pane_id, args, AGENT_START_TIMEOUT_MS)
            {
                Ok(()) => break (Instant::now() + timing.verify_window).min(step_deadline),
                Err(e) if agent::is_early_exit(&e) => {
                    return Err(StepError::Failed(early_exit_reason(&e)))
                }
                Err(e) if agent::is_name_taken(&e) => return Err(StepError::NameTaken(e)),
                // Not a failure: the agent may still be loading a large thread.
                Err(e) if is_agent_start_timeout(&e) => {
                    self.push_trail(
                        key,
                        TrailStage::Launch,
                        json!({ "startTimeout": e.to_string() }),
                    );
                    break step_deadline;
                }
                Err(e)
                    if !e.is_not_found()
                        && !e.is_exe_missing()
                        && reissues < timing.reissue_backoffs.len()
                        && Instant::now() + timing.reissue_backoffs[reissues] < step_deadline =>
                {
                    std::thread::sleep(timing.reissue_backoffs[reissues]);
                    reissues += 1;
                    self.push_trail(
                        key,
                        TrailStage::Launch,
                        json!({ "reissue": reissues, "error": e.to_string() }),
                    );
                }
                Err(e) => return Err(StepError::Failed(e.to_string())),
            }
        };
        let verdict = self.verify(
            key,
            pane_id,
            &Expect::of(&step.decision),
            timing,
            window_end,
        );
        match verdict.failure() {
            None => Ok(()),
            Some(reason) => Err(StepError::Failed(reason)),
        }
    }

    /// Watch `pane_id` until a verdict, or until `window_end` (then settle it).
    fn verify(
        &self,
        key: &SessionKey,
        pane_id: &str,
        expect: &Expect,
        timing: &StrictTiming,
        window_end: Instant,
    ) -> Verdict {
        let mut state = VerifyState::default();
        loop {
            let obs = self.ops.pane_get(pane_id);
            let classified = classify_observation(&mut state, expect, &obs);
            if let Some(detail) = classified.trail {
                self.push_trail(key, TrailStage::Verify, detail);
            }
            if let Some(verdict) = classified.verdict {
                return verdict;
            }
            let now = Instant::now();
            if now >= window_end {
                break;
            }
            std::thread::sleep(timing.poll.min(window_end - now));
        }
        match classify_window_end(&state, expect) {
            Verdict::NeverAppeared => {
                let info = self.ops.pane_process_info(pane_id);
                let verdict = classify_never_appeared(&info);
                let detail = match (&verdict, &info) {
                    (Verdict::PresentUnknown, Ok(info)) => json!({
                        "present_unknown": true,
                        "reason": "foreground_process",
                        "foregroundProcesses": info.foreground_pids.len(),
                    }),
                    (_, Ok(_)) => json!({ "neverAppeared": true, "foregroundProcesses": 0 }),
                    (_, Err(e)) => {
                        json!({ "neverAppeared": true, "processInfoError": e.to_string() })
                    }
                };
                self.push_trail(key, TrailStage::Verify, detail);
                verdict
            }
            Verdict::PresentUnknown => {
                self.push_trail(
                    key,
                    TrailStage::Verify,
                    json!({ "present_unknown": true, "reason": "no_session" }),
                );
                Verdict::PresentUnknown
            }
            other => other,
        }
    }

    /// Record a verified pane as `key`'s session and report the outcome: the
    /// same tail as the default launch (keymap, live set, bridge, recovery
    /// bookkeeping, lifecycle record, `resume` event).
    #[allow(clippy::too_many_arguments)]
    fn finish_strict_launch(
        &self,
        key: &SessionKey,
        cwd: String,
        cols: u16,
        rows: u16,
        id: &PaneIdentity,
        decision: &ResumeDecision,
        first: &ResumeDecision,
    ) -> (u64, ResumeResult, bool) {
        let gen = self.keymap.record(
            key.clone(),
            id.workspace_id.clone(),
            id.pane_id.clone(),
            id.terminal_id.clone(),
            cwd,
        );
        self.live.lock().unwrap().insert(key.clone());
        if let Err(e) = start_bridge_in(
            &*self.ops,
            &self.bridges,
            &self.outputs,
            &self.keymap,
            &self.exit_authority,
            key,
            gen,
            &id.terminal_id,
            cols.max(1),
            rows.max(1),
        ) {
            log::warn!(
                "herdr: strict launch bridge start failed for {}: {e}",
                key.worktree
            );
        }
        self.keymap.resolve_recovered(key);
        let (result, word) = match decision {
            ResumeDecision::Resume { id, .. } => {
                (ResumeResult::Resumed { id: id.clone() }, "resumed")
            }
            ResumeDecision::Continue => (ResumeResult::Continued, "continued"),
            ResumeDecision::Fresh => (ResumeResult::Fresh, "fresh"),
        };
        lifecycle::record(LifecycleEvent::ResumeOutcome {
            key: key.clone(),
            outcome: result.clone(),
        });
        let fell_back = decision != first;
        // The thread the chain could not resume must not come back on the next
        // default restart: forget it (the cleared id is never accepted again) and
        // let herdr's observation record the fresh session.
        if fell_back && matches!(first, ResumeDecision::Resume { .. }) {
            self.store.clear(key);
        }
        if fell_back || *decision != ResumeDecision::Fresh {
            self.sink.emit(
                &session_event("resume", key),
                json!({ "outcome": word, "fell_back": fell_back }),
            );
        }
        (gen, result, fell_back)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::herdr::cli::AgentSession;
    use crate::backend::LaunchPolicy;

    fn launch(resume_target: Option<&str>, fresh: bool) -> AgentLaunch {
        AgentLaunch {
            cli: CliKind::Codex,
            path: "codex".into(),
            autonomy: false,
            resume_target: resume_target.map(str::to_string),
            fresh,
            policy: LaunchPolicy::TransferStrict,
        }
    }

    fn labels(chain: &[StrictStep]) -> Vec<&str> {
        chain.iter().map(|s| s.label.as_str()).collect()
    }

    #[test]
    fn strict_chain_resume_has_three_steps() {
        let chain = strict_chain(&launch(Some("thr-1"), false));
        assert_eq!(
            labels(&chain),
            vec!["resume thr-1", "fresh", "fresh without optional flags"]
        );
        assert_eq!(
            chain.iter().map(|s| s.hook_trust).collect::<Vec<_>>(),
            vec![true, true, false]
        );
        assert_eq!(
            chain[0].decision,
            ResumeDecision::Resume {
                cli: CliKind::Codex,
                id: "thr-1".into()
            }
        );
        assert_eq!(chain[1].decision, ResumeDecision::Fresh);
        assert_eq!(chain[2].decision, ResumeDecision::Fresh);
    }

    #[test]
    fn strict_chain_fresh_starts_at_fresh() {
        let chain = strict_chain(&launch(Some("thr-1"), true));
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].decision, ResumeDecision::Fresh);
        assert_eq!(
            labels(&chain),
            vec!["fresh", "fresh without optional flags"]
        );
    }

    #[test]
    fn strict_chain_without_target_starts_at_fresh() {
        let chain = strict_chain(&launch(None, false));
        assert_eq!(
            labels(&chain),
            vec!["fresh", "fresh without optional flags"]
        );
        assert_eq!(chain[0].decision, ResumeDecision::Fresh);
    }

    #[test]
    fn strict_chain_skips_an_unsafe_thread_id() {
        let chain = strict_chain(&launch(Some("a b; --last"), false));
        assert!(chain
            .iter()
            .all(|s| !matches!(s.decision, ResumeDecision::Resume { .. })));
        assert_eq!(chain.len(), 2);
    }

    #[test]
    fn strict_chain_never_contains_continue() {
        for fresh in [false, true] {
            for target in [None, Some("thr-1"), Some("a b; --last")] {
                let chain = strict_chain(&launch(target, fresh));
                assert!(
                    chain.iter().all(|s| s.decision != ResumeDecision::Continue),
                    "fresh={fresh} target={target:?}"
                );
                for step in &chain {
                    let args = agent::build_agent_args_opts(
                        CliKind::Codex,
                        false,
                        &step.decision,
                        step.hook_trust,
                    );
                    assert!(!args.iter().any(|a| a == "--last"), "{args:?}");
                }
            }
        }
    }

    #[test]
    fn strict_timing_fast_under_test_and_production_values() {
        assert_eq!(strict_timing(), StrictTiming::fast());
        let p = StrictTiming::production();
        assert_eq!(p.shell_ready, Duration::from_millis(300));
        assert_eq!(p.poll, Duration::from_millis(250));
        assert_eq!(p.verify_window, Duration::from_secs(10));
        assert_eq!(p.step_cap, Duration::from_secs(45));
        assert_eq!(p.chain_budget, Duration::from_secs(100));
        assert_eq!(
            p.reissue_backoffs,
            [Duration::from_millis(300), Duration::from_millis(800)]
        );
        // The whole chain stays under the frontend's 120 s restart backstop.
        assert!(p.chain_budget < Duration::from_secs(120));
    }

    #[test]
    fn sanitize_ui_tokens_lowercases_recovering_and_terminated() {
        assert_eq!(
            sanitize_ui_tokens("Recovering then Terminated, recovering ok"),
            "recovering then terminated, recovering ok"
        );
        assert_eq!(sanitize_ui_tokens("RECOVERING"), "RECOVERING");
    }

    #[test]
    fn format_final_error_shape() {
        let tried = vec!["resume thr-1".to_string(), "fresh".to_string()];
        assert_eq!(
            format_final_error("exited early", &tried),
            "codex did not start: exited early (tried: resume thr-1 -> fresh)"
        );
        // Whitespace (newlines included) collapses to single spaces.
        assert_eq!(
            format_final_error("  line one\n\tline   two \r\n", &tried),
            "codex did not start: line one line two (tried: resume thr-1 -> fresh)"
        );
        // Capped at 300 chars.
        let long = "x".repeat(500);
        let out = format_final_error(&long, &tried);
        assert!(
            out.contains(&format!("start: {} (tried", "x".repeat(300))),
            "{out}"
        );
        assert!(!out.contains(&"x".repeat(301)));
        // The pane's error tokens never appear verbatim.
        let out = format_final_error("server Recovering / session Terminated", &tried);
        assert!(
            !out.contains("Recovering") && !out.contains("Terminated"),
            "{out}"
        );
    }

    #[test]
    fn is_agent_start_timeout_matches_code_message_and_transport_timeout() {
        let api = |code: &str, message: &str| HerdrError::Api {
            code: code.into(),
            message: message.into(),
        };
        assert!(is_agent_start_timeout(&HerdrError::Timeout));
        assert!(is_agent_start_timeout(&api("timeout", "anything")));
        assert!(is_agent_start_timeout(&api(
            "",
            "Timed out waiting for agent startup"
        )));
        assert!(!is_agent_start_timeout(&api("agent_start_failed", "exit")));
        assert!(!is_agent_start_timeout(&api("pane_busy", "busy")));
        assert!(!is_agent_start_timeout(&HerdrError::Parse {
            message: "bad".into()
        }));
    }

    // ---- verify state machine ----

    fn session(agent: &str, kind: AgentSessionKind, value: &str, source: &str) -> AgentSession {
        AgentSession {
            source: source.into(),
            agent: agent.into(),
            kind,
            value: value.into(),
        }
    }

    fn pane(agent: Option<&str>, s: Option<AgentSession>) -> Result<Option<PaneInfo>, HerdrError> {
        Ok(Some(PaneInfo {
            pane_id: "w1:p1".into(),
            agent: agent.map(str::to_string),
            agent_status: agent.map(|_| "idle".to_string()),
            agent_session: s,
            ..Default::default()
        }))
    }

    fn codex_id(value: &str) -> Option<AgentSession> {
        Some(session("codex", AgentSessionKind::Id, value, "herdr:codex"))
    }

    fn not_found() -> HerdrError {
        HerdrError::Api {
            code: "pane_not_found".into(),
            message: "no pane".into(),
        }
    }

    /// Run `obs` through one state; the first verdict (and its index), else the
    /// window-end verdict.
    fn run(
        expect: &Expect,
        obs: &[Result<Option<PaneInfo>, HerdrError>],
    ) -> (Verdict, VerifyState) {
        let mut state = VerifyState::default();
        for o in obs {
            if let Some(v) = classify_observation(&mut state, expect, o).verdict {
                return (v, state);
            }
        }
        (classify_window_end(&state, expect), state)
    }

    fn resume() -> Expect {
        Expect::Resume("thr-1".into())
    }

    #[test]
    fn classify_observation_confirms_resume_on_matching_id() {
        let (v, _) = run(&resume(), &[pane(Some("codex"), codex_id("thr-1"))]);
        assert_eq!(v, Verdict::Confirmed);
    }

    #[test]
    fn classify_observation_ignores_source() {
        let s = Some(session(
            "codex",
            AgentSessionKind::Id,
            "thr-1",
            "other:thing",
        ));
        let (v, _) = run(&resume(), &[pane(Some("codex"), s)]);
        assert_eq!(v, Verdict::Confirmed);
    }

    #[test]
    fn classify_observation_resume_mismatch_only_at_window_end() {
        let mut state = VerifyState::default();
        for _ in 0..5 {
            let c = classify_observation(
                &mut state,
                &resume(),
                &pane(Some("codex"), codex_id("other")),
            );
            assert_eq!(c.verdict, None, "a mismatch never ends the window early");
        }
        assert_eq!(
            classify_window_end(&state, &resume()),
            Verdict::Mismatch("other".into())
        );
        // A path-kind session is not a mismatch.
        let s = Some(session(
            "codex",
            AgentSessionKind::Path,
            "/x/rollout.jsonl",
            "herdr:codex",
        ));
        let (v, _) = run(&resume(), &[pane(Some("codex"), s)]);
        assert_eq!(v, Verdict::PresentUnknown);
        // A late match wins over an earlier mismatch.
        let (v, _) = run(
            &resume(),
            &[
                pane(Some("codex"), codex_id("other")),
                pane(Some("codex"), codex_id("thr-1")),
            ],
        );
        assert_eq!(v, Verdict::Confirmed);
    }

    #[test]
    fn classify_observation_fresh_needs_two_consecutive_polls() {
        let mut state = VerifyState::default();
        let one = classify_observation(&mut state, &Expect::Fresh, &pane(Some("codex"), None));
        assert_eq!(one.verdict, None);
        let two = classify_observation(&mut state, &Expect::Fresh, &pane(Some("codex"), None));
        assert_eq!(two.verdict, Some(Verdict::Confirmed));
        // An observation without the agent resets the count (and is a vanish).
        let (v, _) = run(
            &Expect::Fresh,
            &[pane(Some("codex"), None), pane(None, None)],
        );
        assert_eq!(v, Verdict::Vanished);
        // Another agent label is never a fresh codex.
        let (v, _) = run(
            &Expect::Fresh,
            &[pane(Some("claude"), None), pane(Some("claude"), None)],
        );
        assert_eq!(v, Verdict::PresentUnknown);
    }

    #[test]
    fn classify_observation_vanished_after_seen() {
        let (v, _) = run(&resume(), &[pane(Some("codex"), None), pane(None, None)]);
        assert_eq!(v, Verdict::Vanished);
    }

    #[test]
    fn classify_observation_never_seen_defers_to_the_process_check() {
        let (v, state) = run(
            &resume(),
            &[pane(None, None), pane(None, None), pane(None, None)],
        );
        assert_eq!(v, Verdict::NeverAppeared);
        assert!(!state.seen_agent);
        let (v, _) = run(&Expect::Fresh, &[pane(None, None)]);
        assert_eq!(v, Verdict::NeverAppeared);
    }

    #[test]
    fn classify_observation_agent_without_session_is_present_unknown() {
        let (v, _) = run(
            &resume(),
            &[pane(Some("codex"), None), pane(Some("codex"), None)],
        );
        assert_eq!(v, Verdict::PresentUnknown);
    }

    #[test]
    fn classify_observation_gone_on_not_found() {
        let (v, _) = run(&resume(), &[Err(not_found())]);
        assert_eq!(v, Verdict::Gone);
        let (v, _) = run(&Expect::Fresh, &[Ok(None)]);
        assert_eq!(v, Verdict::Gone);
    }

    #[test]
    fn classify_observation_transient_errors_do_not_fail() {
        let (v, state) = run(
            &resume(),
            &[
                Err(HerdrError::Timeout),
                Err(HerdrError::Parse {
                    message: "bad".into(),
                }),
                pane(Some("codex"), codex_id("thr-1")),
            ],
        );
        assert_eq!(v, Verdict::Confirmed);
        assert_eq!(state.transient_errors, 2);
    }

    #[test]
    fn classify_observation_trail_entry_only_when_the_observation_changes() {
        let mut state = VerifyState::default();
        let first = classify_observation(&mut state, &Expect::Fresh, &pane(None, None));
        assert_eq!(
            first.trail,
            Some(json!({ "agent": null, "status": null, "session": null }))
        );
        let again = classify_observation(&mut state, &Expect::Fresh, &pane(None, None));
        assert_eq!(again.trail, None, "an unchanged observation writes nothing");
        let changed = classify_observation(
            &mut state,
            &Expect::Fresh,
            &pane(Some("codex"), codex_id("s-1")),
        );
        assert_eq!(
            changed.trail,
            Some(json!({
                "agent": "codex",
                "status": "idle",
                "session": { "agent": "codex", "kind": "id", "value": "s-1" },
            }))
        );
        // Repeated identical errors are logged once too.
        let mut state = VerifyState::default();
        let e1 = classify_observation(&mut state, &Expect::Fresh, &Err(HerdrError::Timeout));
        let e2 = classify_observation(&mut state, &Expect::Fresh, &Err(HerdrError::Timeout));
        assert!(e1.trail.is_some());
        assert!(e2.trail.is_none());
    }

    #[test]
    fn classify_never_appeared_foreground_process_is_present_unknown() {
        let info = Ok(PaneProcessInfo {
            shell_pid: 1,
            foreground_pids: vec![4200],
        });
        assert_eq!(classify_never_appeared(&info), Verdict::PresentUnknown);
    }

    #[test]
    fn classify_never_appeared_empty_foreground_is_failure() {
        let info = Ok(PaneProcessInfo {
            shell_pid: 1,
            foreground_pids: vec![],
        });
        assert_eq!(classify_never_appeared(&info), Verdict::NeverAppeared);
    }

    #[test]
    fn classify_never_appeared_process_info_error_is_failure() {
        assert_eq!(
            classify_never_appeared(&Err(not_found())),
            Verdict::NeverAppeared
        );
        assert_eq!(
            classify_never_appeared(&Err(HerdrError::Timeout)),
            Verdict::NeverAppeared
        );
    }

    #[test]
    fn verdict_failure_reasons() {
        assert_eq!(Verdict::Confirmed.failure(), None);
        assert_eq!(Verdict::PresentUnknown.failure(), None);
        assert_eq!(
            Verdict::Mismatch("other".into()).failure().unwrap(),
            "session mismatch: pane reports other"
        );
        assert_eq!(
            Verdict::Gone.failure().unwrap(),
            "pane closed during startup"
        );
        assert_eq!(
            Verdict::Vanished.failure().unwrap(),
            "agent vanished after start"
        );
        assert_eq!(
            Verdict::NeverAppeared.failure().unwrap(),
            "agent never appeared"
        );
    }
}
