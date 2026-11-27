//! `keitora session gate`: the permission/question hook for a driven session.
//!
//! When this worktree is driven, the hook records the prompt in `tasks.db`, waits for
//! the driver's decision, and prints the provider's hook JSON. Every failure ends in
//! "no decision" (empty stdout), which makes the provider show its native dialog; the
//! gate never invents an allow.
//!
//! All logic is pure over the [`GateStore`] and [`Clock`] traits so it can be tested
//! with fakes; only [`gate_main`] touches stdin, stdout and the process exit.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::{IsTerminal, Read, Write as _};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use keitora_tasks::{
    current_worktree, resolve, truncate_bytes, Config, LocalStore, NewDrivePrompt, Result,
    DRIVE_ESCALATE_AFTER_MAX_S, DRIVE_PERMISSION_INPUT_MAX, DRIVE_QUESTION_INPUT_MAX,
};
use serde_json::{Map, Value};

use crate::drive::parse_question_list;

/// Most bytes of hook payload the gate reads from stdin.
pub const GATE_STDIN_CAP: u64 = 1024 * 1024;
/// How long the gate waits for stdin to reach EOF.
pub const GATE_STDIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Time between polls of the prompt row.
pub const POLL_TICK: Duration = Duration::from_millis(500);
/// Every this many ticks the row is read even when `data_version` is unchanged.
pub const SAFETY_TICKS: u32 = 20;
/// Consecutive store errors tolerated while polling; one more escalates.
pub const MAX_POLL_ERRORS: u32 = 3;
/// Bytes of a string leaf kept in a stored permission `tool_input`.
pub const INPUT_LEAF_MAX: usize = 1024;
/// Bytes of a stored permission `tool_input` before it collapses to a `_truncated`
/// prefix. Question inputs are never cut (see [`stored_input`]).
pub const INPUT_MAX: usize = 8192;
/// Bytes of a deny message shown to the agent.
pub const DENY_MESSAGE_MAX: usize = 500;
/// Chars of a prompt summary.
pub const SUMMARY_MAX: usize = 200;

const TRUNCATED_MARKER: &str = "...[truncated]";

/// The agent CLI that runs the hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Provider::Claude),
            "codex" => Some(Provider::Codex),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }
}

/// The parsed hook payload (only the fields the gate uses).
#[derive(Debug, Clone, PartialEq)]
pub struct GatePayload {
    pub hook_event_name: String,
    pub tool_name: String,
    /// Always a JSON object (`{}` when the payload has none).
    pub tool_input: Value,
    /// `AskUserQuestion` question texts, in order; empty for other tools.
    pub questions: Vec<String>,
}

/// Why a payload was rejected (the gate then prints nothing).
#[derive(Debug, Clone, PartialEq)]
pub struct GateParseError(pub String);

/// What the gate does with a hook invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Ignore,
    Permission,
    Question,
}

impl Route {
    fn kind(self) -> Option<&'static str> {
        match self {
            Route::Ignore => None,
            Route::Permission => Some("permission"),
            Route::Question => Some("question"),
        }
    }
}

/// A drive binding as the gate needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct DriveInfo {
    pub driver: String,
    pub paused: bool,
    pub escalate_after_s: u32,
}

/// A prompt row as the gate needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptRow {
    pub status: String,
    pub decision_json: Option<String>,
    pub reason: String,
    pub decided_by: Option<String>,
    pub driver: String,
}

/// What the gate tells the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Allow,
    Deny {
        message: String,
    },
    /// `(question text, answer)` in the payload's question order.
    Answer {
        answers: Vec<(String, String)>,
    },
    Escalate,
}

impl Outcome {
    fn is_driver_decision(&self) -> bool {
        !matches!(self, Outcome::Escalate)
    }
}

/// The task-store calls the gate makes; [`LocalStore`] in production, a fake in tests.
pub trait GateStore {
    fn drive_for(&self, worktree: &str) -> Result<Option<DriveInfo>>;
    fn session_state(&self, worktree: &str) -> Result<Option<String>>;
    fn insert_prompt(&self, new: &NewDrivePrompt) -> Result<i64>;
    fn prompt(&self, id: i64) -> Result<PromptRow>;
    /// Compare-and-set `pending` -> `escalated`; a no-op when already terminal.
    fn escalate_if_pending(&self, id: i64, decided_by: &str, reason: &str) -> Result<()>;
    fn set_session_state(&self, worktree: &str, state: &str) -> Result<()>;
    /// After a driver decision: `busy` unless another prompt for `worktree` is still
    /// pending. Never withdraws other gates' prompts.
    fn resume_after_decision(&self, worktree: &str) -> Result<()>;
    fn data_version(&self) -> Result<i64>;
}

/// Time source for the polling loop; virtual in tests.
pub trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
}

pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

impl GateStore for LocalStore {
    fn drive_for(&self, worktree: &str) -> Result<Option<DriveInfo>> {
        Ok(self.drive_get(worktree)?.map(|d| DriveInfo {
            driver: d.driver,
            paused: d.paused,
            escalate_after_s: d.escalate_after_s.clamp(0, i64::from(u32::MAX)) as u32,
        }))
    }

    fn session_state(&self, worktree: &str) -> Result<Option<String>> {
        self.session_state_get(worktree)
    }

    fn insert_prompt(&self, new: &NewDrivePrompt) -> Result<i64> {
        Ok(self.drive_prompt_insert(new)?.id)
    }

    fn prompt(&self, id: i64) -> Result<PromptRow> {
        let p = self
            .drive_prompt_get(id)?
            .ok_or_else(|| keitora_tasks::Error::Conflict(format!("no prompt {id}")))?;
        Ok(PromptRow {
            status: p.status,
            decision_json: p.decision_json,
            reason: p.reason,
            decided_by: p.decided_by,
            driver: p.driver,
        })
    }

    fn escalate_if_pending(&self, id: i64, decided_by: &str, reason: &str) -> Result<()> {
        self.drive_prompt_close(id, "escalated", decided_by, reason)?;
        Ok(())
    }

    fn set_session_state(&self, worktree: &str, state: &str) -> Result<()> {
        let current = self.active_task_id(worktree).ok().flatten();
        self.session_upsert(worktree, state, current, None)
    }

    fn resume_after_decision(&self, worktree: &str) -> Result<()> {
        let current = self.active_task_id(worktree).ok().flatten();
        self.session_resume_after_decision(worktree, current)?;
        Ok(())
    }

    fn data_version(&self) -> Result<i64> {
        LocalStore::data_version(self)
    }
}

// ---- Pure helpers ----

/// Read `reader` to EOF on a helper thread, keeping at most `cap` bytes, and return
/// what arrived within `timeout` (partial data on timeout or error). A terminal is not
/// read at all, so a hook run by hand cannot hang.
pub fn read_bounded<R: Read + Send + 'static>(
    reader: R,
    cap: u64,
    timeout: Duration,
    is_terminal: bool,
) -> Vec<u8> {
    if is_terminal {
        return Vec::new();
    }
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buf);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut r = reader.take(cap);
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => match sink.lock() {
                    Ok(mut b) => b.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                },
            }
        }
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(timeout);
    let out = match buf.lock() {
        Ok(b) => b.clone(),
        Err(_) => Vec::new(),
    };
    out
}

fn parse_err<T>(msg: impl Into<String>) -> std::result::Result<T, GateParseError> {
    Err(GateParseError(msg.into()))
}

/// The `AskUserQuestion` question texts, validating the shape the renderer relies on.
fn parse_questions(input: &Value) -> std::result::Result<Vec<String>, GateParseError> {
    match parse_question_list(input, true) {
        Ok(qs) => Ok(qs.into_iter().map(|q| q.question).collect()),
        Err(msg) => parse_err(msg),
    }
}

/// Parse a hook payload: a UTF-8 JSON object with `hook_event_name`, `tool_name` and
/// an optional object `tool_input`. Unknown fields are ignored.
pub fn parse_payload(raw: &[u8]) -> std::result::Result<GatePayload, GateParseError> {
    let Ok(text) = std::str::from_utf8(raw) else {
        return parse_err("payload is not UTF-8");
    };
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return parse_err("payload is not JSON");
    };
    let Some(obj) = v.as_object() else {
        return parse_err("payload is not a JSON object");
    };
    let event = match obj.get("hook_event_name").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return parse_err("payload has no hook_event_name"),
    };
    let tool_name = match obj.get("tool_name").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return parse_err("payload has no tool_name"),
    };
    let tool_input = match obj.get("tool_input") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return parse_err("tool_input is not an object"),
    };
    let questions = if tool_name == "AskUserQuestion" {
        parse_questions(&tool_input)?
    } else {
        Vec::new()
    };
    Ok(GatePayload {
        hook_event_name: event,
        tool_name,
        tool_input,
        questions,
    })
}

/// Which hook invocations are brokered. Claude brokers `AskUserQuestion` once, through
/// `PreToolUse`; Codex brokers every `PermissionRequest`.
pub fn route(provider: Provider, payload: &GatePayload) -> Route {
    let question = payload.tool_name == "AskUserQuestion";
    match (provider, payload.hook_event_name.as_str()) {
        (Provider::Claude, "PermissionRequest") if question => Route::Ignore,
        (Provider::Claude, "PermissionRequest") => Route::Permission,
        (Provider::Claude, "PreToolUse") if question => Route::Question,
        (Provider::Codex, "PermissionRequest") => Route::Permission,
        _ => Route::Ignore,
    }
}

/// A one-line description of the prompt for the driver.
pub fn summarize(tool_name: &str, input: &Value) -> String {
    let field = |k: &str| input.get(k).and_then(Value::as_str);
    let text = match tool_name {
        "Bash" | "shell" | "local_shell" | "exec_command" => match input.get("command") {
            Some(Value::String(s)) => Some(s.lines().next().unwrap_or("").to_string()),
            Some(Value::Array(parts)) => Some(
                parts
                    .iter()
                    .map(|p| p.as_str().map_or_else(|| p.to_string(), str::to_string))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        },
        "Edit" | "Write" | "MultiEdit" | "Read" => field("file_path").map(str::to_string),
        "NotebookEdit" => field("notebook_path").map(str::to_string),
        "WebFetch" => field("url").map(str::to_string),
        "AskUserQuestion" => input
            .get("questions")
            .and_then(Value::as_array)
            .and_then(|qs| qs.first())
            .and_then(|q| q.get("question"))
            .and_then(Value::as_str)
            .map(str::to_string),
        t if t.starts_with("mcp__") => Some(t.to_string()),
        _ => None,
    };
    let text = text.unwrap_or_else(|| input.to_string());
    keitora_tasks::truncate_chars(&text, SUMMARY_MAX)
}

fn truncate_leaves(v: &Value) -> Value {
    match v {
        Value::String(s) if s.len() > INPUT_LEAF_MAX => Value::String(format!(
            "{}{TRUNCATED_MARKER}",
            truncate_bytes(s, INPUT_LEAF_MAX)
        )),
        Value::Array(a) => Value::Array(a.iter().map(truncate_leaves).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), truncate_leaves(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The `tool_input` to store: string leaves over [`INPUT_LEAF_MAX`] bytes are cut with
/// a marker; a result still over `max` bytes collapses to
/// `{"_truncated": "<first max - 192 bytes>"}`. Cuts land on char boundaries.
pub fn truncate_input(input: &Value, max: usize) -> String {
    let s = truncate_leaves(input).to_string();
    if s.len() <= max {
        return s;
    }
    let prefix = truncate_bytes(&s, max.saturating_sub(192));
    serde_json::json!({ "_truncated": prefix }).to_string()
}

/// The `tool_input` to store for a brokered prompt. A permission input is cut to fit
/// ([`truncate_input`]). A question input is the payload the driver answers, so it is
/// stored whole; one over [`DRIVE_QUESTION_INPUT_MAX`] bytes can't be stored intact and
/// is `None` (the gate then leaves it to the human).
pub fn stored_input(route: Route, input: &Value) -> Option<String> {
    match route {
        Route::Question => {
            let s = input.to_string();
            (s.len() <= DRIVE_QUESTION_INPUT_MAX).then_some(s)
        }
        _ => Some(truncate_input(
            input,
            INPUT_MAX.min(DRIVE_PERMISSION_INPUT_MAX),
        )),
    }
}

/// The payload question a driver's answer key names: its exact text, its 1-based
/// index, or a stored copy cut with [`TRUNCATED_MARKER`] that is a prefix of exactly
/// one question. Repeated question texts resolve to their first occurrence.
fn question_slot(questions: &[String], key: &str) -> Option<usize> {
    let first = |i: usize| questions.iter().position(|q| *q == questions[i]);
    if let Some(i) = questions.iter().position(|q| q == key) {
        return Some(i);
    }
    if let Ok(n) = key.trim().parse::<usize>() {
        if (1..=questions.len()).contains(&n) {
            return first(n - 1);
        }
        return None;
    }
    let stem = key.strip_suffix(TRUNCATED_MARKER)?;
    if stem.is_empty() {
        return None;
    }
    let mut hits = questions
        .iter()
        .enumerate()
        .filter(|(_, q)| q.starts_with(stem));
    let (i, q) = hits.next()?;
    if hits.any(|(_, other)| other != q) {
        return None;
    }
    first(i)
}

/// The answers in a question prompt's `decision_json`, mapped back onto the payload's
/// full question texts, when they cover exactly the payload's questions with non-empty
/// values. A key naming the same question twice, or no question, is rejected.
fn answers_from(
    decision_json: Option<&str>,
    payload: &GatePayload,
) -> Option<Vec<(String, String)>> {
    let v: Value = serde_json::from_str(decision_json?).ok()?;
    let answers = v.get("answers")?.as_object()?;
    let questions = &payload.questions;
    if questions.is_empty() {
        return None;
    }
    let mut slots: Vec<Option<&str>> = vec![None; questions.len()];
    for (key, value) in answers {
        let a = value.as_str()?;
        if a.is_empty() {
            return None;
        }
        let i = question_slot(questions, key)?;
        if slots[i].replace(a).is_some() {
            return None;
        }
    }
    let mut out = Vec::with_capacity(questions.len());
    let mut seen = HashSet::new();
    for (i, q) in questions.iter().enumerate() {
        if !seen.insert(q.as_str()) {
            continue;
        }
        out.push((q.clone(), slots[i]?.to_string()));
    }
    Some(out)
}

/// Map a terminal (or deadline-read) prompt row to what the agent is told. `Allow`
/// comes only from an `allowed` permission row.
pub fn outcome_from_row(kind: &str, row: &PromptRow, payload: &GatePayload) -> Outcome {
    match (row.status.as_str(), kind) {
        ("allowed", "permission") => Outcome::Allow,
        ("denied", "permission" | "question") => {
            let driver = row
                .decided_by
                .as_deref()
                .and_then(|d| d.strip_prefix("driver:"))
                .unwrap_or(&row.driver);
            let msg = if row.reason.trim().is_empty() {
                format!("Denied by driver {driver}")
            } else {
                format!("Denied by driver {driver}: {}", row.reason)
            };
            Outcome::Deny {
                message: truncate_bytes(&msg, DENY_MESSAGE_MAX),
            }
        }
        ("answered", "question") => match answers_from(row.decision_json.as_deref(), payload) {
            Some(answers) => Outcome::Answer { answers },
            None => Outcome::Escalate,
        },
        _ => Outcome::Escalate,
    }
}

/// `s` with every non-ASCII char written as a JSON `\uXXXX` escape (surrogate pairs
/// above the BMP). Valid on serialized JSON, where non-ASCII only appears in strings.
pub fn ascii_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                let _ = write!(out, "\\u{u:04x}");
            }
        }
    }
    out
}

fn json_str(s: &str) -> Option<String> {
    serde_json::to_string(s).ok()
}

/// The exact stdout for an outcome (`None` prints nothing: the native UI shows).
pub fn render(
    provider: Provider,
    route: Route,
    outcome: &Outcome,
    payload: &GatePayload,
) -> Option<String> {
    let line = match (route, outcome) {
        (Route::Ignore, _) | (_, Outcome::Escalate) => return None,
        (Route::Permission, Outcome::Allow) => {
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
                .to_string()
        }
        (Route::Permission, Outcome::Deny { message }) => format!(
            r#"{{"hookSpecificOutput":{{"hookEventName":"PermissionRequest","decision":{{"behavior":"deny","message":{}}}}}}}"#,
            json_str(message)?
        ),
        (Route::Question, Outcome::Deny { message }) if provider == Provider::Claude => format!(
            r#"{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":{}}}}}"#,
            json_str(message)?
        ),
        (Route::Question, Outcome::Answer { answers }) if provider == Provider::Claude => {
            let questions = serde_json::to_string(payload.tool_input.get("questions")?).ok()?;
            let mut a = String::from("{");
            for (i, (q, l)) in answers.iter().enumerate() {
                if i > 0 {
                    a.push(',');
                }
                a.push_str(&json_str(q)?);
                a.push(':');
                a.push_str(&json_str(l)?);
            }
            a.push('}');
            format!(
                r#"{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecision":"allow","permissionDecisionReason":"Answered by driver","updatedInput":{{"questions":{questions},"answers":{a}}}}}}}"#
            )
        }
        _ => return None,
    };
    Some(ascii_escape(&line))
}

// ---- The gate ----

/// Every prompt status except `pending` is terminal.
fn is_terminal_status(status: &str) -> bool {
    status != "pending"
}

/// Why a brokered wait should stop early, if it should.
fn stop_reason<S: GateStore>(store: &S, me: &str, driver: &str) -> Result<Option<&'static str>> {
    match store.drive_for(me)? {
        None => return Ok(Some("drive ended")),
        Some(d) if d.driver != driver => return Ok(Some("drive taken over by another driver")),
        Some(d) if d.paused => return Ok(Some("drive paused")),
        Some(_) => {}
    }
    match store.session_state(driver)?.as_deref() {
        None | Some("offline") => Ok(Some("driver offline")),
        Some(_) => Ok(None),
    }
}

/// Wait for prompt `id` to become terminal. `None` means escalate with no row.
fn poll<S: GateStore, C: Clock>(
    store: &S,
    clock: &C,
    me: &str,
    drive: &DriveInfo,
    id: i64,
) -> Option<PromptRow> {
    let cap = drive
        .escalate_after_s
        .min(DRIVE_ESCALATE_AFTER_MAX_S as u32);
    let deadline = clock.now() + Duration::from_secs(u64::from(cap));
    let give_up = || {
        let _ = store.escalate_if_pending(id, "system", "task store unavailable");
        None
    };
    let mut errors = 0u32;
    let mut last_v = store.data_version().ok();
    let mut need_select = true;
    // A failed read is retried on the next tick, not at the next safety tick.
    let mut retry = false;
    let mut tick = 0u32;
    loop {
        if need_select {
            retry = false;
            match store.prompt(id) {
                Ok(row) if is_terminal_status(&row.status) => return Some(row),
                Ok(_) => match stop_reason(store, me, &drive.driver) {
                    Ok(None) => errors = 0,
                    Ok(Some(reason)) => {
                        let _ = store.escalate_if_pending(id, "system", reason);
                        return store.prompt(id).ok();
                    }
                    Err(_) => {
                        errors += 1;
                        retry = true;
                    }
                },
                Err(_) => {
                    errors += 1;
                    retry = true;
                }
            }
            if errors > MAX_POLL_ERRORS {
                return give_up();
            }
        }
        if clock.now() >= deadline {
            let _ =
                store.escalate_if_pending(id, "timeout", &format!("no driver decision in {cap}s"));
            // A decision that landed in the same instant wins the compare-and-set.
            return store.prompt(id).ok();
        }
        clock.sleep(POLL_TICK);
        tick += 1;
        match store.data_version() {
            Ok(v) => {
                need_select = retry || last_v != Some(v) || tick.is_multiple_of(SAFETY_TICKS);
                last_v = Some(v);
            }
            Err(_) => {
                errors += 1;
                if errors > MAX_POLL_ERRORS {
                    return give_up();
                }
                need_select = true;
            }
        }
    }
}

/// Run the gate for worktree `me`; returns the exact stdout (`None` prints nothing).
/// Never fails: every error is "no decision".
pub fn run_gate<S: GateStore, C: Clock>(
    store: &S,
    clock: &C,
    provider: Provider,
    me: &str,
    payload: &GatePayload,
) -> Option<String> {
    // Codex's PermissionRequest hook replaced `session waiting`; keep that write on
    // every path that does not broker.
    let not_brokered = || {
        if provider == Provider::Codex {
            let _ = store.set_session_state(me, "waiting");
        }
        None
    };
    let route = route(provider, payload);
    let Some(kind) = route.kind() else {
        return not_brokered();
    };
    let drive = match store.drive_for(me) {
        Ok(Some(d)) => d,
        _ => return not_brokered(),
    };
    let driver_online = matches!(
        store.session_state(&drive.driver),
        Ok(Some(ref s)) if s != "offline"
    );
    if drive.paused || !driver_online {
        return not_brokered();
    }
    let Some(tool_input) = stored_input(route, &payload.tool_input) else {
        return not_brokered();
    };
    let new = NewDrivePrompt {
        driven: me.to_string(),
        provider: provider.as_str().to_string(),
        kind: kind.to_string(),
        tool_name: payload.tool_name.clone(),
        tool_input,
        summary: summarize(&payload.tool_name, &payload.tool_input),
        hook_pid: Some(i64::from(std::process::id())),
    };
    let Ok(id) = store.insert_prompt(&new) else {
        return not_brokered();
    };
    if store.set_session_state(me, "waiting").is_err() {
        let _ = store.escalate_if_pending(id, "system", "could not record waiting");
        return None;
    }
    let outcome = match poll(store, clock, me, &drive, id) {
        Some(row) => outcome_from_row(kind, &row, payload),
        None => Outcome::Escalate,
    };
    if outcome.is_driver_decision() {
        // Parallel tool calls run parallel gates: a plain `busy` write would
        // withdraw their still-pending prompts.
        let _ = store.resume_after_decision(me);
    }
    render(provider, route, &outcome, payload)
}

/// Whether `me` is driven, read through a read-only handle so an undriven worktree
/// never pays for the writable open (WAL pragmas and schema init). A missing DB or
/// missing drive tables mean "not driven".
fn is_driven(db_path: &str, me: &str) -> bool {
    LocalStore::open_read_only(db_path)
        .and_then(|ro| ro.drive_get(me))
        .is_ok_and(|d| d.is_some())
}

/// Read stdin, parse, open the project's store and run the gate.
fn gate_output(provider: Option<&str>) -> Option<String> {
    let provider = Provider::parse(provider?)?;
    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    let raw = read_bounded(stdin, GATE_STDIN_CAP, GATE_STDIN_TIMEOUT, terminal);
    let config = Config::from_project(resolve().ok()?);
    let open = || LocalStore::open(&config.db_path).ok();
    let me = current_worktree();
    match parse_payload(&raw) {
        Ok(payload) => {
            // Codex records `waiting` even when nothing is brokered, so only Claude
            // can skip the writable store for an undriven worktree.
            if provider == Provider::Claude && !is_driven(&config.db_path, &me) {
                return None;
            }
            run_gate(&open()?, &RealClock, provider, &me, &payload)
        }
        Err(_) => {
            if provider == Provider::Codex {
                let _ = open()?.set_session_state(&me, "waiting");
            }
            None
        }
    }
}

/// `keitora session gate --provider <p>`: print the decision (if any) and exit 0 on
/// every path, ignoring `--json`.
pub fn gate_main(provider: Option<&str>) -> ! {
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| gate_output(provider)))
        .ok()
        .flatten();
    if let Some(s) = out {
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "{s}");
        let _ = stdout.flush();
    }
    std::process::exit(0)
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
