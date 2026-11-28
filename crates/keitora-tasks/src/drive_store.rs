//! The drive API on [`LocalStore`]: bindings, brokered prompts, pane requests, turn
//! replies and retention. Like `dispatch_requests` and `worktree_sessions`, drive
//! state is always local, so it is synchronous and not on the `TaskBackend` trait.
//!
//! Rule violations are `Error::Conflict` with a sentence for humans.

use std::collections::HashMap;

use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

use crate::drive::{
    truncate_bytes, truncate_chars, Baseline, Delivery, Drive, DriveBind, DrivePrompt,
    DriveRequest, DriveTurn, NewDrivePrompt, NewDriveTurn, PromptDecision, SessionReply,
    DRIVE_CONFIRM_MAX_AGE_SECS, DRIVE_DEFAULT_ESCALATE_AFTER_S, DRIVE_ESCALATE_AFTER_MAX_S,
    DRIVE_ESCALATE_AFTER_MIN_S, DRIVE_PERMISSION_INPUT_MAX, DRIVE_PROMPT_KINDS,
    DRIVE_PROMPT_STALE_SLACK_S, DRIVE_QUESTION_INPUT_MAX, DRIVE_READ_LINES_MAX,
    DRIVE_REQUEST_KINDS, DRIVE_REQUEST_OUTCOMES, DRIVE_SEND_TEXT_MAX, DRIVE_SUMMARY_MAX,
    DRIVE_TABLES, DRIVE_TURNS_KEEP_PER_WORKTREE, DRIVE_TURNS_RETENTION_DAYS, SESSION_REPLY_MAX,
};
use crate::local_store::{cached_exists, LocalStore};
use crate::sqlite_backend::{column_exists, table_exists};
use crate::{Error, Result};

const DRIVE_COLS: &str =
    "driven, driver, since, forced_from, paused, escalate_after_s, awaiting_turn";
const PROMPT_COLS: &str = "id, driven, driver, provider, kind, tool_name, tool_input, summary, \
     status, decision_json, decided_by, reason, timeout_s, hook_pid, created_at, decided_at, \
     MAX(0, CAST(strftime('%s', 'now') - strftime('%s', created_at) AS INTEGER))";
const DELIVERY_COLS: &str =
    "driven, driver, awaiting_turn, delivery_id, baseline_json, delivered_at";
const TURN_COLS: &str = "id, driven, kind, delivery_id, outcome, signal, clause, completed_at, \
     reply, reply_source, caveat, acked_at";
/// `strftime` text for "now" with milliseconds, as `drive_turns` stores it.
const NOW_MS_SQL: &str = "strftime('%Y-%m-%d %H:%M:%f','now')";
const REQUEST_COLS: &str = "id, kind, worktree, requested_by, payload, queue, status, reason, \
     result, created_at, resolved_at, seen_at, wait_reason, wait_reason_at";

/// SQL predicate: the prompt row (columns qualified by `alias`, or bare when empty) is
/// stale — older than its `timeout_s` plus the slack, so no live hook is polling it.
pub(crate) fn stale_sql(alias: &str) -> String {
    let p = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    format!(
        "{p}created_at < datetime('now', '-' || ({p}timeout_s + {DRIVE_PROMPT_STALE_SLACK_S}) || ' seconds')"
    )
}

fn conflict<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Conflict(msg.into()))
}

fn row_to_drive(r: &rusqlite::Row) -> rusqlite::Result<Drive> {
    Ok(Drive {
        driven: r.get(0)?,
        driver: r.get(1)?,
        since: r.get(2)?,
        forced_from: r.get(3)?,
        paused: r.get(4)?,
        escalate_after_s: r.get(5)?,
        awaiting_turn: r.get(6)?,
    })
}

fn row_to_session_reply(r: &rusqlite::Row) -> rusqlite::Result<SessionReply> {
    Ok(SessionReply {
        worktree: r.get(0)?,
        turn: r.get(1)?,
        last_message: r.get(2)?,
        updated_at: r.get(3)?,
    })
}

fn row_to_delivery(r: &rusqlite::Row) -> rusqlite::Result<Delivery> {
    let json: Option<String> = r.get(4)?;
    Ok(Delivery {
        driven: r.get(0)?,
        driver: r.get(1)?,
        awaiting_turn: r.get(2)?,
        delivery_id: r.get(3)?,
        baseline: json.and_then(|j| serde_json::from_str::<Baseline>(&j).ok()),
        delivered_at: r.get(5)?,
    })
}

fn row_to_turn(r: &rusqlite::Row) -> rusqlite::Result<DriveTurn> {
    Ok(DriveTurn {
        id: r.get(0)?,
        driven: r.get(1)?,
        kind: r.get(2)?,
        delivery_id: r.get(3)?,
        outcome: r.get(4)?,
        signal: r.get(5)?,
        clause: r.get(6)?,
        completed_at: r.get(7)?,
        reply: r.get(8)?,
        reply_source: r.get(9)?,
        caveat: r.get(10)?,
        acked_at: r.get(11)?,
    })
}

/// A herdr sequence number as SQLite stores it (saturating at `i64::MAX`).
fn seq_sql(v: Option<u64>) -> Option<i64> {
    v.map(|n| i64::try_from(n).unwrap_or(i64::MAX))
}

fn row_to_prompt(r: &rusqlite::Row) -> rusqlite::Result<DrivePrompt> {
    Ok(DrivePrompt {
        id: r.get(0)?,
        driven: r.get(1)?,
        driver: r.get(2)?,
        provider: r.get(3)?,
        kind: r.get(4)?,
        tool_name: r.get(5)?,
        tool_input: r.get(6)?,
        summary: r.get(7)?,
        status: r.get(8)?,
        decision_json: r.get(9)?,
        decided_by: r.get(10)?,
        reason: r.get(11)?,
        timeout_s: r.get(12)?,
        hook_pid: r.get(13)?,
        created_at: r.get(14)?,
        decided_at: r.get(15)?,
        age_s: r.get(16)?,
    })
}

fn row_to_request(r: &rusqlite::Row) -> rusqlite::Result<DriveRequest> {
    Ok(DriveRequest {
        id: r.get(0)?,
        kind: r.get(1)?,
        worktree: r.get(2)?,
        requested_by: r.get(3)?,
        payload: r.get(4)?,
        queue: r.get(5)?,
        status: r.get(6)?,
        reason: r.get(7)?,
        result: r.get(8)?,
        created_at: r.get(9)?,
        resolved_at: r.get(10)?,
        seen_at: r.get(11)?,
        wait_reason: r.get(12)?,
        wait_reason_at: r.get(13)?,
    })
}

/// Check a request payload against its kind. The message names the offending field.
fn validate_request_payload(kind: &str, payload: &serde_json::Value, queue: bool) -> Result<()> {
    if !DRIVE_REQUEST_KINDS.contains(&kind) {
        return conflict(format!("invalid drive request kind: {kind}"));
    }
    let Some(obj) = payload.as_object() else {
        return conflict(format!("{kind} payload must be a JSON object"));
    };
    if queue && kind != "send" {
        return conflict("queue applies to send only");
    }
    let to_field = |required: bool| -> Result<()> {
        match obj.get("to") {
            None if required => conflict(format!("{kind} needs \"to\" (a provider)")),
            None => Ok(()),
            Some(v) => match v.as_str() {
                Some(s) if !s.trim().is_empty() => Ok(()),
                _ => conflict("\"to\" must be a non-empty provider name"),
            },
        }
    };
    // Optional boolean flags: `send`'s `wake` (start a stopped agent first) and
    // `wake`'s `fresh`.
    let bool_field = |name: &str| -> Result<()> {
        match obj.get(name) {
            None => Ok(()),
            Some(v) if v.is_boolean() => Ok(()),
            Some(_) => conflict(format!("\"{name}\" must be true or false")),
        }
    };
    match kind {
        "send" => match obj.get("text").and_then(serde_json::Value::as_str) {
            None => conflict("send needs \"text\""),
            Some("") => conflict("\"text\" is empty"),
            Some(t) if t.len() > DRIVE_SEND_TEXT_MAX => {
                conflict(format!("\"text\" is over {DRIVE_SEND_TEXT_MAX} bytes"))
            }
            Some(_) => bool_field("wake"),
        },
        "read" => match obj.get("lines") {
            None => Ok(()),
            Some(v) => match v.as_i64() {
                Some(n) if (1..=DRIVE_READ_LINES_MAX).contains(&n) => Ok(()),
                _ => conflict(format!("\"lines\" must be 1..={DRIVE_READ_LINES_MAX}")),
            },
        },
        // Older CLIs filed a wake as a `reset` with `"mode": "wake"`, which an older app
        // ran as a reset; a wake is its own kind now, so only the plain mode is taken.
        "reset" => match obj.get("mode").map(serde_json::Value::as_str) {
            None | Some(Some("reset")) => to_field(false),
            Some(_) => conflict("\"mode\" must be \"reset\"; file a wake as kind \"wake\""),
        },
        // Start an agent that isn't running, resuming unless `fresh`.
        "wake" => bool_field("fresh").and_then(|()| to_field(false)),
        "swap" => to_field(true),
        _ => unreachable!("kind validated above"),
    }
}

impl LocalStore {
    /// Begin a `BEGIN IMMEDIATE` transaction on this handle's connection. Dropped
    /// without `commit` it rolls back, so an early `Err` return leaves no partial write.
    fn drive_tx(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(
            &self.conn,
            TransactionBehavior::Immediate,
        )?)
    }

    /// Whether this DB has the drive tables. A read-only handle on a DB that predates
    /// them has none (read-only opens never create tables); read paths then answer as
    /// if nothing were driven.
    pub(crate) fn drive_tables_exist(&self) -> Result<bool> {
        cached_exists(&self.drive_tables_seen, || {
            let n: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN (?1, ?2, ?3, ?4)",
                params![
                    DRIVE_TABLES[0],
                    DRIVE_TABLES[1],
                    DRIVE_TABLES[2],
                    DRIVE_TABLES[3]
                ],
                |r| r.get(0),
            )?;
            Ok(n == DRIVE_TABLES.len() as i64)
        })
    }

    /// Mark `driven`'s pending prompts `escalated` (the native dialog takes over).
    fn escalate_pending(&self, driven: &str, decided_by: &str, reason: &str) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE drive_prompts \
             SET status = 'escalated', decided_by = ?2, reason = ?3, decided_at = datetime('now') \
             WHERE driven = ?1 AND status = 'pending'",
            params![driven, decided_by, reason],
        )?;
        Ok(n)
    }

    // ---- Drives ----

    /// Bind `driver` to drive `driven`. Drives are one level deep, a worktree can't
    /// drive itself, and a worktree has one driver; `force` takes over an existing
    /// driver's binding (its pending prompts go to the human).
    pub fn drive_bind(
        &self,
        driver: &str,
        driven: &str,
        escalate_after_s: Option<i64>,
        force: bool,
    ) -> Result<DriveBind> {
        if driver == driven {
            return conflict("a worktree can't drive itself");
        }
        if let Some(s) = escalate_after_s {
            if !(DRIVE_ESCALATE_AFTER_MIN_S..=DRIVE_ESCALATE_AFTER_MAX_S).contains(&s) {
                return conflict(format!(
                    "escalate-after must be {DRIVE_ESCALATE_AFTER_MIN_S}..={DRIVE_ESCALATE_AFTER_MAX_S} seconds"
                ));
            }
        }
        let tx = self.drive_tx()?;
        if let Some(d) = self.drive_get(driver)? {
            return conflict(format!(
                "@{driver} is driven by @{}; drives are one level only",
                d.driver
            ));
        }
        if !self.drives_by_driver(driven)?.is_empty() {
            return conflict(format!(
                "@{driven} drives other worktrees; drives are one level only"
            ));
        }
        let existing = self.drive_get(driven)?;
        let mut previous_driver = None;
        let mut escalated_prompts = 0;
        match existing {
            None => {
                tx.execute(
                    "INSERT INTO drives (driven, driver, escalate_after_s) VALUES (?1, ?2, ?3)",
                    params![
                        driven,
                        driver,
                        escalate_after_s.unwrap_or(DRIVE_DEFAULT_ESCALATE_AFTER_S)
                    ],
                )?;
            }
            Some(d) if d.driver == driver => {
                if let Some(s) = escalate_after_s {
                    tx.execute(
                        "UPDATE drives SET escalate_after_s = ?2 WHERE driven = ?1",
                        params![driven, s],
                    )?;
                }
            }
            Some(d) => {
                if !force {
                    return conflict(format!(
                        "@{driven} is already driven by @{} (use --force to take over)",
                        d.driver
                    ));
                }
                escalated_prompts = self.escalate_pending(
                    driven,
                    "system",
                    &format!("drive taken over by @{driver}"),
                )?;
                tx.execute(
                    "UPDATE drives SET driver = ?2, forced_from = ?3, since = datetime('now'), \
                       paused = 0, awaiting_turn = NULL, escalate_after_s = ?4 \
                     WHERE driven = ?1",
                    params![
                        driven,
                        driver,
                        d.driver,
                        escalate_after_s.unwrap_or(DRIVE_DEFAULT_ESCALATE_AFTER_S)
                    ],
                )?;
                previous_driver = Some(d.driver);
            }
        }
        let drive = self
            .drive_get(driven)?
            .ok_or_else(|| Error::Conflict(format!("drive of @{driven} vanished")))?;
        tx.commit()?;
        Ok(DriveBind {
            drive,
            previous_driver,
            escalated_prompts,
        })
    }

    /// `driven`'s binding, if any.
    pub fn drive_get(&self, driven: &str) -> Result<Option<Drive>> {
        if !self.drive_tables_exist()? {
            return Ok(None);
        }
        let row = self
            .conn
            .query_row(
                &format!("SELECT {DRIVE_COLS} FROM drives WHERE driven = ?1"),
                params![driven],
                row_to_drive,
            )
            .optional()?;
        Ok(row)
    }

    /// Every binding, by driven worktree.
    pub fn drives_list(&self) -> Result<Vec<Drive>> {
        if !self.drive_tables_exist()? {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {DRIVE_COLS} FROM drives ORDER BY driven"))?;
        let rows = stmt
            .query_map([], row_to_drive)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The bindings `driver` holds, by driven worktree.
    pub fn drives_by_driver(&self, driver: &str) -> Result<Vec<Drive>> {
        if !self.drive_tables_exist()? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DRIVE_COLS} FROM drives WHERE driver = ?1 ORDER BY driven"
        ))?;
        let rows = stmt
            .query_map(params![driver], row_to_drive)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `driver` stops driving `driven`. Its pending prompts go to the human. Returns
    /// how many were escalated.
    pub fn drive_release(&self, driver: &str, driven: &str) -> Result<usize> {
        let tx = self.drive_tx()?;
        match self.drive_get(driven)? {
            Some(d) if d.driver == driver => {}
            _ => return conflict(format!("@{driven} isn't driven by @{driver}")),
        }
        let n = self.escalate_pending(driven, "system", &format!("released by @{driver}"))?;
        tx.execute("DELETE FROM drives WHERE driven = ?1", params![driven])?;
        tx.commit()?;
        Ok(n)
    }

    /// Release every worktree `driver` drives: `(driven, escalated_prompts)` by name.
    /// Empty (not an error) when it drives none.
    pub fn drive_release_all(&self, driver: &str) -> Result<Vec<(String, usize)>> {
        let tx = self.drive_tx()?;
        let reason = format!("released by @{driver}");
        let mut out = Vec::new();
        for d in self.drives_by_driver(driver)? {
            let n = self.escalate_pending(&d.driven, "system", &reason)?;
            tx.execute("DELETE FROM drives WHERE driven = ?1", params![d.driven])?;
            out.push((d.driven, n));
        }
        tx.commit()?;
        Ok(out)
    }

    /// The human ends `driven`'s drive, whoever drives it. Returns how many pending
    /// prompts were escalated.
    pub fn drive_end(&self, driven: &str) -> Result<usize> {
        let tx = self.drive_tx()?;
        if self.drive_get(driven)?.is_none() {
            return conflict(format!("@{driven} isn't driven"));
        }
        let n = self.escalate_pending(driven, "system", "released by you")?;
        tx.execute("DELETE FROM drives WHERE driven = ?1", params![driven])?;
        tx.commit()?;
        Ok(n)
    }

    /// The human pauses or resumes `driven`'s drive. Pausing escalates its pending
    /// prompts (returned count); resuming, or setting the current value, returns 0.
    pub fn drive_set_paused(&self, driven: &str, paused: bool) -> Result<usize> {
        let tx = self.drive_tx()?;
        let Some(d) = self.drive_get(driven)? else {
            return conflict(format!("@{driven} isn't driven"));
        };
        if d.paused == paused {
            return Ok(0);
        }
        tx.execute(
            "UPDATE drives SET paused = ?2 WHERE driven = ?1",
            params![driven, paused],
        )?;
        let n = if paused {
            self.escalate_pending(driven, "system", "driver paused")?
        } else {
            0
        };
        tx.commit()?;
        Ok(n)
    }

    /// The human takes over `driven`'s pending prompts; the binding stays. Returns the
    /// count, 0 when `driven` isn't driven.
    pub fn drive_take_over(&self, driven: &str) -> Result<usize> {
        let tx = self.drive_tx()?;
        if self.drive_get(driven)?.is_none() {
            return Ok(0);
        }
        let n = self.escalate_pending(driven, "human", "taken over by you")?;
        tx.commit()?;
        Ok(n)
    }

    /// Set `awaiting_turn` to exactly `value` (`None` clears it): clears it after a
    /// `reset`/`swap`, whose turn no longer exists. `false` when
    /// `driven` isn't driven.
    pub fn drive_restore_awaiting_turn(&self, driven: &str, value: Option<i64>) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE drives SET awaiting_turn = ?2 WHERE driven = ?1",
            params![driven, value],
        )?;
        Ok(n == 1)
    }

    /// The finished `turn` was delivered to the driver: clear `awaiting_turn` only if it
    /// still equals `turn`, so a newer `send` is never cleared by a stale ack.
    pub fn drive_ack_turn(&self, driven: &str, turn: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE drives SET awaiting_turn = NULL WHERE driven = ?1 AND awaiting_turn = ?2",
            params![driven, turn],
        )?;
        Ok(n == 1)
    }

    // ---- Deliveries and turns ----

    /// Whether `drives` has the delivery columns. An older DB opened read-only lacks
    /// them (read-only opens never alter tables); a positive answer is cached.
    pub fn drive_delivery_columns_exist(&self) -> Result<bool> {
        if !self.delivery_columns_seen.get() && !self.drive_tables_exist()? {
            return Ok(false);
        }
        cached_exists(&self.delivery_columns_seen, || {
            column_exists(&self.conn, "drives", "delivery_id")
        })
    }

    /// Whether the `drive_turns` table exists; a positive answer is cached.
    pub fn drive_turns_exist(&self) -> Result<bool> {
        cached_exists(&self.drive_turns_seen, || {
            table_exists(&self.conn, "drive_turns")
        })
    }

    /// `driven`'s delivery columns; `None` when it isn't driven or the DB predates them.
    pub fn drive_delivery_get(&self, driven: &str) -> Result<Option<Delivery>> {
        if !self.drive_delivery_columns_exist()? {
            return Ok(None);
        }
        let row = self
            .conn
            .query_row(
                &format!("SELECT {DELIVERY_COLS} FROM drives WHERE driven = ?1"),
                params![driven],
                row_to_delivery,
            )
            .optional()?;
        Ok(row)
    }

    /// Every drive with a send outstanding (`awaiting_turn` set) and no turn recorded
    /// for its delivery yet, by driven worktree. Empty when the DB predates the
    /// delivery columns.
    pub fn drive_deliveries(&self) -> Result<Vec<Delivery>> {
        if !self.drive_delivery_columns_exist()? {
            return Ok(Vec::new());
        }
        let unturned = if self.drive_turns_exist()? {
            " AND NOT EXISTS (SELECT 1 FROM drive_turns t WHERE t.delivery_id = drives.delivery_id)"
        } else {
            ""
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DELIVERY_COLS} FROM drives WHERE awaiting_turn IS NOT NULL{unturned} \
             ORDER BY driven"
        ))?;
        let rows = stmt
            .query_map([], row_to_delivery)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A prompt is about to be typed into `driven`: in one write, wait for the turn
    /// after the last hook-recorded one, take the next `delivery_id` (above every id on
    /// a drive or a turn row, so ids never repeat across a release and rebind) and
    /// store the baseline. Returns `(awaiting_turn, delivery_id)`; `None` when `driven`
    /// isn't driven.
    pub fn drive_begin_delivery(&self, driven: &str, b: &Baseline) -> Result<Option<(i64, i64)>> {
        let json = serde_json::to_string(b)?;
        let v = self
            .conn
            .query_row(
                &format!(
                    "UPDATE drives SET \
                       awaiting_turn = COALESCE((SELECT turn FROM session_replies WHERE worktree = ?1), 0) + 1, \
                       delivery_id = (SELECT COALESCE(MAX(x), 0) + 1 FROM \
                         (SELECT delivery_id AS x FROM drives UNION ALL SELECT delivery_id FROM drive_turns)), \
                       baseline_json = ?2, delivered_at = {NOW_MS_SQL} \
                     WHERE driven = ?1 RETURNING awaiting_turn, delivery_id"
                ),
                params![driven, json],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;
        Ok(v)
    }

    /// Put `prev`'s four delivery columns back (a delivery whose typing failed). `false`
    /// when the drive is gone.
    pub fn drive_restore_delivery(&self, prev: &Delivery) -> Result<bool> {
        let json = match &prev.baseline {
            Some(b) => Some(serde_json::to_string(b)?),
            None => None,
        };
        let n = self.conn.execute(
            "UPDATE drives SET awaiting_turn = ?2, delivery_id = ?3, baseline_json = ?4, \
               delivered_at = ?5 WHERE driven = ?1",
            params![
                prev.driven,
                prev.awaiting_turn,
                prev.delivery_id,
                json,
                prev.delivered_at
            ],
        )?;
        Ok(n == 1)
    }

    /// Record a turn. A `driver` turn is written only while its delivery is still
    /// outstanding on the drive and only once per `delivery_id`. Returns the new row id,
    /// or `None` when nothing was written.
    pub fn drive_turn_insert(&self, t: &NewDriveTurn) -> Result<Option<i64>> {
        let tx = self.drive_tx()?;
        if t.kind == "driver" {
            let outstanding = tx
                .prepare(
                    "SELECT 1 FROM drives \
                     WHERE driven = ?1 AND delivery_id = ?2 AND awaiting_turn IS NOT NULL",
                )?
                .exists(params![t.driven, t.delivery_id])?;
            if !outstanding {
                return Ok(None);
            }
        }
        let n = tx.execute(
            // `ON CONFLICT DO NOTHING` covers the unique delivery index only; unlike
            // `OR IGNORE` it still fails a CHECK.
            "INSERT INTO drive_turns (driven, kind, delivery_id, outcome, signal, \
               clause, completion_seq, state_change_seq, reply, reply_source, caveat) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) ON CONFLICT DO NOTHING",
            params![
                t.driven,
                t.kind,
                t.delivery_id,
                t.outcome,
                t.signal,
                t.clause,
                seq_sql(t.completion_seq),
                seq_sql(t.state_change_seq),
                truncate_bytes(t.reply, SESSION_REPLY_MAX),
                t.reply_source,
                t.caveat
            ],
        )?;
        let id = (n == 1).then(|| self.conn.last_insert_rowid());
        tx.commit()?;
        Ok(id)
    }

    /// `driven`'s unacked turns, oldest first. Empty when the table is absent.
    pub fn drive_turns_unacked(&self, driven: &str) -> Result<Vec<DriveTurn>> {
        if !self.drive_turns_exist()? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TURN_COLS} FROM drive_turns WHERE driven = ?1 AND acked_at IS NULL ORDER BY id"
        ))?;
        let rows = stmt
            .query_map(params![driven], row_to_turn)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `driven`'s newest `limit` turns, newest first (acked or not, any outcome).
    pub fn drive_turns_recent(&self, driven: &str, limit: usize) -> Result<Vec<DriveTurn>> {
        if !self.drive_turns_exist()? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TURN_COLS} FROM drive_turns WHERE driven = ?1 ORDER BY id DESC LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(params![driven, limit as i64], row_to_turn)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `driven`'s newest turn (acked or not, any outcome).
    pub fn drive_turn_latest(&self, driven: &str) -> Result<Option<DriveTurn>> {
        if !self.drive_turns_exist()? {
            return Ok(None);
        }
        let row = self
            .conn
            .query_row(
                &format!(
                    "SELECT {TURN_COLS} FROM drive_turns WHERE driven = ?1 ORDER BY id DESC LIMIT 1"
                ),
                params![driven],
                row_to_turn,
            )
            .optional()?;
        Ok(row)
    }

    /// The turn recorded for `delivery_id`, if any.
    pub fn drive_turn_for_delivery(&self, delivery_id: i64) -> Result<Option<DriveTurn>> {
        if !self.drive_turns_exist()? {
            return Ok(None);
        }
        let row = self
            .conn
            .query_row(
                &format!("SELECT {TURN_COLS} FROM drive_turns WHERE delivery_id = ?1"),
                params![delivery_id],
                row_to_turn,
            )
            .optional()?;
        Ok(row)
    }

    /// The turn `id` was delivered to the driver. A driver turn also ends its delivery:
    /// `awaiting_turn` is cleared only while the drive still holds that `delivery_id`,
    /// so a newer send is never cleared. `true` iff this call acked the row.
    pub fn drive_turn_ack(&self, id: i64) -> Result<bool> {
        let tx = self.drive_tx()?;
        let n = tx.execute(
            &format!(
                "UPDATE drive_turns SET acked_at = {NOW_MS_SQL} WHERE id = ?1 AND acked_at IS NULL"
            ),
            params![id],
        )?;
        if n != 1 {
            return Ok(false);
        }
        let (driven, kind, delivery_id): (String, String, Option<i64>) = tx.query_row(
            "SELECT driven, kind, delivery_id FROM drive_turns WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if let (true, Some(delivery_id)) = (kind == "driver", delivery_id) {
            tx.execute(
                "UPDATE drives SET awaiting_turn = NULL WHERE driven = ?1 AND delivery_id = ?2",
                params![driven, delivery_id],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// The legacy ack: [`LocalStore::drive_ack_turn`]'s compare-and-clear, plus, on
    /// success, acking any turn row recorded for the same delivery so it is never
    /// delivered a second time.
    pub fn drive_ack_turn_legacy(&self, driven: &str, awaiting: i64) -> Result<bool> {
        let tx = self.drive_tx()?;
        let delivery_id: Option<i64> = if self.drive_delivery_columns_exist()? {
            tx.query_row(
                "SELECT delivery_id FROM drives WHERE driven = ?1",
                params![driven],
                |r| r.get(0),
            )
            .optional()?
            .flatten()
        } else {
            None
        };
        if !self.drive_ack_turn(driven, awaiting)? {
            return Ok(false);
        }
        if let (Some(id), true) = (delivery_id, self.drive_turns_exist()?) {
            tx.execute(
                &format!(
                    "UPDATE drive_turns SET acked_at = {NOW_MS_SQL} \
                     WHERE driven = ?1 AND delivery_id = ?2 AND acked_at IS NULL"
                ),
                params![driven, id],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Delete turn rows older than [`DRIVE_TURNS_RETENTION_DAYS`] (counted from the ack,
    /// or from completion when never acked) that are not among their worktree's newest
    /// [`DRIVE_TURNS_KEEP_PER_WORKTREE`]. An unacked row for its drive's outstanding
    /// delivery is kept: without it the delivery would read as unturned and its turn
    /// would be recorded again. Returns the rows deleted.
    pub fn drive_turns_prune(&self) -> Result<usize> {
        if !self.drive_turns_exist()? {
            return Ok(0);
        }
        let outstanding = if self.drive_delivery_columns_exist()? {
            " AND NOT (acked_at IS NULL AND delivery_id IS NOT NULL AND EXISTS \
               (SELECT 1 FROM drives d WHERE d.driven = drive_turns.driven \
                  AND d.awaiting_turn IS NOT NULL AND d.delivery_id = drive_turns.delivery_id))"
        } else {
            ""
        };
        let n = self.conn.execute(
            &format!(
                "DELETE FROM drive_turns \
                 WHERE COALESCE(acked_at, completed_at) \
                         < strftime('%Y-%m-%d %H:%M:%f','now','-{DRIVE_TURNS_RETENTION_DAYS} days') \
                   AND id NOT IN (SELECT t.id FROM drive_turns t \
                                  WHERE t.driven = drive_turns.driven \
                                  ORDER BY t.id DESC LIMIT {DRIVE_TURNS_KEEP_PER_WORKTREE})\
                   {outstanding}"
            ),
            [],
        )?;
        Ok(n)
    }

    // ---- Prompts ----

    /// Record a brokered prompt for the driver to decide. Fails when `driven` isn't
    /// driven or its drive is paused (the gate then escalates). Copies the driver and
    /// `timeout_s` from the binding and truncates the input and summary.
    pub fn drive_prompt_insert(&self, new: &NewDrivePrompt) -> Result<DrivePrompt> {
        if !DRIVE_PROMPT_KINDS.contains(&new.kind.as_str()) {
            return conflict(format!("invalid prompt kind: {}", new.kind));
        }
        let tx = self.drive_tx()?;
        self.drive_prompts_expire_stale()?;
        let Some(d) = self.drive_get(&new.driven)? else {
            return conflict(format!("@{} is not driven", new.driven));
        };
        if d.paused {
            return conflict("drive is paused");
        }
        let input_max = if new.kind == "question" {
            DRIVE_QUESTION_INPUT_MAX
        } else {
            DRIVE_PERMISSION_INPUT_MAX
        };
        tx.execute(
            "INSERT INTO drive_prompts \
               (driven, driver, provider, kind, tool_name, tool_input, summary, timeout_s, hook_pid) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                new.driven,
                d.driver,
                new.provider,
                new.kind,
                new.tool_name,
                truncate_chars(&new.tool_input, input_max),
                truncate_chars(&new.summary, DRIVE_SUMMARY_MAX),
                d.escalate_after_s,
                new.hook_pid
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        let row = self
            .drive_prompt_get(id)?
            .ok_or_else(|| Error::Conflict(format!("prompt {id} vanished")))?;
        tx.commit()?;
        Ok(row)
    }

    /// One prompt, raw (a stale `pending` row reads as `pending` here).
    pub fn drive_prompt_get(&self, id: i64) -> Result<Option<DrivePrompt>> {
        let row = self
            .conn
            .query_row(
                &format!("SELECT {PROMPT_COLS} FROM drive_prompts WHERE id = ?1"),
                params![id],
                row_to_prompt,
            )
            .optional()?;
        Ok(row)
    }

    /// Live (pending, not stale) prompts, optionally for one driver and/or driven
    /// worktree, oldest first.
    pub fn drive_prompts_pending(
        &self,
        driver: Option<&str>,
        driven: Option<&str>,
    ) -> Result<Vec<DrivePrompt>> {
        if !self.drive_tables_exist()? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PROMPT_COLS} FROM drive_prompts \
             WHERE status = 'pending' AND NOT ({stale}) \
               AND (?1 IS NULL OR driver = ?1) AND (?2 IS NULL OR driven = ?2) \
             ORDER BY id",
            stale = stale_sql("")
        ))?;
        let rows = stmt
            .query_map(params![driver, driven], row_to_prompt)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Live (pending, not stale) prompt counts per driven worktree; worktrees with none
    /// are absent.
    pub fn drive_pending_prompt_counts(&self) -> Result<HashMap<String, usize>> {
        if !self.drive_tables_exist()? {
            return Ok(HashMap::new());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT driven, COUNT(*) FROM drive_prompts \
             WHERE status = 'pending' AND NOT ({stale}) GROUP BY driven",
            stale = stale_sql("")
        ))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(rows)
    }

    /// The driver `by_driver` decides prompt `id`. Compare-and-set on `pending`, so of
    /// two racing decisions one wins and the other gets a `Conflict`.
    pub fn drive_prompt_decide(
        &self,
        id: i64,
        by_driver: &str,
        decision: &PromptDecision,
    ) -> Result<DrivePrompt> {
        let tx = self.drive_tx()?;
        let Some(p) = self.drive_prompt_get(id)? else {
            return conflict(format!("no prompt {id}"));
        };
        if p.driver != by_driver {
            return conflict(format!("prompt {id} belongs to @{}'s drive", p.driver));
        }
        if self.drive_get(&p.driven)?.is_some_and(|d| d.paused) {
            return conflict("drive is paused");
        }
        if p.status != "pending" {
            return conflict(format!("prompt {id} is already {}", p.status));
        }
        let stale: bool = self.conn.query_row(
            &format!("SELECT {} FROM drive_prompts WHERE id = ?1", stale_sql("")),
            params![id],
            |r| r.get(0),
        )?;
        if stale {
            self.conn.execute(
                "UPDATE drive_prompts SET status = 'expired', decided_by = 'timeout', \
                   reason = 'hook gone: no decision recorded', decided_at = datetime('now') \
                 WHERE id = ?1 AND status = 'pending'",
                params![id],
            )?;
            tx.commit()?;
            return conflict(format!("prompt {id} expired"));
        }
        let (status, json, reason) = match decision {
            PromptDecision::Allow => {
                if p.kind != "permission" {
                    return conflict(
                        "allow applies to permission prompts; use answer for questions",
                    );
                }
                (
                    "allowed",
                    Some(serde_json::json!({"behavior": "allow"})),
                    String::new(),
                )
            }
            PromptDecision::Deny { reason } => {
                let reason = if reason.trim().is_empty() {
                    "denied by driver".to_string()
                } else {
                    reason.clone()
                };
                (
                    "denied",
                    Some(serde_json::json!({"behavior": "deny", "message": reason})),
                    reason,
                )
            }
            PromptDecision::Answer { answers } => {
                if p.kind != "question" {
                    return conflict("answer applies to question prompts only");
                }
                (
                    "answered",
                    Some(serde_json::json!({ "answers": answers })),
                    String::new(),
                )
            }
            PromptDecision::Escalate { reason } => {
                if reason.trim().is_empty() {
                    return conflict("escalate needs a reason");
                }
                ("escalated", None, reason.clone())
            }
        };
        let json = json.map(|v| v.to_string());
        let n = tx.execute(
            "UPDATE drive_prompts \
             SET status = ?2, decision_json = ?3, decided_by = ?4, reason = ?5, \
                 decided_at = datetime('now') \
             WHERE id = ?1 AND status = 'pending'",
            params![id, status, json, format!("driver:{by_driver}"), reason],
        )?;
        if n != 1 {
            return conflict(format!("prompt {id} is no longer pending"));
        }
        let row = self
            .drive_prompt_get(id)?
            .ok_or_else(|| Error::Conflict(format!("no prompt {id}")))?;
        tx.commit()?;
        Ok(row)
    }

    /// Close a pending prompt without a decision (`escalated`, `expired` or
    /// `withdrawn`); a close never produces `allowed`. `true` when this call changed it.
    pub fn drive_prompt_close(
        &self,
        id: i64,
        status: &str,
        decided_by: &str,
        reason: &str,
    ) -> Result<bool> {
        if !["escalated", "expired", "withdrawn"].contains(&status) {
            return conflict(format!("invalid prompt close status: {status}"));
        }
        let n = self.conn.execute(
            "UPDATE drive_prompts \
             SET status = ?2, decided_by = ?3, reason = ?4, decided_at = datetime('now') \
             WHERE id = ?1 AND status = 'pending'",
            params![id, status, decided_by, reason],
        )?;
        Ok(n == 1)
    }

    /// The driven session changed state under a pending prompt: withdraw its prompts.
    pub fn drive_prompts_withdraw(&self, worktree: &str) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE drive_prompts \
             SET status = 'withdrawn', decided_by = 'system', reason = 'session state changed', \
                 decided_at = datetime('now') \
             WHERE driven = ?1 AND status = 'pending'",
            params![worktree],
        )?;
        Ok(n)
    }

    /// Rewrite stale pending prompts (their hook is gone) to `expired`.
    pub fn drive_prompts_expire_stale(&self) -> Result<usize> {
        let n = self.conn.execute(
            &format!(
                "UPDATE drive_prompts \
                 SET status = 'expired', decided_by = 'timeout', \
                     reason = 'hook gone: no decision recorded', decided_at = datetime('now') \
                 WHERE status = 'pending' AND {}",
                stale_sql("")
            ),
            [],
        )?;
        Ok(n)
    }

    // ---- Requests ----

    /// Ask the running app to apply a pane request on `worktree` for its driver
    /// `requested_by`. Returns the `pending` row.
    pub fn drive_request_add(
        &self,
        kind: &str,
        worktree: &str,
        requested_by: &str,
        payload: &serde_json::Value,
        queue: bool,
    ) -> Result<DriveRequest> {
        validate_request_payload(kind, payload, queue)?;
        let tx = self.drive_tx()?;
        let Some(d) = self.drive_get(worktree)? else {
            return conflict(format!("@{worktree} is not driven"));
        };
        if d.driver != requested_by {
            return conflict(format!("@{requested_by} is not the driver of @{worktree}"));
        }
        // A pane read changes nothing in the driven session, so a paused drive allows it.
        if d.paused && kind != "read" {
            return conflict("drive is paused");
        }
        tx.execute(
            "INSERT INTO drive_requests (kind, worktree, requested_by, payload, queue) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind, worktree, requested_by, payload.to_string(), queue],
        )?;
        let id = self.conn.last_insert_rowid();
        let row = self
            .drive_request_get(id)?
            .ok_or_else(|| Error::Conflict(format!("drive request {id} vanished")))?;
        tx.commit()?;
        Ok(row)
    }

    /// One request by id.
    pub fn drive_request_get(&self, id: i64) -> Result<Option<DriveRequest>> {
        let row = self
            .conn
            .query_row(
                &format!("SELECT {REQUEST_COLS} FROM drive_requests WHERE id = ?1"),
                params![id],
                row_to_request,
            )
            .optional()?;
        Ok(row)
    }

    fn drive_requests_where(&self, cond: &str) -> Result<Vec<DriveRequest>> {
        if !self.drive_tables_exist()? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {REQUEST_COLS} FROM drive_requests WHERE {cond} ORDER BY id"
        ))?;
        let rows = stmt
            .query_map([], row_to_request)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every `pending` request, oldest first (the app's execution order).
    pub fn drive_requests_pending(&self) -> Result<Vec<DriveRequest>> {
        self.drive_requests_where("status = 'pending'")
    }

    /// Every `running` or `pending_confirm` request, oldest first.
    pub fn drive_requests_in_flight(&self) -> Result<Vec<DriveRequest>> {
        self.drive_requests_where("status IN ('running', 'pending_confirm')")
    }

    /// `requested_by`'s unresolved (`pending`, `running`, `pending_confirm`) `kind`
    /// requests on `worktree`, oldest first.
    pub fn drive_requests_open(
        &self,
        kind: &str,
        worktree: &str,
        requested_by: &str,
    ) -> Result<Vec<DriveRequest>> {
        if !self.drive_tables_exist()? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {REQUEST_COLS} FROM drive_requests \
             WHERE kind = ?1 AND worktree = ?2 AND requested_by = ?3 \
               AND status IN ('pending', 'running', 'pending_confirm') \
             ORDER BY id"
        ))?;
        let rows = stmt
            .query_map(params![kind, worktree, requested_by], row_to_request)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `requested_by`'s newest `kind` request on `worktree`, in any status.
    pub fn drive_request_latest(
        &self,
        kind: &str,
        worktree: &str,
        requested_by: &str,
    ) -> Result<Option<DriveRequest>> {
        if !self.drive_tables_exist()? {
            return Ok(None);
        }
        let row = self
            .conn
            .query_row(
                &format!(
                    "SELECT {REQUEST_COLS} FROM drive_requests \
                     WHERE kind = ?1 AND worktree = ?2 AND requested_by = ?3 \
                     ORDER BY id DESC LIMIT 1"
                ),
                params![kind, worktree, requested_by],
                row_to_request,
            )
            .optional()?;
        Ok(row)
    }

    /// The most recent `limit` requests, newest first.
    pub fn drive_requests_list(&self, limit: i64) -> Result<Vec<DriveRequest>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {REQUEST_COLS} FROM drive_requests ORDER BY id DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map(params![limit], row_to_request)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn drive_request_cas(&self, id: i64, from: &str, to: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE drive_requests SET status = ?3 WHERE id = ?1 AND status = ?2",
            params![id, from, to],
        )?;
        Ok(n == 1)
    }

    /// Take a `pending` request (`running`). `true` when this call won.
    pub fn drive_request_begin(&self, id: i64) -> Result<bool> {
        self.drive_request_cas(id, "pending", "running")
    }

    /// A running `swap` is showing its confirm to the human (`pending_confirm`).
    /// Stamps `confirm_started_at`, which ages the row for expiry from here on.
    pub fn drive_request_await_confirm(&self, id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE drive_requests \
             SET status = 'pending_confirm', confirm_started_at = datetime('now') \
             WHERE id = ?1 AND status = 'running'",
            params![id],
        )?;
        Ok(n == 1)
    }

    /// Put a `running` request back to `pending` (a queued send deferred at the last
    /// moment). `true` when this call moved it.
    pub fn drive_request_requeue(&self, id: i64) -> Result<bool> {
        self.drive_request_cas(id, "running", "pending")
    }

    /// The human accepted the confirm; the request runs again.
    pub fn drive_request_resume(&self, id: i64) -> Result<bool> {
        self.drive_request_cas(id, "pending_confirm", "running")
    }

    /// Record a request's outcome (`applied` or `refused`) once.
    pub fn drive_request_resolve(
        &self,
        id: i64,
        status: &str,
        reason: &str,
        result: &str,
    ) -> Result<()> {
        if !DRIVE_REQUEST_OUTCOMES.contains(&status) {
            return conflict(format!("invalid drive request outcome: {status}"));
        }
        let n = self.conn.execute(
            "UPDATE drive_requests \
             SET status = ?2, reason = ?3, result = ?4, resolved_at = datetime('now') \
             WHERE id = ?1 AND status IN ('pending', 'running', 'pending_confirm')",
            params![id, status, reason, result],
        )?;
        if n == 1 {
            return Ok(());
        }
        match self.drive_request_get(id)? {
            None => conflict(format!("no drive request {id}")),
            Some(r) => conflict(format!("drive request {id} is already {}", r.status)),
        }
    }

    /// Withdraw a request the app hasn't begun (the requester stopped waiting).
    pub fn drive_request_cancel(&self, id: i64, reason: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE drive_requests \
             SET status = 'refused', reason = ?2, resolved_at = datetime('now') \
             WHERE id = ?1 AND status = 'pending'",
            params![id, reason],
        )?;
        Ok(n == 1)
    }

    /// A pass examined pending request `id` and leaves it waiting for `reason`: stamp
    /// `seen_at` once and record the reason. `true` when the row changed; the same
    /// reason again (or a row that is no longer pending) writes nothing, so idle passes
    /// leave the DB untouched.
    pub fn drive_request_note_wait(&self, id: i64, reason: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE drive_requests \
             SET seen_at = COALESCE(seen_at, datetime('now')), \
                 wait_reason = ?2, wait_reason_at = datetime('now') \
             WHERE id = ?1 AND status = 'pending' \
               AND (seen_at IS NULL OR wait_reason <> ?2)",
            params![id, reason],
        )?;
        Ok(n == 1)
    }

    /// Whether any drive request is `pending`. Reads only `status`, so it works on a
    /// table of any age (and through a read-only handle that never migrated it).
    pub fn drive_requests_has_pending(&self) -> Result<bool> {
        if !self.drive_tables_exist()? {
            return Ok(false);
        }
        Ok(self
            .conn
            .prepare("SELECT 1 FROM drive_requests WHERE status = 'pending' LIMIT 1")?
            .exists([])?)
    }

    /// Refuse `pending` requests older than `max_age_secs` (the reason says why the
    /// app left each one waiting, or that it never examined it), and `pending_confirm`
    /// requests whose confirm started over [`DRIVE_CONFIRM_MAX_AGE_SECS`] ago (a swap
    /// can queue and run a while before its confirm opens, so creation age would cut
    /// the human's confirm window short). `running` requests are never expired by
    /// age. Returns the total.
    pub fn drive_requests_expire(&self, max_age_secs: i64) -> Result<usize> {
        let age = format!("-{max_age_secs} seconds");
        let confirm_age = format!("-{DRIVE_CONFIRM_MAX_AGE_SECS} seconds");
        let a = self.conn.execute(
            "UPDATE drive_requests \
             SET status = 'refused', resolved_at = datetime('now'), \
                 reason = 'expired after ' || ?2 || 's: ' || \
                   CASE WHEN wait_reason = '' THEN 'the app never examined it' \
                        ELSE wait_reason END \
             WHERE status = 'pending' AND created_at < datetime('now', ?1)",
            params![age, max_age_secs],
        )?;
        let b = self.conn.execute(
            "UPDATE drive_requests \
             SET status = 'refused', resolved_at = datetime('now'), \
                 reason = 'expired: the confirm wasn''t answered in time' \
             WHERE status = 'pending_confirm' \
               AND COALESCE(confirm_started_at, created_at) < datetime('now', ?1)",
            params![confirm_age],
        )?;
        Ok(a + b)
    }

    // ---- Session replies, polling, purge ----

    /// A gate got its decision and the agent resumes: mark `worktree` `busy`, unless
    /// another brokered prompt for it is still pending (parallel tool calls each run
    /// their own gate), in which case it stays `waiting` and nothing is withdrawn.
    /// Unlike [`LocalStore::session_upsert`] this never withdraws pending prompts.
    /// `true` when it wrote `busy`.
    pub fn session_resume_after_decision(
        &self,
        worktree: &str,
        current_task_id: Option<i64>,
    ) -> Result<bool> {
        let tx = self.drive_tx()?;
        let others: i64 = tx.query_row(
            &format!(
                "SELECT COUNT(*) FROM drive_prompts \
                 WHERE driven = ?1 AND status = 'pending' AND NOT ({})",
                stale_sql("")
            ),
            params![worktree],
            |r| r.get(0),
        )?;
        if others > 0 {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO worktree_sessions (worktree, state, current_task_id, pid, last_heartbeat) \
             VALUES (?1, 'busy', ?2, NULL, datetime('now')) \
             ON CONFLICT(worktree) DO UPDATE SET \
               state = 'busy', current_task_id = excluded.current_task_id, \
               pid = excluded.pid, last_heartbeat = excluded.last_heartbeat, \
               session_id = NULL",
            params![worktree, current_task_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Record `worktree`'s finished turn: bumps `turn` (the first is 1) and keeps the
    /// last message, truncated. Returns the new turn.
    pub fn session_reply_record(&self, worktree: &str, last_message: &str) -> Result<i64> {
        self.session_reply_record_for_turn(worktree, last_message, None, None)
    }

    /// [`Self::session_reply_record`] for the Stop of turn `turn_id` in agent session
    /// `session_id`. A repeat of the stored `turn_id` (a Stop that reached the CLI
    /// twice) does not bump: it only fills a stored empty message with a non-empty
    /// one, and returns the existing turn. With a `session_id` the stored session
    /// must match too, since a new session (after a reset or wake) numbers its turns
    /// afresh; without one the turn id alone decides. Any other call bumps and
    /// stores both ids (`None` clears them).
    pub fn session_reply_record_for_turn(
        &self,
        worktree: &str,
        last_message: &str,
        turn_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<i64> {
        let message = truncate_bytes(last_message, SESSION_REPLY_MAX);
        let tx = self.drive_tx()?;
        if let Some(id) = turn_id {
            let stored: Option<(i64, String)> = tx
                .query_row(
                    "SELECT turn, last_message FROM session_replies \
                     WHERE worktree = ?1 AND last_turn_id = ?2 \
                       AND (?3 IS NULL OR last_session_id IS ?3)",
                    params![worktree, id, session_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((turn, stored)) = stored {
                if stored.is_empty() && !message.is_empty() {
                    tx.execute(
                        "UPDATE session_replies SET last_message = ?2, updated_at = datetime('now') \
                         WHERE worktree = ?1",
                        params![worktree, message],
                    )?;
                }
                tx.commit()?;
                return Ok(turn);
            }
        }
        let turn = tx.query_row(
            "INSERT INTO session_replies \
               (worktree, turn, last_message, updated_at, last_turn_id, last_session_id) \
             VALUES (?1, 1, ?2, datetime('now'), ?3, ?4) \
             ON CONFLICT(worktree) DO UPDATE SET \
               turn = session_replies.turn + 1, last_message = excluded.last_message, \
               updated_at = excluded.updated_at, last_turn_id = excluded.last_turn_id, \
               last_session_id = excluded.last_session_id \
             RETURNING turn",
            params![worktree, message, turn_id, session_id],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(turn)
    }

    /// The last finished turn of every driven worktree that has one, by worktree.
    pub fn drive_session_replies(&self) -> Result<Vec<SessionReply>> {
        if !self.drive_tables_exist()? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT r.worktree, r.turn, r.last_message, r.updated_at FROM session_replies r \
             JOIN drives d ON d.driven = r.worktree ORDER BY r.worktree",
        )?;
        let rows = stmt
            .query_map([], row_to_session_reply)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `worktree`'s last finished turn, if any.
    pub fn session_reply_get(&self, worktree: &str) -> Result<Option<SessionReply>> {
        let row = self
            .conn
            .query_row(
                "SELECT worktree, turn, last_message, updated_at FROM session_replies \
                 WHERE worktree = ?1",
                params![worktree],
                row_to_session_reply,
            )
            .optional()?;
        Ok(row)
    }

    /// Seconds since `worktree`'s last session heartbeat; `None` without a session row.
    pub fn session_idle_secs(&self, worktree: &str) -> Result<Option<i64>> {
        let secs = self
            .conn
            .query_row(
                "SELECT CAST(strftime('%s', 'now') - strftime('%s', last_heartbeat) AS INTEGER) \
                 FROM worktree_sessions WHERE worktree = ?1",
                params![worktree],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?;
        Ok(secs.flatten())
    }

    /// `PRAGMA data_version`: changes when **another** connection commits, so a poll
    /// loop can skip its real query while nothing changed.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?)
    }

    /// Delete resolved requests and decided prompts older than `max_age_secs`.
    /// Bindings and turn replies are never purged. Returns the rows deleted.
    pub fn drive_purge(&self, max_age_secs: i64) -> Result<usize> {
        let age = format!("-{max_age_secs} seconds");
        let a = self.conn.execute(
            "DELETE FROM drive_requests \
             WHERE status IN ('applied', 'refused') AND resolved_at < datetime('now', ?1)",
            params![age],
        )?;
        let b = self.conn.execute(
            "DELETE FROM drive_prompts \
             WHERE status <> 'pending' AND decided_at < datetime('now', ?1)",
            params![age],
        )?;
        Ok(a + b)
    }
}

#[cfg(test)]
#[path = "drive_store_tests.rs"]
mod tests;
