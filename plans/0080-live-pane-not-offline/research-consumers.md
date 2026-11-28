# 0080 research: session state writers, consumers and inconsistencies

Read-only inventory of where per-worktree session state (`idle`/`busy`/`waiting`/`offline`),
turn completion, replies and brokered prompts come from, who reads them, and where the
readers disagree. File refs are `path:line` at the head of
`llm/andrew/keitora/0080-live-pane-not-offline`. Placeholder names only.

Tables involved (project `tasks.db`, schema in `crates/keitora-tasks/src/drive.rs:79-121` and
`local_store.rs`):

| Table | Key | Columns that matter |
|---|---|---|
| `worktree_sessions` | `worktree` (name only, no project) | `state`, `current_task_id`, `pid` (always NULL in practice), `last_heartbeat` |
| `session_replies` | `worktree` | `turn` (monotonic counter), `last_message`, `updated_at` |
| `drives` | `driven` | `driver`, `paused`, `escalate_after_s`, `awaiting_turn` |
| `drive_prompts` | `id` | `driven`, `driver`, `kind`, `status`, `decision_json`, `timeout_s`, `hook_pid` |
| `drive_requests` | `id` | `kind` (send/read/reset/swap/wake), `status`, `reason`, `result` |

A second, herdr-sourced store already exists: `resume_sessions` in the global
`<KEITORA_HOME>/herdr/resume.db` (`backend/herdr/env.rs:579`), written by
`ResumeStore::observe` (`backend/herdr/resume.rs:208`) from `pane list`. It holds
`session_id` (herdr's `agent_session`), `agent_present` and `cli` per `(project, worktree)`.
Nothing in the session-state path reads it today.

---

## 1. Writers

### 1.1 Hook installers (event → CLI command)

**Claude** — `.claude/settings.local.json` per worktree (`src-tauri/src/hooks.rs`).

| Event (matcher) | Command | Writes | Ref |
|---|---|---|---|
| `UserPromptSubmit` | `session busy` | `state=busy`, withdraws pending prompts | `hooks.rs:27` |
| `Notification` (`permission_prompt`) | `session waiting` | `state=waiting` | `hooks.rs:28-32` |
| `Stop` | `session idle --stdin` (fallback plain `session idle`) | reply row `turn+1`, then `state=idle`, withdraws prompts | `hooks.rs:33`, `:117` |
| `StopFailure` | `session idle --failed` (fallback `session idle`) | reply row `turn+1` with empty text, `state=idle` | `hooks.rs:34`, `:118` |
| `SessionStart` | `session idle` | `state=idle`, withdraws prompts | `hooks.rs:35` |
| `SessionEnd` | `session offline` | `state=offline`, withdraws prompts | `hooks.rs:36` |
| `PermissionRequest` (all) | `session gate --provider claude` | see gate (1.3) | `hooks.rs:42-43`, `:110-112` |
| `PreToolUse` (`AskUserQuestion`) | `session gate --provider claude` | see gate | same |

`SubagentStop` is deliberately absent (`hooks.rs:19-21`). All commands are POSIX with
`[ -x bin ] && … || true`; state commands silence stdout and stderr (`hooks.rs:66-68`).

**Codex** — `.codex/hooks.json` per worktree, only where the resolved cli is Codex
(`src-tauri/src/codex_hooks.rs`).

| Event | Command | Ref |
|---|---|---|
| `SessionStart` | `session idle` | `codex_hooks.rs:57` |
| `UserPromptSubmit` | `session busy` | `:58` |
| `Stop` | `session idle --stdin` (fallback `session idle`) | `:59`, `:130`/`:121` |
| `PermissionRequest` | `session gate --provider codex` (fallback `session waiting`) | `:60`, `:129`/`:118` |
| `SessionEnd` | `session offline` | `:61` |

No `StopFailure` analog and no `Notification`. Runs under PowerShell on Windows. Fires only if
the launch carries `--dangerously-bypass-hook-trust` (`backend/herdr/agent.rs:95-97`) or the
project layer is trusted (plan B1).

**Antigravity (`agy`)** — one global `~/.gemini/config/hooks.json` entry named `keitora`
(`src-tauri/src/antigravity_hooks.rs:26-30`), commands run `$KEITORA_BIN` and are a no-op
without it (`:40-48`).

| Event | Command | Ref |
|---|---|---|
| `SessionStart` | `session idle --stdin-payload` | `antigravity_hooks.rs:27` |
| `PreInvocation` | `session busy --stdin-payload` | `:28` |
| `Stop` | `session idle --stdin-payload --if-fully-idle` (also records a reply, usually empty text) | `:29`, `keitora-cli/src/main.rs:341-347` |

No `SessionEnd` (agy never reports `offline`), no waiting/permission hook, no gate.

**Install sites** (where the hook files get written):

| Trigger | Claude | Codex | agy | Ref |
|---|---|---|---|---|
| App startup | `install_session_hooks` (cwd + active project) | same loop, gated on `CliKind::Codex` | `sync_registry` | `host.rs:2583`, `host.rs:2625-2628`, `hooks.rs:435-510` |
| Project activation | `install_active_session_hooks` | same | `sync_registry` | `host.rs:1346-1347` |
| `set_worktrees` (re-discover/adopt) | `install_active_session_hooks` | same | `sync_registry` | `host.rs:514-516` |
| `worktree_create` | `install_hooks_for` | `install_codex_hooks_for` if Codex | `sync_worktree` if agy | `host.rs:556-570` |
| Settings provider change | — | — | `sync_registry` | `host.rs:1029` |
| Drive `reset --to` / wake `--to` | **no** | **no** | `prepare_launch` | `drive.rs:486-495`, `:1451` |
| UI agent switch | **no** | **no** | `sync_worktree` | `agent_switch.rs:154` |
| Transfer (claude <-> codex) | **no** | **no** | — | — |

### 1.2 CLI `keitora session <state>` (`crates/keitora-cli/src/main.rs`)

- Arg surface: `main.rs:43-63` (`state`, `--stdin-payload`, `--if-fully-idle`, `--provider`,
  `--stdin`, `--failed`).
- `gate` short-circuits before anything else (`main.rs:317-326` → `gate::gate_main`).
- `--stdin-payload` (agy): reads payload, `chdir`s into `cwd`/`workspacePaths[0]`, skips on
  bad JSON or `fullyIdle != true` (`main.rs:333-350`, `hook_payload.rs:41-63`). Errors are
  printed to stderr and swallowed (`main.rs:347-349`).
- Worktree identity: `cli_actor()` = `current_worktree()` = **git toplevel basename**
  (`main.rs:296-298`, `keitora-tasks/src/config.rs:3347-3363`), not the configured name.
- `run_session` (`main.rs:453-487`): for `--stdin`/`--failed`/agy turn end, records the reply
  first (`session_reply_record`, `main.rs:474-478`), then `session_upsert(me, state,
  active_task_id, None)` (`main.rs:479-480`). The reply is recorded even when the payload is
  unparseable (empty message) — so every Stop bumps `turn`.
- `stop_capture::capture_message` (`stop_capture.rs:135`): `--failed` → empty;
  `last_assistant_message` from payload; else tail of `transcript_path` (Claude/Codex JSONL
  parsers at `:55`, `:74`).

### 1.3 Gate `keitora session gate --provider <p>` (`crates/keitora-cli/src/gate.rs`)

- `gate_output` (`gate.rs:724-748`): undriven **Claude** returns before opening a writable store
  (no state write at all, `:736-738`); **Codex** writes `waiting` on every non-brokered path,
  including a parse error (`:742-744`, `:660-666`).
- `run_gate` (`gate.rs:653-713`): needs a drive row, an unpaused drive and a driver whose **raw**
  `worktree_sessions.state` is present and not `offline` (`:676-679`). Then inserts a
  `drive_prompts` row (`:688-697`, `drive_store.rs:457`), writes `waiting` (`:698`), polls
  (`poll`, `:581-648`), and after a driver decision writes `busy` via
  `session_resume_after_decision` unless another prompt is pending (`:706-710`,
  `drive_store.rs:929-957`).
- `poll`'s early stop (`stop_reason`, `gate.rs:567-577`) also reads the driver's **raw** row:
  `None`/`offline` → escalate "driver offline".
- On escalation/timeout the gate prints nothing; the provider shows its native dialog. Nothing
  writes `busy` when the human then answers, so the row stays `waiting` until the next
  `Stop`/`UserPromptSubmit`.

### 1.4 Store functions (`crates/keitora-tasks`)

| Function | Effect | Callers (prod) | Ref |
|---|---|---|---|
| `session_upsert` | upsert state/task/pid/heartbeat; any non-`waiting` state withdraws pending prompts | CLI `run_session`, gate `set_session_state` | `local_store.rs:104-129` |
| `session_set_offline` | force offline, clear task/pid, withdraw | **none** (doc says "used by the app when its PTY exits"; no caller) | `local_store.rs:146-156` |
| `session_resume_after_decision` | `busy` unless another pending prompt | gate | `drive_store.rs:929-957` |
| `session_reply_record` | `turn += 1`, store message | CLI `run_session` | `drive_store.rs:961-973` |
| `drive_set_awaiting_turn` | `awaiting_turn = reply.turn + 1` | app `deliver_send` | `drive_store.rs:417-429`, `drive.rs:1289` |
| `drive_restore_awaiting_turn` | set/clear | app send failure, reset/swap/wake success | `drive_store.rs:434-440`, `drive.rs:1296,1322,1364` |
| `drive_ack_turn` | CAS clear | CLI `wait` | `drive_store.rs:444-450`, `keitora-cli/src/drive.rs:1544` |
| `drive_prompt_insert` / `_close` / `_decide` | prompt lifecycle | gate; CLI `answer` | `drive_store.rs:457`, `:651`, `:555` |
| `drive_prompts_withdraw` | pending → withdrawn | `session_upsert`, `session_set_offline` | `drive_store.rs:671-680` |
| `drive_prompts_expire_stale` | pending past `timeout_s + slack` → expired | `drive_prompt_insert` | `drive_store.rs:683-695` |

### 1.5 What the app itself writes

- **Nothing to `worktree_sessions` or `session_replies`.** The app never writes session state;
  all of it is hook/CLI-written. `apply_reconcile` changes state in memory only.
- `drives.awaiting_turn` (send, reset/swap/wake: `drive.rs:1282-1331`, `:1362-1367`).
- `drive_requests` status/reason/result (`drive.rs:1020-1035`, housekeeping/expiry
  `:992-1018`).
- `resume_sessions` in `resume.db` (herdr observation, `resume.rs:208-260`) — separate DB.

---

## 2. Consumers

Legend for "source": **raw** = `worktree_sessions` as stored; **reconciled** =
`apply_reconcile(raw, live_set, hints)`; **live set** = keys Keitora has a mapped herdr pane for
(not "agent running"); **hints** = herdr pane `agent_status` mapped to busy/waiting.

### 2.0 The reconcile step itself

- `apply_reconcile` (`src-tauri/src/backend/mod.rs:801-838`):
  - a non-offline row whose key is not in the live set → `offline`;
  - `HINT_WAITING` turns `idle`/`busy` into `waiting`; `HINT_BUSY` turns `idle` into `busy`;
  - it never lifts `offline`, never turns `busy`/`waiting` back to `idle`, and synthesizes no
    row.
- `reconciled_sessions_in` (`src-tauri/src/tasks.rs:111-138`) drops rows whose name isn't a
  configured worktree, so a row written under a git-basename that differs from the configured
  name never surfaces anywhere in the app.
- Live set: `HerdrBackend.live` is rebuilt only by `list_sessions` (`backend/herdr/mod.rs:2946-2963`)
  and edited on spawn (`:1076`, `:2556`), close (`:854`, `:2215`), attach failure (`:2668`) and
  server death (`:415`). Membership = "a keymap entry whose pane is in `pane list`". A pane whose
  agent exited to a bare shell (`PaneIdentity.agent == None`, `cli.rs:27-29`) stays live.
- Hints: `state_hint_for_pane` passes `launch_pending=false` (`backend/herdr/mod.rs:2473-2477`),
  so `launch_pending` never becomes a hint on this path. `agent_hints` asks herdr through a
  ≤5 s pane cache (≤30 s when "low", `status_loop.rs:46-48`, `:174-197`); `cached_agent_hints`
  reads the last list however old (`mod.rs:2966-2974`).

### 2.1 Consumer inventory

| # | Consumer | Ref | Source | Fields | Freshness | Wrong value → |
|---|---|---|---|---|---|---|
| 1 | `sessions_list` command (active project) | `tasks.rs:155-170` | reconciled, **cached** hints, live-set snapshot | state, task, heartbeat, drive cols | 3 s poll + `sessions://changed` | feeds 2-8 below |
| 2 | `projects_overview` (rail, all projects) | `host.rs:200-245` | reconciled (read-only DB), cached hints | same | 3 s poll (`projectsOverview.ts:60`) + events | rail colours |
| 3 | Rail block colour `blockStateCls` | `src/lib/laneState.ts:31-47` | (2) + attention + activity build | `state`, `brokered`, `driven_by` | 3 s | **missing row → `idle` (green) if worktree available** (`:46`); stale state → wrong colour |
| 4 | Rail driven-sibling chip `drivenVisual` | `src/lib/railGroups.ts:144-153` | (2) + activity `working` | state, brokered | 3 s | **missing row → `offline`** (contradicts 3) |
| 5 | Group summary (dots, `waiting`/`busy` counts, `active`, `lastActivity`) | `railGroups.ts:183-225` | (2) + activity | blockState, `last_heartbeat`, `working` | 3 s | "last activity" = last hook write, not last agent activity |
| 6 | Lane tooltip / drive tag / deciding | `components/rail/Lane.tsx:180-215`, `lib/drive.ts:39-71,137` | (2) | state, brokered, escalated_reason | 3 s | wrong badge |
| 7 | Attention: banner, dock badge, "finished" latch | `src/lib/attention.ts:35-73,150-160`; `useSessions` `sessions.ts:133-140` | (1) uncached `liveSessions` | state transitions, driven/brokered | 3 s | finished latch needs an observed busy→idle edge: a turn shorter than the poll, or a stuck `busy`/`waiting`, loses or fakes the banner |
| 8 | Board lanes, card "front reserved & idle", dispatch menu ordering, lane header text | `board/useBoardLanes.ts:71`, `board/cardHelpers.ts:41`, `DispatchMenu.tsx:34-78`, `board/LaneHeader.tsx:26` | (1) | state (missing → `offline`) | 3 s | wrong dispatch vs queue affordance |
| 9 | Background toasts `bg_notifier` | `src-tauri/src/bg_notifier.rs:230-355` (latch `:66`, `needs_input` `:82`, `step_session` `:138`) | **raw** rows, no reconcile, no hints | state, driven_by, brokered, escalated_reason, task | only on a DB write that changes `session_revision` | toasts from stale rows (e.g. `waiting` after the human answered), none for herdr-only transitions; latch needs busy→idle hook pair |
| 10 | `sessions://changed` emitter | `host.rs:2502-2508` | `session_revision` (raw state + task + drive cols) | — | DB write | reconciled changes (live set, hints) never emit; UI relies on 3 s polls |
| 11 | Board dispatch `apply` | `src-tauri/src/dispatch.rs:317-325`, `decide` `:65` | reconciled, **fresh** hints | state; missing → `offline` | per call | non-idle → queued; `has_live_pty = state != offline`, so a bare-shell pane with stored `idle` gets the ritual typed into the shell |
| 12 | Drive send gate `send_state` | `drive.rs:925-975`, `state_refusal` `:767-780` | reconciled, fresh hints, missing → `offline`, plus `send_settling` | state, `session_replies.turn` | per pass (DB events + settle recheck only) | stale `offline` → refused (report A); stale `busy` → queued send never delivers; stale `idle` + bare shell → prompt typed into shell |
| 13 | Drive queued/woken send waits for idle | `drive.rs:1106-1114` | same as 12 | state | **only when a pass runs** | idle from hints/settle end never triggers a pass (plan C1) |
| 14 | Wake `awake()` / `SessionSets::awake` | `drive.rs:905-911`, `:983-985`, `run_restart` `:1399-1412`, `wake_gate` `:1215-1254` | **live set** minus terminated | pane presence | snapshot | `already_awake: true` while send says `offline`; bare shell counts as awake → wake is a no-op on a dead agent |
| 15 | `send_settling` | `drive.rs:240-258`, `mark_sent` `:223`, timeout `:66` (30 s) | in-memory + reconciled state + reply turn | state, turn | per pass | holds only while state is `idle`; hooks that never fire → released after 30 s on a stale `idle` |
| 16 | Auto-wake recheck | `drive.rs:890-897`, `run_auto_wake` `:1257-1279` | live set | pane presence | `send_settle + 0.5 s` scheduled pass | "woken but its agent did not come up" judged on pane presence only |
| 17 | Window wake step | `src/lib/driveEvents.ts:94-109`, pane `Status` `lib/paneStatus.ts:11` | frontend pane status (PTY attach state) | running/suspended/ended/terminated | live | a third liveness notion; `running` = PTY, not agent |
| 18 | Drive `relaunch` (reset/wake/`--to`) | `drive.rs:1422-1514` | — | writes `awaiting_turn=NULL` on success | — | old agent's `SessionEnd` may land after the new `SessionStart` (no session identity) |
| 19 | CLI `drives` | `keitora-cli/src/drive.rs:550-600` | **raw** row; missing → `-` | state, turn, awaiting_turn | on demand | prints `offline`/stale state that the app no longer believes |
| 20 | CLI `wait` (`gather`, `select_event`) | `keitora-cli/src/drive.rs:1403-1455`, `:1321-1363`, ack `:1544` | **raw** row, `session_replies`, `drive_prompts`, `drive_requests`, `session_idle_secs` | awaiting_turn vs turn, state, last_heartbeat | `data_version` poll 0.5 s, re-eval 5 s | TurnFinished only if a Stop hook ran; Stalled only while raw `busy`; idle-with-no-reply or crashed agent → waits until `--timeout` (default 0 = forever) |
| 21 | CLI `read --last` | `keitora-cli/src/drive.rs:785-821` | `session_replies` | last_message, turn | on demand | empty/missing when Stop hook didn't run or provider has no reply (agy) |
| 22 | CLI `read` (pane) | `drive.rs:1335-1340` (app), CLI `:785-856` | herdr `read_history` | scrollback | on demand | independent of hooks (works today) |
| 23 | CLI `prompts` / `answer` | `keitora-cli/src/drive.rs:1120-1141`, `:1243-1262` | `drive_prompts` | status, decision | on demand | only exists if the gate ran |
| 24 | CLI `drive` bind warning | `keitora-cli/src/drive.rs:443-457`, `:459-513` | hook files on disk | gate marker present | bind time | a Codex hook file present but untrusted reads as "installed" |
| 25 | CLI `task watch` | `keitora-cli/src/main.rs:1229-1236` | **raw** rows (filtered by configured names) | state | interval | stale state in the watch stream |
| 26 | Gate driver liveness | `gate.rs:676-679`, `:574-576` | **raw** row of the **driver** | state | per prompt / poll | driver row `offline`/missing (late SessionEnd, agy driver, name mismatch) → every prompt goes to the human |
| 27 | Activity working dot | `src-tauri/src/activity/mod.rs:299-311`, `:358`, `:392` | **fresh** herdr hints via `list_sessions` (also rebuilds live set) | busy/waiting hint | 2.5 s poll (`sessions.ts:260`) | independent of rows; disagrees with lane colour when hooks are stale |
| 28 | Recovery "mid-turn" observation | `backend/herdr/status_loop.rs:235-251` | herdr `agent_status` | busy | status loop | resumes with a nudge only for agents herdr saw working |
| 29 | Resume / skip-resume | `resume.rs:139-141`, `:181-183`, `:208-260` | herdr `agent` presence + `agent_session` | agent_present, session_id | status loop | herdr-sourced; already the identity used for resume |
| 30 | Transfer release | `transfer.rs:1014-1045` | `list_sessions` gen + `is_live` | pane presence | poll | waits for pane exit, not hooks |
| 31 | Transfer handoff injection | `src/App.tsx:547-575` | `liveSessions` state `idle` | state | 400 ms ticks, 800 ms initial delay | explicitly guards against a stale `idle` from the replaced session |
| 32 | Agent-update live sessions | `agent_updates/mod.rs:628-640` | `list_sessions` keys + configured cli | pane presence | on demand | counts panes whose agent already exited |
| 33 | Quit prompt / daemon session counts | `host.rs:2657-2664`, `host.rs:2272-2278`, `App.tsx:489-493` | live set / `list_sessions` | count | on demand | shell-only panes count as sessions |
| 34 | Perf overlay | `src-tauri/src/perf/roots.rs`, `perf/mod.rs` | herdr pane roots | process tree | sampler | does not read session state |
| 35 | Update line (`agent_updates` update pane) | `agent_updates/mod.rs:600-625` | herdr shell family of reserved pane | — | — | does not read session state |

---

## 3. Classification of needs

Categories: **(a)** liveness (an agent process is running in the pane), **(b)** activity
(idle vs busy), **(c)** blocked-on-human (waiting), **(d)** turn completion + reply text,
**(e)** permission brokering, **(f)** session identity.

| Consumer | a | b | c | d | e | f |
|---|---|---|---|---|---|---|
| Rail colour / summary / tooltip (3-6) | x | x | x | | | |
| Attention + bg toasts (7, 9) | x | x | x | x (finished) | x (brokered vs escalated) | |
| Board / dispatch (8, 11) | x | x | x | | | |
| Drive send gate + queue + settle (12, 13, 15) | x | x | x | x (turn counter) | | |
| Wake / auto-wake / window wake (14, 16, 17) | x | | | | | |
| Relaunch (18) | x | | | | | x (stale writer) |
| `drives` (19) | x | x | x | x | | |
| `wait` (20) | x | x | | x | x | |
| `read --last` (21) | | | | x | | |
| `prompts` / `answer` / gate (23, 26) | x (driver) | | x | | x | |
| Activity dot (27) | | x | x | | | |
| Recovery / resume (28, 29) | x | x | | | | x |
| Transfer (30, 31), updates, quit (32, 33) | x | x (31) | | | | |

### Who can provide what

| Need | Hooks | herdr / OS | Notes |
|---|---|---|---|
| (a) liveness | `SessionStart`/`SessionEnd` (Claude, Codex); agy has no end event | **pane `agent` field and `agent list`** (present / `launch_pending` / `done` / absent), pane presence, OS process tree (perf roots) | herdr can answer this alone. Today the live set conflates "pane exists" with "agent running". |
| (b) activity | `UserPromptSubmit`/`PreInvocation` → busy, `Stop` → idle | `agent_status` `working`/`idle`, `launch_pending`, `state_change_seq` | herdr can answer this; per-provider accuracy of herdr's status is unverified (herdr's own detection may itself rely on its integration hooks). |
| (c) waiting | Claude `Notification permission_prompt`, gate writes | `agent_status` `blocked` | herdr can answer "blocked". Hooks are only needed to tell **brokered** (driver deciding) from **escalated** (human), which is gate-local knowledge in `drive_prompts`. |
| (d) turn completion | `Stop`/`StopFailure`, agy `Stop fullyIdle` | `working→idle` edge (with `state_change_seq`); Codex user-level `notify` | herdr can signal completion; the hook-free signal needs a stable edge detector, not a 3 s poll. |
| (d) reply text | **payload `last_assistant_message`, `transcript_path`** | herdr `agent_session` gives session id / transcript path → Keitora can read the transcript itself; `read_history` gives scrollback | Hooks are the cheapest source; transcript reading via herdr's session id is a viable fallback for Claude/Codex. agy: neither path yields text today. |
| (e) permission brokering | **only hooks** (`PermissionRequest`, `PreToolUse AskUserQuestion` — blocking, return a decision) | none (herdr can observe `blocked`, not answer) | Uniquely hooks. Keystroke-driving a native dialog would be the only alternative. |
| (f) session identity | payload `session_id` (Claude, Codex), not stored today | **`agent_session` per pane**, already persisted in `resume_sessions` | herdr already provides this and Keitora already stores it; the stale-writer guard can compare a hook's `session_id` against it. |

---

## 4. Inconsistencies and race windows

### 4.1 Same question, different sources

| Question | Answers in the code |
|---|---|
| "Is the agent running?" | wake `awake()`: live set minus terminated (`drive.rs:905`); send/dispatch: reconciled row ≠ offline (`drive.rs:925`, `dispatch.rs:325`); `drives`, `wait`, gate, task watch, bg toasts: raw row; window wake: pane `Status` (`driveEvents.ts:94`); resume: herdr `agent_present` (`resume.rs:139`); agent updates / quit: `list_sessions` keys. None of them reads herdr's `agent` field except resume. |
| "No session row" | rail colour → `idle` (`laneState.ts:46`); driven chip, board, dispatch menu, send, dispatch → `offline`; `drives` → `-`; gate driver check → offline; `wait` → `None` (never stalls). |
| "Is it busy?" | rail / `sessions_list`: row upgraded by **cached** hints; dispatch / drive: row upgraded by **fresh** hints; activity dot: hints only; `wait` stall, bg toasts, `drives`: raw row only; recovery: hints only. |
| "Is it waiting, and on whom?" | row `waiting` (hook or gate) + `drive_prompts` pending (brokered) + escalated reason; herdr `blocked` hint (cannot distinguish brokered/escalated). Undriven Claude `AskUserQuestion` writes nothing (gate returns early, `gate.rs:736`; `Notification` matcher is only `permission_prompt`), so only the hint shows it. |
| "Did the turn finish?" | `wait`: `session_replies.turn >= awaiting_turn` (hook only); `send_settling`: same turn counter or state ≠ idle; attention / bg: busy→idle edge in polled state; recovery: herdr status. |
| "Which worktree is this?" | hooks: git toplevel basename (`config.rs:3347`); app: configured names (`tasks.rs:124-134`); herdr: `SessionKey{project, worktree}` + pane cwd. Rows are project-less, so two projects with the same worktree name share a row. |

### 4.2 Structural gaps

1. **`apply_reconcile` is one-directional.** It only downgrades to `offline` or upgrades
   `idle`. A stale `offline` (late `SessionEnd`), stale `busy` (missed `Stop`, Codex hooks
   untrusted) or stale `waiting` (human answered an escalated/undriven prompt; nothing writes
   `busy` until `Stop`) is permanent until the next hook write.
2. **Live set ≠ agent present.** A crashed agent leaves a shell pane in the live set and its
   last row (`idle`/`busy`) in place: send and dispatch type into the shell, wake reports
   `already_awake`. herdr's `agent == None` already says the agent is gone
   (`resume.rs:211` stores it as `agent_present`), but reconcile doesn't use it.
3. **No app-side offline write.** `session_set_offline` has no caller; agy never writes
   `offline`; so `offline` comes only from Claude/Codex `SessionEnd` or the reconcile
   downgrade.
4. **Hint plumbing drops `launch_pending`** (`backend/herdr/mod.rs:2476`), so a launching agent
   is neither present nor busy on this path.
5. **Every Stop is a turn.** `turn` bumps on any Stop (human-typed turns, `--failed`, an
   unparseable payload, a duplicate Stop); `awaiting_turn` is just `turn + 1`, so a
   human-initiated turn can satisfy a driver's `wait` with the wrong reply, and a duplicate
   Stop skips one. Codex has no `StopFailure`; agy records a turn only when `fullyIdle`.
6. **`wait` hangs** when the session is `idle` with an outstanding turn and no reply, or when
   the row is missing/offline (Stalled requires raw `busy`, `keitora-cli/src/drive.rs:1347`).
   The stall clock is `last_heartbeat`, which only hooks move, so a long healthy turn also
   reads as "stalled" after `--stall-after` (default 120 s).
7. **Driver liveness for brokering is the driver's raw row** (`gate.rs:676`). A driver whose
   row is `offline`/missing (late `SessionEnd`, agy driver, basename mismatch) silently
   disables brokering for everything it drives.
8. **Any non-`waiting` write withdraws pending prompts** (`local_store.rs:124-126`), including a
   late `SessionStart`/`SessionEnd` from a previous agent or a human's `UserPromptSubmit`.
9. **Background vs foreground attention diverge.** `bg_notifier` reads raw rows and runs only
   on `session_revision` changes; the window reads reconciled state with hints every 3 s.
   Same `needs_input` logic, different inputs.
10. **`sessions://changed` reflects only DB changes.** Live-set and hint changes reach the UI
    by 3 s polling, and reach the drive runner not at all (no pass is scheduled on a hint flip;
    plan C1).
11. **Hook installation is not tied to the provider actually launched** (table in 1.1): Codex
    hooks are missing after `reset --to codex`, UI switch or transfer until restart/activation;
    the Codex trust bypass can be dropped by the strict launch fallback (plan B1b).

### 4.3 Race windows

| Window | Where | Effect |
|---|---|---|
| Late `SessionEnd` after restart/reset/switch | `close_session` → `pane_close` doesn't wait for hooks; `session_upsert` has no identity (`local_store.rs:104`) | new agent's `idle` overwritten by `offline`; send refused; deadlock (no new hook write arrives because send is refused) |
| Old agent's `Stop` after a reset | `clear_awaiting_turn` (`drive.rs:1362`) runs on reset success; a Stop from the killed agent can still bump `turn` | next send's `awaiting_turn` is computed from a counter the old session moved |
| Keystroke → `UserPromptSubmit` lag | `send_settling` (`drive.rs:240-258`, 30 s) | held busy while state is `idle`; released early if state is `offline`/hint flips, late (30 s) if hooks never fire — then a second queued send may type into a working agent unless herdr says `working` |
| Stop writes reply, then state, in two statements | `main.rs:474-480` | a reader can see `turn` bumped with state still `busy` (harmless for `wait`, visible in `drives`) |
| Snapshot reuse in a pass | `drive.rs:1062-1065`, `:915-922` | sends later in one pass judge state from the pass-start snapshot (dropped only after a delivery) |
| Live-set staleness | live set rebuilt only by `list_sessions` (activity 2.5 s, dispatch, drive) | `sessions_list` and `awake()` can disagree with a fresh `pane list` for seconds |
| Hint staleness | pane cache 5 s / 30 s low (`status_loop.rs:46-48`); `cached_sessions` unbounded | rail/attention lag; a fast turn (< poll) never shows `busy`, so no "finished" banner |
| DB-watch debounce | 300 ms (`host.rs:2496`, `bg_notifier.rs:267`) and macOS FSEvents (plan C3) | requests and toasts lag or miss |
| Transfer restart vs stale `idle` | `App.tsx:568-574` (800 ms initial delay) | injection could fire into the replaced session; mitigated only by a delay |
| agy `SessionStart` before first prompt | agy hook timing (plan D) | row stays absent → `offline` everywhere but the rail, which shows `idle` |

---

## 5. Implications for the redesign (notes, not decisions)

- (a) liveness and (b)/(c) activity have a complete herdr source already parsed
  (`PaneIdentity.agent`, `agent_status`, `AgentInfo.launch_pending`, `state_change_seq`). A
  single app-side "session view" built from herdr, with hooks as enrichment, would remove most
  rows in 4.1. It must also be what the CLI reads (`drives`, `wait`, gate driver check), which
  today can only see `tasks.db` — so the app would need to persist the derived state (or the
  CLI query herdr itself).
- (d) needs a turn identity that isn't "number of Stop hooks": e.g. a send-scoped token, or
  herdr's `state_change_seq` edge after the send, with the hook reply (or a transcript read
  via herdr's session id) attached when available.
- (e) stays hook-only. The redesign should keep the gate and `drive_prompts`, and make the
  gate's driver check use the same derived liveness as everything else.
- (f) is already herdr-sourced in `resume_sessions`; hook `session_id` can be checked against it
  to reject stale writers.
- Open: how reliable herdr's own `agent_status` is per provider (Claude/Codex/agy) — if herdr
  derives it from its own integration hooks, it may share the same failure modes.
