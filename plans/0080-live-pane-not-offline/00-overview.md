# 0080 overview: drive state reliability

Parent proposal: `plans/0080-live-pane-not-offline.md` (unchanged; its Decisions and
"Implementation notes" win over earlier text). Evidence: `research-herdr.md`,
`research-consumers.md`, `research-signals.md` in this folder. Placeholder names only.

One PR on this branch, built in phases 0-6. After each phase: `cargo test --workspace` and
`yarn test` are green; run `/simplify`, then `/code-review high`, and fix findings before the
next phase.

## Goal

Make orchestration across Claude, Codex and Antigravity (agy) consistent and reliable:

1. **One answer per question.** Every part of Keitora reports the same thing about the same
   worktree, whoever asks: the rail, `send`, `wake`, `wait`, `drives`, dispatch or toasts. The
   questions are: is an agent running? Is it idle, busy or blocked? Did its turn finish, and
   what did it say?
2. **The answers come from the most reliable signal available,** not from provider hooks that
   fail silently.
3. **Every request is picked up promptly, or fails with a true reason.**
4. **The app checks itself continuously.** It notices when signals disagree or plumbing stalls,
   fixes what it can, and tells the human and the orchestrating agent what is wrong.

## Reports this must fix

Keitora v0.5.1, CLI bundled with the app.

| Id | Report | Root cause (short) |
|---|---|---|
| **A1** | macOS: an agy worktree with a live chat shows grey/offline in the rail. `send --no-wake` is refused as offline. | Liveness comes from hook rows. agy hooks are silent without `KEITORA_BIN`, so the row is missing, and a missing row reads as offline to `send`. |
| **A2** | Same worktree after `reset --to codex`: still offline for `send`, while `wake` returns `already_awake: true` and herdr shows the pane idle and input-ready. | `wake` judges liveness from the pane set, `send` from the row. `reset --to` installs no Codex hooks, and a late `SessionEnd` can overwrite `idle`. `apply_reconcile` never lifts `offline`. |
| **B** | Two Codex prompts were delivered and answered (visible in herdr). `drives` showed `idle`, `awaiting_turn=1`. `wait` never returned `turn_finished`, and `read --last` returned nothing. A Claude control worked. | Turn completion and the reply come only from the Stop hook, which Codex didn't run (untrusted project layer, or a launch without the trust bypass). `Stalled` needs a raw `busy` row, so an idle session with no reply waits forever. |
| **C** | A `read` (Claude) and two `send`s (Codex) timed out with "no running Keitora app picked it up" while the app was running with the project open. | Passes run only on `tasks.db*` file events, a window open-set change, or a one-shot post-wake recheck. An idle flip seen only by herdr writes nothing, so it never re-runs the pass. Watcher threads can exit silently. The message is printed by expiry, which runs inside a running app's pass. |
| **R** | Remote report, request 25: a request for a ready lane in an open project stayed pending until it expired, with no trace of why. | Same pipeline gaps as C, plus no per-request `wait_reason` and no per-pass logging. |

## Diagnosis

`keitora send` already delivers through herdr (`agent prompt`, or paste then Enter). Delivery is
not the problem. What fails is Keitora's bookkeeping around it.

- **Liveness and activity come from hook-written rows**, the least reliable part across
  providers:
  - Codex loads project hooks only for trusted projects. The hook-trust bypass
    (`--dangerously-bypass-hook-trust`) is passed on every normal launch, but the Codex
    transfer-strict launch (`LaunchPolicy::TransferStrict`, `backend/herdr/strict.rs`
    `strict_chain`) drops it on its last step, "fresh without optional flags".
  - agy's hooks are global and do nothing without `KEITORA_BIN` (`antigravity_hooks.rs`
    `hook_command`).
  - Provider changes never install Claude or Codex hooks. `DriveHost::prepare_launch`
    (`drive.rs`) returns early unless the target is agy. The UI switch (`agent_switch.rs`) is
    agy-only too, and transfer installs nothing.
  - A killed agent's late `SessionEnd` overwrites the new agent's `idle` (`session_upsert` has
    no session identity).
  - Every hook discards its stderr (`>/dev/null 2>&1`, or `>nul 2>&1` for agy on Windows).
- **"Is the agent running?" has seven different answers** in the code:
  1. the live pane set (wake's `awake()`: live set minus terminated);
  2. reconciled rows (`send`, dispatch, rail);
  3. raw rows (`drives`, `wait`, the gate, `task watch`, `bg_notifier`);
  4. the window's pane status (`paneStatus.ts`);
  5. herdr's `agent_present` (resume store only);
  6. `list_sessions` keys (agent updates, quit prompt);
  7. activity hints.

  `apply_reconcile` (`backend/mod.rs`) never lifts `offline` and never applies `idle`. A crashed
  agent's leftover shell pane stays in the live set.
- **Turn completion depends on a Stop hook.** When it doesn't run, `wait` hangs. `select_event`
  (`keitora-cli/src/drive.rs`) only reports `Stalled` for a raw `busy` row. The stall clock is
  `last_heartbeat`, which only hooks move, so a long healthy turn also reads as stalled.
- **Requests are picked up only on a `tasks.db*` file event,** on a window open-set change
  (`drive_set_open_projects` -> `retrigger_pending`), or by the single post-wake
  `recheck_after_settle`. There is no periodic pass.
  - A wait condition that clears without a DB write is never rechecked.
  - Watcher threads exit silently: the `notify` watcher fails to start, `watch()` fails, or the
    background watcher's store open fails. `notify` error events are dropped too.
  - The active-project watcher is recreated only on project activation; background watchers only
    on registry changes.
  - "No running Keitora app picked it up" is printed by expiry, which runs inside a running
    app's pass (`housekeeping`).

## Key finding

herdr already knows what we need, without hooks (`research-herdr.md`, verified against the
0.9.5 binary and bundled schema).

- herdr derives `agent_status` (`idle`, `working`, `blocked`, `done`, `unknown`) for Claude,
  Codex and agy by matching the screen against detection rules it can update remotely.
- herdr's own integration hooks only record session ids. They are user-level, so project trust
  doesn't affect status.
- `agent list` reports fields Keitora ignores today: `completion_seq` (nullable; "the current
  idle transition completed work"), `state_change_seq`, `launch_pending`, `interactive_ready`,
  `screen_detection_skipped` (all optional in the schema), and `agent_session`. `pane list`
  (`PaneInfo`) has none of these, so `agent list` is required.
- **Push is not practically available to Keitora.** The socket API's `events.subscribe` exists,
  but no CLI command streams it; Keitora talks to herdr only by spawning the CLI; on Windows the
  socket path is a pointer file (`<pid>:<nonce>`) to an undocumented pipe; no event carries the
  seq counters anyway. So the engine polls `agent list` (one CLI call per tick covers every
  agent) at an adaptive cadence. Push stays a follow-up behind Spike 2 (D2).

Known limits, each with a mitigation (see `02-engine-core.md`, `03-turns-and-cli.md`,
`07-self-diagnostics.md`):
- An unrecognised screen of a known agent falls back to `idle`.
- `blocked` is reported only for approval screens herdr recognises.
- agy reports `idle` while its background tasks or subagents still run.

## Architecture summary

- **Session engine** (app side, one module): a pure `derive(snapshot, rows, prompts, prev, now)
  -> View` function plus a thin driver in the status loop. Polls `agent list` adaptively. Every
  consumer reads its output; nothing else interprets raw signals. Hook rows become evidence.
  -> `02-engine-core.md`
- **Status store:** global `<KEITORA_HOME>/status.db` (single app writer with a lease, CLI
  read-only) for the live view, presence and findings; `drive_turns` in each project's
  `tasks.db` for durable turn events. -> `02-engine-core.md`, `03-turns-and-cli.md`
- **Turns:** a delivery baseline persisted on the `drives` row, a settled-idle turn predicate
  over herdr seqs, driver vs human turn attribution, and a `wait` that always terminates.
  -> `03-turns-and-cli.md`
- **Request pipeline:** engine-driven passes, self-rescheduling, a safety sweep, supervised
  watchers, per-request `wait_reason`, app presence in `status.db`. -> `04-request-pipeline.md`
- **Hooks:** fewer ways to fail; they matter only for permission brokering, reply text and the
  busy veto. -> `05-hook-placement.md`
- **Reply text:** transcript fallbacks. -> `06-reply-fallbacks.md`
- **Self-diagnostics:** a health monitor with findings, UI glyph, Settings section and
  `keitora status`. -> `07-self-diagnostics.md`
- **Threads:** the herdr status loop (engine snapshots), `keitora-status-heartbeat` (5 s lease
  heartbeat and throttled `status.db` flushes) and `keitora-pipeline` (1 s: sweep, watcher
  supervision, health monitor). One table: `02-engine-core.md` S14.

## Decisions

- **D1. Spike venue:** this Windows machine, through `yarn dev:isolated`, gated by Spike 0 and
  Phase 0. (User decision.)
- **D2. Push vs poll:** poll `agent list` adaptively. No CLI command streams events, and the
  Windows transport is an undocumented pointer file. No event carries the seq counters, so
  completion needs `agent list` anyway. Push is a possible follow-up only.
- **D3. PR shape:** one PR, with phases 0-6 and `/simplify` + `/code-review high` after each.
  (User decision.)
- **D4. Status store:** a global `<KEITORA_HOME>/status.db` for the live view, presence and
  findings, plus a `drive_turns` table in each project's `tasks.db` for turn events. Compared
  options:
  - **Presence file (JSON):** whole-file rewrites per change; Windows rename/sharing hazards with
    concurrent readers; no transactions; no `data_version`. Rejected.
  - **A table in `tasks.db` for the live view:** every state flip would fire all `tasks.db`
    watchers, bg toast passes and drive passes (the loop risk the proposal wanted to avoid).
    Rows are project-less, and the DB may be shared by many worktrees. Rejected for the
    high-churn view.
  - **`status.db` beside each `tasks.db`:** one file per project, with N writers, N leases and N
    heartbeats, and the CLI needs the project's `db_path` before it can even say "no app
    running". Rejected in favour of the global file.
  - **Chosen: global `status.db`.** Keyed like `SessionKey` and the global `resume.db`; one
    writer connection, one lease, WAL with the CLI read-only; outside every `tasks.db*` watcher
    by name and location; the isolated dev app gets its own automatically (separate
    `KEITORA_HOME`); a disposable cache.

  Turn events go to `tasks.db` on purpose: they are durable, must be acked transactionally next
  to `drives.awaiting_turn`, and should wake passes and `wait`. They are low-frequency (one per
  turn).
- **D5. Human-typed turns:** a separate `human_turn` event (user decision), attributed by the
  ledger in `03-turns-and-cli.md`.
- **D6. Codex `notify`:** dropped. herdr completion plus the rollout covers it, and a per-launch
  `notify` would replace the user's own `notify` (the Codex desktop app's) inside Keitora panes.
- **D7. The CLI never talks to herdr.** It reads `status.db`/`tasks.db` only. Rejects
  research-herdr's "`wait` shells out to `herdr agent wait`".
- **D8. Hook state stays as evidence** (no hooks removed). Older CLIs on PATH keep working
  through `session_replies`/`worktree_sessions`.

## Failure modes

| Situation | Engine behaviour | Consumers |
|---|---|---|
| **App start, before the first snapshot** | The view is `stale` with `snapshot_at = null`. Nothing is derived from absence. The previous instance's rows are kept but marked stale. | Send, dispatch and wake leave requests pending ("waiting for first herdr snapshot"). The rail shows the last state dimmed. The CLI sees `engine_ready = false`. |
| **herdr down, restarting or recovering** (`ServerGate` refuses) | No herdr calls. The link is `recovering` (`down` after an explicit stop, `unavailable` without a herdr backend) and every view is `stale` (never `offline`). | Same as above, with "waiting for herdr (recovering)" (or `(down)` / `(unavailable)`). `wait` stalls only after `--stall-after`, with "herdr unavailable for Ns (recovering)". |
| **After recovery** | Full re-snapshot. A pane or occupant identity change invalidates baselines (seqs restart with a new server). | An outstanding turn ends as `stalled: agent restarted mid-turn`. Recovery's existing mid-turn nudge is unchanged. |
| **Unrecognised screen (idle fallback)** | Settled-idle damping; the hook busy veto (most-recent-evidence); the "turn start not observed" finding; herdr refuses `agent prompt` to `blocked`. | A send into a misdetected busy agent is possible only if hooks are also silent and herdr shows idle for 2 snapshots. Then the provider queues the text as the next message (Claude and Codex queue mid-turn input), and the delivery is attributed to the next completion. Accepted residual risk. |
| **agy idle during background work** | The busy veto from agy `PreInvocation` (when hooks work). Otherwise completion carries the agy caveat. | `turn_finished.caveat` tells the driver. |
| **A completion_seq reading we didn't expect** | Predicate clauses 2 and 3 still finish the turn. Spike 1 picks the primary clause. | - |
| **Two app instances on one `KEITORA_HOME`** (installed + non-isolated dev) | A lease on the `writer` row: take over only if the heartbeat is older than 15 s. The loser computes its view but doesn't write `status.db` or `drive_turns`, and shows a finding. | The CLI reads one consistent writer. The isolated dev app has its own `KEITORA_HOME`, so it never contends. |
| **App crash** | - | `heartbeat_at` goes stale within 15 s. The CLI reports "no Keitora app is updating status" and uses the legacy hook path. |
| **`status.db` missing or corrupt, or a schema mismatch** | It's a cache: recreate it at startup. | The CLI treats it as stale until the first write. |

## Phase index

| Phase | Doc | Content | Reports fixed | Depends on |
|---|---|---|---|---|
| 0 | `01-phase0-isolation.md` | Isolation hardening (prerequisite for live spikes) | - | - |
| 1 | `02-engine-core.md` | Engine core, `status.db` writer, app consumers migrate | A1, A2 | 0 (live spikes), Spike 1 |
| 2 | `03-turns-and-cli.md` | Baseline, predicate, `drive_turns`, human turns, CLI `wait`/`drives`/etc. | B (hang), A2 (`drives`) | 1, Spike 1 |
| 3 | `04-request-pipeline.md` | Passes, sweep, watcher supervision, `wait_reason`, presence | C, R | 1 (engine view changes, `status.db` DDL, threads), 2 (turn rows wake passes, `Ctx.status_db`, `drive_request_requeue`) |
| 4 | `05-hook-placement.md` | Codex bypass, all-provider `prepare_launch`, Claude `--settings`, agy, stale-writer guard, hook log | A1 (agy rows), A2 (hooks), B (Codex Stop) | 1 (`agent_session` and the derive filter for the guard, `keitora_home()` in `keitora-tasks`), Spike 3 (Spike 3b non-blocking) |
| 5 | `06-reply-fallbacks.md` | Claude transcript, Codex rollout, agy session files (spike-gated, s27b) | B (`read --last`) | 2 (reply collection order), Spike 5 |
| 6 | `07-self-diagnostics.md` | Health monitor, Settings, glyph, `keitora status` | diagnosability of all | 1-5 (consumes their signals) |

Phase 6 comes last because it consumes signals from every other phase. `spikes.md` holds the
spike procedures and which phase each gates.

**Out of scope:** agy prompt brokering; a herdr push subscription (follow-up, behind Spike 2);
Codex `notify`; a Codex `app-server` or Claude Channels integration; replacing the interactive
TUIs.

## Report -> fix -> test

Fixture for every test: fake herdr + temp DBs + fake clock. Detail is in each phase doc.

| Report | Phase | Fix | Regression test |
|---|---|---|---|
| A1 | 1 | Presence from herdr `agent list` | A missing row with agy present and idle -> `idle` in the rail, `sessions_list`, send and `drives`. A stale `offline` row with an agent present -> `idle`. A bare shell (agent absent) with an `idle` row -> `offline`. |
| A1 | 4 | agy absolute path + re-assert | Install writes an absolute path; re-assert after a simulated herdr rewrite restores the key; the marker env makes the hook a no-op outside panes. |
| A2 | 1 | One liveness answer | Property test over provider x row state x herdr state: `wake` reports `already_awake` iff send doesn't refuse as offline. A late `SessionEnd` after reset leaves the view idle. |
| A2 | 4 | Hooks on provider change | `reset --to codex` and wake `--to` call `prepare_launch` for Codex (the fake host records it); the real host writes `.codex/hooks.json` in a temp worktree; every strict step carries the trust bypass. |
| B | 2 | Completion from herdr | A Codex delivery with no Stop: `completion_seq` changes -> `wait` returns `turn_finished` with `reply_source: none` within one hot tick. The idle-no-change case -> `stalled` at 30 s. `working` for 10 minutes -> no stall. A human typing after resolution -> `human_turn`. |
| B | 5 | Reply from rollout | A captured rollout fixture yields the reply; `read --last` returns it. |
| C | 3 | Pipeline | A herdr-only idle flip schedules a pass and applies a queued send with no DB write. A `read` resolves while an older send holds the gate. A dead watcher is restarted and the sweep claims meanwhile. Expiry carries `wait_reason`. The CLI timeout text covers presence absent, stale and fresh. |
| R | 3 | Pipeline + presence | An open project claims a ready lane's request exactly once. A window close/reopen and a project switch away and back restore pickup. A request outside the open set is refused with the unchanged "project not open in Keitora" (a refusal, never a wait state) and the pass log names the open set. Zero writes across idle passes. |

## Reproductions (smallest reliable reproduction per report)

Each report gets one **repro test**: the smallest step list that shows the failure on the
current tree, written as a fixture test (fake herdr backend, temp DBs, fake clock; no live
herdr). Rules:

- **RED first.** The repro test is written and run against the unmodified code of its phase
  *before* the fix, and must fail on the assertion named below. Only then is the fix written
  and the test goes green. Record the failing assertion text in the phase's `/code-review`
  notes, so the red run is evidenced even though the PR holds only the green state. A repro
  that cannot be made red on the old code is wrong and must be re-cut before the fix starts.
- A repro is a flat step list over the fixture's verbs (`set_agent`, `write_row`, `tick`,
  `send`, `wake`, `wait`, `file_request`, `advance`), no helper that hides a step. The test
  name starts with `repro_` and is listed in the owning phase doc's tests.
- The broader regression tests in the table above stay; a repro is the minimal red case, not
  a replacement.

| Report | Repro test (verbatim name) | Fixture steps (smallest) | Red assertion on the old code | Lands RED-first in |
|---|---|---|---|---|
| A1 | `repro_a1_agy_live_without_a_row_reads_idle_and_send_not_refused` (`engine/tests.rs`) | 1. fake backend `set_agent(alpha, agy, idle)`; 2. temp `tasks.db` with **no** `worktree_sessions` row; 3. `tick` twice, 1 s apart (settled); 4. read `sessions_list`, run `send --no-wake alpha "x"` | `sessions_list` says `offline` and `send` refuses `offline`; expected `idle` and delivered | Phase 1 (`02`). Hook-side half `repro_a1_agy_hook_command_is_inert_without_keitora_bin` (`antigravity_hooks.rs`: the generated command with no `KEITORA_BIN` writes nothing) lands RED-first in Phase 4 (`05`) |
| A2 | `repro_a2_reset_to_codex_then_late_session_end_keeps_send_and_wake_in_step` (`engine/tests.rs`) | 1. row `offline` for `bravo`; 2. `set_agent(bravo, codex, idle)` as after `reset --to codex`; 3. `tick` twice; 4. write a late `SessionEnd` row `offline`; 5. run `wake bravo` and `send --no-wake bravo "x"` | `wake` returns `already_awake: true` while `send` refuses `offline` (the mismatch); expected both agree, send delivers | Phase 1 (`02`). Hook-install half `repro_a2_reset_to_codex_prepares_codex_hooks` (`drive_tests.rs`: the fake host records `prepare:codex`) lands RED-first in Phase 4 (`05`) |
| B | `repro_b_codex_answer_without_stop_returns_turn_finished` (`crates/keitora-cli/src/drive_tests.rs`, over `engine/tests.rs` state) | 1. `drives` row for `alpha` with a delivery, baseline `completion_seq 4`; 2. `set_agent(alpha, codex, working)`; 3. `tick`; 4. `set_agent(alpha, codex, idle, completion_seq 5)`; 5. `tick` at +0 s and +1 s; 6. no Stop row is ever written; 7. `wait alpha` | `wait` never returns (fake clock passes 10 minutes, still waiting) and `read --last` is empty; expected `turn_finished`, `reply_source: none`, within one hot tick | Phase 2 (`03`). Reply half `repro_b_codex_rollout_fixture_yields_the_reply` (`06`) lands RED-first in Phase 5 |
| C | `repro_c_herdr_only_idle_flip_applies_a_queued_send_without_a_db_write` (`drive_tests.rs`, pipeline `Fx`) | 1. `set_agent(alpha, claude, working)`; 2. file a queued `send` request (it waits: `...to be idle (herdr: working)`); 3. `set_agent(alpha, claude, idle)`, `tick` twice; 4. no `tasks.db` write, no window change; 5. `advance` 30 s | the request is still `pending` and expiry prints "no running Keitora app picked it up"; expected `applied` after the second tick | Phase 3 (`04`) |
| R | `repro_r_ready_lane_request_in_an_open_project_is_claimed` (`drive_tests.rs`, pipeline `Fx`) | 1. window open set `{my-game}`; 2. lane `bravo` idle and settled; 3. file a `send` for `my-game/bravo`; 4. `advance` 5 s with the watcher marked `Exited` (no file events) | still `pending` at 5 s with an empty `wait_reason` and no pass-log line (no trace of why); expected `applied` by the sweep, or a non-empty `wait_reason` plus a pass-log line naming `open=`, `db=`, `trigger=` and the request id | Phase 3 (`04`) |

Phases 1 and 2 may stub a repro the fake host cannot yet express (for example C needs the
pipeline) only by marking it `#[ignore = "RED until phase N"]` with the exact phase; an ignored
repro must be un-ignored in its phase, and the phase is not done while one remains.

- Every phase ships fixture tests using the fake herdr backend (extended with `agent list`
  entries, seqs, `launch_pending`/`interactive_ready`, and occupant identity), temp DBs and a
  fake clock. No live herdr in tests (`cfg(test)` already skips integration installs and global
  writes).
- Each report's regression test is in the table above and in its phase doc; each report's
  `repro_*` test (Reproductions, above) is written failing first, then goes green with the fix.
- Fixtures use `alpha`/`bravo`/`my-game` and `/Users/me`/`C:/Users/x` paths. Code and comments
  carry no plan or task identifiers.
- Logging: engine view changes at debug level; one info line per delivery, completion (with the
  clause that fired) and stall reason, so the next remote report has a timeline.

## Open question

Spikes 1, 3 and 5 launch real Claude, Codex and agy sessions in the dev app, using real agent
logins and quota, writing transcripts into the real `~/.claude/projects` and Codex sessions dirs
(throwaway dev repo paths only), for roughly 30-60 minutes. Needs the human's OK (or a quiet
window) before the first live spike. See `spikes.md`.

## Glossary

- **Engine / session engine:** the app-side module that derives one view per `(project,
  worktree)` from herdr `agent list` plus hook rows; the single source of truth.
- **View:** the engine's derived record: `state`, `agent`, `herdr_status`, `settled`, `stale`,
  identity, seqs, `last_change_at`, `brokered`, `escalated_reason`.
- **status.db:** global `<KEITORA_HOME>/status.db`, written only by the app (lease + 5 s
  heartbeat), read by the CLI; holds `writer`, `app_projects`, `sessions`, `findings`; every time
  column is INTEGER unix ms; the whole DDL is in `02-engine-core.md` S10.
- **drive_turns:** per-project `tasks.db` table of turn completions for driven worktrees, acked
  transactionally; the source of `turn_finished` and `human_turn`.
- **Baseline (`B`):** `{pane_id, terminal_id, agent_session, state_change_seq, completion_seq,
  status, delivered_at}` captured from a fresh `agent list` right before delivery, persisted on
  the `drives` row with a monotonic `delivery_id`.
- **Settled idle:** `idle`/`done` seen on two consecutive snapshots at least 1 s apart with
  `state_change_seq` unchanged.
- **human_turn:** a completion of a driven worktree not attributable to an outstanding driver
  delivery (the human typed), delivered once as its own `wait` event.
- **wait_reason:** per-request text recording why a request is still pending; carried by expiry.
- **Presence:** whether an app is running and which projects its window has open, stored in
  `status.db` (`writer`, `app_projects`).
- **Health finding:** a per-worktree, per-project or app-level diagnostic row in
  `status.db.findings` (key `(project, worktree, check_id)`) produced by the health monitor.
- **Most-recent-evidence rule:** a hook `busy`/`waiting` write newer than herdr's last status
  change overrides herdr `idle`; a hook `idle`/`offline` never overrides herdr presence or
  `working`/`blocked`.

## Cross-doc reconciliation

Final pass after the per-phase hardening; each item names the owning doc and what changed.

1. **`findings` DDL** lives only in `02` S10: `check_id` (not `check`), severity
   `info|warn|error`, plus 07's `detail`, `occurrences`, `cleared_at`, `remedy`, `remedy_at`;
   `project = ''` means app-level. `07` now references it and writes through
   `StatusPublisher::findings_apply`.
2. **Time unit:** every `status.db` column is INTEGER unix ms (stated once in `02` S10).
   `tasks.db` keeps UTC text; cross-store comparisons use `keitora_tasks::sqlite_utc_to_ms`.
   04/07 no longer carry "if 02 picks another type" hedges.
3. **`app_projects` DDL** moved into `02` S10 with every column 04 needs (`registry_key`,
   `open_reported_at`, `watcher_state`, `last_pass_trigger`, `pending_count`,
   `oldest_pending_at`, `restarts`, `last_error NOT NULL DEFAULT ''`, `last_error_at`); `04` owns
   semantics only. Added `StatusReader::app_project` / `findings` and row types.
4. **Scheduling:** one thread table (`02` S14). Status loop = engine snapshots only;
   `keitora-status-heartbeat` (5 s) = lease, writer link columns, throttled `app_projects` and
   `findings.last_seen` flushes; new `keitora-pipeline` thread (1 s, every backend) =
   sweep 10 s, supervisor 10 s, health 30 s + 2 s debounce. `04` P1 drops the
   `StatusLoopDeps.pipeline` / Unavailable-only thread design; `07` H1 matches.
5. **Deferred sends:** `04`'s wait-reason vocabulary gains the "deferred at delivery" row
   (`drive_request_requeue`, `Outcome::deferred`) and a `deferred: {reason}` pass-log outcome;
   `03` T4 uses `@{wt}` and those exact strings.
6. **One wait-reason vocabulary:** `02` S9 `SendGate::Wait` texts now equal `04`'s strings
   (`waiting for first herdr snapshot`, `waiting for herdr ({link})`, `waiting for herdr (stale
   snapshot)`, `... to be idle (herdr: starting)`, `... (send settling)`). 04 clarified that
   non-queued sends and `wake` wait only on transient conditions.
7. **Stall wording** `herdr unavailable for Ns (<link>)` everywhere: `03` T7 defines the age
   and the `stale snapshot` label; this doc's failure-mode table and `07`'s `herdr_link` message
   use it (no "since T").
8. **Link vocabulary:** `07` used `healthy`; now `starting|up|recovering|down|unavailable`
   (02's `HerdrLink`) in the check, `keitora status` JSON and human output (07 H11).
9. **`reply_source` = `hook|transcript|none`** in `03` (T2 CHECK, T8 JSON) and `06` R4. The
   transcript timing is normative in `03` T5 step 4 (lookup from 1 s on `spawn_blocking`, 3 s
   budget, per ingest); `06` dropped its 250 ms poll and its `session_replies` write (the engine
   never writes that table, 03 T11). `ReplyQuery.delivered_at_ms` comes from
   `Baseline.delivered_at_ms`; `TranscriptSource::reply` returns a miss reason.
10. **`hook_facts`** defined in `05` section 8 (`HookFacts`, `LaunchRecord`, `note_launch`,
    `HOOK_VERSION`, `last_hook_error` from a shared `keitora-tasks` `hook_log` parser); `07`'s
    `hooks_missing` / `agy_plumbing` use its field names.
11. **Stale-writer guard:** `02` S5 rule 0 compares `HookEvidence.session_id` with
    `obs.agent_session` (read via `LocalStore::session_ids`, safe on old DBs) and owns the derive
    test; `05` owns the `worktree_sessions.session_id` column, `--payload-session`,
    `session_upsert_with_id` and an end-to-end test. `keitora_home()` move cited as `02` S2.
12. **"project not open"** stays an immediate refusal (`project not open in Keitora`); the R
    regression row above and `04` say so explicitly; it is in no wait-state list.
13. **Phase 6 engine inputs** added to `02` S6 (`link_state`, `key_health` with
    `present_since_ms`/`unknown_since_ms`) and `03` T5 step 5 (`turn_health`: completion ring
    and the app-side no-activity record), replacing 07's `agent_present_since` and its reliance
    on the CLI-only stall.
14. **`AgentObs`** gains `agent_session_kind`/`agent_session_source` (`02` S4) for 06's locator.
15. **CLI context:** `04` uses `03` T6's `Ctx.project` / `Ctx.status_db` (no separate
    `Ctx.presence`); presence treats `NotReady` as fresh.
16. **Phase index / spikes:** dependencies updated (Phase 3 on 2's `Ctx.status_db` and requeue;
    Phase 4 on 1's derive filter and `keitora_home`; S3b non-blocking in `05`). Files 01-07 =
    phases 0-6 verified in every doc; spike IDs S0, S1, S2, S3, S3b, S5 unchanged.
