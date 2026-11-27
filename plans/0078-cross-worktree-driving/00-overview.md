# 0078 cross-worktree driving: overview

Source: `plans/0078-cross-worktree-driving.md` (kept unchanged; where its "Implementation notes" differ
from earlier text, the notes win and are what these documents carry). Placeholder names only: driver
`alpha`, driven `bravo`/`charlie`, project `my-game`.

## Document index

| Doc | Subsystem | Depends on |
|---|---|---|
| `00-overview.md` | goal, scope, paradigm, decisions, risks, glossary | none |
| `01-task-store.md` | `crates/keitora-tasks` schema, API, state machines, retention | none (foundation) |
| `02-cli.md` | `crates/keitora-cli` verbs, exit codes, identity | 01 (all state); 03 (shares the `session` entry point); 04 (`send`/`read`/`reset`/`swap` are executed by the app) |
| `03-hook-broker.md` | `keitora session gate`, Stop capture, hook installers, decision table | 01 (rows it reads/writes); 02 (CLI entry point, identity) |
| `04-app-runner.md` | `src-tauri` request runner, watchers, window events, session revision | 01, 02 (request kinds), 05 (events consumed by the window) |
| `05-ui.md` | attention model, rail, banner, menu, harness mocks | 01 (model fields), 04 (activity + session rows, Tauri commands) |

Suggested build order follows the dependency arrows: 01, then 02 and 03 in parallel, then 04, then 05.
Sequencing proper is a later stage; this is only the dependency order.

## Goal

Let one worktree's agent session **drive** other worktrees in the same project through the `keitora`
CLI. The driver can:

- prompt them,
- wait on them and read what they said,
- see their pending permission prompts and questions, and answer each one or escalate it to the human,
- reset them to a fresh conversation, optionally on another provider, or swap their provider while
  carrying context.

The rail shows who is driving whom. The human can always take back control.

The target use is an orchestrator session, such as `alpha` running `/workflow` or a fan-out plan, that
hands work to `bravo` and `charlie`, keeps them moving, and calls the human only when a decision
really needs one.

## Scope

**In (v1)**
- `keitora drive <wt>` / `keitora release <wt>`: an explicit, persistent driver-to-driven binding.
  One driver per driven worktree; a driver may drive any number of worktrees. **One level only:** a
  driven worktree can't drive, a driver can't be driven, and self-drive is rejected. (01, 02)
- Free-form prompting of a driven worktree, plus wait/read of its state and last response. (02, 04)
- **Prompt brokering**: permission requests and `AskUserQuestion` from a driven session go to the
  driver, which answers or escalates them. If the driver doesn't answer, the prompt escalates to the
  human automatically. It is never auto-approved. (03)
- **Reset and swap.** `keitora reset <wt> [--to <provider>]` starts a fresh conversation, optionally on
  a different provider, in one step. A context-carrying swap through the existing transfer engine
  stays available as `keitora swap <wt> --to <provider>`. Both take a provider name rather than "the
  other one", so the third provider being added elsewhere (`agy`) slots in for `reset`; `swap`
  supports only the pairs the transfer engine supports (Claude and Codex today). (02, 04)
- Rail: a driver/driven relationship badge, a distinct "driver deciding" state versus the escalated
  "needs you" state, and Take over / Release / Pause actions. (05)
- Human override: you can still type in a driven pane and nothing is held back, but the pane warns you
  that it is driven. A rail action pulls a pending prompt back to you or ends the drive. (05)
- v1 drives only worktrees of a project that is open (warm) in the Keitora window, because `send`,
  `reset` and `swap` are executed through the live panes. Prompt brokering, `answer`, `wait`,
  `prompts` and `drives` are DB-only and work with the window closed. (04)

**Out (v1)**
- Cross-project driving. A drive binding lives in one project's `tasks.db`.
- Push-notifying the driver mid-turn. The driver polls with `keitora wait`. A later version could
  deliver events through Claude's cross-session inbox or Channels.
- Standing auto-answer policies such as "auto-allow edits in bravo". The driver decides each prompt. A
  policy layer can sit on the same broker later.
- Codex `request_user_input` brokering. It only exists in Plan mode, and no hook sees it (see Risks).
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
   (`crates/keitora-cli/src/main.rs:667-672`). Anything that writes to a pane, or needs live herdr
   state, goes in as a request row in `tasks.db`. The app's DB watcher applies it through the writer
   queue and gates (`src-tauri/src/dispatch.rs:534-629`), then writes the result back to the row. The
   CLI polls that row. Prompt brokering is the exception that needs no app: the hook and the driver's
   CLI talk through `tasks.db` alone.
2. **Prompts are brokered at the hook, not scraped from the terminal.** Claude Code and Codex both run
   a `PermissionRequest` hook that can return allow or deny and can block for minutes (600s default
   timeout, configurable). If the hook returns nothing, the normal interactive prompt appears. That
   "no decision" path is how escalation works. Keitora's hooks already run `keitora session <state>`
   (`src-tauri/src/hooks.rs:23-61`, `codex_hooks.rs`). They gain a brokering subcommand that does
   nothing unless the worktree is currently driven.
3. **Escalation is a first-class answer.** The driver's choices are `allow`, `deny`, `answer` (for
   questions), and `escalate`. The driver should escalate anything it cannot judge safely, such as
   destructive commands, credentials, scope changes, or ambiguity. The reason is shown to the human. A
   timeout always escalates and never approves.
4. **The relationship is state the app can see.** It lives in a `drives` table. Session rows and
   `activity` surface it, and the rail renders it. The rail's attention logic distinguishes "waiting
   on the driver" from "waiting on you".

## Resolved decisions (refine cycles 1 and 2)

1. CLI shape: top-level verbs.
2. Broker timeout: 5 min by default, overridable per drive with `--escalate-after` (1..=540 seconds; the
   hook `timeout` is a fixed 600 s and the broker caps itself at 540 s). The timeout is a
   backstop, not the normal path. It covers a driver that is busy in its own turn and not calling
   `wait`, a driver session that crashed or was closed, and the provider's own hook timeout ceiling.
   Escalation is immediate when the driver is offline, released or paused.
3. Drivers mostly want fresh conversations: `keitora reset <wt> [--to <provider>]` does reset and swap
   in one step, with no confirm. The context-carrying `swap` stays and shows the UI confirm.
4. The driven agent is not told it is driven.
5. One level only. A driver may drive many worktrees.
6. `send` is not held back for human typing. The driven pane shows a warning banner instead.
7. `--force` steal stays, with escalation of the old driver's pending prompts and a note shown in the driven-pane banner (see 05, Singletonize decision 5).

## Open questions

None blocking. Revisit after the spikes: whether a standing per-worktree auto-allow policy (out of
scope for v1) is wanted once real driving sessions show how many prompts are routine.

## Risks

Owning doc in brackets.

1. **Claude `PermissionRequest` with no output.** The docs don't explicitly say that the dialog then
   appears; they say it for Elicitation, and Codex documents it. This needs a spike to confirm. If it
   fails, the fallback (03) is to broker via `PreToolUse` -> `ask` for escalation of tool permissions (question
   escalation always prints nothing) and to keep
   `PermissionRequest` for allow/deny only. The same spike confirms `PreToolUse` with `updatedInput`
   answers `AskUserQuestion` without a dialog, and that a session in a bypass or auto-accept mode
   simply never fires `PermissionRequest` (nothing to broker). [03]
2. **Blocking hooks hide the dialog.** While a prompt is brokered, the pane shows nothing to answer.
   The rail state and the Take over action mitigate this, and the broker timeout stays bounded
   (default 5 min). [03, 05]
3. **Codex hooks are less proven here.** `Stop`/`PermissionRequest` were never seen to fire in the
   original spike (the account hit its usage limit). On Windows the hook runs through PowerShell, so
   the spike must also prove stdin reaches `& keitora session gate` and that stdout is passed through
   (the current command discards all streams with `*> $null`; the gate's must discard only stderr).
   [03]
4. **Codex `request_user_input`** can't be brokered, and herdr reports a blocked agent as busy
   (`agent_status_to_state_hint`), so Keitora can't tell it from work. The mitigation is `wait`'s
   `stalled` event, after which the driver `read`s the pane tail; v1 does not add a distinct state.
   [02]
5. **Prompt injection between agents.** A driven agent's output becomes the driver's input. The driver
   is still bounded by its own permission prompts. Document that driving doesn't elevate the driver.
   [02, driver guidance]
6. **Polling cost and DB churn.** See the concurrency notes in 03 and 04.
7. **Secrets in prompts.** A brokered `tool_input` can contain tokens (for example in a shell
   command). It is stored in the local `tasks.db`, truncated, and purged after a day. [01]

## Spikes (before Sequencing)

Two early spikes are recommended:
- (a) Claude `PermissionRequest` no-output and `AskUserQuestion` answer behaviour,
- (b) Codex `PermissionRequest`/`Stop` firing on Windows, including stdin and stdout through
  PowerShell.

Both gate 03.

## Complexity

**Large.** It is cross-cutting: task store schema, CLI, hook protocol for two providers, app request
runner, window events into the existing restart/transfer flows, the attention model, and rail UI.
Recommend all stages (Refine, Fracture, Singletonize, Loss check, Sequencing).

## External dependencies

- The existing dispatch mailbox and watcher (`dispatch.rs`, `host.rs:2303`, `bg_notifier.rs:226`). (04)
- The Claude and Codex hook installers (`hooks.rs`, `codex_hooks.rs`). Codex hooks need trust
  (`--dangerously-bypass-hook-trust`, already used in `src-tauri/src/backend/herdr/agent.rs`). Hook
  content changes re-trigger the Codex trust review for sessions not launched by Keitora. (03)
- The transfer engine (`transfer.rs`, `transfer_registry.rs`). (04)
- The third-provider work (`agy`, in flight on another worktree). `swap --to` and provider-keyed maps
  must not assume two providers (`src-tauri/src/backend/herdr/agent.rs:174`,
  `src/components/Rail.tsx:393`). Coordinate at merge. (02, 04)

## Glossary

- **driver**: the worktree whose session issues `keitora` drive verbs (`alpha`).
- **driven**: a worktree bound to a driver (`bravo`, `charlie`).
- **brokered prompt**: a permission request or `AskUserQuestion` from a driven session, held at the hook
  and recorded as a `drive_prompts` row until decided.
- **escalate**: resolve a brokered prompt with no decision so the native dialog appears for the human.
  Triggered by the driver, the human (Take over), the timeout, or the system (release, steal, pause).
- **gate**: the `keitora session gate` hook subcommand.
- **turn**: counter in `session_replies`, bumped per finished turn; `wait` compares it with
  `drives.awaiting_turn`.
- **driver deciding / brokered**: UI sub-state of a `waiting` session whose prompt is pending with the
  driver.
- **request / mailbox**: `drive_requests` rows the app applies on the CLI's behalf. Kinds `send`, `read`,
  `reset`, `swap`; statuses `pending`, `running`, `pending_confirm`, `applied`, `refused`; the `queue` column
  marks `send --queue`.
- **escalated**: terminal prompt status (and the rail's "needs you" sub-state of a `waiting` session) after no
  decision was given to the hook; `escalated_reason` carries the text. Other prompt statuses: `pending`,
  `allowed`, `denied`, `answered`, `expired`, `withdrawn`.
- **awaiting turn / ack**: `drives.awaiting_turn` is set when the app applies a `send` and cleared by
  `drive_ack_turn` when `wait` delivers `turn_finished` (or by reset/swap).
- **stalled**: a `wait` event only (CLI-only, no rail state): a `busy` awaited worktree with no hook activity
  for `--stall-after` seconds.
- **paused**: `drives.paused`; set by the human from the rail (`drive_set_paused`, no CLI verb); pending prompts
  escalate and new ones escalate immediately. Surfaced as `drive_paused` in activity.
- **`timeout_s`**: `drive_prompts.timeout_s`, the drive's `escalate_after_s` copied at insert; a pending prompt
  older than `timeout_s` + 30 s is stale.
- **forced_from**: previous driver after a `--force` steal.
- **open projects**: the warm projects the window reports through `drive_set_open_projects`; `drive://restart`
  (reset) and `drive://swap` are the backend-to-window events.

## Driver guidance (cross-cutting deliverable)

Ship a short "driving other worktrees" doc (README section + `keitora drive --help`):
- the loop: `send` -> `wait` -> `answer`/`read` -> repeat,
- the escalation contract: escalate destructive, credential, external-side-effect, scope-changing and ambiguous
  prompts, and always give a reason.

An orchestrating agent reads it the same way it reads the `/workflow` command. Owned by 02 (help text)
and the README; it must also say to upgrade the CLI (see 03) and that driving doesn't elevate the
driver.

## Cross-doc reconciliation

Changes made when the six documents were reconciled. The owning doc's version is canonical.

1. **Hook timeout and `--escalate-after` range (owner 03).** Hook `timeout` is a fixed 600 s and the broker cap
   is 540 s, so `--escalate-after` is 1..=540 with default 300. Fixed in 01 (constants `MIN 1`/`MAX 540`,
   `CHECK(escalate_after_s BETWEEN 1 AND 540)`, `drive_bind` message, range test), 02 (flag help, exit 4, new
   test and decision) and here (Resolved decision 2). The old `DRIVE_HOOK_TIMEOUT_SLACK_S` (timeout + 30) is
   replaced by `DRIVE_PROMPT_STALE_SLACK_S`; the stale rule stays `timeout_s + 30`, always under 600 s, and
   03's two statements about "older than the hook timeout" now name that rule.
2. **Withdrawal (owner 03, 01 implements).** Only `busy`, `idle` and `offline` writes withdraw pending prompts;
   `waiting` never does. 01 already had `state != 'waiting'` plus a test; no change needed, confirmed equal.
3. **AskUserQuestion escalation prints nothing (owner 03).** 00, 01 and 02 had no contradicting text; 00's risk 1
   now says `ask` is only the spike-failure fallback for tool permissions.
4. **Gate detection literal.** 02 searches the hook files for `session gate`; every 03 gate command contains
   `session gate --provider ...`. Consistent, no change.
5. **04's four additions to 01 (owner 01).** Added `drive_requests_in_flight`, `drive_restore_awaiting_turn`
   (set to a value or none; covers the send rollback and reset/swap clearing), `drive_request_resume`
   (`pending_confirm` -> `running`, with the state diagram and tests) and `drive_pending_prompt_counts` (stale
   rule applied). 04 now names these instead of describing roles.
6. **Missing drive tables on read-only handles (owner 01).** 01 specifies `drive_tables_exist` and the behaviour
   of `sessions_list`, `session_revision` and the drive read methods when tables are absent, with test
   `read_paths_tolerate_missing_drive_tables`. 04 points at it.
7. **Names in the glossary.** Added `escalated`, `awaiting turn`/ack, `stalled`, `paused`, `timeout_s`,
   `forced_from`, open projects and the request kinds/statuses (01's `drive_ack_turn`, `timeout_s`, `queue`;
   02's `answer`, `send -`, `read --last`; 04's `drive_paused`, `drive_set_open_projects`, `drive://restart`,
   `drive://swap`).
8. **Other mismatches fixed in 04.** `send` payload is `{"text"}` only (`queue` is 01's column); `reset`/`swap`
   `to` is a free-text provider and results use the key `provider` (01/02) instead of `cli`/`fresh`; the
   release command maps to 01's `drive_end`; the read truncation marker counts inside the 64 KiB cap, so 02's
   `truncated` test (`len >= 65536`) holds. Also added to 01 how 03's `session_state`/`set_session_state`
   map onto `sessions_list`/`session_upsert`.
