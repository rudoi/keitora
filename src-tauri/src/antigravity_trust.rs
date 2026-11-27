//! Workspace trust seeding for the Antigravity CLI (`agy`): keeps a worktree root
//! in the `trustedWorkspaces` array of `~/.gemini/antigravity-cli/settings.json`,
//! so the first-run trust prompt of `agy` never blocks a launch.
//!
//! The merge is pure and the writer takes its target path and the write guard as
//! arguments; only [`install_trust_for`] resolves the real home and consults
//! [`crate::global_writes::allowed`]. The file is never clobbered: anything that
//! does not look like the expected shape is left alone.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The settings key holding the trusted workspace roots.
const TRUSTED_KEY: &str = "trustedWorkspaces";

/// Result of the pure merge.
#[derive(Debug, PartialEq)]
pub enum TrustMerge {
    Unchanged,
    Updated(Value),
    Skip,
}

/// The path string stored in the file: the extended-length prefix stripped and,
/// on Windows, native separators.
fn entry_for(worktree: &str, windows: bool) -> String {
    let s = keitora_tasks::strip_extended_prefix_str(worktree);
    if windows {
        s.replace('/', "\\")
    } else {
        s
    }
}

/// Comparison key: on Windows ASCII case and `/` vs `\` are not significant.
/// Trailing separators are dropped (a lone root is kept). `windows` is a
/// parameter rather than `cfg!(windows)` so both forms are testable on any host,
/// which is why the herdr keymap's host-bound path normaliser is not reused.
fn compare_key(path: &str, windows: bool) -> String {
    let s = keitora_tasks::strip_extended_prefix_str(path);
    let mut s = if windows {
        s.replace('\\', "/").to_ascii_lowercase()
    } else {
        s
    };
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
}

/// `existing`: the parsed file (`None` when the file does not exist).
/// `worktree`: the worktree root as a string. `windows`: compare ignoring ASCII
/// case and treating `/` and `\` as equal.
pub fn merge_trusted_workspace(
    existing: Option<Value>,
    worktree: &str,
    windows: bool,
) -> TrustMerge {
    let entry = Value::String(entry_for(worktree, windows));
    let Some(mut doc) = existing else {
        return TrustMerge::Updated(serde_json::json!({ TRUSTED_KEY: [entry] }));
    };
    let Some(obj) = doc.as_object_mut() else {
        return TrustMerge::Skip;
    };
    let Some(list) = obj.get_mut(TRUSTED_KEY) else {
        obj.insert(TRUSTED_KEY.to_string(), Value::Array(vec![entry]));
        return TrustMerge::Updated(doc);
    };
    let Some(items) = list.as_array_mut() else {
        return TrustMerge::Skip;
    };
    if items.iter().any(|v| !v.is_string()) {
        return TrustMerge::Skip;
    }
    let want = compare_key(worktree, windows);
    if items
        .iter()
        .filter_map(Value::as_str)
        .any(|s| compare_key(s, windows) == want)
    {
        return TrustMerge::Unchanged;
    }
    items.push(entry);
    TrustMerge::Updated(doc)
}

/// `<home>/.gemini/antigravity-cli/settings.json`.
pub fn settings_file(home: &Path) -> PathBuf {
    home.join(".gemini")
        .join("antigravity-cli")
        .join("settings.json")
}

/// [`merge_trusted_workspace`] for several worktrees at once: `Updated` when any
/// was added, `Skip` when the file has an unexpected shape.
pub fn merge_trusted_workspaces(
    existing: Option<Value>,
    worktrees: &[String],
    windows: bool,
) -> TrustMerge {
    let mut doc = existing;
    let mut changed = false;
    for wt in worktrees {
        match merge_trusted_workspace(doc.clone(), wt, windows) {
            TrustMerge::Updated(next) => {
                doc = Some(next);
                changed = true;
            }
            TrustMerge::Unchanged => {}
            TrustMerge::Skip => return TrustMerge::Skip,
        }
    }
    match doc {
        Some(doc) if changed => TrustMerge::Updated(doc),
        _ => TrustMerge::Unchanged,
    }
}

/// Merge `worktree` into `file`; returns whether the file was written. Creates the
/// parent directory and the file when missing. Never writes when the merge says
/// Unchanged or Skip, or when `!allowed`. An existing file that cannot be read or
/// parsed is left alone (`Ok(false)`). The single-worktree form of
/// [`apply_trust_all_at`], kept for the tests.
#[cfg(test)]
pub fn apply_trust_at(
    file: &Path,
    worktree: &Path,
    windows: bool,
    allowed: bool,
) -> std::io::Result<bool> {
    apply_trust_all_at(file, &[worktree.to_path_buf()], windows, allowed)
}

/// [`apply_trust_at`] for several worktrees with one read and at most one write.
pub fn apply_trust_all_at(
    file: &Path,
    worktrees: &[PathBuf],
    windows: bool,
    allowed: bool,
) -> std::io::Result<bool> {
    if !allowed || worktrees.is_empty() {
        return Ok(false);
    }
    let Some(existing) = crate::global_writes::read_json_or_skip(file, "antigravity trust") else {
        return Ok(false);
    };
    let worktrees: Vec<String> = worktrees
        .iter()
        .map(|w| w.to_string_lossy().into_owned())
        .collect();
    let TrustMerge::Updated(doc) = merge_trusted_workspaces(existing, &worktrees, windows) else {
        return Ok(false);
    };
    crate::global_writes::write_json_atomic(file, &doc)?;
    Ok(true)
}

/// Seed trust for one worktree in the real home; a no-op unless
/// [`crate::global_writes::allowed`].
pub fn install_trust_for(worktree: &Path) {
    install_trust_for_all(&[worktree.to_path_buf()]);
}

/// Seed trust for every worktree in `worktrees` in the real home with a single
/// read and write of the settings file; a no-op unless
/// [`crate::global_writes::allowed`].
pub fn install_trust_for_all(worktrees: &[PathBuf]) {
    let allowed = crate::global_writes::allowed();
    if !allowed || worktrees.is_empty() {
        return;
    }
    let Some(home) = dirs::home_dir() else {
        log::warn!("antigravity trust: no home directory; trust not seeded");
        return;
    };
    let file = settings_file(&home);
    match apply_trust_all_at(&file, worktrees, cfg!(windows), allowed) {
        Ok(true) => log::debug!(
            "antigravity trust: seeded {} worktree(s) in {}",
            worktrees.len(),
            file.display()
        ),
        Ok(false) => {}
        Err(e) => log::warn!(
            "antigravity trust: seeding {} worktree(s) in {} failed: {e}",
            worktrees.len(),
            file.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_creates_when_missing() {
        assert_eq!(
            merge_trusted_workspace(None, "/wt", false),
            TrustMerge::Updated(json!({ "trustedWorkspaces": ["/wt"] }))
        );
    }

    #[test]
    fn merge_adds_and_keeps_other_keys() {
        assert_eq!(
            merge_trusted_workspace(Some(json!({ "colorScheme": "dark" })), "/wt", false),
            TrustMerge::Updated(json!({ "colorScheme": "dark", "trustedWorkspaces": ["/wt"] }))
        );
    }

    #[test]
    fn merge_appends_to_an_existing_list() {
        assert_eq!(
            merge_trusted_workspace(Some(json!({ "trustedWorkspaces": ["/a"] })), "/wt", false),
            TrustMerge::Updated(json!({ "trustedWorkspaces": ["/a", "/wt"] }))
        );
    }

    #[test]
    fn merge_is_idempotent() {
        assert_eq!(
            merge_trusted_workspace(
                Some(json!({ "trustedWorkspaces": ["/a", "/wt"] })),
                "/wt",
                false
            ),
            TrustMerge::Unchanged
        );
    }

    #[test]
    fn merge_skips_non_object() {
        assert_eq!(
            merge_trusted_workspace(Some(json!([])), "/wt", false),
            TrustMerge::Skip
        );
    }

    #[test]
    fn merge_skips_non_array_trusted_workspaces() {
        assert_eq!(
            merge_trusted_workspace(Some(json!({ "trustedWorkspaces": "x" })), "/wt", false),
            TrustMerge::Skip
        );
    }

    #[test]
    fn merge_skips_array_with_objects() {
        assert_eq!(
            merge_trusted_workspace(
                Some(json!({ "trustedWorkspaces": [{ "path": "/a" }] })),
                "/wt",
                false
            ),
            TrustMerge::Skip
        );
        assert_eq!(
            merge_trusted_workspace(
                Some(json!({ "trustedWorkspaces": ["/a", 1] })),
                "/wt",
                false
            ),
            TrustMerge::Skip,
            "any non-string element"
        );
    }

    #[test]
    fn merge_compares_case_insensitively_on_windows() {
        let doc = json!({ "trustedWorkspaces": ["C:\\Wt"] });
        assert_eq!(
            merge_trusted_workspace(Some(doc.clone()), "c:/wt", true),
            TrustMerge::Unchanged
        );
        assert_eq!(
            merge_trusted_workspace(Some(doc), "c:/wt", false),
            TrustMerge::Updated(json!({ "trustedWorkspaces": ["C:\\Wt", "c:/wt"] }))
        );
    }

    #[test]
    fn merge_ignores_a_trailing_separator() {
        let doc = json!({ "trustedWorkspaces": ["C:\\wt\\"] });
        assert_eq!(
            merge_trusted_workspace(Some(doc), "C:\\wt", true),
            TrustMerge::Unchanged
        );
        let doc = json!({ "trustedWorkspaces": ["/wt/"] });
        assert_eq!(
            merge_trusted_workspace(Some(doc), "/wt", false),
            TrustMerge::Unchanged
        );
        assert_eq!(compare_key("/", false), "/", "a lone root is kept");
    }

    #[test]
    fn merge_writes_native_separators_on_windows() {
        assert_eq!(
            merge_trusted_workspace(None, "C:/Users/x/proj", true),
            TrustMerge::Updated(json!({ "trustedWorkspaces": ["C:\\Users\\x\\proj"] }))
        );
    }

    #[test]
    fn settings_file_is_under_gemini_antigravity_cli() {
        let home = Path::new("/home/u");
        assert_eq!(
            settings_file(home),
            home.join(".gemini")
                .join("antigravity-cli")
                .join("settings.json")
        );
    }

    #[test]
    fn apply_trust_at_writes_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = settings_file(dir.path());
        let wt = Path::new("/wt");
        assert!(apply_trust_at(&file, wt, false, true).unwrap());
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.ends_with('\n'), "trailing newline");
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc, json!({ "trustedWorkspaces": ["/wt"] }));
        assert!(!apply_trust_at(&file, wt, false, true).unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    }

    #[test]
    fn apply_trust_at_keeps_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, r#"{"colorScheme":"dark"}"#).unwrap();
        assert!(apply_trust_at(&file, Path::new("/wt"), false, true).unwrap());
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            doc,
            json!({ "colorScheme": "dark", "trustedWorkspaces": ["/wt"] })
        );
    }

    #[test]
    fn apply_trust_at_refuses_when_not_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let file = settings_file(dir.path());
        assert!(!apply_trust_at(&file, Path::new("/wt"), false, false).unwrap());
        assert!(!file.exists());
        assert!(!dir.path().join(".gemini").exists(), "no parent dir either");
    }

    #[test]
    fn apply_trust_at_leaves_unparseable_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, "not json").unwrap();
        assert!(!apply_trust_at(&file, Path::new("/wt"), false, true).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), b"not json");
    }

    #[test]
    fn merge_many_adds_each_missing_worktree_once() {
        let wts = ["/a".to_string(), "/b".to_string(), "/a".to_string()];
        assert_eq!(
            merge_trusted_workspaces(Some(json!({ "trustedWorkspaces": ["/b"] })), &wts, false),
            TrustMerge::Updated(json!({ "trustedWorkspaces": ["/b", "/a"] }))
        );
        assert_eq!(
            merge_trusted_workspaces(
                Some(json!({ "trustedWorkspaces": ["/a", "/b"] })),
                &wts,
                false
            ),
            TrustMerge::Unchanged
        );
        assert_eq!(
            merge_trusted_workspaces(Some(json!([])), &wts, false),
            TrustMerge::Skip
        );
        assert_eq!(
            merge_trusted_workspaces(None, &[], false),
            TrustMerge::Unchanged
        );
    }

    #[test]
    fn apply_trust_all_at_writes_every_worktree_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let file = settings_file(dir.path());
        let wts = [PathBuf::from("/a"), PathBuf::from("/b")];
        assert!(apply_trust_all_at(&file, &wts, false, true).unwrap());
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(doc, json!({ "trustedWorkspaces": ["/a", "/b"] }));
        assert!(!apply_trust_all_at(&file, &wts, false, true).unwrap());
        assert!(!apply_trust_all_at(&file, &[], false, true).unwrap());
    }

    #[test]
    fn apply_trust_at_leaves_a_foreign_shape_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let original = r#"{"trustedWorkspaces":[{"path":"/a"}]}"#;
        std::fs::write(&file, original).unwrap();
        assert!(!apply_trust_at(&file, Path::new("/wt"), false, true).unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    }
}
