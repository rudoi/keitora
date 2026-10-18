//! `cli` — the pure types and parsers for the `herdr` control-plane CLI output.
//!
//! The process side (spawning `herdr`, capturing stdout) lives in
//! [`super::real::RealHerdr`]; this module only turns herdr's JSON / text output
//! into typed values and decides how herdr error codes read to the backend.
//!
//! Error codes map to [`PtyBackendError`]: `*_not_found` → `NoSuchSession`,
//! everything else → `Other(message)`.

use serde_json::Value;

use crate::backend::PtyBackendError;

use super::ops::HerdrError;

/// The herdr identity returned by `workspace create` / observed in `pane list`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaneIdentity {
    pub workspace_id: String,
    pub pane_id: String,
    pub terminal_id: String,
    /// Pane working directory (the worktree path), when herdr reports it. Present on
    /// `pane list` entries (verified in the probe) and used by the startup
    /// restore reconciliation to re-adopt restored panes by cwd — herdr
    /// reassigns ids on a server-restart restore, so the stable key is the path.
    /// `None` when a pane object omits it (e.g. some `workspace create` root panes).
    pub cwd: Option<String>,
    /// herdr's effective agent label for the pane (`"claude"`, `"codex"`, ...); `None`
    /// once the agent process has exited and the pane is a bare shell.
    pub agent: Option<String>,
    /// The agent's status as herdr reports it (`idle`, `working`, ...), verbatim.
    pub agent_status: Option<String>,
    /// The agent session herdr recorded for this pane (via its integration hook).
    /// Survives on a restored shell even after the agent itself is gone.
    pub agent_session: Option<AgentSession>,
    /// How many lines above the live tail the pane's viewport is scrolled
    /// (`scroll.offset_from_bottom`; `0` is the live tail). `None` when herdr
    /// omits it.
    pub scroll: Option<u32>,
    /// The pane's current row count (`scroll.viewport_rows`). `None` when herdr has
    /// no metrics for the pane yet, or reports zero.
    pub viewport_rows: Option<u16>,
}

/// The shared-interface name for a `pane list` entry.
pub type PaneInfo = PaneIdentity;

/// The agent session herdr recorded for a pane (`agent_session` in `pane list`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSession {
    /// Who reported it, e.g. `"herdr:claude"`.
    pub source: String,
    /// The agent the session belongs to (`"claude"`, `"codex"`).
    pub agent: String,
    pub kind: AgentSessionKind,
    /// The session id (for [`AgentSessionKind::Id`]) or transcript path.
    pub value: String,
}

/// What [`AgentSession::value`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSessionKind {
    Id,
    Path,
}

// Nothing lists agents now (the session listing reads the pane list's agent
// status); the op stays for the agent-level detail (`launch_pending`) the pane
// list lacks.
#[cfg_attr(not(test), allow(dead_code))]
/// One `herdr agent list` entry (the subset Keitora uses).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentInfo {
    pub pane_id: String,
    pub agent: Option<String>,
    /// `idle | working | blocked | done | unknown`, verbatim; `"unknown"` if absent.
    pub agent_status: String,
    pub launch_pending: bool,
}

/// The subset of `herdr status server --json` the backend surfaces.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerStatus {
    pub running: bool,
    pub version: Option<String>,
    pub protocol: Option<u64>,
    pub socket: Option<String>,
}

/// State of one herdr agent integration (hook/extension installed into the agent's
/// global config).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrationStatus {
    Installed,
    NotInstalled,
    Outdated,
    /// Target line missing or wording not recognised. Callers must NOT install.
    Unknown,
}

/// The OS processes rooted in one pane (`herdr pane process-info`): the pane's
/// shell, plus the foreground processes herdr reports for it. A foreground pid
/// may sit outside the shell's process tree (e.g. an agent launched through Git
/// Bash on Windows), which is why both are kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneProcessInfo {
    pub shell_pid: u32,
    /// Distinct foreground pids, in herdr's order, never including `shell_pid`.
    pub foreground_pids: Vec<u32>,
}

/// Read a herdr `error` JSON object (`{"code","message"}`) as an
/// [`HerdrError::Api`]. A missing code reads as `""`, a missing message as
/// `"unknown herdr error"`.
pub fn api_error_from_value(err: &Value) -> HerdrError {
    HerdrError::Api {
        code: err
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        message: err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown herdr error")
            .to_string(),
    }
}

/// Map a herdr error `(code, message)` to a [`PtyBackendError`]: any `*_not_found`
/// becomes [`PtyBackendError::NoSuchSession`]; everything else is `Other`.
pub fn map_error(code: &str, message: &str) -> PtyBackendError {
    if code.ends_with("_not_found") {
        PtyBackendError::NoSuchSession
    } else {
        PtyBackendError::Other(format!("herdr: {message}"))
    }
}

/// Parse a herdr pane object (`root_pane` or a `pane list` entry) into a
/// [`PaneIdentity`]; `None` if any required id is missing. The agent fields are
/// optional: a missing or malformed key leaves the field `None`, never an error.
pub fn parse_pane_identity(value: &Value) -> Option<PaneIdentity> {
    let opt_str = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    Some(PaneIdentity {
        workspace_id: value.get("workspace_id")?.as_str()?.to_string(),
        pane_id: value.get("pane_id")?.as_str()?.to_string(),
        terminal_id: value.get("terminal_id")?.as_str()?.to_string(),
        cwd: opt_str("cwd"),
        agent: opt_str("agent"),
        agent_status: opt_str("agent_status"),
        agent_session: value.get("agent_session").and_then(parse_agent_session),
        scroll: value
            .pointer("/scroll/offset_from_bottom")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        viewport_rows: value
            .pointer("/scroll/viewport_rows")
            .and_then(Value::as_u64)
            .map(|n| u16::try_from(n).unwrap_or(u16::MAX))
            .filter(|&n| n > 0),
    })
}

/// Parse a pane's `agent_session` object. `None` if a field is missing or `kind` is
/// neither `"id"` nor `"path"`.
fn parse_agent_session(value: &Value) -> Option<AgentSession> {
    let field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    let kind = match value.get("kind").and_then(Value::as_str)? {
        "id" => AgentSessionKind::Id,
        "path" => AgentSessionKind::Path,
        _ => return None,
    };
    Some(AgentSession {
        source: field("source")?,
        agent: field("agent")?,
        kind,
        value: field("value")?,
    })
}

/// Parse a `pane process-info` response. The payload sits at
/// `/result/process_info`; a bare `/process_info` is read when there is no
/// `result` envelope. `shell_pid` is required (and must fit a `u32`), so a
/// payload without one is `None`. `foreground_processes` is optional: each
/// entry's `pid` is collected (entries without a valid one are skipped), with
/// duplicates and the shell pid dropped. Every other field is ignored.
pub fn parse_pane_process_info(value: &Value) -> Option<PaneProcessInfo> {
    let info = match value.get("result") {
        Some(result) => result.get("process_info")?,
        None => value.get("process_info")?,
    };
    fn pid_of(v: &Value) -> Option<u32> {
        v.as_u64().and_then(|n| u32::try_from(n).ok())
    }
    let shell_pid = info.get("shell_pid").and_then(pid_of)?;
    let entries = info
        .get("foreground_processes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut foreground_pids: Vec<u32> = Vec::new();
    for pid in entries
        .iter()
        .filter_map(|entry| entry.get("pid").and_then(pid_of))
    {
        if pid != shell_pid && !foreground_pids.contains(&pid) {
            foreground_pids.push(pid);
        }
    }
    Some(PaneProcessInfo {
        shell_pid,
        foreground_pids,
    })
}

/// Pure decision for `agent start` failures: `agent_not_ready` / "blocked during
/// startup" means the agent launched (→ `Ok`); everything else stays an error.
pub fn agent_start_result_from(err: HerdrError) -> Result<(), HerdrError> {
    let blocked_msg = PtyBackendError::from(err.clone())
        .to_string()
        .contains("blocked during startup");
    if err.code() == Some("agent_not_ready") || blocked_msg {
        Ok(())
    } else {
        Err(err)
    }
}

/// Pure mapping for `agent prompt` failures as the backend reports them:
/// `agent_blocked` / `agent_not_ready` → `NoSuchSession` (the dispatcher queues the
/// line); anything else keeps its usual mapping.
pub fn agent_prompt_error_from(err: HerdrError) -> PtyBackendError {
    match err.code() {
        Some("agent_blocked") | Some("agent_not_ready") => PtyBackendError::NoSuchSession,
        _ => err.into(),
    }
}

/// Parse `herdr integration status` text for `target`. Pure.
pub fn parse_integration_status(output: &str, target: &str) -> IntegrationStatus {
    for line in output.lines() {
        let Some(rest) = line.trim().strip_prefix(target) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(':') else {
            continue; // e.g. target "claude" vs a longer name "claude-foo:"
        };
        let state = rest.trim().to_ascii_lowercase();
        return if state.starts_with("not installed") {
            IntegrationStatus::NotInstalled
        } else if state.contains("outdated")
            || state.contains("out of date")
            || state.contains("out-of-date")
        {
            IntegrationStatus::Outdated
        } else if state.starts_with("installed") {
            IntegrationStatus::Installed
        } else {
            IntegrationStatus::Unknown
        };
    }
    IntegrationStatus::Unknown
}

/// Parse one `agent list` entry; `None` if `pane_id` is missing. Other fields
/// default (`agent_status` → "unknown", `launch_pending` → false).
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_agent_info(value: &Value) -> Option<AgentInfo> {
    Some(AgentInfo {
        pane_id: value.get("pane_id")?.as_str()?.to_string(),
        agent: value
            .get("agent")
            .and_then(Value::as_str)
            .map(str::to_string),
        agent_status: value
            .get("agent_status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        launch_pending: value
            .get("launch_pending")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Parse a full `agent list` response (`/result/agents`), skipping bad entries.
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_agent_list(value: &Value) -> Vec<AgentInfo> {
    value
        .pointer("/result/agents")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(parse_agent_info).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_identity_reads_the_scroll_position() {
        let v = serde_json::json!({
            "workspace_id": "w1", "pane_id": "w1:p1", "terminal_id": "t1",
            "scroll": {"offset_from_bottom": 12, "max_offset_from_bottom": 400, "viewport_rows": 30},
        });
        assert_eq!(parse_pane_identity(&v).unwrap().scroll, Some(12));
    }

    #[test]
    fn pane_identity_without_scroll_still_parses() {
        let v = serde_json::json!({
            "workspace_id": "w1", "pane_id": "w1:p1", "terminal_id": "t1",
        });
        assert_eq!(parse_pane_identity(&v).unwrap().scroll, None);
        let bad = serde_json::json!({
            "workspace_id": "w1", "pane_id": "w1:p1", "terminal_id": "t1",
            "scroll": {"max_offset_from_bottom": 4},
        });
        assert_eq!(parse_pane_identity(&bad).unwrap().scroll, None);
    }

    #[test]
    fn pane_identity_reads_the_viewport_rows() {
        let v = serde_json::json!({
            "workspace_id": "w1", "pane_id": "w1:p1", "terminal_id": "t1",
            "scroll": {"offset_from_bottom": 0, "viewport_rows": 30},
        });
        assert_eq!(parse_pane_identity(&v).unwrap().viewport_rows, Some(30));
        let huge = serde_json::json!({
            "workspace_id": "w1", "pane_id": "w1:p1", "terminal_id": "t1",
            "scroll": {"viewport_rows": 100000},
        });
        assert_eq!(
            parse_pane_identity(&huge).unwrap().viewport_rows,
            Some(u16::MAX)
        );
    }

    #[test]
    fn pane_identity_viewport_rows_absent_or_zero_is_none() {
        let none = serde_json::json!({
            "workspace_id": "w1", "pane_id": "w1:p1", "terminal_id": "t1",
        });
        assert_eq!(parse_pane_identity(&none).unwrap().viewport_rows, None);
        let zero = serde_json::json!({
            "workspace_id": "w1", "pane_id": "w1:p1", "terminal_id": "t1",
            "scroll": {"viewport_rows": 0},
        });
        assert_eq!(parse_pane_identity(&zero).unwrap().viewport_rows, None);
    }

    #[test]
    fn error_code_not_found_maps_to_no_such_session() {
        assert!(matches!(
            map_error("pane_not_found", "pane w9:p9 not found"),
            PtyBackendError::NoSuchSession
        ));
        assert!(matches!(
            map_error("workspace_not_found", "nope"),
            PtyBackendError::NoSuchSession
        ));
    }

    #[test]
    fn error_code_other_maps_to_other_with_message() {
        let e = map_error("invalid_key", "unsupported key ctrl-c");
        assert!(matches!(e, PtyBackendError::Other(_)));
        assert_eq!(e.to_string(), "herdr: unsupported key ctrl-c");
    }

    #[test]
    fn map_error_value_reads_code_and_message() {
        let v: Value = serde_json::from_str(
            r#"{"error":{"code":"pane_not_found","message":"pane w9:p9 not found"},"id":"cli:pane:get"}"#,
        )
        .unwrap();
        let err = api_error_from_value(v.get("error").unwrap());
        assert_eq!(
            err,
            HerdrError::Api {
                code: "pane_not_found".into(),
                message: "pane w9:p9 not found".into(),
            }
        );
        assert!(matches!(
            PtyBackendError::from(err),
            PtyBackendError::NoSuchSession
        ));
        // Missing fields default rather than failing.
        assert_eq!(
            api_error_from_value(&serde_json::json!({})),
            HerdrError::Api {
                code: String::new(),
                message: "unknown herdr error".into(),
            }
        );
    }

    #[test]
    fn parse_pane_identity_from_workspace_create_root_pane() {
        // The exact shape herdr 0.9.3 prints for `workspace create`.
        let v: Value = serde_json::from_str(
            r#"{"id":"cli:workspace:create","result":{"root_pane":{"pane_id":"w2:p1","terminal_id":"term_65ce2a67135b32","workspace_id":"w2","tab_id":"w2:t1","scroll":{"viewport_rows":40}}}}"#,
        )
        .unwrap();
        let id = parse_pane_identity(v.pointer("/result/root_pane").unwrap()).unwrap();
        assert_eq!(id.pane_id, "w2:p1");
        assert_eq!(id.terminal_id, "term_65ce2a67135b32");
        assert_eq!(id.workspace_id, "w2");
    }

    #[test]
    fn parse_pane_identity_missing_field_is_none() {
        let v: Value = serde_json::from_str(r#"{"pane_id":"w2:p1","workspace_id":"w2"}"#).unwrap();
        assert!(parse_pane_identity(&v).is_none(), "missing terminal_id");
    }

    #[test]
    fn parse_pane_identity_carries_cwd_when_present() {
        // A `pane list` entry includes `cwd` (the worktree path); restore reconciliation matches on it.
        let v: Value = serde_json::from_str(
            r#"{"pane_id":"w2:p1","terminal_id":"t","workspace_id":"w2","cwd":"D:\\repos\\proj\\wt"}"#,
        )
        .unwrap();
        let id = parse_pane_identity(&v).unwrap();
        assert_eq!(id.cwd.as_deref(), Some(r"D:\repos\proj\wt"));
        // A pane object without `cwd` still parses (root panes may omit it).
        let v2: Value =
            serde_json::from_str(r#"{"pane_id":"w2:p1","terminal_id":"t","workspace_id":"w2"}"#)
                .unwrap();
        assert!(parse_pane_identity(&v2).unwrap().cwd.is_none());
    }

    #[test]
    fn parse_pane_identity_reads_agent_session() {
        // A full `pane list` entry as herdr 0.9.3 serializes it, plus an unknown key.
        let full: Value = serde_json::from_str(
            r#"{"pane_id":"w1:p1","terminal_id":"term_ab12","workspace_id":"w1","tab_id":"w1:t1",
                "focused":true,"cwd":"D:\\repos\\wt","agent":"claude","title":"t","agent_status":"idle",
                "agent_session":{"source":"herdr:claude","agent":"claude","kind":"id","value":"3f2a-uuid"},
                "scroll":{"offset_from_bottom":0},"revision":12,"some_future_key":{"x":1}}"#,
        )
        .unwrap();
        let id = parse_pane_identity(&full).unwrap();
        assert_eq!(id.cwd.as_deref(), Some(r"D:\repos\wt"));
        assert_eq!(id.agent.as_deref(), Some("claude"));
        assert_eq!(id.agent_status.as_deref(), Some("idle"));
        assert_eq!(
            id.agent_session,
            Some(AgentSession {
                source: "herdr:claude".into(),
                agent: "claude".into(),
                kind: AgentSessionKind::Id,
                value: "3f2a-uuid".into(),
            })
        );

        // codex
        let codex: Value = serde_json::from_str(
            r#"{"pane_id":"w1:p2","terminal_id":"t","workspace_id":"w1","agent":"codex",
                "agent_session":{"source":"herdr:codex","agent":"codex","kind":"id","value":"thread-1"}}"#,
        )
        .unwrap();
        let s = parse_pane_identity(&codex).unwrap().agent_session.unwrap();
        assert_eq!(s.agent, "codex");
        assert_eq!(s.source, "herdr:codex");
        assert_eq!(s.kind, AgentSessionKind::Id);
        assert_eq!(s.value, "thread-1");

        // kind "path" parses as a path; an unknown kind drops the session.
        let path: Value = serde_json::from_str(
            r#"{"pane_id":"w1:p3","terminal_id":"t","workspace_id":"w1",
                "agent_session":{"source":"herdr:claude","agent":"claude","kind":"path","value":"/x.jsonl"}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_pane_identity(&path)
                .unwrap()
                .agent_session
                .map(|s| s.kind),
            Some(AgentSessionKind::Path)
        );
        let weird: Value = serde_json::from_str(
            r#"{"pane_id":"w1:p3","terminal_id":"t","workspace_id":"w1",
                "agent_session":{"source":"herdr:claude","agent":"claude","kind":"blob","value":"x"}}"#,
        )
        .unwrap();
        assert!(parse_pane_identity(&weird).unwrap().agent_session.is_none());

        // Missing agent_session, agent and agent_status all stay None (a bare shell).
        let bare: Value =
            serde_json::from_str(r#"{"pane_id":"w1:p4","terminal_id":"t","workspace_id":"w1"}"#)
                .unwrap();
        let id = parse_pane_identity(&bare).unwrap();
        assert!(id.agent.is_none());
        assert!(id.agent_status.is_none());
        assert!(id.agent_session.is_none());
    }

    #[test]
    fn parse_agent_list_full_and_tolerant() {
        let v: Value = serde_json::from_str(
            r#"{"id":"cli:agent:list","result":{"type":"agent_list","agents":[
              {"pane_id":"w1:p1","agent":"claude","agent_status":"working","launch_pending":true,"terminal_id":"t"},
              {"pane_id":"w1:p2"},
              {"agent_status":"idle"}
            ]}}"#,
        )
        .unwrap();
        let agents = parse_agent_list(&v);
        assert_eq!(agents.len(), 2, "entry without pane_id skipped");
        assert_eq!(
            agents[0],
            AgentInfo {
                pane_id: "w1:p1".into(),
                agent: Some("claude".into()),
                agent_status: "working".into(),
                launch_pending: true,
            }
        );
        assert_eq!(agents[1].agent, None);
        assert_eq!(agents[1].agent_status, "unknown");
        assert!(!agents[1].launch_pending);
    }

    #[test]
    fn parse_agent_list_empty_or_missing_is_empty() {
        let v: Value = serde_json::from_str(r#"{"result":{"agents":[]}}"#).unwrap();
        assert!(parse_agent_list(&v).is_empty());
        assert!(parse_agent_list(&Value::Null).is_empty());
    }

    fn coded(code: &str, message: &str) -> HerdrError {
        HerdrError::Api {
            code: code.into(),
            message: message.into(),
        }
    }

    #[test]
    fn agent_start_not_ready_is_ok() {
        let err = coded("agent_not_ready", "agent blocked during startup");
        assert!(agent_start_result_from(err).is_ok());
        // Message-only signal (no recognised code) also counts as launched.
        let err = coded("", "blocked during startup");
        assert!(agent_start_result_from(err).is_ok());
    }

    #[test]
    fn agent_start_pane_not_found_is_err() {
        let err = coded("pane_not_found", "pane w9:p9 not found");
        let got = agent_start_result_from(err.clone());
        assert_eq!(got, Err(err.clone()));
        assert!(matches!(
            PtyBackendError::from(err),
            PtyBackendError::NoSuchSession
        ));
    }

    #[test]
    fn agent_start_other_code_and_transport_are_err() {
        let err = coded("agent_start_timeout", "timed out");
        assert_eq!(agent_start_result_from(err.clone()), Err(err));
        let err = HerdrError::SpawnFailed {
            os_error: None,
            message: "nope".into(),
        };
        assert!(agent_start_result_from(err).is_err());
    }

    #[test]
    fn agent_prompt_blocked_and_not_ready_map_to_no_such_session() {
        for c in ["agent_blocked", "agent_not_ready"] {
            assert!(matches!(
                agent_prompt_error_from(coded(c, "blocked")),
                PtyBackendError::NoSuchSession
            ));
        }
    }

    #[test]
    fn agent_prompt_other_errors_pass_through() {
        assert!(matches!(
            agent_prompt_error_from(coded("invalid_params", "bad")),
            PtyBackendError::Other(_)
        ));
        assert!(matches!(
            agent_prompt_error_from(coded("pane_not_found", "gone")),
            PtyBackendError::NoSuchSession
        ));
    }

    #[test]
    fn pane_process_info_reads_the_enveloped_shape() {
        // A hand-written sample in the shape herdr documents, inside the
        // `result` envelope `RealHerdr::run` hands back.
        let v: Value = serde_json::from_str(
            r#"{"id":"cli:pane:process-info","result":{"type":"pane_process_info","process_info":{
                "pane_id":"w1:p1","shell_pid":4100,"foreground_process_group_id":4200,"tty":null,
                "foreground_processes":[
                  {"pid":4200,"name":"claude","argv0":"claude","argv":["claude"],"cmdline":"claude","cwd":"C:/Users/x/proj"},
                  {"pid":4300,"name":"node","argv0":"node","argv":["node"],"cmdline":"node","cwd":"C:/Users/x/proj"}
                ]}}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_pane_process_info(&v),
            Some(PaneProcessInfo {
                shell_pid: 4100,
                foreground_pids: vec![4200, 4300],
            })
        );
    }

    #[test]
    fn pane_process_info_reads_the_bare_shape() {
        let v = serde_json::json!({
            "type": "pane_process_info",
            "process_info": {"pane_id": "w1:p1", "shell_pid": 77},
        });
        assert_eq!(
            parse_pane_process_info(&v),
            Some(PaneProcessInfo {
                shell_pid: 77,
                foreground_pids: Vec::new(),
            }),
            "no foreground list is an empty one"
        );
    }

    #[test]
    fn pane_process_info_without_a_shell_pid_is_none() {
        for v in [
            serde_json::json!({"result": {"process_info": {"pane_id": "w1:p1"}}}),
            serde_json::json!({"process_info": {"shell_pid": "12"}}),
            serde_json::json!({"process_info": {"shell_pid": -1}}),
            serde_json::json!({"process_info": {"shell_pid": 5_000_000_000u64}}),
            // An envelope without a payload is not read as the bare shape.
            serde_json::json!({"result": {"pane": {}}, "process_info": {"shell_pid": 3}}),
            serde_json::json!({}),
            Value::Null,
        ] {
            assert_eq!(parse_pane_process_info(&v), None, "{v}");
        }
    }

    #[test]
    fn pane_process_info_skips_entries_without_a_pid() {
        let v = serde_json::json!({"result": {"process_info": {
            "shell_pid": 10,
            "foreground_processes": [
                {"name": "no-pid"},
                {"pid": "11"},
                {"pid": -4},
                {"pid": 5_000_000_000u64},
                {"pid": 12},
                "not-an-object",
            ],
        }}});
        assert_eq!(
            parse_pane_process_info(&v).unwrap().foreground_pids,
            vec![12]
        );
    }

    #[test]
    fn pane_process_info_drops_duplicates_and_the_shell_pid() {
        let v = serde_json::json!({"result": {"process_info": {
            "shell_pid": 10,
            "foreground_processes": [
                {"pid": 10}, {"pid": 30}, {"pid": 20}, {"pid": 30}, {"pid": 10}, {"pid": 20},
            ],
        }}});
        assert_eq!(
            parse_pane_process_info(&v),
            Some(PaneProcessInfo {
                shell_pid: 10,
                foreground_pids: vec![30, 20],
            })
        );
    }

    const STATUS_SAMPLE: &str = "pi: not installed (C:\\u\\.pi\\x.ts)\n\
claude: not installed (C:\\u\\.claude\\hooks\\herdr-agent-state.ps1)\n\
codex: installed (C:\\u\\.codex\\herdr-agent-state.ps1)\n\
claude-foo: installed (x)\n\
letta (experimental): outdated (y)\n";

    #[test]
    fn integration_status_parses_each_state() {
        use IntegrationStatus::*;
        assert_eq!(
            parse_integration_status(STATUS_SAMPLE, "claude"),
            NotInstalled
        );
        assert_eq!(parse_integration_status(STATUS_SAMPLE, "codex"), Installed);
        assert_eq!(
            parse_integration_status("claude: installed (outdated)\n", "claude"),
            Outdated
        );
        assert_eq!(
            parse_integration_status("codex: Out of date (p)\n", "codex"),
            Outdated
        );
    }

    #[test]
    fn integration_status_unknown_when_missing_or_unrecognised() {
        use IntegrationStatus::*;
        assert_eq!(parse_integration_status(STATUS_SAMPLE, "grok"), Unknown);
        assert_eq!(parse_integration_status("", "claude"), Unknown);
        assert_eq!(
            parse_integration_status("claude: weird (p)\n", "claude"),
            Unknown
        );
        // A target that is only a prefix of a longer name must not match.
        assert_eq!(
            parse_integration_status("claude-foo: installed (x)\n", "claude"),
            Unknown
        );
    }
}
