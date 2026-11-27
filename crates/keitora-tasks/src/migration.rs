//! Legacy `task-config.json` → `keitora.toml` migration.
//!
//! A one-release fallback: if a project still has a legacy `task-config.json`
//! but no `keitora.toml`, the first `keitora` command (or app launch) auto-migrates it
//! to a committed project `keitora.toml`, rewriting the absolute `db_path` to a
//! repo-relative path. Migration is idempotent (a pre-existing `keitora.toml` is a
//! no-op) and emits a one-time deprecation warning.
//!
//! After the fallback window, this entire module — `find_legacy_config`,
//! `migrate_legacy`, and the legacy JSON shape — is removed and
//! `task-config.json` is ignored.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::{
    default_archive_days, default_log_retention_days, default_worktrees_dir, git_toplevel,
    git_toplevel_in, CliConfig, DispatchConfig, ProjectConfig, ProjectMeta, ProviderConfig,
    TaskConfig, UserConfig, WorktreeEntry,
};
use crate::{Error, Result};

/// The legacy `task-config.json` shape: `{ db_path, archive_days,
/// log_retention_days, worktrees }`. Deserialized verbatim; the `serde`
/// defaults apply only when a key is absent (every real file has all four).
#[derive(Debug, Clone, Deserialize)]
pub struct LegacyConfig {
    pub db_path: String,
    #[serde(default = "default_archive_days")]
    pub archive_days: i64,
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: i64,
    #[serde(default)]
    pub worktrees: Vec<String>,
}

impl LegacyConfig {
    /// Read and parse a legacy `task-config.json` from an explicit path.
    fn load(path: &Path) -> Result<LegacyConfig> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading {}: {e}", path.display())))?;
        serde_json::from_str(&text)
            .map_err(|e| Error::Config(format!("parsing {}: {e}", path.display())))
    }
}

// ---------------------------------------------------------------------------
// Legacy detection — the retained 8-level upward walk
// ---------------------------------------------------------------------------

/// Locate a legacy `task-config.json` (the retained pre-Stage-1
/// `find_config_path` walk). Returns `None` when none is found, leaving
/// `resolve()` to decide what to do.
///
/// Algorithm:
/// 1. `$KEITORA_SHARED_DIR` takes precedence — return `$KEITORA_SHARED_DIR/task-config.json`
///    unconditionally (even if it does not exist; the caller checks existence).
/// 2. Otherwise walk up from the git toplevel (or cwd if not in a repo), up to 8
///    levels, trying `<dir>/keitora-worktrees/shared/task-config.json` then
///    `<dir>/shared/task-config.json` at each level.
pub fn find_legacy_config() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("KEITORA_SHARED_DIR") {
        let candidate = PathBuf::from(dir).join("task-config.json");
        return Some(candidate);
    }

    let start = git_toplevel()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    find_legacy_config_from(&start)
}

/// Walk up from `start` (≤8 levels) for a legacy `task-config.json`, trying
/// `<dir>/keitora-worktrees/shared/` then `<dir>/shared/` at each level. Takes the
/// start dir explicitly (no process-global cwd) so `resolve_in` and tests are
/// hermetic.
pub(crate) fn find_legacy_config_from(start: &Path) -> Option<PathBuf> {
    let mut cur = start;
    for _ in 0..8 {
        for candidate in [
            cur.join("keitora-worktrees")
                .join("shared")
                .join("task-config.json"),
            cur.join("shared").join("task-config.json"),
        ] {
            if candidate.exists() {
                return Some(candidate);
            }
        }
        match cur.parent() {
            Some(parent) => cur = parent,
            None => break,
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Project-root derivation
// ---------------------------------------------------------------------------

/// Derive the committed project root for the implicit (`resolve()`) migration
/// trigger.
///
/// The Keitora layout stores the legacy file at `<worktrees-dir>/shared/task-config.json`
/// where `<worktrees-dir>` is a sibling of the main repo, named `<name>-worktrees`
/// (e.g. `~/code/keitora-worktrees`, main repo `~/code/keitora`). The committed
/// `keitora.toml` must live at the main repo, not inside a worktree or the shared
/// dir — so that the repo-relative `db_path` (`../keitora-worktrees/shared/tasks.db`)
/// is committed once and shared by every worktree.
///
/// Rule:
/// 1. If the legacy file's grandparent directory is named `<prefix>-worktrees`,
///    the project root is its sibling `<prefix>` (`<great-grandparent>/<prefix>`).
///    This is the canonical Keitora worktree layout and matches the design doc's
///    worked example.
/// 2. Otherwise fall back to the design doc's generic rule: `git_toplevel()` from
///    the directory containing the legacy file, else the legacy file's parent
///    directory.
fn derive_project_root(legacy_path: &Path) -> PathBuf {
    // The directory containing `task-config.json` (e.g. `.../keitora-worktrees/shared`).
    let legacy_dir = legacy_path.parent();

    // The `<worktrees-dir>` (e.g. `.../keitora-worktrees`) is the legacy file's
    // grandparent: `.../keitora-worktrees/shared/task-config.json`.
    if let Some(worktrees_dir) = legacy_dir.and_then(Path::parent) {
        if let Some(wt_name) = worktrees_dir.file_name().and_then(|n| n.to_str()) {
            if let Some(prefix) = wt_name.strip_suffix("-worktrees") {
                if !prefix.is_empty() {
                    if let Some(parent) = worktrees_dir.parent() {
                        return parent.join(prefix);
                    }
                }
            }
        }
    }

    // Generic fallback (design doc step 2).
    if let Some(dir) = legacy_dir {
        if let Some(top) = git_toplevel_in(Some(dir)) {
            return top;
        }
        return dir.to_path_buf();
    }
    PathBuf::from(".")
}

// ---------------------------------------------------------------------------
// Path rewrite: absolute db_path → repo-relative
// ---------------------------------------------------------------------------

/// Rewrite an absolute `db_path` to a path relative to `project_root`, using
/// `pathdiff::diff_paths`. Both paths are canonicalized first so the relative
/// result is stable regardless of symlinks/`..` segments.
fn rewrite_db_path(abs_db_path: &str, project_root: &Path) -> Result<PathBuf> {
    let db = PathBuf::from(abs_db_path)
        .canonicalize()
        .map_err(|e| Error::Config(format!("canonicalize db_path {abs_db_path:?}: {e}")))?;
    let root = project_root
        .canonicalize()
        .map_err(|e| Error::Config(format!("canonicalize project root {project_root:?}: {e}")))?;
    pathdiff::diff_paths(&db, &root)
        .ok_or_else(|| Error::Config(format!("cannot express {db:?} relative to {root:?}")))
}

// ---------------------------------------------------------------------------
// Migration
// ---------------------------------------------------------------------------

/// The deprecation warning emitted once after a successful migration.
fn deprecation_warning(keitora_toml: &Path) -> String {
    format!(
        "[keitora] Warning: task-config.json is deprecated. A new keitora.toml has been created at {}.\n\
         task-config.json will not be read in the next release. Run `keitora config migrate` to confirm.",
        keitora_toml.display()
    )
}

/// Migrate a legacy `task-config.json` into a committed `keitora.toml`, deriving
/// the project root from the legacy file's location (see [`derive_project_root`]).
///
/// On success, prints the deprecation warning to stderr and returns the freshly
/// loaded [`ProjectConfig`]. Idempotent: if the target `keitora.toml` already
/// exists, returns the existing config with no warning.
pub fn migrate_legacy(legacy_path: &Path) -> Result<(ProjectConfig, bool)> {
    let project_root = derive_project_root(legacy_path);
    let migrated = migrate_legacy_inner(legacy_path, &project_root)?;

    // Load the freshly written (or pre-existing) keitora.toml so resolution returns
    // a fully-canonicalized ProjectConfig.
    let cfg = ProjectConfig::load(&project_root)
        .map_err(|e| Error::Config(format!("loading migrated keitora.toml: {e}")))?;

    // `wrote` reflects whether a new keitora.toml was actually written (vs an
    // idempotent no-op) — the accurate signal for callers, rather than guessing
    // from registry state.
    let wrote = migrated.is_some();
    if let Some(toml_path) = migrated {
        let canon = toml_path.canonicalize().unwrap_or(toml_path);
        eprintln!("{}", deprecation_warning(&canon));
    }

    Ok((cfg, wrote))
}

/// The migration core, with an explicit `project_root` so it is unit-testable
/// without a real git process.
///
/// Returns `Ok(Some(keitora_toml_path))` when a new `keitora.toml` was written (so the
/// caller can emit the one-time deprecation warning), or `Ok(None)` when the
/// idempotency guard fired (a `keitora.toml` already existed — no-op, no warning).
pub fn migrate_legacy_inner(legacy_path: &Path, project_root: &Path) -> Result<Option<PathBuf>> {
    let keitora_toml = project_root.join("keitora.toml");

    // Idempotency guard: already migrated → no-op, no warning.
    if keitora_toml.exists() {
        return Ok(None);
    }

    // 1. Read the legacy file.
    let legacy = LegacyConfig::load(legacy_path)?;

    // 2. Rewrite db_path absolute → repo-relative.
    let rel_db = rewrite_db_path(&legacy.db_path, project_root)?;

    // 3. Construct the ProjectConfig.
    let name = project_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let cfg = ProjectConfig {
        root: project_root.to_path_buf(),
        effective_worktrees: None,
        project: ProjectMeta {
            name: name.clone(),
            worktrees_dir: Some(default_worktrees_dir()),
            base_branch: None,
            diff_base: None,
        },
        tasks: TaskConfig {
            db_path: rel_db,
            // Legacy task-config.json stores bare names; migrate to path-less
            // entries (the repair pass / worktrees_dir reconstruction resolves them).
            worktrees: legacy
                .worktrees
                .iter()
                .map(WorktreeEntry::name_only)
                .collect(),
            archive_days: legacy.archive_days,
            log_retention_days: legacy.log_retention_days,
            backend: "sqlite".to_string(),
            github: None,
        },
        claude: ProviderConfig::default(),
        codex: ProviderConfig::default(),
        antigravity: ProviderConfig::default(),
        cli: CliConfig::default(),
        dispatch: DispatchConfig::default(),
    };

    // 4. Serialize and write keitora.toml. `root` is `#[serde(skip)]`, so it is
    //    omitted from the file (it is recomputed on load).
    let text = toml::to_string_pretty(&cfg)
        .map_err(|e| Error::Config(format!("serializing keitora.toml: {e}")))?;
    std::fs::write(&keitora_toml, text)
        .map_err(|e| Error::Config(format!("writing {}: {e}", keitora_toml.display())))?;

    // 5. Register + activate in the user registry.
    let _registry = UserConfig::write_lock();
    let mut registry =
        UserConfig::load().map_err(|e| Error::Config(format!("loading user registry: {e}")))?;
    registry.register(&name, project_root.to_path_buf());
    if registry.active.is_none() {
        registry.set_active(&name);
    }
    registry
        .save()
        .map_err(|e| Error::Config(format!("saving user registry: {e}")))?;

    Ok(Some(keitora_toml))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_ENV_LOCK;
    use tempfile::TempDir;

    // --- Unit 3: JSON → TOML migration ---

    #[test]
    fn migrates_legacy_json_to_toml_and_is_idempotent() {
        // The migration registers in the user registry; isolate it to a tempdir
        // via $KEITORA_CONFIG_HOME (same mechanism + lock as the config tests).
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();

        // Redirect the user registry into the tempdir so the real one is
        // untouched.
        std::env::set_var("KEITORA_CONFIG_HOME", base.join("user_config"));

        // Synthetic absolute db_path inside the tempdir, file created so
        // canonicalize() succeeds.
        let shared = base.join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        let db_abs = shared.join("tasks.db");
        std::fs::write(&db_abs, b"").unwrap();

        // Legacy task-config.json next to the db.
        let legacy_path = shared.join("task-config.json");
        let legacy_json = format!(
            r#"{{
  "db_path": "{}",
  "archive_days": 30,
  "log_retention_days": 90,
  "worktrees": ["keitora", "sayo", "sol", "idris"]
}}"#,
            // Escape backslashes so a Windows path is valid JSON (parses back to the
            // original path); no-op on Unix.
            db_abs.display().to_string().replace('\\', "\\\\")
        );
        std::fs::write(&legacy_path, legacy_json).unwrap();

        // project_root = a subdir of tempdir representing the git root.
        let project_root = base.join("keitora");
        std::fs::create_dir_all(&project_root).unwrap();
        let project_root = project_root.canonicalize().unwrap();

        // --- First migration ---
        let written = migrate_legacy_inner(&legacy_path, &project_root)
            .unwrap()
            .expect("first call writes keitora.toml");
        assert_eq!(written, project_root.join("keitora.toml"));

        // Load and assert each field.
        let cfg = ProjectConfig::load(&project_root).unwrap();
        assert_eq!(
            cfg.project.name, "keitora",
            "name = basename of project_root"
        );
        // load() strips the Windows `\\?\` prefix from root; mirror it (no-op on unix).
        assert_eq!(
            cfg.root,
            crate::config::strip_extended_prefix(project_root.clone()),
            "root = canonicalized project_root"
        );
        assert_eq!(cfg.tasks.archive_days, 30);
        assert_eq!(cfg.tasks.log_retention_days, 90);
        assert_eq!(
            cfg.tasks.worktrees,
            vec![
                WorktreeEntry::name_only("keitora"),
                WorktreeEntry::name_only("sayo"),
                WorktreeEntry::name_only("sol"),
                WorktreeEntry::name_only("idris"),
            ]
        );
        // db_path canonicalizes back to the original absolute path (\\?\-stripped on win).
        assert_eq!(
            cfg.tasks.db_path,
            crate::config::strip_extended_prefix(db_abs.canonicalize().unwrap())
        );

        // Round-trip the raw TOML: db_path is stored *relative*, not absolute.
        let toml_str = std::fs::read_to_string(project_root.join("keitora.toml")).unwrap();
        let parsed: ProjectConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.tasks.archive_days, 30);
        assert_eq!(parsed.tasks.log_retention_days, 90);
        assert_eq!(
            parsed.tasks.worktrees,
            vec![
                WorktreeEntry::name_only("keitora"),
                WorktreeEntry::name_only("sayo"),
                WorktreeEntry::name_only("sol"),
                WorktreeEntry::name_only("idris"),
            ]
        );
        assert!(
            parsed.tasks.db_path.is_relative(),
            "db_path is stored relative in keitora.toml, got {:?}",
            parsed.tasks.db_path
        );
        assert_eq!(
            project_root
                .join(&parsed.tasks.db_path)
                .canonicalize()
                .unwrap(),
            db_abs.canonicalize().unwrap(),
        );

        // Registry: project registered + active.
        let reg = UserConfig::load().unwrap();
        assert!(reg.projects.contains_key("keitora"));
        assert_eq!(reg.projects["keitora"].path, project_root);
        assert_eq!(reg.active.as_deref(), Some("keitora"));

        // --- Idempotency: a second call is a no-op (returns None), file unchanged ---
        let mtime_before = std::fs::metadata(project_root.join("keitora.toml"))
            .unwrap()
            .modified()
            .unwrap();
        let second = migrate_legacy_inner(&legacy_path, &project_root).unwrap();
        assert!(second.is_none(), "second call is a no-op (no warning)");
        let mtime_after = std::fs::metadata(project_root.join("keitora.toml"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(mtime_before, mtime_after, "keitora.toml mtime unchanged");

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    // --- Worked-example path rewrite (design doc) ---

    #[test]
    fn rewrites_db_path_to_repo_relative() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();

        // Layout: <base>/keitora  and  <base>/keitora-worktrees/shared/tasks.db
        let root = base.join("keitora");
        std::fs::create_dir_all(&root).unwrap();
        let shared = base.join("keitora-worktrees").join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        let db_abs = shared.join("tasks.db");
        std::fs::write(&db_abs, b"").unwrap();

        let rel = rewrite_db_path(db_abs.to_str().unwrap(), &root).unwrap();
        assert_eq!(rel, PathBuf::from("../keitora-worktrees/shared/tasks.db"));
    }

    // --- Project-root derivation (Keitora worktree layout) ---

    #[test]
    fn derives_main_repo_root_from_worktrees_layout() {
        // Legacy file under `<base>/keitora-worktrees/shared/` ⇒ root `<base>/keitora`.
        let base = PathBuf::from("/Users/x/code");
        let legacy = base
            .join("keitora-worktrees")
            .join("shared")
            .join("task-config.json");
        assert_eq!(derive_project_root(&legacy), base.join("keitora"));

        // A different prefix is handled too.
        let legacy2 = base
            .join("myapp-worktrees")
            .join("shared")
            .join("task-config.json");
        assert_eq!(derive_project_root(&legacy2), base.join("myapp"));
    }
}
