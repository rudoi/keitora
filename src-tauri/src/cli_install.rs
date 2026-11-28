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

use keitora_tasks::{mirror_file, mirror_is_stale, same_file_name};
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

/// Locate a `keitora` CLI binary to link when it isn't on PATH: the main worktree's
/// build tree (dogfood when the app runs from a different cwd). Callers should try
/// `hooks::app_bundled_cli` first (covers packaged + dev).
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

/// Whether the app refreshes the managed CLI in place: the Windows copy goes stale
/// when the app updates; the unix symlink always points at the bundled CLI.
pub const REFRESHES_MANAGED_COPY: bool = cfg!(windows);

/// The managed location of the CLI on PATH: the `~/.local/bin/keitora` symlink on
/// unix, and on Windows the copy the app mirrors to
/// `%LOCALAPPDATA%/keitora/bin/keitora.exe` (see `keitora_tasks::windows_keitora_bin_dir`).
pub fn default_link_path() -> Option<PathBuf> {
    if cfg!(windows) {
        return keitora_tasks::managed_keitora_bin_dir().map(|d| d.join("keitora.exe"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin/keitora"))
}

/// What [`refresh_managed_copy_if_stale`] did.
#[derive(Debug, PartialEq)]
pub enum Refresh {
    /// The `keitora` on PATH is not the managed copy (or there is none, or no bundled
    /// CLI to copy from): left alone.
    NotManaged,
    /// The managed copy already matches the bundled CLI.
    Fresh,
    /// The managed copy was replaced with the bundled CLI.
    Refreshed,
    /// The managed copy is stale but could not be replaced.
    Failed(String),
}

/// Refresh the managed CLI copy from the CLI bundled with the app, but ONLY when the
/// `keitora` resolved on PATH (`on_path`) IS that managed copy (`managed`): a `keitora`
/// the user put on PATH some other way is never touched. Without this a same-version
/// upgrade left an old copy on PATH that lacked newer subcommands.
pub fn refresh_managed_copy_if_stale(
    on_path: Option<&Path>,
    managed: Option<&Path>,
    bundled: Option<&Path>,
) -> Refresh {
    let (Some(on_path), Some(managed), Some(bundled)) = (on_path, managed, bundled) else {
        return Refresh::NotManaged;
    };
    if !same_file_name(on_path, managed) || same_file_name(bundled, managed) {
        return Refresh::NotManaged;
    }
    if !mirror_is_stale(bundled, managed) {
        return Refresh::Fresh;
    }
    match mirror_file(bundled, managed) {
        Ok(()) => Refresh::Refreshed,
        Err(e) => Refresh::Failed(e.to_string()),
    }
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

// These exercise unix symlink install + exec-bit semantics (`0o755`, `:`-PATH);
// the windows CLI-install path (D3) is cfg-stubbed and gets its own tests in
// stage 050.
#[cfg(test)]
mod portable_tests {
    use super::*;

    /// Give `p` a distinct, fixed modified time so staleness is deterministic.
    fn set_mtime(p: &Path, secs: u64) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        std::fs::OpenOptions::new()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn refresh_replaces_a_stale_managed_copy_on_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bundled = tmp.path().join("app").join("keitora-cli.exe");
        let managed = tmp.path().join("bin").join("keitora.exe");
        std::fs::create_dir_all(bundled.parent().unwrap()).unwrap();
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        std::fs::write(&bundled, b"new cli").unwrap();
        std::fs::write(&managed, b"old cli").unwrap(); // same length, older
        set_mtime(&bundled, 2_000_000_000);
        set_mtime(&managed, 1_000_000_000);
        assert!(mirror_is_stale(&bundled, &managed));

        let r = refresh_managed_copy_if_stale(Some(&managed), Some(&managed), Some(&bundled));
        assert_eq!(r, Refresh::Refreshed);
        assert_eq!(std::fs::read(&managed).unwrap(), b"new cli");
        // The copy carries the source's mtime, so a second pass is a no-op.
        assert!(!mirror_is_stale(&bundled, &managed));
        assert_eq!(
            refresh_managed_copy_if_stale(Some(&managed), Some(&managed), Some(&bundled)),
            Refresh::Fresh
        );
        // No temp or moved-aside files are left behind.
        let names: Vec<_> = std::fs::read_dir(managed.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("keitora.exe")]);
    }

    #[test]
    fn refresh_detects_a_size_change() {
        let tmp = tempfile::tempdir().unwrap();
        let bundled = tmp.path().join("keitora-cli.exe");
        let managed = tmp.path().join("keitora.exe");
        std::fs::write(&bundled, b"a longer new cli").unwrap();
        std::fs::write(&managed, b"old").unwrap();
        set_mtime(&bundled, 1_000_000_000);
        set_mtime(&managed, 1_000_000_000);
        assert!(mirror_is_stale(&bundled, &managed));
    }

    #[test]
    fn refresh_never_touches_a_keitora_on_path_that_is_not_the_managed_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let bundled = tmp.path().join("keitora-cli.exe");
        let managed = tmp.path().join("keitora.exe");
        let users_own = tmp.path().join("own").join("keitora.exe");
        std::fs::create_dir_all(users_own.parent().unwrap()).unwrap();
        std::fs::write(&bundled, b"new cli").unwrap();
        std::fs::write(&managed, b"old cli").unwrap();
        std::fs::write(&users_own, b"mine").unwrap();
        set_mtime(&bundled, 2_000_000_000);

        let r = refresh_managed_copy_if_stale(Some(&users_own), Some(&managed), Some(&bundled));
        assert_eq!(r, Refresh::NotManaged);
        assert_eq!(std::fs::read(&managed).unwrap(), b"old cli");
        assert_eq!(std::fs::read(&users_own).unwrap(), b"mine");
        // Nothing on PATH, or no bundled CLI → nothing to do.
        assert_eq!(
            refresh_managed_copy_if_stale(None, Some(&managed), Some(&bundled)),
            Refresh::NotManaged
        );
        assert_eq!(
            refresh_managed_copy_if_stale(Some(&managed), Some(&managed), None),
            Refresh::NotManaged
        );
    }

    #[cfg(windows)]
    #[test]
    fn refresh_matches_the_managed_path_case_insensitively_on_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let bundled = tmp.path().join("keitora-cli.exe");
        let managed = tmp.path().join("keitora.exe");
        std::fs::write(&bundled, b"new cli!").unwrap();
        std::fs::write(&managed, b"old").unwrap();
        let on_path = PathBuf::from(managed.to_string_lossy().to_uppercase());
        assert_eq!(
            refresh_managed_copy_if_stale(Some(&on_path), Some(&managed), Some(&bundled)),
            Refresh::Refreshed
        );
    }
}

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
