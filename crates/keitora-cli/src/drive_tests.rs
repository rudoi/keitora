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
    t: Cell<Duration>,
    sleeps: Cell<u32>,
    /// Runs after each sleep with the sleep count (another connection's writes).
    hook: RefCell<Option<SleepHook>>,
}

impl FakeClock {
    fn new() -> Self {
        FakeClock {
            base: Instant::now(),
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
    let n = ls.drive_set_awaiting_turn(wt).unwrap().unwrap();
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
            awaiting, reply, ..
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
    assert_eq!(kind(&select_event(&[i], 120, 1000)).0, "none");
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
        Some(WaitEvent::Stalled { idle_s, state, .. }) => {
            assert_eq!((idle_s, state.as_str()), (125, "busy"))
        }
        other => panic!("{other:?}"),
    }
    // Recent heartbeat: not stalled however long the call has run.
    i.idle_secs = Some(10);
    assert_eq!(kind(&select_event(&[i.clone()], 120, 1000)).0, "none");
    // Not busy, or nothing awaited: never stalled.
    i.idle_secs = Some(1000);
    i.state = Some("waiting".into());
    assert_eq!(kind(&select_event(&[i.clone()], 120, 1000)).0, "none");
    i.state = Some("busy".into());
    i.awaiting_turn = None;
    assert_eq!(kind(&select_event(&[i.clone()], 120, 1000)).0, "none");
    i.awaiting_turn = Some(1);
    i.idle_secs = None;
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
    assert_eq!(out.human, "  @bravo finished turn 1\nall done");
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
        state: "busy".into(),
        idle_s: 125,
    });
    assert_eq!(
        stalled.human,
        "  @bravo looks stalled (busy, no activity for 125s); try: keitora read bravo"
    );
    assert_eq!(
        stalled.value,
        json!({"event": "stalled", "worktree": "bravo", "state": "busy", "idle_s": 125})
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
