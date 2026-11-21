//! Undo the AppImage runtime's environment for programs Keitora starts on the user's behalf.
//!
//! An AppImage's `AppRun` points `LD_LIBRARY_PATH`, `PATH`, `PYTHONHOME`, `XDG_DATA_DIRS`, the
//! GTK/GIO module paths and more at the mounted image, so Keitora's own WebKit finds its bundled
//! libraries. A terminal or shell started from Keitora inherits all of it and loads the image's
//! libraries instead of the system's, which can crash or mis-theme it. Keitora itself still needs
//! those values, so they are removed per child [`Command`], never from the process.
//!
//! Outside an AppImage (`$APPDIR` unset) nothing changes.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Variables the AppImage runtime sets that only describe the image itself.
const RUNTIME_VARS: &[&str] = &["APPDIR", "APPIMAGE", "ARGV0", "OWD"];

/// One change to a child's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvChange {
    Remove(OsString),
    Set(OsString, OsString),
}

/// Whether this process is itself running from an AppImage. `$APPIMAGE` alone is not enough:
/// another AppImage app leaks it (and `$APPDIR`) to everything it starts, so the running binary
/// must also sit inside `$APPDIR`, the same test Tauri applies before trusting the variables.
pub fn running_as_appimage() -> bool {
    is_appimage(
        std::env::var_os("APPIMAGE").as_deref(),
        std::env::var_os("APPDIR").as_deref(),
        std::env::current_exe().ok().as_deref(),
    )
}

/// [`running_as_appimage`] from its inputs.
pub fn is_appimage(appimage: Option<&OsStr>, appdir: Option<&OsStr>, exe: Option<&Path>) -> bool {
    let (Some(appimage), Some(appdir), Some(exe)) = (appimage, appdir, exe) else {
        return false;
    };
    if appimage.is_empty() || appdir.is_empty() {
        return false;
    }
    // `current_exe` resolves symlinks, so compare against the resolved mount point too.
    let appdir = Path::new(appdir);
    let resolved = std::fs::canonicalize(appdir).unwrap_or_else(|_| PathBuf::from(appdir));
    exe.starts_with(appdir) || exe.starts_with(resolved)
}

/// `$APPDIR`, when set and non-empty.
fn current_appdir() -> Option<OsString> {
    std::env::var_os("APPDIR").filter(|d| !d.is_empty())
}

/// Strip the AppImage's environment from `cmd` when Keitora runs as an AppImage.
pub fn scrub(cmd: &mut Command) {
    let Some(appdir) = current_appdir() else {
        return;
    };
    for change in plan(std::env::vars_os(), &appdir) {
        match change {
            EnvChange::Remove(k) => {
                cmd.env_remove(k);
            }
            EnvChange::Set(k, v) => {
                cmd.env(k, v);
            }
        }
    }
}

/// `value` (a `:`-separated path list) without its entries under `$APPDIR`; `None` when nothing
/// is left. For a list Keitora derived itself, such as a login shell's `PATH` probed from inside
/// the image. Outside an AppImage it is returned unchanged.
pub fn clean_path_list(value: &OsStr) -> Option<OsString> {
    let Some(appdir) = current_appdir() else {
        return Some(value.to_owned());
    };
    match strip_list(value, trim_dir(&appdir)) {
        Stripped::Unchanged => Some(value.to_owned()),
        Stripped::Kept(v) => Some(v),
        Stripped::Empty => None,
    }
}

/// The changes that remove every trace of `appdir` from `vars`: the runtime's own variables go,
/// path lists lose their entries under `appdir` (and go entirely if nothing is left), and the
/// empty `GTK_THEME` the GTK hook exports (which forces GTK's fallback theme) goes.
pub fn plan(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    appdir: &OsStr,
) -> Vec<EnvChange> {
    let appdir = trim_dir(appdir);
    let mut changes = Vec::new();
    for (key, value) in vars {
        let k = key.as_bytes();
        let runtime_var = RUNTIME_VARS.iter().any(|r| r.as_bytes() == k);
        if runtime_var || (k == b"GTK_THEME" && value.is_empty()) {
            changes.push(EnvChange::Remove(key));
            continue;
        }
        match strip_list(&value, appdir) {
            Stripped::Unchanged => {}
            Stripped::Kept(v) => changes.push(EnvChange::Set(key, v)),
            Stripped::Empty => changes.push(EnvChange::Remove(key)),
        }
    }
    changes
}

/// What [`strip_list`] left of a path list.
enum Stripped {
    /// No entry was under the directory.
    Unchanged,
    Kept(OsString),
    Empty,
}

/// `value` split on `:` without the entries under `appdir` (and without empty entries, which
/// AppRun's `%s/usr/lib:%s` leaves behind when the variable was unset).
fn strip_list(value: &OsStr, appdir: &[u8]) -> Stripped {
    let entries = || value.as_bytes().split(|&b| b == b':');
    if !entries().any(|entry| under(entry, appdir)) {
        return Stripped::Unchanged;
    }
    let kept: Vec<&[u8]> = entries()
        .filter(|entry| !entry.is_empty() && !under(entry, appdir))
        .collect();
    if kept.is_empty() {
        Stripped::Empty
    } else {
        Stripped::Kept(OsStr::from_bytes(&kept.join(&b':')).to_owned())
    }
}

fn trim_dir(dir: &OsStr) -> &[u8] {
    let dir = dir.as_bytes();
    dir.strip_suffix(b"/").unwrap_or(dir)
}

/// Whether `path` is `dir` or inside it.
fn under(path: &[u8], dir: &[u8]) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.is_empty() || rest[0] == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPDIR: &str = "/tmp/.mount_KeitoX";

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn plan_of(vars: &[(&str, &str)]) -> Vec<EnvChange> {
        plan(vars.iter().map(|(k, v)| (os(k), os(v))), OsStr::new(APPDIR))
    }

    #[test]
    fn appimage_needs_both_variables_and_the_binary_inside_the_mount() {
        let image = Some(OsStr::new("/home/me/Keitora_0.5.0_amd64.AppImage"));
        let dir = Some(OsStr::new(APPDIR));
        let inside = Path::new("/tmp/.mount_KeitoX/usr/bin/keitora-app");
        assert!(is_appimage(image, dir, Some(inside)));
        // Variables leaked by another AppImage app into an rpm/deb install.
        assert!(!is_appimage(
            image,
            dir,
            Some(Path::new("/usr/bin/keitora-app"))
        ));
        // A sibling mount whose name merely starts with the same characters.
        let sibling = Path::new("/tmp/.mount_KeitoXY/usr/bin/keitora-app");
        assert!(!is_appimage(image, dir, Some(sibling)));
        assert!(!is_appimage(None, dir, Some(inside)));
        assert!(!is_appimage(image, None, Some(inside)));
        assert!(!is_appimage(Some(OsStr::new("")), dir, Some(inside)));
        assert!(!is_appimage(image, dir, None));
    }

    fn strip(value: &str, appdir: &str) -> Option<String> {
        match strip_list(OsStr::new(value), trim_dir(OsStr::new(appdir))) {
            Stripped::Unchanged => Some(value.to_string()),
            Stripped::Kept(v) => Some(v.into_string().unwrap()),
            Stripped::Empty => None,
        }
    }

    #[test]
    fn a_probed_login_path_loses_the_image_entries() {
        // A login shell started from inside the image prints the image's dirs first.
        let login =
            "/tmp/.mount_KeitoX/usr/bin/:/tmp/.mount_KeitoX/usr/sbin/:/home/me/.local/bin:/usr/bin";
        assert_eq!(
            strip(login, APPDIR).as_deref(),
            Some("/home/me/.local/bin:/usr/bin")
        );
        // A trailing slash on $APPDIR matches the same entries.
        assert_eq!(
            strip(login, "/tmp/.mount_KeitoX/").as_deref(),
            Some("/home/me/.local/bin:/usr/bin")
        );
        assert_eq!(strip("/tmp/.mount_KeitoX/usr/bin", APPDIR), None);
        // Untouched lists keep their exact text, empty entries included.
        assert_eq!(
            strip("/usr/bin::/bin", APPDIR).as_deref(),
            Some("/usr/bin::/bin")
        );
    }

    #[test]
    fn path_lists_keep_only_the_system_entries() {
        let changes = plan_of(&[
            (
                "PATH",
                "/tmp/.mount_KeitoX/usr/bin/:/tmp/.mount_KeitoX/bin/:/home/me/.local/bin:/usr/bin",
            ),
            (
                "XDG_DATA_DIRS",
                "/tmp/.mount_KeitoX/usr/share:/usr/share:/tmp/.mount_KeitoX/usr/share/:/usr/local/share",
            ),
        ]);
        assert_eq!(
            changes,
            vec![
                EnvChange::Set(os("PATH"), os("/home/me/.local/bin:/usr/bin")),
                EnvChange::Set(os("XDG_DATA_DIRS"), os("/usr/share:/usr/local/share")),
            ]
        );
    }

    #[test]
    fn variables_that_only_point_into_the_image_are_removed() {
        // AppRun's `%s/usr/lib/:%s` leaves a trailing empty entry when the variable was unset.
        let changes = plan_of(&[
            (
                "LD_LIBRARY_PATH",
                "/tmp/.mount_KeitoX/usr/lib/:/tmp/.mount_KeitoX/lib64/:",
            ),
            ("PYTHONHOME", "/tmp/.mount_KeitoX/usr/"),
            ("GIO_MODULE_DIR", "/tmp/.mount_KeitoX//usr/lib/gio/modules"),
        ]);
        assert_eq!(
            changes,
            vec![
                EnvChange::Remove(os("LD_LIBRARY_PATH")),
                EnvChange::Remove(os("PYTHONHOME")),
                EnvChange::Remove(os("GIO_MODULE_DIR")),
            ]
        );
    }

    #[test]
    fn runtime_variables_and_the_empty_gtk_theme_are_removed() {
        let changes = plan_of(&[
            ("APPDIR", APPDIR),
            ("APPIMAGE", "/home/me/Keitora_0.5.0_amd64.AppImage"),
            ("ARGV0", "./Keitora_0.5.0_amd64.AppImage"),
            ("OWD", "/home/me"),
            ("GTK_THEME", ""),
        ]);
        assert_eq!(
            changes,
            ["APPDIR", "APPIMAGE", "ARGV0", "OWD", "GTK_THEME"]
                .map(|k| EnvChange::Remove(os(k)))
                .to_vec()
        );
    }

    #[test]
    fn unrelated_variables_are_left_alone() {
        let changes = plan_of(&[
            ("HOME", "/home/me"),
            ("GTK_THEME", "Adwaita:dark"),
            ("PATH", "/usr/bin:/bin"),
            // A sibling mount whose name merely starts with the same characters.
            ("SOME_DIR", "/tmp/.mount_KeitoXY/usr"),
        ]);
        assert_eq!(changes, vec![]);
    }
}
