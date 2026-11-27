# 0077 — Antigravity CLI (`agy`) as a third agent provider

## Goal

Everywhere Keitora supports `claude` and `codex` as a worktree's agent, it also supports Google's
Antigravity CLI (`agy`). A worktree can be set to Antigravity, and the agent then:

- launches, and resumes after a restart
- shows busy, waiting and idle state
- shows up in the doctor and in agent updates
- has its own settings group

A worktree can also be switched to or from Antigravity at any time. The conversation does not carry over,
and the user is warned about that. On the way, every hard-wired "claude or codex" becomes "one of the
providers", which makes a fourth provider much cheaper.

The verified CLI facts this design rests on are in `plans/0077-antigravity-provider/facts.md`. That file
records `agy` 1.5.18 and herdr 0.9.5, probed on Windows on 2026-11-26.

## Decisions (user, 2026-11-26)

1. **Hook location.** Hooks for Antigravity go in the **global** `~/.gemini/config/hooks.json`, and they only
   act inside Keitora panes.
2. **Waiting state.** herdr's `blocked` agent status **counts as `waiting` for every provider**, Claude and
   Codex included.
3. **Workspace trust.** Keitora **pre-seeds workspace trust** for `agy`, so its first-run trust prompt
   doesn't block a launch.
4. **Transfer and switching.** Conversation transfer to and from Antigravity is out of scope. Switching a
   worktree to or from `agy` **is in scope**: the agent changes, the conversation does not carry over, and
   the user gets a warning before it happens.
5. **Test install.** The user installed `agy` for the spike and the smoke test.
6. **Glyph.** Use the Antigravity mark.
7. **Naming.** The name is "Antigravity" everywhere: config key `[antigravity]`, serde value
   `"antigravity"`, UI label "Antigravity".

## Scope

**In**

- **Provider model.** Add a third `CliKind` variant, plus a per-provider spec table and `CliKind::ALL`,
  replacing the two-way assumptions in Rust and TS.
- **Persistence.**
  - The `[antigravity]` config table, `antigravity_steps` in dispatch, an update-skip flag, and a
    conversation id in the worktree registry.
  - A migration for the `resume_sessions` CHECK constraint.
- **Discovery.** Find `agy` on each OS, and never pick the `antigravity` IDE launcher.
- **Launch through herdr `--kind agy`.** Arguments cover autonomy, resume by id, and continue.
- **Workspace trust.** Pre-seed it in `~/.gemini/antigravity-cli/settings.json`.
- **Resume across restarts.** Use herdr's `antigravity-cli` integration, the resume store, and the fallback
  chain.
- **Session state.**
  - A Keitora-owned global hook in `~/.gemini/config/hooks.json`.
  - A `keitora session` stdin filter so only a fully idle `Stop` counts.
  - herdr `blocked` mapped to `waiting` for all providers.
- **Agent updates and doctor.** Version probe, latest version from the official release manifest,
  install-kind detection, and `agy update`.
- **Switch agent.** A new "Switch to …" worktree action for any provider pair that transfer doesn't
  support. It confirms with a warning and restarts the pane on the new agent with a fresh conversation.
- **UI.**
  - The Antigravity glyph.
  - A default-agent picker with three options.
  - An Antigravity settings group: path, autonomy, dispatch steps and availability.
  - A per-provider availability map.
  - A menu that offers either transfer or switch for each target.
  - A prompt-chrome detector for `agy`'s input box.
- **Dev isolation.** `scripts/dev-herdr.mjs` must never let an isolated run write the real `~/.gemini`.
- **Docs.** README, CONTRIBUTING, build-plan and windows-backend.

**Out**

- **Transfer of the conversation to or from Antigravity.** The follow-up is a summary handoff through the
  existing `inject_handoff_summary` path, and possibly import later over `agy acp`. File a task for it.
- **Gemini CLI as a provider.** It becomes cheap once providers are table-driven, but nobody has asked
  for it.
- **An in-app installer for `agy`.** The doctor shows the official install command instead.
- **Behaviour changes for Claude and Codex**, with one exception: the approved `blocked` → `waiting` rule.
  The existing test suites guard the rest.

## Architecture

### 1. Providers become a table

`CliKind` (`crates/keitora-tasks/src/config.rs:72-78`) gains `Antigravity`, which serializes as
`"antigravity"`.

- **`CliKind::ALL`.** A constant `[CliKind; 3]` in display order: Claude, Codex, Antigravity.
- **`ProviderSpec`.** A static per-provider spec returned by `CliKind::spec()`, in `keitora-tasks` so the
  CLI, the app and the tests share it. It holds:
  - `title` ("Claude" / "Codex" / "Antigravity")
  - `binary` (`claude` / `codex` / `agy`)
  - `herdr_kind` (`claude` / `codex` / `agy`)
  - `herdr_integration` (`claude` / `codex` / `antigravity-cli`)
  - `herdr_agent_names`: the agent names herdr may report in `agent_session`. For Antigravity these are
    `antigravity_cli`, `antigravity-cli` and `agy`.

  Launch arguments stay as code in `build_agent_args_opts`, because they differ in structure, not just in
  strings. `agent_updates::model::CliInfo` stays where it is and gains an Antigravity entry; its title
  comes from the spec.
- **Order.** `CliKind::ALL` is the display order. The agent-update banner today puts Codex first
  (`agentUpdates.ts:18`, pinned by `agentUpdates.test.ts:71-73`). It switches to `CLI_KINDS` order and its
  test is updated. One order everywhere.
- **Code that iterates over providers uses `CliKind::ALL`:**
  - `agent_names_for` (`backend/herdr/agent.rs:174`, today `-> [String; 2]`) returns
    `[String; CliKind::ALL.len()]`. Its callers are `herdr/mod.rs` `evict_key_panes` and
    `keymap.rs:845` `named_agent_pane`.
  - The supervisor integration targets (`backend/herdr/supervisor.rs:243`) come from the spec's
    `herdr_integration`.
  - `AgentUpdates::check` (`agent_updates/mod.rs:316`).
  - The doctor agent rows (`deps/doctor.rs:157`).
- **Matches stay exhaustive** wherever behaviour really differs, so the compiler finds every site.
- **Frontend.**
  - `CliKind` in `src/lib/ipc.ts:23` gains `"antigravity"`.
  - A new `src/lib/providers.ts` exports `CLI_KINDS`, `cliTitle(cli)`, `transferTargets(from)` (only the
    pairs the backend supports: claude→codex and codex→claude) and `switchTargets(from)` (every other
    provider that isn't a transfer target).
  - `providers.ts` also exports a `TRANSFER_PAIRS` constant, the only definition of the supported pairs.
  - Every `x === "codex" ? A : B` goes through these helpers. The current sites are:
    - `ProviderGlyph.tsx:24,28`. This becomes a `Record<CliKind, {label, path}>`.
    - `Rail.tsx:393,416`
    - `agentUpdates.ts:18-43`, including `updatePromptCliOf`. It must accept the backend's
      `AgentUpdatePrompt: <cli>` token for any `CliKind`. The copy at `:85-100` uses `cliTitle`.
    - `SettingsDrawer.tsx:383-385` (raw `{cli}` label) and `:645-654`
    - `App.tsx:660-688`
    - `laneState.ts:77-104`
    - `transferPhase.ts`:
      - `cliLabel` at `:221` is deleted in favour of `cliTitle`.
      - `freshToastMessage` at `:231-237` is hard-coded to Codex.
      - `MID_TURN_NOTE` at `:300` hard-codes "Claude"; it takes the source cli as a parameter.
      - `spawnFailedCopy` at `:308-320` must name the binary (`agy`), not the id. It also matches the
        backend `"{} did not start"` text (`strict.rs:384`). Both sides use the spec's `binary`.
    - `AgentUpdatesProvider.tsx:294-298`, which reads `snapshot.codex` and `snapshot.claude`
    - `DepsSection.tsx:108`, which prints `row.tool` raw; it uses a title helper
    - The `codexDispatchEditor` prop at `SettingsDrawer.tsx:443,458` and `App.tsx:1269` becomes a
      render prop keyed by provider.
- **Wire shapes that are fixed pairs become maps keyed by provider:**
  - `AgentUpdatesSnapshot { codex, claude, … }` becomes `{ agents: Record<CliKind, AgentUpdateEntry>, … }`.
    In Rust that is a `BTreeMap<CliKind, …>`, which serializes to a JSON object keyed by provider.
  - `SettingsUser.codex_available` becomes `available: Record<CliKind, boolean>`.
  - Both are mirrored in TS and covered by wire-shape tests.
- **TOML keeps one table per provider:** `[claude]`, `[codex]`, `[antigravity]`.

### 2. Persistence

- **Config structs.**
  - `UserConfig` and `ProjectConfig` gain `antigravity: ProviderConfig`.
  - `DispatchConfig` gains `antigravity_steps`.
  - `UpdatesConfig` gains `agent_skip_antigravity`.
  - Extend `project_provider`, `user_provider` and the `is_empty` methods (`DispatchConfig::is_empty` at
    `config.rs:184` must include `antigravity_steps`).
  - The per-machine dispatch override, `ProjectRef` / `ProjectSettingsPatch.codex_dispatch_steps`, gains
    an Antigravity counterpart. Six places carry it, all in `config.rs`:
    - the definitions at `:772` and `:808`
    - the constructor at `:790`
    - the patch apply at `:830`
    - the reset on path change at `:911`
    - the `effective_config` merge at `:1882`
  - Commit-settings-to-file (`host.rs:855-890`) must write `[antigravity]`. Otherwise it silently drops the
    table.
  - Update the struct literals in `factory.rs:73` and `migration.rs:256`.
  - `config_edit.rs` today has a copy of every setter for each provider (`apply_*_path`,
    `apply_*_autonomy`, `apply_*_dispatch_steps`, `write_project_settings`, `set_project_*_path`,
    `set_user_*_path`, `set_dispatch_steps_*`). They become generic over `CliKind`. The old names can stay
    as thin wrappers if that keeps the diff small, but new code calls the generic ones.
- **Registry.**
  - `WorktreeEntry` gains `antigravity_conversation_id: Option<String>`. It needs updating at these
    `config.rs` sites:
    - the literals at `:596`, `:608`, `:679` and `:694`
    - the merge at `:714-728`
  - The two-id signatures of `set_worktree_provider_state_by_root` (`config.rs:1025`) and
    `persist_worktree_cli` (`transfer.rs:2461`) become a `ProviderIds` struct with one `FieldIntent` per
    provider. That way a future provider doesn't add positional arguments.
  - The two existing id fields keep their TOML names, for compatibility.
- **Settings IPC.** The project and user settings DTOs (`host.rs:589-639`, `:680-815`) move to shapes
  keyed by provider:
  - `providers: Record<CliKind, {path, autonomy, dispatch_steps}>`
  - `available: Record<CliKind, bool>`

  The commands that change in step:
  - `save_project_settings` (`host.rs:760`) and `save_user_settings` (`host.rs:896`) take a keyed payload
    instead of positional `claude_path` / `codex_path`.
  - `set_skip_permissions` (`host.rs:922`) gets an exhaustive arm.
  - On the TS side, `settings.ts:17-108` changes in the same commit.

  Today's wire names are asymmetric (`claude_path` / `skip_permissions` / `dispatch_steps` against
  `codex_*`). They disappear from IPC but stay as TOML keys.
- **SQLite.**
  - `resume_sessions` has `CHECK(cli IN ('claude','codex'))` (`sqlite_backend.rs:126`), and
    `CREATE TABLE IF NOT EXISTS` never upgrades it.
  - `init_schema` reads `sqlite_master.sql` for the table. If the text lacks `'antigravity'`, it rebuilds
    the table in one transaction: create `resume_sessions_new` with the new CHECK, copy the rows, drop the
    old table, rename the new one.
  - `cli_to_sql` / `cli_from_sql` (`local_store.rs:45-63`) gain the new arm.
  - Tests cover:
    - a DB built with the old DDL
    - rows preserved
    - an `'antigravity'` insert after the upgrade
    - idempotent re-runs
- **Downgrade risk.** An older Keitora fails to parse a config that says `"antigravity"`. We accept this
  and note it in the release notes.

### 3. Discovery, launch, trust

- **Discovery.** `discover_cli` gets an `Antigravity` arm (unix `config.rs:2558`, Windows `:2643`).
  - **Windows** checks, in order:
    1. `%LOCALAPPDATA%\agy\bin\agy.exe` (the official installer's location, and it may not be on PATH yet)
    2. `~\.local\bin\agy.exe`
    3. a PATHEXT search for `agy`
  - **unix** checks `~/.local/bin/agy`, then the login `$PATH`.
  - It looks only for `agy`, never `antigravity`.
  - In `agent_launch_with_discovery` (`host.rs:1450-1461`), the Codex-only `match cli` runs discovery for
    Antigravity as well, and the default exe name (`:1455`) gains `"agy"`.
  - herdr resolves the executable by kind, so the pane PATH must contain `agy`'s directory (`env.rs`
    `pane_env_with`). The installer doesn't update PATH for processes that are already running.
- **Availability refresh.** Availability is fetched once on mount today (`App.tsx:358-361`, with a stale
  comment at `:187-190`). The `available` map is now re-fetched when the worktree menu opens and when the
  settings drawer closes, so a fresh `agy` install shows up without restarting Keitora. Discovery's
  30-second miss cache (`config.rs:2632`) bounds the delay.
- **Launch arguments.** `build_agent_args_opts` (`backend/herdr/agent.rs:72`) gains an Antigravity arm:

  | Case | Arguments |
  |---|---|
  | Autonomy on | `--dangerously-skip-permissions` |
  | Resume by id | `--conversation <id>` |
  | Continue | `-c` |
  | Fresh | none |

  There is no update-check flag: `agy` updates itself in the background and showed no blocking prompt.
  The arg-matrix test (`agent.rs:239`) grows Antigravity cases.
- **Id safety.** `is_safe_claude_session_id` (`config.rs:81`) is renamed to `is_safe_session_id`, because
  every provider already uses it. Ids are UUIDs.
  - Delete the private wrapper of the same name at `resume.rs:346`.
  - Update the re-export at `lib.rs:41` and the imports at `agent.rs:11` and `strict.rs:22`.
- **Trust seeding.** A new `src-tauri/src/antigravity_trust.rs` keeps the worktree root in the
  `trustedWorkspaces` array of `~/.gemini/antigravity-cli/settings.json`. It runs on the same triggers that
  install hooks (startup and project activation), but only for Antigravity worktrees.
  - **Write guard.** The writer refuses under `cfg!(test)` and when `NO_INTEGRATION_ENV`
    (`supervisor.rs:40`, `KEITORA_HERDR_NO_INTEGRATION`) is set, mirroring `supervisor.rs:224-239`. The
    pure merge is what gets tested. This is the first Keitora writer, apart from herdr's integration, that
    targets the real home directory.
  - It follows the hook installers' rules:
    - Pure merge function, plus a thin I/O wrapper.
    - Idempotent, and writes only on a change.
    - Leaves all other keys untouched.
    - Skips the file entirely if it fails to parse, or if `trustedWorkspaces` exists but isn't an array
      of strings.
    - Creates the file if it's missing.
    - Path comparison ignores case on Windows.
  - The entries are path strings. The element shape is unverified (see facts), so the smoke test must
    confirm that an interactive `agy` in a seeded worktree shows no trust prompt. If objects turn out to
    be required, the fix stays inside this one module.

### 4. Resume

- **herdr integration.** The supervisor installs herdr's `antigravity-cli` integration along with
  `claude` and `codex`, but **only when `discover_cli(Antigravity)` finds `agy`**. Users without `agy`
  never get `~/.gemini/config/hooks/*` written.
- **`resume.rs` `cli_of`** maps any of the spec's `herdr_agent_names` to `Antigravity`. `observe` keeps
  accepting only sources of the form `herdr:<agent>`. The expected source is `herdr:antigravity_cli`.
- **`usable`.** An Antigravity id is usable if `~/.gemini/antigravity-cli/brain/<id>/` exists. This mirrors
  Claude's transcript-exists check.
  - `ResolveCtx` (`resume.rs:39`) gains an injectable `antigravity_dir`, so tests don't touch the real home.
  - Its literal at `herdr/mod.rs:809` and the test literals are updated to match.
- **Fallback with no stored id: `Fresh`.** Unlike Codex's `Continue`, this doesn't use `-c`. `brain/` is
  shared by every workspace and can't tell whether this cwd has history. `agy -c` in a directory with no
  history is an unverified behaviour. The stored id comes from herdr and the registry anyway, so a
  restart normally resumes by id.
- **Registry id.** `antigravity_conversation_id` takes part in `resolve` the same way `codex_thread_id` and
  `claude_session_id` do (`host.rs:1450-1461`).
- **Fake herdr.** `resume_id_from` (`backend/herdr/fake.rs:217`) learns `--conversation <id>`.
  `antigravity_launch()` test helpers sit next to `claude_launch()` and `codex_launch()`.

### 5. Session state

There are two layers.

**herdr layer.** herdr's `antigravity-cli` integration hook drives the pane's `agent_status` and captures
session ids. Nothing beyond the integration target is needed.

**Keitora layer.** A new `src-tauri/src/antigravity_hooks.rs`, modelled on `codex_hooks.rs`:

- **File.** It owns the top-level hook named **`keitora`** in the global `~/.gemini/config/hooks.json`.
  Because the file is keyed by hook name, the merge only replaces `keitora` and leaves every other name
  untouched, including herdr's entry and any of the user's.
  - If the file fails to parse, skip it; never clobber it.
  - Write only on a change.
  - **Install triggers.** Today's triggers only scan the cwd or active project (`host.rs:2403`, `:514`,
    `:1134`), which isn't enough here. The Antigravity hook and trust seeding run in these places:
    - At startup, after a **registry-wide** scan for any worktree whose effective cli is Antigravity,
      including the user default.
    - Right after `switch_worktree_cli` persists an Antigravity target.
    - Right after `save_user_settings` sets `default_cli = antigravity`.
    - In `worktree_create` (`host.rs:558-563`), next to the existing Codex gate.
  - **Write guard.** Same as trust seeding: refuse under `cfg!(test)` and `NO_INTEGRATION_ENV`.
- **Guard and binary path.** The pane environment gains `KEITORA_BIN=<resolved keitora binary>`, using the
  same resolution as `resolve_keitora_bin` (`hooks.rs:161`). It is set for every provider; that's harmless.
  - The default launch (`herdr/mod.rs:974`) calls `pane_env`, which passes no extras. It switches to
    `pane_env_with`, and the strict path (`strict.rs:470`) adds the variable too.
  - `keitora_on_path()` runs a login shell, so its result is cached for the life of the process instead of
    being called on every spawn. The hook command is a no-op
  unless the variable is set, so `agy` outside Keitora is unaffected. This also avoids baking one binary
  path into a global file shared by every worktree.
  - **Windows (cmd.exe):**
    `(if defined KEITORA_BIN "%KEITORA_BIN%" session <state> >nul 2>&1) & exit /b 0`
  - **POSIX:**
    `[ -n "$KEITORA_BIN" ] && "$KEITORA_BIN" session <state> >/dev/null 2>&1 || true`
  - The hook prints nothing on stdout, so `agy` treats it as having no decision. That was verified:
    a silent hook lets `Stop` finish normally.
- **Event map.** The handler shape follows facts.md: non-tool events take a direct handler array.

  | agy event | `keitora session …` | Note |
  |---|---|---|
  | `SessionStart` | `idle --stdin-payload` | |
  | `PreInvocation` | `busy --stdin-payload` | Fires on every model call; idempotent. It is the closest event to prompt submit. |
  | `Stop` | `idle --stdin-payload --if-fully-idle` | Ignored unless the payload has `fullyIdle: true`. |
  | pane gone | offline | Already handled by `apply_reconcile`. |

- **`keitora session <state> --stdin-payload [--if-fully-idle]`** is new logic in `crates/keitora-cli`.
  - `keitora session` today resolves the project and worktree from the process cwd (`resolve()` →
    `current_worktree`, `config.rs:2944`). Nothing guarantees the hook's working directory.
  - With `--stdin-payload`, the CLI:
    1. reads stdin up to a 1 MiB cap
    2. parses the JSON
    3. takes the verified `cwd` field, falling back to `workspacePaths[0]`
    4. changes into that directory before `resolve()`
  - `--if-fully-idle` additionally requires `fullyIdle == true`.
  - Missing or invalid JSON, or a missing directory, means a no-op with exit 0, because a hook must never
    break the agent.
  - Stdin is read only under the flag. The pure payload interpretation is a function, unit-tested on its
    own.
- **Waiting, for all providers (decision 2).** Today `apply_reconcile` (`backend/mod.rs:792-820`) receives
  only `bg: &HashSet<SessionKey>`. `busy_keys()` (`:825-841`) builds that set from
  `state_hint == HINT_BUSY`, and `agent_status_to_state_hint` (`agent.rs:152-160`) maps both `working`
  and `blocked` to busy.
  - Add a `HINT_WAITING` constant. `agent_status_to_state_hint` maps `blocked` → `HINT_WAITING`.
  - `bg` becomes `HashMap<SessionKey, Hint>`, built by a renamed `agent_hints()`.
  - `apply_reconcile` applies, in order:
    1. no live pane → `offline` (wins)
    2. hint `waiting` → `waiting` (overrides both `idle` and `busy`)
    3. hint `busy` → lift `idle` to `busy`
  - The callers to update:
    - `tasks.rs:136,162`
    - `dispatch.rs:321`: dispatch must treat `waiting` as not idle, as it treats busy today.
    - `herdr/mod.rs:3429`
  - The working dot in `activity/mod.rs:206-210` counts `HINT_BUSY` **or** `HINT_WAITING`. A blocked lane
    keeps its working dot as today; the colour of the block comes from the session state.
  - This gives Antigravity a needs-input state without a permission event. For Claude and Codex it
    backfills `waiting` when a hook was missed.
  - Covered by a reconcile table test for every hook-state × hint combination.
- **`.gitignore`.** Unchanged. Nothing is written into worktrees.

### 6. Agent updates and doctor

- **Version.** `agy --version` prints a bare `1.5.18`. Reuse `parse_claude_version_output` (first
  `N.N.N`), renamed to `parse_first_semver`, with an `agy` fixture.
- **Latest version.** `latest_with` (`probe.rs:40-56`) learns a source kind called `ReleaseManifest`. It
  fetches `<manifest base>/manifests/<platform>.json` and reads `version`.
  - The platform key comes from `cfg!(target_os)` and `target_arch`: `windows_amd64`, `windows_arm64`,
    `darwin_amd64`, `darwin_arm64`, `linux_amd64` or `linux_arm64`. musl is not detected, because the glibc
    key gives the same version.
  - A pure `parse_manifest_latest` function, with tests.
- **Version cache.** `installed` and `invalidate` (`probe.rs:115-139`) gain an `agy` version cache, like
  `CLAUDE_VERSION_CACHE`. The manifest fetch reuses `npm_fetch`'s HTTP plumbing.
- **Install kind.**
  - `Native` for `%LOCALAPPDATA%\agy\bin\agy.exe` and `~/.local/bin/agy`. `CliInfo.native_dir` is one
    home-relative string today, so it becomes chosen by `cfg!`: `AppData/Local/agy/bin` on Windows,
    `.local/bin` on unix.
  - The native update runs `agy update`, so `native_update_runs = true`. That's safe because it takes no
    flags and is non-interactive.
  - There is no npm or brew entry. `Unknown` falls back to guidance: the official install one-liner for the
    OS.
- **Startup update screen.** None was seen, so no detector. If the smoke test finds one, add a detector
  next to `is_codex_update_screen`.
- **Doctor.** An `antigravity` row (the third agent row, fifth overall), generated from `CliKind::ALL`.
  `service_tests.rs:1478` and its `[handle; 4]` array are updated to match. `DoctorRow.tool` and its
  TS mirror gain the value. For a missing agent, the row gains an install hint, the official one-liner for
  the OS (only for Antigravity, to keep scope small).
- **Config.** `agent_skip_antigravity`, wired through `commands.rs:51-54`.

### 7. Switching agent (decision 4)

- **Transfer stays restricted to its two pairs.** `transfer_worktree` (`transfer.rs:430`) today matches
  only on the target, so an Antigravity source with a Codex target would run the Claude→Codex import on
  the wrong transcript.
  - Add an entry guard: only (Claude→Codex) and (Codex→Claude). Anything else returns an explicit
    "transfer isn't supported between X and Y; use Switch" error.
  - `preview_claude_transfer` (`host.rs:2535`) gets the same source guard.
  - Both are tested.
- **Backend.** A new `switch_worktree_cli(project, worktree, target: CliKind)` command in a new
  `src-tauri/src/agent_switch.rs`. It returns a `TransferOutcome::Switched { from, to }` variant, so the
  frontend's existing outcome, restart, `transfer_settled` and `worktrees://changed` path is reused.
  There is no new event. What the command does:
  - **Guards.** It refuses when:
    - a transfer or switch is running. It uses `transfer_registry` `begin`/`mark_committed`/`settle`, so
      dispatch's refusal (`dispatch.rs:631`) also covers switches; the refusal text says "transfer or
      switch".
    - the lane is terminated (`refuse_terminated`).
    - `target` is the current cli.
    - `target` isn't available.
    - the pair is a transfer pair. The UI never offers that, but the backend enforces it.
  - **Shared helpers.** `with_recovery_lock` and `refuse_terminated` become `pub(crate)` in
    `transfer.rs`. Stopping the live pane uses the generic `release_codex_session_with`, renamed
    `release_session_with`.
  - **Persist.** It persists under the dispatch gate (`persist_under_gate`) with:
    - `cli = target`
    - `ProviderIds` with `FieldIntent::Clear` for the **target's** id, and the source's id kept
    - `ResumeRecord::Cleared` for the target, as the failed-import path does (`transfer.rs:542`)

    Clearing stops a stale id from before an earlier switch from being resumed on the next boot.
  - **Fresh launch.** "Fresh" is already a per-spawn argument: `pty_spawn(fresh)` → `AgentLaunch.fresh` →
    `ResumeStore::resolve` returns `Fresh`. The frontend restarts with `fresh = true`, so switching back
    to Claude doesn't `Continue`. No new backend flag is needed.
  - **Hooks and trust.** After a switch to Antigravity, it runs the agy hook install and trust seeding
    (§5).
- **Frontend menu model.** `WorktreeMenuItem` is a flat string union today, keyed by `kind`
  (`Rail.tsx:401-418`), and `laneMenuAction(item, transferTo)` carries one target. Two Switch items would
  collide.
  - Items become `{ kind: "transfer" | "switch" | …, to?: CliKind }`, with React key `${kind}:${to}`.
  - `laneMenuAction` returns `{ kind: "switch", to }` directly.
  - `worktreeMenuItems` (`laneState.ts`) offers "Transfer to X" for each of `transferTargets(cli)` and
    "Switch to X" for each of `switchTargets(cli)`:
    - From Claude: Transfer to Codex, Switch to Antigravity.
    - From Codex: Transfer to Claude, Switch to Antigravity.
    - From Antigravity: Switch to Claude, Switch to Codex.
  - Terminated lanes only offer launch items, as today (`laneState.ts:67-69`). They can't be switched.
  - `transferDisabledReason({codexMissing, busy})` becomes `actionDisabledReason(action, to, available,
    busy)`.
    - The busy copy names the action.
    - Every target, Claude included, is gated by `available[to]`. Today's "Claude is never unavailable"
      special case goes away; `available.claude` comes from the same discovery.
- **Pending actions.** `PendingAction` (`pendingIntent.ts:12`) gains `{kind:"switch", to}`.
  `runPendingAction` (`App.tsx:783`) gains the case plus an exhaustive `never` check, so lanes in
  non-active projects don't drop it silently.
- **`onSwitch`** sits next to `onTransfer` (`App.tsx:643`) and shares only the focus, key and guard
  prelude. It opens a `ConfirmDialog`, not `danger`, because nothing is destroyed. The warning text:
  "Switch to Antigravity? Antigravity will start a new conversation. The current Claude conversation
  won't be carried over; it stays on disk, and switching back later starts fresh too."
- **Reducer.** `transferPhase.ts` gains:
  - a `begin {mode: "switch"}` that enters `restarting` directly, with no `importing`, so the 120-second
    backstop applies
  - the `switched` outcome, which gives effect `restart {fresh: true}` and a "Switched to X" toast
  - `cancel_requested` is ignored for a switch, and switches get no Codex diagnostics
- **Overlay.** `TransferOverlay` gains a mode. It shows "Switching to X…" with `aria-label="Switching"`
  instead of the import copy (`TransferOverlay.tsx:49-61`).
- **Strict launch on restart.** The restart passes `transfer: true` (`App.tsx:600`) so the pane skips its
  confirm-kill prompt. The backend's `launch_for_spawn` (`host.rs:1372`) applies strict launch only to
  Codex. Its "codex only" log line is demoted to debug, so switches don't produce noise.
- **Tests.**
  - Rust:
    - guard refusals: busy, terminated, same cli, unavailable, transfer pair
    - persistence plan: cli, target id cleared, source id kept
    - the transfer pair guard
  - TS:
    - menu items and disabled reasons for all three providers
    - reducer: switch begin, outcome and backstop
    - confirm copy
    - the `runPendingAction` switch case

### 8. UI

- **`ProviderGlyph`.** A third SVG path, the Antigravity mark, in single-colour `currentColor` with
  `viewBox="0 0 24 24"` like the existing two, and the label "Antigravity".
  - The existing marks come from Simple Icons. If Simple Icons has no Antigravity mark, draw a simplified
    single-path version of the mark and record its source in a comment.
- **Settings.**
  - The default-agent buttons come from `CLI_KINDS`. An unavailable provider is shown but disabled, with
    a "not found" title.
  - The three provider groups render from one `ProviderSettingsGroup` component. It replaces the
    duplicated Claude and Codex blocks (`SettingsDrawer.tsx:1025-1196`). State is keyed by `CliKind`
    rather than `projClaude`/`projCodex`/… It must keep these current behaviours:
    - The "not found" note and disabled inputs when `available[cli]` is false. Claude now gets this too.
    - Autonomy is saved immediately via `setSkipPermissions`, outside the Save buttons. `skipRestart`
      stays shared and unseeded.
    - The project and user sections seed separately. `refresh({project|user})` doesn't discard the other
      section's unsaved edits.
    - The project path override is disabled without `has_committed_file`.
    - The dispatch-steps editor is a render prop keyed by provider.
  - The "agent cli updates" list uses `cliTitle` labels.
  - `.harness/app.tsx` (untracked mock harness) needs `codex_available` → `available` and the new
    snapshot shape. Keep it working for UI screenshots.
- **`promptChrome.ts`.** An `agy` input-box detector, built from a screen sample captured in the smoke
  test. If no sample exists by implementation time, ship without it. Copying still works; the chrome
  just isn't stripped. Record the gap.

### 9. Dev isolation

- Running `herdr integration install antigravity-cli` and Keitora's agy hook and trust seeding all write
  under the real `~/.gemini`. `agy` has no config-dir override.
- `scripts/dev-herdr.mjs` isolates herdr and `CLAUDE_CONFIG_DIR` but not `HOME`. It already sets
  `KEITORA_HERDR_NO_INTEGRATION=1` (`dev-herdr.mjs:343`), and `dev-herdr.test.ts:267-269` asserts it.
- Keitora's agy hook and trust writers honour that same marker and `cfg!(test)` (§3, §5). There is no
  second marker. Each writer has a guard test showing it refuses under the marker.
- Update the comment at `dev-herdr.mjs:341-342` and the test title to name `~/.gemini` along with
  `~/.claude` and `~/.codex`.
- No dev-isolated run happens on the user's machine without verifying this first.

### 10. Docs

- `README.md:15-19,37-41,109,119`: supported agents and install.
- `CONTRIBUTING.md:38`: the "real `~/.claude` or `~/.codex`" sentence gains `~/.gemini`.
- `docs/windows-backend.md:22,64`
- `docs/build-plan.md`: a phase entry for this work, plus the agent CLI updates section (`:535-557`).
- `.gitignore` is unchanged; nothing is written into worktrees.

## Implementation outline (refined in Sequencing)

1. Provider spec, the `CliKind::Antigravity` variant and the compiler-driven arms. TS union plus
   `providers.ts`. The ternaries are routed through the helpers, with no behaviour change.
2. Persistence: config fields, generic `config_edit`, registry id, SQLite CHECK migration.
3. Discovery, launch arguments, trust seeding, resume (integration target, `cli_of`, `usable`, fake herdr).
4. Session state: `antigravity_hooks.rs`, `KEITORA_BIN` pane env, `--if-fully-idle`, `blocked` → `waiting`.
5. Agent updates and doctor: provider-keyed snapshot, manifest source, install kind, doctor row.
6. Agent switching: backend command and menu.
7. UI: glyph, settings group and availability map, prompt chrome.
8. Dev isolation, docs, follow-up task for transfer.

## Dependencies

- `agy` ≥ 1.5 is installed on the dev machine (done).
- herdr ≥ 0.9.5 for `--kind agy` and `antigravity-cli`. That is already the floor.

## Risks

- **Unverified facts.** These are the `trustedWorkspaces` element shape, the herdr session source name,
  and interactive hook trust. Each one sits behind a single module or a tolerant matcher, and the smoke
  test checks each.
- **Shared global files** (`~/.gemini/config/hooks.json` and `settings.json`). The safeguards:
  - The merge is keyed by name.
  - Unparseable files are skipped.
  - Only changed files are written.
  - The `KEITORA_BIN` guard means `agy` outside Keitora is unaffected.
  - Isolated dev runs skip the writes.
- **`blocked` → `waiting` changes Claude and Codex lanes.** This was approved. Reconcile tests pin the full
  table.
- **Downgrade breakage on `"antigravity"`.** Accepted, and documented in the release notes.
- **A fast-moving CLI.** Arguments and facts sit in one table and one test matrix.
- **Regressions in the generalization.** Claude and Codex behaviour is guarded by the existing suites:
  transfer, the herdr argument matrix, resume and the hook installers.

## Complexity

**Large.** The change cuts across about 65 files: the `keitora-tasks`, `keitora-cli` and `src-tauri`
crates, the frontend, a DB migration, two new global-file mergers, a new switch command and docs. No
stages are skipped.
