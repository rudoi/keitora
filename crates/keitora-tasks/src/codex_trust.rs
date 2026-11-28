//! Read-only look at whether Codex trusts a project, from the user's Codex config
//! (`<codex home>/config.toml`, `[projects."<path>"] trust_level = "trusted"`).
//!
//! Keitora never writes that file (it is the user's own, global config); this only
//! reports, so a missing trust can be shown instead of discovered as a silent hook.

use std::path::Path;

use crate::path_key::{is_within, normalize_path, FOLD_CASE};

/// What Codex's config says about a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectTrust {
    /// The project (or a root above it) has `trust_level = "trusted"`.
    Trusted,
    /// It is listed with another `trust_level` (the value, or `""` when absent).
    Untrusted(String),
    /// No `projects` entry covers it (or the config file does not exist).
    NotListed,
    /// The config could not be read or parsed (why).
    Unreadable(String),
    /// Not a Codex worktree.
    NotApplicable,
}

/// [`codex_project_trust`] with the case rule given.
pub fn codex_project_trust_with(
    config_toml: Option<&str>,
    path: &Path,
    fold_case: bool,
) -> ProjectTrust {
    let Some(text) = config_toml else {
        return ProjectTrust::NotListed;
    };
    let doc: toml::Table = match toml::from_str(text) {
        Ok(d) => d,
        Err(e) => return ProjectTrust::Unreadable(format!("invalid TOML: {e}")),
    };
    let Some(projects) = doc.get("projects").and_then(toml::Value::as_table) else {
        return ProjectTrust::NotListed;
    };
    let want = normalize_path(&path.to_string_lossy(), true, fold_case);
    // The most specific listed root that covers the path wins.
    let best = projects
        .iter()
        .map(|(k, v)| (normalize_path(k, true, fold_case), v))
        .filter(|(k, _)| is_within(&want, k))
        .max_by_key(|(k, _)| k.len());
    match best {
        None => ProjectTrust::NotListed,
        Some((_, v)) => match v.get("trust_level").and_then(toml::Value::as_str) {
            Some("trusted") => ProjectTrust::Trusted,
            Some(other) => ProjectTrust::Untrusted(other.to_string()),
            None => ProjectTrust::Untrusted(String::new()),
        },
    }
}

/// Codex's trust of `path` from the text of its `config.toml` (`None`: the file
/// does not exist). Codex trusts a git root, so an entry for any ancestor counts.
pub fn codex_project_trust(config_toml: Option<&str>, path: &Path) -> ProjectTrust {
    codex_project_trust_with(config_toml, path, FOLD_CASE)
}

/// [`codex_project_trust`] reading `config` (a missing file is `NotListed`, any other
/// read error `Unreadable`).
pub fn read_codex_project_trust(config: &Path, path: &Path) -> ProjectTrust {
    match std::fs::read_to_string(config) {
        Ok(text) => codex_project_trust(Some(&text), path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProjectTrust::NotListed,
        Err(e) => ProjectTrust::Unreadable(format!("reading {} failed: {e}", config.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trust(text: &str, path: &str) -> ProjectTrust {
        codex_project_trust_with(Some(text), Path::new(path), false)
    }

    #[test]
    fn codex_project_trust_matches_the_worktree_path() {
        let cfg = r#"
            model = "x"
            [projects."/Users/me/proj"]
            trust_level = "trusted"
        "#;
        assert_eq!(trust(cfg, "/Users/me/proj"), ProjectTrust::Trusted);
        assert_eq!(trust(cfg, "/Users/me/proj/"), ProjectTrust::Trusted);
    }

    #[test]
    fn codex_project_trust_matches_an_ancestor_root() {
        let cfg = r#"
            [projects."/Users/me/repos"]
            trust_level = "trusted"
            [projects."/Users/me/repos/proj/wt/bravo"]
            trust_level = "untrusted"
        "#;
        assert_eq!(
            trust(cfg, "/Users/me/repos/proj/wt/alpha"),
            ProjectTrust::Trusted
        );
        // The most specific entry wins.
        assert_eq!(
            trust(cfg, "/Users/me/repos/proj/wt/bravo"),
            ProjectTrust::Untrusted("untrusted".into())
        );
    }

    #[test]
    fn codex_project_trust_other_level_is_untrusted() {
        let cfg = r#"
            [projects."/Users/me/proj"]
            trust_level = "untrusted"
            [projects."/Users/me/other-proj"]
            model = "y"
        "#;
        assert_eq!(
            trust(cfg, "/Users/me/proj"),
            ProjectTrust::Untrusted("untrusted".into())
        );
        assert_eq!(
            trust(cfg, "/Users/me/other-proj"),
            ProjectTrust::Untrusted(String::new()),
            "listed without a trust level"
        );
    }

    #[test]
    fn codex_project_trust_unlisted_and_missing_file_are_not_listed() {
        let cfg = r#"
            [projects."/Users/me/other-proj"]
            trust_level = "trusted"
        "#;
        assert_eq!(trust(cfg, "/Users/me/proj"), ProjectTrust::NotListed);
        assert_eq!(
            trust("model = \"x\"", "/Users/me/proj"),
            ProjectTrust::NotListed
        );
        assert_eq!(
            codex_project_trust(None, Path::new("/Users/me/proj")),
            ProjectTrust::NotListed
        );
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            read_codex_project_trust(&tmp.path().join("config.toml"), Path::new("/Users/me/proj")),
            ProjectTrust::NotListed
        );
        // A file that is there is read.
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, cfg).unwrap();
        assert_eq!(
            read_codex_project_trust(&file, Path::new("/Users/me/other-proj")),
            ProjectTrust::Trusted
        );
    }

    #[test]
    fn codex_project_trust_invalid_toml_is_unreadable() {
        assert!(matches!(
            trust(
                "[projects.\"/Users/me/proj\"\ntrust_level = ",
                "/Users/me/proj"
            ),
            ProjectTrust::Unreadable(_)
        ));
        // A directory where the file should be cannot be read.
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_codex_project_trust(tmp.path(), Path::new("/Users/me/proj")),
            ProjectTrust::Unreadable(_)
        ));
    }

    #[test]
    fn codex_project_trust_normalises_separators_case_and_verbatim_prefix() {
        let cfg = r#"
            [projects.'C:\Users\x\repos\proj']
            trust_level = "trusted"
        "#;
        let folded = |p: &str| codex_project_trust_with(Some(cfg), Path::new(p), true);
        assert_eq!(folded(r"C:\Users\x\repos\proj"), ProjectTrust::Trusted);
        assert_eq!(folded("C:/Users/x/repos/proj/"), ProjectTrust::Trusted);
        assert_eq!(
            folded(r"\\?\C:\Users\x\repos\proj\wt\alpha"),
            ProjectTrust::Trusted
        );
        assert_eq!(folded("c:/users/X/Repos/PROJ"), ProjectTrust::Trusted);
        // Without case folding (Linux) a case difference is another path.
        assert_eq!(
            codex_project_trust_with(Some(cfg), Path::new("c:/users/x/repos/proj"), false),
            ProjectTrust::NotListed
        );
    }

    #[test]
    fn codex_project_trust_does_not_match_a_sibling_prefix() {
        let cfg = r#"
            [projects."/Users/me/proj"]
            trust_level = "trusted"
        "#;
        assert_eq!(trust(cfg, "/Users/me/proj2"), ProjectTrust::NotListed);
        assert_eq!(trust(cfg, "/Users/me/proj-b/x"), ProjectTrust::NotListed);
    }
}
