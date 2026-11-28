//! Self-diagnostics: the app checks its own drive plumbing as it runs and reports
//! what disagrees or stalls (hooks that never report, turns finished without a Stop
//! hook, a stalled request pipeline, a restarted watcher, a lost herdr link, ...).
//!
//! The core is pure: [`evaluate`] turns one [`HealthInputs`] snapshot into the
//! findings that hold now, with no I/O and no clock reads. [`reconcile::Reconciler`]
//! keeps the active findings across evaluations (hysteresis, episodes, remedies) and
//! says what to write to `status.db`. [`driver::HealthMonitor`] gathers the inputs
//! and runs both on the pipeline thread; [`commands`] serves the window.

pub mod commands;
pub mod driver;
pub mod reconcile;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use keitora_tasks::{age_text, clock_text, CliKind, ProjectTrust, StatusSession};
use serde::Serialize;
use serde_json::{json, Value};

use crate::engine::model::HookEvidence;
use crate::engine::turns::TurnHealth;
use crate::engine::writer::LeaseState;
use crate::engine::{HerdrLink, KeyHealth};
use crate::hooks::{ClaudeSettings, HookFacts};
use crate::watchers::{RestartWhy, WatcherKind, RESTART_WINDOW_MS};

/// How bad a finding is. `Info` shows only in Settings and `keitora status`; `Warn`
/// and `Error` also badge the rail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl Severity {
    /// The `findings.severity` spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }

    /// `Error` when `error` holds, else `Warn`: a check that turns worse past a limit.
    pub fn escalate(error: bool) -> Severity {
        if error {
            Severity::Error
        } else {
            Severity::Warn
        }
    }
}

/// What the app does about a finding when it appears.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Remedy {
    /// Re-run the launch preparation for the worktree's provider (its hook files).
    ReinstallHooks { project: String, worktree: String },
    /// Re-assert the global agy hook entry.
    SyncAgyHooks { project: String, worktree: String },
    /// Ask for a pass over the project's pending requests.
    RequestPass { project: String },
    /// Refresh the managed `keitora` copy on PATH from the app's bundled CLI.
    RefreshCli,
}

impl Remedy {
    /// The `findings.remedy` text.
    pub fn label(&self) -> &'static str {
        match self {
            Remedy::ReinstallHooks { .. } => "reinstalled hooks",
            Remedy::SyncAgyHooks { .. } => "re-asserted agy hooks",
            Remedy::RequestPass { .. } => "requested a pass",
            Remedy::RefreshCli => "refreshed the managed CLI",
        }
    }
}

/// One thing wrong right now.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    /// `""` for app-level findings.
    pub project: String,
    /// `""` for project- or app-level findings.
    pub worktree: String,
    pub check_id: &'static str,
    pub severity: Severity,
    pub message: String,
    pub detail: Value,
    /// What to do when the finding appears; `None`: report only.
    pub remedy: Option<Remedy>,
    /// What its owner already did about it (e.g. the supervisor restarted a
    /// watcher): recorded as the row's remedy, nothing runs.
    pub note: Option<&'static str>,
}

impl Finding {
    /// A report-only finding (no detail, no remedy). `project` is `""` for app-level
    /// findings, `worktree` `""` for project- or app-level ones.
    pub fn new(
        project: &str,
        worktree: &str,
        check_id: &'static str,
        severity: Severity,
        message: String,
    ) -> Finding {
        Finding {
            project: project.to_string(),
            worktree: worktree.to_string(),
            check_id,
            severity,
            message,
            detail: json!({}),
            remedy: None,
            note: None,
        }
    }
}

// ── Check ids ────────────────────────────────────────────────────────────────

pub const HOOKS_VS_HERDR: &str = "hooks_vs_herdr";
pub const COMPLETION_WITHOUT_STOP: &str = "completion_without_stop";
pub const TURN_START_NOT_OBSERVED: &str = "turn_start_not_observed";
pub const HOOKS_MISSING: &str = "hooks_missing";
pub const CODEX_PROJECT_UNTRUSTED: &str = "codex_project_untrusted";
pub const AGY_PLUMBING: &str = "agy_plumbing";
pub const AGENT_START_REFUSED: &str = "agent_start_refused";
pub const WORKTREE_IDENTITY: &str = "worktree_identity";
pub const PIPELINE_STALLED: &str = "pipeline_stalled";
pub const WATCHER_RESTARTED: &str = "watcher_restarted";
pub const WATCHER_SILENT: &str = "watcher_silent";
pub const HERDR_LINK: &str = "herdr_link";
pub const HERDR_UNKNOWN: &str = "herdr_unknown";
pub const STATUS_WRITER: &str = "status_writer";
pub const VERSION_SKEW: &str = "version_skew";
pub const UNCONFIGURED_SESSIONS: &str = "unconfigured_sessions";

/// Checks that clear on the first evaluation that no longer finds them (the rest
/// need two in a row).
const INSTANT_CLEAR: [&str; 6] = [
    COMPLETION_WITHOUT_STOP,
    TURN_START_NOT_OBSERVED,
    PIPELINE_STALLED,
    HERDR_LINK,
    HERDR_UNKNOWN,
    STATUS_WRITER,
];

/// Whether `check_id` clears on its first absence.
pub fn clears_instantly(check_id: &str) -> bool {
    INSTANT_CLEAR.contains(&check_id)
}

/// The note of the restart findings: the supervisor already restarted it.
pub const RESTARTED_WATCHER: &str = "restarted watcher";

// ── Thresholds ───────────────────────────────────────────────────────────────

const SEC: i64 = 1000;
const MIN: i64 = 60 * SEC;
const HOUR: i64 = 60 * MIN;

/// How long an agent must be present with silent hooks before `hooks_vs_herdr`.
pub const HOOKS_SILENT_AFTER_MS: i64 = 30 * SEC;
/// How far a hook write may precede herdr's first sight of the agent and still count
/// as the agent's: `SessionStart` runs before the next `agent list` sees the agent,
/// which may be one cold interval (up to 30 s) later.
pub const HOOK_WRITE_SLACK_MS: i64 = 60 * SEC;
/// How long a `no activity` stall counts for `turn_start_not_observed`.
pub const STALL_MEMORY_MS: i64 = 24 * HOUR;
/// How old the oldest pending request is before `pipeline_stalled`, and how old the
/// last pass may be.
pub const PIPELINE_STALL_MS: i64 = 30 * SEC;
/// `pipeline_stalled` turns `error` at this age.
pub const PIPELINE_ERROR_MS: i64 = 120 * SEC;
/// `watcher_restarted` turns `error` at this many restarts in the window.
pub const RESTARTS_ERROR_AT: usize = 3;
/// How long the link may be down before `herdr_link`, and when it turns `error`.
pub const LINK_WARN_MS: i64 = 15 * SEC;
pub const LINK_ERROR_MS: i64 = 120 * SEC;
/// How long herdr may report `unknown` before `herdr_unknown`.
pub const UNKNOWN_AFTER_MS: i64 = crate::engine::UNKNOWN_FINDING_AFTER.as_millis() as i64;
/// How recent a session row of an unconfigured worktree must be to count.
pub const UNCONFIGURED_WINDOW_MS: i64 = 24 * HOUR;
/// How many unconfigured names are listed before `+n more`.
const UNCONFIGURED_LISTED: usize = 5;

// ── Inputs ───────────────────────────────────────────────────────────────────

/// One snapshot of everything the checks read. Gathered by the driver; plain data.
#[derive(Clone, Debug, Default)]
pub struct HealthInputs {
    /// The engine's link to herdr; `None` without an engine.
    pub link: Option<LinkFacts>,
    /// This instance's `status.db` lease; `None` without a publisher.
    pub lease: Option<LeaseState>,
    /// The `keitora` on PATH, when one answered `--version`.
    pub path_cli: Option<PathCli>,
    pub projects: Vec<ProjectInputs>,
}

/// The engine's link state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkFacts {
    pub link: HerdrLink,
    pub since_ms: i64,
    pub last_snapshot_ms: Option<i64>,
}

/// The `keitora` found on PATH, next to the CLI bundled with the app (the CLI crate
/// is versioned on its own, so the app's own version is no reference).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathCli {
    pub path: String,
    /// What `keitora --version` printed after the name, e.g. `0.6.0`.
    pub version: String,
    /// A cargo build (`target/debug`, ...): never compared.
    pub dev_build: bool,
    /// What the bundled CLI's `--version` printed; `None`: no bundled CLI (a dev app).
    pub bundled_version: Option<String>,
    /// PATH's `keitora` is the managed copy and differs from the bundled CLI.
    pub managed_stale: bool,
    /// PATH's `keitora` is the managed copy and this platform refreshes it (what
    /// [`Remedy::RefreshCli`] can fix).
    pub refreshable: bool,
}

/// One registered project.
#[derive(Clone, Debug, Default)]
pub struct ProjectInputs {
    /// The resolved `[project].name`.
    pub name: String,
    /// What the request pipeline knows of it.
    pub pipeline: ProjectPipeline,
    /// Every hook row (`worktree_sessions`) of the project's DB: worktree, last write.
    pub hook_rows: Vec<(String, Option<i64>)>,
    /// The configured worktrees.
    pub worktrees: Vec<WorktreeInputs>,
}

/// What the request pipeline knows of one project, for the checks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectPipeline {
    /// The oldest pending drive request, as the last sweep found it.
    pub oldest_pending: Option<PendingFacts>,
    /// Unix ms of the last pass over the project's requests.
    pub last_pass_ms: Option<i64>,
    /// Watcher restarts the supervisor made in the last [`RESTART_WINDOW_MS`] (the
    /// pipeline windows them).
    pub restarts: Vec<RestartNote>,
}

/// A project's oldest pending request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingFacts {
    pub id: i64,
    pub created_at_ms: i64,
    /// A pass has examined it (`seen_at` set).
    pub seen: bool,
}

/// One supervisor restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartNote {
    pub at_ms: i64,
    pub watcher: WatcherKind,
    pub why: RestartWhy,
}

/// One configured worktree.
#[derive(Clone, Debug)]
pub struct WorktreeInputs {
    pub name: String,
    /// The last component of its folder; `None` when unknown.
    pub folder: Option<String>,
    pub provider: CliKind,
    /// A driver binds it.
    pub driven: bool,
    /// The engine's view; `None` without one (never launched in this server).
    pub view: Option<StatusSession>,
    pub key: Option<KeyHealth>,
    /// Its hook row.
    pub hook: Option<HookEvidence>,
    pub turns: Option<TurnHealth>,
    /// What is installed for it; `None` when not read (e.g. the folder is missing).
    pub facts: Option<HookFacts>,
    /// Codex's `config.toml`, for the trust message.
    pub codex_config: Option<String>,
}

impl WorktreeInputs {
    /// A worktree with nothing known yet.
    #[cfg(test)]
    pub fn new(name: &str, provider: CliKind) -> WorktreeInputs {
        WorktreeInputs {
            name: name.to_string(),
            folder: Some(name.to_string()),
            provider,
            driven: false,
            view: None,
            key: None,
            hook: None,
            turns: None,
            facts: None,
            codex_config: None,
        }
    }
}

// ── Text helpers ─────────────────────────────────────────────────────────────

/// The provider's short name in messages.
pub fn provider_label(cli: CliKind) -> &'static str {
    match cli {
        CliKind::Claude => "claude",
        CliKind::Codex => "codex",
        CliKind::Antigravity => "agy",
    }
}

// ── Evaluate ─────────────────────────────────────────────────────────────────

/// Every finding that holds in `inputs` at `now_ms`, sorted by key. Pure.
pub fn evaluate(inputs: &HealthInputs, now_ms: i64) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(herdr_link(inputs, now_ms));
    out.extend(status_writer(inputs));
    out.extend(version_skew(inputs));
    let link_up = inputs.link.is_none_or(|l| l.link == HerdrLink::Up);
    for p in &inputs.projects {
        out.extend(pipeline_stalled(p, now_ms));
        out.extend(watcher_restarted(p));
        out.extend(watcher_silent(p));
        out.extend(unconfigured_sessions(p, now_ms));
        for w in &p.worktrees {
            // The view-based checks need a live, fresh view.
            if link_up && w.view.as_ref().is_some_and(|v| !v.stale) {
                out.extend(hooks_vs_herdr(p, w, now_ms));
                out.extend(herdr_unknown(p, w, now_ms));
            }
            out.extend(completion_without_stop(p, w));
            out.extend(turn_start_not_observed(p, w, now_ms));
            out.extend(hooks_missing(p, w));
            out.extend(codex_project_untrusted(p, w));
            out.extend(agy_plumbing(p, w));
            out.extend(agent_start_refused(p, w));
            out.extend(worktree_identity(p, w));
        }
    }
    out.sort_by(|a, b| {
        (&a.project, &a.worktree, a.check_id).cmp(&(&b.project, &b.worktree, b.check_id))
    });
    out
}

/// herdr sees an agent, but its hook row says nothing about it.
pub fn hooks_vs_herdr(p: &ProjectInputs, w: &WorktreeInputs, now_ms: i64) -> Option<Finding> {
    let v = w.view.as_ref()?;
    if v.agent != "present" {
        return None;
    }
    let since = w.key.as_ref()?.present_since_ms?;
    if now_ms - since < HOOKS_SILENT_AFTER_MS {
        return None;
    }
    let why = match &w.hook {
        None => "no hook rows".to_string(),
        Some(h) => {
            let other_session = h.is_from_other_session(v.agent_session.as_deref());
            let before = h.written_ms.is_none_or(|t| t < since - HOOK_WRITE_SLACK_MS);
            if !(other_session || before || h.state == "offline") {
                return None;
            }
            match h.written_ms {
                Some(t) => format!(
                    "last hook write {} ago, state {}",
                    age_text((now_ms - t) / 1000),
                    h.state
                ),
                None => format!("state {}", h.state),
            }
        }
    };
    let status = v.herdr_status.as_deref().unwrap_or("unknown");
    let mut f = Finding::new(
        &p.name,
        &w.name,
        HOOKS_VS_HERDR,
        Severity::Warn,
        format!(
            "@{}: herdr sees a {} agent ({status}) but its hooks have not reported since the \
             agent appeared ({why}). Keitora is using herdr's view.",
            w.name,
            provider_label(w.provider)
        ),
    );
    f.detail = json!({
        "present_since": since,
        "hook_state": w.hook.as_ref().map(|h| h.state.clone()),
        "hook_written_at": w.hook.as_ref().and_then(|h| h.written_ms),
    });
    Some(f)
}

/// Driven turns keep finishing without the Stop hook.
pub fn completion_without_stop(p: &ProjectInputs, w: &WorktreeInputs) -> Option<Finding> {
    // agy's hooks record state only, never a reply: a herdr-only finish is its normal.
    if w.provider == CliKind::Antigravity {
        return None;
    }
    let recent = &w.turns.as_ref()?.recent;
    let last = |n: usize| &recent[recent.len().saturating_sub(n)..];
    let hookless =
        |c: &&crate::engine::turns::CompletionNote| c.signal == "herdr" && c.reply_source != "hook";
    let window = last(5);
    let n = window.iter().filter(hookless).count();
    if n < 2 {
        return None;
    }
    let latest3 = last(3);
    if latest3.len() == 3 && latest3.iter().all(|c| c.reply_source == "hook") {
        return None;
    }
    let latest = recent.last()?;
    let severity = if latest.reply_source == "none" {
        Severity::Warn
    } else {
        Severity::Info
    };
    let newest_hookless = window.iter().rev().find(hookless)?;
    let from = match newest_hookless.reply_source {
        "transcript" => "the transcript",
        _ => "nowhere (use keitora read)",
    };
    let mut f = Finding::new(
        &p.name,
        &w.name,
        COMPLETION_WITHOUT_STOP,
        severity,
        format!(
            "@{}: {n} of the last {} turns finished with no Stop hook; reply text came from {from}",
            w.name,
            window.len()
        ),
    );
    let misses: Vec<&str> = window.iter().filter_map(|c| c.transcript_miss).collect();
    f.detail = json!({ "transcript_misses": misses });
    Some(f)
}

/// A delivery sat with no herdr activity at all.
pub fn turn_start_not_observed(
    p: &ProjectInputs,
    w: &WorktreeInputs,
    now_ms: i64,
) -> Option<Finding> {
    let (delivery, at) = w.turns.as_ref()?.no_activity?;
    if now_ms - at >= STALL_MEMORY_MS {
        return None;
    }
    let pane = w
        .key
        .as_ref()
        .and_then(|k| k.pane_id.clone())
        .or_else(|| w.view.as_ref().and_then(|v| v.pane_id.clone()));
    let stall_s = crate::engine::turns::STALL_NO_ACTIVITY.as_secs();
    let hint = match &pane {
        Some(id) => format!(" (try: herdr agent explain {id})"),
        None => String::new(),
    };
    let mut f = Finding::new(
        &p.name,
        &w.name,
        TURN_START_NOT_OBSERVED,
        Severity::Warn,
        format!(
            "@{}: a send showed no herdr activity for {stall_s} s; herdr may not recognise \
             this UI{hint}",
            w.name
        ),
    );
    f.detail = json!({ "delivery_id": delivery, "at": at });
    Some(f)
}

/// The worktree's hooks are not (fully) in place.
pub fn hooks_missing(p: &ProjectInputs, w: &WorktreeInputs) -> Option<Finding> {
    let facts = w.facts.as_ref()?;
    let mut parts: Vec<&str> = Vec::new();
    if facts.bin.is_none() {
        parts.push("no keitora CLI resolves for its hooks");
    }
    if facts.codex.as_ref().is_some_and(|c| !c.hooks_json_ok) {
        parts.push(".codex/hooks.json is missing Keitora's hooks");
    }
    if facts.trust_bypass_last_launch() == Some(false) {
        parts.push("the last launch ran without the hook trust bypass");
    }
    let per_launch = facts.per_launch_settings();
    if w.provider == CliKind::Claude
        && facts.claude.as_ref().is_some_and(|c| !c.hooks_installed)
        && !matches!(per_launch, ClaudeSettings::Applied(_))
    {
        parts.push("Claude's Keitora hooks are not installed");
    }
    if !facts.hook_version_ok() {
        parts.push("the last launch ran older hooks");
    }
    if parts.is_empty() {
        return None;
    }
    let severity = Severity::escalate(w.driven);
    let mut f = Finding::new(
        &p.name,
        &w.name,
        HOOKS_MISSING,
        severity,
        format!(
            "@{}: {}; the permission gate and Stop capture won't run",
            w.name,
            parts.join("; ")
        ),
    );
    let skipped = match per_launch {
        ClaudeSettings::Skipped(why) => Some(why),
        _ => None,
    };
    f.detail = json!({
        "last_hook_error": facts.last_hook_error.as_ref().map(|l| json!({
            "at": l.unix_s * 1000,
            "message": l.message,
        })),
        "claude_settings_skipped": skipped,
    });
    f.remedy = Some(Remedy::ReinstallHooks {
        project: p.name.clone(),
        worktree: w.name.clone(),
    });
    Some(f)
}

/// Codex's own config does not trust the project (report only: Keitora never writes
/// the user's global Codex config).
pub fn codex_project_untrusted(p: &ProjectInputs, w: &WorktreeInputs) -> Option<Finding> {
    if w.provider != CliKind::Codex {
        return None;
    }
    let facts = w.facts.as_ref()?;
    let how = match &facts.codex.as_ref()?.project_trust {
        ProjectTrust::NotListed => "not listed".to_string(),
        ProjectTrust::Untrusted(v) if v.is_empty() => "trust_level unset".to_string(),
        ProjectTrust::Untrusted(v) => format!("trust_level {v}"),
        ProjectTrust::Trusted | ProjectTrust::Unreadable(_) | ProjectTrust::NotApplicable => {
            return None
        }
    };
    let severity = if facts.trust_bypass_last_launch() == Some(true) {
        Severity::Info
    } else {
        Severity::Warn
    };
    let config = w.codex_config.as_deref().unwrap_or("~/.codex/config.toml");
    Some(Finding::new(
        &p.name,
        &w.name,
        CODEX_PROJECT_UNTRUSTED,
        severity,
        format!(
            "@{}: Codex does not list this project as trusted in config.toml ({how}). \
             Keitora's own launches bypass the hook trust prompt, but a hand-launched codex or \
             herdr's own session-id hook may not run its hooks here; trust it in Codex ({config})",
            w.name
        ),
    ))
}

/// agy's global hook entry is not wired for this worktree.
pub fn agy_plumbing(p: &ProjectInputs, w: &WorktreeInputs) -> Option<Finding> {
    if w.provider != CliKind::Antigravity {
        return None;
    }
    let facts = w.facts.as_ref()?;
    let agy = facts.agy.as_ref()?;
    let mut parts: Vec<&str> = Vec::new();
    if !agy.key_present {
        parts.push("hooks.json has no keitora key");
    } else if !agy.path_ok {
        parts.push("the hook command path is missing");
    }
    if facts.marker_env_ok() == Some(false) {
        parts.push("the last launch did not mark the pane (KEITORA_PANE)");
    }
    if agy.translocated {
        parts.push("the app runs from a translocated or build folder and offers no CLI");
    }
    if parts.is_empty() {
        return None;
    }
    let mut f = Finding::new(
        &p.name,
        &w.name,
        AGY_PLUMBING,
        Severity::Warn,
        format!(
            "@{}: agy hooks are not wired ({}); agy sessions show only herdr's view",
            w.name,
            parts.join("; ")
        ),
    );
    if !agy.translocated {
        f.remedy = Some(Remedy::SyncAgyHooks {
            project: p.name.clone(),
            worktree: w.name.clone(),
        });
    }
    Some(f)
}

/// herdr refused to start the worktree's agent on its last launch (Claude's
/// folder-trust prompt in a new worktree does this: `agent_not_ready`).
pub fn agent_start_refused(p: &ProjectInputs, w: &WorktreeInputs) -> Option<Finding> {
    let launch = w.facts.as_ref()?.last_launch.as_ref()?;
    let code = launch.start_refused.as_deref()?;
    // The agent came up after all (the human answered the prompt).
    if w.view.as_ref().is_some_and(|v| v.agent == "present")
        && w.key
            .as_ref()
            .and_then(|k| k.present_since_ms)
            .is_some_and(|t| t >= launch.at_ms)
    {
        return None;
    }
    let hint = if launch.cli == CliKind::Claude && code == "agent_not_ready" {
        "Claude may be waiting on its folder-trust prompt in that pane; open it and trust the folder"
    } else {
        "open its pane to see why"
    };
    let mut f = Finding::new(
        &p.name,
        &w.name,
        AGENT_START_REFUSED,
        Severity::Warn,
        format!(
            "@{}: herdr could not start its {} agent ({code}); {hint}",
            w.name,
            provider_label(launch.cli)
        ),
    );
    f.detail = json!({ "code": code, "launched_at": launch.at_ms });
    Some(f)
}

/// The worktree's folder name differs from its configured name.
pub fn worktree_identity(p: &ProjectInputs, w: &WorktreeInputs) -> Option<Finding> {
    let folder = w.folder.as_deref()?;
    if folder.to_lowercase() == w.name.to_lowercase() {
        return None;
    }
    Some(Finding::new(
        &p.name,
        &w.name,
        WORKTREE_IDENTITY,
        Severity::Info,
        format!(
            "@{}: its folder is named {folder}, not {}; CLI verbs run there name themselves \
             after the folder",
            w.name, w.name
        ),
    ))
}

/// A pending request sits with no pass examining it.
pub fn pipeline_stalled(p: &ProjectInputs, now_ms: i64) -> Option<Finding> {
    let pp = &p.pipeline;
    let o = pp.oldest_pending?;
    let age = now_ms - o.created_at_ms;
    if age < PIPELINE_STALL_MS {
        return None;
    }
    let pass_old = pp
        .last_pass_ms
        .is_none_or(|t| now_ms - t > PIPELINE_STALL_MS);
    if o.seen && !pass_old {
        return None;
    }
    let severity = Severity::escalate(age >= PIPELINE_ERROR_MS);
    let since = pp.last_pass_ms.map_or("start".to_string(), clock_text);
    let mut f = Finding::new(
        &p.name,
        "",
        PIPELINE_STALLED,
        severity,
        format!(
            "{}: request {} pending {} with no pass since {since}",
            p.name,
            o.id,
            age_text(age / 1000)
        ),
    );
    f.detail = json!({ "request": o.id, "seen": o.seen, "last_pass_at": pp.last_pass_ms });
    f.remedy = Some(Remedy::RequestPass {
        project: p.name.clone(),
    });
    Some(f)
}

/// The supervisor restarted a watcher that ended or hung.
pub fn watcher_restarted(p: &ProjectInputs) -> Option<Finding> {
    let mine: Vec<&RestartNote> = p
        .pipeline
        .restarts
        .iter()
        .filter(|r| !matches!(r.why, RestartWhy::Silent { .. }))
        .collect();
    let latest = mine.iter().max_by_key(|r| r.at_ms)?;
    let n = mine.len();
    let what = match &latest.why {
        RestartWhy::Exited(reason) => format!("exited ({reason})"),
        _ => "stalled".to_string(),
    };
    let severity = Severity::escalate(n >= RESTARTS_ERROR_AT);
    let mut f = Finding::new(
        &p.name,
        "",
        WATCHER_RESTARTED,
        severity,
        format!(
            "{}: the {} watcher {what} and was restarted ({n}x in {}m)",
            p.name,
            latest.watcher.label(),
            RESTART_WINDOW_MS / MIN
        ),
    );
    f.note = Some(RESTARTED_WATCHER);
    Some(f)
}

/// The supervisor restarted a watcher that missed a request.
pub fn watcher_silent(p: &ProjectInputs) -> Option<Finding> {
    let (request, age_s) = p
        .pipeline
        .restarts
        .iter()
        .filter_map(|r| match r.why {
            RestartWhy::Silent { request, age_s } => Some((r.at_ms, request, age_s)),
            _ => None,
        })
        .max_by_key(|(at, ..)| *at)
        .map(|(_, request, age_s)| (request, age_s))?;
    let mut f = Finding::new(
        &p.name,
        "",
        WATCHER_SILENT,
        Severity::Warn,
        format!(
            "{}: the watcher missed request {request} (no event for {}); restarted, the sweep \
             claimed it",
            p.name,
            age_text(age_s)
        ),
    );
    f.note = Some(RESTARTED_WATCHER);
    Some(f)
}

/// The engine has no live link to herdr.
pub fn herdr_link(inputs: &HealthInputs, now_ms: i64) -> Option<Finding> {
    let l = inputs.link?;
    if l.link == HerdrLink::Up {
        return None;
    }
    let down = now_ms - l.since_ms;
    if down < LINK_WARN_MS {
        return None;
    }
    let severity = Severity::escalate(down >= LINK_ERROR_MS);
    let snapshot = match l.last_snapshot_ms {
        Some(t) => format!("last snapshot {} ago", age_text((now_ms - t) / 1000)),
        None => "no snapshot yet".to_string(),
    };
    Some(Finding::new(
        "",
        "",
        HERDR_LINK,
        severity,
        format!(
            "herdr unavailable for {} ({}); status is stale ({snapshot})",
            age_text(down / 1000),
            l.link.as_str()
        ),
    ))
}

/// herdr has reported the agent's status as `unknown` for a while.
pub fn herdr_unknown(p: &ProjectInputs, w: &WorktreeInputs, now_ms: i64) -> Option<Finding> {
    let v = w.view.as_ref()?;
    if v.agent != "present" || v.herdr_status.as_deref() != Some("unknown") {
        return None;
    }
    let since = w.key.as_ref()?.unknown_since_ms?;
    if now_ms - since <= UNKNOWN_AFTER_MS {
        return None;
    }
    Some(Finding::new(
        &p.name,
        &w.name,
        HERDR_UNKNOWN,
        Severity::Warn,
        format!(
            "@{}: herdr reports the agent status as unknown for {}; Keitora keeps the last \
             known state",
            w.name,
            age_text((now_ms - since) / 1000)
        ),
    ))
}

/// Another instance owns `status.db`.
pub fn status_writer(inputs: &HealthInputs) -> Option<Finding> {
    let LeaseState::Standby { pid, version, .. } = inputs.lease.as_ref()? else {
        return None;
    };
    Some(Finding::new(
        "",
        "",
        STATUS_WRITER,
        Severity::Warn,
        format!(
            "another Keitora instance (pid {pid}, v{version}) owns status.db; this instance is \
             read-only"
        ),
    ))
}

/// The `keitora` on PATH is not the CLI the app ships: a stale managed copy, or
/// another version. Refreshed only when it is the managed copy and this platform
/// refreshes it.
pub fn version_skew(inputs: &HealthInputs) -> Option<Finding> {
    let cli = inputs.path_cli.as_ref()?;
    if cli.dev_build {
        return None;
    }
    if cli.managed_stale {
        let mut f = Finding::new(
            "",
            "",
            VERSION_SKEW,
            Severity::Warn,
            format!(
                "the keitora on PATH is an outdated copy of the app's CLI ({})",
                cli.path
            ),
        );
        f.detail = json!({ "path": cli.path });
        f.remedy = cli.refreshable.then_some(Remedy::RefreshCli);
        return Some(f);
    }
    use crate::deps::floor::parse_version;
    let (on_path, app) = (
        parse_version(&cli.version)?,
        parse_version(cli.bundled_version.as_deref()?)?,
    );
    let (severity, how) = match on_path.cmp(&app) {
        std::cmp::Ordering::Equal => return None,
        std::cmp::Ordering::Less => (Severity::Warn, "older than"),
        std::cmp::Ordering::Greater => (Severity::Info, "newer than"),
    };
    let mut f = Finding::new(
        "",
        "",
        VERSION_SKEW,
        severity,
        format!("the keitora on PATH is {how} the app's ({on_path} vs {app})"),
    );
    f.detail = json!({ "path": cli.path });
    // Only the managed copy is refreshed; another keitora on PATH is the user's.
    if severity == Severity::Warn && cli.refreshable {
        f.remedy = Some(Remedy::RefreshCli);
    }
    Some(f)
}

/// Recent session rows of worktrees the project doesn't configure.
pub fn unconfigured_sessions(p: &ProjectInputs, now_ms: i64) -> Option<Finding> {
    let mut names: Vec<&str> = p
        .hook_rows
        .iter()
        .filter(|(wt, _)| !p.worktrees.iter().any(|w| &w.name == wt))
        .filter(|(_, at)| at.is_some_and(|t| now_ms - t < UNCONFIGURED_WINDOW_MS))
        .map(|(wt, _)| wt.as_str())
        .collect();
    if names.is_empty() {
        return None;
    }
    names.sort_unstable();
    names.dedup();
    let mut listed = names
        .iter()
        .take(UNCONFIGURED_LISTED)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > UNCONFIGURED_LISTED {
        listed.push_str(&format!(", +{} more", names.len() - UNCONFIGURED_LISTED));
    }
    let mut f = Finding::new(
        &p.name,
        "",
        UNCONFIGURED_SESSIONS,
        Severity::Info,
        format!(
            "{}: session rows exist for unconfigured worktrees: {listed}",
            p.name
        ),
    );
    f.detail = json!({ "worktrees": names });
    Some(f)
}
