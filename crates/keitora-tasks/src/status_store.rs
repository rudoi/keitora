//! The global status store, `<KEITORA_HOME>/status.db`.
//!
//! The app's session engine is the only writer: it holds a lease on the `writer` row
//! (taken over only when the holder's heartbeat is older than [`LEASE_STALE_MS`]) and
//! writes one `sessions` row per `(project, worktree)` view, changed rows only. The CLI
//! opens it read-only through [`StatusReader`] and trusts a session row only while
//! [`freshness`] says the writer is [`Freshness::Fresh`].
//!
//! The file is a disposable cache: [`StatusWriter::open_or_recreate`] deletes and
//! recreates it on corruption or a schema mismatch, unless a live writer still holds it.
//!
//! Every time column is INTEGER unix epoch milliseconds. `tasks.db` keeps its UTC text
//! columns; compare across the two with [`sqlite_utc_to_ms`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::NaiveDateTime;
use rusqlite::{params, Connection, ErrorCode, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::local_store::open_ro_conn;
use crate::{Error, Result, WorktreeSession};

/// `PRAGMA user_version` of a current `status.db`.
pub const STATUS_SCHEMA_VERSION: i64 = 1;
/// A writer heartbeat older than this (ms) means the CLI treats the store as stale.
pub const STATUS_FRESH_MS: i64 = 15_000;
/// A writer heartbeat older than this (ms) lets another instance take the lease.
pub const LEASE_STALE_MS: i64 = 15_000;
/// How far (ms) the throttled `app_projects` columns ([`AppProjectTouch`]) may trail
/// what the app knows: queued on its pipeline tick (1 s), written on its next
/// heartbeat (5 s), compared with second-resolution `created_at` stamps, plus margin.
pub const PRESENCE_TOUCH_LAG_MS: i64 = 8_000;
/// The values `writer.herdr_link` may take.
pub const HERDR_LINKS: [&str; 5] = ["starting", "up", "recovering", "down", "unavailable"];

/// The whole `status.db` schema. Applied on a fresh file (`user_version == 0`).
pub const STATUS_DDL: &str = "
CREATE TABLE IF NOT EXISTS writer (
  id               INTEGER PRIMARY KEY CHECK (id = 1),
  instance_id      TEXT    NOT NULL,
  pid              INTEGER NOT NULL,
  version          TEXT    NOT NULL,
  started_at       INTEGER NOT NULL,
  heartbeat_at     INTEGER NOT NULL,
  last_snapshot_at INTEGER,
  herdr_link       TEXT    NOT NULL
                   CHECK (herdr_link IN ('starting','up','recovering','down','unavailable')),
  herdr_link_since INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
  project           TEXT    NOT NULL,
  worktree          TEXT    NOT NULL,
  state             TEXT    NOT NULL CHECK (state IN ('idle','busy','waiting','offline')),
  agent             TEXT    NOT NULL CHECK (agent IN ('present','starting','absent')),
  agent_kind        TEXT,
  herdr_status      TEXT,
  settled           INTEGER NOT NULL CHECK (settled IN (0,1)),
  stale             INTEGER NOT NULL CHECK (stale IN (0,1)),
  interactive_ready INTEGER CHECK (interactive_ready IN (0,1)),
  pane_id           TEXT,
  terminal_id       TEXT,
  agent_session     TEXT,
  state_change_seq  INTEGER,
  completion_seq    INTEGER,
  brokered          INTEGER NOT NULL DEFAULT 0 CHECK (brokered IN (0,1)),
  escalated_reason  TEXT,
  last_change_at    INTEGER NOT NULL,
  written_at        INTEGER NOT NULL,
  PRIMARY KEY (project, worktree)
);
CREATE TABLE IF NOT EXISTS app_projects (
  project               TEXT PRIMARY KEY,
  registry_key          TEXT    NOT NULL,
  db_path               TEXT    NOT NULL,
  open                  INTEGER NOT NULL DEFAULT 0 CHECK (open IN (0,1)),
  open_reported_at      INTEGER,
  watcher_state         TEXT    NOT NULL DEFAULT 'absent'
                        CHECK (watcher_state IN ('absent','starting','running','exited')),
  watcher_started_at    INTEGER,
  watcher_last_event_at INTEGER,
  last_pass_at          INTEGER,
  last_pass_trigger     TEXT,
  pending_count         INTEGER NOT NULL DEFAULT 0,
  oldest_pending_at     INTEGER,
  restarts              INTEGER NOT NULL DEFAULT 0,
  last_error            TEXT    NOT NULL DEFAULT '',
  last_error_at         INTEGER
);
CREATE TABLE IF NOT EXISTS findings (
  project     TEXT    NOT NULL,
  worktree    TEXT    NOT NULL DEFAULT '',
  check_id    TEXT    NOT NULL,
  severity    TEXT    NOT NULL CHECK (severity IN ('info','warn','error')),
  message     TEXT    NOT NULL,
  detail      TEXT    NOT NULL DEFAULT '{}',
  first_seen  INTEGER NOT NULL,
  last_seen   INTEGER NOT NULL,
  occurrences INTEGER NOT NULL DEFAULT 1,
  cleared_at  INTEGER,
  remedy      TEXT    NOT NULL DEFAULT '',
  remedy_at   INTEGER,
  PRIMARY KEY (project, worktree, check_id)
);
";

/// `<KEITORA_HOME>/status.db`.
pub fn status_db_path() -> PathBuf {
    crate::keitora_home().join("status.db")
}

/// The current time as unix epoch milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// A duration in whole seconds as people read it: `{s}s` under two minutes, `{m}m`
/// under two hours, else `{h}h`; a negative duration reads `0s`.
pub fn age_text(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 120 {
        format!("{secs}s")
    } else if secs < 7200 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// `HH:MM:SS`, local time, of a unix-ms instant (`?` when out of range).
pub fn clock_text(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "?".to_string())
}

/// Parse a `tasks.db` UTC text timestamp (`%Y-%m-%d %H:%M:%S` with an optional
/// `.%f`; a `T` separator is accepted too) into unix epoch milliseconds.
pub fn sqlite_utc_to_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = s.split_once([' ', 'T'])?;
    // Fixed-width fields, as SQLite emits them; chrono alone would also accept
    // unpadded numbers. Fractional seconds are truncated to milliseconds.
    let (hms, frac) = match time.split_once('.') {
        Some((hms, frac)) => (hms, Some(frac)),
        None => (time, None),
    };
    if date.len() != 10 || hms.len() != 8 {
        return None;
    }
    let millis = match frac {
        None => 0,
        Some(f) if !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()) => {
            // First three digits, right-padded: ".5" is 500 ms, ".123456" is 123 ms.
            format!("{f:0<3}")[..3].parse::<i64>().ok()?
        }
        Some(_) => return None,
    };
    let dt = NaiveDateTime::parse_from_str(&format!("{date} {hms}"), "%Y-%m-%d %H:%M:%S").ok()?;
    Some(dt.and_utc().timestamp_millis() + millis)
}

// ── Row types ────────────────────────────────────────────────────────────────

/// The `writer` row: who holds the lease and how healthy its herdr link is.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WriterRow {
    pub instance_id: String,
    pub pid: i64,
    pub version: String,
    pub started_at: i64,
    pub heartbeat_at: i64,
    pub last_snapshot_at: Option<i64>,
    pub herdr_link: String,
    pub herdr_link_since: i64,
}

/// One `sessions` row: the engine's view of a `(project, worktree)`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StatusSession {
    pub project: String,
    pub worktree: String,
    pub state: String,
    pub agent: String,
    pub agent_kind: Option<String>,
    pub herdr_status: Option<String>,
    pub settled: bool,
    pub stale: bool,
    pub interactive_ready: Option<bool>,
    pub pane_id: Option<String>,
    pub terminal_id: Option<String>,
    pub agent_session: Option<String>,
    pub state_change_seq: Option<i64>,
    pub completion_seq: Option<i64>,
    pub brokered: bool,
    pub escalated_reason: Option<String>,
    pub last_change_at: i64,
}

/// One `app_projects` row (column for column).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AppProject {
    pub project: String,
    pub registry_key: String,
    pub db_path: String,
    pub open: bool,
    pub open_reported_at: Option<i64>,
    pub watcher_state: String,
    pub watcher_started_at: Option<i64>,
    pub watcher_last_event_at: Option<i64>,
    pub last_pass_at: Option<i64>,
    pub last_pass_trigger: Option<String>,
    pub pending_count: i64,
    pub oldest_pending_at: Option<i64>,
    pub restarts: i64,
    pub last_error: String,
    pub last_error_at: Option<i64>,
}

/// One `findings` row (column for column).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FindingRow {
    pub project: String,
    pub worktree: String,
    pub check_id: String,
    pub severity: String,
    pub message: String,
    pub detail: String,
    pub first_seen: i64,
    pub last_seen: i64,
    pub occurrences: i64,
    pub cleared_at: Option<i64>,
    pub remedy: String,
    pub remedy_at: Option<i64>,
}

// ── Freshness ────────────────────────────────────────────────────────────────

/// How far a reader may trust the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// A live writer with herdr up and at least one snapshot: session rows are usable.
    Fresh(WriterRow),
    /// A live writer that has no usable herdr snapshot yet (link not `up`, or none taken).
    NotReady(WriterRow),
    /// No writer, or its heartbeat is older than [`STATUS_FRESH_MS`].
    Stale { heartbeat_at: Option<i64> },
}

/// Classify the writer row at `now_ms`.
pub fn freshness(writer: Option<WriterRow>, now_ms: i64) -> Freshness {
    match writer {
        None => Freshness::Stale { heartbeat_at: None },
        Some(w) if now_ms - w.heartbeat_at > STATUS_FRESH_MS => Freshness::Stale {
            heartbeat_at: Some(w.heartbeat_at),
        },
        Some(w) if w.herdr_link != "up" || w.last_snapshot_at.is_none() => Freshness::NotReady(w),
        Some(w) => Freshness::Fresh(w),
    }
}

// ── Writer ───────────────────────────────────────────────────────────────────

/// Who is asking for the lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterIdentity {
    pub instance_id: String,
    pub pid: i64,
    pub version: String,
    pub started_at: i64,
}

/// The result of [`StatusWriter::try_acquire`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseOutcome {
    Owned,
    Standby {
        pid: i64,
        version: String,
        heartbeat_at: i64,
    },
}

/// The result of [`StatusWriter::open_or_recreate`].
pub enum WriterOpen {
    /// The file is open, current and writable (the lease is not taken yet).
    Ready(StatusWriter),
    /// The file is unusable (corrupt or another schema) but a live writer still
    /// heartbeats in it, so it was left alone.
    Held {
        pid: i64,
        version: String,
        heartbeat_at: i64,
    },
}

/// The single writer connection to `status.db`.
pub struct StatusWriter {
    conn: Connection,
    /// The instance holding the lease through this connection, if any.
    owner: Option<String>,
    /// The last row written per `(project, worktree)`, for change-only writes.
    written: BTreeMap<(String, String), StatusSession>,
}

impl StatusWriter {
    /// Open (creating the file and its parent dir) and bring the schema to
    /// [`STATUS_SCHEMA_VERSION`]. Fails on a corrupt file or another schema version.
    pub fn open(path: &Path) -> Result<StatusWriter> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Config(format!("cannot create {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(1000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(Error::Config(format!(
                "status.db failed quick_check: {check}"
            )));
        }
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            conn.execute_batch(STATUS_DDL)?;
            conn.pragma_update(None, "user_version", STATUS_SCHEMA_VERSION)?;
        } else if version != STATUS_SCHEMA_VERSION {
            return Err(Error::Config(format!(
                "status.db schema version {version}, expected {STATUS_SCHEMA_VERSION}"
            )));
        }
        Ok(StatusWriter {
            conn,
            owner: None,
            written: BTreeMap::new(),
        })
    }

    /// [`Self::open`], recreating the file when it is corrupt or of another schema
    /// version, unless its `writer` row still heartbeats within [`LEASE_STALE_MS`]
    /// of `now_ms` ([`WriterOpen::Held`]). A busy file is an error (retry later), as is
    /// a delete that fails (another process holds the file open).
    pub fn open_or_recreate(path: &Path, now_ms: i64) -> Result<WriterOpen> {
        let err = match Self::open(path) {
            Ok(w) => return Ok(WriterOpen::Ready(w)),
            Err(e) if is_busy(&e) => return Err(e),
            Err(e) => e,
        };
        if let Some((pid, version, heartbeat_at)) = peek_writer(path) {
            if heartbeat_at >= now_ms - LEASE_STALE_MS {
                return Ok(WriterOpen::Held {
                    pid,
                    version,
                    heartbeat_at,
                });
            }
        }
        log::warn!("status.db unusable ({err}); recreating {}", path.display());
        remove_db_files(path)?;
        Self::open(path).map(WriterOpen::Ready)
    }

    /// The instance currently holding the lease through this connection.
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Take the lease when it is free, expired (heartbeat older than
    /// [`LEASE_STALE_MS`]) or already ours. Taking it resets the link to `starting`,
    /// clears `last_snapshot_at` and marks every session row stale.
    pub fn try_acquire(&mut self, me: &WriterIdentity, now_ms: i64) -> Result<LeaseOutcome> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(String, i64, String, i64)> = tx
            .query_row(
                "SELECT instance_id, pid, version, heartbeat_at FROM writer WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some((instance_id, pid, version, heartbeat_at)) = current {
            if instance_id != me.instance_id && heartbeat_at >= now_ms - LEASE_STALE_MS {
                tx.commit()?;
                self.owner = None;
                return Ok(LeaseOutcome::Standby {
                    pid,
                    version,
                    heartbeat_at,
                });
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO writer (id, instance_id, pid, version, started_at, \
             heartbeat_at, last_snapshot_at, herdr_link, herdr_link_since) \
             VALUES (1, ?1, ?2, ?3, ?4, ?5, NULL, 'starting', ?5)",
            params![me.instance_id, me.pid, me.version, me.started_at, now_ms],
        )?;
        tx.execute("UPDATE sessions SET stale = 1", [])?;
        let written = load_sessions(&tx, None)?
            .into_iter()
            .map(|s| ((s.project.clone(), s.worktree.clone()), s))
            .collect();
        tx.commit()?;
        self.owner = Some(me.instance_id.clone());
        self.written = written;
        Ok(LeaseOutcome::Owned)
    }

    /// Refresh the lease. `false` means it was lost (another instance took it over).
    pub fn heartbeat(&mut self, instance_id: &str, now_ms: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE writer SET heartbeat_at = ?1 WHERE id = 1 AND instance_id = ?2",
            params![now_ms, instance_id],
        )?;
        self.note_lease(instance_id, n)
    }

    /// Record a herdr link change (`link` is one of [`HERDR_LINKS`]). `false` means
    /// the lease was lost.
    pub fn set_herdr_link(&mut self, instance_id: &str, link: &str, since_ms: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE writer SET herdr_link = ?1, herdr_link_since = ?2 \
             WHERE id = 1 AND instance_id = ?3 \
               AND (herdr_link IS NOT ?1 OR herdr_link_since IS NOT ?2)",
            params![link, since_ms, instance_id],
        )?;
        if n > 0 {
            return self.note_lease(instance_id, n);
        }
        self.still_owner(instance_id)
    }

    /// Record the time of the last successful agent snapshot. `false` means the
    /// lease was lost.
    pub fn set_last_snapshot_at(&mut self, instance_id: &str, at_ms: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE writer SET last_snapshot_at = ?1 \
             WHERE id = 1 AND instance_id = ?2 AND last_snapshot_at IS NOT ?1",
            params![at_ms, instance_id],
        )?;
        if n > 0 {
            return self.note_lease(instance_id, n);
        }
        self.still_owner(instance_id)
    }

    /// Give the lease up on clean shutdown so readers see "no app" at once. Only
    /// deletes the row when `instance_id` still holds it.
    pub fn release(&mut self, instance_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM writer WHERE id = 1 AND instance_id = ?1",
            params![instance_id],
        )?;
        if self.owner.as_deref() == Some(instance_id) {
            self.owner = None;
        }
        Ok(())
    }

    /// Make the `sessions` table equal `rows` (the full set of current views),
    /// writing only rows that differ from the last written copy and deleting rows
    /// whose view disappeared, in one transaction. No change opens no transaction.
    /// A no-op (`Ok(0)`) without the lease; a lease lost to a takeover is noticed
    /// inside the transaction and nothing is written. Returns the rows touched.
    pub fn publish_sessions(&mut self, rows: &[StatusSession], now_ms: i64) -> Result<usize> {
        let Some(owner) = self.owner.clone() else {
            return Ok(0);
        };
        let wanted: BTreeMap<(String, String), &StatusSession> = rows
            .iter()
            .map(|s| ((s.project.clone(), s.worktree.clone()), s))
            .collect();
        let upserts: Vec<&StatusSession> = wanted
            .iter()
            .filter(|(k, s)| self.written.get(*k) != Some(**s))
            .map(|(_, s)| *s)
            .collect();
        let deletes: Vec<(String, String)> = self
            .written
            .keys()
            .filter(|k| !wanted.contains_key(*k))
            .cloned()
            .collect();
        if upserts.is_empty() && deletes.is_empty() {
            return Ok(0);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let holder: Option<String> = tx
            .query_row("SELECT instance_id FROM writer WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()?;
        if holder.as_deref() != Some(owner.as_str()) {
            tx.commit()?;
            self.owner = None;
            log::warn!("status.db lease lost; session rows not written");
            return Ok(0);
        }
        for s in &upserts {
            insert_session(&tx, s, now_ms)?;
        }
        for (project, worktree) in &deletes {
            tx.execute(
                "DELETE FROM sessions WHERE project = ?1 AND worktree = ?2",
                params![project, worktree],
            )?;
        }
        tx.commit()?;
        for s in upserts.iter() {
            self.written
                .insert((s.project.clone(), s.worktree.clone()), (*s).clone());
        }
        for k in &deletes {
            self.written.remove(k);
        }
        Ok(upserts.len() + deletes.len())
    }

    fn note_lease(&mut self, instance_id: &str, rows: usize) -> Result<bool> {
        if rows == 0 {
            if self.owner.as_deref() == Some(instance_id) {
                self.owner = None;
            }
            return Ok(false);
        }
        Ok(true)
    }

    fn still_owner(&mut self, instance_id: &str) -> Result<bool> {
        let holder: Option<String> = self
            .conn
            .query_row("SELECT instance_id FROM writer WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()?;
        let ok = holder.as_deref() == Some(instance_id);
        self.note_lease(instance_id, usize::from(ok))
    }
}

/// The throttled `app_projects` columns other threads queue between heartbeats.
/// `None` leaves a column as it is; a later touch of the same project overrides the
/// fields it sets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppProjectTouch {
    pub last_pass_at: Option<i64>,
    pub last_pass_trigger: Option<String>,
    pub watcher_last_event_at: Option<i64>,
    pub pending_count: Option<i64>,
    /// `Some(None)` clears the column.
    pub oldest_pending_at: Option<Option<i64>>,
}

impl AppProjectTouch {
    /// Fold `later` into `self`: every field `later` sets wins.
    pub fn merge(&mut self, later: AppProjectTouch) {
        if later.last_pass_at.is_some() {
            self.last_pass_at = later.last_pass_at;
        }
        if later.last_pass_trigger.is_some() {
            self.last_pass_trigger = later.last_pass_trigger;
        }
        if later.watcher_last_event_at.is_some() {
            self.watcher_last_event_at = later.watcher_last_event_at;
        }
        if later.pending_count.is_some() {
            self.pending_count = later.pending_count;
        }
        if later.oldest_pending_at.is_some() {
            self.oldest_pending_at = later.oldest_pending_at;
        }
    }
}

/// The primary key of a `findings` row.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FindingKey {
    pub project: String,
    pub worktree: String,
    pub check_id: String,
}

/// One change to the `findings` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindingWrite {
    /// Insert or replace the whole row (a new finding, a changed one, a clear).
    Upsert(FindingRow),
    /// Move `last_seen` forward only (never back).
    LastSeen { key: FindingKey, last_seen: i64 },
    /// Delete the row.
    Delete(FindingKey),
    /// Delete every row (a new lease holder starts from what it knows).
    Clear,
}

/// What one heartbeat writes besides `heartbeat_at`.
#[derive(Clone, Debug, Default)]
pub struct Beat<'a> {
    /// `herdr_link` and `herdr_link_since`, written when they differ.
    pub link: Option<(&'a str, i64)>,
    /// `last_snapshot_at`, written when it differs.
    pub last_snapshot_at: Option<i64>,
    pub touches: &'a [(String, AppProjectTouch)],
    pub last_seen: &'a [(FindingKey, i64)],
}

impl StatusWriter {
    /// One heartbeat transaction: refresh `heartbeat_at`, write the link and snapshot
    /// columns when they changed, and flush the queued `app_projects` touches and
    /// `findings.last_seen` moves. `false` (and nothing written) when the lease is
    /// lost or was never held.
    pub fn beat(&mut self, instance_id: &str, now_ms: i64, beat: &Beat) -> Result<bool> {
        if self.owner.as_deref() != Some(instance_id) {
            return Ok(false);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "UPDATE writer SET heartbeat_at = ?1 WHERE id = 1 AND instance_id = ?2",
            params![now_ms, instance_id],
        )?;
        if n == 0 {
            tx.commit()?;
            self.owner = None;
            return Ok(false);
        }
        if let Some((link, since)) = beat.link {
            tx.execute(
                "UPDATE writer SET herdr_link = ?1, herdr_link_since = ?2 \
                 WHERE id = 1 AND (herdr_link IS NOT ?1 OR herdr_link_since IS NOT ?2)",
                params![link, since],
            )?;
        }
        if let Some(at) = beat.last_snapshot_at {
            tx.execute(
                "UPDATE writer SET last_snapshot_at = ?1 \
                 WHERE id = 1 AND last_snapshot_at IS NOT ?1",
                params![at],
            )?;
        }
        for (project, t) in beat.touches {
            touch_app_project(&tx, project, t)?;
        }
        for (key, at) in beat.last_seen {
            move_last_seen(&tx, key, *at)?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Insert or replace `row` in `app_projects`. `false` without the lease.
    pub fn app_project_set(&mut self, row: &AppProject) -> Result<bool> {
        self.with_lease(|tx| insert_app_project(tx, row))
    }

    /// Delete `project`'s `app_projects` row. `false` without the lease.
    pub fn app_project_delete(&mut self, project: &str) -> Result<bool> {
        self.with_lease(|tx| {
            tx.execute(
                "DELETE FROM app_projects WHERE project = ?1",
                params![project],
            )?;
            Ok(())
        })
    }

    /// Apply `writes` to `findings` in one transaction. `false` without the lease.
    pub fn findings_apply(&mut self, writes: &[FindingWrite]) -> Result<bool> {
        self.with_lease(|tx| {
            for w in writes {
                match w {
                    FindingWrite::Upsert(f) => upsert_finding(tx, f)?,
                    FindingWrite::LastSeen { key, last_seen } => {
                        move_last_seen(tx, key, *last_seen)?;
                    }
                    FindingWrite::Delete(key) => {
                        tx.execute(
                            "DELETE FROM findings \
                             WHERE project = ?1 AND worktree = ?2 AND check_id = ?3",
                            params![key.project, key.worktree, key.check_id],
                        )?;
                    }
                    FindingWrite::Clear => {
                        tx.execute("DELETE FROM findings", [])?;
                    }
                }
            }
            Ok(())
        })
    }

    /// Run `f` in one transaction that first checks this connection still holds the
    /// lease. `Ok(false)` (nothing written) without it.
    fn with_lease(&mut self, f: impl FnOnce(&rusqlite::Transaction) -> Result<()>) -> Result<bool> {
        let Some(owner) = self.owner.clone() else {
            return Ok(false);
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let holder: Option<String> = tx
            .query_row("SELECT instance_id FROM writer WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()?;
        if holder.as_deref() != Some(owner.as_str()) {
            tx.commit()?;
            self.owner = None;
            return Ok(false);
        }
        f(&tx)?;
        tx.commit()?;
        Ok(true)
    }
}

/// `INSERT OR REPLACE` one `app_projects` row.
fn insert_app_project(conn: &Connection, row: &AppProject) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO app_projects (project, registry_key, db_path, open, \
         open_reported_at, watcher_state, watcher_started_at, watcher_last_event_at, \
         last_pass_at, last_pass_trigger, pending_count, oldest_pending_at, restarts, \
         last_error, last_error_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            row.project,
            row.registry_key,
            row.db_path,
            row.open,
            row.open_reported_at,
            row.watcher_state,
            row.watcher_started_at,
            row.watcher_last_event_at,
            row.last_pass_at,
            row.last_pass_trigger,
            row.pending_count,
            row.oldest_pending_at,
            row.restarts,
            row.last_error,
            row.last_error_at,
        ],
    )?;
    Ok(())
}

fn move_last_seen(conn: &Connection, key: &FindingKey, at: i64) -> Result<()> {
    conn.execute(
        "UPDATE findings SET last_seen = ?4 \
         WHERE project = ?1 AND worktree = ?2 AND check_id = ?3 AND last_seen < ?4",
        params![key.project, key.worktree, key.check_id, at],
    )?;
    Ok(())
}

fn touch_app_project(conn: &Connection, project: &str, t: &AppProjectTouch) -> Result<()> {
    conn.execute(
        "UPDATE app_projects SET \
           last_pass_at = CASE WHEN ?2 THEN ?3 ELSE last_pass_at END, \
           last_pass_trigger = CASE WHEN ?4 THEN ?5 ELSE last_pass_trigger END, \
           watcher_last_event_at = CASE WHEN ?6 THEN ?7 ELSE watcher_last_event_at END, \
           pending_count = CASE WHEN ?8 THEN ?9 ELSE pending_count END, \
           oldest_pending_at = CASE WHEN ?10 THEN ?11 ELSE oldest_pending_at END \
         WHERE project = ?1",
        params![
            project,
            t.last_pass_at.is_some(),
            t.last_pass_at,
            t.last_pass_trigger.is_some(),
            t.last_pass_trigger,
            t.watcher_last_event_at.is_some(),
            t.watcher_last_event_at,
            t.pending_count.is_some(),
            t.pending_count.unwrap_or(0),
            t.oldest_pending_at.is_some(),
            t.oldest_pending_at.flatten(),
        ],
    )?;
    Ok(())
}

fn is_busy(e: &Error) -> bool {
    matches!(
        e,
        Error::Sqlite(rusqlite::Error::SqliteFailure(f, _))
            if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// Best-effort read of the `writer` row of a file that failed to open normally.
fn peek_writer(path: &Path) -> Option<(i64, String, i64)> {
    let conn = open_ro_conn(path).ok()?;
    conn.query_row(
        "SELECT pid, version, heartbeat_at FROM writer WHERE id = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .ok()
}

fn remove_db_files(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        match std::fs::remove_file(PathBuf::from(p)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Error::Config(format!(
                    "cannot remove {}{suffix}: {e}",
                    path.display()
                )))
            }
        }
    }
    Ok(())
}

fn insert_session(conn: &Connection, s: &StatusSession, now_ms: i64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO sessions (project, worktree, state, agent, agent_kind, \
         herdr_status, settled, stale, interactive_ready, pane_id, terminal_id, agent_session, \
         state_change_seq, completion_seq, brokered, escalated_reason, last_change_at, written_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            s.project,
            s.worktree,
            s.state,
            s.agent,
            s.agent_kind,
            s.herdr_status,
            s.settled,
            s.stale,
            s.interactive_ready,
            s.pane_id,
            s.terminal_id,
            s.agent_session,
            s.state_change_seq,
            s.completion_seq,
            s.brokered,
            s.escalated_reason,
            s.last_change_at,
            now_ms,
        ],
    )?;
    Ok(())
}

const SESSION_COLS: &str = "project, worktree, state, agent, agent_kind, herdr_status, settled, \
     stale, interactive_ready, pane_id, terminal_id, agent_session, state_change_seq, \
     completion_seq, brokered, escalated_reason, last_change_at";

fn row_to_session(r: &rusqlite::Row) -> rusqlite::Result<StatusSession> {
    Ok(StatusSession {
        project: r.get(0)?,
        worktree: r.get(1)?,
        state: r.get(2)?,
        agent: r.get(3)?,
        agent_kind: r.get(4)?,
        herdr_status: r.get(5)?,
        settled: r.get(6)?,
        stale: r.get(7)?,
        interactive_ready: r.get(8)?,
        pane_id: r.get(9)?,
        terminal_id: r.get(10)?,
        agent_session: r.get(11)?,
        state_change_seq: r.get(12)?,
        completion_seq: r.get(13)?,
        brokered: r.get(14)?,
        escalated_reason: r.get(15)?,
        last_change_at: r.get(16)?,
    })
}

fn load_sessions(conn: &Connection, project: Option<&str>) -> Result<Vec<StatusSession>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SESSION_COLS} FROM sessions WHERE ?1 IS NULL OR project = ?1 \
         ORDER BY project, worktree"
    ))?;
    let rows = stmt
        .query_map(params![project], row_to_session)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ── Reader ───────────────────────────────────────────────────────────────────

/// A read-only connection to `status.db` (the CLI side). Never creates the file.
pub struct StatusReader {
    conn: Connection,
}

impl StatusReader {
    /// `Ok(None)` when the file is missing or has another schema version (both
    /// read as stale).
    pub fn open(path: &Path) -> Result<Option<StatusReader>> {
        if !path.is_file() {
            return Ok(None);
        }
        let conn = open_ro_conn(path)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != STATUS_SCHEMA_VERSION {
            return Ok(None);
        }
        Ok(Some(StatusReader { conn }))
    }

    pub fn writer(&self) -> Result<Option<WriterRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT instance_id, pid, version, started_at, heartbeat_at, last_snapshot_at, \
                 herdr_link, herdr_link_since FROM writer WHERE id = 1",
                [],
                |r| {
                    Ok(WriterRow {
                        instance_id: r.get(0)?,
                        pid: r.get(1)?,
                        version: r.get(2)?,
                        started_at: r.get(3)?,
                        heartbeat_at: r.get(4)?,
                        last_snapshot_at: r.get(5)?,
                        herdr_link: r.get(6)?,
                        herdr_link_since: r.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn session(&self, project: &str, worktree: &str) -> Result<Option<StatusSession>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {SESSION_COLS} FROM sessions WHERE project = ?1 AND worktree = ?2"
                ),
                params![project, worktree],
                row_to_session,
            )
            .optional()?)
    }

    /// Every session row of `project`, sorted by worktree.
    pub fn sessions(&self, project: &str) -> Result<Vec<StatusSession>> {
        load_sessions(&self.conn, Some(project))
    }

    /// `project`'s state at `now_ms`, in one writer read and one sessions read. A read
    /// error counts as stale (a sessions error behind a not-ready writer: no views).
    pub fn project_status(&self, project: &str, now_ms: i64) -> ProjectStatus {
        let Ok(writer) = self.writer() else {
            return ProjectStatus::Stale { heartbeat_at: None };
        };
        let by_worktree = |rows: Vec<StatusSession>| {
            rows.into_iter()
                .map(|s| (s.worktree.clone(), s))
                .collect::<HashMap<_, _>>()
        };
        match freshness(writer, now_ms) {
            Freshness::Stale { heartbeat_at } => ProjectStatus::Stale { heartbeat_at },
            Freshness::NotReady(writer) => ProjectStatus::NotReady {
                sessions: self.sessions(project).map(by_worktree).unwrap_or_default(),
                writer,
            },
            Freshness::Fresh(writer) => match self.sessions(project) {
                Ok(rows) => ProjectStatus::Fresh {
                    sessions: by_worktree(rows),
                    writer,
                },
                Err(_) => ProjectStatus::Stale { heartbeat_at: None },
            },
        }
    }

    /// `project`'s engine views keyed by worktree, only while the store is fresh (else
    /// `None`: callers fall back to the hook rows).
    pub fn fresh_sessions(
        &self,
        project: &str,
        now_ms: i64,
    ) -> Option<HashMap<String, StatusSession>> {
        match self.project_status(project, now_ms) {
            ProjectStatus::Fresh { sessions, .. } => Some(sessions),
            _ => None,
        }
    }

    pub fn app_project(&self, project: &str) -> Result<Option<AppProject>> {
        Ok(self
            .conn
            .query_row(
                "SELECT project, registry_key, db_path, open, open_reported_at, watcher_state, \
                 watcher_started_at, watcher_last_event_at, last_pass_at, last_pass_trigger, \
                 pending_count, oldest_pending_at, restarts, last_error, last_error_at \
                 FROM app_projects WHERE project = ?1",
                params![project],
                |r| {
                    Ok(AppProject {
                        project: r.get(0)?,
                        registry_key: r.get(1)?,
                        db_path: r.get(2)?,
                        open: r.get(3)?,
                        open_reported_at: r.get(4)?,
                        watcher_state: r.get(5)?,
                        watcher_started_at: r.get(6)?,
                        watcher_last_event_at: r.get(7)?,
                        last_pass_at: r.get(8)?,
                        last_pass_trigger: r.get(9)?,
                        pending_count: r.get(10)?,
                        oldest_pending_at: r.get(11)?,
                        restarts: r.get(12)?,
                        last_error: r.get(13)?,
                        last_error_at: r.get(14)?,
                    })
                },
            )
            .optional()?)
    }

    /// Findings of `project` (`""` = app-level), all worktrees when `worktree` is
    /// `None`, else that worktree only (`Some("")` = project-level). Sorted by
    /// worktree then check.
    pub fn findings(&self, project: &str, worktree: Option<&str>) -> Result<Vec<FindingRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT project, worktree, check_id, severity, message, detail, first_seen, \
             last_seen, occurrences, cleared_at, remedy, remedy_at FROM findings \
             WHERE project = ?1 AND (?2 IS NULL OR worktree = ?2) ORDER BY worktree, check_id",
        )?;
        let rows = stmt
            .query_map(params![project, worktree], |r| {
                Ok(FindingRow {
                    project: r.get(0)?,
                    worktree: r.get(1)?,
                    check_id: r.get(2)?,
                    severity: r.get(3)?,
                    message: r.get(4)?,
                    detail: r.get(5)?,
                    first_seen: r.get(6)?,
                    last_seen: r.get(7)?,
                    occurrences: r.get(8)?,
                    cleared_at: r.get(9)?,
                    remedy: r.get(10)?,
                    remedy_at: r.get(11)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `PRAGMA data_version`: changes whenever another connection commits.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?)
    }
}

/// One project's `status.db` state, read once ([`StatusReader::project_status`]).
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectStatus {
    /// A live writer with herdr up: its session rows, by worktree.
    Fresh {
        writer: WriterRow,
        sessions: HashMap<String, StatusSession>,
    },
    /// A live writer with no usable herdr snapshot: its (possibly old) rows.
    NotReady {
        writer: WriterRow,
        sessions: HashMap<String, StatusSession>,
    },
    /// No app is updating the store.
    Stale { heartbeat_at: Option<i64> },
}

impl ProjectStatus {
    /// What the store says about `worktree`.
    pub fn engine_read(&self, worktree: &str) -> EngineRead {
        match self {
            ProjectStatus::Stale { heartbeat_at } => EngineRead::Stale {
                heartbeat_at: *heartbeat_at,
            },
            ProjectStatus::NotReady { writer, sessions } => EngineRead::NotReady {
                writer: writer.clone(),
                view: sessions.get(worktree).cloned(),
            },
            ProjectStatus::Fresh { writer, sessions } => match sessions.get(worktree) {
                Some(v) if v.stale => EngineRead::NotReady {
                    writer: writer.clone(),
                    view: Some(v.clone()),
                },
                view => EngineRead::Fresh {
                    view: view.cloned(),
                    writer: writer.clone(),
                },
            },
        }
    }
}

/// What `status.db` says about one worktree, as the CLI reads it.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineRead {
    /// A live app with herdr up: `view` is the engine's answer (`None`: no agent pane).
    Fresh {
        view: Option<StatusSession>,
        writer: WriterRow,
    },
    /// A live app with no usable herdr snapshot (link not up, no snapshot yet, or a
    /// stale view).
    NotReady {
        writer: WriterRow,
        view: Option<StatusSession>,
    },
    /// No app is updating `status.db` (missing, another schema, or no heartbeat for
    /// [`STATUS_FRESH_MS`]).
    Stale { heartbeat_at: Option<i64> },
}

impl EngineRead {
    /// `worktree`'s state from `reader` (`None`: no store to read, stale) at `now_ms`.
    pub fn of(reader: Option<&StatusReader>, project: &str, worktree: &str, now_ms: i64) -> Self {
        reader.map_or(EngineRead::Stale { heartbeat_at: None }, |r| {
            r.project_status(project, now_ms).engine_read(worktree)
        })
    }

    /// The engine view, when there is one.
    pub fn view(&self) -> Option<&StatusSession> {
        match self {
            EngineRead::Fresh { view, .. } | EngineRead::NotReady { view, .. } => view.as_ref(),
            EngineRead::Stale { .. } => None,
        }
    }
}

// ── Overlay ──────────────────────────────────────────────────────────────────

/// Overlay engine views (keyed by worktree) on a project's hook rows.
///
/// - `views == None` (engine not ready / store stale): `rows` scoped to
///   `configured`, each marked `stale`.
/// - Scope: worktrees in `configured` (all when empty) that have a row or a view;
///   sorted by worktree.
/// - A view sets `state`, `agent`, `herdr_status`, `stale`. An `offline` view
///   clears the task, pid and broker fields; any non-`waiting` view clears
///   `brokered` and `escalated_reason`.
/// - A view without a row synthesizes one (`driven_by` from `drivers`).
/// - A row without a view, once the engine is ready, reads `offline` / `absent`.
pub fn overlay_sessions(
    rows: Vec<WorktreeSession>,
    views: Option<&HashMap<String, StatusSession>>,
    configured: &[String],
    drivers: &HashMap<String, String>,
) -> Vec<WorktreeSession> {
    let in_scope = |wt: &str| configured.is_empty() || configured.iter().any(|c| c == wt);
    let Some(views) = views else {
        let mut out: Vec<WorktreeSession> = rows
            .into_iter()
            .filter(|r| in_scope(&r.worktree))
            .map(|mut r| {
                r.stale = true;
                r
            })
            .collect();
        out.sort_by(|a, b| a.worktree.cmp(&b.worktree));
        return out;
    };
    let mut by_wt: BTreeMap<String, WorktreeSession> = BTreeMap::new();
    for r in rows {
        if in_scope(&r.worktree) {
            by_wt.insert(r.worktree.clone(), r);
        }
    }
    let names: BTreeSet<String> = by_wt
        .keys()
        .cloned()
        .chain(views.keys().filter(|w| in_scope(w)).cloned())
        .collect();
    names
        .into_iter()
        .map(|wt| {
            let mut row = by_wt.remove(&wt).unwrap_or_else(|| WorktreeSession {
                project: String::new(),
                worktree: wt.clone(),
                state: String::new(),
                current_task_id: None,
                pid: None,
                last_heartbeat: String::new(),
                driven_by: drivers.get(&wt).cloned(),
                brokered: false,
                escalated_reason: None,
                agent: None,
                herdr_status: None,
                stale: false,
            });
            match views.get(&wt) {
                Some(v) => {
                    row.state = v.state.clone();
                    row.agent = Some(v.agent.clone());
                    row.herdr_status = v.herdr_status.clone();
                    row.stale = v.stale;
                }
                None => {
                    row.state = "offline".to_string();
                    row.agent = Some("absent".to_string());
                    row.herdr_status = None;
                    row.stale = false;
                }
            }
            apply_state_rules(&mut row);
            row
        })
        .collect()
}

/// Offline clears the task, pid and broker fields; only `waiting` keeps the broker
/// fields.
fn apply_state_rules(r: &mut WorktreeSession) {
    if r.state == "offline" {
        r.current_task_id = None;
        r.pid = None;
    }
    if r.state != "waiting" {
        r.brokered = false;
        r.escalated_reason = None;
    }
}

// ── Test helper ──────────────────────────────────────────────────────────────

/// Write a `status.db` at `path` holding exactly `writer` and `sessions` (each row's
/// `written_at` = the writer's heartbeat). For tests in this and other crates.
#[doc(hidden)]
#[cfg(any(test, feature = "test-support"))]
pub fn write_status_for_test(
    path: &Path,
    writer: WriterRow,
    sessions: &[StatusSession],
) -> Result<()> {
    let mut w = StatusWriter::open(path)?;
    let tx = w.conn.transaction()?;
    tx.execute("DELETE FROM writer", [])?;
    tx.execute("DELETE FROM sessions", [])?;
    tx.execute(
        "INSERT INTO writer (id, instance_id, pid, version, started_at, heartbeat_at, \
         last_snapshot_at, herdr_link, herdr_link_since) \
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            writer.instance_id,
            writer.pid,
            writer.version,
            writer.started_at,
            writer.heartbeat_at,
            writer.last_snapshot_at,
            writer.herdr_link,
            writer.herdr_link_since,
        ],
    )?;
    for s in sessions {
        insert_session(&tx, s, writer.heartbeat_at)?;
    }
    tx.commit()?;
    Ok(())
}

/// Insert or replace `row` in the `app_projects` table of the `status.db` at `path`,
/// lease or not. For tests in this and other crates.
#[doc(hidden)]
#[cfg(any(test, feature = "test-support"))]
pub fn write_app_project_for_test(path: &Path, row: &AppProject) -> Result<()> {
    let w = StatusWriter::open(path)?;
    insert_app_project(&w.conn, row)
}

/// Insert `f`, replacing the row of its key.
fn upsert_finding(conn: &rusqlite::Connection, f: &FindingRow) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO findings (project, worktree, check_id, severity, message, \
         detail, first_seen, last_seen, occurrences, cleared_at, remedy, remedy_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            f.project,
            f.worktree,
            f.check_id,
            f.severity,
            f.message,
            f.detail,
            f.first_seen,
            f.last_seen,
            f.occurrences,
            f.cleared_at,
            f.remedy,
            f.remedy_at,
        ],
    )?;
    Ok(())
}

/// Replace every `findings` row of the `status.db` at `path` with `rows`, lease or
/// not. For tests in other crates.
#[doc(hidden)]
#[cfg(any(test, feature = "test-support"))]
pub fn write_findings_for_test(path: &Path, rows: &[FindingRow]) -> Result<()> {
    let mut w = StatusWriter::open(path)?;
    let tx = w.conn.transaction()?;
    tx.execute("DELETE FROM findings", [])?;
    for f in rows {
        upsert_finding(&tx, f)?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
#[path = "status_store_tests.rs"]
mod tests;
