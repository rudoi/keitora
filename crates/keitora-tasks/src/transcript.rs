//! Reply text from agent transcripts (Claude's session `.jsonl`, Codex's rollout
//! `.jsonl`): pure parsers over the parsed lines of a transcript tail, plus the tail
//! read itself. Shared by the CLI's Stop hook (the last reply in the file) and the
//! app's turn tracker (the reply of one finished turn, selected by when the turn
//! started, never by "last line of the file").
//!
//! Every parser is lenient: a line that fails to parse or isn't a JSON object is
//! skipped, an unknown shape yields `None`, and nothing panics.
//!
//! Field paths confirmed against captured transcripts:
//! - **Claude**: every `user`/`assistant`/`system` line has an RFC 3339 `timestamp`
//!   (bookkeeping lines such as `mode` and `last-prompt` have none). A typed or queued
//!   prompt is a `user` line whose `message.content` is a string; harness lines carry
//!   `isMeta: true`, and a local command's echo is a `user` line with
//!   `rowSource: "local"` followed by a `system` `local_command` line. Reply text is
//!   `message.content[].text` of `assistant` lines; the final one has
//!   `message.stop_reason: "end_turn"` (not relied on).
//! - **Codex**: the rollout opens with `session_meta` whose `payload.cwd` is the
//!   working directory. A turn opens with `event_msg` `task_started` and ends with
//!   `event_msg` `task_complete` carrying `payload.last_agent_message`. Prompts appear
//!   as user-role `response_item` messages (no `user_message` event), and prompts sent
//!   back to back share one task. `turn_aborted` was not captured; its handling
//!   follows the synthetic fixtures.

#[path = "transcript_agy.rs"]
pub mod agy;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::CliKind;

/// Bytes of transcript tail scanned for a reply.
pub const TRANSCRIPT_TAIL: u64 = 256 * 1024;
/// Bytes of transcript tail scanned once more when the first tail was cut and shows no
/// turn start (a single huge line pushed it out). A turn whose start is in the first
/// tail is never re-read: a longer tail can't change its reply.
pub const TRANSCRIPT_TAIL_RETRY: u64 = 2 * 1024 * 1024;
/// Bytes of a recorded reply, marker included (the `session_replies` cap).
pub const REPLY_MAX: usize = crate::SESSION_REPLY_MAX;
/// Clock and flush skew allowed between a delivery (or a detection) and the
/// transcript's turn start.
pub const TURN_START_SKEW_MS: i64 = 2_000;

/// When the turn being read could have started: at or after its delivery (less
/// [`TURN_START_SKEW_MS`]) and at or before its end was detected (plus the skew). A
/// prompt logged after the detection opens a later turn and is never this one's start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnWindow {
    /// The delivery, unix ms.
    pub delivered_at: i64,
    /// When the turn's end was detected, unix ms.
    pub detected_at: i64,
}

impl TurnWindow {
    pub fn new(delivered_at: i64, detected_at: i64) -> Self {
        TurnWindow {
            delivered_at,
            detected_at,
        }
    }

    /// No detection bound: the newest turn started since the delivery.
    pub fn since(delivered_at: i64) -> Self {
        Self::new(delivered_at, i64::MAX)
    }

    /// Whether a turn start logged at `t` (unix ms) belongs to this window, allowing
    /// it `early` ms before the delivery.
    pub fn admits(&self, t: i64, early: i64) -> bool {
        t >= self.delivered_at.saturating_sub(early)
            && t <= self.detected_at.saturating_add(TURN_START_SKEW_MS)
    }
}

const REPLY_MARKER: &str = "\n[truncated]";

/// `s` capped at [`REPLY_MAX`] bytes; a cut lands on a char boundary and ends with a
/// `\n[truncated]` marker.
pub fn truncate_reply(s: &str) -> String {
    if s.len() <= REPLY_MAX {
        return s.to_string();
    }
    let mut out = crate::truncate_bytes(s, REPLY_MAX - REPLY_MARKER.len());
    out.push_str(REPLY_MARKER);
    out
}

/// The `text` of every block of `block_type` in `content` (a bare string counts as
/// one `text` block).
fn text_blocks(content: Option<&Value>, block_type: &str) -> Vec<String> {
    match content {
        Some(Value::String(s)) if block_type == "text" => vec![s.clone()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some(block_type))
            .filter_map(|b| b.get("text").and_then(Value::as_str).map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn non_empty(parts: Vec<String>) -> Option<String> {
    let s = parts.join("\n");
    (!s.trim().is_empty()).then_some(s)
}

fn str_at<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str)
}

/// Claude: the trailing run of `assistant` lines (stopping at the first `user` line
/// from the end), their `text` blocks in file order.
fn claude_reply<'a>(lines: impl DoubleEndedIterator<Item = &'a Value>) -> Option<String> {
    let mut run: Vec<&Value> = Vec::new();
    for v in lines.rev() {
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => run.push(v),
            Some("user") => break,
            _ => {}
        }
    }
    let parts = run
        .iter()
        .rev()
        .flat_map(|v| text_blocks(v.pointer("/message/content"), "text"))
        .collect();
    non_empty(parts)
}

/// Codex: the last assistant `response_item` message's `output_text` blocks, else the
/// last `agent_message` event.
fn codex_reply(lines: &[Value]) -> Option<String> {
    let from_items = lines
        .iter()
        .rev()
        .filter(|v| is_response_msg(v, "assistant"))
        .find_map(|v| non_empty(text_blocks(v.pointer("/payload/content"), "output_text")));
    from_items.or_else(|| {
        lines
            .iter()
            .rev()
            .filter(|v| is_event(v, "agent_message"))
            .find_map(|v| {
                str_at(v, "/payload/message")
                    .filter(|m| !m.trim().is_empty())
                    .map(str::to_string)
            })
    })
}

/// A Codex `event_msg` line of `kind`.
fn is_event(v: &Value, kind: &str) -> bool {
    str_at(v, "/type") == Some("event_msg") && str_at(v, "/payload/type") == Some(kind)
}

/// A Codex `response_item` message line from `role`.
fn is_response_msg(v: &Value, role: &str) -> bool {
    str_at(v, "/type") == Some("response_item")
        && str_at(v, "/payload/type") == Some("message")
        && str_at(v, "/payload/role") == Some(role)
}

/// The JSON-object lines of a transcript tail, in file order. `truncated_front` drops
/// the first line, which a tail read may have cut. CRLF endings and a torn last line
/// are tolerated (the torn line fails to parse and is skipped).
pub fn parse_lines(tail: &str, truncated_front: bool) -> Vec<Value> {
    tail.lines()
        .skip(usize::from(truncated_front))
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter(Value::is_object)
        .collect()
}

/// The last assistant reply in a transcript tail (Claude or Codex, detected from the
/// line shapes). `partial_first_line` drops the first line, which a tail read may have
/// cut. Lines that fail to parse are skipped.
pub fn last_assistant_text_from_tail(tail: &str, partial_first_line: bool) -> Option<String> {
    let lines = parse_lines(tail, partial_first_line);
    claude_reply(lines.iter()).or_else(|| codex_reply(&lines))
}

/// How a turn ended, as its transcript shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    Complete,
    Aborted,
}

/// Why a turn's lines gave no reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnMiss {
    /// No turn started at or after the delivery (less [`TURN_START_SKEW_MS`]); a
    /// longer tail may still show one.
    NoStart,
    /// The turn started but has no end yet, or ended without reply text.
    NoReply,
}

/// One turn's reply text (already capped by [`truncate_reply`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnReply {
    pub text: String,
    pub outcome: TurnOutcome,
}

/// An RFC 3339 timestamp as unix ms.
pub fn rfc3339_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// A line's `timestamp` field as unix ms; `None` when it has none (or it doesn't parse).
fn line_ms(v: &Value) -> Option<i64> {
    v.get("timestamp")
        .and_then(Value::as_str)
        .and_then(rfc3339_ms)
}

/// Whether the line's timestamp falls in `window`, allowing it `early` ms before the
/// delivery (a line without one never does).
fn in_window(v: &Value, window: TurnWindow, early: i64) -> bool {
    line_ms(v).is_some_and(|t| window.admits(t, early))
}

/// The reply of the turn that answered a delivery, from a transcript's parsed lines
/// in file order: the newest turn started in `window`. [`TurnMiss::NoStart`] when the
/// lines show no turn started in it; [`TurnMiss::NoReply`] when the turn has no end
/// yet or no reply text. Text from before the turn start, or from a later turn, is
/// never returned. Antigravity keeps no JSON-line transcript (see [`agy`]), so its
/// lines never show a start.
pub fn reply_for_turn(
    cli: CliKind,
    lines: &[Value],
    window: TurnWindow,
) -> Result<TurnReply, TurnMiss> {
    match cli {
        CliKind::Claude => claude_turn(lines, window),
        CliKind::Codex => codex_turn(lines, window),
        CliKind::Antigravity => Err(TurnMiss::NoStart),
    }
}

/// Tags opening the content of a local slash command's `user` echo lines (the
/// command itself, and its output).
const LOCAL_COMMAND_TAGS: [&str; 3] = [
    "<command-name>",
    "<local-command-stdout>",
    "<local-command-stderr>",
];

/// A Claude `user` line echoing a local slash command (`/cost`, `/clear`, ...) or its
/// output: `rowSource: "local"`, or content opening with a local command tag. Such a
/// line is neither a prompt nor the end of a reply.
fn is_local_command_echo(v: &Value) -> bool {
    if v.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    if v.get("rowSource").and_then(Value::as_str) == Some("local") {
        return true;
    }
    let tagged = |s: &str| {
        let s = s.trim_start();
        LOCAL_COMMAND_TAGS.iter().any(|t| s.starts_with(t))
    };
    match v.pointer("/message/content") {
        Some(Value::String(s)) => tagged(s),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|b| b.get("text").and_then(Value::as_str))
            .is_some_and(tagged),
        _ => false,
    }
}

/// A Claude `user` line that is a prompt: string content, or an array with a `text`
/// block and no `tool_result`; never a meta line, a compaction summary, a sub-agent's
/// line or a local command's echo.
fn is_claude_prompt(v: &Value) -> bool {
    if v.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let flag = |k: &str| v.get(k).and_then(Value::as_bool) == Some(true);
    if flag("isMeta") || flag("isCompactSummary") || flag("isSidechain") {
        return false;
    }
    if is_local_command_echo(v) {
        return false;
    }
    match v.pointer("/message/content") {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => {
            let has = |t: &str| {
                blocks
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some(t))
            };
            has("text") && !has("tool_result")
        }
        _ => false,
    }
}

fn claude_turn(lines: &[Value], window: TurnWindow) -> Result<TurnReply, TurnMiss> {
    let start = lines
        .iter()
        .rposition(|v| is_claude_prompt(v) && in_window(v, window, TURN_START_SKEW_MS))
        .ok_or(TurnMiss::NoStart)?;
    // The turn ends where the next prompt (a later turn) starts.
    let end = lines[start + 1..]
        .iter()
        .position(is_claude_prompt)
        .map_or(lines.len(), |i| start + 1 + i);
    let main_thread = lines[start..end].iter().filter(|v| {
        v.get("isSidechain").and_then(Value::as_bool) != Some(true) && !is_local_command_echo(v)
    });
    let text = claude_reply(main_thread).ok_or(TurnMiss::NoReply)?;
    Ok(TurnReply {
        text: truncate_reply(&text),
        outcome: TurnOutcome::Complete,
    })
}

/// A Codex line carrying the user's prompt: a `user_message` event, or a user-role
/// `response_item` message.
fn is_codex_prompt(v: &Value) -> bool {
    is_event(v, "user_message") || is_response_msg(v, "user")
}

fn codex_turn(lines: &[Value], window: TurnWindow) -> Result<TurnReply, TurnMiss> {
    let start = lines
        .iter()
        .rposition(|v| is_event(v, "task_started") && in_window(v, window, TURN_START_SKEW_MS))
        .or_else(|| {
            lines
                .iter()
                .rposition(|v| is_codex_prompt(v) && in_window(v, window, 0))
        })
        .ok_or(TurnMiss::NoStart)?;
    let span = &lines[start..];
    let end = span
        .iter()
        .position(|v| is_event(v, "task_complete") || is_event(v, "turn_aborted"))
        .ok_or(TurnMiss::NoReply)?;
    let (outcome, text) = if is_event(&span[end], "task_complete") {
        let last = str_at(&span[end], "/payload/last_agent_message")
            .filter(|m| !m.trim().is_empty())
            .map(str::to_string);
        (
            TurnOutcome::Complete,
            last.or_else(|| codex_reply(&span[..end])),
        )
    } else {
        (TurnOutcome::Aborted, codex_reply(&span[..end]))
    };
    Ok(TurnReply {
        text: truncate_reply(&text.ok_or(TurnMiss::NoReply)?),
        outcome,
    })
}

/// The text of a transcript's tail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TailRead {
    pub text: String,
    /// The read started mid-file, so its first line may be cut.
    pub truncated_front: bool,
}

/// The last `max_bytes` of the regular file at `path`, read without locking it.
pub fn read_tail(path: &Path, max_bytes: u64) -> std::io::Result<TailRead> {
    let mut f = File::open(path)?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let truncated_front = meta.len() > max_bytes;
    if truncated_front {
        f.seek(SeekFrom::Start(meta.len() - max_bytes))?;
    }
    let mut buf = Vec::new();
    f.take(max_bytes).read_to_end(&mut buf)?;
    Ok(TailRead {
        text: String::from_utf8_lossy(&buf).into_owned(),
        truncated_front,
    })
}

/// Why a transcript gave no reply for a turn.
pub mod miss {
    /// No transcript file was found (or it could not be read).
    pub const NOT_FOUND: &str = "not_found";
    /// The transcript shows no finished turn with reply text since the delivery.
    pub const NOT_THIS_TURN: &str = "not_this_turn";
    /// The transcript has no line Keitora understands.
    pub const PARSE_FAILED: &str = "parse_failed";
    /// No transcript reader exists for the agent.
    pub const NO_PROVIDER_SUPPORT: &str = "no_provider_support";
}

/// The reply of the newest turn started in `window`, read from `cli`'s transcript at
/// `path`: for Claude and Codex its last [`TRANSCRIPT_TAIL`] bytes,
/// then once its last [`TRANSCRIPT_TAIL_RETRY`] bytes when the first read was cut and
/// shows no turn start; for Antigravity its conversation file (see [`agy`]). Errors are
/// [`miss`] reasons.
pub fn read_turn_reply(
    cli: CliKind,
    path: &Path,
    window: TurnWindow,
) -> Result<TurnReply, &'static str> {
    if cli == CliKind::Antigravity {
        return agy::read_agy_turn_reply(path, window);
    }
    let mut last = miss::PARSE_FAILED;
    for max in [TRANSCRIPT_TAIL, TRANSCRIPT_TAIL_RETRY] {
        let tail = read_tail(path, max).map_err(|_| miss::NOT_FOUND)?;
        let lines = parse_lines(&tail.text, tail.truncated_front);
        if !lines.is_empty() {
            match reply_for_turn(cli, &lines, window) {
                Ok(r) => return Ok(r),
                // The turn's start is in this tail, so a longer one reads the same.
                Err(TurnMiss::NoReply) => return Err(miss::NOT_THIS_TURN),
                Err(TurnMiss::NoStart) => last = miss::NOT_THIS_TURN,
            }
        }
        if !tail.truncated_front {
            break;
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T0: &str = "2026-01-01T00:00:10.000Z";

    fn ms(s: &str) -> i64 {
        rfc3339_ms(s).unwrap()
    }

    fn claude(kind: &str, ts: &str, content: Value) -> Value {
        json!({"type": kind, "timestamp": ts, "message": {"role": kind, "content": content}})
    }

    #[test]
    fn rfc3339_parses_offsets_and_rejects_garbage() {
        assert_eq!(
            rfc3339_ms("2026-01-01T00:00:00.500Z"),
            rfc3339_ms("2026-01-01T01:00:00.500+01:00")
        );
        assert_eq!(rfc3339_ms("yesterday"), None);
        assert_eq!(line_ms(&json!({"timestamp": 5})), None);
    }

    #[test]
    fn claude_prompt_detection_skips_tool_results_meta_and_sidechains() {
        assert!(is_claude_prompt(&claude("user", T0, json!("hi"))));
        assert!(is_claude_prompt(&claude(
            "user",
            T0,
            json!([{"type": "text", "text": "hi"}])
        )));
        assert!(!is_claude_prompt(&claude(
            "user",
            T0,
            json!([{"type": "tool_result", "content": "x"}, {"type": "text", "text": "y"}])
        )));
        let mut meta = claude("user", T0, json!("hook context"));
        meta["isMeta"] = json!(true);
        assert!(!is_claude_prompt(&meta));
        let mut side = claude("user", T0, json!("sub prompt"));
        side["isSidechain"] = json!(true);
        assert!(!is_claude_prompt(&side));
        assert!(!is_claude_prompt(&claude("assistant", T0, json!("x"))));
        assert!(!is_claude_prompt(&json!({"type": "user"})));
    }

    #[test]
    fn a_line_without_a_timestamp_never_starts_a_turn() {
        let lines = vec![
            json!({"type": "user", "message": {"role": "user", "content": "hi"}}),
            claude("assistant", T0, json!([{"type": "text", "text": "hello"}])),
        ];
        assert_eq!(
            reply_for_turn(CliKind::Claude, &lines, TurnWindow::since(0)),
            Err(TurnMiss::NoStart)
        );
    }

    #[test]
    fn skew_lets_a_prompt_logged_just_before_the_delivery_count() {
        let lines = vec![
            claude("user", T0, json!("hi")),
            claude("assistant", T0, json!([{"type": "text", "text": "hello"}])),
        ];
        let at = ms(T0);
        let since = TurnWindow::since;
        assert!(reply_for_turn(CliKind::Claude, &lines, since(at + TURN_START_SKEW_MS)).is_ok());
        assert_eq!(
            reply_for_turn(CliKind::Claude, &lines, since(at + TURN_START_SKEW_MS + 1)),
            Err(TurnMiss::NoStart)
        );
    }

    #[test]
    fn codex_turn_without_an_end_is_none_and_aborted_empty_is_none() {
        let start =
            json!({"timestamp": T0, "type": "event_msg", "payload": {"type": "task_started"}});
        let item = json!({"timestamp": T0, "type": "response_item", "payload": {"type": "message",
            "role": "assistant", "content": [{"type": "output_text", "text": "so far"}]}});
        let aborted =
            json!({"timestamp": T0, "type": "event_msg", "payload": {"type": "turn_aborted"}});
        let at = TurnWindow::since(ms(T0));
        assert_eq!(
            reply_for_turn(CliKind::Codex, &[start.clone(), item.clone()], at),
            Err(TurnMiss::NoReply)
        );
        assert_eq!(
            reply_for_turn(CliKind::Codex, &[start.clone(), aborted.clone()], at),
            Err(TurnMiss::NoReply)
        );
        assert_eq!(
            reply_for_turn(CliKind::Codex, &[start, item, aborted], at),
            Ok(TurnReply {
                text: "so far".into(),
                outcome: TurnOutcome::Aborted
            })
        );
    }

    #[test]
    fn a_local_command_echo_after_the_reply_is_not_a_turn() {
        let at = "2026-01-01T00:00:12.000Z";
        let mut cost = claude(
            "user",
            at,
            json!("<command-name>/cost</command-name>\n<command-message>cost</command-message>"),
        );
        cost["rowSource"] = json!("local");
        let system = json!({"type": "system", "subtype": "local_command", "timestamp": at});
        // The output echo, as an array and without a `rowSource`.
        let stdout = claude(
            "user",
            at,
            json!([{"type": "text", "text": "<local-command-stdout>Total cost: $0</local-command-stdout>"}]),
        );
        let stderr = claude(
            "user",
            at,
            json!("<local-command-stderr>x</local-command-stderr>"),
        );
        let lines = vec![
            claude("user", T0, json!("hi")),
            claude("assistant", T0, json!([{"type": "text", "text": "hello"}])),
            cost.clone(),
            system,
            stdout.clone(),
            stderr.clone(),
        ];
        for v in [&cost, &stdout, &stderr] {
            assert!(is_local_command_echo(v) && !is_claude_prompt(v), "{v}");
        }
        assert_eq!(
            reply_for_turn(CliKind::Claude, &lines, TurnWindow::since(ms(T0))).map(|r| r.text),
            Ok("hello".to_string())
        );
        // A prompt that merely mentions a tag is still a prompt.
        assert!(is_claude_prompt(&claude(
            "user",
            T0,
            json!("what does <command-name> do?")
        )));
    }

    /// A turn's lines, then a prompt typed after the turn's end was detected.
    fn claude_turn_then_a_later_prompt() -> (Vec<Value>, TurnWindow) {
        let lines = vec![
            claude("user", T0, json!("first")),
            claude(
                "assistant",
                "2026-01-01T00:00:11.000Z",
                json!([{"type": "text", "text": "first reply"}]),
            ),
            claude("user", "2026-01-01T00:00:20.000Z", json!("second")),
            claude(
                "assistant",
                "2026-01-01T00:00:21.000Z",
                json!([{"type": "text", "text": "second, half"}]),
            ),
        ];
        let window = TurnWindow::new(ms(T0), ms("2026-01-01T00:00:12.000Z"));
        (lines, window)
    }

    #[test]
    fn claude_prompt_after_detection_never_takes_the_reply() {
        let (lines, window) = claude_turn_then_a_later_prompt();
        assert_eq!(
            reply_for_turn(CliKind::Claude, &lines, window).map(|r| r.text),
            Ok("first reply".to_string())
        );
        // Only the later prompt's lines so far: no reply, never the later turn's.
        let (mut lines, window) = claude_turn_then_a_later_prompt();
        lines.remove(1);
        assert_eq!(
            reply_for_turn(CliKind::Claude, &lines, window),
            Err(TurnMiss::NoReply)
        );
    }

    #[test]
    fn codex_task_started_after_detection_never_takes_the_reply() {
        let ev = |ts: &str, payload: Value| json!({"timestamp": ts, "type": "event_msg", "payload": payload});
        let lines = vec![
            ev(T0, json!({"type": "task_started"})),
            ev(
                "2026-01-01T00:00:11.000Z",
                json!({"type": "task_complete", "last_agent_message": "first reply"}),
            ),
            ev("2026-01-01T00:00:20.000Z", json!({"type": "task_started"})),
        ];
        let window = TurnWindow::new(ms(T0), ms("2026-01-01T00:00:12.000Z"));
        assert_eq!(
            reply_for_turn(CliKind::Codex, &lines, window).map(|r| r.text),
            Ok("first reply".to_string())
        );
        // A prompt-only fallback start is bounded the same way.
        let prompt = |ts: &str, text: &str| {
            json!({"timestamp": ts, "type": "response_item", "payload": {"type": "message",
                "role": "user", "content": [{"type": "input_text", "text": text}]}})
        };
        let lines = vec![
            prompt(T0, "first"),
            ev(
                "2026-01-01T00:00:11.000Z",
                json!({"type": "task_complete", "last_agent_message": "first reply"}),
            ),
            prompt("2026-01-01T00:00:20.000Z", "second"),
        ];
        assert_eq!(
            reply_for_turn(CliKind::Codex, &lines, window).map(|r| r.text),
            Ok("first reply".to_string())
        );
    }

    #[test]
    fn the_detection_bound_allows_the_skew() {
        let lines = vec![
            claude("user", T0, json!("hi")),
            claude("assistant", T0, json!([{"type": "text", "text": "hello"}])),
        ];
        let at = ms(T0);
        let ok = TurnWindow::new(at - 5_000, at - TURN_START_SKEW_MS);
        assert!(reply_for_turn(CliKind::Claude, &lines, ok).is_ok());
        let late = TurnWindow::new(at - 5_000, at - TURN_START_SKEW_MS - 1);
        assert_eq!(
            reply_for_turn(CliKind::Claude, &lines, late),
            Err(TurnMiss::NoStart)
        );
    }

    #[test]
    fn the_reply_is_capped() {
        let long = "x".repeat(REPLY_MAX * 2);
        let lines = vec![
            claude("user", T0, json!("hi")),
            claude("assistant", T0, json!([{"type": "text", "text": long}])),
        ];
        let r = reply_for_turn(CliKind::Claude, &lines, TurnWindow::since(ms(T0))).unwrap();
        assert!(r.text.len() <= REPLY_MAX);
        assert!(r.text.ends_with(REPLY_MARKER));
    }

    #[test]
    fn read_tail_marks_a_cut_front() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "0123456789").unwrap();
        assert_eq!(
            read_tail(&p, 4).unwrap(),
            TailRead {
                text: "6789".into(),
                truncated_front: true
            }
        );
        assert!(!read_tail(&p, 10).unwrap().truncated_front);
        assert!(read_tail(dir.path(), 10).is_err());
        assert!(read_tail(&dir.path().join("missing"), 10).is_err());
    }

    #[test]
    fn read_turn_reply_reports_why_it_missed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        assert_eq!(
            read_turn_reply(CliKind::Claude, &p, TurnWindow::since(0)),
            Err(miss::NOT_FOUND)
        );
        std::fs::write(&p, "not json\n").unwrap();
        assert_eq!(
            read_turn_reply(CliKind::Claude, &p, TurnWindow::since(0)),
            Err(miss::PARSE_FAILED)
        );
        std::fs::write(&p, format!("{}\n", claude("user", T0, json!("hi")))).unwrap();
        assert_eq!(
            read_turn_reply(CliKind::Claude, &p, TurnWindow::since(0)),
            Err(miss::NOT_THIS_TURN)
        );
    }
}
