# 02 — Persistence

## Goal

Make config, the worktree registry, the settings IPC and SQLite carry a third provider without positional
arguments, and migrate the `resume_sessions` CHECK constraint so `'antigravity'` rows can be stored.

## Context

- `CliKind::Antigravity` (serde `"antigravity"`), `CliKind::ALL`, `CLI_COUNT`, `ProviderSpec` come from doc 01.
  TOML keeps one table per provider: `[claude]`, `[codex]`, `[antigravity]`.
- Decision 7: naming is "Antigravity" everywhere.
- Out of scope: behaviour changes for Claude and Codex.
- This is step 2 of the implementation outline.
- Verified code state (before this work), all in `crates/keitora-tasks/src` unless noted:
  - `ProviderConfig` `config.rs:102-130` (`path`, `autonomy` with `alias = "skip_permissions"`).
  - `DispatchConfig` `config.rs:168-185` with `claude_steps` (`alias = "steps"`) and `codex_steps`;
    `is_empty` at `:183`.
  - `UserConfig` `config.rs:234-` has `claude`, `codex` (`:250`), `default_cli`; `UpdatesConfig` `:275-`
    has `agent_skip_codex` (`:295`), `agent_skip_claude` (`:299`); `UpdatesConfig::is_empty` `:310-315`.
  - `ProjectConfig` `config.rs:1272-` has `claude`, `codex` (`:1291`), `dispatch`.
  - `WorktreeEntry` `config.rs:550`, wire enum `WorktreeEntryRepr` `:~618-640`, `Serialize` `:~643-665`,
    `Deserialize` `:~667-700`, `merge_worktree_registry_fields` `:~705-735`, `FieldIntent` `:573`.
  - `ProjectRef` `config.rs:746` with `dispatch_steps` (Claude) and `codex_dispatch_steps` (`:772`);
    `ProjectRef::new` `:~783-795`; `ProjectSettingsPatch` `:802-`; `apply_to` `:~812-840`; the
    path-change reset in `UserConfig::register` `:~895-915`; `effective_config` `:1847` with the merge at
    `:1882`.
  - `set_worktree_provider_state_by_root` `config.rs:1021`.
  - `project_provider` / `user_provider` `config.rs:2014` / `:2022`.
  - `config_edit.rs`: `apply_provider_path(doc, table, path)` `:102`, `apply_provider_autonomy` `:150`,
    `apply_dispatch_steps_key(doc, key, steps)` `:196` (already generic by key), per-CLI wrappers at
    `:125-135`, `:182-192`, `:247-257`, `write_project_settings` `:264`, `set_project_claude_path` `:325`,
    `set_project_codex_path` `:335`, `set_dispatch_steps` `:345`, `set_dispatch_steps_codex` `:353`,
    `set_user_claude_path(_at)` `:370/:378`, `set_user_codex_path(_at)` `:384/:391`, `edit_user_doc` `:397`.
  - `sqlite_backend.rs` `init_schema` `:67-170`; `resume_sessions` DDL `:123-133` (CHECK at `:126`).
  - `local_store.rs` `cli_to_sql` / `cli_from_sql` `:45-63`.
  - `src-tauri/src/host.rs`: `SettingsProject` `:582-610`, `SettingsUser` `:621-648`, `get_settings`
    `:~660-705`, `build_settings_patch` `:~712-748`, `save_project_settings` `:760`,
    `commit_settings_to_project` `:~855-892`, `save_user_settings` `:896`, `set_skip_permissions` `:922`.
  - `src-tauri/src/transfer.rs`: `PersistPlan` `:1794`, `persist_worktree_cli` `:2461` (callers `:822`, `:922`).

## Files to touch

- `crates/keitora-tasks/src/config.rs`
- `crates/keitora-tasks/src/config_edit.rs`
- `crates/keitora-tasks/src/lib.rs` (re-exports)
- `crates/keitora-tasks/src/factory.rs` (`:73-74` literal), `crates/keitora-tasks/src/migration.rs` (`:256-257`
  literal)
- `crates/keitora-tasks/src/sqlite_backend.rs`, `crates/keitora-tasks/src/local_store.rs`
- `src-tauri/src/host.rs` (DTOs, `get_settings`, `build_settings_patch`, `save_project_settings`,
  `commit_settings_to_project`, `save_user_settings`, `set_skip_permissions`, literal at `:3199`)
- `src-tauri/src/transfer.rs` (`persist_worktree_cli`, `PersistPlan` use sites `:822`, `:922`, tests)
- `src-tauri/src/dispatch.rs` (`select_ritual_steps` `:236` arm; `DispatchConfig` test literals at
  `:934,956,974,996,1014,1026,1048` gain `antigravity_steps: vec![]`)
- `src/lib/settings.ts` (`:17-108`), `src/lib/ipc.ts` only if it mirrors settings types (it does not today)
- Struct-literal updates the compiler finds in tests: `config.rs:5078,5115,5154,5199,5237,6218,7065`,
  `host.rs:3199` — each `UserConfig`/`ProjectConfig` literal gains `antigravity: ProviderConfig::default()`.
  The non-test literal at `config.rs:1597` (project scaffold) gains the same.

## Design

### Config structs

1. `UserConfig` (after `codex`, `config.rs:250`) and `ProjectConfig` (after `codex`, `:1291`) each gain

   ```rust
   #[serde(default, skip_serializing_if = "ProviderConfig::is_empty")]
   pub antigravity: ProviderConfig,
   ```

2. `DispatchConfig` gains `#[serde(default)] pub antigravity_steps: Vec<String>`; `DispatchConfig::is_empty`
   (`:184`) becomes `self.claude_steps.is_empty() && self.codex_steps.is_empty() &&
   self.antigravity_steps.is_empty()`.
3. `UpdatesConfig` gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub agent_skip_antigravity:
   Option<String>`; `UpdatesConfig::is_empty` adds `&& self.agent_skip_antigravity.is_none()`. The skip
   persist/read sites are in doc 05.
4. `project_provider` / `user_provider` (`:2014`, `:2022`) gain `CliKind::Antigravity => &project.antigravity` /
   `&user.antigravity`. `resolve_provider_path` and `resolve_autonomy` need no change (they go through
   those two).
5. `dispatch.rs:236 select_ritual_steps`: add `CliKind::Antigravity => &cfg.antigravity_steps`. An empty list
   falls back to the built-in prompt, as for the other providers.
6. Per-machine dispatch override. Add the field `antigravity_dispatch_steps` (the Claude one is named
   `dispatch_steps`, Codex `codex_dispatch_steps`; do not rename those). Exactly six edits, all in `config.rs`:
   - `ProjectRef` (`:772`): `#[serde(default, skip_serializing_if = "Option::is_none")] pub
     antigravity_dispatch_steps: Option<Vec<String>>,`
   - `ProjectRef::new` (`:790`): `antigravity_dispatch_steps: None,`
   - `ProjectSettingsPatch` (`:808`): `pub antigravity_dispatch_steps: Option<Option<Vec<String>>>,`
   - `ProjectSettingsPatch::apply_to` (`:830`): `if let Some(v) = &self.antigravity_dispatch_steps {
     pref.antigravity_dispatch_steps = v.clone(); }`
   - `UserConfig::register` reset on path change (`:911`): `entry.antigravity_dispatch_steps = None;`
   - `effective_config` (`:1882`): `if let Some(steps) = &pref.antigravity_dispatch_steps {
     cfg.dispatch.antigravity_steps = steps.clone(); }`
7. `ProjectSettingsPatch` derives `Default`, so existing patch literals that use `..Default::default()` need no
   change; literals that list every field (grep `ProjectSettingsPatch {` in tests) gain
   `antigravity_dispatch_steps: None`.

### `config_edit.rs`: generic over `CliKind`

Add two private helpers (exhaustive matches, no wildcard):

```rust
fn table_of(cli: CliKind) -> &'static str   // Claude "claude", Codex "codex", Antigravity "antigravity"
fn steps_key_of(cli: CliKind) -> &'static str // "claude_steps" | "codex_steps" | "antigravity_steps"
```

`provider_autonomy_legacy_alias` and `dispatch_steps_legacy_alias` stay (Antigravity has no legacy alias, so
their `_ => None` arms already apply). Then:

- New public generic functions (all re-exported in `lib.rs`):
  - `pub fn set_project_provider_path(project_root: &Path, cli: CliKind, path: Option<String>) -> Result<()>`
  - `pub fn set_user_provider_path(cli: CliKind, path: Option<String>) -> Result<()>` (takes
    `UserConfig::write_lock()` exactly like `set_user_claude_path` does at `:370`)
  - `pub fn set_user_provider_path_at(file: &Path, cli: CliKind, path: Option<String>) -> Result<()>`
  - `pub fn set_project_dispatch_steps(project_root: &Path, cli: CliKind, steps: &[String]) -> Result<()>`
- The old public names stay as one-line wrappers that call the generic ones, so the existing tests keep
  passing: `set_project_claude_path`, `set_project_codex_path`, `set_user_claude_path(_at)`,
  `set_user_codex_path(_at)`, `set_dispatch_steps` (Claude), `set_dispatch_steps_codex`. New code (host.rs)
  calls only the generic ones. Do not add Antigravity-named wrappers.
- The private `apply_claude_path` / `apply_codex_path` / `apply_claude_autonomy` / `apply_codex_autonomy` /
  `apply_claude_dispatch_steps` / `apply_codex_dispatch_steps` are deleted; their callers use
  `apply_provider_path(doc, table_of(cli), …)`, `apply_provider_autonomy(doc, table_of(cli), …)` and
  `apply_dispatch_steps_key(doc, steps_key_of(cli), …)` directly. `apply_dispatch_steps_key` already collapses
  `[dispatch]` only when the table is empty, so it needs no change for a third key.
- `write_project_settings` changes signature (its only callers are `host.rs:~812` and `host.rs:~880`, plus the
  tests at `config_edit.rs:990-1160`):

  ```rust
  /// What one provider contributes to a committed-file write.
  #[derive(Debug, Clone, Default)]
  pub struct ProviderWrite {
      pub path: Option<String>,
      pub autonomy: Option<bool>,
      pub dispatch_steps: Vec<String>,
  }

  pub fn write_project_settings(
      project_root: &Path,
      archive_days: i64,
      log_retention_days: i64,
      worktrees_dir: Option<String>,
      providers: &BTreeMap<CliKind, ProviderWrite>,
  ) -> Result<()>
  ```

  It validates days first (unchanged), then applies path, autonomy and dispatch steps for each
  `cli in CliKind::ALL` using `providers.get(&cli)` (a missing entry behaves as `ProviderWrite::default()`:
  path/autonomy removed, steps cleared). Re-export `ProviderWrite` in `lib.rs`.

### Registry

1. `WorktreeEntry` (`config.rs:550`) gains `pub antigravity_conversation_id: Option<String>` (doc comment:
   "Last Antigravity conversation to resume on restart. Registry-only."). Seven edit sites, all in
   `config.rs`:
   - `WorktreeEntry::name_only` (`:596`) and `WorktreeEntry::new` (`:608`): `antigravity_conversation_id: None`
   - `WorktreeEntryRepr::Table` (`:~632-634`): add `#[serde(default)] antigravity_conversation_id:
     Option<String>,`
   - `Serialize for WorktreeEntry` (`:~650-659`): add `|| self.antigravity_conversation_id.is_some()` to the
     table-form condition and the field to the `Table { … }` literal
   - `Deserialize for WorktreeEntry` (`:679` and `:694`): both the `Name` arm (`None`) and the `Table` arm
     (pass through)
   - `merge_worktree_registry_fields` (`:714-728`): add `|| merged.antigravity_conversation_id.is_none()` to the
     outer condition and
     `if merged.antigravity_conversation_id.is_none() { merged.antigravity_conversation_id =
     old.antigravity_conversation_id.clone(); }`
   The two existing id fields keep the TOML names `codex_thread_id` and `claude_session_id`. The new field's
   TOML name is `antigravity_conversation_id`.
2. New in `config.rs`, next to `FieldIntent`:

   ```rust
   /// What a registry write does to each provider's stored conversation id.
   #[derive(Debug, Clone, PartialEq, Eq)]
   pub struct ProviderIds {
       pub claude: FieldIntent<String>,
       pub codex: FieldIntent<String>,
       pub antigravity: FieldIntent<String>,
   }
   impl ProviderIds {
       /// Every id kept.
       pub fn keep() -> Self;
       /// `self` with `cli`'s intent replaced.
       pub fn with(self, cli: CliKind, intent: FieldIntent<String>) -> Self;
       pub fn get(&self, cli: CliKind) -> &FieldIntent<String>;
   }
   ```

   Re-export it from `lib.rs`.
3. `set_worktree_provider_state_by_root` (`config.rs:1021`) becomes
   `(&mut self, root: &Path, worktree: &str, cli: CliKind, ids: ProviderIds, current: &[WorktreeEntry])`,
   applying `ids.codex` to `codex_thread_id`, `ids.claude` to `claude_session_id`, `ids.antigravity` to
   `antigravity_conversation_id`. Its 9 call sites in tests (`config.rs:3337,3360,3380,3402,3423,3443,3463,3482,
   3505`) change to `ProviderIds::keep().with(CliKind::Codex, FieldIntent::Set(…))` style.
4. `persist_worktree_cli` (`transfer.rs:2461`) becomes
   `(project: &ProjectConfig, worktree: &str, cli: CliKind, ids: ProviderIds) -> Result<(), String>`.
   `PersistPlan` (`transfer.rs:1794`) keeps its two fields (it describes only the Claude<->Codex flows). The
   call at `:822` passes `ProviderIds { claude: plan.claude_session_id.clone(), codex:
   plan.codex_thread_id.clone(), antigravity: FieldIntent::Keep }`; the call at `:922` passes
   `ProviderIds::keep()`. Tests at `transfer.rs:6583,6603,6634` are updated.

### Settings IPC (`src-tauri/src/host.rs`, `src/lib/settings.ts`)

Both DTOs change in the same commit as the TS mirror.

Rust DTOs (all `#[derive(serde::Serialize)]`):

```rust
pub struct SettingsProjectProvider { pub path: Option<String>, pub autonomy: bool, pub dispatch_steps: Vec<String> }
pub struct SettingsUserProvider    { pub path: Option<String>, pub autonomy: bool }

pub struct SettingsProject { name, root, db_path, worktrees_dir, archive_days, log_retention_days,
    providers: BTreeMap<CliKind, SettingsProjectProvider>,   // replaces claude_path, skip_permissions,
                                                             // dispatch_steps, codex_path, codex_autonomy,
                                                             // codex_dispatch_steps
    diff_base, worktrees, has_committed_file }

pub struct SettingsUser { active, projects,
    providers: BTreeMap<CliKind, SettingsUserProvider>,      // replaces claude_path, skip_permissions,
                                                             // codex_path, codex_autonomy
    default_cli,
    available: BTreeMap<CliKind, bool>,                      // replaces codex_available
    auto_check_updates, agent_update_check_hours }
```

(The user level has no dispatch steps today, so `SettingsUserProvider` has no `dispatch_steps`; see Open
issue 2 in the README.) `get_settings` fills every `cli in CliKind::ALL`:
`path = project_provider(pc, cli).path.clone()`, `autonomy = …autonomy.unwrap_or(false)`,
`dispatch_steps` from `pc.dispatch` by provider (Claude `claude_steps`, Codex `codex_steps`, Antigravity
`antigravity_steps`); user side from `user_cfg`. `user_cfg.skip_permissions()` is no longer used for the DTO
(it reads Claude autonomy; keep the method, other callers exist). `available` is filled with a new helper
`fn cli_availability(user: &UserConfig, project: Option<&ProjectConfig>) -> BTreeMap<CliKind, bool>` where each
value is `resolve_provider_path(cli, project, user).is_some() || discover_cli_cached(cli).is_some()` (not the
uncached `discover_cli` that `get_settings` calls today: the cached one gives the 30-second miss cache the
availability refresh in doc 03 relies on; `get_settings` is a `#[tauri::command(async)]`, so the first login
shell never runs on the setup thread).

Commands:

- `save_project_settings` (`host.rs:760`) replaces `claude_path`, `codex_path`, `dispatch_steps`,
  `codex_dispatch_steps` with one argument `providers: BTreeMap<CliKind, ProjectProviderInput>` where
  `ProjectProviderInput { path: Option<String>, dispatch_steps: Vec<String> }` is
  `#[derive(serde::Deserialize)]`. It keeps `archive_days`, `log_retention_days`, `worktrees_dir`,
  `diff_base`. The `#[allow(clippy::too_many_arguments)]` can go.
  - `build_settings_patch` (`host.rs:~712`) becomes
    `(archive_days, log_retention_days, worktrees_dir, steps: &BTreeMap<CliKind, Vec<String>>, diff_base)`;
    it sets `dispatch_steps` (Claude), `codex_dispatch_steps` and `antigravity_dispatch_steps` each to
    `Some(Some(clean_dispatch_steps(steps.get(&cli).map(Vec::as_slice).unwrap_or(&[]))))`, i.e. explicit-empty
    clears semantics are preserved for all three.
  - In the registry branch, when `pc.root.join("keitora.toml").exists()` it calls
    `set_project_provider_path(&pc.root, cli, providers[cli].path.clone())` for each `cli in CliKind::ALL`.
  - In the unregistered-project branch it calls the new `write_project_settings(…, &writes)` where
    `writes[cli] = ProviderWrite { path: providers[cli].path.clone(), autonomy: project_provider(&pc, cli)
    .autonomy, dispatch_steps: providers[cli].dispatch_steps.clone() }`.
- `commit_settings_to_project` (`host.rs:~855-892`): the existing-file branch builds `writes` from `pc` for
  every provider (path, autonomy, dispatch steps) and calls `write_project_settings`; the new-file branch sets
  `seed.antigravity.path = pc.antigravity.path.clone()` and
  `seed.dispatch.antigravity_steps = pc.dispatch.antigravity_steps.clone()` alongside the Claude and Codex
  lines. (Without this the `[antigravity]` table is silently dropped.)
- `save_user_settings` (`host.rs:896`) takes `providers: BTreeMap<CliKind, Option<String>>` (path per
  provider) and `default_cli`; it calls `set_user_provider_path(cli, providers.get(&cli).cloned().flatten())`
  for each `cli in CliKind::ALL`, then writes `default_cli` as today. The Antigravity install triggers that
  hang off this command are in doc 04.
- `set_skip_permissions` (`host.rs:922`) gets `CliKind::Antigravity => user_cfg.antigravity.autonomy =
  Some(enabled)`.
- `src/lib/settings.ts:17-108` mirrors: `SettingsProject.providers: Record<CliKind, {path: string | null;
  autonomy: boolean; dispatch_steps: string[]}>`, `SettingsUser.providers: Record<CliKind, {path: string |
  null; autonomy: boolean}>`, `SettingsUser.available: Record<CliKind, boolean>`;
  `saveProjectSettings(input: {archiveDays; logRetentionDays; worktreesDir; providers: Record<CliKind, {path:
  string | null; dispatchSteps: string[]}>; diffBase})`; `saveUserSettings(input: {providers: Record<CliKind,
  string | null>; defaultCli})`. Tauri maps the camelCase argument names to the snake_case Rust parameters
  (`dispatchSteps` inside the provider object is deserialized by serde, so `ProjectProviderInput` carries
  `#[serde(rename_all = "camelCase")]`).

Today's asymmetric wire names (`claude_path`, `skip_permissions`, `dispatch_steps`, `codex_*`) disappear from
IPC but stay as TOML keys.

### SQLite (`crates/keitora-tasks`)

1. `cli_to_sql` / `cli_from_sql` (`local_store.rs:45-63`): add `CliKind::Antigravity => "antigravity"` and
   `"antigravity" => Ok(CliKind::Antigravity)`.
2. `sqlite_backend.rs`: change the CHECK in the `CREATE TABLE IF NOT EXISTS resume_sessions` text (`:126`) to
   `CHECK(cli IN ('claude','codex','antigravity'))`.
3. Add `fn upgrade_resume_sessions_check(conn: &Connection) -> rusqlite::Result<()>` in `sqlite_backend.rs`,
   called at the end of `init_schema` (after the `execute_batch` and the `DISPATCH_LOG_DDL` batch, before
   `Ok(())`). Behaviour:
   - `SELECT sql FROM sqlite_master WHERE type='table' AND name='resume_sessions'`; if no row or the text
     contains `'antigravity'`, return `Ok(())`.
   - Otherwise run, in one transaction (`BEGIN IMMEDIATE` … `COMMIT`, `ROLLBACK` on error):
     `CREATE TABLE resume_sessions_new (…same columns…, CHECK(cli IN ('claude','codex','antigravity')))`;
     `INSERT INTO resume_sessions_new (project, worktree, cli, session_id, source, agent_present, cleared_id,
     observed_at) SELECT project, worktree, cli, session_id, source, agent_present, cleared_id, observed_at
     FROM resume_sessions`; `DROP TABLE resume_sessions`; `ALTER TABLE resume_sessions_new RENAME TO
     resume_sessions`. The `CREATE TABLE` text for `_new` is the same DDL string as the main schema with only
     the table name changed; define the DDL once as a `const RESUME_SESSIONS_DDL: &str` using a `{name}`
     placeholder (`str::replace`) so the two cannot drift.
4. Both `SqliteBackend::open` (`:208`) and `LocalStore::open` (`local_store.rs:72`) already call
   `init_schema`, so one function covers every handle.

### Downgrade risk

An older Keitora fails to parse a config that says `"antigravity"` (unknown enum variant). Accepted. Note it
in the release notes (doc 08).

## Tests required

SQLite (`crates/keitora-tasks/src/local_store.rs` `mod tests`, next to `init_schema_adds_resume_table_to_existing_db`):

- `init_schema_upgrades_resume_check_for_antigravity` — build a DB with the old DDL (CHECK of
  `('claude','codex')`) holding one `claude` and one `codex` row; open via `LocalStore`; assert
  `sqlite_master.sql` for `resume_sessions` now contains `'antigravity'`.
- `resume_upgrade_preserves_rows` — after the upgrade both old rows read back with identical
  `session_id`, `source`, `agent_present`, `cleared_id`, `observed_at`.
- `resume_accepts_antigravity_after_upgrade` — `resume_observe(.., CliKind::Antigravity, Some("id"), ..)`
  succeeds and `resume_get` returns `cli == CliKind::Antigravity`.
- `resume_upgrade_is_idempotent` — opening the same DB twice leaves exactly one `resume_sessions` table and no
  `resume_sessions_new`, and the row count unchanged.
- `fresh_db_has_the_new_check` — a brand-new DB accepts an Antigravity insert without any upgrade.

Config (`config.rs` `mod tests`):

- `antigravity_table_round_trips` — a `UserConfig` with `antigravity.path = Some("/x/agy")`,
  `autonomy = Some(true)` serializes to a `[antigravity]` table and parses back equal; an empty one omits the
  table.
- `dispatch_is_empty_counts_antigravity_steps` — `DispatchConfig { antigravity_steps: vec!["a".into()],
  ..Default::default() }.is_empty() == false`.
- `updates_is_empty_counts_agent_skip_antigravity`.
- `provider_selectors_cover_antigravity` — `resolve_provider_path(Antigravity, Some(&project), &user)` returns
  the project path over the user path, and `resolve_autonomy(Antigravity, …)` the project value over the user
  value.
- `worktree_entry_round_trips_antigravity_conversation_id` — an entry with only that id set serializes in
  table form and parses back; a bare-name entry parses with `antigravity_conversation_id == None`.
- `merge_carries_forward_antigravity_conversation_id` — `merge_worktree_registry_fields` keeps the old id when
  the incoming entry has none.
- `provider_state_set_and_clear_antigravity_id` — `set_worktree_provider_state_by_root` with
  `ProviderIds::keep().with(Antigravity, Set("c1"))` stores it; a later `Clear` removes it; `Keep` leaves the
  Claude and Codex ids untouched in both calls.
- `antigravity_dispatch_override_flows_through_effective_config` — a `ProjectRef` with
  `antigravity_dispatch_steps = Some(vec!["s"])` yields `cfg.dispatch.antigravity_steps == ["s"]`;
  `ProjectSettingsPatch { antigravity_dispatch_steps: Some(None), .. }` clears it; `register` with a different
  path resets it to `None`.

Config edit (`config_edit.rs` `mod tests`):

- `set_project_provider_path_antigravity_creates_and_removes_table` — set then `None` collapses `[antigravity]`.
- `set_project_dispatch_steps_antigravity_writes_key_and_keeps_siblings` — writes
  `[dispatch].antigravity_steps`; clearing it keeps `claude_steps`.
- `write_project_settings_writes_all_three_providers_and_omits_empty` — the map form produces `[claude]`,
  `[codex]`, `[antigravity]` only for non-empty providers (rewrite of the existing test at `:1130`).
- Existing wrapper tests (`set_project_claude_path_*`, `set_dispatch_steps_*`, `set_user_*`) pass unchanged.

Host (`host.rs` tests, wire shapes):

- `settings_snapshot_serializes_providers_keyed_by_cli` — `serde_json::to_value(get_settings-built snapshot)`
  has `project.providers` and `user.providers` objects whose keys are exactly `claude`, `codex`,
  `antigravity`, and `user.available` with the same keys.
- `build_settings_patch_sets_all_three_step_lists` — explicit-empty lists map to `Some(Some([]))` for each
  provider.
- `commit_settings_to_project_writes_antigravity_table` — committing a project with an Antigravity path and
  steps writes `[antigravity]` and `[dispatch].antigravity_steps` (both the existing-file and scaffold
  branches).
- `set_skip_permissions_writes_antigravity_autonomy`.

TypeScript (`src/lib/settings.test.ts`, new): a typed-fixture test asserting the exported interfaces accept the
Rust JSON shape above (compile-time) and that `saveProjectSettings` invokes `save_project_settings` with a
`providers` object keyed by all three providers (mock `invoke`).

## Cross-references

- 01 (provider model), 03 (registry id used by resume; availability refresh), 04 (install triggers after
  `save_user_settings`), 05 (`agent_skip_antigravity` consumers), 06 (uses `ProviderIds` /
  `persist_worktree_cli`), 07 (settings UI consumes the keyed shapes), 08 (release notes mention the
  downgrade).
