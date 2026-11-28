//! Paths as comparable keys: one normalisation for every place that matches a path
//! from one source (herdr, a tool's log, Codex's config, the hook log) against a
//! path Keitora holds.

/// Whether paths compare without case on this platform (Windows, macOS).
pub const FOLD_CASE: bool = cfg!(any(windows, target_os = "macos"));

/// `p` as a comparable key:
/// - a Windows verbatim prefix dropped (`\\?\C:\x` -> `C:\x`, `\\?\UNC\srv\share` ->
///   `\\srv\share`, in either slash form);
/// - `\` turned into `/` when `backslash_separates` (on unix a backslash is an
///   ordinary filename character);
/// - trailing `/` trimmed, keeping a bare root (`/`) and a drive root (`C:/`);
/// - lowercased when `fold_case`.
pub fn normalize_path(p: &str, backslash_separates: bool, fold_case: bool) -> String {
    let p = if let Some(rest) = p
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| p.strip_prefix("//?/UNC/"))
    {
        format!(r"\\{rest}")
    } else {
        p.strip_prefix(r"\\?\")
            .or_else(|| p.strip_prefix("//?/"))
            .unwrap_or(p)
            .to_string()
    };
    let mut s = if backslash_separates {
        p.replace('\\', "/")
    } else {
        p
    };
    while s.len() > 1 && s.ends_with('/') && !s.ends_with(":/") {
        s.pop();
    }
    if fold_case {
        s.to_lowercase()
    } else {
        s
    }
}

/// Whether `child` is `parent` or lies under it (whole components only: `/a/proj`
/// is not under `/a/pro`). Both already [`normalize_path`]ed.
pub fn is_within(child: &str, parent: &str) -> bool {
    child == parent
        || child
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('/') || parent.ends_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_verbatim_prefixes_in_either_slash_form() {
        assert_eq!(
            normalize_path(r"\\?\D:\repos\wt", true, false),
            "D:/repos/wt"
        );
        assert_eq!(
            normalize_path("//?/D:/repos/wt", true, false),
            "D:/repos/wt"
        );
        assert_eq!(
            normalize_path(r"\\?\UNC\srv\share\a", true, false),
            "//srv/share/a"
        );
        assert_eq!(
            normalize_path("//?/UNC/srv/share/a", true, false),
            "//srv/share/a"
        );
    }

    #[test]
    fn trims_trailing_separators_but_keeps_roots() {
        assert_eq!(normalize_path("/repos/wt//", true, false), "/repos/wt");
        assert_eq!(normalize_path(r"D:\repos\wt\", true, false), "D:/repos/wt");
        assert_eq!(normalize_path("/", true, false), "/");
        assert_eq!(normalize_path("C:/", true, false), "C:/");
    }

    #[test]
    fn backslashes_stay_when_they_do_not_separate() {
        assert_eq!(normalize_path(r"/repos/a\b", false, false), r"/repos/a\b");
        assert_eq!(normalize_path(r"/repos/wt\", false, false), r"/repos/wt\");
    }

    #[test]
    fn folds_case_only_when_asked() {
        assert_eq!(normalize_path("C:/Users/X", true, true), "c:/users/x");
        assert_eq!(normalize_path("C:/Users/X", true, false), "C:/Users/X");
    }

    #[test]
    fn is_within_matches_whole_components() {
        assert!(is_within("/Users/me/proj", "/Users/me/proj"));
        assert!(is_within("/Users/me/proj/a", "/Users/me/proj"));
        assert!(!is_within("/Users/me/projx", "/Users/me/proj"));
        assert!(is_within("C:/a", "C:/"));
    }
}
