//! `fake` — a scripted, in-memory [`HerdrOps`] for tests.
//!
//! [`FakeHerdr`] models just enough of a herdr server for backend tests: panes with
//! ids / cwd / agent / recorded agent session, a server that can be killed and
//! respawned (optionally restoring its panes as bare shells with new ids, as herdr
//! does with agent restore off), scripted `agent start` outcomes, per-op failures
//! and hangs, and in-memory bridge pipes. It never sleeps and never spawns a
//! process. Every [`HerdrOps`] call (except `env`) is recorded as a [`Call`] before
//! the scripts are consulted, so tests can assert call order.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

use super::cli::{
    AgentInfo, AgentSession, AgentSessionKind, IntegrationStatus, PaneIdentity, PaneInfo,
    PaneProcessInfo, ServerStatus,
};
use super::env::{ExeDiscovery, HerdrEnv};
use super::ops::{
    BridgeChild, BridgeProcess, ExitInfo, HerdrError, HerdrOps, Op, ServerInstance, ServerProcess,
};

/// How the next [`HerdrOps::spawn_server`] restores the panes of the previous
/// server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreMode {
    /// Agent restore off (the config Keitora writes): every pane comes back as a
    /// bare shell with a new id; the recorded agent session is kept.
    Shells,
    /// Legacy: panes come back with new ids and their agent still running.
    Agents,
}

/// Scripted outcome of one `agent start`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentStartScript {
    /// The agent launches.
    Ok,
    /// The agent exited during startup (`agent_start_failed`).
    EarlyExit,
    /// herdr lost the agent name (`agent_name_not_found`).
    NameLost,
    /// Launched but not ready yet (folded to `Ok`, as the real client does).
    NotReady,
    /// Fail with this error.
    Err(HerdrError),
}

/// One recorded call: the operation and a readable rendering of its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub op: Op,
    pub args: String,
}

/// The stdout sender of one in-memory bridge. Shared between the bridge child
/// (which drops it on kill, ending the stream) and the fake (which pushes frames).
type FrameTx = Arc<Mutex<Option<Sender<Vec<u8>>>>>;

struct FakeState {
    calls: Vec<Call>,
    running: bool,
    /// What `server_instance` reports while running.
    instance: Option<ServerInstance>,
    /// The last instance identity handed out; the next spawn derives from it.
    basis: ServerInstance,
    panes: Vec<PaneIdentity>,
    closed: Vec<String>,
    /// What `pane_process_info` answers, by pane id; a pane without an entry is
    /// not found.
    process_info: HashMap<String, PaneProcessInfo>,
    /// What `pane_read` answers, by pane id; a pane without an entry reads empty.
    pane_text: HashMap<String, String>,
    next_workspace: u32,
    next_terminal: u32,
    next_session: u32,
    restore_mode: RestoreMode,
    /// Empty `pane_list` answers to give after each restart.
    restore_delay: u32,
    /// Remaining empty `pane_list` answers for the current restart.
    restore_delay_left: u32,
    /// After each restart, `pane_list` shows the restored panes one more per
    /// answer instead of all at once.
    restore_gradual: bool,
    /// Restored panes `pane_list` does not show yet, in reveal order.
    unrevealed: VecDeque<String>,
    fail_next: HashMap<Op, VecDeque<HerdrError>>,
    hung: HashSet<Op>,
    fail_spawn_server: Option<std::io::ErrorKind>,
    /// The herdr executable does not exist: every call fails to spawn and a
    /// rediscovery finds nothing.
    exe_missing: bool,
    /// How many times `rediscover` ran.
    rediscoveries: u32,
    agent_scripts: VecDeque<AgentStartScript>,
    /// Exit of each server process by pid, once it has exited.
    exits: HashMap<u32, ExitInfo>,
    /// Server processes whose exit was collected (`wait`, or `try_wait`
    /// returning the exit).
    reaped: HashSet<u32>,
    /// Latest bridge stdout sender per terminal id.
    bridges: HashMap<String, FrameTx>,
    /// Bytes written to each terminal's bridge stdin.
    bridge_input: HashMap<String, Vec<u8>>,
    /// Terminals whose bridge stdin fails every write (a dead control child).
    failing_input: HashSet<String>,
    /// Terminals whose bridge stdin blocks every write until that bridge's
    /// child is killed (a hung control child whose pipe is full).
    blocked_input: HashSet<String>,
    /// How many more `pane_list` answers a closed pane keeps appearing in.
    close_linger: u32,
    /// Closed panes still listed, with their remaining `pane_list` answers.
    lingering: Vec<(PaneIdentity, u32)>,
    /// Run right after a call of the op is recorded (under the fake's lock, so
    /// a hook must not call the fake).
    hooks: HashMap<Op, CallHook>,
    /// Run when a bridge child is killed (outside the fake's lock).
    bridge_kill_hook: Option<CallHook>,
}

/// A test callback attached to one op.
pub type CallHook = Arc<dyn Fn() + Send + Sync>;

struct Shared {
    state: Mutex<FakeState>,
    /// Signalled whenever a server process exits.
    exited: Condvar,
    /// Signalled when a bridge child is killed or input is unblocked.
    input: Condvar,
}

/// The scripted herdr double. See the module docs.
pub struct FakeHerdr {
    env: HerdrEnv,
    shared: Arc<Shared>,
    /// Owns the temp home (keymap etc.) for the fake's lifetime.
    _home: tempfile::TempDir,
}

const INITIAL_INSTANCE: ServerInstance = ServerInstance {
    pid: 1000,
    start_time: 1_000_000,
};

fn api(code: &str, message: &str) -> HerdrError {
    HerdrError::Api {
        code: code.into(),
        message: message.into(),
    }
}

fn not_running() -> HerdrError {
    HerdrError::Exit {
        code: Some(1),
        stderr: "server not running".into(),
    }
}

/// What spawning a herdr executable that does not exist fails with.
pub fn exe_missing_error() -> HerdrError {
    HerdrError::SpawnFailed {
        os_error: Some(2),
        message: "No such file or directory (os error 2)".into(),
    }
}

/// Where a [`FakeHerdr`] with a missing executable says it looked.
pub const FAKE_SEARCHED: [&str; 2] = ["/fake/bin/herdr", "/fake/other/herdr"];

fn pane_not_found(pane_id: &str) -> HerdrError {
    api("pane_not_found", &format!("pane {pane_id} not found"))
}

/// The session id an agent launched with `args` resumes: the value after
/// `--resume` (claude) or `resume` (codex), unless it is itself a flag.
fn resume_id_from(args: &[String]) -> Option<String> {
    args.windows(2)
        .find(|w| (w[0] == "--resume" || w[0] == "resume") && !w[1].starts_with('-'))
        .map(|w| w[1].clone())
}

impl FakeState {
    fn record(&mut self, op: Op, args: String) {
        self.calls.push(Call { op, args });
        if let Some(h) = self.hooks.get(&op) {
            h();
        }
    }

    /// The scripted hang / failure for `op`, if any (a missing executable wins,
    /// then hangs; failures are FIFO).
    fn scripted(&mut self, op: Op) -> Option<HerdrError> {
        if self.exe_missing {
            return Some(exe_missing_error());
        }
        if self.hung.contains(&op) {
            return Some(HerdrError::Timeout);
        }
        self.fail_next.get_mut(&op).and_then(VecDeque::pop_front)
    }

    /// Record `op` then apply scripts and the server-down rule for a control call.
    fn control(&mut self, op: Op, args: String) -> Result<(), HerdrError> {
        self.record(op, args);
        if let Some(e) = self.scripted(op) {
            return Err(e);
        }
        if !self.running {
            return Err(not_running());
        }
        Ok(())
    }

    fn pane_mut(&mut self, pane_id: &str) -> Result<&mut PaneIdentity, HerdrError> {
        self.panes
            .iter_mut()
            .find(|p| p.pane_id == pane_id)
            .ok_or_else(|| pane_not_found(pane_id))
    }

    fn new_pane(&mut self, cwd: &str) -> PaneIdentity {
        self.next_workspace += 1;
        self.next_terminal += 1;
        let ws = format!("w{}", self.next_workspace);
        let pane = PaneIdentity {
            pane_id: format!("{ws}:p1"),
            workspace_id: ws,
            terminal_id: format!("term_{}", self.next_terminal),
            cwd: Some(cwd.to_string()),
            ..Default::default()
        };
        self.panes.push(pane.clone());
        pane
    }

    /// Drop every bridge's stdout sender (their readers see EOF).
    fn end_bridges(&mut self) {
        for tx in self.bridges.values() {
            *tx.lock().unwrap() = None;
        }
    }

    /// The current server process exits with `code`.
    fn kill(&mut self, code: Option<i32>) {
        self.running = false;
        if let Some(i) = self.instance {
            self.exits
                .entry(i.pid)
                .or_insert(ExitInfo { code, signal: None });
        }
        self.end_bridges();
    }

    fn next_instance(&mut self) -> ServerInstance {
        let i = ServerInstance {
            pid: self.basis.pid + 1,
            start_time: self.basis.start_time + 10,
        };
        self.basis = i;
        self.instance = Some(i);
        i
    }

    /// Bring a new server instance up, restoring the previous panes per the
    /// restore mode with renumbered ids.
    fn restart(&mut self) -> ServerInstance {
        let i = self.next_instance();
        self.running = true;
        let old = std::mem::take(&mut self.panes);
        self.next_workspace = 0;
        for p in old {
            let mut restored = self.new_pane(p.cwd.as_deref().unwrap_or_default());
            restored.cwd = p.cwd;
            restored.agent_session = p.agent_session;
            if self.restore_mode == RestoreMode::Agents {
                restored.agent = p.agent;
                restored.agent_status = p.agent_status;
            }
            *self.panes.last_mut().unwrap() = restored;
        }
        self.restore_delay_left = self.restore_delay;
        if self.restore_gradual {
            self.unrevealed = self
                .panes
                .iter()
                .skip(1)
                .map(|p| p.pane_id.clone())
                .collect();
        }
        i
    }
}

impl FakeHerdr {
    /// A running server (instance pid 1000, start time 1 000 000) with no panes.
    pub fn new() -> Arc<Self> {
        let home = tempfile::tempdir().expect("fake herdr home");
        let env = HerdrEnv {
            exe: "herdr-fake".into(),
            discovery: super::env::ExeDiscovery::explicit("herdr-fake".into()),
            env: Vec::new(),
            // The `PATH` a unix HerdrEnv carries, so pane env tests can see it.
            path_source: if cfg!(unix) {
                super::env::PathSource::Fixed("/fake/bin:/usr/bin".to_string())
            } else {
                super::env::PathSource::None
            },
            home: home.path().to_path_buf(),
            original_xdg: super::env::OriginalXdg::default(),
            env_remove: Vec::new(),
        };
        Arc::new(FakeHerdr {
            env,
            shared: Arc::new(Shared {
                state: Mutex::new(FakeState {
                    calls: Vec::new(),
                    running: true,
                    instance: Some(INITIAL_INSTANCE),
                    basis: INITIAL_INSTANCE,
                    panes: Vec::new(),
                    closed: Vec::new(),
                    process_info: HashMap::new(),
                    pane_text: HashMap::new(),
                    next_workspace: 0,
                    next_terminal: 0,
                    next_session: 0,
                    restore_mode: RestoreMode::Shells,
                    restore_delay: 0,
                    restore_delay_left: 0,
                    restore_gradual: false,
                    unrevealed: VecDeque::new(),
                    fail_next: HashMap::new(),
                    hung: HashSet::new(),
                    fail_spawn_server: None,
                    exe_missing: false,
                    rediscoveries: 0,
                    agent_scripts: VecDeque::new(),
                    exits: HashMap::new(),
                    reaped: HashSet::new(),
                    bridges: HashMap::new(),
                    bridge_input: HashMap::new(),
                    failing_input: HashSet::new(),
                    blocked_input: HashSet::new(),
                    close_linger: 0,
                    lingering: Vec::new(),
                    hooks: HashMap::new(),
                    bridge_kill_hook: None,
                }),
                exited: Condvar::new(),
                input: Condvar::new(),
            }),
            _home: home,
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.shared.state.lock().unwrap()
    }
}

// Used by tests added alongside later behaviour (resume, reconcile, recovery).
#[allow(dead_code)]
impl FakeHerdr {
    /// Add a live pane at `cwd` (its own workspace), optionally running `agent`
    /// with a recorded session. Not recorded as a call.
    pub fn add_pane(&self, cwd: &str, agent: Option<(&str, AgentSession)>) -> PaneIdentity {
        let mut st = self.state();
        let mut pane = st.new_pane(cwd);
        if let Some((kind, session)) = agent {
            pane.agent = Some(kind.to_string());
            pane.agent_status = Some("idle".into());
            pane.agent_session = Some(session);
        }
        *st.panes.last_mut().unwrap() = pane.clone();
        pane
    }

    /// Set (or clear) the session herdr has recorded for `pane_id`.
    pub fn set_agent_session(&self, pane_id: &str, s: Option<AgentSession>) {
        if let Ok(p) = self.state().pane_mut(pane_id) {
            p.agent_session = s;
        }
    }

    /// Set (or clear) the lines above the live tail herdr reports for `pane_id`.
    pub fn set_pane_scroll(&self, pane_id: &str, offset_from_bottom: Option<u32>) {
        if let Ok(p) = self.state().pane_mut(pane_id) {
            p.scroll = offset_from_bottom;
        }
    }

    /// Set the text `pane_read` answers for `pane_id`.
    pub fn set_pane_text(&self, pane_id: &str, text: &str) {
        self.state()
            .pane_text
            .insert(pane_id.to_string(), text.to_string());
    }

    /// Set (or clear) the row count herdr reports for `pane_id`'s viewport.
    pub fn set_viewport_rows(&self, pane_id: &str, rows: Option<u16>) {
        if let Ok(p) = self.state().pane_mut(pane_id) {
            p.viewport_rows = rows;
        }
    }

    /// What `pane_process_info` answers for `pane_id` from now on.
    pub fn set_process_info(&self, pane_id: &str, info: PaneProcessInfo) {
        self.state().process_info.insert(pane_id.to_string(), info);
    }

    /// Simulate the agent exiting in (or returning to) its shell.
    pub fn set_agent_present(&self, pane_id: &str, present: bool) {
        if let Ok(p) = self.state().pane_mut(pane_id) {
            if present {
                let kind = p
                    .agent_session
                    .as_ref()
                    .map(|s| s.agent.clone())
                    .unwrap_or_else(|| "claude".into());
                p.agent = Some(kind);
                p.agent_status = Some("idle".into());
            } else {
                p.agent = None;
                p.agent_status = None;
            }
        }
    }

    /// What `server_instance` reports while the server runs.
    pub fn set_instance(&self, i: Option<ServerInstance>) {
        let mut st = self.state();
        st.instance = i;
        if let Some(i) = i {
            st.basis = i;
        }
    }

    /// The server dies with `exit_code`: every control call then fails with
    /// "server not running", and the server process reports the exit.
    pub fn kill_server(&self, exit_code: Option<i32>) {
        self.state().kill(exit_code);
        self.shared.exited.notify_all();
    }

    /// The restore behaviour of the next `spawn_server`.
    pub fn set_restore_mode(&self, m: RestoreMode) {
        self.state().restore_mode = m;
    }

    /// After each restart, `pane_list` answers `[]` for the first `n` calls
    /// (restore not finished yet).
    pub fn restore_delay_polls(&self, n: u32) {
        self.state().restore_delay = n;
    }

    /// After each restart, `pane_list` first shows one restored pane, then one
    /// more per answer (herdr restoring a session's panes one by one).
    pub fn restore_gradually(&self, on: bool) {
        self.state().restore_gradual = on;
    }

    /// A live handoff: a new server instance takes over with every pane, id and
    /// agent preserved.
    pub fn handoff(&self) {
        let mut st = self.state();
        st.next_instance();
        st.running = true;
    }

    /// A closed pane keeps appearing in the next `n` `pane_list` answers (herdr
    /// tearing it down asynchronously).
    pub fn close_linger_polls(&self, n: u32) {
        self.state().close_linger = n;
    }

    /// Run `hook` right after each recorded call of `op`. The hook runs under
    /// the fake's lock and must not call the fake.
    pub fn on_call(&self, op: Op, hook: CallHook) {
        self.state().hooks.insert(op, hook);
    }

    /// Run `hook` when a bridge spawned after this call is killed. It runs
    /// outside the fake's lock, on the thread that stops the bridge.
    pub fn on_bridge_kill(&self, hook: CallHook) {
        self.state().bridge_kill_hook = Some(hook);
    }

    /// The next call of `op` fails with `err` (FIFO per op).
    pub fn fail_next(&self, op: Op, err: HerdrError) {
        self.state().fail_next.entry(op).or_default().push_back(err);
    }

    /// While on, `op` returns [`HerdrError::Timeout`] immediately. For
    /// `SpawnServer` (which has no `HerdrError`) the spawn succeeds but the new
    /// server never starts answering.
    pub fn hang(&self, op: Op, on: bool) {
        let mut st = self.state();
        if on {
            st.hung.insert(op);
        } else {
            st.hung.remove(&op);
        }
    }

    /// While on, the herdr executable does not exist: every call (and both
    /// spawns) fails as a spawn of a missing file does, the server's identity
    /// cannot be read, and [`HerdrOps::rediscover`] finds nothing.
    pub fn set_exe_missing(&self, on: bool) {
        self.state().exe_missing = on;
    }

    /// How many times [`HerdrOps::rediscover`] ran (it is not a recorded call).
    pub fn rediscoveries(&self) -> u32 {
        self.state().rediscoveries
    }

    /// While `Some`, `spawn_server` fails with that error kind.
    pub fn fail_spawn_server(&self, e: Option<std::io::ErrorKind>) {
        self.state().fail_spawn_server = e;
    }

    /// Outcomes of the next `agent start` calls (FIFO; `Ok` once exhausted).
    pub fn script_agent_start(&self, results: Vec<AgentStartScript>) {
        self.state().agent_scripts.extend(results);
    }

    /// Make `ndjson_line` (plus a newline) appear on `terminal_id`'s bridge stdout.
    pub fn push_frame(&self, terminal_id: &str, ndjson_line: &str) {
        let tx = self.state().bridges.get(terminal_id).cloned();
        if let Some(tx) = tx {
            if let Some(tx) = tx.lock().unwrap().as_ref() {
                let _ = tx.send(format!("{ndjson_line}\n").into_bytes());
            }
        }
    }

    /// In one step: the server process `exited_pid` exits, and another server
    /// (`foreign`, not started through this fake) answers the socket instead.
    pub fn replace_with_foreign(&self, exited_pid: u32, foreign: ServerInstance) {
        let mut st = self.state();
        st.exits.entry(exited_pid).or_insert(ExitInfo {
            code: Some(1),
            signal: None,
        });
        st.instance = Some(foreign);
        st.basis = foreign;
        st.running = true;
        drop(st);
        self.shared.exited.notify_all();
    }

    /// Whether the exit of server process `pid` was collected.
    pub fn reaped(&self, pid: u32) -> bool {
        self.state().reaped.contains(&pid)
    }

    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }

    pub fn panes(&self) -> Vec<PaneIdentity> {
        self.state().panes.clone()
    }

    pub fn closed_pane_ids(&self) -> Vec<String> {
        self.state().closed.clone()
    }

    /// While on, every write to `terminal_id`'s bridge stdin fails with a broken pipe.
    pub fn fail_bridge_input(&self, terminal_id: &str, on: bool) {
        let mut st = self.state();
        if on {
            st.failing_input.insert(terminal_id.to_string());
        } else {
            st.failing_input.remove(terminal_id);
        }
    }

    /// While on, every write to `terminal_id`'s bridge stdin blocks until that
    /// bridge's child is killed (then fails with a broken pipe, as a real pipe
    /// does once its reader dies). Turning it off lets blocked writes through.
    pub fn block_bridge_input(&self, terminal_id: &str, on: bool) {
        let mut st = self.state();
        if on {
            st.blocked_input.insert(terminal_id.to_string());
        } else {
            st.blocked_input.remove(terminal_id);
        }
        drop(st);
        self.shared.input.notify_all();
    }

    /// Everything written to `terminal_id`'s bridge stdin, as text.
    pub fn bridge_input(&self, terminal_id: &str) -> String {
        self.state()
            .bridge_input
            .get(terminal_id)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default()
    }
}

/// A fake server process: observes the exit the fake records for its pid.
struct FakeServerProcess {
    pid: u32,
    shared: Arc<Shared>,
}

impl ServerProcess for FakeServerProcess {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitInfo>> {
        let mut st = self.shared.state.lock().unwrap();
        let exit = st.exits.get(&self.pid).copied();
        if exit.is_some() {
            st.reaped.insert(self.pid);
        }
        Ok(exit)
    }

    fn wait(&mut self) -> std::io::Result<ExitInfo> {
        let mut st = self.shared.state.lock().unwrap();
        loop {
            if let Some(e) = st.exits.get(&self.pid).copied() {
                st.reaped.insert(self.pid);
                return Ok(e);
            }
            st = self.shared.exited.wait(st).unwrap();
        }
    }

    /// Behaves like `kill_server(Some(1))` when this is the live server.
    fn kill(&mut self) -> std::io::Result<()> {
        let mut st = self.shared.state.lock().unwrap();
        if st.instance.map(|i| i.pid) == Some(self.pid) && st.running {
            st.kill(Some(1));
        } else {
            st.exits.entry(self.pid).or_insert(ExitInfo {
                code: Some(1),
                signal: None,
            });
        }
        drop(st);
        self.shared.exited.notify_all();
        Ok(())
    }
}

/// A fake bridge child: killing it drops the stdout sender (EOF for the reader),
/// breaks its stdin pipe and runs the kill hook, if one was set when it was
/// spawned.
struct FakeBridgeChild {
    stdout: FrameTx,
    hook: Option<CallHook>,
    killed: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

impl BridgeChild for FakeBridgeChild {
    fn kill(&mut self) {
        *self.stdout.lock().unwrap() = None;
        {
            // Under the state lock, so a write about to block cannot miss it.
            let _st = self.shared.state.lock().unwrap();
            self.killed.store(true, Ordering::SeqCst);
        }
        self.shared.input.notify_all();
        if let Some(h) = &self.hook {
            h();
        }
    }

    fn wait(&mut self) {}
}

/// Bridge stdout: blocks for the next chunk; `Ok(0)` once the sender is gone.
struct FakeStdout {
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl Read for FakeStdout {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pending.is_empty() {
            match self.rx.recv() {
                Ok(chunk) => self.pending = chunk,
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

/// Bridge stdin: appends to the fake's per-terminal input buffer.
struct FakeStdin {
    shared: Arc<Shared>,
    terminal_id: String,
    /// Set when this bridge's child is killed: the pipe is broken.
    killed: Arc<AtomicBool>,
}

impl Write for FakeStdin {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut st = self.shared.state.lock().unwrap();
        while st.blocked_input.contains(&self.terminal_id) && !self.killed.load(Ordering::SeqCst) {
            st = self.shared.input.wait(st).unwrap();
        }
        if self.killed.load(Ordering::SeqCst) || st.failing_input.contains(&self.terminal_id) {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        st.bridge_input
            .entry(self.terminal_id.clone())
            .or_default()
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl HerdrOps for FakeHerdr {
    fn env(&self) -> &HerdrEnv {
        &self.env
    }

    /// Found (the fake's fixed executable) unless the executable is scripted
    /// missing; then nothing is found among [`FAKE_SEARCHED`].
    fn rediscover(&self) -> ExeDiscovery {
        let mut st = self.state();
        st.rediscoveries += 1;
        if st.exe_missing {
            let searched: Vec<std::path::PathBuf> = FAKE_SEARCHED.iter().map(Into::into).collect();
            ExeDiscovery {
                exe: "herdr".into(),
                notable: searched.clone(),
                searched,
                found: false,
                explicit: false,
                login_deferred: false,
            }
        } else {
            self.env.discovery.clone()
        }
    }

    fn workspace_create(
        &self,
        cwd: &str,
        env: &[(String, String)],
    ) -> Result<PaneIdentity, HerdrError> {
        let mut st = self.state();
        st.control(Op::WorkspaceCreate, format!("{cwd} {env:?}"))?;
        Ok(st.new_pane(cwd))
    }

    fn pane_run(&self, pane_id: &str, command: &[&str]) -> Result<(), HerdrError> {
        let mut st = self.state();
        st.control(Op::PaneRun, format!("{pane_id} {}", command.join(" ")))?;
        st.pane_mut(pane_id).map(|_| ())
    }

    fn pane_send_text(&self, pane_id: &str, text: &str) -> Result<(), HerdrError> {
        let mut st = self.state();
        st.control(Op::PaneSendText, format!("{pane_id} {text}"))?;
        st.pane_mut(pane_id).map(|_| ())
    }

    fn pane_send_keys(&self, pane_id: &str, keys: &[String]) -> Result<(), HerdrError> {
        let mut st = self.state();
        st.control(Op::PaneSendKeys, format!("{pane_id} {}", keys.join(" ")))?;
        st.pane_mut(pane_id).map(|_| ())
    }

    /// Removes the pane; its bridge sees `terminal.closed` and then EOF.
    fn pane_close(&self, pane_id: &str) -> Result<(), HerdrError> {
        let mut st = self.state();
        st.control(Op::PaneClose, pane_id.to_string())?;
        let idx = st
            .panes
            .iter()
            .position(|p| p.pane_id == pane_id)
            .ok_or_else(|| pane_not_found(pane_id))?;
        let pane = st.panes.remove(idx);
        st.closed.push(pane.pane_id.clone());
        if st.close_linger > 0 {
            let n = st.close_linger;
            st.lingering.push((pane.clone(), n));
        }
        if let Some(tx) = st.bridges.get(&pane.terminal_id) {
            let mut tx = tx.lock().unwrap();
            if let Some(s) = tx.as_ref() {
                let _ = s.send(b"{\"type\":\"terminal.closed\"}\n".to_vec());
            }
            *tx = None;
        }
        Ok(())
    }

    fn pane_read(
        &self,
        pane_id: &str,
        source: &str,
        format: &str,
        lines: Option<u32>,
    ) -> Result<String, HerdrError> {
        let mut st = self.state();
        let detail = match lines {
            Some(n) => format!("{pane_id} {source} {format} --lines {n}"),
            None => format!("{pane_id} {source} {format}"),
        };
        st.control(Op::PaneRead, detail)?;
        st.pane_mut(pane_id)?;
        Ok(st.pane_text.get(pane_id).cloned().unwrap_or_default())
    }

    fn pane_list(&self) -> Result<Vec<PaneInfo>, HerdrError> {
        let mut st = self.state();
        st.control(Op::PaneList, String::new())?;
        if st.restore_delay_left > 0 {
            st.restore_delay_left -= 1;
            return Ok(Vec::new());
        }
        let hidden: HashSet<String> = st.unrevealed.iter().cloned().collect();
        st.unrevealed.pop_front();
        let mut out: Vec<PaneInfo> = st
            .panes
            .iter()
            .filter(|p| !hidden.contains(&p.pane_id))
            .cloned()
            .collect();
        for (pane, left) in st.lingering.iter_mut() {
            out.push(pane.clone());
            *left -= 1;
        }
        st.lingering.retain(|(_, left)| *left > 0);
        Ok(out)
    }

    fn pane_get(&self, pane_id: &str) -> Result<Option<PaneInfo>, HerdrError> {
        let mut st = self.state();
        st.control(Op::PaneGet, pane_id.to_string())?;
        st.pane_mut(pane_id).map(|p| Some(p.clone()))
    }

    /// The info scripted with [`FakeHerdr::set_process_info`]; a pane without
    /// any is not found.
    fn pane_process_info(&self, pane_id: &str) -> Result<PaneProcessInfo, HerdrError> {
        let mut st = self.state();
        st.control(Op::PaneProcessInfo, pane_id.to_string())?;
        st.process_info
            .get(pane_id)
            .cloned()
            .ok_or_else(|| pane_not_found(pane_id))
    }

    fn agent_start(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
        timeout_ms: u64,
    ) -> Result<(), HerdrError> {
        let mut st = self.state();
        st.control(
            Op::AgentStart,
            format!("{name} {kind} {pane_id} {timeout_ms} {}", args.join(" ")),
        )?;
        st.pane_mut(pane_id)?;
        let script = st.agent_scripts.pop_front().unwrap_or(AgentStartScript::Ok);
        match script {
            AgentStartScript::Ok | AgentStartScript::NotReady => {
                let value = match resume_id_from(args) {
                    Some(id) => id,
                    None => {
                        st.next_session += 1;
                        format!("fake-session-{}", st.next_session)
                    }
                };
                let pane = st.pane_mut(pane_id)?;
                pane.agent = Some(kind.to_string());
                pane.agent_status = Some("idle".into());
                pane.agent_session = Some(AgentSession {
                    source: format!("herdr:{kind}"),
                    agent: kind.to_string(),
                    kind: AgentSessionKind::Id,
                    value,
                });
                Ok(())
            }
            AgentStartScript::EarlyExit => {
                Err(api("agent_start_failed", "agent exited during startup"))
            }
            AgentStartScript::NameLost => Err(api("agent_name_not_found", "agent name not found")),
            AgentStartScript::Err(e) => Err(e),
        }
    }

    fn agent_prompt(&self, pane_id: &str, text: &str) -> Result<(), HerdrError> {
        let mut st = self.state();
        st.control(Op::AgentPrompt, format!("{pane_id} {text}"))?;
        match st.pane_mut(pane_id)?.agent {
            Some(_) => Ok(()),
            None => Err(api("agent_not_found", "no agent in pane")),
        }
    }

    fn agent_list(&self) -> Result<Vec<AgentInfo>, HerdrError> {
        let mut st = self.state();
        st.control(Op::AgentList, String::new())?;
        Ok(st
            .panes
            .iter()
            .filter(|p| p.agent.is_some())
            .map(|p| AgentInfo {
                pane_id: p.pane_id.clone(),
                agent: p.agent.clone(),
                agent_status: p.agent_status.clone().unwrap_or_else(|| "unknown".into()),
                launch_pending: false,
            })
            .collect())
    }

    /// Integrations need no server; always reported installed.
    fn integration_status(&self, target: &str) -> Result<IntegrationStatus, HerdrError> {
        let mut st = self.state();
        st.record(Op::IntegrationStatus, target.to_string());
        match st.scripted(Op::IntegrationStatus) {
            Some(e) => Err(e),
            None => Ok(IntegrationStatus::Installed),
        }
    }

    fn integration_install(&self, target: &str) -> Result<(), HerdrError> {
        let mut st = self.state();
        st.record(Op::IntegrationInstall, target.to_string());
        match st.scripted(Op::IntegrationInstall) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Answers even while the server is down (`running: false`), like herdr.
    fn status_server(&self) -> Result<ServerStatus, HerdrError> {
        let mut st = self.state();
        st.record(Op::StatusServer, String::new());
        if let Some(e) = st.scripted(Op::StatusServer) {
            return Err(e);
        }
        Ok(ServerStatus {
            running: st.running,
            version: st.running.then(|| "fake".to_string()),
            protocol: st.running.then_some(1),
            socket: None,
        })
    }

    fn server_stop(&self) -> Result<(), HerdrError> {
        {
            let mut st = self.state();
            st.control(Op::ServerStop, String::new())?;
            st.kill(Some(0));
        }
        self.shared.exited.notify_all();
        Ok(())
    }

    fn spawn_server(&self) -> std::io::Result<Box<dyn ServerProcess>> {
        let mut st = self.state();
        st.record(Op::SpawnServer, String::new());
        if st.exe_missing {
            return Err(std::io::Error::from_raw_os_error(2));
        }
        if let Some(kind) = st.fail_spawn_server {
            return Err(std::io::Error::from(kind));
        }
        let i = st.restart();
        if st.hung.contains(&Op::SpawnServer) {
            // Spawned, but never starts answering.
            st.running = false;
        }
        Ok(Box::new(FakeServerProcess {
            pid: i.pid,
            shared: Arc::clone(&self.shared),
        }))
    }

    fn spawn_bridge(
        &self,
        terminal_id: &str,
        cols: u16,
        rows: u16,
    ) -> std::io::Result<BridgeProcess> {
        let mut st = self.state();
        st.record(Op::SpawnBridge, format!("{terminal_id} {cols}x{rows}"));
        if let Some(e) = st.scripted(Op::SpawnBridge) {
            return Err(std::io::Error::other(e.to_string()));
        }
        let (tx, rx) = mpsc::channel();
        let tx: FrameTx = Arc::new(Mutex::new(Some(tx)));
        st.bridges.insert(terminal_id.to_string(), Arc::clone(&tx));
        let killed = Arc::new(AtomicBool::new(false));
        Ok(BridgeProcess {
            stdin: Box::new(FakeStdin {
                shared: Arc::clone(&self.shared),
                terminal_id: terminal_id.to_string(),
                killed: Arc::clone(&killed),
            }),
            stdout: Box::new(FakeStdout {
                rx,
                pending: Vec::new(),
            }),
            stderr: None,
            child: Box::new(FakeBridgeChild {
                stdout: tx,
                hook: st.bridge_kill_hook.clone(),
                killed,
                shared: Arc::clone(&self.shared),
            }),
        })
    }

    fn server_pid(&self, _sys: &mut sysinfo::System) -> Option<u32> {
        let mut st = self.state();
        st.record(Op::ServerPid, String::new());
        if !st.running || st.exe_missing {
            return None;
        }
        st.instance.map(|i| i.pid)
    }

    /// A scripted failure or hang of `ServerInstance` reads as "could not be
    /// resolved" (`None`).
    fn server_instance(&self) -> Option<ServerInstance> {
        let mut st = self.state();
        st.record(Op::ServerInstance, String::new());
        if st.scripted(Op::ServerInstance).is_some() || !st.running {
            return None;
        }
        st.instance
    }
}

/// What a [`CaptureOutput`] received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputEvent {
    Frame { b64: String, full: bool },
    Exit,
}

/// A recording [`crate::backend::OutputSink`]: a pane's output as tests see it.
#[derive(Default)]
pub struct CaptureOutput(Mutex<Vec<OutputEvent>>);

impl crate::backend::OutputSink for CaptureOutput {
    fn frame(&self, b64: String, full: bool) {
        self.0
            .lock()
            .unwrap()
            .push(OutputEvent::Frame { b64, full });
    }
    fn exit(&self) {
        self.0.lock().unwrap().push(OutputEvent::Exit);
    }
}

impl CaptureOutput {
    pub fn events(&self) -> Vec<OutputEvent> {
        self.0.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    /// The decoded text of every frame so far, in order.
    pub fn frames(&self) -> Vec<String> {
        use base64::Engine as _;
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                OutputEvent::Frame { b64, .. } => base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .ok()
                    .map(|d| String::from_utf8_lossy(&d).into_owned()),
                OutputEvent::Exit => None,
            })
            .collect()
    }

    pub fn exits(&self) -> usize {
        self.events()
            .iter()
            .filter(|e| **e == OutputEvent::Exit)
            .count()
    }

    /// Wait (bounded) until `pred` holds over the received events.
    pub fn wait_for(&self, pred: impl Fn(&[OutputEvent]) -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if pred(&self.events()) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(agent: &str, id: &str) -> AgentSession {
        AgentSession {
            source: format!("herdr:{agent}"),
            agent: agent.into(),
            kind: AgentSessionKind::Id,
            value: id.into(),
        }
    }

    fn ops_of(fake: &FakeHerdr) -> Vec<Op> {
        fake.calls().into_iter().map(|c| c.op).collect()
    }

    #[test]
    fn a_missing_executable_fails_every_call_and_a_rediscovery() {
        let fake = FakeHerdr::new();
        assert!(fake.rediscover().found);

        fake.set_exe_missing(true);
        assert_eq!(fake.status_server(), Err(exe_missing_error()));
        assert_eq!(fake.server_stop(), Err(exe_missing_error()));
        assert_eq!(fake.pane_list(), Err(exe_missing_error()));
        assert!(exe_missing_error().is_exe_missing());
        let spawn = fake.spawn_server().err().expect("the spawn fails");
        assert_eq!(spawn.raw_os_error(), Some(2));
        assert!(fake.spawn_bridge("term_1", 80, 24).is_err());
        assert_eq!(fake.server_instance(), None);
        let d = fake.rediscover();
        assert!(!d.found);
        assert_eq!(d.exe, std::path::PathBuf::from("herdr"));
        assert_eq!(d.searched.len(), FAKE_SEARCHED.len());
        // The calls are still recorded; the rediscoveries are counted apart.
        assert_eq!(
            ops_of(&fake),
            vec![
                Op::StatusServer,
                Op::ServerStop,
                Op::PaneList,
                Op::SpawnServer,
                Op::SpawnBridge,
                Op::ServerInstance,
            ]
        );
        assert_eq!(fake.rediscoveries(), 2);

        // Back again: the server it had was never touched.
        fake.set_exe_missing(false);
        assert!(fake.rediscover().found);
        assert!(fake.status_server().unwrap().running);
    }

    #[test]
    fn records_calls_in_order() {
        let fake = FakeHerdr::new();
        let p = fake.workspace_create("/w", &[]).unwrap();
        let _ = fake.pane_list();
        let _ = fake.status_server();
        let _ = fake.pane_close(&p.pane_id);
        assert_eq!(
            ops_of(&fake),
            vec![
                Op::WorkspaceCreate,
                Op::PaneList,
                Op::StatusServer,
                Op::PaneClose
            ]
        );
        assert_eq!(fake.calls()[3].args, "w1:p1");
        assert_eq!(fake.closed_pane_ids(), vec!["w1:p1".to_string()]);
    }

    #[test]
    fn workspace_create_allocates_sequential_ids() {
        let fake = FakeHerdr::new();
        let a = fake.workspace_create("/a", &[]).unwrap();
        let b = fake.workspace_create("/b", &[]).unwrap();
        assert_eq!(
            (
                a.workspace_id.as_str(),
                a.pane_id.as_str(),
                a.terminal_id.as_str()
            ),
            ("w1", "w1:p1", "term_1")
        );
        assert_eq!(
            (
                b.workspace_id.as_str(),
                b.pane_id.as_str(),
                b.terminal_id.as_str()
            ),
            ("w2", "w2:p1", "term_2")
        );
        assert_eq!(b.cwd.as_deref(), Some("/b"));
        assert!(
            a.agent.is_none() && a.agent_session.is_none(),
            "a bare shell"
        );
        assert_eq!(fake.pane_list().unwrap().len(), 2);
    }

    #[test]
    fn agent_start_consumes_scripts_and_records_sessions() {
        let fake = FakeHerdr::new();
        let p = fake.workspace_create("/w", &[]).unwrap();
        fake.script_agent_start(vec![
            AgentStartScript::EarlyExit,
            AgentStartScript::NameLost,
            AgentStartScript::NotReady,
        ]);
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let e = fake.agent_start("n", "claude", &p.pane_id, &[], 5000);
        assert_eq!(e.unwrap_err().code(), Some("agent_start_failed"));
        let e = fake.agent_start("n", "claude", &p.pane_id, &[], 5000);
        assert_eq!(e.unwrap_err().code(), Some("agent_name_not_found"));
        assert!(
            fake.panes()[0].agent.is_none(),
            "failed starts leave no agent"
        );

        // NotReady counts as launched; the id comes from `--resume <id>`.
        fake.agent_start("n", "claude", &p.pane_id, &args(&["--resume", "abc"]), 5000)
            .unwrap();
        let pane = fake.panes()[0].clone();
        assert_eq!(pane.agent.as_deref(), Some("claude"));
        assert_eq!(pane.agent_session, Some(session("claude", "abc")));

        // Scripts exhausted → Ok; codex `resume <id>`; `resume --last` → generated id.
        fake.agent_start("n", "codex", &p.pane_id, &args(&["resume", "th-1"]), 5000)
            .unwrap();
        assert_eq!(
            fake.panes()[0].agent_session,
            Some(session("codex", "th-1"))
        );
        fake.agent_start("n", "codex", &p.pane_id, &args(&["resume", "--last"]), 5000)
            .unwrap();
        assert_eq!(
            fake.panes()[0].agent_session,
            Some(session("codex", "fake-session-1"))
        );

        fake.script_agent_start(vec![AgentStartScript::Err(HerdrError::Timeout)]);
        assert_eq!(
            fake.agent_start("n", "claude", &p.pane_id, &[], 5000),
            Err(HerdrError::Timeout)
        );
        assert!(fake
            .agent_start("n", "claude", "w9:p9", &[], 5000)
            .unwrap_err()
            .is_not_found());
    }

    #[test]
    fn kill_server_fails_control_calls_and_reports_exit() {
        let fake = FakeHerdr::new();
        fake.kill_server(Some(3));
        assert_eq!(fake.pane_list(), Err(not_running()));
        assert_eq!(fake.workspace_create("/w", &[]), Err(not_running()));
        assert!(!fake.status_server().unwrap().running);
        assert_eq!(fake.server_instance(), None);

        let mut proc_ = fake.spawn_server().unwrap();
        assert!(fake.status_server().unwrap().running);
        assert_eq!(proc_.try_wait().unwrap(), None, "still running");
        fake.kill_server(Some(7));
        let want = ExitInfo {
            code: Some(7),
            signal: None,
        };
        assert_eq!(proc_.try_wait().unwrap(), Some(want));
        assert_eq!(proc_.wait().unwrap(), want);
    }

    #[test]
    fn server_process_kill_acts_like_kill_server() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        let mut proc_ = fake.spawn_server().unwrap();
        proc_.kill().unwrap();
        assert!(!fake.status_server().unwrap().running);
        assert_eq!(proc_.wait().unwrap().code, Some(1));
        assert_eq!(fake.pane_list(), Err(not_running()));
    }

    #[test]
    fn spawn_after_kill_is_a_new_instance_restoring_shells() {
        let fake = FakeHerdr::new();
        assert_eq!(fake.server_instance(), Some(INITIAL_INSTANCE));
        let a = fake.add_pane("/a", Some(("claude", session("claude", "s-a"))));
        let b = fake.add_pane("/b", None);
        let _ = fake.pane_close(&a.pane_id);
        let a = fake.add_pane("/a", Some(("claude", session("claude", "s-a"))));
        assert_eq!(a.pane_id, "w3:p1");

        fake.kill_server(Some(1));
        let proc_ = fake.spawn_server().unwrap();
        let new = ServerInstance {
            pid: 1001,
            start_time: 1_000_010,
        };
        assert_eq!(proc_.pid(), 1001);
        assert_eq!(fake.server_instance(), Some(new));

        let panes = fake.pane_list().unwrap();
        // Renumbered from 1 in the previous order; cwd kept; agent gone; session kept.
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "w1:p1");
        assert_eq!(panes[0].cwd.as_deref(), Some("/b"));
        assert_eq!(panes[1].pane_id, "w2:p1");
        assert_eq!(panes[1].cwd.as_deref(), Some("/a"));
        assert!(panes[1].agent.is_none());
        assert_eq!(panes[1].agent_session, Some(session("claude", "s-a")));
        assert_ne!(panes[1].terminal_id, a.terminal_id, "new terminal id");
        assert_ne!(panes[0].terminal_id, b.terminal_id);
        // New workspaces continue after the restored ones.
        assert_eq!(fake.workspace_create("/c", &[]).unwrap().pane_id, "w3:p1");
    }

    #[test]
    fn agents_restore_mode_keeps_the_agent() {
        let fake = FakeHerdr::new();
        fake.add_pane("/a", Some(("codex", session("codex", "t"))));
        fake.set_restore_mode(RestoreMode::Agents);
        fake.kill_server(None);
        fake.spawn_server().unwrap();
        let p = fake.pane_list().unwrap().remove(0);
        assert_eq!(p.agent.as_deref(), Some("codex"));
        assert_eq!(p.agent_session, Some(session("codex", "t")));
    }

    #[test]
    fn restore_delay_returns_empty_lists_after_a_restart() {
        let fake = FakeHerdr::new();
        fake.add_pane("/a", None);
        fake.restore_delay_polls(2);
        assert_eq!(
            fake.pane_list().unwrap().len(),
            1,
            "no delay before a restart"
        );
        fake.kill_server(None);
        fake.spawn_server().unwrap();
        assert!(fake.pane_list().unwrap().is_empty());
        assert!(fake.pane_list().unwrap().is_empty());
        assert_eq!(fake.pane_list().unwrap().len(), 1);
    }

    #[test]
    fn handoff_keeps_ids_and_agents_with_a_new_instance() {
        let fake = FakeHerdr::new();
        let a = fake.add_pane("/a", Some(("claude", session("claude", "s"))));
        fake.handoff();
        assert_eq!(
            fake.server_instance(),
            Some(ServerInstance {
                pid: 1001,
                start_time: 1_000_010
            })
        );
        assert_eq!(fake.pane_list().unwrap(), vec![a]);
        fake.set_instance(Some(ServerInstance {
            pid: 42,
            start_time: 7,
        }));
        assert_eq!(fake.server_instance().map(|i| i.pid), Some(42));
        fake.set_instance(None);
        assert_eq!(fake.server_instance(), None);
    }

    #[test]
    fn hang_and_fail_next_are_scripted_per_op() {
        let fake = FakeHerdr::new();
        fake.fail_next(Op::PaneList, api("first", "1"));
        fake.fail_next(Op::PaneList, api("second", "2"));
        assert_eq!(fake.pane_list().unwrap_err().code(), Some("first"));
        assert_eq!(fake.pane_list().unwrap_err().code(), Some("second"));
        assert!(fake.pane_list().is_ok(), "queue drained");
        assert!(fake.status_server().is_ok(), "other ops unaffected");

        fake.hang(Op::StatusServer, true);
        assert_eq!(fake.status_server(), Err(HerdrError::Timeout));
        assert_eq!(fake.status_server(), Err(HerdrError::Timeout));
        fake.hang(Op::StatusServer, false);
        assert!(fake.status_server().is_ok());
    }

    #[test]
    fn pane_process_info_answers_what_was_scripted() {
        let fake = FakeHerdr::new();
        let info = PaneProcessInfo {
            shell_pid: 4100,
            foreground_pids: vec![4200],
        };
        fake.set_process_info("w1:p1", info.clone());
        assert_eq!(fake.pane_process_info("w1:p1"), Ok(info));
        assert!(
            fake.pane_process_info("w9:p9").unwrap_err().is_not_found(),
            "an unscripted pane is not found"
        );
        assert_eq!(
            fake.calls(),
            vec![
                Call {
                    op: Op::PaneProcessInfo,
                    args: "w1:p1".into(),
                },
                Call {
                    op: Op::PaneProcessInfo,
                    args: "w9:p9".into(),
                },
            ]
        );
    }

    #[test]
    fn pane_process_info_honours_fail_next_and_a_dead_server() {
        let fake = FakeHerdr::new();
        let info = PaneProcessInfo {
            shell_pid: 7,
            foreground_pids: Vec::new(),
        };
        fake.set_process_info("w1:p1", info.clone());
        fake.fail_next(Op::PaneProcessInfo, HerdrError::Timeout);
        assert_eq!(fake.pane_process_info("w1:p1"), Err(HerdrError::Timeout));
        assert_eq!(fake.pane_process_info("w1:p1"), Ok(info), "queue drained");
        assert!(fake.pane_list().is_ok(), "other ops unaffected");

        fake.kill_server(None);
        assert_eq!(fake.pane_process_info("w1:p1"), Err(not_running()));
        assert_eq!(
            ops_of(&fake),
            vec![
                Op::PaneProcessInfo,
                Op::PaneProcessInfo,
                Op::PaneList,
                Op::PaneProcessInfo,
            ]
        );
    }

    #[test]
    fn agent_presence_and_session_can_be_scripted() {
        let fake = FakeHerdr::new();
        let p = fake.add_pane("/a", Some(("claude", session("claude", "s"))));
        fake.set_agent_present(&p.pane_id, false);
        assert!(fake.panes()[0].agent.is_none());
        assert!(fake.agent_list().unwrap().is_empty());
        assert!(fake.agent_prompt(&p.pane_id, "hi").is_err());
        fake.set_agent_present(&p.pane_id, true);
        assert_eq!(
            fake.agent_list().unwrap()[0].agent.as_deref(),
            Some("claude")
        );
        assert!(fake.agent_prompt(&p.pane_id, "hi").is_ok());
        fake.set_agent_session(&p.pane_id, None);
        assert!(fake.panes()[0].agent_session.is_none());
    }

    #[test]
    fn bridge_pipes_carry_frames_and_input() {
        let fake = FakeHerdr::new();
        let mut b = fake.spawn_bridge("term_1", 80, 24).unwrap();
        fake.push_frame("term_1", r#"{"type":"terminal.bell"}"#);
        b.stdin.write_all(b"typed").unwrap();
        assert_eq!(fake.bridge_input("term_1"), "typed");
        b.child.kill();
        let mut out = String::new();
        b.stdout.read_to_string(&mut out).unwrap();
        assert_eq!(out, "{\"type\":\"terminal.bell\"}\n", "EOF after kill");
        assert_eq!(
            b.stdin.write_all(b"late").unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe,
            "the pipe breaks with its child"
        );
        assert!(fake.spawn_server().is_ok());
        fake.fail_spawn_server(Some(std::io::ErrorKind::OutOfMemory));
        assert_eq!(
            fake.spawn_server().err().map(|e| e.kind()),
            Some(std::io::ErrorKind::OutOfMemory)
        );
    }
}
