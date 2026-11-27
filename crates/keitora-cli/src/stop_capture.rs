//! Stop-hook reply capture (`keitora session idle --stdin` / `--failed`): the turn's
//! last assistant message, from the Stop payload or, failing that, the tail of the
//! session transcript. Pure over strings; only [`last_assistant_text`] reads a file.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

/// Most bytes of Stop payload read from stdin.
pub const STOP_STDIN_CAP: u64 = 64 * 1024;
/// How long the Stop capture waits for stdin.
pub const STOP_STDIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Bytes of transcript tail scanned for the last reply.
pub const TRANSCRIPT_TAIL: u64 = 256 * 1024;
/// Bytes of a recorded reply, marker included.
pub const REPLY_MAX: usize = 32 * 1024;
/// The reply recorded for a `StopFailure`.
pub const FAILED_MESSAGE: &str = "turn ended with an error";

const REPLY_MARKER: &str = "\n[truncated]";

/// `s` capped at [`REPLY_MAX`] bytes; a cut lands on a char boundary and ends with a
/// `\n[truncated]` marker.
pub fn truncate_reply(s: &str) -> String {
    if s.len() <= REPLY_MAX {
        return s.to_string();
    }
    let mut out = keitora_tasks::truncate_bytes(s, REPLY_MAX - REPLY_MARKER.len());
    out.push_str(REPLY_MARKER);
    out
}

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

/// Claude: the trailing run of `assistant` lines (stopping at the first `user` line
/// from the end), their `text` blocks in file order.
fn claude_reply(lines: &[Value]) -> Option<String> {
    let mut run: Vec<&Value> = Vec::new();
    for v in lines.iter().rev() {
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
    let str_at = |v: &Value, p: &str| v.pointer(p).and_then(Value::as_str).map(str::to_string);
    let from_items = lines.iter().rev().find_map(|v| {
        let is_msg = str_at(v, "/type").as_deref() == Some("response_item")
            && str_at(v, "/payload/type").as_deref() == Some("message")
            && str_at(v, "/payload/role").as_deref() == Some("assistant");
        if !is_msg {
            return None;
        }
        non_empty(text_blocks(v.pointer("/payload/content"), "output_text"))
    });
    from_items.or_else(|| {
        lines.iter().rev().find_map(|v| {
            let is_event = str_at(v, "/type").as_deref() == Some("event_msg")
                && str_at(v, "/payload/type").as_deref() == Some("agent_message");
            if is_event {
                str_at(v, "/payload/message").filter(|m| !m.trim().is_empty())
            } else {
                None
            }
        })
    })
}

/// The last assistant reply in a transcript tail (Claude or Codex, detected from the
/// line shapes). `partial_first_line` drops the first line, which a tail read may have
/// cut. Lines that fail to parse are skipped.
pub fn last_assistant_text_from_tail(tail: &str, partial_first_line: bool) -> Option<String> {
    let skip = usize::from(partial_first_line);
    let lines: Vec<Value> = tail
        .lines()
        .skip(skip)
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter(Value::is_object)
        .collect();
    claude_reply(&lines).or_else(|| codex_reply(&lines))
}

/// The last assistant reply in the `.jsonl` transcript at `path`, read from its last
/// [`TRANSCRIPT_TAIL`] bytes. `None` for any other path or a read failure.
pub fn last_assistant_text(path: &Path) -> Option<String> {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return None;
    }
    let mut f = File::open(path).ok()?;
    let meta = f.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let seeked = meta.len() > TRANSCRIPT_TAIL;
    if seeked {
        f.seek(SeekFrom::Start(meta.len() - TRANSCRIPT_TAIL)).ok()?;
    }
    let mut buf = Vec::new();
    f.take(TRANSCRIPT_TAIL).read_to_end(&mut buf).ok()?;
    last_assistant_text_from_tail(&String::from_utf8_lossy(&buf), seeked)
}

/// The reply to record for a finished turn: the fixed failure message for `--failed`;
/// else the payload's `last_assistant_message`; else the transcript fallback; else
/// empty. Any parse or read failure yields an empty message. Already truncated.
pub fn capture_message(failed: bool, payload: &[u8]) -> String {
    if failed {
        return FAILED_MESSAGE.to_string();
    }
    let Ok(v) = serde_json::from_slice::<Value>(payload) else {
        return String::new();
    };
    if let Some(m) = v.get("last_assistant_message").and_then(Value::as_str) {
        if !m.is_empty() {
            return truncate_reply(m);
        }
    }
    let from_transcript = v
        .get("transcript_path")
        .and_then(Value::as_str)
        .and_then(|p| last_assistant_text(Path::new(p)));
    truncate_reply(&from_transcript.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn claude_line(kind: &str, content: Value) -> String {
        json!({"type": kind, "uuid": "u", "message": {"role": kind, "content": content}})
            .to_string()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!(
            "keitora-stop-capture-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn claude_transcript() -> String {
        [
            claude_line("user", json!("do the thing")),
            claude_line("assistant", json!([{"type": "text", "text": "old reply"}])),
            claude_line("user", json!([{"type": "tool_result", "content": "ok"}])),
            claude_line(
                "assistant",
                json!([{"type": "thinking", "thinking": "hmm"}, {"type": "text", "text": "Part one."}]),
            ),
            claude_line("assistant", json!([{"type": "tool_use", "name": "Bash", "input": {}}])),
            "{not json".to_string(),
            json!({"type": "system", "subtype": "x"}).to_string(),
            claude_line("assistant", json!([{"type": "text", "text": "Part two."}])),
        ]
        .join("\n")
    }

    fn codex_transcript(with_item: bool) -> String {
        let mut lines = vec![
            json!({"type": "session_meta", "payload": {"id": "x"}}).to_string(),
            json!({"type": "response_item", "payload": {"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": "hi"}]}})
            .to_string(),
            json!({"type": "event_msg", "payload": {"type": "agent_message", "message": "event reply"}})
                .to_string(),
        ];
        if with_item {
            lines.push(
                json!({"type": "response_item", "payload": {"type": "message", "role": "assistant",
                       "content": [{"type": "output_text", "text": "Done."},
                                   {"type": "output_text", "text": "All green."}]}})
                .to_string(),
            );
        }
        lines.push(json!({"type": "event_msg", "payload": {"type": "token_count"}}).to_string());
        lines.join("\n")
    }

    #[test]
    fn payload_message_wins() {
        let p = json!({"last_assistant_message": "from payload", "transcript_path": "/nope.jsonl"});
        assert_eq!(
            capture_message(false, p.to_string().as_bytes()),
            "from payload"
        );
    }

    #[test]
    fn failed_uses_the_fixed_message() {
        assert_eq!(capture_message(true, b""), FAILED_MESSAGE);
        assert_eq!(
            capture_message(true, b"{\"last_assistant_message\":\"x\"}"),
            FAILED_MESSAGE
        );
    }

    #[test]
    fn empty_or_invalid_payload_gives_empty_message() {
        assert_eq!(capture_message(false, b""), "");
        assert_eq!(capture_message(false, b"garbage"), "");
        assert_eq!(capture_message(false, b"{}"), "");
        assert_eq!(
            capture_message(false, br#"{"last_assistant_message":""}"#),
            ""
        );
        assert_eq!(
            capture_message(false, br#"{"transcript_path":"/missing/x.jsonl"}"#),
            ""
        );
    }

    #[test]
    fn claude_trailing_run_joins_text_blocks_and_skips_tool_use() {
        assert_eq!(
            last_assistant_text_from_tail(&claude_transcript(), false).as_deref(),
            Some("Part one.\nPart two.")
        );
        // A trailing user line ends the run with nothing.
        let t = format!(
            "{}\n{}",
            claude_transcript(),
            claude_line("user", json!("next"))
        );
        assert_eq!(last_assistant_text_from_tail(&t, false), None);
    }

    #[test]
    fn partial_first_line_is_dropped() {
        let t = format!(
            "{}\n{}",
            claude_line("assistant", json!([{"type": "text", "text": "cut"}])),
            claude_line("user", json!("x"))
        );
        // Without the user line (dropped as partial) the assistant run would end at EOF.
        let tail = format!(
            "{t}\n{}",
            claude_line("assistant", json!([{"type": "text", "text": "kept"}]))
        );
        assert_eq!(
            last_assistant_text_from_tail(&tail, true).as_deref(),
            Some("kept")
        );
        let only = claude_line("assistant", json!([{"type": "text", "text": "cut"}]));
        assert_eq!(last_assistant_text_from_tail(&only, true), None);
        assert_eq!(
            last_assistant_text_from_tail(&only, false).as_deref(),
            Some("cut")
        );
    }

    #[test]
    fn codex_output_text_then_agent_message_fallback() {
        assert_eq!(
            last_assistant_text_from_tail(&codex_transcript(true), false).as_deref(),
            Some("Done.\nAll green.")
        );
        assert_eq!(
            last_assistant_text_from_tail(&codex_transcript(false), false).as_deref(),
            Some("event reply")
        );
    }

    #[test]
    fn transcript_fallback_reads_jsonl_files_only() {
        let d = temp_dir("fallback");
        let jsonl = d.join("s1.jsonl");
        std::fs::write(&jsonl, claude_transcript()).unwrap();
        let p = json!({"transcript_path": jsonl.to_string_lossy()});
        assert_eq!(
            capture_message(false, p.to_string().as_bytes()),
            "Part one.\nPart two."
        );

        let txt = d.join("s1.txt");
        std::fs::write(&txt, claude_transcript()).unwrap();
        assert_eq!(last_assistant_text(&txt), None);
        let p = json!({"transcript_path": txt.to_string_lossy()});
        assert_eq!(capture_message(false, p.to_string().as_bytes()), "");
        // A directory named like a transcript is not a regular file.
        let dir = d.join("dir.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(last_assistant_text(&dir), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn large_transcript_reads_only_the_tail() {
        let d = temp_dir("tail");
        let path = d.join("rollout-1.jsonl");
        let filler = claude_line(
            "assistant",
            json!([{"type": "text", "text": "x".repeat(1000)}]),
        );
        let mut body = String::new();
        while body.len() < (TRANSCRIPT_TAIL as usize) * 2 {
            body.push_str(&claude_line("user", json!("u")));
            body.push('\n');
            body.push_str(&filler);
            body.push('\n');
        }
        body.push_str(&claude_line("user", json!("last")));
        body.push('\n');
        body.push_str(&claude_line(
            "assistant",
            json!([{"type": "text", "text": "final"}]),
        ));
        std::fs::write(&path, body).unwrap();
        assert_eq!(last_assistant_text(&path).as_deref(), Some("final"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn reply_truncates_on_a_char_boundary_with_marker() {
        assert_eq!(truncate_reply("short"), "short");
        let exact = "a".repeat(REPLY_MAX);
        assert_eq!(truncate_reply(&exact), exact);
        let long = "日".repeat(20_000); // 60000 bytes
        let out = truncate_reply(&long);
        assert!(out.len() <= REPLY_MAX);
        assert!(out.ends_with("\n[truncated]"));
        assert!(out
            .trim_end_matches("\n[truncated]")
            .chars()
            .all(|c| c == '日'));
        let p = json!({ "last_assistant_message": long });
        assert_eq!(capture_message(false, p.to_string().as_bytes()), out);
    }
}
