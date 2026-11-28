//! The hook error log: one line per failed or deliberately skipped hook, under
//! `<keitora home>/logs/hooks.log`, so a hook that writes nothing leaves a trace.
//!
//! Hook commands discard the CLI's output (stdout of a state hook would reach the
//! agent), so this file is where a hook's failure is visible. Successes are not
//! logged. The line format lives in [`keitora_tasks::hook_log`], which the app reads
//! back. Every I/O error here is swallowed: logging must never fail a hook.

use std::io::Write as _;
use std::path::Path;

use keitora_tasks::hook_log::{format_line, hook_log_in, HOOK_LOG_MAX_BYTES};
use keitora_tasks::HookLogLevel;

/// One event to log, with the context of the hook that hit it.
pub struct HookEvent<'a> {
    pub level: HookLogLevel,
    /// The command line after the binary, e.g. `session busy --payload-session`.
    pub cmd: &'a str,
    pub cwd: &'a str,
    pub message: &'a str,
}

/// A rotation lock older than this was left by a process that died mid-rotation.
const ROTATE_LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(60);

/// `<file>` with `suffix` appended to its name.
fn sibling(file: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

/// The lock file held while one process rotates `file`.
fn rotate_lock(file: &Path) -> std::path::PathBuf {
    sibling(file, ".rotating")
}

fn past_cap(file: &Path, cap: u64) -> bool {
    std::fs::metadata(file).is_ok_and(|m| m.len() > cap)
}

/// Move `file` to its one `.1` backup when it is past `cap` bytes. Hook processes
/// run concurrently, so the move happens only under an exclusive lock file and
/// after re-checking the size there: two processes that both saw a full log
/// otherwise rotate twice, and the second move replaces the full backup with the
/// line the first had just started the new log with. A process that finds the
/// lock held skips rotating (the holder does it); a lock left by a dead process
/// is cleared once stale, and the next write rotates.
fn rotate_if_past_cap(file: &Path, cap: u64) {
    if !past_cap(file, cap) {
        return;
    }
    let lock = rotate_lock(file);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let stale = std::fs::metadata(&lock)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > ROTATE_LOCK_STALE));
            if stale {
                let _ = std::fs::remove_file(&lock);
            }
            return;
        }
        Err(_) => return,
    }
    if past_cap(file, cap) {
        let _ = std::fs::rename(file, sibling(file, ".1"));
    }
    let _ = std::fs::remove_file(&lock);
}

/// Append `ev` to `<dir>/hooks.log`, rotating first when the file is past `cap`
/// bytes (one `hooks.log.1` backup, replaced each time; see
/// [`rotate_if_past_cap`]). Creates `dir`. Never fails.
pub fn log_in(dir: &Path, ev: &HookEvent<'_>, unix_s: i64, cap: u64) {
    let _ = std::fs::create_dir_all(dir);
    let file = hook_log_in(dir);
    rotate_if_past_cap(&file, cap);
    let mut line = format_line(
        unix_s,
        ev.level,
        std::process::id(),
        ev.cmd,
        ev.cwd,
        ev.message,
    );
    line.push('\n');
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// This process's command line after the binary name.
pub fn current_cmd() -> String {
    std::env::args().skip(1).collect::<Vec<_>>().join(" ")
}

/// This process's working directory (empty when unreadable).
pub fn current_cwd() -> String {
    std::env::current_dir()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Log `message` for this hook process to the Keitora home's `logs/hooks.log`.
pub fn log(level: HookLogLevel, message: &str) {
    log_in(
        &keitora_tasks::hook_log::hook_log_dir(),
        &HookEvent {
            level,
            cmd: &current_cmd(),
            cwd: &current_cwd(),
            message,
        },
        keitora_tasks::now_ms() / 1000,
        HOOK_LOG_MAX_BYTES,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use keitora_tasks::hook_log::parse_line;

    fn ev<'a>(message: &'a str) -> HookEvent<'a> {
        HookEvent {
            level: HookLogLevel::Error,
            cmd: "session busy --payload-session",
            cwd: "/Users/me/proj",
            message,
        }
    }

    fn lines(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(hook_log_in(dir))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn appends_one_line_per_event_with_context() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        log_in(&dir, &ev("first"), 100, HOOK_LOG_MAX_BYTES);
        log_in(
            &dir,
            &HookEvent {
                level: HookLogLevel::Warn,
                ..ev("second")
            },
            101,
            HOOK_LOG_MAX_BYTES,
        );
        let got = lines(&dir);
        assert_eq!(got.len(), 2, "{got:?}");
        let a = parse_line(&got[0]).unwrap();
        assert_eq!(
            (a.unix_s, a.level, a.cmd.as_str(), a.cwd.as_str()),
            (
                100,
                HookLogLevel::Error,
                "session busy --payload-session",
                "/Users/me/proj"
            )
        );
        assert_eq!(a.pid, std::process::id());
        assert_eq!(a.message, "first");
        let b = parse_line(&got[1]).unwrap();
        assert_eq!(
            (b.level, b.message.as_str()),
            (HookLogLevel::Warn, "second")
        );
    }

    #[test]
    fn escapes_newlines_in_messages() {
        let tmp = tempfile::tempdir().unwrap();
        log_in(
            tmp.path(),
            &ev("line one\nline two\r\n"),
            1,
            HOOK_LOG_MAX_BYTES,
        );
        let got = lines(tmp.path());
        assert_eq!(got.len(), 1, "one physical line: {got:?}");
        assert!(got[0].ends_with("line one\\nline two\\r\\n"), "{}", got[0]);
        assert_eq!(
            parse_line(&got[0]).unwrap().message,
            "line one\nline two\r\n"
        );
    }

    #[test]
    fn rotates_to_one_backup_past_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let backup = dir.join("hooks.log.1");
        log_in(dir, &ev("a"), 1, 10);
        assert!(!backup.exists(), "nothing to rotate on the first write");
        // The file is now past 10 bytes: the next append rotates it.
        log_in(dir, &ev("b"), 2, 10);
        assert_eq!(parse_line(&lines(dir)[0]).unwrap().message, "b");
        let old = std::fs::read_to_string(&backup).unwrap();
        assert_eq!(parse_line(old.trim_end()).unwrap().message, "a");
        // The backup is replaced, never chained.
        log_in(dir, &ev("c"), 3, 10);
        let old = std::fs::read_to_string(&backup).unwrap();
        assert_eq!(parse_line(old.trim_end()).unwrap().message, "b");
        assert!(!dir.join("hooks.log.2").exists());
        assert_eq!(lines(dir).len(), 1);
    }

    #[test]
    fn a_rotation_in_progress_elsewhere_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let backup = dir.join("hooks.log.1");
        std::fs::write(&backup, "older backup\n").unwrap();
        log_in(dir, &ev("a"), 1, 10);
        // Another hook process holds the rotation lock: this one neither rotates
        // nor touches the backup, and still appends its line.
        std::fs::write(rotate_lock(&hook_log_in(dir)), b"").unwrap();
        log_in(dir, &ev("b"), 2, 10);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "older backup\n");
        assert_eq!(lines(dir).len(), 2);
    }

    #[test]
    fn rotation_releases_its_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        log_in(dir, &ev("a"), 1, 10);
        log_in(dir, &ev("b"), 2, 10);
        assert!(dir.join("hooks.log.1").is_file(), "rotated");
        assert!(!rotate_lock(&hook_log_in(dir)).exists(), "lock released");
    }

    #[test]
    fn a_stale_rotation_lock_is_cleared() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let lock = rotate_lock(&hook_log_in(dir));
        log_in(dir, &ev("a"), 1, 10);
        // A process that died while rotating left its lock behind long ago.
        let f = std::fs::File::create(&lock).unwrap();
        f.set_modified(std::time::SystemTime::now() - ROTATE_LOCK_STALE * 2)
            .unwrap();
        drop(f);
        log_in(dir, &ev("b"), 2, 10);
        log_in(dir, &ev("c"), 3, 10);
        assert!(!lock.exists(), "stale lock cleared");
        assert!(dir.join("hooks.log.1").is_file(), "rotation resumed");
    }

    #[test]
    fn swallows_io_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // The log dir is a file: nothing can be created under it, and nothing panics.
        let not_a_dir = tmp.path().join("logs");
        std::fs::write(&not_a_dir, b"x").unwrap();
        log_in(&not_a_dir, &ev("lost"), 1, HOOK_LOG_MAX_BYTES);
        assert_eq!(std::fs::read(&not_a_dir).unwrap(), b"x");
    }

    #[test]
    fn path_is_under_the_given_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let dir = home.join("logs");
        log_in(&dir, &ev("x"), 1, HOOK_LOG_MAX_BYTES);
        assert!(home.join("logs").join("hooks.log").is_file());
        assert_eq!(hook_log_in(&dir), home.join("logs").join("hooks.log"));
    }
}
