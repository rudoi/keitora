# Research: herdr's agent model as a state and completion source

Scope: can Keitora derive session state (idle/busy/waiting/offline) and "turn finished" from
herdr instead of, or alongside, its own provider hooks? Read-only research against herdr
0.9.5 (protocol 22). Sources:

- the installed binary's `--help` text, `herdr --skill`, and `herdr api schema --json` (the
  bundled schema; none of these contact the daemon);
- the hook files herdr installed under the user's home;
- herdr's v0.9.5 docs (`docs/next/website/src/content/docs/{agents,integrations,socket-api,add-herdr-support}.mdx`
  at `raw.githubusercontent.com/herdrdev/herdr/v0.9.5/...`);
- the Keitora source.

The running herdr was never contacted.

## TL;DR

1. **herdr's lifecycle state for Claude, Codex and agy comes from screen detection, not
   hooks.** In 0.9.5 all three herdr integrations are *session-only*: one `SessionStart` (or,
   for agy, `PreInvocation`) hook that calls `herdr pane report-agent-session`. herdr's
   integrations never report idle/working/blocked. So herdr's status doesn't depend on Codex
   project trust, a Stop hook firing, or Keitora's hook install timing.
2. **herdr pushes status changes.** The socket API's `events.subscribe` streams
   `pane.agent_status_changed`, `pane.agent_detected` (with `released` and `final_status`),
   `pane.exited` and `pane.closed` over NDJSON on the named pipe or Unix socket. There is
   no CLI wrapper for subscribe. `agent wait` and `agent prompt --wait` are server-side,
   event-driven waits.
3. **There's a turn-ish counter.** `agent.list` returns `completion_seq` ("The current idle
   transition completed work, independently of who has viewed it") and `state_change_seq`,
   plus `interactive_ready`, `launch_pending`, `screen_detection_skipped` and
   `agent_session`. Keitora parses only `agent_status`, `launch_pending`, `name`, `cwd` and
   `pane_id`.
4. **Keitora uses herdr state only as a one-way hint.** `working` → busy and `blocked` →
   waiting can upgrade a hook-written row. herdr presence never lifts `offline`, and
   `idle`/`done` are ignored. herdr is polled through `pane list`: every 30 s in the status
   loop, and reused for up to 5 s by `list_sessions`.
5. **Caveats.** Screen detection falls back to `idle` for an unrecognized screen of a known
   agent. `blocked` needs a known approval UI. agy (and Grok) report `idle` while background
   tasks or subagents still run. herdr tracks no turn ids and no last message.

## 1. herdr's agent model

### CLI surface (`herdr agent --help`)

`list`, `get`, `read`, `send-keys`, `prompt`, `rename`, `focus`, `wait`, `attach`, `start`,
`explain`.

- `agent start <NAME> --kind <KIND> --pane <ID> [--timeout <MS>] [-- AGENT_ARG...]`
  - kinds include `claude`, `codex` and `agy`;
  - the pane "must be at its interactive shell prompt";
  - "Success means the expected agent was detected in the same terminal and is ready for
    input" (default timeout 30000, max 300000).
- `agent wait <TARGET> [--until <STATUS>]... [--timeout <MS>]`: "Without --until, matches
  idle, done, or blocked. Use --until unknown explicitly when needed. Without --timeout,
  waits indefinitely."
- `agent prompt <TARGET> <TEXT> [--wait] [--until <STATUS>]... [--timeout <MS>]`. Verbatim:

  > If the agent is already blocked, submission is rejected with agent_blocked before any
  > input is sent. When an accepted submission starts from another non-working state, --wait
  > requires an observed working or blocked state within 5000ms; otherwise it returns
  > agent_prompt_stalled. A caller timeout that expires first returns timeout. It then matches
  > idle, done, or blocked by default, or any exact --until state. It does not track turns: if
  > the agent is already working, that active turn's completion may match.

- `agent explain [TARGET] [--file <PATH>] [--agent <LABEL>] [--json] [-v]`: explains
  detection, and the response carries `screen_detection_skip_reason`.
- `agent read <TARGET> --source visible|recent|recent-unwrapped|detection`: `detection` is
  "the plain-text bottom-buffer snapshot used for agent detection" (skill text).

### Status values

The schema defines `AgentStatus = idle | working | blocked | done | unknown`.

- socket-api.mdx: "`done` means idle and not yet seen". Explicit focus commands mark the
  target as seen; reads don't.
- The skill says: "`idle` and `done` normally indicate the agent is ready for input; neither
  guarantees that an unfamiliar approval dialog was recognized … `blocked` means Herdr
  recognized an approval or question UI. `unknown` means an agent is present but Herdr
  cannot classify it confidently; it does not prove completion."
- The lifecycle-report enum (`PaneAgentState`) has no `done`. `done` is derived by herdr.

### `agent.list` fields (`AgentInfo` in the bundled schema)

`pane_id`, `terminal_id`, `workspace_id`, `tab_id`, `name`, `agent`, `display_agent`,
`agent_status`, `agent_session {source, agent, kind: id|path, value}`, `launch_pending`,
`interactive_ready`, `screen_detection_skipped`, `completion_seq` (nullable u64:
"The current idle transition completed work, independently of who has viewed it"),
`state_change_seq` (u64, default 0), `revision`, `cwd`, `foreground_cwd`, `focused`,
`title`, `terminal_title(_stripped)`, `state_labels`, `tokens`.

`PaneInfo` (`pane list`) has `agent`, `agent_status`, `agent_session`, `revision`, `cwd` and
`restore_error`. It does **not** have `launch_pending`, `interactive_ready` or the seq
counters. Those come from `agent list`/`agent get` only.

### Push events (no polling needed)

From the socket API:

- Transport: "newline-delimited JSON over a local socket". On Windows it's a named pipe;
  `HERDR_SOCKET_PATH` and `HERDR_SESSION` select the socket.
- `events.subscribe {subscriptions:[...]}`: "Event subscriptions keep the connection open
  after the initial response. The first response acknowledges the subscription. Later lines
  are pushed events."
- No replay: "Lifecycle subscriptions … do not replay events retained before that point."
  A slow subscriber gets `events_lost` and the connection closes.
- Recovery: re-subscribe, wait for `subscription_started`, then call `session.snapshot` on
  another connection and reconcile.
- If any entry references a missing pane, the whole request is rejected.
- Subscribable types (`Subscription` in the schema):
  - `pane.agent_status_changed {pane_id, agent_status?}`, where `agent_status` is an
    optional filter. It's per-pane; `pane_id` is required.
  - `pane.agent_detected`, `pane.exited`, `pane.closed`, `pane.created`, `pane.updated`,
    `pane.moved`;
  - `pane.output_matched {pane_id, source, match}`, `pane.scroll_changed`;
  - `workspace.*`, `tab.*`, `worktree.*`, `layout.updated`.
- Event payloads:
  - `pane_agent_status_changed {pane_id, workspace_id, agent, display_agent, agent_status,
    title, state_labels}`;
  - `pane_agent_detected {pane_id, workspace_id, agent, released: bool, final_status}`. This
    is the exit/release signal.
  - `pane_exited {pane_id, workspace_id}`.
- `events.wait {match_event, timeout_ms}` is a one-shot wait on one matching event (the
  `EventMatch` schema includes `pane_agent_status_changed` with a required `agent_status`).
- `agent.wait` "is server-owned and event-driven. It pins the resolved pane occupant so a
  replacement can't satisfy the wait."
- **There is no CLI command for subscribe.** `herdr api` has only `snapshot` and `schema`.
  Using the event stream means opening the socket from Rust. Keitora already speaks NDJSON
  to `herdr terminal session control` children (`backend/herdr/bridge.rs:1-24`), but not
  to the API socket.

### Lifecycle authority (what an integration *could* report)

`pane report-agent --source <ID> --agent <LABEL> --state idle|working|blocked|unknown
[--seq N] [--message] [--agent-session-id] [-- RESUME_ARGV]`, plus `report-agent-session`,
`release-agent` and `report-metadata`.

- Docs: "When an integration also reports state, Herdr uses those reports instead of
  reading the screen". `agent explain` shows "whether screen detection was skipped by a
  full lifecycle authority".
- `seq` must strictly increase per source. Lower or equal values are "accepted by the API
  but ignored".
- Source ids must not start with `herdr:`.
- On process exit without a release, herdr releases "once the pane is back at its idle shell
  prompt, which usually takes a second or two".

## 2. How herdr derives `agent_status` per provider

### herdr's integrations are session-only (0.9.5)

What `herdr integration install` wrote on this machine (read-only inspection):

| Provider | Registration | Event(s) | Script | Action |
|---|---|---|---|---|
| claude (v11) | `~/.claude/settings.json` hooks | `SessionStart`, matcher `^(startup\|resume\|clear\|compact\|fork)$` | `~/.claude/hooks/herdr-agent-state.ps1` | `pane report-agent-session --source herdr:claude --agent-session-id <session_id> [--agent-session-path <transcript>] [--session-start-source]` |
| codex (v10) | `~/.codex/hooks.json` (user level) + `[features] hooks = true` in `config.toml` | `SessionStart` | `~/.codex/herdr-agent-state.ps1` | same, `--source herdr:codex`; skips if `CODEX_THREAD_ID` ≠ the payload `session_id` or no `transcript_path` |
| agy (v4) | `~/.gemini/config/hooks.json`, named block `"herdr"` | `PreInvocation` | `~/.gemini/config/hooks/herdr-agent-state.ps1` | `--source herdr:antigravity_cli --agent agy --agent-session-id <conversationId>` |

The scripts never call `pane report-agent`. The agy script says so outright: "Session-only:
this hook reports the Antigravity conversation so Herdr can resume the pane. Lifecycle state
comes from Herdr's screen detection." The integrations doc agrees for Claude ("Claude Code
state comes from Herdr's screen manifest detection") and Codex (same wording). For
Antigravity: "Working, idle, and blocked states come only from what is drawn on screen."

All scripts no-op outside herdr (`HERDR_ENV != 1` or no `HERDR_PANE_ID`) and swallow errors.
They're on Windows PowerShell here; the docs' Unix equivalent is `herdr-agent-state.sh`.

### Screen detection

agents.mdx:

- herdr "reads the live bottom of the pane, not the part you scrolled to".
- "Rules in a detection manifest decide whether that screen means `idle`, `working`, or
  `blocked`."
- "If no rule matches a known agent, Herdr falls back to `idle`", reported as
  `default_known_agent_idle_fallback` in explain output.
- Blocked: herdr "only marks `blocked` when the live bottom-buffer snapshot matches known
  visible approval, question, or permission UI". "Unusual new agent prompts may initially
  show as `idle` instead of `blocked`."

Manifests are versioned per agent and can be updated remotely: "apply valid per-agent rule
updates automatically without requiring a Herdr restart". "Local overrides always win."
The schema exposes this as `server.agent_manifests` and `server.reload_agent_manifests`.
This is good, because herdr can fix a Codex UI change without a release. It also means
status behaviour can change under Keitora without a version bump.

### Global vs project scope and Codex trust

- All herdr registrations are **user-level** (`~/.claude`, `~/.codex`, `~/.gemini/config`;
  `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `ANTIGRAVITY_CLI_CONFIG_DIR` override them). None
  are per project, so Codex *project* trust doesn't gate them.
- Codex hook trust is a separate gate, and status doesn't depend on it.
  - `~/.codex/config.toml` has `[hooks.state."…"] trusted_hash` entries for plugin hooks
    only. There's none for herdr's user-level hook. Inference: on a launch without
    `--dangerously-bypass-hook-trust`, Codex may skip herdr's session hook too.
  - Keitora always passes that flag except on the strict chain's last fallback
    (`agent.rs:36-41`, `strict.rs`).
  - If the hook is skipped, only resume ids suffer. `agent_status` comes from the screen and
    is unaffected.
- The user's `config.toml` already sets a top-level
  `notify = [ "<Codex desktop>…codex-computer-use.exe", "turn-ended" ]`. Codex has a single
  `notify` slot, so the plan's Open question 2 (use Codex `notify` for completion) would
  overwrite or have to chain a user-owned setting.

### How Keitora installs them

- `Supervisor::ensure_ready` (`src-tauri/src/backend/herdr/supervisor.rs:249-296`) runs
  once per process:
  - on adopt (`backend/herdr/mod.rs:1432-1434`) and on the primary launch path
    (`mod.rs:2590-2594`);
  - runs `integration status`, then `integration install <target>` for anything not
    installed or outdated;
  - is opted out by `KEITORA_HERDR_NO_INTEGRATION` and always skipped under `cfg(test)`.
- Targets are `integration_targets(agy_found)` (`supervisor.rs:51-60`). `antigravity-cli`
  is included **only when `agy` was found** (a configured path or discovery) at that moment.
  If `agy` is installed after Keitora started, the integration isn't installed until
  `refresh_integration` (`supervisor.rs:301`) or a restart.
  - Effect: **no resume id**.
  - Status is unaffected, because agy state is screen-only regardless.
- A failed install is a `warn` that names only the resume consequence (`supervisor.rs:281-285`).
  That is accurate given the session-only hooks.

## 3. How Keitora consumes herdr state today

- **Polling cadence** (`backend/herdr/status_loop.rs`):
  - wake `WAKE = 1s`;
  - `pane list` observation every `HEALTHY_CADENCE.observe = 30s` (`LOW_CADENCE` 60 s,
    `status_loop.rs:69-82`);
  - `list_sessions` reuses a pane list up to `SESSIONS_MAX_AGE = 5s` old (30 s under low
    headroom, `:44-47`). It's polled by the activity engine (`activity/mod.rs:358`) and
    `agent_hints`.
  - `cached_sessions` never calls herdr (`mod.rs:2966-2974`).
  - There's no subscription anywhere. Grepping `src-tauri` for `events.subscribe`,
    `session.snapshot`, `completion_seq` or `interactive_ready` finds only a test fixture
    string in `cli.rs:544`.
- **Mapping:** `agent_status_to_state_hint(status, launch_pending)`
  (`backend/herdr/agent.rs:164-180`):
  - `launch_pending` or `working` → `HINT_BUSY`;
  - `blocked` → `HINT_WAITING`;
  - `idle`, `done`, `unknown` and anything else → `None`.
  - The pane path calls it with `launch_pending = false` because `PaneInfo` lacks the field
    (`mod.rs:2473-2477`, `status_loop.rs:243-245`).
- **Reconcile:** `apply_reconcile` (`src-tauri/src/backend/mod.rs:801-838`):
  - downgrades a session that isn't in the live set to `offline`;
  - a hint can only move `idle` → `busy` or `idle`/`busy` → `waiting`;
  - it **never** lifts `offline` and never applies `idle`.
  - So herdr saying "agent present, idle" carries no information today (root cause A1 in the
    proposal).
- **`agent list`** (`real.rs:409-413`, parsed into `cli.rs:67-80` `AgentInfo`) is used only
  for name/cwd matching: `close_name_holders` (`mod.rs:884`), reconcile's `live_agents`
  (`reconcile.rs:224-229`), and keymap ranking that prefers non-`done` (`keymap.rs:871`).
  `launch_pending` is parsed, but no state decision consumes it.
- **Resume store** (`backend/herdr/resume.rs:1-11, 208-235`): stores the newest
  `agent_session` id per `(project, worktree)` when `source == "herdr:<agent>"`,
  `kind == id` and the id is safe. Absence only refreshes the "still running" bit
  (`present = pane.agent.is_some()`).
- **Prompting:** `agent_prompt` is fire-and-forget, with no `--wait` (`real.rs:399-405`,
  `mod.rs:771`). `agent_blocked` and `agent_not_ready` map to "queue the line".

### herdr signals Keitora ignores that would help

- **Presence:** `agent` non-null, or `agent list` has the Keitora-named `{kind}-{hash}`
  entry. This would lift a stale `offline` (A1).
- **`idle`/`done` as positive states.** `done` = idle and unseen, so treat it as idle for
  dispatch.
- **`interactive_ready` and `launch_pending`** (agent list), for "launched, not ready yet".
- **`completion_seq` and `state_change_seq`.** A `completion_seq` that rises after a prompt
  is a completion signal that is independent of provider hooks (§5).
- **`pane_agent_detected{released, final_status}`, `pane_exited`, `pane_closed`.**
  Event-driven offline, with no 30 s lag.
- **`screen_detection_skipped` and `agent explain`.** These are diagnostics for the doctor,
  e.g. "status for @wt is a fallback idle".
- **`agent_session` changes.** A new session id on the same pane means the agent restarted
  or ran `/clear`. That makes a natural stale-writer guard key for the plan's A3.

## 4. Reliability per provider

| | Claude | Codex | agy |
|---|---|---|---|
| State source | screen manifest | screen manifest | screen manifest only |
| herdr docs status | "Supported by Herdr" | "Supported by Herdr" | Supported, with caveat: idle while background tasks/subagents run |
| Depends on hooks/trust | no (hook = resume id only) | no; resume id needs the user-level hook to run (hook trust; Keitora's bypass flag covers it) | no; integration installed only if `agy` found at startup (resume only) |
| Blocked/permission | only for recognized approval UIs; new prompts may read `idle` | same | same |
| Turn end | `working`→`idle` (`done` until seen) | same | same, but "idle" ≠ background jobs done |
| Exit | herdr releases after shell prompt returns (~1–2 s); `pane_agent_detected{released}` | same | same |
| Ready on launch | `agent start` returns after interactive ready; `agent_not_ready` if blocked at startup | same (Keitora disables the update screen via `-c check_for_update_on_startup=false`) | same |

Risks:

- **Fallback idle.** An unrecognized screen of a known agent reads `idle`. A new provider UI,
  an unusual modal, or a resize mid-render can briefly look idle. For "turn finished",
  require an observed `working` first, and ideally a rising `completion_seq`.
- **Remote manifest updates** can change classification without a herdr upgrade. Pin or
  record `server.agent_manifests` versions in diagnostics.
- **Latency.** The docs don't state the screen-detection cadence. Waits and events are
  event-driven server-side, and `agent prompt --wait` treats 5 s without `working`/`blocked`
  as stalled, which implies detection runs well under 5 s. Keitora's own latency (5–30 s
  polling) dominates today.
- **Bracketed paste / Codex on Windows:** "Herdr sends a paste boundary before Enter" (skill
  text). `agent prompt` is the right submission path. Keitora already uses it.
- **No durability.** Events aren't replayed. A subscriber must re-snapshot after reconnecting
  or after a herdr restart (the watchdog and recovery path already know when this happens).

Compared with Keitora's hooks, herdr's status avoids every failure mode in the proposal's A
and B sections:

- Codex project trust;
- a dropped `--dangerously-bypass-hook-trust`;
- missing hooks after `reset --to`;
- a late `SessionEnd` overwriting `idle`;
- agy hooks being silent on macOS.

What it doesn't give is the **reply text**.

## 5. Turn-level info and completion

- **No turn ids and no last message.** The help says "It does not track turns". The schema
  has no turn id, and no last-assistant-message field anywhere. The closest are
  `completion_seq` and `state_change_seq` (counters), plus `agent read` / `pane read` (the
  screen or scrollback text, not a structured reply).
- **"Turn finished" from transitions works, with guards.** Before sending, record
  `completion_seq` (and `state_change_seq`). After `agent prompt`, a turn is finished when:
  - `working` (or `blocked`) was observed after submit, then `idle`/`done`; or
  - `completion_seq` exceeds the recorded value.

  Edge cases:
  - Sending while already `working` can match the previous turn. Keitora should refuse or
    queue unless the agent is idle, which it effectively does.
  - A turn faster than detection might skip an observed `working`. herdr's prompt gate then
    returns `agent_prompt_stalled`, and `completion_seq` is the tiebreak.
- **`agent prompt --wait` as a primitive.** It's usable. It submits and waits in one
  server-side request with no race, and returns `agent_blocked`, `agent_prompt_stalled`,
  `timeout`, or the settled status. Downsides:
  - the call blocks a process or connection for the whole turn (minutes to hours);
  - "a timeout or stalled response does not prove the prompt was never delivered" (skill);
  - from the CLI it ties up one `herdr` child per in-flight prompt;
  - it pins the pane occupant, so an agent restart fails the wait instead of matching
    wrongly. That's desirable.

  Better fit for an app: keep the fire-and-forget `agent prompt`, then either one long-lived
  `events.subscribe` connection (`pane.agent_status_changed` for each mapped pane, plus
  `pane.agent_detected` and `pane.exited`), or short `agent wait --until idle --until done
  --until blocked --timeout N` calls owned by the drive request. The `keitora wait` CLI could
  shell out to `herdr agent wait <name>` directly, using the Keitora agent name, as a
  hook-free completion source.
- **The reply text still needs another source.** Options:
  - the provider transcript: herdr gives `agent_session.value` (session id) and, for Claude
    and agy, the `--agent-session-path` transcript path;
  - Keitora's Stop-hook capture;
  - `agent read --source recent-unwrapped` as a last resort.

  A suggested split: **herdr owns presence, state and turn completion; hooks or transcripts
  only enrich with the reply**. `wait` should resolve on herdr's completion even when no
  reply row exists. It would report "finished, reply unavailable — use `keitora read`"
  instead of hanging (B2).

## Suggested direction

1. Make herdr the single authority for presence and state.
   - Map `working` → busy, `blocked` → waiting, `idle`/`done` → idle, and agent present →
     not offline.
   - `unknown` → keep the last state.
   - Released, exited or closed → offline.
   - Hook rows become advisory, used for reply capture and current task.
2. Add one `events.subscribe` connection per server instance.
   - Subscribe to `pane.agent_status_changed` for each keymap pane, plus
     `pane.agent_detected` and `pane.exited`.
   - Re-subscribe and re-snapshot (`agent list`) on reconnect, on `events_lost`, and on a
     pane-set change (the subscription needs explicit pane ids).
   - Keep the 30 s `pane list` as the safety net.
3. Extend `AgentInfo` parsing with `interactive_ready`, `completion_seq`,
   `state_change_seq`, `screen_detection_skipped` and `agent_session`. Record
   `completion_seq` at send; complete the awaited turn on increase.
4. Doctor: `herdr integration status` (resume ids), `agent explain --json` per live agent
   (fallback idle, skip reason), and manifest versions.
5. Don't have Keitora call `pane report-agent` with lifecycle state. Becoming a "full
   lifecycle authority" makes herdr skip screen detection, which would bring back
   hook-dependence. Session-only `report-agent-session` is safe if ever needed.

## Not verified (needs a sandboxed herdr)

- The screen-detection cadence and real latency.
- Exact `completion_seq` semantics: does it increment on every working→idle, and is it null
  before the first turn?
- Whether `pane list`'s `agent_status` reports `done` or only `idle`.
- Whether Codex skips herdr's user-level `SessionStart` hook without the trust bypass.
- How `events.subscribe` behaves over the Windows named pipe.

Check these only against an isolated herdr session, never the user's running daemon.

## Corrections and additions (refine cycle 4)

Re-checked against the 0.9.5 binary's `--help`, `herdr --skill` and `herdr api schema --json`
(protocol 22, schema_version 1), plus the Keitora source. No server was contacted.

1. **No CLI command streams lifecycle events.** Confirmed. `herdr api` has only `snapshot` and
   `schema`. The only long-lived CLI streams are `terminal session control|observe <target>`,
   and they carry terminal frames (`terminal.frame`, `terminal.closed`, `terminal.bell`), not
   agent status. `agent wait`, `agent prompt --wait` and `pane wait-output` are one-shot,
   per-target blocking calls. Subscribing therefore means speaking the socket protocol from
   Rust.
2. **The Windows transport is not a plain named-pipe path.** In a herdr pane on Windows,
   `HERDR_SOCKET_PATH` points at `<config>/herdr/herdr.sock`, which is a small *regular file*
   holding `<server pid>:<nonce>`. The real transport (a named pipe, presumably named from that
   pair) is undocumented. A Rust subscriber on Windows would depend on that indirection. The
   §1 "Transport" line ("On Windows it's a named pipe") is right in effect but hides this.
3. **Subscription scope.** Only `pane.agent_status_changed`, `pane.output_matched` and
   `pane.scroll_changed` take a `pane_id`. `pane.agent_detected`, `pane.exited`,
   `pane.closed`, `pane.created` and `pane.updated` are **global** (no pane id). `pane_updated`
   carries a full `PaneInfo`, including `agent_status`; whether it fires on every status change
   is unverified.
4. **No event carries a seq counter.** `completion_seq` and `state_change_seq` exist only on
   `AgentInfo` (`agent list` / `agent get`). Even with push, completion bookkeeping needs an
   `agent list` read.
5. **Optional fields.** In `AgentInfo`, only `terminal_id`, `agent_status`, `workspace_id`,
   `tab_id`, `pane_id`, `focused` and `revision` are required. `launch_pending`,
   `interactive_ready`, `screen_detection_skipped`, `completion_seq` and `agent_session` may be
   absent. Parse them with defaults.
6. **`completion_seq` has two plausible readings.** The description says "the *current* idle
   transition completed work". That fits a counter of completions, but it also fits "the id
   (seq) of the idle transition we are in, when that idle followed work; null otherwise". Code
   must not assume monotonic `+1` steps. "Changed from the baseline value and non-null" is
   correct under both readings (see the proposal's turn predicate).
7. **Inherited `HERDR_*` env.** Every herdr pane exports `HERDR_SOCKET_PATH`, `HERDR_PANE_ID`,
   `HERDR_ENV`, `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`, `HERDR_BIN_PATH` and `HERDR_CONFIG_PATH`.
   Neither `HerdrEnv::apply` (`backend/herdr/env.rs`) nor `herdrDevEnv`
   (`scripts/dev-herdr.mjs`) removes `HERDR_SOCKET_PATH`/`HERDR_SESSION`. A process started
   from inside a herdr pane can therefore reach the *outer* herdr server whatever `XDG_*` says.
   That is a likely cause of the "isolated launcher adopts the real herdr on Windows" report.
   The launcher's `isolationVerdict` catches it (the socket lies outside the isolated home, or
   `running: true`) and refuses to launch.
8. **Rejected suggestion.** "`keitora wait` could shell out to `herdr agent wait`" (§5)
   conflicts with an existing invariant: the CLI never talks to herdr (`keitora-cli` has no
   herdr calls, and pane verbs go through the app). Keep it that way. The CLI reads the app's
   status store.
