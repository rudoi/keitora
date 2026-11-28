//! Stop-hook reply capture (`keitora session idle --stdin` / `--failed`): the turn's
//! last assistant message, from the Stop payload or, failing that, the tail of the
//! session transcript. Pure over strings; only [`last_assistant_text`] reads a file.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

#[cfg(test)]
use keitora_tasks::transcript::REPLY_MAX;
pub use keitora_tasks::transcript::{
    last_assistant_text_from_tail, truncate_reply, TRANSCRIPT_TAIL,
};

/// Most bytes of Stop payload read from stdin.
pub const STOP_STDIN_CAP: u64 = 64 * 1024;
/// How long the Stop capture waits for stdin.
pub const STOP_STDIN_TIMEOUT: Duration = Duration::from_secs(1);
/// The reply recorded for a `StopFailure`.
pub const FAILED_MESSAGE: &str = "turn ended with an error";

/// The last assistant reply in the `.jsonl` transcript at `path`, read from its last
/// [`TRANSCRIPT_TAIL`] bytes. `None` for any other path or a read failure.
pub fn last_assistant_text(path: &Path) -> Option<String> {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return None;
    }
    let tail = keitora_tasks::transcript::read_tail(path, TRANSCRIPT_TAIL).ok()?;
    last_assistant_text_from_tail(&tail.text, tail.truncated_front)
}

/// Longest `turn_id` kept from a Stop payload.
pub const TURN_ID_MAX: usize = 128;

/// The Stop payload's `turn_id` (Codex sends one; Claude does not): a non-empty
/// string of at most [`TURN_ID_MAX`] bytes, or a number rendered as text. Anything
/// else (missing, `null`, empty, too long, another type, unparsable) is `None`.
pub fn payload_turn_id(payload: &[u8]) -> Option<String> {
    crate::hook_payload::capped_str_field(payload, "turn_id", TURN_ID_MAX, true)
}

/// The reply to record for a finished turn: the fixed failure message for `--failed`;
/// else the payload's `last_assistant_message` when it is a non-empty string (Codex
/// sends `null` for a turn with no final message); else the transcript fallback; else
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

    /// The documented Codex Stop payload (placeholder ids and paths).
    fn codex_stop(message: Value, transcript: &str) -> Vec<u8> {
        json!({
            "session_id": "s-1",
            "turn_id": "t-1",
            "transcript_path": transcript,
            "cwd": "/Users/me/proj",
            "hook_event_name": "Stop",
            "model": "gpt-x",
            "stop_hook_active": false,
            "last_assistant_message": message,
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn codex_stop_payload_string_message_is_captured() {
        let d = temp_dir("codex-string");
        let rollout = d.join("rollout-s-1.jsonl");
        std::fs::write(&rollout, codex_transcript(true)).unwrap();
        // The payload's string wins over a readable transcript...
        let p = codex_stop(json!("All tests pass."), &rollout.to_string_lossy());
        assert_eq!(capture_message(false, &p), "All tests pass.");
        // ...and needs none.
        let p = codex_stop(json!("All tests pass."), "/Users/me/.codex/missing.jsonl");
        assert_eq!(capture_message(false, &p), "All tests pass.");
        assert_eq!(payload_turn_id(&p).as_deref(), Some("t-1"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn codex_stop_payload_null_message_falls_back_to_the_transcript() {
        let d = temp_dir("codex-null");
        let rollout = d.join("rollout-s-1.jsonl");
        std::fs::write(&rollout, codex_transcript(true)).unwrap();
        let path = rollout.to_string_lossy();
        for message in [Value::Null, json!(""), json!(7), json!({"text": "x"})] {
            let p = codex_stop(message.clone(), &path);
            assert_eq!(capture_message(false, &p), "Done.\nAll green.", "{message}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn codex_stop_payload_null_message_without_transcript_is_empty() {
        let p = codex_stop(Value::Null, "/Users/me/.codex/sessions/missing.jsonl");
        assert_eq!(capture_message(false, &p), "");
        let mut v: Value = serde_json::from_slice(&p).unwrap();
        v.as_object_mut().unwrap().remove("transcript_path");
        assert_eq!(capture_message(false, v.to_string().as_bytes()), "");
    }

    #[test]
    fn payload_turn_id_reads_string_and_number_and_rejects_the_rest() {
        let id = |v: Value| payload_turn_id(json!({ "turn_id": v }).to_string().as_bytes());
        assert_eq!(id(json!("t-1")).as_deref(), Some("t-1"));
        assert_eq!(id(json!(42)).as_deref(), Some("42"));
        assert_eq!(
            id(json!("x".repeat(TURN_ID_MAX))).map(|s| s.len()),
            Some(TURN_ID_MAX)
        );
        for bad in [
            json!(""),
            json!("x".repeat(TURN_ID_MAX + 1)),
            json!({"id": 1}),
            json!(["t-1"]),
            json!(true),
            Value::Null,
        ] {
            assert_eq!(id(bad.clone()), None, "{bad}");
        }
        assert_eq!(payload_turn_id(br#"{"session_id":"s-1"}"#), None, "missing");
        assert_eq!(payload_turn_id(b"garbage"), None);
        assert_eq!(payload_turn_id(b""), None);
        assert_eq!(payload_turn_id(b"null"), None);
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
