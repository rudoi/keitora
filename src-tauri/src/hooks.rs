// Auto-install the session-state hooks into each worktree's
// `.claude/settings.local.json` on startup. We write `settings.local.json` (not
// `settings.json`) because it is gitignored — our machine-specific absolute hook
// paths must not dirty every worktree's `git status`. Claude Code merges both and
// hot-reloads, so a running session picks the hooks up without a restart.
//
// The merge is idempotent and additive: it preserves unrelated keys (permissions,
// other hooks) and replaces only our own entries (so a rebuilt binary path updates
// cleanly instead of duplicating).

use std::path::Path;

use keitora_tasks::Config;
use serde_json::{json, Value};

/// (event, matcher, state). `Notification` is filtered to ONLY `permission_prompt`
/// — i.e. Claude is actually blocked asking permission. We deliberately exclude
/// `idle_prompt` (the "you've been idle" nudge): the session is idle/ready there,
/// not waiting mid-task, and mapping it to `waiting` would wrongly flip a fresh
/// idle session. `StopFailure` mirrors `Stop` so an errored turn doesn't stick at
/// `busy`. `SubagentStop` is intentionally absent — it fires mid-turn and would
/// wrongly flip the main session to idle.
const HOOK_MAP: [(&str, &str, &str); 6] = [
    ("UserPromptSubmit", "", "busy"),
    ("Notification", "permission_prompt", "waiting"),
    ("Stop", "", "idle"),
    ("StopFailure", "", "idle"),
    ("SessionStart", "", "idle"),
    ("SessionEnd", "", "offline"),
];

/// Marker embedded in every command we write, so we can recognize (and replace)
/// our own entries regardless of the surrounding shell guard.
/// Detectable substring identifying a hook entry as ours (so a refresh replaces
/// rather than duplicates it). Carried as a trailing `# …` (sh) or `rem …` (cmd)
/// comment depending on platform; [`is_ours`] matches the bare token.
const HOOK_MARKER: &str = "keitora:session-hook";

/// The resilient command we install for one (binary, state).
///
/// Claude Code runs hooks through a **POSIX shell on every platform** — `/bin/sh`
/// on unix and Git Bash on Windows (verified: the hook runner is `/usr/bin/bash`,
/// which is why a `cmd.exe`-syntax command fails with `rem: command not found` and
/// `>nul` leaks stray files). So this is POSIX everywhere: the `[ -x ]` test
/// short-circuits a missing/not-yet-built binary, output is silenced, and `|| true`
/// guarantees exit 0 so Claude never sees a hook failure.
///
/// The only Windows-specific need is stripping the `\\?\` extended-length prefix the
/// resolved build path can carry (Git Bash's `[ -x ]` and execution both handle a
/// plain `C:\…\keitora.exe`, but not a `\\?\…` one). The strip is UNC-safe and a no-op
/// on unix.
fn hook_command(task_bin: &str, state: &str) -> String {
    let bin = keitora_tasks::strip_extended_prefix_str(task_bin);
    // Git Bash (which Claude Code uses to run hooks on Windows) resolves a
    // forward-slashed drive path (`D:/…/keitora.exe`) unambiguously; convert
    // backslashes so `[ -x ]`/exec never depend on bash's backslash-in-quotes
    // handling. No-op on unix (no backslashes there).
    #[cfg(windows)]
    let bin = bin.replace('\\', "/");
    format!("[ -x \"{bin}\" ] && \"{bin}\" session {state} >/dev/null 2>&1 || true # {HOOK_MARKER}")
}

/// True if a hook-group entry is one we manage. Matches the current marker and the
/// bare `…/keitora session …` / legacy `…/task session …` forms, so `task`-era
/// installs are recognized and replaced — not duplicated — on the next refresh.
/// Hooks left under the previous app name are intentionally NOT matched (the
/// hard cut): they are left in place for the user to clear.
fn is_ours(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|h| {
                h.get("command").and_then(|c| c.as_str()).is_some_and(|c| {
                    c.contains(HOOK_MARKER)
                        || c.contains("/keitora session ")
                        || c.contains("/task session ")
                })
            })
        })
}

/// Pure merge: return `existing` with our session hooks ensured. Idempotent;
/// preserves unrelated keys and any non-ours hook entries on shared events.
pub fn merged_hooks_settings(existing: Value, task_bin: &str) -> Value {
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

    for (event, matcher, state) in HOOK_MAP {
        let command = hook_command(task_bin, state);
        let entry = json!({
            "matcher": matcher,
            "hooks": [ { "type": "command", "command": command } ],
        });
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

/// True if `p` is an existing regular file with any execute bit set.
#[cfg(unix)]
fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

/// Windows has no execute bit; a file is "runnable" if it exists and carries an
/// executable extension (`.exe`/`.cmd`/`.bat`) — the forms a hook `command` can
/// invoke directly.
#[cfg(windows)]
fn is_exec(p: &Path) -> bool {
    if !std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false) {
        return false;
    }
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("exe" | "cmd" | "bat")
    )
}

/// Resolve the absolute `keitora` binary to embed in `wt_path`'s session hooks.
///
/// Order: `keitora` on the user's login PATH (`path_keitora`, precomputed by the caller via
/// `keitora_tasks::keitora_on_path()` so we don't re-scan PATH per worktree) → the
/// `keitora.toml [cli].path` override (`override_path`) → the worktree's own fresh build.
/// `None` → the caller skips install for this worktree (clean no-op). The hooks must
/// not go through the `./keitora` wrapper's `cargo run` fallback — far too slow per turn.
///
/// PATH wins by design: the installed CLI is unambiguous and can never be the GUI app,
/// so the per-turn session hooks always run the real CLI. The worktree build is only a
/// last resort — automatic dogfooding for a dev with neither an installed `keitora` nor an
/// override — because resolving it FIRST kept biting: a stale build, or the GUI app on
/// a pre-rename branch, would get run by every hook.
///
/// CRITICAL: even as a last resort, a worktree's `target/<profile>/keitora` is the CLI
/// only on a build that has adopted the rename (where the Tauri app builds as
/// `keitora-app`). On an older branch the SAME path may be the GUI app, and embedding
/// it would make every per-turn hook run `<gui> session <state>` — which launches a
/// window. So the build candidate is trusted ONLY when a sibling `keitora-app` is
/// present (the unambiguous marker that `keitora` here is the CLI).
pub fn resolve_keitora_bin(
    wt_path: &Path,
    override_path: Option<&str>,
    path_keitora: Option<&str>,
) -> Option<String> {
    // PATH first: the installed CLI is the safe, unsurprising choice and can never be
    // the GUI app. It wins even over an explicit `[cli].path` override.
    if let Some(p) = path_keitora {
        return Some(p.to_string());
    }
    if let Some(o) = override_path {
        if is_exec(Path::new(o)) {
            return Some(o.to_string());
        }
    }
    // The worktree build is the LAST resort (e.g. a dev with no installed `keitora` on
    // PATH and no override) — and only when the sibling `keitora-app` marker proves this
    // build renamed the app, so `keitora` here is the CLI and not the GUI.
    for prof in ["release", "debug"] {
        // `EXE_SUFFIX` is "" on Unix and ".exe" on Windows, so the build artifacts
        // are `keitora`/`keitora-app` or `keitora.exe`/`keitora-app.exe` respectively. Join
        // component-by-component so the resulting path uses native separators (a
        // forward-slash `format!` would emit a mixed-separator path on Windows that
        // could confuse the hook `command`).
        let exe = std::env::consts::EXE_SUFFIX;
        let dir = wt_path.join("target").join(prof);
        let cli = dir.join(format!("keitora{exe}"));
        let app = dir.join(format!("keitora-app{exe}"));
        // `is_file()` (not `exists()`) so a directory named `keitora-app` can't satisfy it.
        if is_exec(&cli) && app.is_file() {
            return Some(cli.to_string_lossy().into_owned());
        }
    }
    None
}

/// Pure: the pane env pair list for an already-resolved `keitora` binary.
pub fn keitora_bin_pair(bin: Option<String>) -> Vec<(String, String)> {
    bin.map(|b| vec![("KEITORA_BIN".to_string(), b)])
        .unwrap_or_default()
}

/// `[("KEITORA_BIN", bin)]` when a `keitora` binary resolves for `cwd`, else empty.
/// Resolution is [`resolve_keitora_bin`] with the user's `[cli].path` override and
/// the `keitora` on PATH (looked up once per process: on unix it runs a login shell).
/// Empty under `cfg!(test)` (no login shell in unit tests).
pub fn keitora_bin_extra_env(cwd: &Path) -> Vec<(String, String)> {
    if cfg!(test) {
        return Vec::new();
    }
    let on_path = keitora_on_path_cached();
    let user = keitora_tasks::UserConfig::load().unwrap_or_default();
    let user_override = keitora_tasks::resolve_keitora_path(None, &user);
    keitora_bin_pair(resolve_keitora_bin(
        cwd,
        user_override.as_deref(),
        on_path.as_deref(),
    ))
}

/// The `keitora` on PATH, looked up once per process.
fn keitora_on_path_cached() -> &'static Option<String> {
    static ON_PATH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ON_PATH.get_or_init(keitora_tasks::keitora_on_path)
}

/// Start the once-per-process `keitora` PATH lookup on a background thread, so
/// the first pane spawn finds it done instead of running it (on unix, a login
/// shell) while holding up every concurrent spawn. Skipped under `cfg!(test)`.
pub fn warm_keitora_on_path() {
    if cfg!(test) {
        return;
    }
    std::thread::spawn(|| {
        keitora_on_path_cached();
    });
}

/// Install/refresh the session hooks for a single worktree at `wt_path`.
///
/// Reads `.claude/settings.local.json`, merges our hooks in, and writes back only
/// if the content actually changed (preserving a clean `git status` for unchanged
/// worktrees).
pub fn install_hooks_for(wt_path: &Path, task_bin: &str) {
    let dir = wt_path.join(".claude");
    let file = dir.join("settings.local.json");

    let existing: Value = match std::fs::read_to_string(&file) {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "[keitora] session hooks: skip {} (parse error: {e})",
                    file.display()
                );
                return;
            }
        },
        Err(_) => json!({}), // file absent → start fresh
    };

    let merged = merged_hooks_settings(existing.clone(), task_bin);
    if merged == existing {
        return; // already installed → no write (keep git status clean)
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "[keitora] session hooks: mkdir {} failed: {e}",
            dir.display()
        );
        return;
    }
    let pretty = serde_json::to_string_pretty(&merged).unwrap_or_default();
    if let Err(e) = std::fs::write(&file, format!("{pretty}\n")) {
        eprintln!(
            "[keitora] session hooks: write {} failed: {e}",
            file.display()
        );
    }
}

/// Install/refresh the hooks for every configured worktree that exists on disk.
///
/// The `keitora` binary is resolved per worktree (build → `[cli].path` override → PATH),
/// so hooks install in any project where `keitora` is reachable — not just the keitora repo
/// with an in-tree build. Worktrees with no resolvable binary are skipped (not fatal).
pub fn install_session_hooks() {
    let Ok(config) = Config::find() else {
        return; // no config yet → nothing to install
    };
    install_hooks_into(config);
}

/// Active-project variant of [`install_session_hooks`]: install hooks for the
/// worktrees of the registry's **active** project (via [`Config::find_active`]),
/// not whatever the cwd resolves to. Used by the GUI commands that activate a
/// project (switch / re-discover-adopt / onboard) so the now-active project's
/// worktrees get hooks even though the process cwd never changes. No-op if no
/// active project is resolvable.
pub fn install_active_session_hooks() {
    let Ok(config) = Config::find_active() else {
        return;
    };
    install_hooks_into(config);
}

/// Install/refresh session hooks for every existing worktree of `config`. Shared
/// by the startup (cwd) and active-project install paths.
///
/// The `keitora` binary override is derived from THIS config's project (not a fresh
/// `resolve_active()`) so the embedded path matches the project we install into.
/// The PATH scan (`keitora_on_path`) runs a login shell, so both candidates are
/// computed once, outside the per-worktree loop.
fn install_hooks_into(config: Config) {
    let user = keitora_tasks::UserConfig::load().unwrap_or_default();
    let override_path = keitora_tasks::resolve_keitora_path(Some(config.project_config()), &user);
    let path_keitora = keitora_tasks::keitora_on_path();

    let worktrees = match config.resolved_worktrees() {
        Ok(w) => w,
        Err(_) => return,
    };

    let mut warned_none = false;
    for wt in worktrees {
        if !wt.path.is_dir() {
            continue;
        }
        match resolve_keitora_bin(&wt.path, override_path.as_deref(), path_keitora.as_deref()) {
            Some(bin) => {
                install_hooks_for(&wt.path, &bin);
                // Codex worktrees additionally get a worktree-local `.codex/hooks.json`
                // that reports the same session states. Gated on
                // `CliKind::Codex` so a hand-run `codex` in a Claude worktree is never
                // prompted to trust our hooks.
                if wt.cli == keitora_tasks::CliKind::Codex {
                    crate::codex_hooks::install_codex_hooks_for(&wt.path, &bin);
                }
            }
            None => {
                if !warned_none {
                    eprintln!(
                        "[keitora] session hooks: no `keitora` binary found (build, [cli].path, or PATH); \
                         skipping install for worktrees without one"
                    );
                    warned_none = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keitora_bin_pair_sets_the_variable() {
        assert_eq!(
            keitora_bin_pair(Some("/x/keitora".into())),
            vec![("KEITORA_BIN".to_string(), "/x/keitora".to_string())]
        );
        assert!(keitora_bin_pair(None).is_empty());
    }

    #[test]
    fn keitora_bin_extra_env_is_empty_under_test() {
        assert!(keitora_bin_extra_env(Path::new("/home/u/proj")).is_empty());
    }

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
    fn empty_input_gets_all_six_events() {
        let out = merged_hooks_settings(json!({}), "/abs/keitora");
        for (event, _m, state) in HOOK_MAP {
            assert_eq!(
                commands_for(&out, event),
                vec![hook_command("/abs/keitora", state)],
                "event {event}"
            );
        }
        // The installed command (POSIX on every platform — Claude Code runs hooks via
        // a POSIX shell, incl. Git Bash on Windows) is guarded + silenced + never fails.
        let cmd = &commands_for(&out, "Stop")[0];
        assert!(cmd.starts_with("[ -x \"/abs/keitora\" ]"), "guarded: {cmd}");
        assert!(cmd.contains("|| true"), "never errors: {cmd}");
        assert!(cmd.contains(HOOK_MARKER), "carries marker: {cmd}");
        // matcher carried through (Notification filtered to permission prompts only).
        assert_eq!(
            out["hooks"]["Notification"][0]["matcher"],
            "permission_prompt"
        );
        assert_eq!(out["hooks"]["Stop"][0]["matcher"], "");
    }

    #[test]
    fn is_idempotent() {
        let once = merged_hooks_settings(json!({}), "/abs/keitora");
        let twice = merged_hooks_settings(once.clone(), "/abs/keitora");
        assert_eq!(once, twice, "applying twice must not change anything");
    }

    #[test]
    fn preserves_unrelated_keys_and_hooks() {
        let existing = json!({
            "permissions": { "allow": ["Bash(ls)"] },
            "hooks": {
                "Stop": [ { "matcher": "", "hooks": [ { "type": "command", "command": "/usr/bin/mine.sh" } ] } ]
            }
        });
        let out = merged_hooks_settings(existing, "/abs/keitora");
        // permissions untouched
        assert_eq!(out["permissions"]["allow"][0], "Bash(ls)");
        // the user's Stop hook is kept, ours appended
        let stop = commands_for(&out, "Stop");
        assert!(stop.contains(&"/usr/bin/mine.sh".to_string()));
        assert!(stop.contains(&hook_command("/abs/keitora", "idle")));
        assert_eq!(stop.len(), 2);
    }

    #[test]
    fn legacy_bare_command_is_replaced_not_duplicated() {
        // A worktree still carrying the old unguarded format must be recognized
        // as ours and upgraded to the guarded form — not left alongside a dupe.
        let legacy = json!({
            "hooks": {
                "Stop": [ {
                    "matcher": "",
                    "hooks": [ { "type": "command", "command": "/abs/keitora session idle" } ]
                } ]
            }
        });
        let out = merged_hooks_settings(legacy, "/abs/keitora");
        assert_eq!(
            commands_for(&out, "Stop"),
            vec![hook_command("/abs/keitora", "idle")]
        );
    }

    #[test]
    fn updates_changed_binary_path_without_duplicating() {
        let first = merged_hooks_settings(json!({}), "/old/keitora");
        let second = merged_hooks_settings(first, "/new/keitora");
        // only the new path remains for each event (no stale /old entry)
        assert_eq!(
            commands_for(&second, "Stop"),
            vec![hook_command("/new/keitora", "idle")]
        );
        assert_eq!(
            commands_for(&second, "UserPromptSubmit"),
            vec![hook_command("/new/keitora", "busy")]
        );
    }

    #[test]
    fn migrates_pre_rename_task_command_to_keitora() {
        // A worktree carrying the pre-rename `…/task session …` command must be
        // recognized as ours and upgraded to the `keitora` command — not duplicated.
        let pre_rename = json!({
            "hooks": {
                "Stop": [ {
                    "matcher": "",
                    "hooks": [ { "type": "command", "command": "/main/target/debug/task session idle" } ]
                } ]
            }
        });
        let out = merged_hooks_settings(pre_rename, "/main/target/debug/keitora");
        assert_eq!(
            commands_for(&out, "Stop"),
            vec![hook_command("/main/target/debug/keitora", "idle")],
            "pre-rename task command upgraded to keitora, no duplicate"
        );
    }

    // resolve_keitora_bin's runnability gate is exec-bit-based on unix; the windows
    // PATHEXT equivalent (D2) and its test land in stage 050.
    #[cfg(unix)]
    #[test]
    fn resolve_keitora_bin_prefers_path_then_override_then_build() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        let dbg = wt.join("target/debug");
        std::fs::create_dir_all(&dbg).unwrap();

        let mk_exec = |p: &Path| {
            std::fs::write(p, b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let override_bin = tmp.path().join("override-keitora");
        let path_bin = tmp.path().join("path-keitora");
        mk_exec(&override_bin);
        mk_exec(&path_bin);
        let override_s = override_bin.to_string_lossy().into_owned();
        let path_s = path_bin.to_string_lossy().into_owned();

        // PATH wins even over an explicit override — the installed CLI is always
        // preferred so a stale/GUI build can never be embedded in the hooks.
        assert_eq!(
            resolve_keitora_bin(&wt, Some(&override_s), Some(&path_s)).as_deref(),
            Some(path_s.as_str()),
            "PATH must win even when an override is set"
        );
        // No PATH → the override is the fallback.
        assert_eq!(
            resolve_keitora_bin(&wt, Some(&override_s), None).as_deref(),
            Some(override_s.as_str())
        );
        // Only PATH available → PATH.
        assert_eq!(
            resolve_keitora_bin(&wt, None, Some(&path_s)).as_deref(),
            Some(path_s.as_str())
        );
        // Nothing available → None (caller skips install).
        assert_eq!(resolve_keitora_bin(&wt, None, None), None);

        // A renamed worktree build (`keitora` + sibling `keitora-app`) is used ONLY as a last
        // resort, when there is neither a PATH `keitora` nor an override.
        let build = dbg.join("keitora");
        mk_exec(&build);
        mk_exec(&dbg.join("keitora-app"));
        assert_eq!(
            resolve_keitora_bin(&wt, None, None).as_deref(),
            Some(build.to_string_lossy().as_ref()),
            "build is the last-resort fallback when nothing else resolves"
        );
        // ...but PATH and the override still outrank it.
        assert_eq!(
            resolve_keitora_bin(&wt, Some(&override_s), Some(&path_s)).as_deref(),
            Some(path_s.as_str())
        );
        assert_eq!(
            resolve_keitora_bin(&wt, Some(&override_s), None).as_deref(),
            Some(override_s.as_str())
        );

        // A non-executable build file is ignored even with the sibling present —
        // `is_exec(&cli)` must gate the build candidate, not just `keitora-app.is_file()`.
        std::fs::remove_file(&build).unwrap();
        std::fs::write(&build, b"not exec\n").unwrap();
        assert_eq!(
            resolve_keitora_bin(&wt, None, None),
            None,
            "a non-executable build must not be used even with a sibling keitora-app"
        );
        mk_exec(&build); // restore for the next case

        // CRITICAL: a build `target/debug/keitora` WITHOUT a sibling `keitora-app` is the
        // pre-rename GUI app — it must NOT be trusted (else hooks launch a window),
        // even as a last resort.
        std::fs::remove_file(dbg.join("keitora-app")).unwrap();
        assert_eq!(
            resolve_keitora_bin(&wt, None, None),
            None,
            "keitora without sibling keitora-app must be treated as the GUI app, not the CLI"
        );
    }

    /// Windows: `is_exec` is extension-based, and `resolve_keitora_bin` probes the
    /// `.exe` build artifacts (`keitora.exe` + sibling `keitora-app.exe`).
    #[cfg(windows)]
    #[test]
    fn resolve_keitora_bin_windows_probes_exe() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        let dbg = wt.join("target/debug");
        std::fs::create_dir_all(&dbg).unwrap();

        // Extension gate.
        let exe = dbg.join("keitora.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        assert!(is_exec(&exe), ".exe must be runnable");
        let txt = dbg.join("notes.txt");
        std::fs::write(&txt, b"x").unwrap();
        assert!(!is_exec(&txt), ".txt must not be runnable");
        let cmd = dbg.join("claude.cmd");
        std::fs::write(&cmd, b"@echo off").unwrap();
        assert!(is_exec(&cmd), ".cmd must be runnable");

        // Build candidate needs the sibling keitora-app.exe marker.
        assert_eq!(
            resolve_keitora_bin(&wt, None, None),
            None,
            "keitora.exe without sibling keitora-app.exe is the GUI app, not the CLI"
        );
        std::fs::write(dbg.join("keitora-app.exe"), b"MZ").unwrap();
        // Compare as PathBuf so separator style (native vs the test's join) doesn't
        // matter — only that it resolves to the same file.
        assert_eq!(
            resolve_keitora_bin(&wt, None, None).map(std::path::PathBuf::from),
            Some(exe.clone()),
            "keitora.exe + sibling keitora-app.exe resolves as the CLI build"
        );
    }
}
