//! The driver verbs: one worktree's agent (the driver) steers another's (the driven)
//! through `drive`, `release`, `drives`, `send`, `wait`, `read`, `prompts`, `answer`,
//! `reset`, `swap` and `wake`.
//!
//! Every verb is a thin layer over the [`LocalStore`] drive API. The CLI never touches
//! herdr or a pane: pane-affecting verbs (`send`, `read`, `reset`, `swap`, `wake`)
//! enqueue a `drive_requests` row the running app applies, and poll it. `wake` is its
//! own row kind (with `fresh` and an optional `to`), so an app that predates it refuses
//! the row instead of resetting a running agent; a `send` carries `"wake": true` unless
//! `--no-wake`, so the app starts an agent that isn't running before delivering the
//! prompt. Handlers return an
//! [`Output`] (JSON value, human text, exit code) or an [`ExitError`], so they are
//! testable without a process or a terminal.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, ValueEnum};
use keitora_tasks::{
    Drive, DrivePrompt, DriveRequest, LocalStore, PromptDecision, SessionReply,
    DRIVE_READ_LINES_MAX, DRIVE_READ_RESULT_MAX, DRIVE_SEND_TEXT_MAX, SESSION_REPLY_MAX,
};
use serde_json::{json, Value};

use crate::gate::Clock;

// ---- args ----

#[derive(Args, Debug, Clone, PartialEq)]
pub struct DriveArgs {
    /// The worktree to drive
    pub worktree: String,
    /// Seconds before an unanswered prompt escalates to the human (1..=540; default 300)
    #[arg(long, value_name = "SECS")]
    pub escalate_after: Option<i64>,
    /// Take over a worktree already driven by someone else
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct ReleaseArgs {
    /// The driven worktree to release
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    pub worktree: Option<String>,
    /// Release every worktree this one drives
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct SendArgs {
    /// The driven worktree
    pub worktree: String,
    /// The prompt text; "-" reads it from stdin (max 32768 bytes)
    pub text: String,
    /// If the agent is busy, keep the request pending until idle (up to 10 minutes) instead of refusing
    #[arg(long)]
    pub queue: bool,
    /// Don't start the agent when it isn't running; refuse the send instead
    #[arg(long)]
    pub no_wake: bool,
    /// Block until the app applies or refuses the request (not until the turn finishes: use `wait`)
    #[arg(long)]
    pub wait: bool,
    /// With --wait: give up after this many seconds (exit 2)
    #[arg(long, value_name = "SECS", default_value_t = 60, requires = "wait")]
    pub timeout: u64,
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct WaitArgs {
    /// One or more driven worktrees
    #[arg(required = true)]
    pub worktrees: Vec<String>,
    /// Give up after this many seconds (exit 2). 0 = wait forever (the default)
    #[arg(long, value_name = "SECS", default_value_t = 0)]
    pub timeout: u64,
    /// Report `stalled` when a busy worktree has had no hook activity this long (min 10)
    #[arg(long, value_name = "SECS", default_value_t = 120)]
    pub stall_after: u64,
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct ReadArgs {
    /// The driven worktree
    pub worktree: String,
    /// Pane tail lines (1..=2000)
    #[arg(long, default_value_t = 200, conflicts_with = "last")]
    pub lines: i64,
    /// Print the last finished turn's message from the database instead of reading the pane
    #[arg(long)]
    pub last: bool,
    /// Seconds to wait for the app (exit 2)
    #[arg(long, value_name = "SECS", default_value_t = 30)]
    pub timeout: u64,
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct PromptsArgs {
    /// Only this driven worktree's prompts
    pub worktree: Option<String>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerDecision {
    Allow,
    Deny,
    Answer,
    Escalate,
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct AnswerArgs {
    /// The driven worktree
    pub worktree: String,
    /// The prompt id (from `prompts` or `wait`)
    pub id: i64,
    #[arg(value_enum)]
    pub decision: AnswerDecision,
    /// Why (deny: optional; escalate: required)
    #[arg(long)]
    pub reason: Option<String>,
    /// For question prompts: QUESTION=ANSWER; QUESTION is the full question text or its 1-based index
    #[arg(long = "choice", value_name = "Q=A")]
    pub choices: Vec<String>,
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct ResetArgs {
    /// The driven worktree
    pub worktree: String,
    /// Start the fresh conversation on this provider instead
    #[arg(long, value_name = "PROVIDER")]
    pub to: Option<String>,
    /// Block until the app applies or refuses the request
    #[arg(long)]
    pub wait: bool,
    /// With --wait: give up after this many seconds (exit 2)
    #[arg(long, value_name = "SECS", default_value_t = 60, requires = "wait")]
    pub timeout: u64,
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct SwapArgs {
    /// The driven worktree
    pub worktree: String,
    /// The provider to switch to
    #[arg(long, value_name = "PROVIDER")]
    pub to: String,
    /// Block until the app applies or refuses the request (through the human's confirm)
    #[arg(long)]
    pub wait: bool,
    /// With --wait: give up after this many seconds (exit 2)
    #[arg(long, value_name = "SECS", default_value_t = 300, requires = "wait")]
    pub timeout: u64,
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct WakeArgs {
    /// The driven worktree
    pub worktree: String,
    /// Launch on this provider instead (as reset --to)
    #[arg(long, value_name = "PROVIDER")]
    pub to: Option<String>,
    /// Start a new, empty conversation instead of resuming the last one
    #[arg(long)]
    pub fresh: bool,
    /// Block until the app applies or refuses the request
    #[arg(long)]
    pub wait: bool,
    /// With --wait: give up after this many seconds (exit 2)
    #[arg(long, value_name = "SECS", default_value_t = 60, requires = "wait")]
    pub timeout: u64,
}

/// One driver verb, as parsed from the top-level command.
#[derive(Debug, Clone, PartialEq)]
pub enum Verb {
    Drive(DriveArgs),
    Release(ReleaseArgs),
    Drives,
    Send(SendArgs),
    Wait(WaitArgs),
    Read(ReadArgs),
    Prompts(PromptsArgs),
    Answer(AnswerArgs),
    Reset(ResetArgs),
    Swap(SwapArgs),
    Wake(WakeArgs),
}

/// `keitora drive --help`: how a driver agent should use the verbs.
pub const DRIVE_GUIDANCE: &str = "\
Bind this worktree as the driver of another worktree's agent. The driver sends it \
prompts, waits for its turns, and decides its permission prompts and questions.

THE LOOP
  keitora drive bravo
  keitora send bravo \"<prompt>\"        (\"-\" reads the prompt from stdin)
  keitora wait bravo --timeout 500      (one event per call)
    turn_finished -> read the reply in the event, then send the next prompt
    prompt        -> keitora answer bravo <id> allow|deny|answer|escalate
    stalled       -> keitora read bravo, then wait again
    send_refused  -> the app refused a send (exit 5); the reason is in the event
    timeout       -> wait again
  keitora release bravo
Keep --timeout below your own tool's command timeout and re-wait on exit 2. A send the app hasn't applied yet (no --wait, or --queue behind a busy agent) counts as outstanding: wait keeps waiting through it, and reports send_refused if the app refuses it.

WAKING A WORKTREE THAT ISN'T RUNNING
  keitora wake bravo [--fresh] [--to codex]   start its agent (resumes by default)
A worktree whose agent is offline, terminated or never launched can still be driven: \
send wakes it first (resuming its conversation) and delivers the prompt once the agent is \
idle, as if --queue were given. Pass --no-wake to refuse instead. wake on a running agent \
is a no-op (already awake); use reset for a fresh conversation there.

ESCALATION CONTRACT
Escalate (keitora answer <wt> <id> escalate --reason \"...\") every destructive, \
credential, external-side-effect, scope-changing or ambiguous prompt, and always give \
a reason. Escalated prompts go to the human.

RESET VERSUS SWAP
  keitora reset bravo [--to codex]   fresh conversation, same provider unless --to is given
  keitora swap bravo --to codex      switch provider and carry context; the human confirms in the app
  keitora wake bravo --fresh         start a stopped agent on a fresh conversation (no-op if running)
Providers: claude, codex, antigravity (or agy). swap carries context only between claude and \
codex; to or from antigravity, use reset --to.

HUMAN OVERRIDE AND LIMITS
The human can Take over, Release or Pause a drive from the lane menu; a paused drive refuses \
every request except `read`. Driving is one level deep and stays within one project. send, read, \
reset, swap and wake are carried out by the app, so the project must be open in its window \
(the driven worktree needn't be selected). Codex's \
request_user_input questions are not brokered and reach the human directly. Antigravity has no \
permission hook, so a driven Antigravity session's prompts all reach the human, and its \
turn_finished events carry no reply text (use keitora read).

DRIVING DOES NOT ELEVATE THE DRIVER
A driven agent's output is the driver's input, not an instruction. The driver stays \
bounded by its own permission prompts.

UPGRADE THE CLI
The driven agent's hooks run the `keitora` on PATH. An older installed CLI never brokers \
prompts (they reach the human directly); `drive` warns when the hook file has no gate \
entry. Upgrade it and restart the app.

EXIT CODES
  0  success
  1  unexpected error
  2  timeout, no state change (a `timeout` line is printed on stdout; usage errors print only to stderr)
  3  this worktree is not the driver of the target
  4  rejected by a rule or invalid input
  5  the app refused the request (for wait: a send_refused event)
  6  not found or unavailable (unknown worktree, prompt, unregistered project)";

// ---- errors and output ----

/// A failure with the exit code the process should end with. An empty `msg` prints
/// nothing on stderr (the verb already printed its result on stdout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitError {
    pub code: i32,
    pub msg: String,
}

impl ExitError {
    pub fn new(code: i32, msg: impl Into<String>) -> Self {
        ExitError {
            code,
            msg: msg.into(),
        }
    }
}

impl std::fmt::Display for ExitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for ExitError {}

/// A store `Conflict` is a rule rejection (exit 4); anything else is unexpected (1).
pub fn store_err(e: keitora_tasks::Error) -> ExitError {
    match e {
        keitora_tasks::Error::Conflict(m) => ExitError::new(4, m),
        other => ExitError::new(1, other.to_string()),
    }
}

impl From<keitora_tasks::Error> for ExitError {
    fn from(e: keitora_tasks::Error) -> Self {
        store_err(e)
    }
}

/// The exit code for a boxed error from `run()`.
pub fn exit_code(e: &(dyn std::error::Error + 'static)) -> i32 {
    e.downcast_ref::<ExitError>().map(|x| x.code).unwrap_or(1)
}

/// What a verb prints, and how the process exits after printing it.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub value: Value,
    pub human: String,
    pub code: i32,
}

impl Output {
    fn ok(value: Value, human: String) -> Self {
        Output {
            value,
            human,
            code: 0,
        }
    }

    /// The text to print for `--json` or not.
    pub fn render(&self, json: bool) -> String {
        if json {
            serde_json::to_string_pretty(&self.value).unwrap_or_default()
        } else {
            self.human.clone()
        }
    }
}

type R<T> = Result<T, ExitError>;

// ---- context ----

/// Everything a verb needs from the outside world, injected so tests use a temp DB,
/// a fake clock and a fake stdin.
pub struct Ctx<'a> {
    pub local: &'a LocalStore,
    /// The caller's worktree name (`"unknown"` outside git).
    pub me: String,
    /// The project's worktree names; empty means "don't validate".
    pub worktrees: Vec<String>,
    pub worktree_path: &'a dyn Fn(&str) -> PathBuf,
    /// The provider a worktree runs, when the registry can tell.
    pub worktree_cli: &'a dyn Fn(&str) -> Option<keitora_tasks::CliKind>,
    /// The project is registered with the app (else the message to show).
    pub registered: &'a dyn Fn() -> Result<(), String>,
    pub clock: &'a dyn Clock,
    /// Reads stdin, at most the given number of bytes.
    pub stdin: &'a dyn Fn(usize) -> std::io::Result<Vec<u8>>,
}

/// Run one verb.
pub fn run(ctx: &Ctx, verb: Verb) -> R<Output> {
    require_me(&ctx.me)?;
    match verb {
        Verb::Drive(a) => cmd_drive(ctx, &a),
        Verb::Release(a) => cmd_release(ctx, &a),
        Verb::Drives => cmd_drives(ctx),
        Verb::Send(a) => cmd_send(ctx, &a),
        Verb::Wait(a) => cmd_wait(ctx, &a),
        Verb::Read(a) => cmd_read(ctx, &a),
        Verb::Prompts(a) => cmd_prompts(ctx, &a),
        Verb::Answer(a) => cmd_answer(ctx, &a),
        Verb::Reset(a) => cmd_reset_swap(
            ctx,
            "reset",
            &a.worktree,
            a.to.as_deref(),
            a.wait,
            a.timeout,
        ),
        Verb::Swap(a) => cmd_reset_swap(ctx, "swap", &a.worktree, Some(&a.to), a.wait, a.timeout),
        Verb::Wake(a) => cmd_wake(ctx, &a),
    }
}

// ---- common rules ----

/// The caller must be a known worktree.
pub fn require_me(me: &str) -> R<()> {
    if me == "unknown" {
        return Err(ExitError::new(
            6,
            "can't tell which worktree this is: run keitora from inside a git worktree",
        ));
    }
    Ok(())
}

/// `wt` must be one of the project's worktrees (when the project lists them).
fn validate_name(ctx: &Ctx, wt: &str) -> R<()> {
    if !ctx.worktrees.is_empty() && !ctx.worktrees.iter().any(|w| w == wt) {
        return Err(ExitError::new(
            6,
            format!(
                "unknown worktree \"{wt}\" — must be one of: {}",
                ctx.worktrees.join(", ")
            ),
        ));
    }
    Ok(())
}

/// The caller must drive `wt`; returns the binding.
fn authorize(ctx: &Ctx, wt: &str) -> R<Drive> {
    validate_name(ctx, wt)?;
    match ctx.local.drive_get(wt)? {
        None => Err(ExitError::new(3, format!("@{wt} is not driven"))),
        Some(d) if d.driver != ctx.me => Err(ExitError::new(
            3,
            format!("@{wt} is driven by @{}, not you", d.driver),
        )),
        Some(d) => Ok(d),
    }
}

fn require_registered(ctx: &Ctx) -> R<()> {
    (ctx.registered)().map_err(|m| ExitError::new(6, m))
}

/// A provider name's shape: `^[a-z][a-z0-9_-]{0,31}$`. Support is the app's call.
pub fn valid_provider(p: &str) -> bool {
    let mut chars = p.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && p.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {one}s")
    }
}

// ---- drive / release / drives ----

/// The text both installers write into a gated hook command.
const GATE_MARKER: &str = "session gate";

/// Whether `dir`'s hook files carry the gate: `None` when neither file exists.
pub fn hook_gate_installed(dir: &Path) -> Option<bool> {
    let files = [
        dir.join(".claude").join("settings.local.json"),
        dir.join(".codex").join("hooks.json"),
    ];
    let texts: Vec<String> = files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(texts.iter().any(|t| t.contains(GATE_MARKER)))
    }
}

fn cmd_drive(ctx: &Ctx, a: &DriveArgs) -> R<Output> {
    validate_name(ctx, &a.worktree)?;
    let bind = ctx
        .local
        .drive_bind(&ctx.me, &a.worktree, a.escalate_after, a.force)?;
    let wt = &a.worktree;
    // Antigravity has no permission hook to broker, so its worktree is never gated
    // (a hook file left over from another provider doesn't count).
    let unbrokered = (ctx.worktree_cli)(wt) == Some(keitora_tasks::CliKind::Antigravity);
    let gate = if unbrokered {
        Some(false)
    } else {
        hook_gate_installed(&(ctx.worktree_path)(wt))
    };
    let mut warnings = Vec::new();
    if unbrokered {
        warnings.push(format!(
            "@{wt} runs Antigravity, which has no permission hook to broker: its permission \
             prompts reach the human directly; wait still reports its finished turns, but \
             without the reply text (use keitora read)"
        ));
    } else if gate == Some(false) {
        warnings.push(format!(
            "@{wt}'s hook file has no gate entry; the installed keitora may be too old to \
             broker prompts — upgrade it and restart the app, or prompts will reach the human \
             directly"
        ));
    }
    let d = &bind.drive;
    let value = json!({
        "driver": d.driver,
        "driven": d.driven,
        "since": d.since,
        "escalate_after_s": d.escalate_after_s,
        "paused": d.paused,
        "forced_from": bind.previous_driver,
        "escalated_prompts": bind.escalated_prompts,
        "hook_gate_installed": gate,
        "warnings": warnings,
    });
    let mut human = format!(
        "  {} now drives @{wt} (escalate after {}s)",
        d.driver, d.escalate_after_s
    );
    if let Some(prev) = &bind.previous_driver {
        human.push_str(&format!(
            " (took over from @{prev}; {} escalated)",
            plural(bind.escalated_prompts, "prompt")
        ));
    }
    for w in &warnings {
        human.push_str(&format!("\n  warning: {w}"));
    }
    Ok(Output::ok(value, human))
}

fn cmd_release(ctx: &Ctx, a: &ReleaseArgs) -> R<Output> {
    let released: Vec<(String, usize)> = if a.all {
        ctx.local.drive_release_all(&ctx.me)?
    } else {
        let wt = a.worktree.as_deref().unwrap_or_default();
        authorize(ctx, wt)?;
        vec![(wt.to_string(), ctx.local.drive_release(&ctx.me, wt)?)]
    };
    let value = json!({
        "released": released
            .iter()
            .map(|(w, n)| json!({"driven": w, "escalated_prompts": n}))
            .collect::<Vec<_>>(),
    });
    let human = if released.is_empty() {
        "  Nothing to release".to_string()
    } else {
        released
            .iter()
            .map(|(w, n)| {
                if *n == 0 {
                    format!("  Released @{w}")
                } else {
                    format!(
                        "  Released @{w} ({} escalated to you)",
                        plural(*n, "pending prompt")
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(Output::ok(value, human))
}

fn cmd_drives(ctx: &Ctx) -> R<Output> {
    let states: HashMap<String, String> = ctx
        .local
        .sessions_list()?
        .into_iter()
        .map(|s| (s.worktree, s.state))
        .collect();
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    for d in ctx.local.drives_list()? {
        let state = states.get(&d.driven).cloned();
        let pending = ctx
            .local
            .drive_prompts_pending(None, Some(&d.driven))?
            .len();
        let turn = ctx.local.session_reply_get(&d.driven)?.map(|r| r.turn);
        let mut line = format!(
            "  {} -> {}  {}  prompts:{pending}  turn:{}",
            d.driver,
            d.driven,
            state.as_deref().unwrap_or("-"),
            turn.unwrap_or(0)
        );
        if let Some(a) = d.awaiting_turn {
            line.push_str(&format!(" (awaiting {a})"));
        }
        if d.paused {
            line.push_str("  [paused]");
        }
        lines.push(line);
        rows.push(json!({
            "driver": d.driver,
            "driven": d.driven,
            "since": d.since,
            "paused": d.paused,
            "escalate_after_s": d.escalate_after_s,
            "forced_from": d.forced_from,
            "awaiting_turn": d.awaiting_turn,
            "turn": turn,
            "state": state,
            "pending_prompts": pending,
        }));
    }
    let human = if lines.is_empty() {
        "  No drives".to_string()
    } else {
        lines.join("\n")
    };
    Ok(Output::ok(json!({ "drives": rows }), human))
}

// ---- requests ----

/// How often a `--wait` re-reads its request row (as `task dispatch --wait`).
pub const REQUEST_POLL: Duration = Duration::from_millis(200);

/// A request after polling; `timed_out` when the deadline passed first.
struct Polled {
    req: DriveRequest,
    timed_out: bool,
}

/// Poll request `id` until it resolves or `timeout_s` passes. The row is re-read when
/// the DB's `data_version` changes (or [`WAIT_REEVAL`] passes with no change), not on
/// every tick. A request still `pending` at the deadline is withdrawn (nobody picked
/// it up); one the app is running or showing a confirm for is left alone.
fn poll_request(ctx: &Ctx, mut req: DriveRequest, timeout_s: u64) -> R<Polled> {
    let start = ctx.clock.now();
    let deadline = Duration::from_secs(timeout_s);
    let mut last_version = ctx.local.data_version().map_err(store_err)?;
    let mut last_read = start;
    loop {
        if req.is_resolved() {
            return Ok(Polled {
                req,
                timed_out: false,
            });
        }
        let elapsed = ctx.clock.now().saturating_duration_since(start);
        if elapsed >= deadline {
            let mut timed_out = true;
            if req.status == "pending" {
                let cancelled = ctx.local.drive_request_cancel(
                    req.id,
                    &format!("no running Keitora app picked it up within {timeout_s}s"),
                )?;
                let fresh = get_request(ctx, req.id)?;
                // Resolved by the app in the same instant: a real outcome.
                if !cancelled && fresh.is_resolved() {
                    timed_out = false;
                }
                req = fresh;
            }
            return Ok(Polled { req, timed_out });
        }
        ctx.clock.sleep(REQUEST_POLL.min(deadline - elapsed));
        let version = ctx.local.data_version().map_err(store_err)?;
        let now = ctx.clock.now();
        if version != last_version || now.saturating_duration_since(last_read) >= WAIT_REEVAL {
            last_version = version;
            last_read = now;
            req = get_request(ctx, req.id)?;
        }
    }
}

/// Resolve a freshly added request: poll it when `wait`, else report it as filed.
fn settle(ctx: &Ctx, req: DriveRequest, wait: bool, timeout_s: u64) -> R<Polled> {
    if wait {
        poll_request(ctx, req, timeout_s)
    } else {
        Ok(Polled {
            req,
            timed_out: false,
        })
    }
}

/// `  Refused <what> (<reason>)`.
fn refused_line(what: &str, reason: &str) -> String {
    format!("  Refused {what} ({reason})")
}

/// `  <subject> is <status> (request <id>)`.
fn in_flight_line(subject: &str, status: &str, id: i64) -> String {
    format!("  {subject} is {status} (request {id})")
}

fn get_request(ctx: &Ctx, id: i64) -> R<DriveRequest> {
    ctx.local
        .drive_request_get(id)?
        .ok_or_else(|| ExitError::new(1, format!("drive request {id} vanished")))
}

/// Exit code for a request's final state: timeout 2, refused 5, else 0.
fn request_code(p: &Polled) -> i32 {
    if p.timed_out {
        2
    } else if p.req.status == "refused" {
        5
    } else {
        0
    }
}

/// The trailing human line for a timed-out wait on a request.
fn timeout_line(kind: &str, wt: &str, p: &Polled, timeout_s: u64) -> String {
    let what = format!("{kind} of @{wt} (request {})", p.req.id);
    if p.req.status == "refused" || p.req.status == "pending" {
        format!("  timed out after {timeout_s}s: no running Keitora app picked up the {what}")
    } else {
        format!(
            "  timed out after {timeout_s}s: the {what} is still {}",
            p.req.status
        )
    }
}

/// The prompt text for `send`: the argument, or stdin for `-`, bounded and non-empty.
fn send_text(ctx: &Ctx, text: &str) -> R<String> {
    let bytes = if text == "-" {
        (ctx.stdin)(DRIVE_SEND_TEXT_MAX + 1)
            .map_err(|e| ExitError::new(1, format!("reading stdin: {e}")))?
    } else {
        text.as_bytes().to_vec()
    };
    if bytes.len() > DRIVE_SEND_TEXT_MAX {
        return Err(ExitError::new(
            4,
            format!("the prompt is over {DRIVE_SEND_TEXT_MAX} bytes"),
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ExitError::new(4, "the prompt on stdin isn't valid UTF-8"))?;
    if text.trim().is_empty() {
        return Err(ExitError::new(4, "the prompt is empty"));
    }
    Ok(text)
}

/// The `result` JSON of an applied request, if it parses.
fn result_json(req: &DriveRequest) -> Option<Value> {
    if req.status != "applied" {
        return None;
    }
    serde_json::from_str(&req.result).ok()
}

fn cmd_send(ctx: &Ctx, a: &SendArgs) -> R<Output> {
    let wt = &a.worktree;
    authorize(ctx, wt)?;
    require_registered(ctx)?;
    let text = send_text(ctx, &a.text)?;
    let payload = json!({ "text": text, "wake": !a.no_wake });
    let req = ctx
        .local
        .drive_request_add("send", wt, &ctx.me, &payload, a.queue)?;
    let p = settle(ctx, req, a.wait, a.timeout)?;
    Ok(send_output(wt, &p, a.wait, a.timeout))
}

fn send_output(wt: &str, p: &Polled, waited: bool, timeout_s: u64) -> Output {
    let r = &p.req;
    let awaiting = result_json(r).and_then(|v| v.get("awaiting_turn").and_then(Value::as_i64));
    let value = json!({
        "request_id": r.id,
        "status": r.status,
        "reason": r.reason,
        "queued": r.queue,
        "awaiting_turn": awaiting,
        "timed_out": p.timed_out,
    });
    let human = if p.timed_out {
        timeout_line("send", wt, p, timeout_s)
    } else {
        match r.status.as_str() {
            "applied" => match awaiting {
                Some(n) => format!("  Sent to @{wt} (turn {n})"),
                None => format!("  Sent to @{wt}"),
            },
            "refused" => refused_line(&format!("send to @{wt}"), &r.reason),
            _ if !waited => format!(
                "  Requested send to @{wt} (request {}); the running Keitora app applies it",
                r.id
            ),
            s => in_flight_line(&format!("Send to @{wt}"), s, r.id),
        }
    };
    Output {
        value,
        human,
        code: request_code(p),
    }
}

fn cmd_read(ctx: &Ctx, a: &ReadArgs) -> R<Output> {
    let wt = &a.worktree;
    authorize(ctx, wt)?;
    if a.last {
        let Some(reply) = ctx.local.session_reply_get(wt)? else {
            return Err(ExitError::new(
                6,
                format!("no finished turn recorded for @{wt}"),
            ));
        };
        return Ok(read_last_output(&reply));
    }
    if !(1..=DRIVE_READ_LINES_MAX).contains(&a.lines) {
        return Err(ExitError::new(
            4,
            format!("--lines must be 1..={DRIVE_READ_LINES_MAX}"),
        ));
    }
    require_registered(ctx)?;
    let req =
        ctx.local
            .drive_request_add("read", wt, &ctx.me, &json!({ "lines": a.lines }), false)?;
    let p = poll_request(ctx, req, a.timeout)?;
    Ok(read_pane_output(wt, a.lines, &p, a.timeout))
}

fn read_last_output(reply: &SessionReply) -> Output {
    let value = json!({
        "worktree": reply.worktree,
        "source": "last_message",
        "text": reply.last_message,
        "truncated": reply.last_message.len() >= SESSION_REPLY_MAX,
        "turn": reply.turn,
        "updated_at": reply.updated_at,
    });
    Output::ok(value, reply.last_message.clone())
}

fn read_pane_output(wt: &str, lines: i64, p: &Polled, timeout_s: u64) -> Output {
    let r = &p.req;
    if !p.timed_out && r.status == "applied" {
        let value = json!({
            "worktree": wt,
            "source": "pane",
            "text": r.result,
            "truncated": r.result.len() >= DRIVE_READ_RESULT_MAX,
            "lines": lines,
        });
        return Output::ok(value, r.result.clone());
    }
    let value = json!({
        "worktree": wt,
        "source": "pane",
        "request_id": r.id,
        "status": r.status,
        "reason": r.reason,
        "lines": lines,
        "timed_out": p.timed_out,
    });
    let human = if p.timed_out {
        timeout_line("read", wt, p, timeout_s)
    } else {
        refused_line(&format!("read of @{wt}"), &r.reason)
    };
    Output {
        value,
        human,
        code: request_code(p),
    }
}

fn cmd_reset_swap(
    ctx: &Ctx,
    kind: &str,
    wt: &str,
    to: Option<&str>,
    wait: bool,
    timeout_s: u64,
) -> R<Output> {
    authorize(ctx, wt)?;
    require_registered(ctx)?;
    check_provider(to)?;
    let payload = match to {
        Some(p) => json!({ "to": p }),
        None => json!({}),
    };
    let req = ctx
        .local
        .drive_request_add(kind, wt, &ctx.me, &payload, false)?;
    let p = settle(ctx, req, wait, timeout_s)?;
    Ok(provider_output(kind, wt, to, &p, wait, timeout_s))
}

/// `--to`'s shape (exit 4); whether the app supports it is the app's call.
fn check_provider(to: Option<&str>) -> R<()> {
    match to {
        Some(p) if !valid_provider(p) => Err(ExitError::new(
            4,
            format!("invalid provider \"{p}\": expected a lowercase name like claude or codex"),
        )),
        _ => Ok(()),
    }
}

/// `wake`: a `wake` row. The app decides whether the agent is running
/// (only it knows the live panes), so an awake target comes back applied with
/// `already_awake`.
fn cmd_wake(ctx: &Ctx, a: &WakeArgs) -> R<Output> {
    let wt = &a.worktree;
    authorize(ctx, wt)?;
    require_registered(ctx)?;
    check_provider(a.to.as_deref())?;
    let payload = wake_payload(a.to.as_deref(), a.fresh);
    let req = ctx
        .local
        .drive_request_add("wake", wt, &ctx.me, &payload, false)?;
    let p = settle(ctx, req, a.wait, a.timeout)?;
    Ok(provider_output(
        "wake",
        wt,
        a.to.as_deref(),
        &p,
        a.wait,
        a.timeout,
    ))
}

/// The `wake` payload: resume unless `fresh`, optionally on provider `to`.
fn wake_payload(to: Option<&str>, fresh: bool) -> Value {
    let mut v = json!({ "fresh": fresh });
    if let Some(p) = to {
        v["to"] = json!(p);
    }
    v
}

/// The output of a `reset`, `swap` or `wake` (`kind`): the provider the worktree is
/// on, and for a wake whether the agent was already running.
fn provider_output(
    kind: &str,
    wt: &str,
    to: Option<&str>,
    p: &Polled,
    waited: bool,
    timeout_s: u64,
) -> Output {
    let r = &p.req;
    let result = result_json(r);
    let provider = result
        .as_ref()
        .and_then(|v| v.get("provider").and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| to.map(str::to_string));
    // Known only once the wake is applied: `null` while pending or when refused.
    let already_awake = (kind == "wake" && r.status == "applied").then(|| {
        result
            .as_ref()
            .and_then(|v| v.get("already_awake").and_then(Value::as_bool))
            .unwrap_or(false)
    });
    let mut value = json!({
        "request_id": r.id,
        "status": r.status,
        "reason": r.reason,
        "provider": provider,
        "timed_out": p.timed_out,
    });
    if kind == "wake" {
        value["already_awake"] = json!(already_awake);
    }
    let confirm = format!("  @{wt} is waiting for the human to confirm the swap");
    let human = if p.timed_out {
        let line = timeout_line(kind, wt, p, timeout_s);
        if r.status == "pending_confirm" {
            format!("{confirm}\n{line}")
        } else {
            line
        }
    } else {
        match r.status.as_str() {
            "applied" if already_awake == Some(true) => format!("  @{wt} is already awake"),
            "applied" => match (kind, provider.as_deref()) {
                ("swap", Some(pv)) => format!("  Swapped @{wt} to {pv}"),
                ("wake", Some(pv)) => format!("  Woke @{wt} on {pv}"),
                ("wake", None) => format!("  Woke @{wt}"),
                (_, Some(pv)) => format!("  Reset @{wt} on {pv}"),
                _ => format!("  Reset @{wt}"),
            },
            "refused" => refused_line(&format!("{kind} of @{wt}"), &r.reason),
            "pending_confirm" => confirm,
            _ if !waited => format!("  Requested {kind} of @{wt} (request {})", r.id),
            s => in_flight_line(&format!("The {kind} of @{wt}"), s, r.id),
        }
    };
    Output {
        value,
        human,
        code: request_code(p),
    }
}

// ---- prompts / answer ----

/// One question of an `AskUserQuestion` prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub index: usize,
    pub question: String,
    pub header: String,
    pub multi_select: bool,
    pub options: Vec<String>,
}

/// The questions in a question prompt's `tool_input`; `None` when it doesn't parse.
pub fn parse_questions(tool_input: &str) -> Option<Vec<Question>> {
    let v: Value = serde_json::from_str(tool_input).ok()?;
    parse_question_list(&v, false).ok()
}

/// The questions in an `AskUserQuestion` input, the one parser behind both the gate
/// and the driver verbs. Lenient (`strict == false`, the driver side) accepts an empty
/// list, a missing `options`, bare-string options and `multi_select`; strict (the
/// gate, which renders the answer back) additionally rejects an empty list, a missing
/// `options` array and any option without a string `label`.
pub fn parse_question_list(
    input: &Value,
    strict: bool,
) -> std::result::Result<Vec<Question>, &'static str> {
    let Some(arr) = input.get("questions").and_then(Value::as_array) else {
        return Err("AskUserQuestion without a questions array");
    };
    if strict && arr.is_empty() {
        return Err("AskUserQuestion with no questions");
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, q) in arr.iter().enumerate() {
        let Some(question) = q.get("question").and_then(Value::as_str) else {
            return Err("question without a string `question`");
        };
        let opts = q.get("options").and_then(Value::as_array);
        if strict {
            let Some(opts) = opts else {
                return Err("question without an options array");
            };
            if !opts
                .iter()
                .all(|o| o.get("label").and_then(Value::as_str).is_some())
            {
                return Err("option without a string `label`");
            }
        }
        out.push(Question {
            index: i + 1,
            question: question.to_string(),
            header: q
                .get("header")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            multi_select: q
                .get("multiSelect")
                .or_else(|| q.get("multi_select"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            options: opts
                .map(|os| {
                    os.iter()
                        .filter_map(|o| {
                            o.get("label")
                                .and_then(Value::as_str)
                                .or_else(|| o.as_str())
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    Ok(out)
}

/// The prompt object shared by `prompts` and `wait`'s `prompt` event.
pub fn prompt_object(p: &DrivePrompt) -> Value {
    let questions = if p.kind == "question" {
        parse_questions(&p.tool_input).map(|qs| {
            qs.iter()
                .map(|q| {
                    json!({
                        "index": q.index,
                        "question": q.question,
                        "header": q.header,
                        "multi_select": q.multi_select,
                        "options": q.options,
                    })
                })
                .collect::<Vec<_>>()
        })
    } else {
        None
    };
    json!({
        "id": p.id,
        "driven": p.driven,
        "kind": p.kind,
        "tool_name": p.tool_name,
        "summary": p.summary,
        "tool_input": p.tool_input,
        "age_s": p.age_s,
        "timeout_s": p.timeout_s,
        "created_at": p.created_at,
        "provider": p.provider,
        "questions": questions,
    })
}

/// `Bash: rm -rf build`, or just the summary when there's no tool name.
fn prompt_what(p: &DrivePrompt) -> String {
    match (p.tool_name.is_empty(), p.summary.is_empty()) {
        (true, _) => p.summary.clone(),
        (false, true) => p.tool_name.clone(),
        (false, false) => format!("{}: {}", p.tool_name, p.summary),
    }
}

fn prompt_line(p: &DrivePrompt) -> String {
    format!(
        "  #{} @{} {} {}  ({}s ago)",
        p.id,
        p.driven,
        p.kind,
        prompt_what(p),
        p.age_s
    )
}

fn cmd_prompts(ctx: &Ctx, a: &PromptsArgs) -> R<Output> {
    if let Some(wt) = &a.worktree {
        authorize(ctx, wt)?;
    }
    let prompts = ctx
        .local
        .drive_prompts_pending(Some(&ctx.me), a.worktree.as_deref())?;
    let value = json!({
        "prompts": prompts.iter().map(prompt_object).collect::<Vec<_>>(),
    });
    let human = if prompts.is_empty() {
        "  No pending prompts".to_string()
    } else {
        prompts
            .iter()
            .map(prompt_line)
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(Output::ok(value, human))
}

/// Build the driver's decision for `prompt` from the verb's arguments.
pub fn build_decision(
    prompt: &DrivePrompt,
    decision: AnswerDecision,
    reason: Option<&str>,
    choices: &[String],
) -> R<PromptDecision> {
    let reject = |m: &str| Err(ExitError::new(4, m));
    match decision {
        AnswerDecision::Allow => {
            if reason.is_some() || !choices.is_empty() {
                return reject("allow takes no --reason or --choice");
            }
            Ok(PromptDecision::Allow)
        }
        AnswerDecision::Deny => {
            if !choices.is_empty() {
                return reject("--choice applies to answer only");
            }
            Ok(PromptDecision::Deny {
                reason: reason.unwrap_or_default().to_string(),
            })
        }
        AnswerDecision::Escalate => {
            if !choices.is_empty() {
                return reject("--choice applies to answer only");
            }
            match reason.map(str::trim) {
                Some(r) if !r.is_empty() => Ok(PromptDecision::Escalate {
                    reason: r.to_string(),
                }),
                _ => reject("escalate needs --reason (say why the human should decide)"),
            }
        }
        AnswerDecision::Answer => {
            if prompt.kind != "question" {
                return reject("answer applies to question prompts only; use allow or deny");
            }
            if reason.is_some() {
                return reject("--reason doesn't apply to answer");
            }
            if choices.is_empty() {
                return reject("answer needs at least one --choice QUESTION=ANSWER");
            }
            let Some(questions) = parse_questions(&prompt.tool_input) else {
                return reject("cannot read this question; use escalate");
            };
            let answers = parse_choices(&questions, choices)?;
            Ok(PromptDecision::Answer { answers })
        }
    }
}

/// Map `--choice Q=A` flags onto the questions' texts. Splits at the first `=`; a
/// numeric key is a 1-based index. Every question must be answered; a repeated key
/// keeps the last value.
pub fn parse_choices(questions: &[Question], choices: &[String]) -> R<BTreeMap<String, String>> {
    let mut answers = BTreeMap::new();
    for c in choices {
        let Some((k, v)) = c.split_once('=') else {
            return Err(ExitError::new(
                4,
                format!("bad --choice \"{c}\": expected QUESTION=ANSWER"),
            ));
        };
        let key = k.trim();
        if key.is_empty() || v.trim().is_empty() {
            return Err(ExitError::new(
                4,
                format!("bad --choice \"{c}\": expected QUESTION=ANSWER"),
            ));
        }
        let q = if let Ok(n) = key.parse::<usize>() {
            questions.iter().find(|q| q.index == n).ok_or_else(|| {
                ExitError::new(
                    4,
                    format!("no question {n} (this prompt has {})", questions.len()),
                )
            })?
        } else {
            questions
                .iter()
                .find(|q| q.question == key)
                .ok_or_else(|| ExitError::new(4, format!("unknown question \"{key}\"")))?
        };
        answers.insert(q.question.clone(), v.to_string());
    }
    let missing: Vec<String> = questions
        .iter()
        .filter(|q| !answers.contains_key(&q.question))
        .map(|q| format!("{} \"{}\"", q.index, q.question))
        .collect();
    if !missing.is_empty() {
        return Err(ExitError::new(
            4,
            format!("unanswered question(s): {}", missing.join(", ")),
        ));
    }
    Ok(answers)
}

fn cmd_answer(ctx: &Ctx, a: &AnswerArgs) -> R<Output> {
    let wt = &a.worktree;
    authorize(ctx, wt)?;
    let prompt = match ctx.local.drive_prompt_get(a.id)? {
        Some(p) if p.driven == *wt => p,
        _ => return Err(ExitError::new(6, format!("no prompt {} for @{wt}", a.id))),
    };
    let decision = build_decision(&prompt, a.decision, a.reason.as_deref(), &a.choices)?;
    let row = ctx.local.drive_prompt_decide(a.id, &ctx.me, &decision)?;
    let value = json!({
        "id": row.id,
        "status": row.status,
        "decided_by": row.decided_by,
        "reason": row.reason,
    });
    Ok(Output::ok(
        value,
        format!("  Prompt {} {}", row.id, row.status),
    ))
}

// ---- wait ----

/// How often `wait` checks `data_version`.
pub const WAIT_POLL: Duration = Duration::from_millis(500);
/// `wait` re-evaluates at least this often even with no DB change (for `stalled`).
pub const WAIT_REEVAL: Duration = Duration::from_secs(5);
/// The smallest `--stall-after`.
pub const STALL_AFTER_MIN: u64 = 10;

/// One listed worktree's state for one evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct WaitInput {
    pub worktree: String,
    pub awaiting_turn: Option<i64>,
    pub reply: Option<SessionReply>,
    /// The oldest pending prompt for the caller.
    pub prompt: Option<DrivePrompt>,
    pub state: Option<String>,
    /// Seconds since the last session heartbeat.
    pub idle_secs: Option<i64>,
    /// A send this call saw outstanding that the app then refused.
    pub refused_send: Option<RefusedSend>,
}

/// A refused `send` request, as `wait` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct RefusedSend {
    pub request_id: i64,
    pub reason: String,
}

/// What a `wait` call delivers.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitEvent {
    Prompt {
        worktree: String,
        prompt: Box<DrivePrompt>,
    },
    TurnFinished {
        worktree: String,
        awaiting: i64,
        reply: SessionReply,
    },
    Stalled {
        worktree: String,
        state: String,
        idle_s: i64,
    },
    SendRefused {
        worktree: String,
        send: RefusedSend,
    },
}

/// Pick the one event to deliver: any prompt, else any finished turn, else any refused
/// send, else any stall; ties go to argument order. `elapsed_s` is the time since this call started, which
/// clamps the idle time so a re-wait isn't told about the same stall immediately.
pub fn select_event(inputs: &[WaitInput], stall_after: u64, elapsed_s: u64) -> Option<WaitEvent> {
    if let Some(i) = inputs.iter().find(|i| i.prompt.is_some()) {
        return Some(WaitEvent::Prompt {
            worktree: i.worktree.clone(),
            prompt: Box::new(i.prompt.clone()?),
        });
    }
    for i in inputs {
        if let Some(a) = i.awaiting_turn {
            let turn = i.reply.as_ref().map(|r| r.turn).unwrap_or(0);
            if let (true, Some(reply)) = (turn >= a, &i.reply) {
                return Some(WaitEvent::TurnFinished {
                    worktree: i.worktree.clone(),
                    awaiting: a,
                    reply: reply.clone(),
                });
            }
        }
    }
    if let Some(i) = inputs.iter().find(|i| i.refused_send.is_some()) {
        return Some(WaitEvent::SendRefused {
            worktree: i.worktree.clone(),
            send: i.refused_send.clone()?,
        });
    }
    for i in inputs {
        if i.awaiting_turn.is_none() || i.state.as_deref() != Some("busy") {
            continue;
        }
        let Some(idle) = i.idle_secs else { continue };
        let idle = idle.max(0).min(elapsed_s as i64);
        if idle >= stall_after as i64 {
            return Some(WaitEvent::Stalled {
                worktree: i.worktree.clone(),
                state: "busy".into(),
                idle_s: idle,
            });
        }
    }
    None
}

/// The poll loop behind `wait`: evaluate once, then every [`WAIT_POLL`] check
/// `version`, re-evaluating only when it changed or [`WAIT_REEVAL`] passed. `None`
/// when `timeout` passes first.
pub fn poll_events<T>(
    clock: &dyn Clock,
    timeout: Option<Duration>,
    version: &mut dyn FnMut() -> R<i64>,
    eval: &mut dyn FnMut() -> R<Option<T>>,
) -> R<Option<T>> {
    let start = clock.now();
    let mut last_version = version()?;
    let mut last_eval = start;
    if let Some(e) = eval()? {
        return Ok(Some(e));
    }
    loop {
        let elapsed = clock.now().saturating_duration_since(start);
        let mut nap = WAIT_POLL;
        if let Some(t) = timeout {
            if elapsed >= t {
                return Ok(None);
            }
            nap = nap.min(t - elapsed);
        }
        clock.sleep(nap);
        let v = version()?;
        let now = clock.now();
        if v != last_version || now.saturating_duration_since(last_eval) >= WAIT_REEVAL {
            last_version = v;
            last_eval = now;
            if let Some(e) = eval()? {
                return Ok(Some(e));
            }
        }
    }
}

/// Read each listed worktree's state; a drive that is gone or changed hands is exit 3.
/// `tracked` collects the caller's open sends seen so far, so one that later ends
/// `refused` is reported (an `applied` one hands over to `awaiting_turn`).
fn gather(ctx: &Ctx, worktrees: &[String], tracked: &mut BTreeSet<i64>) -> R<Vec<WaitInput>> {
    let sessions = ctx.local.sessions_list()?;
    let mut out = Vec::new();
    for w in worktrees {
        let d = match ctx.local.drive_get(w)? {
            Some(d) if d.driver == ctx.me => d,
            _ => {
                return Err(ExitError::new(
                    3,
                    format!("@{w} is no longer driven by you"),
                ))
            }
        };
        let prompt = ctx
            .local
            .drive_prompts_pending(Some(&ctx.me), Some(w))?
            .into_iter()
            .next();
        let open = ctx.local.drive_requests_open("send", w, &ctx.me)?;
        tracked.extend(open.iter().map(|r| r.id));
        let mut refused_send = None;
        for id in tracked.clone() {
            if open.iter().any(|r| r.id == id) {
                continue;
            }
            match ctx.local.drive_request_get(id)? {
                Some(r) if r.worktree != *w => {}
                Some(r) if r.status == "refused" => {
                    refused_send.get_or_insert(RefusedSend {
                        request_id: r.id,
                        reason: r.reason,
                    });
                }
                _ => {
                    tracked.remove(&id);
                }
            }
        }
        out.push(WaitInput {
            worktree: w.clone(),
            awaiting_turn: d.awaiting_turn,
            reply: ctx.local.session_reply_get(w)?,
            prompt,
            state: sessions
                .iter()
                .find(|s| &s.worktree == w)
                .map(|s| s.state.clone()),
            idle_secs: ctx.local.session_idle_secs(w)?,
            refused_send,
        });
    }
    Ok(out)
}

fn cmd_wait(ctx: &Ctx, a: &WaitArgs) -> R<Output> {
    wait_with(ctx, a, &mut |inputs, stall, elapsed| {
        select_event(inputs, stall, elapsed)
    })
}

/// [`select_event`]'s shape: (inputs, stall_after, elapsed_s).
type Selector<'a> = dyn FnMut(&[WaitInput], u64, u64) -> Option<WaitEvent> + 'a;

/// `wait` with the selection injected (tests count evaluations through it).
fn wait_with(ctx: &Ctx, a: &WaitArgs, select: &mut Selector) -> R<Output> {
    if a.stall_after < STALL_AFTER_MIN {
        return Err(ExitError::new(
            4,
            format!("--stall-after must be at least {STALL_AFTER_MIN} seconds"),
        ));
    }
    let mut worktrees: Vec<String> = Vec::new();
    for w in &a.worktrees {
        if !worktrees.contains(w) {
            worktrees.push(w.clone());
        }
    }
    let mut outstanding = false;
    for w in &worktrees {
        let d = authorize(ctx, w)?;
        outstanding |= d.awaiting_turn.is_some()
            || !ctx
                .local
                .drive_prompts_pending(Some(&ctx.me), Some(w))?
                .is_empty()
            || !ctx
                .local
                .drive_requests_open("send", w, &ctx.me)?
                .is_empty();
    }
    if !outstanding {
        // The caller's last send was refused before this call: say so, not "nothing".
        for w in &worktrees {
            if let Some(r) = ctx.local.drive_request_latest("send", w, &ctx.me)? {
                if r.status == "refused" {
                    return Ok(event_output(&WaitEvent::SendRefused {
                        worktree: w.clone(),
                        send: RefusedSend {
                            request_id: r.id,
                            reason: r.reason,
                        },
                    }));
                }
            }
        }
        return Err(ExitError::new(
            4,
            format!(
                "nothing to wait for: no send is outstanding on {}",
                worktrees.join(", ")
            ),
        ));
    }

    let start = ctx.clock.now();
    let timeout = (a.timeout > 0).then(|| Duration::from_secs(a.timeout));
    let mut tracked = BTreeSet::new();
    let event = poll_events(
        ctx.clock,
        timeout,
        &mut || ctx.local.data_version().map_err(store_err),
        &mut || {
            let inputs = gather(ctx, &worktrees, &mut tracked)?;
            let elapsed = ctx.clock.now().saturating_duration_since(start).as_secs();
            Ok(select(&inputs, a.stall_after, elapsed))
        },
    )?;

    let Some(event) = event else {
        let waited = ctx.clock.now().saturating_duration_since(start).as_secs();
        return Ok(Output {
            value: json!({"event": "timeout", "waited_s": waited, "worktrees": worktrees}),
            human: format!("  timed out after {waited}s"),
            code: 2,
        });
    };
    if let WaitEvent::TurnFinished {
        worktree, awaiting, ..
    } = &event
    {
        // Ack before printing so the same turn is never delivered twice.
        ctx.local.drive_ack_turn(worktree, *awaiting)?;
    }
    Ok(event_output(&event))
}

fn event_output(e: &WaitEvent) -> Output {
    match e {
        WaitEvent::Prompt { worktree, prompt } => Output::ok(
            json!({"event": "prompt", "worktree": worktree, "prompt": prompt_object(prompt)}),
            format!(
                "  @{worktree} needs a decision: prompt {} ({}, {})",
                prompt.id,
                prompt.kind,
                prompt_what(prompt)
            ),
        ),
        WaitEvent::TurnFinished {
            worktree, reply, ..
        } => {
            let mut human = format!("  @{worktree} finished turn {}", reply.turn);
            if !reply.last_message.is_empty() {
                human.push('\n');
                human.push_str(&reply.last_message);
            }
            Output::ok(
                json!({
                    "event": "turn_finished",
                    "worktree": worktree,
                    "turn": reply.turn,
                    "last_message": reply.last_message,
                    "updated_at": reply.updated_at,
                }),
                human,
            )
        }
        WaitEvent::Stalled {
            worktree,
            state,
            idle_s,
        } => Output::ok(
            json!({"event": "stalled", "worktree": worktree, "state": state, "idle_s": idle_s}),
            format!(
                "  @{worktree} looks stalled ({state}, no activity for {idle_s}s); try: keitora read {worktree}"
            ),
        ),
        WaitEvent::SendRefused { worktree, send } => Output {
            value: json!({
                "event": "send_refused",
                "worktree": worktree,
                "request_id": send.request_id,
                "reason": send.reason,
            }),
            human: format!("  Refused send to @{worktree} ({})", send.reason),
            code: 5,
        },
    }
}

#[cfg(test)]
#[path = "drive_tests.rs"]
mod tests;
