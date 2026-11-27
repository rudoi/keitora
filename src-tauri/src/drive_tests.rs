//! Tests for the drive request runner and commands. Every test uses a temp-file DB, a
//! `FakeBackend` and a fake host; nothing touches herdr, a window or the real config.

use std::sync::Mutex as StdMutex;

use keitora_tasks::{NewDrivePrompt, WorktreeEntry};

use super::*;
use crate::backend::fake::{Call, FakeBackend};
use crate::backend::PtyBackendError;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Records what the runner asked of the app.
#[derive(Default)]
struct FakeHost {
    /// `restart`, `swap`, `worktrees_changed`, `persist:<cli>`, `prepare:<cli>` in order.
    log: StdMutex<Vec<String>>,
    restarts: StdMutex<Vec<DriveRestartEvent>>,
    swaps: StdMutex<Vec<DriveSwapEvent>>,
    /// Each worktree's provider (claude when absent).
    cli: StdMutex<HashMap<String, CliKind>>,
    persist_fails: StdMutex<bool>,
    /// Follow-up passes asked for, as (project, delay).
    scheduled: StdMutex<Vec<(String, Duration)>>,
}

impl FakeHost {
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    fn scheduled(&self) -> Vec<(String, Duration)> {
        self.scheduled.lock().unwrap().clone()
    }
    fn cli_of(&self, wt: &str) -> CliKind {
        self.cli
            .lock()
            .unwrap()
            .get(wt)
            .copied()
            .unwrap_or(CliKind::Claude)
    }
}

impl DriveHost for FakeHost {
    fn emit_restart(&self, ev: &DriveRestartEvent) {
        self.log.lock().unwrap().push("restart".into());
        self.restarts.lock().unwrap().push(ev.clone());
    }
    fn emit_swap(&self, ev: &DriveSwapEvent) {
        self.log.lock().unwrap().push("swap".into());
        self.swaps.lock().unwrap().push(ev.clone());
    }
    fn worktrees_changed(&self) {
        self.log.lock().unwrap().push("worktrees_changed".into());
    }
    fn current_cli(&self, _project: &str, _config: &ProjectConfig, worktree: &str) -> CliKind {
        self.cli_of(worktree)
    }
    fn persist_cli(
        &self,
        _config: &ProjectConfig,
        worktree: &str,
        cli: CliKind,
    ) -> Result<(), String> {
        if *self.persist_fails.lock().unwrap() {
            return Err("disk full".into());
        }
        self.cli.lock().unwrap().insert(worktree.to_string(), cli);
        self.log
            .lock()
            .unwrap()
            .push(format!("persist:{}", cli.spec().config_key));
        Ok(())
    }
    fn prepare_launch(&self, _config: &ProjectConfig, _worktree: &str, cli: CliKind) {
        self.log
            .lock()
            .unwrap()
            .push(format!("prepare:{}", cli.spec().config_key));
    }
    fn schedule_pass(&self, project: &str, after: Duration) {
        self.scheduled
            .lock()
            .unwrap()
            .push((project.to_string(), after));
    }
}

const WINDOW: Duration = Duration::from_millis(300);

struct Fx {
    _dir: tempfile::TempDir,
    env: Arc<DriveEnv>,
    fake: Arc<FakeBackend>,
    host: Arc<FakeHost>,
    gate: DispatchGate,
}

/// Project `proj` with worktrees alpha/bravo/charlie, `alpha` driving `bravo`, and the
/// window reporting `proj` open.
fn fixture() -> Fx {
    fx_with(Timeouts {
        max_age_secs: DRIVE_REQUEST_MAX_AGE_SECS,
        reset: WINDOW,
        swap_confirm: WINDOW,
        swap_run: WINDOW,
        send_settle: SEND_SETTLE_TIMEOUT,
    })
}

fn fx_with(timeouts: Timeouts) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let mut config = ProjectConfig::scaffold(&root);
    config.effective_worktrees = Some(
        ["alpha", "bravo", "charlie"]
            .into_iter()
            .map(WorktreeEntry::name_only)
            .collect(),
    );
    let db_path = dir.path().join("tasks.db").to_string_lossy().into_owned();
    let local = LocalStore::open(&db_path).unwrap();
    local.drive_bind("alpha", "bravo", None, false).unwrap();
    let fake = Arc::new(FakeBackend::new());
    let host = Arc::new(FakeHost::default());
    let runtime = DriveRuntime::default();
    runtime.set_open(vec!["proj".to_string()]);
    let env = Arc::new(DriveEnv {
        project: config.project.name.clone(),
        config,
        db_path,
        backend: fake.clone(),
        registry: TransferRegistry::new(),
        runtime,
        host: host.clone(),
        timeouts,
    });
    assert_eq!(env.project, "proj");
    Fx {
        _dir: dir,
        env,
        fake,
        host,
        gate: DispatchGate::default(),
    }
}

impl Fx {
    fn store(&self) -> LocalStore {
        LocalStore::open(&self.env.db_path).unwrap()
    }
    fn key(&self, wt: &str) -> SessionKey {
        self.env.key(wt)
    }
    /// Record `wt`'s hook state and mark its pane live.
    fn session(&self, wt: &str, state: &str) {
        self.store().session_upsert(wt, state, None, None).unwrap();
        self.fake.insert_live(self.key(wt));
    }
    fn add(&self, kind: &str, payload: serde_json::Value, queue: bool) -> i64 {
        self.store()
            .drive_request_add(kind, "bravo", "alpha", &payload, queue)
            .unwrap()
            .id
    }
    fn row(&self, id: i64) -> DriveRequest {
        self.store().drive_request_get(id).unwrap().unwrap()
    }
    fn drive(&self) -> Drive {
        self.store().drive_get("bravo").unwrap().unwrap()
    }
    fn pass(&self) -> Vec<Job> {
        tauri::async_runtime::block_on(run_pass(&self.env, &self.gate))
    }
    /// Run a pass and finish its spawned jobs (no window reports).
    fn run(&self) {
        for job in self.pass() {
            tauri::async_runtime::block_on(job);
        }
    }
    fn writes(&self) -> Vec<String> {
        self.fake
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::WriteLine { text, .. } => Some(format!("line:{text}")),
                Call::Paste { text, .. } => Some(format!("paste:{text}")),
                _ => None,
            })
            .collect()
    }
    fn prompt(&self, wt: &str) -> i64 {
        self.store()
            .drive_prompt_insert(&NewDrivePrompt {
                driven: wt.to_string(),
                provider: "claude".to_string(),
                kind: "permission".to_string(),
                tool_name: "Bash".to_string(),
                tool_input: "{}".to_string(),
                summary: "ls".to_string(),
                hook_pid: None,
            })
            .unwrap()
            .id
    }
}

fn applied(row: &DriveRequest) -> serde_json::Value {
    assert_eq!(row.status, "applied", "reason: {}", row.reason);
    serde_json::from_str(&row.result).unwrap()
}

fn refused(row: &DriveRequest) -> &str {
    assert_eq!(row.status, "refused", "result: {}", row.result);
    &row.reason
}

/// Run one pass whose single job waits on the window, while `window` plays the
/// window's part (it gets the runtime and the host once the job has emitted).
fn run_with_window(fx: &Fx, window: impl FnOnce(&DriveRuntime, &FakeHost) + Send + 'static) {
    let mut jobs = fx.pass();
    assert_eq!(jobs.len(), 1, "one reset/swap job");
    let job = jobs.pop().unwrap();
    let runtime = fx.env.runtime.clone();
    let host = fx.host.clone();
    tauri::async_runtime::block_on(async move {
        let handle = tauri::async_runtime::spawn(job);
        for _ in 0..200 {
            if !host.restarts.lock().unwrap().is_empty() || !host.swaps.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        window(&runtime, &host);
        handle.await.unwrap();
    });
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn payloads_parse_per_kind_and_bad_json_is_refused() {
    assert_eq!(
        parse_request("send", r#"{"text":"hi"}"#),
        Ok(Parsed::Send {
            text: "hi".into(),
            wake: false
        })
    );
    let reset = Parsed::Restart {
        to: None,
        mode: RestartMode::Reset,
        fresh: true,
    };
    assert_eq!(parse_request("reset", "{}"), Ok(reset.clone()));
    assert_eq!(
        parse_request("send", r#"{"text":"hi","wake":true}"#),
        Ok(Parsed::Send {
            text: "hi".into(),
            wake: true
        })
    );
    assert_eq!(parse_request("reset", r#"{"mode":"reset"}"#), Ok(reset));
    let woke = Parsed::Restart {
        to: None,
        mode: RestartMode::Wake,
        fresh: false,
    };
    assert_eq!(parse_request("wake", "{}"), Ok(woke.clone()));
    assert_eq!(
        parse_request("wake", r#"{"fresh":true,"to":"codex"}"#),
        Ok(Parsed::Restart {
            to: Some("codex".into()),
            mode: RestartMode::Wake,
            fresh: true
        })
    );
    // A row from an older CLI that filed a wake as a reset.
    assert_eq!(parse_request("reset", r#"{"mode":"wake"}"#), Ok(woke));
    assert_eq!(
        parse_request("reset", r#"{"mode":"wake","fresh":true,"to":"codex"}"#),
        Ok(Parsed::Restart {
            to: Some("codex".into()),
            mode: RestartMode::Wake,
            fresh: true
        })
    );
    assert_eq!(
        parse_request("reset", r#"{"mode":"nap"}"#),
        Err("unknown reset mode `nap`".to_string())
    );
    assert_eq!(
        parse_request("swap", r#"{"to":"codex"}"#),
        Ok(Parsed::Swap { to: "codex".into() })
    );
    for (kind, payload) in [
        ("send", "{"),
        ("send", "{}"),
        ("swap", "{}"),
        ("read", r#"{"lines":"x"}"#),
        ("wake", r#"{"fresh":"yes"}"#),
    ] {
        let e = parse_request(kind, payload).unwrap_err();
        assert!(
            e.starts_with("bad request payload: "),
            "{kind} {payload}: {e}"
        );
    }
    assert_eq!(
        parse_request("dance", "{}"),
        Err("unknown request kind `dance`".to_string())
    );
}

#[test]
fn read_lines_default_and_clamp() {
    assert_eq!(clamp_lines(None), 200);
    assert_eq!(clamp_lines(Some(0)), 1);
    assert_eq!(clamp_lines(Some(-5)), 1);
    assert_eq!(clamp_lines(Some(50)), 50);
    assert_eq!(clamp_lines(Some(5000)), 2000);
    assert_eq!(
        parse_request("read", r#"{"lines":9999}"#),
        Ok(Parsed::Read { lines: 2000 })
    );
}

#[test]
fn providers_parse_and_unknown_lists_every_provider() {
    assert_eq!(parse_provider("claude"), Ok(CliKind::Claude));
    assert_eq!(parse_provider("codex"), Ok(CliKind::Codex));
    // Antigravity by its config name or its executable name.
    assert_eq!(parse_provider("antigravity"), Ok(CliKind::Antigravity));
    assert_eq!(parse_provider("agy"), Ok(CliKind::Antigravity));
    assert_eq!(
        parse_provider("zeta"),
        Err("unknown provider `zeta`; expected one of: claude, codex, antigravity (agy)".into())
    );
    // Every provider parses back from its wire name.
    for c in CliKind::ALL {
        assert_eq!(parse_provider(c.spec().config_key), Ok(c));
    }
}

#[test]
fn wake_marks_of_requests_no_longer_pending_are_pruned() {
    let rt = DriveRuntime::default();
    rt.mark_woken("proj", 1);
    rt.mark_woken("proj", 2);
    rt.mark_woken("other-proj", 2);
    let due = Instant::now();
    assert!(rt.claim_recheck("proj", 2, due));
    rt.retain_woken("proj", &HashSet::from([1]));
    assert!(rt.wake_mark("proj", 1).is_some(), "still pending");
    assert!(rt.wake_mark("proj", 2).is_none(), "no longer pending");
    assert!(rt.wake_mark("other-proj", 2).is_some(), "another project's");
    assert!(rt.claim_recheck("proj", 2, due), "its recheck went too");
}

#[test]
fn a_slot_wait_never_downgrades_or_counts_as_an_own_wake() {
    let rt = DriveRuntime::default();
    rt.mark_slot_wait("proj", 1);
    assert!(!rt.wake_mark("proj", 1).unwrap().own);
    rt.mark_woken("proj", 1);
    rt.mark_slot_wait("proj", 1);
    assert!(rt.wake_mark("proj", 1).unwrap().own, "its own wake is kept");
}

#[test]
fn one_recheck_serves_every_pass_that_asks_for_the_same_deadline() {
    let rt = DriveRuntime::default();
    let due = Instant::now() + Duration::from_secs(5);
    assert!(rt.claim_recheck("proj", 1, due));
    assert!(!rt.claim_recheck("proj", 1, due), "deduped");
    assert!(!rt.claim_recheck("proj", 1, due - Duration::from_secs(1)));
    assert!(rt.claim_recheck("proj", 2, due), "per request");
    assert!(
        rt.claim_recheck("proj", 1, due + Duration::from_secs(1)),
        "later"
    );
    rt.forget_woken("proj", 1);
    assert!(
        rt.claim_recheck("proj", 1, due),
        "forgotten with the request"
    );
}

#[test]
fn single_line_prompts_collapse_control_chars() {
    assert_eq!(
        sanitize_prompt("run\tthe \u{1b}[31mtests\x07  now"),
        Ok(Prompt::Line("run the [31mtests now".into()))
    );
    // A trailing line ending is not a second line.
    assert_eq!(sanitize_prompt("go\r\n"), Ok(Prompt::Line("go".into())));
    for empty in ["", "   ", "\n\n", "\u{1b}"] {
        assert_eq!(
            sanitize_prompt(empty),
            Err("empty prompt".into()),
            "{empty:?}"
        );
    }
    let long = "x".repeat(DRIVE_SEND_TEXT_MAX + 1);
    assert_eq!(
        sanitize_prompt(&long),
        Err("prompt too long (max 32 KiB)".into())
    );
}

#[test]
fn multi_line_prompts_keep_newlines_only() {
    assert_eq!(
        sanitize_prompt("first\r\nsecond\rthird\t\u{1b}x  \n"),
        Ok(Prompt::Block("first\nsecond\nthird x".into()))
    );
}

#[test]
fn state_refusals_name_the_state() {
    assert_eq!(state_refusal("idle", "bravo"), None);
    assert_eq!(
        state_refusal("busy", "bravo").unwrap(),
        "@bravo is busy; send only goes to an idle session (use --queue to wait for it)"
    );
    assert_eq!(
        state_refusal("waiting", "bravo").unwrap(),
        "@bravo is waiting on a prompt; answer it first (or use --queue)"
    );
    assert_eq!(
        state_refusal("offline", "bravo").unwrap(),
        "@bravo is offline; start its session first"
    );
}

#[test]
fn read_results_keep_the_tail_on_a_char_boundary() {
    assert_eq!(tail_cap("short", 64), "short");
    let text = format!("{}{}", "a".repeat(100), "é".repeat(50)); // 200 bytes
    let cut = tail_cap(&text, 60);
    assert!(cut.len() <= 60, "{}", cut.len());
    assert!(cut.starts_with(TRUNCATED_MARKER));
    assert!(cut.ends_with("éé"));
    assert!(text.ends_with(&cut[TRUNCATED_MARKER.len()..]));
    // The real cap.
    let big = "z".repeat(DRIVE_READ_RESULT_MAX * 2);
    let cut = tail_cap(&big, DRIVE_READ_RESULT_MAX);
    assert_eq!(cut.len(), DRIVE_READ_RESULT_MAX);
}

#[test]
fn swap_needs_the_other_of_claude_and_codex() {
    assert_eq!(swap_refusal("bravo", CliKind::Claude, CliKind::Codex), None);
    assert_eq!(swap_refusal("bravo", CliKind::Codex, CliKind::Claude), None);
    assert_eq!(
        swap_refusal("bravo", CliKind::Codex, CliKind::Codex),
        Some("@bravo is already on codex".into())
    );
    // Antigravity is never a transfer pair, in either direction.
    assert_eq!(
        swap_refusal("bravo", CliKind::Claude, CliKind::Antigravity),
        Some(
            "swap can't carry context from claude to antigravity; use reset --to antigravity instead"
                .into()
        )
    );
    assert_eq!(
        swap_refusal("bravo", CliKind::Antigravity, CliKind::Codex),
        Some(
            "swap can't carry context from antigravity to codex; use reset --to codex instead"
                .into()
        )
    );
}

#[test]
fn age_and_duration_helpers() {
    let now = chrono::NaiveDateTime::parse_from_str("2026-01-01 00:20:00", "%Y-%m-%d %H:%M:%S")
        .unwrap()
        .and_utc();
    assert!(is_older_than("2026-01-01 00:09:59", now, 600));
    assert!(!is_older_than("2026-01-01 00:10:00", now, 600));
    assert!(!is_older_than("not a time", now, 600));
    assert_eq!(human_duration(Duration::from_secs(300)), "5 minutes");
    assert_eq!(human_duration(Duration::from_secs(60)), "1 minute");
    assert_eq!(human_duration(Duration::from_secs(120)), "2 minutes");
    assert_eq!(human_duration(Duration::from_secs(90)), "90 s");
}

fn drive_row(driver: &str, paused: bool) -> Drive {
    Drive {
        driven: "bravo".into(),
        driver: driver.into(),
        since: "now".into(),
        forced_from: None,
        paused,
        escalate_after_s: 300,
        awaiting_turn: None,
    }
}

#[test]
fn common_gates_refuse_in_order_with_exact_reasons() {
    let open: HashSet<String> = ["proj".to_string()].into();
    let configured = vec!["alpha".to_string(), "bravo".to_string()];
    let driven = drive_row("alpha", false);
    let paused = drive_row("alpha", true);
    let other = drive_row("charlie", false);
    let transfer = Some(
        "a transfer or switch is in progress for worktree `bravo`; try again when it finishes"
            .to_string(),
    );
    let check = |kind: &str,
                 project: &str,
                 wt: &str,
                 drive: Option<&Drive>,
                 transfer: Option<String>,
                 terminated: bool| {
        common_refusal(
            kind,
            project,
            wt,
            "alpha",
            &GateFacts {
                open: &open,
                configured: &configured,
                drive,
                transfer,
                terminated,
            },
        )
    };
    assert_eq!(
        check("send", "proj", "bravo", Some(&driven), None, false),
        None
    );
    assert_eq!(
        check("send", "other-proj", "bravo", Some(&driven), None, false).unwrap(),
        "project not open in Keitora"
    );
    assert_eq!(
        check("send", "proj", "zulu", Some(&driven), None, false).unwrap(),
        "unknown worktree `zulu`; expected one of: alpha, bravo"
    );
    assert_eq!(
        check("send", "proj", "bravo", None, None, false).unwrap(),
        "@bravo is not driven by @alpha"
    );
    assert_eq!(
        check("send", "proj", "bravo", Some(&other), None, false).unwrap(),
        "@bravo is not driven by @alpha"
    );
    for kind in ["send", "reset", "swap"] {
        assert_eq!(
            check(kind, "proj", "bravo", Some(&paused), None, false).unwrap(),
            "drive of @bravo is paused; resume it before sending"
        );
        assert_eq!(
            check(
                kind,
                "proj",
                "bravo",
                Some(&driven),
                transfer.clone(),
                false
            ),
            transfer.clone()
        );
    }
    // A read is allowed while paused and mid-transfer.
    assert_eq!(
        check(
            "read",
            "proj",
            "bravo",
            Some(&paused),
            transfer.clone(),
            false
        ),
        None
    );
    for kind in ["send", "read", "reset", "swap"] {
        assert_eq!(
            check(kind, "proj", "bravo", Some(&driven), None, true).unwrap(),
            "@bravo is terminated; launch it from the rail first"
        );
    }
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

#[test]
fn open_set_reports_change_only_when_it_changes() {
    let rt = DriveRuntime::default();
    assert_eq!(rt.open_set(), None);
    assert!(rt.set_open(vec!["proj".into()]), "first report");
    assert!(!rt.set_open(vec!["proj".into()]), "same set");
    assert!(rt.set_open(vec!["proj".into(), "other-proj".into()]));
    assert!(rt.set_open(vec![]), "emptied");
    assert_eq!(rt.open_set(), Some(HashSet::new()));
}

#[test]
fn report_errors_without_a_waiter_or_with_a_bad_outcome() {
    let rt = DriveRuntime::default();
    assert_eq!(
        rt.report("proj", 7, "applied", String::new()),
        Err("request is no longer waiting for Keitora".into())
    );
    let (mut rx, guard) = rt.register_waiter("proj", 7, WaitKind::Reset);
    assert_eq!(
        rt.report("proj", 7, "done", String::new()),
        Err("outcome must be accepted, applied or refused".into())
    );
    assert_eq!(
        rt.report("proj", 7, "accepted", String::new()),
        Err("only a swap is confirmed".into())
    );
    rt.report("proj", 7, "refused", "nope".into()).unwrap();
    assert_eq!(rx.try_recv().unwrap(), DriveReport::Refused("nope".into()));
    // Another project's id doesn't match.
    assert!(rt
        .report("other-proj", 7, "applied", String::new())
        .is_err());
    drop(guard);
    assert!(rt.report("proj", 7, "applied", String::new()).is_err());

    let (mut rx, _g) = rt.register_waiter("proj", 8, WaitKind::Swap);
    rt.report("proj", 8, "accepted", String::new()).unwrap();
    assert_eq!(rx.try_recv().unwrap(), DriveReport::Accepted);
}

#[test]
fn a_worktree_holds_one_reset_or_swap_slot() {
    let rt = DriveRuntime::default();
    let first = rt.claim_worktree("proj", "bravo").expect("free");
    assert!(rt.claim_worktree("proj", "bravo").is_none());
    assert!(rt.claim_worktree("other-proj", "bravo").is_some());
    drop(first);
    assert!(rt.claim_worktree("proj", "bravo").is_some());
}

// ---------------------------------------------------------------------------
// Runner: open set
// ---------------------------------------------------------------------------

#[test]
fn requests_stay_pending_until_the_window_reports() {
    let fx = fixture();
    // A fresh runtime: no report yet.
    let env = Arc::new(DriveEnv {
        project: fx.env.project.clone(),
        config: fx.env.config.clone(),
        db_path: fx.env.db_path.clone(),
        backend: fx.fake.clone(),
        registry: TransferRegistry::new(),
        runtime: DriveRuntime::default(),
        host: fx.host.clone(),
        timeouts: fx.env.timeouts,
    });
    fx.session("bravo", "idle");
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    let jobs = tauri::async_runtime::block_on(run_pass(&env, &fx.gate));
    assert!(jobs.is_empty());
    assert_eq!(fx.row(id).status, "pending");
    assert!(fx.writes().is_empty());

    // Once reported, the same request runs.
    env.runtime.set_open(vec!["proj".into()]);
    tauri::async_runtime::block_on(run_pass(&env, &fx.gate));
    applied(&fx.row(id));
}

#[test]
fn a_project_the_window_does_not_have_open_is_refused() {
    let fx = fixture();
    fx.env.runtime.set_open(vec!["other-proj".into()]);
    fx.session("bravo", "idle");
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    let queued = fx.add("send", serde_json::json!({"text": "later"}), true);
    fx.run();
    assert_eq!(refused(&fx.row(id)), "project not open in Keitora");
    assert_eq!(refused(&fx.row(queued)), "project not open in Keitora");
    assert!(fx.writes().is_empty());
}

// ---------------------------------------------------------------------------
// Runner: send
// ---------------------------------------------------------------------------

#[test]
fn send_to_an_idle_session_writes_one_collapsed_line() {
    let fx = fixture();
    fx.session("bravo", "idle");
    let id = fx.add(
        "send",
        serde_json::json!({"text": "fix \u{1b}[2Jthe\tbug"}),
        false,
    );
    fx.run();
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"awaiting_turn": 1})
    );
    assert_eq!(fx.writes(), vec!["line:fix [2Jthe bug".to_string()]);
    let delays: Vec<u32> = fx
        .fake
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::WriteLine {
                submit_delay_ms, ..
            } => Some(submit_delay_ms),
            _ => None,
        })
        .collect();
    assert_eq!(delays, vec![60]);
    assert_eq!(fx.drive().awaiting_turn, Some(1));

    // The awaited turn follows the last finished one.
    fx.store().session_reply_record("bravo", "done").unwrap();
    let id = fx.add("send", serde_json::json!({"text": "next"}), false);
    fx.run();
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"awaiting_turn": 2})
    );
}

#[test]
fn multi_line_send_pastes_then_submits() {
    let fx = fixture();
    fx.session("bravo", "idle");
    let id = fx.add("send", serde_json::json!({"text": "one\r\ntwo\n"}), false);
    fx.run();
    applied(&fx.row(id));
    assert_eq!(
        fx.writes(),
        vec!["paste:one\ntwo".to_string(), "line:".to_string()]
    );
}

#[test]
fn send_refuses_sessions_that_are_not_idle() {
    for (state, live, want) in [
        (
            "busy",
            true,
            "@bravo is busy; send only goes to an idle session (use --queue to wait for it)",
        ),
        (
            "waiting",
            true,
            "@bravo is waiting on a prompt; answer it first (or use --queue)",
        ),
        ("idle", false, "@bravo is offline; start its session first"),
    ] {
        let fx = fixture();
        fx.store()
            .session_upsert("bravo", state, None, None)
            .unwrap();
        if live {
            fx.fake.insert_live(fx.key("bravo"));
        }
        let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
        fx.run();
        assert_eq!(refused(&fx.row(id)), want, "{state}");
        assert!(fx.writes().is_empty());
        assert_eq!(fx.drive().awaiting_turn, None, "{state}: nothing awaited");
    }
}

#[test]
fn a_queued_send_waits_unbegun_until_idle() {
    let fx = fixture();
    fx.session("bravo", "busy");
    let id = fx.add("send", serde_json::json!({"text": "after"}), true);
    assert!(fx.pass().is_empty());
    assert_eq!(fx.row(id).status, "pending", "not begun");
    assert!(fx.writes().is_empty());

    fx.session("bravo", "idle");
    fx.run();
    applied(&fx.row(id));
    assert_eq!(fx.writes(), vec!["line:after".to_string()]);
}

#[test]
fn a_queued_send_expires_when_it_waits_too_long() {
    let fx = fx_with(Timeouts {
        max_age_secs: 0,
        reset: WINDOW,
        swap_confirm: WINDOW,
        swap_run: WINDOW,
        send_settle: SEND_SETTLE_TIMEOUT,
    });
    fx.session("bravo", "busy");
    let id = fx.add("send", serde_json::json!({"text": "after"}), true);
    // `created_at` has one-second resolution.
    std::thread::sleep(Duration::from_millis(1100));
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "expired: no running Keitora app picked it up in time"
    );
}

#[test]
fn a_failed_write_restores_the_awaited_turn() {
    let fx = fixture();
    fx.session("bravo", "idle");
    // Nothing awaited before: restored to none.
    fx.fake.push_write_line(Err(PtyBackendError::NoSuchSession));
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "could not send to @bravo (no live session)"
    );
    assert_eq!(fx.drive().awaiting_turn, None);

    // A previous value comes back.
    fx.store().drive_set_awaiting_turn("bravo").unwrap();
    fx.fake.push_write_line(Err(PtyBackendError::NotConnected));
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "could not send to @bravo (herdr unreachable)"
    );
    assert_eq!(fx.drive().awaiting_turn, Some(1));

    // A failed paste is a failed send too.
    fx.fake
        .push_paste(Err(PtyBackendError::PasteTooLarge { bytes: 2, limit: 1 }));
    let id = fx.add("send", serde_json::json!({"text": "a\nb"}), false);
    fx.run();
    assert!(refused(&fx.row(id)).starts_with("could not send to @bravo ("));
    assert_eq!(fx.drive().awaiting_turn, Some(1));
}

#[test]
fn a_paste_whose_enter_fails_restores_the_awaited_turn() {
    let fx = fixture();
    fx.session("bravo", "idle");
    fx.fake.push_write_line(Err(PtyBackendError::NoSuchSession));
    let id = fx.add("send", serde_json::json!({"text": "a\nb"}), false);
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "pasted but could not submit to @bravo (no live session); \
         the text may be left in its input box"
    );
    assert_eq!(
        fx.writes(),
        vec!["paste:a\nb".to_string(), "line:".to_string()]
    );
    assert_eq!(fx.drive().awaiting_turn, None, "nothing was submitted");

    // A previous value comes back, and the failed send doesn't hold the worktree.
    fx.store().drive_set_awaiting_turn("bravo").unwrap();
    fx.fake.push_write_line(Err(PtyBackendError::NotConnected));
    let id = fx.add("send", serde_json::json!({"text": "a\nb"}), false);
    fx.run();
    assert!(refused(&fx.row(id)).starts_with("pasted but could not submit to @bravo ("));
    assert_eq!(fx.drive().awaiting_turn, Some(1));
    let id = fx.add("send", serde_json::json!({"text": "next"}), false);
    fx.run();
    applied(&fx.row(id));
}

#[test]
fn queued_sends_wait_for_the_delivered_turn_to_start() {
    let fx = fixture();
    fx.session("bravo", "idle");
    let first = fx.add("send", serde_json::json!({"text": "one"}), true);
    let second = fx.add("send", serde_json::json!({"text": "two"}), true);
    fx.run();
    assert_eq!(
        applied(&fx.row(first)),
        serde_json::json!({"awaiting_turn": 1})
    );
    assert_eq!(
        fx.row(second).status,
        "pending",
        "the hook hasn't written busy yet"
    );
    assert_eq!(fx.writes(), vec!["line:one".to_string()]);

    // A later pass before the hook catches up still waits.
    fx.run();
    assert_eq!(fx.row(second).status, "pending");

    // The turn starts, then finishes: the second send goes in.
    fx.session("bravo", "busy");
    fx.run();
    assert_eq!(fx.row(second).status, "pending");
    fx.store().session_reply_record("bravo", "done").unwrap();
    fx.session("bravo", "idle");
    fx.run();
    assert_eq!(
        applied(&fx.row(second)),
        serde_json::json!({"awaiting_turn": 2})
    );
    assert_eq!(
        fx.writes(),
        vec!["line:one".to_string(), "line:two".to_string()]
    );
}

#[test]
fn a_send_right_after_a_delivery_is_refused_as_busy() {
    let fx = fixture();
    fx.session("bravo", "idle");
    let first = fx.add("send", serde_json::json!({"text": "one"}), false);
    let second = fx.add("send", serde_json::json!({"text": "two"}), false);
    fx.run();
    applied(&fx.row(first));
    assert_eq!(
        refused(&fx.row(second)),
        "@bravo is busy; send only goes to an idle session (use --queue to wait for it)"
    );
    assert_eq!(fx.writes(), vec!["line:one".to_string()]);
    assert_eq!(fx.drive().awaiting_turn, Some(1));

    // A finished turn (a fast turn whose busy was never seen) releases the hold.
    fx.store().session_reply_record("bravo", "done").unwrap();
    let third = fx.add("send", serde_json::json!({"text": "three"}), false);
    fx.run();
    assert_eq!(
        applied(&fx.row(third)),
        serde_json::json!({"awaiting_turn": 2})
    );
}

#[test]
fn the_send_hold_lapses_after_the_settle_timeout() {
    let fx = fx_with(Timeouts {
        max_age_secs: DRIVE_REQUEST_MAX_AGE_SECS,
        reset: WINDOW,
        swap_confirm: WINDOW,
        swap_run: WINDOW,
        send_settle: Duration::from_millis(50),
    });
    fx.session("bravo", "idle");
    let first = fx.add("send", serde_json::json!({"text": "one"}), true);
    let second = fx.add("send", serde_json::json!({"text": "two"}), true);
    fx.run();
    applied(&fx.row(first));
    assert_eq!(fx.row(second).status, "pending");
    std::thread::sleep(Duration::from_millis(80));
    fx.run();
    applied(&fx.row(second));
}

#[test]
fn a_reset_drops_the_send_hold() {
    let fx = fixture();
    fx.session("bravo", "idle");
    let id = fx.add("send", serde_json::json!({"text": "one"}), false);
    fx.run();
    applied(&fx.row(id));
    clear_awaiting_turn(&fx.env, "bravo");
    let id = fx.add("send", serde_json::json!({"text": "two"}), false);
    fx.run();
    applied(&fx.row(id));
}

#[test]
fn send_refusals_for_empty_paused_transfer_terminated_and_wrong_driver() {
    // Empty after sanitising.
    let fx = fixture();
    fx.session("bravo", "idle");
    let id = fx.add("send", serde_json::json!({"text": "\u{1b}\u{7}"}), false);
    fx.run();
    assert_eq!(refused(&fx.row(id)), "empty prompt");

    // Paused after the request was made.
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.store().drive_set_paused("bravo", true).unwrap();
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "drive of @bravo is paused; resume it before sending"
    );
    fx.store().drive_set_paused("bravo", false).unwrap();

    // Mid-transfer.
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    {
        let _t = fx.env.registry.begin("proj", "bravo").unwrap();
        fx.run();
    }
    assert_eq!(
        refused(&fx.row(id)),
        "a transfer or switch is in progress for worktree `bravo`; try again when it finishes"
    );

    // Taken over by another driver.
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.store()
        .drive_bind("charlie", "bravo", None, true)
        .unwrap();
    fx.run();
    assert_eq!(refused(&fx.row(id)), "@bravo is not driven by @alpha");
    assert!(fx.writes().is_empty());

    // Terminated.
    let fx = fx_terminated();
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "@bravo is terminated; launch it from the rail first"
    );
}

fn fx_terminated() -> Fx {
    let fx = fixture();
    fx.session("bravo", "idle");
    fx.fake.insert_terminated(fx.key("bravo"));
    fx
}

// ---------------------------------------------------------------------------
// Runner: read
// ---------------------------------------------------------------------------

#[test]
fn read_returns_the_pane_text_even_when_paused_or_transferring() {
    let fx = fixture();
    fx.session("bravo", "busy");
    fx.fake.push_read_history(Ok("line 1\nline 2".into()));
    let id = fx.add("read", serde_json::json!({"lines": 50}), false);
    fx.run();
    let row = fx.row(id);
    assert_eq!(row.status, "applied");
    assert_eq!(row.result, "line 1\nline 2");
    assert_eq!(fx.fake.read_history_asks(), vec![(fx.key("bravo"), 50)]);

    fx.store().drive_set_paused("bravo", true).unwrap();
    let _t = fx.env.registry.begin("proj", "bravo").unwrap();
    fx.fake
        .push_read_history(Ok("x".repeat(DRIVE_READ_RESULT_MAX + 10)));
    let id = fx.add("read", serde_json::json!({}), false);
    fx.run();
    let row = fx.row(id);
    assert_eq!(row.status, "applied", "{}", row.reason);
    assert!(row.result.starts_with(TRUNCATED_MARKER));
    assert_eq!(row.result.len(), DRIVE_READ_RESULT_MAX);
    assert_eq!(fx.fake.read_history_asks()[1].1, 200);
}

#[test]
fn read_refuses_a_terminated_pane_and_reports_backend_errors() {
    let fx = fx_terminated();
    let id = fx.add("read", serde_json::json!({}), false);
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "@bravo is terminated; launch it from the rail first"
    );
    assert!(fx.fake.read_history_asks().is_empty());

    let fx = fixture();
    fx.fake
        .push_read_history(Err(PtyBackendError::NotConnected));
    let id = fx.add("read", serde_json::json!({}), false);
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "could not read @bravo: herdr unreachable"
    );
}

// ---------------------------------------------------------------------------
// Runner: reset
// ---------------------------------------------------------------------------

#[test]
fn reset_without_to_restarts_and_clears_the_awaited_turn() {
    let fx = fixture();
    fx.session("bravo", "busy");
    fx.store().drive_set_awaiting_turn("bravo").unwrap();
    let id = fx.add("reset", serde_json::json!({}), false);
    run_with_window(&fx, move |rt, host| {
        let ev = host.restarts.lock().unwrap()[0].clone();
        assert_eq!(
            ev,
            DriveRestartEvent {
                request_id: id,
                project: "proj".into(),
                worktree: "bravo".into(),
                driver: "alpha".into(),
                mode: RestartMode::Reset,
                fresh: true,
                replace: true,
            }
        );
        rt.report("proj", id, "applied", String::new()).unwrap();
    });
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"provider": "claude"})
    );
    assert_eq!(
        fx.host.log(),
        vec!["restart".to_string()],
        "provider untouched"
    );
    assert_eq!(fx.drive().awaiting_turn, None);
    assert!(
        fx.env.runtime.claim_worktree("proj", "bravo").is_some(),
        "slot freed"
    );
}

#[test]
fn reset_to_another_provider_persists_before_the_restart() {
    let fx = fixture();
    let id = fx.add("reset", serde_json::json!({"to": "codex"}), false);
    run_with_window(&fx, move |rt, _| {
        rt.report("proj", id, "applied", String::new()).unwrap();
    });
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"provider": "codex"})
    );
    assert_eq!(
        fx.host.log(),
        vec![
            "persist:codex",
            "worktrees_changed",
            "prepare:codex",
            "restart"
        ]
    );
    assert_eq!(fx.host.cli_of("bravo"), CliKind::Codex);
}

#[test]
fn reset_to_agy_switches_to_antigravity_and_prepares_its_launch() {
    let fx = fixture();
    let id = fx.add("reset", serde_json::json!({"to": "agy"}), false);
    run_with_window(&fx, move |rt, _| {
        rt.report("proj", id, "applied", String::new()).unwrap();
    });
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"provider": "antigravity"})
    );
    assert_eq!(
        fx.host.log(),
        vec![
            "persist:antigravity",
            "worktrees_changed",
            "prepare:antigravity",
            "restart"
        ]
    );
    assert_eq!(fx.host.cli_of("bravo"), CliKind::Antigravity);
}

#[test]
fn a_refused_restart_rolls_the_provider_back() {
    let fx = fixture();
    let id = fx.add("reset", serde_json::json!({"to": "codex"}), false);
    run_with_window(&fx, move |rt, _| {
        rt.report(
            "proj",
            id,
            "refused",
            "@bravo has no pane in Keitora".into(),
        )
        .unwrap();
    });
    assert_eq!(
        refused(&fx.row(id)),
        "reset failed: @bravo has no pane in Keitora (provider restored to claude)"
    );
    assert_eq!(fx.host.cli_of("bravo"), CliKind::Claude);
    assert_eq!(
        fx.host.log(),
        vec![
            "persist:codex",
            "worktrees_changed",
            "prepare:codex",
            "restart",
            "persist:claude",
            "worktrees_changed"
        ]
    );
}

#[test]
fn a_restart_the_window_never_reports_times_out_and_rolls_back() {
    let fx = fixture();
    let id = fx.add("reset", serde_json::json!({"to": "codex"}), false);
    fx.run();
    let reason = refused(&fx.row(id)).to_string();
    assert!(
        reason.starts_with("reset failed: the Keitora window did not restart @bravo within "),
        "{reason}"
    );
    assert!(
        reason.ends_with(" (provider restored to claude)"),
        "{reason}"
    );
    assert_eq!(fx.host.cli_of("bravo"), CliKind::Claude);
    // The waiter is gone: a late report is rejected.
    assert!(fx
        .env
        .runtime
        .report("proj", id, "applied", String::new())
        .is_err());
}

#[test]
fn reset_refusals() {
    // Unknown provider.
    let fx = fixture();
    let id = fx.add("reset", serde_json::json!({"to": "zeta"}), false);
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "unknown provider `zeta`; expected one of: claude, codex, antigravity (agy)"
    );
    // The provider can't be persisted.
    *fx.host.persist_fails.lock().unwrap() = true;
    let id = fx.add("reset", serde_json::json!({"to": "codex"}), false);
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "could not switch @bravo to codex: disk full"
    );
    assert!(fx.host.restarts.lock().unwrap().is_empty());
    // Mid-transfer.
    let id = fx.add("reset", serde_json::json!({}), false);
    {
        let _t = fx.env.registry.begin("proj", "bravo").unwrap();
        fx.run();
    }
    assert!(refused(&fx.row(id)).starts_with("a transfer or switch is in progress"));
    // Terminated.
    let fx = fx_terminated();
    let id = fx.add("reset", serde_json::json!({}), false);
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "@bravo is terminated; launch it from the rail first"
    );
}

#[test]
fn a_second_reset_for_the_same_worktree_is_refused() {
    let fx = fixture();
    let first = fx.add("reset", serde_json::json!({}), false);
    let second = fx.add("swap", serde_json::json!({"to": "codex"}), false);
    let jobs = fx.pass();
    assert_eq!(jobs.len(), 1);
    assert_eq!(fx.row(first).status, "running");
    assert_eq!(
        refused(&fx.row(second)),
        "another reset, swap or wake is in progress for @bravo"
    );
    for job in jobs {
        tauri::async_runtime::block_on(job);
    }
    assert_eq!(fx.row(first).status, "refused");
}

// ---------------------------------------------------------------------------
// Runner: swap
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Runner: wake
// ---------------------------------------------------------------------------

fn wake(fresh: bool) -> serde_json::Value {
    serde_json::json!({"fresh": fresh})
}

#[test]
fn wake_of_an_offline_worktree_emits_a_wake_restart() {
    let fx = fixture();
    fx.store().drive_set_awaiting_turn("bravo").unwrap();
    let id = fx.add("wake", wake(false), false);
    run_with_window(&fx, move |rt, host| {
        let ev = host.restarts.lock().unwrap()[0].clone();
        assert_eq!(
            ev,
            DriveRestartEvent {
                request_id: id,
                project: "proj".into(),
                worktree: "bravo".into(),
                driver: "alpha".into(),
                mode: RestartMode::Wake,
                fresh: false,
                replace: false,
            }
        );
        rt.report("proj", id, "applied", String::new()).unwrap();
    });
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"provider": "claude", "already_awake": false})
    );
    assert_eq!(fx.drive().awaiting_turn, None, "the dead turn is dropped");
}

#[test]
fn wake_lifts_the_terminated_gate_and_can_start_fresh_on_another_provider() {
    let fx = fx_terminated();
    let id = fx.add(
        "wake",
        serde_json::json!({"fresh": true, "to": "codex"}),
        false,
    );
    run_with_window(&fx, move |rt, host| {
        let ev = host.restarts.lock().unwrap()[0].clone();
        assert_eq!(
            (ev.mode, ev.fresh, ev.replace),
            (RestartMode::Wake, true, true)
        );
        rt.report("proj", id, "applied", String::new()).unwrap();
    });
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"provider": "codex", "already_awake": false})
    );
    assert_eq!(
        fx.host.log(),
        vec![
            "persist:codex",
            "worktrees_changed",
            "prepare:codex",
            "restart"
        ]
    );
}

#[test]
fn wake_of_a_running_agent_is_a_no_op() {
    let fx = fixture();
    fx.session("bravo", "busy");
    fx.store().drive_set_awaiting_turn("bravo").unwrap();
    let id = fx.add("wake", wake(true), false);
    fx.run();
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"provider": "claude", "already_awake": true})
    );
    assert!(fx.host.log().is_empty(), "no restart, no provider change");
    assert_eq!(fx.drive().awaiting_turn, Some(1), "the running turn stays");

    // Asking for another provider on a running agent is a reset, not a wake.
    let id = fx.add("wake", serde_json::json!({"to": "codex"}), false);
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "@bravo is already awake on claude; use reset --to codex to switch"
    );
}

#[test]
fn a_failed_wake_is_refused_and_rolls_the_provider_back() {
    let fx = fixture();
    let id = fx.add("wake", serde_json::json!({"to": "codex"}), false);
    run_with_window(&fx, move |rt, host| {
        // Another provider: the pane must start a new session, not keep the old one.
        let ev = host.restarts.lock().unwrap()[0].clone();
        assert_eq!((ev.fresh, ev.replace), (false, true));
        rt.report("proj", id, "refused", "spawn failed".into())
            .unwrap();
    });
    assert_eq!(
        refused(&fx.row(id)),
        "wake failed: spawn failed (provider restored to claude)"
    );
    assert_eq!(fx.host.cli_of("bravo"), CliKind::Claude);

    let id = fx.add("wake", wake(false), false);
    fx.run();
    let reason = refused(&fx.row(id)).to_string();
    assert!(
        reason.starts_with("wake failed: the Keitora window did not wake @bravo within "),
        "{reason}"
    );
}

#[test]
fn wake_on_a_paused_drive_is_refused() {
    let fx = fixture();
    let id = fx.add("wake", wake(false), false);
    fx.store().drive_set_paused("bravo", true).unwrap();
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "drive of @bravo is paused; resume it before sending"
    );
    assert!(fx.host.restarts.lock().unwrap().is_empty());
}

fn wake_send(text: &str) -> serde_json::Value {
    serde_json::json!({"text": text, "wake": true})
}

#[test]
fn a_send_to_an_offline_worktree_wakes_it_then_waits_for_idle() {
    let fx = fixture();
    let id = fx.add("send", wake_send("hi"), false);
    let key = fx.key("bravo");
    let fake = fx.fake.clone();
    run_with_window(&fx, move |rt, host| {
        let ev = host.restarts.lock().unwrap()[0].clone();
        assert_eq!(ev.request_id, id, "the wake reports for the send");
        assert_eq!((ev.mode, ev.fresh), (RestartMode::Wake, false), "resumes");
        // The window launched it: the pane is live, its hook hasn't written yet.
        fake.insert_live(key);
        rt.report("proj", id, "applied", String::new()).unwrap();
    });
    assert_eq!(fx.row(id).status, "pending", "queued behind the wake");
    assert!(fx.writes().is_empty());

    // Live, but no idle write yet: still waiting (and no second wake).
    fx.store()
        .session_upsert("bravo", "offline", None, None)
        .unwrap();
    assert!(fx.pass().is_empty(), "no second wake");
    assert_eq!(fx.row(id).status, "pending");

    // The agent's SessionStart hook writes idle: the prompt goes in.
    fx.session("bravo", "idle");
    fx.run();
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"awaiting_turn": 1})
    );
    assert_eq!(fx.writes(), vec!["line:hi".to_string()]);
    assert_eq!(fx.host.restarts.lock().unwrap().len(), 1);
}

#[test]
fn two_sends_to_an_offline_worktree_wake_it_once_and_both_go_in_order() {
    let fx = fixture();
    let first = fx.add("send", wake_send("one"), false);
    let second = fx.add("send", wake_send("two"), false);
    let key = fx.key("bravo");
    let fake = fx.fake.clone();
    run_with_window(&fx, move |rt, _| {
        fake.insert_live(key);
        rt.report("proj", first, "applied", String::new()).unwrap();
    });
    assert_eq!(fx.host.restarts.lock().unwrap().len(), 1, "one wake");
    fx.session("bravo", "idle");
    fx.run();
    applied(&fx.row(first));
    assert_eq!(
        fx.row(second).status,
        "pending",
        "queued behind the first turn"
    );
    fx.store().session_reply_record("bravo", "done").unwrap();
    fx.session("bravo", "idle");
    fx.run();
    applied(&fx.row(second));
    assert_eq!(
        fx.writes(),
        vec!["line:one".to_string(), "line:two".to_string()]
    );
}

#[test]
fn a_send_wakes_a_terminated_worktree() {
    let fx = fixture();
    fx.fake.insert_terminated(fx.key("bravo"));
    let id = fx.add("send", wake_send("hi"), false);
    let jobs = fx.pass();
    assert_eq!(jobs.len(), 1, "a wake job, not a terminated refusal");
    assert_eq!(fx.row(id).status, "pending");
}

#[test]
fn a_send_whose_wake_fails_is_refused() {
    let fx = fixture();
    let id = fx.add("send", wake_send("hi"), false);
    run_with_window(&fx, move |rt, _| {
        rt.report("proj", id, "refused", "spawn failed".into())
            .unwrap();
    });
    assert_eq!(
        refused(&fx.row(id)),
        "could not wake @bravo: wake failed: spawn failed"
    );
    assert!(fx.writes().is_empty());
}

#[test]
fn a_woken_send_whose_agent_never_comes_up_is_refused() {
    let fx = fx_with(Timeouts {
        max_age_secs: DRIVE_REQUEST_MAX_AGE_SECS,
        reset: WINDOW,
        swap_confirm: WINDOW,
        swap_run: WINDOW,
        send_settle: Duration::from_millis(50),
    });
    let id = fx.add("send", wake_send("hi"), false);
    run_with_window(&fx, move |rt, _| {
        // Reported launched, but the pane never shows up live.
        rt.report("proj", id, "applied", String::new()).unwrap();
    });
    // Nothing writes the DB from here on, so the runner schedules its own pass for
    // just after the grace period.
    let scheduled = fx.host.scheduled();
    assert_eq!(scheduled.len(), 1, "{scheduled:?}");
    assert_eq!(scheduled[0].0, "proj");
    assert!(scheduled[0].1 > Duration::from_millis(50));
    assert!(scheduled[0].1 <= Duration::from_millis(50) + WAKE_RECHECK_SLACK);
    assert!(fx.pass().is_empty(), "inside the grace period: waits");
    assert_eq!(fx.row(id).status, "pending");
    assert_eq!(fx.host.scheduled().len(), 1, "one recheck per deadline");
    // The scheduled pass.
    std::thread::sleep(Duration::from_millis(80));
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "@bravo was woken but its agent did not come up; check its pane"
    );
    assert_eq!(fx.host.restarts.lock().unwrap().len(), 1, "woken once");
}

#[test]
fn a_send_that_only_waited_on_another_wake_wakes_its_target_itself() {
    let fx = fixture();
    let first = fx.add("send", wake_send("one"), false);
    let second = fx.add("send", wake_send("two"), false);
    // The first send's wake fails; the second only waited on it meanwhile.
    run_with_window(&fx, move |rt, _| {
        rt.report("proj", first, "refused", "spawn failed".into())
            .unwrap();
    });
    assert!(refused(&fx.row(first)).starts_with("could not wake @bravo"));
    let mark = fx.env.runtime.wake_mark("proj", second).unwrap();
    assert!(!mark.own, "waiting on the slot is not a wake of its own");
    assert_eq!(fx.row(second).status, "pending");

    // The slot is free and the target still down: the second send wakes it rather
    // than being refused for a wake it never made.
    let jobs = fx.pass();
    assert_eq!(jobs.len(), 1, "its own wake");
    assert_eq!(fx.row(second).status, "pending");
    assert!(fx.env.runtime.wake_mark("proj", second).unwrap().own);
    let runtime = fx.env.runtime.clone();
    let host = fx.host.clone();
    tauri::async_runtime::block_on(async move {
        let handle = tauri::async_runtime::spawn(jobs.into_iter().next().unwrap());
        for _ in 0..200 {
            if host.restarts.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(host.restarts.lock().unwrap()[1].request_id, second);
        runtime
            .report("proj", second, "refused", "still failing".into())
            .unwrap();
        handle.await.unwrap();
    });
    assert_eq!(
        refused(&fx.row(second)),
        "could not wake @bravo: wake failed: still failing"
    );
}

#[test]
fn a_send_waiting_on_a_reset_is_not_marked_woken() {
    let fx = fixture();
    fx.add("reset", serde_json::json!({}), false);
    let send = fx.add("send", wake_send("hi"), false);
    let mut jobs = fx.pass();
    assert_eq!(jobs.len(), 1, "the reset's job; the send waits on its slot");
    assert_eq!(fx.row(send).status, "pending");
    let mark = fx.env.runtime.wake_mark("proj", send).unwrap();
    assert!(!mark.own);
    assert!(
        fx.host.scheduled().is_empty(),
        "the reset's end is a DB write"
    );
    drop(jobs.pop());
}

#[test]
fn a_no_wake_send_to_an_offline_worktree_is_refused_as_before() {
    let fx = fixture();
    let id = fx.add(
        "send",
        serde_json::json!({"text": "hi", "wake": false}),
        false,
    );
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "@bravo is offline; start its session first"
    );
    assert!(fx.host.restarts.lock().unwrap().is_empty());
}

#[test]
fn a_waking_send_to_a_busy_agent_is_refused_unless_queued() {
    let fx = fixture();
    fx.session("bravo", "busy");
    let id = fx.add("send", wake_send("hi"), false);
    fx.run();
    assert!(refused(&fx.row(id)).starts_with("@bravo is busy"));
    assert!(
        fx.host.restarts.lock().unwrap().is_empty(),
        "nothing to wake"
    );
}

#[test]
fn a_waking_send_on_a_paused_drive_is_refused_without_waking() {
    let fx = fixture();
    let id = fx.add("send", wake_send("hi"), false);
    fx.store().drive_set_paused("bravo", true).unwrap();
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "drive of @bravo is paused; resume it before sending"
    );
    assert!(fx.host.restarts.lock().unwrap().is_empty());
}

#[test]
fn swap_waits_for_confirm_then_for_the_transfer() {
    let fx = fixture();
    fx.store().drive_set_awaiting_turn("bravo").unwrap();
    let id = fx.add("swap", serde_json::json!({"to": "codex"}), false);
    let db = fx.env.db_path.clone();
    run_with_window(&fx, move |rt, host| {
        let ev = host.swaps.lock().unwrap()[0].clone();
        assert_eq!(ev.to, CliKind::Codex);
        assert_eq!(ev.driver, "alpha");
        let status = |db: &str| {
            LocalStore::open(db)
                .unwrap()
                .drive_request_get(id)
                .unwrap()
                .unwrap()
                .status
        };
        assert_eq!(status(&db), "pending_confirm");
        rt.report("proj", id, "accepted", String::new()).unwrap();
        // The runner moves the row back to running on accept.
        for _ in 0..200 {
            if status(&db) == "running" {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(status(&db), "running");
        rt.report("proj", id, "applied", "Switched to Codex".into())
            .unwrap();
    });
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"provider": "codex"})
    );
    assert_eq!(fx.drive().awaiting_turn, None);
    assert!(
        !fx.host.log().iter().any(|e| e.starts_with("persist")),
        "the transfer persists, not the runner"
    );
}

#[test]
fn a_cancelled_swap_is_refused_with_the_window_reason() {
    let fx = fixture();
    let id = fx.add("swap", serde_json::json!({"to": "codex"}), false);
    run_with_window(&fx, move |rt, _| {
        rt.report("proj", id, "refused", "swap cancelled in Keitora".into())
            .unwrap();
    });
    assert_eq!(refused(&fx.row(id)), "swap cancelled in Keitora");
}

#[test]
fn an_unconfirmed_swap_times_out() {
    let fx = fixture();
    let id = fx.add("swap", serde_json::json!({"to": "codex"}), false);
    fx.run();
    assert!(refused(&fx.row(id)).starts_with("swap was not confirmed in Keitora within "));
}

#[test]
fn a_swap_whose_transfer_never_finishes_times_out() {
    let fx = fixture();
    let id = fx.add("swap", serde_json::json!({"to": "codex"}), false);
    run_with_window(&fx, move |rt, _| {
        rt.report("proj", id, "accepted", String::new()).unwrap();
    });
    let reason = refused(&fx.row(id)).to_string();
    assert!(
        reason.starts_with("the transfer did not finish within "),
        "{reason}"
    );
    assert!(reason.ends_with("; check @bravo's pane"), "{reason}");
}

#[test]
fn swap_to_the_current_provider_or_unknown_is_refused() {
    let fx = fixture();
    let id = fx.add("swap", serde_json::json!({"to": "claude"}), false);
    fx.run();
    assert_eq!(refused(&fx.row(id)), "@bravo is already on claude");
    let id = fx.add("swap", serde_json::json!({"to": "zeta"}), false);
    fx.run();
    assert!(refused(&fx.row(id)).starts_with("unknown provider `zeta`"));
    assert!(fx.host.swaps.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Commands (DB side)
// ---------------------------------------------------------------------------

#[test]
fn drive_views_carry_pending_prompts() {
    let fx = fixture();
    fx.store()
        .drive_bind("alpha", "charlie", None, false)
        .unwrap();
    let p = fx.prompt("bravo");
    let views = drive_views(&fx.store()).unwrap();
    assert_eq!(views.len(), 2);
    assert_eq!(views[0].driven, "bravo");
    assert_eq!(views[0].driver, "alpha");
    assert_eq!(views[0].pending.len(), 1);
    assert_eq!(views[0].pending[0].id, p);
    assert_eq!(views[0].pending[0].kind, "permission");
    assert_eq!(views[0].pending[0].tool_name, "Bash");
    assert!(views[1].pending.is_empty());
    let v = serde_json::to_value(&views[0]).unwrap();
    for k in [
        "driven",
        "driver",
        "since",
        "forced_from",
        "paused",
        "escalate_after_s",
        "awaiting_turn",
        "pending",
    ] {
        assert!(v.get(k).is_some(), "{k}");
    }
}

#[test]
fn take_over_counts_and_marks_prompts_for_the_human() {
    let fx = fixture();
    let p = fx.prompt("bravo");
    let local = fx.store();
    assert_eq!(take_over_in(&local, "bravo"), Ok(1));
    let row = local.drive_prompt_get(p).unwrap().unwrap();
    assert_eq!(row.status, "escalated");
    assert_eq!(row.decided_by.as_deref(), Some("human"));
    assert!(local.drive_get("bravo").unwrap().is_some(), "binding stays");
    assert_eq!(take_over_in(&local, "bravo"), Ok(0));
    assert_eq!(
        take_over_in(&local, "charlie"),
        Ok(0),
        "not driven is not an error"
    );
}

#[test]
fn release_and_pause_escalate_pending_prompts() {
    let fx = fixture();
    let local = fx.store();
    let p = fx.prompt("bravo");
    pause_in(&local, "bravo", true).unwrap();
    let row = local.drive_prompt_get(p).unwrap().unwrap();
    assert_eq!(row.status, "escalated");
    assert!(local.drive_get("bravo").unwrap().unwrap().paused);
    pause_in(&local, "bravo", false).unwrap();

    let p = fx.prompt("bravo");
    release_in(&local, "bravo").unwrap();
    let row = local.drive_prompt_get(p).unwrap().unwrap();
    assert_eq!(row.status, "escalated");
    assert_eq!(row.decided_by.as_deref(), Some("system"));
    assert!(local.drive_get("bravo").unwrap().is_none());
    assert!(release_in(&local, "bravo").is_err(), "not driven");
    assert!(pause_in(&local, "bravo", true).is_err(), "not driven");
}

#[test]
fn event_payloads_use_snake_case_keys() {
    let v = serde_json::to_value(DriveSwapEvent {
        request_id: 3,
        project: "proj".into(),
        worktree: "bravo".into(),
        driver: "alpha".into(),
        to: CliKind::Codex,
    })
    .unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "request_id": 3, "project": "proj", "worktree": "bravo",
            "driver": "alpha", "to": "codex"
        })
    );
    let v = serde_json::to_value(DriveRestartEvent {
        request_id: 4,
        project: "proj".into(),
        worktree: "bravo".into(),
        driver: "alpha".into(),
        mode: RestartMode::Wake,
        fresh: false,
        replace: false,
    })
    .unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "request_id": 4, "project": "proj", "worktree": "bravo",
            "driver": "alpha", "mode": "wake", "fresh": false, "replace": false
        })
    );
    assert_eq!(RESTART_EVENT, "drive://restart");
    assert_eq!(SWAP_EVENT, "drive://swap");
}
