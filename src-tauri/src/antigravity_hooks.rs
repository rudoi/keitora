//! The Keitora session hook for the Antigravity CLI (`agy`), installed into the
//! user's global `~/.gemini/config/hooks.json`.
//!
//! `agy` loads hooks from that global file (and per-workspace `.agents/hooks.json`,
//! which Keitora does not write). The file is a JSON object keyed by hook *name*;
//! Keitora owns exactly the `keitora` name and leaves every other one (herdr's,
//! the user's) untouched. Non-tool events take a handler array directly — wrapping
//! them in `{"hooks":[…]}` makes `agy` reject the whole file.
//!
//! The file is shared by every worktree and by `agy` runs outside Keitora, so each
//! command first checks for a Keitora pane, by the `KEITORA_PANE` marker or the
//! `KEITORA_BIN` an older build's panes carry (herdr keeps those alive across a
//! Keitora upgrade), and is a silent no-op with neither. It then runs the absolute path of the global
//! `keitora` CLI (on PATH, the `[cli].path` override, or the app-bundled CLI; never a
//! worktree build), so it works even in a pane where no `KEITORA_BIN` resolved. With
//! no CLI resolvable it falls back to running `$KEITORA_BIN`. Hook commands run
//! under cmd.exe on Windows and a POSIX shell elsewhere.
//!
//! The merge is pure and the writer takes its target path and the write guard as
//! arguments; only the real-home wrappers consult [`crate::global_writes::allowed`].

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// The top-level hook name Keitora owns in the global hooks file.
pub const HOOK_NAME: &str = "keitora";

/// (agy event, arguments after `keitora session`) — the three events Keitora registers.
const HOOK_EVENTS: [(&str, &str); 3] = [
    ("SessionStart", "idle --stdin-payload"),
    ("PreInvocation", "busy --stdin-payload"),
    ("Stop", "idle --stdin-payload --if-fully-idle"),
];

/// Per-handler timeout, in seconds.
const HOOK_TIMEOUT_SECS: u64 = 10;

/// The env var that overrides agy's config directory.
pub const CONFIG_DIR_ENV: &str = "ANTIGRAVITY_CLI_CONFIG_DIR";

/// The command string for one registration; `windows` selects cmd.exe vs POSIX
/// shell syntax. Both forms first test for a Keitora pane (the marker, or the
/// `KEITORA_BIN` of a pane an older build started) and do nothing outside one, print nothing (so `agy` treats the hook as having no decision)
/// and always exit 0, so a failing or missing `keitora` never fails the agent.
///
/// `bin` is the absolute global `keitora` CLI (`\\?\` prefix stripped). With
/// `None` the older `$KEITORA_BIN` form is kept, so nothing regresses where no CLI
/// resolves. The cmd.exe forms group the conditional so `& exit /b 0` always runs.
pub fn hook_command(args: &str, windows: bool, bin: Option<&str>) -> String {
    let marker = crate::hooks::PANE_MARKER_ENV;
    let bin = bin.map(keitora_tasks::strip_extended_prefix_str);
    // Per shell: the test that must pass, and the executable it then runs.
    if windows {
        match &bin {
            // cmd.exe has no `or`: leave early unless either variable is defined.
            Some(bin) => format!(
                "(if not defined {marker} if not defined KEITORA_BIN exit /b 0) & (\"{bin}\" session {args} >nul 2>&1) & exit /b 0"
            ),
            None => format!(
                "(if defined KEITORA_BIN \"%KEITORA_BIN%\" session {args} >nul 2>&1) & exit /b 0"
            ),
        }
    } else {
        let (guard, exe) = match &bin {
            Some(bin) => (
                format!("[ -n \"${marker}$KEITORA_BIN\" ] && [ -x \"{bin}\" ]"),
                bin.as_str(),
            ),
            None => ("[ -n \"$KEITORA_BIN\" ]".to_string(), "$KEITORA_BIN"),
        };
        format!("{guard} && \"{exe}\" session {args} >/dev/null 2>&1 || true")
    }
}

/// The value stored under [`HOOK_NAME`].
fn keitora_hook(windows: bool, bin: Option<&str>) -> Value {
    let mut events = serde_json::Map::new();
    for (event, args) in HOOK_EVENTS {
        events.insert(
            event.to_string(),
            json!([{
                "type": "command",
                "command": hook_command(args, windows, bin),
                "timeout": HOOK_TIMEOUT_SECS,
            }]),
        );
    }
    Value::Object(events)
}

/// Return `existing` with the top-level `keitora` hook ensured. `None` when
/// `existing` is not a JSON object (the caller must then skip the file). Every
/// other top-level name is left untouched, including herdr's entry and any of the
/// user's. The whole `keitora` entry is replaced, so the merge is idempotent and
/// picks up a changed shape (or a key another writer dropped or rewrote).
pub fn merged_antigravity_hooks(
    existing: Value,
    windows: bool,
    bin: Option<&str>,
) -> Option<Value> {
    let Value::Object(mut obj) = existing else {
        return None;
    };
    obj.insert(HOOK_NAME.to_string(), keitora_hook(windows, bin));
    Some(Value::Object(obj))
}

/// `<home>/.gemini/config/hooks.json`.
pub fn hooks_file(home: &Path) -> PathBuf {
    home.join(".gemini").join("config").join("hooks.json")
}

/// The global hooks file: `<config_dir>/hooks.json` when agy's config-dir override
/// is set and non-empty (it stands for the `.gemini/config` directory), else
/// [`hooks_file`] under `home`.
pub fn hooks_file_in(home: &Path, config_dir: Option<std::ffi::OsString>) -> PathBuf {
    match config_dir.filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("hooks.json"),
        None => hooks_file(home),
    }
}

/// Merge into `file`; returns whether it was written. Creates the parent dir and
/// the file when missing; a file that cannot be read or parsed (or is not a JSON
/// object) is skipped (`Ok(false)`), never clobbered; writes only when the merged
/// value differs; never writes when `!allowed`.
pub fn apply_hooks_at(
    file: &Path,
    windows: bool,
    bin: Option<&str>,
    allowed: bool,
) -> std::io::Result<bool> {
    if !allowed {
        return Ok(false);
    }
    let Some(existing) = crate::global_writes::read_json_or_skip(file, "antigravity hooks") else {
        return Ok(false);
    };
    let base = existing.clone().unwrap_or_else(|| json!({}));
    let Some(merged) = merged_antigravity_hooks(base, windows, bin) else {
        log::warn!(
            "antigravity hooks: {} is not a JSON object; leaving it alone",
            file.display()
        );
        return Ok(false);
    };
    if existing.as_ref() == Some(&merged) {
        return Ok(false);
    }
    crate::global_writes::write_json_atomic(file, &merged)?;
    Ok(true)
}

/// A hooks file as an install left it in place: the file, the `keitora` it
/// embeds, and its length and modification time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledFile {
    file: PathBuf,
    bin: Option<String>,
    len: u64,
    modified: std::time::SystemTime,
}

impl InstalledFile {
    /// `file` as it is on disk now, for `bin`; `None` when it cannot be read.
    fn now(file: &Path, bin: Option<&str>) -> Option<Self> {
        let meta = std::fs::metadata(file).ok()?;
        Some(Self {
            file: file.to_path_buf(),
            bin: bin.map(str::to_string),
            len: meta.len(),
            modified: meta.modified().ok()?,
        })
    }
}

/// The last install's result, so a later install of the same entry into a file
/// nobody touched since is skipped without reading it (every agy launch, and every
/// agy pane resumed at boot, asks for the entry).
pub type InstallMemo = std::sync::Mutex<Option<InstalledFile>>;

/// [`apply_hooks_at`], skipped when `memo` shows `file` unchanged (same length and
/// modification time) since an install of the same `bin` left it in place. Another
/// writer's rewrite (herdr's integration install) changes the file, so it is merged
/// again.
pub fn apply_hooks_once(
    file: &Path,
    windows: bool,
    bin: Option<&str>,
    allowed: bool,
    memo: &InstallMemo,
) -> std::io::Result<bool> {
    if !allowed {
        return Ok(false);
    }
    let unchanged = |m: &Option<InstalledFile>| {
        m.is_some() && m.as_ref() == InstalledFile::now(file, bin).as_ref()
    };
    if memo.lock().is_ok_and(|m| unchanged(&m)) {
        return Ok(false);
    }
    let wrote = apply_hooks_at(file, windows, bin, allowed)?;
    if let Ok(mut m) = memo.lock() {
        *m = InstalledFile::now(file, bin);
    }
    Ok(wrote)
}

/// Install into the real home; a no-op unless [`crate::global_writes::allowed`].
pub fn install_antigravity_hooks() {
    let allowed = crate::global_writes::allowed();
    if !allowed {
        return;
    }
    let Some(home) = dirs::home_dir() else {
        log::warn!("antigravity hooks: no home directory; hook not installed");
        return;
    };
    let file = hooks_file_in(&home, std::env::var_os(CONFIG_DIR_ENV));
    // Refreshed on every sync, so an app update that moves the bundled CLI heals.
    let sources = crate::hooks::KeitoraBinSources::gather(
        None,
        crate::hooks::keitora_on_path_cached().clone(),
    );
    let bin = crate::hooks::resolve_global_keitora_bin(&sources);
    if bin.is_none() {
        log::warn!("antigravity hooks: no global `keitora` CLI found; the hook runs $KEITORA_BIN");
    }
    static MEMO: InstallMemo = std::sync::Mutex::new(None);
    match apply_hooks_once(&file, cfg!(windows), bin.as_deref(), allowed, &MEMO) {
        Ok(true) => log::info!("antigravity hooks: installed into {}", file.display()),
        Ok(false) => {}
        Err(e) => log::warn!(
            "antigravity hooks: install into {} failed: {e}",
            file.display()
        ),
    }
}

/// Worktree roots whose effective cli is Antigravity (explicit entry cli, else the
/// user `default_cli`), across every registered project. A project whose config
/// cannot be resolved is skipped.
pub fn antigravity_worktrees(user: &keitora_tasks::UserConfig) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for pref in user.projects.values() {
        let Ok(pc) = keitora_tasks::effective_config(pref) else {
            continue;
        };
        for entry in pc.worktree_entries() {
            if keitora_tasks::resolve_cli(Some(&entry), user) == keitora_tasks::CliKind::Antigravity
            {
                out.push(pc.worktree_path(&entry.name));
            }
        }
    }
    out
}

/// Registry-wide: if any worktree's effective cli is Antigravity, install the hook
/// and seed trust for each such worktree that exists on disk. A no-op unless
/// [`crate::global_writes::allowed`].
pub fn sync_registry() {
    if !crate::global_writes::allowed() {
        return;
    }
    let user = keitora_tasks::UserConfig::load().unwrap_or_default();
    let worktrees = antigravity_worktrees(&user);
    if worktrees.is_empty() {
        return;
    }
    install_antigravity_hooks();
    let existing: Vec<PathBuf> = worktrees.into_iter().filter(|p| p.is_dir()).collect();
    crate::antigravity_trust::install_trust_for_all(&existing);
}

/// One worktree: install the hook and seed trust for `worktree_root`. A no-op
/// unless [`crate::global_writes::allowed`].
pub fn sync_worktree(worktree_root: &Path) {
    if !crate::global_writes::allowed() {
        return;
    }
    install_antigravity_hooks();
    crate::antigravity_trust::install_trust_for(worktree_root);
}

#[cfg(test)]
mod tests {
    use super::*;
    use keitora_tasks::{CliKind, ProjectRef, UserConfig, WorktreeEntry};

    const EVENTS: [&str; 3] = ["SessionStart", "PreInvocation", "Stop"];

    /// The merge with no global `keitora` (the `$KEITORA_BIN` form).
    fn merged(existing: Value, windows: bool) -> Option<Value> {
        merged_antigravity_hooks(existing, windows, None)
    }

    /// An allowed install of the `$KEITORA_BIN` form into `file`; whether it wrote.
    fn apply(file: &Path, windows: bool) -> bool {
        apply_hooks_at(file, windows, None, true).unwrap()
    }

    #[test]
    fn apply_hooks_once_skips_a_file_unchanged_since_its_install() {
        let dir = tempfile::tempdir().unwrap();
        let file = hooks_file(dir.path());
        let memo = InstallMemo::default();
        let once = |bin: &str| apply_hooks_once(&file, false, Some(bin), true, &memo).unwrap();
        assert!(once("/x/keitora"), "first install writes");
        assert!(!once("/x/keitora"));
        // Same length and modification time: the file is not even read, so a
        // same-size edit that keeps the time goes unseen.
        let text = std::fs::read_to_string(&file).unwrap();
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        let edited = text.replacen("/x/keitora", "/y/keitora", 1);
        assert_eq!(edited.len(), text.len());
        std::fs::write(&file, &edited).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert!(!once("/x/keitora"));
        // Another writer's rewrite changes the file: merged again.
        std::fs::write(&file, "{}").unwrap();
        assert!(once("/x/keitora"), "the dropped entry is put back");
        // Another `keitora` is merged in.
        assert!(once("/z/keitora"));
        // Never without the write guard.
        let guarded = tempfile::tempdir().unwrap();
        let other = hooks_file(guarded.path());
        assert!(!apply_hooks_once(&other, false, None, false, &memo).unwrap());
        assert!(!other.exists());
    }

    fn command_of(v: &Value, event: &str) -> String {
        v[HOOK_NAME][event][0]["command"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn merge_creates_keitora_hook_in_empty_object() {
        let v = merged(json!({}), false).unwrap();
        let hook = v[HOOK_NAME].as_object().unwrap();
        let mut keys: Vec<&str> = hook.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["PreInvocation", "SessionStart", "Stop"]);
        for event in EVENTS {
            let arr = hook[event].as_array().unwrap();
            assert_eq!(arr.len(), 1, "{event}");
            assert_eq!(arr[0]["type"], "command", "{event}");
            assert_eq!(arr[0]["timeout"], 10, "{event}");
        }
        assert!(hook.get("enabled").is_none(), "never adds an enabled key");
    }

    #[test]
    fn merge_uses_direct_handler_arrays() {
        let v = merged(json!({}), true).unwrap();
        for event in EVENTS {
            let handler = &v[HOOK_NAME][event][0];
            assert!(handler.get("command").is_some(), "{event}");
            assert!(handler.get("hooks").is_none(), "{event}");
        }
    }

    #[test]
    fn merge_keeps_other_hook_names() {
        let herdr =
            json!({ "Stop": [ { "type": "command", "command": "herdr-state", "timeout": 5 } ] });
        let mine = json!({ "enabled": false, "PreToolUse": [ { "matcher": "*", "hooks": [] } ] });
        let v = merged(
            json!({ "herdr-agent-state": herdr.clone(), "mine": mine.clone() }),
            false,
        )
        .unwrap();
        assert_eq!(v["herdr-agent-state"], herdr);
        assert_eq!(v["mine"], mine);
        assert_eq!(v.as_object().unwrap().len(), 3);
    }

    #[test]
    fn merge_replaces_only_keitora() {
        let old = json!({
            "keitora": { "Stop": [ { "type": "command", "command": "old", "timeout": 1 } ],
                         "PostInvocation": [] },
            "other": { "Stop": [] },
        });
        let v = merged(old, false).unwrap();
        assert_eq!(
            v,
            merged(json!({ "other": { "Stop": [] } }), false).unwrap()
        );
        assert_eq!(v[HOOK_NAME]["Stop"].as_array().unwrap().len(), 1);
        assert!(v[HOOK_NAME].get("PostInvocation").is_none());
    }

    #[test]
    fn merge_is_idempotent() {
        for windows in [false, true] {
            let once = merged(json!({ "x": {} }), windows).unwrap();
            let twice = merged(once.clone(), windows).unwrap();
            assert_eq!(once, twice);
        }
    }

    #[test]
    fn merge_rejects_non_object() {
        assert_eq!(merged(json!([]), false), None);
        assert_eq!(merged(json!("x"), false), None);
        assert_eq!(merged(Value::Null, false), None);
    }

    #[test]
    fn hook_command_windows_shape() {
        assert_eq!(
            hook_command("busy --stdin-payload", true, None),
            "(if defined KEITORA_BIN \"%KEITORA_BIN%\" session busy --stdin-payload >nul 2>&1) & exit /b 0"
        );
        assert_eq!(
            hook_command("idle --stdin-payload --if-fully-idle", true, None),
            "(if defined KEITORA_BIN \"%KEITORA_BIN%\" session idle --stdin-payload --if-fully-idle >nul 2>&1) & exit /b 0"
        );
    }

    #[test]
    fn apply_hooks_at_rewrites_an_older_command_form() {
        let dir = tempfile::tempdir().unwrap();
        let file = hooks_file(dir.path());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let old_cmd =
            "if defined KEITORA_BIN \"%KEITORA_BIN%\" session idle --stdin-payload >nul 2>&1";
        let old = json!({
            "keitora": { "Stop": [ { "type": "command", "command": old_cmd, "timeout": 10 } ] },
            "other": { "Stop": [] },
        });
        std::fs::write(&file, serde_json::to_string(&old).unwrap()).unwrap();
        assert!(apply(&file, true));
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            command_of(&doc, "Stop"),
            hook_command("idle --stdin-payload --if-fully-idle", true, None)
        );
        assert_eq!(doc["other"], json!({ "Stop": [] }));
        assert!(!apply(&file, true), "then stable");
    }

    #[test]
    fn hook_command_posix_shape() {
        assert_eq!(
            hook_command("busy --stdin-payload", false, None),
            "[ -n \"$KEITORA_BIN\" ] && \"$KEITORA_BIN\" session busy --stdin-payload >/dev/null 2>&1 || true"
        );
    }

    #[test]
    fn stop_command_carries_the_idle_filter() {
        for windows in [false, true] {
            let v = merged(json!({}), windows).unwrap();
            assert!(command_of(&v, "Stop").contains("--if-fully-idle"));
            assert!(!command_of(&v, "SessionStart").contains("--if-fully-idle"));
            assert!(!command_of(&v, "PreInvocation").contains("--if-fully-idle"));
            assert!(command_of(&v, "SessionStart").contains("session idle --stdin-payload"));
            assert!(command_of(&v, "PreInvocation").contains("session busy --stdin-payload"));
        }
    }

    #[test]
    fn hooks_file_is_under_gemini_config() {
        let home = Path::new("/home/u");
        assert_eq!(
            hooks_file(home),
            home.join(".gemini").join("config").join("hooks.json")
        );
    }

    #[test]
    fn apply_hooks_at_writes_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = hooks_file(dir.path());
        assert!(apply(&file, false));
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.ends_with('\n'), "trailing newline");
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc, merged(json!({}), false).unwrap());
        assert!(!apply(&file, false));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            text,
            "not rewritten"
        );
    }

    #[test]
    fn apply_hooks_at_keeps_foreign_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hooks.json");
        std::fs::write(&file, r#"{"herdr-agent-state":{"Stop":[]}}"#).unwrap();
        assert!(apply(&file, true));
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(doc["herdr-agent-state"], json!({ "Stop": [] }));
        assert!(doc[HOOK_NAME].is_object());
    }

    #[test]
    fn apply_hooks_at_skips_unparseable_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hooks.json");
        std::fs::write(&file, "not json").unwrap();
        assert!(!apply(&file, false));
        assert_eq!(std::fs::read(&file).unwrap(), b"not json");
    }

    #[test]
    fn apply_hooks_at_skips_non_object_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hooks.json");
        std::fs::write(&file, "[]").unwrap();
        assert!(!apply(&file, false));
        assert_eq!(std::fs::read(&file).unwrap(), b"[]");
    }

    #[test]
    fn apply_hooks_at_refuses_when_not_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let file = hooks_file(dir.path());
        assert!(!apply_hooks_at(&file, false, None, false).unwrap());
        assert!(!file.exists());
        assert!(!dir.path().join(".gemini").exists(), "no parent dir either");
    }

    fn entry(name: &str, path: &Path, cli: Option<CliKind>) -> WorktreeEntry {
        WorktreeEntry {
            path: Some(path.join(name)),
            cli,
            ..WorktreeEntry::name_only(name)
        }
    }

    #[test]
    fn antigravity_worktrees_follow_effective_cli() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let wts = tmp.path().join("wt");
        let pref = ProjectRef {
            worktrees: Some(vec![
                entry("a", &wts, Some(CliKind::Antigravity)),
                entry("b", &wts, Some(CliKind::Claude)),
                entry("c", &wts, None),
            ]),
            ..ProjectRef::new(root)
        };
        let mut user = UserConfig::default();
        user.projects.insert("proj".to_string(), pref);

        user.default_cli = Some(CliKind::Antigravity);
        assert_eq!(
            antigravity_worktrees(&user),
            vec![wts.join("a"), wts.join("c")]
        );
        user.default_cli = None;
        assert_eq!(antigravity_worktrees(&user), vec![wts.join("a")]);
    }

    /// A fake `keitora` in `dir` that writes its arguments to `<dir>/ran`.
    fn fake_keitora(dir: &Path) -> std::path::PathBuf {
        let out = dir.join("ran");
        let (bin, body) = if cfg!(windows) {
            (
                dir.join("fake-keitora.cmd"),
                format!("@echo %* > \"{}\"\r\n", out.display()),
            )
        } else {
            (
                dir.join("fake-keitora"),
                format!("#!/bin/sh\necho \"$@\" > '{}'\n", out.display()),
            )
        };
        crate::hooks::write_executable(&bin, &body);
        bin
    }

    /// Run a hook `command` the way agy does (cmd.exe on Windows, `sh -c`
    /// elsewhere) with `env` set and neither `KEITORA_BIN` nor `KEITORA_PANE`
    /// inherited. Returns whether the fake recorded a call.
    fn run_hook(dir: &Path, command: &str, env: &[(&str, &str)]) -> bool {
        #[cfg(windows)]
        let mut c = {
            use std::os::windows::process::CommandExt as _;
            let mut c = std::process::Command::new("cmd");
            c.raw_arg("/D /C").raw_arg(command);
            c
        };
        #[cfg(not(windows))]
        let mut c = {
            let mut c = std::process::Command::new("sh");
            c.arg("-c").arg(command);
            c
        };
        c.env_remove("KEITORA_BIN").env_remove("KEITORA_PANE");
        for (k, v) in env {
            c.env(k, v);
        }
        let status = c.stdin(std::process::Stdio::null()).status().unwrap();
        assert!(status.success(), "a hook always exits 0: {command}");
        dir.join("ran").exists()
    }

    #[test]
    fn repro_a1_agy_hook_command_is_inert_without_keitora_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = fake_keitora(tmp.path());
        let bin = bin.to_string_lossy();
        let command = hook_command("idle --stdin-payload", cfg!(windows), Some(&bin));
        // A Keitora pane where no `KEITORA_BIN` resolved: the hook must still record.
        assert!(
            run_hook(tmp.path(), &command, &[("KEITORA_PANE", "1")]),
            "the hook recorded nothing in a Keitora pane without KEITORA_BIN: {command}"
        );
    }

    /// Run `command` with `env` in a fresh directory holding the fake CLI it calls.
    fn runs_with(env: &[(&str, &str)]) -> bool {
        let tmp = tempfile::tempdir().unwrap();
        let bin = fake_keitora(tmp.path());
        let command = hook_command(
            "busy --stdin-payload",
            cfg!(windows),
            Some(&bin.to_string_lossy()),
        );
        run_hook(tmp.path(), &command, env)
    }

    #[test]
    fn hook_command_is_a_noop_outside_a_keitora_pane() {
        // Neither variable: an agy run outside Keitora does nothing, yet exits 0.
        assert!(!runs_with(&[]));
        // Empty values count as unset in the POSIX form; cmd.exe cannot set an
        // empty variable, so only check the POSIX shell there.
        if !cfg!(windows) {
            assert!(!runs_with(&[("KEITORA_PANE", ""), ("KEITORA_BIN", "")]));
        }
    }

    #[test]
    fn hook_command_runs_in_a_pane_with_either_variable() {
        // A pane from the current build carries the marker.
        assert!(runs_with(&[("KEITORA_PANE", "1")]));
        // A pane started by an older build (kept alive by herdr across a Keitora
        // upgrade) carries only `KEITORA_BIN` and must not go silent.
        assert!(runs_with(&[("KEITORA_BIN", "/x/older-keitora")]));
        assert!(runs_with(&[("KEITORA_PANE", "1"), ("KEITORA_BIN", "/x/k")]));
    }

    // The verbatim prefix only exists (and is only stripped) on Windows.
    #[cfg(windows)]
    #[test]
    fn hook_command_windows_embeds_the_absolute_path_and_marker() {
        assert_eq!(
            hook_command(
                "idle --stdin-payload --if-fully-idle",
                true,
                Some(r"\\?\C:\Users\x\AppData\Local\Keitora\keitora-cli.exe")
            ),
            "(if not defined KEITORA_PANE if not defined KEITORA_BIN exit /b 0) & (\"C:\\Users\\x\\AppData\\Local\\Keitora\\keitora-cli.exe\" session idle --stdin-payload --if-fully-idle >nul 2>&1) & exit /b 0"
        );
    }

    #[test]
    fn hook_command_posix_embeds_the_absolute_path_and_marker() {
        assert_eq!(
            hook_command("busy --stdin-payload", false, Some("/Users/me/.local/bin/keitora")),
            "[ -n \"$KEITORA_PANE$KEITORA_BIN\" ] && [ -x \"/Users/me/.local/bin/keitora\" ] && \"/Users/me/.local/bin/keitora\" session busy --stdin-payload >/dev/null 2>&1 || true"
        );
    }

    #[test]
    fn hook_command_without_a_resolved_bin_keeps_the_keitora_bin_form() {
        assert_eq!(
            hook_command("busy --stdin-payload", true, None),
            "(if defined KEITORA_BIN \"%KEITORA_BIN%\" session busy --stdin-payload >nul 2>&1) & exit /b 0"
        );
        assert_eq!(
            hook_command("busy --stdin-payload", false, None),
            "[ -n \"$KEITORA_BIN\" ] && \"$KEITORA_BIN\" session busy --stdin-payload >/dev/null 2>&1 || true"
        );
    }

    #[test]
    fn hooks_file_in_honours_the_config_dir_override() {
        let home = Path::new("/Users/me");
        assert_eq!(
            hooks_file_in(home, Some("/Users/me/agy-config".into())),
            Path::new("/Users/me/agy-config").join("hooks.json")
        );
        assert_eq!(
            hooks_file_in(home, Some("".into())),
            hooks_file(home),
            "empty = default"
        );
        assert_eq!(
            hooks_file_in(home, None),
            hooks_file(home),
            "unset = default"
        );
    }

    /// A hooks file holding our entry (for `bin`) next to herdr's.
    fn installed(dir: &Path, bin: &str) -> PathBuf {
        let file = dir.join("hooks.json");
        std::fs::write(&file, r#"{"herdr-agent-state":{"Stop":[]}}"#).unwrap();
        assert!(apply_hooks_at(&file, cfg!(windows), Some(bin), true).unwrap());
        file
    }

    fn read(file: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap()
    }

    #[test]
    fn apply_hooks_at_restores_a_key_herdr_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let file = installed(dir.path(), "/x/keitora");
        let want = read(&file);
        // herdr's integration install rewrites the file without our key.
        std::fs::write(
            &file,
            r#"{"herdr-agent-state":{"Stop":[{"type":"command","command":"herdr"}]}}"#,
        )
        .unwrap();
        assert!(apply_hooks_at(&file, cfg!(windows), Some("/x/keitora"), true).unwrap());
        let doc = read(&file);
        assert_eq!(doc[HOOK_NAME], want[HOOK_NAME], "restored");
        assert_eq!(
            doc["herdr-agent-state"]["Stop"][0]["command"], "herdr",
            "herdr's rewrite kept"
        );
    }

    #[test]
    fn apply_hooks_at_restores_a_key_whose_command_was_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let file = installed(dir.path(), "/x/keitora");
        let want = read(&file);
        let mut doc = want.clone();
        doc[HOOK_NAME]["Stop"][0]["command"] = json!("echo rewritten");
        std::fs::write(&file, doc.to_string()).unwrap();
        assert!(apply_hooks_at(&file, cfg!(windows), Some("/x/keitora"), true).unwrap());
        assert_eq!(read(&file), want);
    }

    #[test]
    fn apply_hooks_at_leaves_an_unchanged_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let file = installed(dir.path(), "/x/keitora");
        let bytes = std::fs::read(&file).unwrap();
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert!(!apply_hooks_at(&file, cfg!(windows), Some("/x/keitora"), true).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), mtime);
        // A moved CLI is a change: the stored path is refreshed.
        assert!(apply_hooks_at(&file, cfg!(windows), Some("/y/keitora"), true).unwrap());
        assert!(std::fs::read_to_string(&file)
            .unwrap()
            .contains("/y/keitora"));
    }

    #[test]
    fn real_home_writers_are_inert_under_test() {
        // The guard refuses in the test binary, so neither touches `~/.gemini`.
        assert!(!crate::global_writes::allowed());
        install_antigravity_hooks();
        sync_registry();
        sync_worktree(Path::new("/home/u/proj"));
    }
}
