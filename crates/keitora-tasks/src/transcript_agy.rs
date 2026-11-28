//! Antigravity (`agy`) replies from its per-conversation SQLite file
//! (`<agy state>/conversations/<conversation id>.db`).
//!
//! The file's `steps` table holds one row per step of the conversation, in `idx`
//! order, with protobuf blobs. Fields read (confirmed against a captured file):
//! - `step_type` `14` is the user's input, `15` a planner response (other values are
//!   tool steps); `status` `3` is done.
//! - `metadata` field 1 is the step's creation time (`{1: seconds, 2: nanos}`).
//! - a planner response's text is `step_payload` field 20, field 1 (field 8 holds
//!   the same text and is the fallback).
//!
//! The turn answering a delivery starts at the last user input created in its
//! [`TurnWindow`] (from the delivery less [`TURN_START_SKEW_MS`] to the detection plus
//! the skew); its reply is the text of the last planner response after it and before
//! the next user input that has text, provided that response is done.
//! Anything else (no such input, no text, a response still running, an unreadable
//! file) gives no reply.

use std::path::Path;

use crate::local_store::{open_ro_conn, open_ro_conn_immutable};

use super::{
    miss, truncate_reply, TurnMiss, TurnOutcome, TurnReply, TurnWindow, TURN_START_SKEW_MS,
};

/// `step_type` of the user's input.
pub const STEP_USER_INPUT: i64 = 14;
/// `step_type` of a planner response.
pub const STEP_PLANNER_RESPONSE: i64 = 15;
/// `status` of a finished step.
pub const STATUS_DONE: i64 = 3;
/// Most user-input and planner-response steps read from the end of a conversation.
pub const STEPS_MAX: usize = 500;

/// One step, as far as reply selection needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgyStep {
    pub step_type: i64,
    pub status: i64,
    /// Unix ms the step was created; `None` when the metadata has no time.
    pub created_at_ms: Option<i64>,
    /// A planner response's text.
    pub text: Option<String>,
}

/// A protobuf field's value: a varint or a length-delimited payload (fixed-width
/// values are skipped).
enum Wire<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut out = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*i)?;
        *i += 1;
        out |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(out);
        }
    }
    None
}

/// The fields of a protobuf message, in order; `None` when it doesn't parse.
fn fields(b: &[u8]) -> Option<Vec<(u64, Wire<'_>)>> {
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let key = varint(b, &mut i)?;
        let value = match key & 7 {
            0 => Wire::Varint(varint(b, &mut i)?),
            1 => {
                i = i.checked_add(8).filter(|&e| e <= b.len())?;
                Wire::Fixed
            }
            2 => {
                let n = usize::try_from(varint(b, &mut i)?).ok()?;
                let end = i.checked_add(n).filter(|&e| e <= b.len())?;
                let v = &b[i..end];
                i = end;
                Wire::Bytes(v)
            }
            5 => {
                i = i.checked_add(4).filter(|&e| e <= b.len())?;
                Wire::Fixed
            }
            _ => return None,
        };
        out.push((key >> 3, value));
    }
    Some(out)
}

/// The last length-delimited field `n` of a parsed message.
fn bytes_field<'a>(fields: &[(u64, Wire<'a>)], n: u64) -> Option<&'a [u8]> {
    fields.iter().rev().find_map(|(f, v)| match v {
        Wire::Bytes(x) if *f == n => Some(*x),
        _ => None,
    })
}

/// The last varint field `n` of a parsed message.
fn varint_field(fields: &[(u64, Wire<'_>)], n: u64) -> Option<u64> {
    fields.iter().rev().find_map(|(f, v)| match v {
        Wire::Varint(x) if *f == n => Some(*x),
        _ => None,
    })
}

/// A step's creation time from its `metadata` blob.
fn created_at_ms(metadata: &[u8]) -> Option<i64> {
    let ts = fields(bytes_field(&fields(metadata)?, 1)?)?;
    let secs = i64::try_from(varint_field(&ts, 1)?).ok()?;
    let nanos = i64::try_from(varint_field(&ts, 2).unwrap_or(0)).ok()?;
    secs.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}

/// A planner response's text from its `step_payload` blob.
fn response_text(payload: &[u8]) -> Option<String> {
    let response = fields(bytes_field(&fields(payload)?, 20)?)?;
    [1, 8].into_iter().find_map(|n| {
        let text = std::str::from_utf8(bytes_field(&response, n)?).ok()?;
        (!text.trim().is_empty()).then(|| text.to_string())
    })
}

/// One `steps` row decoded. Blobs that don't parse leave the time or text unset.
pub fn agy_step(step_type: i64, status: i64, metadata: &[u8], payload: &[u8]) -> AgyStep {
    AgyStep {
        step_type,
        status,
        created_at_ms: created_at_ms(metadata),
        text: (step_type == STEP_PLANNER_RESPONSE)
            .then(|| response_text(payload))
            .flatten(),
    }
}

/// The reply of the newest turn started in `window`, from a conversation's steps in
/// `idx` order. See the module docs.
pub fn agy_reply_for_turn(steps: &[AgyStep], window: TurnWindow) -> Result<TurnReply, TurnMiss> {
    let input = |s: &AgyStep| s.step_type == STEP_USER_INPUT;
    let start = steps
        .iter()
        .rposition(|s| {
            input(s)
                && s.created_at_ms
                    .is_some_and(|t| window.admits(t, TURN_START_SKEW_MS))
        })
        .ok_or(TurnMiss::NoStart)?;
    // The turn ends where the next input (a later turn) starts.
    let end = steps[start + 1..]
        .iter()
        .position(input)
        .map_or(steps.len(), |i| start + 1 + i);
    let last = steps[start..end]
        .iter()
        .rev()
        .find(|s| s.step_type == STEP_PLANNER_RESPONSE && s.text.is_some())
        .ok_or(TurnMiss::NoReply)?;
    if last.status != STATUS_DONE {
        return Err(TurnMiss::NoReply);
    }
    Ok(TurnReply {
        text: truncate_reply(last.text.as_deref().ok_or(TurnMiss::NoReply)?),
        outcome: TurnOutcome::Complete,
    })
}

/// How a conversation file is opened. Every mode is read-only: nothing is ever
/// written to the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenMode {
    /// SQLite's normal read-only open, with shared locks: a writer's transaction is
    /// waited out (or seen through its `-wal`), never read half-done. Used for a
    /// rollback-journal file (nothing is created next to it) and for a WAL file whose
    /// `-wal` exists (agy has it open; its `-wal`/`-shm` are reused). Only if agy
    /// closes the file between that check and the open can SQLite create the two.
    Locked,
    /// `immutable=1`: no locks, and no `-wal`/`-shm` created next to the file. Used
    /// only for a WAL file with no `-wal` (no one has it open), where a locked
    /// read-only open would create and leave both files behind. A writer that opens
    /// the file meanwhile is caught by the change check after the read.
    Immutable,
}

/// `path` with `suffix` appended (`<id>.db-wal`).
fn sibling(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    s.into()
}

/// Whether the file's header marks it as write-ahead-logged (format versions at
/// bytes 18 and 19 are 2). A header that can't be read counts as not.
fn header_says_wal(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 20];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| head[18] == 2 && head[19] == 2)
}

fn open_mode(path: &Path) -> OpenMode {
    if !sibling(path, "-wal").is_file() && header_says_wal(path) {
        OpenMode::Immutable
    } else {
        OpenMode::Locked
    }
}

/// What changes when a writer touches the file: its length and mtime, and whether a
/// `-wal` exists.
fn stamp(path: &Path) -> Option<(u64, std::time::SystemTime, bool)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()?, sibling(path, "-wal").is_file()))
}

/// An error a second read may not hit: a writer's lock, or pages caught mid-write.
fn worth_a_retry(e: &rusqlite::Error) -> bool {
    use rusqlite::ErrorCode::{DatabaseBusy, DatabaseCorrupt, DatabaseLocked};
    matches!(
        e.sqlite_error_code(),
        Some(DatabaseBusy | DatabaseLocked | DatabaseCorrupt)
    )
}

fn query_steps(path: &Path, mode: OpenMode) -> rusqlite::Result<Vec<AgyStep>> {
    let conn = match mode {
        OpenMode::Locked => open_ro_conn(path)?,
        OpenMode::Immutable => open_ro_conn_immutable(path)?,
    };
    let mut stmt = conn.prepare(
        "SELECT step_type, status, metadata, step_payload FROM steps \
             WHERE step_type IN (?1, ?2) ORDER BY idx DESC LIMIT ?3",
    )?;
    let params = [STEP_USER_INPUT, STEP_PLANNER_RESPONSE, STEPS_MAX as i64];
    let rows = stmt.query_map(params, |r| {
        let metadata: Option<Vec<u8>> = r.get(2)?;
        let payload: Option<Vec<u8>> = r.get(3)?;
        Ok(agy_step(
            r.get(0)?,
            r.get(1)?,
            metadata.as_deref().unwrap_or_default(),
            payload.as_deref().unwrap_or_default(),
        ))
    })?;
    let mut steps = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    steps.reverse();
    Ok(steps)
}

/// The last [`STEPS_MAX`] user-input and planner-response steps of the conversation
/// file at `path`, in `idx` order (tool steps and their payloads are never loaded).
/// The file is only ever read (see [`OpenMode`]); a read that met a busy or corrupt
/// file, or an immutable read the file changed under, is made once more. Errors are
/// [`miss`] reasons.
pub fn read_agy_steps(path: &Path) -> Result<Vec<AgyStep>, &'static str> {
    read_agy_steps_checked(path, || {})
}

/// [`read_agy_steps`], calling `after_read` between an immutable read and its change
/// check (tests stand in for a writer there).
fn read_agy_steps_checked(
    path: &Path,
    after_read: impl Fn(),
) -> Result<Vec<AgyStep>, &'static str> {
    if !path.is_file() {
        return Err(miss::NOT_FOUND);
    }
    let unreadable = |why: &dyn std::fmt::Display| {
        log::debug!(
            "transcript: agy steps unreadable in {}: {why}",
            path.display()
        );
        miss::PARSE_FAILED
    };
    for last in [false, true] {
        let mode = open_mode(path);
        let before = (mode == OpenMode::Immutable).then(|| stamp(path));
        match query_steps(path, mode) {
            Ok(steps) => {
                after_read();
                if before.is_none_or(|b| b.is_some() && b == stamp(path)) {
                    return Ok(steps);
                }
                if last {
                    return Err(unreadable(&"the file changed during the read"));
                }
            }
            Err(e) if !last && worth_a_retry(&e) => {}
            Err(e) => return Err(unreadable(&e)),
        }
    }
    Err(miss::PARSE_FAILED)
}

/// The reply of the newest turn started in `window`, read from agy's conversation
/// file at `path`. Errors are [`miss`] reasons.
pub fn read_agy_turn_reply(path: &Path, window: TurnWindow) -> Result<TurnReply, &'static str> {
    let steps = read_agy_steps(path)?;
    agy_reply_for_turn(&steps, window).map_err(|_| miss::NOT_THIS_TURN)
}

/// The `steps` table as agy creates it (test fixtures build files with it).
pub const STEPS_DDL: &str = "CREATE TABLE `steps` (`idx` integer,`step_type` integer NOT NULL \
    DEFAULT 0,`status` integer NOT NULL DEFAULT 0,`has_subtrajectory` numeric NOT NULL DEFAULT \
    false,`metadata` blob,`error_details` blob,`permissions` blob,`task_details` blob,\
    `render_info` blob,`step_payload` blob,`step_format` integer NOT NULL DEFAULT 0,\
    PRIMARY KEY (`idx`))";

/// Builders for files shaped like agy's, shared by this crate's and the app's tests
/// (not used by the reader).
#[doc(hidden)]
pub mod fixture {
    use std::path::Path;

    use super::STEPS_DDL;

    fn varint(mut n: u64, out: &mut Vec<u8>) {
        while n >= 0x80 {
            out.push((n as u8) | 0x80);
            n >>= 7;
        }
        out.push(n as u8);
    }

    /// A protobuf varint field.
    pub fn pb_varint(field: u64, n: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(field << 3, &mut out);
        varint(n, &mut out);
        out
    }

    /// A protobuf length-delimited field.
    pub fn pb_bytes(field: u64, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint((field << 3) | 2, &mut out);
        varint(body.len() as u64, &mut out);
        out.extend_from_slice(body);
        out
    }

    /// A step's `metadata` blob created at unix ms `ms`.
    pub fn metadata(ms: i64) -> Vec<u8> {
        let ts = [
            pb_varint(1, (ms / 1000) as u64),
            pb_varint(2, (ms % 1000 * 1_000_000) as u64),
        ]
        .concat();
        pb_bytes(1, &ts)
    }

    /// A done planner response's `step_payload` with `text` in response field
    /// `field` (1, or the fallback 8).
    pub fn response(text: &[u8], field: u64) -> Vec<u8> {
        [
            pb_varint(1, 15),
            pb_varint(4, 3),
            pb_bytes(20, &pb_bytes(field, text)),
        ]
        .concat()
    }

    /// One `steps` row: `idx`, `step_type`, `status`, `metadata`, `step_payload`.
    pub type Row = (i64, i64, i64, Vec<u8>, Vec<u8>);

    /// A conversation file at `path` holding `rows`.
    pub fn write_conversation(path: &Path, rows: &[Row]) -> rusqlite::Result<()> {
        let conn = rusqlite::Connection::open(path)?;
        conn.execute_batch(STEPS_DDL)?;
        for (idx, kind, status, metadata, payload) in rows {
            conn.execute(
                "INSERT INTO steps (idx, step_type, status, metadata, step_payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![idx, kind, status, metadata, payload],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{metadata, response, write_conversation};
    use super::*;

    #[test]
    fn decodes_time_and_text_from_either_field() {
        let s = agy_step(15, 3, &metadata(1_500), &response(b"hi", 1));
        assert_eq!(s.created_at_ms, Some(1_500));
        assert_eq!(s.text.as_deref(), Some("hi"));
        let s = agy_step(15, 3, &metadata(1_500), &response(b"via eight", 8));
        assert_eq!(s.text.as_deref(), Some("via eight"));
        // Text is only read from planner responses.
        assert_eq!(agy_step(14, 3, &[], &response(b"hi", 1)).text, None);
    }

    #[test]
    fn garbage_blobs_never_panic() {
        for blob in [
            &b""[..],
            &[0xff, 0xff, 0xff][..],
            &[0x0a, 0x7f, 0x01][..], // length past the end
            &[0x0f][..],             // wire type 7
            &[0x09, 1, 2][..],       // short fixed64
            &[0xa2, 0x01, 0x02, 0x0a, 0x05][..],
            &[0x80; 12][..], // unterminated varint
        ] {
            let s = agy_step(15, 3, blob, blob);
            assert_eq!((s.created_at_ms, s.text), (None, None), "{blob:?}");
        }
        // Text that isn't UTF-8 is no text.
        assert_eq!(agy_step(15, 3, &[], &response(&[0xff, 0xfe], 1)).text, None);
    }

    fn step(kind: i64, status: i64, ms: i64, text: Option<&str>) -> AgyStep {
        AgyStep {
            step_type: kind,
            status,
            created_at_ms: Some(ms),
            text: text.map(str::to_string),
        }
    }

    #[test]
    fn a_running_last_response_or_no_text_is_no_reply() {
        let steps = vec![
            step(14, 3, 10_000, None),
            step(15, 3, 11_000, Some("first part")),
            step(132, 3, 12_000, None),
            step(15, 1, 13_000, Some("still writ")),
        ];
        let since = TurnWindow::since;
        assert_eq!(
            agy_reply_for_turn(&steps, since(10_000)),
            Err(TurnMiss::NoReply)
        );
        assert_eq!(
            agy_reply_for_turn(&steps[..1], since(10_000)),
            Err(TurnMiss::NoReply),
            "an input with no response yet"
        );
        let mut untimed = steps.clone();
        untimed[0].created_at_ms = None;
        assert_eq!(
            agy_reply_for_turn(&untimed, since(0)),
            Err(TurnMiss::NoStart)
        );
    }

    #[test]
    fn an_input_after_detection_never_takes_the_reply() {
        let steps = vec![
            step(14, 3, 10_000, None),
            step(15, 3, 11_000, Some("first reply")),
            step(14, 3, 20_000, None),
            step(15, 1, 21_000, Some("second, half")),
        ];
        let window = TurnWindow::new(10_000, 12_000);
        assert_eq!(
            agy_reply_for_turn(&steps, window).map(|r| r.text),
            Ok("first reply".to_string())
        );
        // The later input with no response yet.
        assert_eq!(
            agy_reply_for_turn(&steps[..3], window).map(|r| r.text),
            Ok("first reply".to_string())
        );
    }

    /// A conversation file holding `rows` (`step_type`, `status`, metadata, payload)
    /// at consecutive `idx`.
    fn conversation(dir: &Path, rows: &[(i64, i64, Vec<u8>, Vec<u8>)]) -> std::path::PathBuf {
        let path = dir.join("c-1.db");
        let rows: Vec<_> = (0..)
            .zip(rows.iter().cloned())
            .map(|(idx, (kind, status, md, pl))| (idx, kind, status, md, pl))
            .collect();
        write_conversation(&path, &rows).unwrap();
        path
    }

    #[test]
    fn tool_steps_are_never_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = conversation(
            dir.path(),
            &[
                (14, 3, metadata(10_000), Vec::new()),
                (132, 3, metadata(10_500), response(b"tool output", 1)),
                (15, 3, metadata(11_000), response(b"done", 1)),
            ],
        );
        let kinds: Vec<_> = read_agy_steps(&path)
            .unwrap()
            .iter()
            .map(|s| s.step_type)
            .collect();
        assert_eq!(kinds, vec![STEP_USER_INPUT, STEP_PLANNER_RESPONSE]);
    }

    #[test]
    fn reads_a_conversation_file_without_leaving_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = conversation(
            dir.path(),
            &[
                (14, 3, metadata(10_000), Vec::new()),
                (15, 3, metadata(11_000), response(b"done", 1)),
            ],
        );
        let before: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            read_agy_turn_reply(&path, TurnWindow::since(10_000)).map(|r| r.text),
            Ok("done".to_string())
        );
        let after: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(before, after, "no -wal/-shm created");
        assert_eq!(
            read_agy_turn_reply(&path, TurnWindow::since(60_000)),
            Err(miss::NOT_THIS_TURN)
        );
        assert_eq!(
            read_agy_turn_reply(&dir.path().join("none.db"), TurnWindow::since(0)),
            Err(miss::NOT_FOUND)
        );
        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, b"title: x").unwrap();
        assert_eq!(
            read_agy_turn_reply(&junk, TurnWindow::since(0)),
            Err(miss::PARSE_FAILED)
        );
    }

    /// The dir's file names, sorted.
    fn listing(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        names.sort();
        names
    }

    /// A conversation file switched to WAL mode and closed again (agy's file while
    /// agy isn't running): no `-wal` next to it.
    fn closed_wal_conversation(dir: &Path) -> std::path::PathBuf {
        let path = conversation(dir, &[(14, 3, metadata(10_000), Vec::new())]);
        let c = rusqlite::Connection::open(&path).unwrap();
        c.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))
            .unwrap();
        drop(c);
        assert!(!sibling(&path, "-wal").exists());
        path
    }

    #[test]
    fn the_open_mode_follows_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let rollback = conversation(dir.path(), &[(14, 3, metadata(10_000), Vec::new())]);
        assert_eq!(open_mode(&rollback), OpenMode::Locked);
        std::fs::remove_file(&rollback).unwrap();
        let wal = closed_wal_conversation(dir.path());
        assert_eq!(open_mode(&wal), OpenMode::Immutable);
        let writer = rusqlite::Connection::open(&wal).unwrap();
        writer.execute("UPDATE steps SET status = 3", []).unwrap();
        assert_eq!(open_mode(&wal), OpenMode::Locked, "agy holds it");
        drop(writer);
        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, b"title: x").unwrap();
        assert_eq!(open_mode(&junk), OpenMode::Locked);
    }

    #[test]
    fn a_rollback_journal_file_is_read_under_its_locks() {
        let dir = tempfile::tempdir().unwrap();
        let path = conversation(dir.path(), &[(14, 3, metadata(10_000), Vec::new())]);
        let before = listing(dir.path());
        // agy mid-transaction: the read waits it out instead of reading under it.
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        writer
            .execute(
                "INSERT INTO steps (idx, step_type, status, metadata, step_payload) \
                 VALUES (1, 15, 3, ?1, ?2)",
                rusqlite::params![metadata(11_000), response(b"committed", 1)],
            )
            .unwrap();
        assert_eq!(read_agy_steps(&path), Err(miss::PARSE_FAILED));
        writer.execute_batch("COMMIT").unwrap();
        drop(writer);
        assert_eq!(
            read_agy_turn_reply(&path, TurnWindow::since(10_000)).map(|r| r.text),
            Ok("committed".to_string())
        );
        assert_eq!(before, listing(dir.path()), "no files created");
    }

    #[test]
    fn a_closed_wal_file_is_read_without_leaving_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = closed_wal_conversation(dir.path());
        let before = listing(dir.path());
        assert_eq!(read_agy_steps(&path).map(|s| s.len()), Ok(1));
        assert_eq!(before, listing(dir.path()), "no -wal/-shm created");
    }

    #[test]
    fn a_writer_opening_the_file_during_an_immutable_read_forces_a_locked_reread() {
        let dir = tempfile::tempdir().unwrap();
        let path = closed_wal_conversation(dir.path());
        let writer = std::cell::RefCell::new(None);
        // agy opens the file and writes its response while the first read runs.
        let steps = read_agy_steps_checked(&path, || {
            if writer.borrow().is_some() {
                return;
            }
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute("PRAGMA wal_autocheckpoint=0", []).ok();
            c.execute(
                "INSERT INTO steps (idx, step_type, status, metadata, step_payload) \
                 VALUES (1, 15, 3, ?1, ?2)",
                rusqlite::params![metadata(11_000), response(b"late", 1)],
            )
            .unwrap();
            *writer.borrow_mut() = Some(c);
        })
        .unwrap();
        assert_eq!(steps.len(), 2, "re-read through the writer's log");
        assert_eq!(steps[1].text.as_deref(), Some("late"));
    }

    #[test]
    fn busy_locked_and_corrupt_are_retried_once() {
        use rusqlite::ffi;
        let err = |code| rusqlite::Error::SqliteFailure(ffi::Error::new(code), None);
        assert!(worth_a_retry(&err(ffi::SQLITE_BUSY)));
        assert!(worth_a_retry(&err(ffi::SQLITE_LOCKED)));
        assert!(worth_a_retry(&err(ffi::SQLITE_CORRUPT)));
        assert!(!worth_a_retry(&err(ffi::SQLITE_NOTADB)));
        assert!(!worth_a_retry(&rusqlite::Error::QueryReturnedNoRows));
    }

    #[test]
    fn reads_through_the_write_ahead_log_while_agy_holds_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = conversation(dir.path(), &[(14, 3, metadata(10_000), Vec::new())]);
        // agy's own connection, in WAL mode, with a response not yet checkpointed.
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer
            .query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))
            .unwrap();
        writer.execute("PRAGMA wal_autocheckpoint=0", []).ok();
        writer
            .execute(
                "INSERT INTO steps (idx, step_type, status, metadata, step_payload) \
                 VALUES (1, 15, 3, ?1, ?2)",
                rusqlite::params![metadata(11_000), response(b"from the log", 1)],
            )
            .unwrap();
        assert!(path.with_extension("db-wal").is_file());
        assert_eq!(
            read_agy_turn_reply(&path, TurnWindow::since(10_000)).map(|r| r.text),
            Ok("from the log".to_string())
        );
        // A read never blocks agy's next write.
        writer
            .execute("UPDATE steps SET status = 3 WHERE idx = 0", [])
            .unwrap();
    }
}
