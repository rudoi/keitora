# Phase 6: self-diagnostics

Part of 0080; see `00-overview.md`. Last phase because it consumes signals from every other
phase.

## Purpose

The app watches itself. It notices when signals disagree or plumbing stalls, fixes what it can,
and tells the human and the orchestrating agent what is wrong, so the next report is
diagnosable.

## Verified current code

All paths under `D:/repos/keitora`.

| Symbol | Where | Fact that matters |
|---|---|---|
| `StatusLoop::tick`, `due()` | `src-tauri/src/backend/herdr/status_loop.rs` | Returns early on explicit stop and when `ServerGate` refuses; absent under `UnavailableBackend`. The monitor is therefore driven by `Pipeline::tick` on the `keitora-pipeline` thread (`04-request-pipeline.md` P1, `02-engine-core.md` S14), not by the loop. |
| `reconciled_sessions_in` | `src-tauri/src/tasks.rs` | Filters session rows to the active project's configured worktrees; rows for other names are dropped silently today. |
| `WorktreeSession` (`state`, `last_heartbeat`, `driven_by`, `brokered`, ...) | `crates/keitora-tasks/src/model.rs` | Hook-row evidence: `last_heartbeat` is the last hook write. |
| `current_worktree()`, `cli_actor()` | `crates/keitora-cli/src/main.rs` | The CLI names the acting worktree from where it runs (the folder), which is why a folder name that differs from the configured name matters. |
| `refresh_managed_copy_if_stale`, `decide`, `default_link_path`, `locate_installable` | `src-tauri/src/cli_install.rs` | Existing remedy for a stale managed CLI copy; only touches the copy when PATH's `keitora` *is* the managed one. |
| `exe_is_untrusted_for_bundled_cli` (translocated or cargo build) | `src-tauri/src/hooks.rs` | The existing "app is translocated" test the agy check reuses. |
| `hook_command`, `hooks_file`, `sync_worktree`, `sync_registry`, `install_antigravity_hooks` | `src-tauri/src/antigravity_hooks.rs` | Install/re-assert entry points for the agy remedy. |
| `DoctorRow`, `DoctorStatus`, `DoctorAction` | `src-tauri/src/deps/doctor.rs` | The existing tool doctor (herdr, git, claude, codex, antigravity). Health is a separate list, not new doctor rows. |
| `DepsSection`, `DepsProvider` | `src/components/DepsSection.tsx`, `DepsProvider.tsx` | The pattern for a Settings section backed by a provider and an IPC command. `SettingsDrawer.tsx` renders `<DepsSection />` last (about line 1043). |
| `Lane` (`lane-name`, `lane-stage`, activity dot `title`s), `ProjectGroup` | `src/components/rail/Lane.tsx`, `ProjectGroup.tsx` | Where the glyph attaches. |
| `Verb`, `Cmd`, drive exit codes (2 timeout, 4 usage/nothing to wait for, 5 refused) | `crates/keitora-cli/src/main.rs`, `drive.rs` | No `status` verb exists (checked); `Cmd::Task` has `--status` filters only. |
| `findings` table | `02-engine-core.md` S10 (the one DDL) | Earlier text named a column `check`, a reserved word in SQLite; it is `check_id`, and 02's DDL carries this phase's columns (`detail`, `occurrences`, `cleared_at`, `remedy`, `remedy_at`). |

## Design

### Monitor structure

`src-tauri/src/health/mod.rs` (new) with a pure core:

```rust
pub struct HealthInputs { /* gathered below */ }
pub struct Finding {
    pub project: String,   // "" for app-level findings
    pub worktree: String,  // "" for project- or app-level findings
    pub check_id: &'static str,
    pub severity: Severity, // Info | Warn | Error
    pub message: String,
    pub detail: serde_json::Value,
}
pub fn evaluate(inputs: &HealthInputs, now_ms: i64) -> Vec<Finding>; // pure, no I/O
```

`HealthMonitor` wraps it: `tick(now)` is a 30 s item of `Pipeline::tick`; `on_view_change(project,
worktree)` (an `engine.subscribe` callback on the status-loop thread) only marks the key dirty,
and `Pipeline::tick` re-evaluates dirty keys 2 s after their first mark (the debounce). A driver gathers
`HealthInputs` (cheap reads: engine views, in-memory rings, hook rows through one read-only
`LocalStore` per project, pipeline state, link state, hook facts, identity facts, version
facts) and passes it to `evaluate`; all logic is in `evaluate`, so tests need no herdr, no
disk and no clock.

**Reconciliation** (`reconcile(active, desired, now)`), run after each `evaluate`:
- Key = `(project, worktree, check_id)`.
- New key: insert, `first_seen = last_seen = now`, `occurrences = 1`.
- Same key, same `severity` and `message`: no write; `last_seen` is flushed at most once per
  60 s (queued through `StatusPublisher::findings_apply` and written inside the
  `keitora-status-heartbeat` transaction, `02-engine-core.md` S10), so an unchanged finding costs
  ~1 write/min and a healthy app costs none.
- Same key, changed `severity` or `message`: update both and `detail`.
- Key absent from `desired` on **two consecutive evaluations** (at least one full 30 s
  period, or two debounced evaluations): set `cleared_at = now`. Checks marked "instant" below
  clear on the first absence. A cleared key that reappears starts a new episode:
  `cleared_at = NULL`, `first_seen = now`, `occurrences += 1`.
- Cleared rows are deleted 24 h after `cleared_at` and at app start.

**Remedies** run from `reconcile` when a finding is *newly inserted* or re-opened, at most once
per key per 10 minutes, are skipped when `global_writes` is disabled (`cfg(test)`,
`KEITORA_HERDR_NO_INTEGRATION`), and are recorded in `findings.remedy` / `remedy_at`
(`"reinstalled hooks"`, `"restarted watcher"`, ...). A remedy never raises severity and never
blocks a tick (it is spawned).

### Storage

`status.db` (`02-engine-core.md`) owns the file, the lease and the **DDL** of the `findings`
table (S10: `project`, `worktree`, `check_id`, `severity` in `info|warn|error`, `message`,
`detail` JSON, `first_seen`, `last_seen`, `occurrences`, `cleared_at`, `remedy`, `remedy_at`;
primary key `(project, worktree, check_id)`; `project = ''` for app-level and `worktree = ''`
for project- or app-level findings). This phase owns what goes in it (checks, reconciliation,
remedies) and writes it only through `StatusPublisher::findings_apply`.

- Times are INTEGER unix epoch milliseconds, like every `status.db` table (02 S10).
- Only the lease holder writes. A loser (`status_writer` finding) keeps its findings in memory
  and serves the UI from memory; the file is not touched.
- The Settings and glyph surfaces read the **in-memory** monitor state through IPC; only the CLI
  reads the table.

### The checks

Severity: `info` shows only in Settings/`keitora status`; `warn` and `error` also show the
glyph. "Clear" is the default two-evaluation hysteresis unless marked instant. `T` = local
time `HH:MM:SS`; `age` = `{s}s` under 120 s, `{m}m` under 2 h, else `{h}h`.

| id | scope | inputs | condition | sev | message template | auto-remedy | clear |
|---|---|---|---|---|---|---|---|
| `hooks_vs_herdr` | worktree | engine view (`agent`, `herdr_status`), `engine.key_health()` `present_since_ms` (02 S6), `worktree_sessions` row (`state`, `last_heartbeat`, `session_id`) | `agent == present` for >= 30 s **and** (no hook row, or a row from another session (02 S5 rule 0), or row `state == offline`, or `last_heartbeat < present_since_ms`) | warn | `@{wt}: herdr sees a {provider} agent ({herdr_status}) but its hooks have not reported since the agent appeared ({no hook rows \| last hook write {age} ago, state {state}}). Keitora is using herdr's view.` | none (the engine already ignores the stale row) | a hook write at or after `present_since_ms`, or agent absent |
| `completion_without_stop` | worktree | `engine.turn_health()` `recent`: ring of the last 10 `CompletionNote {kind, signal, reply_source, transcript_miss}` (`03-turns-and-cli.md` T5 step 5) | at least 2 of the last 5 completions have `signal == herdr` and no hook reply | info; warn when the latest completion's `reply_source` is `none` | `@{wt}: {n} of the last {m} turns finished with no Stop hook; reply text came from {the transcript \| nowhere (use keitora read)}` | transcript fallback (by design, nothing to run) | instant: the latest 3 completions all had a hook reply |
| `turn_start_not_observed` | worktree | `engine.turn_health()` `no_activity` (delivery id, time): the app-side twin of the CLI's `no activity observed since delivery` stall (`03-turns-and-cli.md` T5 step 5) | a delivery stalled with that reason in the last 24 h and no later delivery showed `working` or a completion | warn | `@{wt}: a send showed no herdr activity for 30 s; herdr may not recognise this UI (try: herdr agent explain {pane_id})` | none | instant: a later delivery showed `working` or completed |
| `hooks_missing` | worktree | `hook_facts(config, wt)` from `05-hook-placement.md` section 8 | any of: `codex.hooks_json_ok == false`; `codex.trust_bypass_last_launch == Some(false)`; Claude's hooks are neither applied per launch (`claude.per_launch_settings != Applied`) nor installed (`claude.hooks_installed == false`); `hook_version_ok == false`. `detail` carries `last_hook_error` and a skipped settings reason | warn (error when the worktree is driven and `brokered` is expected) | `@{wt}: {.codex/hooks.json is missing Keitora's hooks \| the last launch ran without the hook trust bypass \| Claude's Keitora hooks are not installed}; the permission gate and Stop capture won't run` (joined with `; ` when several) | reinstall hooks for the worktree's provider (the `prepare_launch` installer from `05-hook-placement.md`), effective on the next launch | facts clean on re-read (after the remedy or a relaunch) |
| `codex_project_untrusted` | worktree (Codex only) | `hook_facts(..).codex.project_trust` (`05-hook-placement.md` section 8; read-only look at `~/.codex/config.toml`) | `project_trust` is `NotListed` or `Untrusted(_)` for >= 1 evaluation | info when the last launch carried the bypass (`trust_bypass_last_launch == Some(true)`), else warn; `Unreadable` and `NotApplicable` never fire | `@{wt}: Codex does not list this project as trusted in config.toml ({not listed | trust_level {value}}). Keitora's own launches bypass the hook trust prompt, but a hand-launched codex or herdr's own session-id hook may not run its hooks here; trust it in Codex ({path_to_config})` | **none, by design**: report only; Keitora never writes the user's global `~/.codex/config.toml` | facts clean on re-read (the user trusted the project) |
| `agy_plumbing` | worktree (agy only) | `hook_facts(..).agy` (`05-hook-placement.md` section 8): `key_present`, `path_ok`, `marker_env_ok`, `translocated` | any of: `!key_present`; `!path_ok` (command path not absolute or file missing); `marker_env_ok == Some(false)` (the last launch did not set `KEITORA_PANE`); `translocated` (or a cargo build) | warn | `@{wt}: agy hooks are not wired ({hooks.json has no keitora key \| the hook command path is missing \| KEITORA_BIN is unset in the pane \| the app is translocated; move it to Applications}); agy sessions show only herdr's view` | `antigravity_hooks::sync_worktree` + `sync_registry` (re-assert the key; skipped when translocated) | facts clean |
| `worktree_identity` | worktree | configured name, `worktree_path(name)` folder name | folder name (last path component, case-insensitive) differs from the configured name | info | `@{name}: its folder is named {folder}, not {name}; CLI verbs run there name themselves after the folder` | none | names match |
| `pipeline_stalled` | project | `app_projects` (`pending_count`, `oldest_pending_at`, `last_pass_at`), the pending row's `seen_at` | oldest pending request >= 30 s old **and** (`seen_at` is NULL **or** `now - last_pass_at > 30 s`) | warn (error at >= 120 s) | `{project}: request {id} pending {age} with no pass since {T \| "start"}` | `request_pass(project, 0, ...)` and the watcher restart from `04-request-pipeline.md` | instant: a pass ran after the row's `created_at` and the row has `seen_at` (or is resolved) |
| `watcher_restarted` | project | `WatcherRegistry` entries (`restarts` in the last 10 min, `exit_reason`) | a watcher was restarted by the supervisor in the last 10 min | warn; error at >= 3 restarts | `{project}: the {tasks \| background} watcher {exited ({reason}) \| stalled} and was restarted ({n}x in 10m)` | already restarted (the finding reports it) | 10 min without a restart |
| `watcher_silent` | project | supervisor's silent-while-aged rule | a restart happened because a request aged >= 15 s with no watcher event | warn | `{project}: the watcher missed request {id} (no event for {age}); restarted, the sweep claimed it` | already restarted | 10 min without one |
| `herdr_link` | app | `engine.link_state()` (02 S6): `link` (`starting`, `up`, `recovering`, `down`, `unavailable`), `since_ms`, `last_snapshot_ms` | link not `up` for >= 15 s | warn; error at >= 120 s | `herdr unavailable for {age} ({link}); status is stale (last snapshot {age} ago \| no snapshot yet)` | none (wait for recovery) | instant: a fresh snapshot |
| `herdr_unknown` | worktree | view `herdr_status == unknown` with `agent == present`, `unknown_since` | `unknown` for > 30 s | warn | `@{wt}: herdr reports the agent status as unknown for {age}; Keitora keeps the last known state` | none | instant: status changes |
| `status_writer` | app | `status.db` lease (`writer.pid`, `version`, `heartbeat_at`) | this instance isn't the lease holder | warn | `another Keitora instance (pid {pid}, v{version}) owns status.db; this instance is read-only` | none (this instance stays read-only) | instant: this instance takes the lease |
| `version_skew` | app | PATH `keitora --version` (run at start, then every 10 min, `3 s` timeout), app version, `refresh_managed_copy_if_stale` inputs | PATH `keitora` exists, is not a cargo/dev build, and its version differs from the app's | warn when older; info when newer | `the keitora on PATH is {older than \| newer than} the app ({path_ver} vs {app_ver})` | `refresh_managed_copy_if_stale(on_path, managed, bundled)` when PATH's copy is the managed one | versions match |
| `unconfigured_sessions` | project | session/hook rows whose worktree name isn't in the project's configured worktrees (previously dropped by `apply_reconcile`/`reconciled_sessions_in`) | one or more such rows with `last_heartbeat` within 24 h | info | `{project}: session rows exist for unconfigured worktrees: {names}` (first 5, then `+n more`) | none | no such rows |

Notes:
- `{provider}` is `claude`, `codex` or `agy` from the worktree's resolved provider.
- `hooks_vs_herdr` and `hooks_missing` can both fire; the glyph tooltip shows the highest
  severity first, then the newest.
- **Inputs required from other phases** (interfaces, not implemented here):
  - `02-engine-core.md` S6: `key_health()` (`present_since_ms`, `unknown_since_ms`, `pane_id`),
    `link_state()` (`link`, `since_ms`, `last_snapshot_ms`), `all_views()`, `subscribe` (the
    debounce is this phase's), `StatusPublisher::lease()` and `findings_apply` (S10),
    `StatusReader::findings` for the CLI.
  - `03-turns-and-cli.md` T5 step 5: `turn_health()` (`recent` completion ring, `no_activity`).
  - `05-hook-placement.md` section 8: `hook_facts(config, worktree) -> HookFacts` (`bin`,
    `codex {hooks_json_ok, trust_bypass_last_launch, project_trust}`, `claude {hooks_installed,
    per_launch_settings}`, `agy {key_present, path_ok, marker_env_ok, translocated}`,
    `last_launch`, `hook_version_ok`, `last_hook_error`). Read-only; the remedies are
    `prepare_worktree` and `sync_worktree` + `sync_registry` there.
  - `06-reply-fallbacks.md`: the transcript miss reasons, carried in `CompletionNote.transcript_miss`.
  - `04-request-pipeline.md`: `app_projects`, `WatcherRegistry`, the restart history.

### Surfaces

**Rail glyph.**
- New `src/components/rail/HealthGlyph.tsx` (a small `!` badge, amber for `warn`, red for
  `error`; nothing for `info` or no findings). Rendered in `Lane.tsx` after the `lane-name`
  span (inside the existing name row) for worktree findings, and in `ProjectGroup.tsx`'s header
  for the maximum severity over the project's worktrees plus project-level findings.
- Props: `findings: HealthFinding[]` (already filtered to the lane or project, severity >= warn),
  `scope: "lane" | "project"`. Tooltip (`title`) text: the first finding's `message`, plus
  `\n(+{n} more)` when there are more; the order is severity desc, then `last_seen` desc.
  `aria-label` = `Health: {n} issue{s}`. Clicking opens Settings scrolled to the Health section
  (existing settings open event; no new modal).
- State comes from a new `HealthProvider.tsx` (mirrors `DepsProvider`): invokes `health_list`,
  listens to `health://changed`, and exposes `useHealth(project, worktree?)`. `src/lib/ipc.ts`
  gains `healthList(): Promise<HealthFinding[]>` and the `HealthFinding` type (camelCase of the
  fields above plus `clearedAt`).
- `paneStatus.ts` and `laneState.ts` are **not** changed: the glyph is additive and never
  changes lane state.

**Settings Health section.**
- New `src/components/HealthSection.tsx`, rendered in `SettingsDrawer.tsx` after
  `<DepsSection />`, same `<section className="flex flex-col gap-3 border-t border-line pt-6">`
  pattern. Title `health`. Content: an empty state (`no issues`), else a list grouped by
  project then worktree, each row: severity dot, `check_id`, message, `first seen {T}`, `last
  seen {age} ago`, `{occurrences}x`, and `remedy` when set (`fixed by: {remedy} at {T}`); cleared
  findings from the last 24 h under a collapsed `recently cleared` group. A `copy as JSON`
  button copies the same payload `keitora status --json` prints (`findings` array), so a human
  can paste it into a report.
- Tauri: `health_list() -> Vec<HealthFinding>` (active and recently cleared) in
  `src-tauri/src/health/commands.rs`, registered in `host.rs` `invoke_handler`; event
  `health://changed` emitted by `reconcile` on any insert, update or clear.

**`keitora status [<wt>] [--json]`** (new top-level `Cmd::Status` in `crates/keitora-cli/src/
main.rs`, handled like the drive verbs: it needs only the read-only `status.db` reader and the
project's `tasks.db`). It never talks to herdr (D7), never writes, and exits 0 whenever it could
produce output (1 on an unresolvable project).

Freshness: `status.db` readable and `writer.heartbeat_at` within 15 s => `source: "status_db"`;
otherwise `source: "legacy"` (worktree rows from `tasks.db` only, `app.running = false`).

JSON shape (`--json`; `<wt>` filters `worktrees` to one entry and adds `detail`):

```json
{
  "source": "status_db",
  "app": { "running": true, "pid": 4242, "version": "0.6.0", "started_at": "2026-01-01T10:00:00Z",
           "heartbeat_age_s": 2, "herdr_link": "up", "snapshot_age_s": 1, "engine_ready": true },
  "project": { "name": "my-game", "open": true,
               "watcher": { "state": "running", "last_event_age_s": 40, "last_pass_age_s": 12, "restarts": 0 },
               "pending_requests": 0, "oldest_pending_age_s": null },
  "worktrees": [
    { "name": "bravo", "provider": "codex", "state": "idle", "agent": "present", "herdr_status": "idle",
      "settled": true, "stale": false, "pane_id": "p1", "agent_session": "00000000-0000-4000-8000-000000000001",
      "driven_by": "alpha", "last_change_age_s": 31,
      "hook": { "state": "offline", "last_write_age_s": 7200 },
      "findings": ["hooks_vs_herdr"] }
  ],
  "findings": [
    { "project": "my-game", "worktree": "bravo", "check_id": "hooks_vs_herdr", "severity": "warn",
      "message": "...", "first_seen": "2026-01-01T10:05:00Z", "last_seen": "2026-01-01T10:09:00Z",
      "occurrences": 1, "remedy": "" }
  ],
  "detail": { "turns": [ { "kind": "driver", "signal": "herdr", "reply_source": "none", "completed_at": "..." } ] }
}
```

- Times in JSON are RFC 3339 UTC; ages are whole seconds; unknown values are `null`.
- `detail` appears only with `<wt>` and holds the last 5 `drive_turns` rows for that worktree.
- With `source: "legacy"` the `app` object is `{ "running": false, "heartbeat_age_s": <n|null> }`,
  `worktrees[].state` comes from the hook row (as old CLIs show it), `findings` is `[]`, and
  `engine_ready` is absent.

Human output (no `--json`):

```
keitora status (project my-game)
  app       running (pid 4242, v0.6.0), herdr up, snapshot 1s ago
  project   open in window; watcher running; 0 pending requests
  worktrees
    @alpha  idle   claude  herdr: idle (settled)
    @bravo  idle   codex   herdr: idle (settled)   driven by @alpha   hooks: offline, last write 2h ago
  findings (1)
    warn  @bravo  hooks_vs_herdr  @bravo: herdr sees a codex agent (idle) but its hooks ...
```

With `<wt>`: the one worktree's line, its findings in full (message, first seen, occurrences,
remedy), and its last 5 turns. `app` reads `not running (last update {age} ago)` or `not running
(no status reported on this machine)` when stale or missing, followed by `showing hook rows
only`.

**Drive verbs append findings and detail.**
- *App-side refusals* (the text is stored in the request row, so the app builds it). The
  offline/busy refusals from `state_refusal` (`src-tauri/src/drive.rs`) gain a parenthetical
  built from the engine view and the hook row: `@{wt} is offline; start its session first
  (herdr: {no agent \| status}; {last hook write {age} ago \| no hook rows})`. Example:
  `@bravo is offline; start its session first (herdr: no agent; last hook write 2h ago)`.
  Existing exact-text tests of `state_refusal` are updated.
- *`wait` stalls and timeouts* (CLI-side): every `stalled` event, and the request timeouts from
  `04-request-pipeline.md`, add `"findings": [{ "check_id", "severity", "message" }]` (JSON) and
  one trailing human line per finding, `  health: {message}`, for the driven worktree's active
  `warn`/`error` findings, at most 2 (highest severity, then newest). Read from `status.db`
  while fresh; absent otherwise.
- Wording is fixed here so tests can assert it; messages come from the templates above.

## Files touched (as referenced)

- new `src-tauri/src/health/` (`mod.rs` pure core, `driver.rs` input gathering and
  reconcile, `commands.rs` IPC), registered in `host.rs`; ticked by `src-tauri/src/pipeline.rs`
- `status.db` `findings` DDL (with `02-engine-core.md`)
- `src-tauri/src/drive.rs` (`state_refusal` detail), `src-tauri/src/tasks.rs`
  (`reconciled_sessions_in` reports unconfigured names to the monitor)
- `src/components/rail/HealthGlyph.tsx`, `Lane.tsx`, `ProjectGroup.tsx`;
  `src/components/HealthProvider.tsx`, `HealthSection.tsx`, `SettingsDrawer.tsx`;
  `src/lib/ipc.ts`
- `crates/keitora-cli/src/main.rs` (`Cmd::Status`), `crates/keitora-cli/src/drive.rs` or a new
  `status.rs` (rendering), the `status.db` reader (`02`/`03`), `wait` stall output

## Reports fixed

All reports, for diagnosability (A1, A2, B, C, R).

## Regression tests

Pure `evaluate` tests (table-driven, fake clock, no I/O), one fires-then-clears test per check:

| Check | Fires on | Clears on |
|---|---|---|
| `hooks_vs_herdr` | agent present 31 s, no hook row; and row `offline`; and `last_heartbeat` before `present_since_ms`; and a `busy` row from another `session_id` | hook write after `present_since_ms`; agent absent. Does not fire at 29 s. |
| `completion_without_stop` | 2 of the last 5 completions herdr-only (info), latest `none` (warn) | 3 consecutive hook replies |
| `turn_start_not_observed` | the 30 s no-activity stall record | a later delivery with `working` |
| `hooks_missing` | each fact alone (codex file, no bypass, claude missing, old hook version) and several joined with `; ` | facts clean |
| `codex_project_untrusted` | `NotListed` and `Untrusted("untrusted")` each alone; severity info with the bypass recorded, warn without; `Unreadable` / `NotApplicable` / non-Codex worktree do not fire; the evaluation returns no remedy (a test asserts `remedy.is_none()` and that no file under the temp `CODEX_HOME` changed) | `Trusted` on re-read |
| `agy_plumbing` | each fact alone; translocated suppresses the remedy | facts clean |
| `worktree_identity` | `Bravo` vs `bravo` does not fire (case-insensitive); `bravo2` does | names match |
| `pipeline_stalled` | 30 s old, `seen_at` NULL; and 30 s old with `last_pass_at` 31 s ago; severity error at 120 s | a pass after `created_at` with `seen_at` set |
| `watcher_restarted`, `watcher_silent` | restart records (1x warn, 3x error) | 10 min quiet |
| `herdr_link` | recovering 15 s (warn), 120 s (error); message reads `herdr unavailable for 15s (recovering)...` | fresh snapshot |
| `herdr_unknown` | `unknown` 31 s | status changes |
| `status_writer` | not the lease holder | takes the lease |
| `version_skew` | PATH older (warn), newer (info), dev build ignored | versions match |
| `unconfigured_sessions` | one row, six rows (`+1 more`) | rows gone |

Monitor and surfaces:
- `reconcile`: insert once, no write on an unchanged finding within 60 s, update on message
  change, two-evaluation hysteresis, instant clears, re-open increments `occurrences`, cleared
  rows pruned at 24 h; zero writes for a healthy app.
- Remedies: run once per key per 10 min, never under `global_writes` off, recorded in
  `remedy`/`remedy_at`; the remedy functions are faked in tests.
- Loser instance: findings kept in memory, nothing written to `status.db`.
- `findings` DDL: covered by 02's `status_ddl_matches_the_documented_columns`; this phase tests
  `findings_apply` round-trips every column.
- `keitora status`: JSON shape snapshot for fresh, stale and missing `status.db`; `<wt>` filter
  with `detail.turns`; human output snapshot for fresh and legacy; exit 0 and 1.
- Verb integration: `state_refusal` offline text with and without a hook row; a `stalled` event
  and a request timeout both carry `findings` / `health:` lines, at most 2, highest severity
  first.
- Frontend (`vitest`): `HealthGlyph` shows nothing for `info`/empty, amber for `warn`, red for
  `error`, tooltip text with the `+n more` suffix and its ordering; `HealthSection` empty
  state, grouping, `recently cleared` collapsed group, JSON copy.

## Dependencies

- `02-engine-core.md` (view, `status.db`, `findings` DDL, link state), `03-turns-and-cli.md`
  (completion ring, no-activity stall, stalls, the CLI `status.db` reader),
  `04-request-pipeline.md` (`Pipeline::tick`, `app_projects`, `WatcherRegistry`, restart
  history), `05-hook-placement.md` (`hook_facts`, launch record, remedies),
  `06-reply-fallbacks.md` (transcript lookup failures).

## Singletonize decisions

- **H1. Driver.** The monitor is a 30 s item of `Pipeline::tick` on the `keitora-pipeline`
  thread plus a 2 s debounced per-worktree evaluation of keys marked dirty by view changes, so it
  runs in recovery and without herdr (the status loop doesn't). Thread table:
  `02-engine-core.md` S14.
- **H2. Pure core.** `evaluate(inputs, now) -> Vec<Finding>`; reconciliation, hysteresis and
  remedies are separate and testable.
- **H3. Schema.** `check` -> `check_id` (reserved word); added `detail`, `occurrences`,
  `cleared_at`, `remedy`, `remedy_at`; primary key `(project, worktree, check_id)`; unix-ms
  integers. The DDL itself lives only in `02-engine-core.md` S10.
- **H4. Memory is the UI source.** IPC reads in-memory state; the table exists for the CLI.
  A lease loser never writes.
- **H5. Clear semantics.** Two-evaluation hysteresis by default; instant clears only where
  marked; episodes counted by `occurrences`; cleared rows kept 24 h.
- **H6. Severity.** `info` is Settings/`status` only; the glyph needs `warn`.
- **H7. Added checks.** Beyond the proposal's table: `pipeline_stalled`, `watcher_restarted`,
  `watcher_silent`, `herdr_unknown` and `unconfigured_sessions` are separate ids (the proposal
  listed them as one row or an aside).
- **H8. `keitora status`.** Top-level verb; `status_db` vs `legacy` source; JSON/human shapes
  above; exit 0 unless the project can't be resolved.
- **H9. Verb integration.** Refusal detail is app-side text; stall/timeout findings are
  CLI-side from `status.db`, capped at 2 and only `warn`/`error`.
- **H10. Hook facts are an input contract on 05** (`hook_facts`, `05-hook-placement.md`
  section 8), read-only; remedies live in 05's installers and are only invoked from `reconcile`.
- **H11. Link vocabulary** is 02's `HerdrLink` (`starting|up|recovering|down|unavailable`); there
  is no `healthy` value, in the check, in `keitora status` JSON or in its human output.
