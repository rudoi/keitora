//! Factory function for constructing a [`TaskBackend`] from a [`ProjectConfig`].
//!
//! This is the single, canonical entry point for opening a backend — both the GUI
//! (`src-tauri`) and the CLI (`keitora-cli`) should call `open_backend` rather than
//! constructing a [`SqliteBackend`] directly, so that future backend kinds (e.g.
//! `"github"`) slot in without touching call sites.

use crate::{BackendError, BackendResult, ProjectConfig, SqliteBackend, TaskBackend};

/// Construct a [`TaskBackend`] for the given resolved project config.
///
/// Reads the backend kind from `config.tasks.backend` (defaults to `"sqlite"` for
/// `keitora.toml` files that omit the field).
///
/// # Errors
///
/// - [`BackendError::Config`] — unrecognized backend kind.
/// - [`BackendError::Permanent`] — the SQLite file could not be opened (bad path,
///   permissions, corrupt database).
pub fn open_backend(config: &ProjectConfig) -> BackendResult<Box<dyn TaskBackend>> {
    let tasks = &config.tasks;
    match tasks.backend.as_str() {
        "sqlite" => {
            let db_path = tasks.db_path.to_string_lossy();
            let backend = SqliteBackend::open(db_path.as_ref())
                .map_err(|e| BackendError::Permanent(e.to_string()))?;
            Ok(Box::new(backend))
        }
        "github" => {
            let gh = tasks.github.clone().unwrap_or_default();
            let sidecar_path = config.resolve_sidecar_path().ok_or_else(|| {
                BackendError::Config(
                    "could not resolve a GitHub sidecar path (no home/config dir)".to_string(),
                )
            })?;
            // `GitHubBackend::open` preserves Config/Unavailable/Unauthorized kinds.
            let backend =
                crate::github::backend::GitHubBackend::open(&gh, &sidecar_path, &config.root)?;
            Ok(Box::new(backend))
        }
        other => Err(BackendError::Config(format!(
            "unknown tasks.backend {:?}: only \"sqlite\" is supported",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CliConfig, DispatchConfig, ProjectMeta, ProviderConfig, TaskConfig};
    use std::path::PathBuf;
    use tempfile::NamedTempFile;

    fn project_config_with(db_path: &str, backend: &str) -> ProjectConfig {
        ProjectConfig {
            root: PathBuf::from("/tmp"),
            effective_worktrees: None,
            project: ProjectMeta {
                name: "test".to_string(),
                worktrees_dir: None,
                base_branch: None,
                diff_base: None,
            },
            tasks: TaskConfig {
                db_path: PathBuf::from(db_path),
                worktrees: vec![],
                archive_days: 30,
                log_retention_days: 90,
                backend: backend.to_string(),
                github: None,
            },
            claude: ProviderConfig::default(),
            codex: ProviderConfig::default(),
            antigravity: ProviderConfig::default(),
            cli: CliConfig::default(),
            dispatch: DispatchConfig::default(),
        }
    }

    /// Opening a sqlite backend via the factory returns a working `Box<dyn TaskBackend>`.
    #[test]
    fn factory_returns_sqlite_backend() {
        let f = NamedTempFile::new().unwrap();
        let cfg = project_config_with(f.path().to_str().unwrap(), "sqlite");
        let backend = open_backend(&cfg).unwrap();
        // Trait object is valid — consistency() is synchronous / infallible.
        assert!(matches!(backend.consistency(), crate::Consistency::Strong));
    }

    /// An unrecognized backend kind returns `BackendError::Config` naming it.
    #[test]
    fn factory_unknown_kind_is_config_error() {
        let f = NamedTempFile::new().unwrap();
        let cfg = project_config_with(f.path().to_str().unwrap(), "frobnicate");
        match open_backend(&cfg) {
            Err(crate::BackendError::Config(msg)) => {
                assert!(
                    msg.contains("frobnicate") && msg.contains("sqlite"),
                    "error should name the unknown kind: {msg}"
                );
            }
            Err(other) => panic!("expected Config error, got: {other}"),
            Ok(_) => panic!("expected an error for unknown backend kind, got Ok"),
        }
    }

    /// `"github"` is now a real backend kind: the factory no longer rejects it
    /// with the "only sqlite is supported" message. In this test environment
    /// (no owner/repo/project resolvable) construction fails with a *Config*
    /// error preserving its kind — never flattened to Permanent, and never the
    /// old unknown-kind message.
    #[test]
    fn factory_github_is_a_real_kind() {
        let f = NamedTempFile::new().unwrap();
        let mut cfg = project_config_with(f.path().to_str().unwrap(), "github");
        // Point root at a non-repo temp dir so git-remote resolution fails
        // deterministically (no `origin`), and omit owner/repo/project.
        let dir = tempfile::tempdir().unwrap();
        cfg.root = dir.path().to_path_buf();
        cfg.tasks.github = None;
        match open_backend(&cfg) {
            Err(crate::BackendError::Config(msg)) => {
                assert!(
                    !msg.contains("only \"sqlite\" is supported"),
                    "github must no longer be the unknown-kind path: {msg}"
                );
            }
            // If the host happens to construct (gh present, repo resolvable), that
            // is also acceptable — the point is it is no longer the unknown-kind
            // rejection. Any non-Config error other than the unknown-kind message
            // would be a regression in kind preservation.
            Err(other) => panic!("expected a Config error from construction, got: {other}"),
            Ok(_) => { /* constructed successfully in this environment */ }
        }
    }

    /// Deserializing a TOML `[tasks]` block that has no `backend` key yields `"sqlite"`.
    #[test]
    fn task_config_default_backend_is_sqlite() {
        let toml_str = r#"db_path = "/tmp/test.db""#;
        let tc: TaskConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(tc.backend, "sqlite");
    }

    /// The legacy `Config` shim exposes `backend()` and returns `"sqlite"` by default.
    #[test]
    fn config_shim_backend_is_sqlite_by_default() {
        use crate::Config;
        let f = NamedTempFile::new().unwrap();
        // Write a minimal keitora.toml that has no `backend` field.
        let toml_content = format!(
            "[project]\nname = \"test\"\n\n[tasks]\ndb_path = \"{}\"\n",
            // Escape backslashes so a Windows path is a valid TOML basic string;
            // no-op on Unix.
            f.path().to_str().unwrap().replace('\\', "\\\\")
        );
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("keitora.toml");
        std::fs::write(&toml_path, toml_content).unwrap();
        let pc = crate::ProjectConfig::load(dir.path()).unwrap();
        let cfg = Config::from_project(pc);
        assert_eq!(cfg.backend(), "sqlite");
    }
}
