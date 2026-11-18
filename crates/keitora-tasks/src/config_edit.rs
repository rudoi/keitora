//! Surgical `toml_edit` setters for every editable settings field.
//!
//! Each function loads the target `keitora.toml`, mutates only the affected key(s)
//! via [`toml_edit`], and writes the file back — preserving comments, untouched
//! keys, and formatting.  The pattern mirrors [`crate::discovery::set_project_worktrees`].
//!
//! **Project setters** operate on `<project_root>/keitora.toml`.
//! **User setters** operate on the per-user registry (`dirs::config_dir()/keitora/keitora.toml`
//! or `$KEITORA_CONFIG_HOME/keitora/keitora.toml`).

use std::path::{Path, PathBuf};

use crate::{Error, Result};

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Load `<file>` text → parse as `DocumentMut`. Errors use `Error::Config`.
fn load_doc(file: &Path) -> Result<toml_edit::DocumentMut> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| Error::Config(format!("reading {}: {e}", file.display())))?;
    text.parse::<toml_edit::DocumentMut>()
        .map_err(|e| Error::Config(format!("parsing {}: {e}", file.display())))
}

/// Write `doc` back to `file` atomically (readers never see a partial file).
fn save_doc(file: &Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    crate::config::write_atomic(file, doc.to_string().as_bytes())
        .map_err(|e| Error::Config(format!("writing {}: {e}", file.display())))
}

/// The user-registry path — the single source of truth shared with
/// `UserConfig::load` (so settings writes land on exactly the file reads use).
/// Maps `config.rs`'s `ResolveError` onto `crate::Error` for this module.
fn user_config_file() -> Result<PathBuf> {
    crate::config::user_config_path().map_err(|e| Error::Config(e.to_string()))
}

/// Reject sub-1 day counts (serde accepts any i64 on load; we add a floor at the
/// write boundary). Returns `Err` *before* the file is opened, so a rejected
/// value never touches disk. `pub` so the registry write path
/// (`host::save_project_settings`) enforces the identical floor.
pub fn check_days(field: &str, days: i64) -> Result<()> {
    if days < 1 {
        return Err(Error::Config(format!("{field} must be >= 1, got {days}")));
    }
    Ok(())
}

/// Normalize a raw `worktrees_dir` form value: trim, and treat empty/whitespace as
/// `None` (fall back to the `~/.keitora/<name>` default). The single source of truth
/// shared by the committed-file setter (`apply_worktrees_dir`) and the registry
/// write path (`host::build_settings_patch`), so the two cannot diverge.
pub fn normalize_worktrees_dir(dir: Option<&str>) -> Option<String> {
    dir.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Normalize a raw `dispatch_steps` form value: trim each step and drop empty ones,
/// preserving order. Shared by `apply_dispatch_steps_key` and `host::build_settings_patch`
///.
pub fn clean_dispatch_steps(steps: &[String]) -> Vec<String> {
    steps
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// Mutation helpers — operate on an in-memory `DocumentMut`, so a single load
// can apply many edits and save once (see `write_project_settings`). Each
// public setter is a thin load → apply → save wrapper around these.
// ---------------------------------------------------------------------------

fn apply_archive_days(doc: &mut toml_edit::DocumentMut, days: i64) {
    doc["tasks"]["archive_days"] = toml_edit::value(days);
}

fn apply_log_retention_days(doc: &mut toml_edit::DocumentMut, days: i64) {
    doc["tasks"]["log_retention_days"] = toml_edit::value(days);
}

/// Set `[project].worktrees_dir`, or remove the key when `None`/empty (falls
/// back to the `~/.keitora/<name>` default). Raw value stored as-is (tilde kept).
fn apply_worktrees_dir(doc: &mut toml_edit::DocumentMut, dir: Option<&str>) {
    match normalize_worktrees_dir(dir) {
        Some(trimmed) => doc["project"]["worktrees_dir"] = toml_edit::value(trimmed),
        None => {
            if let Some(project) = doc.get_mut("project").and_then(|t| t.as_table_mut()) {
                project.remove("worktrees_dir");
            }
        }
    }
}

/// Set `[<table>].path`, or remove the `path` key and collapse the now-empty
/// table. Shared by the per-CLI (`claude`/`codex`) project and user path setters.
fn apply_provider_path(doc: &mut toml_edit::DocumentMut, table: &str, path: Option<&str>) {
    match path.map(str::trim).filter(|s| !s.is_empty()) {
        Some(trimmed) => {
            if !doc.contains_key(table) {
                doc[table] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            doc[table]["path"] = toml_edit::value(trimmed);
        }
        None => {
            let now_empty = if let Some(t) = doc.get_mut(table).and_then(|t| t.as_table_mut()) {
                t.remove("path");
                t.is_empty()
            } else {
                false
            };
            if now_empty {
                doc.remove(table);
            }
        }
    }
}

/// Set `[claude].path`, or remove it and collapse the now-empty `[claude]` table.
fn apply_claude_path(doc: &mut toml_edit::DocumentMut, path: Option<&str>) {
    apply_provider_path(doc, "claude", path);
}

/// Set `[codex].path`, or remove it and collapse the now-empty `[codex]` table.
fn apply_codex_path(doc: &mut toml_edit::DocumentMut, path: Option<&str>) {
    apply_provider_path(doc, "codex", path);
}

/// The legacy committed-file key that serde still reads as an alias of this
/// table's `autonomy` field, if any. Only `[claude]` had one
/// (`skip_permissions`); `[codex]` is new and never had a legacy name.
/// A surgical writer must delete this alias whenever it writes `autonomy`, or the
/// file would carry BOTH keys and `ProjectConfig::load` fails with a
/// `duplicate field` error.
fn provider_autonomy_legacy_alias(table: &str) -> Option<&'static str> {
    match table {
        "claude" => Some("skip_permissions"),
        _ => None,
    }
}

/// Set `[<table>].autonomy`, or remove the key and collapse the now-empty table
/// when `autonomy` is `None`. Shared by the per-CLI autonomy setters; mirrors
/// [`apply_provider_path`]'s create-if-missing / collapse-if-now-empty shape.
fn apply_provider_autonomy(doc: &mut toml_edit::DocumentMut, table: &str, autonomy: Option<bool>) {
    match autonomy {
        Some(v) => {
            if !doc.contains_key(table) {
                doc[table] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            doc[table]["autonomy"] = toml_edit::value(v);
            // Back-compat: drop the legacy `skip_permissions` alias so the table
            // never carries both it AND `autonomy` (serde reads both as the same
            // field → `duplicate field` on load). No-op for the fresh-table case
            // and for `[codex]` (which has no legacy alias).
            if let Some(legacy) = provider_autonomy_legacy_alias(table) {
                if let Some(t) = doc.get_mut(table).and_then(|t| t.as_table_mut()) {
                    t.remove(legacy);
                }
            }
        }
        None => {
            let now_empty = if let Some(t) = doc.get_mut(table).and_then(|t| t.as_table_mut()) {
                t.remove("autonomy");
                t.is_empty()
            } else {
                false
            };
            if now_empty {
                doc.remove(table);
            }
        }
    }
}

/// Set `[claude].autonomy`, or remove it and collapse the now-empty `[claude]` table.
fn apply_claude_autonomy(doc: &mut toml_edit::DocumentMut, autonomy: Option<bool>) {
    apply_provider_autonomy(doc, "claude", autonomy);
}

/// Set `[codex].autonomy`, or remove it and collapse the now-empty `[codex]` table.
fn apply_codex_autonomy(doc: &mut toml_edit::DocumentMut, autonomy: Option<bool>) {
    apply_provider_autonomy(doc, "codex", autonomy);
}

/// Set `[dispatch].<key>` (`claude_steps` / `codex_steps`) to the trimmed,
/// non-empty elements of `steps`. When nothing remains, remove only that key and
/// collapse the `[dispatch]` table ONLY when BOTH `claude_steps` and `codex_steps`
/// are then absent — a table carrying the other CLI's steps (or any sibling key)
/// must survive clearing this one.
fn apply_dispatch_steps_key(doc: &mut toml_edit::DocumentMut, key: &str, steps: &[String]) {
    // The legacy committed-file key serde still reads as an alias of
    // `claude_steps` (`[dispatch] steps`). Only the claude key has one; `codex_steps`
    // is new. Removing it wherever we touch the claude key keeps the table from ever
    // carrying both the alias AND `claude_steps` (which fails `ProjectConfig::load`
    // with a `duplicate field` error).
    let legacy = dispatch_steps_legacy_alias(key);
    let cleaned = clean_dispatch_steps(steps);
    if cleaned.is_empty() {
        let now_empty =
            if let Some(dispatch) = doc.get_mut("dispatch").and_then(|t| t.as_table_mut()) {
                dispatch.remove(key);
                if let Some(legacy) = legacy {
                    dispatch.remove(legacy);
                }
                dispatch.is_empty()
            } else {
                false
            };
        if now_empty {
            doc.remove("dispatch");
        }
    } else {
        let mut arr = toml_edit::Array::new();
        for s in &cleaned {
            arr.push(s.as_str());
        }
        if !doc.contains_key("dispatch") {
            doc["dispatch"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        doc["dispatch"][key] = toml_edit::value(arr);
        // Drop the legacy alias so serde never sees it alongside `claude_steps`.
        if let Some(legacy) = legacy {
            if let Some(t) = doc.get_mut("dispatch").and_then(|t| t.as_table_mut()) {
                t.remove(legacy);
            }
        }
    }
}

/// The legacy committed-file key that serde reads as an alias of this dispatch
/// key, if any. Only `claude_steps` had one (`steps`); `codex_steps` is new.
fn dispatch_steps_legacy_alias(key: &str) -> Option<&'static str> {
    match key {
        "claude_steps" => Some("steps"),
        _ => None,
    }
}

/// Set `[dispatch].claude_steps` (freshly written TOML always uses this key, not
/// the legacy `steps` alias).
fn apply_claude_dispatch_steps(doc: &mut toml_edit::DocumentMut, steps: &[String]) {
    apply_dispatch_steps_key(doc, "claude_steps", steps);
}

/// Set `[dispatch].codex_steps`.
fn apply_codex_dispatch_steps(doc: &mut toml_edit::DocumentMut, steps: &[String]) {
    apply_dispatch_steps_key(doc, "codex_steps", steps);
}

// ---------------------------------------------------------------------------
// Project setters
// ---------------------------------------------------------------------------

/// Write every editable project field in ONE load → apply-all → save cycle, so
/// a save is atomic (no partial-write window) and touches the file once instead
/// of per-field. Days are validated up front, before the file is opened.
#[allow(clippy::too_many_arguments)]
pub fn write_project_settings(
    project_root: &Path,
    archive_days: i64,
    log_retention_days: i64,
    worktrees_dir: Option<String>,
    claude_path: Option<String>,
    codex_path: Option<String>,
    claude_autonomy: Option<bool>,
    codex_autonomy: Option<bool>,
    claude_dispatch_steps: &[String],
    codex_dispatch_steps: &[String],
) -> Result<()> {
    check_days("archive_days", archive_days)?;
    check_days("log_retention_days", log_retention_days)?;
    let file = project_root.join("keitora.toml");
    let mut doc = load_doc(&file)?;
    apply_archive_days(&mut doc, archive_days);
    apply_log_retention_days(&mut doc, log_retention_days);
    apply_worktrees_dir(&mut doc, worktrees_dir.as_deref());
    apply_claude_path(&mut doc, claude_path.as_deref());
    apply_codex_path(&mut doc, codex_path.as_deref());
    apply_claude_autonomy(&mut doc, claude_autonomy);
    apply_codex_autonomy(&mut doc, codex_autonomy);
    apply_claude_dispatch_steps(&mut doc, claude_dispatch_steps);
    apply_codex_dispatch_steps(&mut doc, codex_dispatch_steps);
    save_doc(&file, &doc)
}

/// Set `[tasks].archive_days`. Rejects `days < 1` with `Err` and leaves
/// the file unchanged.
pub fn set_archive_days(project_root: &Path, days: i64) -> Result<()> {
    check_days("archive_days", days)?;
    let file = project_root.join("keitora.toml");
    let mut doc = load_doc(&file)?;
    apply_archive_days(&mut doc, days);
    save_doc(&file, &doc)
}

/// Set `[tasks].log_retention_days`. Rejects `days < 1` with `Err` and
/// leaves the file unchanged.
pub fn set_log_retention_days(project_root: &Path, days: i64) -> Result<()> {
    check_days("log_retention_days", days)?;
    let file = project_root.join("keitora.toml");
    let mut doc = load_doc(&file)?;
    apply_log_retention_days(&mut doc, days);
    save_doc(&file, &doc)
}

/// Set `[project].worktrees_dir` to the trimmed value, or **remove the key**
/// when `dir` is `None` or empty (falls back to the `~/.keitora/<name>` default).
/// The raw value is stored as-is (tilde preserved).
pub fn set_worktrees_dir(project_root: &Path, dir: Option<String>) -> Result<()> {
    let file = project_root.join("keitora.toml");
    let mut doc = load_doc(&file)?;
    apply_worktrees_dir(&mut doc, dir.as_deref());
    save_doc(&file, &doc)
}

/// Set `[claude].path` in the project `keitora.toml`. When `path` is `None` or
/// empty, remove the `path` key and, if the `[claude]` table is now empty,
/// remove the whole table (mirrors `ProviderConfig::is_empty`).
pub fn set_project_claude_path(project_root: &Path, path: Option<String>) -> Result<()> {
    let file = project_root.join("keitora.toml");
    let mut doc = load_doc(&file)?;
    apply_claude_path(&mut doc, path.as_deref());
    save_doc(&file, &doc)
}

/// Set `[codex].path` in the project `keitora.toml`. Mirrors
/// [`set_project_claude_path`]: `None`/empty removes the key and collapses the
/// `[codex]` table when it is then empty.
pub fn set_project_codex_path(project_root: &Path, path: Option<String>) -> Result<()> {
    let file = project_root.join("keitora.toml");
    let mut doc = load_doc(&file)?;
    apply_codex_path(&mut doc, path.as_deref());
    save_doc(&file, &doc)
}

/// Set `[dispatch].claude_steps` to the trimmed, non-empty elements of `steps`.
/// When nothing remains, remove the key and collapse `[dispatch]` only when
/// `codex_steps` is also absent (keeping the built-in-prompt fallback + siblings).
pub fn set_dispatch_steps(project_root: &Path, steps: &[String]) -> Result<()> {
    let file = project_root.join("keitora.toml");
    let mut doc = load_doc(&file)?;
    apply_claude_dispatch_steps(&mut doc, steps);
    save_doc(&file, &doc)
}

/// Set `[dispatch].codex_steps`. The Codex sibling of [`set_dispatch_steps`].
pub fn set_dispatch_steps_codex(project_root: &Path, steps: &[String]) -> Result<()> {
    let file = project_root.join("keitora.toml");
    let mut doc = load_doc(&file)?;
    apply_codex_dispatch_steps(&mut doc, steps);
    save_doc(&file, &doc)
}

// ---------------------------------------------------------------------------
// User setters
// ---------------------------------------------------------------------------

/// Set `[claude].path` in the user `keitora.toml`. When `path` is `None` or
/// empty, remove the `path` key and, if the `[claude]` table is now empty,
/// remove the whole table.
///
/// Creates the user-config file (and its parent directory) when it does not
/// yet exist.
pub fn set_user_claude_path(path: Option<String>) -> Result<()> {
    let file = user_config_file()?;
    let _registry = crate::UserConfig::write_lock();
    set_user_claude_path_at(&file, path)
}

/// Same as [`set_user_claude_path`] but operates on an explicit path
/// (used by tests for hermeticity).
pub fn set_user_claude_path_at(file: &Path, path: Option<String>) -> Result<()> {
    edit_user_doc(file, |doc| apply_claude_path(doc, path.as_deref()))
}

/// Set `[codex].path` in the user `keitora.toml`. The Codex sibling of
/// [`set_user_claude_path`]; creates the file/parent dir when absent.
pub fn set_user_codex_path(path: Option<String>) -> Result<()> {
    let file = user_config_file()?;
    let _registry = crate::UserConfig::write_lock();
    set_user_codex_path_at(&file, path)
}

/// Same as [`set_user_codex_path`] but operates on an explicit path (tests).
pub fn set_user_codex_path_at(file: &Path, path: Option<String>) -> Result<()> {
    edit_user_doc(file, |doc| apply_codex_path(doc, path.as_deref()))
}

/// Load the user registry doc (empty when absent), apply `edit`, and save it,
/// creating the parent directory. Shared by the user-level per-CLI path setters.
fn edit_user_doc(file: &Path, edit: impl FnOnce(&mut toml_edit::DocumentMut)) -> Result<()> {
    // Ensure parent directory exists.
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Config(format!("creating {}: {e}", parent.display())))?;
    }

    // If the file doesn't exist yet, start from an empty document.
    let mut doc = if file.exists() {
        load_doc(file)?
    } else {
        toml_edit::DocumentMut::new()
    };

    edit(&mut doc);

    save_doc(file, &doc)
}

/// Remove a project from the user registry by name. If the `active` key
/// equals `name`, clears it too (so the app falls back to onboarding).
/// No-op-safe when the key is absent.
pub fn remove_project_from_registry(name: &str) -> Result<()> {
    let file = user_config_file()?;
    let _registry = crate::UserConfig::write_lock();
    remove_project_from_registry_at(&file, name)
}

/// Same as [`remove_project_from_registry`] but operates on an explicit path
/// (used by tests for hermeticity).
pub fn remove_project_from_registry_at(file: &Path, name: &str) -> Result<()> {
    if !file.exists() {
        // Nothing to remove.
        return Ok(());
    }

    let mut doc = load_doc(file)?;

    // Remove the key from [projects].
    if let Some(projects) = doc.get_mut("projects").and_then(|t| t.as_table_mut()) {
        projects.remove(name);
    }

    // If [active] equals name, clear it.
    let active_matches = doc
        .get("active")
        .and_then(|v| v.as_str())
        .map(|s| s == name)
        .unwrap_or(false);
    if active_matches {
        doc.remove("active");
    }

    save_doc(file, &doc)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ---- helpers -----------------------------------------------------------

    /// Write a `keitora.toml` with a comment, worktrees, and both retention fields,
    /// returning the root dir (kept alive by caller).
    fn write_sample_project(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("keitora.toml"),
            "# top-level comment\n\
             [project]\n\
             name = \"acme\"\n\
             worktrees_dir = \"../acme-worktrees\"\n\
             \n\
             [tasks]\n\
             db_path = \"../acme-worktrees/shared/tasks.db\"\n\
             worktrees = [\"acme\", \"sayo\"]\n\
             archive_days = 30\n\
             log_retention_days = 90\n",
        )
        .unwrap();
        dir.to_path_buf()
    }

    fn read_doc(root: &Path) -> toml_edit::DocumentMut {
        let text = std::fs::read_to_string(root.join("keitora.toml")).unwrap();
        text.parse().unwrap()
    }

    // ---- shared normalizers ----------------------------------------

    #[test]
    fn normalize_worktrees_dir_trims_and_empties_to_none() {
        assert_eq!(
            normalize_worktrees_dir(Some("  ~/wt  ")),
            Some("~/wt".to_string())
        );
        assert_eq!(normalize_worktrees_dir(Some("   ")), None);
        assert_eq!(normalize_worktrees_dir(Some("")), None);
        assert_eq!(normalize_worktrees_dir(None), None);
    }

    #[test]
    fn clean_dispatch_steps_trims_and_drops_empty_preserving_order() {
        let got = clean_dispatch_steps(&[
            " /clear ".to_string(),
            "  ".to_string(),
            "/park".to_string(),
        ]);
        assert_eq!(got, vec!["/clear".to_string(), "/park".to_string()]);
        assert!(clean_dispatch_steps(&[]).is_empty());
        assert!(clean_dispatch_steps(&["  ".to_string(), "\t".to_string()]).is_empty());
    }

    #[test]
    fn check_days_floor() {
        assert!(check_days("archive_days", 1).is_ok());
        assert!(check_days("archive_days", 0).is_err());
        assert!(check_days("archive_days", -3).is_err());
    }

    // ---- archive_days ------------------------------------------------------

    #[test]
    fn set_archive_days_updates_value() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        set_archive_days(tmp.path(), 14).unwrap();

        let doc = read_doc(tmp.path());
        assert_eq!(doc["tasks"]["archive_days"].as_integer(), Some(14));
    }

    #[test]
    fn set_archive_days_preserves_comments_and_other_fields() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        set_archive_days(tmp.path(), 60).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        assert!(raw.contains("# top-level comment"), "comment must survive");
        let doc: toml_edit::DocumentMut = raw.parse().unwrap();
        // Untouched fields survive.
        assert_eq!(doc["tasks"]["log_retention_days"].as_integer(), Some(90));
        assert_eq!(doc["project"]["name"].as_str(), Some("acme"));
        assert_eq!(
            doc["tasks"]["db_path"].as_str(),
            Some("../acme-worktrees/shared/tasks.db")
        );
    }

    #[test]
    fn set_archive_days_rejects_zero_and_negative() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        assert!(set_archive_days(tmp.path(), 0).is_err());
        assert!(set_archive_days(tmp.path(), -5).is_err());

        // File must be unchanged after rejection.
        let doc = read_doc(tmp.path());
        assert_eq!(doc["tasks"]["archive_days"].as_integer(), Some(30));
    }

    // ---- log_retention_days ------------------------------------------------

    #[test]
    fn set_log_retention_days_updates_value() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        set_log_retention_days(tmp.path(), 180).unwrap();

        let doc = read_doc(tmp.path());
        assert_eq!(doc["tasks"]["log_retention_days"].as_integer(), Some(180));
    }

    #[test]
    fn set_log_retention_days_rejects_zero_and_negative() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        assert!(set_log_retention_days(tmp.path(), 0).is_err());
        assert!(set_log_retention_days(tmp.path(), -1).is_err());

        // File unchanged.
        let doc = read_doc(tmp.path());
        assert_eq!(doc["tasks"]["log_retention_days"].as_integer(), Some(90));
    }

    // ---- worktrees_dir -----------------------------------------------------

    #[test]
    fn set_worktrees_dir_sets_value() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        set_worktrees_dir(tmp.path(), Some("~/.keitora/acme".to_string())).unwrap();

        let doc = read_doc(tmp.path());
        assert_eq!(
            doc["project"]["worktrees_dir"].as_str(),
            Some("~/.keitora/acme")
        );
    }

    #[test]
    fn set_worktrees_dir_removes_key_on_none() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        set_worktrees_dir(tmp.path(), None).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        // Key must be absent, not just empty.
        assert!(
            !raw.contains("worktrees_dir"),
            "key must be removed:\n{raw}"
        );
        // Other project fields survive.
        let doc: toml_edit::DocumentMut = raw.parse().unwrap();
        assert_eq!(doc["project"]["name"].as_str(), Some("acme"));
    }

    #[test]
    fn set_worktrees_dir_removes_key_on_empty_string() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        set_worktrees_dir(tmp.path(), Some("   ".to_string())).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        assert!(
            !raw.contains("worktrees_dir"),
            "whitespace-only is treated as None:\n{raw}"
        );
    }

    #[test]
    fn set_worktrees_dir_preserves_tilde() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        set_worktrees_dir(tmp.path(), Some("~/code/acme-worktrees".to_string())).unwrap();

        let doc = read_doc(tmp.path());
        assert_eq!(
            doc["project"]["worktrees_dir"].as_str(),
            Some("~/code/acme-worktrees"),
            "tilde must be stored verbatim"
        );
    }

    // ---- set_project_claude_path -------------------------------------------

    #[test]
    fn set_project_claude_path_creates_claude_table() {
        let tmp = TempDir::new().unwrap();
        // Start without a [claude] section.
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(
            tmp.path().join("keitora.toml"),
            "[project]\nname = \"acme\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n",
        )
        .unwrap();

        set_project_claude_path(tmp.path(), Some("/usr/local/bin/claude".to_string())).unwrap();

        let doc = read_doc(tmp.path());
        assert_eq!(
            doc["claude"]["path"].as_str(),
            Some("/usr/local/bin/claude")
        );
    }

    #[test]
    fn set_project_claude_path_updates_existing_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(
            tmp.path().join("keitora.toml"),
            "[project]\nname = \"acme\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n\n\
             [claude]\npath = \"/old/claude\"\n",
        )
        .unwrap();

        set_project_claude_path(tmp.path(), Some("/new/claude".to_string())).unwrap();

        let doc = read_doc(tmp.path());
        assert_eq!(doc["claude"]["path"].as_str(), Some("/new/claude"));
    }

    #[test]
    fn set_project_claude_path_none_removes_key_and_empty_table() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(
            tmp.path().join("keitora.toml"),
            "[project]\nname = \"acme\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n\n\
             [claude]\npath = \"/usr/bin/claude\"\n",
        )
        .unwrap();

        set_project_claude_path(tmp.path(), None).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        assert!(
            !raw.contains("[claude]"),
            "empty [claude] table must be removed:\n{raw}"
        );
        // Other sections survive.
        let doc: toml_edit::DocumentMut = raw.parse().unwrap();
        assert!(
            doc.get("claude").is_none(),
            "claude table must not exist:\n{raw}"
        );
        assert_eq!(doc["project"]["name"].as_str(), Some("acme"));
    }

    #[test]
    fn set_project_claude_path_empty_string_removes_table() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(
            tmp.path().join("keitora.toml"),
            "[project]\nname = \"acme\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n\n\
             [claude]\npath = \"/usr/bin/claude\"\n",
        )
        .unwrap();

        set_project_claude_path(tmp.path(), Some("  ".to_string())).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        assert!(
            !raw.contains("[claude]"),
            "whitespace-only path must remove the [claude] table:\n{raw}"
        );
    }

    // ---- set_dispatch_steps ------------------------------------------------

    #[test]
    fn set_dispatch_steps_writes_ordered_array() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        let steps = vec![
            "/clear".to_string(),
            "/park".to_string(),
            "/workflow {task_number}".to_string(),
        ];
        set_dispatch_steps(tmp.path(), &steps).unwrap();

        let doc = read_doc(tmp.path());
        let arr = doc["dispatch"]["claude_steps"].as_array().unwrap();
        let got: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(got, vec!["/clear", "/park", "/workflow {task_number}"]);
    }

    #[test]
    fn set_dispatch_steps_empty_removes_dispatch_table() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(
            tmp.path().join("keitora.toml"),
            "[project]\nname = \"acme\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n\n\
             [dispatch]\nclaude_steps = [\"/clear\"]\n",
        )
        .unwrap();

        set_dispatch_steps(tmp.path(), &[]).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        assert!(
            !raw.contains("[dispatch]"),
            "empty steps must remove [dispatch]:\n{raw}"
        );
    }

    #[test]
    fn set_dispatch_steps_whitespace_only_steps_are_dropped() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        // All steps are whitespace-only → same as empty → dispatch removed.
        set_dispatch_steps(tmp.path(), &["  ".to_string(), "\t".to_string()]).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        assert!(
            !raw.contains("[dispatch]"),
            "whitespace-only steps must remove [dispatch]:\n{raw}"
        );
    }

    #[test]
    fn set_dispatch_steps_reorder_is_reflected() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(
            tmp.path().join("keitora.toml"),
            "[project]\nname = \"acme\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n\n\
             [dispatch]\nclaude_steps = [\"a\", \"b\", \"c\"]\n",
        )
        .unwrap();

        set_dispatch_steps(
            tmp.path(),
            &["c".to_string(), "a".to_string(), "b".to_string()],
        )
        .unwrap();

        let doc = read_doc(tmp.path());
        let arr = doc["dispatch"]["claude_steps"].as_array().unwrap();
        let got: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(got, vec!["c", "a", "b"], "reordered steps must match");
    }

    #[test]
    fn set_dispatch_steps_preserves_other_fields() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        set_dispatch_steps(tmp.path(), &["/clear".to_string()]).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        assert!(raw.contains("# top-level comment"), "comment must survive");
        let doc: toml_edit::DocumentMut = raw.parse().unwrap();
        assert_eq!(doc["tasks"]["archive_days"].as_integer(), Some(30));
        assert_eq!(doc["project"]["name"].as_str(), Some("acme"));
    }

    // ---- set_user_claude_path ----------------------------------------------

    fn user_file(tmp: &TempDir) -> PathBuf {
        tmp.path().join("keitora").join("keitora.toml")
    }

    fn write_user_config(tmp: &TempDir, content: &str) {
        let path = user_file(tmp);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }

    #[test]
    fn set_user_claude_path_creates_file_when_absent() {
        let tmp = TempDir::new().unwrap();
        let file = user_file(&tmp);

        set_user_claude_path_at(&file, Some("/usr/bin/claude".to_string())).unwrap();

        assert!(file.exists());
        let doc: toml_edit::DocumentMut = std::fs::read_to_string(&file).unwrap().parse().unwrap();
        assert_eq!(doc["claude"]["path"].as_str(), Some("/usr/bin/claude"));
    }

    #[test]
    fn set_user_claude_path_updates_existing() {
        let tmp = TempDir::new().unwrap();
        write_user_config(&tmp, "[claude]\npath = \"/old/claude\"\n\n[projects]\n");

        let file = user_file(&tmp);
        set_user_claude_path_at(&file, Some("/new/claude".to_string())).unwrap();

        let doc: toml_edit::DocumentMut = std::fs::read_to_string(&file).unwrap().parse().unwrap();
        assert_eq!(doc["claude"]["path"].as_str(), Some("/new/claude"));
    }

    #[test]
    fn set_user_claude_path_none_removes_key_and_empty_table() {
        let tmp = TempDir::new().unwrap();
        write_user_config(
            &tmp,
            "active = \"keitora\"\n\n[claude]\npath = \"/usr/bin/claude\"\n\n[projects]\n",
        );

        let file = user_file(&tmp);
        set_user_claude_path_at(&file, None).unwrap();

        let raw = std::fs::read_to_string(&file).unwrap();
        assert!(
            !raw.contains("[claude]"),
            "empty [claude] must be removed:\n{raw}"
        );
        // Other keys survive.
        let doc: toml_edit::DocumentMut = raw.parse().unwrap();
        assert_eq!(doc["active"].as_str(), Some("keitora"));
    }

    #[test]
    fn set_user_claude_path_empty_string_removes_table() {
        let tmp = TempDir::new().unwrap();
        write_user_config(&tmp, "[claude]\npath = \"/usr/bin/claude\"\n");

        let file = user_file(&tmp);
        set_user_claude_path_at(&file, Some("   ".to_string())).unwrap();

        let raw = std::fs::read_to_string(&file).unwrap();
        assert!(
            !raw.contains("[claude]"),
            "whitespace-only must remove [claude]:\n{raw}"
        );
    }

    // ---- remove_project_from_registry --------------------------------------

    fn write_registry(tmp: &TempDir, content: &str) -> PathBuf {
        let file = user_file(tmp);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, content).unwrap();
        file
    }

    #[test]
    fn remove_project_from_registry_removes_key() {
        let tmp = TempDir::new().unwrap();
        let file = write_registry(
            &tmp,
            "active = \"keitora\"\n\n[projects.keitora]\npath = \"/Users/x/code/keitora\"\n\n\
             [projects.acme]\npath = \"/Users/x/code/acme\"\n",
        );

        remove_project_from_registry_at(&file, "acme").unwrap();

        let doc: toml_edit::DocumentMut = std::fs::read_to_string(&file).unwrap().parse().unwrap();
        // acme is gone; keitora survives.
        assert!(
            doc["projects"].get("acme").is_none(),
            "acme must be removed"
        );
        assert!(
            doc["projects"]["keitora"].get("path").is_some(),
            "keitora must survive"
        );
        // active is unchanged (it was "keitora", not "acme").
        assert_eq!(doc["active"].as_str(), Some("keitora"));
    }

    #[test]
    fn remove_project_from_registry_clears_active_when_it_matched() {
        let tmp = TempDir::new().unwrap();
        let file = write_registry(
            &tmp,
            "active = \"acme\"\n\n[projects.acme]\npath = \"/Users/x/code/acme\"\n\n\
             [projects.keitora]\npath = \"/Users/x/code/keitora\"\n",
        );

        remove_project_from_registry_at(&file, "acme").unwrap();

        let raw = std::fs::read_to_string(&file).unwrap();
        // active must be cleared.
        assert!(
            !raw.contains("active"),
            "active must be removed when it pointed at the removed project:\n{raw}"
        );
        // Other entries survive.
        let doc: toml_edit::DocumentMut = raw.parse().unwrap();
        assert!(doc["projects"]["keitora"].get("path").is_some());
    }

    #[test]
    fn remove_project_from_registry_absent_key_is_noop() {
        let tmp = TempDir::new().unwrap();
        let file = write_registry(
            &tmp,
            "active = \"keitora\"\n\n[projects.keitora]\npath = \"/Users/x/code/keitora\"\n",
        );

        // "ghost" does not exist → no error.
        remove_project_from_registry_at(&file, "ghost").unwrap();

        let doc: toml_edit::DocumentMut = std::fs::read_to_string(&file).unwrap().parse().unwrap();
        assert_eq!(doc["active"].as_str(), Some("keitora"));
        assert!(doc["projects"]["keitora"].get("path").is_some());
    }

    #[test]
    fn remove_project_from_registry_missing_file_is_noop() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("nonexistent").join("keitora.toml");

        // Must not error when file doesn't exist.
        remove_project_from_registry_at(&file, "any").unwrap();
    }

    // ---- write_project_settings (transactional) ----------------------------

    #[test]
    fn write_project_settings_applies_all_fields_in_one_pass() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        write_project_settings(
            tmp.path(),
            14,
            45,
            Some("~/wt".to_string()),
            Some("/usr/bin/claude".to_string()),
            Some("/usr/bin/codex".to_string()),
            Some(true),
            Some(false),
            &["/clear".to_string(), "/park".to_string()],
            &["/codex-clear".to_string()],
        )
        .unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        assert!(raw.contains("# top-level comment"), "comment must survive");
        let doc: toml_edit::DocumentMut = raw.parse().unwrap();
        assert_eq!(doc["tasks"]["archive_days"].as_integer(), Some(14));
        assert_eq!(doc["tasks"]["log_retention_days"].as_integer(), Some(45));
        assert_eq!(doc["project"]["worktrees_dir"].as_str(), Some("~/wt"));
        assert_eq!(doc["claude"]["path"].as_str(), Some("/usr/bin/claude"));
        assert_eq!(doc["claude"]["autonomy"].as_bool(), Some(true));
        assert_eq!(doc["codex"]["path"].as_str(), Some("/usr/bin/codex"));
        assert_eq!(doc["codex"]["autonomy"].as_bool(), Some(false));
        let steps: Vec<&str> = doc["dispatch"]["claude_steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(steps, vec!["/clear", "/park"]);
        let codex_steps: Vec<&str> = doc["dispatch"]["codex_steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(codex_steps, vec!["/codex-clear"]);
        // Untouched required fields survive.
        assert_eq!(doc["project"]["name"].as_str(), Some("acme"));
        assert!(doc["tasks"]["db_path"].as_str().is_some());
    }

    #[test]
    fn write_project_settings_rejects_bad_days_without_touching_file() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());
        let before = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();

        // archive_days < 1 → Err, validated before the file is opened.
        assert!(
            write_project_settings(tmp.path(), 0, 90, None, None, None, None, None, &[], &[])
                .is_err()
        );
        // log_retention_days < 1 → Err.
        assert!(
            write_project_settings(tmp.path(), 30, 0, None, None, None, None, None, &[], &[])
                .is_err()
        );

        let after = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        assert_eq!(
            before, after,
            "rejected save must leave the file byte-identical"
        );
    }

    // ---- codex path / autonomy / per-CLI dispatch ------------------

    #[test]
    fn set_project_codex_path_creates_and_removes_codex_table() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(
            tmp.path().join("keitora.toml"),
            "[project]\nname = \"acme\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n",
        )
        .unwrap();

        set_project_codex_path(tmp.path(), Some("/usr/bin/codex".to_string())).unwrap();
        let doc = read_doc(tmp.path());
        assert_eq!(doc["codex"]["path"].as_str(), Some("/usr/bin/codex"));

        // Clearing removes the key and collapses the now-empty [codex] table.
        set_project_codex_path(tmp.path(), None).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        assert!(
            !raw.contains("[codex]"),
            "empty [codex] must be removed:\n{raw}"
        );
    }

    #[test]
    fn set_dispatch_steps_codex_writes_codex_steps() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        set_dispatch_steps_codex(tmp.path(), &["/codex-clear".to_string()]).unwrap();

        let doc = read_doc(tmp.path());
        let arr = doc["dispatch"]["codex_steps"].as_array().unwrap();
        let got: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(got, vec!["/codex-clear"]);
    }

    #[test]
    fn clearing_one_cli_steps_keeps_dispatch_table_when_other_cli_present() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(
            tmp.path().join("keitora.toml"),
            "[project]\nname = \"acme\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n\n\
             [dispatch]\nclaude_steps = [\"/c\"]\ncodex_steps = [\"/x\"]\n",
        )
        .unwrap();

        // Clear claude_steps only: [dispatch] must survive because codex_steps remain.
        set_dispatch_steps(tmp.path(), &[]).unwrap();
        let doc = read_doc(tmp.path());
        assert!(
            doc["dispatch"].get("claude_steps").is_none(),
            "claude_steps must be removed"
        );
        let arr = doc["dispatch"]["codex_steps"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "codex_steps must survive:\n{}", doc);

        // Now clear codex_steps too → the whole [dispatch] table collapses.
        set_dispatch_steps_codex(tmp.path(), &[]).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        assert!(
            !raw.contains("[dispatch]"),
            "both keys cleared must remove [dispatch]:\n{raw}"
        );
    }

    #[test]
    fn write_project_settings_writes_both_cli_autonomy_and_omits_empty() {
        let tmp = TempDir::new().unwrap();
        write_sample_project(tmp.path());

        // Only claude autonomy set; codex fully unconfigured → no [codex] table.
        write_project_settings(
            tmp.path(),
            30,
            90,
            None,
            None,
            None,
            Some(true),
            None,
            &[],
            &[],
        )
        .unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        let doc: toml_edit::DocumentMut = raw.parse().unwrap();
        assert_eq!(doc["claude"]["autonomy"].as_bool(), Some(true));
        assert!(
            !raw.contains("[codex]"),
            "unconfigured [codex] omitted:\n{raw}"
        );
    }

    #[test]
    fn set_user_codex_path_creates_file_when_absent() {
        let tmp = TempDir::new().unwrap();
        let file = user_file(&tmp);

        set_user_codex_path_at(&file, Some("/usr/bin/codex".to_string())).unwrap();

        assert!(file.exists());
        let doc: toml_edit::DocumentMut = std::fs::read_to_string(&file).unwrap().parse().unwrap();
        assert_eq!(doc["codex"]["path"].as_str(), Some("/usr/bin/codex"));
    }

    #[test]
    fn set_dispatch_steps_clearing_preserves_sibling_keys() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(
            tmp.path().join("keitora.toml"),
            "[project]\nname = \"acme\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n\n\
             [dispatch]\nclaude_steps = [\"/clear\"]\ntimeout = 30\n",
        )
        .unwrap();

        // Clearing steps must drop only `claude_steps`, not a sibling key.
        set_dispatch_steps(tmp.path(), &[]).unwrap();

        let doc = read_doc(tmp.path());
        assert!(
            doc["dispatch"].get("claude_steps").is_none(),
            "claude_steps key must be removed"
        );
        assert_eq!(
            doc["dispatch"]["timeout"].as_integer(),
            Some(30),
            "sibling [dispatch] key must survive clearing steps"
        );
    }

    // ---- back-compat: legacy alias must be dropped on write ----------
    // A surgical writer that wrote the new key WITHOUT removing the legacy
    // legacy alias left BOTH keys in the table; serde reads both as the same
    // aliased field → `duplicate field` → the project could no longer load. These
    // tests start from the LEGACY key, run the actual toml_edit writer, then assert
    // (a) only the new key remains and (b) the file still deserializes through the
    // real `ProjectConfig` load with the value preserved.

    /// Deserialize a `keitora.toml` string through the real `ProjectConfig` path
    /// (writing it to a temp dir), returning the load result.
    fn load_project_from_toml(
        toml: &str,
    ) -> std::result::Result<crate::config::ProjectConfig, String> {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("keitora.toml"), toml).unwrap();
        crate::config::ProjectConfig::load(tmp.path()).map_err(|e| e.to_string())
    }

    const MINIMAL_PROJECT: &str = "[project]\nname = \"acme\"\n\n[tasks]\ndb_path = \"tasks.db\"\n";

    #[test]
    fn duplicate_autonomy_and_skip_permissions_fails_to_load() {
        // Guards the hazard the fix prevents: a table carrying BOTH the legacy
        // alias and the new key is a `duplicate field` on load.
        let both =
            format!("{MINIMAL_PROJECT}\n[claude]\nskip_permissions = true\nautonomy = true\n");
        assert!(
            load_project_from_toml(&both).is_err(),
            "both keys present must fail to deserialize"
        );
    }

    #[test]
    fn apply_claude_autonomy_removes_legacy_skip_permissions_key() {
        // START from the LEGACY key only.
        let start = format!("{MINIMAL_PROJECT}\n[claude]\nskip_permissions = true\n");
        let mut doc: toml_edit::DocumentMut = start.parse().unwrap();

        apply_claude_autonomy(&mut doc, Some(true));

        let out = doc.to_string();
        // (a) only the new key remains — no legacy `skip_permissions` left.
        assert!(
            !out.contains("skip_permissions"),
            "legacy skip_permissions must be removed:\n{out}"
        );
        assert!(
            out.contains("autonomy"),
            "new autonomy key must be written:\n{out}"
        );

        // (b) round-trips through the real ProjectConfig load, value preserved.
        let cfg = load_project_from_toml(&out).expect("must load without duplicate-field error");
        assert_eq!(cfg.claude.autonomy, Some(true));
    }

    #[test]
    fn write_project_settings_drops_legacy_skip_permissions() {
        // The transactional writer must also clear the legacy alias.
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("keitora.toml"),
            format!("{MINIMAL_PROJECT}\n[claude]\nskip_permissions = true\n"),
        )
        .unwrap();

        write_project_settings(
            tmp.path(),
            30,
            90,
            None,
            None,
            None,
            Some(false),
            None,
            &[],
            &[],
        )
        .unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("keitora.toml")).unwrap();
        assert!(
            !raw.contains("skip_permissions"),
            "legacy skip_permissions must be removed:\n{raw}"
        );
        let cfg = crate::config::ProjectConfig::load(tmp.path()).expect("must load");
        assert_eq!(cfg.claude.autonomy, Some(false));
    }

    #[test]
    fn apply_claude_dispatch_steps_removes_legacy_steps_key() {
        // START from the LEGACY `steps` key only.
        let start = format!("{MINIMAL_PROJECT}\n[dispatch]\nsteps = [\"/clear\"]\n");
        let mut doc: toml_edit::DocumentMut = start.parse().unwrap();

        apply_claude_dispatch_steps(&mut doc, &["/clear".to_string(), "/park".to_string()]);

        let out = doc.to_string();
        // (a) the legacy `steps` key is gone; only `claude_steps` remains.
        // (match the whole key token so `claude_steps` doesn't count as `steps`).
        assert!(
            !out.contains("\nsteps ") && !out.starts_with("steps "),
            "legacy steps key must be removed:\n{out}"
        );
        assert!(
            out.contains("claude_steps"),
            "new claude_steps key must be written:\n{out}"
        );

        // (b) round-trips through the real ProjectConfig load, value preserved.
        let cfg = load_project_from_toml(&out).expect("must load without duplicate-field error");
        assert_eq!(cfg.dispatch.claude_steps, vec!["/clear", "/park"]);
    }

    #[test]
    fn codex_writers_do_not_touch_a_legacy_claude_alias() {
        // Writing the codex keys must NOT remove the claude legacy alias (codex has
        // none, and the claude alias belongs to a different table/key).
        let start = format!(
            "{MINIMAL_PROJECT}\n[claude]\nskip_permissions = true\n\n[dispatch]\nsteps = [\"/clear\"]\n"
        );
        let mut doc: toml_edit::DocumentMut = start.parse().unwrap();

        apply_codex_autonomy(&mut doc, Some(true));
        apply_codex_dispatch_steps(&mut doc, &["/codex".to_string()]);

        let out = doc.to_string();
        assert!(
            out.contains("skip_permissions"),
            "codex writers must leave the claude legacy alias intact:\n{out}"
        );
        assert!(
            out.contains("[codex]"),
            "codex autonomy must be written:\n{out}"
        );
        assert!(
            out.contains("codex_steps"),
            "codex_steps must be written:\n{out}"
        );
    }
}
