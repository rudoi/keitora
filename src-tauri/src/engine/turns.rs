//! Turn tracking for driven worktrees: when a delivered prompt's turn finished, from
//! herdr's `agent list` alone (no Stop hook needed), which completions were the
//! human's, and the reply text that goes with each.
//!
//! [`evaluate`] is the pure predicate over a delivery's [`Baseline`]. [`TurnMemory`]
//! carries one driven worktree's state between snapshots; its [`TurnMemory::step`]
//! turns one snapshot into rows to write ([`TurnInsert`]) and transcript lookups to
//! start ([`Lookup`]). The engine runs both outside its lock, and only the
//! `status.db` lease holder tracks turns at all.
//!
//! herdr's `completion_seq` is a nullable, server-wide stamp of the agent's latest
//! completed-work idle: it is compared for a difference only, never required to rise,
//! and may be absent after back-to-back prompts or `/clear`, so the
//! `state_change_seq` and observed-activity clauses carry those cases.
//!
//! agy reports a finished turn while its background tasks may still run; such a
//! herdr-only completion carries [`AGY_CAVEAT`] and the engine does not wait them out.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use keitora_tasks::{Baseline, CliKind};

use super::model::{AgentObs, ProjectEvidence, View, ViewQuery};
use super::SETTLE;
use crate::backend::SessionKey;

/// How long a herdr-detected completion waits for the Stop hook's reply.
pub const REPLY_WAIT: Duration = Duration::from_secs(3);
/// When, after a herdr-detected completion, the transcript is looked up.
pub const TRANSCRIPT_AFTER: Duration = Duration::from_secs(1);
/// A delivery whose target stays settled idle at its baseline this long saw no
/// activity: the prompt may not have been submitted.
pub const STALL_NO_ACTIVITY: Duration =
    Duration::from_secs(keitora_tasks::DRIVE_STALL_NO_ACTIVITY_S as u64);
/// Completions kept per worktree for the health monitor.
pub const RECENT_CAP: usize = 10;
/// The caveat on a herdr-only completion of an agy turn.
pub const AGY_CAVEAT: &str = "agy may still run background tasks";

/// Whose turn a completion was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnKind {
    /// The driver's delivered prompt.
    Driver,
    /// Something the human typed.
    Human,
}

impl TurnKind {
    /// The `drive_turns.kind` spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            TurnKind::Driver => "driver",
            TurnKind::Human => "human",
        }
    }
}

/// Which rule finished a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clause {
    /// `completion_seq` present and different from the baseline's.
    CompletionSeq,
    /// `state_change_seq` at least two past the baseline's.
    StateChangeSeq,
    /// `working` or `blocked` was seen after the delivery.
    ObservedActive,
    /// The Stop hook recorded the turn.
    StopHook,
    /// Another agent occupies the pane now (or none does): the turn ended unseen.
    OccupantChanged,
}

impl Clause {
    /// The `drive_turns.clause` spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Clause::CompletionSeq => "completion_seq",
            Clause::StateChangeSeq => "state_change_seq",
            Clause::ObservedActive => "observed_active",
            Clause::StopHook => "stop_hook",
            Clause::OccupantChanged => "occupant_changed",
        }
    }
}

/// A delivery's turn, judged on one snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnVerdict {
    Pending,
    Finished(Clause),
    /// Another agent occupies the pane now (or none does): the result is unknown.
    Restarted,
}

/// Pure: whether the turn delivered over `b` finished, given the agent herdr lists
/// now (`None`: no agent), whether the view is a settled idle (not vetoed by a hook
/// row), whether `working`/`blocked` was seen since the delivery, whether the Stop hook
/// recorded the turn, and whether the agent was absent on two snapshots a second apart.
pub fn evaluate(
    b: &Baseline,
    obs: Option<&AgentObs>,
    settled_idle: bool,
    active_seen: bool,
    hook_finished: bool,
    absent_twice: bool,
) -> TurnVerdict {
    if hook_finished {
        return TurnVerdict::Finished(Clause::StopHook);
    }
    let Some(o) = obs else {
        return if absent_twice {
            TurnVerdict::Restarted
        } else {
            TurnVerdict::Pending
        };
    };
    if !Occupant::of_obs(o).same_as(&Occupant::of_baseline(b))
        || o.state_change_seq < b.state_change_seq
    {
        return TurnVerdict::Restarted;
    }
    if !settled_idle {
        return TurnVerdict::Pending;
    }
    completion_clause(
        (b.completion_seq, b.state_change_seq),
        (o.completion_seq, o.state_change_seq),
        active_seen,
    )
    .map_or(TurnVerdict::Pending, TurnVerdict::Finished)
}

/// Pure: which clause, if any, says a settled idle at `now` completed work since
/// `prev` (both `(completion_seq, state_change_seq)`); `active` tells whether
/// `working`/`blocked` was seen in between.
fn completion_clause(
    prev: (Option<u64>, u64),
    now: (Option<u64>, u64),
    active: bool,
) -> Option<Clause> {
    if now.0.is_some() && now.0 != prev.0 {
        Some(Clause::CompletionSeq)
    } else if now.1 >= prev.1.saturating_add(2) {
        Some(Clause::StateChangeSeq)
    } else if active {
        Some(Clause::ObservedActive)
    } else {
        None
    }
}

/// Who occupies a pane: herdr's pane and terminal, and the agent session when known.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Occupant {
    pane: String,
    terminal: String,
    session: Option<String>,
}

impl Occupant {
    fn of_obs(o: &AgentObs) -> Self {
        Occupant {
            pane: o.pane_id.clone(),
            terminal: o.terminal_id.clone(),
            session: o.agent_session.clone(),
        }
    }

    fn of_baseline(b: &Baseline) -> Self {
        Occupant {
            pane: b.pane_id.clone(),
            terminal: b.terminal_id.clone(),
            session: b.agent_session.clone(),
        }
    }

    /// Same pane and terminal, and no two different known sessions (a session herdr
    /// learns or forgets later is still the same occupant).
    fn same_as(&self, other: &Occupant) -> bool {
        let other_session = matches!(
            (&self.session, &other.session),
            (Some(a), Some(b)) if a != b
        );
        self.pane == other.pane && self.terminal == other.terminal && !other_session
    }
}

/// What a transcript lookup is asked: one finished turn of one agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyQuery {
    pub key: SessionKey,
    /// herdr's agent label.
    pub agent: Option<String>,
    pub agent_session: Option<String>,
    /// `"id"` or `"path"`.
    pub agent_session_kind: Option<String>,
    /// The delivery (driver turn) or the previous settled idle (human turn), unix ms.
    pub delivered_at_ms: i64,
    /// When the completion was detected, unix ms.
    pub detected_at_ms: i64,
}

/// A reply read from an agent's transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnReply {
    pub text: String,
}

/// Reads a finished turn's reply from the agent's own transcript.
pub trait TranscriptSource: Send + Sync {
    /// The reply, or why there is none (`not_found`, `not_this_turn`,
    /// `parse_failed`, `no_provider_support`).
    fn reply(&self, q: &ReplyQuery) -> Result<TurnReply, &'static str>;
}

/// No transcript support: every lookup misses.
pub struct NoTranscript;

impl TranscriptSource for NoTranscript {
    fn reply(&self, _q: &ReplyQuery) -> Result<TurnReply, &'static str> {
        Err("no_provider_support")
    }
}

/// Where a pending reply's transcript lookup stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptState {
    NotStarted,
    InFlight,
    Done(Result<TurnReply, &'static str>),
}

/// The delivery a driven worktree is waiting on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryTrack {
    pub delivery_id: i64,
    pub baseline: Baseline,
    pub awaiting_turn: i64,
    /// `working`/`blocked` seen on a snapshot taken after the delivery (in memory
    /// only: a restart loses it).
    pub active_seen: bool,
    /// The first snapshot of the current run without an agent.
    pub absent_since: Option<Instant>,
}

/// A finished turn waiting (up to [`REPLY_WAIT`]) for its reply text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReply {
    pub kind: TurnKind,
    pub delivery_id: Option<i64>,
    /// The delivery's `awaiting_turn` (driver turns): a hook turn at or past it is
    /// this turn's reply.
    pub awaiting_turn: Option<i64>,
    pub clause: Clause,
    pub detected_at: Instant,
    /// `(completion_seq, state_change_seq)` at the completion.
    pub identity: (Option<u64>, u64),
    /// `session_replies.turn` when the completion was detected.
    pub reply_turn_at_detection: i64,
    pub transcript: TranscriptState,
    /// Matches a lookup's result to this pending reply.
    pub token: u64,
    pub query: ReplyQuery,
}

/// One recorded completion, for the health monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionNote {
    pub kind: TurnKind,
    pub signal: &'static str,
    pub reply_source: &'static str,
    /// Why the transcript gave nothing, when it was asked and missed.
    pub transcript_miss: Option<&'static str>,
    pub at_ms: i64,
}

/// A worktree's turn facts for the health monitor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnHealth {
    /// The last [`RECENT_CAP`] recorded completions, oldest first.
    pub recent: Vec<CompletionNote>,
    /// `(delivery_id, wall ms)` of a delivery that saw no activity for
    /// [`STALL_NO_ACTIVITY`].
    pub no_activity: Option<(i64, i64)>,
}

/// The last settled completion the ledger counted, and the pane it was seen in.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ledger {
    identity: (Option<u64>, u64),
    occupant: Occupant,
}

/// One driven worktree's turn state, carried between snapshots.
#[derive(Clone, Debug, Default)]
pub struct TurnMemory {
    ledger: Option<Ledger>,
    /// `session_replies.turn` when the ledger last moved.
    pub ledger_reply_turn: i64,
    /// `working`/`blocked` seen since the ledger moved.
    pub active_since_ledger: bool,
    pub delivery: Option<DeliveryTrack>,
    pub pending_reply: Option<PendingReply>,
    /// Wall ms the ledger last moved (a human turn's lookup bound).
    pub ledger_moved_ms: i64,
    pub recent: VecDeque<CompletionNote>,
    pub no_activity: Option<(i64, i64)>,
    /// When the Stop hook resolved a driver turn before herdr showed its completion:
    /// the next completion the ledger counts is that turn's, not the human's. It lapses
    /// once herdr sits settled idle at the unchanged ledger for [`SETTLE`] after it (herdr
    /// saw nothing of that turn), and with any ledger move.
    claim_since: Option<Instant>,
    /// Rows whose write failed, offered again on every step until one lands. A driver
    /// row here keeps its delivery from being tracked (and judged) again meanwhile.
    unwritten: Vec<TurnInsert>,
    /// The last resolved delivery and whether activity was seen on it: a delivery that
    /// comes back under the same id resumes with that activity.
    resolved: Option<(i64, bool)>,
}

/// One snapshot's facts for one driven worktree.
pub struct TurnInput<'a> {
    pub key: &'a SessionKey,
    /// herdr's agent entry; `None` when no agent occupies the pane.
    pub obs: Option<&'a AgentObs>,
    /// The view derived from this snapshot.
    pub view: &'a View,
    pub evidence: &'a ProjectEvidence,
    pub at: Instant,
    pub at_ms: i64,
}

/// A `drive_turns` row to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnInsert {
    pub key: SessionKey,
    pub db_path: String,
    pub kind: TurnKind,
    pub delivery_id: Option<i64>,
    /// `finished` or `restarted`.
    pub outcome: &'static str,
    /// `herdr` or `hook`.
    pub signal: &'static str,
    pub clause: Clause,
    pub completion_seq: Option<u64>,
    pub state_change_seq: Option<u64>,
    pub reply: String,
    pub reply_source: &'static str,
    pub caveat: Option<&'static str>,
    pub transcript_miss: Option<&'static str>,
    pub at_ms: i64,
}

impl TurnInsert {
    /// A `finished`, herdr-signalled row for `i`'s worktree with no seqs and no reply;
    /// callers set the fields that differ.
    fn new(i: &TurnInput, kind: TurnKind, delivery_id: Option<i64>, clause: Clause) -> Self {
        TurnInsert {
            key: i.key.clone(),
            db_path: i.evidence.db_path.clone(),
            kind,
            delivery_id,
            outcome: "finished",
            signal: "herdr",
            clause,
            completion_seq: None,
            state_change_seq: None,
            reply: String::new(),
            reply_source: "none",
            caveat: None,
            transcript_miss: None,
            at_ms: i.at_ms,
        }
    }

    /// The store's input for this row.
    pub fn row(&self) -> keitora_tasks::NewDriveTurn<'_> {
        keitora_tasks::NewDriveTurn {
            driven: &self.key.worktree,
            kind: self.kind.as_str(),
            delivery_id: self.delivery_id,
            outcome: self.outcome,
            signal: self.signal,
            clause: self.clause.as_str(),
            completion_seq: self.completion_seq,
            state_change_seq: self.state_change_seq,
            reply: &self.reply,
            reply_source: self.reply_source,
            caveat: self.caveat,
        }
    }

    /// The health record of this row once written.
    pub fn note(&self) -> CompletionNote {
        CompletionNote {
            kind: self.kind,
            signal: self.signal,
            reply_source: self.reply_source,
            transcript_miss: self.transcript_miss,
            at_ms: self.at_ms,
        }
    }
}

/// A transcript lookup to start off the status loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lookup {
    pub token: u64,
    pub query: ReplyQuery,
}

/// What one step asks of the engine.
#[derive(Debug, Default)]
pub struct StepOut {
    pub inserts: Vec<TurnInsert>,
    pub lookups: Vec<Lookup>,
}

/// Whether `label` is herdr's name for the agy agent.
fn is_agy(label: Option<&str>) -> bool {
    label.and_then(CliKind::from_herdr_agent) == Some(CliKind::Antigravity)
}

impl TurnMemory {
    /// Whether the worktree should be polled hot: a delivery or a reply is pending.
    pub fn awaited(&self) -> bool {
        self.delivery.is_some() || self.pending_reply.is_some() || !self.unwritten.is_empty()
    }

    /// `ins` was written (or the store already had it): stop offering it.
    pub fn written(&mut self, ins: &TurnInsert) {
        self.unwritten.retain(|r| r != ins);
    }

    /// Writing `ins` failed: offer it again on the next step. Returns whether this is
    /// its first failure.
    pub fn write_failed(&mut self, ins: &TurnInsert) -> bool {
        if self.unwritten.contains(ins) {
            return false;
        }
        self.unwritten.push(ins.clone());
        true
    }

    /// The health facts.
    pub fn health(&self) -> TurnHealth {
        TurnHealth {
            recent: self.recent.iter().cloned().collect(),
            no_activity: self.no_activity,
        }
    }

    /// Record a written row's health note.
    pub fn push_note(&mut self, note: CompletionNote) {
        if self.recent.len() == RECENT_CAP {
            self.recent.pop_front();
        }
        self.recent.push_back(note);
        self.no_activity = None;
    }

    /// Run one snapshot: load the delivery, judge it, count the ledger's completions,
    /// and collect replies. `results` holds finished transcript lookups by token;
    /// `next_token` numbers new pending replies.
    pub fn step(
        &mut self,
        i: &TurnInput,
        results: &mut HashMap<u64, Result<TurnReply, &'static str>>,
        next_token: &mut u64,
        out: &mut StepOut,
    ) {
        let wt = i.key.worktree.as_str();
        let reply = i.evidence.drive.replies.get(wt);
        let reply_turn = reply.map_or(0, |r| r.turn);
        let active = i
            .obs
            .is_some_and(|o| matches!(o.agent_status.as_str(), "working" | "blocked"));
        let settled_idle = i.obs.is_some() && i.view.is_idle_settled();
        if active {
            self.active_since_ledger = true;
            self.no_activity = None;
        }
        out.inserts.extend(self.unwritten.iter().cloned());
        self.load_delivery(i);
        self.judge_delivery(i, active, settled_idle, reply, next_token, out);
        self.count_ledger(i, settled_idle, reply_turn, next_token, out);
        self.note_no_activity(i, settled_idle);
        self.collect_reply(i, reply, results, out);
    }

    /// Track the store's outstanding delivery for this worktree (one with no
    /// turn row yet), if it has a baseline. A changed `delivery_id` starts a fresh
    /// track.
    fn load_delivery(&mut self, i: &TurnInput) {
        let wt = i.key.worktree.as_str();
        let outstanding = i
            .evidence
            .drive
            .deliveries
            .iter()
            .find(|d| d.driven == wt)
            .and_then(|d| match (d.awaiting_turn, d.delivery_id, &d.baseline) {
                (Some(awaiting), Some(id), Some(b)) => Some((awaiting, id, b)),
                _ => None,
            })
            .filter(|&(_, id, _)| {
                !self
                    .unwritten
                    .iter()
                    .any(|r| r.kind == TurnKind::Driver && r.delivery_id == Some(id))
            });
        match outstanding {
            Some((awaiting, id, b)) => {
                if self.delivery.as_ref().map(|t| t.delivery_id) != Some(id) {
                    self.delivery = Some(DeliveryTrack {
                        delivery_id: id,
                        baseline: b.clone(),
                        awaiting_turn: awaiting,
                        active_seen: self.resolved.is_some_and(|(r, seen)| r == id && seen),
                        absent_since: None,
                    });
                }
            }
            None => self.delivery = None,
        }
        // A driver reply whose delivery is no longer outstanding has nothing to finish.
        let tracked = self.delivery.as_ref().map(|t| t.delivery_id);
        if self
            .pending_reply
            .as_ref()
            .is_some_and(|p| p.kind == TurnKind::Driver && p.delivery_id != tracked)
        {
            self.pending_reply = None;
        }
    }

    /// Judge the tracked delivery on this snapshot.
    fn judge_delivery(
        &mut self,
        i: &TurnInput,
        active: bool,
        settled_idle: bool,
        reply: Option<&keitora_tasks::SessionReply>,
        next_token: &mut u64,
        out: &mut StepOut,
    ) {
        let Some(t) = self.delivery.as_mut() else {
            return;
        };
        if active && i.at_ms > t.baseline.delivered_at_ms {
            t.active_seen = true;
        }
        if i.obs.is_some() {
            t.absent_since = None;
        } else {
            t.absent_since.get_or_insert(i.at);
        }
        if self
            .pending_reply
            .as_ref()
            .is_some_and(|p| p.kind == TurnKind::Driver)
        {
            return;
        }
        let absent_twice = t
            .absent_since
            .is_some_and(|s| i.at.saturating_duration_since(s) >= SETTLE);
        let hook_finished = reply.is_some_and(|r| r.turn >= t.awaiting_turn);
        let verdict = evaluate(
            &t.baseline,
            i.obs,
            settled_idle,
            t.active_seen,
            hook_finished,
            absent_twice,
        );
        let DeliveryTrack {
            delivery_id: id,
            awaiting_turn: awaiting,
            ref baseline,
            ..
        } = *t;
        let (agent, delivered_at_ms) = (baseline.agent.clone(), baseline.delivered_at_ms);
        // The row of a turn resolved on this snapshot, with herdr's seqs as seen now.
        let resolved = |clause| TurnInsert {
            completion_seq: i.obs.and_then(|o| o.completion_seq),
            state_change_seq: i.obs.map(|o| o.state_change_seq),
            ..TurnInsert::new(i, TurnKind::Driver, Some(id), clause)
        };
        match verdict {
            TurnVerdict::Pending => {}
            TurnVerdict::Restarted => {
                out.inserts.push(TurnInsert {
                    outcome: "restarted",
                    ..resolved(Clause::OccupantChanged)
                });
                self.resolve_delivery(id);
                // A new occupant starts a new ledger: its first settled idle is no turn.
                self.ledger = None;
            }
            TurnVerdict::Finished(Clause::StopHook) => {
                out.inserts.push(TurnInsert {
                    signal: "hook",
                    reply: reply.map(|r| r.last_message.clone()).unwrap_or_default(),
                    reply_source: "hook",
                    ..resolved(Clause::StopHook)
                });
                self.resolve_delivery(id);
                self.claim_since = Some(i.at);
            }
            TurnVerdict::Finished(clause) => {
                // A human turn still collecting its reply goes out with what it has.
                if self.pending_reply.is_some() {
                    self.flush_pending(i, reply, out);
                }
                let o = i.obs.expect("a finished verdict has an agent");
                self.pending_reply = Some(PendingReply {
                    kind: TurnKind::Driver,
                    delivery_id: Some(id),
                    awaiting_turn: Some(awaiting),
                    clause,
                    detected_at: i.at,
                    identity: (o.completion_seq, o.state_change_seq),
                    reply_turn_at_detection: reply.map_or(0, |r| r.turn),
                    transcript: TranscriptState::NotStarted,
                    token: take_token(next_token),
                    query: ReplyQuery {
                        key: i.key.clone(),
                        agent,
                        agent_session: o.agent_session.clone(),
                        agent_session_kind: o.agent_session_kind.clone(),
                        delivered_at_ms,
                        detected_at_ms: i.at_ms,
                    },
                });
            }
        }
    }

    /// Count a settled completion the ledger hasn't seen. Without an
    /// outstanding delivery (or a hook-resolved driver turn it belongs to) it is the
    /// human's.
    fn count_ledger(
        &mut self,
        i: &TurnInput,
        settled_idle: bool,
        reply_turn: i64,
        next_token: &mut u64,
        out: &mut StepOut,
    ) {
        let Some(o) = i.obs.filter(|_| settled_idle) else {
            return;
        };
        let now = Ledger {
            identity: (o.completion_seq, o.state_change_seq),
            occupant: Occupant::of_obs(o),
        };
        let clause = match &self.ledger {
            None => None,
            Some(l) if l.identity == now.identity && l.occupant.same_as(&now.occupant) => {
                if self
                    .claim_since
                    .is_some_and(|s| i.at.saturating_duration_since(s) >= SETTLE)
                {
                    self.claim_since = None;
                }
                return;
            }
            // Another occupant or a reset sequence: a new ledger, no event.
            Some(l) if !l.occupant.same_as(&now.occupant) || now.identity.1 < l.identity.1 => None,
            Some(l) => completion_clause(l.identity, now.identity, self.active_since_ledger),
        };
        let driver_owned = self.claim_since.is_some()
            || self.delivery.is_some()
            || self
                .pending_reply
                .as_ref()
                .is_some_and(|p| p.kind == TurnKind::Driver);
        if let (Some(clause), false) = (clause, driver_owned) {
            if self.pending_reply.is_some() {
                let reply = i.evidence.drive.replies.get(i.key.worktree.as_str());
                self.flush_pending(i, reply, out);
            }
            self.pending_reply = Some(PendingReply {
                kind: TurnKind::Human,
                delivery_id: None,
                awaiting_turn: None,
                clause,
                detected_at: i.at,
                identity: now.identity,
                reply_turn_at_detection: reply_turn,
                transcript: TranscriptState::NotStarted,
                token: take_token(next_token),
                query: ReplyQuery {
                    key: i.key.clone(),
                    agent: o.agent.clone(),
                    agent_session: o.agent_session.clone(),
                    agent_session_kind: o.agent_session_kind.clone(),
                    delivered_at_ms: self.ledger_moved_ms,
                    detected_at_ms: i.at_ms,
                },
            });
        }
        self.ledger = Some(now);
        self.ledger_reply_turn = reply_turn;
        self.ledger_moved_ms = i.at_ms;
        self.active_since_ledger = false;
        self.claim_since = None;
    }

    /// Record, once, a delivery whose target has sat settled idle at its baseline for
    /// [`STALL_NO_ACTIVITY`]: nothing happened since it was sent.
    fn note_no_activity(&mut self, i: &TurnInput, settled_idle: bool) {
        let (Some(t), Some(o)) = (&self.delivery, i.obs) else {
            return;
        };
        let b = &t.baseline;
        if self.no_activity.is_none()
            && settled_idle
            && !t.active_seen
            && o.state_change_seq == b.state_change_seq
            && o.completion_seq == b.completion_seq
            && i.at_ms - b.delivered_at_ms >= STALL_NO_ACTIVITY.as_millis() as i64
        {
            self.no_activity = Some((t.delivery_id, i.at_ms));
            log::info!(
                "drive: @{} delivery {} stalled: no activity observed since delivery",
                i.key.worktree,
                t.delivery_id
            );
        }
    }

    /// Give a pending reply its text: the Stop hook's (as soon as it lands),
    /// else the transcript's (looked up from [`TRANSCRIPT_AFTER`]), else none at
    /// [`REPLY_WAIT`].
    fn collect_reply(
        &mut self,
        i: &TurnInput,
        reply: Option<&keitora_tasks::SessionReply>,
        results: &mut HashMap<u64, Result<TurnReply, &'static str>>,
        out: &mut StepOut,
    ) {
        let Some(p) = self.pending_reply.as_mut() else {
            return;
        };
        if let Some(r) = results.remove(&p.token) {
            p.transcript = TranscriptState::Done(r);
        }
        let waited = i.at.saturating_duration_since(p.detected_at);
        if hook_reply(p, reply).is_some()
            || matches!(p.transcript, TranscriptState::Done(Ok(_)))
            || waited >= REPLY_WAIT
        {
            self.flush_pending(i, reply, out);
        } else if waited >= TRANSCRIPT_AFTER && p.transcript == TranscriptState::NotStarted {
            p.transcript = TranscriptState::InFlight;
            out.lookups.push(Lookup {
                token: p.token,
                query: p.query.clone(),
            });
        }
    }

    /// Write the pending reply's row with the best text it has: the hook's, the
    /// transcript's, or none.
    fn flush_pending(
        &mut self,
        i: &TurnInput,
        reply: Option<&keitora_tasks::SessionReply>,
        out: &mut StepOut,
    ) {
        let Some(p) = self.pending_reply.take() else {
            return;
        };
        let (text, source, miss) = match (hook_reply(&p, reply), &p.transcript) {
            (Some(text), _) => (text, "hook", None),
            (None, TranscriptState::Done(Ok(r))) => (r.text.clone(), "transcript", None),
            (None, TranscriptState::Done(Err(m))) => (String::new(), "none", Some(*m)),
            (None, _) => (String::new(), "none", None),
        };
        let caveat = (source != "hook" && is_agy(p.query.agent.as_deref())).then_some(AGY_CAVEAT);
        out.inserts.push(TurnInsert {
            completion_seq: p.identity.0,
            state_change_seq: Some(p.identity.1),
            reply: text,
            reply_source: source,
            caveat,
            transcript_miss: miss,
            ..TurnInsert::new(i, p.kind, p.delivery_id, p.clause)
        });
        if let Some(id) = p.delivery_id {
            self.resolve_delivery(id);
        }
    }

    fn resolve_delivery(&mut self, id: i64) {
        if let Some(t) = self.delivery.take_if(|t| t.delivery_id == id) {
            self.resolved = Some((id, t.active_seen));
        }
    }
}

/// The Stop hook's reply for `p`, once the hook recorded a turn past the detection
/// (or, for a driver turn, at or past the awaited one).
fn hook_reply(p: &PendingReply, reply: Option<&keitora_tasks::SessionReply>) -> Option<String> {
    let r = reply?;
    let landed = r.turn > p.reply_turn_at_detection || p.awaiting_turn.is_some_and(|a| r.turn >= a);
    landed.then(|| r.last_message.clone())
}

fn take_token(next: &mut u64) -> u64 {
    *next += 1;
    *next
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::model::obs;

    fn base(cs: Option<u64>, scs: u64) -> Baseline {
        let o = obs("w1:p1", "idle", scs, cs);
        Baseline {
            agent: o.agent.clone(),
            pane_id: o.pane_id.clone(),
            terminal_id: o.terminal_id.clone(),
            agent_session: o.agent_session.clone(),
            state_change_seq: scs,
            completion_seq: cs,
            status: "idle".into(),
            delivered_at_ms: 1_000,
        }
    }

    /// `evaluate` on a settled idle `o`, no activity, no hook, present.
    fn settled(b: &Baseline, o: &AgentObs) -> TurnVerdict {
        evaluate(b, Some(o), true, false, false, false)
    }

    #[test]
    fn completion_seq_change_finishes() {
        let b = base(Some(4), 11);
        let done = TurnVerdict::Finished(Clause::CompletionSeq);
        assert_eq!(settled(&b, &obs("w1:p1", "idle", 11, Some(5))), done);
        // From none to a stamp.
        let b0 = base(None, 11);
        assert_eq!(settled(&b0, &obs("w1:p1", "idle", 11, Some(3))), done);
        // A stamp that went away is no completion.
        assert_eq!(
            settled(&b, &obs("w1:p1", "idle", 11, None)),
            TurnVerdict::Pending
        );
        assert_eq!(
            settled(&b, &obs("w1:p1", "idle", 11, Some(4))),
            TurnVerdict::Pending
        );
    }

    #[test]
    fn completion_seq_any_difference_counts() {
        let b = base(Some(5), 11);
        assert_eq!(
            settled(&b, &obs("w1:p1", "idle", 11, Some(2))),
            TurnVerdict::Finished(Clause::CompletionSeq),
            "a stamp, not a counter: lower still differs"
        );
    }

    #[test]
    fn state_change_seq_plus_two_finishes() {
        let b = base(None, 11);
        assert_eq!(
            settled(&b, &obs("w1:p1", "idle", 13, None)),
            TurnVerdict::Finished(Clause::StateChangeSeq)
        );
        assert_eq!(
            settled(&b, &obs("w1:p1", "idle", 12, None)),
            TurnVerdict::Pending
        );
    }

    #[test]
    fn observed_active_then_settled_idle_finishes() {
        let b = base(None, 11);
        let o = obs("w1:p1", "idle", 12, None);
        assert_eq!(
            evaluate(&b, Some(&o), true, true, false, false),
            TurnVerdict::Finished(Clause::ObservedActive)
        );
        assert_eq!(settled(&b, &o), TurnVerdict::Pending);
    }

    #[test]
    fn unsettled_or_vetoed_idle_never_finishes() {
        let b = base(Some(4), 11);
        let o = obs("w1:p1", "idle", 13, Some(5));
        // Not settled (or a hook row vetoes the idle): every clause waits.
        assert_eq!(
            evaluate(&b, Some(&o), false, true, false, false),
            TurnVerdict::Pending
        );
        let working = obs("w1:p1", "working", 12, None);
        assert_eq!(
            evaluate(&b, Some(&working), false, true, false, false),
            TurnVerdict::Pending
        );
    }

    #[test]
    fn stop_hook_finishes_even_while_herdr_is_busy() {
        let b = base(Some(4), 11);
        let working = obs("w1:p1", "working", 12, None);
        assert_eq!(
            evaluate(&b, Some(&working), false, true, true, false),
            TurnVerdict::Finished(Clause::StopHook)
        );
        assert_eq!(
            evaluate(&b, None, false, false, true, false),
            TurnVerdict::Finished(Clause::StopHook)
        );
    }

    #[test]
    fn occupant_change_is_restarted() {
        let b = base(Some(4), 11);
        let same = obs("w1:p1", "idle", 11, Some(4));
        let cases = [
            AgentObs {
                pane_id: "w1:p2".into(),
                ..same.clone()
            },
            AgentObs {
                terminal_id: "term-other".into(),
                ..same.clone()
            },
            AgentObs {
                agent_session: Some("s-2".into()),
                ..same.clone()
            },
        ];
        for o in cases {
            assert_eq!(
                evaluate(&b, Some(&o), false, false, false, false),
                TurnVerdict::Restarted,
                "{o:?}"
            );
        }
        // A session herdr learns only now is the same occupant.
        let b_none = Baseline {
            agent_session: None,
            ..b.clone()
        };
        assert_eq!(settled(&b_none, &same), TurnVerdict::Pending);
        // And one it forgot is too.
        let forgot = AgentObs {
            agent_session: None,
            ..same
        };
        assert_eq!(settled(&b, &forgot), TurnVerdict::Pending);
    }

    #[test]
    fn herdr_restart_with_reset_seqs_is_restarted() {
        let b = base(Some(40), 110);
        assert_eq!(
            settled(&b, &obs("w1:p1", "idle", 3, Some(2))),
            TurnVerdict::Restarted
        );
    }

    #[test]
    fn absent_once_is_pending_absent_twice_is_restarted() {
        let b = base(Some(4), 11);
        assert_eq!(
            evaluate(&b, None, false, false, false, false),
            TurnVerdict::Pending
        );
        assert_eq!(
            evaluate(&b, None, false, false, false, true),
            TurnVerdict::Restarted
        );
    }

    #[test]
    fn no_transcript_misses_with_no_provider_support() {
        let q = ReplyQuery {
            key: SessionKey {
                project: "proj".into(),
                worktree: "alpha".into(),
            },
            agent: Some("codex".into()),
            agent_session: None,
            agent_session_kind: None,
            delivered_at_ms: 0,
            detected_at_ms: 0,
        };
        assert_eq!(NoTranscript.reply(&q), Err("no_provider_support"));
    }

    #[test]
    fn agy_labels_are_recognized() {
        assert!(is_agy(Some("agy")));
        assert!(is_agy(Some("antigravity_cli")));
        assert!(!is_agy(Some("codex")));
        assert!(!is_agy(None));
    }

    #[test]
    fn a_delivery_tracked_again_keeps_its_observed_activity() {
        let key = SessionKey {
            project: "proj".into(),
            worktree: "alpha".into(),
        };
        let delivery = |id: i64| keitora_tasks::Delivery {
            driven: "alpha".into(),
            driver: "bravo".into(),
            awaiting_turn: Some(1),
            delivery_id: Some(id),
            baseline: Some(base(None, 11)),
            delivered_at: None,
        };
        let evidence = |d: Option<keitora_tasks::Delivery>| ProjectEvidence {
            drive: std::sync::Arc::new(super::super::model::DriveEvidence {
                deliveries: d.into_iter().collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let view = View {
            state: "idle",
            agent: super::super::model::Presence::Present,
            agent_kind: None,
            herdr_status: Some("idle".into()),
            settled: true,
            stale: false,
            interactive_ready: Some(true),
            pane_id: None,
            terminal_id: None,
            agent_session: None,
            state_change_seq: Some(12),
            completion_seq: None,
            brokered: false,
            escalated_reason: None,
            last_change_at_ms: 0,
        };
        let load = |m: &mut TurnMemory, ev: &ProjectEvidence| {
            m.load_delivery(&TurnInput {
                key: &key,
                obs: None,
                view: &view,
                evidence: ev,
                at: Instant::now(),
                at_ms: 0,
            });
        };
        let mut m = TurnMemory::default();
        load(&mut m, &evidence(Some(delivery(7))));
        m.delivery.as_mut().unwrap().active_seen = true;
        m.resolve_delivery(7);
        assert!(m.delivery.is_none());
        // Its row didn't land: the store lists it again.
        load(&mut m, &evidence(Some(delivery(7))));
        assert!(m.delivery.as_ref().unwrap().active_seen, "same id resumes");
        // Another delivery starts afresh.
        m.resolve_delivery(7);
        load(&mut m, &evidence(Some(delivery(8))));
        assert!(!m.delivery.as_ref().unwrap().active_seen);
    }

    #[test]
    fn the_health_ring_keeps_the_last_ten() {
        let mut m = TurnMemory {
            no_activity: Some((1, 5)),
            ..TurnMemory::default()
        };
        for n in 0..12 {
            m.push_note(CompletionNote {
                kind: TurnKind::Human,
                signal: "herdr",
                reply_source: "none",
                transcript_miss: None,
                at_ms: n,
            });
        }
        let h = m.health();
        assert_eq!(h.recent.len(), RECENT_CAP);
        assert_eq!(h.recent[0].at_ms, 2);
        assert_eq!(h.no_activity, None, "a completion clears it");
    }
}
