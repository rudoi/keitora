use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use keitora_tasks::ProjectConfig;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

#[tauri::command(async)]
pub fn open_terminal_file(
    app: AppHandle,
    project: String,
    worktree: String,
    path: String,
) -> Result<(), String> {
    let config = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let root = worktree_root(&config, &project, &worktree)?;
    let target = validate_target(&root, &path)?;
    // Some default handlers mis-handle the `\\?\` prefix canonicalization adds on Windows.
    let target = dunce::simplified(&target);
    app.opener()
        .open_path(target.to_string_lossy().into_owned(), None::<String>)
        .map_err(|e| format!("Could not open file: {e}"))
}

fn worktree_root(config: &ProjectConfig, project: &str, worktree: &str) -> Result<PathBuf, String> {
    if config.project.name != project {
        return Err("The terminal's project is no longer active".into());
    }
    // worktree_path otherwise falls back to joining any supplied name.
    if worktree != config.main_worktree()
        && !config
            .worktree_entries()
            .iter()
            .any(|entry| entry.name == worktree)
    {
        return Err("The terminal's worktree is not configured".into());
    }
    Ok(config.worktree_path(worktree))
}

fn validate_path_text(path: &str) -> Result<(), String> {
    let normalized = path.replace('\\', "/");
    if normalized.is_empty()
        || normalized.chars().any(char::is_control)
        || normalized.starts_with("//")
    {
        return Err("Only local filesystem paths can be opened".into());
    }
    let bytes = normalized.as_bytes();
    let drive =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/';
    if normalized[if drive { 2 } else { 0 }..].contains(':') {
        return Err(
            "File URIs, drive-relative paths, and alternate data streams are unsupported".into(),
        );
    }
    // Windows trims these characters and may otherwise select a different file.
    if normalized
        .split('/')
        .any(|part| part != "." && part != ".." && (part.ends_with('.') || part.ends_with(' ')))
    {
        return Err("File paths with trailing dots or spaces are unsupported".into());
    }
    if normalized.split('/').any(|part| {
        let stem = part
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end()
            .to_ascii_uppercase();
        matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
    }) {
        return Err("Windows device names cannot be opened from terminal links".into());
    }
    Ok(())
}

fn validate_extension(path: &Path) -> Result<(), String> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(
        extension.as_str(),
        "exe"
            | "com"
            | "bat"
            | "cmd"
            | "ps1"
            | "psm1"
            | "psd1"
            | "vbs"
            | "vbe"
            | "js"
            | "jse"
            | "wsf"
            | "wsh"
            | "hta"
            | "scr"
            | "pif"
            | "cpl"
            | "msi"
            | "msp"
            | "mst"
            | "lnk"
            | "url"
            | "scf"
            | "reg"
            | "inf"
            | "application"
            | "appref-ms"
            | "app"
            | "command"
            | "desktop"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "py"
            | "pyw"
            | "pl"
            | "rb"
            | "jar"
            | "workflow"
            | "action"
            | "webloc"
            | "website"
            | "appx"
            | "appxbundle"
            | "msix"
            | "msixbundle"
            // Other handlers that run code, mount images, or install on open.
            | "msc"
            | "chm"
            | "hlp"
            | "xll"
            | "gadget"
            | "ps1xml"
            | "wsc"
            | "sct"
            | "cab"
            | "diagcab"
            | "iso"
            | "img"
            | "vhd"
            | "vhdx"
            | "jnlp"
            | "settingcontent-ms"
            | "library-ms"
            | "search-ms"
            | "searchconnector-ms"
            | "terminal"
            | "tool"
            | "scpt"
            | "applescript"
            | "pkg"
            | "mpkg"
            | "dmg"
    ) {
        return Err(
            "Executable files, scripts, and shortcuts cannot be opened from terminal links".into(),
        );
    }
    Ok(())
}

fn validate_target(root: &Path, path: &str) -> Result<PathBuf, String> {
    validate_path_text(path)?;
    validate_extension(Path::new(path))?;
    let root = root
        .canonicalize()
        .map_err(|e| format!("Worktree is unavailable: {e}"))?;
    // Relative paths resolve against the worktree; absolute ones may point anywhere local.
    if Path::new(path).is_relative() && is_network_path(&root) {
        return Err("Network worktrees cannot open local terminal file links".into());
    }
    let candidate = root.join(path);
    let target = candidate
        .canonicalize()
        .map_err(|e| format!("File is unavailable: {e}"))?;
    validate_canonical_target(&target)?;
    Ok(target)
}

/// Whether a canonical path lives on a network share (or a raw device namespace).
#[cfg(windows)]
fn is_network_path(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    matches!(path.components().next(), Some(Component::Prefix(prefix))
        if matches!(prefix.kind(), Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _) | Prefix::DeviceNS(_)))
}

#[cfg(not(windows))]
fn is_network_path(_path: &Path) -> bool {
    false
}

fn validate_canonical_target(target: &Path) -> Result<(), String> {
    if is_network_path(target) {
        return Err("Files on network shares cannot be opened from terminal links".into());
    }
    if !target.is_file() {
        return Err("Only existing regular files can be opened".into());
    }
    validate_extension(target)?;
    let mut file = File::open(target).map_err(|e| format!("Cannot inspect file: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file
            .metadata()
            .map_err(|e| e.to_string())?
            .permissions()
            .mode()
            & 0o111
            != 0
        {
            return Err("Executable files cannot be opened from terminal links".into());
        }
    }
    let mut header = [0; 4];
    let count = file
        .read(&mut header)
        .map_err(|e| format!("Cannot inspect file: {e}"))?;
    let header = &header[..count];
    if header.starts_with(b"MZ")
        || header.starts_with(b"\x7fELF")
        || header.starts_with(b"#!")
        || matches!(
            header,
            [0xfe, 0xed, 0xfa, 0xce | 0xcf]
                | [0xce | 0xcf, 0xfa, 0xed, 0xfe]
                | [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
        )
    {
        return Err("Executable files and scripts cannot be opened from terminal links".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("keitora-links-{:016x}", rand::random::<u64>()));
            std::fs::create_dir_all(path.join("worktree")).unwrap();
            Self(path)
        }
        fn root(&self) -> PathBuf {
            self.0.join("worktree")
        }
        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.root().join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn terminal_links_accept_relative_absolute_spaces_and_literal_percent() {
        let fixture = Fixture::new();
        for name in ["source.rs", "source file.ts", "literal%20file.md"] {
            let path = fixture.write(name, b"source text");
            assert_eq!(
                validate_target(&fixture.root(), name).unwrap(),
                path.canonicalize().unwrap()
            );
            assert_eq!(
                validate_target(&fixture.root(), path.to_str().unwrap()).unwrap(),
                path.canonicalize().unwrap()
            );
        }
    }

    #[test]
    fn terminal_links_reject_missing_and_directories() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.root().join("folder")).unwrap();
        for path in ["missing.txt", ".", "folder", "../missing.txt"] {
            assert!(validate_target(&fixture.root(), path).is_err(), "{path}");
        }
    }

    #[test]
    fn terminal_links_open_files_outside_the_worktree() {
        let fixture = Fixture::new();
        let outside = fixture.0.join("outside.txt");
        std::fs::write(&outside, "outside").unwrap();
        std::fs::create_dir(fixture.0.join("worktree-sibling")).unwrap();
        let sibling = fixture.0.join("worktree-sibling/file.txt");
        std::fs::write(&sibling, "outside").unwrap();
        for (path, expected) in [
            ("../outside.txt".to_string(), &outside),
            (outside.to_str().unwrap().to_string(), &outside),
            ("../worktree-sibling/file.txt".to_string(), &sibling),
            (sibling.to_str().unwrap().to_string(), &sibling),
        ] {
            assert_eq!(
                validate_target(&fixture.root(), &path).unwrap(),
                expected.canonicalize().unwrap(),
                "{path}"
            );
        }
        // Safety checks still apply outside the worktree.
        let script = fixture.0.join("run.bat");
        std::fs::write(&script, "text").unwrap();
        assert!(validate_target(&fixture.root(), script.to_str().unwrap()).is_err());
        assert!(validate_target(&fixture.root(), "../run.bat").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn terminal_links_refuse_network_targets() {
        for path in [
            r"\\?\UNC\server\share\file.txt",
            r"\\server\share\file.txt",
            r"\\.\PhysicalDrive0",
        ] {
            assert!(is_network_path(Path::new(path)), "{path}");
            assert!(
                validate_canonical_target(Path::new(path)).is_err(),
                "{path}"
            );
        }
        for path in [r"\\?\C:\repo\file.txt", r"C:\repo\file.txt"] {
            assert!(!is_network_path(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn terminal_links_reject_unsafe_path_syntax() {
        for path in [
            "",
            "\\\\server\\share\\file.txt",
            "\\\\?\\C:\\file.txt",
            "//server/share/file.txt",
            "C:relative.txt",
            "file:///C:/file.txt",
            "C:/file.txt:stream",
            "file.txt:12",
            "file.exe.",
            "file.exe ",
            "a\0b",
            "CON.txt",
            "folder/NUL",
            "COM1.log",
            "LPT².txt",
        ] {
            assert!(validate_path_text(path).is_err(), "{path:?}");
        }
        for path in [
            "C:/repo/file.ts",
            "C:\\repo\\file.ts",
            "./src/file.rs",
            "../file.txt",
            "/tmp/file.txt",
        ] {
            assert!(validate_path_text(path).is_ok(), "{path}");
        }
    }

    #[test]
    fn terminal_links_reject_scripts_shortcuts_and_disguised_binaries() {
        let fixture = Fixture::new();
        for name in [
            "run.EXE",
            "run.bat",
            "run.cmd",
            "run.ps1",
            "run.js",
            "run.lnk",
            "run.url",
            "run.desktop",
            "run.py",
            "console.msc",
            "help.CHM",
            "disk.iso",
            "launch.jnlp",
            "run.settingcontent-ms",
            "run.terminal",
            "installer.dmg",
        ] {
            fixture.write(name, b"text");
            assert!(validate_target(&fixture.root(), name).is_err(), "{name}");
        }
        for bytes in [
            &b"MZbinary"[..],
            &b"\x7fELFbinary"[..],
            &b"#!/bin/sh"[..],
            &[0xcf, 0xfa, 0xed, 0xfe][..],
        ] {
            fixture.write("disguised.txt", bytes);
            assert!(validate_target(&fixture.root(), "disguised.txt").is_err());
        }
    }

    #[test]
    fn terminal_links_check_project_and_worktree_identity() {
        let fixture = Fixture::new();
        let mut config = ProjectConfig::scaffold(&fixture.root());
        assert!(worktree_root(&config, "wrong", &config.main_worktree()).is_err());
        assert!(worktree_root(&config, &config.project.name, "../outside").is_err());
        assert_eq!(
            worktree_root(&config, &config.project.name, &config.main_worktree()).unwrap(),
            config.root
        );
        config.tasks.worktrees.push(
            serde_json::from_value(serde_json::json!({
                "name": "configured", "path": fixture.root()
            }))
            .unwrap(),
        );
        assert_eq!(
            worktree_root(&config, &config.project.name, "configured").unwrap(),
            fixture.root()
        );
    }

    #[test]
    fn terminal_links_validate_resolved_alias_targets() {
        let fixture = Fixture::new();
        let executable = fixture.write("run.exe", b"text").canonicalize().unwrap();
        let outside = fixture.0.join("outside.txt");
        std::fs::write(&outside, "outside").unwrap();
        assert!(validate_canonical_target(&outside.canonicalize().unwrap()).is_ok());
        assert!(validate_canonical_target(&executable).is_err());
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "requires Windows Developer Mode or symlink privilege; run with --ignored"
    )]
    fn terminal_links_follow_symlink_escape_but_reject_executable_alias() {
        let fixture = Fixture::new();
        let outside = fixture.0.join("outside.txt");
        std::fs::write(&outside, "outside").unwrap();
        let executable = fixture.write("run.exe", b"text");
        for (name, target) in [("escape.txt", outside), ("alias.txt", executable)] {
            let link = fixture.root().join(name);
            #[cfg(windows)]
            let result = std::os::windows::fs::symlink_file(&target, &link);
            #[cfg(unix)]
            let result = std::os::unix::fs::symlink(&target, &link);
            result.expect("symlink test requires Developer Mode or symlink permission");
        }
        assert!(validate_target(&fixture.root(), "escape.txt").is_ok());
        assert!(validate_target(&fixture.root(), "alias.txt").is_err());
    }
}
