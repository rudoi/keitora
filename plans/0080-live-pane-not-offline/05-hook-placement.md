# Phase 4: hook placement

Part of 0080; see `00-overview.md`.

## Purpose

Fewer ways for hooks to fail. After Phases 1-2, hooks matter for permission brokering, reply
text and the busy veto, not for liveness or completion. This phase makes them install reliably
across launches and provider changes, makes failures visible, and removes the stale-writer
hazard.

## Verified facts (code as of this branch)

- `src-tauri/src/hooks.rs`: `HOOK_MAP` (6 events: `UserPromptSubmit`, `Notification`
  (`permission_prompt`), `Stop`, `StopFailure`, `SessionStart`, `SessionEnd`),
  `CLAUDE_GATE_MAP` (`PermissionRequest`, `PreToolUse` matcher `AskUserQuestion`),
  `hook_command(task_bin, HookCmd)` (private), `posix_state_command` /
  `posix_fallback_command` (`pub(crate)`, shared with Codex), `merged_hooks_settings(existing,
  task_bin) -> Value` (pure, idempotent, marker `keitora:session-hook`), `install_hooks_for(wt,
  bin)` (writes `.claude/settings.local.json`), `resolve_keitora_bin(wt, &KeitoraBinSources)`,
  `keitora_bin_extra_env(cwd)` (empty under `cfg!(test)`), `keitora_on_path_cached()`,
  `install_hooks_into(config)` (startup; installs Claude file for every worktree and the Codex
  file only for `CliKind::Codex`). Claude runs hooks through a POSIX shell on every platform
  (Git Bash on Windows).
- `codex_hooks.rs`: `CODEX_HOOK_MAP`, `build_command(bin, CodexCmd, windows)` (PowerShell
  syntax on Windows, POSIX elsewhere), `merged_codex_hooks`, `install_codex_hooks_for`.
- `antigravity_hooks.rs`: `HOOK_EVENTS` (`SessionStart`, `PreInvocation`, `Stop`),
  `hook_command(args, windows)` (needs `KEITORA_BIN`, silent no-op without it), `keitora_hook`,
  `merged_antigravity_hooks`, `hooks_file(home)` (`<home>/.gemini/config/hooks.json`),
  `apply_hooks_at(file, windows, allowed)`, `install_antigravity_hooks`,
  `sync_registry`, `sync_worktree`. Writes go through `global_writes::allowed()` (false under
  `cfg!(test)` and when `KEITORA_HERDR_NO_INTEGRATION` is set; the dev launcher sets it).
  Nothing reads `ANTIGRAVITY_CLI_CONFIG_DIR` today.
- `backend/herdr/agent.rs`: `build_agent_args(cli, autonomy, &ResumeDecision)` calls
  `build_agent_args_opts(.., hook_trust = true)`. Codex args: `-c check_for_update_on_startup=false`,
  then `resume <id>` / `resume --last`, then `--dangerously-bypass-approvals-and-sandbox`
  (autonomy only), then `--dangerously-bypass-hook-trust`. Claude args: optional
  `--dangerously-skip-permissions`, then `--resume <id>` / `--continue`.
- `backend/herdr/strict.rs`: `strict_chain(&AgentLaunch) -> Vec<StrictStep>`. `StrictStep` has
  `decision`, `hook_trust: bool`, `label`. Steps: `resume <id>` (only when not fresh and the id
  is safe), `fresh`, `fresh without optional flags` (the only step with `hook_trust: false`).
  Line 512 calls `agent::build_agent_args_opts(.., step.hook_trust)`.
- Launch choke points, both of which already compute the pane env:
  `backend/herdr/mod.rs` `spawn_agent_locked` (about line 979, `env::pane_env_with(.., &keitora_bin_extra_env(cwd))`,
  then `build_agent_args(...)` at about line 995 and `ops.agent_start(...)`) and
  `strict.rs` `spawn_agent_transfer_strict_with` (about line 470, same call plus
  `codex_env::strict_pane_extra_env`). Recovery, boot-resume, reset, wake, UI switch and
  transfer all end in one of these two.
- `drive.rs`: trait `DriveHost::prepare_launch(&self, &ProjectConfig, &str, CliKind)` (line 438);
  `TauriHost` impl (line 486) returns early `if cli != CliKind::Antigravity`; the only caller is
  `relaunch` (about line 1451) and it runs only when the provider **changed**. `drive_tests.rs:78`
  is the fake host that records `prepare:<cli>`.
- `agent_switch.rs` (about line 149): after `persist_under_gate`, installs only for
  `target_cli == Antigravity` (`antigravity_hooks::sync_worktree` + `refresh_integration`).
  `host.rs` `worktree_create` (about lines 556-571) and `install_hooks_into` repeat the same
  per-provider logic inline. Transfer (`transfer.rs`) installs nothing.
- `backend/herdr/supervisor.rs` `ensure_ready` (about line 252) runs herdr's
  `integration install` for `integration_targets(agy_found)` once per process (skipped under
  `cfg!(test)` and `KEITORA_HERDR_NO_INTEGRATION`); `refresh_integration` resets the latch.
  Nothing re-checks Keitora's `keitora` key in `~/.gemini/config/hooks.json` afterwards.
- CLI (`crates/keitora-cli/src/main.rs`): hook entry is `Cmd::Session { state, stdin_payload,
  if_fully_idle, provider, stdin, failed }`. `run()` (about line 312): the gate runs first;
  `--stdin-payload` hooks go through `enter_payload_dir` (returns `None` silently on bad
  payload, filtered event or missing dir) and `run_resolved`, whose error is printed with
  `eprintln!` (about line 348) and swallowed; every other path returns `Err` to `main()` which
  prints it on stderr. Every hook command discards stderr (`>/dev/null 2>&1`, `>nul 2>&1`,
  `*> $null`, or `2>/dev/null` for the stdout-carrying forms), so every failure is lost.
  `run_session` (about line 453) calls `local.session_upsert(&me, state, current, None)`; the
  CLI has no herdr access (invariant D7). `keitora-cli` depends only on `keitora-tasks`, clap,
  serde_json, tokio.
- `<KEITORA_HOME>/logs/keitora.log` is the app log (rotating, `logging.rs`: 5 MiB, 4 backups).
  `keitora_home()` is in `src-tauri/src/paths.rs` today; `02-engine-core.md` S2 moves it into
  `keitora-tasks` as `keitora_tasks::keitora_home()` (`home.rs`, Phase 1 lands first), which the
  CLI uses for the hook log path and the app for the settings file and `hook_facts`.

## Design

### 1. One preparation function, run at the launch choke points

New in `hooks.rs`:

```rust
pub struct LaunchPrep {
    pub env: Vec<(String, String)>,        // KEITORA_PANE, KEITORA_BIN, KEITORA_LAUNCH_HOOKS
    pub agent_args: Vec<String>,           // appended to the provider args (Claude --settings)
}
pub fn prepare_worktree(wt: &Path, cli: CliKind, sources: &KeitoraBinSources) -> Prepared
pub fn launch_prep(cwd: &Path, cli: CliKind) -> LaunchPrep
```

`prepare_worktree` is the file-writing half, with explicit inputs so it is testable on a temp
dir. For `bin = resolve_keitora_bin(wt, sources)`:

| Provider | Writes (all idempotent, write only when content differs) |
|---|---|
| Claude | `.claude/settings.local.json` via `install_hooks_for` (fallback for hand-started sessions; guarded, see 2) and the per-launch settings file (see 2) |
| Codex | `.claude/settings.local.json` (as today, for hand-run claude) and `.codex/hooks.json` via `install_codex_hooks_for` |
| agy | `antigravity_hooks::sync_worktree(wt)` (global hook entry + workspace trust). No Claude/Codex file |

No `bin` resolved: the hook files are skipped (as today), and the outcome says so for `07`.

`launch_prep` wraps it for the launch path: returns `LaunchPrep::default()` under
`cfg!(test)` (like `keitora_bin_extra_env`, so the existing exact-env tests keep passing);
otherwise `sources = KeitoraBinSources::gather(None, keitora_on_path_cached().clone())`, runs
`prepare_worktree`, and returns:
- `env`: `("KEITORA_PANE", "1")` **always** (the agy marker, set even when no CLI resolves),
  `KEITORA_BIN` when resolved (existing behaviour), and `("KEITORA_LAUNCH_HOOKS", "1")` only
  when a Claude settings file was written and is being passed (see 2);
- `agent_args`: `["--settings", "<path>"]` for Claude under the same condition, else empty.

Call sites (replace the existing `keitora_bin_extra_env` call):
- `backend/herdr/mod.rs` `spawn_agent_locked`: `let prep = crate::hooks::launch_prep(Path::new(&cwd), launch.cli);`
  pass `&prep.env` as `extra`; after `build_agent_args(..)` do `args.extend(prep.agent_args.clone())`
  (inside the retry loop, so every fallback decision keeps the flag).
- `strict.rs` `spawn_agent_transfer_strict_with`: `extra.extend(prep.env)`; Codex has no
  `agent_args`.

Because both choke points run it, **every** launch prepares its provider: boot-resume,
recovery, reset, wake, UI switch, transfer and a plain new pane.

### 2. Claude: per-launch `--settings`, guarded against double firing (gated by Spike 3)

Applies only on the branch where Spike 3 passes (see the decision table in `spikes.md`).

- **Settings file:** `<KEITORA_HOME>/hooks/claude-launch-settings.json`, content
  `hooks::merged_hooks_settings(json!({}), bin)` rendered with `launch = true` commands (below).
  It is one file for all worktrees: the commands resolve the worktree from the hook's `cwd`.
  Written atomically with `keitora_tasks::write_atomic`, only when content differs.
  Why a file and not an inline JSON string: the argv goes through `herdr agent start -- <args>`
  into an interactive shell prompt on Windows (PowerShell/cmd) and macOS (zsh); inline JSON
  needs per-shell quoting.
- **Plain-argument rule:** the path is passed only when it matches `^[A-Za-z0-9_./:@+-]+$`
  after converting backslashes to `/`. Otherwise (a home with a space, say) no `--settings` is
  passed, `KEITORA_LAUNCH_HOOKS` is not set, and the project file stays the only source.
  `prepare_worktree` records `claude_launch_settings_skipped: "path needs quoting"` for `07`.
  Spike 3 step 6 records whether quoting works; if it does, lift the rule in a follow-up, not
  here.
- **Argument position:** after the existing Claude args (autonomy flag, resume flags):
  `... [--dangerously-skip-permissions] [--resume <id>|--continue] --settings <path>`.
- **Double-fire guard.** `--settings` hooks add to project-file hooks; both would run (double
  `session` writes, double reply capture, double gate prompts). So:
  - `hooks::hook_command` gets a `launch: bool` parameter (the Codex builders in
    `codex_hooks.rs` call the shared `posix_*` helpers, which get an extra `guard: bool`).
  - `launch = false` (project file): the command is prefixed with
    `[ -z "$KEITORA_LAUNCH_HOOKS" ] && `, e.g.
    `[ -z "$KEITORA_LAUNCH_HOOKS" ] && [ -x "<bin>" ] && "<bin>" session busy ... || true # keitora:session-hook`.
    The gate form keeps stdout attached and still ends `|| true`.
  - `launch = true` (settings file): no prefix.
  - Result: a Keitora-launched Claude (env set) runs only the settings-file hooks; a hand-started
    Claude in the worktree runs only the project-file hooks. Codex commands pass `guard =
    false` (unchanged text).
  - `is_ours` is unchanged (it matches the marker), so old entries are replaced on the next
    install.
- **User-level hooks:** herdr's `SessionStart` hook (user settings) is untouched and still
  fires alongside; Spike 3 step 4 confirms.

### 3. Codex

- Keep `.codex/hooks.json` (the `PermissionRequest` gate needs it); it is now written by
  `prepare_worktree` on every launch and provider change, not only at startup.
- **Trust bypass on every strict step.** The third strict step exists only to drop
  `--dangerously-bypass-hook-trust` (the sole optional flag) in case an old codex rejects it,
  and the normal launch path has no such fallback. Remove it: `StrictStep.hook_trust` is deleted
  and `strict_chain` returns `[resume <id>]? + fresh`. `strict.rs` line 512 calls
  `agent::build_agent_args(STRICT_CLI, launch.autonomy, &step.decision)`. `build_agent_args_opts`
  and its `hook_trust` parameter stay (still used by `build_agent_args` and by its tests).
  Update the module doc and `format_final_error` callers: the final error now reads
  `(tried: resume thr-1 -> fresh)`.
- No `-c notify` (D6).

### 4. agy: absolute path, marker, config-dir override, re-assert

- **Command shape** (`antigravity_hooks::hook_command(args, windows, bin: Option<&str>)`):
  - Windows (cmd.exe): `(if defined KEITORA_PANE "<bin>" session {args} >nul 2>&1) & exit /b 0`
  - POSIX: `[ -n "$KEITORA_PANE" ] && [ -x "<bin>" ] && "<bin>" session {args} >/dev/null 2>&1 || true`
  - `bin` is an absolute path with the `\\?\` prefix stripped (`keitora_tasks::strip_extended_prefix_str`).
    With `bin = None` (nothing resolvable) emit today's `KEITORA_BIN` form unchanged, so
    nothing regresses.
  - Hook output stays silent; failures are logged by the CLI itself (section 6).
- **Which bin:** new `hooks::resolve_global_keitora_bin(&KeitoraBinSources)`: on-PATH, then
  `[cli].path` override, then the app-bundled CLI. Never a worktree build (the file is global).
  `install_antigravity_hooks` and `sync_registry`/`sync_worktree` gather sources with
  `KeitoraBinSources::gather(None, keitora_on_path_cached().clone())`. The stored path is
  refreshed on every sync, so an app update that moves the bundled CLI heals at next start.
- **Marker:** `KEITORA_PANE=1` is set by `launch_prep` on every agent pane (section 1), so
  A1's failure mode (no `KEITORA_BIN` resolved, hook silently inert) cannot recur: the
  absolute path is in the file and the marker is unconditional.
- **File location:** `hooks_file_in(home, config_dir: Option<OsString>)`: if
  `ANTIGRAVITY_CLI_CONFIG_DIR` is set and non-empty, the file is `<that dir>/hooks.json`;
  else `<home>/.gemini/config/hooks.json`. `install_antigravity_hooks` passes
  `std::env::var_os("ANTIGRAVITY_CLI_CONFIG_DIR")`. The layout (the variable replaces the
  `.gemini/config` directory) is an inference from `research-herdr.md`; Spike 3b verifies it.
  `antigravity_trust.rs` is untouched.
- **Re-assert after herdr's integration install:** at the end of `Supervisor::ensure_ready`
  (after the `for target in integration_targets(agy_found)` loop, only when `agy_found`) call
  `crate::antigravity_hooks::sync_registry()`. It is already a no-op without
  `global_writes::allowed()` and when no worktree is on agy, and `apply_hooks_at` writes only
  on a diff, so a herdr rewrite that dropped or altered the `keitora` key is restored and an
  untouched file is not rewritten. `refresh_integration` goes through `ensure_ready`, so the
  Settings/UI paths (`agent_switch.rs`, `host.rs` settings handler) get it too.
- Global writes stay behind `global_writes::allowed()`. agy has no permission event: unbrokered.

### 5. Provider changes: what runs where

| Trigger | Entry point | Prepare step |
|---|---|---|
| Any launch (boot-resume, recovery, reset, wake, switch, transfer, new pane) | `spawn_agent_locked`, `spawn_agent_transfer_strict_with` | `launch_prep` (section 1). This is the guarantee |
| `reset --to`, `wake --to`, plain `reset` / `wake` via the pending-request path | `drive.rs` `relaunch` | `host.prepare_launch(..)` runs on **every** relaunch (remove the `if changed` guard; keep `changed` for `persist_cli` and `replace`), before `emit_restart` |
| UI switch | `agent_switch.rs` | replace the agy-only block with a call that prepares **any** target (blocking task, before the outcome returns); `refresh_integration()` stays agy-only |
| Transfer | `transfer.rs` | nothing extra: the strict spawn runs `launch_prep` |
| `worktree_create` | `host.rs` (about line 556) | replace the inline Claude/Codex/agy blocks with `prepare_worktree` for `wt.cli` |
| Startup / project change / settings change | `install_hooks_into`, `sync_registry` | unchanged, except they call `prepare_worktree` per worktree instead of repeating the logic |

`DriveHost::prepare_launch` (`drive.rs:486`) becomes: build `KeitoraBinSources::gather(Some(config),
keitora_on_path_cached().clone())`, call `hooks::prepare_worktree(&config.worktree_path(worktree),
cli, &sources)`, and for `CliKind::Antigravity` also `backend.refresh_integration()` (existing).
Update the trait doc comment ("hooks and trust for any provider"). The fake host in
`drive_tests.rs` is unchanged.

### 6. Hook error log (`keitora-cli`)

- New module `crates/keitora-cli/src/hook_log.rs`.
- **Where:** `<KEITORA_HOME>/logs/hooks.log` (same `logs/` dir as `keitora.log`; the isolated
  dev app logs to its own home because panes inherit `KEITORA_HOME`).
- **Format:** one line per event, `\r`/`\n` in messages escaped as the two characters:
  `unix=<epoch secs> LEVEL pid=<pid> cmd="session <state> [flags]" cwd="<cwd>" <message>`.
  `LEVEL` is `ERROR` (the hook failed) or `WARN` (the hook skipped on purpose: unreadable or
  oversize payload, filtered event, payload dir missing). No timestamps library: epoch
  seconds only (`date -d @N`).
- **Rotation:** before each append, if `hooks.log` is larger than 256 KiB, rename it over
  `hooks.log.1` (one backup, replaced each time), then append to a fresh file. Appends use
  `OpenOptions::append`, one `write_all` per line. Every I/O error is swallowed: logging must
  never fail a hook, and the hook still exits 0 exactly as today.
- **What is logged:**
  - every `Err` from a `Cmd::Session` run: in `run()`, wrap the existing flow so an error is
    logged before it propagates (and in the `--stdin-payload` branch, next to the existing
    `eprintln!`);
  - every silent skip in `enter_payload_dir` (distinguish: read failed or oversize, payload
    not JSON / no cwd, event filtered by `--if-fully-idle`, cwd not a directory), at `WARN`;
  - gate failures: the error paths in `gate::gate_main` that currently return silently
    (read each path; each gets one `ERROR`/`WARN` line).
- Not logged: successes (rows and heartbeats are the success evidence; `07` reads them).
- Shell-level failures (binary missing or not executable, so the CLI never runs) cannot be
  logged by the CLI; `07`'s `hooks_vs_herdr` finding (herdr sees an agent, no hook rows since
  it appeared) covers them.
- The line format is parsed back by `hook_facts` (section 8); `hook_log::format_line` /
  `parse_line` live in `crates/keitora-tasks/src/hook_log.rs`, and the CLI's
  `crates/keitora-cli/src/hook_log.rs` keeps only the append/rotate I/O.

### 7. Stale-writer guard

A hook write from a killed agent's session must not be taken as evidence about the new one.
The CLI cannot ask herdr (D7), so the guard is split:

- **CLI records the hook's session id.** `worktree_sessions` gets a nullable
  `session_id TEXT` column (additive migration: `ALTER TABLE ... ADD COLUMN` guarded by a
  `pragma_table_info` check, the same pattern as `confirm_started_at` at
  `sqlite_backend.rs` about line 158, plus the column in the `CREATE TABLE IF NOT EXISTS` at
  about line 112). `LocalStore::session_upsert_with_id(worktree, state, current, pid,
  session_id: Option<&str>)` is added; `session_upsert` delegates with `None`.
- **Where the id comes from.** Claude and Codex pass `session_id` in every hook payload on
  stdin. `--stdin` (Stop/StopFailure reply capture) already reads that payload: also extract
  `session_id` from it. State hooks (`busy`, `waiting`, `idle` from SessionStart, `offline`) do
  not read stdin today: add `--payload-session`, which reads the payload with
  `gate::read_bounded(.., stop_capture::STOP_STDIN_CAP, stop_capture::STOP_STDIN_TIMEOUT, terminal)` (64 KiB, 1 s), extracts `session_id`
  and records it. It prints nothing on success (so a primary command that falls back never
  leaks stdout into the agent: `UserPromptSubmit` stdout is injected into context). agy's
  `--stdin-payload` reads `conversationId` / `session id` fields the same way if present
  (check `hook_payload.rs`; absent means `None`).
- **Hook commands:** Claude/Codex state hooks become
  `posix_fallback_command(bin, "<state> --payload-session", "<state>", marker)` (PowerShell
  Codex forms get the equivalent `2>$null; if ($LASTEXITCODE -ne 0) { fallback }`), so an
  older installed CLI that rejects the flag still records the state.
- **The app ignores mismatches.** The engine applies `02-engine-core.md` S5 rule 0: a hook row
  whose `session_id` is `Some` and differs from herdr's current `agent_session` for that pane is
  treated as no row (it does not feed the most-recent-evidence rule; for Phase 6's
  `hooks_vs_herdr` it counts as "no hook rows"). Rows with `None` (old CLIs, hooks that carry no
  id) behave as today. The rule, `HookEvidence.session_id`, `LocalStore::session_ids()` and the
  derive table test `hook_row_from_another_session_is_ignored_as_evidence` land in Phase 1
  (inert until this column exists); this phase adds the column, the CLI writes, and an
  end-to-end engine test over a real temp `tasks.db` (below).
- Names are fixed: column `worktree_sessions.session_id`, CLI flag `--payload-session`, store
  method `session_upsert_with_id`, read method `session_ids`.

### 8. Read-only hook facts (input contract for `07-self-diagnostics.md`)

New in `hooks.rs`; pure reads, never writes (remedies stay `prepare_worktree` /
`antigravity_hooks::sync_worktree` + `sync_registry`):

```rust
pub const HOOK_VERSION: u32 = 1;  // bump whenever any generated hook command text changes

pub struct LaunchRecord {                 // kept per SessionKey in a process-wide Mutex<HashMap>
    pub at_ms: i64,
    pub cli: CliKind,
    pub trust_bypass: bool,               // Codex args carried --dangerously-bypass-hook-trust
    pub env_names: Vec<String>,           // names launch_prep set (KEITORA_PANE, KEITORA_BIN, KEITORA_LAUNCH_HOOKS)
    pub claude_settings: ClaudeSettings,  // Applied | Skipped(String) | NotApplicable (from Prepared)
    pub hook_version: u32,
}
pub fn note_launch(key: &SessionKey, rec: LaunchRecord);   // called by both spawn choke points after launch_prep
pub fn last_launch(key: &SessionKey) -> Option<LaunchRecord>;

pub struct HookFacts {
    pub bin: Option<PathBuf>,                 // resolve_keitora_bin for the worktree; None = no CLI resolvable
    pub codex: Option<CodexFacts>,            // Some for a Codex worktree
    pub claude: Option<ClaudeFacts>,          // Some for Claude and Codex worktrees (both get the project file)
    pub agy: Option<AgyFacts>,                // Some for an agy worktree
    pub last_launch: Option<LaunchRecord>,
    pub hook_version_ok: bool,                // last_launch.hook_version == HOOK_VERSION; true with no launch
    pub last_hook_error: Option<HookLogLine>, // newest ERROR line in hooks.log whose cwd is inside the worktree, last 24 h
}
pub struct CodexFacts { pub hooks_json_ok: bool,                 // .codex/hooks.json has every CODEX_HOOK_MAP entry with our marker
                        pub trust_bypass_last_launch: Option<bool>, // None: no launch recorded this process
                        pub project_trust: ProjectTrust }        // read-only look at ~/.codex/config.toml (below)
pub enum ProjectTrust { Trusted, Untrusted(String /*trust_level value or ""*/), NotListed,
                        Unreadable(String /*why*/), NotApplicable }
pub struct ClaudeFacts { pub hooks_installed: bool,              // .claude/settings.local.json has every HOOK_MAP entry with our marker
                         pub per_launch_settings: ClaudeSettings }// from the last launch record (NotApplicable without one)
pub struct AgyFacts { pub key_present: bool,                     // hooks_file_in(..) has the "keitora" key
                      pub path_ok: bool,                         // its command embeds an absolute path to an existing file
                      pub marker_env_ok: Option<bool>,           // last launch set KEITORA_PANE; None without a launch
                      pub translocated: bool }                   // exe_is_untrusted_for_bundled_cli(current_exe)
pub struct HookLogLine { pub unix_s: i64, pub level: String, pub cmd: String, pub message: String }

pub fn hook_facts(config: &ProjectConfig, worktree: &str) -> HookFacts;
```

- `hook_facts` reads the two project files, the agy hooks file, the launch record and the tail
  (last 64 KiB) of `<keitora_home()>/logs/hooks.log` (parsed with `hook_log::parse_line`, which
  lives in `keitora-tasks` so the CLI writer and the app reader share the format). Any read error
  makes the affected field `false`/`None`, never a panic.
- **`project_trust` (report-only, never a write).** Codex records trust per project in its
  user config as `[projects."<path>"]` with `trust_level = "trusted"` (or `"untrusted"`).
  `hook_facts` reads `<codex home>/config.toml` (home resolved as `codex_env.rs` does:
  `CODEX_HOME`, else `~/.codex`) with the pure `keitora_tasks::codex_project_trust(toml_text,
  worktree_path) -> ProjectTrust` (`keitora-tasks` already depends on `toml`). Matching: the
  worktree path or any ancestor path is a `projects` key (Codex trusts a git root, and a
  worktree may sit inside a trusted root); keys and the path are compared after normalising
  separators, a trailing slash, a Windows verbatim path prefix and, on Windows and macOS, case.
  Results: a matching key with `trust_level == "trusted"` -> `Trusted`; matching key with
  another value -> `Untrusted(value)`; no matching key -> `NotListed`; file missing,
  unreadable or invalid TOML -> `Unreadable(why)` (a missing file is `NotListed`, not an error);
  non-Codex worktrees -> `NotApplicable`. Keitora never edits `config.toml` (the user's own file,
  global scope; D6's reasoning applies). Why it matters even though every Keitora launch passes
  `--dangerously-bypass-hook-trust`: that flag covers the hook trust prompt of Keitora's own
  launches only. An untrusted project layer still makes Codex ignore project-scoped config and
  hooks for any launch **without** the flag (a hand-launched `codex` in the pane, a resume
  that drops it, an old Codex that rejects it), and herdr's own session-id hook and the
  `agent_session` it feeds the stale-writer guard (section 7) rest on Codex running its hooks
  in that project. The finding tells the human so, instead of leaving it to be discovered as a
  silent missing reply.
- `prepare_worktree`'s `Prepared` result (`bin_resolved`, `claude_settings`, `codex_file`) feeds
  `LaunchRecord`; `claude_settings = Skipped("path needs quoting")` is reported in the
  `hooks_missing` finding's `detail`, not as its own check.
- Tests (`hooks.rs`): `hook_facts_reports_each_missing_piece` (temp worktree + temp home: codex
  file missing, claude file missing, agy key missing, relative agy path),
  `hook_facts_reads_the_last_hook_error_for_this_worktree_only`,
  `hook_version_mismatch_is_reported`, `note_launch_records_the_trust_bypass`,
  `hook_facts_reports_codex_project_trust` (temp `CODEX_HOME` with a `config.toml`; see 07).
- `keitora-tasks` (`codex_trust.rs`, pure): `codex_project_trust_matches_the_worktree_path`,
  `codex_project_trust_matches_an_ancestor_root`, `codex_project_trust_other_level_is_untrusted`,
  `codex_project_trust_unlisted_and_missing_file_are_not_listed`,
  `codex_project_trust_invalid_toml_is_unreadable`,
  `codex_project_trust_normalises_separators_case_and_verbatim_prefix`,
  `codex_project_trust_does_not_match_a_sibling_prefix` (`/Users/me/proj` vs `/Users/me/proj2`).

### 9. Stop-capture hardening (`keitora session idle --stdin`)

Stop capture (`stop_capture::capture_message`, `run_session`, `LocalStore::session_reply_record`)
is the one writer of `session_replies.turn`, which the legacy `wait` path and the engine's
`hook_finished` clause read (`03-turns-and-cli.md` T5). The engine no longer depends on it for
completion, but a wrong bump still turns into a wrong or duplicate `turn_finished`, so its
contract is pinned here (it was implicit before):

- **The documented Codex Stop payload.** One JSON object on stdin with at least `session_id`,
  `transcript_path`, `cwd`, `hook_event_name: "Stop"`, `turn_id` and `last_assistant_message`,
  which is a **string or `null`** (null when the turn produced no final message, for example an
  interrupted turn). `stop_hook_active` may be present. Claude's Stop payload has the same
  `session_id`, `transcript_path` and `hook_event_name`, and no `turn_id`.
- **Exactly one turn per invocation.** Every `--stdin` invocation records exactly one turn
  (one `turn` increment, one `session_replies` write), whatever the payload: a full Codex
  payload, `last_assistant_message: null`, the JSON literal `null`, a JSON array, garbage
  bytes, invalid UTF-8, empty stdin, an oversize payload (cap `STOP_STDIN_CAP`) and a read
  timeout. Unusable payloads record an **empty** message; they never skip the bump and never
  bump twice. (A Stop that fired is a finished turn even when its text is lost; reply text then
  comes from the transcript fallback, `06-reply-fallbacks.md`.) `capture_message` already never
  panics on bad input; this makes the one-bump rule explicit and tested.
- **Message precedence is unchanged:** `--failed` fixed text; else a non-empty string
  `last_assistant_message`; else the `transcript_path` fallback; else empty. A `null`, an empty
  string and a non-string value all mean "no payload message".
- **Dedupe of a repeated Stop by `turn_id`.** A Stop can reach the CLI twice for one turn
  (Codex re-fires on `stop_hook_active`, or a project file and a user-level hook both run).
  `stop_capture::payload_turn_id(payload) -> Option<String>` reads `turn_id` (a string, or a
  number rendered as text; empty, over 128 bytes or any other type is `None`).
  `session_replies` gets a nullable `last_turn_id TEXT` (additive migration, same
  `pragma_table_info` guard as section 7) and the store gets
  `LocalStore::session_reply_record_for_turn(worktree, last_message, turn_id: Option<&str>) ->
  Result<i64>`; `session_reply_record` delegates with `None`.
  - `turn_id == Some(x)` equal to the stored `last_turn_id`: **no bump**; the stored message is
    replaced only when the stored one is empty and the new one is not (a second copy that carries
    the text completes the first one's empty capture). Returns the existing `turn`.
  - Any other case (`None`, or a different id): bump as today and store the id (`None` clears
    it).
  - State is still upserted (`idle`) on a deduped Stop (idempotent).
- Names are fixed: `payload_turn_id`, `session_reply_record_for_turn`, column
  `session_replies.last_turn_id`.

## Files touched

- `src-tauri/src/hooks.rs` (`LaunchPrep`, `prepare_worktree`, `launch_prep`,
  `resolve_global_keitora_bin`, `hook_command(.., launch)`, `--payload-session` commands,
  `claude_launch_settings`, `HOOK_VERSION`, `LaunchRecord`/`note_launch`, `HookFacts`/`hook_facts`)
- `crates/keitora-tasks/src/hook_log.rs` (new: line format and parser shared with the app)
- `src-tauri/src/codex_hooks.rs` (guard param, `--payload-session` forms)
- `src-tauri/src/antigravity_hooks.rs` (`hook_command` shape, `hooks_file_in`, sources)
- `src-tauri/src/backend/herdr/mod.rs` (`spawn_agent_locked`: `launch_prep`, args)
- `src-tauri/src/backend/herdr/strict.rs` (`strict_chain`, `StrictStep`, line 512, docs)
- `src-tauri/src/backend/herdr/supervisor.rs` (`ensure_ready` re-assert)
- `src-tauri/src/drive.rs` (`TauriHost::prepare_launch`, `relaunch`), `agent_switch.rs`,
  `host.rs` (`worktree_create`, `install_hooks_into`)
- `crates/keitora-cli/src/main.rs`, `hook_log.rs` (new), `gate.rs` (log calls),
  `hook_payload.rs`
- `crates/keitora-tasks/src/sqlite_backend.rs`, `local_store.rs` (`session_id` column,
  `session_replies.last_turn_id`, `session_reply_record_for_turn`), `codex_trust.rs` (new)
- `crates/keitora-cli/src/stop_capture.rs` (`payload_turn_id`, section 9 fixtures)

## Reports fixed

A1 (agy rows), A2 (hooks after `reset --to`), B (Codex Stop hook not running).

## Regression tests

Names are for the implementer to use verbatim.

**Repros (written RED first, `00-overview.md` "Reproductions")**
- `repro_a1_agy_hook_command_is_inert_without_keitora_bin` (`antigravity_hooks.rs`): the old
  generated command, run with no `KEITORA_BIN`, records nothing; fails on the old text.
- `repro_a2_reset_to_codex_prepares_codex_hooks` (`drive_tests.rs`): `reset --to codex` on the
  fake host logs no `prepare:codex` on the old code.

**`antigravity_hooks.rs`**
- `hook_command_windows_embeds_the_absolute_path_and_marker`
- `hook_command_posix_embeds_the_absolute_path_and_marker`
- `hook_command_without_a_resolved_bin_keeps_the_keitora_bin_form`
- `hook_command_is_a_noop_without_the_marker` (assert the string shape: marker test comes first
  in both shells)
- `hooks_file_in_honours_the_config_dir_override` (set, empty = default, unset = default)
- `apply_hooks_at_restores_a_key_herdr_dropped` and
  `apply_hooks_at_restores_a_key_whose_command_was_rewritten` (simulate herdr rewriting the file)
- `apply_hooks_at_leaves_an_unchanged_file_untouched` (mtime / return `Ok(false)`)
- update the existing `hook_command_*` and `merge_*` tests to the new signature.

**`hooks.rs`**
- `prepare_worktree_claude_writes_project_file_and_launch_settings` (temp worktree + temp
  `KEITORA_HOME`)
- `prepare_worktree_codex_writes_codex_hooks_json`
- `prepare_worktree_agy_writes_no_project_files` (global writer is inert under test)
- `prepare_worktree_is_idempotent_and_does_not_rewrite`
- `prepare_worktree_without_a_bin_skips_and_reports`
- `launch_prep_is_empty_under_test`
- `launch_pane_env_always_carries_the_marker` (the pure helper behind `launch_prep.env`: marker
  present with and without a bin; `KEITORA_LAUNCH_HOOKS` only with a settings file)
- `claude_settings_args_pass_a_plain_path_only` (spaces, quotes, backslashes -> converted or
  refused)
- `project_file_hook_commands_carry_the_launch_guard` and
  `launch_settings_commands_do_not_carry_the_launch_guard`
- `launch_guard_makes_the_project_command_a_noop_when_the_env_is_set` (run the POSIX string via
  `sh -c` on unix CI; skip on Windows)
- `state_hook_commands_fall_back_when_the_flag_is_rejected` (string shape of
  `posix_fallback_command` use)

**`backend/herdr/strict.rs` and `mod.rs`**
- Replace `strict_chain_resume_has_three_steps` with `strict_chain_resume_has_two_steps`;
  update `strict_chain_fresh_starts_at_fresh`, `strict_chain_without_target_starts_at_fresh`,
  `strict_chain_skips_an_unsafe_thread_id` (length 1 for the unsafe case: just `fresh`).
- `every_strict_step_carries_the_hook_trust_bypass` (all (fresh, target) combinations, all
  autonomy values: each step's args contain `--dangerously-bypass-hook-trust`).
- Update in `mod.rs`: `EXHAUSTED` constant (`tried: resume thr-1 -> fresh`),
  `strict_early_exit_flag_rejection_drops_the_flag` becomes
  `strict_early_exit_on_every_step_ends_the_chain` (script `[EarlyExit, EarlyExit]`, expects an
  error and args `[resume thr-1 HOOK, HOOK]`),
  `strict_agent_never_appeared_exhausts_with_structured_error` (two steps; `closed_pane_ids`
  two entries), `strict_trail_records_every_step_and_observation` (two launches),
  `strict_chain_never_exceeds_budget` (script two starts). Run
  `cargo test -p keitora --lib backend::herdr` and fix each remaining failure the same way.
- `claude_launch_appends_settings_after_resume_flags` (spawn path with a test seam supplying
  `LaunchPrep`; the fake `agent_start` args end with `--settings <path>`, and a resume fallback
  step keeps it).
- Update `default_launch_env_comes_from_pane_env_with` (`mod.rs` about line 8166): `want` uses
  `pane_env_with(.., &[])`; still true because `launch_prep` is empty under test.

**`drive.rs` / `drive_tests.rs`**
- `reset_to_codex_prepares_codex` and `wake_to_codex_prepares_codex` (the fake host logs
  `prepare:codex`).
- `relaunch_without_a_provider_change_still_prepares` (plain `reset`, `wake`).
- `prepare_launch_is_called_before_the_restart_event`.

**`agent_switch.rs`**
- `switch_to_claude_and_codex_prepare_hooks` (call the extracted prepare helper with a temp
  worktree); existing agy switch tests stay.

**`supervisor.rs`**
- A pure seam for the re-assert (`reassert_agy_hooks(agy_found, sync: impl FnOnce())`) with
  `reassert_runs_only_when_agy_was_found`.

**`keitora-cli`**
- `hook_log.rs`: `appends_one_line_per_event_with_context`,
  `escapes_newlines_in_messages`, `rotates_to_one_backup_past_the_cap`,
  `swallows_io_errors` (log dir is a file), `path_is_under_the_given_home` (use the
  `log_in(dir, ..)` form).
- `main.rs` / `hook_payload.rs`: `silent_payload_skips_are_classified` (each skip reason maps
  to a distinct logged message), `session_failure_is_logged_and_still_exits_zero_in_hook_mode`
  (pure helper that formats and decides).
- `payload_session_extracts_session_id` / `payload_session_absent_is_none`,
  `payload_session_flag_prints_nothing`.

**Stop capture (section 9; `stop_capture.rs` and `main.rs` tests, pure; fixtures use placeholder
paths and ids)**
- `codex_stop_payload_string_message_is_captured` (full documented payload: `session_id`
  `"s-1"`, `turn_id` `"t-1"`, `last_assistant_message` a string, `transcript_path` pointing at a
  missing file -> the string, not the transcript).
- `codex_stop_payload_null_message_falls_back_to_the_transcript` (`last_assistant_message:
  null` + a temp rollout fixture -> its last assistant text) and
  `codex_stop_payload_null_message_without_transcript_is_empty`.
- `one_turn_per_invocation_for_every_payload_shape` (table over: full payload, null message,
  empty-string message, JSON `null`, JSON array, garbage bytes, invalid UTF-8, empty stdin,
  oversize payload, read timeout: run the `run_session` capture path against a temp `tasks.db`;
  `session_replies.turn` goes exactly 0 -> 1 -> 2 across two invocations, never +2 or +0;
  unusable payloads store an empty message; `worktree_sessions` ends `idle`).
- `failed_stop_records_one_turn_with_the_fixed_message`.
- `payload_turn_id_reads_string_and_number_and_rejects_the_rest` (missing, empty, over 128
  bytes, object, `null` -> `None`).
- `repeated_stop_with_the_same_turn_id_does_not_bump_twice` (two invocations, same `turn_id`:
  `turn` stays 1; a different `turn_id` -> 2; `None` twice -> 2, the legacy behaviour).
- `repeated_stop_fills_an_empty_message_but_never_replaces_text` (first copy empty, second
  has text -> text stored, `turn` 1; first has text, second differs -> first text kept).
- `dedupe_keeps_state_idle_and_prints_nothing`.

**`keitora-tasks`**
- `session_reply_record_for_turn_dedupes_and_migrates`, `old_database_gains_the_last_turn_id_column`
  (an old `session_replies` table keeps its rows; the plain `session_reply_record` still bumps
  every call).
- `session_upsert_with_id_round_trips`, `session_upsert_without_id_clears_it` (an id-less
  write replaces the stored id with NULL; decision below),
  `old_database_gains_the_session_id_column`.

**Engine** (the derive table test of the same name is Phase 1's, `02-engine-core.md` S13)
- `stale_writer_row_from_a_previous_session_is_ignored_end_to_end` (temp `tasks.db` with the
  `session_id` column, a `busy` row from `s-old`, `FakeEvidence` replaced by `RegistryEvidence`,
  obs `agent_session` `s-new` settled idle -> view `idle`).

## Dependencies

- `02-engine-core.md`: `keitora_home()` in `keitora-tasks` (hook log path, settings file path);
  the engine exposes `agent_session` per pane for the guard; hook rows are already evidence only.
- Spike 3 (`spikes.md`, blocking) gates the Claude part; Spike 3b (non-blocking, default branch
  exists) decides the agy `ANTIGRAVITY_CLI_CONFIG_DIR` / re-assert details. Spike 1's Codex-without-bypass check can raise the priority of the bypass fix.
- `07-self-diagnostics.md` reads `hook_facts` (section 8) for its `hooks_missing` and
  `agy_plumbing` checks (the skipped Claude settings file goes in `hooks_missing`'s `detail`),
  covers "installed but no rows" with `hooks_vs_herdr`, and uses `prepare_worktree` ("reinstall
  on next launch") and `sync_worktree` + `sync_registry` ("re-assert the key") as remedies.
  `prepare_worktree` returns a small `Prepared` struct (`bin_resolved`, `claude_settings:
  Applied|Skipped(reason)|NotApplicable`, `codex_file: Written|Unchanged|NotApplicable`) that
  feeds the launch record.

## Singletonize decisions

- **Prepare at the two spawn choke points, not only on provider change.** Every launch path
  (recovery, boot-resume, wake, strict transfer) funnels through `spawn_agent_locked` or the
  strict spawn, so one `launch_prep` call fixes all of them; `DriveHost::prepare_launch` still
  runs on every relaunch to prepare early, and the UI switch prepares any provider.
- **Strict chain loses its third step** rather than keeping it without the bypass. The
  proposal says the bypass is on every step and no other optional flag exists; the old third
  step would have been a duplicate of `fresh` with the same flags. Cost: a codex that rejects
  the flag now fails the transfer launch loudly (as a normal launch already would) instead of
  starting with silent hooks.
- **Claude settings go in a file, passed by path, only when the path needs no quoting.** This
  avoids guessing how herdr composes the shell line on Windows/macOS. Fallback is the project
  file. The Spike 3 quoting result can lift this later.
- **`KEITORA_LAUNCH_HOOKS` guard** on project-file commands avoids double firing and keeps the
  project file as the hand-started-session fallback the proposal asks for. Rejected: removing
  project-file hooks when `--settings` is used (would break hand-started sessions); deduping
  in the CLI (can't tell which copy ran).
- **Marker name `KEITORA_PANE=1`**, set on every agent pane unconditionally (taken from
  `research-signals.md`).
- **agy bin resolution ignores worktree builds** (global file); `bin = None` keeps the old
  `KEITORA_BIN` form so nothing regresses.
- **Re-assert hook point:** end of `ensure_ready`, because that is the one place herdr's
  install runs and `refresh_integration` reuses it. No file watcher.
- **Hook log:** epoch-second timestamps (no new dependency), one 256 KiB backup, same `logs/`
  dir as the app log; successes are not logged.
- **Hook facts are read-only** (`hook_facts`, section 8) and are the only interface 07 uses;
  the launch record is in memory (a restart forgets it, so launch-dependent facts are `None`
  until the next launch).
- **Stale-writer guard is split:** column + CLI write here, comparison in the engine
  (`02-engine-core.md` S5 rule 0).
  An id-less write (an older hook command) overwrites the stored id with NULL so a stale id
  never outlives the writer that set it.
- **State hooks use `--payload-session` with a fallback** (an old CLI on PATH keeps working)
  and the flag prints nothing, because a primary that leaks stdout would inject text into the
  agent's context.
