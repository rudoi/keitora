//! `FakeBackend` — a scriptable [`PtyBackend`] for unit tests.
//!
//! Records every trait call in a log and serves scripted result queues for the
//! methods tests care about (`write_line`, `kill`, `list_sessions`, `pane_shell`,
//! `run_line`), plus a hook to
//! flip a key out of the live-set after N `is_live` calls (to simulate a session
//! exiting). Needs no `AppHandle`. The whole module is `#[cfg(test)]` (declared so
//! in `backend/mod.rs`).
//!
//! `PtyBackendError` is not `Clone`, so scripted results are built per test and
//! consumed from `VecDeque`s rather than cloned.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use super::{
    AgentLaunch, AttachResult, OutputSink, PtyBackend, PtyBackendError, SessionKey, SessionSummary,
    StatusPayload,
};
use crate::engine::model::AgentObs;

/// One recorded trait call (args captured where a test might assert on them).
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Start,
    Spawn {
        key: SessionKey,
        cwd: String,
        db_path: String,
        size: Option<(u16, u16)>,
        command: Option<String>,
    },
    SpawnAgent {
        key: SessionKey,
        cwd: String,
        db_path: String,
        size: Option<(u16, u16)>,
        launch: AgentLaunch,
    },
    Attach {
        key: SessionKey,
        size: Option<(u16, u16)>,
    },
    Detach {
        key: SessionKey,
    },
    Write {
        key: SessionKey,
        data: Vec<u8>,
    },
    Paste {
        key: SessionKey,
        text: String,
    },
    WriteLine {
        key: SessionKey,
        text: String,
        submit_delay_ms: u32,
    },
    Resize {
        key: SessionKey,
        cols: u16,
        rows: u16,
    },
    PaneShell {
        key: SessionKey,
    },
    RunLine {
        key: SessionKey,
        gen: u64,
        line: String,
    },
    Kill {
        key: SessionKey,
        gen: u64,
    },
    ListSessions,
    SetTerminated {
        key: SessionKey,
        terminated: bool,
    },
    LiveSetSnapshot,
    IsLive {
        key: SessionKey,
    },
    StatusPayloadSnapshot,
    Shutdown,
    Restart,
    RefreshIntegration,
    AgentSnapshot {
        key: SessionKey,
    },
}

#[derive(Default)]
struct FakeState {
    live: HashSet<SessionKey>,
    sessions: Vec<SessionSummary>,
    calls: Vec<Call>,
    write_line_results: VecDeque<Result<(), PtyBackendError>>,
    paste_results: VecDeque<Result<(), PtyBackendError>>,
    /// `read_history` answers: `(key, lines)` asked for, and the scripted results
    /// (the trait default's "unavailable" error once empty).
    read_history_asks: Vec<(SessionKey, u32)>,
    read_history_results: VecDeque<Result<String, PtyBackendError>>,
    kill_results: VecDeque<Result<(), PtyBackendError>>,
    /// `spawn` answers `Ok(1)` once this is empty.
    spawn_results: VecDeque<Result<u64, PtyBackendError>>,
    /// What `pane_shell` answers once `pane_shell_results` is empty.
    pane_shell: Option<String>,
    pane_shell_results: VecDeque<Result<Option<String>, PtyBackendError>>,
    run_line_results: VecDeque<Result<(), PtyBackendError>>,
    list_sessions_results: VecDeque<Result<Vec<SessionSummary>, PtyBackendError>>,
    is_live_count: u32,
    live_flip_after: Option<u32>,
    live_flip_key: Option<SessionKey>,
    terminated: HashSet<SessionKey>,
    set_terminated_fails: bool,
    /// `status_payload_snapshot` reports `Connected` while set.
    connected: bool,
    /// What `recovery_snapshot` answers (the trait default while `None`).
    recovery: Option<super::herdr::gate::RecoveryPayload>,
    /// `restart` answers `Ok(())` once this is empty.
    restart_results: VecDeque<Result<(), PtyBackendError>>,
    floor: Option<Arc<crate::deps::floor::FloorSlot>>,
    /// What `agent_snapshot` answers per key (`Ok(None)` when unset).
    agents: HashMap<SessionKey, AgentObs>,
}

/// A scriptable, call-logging [`PtyBackend`] for tests.
pub struct FakeBackend {
    state: Mutex<FakeState>,
}

impl FakeBackend {
    pub fn new() -> Self {
        FakeBackend {
            state: Mutex::new(FakeState::default()),
        }
    }

    /// Mark `key` as live (present in the live-set).
    pub fn insert_live(&self, key: SessionKey) {
        self.state.lock().unwrap().live.insert(key);
    }

    /// Remove `key` from the live-set after the `n`-th `is_live` call (simulate an
    /// exit observed by the push loop).
    pub fn flip_live_after(&self, n: u32, key: SessionKey) {
        let mut st = self.state.lock().unwrap();
        st.live_flip_after = Some(n);
        st.live_flip_key = Some(key);
    }

    /// Script the next `write_line` result.
    pub fn push_write_line(&self, r: Result<(), PtyBackendError>) {
        self.state.lock().unwrap().write_line_results.push_back(r);
    }

    /// Script the next `paste` result.
    pub fn push_paste(&self, r: Result<(), PtyBackendError>) {
        self.state.lock().unwrap().paste_results.push_back(r);
    }

    /// Script the next `read_history` result.
    pub fn push_read_history(&self, r: Result<String, PtyBackendError>) {
        self.state.lock().unwrap().read_history_results.push_back(r);
    }

    /// Every `read_history` call so far, as `(key, lines)`.
    pub fn read_history_asks(&self) -> Vec<(SessionKey, u32)> {
        self.state.lock().unwrap().read_history_asks.clone()
    }

    /// Script the next `spawn` result (a terminated key still refuses first).
    pub fn push_spawn(&self, r: Result<u64, PtyBackendError>) {
        self.state.lock().unwrap().spawn_results.push_back(r);
    }

    /// Script the next `kill` result.
    pub fn push_kill(&self, r: Result<(), PtyBackendError>) {
        self.state.lock().unwrap().kill_results.push_back(r);
    }

    /// What `pane_shell` answers from now on (after any queued results).
    pub fn set_pane_shell(&self, name: Option<&str>) {
        self.state.lock().unwrap().pane_shell = name.map(str::to_string);
    }

    /// Script the next `pane_shell` result.
    pub fn push_pane_shell(&self, r: Result<Option<String>, PtyBackendError>) {
        self.state.lock().unwrap().pane_shell_results.push_back(r);
    }

    /// Script the next `run_line` result.
    pub fn push_run_line(&self, r: Result<(), PtyBackendError>) {
        self.state.lock().unwrap().run_line_results.push_back(r);
    }

    /// Script the next `list_sessions` result.
    pub fn push_list_sessions(&self, r: Result<Vec<SessionSummary>, PtyBackendError>) {
        self.state
            .lock()
            .unwrap()
            .list_sessions_results
            .push_back(r);
    }

    /// Flag `key` terminated without logging a call.
    pub fn insert_terminated(&self, key: SessionKey) {
        self.state.lock().unwrap().terminated.insert(key);
    }

    /// Whether `key` is flagged terminated.
    pub fn is_terminated(&self, key: &SessionKey) -> bool {
        self.state.lock().unwrap().terminated.contains(key)
    }

    /// Make every later `set_terminated` fail (a store that cannot persist).
    pub fn fail_set_terminated(&self) {
        self.state.lock().unwrap().set_terminated_fails = true;
    }

    /// Report the server as `Connected` (or not) from `status_payload_snapshot`.
    pub fn set_connected(&self, on: bool) {
        self.state.lock().unwrap().connected = on;
    }

    /// What `recovery_snapshot` answers from now on.
    pub fn set_recovery(&self, payload: super::herdr::gate::RecoveryPayload) {
        self.state.lock().unwrap().recovery = Some(payload);
    }

    /// Script the next `restart` result.
    pub fn push_restart(&self, r: Result<(), PtyBackendError>) {
        self.state.lock().unwrap().restart_results.push_back(r);
    }

    /// Expose `slot` as the backend's floor slot.
    pub fn set_floor_slot(&self, slot: Arc<crate::deps::floor::FloorSlot>) {
        self.state.lock().unwrap().floor = Some(slot);
    }

    /// Set (or, with `None`, clear) the agent entry `agent_snapshot` reports for `key`.
    pub fn set_agent(&self, key: SessionKey, agent: Option<AgentObs>) {
        let mut st = self.state.lock().unwrap();
        match agent {
            Some(a) => st.agents.insert(key, a),
            None => st.agents.remove(&key),
        };
    }

    /// Snapshot of the recorded call log.
    pub fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }
}

impl PtyBackend for FakeBackend {
    fn start(&self) {
        self.state.lock().unwrap().calls.push(Call::Start);
    }

    fn spawn(
        &self,
        key: SessionKey,
        cwd: String,
        db_path: String,
        size: Option<(u16, u16)>,
        command: Option<String>,
    ) -> Result<u64, PtyBackendError> {
        let mut st = self.state.lock().unwrap();
        let terminated = st.terminated.contains(&key);
        st.calls.push(Call::Spawn {
            key,
            cwd,
            db_path,
            size,
            command,
        });
        if terminated {
            return Err(PtyBackendError::Terminated);
        }
        st.spawn_results.pop_front().unwrap_or(Ok(1))
    }

    fn spawn_agent(
        &self,
        key: SessionKey,
        cwd: String,
        db_path: String,
        size: Option<(u16, u16)>,
        launch: AgentLaunch,
    ) -> Result<u64, PtyBackendError> {
        let mut st = self.state.lock().unwrap();
        let terminated = st.terminated.contains(&key);
        st.calls.push(Call::SpawnAgent {
            key,
            cwd,
            db_path,
            size,
            launch,
        });
        if terminated {
            return Err(PtyBackendError::Terminated);
        }
        Ok(1)
    }

    fn attach(
        &self,
        key: SessionKey,
        size: Option<(u16, u16)>,
        _out: std::sync::Arc<dyn OutputSink>,
    ) -> Result<AttachResult, PtyBackendError> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(Call::Attach { key, size });
        Ok(AttachResult {
            gen: 1,
            scrollback: Vec::new(),
            attach_id: 1,
            repaints: false,
        })
    }

    fn detach(&self, key: SessionKey, _attach_id: Option<u64>) -> Result<(), PtyBackendError> {
        self.state.lock().unwrap().calls.push(Call::Detach { key });
        Ok(())
    }

    fn write(&self, key: &SessionKey, data: &[u8]) -> Result<(), PtyBackendError> {
        self.state.lock().unwrap().calls.push(Call::Write {
            key: key.clone(),
            data: data.to_vec(),
        });
        Ok(())
    }

    fn paste(&self, key: &SessionKey, text: &str) -> Result<(), PtyBackendError> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::Paste {
            key: key.clone(),
            text: text.to_string(),
        });
        st.paste_results.pop_front().unwrap_or(Ok(()))
    }

    fn write_line(
        &self,
        key: &SessionKey,
        text: &str,
        submit_delay_ms: u32,
    ) -> Result<(), PtyBackendError> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::WriteLine {
            key: key.clone(),
            text: text.to_string(),
            submit_delay_ms,
        });
        st.write_line_results.pop_front().unwrap_or(Ok(()))
    }

    fn read_history(&self, key: &SessionKey, lines: u32) -> Result<String, PtyBackendError> {
        let mut st = self.state.lock().unwrap();
        st.read_history_asks.push((key.clone(), lines));
        st.read_history_results.pop_front().unwrap_or_else(|| {
            Err(PtyBackendError::Other(
                "history is not available on this backend".to_string(),
            ))
        })
    }

    fn resize(&self, key: &SessionKey, cols: u16, rows: u16) -> Result<(), PtyBackendError> {
        self.state.lock().unwrap().calls.push(Call::Resize {
            key: key.clone(),
            cols,
            rows,
        });
        Ok(())
    }

    fn pane_shell(&self, key: &SessionKey) -> Result<Option<String>, PtyBackendError> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::PaneShell { key: key.clone() });
        match st.pane_shell_results.pop_front() {
            Some(r) => r,
            None => Ok(st.pane_shell.clone()),
        }
    }

    /// The entry [`FakeBackend::set_agent`] set, or `None`. Recorded as
    /// [`Call::AgentSnapshot`].
    fn agent_snapshot(&self, key: &SessionKey) -> Result<Option<AgentObs>, PtyBackendError> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::AgentSnapshot { key: key.clone() });
        Ok(st.agents.get(key).cloned())
    }

    fn run_line(&self, key: &SessionKey, gen: u64, line: &str) -> Result<(), PtyBackendError> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::RunLine {
            key: key.clone(),
            gen,
            line: line.to_string(),
        });
        st.run_line_results.pop_front().unwrap_or(Ok(()))
    }

    fn kill(&self, key: &SessionKey, gen: u64) -> Result<(), PtyBackendError> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::Kill {
            key: key.clone(),
            gen,
        });
        st.kill_results.pop_front().unwrap_or(Ok(()))
    }

    fn list_sessions(&self) -> Result<Vec<SessionSummary>, PtyBackendError> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::ListSessions);
        match st.list_sessions_results.pop_front() {
            Some(r) => r,
            None => Ok(st.sessions.clone()),
        }
    }

    /// Not logged: read on many paths whose call logs tests assert exactly.
    fn terminated_keys(&self, project: &str) -> HashSet<String> {
        self.state
            .lock()
            .unwrap()
            .terminated
            .iter()
            .filter(|k| k.project == project)
            .map(|k| k.worktree.clone())
            .collect()
    }

    fn set_terminated(&self, key: &SessionKey, terminated: bool) -> Result<(), PtyBackendError> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::SetTerminated {
            key: key.clone(),
            terminated,
        });
        if st.set_terminated_fails {
            return Err(PtyBackendError::Other("store unavailable".into()));
        }
        if terminated {
            st.terminated.insert(key.clone());
        } else {
            st.terminated.remove(key);
        }
        Ok(())
    }

    fn live_set_snapshot(&self) -> HashSet<SessionKey> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::LiveSetSnapshot);
        st.live.clone()
    }

    fn is_live(&self, key: &SessionKey) -> bool {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::IsLive { key: key.clone() });
        st.is_live_count += 1;
        if let (Some(n), Some(fk)) = (st.live_flip_after, st.live_flip_key.clone()) {
            if st.is_live_count >= n {
                st.live.remove(&fk);
            }
        }
        st.live.contains(key)
    }

    fn status_payload_snapshot(&self) -> StatusPayload {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(Call::StatusPayloadSnapshot);
        let connected = self.state.lock().unwrap().connected;
        StatusPayload {
            state: if connected {
                "Connected"
            } else {
                "Disconnected"
            },
            pid: None,
            started_at: None,
            daemon_version: None,
            session_count: None,
            reason: None,
            restartable: true,
            log_path: "~/.keitora/herdr.log".to_string(),
        }
    }

    fn shutdown(&self) -> Result<(), PtyBackendError> {
        self.state.lock().unwrap().calls.push(Call::Shutdown);
        Ok(())
    }

    fn restart(&self) -> Result<(), PtyBackendError> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::Restart);
        st.restart_results.pop_front().unwrap_or(Ok(()))
    }

    fn recovery_snapshot(&self) -> super::herdr::gate::RecoveryPayload {
        self.state
            .lock()
            .unwrap()
            .recovery
            .clone()
            .unwrap_or_else(super::herdr::gate::RecoveryPayload::healthy)
    }

    fn floor_slot(&self) -> Option<Arc<crate::deps::floor::FloorSlot>> {
        self.state.lock().unwrap().floor.clone()
    }

    fn refresh_integration(&self) {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(Call::RefreshIntegration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::LaunchPolicy;

    #[test]
    fn agent_snapshot_answers_the_set_agent_and_is_recorded() {
        let b = FakeBackend::new();
        let key = SessionKey {
            project: "proj".into(),
            worktree: "alpha".into(),
        };
        assert_eq!(b.agent_snapshot(&key).unwrap(), None);
        let o = crate::engine::model::obs("w1:p1", "idle", 3, Some(3));
        b.set_agent(key.clone(), Some(o.clone()));
        assert_eq!(b.agent_snapshot(&key).unwrap(), Some(o));
        b.set_agent(key.clone(), None);
        assert_eq!(b.agent_snapshot(&key).unwrap(), None);
        assert_eq!(b.calls(), vec![Call::AgentSnapshot { key: key.clone() }; 3]);
    }

    #[test]
    fn spawn_agent_records_the_launch_and_returns_generation_one() {
        let fake = FakeBackend::new();
        let key = SessionKey {
            project: "proj".into(),
            worktree: "sayo".into(),
        };
        let launch = AgentLaunch {
            cli: keitora_tasks::CliKind::Codex,
            path: "codex".into(),
            autonomy: true,
            resume_target: Some("thr_1".into()),
            fresh: false,
            policy: LaunchPolicy::Default,
        };
        let gen = fake
            .spawn_agent(
                key.clone(),
                "/w".into(),
                "/db".into(),
                Some((80, 24)),
                launch.clone(),
            )
            .unwrap();
        assert_eq!(gen, 1);
        assert_eq!(
            fake.calls(),
            vec![Call::SpawnAgent {
                key,
                cwd: "/w".into(),
                db_path: "/db".into(),
                size: Some((80, 24)),
                launch,
            }]
        );
    }

    #[test]
    fn pane_shell_and_run_line_answer_from_scripts_and_are_recorded() {
        let fake = FakeBackend::new();
        let key = SessionKey {
            project: "proj".into(),
            worktree: "wt".into(),
        };
        assert_eq!(fake.pane_shell(&key).unwrap(), None);
        fake.set_pane_shell(Some("pwsh.exe"));
        fake.push_pane_shell(Err(PtyBackendError::NoSuchSession));
        assert!(matches!(
            fake.pane_shell(&key),
            Err(PtyBackendError::NoSuchSession)
        ));
        assert_eq!(fake.pane_shell(&key).unwrap().as_deref(), Some("pwsh.exe"));
        assert!(fake.run_line(&key, 3, "echo hi").is_ok());
        fake.push_run_line(Err(PtyBackendError::NoSuchSession));
        assert!(matches!(
            fake.run_line(&key, 4, "echo again"),
            Err(PtyBackendError::NoSuchSession)
        ));
        assert_eq!(
            fake.calls(),
            vec![
                Call::PaneShell { key: key.clone() },
                Call::PaneShell { key: key.clone() },
                Call::PaneShell { key: key.clone() },
                Call::RunLine {
                    key: key.clone(),
                    gen: 3,
                    line: "echo hi".into(),
                },
                Call::RunLine {
                    key,
                    gen: 4,
                    line: "echo again".into(),
                },
            ]
        );
    }

    #[test]
    fn paste_records_the_call_and_returns_the_scripted_result() {
        let fake = FakeBackend::new();
        let key = SessionKey {
            project: "proj".into(),
            worktree: "wt".into(),
        };
        assert!(fake.paste(&key, "a\rb").is_ok());
        fake.push_paste(Err(PtyBackendError::PasteTooLarge { bytes: 2, limit: 1 }));
        assert!(matches!(
            fake.paste(&key, "xy"),
            Err(PtyBackendError::PasteTooLarge { bytes: 2, limit: 1 })
        ));
        assert_eq!(
            fake.calls(),
            vec![
                Call::Paste {
                    key: key.clone(),
                    text: "a\rb".into(),
                },
                Call::Paste {
                    key,
                    text: "xy".into(),
                },
            ]
        );
    }
}
