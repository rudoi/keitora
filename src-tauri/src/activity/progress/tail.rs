//! `tail.rs` — a shared-read incremental file tailer for the UBT log.
//!
//! The installed-engine UBT `Log.txt` is shared across worktrees (one per machine) and is
//! kept open + rotated by UBT while a build runs. The tailer therefore:
//!
//! - opens with **shared** read so UBT can keep writing/rotating (`FILE_SHARE_READ |
//!   FILE_SHARE_WRITE | FILE_SHARE_DELETE` on Windows via [`OpenOptionsExt`]);
//! - keeps a byte `offset` and reads only `[offset, len)` each tick, buffering a partial
//!   trailing line until its newline arrives;
//! - resets `offset`/partial when the file shrinks (per-run truncation or rotation to
//!   `Log-backup-*`), signaling the caller to reset adapter state (a new run);
//! - also resets when the file's head no longer matches the one remembered (a rotated log
//!   that has already grown past the old offset), with a changed creation time as a weaker
//!   second signal;
//! - on first attach to an existing log starts near the end (`len - 256 KiB`) so a long
//!   log does not replay, but re-reads from `0` once if the current run's
//!   `Resolved project argument` line is not in that window;
//! - reads at most 1 MiB per tick.
//!
//! Attribution (mapping the run's `Resolved project argument` / `-Project=` to a worktree)
//! lives in the engine, which owns the worktree→path mapping; the tailer only delivers lines.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

/// Bytes read per tick, cap (1 MiB).
const MAX_READ_PER_TICK: u64 = 1024 * 1024;
/// First-attach look-back window (256 KiB).
const FIRST_ATTACH_WINDOW: u64 = 256 * 1024;
/// Longest file head remembered as the log's identity (256 bytes).
const HEAD_FINGERPRINT_MAX: usize = 256;
/// Marker whose presence in the first-attach window means the current run is in view.
const RESOLVED_MARKER: &str = "Resolved project argument";

/// Mutable cursor for the tailed log file. The engine holds a single cursor (the UBT log is
/// one shared file).
#[derive(Debug, Default)]
pub struct TailCursor {
    /// Next file byte to read.
    pub offset: u64,
    /// Buffered trailing bytes past the last newline (already counted in `offset`).
    pub partial: String,
    /// `true` until the first successful read positions `offset`.
    pub first_attach: bool,
    /// Fingerprint of the file head: the bytes before the first newline (at most
    /// [`HEAD_FINGERPRINT_MAX`]). A different head means a different log. Empty until the
    /// file has content.
    pub head: Vec<u8>,
    /// Creation time of the file last read. A weak signal: it can carry over to a re-created
    /// file, and it is `None` where unsupported.
    pub created: Option<SystemTime>,
}

impl TailCursor {
    /// A fresh cursor that will position itself near the end on its first read.
    pub fn new() -> TailCursor {
        TailCursor {
            offset: 0,
            partial: String::new(),
            first_attach: true,
            head: Vec::new(),
            created: None,
        }
    }
}

/// Result of one tail tick.
#[derive(Debug, Default, PartialEq)]
pub struct TailRead {
    /// Complete lines (trailing `\r` stripped) read this tick.
    pub lines: Vec<String>,
    /// `true` when the file shrank (truncation/rotation) → caller resets adapter state.
    pub reset: bool,
}

/// Open `path` for reading, sharing with UBT's writer/rotator on Windows.
pub fn open_shared(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ(1) | FILE_SHARE_WRITE(2) | FILE_SHARE_DELETE(4) = 0x7.
        OpenOptions::new().read(true).share_mode(0x7).open(path)
    }
    #[cfg(not(windows))]
    {
        OpenOptions::new().read(true).open(path)
    }
}

/// Read newly-appended complete lines from `path`, advancing `cursor`.
///
/// Returns the complete lines read this tick plus a `reset` flag when the file shrank
/// (truncation/rotation). A missing or unopenable file is an `Err` (from `open_shared`); the
/// engine treats that as "no build log yet" via `unwrap_or_default()`.
pub fn read_new_lines(path: &Path, cursor: &mut TailCursor) -> io::Result<TailRead> {
    let mut file = open_shared(path)?;
    let meta = file.metadata()?;
    let len = meta.len();
    let created = meta.created().ok();

    let mut out = TailRead::default();

    // The head is read first (from offset 0) so first-attach positioning can start mid-file.
    let raw_head = read_head(&mut file, len)?;
    let head = head_line(&raw_head);

    if cursor.first_attach {
        position_first_attach(&mut file, cursor, len)?;
        cursor.head = head;
        cursor.created = created;
    } else {
        // A shorter file was truncated or rotated; otherwise compare only the prefix both
        // the remembered and the current head cover, so a growing log does not false-reset.
        let common = cursor.head.len().min(raw_head.len());
        let rotated = len < cursor.offset
            || cursor.head[..common] != raw_head[..common]
            || matches!((cursor.created, created), (Some(a), Some(b)) if a != b);
        if rotated {
            cursor.offset = 0;
            cursor.partial.clear();
            cursor.head = head;
            out.reset = true;
        } else if head.len() > cursor.head.len() {
            // The first line was still being written and has grown (or the file was empty).
            cursor.head = head;
        }
        if created.is_some() {
            cursor.created = created;
        }
    }

    if len <= cursor.offset {
        return Ok(out);
    }

    let to_read = (len - cursor.offset).min(MAX_READ_PER_TICK);
    file.seek(SeekFrom::Start(cursor.offset))?;
    let mut buf = vec![0u8; to_read as usize];
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    cursor.offset += n as u64;

    split_into_lines(&buf, cursor, &mut out.lines);
    Ok(out)
}

/// Read up to [`HEAD_FINGERPRINT_MAX`] bytes from the start of the file.
fn read_head(file: &mut File, len: u64) -> io::Result<Vec<u8>> {
    let want = len.min(HEAD_FINGERPRINT_MAX as u64) as usize;
    let mut buf = vec![0u8; want];
    file.seek(SeekFrom::Start(0))?;
    let mut filled = 0;
    while filled < want {
        let n = file.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}

/// The fingerprint of a file head: the bytes before the first newline.
fn head_line(raw: &[u8]) -> Vec<u8> {
    let end = raw.iter().position(|&b| b == b'\n').unwrap_or(raw.len());
    raw[..end].to_vec()
}

/// Position the cursor for the first read: near the end, but from `0` if the current
/// run's `Resolved project argument` line is not inside the look-back window.
fn position_first_attach(file: &mut File, cursor: &mut TailCursor, len: u64) -> io::Result<()> {
    cursor.first_attach = false;
    cursor.partial.clear();

    if len <= FIRST_ATTACH_WINDOW {
        cursor.offset = 0;
        return Ok(());
    }

    let start = len - FIRST_ATTACH_WINDOW;
    file.seek(SeekFrom::Start(start))?;
    let mut window = vec![0u8; FIRST_ATTACH_WINDOW as usize];
    let n = file.read(&mut window)?;
    window.truncate(n);
    let text = String::from_utf8_lossy(&window);

    if text.contains(RESOLVED_MARKER) {
        // Start just past the first newline so we don't emit a half line.
        match window.iter().position(|&b| b == b'\n') {
            Some(nl) => cursor.offset = start + nl as u64 + 1,
            None => cursor.offset = start,
        }
    } else {
        // The run's resolve line is older than the window — read from the top once.
        cursor.offset = 0;
    }
    Ok(())
}

/// Append `chunk` to the partial buffer and extract complete lines (trailing `\r`
/// stripped). The unterminated remainder stays in `cursor.partial`.
fn split_into_lines(chunk: &[u8], cursor: &mut TailCursor, lines: &mut Vec<String>) {
    let text = String::from_utf8_lossy(chunk);
    cursor.partial.push_str(&text);

    // Clippy 1.98 reports `while_let_loop` on this loop; the loop stays as written because
    // `while let Some(nl) = cursor.partial.find('\n')` runs the same iterations.
    #[allow(clippy::while_let_loop)]
    loop {
        let Some(nl) = cursor.partial.find('\n') else {
            break;
        };
        let line: String = cursor.partial.drain(..=nl).collect();
        let trimmed = line.trim_end_matches(['\n', '\r']);
        lines.push(trimmed.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, bytes: &[u8]) {
        let mut f = File::create(path).unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut f = OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
    }

    #[test]
    fn incremental_append_reads_only_new_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        write(&path, b"line one\nline two\n");
        let mut cur = TailCursor::new(); // small file → starts at 0
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert_eq!(r.lines, vec!["line one", "line two"]);
        assert!(!r.reset);

        append(&path, b"line three\n");
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert_eq!(r.lines, vec!["line three"], "only the new line");
    }

    #[test]
    fn partial_last_line_is_buffered_until_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        write(&path, b"complete\npartial");
        let mut cur = TailCursor::new();
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert_eq!(r.lines, vec!["complete"], "partial withheld");

        append(&path, b" rest\n");
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert_eq!(r.lines, vec!["partial rest"], "partial completed");
    }

    #[test]
    fn truncate_to_shorter_resets_offset_and_signals_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        write(&path, b"old run line 1\nold run line 2\n");
        let mut cur = TailCursor::new();
        let _ = read_new_lines(&path, &mut cur).unwrap();
        assert!(cur.offset > 0);

        // New run truncates the file to something shorter.
        write(&path, b"new\n");
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(r.reset, "shrink signals reset");
        assert_eq!(r.lines, vec!["new"]);
    }

    #[test]
    fn rotation_rename_and_recreate_is_handled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        write(&path, b"aaaaaaaaaaaaaaaaaaaa\nbbbbbbbbbbbbbbbbbbbb\n");
        let mut cur = TailCursor::new();
        let _ = read_new_lines(&path, &mut cur).unwrap();

        // Rotate: move the big log aside, recreate a fresh (shorter) one.
        std::fs::rename(&path, dir.path().join("Log-backup-1.txt")).unwrap();
        write(&path, b"fresh run\n");
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(r.reset);
        assert_eq!(r.lines, vec!["fresh run"]);
    }

    /// Replace the log with a different file at the same path, as a rotation does.
    fn rotate(dir: &Path, path: &Path, bytes: &[u8]) {
        std::fs::rename(path, dir.join("Log-backup.txt")).unwrap();
        write(path, bytes);
    }

    #[test]
    fn rotation_to_a_longer_file_resets_via_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        write(&path, b"Log started at 10:00:00\nold 1\n");
        let mut cur = TailCursor::new();
        let _ = read_new_lines(&path, &mut cur).unwrap();
        // Only the head may tell the logs apart here, not the creation time.
        cur.created = None;

        let new = b"Log started at 10:05:00\nnew line number one\nnew line number two\nnew 3\n";
        assert!(new.len() as u64 > cur.offset);
        rotate(dir.path(), &path, new);
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(r.reset);
        assert_eq!(r.lines[0], "Log started at 10:05:00");
        assert_eq!(r.lines.len(), 4);
    }

    #[test]
    fn a_growing_file_does_not_false_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        write(&path, b"Log started at 10:00:00\none\n");
        let mut cur = TailCursor::new();
        let _ = read_new_lines(&path, &mut cur).unwrap();
        for i in 0..3 {
            append(&path, format!("more {i}\n").as_bytes());
            let r = read_new_lines(&path, &mut cur).unwrap();
            assert!(!r.reset);
            assert_eq!(r.lines, vec![format!("more {i}")]);
        }
    }

    #[test]
    fn a_creation_time_change_resets_and_an_unknown_one_never_does() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        write(&path, b"Log started at 10:00:00\none\n");
        let mut cur = TailCursor::new();
        let _ = read_new_lines(&path, &mut cur).unwrap();

        // Same head, longer file, but the remembered creation time differs.
        append(&path, b"two\n");
        cur.created = Some(SystemTime::UNIX_EPOCH);
        let r = read_new_lines(&path, &mut cur).unwrap();
        if std::fs::metadata(&path).unwrap().created().is_ok() {
            assert!(r.reset, "a changed creation time resets");
            assert_eq!(r.lines.len(), 3, "re-read from the top");
        }

        // A creation time that is unknown on either side never fires.
        append(&path, b"three\n");
        cur.created = None;
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(!r.reset);
        assert_eq!(r.lines, vec!["three"]);
    }

    #[test]
    fn a_partial_first_line_that_completes_does_not_false_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        write(&path, b"Log star");
        let mut cur = TailCursor::new();
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(r.lines.is_empty());
        assert_eq!(cur.head, b"Log star");

        append(&path, b"ted at 10:00:00\nbody\n");
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(!r.reset);
        assert_eq!(r.lines, vec!["Log started at 10:00:00", "body"]);
        assert_eq!(cur.head, b"Log started at 10:00:00");
    }

    #[test]
    fn an_empty_file_then_a_first_write_captures_the_fingerprint_without_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        write(&path, b"");
        let mut cur = TailCursor::new();
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(!r.reset);
        assert!(cur.head.is_empty());

        append(&path, b"Log started at 10:00:00\nbody\n");
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(!r.reset);
        assert_eq!(r.lines, vec!["Log started at 10:00:00", "body"]);
        assert_eq!(cur.head, b"Log started at 10:00:00");
    }

    #[test]
    fn first_attach_captures_the_head_even_when_starting_mid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        let mut data = b"Log started at 10:00:00\n".to_vec();
        data.extend(vec![b'x'; (FIRST_ATTACH_WINDOW as usize) + 10_000]);
        data.push(b'\n');
        data.extend_from_slice(b"Resolved project argument a to b\n");
        write(&path, &data);
        let mut cur = TailCursor::new();
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(!r.reset);
        assert!(cur.offset > 0, "started near the end");
        assert_eq!(cur.head, b"Log started at 10:00:00");
        // Growing afterwards does not reset.
        append(&path, b"next\n");
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(!r.reset);
        assert_eq!(r.lines, vec!["next"]);
    }

    #[test]
    fn rotation_plus_shrink_yields_a_single_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        write(
            &path,
            b"Log started at 10:00:00\nold line one\nold line two\n",
        );
        let mut cur = TailCursor::new();
        let _ = read_new_lines(&path, &mut cur).unwrap();

        rotate(dir.path(), &path, b"Log started at 10:05:00\n");
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(r.reset);
        assert_eq!(r.lines, vec!["Log started at 10:05:00"]);

        // The next read sees the new log as the known one.
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(!r.reset);
        assert!(r.lines.is_empty());
    }

    #[test]
    fn first_attach_starts_near_end_when_resolved_in_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        // > 256 KiB of filler, then the current run near the end.
        let mut data = vec![b'x'; (FIRST_ATTACH_WINDOW as usize) + 10_000];
        data.push(b'\n');
        data.extend_from_slice(
            b"Resolved project argument D:\\wt\\X.uproject to D:\\wt\\X.uproject\n",
        );
        data.extend_from_slice(b"[1/5] Compile a\n");
        write(&path, &data);

        let mut cur = TailCursor::new();
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(
            r.lines
                .iter()
                .any(|l| l.contains("Resolved project argument")),
            "the run's resolve line is in view"
        );
        assert!(r.lines.iter().any(|l| l == "[1/5] Compile a"));
    }

    #[test]
    fn first_attach_reads_from_zero_when_resolved_older_than_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        let mut data =
            b"Resolved project argument D:\\wt\\X.uproject to D:\\wt\\X.uproject\n".to_vec();
        data.extend(vec![b'y'; (FIRST_ATTACH_WINDOW as usize) + 50_000]);
        data.push(b'\n');
        write(&path, &data);

        let mut cur = TailCursor::new();
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert!(
            r.lines
                .iter()
                .any(|l| l.contains("Resolved project argument")),
            "fell back to reading from 0"
        );
    }

    #[test]
    fn missing_file_is_not_an_error_path_level() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.txt");
        let mut cur = TailCursor::new();
        assert!(read_new_lines(&path, &mut cur).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn shared_read_while_writer_holds_the_file_open() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        // A writer keeps the file open (as UBT does) ...
        let mut writer = OpenOptions::new()
            .create(true)
            .append(true)
            .share_mode(0x7)
            .open(&path)
            .unwrap();
        writer.write_all(b"[1/5] Compile a\n").unwrap();
        writer.flush().unwrap();

        let mut cur = TailCursor::new();
        let r = read_new_lines(&path, &mut cur).expect("shared read succeeds");
        assert_eq!(r.lines, vec!["[1/5] Compile a"]);

        writer.write_all(b"[2/5] Compile b\n").unwrap();
        writer.flush().unwrap();
        let r = read_new_lines(&path, &mut cur).unwrap();
        assert_eq!(r.lines, vec!["[2/5] Compile b"]);
    }
}
