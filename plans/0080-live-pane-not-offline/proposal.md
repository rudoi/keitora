# 0080 drive state reliability: one source of truth per question, plus self-diagnostics

Status: proposal (refine cycle 4, automated Refine review applied). Placeholder names only.

Evidence (read before reviewing):
- `plans/0080-live-pane-not-offline/research-herdr.md`: what herdr reports, and how it knows.
  It has a "Corrections and additions" section from this cycle, checked against the 0.9.5
  binary and its bundled schema.
- `plans/0080-live-pane-not-offline/research-consumers.md`: every writer and reader of session
  state today, and where they disagree.
- `plans/0080-live-pane-not-offline/research-signals.md`: signals that don't depend on hooks,
  ranked by reliability.

## Goal

Make orchestration across Claude, Codex and Antigravity (agy) **consistent and reliable**:

1. **One answer per question.** Every part of Keitora reports the same thing about the same
   worktree, whoever asks: the rail, `send`, `wake`, `wait`, `drives`, dispatch or toasts. The
   questions are:
   - Is an agent running?
   - Is it idle, busy or blocked?
   - Did its turn finish, and what did it say?
2. **The answers come from the most reliable signal available,** not from provider hooks that
   fail silently.
3. **Every request is picked up promptly, or fails with a true reason.**
4. **The app checks itself continuously.** It notices when signals disagree or plumbing stalls,
   fixes what it can, and tells the human and the orchestrating agent what's wrong.

## Reports this must fix

Keitora v0.5.1, CLI bundled with the app.

| Id | Report | Root cause (short) |
|---|---|---|
| **A1** | macOS: an agy worktree with a live chat shows grey/offline in the rail. `send --no-wake` is refused as offline. | Liveness comes from hook rows. agy hooks are silent without `KEITORA_BIN`, so the row is missing, and a missing row reads as offline to `send`. |
| **A2** | Same worktree after `reset --to codex`: still offline for `send`, while `wake` returns `already_awake: true` and herdr shows the pane idle and input-ready. | `wake` judges liveness from the pane set, `send` from the row. `reset --to` installs no Codex hooks, and a late `SessionEnd` can overwrite `idle`. `apply_reconcile` never lifts `offline`. |
| **B** | Two Codex prompts were delivered and answered (visible in herdr). `drives` showed `idle`, `awaiting_turn=1`. `wait` never returned `turn_finished`, and `read --last` returned nothing. A Claude control worked. | Turn completion and the reply come only from the Stop hook, which Codex didn't run (untrusted project layer, or a launch without the trust bypass). `Stalled` needs a raw `busy` row, so an idle session with no reply waits forever. |
| **C** | A `read` (Claude) and two `send`s (Codex) timed out with "no running Keitora app picked it up" while the app was running with the project open. | Passes run only on `tasks.db*` file events, a window open-set change, or a one-shot post-wake recheck. An idle flip seen only by herdr writes nothing, so it never re-runs the pass. Watcher threads can exit silently. The message is printed by expiry, which runs inside a running app's pass. |
| **R** | Remote report, request 25: a request for a ready lane in an open project stayed pending until it expired, with no trace of why. | Same pipeline gaps as C, plus no per-request `wait_reason` and no per-pass logging. |

Each report gets a named regression test in a named phase (see "Report → fix → test").

## What's actually failing (the diagnosis)

`keitora send` already delivers through herdr (`agent prompt`, or paste then Enter). Delivery
isn't the problem. What fails is Keitora's bookkeeping around it. All references below were
re-verified against the code in this cycle.

- **Liveness and activity come from hook-written rows**, and hooks are the least reliable part
  across providers:
  - Codex loads project hooks only for trusted projects. The hook-trust bypass
    (`--dangerously-bypass-hook-trust`) is passed on every normal launch, but the Codex
    **transfer-strict** launch (`LaunchPolicy::TransferStrict`, `backend/herdr/strict.rs`
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

  `apply_reconcile` (`backend/mod.rs`) never lifts `offline` and never applies `idle`. A
  crashed agent's leftover shell pane stays in the live set.
- **Turn completion depends on a Stop hook.** When it doesn't run, `wait` hangs. `select_event`
  (`keitora-cli/src/drive.rs`) only reports `Stalled` for a raw `busy` row. The stall clock is
  `last_heartbeat`, which only hooks move, so a long healthy turn also reads as stalled.
- **Requests are picked up only on a `tasks.db*` file event,** on a window open-set change
  (`drive_set_open_projects` → `retrigger_pending`), or by the single post-wake
  `recheck_after_settle`. There's no periodic pass.
  - A wait condition that clears without a DB write is never rechecked.
  - Watcher threads exit silently: the `notify` watcher fails to start, `watch()` fails, or the
    background watcher's store open fails. `notify` error events are dropped too.
  - The active-project watcher is recreated only on project activation; background watchers only
    on registry changes.
  - "No running Keitora app picked it up" is printed by expiry, which runs inside a running
    app's pass (`housekeeping`).

## Key finding

**herdr already knows what we need, without hooks** (`research-herdr.md`, re-verified against
the 0.9.5 binary and bundled schema).

- herdr derives `agent_status` (`idle`, `working`, `blocked`, `done`, `unknown`) for Claude,
  Codex and agy by **matching the screen** against detection rules it can update remotely.
- herdr's own integration hooks only record session ids. They're user-level, so project trust
  doesn't affect status.
- `agent list` reports fields Keitora ignores today:
  - `completion_seq`: nullable; "the current idle transition completed work";
  - `state_change_seq`;
  - `launch_pending`, `interactive_ready`, `screen_detection_skipped` (all optional in the
    schema);
  - `agent_session`.

  `pane list` (`PaneInfo`) has none of these, so `agent list` is required.
- **Push is not practically available to Keitora.**
  - The socket API's `events.subscribe` exists, but no CLI command streams it.
  - Keitora talks to herdr only by spawning the CLI.
  - On Windows the socket path is a pointer file (`<pid>:<nonce>`) to an undocumented pipe.
  - No event carries the seq counters anyway.

  So the engine **polls `agent list`** (one CLI call per tick covers every agent), at an
  adaptive cadence. Push stays a follow-up behind Spike 2 (see Decisions D2).

Known limits, each with a mitigation in §2:
- An unrecognised screen of a known agent falls back to `idle`.
- `blocked` is reported only for approval screens herdr recognises.
- agy reports `idle` while its background tasks or subagents still run.

## Architecture

### 1. The session engine: one source of truth, in the app

A single app-side module, the **session engine**, owns the per-worktree view. Every consumer
reads its output. Nothing else interprets raw signals.

| Question | Primary signal | Secondary or enrichment | Never |
|---|---|---|---|
| **Is an agent running?** | herdr `agent list`: an entry for the worktree's pane (matched by keymap `pane_id`) with `agent` set, or `launch_pending` | Keitora's launch records (it started `{kind}-{hash}`) | the pane's existence alone (a leftover shell); a hook row |
| **Idle, busy or blocked?** | herdr `agent_status`, settled (see §2) | Hook rows under the most-recent-evidence rule; the gate's brokered-prompt rows (`blocked` → "driver deciding") | a hook row older than herdr's last status change |
| **Did the turn finish?** | the turn predicate over the delivery baseline (§2) | a Stop hook / agy `fullyIdle` Stop for that delivery | waiting forever on a hook |
| **What did it say?** | Stop-hook reply for that delivery | transcript (Claude via `--session-id`, Codex rollout `task_complete`); then `reply_source: "none"` and a `keitora read` hint | — |
| **Which session is this?** | herdr `agent_session` (already in `resume.db`) | hook `session_id` | worktree name alone |
| **Permission decision** | hooks (the only source) | — | — |

**Polling.** The engine is a due item in the existing herdr status loop (`status_loop.rs`, 1 s
wake). It reuses that loop's server gate and headroom logic instead of adding a thread.

| Mode | When | `agent list` every |
|---|---|---|
| hot | any awaited turn, pending drive or dispatch request, or launch in flight | 1 s (2 s under low headroom) |
| warm | the window is open, nothing pending | 5 s (15 s under low headroom) |
| cold | window hidden or closed, nothing pending | 15 s (30 s under low headroom) |

- Each tick is a single `agent list` call. `pane list` stays on its existing cadence for the
  live set and the resume store.
- Every call asks the `ServerGate` first (`admit(None)`). While recovery isn't healthy, the
  engine doesn't call herdr. It marks the link `recovering` (see Failure modes).

**Derived view per `(project, worktree)`:**

| Field | Values / meaning |
|---|---|
| `state` | `idle`, `busy`, `waiting` or `offline` (the existing vocabulary, so every consumer keeps working) |
| `agent` | `present`, `starting` or `absent` |
| `herdr_status` | the raw herdr status |
| `settled` | idle confirmed (see §2) |
| `stale` | no fresh herdr snapshot |
| `pane_id`, `terminal_id`, `agent_session` | identity of the pane occupant |
| `state_change_seq`, `completion_seq` | herdr's counters |
| `last_change_at` | when the view last changed |
| `brokered`, `escalated_reason` | from the gate's prompt rows |

Mapping:
- **Agent absent → `offline`,** whatever the rows say. A bare shell is offline.
- **`launch_pending`, or not `interactive_ready` → `busy`,** with `agent: starting`. Send and
  dispatch wait; they don't refuse.
- **herdr `working` → `busy`; `blocked` → `waiting`.**
- **`idle`/`done` → `idle`,** but only once settled. `done` = idle and unseen.
- **`unknown` → keep the last state,** and add a diagnostic after 30 s.
- **Hook rows under the most-recent-evidence rule.** A hook `busy`/`waiting` write *newer than
  herdr's last status change* overrides herdr `idle`, until herdr changes again. A hook
  `idle`/`offline` never overrides herdr presence or `working`/`blocked`.
  - This catches herdr's idle fallback and agy background work, whenever hooks work.
  - A stale `busy` (a missed Stop) or a late `SessionEnd` loses automatically, because herdr's
    next change is newer.

**Outputs.**
- **In the app:**
  - `sessions://changed`, emitted by the engine on any view change (today it fires only on DB
    writes);
  - `sessions_list` and `projects_overview`;
  - the activity working dot;
  - `bg_notifier`, which now runs on engine changes instead of raw rows.
- **The status store** (Decisions D4), which the CLI reads:
  - a global `<KEITORA_HOME>/status.db`, keyed by `(project, worktree)`;
  - the app is its single writer, holding a lease; the CLI reads it read-only;
  - rows are written only on change, plus a 5 s writer heartbeat.

**Consumers migrate to the engine:**
- the rail, lane state, attention, toasts and `bg_notifier`;
- `sessions_list`, `projects_overview` and dispatch idle gating;
- the drive runner's `send`, `wake`, `queue`, `awake()` and `send_settling`;
- the CLI's `drives`, `wait`, `read --last`, `task watch` and the gate's driver-liveness check
  (from `status.db` while it's fresh, else today's raw row);
- agent updates and the quit prompt (agent present, not pane present);
- recovery and resume, where they ask "running?".

`apply_reconcile` and its one-way rules are deleted. `reconciled_sessions_in` becomes a thin
read of the engine view, filtered to configured worktrees. Rows for unconfigured names are
reported as a finding, not dropped silently.

**Hook rows become inputs, not truth.** Hooks still write `worktree_sessions` and
`session_replies` (compatible with older CLIs on PATH). The engine reads them as evidence. The
stale-writer problem goes away because `offline` no longer comes from `SessionEnd`.

### 2. Turns: delivery baseline, completion, human turns

**Baseline.** `deliver_send` reads a **fresh** `agent list` entry for the target (never the
cached tick) right before `agent prompt`. Its baseline is `B = {pane_id, terminal_id,
agent_session, state_change_seq, completion_seq, status, delivered_at}`, and the send is
refused (left queued) unless `B.status` is idle or done, settled, and `interactive_ready`.

- `B` is persisted in `tasks.db` on the `drives` row, as new columns next to `awaiting_turn`
  along with a monotonic `delivery_id`, in the same write that sets `awaiting_turn`.
- So a turn that completes while the app restarts is still resolved: the first snapshot after
  startup compares against the persisted `B`.

**Settled idle.** `idle`/`done` counts only when seen on two consecutive snapshots at least
1 s apart with `state_change_seq` unchanged. This damps a resize or redraw that briefly matches
idle.

**Turn predicate.** On a snapshot with the same occupant as `B` (same `pane_id` and
`terminal_id`; the same `agent_session`, or one newly set), the delivery's turn is **finished**
when status is settled idle and any of these holds:
1. `completion_seq` is non-null and differs from `B.completion_seq`. "Differs", not "is
   greater": this is correct whether the field is a counter or the current idle transition's id
   (research-herdr correction 6).
2. `state_change_seq ≥ B.state_change_seq + 2`: some status change happened and came back to
   idle.
3. The engine itself observed `working` or `blocked` after `delivered_at`.

A Stop hook (or agy Stop with `fullyIdle`) written after `delivered_at` also finishes the turn.
Either source finishes it, the first one wins, and the turn is deduped by `delivery_id`.

**Outcomes other than finished.** `wait` always terminates:

| Condition | `wait` event |
|---|---|
| Finished | `turn_finished {reply, reply_source, caveat?}` |
| Brokered prompt pending | `prompt` (unchanged) |
| herdr `blocked`, no brokered prompt, for 10 s | `stalled {reason: "blocked on a dialog nothing brokers"}` |
| Still idle with no seq change 30 s after delivery | `stalled {reason: "no activity observed since delivery"}` (the prompt may not have been submitted, or herdr can't read this UI) |
| Occupant changed (restart, `/clear`, recovery resume) | `stalled {reason: "agent restarted mid-turn; result unknown"}` |
| herdr unavailable or recovering for `--stall-after` | `stalled {reason: "herdr unavailable since T"}` |
| `status.db` stale (no app) for `--stall-after` | `stalled {reason: "no Keitora app is updating status"}`, after the legacy hook path below finds nothing |
| herdr `working` | never a stall: a long healthy turn is not stalled |

**Reply text.** On completion the engine collects the reply in this order:
1. A Stop-hook reply row written after `delivered_at`. The engine waits up to 3 s for it after
   a herdr-detected completion.
2. The transcript (Phase 5).
3. None, with `reply_source: "none"` and a hint to use `keitora read`.

For agy, a herdr-only completion carries `caveat: "agy may still run background tasks"`.

**Turn events.** The engine records each completion of a **driven** worktree as a row in a new
`drive_turns` table in the project's `tasks.db`:
`{id, driven, kind: driver|human, delivery_id?, completed_at, signal: herdr|hook, reply,
reply_source, caveat, acked_at}`.
- The app is the only writer.
- The write is meant to fire the `tasks.db` watchers and `wait`'s `data_version`. A completion
  means the target is idle, so queued sends should get a pass.
- Rows are pruned 7 days after ack, keeping the last 50 per worktree.

**Human turns.** The user decided these surface as a separate `human_turn` event. Attribution
uses a per-worktree ledger:
- Each settled completion is identified by its `(completion_seq, state_change_seq)` at idle.
  The engine remembers the last one it recorded.
- A new completion is a **driver** turn if an outstanding delivery exists whose `B` predates it
  and which isn't yet resolved. That is the first completion past the baseline.
- Any other new completion of a driven worktree is a **human** turn. Examples:
  - the human typed while nothing was outstanding;
  - a second completion after the driver's turn resolved (a message the human queued
    mid-turn);
  - a completion while the driver's send is still queued and undelivered.
- Race: the human submits at the same instant a driver send is delivered. The first completion
  is attributed to the driver. This is accepted and documented, because sends require settled
  idle.
- Completions that happened before the engine's first snapshot produce no events. The
  exception is a persisted outstanding `B`, which resolves the driver turn.

`wait` changes:
- Event priority: `prompt` > `turn_finished` > `human_turn` > `send_refused` > `stalled`.
- Each `human_turn` is delivered once (it's acked).
- "Nothing to wait for" (exit 4) now also requires no unacked human turn.
- `wait` polls both `tasks.db` and `status.db` `data_version`. That's one extra cheap `data_version`
  read per 500 ms.

**Legacy path.** When `status.db` is stale (no app, or an old app), `wait` falls back to today's
`session_replies.turn ≥ awaiting_turn` check, with `reply_source: "hook"` and attribution
unknown. With a fresh app it uses only `drive_turns`, so a turn is never delivered twice. An
older CLI on PATH keeps working through `session_replies`.

### 3. Hook placement: fewer ways to fail

Hooks now matter for permission brokering, reply text and the busy veto, not for liveness or
completion.

- **Claude:** pass the hooks per launch with `--settings '<json>'` (the gate, Stop capture and
  `SessionStart`), so they don't depend on project files or a reinstall after a provider change.
  Gated by Spike 3. If per-launch hooks don't fire in an untrusted or not-yet-trusted worktree,
  keep `.claude/settings.local.json` and install it on every launch and provider change instead.
- **Codex:**
  - Keep `.codex/hooks.json` (the `PermissionRequest` gate needs it), installed on every launch
    and provider change, not only at startup.
  - Pass `--dangerously-bypass-hook-trust` on every strict-chain step. The last step drops only
    flags that can fail a launch, and this flag doesn't.
  - No `-c notify` (Decisions D6).
- **agy:**
  - Write the CLI's absolute path into the global hooks file at install time, plus a marker
    env var so the hook does nothing outside Keitora panes.
  - Honour `ANTIGRAVITY_CLI_CONFIG_DIR` for the file location.
  - Re-assert Keitora's key after herdr's `integration install`.
  - Global writes stay behind `global_writes` (off under `KEITORA_HERDR_NO_INTEGRATION`).
  - agy has no permission event, so it stays unbrokered.
- **Provider changes:** `reset --to`, wake `--to`, the UI switch and transfer prepare the target
  provider fully: hooks, trust bypass and per-launch flags. `prepare_launch` covers all three
  providers.
- **Stale-writer guard:** a hook write carrying a `session_id` different from herdr's current
  `agent_session` for that pane is recorded as evidence of an old session and ignored.
- **Hook error log:** the CLI logs its own failures when run as a hook to a small rotating log
  under the Keitora home. Hooks no longer lose errors to `/dev/null`.

### 4. Request pipeline

- **Engine-driven passes.** When the engine's view changes for a worktree that has pending
  requests (it settles idle, an agent appears, or a launch finishes), the engine calls
  `schedule_pass(project, 0)`, deduped per project. This replaces "wait for a hook DB write".
- **Self-rescheduling passes.** A pass that leaves rows pending schedules a recheck after 2 s,
  deduped.
- **Safety sweep.** Every 10 s the status loop checks each registered project for pending
  requests (one read-only `LIMIT 1` query per project) and runs a pass for any that has some.
  Watchers become an optimisation, not a dependency.
- **Watchers are supervised.** Each watcher records `started_at`, `last_event_at` and
  `last_pass_at`. The supervisor runs in the status loop every 10 s:
  - a watcher thread that has exited (start failure, store-open failure, disconnect) is
    restarted, with a finding;
  - a watcher silent while a request ages past 15 s gets one restart, and the sweep covers the
    gap.
  - `notify` error events are logged, not dropped.
- **Reads never queue behind gated sends.** A pass resolves every `read` before it takes the
  `DispatchGate` for any send.
- **Every request records `seen_at` (once) and `wait_reason` (only when it changes)**, for
  example:
  - "window hasn't reported open projects";
  - "waiting for @bravo to be idle (herdr: working)";
  - "waiting for herdr (recovering)";
  - "waiting on the wake slot";
  - "project not open in the window".

  Expiry carries the reason.
- **No write loops.** Passes write only on change. A pass triggered by its own write finds
  nothing new and writes nothing. A test asserts that N passes with no external change cause
  zero writes.
- **App presence lives in `status.db`,** not in a separate file (Decisions D4):
  `writer {instance_id, pid, version, started_at, heartbeat_at}` plus
  `app_projects {project, open, db_path, watcher_started_at, watcher_last_event_at,
  last_pass_at, last_error}`. The CLI's timeout message chooses between:
  - "no Keitora app is running" (no fresh heartbeat);
  - "app running, `<project>` not open in its window";
  - "app running, its watcher for `<project>` hasn't fired since T";
  - "app saw it: `<wait_reason>`".
- **Per-pass logging:** the open-project set, the resolved DB path, and the request ids seen,
  plus one outcome line per request id.

### 5. Self-diagnostics: the app watches itself

A health monitor runs on engine updates and on a slow timer (about 30 s). It records findings
per worktree and per project in `status.db`.

| Check | Example finding | Auto-remedy |
|---|---|---|
| Hook vs herdr disagreement | "bravo: herdr says agent idle, hooks say offline. Hooks aren't writing for Codex here" | none (the engine already ignores the stale row) |
| Completion without Stop | "bravo: 3 turns finished with no Stop hook. Reply text comes from the transcript" | transcript fallback |
| Turn start not observed | "bravo: a send showed no herdr activity for 30 s. herdr may not recognise this UI" | none (report; `agent explain` hint) |
| Hooks missing or untrusted | ".codex/hooks.json missing; launch ran without the trust bypass; Claude settings not applied" | reinstall on the next launch |
| agy plumbing | "`hooks.json` has no keitora key; `KEITORA_BIN` unset in pane; app is translocated" | re-assert the key |
| Worktree identity | "git folder name ≠ configured name `bravo`" | none (report) |
| Request pipeline | "request 25 pending 12 s, no pass since T; watcher restarted" | watcher restart + sweep |
| herdr link | "herdr recovering; status stale since T" | none (wait for recovery) |
| Status writer | "another Keitora instance (pid N) owns status.db" | none (this instance stays read-only) |
| Version skew | "the PATH `keitora` is older than the app" | refresh the managed copy |

Surfaces:
- a small warning glyph on the lane or project header, with a tooltip;
- a **Health** section in Settings with the full list and the last-seen times;
- `keitora status [<wt>] [--json]` (a new verb; no clash), so an orchestrating agent can ask
  *why* instead of guessing;
- drive verbs append the relevant finding to refusals and stalls, e.g. "refused: @bravo
  offline (herdr: no agent; last hook write 2 h ago)".

### 6. Failure modes

| Situation | Engine behaviour | Consumers |
|---|---|---|
| **App start, before the first snapshot** | The view is `stale` with `snapshot_at = null`. Nothing is derived from absence. The previous instance's rows are kept but marked stale. | Send, dispatch and wake leave requests pending ("waiting for first herdr snapshot"). The rail shows the last state dimmed. The CLI sees `engine_ready = false`. |
| **herdr down, restarting or recovering** (`ServerGate` refuses) | No herdr calls. The link is `recovering` and every view is `stale` (never `offline`). | Same as above, with "waiting for herdr (recovering)". `wait` stalls only after `--stall-after`. |
| **After recovery** | Full re-snapshot. A pane or occupant identity change invalidates baselines (seqs restart with a new server). | An outstanding turn ends as `stalled: agent restarted mid-turn`. Recovery's existing mid-turn nudge is unchanged. |
| **Unrecognised screen (idle fallback)** | Settled-idle damping; the hook busy veto (most-recent-evidence); the "turn start not observed" finding; herdr refuses `agent prompt` to `blocked`. | A send into a misdetected busy agent is possible only if hooks are also silent *and* herdr shows idle for 2 snapshots. Then the provider queues the text as the next message (Claude and Codex queue mid-turn input), and the delivery is attributed to the next completion. Accepted residual risk. |
| **agy idle during background work** | The busy veto from agy `PreInvocation` (when hooks work). Otherwise completion carries the agy caveat. | `turn_finished.caveat` tells the driver. |
| **A completion_seq reading we didn't expect** | Predicate clauses 2 and 3 still finish the turn. Spike 1 picks the primary clause. | — |
| **Two app instances on one `KEITORA_HOME`** (installed + non-isolated dev) | A lease on the `writer` row: take over only if the heartbeat is older than 15 s. The loser computes its view but doesn't write `status.db` or `drive_turns`, and shows a finding. | The CLI reads one consistent writer. The isolated dev app has its own `KEITORA_HOME`, so it never contends. |
| **App crash** | — | `heartbeat_at` goes stale within 15 s. The CLI reports "no Keitora app is updating status" and uses the legacy hook path. |
| **`status.db` missing or corrupt, or a schema mismatch** | It's a cache: recreate it at startup. | The CLI treats it as stale until the first write. |

## Scope and phasing

**One PR** (user decision) on this branch, built in phases. After each phase:
- `cargo test --workspace` and `yarn test` are green;
- run `/simplify`, then `/code-review high`, and fix the findings before the next phase.

| Phase | Content | Reports fixed |
|---|---|---|
| **0. Isolation hardening** (prerequisite for spikes) | `herdrDevEnv` strips inherited `HERDR_*` (socket, session, pane, env, workspace, tab, bin); `HerdrEnv::apply` removes `HERDR_SOCKET_PATH`/`HERDR_SESSION`; the launcher re-runs `isolationVerdict` right before spawning the app. Tests in `dev-herdr.test.ts` and `env.rs`. | — |
| **1. Engine core (app side)** | `AgentInfo` parses the new fields (optional, defaulted). The engine runs as a status-loop due item with adaptive cadence and gate awareness. The `status.db` writer has a lease and heartbeat. App consumers migrate (rail, `sessions_list`, `projects_overview`, activity, `bg_notifier`, dispatch, the drive runner's send/wake/queue/settle, agent updates, quit). `apply_reconcile` is deleted. The fake backend gains agent list with seqs. | A1, A2 |
| **2. Turns and CLI** | Delivery baseline, `delivery_id`, the turn predicate, `drive_turns`, the human-turn ledger. CLI `wait` (new events, stall reasons, dual `data_version`, legacy path), `drives`, `read --last`, `task watch`, and the gate's driver check read `status.db` while it's fresh. | B (hang), A2 (`drives` consistency) |
| **3. Request pipeline** | Engine-driven passes, self-rescheduling, the safety sweep, watcher supervision, reads before gated sends, `seen_at`/`wait_reason`, presence rows, CLI timeout messages, per-pass logging. | C, R |
| **4. Hook placement** | Codex trust bypass on every strict step; all-provider `prepare_launch`; Claude `--settings` (per Spike 3); agy absolute path, marker, re-assert and `ANTIGRAVITY_CLI_CONFIG_DIR`; stale-writer guard; hook error log. | A1 (agy rows), A2 (hooks after `reset --to`), B (Codex Stop) |
| **5. Reply text fallbacks** | Claude transcript via `--session-id`, Codex rollout `task_complete`, backed by captured fixtures (Spike 5). | B (`read --last` empty) |
| **6. Self-diagnostics** | Health monitor, Health settings, lane glyph, `keitora status`, findings appended to refusals and stalls. | all (diagnosability) |

Phase 6 comes last because it consumes signals from every other phase.

**Out:** agy prompt brokering; a herdr push subscription (follow-up, behind Spike 2); Codex
`notify`; a Codex `app-server` or Claude Channels integration; replacing the interactive TUIs.

## Report → fix → test

| Report | Phase | Fix | Regression test (fixture: fake herdr + temp DBs + fake clock) |
|---|---|---|---|
| A1 | 1 | Presence from herdr `agent list` | A missing row with agy present and idle → `idle` in the rail, `sessions_list`, send and `drives`. A stale `offline` row with an agent present → `idle`. A bare shell (agent absent) with an `idle` row → `offline`. |
| A1 | 4 | agy absolute path + re-assert | Install writes an absolute path; re-assert after a simulated herdr rewrite restores the key; the marker env makes the hook a no-op outside panes. |
| A2 | 1 | One liveness answer | Property test over provider × row state × herdr state: `wake` reports `already_awake` ⇔ send doesn't refuse as offline. A late `SessionEnd` after reset leaves the view idle. |
| A2 | 4 | Hooks on provider change | `reset --to codex` and wake `--to` call `prepare_launch` for Codex (the fake host records it); the real host writes `.codex/hooks.json` in a temp worktree; every strict step carries the trust bypass. |
| B | 2 | Completion from herdr | A Codex delivery with no Stop: `completion_seq` changes → `wait` returns `turn_finished` with `reply_source: none` within one hot tick. The idle-no-change case → `stalled` at 30 s. `working` for 10 minutes → no stall. A human typing after resolution → `human_turn`. |
| B | 5 | Reply from rollout | A captured rollout fixture yields the reply; `read --last` returns it. |
| C | 3 | Pipeline | A herdr-only idle flip schedules a pass and applies a queued send with no DB write. A `read` resolves while an older send holds the gate. A dead watcher is restarted and the sweep claims meanwhile. Expiry carries `wait_reason`. The CLI timeout text covers presence absent, stale and fresh. |
| R | 3 | Pipeline + presence | An open project claims a ready lane's request exactly once. A window close/reopen and a project switch away and back restore pickup. A request outside the open set gets an explicit diagnostic. Zero writes across idle passes. |

## Spikes

All spikes run on this Windows machine through the isolated launcher (`yarn dev:isolated`,
`scripts/dev-herdr.mjs`) after Phase 0 lands. They must never touch the user's running herdr.
Spikes 1–3 and 5 are blocking for the phases that use them. Spike 2 is not blocking.

### Spike 0: isolation gate (run first, every session)

1. Open a **fresh** terminal from the Start menu, not a Keitora or herdr pane. Check
   `$env:HERDR_SOCKET_PATH` and `$env:HERDR_SESSION` are empty. Abort if set and Phase 0
   hasn't landed.
2. Record the real server:
   `Get-CimInstance Win32_Process -Filter "Name='herdr.exe'" | Select ProcessId,ParentProcessId,CreationDate`,
   plus the mtime of `~/.keitora/herdr/config/herdr/herdr.sock`. Don't print its contents.
3. Run `yarn dev:isolated`. Abort if it prints `refusing to launch`.
4. In the app's console, the line `herdr: at launch the server is … : <Class>` must say `Fresh`.
   **Abort** on `Adopt`, `RecoverDifferentInstance` or `Indeterminate`: close the window, and
   the launcher refuses to stop a server it can't prove is its own.
5. After the window is up:
   - the dev home's `…-herdr/herdr/config/herdr/herdr.sock` exists, and its pid field is a
     **new** `herdr.exe` that isn't in the step-2 set;
   - the real `herdr.sock` mtime is unchanged;
   - every step-2 pid is still alive.

   Abort and close the dev window if any check fails.
6. Run every probe below from a **shell pane inside the dev app**. Its `HERDR_SOCKET_PATH`
   points into the dev home; `echo` it first and abort if it doesn't.
7. Teardown: close the window. Expect the launcher to stop only the isolated server. Re-run the
   step-5 checks against the real server.

### Spike 1: herdr `agent list` semantics and latency (blocking for Phases 1–2)

- **Probe:** in a dev shell pane, run
  `while ($true) { "$(Get-Date -Format o) $(herdr agent list)" >> $env:KEITORA_HOME\spike1.ndjson; Start-Sleep -Milliseconds 250 }`.
- **Per provider** (claude, codex, agy), launched from the dev UI in a demo worktree:
  1. a fresh launch to ready;
  2. a short prompt ("reply with ok");
  3. a tool turn;
  4. a permission prompt (autonomy off);
  5. an interrupt mid-turn;
  6. `/clear`;
  7. two prompts back to back;
  8. focusing the pane after a turn (`done` → `idle`);
  9. agy only: a prompt that starts background work;
  10. codex only: one launch without `--dangerously-bypass-hook-trust` (does `agent_session`
      still appear?).
- **Record:**
  - `completion_seq` before the first turn, after each turn, on an interrupt, and on blocked →
    idle;
  - its relation to `state_change_seq`;
  - whether `launch_pending`/`interactive_ready` are present;
  - latency from submit to `working`, and from the visibly finished screen to `idle`;
  - whether `pane list` ever reports `done`.

### Spike 2: push transport (not blocking)

- Read herdr 0.9.5's socket code (public source, read-only) for the Windows pipe naming behind
  the pointer file.
- Run a scratch client (outside the repo) against the **isolated** server only. Subscribe to
  global `pane.updated`, `pane.agent_detected` and `pane.exited`, plus one per-pane
  `pane.agent_status_changed`.
- Record whether `pane.updated` fires on every status change, the reconnect behaviour, and
  `events_lost`.

### Spike 3: Claude per-launch hooks (blocking for Phase 4, Claude part)

In a dev shell pane, in a never-trusted demo worktree, run
`claude --settings <devhome>\spike3.json`. The file's `SessionStart`/`Stop`/`PermissionRequest`
hooks append to `<devhome>\spike3.log`. Record:
- whether they fire before and after the trust dialog;
- whether the user-level hooks (herdr's `SessionStart`) still fire alongside them;
- whether a JSON string works as well as a file path on Windows quoting through herdr
  `agent start`.

### Spike 5: transcript fixtures (blocking for Phase 5)

- From the Spike 1 runs, copy the Claude `.jsonl` and Codex rollout files for the **dev repo
  paths only**, and scrub them (placeholder paths and names, no prompts with real content).
- Cover a plain turn, a tool turn, a denied permission, an interrupt, `/clear`, compaction and a
  Codex resume.
- Confirm Claude's final-line `stop_reason` and Codex `task_complete`/`turn_aborted`.

(Spike 4, Codex `-c notify`, is dropped: Decisions D6.)

### What each spike result changes

| Spike result | Design change |
|---|---|
| S0 fails (adopts or a second server is missing) even after Phase 0 | Stop all live spikes. Build Phases 1–2 on the fake backend with the schema-derived semantics, and keep every predicate clause enabled. Spikes move to a machine without a running herdr. |
| S1: `completion_seq` is a counter (+1 per working→idle) | Clause 1 is primary; clauses 2–3 stay as fallbacks. |
| S1: `completion_seq` is the current idle transition's id | No change: "differs" already handles it. Document it. |
| S1: `completion_seq` absent or null on some provider | That provider uses clauses 2–3. Diagnostics note it. |
| S1: submit→`working` latency > 1 s | The send-settle hold and the "no activity" stall stay at 30 s. The hot cadence stays at 1 s (polling can't beat detection). |
| S1: `interactive_ready` absent | Gate on `launch_pending` and a non-`unknown` status only. |
| S1: agy background work shows `idle` | Confirms the caveat and the busy veto. No change. |
| S1: Codex without the bypass loses `agent_session` | Raise the Phase 4 bypass fix's priority. Resume diagnostics name it. |
| S2: a stable transport + a global status event | Push becomes a follow-up task (event source → same engine input). Not in this PR. |
| S2: no stable transport | Polling is final. Record it in herdr notes. |
| S3: per-launch hooks fire in untrusted worktrees | Phase 4 moves Claude hooks to `--settings`; the project file stays as a fallback for hand-started sessions. |
| S3: they don't (or quoting fails on Windows) | Keep project-file hooks, installed on every launch and provider change. Note the trust requirement in diagnostics. |
| S5: Claude final line lacks a reliable `end_turn` | The Claude transcript is used only for reply text after a herdr- or hook-detected completion, never as a completion signal. That's already the design. |

## Testing

- **Every phase ships fixture tests** using the fake herdr backend (extended with `agent list`
  entries, seqs, `launch_pending`/`interactive_ready`, and occupant identity), temp DBs and a
  fake clock. There's no live herdr in tests (`cfg(test)` already skips integration installs and
  global writes).
- **Engine table tests:** the mapping (presence × status × hook row × age), settled-idle
  damping, the most-recent-evidence rule, and the stale/recovering view.
- **Turn predicate tests:** each clause alone, an occupant change, a herdr restart (seqs reset),
  a persisted baseline across an app restart, and human-turn attribution including the queued
  mid-turn message.
- **Store tests:** the `status.db` lease takeover and refusal, heartbeat staleness for the CLI,
  recreate on corruption, `drive_turns` ack and prune, and zero writes with no change.
- **Each report's regression test** is in the table above.
- **Health monitor:** each check fires on its fixture and clears when fixed.
- **macOS CI:** a WAL-only write from a second connection fires the watcher within 2 s,
  including after a checkpoint.
- **Launcher:** `dev-herdr.test.ts` covers stripping `HERDR_*` and the re-verdict before
  spawn.

## Decisions

- **D1. Spike venue:** this Windows machine, through `yarn dev:isolated`, gated by Spike 0 and
  Phase 0. (User decision.)
- **D2. Push vs poll:** poll `agent list` adaptively. No CLI command streams events, and the
  Windows transport is an undocumented pointer file. No event carries the seq counters, so
  completion needs `agent list` anyway. Push is a possible follow-up only.
- **D3. PR shape:** one PR, with phases 0–6 above and `/simplify` + `/code-review high` after
  each. (User decision.)
- **D4. Status store:** a global `<KEITORA_HOME>/status.db` for the live view, presence and
  findings, plus a `drive_turns` table in each project's `tasks.db` for turn events. Compared
  options:
  - **Presence file (JSON):** whole-file rewrites per change; Windows rename/sharing hazards
    with concurrent readers; no transactions; no `data_version`. Rejected.
  - **A table in `tasks.db` for the live view:** every state flip would fire all `tasks.db`
    watchers, bg toast passes and drive passes (the loop risk the proposal wanted to avoid).
    Rows are project-less, and the DB may be shared by many worktrees. Rejected for the
    high-churn view.
  - **`status.db` beside each `tasks.db`:** one file per project, with N writers, N leases and
    N heartbeats, and the CLI needs the project's `db_path` before it can even say "no app
    running". Rejected in favour of the global file.
  - **Chosen: global `status.db`.**
    - It's keyed like `SessionKey` and the global `resume.db`.
    - It has one writer connection, one lease, and WAL with the CLI read-only.
    - It's outside every `tasks.db*` watcher by name and location.
    - The isolated dev app gets its own automatically (separate `KEITORA_HOME`).
    - It's a disposable cache.

    Turn events go to `tasks.db` on purpose: they're durable, must be acked transactionally next
    to `drives.awaiting_turn`, and *should* wake passes and `wait`. They're low-frequency (one per
    turn).
- **D5. Human-typed turns:** a separate `human_turn` event (user decision), attributed by the
  ledger in §2.
- **D6. Codex `notify`:** dropped. herdr completion plus the rollout covers it, and a per-launch
  `notify` would replace the user's own `notify` (the Codex desktop app's) inside Keitora panes.
- **D7. The CLI never talks to herdr.** It reads `status.db`/`tasks.db` only. Rejects
  research-herdr's "`wait` shells out to `herdr agent wait`".
- **D8. Hook state stays as evidence** (no hooks removed). Older CLIs on PATH keep working
  through `session_replies`/`worktree_sessions`.

## Open questions

None block refine. One needs the human before the first live spike:

1. **Running real agents for spikes.** Spikes 1, 3 and 5 launch real Claude, Codex and agy
   sessions in the dev app. They use your real agent logins and quota. They write transcripts
   into your real `~/.claude/projects` and Codex sessions dirs (for the throwaway dev repo paths
   only), and they keep your machine busy for roughly 30–60 minutes. OK to run them on this
   machine while your main herdr is in use, or should they wait for a quiet window?

## Complexity

**Large,** one PR in 7 phases. Recommend: finish refine on this architecture → Fracture into
phase docs → Singletonize → Loss check → Sequencing per phase.

## Implementation notes

These are the concrete decisions taken to close gaps found in this review. **Where they
conflict with earlier text, these win.**

1. **`AgentInfo` parsing:**
   - add `completion_seq: Option<u64>`, `state_change_seq: u64` (default 0),
     `interactive_ready: Option<bool>`, `screen_detection_skipped: bool` (default false),
     `agent_session`, `terminal_id` and `revision`;
   - make `launch_pending` default to false when absent;
   - match entries to worktrees by keymap `pane_id`, never by name alone. Agents Keitora didn't
     start (the user ran `claude` by hand) still count as present.
2. **Engine placement:** a pure `derive(snapshot, rows, prompts, prev, now) -> View` function
   plus a thin driver in the status loop. All the logic is in the pure function, so the table
   tests need no herdr.
3. **Freshness:** a view is `stale` when its snapshot is older than 3× the current cadence or
   the gate refuses. Stale views never produce `offline` or a refusal; they produce "wait".
4. **Cadence change:** the engine reports its mode (hot/warm/cold) to the status loop each tick,
   so the loop picks the next due time without a new thread.
5. **`status.db` schema** (version-stamped; drop and recreate on mismatch):
   - `writer(id=1, instance_id, pid, version, started_at, heartbeat_at, last_snapshot_at,
     herdr_link)`;
   - `app_projects(...)` as in §4;
   - `sessions(project, worktree, state, agent, herdr_status, settled, stale, pane_id,
     terminal_id, agent_session, state_change_seq, completion_seq, brokered, escalated_reason,
     last_change_at)`;
   - `findings(project, worktree, check, severity, message, first_seen, last_seen)`.

   It uses WAL with `synchronous=NORMAL`. The CLI opens it with the existing read-only open
   pattern and a busy timeout.
6. **Freshness rule for the CLI:** `heartbeat_at` within 15 s. The heartbeat is written every
   5 s and touches only the `writer` row.
7. **`keitora_home()`** moves from `src-tauri/src/paths.rs` into `keitora-tasks`, so the CLI
   resolves the same `status.db`.
8. **`drives` gains** `delivery_id INTEGER`, `baseline_json TEXT` and `delivered_at TEXT`,
   additive. `awaiting_turn` keeps its meaning for older CLIs. The new CLI acks by
   `delivery_id` (CAS) and also clears `awaiting_turn`.
9. **The send gate** = engine `state == idle && settled && agent == present &&
   interactive_ready != Some(false) && !stale`, plus the existing `send_settling` hold, which is
   now released by herdr `working`/seq change instead of a hook turn bump.
10. **Gate (permission hook) driver check:**
    - while `status.db` is fresh, it uses the driver's engine view (`agent == present`);
    - while it's stale, it uses today's raw row;
    - `stop_reason` uses the same function.
11. **`bg_notifier`** keeps its per-project watcher for dispatch requests. Its toast logic moves
    to an engine subscription (in-process), so toasts follow herdr-only transitions.
12. **The safety sweep, watcher supervisor and health monitor** are status-loop due items (10 s,
    10 s, 30 s). Nothing new spins while nothing is pending except the 10 s sweep check, which
    costs one read-only query per registered project.
13. **Logging:** engine view changes at debug level; one info line per delivery, completion
    (with the clause that fired) and stall reason. That gives the next remote report a
    timeline.
14. **The launcher re-verdict** runs immediately before `spawn(appBin)`, using the same
    `herdrStatus` call. If it now fails, the launcher exits without starting the app.
15. **Placeholders:** fixtures use `alpha`/`bravo`/`my-game` and `/Users/me`/`C:/Users/x`
    paths. Code and comments carry no plan or task identifiers.
