use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use keitora_tasks::Error;
use serde_json::json;

use super::*;

// ---- Fixtures ----

fn claude_bash() -> Vec<u8> {
    json!({
        "session_id": "s1",
        "transcript_path": "/Users/me/.claude/projects/x/s1.jsonl",
        "cwd": "/Users/me/proj",
        "permission_mode": "default",
        "hook_event_name": "PermissionRequest",
        "tool_name": "Bash",
        "tool_input": {"command": "cargo test\necho done", "description": "run tests"},
        "permission_suggestions": [],
        "some_future_field": {"x": 1}
    })
    .to_string()
    .into_bytes()
}

fn question_input() -> Value {
    json!({
        "questions": [
            {"question": "Which db?", "header": "DB", "multiSelect": false,
             "options": [{"label": "sqlite", "description": "file"}, {"label": "pg"}]},
            {"question": "Ship it?", "options": [{"label": "yes"}, {"label": "no"}]}
        ]
    })
}

fn claude_question() -> Vec<u8> {
    json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "AskUserQuestion",
        "tool_input": question_input(),
        "tool_use_id": "t1"
    })
    .to_string()
    .into_bytes()
}

fn codex_permission() -> Vec<u8> {
    json!({
        "hook_event_name": "PermissionRequest",
        "tool_name": "shell",
        "tool_input": {"command": ["git", "push"]},
        "turn_id": "x",
        "model": "m"
    })
    .to_string()
    .into_bytes()
}

fn payload(raw: &[u8]) -> GatePayload {
    parse_payload(raw).expect("fixture parses")
}

fn ev(event: &str, tool: &str) -> GatePayload {
    let input = if tool == "AskUserQuestion" {
        question_input()
    } else {
        json!({})
    };
    payload(
        json!({"hook_event_name": event, "tool_name": tool, "tool_input": input})
            .to_string()
            .as_bytes(),
    )
}

fn row(status: &str) -> PromptRow {
    PromptRow {
        status: status.into(),
        decision_json: None,
        reason: String::new(),
        decided_by: Some("driver:alpha".into()),
        driver: "alpha".into(),
    }
}

fn answered(answers: Value) -> PromptRow {
    PromptRow {
        decision_json: Some(json!({ "answers": answers }).to_string()),
        ..row("answered")
    }
}

// ---- Fakes ----

struct FakeClock {
    base: Instant,
    t: Rc<Cell<Duration>>,
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.base + self.t.get()
    }
    fn sleep(&self, d: Duration) {
        self.t.set(self.t.get() + d);
    }
}

#[derive(Default)]
struct State {
    drive: Option<DriveInfo>,
    sessions: HashMap<String, String>,
    row: Option<PromptRow>,
    dv: i64,
}

type Event = Box<dyn Fn(&mut State)>;

#[derive(Default)]
struct Calls {
    inserts: Vec<NewDrivePrompt>,
    prompt: u32,
    dv: u32,
    writes: Vec<(String, String)>,
    escalations: Vec<(String, String)>,
}

#[derive(Default)]
struct FakeStore {
    t: Rc<Cell<Duration>>,
    s: RefCell<State>,
    /// (virtual time, change made by another connection): applied once due.
    script: RefCell<Vec<(Duration, Event)>>,
    /// Applied just before the escalate compare-and-set (a racing driver decision).
    on_escalate: RefCell<Option<Event>>,
    calls: RefCell<Calls>,
    fail_prompt: Cell<u32>,
    fail_dv: Cell<u32>,
    fail_drive_for: Cell<bool>,
    fail_insert: Cell<bool>,
    fail_waiting: Cell<bool>,
}

fn err<T>() -> Result<T> {
    Err(Error::Conflict("database is locked".into()))
}

impl FakeStore {
    fn driven(escalate_after_s: u32) -> Self {
        let s = FakeStore::default();
        {
            let mut st = s.s.borrow_mut();
            st.drive = Some(DriveInfo {
                driver: "alpha".into(),
                paused: false,
                escalate_after_s,
            });
            st.sessions.insert("alpha".into(), "busy".into());
        }
        s
    }

    fn clock(&self) -> FakeClock {
        FakeClock {
            base: Instant::now(),
            t: Rc::clone(&self.t),
        }
    }

    /// At virtual time `at`, another connection commits `f`.
    fn at(&self, at: Duration, f: impl Fn(&mut State) + 'static) {
        self.script.borrow_mut().push((
            at,
            Box::new(move |st: &mut State| {
                f(st);
                st.dv += 1;
            }),
        ));
    }

    fn decide_at(&self, at: Duration, r: PromptRow) {
        self.at(at, move |st| st.row = Some(r.clone()));
    }

    /// Scripted changes model other connections acting on the inserted prompt, so
    /// they start once the row exists.
    fn apply_due(&self) {
        if self.s.borrow().row.is_none() {
            return;
        }
        let now = self.t.get();
        let mut script = self.script.borrow_mut();
        let mut st = self.s.borrow_mut();
        script.retain(|(at, f)| {
            if *at <= now {
                f(&mut st);
                false
            } else {
                true
            }
        });
    }

    fn take_fail(c: &Cell<u32>) -> bool {
        if c.get() > 0 {
            c.set(c.get() - 1);
            true
        } else {
            false
        }
    }
}

impl GateStore for FakeStore {
    fn drive_for(&self, _worktree: &str) -> Result<Option<DriveInfo>> {
        self.apply_due();
        if self.fail_drive_for.get() {
            return err();
        }
        Ok(self.s.borrow().drive.clone())
    }
    fn session_state(&self, worktree: &str) -> Result<Option<String>> {
        self.apply_due();
        Ok(self.s.borrow().sessions.get(worktree).cloned())
    }
    fn insert_prompt(&self, new: &NewDrivePrompt) -> Result<i64> {
        if self.fail_insert.get() {
            return err();
        }
        self.calls.borrow_mut().inserts.push(new.clone());
        self.s.borrow_mut().row = Some(PromptRow {
            status: "pending".into(),
            decision_json: None,
            reason: String::new(),
            decided_by: None,
            driver: "alpha".into(),
        });
        Ok(7)
    }
    fn prompt(&self, id: i64) -> Result<PromptRow> {
        assert_eq!(id, 7);
        self.apply_due();
        self.calls.borrow_mut().prompt += 1;
        if Self::take_fail(&self.fail_prompt) {
            return err();
        }
        Ok(self.s.borrow().row.clone().expect("row inserted"))
    }
    fn escalate_if_pending(&self, _id: i64, decided_by: &str, reason: &str) -> Result<()> {
        if let Some(f) = self.on_escalate.borrow_mut().take() {
            f(&mut self.s.borrow_mut());
        }
        self.calls
            .borrow_mut()
            .escalations
            .push((decided_by.into(), reason.into()));
        let mut st = self.s.borrow_mut();
        if let Some(r) = st.row.as_mut() {
            if r.status == "pending" {
                r.status = "escalated".into();
                r.decided_by = Some(decided_by.into());
                r.reason = reason.into();
            }
        }
        Ok(())
    }
    fn set_session_state(&self, worktree: &str, state: &str) -> Result<()> {
        if state == "waiting" && self.fail_waiting.get() {
            return err();
        }
        self.calls
            .borrow_mut()
            .writes
            .push((worktree.into(), state.into()));
        Ok(())
    }
    fn resume_after_decision(&self, worktree: &str) -> Result<()> {
        self.set_session_state(worktree, "busy")
    }
    fn data_version(&self) -> Result<i64> {
        self.apply_due();
        self.calls.borrow_mut().dv += 1;
        if Self::take_fail(&self.fail_dv) {
            return err();
        }
        Ok(self.s.borrow().dv)
    }
}

fn gate(store: &FakeStore, provider: Provider, raw: &[u8]) -> Option<String> {
    run_gate(store, &store.clock(), provider, "bravo", &payload(raw))
}

fn writes(store: &FakeStore) -> Vec<String> {
    store
        .calls
        .borrow()
        .writes
        .iter()
        .map(|(w, s)| format!("{w}={s}"))
        .collect()
}

const ALLOW_PERMISSION: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;

// ---- parse_payload ----

#[test]
fn parses_claude_permission_payload_ignoring_unknown_fields() {
    let p = payload(&claude_bash());
    assert_eq!(p.hook_event_name, "PermissionRequest");
    assert_eq!(p.tool_name, "Bash");
    assert_eq!(p.tool_input["command"], "cargo test\necho done");
    assert!(p.questions.is_empty());
}

#[test]
fn parses_claude_question_payload() {
    let p = payload(&claude_question());
    assert_eq!(p.hook_event_name, "PreToolUse");
    assert_eq!(p.questions, vec!["Which db?", "Ship it?"]);
    assert_eq!(p.tool_input, question_input());
}

#[test]
fn parses_codex_payload_and_defaults_missing_input() {
    let p = payload(&codex_permission());
    assert_eq!(p.tool_name, "shell");
    let p = payload(br#"{"hook_event_name":"PermissionRequest","tool_name":"shell"}"#);
    assert_eq!(p.tool_input, json!({}));
}

#[test]
fn rejects_malformed_payloads() {
    let bad: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"not json".to_vec(),
        b"[1,2]".to_vec(),
        vec![0xff, 0xfe, b'{', b'}'],
        br#"{"hook_event_name":"PermissionRequest"}"#.to_vec(),
        br#"{"hook_event_name":"PermissionRequest","tool_name":""}"#.to_vec(),
        br#"{"tool_name":"Bash"}"#.to_vec(),
        br#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":"x"}"#.to_vec(),
    ];
    for b in &bad {
        assert!(
            parse_payload(b).is_err(),
            "{:?}",
            String::from_utf8_lossy(b)
        );
    }
    let bad_questions = [
        json!({}),
        json!({"questions": []}),
        json!({"questions": "x"}),
        json!({"questions": [{"options": [{"label": "a"}]}]}),
        json!({"questions": [{"question": "q"}]}),
        json!({"questions": [{"question": "q", "options": [{"description": "no label"}]}]}),
        json!({"questions": [{"question": 3, "options": []}]}),
    ];
    for input in bad_questions {
        let raw = json!({"hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion",
                         "tool_input": input})
        .to_string();
        assert!(parse_payload(raw.as_bytes()).is_err(), "{raw}");
    }
}

// ---- route ----

#[test]
fn routes_every_table_row() {
    use Provider::*;
    use Route::*;
    let cases = [
        (Claude, "PermissionRequest", "AskUserQuestion", Ignore),
        (Claude, "PermissionRequest", "Bash", Permission),
        (Claude, "PermissionRequest", "mcp__srv__tool", Permission),
        (Claude, "PreToolUse", "AskUserQuestion", Question),
        (Claude, "PreToolUse", "Bash", Ignore),
        (Codex, "PermissionRequest", "shell", Permission),
        (Codex, "PermissionRequest", "AskUserQuestion", Permission),
        (Codex, "PreToolUse", "AskUserQuestion", Ignore),
        (Claude, "Stop", "Bash", Ignore),
        (Codex, "Notification", "shell", Ignore),
    ];
    for (provider, event, tool, want) in cases {
        assert_eq!(
            route(provider, &ev(event, tool)),
            want,
            "{provider:?} {event} {tool}"
        );
    }
    assert_eq!(Provider::parse("claude"), Some(Claude));
    assert_eq!(Provider::parse("codex"), Some(Codex));
    assert_eq!(Provider::parse("gemini"), None);
}

// ---- read_bounded ----

struct NeverEnds {
    first: Option<Vec<u8>>,
    _hold: mpsc::Sender<()>,
    block: mpsc::Receiver<()>,
}

impl Read for NeverEnds {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(b) = self.first.take() {
            buf[..b.len()].copy_from_slice(&b);
            return Ok(b.len());
        }
        // Blocks forever: the sender is held by this same reader.
        let _ = self.block.recv();
        Ok(0)
    }
}

#[test]
fn read_bounded_caps_and_reads_to_eof() {
    let data = vec![b'x'; 10_000];
    let got = read_bounded(
        std::io::Cursor::new(data.clone()),
        100,
        Duration::from_secs(5),
        false,
    );
    assert_eq!(got.len(), 100);
    let got = read_bounded(
        std::io::Cursor::new(data),
        1 << 20,
        Duration::from_secs(5),
        false,
    );
    assert_eq!(got.len(), 10_000);
}

#[test]
fn read_bounded_returns_partial_data_when_the_reader_never_finishes() {
    let (tx, rx) = mpsc::channel();
    let reader = NeverEnds {
        first: Some(br#"{"a":1}"#.to_vec()),
        _hold: tx,
        block: rx,
    };
    let got = read_bounded(reader, 1 << 20, Duration::from_millis(100), false);
    assert_eq!(got, br#"{"a":1}"#);
}

#[test]
fn read_bounded_skips_a_terminal() {
    let got = read_bounded(
        std::io::Cursor::new(b"abc".to_vec()),
        10,
        Duration::from_secs(1),
        true,
    );
    assert!(got.is_empty());
}

// ---- summarize / truncate_input ----

#[test]
fn summarizes_by_tool() {
    assert_eq!(
        summarize("Bash", &json!({"command": "cargo test\nrm -rf x"})),
        "cargo test"
    );
    let long = "a".repeat(500);
    assert_eq!(summarize("Bash", &json!({ "command": long })).len(), 200);
    assert_eq!(
        summarize("Edit", &json!({"file_path": "src/main.rs"})),
        "src/main.rs"
    );
    assert_eq!(
        summarize("Write", &json!({"file_path": "a.txt", "content": "x"})),
        "a.txt"
    );
    assert_eq!(
        summarize(
            "WebFetch",
            &json!({"url": "https://example.com", "prompt": "p"})
        ),
        "https://example.com"
    );
    assert_eq!(summarize("mcp__srv__do", &json!({"x": 1})), "mcp__srv__do");
    assert_eq!(
        summarize("shell", &json!({"command": ["git", "push"]})),
        "git push"
    );
    assert_eq!(summarize("AskUserQuestion", &question_input()), "Which db?");
    assert_eq!(
        summarize("Glob", &json!({"pattern": "*.rs"})),
        r#"{"pattern":"*.rs"}"#
    );
}

#[test]
fn truncates_long_leaves_and_collapses_huge_inputs() {
    let small = json!({"command": "ls"});
    assert_eq!(truncate_input(&small, INPUT_MAX), r#"{"command":"ls"}"#);

    // A multibyte leaf over 1024 bytes is cut on a char boundary with a marker.
    let leaf = "é".repeat(600); // 1200 bytes
    let out: Value =
        serde_json::from_str(&truncate_input(&json!({ "s": leaf }), INPUT_MAX)).unwrap();
    let s = out["s"].as_str().unwrap();
    assert!(s.ends_with(TRUNCATED_MARKER));
    assert_eq!(s.len(), 1024 + TRUNCATED_MARKER.len());
    assert!(s.starts_with("éé"));

    // Many leaves under the leaf cap still exceed the total: collapse.
    let many: Map<String, Value> = (0..20)
        .map(|i| (format!("k{i}"), Value::String("日".repeat(300))))
        .collect();
    let out = truncate_input(&Value::Object(many), INPUT_MAX);
    let v: Value = serde_json::from_str(&out).unwrap();
    let prefix = v["_truncated"].as_str().unwrap();
    assert!(
        prefix.len() <= 8000 && prefix.len() > 7990,
        "{}",
        prefix.len()
    );
    assert_eq!(v.as_object().unwrap().len(), 1);
}

// ---- outcome_from_row ----

#[test]
fn outcome_table_for_permissions() {
    let p = payload(&claude_bash());
    assert_eq!(
        outcome_from_row("permission", &row("allowed"), &p),
        Outcome::Allow
    );
    let mut d = row("denied");
    d.reason = "not on main".into();
    assert_eq!(
        outcome_from_row("permission", &d, &p),
        Outcome::Deny {
            message: "Denied by driver alpha: not on main".into()
        }
    );
    assert_eq!(
        outcome_from_row("permission", &row("denied"), &p),
        Outcome::Deny {
            message: "Denied by driver alpha".into()
        }
    );
    for s in [
        "answered",
        "escalated",
        "expired",
        "withdrawn",
        "pending",
        "bogus",
    ] {
        assert_eq!(
            outcome_from_row("permission", &row(s), &p),
            Outcome::Escalate,
            "{s}"
        );
    }
    let mut long = row("denied");
    long.reason = "ü".repeat(400);
    let Outcome::Deny { message } = outcome_from_row("permission", &long, &p) else {
        panic!("deny");
    };
    assert!(message.len() <= 500 && message.len() >= 499);
}

#[test]
fn outcome_table_for_questions() {
    let p = payload(&claude_question());
    assert_eq!(
        outcome_from_row("question", &row("allowed"), &p),
        Outcome::Escalate
    );
    assert!(matches!(
        outcome_from_row("question", &row("denied"), &p),
        Outcome::Deny { .. }
    ));
    let good = answered(json!({"Ship it?": "yes", "Which db?": "pg"}));
    assert_eq!(
        outcome_from_row("question", &good, &p),
        Outcome::Answer {
            answers: vec![
                ("Which db?".into(), "pg".into()),
                ("Ship it?".into(), "yes".into())
            ]
        }
    );
    let bad = [
        answered(json!({"Which db?": "pg"})), // partial
        answered(json!({"Which db?": "pg", "Ship it?": "yes", "Extra?": "x"})), // extra
        answered(json!({"Which db?": "pg", "Ship?": "yes"})), // wrong key
        answered(json!({"Which db?": "pg", "Ship it?": ""})), // empty
        answered(json!({"Which db?": "pg", "Ship it?": 1})), // not a string
        PromptRow {
            decision_json: Some("{not json".into()),
            ..row("answered")
        },
        PromptRow {
            decision_json: None,
            ..row("answered")
        },
    ];
    for r in &bad {
        assert_eq!(
            outcome_from_row("question", r, &p),
            Outcome::Escalate,
            "{r:?}"
        );
    }
    for s in ["escalated", "expired", "withdrawn", "pending", "bogus"] {
        assert_eq!(
            outcome_from_row("question", &row(s), &p),
            Outcome::Escalate,
            "{s}"
        );
    }
    // A free-text answer that matches no label passes through.
    let other = answered(json!({"Which db?": "duckdb", "Ship it?": "yes, no"}));
    assert!(matches!(
        outcome_from_row("question", &other, &p),
        Outcome::Answer { .. }
    ));
}

// ---- render ----

#[test]
fn renders_every_cell_byte_for_byte() {
    let bash = payload(&claude_bash());
    let q = payload(&claude_question());
    let codex = payload(&codex_permission());
    let deny = Outcome::Deny {
        message: "Denied by driver alpha: no".into(),
    };
    let deny_perm = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"Denied by driver alpha: no"}}}"#;

    use Provider::*;
    assert_eq!(
        render(Claude, Route::Permission, &Outcome::Allow, &bash).as_deref(),
        Some(ALLOW_PERMISSION)
    );
    assert_eq!(
        render(Codex, Route::Permission, &Outcome::Allow, &codex).as_deref(),
        Some(ALLOW_PERMISSION)
    );
    assert_eq!(
        render(Claude, Route::Permission, &deny, &bash).as_deref(),
        Some(deny_perm)
    );
    assert_eq!(
        render(Codex, Route::Permission, &deny, &codex).as_deref(),
        Some(deny_perm)
    );
    assert_eq!(
        render(Claude, Route::Question, &deny, &q).as_deref(),
        Some(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"Denied by driver alpha: no"}}"#
        )
    );

    let answer = Outcome::Answer {
        answers: vec![
            ("Which db?".into(), "pg".into()),
            ("Ship it?".into(), "yes".into()),
        ],
    };
    let out = render(Claude, Route::Question, &answer, &q).unwrap();
    let questions = serde_json::to_string(&question_input()["questions"]).unwrap();
    assert_eq!(
        out,
        format!(
            r#"{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecision":"allow","permissionDecisionReason":"Answered by driver","updatedInput":{{"questions":{questions},"answers":{{"Which db?":"pg","Ship it?":"yes"}}}}}}}}"#
        )
    );
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["updatedInput"]["questions"],
        question_input()["questions"]
    );

    // Not produced: escalate instead.
    assert_eq!(render(Claude, Route::Question, &Outcome::Allow, &q), None);
    assert_eq!(render(Claude, Route::Permission, &answer, &bash), None);
    assert_eq!(render(Codex, Route::Permission, &answer, &codex), None);
    for (p, r, pl) in [
        (Claude, Route::Permission, &bash),
        (Claude, Route::Question, &q),
        (Codex, Route::Permission, &codex),
    ] {
        assert_eq!(render(p, r, &Outcome::Escalate, pl), None);
    }
    assert_eq!(render(Claude, Route::Ignore, &Outcome::Allow, &bash), None);
}

#[test]
fn render_output_is_ascii_only() {
    let bash = payload(&claude_bash());
    let deny = Outcome::Deny {
        message: "nö 日 😀".into(),
    };
    let out = render(Provider::Claude, Route::Permission, &deny, &bash).unwrap();
    assert!(out.is_ascii());
    let escaped = ["n", "u00f6 ", "u65e5 ", "ud83d", "ude00"].join("\\");
    assert!(out.contains(&escaped), "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["decision"]["message"], "nö 日 😀");
}

// ---- run_gate ----

#[test]
fn undriven_worktree_prints_nothing_and_inserts_nothing() {
    let s = FakeStore::default();
    assert_eq!(gate(&s, Provider::Claude, &claude_bash()), None);
    assert!(s.calls.borrow().inserts.is_empty());
    assert!(writes(&s).is_empty());

    let s = FakeStore::default();
    assert_eq!(gate(&s, Provider::Codex, &codex_permission()), None);
    assert!(s.calls.borrow().inserts.is_empty());
    assert_eq!(writes(&s), ["bravo=waiting"]);
}

#[test]
fn ignored_route_prints_nothing_codex_still_writes_waiting() {
    let s = FakeStore::driven(300);
    let raw = json!({"hook_event_name":"PreToolUse","tool_name":"Bash"}).to_string();
    assert_eq!(gate(&s, Provider::Claude, raw.as_bytes()), None);
    assert!(writes(&s).is_empty());
    let raw = json!({"hook_event_name":"PreToolUse","tool_name":"shell"}).to_string();
    assert_eq!(gate(&s, Provider::Codex, raw.as_bytes()), None);
    assert_eq!(writes(&s), ["bravo=waiting"]);
    assert!(s.calls.borrow().inserts.is_empty());
}

#[test]
fn paused_or_offline_driver_escalates_without_a_row() {
    let s = FakeStore::driven(300);
    s.s.borrow_mut().drive.as_mut().unwrap().paused = true;
    assert_eq!(gate(&s, Provider::Codex, &codex_permission()), None);
    assert!(s.calls.borrow().inserts.is_empty());
    assert_eq!(writes(&s), ["bravo=waiting"]);

    for state in [None, Some("offline")] {
        let s = FakeStore::driven(300);
        match state {
            None => s.s.borrow_mut().sessions.clear(),
            Some(st) => {
                s.s.borrow_mut().sessions.insert("alpha".into(), st.into());
            }
        }
        assert_eq!(gate(&s, Provider::Claude, &claude_bash()), None);
        assert!(s.calls.borrow().inserts.is_empty());
    }
}

#[test]
fn driver_allow_is_printed_and_marks_busy() {
    let s = FakeStore::driven(300);
    s.decide_at(Duration::from_secs(3), row("allowed"));
    assert_eq!(
        gate(&s, Provider::Claude, &claude_bash()).as_deref(),
        Some(ALLOW_PERMISSION)
    );
    assert_eq!(writes(&s), ["bravo=waiting", "bravo=busy"]);
    let calls = s.calls.borrow();
    let ins = &calls.inserts[0];
    assert_eq!(
        (
            ins.driven.as_str(),
            ins.provider.as_str(),
            ins.kind.as_str(),
            ins.tool_name.as_str()
        ),
        ("bravo", "claude", "permission", "Bash")
    );
    assert_eq!(ins.summary, "cargo test");
    assert!(ins.hook_pid.is_some());
    assert!(calls.escalations.is_empty());
    // Decided within a tick or two of it landing.
    assert!(s.t.get() <= Duration::from_millis(3500));
}

#[test]
fn decision_already_present_at_insert_is_read_immediately() {
    let s = FakeStore::driven(300);
    s.decide_at(Duration::ZERO, row("allowed"));
    assert_eq!(
        gate(&s, Provider::Codex, &codex_permission()).as_deref(),
        Some(ALLOW_PERMISSION)
    );
    assert_eq!(s.t.get(), Duration::ZERO);
}

#[test]
fn driver_deny_and_answer_are_printed() {
    let s = FakeStore::driven(300);
    let mut d = row("denied");
    d.reason = "use a branch".into();
    s.decide_at(Duration::from_secs(1), d);
    let out = gate(&s, Provider::Codex, &codex_permission()).unwrap();
    assert!(out.contains(r#""behavior":"deny","message":"Denied by driver alpha: use a branch""#));
    assert_eq!(writes(&s), ["bravo=waiting", "bravo=busy"]);

    let s = FakeStore::driven(300);
    s.decide_at(
        Duration::from_secs(1),
        answered(json!({"Which db?": "pg", "Ship it?": "no"})),
    );
    let out = gate(&s, Provider::Claude, &claude_question()).unwrap();
    assert!(out.contains(r#""answers":{"Which db?":"pg","Ship it?":"no"}"#));
    assert_eq!(s.calls.borrow().inserts[0].kind, "question");
    assert_eq!(writes(&s), ["bravo=waiting", "bravo=busy"]);
}

#[test]
fn human_escalation_and_withdrawal_print_nothing_and_stay_waiting() {
    for status in ["escalated", "withdrawn", "expired"] {
        let s = FakeStore::driven(300);
        s.decide_at(Duration::from_secs(2), row(status));
        assert_eq!(gate(&s, Provider::Claude, &claude_bash()), None, "{status}");
        assert_eq!(writes(&s), ["bravo=waiting"], "{status}");
    }
}

#[test]
fn timeout_escalates_at_the_drive_setting() {
    let s = FakeStore::driven(300);
    assert_eq!(gate(&s, Provider::Claude, &claude_bash()), None);
    assert_eq!(s.t.get(), Duration::from_secs(300));
    assert_eq!(
        s.calls.borrow().escalations,
        [(
            "timeout".to_string(),
            "no driver decision in 300s".to_string()
        )]
    );
    assert_eq!(s.s.borrow().row.as_ref().unwrap().status, "escalated");
    assert_eq!(writes(&s), ["bravo=waiting"]);
}

#[test]
fn timeout_is_capped_at_540s() {
    let s = FakeStore::driven(9999);
    assert_eq!(gate(&s, Provider::Codex, &codex_permission()), None);
    assert_eq!(s.t.get(), Duration::from_secs(540));
    assert_eq!(
        s.calls.borrow().escalations[0].1,
        "no driver decision in 540s"
    );
}

#[test]
fn timeout_race_honours_the_driver_decision() {
    let s = FakeStore::driven(10);
    *s.on_escalate.borrow_mut() = Some(Box::new(|st: &mut State| st.row = Some(row("allowed"))));
    assert_eq!(
        gate(&s, Provider::Claude, &claude_bash()).as_deref(),
        Some(ALLOW_PERMISSION)
    );
    assert_eq!(s.s.borrow().row.as_ref().unwrap().status, "allowed");
    assert_eq!(writes(&s), ["bravo=waiting", "bravo=busy"]);
}

#[test]
fn drive_released_stolen_paused_or_driver_offline_mid_wait_escalates() {
    let changes: Vec<(&str, Event)> = vec![
        ("drive ended", Box::new(|st: &mut State| st.drive = None)),
        (
            "drive taken over by another driver",
            Box::new(|st: &mut State| st.drive.as_mut().unwrap().driver = "charlie".into()),
        ),
        (
            "drive paused",
            Box::new(|st: &mut State| st.drive.as_mut().unwrap().paused = true),
        ),
        (
            "driver offline",
            Box::new(|st: &mut State| {
                st.sessions.insert("alpha".into(), "offline".into());
            }),
        ),
    ];
    for (reason, change) in changes {
        let s = FakeStore::driven(300);
        s.at(Duration::from_secs(5), change);
        assert_eq!(gate(&s, Provider::Claude, &claude_bash()), None, "{reason}");
        assert_eq!(
            s.calls.borrow().escalations,
            [("system".to_string(), reason.to_string())]
        );
        assert!(s.t.get() < Duration::from_secs(6), "{reason}");
    }
}

#[test]
fn three_transient_errors_recover_a_fourth_escalates() {
    let s = FakeStore::driven(300);
    s.fail_dv.set(3);
    s.decide_at(Duration::from_secs(5), row("allowed"));
    assert_eq!(
        gate(&s, Provider::Claude, &claude_bash()).as_deref(),
        Some(ALLOW_PERMISSION)
    );

    let s = FakeStore::driven(300);
    s.fail_prompt.set(3);
    s.decide_at(Duration::from_secs(5), row("allowed"));
    assert_eq!(
        gate(&s, Provider::Claude, &claude_bash()).as_deref(),
        Some(ALLOW_PERMISSION)
    );

    let s = FakeStore::driven(300);
    s.fail_prompt.set(u32::MAX);
    s.decide_at(Duration::from_secs(5), row("allowed"));
    assert_eq!(gate(&s, Provider::Claude, &claude_bash()), None);
    assert_eq!(s.calls.borrow().escalations[0].0, "system");
    assert!(s.t.get() < Duration::from_secs(5));
    assert_eq!(writes(&s), ["bravo=waiting"]);
}

#[test]
fn unchanged_data_version_skips_selects_between_safety_ticks() {
    let s = FakeStore::driven(25);
    assert_eq!(gate(&s, Provider::Claude, &claude_bash()), None);
    let calls = s.calls.borrow();
    // Initial read, safety reads at 10s and 20s, and the post-deadline re-read.
    assert_eq!(calls.prompt, 4);
    // One read after the insert plus one per tick.
    assert_eq!(calls.dv, 1 + 50);
}

#[test]
fn insert_or_waiting_failure_prints_nothing() {
    let s = FakeStore::driven(300);
    s.fail_insert.set(true);
    assert_eq!(gate(&s, Provider::Claude, &claude_bash()), None);

    let s = FakeStore::driven(300);
    s.fail_waiting.set(true);
    s.decide_at(Duration::ZERO, row("allowed"));
    assert_eq!(gate(&s, Provider::Claude, &claude_bash()), None);
    assert_eq!(s.calls.borrow().escalations[0].0, "system");

    let s = FakeStore::driven(300);
    s.fail_drive_for.set(true);
    assert_eq!(gate(&s, Provider::Claude, &claude_bash()), None);
    assert!(s.calls.borrow().inserts.is_empty());
}

#[test]
fn only_an_allowed_permission_row_ever_prints_allow() {
    let statuses = [
        "denied",
        "escalated",
        "expired",
        "withdrawn",
        "pending",
        "bogus",
        "answered",
    ];
    for status in statuses {
        for (provider, raw) in [
            (Provider::Claude, claude_bash()),
            (Provider::Codex, codex_permission()),
        ] {
            let s = FakeStore::driven(5);
            let mut r = row(status);
            r.decision_json = Some(json!({"answers": {"x": "y"}, "behavior": "allow"}).to_string());
            s.decide_at(Duration::from_secs(1), r);
            let out = gate(&s, provider, &raw);
            assert!(
                !out.unwrap_or_default().contains("allow"),
                "{status} {provider:?}"
            );
        }
    }
    // Questions: an `allowed` row is unusable.
    let s = FakeStore::driven(5);
    s.decide_at(Duration::from_secs(1), row("allowed"));
    assert_eq!(gate(&s, Provider::Claude, &claude_question()), None);
    // Every error path.
    let s = FakeStore::driven(5);
    s.fail_prompt.set(u32::MAX);
    assert_eq!(gate(&s, Provider::Claude, &claude_bash()), None);
    let s = FakeStore::driven(5);
    s.fail_dv.set(u32::MAX);
    s.fail_prompt.set(u32::MAX);
    assert_eq!(gate(&s, Provider::Codex, &codex_permission()), None);
}

// ---- LocalStore impl over a temp-file DB ----

/// A clock whose first sleep lets a second connection (the driver's CLI) decide.
struct DecidingClock<F: Fn()> {
    t: Cell<Duration>,
    base: Instant,
    on_first_sleep: RefCell<Option<F>>,
}

impl<F: Fn()> Clock for DecidingClock<F> {
    fn now(&self) -> Instant {
        self.base + self.t.get()
    }
    fn sleep(&self, d: Duration) {
        if let Some(f) = self.on_first_sleep.borrow_mut().take() {
            f();
        }
        self.t.set(self.t.get() + d);
    }
}

#[test]
fn local_store_gate_sees_a_decision_from_another_connection() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("keitora-gate-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("tasks.db").to_string_lossy().into_owned();

    let hook = LocalStore::open(&db).unwrap();
    let driver = LocalStore::open(&db).unwrap();
    // Not driven yet: nothing brokered, nothing written for Claude.
    assert_eq!(GateStore::drive_for(&hook, "bravo").unwrap(), None);
    driver
        .drive_bind("alpha", "bravo", Some(60), false)
        .unwrap();
    driver.session_upsert("alpha", "busy", None, None).unwrap();
    assert_eq!(
        GateStore::session_state(&hook, "alpha").unwrap().as_deref(),
        Some("busy")
    );

    let clock = DecidingClock {
        t: Cell::new(Duration::ZERO),
        base: Instant::now(),
        on_first_sleep: RefCell::new(Some(|| {
            let p = driver
                .drive_prompts_pending(Some("alpha"), Some("bravo"))
                .unwrap();
            assert_eq!(p.len(), 1);
            assert_eq!(p[0].summary, "cargo test");
            driver
                .drive_prompt_decide(p[0].id, "alpha", &keitora_tasks::PromptDecision::Allow)
                .unwrap();
        })),
    };
    let out = run_gate(
        &hook,
        &clock,
        Provider::Claude,
        "bravo",
        &payload(&claude_bash()),
    );
    assert_eq!(out.as_deref(), Some(ALLOW_PERMISSION));
    assert_eq!(
        GateStore::session_state(&hook, "bravo").unwrap().as_deref(),
        Some("busy")
    );
    assert!(clock.t.get() <= Duration::from_secs(1));

    // A timeout escalates the row through the compare-and-set.
    let idle = DecidingClock {
        t: Cell::new(Duration::ZERO),
        base: Instant::now(),
        on_first_sleep: RefCell::new(None::<fn()>),
    };
    driver.drive_bind("alpha", "bravo", Some(2), true).unwrap();
    assert_eq!(
        run_gate(
            &hook,
            &idle,
            Provider::Claude,
            "bravo",
            &payload(&claude_bash())
        ),
        None
    );
    let rows = driver.drive_prompts_pending(None, Some("bravo")).unwrap();
    assert!(rows.is_empty());
    drop((hook, driver));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_decided_gate_leaves_a_parallel_gates_prompt_pending() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("tasks.db").to_string_lossy().into_owned();
    let hook = LocalStore::open(&db).unwrap();
    let driver = LocalStore::open(&db).unwrap();
    driver
        .drive_bind("alpha", "bravo", Some(60), false)
        .unwrap();
    driver.session_upsert("alpha", "busy", None, None).unwrap();
    // A parallel tool call's gate is already waiting on its own prompt.
    let other = driver
        .drive_prompt_insert(&NewDrivePrompt {
            driven: "bravo".into(),
            provider: "claude".into(),
            kind: "permission".into(),
            tool_name: "Bash".into(),
            tool_input: r#"{"command":"ls"}"#.into(),
            summary: "ls".into(),
            hook_pid: None,
        })
        .unwrap()
        .id;
    driver
        .session_upsert("bravo", "waiting", None, None)
        .unwrap();

    let clock = DecidingClock {
        t: Cell::new(Duration::ZERO),
        base: Instant::now(),
        on_first_sleep: RefCell::new(Some(|| {
            let mine = driver
                .drive_prompts_pending(Some("alpha"), Some("bravo"))
                .unwrap()
                .into_iter()
                .find(|p| p.id != other)
                .unwrap();
            driver
                .drive_prompt_decide(mine.id, "alpha", &keitora_tasks::PromptDecision::Allow)
                .unwrap();
        })),
    };
    let out = run_gate(
        &hook,
        &clock,
        Provider::Claude,
        "bravo",
        &payload(&claude_bash()),
    );
    assert_eq!(out.as_deref(), Some(ALLOW_PERMISSION));
    assert_eq!(
        driver.drive_prompt_get(other).unwrap().unwrap().status,
        "pending",
        "the parallel gate's prompt is not withdrawn"
    );
    assert_eq!(
        GateStore::session_state(&hook, "bravo").unwrap().as_deref(),
        Some("waiting")
    );
}

#[test]
fn undriven_check_treats_a_missing_db_as_not_driven() {
    let dir = std::env::temp_dir().join("keitora-gate-no-such-db");
    let path = dir.join("tasks.db");
    assert!(!is_driven(path.to_str().unwrap(), "bravo"));
    assert!(!path.exists(), "a read-only probe never creates the file");
}

// ---- question inputs are stored whole ----

fn long_question_payload(texts: &[String]) -> Vec<u8> {
    let questions: Vec<Value> = texts
        .iter()
        .map(|t| json!({"question": t, "options": [{"label": "pg"}, {"label": "sqlite"}]}))
        .collect();
    json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "AskUserQuestion",
        "tool_input": {"questions": questions},
    })
    .to_string()
    .into_bytes()
}

#[test]
fn answers_map_by_text_index_or_truncated_stem() {
    let p = payload(&claude_question());
    let want = Outcome::Answer {
        answers: vec![
            ("Which db?".into(), "pg".into()),
            ("Ship it?".into(), "yes".into()),
        ],
    };
    for answers in [
        json!({"1": "pg", "2": "yes"}),
        json!({"Which db?": "pg", "2": "yes"}),
        json!({ format!("Which{TRUNCATED_MARKER}"): "pg", "Ship it?": "yes" }),
    ] {
        assert_eq!(
            outcome_from_row("question", &answered(answers.clone()), &p),
            want,
            "{answers}"
        );
    }
    for answers in [
        json!({"1": "pg", "Which db?": "sqlite", "2": "yes"}), // same question twice
        json!({"1": "pg", "3": "yes"}),                        // no question 3
        json!({ format!("{TRUNCATED_MARKER}"): "pg", "2": "yes" }), // empty stem
    ] {
        assert_eq!(
            outcome_from_row("question", &answered(answers.clone()), &p),
            Outcome::Escalate,
            "{answers}"
        );
    }
}

#[test]
fn question_inputs_are_stored_whole_up_to_the_store_cap() {
    // Long leaves and a total well over the permission cap are kept verbatim.
    let long = long_question_payload(&["q".repeat(3000), "r".repeat(6000)]);
    let p = payload(&long);
    let stored = stored_input(Route::Question, &p.tool_input).unwrap();
    assert!(stored.len() > 8192);
    assert_eq!(
        serde_json::from_str::<Value>(&stored).unwrap(),
        p.tool_input
    );
    // Permission inputs are still cut.
    let perm = stored_input(Route::Permission, &json!({"s": "x".repeat(5000)})).unwrap();
    assert!(perm.len() < 2048);

    // Over the store's cap: nothing is stored, the human answers it.
    let huge = long_question_payload(&["z".repeat(DRIVE_QUESTION_INPUT_MAX)]);
    assert_eq!(
        stored_input(Route::Question, &payload(&huge).tool_input),
        None
    );
    let store = FakeStore::driven(60);
    assert_eq!(gate(&store, Provider::Claude, &huge), None);
    assert!(store.calls.borrow().inserts.is_empty());
    assert!(writes(&store).is_empty());
}

#[test]
fn a_long_question_round_trips_through_the_store_and_the_driver() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("tasks.db").to_string_lossy().into_owned();
    let hook = LocalStore::open(&db).unwrap();
    let driver = LocalStore::open(&db).unwrap();
    driver
        .drive_bind("alpha", "bravo", Some(60), false)
        .unwrap();
    driver.session_upsert("alpha", "busy", None, None).unwrap();
    let texts = vec![
        format!("{}?", "which db ".repeat(400)),
        "Ship it?".to_string(),
    ];
    let raw = long_question_payload(&texts);

    let clock = DecidingClock {
        t: Cell::new(Duration::ZERO),
        base: Instant::now(),
        on_first_sleep: RefCell::new(Some(|| {
            let p = driver
                .drive_prompts_pending(Some("alpha"), Some("bravo"))
                .unwrap()
                .remove(0);
            let questions = crate::drive::parse_questions(&p.tool_input).unwrap();
            assert_eq!(questions[0].question, texts[0], "stored whole");
            let answers =
                crate::drive::parse_choices(&questions, &["1=pg".into(), "2=yes".into()]).unwrap();
            driver
                .drive_prompt_decide(
                    p.id,
                    "alpha",
                    &keitora_tasks::PromptDecision::Answer { answers },
                )
                .unwrap();
        })),
    };
    let out = run_gate(&hook, &clock, Provider::Claude, "bravo", &payload(&raw)).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let answers = &v["hookSpecificOutput"]["updatedInput"]["answers"];
    assert_eq!(answers[&texts[0]], "pg");
    assert_eq!(answers["Ship it?"], "yes");
}
