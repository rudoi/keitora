# 0078 cross-worktree driving

Status: proposal (refine cycle 2). Placeholder names only: driver `alpha`, driven `bravo`/`charlie`,
project `my-game`. The "Implementation notes" section at the end wins over earlier text.

## Goal

Let one worktree's agent session **drive** other worktrees in the same project through the
`keitora` CLI. The driver can:

- prompt them,
- wait on them and read what they said,
- see their pending permission prompts and questions, and answer each one or escalate it to the
  human,
- reset them to a fresh conversation, optionally on another provider, or swap their provider
  while carrying context.

The rail shows who is driving whom. The human can always take back control.

The target use is an orchestrator session, such as `alpha` running `/workflow` or a fan-out
plan, that hands work to `bravo` and `charlie`, keeps them moving, and calls the human only when
a decision really needs one.

## Scope

**In (v1)**
- `keitora drive <wt>` / `keitora release <wt>`: an explicit, persistent driver→driven binding.
  There is one driver per driven worktree, and a driver may drive any number of worktrees.
  **One level only:** a driven worktree can't drive, a driver can't be driven, and self-drive is
  rejected.
- Free-form prompting of a driven worktree, plus wait/read of its state and last response.
- **Prompt brokering**: permission requests and `AskUserQuestion` from a driven session go to the
  driver, which answers or escalates them. If the driver doesn't answer, the prompt escalates to
  the human automatically. It is never auto-approved.
- **Reset and swap.** `keitora reset <wt> [--to <provider>]` starts a fresh conversation,
  optionally on a different provider, in one step. Drivers mostly want fresh sessions. A
  context-carrying swap through the existing transfer engine stays available as
  `keitora swap <wt> --to <provider>`. Both take a provider name rather than "the other one", so
  the third provider being added elsewhere (`agy`) slots in for `reset`; `swap` supports only the
  pairs the transfer engine supports (Claude and Codex today).
- Rail: a driver/driven relationship badge, a distinct "driver deciding" state versus the
  escalated "needs you" state, and Take over / Release / Pause actions.
- Human override: you can still type in a driven pane and nothing is held back, but the pane
  warns you that it is driven (see Rail UI). A rail action pulls a pending prompt back to you or
  ends the drive.
- v1 drives only worktrees of a project that is open (warm) in the Keitora window, because
  `send`, `reset` and `swap` are executed through the live panes. Prompt brokering, `answer`,
  `wait`, `prompts` and `drives` are DB-only and work with the window closed.

**Out (v1)**
- Cross-project driving. A drive binding lives in one project's `tasks.db`.
- Push-notifying the driver mid-turn. The driver polls with `keitora wait`. A later version could
  deliver events through Claude's cross-session inbox or Channels.
- Standing auto-answer policies such as "auto-allow edits in bravo". The driver decides each
  prompt. A policy layer can sit on the same broker later.
- Codex `request_user_input` brokering. It only exists in Plan mode, and no hook sees it (see
  Risks).
- Telling the driven agent it is driven (no clear gain).
- Authentication between worktrees. Same trust model as `task dispatch`: anyone who can write
  `tasks.db` can drive, and the caller identity is just the git toplevel name.

## Paradigm

```
 alpha (driver session)                      Keitora app                         bravo (driven pane)
 ─────────────────────                      ───────────                         ───────────────────
 keitora drive bravo        ──tasks.db──▶   drives row  ──activity──▶ rail: bravo ⇠ alpha
 keitora send bravo "…"     ──request──▶    dispatch-style watcher ──write_line──▶ turn starts
 keitora wait bravo         ◀──poll──       session_replies(turn, last_message)  ◀── Stop hook
                                                                                   │
                                            drive_prompts row ◀── PermissionRequest/PreToolUse hook
 keitora wait → {prompt}    ◀──poll──       (hook blocks, polling for a decision)   │
 keitora answer bravo 7 allow ──tasks.db──▶ decision ──────────────────────────────▶ hook returns allow
 keitora answer bravo 8 escalate --reason "needs prod creds" ──▶ hook returns no decision ──▶ normal dialog,
                                                         rail pulses + toast "alpha escalated: …"
```

Four ideas carry the design:

1. **The CLI never touches herdr or panes directly.** Same rule as `task dispatch`
   (`crates/keitora-cli/src/main.rs:667-672`). Anything that writes to a pane, or needs live
   herdr state, goes in as a request row in `tasks.db`. The app's DB watcher applies it through
   the writer queue and gates (`src-tauri/src/dispatch.rs:534-629`), then writes the result back
   to the row. The CLI polls that row. Prompt brokering is the exception that needs no app: the
   hook and the driver's CLI talk through `tasks.db` alone.
2. **Prompts are brokered at the hook, not scraped from the terminal.** Claude Code and Codex both
   run a `PermissionRequest` hook that can return allow or deny and can block for minutes (600s
   default timeout, configurable). If the hook returns nothing, the normal interactive prompt
   appears. That "no decision" path is how escalation works. Keitora's hooks already run
   `keitora session <state>` (`src-tauri/src/hooks.rs:23-61`, `codex_hooks.rs`). They gain a
   brokering subcommand that does nothing unless the worktree is currently driven.
3. **Escalation is a first-class answer.** The driver's choices are `allow`, `deny`, `answer`
   (for questions), and `escalate`. The driver should escalate anything it cannot judge safely,
   such as destructive commands, credentials, scope changes, or ambiguity. The reason is shown
   to the human. A timeout always escalates and never approves.
4. **The relationship is state the app can see.** It lives in a `drives` table. Session rows and
   `activity` surface it, and the rail renders it. The rail's attention logic distinguishes
   "waiting on the driver" from "waiting on you".

## CLI surface

Top-level verbs, per your note that `drive attach` was redundant. Every verb resolves the caller
from the git toplevel (`crates/keitora-tasks/src/config.rs:2944`). Every verb except `drive` and
`drives` refuses unless the caller is the target's current driver. `--json` is available
everywhere.

| Command | Effect |
|---|---|
| `keitora drive <wt> [--escalate-after S] [--force]` | Bind caller → `<wt>`. Refuses if `<wt>` is already driven, if `<wt>` is the caller, if `<wt>` itself drives anything, or if the caller is driven (one level only). `--force` steals a driven worktree: the old driver's pending prompts escalate to the human and the rail notes the steal. `--escalate-after` sets this drive's broker timeout (default 300s). Warns when `<wt>`'s hook file lacks the gate entry (see Implementation notes). |
| `keitora release [<wt>\|--all]` | End the binding (caller must be the driver). Any pending prompts of `<wt>` escalate to the human at that point. The human can also release from the rail. |
| `keitora drives` | List bindings in the project: who drives whom, the driven state, and pending prompt counts. Open to any caller. |
| `keitora send <wt> <text> [--queue] [--wait]` | Send a free-form prompt. It is applied only when the *agent* is idle; otherwise it is refused, unless `--queue`, which leaves the row pending and retried on each DB change until idle or until it expires (10 min, the existing request expiry). It is not held back for human typing; the pane banner warns instead. `--wait` blocks until the request is applied or refused (like `task dispatch --wait`); it does not wait for the turn, `keitora wait` does. Prints the turn number the send will complete as. |
| `keitora wait <wt>… [--timeout S] [--stall-after S]` | Block until any listed worktree finishes the turn that its last `send` started (returns the `last_message`) or has a pending prompt (returns the prompt JSON). Also returns a `stalled` event when a busy worktree has had no hook activity for `--stall-after` seconds (default 120), so the driver can `read` the tail. `--timeout` ends the wait with exit code 2 and no state change. This is the driver's main loop primitive. |
| `keitora read <wt> [--lines N]` | Read the pane tail through an app request (`pty_read_history`, `src-tauri/src/host.rs:1823`). This is the fallback when the hook data isn't enough. |
| `keitora prompts [<wt>]` | List pending brokered prompts: id, kind, tool, input summary, age. |
| `keitora answer <wt> <id> allow\|deny\|escalate [--reason R] [--choice Q=A]…` | Resolve a brokered prompt. `--choice` fills `AskUserQuestion` answers. `deny` and `escalate` take `--reason`. |
| `keitora reset <wt> [--to <provider>] [--wait]` | Fresh conversation: persist the provider (if `--to`), then restart the pane fresh with no confirm. This is the expected common case. It needs no transfer, no import ledger and no confirm. |
| `keitora swap <wt> --to <provider> [--wait]` | Context-carrying swap through the existing transfer engine (see Technical design). The UI shows its normal confirm; the request reports `pending_confirm` until a human accepts or cancels. |

Possible later additions: `keitora stop <wt>`, `keitora launch <wt>`, and `keitora interrupt <wt>`
(send Esc). They are cheap once the request mailbox exists, but not required for v1.

## Technical design

### Data (crates/keitora-tasks, `tasks.db`)

Extend `crates/keitora-tasks` with the following. Do not fork the logic. New tables are
`CREATE TABLE IF NOT EXISTS` additions to `init_schema` (see Implementation notes for why no
`ALTER`).

- `drives(driven TEXT PK, driver TEXT, since, forced_from TEXT NULL, paused INTEGER,
  escalate_after_s INTEGER, awaiting_turn INTEGER NULL)`. Insert runs in one immediate
  transaction and enforces the one-level rule: the driver is not driven, the driven doesn't
  drive, and self-drive is rejected.
- `drive_prompts`, one row per brokered prompt:
  - `id`, `driven`, `driver`, and `provider`
  - `kind` (`permission`|`question`) and `tool_name`
  - `tool_input` (JSON, truncated) and `summary`
  - `status` (`pending|allowed|denied|answered|escalated|expired|withdrawn`)
  - `decision_json`, `decided_by` (`driver:<wt>`|`human`|`timeout`|`system`), and `reason`
  - `hook_pid`, `created_at` and `decided_at`
- `drive_requests`, generalising `dispatch_requests` (`crates/keitora-tasks/src/sqlite_backend.rs`,
  the `CREATE TABLE` at ~150-160) with a `kind` (`send|read|reset|swap`), a `payload`, a
  `result`, and a status set that adds `pending_confirm`. It is a separate table because
  `dispatch_requests.task_id` is NOT NULL. Reuse the same CAS claim, expiry and status machine.
- `session_replies(worktree PK, turn INTEGER, last_message TEXT, updated_at)`: a side table, not
  columns on `worktree_sessions`. `turn` increases by one per finished turn. `wait` uses it to tell
  a new idle from an old one.

All of this gets `#[cfg(test)]` coverage in the same change:
- binding exclusivity, one-level and self rejection, `--force` steal,
- prompt state transitions, timeout escalation, and withdrawal,
- release and pause escalating pending prompts,
- request lifecycle, including `--queue` expiry,
- opening an old-schema DB adds the new tables.

### Hook broker (`keitora session …` grows stdin)

Today `keitora session` ignores stdin (`crates/keitora-cli/src/main.rs:268-275`; the `Session`
variant takes only a `state`). The changes:

- **New hook entries.** Claude gets two:
  - `PermissionRequest` for all tools,
  - `PreToolUse` with matcher `AskUserQuestion`.

  Codex's existing `PermissionRequest` entry (today `session waiting`) is replaced by the gate.
  Each runs `keitora session gate`, which reads the hook payload from stdin. The gate ignores
  `AskUserQuestion` in `PermissionRequest` payloads so a question is brokered once, by the
  `PreToolUse` hook. Each gate hook entry carries a `timeout` (seconds) above the broker timeout.
- **Fast path.** If the worktree is not driven, `gate` exits 0 with no output, so behaviour is
  unchanged (and for Codex it still records `waiting`, as the entry it replaces did). The cost is
  one SQLite read.
- **Driven path.** `gate` inserts a `drive_prompts` row, marks the session `waiting`, and polls
  until the row reaches a terminal status or the broker timeout expires (default 5 min, per-drive
  override). It escalates at once if the driver's session is offline, the binding is released, or
  the drive is paused. When the prompt is decided by the driver it sets the session back to
  `busy`. It then prints the decision:

  | Decision | Claude `PermissionRequest` | Claude `AskUserQuestion` (`PreToolUse`) | Codex `PermissionRequest` |
  |---|---|---|---|
  | allow | `decision.behavior: allow` | — | `behavior: allow` |
  | deny | `behavior: deny` + message | `permissionDecision: deny` + reason | `behavior: deny` + message |
  | answer | — | `allow` + `updatedInput{questions, answers}` | — |
  | escalate / timeout | no output (normal dialog) | `permissionDecision: ask` | no output (normal approval) |

- **Stop hook.** `keitora session idle --stdin` (used by `Stop` only) reads `last_assistant_message`
  from the payload into `session_replies` and bumps `turn`. If the field is absent it falls back
  to the last assistant entry of the payload's `transcript_path`. `StopFailure` bumps `turn` with a
  fixed "turn ended with an error" message. No terminal scraping is needed for the common case.
- **Withdrawal.** Any other session-state write for a worktree (`busy`, `idle`, `offline`) marks its
  still-pending prompts `withdrawn`: the hook that owned them is gone (the user pressed Esc, or the
  agent exited).
- **Fail-safe.** Any broker error (DB locked, parse failure, unknown subcommand on an older CLI)
  acts as escalate. The hook must never convert an error into allow.

### App side

- **Request runner.** Generalise `dispatch.rs` `run_dispatch_requests` to also drain
  `drive_requests`, from the same two call sites (`src-tauri/src/host.rs:2303` and
  `src-tauri/src/bg_notifier.rs:226`, which already runs before the heartbeat skip). Each request
  runs in its own task so a long `reset` or `swap` doesn't block `send`:
  - `send` → `backend.write_line`, idle-only (`decide`), under `DispatchGate`, using the same
    `collapse` sanitising (`dispatch.rs:185`). Multi-line text goes through `paste` + enter. On
    apply it sets `drives.awaiting_turn` to the next turn.
  - `read` → `read_history`, with the result stored on the row, capped at 64 KB.
  - `reset` → refused while a transfer is in flight or the session is terminated. Otherwise:
    - if `--to`, write `cli` with `set_worktree_provider_state_by_root`
      (`crates/keitora-tasks/src/config.rs:1021`) and emit `worktrees://changed`; the spawn re-derives
      its launch from persisted state,
    - emit a `drive://restart` event to the window. The window runs the existing fresh-restart path
      (the rail's "fresh" menu entry, `bumpRestartNonce` with `fresh`) in a new no-confirm,
      reports-the-spawn mode modelled on the transfer restart, then calls `drive_request_resolve`
      with the spawn outcome. The pane knows its measured size and the backend owns cwd, so no
      headless spawn is needed. If the spawn fails after `--to`, the previous provider is restored
      and the request is refused with the reason.
  - `swap` → `drive://swap` runs the existing `onTransfer` flow in the window, including its
    confirm dialog, and reports the outcome through `drive_request_resolve`. A cancelled confirm
    refuses the request.
- **Window listener.** `App.tsx` handles the two events only when the request's project is in the
  warm registry (`warm.projects`); otherwise the runner refuses with "project not open in Keitora".
  The existing pending-intent switch path is deliberately not used: a driver must not switch the
  human's active project.
- **Why not move the transfer controller to the backend.** The restart, handoff injection and
  `transfer_settled` steps live in the frontend by design (`App.tsx:494-564, 643-709`; restarts
  always go through the mounted pane, never a backend same-key respawn). Moving that controller
  (`src/lib/transferPhase.ts`, ~450 lines plus tests, and the pane's spawn-settled plumbing) is a
  large, risky change that buys nothing for v1; events into the existing flow are ~100 lines.
- **Human commands.** Tauri commands `drive_release`, `drive_take_over`, `drive_pause`,
  `drive_request_resolve` and `drives_list` write the same tables the CLI does.
- **Session rows.** `WorktreeSession` (`crates/keitora-tasks/src/model.rs`) gains `driven_by`,
  `brokered` (a pending, undecided prompt exists) and `escalated_reason` (serde-defaulted), filled in
  `sessions_list`. `session_revision` includes them so `sessions://changed` fires; today it
  covers only state and task id, so drive writes would otherwise never reach the UI.
- **Activity.** `WorktreeActivity` gains `driven_by: Option<String>`, `drives: Vec<String>` and
  `pending_prompts: u32`, filled in `compute()` (`src-tauri/src/activity/mod.rs:168-239`; struct at
  44-65) from each project's `tasks.db` through a read-only handle. This feeds rail badges for every
  project on the existing 2.5s poll (`useWorktreeActivity`).
- **Attention.**
  - `src/lib/attention.ts:41-96`, `bg_notifier.rs:67-80, 282-299` (a Rust port of the same
    logic; change both and their tests together) and `laneState.ts` learn the sub-state
    "waiting on driver": a `waiting` session whose `brokered` flag is set classifies as no attention
    (no toast, no pulse, no dock badge) and gets the quiet "alpha deciding" block style.
  - A driven worktree never raises `finished` attention either: the driver reads the turn through
    `wait`.
  - An *escalated* prompt clears `brokered`, so the session reads as ordinary `waiting`: pulse plus
    toast. The toast body becomes "alpha escalated: <reason>" from `escalated_reason`.
- **Human override.**
  - Typing into a driven pane is always allowed.
  - Rail actions:
    - **Take over** marks the pending prompts `escalated (human)`. The blocked hook then returns,
      and the native dialog appears in the pane.
    - **Release** ends the binding.
    - **Pause driver** keeps the binding but escalates pending prompts and makes the gate escalate
      immediately and `send`/`answer`/`reset`/`swap` refuse, until resumed.
  - This is necessary because while the hook is blocking, the TUI dialog is not yet on screen, so
    the human can't answer in the pane until the hook returns.

### Rail UI

Fields flow `WorktreeActivity` → `src/lib/ipc.ts:357-458` → `LaneModel`
(`src/lib/railGroups.ts:24-43, 101-129`). The rows (`src/components/rail/Lane.tsx:153-296`):

- **Driven lane.** A `⇠ alpha` badge in the head row, tinted with the driver's project hue (the
  same project for v1). It shows a pending-prompt count when there is one. While the driver is
  deciding, the status block (`blockStateCls`) uses a new "brokered" style: a waiting colour with
  a dashed outline and no pulse.
- **Driver lane.** A `drives 2` chip, with a tooltip listing the driven worktrees and their states.
- **Group header.** A `HeaderSummary` row-1 meta item (`src/components/rail/ProjectGroup.tsx:60`):
  "2 driven".
- **Context menu.** Release, Take over, Pause/Resume driver, added to `worktreeMenuItems`
  (`laneState.ts`). The existing "fresh" entry is the human's equivalent of `reset`.
- **Driven-pane banner.** A slim strip on the terminal pane of a driven worktree: "Driven by alpha
  — what you type here mixes with alpha's prompts." It carries Take over and Release buttons, and
  a pending-prompt line when one is brokered ("alpha is deciding: Bash `rm -rf build`"), from
  `drives_list`, refetched on `sessions://changed`. Input is never blocked; the strip is the
  warning.
- Mock states go in the `.harness` app (`worktree_activity` mock behind a `?drive=` flag) for
  dark/light screenshots. Placeholder names only.

### Driver guidance

Ship a short "driving other worktrees" doc (README section + `keitora drive --help`):
- the loop: `send` → `wait` → `answer`/`read` → repeat,
- the escalation contract: escalate destructive, credential, external-side-effect and ambiguous
  prompts, and always give a reason.

An orchestrating agent reads it the same way it reads the `/workflow` command.

## Dependencies

- The existing dispatch mailbox and watcher (`dispatch.rs`, `host.rs:2303`, `bg_notifier.rs:226`).
- The Claude and Codex hook installers (`hooks.rs`, `codex_hooks.rs`). Codex hooks need trust
  (`--dangerously-bypass-hook-trust`, already used in `src-tauri/src/backend/herdr/agent.rs`).
  Hook content changes re-trigger the Codex trust review for sessions not launched by Keitora.
- The transfer engine (`transfer.rs`, `transfer_registry.rs`).
- The third-provider work (`agy`, in flight on another worktree). `swap --to` and provider-keyed
  maps must not assume two providers (`src-tauri/src/backend/herdr/agent.rs:174`,
  `src/components/Rail.tsx:393`). Coordinate at merge.

## Risks

1. **Claude `PermissionRequest` with no output.** The docs don't explicitly say that the dialog
   then appears; they say it for Elicitation, and Codex documents it. This needs a spike to
   confirm. If it fails, the fallback is to broker via `PreToolUse` → `ask` for escalation and to
   keep `PermissionRequest` for allow/deny only. The same spike confirms `PreToolUse` with
   `updatedInput` answers `AskUserQuestion` without a dialog, and that a session in a bypass or
   auto-accept mode simply never fires `PermissionRequest` (nothing to broker).
2. **Blocking hooks hide the dialog.** While a prompt is brokered, the pane shows nothing to
   answer. The rail state and the Take over action mitigate this, and the broker timeout stays
   bounded (default 5 min).
3. **Codex hooks are less proven here.** `Stop`/`PermissionRequest` were never seen to fire in the
   original spike (the account hit its usage limit). On Windows the hook runs through PowerShell,
   so the spike must also prove stdin reaches `& keitora session gate` and that stdout is passed
   through (the current command discards all streams with `*> $null`; the gate's must discard only
   stderr).
4. **Codex `request_user_input`** can't be brokered, and herdr reports a blocked agent as busy
   (`agent_status_to_state_hint`), so Keitora can't tell it from work. The mitigation is `wait`'s
   `stalled` event, after which the driver `read`s the pane tail; v1 does not add a distinct state.
5. **Prompt injection between agents.** A driven agent's output becomes the driver's input. The
   driver is still bounded by its own permission prompts. Document that driving doesn't elevate
   the driver.
6. **Polling cost and DB churn.** See Implementation notes.
7. **Secrets in prompts.** A brokered `tool_input` can contain tokens (for example in a shell
   command). It is stored in the local `tasks.db`, truncated, and purged after a day.

## Resolved decisions (refine cycles 1 and 2)

1. CLI shape: top-level verbs.
2. Broker timeout: 5 min by default, overridable per drive with `--escalate-after`. The timeout is
   a backstop, not the normal path. It covers a driver that is busy in its own turn and not
   calling `wait`, a driver session that crashed or was closed, and the provider's own hook
   timeout ceiling. Escalation is immediate when the driver is offline, released or paused.
3. Drivers mostly want fresh conversations: `keitora reset <wt> [--to <provider>]` does reset and
   swap in one step, with no confirm. The context-carrying `swap` stays and shows the UI confirm.
4. The driven agent is not told it is driven.
5. One level only. A driver may drive many worktrees.
6. `send` is not held back for human typing. The driven pane shows a warning banner instead.
7. `--force` steal stays, with escalation of the old driver's pending prompts and a rail note.

## Open questions

None blocking. Revisit after the spikes: whether a standing per-worktree auto-allow policy (out of
scope for v1) is wanted once real driving sessions show how many prompts are routine.

## Complexity

**Large.** It is cross-cutting: task store schema, CLI, hook protocol for two providers, app
request runner, window events into the existing restart/transfer flows, the attention model, and
rail UI. Recommend all stages (Refine → Fracture → Singletonize → Loss check → Sequencing).
Recommend two early spikes before Sequencing:
- (a) Claude `PermissionRequest` no-output and `AskUserQuestion` answer behaviour,
- (b) Codex `PermissionRequest`/`Stop` firing on Windows, including stdin and stdout through
  PowerShell.

## Implementation notes

Concrete decisions from the refine verification pass. This section wins over earlier text.

**Hook installation.**
- Claude entries are built in `src-tauri/src/hooks.rs` from `HOOK_MAP` through `hook_command`. Add a
  second const for the gate events (`PermissionRequest`, and `PreToolUse` with matcher
  `AskUserQuestion`) and a `timeout` field on those hook objects (the broker timeout plus 30s).
  Codex's `CODEX_HOOK_MAP` entry for `PermissionRequest` moves from the state command to the gate
  command, with the same `timeout`.
- The gate command must not silence stdout (the decision JSON goes there) but must silence stderr
  and end in `|| true`, so an old CLI that doesn't know `session gate` exits 0 with no output, which
  reads as "no decision". The Claude shape is the existing POSIX guard with `2>/dev/null` in place of
  `>/dev/null 2>&1`; the Codex shape keeps its PowerShell/POSIX split with only stderr discarded.
  The `Stop` command gains `--stdin`.
- Detection and replacement already work through the marker comments (`is_ours`), which every new
  command carries; do not rely on the bare `/keitora session ` substring (the quoted gate command
  doesn't match it).
- Re-install is automatic: `install_hooks_for` and `install_codex_hooks_for` rewrite a worktree's
  file whenever the merged content differs, and they run at app start (`host.rs:514`), on project
  activation (`host.rs:1134`), and on config reload (`host.rs:2403`). Claude hot-reloads the file; a
  running Codex session reads its hooks at launch, so it needs a `reset`/restart to pick up the gate.
  The hook binary is the PATH `keitora` (`resolve_keitora_bin`), so an older installed CLI silently
  never brokers. `keitora drive` therefore checks the driven worktree's hook file for the gate
  entry and warns, and the driver guidance says to upgrade the CLI.

**Identity and project resolution in the hook.** The hook runs with the agent's cwd, which is the
worktree (or a subdirectory). `resolve()` (registry match, then cwd walk to `keitora.toml`, then
`$KEITORA_PROJECT`) lands on the project's single `tasks.db` for every worktree, and
`current_worktree()` is the git toplevel basename, which is correct from a subdirectory. This is the
same identity the existing `session` hooks already rely on. `Cmd::Session` is dispatched before the
tracker backend is opened (today `run()` opens `open_backend` first) so the gate and `session`
hooks only open the local store. `gate` is a reserved first argument of the `Session` command, not
a session state.

**Stop payload.** Today stdin is ignored. `--stdin` reads at most 64 KB with a 1s read timeout (so
a hook run by hand on a terminal can't hang), parses JSON, and stores at most 32 KB of
`last_assistant_message`.

**Schema.** `init_schema` has no versioning or `ALTER` precedent: every table is
`CREATE TABLE IF NOT EXISTS`, run by every CLI and hook open. So all new state is new tables
(`drives`, `drive_prompts`, `drive_requests`, `session_replies`), which existing DBs gain on next
open with no migration step. Avoiding new columns on `worktree_sessions` also keeps `session_upsert`'s
upsert from clobbering them.

**Watcher pickup.** One edit covers both watchers: `spawn_pending_requests` also checks for
pending `drive_requests`. It already runs from the active project's watcher (`host.rs:2303`) and from
each registered project's background watcher before the heartbeat skip (`bg_notifier.rs:226`), so
non-active projects are served. `answer`, release and take-over need no app-side handling: the hook
polls the rows directly.

**Concurrency and churn.** `db_watch::watch_dir` fires on any change to the `tasks.db*` family
(including `-wal` and `-shm`), debounced 300ms, and each pass runs a few cheap reads and emits only
when a revision changes. The hook and `wait` therefore poll at 500ms, hold one connection, and run
the real `SELECT` only when `PRAGMA data_version` changes; they never hold a read transaction open
(that would stall WAL checkpoints). Their writes are one insert and one status update per prompt
plus the state updates the hooks already make. The session revision is extended with drive state, so
these writes do produce one `sessions://changed` per decision, which is intended.

**Headless spawn.** Not needed. `reset` and `swap` go through the mounted pane (see App side).
For the record, a headless spawn would be feasible without a measured size, since
`requested_size` returns `None` and the backend picks one, but `spawn_pty` also needs the active
project's db path (`spawn_db_path`) and the frontend's restart contract forbids a backend same-key
respawn.

**Transfer controller.** Stays in the frontend. Work: two window event listeners in `App.tsx`; a
no-confirm "driven restart" flavour next to the transfer restart in `restartPlan` and the pane's
restart effect; an optional `project` parameter on `onTransfer` (it reads the active project
today); and `drive_request_resolve`. Estimated at a few hundred lines with tests.

**Attention data flow.** Event-driven for the active project and background watchers (session rows
carry `driven_by`, `brokered`, `escalated_reason`); the 2.5s activity poll carries the badges and
counts for every project. The two can disagree for up to one poll interval, which is harmless.

**Gate lifecycle details.**
- A pending prompt older than the hook timeout is treated as `expired` by any reader.
- `wait` identifies "this turn finished" by `turn >= drives.awaiting_turn`, set when `send` is applied,
  so a stale idle before the agent reacts doesn't end the wait.
- `drive --force`, `release`, and pause all escalate pending prompts with `decided_by = system`.
- Rows in `drive_requests` and decided `drive_prompts` older than 24h are deleted when the app's
  expiry pass runs.
