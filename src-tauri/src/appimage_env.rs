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
use std::process::Command;

/// Variables the AppImage runtime sets that only describe the image itself.
const RUNTIME_VARS: &[&str] = &["APPDIR", "APPIMAGE", "ARGV0", "OWD"];

/// One change to a child's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvChange {
    Remove(OsString),
    Set(OsString, OsString),
}

/// Strip the AppImage's environment from `cmd` when Keitora runs as an AppImage.
pub fn scrub(cmd: &mut Command) {
    let Some(appdir) = std::env::var_os("APPDIR").filter(|d| !d.is_empty()) else {
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

/// The changes that remove every trace of `appdir` from `vars`: the runtime's own variables go,
/// path lists lose their entries under `appdir` (and go entirely if nothing is left), and the
/// empty `GTK_THEME` the GTK hook exports (which forces GTK's fallback theme) goes.
pub fn plan(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    appdir: &OsStr,
) -> Vec<EnvChange> {
    let appdir = appdir.as_bytes();
    let appdir = appdir.strip_suffix(b"/").unwrap_or(appdir);
    let mut changes = Vec::new();
    for (key, value) in vars {
        let k = key.as_bytes();
        let runtime_var = RUNTIME_VARS.iter().any(|r| r.as_bytes() == k);
        if runtime_var || (k == b"GTK_THEME" && value.is_empty()) {
            changes.push(EnvChange::Remove(key));
        } else if value
            .as_bytes()
            .split(|&b| b == b':')
            .any(|entry| under(entry, appdir))
        {
            let kept: Vec<&[u8]> = value
                .as_bytes()
                .split(|&b| b == b':')
                .filter(|entry| !entry.is_empty() && !under(entry, appdir))
                .collect();
            if kept.is_empty() {
                changes.push(EnvChange::Remove(key));
            } else {
                let joined = kept.join(&b':');
                changes.push(EnvChange::Set(key, OsStr::from_bytes(&joined).to_owned()));
            }
        }
    }
    changes
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
