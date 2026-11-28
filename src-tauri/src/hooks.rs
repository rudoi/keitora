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
///
/// `Stop`/`StopFailure` also capture the turn's reply for a driver (`--stdin` /
/// `--failed`), still ending in the same `idle` state.
const HOOK_MAP: [(&str, &str, HookCmd); 6] = [
    ("UserPromptSubmit", "", HookCmd::State("busy")),
    (
        "Notification",
        "permission_prompt",
        HookCmd::State("waiting"),
    ),
    ("Stop", "", HookCmd::Stop),
    ("StopFailure", "", HookCmd::StopFailure),
    ("SessionStart", "", HookCmd::State("idle")),
    ("SessionEnd", "", HookCmd::State("offline")),
];

/// (event, matcher) pairs that run the permission/question gate. `PermissionRequest`
/// covers every tool; `PreToolUse` is limited to `AskUserQuestion` so a driver can
/// answer questions. Both carry [`keitora_tasks::DRIVE_HOOK_TIMEOUT_S`].
const CLAUDE_GATE_MAP: [(&str, &str); 2] =
    [("PermissionRequest", ""), ("PreToolUse", "AskUserQuestion")];

/// Which command a hook entry runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookCmd {
    /// `session <state>`, all streams silenced.
    State(&'static str),
    /// Turn end: capture the reply from stdin, then record idle.
    Stop,
    /// Errored turn end: record the failure, then idle.
    StopFailure,
    /// Permission/question gate; stdout reaches Claude, stderr is discarded.
    Gate,
}

/// Marker embedded in every command we write, so we can recognize (and replace)
/// our own entries regardless of the surrounding shell guard.
/// Detectable substring identifying a hook entry as ours (so a refresh replaces
/// rather than duplicates it). Carried as a trailing `# …` (sh) or `rem …` (cmd)
/// comment depending on platform; [`is_ours`] matches the bare token.
const HOOK_MARKER: &str = "keitora:session-hook";

/// POSIX guard that runs `session <primary>` with only stderr discarded (so stdout
/// reaches the agent) and falls back to a plain silenced `session <fallback>` when an
/// older installed CLI rejects the primary form; exit 0 is forced.
pub(crate) fn posix_fallback_command(
    bin: &str,
    primary: &str,
    fallback: &str,
    marker: &str,
) -> String {
    format!(
        "[ -x \"{bin}\" ] && {{ \"{bin}\" session {primary} 2>/dev/null || \"{bin}\" session {fallback} >/dev/null 2>&1; }} || true # {marker}"
    )
}

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
///
/// `launch` picks the copy: `true` for the per-launch settings file a Keitora launch
/// passes with `--settings`; `false` for the project file, whose commands start
/// with the [`LAUNCH_HOOKS_ENV`] guard. Claude runs both files' hooks, so the guard
/// makes a Keitora-launched Claude (which has the variable set) run only the
/// launch copy, and a hand-started one run only the project copy.
fn hook_command(task_bin: &str, cmd: HookCmd, launch: bool) -> String {
    let command = hook_command_unguarded(task_bin, cmd);
    if launch {
        command
    } else {
        format!("[ -z \"${LAUNCH_HOOKS_ENV}\" ] && {command}")
    }
}

/// [`hook_command`] without the launch guard.
fn hook_command_unguarded(task_bin: &str, cmd: HookCmd) -> String {
    let bin = keitora_tasks::strip_extended_prefix_str(task_bin);
    // Git Bash (which Claude Code uses to run hooks on Windows) resolves a
    // forward-slashed drive path (`D:/…/keitora.exe`) unambiguously; convert
    // backslashes so `[ -x ]`/exec never depend on bash's backslash-in-quotes
    // handling. No-op on unix (no backslashes there).
    #[cfg(windows)]
    let bin = bin.replace('\\', "/");
    let m = HOOK_MARKER;
    match cmd {
        // The primary records the payload's session id (and prints nothing); an
        // older CLI that rejects the flag still records the state.
        HookCmd::State(state) => {
            posix_fallback_command(&bin, &format!("{state} --payload-session"), state, m)
        }
        // Only fd 2 is redirected so stdout (the decision) reaches Claude; `|| true`
        // forces exit 0 whatever the child did.
        HookCmd::Gate => format!(
            "[ -x \"{bin}\" ] && \"{bin}\" session gate --provider claude 2>/dev/null || true # {m}"
        ),
        // Fall back to a plain `session idle` so an older installed CLI still records
        // idle and a session can never stick at `busy`.
        HookCmd::Stop => posix_fallback_command(&bin, "idle --stdin", "idle", m),
        HookCmd::StopFailure => posix_fallback_command(&bin, "idle --failed", "idle", m),
    }
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

/// Pure merge: return `existing` with our session hooks ensured, in their
/// project-file form (see [`hook_command`]). Idempotent; preserves unrelated keys and
/// any non-ours hook entries on shared events.
pub fn merged_hooks_settings(existing: Value, task_bin: &str) -> Value {
    merged_hooks_settings_for(existing, task_bin, false)
}

/// The per-launch settings file's content: our hooks only, without the guard.
pub fn launch_hooks_settings(task_bin: &str) -> Value {
    merged_hooks_settings_for(json!({}), task_bin, true)
}

/// [`merged_hooks_settings`] for either copy (`launch`, see [`hook_command`]).
fn merged_hooks_settings_for(existing: Value, task_bin: &str, launch: bool) -> Value {
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

    let mut install = |event: &str, entry: Value| {
        let arr = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        let arr = arr.as_array_mut().unwrap();
        // Drop our prior entries (every command we write carries the marker) →
        // idempotent + path-change safe; a user's own hooks on the event stay.
        arr.retain(|e| !is_ours(e));
        arr.push(entry);
    };

    for (event, matcher, cmd) in HOOK_MAP {
        install(
            event,
            json!({
                "matcher": matcher,
                "hooks": [ { "type": "command", "command": hook_command(task_bin, cmd, launch) } ],
            }),
        );
    }
    for (event, matcher) in CLAUDE_GATE_MAP {
        install(
            event,
            json!({
                "matcher": matcher,
                "hooks": [ {
                    "type": "command",
                    "command": hook_command(task_bin, HookCmd::Gate, launch),
                    "timeout": keitora_tasks::DRIVE_HOOK_TIMEOUT_S,
                } ],
            }),
        );
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

/// The candidate `keitora` binaries other than a worktree's own build, gathered once and
/// handed to [`resolve_keitora_bin`]. Named fields keep the three same-typed paths from
/// being swapped at a call site.
#[derive(Debug, Default, Clone)]
pub struct KeitoraBinSources {
    /// The `keitora.toml [cli].path` override (project over user).
    pub override_path: Option<String>,
    /// The CLI bundled next to the running app (see [`app_bundled_cli`]).
    pub app_cli: Option<String>,
    /// `keitora` on the user's login PATH.
    pub on_path: Option<String>,
}

impl KeitoraBinSources {
    /// Gather the sources for `project` (`None` → user-level override only). `on_path` is
    /// passed in because its lookup runs a login shell on unix: callers hold the result
    /// (computed once per process, or once per install pass) rather than re-scanning.
    pub fn gather(project: Option<&keitora_tasks::ProjectConfig>, on_path: Option<String>) -> Self {
        let user = keitora_tasks::UserConfig::load().unwrap_or_default();
        Self {
            override_path: keitora_tasks::resolve_keitora_path(project, &user),
            app_cli: app_bundled_cli().map(str::to_string),
            on_path,
        }
    }
}

/// Resolve the absolute `keitora` binary to embed in `wt_path`'s session hooks.
///
/// Order: `keitora` on the user's login PATH (`sources.on_path`, precomputed by the caller so
/// we don't re-scan PATH per worktree) → the `keitora.toml [cli].path` override
/// (`sources.override_path`) → the CLI bundled next to the running app (`sources.app_cli`)
/// → the worktree's own fresh build.
/// `None` → the caller skips install for this worktree (clean no-op). The hooks must
/// not go through the `./keitora` wrapper's `cargo run` fallback — far too slow per turn.
///
/// The bundled CLI outranks the worktree build because the installed app's process
/// PATH often lacks the installer's bin dir: without it, every worktree fell back to
/// its own build, which can be months old and miss newer subcommands the hooks call.
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
pub fn resolve_keitora_bin(wt_path: &Path, sources: &KeitoraBinSources) -> Option<String> {
    // PATH first: the installed CLI is the safe, unsurprising choice and can never be
    // the GUI app. It wins even over an explicit `[cli].path` override.
    if let Some(p) = &sources.on_path {
        return Some(p.clone());
    }
    if let Some(o) = &sources.override_path {
        if is_exec(Path::new(o)) {
            return Some(o.clone());
        }
    }
    // The CLI shipped with the running app: always as new as the app itself.
    if let Some(s) = &sources.app_cli {
        if is_exec(Path::new(s)) {
            return Some(s.clone());
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

/// The `keitora` CLI to embed in a file every worktree shares (the global agy
/// hooks): on PATH, then the `[cli].path` override, then the app-bundled CLI. Never
/// a worktree build. Only an absolute path to a runnable file counts.
pub fn resolve_global_keitora_bin(sources: &KeitoraBinSources) -> Option<String> {
    [&sources.on_path, &sources.override_path, &sources.app_cli]
        .into_iter()
        .flatten()
        .find(|p| {
            let p = Path::new(p.as_str());
            p.is_absolute() && is_exec(p)
        })
        .cloned()
}

/// Pure: the pane env pair list for an already-resolved `keitora` binary.
pub fn keitora_bin_pair(bin: Option<String>) -> Vec<(String, String)> {
    bin.map(|b| vec![("KEITORA_BIN".to_string(), b)])
        .unwrap_or_default()
}

/// The pane marker every Keitora agent pane carries: the global Antigravity hook
/// does nothing in a shell without it.
pub const PANE_MARKER_ENV: &str = "KEITORA_PANE";

/// The codex flag that lets its project hooks (and herdr's session hook) run
/// without an interactive trust prompt.
pub const TRUST_BYPASS_FLAG: &str = "--dangerously-bypass-hook-trust";

/// Set in the pane of a Claude launched with the per-launch settings file; the
/// project file's hooks do nothing while it is set (see [`hook_command`]).
pub const LAUNCH_HOOKS_ENV: &str = "KEITORA_LAUNCH_HOOKS";

/// The per-launch Claude settings file, under the Keitora home. One file serves
/// every worktree: its commands take the worktree from the hook's cwd.
pub const CLAUDE_LAUNCH_SETTINGS: &str = "claude-launch-settings.json";

/// Bump whenever any generated hook command text changes, so a launch made with
/// older hooks can be told apart.
pub const HOOK_VERSION: u32 = 3;

/// What one hook-file install did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileWrite {
    /// The file changed on disk.
    Written,
    /// The file already held our hooks.
    Unchanged,
    /// The file applies but no `keitora` binary resolved, so nothing was written.
    Skipped,
    /// Reading, parsing or writing the file failed (logged).
    Failed,
    /// The provider does not use this file.
    NotApplicable,
}

/// Whether a Claude launch passes its hooks in a per-launch settings file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeSettings {
    /// Passed: the `--settings` argument.
    Applied(String),
    /// Why not (e.g. the project file holds the hooks, or the path would need
    /// shell quoting).
    Skipped(String),
    NotApplicable,
}

/// What [`prepare_worktree`] put in place for one launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// The `keitora` binary the hooks run; `None` skips every file.
    pub bin_resolved: Option<String>,
    /// `.claude/settings.local.json` (Claude and Codex worktrees).
    pub claude_file: FileWrite,
    pub claude_settings: ClaudeSettings,
    /// `.codex/hooks.json` (Codex worktrees).
    pub codex_file: FileWrite,
}

/// The path text to pass as Claude's `--settings` argument, or why not. The argv
/// reaches an interactive shell prompt through herdr, so only a path that needs
/// no quoting in any shell is passed: after dropping a `\\?\` prefix and turning
/// backslashes into `/`, it must match `^[A-Za-z0-9_./:@+-]+$`.
pub fn claude_settings_arg(path: &Path) -> Result<String, String> {
    let text = keitora_tasks::strip_extended_prefix_str(&path.to_string_lossy()).replace('\\', "/");
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:@+-".contains(c));
    if plain {
        Ok(text)
    } else {
        Err("path needs quoting".to_string())
    }
}

/// Write the per-launch settings file under `keitora_home` (atomically, only when its
/// content differs) and return the argument to pass, or why none is passed.
fn write_claude_launch_settings(keitora_home: &Path, bin: &str) -> Result<String, String> {
    let file = keitora_home.join("hooks").join(CLAUDE_LAUNCH_SETTINGS);
    let arg = claude_settings_arg(&file)?;
    let mut text =
        serde_json::to_string_pretty(&launch_hooks_settings(bin)).map_err(|e| e.to_string())?;
    text.push('\n');
    if std::fs::read_to_string(&file).ok().as_deref() != Some(text.as_str()) {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("creating {} failed: {e}", dir.display()))?;
        }
        keitora_tasks::write_atomic(&file, text.as_bytes())
            .map_err(|e| format!("writing {} failed: {e}", file.display()))?;
    }
    Ok(arg)
}

/// Why a Claude launch does not pass the settings file while the project file holds
/// its hooks.
pub const PROJECT_FILE_HOLDS_THE_HOOKS: &str = "the project file holds the hooks";

/// Whether a Claude launch passes the per-launch settings file, given how the
/// project file install went. Only when that install failed: both files' hooks
/// run, and the guard that quiets the project copy ([`LAUNCH_HOOKS_ENV`]) lives
/// in the pane's shell, where it would also silence a Claude later started there
/// by hand. The file is shared by every worktree, so it runs the global `keitora`
/// CLI ([`resolve_global_keitora_bin`]), never one worktree's build.
fn claude_launch_settings(
    project_file: FileWrite,
    sources: &KeitoraBinSources,
    keitora_home: &Path,
) -> ClaudeSettings {
    if project_file != FileWrite::Failed {
        return ClaudeSettings::Skipped(PROJECT_FILE_HOLDS_THE_HOOKS.to_string());
    }
    let Some(bin) = resolve_global_keitora_bin(sources) else {
        return ClaudeSettings::Skipped("no global keitora CLI".to_string());
    };
    match write_claude_launch_settings(keitora_home, &bin) {
        Ok(arg) => ClaudeSettings::Applied(arg),
        Err(why) => {
            log::warn!("claude launch settings not used: {why}");
            ClaudeSettings::Skipped(why)
        }
    }
}

/// Write the hooks `cli` needs into worktree `wt`, with explicit inputs. Every write
/// is idempotent and happens only when the content differs.
///
/// | cli    | writes |
/// |--------|--------|
/// | Claude | `.claude/settings.local.json` |
/// | Codex  | `.claude/settings.local.json` (for a hand-run claude) and `.codex/hooks.json` |
/// | agy    | the global agy hook entry (when it changed) and workspace trust (no project files) |
///
/// A Claude worktree whose project file could not be written gets the per-launch
/// settings file under `keitora_home` instead (shared by every worktree); see
/// [`claude_launch_settings`].
pub fn prepare_worktree(
    wt: &Path,
    cli: keitora_tasks::CliKind,
    sources: &KeitoraBinSources,
    keitora_home: &Path,
) -> Prepared {
    use keitora_tasks::CliKind;
    let bin = resolve_keitora_bin(wt, sources);
    let mut out = Prepared {
        bin_resolved: bin.clone(),
        claude_file: FileWrite::NotApplicable,
        claude_settings: ClaudeSettings::NotApplicable,
        codex_file: FileWrite::NotApplicable,
    };
    // A spawn into a deleted worktree must not bring its folder back: every file
    // install below creates its parent directories.
    if !wt.is_dir() {
        log::warn!(
            "session hooks: worktree {} is missing; hooks not installed",
            wt.display()
        );
        match cli {
            CliKind::Antigravity => {}
            CliKind::Claude => {
                out.claude_file = FileWrite::Skipped;
                out.claude_settings = ClaudeSettings::Skipped("worktree is missing".to_string());
            }
            CliKind::Codex => {
                out.claude_file = FileWrite::Skipped;
                out.codex_file = FileWrite::Skipped;
            }
        }
        return out;
    }
    match cli {
        CliKind::Antigravity => crate::antigravity_hooks::sync_worktree(wt),
        CliKind::Claude | CliKind::Codex => {
            let codex = cli == CliKind::Codex;
            let Some(bin) = bin else {
                log::warn!(
                    "session hooks: no `keitora` binary found for {} (PATH, [cli].path, app bundle, or build); hooks not installed",
                    wt.display()
                );
                out.claude_file = FileWrite::Skipped;
                if codex {
                    out.codex_file = FileWrite::Skipped;
                } else {
                    out.claude_settings =
                        ClaudeSettings::Skipped("no keitora CLI resolved".to_string());
                }
                return out;
            };
            // The project file stays the hooks of a hand-started Claude.
            out.claude_file = install_hooks_for(wt, &bin);
            if codex {
                out.codex_file = crate::codex_hooks::install_codex_hooks_for(wt, &bin);
            } else {
                out.claude_settings =
                    claude_launch_settings(out.claude_file, sources, keitora_home);
            }
        }
    }
    out
}

/// What a launch adds to its pane: env vars, and args appended to the provider's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchPrep {
    pub env: Vec<(String, String)>,
    pub agent_args: Vec<String>,
    /// What [`prepare_worktree`] did (`None` under test, where nothing runs).
    pub prepared: Option<Prepared>,
}

impl LaunchPrep {
    /// The launch record for `cli` started with `args`.
    pub fn record(&self, cli: keitora_tasks::CliKind, args: &[String]) -> LaunchRecord {
        LaunchRecord {
            at_ms: keitora_tasks::now_ms(),
            cli,
            trust_bypass: args.iter().any(|a| a == TRUST_BYPASS_FLAG),
            env_names: self.env.iter().map(|(k, _)| k.clone()).collect(),
            claude_settings: self
                .prepared
                .as_ref()
                .map_or(ClaudeSettings::NotApplicable, |p| p.claude_settings.clone()),
            hook_version: HOOK_VERSION,
            start_refused: None,
        }
    }
}

/// Pure: the env a launch adds to its pane. The marker always (even with no
/// `keitora` resolved), `KEITORA_BIN` when one resolved, and [`LAUNCH_HOOKS_ENV`]
/// only when the launch passes the per-launch settings file.
pub fn launch_pane_env(bin: Option<&str>, launch_hooks: bool) -> Vec<(String, String)> {
    let mut env = vec![(PANE_MARKER_ENV.to_string(), "1".to_string())];
    env.extend(keitora_bin_pair(bin.map(str::to_string)));
    if launch_hooks {
        env.push((LAUNCH_HOOKS_ENV.to_string(), "1".to_string()));
    }
    env
}

/// Pure: what a launch adds for `prepared` (see [`launch_prep`]).
pub fn launch_prep_for(prepared: Prepared) -> LaunchPrep {
    let agent_args = match &prepared.claude_settings {
        ClaudeSettings::Applied(arg) => vec!["--settings".to_string(), arg.clone()],
        _ => Vec::new(),
    };
    LaunchPrep {
        env: launch_pane_env(prepared.bin_resolved.as_deref(), !agent_args.is_empty()),
        agent_args,
        prepared: Some(prepared),
    }
}

/// Prepare `cwd` for launching `cli` and return what the launch adds, with the
/// `[cli].path` override of `project` (`None`: the user's only). Run by both spawn
/// choke points, so every launch (boot-resume, recovery, reset, wake, switch,
/// transfer, a new worktree's first pane) finds its provider's hooks in place
/// before the agent starts. Empty under `cfg!(test)` (no login shell, no real
/// files).
pub fn launch_prep(
    project: Option<&keitora_tasks::ProjectConfig>,
    cwd: &Path,
    cli: keitora_tasks::CliKind,
) -> LaunchPrep {
    if cfg!(test) {
        return LaunchPrep::default();
    }
    let sources = KeitoraBinSources::gather(project, keitora_on_path_cached().clone());
    launch_prep_for(prepare_worktree(
        cwd,
        cli,
        &sources,
        &keitora_tasks::keitora_home(),
    ))
}

/// Write a just-created worktree's provider hooks, so an agent started in it by
/// hand (before Keitora spawns its first pane) already records its state. Not a
/// launch: nothing is added to a pane, and the first spawn's own [`launch_prep`]
/// finds the files in place and writes nothing. A no-op under `cfg!(test)`.
pub fn prepare_new_worktree(
    project: &keitora_tasks::ProjectConfig,
    wt: &Path,
    cli: keitora_tasks::CliKind,
) {
    let _ = launch_prep(Some(project), wt, cli);
}

/// [`launch_prep`] for the session `key`: its project is the registered one whose
/// resolved name the key carries, so a spawn embeds the same `keitora` as every
/// other install for that project. Empty under `cfg!(test)` (no user config read).
pub fn launch_prep_for_session(
    key: &crate::backend::SessionKey,
    cwd: &Path,
    cli: keitora_tasks::CliKind,
) -> LaunchPrep {
    if cfg!(test) {
        return LaunchPrep::default();
    }
    let project = keitora_tasks::UserConfig::load()
        .ok()
        .and_then(|user| user.project_config_by_name(&key.project));
    launch_prep(project.as_ref(), cwd, cli)
}

/// How the last launch of a session went, for the hook diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRecord {
    pub at_ms: i64,
    pub cli: keitora_tasks::CliKind,
    /// The args carried [`TRUST_BYPASS_FLAG`].
    pub trust_bypass: bool,
    /// The env names the launch set.
    pub env_names: Vec<String>,
    pub claude_settings: ClaudeSettings,
    pub hook_version: u32,
    /// Why herdr refused to start the agent when it did, e.g. `agent_not_ready`
    /// while Claude's folder-trust dialog is up in a new worktree.
    pub start_refused: Option<String>,
}

type LaunchRecords =
    std::sync::Mutex<std::collections::HashMap<crate::backend::SessionKey, LaunchRecord>>;

fn launch_records() -> &'static LaunchRecords {
    static RECORDS: std::sync::OnceLock<LaunchRecords> = std::sync::OnceLock::new();
    RECORDS.get_or_init(Default::default)
}

/// Remember `rec` as `key`'s last launch (in memory: a restart forgets it).
pub fn note_launch(key: &crate::backend::SessionKey, rec: LaunchRecord) {
    if let Ok(mut m) = launch_records().lock() {
        m.insert(key.clone(), rec);
    }
}

/// Record on `key`'s last launch that herdr refused to start its agent (`code`).
pub fn note_start_refused(key: &crate::backend::SessionKey, code: &str) {
    if let Ok(mut m) = launch_records().lock() {
        if let Some(rec) = m.get_mut(key) {
            rec.start_refused = Some(code.to_string());
        }
    }
}

/// `key`'s last launch in this process.
pub fn last_launch(key: &crate::backend::SessionKey) -> Option<LaunchRecord> {
    launch_records().lock().ok()?.get(key).cloned()
}

// ---------------------------------------------------------------------------
// Read-only hook facts: what is installed for a worktree and how its last launch
// went. Pure reads (never a write); the remedies are `prepare_worktree` and the agy
// syncs. Read by the health monitor.
// ---------------------------------------------------------------------------

/// Codex's side of a worktree's hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexFacts {
    /// `.codex/hooks.json` has our entry on every event we register.
    pub hooks_json_ok: bool,
    /// What Codex's own config says about the project (report only).
    pub project_trust: keitora_tasks::ProjectTrust,
}

/// Claude's side (Claude and Codex worktrees both get the project file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeFacts {
    /// `.claude/settings.local.json` has our entry on every event we register.
    pub hooks_installed: bool,
}

/// Antigravity's global hook entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyFacts {
    /// The global hooks file has the `keitora` key.
    pub key_present: bool,
    /// Every command of that key runs an absolute path to an existing file.
    pub path_ok: bool,
    /// The running app is translocated (or a cargo build): it offers no CLI.
    pub translocated: bool,
}

/// Everything [`hook_facts`] reports for one worktree. What the last launch did is
/// read from [`Self::last_launch`] through the methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookFacts {
    /// The `keitora` the worktree's hooks would run; `None`: no CLI resolvable.
    pub bin: Option<std::path::PathBuf>,
    /// `Some` for a Codex worktree.
    pub codex: Option<CodexFacts>,
    /// `Some` for Claude and Codex worktrees.
    pub claude: Option<ClaudeFacts>,
    /// `Some` for an Antigravity worktree.
    pub agy: Option<AgyFacts>,
    pub last_launch: Option<LaunchRecord>,
    /// The newest `ERROR` line of the hook log from inside this worktree, in the
    /// last 24 h.
    pub last_hook_error: Option<keitora_tasks::HookLogLine>,
}

impl HookFacts {
    /// The last launch used the current [`HOOK_VERSION`]; `true` with no launch.
    pub fn hook_version_ok(&self) -> bool {
        self.last_launch
            .as_ref()
            .is_none_or(|r| r.hook_version == HOOK_VERSION)
    }

    /// Whether the last Codex launch carried the hook-trust bypass; `None` without
    /// a Codex launch in this process (another provider's launch says nothing).
    pub fn trust_bypass_last_launch(&self) -> Option<bool> {
        self.last_launch
            .as_ref()
            .filter(|r| r.cli == keitora_tasks::CliKind::Codex)
            .map(|r| r.trust_bypass)
    }

    /// How the last launch passed Claude's hooks; `NotApplicable` without a launch.
    pub fn per_launch_settings(&self) -> ClaudeSettings {
        self.last_launch
            .as_ref()
            .map_or(ClaudeSettings::NotApplicable, |r| r.claude_settings.clone())
    }

    /// Whether the last launch set the pane marker; `None` without a launch.
    pub fn marker_env_ok(&self) -> Option<bool> {
        self.last_launch
            .as_ref()
            .map(|r| r.env_names.iter().any(|n| n == PANE_MARKER_ENV))
    }
}

/// Where [`hook_facts_in`] reads, injected so tests never touch a real home.
#[derive(Debug, Clone)]
pub struct FactsPaths {
    /// The hook log (`<keitora home>/logs/hooks.log`).
    pub hook_log: std::path::PathBuf,
    /// The global agy hooks file (`None`: no home).
    pub agy_hooks_file: Option<std::path::PathBuf>,
    /// Codex's `config.toml` (`None`: no Codex home).
    pub codex_config: Option<std::path::PathBuf>,
    /// The running app's executable.
    pub exe: Option<std::path::PathBuf>,
    /// Now, in unix seconds.
    pub now_s: i64,
}

/// Bytes of the hook log's tail [`hook_facts`] reads.
const HOOK_LOG_TAIL: u64 = 64 * 1024;
/// How far back [`HookFacts::last_hook_error`] looks.
const HOOK_ERROR_WINDOW_S: i64 = 24 * 60 * 60;

/// Whether `v["hooks"][event]` holds an entry with a command containing `marker`,
/// for every one of `events`.
pub(crate) fn every_event_has<'a>(
    v: &Value,
    mut events: impl Iterator<Item = &'a str>,
    marker: &str,
) -> bool {
    events.all(|event| {
        v["hooks"][event].as_array().is_some_and(|arr| {
            arr.iter().any(|e| {
                e["hooks"].as_array().is_some_and(|hs| {
                    hs.iter()
                        .any(|h| h["command"].as_str().is_some_and(|c| c.contains(marker)))
                })
            })
        })
    })
}

/// A JSON file's value; `None` when missing or unparsable.
fn read_json(file: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
}

/// Whether a `.claude/settings.local.json` value carries our hooks on every event.
fn claude_hooks_installed(v: &Value) -> bool {
    let events = HOOK_MAP
        .iter()
        .map(|(e, _, _)| *e)
        .chain(CLAUDE_GATE_MAP.iter().map(|(e, _)| *e));
    every_event_has(v, events, HOOK_MARKER)
}

/// The executable an agy hook command runs: its first quoted string that is not a
/// variable (`"$KEITORA_PANE"`, `"%KEITORA_BIN%"`).
fn agy_command_path(command: &str) -> Option<&str> {
    command
        .split('"')
        .skip(1)
        .step_by(2)
        .find(|s| !s.starts_with('$') && !s.starts_with('%'))
}

/// The agy facts from the global hooks file's value (`None`: missing/unreadable).
fn agy_facts(doc: Option<&Value>, translocated: bool) -> AgyFacts {
    let key = doc
        .and_then(|d| d.get(crate::antigravity_hooks::HOOK_NAME))
        .and_then(Value::as_object);
    let commands: Vec<&str> = key
        .into_iter()
        .flat_map(|k| k.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|h| h["command"].as_str())
        .collect();
    let path_ok = !commands.is_empty()
        && commands.iter().all(|c| {
            agy_command_path(c).is_some_and(|p| {
                let p = Path::new(p);
                p.is_absolute() && p.is_file()
            })
        });
    AgyFacts {
        key_present: key.is_some(),
        path_ok,
        translocated,
    }
}

/// The `ERROR` lines in the last [`HOOK_LOG_TAIL`] bytes of `log`, each with its
/// normalized `cwd` (empty when the log is missing or unreadable).
fn hook_log_errors(log: &Path) -> Vec<(String, keitora_tasks::HookLogLine)> {
    use keitora_tasks::path_key::{normalize_path, FOLD_CASE};
    use std::io::{Read, Seek, SeekFrom};
    let read = || -> Option<String> {
        let mut f = std::fs::File::open(log).ok()?;
        let len = f.metadata().ok()?.len();
        let cut = len > HOOK_LOG_TAIL;
        if cut {
            f.seek(SeekFrom::Start(len - HOOK_LOG_TAIL)).ok()?;
        }
        let mut buf = Vec::new();
        f.take(HOOK_LOG_TAIL).read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);
        // A cut tail starts mid-line: drop it.
        Some(match (cut, text.split_once('\n')) {
            (true, Some((_, rest))) => rest.to_string(),
            (true, None) => String::new(),
            (false, _) => text.into_owned(),
        })
    };
    let Some(text) = read() else {
        return Vec::new();
    };
    text.lines()
        .filter_map(keitora_tasks::hook_log::parse_line)
        .filter(|l| l.level == keitora_tasks::HookLogLevel::Error)
        .map(|l| (normalize_path(&l.cwd, true, FOLD_CASE), l))
        .collect()
}

/// The newest of `errors` written from inside `wt`, no older than
/// [`HOOK_ERROR_WINDOW_S`] before `now_s`.
fn last_hook_error_of(
    errors: &[(String, keitora_tasks::HookLogLine)],
    wt: &Path,
    now_s: i64,
) -> Option<keitora_tasks::HookLogLine> {
    use keitora_tasks::path_key::{is_within, normalize_path, FOLD_CASE};
    let want = normalize_path(&wt.to_string_lossy(), true, FOLD_CASE);
    errors
        .iter()
        .filter(|(_, l)| l.unix_s >= now_s - HOOK_ERROR_WINDOW_S)
        .filter(|(cwd, _)| is_within(cwd, &want))
        .max_by_key(|(_, l)| l.unix_s)
        .map(|(_, l)| l.clone())
}

/// Codex's `config.toml` text: `Ok(None)` when missing, `Err` on any other read error.
fn read_codex_config(config: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(config) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("reading {} failed: {e}", config.display())),
    }
}

/// One shared file's parsed content, re-read only when its path, size or mtime moves.
struct Stamped<T> {
    stamp: Option<(std::path::PathBuf, u64, std::time::SystemTime)>,
    value: Option<T>,
}

impl<T> Default for Stamped<T> {
    fn default() -> Self {
        Stamped {
            stamp: None,
            value: None,
        }
    }
}

/// `path`'s value from `slot`, read with `read` when the slot holds none for the
/// file as it is now; then `with` it. A file without an mtime is read every time.
fn stamped<T, R>(
    slot: &std::sync::Mutex<Stamped<T>>,
    path: &Path,
    read: impl FnOnce(&Path) -> T,
    with: impl FnOnce(&T) -> R,
) -> R {
    let now = std::fs::metadata(path)
        .ok()
        .and_then(|m| Some((path.to_path_buf(), m.len(), m.modified().ok()?)));
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    if now.is_none() || slot.stamp != now || slot.value.is_none() {
        slot.value = Some(read(path));
        slot.stamp = now;
    }
    with(slot.value.as_ref().expect("just filled"))
}

/// The files every worktree's hook facts share (the hook log, the agy hooks file and
/// Codex's config), read once and re-read when they change. One per monitor.
#[derive(Default)]
pub struct SharedFactsFiles {
    hook_log: std::sync::Mutex<Stamped<Vec<(String, keitora_tasks::HookLogLine)>>>,
    agy_hooks: std::sync::Mutex<Stamped<Option<Value>>>,
    codex_config: std::sync::Mutex<Stamped<Result<Option<String>, String>>>,
}

/// What Keitora installed for the worktree at `wt` and how its last launch went, with
/// every input explicit; the shared files are read through `files`.
pub fn hook_facts_in(
    wt: &Path,
    cli: keitora_tasks::CliKind,
    sources: &KeitoraBinSources,
    key: &crate::backend::SessionKey,
    paths: &FactsPaths,
    files: &SharedFactsFiles,
) -> HookFacts {
    use keitora_tasks::CliKind;
    let claude = matches!(cli, CliKind::Claude | CliKind::Codex).then(|| ClaudeFacts {
        hooks_installed: read_json(&wt.join(".claude").join("settings.local.json"))
            .is_some_and(|v| claude_hooks_installed(&v)),
    });
    let codex = (cli == CliKind::Codex).then(|| CodexFacts {
        hooks_json_ok: read_json(&wt.join(".codex").join("hooks.json"))
            .is_some_and(|v| crate::codex_hooks::hooks_installed(&v)),
        project_trust: match &paths.codex_config {
            Some(cfg) => stamped(
                &files.codex_config,
                cfg,
                read_codex_config,
                |text| match text {
                    Ok(text) => keitora_tasks::codex_project_trust(text.as_deref(), wt),
                    Err(e) => keitora_tasks::ProjectTrust::Unreadable(e.clone()),
                },
            ),
            None => keitora_tasks::ProjectTrust::Unreadable("no Codex home".into()),
        },
    });
    let agy = (cli == CliKind::Antigravity).then(|| {
        let translocated = paths
            .exe
            .as_deref()
            .is_some_and(exe_is_untrusted_for_bundled_cli);
        match paths.agy_hooks_file.as_deref() {
            Some(file) => stamped(&files.agy_hooks, file, read_json, |doc| {
                agy_facts(doc.as_ref(), translocated)
            }),
            None => agy_facts(None, translocated),
        }
    });
    HookFacts {
        bin: resolve_keitora_bin(wt, sources).map(std::path::PathBuf::from),
        codex,
        claude,
        agy,
        last_launch: last_launch(key),
        last_hook_error: stamped(
            &files.hook_log,
            &paths.hook_log,
            hook_log_errors,
            |errors| last_hook_error_of(errors, wt, paths.now_s),
        ),
    }
}

/// The real locations [`hook_facts_in`] reads: the Keitora home's hook log, the agy
/// hooks file, Codex's `config.toml` and the app's executable, as of now.
pub fn real_facts_paths() -> FactsPaths {
    let home = dirs::home_dir();
    FactsPaths {
        hook_log: keitora_tasks::hook_log::hook_log_path(),
        agy_hooks_file: home.as_deref().map(|h| {
            crate::antigravity_hooks::hooks_file_in(
                h,
                std::env::var_os(crate::antigravity_hooks::CONFIG_DIR_ENV),
            )
        }),
        codex_config: crate::codex_env::codex_home_dir(
            std::env::var("CODEX_HOME").ok().as_deref(),
            home.as_deref(),
        )
        .map(|(dir, _)| dir.join("config.toml")),
        exe: std::env::current_exe().ok(),
        now_s: keitora_tasks::now_ms() / 1000,
    }
}

/// The `keitora` CLI shipped next to the running app executable (`keitora-cli.exe`
/// beside an installed `keitora-app.exe`), looked up once per process. `None` under
/// `cfg!(test)`, so tests never pick up the real app's sibling, and `None` for an app
/// that isn't an installed one (see [`exe_is_untrusted_for_bundled_cli`]): a dev app's
/// `target/debug` CLI would otherwise outrank worktree builds and get written into the
/// real active project's hooks.
pub fn app_bundled_cli() -> Option<&'static str> {
    if cfg!(test) {
        return None;
    }
    static SIDECAR: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    SIDECAR
        .get_or_init(|| {
            let exe = std::env::current_exe().ok()?;
            if cfg!(debug_assertions) || exe_is_untrusted_for_bundled_cli(&exe) {
                return None;
            }
            keitora_tasks::bundled_cli(&exe).map(|p| p.to_string_lossy().into_owned())
        })
        .as_deref()
}

/// Pure: whether the app at `exe` must not offer its sibling CLI for hooks or install —
/// a cargo build (`…/target/{debug,release}/…`, or `…/target/<triple>/{debug,release}/…`)
/// or a macOS translocated copy (`…/AppTranslocation/…`, a randomized read-only path
/// that vanishes when the app quits).
pub(crate) fn exe_is_untrusted_for_bundled_cli(exe: &Path) -> bool {
    let parts: Vec<String> = exe
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    let is_profile = |s: &str| s == "debug" || s == "release";
    parts.iter().any(|p| p == "apptranslocation")
        || parts.iter().enumerate().any(|(i, p)| {
            p == "target"
                && (parts.get(i + 1).is_some_and(|n| is_profile(n))
                    || parts.get(i + 2).is_some_and(|n| is_profile(n)))
        })
}

/// Test support: write `body` to `path` as a runnable file (mode 755 on unix).
#[cfg(test)]
pub(crate) fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The `keitora` on PATH, looked up once per process.
pub(crate) fn keitora_on_path_cached() -> &'static Option<String> {
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
pub fn install_hooks_for(wt_path: &Path, task_bin: &str) -> FileWrite {
    let file = wt_path.join(".claude").join("settings.local.json");
    merge_json_file(&file, "session hooks", |v| {
        merged_hooks_settings(v, task_bin)
    })
}

/// Read the JSON object at `file` (absent: `{}`), apply `merge`, and write the result
/// back pretty-printed only when it differs. A file that is not valid JSON is left
/// alone. Failures are logged under `what`.
pub(crate) fn merge_json_file(
    file: &Path,
    what: &str,
    merge: impl FnOnce(Value) -> Value,
) -> FileWrite {
    let existing: Value = match std::fs::read_to_string(file) {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("{what}: skip {} (parse error: {e})", file.display());
                return FileWrite::Failed;
            }
        },
        Err(_) => json!({}), // file absent → start fresh
    };
    let merged = merge(existing.clone());
    if merged == existing {
        return FileWrite::Unchanged; // already installed → no write (keep git status clean)
    }
    if let Some(dir) = file.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            log::warn!("{what}: mkdir {} failed: {e}", dir.display());
            return FileWrite::Failed;
        }
    }
    let pretty = serde_json::to_string_pretty(&merged).unwrap_or_default();
    match std::fs::write(file, format!("{pretty}\n")) {
        Ok(()) => FileWrite::Written,
        Err(e) => {
            log::warn!("{what}: write {} failed: {e}", file.display());
            FileWrite::Failed
        }
    }
}

/// Startup install: refresh the hooks for every existing worktree of the project the
/// cwd resolves to AND of the registry's active project (once if they are the same).
///
/// The cwd alone is not enough: an installed app starts in a directory that is no
/// project, so hooks only ever got (re)installed on project activation and an upgrade
/// left every worktree's hooks stale. A dev run (`yarn tauri dev` from the repo)
/// still resolves its repo through the cwd. The `keitora` binary is resolved per
/// worktree (see [`resolve_keitora_bin`]); worktrees with none are skipped.
pub fn install_session_hooks() {
    let cwd = Config::find().ok();
    let active = Config::find_active().ok();
    for config in startup_hook_targets(cwd, active, Config::main_worktree_path) {
        install_hooks_into(config);
    }
}

/// The projects to install hooks into at startup: the cwd's project, then the
/// active project unless it is the same one (by `key`). Pure.
fn startup_hook_targets<T, K: PartialEq>(
    cwd: Option<T>,
    active: Option<T>,
    key: impl Fn(&T) -> K,
) -> Vec<T> {
    let mut out: Vec<T> = cwd.into_iter().collect();
    if let Some(a) = active {
        if !out.iter().any(|c| key(c) == key(&a)) {
            out.push(a);
        }
    }
    out
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
/// The PATH scan (`keitora_on_path`) runs a login shell, so the sources are gathered
/// once, outside the per-worktree loop.
fn install_hooks_into(config: Config) {
    let sources = KeitoraBinSources::gather(
        Some(config.project_config()),
        keitora_tasks::keitora_on_path(),
    );

    let worktrees = match config.resolved_worktrees() {
        Ok(w) => w,
        Err(_) => return,
    };

    let home = keitora_tasks::keitora_home();
    for wt in worktrees {
        if wt.path.is_dir() {
            // Codex worktrees also get `.codex/hooks.json`; only they do, so a
            // hand-run `codex` in a Claude worktree is never asked to trust our hooks.
            prepare_worktree(&wt.path, wt.cli, &sources, &home);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_and_translocated_apps_offer_no_bundled_cli() {
        let untrusted = [
            "D:/repos/proj/target/debug/keitora-app.exe",
            "D:/repos/proj/target/release/keitora-app.exe",
            "/Users/me/src/keitora/target/debug/keitora-app",
            "/Users/me/src/keitora/target/aarch64-apple-darwin/release/keitora-app",
            "D:/repos/proj/Target/Debug/keitora-app.exe",
            "/private/var/folders/x/AppTranslocation/ABC/d/Keitora.app/Contents/MacOS/keitora-app",
        ];
        for p in untrusted {
            assert!(exe_is_untrusted_for_bundled_cli(Path::new(p)), "{p}");
        }
        let installed = [
            "C:/Users/x/AppData/Local/Keitora/keitora-app.exe",
            "C:/Program Files/Keitora/keitora-app.exe",
            "/Applications/Keitora.app/Contents/MacOS/keitora-app",
            // a "target" dir that isn't a cargo profile dir
            "/opt/target/bin/keitora-app",
        ];
        for p in installed {
            assert!(!exe_is_untrusted_for_bundled_cli(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn keitora_bin_pair_sets_the_variable() {
        assert_eq!(
            keitora_bin_pair(Some("/x/keitora".into())),
            vec![("KEITORA_BIN".to_string(), "/x/keitora".to_string())]
        );
        assert!(keitora_bin_pair(None).is_empty());
    }

    #[test]
    fn resolve_global_keitora_bin_never_uses_a_worktree_build() {
        let tmp = tempfile::tempdir().unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let path_bin = s(&mk_runnable(tmp.path(), "path-keitora"));
        let override_bin = s(&mk_runnable(tmp.path(), "override-keitora"));
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let app_cli = s(&mk_runnable(&app_dir, "keitora-cli"));
        let all = KeitoraBinSources {
            override_path: Some(override_bin.clone()),
            app_cli: Some(app_cli.clone()),
            on_path: Some(path_bin.clone()),
        };
        assert_eq!(resolve_global_keitora_bin(&all), Some(path_bin));
        let no_path = KeitoraBinSources {
            on_path: None,
            ..all.clone()
        };
        assert_eq!(resolve_global_keitora_bin(&no_path), Some(override_bin));
        let app_only = KeitoraBinSources {
            app_cli: Some(app_cli.clone()),
            ..Default::default()
        };
        assert_eq!(resolve_global_keitora_bin(&app_only), Some(app_cli));
        // A worktree build that `resolve_keitora_bin` would take is never used.
        let wt = tmp.path().join("wt");
        let dbg = wt.join("target").join("debug");
        std::fs::create_dir_all(&dbg).unwrap();
        mk_runnable(&dbg, "keitora");
        mk_runnable(&dbg, "keitora-app");
        assert!(resolve_keitora_bin(&wt, &KeitoraBinSources::default()).is_some());
        assert_eq!(
            resolve_global_keitora_bin(&KeitoraBinSources::default()),
            None
        );
        // Missing, relative or not runnable paths do not count.
        let bad = KeitoraBinSources {
            override_path: Some("keitora".into()),
            app_cli: Some(s(&tmp.path().join("gone"))),
            on_path: None,
        };
        assert_eq!(resolve_global_keitora_bin(&bad), None);
    }

    #[test]
    fn launch_prep_is_empty_under_test() {
        let key = crate::backend::SessionKey {
            project: "proj".into(),
            worktree: "main".into(),
        };
        for cli in keitora_tasks::CliKind::ALL {
            let cwd = Path::new("/home/u/proj");
            assert_eq!(launch_prep(None, cwd, cli), LaunchPrep::default());
            assert_eq!(
                launch_prep_for_session(&key, cwd, cli),
                LaunchPrep::default()
            );
        }
    }

    #[test]
    fn launch_pane_env_adds_the_guard_only_with_a_settings_file() {
        assert_eq!(
            launch_pane_env(None, true),
            vec![
                ("KEITORA_PANE".to_string(), "1".to_string()),
                ("KEITORA_LAUNCH_HOOKS".to_string(), "1".to_string())
            ]
        );
    }

    #[test]
    fn launch_pane_env_always_carries_the_marker() {
        assert_eq!(
            launch_pane_env(None, false),
            vec![("KEITORA_PANE".to_string(), "1".to_string())]
        );
        assert_eq!(
            launch_pane_env(Some("/x/keitora"), false),
            vec![
                ("KEITORA_PANE".to_string(), "1".to_string()),
                ("KEITORA_BIN".to_string(), "/x/keitora".to_string())
            ]
        );
    }

    /// Sources that resolve `bin` (as the override) and nothing else.
    fn sources_with(bin: Option<&Path>) -> KeitoraBinSources {
        KeitoraBinSources {
            override_path: bin.map(|b| b.to_string_lossy().into_owned()),
            ..Default::default()
        }
    }

    fn temp_bin(dir: &Path) -> std::path::PathBuf {
        mk_runnable(dir, "keitora-bin")
    }

    #[test]
    fn prepare_worktree_codex_writes_codex_hooks_json() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("bravo");
        std::fs::create_dir_all(&wt).unwrap();
        let bin = temp_bin(tmp.path());
        let p = prepare_worktree(
            &wt,
            keitora_tasks::CliKind::Codex,
            &sources_with(Some(&bin)),
            tmp.path(),
        );
        assert_eq!(
            p.bin_resolved.as_deref(),
            Some(bin.to_string_lossy().as_ref())
        );
        assert_eq!(
            (p.claude_file, p.codex_file),
            (FileWrite::Written, FileWrite::Written)
        );
        let codex = std::fs::read_to_string(wt.join(".codex/hooks.json")).unwrap();
        assert!(codex.contains("session gate --provider codex"), "{codex}");
        assert!(
            wt.join(".claude/settings.local.json").is_file(),
            "hand-run claude"
        );
    }

    #[test]
    fn prepare_worktree_claude_relies_on_the_project_file_when_it_is_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("alpha");
        std::fs::create_dir_all(&wt).unwrap();
        let home = tmp.path().join("keitora-home");
        let bin = temp_bin(tmp.path());
        let p = prepare_worktree(
            &wt,
            keitora_tasks::CliKind::Claude,
            &sources_with(Some(&bin)),
            &home,
        );
        assert_eq!(p.claude_file, FileWrite::Written);
        assert_eq!(p.codex_file, FileWrite::NotApplicable);
        assert!(
            !wt.join(".codex").exists(),
            "never asks claude worktrees to trust codex hooks"
        );
        let project = std::fs::read_to_string(wt.join(".claude/settings.local.json")).unwrap();
        assert!(
            project.contains("KEITORA_LAUNCH_HOOKS"),
            "guarded project copy"
        );
        assert_eq!(
            p.claude_settings,
            ClaudeSettings::Skipped(PROJECT_FILE_HOLDS_THE_HOOKS.into())
        );
        assert!(!home.exists(), "no per-launch settings file");
        // The pane shell carries no guard: a Claude started there by hand after the
        // launched one exits still runs the project file's hooks.
        let prep = launch_prep_for(p);
        assert!(prep.agent_args.is_empty(), "no --settings");
        assert!(
            !prep.env.iter().any(|(k, _)| k == LAUNCH_HOOKS_ENV),
            "{:?}",
            prep.env
        );
        let again = prepare_worktree(
            &wt,
            keitora_tasks::CliKind::Claude,
            &sources_with(Some(&bin)),
            &home,
        );
        assert_eq!(again.claude_file, FileWrite::Unchanged);
        assert_eq!(
            again.claude_settings,
            ClaudeSettings::Skipped(PROJECT_FILE_HOLDS_THE_HOOKS.into())
        );
    }

    #[test]
    fn prepare_worktree_claude_passes_launch_settings_when_the_project_file_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("alpha");
        std::fs::create_dir_all(wt.join(".claude")).unwrap();
        // A project file that is not JSON is left alone: the install fails.
        std::fs::write(wt.join(".claude/settings.local.json"), "{ not json").unwrap();
        // A Keitora home whose path needs no quoting.
        let home = tmp.path().join("keitora-home");
        let bin = temp_bin(tmp.path());
        let p = prepare_worktree(
            &wt,
            keitora_tasks::CliKind::Claude,
            &sources_with(Some(&bin)),
            &home,
        );
        assert_eq!(p.claude_file, FileWrite::Failed);
        let file = home.join("hooks").join(CLAUDE_LAUNCH_SETTINGS);
        let Ok(arg) = claude_settings_arg(&file) else {
            // A temp dir whose path needs quoting (e.g. a space in the user name).
            assert_eq!(
                p.claude_settings,
                ClaudeSettings::Skipped("path needs quoting".into())
            );
            return;
        };
        assert_eq!(p.claude_settings, ClaudeSettings::Applied(arg.clone()));
        let launch: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(launch, launch_hooks_settings(&bin.to_string_lossy()));
        let prep = launch_prep_for(p);
        assert_eq!(prep.agent_args, vec!["--settings".to_string(), arg]);
        assert!(prep
            .env
            .contains(&(LAUNCH_HOOKS_ENV.to_string(), "1".to_string())));
        // Idempotent: the settings file is not rewritten.
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        prepare_worktree(
            &wt,
            keitora_tasks::CliKind::Claude,
            &sources_with(Some(&bin)),
            &home,
        );
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), mtime);
    }

    #[test]
    fn claude_launch_settings_never_embed_a_worktree_build() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("alpha");
        // Only the worktree's own build resolves: fine for the worktree's project
        // file, never for the file every worktree shares.
        let dbg = wt.join("target").join("debug");
        std::fs::create_dir_all(&dbg).unwrap();
        mk_runnable(&dbg, "keitora");
        mk_runnable(&dbg, "keitora-app");
        let sources = KeitoraBinSources::default();
        assert!(resolve_keitora_bin(&wt, &sources).is_some());
        let home = tmp.path().join("keitora-home");
        assert_eq!(
            claude_launch_settings(FileWrite::Failed, &sources, &home),
            ClaudeSettings::Skipped("no global keitora CLI".into())
        );
        assert!(!home.exists());
    }

    /// A temp world for [`hook_facts_in`]: a worktree, a Keitora home, an agy hooks
    /// file, a Codex config and a runnable `keitora`.
    struct FactsRig {
        tmp: tempfile::TempDir,
        wt: std::path::PathBuf,
        bin: std::path::PathBuf,
        paths: FactsPaths,
    }

    const NOW_S: i64 = 1_800_000_000;

    fn facts_rig(name: &str) -> FactsRig {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt").join(name);
        std::fs::create_dir_all(&wt).unwrap();
        let bin = temp_bin(tmp.path());
        let paths = FactsPaths {
            hook_log: keitora_tasks::hook_log::hook_log_in(
                &tmp.path().join("keitora-home").join("logs"),
            ),
            agy_hooks_file: Some(tmp.path().join("gemini").join("hooks.json")),
            codex_config: Some(tmp.path().join("codex").join("config.toml")),
            exe: Some(tmp.path().join("Keitora").join("keitora-app.exe")),
            now_s: NOW_S,
        };
        FactsRig {
            tmp,
            wt,
            bin,
            paths,
        }
    }

    impl FactsRig {
        fn key(&self) -> crate::backend::SessionKey {
            crate::backend::SessionKey {
                project: "facts-proj".into(),
                worktree: self.wt.file_name().unwrap().to_string_lossy().into_owned(),
            }
        }
        fn facts(&self, cli: keitora_tasks::CliKind) -> HookFacts {
            hook_facts_in(
                &self.wt,
                cli,
                &sources_with(Some(&self.bin)),
                &self.key(),
                &self.paths,
                &SharedFactsFiles::default(),
            )
        }
        fn prepare(&self, cli: keitora_tasks::CliKind) {
            prepare_worktree(
                &self.wt,
                cli,
                &sources_with(Some(&self.bin)),
                &self.tmp.path().join("keitora-home"),
            );
        }
        fn write_agy(&self, bin: Option<&str>) {
            let file = self.paths.agy_hooks_file.as_ref().unwrap();
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            assert!(
                crate::antigravity_hooks::apply_hooks_at(file, cfg!(windows), bin, true).unwrap()
            );
        }
        fn log(&self, lines: &[String]) {
            let log = &self.paths.hook_log;
            std::fs::create_dir_all(log.parent().unwrap()).unwrap();
            std::fs::write(log, lines.join("\n") + "\n").unwrap();
        }
    }

    #[test]
    fn hook_facts_reports_each_missing_piece() {
        use keitora_tasks::CliKind;
        // Codex worktree with nothing installed.
        let r = facts_rig("facts-codex");
        let f = r.facts(CliKind::Codex);
        assert_eq!(f.bin.as_deref(), Some(r.bin.as_path()));
        let codex = f.codex.clone().unwrap();
        assert!(!codex.hooks_json_ok, "codex file missing");
        assert_eq!(f.trust_bypass_last_launch(), None, "no launch yet");
        assert!(
            !f.claude.as_ref().unwrap().hooks_installed,
            "claude file missing"
        );
        assert!(f.agy.is_none());
        assert!(f.hook_version_ok(), "true with no launch");
        assert_eq!(f.last_hook_error, None);
        // Installed: both read clean.
        r.prepare(CliKind::Codex);
        let f = r.facts(CliKind::Codex);
        assert!(f.codex.unwrap().hooks_json_ok);
        assert!(f.claude.unwrap().hooks_installed);
        // A codex file missing one of our events is not ok.
        let file = r.wt.join(".codex").join("hooks.json");
        let mut v = read_json(&file).unwrap();
        v["hooks"].as_object_mut().unwrap().remove("Stop");
        std::fs::write(&file, v.to_string()).unwrap();
        assert!(!r.facts(CliKind::Codex).codex.unwrap().hooks_json_ok);

        // A Claude worktree has no codex or agy facts.
        let c = facts_rig("facts-claude");
        let f = c.facts(CliKind::Claude);
        assert!(f.codex.is_none() && f.agy.is_none());
        assert_eq!(
            f.per_launch_settings(),
            ClaudeSettings::NotApplicable,
            "no launch record"
        );

        // Antigravity: key missing, then present with a relative/variable path, then
        // an absolute one.
        let a = facts_rig("facts-agy");
        let f = a.facts(CliKind::Antigravity);
        assert_eq!(f.marker_env_ok(), None);
        let agy = f.agy.unwrap();
        assert!(!agy.key_present && !agy.path_ok);
        assert!(!agy.translocated);
        a.write_agy(None);
        let agy = a.facts(CliKind::Antigravity).agy.unwrap();
        assert!(agy.key_present);
        assert!(!agy.path_ok, "the $KEITORA_BIN form embeds no path");
        a.write_agy(Some("keitora"));
        assert!(
            !a.facts(CliKind::Antigravity).agy.unwrap().path_ok,
            "relative"
        );
        a.write_agy(Some(&a.tmp.path().join("gone").to_string_lossy()));
        assert!(
            !a.facts(CliKind::Antigravity).agy.unwrap().path_ok,
            "missing file"
        );
        a.write_agy(Some(&a.bin.to_string_lossy()));
        assert!(a.facts(CliKind::Antigravity).agy.unwrap().path_ok);
        // The marker comes from the last launch.
        let rec = LaunchPrep {
            env: launch_pane_env(None, false),
            ..Default::default()
        }
        .record(CliKind::Antigravity, &[]);
        note_launch(&a.key(), rec.clone());
        assert_eq!(a.facts(CliKind::Antigravity).marker_env_ok(), Some(true));
        note_launch(
            &a.key(),
            LaunchRecord {
                env_names: vec![],
                ..rec
            },
        );
        assert_eq!(a.facts(CliKind::Antigravity).marker_env_ok(), Some(false));
    }

    #[test]
    fn hook_facts_detects_a_translocated_app() {
        let mut r = facts_rig("facts-translocated");
        r.paths.exe = Some(
            "/private/var/folders/x/AppTranslocation/ABC/d/Keitora.app/Contents/MacOS/keitora-app"
                .into(),
        );
        let agy = r.facts(keitora_tasks::CliKind::Antigravity).agy.unwrap();
        assert!(agy.translocated);
        r.paths.exe = Some("/Applications/Keitora.app/Contents/MacOS/keitora-app".into());
        assert!(
            !r.facts(keitora_tasks::CliKind::Antigravity)
                .agy
                .unwrap()
                .translocated
        );
    }

    #[test]
    fn hook_facts_reads_the_last_hook_error_for_this_worktree_only() {
        use keitora_tasks::hook_log::format_line;
        use keitora_tasks::HookLogLevel::{Error, Warn};
        let r = facts_rig("facts-log");
        let here = r.wt.to_string_lossy().into_owned();
        let sub = r.wt.join("src").to_string_lossy().into_owned();
        let sibling = format!("{here}2");
        let other = r
            .tmp
            .path()
            .join("wt")
            .join("other")
            .to_string_lossy()
            .into_owned();
        r.log(&[
            "garbage line".to_string(),
            format_line(NOW_S - 100, Error, 1, "session busy", &here, "older error"),
            format_line(
                NOW_S - 50,
                Error,
                2,
                "session idle --stdin",
                &sub,
                "newest here",
            ),
            format_line(NOW_S - 10, Warn, 3, "session idle", &here, "a warning"),
            format_line(
                NOW_S - 5,
                Error,
                4,
                "session busy",
                &other,
                "another worktree",
            ),
            format_line(
                NOW_S - 4,
                Error,
                5,
                "session busy",
                &sibling,
                "a sibling prefix",
            ),
            format_line(NOW_S - 90_000, Error, 6, "session busy", &here, "too old"),
        ]);
        let e = r
            .facts(keitora_tasks::CliKind::Claude)
            .last_hook_error
            .unwrap();
        assert_eq!(e.message, "newest here");
        assert_eq!(e.cmd, "session idle --stdin");
        assert_eq!(e.unix_s, NOW_S - 50);
        // Only lines older than a day: none.
        r.log(&[format_line(
            NOW_S - 90_000,
            Error,
            6,
            "session busy",
            &here,
            "too old",
        )]);
        assert_eq!(
            r.facts(keitora_tasks::CliKind::Claude).last_hook_error,
            None
        );
    }

    #[test]
    fn shared_files_are_reread_only_when_they_change() {
        use keitora_tasks::hook_log::format_line;
        use keitora_tasks::HookLogLevel::Error;
        let r = facts_rig("facts-shared");
        let here = r.wt.to_string_lossy().into_owned();
        let files = SharedFactsFiles::default();
        let facts = || {
            hook_facts_in(
                &r.wt,
                keitora_tasks::CliKind::Claude,
                &sources_with(Some(&r.bin)),
                &r.key(),
                &r.paths,
                &files,
            )
            .last_hook_error
            .map(|e| e.message)
        };
        let line = |msg: &str| format_line(NOW_S - 5, Error, 1, "session busy", &here, msg);
        r.log(&[line("first")]);
        let log = &r.paths.hook_log;
        let stamp = std::fs::metadata(log).unwrap().modified().unwrap();
        assert_eq!(facts().as_deref(), Some("first"));
        // Same size and mtime: the cached read stands.
        r.log(&[line("again")]);
        std::fs::File::options()
            .write(true)
            .open(log)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        assert_eq!(facts().as_deref(), Some("first"));
        // A new mtime: re-read.
        std::fs::File::options()
            .write(true)
            .open(log)
            .unwrap()
            .set_modified(stamp + std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(facts().as_deref(), Some("again"));
    }

    #[test]
    fn hook_version_mismatch_is_reported() {
        let r = facts_rig("facts-version");
        let rec = LaunchPrep::default().record(keitora_tasks::CliKind::Claude, &[]);
        note_launch(&r.key(), rec.clone());
        let f = r.facts(keitora_tasks::CliKind::Claude);
        assert!(f.hook_version_ok());
        assert_eq!(f.last_launch, Some(rec.clone()));
        note_launch(
            &r.key(),
            LaunchRecord {
                hook_version: HOOK_VERSION - 1,
                ..rec
            },
        );
        assert!(!r.facts(keitora_tasks::CliKind::Claude).hook_version_ok());
    }

    #[test]
    fn hook_facts_reports_codex_project_trust() {
        use keitora_tasks::ProjectTrust;
        let r = facts_rig("facts-trust");
        let facts = |r: &FactsRig| r.facts(keitora_tasks::CliKind::Codex);
        let trust = |r: &FactsRig| facts(r).codex.unwrap();
        assert_eq!(
            trust(&r).project_trust,
            ProjectTrust::NotListed,
            "no config.toml"
        );
        let cfg = r.paths.codex_config.clone().unwrap();
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        // Codex trusts a root above the worktree; TOML literal strings keep `\`.
        let root = r.tmp.path().join("wt").to_string_lossy().into_owned();
        std::fs::write(
            &cfg,
            format!("[projects.'{root}']\ntrust_level = \"trusted\"\n"),
        )
        .unwrap();
        assert_eq!(trust(&r).project_trust, ProjectTrust::Trusted);
        std::fs::write(
            &cfg,
            format!("[projects.'{root}']\ntrust_level = \"untrusted\"\n"),
        )
        .unwrap();
        assert_eq!(
            trust(&r).project_trust,
            ProjectTrust::Untrusted("untrusted".into())
        );
        std::fs::write(&cfg, "not = [toml").unwrap();
        assert!(matches!(
            trust(&r).project_trust,
            ProjectTrust::Unreadable(_)
        ));
        // The last Codex launch's bypass is reported; another provider's is not.
        let prep = LaunchPrep::default();
        note_launch(&r.key(), prep.record(keitora_tasks::CliKind::Claude, &[]));
        assert_eq!(facts(&r).trust_bypass_last_launch(), None);
        let args = vec![TRUST_BYPASS_FLAG.to_string()];
        note_launch(&r.key(), prep.record(keitora_tasks::CliKind::Codex, &args));
        assert_eq!(facts(&r).trust_bypass_last_launch(), Some(true));
        note_launch(&r.key(), prep.record(keitora_tasks::CliKind::Codex, &[]));
        assert_eq!(facts(&r).trust_bypass_last_launch(), Some(false));
    }

    #[test]
    fn agy_command_path_skips_variables() {
        assert_eq!(
            agy_command_path(
                "(if not defined KEITORA_PANE if not defined KEITORA_BIN exit /b 0) & (\"C:\\x\\k.exe\" session idle >nul 2>&1) & exit /b 0"
            ),
            Some("C:\\x\\k.exe")
        );
        assert_eq!(
            agy_command_path(
                "[ -n \"$KEITORA_PANE$KEITORA_BIN\" ] && [ -x \"/x/k\" ] && \"/x/k\" session idle"
            ),
            Some("/x/k")
        );
        assert_eq!(
            agy_command_path("(if defined KEITORA_BIN \"%KEITORA_BIN%\" session idle) & exit /b 0"),
            None
        );
    }

    #[test]
    fn claude_launch_prep_falls_back_to_the_project_file_when_skipped() {
        let prepared = Prepared {
            bin_resolved: Some("/x/keitora".into()),
            claude_file: FileWrite::Unchanged,
            claude_settings: ClaudeSettings::Skipped("path needs quoting".into()),
            codex_file: FileWrite::NotApplicable,
        };
        let prep = launch_prep_for(prepared);
        assert!(prep.agent_args.is_empty(), "no --settings");
        assert!(
            !prep.env.iter().any(|(k, _)| k == LAUNCH_HOOKS_ENV),
            "no guard env"
        );
        assert_eq!(
            prep.record(keitora_tasks::CliKind::Claude, &[])
                .claude_settings,
            ClaudeSettings::Skipped("path needs quoting".into())
        );
    }

    #[test]
    fn prepare_worktree_agy_writes_no_project_files() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("charlie");
        std::fs::create_dir_all(&wt).unwrap();
        let bin = temp_bin(tmp.path());
        let p = prepare_worktree(
            &wt,
            keitora_tasks::CliKind::Antigravity,
            &sources_with(Some(&bin)),
            tmp.path(),
        );
        assert_eq!(
            (p.claude_file, p.codex_file),
            (FileWrite::NotApplicable, FileWrite::NotApplicable)
        );
        assert_eq!(
            std::fs::read_dir(&wt).unwrap().count(),
            0,
            "nothing in the worktree"
        );
        // The global writer is inert in the test binary.
        assert!(!crate::global_writes::allowed());
    }

    #[test]
    fn prepare_worktree_is_idempotent_and_does_not_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("bravo");
        std::fs::create_dir_all(&wt).unwrap();
        let bin = temp_bin(tmp.path());
        let sources = sources_with(Some(&bin));
        prepare_worktree(&wt, keitora_tasks::CliKind::Codex, &sources, tmp.path());
        let files = [
            wt.join(".claude/settings.local.json"),
            wt.join(".codex/hooks.json"),
        ];
        let before: Vec<_> = files
            .iter()
            .map(|f| {
                (
                    std::fs::read(f).unwrap(),
                    std::fs::metadata(f).unwrap().modified().unwrap(),
                )
            })
            .collect();
        let p = prepare_worktree(&wt, keitora_tasks::CliKind::Codex, &sources, tmp.path());
        assert_eq!(
            (p.claude_file, p.codex_file),
            (FileWrite::Unchanged, FileWrite::Unchanged)
        );
        let after: Vec<_> = files
            .iter()
            .map(|f| {
                (
                    std::fs::read(f).unwrap(),
                    std::fs::metadata(f).unwrap().modified().unwrap(),
                )
            })
            .collect();
        assert_eq!(before, after, "no rewrite");
    }

    #[test]
    fn prepare_worktree_never_recreates_a_deleted_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = temp_bin(tmp.path());
        let sources = sources_with(Some(&bin));
        for cli in [
            keitora_tasks::CliKind::Claude,
            keitora_tasks::CliKind::Codex,
            keitora_tasks::CliKind::Antigravity,
        ] {
            let wt = tmp.path().join("gone");
            let p = prepare_worktree(&wt, cli, &sources, tmp.path());
            assert!(!wt.exists(), "{cli:?} recreated the worktree folder");
            assert!(
                !matches!(p.claude_file, FileWrite::Written | FileWrite::Unchanged),
                "{cli:?}: {p:?}"
            );
            assert!(
                !matches!(p.codex_file, FileWrite::Written | FileWrite::Unchanged),
                "{cli:?}: {p:?}"
            );
            assert!(
                !matches!(p.claude_settings, ClaudeSettings::Applied(_)),
                "{cli:?}: {p:?}"
            );
        }
    }

    #[test]
    fn prepare_worktree_without_a_bin_skips_and_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("bravo");
        std::fs::create_dir_all(&wt).unwrap();
        let p = prepare_worktree(
            &wt,
            keitora_tasks::CliKind::Codex,
            &sources_with(None),
            tmp.path(),
        );
        assert_eq!(p.bin_resolved, None);
        assert_eq!(
            (p.claude_file, p.codex_file),
            (FileWrite::Skipped, FileWrite::Skipped)
        );
        assert_eq!(std::fs::read_dir(&wt).unwrap().count(), 0);
    }

    #[test]
    fn note_launch_records_the_trust_bypass() {
        let prep = LaunchPrep {
            env: launch_pane_env(Some("/x/keitora"), false),
            ..Default::default()
        };
        let key = crate::backend::SessionKey {
            project: "proj".into(),
            worktree: "launch-record".into(),
        };
        assert_eq!(last_launch(&key), None);
        let args = vec!["-c".to_string(), TRUST_BYPASS_FLAG.to_string()];
        note_launch(&key, prep.record(keitora_tasks::CliKind::Codex, &args));
        let rec = last_launch(&key).unwrap();
        assert!(rec.trust_bypass);
        assert_eq!(rec.env_names, ["KEITORA_PANE", "KEITORA_BIN"]);
        assert_eq!(rec.hook_version, HOOK_VERSION);
        note_launch(&key, prep.record(keitora_tasks::CliKind::Codex, &[]));
        assert!(
            !last_launch(&key).unwrap().trust_bypass,
            "the latest launch wins"
        );
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
        for (event, _m, cmd) in HOOK_MAP {
            assert_eq!(
                commands_for(&out, event),
                vec![hook_command("/abs/keitora", cmd, false)],
                "event {event}"
            );
        }
        // The installed command (POSIX on every platform — Claude Code runs hooks via
        // a POSIX shell, incl. Git Bash on Windows) is guarded + silenced + never fails.
        for (event, _m, cmd) in HOOK_MAP {
            if let HookCmd::State(state) = cmd {
                let c = &commands_for(&out, event)[0];
                assert!(
                    c.contains(&format!("session {state} --payload-session 2>/dev/null ||")),
                    "{event} records the payload session: {c}"
                );
            }
        }
        let cmd = &commands_for(&out, "Stop")[0];
        assert!(
            cmd.starts_with("[ -z \"$KEITORA_LAUNCH_HOOKS\" ] && [ -x \"/abs/keitora\" ]"),
            "guarded: {cmd}"
        );
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
        assert!(stop.contains(&hook_command("/abs/keitora", HookCmd::Stop, false)));
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
            vec![hook_command("/abs/keitora", HookCmd::Stop, false)]
        );
    }

    #[test]
    fn updates_changed_binary_path_without_duplicating() {
        let first = merged_hooks_settings(json!({}), "/old/keitora");
        let second = merged_hooks_settings(first, "/new/keitora");
        // only the new path remains for each event (no stale /old entry)
        assert_eq!(
            commands_for(&second, "Stop"),
            vec![hook_command("/new/keitora", HookCmd::Stop, false)]
        );
        assert_eq!(
            commands_for(&second, "UserPromptSubmit"),
            vec![hook_command("/new/keitora", HookCmd::State("busy"), false)]
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
            vec![hook_command(
                "/main/target/debug/keitora",
                HookCmd::Stop,
                false
            )],
            "pre-rename task command upgraded to keitora, no duplicate"
        );
    }

    #[test]
    fn exact_command_strings() {
        let b = "/abs/keitora";
        assert_eq!(
            hook_command(b, HookCmd::State("busy"), true),
            "[ -x \"/abs/keitora\" ] && { \"/abs/keitora\" session busy --payload-session 2>/dev/null || \"/abs/keitora\" session busy >/dev/null 2>&1; } || true # keitora:session-hook"
        );
        let gate = "[ -x \"/abs/keitora\" ] && \"/abs/keitora\" session gate --provider claude 2>/dev/null || true # keitora:session-hook";
        assert_eq!(hook_command(b, HookCmd::Gate, true), gate);
        assert_eq!(
            hook_command(b, HookCmd::Stop, true),
            "[ -x \"/abs/keitora\" ] && { \"/abs/keitora\" session idle --stdin 2>/dev/null || \"/abs/keitora\" session idle >/dev/null 2>&1; } || true # keitora:session-hook"
        );
        assert_eq!(
            hook_command(b, HookCmd::StopFailure, true),
            "[ -x \"/abs/keitora\" ] && { \"/abs/keitora\" session idle --failed 2>/dev/null || \"/abs/keitora\" session idle >/dev/null 2>&1; } || true # keitora:session-hook"
        );
        // The project copy is the same command behind the launch guard.
        for cmd in [
            HookCmd::State("busy"),
            HookCmd::Gate,
            HookCmd::Stop,
            HookCmd::StopFailure,
        ] {
            assert_eq!(
                hook_command(b, cmd, false),
                format!(
                    "[ -z \"$KEITORA_LAUNCH_HOOKS\" ] && {}",
                    hook_command(b, cmd, true)
                )
            );
        }
        // The CLI's install check keys off this literal.
        assert!(gate.contains("session gate"));
    }

    /// Every command a settings value installs.
    fn all_commands(v: &Value) -> Vec<String> {
        v["hooks"]
            .as_object()
            .unwrap()
            .keys()
            .flat_map(|e| commands_for(v, e))
            .collect()
    }

    #[test]
    fn project_file_hook_commands_carry_the_launch_guard() {
        let out = merged_hooks_settings(json!({}), "/abs/keitora");
        let cmds = all_commands(&out);
        assert_eq!(cmds.len(), HOOK_MAP.len() + CLAUDE_GATE_MAP.len());
        for c in cmds {
            assert!(c.starts_with("[ -z \"$KEITORA_LAUNCH_HOOKS\" ] && "), "{c}");
            assert!(c.ends_with(&format!("|| true # {HOOK_MARKER}")), "{c}");
        }
    }

    #[test]
    fn launch_settings_commands_do_not_carry_the_launch_guard() {
        let out = launch_hooks_settings("/abs/keitora");
        let cmds = all_commands(&out);
        assert_eq!(cmds.len(), HOOK_MAP.len() + CLAUDE_GATE_MAP.len());
        for c in &cmds {
            assert!(!c.contains("KEITORA_LAUNCH_HOOKS"), "{c}");
            assert!(c.starts_with("[ -x \"/abs/keitora\" ]"), "{c}");
        }
        // Same events, matchers and timeouts as the project copy.
        let project = merged_hooks_settings(json!({}), "/abs/keitora");
        assert_eq!(
            out["hooks"].as_object().unwrap().keys().collect::<Vec<_>>(),
            project["hooks"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            out["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"],
            600
        );
        // Both copies are ours: a project merge over the launch copy replaces it.
        let merged = merged_hooks_settings(out, "/abs/keitora");
        assert_eq!(merged, project);
    }

    #[cfg(unix)]
    #[test]
    fn launch_guard_makes_the_project_command_a_noop_when_the_env_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("ran");
        let bin = tmp.path().join("fake-keitora");
        write_executable(
            &bin,
            &format!("#!/bin/sh\necho \"$@\" >> '{}'\n", out.display()),
        );
        let project = hook_command(&bin.to_string_lossy(), HookCmd::State("busy"), false);
        let run = |env: Option<&str>| {
            let mut c = std::process::Command::new("sh");
            c.arg("-c").arg(&project).env_remove("KEITORA_LAUNCH_HOOKS");
            if let Some(v) = env {
                c.env("KEITORA_LAUNCH_HOOKS", v);
            }
            assert!(c
                .stdin(std::process::Stdio::null())
                .status()
                .unwrap()
                .success());
        };
        run(Some("1"));
        assert!(!out.exists(), "a Keitora launch runs only the launch copy");
        run(None);
        assert!(out.exists(), "a hand-started claude runs the project copy");
    }

    #[test]
    fn state_hook_commands_fall_back_when_the_flag_is_rejected() {
        for launch in [false, true] {
            for (event, _m, cmd) in HOOK_MAP {
                let HookCmd::State(state) = cmd else {
                    continue;
                };
                let c = hook_command("/abs/keitora", cmd, launch);
                assert!(
                    c.contains(&format!(
                        "{{ \"/abs/keitora\" session {state} --payload-session 2>/dev/null || \"/abs/keitora\" session {state} >/dev/null 2>&1; }}"
                    )),
                    "{event}: {c}"
                );
            }
        }
    }

    #[test]
    fn claude_settings_args_pass_a_plain_path_only() {
        let ok = |p: &str| claude_settings_arg(Path::new(p));
        assert_eq!(
            ok("/Users/me/.keitora/hooks/claude-launch-settings.json").as_deref(),
            Ok("/Users/me/.keitora/hooks/claude-launch-settings.json")
        );
        assert_eq!(
            ok(r"C:\Users\x\.keitora\hooks\claude-launch-settings.json").as_deref(),
            Ok("C:/Users/x/.keitora/hooks/claude-launch-settings.json"),
            "backslashes become /"
        );
        // The verbatim prefix only exists (and is only stripped) on Windows.
        #[cfg(windows)]
        assert_eq!(
            ok(r"\\?\C:\Users\x\.keitora\hooks\s.json").as_deref(),
            Ok("C:/Users/x/.keitora/hooks/s.json")
        );
        for bad in [
            "/Users/John Doe/.keitora/hooks/s.json",
            "/Users/me/it's/s.json",
            "/Users/me/\"q\"/s.json",
            "/Users/me/$HOME/s.json",
            "/Users/me/a;b/s.json",
            "/Users/me/é/s.json",
            "",
        ] {
            assert_eq!(ok(bad), Err("path needs quoting".to_string()), "{bad:?}");
        }
    }

    #[test]
    fn gate_entries_installed_with_timeout_only_there() {
        let out = merged_hooks_settings(json!({}), "/abs/keitora");
        let gate = hook_command("/abs/keitora", HookCmd::Gate, false);
        for (event, matcher) in CLAUDE_GATE_MAP {
            let arr = out["hooks"][event].as_array().unwrap();
            assert_eq!(arr.len(), 1, "{event}");
            assert_eq!(arr[0]["matcher"], matcher);
            assert_eq!(arr[0]["hooks"][0]["command"], gate);
            assert_eq!(arr[0]["hooks"][0]["timeout"], 600);
        }
        assert_eq!(out["hooks"]["PermissionRequest"][0]["matcher"], "");
        assert_eq!(out["hooks"]["PreToolUse"][0]["matcher"], "AskUserQuestion");
        for (event, _m, _c) in HOOK_MAP {
            for e in out["hooks"][event].as_array().unwrap() {
                assert!(e["hooks"][0].get("timeout").is_none(), "{event}");
            }
        }
    }

    #[test]
    fn is_ours_matches_every_new_command() {
        for cmd in [
            HookCmd::State("idle"),
            HookCmd::Stop,
            HookCmd::StopFailure,
            HookCmd::Gate,
        ] {
            let entry = json!({ "hooks": [ { "type": "command", "command": hook_command("/abs/keitora", cmd, false) } ] });
            assert!(is_ours(&entry), "{cmd:?}");
        }
    }

    #[test]
    fn user_hooks_on_gate_events_are_preserved() {
        let existing = json!({
            "hooks": {
                "PermissionRequest": [ { "matcher": "Bash", "hooks": [ { "type": "command", "command": "/usr/bin/mine.sh" } ] } ],
                "PreToolUse": [ { "matcher": "Write", "hooks": [ { "type": "command", "command": "/usr/bin/lint.sh" } ] } ]
            }
        });
        let out = merged_hooks_settings(existing, "/abs/keitora");
        let gate = hook_command("/abs/keitora", HookCmd::Gate, false);
        assert_eq!(
            commands_for(&out, "PermissionRequest"),
            vec!["/usr/bin/mine.sh".to_string(), gate.clone()]
        );
        assert_eq!(
            commands_for(&out, "PreToolUse"),
            vec!["/usr/bin/lint.sh".to_string(), gate]
        );
    }

    #[test]
    fn gate_path_change_replaces_and_second_install_is_noop() {
        let first = merged_hooks_settings(json!({}), "/old/keitora");
        let second = merged_hooks_settings(first, "/new/keitora");
        assert_eq!(
            commands_for(&second, "PermissionRequest"),
            vec![hook_command("/new/keitora", HookCmd::Gate, false)]
        );
        assert_eq!(
            commands_for(&second, "PreToolUse"),
            vec![hook_command("/new/keitora", HookCmd::Gate, false)]
        );
        assert_eq!(
            second,
            merged_hooks_settings(second.clone(), "/new/keitora")
        );
    }

    #[test]
    fn install_hooks_for_writes_once_in_temp_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        install_hooks_for(tmp.path(), "/abs/keitora");
        let file = tmp.path().join(".claude/settings.local.json");
        let first = std::fs::read_to_string(&file).unwrap();
        assert!(first.contains("session gate --provider claude"));
        install_hooks_for(tmp.path(), "/abs/keitora");
        assert_eq!(first, std::fs::read_to_string(&file).unwrap());
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
            resolve_keitora_bin(
                &wt,
                &KeitoraBinSources {
                    override_path: Some(override_s.clone()),
                    on_path: Some(path_s.clone()),
                    ..Default::default()
                }
            )
            .as_deref(),
            Some(path_s.as_str()),
            "PATH must win even when an override is set"
        );
        // No PATH → the override is the fallback.
        assert_eq!(
            resolve_keitora_bin(
                &wt,
                &KeitoraBinSources {
                    override_path: Some(override_s.clone()),
                    ..Default::default()
                }
            )
            .as_deref(),
            Some(override_s.as_str())
        );
        // Only PATH available → PATH.
        assert_eq!(
            resolve_keitora_bin(
                &wt,
                &KeitoraBinSources {
                    on_path: Some(path_s.clone()),
                    ..Default::default()
                }
            )
            .as_deref(),
            Some(path_s.as_str())
        );
        // Nothing available → None (caller skips install).
        assert_eq!(
            resolve_keitora_bin(&wt, &KeitoraBinSources::default()),
            None
        );

        // A renamed worktree build (`keitora` + sibling `keitora-app`) is used ONLY as a last
        // resort, when there is neither a PATH `keitora` nor an override.
        let build = dbg.join("keitora");
        mk_exec(&build);
        mk_exec(&dbg.join("keitora-app"));
        assert_eq!(
            resolve_keitora_bin(&wt, &KeitoraBinSources::default()).as_deref(),
            Some(build.to_string_lossy().as_ref()),
            "build is the last-resort fallback when nothing else resolves"
        );
        // ...but PATH and the override still outrank it.
        assert_eq!(
            resolve_keitora_bin(
                &wt,
                &KeitoraBinSources {
                    override_path: Some(override_s.clone()),
                    on_path: Some(path_s.clone()),
                    ..Default::default()
                }
            )
            .as_deref(),
            Some(path_s.as_str())
        );
        assert_eq!(
            resolve_keitora_bin(
                &wt,
                &KeitoraBinSources {
                    override_path: Some(override_s.clone()),
                    ..Default::default()
                }
            )
            .as_deref(),
            Some(override_s.as_str())
        );

        // A non-executable build file is ignored even with the sibling present —
        // `is_exec(&cli)` must gate the build candidate, not just `keitora-app.is_file()`.
        std::fs::remove_file(&build).unwrap();
        std::fs::write(&build, b"not exec\n").unwrap();
        assert_eq!(
            resolve_keitora_bin(&wt, &KeitoraBinSources::default()),
            None,
            "a non-executable build must not be used even with a sibling keitora-app"
        );
        mk_exec(&build); // restore for the next case

        // CRITICAL: a build `target/debug/keitora` WITHOUT a sibling `keitora-app` is the
        // pre-rename GUI app — it must NOT be trusted (else hooks launch a window),
        // even as a last resort.
        std::fs::remove_file(dbg.join("keitora-app")).unwrap();
        assert_eq!(
            resolve_keitora_bin(&wt, &KeitoraBinSources::default()),
            None,
            "keitora without sibling keitora-app must be treated as the GUI app, not the CLI"
        );
    }

    #[test]
    fn startup_hook_targets_fall_back_to_the_active_project() {
        let key = |p: &&str| p.to_string();
        // Installed app: the cwd is no project → the active project still gets hooks.
        assert_eq!(startup_hook_targets(None, Some("proj"), key), ["proj"]);
        // Dev run from the repo that is also active → installed once.
        assert_eq!(
            startup_hook_targets(Some("proj"), Some("proj"), key),
            ["proj"]
        );
        // Dev run from one repo while another is active → both.
        assert_eq!(
            startup_hook_targets(Some("proj"), Some("other-proj"), key),
            ["proj", "other-proj"]
        );
        assert_eq!(startup_hook_targets(Some("proj"), None, key), ["proj"]);
        assert!(startup_hook_targets::<&str, String>(None, None, key).is_empty());
    }

    /// A runnable `name` (an `.exe` on Windows, an exec-bit file on unix) in `dir`.
    fn mk_runnable(dir: &Path, name: &str) -> std::path::PathBuf {
        let p = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        write_executable(&p, "#!/bin/sh\n");
        p
    }

    /// The app-bundled CLI sits after PATH and the override but ahead of the
    /// worktree's own build, so a stale worktree build is never embedded when the
    /// running app ships a CLI.
    #[test]
    fn resolve_keitora_bin_prefers_app_cli_over_worktree_build() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        let dbg = wt.join("target").join("debug");
        std::fs::create_dir_all(&dbg).unwrap();
        let build = mk_runnable(&dbg, "keitora");
        mk_runnable(&dbg, "keitora-app");
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let app_cli = mk_runnable(&app_dir, "keitora-cli");
        let override_bin = mk_runnable(tmp.path(), "override-keitora");
        let path_bin = mk_runnable(tmp.path(), "path-keitora");
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let (app_s, override_s, path_s) = (s(&app_cli), s(&override_bin), s(&path_bin));

        // The bundled CLI beats the worktree build.
        assert_eq!(
            resolve_keitora_bin(
                &wt,
                &KeitoraBinSources {
                    app_cli: Some(app_s.clone()),
                    ..Default::default()
                }
            ),
            Some(app_s.clone())
        );
        // PATH and the override still outrank it.
        assert_eq!(
            resolve_keitora_bin(
                &wt,
                &KeitoraBinSources {
                    app_cli: Some(app_s.clone()),
                    on_path: Some(path_s.clone()),
                    ..Default::default()
                }
            ),
            Some(path_s.clone())
        );
        assert_eq!(
            resolve_keitora_bin(
                &wt,
                &KeitoraBinSources {
                    override_path: Some(override_s.clone()),
                    app_cli: Some(app_s.clone()),
                    ..Default::default()
                }
            ),
            Some(override_s.clone())
        );
        // A bundled path that is not runnable falls through to the build.
        let missing = s(&app_dir.join("gone"));
        assert_eq!(
            resolve_keitora_bin(
                &wt,
                &KeitoraBinSources {
                    app_cli: Some(missing.clone()),
                    ..Default::default()
                }
            )
            .map(std::path::PathBuf::from),
            Some(build)
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
            resolve_keitora_bin(&wt, &KeitoraBinSources::default()),
            None,
            "keitora.exe without sibling keitora-app.exe is the GUI app, not the CLI"
        );
        std::fs::write(dbg.join("keitora-app.exe"), b"MZ").unwrap();
        // Compare as PathBuf so separator style (native vs the test's join) doesn't
        // matter — only that it resolves to the same file.
        assert_eq!(
            resolve_keitora_bin(&wt, &KeitoraBinSources::default()).map(std::path::PathBuf::from),
            Some(exe.clone()),
            "keitora.exe + sibling keitora-app.exe resolves as the CLI build"
        );
    }
}
