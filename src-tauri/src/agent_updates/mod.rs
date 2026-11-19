//! Agent CLI updates: knows each CLI's installed and latest version and how it was
//! installed, checks on app start (once session recovery settles) and every
//! `[updates] agent_check_hours`, and runs an update in Keitora's reserved pane, watching
//! it to completion through the pane's history.
//!
//! [`AgentUpdates`] holds the state and every decision over a [`Prober`] and an
//! [`EmitSink`], so it tests without an `AppHandle`. The Tauri commands are in
//! [`commands`]; the pure rules are in [`model`]; machine reads are in [`probe`].

pub mod commands;
pub mod model;
pub mod probe;

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use keitora_tasks::{CliKind, UpdatesConfig, UserConfig};
use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::activity::lock;
use crate::backend::herdr::recovery::Phase;
use crate::backend::{EmitSink, SessionKey, SharedBackend};
use model::{AgentUpdateState, InstallKind, RunOutcome};
use probe::{MachineProber, Prober};

pub const STATUS_EVENT: &str = "agent-updates://status";
pub const RUN_EVENT: &str = "agent-updates://run";
pub const STOP_EVENT: &str = "agent-updates://stop";
pub const RESTART_EVENT: &str = "agent-updates://restart";

/// Grace after launch before the first check starts waiting on recovery, so a boot
/// recovery that has not begun yet is not mistaken for a settled one.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10);
const SETTLE_POLL: Duration = Duration::from_secs(2);
const SETTLE_CAP: Duration = Duration::from_secs(3 * 60);
/// How often the schedule re-reads the interval.
const SCHEDULE_TICK: Duration = Duration::from_secs(60);
/// A non-forced check this soon after the last one answers with its result instead.
const CHECK_COALESCE: Duration = Duration::from_secs(30);
/// How long a run is watched before it is given up on.
const WATCH_MAX: Duration = Duration::from_secs(15 * 60);
/// History lines read per watch poll.
const WATCH_LINES: u32 = 50;

/// A session by name, as the frontend keys its panes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct UpdateSession {
    pub project: String,
    pub worktree: String,
}

impl From<&SessionKey> for UpdateSession {
    fn from(k: &SessionKey) -> Self {
        UpdateSession {
            project: k.project.clone(),
            worktree: k.worktree.clone(),
        }
    }
}

/// A session stopped so its CLI could be updated, kept until it is restarted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoppedSession {
    #[serde(flatten)]
    pub session: UpdateSession,
    pub cli: CliKind,
}

/// One CLI's update picture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentUpdateEntry {
    pub cli: CliKind,
    pub state: AgentUpdateState,
    pub installed: Option<String>,
    pub latest: Option<String>,
    /// The executable checked (`None` when not found).
    pub exe: Option<String>,
    pub install_kind: Option<InstallKind>,
    /// What Keitora runs to update; `None` means there is no Update button.
    pub command: Option<String>,
    /// The command to show the user (the run command, or a suggestion for their own
    /// terminal when there is none).
    pub guidance: Option<String>,
}

impl AgentUpdateEntry {
    fn unchecked(cli: CliKind) -> Self {
        AgentUpdateEntry {
            cli,
            state: AgentUpdateState::Unchecked,
            installed: None,
            latest: None,
            exe: None,
            install_kind: None,
            command: None,
            guidance: None,
        }
    }

    fn reassess(&mut self, skipped: Option<&str>) {
        if self.state == AgentUpdateState::Unchecked {
            return;
        }
        self.state = model::assess(
            self.exe.is_some(),
            self.installed.as_deref(),
            self.latest.as_deref(),
            skipped,
            self.command.clone(),
        );
    }
}

/// The `agent_updates_status` reply and the `agent-updates://status` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentUpdatesSnapshot {
    pub codex: AgentUpdateEntry,
    pub claude: AgentUpdateEntry,
    /// RFC 3339 time of the last completed check.
    pub checked_at: Option<String>,
    pub stopped: Vec<StoppedSession>,
    /// The CLI whose update is running in the reserved pane, if any.
    pub running: Option<CliKind>,
}

impl Default for AgentUpdatesSnapshot {
    fn default() -> Self {
        AgentUpdatesSnapshot {
            codex: AgentUpdateEntry::unchecked(CliKind::Codex),
            claude: AgentUpdateEntry::unchecked(CliKind::Claude),
            checked_at: None,
            stopped: Vec::new(),
            running: None,
        }
    }
}

impl AgentUpdatesSnapshot {
    fn entry_mut(&mut self, cli: CliKind) -> &mut AgentUpdateEntry {
        match cli {
            CliKind::Codex => &mut self.codex,
            CliKind::Claude => &mut self.claude,
        }
    }

    fn entry(&self, cli: CliKind) -> &AgentUpdateEntry {
        match cli {
            CliKind::Codex => &self.codex,
            CliKind::Claude => &self.claude,
        }
    }
}

/// `agent_update_run` reply: the shell line to type into the reserved pane and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunPlan {
    pub command: String,
    pub cwd: String,
}

/// A started run, as its watcher needs it.
#[derive(Debug, Clone)]
pub struct RunTicket {
    pub cli: CliKind,
    gen: u64,
    exe: Option<String>,
    before: Option<String>,
    pub plan: RunPlan,
}

/// `agent-updates://run` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunEvent {
    pub cli: CliKind,
    pub outcome: RunOutcome,
}

/// `agent-updates://stop` and `agent-updates://restart` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionsEvent {
    pub cli: CliKind,
    pub keys: Vec<UpdateSession>,
}

/// How a watch ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEnd {
    Done(RunOutcome),
    TimedOut,
    /// A newer run started or the pane was closed.
    Cancelled,
}

/// The version of `cli` the user skipped, from `cfg`.
fn skipped_version(cfg: &UserConfig, cli: CliKind) -> Option<&str> {
    match cli {
        CliKind::Codex => cfg.updates.agent_skip_codex.as_deref(),
        CliKind::Claude => cfg.updates.agent_skip_claude.as_deref(),
    }
}

/// Managed state: the snapshot plus the seams it is computed through.
pub struct AgentUpdates {
    prober: Box<dyn Prober>,
    sink: Arc<dyn EmitSink>,
    /// The user's home directory: where update runs start.
    home: Option<String>,
    state: Mutex<AgentUpdatesSnapshot>,
    /// Serialises checks so two never probe at once; holds when the last one completed.
    check_lock: Mutex<Option<Instant>>,
    /// Bumped by every run start and close; a watcher whose run is no longer current stops.
    run_gen: Mutex<u64>,
    /// Signalled whenever `run_gen` changes, so a waiting watcher stops at once.
    run_changed: Condvar,
}

impl AgentUpdates {
    pub fn new(prober: Box<dyn Prober>, sink: Arc<dyn EmitSink>, home: Option<String>) -> Self {
        AgentUpdates {
            prober,
            sink,
            home,
            state: Mutex::new(AgentUpdatesSnapshot::default()),
            check_lock: Mutex::new(None),
            run_gen: Mutex::new(0),
            run_changed: Condvar::new(),
        }
    }

    pub fn snapshot(&self) -> AgentUpdatesSnapshot {
        lock(&self.state).clone()
    }

    fn emit<T: Serialize>(&self, event: &str, payload: &T) {
        match serde_json::to_value(payload) {
            Ok(v) => self.sink.emit(event, v),
            Err(e) => log::warn!("[agent-updates] couldn't serialize {event}: {e}"),
        }
    }

    /// Change the snapshot, then emit it.
    fn update(&self, f: impl FnOnce(&mut AgentUpdatesSnapshot)) -> AgentUpdatesSnapshot {
        let snap = {
            let mut state = lock(&self.state);
            f(&mut state);
            state.clone()
        };
        self.emit(STATUS_EVENT, &snap);
        snap
    }

    /// Probe one CLI from scratch.
    fn probe_entry(&self, cli: CliKind, cfg: &UserConfig) -> AgentUpdateEntry {
        let Some(exe) = self.prober.exe(cli, cfg) else {
            return AgentUpdateEntry {
                state: AgentUpdateState::NotInstalled,
                ..AgentUpdateEntry::unchecked(cli)
            };
        };
        let installed = self.prober.installed(cli, &exe);
        let resolved = self.prober.resolve(&exe);
        let kind = model::install_kind(cli, &exe, &resolved, self.home.as_deref());
        // An npm install updates through its own npm when one sits beside it.
        let npm = (kind == InstallKind::Npm)
            .then(|| {
                model::npm_candidates(&exe, &resolved, cfg!(windows))
                    .into_iter()
                    .find(|p| self.prober.is_file(p))
            })
            .flatten();
        let cmds = model::update_commands(cli, kind, npm.as_deref());
        let latest = self.prober.latest(cli);
        let state = model::assess(
            true,
            installed.as_deref(),
            latest.as_deref(),
            skipped_version(cfg, cli),
            cmds.run.clone(),
        );
        AgentUpdateEntry {
            cli,
            state,
            installed,
            latest,
            exe: Some(exe),
            install_kind: Some(kind),
            command: cmds.run,
            guidance: Some(cmds.guidance),
        }
    }

    /// Check both CLIs (concurrently), store and emit the result. Unless `force`, a check
    /// within [`CHECK_COALESCE`] of the last completed one returns its snapshot unprobed.
    pub fn check(&self, force: bool) -> AgentUpdatesSnapshot {
        let mut last = lock(&self.check_lock);
        if !force && last.is_some_and(|at| at.elapsed() < CHECK_COALESCE) {
            return self.snapshot();
        }
        let cfg = self.prober.config();
        let (codex, claude) = thread::scope(|s| {
            let codex = s.spawn(|| self.probe_entry(CliKind::Codex, &cfg));
            let claude = self.probe_entry(CliKind::Claude, &cfg);
            let codex = codex
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            (codex, claude)
        });
        log::info!(
            "[agent-updates] checked: codex {:?}, claude {:?}",
            codex.state,
            claude.state
        );
        let snap = self.update(|s| {
            s.codex = codex;
            s.claude = claude;
            s.checked_at = Some(chrono::Utc::now().to_rfc3339());
        });
        *last = Some(Instant::now());
        snap
    }

    /// Re-assess `cli` after its skipped version changed.
    pub fn skip_changed(&self, cli: CliKind) -> AgentUpdatesSnapshot {
        let cfg = self.prober.config();
        let skipped = skipped_version(&cfg, cli);
        self.update(|s| s.entry_mut(cli).reassess(skipped))
    }

    /// Bump the run generation (superseding any watcher) and wake waiting watchers.
    fn bump_run_gen(&self) -> u64 {
        let gen = {
            let mut gen = lock(&self.run_gen);
            *gen += 1;
            *gen
        };
        self.run_changed.notify_all();
        gen
    }

    /// Start a run of `cli`'s update command. Refused when there is no command to run or
    /// no home directory to run it in. Supersedes any earlier run's watcher.
    pub fn begin_run(&self, cli: CliKind) -> Result<RunTicket, String> {
        let entry = self.snapshot().entry(cli).clone();
        let Some(command) = entry.command.clone() else {
            return Err(format!(
                "Keitora can't update {} here; run the suggested command in your own terminal",
                model::cli_title(cli)
            ));
        };
        let cwd = self
            .home
            .clone()
            .ok_or_else(|| "could not resolve the home directory".to_string())?;
        let gen = self.bump_run_gen();
        self.update(|s| s.running = Some(cli));
        Ok(RunTicket {
            cli,
            gen,
            exe: entry.exe,
            before: entry.installed,
            plan: RunPlan {
                command: model::build_run_command(&command),
                cwd,
            },
        })
    }

    fn is_current(&self, ticket: &RunTicket) -> bool {
        *lock(&self.run_gen) == ticket.gen
    }

    /// Wait up to `timeout`, returning early once `ticket`'s run is superseded or closed.
    pub fn wait_unless_superseded(&self, ticket: &RunTicket, timeout: Duration) {
        let gen = lock(&self.run_gen);
        let _ = self
            .run_changed
            .wait_timeout_while(gen, timeout, |g| *g == ticket.gen)
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// Poll `read` (the reserved pane's recent history; `None` while it cannot be read)
    /// until the sentinel shows, the run is superseded, or `max_wait` of waits pass,
    /// calling `wait` with each [`model::watch_delay`] between polls. A completed run is
    /// finished here.
    pub fn watch(
        &self,
        ticket: &RunTicket,
        mut read: impl FnMut() -> Option<String>,
        mut wait: impl FnMut(Duration),
        max_wait: Duration,
    ) -> WatchEnd {
        let mut waited = Duration::ZERO;
        let mut poll = 0;
        while waited < max_wait {
            if !self.is_current(ticket) {
                return WatchEnd::Cancelled;
            }
            if let Some(history) = read() {
                if let Some(code) = model::parse_sentinel(&history) {
                    return WatchEnd::Done(self.finish_run(ticket, code, &history));
                }
            }
            let delay = model::watch_delay(poll);
            wait(delay);
            waited += delay;
            poll += 1;
        }
        if !self.is_current(ticket) {
            return WatchEnd::Cancelled;
        }
        log::warn!(
            "[agent-updates] {:?} update run never reported back",
            ticket.cli
        );
        self.emit(
            RUN_EVENT,
            &RunEvent {
                cli: ticket.cli,
                outcome: RunOutcome::TimedOut,
            },
        );
        self.update(|s| s.running = None);
        WatchEnd::TimedOut
    }

    /// The run exited with `code`: re-probe the version, report the outcome, re-assess.
    fn finish_run(&self, ticket: &RunTicket, code: i32, history: &str) -> RunOutcome {
        let cli = ticket.cli;
        let after = ticket.exe.as_deref().and_then(|exe| {
            self.prober.invalidate(cli, exe);
            self.prober.installed(cli, exe)
        });
        let outcome = model::run_outcome(
            cli,
            code,
            ticket.before.as_deref(),
            after.as_deref(),
            history,
        );
        log::info!("[agent-updates] {cli:?} update run: {outcome:?}");
        self.emit(
            RUN_EVENT,
            &RunEvent {
                cli,
                outcome: outcome.clone(),
            },
        );
        let cfg = self.prober.config();
        let skipped = skipped_version(&cfg, cli);
        self.update(|s| {
            let entry = s.entry_mut(cli);
            if after.is_some() {
                entry.installed = after.clone();
            }
            entry.reassess(skipped);
            s.running = None;
        });
        outcome
    }

    /// The update pane is closing: stop watching it.
    pub fn close(&self) {
        self.bump_run_gen();
        if self.snapshot().running.is_some() {
            self.update(|s| s.running = None);
        }
    }

    /// Record `keys` as stopped for `cli`'s update and tell their panes. Call before
    /// killing them, so a pane knows why it ended when the exit arrives.
    pub fn stop(&self, cli: CliKind, keys: &[SessionKey]) {
        let keys: Vec<UpdateSession> = keys.iter().map(UpdateSession::from).collect();
        self.emit(
            STOP_EVENT,
            &SessionsEvent {
                cli,
                keys: keys.clone(),
            },
        );
        self.update(|s| {
            for session in keys {
                let stopped = StoppedSession { session, cli };
                if !s.stopped.contains(&stopped) {
                    s.stopped.push(stopped);
                }
            }
        });
    }

    /// Ask the frontend to restart `cli`'s sessions: those stopped for its update plus
    /// `live` (its running sessions). Clears the stopped list for `cli`. Returns the keys.
    pub fn restart(&self, cli: CliKind, live: &[SessionKey]) -> Vec<UpdateSession> {
        let mut keys: Vec<UpdateSession> = Vec::new();
        self.update(|s| {
            let (mine, others): (Vec<StoppedSession>, Vec<StoppedSession>) =
                std::mem::take(&mut s.stopped)
                    .into_iter()
                    .partition(|st| st.cli == cli);
            s.stopped = others;
            keys = mine.into_iter().map(|st| st.session).collect();
        });
        for k in live.iter().map(UpdateSession::from) {
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        self.emit(
            RESTART_EVENT,
            &SessionsEvent {
                cli,
                keys: keys.clone(),
            },
        );
        keys
    }
}

/// The live sessions whose agent is `cli` (never the reserved pane). The user config is
/// loaded once for every key.
pub fn live_sessions(backend: &SharedBackend, cli: CliKind) -> Result<Vec<SessionKey>, String> {
    let sessions = backend.list_sessions().map_err(|e| e.to_string())?;
    let user = UserConfig::load().ok();
    Ok(model::sessions_of(
        sessions.into_iter().map(|s| s.key),
        cli,
        |k| {
            let user = user.as_ref()?;
            crate::host::launch_for(user, k).map(|(_, launch)| launch.cli)
        },
    ))
}

/// Watch `ticket`'s run in the reserved pane on its own thread.
pub fn spawn_watcher(app: AppHandle, backend: SharedBackend, ticket: RunTicket) {
    let spawned = thread::Builder::new()
        .name("agent-update-watch".into())
        .spawn(move || {
            let key = SessionKey::agent_update();
            let updates = app.state::<AgentUpdates>();
            // Reads fail until the frontend has spawned the pane; they are retried.
            updates.watch(
                &ticket,
                || backend.read_history(&key, WATCH_LINES).ok(),
                |delay| updates.wait_unless_superseded(&ticket, delay),
                WATCH_MAX,
            );
        });
    if let Err(e) = spawned {
        log::warn!("[agent-updates] couldn't start the run watcher: {e}");
    }
}

/// Manage the state and start the check schedule. Called from the app's setup, after the
/// backend is managed.
pub fn startup(app: AppHandle) {
    let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned());
    app.manage(AgentUpdates::new(
        Box::new(MachineProber),
        Arc::new(app.clone()),
        home,
    ));
    spawn_schedule(app);
}

/// Whether session recovery has settled: healthy with nothing left to resume.
fn recovery_settled(app: &AppHandle) -> bool {
    match app.try_state::<SharedBackend>() {
        Some(backend) => {
            let snap = backend.recovery_snapshot();
            snap.phase == Phase::Healthy && snap.queue.is_empty()
        }
        None => true,
    }
}

/// Lives until the process exits; `AppHandle` has no weak form to stop on. Runs in debug
/// builds too: a check is a few cheap reads.
fn spawn_schedule(app: AppHandle) {
    let spawned = thread::Builder::new()
        .name("agent-updates-schedule".into())
        .spawn(move || {
            thread::sleep(FIRST_CHECK_DELAY);
            // Wait for recovery to settle, but check anyway once the cap passes.
            let waiting = Instant::now();
            while !recovery_settled(&app) && waiting.elapsed() < SETTLE_CAP {
                thread::sleep(SETTLE_POLL);
            }
            app.state::<AgentUpdates>().check(false);
            let mut last = Instant::now();
            loop {
                thread::sleep(SCHEDULE_TICK);
                let hours = UserConfig::load()
                    .map(|c| c.updates.agent_check_hours())
                    .unwrap_or(UpdatesConfig::DEFAULT_AGENT_CHECK_HOURS);
                if model::check_due(last.elapsed(), hours) {
                    app.state::<AgentUpdates>().check(false);
                    last = Instant::now();
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("[agent-updates] couldn't start the check schedule: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeProber {
        /// The skipped codex version the config reports.
        skip_codex: Arc<Mutex<Option<String>>>,
        config_calls: Arc<AtomicUsize>,
        exes: HashMap<CliKind, String>,
        /// Symlink targets; an exe not listed resolves to itself.
        links: HashMap<String, String>,
        /// The files that exist.
        files: Vec<String>,
        /// Installed version per CLI; replaced by `after_update` once invalidated.
        installed: Mutex<HashMap<CliKind, String>>,
        after_update: HashMap<CliKind, String>,
        latest: HashMap<CliKind, String>,
        invalidated: Mutex<Vec<(CliKind, String)>>,
        latest_calls: Arc<AtomicUsize>,
    }

    impl Prober for FakeProber {
        fn config(&self) -> UserConfig {
            self.config_calls.fetch_add(1, Ordering::SeqCst);
            let mut cfg = UserConfig::default();
            cfg.updates.agent_skip_codex = lock(&self.skip_codex).clone();
            cfg
        }
        fn exe(&self, cli: CliKind, _cfg: &UserConfig) -> Option<String> {
            self.exes.get(&cli).cloned()
        }
        fn resolve(&self, exe: &str) -> String {
            self.links
                .get(exe)
                .cloned()
                .unwrap_or_else(|| exe.to_string())
        }
        fn is_file(&self, path: &str) -> bool {
            self.files.iter().any(|f| f == path)
        }
        fn installed(&self, cli: CliKind, _exe: &str) -> Option<String> {
            lock(&self.installed).get(&cli).cloned()
        }
        fn invalidate(&self, cli: CliKind, exe: &str) {
            lock(&self.invalidated).push((cli, exe.to_string()));
            if let Some(v) = self.after_update.get(&cli) {
                lock(&self.installed).insert(cli, v.clone());
            }
        }
        fn latest(&self, cli: CliKind) -> Option<String> {
            self.latest_calls.fetch_add(1, Ordering::SeqCst);
            self.latest.get(&cli).cloned()
        }
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(String, serde_json::Value)>>);

    impl EmitSink for Recorder {
        fn emit(&self, event: &str, payload: serde_json::Value) {
            lock(&self.0).push((event.to_string(), payload));
        }
    }

    impl Recorder {
        fn named(&self, event: &str) -> Vec<serde_json::Value> {
            lock(&self.0)
                .iter()
                .filter(|(e, _)| e == event)
                .map(|(_, v)| v.clone())
                .collect()
        }
    }

    /// Codex via npm at 0.160.0 with 0.163.0 out; Claude native and current.
    fn prober() -> FakeProber {
        FakeProber {
            exes: HashMap::from([
                (
                    CliKind::Codex,
                    "/Users/me/.nvm/versions/node/v22.0.0/bin/codex".to_string(),
                ),
                (CliKind::Claude, "/Users/me/.local/bin/claude".to_string()),
            ]),
            installed: Mutex::new(HashMap::from([
                (CliKind::Codex, "0.160.0".to_string()),
                (CliKind::Claude, "2.1.328".to_string()),
            ])),
            after_update: HashMap::from([(CliKind::Codex, "0.163.0".to_string())]),
            latest: HashMap::from([
                (CliKind::Codex, "0.163.0".to_string()),
                (CliKind::Claude, "2.1.328".to_string()),
            ]),
            ..Default::default()
        }
    }

    fn updates_with(
        mut prober: FakeProber,
        skip: Arc<Mutex<Option<String>>>,
    ) -> (AgentUpdates, Arc<Recorder>) {
        prober.skip_codex = skip;
        let rec = Arc::new(Recorder::default());
        let updates = AgentUpdates::new(Box::new(prober), rec.clone(), Some("/Users/me".into()));
        (updates, rec)
    }

    fn key(p: &str, w: &str) -> SessionKey {
        SessionKey {
            project: p.into(),
            worktree: w.into(),
        }
    }

    #[test]
    fn starts_unchecked() {
        let (updates, rec) = updates_with(prober(), Arc::default());
        let snap = updates.snapshot();
        assert_eq!(snap.codex.state, AgentUpdateState::Unchecked);
        assert_eq!(snap.claude.state, AgentUpdateState::Unchecked);
        assert!(snap.checked_at.is_none());
        assert!(rec.named(STATUS_EVENT).is_empty());
    }

    #[test]
    fn check_assesses_both_clis_and_emits_status() {
        let p = prober();
        let config_calls = p.config_calls.clone();
        let (updates, rec) = updates_with(p, Arc::default());
        let snap = updates.check(true);
        assert_eq!(
            config_calls.load(Ordering::SeqCst),
            1,
            "one config load per check"
        );
        assert_eq!(
            snap.codex.state,
            AgentUpdateState::Available {
                installed: "0.160.0".into(),
                latest: "0.163.0".into(),
                command: Some("npm install -g @openai/codex@latest".into()),
            }
        );
        assert_eq!(snap.codex.install_kind, Some(InstallKind::Npm));
        assert_eq!(snap.claude.state, AgentUpdateState::UpToDate);
        assert_eq!(snap.claude.install_kind, Some(InstallKind::Native));
        assert_eq!(snap.claude.guidance.as_deref(), Some("claude update"));
        assert!(snap.checked_at.is_some());
        let statuses = rec.named(STATUS_EVENT);
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0]["codex"]["state"]["type"], "available");
        assert_eq!(statuses[0]["claude"]["cli"], "claude");
    }

    #[test]
    fn a_missing_cli_is_not_installed_and_never_fetched() {
        let mut p = prober();
        p.exes.remove(&CliKind::Claude);
        let calls = p.latest_calls.clone();
        let (updates, _rec) = updates_with(p, Arc::default());
        let snap = updates.check(true);
        assert_eq!(snap.claude.state, AgentUpdateState::NotInstalled);
        assert_eq!(snap.claude.exe, None);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "codex only");
    }

    #[test]
    fn unforced_checks_coalesce_until_one_completes_long_enough_ago() {
        let p = prober();
        let calls = p.latest_calls.clone();
        let (updates, rec) = updates_with(p, Arc::default());
        let first = updates.check(false);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "the first check probes");
        assert_eq!(updates.check(false), first, "a recent check answers");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            rec.named(STATUS_EVENT).len(),
            1,
            "a coalesced check emits nothing"
        );
        updates.check(true);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "a forced check always probes"
        );
        // Once the last check is older than the window, an unforced one probes again.
        *lock(&updates.check_lock) = Instant::now().checked_sub(CHECK_COALESCE);
        updates.check(false);
        assert_eq!(calls.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn skipping_hides_until_a_newer_release() {
        let skip: Arc<Mutex<Option<String>>> = Arc::default();
        let (updates, _rec) = updates_with(prober(), skip.clone());
        updates.check(true);
        *lock(&skip) = Some("0.163.0".into());
        let snap = updates.skip_changed(CliKind::Codex);
        assert_eq!(
            snap.codex.state,
            AgentUpdateState::Skipped {
                latest: "0.163.0".into()
            }
        );
        // A later check that finds a newer release shows it again.
        let mut p = prober();
        p.latest.insert(CliKind::Codex, "0.164.0".into());
        let (updates, _rec) = updates_with(p, skip);
        assert!(matches!(
            updates.check(true).codex.state,
            AgentUpdateState::Available { .. }
        ));
    }

    #[test]
    fn run_is_refused_without_a_command() {
        let mut p = prober();
        p.exes
            .insert(CliKind::Codex, "/Users/me/.codex/bin/codex".into());
        let (updates, rec) = updates_with(p, Arc::default());
        updates.check(true);
        let err = updates.begin_run(CliKind::Codex).unwrap_err();
        assert!(err.contains("your own terminal"), "{err}");
        assert_eq!(updates.snapshot().running, None);
        assert_eq!(rec.named(STATUS_EVENT).len(), 1, "no running status");
    }

    #[test]
    fn run_plan_carries_the_sentinel_and_home() {
        let (updates, _rec) = updates_with(prober(), Arc::default());
        updates.check(true);
        let ticket = updates.begin_run(CliKind::Codex).unwrap();
        assert_eq!(
            ticket.plan,
            RunPlan {
                command:
                    "npm install -g @openai/codex@latest; echo \"__KEITORA_AGENT_UPDATE_DONE:$?\""
                        .into(),
                cwd: "/Users/me".into(),
            }
        );
        assert_eq!(updates.snapshot().running, Some(CliKind::Codex));
    }

    /// The npm beside an npm install runs the update, by its quoted absolute path.
    #[test]
    fn an_npm_install_updates_through_its_own_npm() {
        let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
        let bin = "/Users/me/my node/bin";
        let mut p = prober();
        p.exes.insert(CliKind::Codex, format!("{bin}/codex"));
        p.links.insert(
            format!("{bin}/codex"),
            "/Users/me/my node/lib/node_modules/@openai/codex/bin/codex.js".into(),
        );
        p.files.push(format!("{bin}/{npm}"));
        let (updates, _rec) = updates_with(p, Arc::default());
        let snap = updates.check(true);
        assert_eq!(snap.codex.install_kind, Some(InstallKind::Npm));
        assert_eq!(
            snap.codex.command,
            Some(format!("'{bin}/{npm}' install -g @openai/codex@latest"))
        );
    }

    /// A brew launcher linking into node_modules is an npm install; with no npm found
    /// beside it the update runs the npm on PATH.
    #[test]
    fn a_linked_npm_install_without_its_npm_falls_back_to_path() {
        let mut p = prober();
        p.exes
            .insert(CliKind::Codex, "/opt/homebrew/bin/codex".into());
        p.links.insert(
            "/opt/homebrew/bin/codex".into(),
            "/opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js".into(),
        );
        let (updates, _rec) = updates_with(p, Arc::default());
        let snap = updates.check(true);
        assert_eq!(snap.codex.install_kind, Some(InstallKind::Npm));
        assert_eq!(
            snap.codex.command.as_deref(),
            Some("npm install -g @openai/codex@latest")
        );
    }

    #[test]
    fn watch_finishes_on_the_sentinel_and_reprobes() {
        let (updates, rec) = updates_with(prober(), Arc::default());
        updates.check(true);
        let ticket = updates.begin_run(CliKind::Codex).unwrap();
        // Not spawned yet, then the typed line, then the exit line.
        let mut reads = vec![
            None,
            Some(format!("$ {}", ticket.plan.command)),
            Some(format!(
                "$ {}\nadded 1 package\n__KEITORA_AGENT_UPDATE_DONE:0\n$ ",
                ticket.plan.command
            )),
        ]
        .into_iter();
        let mut sleeps = 0;
        let end = updates.watch(
            &ticket,
            || reads.next().flatten(),
            |_| sleeps += 1,
            Duration::from_secs(10),
        );
        let outcome = RunOutcome::Updated {
            from: Some("0.160.0".into()),
            to: "0.163.0".into(),
        };
        assert_eq!(end, WatchEnd::Done(outcome.clone()));
        assert_eq!(sleeps, 2);
        assert_eq!(
            rec.named(RUN_EVENT),
            vec![
                serde_json::json!({"cli": "codex", "outcome": {"type": "updated", "from": "0.160.0", "to": "0.163.0"}})
            ]
        );
        let snap = updates.snapshot();
        assert_eq!(snap.codex.state, AgentUpdateState::UpToDate);
        assert_eq!(snap.codex.installed.as_deref(), Some("0.163.0"));
        assert_eq!(snap.running, None);
    }

    #[test]
    fn a_failed_run_reports_its_hint_and_keeps_the_banner() {
        let mut p = prober();
        p.after_update.clear();
        let (updates, rec) = updates_with(p, Arc::default());
        updates.check(true);
        let ticket = updates.begin_run(CliKind::Codex).unwrap();
        let history = "npm error code EBUSY\n__KEITORA_AGENT_UPDATE_DONE:1".to_string();
        let end = updates.watch(
            &ticket,
            || Some(history.clone()),
            |_| {},
            Duration::from_secs(5),
        );
        assert_eq!(
            end,
            WatchEnd::Done(RunOutcome::Failed {
                code: 1,
                hint: Some("close running Codex sessions first".into())
            })
        );
        assert_eq!(rec.named(RUN_EVENT).len(), 1);
        assert!(matches!(
            updates.snapshot().codex.state,
            AgentUpdateState::Available { .. }
        ));
    }

    #[test]
    fn watch_times_out_and_close_cancels() {
        let (updates, rec) = updates_with(prober(), Arc::default());
        updates.check(true);
        let ticket = updates.begin_run(CliKind::Codex).unwrap();
        assert_eq!(
            updates.watch(
                &ticket,
                || Some("still going".into()),
                |_| {},
                Duration::from_secs(3)
            ),
            WatchEnd::TimedOut
        );
        assert_eq!(
            rec.named(RUN_EVENT),
            vec![serde_json::json!({"cli": "codex", "outcome": {"type": "timed_out"}})]
        );
        assert_eq!(updates.snapshot().running, None);

        let ticket = updates.begin_run(CliKind::Codex).unwrap();
        let mut polls = 0;
        let end = updates.watch(
            &ticket,
            || {
                polls += 1;
                if polls == 2 {
                    updates.close();
                }
                None
            },
            |_| {},
            Duration::from_secs(10),
        );
        assert_eq!(end, WatchEnd::Cancelled);
        assert_eq!(polls, 2);
        assert_eq!(updates.snapshot().running, None);
        assert_eq!(
            rec.named(RUN_EVENT).len(),
            1,
            "a cancelled run reports nothing"
        );
    }

    #[test]
    fn a_newer_run_supersedes_the_old_watcher() {
        let (updates, _rec) = updates_with(prober(), Arc::default());
        updates.check(true);
        let old = updates.begin_run(CliKind::Codex).unwrap();
        let _new = updates.begin_run(CliKind::Codex).unwrap();
        assert_eq!(
            updates.watch(
                &old,
                || Some("__KEITORA_AGENT_UPDATE_DONE:0".into()),
                |_| {},
                Duration::from_secs(5)
            ),
            WatchEnd::Cancelled
        );
    }

    #[test]
    fn stop_then_restart_round_trip() {
        let (updates, rec) = updates_with(prober(), Arc::default());
        updates.stop(
            CliKind::Codex,
            &[key("proj", "alpha"), key("proj", "bravo")],
        );
        // Stopping the same key again does not duplicate it.
        updates.stop(CliKind::Codex, &[key("proj", "alpha")]);
        updates.stop(CliKind::Claude, &[key("other-proj", "main")]);
        assert_eq!(updates.snapshot().stopped.len(), 3);
        assert_eq!(
            rec.named(STOP_EVENT)[0],
            serde_json::json!({"cli": "codex", "keys": [
                {"project": "proj", "worktree": "alpha"},
                {"project": "proj", "worktree": "bravo"}
            ]})
        );

        let keys = updates.restart(
            CliKind::Codex,
            &[key("proj", "bravo"), key("proj", "charlie")],
        );
        assert_eq!(
            keys,
            vec![
                UpdateSession::from(&key("proj", "alpha")),
                UpdateSession::from(&key("proj", "bravo")),
                UpdateSession::from(&key("proj", "charlie")),
            ]
        );
        assert_eq!(
            updates.snapshot().stopped,
            vec![StoppedSession {
                session: UpdateSession::from(&key("other-proj", "main")),
                cli: CliKind::Claude
            }],
            "only codex's stopped sessions are cleared"
        );
        assert_eq!(rec.named(RESTART_EVENT).len(), 1);
        assert_eq!(
            rec.named(RESTART_EVENT)[0]["keys"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn watch_waits_back_off_and_stop_at_the_bound() {
        let (updates, rec) = updates_with(prober(), Arc::default());
        updates.check(true);
        let ticket = updates.begin_run(CliKind::Codex).unwrap();
        let mut waits = Vec::new();
        let end = updates.watch(&ticket, || None, |d| waits.push(d), WATCH_MAX);
        assert_eq!(end, WatchEnd::TimedOut);
        let fast = Duration::from_millis(500);
        assert!(waits[..10].iter().all(|d| *d == fast), "{:?}", &waits[..10]);
        assert!(waits[10..].iter().all(|d| *d == Duration::from_secs(2)));
        let total: Duration = waits.iter().sum();
        assert!(
            total >= WATCH_MAX && total < WATCH_MAX + Duration::from_secs(2),
            "bounded at about 15 minutes: {total:?}"
        );
        assert_eq!(rec.named(RUN_EVENT).len(), 1);
    }

    #[test]
    fn close_and_a_newer_run_wake_a_waiting_watcher() {
        let (updates, _rec) = updates_with(prober(), Arc::default());
        updates.check(true);
        let long = Duration::from_secs(30);
        for supersede in [
            (|u: &AgentUpdates| u.close()) as fn(&AgentUpdates),
            |u: &AgentUpdates| {
                u.begin_run(CliKind::Codex).unwrap();
            },
        ] {
            let ticket = updates.begin_run(CliKind::Codex).unwrap();
            let started = Instant::now();
            thread::scope(|s| {
                let waiter = s.spawn(|| updates.wait_unless_superseded(&ticket, long));
                thread::sleep(Duration::from_millis(20));
                supersede(&updates);
                waiter.join().unwrap();
            });
            assert!(started.elapsed() < long / 2, "woken, not timed out");
            assert!(!updates.is_current(&ticket));
        }
    }

    #[test]
    fn a_current_run_waits_out_its_delay() {
        let (updates, _rec) = updates_with(prober(), Arc::default());
        updates.check(true);
        let ticket = updates.begin_run(CliKind::Codex).unwrap();
        let started = Instant::now();
        updates.wait_unless_superseded(&ticket, Duration::from_millis(30));
        assert!(started.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn stopped_session_wire_shape_is_flat() {
        assert_eq!(
            serde_json::to_value(StoppedSession {
                session: UpdateSession::from(&key("proj", "alpha")),
                cli: CliKind::Codex,
            })
            .unwrap(),
            serde_json::json!({"project": "proj", "worktree": "alpha", "cli": "codex"})
        );
    }
}
