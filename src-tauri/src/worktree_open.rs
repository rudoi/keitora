//! Rail affordances that hand a worktree to the OS: open its folder in the file manager,
//! or start a shell (PowerShell on Windows) rooted there.
//!
//! The frontend names a worktree by registry key + worktree name, never by path; the
//! directory is resolved here from the project registry, so neither command can be pointed
//! at an arbitrary location.

use std::path::{Path, PathBuf};
use std::process::Command;

use keitora_tasks::{ProjectConfig, UserConfig};
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

/// Open the worktree's root folder in the OS file manager.
#[tauri::command(async)]
pub fn open_worktree_folder(
    app: AppHandle,
    project: String,
    worktree: String,
) -> Result<(), String> {
    let dir = resolve_worktree_dir(&project, &worktree)?;
    app.opener()
        .open_path(dir.to_string_lossy().into_owned(), None::<String>)
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
    Command::new("open")
        .arg("-a")
        .arg("Terminal")
        .arg(dir)
        .spawn()
        .map(|_| ())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn spawn_shell(dir: &Path) -> std::io::Result<()> {
    Command::new("x-terminal-emulator")
        .current_dir(dir)
        .spawn()
        .map(|_| ())
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
