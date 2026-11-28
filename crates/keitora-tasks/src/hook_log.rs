//! The hook error log line format, shared by the CLI (which appends the lines) and
//! the app (which reads them back for its hook diagnostics).
//!
//! One line per event:
//! `unix=<epoch secs> LEVEL pid=<pid> cmd="<cmd>" cwd="<cwd>" <message>`.
//! `LEVEL` is `ERROR` (the hook failed) or `WARN` (it skipped on purpose). Inside
//! the quoted fields and the message, `\` `"` CR and LF are escaped (`\\`, `\"`,
//! `\r`, `\n`), so every event stays on one line and the quoted fields parse back.

use std::path::{Path, PathBuf};

/// The log's file name, under `<keitora home>/logs/`.
pub const HOOK_LOG_FILE: &str = "hooks.log";
/// Past this size the log is rotated to one `.1` backup before the next append.
pub const HOOK_LOG_MAX_BYTES: u64 = 256 * 1024;

/// How bad a logged hook event is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookLogLevel {
    /// The hook failed.
    Error,
    /// The hook skipped on purpose (unusable payload, filtered event, no such dir).
    Warn,
}

impl HookLogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            HookLogLevel::Error => "ERROR",
            HookLogLevel::Warn => "WARN",
        }
    }

    /// The level named by `s` ([`Self::as_str`]'s text); `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ERROR" => Some(HookLogLevel::Error),
            "WARN" => Some(HookLogLevel::Warn),
            _ => None,
        }
    }
}

/// One parsed log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookLogLine {
    pub unix_s: i64,
    pub level: HookLogLevel,
    pub pid: u32,
    /// The hook's command line after the binary, e.g. `session busy --payload-session`.
    pub cmd: String,
    /// The hook process's working directory.
    pub cwd: String,
    pub message: String,
}

/// `<dir>/hooks.log`.
pub fn hook_log_in(dir: &Path) -> PathBuf {
    dir.join(HOOK_LOG_FILE)
}

/// `<keitora home>/logs`, the directory the log lives in.
pub fn hook_log_dir() -> PathBuf {
    crate::keitora_home().join("logs")
}

/// `<keitora home>/logs/hooks.log`.
pub fn hook_log_path() -> PathBuf {
    hook_log_in(&hook_log_dir())
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// One log line, without the trailing newline.
pub fn format_line(
    unix_s: i64,
    level: HookLogLevel,
    pid: u32,
    cmd: &str,
    cwd: &str,
    message: &str,
) -> String {
    format!(
        "unix={unix_s} {} pid={pid} cmd=\"{}\" cwd=\"{}\" {}",
        level.as_str(),
        escape(cmd),
        escape(cwd),
        escape(message)
    )
}

/// `rest` starts with `<key>="…"`: the unescaped value and what follows the closing
/// quote (its leading space dropped).
fn quoted<'a>(rest: &'a str, key: &str) -> Option<(String, &'a str)> {
    let body = rest.strip_prefix(key)?.strip_prefix("=\"")?;
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => {
                let tail = &body[i + 1..];
                return Some((unescape(&body[..i]), tail.strip_prefix(' ').unwrap_or(tail)));
            }
            _ => {}
        }
    }
    None
}

/// Parse a line written by [`format_line`]; `None` for anything else.
pub fn parse_line(line: &str) -> Option<HookLogLine> {
    let line = line.trim_end_matches(['\r', '\n']);
    let rest = line.strip_prefix("unix=")?;
    let (unix, rest) = rest.split_once(' ')?;
    let unix_s = unix.parse().ok()?;
    let (level, rest) = rest.split_once(' ')?;
    let level = HookLogLevel::parse(level)?;
    let (pid, rest) = rest.strip_prefix("pid=")?.split_once(' ')?;
    let pid = pid.parse().ok()?;
    let (cmd, rest) = quoted(rest, "cmd")?;
    let (cwd, rest) = quoted(rest, "cwd")?;
    Some(HookLogLine {
        unix_s,
        level,
        pid,
        cmd,
        cwd,
        message: unescape(rest),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_formatted_line_parses_back() {
        let line = format_line(
            1_700_000_000,
            HookLogLevel::Error,
            42,
            "session busy --payload-session",
            "C:/Users/x/proj",
            "database is locked",
        );
        assert_eq!(
            line,
            "unix=1700000000 ERROR pid=42 cmd=\"session busy --payload-session\" cwd=\"C:/Users/x/proj\" database is locked"
        );
        let p = parse_line(&line).unwrap();
        assert_eq!(p.unix_s, 1_700_000_000);
        assert_eq!(p.level, HookLogLevel::Error);
        assert_eq!(p.pid, 42);
        assert_eq!(p.cmd, "session busy --payload-session");
        assert_eq!(p.cwd, "C:/Users/x/proj");
        assert_eq!(p.message, "database is locked");
    }

    #[test]
    fn newlines_quotes_and_backslashes_round_trip_on_one_line() {
        let line = format_line(
            1,
            HookLogLevel::Warn,
            7,
            "session \"idle\"",
            "D:\\repos\\proj",
            "first\r\nsecond \\ \"q\"",
        );
        assert!(!line.contains('\n') && !line.contains('\r'), "{line}");
        let p = parse_line(&format!("{line}\n")).unwrap();
        assert_eq!(p.level, HookLogLevel::Warn);
        assert_eq!(p.cmd, "session \"idle\"");
        assert_eq!(p.cwd, "D:\\repos\\proj");
        assert_eq!(p.message, "first\r\nsecond \\ \"q\"");
    }

    #[test]
    fn foreign_lines_do_not_parse() {
        for line in [
            "",
            "hello",
            "unix=x ERROR pid=1 cmd=\"a\" cwd=\"b\" m",
            "unix=1 INFO pid=1 cmd=\"a\" cwd=\"b\" m",
            "unix=1 ERROR pid=1 cmd=\"a cwd=\"b\" m",
            "unix=1 ERROR pid=1 cmd=\"a\"",
        ] {
            assert_eq!(parse_line(line), None, "{line:?}");
        }
    }

    #[test]
    fn the_log_lives_in_the_logs_dir() {
        assert_eq!(
            hook_log_in(Path::new("/Users/me/.keitora/logs")),
            Path::new("/Users/me/.keitora/logs").join("hooks.log")
        );
    }
}
