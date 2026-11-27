//! Rail affordances that hand a worktree to the OS: open its folder in the file manager,
//! or start a shell rooted there: PowerShell on Windows, Terminal on macOS, and on Linux the
//! user's terminal emulator (see [`terminal_candidates`]).
//!
//! The frontend names a worktree by registry key + worktree name, never by path; the
//! directory is resolved here from the project registry, so neither command can be pointed
//! at an arbitrary location.

use std::path::{Path, PathBuf};
use std::process::Command;

use keitora_tasks::{ProjectConfig, UserConfig};
use tauri::AppHandle;

/// Open the worktree's root folder in the OS file manager.
#[tauri::command(async)]
pub fn open_worktree_folder(
    app: AppHandle,
    project: String,
    worktree: String,
) -> Result<(), String> {
    let dir = resolve_worktree_dir(&project, &worktree)?;
    crate::external_open::open_path(&app, &dir.to_string_lossy())
        .map_err(|e| format!("Could not open folder: {e}"))
}

/// Start a new shell window whose working directory is the worktree's root.
#[tauri::command(async)]
pub fn open_worktree_shell(project: String, worktree: String) -> Result<(), String> {
    let dir = resolve_worktree_dir(&project, &worktree)?;
    spawn_shell(&dir).map_err(|e| format!("Could not start a shell: {e}"))
}

fn resolve_worktree_dir(project: &str, worktree: &str) -> Result<PathBuf, String> {
    let user = UserConfig::load().map_err(|e| e.to_string())?;
    let pref = user
        .projects
        .get(project)
        .ok_or_else(|| format!("Project {project} is not registered"))?;
    let config = keitora_tasks::effective_config(pref).map_err(|e| e.to_string())?;
    let dir = configured_worktree_path(&config, worktree)?;
    if !dir.is_dir() {
        return Err(format!("{} is not a folder", dir.display()));
    }
    Ok(dir)
}

/// The directory of a worktree the project actually configures. `worktree_path` alone
/// would join any name onto the worktrees root, so unknown names are rejected first.
fn configured_worktree_path(config: &ProjectConfig, worktree: &str) -> Result<PathBuf, String> {
    let known = worktree == config.main_worktree()
        || config
            .worktree_entries()
            .iter()
            .any(|entry| entry.name == worktree);
    if !known {
        return Err(format!("Worktree {worktree} is not configured"));
    }
    Ok(config.worktree_path(worktree))
}

#[cfg(windows)]
fn spawn_shell(dir: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    // The app has no console of its own; give the shell a fresh window.
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    // Prefer PowerShell 7 when installed, else the in-box Windows PowerShell.
    let mut last_err = None;
    for exe in ["pwsh.exe", "powershell.exe"] {
        match Command::new(exe)
            .arg("-NoLogo")
            .arg("-NoExit")
            .current_dir(dir)
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
        {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("no PowerShell found")))
}

#[cfg(target_os = "macos")]
fn spawn_shell(dir: &Path) -> std::io::Result<()> {
    let child = Command::new("open")
        .arg("-a")
        .arg("Terminal")
        .arg(dir)
        .spawn()?;
    reap(child);
    Ok(())
}

/// Wait for `child` on a thread of its own, so once it exits it does not linger as a zombie
/// until Keitora does.
#[cfg(unix)]
fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || child.wait());
}

/// How long a terminal gets to fail before it counts as started. A launcher such as
/// `xdg-terminal-exec` with no terminal configured starts fine and exits with an error at once.
#[cfg(all(unix, not(target_os = "macos")))]
const START_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

#[cfg(all(unix, not(target_os = "macos")))]
fn spawn_shell(dir: &Path) -> std::io::Result<()> {
    let terminal = std::env::var("TERMINAL").ok();
    spawn_first(
        terminal_candidates(terminal.as_deref(), dir),
        dir,
        START_GRACE,
    )
}

/// Start the first of `candidates` that is installed and does not fail within `grace`.
#[cfg(all(unix, not(target_os = "macos")))]
fn spawn_first(
    candidates: Vec<(String, Vec<std::ffi::OsString>)>,
    dir: &Path,
    grace: std::time::Duration,
) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut failed = Vec::new();
    for (program, args) in candidates {
        let mut cmd = Command::new(&program);
        // Detached stdio: the terminal's own logging would otherwise land in Keitora's log.
        cmd.args(&args)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(target_os = "linux")]
        keitora_tasks::appimage_env::scrub(&mut cmd);
        match cmd.spawn() {
            Ok(child) => match early_failure(child, grace) {
                None => return Ok(()),
                Some(status) => failed.push(format!("{program} ({status})")),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing.push(program),
            Err(e) => return Err(e),
        }
    }
    let mut msg = String::from("no terminal emulator could be started");
    if !failed.is_empty() {
        msg.push_str(&format!("; {} exited with an error", failed.join(", ")));
    }
    if !missing.is_empty() {
        msg.push_str(&format!("; not installed: {}", missing.join(", ")));
    }
    msg.push_str("; set $TERMINAL to yours");
    Err(std::io::Error::other(msg))
}

/// The exit status of a `child` that fails within `grace`; `None` once it is still running at
/// the deadline or has exited cleanly (a launcher that handed off to the terminal). A child
/// still running is reaped in the background.
#[cfg(all(unix, not(target_os = "macos")))]
fn early_failure(
    mut child: std::process::Child,
    grace: std::time::Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (!status.success()).then_some(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            _ => {
                reap(child);
                return None;
            }
        }
    }
}

/// Terminals tried after `$TERMINAL`, in order: the freedesktop default-terminal launcher, the
/// Debian alternative, then common emulators, each with its flag for the starting directory.
/// The directory is also the child's working directory, which covers the launchers and xterm.
#[cfg(all(unix, not(target_os = "macos")))]
const KNOWN_TERMINALS: &[&str] = &[
    "xdg-terminal-exec",
    "x-terminal-emulator",
    "ghostty",
    "foot",
    "kitty",
    "alacritty",
    "wezterm",
    "ptyxis",
    "gnome-terminal",
    "kgx",
    "konsole",
    "xfce4-terminal",
    "tilix",
    "xterm",
];

/// The arguments that open `program` in `dir`, by the program's file name. Unknown programs get
/// none and rely on inheriting the working directory.
#[cfg(all(unix, not(target_os = "macos")))]
fn working_dir_args(program: &str, dir: &Path) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let name = Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program);
    let joined = |flag: &str| {
        let mut arg = OsString::from(flag);
        arg.push(dir.as_os_str());
        vec![arg]
    };
    let split = |flags: &[&str]| {
        let mut args: Vec<OsString> = flags.iter().map(OsString::from).collect();
        args.push(dir.as_os_str().to_owned());
        args
    };
    match name {
        "ghostty" | "foot" | "gnome-terminal" | "kgx" | "xfce4-terminal" | "tilix" => {
            joined("--working-directory=")
        }
        "ptyxis" => {
            let mut args = vec![OsString::from("--new-window")];
            args.extend(joined("--working-directory="));
            args
        }
        "kitty" => split(&["--directory"]),
        "alacritty" => split(&["--working-directory"]),
        "wezterm" => split(&["start", "--cwd"]),
        "konsole" => split(&["--workdir"]),
        _ => Vec::new(),
    }
}

/// Every `(program, args)` to try, in order: `$TERMINAL` (a program, optionally with its own
/// arguments) when set, then [`KNOWN_TERMINALS`] without repeating it.
#[cfg(all(unix, not(target_os = "macos")))]
fn terminal_candidates(
    terminal: Option<&str>,
    dir: &Path,
) -> Vec<(String, Vec<std::ffi::OsString>)> {
    let mut out: Vec<(String, Vec<std::ffi::OsString>)> = Vec::new();
    if let Some(mut words) = terminal.map(str::split_whitespace) {
        if let Some(program) = words.next() {
            let own: Vec<std::ffi::OsString> = words.map(Into::into).collect();
            // A user's own arguments may already choose a directory; only add ours without them.
            let args = if own.is_empty() {
                working_dir_args(program, dir)
            } else {
                own
            };
            out.push((program.to_string(), args));
        }
    }
    for &program in KNOWN_TERMINALS {
        if !out.iter().any(|(p, _)| p == program) {
            out.push((program.to_string(), working_dir_args(program, dir)));
        }
    }
    out
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod terminal_tests {
    use super::{terminal_candidates, KNOWN_TERMINALS};
    use std::ffi::OsString;
    use std::path::Path;

    const DIR: &str = "/repos/proj-wt/alpha";

    fn args(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn candidates(terminal: Option<&str>) -> Vec<(String, Vec<OsString>)> {
        terminal_candidates(terminal, Path::new(DIR))
    }

    #[test]
    fn without_terminal_env_tries_the_launchers_then_known_emulators() {
        let c = candidates(None);
        let names: Vec<&str> = c.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(names, KNOWN_TERMINALS);
        assert_eq!(c[0], ("xdg-terminal-exec".into(), vec![]));
        assert_eq!(c[1], ("x-terminal-emulator".into(), vec![]));
    }

    #[test]
    fn each_emulator_gets_its_working_directory_flag() {
        let c = candidates(None);
        let find = |name: &str| c.iter().find(|(p, _)| p == name).unwrap().1.clone();
        assert_eq!(
            find("ghostty"),
            args(&["--working-directory=/repos/proj-wt/alpha"])
        );
        assert_eq!(
            find("foot"),
            args(&["--working-directory=/repos/proj-wt/alpha"])
        );
        assert_eq!(find("kitty"), args(&["--directory", DIR]));
        assert_eq!(find("alacritty"), args(&["--working-directory", DIR]));
        assert_eq!(find("wezterm"), args(&["start", "--cwd", DIR]));
        assert_eq!(find("konsole"), args(&["--workdir", DIR]));
        assert_eq!(
            find("ptyxis"),
            args(&["--new-window", "--working-directory=/repos/proj-wt/alpha"])
        );
        assert_eq!(find("xterm"), args(&[]));
    }

    #[test]
    fn terminal_env_comes_first_and_is_not_repeated() {
        let c = candidates(Some("/usr/bin/foot"));
        assert_eq!(
            c[0],
            (
                "/usr/bin/foot".into(),
                args(&["--working-directory=/repos/proj-wt/alpha"])
            )
        );
        let c = candidates(Some("kitty"));
        assert_eq!(c[0].0, "kitty");
        assert_eq!(c.iter().filter(|(p, _)| p == "kitty").count(), 1);
    }

    #[test]
    fn terminal_env_with_its_own_arguments_keeps_them() {
        let c = candidates(Some("kitty --single-instance"));
        assert_eq!(c[0], ("kitty".into(), args(&["--single-instance"])));
    }

    #[test]
    fn blank_terminal_env_is_ignored() {
        assert_eq!(candidates(Some("  ")), candidates(None));
    }

    fn sh(script: &str) -> (String, Vec<OsString>) {
        ("sh".into(), args(&["-c", script]))
    }

    #[test]
    fn a_launcher_that_fails_at_once_falls_through_to_the_next_terminal() {
        let tmp = std::env::temp_dir();
        let grace = std::time::Duration::from_secs(5);
        let c = vec![
            sh("exit 3"),
            ("keitora-no-such-terminal-6f1c".into(), vec![]),
            sh("exit 0"),
        ];
        assert!(super::spawn_first(c, &tmp, grace).is_ok());
        // Still running at the deadline: started.
        let c = vec![sh("sleep 2")];
        let short = std::time::Duration::from_millis(50);
        assert!(super::spawn_first(c, &tmp, short).is_ok());
    }

    #[test]
    fn nothing_started_names_what_failed_and_what_is_missing() {
        let tmp = std::env::temp_dir();
        let grace = std::time::Duration::from_secs(5);
        let c = vec![
            sh("exit 3"),
            ("keitora-no-such-terminal-6f1c".into(), vec![]),
        ];
        let err = super::spawn_first(c, &tmp, grace).unwrap_err().to_string();
        assert!(err.contains("sh (exit status: 3)"), "{err}");
        assert!(
            err.contains("not installed: keitora-no-such-terminal-6f1c"),
            "{err}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::configured_worktree_path;
    use keitora_tasks::{ProjectConfig, WorktreeEntry};
    use std::path::PathBuf;

    fn config_with(entries: Vec<WorktreeEntry>) -> ProjectConfig {
        let mut c = ProjectConfig::scaffold(&PathBuf::from("/repos/proj"));
        c.root = PathBuf::from("/repos/proj");
        c.effective_worktrees = Some(entries);
        c
    }

    #[test]
    fn main_worktree_resolves_to_the_project_root() {
        let c = config_with(vec![]);
        assert_eq!(
            configured_worktree_path(&c, "proj").unwrap(),
            PathBuf::from("/repos/proj")
        );
    }

    #[test]
    fn configured_worktree_resolves_to_its_stored_path() {
        let c = config_with(vec![WorktreeEntry::new("alpha", "/repos/proj-wt/alpha")]);
        assert_eq!(
            configured_worktree_path(&c, "alpha").unwrap(),
            PathBuf::from("/repos/proj-wt/alpha")
        );
    }

    #[test]
    fn unknown_worktree_is_rejected() {
        let c = config_with(vec![WorktreeEntry::new("alpha", "/repos/proj-wt/alpha")]);
        assert!(configured_worktree_path(&c, "bravo").is_err());
        assert!(configured_worktree_path(&c, "../elsewhere").is_err());
    }
}
