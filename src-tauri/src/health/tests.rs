//! The health checks fire and clear on their boundaries; the reconciler inserts,
//! updates, clears, re-opens and prunes as documented; remedies are rate-limited and
//! never run with global writes off. No herdr, no clock, no real home.

use std::sync::Arc;

use keitora_tasks::{CliKind, FindingKey, FindingWrite, ProjectTrust, StatusSession};
use serde_json::json;

use super::reconcile::{Reconciler, Scope, CLEARED_KEEP_MS, LAST_SEEN_FLUSH_MS, REMEDY_EVERY_MS};
use super::*;
use crate::engine::turns::{CompletionNote, TurnKind};
use crate::hooks::{
    AgyFacts, ClaudeFacts, CodexFacts, LaunchRecord, HOOK_VERSION, PANE_MARKER_ENV,
};

const T0: i64 = 1_800_000_000_000;
const S: i64 = 1000;

// ── Fixtures ────────────────────────────────────────────────────────────────

fn view(agent: &str, status: &str) -> StatusSession {
    StatusSession {
        project: "proj".into(),
        worktree: "bravo".into(),
        state: "idle".into(),
        agent: agent.into(),
        agent_kind: Some("id".into()),
        herdr_status: Some(status.into()),
        settled: true,
        stale: false,
        interactive_ready: Some(true),
        pane_id: Some("p1".into()),
        terminal_id: Some("t1".into()),
        agent_session: Some("s-live".into()),
        state_change_seq: Some(4),
        completion_seq: Some(3),
        brokered: false,
        escalated_reason: None,
        last_change_at: T0,
    }
}

fn key_health(present_since: Option<i64>, unknown_since: Option<i64>) -> KeyHealth {
    KeyHealth {
        present_since_ms: present_since,
        unknown_since_ms: unknown_since,
        pane_id: Some("p1".into()),
    }
}

fn hook(state: &str, written: Option<i64>, session: Option<&str>) -> HookEvidence {
    HookEvidence {
        state: state.into(),
        written_ms: written,
        brokered: false,
        escalated_reason: None,
        session_id: session.map(str::to_string),
    }
}

fn launch(cli: CliKind) -> LaunchRecord {
    LaunchRecord {
        at_ms: T0,
        cli,
        trust_bypass: cli == CliKind::Codex,
        env_names: vec![PANE_MARKER_ENV.to_string()],
        claude_settings: crate::hooks::ClaudeSettings::NotApplicable,
        hook_version: HOOK_VERSION,
        start_refused: None,
    }
}

/// Clean facts for `cli`: everything installed, no launch.
fn facts(cli: CliKind) -> HookFacts {
    HookFacts {
        bin: Some("C:/Users/x/bin/keitora.exe".into()),
        codex: (cli == CliKind::Codex).then_some(CodexFacts {
            hooks_json_ok: true,
            project_trust: ProjectTrust::Trusted,
        }),
        claude: matches!(cli, CliKind::Claude | CliKind::Codex).then_some(ClaudeFacts {
            hooks_installed: true,
        }),
        agy: (cli == CliKind::Antigravity).then_some(AgyFacts {
            key_present: true,
            path_ok: true,
            translocated: false,
        }),
        last_launch: None,
        last_hook_error: None,
    }
}

fn wt(cli: CliKind) -> WorktreeInputs {
    let mut w = WorktreeInputs::new("bravo", cli);
    w.facts = Some(facts(cli));
    w
}

fn project(w: WorktreeInputs) -> ProjectInputs {
    ProjectInputs {
        name: "proj".into(),
        worktrees: vec![w],
        ..Default::default()
    }
}

fn inputs(projects: Vec<ProjectInputs>) -> HealthInputs {
    HealthInputs {
        link: Some(LinkFacts {
            link: HerdrLink::Up,
            since_ms: T0,
            last_snapshot_ms: Some(T0),
        }),
        lease: Some(LeaseState::Owned),
        path_cli: None,
        projects,
    }
}

fn ids(i: &HealthInputs, now: i64) -> Vec<&'static str> {
    evaluate(i, now).into_iter().map(|f| f.check_id).collect()
}

fn only(i: &HealthInputs, now: i64, check: &str) -> Option<Finding> {
    evaluate(i, now).into_iter().find(|f| f.check_id == check)
}

#[test]
fn a_healthy_app_has_no_findings() {
    for cli in [CliKind::Claude, CliKind::Codex, CliKind::Antigravity] {
        let mut w = wt(cli);
        w.view = Some(view("present", "idle"));
        w.key = Some(key_health(Some(T0), None));
        w.hook = Some(hook("idle", Some(T0 + S), Some("s-live")));
        let mut l = launch(cli);
        l.trust_bypass = true;
        w.facts.as_mut().unwrap().last_launch = Some(l);
        let i = inputs(vec![project(w)]);
        assert_eq!(ids(&i, T0 + 3600 * S), Vec::<&str>::new(), "{cli:?}");
    }
}

// ── hooks_vs_herdr ──────────────────────────────────────────────────────────

fn present_with(hook_row: Option<HookEvidence>) -> HealthInputs {
    let mut w = wt(CliKind::Codex);
    w.view = Some(view("present", "idle"));
    w.key = Some(key_health(Some(T0), None));
    w.hook = hook_row;
    inputs(vec![project(w)])
}

#[test]
fn hooks_vs_herdr_fires_after_30s_without_a_hook_row() {
    let i = present_with(None);
    assert!(
        only(&i, T0 + 29 * S, HOOKS_VS_HERDR).is_none(),
        "not at 29 s"
    );
    let f = only(&i, T0 + 31 * S, HOOKS_VS_HERDR).expect("fires at 31 s");
    assert_eq!(f.severity, Severity::Warn);
    assert_eq!(
        f.message,
        "@bravo: herdr sees a codex agent (idle) but its hooks have not reported since the \
         agent appeared (no hook rows). Keitora is using herdr's view."
    );
    assert!(f.remedy.is_none());
}

#[test]
fn hooks_vs_herdr_fires_on_offline_old_or_foreign_rows() {
    let now = T0 + 2 * 3600 * S + S;
    let offline = present_with(Some(hook("offline", Some(T0 + S), None)));
    assert!(only(&offline, now, HOOKS_VS_HERDR)
        .unwrap()
        .message
        .contains("(last hook write 2h ago, state offline)"));
    let old = present_with(Some(hook("idle", Some(T0 - 2 * 3600 * S), None)));
    assert!(only(&old, now, HOOKS_VS_HERDR)
        .unwrap()
        .message
        .contains("(last hook write 4h ago, state idle)"));
    let foreign = present_with(Some(hook("busy", Some(T0 + S), Some("s-old"))));
    assert!(only(&foreign, now, HOOKS_VS_HERDR).is_some());
}

#[test]
fn hooks_vs_herdr_clears_on_a_fresh_write_or_no_agent() {
    let now = T0 + 120 * S;
    // A write at or after the agent appeared (or just before: SessionStart runs
    // before herdr's next listing) is the agent's.
    for at in [T0, T0 + 50 * S, T0 - HOOK_WRITE_SLACK_MS] {
        let i = present_with(Some(hook("idle", Some(at), Some("s-live"))));
        assert!(only(&i, now, HOOKS_VS_HERDR).is_none(), "write at {at}");
    }
    let mut i = present_with(None);
    i.projects[0].worktrees[0].view = Some(view("absent", "idle"));
    assert!(only(&i, now, HOOKS_VS_HERDR).is_none(), "agent absent");
    // A stale view or a link that is down says nothing.
    let mut i = present_with(None);
    i.projects[0].worktrees[0].view.as_mut().unwrap().stale = true;
    assert!(only(&i, now, HOOKS_VS_HERDR).is_none(), "stale view");
}

// ── completion_without_stop ─────────────────────────────────────────────────

fn note(signal: &'static str, source: &'static str) -> CompletionNote {
    CompletionNote {
        kind: TurnKind::Driver,
        signal,
        reply_source: source,
        transcript_miss: (source == "none").then_some("not_found"),
        at_ms: T0,
    }
}

fn with_turns(cli: CliKind, recent: Vec<CompletionNote>) -> HealthInputs {
    let mut w = wt(cli);
    w.turns = Some(TurnHealth {
        recent,
        no_activity: None,
    });
    inputs(vec![project(w)])
}

#[test]
fn completion_without_stop_info_then_warn_then_clears() {
    let hook = note("hook", "hook");
    let transcript = note("herdr", "transcript");
    let none = note("herdr", "none");
    let i = with_turns(CliKind::Codex, vec![hook.clone(), transcript.clone()]);
    assert!(
        only(&i, T0, COMPLETION_WITHOUT_STOP).is_none(),
        "one is not a pattern"
    );

    let i = with_turns(
        CliKind::Codex,
        vec![transcript.clone(), hook.clone(), transcript.clone()],
    );
    let f = only(&i, T0, COMPLETION_WITHOUT_STOP).expect("2 of 3");
    assert_eq!(f.severity, Severity::Info);
    assert_eq!(
        f.message,
        "@bravo: 2 of the last 3 turns finished with no Stop hook; reply text came from the \
         transcript"
    );

    let i = with_turns(CliKind::Codex, vec![transcript.clone(), none.clone()]);
    let f = only(&i, T0, COMPLETION_WITHOUT_STOP).unwrap();
    assert_eq!(f.severity, Severity::Warn, "the latest has no reply");
    assert!(f.message.ends_with("came from nowhere (use keitora read)"));
    assert_eq!(f.detail["transcript_misses"], json!(["not_found"]));

    // Three hook replies in a row clear it, even with older herdr-only turns.
    let i = with_turns(
        CliKind::Codex,
        vec![none.clone(), none, hook.clone(), hook.clone(), hook],
    );
    assert!(only(&i, T0, COMPLETION_WITHOUT_STOP).is_none());
    // agy's hooks never carry a reply: not a finding.
    let i = with_turns(CliKind::Antigravity, vec![transcript.clone(), transcript]);
    assert!(only(&i, T0, COMPLETION_WITHOUT_STOP).is_none());
}

#[test]
fn completion_without_stop_looks_at_the_last_five_only() {
    let t = note("herdr", "transcript");
    let h = note("hook", "hook");
    let i = with_turns(
        CliKind::Claude,
        vec![
            t.clone(),
            t.clone(),
            t,
            h.clone(),
            h.clone(),
            h.clone(),
            h.clone(),
            h,
        ],
    );
    assert!(only(&i, T0, COMPLETION_WITHOUT_STOP).is_none());
}

// ── turn_start_not_observed ─────────────────────────────────────────────────

#[test]
fn turn_start_not_observed_fires_on_a_stall_and_clears_with_it() {
    let mut w = wt(CliKind::Claude);
    w.key = Some(key_health(Some(T0), None));
    w.turns = Some(TurnHealth {
        recent: Vec::new(),
        no_activity: Some((7, T0)),
    });
    let i = inputs(vec![project(w)]);
    let f = only(&i, T0 + S, TURN_START_NOT_OBSERVED).expect("fires");
    assert_eq!(
        f.message,
        "@bravo: a send showed no herdr activity for 30 s; herdr may not recognise this UI \
         (try: herdr agent explain p1)"
    );
    assert_eq!(f.detail["delivery_id"], json!(7));
    assert!(
        only(&i, T0 + 25 * 3600 * S, TURN_START_NOT_OBSERVED).is_none(),
        "a day later"
    );
    // A later delivery that showed `working` resets the engine's record.
    let mut i = i;
    i.projects[0].worktrees[0]
        .turns
        .as_mut()
        .unwrap()
        .no_activity = None;
    assert!(only(&i, T0 + S, TURN_START_NOT_OBSERVED).is_none());
}

// ── hooks_missing ───────────────────────────────────────────────────────────

fn missing_with(cli: CliKind, f: impl FnOnce(&mut HookFacts)) -> Option<Finding> {
    let mut w = wt(cli);
    f(w.facts.as_mut().unwrap());
    only(&inputs(vec![project(w)]), T0, HOOKS_MISSING)
}

#[test]
fn hooks_missing_names_each_fact() {
    let f = missing_with(CliKind::Codex, |f| {
        f.codex.as_mut().unwrap().hooks_json_ok = false
    })
    .unwrap();
    assert_eq!(
        f.message,
        "@bravo: .codex/hooks.json is missing Keitora's hooks; the permission gate and Stop \
         capture won't run"
    );
    assert_eq!(f.severity, Severity::Warn);
    assert_eq!(
        f.remedy,
        Some(Remedy::ReinstallHooks {
            project: "proj".into(),
            worktree: "bravo".into()
        })
    );

    let f = missing_with(CliKind::Codex, |f| {
        let mut l = launch(CliKind::Codex);
        l.trust_bypass = false;
        f.last_launch = Some(l);
    })
    .unwrap();
    assert!(f
        .message
        .contains("the last launch ran without the hook trust bypass"));

    let f = missing_with(CliKind::Claude, |f| {
        f.claude.as_mut().unwrap().hooks_installed = false
    })
    .unwrap();
    assert!(f
        .message
        .contains("Claude's Keitora hooks are not installed"));

    let f = missing_with(CliKind::Claude, |f| {
        let mut l = launch(CliKind::Claude);
        l.hook_version = HOOK_VERSION - 1;
        f.last_launch = Some(l);
    })
    .unwrap();
    assert!(f.message.contains("the last launch ran older hooks"));

    // Several facts are joined with "; ".
    let f = missing_with(CliKind::Codex, |f| {
        f.codex.as_mut().unwrap().hooks_json_ok = false;
        let mut l = launch(CliKind::Codex);
        l.trust_bypass = false;
        f.last_launch = Some(l);
    })
    .unwrap();
    assert_eq!(
        f.message,
        "@bravo: .codex/hooks.json is missing Keitora's hooks; the last launch ran without the \
         hook trust bypass; the permission gate and Stop capture won't run"
    );
}

#[test]
fn hooks_missing_respects_per_launch_settings_and_drive() {
    // Claude's hooks passed with --settings count as installed.
    assert!(missing_with(CliKind::Claude, |f| {
        f.claude.as_mut().unwrap().hooks_installed = false;
        let mut l = launch(CliKind::Claude);
        l.claude_settings = crate::hooks::ClaudeSettings::Applied("x".into());
        f.last_launch = Some(l);
    })
    .is_none());
    // A Codex worktree's own .claude file is not its hooks.
    assert!(missing_with(CliKind::Codex, |f| {
        f.claude.as_mut().unwrap().hooks_installed = false
    })
    .is_none());
    // Driven: an error.
    let mut w = wt(CliKind::Claude);
    w.driven = true;
    w.facts.as_mut().unwrap().claude = Some(ClaudeFacts {
        hooks_installed: false,
    });
    let f = only(&inputs(vec![project(w)]), T0, HOOKS_MISSING).unwrap();
    assert_eq!(f.severity, Severity::Error);
    // Facts clean: nothing. No facts (folder missing): nothing.
    assert!(missing_with(CliKind::Claude, |_| {}).is_none());
    let mut w = wt(CliKind::Claude);
    w.facts = None;
    assert!(only(&inputs(vec![project(w)]), T0, HOOKS_MISSING).is_none());
}

// ── codex_project_untrusted ─────────────────────────────────────────────────

fn untrusted(trust: ProjectTrust, bypass: Option<bool>, cli: CliKind) -> Option<Finding> {
    let mut w = wt(CliKind::Codex);
    w.provider = cli;
    w.codex_config = Some("C:/Users/x/.codex/config.toml".into());
    let f = w.facts.as_mut().unwrap();
    f.codex.as_mut().unwrap().project_trust = trust;
    f.last_launch = bypass.map(|b| {
        let mut l = launch(CliKind::Codex);
        l.trust_bypass = b;
        l
    });
    only(&inputs(vec![project(w)]), T0, CODEX_PROJECT_UNTRUSTED)
}

#[test]
fn codex_project_untrusted_severity_follows_the_bypass() {
    let f = untrusted(ProjectTrust::NotListed, Some(true), CliKind::Codex).unwrap();
    assert_eq!(f.severity, Severity::Info);
    assert_eq!(
        f.message,
        "@bravo: Codex does not list this project as trusted in config.toml (not listed). \
         Keitora's own launches bypass the hook trust prompt, but a hand-launched codex or \
         herdr's own session-id hook may not run its hooks here; trust it in Codex \
         (C:/Users/x/.codex/config.toml)"
    );
    assert!(f.remedy.is_none(), "report only");
    let f = untrusted(
        ProjectTrust::Untrusted("untrusted".into()),
        None,
        CliKind::Codex,
    )
    .unwrap();
    assert_eq!(f.severity, Severity::Warn);
    assert!(f.message.contains("(trust_level untrusted)"));
    assert!(f.remedy.is_none());
    for quiet in [
        ProjectTrust::Trusted,
        ProjectTrust::Unreadable("bad toml".into()),
        ProjectTrust::NotApplicable,
    ] {
        assert!(untrusted(quiet, None, CliKind::Codex).is_none());
    }
    assert!(untrusted(ProjectTrust::NotListed, None, CliKind::Claude).is_none());
}

#[test]
fn codex_project_untrusted_never_touches_codex_config() {
    let tmp = tempfile::tempdir().unwrap();
    let codex = tmp.path().join("codex");
    std::fs::create_dir_all(&codex).unwrap();
    let config = codex.join("config.toml");
    std::fs::write(
        &config,
        "[projects.'D:/repos/other-proj']\ntrust_level = \"trusted\"\n",
    )
    .unwrap();
    let wt_dir = tmp.path().join("bravo");
    std::fs::create_dir_all(&wt_dir).unwrap();
    let snapshot = |p: &std::path::Path| {
        let mut v: Vec<(String, Vec<u8>)> = std::fs::read_dir(p)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read(e.path()).unwrap(),
                )
            })
            .collect();
        v.sort();
        v
    };
    let before = snapshot(&codex);
    let paths = crate::hooks::FactsPaths {
        hook_log: tmp.path().join("hooks.log"),
        agy_hooks_file: None,
        codex_config: Some(config.clone()),
        exe: None,
        now_s: T0 / 1000,
    };
    let key = crate::backend::SessionKey {
        project: "proj".into(),
        worktree: "bravo-untrusted-probe".into(),
    };
    let facts = crate::hooks::hook_facts_in(
        &wt_dir,
        CliKind::Codex,
        &crate::hooks::KeitoraBinSources::default(),
        &key,
        &paths,
        &crate::hooks::SharedFactsFiles::default(),
    );
    let mut w = WorktreeInputs::new("bravo", CliKind::Codex);
    w.facts = Some(facts);
    let found = evaluate(&inputs(vec![project(w)]), T0);
    let f = found
        .iter()
        .find(|f| f.check_id == CODEX_PROJECT_UNTRUSTED)
        .expect("not listed");
    assert!(f.remedy.is_none());
    // Reconciling with remedies allowed still runs nothing for it.
    let mut r = Reconciler::new(true);
    let out = r.reconcile(
        found
            .into_iter()
            .filter(|f| f.check_id == CODEX_PROJECT_UNTRUSTED)
            .collect(),
        None,
        T0,
    );
    assert!(out.remedies.is_empty());
    assert_eq!(
        snapshot(&codex),
        before,
        "no file under the Codex home changed"
    );
}

// ── agy_plumbing ────────────────────────────────────────────────────────────

fn agy_with(f: impl FnOnce(&mut HookFacts)) -> Option<Finding> {
    let mut w = wt(CliKind::Antigravity);
    f(w.facts.as_mut().unwrap());
    only(&inputs(vec![project(w)]), T0, AGY_PLUMBING)
}

#[test]
fn agy_plumbing_names_each_fact_and_translocation_suppresses_the_remedy() {
    let f = agy_with(|f| f.agy.as_mut().unwrap().key_present = false).unwrap();
    assert_eq!(
        f.message,
        "@bravo: agy hooks are not wired (hooks.json has no keitora key); agy sessions show \
         only herdr's view"
    );
    assert!(matches!(f.remedy, Some(Remedy::SyncAgyHooks { .. })));
    let f = agy_with(|f| f.agy.as_mut().unwrap().path_ok = false).unwrap();
    assert!(f.message.contains("the hook command path is missing"));
    let f = agy_with(|f| {
        let mut l = launch(CliKind::Antigravity);
        l.env_names.clear();
        f.last_launch = Some(l);
    })
    .unwrap();
    assert!(f.message.contains("KEITORA_PANE"));
    let f = agy_with(|f| f.agy.as_mut().unwrap().translocated = true).unwrap();
    assert!(f.message.contains("translocated"));
    assert!(
        f.remedy.is_none(),
        "nothing to re-assert from a translocated app"
    );
    assert!(agy_with(|_| {}).is_none(), "facts clean");
}

// ── agent_start_refused ─────────────────────────────────────────────────────

#[test]
fn a_refused_start_is_reported_until_the_agent_comes_up() {
    let mut w = wt(CliKind::Claude);
    let mut l = launch(CliKind::Claude);
    l.start_refused = Some("agent_not_ready".into());
    w.facts.as_mut().unwrap().last_launch = Some(l);
    let i = inputs(vec![project(w)]);
    let f = only(&i, T0 + S, AGENT_START_REFUSED).unwrap();
    assert_eq!(f.severity, Severity::Warn);
    assert!(f.message.starts_with(
        "@bravo: herdr could not start its claude agent (agent_not_ready); Claude may be waiting \
         on its folder-trust prompt"
    ));
    let mut i = i;
    let w = &mut i.projects[0].worktrees[0];
    w.view = Some(view("present", "idle"));
    w.key = Some(key_health(Some(T0 + 5 * S), None));
    assert!(only(&i, T0 + 10 * S, AGENT_START_REFUSED).is_none());
}

// ── worktree_identity ───────────────────────────────────────────────────────

#[test]
fn worktree_identity_compares_folder_names_without_case() {
    let mut w = wt(CliKind::Claude);
    w.folder = Some("Bravo".into());
    assert!(only(&inputs(vec![project(w.clone())]), T0, WORKTREE_IDENTITY).is_none());
    w.folder = Some("bravo2".into());
    let f = only(&inputs(vec![project(w)]), T0, WORKTREE_IDENTITY).unwrap();
    assert_eq!(f.severity, Severity::Info);
    assert_eq!(
        f.message,
        "@bravo: its folder is named bravo2, not bravo; CLI verbs run there name themselves \
         after the folder"
    );
}

// ── pipeline_stalled ────────────────────────────────────────────────────────

fn pending(seen: bool, last_pass: Option<i64>) -> HealthInputs {
    let mut p = project(wt(CliKind::Claude));
    p.pipeline.oldest_pending = Some(PendingFacts {
        id: 12,
        created_at_ms: T0,
        seen,
    });
    p.pipeline.last_pass_ms = last_pass;
    inputs(vec![p])
}

#[test]
fn pipeline_stalled_fires_on_an_unseen_or_unpassed_request() {
    let i = pending(false, None);
    assert!(only(&i, T0 + 29 * S, PIPELINE_STALLED).is_none());
    let f = only(&i, T0 + 30 * S, PIPELINE_STALLED).expect("unseen at 30 s");
    assert_eq!(f.severity, Severity::Warn);
    assert_eq!(f.project, "proj");
    assert_eq!(f.worktree, "");
    assert_eq!(
        f.message,
        "proj: request 12 pending 30s with no pass since start"
    );
    assert_eq!(
        f.remedy,
        Some(Remedy::RequestPass {
            project: "proj".into()
        })
    );
    // Seen, but the last pass was 31 s ago.
    let i = pending(true, Some(T0 - S));
    let f = only(&i, T0 + 30 * S, PIPELINE_STALLED).expect("no pass for 31 s");
    assert!(f
        .message
        .contains(&format!("no pass since {}", clock_text(T0 - S))));
    let f = only(&i, T0 + 120 * S, PIPELINE_STALLED).unwrap();
    assert_eq!(f.severity, Severity::Error);
    // Seen and passed recently: a request that legitimately waits.
    let i = pending(true, Some(T0 + 20 * S));
    assert!(only(&i, T0 + 40 * S, PIPELINE_STALLED).is_none());
}

// ── watchers ────────────────────────────────────────────────────────────────

fn restarts(notes: Vec<RestartNote>) -> HealthInputs {
    let mut p = project(wt(CliKind::Claude));
    p.pipeline.restarts = notes;
    inputs(vec![p])
}

fn exited(at: i64) -> RestartNote {
    RestartNote {
        at_ms: at,
        watcher: WatcherKind::Bg,
        why: RestartWhy::Exited("notify error".into()),
    }
}

#[test]
fn watcher_restarts_warn_then_error() {
    let i = restarts(vec![exited(T0)]);
    let f = only(&i, T0 + S, WATCHER_RESTARTED).unwrap();
    assert_eq!(f.severity, Severity::Warn);
    assert_eq!(
        f.message,
        "proj: the background watcher exited (notify error) and was restarted (1x in 10m)"
    );
    assert_eq!(f.note, Some(RESTARTED_WATCHER));
    assert_eq!(f.remedy, None);
    let i = restarts(vec![
        exited(T0),
        exited(T0 + S),
        RestartNote {
            at_ms: T0 + 2 * S,
            watcher: WatcherKind::Tasks,
            why: RestartWhy::Stalled,
        },
    ]);
    let f = only(&i, T0 + 3 * S, WATCHER_RESTARTED).unwrap();
    assert_eq!(f.severity, Severity::Error);
    assert!(f.message.contains("the tasks watcher stalled"));
    assert!(f.message.ends_with("(3x in 10m)"));
}

#[test]
fn watcher_silent_names_the_missed_request() {
    let i = restarts(vec![RestartNote {
        at_ms: T0,
        watcher: WatcherKind::Bg,
        why: RestartWhy::Silent {
            request: 9,
            age_s: 16,
        },
    }]);
    let f = only(&i, T0 + S, WATCHER_SILENT).unwrap();
    assert_eq!(
        f.message,
        "proj: the watcher missed request 9 (no event for 16s); restarted, the sweep claimed it"
    );
    assert!(
        only(&i, T0 + S, WATCHER_RESTARTED).is_none(),
        "a separate id"
    );
}

// ── herdr_link / herdr_unknown / status_writer / version_skew ──────────────

fn link(l: HerdrLink, last: Option<i64>) -> HealthInputs {
    let mut i = inputs(Vec::new());
    i.link = Some(LinkFacts {
        link: l,
        since_ms: T0,
        last_snapshot_ms: last,
    });
    i
}

#[test]
fn herdr_link_warns_at_15s_and_errors_at_120s() {
    let i = link(HerdrLink::Recovering, Some(T0 - 5 * S));
    assert!(only(&i, T0 + 14 * S, HERDR_LINK).is_none());
    let f = only(&i, T0 + 15 * S, HERDR_LINK).unwrap();
    assert_eq!(f.severity, Severity::Warn);
    assert_eq!(
        f.message,
        "herdr unavailable for 15s (recovering); status is stale (last snapshot 20s ago)"
    );
    let f = only(&link(HerdrLink::Down, None), T0 + 120 * S, HERDR_LINK).unwrap();
    assert_eq!(f.severity, Severity::Error);
    assert_eq!(
        f.message,
        "herdr unavailable for 2m (down); status is stale (no snapshot yet)"
    );
    assert!(only(&link(HerdrLink::Up, Some(T0)), T0 + 600 * S, HERDR_LINK).is_none());
}

#[test]
fn herdr_link_down_silences_view_checks() {
    let mut i = present_with(None);
    i.link = Some(LinkFacts {
        link: HerdrLink::Recovering,
        since_ms: T0,
        last_snapshot_ms: Some(T0),
    });
    assert!(only(&i, T0 + 60 * S, HOOKS_VS_HERDR).is_none());
}

#[test]
fn herdr_unknown_fires_after_30s_and_clears_on_a_change() {
    let mut w = wt(CliKind::Claude);
    w.view = Some(view("present", "unknown"));
    w.key = Some(key_health(Some(T0), Some(T0)));
    w.hook = Some(hook("idle", Some(T0), None));
    let i = inputs(vec![project(w)]);
    assert!(only(&i, T0 + 30 * S, HERDR_UNKNOWN).is_none());
    let f = only(&i, T0 + 31 * S, HERDR_UNKNOWN).unwrap();
    assert_eq!(
        f.message,
        "@bravo: herdr reports the agent status as unknown for 31s; Keitora keeps the last \
         known state"
    );
    let mut i = i;
    i.projects[0].worktrees[0].view = Some(view("present", "idle"));
    assert!(only(&i, T0 + 31 * S, HERDR_UNKNOWN).is_none());
}

#[test]
fn status_writer_fires_for_a_standby_instance_only() {
    let mut i = inputs(Vec::new());
    i.lease = Some(LeaseState::Standby {
        pid: 4242,
        version: "0.5.0".into(),
        heartbeat_at: T0,
    });
    let f = only(&i, T0, STATUS_WRITER).unwrap();
    assert_eq!(
        f.message,
        "another Keitora instance (pid 4242, v0.5.0) owns status.db; this instance is read-only"
    );
    i.lease = Some(LeaseState::Owned);
    assert!(only(&i, T0, STATUS_WRITER).is_none());
}

#[test]
fn version_skew_older_warns_newer_informs_dev_ignored() {
    let mut i = inputs(Vec::new());
    let cli = |v: &str, dev: bool| PathCli {
        path: "C:/Users/x/AppData/Local/keitora/keitora.exe".into(),
        version: v.into(),
        dev_build: dev,
        bundled_version: Some("0.6.0".into()),
        managed_stale: false,
        refreshable: true,
    };
    i.path_cli = Some(cli("0.5.1", false));
    let f = only(&i, T0, VERSION_SKEW).unwrap();
    assert_eq!(f.severity, Severity::Warn);
    assert_eq!(
        f.message,
        "the keitora on PATH is older than the app's (0.5.1 vs 0.6.0)"
    );
    assert_eq!(f.remedy, Some(Remedy::RefreshCli));
    // An older keitora the app doesn't manage (or can't refresh here): reported only.
    let mut c = cli("0.5.1", false);
    c.refreshable = false;
    i.path_cli = Some(c);
    let f = only(&i, T0, VERSION_SKEW).unwrap();
    assert_eq!(f.severity, Severity::Warn);
    assert!(f.remedy.is_none());
    i.path_cli = Some(cli("v0.7.0", false));
    let f = only(&i, T0, VERSION_SKEW).unwrap();
    assert_eq!(f.severity, Severity::Info);
    assert!(f.message.contains("newer than"));
    assert!(f.remedy.is_none());
    i.path_cli = Some(cli("0.1.0", true));
    assert!(only(&i, T0, VERSION_SKEW).is_none(), "dev build");
    i.path_cli = Some(cli("0.6.0", false));
    assert!(only(&i, T0, VERSION_SKEW).is_none(), "versions match");
    // No bundled CLI (a dev app): nothing to compare.
    let mut c = cli("0.5.1", false);
    c.bundled_version = None;
    i.path_cli = Some(c.clone());
    assert!(only(&i, T0, VERSION_SKEW).is_none());
    // A stale managed copy warns whatever the versions say.
    c.managed_stale = true;
    i.path_cli = Some(c.clone());
    let f = only(&i, T0, VERSION_SKEW).unwrap();
    assert_eq!(
        f.message,
        "the keitora on PATH is an outdated copy of the app's CLI (C:/Users/x/AppData/Local/keitora/keitora.exe)"
    );
    assert_eq!(f.remedy, Some(Remedy::RefreshCli));
    // A platform that doesn't refresh the managed copy: reported only.
    c.refreshable = false;
    i.path_cli = Some(c);
    assert!(only(&i, T0, VERSION_SKEW).unwrap().remedy.is_none());
}

// ── unconfigured_sessions ───────────────────────────────────────────────────

#[test]
fn unconfigured_sessions_lists_five_then_counts() {
    let mut p = project(wt(CliKind::Claude));
    p.hook_rows = vec![("bravo".into(), Some(T0)), ("zulu".into(), Some(T0))];
    let f = only(&inputs(vec![p.clone()]), T0, UNCONFIGURED_SESSIONS).unwrap();
    assert_eq!(f.severity, Severity::Info);
    assert_eq!(
        f.message,
        "proj: session rows exist for unconfigured worktrees: zulu"
    );
    p.hook_rows = ["a", "b", "c", "d", "e", "f"]
        .iter()
        .map(|n| (n.to_string(), Some(T0)))
        .collect();
    let f = only(&inputs(vec![p.clone()]), T0, UNCONFIGURED_SESSIONS).unwrap();
    assert_eq!(
        f.message,
        "proj: session rows exist for unconfigured worktrees: a, b, c, d, e, +1 more"
    );
    // Rows older than a day, or gone: nothing.
    assert!(only(
        &inputs(vec![p.clone()]),
        T0 + 25 * 3600 * S,
        UNCONFIGURED_SESSIONS
    )
    .is_none());
    p.hook_rows.clear();
    assert!(only(&inputs(vec![p]), T0, UNCONFIGURED_SESSIONS).is_none());
}

// ── Reconcile ───────────────────────────────────────────────────────────────

fn finding(check: &'static str, msg: &str) -> Finding {
    Finding {
        project: "proj".into(),
        worktree: "bravo".into(),
        check_id: check,
        severity: Severity::Warn,
        message: msg.into(),
        detail: json!({}),
        remedy: None,
        note: None,
    }
}

fn upserts(out: &reconcile::Outcome) -> Vec<&keitora_tasks::FindingRow> {
    out.writes
        .iter()
        .filter_map(|w| match w {
            FindingWrite::Upsert(r) => Some(r),
            _ => None,
        })
        .collect()
}

#[test]
fn reconcile_inserts_once_and_writes_nothing_while_unchanged() {
    let mut r = Reconciler::new(false);
    let out = r.reconcile(vec![finding(HOOKS_MISSING, "m")], None, T0);
    let rows = upserts(&out);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].first_seen, rows[0].last_seen, rows[0].occurrences),
        (T0, T0, 1)
    );
    assert!(out.changed);
    let out = r.reconcile(vec![finding(HOOKS_MISSING, "m")], None, T0 + 30 * S);
    assert!(
        out.writes.is_empty() && !out.changed,
        "no write within 60 s"
    );
    let out = r.reconcile(
        vec![finding(HOOKS_MISSING, "m")],
        None,
        T0 + LAST_SEEN_FLUSH_MS,
    );
    assert_eq!(
        out.writes,
        vec![FindingWrite::LastSeen {
            key: FindingKey {
                project: "proj".into(),
                worktree: "bravo".into(),
                check_id: HOOKS_MISSING.into()
            },
            last_seen: T0 + LAST_SEEN_FLUSH_MS
        }]
    );
    assert!(!out.changed, "a last_seen move is not a change for the UI");
    assert_eq!(r.list()[0].last_seen, T0 + LAST_SEEN_FLUSH_MS);
}

#[test]
fn reconcile_updates_on_a_changed_message_or_severity() {
    let mut r = Reconciler::new(false);
    r.reconcile(vec![finding(HOOKS_MISSING, "m")], None, T0);
    let out = r.reconcile(vec![finding(HOOKS_MISSING, "m2")], None, T0 + S);
    let rows = upserts(&out);
    assert_eq!(rows[0].message, "m2");
    assert_eq!(rows[0].first_seen, T0, "same episode");
    let mut e = finding(HOOKS_MISSING, "m2");
    e.severity = Severity::Error;
    let out = r.reconcile(vec![e], None, T0 + 2 * S);
    assert_eq!(upserts(&out)[0].severity, "error");
}

#[test]
fn reconcile_writes_a_changed_detail() {
    let mut r = Reconciler::new(false);
    r.reconcile(vec![finding(HOOKS_MISSING, "m")], None, T0);
    let mut f = finding(HOOKS_MISSING, "m");
    f.detail = json!({ "path": "D:/repos/proj" });
    let out = r.reconcile(vec![f.clone()], None, T0 + S);
    let rows = upserts(&out);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].detail, f.detail.to_string());
    assert!(out.changed, "the window shows the detail");
    assert_eq!(r.list()[0].detail, f.detail);
    let out = r.reconcile(vec![f], None, T0 + 2 * S);
    assert!(out.writes.is_empty() && !out.changed, "then unchanged");
}

#[test]
fn reconcile_clears_after_two_misses_or_at_once_when_instant() {
    let mut r = Reconciler::new(false);
    r.reconcile(
        vec![finding(HOOKS_MISSING, "m"), finding(HERDR_UNKNOWN, "u")],
        None,
        T0,
    );
    let out = r.reconcile(Vec::new(), None, T0 + S);
    let rows = upserts(&out);
    assert_eq!(
        rows.len(),
        1,
        "only the instant one clears on the first miss"
    );
    assert_eq!(rows[0].check_id, HERDR_UNKNOWN);
    assert_eq!(rows[0].cleared_at, Some(T0 + S));
    // A finding back after one miss is the same episode.
    r.reconcile(vec![finding(HOOKS_MISSING, "m")], None, T0 + 2 * S);
    r.reconcile(Vec::new(), None, T0 + 3 * S);
    let out = r.reconcile(Vec::new(), None, T0 + 4 * S);
    let rows = upserts(&out);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].check_id.as_str(), rows[0].cleared_at),
        (HOOKS_MISSING, Some(T0 + 4 * S))
    );
    // Cleared ones stay listed.
    assert_eq!(r.list().len(), 2);
    assert!(r.list().iter().all(|f| f.cleared_at.is_some()));
}

#[test]
fn a_reopened_finding_counts_a_new_episode() {
    let mut r = Reconciler::new(false);
    r.reconcile(vec![finding(HERDR_UNKNOWN, "u")], None, T0);
    r.reconcile(Vec::new(), None, T0 + S);
    let out = r.reconcile(vec![finding(HERDR_UNKNOWN, "u")], None, T0 + 2 * S);
    let rows = upserts(&out);
    assert_eq!(rows[0].occurrences, 2);
    assert_eq!(rows[0].first_seen, T0 + 2 * S);
    assert_eq!(rows[0].cleared_at, None);
}

#[test]
fn cleared_findings_are_pruned_after_a_day() {
    let mut r = Reconciler::new(false);
    r.reconcile(vec![finding(HERDR_UNKNOWN, "u")], None, T0);
    r.reconcile(Vec::new(), None, T0 + S);
    let out = r.reconcile(Vec::new(), None, T0 + S + CLEARED_KEEP_MS - 1);
    assert!(out.writes.is_empty());
    let out = r.reconcile(Vec::new(), None, T0 + S + CLEARED_KEEP_MS);
    assert_eq!(
        out.writes,
        vec![FindingWrite::Delete(FindingKey {
            project: "proj".into(),
            worktree: "bravo".into(),
            check_id: HERDR_UNKNOWN.into()
        })]
    );
    assert!(r.list().is_empty());
}

#[test]
fn a_healthy_app_never_writes() {
    let mut r = Reconciler::new(true);
    for n in 0..100 {
        let out = r.reconcile(Vec::new(), None, T0 + n * 30 * S);
        assert_eq!(out, reconcile::Outcome::default());
    }
}

#[test]
fn a_scoped_evaluation_leaves_other_keys_alone() {
    let mut r = Reconciler::new(false);
    let mut other = finding(HERDR_UNKNOWN, "u");
    other.worktree = "alpha".into();
    r.reconcile(vec![finding(HERDR_UNKNOWN, "b"), other.clone()], None, T0);
    let scope: Scope = [("proj".to_string(), "bravo".to_string())].into();
    // alpha's finding is outside the scope: neither cleared nor taken from `desired`.
    let out = r.reconcile(vec![other], Some(&scope), T0 + S);
    let rows = upserts(&out);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].worktree, "bravo");
    assert!(r
        .list()
        .iter()
        .any(|f| f.worktree == "alpha" && f.cleared_at.is_none()));
}

fn remedied(check: &'static str) -> Finding {
    let mut f = finding(check, "m");
    f.remedy = Some(Remedy::ReinstallHooks {
        project: "proj".into(),
        worktree: "bravo".into(),
    });
    f
}

#[test]
fn remedies_run_once_per_key_per_ten_minutes_and_are_recorded() {
    let mut r = Reconciler::new(true);
    let out = r.reconcile(vec![remedied(HOOKS_MISSING)], None, T0);
    assert_eq!(out.remedies.len(), 1);
    let row = upserts(&out)[0].clone();
    assert_eq!(row.remedy, "reinstalled hooks");
    assert_eq!(row.remedy_at, Some(T0));
    // Still present: no rerun.
    let out = r.reconcile(vec![remedied(HOOKS_MISSING)], None, T0 + S);
    assert!(out.remedies.is_empty());
    // Cleared and re-opened inside ten minutes: no rerun, and the new episode
    // claims no remedy.
    r.reconcile(Vec::new(), None, T0 + 2 * S);
    r.reconcile(Vec::new(), None, T0 + 3 * S);
    let out = r.reconcile(vec![remedied(HOOKS_MISSING)], None, T0 + 4 * S);
    assert!(out.remedies.is_empty());
    assert_eq!(
        (upserts(&out)[0].remedy.as_str(), upserts(&out)[0].remedy_at),
        ("", None)
    );
    // Re-opened after ten minutes: runs again.
    r.reconcile(Vec::new(), None, T0 + 5 * S);
    r.reconcile(Vec::new(), None, T0 + 6 * S);
    let at = T0 + REMEDY_EVERY_MS;
    let out = r.reconcile(vec![remedied(HOOKS_MISSING)], None, at);
    assert_eq!(out.remedies.len(), 1);
    assert_eq!(upserts(&out)[0].remedy_at, Some(at));
    // The severity is the finding's own.
    assert_eq!(upserts(&out)[0].severity, "warn");
}

#[test]
fn remedies_never_run_with_global_writes_off() {
    let mut r = Reconciler::new(false);
    let out = r.reconcile(vec![remedied(HOOKS_MISSING)], None, T0);
    assert!(out.remedies.is_empty());
    assert_eq!(upserts(&out)[0].remedy, "");
    // An owner's note (already done) is recorded either way.
    let mut f = finding(WATCHER_RESTARTED, "w");
    f.note = Some(RESTARTED_WATCHER);
    let out = r.reconcile(vec![f], None, T0);
    assert!(out.remedies.is_empty());
    assert_eq!(upserts(&out)[0].remedy, "restarted watcher");
}

#[test]
fn a_reopened_finding_drops_the_last_episodes_note() {
    let mut r = Reconciler::new(false);
    let mut f = finding(WATCHER_RESTARTED, "w");
    f.note = Some(RESTARTED_WATCHER);
    r.reconcile(vec![f], None, T0);
    r.reconcile(Vec::new(), None, T0 + S);
    r.reconcile(Vec::new(), None, T0 + 2 * S);
    // Back without a note: nothing was done for this episode.
    let out = r.reconcile(vec![finding(WATCHER_RESTARTED, "w")], None, T0 + 3 * S);
    let row = upserts(&out)[0].clone();
    assert_eq!(row.occurrences, 2);
    assert_eq!((row.remedy.as_str(), row.remedy_at), ("", None));
}

#[test]
fn resync_clears_the_table_then_writes_every_kept_row() {
    let mut r = Reconciler::new(false);
    r.reconcile(
        vec![finding(HOOKS_MISSING, "m"), finding(HERDR_UNKNOWN, "u")],
        None,
        T0,
    );
    let w = r.resync();
    assert_eq!(w[0], FindingWrite::Clear);
    assert_eq!(w.len(), 3);
}

// ── Publisher: only the lease holder writes ─────────────────────────────────

fn publisher(
    path: &std::path::Path,
    id: &str,
    now: i64,
) -> Arc<crate::engine::writer::StatusPublisher> {
    crate::engine::writer::StatusPublisher::open(
        path.to_path_buf(),
        keitora_tasks::WriterIdentity {
            instance_id: id.into(),
            pid: 1,
            version: "0.6.0".into(),
            started_at: now,
        },
        now,
    )
}

#[test]
fn a_lease_loser_keeps_findings_in_memory_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("status.db");
    let holder = publisher(&path, "holder", T0);
    let loser = publisher(&path, "loser", T0);
    assert!(matches!(loser.lease(), LeaseState::Standby { .. }));
    let mut r = Reconciler::new(false);
    let out = r.reconcile(vec![finding(HOOKS_MISSING, "m")], None, T0);
    loser.findings_apply(&out.writes);
    let reader = keitora_tasks::StatusReader::open(&path).unwrap().unwrap();
    assert!(reader.findings("proj", None).unwrap().is_empty());
    assert_eq!(r.list().len(), 1, "kept in memory");
    // The holder writes the same outcome.
    holder.findings_apply(&r.resync());
    let rows = reader.findings("proj", None).unwrap();
    assert_eq!(rows, r.rows());
}
