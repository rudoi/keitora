// First-run installer for the `keitora` CLI.
//
// Session hooks resolve `keitora` from `keitora` on PATH, then a `[cli].path` override, then
// a worktree build as a last resort (see `hooks::resolve_keitora_bin`). For a packaged app
// in an arbitrary
// project, the friendly default is "`keitora` on PATH" — but the user may never have put
// it there. This module detects that and offers to symlink a located `keitora` binary
// into `~/.local/bin`, so hooks (and the `keitora` command) work everywhere.
//
// The Tauri layer (src/lib.rs) is a thin shell over these pure helpers; the logic
// here is unit-tested.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// True if `p` is an existing regular file with any execute bit set. The binary we
/// link onto PATH must be runnable — a non-executable file would make `keitora` fail and
/// the `[ -x ]`-guarded session hooks silently no-op.
#[cfg(unix)]
fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

/// Windows has no execute bit; treat any existing regular file as runnable.
/// PATHEXT-aware runnability is a stage-050 concern.
#[cfg(windows)]
fn is_exec(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// What the frontend needs to know about the `keitora` CLI's install state.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CliStatus {
    /// `"on_path"` (already resolvable), `"installable"` (missing but a binary was
    /// located to link), `"missing"` (missing and nothing to link), or
    /// `"installed"` (just linked).
    pub state: String,
    /// The resolved-on-PATH path, the located source, or the freshly-created link,
    /// depending on `state`.
    pub path: Option<String>,
    /// A human note, e.g. when the link dir is not on PATH.
    pub note: Option<String>,
}

/// The decision the installer should take, computed purely from inputs.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// `keitora` already resolves on PATH at this absolute path — nothing to do.
    AlreadyOnPath(String),
    /// `keitora` is missing but this binary can be linked onto PATH.
    Install(PathBuf),
    /// `keitora` is missing and no binary was located to link.
    NoBinary,
}

/// Decide what to do, given whether `keitora` is on PATH and whether a linkable binary
/// was located. Pure — unit-tested. PATH presence wins (idempotent: re-running after
/// install reports `AlreadyOnPath`).
pub fn decide(on_path: Option<&str>, located: Option<&Path>) -> Action {
    match (on_path, located) {
        (Some(p), _) => Action::AlreadyOnPath(p.to_string()),
        (None, Some(src)) => Action::Install(src.to_path_buf()),
        (None, None) => Action::NoBinary,
    }
}

/// A `keitora` CLI binary sitting next to the running app executable, if present.
/// Two forms, both covered: under `tauri dev` the per-worktree build is the sibling
/// `keitora` (`target/debug/keitora`); in a packaged `.app` the bundled sidecar is
/// `keitora-cli` (Tauri strips the target-triple suffix at bundle time, and the sidecar
/// cannot be named `keitora` — that clashes with the `keitora` app package name). Either
/// way the installer links it onto PATH as `keitora`. Pure (takes the exe path).
pub fn sidecar_candidate(current_exe: &Path) -> Option<PathBuf> {
    let dir = current_exe.parent()?;
    for name in ["keitora", "keitora-cli"] {
        let p = dir.join(name);
        if is_exec(&p) {
            return Some(p);
        }
    }
    None
}

/// Locate a `keitora` CLI binary to link when it isn't on PATH: the main worktree's
/// build tree (dogfood when the app runs from a different cwd). Callers should try
/// [`sidecar_candidate`] first (covers packaged + dev).
///
/// As in `hooks::resolve_keitora_bin`, `target/<profile>/keitora` is trusted as the CLI ONLY
/// when a sibling `keitora-app` is present — on a pre-rename branch `keitora` is the GUI app
/// and must NOT be linked onto PATH (that would point every PATH-resolved `keitora` — and
/// the session hooks — at the GUI). Returns the first valid candidate.
pub fn locate_installable(main_worktree: &Path) -> Option<PathBuf> {
    for prof in ["release", "debug"] {
        let cli = main_worktree.join(format!("target/{prof}/keitora"));
        let app = main_worktree.join(format!("target/{prof}/keitora-app"));
        // `is_file()` (not `exists()`) for the marker: a directory named `keitora-app`
        // must not satisfy the guard. `is_exec` for the CLI: it gets symlinked onto PATH.
        if is_exec(&cli) && app.is_file() {
            return Some(cli);
        }
    }
    None
}

/// The default link location for the CLI: `~/.local/bin/keitora`.
pub fn default_link_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin/keitora"))
}

/// True if `dir` appears as a segment of the colon-separated `path_env`.
pub fn dir_on_path(dir: &Path, path_env: Option<&str>) -> bool {
    let dir_s = dir.to_string_lossy();
    path_env
        .map(|p| {
            p.split(':')
                .any(|seg| !seg.is_empty() && Path::new(seg) == Path::new(dir_s.as_ref()))
        })
        .unwrap_or(false)
}

/// Create (or refresh) a symlink at `link` pointing to `src`.
///
/// Safety: we create the parent dir; if `link` already exists we replace it ONLY
/// when it is itself a symlink (one we presumably own) — never a regular file or
/// directory (that would clobber unrelated user data). Returns the link path on
/// success.
pub fn install_symlink(src: &Path, link: &Path) -> Result<PathBuf, String> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    match std::fs::symlink_metadata(link) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                std::fs::remove_file(link)
                    .map_err(|e| format!("replacing existing symlink {}: {e}", link.display()))?;
            } else {
                return Err(format!(
                    "{} already exists and is not a symlink — leaving it untouched",
                    link.display()
                ));
            }
        }
        Err(_) => { /* nothing there — fall through to create */ }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, link)
            .map_err(|e| format!("linking {} -> {}: {e}", link.display(), src.display()))?;
        Ok(link.to_path_buf())
    }
    #[cfg(not(unix))]
    {
        let _ = src;
        Err("CLI install via symlink is not supported on this platform".to_string())
    }
}

/// Whether the `keitora` at `found` is an older build than the app's own CLI at
/// `bundled`: a different file, modified earlier, whose bytes differ. The same file (a
/// link to it), an identical copy or a newer build is fine. Unreadable files aren't
/// judged stale. Compares builds rather than version strings, which dev builds share.
pub fn is_older_build(found: &Path, bundled: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(found), std::fs::canonicalize(bundled)) {
        if a == b {
            return false;
        }
    }
    let (Ok(f), Ok(b)) = (std::fs::metadata(found), std::fs::metadata(bundled)) else {
        return false;
    };
    let older = match (f.modified(), b.modified()) {
        (Ok(fm), Ok(bm)) => fm < bm,
        _ => false,
    };
    if !older {
        return false;
    }
    let same = f.len() == b.len()
        && matches!(
            (std::fs::read(found), std::fs::read(bundled)),
            (Ok(x), Ok(y)) if x == y
        );
    !same
}

/// The app's warning for `keitora` copies older than its own build.
pub fn stale_cli_message(paths: &[String]) -> String {
    format!(
        "The keitora CLI at {} is an older build than this app, so agents and session \
         hooks still run the old one. If a session is using it, close that session and \
         restart Keitora to update it; otherwise replace or remove the old copy.",
        paths.join(" and ")
    )
}

#[cfg(test)]
mod staleness_tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn write_at(p: &Path, bytes: &[u8], age: Duration) {
        std::fs::write(p, bytes).unwrap();
        let f = std::fs::File::options().write(true).open(p).unwrap();
        f.set_modified(SystemTime::now() - age).unwrap();
    }

    #[test]
    fn an_older_different_build_is_stale_and_nothing_else_is() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bundled = tmp.path().join("bundled");
        let found = tmp.path().join("found");
        write_at(&bundled, b"new build", Duration::from_secs(10));

        write_at(&found, b"old build", Duration::from_secs(3600));
        assert!(is_older_build(&found, &bundled));
        // Same bytes (a copy that kept an older timestamp): current.
        write_at(&found, b"new build", Duration::from_secs(3600));
        assert!(!is_older_build(&found, &bundled));
        // A newer, different build (a local dev build): not older.
        write_at(&found, b"dev build", Duration::from_secs(1));
        assert!(!is_older_build(&found, &bundled));
        // The bundled file itself, and a missing one, are never stale.
        assert!(!is_older_build(&bundled, &bundled));
        assert!(!is_older_build(&tmp.path().join("missing"), &bundled));
    }

    #[test]
    fn stale_message_names_every_copy() {
        let msg = stale_cli_message(&["C:/a/keitora.exe".into(), "C:/b/keitora.exe".into()]);
        assert!(
            msg.contains("C:/a/keitora.exe and C:/b/keitora.exe"),
            "{msg}"
        );
        assert!(msg.contains("restart Keitora"), "{msg}");
    }
}

// These exercise unix symlink install + exec-bit semantics (`0o755`, `:`-PATH);
// the windows CLI-install path (D3) is cfg-stubbed and gets its own tests in
// stage 050.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mk_exec(p: &Path) {
        std::fs::write(p, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn decide_prefers_path_then_install_then_nothing() {
        let src = PathBuf::from("/some/keitora");
        assert_eq!(
            decide(Some("/usr/bin/keitora"), Some(&src)),
            Action::AlreadyOnPath("/usr/bin/keitora".into())
        );
        assert_eq!(decide(None, Some(&src)), Action::Install(src.clone()));
        assert_eq!(decide(None, None), Action::NoBinary);
    }

    #[test]
    fn sidecar_candidate_finds_sibling_dev_or_bundled() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("keitora-app");
        std::fs::write(&exe, b"app").unwrap();
        // No sibling yet.
        assert_eq!(sidecar_candidate(&exe), None);
        // A non-executable sibling is ignored (it would fail the [ -x ] hook guard).
        let bundled = tmp.path().join("keitora-cli");
        std::fs::write(&bundled, b"cli").unwrap();
        assert_eq!(sidecar_candidate(&exe), None);
        // Bundled form: executable sibling `keitora-cli` → found.
        mk_exec(&bundled);
        assert_eq!(sidecar_candidate(&exe), Some(bundled));
        // Dev form: an executable sibling `keitora` takes precedence (per-worktree build).
        let dev = tmp.path().join("keitora");
        mk_exec(&dev);
        assert_eq!(sidecar_candidate(&exe), Some(dev));
    }

    #[test]
    fn locate_installable_prefers_release_then_debug() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("target/release")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        // A bare `keitora` (no sibling `keitora-app`) is the pre-rename GUI app → NOT linked.
        mk_exec(&root.join("target/debug/keitora"));
        assert_eq!(
            locate_installable(root),
            None,
            "keitora without keitora-app is the GUI app"
        );
        // Debug build with its `keitora-app` sibling → trusted (the renamed CLI).
        std::fs::write(root.join("target/debug/keitora-app"), b"app").unwrap();
        assert_eq!(
            locate_installable(root),
            Some(root.join("target/debug/keitora"))
        );
        // Release (with sibling) wins over debug.
        mk_exec(&root.join("target/release/keitora"));
        std::fs::write(root.join("target/release/keitora-app"), b"app").unwrap();
        assert_eq!(
            locate_installable(root),
            Some(root.join("target/release/keitora"))
        );
        // Neither → None.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(locate_installable(empty.path()), None);
    }

    #[test]
    fn dir_on_path_matches_exact_segment() {
        let dir = Path::new("/home/u/.local/bin");
        assert!(dir_on_path(dir, Some("/usr/bin:/home/u/.local/bin:/bin")));
        assert!(!dir_on_path(dir, Some("/usr/bin:/bin")));
        assert!(!dir_on_path(dir, None));
        // A substring that isn't a full segment must not match.
        assert!(!dir_on_path(dir, Some("/home/u/.local/bin/extra")));
    }

    #[test]
    fn install_symlink_creates_and_refreshes_but_refuses_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("keitora-bin");
        mk_exec(&src);
        let link = tmp.path().join("bin/keitora");

        // Fresh create (parent dir made on the fly).
        let made = install_symlink(&src, &link).unwrap();
        assert_eq!(made, link);
        assert_eq!(std::fs::read_link(&link).unwrap(), src);

        // Re-running replaces our symlink (idempotent), still points at src.
        let src2 = tmp.path().join("keitora-bin-2");
        mk_exec(&src2);
        install_symlink(&src2, &link).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), src2);

        // Refuses to clobber a regular file sitting at the link path.
        let regular = tmp.path().join("regular");
        std::fs::write(&regular, b"important").unwrap();
        let err = install_symlink(&src, &regular).unwrap_err();
        assert!(err.contains("not a symlink"), "got: {err}");
        assert_eq!(std::fs::read(&regular).unwrap(), b"important");
    }
}
