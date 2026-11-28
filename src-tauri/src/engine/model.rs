//! The engine's data model: what herdr reported for a worktree's pane
//! ([`AgentObs`]), what the hooks wrote ([`HookEvidence`]), the derived per-worktree
//! [`View`], and the per-key [`KeyMemory`] carried between snapshots.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use crate::backend::herdr::cli::{AgentInfo, AgentSessionKind};
use crate::backend::herdr::ops::Generation;
use crate::backend::SessionKey;

/// One `herdr agent list` entry matched to a worktree's key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentObs {
    pub pane_id: String,
    pub terminal_id: String,
    /// herdr's agent label (`"claude"`, `"codex"`, `"agy"`, ...); `None` for a bare shell.
    pub agent: Option<String>,
    pub agent_status: String,
    pub launch_pending: bool,
    pub interactive_ready: Option<bool>,
    pub screen_detection_skipped: bool,
    /// A server-wide stamp of the latest completed-work idle; nullable, never a counter.
    pub completion_seq: Option<u64>,
    pub state_change_seq: u64,
    /// The agent session id or transcript path herdr recorded.
    pub agent_session: Option<String>,
    /// `"id"` or `"path"`: what [`AgentObs::agent_session`] holds.
    pub agent_session_kind: Option<String>,
    /// Who reported the session, e.g. `"herdr:claude"`.
    pub agent_session_source: Option<String>,
}

impl AgentObs {
    /// The engine's copy of one parsed `agent list` entry.
    pub fn from_info(info: &AgentInfo) -> Self {
        let session = info.agent_session.as_ref();
        AgentObs {
            pane_id: info.pane_id.clone(),
            terminal_id: info.terminal_id.clone(),
            agent: info.agent.clone(),
            agent_status: info.agent_status.clone(),
            launch_pending: info.launch_pending,
            interactive_ready: info.interactive_ready,
            screen_detection_skipped: info.screen_detection_skipped,
            completion_seq: info.completion_seq,
            state_change_seq: info.state_change_seq,
            agent_session: session.map(|s| s.value.clone()),
            agent_session_kind: session.map(|s| {
                match s.kind {
                    AgentSessionKind::Id => "id",
                    AgentSessionKind::Path => "path",
                }
                .to_string()
            }),
            agent_session_source: session.map(|s| s.source.clone()),
        }
    }
}

/// Whether an agent occupies the worktree's pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Presence {
    Present,
    Starting,
    Absent,
}

impl Presence {
    /// The wire spelling: `"present"` / `"starting"` / `"absent"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Presence::Present => "present",
            Presence::Starting => "starting",
            Presence::Absent => "absent",
        }
    }

    /// Read the wire spelling back. Anything but `"starting"` or `"absent"` reads as
    /// `Present`, so a reader never under-counts running agents.
    pub fn from_wire(s: &str) -> Presence {
        match s {
            "starting" => Presence::Starting,
            "absent" => Presence::Absent,
            _ => Presence::Present,
        }
    }
}

/// The questions every consumer asks of a view, answered one way for the engine's
/// [`View`] and for its published row ([`keitora_tasks::StatusSession`]). Nobody
/// compares `agent`/`state` strings on their own.
pub trait ViewQuery {
    fn presence(&self) -> Presence;
    /// `"idle"` | `"busy"` | `"waiting"` | `"offline"`.
    fn state_name(&self) -> &str;
    /// Idle confirmed by the settled-idle rule.
    fn settled_flag(&self) -> bool;
    fn input_ready(&self) -> Option<bool>;

    /// An agent occupies the pane: present or starting.
    fn is_running(&self) -> bool {
        self.presence() != Presence::Absent
    }

    /// The agent is starting, or present but not ready for input yet.
    fn is_starting(&self) -> bool {
        match self.presence() {
            Presence::Starting => true,
            Presence::Present => self.input_ready() == Some(false),
            Presence::Absent => false,
        }
    }

    /// The view reads idle (settled or not).
    fn is_idle(&self) -> bool {
        self.state_name() == "idle"
    }

    /// Idle, and confirmed so by the settled-idle rule (no hook veto, held across
    /// snapshots at an unchanged `state_change_seq`).
    fn is_idle_settled(&self) -> bool {
        self.is_idle() && self.settled_flag()
    }
}

impl ViewQuery for View {
    fn presence(&self) -> Presence {
        self.agent
    }
    fn state_name(&self) -> &str {
        self.state
    }
    fn settled_flag(&self) -> bool {
        self.settled
    }
    fn input_ready(&self) -> Option<bool> {
        self.interactive_ready
    }
}

impl ViewQuery for keitora_tasks::StatusSession {
    fn presence(&self) -> Presence {
        Presence::from_wire(&self.agent)
    }
    fn state_name(&self) -> &str {
        &self.state
    }
    fn settled_flag(&self) -> bool {
        self.settled
    }
    fn input_ready(&self) -> Option<bool> {
        self.interactive_ready
    }
}

/// The derived view of one `(project, worktree)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// `"idle"` | `"busy"` | `"waiting"` | `"offline"`.
    pub state: &'static str,
    pub agent: Presence,
    pub agent_kind: Option<String>,
    /// herdr's raw `agent_status`.
    pub herdr_status: Option<String>,
    /// Idle confirmed by the settled-idle rule.
    pub settled: bool,
    /// No fresh herdr snapshot backs this view.
    pub stale: bool,
    pub interactive_ready: Option<bool>,
    pub pane_id: Option<String>,
    pub terminal_id: Option<String>,
    pub agent_session: Option<String>,
    pub state_change_seq: Option<u64>,
    pub completion_seq: Option<u64>,
    pub brokered: bool,
    pub escalated_reason: Option<String>,
    /// Unix ms of the last change of any field above except `stale`.
    pub last_change_at_ms: i64,
}

impl View {
    /// Whether `self` and `other` agree on every field except `stale` and
    /// `last_change_at_ms`.
    pub fn same_content(&self, other: &View) -> bool {
        let strip = |v: &View| View {
            stale: false,
            last_change_at_ms: 0,
            ..v.clone()
        };
        strip(self) == strip(other)
    }

    /// The `status.db` row for this view under `key`.
    pub fn to_status(&self, key: &SessionKey) -> keitora_tasks::StatusSession {
        let seq = |s: Option<u64>| s.map(|v| i64::try_from(v).unwrap_or(i64::MAX));
        keitora_tasks::StatusSession {
            project: key.project.clone(),
            worktree: key.worktree.clone(),
            state: self.state.to_string(),
            agent: self.agent.as_str().to_string(),
            agent_kind: self.agent_kind.clone(),
            herdr_status: self.herdr_status.clone(),
            settled: self.settled,
            stale: self.stale,
            interactive_ready: self.interactive_ready,
            pane_id: self.pane_id.clone(),
            terminal_id: self.terminal_id.clone(),
            agent_session: self.agent_session.clone(),
            state_change_seq: seq(self.state_change_seq),
            completion_seq: seq(self.completion_seq),
            brokered: self.brokered,
            escalated_reason: self.escalated_reason.clone(),
            last_change_at: self.last_change_at_ms,
        }
    }
}

/// Per-key state carried between snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyMemory {
    pub view: View,
    /// The first idle/done snapshot of the current idle run and its `state_change_seq`.
    pub idle_first: Option<(Instant, u64)>,
    /// When the current `state_change_seq` was first seen (monotonic, wall ms).
    pub herdr_changed: (Instant, i64),
    pub last_scs: Option<u64>,
    pub unknown_since: Option<Instant>,
    /// Wall-ms twin of [`KeyMemory::unknown_since`].
    pub unknown_since_ms: Option<i64>,
    /// Wall ms the agent became `Present`; `None` while `Absent` or `Starting`.
    pub present_since_ms: Option<i64>,
    /// The keymap generation this memory belongs to.
    pub generation: Generation,
}

/// One `worktree_sessions` hook row, as evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookEvidence {
    pub state: String,
    /// `last_heartbeat` in unix ms; `None` when unparsable.
    pub written_ms: Option<i64>,
    pub brokered: bool,
    pub escalated_reason: Option<String>,
    /// The writer's session id; `None` when the column is absent or the writer sent none.
    pub session_id: Option<String>,
}

impl HookEvidence {
    /// The row was written by a session other than `live` (both ids known and
    /// different): a previous occupant of the pane, not evidence about this one.
    pub fn is_from_other_session(&self, live: Option<&str>) -> bool {
        matches!(
            (self.session_id.as_deref(), live),
            (Some(row), Some(live)) if row != live
        )
    }
}

/// One project's hook rows and configured worktrees, plus the drive state the turn
/// tracker reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectEvidence {
    pub db_path: String,
    pub configured: Vec<String>,
    pub rows: HashMap<String, HookEvidence>,
    /// Shared with the evidence cache: reused untouched while the DB is unchanged.
    pub drive: Arc<DriveEvidence>,
}

/// One project's drive state, as the turn tracker reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DriveEvidence {
    /// Drives with a send outstanding (`awaiting_turn` set) and no turn recorded yet.
    pub deliveries: Vec<keitora_tasks::Delivery>,
    /// Every driven worktree.
    pub driven: HashSet<String>,
    /// The hook-recorded last turn of each driven worktree that has one.
    pub replies: HashMap<String, keitora_tasks::SessionReply>,
}

/// Test helper: an agent entry for `pane` with `status` and the two seqs.
#[cfg(test)]
pub(crate) fn obs(pane: &str, status: &str, scs: u64, cs: Option<u64>) -> AgentObs {
    AgentObs {
        pane_id: pane.to_string(),
        terminal_id: format!("term-{pane}"),
        agent: Some("claude".to_string()),
        agent_status: status.to_string(),
        launch_pending: false,
        interactive_ready: Some(true),
        screen_detection_skipped: false,
        completion_seq: cs,
        state_change_seq: scs,
        agent_session: Some("s-1".to_string()),
        agent_session_kind: Some("id".to_string()),
        agent_session_source: Some("herdr:claude".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::herdr::cli::AgentSession;

    fn view() -> View {
        View {
            state: "busy",
            agent: Presence::Present,
            agent_kind: Some("codex".into()),
            herdr_status: Some("working".into()),
            settled: false,
            stale: true,
            interactive_ready: Some(true),
            pane_id: Some("w1:p1".into()),
            terminal_id: Some("term-1".into()),
            agent_session: Some("s-1".into()),
            state_change_seq: Some(7),
            completion_seq: None,
            brokered: false,
            escalated_reason: None,
            last_change_at_ms: 1_000,
        }
    }

    #[test]
    fn view_converts_to_a_status_row_for_its_key() {
        let key = SessionKey {
            project: "proj".into(),
            worktree: "alpha".into(),
        };
        let row = view().to_status(&key);
        assert_eq!(row.project, "proj");
        assert_eq!(row.worktree, "alpha");
        assert_eq!(row.state, "busy");
        assert_eq!(row.agent, "present");
        assert_eq!(row.agent_kind.as_deref(), Some("codex"));
        assert_eq!(row.herdr_status.as_deref(), Some("working"));
        assert!(row.stale);
        assert_eq!(row.state_change_seq, Some(7));
        assert_eq!(row.completion_seq, None);
        assert_eq!(row.pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(row.agent_session.as_deref(), Some("s-1"));
        assert_eq!(row.last_change_at, 1_000);
    }

    #[test]
    fn same_content_ignores_stale_and_last_change() {
        let a = view();
        let mut b = view();
        b.stale = false;
        b.last_change_at_ms = 9_999;
        assert!(a.same_content(&b));
        b.state = "idle";
        assert!(!a.same_content(&b));
    }

    #[test]
    fn presence_spells_lowercase() {
        assert_eq!(Presence::Starting.as_str(), "starting");
        assert_eq!(
            serde_json::to_string(&Presence::Present).unwrap(),
            "\"present\""
        );
    }

    #[test]
    fn presence_reads_its_wire_spelling_back() {
        for p in [Presence::Present, Presence::Starting, Presence::Absent] {
            assert_eq!(Presence::from_wire(p.as_str()), p);
        }
        assert_eq!(Presence::from_wire("something-new"), Presence::Present);
    }

    #[test]
    fn view_queries_agree_for_the_view_and_its_row() {
        let key = SessionKey {
            project: "proj".into(),
            worktree: "alpha".into(),
        };
        let cases = [
            // (agent, state, settled, interactive_ready, running, starting, idle_settled)
            (
                Presence::Present,
                "idle",
                true,
                Some(true),
                true,
                false,
                true,
            ),
            (
                Presence::Present,
                "idle",
                false,
                Some(true),
                true,
                false,
                false,
            ),
            (
                Presence::Present,
                "idle",
                true,
                Some(false),
                true,
                true,
                true,
            ),
            (Presence::Present, "busy", false, None, true, false, false),
            (Presence::Starting, "busy", false, None, true, true, false),
            (
                Presence::Absent,
                "offline",
                false,
                None,
                false,
                false,
                false,
            ),
        ];
        for (agent, state, settled, ready, running, starting, idle_settled) in cases {
            let v = View {
                agent,
                state,
                settled,
                interactive_ready: ready,
                ..view()
            };
            let row = v.to_status(&key);
            for (name, q) in [("view", &v as &dyn ViewQuery), ("row", &row)] {
                let case = format!("{name} {agent:?} {state} {settled} {ready:?}");
                assert_eq!(q.is_running(), running, "{case}");
                assert_eq!(q.is_starting(), starting, "{case}");
                assert_eq!(q.is_idle(), state == "idle", "{case}");
                assert_eq!(q.is_idle_settled(), idle_settled, "{case}");
            }
        }
    }

    #[test]
    fn agent_obs_copies_the_session_identity() {
        let info = AgentInfo {
            pane_id: "w1:p1".into(),
            terminal_id: "term-1".into(),
            agent: Some("agy".into()),
            agent_status: "idle".into(),
            launch_pending: false,
            interactive_ready: None,
            screen_detection_skipped: true,
            completion_seq: Some(4),
            state_change_seq: 9,
            agent_session: Some(AgentSession {
                source: "herdr:antigravity_cli".into(),
                agent: "agy".into(),
                kind: AgentSessionKind::Path,
                value: "/Users/me/s.db".into(),
            }),
            revision: 2,
            name: None,
            cwd: None,
        };
        let o = AgentObs::from_info(&info);
        assert_eq!(o.agent_session.as_deref(), Some("/Users/me/s.db"));
        assert_eq!(o.agent_session_kind.as_deref(), Some("path"));
        assert_eq!(
            o.agent_session_source.as_deref(),
            Some("herdr:antigravity_cli")
        );
        assert_eq!(o.completion_seq, Some(4));
        assert_eq!(o.state_change_seq, 9);
        assert!(o.screen_detection_skipped);

        let bare = AgentObs::from_info(&AgentInfo {
            agent_session: None,
            ..info
        });
        assert_eq!(bare.agent_session_kind, None);
        assert_eq!(bare.agent_session_source, None);
    }
}
