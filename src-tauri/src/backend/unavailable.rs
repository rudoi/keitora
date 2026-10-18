//! `UnavailableBackend` — the [`PtyBackend`] the app runs on when herdr cannot be
//! constructed (user config unreadable, or the isolated herdr env cannot be set
//! up). A herdr executable that is merely not found does not end up here: the
//! herdr backend runs, reports it, and looks again on restart.
//!
//! It holds no sessions and starts nothing. It reports `Disconnected` on
//! `daemon://status` with the reason and `restartable: false`, so the user sees
//! why sessions are off, and refuses every session RPC. Recovering means fixing
//! the cause and relaunching Keitora: `restart` is a no-op and there is no
//! runtime swap to herdr.
//!
//! It never returns `RecoveryPending` / `RecoveryPaused`: those park a pane at
//! "recovering" waiting for a recovery that will never come.

use std::collections::HashSet;
use std::sync::Arc;

use super::herdr::status_loop::STATUS_EVENT;
use super::{
    AgentLaunch, AttachResult, EmitSink, OutputSink, PtyBackend, PtyBackendError, SessionKey,
    SessionSummary, StatusPayload,
};

/// The user-facing text for a herdr that could not be set up: the short
/// statement first, then the cause, then what to do (the same order as the herdr
/// backend's "herdr was not found" message).
pub fn unavailable_message(cause: &str) -> String {
    format!(
        "herdr is unavailable: {}. Fix this, then relaunch Keitora.",
        cause.trim_end_matches('.')
    )
}

/// A backend that refuses everything because herdr is unavailable.
pub struct UnavailableBackend {
    /// Why herdr could not be constructed (the construction error's text).
    reason: String,
    sink: Arc<dyn EmitSink>,
}

impl UnavailableBackend {
    pub fn new(sink: Arc<dyn EmitSink>, reason: String) -> Self {
        Self { reason, sink }
    }

    /// The error a spawn or attach gets: `Other`, so the frontend fails the pane
    /// at once and shows the text (rather than retrying as for `NotConnected`).
    fn refused(&self) -> PtyBackendError {
        PtyBackendError::Other(format!("herdr unavailable: {}", self.reason))
    }
}

impl PtyBackend for UnavailableBackend {
    /// Emit the one `Disconnected` status; nothing else ever changes it.
    fn start(&self) {
        if let Ok(value) = serde_json::to_value(self.status_payload_snapshot()) {
            self.sink.emit(STATUS_EVENT, value);
        }
    }

    fn spawn(
        &self,
        _key: SessionKey,
        _cwd: String,
        _db_path: String,
        _size: Option<(u16, u16)>,
        _command: Option<String>,
    ) -> Result<u64, PtyBackendError> {
        Err(self.refused())
    }

    fn spawn_agent(
        &self,
        _key: SessionKey,
        _cwd: String,
        _db_path: String,
        _size: Option<(u16, u16)>,
        _launch: AgentLaunch,
    ) -> Result<u64, PtyBackendError> {
        Err(self.refused())
    }

    fn attach(
        &self,
        _key: SessionKey,
        _size: Option<(u16, u16)>,
        _out: Arc<dyn OutputSink>,
    ) -> Result<AttachResult, PtyBackendError> {
        Err(self.refused())
    }

    /// Nothing is ever attached, so there is nothing to release.
    fn detach(&self, _key: SessionKey, _attach_id: Option<u64>) -> Result<(), PtyBackendError> {
        Ok(())
    }

    fn write(&self, _key: &SessionKey, _data: &[u8]) -> Result<(), PtyBackendError> {
        Err(PtyBackendError::NotConnected)
    }

    fn paste(&self, _key: &SessionKey, _text: &str) -> Result<(), PtyBackendError> {
        Err(PtyBackendError::NotConnected)
    }

    /// `NotConnected` maps to `Queued`, so dispatch keeps the task reserved
    /// rather than dropping it.
    fn write_line(
        &self,
        _key: &SessionKey,
        _text: &str,
        _submit_delay_ms: u32,
    ) -> Result<(), PtyBackendError> {
        Err(PtyBackendError::NotConnected)
    }

    fn resize(&self, _key: &SessionKey, _cols: u16, _rows: u16) -> Result<(), PtyBackendError> {
        Err(PtyBackendError::NotConnected)
    }

    fn repaint(&self, _key: &SessionKey) -> Result<(), PtyBackendError> {
        Err(PtyBackendError::NotConnected)
    }

    fn kill(&self, _key: &SessionKey, _gen: u64) -> Result<(), PtyBackendError> {
        Err(PtyBackendError::NotConnected)
    }

    fn list_sessions(&self) -> Result<Vec<SessionSummary>, PtyBackendError> {
        Ok(Vec::new())
    }

    fn terminated_keys(&self, _project: &str) -> HashSet<String> {
        HashSet::new()
    }

    /// There is no store to persist the flag in.
    fn set_terminated(&self, _key: &SessionKey, _terminated: bool) -> Result<(), PtyBackendError> {
        Err(self.refused())
    }

    fn live_set_snapshot(&self) -> HashSet<SessionKey> {
        HashSet::new()
    }

    fn is_live(&self, _key: &SessionKey) -> bool {
        false
    }

    fn status_payload_snapshot(&self) -> StatusPayload {
        StatusPayload {
            state: "Disconnected",
            pid: None,
            started_at: None,
            daemon_version: None,
            session_count: None,
            reason: Some(unavailable_message(&self.reason)),
            // Restart is a no-op here: only a relaunch constructs herdr again.
            restartable: false,
            // No log to reveal; the frontend hides "reveal log" when empty.
            log_path: String::new(),
        }
    }

    fn shutdown(&self) -> Result<(), PtyBackendError> {
        Ok(())
    }

    /// No-op: herdr is only picked up by relaunching Keitora.
    fn restart(&self) -> Result<(), PtyBackendError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{map_write_line_result, DaemonStats, WriteLineOutcome};
    use serde_json::Value;
    use std::sync::Mutex;

    /// Capturing [`EmitSink`]: every event emitted, in order.
    #[derive(Default)]
    struct CaptureSink(Mutex<Vec<(String, Value)>>);
    impl EmitSink for CaptureSink {
        fn emit(&self, event: &str, payload: Value) {
            self.0.lock().unwrap().push((event.to_string(), payload));
        }
    }

    /// An [`OutputSink`] that must never be used.
    struct NoOutput;
    impl OutputSink for NoOutput {
        fn frame(&self, _b64: String, _full: bool) {
            panic!("an unavailable backend delivers no output");
        }
        fn exit(&self) {
            panic!("an unavailable backend delivers no output");
        }
    }

    const REASON: &str = "herdr not found on PATH";

    fn backend() -> (UnavailableBackend, Arc<CaptureSink>) {
        let sink = Arc::new(CaptureSink::default());
        let b = UnavailableBackend::new(sink.clone(), REASON.to_string());
        (b, sink)
    }

    fn key() -> SessionKey {
        SessionKey {
            project: "proj".into(),
            worktree: "sayo".into(),
        }
    }

    fn assert_refused<T: std::fmt::Debug>(r: Result<T, PtyBackendError>) {
        match r {
            Err(PtyBackendError::Other(m)) => {
                assert_eq!(m, format!("herdr unavailable: {REASON}"))
            }
            other => panic!("expected Other(herdr unavailable), got {other:?}"),
        }
    }

    #[test]
    fn spawn_spawn_agent_and_attach_are_refused_with_the_reason() {
        let (b, _) = backend();
        assert_refused(b.spawn(key(), "/w".into(), "/db".into(), Some((80, 24)), None));
        assert_refused(b.spawn(
            key(),
            "/w".into(),
            "/db".into(),
            Some((80, 24)),
            Some("echo hi".into()),
        ));
        let launch = AgentLaunch {
            cli: keitora_tasks::CliKind::Claude,
            path: "claude".into(),
            autonomy: false,
            resume_target: None,
            fresh: false,
        };
        assert_refused(b.spawn_agent(key(), "/w".into(), "/db".into(), Some((80, 24)), launch));
        assert_refused(b.attach(key(), Some((80, 24)), Arc::new(NoOutput)));
    }

    #[test]
    fn terminate_flags_are_refused_and_empty() {
        let (b, _) = backend();
        assert_refused(b.set_terminated(&key(), true));
        assert!(b.terminated_keys("proj").is_empty());
    }

    #[test]
    fn session_rpcs_are_not_connected() {
        let (b, _) = backend();
        let k = key();
        assert!(matches!(
            b.write(&k, b"ls\r"),
            Err(PtyBackendError::NotConnected)
        ));
        assert!(matches!(
            b.paste(&k, "pasted"),
            Err(PtyBackendError::NotConnected)
        ));
        assert!(matches!(
            b.write_line(&k, "do it", 50),
            Err(PtyBackendError::NotConnected)
        ));
        assert!(matches!(
            b.resize(&k, 100, 30),
            Err(PtyBackendError::NotConnected)
        ));
        assert!(matches!(b.repaint(&k), Err(PtyBackendError::NotConnected)));
        assert!(matches!(b.kill(&k, 3), Err(PtyBackendError::NotConnected)));
    }

    #[test]
    fn write_line_refusal_keeps_the_task_queued() {
        let (b, _) = backend();
        assert_eq!(
            map_write_line_result(&b.write_line(&key(), "do it", 50)),
            WriteLineOutcome::Queued
        );
    }

    #[test]
    fn detach_releases_nothing_and_succeeds() {
        let (b, _) = backend();
        assert!(b.detach(key(), None).is_ok());
        assert!(b.detach(key(), Some(7)).is_ok());
    }

    #[test]
    fn there_are_no_sessions() {
        let (b, _) = backend();
        assert!(b.list_sessions().unwrap().is_empty());
        assert!(b.cached_sessions().unwrap().is_empty());
        assert!(b.live_set_snapshot().is_empty());
        assert!(!b.is_live(&key()));
    }

    #[test]
    fn stats_and_recovery_are_the_trait_defaults() {
        let (b, _) = backend();
        assert_eq!(b.daemon_stats(), DaemonStats::default());
        assert_eq!(
            b.recovery_snapshot(),
            crate::backend::herdr::gate::RecoveryPayload::healthy()
        );
        assert!(b.recovery().is_none());
        assert_eq!(b.take_xdg_leak_notice(), None);
    }

    #[test]
    fn restart_and_shutdown_are_no_ops_that_emit_nothing() {
        let (b, sink) = backend();
        assert!(b.restart().is_ok());
        assert!(b.shutdown().is_ok());
        assert!(sink.0.lock().unwrap().is_empty());
    }

    #[test]
    fn status_snapshot_is_disconnected_with_the_reason() {
        let (b, _) = backend();
        assert_eq!(
            b.status_payload_snapshot(),
            StatusPayload {
                state: "Disconnected",
                pid: None,
                started_at: None,
                daemon_version: None,
                session_count: None,
                reason: Some(format!(
                    "herdr is unavailable: {REASON}. Fix this, then relaunch Keitora."
                )),
                restartable: false,
                log_path: String::new(),
            }
        );
    }

    #[test]
    fn start_emits_exactly_one_disconnected_status_with_the_reason() {
        let (b, sink) = backend();
        b.start();
        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        let (event, payload) = &events[0];
        assert_eq!(event, "daemon://status");
        assert_eq!(payload["state"], "Disconnected");
        assert_eq!(payload["log_path"], "");
        let reason = payload["reason"].as_str().expect("reason is a string");
        assert!(reason.contains(REASON), "{reason}");
        assert!(reason.contains("relaunch Keitora"), "{reason}");
        // The pushed event matches the pulled snapshot.
        assert_eq!(
            *payload,
            serde_json::to_value(b.status_payload_snapshot()).unwrap()
        );
    }

    #[test]
    fn unavailable_message_leads_with_the_statement_and_ends_with_the_remedy() {
        assert_eq!(
            unavailable_message("loading user config: bad toml"),
            "herdr is unavailable: loading user config: bad toml. Fix this, then relaunch Keitora."
        );
        // A cause that already ends its sentence gets no second full stop.
        assert_eq!(
            unavailable_message("the socket path is too long."),
            "herdr is unavailable: the socket path is too long. Fix this, then relaunch Keitora."
        );
    }

    #[test]
    fn status_json_omits_reason_when_none() {
        let payload = StatusPayload {
            state: "Connected",
            pid: Some(1),
            started_at: None,
            daemon_version: None,
            session_count: Some(0),
            reason: None,
            restartable: true,
            log_path: "/l".into(),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert!(json.get("reason").is_none(), "{json}");
        assert_eq!(
            json,
            serde_json::json!({
                "state": "Connected",
                "pid": 1,
                "session_count": 0,
                "restartable": true,
                "log_path": "/l",
            })
        );
    }
}
