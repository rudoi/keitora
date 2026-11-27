//! The guard every Keitora writer into the user's real home directory (`~/.gemini`)
//! goes through. Writers take the guard as an explicit `allowed` argument in their
//! path-injectable form, so tests can exercise both outcomes against a temp dir;
//! only the real-home wrappers call [`allowed`].

use std::path::Path;

use serde_json::Value;

use crate::backend::herdr::supervisor::{integration_enabled, NO_INTEGRATION_ENV};

/// Whether Keitora may write the user's real home (`~/.gemini`): never under
/// `cfg!(test)`, never when `KEITORA_HERDR_NO_INTEGRATION` is set to anything.
/// Mirrors the herdr integration-install opt-out in the supervisor.
pub fn allowed_with(is_test: bool, env_val: Option<String>) -> bool {
    !is_test && integration_enabled(env_val)
}

/// [`allowed_with`] for this process: the build's `cfg!(test)` and the live env.
pub fn allowed() -> bool {
    allowed_with(cfg!(test), std::env::var(NO_INTEGRATION_ENV).ok())
}

/// Read and parse a JSON file another tool owns. `Some(None)`: the file does not
/// exist. `Some(Some(v))`: parsed. `None`: unreadable or not valid JSON, so the
/// caller must skip it rather than clobber it (logged with `label`).
pub fn read_json_or_skip(file: &Path, label: &str) -> Option<Option<Value>> {
    match std::fs::read_to_string(file) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => Some(Some(v)),
            Err(e) => {
                log::warn!(
                    "{label}: {} is not valid JSON ({e}); leaving it alone",
                    file.display()
                );
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(e) => {
            log::warn!(
                "{label}: cannot read {} ({e}); leaving it alone",
                file.display()
            );
            None
        }
    }
}

/// Write `value` to `file` as pretty JSON with a trailing newline, creating the
/// parent directory; atomic, so a concurrent reader never sees a partial file.
pub fn write_json_atomic(file: &Path, value: &Value) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    text.push('\n');
    keitora_tasks::write_atomic(file, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_json_or_skip_distinguishes_missing_parsed_and_bad() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.json");
        assert_eq!(read_json_or_skip(&file, "t"), Some(None));
        std::fs::write(&file, r#"{"a":1}"#).unwrap();
        assert_eq!(
            read_json_or_skip(&file, "t"),
            Some(Some(serde_json::json!({ "a": 1 })))
        );
        std::fs::write(&file, "not json").unwrap();
        assert_eq!(read_json_or_skip(&file, "t"), None);
    }

    #[test]
    fn write_json_atomic_creates_parent_and_ends_with_newline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a").join("b.json");
        write_json_atomic(&file, &serde_json::json!({ "k": [1] })).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, "{\n  \"k\": [\n    1\n  ]\n}\n");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("a"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "no temp file left behind");
    }

    #[test]
    fn allowed_with_refuses_under_test() {
        assert!(!allowed_with(true, None));
        assert!(!allowed_with(true, Some("1".into())));
    }

    #[test]
    fn allowed_with_refuses_when_marker_set() {
        assert!(!allowed_with(false, Some("1".into())));
        assert!(
            !allowed_with(false, Some(String::new())),
            "any value opts out"
        );
    }

    #[test]
    fn allowed_with_permits_otherwise() {
        assert!(allowed_with(false, None));
    }

    #[test]
    fn allowed_is_false_in_this_test_binary() {
        assert!(!allowed());
    }
}
