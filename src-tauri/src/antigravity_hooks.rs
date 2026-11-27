//! The Keitora session hook for the Antigravity CLI (`agy`), installed into the
//! user's global `~/.gemini/config/hooks.json`.
//!
//! `agy` loads hooks from that global file (and per-workspace `.agents/hooks.json`,
//! which Keitora does not write). The file is a JSON object keyed by hook *name*;
//! Keitora owns exactly the `keitora` name and leaves every other one (herdr's,
//! the user's) untouched. Non-tool events take a handler array directly — wrapping
//! them in `{"hooks":[…]}` makes `agy` reject the whole file.
//!
//! The file is shared by every worktree and by `agy` runs outside Keitora, so no
//! binary path is baked in: each command runs `$KEITORA_BIN` (set in every Keitora
//! pane) and is a silent no-op when the variable is absent. Hook commands run under
//! cmd.exe on Windows and a POSIX shell elsewhere.
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

/// The command string for one registration; `windows` selects cmd.exe vs POSIX
/// shell syntax. Both forms do nothing unless `KEITORA_BIN` is set, print
/// nothing (so `agy` treats the hook as having no decision) and always exit 0, so
/// a failing or missing `keitora` never fails the agent. The cmd.exe form groups
/// the conditional so `& exit /b 0` runs whether or not `KEITORA_BIN` is defined.
pub fn hook_command(args: &str, windows: bool) -> String {
    if windows {
        format!("(if defined KEITORA_BIN \"%KEITORA_BIN%\" session {args} >nul 2>&1) & exit /b 0")
    } else {
        format!(
            "[ -n \"$KEITORA_BIN\" ] && \"$KEITORA_BIN\" session {args} >/dev/null 2>&1 || true"
        )
    }
}

/// The value stored under [`HOOK_NAME`].
fn keitora_hook(windows: bool) -> Value {
    let mut events = serde_json::Map::new();
    for (event, args) in HOOK_EVENTS {
        events.insert(
            event.to_string(),
            json!([{
                "type": "command",
                "command": hook_command(args, windows),
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
/// picks up a changed shape.
pub fn merged_antigravity_hooks(existing: Value, windows: bool) -> Option<Value> {
    let Value::Object(mut obj) = existing else {
        return None;
    };
    obj.insert(HOOK_NAME.to_string(), keitora_hook(windows));
    Some(Value::Object(obj))
}

/// `<home>/.gemini/config/hooks.json`.
pub fn hooks_file(home: &Path) -> PathBuf {
    home.join(".gemini").join("config").join("hooks.json")
}

/// Merge into `file`; returns whether it was written. Creates the parent dir and
/// the file when missing; a file that cannot be read or parsed (or is not a JSON
/// object) is skipped (`Ok(false)`), never clobbered; writes only when the merged
/// value differs; never writes when `!allowed`.
pub fn apply_hooks_at(file: &Path, windows: bool, allowed: bool) -> std::io::Result<bool> {
    if !allowed {
        return Ok(false);
    }
    let Some(existing) = crate::global_writes::read_json_or_skip(file, "antigravity hooks") else {
        return Ok(false);
    };
    let base = existing.clone().unwrap_or_else(|| json!({}));
    let Some(merged) = merged_antigravity_hooks(base, windows) else {
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
    let file = hooks_file(&home);
    match apply_hooks_at(&file, cfg!(windows), allowed) {
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

    fn command_of(v: &Value, event: &str) -> String {
        v[HOOK_NAME][event][0]["command"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn merge_creates_keitora_hook_in_empty_object() {
        let v = merged_antigravity_hooks(json!({}), false).unwrap();
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
        let v = merged_antigravity_hooks(json!({}), true).unwrap();
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
        let v = merged_antigravity_hooks(
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
        let v = merged_antigravity_hooks(old, false).unwrap();
        assert_eq!(
            v,
            merged_antigravity_hooks(json!({ "other": { "Stop": [] } }), false).unwrap()
        );
        assert_eq!(v[HOOK_NAME]["Stop"].as_array().unwrap().len(), 1);
        assert!(v[HOOK_NAME].get("PostInvocation").is_none());
    }

    #[test]
    fn merge_is_idempotent() {
        for windows in [false, true] {
            let once = merged_antigravity_hooks(json!({ "x": {} }), windows).unwrap();
            let twice = merged_antigravity_hooks(once.clone(), windows).unwrap();
            assert_eq!(once, twice);
        }
    }

    #[test]
    fn merge_rejects_non_object() {
        assert_eq!(merged_antigravity_hooks(json!([]), false), None);
        assert_eq!(merged_antigravity_hooks(json!("x"), false), None);
        assert_eq!(merged_antigravity_hooks(Value::Null, false), None);
    }

    #[test]
    fn hook_command_windows_shape() {
        assert_eq!(
            hook_command("busy --stdin-payload", true),
            "(if defined KEITORA_BIN \"%KEITORA_BIN%\" session busy --stdin-payload >nul 2>&1) & exit /b 0"
        );
        assert_eq!(
            hook_command("idle --stdin-payload --if-fully-idle", true),
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
        assert!(apply_hooks_at(&file, true, true).unwrap());
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            command_of(&doc, "Stop"),
            hook_command("idle --stdin-payload --if-fully-idle", true)
        );
        assert_eq!(doc["other"], json!({ "Stop": [] }));
        assert!(!apply_hooks_at(&file, true, true).unwrap(), "then stable");
    }

    #[test]
    fn hook_command_posix_shape() {
        assert_eq!(
            hook_command("busy --stdin-payload", false),
            "[ -n \"$KEITORA_BIN\" ] && \"$KEITORA_BIN\" session busy --stdin-payload >/dev/null 2>&1 || true"
        );
    }

    #[test]
    fn stop_command_carries_the_idle_filter() {
        for windows in [false, true] {
            let v = merged_antigravity_hooks(json!({}), windows).unwrap();
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
        assert!(apply_hooks_at(&file, false, true).unwrap());
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.ends_with('\n'), "trailing newline");
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc, merged_antigravity_hooks(json!({}), false).unwrap());
        assert!(!apply_hooks_at(&file, false, true).unwrap());
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
        assert!(apply_hooks_at(&file, true, true).unwrap());
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(doc["herdr-agent-state"], json!({ "Stop": [] }));
        assert!(doc[HOOK_NAME].is_object());
    }

    #[test]
    fn apply_hooks_at_skips_unparseable_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hooks.json");
        std::fs::write(&file, "not json").unwrap();
        assert!(!apply_hooks_at(&file, false, true).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), b"not json");
    }

    #[test]
    fn apply_hooks_at_skips_non_object_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hooks.json");
        std::fs::write(&file, "[]").unwrap();
        assert!(!apply_hooks_at(&file, false, true).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), b"[]");
    }

    #[test]
    fn apply_hooks_at_refuses_when_not_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let file = hooks_file(dir.path());
        assert!(!apply_hooks_at(&file, false, false).unwrap());
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

    #[test]
    fn real_home_writers_are_inert_under_test() {
        // The guard refuses in the test binary, so neither touches `~/.gemini`.
        assert!(!crate::global_writes::allowed());
        install_antigravity_hooks();
        sync_registry();
        sync_worktree(Path::new("/home/u/proj"));
    }
}
