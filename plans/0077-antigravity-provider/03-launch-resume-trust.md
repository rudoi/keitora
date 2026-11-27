# 03 — Discovery, launch, trust, resume

## Goal

Find `agy` on each OS, launch it through herdr (`--kind agy`), pre-seed its workspace trust, and resume its
conversation across restarts through herdr's `antigravity-cli` integration, the resume store and the fallback
chain.

## Context

- Verified facts (`facts.md`):
  - The binary is `agy`. `antigravity` on PATH is the VS Code-style IDE launcher (prints `1.107.0` and a
    commit hash); discovery must never pick it.
  - Windows install: `%LOCALAPPDATA%\agy\bin\agy.exe`. The installer does not add that folder to the PATH of
    already-running processes. unix install: `~/.local/bin/agy`.
  - Flags: resume by id `--conversation <id>` (aliases `-r`, `--resume`); continue `-c` / `--continue`;
    `--dangerously-skip-permissions` auto-approves tool permission requests.
  - Conversation state: `~/.gemini/antigravity-cli/brain/<conversationId>/` and
    `conversations/<conversationId>.db`; ids are lowercase UUIDs.
  - `~/.gemini/antigravity-cli/settings.json` has a `trustedWorkspaces` key (verified in binary strings); the
    element shape (string paths vs objects) is **unverified**. A malformed `settings.json` is logged and
    ignored by `agy`.
  - herdr 0.9.5: `herdr agent start --kind agy` is supported; `herdr integration install antigravity-cli`
    installs `~/.gemini/config/hooks/herdr-agent-state.ps1`. Expected `agent_session.source` is
    `herdr:antigravity_cli` with agent `antigravity_cli` (**unverified** live), so `cli_of` accepts
    `antigravity_cli`, `antigravity-cli` and `agy`.
- Decision 3: Keitora pre-seeds workspace trust for `agy`, so its first-run trust prompt doesn't block a
  launch.
- From doc 01: `CliKind::Antigravity`, `CliKind::ALL`, `ProviderSpec` (`binary`, `herdr_kind`,
  `herdr_integration`, `herdr_agent_names`). From doc 02: `WorktreeEntry.antigravity_conversation_id`.
- Step 3 of the implementation outline.
- Verified code state (before this work):
  - `discover_cli` unix `crates/keitora-tasks/src/config.rs:2558`, Windows `:2643`; `discover_cli_cached`
    `:2632` with `DISCOVER_NONE_TTL = 30 s`; helpers `login_path`, `find_in_path`, `first_executable`
    (`:2583`), `find_on_path_windows(path: &str, names: &[&str])` (`:2305`), `is_executable_file`.
  - `agent_launch_with_discovery` `src-tauri/src/host.rs:1435-1472`; `discover_for_launch` `:1419-1431`.
  - `build_agent_args_opts` `src-tauri/src/backend/herdr/agent.rs:53`; `next_fallback` `:102`;
    `build_agent_args_matrix` test `:239`.
  - `ResumeStore::resolve` `resume.rs:254`, `cli_of` `:62`, `observe` `:193`, `ResolveCtx` `:39`, private
    `is_safe_session_id` wrapper `:346`.
  - `HerdrBackend.projects_dir` `herdr/mod.rs:292`, second copy in the resolver struct `:786`, set at `:1982`
    and `:2019`, cloned at `:2178`, used at `:810`.
  - supervisor `ensure_ready` `herdr/supervisor.rs:224-262` (target list literal at `:243`);
    `NO_INTEGRATION_ENV` `:40`.
  - fake herdr `resume_id_from` `herdr/fake.rs:217`.

## Files to touch

- `crates/keitora-tasks/src/config.rs` (`discover_cli` x2, new candidate helpers, `is_safe_session_id`
  rename at `:81` and its tests `:6999-7018`), `crates/keitora-tasks/src/lib.rs` (`:41` re-export rename)
- `src-tauri/src/host.rs` (`agent_launch_with_discovery`, `get_settings` uses doc 02's helper)
- `src-tauri/src/backend/herdr/agent.rs` (args arm, imports `:11`, call `:62`, tests)
- `src-tauri/src/backend/herdr/strict.rs` (`:22` import, `:146` call)
- `src-tauri/src/transfer.rs` (`:2250` call)
- `src-tauri/src/backend/herdr/resume.rs` (`cli_of`, `usable`, `ResolveCtx`, delete wrapper `:344-348`,
  import `:17`, tests)
- `src-tauri/src/backend/herdr/mod.rs` (`antigravity_dir` field at `:292` and `:786`, init `:1982/:2019`,
  clone `:2178`, ctx `:805-811`, test helper `antigravity_launch()` next to `codex_launch()` at `:8100`)
- `src-tauri/src/backend/herdr/supervisor.rs` (target list)
- `src-tauri/src/backend/herdr/fake.rs` (`resume_id_from`)
- new `src-tauri/src/antigravity_trust.rs`, new `src-tauri/src/global_writes.rs`, `src-tauri/src/lib.rs` (two
  `mod` lines, alphabetical: `mod antigravity_trust;` after `mod agent_updates;`, `mod global_writes;` after
  `mod diff; mod dispatch;`)
- `src/App.tsx` (availability refresh, `:187-190`, `:358-361`), `src/components/Rail.tsx` (new
  `onWorktreeMenuOpen` prop; the menu change itself is doc 06)

## Design

### Discovery

`crates/keitora-tasks/src/config.rs`, both `discover_cli` functions (`#[cfg(unix)]` `:2558`,
`#[cfg(windows)]` `:2643`) get an `CliKind::Antigravity` arm. It looks only for `agy`, never `antigravity`.

New pure candidate helpers (compiled on every OS so they are testable everywhere; mark
`#[cfg_attr(not(…), allow(dead_code))]` the same way `codex_unix_candidates` does at `:2574`):

```rust
/// unix: `<home>/.local/bin/agy`. A `None` home gives an empty list.
pub(crate) fn agy_unix_candidates(home: Option<&Path>) -> Vec<PathBuf>
/// Windows, in priority order: `<local_app_data>/agy/bin/agy.exe`, then `<home>/.local/bin/agy.exe`.
/// A `None` input drops its entry.
pub(crate) fn agy_windows_candidates(local_app_data: Option<&Path>, home: Option<&Path>) -> Vec<PathBuf>
```

- **unix arm:** `first_executable(&agy_unix_candidates(dirs::home_dir().as_deref()), &is_executable_file)` first
  (the `~/.local/bin/agy` install comes before the login `$PATH`); else `login_path()` then
  `find_in_path(&path, "agy")`.
- **Windows arm:** the first of `agy_windows_candidates(std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
  .as_deref(), dirs::home_dir().as_deref())` that `is_executable_file`; else
  `find_on_path_windows(&std::env::var_os("PATH")?.to_string_lossy(), &["agy.exe", "agy.cmd", "agy.bat"])`.

`host.rs` `agent_launch_with_discovery` (`:1450-1461`):

- the `or_else` becomes `match cli { CliKind::Codex | CliKind::Antigravity => discover(cli),
  CliKind::Claude => None }`;
- the final `unwrap_or_else` default becomes `cli.spec().binary.to_string()` (so `claude`, `codex`, `agy`);
- `resume_target` gets `CliKind::Antigravity => entry.and_then(|e| e.antigravity_conversation_id.clone())`.

herdr resolves the executable by kind, so the pane PATH must contain `agy`'s directory. That already holds:
`env::pane_env_with` (`env.rs:759`) prepends the directory of `launch.path` when it is an absolute path, and
discovery returns an absolute path. (The installer does not update PATH for already-running processes.)

### Availability refresh

`get_settings` returns `user.available` (doc 02). Frontend:

- `src/App.tsx:190` `const [codexAvailable, setCodexAvailable] = useState(false)` becomes
  `const [available, setAvailable] = useState<Record<CliKind, boolean>>({claude: false, codex: false,
  antigravity: false})`. Replace the stale comment at `:187-190`.
- `:358-361` effect becomes
  `const refreshAvailability = useCallback(() => { void getSettings().then((s) =>
  setAvailable(s.user.available)).catch(() => {}); }, []);` plus `useEffect(() => { refreshAvailability(); },
  [refreshAvailability])` (once on mount).
- `refreshAvailability` is also called (a) when the worktree menu opens: `Rail` gains a prop
  `onWorktreeMenuOpen: () => void`, invoked from the menu's `onOpenChange(true)` path (`Rail.tsx` where
  `setMenu(state)` opens a menu), wired to `refreshAvailability` at `App.tsx:~1258`; (b) when the settings
  drawer closes (`SettingsDrawer` `onClose` handler in `App.tsx`).
- `App.tsx:1258` `codexAvailable={codexAvailable}` becomes `available={available}`; the `Rail` prop type
  changes to `available: Record<CliKind, boolean>` (`Rail.tsx:389,590,612,839`).
- A fresh `agy` install is visible without restarting Keitora, bounded by discovery's 30-second miss cache
  (`DISCOVER_NONE_TTL`).

### Launch arguments

`build_agent_args_opts` (`agent.rs:53`) gains this arm (exhaustive `match cli`, no wildcard):

```rust
CliKind::Antigravity => {
    if autonomy { args.push("--dangerously-skip-permissions".into()); }
    match resume {
        Some(Some(id)) => args.extend(["--conversation".into(), id.to_string()]),
        Some(None) => args.push("-c".into()),
        None => {}
    }
}
```

| Case | Arguments |
|---|---|
| Autonomy on | `--dangerously-skip-permissions` (first) |
| Resume by id | `--conversation <id>` |
| Continue | `-c` |
| Fresh | none |

There is no update-check flag: `agy` updates itself in the background and showed no blocking prompt. The
`hook_trust` parameter is ignored for Antigravity (as it is for Claude). Update the doc comment above
`build_agent_args` (`:25-34`) with an `agy:` bullet. `next_fallback` (`:102`) gets one Antigravity arm:
`ResumeDecision::Resume { cli: CliKind::Antigravity, .. }` maps to `Fresh`, so a failed resume skips `Continue`
(README Open issue 1 resolution). The Claude and Codex chains are unchanged. An unsafe Antigravity id degrades to
`Fresh` (no arguments), not to `-c`, for the same reason.

### Id safety

Rename `is_safe_claude_session_id` to `is_safe_session_id` everywhere (it already guards every provider's id):

- `crates/keitora-tasks/src/config.rs:81` (definition) and its tests `:6999,7002,7016,7018`
- `crates/keitora-tasks/src/lib.rs:41` (re-export)
- `src-tauri/src/backend/herdr/agent.rs:11` (import) and `:62` (call)
- `src-tauri/src/backend/herdr/strict.rs:22` (import) and `:146` (call)
- `src-tauri/src/backend/herdr/resume.rs:17` (import): delete the private wrapper `fn is_safe_session_id`
  and its doc comment at `:344-348`; the two callers (`:209`, `:269`) now call the imported function of the
  same name
- `src-tauri/src/transfer.rs:2250`

Ids are UUIDs (`[A-Za-z0-9_-]`, first char alphanumeric, at most 128 chars), which the existing predicate
accepts.

### Trust seeding: `src-tauri/src/antigravity_trust.rs`

Keeps the worktree root in the `trustedWorkspaces` array of `~/.gemini/antigravity-cli/settings.json` (path
from `dirs::home_dir()`). Public surface (exact):

```rust
/// Result of the pure merge.
#[derive(Debug, PartialEq)]
pub enum TrustMerge { Unchanged, Updated(serde_json::Value), Skip }

/// `existing`: the parsed file (`None` when the file does not exist).
/// `worktree`: the worktree root as a string. `windows`: compare ignoring ASCII case and treating `/` and `\`
/// as equal.
pub fn merge_trusted_workspace(existing: Option<serde_json::Value>, worktree: &str, windows: bool) -> TrustMerge;

/// `<home>/.gemini/antigravity-cli/settings.json`.
pub fn settings_file(home: &Path) -> PathBuf;

/// Merge `worktree` into `file`; returns whether the file was written. Creates the parent directory and the
/// file when missing. Never writes when the merge says Unchanged or Skip, or when `!allowed`.
pub fn apply_trust_at(file: &Path, worktree: &Path, windows: bool, allowed: bool) -> std::io::Result<bool>;

/// Seed trust for one worktree in the real home; a no-op unless `global_writes::allowed()`.
pub fn install_trust_for(worktree: &Path);
```

`merge_trusted_workspace` rules:

- `existing == None` -> `Updated({"trustedWorkspaces": [worktree]})`.
- `existing` not a JSON object -> `Skip` (never clobber).
- key `trustedWorkspaces` absent -> add `[worktree]`, keep every other key: `Updated`.
- key present but not an array, or an array containing any non-string element -> `Skip`.
- array of strings already containing `worktree` (per the `windows` comparison) -> `Unchanged`.
- otherwise append `worktree` (the path string with `strip_extended_prefix_str` applied, native separators)
  -> `Updated`.

`apply_trust_at` reads the file (a read or JSON parse error on an existing file means: do not write, return
`Ok(false)`), calls the merge, writes with `serde_json::to_string_pretty` plus a trailing newline only on
`Updated`. `install_trust_for` resolves `dirs::home_dir()` and calls `apply_trust_at(…, cfg!(windows),
crate::global_writes::allowed())`, logging at `debug` (changed) / `warn` (I/O error). Callers run it only for
Antigravity worktrees (trigger list in doc 04).

Entries are path strings. The element shape is unverified (facts.md): the smoke test must confirm that an
interactive `agy` in a seeded worktree shows no trust prompt. If objects turn out to be required, the fix
stays inside this module (`merge_trusted_workspace` and its tests).

### Write guard: `src-tauri/src/global_writes.rs` (new, shared by doc 04)

```rust
/// Whether Keitora may write the user's real home (`~/.gemini`): never under `cfg!(test)`, never when
/// `KEITORA_HERDR_NO_INTEGRATION` (supervisor::NO_INTEGRATION_ENV) is set to anything.
pub fn allowed_with(is_test: bool, env_val: Option<String>) -> bool {
    !is_test && crate::backend::herdr::supervisor::integration_enabled(env_val)
}
pub fn allowed() -> bool {
    allowed_with(cfg!(test), std::env::var(crate::backend::herdr::supervisor::NO_INTEGRATION_ENV).ok())
}
```

It mirrors `supervisor.rs:224-239`. This is the first Keitora writer, apart from herdr's own integration, that
targets the real home directory, so every public writer in `antigravity_trust.rs` and `antigravity_hooks.rs`
takes the guard as an explicit `allowed: bool` argument in its `_at` form (so tests can pass `false` and `true`
against a temp dir without touching the real home) and the `install_*` wrappers pass `global_writes::allowed()`.

### Herdr integration gating

`supervisor.rs:243` `for target in ["claude", "codex"]` becomes `for target in integration_targets(agy_found)`
where

```rust
/// herdr integration targets to install: every provider's `herdr_integration`, Antigravity's only when
/// `agy` was found (so users without `agy` never get `~/.gemini/config/hooks/*` written).
pub fn integration_targets(agy_found: bool) -> Vec<&'static str> {
    CliKind::ALL.iter()
        .filter(|c| **c != CliKind::Antigravity || agy_found)
        .map(|c| c.spec().herdr_integration)
        .collect()
}
```

`agy_found = keitora_tasks::discover_cli_cached(CliKind::Antigravity).is_some()`, evaluated inside
`ensure_ready` after the existing `cfg!(test) || !integration_enabled(opt_out)` early return (so tests and
isolated runs never run discovery). Update the warn text `"{target} session ids will not be recorded …"`
unchanged. `ensure_ready` runs once per process. For a late `agy` install (README Open issue 5 resolution),
`switch_worktree_cli` (target Antigravity) and `save_user_settings` (`default_cli = antigravity`) also call the
backend's existing `refresh_integration()` (`herdr/mod.rs:3060`, which reaches `supervisor.rs:268`) after the agy
hook install. The same `cfg!(test)` / `NO_INTEGRATION_ENV` rules apply.
Update the doc comments at `supervisor.rs:36-39,220-223` to say `~/.gemini` as well as `~/.claude` /
`~/.codex`.

### Resume

- **`resume.rs` `cli_of`** (`:62`) becomes
  `CliKind::ALL.into_iter().find(|c| c.spec().herdr_agent_names.contains(&agent))`.
  `observe` (`:193`) keeps accepting only `s.source == format!("herdr:{}", s.agent)` and
  `s.kind == AgentSessionKind::Id`, so the expected record for Antigravity is agent `antigravity_cli`, source
  `herdr:antigravity_cli`. (An agent `agy` with source `herdr:agy` would also be accepted, by design.)
- **`usable`** (`resume.rs:268`): the `match ctx.cli` gains
  `CliKind::Antigravity => antigravity_brain_dir(ctx.antigravity_dir).is_some_and(|d| d.join(id).is_dir())`,
  where `fn antigravity_brain_dir(injected: Option<&Path>) -> Option<PathBuf>` returns `injected` if `Some`,
  else `dirs::home_dir()?.join(".gemini").join("antigravity-cli").join("brain")`. This mirrors Claude's
  transcript-exists check.
- **`ResolveCtx`** (`resume.rs:39`) gains `pub antigravity_dir: Option<&'a Path>` with the doc comment
  "Antigravity's `brain/` dir; `None` -> `~/.gemini/antigravity-cli/brain`". Updated literals: `herdr/mod.rs:805`
  (`antigravity_dir: self.antigravity_dir.as_deref()`), `resume.rs:444` (test helper `ctx`, set
  `Some(&self.antigravity)` where the test `Fixture` (`resume.rs:415`) gains an `antigravity: PathBuf` temp dir field next to
  `projects`), `resume.rs:573`, `resume.rs:585` (`antigravity_dir: None`).
- `HerdrBackend` and the resolver struct each gain `antigravity_dir: Option<PathBuf>` next to `projects_dir`
  (`mod.rs:292`, `:786`), initialised at `:1982` as
  `let antigravity_dir = cfg!(test).then(|| ops.env().home.join("antigravity-brain"));` and passed in the
  literal at `:2019` and the clone at `:2178`, so unit tests never look at the real home.
- **No stored id: `Fresh`.** In `resolve` (`:~296-312`) the final `match ctx.cli` gains
  `CliKind::Antigravity => ResumeDecision::Fresh`. Unlike Codex's `Continue`, this does not use `-c`: `brain/`
  is shared by every workspace and cannot tell whether this cwd has history, and `agy -c` in a directory with
  no history is unverified. The stored id comes from herdr and the registry anyway, so a restart normally
  resumes by id. Update the doc comment above `resolve` (`:247-253`) with an Antigravity clause.
- **Registry id.** `antigravity_conversation_id` takes part in `resolve` through
  `AgentLaunch.resume_target`, set in `agent_launch_with_discovery` above (the same way `codex_thread_id` and
  `claude_session_id` do). Update the `ResolveCtx.registry_id` doc comment (`resume.rs:~44`) to list the third
  field.
- **Fake herdr.** `resume_id_from` (`fake.rs:217`) matches
  `w[0] == "--resume" || w[0] == "resume" || w[0] == "--conversation"`. Add
  `fn antigravity_launch() -> AgentLaunch` (`cli: Antigravity`, `path: "agy"`, `autonomy: false`,
  `resume_target: Some("0b9e6c3e-1f2a-4c5d-8e7f-a1b2c3d4e5f6")`, `fresh: false`, `policy:
  LaunchPolicy::Default`) in `herdr/mod.rs` tests directly after `codex_launch()` (`:8100`). A `claude_launch()`
  exists at `:5570`; no change.

## Tests required

`crates/keitora-tasks/src/config.rs`:

- `agy_unix_candidates_is_home_local_bin` — `Some("/home/u")` gives exactly `["/home/u/.local/bin/agy"]`;
  `None` gives `[]`.
- `agy_windows_candidates_prefers_localappdata_then_home` — with both inputs, order is
  `<lad>/agy/bin/agy.exe` then `<home>/.local/bin/agy.exe`; each `None` drops its entry.
- `agy_candidates_never_name_the_ide_launcher` — no candidate's file name is `antigravity` or
  `antigravity.exe`.
- `is_safe_session_id_*` — the existing four assertions at `:6999-7018` renamed, plus
  `is_safe_session_id("0b9e6c3e-1f2a-4c5d-8e7f-a1b2c3d4e5f6")` is true.

`agent.rs`:

- `build_agent_args_matrix` gains these rows (id `0b9e6c3e-…`): `(Antigravity, true, resume(ag, id),
  ["--dangerously-skip-permissions","--conversation",id])`, `(Antigravity, false, resume(ag, id),
  ["--conversation",id])`, `(Antigravity, true, Continue, ["--dangerously-skip-permissions","-c"])`,
  `(Antigravity, false, Continue, ["-c"])`, `(Antigravity, true, FRESH, ["--dangerously-skip-permissions"])`,
  `(Antigravity, false, FRESH, [])`.
- `antigravity_ignores_hook_trust` — `build_agent_args_opts(Antigravity, false, &FRESH, false) ==
  build_agent_args_opts(Antigravity, false, &FRESH, true) == []`.
- `unsafe_antigravity_id_degrades_to_fresh` — `resume(ag, "a b")` gives `[]`, not `["-c"]`.
- `next_fallback_antigravity_skips_continue` (README Open issue 1) — `next_fallback(&resume(Antigravity, id)) ==
  Some(FRESH)`; the existing `next_fallback_chain` test still holds for Claude.

`antigravity_trust.rs` (pure, temp dirs only):

- `merge_creates_when_missing` — `None` -> `Updated({"trustedWorkspaces":["/wt"]})`.
- `merge_adds_and_keeps_other_keys` — `{"colorScheme":"dark"}` -> `Updated` containing both keys.
- `merge_is_idempotent` — merging `/wt` into a doc that already lists it is `Unchanged`.
- `merge_skips_non_object` — `Some(json!([]))` -> `Skip`.
- `merge_skips_non_array_trusted_workspaces` — `{"trustedWorkspaces":"x"}` -> `Skip`.
- `merge_skips_array_with_objects` — `{"trustedWorkspaces":[{"path":"/a"}]}` -> `Skip`.
- `merge_compares_case_insensitively_on_windows` — with `windows=true`, `C:\Wt` already present makes
  `c:/wt` `Unchanged`; with `windows=false` it is `Updated`.
- `apply_trust_at_writes_once` — against a temp file: first call returns `true` and the file holds the path;
  second returns `false`.
- `apply_trust_at_refuses_when_not_allowed` — `allowed=false` returns `false` and creates no file.
- `apply_trust_at_leaves_unparseable_file_alone` — a file containing `not json` is byte-identical afterwards
  and the call returns `false`.

`global_writes.rs`:

- `allowed_with_refuses_under_test` — `allowed_with(true, None) == false`.
- `allowed_with_refuses_when_marker_set` — `allowed_with(false, Some("1".into())) == false`.
- `allowed_with_permits_otherwise` — `allowed_with(false, None) == true`.
- `allowed_is_false_in_this_test_binary` — `allowed() == false`.

`supervisor.rs`:

- `integration_targets_omits_antigravity_without_agy` — `[ "claude", "codex" ]`.
- `integration_targets_includes_antigravity_with_agy` — `[ "claude", "codex", "antigravity-cli" ]`.
- `refresh_integration_is_skipped_under_test` — the late-install refresh (Open issue 5) does nothing under
  `cfg!(test)` and under `NO_INTEGRATION_ENV`.

`resume.rs`:

- `cli_of_accepts_the_three_antigravity_names` — `antigravity_cli`, `antigravity-cli`, `agy` map to
  `Some(Antigravity)`; `gemini` maps to `None`.
- `observe_records_an_antigravity_session` — pane agent `antigravity_cli`, session source
  `herdr:antigravity_cli`, value a UUID: the row has `cli == Antigravity` and that `session_id`.
- `observe_ignores_antigravity_with_a_foreign_source` — source `herdr:other` is not recorded.
- `resolve_antigravity_store_id_resumes_when_brain_dir_exists` — creating `<antigravity_dir>/<id>/` makes
  `Resume { cli: Antigravity, id }`.
- `resolve_antigravity_store_id_without_brain_dir_is_fresh` — no dir: `Fresh`.
- `resolve_antigravity_registry_id_resumes_when_usable` — registry id with the dir present resumes.
- `resolve_antigravity_without_any_id_is_fresh` — not `Continue`.
- `resolve_antigravity_fresh_flag_clears_the_record` — `fresh: true` returns `Fresh` and clears the row.

`host.rs` (next to `agent_launch_with_discovery_*` tests at `:3805-3839`):

- `agent_launch_with_discovery_uses_discovered_path_for_antigravity_when_unconfigured`.
- `agent_launch_with_discovery_bare_agy_when_discovery_finds_nothing` — `launch.path == "agy"`.
- `agent_launch_with_discovery_antigravity_resume_target_is_the_conversation_id`.

`fake.rs`:

- `resume_id_from_reads_conversation_flag` — `["--dangerously-skip-permissions","--conversation","c1"]` gives
  `Some("c1")`; `["-c"]` gives `None`.

TypeScript (`src/App.tsx` logic is thin; test the pure part): no new file; the availability map shape is
covered by the settings wire test in doc 02.

## Cross-references

- 01 (spec fields used above), 02 (`antigravity_conversation_id`, `available`), 04 (hook install triggers
  also call `install_trust_for`; `KEITORA_BIN`), 06 (`switch_worktree_cli` clears the target id and calls
  trust seeding), 08 (isolation marker and docs).
