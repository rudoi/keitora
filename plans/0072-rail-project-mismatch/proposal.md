# 0072 — Rail files worktrees under the wrong project

## Goal

The rail must never show one project's worktrees inside another project's group, and if
the frontend's idea of the active project ever drifts from the backend's, it must heal
on its own instead of persisting until restart.

## The bug

Observed (macOS, under suspected memory pressure): the active group `proj` rendered the
worktrees of a different registered project (`other-proj`). It stayed that way until the
app was restarted. Nobody recalls a project switch right before; which project the
backend had active is unknown.

### Mechanism

Two independent reads feed the active project's rail group, and they can disagree:

| Read | Source | Re-read when |
|---|---|---|
| `project` (active project summary) | `loadProject()` → `current_project` (`src/App.tsx:359`) | `project://changed` (`src/App.tsx:432`); the switch path's own `Promise.all` (`src/App.tsx:863`). **No periodic resync.** |
| `worktrees` (active project's list) | `useLiveResource(listWorktrees, "worktrees://changed")` → `worktrees` (`src-tauri/src/host.rs:46`) | `worktrees://changed`, manual `reloadWorktrees()`, **blind 30 s tick** |

The `worktrees` response carries **no project identity** — it is whatever the user
registry says is active *at read time*. The warm-seeding effect (`src/App.tsx:980`) files
"the current `worktrees`" under "the current `project.name`" whenever `switchingTo` is
clear; `activeWorktrees` (`src/App.tsx:158`) falls back to the raw `worktrees` too.
Neither read is ordered: whichever response lands last wins.

Failure paths, most to least likely to explain a restart-only fix:

1. **Silent active-project change (persistent).** `keitora config use X`
   (`crates/keitora-cli/src/main.rs:733`), `keitora init` on an empty registry
   (`main.rs:642`), or a second Keitora instance's `set_active_project` rewrites the user
   registry's `active`. **Nothing watches the user registry** — the config watcher
   (`host.rs:2059`) watches only the active project's `keitora.toml`. The next 30 s
   `worktrees` tick returns X's list; `project` still holds the old project; the warm
   effect files X's worktrees under it. It persists until some unrelated
   `project://changed` (settings save, worktree create) happens to run `loadProject()`.
2. **Out-of-order `currentProject()` (persistent).** A switch runs `loadProject()` in its
   `Promise.all`, and `project://changed` (emitted directly by `set_active_project`, and by
   settings/worktree/discovery commands) runs another. Under load an older response can
   land last: `project` reverts to the previous project while the backend has moved on.
   Same persistence as (1).
3. **Stale `worktrees` response (transient).** A load started under `other-proj` lands
   after the switch to `proj` (slow backend; the 4 s `switchingTo` safety net at
   `App.tsx:832` clearing before the switch's reload returns). Heals on the next reload.
4. **Duplicate resolved names (persistent, structural).** `buildGroups` marks *every*
   registry entry whose `resolved_name` equals the active one as `isActive`
   (`src/lib/railGroups.ts:187`), so two registered projects that resolve to the same
   `[project].name` both render the active lanes. No uniqueness check at registration.
   Unlikely for the reported case (names differed) but the same symptom.

Also noted: `UserConfig::save_to` (`crates/keitora-tasks/src/config.rs:1122`) writes the
registry non-atomically, and the CLI's `config use` / `init` don't take
`UserConfig::write_lock()`; a torn read makes `resolve_active_in` fall through to
`registry_match(cwd)` / `NeedsOnboarding` (empty rail, not this symptom, but adjacent).

## Scope

**In**
- Tag the worktree list with the project it was resolved for (primary correctness guard).
- The warm registry / `activeWorktrees` only accept a list whose tag matches.
- Self-heal on a settled mismatch: reload both `project` and `worktrees`.
- Watch the user registry so silent active-project changes emit `project://changed`
  (fixes cause 1 at the source).
- Ordered loads: newest-settled-wins in `useLiveResource` and `loadProject()`.
- `buildGroups` renders rich lanes only for the active **registry key**.
- Atomic registry save; CLI registry writes take the write lock.
- Correct stale comments claiming the config watcher fires on a switch
  (`host.rs:269-271`, `src/lib/switchProject.ts:10-14`, `:62-66`).

**Out**
- Uniqueness enforcement for resolved names at registration (UX decision; the rail
  guard is enough to stop the mis-render).
- Tagging other project-scoped resources (tasks, sessions) — they inherit ordering only.

## Technical design

### 1. Backend: tagged worktree list
`worktrees` returns `ProjectWorktrees { project: Option<String>, worktrees: Vec<WorktreeView> }`.
`project` is the `pc.project.name` `worktrees_in` already resolves — the resolved name,
the same value as the frontend's `project.name` (`host.rs:112-114`) — or `None` with no
active project. One `resolve_active()` produces both, so they can't disagree.
Tests: extend `worktrees_carry_the_terminated_flag` (`host.rs:~2632`) to assert the tag;
the `None` case returns `project: None` and an empty list.

### 2. Backend: user-registry watcher
A watcher on the user registry file (`user_config_path()`, `config.rs:814`) using the
existing `db_watch::watch_dir`. On change it compares the registry's `active` against
the last value it saw and, only when it differs, runs the same path as
`set_active_project` minus the write (`on_project_activated` + `restart_project_watchers`
+ the four change emits). Extract that tail of `set_active_project` into a shared fn so
both callers stay in sync. Started once at app setup; lives for the app's lifetime.
Tests: the "did the active project change" decision is a pure fn, unit-tested
(same / changed / became none / was none).

### 3. Registry write safety
- `UserConfig::save_to`: write to a sibling temp file, then rename over the target
  (atomic on the same filesystem). Test: save then load round-trips; no temp file left.
- CLI `cmd_config_use` and `init`'s registry update take `UserConfig::write_lock()`,
  matching `set_active_project` and `migration.rs:270`.

### 4. Frontend: tagged list + guards (`src/lib/ipc.ts`, `src/App.tsx`)
- `listWorktrees(): Promise<ProjectWorktrees>`; `worktrees` state holds the tagged value.
- A pure helper in `src/lib/warmProjects.ts`,
  `warmDecision(projectName, tagged, inFlight) → "warm" | "skip" | "resync"`:
  - `inFlight` (a switch is still awaiting the backend) → `skip`;
  - tag matches → `warm`;
  - both non-null and different → `resync`;
  - else `skip`. Unit-tested.
- Warm effect (`App.tsx:980`) acts on the decision. On `resync`, call
  `loadProject()` **and** `reloadWorktrees()` once per mismatched `(project, tag)` pair
  (a ref holds the last pair; re-arm only when it changes), so it cannot loop.
- `inFlight` is a counter ref incremented at the start of `handleSwitchProject` and
  decremented in its `finally` — **not** `switchingTo`, which the 4 s safety net clears
  while the switch may still be in flight.
- `activeWorktrees` (`App.tsx:158`) falls back to the raw list only on a tag match, else `[]`.
- "Keep selected valid" (`App.tsx:1044`) skips while the tag mismatches, so a transient
  mismatch can't null the selection.
- Loading / empty gates (`App.tsx:~1132`) read `.worktrees` and require a tag match
  before showing `WorktreePicker`.
- `KEITORA_PROJECT` overrides the registry; both reads agree under it, so no heal
  is needed there — document it on the helper.

### 5. Frontend: ordered loads
- `subscribeLive` (`src/lib/useLiveResource.ts`): every load (initial, event, tick)
  takes an increasing ticket; a result (data or error) is applied only if its ticket is
  greater than the last **settled** ticket. A later load still in flight does not block
  an earlier one (no starvation); a later one that already settled does.
  `lastRevision` advances only for an applied result.
- The hook's manual `reload()` shares that ticket sequence: `subscribeLive` returns
  `{ teardown, reload }`, or the hook wraps `load` with a ticket held in a ref,
  scoped per subscription so a re-subscribe (e.g. `useTasks` on a `load` change) resets
  it. Pick the smaller diff during sequencing; keep existing `subscribeLive` tests
  passing.
- `loadProject()`: same newest-settled-wins via a sequence ref.
- Tests in `src/lib/useLiveResource.test.ts`: slow early load landing after a fast later
  one is dropped; a later in-flight load doesn't block an earlier one; a dropped result
  doesn't advance `lastRevision`; manual reload and tick are mutually ordered.

### 6. Rail: active by key (`src/lib/railGroups.ts`)
`BuildGroupsInput` gains `activeKey`; `rich` requires `key === activeKey` (in addition to
the resolved-name match). `isActive` stays resolved-name based for sessions/daemon data.
Test: two projects with the same resolved name — only the active key renders rich lanes.

## Dependencies
None. Frontend and backend ship together; no protocol bump.

## Risks
- Registry watcher feedback loop: the app's own `set_active_project` / reorder writes will
  also fire it. The "active changed" comparison makes those no-ops for reorders; for a
  switch, the duplicate reseed is idempotent (and the shared tail is cheap). Seed the
  watcher's "last seen" from each `set_active_project` call so the app's own switch
  doesn't double-run `on_project_activated`.
- Atomic rename on Windows: `std::fs::rename` replaces an existing file on Windows too;
  verify against a file held open by a concurrent reader (retry once on
  `PermissionDenied`).
- The UI mock harness (`.harness/app.tsx:348`) returns `PROJECTS[0]`'s worktrees
  regardless of the active project — it must return the tagged shape.

## Verification
- `cargo test --workspace`, `yarn test`, `tsc --noEmit`.
- UI mock harness: make the `worktrees` handler return a list tagged for a project other
  than the active one, and confirm the rail does not mis-file it and a resync is
  requested. No herdr contact.

## Open questions
- None blocking. Complexity: **small → medium** (≈10 files, frontend + backend + CLI);
  still one concern, so Fracture / Singletonize / Loss check stay skipped.
