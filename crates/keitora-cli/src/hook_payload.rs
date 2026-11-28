//! Interpretation of the JSON payload an agent hook passes on stdin, for
//! `keitora session <state> --stdin-payload`.
//!
//! The Antigravity CLI runs its hooks from an unspecified working directory, so
//! the session command takes the worktree from the payload (`cwd`, else the first
//! of `workspacePaths`) instead of the process cwd. Everything here is pure except
//! [`read_capped`], which only reads the given reader.

use std::io::Read;
use std::path::PathBuf;

use serde_json::Value;

/// Upper bound on the payload read from stdin; anything longer is ignored.
pub const MAX_PAYLOAD_BYTES: usize = 1_048_576;

/// What the session command should do with a hook payload.
#[derive(Debug, PartialEq)]
pub enum PayloadAction {
    /// Do nothing and exit successfully, for this reason.
    Skip(PayloadSkip),
    /// Record the state for the worktree containing `cwd`.
    Run { cwd: PathBuf },
}

/// Why a `--stdin-payload` hook did nothing. Each is logged at `WARN`.
#[derive(Debug, Clone, PartialEq)]
pub enum PayloadSkip {
    /// Reading stdin failed.
    Unreadable(String),
    /// The payload is longer than the cap.
    Oversize,
    /// The payload is not a JSON object.
    NotAnObject,
    /// `--if-fully-idle` and the payload's `fullyIdle` is not `true`.
    NotFullyIdle,
    /// Neither a non-empty `cwd` nor a non-empty `workspacePaths[0]`.
    NoDirectory,
    /// The payload's directory is not a directory here (or cannot be entered).
    NotADirectory(PathBuf),
}

impl PayloadSkip {
    /// Whether this skip goes to the hook log. A payload that is not fully idle is
    /// the routine case of every non-final Stop, so it is not logged.
    pub fn logged(&self) -> bool {
        !matches!(self, PayloadSkip::NotFullyIdle)
    }

    /// The log message for this skip.
    pub fn message(&self) -> String {
        match self {
            PayloadSkip::Unreadable(e) => format!("hook payload unreadable: {e}"),
            PayloadSkip::Oversize => {
                format!("hook payload over {MAX_PAYLOAD_BYTES} bytes; ignored")
            }
            PayloadSkip::NotAnObject => "hook payload is not a JSON object".to_string(),
            PayloadSkip::NotFullyIdle => {
                "event filtered: the payload is not fully idle".to_string()
            }
            PayloadSkip::NoDirectory => "hook payload names no cwd or workspace".to_string(),
            PayloadSkip::NotADirectory(p) => {
                format!("payload directory {} is not a directory", p.display())
            }
        }
    }
}

/// Read at most `cap` bytes from `r`. `Ok(None)` when the input is longer than
/// `cap` (the caller skips it as [`PayloadSkip::Oversize`]).
pub fn read_capped(r: &mut impl Read, cap: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    // One byte past the cap tells an exactly-`cap` input from an oversize one.
    r.take(cap as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > cap {
        return Ok(None);
    }
    Ok(Some(buf))
}

/// The raw payload from a [`read_capped`] result, or why it is skipped.
pub fn read_outcome(read: std::io::Result<Option<Vec<u8>>>) -> Result<Vec<u8>, PayloadSkip> {
    match read {
        Ok(Some(raw)) => Ok(raw),
        Ok(None) => Err(PayloadSkip::Oversize),
        Err(e) => Err(PayloadSkip::Unreadable(e.to_string())),
    }
}

/// Pure interpretation of the hook payload. Skips when the payload is not a JSON
/// object, when `if_fully_idle` is set and `fullyIdle` is not the boolean `true`,
/// or when neither a non-empty `cwd` nor a non-empty `workspacePaths[0]` is given.
pub fn interpret_payload(raw: &[u8], if_fully_idle: bool) -> PayloadAction {
    let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(raw) else {
        return PayloadAction::Skip(PayloadSkip::NotAnObject);
    };
    if if_fully_idle && obj.get("fullyIdle") != Some(&Value::Bool(true)) {
        return PayloadAction::Skip(PayloadSkip::NotFullyIdle);
    }
    let non_empty = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    };
    let dir = non_empty(obj.get("cwd")).or_else(|| {
        non_empty(
            obj.get("workspacePaths")
                .and_then(Value::as_array)
                .and_then(|a| a.first()),
        )
    });
    match dir {
        Some(cwd) => PayloadAction::Run { cwd },
        None => PayloadAction::Skip(PayloadSkip::NoDirectory),
    }
}

/// Longest agent session id taken from a payload; anything longer is ignored.
pub const SESSION_ID_MAX: usize = 256;

/// The agent session id in a Claude or Codex hook payload (`session_id`, a
/// non-empty string of at most [`SESSION_ID_MAX`] bytes), else `None`.
pub fn payload_session_id(raw: &[u8]) -> Option<String> {
    capped_str_field(raw, "session_id", SESSION_ID_MAX, false)
}

/// The JSON payload's top-level `key` as text: a non-empty string of at most `max`
/// bytes, or (with `numbers`) a number rendered as text. Anything else (missing,
/// `null`, empty, too long, another type, unparsable) is `None`.
pub fn capped_str_field(raw: &[u8], key: &str, max: usize, numbers: bool) -> Option<String> {
    let v: Value = serde_json::from_slice(raw).ok()?;
    let text = match v.get(key)? {
        Value::String(s) => s.clone(),
        Value::Number(n) if numbers => n.to_string(),
        _ => return None,
    };
    (!text.is_empty() && text.len() <= max).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skip(why: PayloadSkip) -> PayloadAction {
        PayloadAction::Skip(why)
    }

    fn run(p: &str) -> PayloadAction {
        PayloadAction::Run {
            cwd: PathBuf::from(p),
        }
    }

    #[test]
    fn interpret_prefers_cwd() {
        assert_eq!(
            interpret_payload(br#"{"cwd":"/a","workspacePaths":["/b"]}"#, false),
            run("/a")
        );
    }

    #[test]
    fn interpret_falls_back_to_first_workspace_path() {
        assert_eq!(
            interpret_payload(br#"{"workspacePaths":["/b","/c"]}"#, false),
            run("/b")
        );
        assert_eq!(
            interpret_payload(br#"{"cwd":"","workspacePaths":["/b"]}"#, false),
            run("/b"),
            "an empty cwd falls through"
        );
    }

    #[test]
    fn interpret_skips_invalid_json() {
        assert_eq!(
            interpret_payload(b"nope", false),
            skip(PayloadSkip::NotAnObject)
        );
        assert_eq!(
            interpret_payload(b"[]", false),
            skip(PayloadSkip::NotAnObject)
        );
    }

    #[test]
    fn interpret_skips_when_no_directory() {
        assert_eq!(
            interpret_payload(b"{}", false),
            skip(PayloadSkip::NoDirectory)
        );
        assert_eq!(
            interpret_payload(br#"{"cwd":""}"#, false),
            skip(PayloadSkip::NoDirectory)
        );
        assert_eq!(
            interpret_payload(br#"{"cwd":1,"workspacePaths":[""]}"#, false),
            skip(PayloadSkip::NoDirectory)
        );
    }

    #[test]
    fn interpret_if_fully_idle_requires_true() {
        assert_eq!(
            interpret_payload(br#"{"cwd":"/a","fullyIdle":true}"#, true),
            run("/a")
        );
        for raw in [
            br#"{"cwd":"/a","fullyIdle":false}"#.as_slice(),
            br#"{"cwd":"/a"}"#.as_slice(),
            br#"{"cwd":"/a","fullyIdle":"true"}"#.as_slice(),
        ] {
            assert_eq!(
                interpret_payload(raw, true),
                skip(PayloadSkip::NotFullyIdle)
            );
        }
    }

    #[test]
    fn interpret_without_the_flag_ignores_fully_idle() {
        assert_eq!(
            interpret_payload(br#"{"cwd":"/a","fullyIdle":false}"#, false),
            run("/a")
        );
    }

    #[test]
    fn read_capped_rejects_oversize() {
        let cap = 16;
        let over = vec![b'x'; cap + 1];
        assert_eq!(read_capped(&mut over.as_slice(), cap).unwrap(), None);
        let exact = vec![b'x'; cap];
        assert_eq!(
            read_capped(&mut exact.as_slice(), cap).unwrap(),
            Some(exact.clone())
        );
    }

    #[test]
    fn silent_payload_skips_are_classified() {
        let oversize = read_outcome(read_capped(&mut vec![b'x'; 17].as_slice(), 16));
        let unreadable = read_outcome(Err(std::io::Error::other("broken pipe")));
        let skips = [
            unreadable.unwrap_err(),
            oversize.unwrap_err(),
            PayloadSkip::NotAnObject,
            PayloadSkip::NotFullyIdle,
            PayloadSkip::NoDirectory,
            PayloadSkip::NotADirectory(PathBuf::from("/Users/me/gone")),
        ];
        let messages: Vec<String> = skips.iter().map(PayloadSkip::message).collect();
        let distinct: std::collections::HashSet<&String> = messages.iter().collect();
        assert_eq!(distinct.len(), skips.len(), "{messages:?}");
        assert!(messages[0].contains("broken pipe"), "{}", messages[0]);
        assert!(messages[5].contains("/Users/me/gone"), "{}", messages[5]);
        assert_eq!(read_outcome(Ok(Some(b"{}".to_vec()))), Ok(b"{}".to_vec()));
    }

    #[test]
    fn a_routine_not_fully_idle_skip_is_not_logged() {
        // Every non-final agy Stop is filtered this way; logging it buries real skips.
        assert!(!PayloadSkip::NotFullyIdle.logged());
        for skip in [
            PayloadSkip::Unreadable("broken pipe".into()),
            PayloadSkip::Oversize,
            PayloadSkip::NotAnObject,
            PayloadSkip::NoDirectory,
            PayloadSkip::NotADirectory(PathBuf::from("/Users/me/gone")),
        ] {
            assert!(skip.logged(), "{skip:?}");
        }
    }

    #[test]
    fn payload_session_extracts_session_id() {
        let raw = br#"{"session_id":"s-1","cwd":"/Users/me/proj","hook_event_name":"Stop"}"#;
        assert_eq!(payload_session_id(raw).as_deref(), Some("s-1"));
        let long = "x".repeat(SESSION_ID_MAX);
        let raw = serde_json::json!({ "session_id": long }).to_string();
        assert_eq!(payload_session_id(raw.as_bytes()), Some(long));
    }

    #[test]
    fn payload_session_absent_is_none() {
        for raw in [
            b"".as_slice(),
            b"garbage",
            b"null",
            b"[]",
            br#"{}"#,
            br#"{"session_id":""}"#,
            br#"{"session_id":7}"#,
            br#"{"session_id":null}"#,
            br#"{"sessionId":"s-1"}"#,
        ] {
            assert_eq!(
                payload_session_id(raw),
                None,
                "{}",
                String::from_utf8_lossy(raw)
            );
        }
        let too_long = serde_json::json!({ "session_id": "x".repeat(SESSION_ID_MAX + 1) });
        assert_eq!(payload_session_id(too_long.to_string().as_bytes()), None);
    }
}
