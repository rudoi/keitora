//! Tests for the driver verbs. DB tests use a temp-file `LocalStore` (a second
//! connection stands in for the app and the hooks) and a fake clock, so nothing
//! sleeps for real and nothing touches herdr or a real project.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{CommandFactory, Parser};
use keitora_tasks::{Config, LocalStore, NewDrivePrompt, ProjectConfig, SessionReply};
use serde_json::{json, Value};

use super::*;
use crate::Cli;

// ---- fixtures ----

type SleepHook = Box<dyn FnMut(u32)>;

struct FakeClock {
    base: Instant,
    /// Wall ms at `base`.
    wall0: i64,
    t: Cell<Duration>,
    sleeps: Cell<u32>,
    /// Runs after each sleep with the sleep count (another connection's writes).
    hook: RefCell<Option<SleepHook>>,
}

impl FakeClock {
    fn new() -> Self {
        FakeClock {
            base: Instant::now(),
            wall0: keitora_tasks::now_ms(),
            t: Cell::new(Duration::ZERO),
            sleeps: Cell::new(0),
            hook: RefCell::new(None),
        }
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.base + self.t.get()
    }
    fn wall_ms(&self) -> i64 {
        self.wall0 + self.t.get().as_millis() as i64
    }
    fn sleep(&self, d: Duration) {
        self.t.set(self.t.get() + d);
        self.sleeps.set(self.sleeps.get() + 1);
        if let Some(h) = self.hook.borrow_mut().as_mut() {
            h(self.sleeps.get());
        }
    }
}

struct Env {
    dir: tempfile::TempDir,
    path: String,
    local: LocalStore,
    clock: FakeClock,
    registered: Cell<bool>,
    stdin: RefCell<Vec<u8>>,
    worktrees: Vec<String>,
    /// Each worktree's provider, as the registry would report it (unknown when absent).
    clis: RefCell<HashMap<String, keitora_tasks::CliKind>>,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db").to_str().unwrap().to_string();
        let local = LocalStore::open(&path).unwrap();
        Env {
            dir,
            path,
            local,
            clock: FakeClock::new(),
            registered: Cell::new(true),
            stdin: RefCell::new(Vec::new()),
            worktrees: vec!["alpha".into(), "bravo".into(), "charlie".into()],
            clis: RefCell::new(HashMap::new()),
        }
    }

    /// A second connection: the app, a hook, or the human.
    fn other(&self) -> LocalStore {
        LocalStore::open(&self.path).unwrap()
    }

    /// Where this fixture's `status.db` lives.
    fn status_path(&self) -> PathBuf {
        self.dir.path().join("status.db")
    }

    fn wt_dir(&self, w: &str) -> PathBuf {
        self.dir.path().join(w)
    }

    /// Install `f` to run after each sleep, counting sleeps from 1 again.
    fn on_sleep(&self, f: impl FnMut(u32) + 'static) {
        self.clock.sleeps.set(0);
        *self.clock.hook.borrow_mut() = Some(Box::new(f));
    }

    fn run(&self, me: &str, verb: Verb) -> Result<Output, ExitError> {
        let wp = |w: &str| self.wt_dir(w);
        let reg = || {
            if self.registered.get() {
                Ok(())
            } else {
                Err("the project at D:/repos/proj isn't registered with Keitora".to_string())
            }
        };
        let stdin = |cap: usize| {
            let b = self.stdin.borrow();
            Ok(b[..b.len().min(cap)].to_vec())
        };
        let cli_of = |w: &str| self.clis.borrow().get(w).copied();
        let ctx = Ctx {
            local: &self.local,
            me: me.into(),
            worktrees: self.worktrees.clone(),
            worktree_path: &wp,
            worktree_cli: &cli_of,
            registered: &reg,
            clock: &self.clock,
            stdin: &stdin,
            project: PROJECT.into(),
            status_db: Some(self.status_path()),
        };
        run(&ctx, verb)
    }

    /// Run a parsed command line as `me`.
    fn cli(&self, me: &str, args: &[&str]) -> Result<Output, ExitError> {
        self.run(me, verb(args))
    }

    fn bind(&self, driver: &str, driven: &str) {
        self.local.drive_bind(driver, driven, None, false).unwrap();
    }
}

fn verb(args: &[&str]) -> Verb {
    let mut full = vec!["keitora"];
    full.extend_from_slice(args);
    Cli::try_parse_from(full).unwrap().cmd.into_verb().unwrap()
}

fn code(r: Result<Output, ExitError>) -> i32 {
    match r {
        Ok(o) => o.code,
        Err(e) => e.code,
    }
}

fn err(r: Result<Output, ExitError>) -> ExitError {
    r.expect_err("expected an error")
}

fn permission(ls: &LocalStore, driven: &str) -> DrivePrompt {
    ls.drive_prompt_insert(&NewDrivePrompt {
        driven: driven.into(),
        provider: "claude".into(),
        kind: "permission".into(),
        tool_name: "Bash".into(),
        tool_input: r#"{"command":"rm -rf build"}"#.into(),
        summary: "rm -rf build".into(),
        hook_pid: None,
    })
    .unwrap()
}

const QUESTIONS: &str = r#"{"questions":[
  {"question":"Which db?","header":"DB","multiSelect":false,
   "options":[{"label":"pg","description":"x"},{"label":"sqlite"}]},
  {"question":"Seed it?","header":"Seed","multiSelect":true,"options":[{"label":"yes"},{"label":"no"}]}
]}"#;

fn question(ls: &LocalStore, driven: &str, input: &str) -> DrivePrompt {
    ls.drive_prompt_insert(&NewDrivePrompt {
        driven: driven.into(),
        provider: "claude".into(),
        kind: "question".into(),
        tool_name: "AskUserQuestion".into(),
        tool_input: input.into(),
        summary: "Which db?".into(),
        hook_pid: None,
    })
    .unwrap()
}

/// Begin and resolve request `id` from another connection, as the app runner does.
fn app_resolve(ls: &LocalStore, id: i64, status: &str, reason: &str, result: &str) {
    ls.drive_request_begin(id).unwrap();
    ls.drive_request_resolve(id, status, reason, result)
        .unwrap();
}

/// The app applying a `send`: bump `awaiting_turn` and report it.
fn app_apply_send(ls: &LocalStore, id: i64, wt: &str) -> i64 {
    ls.drive_request_begin(id).unwrap();
    let (n, _) = ls
        .drive_begin_delivery(wt, &codex_baseline(Some(4), 11, 0))
        .unwrap()
        .unwrap();
    ls.drive_request_resolve(
        id,
        "applied",
        "",
        &json!({ "awaiting_turn": n }).to_string(),
    )
    .unwrap();
    n
}

fn last_request(ls: &LocalStore) -> DriveRequest {
    ls.drive_requests_list(1).unwrap().remove(0)
}

// ---- parsing ----

#[test]
fn parses_drive_verbs() {
    assert_eq!(
        verb(&["drive", "bravo"]),
        Verb::Drive(DriveArgs {
            worktree: "bravo".into(),
            escalate_after: None,
            force: false
        })
    );
    assert_eq!(
        verb(&["drive", "bravo", "--escalate-after", "60", "--force"]),
        Verb::Drive(DriveArgs {
            worktree: "bravo".into(),
            escalate_after: Some(60),
            force: true
        })
    );
    assert_eq!(
        verb(&["release", "--all"]),
        Verb::Release(ReleaseArgs {
            worktree: None,
            all: true
        })
    );
    assert_eq!(verb(&["drives"]), Verb::Drives);
    assert_eq!(
        verb(&["send", "bravo", "hi"]),
        Verb::Send(SendArgs {
            worktree: "bravo".into(),
            text: "hi".into(),
            queue: false,
            no_wake: false,
            wait: false,
            timeout: 60
        })
    );
    assert_eq!(
        verb(&["send", "bravo", "-", "--queue", "--wait", "--timeout", "5"]),
        Verb::Send(SendArgs {
            worktree: "bravo".into(),
            text: "-".into(),
            queue: true,
            no_wake: false,
            wait: true,
            timeout: 5
        })
    );
    assert_eq!(
        verb(&["wait", "bravo", "charlie"]),
        Verb::Wait(WaitArgs {
            worktrees: vec!["bravo".into(), "charlie".into()],
            timeout: 0,
            stall_after: 120
        })
    );
    assert_eq!(
        verb(&["wait", "bravo", "--timeout", "90", "--stall-after", "30"]),
        Verb::Wait(WaitArgs {
            worktrees: vec!["bravo".into()],
            timeout: 90,
            stall_after: 30
        })
    );
    assert_eq!(
        verb(&["read", "bravo"]),
        Verb::Read(ReadArgs {
            worktree: "bravo".into(),
            lines: 200,
            last: false,
            timeout: 30
        })
    );
    assert_eq!(
        verb(&["read", "bravo", "--last"]),
        Verb::Read(ReadArgs {
            worktree: "bravo".into(),
            lines: 200,
            last: true,
            timeout: 30
        })
    );
    assert_eq!(
        verb(&["prompts"]),
        Verb::Prompts(PromptsArgs { worktree: None })
    );
    assert_eq!(
        verb(&["prompts", "bravo"]),
        Verb::Prompts(PromptsArgs {
            worktree: Some("bravo".into())
        })
    );
    assert_eq!(
        verb(&[
            "answer",
            "bravo",
            "7",
            "answer",
            "--choice",
            "1=pg",
            "--choice",
            "Seed it?=yes"
        ]),
        Verb::Answer(AnswerArgs {
            worktree: "bravo".into(),
            id: 7,
            decision: AnswerDecision::Answer,
            reason: None,
            choices: vec!["1=pg".into(), "Seed it?=yes".into()]
        })
    );
    for (word, d) in [
        ("allow", AnswerDecision::Allow),
        ("deny", AnswerDecision::Deny),
        ("escalate", AnswerDecision::Escalate),
    ] {
        match verb(&["answer", "bravo", "7", word]) {
            Verb::Answer(a) => assert_eq!(a.decision, d),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        verb(&["reset", "bravo"]),
        Verb::Reset(ResetArgs {
            worktree: "bravo".into(),
            to: None,
            wait: false,
            timeout: 60
        })
    );
    assert_eq!(
        verb(&["reset", "bravo", "--to", "codex", "--wait"]),
        Verb::Reset(ResetArgs {
            worktree: "bravo".into(),
            to: Some("codex".into()),
            wait: true,
            timeout: 60
        })
    );
    assert_eq!(
        verb(&["swap", "bravo", "--to", "codex", "--wait"]),
        Verb::Swap(SwapArgs {
            worktree: "bravo".into(),
            to: "codex".into(),
            wait: true,
            timeout: 300
        })
    );
    // `--json` stays global on the new verbs.
    assert!(
        Cli::try_parse_from(["keitora", "drives", "--json"])
            .unwrap()
            .json
    );
    // A bad decision word and a non-numeric id are usage errors.
    assert!(Cli::try_parse_from(["keitora", "answer", "bravo", "7", "maybe"]).is_err());
    assert!(Cli::try_parse_from(["keitora", "answer", "bravo", "x", "allow"]).is_err());
    assert!(Cli::try_parse_from(["keitora", "read", "bravo", "--lines", "5", "--last"]).is_err());
    assert!(Cli::try_parse_from(["keitora", "wait"]).is_err());
}

#[test]
fn release_requires_worktree_or_all_but_not_both() {
    assert!(Cli::try_parse_from(["keitora", "release"]).is_err());
    assert!(Cli::try_parse_from(["keitora", "release", "bravo", "--all"]).is_err());
    assert!(Cli::try_parse_from(["keitora", "release", "bravo"]).is_ok());
    assert!(Cli::try_parse_from(["keitora", "release", "--all"]).is_ok());
}

#[test]
fn send_timeout_requires_wait() {
    assert!(Cli::try_parse_from(["keitora", "send", "bravo", "hi", "--timeout", "5"]).is_err());
    assert!(Cli::try_parse_from(["keitora", "reset", "bravo", "--timeout", "5"]).is_err());
    assert!(
        Cli::try_parse_from(["keitora", "swap", "bravo", "--to", "x", "--timeout", "5"]).is_err()
    );
    assert!(
        Cli::try_parse_from(["keitora", "send", "bravo", "hi", "--wait", "--timeout", "5"]).is_ok()
    );
}

#[test]
fn swap_requires_to() {
    assert!(Cli::try_parse_from(["keitora", "swap", "bravo"]).is_err());
}

#[test]
fn drive_help_carries_the_guidance() {
    let mut cmd = Cli::command();
    let drive = cmd.find_subcommand_mut("drive").unwrap();
    let help = drive.render_long_help().to_string();
    for needle in [
        "keitora send bravo",
        "keitora wait bravo",
        "escalate",
        "always give",
        "DOES NOT ELEVATE",
        "own permission prompts",
        "UPGRADE THE CLI",
        "EXIT CODES",
        "2  timeout",
        "3  this worktree is not the driver",
        "5  the app refused",
    ] {
        assert!(help.contains(needle), "missing {needle:?} in:\n{help}");
    }
}

// ---- identity and project ----

fn git(dir: &std::path::Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn identity_is_the_git_toplevel_name_even_from_a_subdirectory() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("bravo");
    let sub = repo.join("src").join("deep");
    std::fs::create_dir_all(&sub).unwrap();
    if !git(&repo, &["init", "-q"]) {
        eprintln!("git unavailable; skipping");
        return;
    }
    assert_eq!(keitora_tasks::current_worktree_in(&sub), "bravo");
    assert_eq!(keitora_tasks::current_worktree_in(&repo), "bravo");
    let outside = tmp.path().join("loose");
    std::fs::create_dir_all(&outside).unwrap();
    // A temp dir outside any repo has no worktree identity.
    if !git(&outside, &["rev-parse", "--show-toplevel"]) {
        assert_eq!(keitora_tasks::current_worktree_in(&outside), "unknown");
    }
}

#[test]
fn unknown_caller_is_exit_6_for_every_verb() {
    let env = Env::new();
    for args in [
        &["drive", "bravo"][..],
        &["release", "--all"],
        &["drives"],
        &["send", "bravo", "hi"],
        &["wait", "bravo"],
        &["read", "bravo", "--last"],
        &["prompts"],
        &["answer", "bravo", "1", "allow"],
        &["reset", "bravo"],
        &["swap", "bravo", "--to", "codex"],
    ] {
        assert_eq!(code(env.cli("unknown", args)), 6, "{args:?}");
    }
}

#[test]
fn project_worktree_list_from_keitora_toml_validates_names() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("keitora.toml"),
        "[project]\nname = \"proj\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = [\"alpha\", \"bravo\"]\n",
    )
    .unwrap();
    let config = Config::from_project(ProjectConfig::load(tmp.path()).unwrap());
    assert_eq!(config.worktrees, vec!["alpha", "bravo"]);
    let mut env = Env::new();
    env.worktrees = config.worktrees.clone();
    let e = err(env.cli("alpha", &["drive", "charlie"]));
    assert_eq!(e.code, 6);
    assert_eq!(
        e.msg,
        "unknown worktree \"charlie\" — must be one of: alpha, bravo"
    );
    assert_eq!(code(env.cli("alpha", &["drive", "bravo"])), 0);
}

// ---- exit codes and authorization ----

#[test]
fn non_driver_is_refused_with_exit_3() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    for args in [
        &["send", "bravo", "hi"][..],
        &["wait", "bravo"],
        &["read", "bravo"],
        &["read", "bravo", "--last"],
        &["prompts", "bravo"],
        &["answer", "bravo", "1", "allow"],
        &["reset", "bravo"],
        &["swap", "bravo", "--to", "codex"],
        &["release", "bravo"],
    ] {
        let e = err(env.cli("charlie", args));
        assert_eq!(e.code, 3, "{args:?}");
        assert_eq!(e.msg, "@bravo is driven by @alpha, not you", "{args:?}");
    }
    let e = err(env.cli("alpha", &["send", "charlie", "hi"]));
    assert_eq!((e.code, e.msg.as_str()), (3, "@charlie is not driven"));
    // Nothing was enqueued by the refused verbs.
    assert!(env.local.drive_requests_list(10).unwrap().is_empty());
}

#[test]
fn unknown_worktree_is_exit_6() {
    let env = Env::new();
    for args in [
        &["drive", "zulu"][..],
        &["send", "zulu", "hi"],
        &["wait", "zulu"],
        &["release", "zulu"],
    ] {
        let e = err(env.cli("alpha", args));
        assert_eq!(e.code, 6, "{args:?}");
        assert!(e.msg.starts_with("unknown worktree \"zulu\""), "{}", e.msg);
    }
}

#[test]
fn one_level_and_self_rejection_surface_as_exit_4() {
    let env = Env::new();
    let e = err(env.cli("alpha", &["drive", "alpha"]));
    assert_eq!(
        (e.code, e.msg.as_str()),
        (4, "a worktree can't drive itself")
    );
    env.bind("alpha", "bravo");
    let e = err(env.cli("bravo", &["drive", "charlie"]));
    assert_eq!(e.code, 4);
    assert!(e.msg.contains("one level"), "{}", e.msg);
    let e = err(env.cli("charlie", &["drive", "alpha"]));
    assert_eq!(e.code, 4);
    assert!(e.msg.contains("one level"), "{}", e.msg);
    let e = err(env.cli("charlie", &["drive", "bravo"]));
    assert_eq!(e.code, 4);
    assert!(e.msg.contains("--force"), "{}", e.msg);
}

#[test]
fn drive_force_steal_reports_previous_driver_and_escalated_count() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    permission(&env.other(), "bravo");
    let out = env.cli("charlie", &["drive", "bravo", "--force"]).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.value["driver"], "charlie");
    assert_eq!(out.value["forced_from"], "alpha");
    assert_eq!(out.value["escalated_prompts"], 1);
    assert_eq!(
        out.human,
        "  charlie now drives @bravo (escalate after 300s) (took over from @alpha; 1 prompt escalated)"
    );
    // Re-binding as the same driver is not a steal.
    let out = env.cli("charlie", &["drive", "bravo"]).unwrap();
    assert_eq!(out.value["forced_from"], Value::Null);
}

#[test]
fn drive_escalate_after_out_of_range_is_exit_4() {
    for (secs, want) in [("0", 4), ("541", 4), ("1", 0), ("540", 0)] {
        let env = Env::new();
        let r = env.cli("alpha", &["drive", "bravo", "--escalate-after", secs]);
        assert_eq!(code(r.clone()), want, "{secs}");
        if want == 0 {
            assert_eq!(
                r.unwrap().value["escalate_after_s"],
                secs.parse::<i64>().unwrap()
            );
        }
    }
}

#[test]
fn store_conflict_maps_to_exit_4_other_errors_to_1() {
    let e = store_err(keitora_tasks::Error::Conflict("drive is paused".into()));
    assert_eq!(e, ExitError::new(4, "drive is paused"));
    let e = store_err(keitora_tasks::Error::Config("bad".into()));
    assert_eq!(e.code, 1);
    let e = store_err(keitora_tasks::Error::NotFound(3));
    assert_eq!(e.code, 1);
    // The boxed form `main` sees.
    let boxed: Box<dyn std::error::Error> = Box::new(ExitError::new(5, ""));
    assert_eq!(exit_code(boxed.as_ref()), 5);
    let boxed: Box<dyn std::error::Error> = "plain".into();
    assert_eq!(exit_code(boxed.as_ref()), 1);
}

#[test]
fn paused_drive_refuses_send_answer_reset_swap_but_allows_read_wait() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let other = env.other();
    let p = permission(&other, "bravo");
    other.session_reply_record("bravo", "done").unwrap();
    other.drive_restore_awaiting_turn("bravo", Some(1)).unwrap();
    other.drive_set_paused("bravo", true).unwrap();

    for args in [
        &["send", "bravo", "hi"][..],
        &["reset", "bravo"],
        &["swap", "bravo", "--to", "codex"],
    ] {
        let e = err(env.cli("alpha", args));
        assert_eq!((e.code, e.msg.as_str()), (4, "drive is paused"), "{args:?}");
    }
    let id = p.id.to_string();
    let e = err(env.cli("alpha", &["answer", "bravo", &id, "deny"]));
    assert_eq!((e.code, e.msg.as_str()), (4, "drive is paused"));

    // read: both forms work.
    assert_eq!(
        env.cli("alpha", &["read", "bravo", "--last"])
            .unwrap()
            .human,
        "done"
    );
    let app = env.other();
    env.on_sleep(move |_| {
        if let Some(r) = app.drive_requests_pending().unwrap().first() {
            app_resolve(&app, r.id, "applied", "", "tail");
        }
    });
    let out = env.cli("alpha", &["read", "bravo"]).unwrap();
    assert_eq!((out.code, out.human.as_str()), (0, "tail"));

    // wait: the turn still completes.
    let out = env.cli("alpha", &["wait", "bravo"]).unwrap();
    assert_eq!(out.value["event"], "turn_finished");
}

// ---- wait selection (pure) ----

fn input(w: &str) -> WaitInput {
    WaitInput {
        worktree: w.into(),
        awaiting_turn: None,
        reply: None,
        prompt: None,
        state: None,
        idle_secs: None,
        refused_send: None,
        delivery: None,
        unacked: Vec::new(),
        engine: EngineRead::Stale { heartbeat_at: None },
        now_ms: 0,
    }
}

fn reply(w: &str, turn: i64) -> SessionReply {
    SessionReply {
        worktree: w.into(),
        turn,
        last_message: format!("reply {turn}"),
        updated_at: "2026-01-01 10:00:00".into(),
    }
}

fn fake_prompt(id: i64, w: &str) -> DrivePrompt {
    DrivePrompt {
        id,
        driven: w.into(),
        driver: "alpha".into(),
        provider: "claude".into(),
        kind: "permission".into(),
        tool_name: "Bash".into(),
        tool_input: "{}".into(),
        summary: "ls".into(),
        status: "pending".into(),
        decision_json: None,
        decided_by: None,
        reason: String::new(),
        timeout_s: 300,
        hook_pid: None,
        created_at: "2026-01-01 10:00:00".into(),
        decided_at: None,
        age_s: 0,
    }
}

fn kind(e: &Option<WaitEvent>) -> (&'static str, String) {
    match e {
        Some(WaitEvent::Prompt { worktree, .. }) => ("prompt", worktree.clone()),
        Some(WaitEvent::TurnFinished { worktree, .. }) => ("turn_finished", worktree.clone()),
        Some(WaitEvent::HumanTurn { worktree, .. }) => ("human_turn", worktree.clone()),
        Some(WaitEvent::Stalled { worktree, .. }) => ("stalled", worktree.clone()),
        Some(WaitEvent::SendRefused { worktree, .. }) => ("send_refused", worktree.clone()),
        None => ("none", String::new()),
    }
}

#[test]
fn wait_prefers_prompt_over_turn_over_stalled() {
    let mut stalled = input("bravo");
    stalled.awaiting_turn = Some(2);
    stalled.state = Some("busy".into());
    stalled.idle_secs = Some(500);
    let mut finished = input("charlie");
    finished.awaiting_turn = Some(1);
    finished.reply = Some(reply("charlie", 1));
    let mut prompted = input("alpha");
    prompted.prompt = Some(fake_prompt(7, "alpha"));

    let all = [stalled.clone(), finished.clone(), prompted.clone()];
    assert_eq!(
        kind(&select_event(&all, 120, 500)),
        ("prompt", "alpha".into())
    );
    let two = [stalled.clone(), finished.clone()];
    assert_eq!(
        kind(&select_event(&two, 120, 500)),
        ("turn_finished", "charlie".into())
    );
    assert_eq!(
        kind(&select_event(&[stalled], 120, 500)),
        ("stalled", "bravo".into())
    );
}

#[test]
fn wait_turn_finished_requires_turn_ge_awaiting_and_missing_reply_is_zero() {
    let mut i = input("bravo");
    i.awaiting_turn = Some(1);
    assert_eq!(kind(&select_event(&[i.clone()], 120, 0)).0, "none");
    i.reply = Some(reply("bravo", 1));
    match select_event(&[i.clone()], 120, 0) {
        Some(WaitEvent::TurnFinished {
            turn: Finished::Legacy { awaiting, reply },
            ..
        }) => assert_eq!((awaiting, reply.turn), (1, 1)),
        other => panic!("{other:?}"),
    }
    i.reply = Some(reply("bravo", 3));
    assert_eq!(kind(&select_event(&[i], 120, 0)).0, "turn_finished");
}

#[test]
fn wait_stale_idle_before_turn_bump_does_not_finish() {
    // The hook wrote idle for the previous turn; the reply is still turn 3.
    let mut i = input("bravo");
    i.awaiting_turn = Some(4);
    i.reply = Some(reply("bravo", 3));
    i.state = Some("idle".into());
    i.idle_secs = Some(1000);
    assert_eq!(kind(&select_event(&[i.clone()], 120, 100)).0, "none");
    // With no app it ends as a stall, never as that old turn.
    assert_eq!(
        stall_reason(&select_event(&[i], 120, 1000)),
        "no Keitora app is updating status"
    );
}

/// The reason of a `stalled` event (panics on anything else).
fn stall_reason(e: &Option<WaitEvent>) -> String {
    match e {
        Some(WaitEvent::Stalled { reason, .. }) => reason.clone(),
        other => panic!("expected a stall, got {other:?}"),
    }
}

#[test]
fn wait_stalled_only_when_busy_and_awaiting_and_clamped_to_call_start() {
    let mut i = input("bravo");
    i.awaiting_turn = Some(1);
    i.state = Some("busy".into());
    i.idle_secs = Some(300);
    // Idle long enough, but this call only started 30s ago.
    assert_eq!(kind(&select_event(&[i.clone()], 120, 30)).0, "none");
    match select_event(&[i.clone()], 120, 125) {
        Some(WaitEvent::Stalled {
            idle_s,
            state,
            reason,
            ..
        }) => assert_eq!(
            (idle_s, state.as_str(), reason.as_str()),
            (125, "busy", "no hook activity for 125s")
        ),
        other => panic!("{other:?}"),
    }
    // With no app updating status every outstanding send ends in a stall once the
    // call has run --stall-after; the hook rule only names a busy row's silence.
    let no_app = "no Keitora app is updating status";
    // Recent heartbeat.
    i.idle_secs = Some(10);
    assert_eq!(kind(&select_event(&[i.clone()], 120, 119)).0, "none");
    assert_eq!(stall_reason(&select_event(&[i.clone()], 120, 1000)), no_app);
    // Not busy.
    i.idle_secs = Some(1000);
    i.state = Some("waiting".into());
    assert_eq!(stall_reason(&select_event(&[i.clone()], 120, 1000)), no_app);
    // No heartbeat at all.
    i.state = Some("busy".into());
    i.idle_secs = None;
    assert_eq!(stall_reason(&select_event(&[i.clone()], 120, 1000)), no_app);
    // Nothing awaited: never stalled.
    i.awaiting_turn = None;
    assert_eq!(kind(&select_event(&[i], 120, 1000)).0, "none");
}

#[test]
fn wait_multiple_worktrees_picks_first_in_argument_order_within_a_class() {
    let mut b = input("bravo");
    b.awaiting_turn = Some(1);
    b.reply = Some(reply("bravo", 1));
    let mut c = input("charlie");
    c.awaiting_turn = Some(1);
    c.reply = Some(reply("charlie", 1));
    assert_eq!(
        kind(&select_event(&[c.clone(), b.clone()], 120, 0)).1,
        "charlie"
    );
    assert_eq!(kind(&select_event(&[b, c], 120, 0)).1, "bravo");
    let mut p1 = input("bravo");
    p1.prompt = Some(fake_prompt(9, "bravo"));
    let mut p2 = input("charlie");
    p2.prompt = Some(fake_prompt(3, "charlie"));
    assert_eq!(kind(&select_event(&[p1, p2], 120, 0)).1, "bravo");
}

#[test]
fn wait_refused_send_ranks_after_turn_and_before_stalled() {
    let refused = Some(RefusedSend {
        request_id: 4,
        reason: "agent is busy".into(),
    });
    let mut stalled = input("bravo");
    stalled.awaiting_turn = Some(2);
    stalled.state = Some("busy".into());
    stalled.idle_secs = Some(500);
    let mut finished = input("charlie");
    finished.awaiting_turn = Some(1);
    finished.reply = Some(reply("charlie", 1));
    let mut r = input("alpha");
    r.refused_send = refused.clone();
    assert_eq!(
        kind(&select_event(&[r.clone(), finished.clone()], 120, 500)),
        ("turn_finished", "charlie".into())
    );
    assert_eq!(
        kind(&select_event(&[stalled.clone(), r.clone()], 120, 500)),
        ("send_refused", "alpha".into())
    );
    match select_event(&[r], 120, 0) {
        Some(WaitEvent::SendRefused { send, .. }) => assert_eq!(Some(send), refused),
        other => panic!("{other:?}"),
    }
}

// ---- wait (DB) ----

#[test]
fn wait_after_an_unapplied_send_waits_for_the_turn() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    // No --wait: the request is still pending and awaiting_turn is NULL.
    let sent = env.cli("alpha", &["send", "bravo", "hi"]).unwrap();
    let id = sent.value["request_id"].as_i64().unwrap();
    assert_eq!(
        env.local.drive_get("bravo").unwrap().unwrap().awaiting_turn,
        None
    );
    let other = env.other();
    env.on_sleep(move |n| match n {
        2 => {
            app_apply_send(&other, id, "bravo");
        }
        4 => {
            other.session_reply_record("bravo", "done").unwrap();
        }
        _ => {}
    });
    let out = env.cli("alpha", &["wait", "bravo"]).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.value["event"], "turn_finished");
    assert_eq!(out.value["last_message"], "done");
}

#[test]
fn wait_reports_a_queued_send_the_app_refuses_with_exit_5() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let sent = env
        .cli("alpha", &["send", "bravo", "hi", "--queue"])
        .unwrap();
    let id = sent.value["request_id"].as_i64().unwrap();
    let other = env.other();
    env.on_sleep(move |n| {
        if n == 3 {
            other
                .drive_request_resolve(id, "refused", "expired: still busy", "")
                .unwrap();
        }
    });
    let out = env.cli("alpha", &["wait", "bravo"]).unwrap();
    assert_eq!(out.code, 5);
    assert_eq!(
        out.value,
        json!({"event": "send_refused", "worktree": "bravo", "request_id": id,
               "reason": "expired: still busy"})
    );
    assert_eq!(out.human, "  Refused send to @bravo (expired: still busy)");
    // Already refused when the next wait starts: reported, not "nothing to wait for".
    let again = env.cli("alpha", &["wait", "bravo"]).unwrap();
    assert_eq!(
        (again.code, again.value["request_id"].as_i64()),
        (5, Some(id))
    );
    // A later applied send supersedes it.
    let next = env.cli("alpha", &["send", "bravo", "again"]).unwrap();
    let next_id = next.value["request_id"].as_i64().unwrap();
    app_apply_send(&env.other(), next_id, "bravo");
    env.local
        .drive_ack_turn(
            "bravo",
            env.local
                .drive_get("bravo")
                .unwrap()
                .unwrap()
                .awaiting_turn
                .unwrap(),
        )
        .unwrap();
    assert_eq!(err(env.cli("alpha", &["wait", "bravo"])).code, 4);
}

#[test]
fn wait_acks_turn_so_it_is_not_redelivered() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let other = env.other();
    other.drive_restore_awaiting_turn("bravo", Some(1)).unwrap();
    // The turn finishes after the wait started, from the hook's connection.
    env.on_sleep(move |n| {
        if n == 2 {
            other.session_reply_record("bravo", "all done").unwrap();
        }
    });
    let out = env.cli("alpha", &["wait", "bravo"]).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.value["event"], "turn_finished");
    assert_eq!(out.value["turn"], 1);
    assert_eq!(out.value["last_message"], "all done");
    assert_eq!(out.human, "  @bravo finished its turn\nall done");
    assert_eq!(
        env.local.drive_get("bravo").unwrap().unwrap().awaiting_turn,
        None
    );
    let e = err(env.cli("alpha", &["wait", "bravo"]));
    assert_eq!(e.code, 4);
    assert!(e.msg.starts_with("nothing to wait for"), "{}", e.msg);
}

#[test]
fn wait_nothing_outstanding_is_exit_4() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    env.bind("alpha", "charlie");
    let e = err(env.cli("alpha", &["wait", "bravo", "charlie", "bravo"]));
    assert_eq!(
        (e.code, e.msg.as_str()),
        (
            4,
            "nothing to wait for: no send is outstanding on bravo, charlie"
        )
    );
    let e = err(env.cli("alpha", &["wait", "bravo", "--stall-after", "9"]));
    assert_eq!(e.code, 4);
    // A pending prompt alone is something to wait for.
    permission(&env.other(), "charlie");
    let out = env.cli("alpha", &["wait", "bravo", "charlie"]).unwrap();
    assert_eq!(out.value["event"], "prompt");
    assert_eq!(out.value["worktree"], "charlie");
    assert_eq!(out.value["prompt"]["tool_name"], "Bash");
}

#[test]
fn wait_timeout_prints_event_and_exits_2_without_state_change() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    env.local
        .drive_restore_awaiting_turn("bravo", Some(1))
        .unwrap();
    let out = env
        .cli("alpha", &["wait", "bravo", "--timeout", "3"])
        .unwrap();
    assert_eq!(out.code, 2);
    assert_eq!(
        out.value,
        json!({"event": "timeout", "waited_s": 3, "worktrees": ["bravo"]})
    );
    assert_eq!(out.human, "  timed out after 3s");
    let d = env.local.drive_get("bravo").unwrap().unwrap();
    assert_eq!(d.awaiting_turn, Some(1));
    assert!(env.local.drive_requests_list(5).unwrap().is_empty());
}

#[test]
fn wait_released_mid_wait_is_exit_3() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    env.local
        .drive_restore_awaiting_turn("bravo", Some(1))
        .unwrap();
    let other = env.other();
    env.on_sleep(move |n| {
        if n == 3 {
            other.drive_end("bravo").unwrap();
        }
    });
    let e = err(env.cli("alpha", &["wait", "bravo"]));
    assert_eq!(
        (e.code, e.msg.as_str()),
        (3, "@bravo is no longer driven by you")
    );
}

fn counting_wait(env: &Env, timeout: u64) -> (Output, u32) {
    let count = Cell::new(0u32);
    let wp = |w: &str| env.wt_dir(w);
    let reg = || Ok(());
    let stdin = |_: usize| Ok(Vec::new());
    let cli_of = |_: &str| None;
    let ctx = Ctx {
        local: &env.local,
        me: "alpha".into(),
        worktrees: env.worktrees.clone(),
        worktree_path: &wp,
        worktree_cli: &cli_of,
        registered: &reg,
        clock: &env.clock,
        stdin: &stdin,
        project: PROJECT.into(),
        status_db: Some(env.status_path()),
    };
    let args = WaitArgs {
        worktrees: vec!["bravo".into()],
        timeout,
        stall_after: 120,
    };
    let out = wait_with(&ctx, &args, &mut |i, s, e| {
        count.set(count.get() + 1);
        select_event(i, s, e)
    })
    .unwrap();
    (out, count.get())
}

#[test]
fn wait_skips_select_when_data_version_unchanged() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    env.local
        .drive_restore_awaiting_turn("bravo", Some(1))
        .unwrap();
    // No change for 3s: only the initial evaluation runs.
    let (out, n) = counting_wait(&env, 3);
    assert_eq!((out.code, n), (2, 1));
    // 12s with no change: the initial one plus one every 5s.
    let (_, n) = counting_wait(&env, 12);
    assert_eq!(n, 3);
    // Another connection's commit triggers an evaluation at the next poll.
    let other = env.other();
    env.on_sleep(move |s| {
        if s == 2 {
            other.session_upsert("charlie", "busy", None, None).unwrap();
        }
    });
    let (_, n) = counting_wait(&env, 3);
    assert_eq!(n, 2);
}

// ---- answer ----

fn qs() -> Vec<Question> {
    parse_questions(QUESTIONS).unwrap()
}

fn choices(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn answer_choice_parses_first_equals_and_index_or_text_key() {
    let a = parse_choices(&qs(), &choices(&["1=pg=16", "Seed it?=yes, no"])).unwrap();
    assert_eq!(a["Which db?"], "pg=16");
    assert_eq!(a["Seed it?"], "yes, no");
    // A repeated key keeps the last value; index and text name the same question.
    let a = parse_choices(&qs(), &choices(&["1=pg", "Which db?=sqlite", "2=no"])).unwrap();
    assert_eq!(a["Which db?"], "sqlite");
    for bad in ["1", "=pg", "1=", " = "] {
        let e = parse_choices(&qs(), &choices(&[bad, "2=no"])).unwrap_err();
        assert_eq!(e.code, 4, "{bad}");
        assert!(e.msg.starts_with("bad --choice"), "{}", e.msg);
    }
}

#[test]
fn answer_requires_every_question_and_rejects_unknown_key() {
    let e = parse_choices(&qs(), &choices(&["1=pg"])).unwrap_err();
    assert_eq!(
        (e.code, e.msg.as_str()),
        (4, "unanswered question(s): 2 \"Seed it?\"")
    );
    let e = parse_choices(&qs(), &choices(&["Which DB?=pg", "2=no"])).unwrap_err();
    assert_eq!(
        (e.code, e.msg.as_str()),
        (4, "unknown question \"Which DB?\"")
    );
    let e = parse_choices(&qs(), &choices(&["3=x", "1=pg", "2=no"])).unwrap_err();
    assert_eq!(e.msg, "no question 3 (this prompt has 2)");
}

#[test]
fn answer_escalate_without_reason_is_exit_4() {
    let p = fake_prompt(1, "bravo");
    for reason in [None, Some(""), Some("   ")] {
        let e = build_decision(&p, AnswerDecision::Escalate, reason, &[]).unwrap_err();
        assert_eq!(e.code, 4);
    }
    assert_eq!(
        build_decision(&p, AnswerDecision::Escalate, Some("deletes files"), &[]).unwrap(),
        PromptDecision::Escalate {
            reason: "deletes files".into()
        }
    );
    // Through the verb, with a real prompt left pending.
    let env = Env::new();
    env.bind("alpha", "bravo");
    let p = permission(&env.other(), "bravo");
    let id = p.id.to_string();
    let e = err(env.cli("alpha", &["answer", "bravo", &id, "escalate"]));
    assert_eq!(e.code, 4);
    assert_eq!(
        env.local.drive_prompt_get(p.id).unwrap().unwrap().status,
        "pending"
    );
}

#[test]
fn answer_allow_on_question_and_answer_on_permission_are_exit_4() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let other = env.other();
    let q = question(&other, "bravo", QUESTIONS).id.to_string();
    let p = permission(&other, "bravo").id.to_string();
    let e = err(env.cli("alpha", &["answer", "bravo", &q, "allow"]));
    assert_eq!(e.code, 4);
    assert!(e.msg.contains("use answer"), "{}", e.msg);
    let e = err(env.cli(
        "alpha",
        &["answer", "bravo", &p, "answer", "--choice", "1=x"],
    ));
    assert_eq!(e.code, 4);
    // Flags a decision doesn't take.
    let e = err(env.cli("alpha", &["answer", "bravo", &p, "allow", "--reason", "x"]));
    assert_eq!(e.code, 4);
    let e = err(env.cli("alpha", &["answer", "bravo", &p, "deny", "--choice", "1=x"]));
    assert_eq!(e.code, 4);
}

#[test]
fn answer_unparseable_question_input_suggests_escalate() {
    let mut p = fake_prompt(1, "bravo");
    p.kind = "question".into();
    for input in ["not json", "{}", r#"{"questions":"x"}"#] {
        p.tool_input = input.into();
        let e = build_decision(&p, AnswerDecision::Answer, None, &choices(&["1=a"])).unwrap_err();
        assert_eq!(
            (e.code, e.msg.as_str()),
            (4, "cannot read this question; use escalate"),
            "{input}"
        );
    }
}

#[test]
fn answer_records_the_decision_and_missing_prompt_is_exit_6() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    env.bind("alpha", "charlie");
    let other = env.other();
    let q = question(&other, "bravo", QUESTIONS);
    let id = q.id.to_string();
    let out = env
        .cli(
            "alpha",
            &[
                "answer", "bravo", &id, "answer", "--choice", "1=pg", "--choice", "2=yes",
            ],
        )
        .unwrap();
    assert_eq!(
        out.value,
        json!({"id": q.id, "status": "answered", "decided_by": "driver:alpha", "reason": ""})
    );
    assert_eq!(out.human, format!("  Prompt {} answered", q.id));
    let row = env.local.drive_prompt_get(q.id).unwrap().unwrap();
    let decision: Value = serde_json::from_str(row.decision_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        decision,
        json!({"answers": {"Which db?": "pg", "Seed it?": "yes"}})
    );
    // Already decided: a store conflict.
    assert_eq!(code(env.cli("alpha", &["answer", "bravo", &id, "deny"])), 4);
    // Wrong worktree for the id, or no such id.
    let e = err(env.cli("alpha", &["answer", "charlie", &id, "deny"]));
    assert_eq!((e.code, e.msg), (6, format!("no prompt {id} for @charlie")));
    assert_eq!(
        code(env.cli("alpha", &["answer", "bravo", "999", "deny"])),
        6
    );
    // Deny without a reason gets the default.
    let p = permission(&other, "bravo");
    let out = env
        .cli("alpha", &["answer", "bravo", &p.id.to_string(), "deny"])
        .unwrap();
    assert_eq!(out.value["status"], "denied");
    assert_eq!(out.value["reason"], "denied by driver");
}

// ---- requests ----

#[test]
fn send_enqueues_request_and_wait_resolves_applied_with_awaiting_turn() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    env.other()
        .session_reply_record("bravo", "earlier")
        .unwrap();
    // Without --wait: the pending row.
    let out = env
        .cli("alpha", &["send", "bravo", "do it", "--queue"])
        .unwrap();
    let r = last_request(&env.local);
    assert_eq!((r.kind.as_str(), r.queue), ("send", true));
    assert_eq!(
        out.value,
        json!({"request_id": r.id, "status": "pending", "reason": "", "queued": true,
               "awaiting_turn": null, "timed_out": false})
    );
    assert_eq!(
        out.human,
        format!(
            "  Requested send to @bravo (request {}); the running Keitora app applies it",
            r.id
        )
    );
    // With --wait: the app applies it from its own connection.
    let app = env.other();
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            app_apply_send(&app, r.id, &r.worktree);
        }
    });
    let out = env
        .cli("alpha", &["send", "bravo", "next", "--wait"])
        .unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.value["status"], "applied");
    assert_eq!(out.value["awaiting_turn"], 2);
    assert_eq!(out.human, "  Sent to @bravo (turn 2)");
}

#[test]
fn send_wait_timeout_cancels_pending_request_and_exits_2() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let out = env
        .cli(
            "alpha",
            &["send", "bravo", "hi", "--wait", "--timeout", "2"],
        )
        .unwrap();
    assert_eq!(out.code, 2);
    let r = last_request(&env.local);
    assert_eq!(r.status, "refused");
    assert_eq!(r.reason, "no running Keitora app picked it up within 2s");
    assert_eq!(out.value["timed_out"], true);
    assert!(out.value.get("findings").is_none(), "no app: no findings");
    assert_eq!(
        out.human,
        format!(
            "  timed out after 2s: no running Keitora app picked up the send of @bravo (request {})",
            r.id
        )
    );
    // A running request is left alone at the deadline.
    let app = env.other();
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            app.drive_request_begin(r.id).unwrap();
        }
    });
    let out = env
        .cli(
            "alpha",
            &["send", "bravo", "hi", "--wait", "--timeout", "2"],
        )
        .unwrap();
    assert_eq!(out.code, 2);
    assert_eq!(last_request(&env.local).status, "running");
    assert!(out.human.ends_with("is still running"), "{}", out.human);
}

#[test]
fn send_refused_prints_reason_then_exit_5() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let app = env.other();
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            app_resolve(&app, r.id, "refused", "@bravo is busy", "");
        }
    });
    let out = env
        .cli("alpha", &["send", "bravo", "hi", "--wait"])
        .unwrap();
    assert_eq!(out.code, 5);
    assert_eq!(out.value["status"], "refused");
    assert_eq!(out.value["reason"], "@bravo is busy");
    assert_eq!(out.human, "  Refused send to @bravo (@bravo is busy)");
}

#[test]
fn send_dash_reads_stdin_and_enforces_size_limit() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    *env.stdin.borrow_mut() = b"line one\nline two\n".to_vec();
    env.cli("alpha", &["send", "bravo", "-"]).unwrap();
    let r = last_request(&env.local);
    let payload: Value = serde_json::from_str(&r.payload).unwrap();
    assert_eq!(
        payload,
        json!({"text": "line one\nline two\n", "wake": true})
    );

    *env.stdin.borrow_mut() = vec![b'x'; DRIVE_SEND_TEXT_MAX + 1];
    assert_eq!(code(env.cli("alpha", &["send", "bravo", "-"])), 4);
    *env.stdin.borrow_mut() = vec![b'x'; DRIVE_SEND_TEXT_MAX];
    assert_eq!(code(env.cli("alpha", &["send", "bravo", "-"])), 0);
    *env.stdin.borrow_mut() = b"  \n".to_vec();
    assert_eq!(code(env.cli("alpha", &["send", "bravo", "-"])), 4);
    assert_eq!(code(env.cli("alpha", &["send", "bravo", ""])), 4);
    *env.stdin.borrow_mut() = vec![0xff, 0xfe];
    assert_eq!(code(env.cli("alpha", &["send", "bravo", "-"])), 4);
}

#[test]
fn request_verbs_need_a_registered_project() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    env.registered.set(false);
    for args in [
        &["send", "bravo", "hi"][..],
        &["read", "bravo"],
        &["reset", "bravo"],
        &["swap", "bravo", "--to", "codex"],
    ] {
        let e = err(env.cli("alpha", args));
        assert_eq!(e.code, 6, "{args:?}");
        assert!(e.msg.contains("isn't registered"), "{}", e.msg);
    }
    // read --last is DB-only.
    env.other().session_reply_record("bravo", "hello").unwrap();
    assert_eq!(code(env.cli("alpha", &["read", "bravo", "--last"])), 0);
}

#[test]
fn read_last_needs_no_app_and_missing_reply_is_exit_6() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let e = err(env.cli("alpha", &["read", "bravo", "--last"]));
    assert_eq!(
        (e.code, e.msg.as_str()),
        (6, "no finished turn recorded for @bravo")
    );
    env.other()
        .session_reply_record("bravo", "the reply")
        .unwrap();
    let out = env.cli("alpha", &["read", "bravo", "--last"]).unwrap();
    assert_eq!(out.human, "the reply");
    assert_eq!(out.value["source"], "last_message");
    assert_eq!(out.value["turn"], 1);
    assert_eq!(out.value["truncated"], false);
    assert!(env.local.drive_requests_list(5).unwrap().is_empty());
}

#[test]
fn read_pane_enqueues_lines_and_prints_the_result() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    assert_eq!(
        code(env.cli("alpha", &["read", "bravo", "--lines", "0"])),
        4
    );
    assert_eq!(
        code(env.cli("alpha", &["read", "bravo", "--lines", "2001"])),
        4
    );
    let app = env.other();
    let big = "y".repeat(DRIVE_READ_RESULT_MAX);
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            app_resolve(&app, r.id, "applied", "", &big);
        }
    });
    let out = env
        .cli("alpha", &["read", "bravo", "--lines", "50"])
        .unwrap();
    let r = last_request(&env.local);
    assert_eq!(
        serde_json::from_str::<Value>(&r.payload).unwrap(),
        json!({"lines": 50})
    );
    assert_eq!(out.value["source"], "pane");
    assert_eq!(out.value["lines"], 50);
    assert_eq!(out.value["truncated"], true);
    // Refused (pane not open) is exit 5; no app at all is a timeout.
    let app = env.other();
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            app_resolve(&app, r.id, "refused", "project not open in Keitora", "");
        }
    });
    let out = env.cli("alpha", &["read", "bravo"]).unwrap();
    assert_eq!(out.code, 5);
    assert_eq!(
        out.human,
        "  Refused read of @bravo (project not open in Keitora)"
    );
    *env.clock.hook.borrow_mut() = None;
    let out = env
        .cli("alpha", &["read", "bravo", "--timeout", "1"])
        .unwrap();
    assert_eq!(out.code, 2);
    assert_eq!(last_request(&env.local).status, "refused");
}

#[test]
fn swap_pending_confirm_is_polled_through_to_applied_or_refused() {
    for (outcome, want) in [("applied", 0), ("refused", 5)] {
        let env = Env::new();
        env.bind("alpha", "bravo");
        let app = env.other();
        let seen_confirm = std::rc::Rc::new(Cell::new(false));
        let seen = seen_confirm.clone();
        env.on_sleep(move |n| {
            let r = app.drive_requests_list(1).unwrap().remove(0);
            match n {
                1 => {
                    app.drive_request_begin(r.id).unwrap();
                    app.drive_request_await_confirm(r.id).unwrap();
                }
                // The human takes a while to answer the confirm.
                2..=20 => seen.set(r.status == "pending_confirm"),
                21 if outcome == "applied" => {
                    app.drive_request_resume(r.id).unwrap();
                    app.drive_request_resolve(r.id, "applied", "", r#"{"provider":"codex"}"#)
                        .unwrap();
                }
                21 => app
                    .drive_request_resolve(r.id, "refused", "swap cancelled", "")
                    .unwrap(),
                _ => {}
            }
        });
        let out = env
            .cli("alpha", &["swap", "bravo", "--to", "codex", "--wait"])
            .unwrap();
        assert!(seen_confirm.get());
        assert_eq!(out.code, want, "{outcome}");
        assert_eq!(out.value["status"], outcome);
        assert_eq!(out.value["provider"], "codex");
        if outcome == "applied" {
            assert_eq!(out.human, "  Swapped @bravo to codex");
        } else {
            assert_eq!(out.human, "  Refused swap of @bravo (swap cancelled)");
        }
    }
    // Timeout while the confirm is up: exit 2 and the request is left.
    let env = Env::new();
    env.bind("alpha", "bravo");
    let app = env.other();
    env.on_sleep(move |n| {
        if n == 1 {
            let r = app.drive_requests_list(1).unwrap().remove(0);
            app.drive_request_begin(r.id).unwrap();
            app.drive_request_await_confirm(r.id).unwrap();
        }
    });
    let out = env
        .cli(
            "alpha",
            &["swap", "bravo", "--to", "codex", "--wait", "--timeout", "2"],
        )
        .unwrap();
    assert_eq!(out.code, 2);
    assert_eq!(last_request(&env.local).status, "pending_confirm");
    assert!(
        out.human
            .starts_with("  @bravo is waiting for the human to confirm the swap\n"),
        "{}",
        out.human
    );
}

#[test]
fn reset_provider_validation_accepts_unknown_but_wellformed_names() {
    for p in ["claude", "codex", "agy", "my-cli_2", "a"] {
        assert!(valid_provider(p), "{p}");
    }
    for p in ["", "Codex", "2cli", "-x", "has space", &"a".repeat(33)] {
        assert!(!valid_provider(p), "{p}");
    }
    assert!(valid_provider(&"a".repeat(32)));
    let env = Env::new();
    env.bind("alpha", "bravo");
    let out = env
        .cli("alpha", &["reset", "bravo", "--to", "agy"])
        .unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.value["provider"], "agy");
    assert_eq!(
        code(env.cli("alpha", &["reset", "bravo", "--to", "Bad"])),
        4
    );
    assert_eq!(code(env.cli("alpha", &["swap", "bravo", "--to", "x y"])), 4);
}

#[test]
fn request_payloads_match_the_app_contract() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let payload = |env: &Env| -> (String, Value, bool) {
        let r = last_request(&env.local);
        (r.kind, serde_json::from_str(&r.payload).unwrap(), r.queue)
    };
    env.cli("alpha", &["send", "bravo", "hello"]).unwrap();
    assert_eq!(
        payload(&env),
        ("send".into(), json!({"text": "hello", "wake": true}), false)
    );
    env.cli("alpha", &["reset", "bravo"]).unwrap();
    assert_eq!(payload(&env), ("reset".into(), json!({}), false));
    env.cli("alpha", &["reset", "bravo", "--to", "codex"])
        .unwrap();
    assert_eq!(
        payload(&env),
        ("reset".into(), json!({"to": "codex"}), false)
    );
    env.cli("alpha", &["swap", "bravo", "--to", "claude"])
        .unwrap();
    assert_eq!(
        payload(&env),
        ("swap".into(), json!({"to": "claude"}), false)
    );
    // read polls; let it time out immediately and inspect the row.
    env.cli("alpha", &["read", "bravo", "--timeout", "0"])
        .unwrap();
    assert_eq!(payload(&env), ("read".into(), json!({"lines": 200}), false));
    for r in env.local.drive_requests_list(10).unwrap() {
        assert_eq!(
            (r.worktree.as_str(), r.requested_by.as_str()),
            ("bravo", "alpha")
        );
    }
}

// ---- output ----

#[test]
fn drives_json_joins_state_prompts_and_turn() {
    let env = Env::new();
    assert_eq!(env.cli("alpha", &["drives"]).unwrap().human, "  No drives");
    env.bind("alpha", "bravo");
    env.bind("alpha", "charlie");
    let other = env.other();
    other.session_upsert("bravo", "busy", None, None).unwrap();
    other.session_reply_record("bravo", "x").unwrap();
    other.session_reply_record("bravo", "y").unwrap();
    other.drive_restore_awaiting_turn("bravo", Some(3)).unwrap();
    permission(&other, "bravo");
    other.drive_set_paused("charlie", true).unwrap();
    // Open to any caller, not only drivers.
    let out = env.cli("bravo", &["drives"]).unwrap();
    let rows = out.value["drives"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let b = &rows[0];
    assert_eq!(b["driven"], "bravo");
    assert_eq!(b["state"], "busy");
    assert_eq!(b["pending_prompts"], 1);
    assert_eq!(b["turn"], 2);
    assert_eq!(b["awaiting_turn"], 3);
    assert_eq!(b["paused"], false);
    assert_eq!(b["escalate_after_s"], 300);
    let c = &rows[1];
    assert_eq!(
        (c["state"].clone(), c["turn"].clone()),
        (Value::Null, Value::Null)
    );
    assert_eq!(c["paused"], true);
    assert_eq!(
        out.human,
        "  alpha -> bravo  busy  prompts:1  turn:2 (awaiting 3)\n  alpha -> charlie  -  prompts:0  turn:0  [paused]"
    );
}

#[test]
fn prompt_object_parses_question_options() {
    let mut p = fake_prompt(7, "bravo");
    p.kind = "question".into();
    p.tool_input = QUESTIONS.into();
    p.age_s = 42;
    let v = prompt_object(&p);
    assert_eq!(v["age_s"], 42);
    assert_eq!(
        v["questions"],
        json!([
            {"index": 1, "question": "Which db?", "header": "DB", "multi_select": false,
             "options": ["pg", "sqlite"]},
            {"index": 2, "question": "Seed it?", "header": "Seed", "multi_select": true,
             "options": ["yes", "no"]}
        ])
    );
    p.tool_input = "{not json".into();
    assert_eq!(prompt_object(&p)["questions"], Value::Null);
    // A permission prompt never carries questions.
    let mut perm = fake_prompt(8, "bravo");
    perm.tool_input = QUESTIONS.into();
    let v = prompt_object(&perm);
    assert_eq!(v["questions"], Value::Null);
    for key in [
        "id",
        "driven",
        "kind",
        "tool_name",
        "summary",
        "tool_input",
        "age_s",
        "timeout_s",
        "created_at",
        "provider",
        "questions",
    ] {
        assert!(v.get(key).is_some(), "{key}");
    }
}

#[test]
fn human_lines_for_each_verb() {
    let env = Env::new();
    assert_eq!(
        env.cli("alpha", &["drive", "bravo"]).unwrap().human,
        "  alpha now drives @bravo (escalate after 300s)"
    );
    assert_eq!(
        env.cli("alpha", &["prompts"]).unwrap().human,
        "  No pending prompts"
    );
    let p = permission(&env.other(), "bravo");
    let line = env.cli("alpha", &["prompts", "bravo"]).unwrap().human;
    assert!(
        line.starts_with(&format!(
            "  #{} @bravo permission Bash: rm -rf build  (",
            p.id
        )),
        "{line}"
    );
    assert!(line.ends_with("s ago)"), "{line}");
    let out = env.cli("alpha", &["prompts"]).unwrap();
    assert_eq!(out.value["prompts"][0]["id"], p.id);

    let w = env.cli("alpha", &["wait", "bravo"]).unwrap().human;
    assert_eq!(
        w,
        format!(
            "  @bravo needs a decision: prompt {} (permission, Bash: rm -rf build)",
            p.id
        )
    );
    let stalled = event_output(&WaitEvent::Stalled {
        worktree: "bravo".into(),
        reason: "no hook activity for 125s".into(),
        state: "busy".into(),
        idle_s: 125,
        delivery_id: Some(7),
        turn_id: None,
    });
    assert_eq!(
        stalled.human,
        "  @bravo looks stalled: no hook activity for 125s; try: keitora read bravo"
    );
    assert_eq!(
        stalled.value,
        json!({"event": "stalled", "worktree": "bravo", "reason": "no hook activity for 125s",
               "state": "busy", "idle_s": 125, "delivery_id": 7})
    );

    let out = env.cli("alpha", &["reset", "bravo"]).unwrap();
    let id = last_request(&env.local).id;
    assert_eq!(
        out.human,
        format!("  Requested reset of @bravo (request {id})")
    );
    assert_eq!(out.value["provider"], Value::Null);
    let app = env.other();
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            app_resolve(&app, r.id, "applied", "", r#"{"provider":"codex"}"#);
        }
    });
    let out = env.cli("alpha", &["reset", "bravo", "--wait"]).unwrap();
    assert_eq!(out.human, "  Reset @bravo on codex");
    assert_eq!(out.value["provider"], "codex");
    *env.clock.hook.borrow_mut() = None;

    let out = env.cli("alpha", &["release", "bravo"]).unwrap();
    assert_eq!(
        out.human,
        "  Released @bravo (1 pending prompt escalated to you)"
    );
    assert_eq!(
        out.value,
        json!({"released": [{"driven": "bravo", "escalated_prompts": 1}]})
    );
    let out = env.cli("alpha", &["release", "--all"]).unwrap();
    assert_eq!((out.code, out.human.as_str()), (0, "  Nothing to release"));
    assert_eq!(out.value, json!({"released": []}));
    env.bind("alpha", "bravo");
    env.bind("alpha", "charlie");
    let out = env.cli("alpha", &["release", "--all"]).unwrap();
    assert_eq!(out.human, "  Released @bravo\n  Released @charlie");
    // JSON rendering is pretty-printed.
    assert!(out.render(true).contains("\n  \"released\""));
}

#[test]
fn gate_hook_check_warns_only_when_files_exist_without_session_gate() {
    let env = Env::new();
    // Neither file: no verdict, no warning.
    let out = env.cli("alpha", &["drive", "bravo"]).unwrap();
    assert_eq!(out.value["hook_gate_installed"], Value::Null);
    assert_eq!(out.value["warnings"], json!([]));

    let claude = env.wt_dir("bravo").join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join("settings.local.json"),
        r#"{"hooks":{"Stop":[{"command":"keitora session idle"}]}}"#,
    )
    .unwrap();
    let out = env.cli("alpha", &["drive", "bravo"]).unwrap();
    assert_eq!(out.value["hook_gate_installed"], false);
    let warnings = out.value["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0]
        .as_str()
        .unwrap()
        .starts_with("@bravo's hook file has no gate entry"));
    assert!(out.human.contains("\n  warning: @bravo's hook file"));

    // Either file carrying the gate is enough.
    let codex = env.wt_dir("bravo").join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::write(
        codex.join("hooks.json"),
        r#"{"command":"keitora session gate --provider codex"}"#,
    )
    .unwrap();
    let out = env.cli("alpha", &["drive", "bravo"]).unwrap();
    assert_eq!(out.value["hook_gate_installed"], true);
    assert_eq!(out.value["warnings"], json!([]));
}

#[test]
fn driving_an_antigravity_worktree_warns_that_it_is_unbrokered() {
    let env = Env::new();
    // A gated Claude hook file left over from before the switch doesn't count.
    let claude = env.wt_dir("bravo").join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join("settings.local.json"),
        r#"{"command":"keitora session gate --provider claude"}"#,
    )
    .unwrap();
    env.clis
        .borrow_mut()
        .insert("bravo".into(), keitora_tasks::CliKind::Antigravity);
    let out = env.cli("alpha", &["drive", "bravo"]).unwrap();
    assert_eq!(out.value["hook_gate_installed"], false);
    let warnings = out.value["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1);
    let w = warnings[0].as_str().unwrap();
    assert!(
        w.starts_with("@bravo runs Antigravity, which has no permission hook to broker"),
        "{w}"
    );
    assert!(out.human.contains(
        "
  warning: @bravo runs Antigravity"
    ));

    // The same worktree on Claude is gated and warns nothing.
    env.clis
        .borrow_mut()
        .insert("bravo".into(), keitora_tasks::CliKind::Claude);
    let out = env.cli("alpha", &["drive", "bravo"]).unwrap();
    assert_eq!(out.value["hook_gate_installed"], true);
    assert_eq!(out.value["warnings"], json!([]));
}

#[test]
fn question_parser_is_lenient_for_drivers_and_strict_for_the_gate() {
    let loose = json!({"questions": [
        {"question": "Pick?", "multi_select": true, "options": ["a", {"label": "b"}]},
        {"question": "Free text?"}
    ]});
    let qs = parse_question_list(&loose, false).unwrap();
    assert_eq!(qs.len(), 2);
    assert!(qs[0].multi_select);
    assert_eq!(qs[0].options, ["a", "b"]);
    assert!(qs[1].options.is_empty());
    assert!(parse_question_list(&loose, true).is_err());

    let empty = json!({"questions": []});
    assert_eq!(parse_question_list(&empty, false).unwrap(), vec![]);
    assert!(parse_question_list(&empty, true).is_err());

    let strict_ok = json!({"questions": [{"question": "Q?", "options": [{"label": "x"}]}]});
    assert_eq!(parse_question_list(&strict_ok, true).unwrap().len(), 1);
    assert!(parse_question_list(&json!({}), false).is_err());
}

// ---- wake ----

#[test]
fn parses_wake_and_send_no_wake() {
    assert_eq!(
        verb(&["wake", "bravo"]),
        Verb::Wake(WakeArgs {
            worktree: "bravo".into(),
            to: None,
            fresh: false,
            wait: false,
            timeout: 60
        })
    );
    assert_eq!(
        verb(&[
            "wake",
            "bravo",
            "--to",
            "codex",
            "--fresh",
            "--wait",
            "--timeout",
            "90"
        ]),
        Verb::Wake(WakeArgs {
            worktree: "bravo".into(),
            to: Some("codex".into()),
            fresh: true,
            wait: true,
            timeout: 90
        })
    );
    assert!(Cli::try_parse_from(["keitora", "wake", "bravo", "--timeout", "5"]).is_err());
    assert!(Cli::try_parse_from(["keitora", "wake"]).is_err());
    match verb(&["send", "bravo", "hi", "--no-wake"]) {
        Verb::Send(a) => assert!(a.no_wake),
        other => panic!("{other:?}"),
    }
}

#[test]
fn wake_enqueues_a_wake_request() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let out = env.cli("alpha", &["wake", "bravo"]).unwrap();
    let r = last_request(&env.local);
    assert_eq!(r.kind, "wake");
    assert_eq!(
        serde_json::from_str::<Value>(&r.payload).unwrap(),
        json!({"fresh": false})
    );
    assert_eq!(out.code, 0);
    // Whether it was already awake is unknown until the app applies it.
    assert_eq!(
        out.value,
        json!({"request_id": r.id, "status": "pending", "reason": "", "provider": null,
               "already_awake": null, "timed_out": false})
    );
    assert_eq!(
        out.human,
        format!("  Requested wake of @bravo (request {})", r.id)
    );

    env.cli("alpha", &["wake", "bravo", "--fresh", "--to", "codex"])
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&last_request(&env.local).payload).unwrap(),
        json!({"fresh": true, "to": "codex"})
    );
    assert_eq!(code(env.cli("alpha", &["wake", "bravo", "--to", "Bad"])), 4);
}

#[test]
fn wake_of_a_running_agent_is_an_already_awake_no_op() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let app = env.other();
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            let result = json!({"provider": "claude", "already_awake": true}).to_string();
            app_resolve(&app, r.id, "applied", "", &result);
        }
    });
    let out = env.cli("alpha", &["wake", "bravo", "--wait"]).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.value["status"], "applied");
    assert_eq!(out.value["already_awake"], true);
    assert_eq!(out.value["provider"], "claude");
    assert_eq!(out.human, "  @bravo is already awake");
}

#[test]
fn wake_of_a_stopped_agent_reports_the_provider_or_the_refusal() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let app = env.other();
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            let result = json!({"provider": "codex", "already_awake": false}).to_string();
            app_resolve(&app, r.id, "applied", "", &result);
        }
    });
    let out = env
        .cli("alpha", &["wake", "bravo", "--to", "codex", "--wait"])
        .unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.value["already_awake"], false);
    assert_eq!(out.human, "  Woke @bravo on codex");

    let app = env.other();
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            app_resolve(&app, r.id, "refused", "project not open in Keitora", "");
        }
    });
    let out = env.cli("alpha", &["wake", "bravo", "--wait"]).unwrap();
    assert_eq!(out.code, 5);
    assert_eq!(
        out.human,
        "  Refused wake of @bravo (project not open in Keitora)"
    );
    assert_eq!(out.value["already_awake"], Value::Null, "refused: unknown");
    assert_eq!(out.value["status"], "refused");
}

#[test]
fn wake_already_awake_is_null_unless_applied() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    // Still running at the deadline: not known yet.
    let app = env.other();
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            app.drive_request_begin(r.id).unwrap();
        }
    });
    let out = env
        .cli("alpha", &["wake", "bravo", "--wait", "--timeout", "2"])
        .unwrap();
    assert_eq!(out.code, 2);
    assert_eq!(out.value["timed_out"], true);
    assert_eq!(out.value["status"], "running");
    assert_eq!(out.value["already_awake"], Value::Null);

    // A refusal that carries a stray result still says nothing about being awake.
    let app = env.other();
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            let result = json!({"already_awake": true}).to_string();
            app_resolve(&app, r.id, "refused", "spawn failed", &result);
        }
    });
    let out = env.cli("alpha", &["wake", "bravo", "--wait"]).unwrap();
    assert_eq!(out.code, 5);
    assert_eq!(out.value["already_awake"], Value::Null);
    assert_eq!(out.human, "  Refused wake of @bravo (spawn failed)");

    // Other kinds never carry the field.
    let out = env.cli("alpha", &["reset", "bravo"]).unwrap();
    assert!(out.value.get("already_awake").is_none());
}

#[test]
fn wake_needs_the_driver_and_a_registered_project() {
    let env = Env::new();
    assert_eq!(code(env.cli("alpha", &["wake", "bravo"])), 3);
    env.bind("charlie", "bravo");
    let e = err(env.cli("alpha", &["wake", "bravo"]));
    assert_eq!(e.code, 3);
    assert_eq!(e.msg, "@bravo is driven by @charlie, not you");
    assert!(env.local.drive_requests_list(10).unwrap().is_empty());

    let env = Env::new();
    env.bind("alpha", "bravo");
    env.registered.set(false);
    assert_eq!(code(env.cli("alpha", &["wake", "bravo"])), 6);
    assert_eq!(code(env.cli("alpha", &["wake", "delta"])), 6);
}

#[test]
fn send_auto_wakes_unless_no_wake() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let wake_flag = |env: &Env| -> Value {
        let r = last_request(&env.local);
        serde_json::from_str::<Value>(&r.payload).unwrap()["wake"].clone()
    };
    env.cli("alpha", &["send", "bravo", "hi"]).unwrap();
    assert_eq!(wake_flag(&env), json!(true));
    env.cli("alpha", &["send", "bravo", "hi", "--no-wake"])
        .unwrap();
    assert_eq!(wake_flag(&env), json!(false));

    // An opted-out send to a stopped agent comes back refused (exit 5).
    let app = env.other();
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            app_resolve(
                &app,
                r.id,
                "refused",
                "@bravo is offline; start its session first",
                "",
            );
        }
    });
    let out = env
        .cli("alpha", &["send", "bravo", "hi", "--no-wake", "--wait"])
        .unwrap();
    assert_eq!(out.code, 5);
}

#[test]
fn drive_help_explains_wake() {
    let mut cmd = Cli::command();
    let help = cmd
        .find_subcommand_mut("drive")
        .unwrap()
        .render_long_help()
        .to_string();
    for needle in ["keitora wake bravo", "--no-wake", "already awake"] {
        assert!(help.contains(needle), "missing {needle:?} in:\n{help}");
    }
}

// ---- status.db fixtures ----

/// The project name the status rows are keyed by.
const PROJECT: &str = "proj";

/// A writer row whose heartbeat is `at_ms`, herdr up, with a snapshot.
fn writer_at(at_ms: i64) -> keitora_tasks::WriterRow {
    keitora_tasks::WriterRow {
        instance_id: "app-1".into(),
        pid: 4242,
        version: "test".into(),
        started_at: at_ms - 60_000,
        heartbeat_at: at_ms,
        last_snapshot_at: Some(at_ms),
        herdr_link: "up".into(),
        herdr_link_since: at_ms - 60_000,
    }
}

/// The engine's published view of `wt`.
fn view(
    wt: &str,
    herdr_status: &str,
    scs: i64,
    cs: Option<i64>,
    settled: bool,
    at_ms: i64,
) -> keitora_tasks::StatusSession {
    let state = match herdr_status {
        "idle" | "done" if settled => "idle",
        "blocked" => "waiting",
        _ => "busy",
    };
    keitora_tasks::StatusSession {
        project: PROJECT.into(),
        worktree: wt.into(),
        state: state.into(),
        agent: "present".into(),
        agent_kind: Some("codex".into()),
        herdr_status: Some(herdr_status.into()),
        settled,
        stale: false,
        interactive_ready: Some(true),
        pane_id: Some("p-1".into()),
        terminal_id: Some("t-1".into()),
        agent_session: Some("s-1".into()),
        state_change_seq: Some(scs),
        completion_seq: cs,
        brokered: false,
        escalated_reason: None,
        last_change_at: at_ms,
    }
}

fn codex_baseline(cs: Option<u64>, scs: u64, at_ms: i64) -> keitora_tasks::Baseline {
    keitora_tasks::Baseline {
        agent: Some("codex".into()),
        pane_id: "p-1".into(),
        terminal_id: "t-1".into(),
        agent_session: Some("s-1".into()),
        state_change_seq: scs,
        completion_seq: cs,
        status: "idle".into(),
        delivered_at_ms: at_ms,
    }
}

// ---- reproductions ----

#[test]
fn codex_answer_without_stop_returns_turn_finished() {
    let env = Env::new();
    let app = env.other();
    let status = env.status_path();
    let t0 = env.clock.wall_ms();
    // 1. bravo drives alpha; the app delivered a prompt over a baseline with cs 4.
    env.bind("bravo", "alpha");
    let (_, delivery) = app
        .drive_begin_delivery("alpha", &codex_baseline(Some(4), 11, t0))
        .unwrap()
        .unwrap();
    // 2-3. Codex works; the engine publishes it.
    keitora_tasks::status_store::write_status_for_test(
        &status,
        writer_at(t0),
        &[view("alpha", "working", 12, Some(4), false, t0)],
    )
    .unwrap();
    // 4-5. Codex goes idle with cs 5, seen at +0 s and again at +1 s (settled).
    keitora_tasks::status_store::write_status_for_test(
        &status,
        writer_at(t0),
        &[view("alpha", "idle", 13, Some(5), false, t0)],
    )
    .unwrap();
    keitora_tasks::status_store::write_status_for_test(
        &status,
        writer_at(t0 + 1_000),
        &[view("alpha", "idle", 13, Some(5), true, t0)],
    )
    .unwrap();
    // The engine records the herdr-detected completion (no reply text).
    app.drive_turn_insert(&keitora_tasks::NewDriveTurn {
        driven: "alpha",
        kind: "driver",
        delivery_id: Some(delivery),
        outcome: "finished",
        signal: "herdr",
        clause: "completion_seq",
        completion_seq: Some(5),
        state_change_seq: Some(13),
        reply: "",
        reply_source: "none",
        caveat: None,
    })
    .unwrap()
    .unwrap();
    // 6. No Stop row is ever written.
    assert!(app.session_reply_get("alpha").unwrap().is_none());
    // 7. wait, for up to 10 fake minutes.
    let out = env
        .cli("bravo", &["wait", "alpha", "--timeout", "600"])
        .unwrap();
    assert_eq!(out.value["event"], "turn_finished", "{}", out.value);
    assert_eq!(out.value["reply_source"], "none");
    assert!(
        env.clock.t.get() <= Duration::from_secs(1),
        "within one hot tick, took {:?}",
        env.clock.t.get()
    );
    let last = env.cli("bravo", &["read", "alpha", "--last"]);
    assert_eq!(code(last), 0, "read --last finds the turn");
}

// ---- engine-backed wait ----

/// A live app publishing `views` (its heartbeat stays fresh for an hour of fake time).
fn app_publishes(env: &Env, views: &[keitora_tasks::StatusSession]) {
    let live = env.clock.wall_ms() + 3_600_000;
    keitora_tasks::status_store::write_status_for_test(&env.status_path(), writer_at(live), views)
        .unwrap();
}

/// The app delivering a prompt to `wt` over a codex baseline at the fake clock's now.
fn app_delivers(env: &Env, wt: &str) -> (i64, i64) {
    env.other()
        .drive_begin_delivery(wt, &codex_baseline(Some(4), 11, env.clock.wall_ms()))
        .unwrap()
        .unwrap()
}

/// The app recording a turn of `wt`.
fn app_records(
    env: &Env,
    wt: &str,
    kind: &str,
    delivery_id: Option<i64>,
    outcome: &str,
    reply: &str,
) -> i64 {
    let (clause, source) = match outcome {
        "restarted" => ("occupant_changed", "none"),
        _ if reply.is_empty() => ("completion_seq", "none"),
        _ => ("completion_seq", "hook"),
    };
    env.other()
        .drive_turn_insert(&keitora_tasks::NewDriveTurn {
            driven: wt,
            kind,
            delivery_id,
            outcome,
            signal: "herdr",
            clause,
            completion_seq: Some(5),
            state_change_seq: Some(13),
            reply,
            reply_source: source,
            caveat: None,
        })
        .unwrap()
        .unwrap()
}

fn turn_row(id: i64, kind: &str, outcome: &str) -> DriveTurn {
    DriveTurn {
        id,
        driven: "bravo".into(),
        kind: kind.into(),
        delivery_id: (kind == "driver").then_some(7),
        outcome: outcome.into(),
        signal: "herdr".into(),
        clause: "completion_seq".into(),
        completed_at: "2026-01-01 10:00:00.123".into(),
        reply: "the reply".into(),
        reply_source: "hook".into(),
        caveat: None,
        acked_at: None,
    }
}

const T: i64 = 1_800_000_000_000;

/// `bravo` awaiting delivery 7 (baseline cs 4, scs 11, at `T`), with `engine`.
fn delivered(engine: EngineRead, now_ms: i64) -> WaitInput {
    let mut i = input("bravo");
    i.awaiting_turn = Some(1);
    i.delivery = Some(Delivery {
        driven: "bravo".into(),
        driver: "alpha".into(),
        awaiting_turn: Some(1),
        delivery_id: Some(7),
        baseline: Some(codex_baseline(Some(4), 11, T)),
        delivered_at: Some("2026-01-01 10:00:00.000".into()),
    });
    i.engine = engine;
    i.now_ms = now_ms;
    i
}

fn fresh(view: keitora_tasks::StatusSession) -> EngineRead {
    EngineRead::Fresh {
        view: Some(view),
        writer: writer_at(T),
    }
}

#[test]
fn engine_read_classifies_the_store() {
    let env = Env::new();
    let now = env.clock.wall_ms();
    let open = || keitora_tasks::StatusReader::open(&env.status_path()).unwrap();
    assert_eq!(
        EngineRead::of(None, PROJECT, "bravo", now),
        EngineRead::Stale { heartbeat_at: None }
    );
    let idle = view("bravo", "idle", 11, Some(4), true, now);
    keitora_tasks::status_store::write_status_for_test(
        &env.status_path(),
        writer_at(now),
        std::slice::from_ref(&idle),
    )
    .unwrap();
    let r = open();
    assert_eq!(
        EngineRead::of(r.as_ref(), PROJECT, "bravo", now),
        EngineRead::Fresh {
            view: Some(idle.clone()),
            writer: writer_at(now)
        }
    );
    assert!(matches!(
        EngineRead::of(r.as_ref(), PROJECT, "charlie", now),
        EngineRead::Fresh { view: None, .. }
    ));
    assert!(matches!(
        EngineRead::of(r.as_ref(), "other-proj", "bravo", now),
        EngineRead::Fresh { view: None, .. }
    ));
    // A stale view is not ready.
    let stale = keitora_tasks::StatusSession {
        stale: true,
        ..idle.clone()
    };
    keitora_tasks::status_store::write_status_for_test(
        &env.status_path(),
        writer_at(now),
        std::slice::from_ref(&stale),
    )
    .unwrap();
    assert!(matches!(
        EngineRead::of(open().as_ref(), PROJECT, "bravo", now),
        EngineRead::NotReady { view: Some(_), .. }
    ));
    // herdr down.
    let down = keitora_tasks::WriterRow {
        herdr_link: "down".into(),
        ..writer_at(now)
    };
    keitora_tasks::status_store::write_status_for_test(&env.status_path(), down, &[idle]).unwrap();
    assert!(matches!(
        EngineRead::of(open().as_ref(), PROJECT, "bravo", now),
        EngineRead::NotReady { .. }
    ));
    // No heartbeat for 15 s.
    assert_eq!(
        EngineRead::of(open().as_ref(), PROJECT, "bravo", now + 15_001),
        EngineRead::Stale {
            heartbeat_at: Some(now)
        }
    );
}

#[test]
fn turn_row_is_delivered_as_turn_finished_with_reply_source_none() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    app_publishes(&env, &[view("bravo", "working", 12, None, false, 0)]);
    let (_, delivery) = app_delivers(&env, "bravo");
    let other = env.other();
    env.on_sleep(move |n| {
        if n == 3 {
            other
                .drive_turn_insert(&keitora_tasks::NewDriveTurn {
                    driven: "bravo",
                    kind: "driver",
                    delivery_id: Some(delivery),
                    outcome: "finished",
                    signal: "herdr",
                    clause: "completion_seq",
                    completion_seq: Some(5),
                    state_change_seq: Some(13),
                    reply: "",
                    reply_source: "none",
                    caveat: None,
                })
                .unwrap();
        }
    });
    let out = env.cli("alpha", &["wait", "bravo"]).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.value["event"], "turn_finished");
    assert_eq!(out.value["reply_source"], "none");
    assert_eq!(out.value["delivery_id"], delivery);
    assert_eq!(out.value["clause"], "completion_seq");
    assert_eq!(
        out.human,
        "  @bravo finished its turn\n(no reply text captured; run: keitora read bravo)"
    );
    // Acked: the delivery is over.
    assert_eq!(
        env.local.drive_get("bravo").unwrap().unwrap().awaiting_turn,
        None
    );
    assert_eq!(err(env.cli("alpha", &["wait", "bravo"])).code, 4);
}

#[test]
fn stall_after_30s_with_no_seq_change() {
    let idle = view("bravo", "idle", 11, Some(4), true, T - 5_000);
    let i = delivered(fresh(idle), T + 30_000);
    match select_event(&[i], 120, 30) {
        Some(WaitEvent::Stalled {
            reason,
            state,
            idle_s,
            delivery_id,
            turn_id,
            ..
        }) => assert_eq!(
            (
                reason.as_str(),
                state.as_str(),
                idle_s,
                delivery_id,
                turn_id
            ),
            (
                "no activity observed since delivery",
                "idle",
                30,
                Some(7),
                None
            )
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn no_stall_at_29s() {
    let idle = view("bravo", "idle", 11, Some(4), true, T - 5_000);
    let i = delivered(fresh(idle.clone()), T + 29_999);
    assert_eq!(kind(&select_event(&[i], 120, 29)).0, "none");
    // Clamped to this call: a re-wait at 40 s of idle waits its own 30 s.
    let i = delivered(fresh(idle), T + 40_000);
    assert_eq!(kind(&select_event(&[i], 120, 5)).0, "none");
}

#[test]
fn working_for_10_minutes_never_stalls() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    app_delivers(&env, "bravo");
    app_publishes(&env, &[view("bravo", "working", 12, None, false, 0)]);
    let out = env
        .cli(
            "alpha",
            &["wait", "bravo", "--timeout", "600", "--stall-after", "10"],
        )
        .unwrap();
    assert_eq!(out.code, 2, "{}", out.value);
    assert_eq!(out.value["event"], "timeout");
}

#[test]
fn blocked_unbrokered_for_10s_stalls() {
    let blocked = view("bravo", "blocked", 12, None, false, T);
    let i = delivered(fresh(blocked.clone()), T + 9_999);
    assert_eq!(kind(&select_event(&[i], 120, 60)).0, "none");
    let i = delivered(fresh(blocked), T + 10_000);
    assert_eq!(
        stall_reason(&select_event(&[i], 120, 60)),
        "blocked on a dialog nothing brokers"
    );
}

#[test]
fn brokered_blocked_is_a_prompt_not_a_stall() {
    let brokered = keitora_tasks::StatusSession {
        brokered: true,
        ..view("bravo", "blocked", 12, None, false, T)
    };
    let i = delivered(fresh(brokered.clone()), T + 60_000);
    assert_eq!(
        kind(&select_event(std::slice::from_ref(&i), 120, 60)).0,
        "none"
    );
    let mut with_prompt = i;
    with_prompt.prompt = Some(fake_prompt(3, "bravo"));
    assert_eq!(kind(&select_event(&[with_prompt], 120, 60)).0, "prompt");

    // Through the DB: the gate's prompt row is what wait reports.
    let env = Env::new();
    env.bind("alpha", "bravo");
    app_delivers(&env, "bravo");
    app_publishes(&env, &[brokered]);
    permission(&env.other(), "bravo");
    let out = env.cli("alpha", &["wait", "bravo"]).unwrap();
    assert_eq!(out.value["event"], "prompt");
}

#[test]
fn restarted_row_is_a_stall_and_resolves_the_delivery() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    app_publishes(&env, &[view("bravo", "idle", 2, None, true, 0)]);
    let (_, delivery) = app_delivers(&env, "bravo");
    app_records(&env, "bravo", "driver", Some(delivery), "restarted", "");
    let out = env.cli("alpha", &["wait", "bravo"]).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.value["event"], "stalled");
    assert_eq!(
        out.value["reason"],
        "agent restarted mid-turn; result unknown"
    );
    assert_eq!(out.value["idle_s"], 0);
    assert_eq!(out.value["delivery_id"], delivery);
    let e = err(env.cli("alpha", &["wait", "bravo"]));
    assert_eq!(e.code, 4, "{}", e.msg);
}

#[test]
fn herdr_unavailable_stalls_after_stall_after() {
    let down = keitora_tasks::WriterRow {
        herdr_link: "down".into(),
        herdr_link_since: T,
        ..writer_at(T)
    };
    let not_ready = |w: keitora_tasks::WriterRow| EngineRead::NotReady {
        writer: w,
        view: None,
    };
    let i = delivered(not_ready(down.clone()), T + 119_000);
    assert_eq!(kind(&select_event(&[i], 120, 500)).0, "none");
    let i = delivered(not_ready(down), T + 120_000);
    assert_eq!(
        stall_reason(&select_event(&[i], 120, 500)),
        "herdr unavailable for 120s (down)"
    );
    // Up, but the last snapshot is old.
    let old_snapshot = keitora_tasks::WriterRow {
        last_snapshot_at: Some(T),
        herdr_link_since: T - 600_000,
        ..writer_at(T)
    };
    let i = delivered(not_ready(old_snapshot), T + 130_000);
    assert_eq!(
        stall_reason(&select_event(&[i], 120, 500)),
        "herdr unavailable for 130s (stale snapshot)"
    );
}

#[test]
fn no_app_uses_the_legacy_hook_turn_then_stalls_with_no_app() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    app_delivers(&env, "bravo");
    env.other()
        .session_reply_record("bravo", "from the hook")
        .unwrap();
    let out = env.cli("alpha", &["wait", "bravo"]).unwrap();
    assert_eq!(out.value["event"], "turn_finished");
    assert_eq!(out.value["reply_source"], "hook");
    assert_eq!(out.value["signal"], "hook");
    assert_eq!(out.value["turn_id"], Value::Null);
    assert_eq!(out.value["last_message"], "from the hook");
    assert_eq!(
        env.local.drive_get("bravo").unwrap().unwrap().awaiting_turn,
        None
    );
    // The next delivery's turn never comes: a stall once --stall-after has passed.
    app_delivers(&env, "bravo");
    let out = env
        .cli("alpha", &["wait", "bravo", "--stall-after", "10"])
        .unwrap();
    assert_eq!(out.value["event"], "stalled");
    assert_eq!(out.value["reason"], "no Keitora app is updating status");
    assert_eq!(out.value["idle_s"], 10);
}

#[test]
fn fresh_app_ignores_session_replies_for_a_new_delivery() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let (_, delivery) = app_delivers(&env, "bravo");
    app_publishes(&env, &[view("bravo", "working", 12, None, false, 0)]);
    // The hook bumps its turn, but the app records this delivery's turns.
    env.other().session_reply_record("bravo", "hook").unwrap();
    let out = env
        .cli("alpha", &["wait", "bravo", "--timeout", "3"])
        .unwrap();
    assert_eq!(out.code, 2, "{}", out.value);
    app_records(&env, "bravo", "driver", Some(delivery), "finished", "hook");
    let out = env.cli("alpha", &["wait", "bravo"]).unwrap();
    assert_eq!(out.value["event"], "turn_finished");
    assert!(out.value["turn_id"].is_i64());
    assert_eq!(out.value["turn"], 1, "the hook's counter rides along");
    // Delivered once.
    assert_eq!(err(env.cli("alpha", &["wait", "bravo"])).code, 4);
}

#[test]
fn legacy_check_applies_to_a_delivery_without_delivery_id() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    app_publishes(&env, &[view("bravo", "idle", 13, Some(5), true, 0)]);
    // An older app set awaiting_turn without a delivery id.
    env.other()
        .drive_restore_awaiting_turn("bravo", Some(1))
        .unwrap();
    env.other()
        .session_reply_record("bravo", "old app")
        .unwrap();
    let out = env.cli("alpha", &["wait", "bravo"]).unwrap();
    assert_eq!(out.value["event"], "turn_finished");
    assert_eq!(out.value["reply_source"], "hook");
    assert_eq!(out.value["last_message"], "old app");
}

#[test]
fn legacy_hook_turn_finishes_when_the_engine_sees_no_agent() {
    let finished = |engine: EngineRead| {
        let mut i = delivered(engine, T);
        i.reply = Some(reply("bravo", 1));
        kind(&select_event(&[i], 120, 0)).0
    };
    // No view of the worktree at all: only the hook can report this turn.
    let none = EngineRead::Fresh {
        view: None,
        writer: writer_at(T),
    };
    assert_eq!(finished(none), "turn_finished");
    let absent = keitora_tasks::StatusSession {
        agent: "absent".into(),
        state: "offline".into(),
        ..view("bravo", "idle", 11, Some(4), false, T)
    };
    assert_eq!(finished(fresh(absent)), "turn_finished");
    // An agent the engine sees: it records the turn, the hook reply is ignored.
    let present = view("bravo", "idle", 12, Some(5), true, T);
    assert_eq!(finished(fresh(present)), "none");
}

#[test]
fn human_turn_is_delivered_once() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    app_records(&env, "bravo", "human", None, "finished", "typed by hand");
    let out = env.cli("alpha", &["wait", "bravo"]).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.value["event"], "human_turn");
    assert_eq!(out.value["reply"], "typed by hand");
    assert_eq!(out.value["reply_source"], "hook");
    assert_eq!(
        out.human,
        "  @bravo: the human finished a turn\ntyped by hand"
    );
    assert_eq!(err(env.cli("alpha", &["wait", "bravo"])).code, 4);
}

#[test]
fn event_priority_prompt_turn_human_refused_stalled() {
    let mut prompt = input("alpha");
    prompt.prompt = Some(fake_prompt(1, "alpha"));
    let mut finished = input("bravo");
    finished.unacked = vec![turn_row(4, "driver", "finished")];
    let mut human = input("charlie");
    human.unacked = vec![turn_row(5, "human", "finished")];
    let mut refused = input("wt");
    refused.refused_send = Some(RefusedSend {
        request_id: 2,
        reason: "no".into(),
    });
    let mut stalled = input("main");
    stalled.awaiting_turn = Some(1);
    stalled.unacked = vec![turn_row(6, "driver", "restarted")];
    let all = [stalled, refused, human, finished, prompt];
    let order = [
        "prompt",
        "turn_finished",
        "human_turn",
        "send_refused",
        "stalled",
    ];
    for (n, want) in order.iter().enumerate() {
        let got = select_event(&all[..all.len() - n], 120, 0);
        assert_eq!(kind(&got).0, *want, "with {} inputs", all.len() - n);
    }
    // A worktree's own human turn waits behind its driver turn.
    let mut both = input("bravo");
    both.unacked = vec![
        turn_row(3, "human", "finished"),
        turn_row(4, "driver", "finished"),
    ];
    match select_event(&[both], 120, 0) {
        Some(WaitEvent::TurnFinished {
            turn: Finished::Recorded { row, .. },
            ..
        }) => assert_eq!(row.id, 4),
        other => panic!("{other:?}"),
    }
}

#[test]
fn nothing_to_wait_for_requires_no_unacked_turn() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    assert_eq!(err(env.cli("alpha", &["wait", "bravo"])).code, 4);
    let id = app_records(&env, "bravo", "human", None, "finished", "");
    assert_eq!(
        env.cli("alpha", &["wait", "bravo"]).unwrap().value["turn_id"],
        id
    );
    assert_eq!(err(env.cli("alpha", &["wait", "bravo"])).code, 4);
}

#[test]
fn a_lost_ack_race_keeps_polling() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    let id = app_records(&env, "bravo", "human", None, "finished", "");
    let rival = env.other();
    let wp = |w: &str| env.wt_dir(w);
    let reg = || Ok(());
    let stdin = |_: usize| Ok(Vec::new());
    let cli_of = |_: &str| None;
    let ctx = Ctx {
        local: &env.local,
        me: "alpha".into(),
        worktrees: env.worktrees.clone(),
        worktree_path: &wp,
        worktree_cli: &cli_of,
        registered: &reg,
        clock: &env.clock,
        stdin: &stdin,
        project: PROJECT.into(),
        status_db: Some(env.status_path()),
    };
    let args = WaitArgs {
        worktrees: vec!["bravo".into()],
        timeout: 3,
        stall_after: 120,
    };
    // Another wait acks the row between this one's read and its ack.
    let out = wait_with(&ctx, &args, &mut |i, s, e| {
        let ev = select_event(i, s, e);
        if ev.is_some() {
            rival.drive_turn_ack(id).unwrap();
        }
        ev
    })
    .unwrap();
    assert_eq!(out.code, 2, "{}", out.value);
}

#[test]
fn a_lost_legacy_ack_race_keeps_polling() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    // No app: the hook's reply finishes the turn through the legacy path.
    let (turn, _) = app_delivers(&env, "bravo");
    env.other().session_reply_record("bravo", "hook").unwrap();
    let rival = env.other();
    let wp = |w: &str| env.wt_dir(w);
    let reg = || Ok(());
    let stdin = |_: usize| Ok(Vec::new());
    let cli_of = |_: &str| None;
    let ctx = Ctx {
        local: &env.local,
        me: "alpha".into(),
        worktrees: env.worktrees.clone(),
        worktree_path: &wp,
        worktree_cli: &cli_of,
        registered: &reg,
        clock: &env.clock,
        stdin: &stdin,
        project: PROJECT.into(),
        status_db: Some(env.status_path()),
    };
    let args = WaitArgs {
        worktrees: vec!["bravo".into()],
        timeout: 3,
        stall_after: 120,
    };
    // Another wait acks the turn between this one's read and its ack.
    let out = wait_with(&ctx, &args, &mut |i, s, e| {
        let ev = select_event(i, s, e);
        if ev.is_some() {
            assert!(rival.drive_ack_turn_legacy("bravo", turn).unwrap());
        }
        ev
    })
    .unwrap();
    assert_eq!(out.code, 2, "{}", out.value);
}

#[test]
fn drives_reads_engine_state_when_fresh() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    // No hook row for bravo; the engine sees its agent idle.
    app_publishes(&env, &[view("bravo", "idle", 11, Some(4), true, 0)]);
    let (_, delivery) = app_delivers(&env, "bravo");
    app_records(&env, "bravo", "human", None, "finished", "");
    let out = env.cli("alpha", &["drives"]).unwrap();
    let d = &out.value["drives"][0];
    assert_eq!(d["state"], "idle");
    assert_eq!(d["state_source"], "engine");
    assert_eq!(d["agent"], "present");
    assert_eq!(d["herdr_status"], "idle");
    assert_eq!(d["stale"], false);
    assert_eq!(d["delivery_id"], delivery);
    assert!(d["delivered_at"].is_string());
    assert_eq!(d["unacked_turns"], 1);
    // A stale engine view is marked.
    let stale = keitora_tasks::StatusSession {
        stale: true,
        ..view("bravo", "idle", 11, Some(4), true, 0)
    };
    app_publishes(&env, &[stale]);
    let out = env.cli("alpha", &["drives"]).unwrap();
    assert_eq!(out.value["drives"][0]["stale"], true);
    assert!(out.human.contains("[stale]"), "{}", out.human);
}

#[test]
fn drives_falls_back_to_rows_when_stale() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    env.other()
        .session_upsert("bravo", "busy", None, None)
        .unwrap();
    let out = env.cli("alpha", &["drives"]).unwrap();
    let d = &out.value["drives"][0];
    assert_eq!(d["state"], "busy");
    assert_eq!(d["state_source"], "hooks");
    assert_eq!(d["stale"], false);
    assert_eq!(d["delivery_id"], Value::Null);
    assert_eq!(d["unacked_turns"], 0);
}

#[test]
fn read_last_picks_the_newer_of_turn_and_reply() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    env.other()
        .session_reply_record("bravo", "older hook reply")
        .unwrap();
    // Recorded in the same second or later (the turn has milliseconds): the turn wins.
    let id = app_records(&env, "bravo", "human", None, "finished", "the turn's reply");
    let out = env.cli("alpha", &["read", "bravo", "--last"]).unwrap();
    assert_eq!(out.value["source"], "turn");
    assert_eq!(out.value["turn_id"], id);
    assert_eq!(out.value["kind"], "human");
    assert_eq!(out.value["turn"], 1);
    assert_eq!(out.human, "the turn's reply");
    assert!(
        !env.local.drive_turns_unacked("bravo").unwrap().is_empty(),
        "read never acks"
    );

    // The comparison itself.
    let t = turn_row(1, "driver", "finished");
    let at = |s: &str| SessionReply {
        worktree: "bravo".into(),
        turn: 2,
        last_message: "x".into(),
        updated_at: s.into(),
    };
    assert!(newer_reply(&t, &at("2026-01-01 10:00:01")));
    assert!(!newer_reply(&t, &at("2026-01-01 10:00:00")));
    assert!(!newer_reply(&t, &at("2026-01-01 10:00:00.123")), "a tie");
    assert!(!newer_reply(&t, &at("garbage")));
}

#[test]
fn read_last_none_reply_prints_the_hint() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    app_records(&env, "bravo", "human", None, "finished", "");
    let out = env.cli("alpha", &["read", "bravo", "--last"]).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.value["source"], "turn");
    assert_eq!(out.value["reply_source"], "none");
    assert_eq!(
        out.human,
        "(no reply text captured for this turn; run: keitora read bravo)"
    );
}

#[test]
fn turn_finished_json_keeps_the_legacy_keys() {
    let row = DriveTurn {
        caveat: Some("agy may still run background tasks".into()),
        ..turn_row(12, "driver", "finished")
    };
    let out = event_output(&WaitEvent::TurnFinished {
        worktree: "bravo".into(),
        turn: Finished::Recorded {
            row: Box::new(row),
            reply_turn: Some(3),
        },
    });
    assert_eq!(
        out.value,
        json!({"event": "turn_finished", "worktree": "bravo", "turn_id": 12, "delivery_id": 7,
               "kind": "driver", "reply": "the reply", "reply_source": "hook",
               "caveat": "agy may still run background tasks", "signal": "herdr",
               "clause": "completion_seq", "completed_at": "2026-01-01 10:00:00.123",
               "turn": 3, "last_message": "the reply", "updated_at": "2026-01-01 10:00:00.123"})
    );
    assert_eq!(
        out.human,
        "  @bravo finished its turn\nthe reply\nagy may still run background tasks"
    );
    let legacy = event_output(&WaitEvent::TurnFinished {
        worktree: "bravo".into(),
        turn: Finished::Legacy {
            awaiting: 3,
            reply: reply("bravo", 3),
        },
    });
    assert_eq!(
        legacy.value,
        json!({"event": "turn_finished", "worktree": "bravo", "turn_id": null,
               "delivery_id": null, "kind": "driver", "reply": "reply 3",
               "reply_source": "hook", "caveat": null, "signal": "hook", "clause": null,
               "completed_at": "2026-01-01 10:00:00", "turn": 3, "last_message": "reply 3",
               "updated_at": "2026-01-01 10:00:00"})
    );
}

#[test]
fn an_absent_agent_with_a_delivery_stalls_after_stall_after() {
    let absent = keitora_tasks::StatusSession {
        agent: "absent".into(),
        state: "offline".into(),
        ..view("bravo", "idle", 11, Some(4), false, T)
    };
    let i = delivered(fresh(absent), T + 120_000);
    assert_eq!(
        stall_reason(&select_event(&[i], 120, 500)),
        "no agent is running; result unknown"
    );
    let gone = delivered(
        EngineRead::Fresh {
            view: None,
            writer: writer_at(T),
        },
        T + 119_000,
    );
    assert_eq!(kind(&select_event(&[gone], 120, 500)).0, "none");
}

// ---- timeout diagnosis ----

/// An `app_projects` row for `proj`, open and reported, with a running watcher.
fn app_row() -> keitora_tasks::AppProject {
    keitora_tasks::AppProject {
        project: PROJECT.into(),
        registry_key: PROJECT.into(),
        db_path: "D:/repos/proj/tasks.db".into(),
        open: true,
        open_reported_at: Some(1),
        watcher_state: "running".into(),
        watcher_started_at: Some(1),
        watcher_last_event_at: None,
        last_pass_at: None,
        last_pass_trigger: None,
        pending_count: 1,
        oldest_pending_at: None,
        restarts: 0,
        last_error: String::new(),
        last_error_at: None,
    }
}

/// A live app (heartbeat well ahead of the fake clock) with `row` for the project.
fn app_presence(env: &Env, row: Option<keitora_tasks::AppProject>) {
    app_publishes(env, &[]);
    if let Some(row) = row {
        keitora_tasks::status_store::write_app_project_for_test(&env.status_path(), &row).unwrap();
    }
}

/// `send bravo --wait --timeout 2`, timed out; the outcome and the withdrawn row.
fn timed_out_send(env: &Env) -> (Output, DriveRequest) {
    timed_out_send_after(env, 2)
}

/// [`timed_out_send`] with `--timeout {secs}`.
fn timed_out_send_after(env: &Env, secs: u64) -> (Output, DriveRequest) {
    env.bind("alpha", "bravo");
    let secs = secs.to_string();
    let out = env
        .cli(
            "alpha",
            &["send", "bravo", "hi", "--wait", "--timeout", &secs],
        )
        .unwrap();
    assert_eq!(out.code, 2);
    assert_eq!(out.value["timed_out"], true);
    let r = last_request(&env.local);
    assert_eq!(r.status, "refused");
    (out, r)
}

/// The row's stored reason, the printed line and the JSON code all name `cause`.
fn assert_diagnosed(out: &Output, r: &DriveRequest, code: &str, cause: &str) {
    assert_diagnosed_after(out, r, code, cause, 2);
}

/// [`assert_diagnosed`] for a `--timeout {secs}` wait.
fn assert_diagnosed_after(out: &Output, r: &DriveRequest, code: &str, cause: &str, secs: u64) {
    assert_eq!(out.value["diagnosis"]["code"], code, "{}", out.value);
    assert_eq!(out.value["diagnosis"]["cause"], cause);
    assert_eq!(r.reason, format!("timed out after {secs}s: {cause}"));
    assert_eq!(
        out.human,
        format!(
            "  timed out after {secs}s: {cause} (send of @bravo (request {}))",
            r.id
        )
    );
}

/// A wait long enough that the app's throttled presence columns have caught up.
const PAST_LAG_S: u64 = (keitora_tasks::PRESENCE_TOUCH_LAG_MS / 1000 + 2) as u64;

#[test]
fn timeout_without_a_status_db_keeps_the_legacy_wording() {
    let env = Env::new();
    let (out, r) = timed_out_send(&env);
    assert_eq!(out.value["diagnosis"]["code"], "no_status");
    assert_eq!(r.reason, "no running Keitora app picked it up within 2s");
    assert_eq!(
        out.human,
        format!(
            "  timed out after 2s: no running Keitora app picked up the send of @bravo (request {})",
            r.id
        )
    );
}

#[test]
fn timeout_with_a_stale_app_says_when_it_last_updated() {
    let env = Env::new();
    keitora_tasks::status_store::write_status_for_test(
        &env.status_path(),
        writer_at(env.clock.wall_ms() - 300_000),
        &[],
    )
    .unwrap();
    let (out, r) = timed_out_send(&env);
    assert_diagnosed(
        &out,
        &r,
        "app_stale",
        "no Keitora app is updating status (last update 5m ago); start Keitora, then retry",
    );
}

#[test]
fn timeout_with_no_app_ever_reporting_says_so() {
    let env = Env::new();
    // A status.db with no writer row.
    drop(keitora_tasks::StatusWriter::open(&env.status_path()).unwrap());
    let (out, r) = timed_out_send(&env);
    assert_diagnosed(
        &out,
        &r,
        "app_stale",
        "no Keitora app has reported status on this machine; start Keitora, then retry",
    );
}

#[test]
fn timeout_with_the_project_unknown_to_the_app() {
    let env = Env::new();
    app_presence(&env, None);
    let (out, r) = timed_out_send(&env);
    assert_diagnosed(
        &out,
        &r,
        "project_unknown",
        "Keitora is running but hasn't reported project proj yet (it may predate this check or have just been registered)",
    );
}

#[test]
fn timeout_before_the_window_reported() {
    let env = Env::new();
    app_presence(
        &env,
        Some(keitora_tasks::AppProject {
            open: false,
            open_reported_at: None,
            ..app_row()
        }),
    );
    let (out, r) = timed_out_send(&env);
    assert_diagnosed(
        &out,
        &r,
        "window_unreported",
        "Keitora is running but its window hasn't reported open projects",
    );
}

#[test]
fn timeout_with_the_project_not_open_in_the_window() {
    let env = Env::new();
    app_presence(
        &env,
        Some(keitora_tasks::AppProject {
            open: false,
            ..app_row()
        }),
    );
    let (out, r) = timed_out_send(&env);
    assert_diagnosed(
        &out,
        &r,
        "project_not_open",
        "Keitora is running but project proj is not open in its window; open it in Keitora, then retry",
    );
}

#[test]
fn timeout_with_the_watcher_not_running() {
    let env = Env::new();
    app_presence(
        &env,
        Some(keitora_tasks::AppProject {
            watcher_state: "exited".into(),
            ..app_row()
        }),
    );
    let (out, r) = timed_out_send(&env);
    assert_diagnosed(
        &out,
        &r,
        "watcher_not_running",
        "Keitora is running but its watcher for proj is not running (it restarts on its own)",
    );
}

#[test]
fn timeout_with_a_watcher_that_never_fired() {
    let env = Env::new();
    app_presence(&env, Some(app_row()));
    let (out, r) = timed_out_send_after(&env, PAST_LAG_S);
    assert_diagnosed_after(
        &out,
        &r,
        "watcher_silent",
        "Keitora is running but its watcher for proj hasn't fired since it started",
        PAST_LAG_S,
    );
}

#[test]
fn a_young_request_is_not_blamed_on_a_watcher_column_that_trails() {
    // The app queues the watcher's last event for its next heartbeat: a request
    // this young may have fired it already, unwritten.
    let env = Env::new();
    let last = env.clock.wall_ms() - 600_000;
    app_presence(
        &env,
        Some(keitora_tasks::AppProject {
            watcher_last_event_at: Some(last),
            ..app_row()
        }),
    );
    let (out, r) = timed_out_send(&env);
    assert_diagnosed(
        &out,
        &r,
        "not_examined",
        "Keitora is running but has not examined the request (last pass never)",
    );
}

#[test]
fn timeout_with_a_watcher_silent_since_before_the_request() {
    let env = Env::new();
    let last = env.clock.wall_ms() - 600_000;
    app_presence(
        &env,
        Some(keitora_tasks::AppProject {
            watcher_last_event_at: Some(last),
            ..app_row()
        }),
    );
    let (out, r) = timed_out_send_after(&env, PAST_LAG_S);
    assert_diagnosed_after(
        &out,
        &r,
        "watcher_silent",
        &format!(
            "Keitora is running but its watcher for proj hasn't fired since {}",
            clock_text(last)
        ),
        PAST_LAG_S,
    );
}

#[test]
fn timeout_with_the_request_never_examined() {
    let env = Env::new();
    let later = env.clock.wall_ms() + 600_000;
    app_presence(
        &env,
        Some(keitora_tasks::AppProject {
            watcher_last_event_at: Some(later),
            ..app_row()
        }),
    );
    let (out, r) = timed_out_send(&env);
    assert_diagnosed(
        &out,
        &r,
        "not_examined",
        "Keitora is running but has not examined the request (last pass never)",
    );

    // With a pass on record, its time.
    let env = Env::new();
    let pass = env.clock.wall_ms() - 5_000;
    app_presence(
        &env,
        Some(keitora_tasks::AppProject {
            watcher_last_event_at: Some(later),
            last_pass_at: Some(pass),
            ..app_row()
        }),
    );
    let (out, r) = timed_out_send(&env);
    assert_diagnosed(
        &out,
        &r,
        "not_examined",
        &format!(
            "Keitora is running but has not examined the request (last pass {})",
            clock_text(pass)
        ),
    );
}

#[test]
fn timeout_of_a_request_the_app_saw_names_its_wait_reason() {
    let env = Env::new();
    app_presence(&env, Some(app_row()));
    let app = env.other();
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            app.drive_request_note_wait(r.id, "waiting for @bravo to be idle (herdr: working)")
                .unwrap();
        }
    });
    let (out, r) = timed_out_send(&env);
    assert_diagnosed(
        &out,
        &r,
        "waiting",
        "Keitora saw it: waiting for @bravo to be idle (herdr: working)",
    );
}

#[test]
fn timeout_of_a_request_the_app_saw_without_a_reason() {
    let env = Env::new();
    app_presence(&env, Some(app_row()));
    let app = env.other();
    env.on_sleep(move |_| {
        for r in app.drive_requests_pending().unwrap() {
            app.drive_request_note_wait(r.id, "").unwrap();
        }
    });
    let (out, r) = timed_out_send(&env);
    assert_diagnosed(
        &out,
        &r,
        "no_reason",
        "Keitora examined it but recorded no reason",
    );
}

#[test]
fn a_read_and_a_wake_carry_the_diagnosis_too() {
    let env = Env::new();
    app_presence(&env, Some(app_row()));
    env.bind("alpha", "bravo");
    let secs = PAST_LAG_S.to_string();
    let out = env
        .cli("alpha", &["read", "bravo", "--timeout", &secs])
        .unwrap();
    let r = last_request(&env.local);
    assert_eq!(out.code, 2);
    assert_eq!(out.value["diagnosis"]["code"], "watcher_silent");
    assert!(
        out.human
            .ends_with(&format!("(read of @bravo (request {}))", r.id)),
        "{}",
        out.human
    );
    let out = env
        .cli("alpha", &["wake", "bravo", "--wait", "--timeout", &secs])
        .unwrap();
    assert_eq!(out.value["diagnosis"]["code"], "watcher_silent");
}

#[test]
fn ages_and_dispatch_diagnoses_follow_the_same_rules() {
    assert_eq!(
        [0, 119, 120, 7199, 7200].map(age_text),
        ["0s", "119s", "2m", "119m", "2h"]
    );
    let what = "dispatch of #4 to @bravo (request 9)";
    let d = |p: &PresenceReading| diagnose_dispatch_timeout(p, PROJECT, what);
    assert_eq!(d(&PresenceReading::Unavailable).code, "no_status");
    assert_eq!(
        d(&PresenceReading::Unavailable).cancel_reason(30),
        "no running Keitora app picked it up within 30s"
    );
    assert_eq!(
        d(&PresenceReading::Stale { age_s: Some(30) }).code,
        "app_stale"
    );
    assert_eq!(d(&PresenceReading::Stale { age_s: None }).code, "app_stale");
    assert_eq!(
        d(&PresenceReading::Fresh { project: None }).code,
        "project_unknown"
    );
    let closed = keitora_tasks::AppProject {
        open: false,
        ..app_row()
    };
    let not_open = d(&PresenceReading::Fresh {
        project: Some(closed),
    });
    assert_eq!(not_open.code, "project_not_open");
    assert_eq!(
        not_open.cancel_reason(30),
        "timed out after 30s: Keitora is running but project proj is not open in its window; open it in Keitora, then retry"
    );
    assert_eq!(
        d(&PresenceReading::Fresh {
            project: Some(app_row())
        })
        .code,
        "not_examined"
    );
}

// ---- health findings on stalls and timeouts ----

/// The app's findings for `proj`: two for bravo (warn, error), one info and one for
/// alpha (never shown for bravo), and an older warn (cut by the cap of two).
fn app_reports_findings(env: &Env) {
    let at = env.clock.wall_ms();
    let row = |wt: &str, check: &str, severity: &str, last_seen: i64| keitora_tasks::FindingRow {
        project: PROJECT.into(),
        worktree: wt.into(),
        check_id: check.into(),
        severity: severity.into(),
        message: format!("{check} says so"),
        detail: "{}".into(),
        first_seen: at,
        last_seen,
        occurrences: 1,
        cleared_at: None,
        remedy: String::new(),
        remedy_at: None,
    };
    keitora_tasks::status_store::write_findings_for_test(
        &env.status_path(),
        &[
            row("bravo", "hooks_vs_herdr", "warn", at),
            row("bravo", "hooks_missing", "error", at - 5),
            row("bravo", "worktree_identity", "info", at),
            row("bravo", "herdr_unknown", "warn", at - 9),
            row("alpha", "agy_plumbing", "error", at),
        ],
    )
    .unwrap();
}

#[test]
fn a_stalled_wait_carries_the_worktree_health_findings() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    app_publishes(&env, &[view("bravo", "idle", 2, None, true, 0)]);
    app_reports_findings(&env);
    let (_, delivery) = app_delivers(&env, "bravo");
    app_records(&env, "bravo", "driver", Some(delivery), "restarted", "");
    let out = env.cli("alpha", &["wait", "bravo"]).unwrap();
    assert_eq!(out.value["event"], "stalled");
    assert_eq!(
        out.value["findings"],
        json!([
            {"check_id": "hooks_missing", "severity": "error", "message": "hooks_missing says so"},
            {"check_id": "hooks_vs_herdr", "severity": "warn", "message": "hooks_vs_herdr says so"},
        ])
    );
    assert_eq!(
        out.human,
        "  @bravo looks stalled: agent restarted mid-turn; result unknown; try: keitora read \
         bravo\n  health: hooks_missing says so\n  health: hooks_vs_herdr says so"
    );
}

#[test]
fn a_stall_without_a_fresh_app_carries_no_findings() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    app_reports_findings(&env);
    app_delivers(&env, "bravo");
    let out = env
        .cli("alpha", &["wait", "bravo", "--stall-after", "10"])
        .unwrap();
    assert_eq!(out.value["event"], "stalled");
    assert!(out.value.get("findings").is_none());
    assert!(!out.human.contains("health:"), "{}", out.human);
}

#[test]
fn a_timed_out_request_carries_findings_while_the_app_is_fresh() {
    let env = Env::new();
    env.bind("alpha", "bravo");
    app_publishes(&env, &[]);
    app_reports_findings(&env);
    let out = env
        .cli(
            "alpha",
            &["send", "bravo", "hi", "--wait", "--timeout", "2"],
        )
        .unwrap();
    assert_eq!(out.code, 2);
    assert_eq!(out.value["timed_out"], true);
    assert_eq!(out.value["findings"].as_array().unwrap().len(), 2);
    assert_eq!(out.value["findings"][0]["check_id"], "hooks_missing");
    let lines: Vec<&str> = out.human.lines().collect();
    assert!(
        lines[0].starts_with("  timed out after 2s"),
        "{}",
        out.human
    );
    assert_eq!(
        &lines[1..],
        [
            "  health: hooks_missing says so",
            "  health: hooks_vs_herdr says so"
        ]
    );
    // An answered request carries none.
    let out = env.cli("alpha", &["send", "bravo", "hi"]).unwrap();
    assert!(out.value.get("findings").is_none());
}
