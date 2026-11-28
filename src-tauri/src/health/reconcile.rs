//! Findings across evaluations: which are active, which cleared, what to write.
//!
//! - A new key is inserted (`first_seen = last_seen = now`, `occurrences = 1`).
//! - An unchanged finding writes nothing; its `last_seen` is flushed at most once per
//!   [`LAST_SEEN_FLUSH_MS`] (queued for the publisher's next heartbeat).
//! - A changed severity, message or detail rewrites the row.
//! - A key missing from two evaluations in a row clears (`cleared_at = now`); checks
//!   marked instant clear on the first. A cleared key that comes back starts a new
//!   episode (`occurrences += 1`).
//! - Cleared rows go [`CLEARED_KEEP_MS`] after they cleared.
//! - An owner's note (what it already did) is recorded when a finding is inserted or
//!   re-opened; a re-opened finding drops the last episode's note or remedy first.
//! - Remedies run when a finding is inserted or re-opened, at most once per key per
//!   [`REMEDY_EVERY_MS`], never when global writes are off; each is recorded in the
//!   row (`remedy`, `remedy_at`) and handed back to the caller to spawn.

use std::collections::{BTreeMap, HashMap, HashSet};

use keitora_tasks::{FindingKey, FindingRow, FindingWrite};
use serde_json::Value;

use super::commands::HealthFinding;
use super::{clears_instantly, Finding, Remedy};

/// How often an unchanged finding's `last_seen` reaches `status.db`.
pub const LAST_SEEN_FLUSH_MS: i64 = 60_000;
/// How long a cleared finding is kept (and listed as recently cleared).
pub const CLEARED_KEEP_MS: i64 = 24 * 60 * 60 * 1000;
/// The least time between two remedies of one key.
pub const REMEDY_EVERY_MS: i64 = 10 * 60 * 1000;

/// One finding the reconciler keeps.
#[derive(Clone, Debug, PartialEq)]
struct Tracked {
    /// The row as written (its `detail` is `detail` serialized).
    row: FindingRow,
    /// The finding's detail, kept as a value so the window's list needs no parse.
    detail: Value,
    /// Evaluations in a row that didn't find it.
    misses: u8,
    /// The `last_seen` last written.
    flushed_last_seen: i64,
}

impl Tracked {
    /// Take `detail`, re-serializing the row's copy only when it changed. `true` when
    /// it did.
    fn set_detail(&mut self, detail: Value) -> bool {
        if self.detail == detail {
            return false;
        }
        self.row.detail = detail.to_string();
        self.detail = detail;
        true
    }
}

/// What one reconcile decided.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    /// For `StatusPublisher::findings_apply`.
    pub writes: Vec<FindingWrite>,
    /// Remedies to run (already recorded in the rows written).
    pub remedies: Vec<Remedy>,
    /// A finding was inserted, changed, cleared or dropped: the UI should re-read.
    /// (A flushed `last_seen` alone is a `LastSeen` write, not a change.)
    pub changed: bool,
}

/// Which `(project, worktree)` pairs an evaluation covered.
pub type Scope = HashSet<(String, String)>;

/// The monitor's memory of findings. See the module docs.
#[derive(Debug, Default)]
pub struct Reconciler {
    tracked: BTreeMap<FindingKey, Tracked>,
    /// When each key's remedy last ran (kept across episodes).
    remedied: HashMap<FindingKey, i64>,
    /// Whether remedies may run (global writes allowed).
    pub remedies_allowed: bool,
}

fn key_of(f: &Finding) -> FindingKey {
    FindingKey {
        project: f.project.clone(),
        worktree: f.worktree.clone(),
        check_id: f.check_id.to_string(),
    }
}

impl Reconciler {
    pub fn new(remedies_allowed: bool) -> Reconciler {
        Reconciler {
            remedies_allowed,
            ..Default::default()
        }
    }

    /// Fold one evaluation's `desired` findings in at `now_ms`. With `scope`, only keys
    /// of those `(project, worktree)` pairs are considered (a partial evaluation); the
    /// rest are left as they are.
    pub fn reconcile(
        &mut self,
        desired: Vec<Finding>,
        scope: Option<&Scope>,
        now_ms: i64,
    ) -> Outcome {
        let in_scope = |k: &FindingKey| {
            scope.is_none_or(|s| s.contains(&(k.project.clone(), k.worktree.clone())))
        };
        let mut out = Outcome::default();
        let mut seen = HashSet::new();
        for f in desired {
            let key = key_of(&f);
            if !in_scope(&key) || !seen.insert(key.clone()) {
                continue;
            }
            self.fold_in(key, f, now_ms, &mut out);
        }
        let absent: Vec<FindingKey> = self
            .tracked
            .iter()
            .filter(|(k, t)| t.row.cleared_at.is_none() && in_scope(k) && !seen.contains(*k))
            .map(|(k, _)| k.clone())
            .collect();
        for key in absent {
            let t = self.tracked.get_mut(&key).expect("listed");
            t.misses = t.misses.saturating_add(1);
            if t.misses >= 2 || clears_instantly(&key.check_id) {
                t.row.cleared_at = Some(now_ms);
                out.writes.push(FindingWrite::Upsert(t.row.clone()));
                out.changed = true;
            }
        }
        self.prune(now_ms, &mut out);
        out
    }

    fn fold_in(&mut self, key: FindingKey, f: Finding, now_ms: i64, out: &mut Outcome) {
        let active = self.tracked.get(&key).map(|t| t.row.cleared_at.is_none());
        match active {
            Some(true) => {
                let t = self.tracked.get_mut(&key).expect("tracked");
                t.misses = 0;
                t.row.last_seen = now_ms;
                let detail_changed = t.set_detail(f.detail);
                if detail_changed
                    || t.row.severity != f.severity.as_str()
                    || t.row.message != f.message
                {
                    t.row.severity = f.severity.as_str().to_string();
                    t.row.message = f.message;
                    t.flushed_last_seen = now_ms;
                    out.writes.push(FindingWrite::Upsert(t.row.clone()));
                    out.changed = true;
                } else if now_ms - t.flushed_last_seen >= LAST_SEEN_FLUSH_MS {
                    t.flushed_last_seen = now_ms;
                    out.writes.push(FindingWrite::LastSeen {
                        key: key.clone(),
                        last_seen: now_ms,
                    });
                }
                return;
            }
            // A cleared key comes back: a new episode.
            Some(false) => {
                let t = self.tracked.get_mut(&key).expect("tracked");
                t.misses = 0;
                t.flushed_last_seen = now_ms;
                t.row.severity = f.severity.as_str().to_string();
                t.row.message = f.message;
                t.set_detail(f.detail);
                t.row.first_seen = now_ms;
                t.row.last_seen = now_ms;
                t.row.occurrences += 1;
                t.row.cleared_at = None;
                // The last episode's note or remedy says nothing about this one.
                t.row.remedy.clear();
                t.row.remedy_at = None;
            }
            None => {
                let row = FindingRow {
                    project: key.project.clone(),
                    worktree: key.worktree.clone(),
                    check_id: key.check_id.clone(),
                    severity: f.severity.as_str().to_string(),
                    message: f.message,
                    detail: f.detail.to_string(),
                    first_seen: now_ms,
                    last_seen: now_ms,
                    occurrences: 1,
                    cleared_at: None,
                    remedy: String::new(),
                    remedy_at: None,
                };
                self.tracked.insert(
                    key.clone(),
                    Tracked {
                        row,
                        detail: f.detail,
                        misses: 0,
                        flushed_last_seen: now_ms,
                    },
                );
            }
        }
        let opened = self.tracked.get_mut(&key).expect("tracked");
        if let Some(note) = f.note {
            opened.row.remedy = note.to_string();
            opened.row.remedy_at = Some(now_ms);
        }
        if let Some(r) = f.remedy {
            let run = self.remedies_allowed
                && self
                    .remedied
                    .get(&key)
                    .is_none_or(|at| now_ms - at >= REMEDY_EVERY_MS);
            if run {
                opened.row.remedy = r.label().to_string();
                opened.row.remedy_at = Some(now_ms);
                self.remedied.insert(key, now_ms);
                out.remedies.push(r);
            }
        }
        out.writes.push(FindingWrite::Upsert(opened.row.clone()));
        out.changed = true;
    }

    /// Drop findings cleared [`CLEARED_KEEP_MS`] ago, and remedy times older than a
    /// remedy period with no finding left.
    fn prune(&mut self, now_ms: i64, out: &mut Outcome) {
        let gone: Vec<FindingKey> = self
            .tracked
            .iter()
            .filter(|(_, t)| {
                t.row
                    .cleared_at
                    .is_some_and(|c| now_ms - c >= CLEARED_KEEP_MS)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in gone {
            self.tracked.remove(&key);
            out.writes.push(FindingWrite::Delete(key));
            out.changed = true;
        }
        let tracked = &self.tracked;
        self.remedied
            .retain(|k, at| tracked.contains_key(k) || now_ms - *at < REMEDY_EVERY_MS);
    }

    /// Every kept finding as the window reads it: active ones and those cleared in the
    /// last [`CLEARED_KEEP_MS`], sorted by key.
    pub fn list(&self) -> Vec<HealthFinding> {
        self.tracked
            .values()
            .map(|t| HealthFinding::from_row(&t.row, t.detail.clone()))
            .collect()
    }

    /// Every kept row, as [`Self::resync`] writes them.
    #[cfg(test)]
    pub fn rows(&self) -> Vec<FindingRow> {
        self.tracked.values().map(|t| t.row.clone()).collect()
    }

    /// The writes that make a `findings` table equal what is kept here: clear it, then
    /// write every row. For a fresh lease holder (start, takeover).
    pub fn resync(&mut self) -> Vec<FindingWrite> {
        let mut w = vec![FindingWrite::Clear];
        for t in self.tracked.values_mut() {
            t.flushed_last_seen = t.row.last_seen;
            w.push(FindingWrite::Upsert(t.row.clone()));
        }
        w
    }
}
