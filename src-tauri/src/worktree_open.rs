//! The worktree menu's "Open folder" and "Open terminal here": hand a worktree's
//! directory to the OS file manager, or start a system terminal window in it.
//!
//! The frontend names the worktree by registry key + worktree name and the directory
//! is resolved here, so the webview can't point either command at an arbitrary path.
//! Any registered project works, not only the active one.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use keitora_tasks::UserConfig;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

/// Open the worktree's directory in the OS file manager.
#[tauri::command(async)]
pub fn open_worktree_folder(
    app: AppHandle,
    project: String,
    worktree: String,
) -> Result<(), String> {
    let user = UserConfig::load().map_err(|e| e.to_string())?;
    let dir = worktree_dir(&user, &project, &worktree)?;
    app.opener()
        .open_path(dir.to_string_lossy().into_owned(), None::<String>)
        .map_err(|e| format!("Could not open folder: {e}"))
}

/// Start a system terminal window whose working directory is the worktree.
#[tauri::command(async)]
pub fn open_worktree_terminal(project: String, worktree: String) -> Result<(), String> {
    let user = UserConfig::load().map_err(|e| e.to_string())?;
    let dir = worktree_dir(&user, &project, &worktree)?;
    let terminal_env = std::env::var("TERMINAL").ok();
    launch_terminal(
        &terminal_launchers(std::env::consts::OS, &dir, terminal_env.as_deref()),
        &dir,
    )
}

/// The directory of `worktree` in the registered project `project` (registry key). The
/// name must be the main checkout or a configured worktree, and the directory must exist.
fn worktree_dir(user: &UserConfig, project: &str, worktree: &str) -> Result<PathBuf, String> {
    let pref = user
        .projects
        .get(project)
        .ok_or_else(|| format!("{project} is no longer registered"))?;
    let config = keitora_tasks::effective_config(pref).map_err(|e| e.to_string())?;
    // worktree_path otherwise falls back to joining any supplied name.
    if worktree != config.main_worktree()
        && !config
            .worktree_entries()
            .iter()
            .any(|entry| entry.name == worktree)
    {
        return Err(format!(
            "{worktree} is not a configured worktree of {project}"
        ));
    }
    let dir = config.worktree_path(worktree);
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    Ok(dir)
}

/// One way to start a terminal: a program and its arguments, run with the worktree as
/// its working directory.
#[derive(Debug, PartialEq, Eq)]
struct Launcher {
    program: String,
    args: Vec<String>,
}

impl Launcher {
    fn new(program: &str, args: &[&str]) -> Self {
        Launcher {
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }
}

/// The terminals to try for `os` (`std::env::consts::OS`), in order; the first that
/// starts wins. `terminal_env` is `$TERMINAL`, which leads on Linux and the BSDs.
fn terminal_launchers(os: &str, dir: &Path, terminal_env: Option<&str>) -> Vec<Launcher> {
    let d = dir.to_string_lossy();
    match os {
        "windows" => {
            let mut v = Vec::new();
            // Windows Terminal splits its command line on `;` even inside a quoted
            // argument, so a directory containing one goes straight to PowerShell.
            if !d.contains(';') {
                v.push(Launcher::new("wt.exe", &["-d", &d]));
            }
            v.push(Launcher::new("powershell.exe", &["-NoLogo", "-NoExit"]));
            v
        }
        "macos" => vec![Launcher::new("open", &["-a", "Terminal", &d])],
        _ => {
            let mut v = Vec::new();
            if let Some(t) = terminal_env.map(str::trim).filter(|t| !t.is_empty()) {
                v.push(Launcher::new(t, &[]));
            }
            v.extend([
                Launcher::new("x-terminal-emulator", &[]),
                Launcher::new("gnome-terminal", &[&format!("--working-directory={d}")]),
                Launcher::new("konsole", &["--workdir", &d]),
                Launcher::new("xfce4-terminal", &[&format!("--working-directory={d}")]),
                Launcher::new("alacritty", &["--working-directory", &d]),
                Launcher::new("kitty", &["--directory", &d]),
                Launcher::new("wezterm", &["start", "--cwd", &d]),
                Launcher::new("foot", &[]),
                Launcher::new("xterm", &[]),
            ]);
            v
        }
    }
}

/// Start the first launcher that spawns, detached from Keitora's stdio. A missing
/// program moves on to the next; the error names the last failure.
fn launch_terminal(launchers: &[Launcher], dir: &Path) -> Result<(), String> {
    let mut last_err = String::from("no terminal to try");
    for l in launchers {
        let mut cmd = Command::new(&l.program);
        cmd.args(&l.args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // A console of its own: Keitora has none for PowerShell to inherit.
            const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
            cmd.creation_flags(CREATE_NEW_CONSOLE);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                // Reap it when it exits so a closed terminal leaves no zombie behind.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(());
            }
            Err(e) => last_err = format!("{}: {e}", l.program),
        }
    }
    Err(format!("Could not start a terminal ({last_err})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use keitora_tasks::{ProjectRef, WorktreeEntry};

    /// A registry with one file-less project `proj` rooted at `<tmp>/proj`, its task DB
    /// kept in the temp dir, and a worktree `alpha` at `<tmp>/alpha`.
    fn registry(tmp: &Path) -> UserConfig {
        let root = tmp.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(tmp.join("alpha")).unwrap();
        let mut pref = ProjectRef::new(root);
        pref.db_path = Some(tmp.join("proj.db"));
        let mut alpha = WorktreeEntry::name_only("alpha");
        alpha.path = Some(tmp.join("alpha"));
        pref.worktrees = Some(vec![alpha, WorktreeEntry::name_only("bravo")]);
        let mut user = UserConfig::default();
        user.projects.insert("proj".into(), pref);
        user
    }

    #[test]
    fn resolves_the_main_checkout_and_configured_worktrees() {
        let tmp = tempfile::TempDir::new().unwrap();
        let user = registry(tmp.path());
        let main = worktree_dir(&user, "proj", "proj").unwrap();
        assert_eq!(main.file_name().unwrap(), "proj");
        assert!(main.is_dir());
        let alpha = worktree_dir(&user, "proj", "alpha").unwrap();
        assert_eq!(alpha, tmp.path().join("alpha"));
    }

    #[test]
    fn refuses_unknown_projects_unknown_worktrees_and_missing_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let user = registry(tmp.path());
        assert!(worktree_dir(&user, "other-proj", "proj").is_err());
        // Not configured: never joined onto the worktrees dir.
        assert!(worktree_dir(&user, "proj", "charlie").is_err());
        assert!(worktree_dir(&user, "proj", "../alpha").is_err());
        // Configured, but its directory doesn't exist.
        assert!(worktree_dir(&user, "proj", "bravo").is_err());
    }

    #[test]
    fn windows_prefers_windows_terminal_then_powershell() {
        let v = terminal_launchers("windows", Path::new("D:/repos/proj"), Some("ignored"));
        assert_eq!(
            v,
            [
                Launcher::new("wt.exe", &["-d", "D:/repos/proj"]),
                Launcher::new("powershell.exe", &["-NoLogo", "-NoExit"]),
            ]
        );
    }

    #[test]
    fn windows_skips_windows_terminal_for_a_dir_with_a_semicolon() {
        let v = terminal_launchers("windows", Path::new("D:/repos/a;b"), None);
        assert_eq!(
            v,
            [Launcher::new("powershell.exe", &["-NoLogo", "-NoExit"])]
        );
    }

    #[test]
    fn macos_opens_terminal_app_at_the_dir() {
        let v = terminal_launchers("macos", Path::new("/Users/me/proj"), None);
        assert_eq!(
            v,
            [Launcher::new("open", &["-a", "Terminal", "/Users/me/proj"])]
        );
    }

    #[test]
    fn linux_tries_terminal_env_first_then_common_terminals() {
        let v = terminal_launchers("linux", Path::new("/home/me/proj"), Some("my-term"));
        assert_eq!(v[0], Launcher::new("my-term", &[]));
        assert_eq!(v[1], Launcher::new("x-terminal-emulator", &[]));
        assert!(v.contains(&Launcher::new(
            "gnome-terminal",
            &["--working-directory=/home/me/proj"]
        )));
        assert_eq!(v.last().unwrap(), &Launcher::new("xterm", &[]));
        // A blank $TERMINAL is ignored.
        let v = terminal_launchers("linux", Path::new("/home/me/proj"), Some("  "));
        assert_eq!(v[0], Launcher::new("x-terminal-emulator", &[]));
    }

    #[test]
    fn launch_reports_when_no_terminal_starts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = launch_terminal(
            &[Launcher::new("keitora-no-such-terminal", &[])],
            tmp.path(),
        )
        .unwrap_err();
        assert!(err.contains("keitora-no-such-terminal"), "{err}");
        assert!(launch_terminal(&[], tmp.path()).is_err());
    }
}
