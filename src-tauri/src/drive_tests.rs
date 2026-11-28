//! Tests for the drive request runner and commands. Every test uses a temp-file DB, a
//! `FakeBackend` and a fake host; nothing touches herdr, a window or the real config.

use std::sync::Mutex as StdMutex;

use keitora_tasks::{NewDrivePrompt, WorktreeEntry};

use super::*;
use crate::backend::fake::{Call, FakeBackend};
use crate::backend::herdr::ops::Generation;
use crate::backend::{PtyBackend as _, PtyBackendError};
use crate::engine::evidence::RegistryEvidence;
use crate::engine::model::{obs, AgentObs, HookEvidence};
use crate::engine::tests::{FakeEvidence, TestClock};
use crate::engine::{self, HerdrLink, HerdrSnapshot, SessionEngine};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Records what the runner asked of the app.
#[derive(Default)]
struct FakeHost {
    /// `restart`, `swap`, `worktrees_changed`, `persist:<cli>`, `refresh` in order.
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
    fn refresh_integration(&self) {
        self.log.lock().unwrap().push("refresh".into());
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
    /// The engine's clock: moved only by [`Fx::tick`] and [`Fx::age`].
    clock: TestClock,
    t0: Instant,
    t0_ms: i64,
    /// Fake time since `t0`, and herdr's last `state_change_seq`.
    elapsed: StdMutex<Duration>,
    scs: StdMutex<u64>,
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
    let fx = fx_cold(timeouts);
    // A first (empty) snapshot: the engine is ready.
    fx.tick();
    fx
}

/// [`fx_with`] before the engine's first snapshot (herdr's link still starting).
fn fx_cold(timeouts: Timeouts) -> Fx {
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
    let lookup = (db_path.clone(), config.worktrees());
    let evidence =
        RegistryEvidence::with_lookup(Box::new(move |p| (p == "proj").then(|| lookup.clone())));
    let t0 = Instant::now();
    let clock = TestClock::new(t0);
    let engine = SessionEngine::with_clock(Arc::new(evidence), None, clock.clock());
    let env = Arc::new(DriveEnv {
        project: config.project.name.clone(),
        config,
        db_path,
        backend: fake.clone(),
        registry: TransferRegistry::new(),
        runtime,
        host: host.clone(),
        timeouts,
        engine,
    });
    assert_eq!(env.project, "proj");
    Fx {
        _dir: dir,
        env,
        fake,
        host,
        gate: DispatchGate::default(),
        clock,
        t0,
        t0_ms: keitora_tasks::now_ms(),
        elapsed: StdMutex::new(Duration::ZERO),
        scs: StdMutex::new(0),
    }
}

impl Fx {
    fn store(&self) -> LocalStore {
        LocalStore::open(&self.env.db_path).unwrap()
    }
    fn key(&self, wt: &str) -> SessionKey {
        self.env.key(wt)
    }
    /// Record `wt`'s hook state, mark its pane live with herdr reporting the same
    /// (`busy` working, `waiting` blocked, `offline` a bare shell), and tick.
    fn session(&self, wt: &str, state: &str) {
        self.store().session_upsert(wt, state, None, None).unwrap();
        self.fake.insert_live(self.key(wt));
        let agent = match state {
            "offline" => None,
            "busy" => Some("working"),
            "waiting" => Some("blocked"),
            _ => Some("idle"),
        };
        self.herdr(wt, agent);
        self.tick();
    }
    /// Set herdr's entry for `wt`: an agent in `status` (a new `state_change_seq`),
    /// or a bare shell. Doesn't tick.
    fn herdr(&self, wt: &str, status: Option<&str>) {
        let scs = {
            let mut scs = self.scs.lock().unwrap();
            *scs += 1;
            *scs
        };
        let pane = format!("w1:{wt}");
        let entry = match status {
            Some(status) => obs(&pane, status, scs, None),
            None => AgentObs {
                agent: None,
                ..obs(&pane, "unknown", scs, None)
            },
        };
        self.fake.set_agent(self.key(wt), Some(entry));
    }
    /// Move the engine's clock forward by `by`.
    fn age(&self, by: Duration) {
        let mut elapsed = self.elapsed.lock().unwrap();
        *elapsed += by;
        self.clock.set(self.t0 + *elapsed);
    }
    /// Two status-loop ticks a second apart (idles settle).
    fn tick(&self) {
        self.snap();
        self.snap();
    }
    /// One status-loop tick a second on: every live pane with its agent entry.
    fn snap(&self) {
        {
            self.age(engine::SETTLE);
            let elapsed = *self.elapsed.lock().unwrap();
            let mut live: Vec<SessionKey> = self.fake.live_set_snapshot().into_iter().collect();
            live.sort_by(|a, b| a.worktree.cmp(&b.worktree));
            let keys = live
                .into_iter()
                .map(|k| {
                    let o = self.fake.agent_snapshot(&k).unwrap();
                    (k, o)
                })
                .collect();
            self.env.engine.ingest(HerdrSnapshot {
                at: self.t0 + elapsed,
                at_ms: self.t0_ms + elapsed.as_millis() as i64,
                generation: Generation(1),
                keys,
            });
        }
    }
    /// Record a delivery to `wt` as the runner would (no prompt typed).
    fn begin_delivery(&self, wt: &str) -> (i64, i64) {
        let b = baseline_of(&obs(&format!("w1:{wt}"), "idle", 1, None), self.t0_ms);
        self.store().drive_begin_delivery(wt, &b).unwrap().unwrap()
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
        tauri::async_runtime::block_on(run_pass(&self.env, &self.gate, Trigger::Watcher))
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

/// A view of `bravo` for the pure gate tests: `agent` present unless the state is
/// `offline`, fresh and settled.
fn gate_view(state: &str, herdr: Option<&str>) -> StatusSession {
    StatusSession {
        project: "proj".into(),
        worktree: "bravo".into(),
        state: state.into(),
        agent: if state == "offline" {
            "absent"
        } else {
            "present"
        }
        .into(),
        agent_kind: herdr.map(|_| "claude".to_string()),
        herdr_status: herdr.map(str::to_string),
        settled: state == "idle",
        stale: false,
        interactive_ready: herdr.map(|_| true),
        pane_id: Some("w1:p1".into()),
        terminal_id: None,
        agent_session: None,
        state_change_seq: herdr.map(|_| 7),
        completion_seq: None,
        brokered: false,
        escalated_reason: None,
        last_change_at: 0,
    }
}

fn gate_sets(ready: bool, link: HerdrLink, view: Option<StatusSession>) -> SessionSets {
    SessionSets {
        views: ProjectViews {
            ready,
            link,
            views: view.map(|v| ("bravo".to_string(), v)).into_iter().collect(),
        },
        terminated: HashSet::new(),
        hooks: HashMap::new(),
        at_ms: 0,
    }
}

#[test]
fn send_gate_branches_carry_the_exact_texts() {
    let wait = |t: &str| SendGate::Wait(t.to_string());
    let refuse = |t: &str| SendGate::Refuse(t.to_string());
    let idle = || Some(gate_view("idle", Some("idle")));
    let gate =
        |sets: &SessionSets, queued: bool, hold: bool| sets.send_gate("bravo", queued, |_| hold);
    // No view while ready: offline.
    assert_eq!(
        gate(&gate_sets(true, HerdrLink::Up, None), false, false),
        refuse("@bravo is offline; start its session first (herdr: no agent; no hook rows)")
    );
    // No snapshot yet.
    for link in [HerdrLink::Starting, HerdrLink::Up] {
        assert_eq!(
            gate(&gate_sets(false, link, None), false, false),
            wait("waiting for first herdr snapshot"),
            "{link:?}"
        );
    }
    // herdr not up: the views are stale, never offline.
    for (link, text) in [
        (HerdrLink::Recovering, "waiting for herdr (recovering)"),
        (HerdrLink::Down, "waiting for herdr (down)"),
        (HerdrLink::Unavailable, "waiting for herdr (unavailable)"),
    ] {
        assert_eq!(
            gate(&gate_sets(false, link, idle()), false, false),
            wait(text)
        );
        assert_eq!(gate(&gate_sets(false, link, None), true, false), wait(text));
    }
    // A stale view under an up link.
    let mut stale = gate_view("idle", Some("idle"));
    stale.stale = true;
    assert_eq!(
        gate(&gate_sets(true, HerdrLink::Up, Some(stale)), false, false),
        wait("waiting for herdr (stale snapshot)")
    );
    // An agent still starting.
    let mut starting = gate_view("busy", Some("idle"));
    starting.agent = "starting".into();
    assert_eq!(
        gate(
            &gate_sets(true, HerdrLink::Up, Some(starting)),
            false,
            false
        ),
        wait("waiting for @bravo to be idle (herdr: starting)")
    );
    let mut not_ready = gate_view("idle", Some("idle"));
    not_ready.interactive_ready = Some(false);
    assert_eq!(
        gate(
            &gate_sets(true, HerdrLink::Up, Some(not_ready)),
            false,
            false
        ),
        wait("waiting for @bravo to be idle (herdr: starting)")
    );
    // Idle, not settled yet.
    let mut unsettled = gate_view("idle", Some("idle"));
    unsettled.settled = false;
    assert_eq!(
        gate(
            &gate_sets(true, HerdrLink::Up, Some(unsettled)),
            false,
            false
        ),
        wait("waiting for @bravo to settle")
    );
    // Settled idle: ready, unless a send just went in.
    assert_eq!(
        gate(&gate_sets(true, HerdrLink::Up, idle()), false, false),
        SendGate::Ready
    );
    assert_eq!(
        gate(&gate_sets(true, HerdrLink::Up, idle()), false, true),
        wait("waiting for @bravo to start its turn (send settling)")
    );
    // Not idle: refused, or a queued send waits with herdr's status.
    for (state, herdr, refusal, queued) in [
        (
            "busy",
            Some("working"),
            "@bravo is busy; send only goes to an idle session (use --queue to wait for it) (herdr: working; no hook rows)",
            "waiting for @bravo to be idle (herdr: working)",
        ),
        (
            "waiting",
            Some("blocked"),
            "@bravo is waiting on a prompt; answer it first (or use --queue)",
            "waiting for @bravo to be idle (herdr: blocked)",
        ),
        (
            "offline",
            None,
            "@bravo is offline; start its session first (herdr: no agent; no hook rows)",
            "waiting for @bravo to be idle (herdr: no agent)",
        ),
        (
            "busy",
            Some("unknown"),
            "@bravo is busy; send only goes to an idle session (use --queue to wait for it) (herdr: unknown; no hook rows)",
            "waiting for @bravo to be idle (herdr: unknown)",
        ),
    ] {
        let sets = gate_sets(true, HerdrLink::Up, Some(gate_view(state, herdr)));
        assert_eq!(gate(&sets, false, false), refuse(refusal), "{state}");
        assert_eq!(gate(&sets, true, false), wait(queued), "{state} queued");
    }
}

#[test]
fn awake_reads_terminated_freshness_and_presence() {
    let sets = |ready, view: Option<StatusSession>| gate_sets(ready, HerdrLink::Up, view);
    assert_eq!(sets(false, None).awake("bravo"), Awake::Unknown);
    assert_eq!(
        sets(false, Some(gate_view("idle", Some("idle")))).awake("bravo"),
        Awake::Unknown
    );
    let mut stale = gate_view("idle", Some("idle"));
    stale.stale = true;
    assert_eq!(sets(true, Some(stale)).awake("bravo"), Awake::Unknown);
    assert_eq!(sets(true, None).awake("bravo"), Awake::No);
    assert_eq!(
        sets(true, Some(gate_view("offline", None))).awake("bravo"),
        Awake::No
    );
    assert_eq!(
        sets(true, Some(gate_view("busy", Some("working")))).awake("bravo"),
        Awake::Yes
    );
    let mut starting = gate_view("busy", None);
    starting.agent = "starting".into();
    assert_eq!(sets(true, Some(starting)).awake("bravo"), Awake::Yes);
    // Terminated wins over anything the engine says, even when not ready.
    let mut terminated = sets(false, Some(gate_view("idle", Some("idle"))));
    terminated.terminated.insert("bravo".into());
    assert_eq!(terminated.awake("bravo"), Awake::No);
}

#[test]
fn a2_wake_already_awake_iff_send_not_refused_offline() {
    let offline = SendGate::Refuse(
        "@bravo is offline; start its session first (herdr: no agent; no hook rows)".to_string(),
    );
    let stale_wait = SendGate::Wait("waiting for herdr (stale snapshot)".to_string());
    let key = SessionKey {
        project: "proj".into(),
        worktree: "bravo".into(),
    };
    for provider in ["claude", "codex", "agy"] {
        for row in [
            None,
            Some("idle"),
            Some("busy"),
            Some("waiting"),
            Some("offline"),
        ] {
            for herdr in [
                "absent", "starting", "idle", "working", "blocked", "unknown", "stale",
            ] {
                let evidence = Arc::new(FakeEvidence::default());
                let t0 = Instant::now();
                let t0_ms = 1_800_000_000_000;
                let clock = TestClock::new(t0);
                let engine = SessionEngine::with_clock(evidence.clone(), None, clock.clock());
                if let Some(state) = row {
                    evidence.set_row(
                        "proj",
                        "bravo",
                        Some(HookEvidence {
                            state: state.into(),
                            written_ms: Some(t0_ms - 60_000),
                            brokered: false,
                            escalated_reason: None,
                            session_id: None,
                        }),
                    );
                }
                let entry = match herdr {
                    "absent" => None,
                    "starting" => Some(AgentObs {
                        launch_pending: true,
                        interactive_ready: None,
                        ..obs("w1:p1", "unknown", 3, None)
                    }),
                    "stale" => Some(obs("w1:p1", "idle", 3, None)),
                    status => Some(obs("w1:p1", status, 3, None)),
                }
                .map(|o| AgentObs {
                    agent: Some(provider.into()),
                    ..o
                });
                for ms in [0u64, 1_000] {
                    let at = t0 + Duration::from_millis(ms);
                    clock.set(at);
                    engine.ingest(HerdrSnapshot {
                        at,
                        at_ms: t0_ms + ms as i64,
                        generation: Generation(1),
                        keys: vec![(key.clone(), entry.clone())],
                    });
                }
                if herdr == "stale" {
                    clock.set(t0 + Duration::from_secs(120));
                }
                let sets = SessionSets {
                    views: engine.views("proj"),
                    terminated: HashSet::new(),
                    hooks: HashMap::new(),
                    at_ms: 0,
                };
                let awake = sets.awake("bravo");
                let gate = sets.send_gate("bravo", false, |_| false);
                let case = format!("{provider} row={row:?} herdr={herdr}: {awake:?} {gate:?}");
                assert_eq!(awake != Awake::No, gate != offline, "{case}");
                assert_eq!(awake == Awake::Unknown, gate == stale_wait, "{case}");
                assert_eq!(awake == Awake::Unknown, herdr == "stale", "{case}");
            }
        }
    }
}

#[test]
fn send_settling_is_released_by_working_by_a_seq_change_or_by_time() {
    let rt = DriveRuntime::default();
    let settle = Duration::from_secs(30);
    let view = |herdr: &str, scs: i64| StatusSession {
        state_change_seq: Some(scs),
        ..gate_view("idle", Some(herdr))
    };
    assert!(
        !rt.send_settling("proj", "bravo", &view("idle", 5), settle),
        "no hold"
    );

    rt.mark_sent("proj", "bravo", 0, 5);
    assert!(rt.send_settling("proj", "bravo", &view("idle", 5), settle));
    // herdr reports the turn running: released and forgotten.
    assert!(!rt.send_settling("proj", "bravo", &view("working", 5), settle));
    assert!(!rt.send_settling("proj", "bravo", &view("idle", 5), settle));

    rt.mark_sent("proj", "bravo", 0, 5);
    assert!(!rt.send_settling("proj", "bravo", &view("blocked", 5), settle));

    // A changed seq (a fast turn whose working was never seen).
    rt.mark_sent("proj", "bravo", 0, 5);
    assert!(!rt.send_settling("proj", "bravo", &view("idle", 7), settle));
    assert!(!rt.send_settling("proj", "bravo", &view("idle", 5), settle));

    // The settle window elapsed.
    rt.mark_sent("proj", "bravo", 0, 5);
    assert!(!rt.send_settling("proj", "bravo", &view("idle", 5), Duration::ZERO));
    // Holds are per worktree.
    rt.mark_sent("proj", "bravo", 0, 5);
    assert!(!rt.send_settling("proj", "charlie", &view("idle", 5), settle));
    assert!(rt.send_settling("proj", "bravo", &view("idle", 5), settle));
}

#[test]
fn state_refusals_name_the_state() {
    assert_eq!(state_refusal("idle", "bravo", "(x)"), None);
    assert_eq!(
        state_refusal("busy", "bravo", "(x)").unwrap(),
        "@bravo is busy; send only goes to an idle session (use --queue to wait for it) (x)"
    );
    assert_eq!(
        state_refusal("waiting", "bravo", "(x)").unwrap(),
        "@bravo is waiting on a prompt; answer it first (or use --queue)"
    );
    assert_eq!(
        state_refusal("offline", "bravo", "(x)").unwrap(),
        "@bravo is offline; start its session first (x)"
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
fn open_set_reports_how_it_changed_and_a_cleared_set_reports_first_again() {
    let rt = DriveRuntime::default();
    assert_eq!(rt.open_set(), None);
    assert_eq!(rt.open_reported_at(), None);
    assert_eq!(rt.set_open(vec!["proj".into()]), OpenChange::First);
    assert!(rt.open_reported_at().is_some(), "every report is stamped");
    assert_eq!(rt.set_open(vec!["proj".into()]), OpenChange::Same);
    assert_eq!(
        rt.set_open(vec!["proj".into(), "other-proj".into()]),
        OpenChange::Changed
    );
    assert_eq!(rt.set_open(vec![]), OpenChange::Changed, "emptied");
    assert_eq!(rt.open_set(), Some(HashSet::new()));
    // The window closed: the next report of the very same set is a first one.
    rt.clear_open();
    assert_eq!(rt.open_set(), None);
    assert_eq!(rt.open_reported_at(), None);
    assert_eq!(rt.set_open(vec![]), OpenChange::First);
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
        engine: fx.env.engine.clone(),
    });
    fx.session("bravo", "idle");
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    let jobs = tauri::async_runtime::block_on(run_pass(&env, &fx.gate, Trigger::Watcher));
    assert!(jobs.is_empty());
    assert_eq!(fx.row(id).status, "pending");
    assert_eq!(
        fx.row(id).wait_reason,
        "window hasn't reported open projects"
    );
    assert!(fx.row(id).seen_at.is_some());
    assert!(fx.writes().is_empty());
    let lines = env.runtime.pass_lines();
    assert!(
        lines[0].contains(" open=unreported "),
        "the pass log says the window hasn't reported: {lines:?}"
    );

    // Once reported, the same request runs.
    env.runtime.set_open(vec!["proj".into()]);
    tauri::async_runtime::block_on(run_pass(&env, &fx.gate, Trigger::Watcher));
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
    // The pass log names the open set and the project, and the refusal.
    let lines = fx.env.runtime.pass_lines();
    let start = lines.iter().find(|l| l.contains(" start ")).unwrap();
    assert!(
        start.contains("project=proj trigger=watcher open=other-proj db="),
        "{start}"
    );
    assert!(
        start.ends_with(&format!("pending=[{id},{queued}]")),
        "{start}"
    );
    assert!(lines.contains(&format!(
        "drive pass {} request {id} (send @bravo by @alpha): refused: project not open in Keitora",
        start.split(' ').nth(2).unwrap()
    )));
}

#[test]
fn pass_log_lines_name_the_open_set_and_each_request() {
    let req = DriveRequest {
        id: 25,
        kind: "send".into(),
        worktree: "bravo".into(),
        requested_by: "alpha".into(),
        payload: "{}".into(),
        queue: false,
        status: "pending".into(),
        reason: String::new(),
        result: String::new(),
        created_at: String::new(),
        resolved_at: None,
        seen_at: None,
        wait_reason: String::new(),
        wait_reason_at: None,
    };
    let open = HashSet::from(["other-proj".to_string(), "alpha-proj".to_string()]);
    assert_eq!(
        pass_start_line(
            7,
            "proj",
            Trigger::Sweep,
            Some(&open),
            "C:/Users/x/proj/tasks.db",
            &[25]
        ),
        "drive pass 7 start project=proj trigger=sweep open=alpha-proj,other-proj \
         db=C:/Users/x/proj/tasks.db pending=[25]"
    );
    assert_eq!(
        pass_start_line(8, "proj", Trigger::OpenSet, Some(&HashSet::new()), "d", &[]),
        "drive pass 8 start project=proj trigger=open_set open=none db=d pending=[]"
    );
    assert_eq!(
        pass_start_line(9, "proj", Trigger::Engine, None, "d", &[1, 2]),
        "drive pass 9 start project=proj trigger=engine open=unreported db=d pending=[1,2]"
    );
    assert_eq!(
        pass_request_line(7, &req, "refused: project not open in Keitora"),
        "drive pass 7 request 25 (send @bravo by @alpha): refused: project not open in Keitora"
    );
    assert_eq!(
        [
            Trigger::Watcher,
            Trigger::Sweep,
            Trigger::Recheck,
            Trigger::Engine,
            Trigger::OpenSet
        ]
        .map(Trigger::as_str),
        ["watcher", "sweep", "recheck", "engine", "open_set"]
    );
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
        serde_json::json!({"awaiting_turn": 1, "delivery_id": 1})
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
    fx.session("bravo", "idle");
    let id = fx.add("send", serde_json::json!({"text": "next"}), false);
    fx.run();
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"awaiting_turn": 2, "delivery_id": 2})
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
        if live {
            fx.session("bravo", state);
        } else {
            // A hook row without a pane: no view, so offline.
            fx.store()
                .session_upsert("bravo", state, None, None)
                .unwrap();
            fx.tick();
        }
        let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
        fx.run();
        assert_eq!(without_detail(refused(&fx.row(id))), want, "{state}");
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
        "expired after 0s: the app never examined it"
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
    fx.begin_delivery("bravo");
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
    fx.begin_delivery("bravo");
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
        serde_json::json!({"awaiting_turn": 1, "delivery_id": 1})
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
        serde_json::json!({"awaiting_turn": 2, "delivery_id": 2})
    );
    assert_eq!(
        fx.writes(),
        vec!["line:one".to_string(), "line:two".to_string()]
    );
}

#[test]
fn a_send_right_after_a_delivery_waits_for_the_turn_to_start() {
    let fx = fixture();
    fx.session("bravo", "idle");
    let first = fx.add("send", serde_json::json!({"text": "one"}), false);
    let second = fx.add("send", serde_json::json!({"text": "two"}), false);
    fx.run();
    applied(&fx.row(first));
    assert_eq!(fx.row(second).status, "pending", "held, not refused");
    assert_eq!(fx.writes(), vec!["line:one".to_string()]);
    assert_eq!(fx.drive().awaiting_turn, Some(1));
    // A tick that shows nothing new keeps the hold.
    fx.tick();
    fx.run();
    assert_eq!(fx.row(second).status, "pending");

    // A fast turn whose working was never seen: herdr's seq moved, the hold lifts.
    fx.store().session_reply_record("bravo", "done").unwrap();
    fx.session("bravo", "idle");
    fx.run();
    assert_eq!(
        applied(&fx.row(second)),
        serde_json::json!({"awaiting_turn": 2, "delivery_id": 2})
    );
}

#[test]
fn send_waits_not_refuses_while_stale() {
    let fx = fixture();
    fx.session("bravo", "idle");
    // Past three cold intervals with no snapshot.
    fx.age(Duration::from_secs(120));
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.run();
    assert_eq!(fx.row(id).status, "pending", "stale: waits");
    assert!(fx.writes().is_empty());
    // herdr down: still waits, never offline.
    fx.env
        .engine
        .mark_link(HerdrLink::Recovering, keitora_tasks::now_ms());
    fx.run();
    assert_eq!(fx.row(id).status, "pending");

    fx.tick();
    fx.run();
    applied(&fx.row(id));
}

#[test]
fn send_waits_while_starting() {
    let fx = fixture();
    fx.session("bravo", "idle");
    fx.fake.set_agent(
        fx.key("bravo"),
        Some(AgentObs {
            launch_pending: true,
            ..obs("w1:bravo", "unknown", 50, None)
        }),
    );
    fx.tick();
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.run();
    assert_eq!(fx.row(id).status, "pending", "starting: waits");

    fx.herdr("bravo", Some("idle"));
    fx.tick();
    fx.run();
    applied(&fx.row(id));
}

#[test]
fn send_waits_until_idle_settles() {
    let fx = fixture();
    fx.session("bravo", "idle");
    // A new idle: one snapshot is not settled yet.
    fx.herdr("bravo", Some("idle"));
    fx.snap();
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.run();
    assert_eq!(fx.row(id).status, "pending", "unsettled: waits");

    fx.snap();
    fx.run();
    applied(&fx.row(id));
}

#[test]
fn a_send_waiting_on_a_transient_condition_is_still_refused_by_a_gate() {
    let fx = fixture();
    fx.session("bravo", "idle");
    fx.age(Duration::from_secs(120));
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.store().drive_set_paused("bravo", true).unwrap();
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "drive of @bravo is paused; resume it before sending"
    );
}

#[test]
fn a_late_session_end_after_a_reset_leaves_the_view_idle() {
    let fx = fixture();
    fx.host
        .cli
        .lock()
        .unwrap()
        .insert("bravo".into(), CliKind::Codex);
    fx.session("bravo", "idle");
    // Codex runs idle after `reset --to codex`; the old session's SessionEnd lands late.
    fx.fake.set_agent(
        fx.key("bravo"),
        Some(AgentObs {
            agent: Some("codex".into()),
            ..obs("w1:bravo", "idle", 60, None)
        }),
    );
    fx.tick();
    fx.store()
        .session_upsert("bravo", "offline", None, None)
        .unwrap();
    fx.tick();
    let sets = SessionSets {
        views: fx.env.engine.views("proj"),
        terminated: HashSet::new(),
        hooks: HashMap::new(),
        at_ms: 0,
    };
    assert_eq!(sets.views.views["bravo"].state, "idle");
    assert_eq!(sets.awake("bravo"), Awake::Yes);

    let wake = fx.add("wake", wake(false), false);
    fx.run();
    assert_eq!(
        applied(&fx.row(wake)),
        serde_json::json!({"provider": "codex", "already_awake": true})
    );
    let send = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.run();
    applied(&fx.row(send));
}

#[test]
fn wake_waits_while_status_is_stale() {
    let fx = fixture();
    fx.session("bravo", "idle");
    fx.age(Duration::from_secs(120));
    let id = fx.add("wake", wake(false), false);
    assert!(fx.pass().is_empty(), "no wake job on stale information");
    assert_eq!(fx.row(id).status, "pending", "not begun");
    assert!(fx.host.restarts.lock().unwrap().is_empty());

    fx.tick();
    fx.run();
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"provider": "claude", "already_awake": true})
    );
}

#[test]
fn wake_waits_until_the_engine_is_ready() {
    let fx = fixture();
    fx.session("bravo", "idle");
    fx.env
        .engine
        .mark_link(HerdrLink::Recovering, keitora_tasks::now_ms());
    let id = fx.add("wake", wake(false), false);
    let send = fx.add("send", wake_send("hi"), false);
    assert!(fx.pass().is_empty(), "neither wakes while herdr is down");
    assert_eq!(fx.row(id).status, "pending");
    assert_eq!(fx.row(send).status, "pending");
    assert!(fx.host.restarts.lock().unwrap().is_empty());

    fx.tick();
    fx.run();
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"provider": "claude", "already_awake": true})
    );
    applied(&fx.row(send));
    assert!(fx.host.restarts.lock().unwrap().is_empty());
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
    fx.begin_delivery("bravo");
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
    assert_eq!(fx.host.log(), vec!["restart"], "provider untouched");
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
        vec!["persist:codex", "worktrees_changed", "restart"]
    );
    assert_eq!(fx.host.cli_of("bravo"), CliKind::Codex);
}

/// The provider the window's restart sees persisted for `wt` when it arrives: the
/// spawn it triggers launches that provider and writes its hooks before the agent
/// starts.
fn provider_at_restart(fx: &Fx, kind: &str, args: serde_json::Value) -> CliKind {
    let id = fx.add(kind, args, false);
    let seen = Arc::new(StdMutex::new(None));
    let at_restart = Arc::clone(&seen);
    run_with_window(fx, move |rt, host| {
        *at_restart.lock().unwrap() = Some(host.cli_of("bravo"));
        rt.report("proj", id, "applied", String::new()).unwrap();
    });
    let cli = seen.lock().unwrap().expect("a restart");
    cli
}

/// `reset --to codex` on a worktree whose provider already reads codex (switched
/// earlier without its hooks) must still launch codex, so the spawn prepares
/// codex's hooks.
#[test]
fn repro_a2_reset_to_codex_prepares_codex_hooks() {
    let fx = fixture();
    fx.host
        .cli
        .lock()
        .unwrap()
        .insert("bravo".into(), CliKind::Codex);
    assert_eq!(
        provider_at_restart(&fx, "reset", serde_json::json!({"to": "codex"})),
        CliKind::Codex
    );
    assert_eq!(fx.host.log(), vec!["restart"], "nothing to persist");
}

#[test]
fn reset_and_wake_to_codex_restart_on_codex() {
    let fx = fixture();
    assert_eq!(
        provider_at_restart(&fx, "reset", serde_json::json!({"to": "codex"})),
        CliKind::Codex
    );
    let fx = fx_terminated();
    assert_eq!(
        provider_at_restart(&fx, "wake", serde_json::json!({"to": "codex"})),
        CliKind::Codex
    );
}

#[test]
fn relaunch_without_a_provider_change_only_restarts() {
    // A plain reset.
    let fx = fixture();
    assert_eq!(
        provider_at_restart(&fx, "reset", serde_json::json!({})),
        CliKind::Claude
    );
    assert_eq!(fx.host.log(), vec!["restart"]);
    // A plain wake.
    let fx = fx_terminated();
    assert_eq!(
        provider_at_restart(&fx, "wake", serde_json::json!({})),
        CliKind::Claude
    );
    assert_eq!(fx.host.log(), vec!["restart"]);
}

#[test]
fn reset_to_agy_refreshes_herdrs_integration_before_the_restart() {
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
            "refresh",
            "restart"
        ]
    );
    assert_eq!(fx.host.cli_of("bravo"), CliKind::Antigravity);
}

#[test]
fn a_plain_reset_or_wake_of_an_agy_worktree_skips_the_integration_refresh() {
    let on_agy = |fx: &Fx| {
        fx.host
            .cli
            .lock()
            .unwrap()
            .insert("bravo".into(), CliKind::Antigravity);
    };
    let fx = fixture();
    on_agy(&fx);
    assert_eq!(
        provider_at_restart(&fx, "reset", serde_json::json!({})),
        CliKind::Antigravity
    );
    assert_eq!(fx.host.log(), vec!["restart"]);
    // Naming the provider it is already on is not a switch either.
    let fx = fixture();
    on_agy(&fx);
    provider_at_restart(&fx, "reset", serde_json::json!({"to": "agy"}));
    assert_eq!(fx.host.log(), vec!["restart"]);
    let fx = fx_terminated();
    on_agy(&fx);
    assert_eq!(
        provider_at_restart(&fx, "wake", serde_json::json!({})),
        CliKind::Antigravity
    );
    assert_eq!(fx.host.log(), vec!["restart"]);
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
    fx.begin_delivery("bravo");
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
        vec!["persist:codex", "worktrees_changed", "restart"]
    );
}

#[test]
fn wake_of_a_running_agent_is_a_no_op() {
    let fx = fixture();
    fx.session("bravo", "busy");
    fx.begin_delivery("bravo");
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
        serde_json::json!({"awaiting_turn": 1, "delivery_id": 1})
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
fn a_woken_send_waiting_for_its_agent_asks_for_no_pass_of_its_own() {
    let fx = fx_with(Timeouts {
        max_age_secs: DRIVE_REQUEST_MAX_AGE_SECS,
        reset: WINDOW,
        swap_confirm: WINDOW,
        swap_run: WINDOW,
        send_settle: Duration::from_millis(300),
    });
    let id = fx.add("send", wake_send("hi"), false);
    run_with_window(&fx, move |rt, _| {
        // Launched, but the agent never shows up.
        rt.report("proj", id, "applied", String::new()).unwrap();
    });
    let pipe = watch(&fx);
    // Inside the grace period every pass waits; none asks for the next one.
    for _ in 0..3 {
        assert!(fx.pass().is_empty());
        assert_eq!(fx.row(id).status, "pending");
    }
    assert!(
        pipe.requested(Trigger::Recheck).is_empty(),
        "no pass loop while the agent is absent"
    );
    // Past it the send is refused, and that asks for nothing either.
    std::thread::sleep(Duration::from_millis(350));
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "@bravo was woken but its agent did not come up; check its pane"
    );
    assert!(pipe.requested(Trigger::Recheck).is_empty());
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
        "@bravo is offline; start its session first (herdr: no agent; no hook rows)"
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
    fx.begin_delivery("bravo");
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

// ---------------------------------------------------------------------------
// Engine-driven reruns and cadence
// ---------------------------------------------------------------------------

/// Runs requested passes for a fixture: each request is kept with the fake time it
/// becomes due, and [`Fx::advance`] runs it then.
#[derive(Default)]
struct FakeRunner {
    /// Fake time since the runner was installed.
    now: StdMutex<Duration>,
    /// (project, due token, fake due time, trigger), in request order.
    queue: StdMutex<Vec<(String, Instant, Duration, Trigger)>>,
    /// Every request, as (project, trigger).
    log: StdMutex<Vec<(String, Trigger)>>,
}

impl PassRunner for FakeRunner {
    fn run_at(&self, project: String, due: Instant, after: Duration, why: Trigger) {
        let at = *self.now.lock().unwrap() + after;
        self.log.lock().unwrap().push((project.clone(), why));
        self.queue.lock().unwrap().push((project, due, at, why));
    }
}

/// The app's request triggers wired to a fixture.
struct Pipe {
    runner: Arc<FakeRunner>,
    /// The pipeline thread's work, ticked by [`Fx::advance`] (when wired).
    pipeline: Option<StdMutex<crate::pipeline::Pipeline>>,
    /// The fixture's watchers, and how often the supervisor restarted one.
    watchers: Arc<crate::watchers::WatcherRegistry>,
    restarts: Arc<std::sync::atomic::AtomicUsize>,
}

impl Pipe {
    /// The projects passes were requested for by `why`, in order.
    fn requested(&self, why: Trigger) -> Vec<String> {
        self.runner
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, w)| *w == why)
            .map(|(p, _)| p.clone())
            .collect()
    }
    fn clear(&self) {
        self.runner.log.lock().unwrap().clear();
    }
}

/// Wire `fx`'s runtime to a fake pass runner and the engine trigger.
fn watch(fx: &Fx) -> Pipe {
    let runner = Arc::new(FakeRunner::default());
    fx.env.runtime.set_runner(runner.clone());
    watch_engine(&fx.env.engine, &fx.env.runtime);
    Pipe {
        runner,
        pipeline: None,
        watchers: Arc::default(),
        restarts: Arc::default(),
    }
}

#[test]
fn a_waiting_send_goes_in_after_an_engine_view_change_with_no_db_write() {
    let fx = fixture();
    let pipe = watch(&fx);
    fx.session("bravo", "idle");
    // A new idle: one snapshot is not settled yet.
    fx.herdr("bravo", Some("idle"));
    fx.snap();
    pipe.clear();
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.run();
    assert_eq!(fx.row(id).status, "pending", "unsettled: waits");
    assert!(fx.env.engine.is_pending("proj"));
    assert_eq!(fx.env.engine.mode(), engine::Mode::Hot, "hot while pending");
    // The settle is time-based: a follow-up pass is scheduled for it.
    assert_eq!(fx.host.scheduled().len(), 1);
    assert_eq!(fx.host.scheduled()[0].0, "proj");

    // herdr's next snapshot settles the idle: a view change, no DB write.
    assert!(pipe.requested(Trigger::Engine).is_empty());
    fx.snap();
    assert_eq!(pipe.requested(Trigger::Engine), ["proj"]);
    fx.run();
    applied(&fx.row(id));
    assert!(!fx.env.engine.is_pending("proj"), "nothing left pending");
}

#[test]
fn view_changes_rerun_only_projects_with_pending_requests() {
    let fx = fixture();
    let pipe = watch(&fx);
    fx.session("bravo", "idle");
    fx.herdr("bravo", Some("working"));
    fx.tick();
    assert!(
        pipe.requested(Trigger::Engine).is_empty(),
        "nothing pending"
    );
}

#[test]
fn a_send_held_by_a_delivery_gets_a_pass_when_the_hold_lapses() {
    let fx = fixture();
    fx.session("bravo", "idle");
    let first = fx.add("send", serde_json::json!({"text": "one"}), false);
    let second = fx.add("send", serde_json::json!({"text": "two"}), false);
    fx.run();
    applied(&fx.row(first));
    assert_eq!(fx.row(second).status, "pending");
    let scheduled = fx.host.scheduled();
    assert_eq!(scheduled.len(), 1);
    assert!(
        scheduled[0].1 > SEND_SETTLE_TIMEOUT - Duration::from_secs(1)
            && scheduled[0].1 <= SEND_SETTLE_TIMEOUT + WAKE_RECHECK_SLACK,
        "at the hold's lapse: {:?}",
        scheduled[0].1
    );
    // Another pass asks for the same deadline: one recheck serves both.
    fx.run();
    assert_eq!(fx.host.scheduled().len(), 1);
}

#[test]
fn the_window_report_sets_the_warm_cadence() {
    let fx = fixture();
    let rt = DriveRuntime::default();
    assert_eq!(fx.env.engine.mode(), engine::Mode::Cold);
    assert_eq!(
        set_open_projects(&rt, &fx.env.engine, vec!["proj".into()]),
        OpenChange::First
    );
    assert_eq!(fx.env.engine.mode(), engine::Mode::Warm);
    assert_eq!(
        set_open_projects(&rt, &fx.env.engine, vec![]),
        OpenChange::Changed
    );
    assert_eq!(fx.env.engine.mode(), engine::Mode::Cold);
}

// ---------------------------------------------------------------------------
// Delivery baseline
// ---------------------------------------------------------------------------

/// A settled idle view of `bravo` at `scs`.
fn settled_view(scs: i64) -> KeyView {
    KeyView::View(StatusSession {
        project: "proj".into(),
        worktree: "bravo".into(),
        state: "idle".into(),
        agent: "present".into(),
        agent_kind: Some("claude".into()),
        herdr_status: Some("idle".into()),
        settled: true,
        stale: false,
        interactive_ready: Some(true),
        pane_id: Some("w1:bravo".into()),
        terminal_id: Some("term-w1:bravo".into()),
        agent_session: Some("s-1".into()),
        state_change_seq: Some(scs),
        completion_seq: None,
        brokered: false,
        escalated_reason: None,
        last_change_at: 0,
    })
}

#[test]
fn fresh_read_hold_needs_a_settled_idle_at_the_same_seq() {
    let idle = obs("w1:bravo", "idle", 5, None);
    assert_eq!(fresh_read_hold(&idle, &settled_view(5), "bravo"), None);
    let done = obs("w1:bravo", "done", 5, Some(3));
    assert_eq!(fresh_read_hold(&done, &settled_view(5), "bravo"), None);

    let hold = |o: &AgentObs, v: &KeyView| fresh_read_hold(o, v, "bravo").unwrap();
    let working = obs("w1:bravo", "working", 6, None);
    assert_eq!(
        hold(&working, &settled_view(5)),
        FreshHold {
            wait: "waiting for @bravo to be idle (herdr: working)".into(),
            status: "working".into(),
            transient: false,
        }
    );
    let not_ready = AgentObs {
        interactive_ready: Some(false),
        ..idle.clone()
    };
    assert_eq!(
        hold(&not_ready, &settled_view(5)).wait,
        "waiting for @bravo to be idle (herdr: starting)"
    );
    let pending = AgentObs {
        launch_pending: true,
        ..idle.clone()
    };
    assert_eq!(hold(&pending, &settled_view(5)).status, "starting");
    assert!(hold(&pending, &settled_view(5)).transient);
    assert!(hold(&not_ready, &settled_view(5)).transient);
    // Idle, but not at the settled view's seq (moved since), or no settled view.
    let settle = "waiting for @bravo to settle";
    assert_eq!(hold(&idle, &settled_view(4)).wait, settle);
    assert!(hold(&idle, &settled_view(4)).transient);
    assert_eq!(hold(&idle, &KeyView::NotReady).wait, settle);
    assert_eq!(hold(&idle, &KeyView::Missing).wait, settle);
    let KeyView::View(v) = settled_view(5) else {
        unreachable!()
    };
    let stale = KeyView::View(StatusSession {
        stale: true,
        ..v.clone()
    });
    assert_eq!(hold(&idle, &stale).wait, settle);
    let unsettled = KeyView::View(StatusSession {
        settled: false,
        ..v
    });
    assert_eq!(hold(&idle, &unsettled).wait, settle);
}

#[test]
fn baseline_of_copies_the_fresh_read() {
    let o = AgentObs {
        agent: Some("codex".into()),
        ..obs("w1:bravo", "done", 9, Some(4))
    };
    assert_eq!(
        baseline_of(&o, 1_234),
        Baseline {
            agent: Some("codex".into()),
            pane_id: "w1:bravo".into(),
            terminal_id: "term-w1:bravo".into(),
            agent_session: Some("s-1".into()),
            state_change_seq: 9,
            completion_seq: Some(4),
            status: "done".into(),
            delivered_at_ms: 1_234,
        }
    );
}

#[test]
fn send_captures_a_fresh_baseline_and_persists_it() {
    let fx = fixture();
    fx.session("bravo", "idle");
    // herdr stamped a completion the cached view hasn't seen (same seq).
    let scs = *fx.scs.lock().unwrap();
    fx.fake
        .set_agent(fx.key("bravo"), Some(obs("w1:bravo", "idle", scs, Some(7))));
    let before = keitora_tasks::now_ms();
    let id = fx.add("send", serde_json::json!({"text": "one"}), false);
    fx.run();
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"awaiting_turn": 1, "delivery_id": 1})
    );
    let del = fx.store().drive_delivery_get("bravo").unwrap().unwrap();
    assert_eq!(del.awaiting_turn, Some(1));
    assert_eq!(del.delivery_id, Some(1));
    let b = del.baseline.expect("baseline stored");
    assert!(b.delivered_at_ms >= before);
    assert_eq!(
        b,
        Baseline {
            delivered_at_ms: b.delivered_at_ms,
            ..baseline_of(&obs("w1:bravo", "idle", scs, Some(7)), 0)
        },
        "the fresh read, not the cached view (cs None)"
    );
    assert!(fx.fake.calls().contains(&Call::AgentSnapshot {
        key: fx.key("bravo")
    }));

    // The next delivery takes the next id.
    fx.store().session_reply_record("bravo", "done").unwrap();
    fx.session("bravo", "idle");
    let id = fx.add("send", serde_json::json!({"text": "two"}), false);
    fx.run();
    assert_eq!(
        applied(&fx.row(id)),
        serde_json::json!({"awaiting_turn": 2, "delivery_id": 2})
    );
}

#[test]
fn send_requeues_a_queued_send_when_the_fresh_read_is_not_idle() {
    let fx = fixture();
    fx.session("bravo", "idle");
    // herdr moved to working; the engine hasn't ticked, so the gate still says go.
    fx.herdr("bravo", Some("working"));
    let id = fx.add("send", serde_json::json!({"text": "one"}), true);
    fx.run();
    assert_eq!(fx.row(id).status, "pending", "deferred, not refused");
    assert_eq!(
        fx.row(id).wait_reason,
        "waiting for @bravo to be idle (herdr: working)",
        "the deferred send says why it waits"
    );
    assert!(fx.env.runtime.pass_lines().iter().any(|l| l.ends_with(&format!(
        "request {id} (send @bravo by @alpha): deferred: waiting for @bravo to be idle (herdr: working)"
    ))));
    assert!(fx.writes().is_empty());
    assert_eq!(fx.drive().awaiting_turn, None);

    // Idle again at a new seq the engine hasn't settled: still deferred.
    fx.herdr("bravo", Some("idle"));
    fx.run();
    assert_eq!(fx.row(id).status, "pending");

    // Settled, but the agent is not ready for input.
    fx.tick();
    let scs = *fx.scs.lock().unwrap();
    fx.fake.set_agent(
        fx.key("bravo"),
        Some(AgentObs {
            interactive_ready: Some(false),
            ..obs("w1:bravo", "idle", scs, None)
        }),
    );
    fx.run();
    assert_eq!(fx.row(id).status, "pending");
    assert!(fx.writes().is_empty());

    // Ready: it goes in.
    fx.fake
        .set_agent(fx.key("bravo"), Some(obs("w1:bravo", "idle", scs, None)));
    fx.run();
    applied(&fx.row(id));
    assert_eq!(fx.writes(), vec!["line:one".to_string()]);
}

#[test]
fn a_deferred_send_gets_its_own_recheck() {
    let fx = fixture();
    fx.session("bravo", "idle");
    // Settled idle in the engine, but the fresh read says not ready for input: no
    // view will change when it becomes ready.
    let scs = *fx.scs.lock().unwrap();
    fx.fake.set_agent(
        fx.key("bravo"),
        Some(AgentObs {
            interactive_ready: Some(false),
            ..obs("w1:bravo", "idle", scs, None)
        }),
    );
    let id = fx.add("send", serde_json::json!({"text": "one"}), true);
    fx.run();
    assert_eq!(fx.row(id).status, "pending", "deferred");
    // The requeue is the pass's own write: the watcher path skips it.
    assert!(fx.env.runtime.unchanged_since_pass(&fx.env.db_path));
    let scheduled = fx.host.scheduled();
    assert_eq!(scheduled.len(), 1, "{scheduled:?}");
    assert_eq!(scheduled[0].0, "proj");
    assert_eq!(scheduled[0].1, engine::SETTLE + WAKE_RECHECK_SLACK);
    // Ready by the recheck: it goes in.
    fx.fake
        .set_agent(fx.key("bravo"), Some(obs("w1:bravo", "idle", scs, None)));
    fx.run();
    applied(&fx.row(id));
}

#[test]
fn send_refuses_a_direct_send_when_the_fresh_read_is_not_idle() {
    let fx = fixture();
    fx.session("bravo", "idle");
    fx.herdr("bravo", Some("working"));
    let id = fx.add("send", serde_json::json!({"text": "one"}), false);
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "@bravo changed state just before delivery (herdr: working); use --queue to wait"
    );

    fx.tick();
    fx.herdr("bravo", Some("idle"));
    fx.tick();
    let scs = *fx.scs.lock().unwrap();
    fx.fake.set_agent(
        fx.key("bravo"),
        Some(AgentObs {
            launch_pending: true,
            ..obs("w1:bravo", "idle", scs, None)
        }),
    );
    let id = fx.add("send", serde_json::json!({"text": "two"}), false);
    fx.run();
    assert_eq!(
        fx.row(id).status,
        "pending",
        "an agent still starting is transient: deferred even without --queue"
    );
    assert!(fx.store().drive_request_cancel(id, "test").unwrap());

    // The agent left between the tick and the read: offline.
    fx.fake.set_agent(
        fx.key("bravo"),
        Some(AgentObs {
            agent: None,
            ..obs("w1:bravo", "unknown", scs, None)
        }),
    );
    let id = fx.add("send", serde_json::json!({"text": "three"}), false);
    fx.run();
    let row = fx.row(id);
    let text = refused(&row);
    assert_eq!(
        without_detail(text),
        "@bravo is offline; start its session first"
    );
    assert!(
        text.contains("(herdr: no agent; last hook write "),
        "the fresh read saw no agent; the hook row is bravo's: {text}"
    );
    assert!(fx.writes().is_empty());
    assert_eq!(fx.drive().awaiting_turn, None);
}

#[test]
fn send_defers_a_direct_send_when_the_fresh_read_has_not_settled() {
    let fx = fixture();
    fx.session("bravo", "idle");
    // Idle, but herdr's seq moved past the engine's settled view: transient.
    let scs = *fx.scs.lock().unwrap();
    fx.fake.set_agent(
        fx.key("bravo"),
        Some(obs("w1:bravo", "idle", scs + 1, None)),
    );
    let id = fx.add("send", serde_json::json!({"text": "one"}), false);
    fx.run();
    assert_eq!(fx.row(id).status, "pending", "deferred, not refused");
    assert!(fx.writes().is_empty());
    assert_eq!(fx.drive().awaiting_turn, None);

    // The engine settles at that seq: it goes in.
    fx.fake
        .set_agent(fx.key("bravo"), Some(obs("w1:bravo", "idle", scs, None)));
    fx.run();
    applied(&fx.row(id));
    assert_eq!(fx.writes(), vec!["line:one".to_string()]);
}

#[test]
fn failed_typing_restores_the_previous_delivery() {
    let fx = fixture();
    fx.session("bravo", "idle");
    fx.begin_delivery("bravo");
    let prev = fx.store().drive_delivery_get("bravo").unwrap().unwrap();
    assert_eq!(prev.delivery_id, Some(1));
    fx.fake.push_write_line(Err(PtyBackendError::NotConnected));
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "could not send to @bravo (herdr unreachable)"
    );
    assert_eq!(
        fx.store().drive_delivery_get("bravo").unwrap().unwrap(),
        prev,
        "all four columns are back"
    );
}

#[test]
fn send_refuses_when_the_previous_delivery_is_unreadable() {
    let fx = fixture();
    fx.session("bravo", "idle");
    // A row the store can't decode: a failed paste would have nothing to restore.
    let conn = rusqlite::Connection::open(&fx.env.db_path).unwrap();
    conn.execute(
        "UPDATE drives SET delivery_id = 'garbled' WHERE driven = 'bravo'",
        [],
    )
    .unwrap();
    drop(conn);
    assert!(fx.store().drive_delivery_get("bravo").is_err());
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.run();
    assert!(
        refused(&fx.row(id)).starts_with("could not read the delivery"),
        "{}",
        refused(&fx.row(id))
    );
    assert!(fx.writes().is_empty(), "nothing typed");
}

#[test]
fn settling_released_by_a_turn_row() {
    let fx = fixture();
    fx.session("bravo", "idle");
    let first = fx.add("send", serde_json::json!({"text": "one"}), false);
    let second = fx.add("send", serde_json::json!({"text": "two"}), false);
    fx.run();
    let delivery = applied(&fx.row(first))["delivery_id"].as_i64().unwrap();
    assert_eq!(fx.row(second).status, "pending", "held by the send");
    fx.run();
    assert_eq!(fx.row(second).status, "pending");

    // The engine recorded the delivery's turn (herdr never showed it working).
    fx.store()
        .drive_turn_insert(&keitora_tasks::NewDriveTurn {
            driven: "bravo",
            kind: "driver",
            delivery_id: Some(delivery),
            outcome: "finished",
            signal: "hook",
            clause: "stop_hook",
            completion_seq: None,
            state_change_seq: None,
            reply: "done",
            reply_source: "hook",
            caveat: None,
        })
        .unwrap()
        .unwrap();
    fx.run();
    applied(&fx.row(second));
    assert_eq!(
        fx.writes(),
        vec!["line:one".to_string(), "line:two".to_string()]
    );
}

#[test]
fn send_settling_or_turned_asks_only_for_a_recorded_delivery() {
    let rt = DriveRuntime::default();
    let KeyView::View(v) = settled_view(5) else {
        unreachable!()
    };
    let settle = Duration::from_secs(30);
    // A dispatch hold (no delivery id) never asks.
    rt.mark_sent("proj", "bravo", 0, 5);
    assert!(rt.send_settling_or_turned("proj", "bravo", &v, settle, |_| panic!("asked")));
    rt.mark_sent("proj", "bravo", 3, 5);
    assert!(
        rt.send_settling_or_turned("proj", "bravo", &v, settle, |id| {
            assert_eq!(id, 3);
            false
        })
    );
    assert!(!rt.send_settling_or_turned("proj", "bravo", &v, settle, |_| true));
    assert!(!rt.send_settling("proj", "bravo", &v, settle), "forgotten");
}

// ---------------------------------------------------------------------------
// Reproductions: requests left unclaimed while the app runs
// ---------------------------------------------------------------------------

impl Fx {
    /// The fixture's project as the registry lists it (its DB is the fixture's).
    fn registered(&self) -> ProjectConfig {
        let mut pc = self.env.config.clone();
        pc.tasks.db_path = self.env.db_path.clone().into();
        pc
    }

    /// Wire the app's request triggers to this fixture: the engine trigger, the
    /// requested-pass runner and the pipeline thread's sweep and supervisor (over the
    /// fixture's project and its own watcher registry). The DB watchers never run.
    fn pipeline(&self) -> Pipe {
        use crate::watchers::{WatchState, WatcherHealth, WatcherId, WatcherKind};
        let mut pipe = watch(self);
        let (weak, count) = (Arc::downgrade(&pipe.watchers), Arc::clone(&pipe.restarts));
        pipe.watchers.set_restart(
            WatcherKind::Bg,
            Arc::new(move |project: &str| {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // A respawned watcher that, like the old one, never sees an event.
                let h = Arc::new(WatcherHealth::default());
                h.force_state(WatchState::Running);
                weak.upgrade()
                    .unwrap()
                    .register(WatcherId::new(WatcherKind::Bg, project), h);
                true
            }),
        );
        let pc = self.registered();
        pipe.pipeline = Some(StdMutex::new(crate::pipeline::Pipeline::new(
            Box::new(move || vec![("proj".to_string(), pc.clone())]),
            Arc::new(crate::project_stores::ReadStores::default()),
            self.env.runtime.clone(),
            Arc::clone(&pipe.watchers),
            None,
        )));
        pipe
    }

    /// The trigger of the pass that resolved (or spawned the job of) request `id`.
    fn resolved_by(&self, id: i64) -> Option<String> {
        let lines = self.env.runtime.pass_lines();
        let outcome = lines.iter().find(|l| {
            l.contains(&format!(" request {id} "))
                && (l.ends_with(": applied")
                    || l.contains(": refused: ")
                    || l.ends_with(": job: spawned"))
        })?;
        let pass = outcome.split(' ').nth(2)?;
        let start = lines
            .iter()
            .find(|l| l.starts_with(&format!("drive pass {pass} start ")))?;
        let trigger = start.split(" trigger=").nth(1)?.split(' ').next()?;
        Some(trigger.to_string())
    }

    /// Run every requested pass due by the runner's fake time, jobs included, until
    /// none is left due.
    fn run_due(&self, pipe: &Pipe) {
        loop {
            let now = *pipe.runner.now.lock().unwrap();
            let due: Vec<(String, Instant, Duration, Trigger)> = {
                let mut q = pipe.runner.queue.lock().unwrap();
                let (due, later): (Vec<_>, Vec<_>) = q.drain(..).partition(|e| e.2 <= now);
                *q = later;
                due
            };
            if due.is_empty() {
                return;
            }
            for (project, token, _, why) in due {
                if !self.env.runtime.take_due(&project, token) {
                    continue; // a sooner request ran it
                }
                let jobs = tauri::async_runtime::block_on(run_pass(&self.env, &self.gate, why));
                for job in jobs {
                    tauri::async_runtime::block_on(job);
                }
            }
        }
    }

    /// Let `secs` seconds pass: herdr keeps answering every second (no change unless
    /// a test set one), and every requested pass runs once due.
    fn advance(&self, pipe: &Pipe, secs: u64) {
        for _ in 0..secs {
            let now = {
                let mut now = pipe.runner.now.lock().unwrap();
                *now += Duration::from_secs(1);
                *now
            };
            self.snap();
            if let Some(p) = &pipe.pipeline {
                p.lock()
                    .unwrap()
                    .tick(self.t0 + now, self.t0_ms + now.as_millis() as i64);
            }
            self.run_due(pipe);
        }
    }
}

#[test]
fn repro_c_herdr_only_idle_flip_applies_a_queued_send_without_a_db_write() {
    let fx = fixture();
    let pipe = fx.pipeline();
    fx.session("bravo", "busy");
    // A queued send filed while the target works: the DB write runs a pass, it waits.
    let id = fx.add("send", serde_json::json!({"text": "hi"}), true);
    fx.run();
    assert_eq!(fx.row(id).status, "pending");
    assert_eq!(
        fx.row(id).wait_reason,
        "waiting for @bravo to be idle (herdr: working)"
    );
    // herdr alone sees the turn end: no tasks.db write, no window change.
    fx.herdr("bravo", Some("idle"));
    fx.tick();
    assert_eq!(pipe.requested(Trigger::Engine), ["proj"]);
    fx.advance(&pipe, 30);
    applied(&fx.row(id));
    assert_eq!(fx.resolved_by(id).as_deref(), Some("engine"));
}

#[test]
fn repro_c_first_snapshot_without_sessions_reruns_a_waiting_wake() {
    let fx = fx_cold(Timeouts {
        max_age_secs: DRIVE_REQUEST_MAX_AGE_SECS,
        reset: WINDOW,
        swap_confirm: WINDOW,
        swap_run: WINDOW,
        send_settle: SEND_SETTLE_TIMEOUT,
    });
    let pipe = fx.pipeline();
    // A wake filed before herdr's first snapshot: no fresh answer, it waits.
    let id = fx.add("wake", serde_json::json!({}), false);
    fx.run();
    assert_eq!(fx.row(id).status, "pending");
    assert!(fx.host.log().is_empty(), "nothing started on no answer");
    // The first snapshot lists no pane of the project: no view of it changes, and
    // nothing is written to tasks.db.
    fx.snap();
    assert_eq!(pipe.requested(Trigger::Engine), ["proj"]);
    fx.advance(&pipe, 5);
    assert_eq!(fx.host.log(), ["restart"], "the wake started");
    assert_eq!(fx.resolved_by(id).as_deref(), Some("engine"));
}

#[test]
fn repro_r_ready_lane_request_in_an_open_project_is_claimed() {
    use crate::watchers::{WatcherHealth, WatcherId, WatcherKind};
    let fx = fixture();
    let pipe = fx.pipeline();
    // The window reports `proj` open (the fixture's report) and bravo is idle.
    assert_eq!(
        fx.env.runtime.open_set(),
        Some(HashSet::from(["proj".to_string()]))
    );
    fx.session("bravo", "idle");
    // The project's watcher is dead: no file event ever arrives.
    let dead = Arc::new(WatcherHealth::default());
    dead.mark_exited("event channel disconnected");
    pipe.watchers
        .register(WatcherId::new(WatcherKind::Bg, "proj"), dead);
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.advance(&pipe, 5);
    let row = fx.row(id);
    if row.status != "applied" {
        assert!(
            !row.wait_reason.is_empty(),
            "pending with no reason: {row:?}"
        );
        let lines = fx.env.runtime.pass_lines();
        assert!(
            lines.iter().any(|l| l.contains("open=")
                && l.contains("db=")
                && l.contains("trigger=")
                && l.contains(&format!("pending=[{id}]"))),
            "no pass-log line for request {id}: {lines:?}"
        );
    }
    applied(&row);
    assert_eq!(fx.resolved_by(id).as_deref(), Some("sweep"));
    assert_eq!(
        pipe.restarts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the dead watcher was restarted meanwhile"
    );
}

#[test]
fn an_open_project_claims_a_ready_lanes_request_exactly_once() {
    let fx = Arc::new(fixture());
    fx.session("bravo", "idle");
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    // Two passes list the request while a dispatch holds the gate.
    let held = fx.gate.0.try_lock().unwrap();
    let spawn_pass = |why: Trigger| {
        let fx = Arc::clone(&fx);
        std::thread::spawn(move || {
            let jobs = tauri::async_runtime::block_on(run_pass(&fx.env, &fx.gate, why));
            assert!(jobs.is_empty());
        })
    };
    let starts = |fx: &Fx| {
        fx.env
            .runtime
            .pass_lines()
            .iter()
            .filter(|l| l.contains(" start "))
            .count()
    };
    let wait_for = |cond: &dyn Fn() -> bool| {
        for _ in 0..300 {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out");
    };
    let first = spawn_pass(Trigger::Watcher);
    wait_for(&|| fx.row(id).wait_reason == "waiting for the dispatch gate");
    let second = spawn_pass(Trigger::Sweep);
    wait_for(&|| starts(&fx) == 2);
    // Let the second pass queue on the gate behind the first.
    std::thread::sleep(Duration::from_millis(200));
    drop(held);
    first.join().unwrap();
    second.join().unwrap();

    applied(&fx.row(id));
    assert_eq!(fx.writes(), vec!["line:hi".to_string()], "typed once");
    let outcomes: Vec<String> = fx
        .env
        .runtime
        .pass_lines()
        .into_iter()
        .filter(|l| l.contains(&format!(" request {id} ")) && !l.contains("waiting: "))
        .map(|l| l.rsplit("): ").next().unwrap().to_string())
        .collect();
    let mut sorted = outcomes.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        ["applied", "skipped: taken by another pass"],
        "{outcomes:?}"
    );
}

#[test]
fn a_closed_and_reopened_window_picks_up_a_request_filed_meanwhile() {
    let fx = fixture();
    let pipe = watch(&fx);
    let stores = crate::project_stores::ReadStores::default();
    let pc = fx.registered();
    fx.session("bravo", "idle");
    // The window closes; a request is filed while it is gone.
    fx.env.runtime.clear_open();
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    fx.run();
    assert_eq!(
        fx.row(id).wait_reason,
        "window hasn't reported open projects"
    );
    pipe.clear();
    // The reopened window reports the very same set: a first report, and a pass.
    let change = report_open_projects(
        &fx.env.runtime,
        &fx.env.engine,
        vec!["proj".into()],
        std::iter::once(&pc),
        &stores,
    );
    assert_eq!(change, OpenChange::First);
    assert_eq!(pipe.requested(Trigger::OpenSet), ["proj"]);
    fx.advance(&pipe, 1);
    applied(&fx.row(id));
    assert_eq!(fx.resolved_by(id).as_deref(), Some("open_set"));
}

#[test]
fn switching_away_and_back_picks_up_a_request_filed_meanwhile() {
    let fx = fixture();
    let pipe = watch(&fx);
    let stores = crate::project_stores::ReadStores::default();
    let pc = fx.registered();
    fx.session("bravo", "idle");
    let report = |projects: Vec<String>| {
        report_open_projects(
            &fx.env.runtime,
            &fx.env.engine,
            projects,
            std::iter::once(&pc),
            &stores,
        )
    };
    // The human switches to another project: nothing pending, no pass.
    assert_eq!(report(vec!["other-proj".into()]), OpenChange::Changed);
    assert!(pipe.requested(Trigger::OpenSet).is_empty());
    // A request is filed meanwhile; no watcher event reaches the runner.
    let id = fx.add("send", serde_json::json!({"text": "hi"}), false);
    // Back to `proj`: the report gives it a pass.
    assert_eq!(report(vec!["proj".into()]), OpenChange::Changed);
    assert_eq!(pipe.requested(Trigger::OpenSet), ["proj"]);
    fx.advance(&pipe, 1);
    applied(&fx.row(id));
    // A same-set re-report changes nothing the runner acts on: no pass.
    pipe.clear();
    fx.add("send", serde_json::json!({"text": "again"}), true);
    assert_eq!(report(vec!["proj".into()]), OpenChange::Same);
    assert!(pipe.requested(Trigger::OpenSet).is_empty());
}

// ---------------------------------------------------------------------------
// Request passes
// ---------------------------------------------------------------------------

/// A pass runner that only records.
#[derive(Default)]
struct Recorder(StdMutex<Vec<(String, Instant, Trigger)>>);

impl PassRunner for Recorder {
    fn run_at(&self, project: String, due: Instant, _after: Duration, why: Trigger) {
        self.0.lock().unwrap().push((project, due, why));
    }
}

#[test]
fn request_pass_coalesces_a_burst_and_the_earliest_due_wins() {
    let rt = DriveRuntime::default();
    let t0 = Instant::now();
    assert!(
        !rt.request_pass_at(t0, "proj", Duration::ZERO, Trigger::Engine),
        "nothing runs passes yet"
    );
    let rec = Arc::new(Recorder::default());
    rt.set_runner(rec.clone());

    // Three requests inside the coalesce window: one pass.
    assert!(rt.request_pass_at(t0, "proj", Duration::ZERO, Trigger::Watcher));
    assert!(!rt.request_pass_at(
        t0 + Duration::from_millis(50),
        "proj",
        Duration::ZERO,
        Trigger::Engine
    ));
    assert!(!rt.request_pass_at(
        t0 + Duration::from_millis(149),
        "proj",
        Duration::ZERO,
        Trigger::Sweep
    ));
    assert_eq!(rec.0.lock().unwrap().len(), 1);
    let first = rec.0.lock().unwrap()[0].1;
    assert_eq!(first, t0 + PASS_COALESCE);
    // Another project is its own key.
    assert!(rt.request_pass_at(t0, "other-proj", Duration::ZERO, Trigger::Watcher));
    assert!(rt.take_due("proj", first));
    assert!(rt.take_due("other-proj", t0 + PASS_COALESCE));

    // A later request, then a sooner one: both run.
    assert!(rt.request_pass_at(t0, "proj", Duration::from_secs(2), Trigger::Recheck));
    assert!(rt.request_pass_at(t0, "proj", Duration::ZERO, Trigger::Engine));
    let q = rec.0.lock().unwrap().clone();
    let (later, sooner) = (q[q.len() - 2].1, q[q.len() - 1].1);
    assert!(sooner < later);
    assert!(rt.take_due("proj", sooner), "the sooner timer runs a pass");
    assert!(rt.take_due("proj", later), "and so does the later one");
    assert!(!rt.take_due("proj", later), "once");
}

#[test]
fn a_timed_recheck_still_fires_after_a_sooner_pass() {
    let rt = DriveRuntime::default();
    let rec = Arc::new(Recorder::default());
    rt.set_runner(rec.clone());
    let t0 = Instant::now();
    let recheck = Duration::from_millis(30_500);
    // A send's settle recheck, then an engine change a second later.
    assert!(rt.request_pass_at(t0, "proj", recheck, Trigger::Recheck));
    let s = Duration::from_secs(1);
    assert!(rt.request_pass_at(t0 + s, "proj", Duration::ZERO, Trigger::Engine));
    let dues: Vec<Instant> = rec.0.lock().unwrap().iter().map(|e| e.1).collect();
    assert_eq!(dues, [t0 + recheck, t0 + s + PASS_COALESCE]);
    assert!(rt.take_due("proj", dues[1]), "the engine pass runs");
    assert!(rt.take_due("proj", t0 + recheck), "the recheck still fires");

    // The other order: a pass queued first doesn't swallow a later recheck.
    assert!(rt.request_pass_at(t0, "proj", Duration::ZERO, Trigger::Engine));
    assert!(rt.request_pass_at(t0, "proj", recheck, Trigger::Recheck));
    assert!(rt.take_due("proj", t0 + PASS_COALESCE));
    assert!(rt.take_due("proj", t0 + recheck));
    // A request just after a queued pass's due still rides on it.
    assert!(rt.request_pass_at(t0, "proj", recheck, Trigger::Recheck));
    assert!(!rt.request_pass_at(
        t0 + Duration::from_millis(100),
        "proj",
        recheck,
        Trigger::Recheck
    ));
}

#[test]
fn wait_reason_for_pins_every_string() {
    let rows = [
        (WaitFact::Unreported, "window hasn't reported open projects"),
        (
            WaitFact::Hold(Hold::FirstSnapshot, "bravo"),
            "waiting for first herdr snapshot",
        ),
        (
            WaitFact::Hold(Hold::Link(HerdrLink::Recovering), "bravo"),
            "waiting for herdr (recovering)",
        ),
        (
            WaitFact::Hold(Hold::Link(HerdrLink::Down), "bravo"),
            "waiting for herdr (down)",
        ),
        (
            WaitFact::Hold(Hold::Link(HerdrLink::Unavailable), "bravo"),
            "waiting for herdr (unavailable)",
        ),
        (
            WaitFact::Hold(Hold::Stale, "bravo"),
            "waiting for herdr (stale snapshot)",
        ),
        (
            WaitFact::Hold(Hold::Starting, "bravo"),
            "waiting for @bravo to be idle (herdr: starting)",
        ),
        (
            WaitFact::Hold(Hold::Unsettled, "bravo"),
            "waiting for @bravo to settle",
        ),
        (
            WaitFact::Hold(Hold::SendSettling, "bravo"),
            "waiting for @bravo to start its turn (send settling)",
        ),
        (
            WaitFact::NotIdle {
                worktree: "bravo",
                status: "working",
            },
            "waiting for @bravo to be idle (herdr: working)",
        ),
        (
            WaitFact::NotIdle {
                worktree: "bravo",
                status: "no agent",
            },
            "waiting for @bravo to be idle (herdr: no agent)",
        ),
        (
            WaitFact::Woken("bravo"),
            "waiting for @bravo's agent to come up (woken)",
        ),
        (WaitFact::SlotHeld, "waiting on the wake slot"),
        (WaitFact::DispatchGate, "waiting for the dispatch gate"),
    ];
    for (fact, text) in rows {
        assert_eq!(wait_reason_for(&fact), text, "{fact:?}");
    }

    // The send gate's wait texts are the same strings.
    let sets = |link: HerdrLink, ready: bool| SessionSets {
        views: ProjectViews {
            ready,
            link,
            views: HashMap::new(),
        },
        terminated: HashSet::new(),
        hooks: HashMap::new(),
        at_ms: 0,
    };
    assert_eq!(
        sets(HerdrLink::Down, false).send_gate("bravo", false, |_| false),
        SendGate::Wait(wait_reason_for(&WaitFact::Hold(
            Hold::Link(HerdrLink::Down),
            "bravo"
        )))
    );
    assert_eq!(
        sets(HerdrLink::Starting, false).send_gate("bravo", true, |_| false),
        SendGate::Wait(wait_reason_for(&WaitFact::Hold(
            Hold::FirstSnapshot,
            "bravo"
        )))
    );
    // And a fresh read's hold.
    let busy = fresh_read_hold(
        &obs("w1:bravo", "working", 3, None),
        &KeyView::NotReady,
        "bravo",
    )
    .unwrap();
    assert_eq!(
        busy.wait,
        wait_reason_for(&WaitFact::NotIdle {
            worktree: "bravo",
            status: "working"
        })
    );
}

#[test]
fn waiting_requests_record_why_and_log_once_per_reason() {
    let fx = fixture();
    let reader = fx.store();
    fx.session("bravo", "busy");
    let id = fx.add("send", serde_json::json!({"text": "later"}), true);
    fx.run();
    let row = fx.row(id);
    assert_eq!(row.status, "pending");
    assert_eq!(
        row.wait_reason,
        "waiting for @bravo to be idle (herdr: working)"
    );
    let waiting = |fx: &Fx| {
        fx.env
            .runtime
            .pass_lines()
            .iter()
            .filter(|l| l.contains(&format!("request {id} ")) && l.contains("waiting: "))
            .count()
    };
    assert_eq!(waiting(&fx), 1);

    // Idle passes with nothing new: no row write, no new waiting line.
    let v = reader.data_version().unwrap();
    for _ in 0..3 {
        fx.run();
    }
    assert_eq!(
        reader.data_version().unwrap(),
        v,
        "idle passes write nothing"
    );
    assert_eq!(waiting(&fx), 1, "logged once while the reason holds");

    // A new reason is written and logged.
    fx.session("bravo", "waiting");
    fx.run();
    assert_eq!(
        fx.row(id).wait_reason,
        "waiting for @bravo to be idle (herdr: blocked)"
    );
    assert_eq!(waiting(&fx), 2);
}

#[test]
fn expiry_names_the_reason_the_request_waited_for() {
    let fx = fx_with(Timeouts {
        max_age_secs: 1,
        reset: WINDOW,
        swap_confirm: WINDOW,
        swap_run: WINDOW,
        send_settle: SEND_SETTLE_TIMEOUT,
    });
    fx.session("bravo", "busy");
    let id = fx.add("send", serde_json::json!({"text": "later"}), true);
    fx.run();
    assert_eq!(fx.row(id).status, "pending");
    // `created_at` has one-second resolution.
    std::thread::sleep(Duration::from_millis(2100));
    fx.run();
    assert_eq!(
        refused(&fx.row(id)),
        "expired after 1s: waiting for @bravo to be idle (herdr: working)"
    );
}

#[test]
fn a_read_resolves_while_an_older_send_waits_on_the_dispatch_gate() {
    let fx = Arc::new(fixture());
    fx.session("bravo", "idle");
    let send = fx.add("send", serde_json::json!({"text": "hi"}), false);
    let read = fx.add("read", serde_json::json!({"lines": 5}), false);
    fx.fake.push_read_history(Ok("line 1".into()));
    // A dispatch holds the gate.
    let held = fx.gate.0.try_lock().unwrap();
    let runner = Arc::clone(&fx);
    let pass = std::thread::spawn(move || {
        let jobs =
            tauri::async_runtime::block_on(run_pass(&runner.env, &runner.gate, Trigger::Watcher));
        assert!(jobs.is_empty());
    });
    // The send is held on the gate and the read, filed after it, goes ahead.
    let mut read_done = false;
    for _ in 0..300 {
        if fx.row(read).status == "applied" {
            read_done = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(read_done, "the read didn't wait");
    assert_eq!(
        fx.row(send).wait_reason,
        "waiting for the dispatch gate",
        "the send records that it waits for the gate"
    );
    assert_eq!(fx.row(read).result, "line 1");
    assert_eq!(fx.row(send).status, "pending", "still behind the gate");
    drop(held);
    pass.join().unwrap();
    applied(&fx.row(send));
}

/// The index of the pass line resolving request `id` as applied.
fn applied_at(fx: &Fx, id: i64) -> usize {
    fx.env
        .runtime
        .pass_lines()
        .iter()
        .position(|l| l.contains(&format!(" request {id} ")) && l.ends_with(": applied"))
        .unwrap_or_else(|| panic!("request {id} not applied"))
}

#[test]
fn a_read_filed_after_a_deliverable_send_runs_after_it() {
    let fx = fixture();
    fx.session("bravo", "idle");
    let send = fx.add("send", serde_json::json!({"text": "hi"}), false);
    let read = fx.add("read", serde_json::json!({"lines": 5}), false);
    fx.fake.push_read_history(Ok("hi
working"
        .into()));
    fx.run();
    applied(&fx.row(send));
    assert_eq!(
        fx.row(read).result,
        "hi
working"
    );
    assert!(
        applied_at(&fx, send) < applied_at(&fx, read),
        "the read sees the prompt typed: {:?}",
        fx.env.runtime.pass_lines()
    );
}

#[test]
fn requests_after_a_send_held_on_the_gate_keep_their_order() {
    let fx = Arc::new(fixture());
    fx.session("bravo", "idle");
    let send = fx.add("send", serde_json::json!({"text": "one"}), false);
    let read = fx.add("read", serde_json::json!({"lines": 5}), false);
    let later = fx.add("send", serde_json::json!({"text": "two"}), true);
    fx.fake.push_read_history(Ok("before".into()));
    let held = fx.gate.0.try_lock().unwrap();
    let runner = Arc::clone(&fx);
    let pass = std::thread::spawn(move || {
        let jobs =
            tauri::async_runtime::block_on(run_pass(&runner.env, &runner.gate, Trigger::Watcher));
        assert!(jobs.is_empty());
    });
    let mut read_done = false;
    for _ in 0..300 {
        if fx.row(read).status == "applied" {
            read_done = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(read_done, "the read went ahead of the held send");
    assert_eq!(fx.row(send).status, "pending");
    assert_eq!(
        fx.row(later).status,
        "pending",
        "held behind the first send"
    );
    drop(held);
    pass.join().unwrap();
    applied(&fx.row(send));
    // The queued send is behind the first's turn now.
    assert_eq!(fx.row(later).status, "pending");
    assert_eq!(fx.writes(), vec!["line:one".to_string()]);
}

#[test]
fn a_send_that_waits_anyway_neither_blames_nor_waits_for_a_busy_gate() {
    let fx = Arc::new(fixture());
    fx.session("bravo", "busy");
    let id = fx.add("send", serde_json::json!({"text": "later"}), true);
    let held = fx.gate.0.try_lock().unwrap();
    let runner = Arc::clone(&fx);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let jobs =
            tauri::async_runtime::block_on(run_pass(&runner.env, &runner.gate, Trigger::Watcher));
        tx.send(jobs.is_empty()).unwrap();
    });
    let done = rx.recv_timeout(Duration::from_secs(3));
    drop(held);
    assert_eq!(done, Ok(true), "the pass didn't wait for the gate");
    assert_eq!(
        fx.row(id).wait_reason,
        "waiting for @bravo to be idle (herdr: working)"
    );
    assert!(
        !fx.env
            .runtime
            .pass_lines()
            .iter()
            .any(|l| l.contains("dispatch gate")),
        "{:?}",
        fx.env.runtime.pass_lines()
    );
}

#[test]
fn engine_changes_request_passes_only_when_they_could_unblock() {
    let change = |prev: Option<StatusSession>, next: Option<StatusSession>| ViewChange {
        key: SessionKey {
            project: "proj".into(),
            worktree: "bravo".into(),
        },
        prev,
        next,
    };
    let view = |state: &str, agent: &str, settled: bool, stale: bool| {
        let KeyView::View(mut v) = settled_view(1) else {
            unreachable!()
        };
        v.state = state.into();
        v.agent = agent.into();
        v.settled = settled;
        v.stale = stale;
        v
    };
    let idle = view("idle", "present", true, false);
    assert!(could_unblock(&change(None, Some(idle.clone()))), "appeared");
    assert!(
        could_unblock(&change(Some(idle.clone()), None)),
        "went away"
    );
    assert!(could_unblock(&change(
        Some(view("idle", "present", false, false)),
        Some(idle.clone())
    )));
    assert!(could_unblock(&change(
        Some(view("busy", "starting", false, false)),
        Some(view("busy", "present", false, false))
    )));
    assert!(could_unblock(&change(
        Some(view("busy", "present", false, false)),
        Some(view("idle", "present", false, false))
    )));
    assert!(could_unblock(&change(
        Some(view("idle", "present", true, true)),
        Some(idle.clone())
    )));
    assert!(
        !could_unblock(&change(
            Some(view("idle", "present", true, false)),
            Some(view("busy", "present", false, false))
        )),
        "going busy unblocks nothing"
    );
}

#[test]
fn a_target_turning_idle_during_a_request_s_first_pass_asks_for_another() {
    let fx = Arc::new(fixture());
    let pipe = watch(&fx);
    fx.store()
        .drive_bind("alpha", "charlie", None, false)
        .unwrap();
    fx.session("charlie", "idle");
    fx.session("bravo", "busy");
    pipe.clear();
    // A send that needs the dispatch gate, and a queued send to the busy bravo.
    let first = fx
        .store()
        .drive_request_add(
            "send",
            "charlie",
            "alpha",
            &serde_json::json!({"text": "one"}),
            false,
        )
        .unwrap()
        .id;
    let queued = fx.add("send", serde_json::json!({"text": "two"}), true);
    // A dispatch holds the gate: the requests' first pass stalls on it.
    let held = fx.gate.0.try_lock().unwrap();
    let runner = Arc::clone(&fx);
    let pass = std::thread::spawn(move || {
        let jobs =
            tauri::async_runtime::block_on(run_pass(&runner.env, &runner.gate, Trigger::Watcher));
        assert!(jobs.is_empty());
    });
    let mut stalled = false;
    for _ in 0..300 {
        if fx.row(first).wait_reason == "waiting for the dispatch gate" {
            stalled = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(stalled, "the pass is on the gate");
    // bravo turns idle while that pass runs: no DB write, only the engine sees it.
    fx.herdr("bravo", Some("idle"));
    fx.tick();
    assert_eq!(pipe.requested(Trigger::Engine), ["proj"]);
    drop(held);
    pass.join().unwrap();
    fx.advance(&pipe, 30);
    applied(&fx.row(first));
    applied(&fx.row(queued));
}

#[test]
fn a_request_waiting_on_a_busy_target_gets_no_blanket_recheck() {
    let fx = fixture();
    let pipe = watch(&fx);
    fx.session("bravo", "busy");
    fx.add("send", serde_json::json!({"text": "later"}), true);
    fx.run();
    // The target turning idle is an engine change: that trigger, not a timer.
    assert!(pipe.requested(Trigger::Recheck).is_empty());
}

#[test]
fn a_freed_wake_slot_asks_for_a_pass() {
    let fx = fixture();
    let pipe = watch(&fx);
    let claim = fx.env.runtime.claim_worktree("proj", "bravo").unwrap();
    assert!(pipe.requested(Trigger::Recheck).is_empty());
    drop(claim);
    assert_eq!(pipe.requested(Trigger::Recheck), ["proj"]);
}

#[test]
fn a_pass_s_own_writes_leave_its_db_unchanged_for_the_watcher() {
    let fx = fixture();
    fx.session("bravo", "busy");
    let db = fx.env.db_path.clone();
    assert!(!fx.env.runtime.unchanged_since_pass(&db), "no pass yet");
    let id = fx.add("send", serde_json::json!({"text": "later"}), true);
    fx.run();
    assert!(
        !fx.row(id).wait_reason.is_empty(),
        "the pass wrote a reason"
    );
    assert!(
        fx.env.runtime.unchanged_since_pass(&db),
        "only the pass's own writes since it began"
    );
    // A request filed from another connection is news.
    fx.add("send", serde_json::json!({"text": "more"}), true);
    assert!(!fx.env.runtime.unchanged_since_pass(&db));
    // The next pass reuses the kept store and records the new baseline.
    fx.run();
    assert!(fx.env.runtime.unchanged_since_pass(&db));
    // A project that left the registry: its kept store goes.
    fx.env.runtime.retain_stores(|p| p != db);
    assert!(!fx.env.runtime.unchanged_since_pass(&db));
}

#[test]
fn full_housekeeping_runs_at_most_once_a_minute_per_project() {
    let rt = DriveRuntime::default();
    let t0 = Instant::now();
    assert!(rt.housekeeping_due("proj", t0));
    assert!(!rt.housekeeping_due("proj", t0 + Duration::from_secs(59)));
    assert!(rt.housekeeping_due("other-proj", t0), "per project");
    assert!(rt.housekeeping_due("proj", t0 + Duration::from_secs(60)));
}

/// A refusal text without its trailing `(herdr: ...)` detail.
fn without_detail(text: &str) -> &str {
    match text.find(" (herdr: ") {
        Some(i) if text.ends_with(')') => &text[..i],
        _ => text,
    }
}

#[test]
fn offline_and_busy_refusals_say_what_herdr_and_the_hooks_saw() {
    const NOW: i64 = 1_800_000_000_000;
    let row = |at: Option<i64>| HookEvidence {
        state: "idle".into(),
        written_ms: at,
        brokered: false,
        escalated_reason: None,
        session_id: None,
    };
    let two_hours = Some(NOW - 2 * 3600 * 1000);
    assert_eq!(
        state_refusal(
            "offline",
            "bravo",
            &refusal_detail(None, Some(&row(two_hours)), NOW)
        )
        .unwrap(),
        "@bravo is offline; start its session first (herdr: no agent; last hook write 2h ago)"
    );
    assert_eq!(
        state_refusal("offline", "bravo", &refusal_detail(None, None, NOW)).unwrap(),
        "@bravo is offline; start its session first (herdr: no agent; no hook rows)"
    );
    let working = gate_view("busy", Some("working"));
    assert_eq!(
        state_refusal(
            "busy",
            "bravo",
            &refusal_detail(Some(&working), Some(&row(Some(NOW - 7_000))), NOW)
        )
        .unwrap(),
        "@bravo is busy; send only goes to an idle session (use --queue to wait for it) (herdr: working; last hook write 7s ago)"
    );
    // Other states carry no detail; idle is no refusal.
    assert_eq!(
        state_refusal("waiting", "bravo", "(x)").unwrap(),
        "@bravo is waiting on a prompt; answer it first (or use --queue)"
    );
    assert_eq!(state_refusal("idle", "bravo", "(x)"), None);
    assert_eq!(
        without_detail(
            "@bravo is offline; start its session first (herdr: no agent; no hook rows)"
        ),
        "@bravo is offline; start its session first"
    );
}
