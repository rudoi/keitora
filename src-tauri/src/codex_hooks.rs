// Auto-install the session-state hooks into each Codex worktree's
// `.codex/hooks.json` on startup — the Codex analog of [`crate::hooks`]'s
// `.claude/settings.local.json` installer.
//
// Codex ships a native, enabled-by-default hooks engine (`codex features list` →
// `hooks stable true`). A worktree-local `.codex/hooks.json` is discovered per
// session and maps lifecycle events to shell commands, exactly like Claude Code's
// settings hooks. We register the SAME `keitora session <state>` reporting path the
// Claude hooks use, so a Codex worktree drives the daemon/rail identically.
//
// Two behaviours differ from the Claude installer and are load-bearing:
//   1. **Trust.** A freshly-registered Codex hook is UNTRUSTED and will NOT fire
//      until either a human grants trust interactively or the launch passes
//      `--dangerously-bypass-hook-trust`. Every Keitora-spawned session is
//      headless/PTY-driven with no human present, so that flag is added to the
//      Codex launch argv (see `backend::herdr::agent`). Verified empirically: without
//      the flag the hooks are silently skipped; with it they fire.
//   2. **Shell.** Codex runs a hook `command` through **PowerShell** on Windows
//      (verified: `>`-redirected output came back UTF-16LE-with-BOM, PowerShell's
//      signature, and the binary references `powershell.exe`) — NOT the Git Bash
//      that Claude Code uses. So the Windows command is PowerShell syntax (call
//      operator `&`, `*> $null`), while unix stays POSIX `/bin/sh` like Claude.
//
// Only worktrees whose resolved `CliKind` is `Codex` get this file, so a user who
// runs `codex` by hand in a Claude worktree is never prompted to trust our hooks.

use std::path::Path;

use crate::hooks::{merge_json_file, posix_fallback_command, FileWrite};
use serde_json::{json, Value};

/// (Codex event, session state). The Codex analog of `hooks::HOOK_MAP`, using the
/// same four states the Claude hooks report. Codex's event catalogue
/// (`HookEventName`: `SessionStart, UserPromptSubmit, Stop, PermissionRequest,
/// SessionEnd, …`) is near 1:1 with Claude's, so the mapping mirrors it:
///
/// | Codex event        | State   | Claude analog          |
/// |--------------------|---------|------------------------|
/// | `SessionStart`     | idle    | `SessionStart`         |
/// | `UserPromptSubmit` | busy    | `UserPromptSubmit`     |
/// | `Stop`             | idle    | `Stop`                 |
/// | `PermissionRequest`| waiting | `Notification`/perm    |
/// | `SessionEnd`       | offline | `SessionEnd`           |
///
/// `SessionStart`, `UserPromptSubmit`, `SessionEnd` are empirically confirmed to
/// fire headlessly with these payloads. `Stop` (turn-complete) and
/// `PermissionRequest` (approval-needed) could not be *fired* in the spike (the
/// account hit its usage limit before a turn could complete or request approval),
/// but their config keys are accepted by Codex's parser and are the correct
/// PascalCase `HookEventName` wire names, so they are registered here. Codex has no
/// distinct `StopFailure` analog (Claude's second idle trigger), and `SubagentStop`
/// is deliberately omitted — it fires mid-turn and would wrongly flip to idle.
///
/// `PermissionRequest` runs the gate (which itself writes `waiting` when nothing
/// brokers the prompt) and `Stop` captures the reply for a driver before idle.
const CODEX_HOOK_MAP: [(&str, CodexCmd); 5] = [
    ("SessionStart", CodexCmd::State("idle")),
    ("UserPromptSubmit", CodexCmd::State("busy")),
    ("Stop", CodexCmd::Stop),
    ("PermissionRequest", CodexCmd::Gate),
    ("SessionEnd", CodexCmd::State("offline")),
];

/// Which command a Codex hook entry runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexCmd {
    /// `session <state>`, all streams silenced.
    State(&'static str),
    /// Permission gate; stdout reaches Codex, only stderr is discarded.
    Gate,
    /// Turn end: capture the reply from stdin, then record idle.
    Stop,
}

/// Detectable substring identifying a hook `command` as ours, so a refresh replaces
/// rather than duplicates it (distinct from the Claude installer's marker). Carried
/// as a trailing `# …` comment — valid in both PowerShell and `/bin/sh`.
const CODEX_HOOK_MARKER: &str = "keitora:codex-session-hook";

/// The resilient hook `command` we install for one (binary, state).
///
/// Codex runs this through **PowerShell** on Windows and **`/bin/sh`** on unix, so
/// the shape is platform-specific (unlike the Claude installer, which is POSIX on
/// every platform because Claude Code always uses Git Bash):
///
/// - **Windows (PowerShell):** `if (Test-Path -LiteralPath "<bin>") { & "<bin>"
///   session <state> *> $null }`. The `Test-Path` guard short-circuits a
///   missing/not-yet-built binary; the call operator `&` runs a quoted absolute
///   path (a bare quoted string would merely be echoed); `*> $null` silences all
///   streams so a hook never leaks output or a non-zero exit into the session.
/// - **Unix (`/bin/sh`):** identical to the Claude hook — `[ -x "<bin>" ] && "<bin>"
///   session <state> >/dev/null 2>&1 || true`.
///
/// The `\\?\` extended-length prefix the resolved build path can carry is stripped
/// (PowerShell's `Test-Path`/`&` and `/bin/sh` both choke on it); UNC-safe, and a
/// no-op on unix / on a path without the prefix.
fn hook_command(task_bin: &str, cmd: CodexCmd) -> String {
    let bin = keitora_tasks::strip_extended_prefix_str(task_bin);
    build_command(&bin, cmd, cfg!(windows))
}

/// Platform-independent builder behind [`hook_command`], so both shapes are testable
/// on any OS.
///
/// The gate and `--stdin` forms discard only stderr (`2>$null` / `2>/dev/null`) so
/// stdout reaches Codex; each falls back to a plain silenced call (`session waiting` /
/// `session idle`) when the installed CLI predates the new flags. On Windows a trailing
/// `exit 0` stops an old CLI's non-zero clap exit from reading as a block.
fn build_command(bin: &str, cmd: CodexCmd, windows: bool) -> String {
    let m = CODEX_HOOK_MARKER;
    if windows {
        // Backslashes are literal inside PowerShell double-quotes (backtick is the
        // escape char, not backslash), so a native `C:\…\keitora.exe` is used as-is.
        match cmd {
            // The primary records the payload's session id (and prints nothing); an
            // older CLI that rejects the flag still records the state.
            CodexCmd::State(state) => format!(
                "if (Test-Path -LiteralPath \"{bin}\") {{ & \"{bin}\" session {state} --payload-session 2>$null; if ($LASTEXITCODE -ne 0) {{ & \"{bin}\" session {state} *> $null }} }}; exit 0 # {m}"
            ),
            CodexCmd::Gate => format!(
                "if (Test-Path -LiteralPath \"{bin}\") {{ & \"{bin}\" session gate --provider codex 2>$null; if ($LASTEXITCODE -ne 0) {{ & \"{bin}\" session waiting *> $null }} }}; exit 0 # {m}"
            ),
            CodexCmd::Stop => format!(
                "if (Test-Path -LiteralPath \"{bin}\") {{ & \"{bin}\" session idle --stdin 2>$null; if ($LASTEXITCODE -ne 0) {{ & \"{bin}\" session idle *> $null }} }}; exit 0 # {m}"
            ),
        }
    } else {
        match cmd {
            CodexCmd::State(state) => {
                posix_fallback_command(bin, &format!("{state} --payload-session"), state, m)
            }
            CodexCmd::Gate => posix_fallback_command(bin, "gate --provider codex", "waiting", m),
            CodexCmd::Stop => posix_fallback_command(bin, "idle --stdin", "idle", m),
        }
    }
}

/// True if a Codex hook-group entry is one we manage. Matches the current marker and
/// the bare `…keitora session …` / legacy `…task session …` forms, so older installs
/// are recognized and replaced — not duplicated — on the next refresh.
fn is_ours(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|h| {
                h.get("command").and_then(|c| c.as_str()).is_some_and(|c| {
                    c.contains(CODEX_HOOK_MARKER)
                        || c.contains("keitora\" session ")
                        || c.contains("keitora session ")
                        || c.contains("task session ")
                })
            })
        })
}

/// Pure merge: return `existing` with our Codex session hooks ensured. Idempotent;
/// preserves unrelated keys and any non-ours hook entries on shared events. The
/// `.codex/hooks.json` shape mirrors Claude's settings hooks:
/// `{ "hooks": { "<Event>": [ { "hooks": [ { "type": "command", "command": … } ] } ] } }`.
pub fn merged_codex_hooks(existing: Value, task_bin: &str) -> Value {
    let mut root = match existing {
        Value::Object(m) => Value::Object(m),
        _ => json!({}),
    };
    let obj = root.as_object_mut().unwrap();

    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();

    for (event, cmd) in CODEX_HOOK_MAP {
        let mut hook = json!({ "type": "command", "command": hook_command(task_bin, cmd) });
        if cmd == CodexCmd::Gate {
            hook["timeout"] = json!(keitora_tasks::DRIVE_HOOK_TIMEOUT_S);
        }
        let entry = json!({ "hooks": [ hook ] });
        let arr = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        let arr = arr.as_array_mut().unwrap();
        arr.retain(|e| !is_ours(e)); // drop our prior entries → idempotent + path-change safe
        arr.push(entry);
    }
    root
}

/// Whether a `.codex/hooks.json` value carries one of our entries on every event we
/// register (read-only; for the hook diagnostics).
pub fn hooks_installed(v: &Value) -> bool {
    crate::hooks::every_event_has(v, CODEX_HOOK_MAP.iter().map(|(e, _)| *e), CODEX_HOOK_MARKER)
}

/// Install/refresh the Codex session hooks for a single worktree at `wt_path`.
///
/// Reads `.codex/hooks.json`, merges our hooks in, and writes back only if the
/// content actually changed (preserving a clean `git status` for unchanged
/// worktrees). Callers gate this on the worktree's resolved `CliKind == Codex`.
pub fn install_codex_hooks_for(wt_path: &Path, task_bin: &str) -> FileWrite {
    let file = wt_path.join(".codex").join("hooks.json");
    merge_json_file(&file, "codex session hooks", |v| {
        merged_codex_hooks(v, task_bin)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands_for(v: &Value, event: &str) -> Vec<String> {
        v["hooks"][event]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|e| e["hooks"].as_array().unwrap().iter())
            .map(|h| h["command"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn empty_input_gets_all_five_events_mapped() {
        let out = merged_codex_hooks(json!({}), "/abs/keitora");
        for (event, cmd) in CODEX_HOOK_MAP {
            assert_eq!(
                commands_for(&out, event),
                vec![hook_command("/abs/keitora", cmd)],
                "event {event}"
            );
        }
    }

    /// The event → state mapping table itself is the load-bearing spike output;
    /// pin every pair so a future edit can't silently repoint a state.
    #[test]
    fn event_state_mapping_is_exact() {
        let map: std::collections::BTreeMap<_, _> = CODEX_HOOK_MAP.iter().copied().collect();
        assert_eq!(map.get("SessionStart"), Some(&CodexCmd::State("idle")));
        assert_eq!(map.get("UserPromptSubmit"), Some(&CodexCmd::State("busy")));
        assert_eq!(map.get("Stop"), Some(&CodexCmd::Stop));
        assert_eq!(map.get("PermissionRequest"), Some(&CodexCmd::Gate));
        assert_eq!(map.get("SessionEnd"), Some(&CodexCmd::State("offline")));
        assert_eq!(map.len(), 5, "no unexpected events");
        // `SubagentStop` must never be mapped (fires mid-turn → false idle).
        assert!(!map.contains_key("SubagentStop"));
    }

    /// The installed command must be guarded, silenced, marker-carrying, and use the
    /// right shell per platform (PowerShell on Windows, POSIX `/bin/sh` on unix).
    #[test]
    fn command_is_guarded_silenced_and_marked() {
        let cmd = hook_command("/abs/keitora", CodexCmd::State("busy"));
        assert!(cmd.contains(CODEX_HOOK_MARKER), "carries marker: {cmd}");
        assert!(cmd.contains("session busy"), "invokes state: {cmd}");
        #[cfg(windows)]
        {
            assert!(cmd.starts_with("if (Test-Path"), "guarded: {cmd}");
            assert!(cmd.contains("& \"/abs/keitora\""), "call operator: {cmd}");
            assert!(cmd.contains("*> $null"), "silenced fallback: {cmd}");
            assert!(
                cmd.ends_with(&format!("exit 0 # {CODEX_HOOK_MARKER}")),
                "{cmd}"
            );
        }
        #[cfg(not(windows))]
        {
            assert!(cmd.starts_with("[ -x \"/abs/keitora\" ]"), "guarded: {cmd}");
            assert!(cmd.contains(">/dev/null 2>&1"), "silenced fallback: {cmd}");
            assert!(cmd.contains("|| true"), "never errors: {cmd}");
        }
    }

    #[test]
    fn exact_unix_command_strings() {
        let b = "/abs/keitora";
        assert_eq!(
            build_command(b, CodexCmd::State("busy"), false),
            "[ -x \"/abs/keitora\" ] && { \"/abs/keitora\" session busy --payload-session 2>/dev/null || \"/abs/keitora\" session busy >/dev/null 2>&1; } || true # keitora:codex-session-hook"
        );
        assert_eq!(
            build_command(b, CodexCmd::Gate, false),
            "[ -x \"/abs/keitora\" ] && { \"/abs/keitora\" session gate --provider codex 2>/dev/null || \"/abs/keitora\" session waiting >/dev/null 2>&1; } || true # keitora:codex-session-hook"
        );
        assert_eq!(
            build_command(b, CodexCmd::Stop, false),
            "[ -x \"/abs/keitora\" ] && { \"/abs/keitora\" session idle --stdin 2>/dev/null || \"/abs/keitora\" session idle >/dev/null 2>&1; } || true # keitora:codex-session-hook"
        );
    }

    #[test]
    fn exact_windows_command_strings() {
        let b = "C:\\bin\\keitora.exe";
        assert_eq!(
            build_command(b, CodexCmd::State("busy"), true),
            "if (Test-Path -LiteralPath \"C:\\bin\\keitora.exe\") { & \"C:\\bin\\keitora.exe\" session busy --payload-session 2>$null; if ($LASTEXITCODE -ne 0) { & \"C:\\bin\\keitora.exe\" session busy *> $null } }; exit 0 # keitora:codex-session-hook"
        );
        assert_eq!(
            build_command(b, CodexCmd::Gate, true),
            "if (Test-Path -LiteralPath \"C:\\bin\\keitora.exe\") { & \"C:\\bin\\keitora.exe\" session gate --provider codex 2>$null; if ($LASTEXITCODE -ne 0) { & \"C:\\bin\\keitora.exe\" session waiting *> $null } }; exit 0 # keitora:codex-session-hook"
        );
        assert_eq!(
            build_command(b, CodexCmd::Stop, true),
            "if (Test-Path -LiteralPath \"C:\\bin\\keitora.exe\") { & \"C:\\bin\\keitora.exe\" session idle --stdin 2>$null; if ($LASTEXITCODE -ne 0) { & \"C:\\bin\\keitora.exe\" session idle *> $null } }; exit 0 # keitora:codex-session-hook"
        );
    }

    #[test]
    fn gate_and_stop_keep_stdout_unredirected() {
        for windows in [false, true] {
            for cmd in [CodexCmd::Gate, CodexCmd::Stop] {
                let c = build_command("/abs/keitora", cmd, windows);
                assert!(c.contains("session gate") || c.contains("--stdin"), "{c}");
                // the primary invocation discards stderr only
                assert!(
                    c.contains("2>$null;") || c.contains("2>/dev/null ||"),
                    "{c}"
                );
            }
        }
    }

    #[test]
    fn timeout_only_on_permission_request() {
        let out = merged_codex_hooks(json!({}), "/abs/keitora");
        assert_eq!(
            out["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"],
            600
        );
        for (event, _cmd) in CODEX_HOOK_MAP {
            if event != "PermissionRequest" {
                assert!(
                    out["hooks"][event][0]["hooks"][0].get("timeout").is_none(),
                    "{event}"
                );
            }
        }
    }

    #[test]
    fn is_ours_matches_every_command_shape() {
        for windows in [false, true] {
            for cmd in [CodexCmd::State("idle"), CodexCmd::Gate, CodexCmd::Stop] {
                let entry = json!({ "hooks": [ { "type": "command", "command": build_command("/abs/keitora", cmd, windows) } ] });
                assert!(is_ours(&entry), "{cmd:?} windows={windows}");
            }
        }
    }

    #[test]
    fn user_permission_request_hook_preserved() {
        let existing = json!({
            "hooks": { "PermissionRequest": [ { "hooks": [ { "type": "command", "command": "/usr/bin/mine.sh" } ] } ] }
        });
        let out = merged_codex_hooks(existing, "/abs/keitora");
        assert_eq!(
            commands_for(&out, "PermissionRequest"),
            vec![
                "/usr/bin/mine.sh".to_string(),
                hook_command("/abs/keitora", CodexCmd::Gate)
            ]
        );
    }

    #[test]
    fn install_writes_once_in_temp_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        install_codex_hooks_for(tmp.path(), "/abs/keitora");
        let file = tmp.path().join(".codex/hooks.json");
        let first = std::fs::read_to_string(&file).unwrap();
        assert!(first.contains("session gate --provider codex"));
        install_codex_hooks_for(tmp.path(), "/abs/keitora");
        assert_eq!(first, std::fs::read_to_string(&file).unwrap());
    }

    #[test]
    fn is_idempotent() {
        let once = merged_codex_hooks(json!({}), "/abs/keitora");
        let twice = merged_codex_hooks(once.clone(), "/abs/keitora");
        assert_eq!(once, twice, "applying twice must not change anything");
    }

    #[test]
    fn preserves_unrelated_keys_and_hooks() {
        let existing = json!({
            "someOtherKey": { "a": 1 },
            "hooks": {
                "Stop": [ { "hooks": [ { "type": "command", "command": "/usr/bin/mine.sh" } ] } ],
                "PostToolUse": [ { "hooks": [ { "type": "command", "command": "/usr/bin/audit.sh" } ] } ]
            }
        });
        let out = merged_codex_hooks(existing, "/abs/keitora");
        // unrelated top-level key untouched
        assert_eq!(out["someOtherKey"]["a"], 1);
        // an unrelated event we don't manage is left intact
        assert_eq!(commands_for(&out, "PostToolUse"), vec!["/usr/bin/audit.sh"]);
        // the user's own Stop hook is kept, ours appended
        let stop = commands_for(&out, "Stop");
        assert!(stop.contains(&"/usr/bin/mine.sh".to_string()));
        assert!(stop.contains(&hook_command("/abs/keitora", CodexCmd::Stop)));
        assert_eq!(stop.len(), 2);
    }

    #[test]
    fn updates_changed_binary_path_without_duplicating() {
        let first = merged_codex_hooks(json!({}), "/old/keitora");
        let second = merged_codex_hooks(first, "/new/keitora");
        assert_eq!(
            commands_for(&second, "Stop"),
            vec![hook_command("/new/keitora", CodexCmd::Stop)]
        );
        assert_eq!(
            commands_for(&second, "UserPromptSubmit"),
            vec![hook_command("/new/keitora", CodexCmd::State("busy"))]
        );
    }

    #[test]
    fn legacy_bare_command_is_replaced_not_duplicated() {
        // A worktree carrying an older unmarked `keitora session …` command must be
        // recognized as ours and upgraded to the guarded form — not left as a dupe.
        let legacy = json!({
            "hooks": {
                "SessionEnd": [ {
                    "hooks": [ { "type": "command", "command": "/abs/keitora session offline" } ]
                } ]
            }
        });
        let out = merged_codex_hooks(legacy, "/abs/keitora");
        assert_eq!(
            commands_for(&out, "SessionEnd"),
            vec![hook_command("/abs/keitora", CodexCmd::State("offline"))]
        );
    }
}
