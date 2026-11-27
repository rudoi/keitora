# 06 — Switching agent

## Goal

Let a worktree be switched to or from Antigravity at any time. The agent changes; the conversation does not
carry over; the user is warned first. Transfer stays restricted to its two existing pairs.

## Context

- Decision 4: conversation transfer to and from Antigravity is out of scope. Switching **is** in scope.
- Out of scope: transfer of the conversation to or from Antigravity. The follow-up is a summary handoff
  through the existing `inject_handoff_summary` path, and possibly import later over `agy acp`; file a task
  for it (doc 08).
- From doc 01: `CliKind::Antigravity`, `cliTitle`, `transferTargets`, `switchTargets`, `TRANSFER_PAIRS`. From
  doc 02: `ProviderIds` and `persist_worktree_cli(project, worktree, cli, ids)`. From docs 03/04:
  `antigravity_hooks::sync_worktree` (hook install plus trust seeding) and the `available` map.
- Step 6 of the implementation outline.
- Verified code state (before this work), `src-tauri/src` unless noted:
  - `transfer.rs`: `TransferOutcome` `:192-204` (serde `tag = "kind"`, `rename_all = "snake_case"`,
    `rename_all_fields = "camelCase"`; variants `Imported`, `Fresh`, `Done`, `Cancelled`),
    `preview_claude_transfer` `:380-393`, `load_project` `:407`, `transfer_worktree` `:427-474` (begin at
    `:441`, `refuse_terminated` `:476` and call `:451`, `with_recovery_lock` `:485`, the `match target_cli` at
    `:461`), `resume_record_for` `:537`, `persist_under_gate` `:651` (`pub(crate)`),
    `CommitCancel` `:664`, `commit_then_persist` `:683` (`pub(crate)`), `record_resume` `:~698` (private),
    `release_codex_session` / `release_codex_session_with` `:956-962` with consts `WRITER_RELEASE_TIMEOUT` /
    `WRITER_RELEASE_POLL` (private, `:~940`), `persist_worktree_cli` `:2461`.
  - `transfer_registry.rs`: `TransferRegistry::begin` `:215`, `mark_committed` `:291`, `is_transferring` `:310`,
    `settle` `:341`.
  - `dispatch.rs:631-635` `transfer_refusal` (text pinned by the tests at `:642-682`).
  - `host.rs`: `launch_for_spawn` `:1371-1387` (strict launch for Codex only, `log::info!` at `:1377`);
    command registration `crate::transfer::preview_claude_transfer`, `transfer_worktree`, `transfer_settled` at
    `:2535-2539`.
  - Frontend: `laneState.ts:49-116` (`WorktreeMenuItem` string union `:49-57`, `worktreeMenuItems` `:~62-72`,
    `laneMenuAction` `:~74-92`, `transferDisabledReason` `:~95-103`, `sessionItemDisabledReason` `:~108-116`);
    `Rail.tsx:357-368` (`WorktreeMenuState` with `cli`), `:370` `WORKTREE_ITEM_LABEL`, `:376-440` the menu
    component, `:691-701` `pickLaneItem`; `pendingIntent.ts:10-17`; `App.tsx` `bumpRestartNonce` call `:565`,
    `onTransfer` `:643-715`, `runPendingAction` `:772-799`; `transferPhase.ts` (whole file);
    `TransferOverlay.tsx:49-61`; `ipc.ts:1126-1131` `TransferOutcome`, `:1160` `transferWorktree`.

## Files to touch

Rust:

- new `src-tauri/src/agent_switch.rs`; `src-tauri/src/lib.rs` (`mod agent_switch;`)
- `src-tauri/src/transfer.rs`, `src-tauri/src/dispatch.rs` (`:631-635` and its three tests),
  `src-tauri/src/host.rs` (register command at `:2536-2539`; `launch_for_spawn` log level)

TypeScript:

- `src/lib/ipc.ts`, `src/lib/laneState.ts`, `src/lib/laneState.test.ts`, `src/lib/pendingIntent.ts`,
  `src/lib/transferPhase.ts`, `src/lib/transferPhase.test.ts`, new `src/lib/agentSwitch.ts` and
  `src/lib/agentSwitch.test.ts`
- `src/components/Rail.tsx`, `src/components/TransferOverlay.tsx`, `src/App.tsx`

## Design

### Transfer stays restricted to its two pairs

`transfer_worktree` (`transfer.rs:427`) today matches only on the target, so an Antigravity source with a Codex
target would run the Claude->Codex import on the wrong transcript.

1. New in `transfer.rs`:

   ```rust
   /// The worktree's effective provider: its registry entry's `cli`, else the user default, else Claude.
   pub(crate) fn source_cli(user: &UserConfig, config: &ProjectConfig, worktree: &str) -> CliKind;
   /// Only (Claude -> Codex) and (Codex -> Claude) are transfer pairs.
   pub(crate) fn is_transfer_pair(from: CliKind, to: CliKind) -> bool;
   /// Err("transfer isn't supported between {From} and {To}; use Switch") unless `is_transfer_pair`.
   pub(crate) fn check_transfer_pair(from: CliKind, to: CliKind) -> Result<(), String>;
   ```

   (`{From}` / `{To}` are `spec().title`, so the message reads `transfer isn't supported between Antigravity and
   Codex; use Switch`.)
2. `transfer_worktree`: after `refuse_terminated` (`:451`) and before building `TransferCtx`, add
   `let (user, config) = load_project(&project).await?; check_transfer_pair(source_cli(&user, &config,
   &worktree), target_cli)?;`. The early `Err` drops `guard`, which releases the marker (nothing committed).
   The `match target_cli` (`:461`) gains `CliKind::Antigravity => unreachable!("check_transfer_pair rejects it")`
   so it stays exhaustive.
3. `preview_claude_transfer` (`:380`): inside the `off_runtime` closure, after `resolve_project`, add
   `let source = source_cli(&user, &config, &worktree); if source != CliKind::Claude { return Err(format!(
   "a transfer preview needs a Claude session; this worktree runs {}", source.spec().title)); }`.
4. `refuse_terminated(client, key)` (`:476`) becomes `pub(crate) fn refuse_terminated(client: &dyn PtyBackend,
   key: &SessionKey, verb: &str) -> Result<(), String>` returning `"session is terminated; launch it before
   {verb}"`. The transfer call passes `"transferring"` (its text is unchanged); the test at `:3387` still passes.
5. The `begin` error (`:441`) becomes `"a transfer or switch is already in progress for this worktree"`.
6. `dispatch.rs:633` becomes `format!("a transfer or switch is in progress for worktree `{worktree}`; try again
   when it finishes")`; the tests `transfer_refusal_message_is_exact` (`:680`) and the `contains("in progress")`
   test (`:647`) are updated to the new text.

### Backend: `src-tauri/src/agent_switch.rs`

```rust
#[tauri::command]
pub async fn switch_worktree_cli(
    project: String,
    worktree: String,
    target_cli: CliKind,
    gate: State<'_, DispatchGate>,
    client: State<'_, SharedBackend>,
    registry: State<'_, TransferRegistry>,
    app: tauri::AppHandle,
) -> Result<TransferOutcome, String>
```

registered next to `transfer_worktree` in `host.rs`'s handler list (`:2536`). It returns the new variant
`TransferOutcome::Switched { from: CliKind, to: CliKind }` (add to the enum at `transfer.rs:192`; serialized
`{"kind":"switched","from":"claude","to":"antigravity"}`), so the frontend's existing outcome, restart,
`transfer_settled` and `worktrees://changed` path is reused. There is no new event. `changes_worktrees`
(`transfer.rs:519`) already treats every outcome except `Cancelled` as a change (add `Switched` to its test at
`:6419`).

Pure pieces (unit-tested):

```rust
/// Refusals that need no I/O. Messages are exact.
pub(crate) fn check_switch(current: CliKind, target: CliKind, target_available: bool) -> Result<(), String>;
//   target == current          -> "this worktree already runs {Title}"
//   is_transfer_pair(c, t)     -> "switching between {From} and {To} isn't supported; use Transfer"
//   !target_available          -> "{Title} isn't installed"
//   (checked in that order)

pub(crate) struct SwitchPlan { pub cli: CliKind, pub ids: ProviderIds, pub resume: ResumeRecord }
/// cli = target; ids = ProviderIds::keep().with(target, FieldIntent::Clear) (the SOURCE's id is kept);
/// resume = ResumeRecord::Cleared.
pub(crate) fn switch_plan(target: CliKind) -> SwitchPlan;
```

Command body, in this order:

1. `let guard = registry.begin(&project, &worktree).map_err(|_| "a transfer or switch is already in progress for
   this worktree".to_string())?;` (declared first so it outlives everything; it releases the marker unless the
   switch committed) and `let key = SessionKey { project, worktree }`.
2. `refuse_terminated(client.inner().as_ref(), &key, "switching")?` (the lane is terminated -> refused; the UI
   never offers Switch on a terminated lane, but the backend enforces it).
3. `let (user, config) = load_project(&project).await?; let current = source_cli(&user, &config, &worktree);`
4. `let available = resolve_provider_path(target_cli, Some(&config), &user).is_some() ||
   discover_cli_cached(target_cli).is_some(); check_switch(current, target_cli, available)?;`
5. `with_recovery_lock(client.recovery(), &key, async { … })` containing:
   - stop the live pane: `release_session_with(client.as_ref(), &key, WRITER_RELEASE_TIMEOUT,
     WRITER_RELEASE_POLL)` (the generic rename of `release_codex_session_with` at `transfer.rs:962`; update its
     caller `release_codex_session` at `:957` and the test helper `fast_release` at `:6150`. The error strings
     inside it that mention Codex are left unchanged, because the test at `:6224` asserts one of them). Make
     `WRITER_RELEASE_TIMEOUT` (`:941`), `WRITER_RELEASE_POLL` (`:943`), `release_session_with`,
     `with_recovery_lock` (`:485`) and `record_resume` (`:696`) `pub(crate)`.
   - persist under the dispatch gate:
     `persist_under_gate(&gate, move || commit_then_persist(&registry, &project, &worktree,
     CommitCancel::Ignore, || { persist_worktree_cli(&config, &worktree, plan.cli, plan.ids.clone())?;
     record_resume(client.as_ref(), &key, plan.resume.clone()); Ok(()) }))`, with `plan = switch_plan(target_cli)`.
     Clearing the target's id stops a stale id from before an earlier switch from being resumed on the next boot.
     `Committed::Cancelled` is unreachable with `Ignore` and maps to an `Err("switch was cancelled")`.
6. If `target_cli == CliKind::Antigravity`: `antigravity_hooks::sync_worktree(&config.worktree_path(&worktree))`
   on a `std::thread::spawn` (hook install plus trust seeding, docs 04/03), then the backend's existing
   `refresh_integration()` (README Open issue 5 resolution), so a late `agy` install gets herdr's
   `antigravity-cli` integration without a restart.
7. `app.emit("worktrees://changed", ())` and return `Ok(TransferOutcome::Switched { from: current, to:
   target_cli })`.

**Fresh launch.** "Fresh" is already a per-spawn argument: `pty_spawn(fresh)` -> `AgentLaunch.fresh` ->
`ResumeStore::resolve` returns `Fresh`. The frontend restarts with `fresh = true`, so switching back to Claude
does not `Continue`. No new backend flag is needed.

**Strict launch on restart.** The restart passes `transfer: true` (`App.tsx:565`, `bumpRestartNonce(…, true)`)
so the pane skips its confirm-kill prompt. The backend's `launch_for_spawn` (`host.rs:1371`) applies strict
launch only to Codex, so a switch to Codex takes the same strict-fresh path as the existing "fresh" transfer
outcome. Demote the log line at `:1377` (`"transfer: strict launch applies to codex only"`) from
`log::info!` to `log::debug!` so switches do not produce noise.

### Frontend menu model (`laneState.ts`, `Rail.tsx`)

`WorktreeMenuItem` is a flat string union today, and `laneMenuAction(item, transferTo)` carries one target, so
two Switch items would collide. New shapes:

```ts
export type WorktreeMenuItem =
  | { kind: "focus" | "restart" | "fresh" | "terminate" | "launch" | "launch-fresh" }
  | { kind: "transfer" | "switch"; to: CliKind };
/** React key: `${kind}:${to}` for transfer/switch, else `kind`. */
export function menuItemKey(item: WorktreeMenuItem): string;
export function worktreeMenuItems(
  cli: CliKind, terminated: boolean, opts?: { inactive?: boolean },
): WorktreeMenuItem[];
export function laneMenuAction(item: WorktreeMenuItem): PendingAction | null;
export type LaneAction = "transfer" | "switch";
export function actionDisabledReason(
  action: LaneAction, to: CliKind, available: Record<CliKind, boolean>, busy: boolean,
): string | null;
export function sessionItemDisabledReason(item: WorktreeMenuItem, transferBusy: boolean): string | null;
```

- `worktreeMenuItems`: terminated -> `[launch, launch-fresh]` (terminated lanes only offer launch items, as
  today, `laneState.ts:67-69`; they cannot be switched). Otherwise `[restart, fresh, …transferTargets(cli)
  .map((to) => ({kind:"transfer", to})), …switchTargets(cli).map((to) => ({kind:"switch", to})), terminate]`.
  `inactive` prepends `focus` as today. Resulting menus: from Claude: Transfer to Codex, Switch to Antigravity;
  from Codex: Transfer to Claude, Switch to Antigravity; from Antigravity: Switch to Claude, Switch to Codex.
- `laneMenuAction`: `transfer` -> `{kind:"transfer", to}`; `switch` -> `{kind:"switch", to}`; others as today.
- `actionDisabledReason`: `busy` wins -> `"A transfer is already in progress"` for `transfer` and `"A switch is
  already in progress"` for `switch` (the busy copy names the action); else `!available[to]` ->
  `` `${cliTitle(to)} isn't installed` ``; else `null`. Every target, Claude included, is gated by `available[to]`;
  today's "Claude is never unavailable" special case (`Rail.tsx:394-398`) goes away.
- `sessionItemDisabledReason`: kills = kind in `restart|fresh|terminate`; copy becomes `"A transfer or switch is
  in progress"` (the busy marker is shared).
- `Rail.tsx`: the menu component receives `available: Record<CliKind, boolean>` (doc 03) instead of
  `codexAvailable`, drops the `targetCli` computation (`:393-399`), iterates `items.map(item)` with
  `key={menuItemKey(item)}`, labels transfer/switch items `` `Transfer to ${cliTitle(item.to)}` `` /
  `` `Switch to ${cliTitle(item.to)}` `` (the `WORKTREE_ITEM_LABEL` record covers only the other kinds), and
  `onPick: (state, item) => void` (no `targetCli`). `pickLaneItem` (`:691`) calls `laneMenuAction(item)`.
  The `focus` separator check becomes `item.kind === "focus"`. `worktreeMenuItems(state.cli, state.terminated,
  { inactive: state.inactive })` replaces the call at `:400`.

### Pending actions

`PendingAction` (`pendingIntent.ts:10-17`) gains `| { kind: "switch"; to: CliKind }`. `runPendingAction`
(`App.tsx:772`) keeps its early `if (action.kind === "new-worktree") { setWtOpen(true); return; }`, so inside
the `switch (action.kind)` that follows, `action` is narrowed to the remaining kinds. Add
`case "switch": onSwitch(worktree, action.to); break;` and a final
`default: { const unreachable: never = action; return unreachable; }` (the exhaustive `never` check), so a
future `PendingAction` kind fails to compile and lanes in non-active projects (which route through
`runPendingAction` after a project switch) never drop a switch silently. Add `onSwitch` to the `useCallback`
dependency array.

### `onSwitch` (`App.tsx`)

Sits next to `onTransfer` (`:643`); shares only the focus, key and guard prelude:

```ts
const onSwitch = useCallback((worktree: string, to: CliKind) => {
  setSelected(worktree); setView("focus");
  const project = warmRef.current.active; if (project === null) return;
  const key = transferKey(project, worktree);
  if (!shouldStartTransfer(transferCtl.getMap(), key)) { showToast("A transfer or switch is already in progress for this worktree."); return; }
  const from = activeWorktrees.find((w) => w.name === worktree)?.cli;
  if (from === undefined) { showToast(`Worktree ${worktree} not found.`); return; }
  void (async () => {
    const copy = switchConfirmCopy(from, to);
    const ok = await confirmDialog({ title: copy.title, body: copy.body, confirmLabel: copy.confirmLabel });
    if (!ok) return;                                   // confirm is NOT `danger`: nothing is destroyed
    if (!shouldStartTransfer(transferCtl.getMap(), key)) return;
    transferCtl.dispatch({ type: "begin", project, worktree, to, mode: "switch", now: Date.now() });
    subscribeResume(key, project, worktree);
    try { transferCtl.dispatch({ type: "outcome", key, outcome: await switchWorktreeCli(project, worktree, to) }); }
    catch (e) { transferCtl.dispatch({ type: "failed", key, error: String(e) }); }
  })();
}, [showToast, confirmDialog, transferCtl, subscribeResume, activeWorktrees]);
```

`switchWorktreeCli(project, worktree, targetCli): Promise<TransferOutcome>` is new in `ipc.ts` next to
`transferWorktree` (`invoke("switch_worktree_cli", { project, worktree, targetCli })`); `TransferOutcome`
(`ipc.ts:1126`) gains `| { kind: "switched"; from: CliKind; to: CliKind }` and its doc comment a `switched`
line. The confirm copy comes from the new pure `src/lib/agentSwitch.ts`:

```ts
export function switchConfirmCopy(from: CliKind, to: CliKind): { title: string; body: string; confirmLabel: string };
//  title:        `Switch to ${cliTitle(to)}?`
//  body:         `${cliTitle(to)} will start a new conversation. The current ${cliTitle(from)} conversation won't be carried over; it stays on disk, and switching back later starts fresh too.`
//  confirmLabel: `Switch to ${cliTitle(to)}`
```

For `from = claude`, `to = antigravity` the body is exactly: "Antigravity will start a new conversation. The
current Claude conversation won't be carried over; it stays on disk, and switching back later starts fresh
too."

### Reducer (`transferPhase.ts`)

- `TransferEntry` gains `mode: "transfer" | "switch"`. `TransferResult` gains `| { kind: "switched" }`.
  `TransferEvent` `begin` gains an optional `mode?: "transfer" | "switch"` (default `"transfer"`).
- `begin` with `mode: "switch"` creates the entry directly in phase `"restarting"` with `result: null` (no
  `importing`), so the existing 120-second backstop (`syncTimers` arms a timer for every `restarting` entry)
  covers the backend call plus the restart. `begin` with `"transfer"` is unchanged.
- A switch entry is "awaiting its outcome" while `mode === "switch" && phase === "restarting" && result ===
  null`. Define `const awaitingOutcome = (e) => e.phase === "importing" || (e.mode === "switch" && e.phase ===
  "restarting" && e.result === null)` and use it as the guard in the `outcome` and `failed` cases instead of
  `e.phase !== "importing"`. (This resolves the proposal's apparent conflict between "enters restarting
  directly" and "the `switched` outcome gives a restart effect"; see README Open issue 8.)
- The `outcome` case gains `case "switched":` which, only when `e.mode === "switch"`, sets `result: {kind:
  "switched"}` (phase stays `"restarting"`) and returns effects `[{ kind: "restart", ...target, fresh: true }]`.
  A `switched` outcome for a `transfer`-mode entry is a no-op. A second `outcome` for the same entry is a no-op
  (`result !== null`).
- `failed` on a switch entry: `finish(map, key, e, event.error, null)` (no diagnostics).
- `cancel_requested` stays a no-op for a switch (its guard already requires `phase === "importing"`), and
  `codexDiagnostics(e)` returns `null` when `e.mode === "switch"` (so switches get no Codex diagnostics). The
  `spawn_settled` failure and `backstop` cases go through `codexDiagnostics`, so they inherit that.
- `spawn_settled` for a switch entry whose `result` is still `null` is a no-op (the restart has not been
  requested yet).
- `resultToast`: `switched` -> `` `Switched ${entry.worktree} to ${cliTitle(entry.to)}.` ``
  (`"Switched {worktree} to {Title}."`); `backstopMessage` already uses the target title.
- `blocksInput(e)`: `e?.phase === "importing" || (e?.mode === "switch" && e.result === null)`.
- `resume_notice` is unchanged (it requires `e.to === "codex"` and `result.kind === "imported"`).

### Overlay (`TransferOverlay.tsx`)

`TransferOverlay` reads `entry.mode`. For `mode === "switch"` it renders the spinner with
`aria-label="Switching"` and, while `entry.result === null`, `<div className="pane-msg">Switching to
{cliTitle(entry.to)}…</div>`; once `entry.result !== null` (restarting) it shows `Starting {cliTitle(entry.to)}…`
as the transfer overlay does. It never renders the importing view, progress lines, elapsed time or Cancel for a
switch. For `mode === "transfer"` the markup is unchanged except `cliLabel` -> `cliTitle` and the spinner
`aria-label="Transferring"` stays. Replace the `cliLabel` import (`:10`) with `cliTitle` from
`../lib/providers`; `showCancel` (`:36`) additionally requires `entry.mode === "transfer"`.

## Tests required

Rust (`agent_switch.rs` `mod tests`, `transfer.rs`, `dispatch.rs`):

- `check_switch_refuses_same_cli` — `check_switch(Claude, Claude, true) == Err("this worktree already runs Claude")`.
- `check_switch_refuses_transfer_pairs` — `(Claude, Codex)` and `(Codex, Claude)` ->
  `Err("switching between Claude and Codex isn't supported; use Transfer")` / the Codex-first text.
- `check_switch_refuses_unavailable_target` — `(Claude, Antigravity, false) == Err("Antigravity isn't
  installed")`.
- `check_switch_allows_the_four_switch_pairs` — Claude->Antigravity, Codex->Antigravity, Antigravity->Claude,
  Antigravity->Codex with `available = true` are `Ok(())`.
- `check_switch_checks_same_before_pair_before_availability` — `(Claude, Claude, false)` reports the same-cli
  message.
- `switch_plan_clears_target_id_and_keeps_source` — `switch_plan(Antigravity)`: `cli == Antigravity`,
  `ids.antigravity == Clear`, `ids.claude == Keep`, `ids.codex == Keep`, `resume == Cleared`; and
  `switch_plan(Claude)`: `ids.claude == Clear`, others `Keep`.
- `switch_persists_cli_and_clears_target_id` — against a temp registry with a worktree on Claude holding
  `claude_session_id = "s1"` and a stale `antigravity_conversation_id = "old"`: after
  `persist_worktree_cli(…, switch_plan(Antigravity))` the entry has `cli == Antigravity`, `claude_session_id ==
  Some("s1")`, `antigravity_conversation_id == None`.
- `switch_refuses_while_a_transfer_is_running` — with `registry.begin(p, w)` held, a second `begin` fails and
  `transfer_refusal` returns the new "transfer or switch" text.
- `switch_refuses_a_terminated_lane` — `refuse_terminated(&fake, &key, "switching")` returns
  `"session is terminated; launch it before switching"` for a terminated key.
- `transfer_outcome_switched_serializes_as_documented` — `serde_json::to_value(TransferOutcome::Switched { from:
  Claude, to: Antigravity })` equals `{"kind":"switched","from":"claude","to":"antigravity"}`.
- `is_transfer_pair_is_exactly_the_two_pairs` — true for (Claude,Codex), (Codex,Claude); false for the other
  seven ordered pairs.
- `check_transfer_pair_refuses_antigravity_source` —
  `check_transfer_pair(Antigravity, Codex) == Err("transfer isn't supported between Antigravity and Codex; use
  Switch")`; `(Claude, Codex)` and `(Codex, Claude)` are `Ok`.
- `source_cli_follows_entry_then_default` — entry `cli` wins; else `user.default_cli`; else Claude.
- `preview_refuses_a_non_claude_source` — the guard logic for `source_cli != Claude` returns the documented
  message (test the pure condition via `source_cli`).
- `release_session_with_*` — the existing Codex tests at `:6150` pass under the renamed function.
- dispatch: `transfer_refusal_message_is_exact` expects the new text; `transfer_refusal_blocks_only_the_
  transferring_worktree` still passes.

TypeScript:

- `laneState.test.ts`:
  - `worktreeMenuItems("claude", false)` -> `[restart, fresh, {transfer,codex}, {switch,antigravity}, terminate]`
    as objects; `("codex", false)` -> transfer to claude, switch to antigravity; `("antigravity", false)` ->
    `[restart, fresh, {switch,claude}, {switch,codex}, terminate]`; terminated -> `[launch, launch-fresh]` for
    each provider; `inactive` prepends `focus`.
  - `menuItemKey` is unique across the three-provider menus and equals `switch:claude` / `transfer:codex` /
    `restart`.
  - `laneMenuAction({kind:"switch", to:"antigravity"})` -> `{kind:"switch", to:"antigravity"}`; the existing
    transfer/restart/terminate/launch cases adapted to object items; `focus` -> `null`.
  - `actionDisabledReason`: busy beats unavailable (`("transfer","codex",{codex:false,…},true)` -> `"A transfer
    is already in progress"`); `("switch","antigravity",…,true)` -> `"A switch is already in progress"`;
    unavailable -> `"Antigravity isn't installed"`; `("transfer","claude",{claude:false,…},false)` ->
    `"Claude isn't installed"`; all available and idle -> `null`.
  - `sessionItemDisabledReason`: kills are disabled with `"A transfer or switch is in progress"` while busy.
- `agentSwitch.test.ts`: `switchConfirmCopy("claude","antigravity")` equals the exact strings above;
  `("antigravity","codex")` names Codex and Antigravity.
- `transferPhase.test.ts`:
  - `begin switch enters restarting with no result` — entry `mode "switch"`, `phase "restarting"`, `result null`.
  - `switched outcome requests a fresh restart` — after `begin` switch, `outcome {kind:"switched"}` returns
    effects `[{kind:"restart", fresh:true, …}]` and the entry has `result {kind:"switched"}`.
  - `switched outcome is ignored for a transfer entry` and `a second outcome is ignored`.
  - `switch failure finishes with the error and no diagnostics` — `failed` yields a toast with
    `diagnostics: null` and a `settle` effect.
  - `switch spawn_settled ok toasts "Switched alpha to Antigravity."` and settles (no `inject_handoff`).
  - `switch backstop fires after RESTARTING_BACKSTOP_MS` (controller test with `backstopMs` injected) and its
    toast names Antigravity.
  - `cancel_requested is ignored for a switch`.
  - `blocksInput is true while a switch awaits its outcome and false after`.
- `pendingIntent.test.ts`: a `{kind:"switch", to:"antigravity"}` action type-checks and survives
  `intentForSwitch`/`canConsume` like the other actions.
- App-level (pure): a `runPendingAction`-style exhaustiveness test is the compile-time `never` check; add a test
  that `laneMenuAction` output kinds are a subset of `PendingAction["kind"]`.

## Cross-references

- 01 (provider helpers, `cliTitle`), 02 (`ProviderIds`, `persist_worktree_cli`), 03 (`available`, trust
  seeding), 04 (`sync_worktree`), 07 (settings UI unaffected; overlay styles), 08 (follow-up task for
  transfer).
