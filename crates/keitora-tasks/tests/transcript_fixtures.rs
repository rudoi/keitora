//! Turn replies read from transcript fixtures: each `<case>.<kind>.jsonl` under
//! `tests/fixtures/transcripts/<provider>/` against its `<case>.expect.json`
//! (`delivered_at`, the expected `reply` or `null`, and its `outcome`).

use std::path::PathBuf;

use keitora_tasks::transcript::agy::fixture::write_conversation;
use keitora_tasks::transcript::{
    parse_lines, read_turn_reply, reply_for_turn, rfc3339_ms, TurnOutcome, TurnReply, TurnWindow,
};
use keitora_tasks::CliKind;
use serde_json::Value;

fn fixture_dir(provider: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcripts")
        .join(provider)
}

fn provider_of(name: &str) -> CliKind {
    match name {
        "claude" => CliKind::Claude,
        "codex" => CliKind::Codex,
        other => panic!("no provider {other}"),
    }
}

struct Case {
    provider: CliKind,
    text: String,
    path: PathBuf,
    delivered_at: i64,
    expected: Option<TurnReply>,
}

impl Case {
    /// The fixture's delivery, with its turn's end detected after every line.
    fn window(&self) -> TurnWindow {
        TurnWindow::since(self.delivered_at)
    }
}

fn load(provider: &str, case: &str, kind: &str) -> Case {
    let dir = fixture_dir(provider);
    let path = dir.join(format!("{case}.{kind}.jsonl"));
    let text = std::fs::read_to_string(&path).unwrap();
    let expect: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join(format!("{case}.expect.json"))).unwrap(),
    )
    .unwrap();
    let delivered_at = rfc3339_ms(expect["delivered_at"].as_str().unwrap()).unwrap();
    let expected = expect["reply"].as_str().map(|reply| TurnReply {
        text: reply.to_string(),
        outcome: match expect["outcome"].as_str() {
            Some("complete") => TurnOutcome::Complete,
            Some("aborted") => TurnOutcome::Aborted,
            other => panic!("{case}: bad outcome {other:?}"),
        },
    });
    Case {
        provider: provider_of(provider),
        text,
        path,
        delivered_at,
        expected,
    }
}

/// The fixture's reply from its parsed lines and from the file, and never a reply
/// once the delivery is later than every line.
fn check(provider: &str, case: &str, kind: &str) {
    let c = load(provider, case, kind);
    let lines = parse_lines(&c.text, false);
    assert_eq!(
        reply_for_turn(c.provider, &lines, c.window()).ok(),
        c.expected,
        "{provider}/{case}"
    );
    assert_eq!(
        read_turn_reply(c.provider, &c.path, c.window()).ok(),
        c.expected,
        "{provider}/{case} from the file"
    );
    let after_everything = TurnWindow::since(c.delivered_at + 24 * 3600 * 1000);
    assert_eq!(
        reply_for_turn(c.provider, &lines, after_everything).ok(),
        None,
        "{provider}/{case}: a reply from before the delivery"
    );
}

macro_rules! fixture_tests {
    ($($name:ident: $provider:literal, $case:literal, $kind:literal;)*) => {
        $(
            #[test]
            fn $name() {
                check($provider, $case, $kind);
            }
        )*
    };
}

fixture_tests! {
    claude_plain_synthetic: "claude", "plain", "synthetic";
    claude_tool_synthetic: "claude", "tool", "synthetic";
    claude_denied_permission_synthetic: "claude", "denied_permission", "synthetic";
    claude_interrupt_synthetic: "claude", "interrupt", "synthetic";
    claude_clear_synthetic: "claude", "clear", "synthetic";
    claude_compaction_synthetic: "claude", "compaction", "synthetic";
    claude_resumed_synthetic: "claude", "resumed", "synthetic";
    claude_two_turns_synthetic: "claude", "two_turns", "synthetic";
    claude_thinking_text_synthetic: "claude", "thinking_text", "synthetic";
    claude_sidechain_synthetic: "claude", "sidechain", "synthetic";
    claude_meta_after_prompt_synthetic: "claude", "meta_after_prompt", "synthetic";
    codex_plain_synthetic: "codex", "plain", "synthetic";
    codex_tool_synthetic: "codex", "tool", "synthetic";
    codex_denied_permission_synthetic: "codex", "denied_permission", "synthetic";
    codex_interrupt_synthetic: "codex", "interrupt", "synthetic";
    codex_compaction_synthetic: "codex", "compaction", "synthetic";
    codex_resumed_synthetic: "codex", "resumed", "synthetic";
    codex_two_turns_synthetic: "codex", "two_turns", "synthetic";
    codex_no_cwd_synthetic: "codex", "no_cwd", "synthetic";
    codex_no_last_agent_message_synthetic: "codex", "no_last_agent_message", "synthetic";
    codex_merged_prompt_synthetic: "codex", "merged_prompt", "synthetic";
    codex_unfinished_synthetic: "codex", "unfinished", "synthetic";
    claude_single_turn_captured: "claude", "single_turn", "captured";
    claude_back_to_back_captured: "claude", "back_to_back", "captured";
    codex_back_to_back_captured: "codex", "back_to_back", "captured";
}

#[test]
fn repro_b_codex_rollout_fixture_yields_the_reply() {
    let c = load("codex", "back_to_back", "captured");
    let got = read_turn_reply(CliKind::Codex, &c.path, c.window());
    assert_eq!(got.map(|r| r.text), Ok("two".to_string()));
}

#[test]
fn back_to_back_prompts_answer_with_the_second_reply_captured() {
    // Claude: a delivery before the first prompt still gets the latest turn's reply.
    let c = load("claude", "back_to_back", "captured");
    let lines = parse_lines(&c.text, false);
    let first_prompt = rfc3339_ms("2026-01-01T00:00:05.875Z").unwrap();
    assert_eq!(
        reply_for_turn(c.provider, &lines, TurnWindow::since(first_prompt)).map(|r| r.text),
        Ok("two".into())
    );
    // Codex: both prompts share one task; a delivery after its start finds the
    // second prompt and the task's end.
    let c = load("codex", "back_to_back", "captured");
    let lines = parse_lines(&c.text, false);
    let second_prompt = rfc3339_ms("2026-01-01T00:00:02.210Z").unwrap();
    assert_eq!(
        reply_for_turn(c.provider, &lines, TurnWindow::since(second_prompt)).map(|r| r.text),
        Ok("two".into())
    );
}

#[test]
fn the_first_of_two_turns_is_never_returned_for_the_second_delivery() {
    for provider in ["claude", "codex"] {
        let c = load(provider, "two_turns", "synthetic");
        let lines = parse_lines(&c.text, false);
        // Delivered before the first turn: the latest turn since then is the second.
        let early = TurnWindow::since(c.delivered_at - 10_000);
        let early = reply_for_turn(c.provider, &lines, early).unwrap();
        assert_eq!(early.text, "two", "{provider}");
        let r = reply_for_turn(c.provider, &lines, c.window()).unwrap();
        assert_ne!(r.text, "one", "{provider}");
    }
}

/// A copy of `case` in a temp file, transformed by `edit`.
fn temp_copy(
    provider: &str,
    case: &str,
    edit: impl Fn(&str) -> String,
) -> (tempfile::TempDir, Case) {
    let mut c = load(provider, case, "synthetic");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    std::fs::write(&path, edit(&c.text)).unwrap();
    c.path = path;
    (dir, c)
}

#[test]
fn crlf_line_endings_parse_synthetic() {
    for provider in ["claude", "codex"] {
        let (_d, c) = temp_copy(provider, "plain", |t| t.replace('\n', "\r\n"));
        assert_eq!(
            read_turn_reply(c.provider, &c.path, c.window()).ok(),
            c.expected,
            "{provider}"
        );
    }
}

#[test]
fn a_torn_last_line_is_skipped_synthetic() {
    for provider in ["claude", "codex"] {
        // A half-written line after the turn's end changes nothing.
        let (_d, c) = temp_copy(provider, "plain", |t| {
            format!("{t}{{\"timestamp\":\"2026-01-01T00:00:30.000Z\",\"type\":\"assis")
        });
        assert_eq!(
            read_turn_reply(c.provider, &c.path, c.window()).ok(),
            c.expected,
            "{provider}"
        );
    }
    // Codex: the end event itself torn -> the turn has no end yet.
    let (_d, c) = temp_copy("codex", "plain", |t| {
        let t = t.trim_end();
        t[..t.len() - 10].to_string()
    });
    assert_eq!(read_turn_reply(c.provider, &c.path, c.window()).ok(), None);
}

#[test]
fn a_line_over_the_tail_forces_the_longer_read_synthetic() {
    // The prompt, then a tool result bigger than the first tail, then the reply.
    let big = "x".repeat(300 * 1024);
    let claude = [
        r#"{"type":"user","timestamp":"2026-01-01T00:00:10.000Z","message":{"role":"user","content":"read the big file"}}"#.to_string(),
        r#"{"type":"assistant","timestamp":"2026-01-01T00:00:11.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu-1","name":"Read","input":{}}]}}"#.to_string(),
        format!(r#"{{"type":"user","timestamp":"2026-01-01T00:00:12.000Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"tu-1","content":"{big}"}}]}}}}"#),
        r#"{"type":"assistant","timestamp":"2026-01-01T00:00:13.000Z","message":{"role":"assistant","content":[{"type":"text","text":"It is big."}]}}"#.to_string(),
    ]
    .join("\n");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.jsonl");
    std::fs::write(&path, claude).unwrap();
    let at = rfc3339_ms("2026-01-01T00:00:09.500Z").unwrap();
    assert_eq!(
        read_turn_reply(CliKind::Claude, &path, TurnWindow::since(at)).map(|r| r.text),
        Ok("It is big.".to_string())
    );
}

// ── Antigravity: a captured conversation's `steps` rows (blobs as hex) ─────────

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// The captured agy case's steps written into a conversation file under `dir`, and
/// its expected delivery and reply.
fn agy_case(dir: &std::path::Path, case: &str) -> (PathBuf, i64, Option<TurnReply>) {
    let fixtures = fixture_dir("agy");
    let rows: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures.join(format!("{case}.captured.json"))).unwrap(),
    )
    .unwrap();
    let path = dir.join(format!("{case}.db"));
    let rows: Vec<_> = rows["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["idx"].as_i64().unwrap(),
                s["step_type"].as_i64().unwrap(),
                s["status"].as_i64().unwrap(),
                unhex(s["metadata"].as_str().unwrap()),
                unhex(s["step_payload"].as_str().unwrap()),
            )
        })
        .collect();
    write_conversation(&path, &rows).unwrap();
    let expect: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures.join(format!("{case}.expect.json"))).unwrap(),
    )
    .unwrap();
    let delivered_at = rfc3339_ms(expect["delivered_at"].as_str().unwrap()).unwrap();
    let reply = expect["reply"].as_str().map(|t| TurnReply {
        text: t.to_string(),
        outcome: TurnOutcome::Complete,
    });
    (path, delivered_at, reply)
}

const AGY_CAPTURED: &[&str] = &["three_turns"];

#[test]
fn agy_reply_captured() {
    let dir = tempfile::tempdir().unwrap();
    for case in AGY_CAPTURED {
        let (path, delivered_at, expected) = agy_case(dir.path(), case);
        assert_eq!(
            read_turn_reply(CliKind::Antigravity, &path, TurnWindow::since(delivered_at)).ok(),
            expected,
            "{case}"
        );
    }
}

#[test]
fn agy_turn_selection_ignores_lines_before_delivered_at() {
    use keitora_tasks::transcript::agy::{agy_reply_for_turn, read_agy_steps};
    let dir = tempfile::tempdir().unwrap();
    let (path, _, _) = agy_case(dir.path(), "three_turns");
    let steps = read_agy_steps(&path).unwrap();
    let at = |s: &str| rfc3339_ms(s).unwrap();
    let reply = |t: &str| {
        agy_reply_for_turn(&steps, TurnWindow::since(at(t)))
            .ok()
            .map(|r| r.text)
    };
    // Delivered before the second prompt: the latest turn since then is the third.
    assert_eq!(
        reply("2026-01-01T00:00:02.000Z").as_deref(),
        Some("the tool ran")
    );
    // Delivered after the last input: nothing in the file answers it.
    assert_eq!(reply("2026-01-01T00:00:13.000Z"), None);
    // The skew still admits an input logged just before the delivery.
    assert_eq!(
        reply("2026-01-01T00:00:12.900Z").as_deref(),
        Some("the tool ran")
    );
    // Cut the file before the third turn's final response: its last text is the
    // in-progress line, never the second turn's reply.
    let cut = &steps[..steps.len() - 1];
    assert_eq!(
        agy_reply_for_turn(cut, TurnWindow::since(at("2026-01-01T00:00:10.500Z")))
            .ok()
            .map(|r| r.text)
            .as_deref(),
        Some("running the tool")
    );
}
