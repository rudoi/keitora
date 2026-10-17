//! `FakeBackend` — a scriptable [`PtyBackend`] for unit tests.
//!
//! Records every trait call in a log and serves scripted result queues for the
//! methods tests care about (`write_line`, `kill`, `list_sessions`), plus a hook to
//! flip a key out of the live-set after N `is_live` calls (to simulate a session
//! exiting). Needs no `AppHandle`. The whole module is `#[cfg(test)]` (declared so
//! in `backend/mod.rs`).
//!
//! `PtyBackendError` is not `Clone`, so scripted results are built per test and
//! consumed from `VecDeque`s rather than cloned.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use super::{
    AgentLaunch, AttachResult, OutputSink, PtyBackend, PtyBackendError, SessionKey, SessionSummary,
    StatusPayload,
};

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
}

#[derive(Default)]
struct FakeState {
    live: HashSet<SessionKey>,
    sessions: Vec<SessionSummary>,
    calls: Vec<Call>,
    write_line_results: VecDeque<Result<(), PtyBackendError>>,
    kill_results: VecDeque<Result<(), PtyBackendError>>,
    list_sessions_results: VecDeque<Result<Vec<SessionSummary>, PtyBackendError>>,
    is_live_count: u32,
    live_flip_after: Option<u32>,
    live_flip_key: Option<SessionKey>,
    terminated: HashSet<SessionKey>,
    set_terminated_fails: bool,
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

    /// Script the next `kill` result.
    pub fn push_kill(&self, r: Result<(), PtyBackendError>) {
        self.state.lock().unwrap().kill_results.push_back(r);
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
        Ok(1)
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

    fn resize(&self, key: &SessionKey, cols: u16, rows: u16) -> Result<(), PtyBackendError> {
        self.state.lock().unwrap().calls.push(Call::Resize {
            key: key.clone(),
            cols,
            rows,
        });
        Ok(())
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
        StatusPayload {
            state: "Disconnected",
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
        self.state.lock().unwrap().calls.push(Call::Restart);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
