# Research: agent signals that don't depend on provider hooks

Question: which signals give Keitora per-worktree **liveness**, **idle/busy**, **blocked on a
human** (permission or question pending), **turn completion + last reply text**, and **permission
brokering** for Claude Code, Codex CLI and Antigravity CLI (`agy`), without leaning on the
per-worktree project hooks that have failed in practice (Codex project trust and hook-hash review,
global agy hooks that need `KEITORA_BIN`, late `SessionEnd` races, failures hidden by `2>/dev/null`)?

Companion doc: `research-herdr.md` covers herdr's `agent_status`, `events.subscribe` and
`completion_seq` in depth. This doc covers everything else and only summarizes herdr where it's
needed for the ranking.

Sources: official docs (code.claude.com, learn.chatgpt.com (Codex), antigravity.google), the
Keitora code (`stop_capture.rs`, `transfer.rs`, `backend/herdr/{resume,agent,bridge,cli}.rs`,
`hooks.rs`), `plans/0077-antigravity-provider/facts.md`, and a few third-party transcript
parsers, which are marked as such. I read **no** real session or transcript files. Every format
claim about on-disk transcripts is either from docs, from Keitora's existing parsers, or marked
**unverified** and needs a captured fixture before code relies on it.

## TL;DR

1. **Make liveness and busy/blocked hook-free: herdr's screen detection plus the pane process
   tree.** herdr already derives `working`/`blocked`/`idle`/`done` from the screen for all three
   providers, with no hooks (`research-herdr.md`). The process tree confirms the agent is alive.
   Both are cross-platform and already wired in.
2. **Get turn completion and reply text by watching transcript files**, with the session id taken
   from herdr's `agent_session` or chosen at launch (Claude `--session-id`). Keitora already
   parses Claude and Codex transcript tails (`stop_capture.rs`). A watcher that fires on file
   growth and re-runs that parser yields "turn ended + text" without a Stop hook. Codex rollouts
   carry explicit `task_started`/`task_complete` events. Claude's lines need a heuristic (see 1a).
   agy's `transcript_full.jsonl` format is unverified.
3. **Move hooks off the project layer onto a per-launch layer, not the user layer.** This is the
   biggest reliability win for the least code:
   - **Claude:** `claude --settings '<json with hooks>'` applies to that one process. It needs no
     project file and no workspace trust. User settings still merge.
   - **Codex:** `codex -c notify='["keitora","codex-notify",…]'` gives a per-process
     `agent-turn-complete` callback with `last-assistant-message`. It isn't a hook, so it isn't
     subject to hook-hash review or project trust. Keitora already passes `-c` overrides today.
   - **agy:** hooks load only from `hooks.json` files: the global `~/.gemini/config/hooks.json`
     or the workspace's `.agents/hooks.json`. Keep the global file, and make it carry its own
     routing instead of needing `KEITORA_BIN` (details in 5).

   User-level hooks (`~/.claude/settings.json`, `~/.codex/hooks.json`) are a worse fit. They fire
   for every session on the machine. Codex *still* requires hash review for user hooks. They also
   break the user's own setup if Keitora's CLI moves or breaks.
4. **Permission brokering stays hook-based for Claude and Codex, with no hook-free alternative
   that keeps the interactive TUI.** Each candidate has a catch:
   - **Codex `app-server`:** has real approval requests (`item/commandExecution/requestApproval`
     and others), but only for threads that run *inside* that app-server. Keitora's TUI would have
     to become `codex --remote <socket>`, a launch-architecture change. Whether two clients
     subscribed to one thread race on approvals isn't documented.
   - **Claude Channels:** a channel can declare `claude/channel/permission` to receive and answer
     prompts, and it works alongside the local dialog. But channels are a research preview, and
     custom ones need `--dangerously-load-development-channels`, which shows a full-screen consent
     dialog on each launch.
   - **Claude cross-session messaging:** explicitly **cannot** approve permissions.
   - **agy:** has no `PermissionRequest` event at all. `agy acp` presumably carries
     `session/request_permission`, but ACP replaces the TUI.
5. **Treat screen parsing as herdr's job.** Keitora shouldn't grow its own prompt regexes except
   for narrow, testable cases, as it already does for the Codex update screen. When herdr misses a
   screen, the fix belongs upstream in herdr's detection manifest.
6. **OpenTelemetry is a credible secondary event stream** for Claude (env vars per pane) and Codex
   (`-c otel.*` per launch, needs verifying). It costs Keitora a small local OTLP/HTTP receiver.
   It gives prompt-start, tool decisions and (opt-in) assistant text. It has no turn-end or
   "pending approval" event (only a beta trace span), and logs batch every 5 s by default. That
   makes it a corroborating source, not a primary one.

## 1. Transcript / session-file watching

### 1a. Claude Code: `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`

- **Location and mapping.**
  - The path is `<claude projects>/<encoded-cwd>/<id>.jsonl`. Keitora already computes
    `encoded_cwd_dir_name` and `claude_session_file_exists` (`transfer.rs`), and
    `find_live_claude_jsonl` picks the live file.
  - The pane→file mapping has three ways in, best first:
    1. **Pick the id at launch:** `claude --session-id <uuid>` (documented: "Use a specific session
       ID for the conversation"). Keitora then *knows* the file before the agent writes it. A
       resume already passes the id.
    2. herdr's `agent_session` (`source == "herdr:claude"`). This comes from herdr's integration
       hook (`research-herdr.md` §2), so it is *also* hook-dependent. It's a cross-check, not a
       root of trust.
    3. The newest-mtime `.jsonl` in the encoded-cwd dir. That's safe enough because Keitora runs
       one agent per worktree. `/clear` and `/resume` switch to a different file, so the watcher
       must follow the dir, not one file.
- **What's in it (docs plus Keitora's parser).**
  - Each line is a JSON object with a `type`: `user`, `assistant`, `system`, plus others
    (community parsers list `mode` and summary types).
  - Assistant text lives in `message.content[].text`. `stop_capture::claude_reply` already
    extracts "the trailing run of assistant lines since the last user line".
  - Tool use is an `assistant` `tool_use` block. The result comes back as a later `user` line
    carrying `tool_result`.
  - Subagent transcripts are separate files.
- **Derivable signals.**
  - **Busy start:** a new `user` line whose content is a prompt (not a `tool_result`).
  - **Turn complete:**
    - Heuristic: the last line is an `assistant` message whose `message.stop_reason` is
      `end_turn` (or `stop_sequence`/`max_tokens`), with nothing appended for N ms.
    - Claude streams one content block per line, so intermediate lines may carry
      `stop_reason: null`.
    - **Unverified:** whether every turn's final line carries `end_turn`, and whether newer builds
      write an explicit system turn-end line. Capture fixtures before relying on either.
  - **Reply text:** exactly what `stop_capture.rs` does today, triggered by the watcher instead of
    the Stop hook.
  - **Waiting on approval: not derivable.**
    - A pending permission looks like an `assistant` `tool_use` with no matching `tool_result` yet.
      That is byte-identical to a long-running tool (a 10-minute `cargo build`).
    - Docs don't describe any permission-request entry. The OTel docs say the wait is visible only
      as a beta trace span (`claude_code.tool.blocked_on_user`).
- **Reliability.**
  - The file is the conversation's source of truth (resume reads it), so it's always written.
  - The risk is **format drift**: the schema is undocumented, and community parsers warn that it
    changes between releases.
  - Mitigate by parsing defensively, as `stop_capture.rs` already does (unknown types are
    skipped), and by pinning fixture tests per supported Claude version.
- **Latency.** File-system events (`notify` crate: inotify, FSEvents, ReadDirectoryChangesW) arrive
  in under 100 ms on Linux and Windows. FSEvents on macOS can coalesce for about 1 s or more; plan
  item C3 already flags that. Claude writes each line as it streams.
- **Cross-platform.** Fine. The path encoding is already handled on both OSes.
  `CLAUDE_CONFIG_DIR` relocates the projects dir, and `claude_projects_dir()` must honour it.
- **Cost.** Low to moderate. Keitora already has a watcher pattern (`db_watch.rs`, `diff/`) and
  the tail parser. New work: a per-worktree tail-follower, the end-of-turn heuristic, debouncing,
  and fixtures.

### 1b. Codex: `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`

- **Mapping.**
  - The thread id is in the filename and in the first line (`session_meta`: `id`, `cwd`,
    `originator`, `cli_version`; per third-party parsers).
  - Use herdr's `agent_session` (`herdr:codex`) when present, otherwise the newest rollout under
    today's and yesterday's date dirs whose `session_meta.cwd` equals the worktree.
  - Codex has no documented `--session-id` for a fresh launch, so the first mapping can lag a few
    hundred ms after start.
  - `codex resume <id>` appends to the *original* dated file. Don't assume today's dir.
- **What's in it.**
  - `response_item` lines: `message` (role `assistant`, `output_text` blocks), `function_call`,
    `function_call_output`.
  - `event_msg` lines with `payload.type` of `task_started`, `user_message`, `agent_message`,
    `token_count`, `task_complete`, and `*_end` tool events (per third-party parsers).
  - `stop_capture::codex_reply` already reads assistant `response_item`s and falls back to the
    last `agent_message`.
  - One parser notes: "Codex interactive sessions emit task_complete after every turn". That's
    exactly the per-turn completion signal Keitora wants.
- **Derivable signals.**
  - **Busy:** `task_started`.
  - **Turn complete:** `task_complete`. This is an explicit event, much more robust than Claude's
    heuristic. Use `turn_aborted` (an interrupt) as a completion with a reason; unverified,
    confirm with a fixture.
  - **Reply text:** the existing `codex_reply`.
  - **Waiting on approval: probably not derivable.**
    - Codex persists a filtered subset of protocol events. Approval request events
      (`exec_approval_request`) weren't found by any parser surveyed, and one reverse-engineering
      write-up found even `exec_command_begin`/`end` absent from the on-disk format.
    - Treat it as absent unless a captured rollout shows it.
- **Reliability.**
  - The rollout is the thread's source of truth (`resume` reads it).
  - `[history] persistence = "none"` affects `history.jsonl`, not rollouts, as far as the docs
    say. Verify.
  - Format-drift risk is moderate: the format is internal and has changed across versions, but
    `task_started`/`task_complete` have been stable across the parsers surveyed.
- **Latency.** Same as 1a. Codex flushes per event.
- **Cost.** Low. Same watcher, plus the date-tree scan and the `session_meta` read.

### 1c. Antigravity CLI: `~/.gemini/antigravity-cli/brain/<conversationId>/.system_generated/logs/transcript_full.jsonl`

- **Mapping.**
  - The conversation id comes from herdr's `agent_session` (`herdr:antigravity_cli`, unverified
    live; `facts.md`) or from the newest `brain/<id>/` dir.
  - `brain/` is shared by every workspace, so newest-dir is **not** safe with two agy worktrees
    running at once. The transcript's own `cwd`/workspace field (if any; unverified) would have to
    disambiguate.
  - There's no `--session-id`-style flag for a fresh launch; `--conversation <id>` only resumes.
- **Format.**
  - **Unverified.** The hook payload names `transcript_full.jsonl`, and Google's docs say
    `transcript.jsonl`; trust the payload.
  - Each conversation also has `conversations/<id>.db`.
  - Nothing about line shapes, turn boundaries or a stop marker is documented.
- **Assessment.** Usable for reply text *after* the fact, once someone captures a fixture. Not yet
  a turn-completion signal. agy's `Stop` hook carries `fullyIdle` and `terminationReason`, which
  is richer than anything the file is known to hold.
- **Other files.** `cli.log` and `log/cli-<timestamp>.log` exist, and `--log-file <path>` can
  route a session's log to a Keitora-chosen file *per launch*. Whether the log has turn events
  is unverified, but it's worth a 10-minute probe because it needs no hooks and is per-process.

### 1d. Verdict on transcripts

| Need | Claude | Codex | agy |
|---|---|---|---|
| Busy start | Good (new prompt line) | Good (`task_started`) | Unknown |
| Turn complete | Heuristic (`end_turn` + quiet) | **Explicit** (`task_complete`) | Unknown |
| Reply text | Good (existing parser) | Good (existing parser) | Unknown format |
| Waiting on approval | No (indistinguishable from a running tool) | Probably no | No |
| Pane→file mapping | **Deterministic** with `--session-id` | herdr id or cwd match | herdr id only (shared dir) |

## 2. Process-level signals

- **What exists.**
  - herdr `pane process-info` gives the shell pid plus foreground pids. Keitora's perf overlay
    already walks those roots (`perf_source.rs`).
  - `agent.list` adds `launch_pending` and `interactive_ready`.
- **Liveness: strong.**
  - The agent binary (`claude`, `codex`, `agy` / `node`) is the pane's foreground process, or a
    descendant of the shell. Its presence is the most direct "agent present" fact there is, and
    it's immune to hook races.
  - It answers report A outright: a live process means the pane is not `offline`.
  - Matching by executable name needs care for wrapper scripts (`node …/cli.js`, the `codex.cmd`
    shim on Windows). Prefer herdr's own agent label, then fall back to the process.
- **Busy: weak.**
  - CPU above a threshold, or child processes under the agent (a shell running a tool), mean
    "executing a tool". abtop uses "has active descendant" for its `Executing` state.
  - Model thinking/streaming is network I/O with little CPU, so an idle-looking process can still
    be busy.
  - A permission prompt is indistinguishable from idle.
- **Turn complete / reply / blocked:** nothing.
- **Cross-platform.**
  - herdr abstracts process enumeration.
  - Mapping a pid to its open transcript file works with `lsof` (macOS) or `/proc/<pid>/fd`
    (Linux). On Windows there's no cheap equivalent: abtop falls back to newest-mtime there.
    Don't rely on fd mapping.
- **Cost.** Very low; already plumbed.

## 3. Terminal screen parsing

- **herdr already does it.** herdr's lifecycle status for all three providers comes from screen
  detection manifests, not integrations (`research-herdr.md` §2). It's conservative: an
  unrecognized approval UI falls back to `idle`, not `blocked`. herdr pushes changes over
  `events.subscribe`, and `agent.list` exposes `completion_seq`.
- **What Keitora could add with `pane read`.**
  - Narrow, versioned patterns, as it already does for the Codex update screen
    (`is_codex_update_screen`).
  - Fragility:
    - TUI redesigns ship weekly (Claude and Codex), and agy self-updates on every launch.
    - Themes, widths and localization vary.
    - Wrapped lines and alternate-screen redraws complicate matching.
    - Reading a pane costs one herdr call per poll.
  - Keitora should only consume herdr's status, and send detection fixes upstream.
- **Bell frames (cheap, partial).**
  - The bridge's control stream already delivers `{"type":"terminal.bell"}` (parsed as
    `FrameEvent::Other` today).
  - Codex can be told per launch to ring on approval requests and turn completion:
    `-c tui.notifications='["approval-requested","agent-turn-complete"]'`,
    `-c tui.notification_method=bel`, `-c tui.notification_condition=always`.
  - Claude has a similar per-launch notification-channel setting via `--settings`, but the exact
    key and values weren't verified for this doc.
  - Limits:
    - A bell only reaches Keitora for panes with an attached bridge.
    - A bell doesn't say *which* event fired; pair it with herdr status.
    - herdr's emulator probably swallows OSC 9 (only the bell surfaces).
  - It's a useful **tiebreaker** that makes the "blocked" edge immediate on a visible pane, not a
    primary signal.

## 4. Provider-native programmatic channels

### 4a. Codex `app-server` (JSON-RPC)

- **Capabilities (docs).**
  - Transports: stdio (the default), `--listen unix://[PATH]`, and `--listen ws://IP:PORT`
    ("experimental and unsupported"; plain ws only for loopback).
  - Threads and turns: `thread/start`, `thread/resume`, `turn/start`, `turn/steer`,
    `turn/interrupt`.
  - Streamed notifications: `turn/started`, `item/started`, `item/agentMessage/delta`,
    `item/completed`, `turn/completed`.
  - **Approvals as server→client requests:** `item/commandExecution/requestApproval` (decisions
    `accept|acceptForSession|decline|cancel|acceptWithExecpolicyAmendment`),
    `item/fileChange/requestApproval`, `item/permissions/requestApproval`,
    `mcpServer/elicitation/request`, and the experimental `tool/requestUserInput`. A
    `serverRequest/resolved` notification follows each one.
  - **Status:** `thread/status/changed`, with statuses `notLoaded|idle|systemError|active` and
    `activeFlags` such as **`waitingOnApproval`**. That's precisely the blocked-on-human signal.
    `thread/list` and `thread/loaded/list` report runtime status.
- **The catch: the interactive TUI must run on that app-server.**
  - `codex --remote <endpoint>` (`ws://`, `wss://`, `unix://`, `unix://PATH`) attaches the TUI to
    a running app-server. Keitora would start one app-server (per worktree, or one shared), launch
    the pane as `codex --remote unix://…`, and connect its own client to the same server to
    subscribe to the thread via `thread/resume`.
  - Open questions the docs don't answer:
    - whether two connections subscribed to one thread both receive server→client approval
      requests (and who wins);
    - whether a second subscriber can `thread/resume` a thread the TUI created without disturbing
      it;
    - whether `unix://` works on native Windows (AF_UNIX exists on Windows 10+, but Codex support
      isn't documented). Loopback ws works everywhere but is labelled unsupported.
  - Third-party guides claim multi-subscriber support; the official docs are silent.
  - Keitora already has a minimal app-server JSON-RPC client (`transfer.rs`), so client cost is
    modest. Launch, lifecycle and resume cost is large: the app-server becomes a supervised
    daemon, herdr resume must restart both, and a crash takes out the TUI.
- **Verdict.**
  - The best-structured signal of any provider, and the only hook-free blocked signal for Codex.
  - A medium-to-large architecture change with unverified multi-client semantics. **Spike it
    behind a flag** (one worktree, macOS and Windows) before committing.
  - Don't make it the 0080 fix.

### 4b. Codex `notify` (per launch via `-c`)

- **Docs.**
  - User-level only: project `.codex/config.toml` can't set it.
  - Runs a program with one JSON argument for `agent-turn-complete`, with fields `type`,
    `thread-id`, `turn-id`, `cwd`, `input-messages` and `last-assistant-message`.
- **Per-launch instead of global.**
  - Keitora already launches Codex with `-c` overrides (`CODEX_NO_UPDATE_CHECK`).
  - `-c notify='["<abs keitora>","session","codex-notify"]'` should apply to that process only.
    That avoids the global-write conflict raised in plan Open question 2, and the user's own
    `notify` is untouched for non-Keitora sessions.
  - **Verify:** that a `-c` override of `notify` is honoured (it is a CLI layer, not a project
    layer), and how argv quoting behaves on Windows through herdr and PowerShell.
  - `notify` replaces rather than chains the user's value *for that process*. If the user relies
    on `notify` inside Keitora panes, Keitora's handler can exec the user's original command
    (read from their config) after recording.
- **Properties.**
  - Not a hook, so no hook-hash review, no project trust, no `--dangerously-bypass-hook-trust`.
  - Carries `turn-id`, so a repeated turn can be deduped.
  - Fires on turn completion only: no busy-start, no approval.
  - Still a child process that can fail silently, so log failures (plan D).

### 4c. Codex OpenTelemetry

- `[otel]`, user-level only per docs; whether a `-c otel.*` override works needs verifying.
- Event names: `codex.conversation_starts`, `codex.user_prompt`, `codex.tool_decision`,
  `codex.tool_result`, `codex.sse_event`, `codex.agent_response` (opt-in
  `log_agent_responses`).
- No turn-end or approval-pending event.
- Corroboration only.

### 4d. Claude Code cross-session messaging socket

- **Facts (docs).**
  - Each session binds an inbox: a Unix socket on macOS and Linux, a named pipe on native Windows
    (v2.1.234+).
  - The path is exported to hooks and Bash as `CLAUDE_CODE_MESSAGING_SOCKET`, and
    `CLAUDE_CODE_MESSAGING_TOKEN` is the auth token.
  - Windows *requires* a first line `{"type":"auth","token":…}`.
  - Messages from unverified senders are held for approval when the receiving session bypasses
    permissions.
  - An idle session starts a new turn when a message arrives. A busy one reads it between tool
    calls.
  - It **can't approve permissions**, and commands in the text don't run.
  - `notify_when_idle` exists, but only Claude-to-Claude via `SendMessage`.
- **For Keitora.**
  - Discovering the socket path and token needs either a hook or Bash inside the session (which
    defeats the purpose), or reading `/status`.
  - Delivery is framed as a *peer message*, not a user prompt: the model is told it came from
    another session, which changes behaviour.
  - Not a status source, and not a broker.
  - **Not recommended**; herdr `agent prompt` is the right prompt-injection path.

### 4e. Claude Code Channels (MCP)

- **Mechanism.**
  - A channel is an MCP server declaring `experimental['claude/channel']`. It pushes
    `notifications/claude/channel` events into the session.
  - With `experimental['claude/channel/permission']`, it also receives
    `notifications/claude/channel/permission_request` (`request_id`, `tool_name`, `description`,
    `input_preview`) and answers with `notifications/claude/channel/permission` (`allow`/`deny`).
    The local dialog stays live, and the first answer wins.
  - Covers tool approvals only, not trust or MCP-consent dialogs.
- **For Keitora.**
  - This is the cleanest *hook-free* Claude permission broker, and the request notification
    itself is a precise "blocked on approval" signal with the tool name.
  - Blockers:
    - It's a research preview.
    - A custom channel needs `--dangerously-load-development-channels server:<name>`, which shows
      a full-screen consent dialog on *every* launch (it breaks unattended wakes).
    - Team and Enterprise orgs must enable `channelsEnabled`.
    - The flag is ignored under `-p` and the SDK.
  - The MCP server config can be injected per launch (`--mcp-config`), so no project files are
    needed.
- **Verdict.** Track it. Adopt it when custom channels can load without the dev flag (allowlist
  or org `allowedChannelPlugins`). Until then hooks remain the Claude broker.

### 4f. Claude headless (`-p --input-format stream-json --output-format stream-json`) / Agent SDK

- Gives perfect structured events (assistant messages, `result` with stop info) and permission
  handling via `--permission-prompt-tool` or the SDK `canUseTool`.
- But it **replaces the interactive TUI**: there's nothing for the user to type into in the herdr
  pane.
- Only fits a future "headless driven worktree" mode. Not an answer for 0080.
- Same for Claude background sessions: `claude agents --json` lists only supervisor-hosted
  `--bg` sessions, not PTY sessions in herdr.

### 4g. Claude OpenTelemetry (per pane via env)

- **Setup.** `CLAUDE_CODE_ENABLE_TELEMETRY=1`, `OTEL_LOGS_EXPORTER=otlp`,
  `OTEL_EXPORTER_OTLP_PROTOCOL=http/json`, `OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:<port>`,
  plus `OTEL_RESOURCE_ATTRIBUTES=keitora.worktree=<id>`. Each goes in the pane env, which Keitora
  already sets (`env::pane_env_with`).
  - Project settings can't set OTEL vars; env can.
  - `OTEL_LOGS_EXPORT_INTERVAL` defaults to 5000 ms and can be lowered.
- **Events.**
  - `claude_code.user_prompt` (busy start, with `prompt.id`).
  - `claude_code.tool_decision` / `tool_result`.
  - `claude_code.api_request`.
  - `claude_code.assistant_response`: text only with `OTEL_LOG_ASSISTANT_RESPONSES=1`.
  - No turn-end event and no pending-permission event. Permission waits appear only as the beta
    trace span `claude_code.tool.blocked_on_user`, which needs tracing.
- **Risks.**
  - It overrides a user's own `OTEL_*` setup in Keitora panes: detect it and skip.
  - Keitora must host an OTLP/HTTP-JSON receiver.
  - Batching adds latency.
- **Verdict.** Secondary. Worth it only if the trace span proves to be a reliable "blocked"
  source.

### 4h. Antigravity CLI

- `agy acp` (Agent Client Protocol over stdio) presumably exposes `session/prompt`,
  `session/update` streaming and `session/request_permission`. It's a full programmatic channel,
  but like 4f it **replaces the TUI**.
- `--remote-control` / `remote-control` exist (`facts.md`). Their transport and whether they're
  local are undocumented here; probe them.
- `-p --output-format json|stream-json` is headless only.
- Nothing hook-free gives turn completion for the interactive TUI today, other than herdr's screen
  status going `working`→`idle` and, possibly, `--log-file` content (unverified).

## 5. User-level vs project-level (vs per-launch) hooks

| Layer | Claude | Codex | agy |
|---|---|---|---|
| Project (today) | `.claude/settings.json` (or `.local`): runs in trusted or `-p` sessions; worktrees share the main checkout's trust | `.codex/hooks.json`: **ignored unless the project is trusted**, plus hash review unless bypassed | `.agents/hooks.json`: interactive trust gating unverified |
| User | `~/.claude/settings.json`: always loads | `~/.codex/hooks.json`: loads in untrusted projects, **but still needs hash review** ("new or changed hooks are marked for review and skipped until trusted") | `~/.gemini/config/hooks.json`: Keitora already uses it |
| **Per launch** | **`--settings '<json>'`**: one process only; hooks merge with the user's | **`-c notify=…`** (turn-complete only); whether `-c` can define hooks is undocumented; `--dangerously-bypass-hook-trust` per launch | `--settings` overlay exists, but hooks load only from `hooks.json` files |

**User-level risks.**

- Hooks fire for *every* session on the machine, including ones the user runs outside Keitora.
- A missing or old `keitora` binary breaks or slows every session. On Claude, a hook that hangs
  stalls `SessionEnd` past its 1.5 s budget.
- Codex user hooks still trip hash review on every Keitora upgrade that changes the command line.
- The user's own `disableAllHooks`/`allowManagedHooksOnly` policies apply.
- It's a global write the user may not expect. That goes against the posture already in
  `global_writes.rs`.
- Routing needs a worktree resolver in the CLI (by `cwd` plus the Keitora registry). That resolver
  is already the agy design, and its silent-failure mode is exactly report D.

**Recommendation.**

- **Prefer per-launch injection**, so hooks exist exactly for Keitora-launched processes:
  - Claude: `--settings` carrying the same hook set `hooks.rs` writes today.
  - Codex: `-c notify` for completion; keep `--dangerously-bypass-hook-trust` on *every* launch
    step for the remaining Codex hooks (plan B1b), or move Codex brokering to app-server later.
- **Keep project-file hooks only as a fallback** for sessions the user starts by hand in a
  worktree.
- **agy stays global.** Make its command self-routing, without `KEITORA_BIN`:
  - write an absolute CLI path at install time;
  - pass a marker env (e.g. `KEITORA_PANE=1`) so the hook no-ops outside Keitora panes;
  - re-assert the file after herdr's integration install.

## 6. Recommended signal per need, per provider

"Primary" is what the state machine trusts. "Fallback" is used when the primary is absent or
stale, and as a cross-check.

| Need | Claude Code | Codex CLI | Antigravity CLI |
|---|---|---|---|
| **Liveness** | herdr `agent_status ∈ {launch_pending, working, blocked, idle}` + pane process tree / **fallback:** transcript mtime recency | same / **fallback:** rollout mtime | same / **fallback:** `brain/<id>` mtime |
| **Idle / busy** | herdr status (pushed via `events.subscribe`) / **fallback:** transcript (new prompt line → busy; `end_turn` + quiet → idle); hooks via `--settings` as a third source | herdr status / **fallback:** rollout `task_started` / `task_complete` | herdr status (screen-only) / **fallback:** `Stop` hook (global) |
| **Blocked on human** | herdr `blocked` / **fallback:** `Notification(permission_prompt)` + `PermissionRequest` hooks via `--settings`; bell frame on attached panes as a tiebreaker. Future: channel `permission_request` | herdr `blocked` / **fallback:** `PermissionRequest` hook (bypass-trust launch) + `tui.notifications` bell. Future: app-server `activeFlags: waitingOnApproval` | herdr `blocked` only (no agy event exists); attached-pane bell if agy rings (unverified) |
| **Turn complete + reply text** | transcript watcher (`--session-id`-mapped file, existing `claude_reply`) **and** `Stop` hook via `--settings` (`last_assistant_message`); dedupe by session + turn; herdr `completion_seq` as a timing hint | **`-c notify` `agent-turn-complete`** (`last-assistant-message`, `turn-id`) **and** rollout `task_complete` + `codex_reply`; Stop hook third | global `Stop` hook (`fullyIdle`) / **fallback:** herdr `working→idle` edge + `transcript_full.jsonl` tail once its format is fixtured |
| **Permission brokering** | `PermissionRequest` hook via `--settings` (project file as fallback). Future: Channels relay | `PermissionRequest` hook with trust bypass on every launch. Future: app-server approval requests | none (out of scope, as in the plan) |

**Completion semantics.**

- With two sources per provider, `wait` resolves on whichever arrives first, deduped by
  `(session id, turn id or transcript line uuid)`.
- That's also the fix for B2: "idle with no reply" becomes "fetch the reply from the transcript
  now" before declaring `Stalled`.

## 7. Reliability ranking (most to least reliable, for this use)

1. **Process tree / herdr agent presence.** Liveness only, but near-certain and hook-free.
2. **Codex rollout `task_complete` and Codex `notify` via `-c`.** Explicit per-turn events
   written by the agent itself, no trust gating. Format risk is moderate for the rollout and low
   for `notify`, which is a documented contract.
3. **herdr screen status (`working`/`blocked`/`idle`), pushed.** Hook-free for all three
   providers and already integrated. It errs toward `idle` on unknown prompts, which misses
   `blocked`, and is only as current as herdr's manifests.
4. **Per-launch hooks** (Claude `--settings`). The same contract as today's hooks, minus the
   trust and project-file failure modes. Silent failure and process races still need plan D's
   logging and the session-id stale-writer guard.
5. **Claude transcript watcher.** Always written and carries reply text, but turn end is
   heuristic and the format is undocumented.
6. **Codex app-server events / Claude Channels permission relay.** The most precise signals, but
   each needs an architecture change or a research-preview dev flag. Spike, don't depend.
7. **OpenTelemetry (Claude, Codex).** Documented but batched, with no turn-end or pending-approval
   log event, and it needs a local receiver.
8. **User-level hooks.** Fewer trust problems than project hooks for Claude, but global blast
   radius. Codex hash review still applies.
9. **agy transcript / logs.** The format is unverified, and the shared `brain/` dir hurts
   mapping.
10. **Keitora-side screen parsing / bells.** Fragile, attached-only (bells), or ambiguous. Only
    as tiebreakers or narrow, fixture-tested cases.
11. **Claude cross-session socket, Claude headless/SDK, agy ACP.** Wrong tool: they can't approve
    anything, frame input as a peer message, or replace the TUI.

## 8. Verification owed before building on this

- **Claude transcript shape:**
  - Capture scrubbed fixtures (current version) for a plain turn, a tool turn, a denied
    permission, an interrupted turn, `/clear`, and compaction.
  - Confirm the final-line `stop_reason` and any system turn-end entry.
- **Codex rollout shape:**
  - Fixtures for one turn, an approval-gated command, an interrupt (`turn_aborted`?) and a resume.
  - Confirm whether any approval event is persisted.
- **Codex `-c` overrides:**
  - `-c notify=[…]` honoured, argv quoting on Windows and macOS through herdr.
  - Whether `-c otel.*` and `-c hooks…` are accepted.
- **Claude `--settings` hooks:** they fire in an interactive session in an untrusted worktree,
  and they merge with (don't replace) user hooks.
- **agy:** `transcript_full.jsonl` line shapes, `--log-file` content, whether it rings a bell when
  blocked, and `remote-control` transport.
- **Codex app-server spike:** a TUI via `codex --remote unix://…` plus a Keitora subscriber on the
  same thread, on macOS and Windows. Who gets the `requestApproval`?
- **macOS FSEvents latency** for appended `.jsonl` lines (pairs with plan C3).

## Sources

- Claude Code: [hooks](https://code.claude.com/docs/en/hooks),
  [cross-session messaging](https://code.claude.com/docs/en/cross-session-messaging.md),
  [channels reference](https://code.claude.com/docs/en/channels-reference.md),
  [CLI reference](https://code.claude.com/docs/en/cli-reference.md),
  [permissions / workspace trust](https://code.claude.com/docs/en/permissions.md),
  [monitoring (OTel)](https://code.claude.com/docs/en/monitoring-usage.md)
- Codex: [app-server](https://learn.chatgpt.com/docs/app-server),
  [advanced config (notify, otel, hooks layers)](https://learn.chatgpt.com/docs/config-file/config-advanced),
  [hooks](https://learn.chatgpt.com/docs/hooks)
- Antigravity: [hooks](https://antigravity.google/docs/hooks), plus `plans/0077-antigravity-provider/facts.md`
- Third-party (format hints only, unverified): abtop's Codex collector (docs.rs `abtop`),
  `toolpath-codex` types (docs.rs), community Claude transcript-format notes.
- Keitora: `crates/keitora-cli/src/stop_capture.rs`, `src-tauri/src/transfer.rs`,
  `src-tauri/src/backend/herdr/{resume,agent,bridge,cli}.rs`, `src-tauri/src/hooks.rs`,
  `research-herdr.md`.
