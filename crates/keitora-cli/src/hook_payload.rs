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
    /// Do nothing and exit successfully.
    Skip,
    /// Record the state for the worktree containing `cwd`.
    Run { cwd: PathBuf },
}

/// Read at most `cap` bytes from `r`. `Ok(None)` when the input is longer than
/// `cap` (the caller treats that as [`PayloadAction::Skip`]).
pub fn read_capped(r: &mut impl Read, cap: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    // One byte past the cap tells an exactly-`cap` input from an oversize one.
    r.take(cap as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > cap {
        return Ok(None);
    }
    Ok(Some(buf))
}

/// Pure interpretation of the hook payload. Skips when the payload is not a JSON
/// object, when `if_fully_idle` is set and `fullyIdle` is not the boolean `true`,
/// or when neither a non-empty `cwd` nor a non-empty `workspacePaths[0]` is given.
pub fn interpret_payload(raw: &[u8], if_fully_idle: bool) -> PayloadAction {
    let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(raw) else {
        return PayloadAction::Skip;
    };
    if if_fully_idle && obj.get("fullyIdle") != Some(&Value::Bool(true)) {
        return PayloadAction::Skip;
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
        None => PayloadAction::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(interpret_payload(b"nope", false), PayloadAction::Skip);
        assert_eq!(interpret_payload(b"[]", false), PayloadAction::Skip);
    }

    #[test]
    fn interpret_skips_when_no_directory() {
        assert_eq!(interpret_payload(b"{}", false), PayloadAction::Skip);
        assert_eq!(
            interpret_payload(br#"{"cwd":""}"#, false),
            PayloadAction::Skip
        );
        assert_eq!(
            interpret_payload(br#"{"cwd":1,"workspacePaths":[""]}"#, false),
            PayloadAction::Skip
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
            assert_eq!(interpret_payload(raw, true), PayloadAction::Skip);
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
}
