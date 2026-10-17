# Keitora — Logistics Dispatch · Build Plan

A cross-platform (Mac-first) desktop app that unifies **task tracking** and the **live Claude Code
terminal sessions** across a multi-worktree repo, plus an **advisory dispatcher** that matches open
tasks to available worktrees. The app is desktop-only; the former mobile/remote surface (iOS client,
iroh transport, remote host) has been removed.

Keitora replaces the manual iTerm-tabs workflow: all worktree Claude sessions run **inside Keitora** as
embedded terminals.

---

## 1. Locked decisions

| Decision | Choice | Rationale |
|---|---|---|
| Desktop stack | **Tauri** (Rust backend + React webview) | Lighter binaries; small native footprint. PTY + SQLite handled in Rust. |
| Platform priority | **macOS first**, Windows fast-follow | Use raw cross-platform PTYs (one code path), so Windows needs no separate session backend (see §5, §10). |
| Dispatcher autonomy (v1) | **Advisory** — proposes + injects only on approval | Keeps the human in the loop; reuses the existing atomic claim/deps protocol as the source of truth. |
| Terminals | **Embedded in Keitora** (xterm.js panes, no iTerm) | Single window for board + all sessions; Keitora owns the PTYs so it can observe idle/busy and inject prompts. |

---

## 2. Context: the task store & worktree model

Keitora is built around a **shared SQLite task DB** that is the coordination bus between worktree sessions.

- **Shared task DB** at `~/code/keitora-worktrees/shared/tasks.db` (WAL). Config `task-config.json` →
  `{ db_path, archive_days, log_retention_days, worktrees[] }`. Keitora reads this config; the `worktrees`
  list drives the rail lanes and board columns.
- **Schema** (defined in `crates/keitora-tasks`):
  - `tasks(id, title, description, status, priority, assigned_to, created_by, tags, branch, pr_url, created_at, updated_at)`
    — status ∈ `open|active|backlog|done|cancelled`; priority ∈ `low|normal|high|urgent`; `assigned_to` = worktree name.
  - `task_deps(task_id, depends_on)` — dependency DAG with cycle detection.
  - `task_log(id, task_id, worktree, action, detail, created_at)` — activity log.
- **Clients on the same DB:**
  - **CLI** `./keitora` (the `keitora-cli` crate; task tracking under `keitora task …`) — atomic `claim` (TOCTOU-safe; refuses if assigned elsewhere
    or blocked), `done` (reports newly-unblocked tasks), `block/unblock/deps`, `board`, `--json` everywhere.
  - **The Tauri app** (Phase 2) reads/writes the same DB through the same `keitora-tasks` library.

**Design principle:** the DB is the coordination bus, and task logic lives **once** in `keitora-tasks` —
the CLI and the app are peers on top of it, never forks of the logic.

**Pluggable backend (#0077).** Task tracking + dispatch now sit behind an async `TaskBackend` trait
(`crates/keitora-tasks/src/backend.rs`); `SqliteBackend` is the first implementation, selected by the
factory `open_backend(&ProjectConfig)` via a forward-compatible `[tasks] backend` config field
(default `"sqlite"`). Machine-local coordination GitHub can't model — worktree sessions —
stays on a concrete synchronous `LocalStore`. A two-tier
contract suite (`consistency()`-gated) validates any backend. This makes the store swappable for an
online backend (below) without touching consumers.

---

## 3. Architecture (Tauri)

```
┌─────────────────────────── Keitora (Tauri) ─────────────────────────┐
│  Webview · React 19 + Vite + TS                                      │
│   ┌────────────┐  ┌──────────────────────────────────────────────┐  │
│   │ Task Board │  │ Terminal Grid — xterm.js, one pane / worktree │  │
│   │ (Kanban,   │  │ sayo · sol · idris · chaerin · feng · vesper  │  │
│   │  live)     │  │ each pane: idle/busy/waiting badge + task #   │  │
│   └────────────┘  └──────────────────────────────────────────────┘  │
│   ┌──────────────── Dispatcher (advisory) ─────────────────────────┐ │
│   │ Suggestion cards: "idris idle + tag dialogue → #42"  [Approve] │ │
│   └────────────────────────────────────────────────────────────────┘│
├──────────────── Tauri commands (invoke) + events ───────────────────┤
│  Rust backend (src-tauri/)                                          │
│   • pty.rs       portable-pty: spawn/own one PTY per worktree,      │
│                  stream output → events, accept input/resize        │
│   • tasks.rs     rusqlite on shared/tasks.db (WAL); reuse atomic    │
│                  claim SQL; poll for external changes → event       │
│   • sessions.rs  worktree_sessions state, fed by Claude Code hooks  │
│   • dispatch.rs  headless `claude -p` → assignment proposals        │
└──────────────────────────────────────────────────────────────────────┘
```

### Backend ↔ frontend contract
- **Events (Rust → JS):**
  - `pty://output/<worktree>` — raw bytes for that pane's xterm.
  - `pty://exit/<worktree>` — session ended.
  - `tasks://changed` — DB changed (carries new revision); frontend refetches the board.
  - `sessions://changed` — a worktree's idle/busy/waiting state changed.
- **Commands (JS → Rust, `invoke`):**
  - `pty_write(worktree, data)`, `pty_resize(worktree, cols, rows)`, `pty_restart(worktree)`.
  - `tasks_list(filter)`, `task_claim(id, worktree)`, `task_done(id, worktree)`, `task_update(...)`,
    `task_new(...)`, `task_board()` — thin wrappers over the same SQL the CLI uses.
  - `dispatch_suggest()` → ranked proposals; `dispatch_apply(worktree, task_id)` → gated injection.

---

## 4. UX / UI design & layout

> Keitora must be **beautiful and performant** — not a utilitarian dashboard. Beautiful here means
> disciplined typography, spacing, and a tight semantic color system; performant means a steady 60fps
> with 6 live terminals.

### Design intent
A **dispatch control room**: calm, dense, legible — a flight-strip / air-traffic board where 6 agents are
working and you orchestrate them. Dark-first, one restrained accent, motion only where it conveys state.

### Layout — three zones

```
┌─ top bar ─ Keitora · project / worktree ─────────────── [Board][Focus][Grid] ─┐
├──────────┬────────────────────────────────────────────────┬──────────────────┤
│ ▾ proj-a │  STAGE                                           │  INSPECTOR       │
│ ●sayo #42│  active surface: Board / Terminal / Split / Grid │ task · session · │
│ ◐sol  #7 │                                                  │ or proposal      │
│ ◑idris   │                                                  │ detail (slides)  │
│ ●chaerin │                                                  │                  │
│ ◐feng #19│                                                  │                  │
│ ○vesper  │                                                  │                  │
│ ──────── │                                                  │                  │
│ ⚡Dispatch│                                                  │                  │
└──────────┴────────────────────────────────────────────────┴──────────────────┘
```

- **Rail (left, ~240px, collapsible to icons):** the persistent spine and at-a-glance monitor. Registered
  projects are collapsible **groups** (drag to reorder, context menus on project and lane); the active
  project's group lists one lane per worktree — status dot, name, active task chip, and a throttled "last
  line" preview when busy — while other groups show their live lanes. Clicking a lane in another project
  switches to it. Dispatcher pinned at the bottom. Always visible: situational awareness is never lost.
- **Top bar:** brand, a read-only `project / worktree` breadcrumb, and the view pills.
- **Stage (center):** the active surface, switchable (⌘1/2/3):
  - **Board** — kanban by worktree (the task board) + a dispatch-suggestions strip.
  - **Terminal** — one focused, full-fidelity xterm for the selected worktree.
  - **Split** — board + focused terminal side by side (resizable): work while watching.
  - **Grid** — all 6 terminals tiled, for pure monitoring.
- **Inspector (right, contextual, collapsible):** task detail (logs, deps, branch/PR), session detail, or a
  dispatch proposal with rationale + Approve. Slides in; never blocks the stage.

### Status as visual language
One semantic system reused everywhere (dots, lane tints, badges):
- **Session:** idle = green · busy = amber (gentle pulse) · waiting-for-input = blue · offline = gray.
- **Task:** priority (urgent/high/normal/low) and status colors **mirror the `task` CLI** so the
  vocabulary stays consistent across CLI and app.

### Signature interactions
- **⌘K command palette** — jump to a worktree, new task, request suggestions, switch views.
- **Drag a task card onto a lane** = manual dispatch (override the advisory engine), via dnd-kit.
- **Approve a suggestion** — the card animates into the target lane as the prompt is injected; you watch
  that session pick it up.
- Keyboard-first: ⌘1–6 focus a session, ⌘B board, ⌘J terminal, ⌘\\ split.
- OS notification + rail glow when a session enters `waiting`.

### Visual / styling stack
- **Tailwind CSS + Radix UI primitives** (accessible dialogs, dropdowns, tooltips) — full bespoke design
  control with built-in a11y. (Alternative: emotion/styled, but Tailwind+Radix is the better fit for a
  fresh, polished, custom look.)
- `react-resizable-panels` for zone splits · `lucide-react` icons · `motion` (Framer) for the few
  purposeful animations (inspector slide, card fly-in, dot pulse) — all gated on `prefers-reduced-motion`.
- Typography: high-legibility mono in terminals (JetBrains Mono / SF Mono), Inter/SF for UI, tabular
  numerals for IDs/counts.
- A small **design-token layer** (color, spacing, radii, elevation, motion) so the look stays consistent
  and themeable.

### Performance budget (60fps with 6 live PTYs)
- **xterm WebGL addon** on every terminal (GPU, not DOM).
- **Only focused/visible terminals render live.** Off-stage panes pause xterm writes and show the
  throttled last-line preview in the rail; freeze or unmount background xterms.
- **Batch PTY output** on the Rust side (flush ~16–32ms) so the webview isn't event-stormed.
- **Isolate terminal state from data state** — terminals own their xterm via refs and never re-render on
  task/board updates.
- **Virtualize** long task lists and logs. Use CSS transforms for panel motion; avoid layout thrash.

### Screens to design (high-fi, before/with Phase 1–2)
1. Dispatch home (Board + suggestions + rail) — the hero screen.
2. Terminal focus + rail · 3. Terminal grid · 4. Split.
5. Task detail inspector · 6. Dispatch proposal card + approval · 7. ⌘K palette ·
8. Empty / loading / error / offline states.

### Process recommendation
Before wiring live data, produce a **static high-fidelity mockup** (a single self-contained HTML page) of
the Dispatch home + one terminal view, plus the token/color system. Cheap to iterate, locks the look, and
de-risks "beautiful" before any Rust/PTY work.

---

## 5. Session model (embedded terminals)

- Each worktree session = a **raw PTY** (Rust `portable-pty`) running `claude` directly in the worktree
  directory. **No tmux.**
  - **Why raw PTY:** `portable-pty` is one cross-platform API — Unix PTY on macOS/Linux, ConPTY on
    Windows — so **the same session code ships on both platforms** with no separate backend. tmux is
    macOS/Linux-only and would force a divergent Windows path.
  - **Context recovery via the session daemon (task #0040):** PTY ownership now lives in a standalone
    **`keitora-ptyd`** daemon (Unix-socket; same cross-platform `portable-pty`, still **no tmux**) that
    outlives the GUI. Relaunching Keitora or switching projects **reattaches to the still-running `claude`
    with scrollback intact**, rather than restarting with `claude --continue`. The GUI is a thin daemon
    client. (The earlier v1 plan — raw PTYs tied to Keitora's lifecycle, recovered
    with `claude --continue` and losing live scrollback on restart — is **superseded** by the daemon.)
  **2026-10-06 note:** `keitora-ptyd` and `keitora-pty-proto` have since been removed. **herdr is now the only PTY
  backend** (a herdr server owns the PTYs and outlives the GUI; if it can't start the app shows an unavailable
  backend and the rail says why). The daemon text above is kept as history.
  **2026-10-17 note (scrollback model):** herdr streams rendered viewport frames, not raw PTY bytes, so
  xterm keeps almost no history of its own. **herdr owns each pane's history** (`[pty]
  herdr_scrollback_limit_bytes`, default 16 MB of ghostty page memory, about 9k rows at 200 columns), and
  the wheel / Shift+PgUp/PgDn send `terminal.scroll` to herdr, which routes it: a mouse report to an app
  that enabled mouse reporting (e.g. Claude Code's fullscreen mode scrolls its own transcript), arrow keys
  under alternate-scroll, otherwise herdr's own history. An app that emits `ESC[3J` (Claude Code's
  fullscreen clear and its scroll-region renderer on resize/view switches; Codex's full clear) erases herdr's
  history; that can't be recovered on the Keitora side. Cmd+F searches the visible viewport only.
  **2026-10-10 note:** the herdr backend is verified on macOS. herdr is located from `[pty] herdr_bin`,
  `KEITORA_HERDR_BIN`, the process `PATH`, the Homebrew / `/usr/local` / `~/.local` bin dirs, the newest
  standalone release, then the login-shell `PATH`, and a missing herdr shows a not-found message on the rail with a Restart
  button instead of an unavailable backend.
  **2026-10-11 note:** when herdr is recovered, sessions are resumed up to `[recovery] resume_concurrency`
  (default 3) at a time, and each pane unlocks as soon as its own session is back. The run drops to one at a
  time after the first agent-startup timeout or early exit. A session can be **terminated** from the rail's
  right-click menu. It stays down across restarts, is skipped by recovery, and comes back only through
  **Launch** or **Launch fresh**. The flag is kept in `session_flags` in the resume DB.
- Read loop per PTY in a Rust thread → emit `pty://output/<wt>`; xterm renders (webgl addon for perf).
- Input/resize via `pty_write` / `pty_resize`; fit-addon keeps the PTY sized to the pane.
- Worktree list + paths come from `task-config.json` (single source of truth).

---

## 6. Session status (idle / busy / waiting) — via Claude Code hooks

Keitora needs to know when a session is **safe to dispatch to**. Output-scraping is fragile; use
**Claude Code hooks** installed per worktree (`.claude/settings.json`), mapping turn lifecycle → state:

| Hook | Meaning | Recorded state |
|---|---|---|
| `UserPromptSubmit` | a turn started | `busy` |
| `Notification` | waiting for input / permission | `waiting` |
| `Stop` | turn finished | `idle` |
| `SessionStart` / `SessionEnd` | session up/down | `idle` / offline |

Each hook runs a tiny command (a new `keitora session <state>` subcommand on the shared CLI, or a small
`keitora-hook` script) that upserts a new table:

```sql
CREATE TABLE IF NOT EXISTS worktree_sessions (
  worktree        TEXT PRIMARY KEY,
  state           TEXT NOT NULL DEFAULT 'idle',   -- idle | busy | waiting | offline
  current_task_id INTEGER,
  pid             INTEGER,
  last_heartbeat  TEXT NOT NULL DEFAULT (datetime('now'))
);
```

Keitora polls/watches this table (with PTY ownership as a fallback signal) and emits `sessions://changed`.

---

## 7. Dispatcher (advisory v1)

The LLM is **purely advisory**; Keitora performs the mechanical injection, gated on idle state.

1. **Propose** — `dispatch_suggest()` invokes a **headless** `claude -p` with `--output-format stream-json`,
   given: open + unblocked tasks (title, tags, priority, deps), and each worktree's state + recent tags
   (its specialties). It returns ranked `{task_id, worktree, rationale, confidence}` proposals.
2. **Render** — Dispatcher panel shows proposal cards with rationale + an **Approve** button.
3. **Apply** — `dispatch_apply(worktree, task_id)`:
   - **Refuses** unless `worktree_sessions.state == 'idle'` (never inject into a busy session).
   - Injects into that pane's PTY a prompt like: *"Claim task #42 with `keitora task claim 42`, then work on it: <title>. <description>."*
   - The worktree's own Claude is the executor; the existing atomic `claim`/deps logic governs correctness.
   - Logs to a new `dispatch_log(id, task_id, worktree, decision, rationale, created_at)` for audit.
4. **v2 (later):** an "auto-dispatch" toggle that applies high-confidence proposals to idle sessions
   automatically. Out of scope for v1.

Alternative considered: dispatcher as its own interactive pane with a `/dispatch` skill. Rejected for v1 —
headless-propose + UI-approve is more transparent and keeps the LLM out of the injection path.

---

## 8. Repo layout

```
keitora/
  docs/build-plan.md            ← this document
  package.json  vite.config.ts  tsconfig.json
  src/                          ← React 19 frontend
    main.tsx  App.tsx
    components/
      Shell/        AppShell.tsx Rail.tsx TopBar.tsx Inspector.tsx
      TerminalGrid/ TerminalPane.tsx   (xterm.js + fit + webgl + search)
      TaskBoard/                        (task board UI — kanban by worktree)
      Dispatcher/   SuggestionsPanel.tsx
    lib/ipc.ts  lib/events.ts           (invoke + event wrappers)
    styles/tokens.css  tailwind.config.ts   (design-token layer)
  crates/
    keitora-tasks/    (Rust task-store library: schema, atomic claim, deps — shared by CLI + app)  [done]
    keitora-cli/      (clap CLI `task` over keitora-tasks)                                            [done]
  task                                    (wrapper script for the CLI)                          [done]
  Cargo.toml                              ([workspace] over crates + src-tauri)                 [done]
  src-tauri/
    Cargo.toml  tauri.conf.json
    src/
      main.rs        (setup, command/event registration)
      pty.rs         (portable-pty session manager)
      tasks.rs       (thin Tauri commands over the keitora-tasks crate; change polling)
      sessions.rs    (worktree_sessions ingestion + watch)
      dispatch.rs    (headless claude proposals)
```

Key crates: `tauri`, `portable-pty`, `rusqlite` (bundled, WAL), `serde`, `notify` (fs watch), `tokio`.
Frontend: React 19, `@xterm/xterm` + `@xterm/addon-fit`/`-webgl`/`-search`, **Tailwind CSS + Radix UI**,
`react-resizable-panels`, `lucide-react`, `motion`, `@dnd-kit/*` (drag-to-dispatch).

---

## 9. Phased plan

Status: **Phase 0 ✅ done** · **Phase 1 ✅ done (PR #1)** · **Phase 2 ✅ done (PR #2)** · **Phase 3 ✅ done (PR #4)** · **Phase 4 ✅ done (PR #5)** · **Phase 5 ✅ done (PR #7)** · Phase 6 planned.

- **Phase 0 — Design + spike (prove the look and the core).**
  (a) **Design:** static high-fi mockup of the Dispatch home + a terminal view, plus the design-token /
  color system (§4). Lock the look before wiring data.
  (b) **Spike:** Tauri app boots; one xterm pane bound to a raw `portable-pty` running `claude` in `sayo`;
  bidirectional I/O + resize work. *Exit:* the mockup is approved, and you can type in Keitora and see Claude respond.
- **Phase 1 — Terminal grid.** All 6 worktrees from `task-config.json`; raw cross-platform PTYs
  (no tmux); layout/focus/resize; context recovery via `claude --continue` on app restart (§5).
  *Exit:* Keitora fully replaces the iTerm tabs.
- **#0033 — Clickable terminal links. Done (PR #123).** Plain and OSC 8 web/file
  links open through system default applications; source location suffixes are
  removed. File opening validates local worktree containment and rejects executable
  targets. Hover destinations, error feedback, and selection guards are included.
- **Phase 2 — Task board. ✅ done (PR #2).** Wired the app onto the `keitora-tasks` crate; `tasks://changed`
  WAL watcher; Board (kanban) + Inspector; claim/unclaim/done/update/new; live updates; Tailwind+Radix UI.
  *Exit met:* the board reflects the DB live and edits round-trip with the `task` CLI. (Dispatch
  suggestions → Phase 4; live session-state dots → Phase 3.)
- **Phase 3 — Session status. ✅ done (PR #4).** Auto-installs Claude Code hooks per worktree
  (`.claude/settings.local.json`) running `keitora session <state>`; `worktree_sessions` populated;
  idle/busy/waiting/offline dots on the Rail (with task chip) and board column headers; live via
  `sessions://changed`. *Exit met:* states are accurate in real time. (Pane-badge overlay deferred;
  offline = SessionEnd + PTY-exit/ownership, not heartbeat staleness.)
- **Phase 4 — Dispatcher (advisory). ✅ done (PR #5).** `dispatch_suggest` (headless `claude -p`,
  `--json-schema` → `structured_output`, neutral cwd, 90s hard-kill) → suggestion cards →
  `dispatch_apply` (reserve always; inject only into an idle worktree with a live PTY) → `dispatch_log`;
  drag-to-dispatch board (dnd-kit) with reserve/queue states; Inspector approve + busy→reserve.
  *Exit met:* approve/drag a task and the idle worktree claims and starts it. (Injection submit byte
  resolved in #0019 — the Enter is written after a short delay so the TUI registers it as a keypress
  rather than swallowing a contiguous `text\r` paste burst. Remaining follow-up: prompt/filter tuning
  for empty results.)
- **#0019 — Custom dispatch prompt configuration. ✅ done (PR #15).** Configurable `[dispatch]` steps in
  the project `keitora.toml`, each typed into the target worktree's Claude pane and submitted as its own
  line, with named-placeholder interpolation (`{task_number}` `{task_id}` `{task_title}`
  `{task_description}` `{branch}` `{worktree}`); empty config falls back to the built-in prompt. Folded in
  the submit-byte fix (above). Follow-ups filed: #32 (drag + button double-inject), #33 (`keitora task` on
  PATH so non-keitora projects get session-hook state).
- **#0007 — Unify CLI (`task` → `keitora task`) + PATH-resolvable session hooks. ✅ done (with #33).** The
  standalone `task` binary became the unified `keitora` CLI: task verbs under `keitora task …`, with `session`
  and `config` as siblings. The Tauri app bin was renamed `keitora-app` to free the `keitora` name (bundle stays
  `Keitora.app`). Session hooks now resolve the `keitora` binary per worktree (own build → `keitora.toml [cli].path`
  override → `keitora` on PATH), so hooks install in any project, not just the keitora repo — closing **#33**. The
  app offers to install `keitora` onto PATH on first run. `./task` wrapper → `./keitora`; dispatch prompt,
  permissions, and docs swept. The release build bundles `keitora` as a Tauri sidecar (`keitora-cli`, via a
  release-only `externalBin` overlay) so a packaged `.app` ships the binary for first-run install —
  codesigning/notarizing that sidecar is left to #17.
  **#0005 correction:** the sidecar staging was POSIX-shell-only (`bundle-cli.sh`), so it silently no-op'd on
  Windows and the first Windows installer shipped **no** `keitora-ptyd.exe` — the PTY daemon never started.
  Staging is now a cross-platform Node script (`scripts/stage-sidecars.mjs`, run by `yarn bundle-cli`). Rule:
  **release installers (dmg + nsis) MUST be built with `yarn tauri:bundle`** — a plain `tauri build` skips the
  overlay and ships no sidecars. (Same phase also added the scoped `opener:allow-open-path` capability so the
  **2026-10-06 note:** the `keitora-ptyd` sidecar no longer exists; the bundle ships only `keitora-cli`, and herdr
  is a separate install.
  in-app reveal-log button works.)
- **#0006 — Delete a task from the Inspector. ✅ done (PR #91).** Full-stack hard-delete extending the
  Phase 2 Inspector CRUD. `TaskBackend::delete`: SQLite does a true row-delete (CASCADE drops dep edges +
  log rows; `dispatch_log` is purged explicitly since it has no FK), GitHub tombstones (ensure + apply the
  `keitora:deleted` label, then a best-effort close — hidden from every read path so `get → None`, no
  repo-admin perms; the label is applied first so a failure never leaves a closed-but-visible
  half-tombstone). Refuses deletion when other tasks depend on the target → `Conflict` naming them (no
  silent unblocking, D3); any status is deletable; no audit record (D6). `task_delete` IPC + `keitora task
  delete <id>` (alias `rm`); a two-click `Delete → Confirm delete?` control in the desktop Inspector.
  Universal contract tests cover the delete contract (gone, dependent-block, repeated →
  NotFound, dispatch_log purge) on both backends.
- **#0021 — Notifications fire for non-focused waiting worktrees + app-icon badge. ✅ done (PR #17).**
  Fixed the suppression rule that keyed off pane-rendering (`isPaneVisible` returns true for every pane in
  grid view, so the app-focused grid setup suppressed all banners); it now suppresses only the genuinely
  focused worktree. Broadened the trigger from `waiting`-only to a derived `needsInput` (`waiting` OR a
  latched `busy`/`waiting`→`idle` turn-completion), so a finished turn alerts too. Added a macOS dock badge
  = count of worktrees needing input (cleared at 0) via `getCurrentWindow().setBadgeCount` +
  `core:window:allow-set-badge-count` capability. Consolidated into one `useAttention` hook over pure,
  unit-tested functions in `src/lib/attention.ts` (replaces `notifications.ts`); stood up vitest
  (`attention.test.ts`). Follow-up filed: #36 (worktree creation hardcodes `main` base branch).
- **#0038 — Worktree list → user registry (per-machine). ✅ done (PR #22).** The worktree list was in the
  committed project `keitora.toml` (`[tasks].worktrees`) but is per-machine state, so it churned as a
  perpetual uncommitted diff. Moved authority to the user registry (`ProjectRef.worktrees`,
  `Option<Vec<String>>`: `None` → fall back to the committed list, `Some` → authoritative, even `Some([])`).
  An effective list is stamped onto a `#[serde(skip)]` `ProjectConfig` field via a single
  `stamp_from_registry` choke point on every resolution path (so it can't leak into the committed file
  through `ProjectConfig::write()`), read via a `worktrees()` accessor. Writes route registry-first
  (`set_project_worktrees_by_root`, canonical-root match, updates all matching entries) with a
  committed-file fallback for unregistered projects; `create_worktree` builds from the effective list (lazy
  migration). Committed `[tasks].worktrees` is now a read-only fallback (`#[serde(default)]`); removing it
  from the committed file is a follow-up.
- **#0039 — Notification badge clears + in-app banners for unfocused worktrees. ✅ done (PR #23).** Two
  regressions on top of #0021. (1) The dock badge was stuck: the `idleFromTurn` latch only cleared on
  `→busy`/`→offline`, so a finished worktree badged forever with no dismiss path. Added an acknowledged
  set in `useAttention` (pure `nextAcknowledged` reducer, unit-tested) — the worktree the user views drops
  out of the badge and re-arms on its next turn; suppression and acknowledgement share the
  `focused = appFocused && isWorktreeFocused(...)` predicate. (2) No banners while Keitora was focused: macOS
  routes a foreground app's `UNUserNotificationCenter` notifications silently to Notification Center, so
  `sendNotification` fired invisibly. Banners are now hybrid-routed — an in-app `AttentionBanners` (Radix
  toast, click-to-jump) when Keitora is focused on macOS, the OS notification otherwise (Windows/Linux show
  foreground notifications natively, so they keep the OS path). App-window focus now tracks Tauri's
  `onFocusChanged` via a new `useAppFocus` hook rather than non-reactive `document.hasFocus()`. Frontend
  only; no Rust changes. Deferred follow-ups from code review: banner-click badge-clear TOCTOU,
  single-banner queue, `AttentionBanners`/`Toast` dedup.
- **#0062 — (Re)install session hooks on project activation. ✅ done (PR #45).** Extends Phase 3's
  per-worktree hook install (§6): startup-only install meant switching projects (or onboarding,
  adopting/re-discovering worktrees, or migrating a legacy config) left the now-active project's
  worktrees hookless — empty status lights. Root cause: `install_session_hooks` resolves its worktree
  list via the cwd-based `Config::find()`, which ignores the registry's active-project selection. Added
  `Config::find_active()` (`resolve_active()`-based, hermetically tested) + `install_active_session_hooks`,
  and routed the activation side effects (cache invalidate + off-thread hook install) through a single
  `on_project_activated()` helper used by `set_active_project`/`onboard_project`/`migrate_legacy`
  (`set_worktrees` spawns directly; `remove_project` keeps the bare invalidate). The override `[cli].path`
  is now derived from the resolved config's project so it matches the worktrees installed into.
- **#0003 — Add-project UI + file-less projects + registry-backed settings. ✅ done (PR #88).** The app
  could previously only use the initial `keitora` project. Added a reusable add-project flow (folder pick →
  `onboard_project` → worktree multiselect), surfaced from both first-run onboarding and an on-demand
  dialog. A project no longer REQUIRES a committed `keitora.toml`: a file-less project is stored in the user
  registry and resolves through an `effective_config` choke point over scaffold defaults + per-machine
  `ProjectRef` overrides (`db_path`, `worktrees_dir`, `archive_days`, `log_retention_days`,
  `dispatch_steps`). Per-project settings write registry-first, with a "commit to file" affordance that
  seeds a committed `keitora.toml` on demand. A legacy `task-config.json` is detected read-only and migrated
  only on explicit import (never silently written). **Known follow-up (#0004):** tracked worktrees are
  persisted as names only and their paths are reconstructed from `worktrees_dir`, so a file-less project
  whose worktrees live outside the default dir shows "path not found" — the fix is to store name+path and
  stop reconstructing. **Follow-up fixed (#0018, PR #115):** worktree *creation* still hard-required a
  committed `keitora.toml` — `create_worktree`'s step-7 additive re-read went through
  `effective_worktrees_now` → `ProjectConfig::load`, which errored on a missing file (after `git worktree
  add` had already run), so a registered file-less project could not create worktrees; routed through the
  registry-aware `effective_entries_for`.
- **#0009 — Windows: `keitora` CLI on PATH for spawned terminals. ✅ done (PR #92).** On Windows the
  `keitora-ptyd` daemon spawned each session with its native PATH untouched, so an agent couldn't run
  `keitora task …` — the bundled CLI ships as a sibling `keitora-cli.exe` (a sidecar can't be named `keitora`) in a
  non-user-writable dir, and the CLI installer is Unix-only (Windows install deferred to stage 050). Fix
  (spawn-scoped, no system-PATH mutation): the daemon mirrors the sibling CLI to
  `%LOCALAPPDATA%\keitora\bin\keitora.exe` (once per daemon, refreshed when stale via temp-then-rename) and
  PREPENDS that dir to each spawned child's PATH — augment, never replace, so `cmd.exe`/`claude`
  resolution stays intact. All new code is `#[cfg(windows)]` in `keitora-tasks` (`pick_cli_source`,
  `alias_needs_refresh`, `refresh_alias`, `prepend_path`, memoized `windows_keitora_bin_dir`); the Unix spawn
  path is unchanged. Session hooks already embed an absolute resolved path, so they were unaffected.
  Follow-up: a global `%PATH%` install can later reuse the same `…\keitora\bin` dir (stage 050), routing its
  Windows resolution through `pick_cli_source`.
  **2026-10-06 note:** the daemon is gone; herdr agent panes now get this alias dir prepended to their `PATH` via
  per-pane env instead.
- **#0001 — Purely-visual project switching. ✅ done (PR #96).** Switching between already-visited
  ("warm") projects is now instant — no full-screen overlay, no terminal teardown/remount/re-attach.
  The `keitora-ptyd` daemon was already multi-project (PTYs survive switches); the "heavy" feeling was
  frontend-only. New frontend **warm-project registry** (`src/lib/warmProjects.ts` — pure reducer +
  vitest) keeps every visited project's `TerminalPane`s mounted + streaming, hidden via the existing
  per-pane `active`/`display:none` path; a switch flips visibility (`App.tsx`). Cold first-visit keeps
  the overlay and warms the project on completion; a settled-only effect never warms mid-switch, so
  panes can't key new-project/old-worktree (**#0045 dissolved**). Per-project focus/view memory
  restores `selected`+`view` on switch-back. Decisions: stay-mounted+streaming · unbounded warm set
  (v1) · lazy warming. Verified end-to-end on a release installer; macOS-safe (reuses focus view's
  visibility + WebGL-dispose machinery, so warm panes hold no extra GPU contexts). Code-review fix:
  the optimistic warm flip reverts if the backend rejects the switch.
- **#0002 — Keybind / project-switch rework. ✅ done (PR #97).** Spatial keyboard nav: **Cmd/Ctrl+1..9**
  selects the Nth available worktree (→ Focus), **Cmd/Ctrl+←/→** cycles the active project (the old
  Cmd+1/2/3 view chords are dropped — views switch via the on-screen pill); worktree cycling stays on
  Cmd/Ctrl+↑/↓. Holding the modifier (~300ms) reveals a discoverability overlay — number badges on the
  rail worktrees + a cycle hint on the rail's project header (`src/lib/useModifierHint.ts`, pure reducer + vitest; hint
  persists through chord presses, hides on release/blur). The desktop project **dropdown** was first replaced
  by a tab strip, since superseded by the rail's project groups; both are fed by a shared `useProjectList()` so
  the visible group order and the keymap cycle order share one fetch (no race/divergence). Platform-aware
  modifier label (Ctrl on Win/Linux, ⌘ on macOS). Accepted trade-off: global cycle chords shadow
  terminal word/line nav (Ctrl+←/→) — revisit if it bites. No backend changes.
- **#0004 — Task board scoped to the active project. ✅ done (PR #99).** Follow-up bug from #0001's
  purely-visual switching: terminals/worktrees flipped instantly but the task board lagged or showed another
  project's cards, because task data was global and unscoped — a slow/late `tasks_list` response from a
  project the user switched away from overwrote the active board (no supersession guard), and switch-back
  flashed empty (no per-project cache). Brought tasks to parity with the sessions fix (#0070/#0045):
  `tasks_list` returns `TaskBoard { project, tasks }` stamped with the resolved active project; `useTasks(
  activeProject)` drops responses for a non-active project and seeds from a per-project `TaskCache` for an
  instant correct board on revisit (`shouldSeedFromCache` lifted to `src/lib/projectScope.ts`, shared with
  sessions). Tests: `tasks.test.ts`
  (supersession guard + per-project cache isolation/defensive-copy).
- **#0056 — Rail project groups replace the top-bar tabs. ✅ done (PR #2).** Projects live in the rail as
  multi-expand collapsible groups (peek without switching; open set persisted in `keitora:railExpanded`).
  Collapsed headers show a per-worktree dot strip (focused worktree ringed), needs-input pulse + edge,
  busy/building dot and live count; the active project's header keeps a persistent selected treatment and
  the focused lane an accent bar. `projects_overview` gives every project's worktrees + session states
  (read-only, short-wait DB handle). Clicking a lane in another project silently switches and focuses it;
  its context menu actions run once the switch settles (pending-intent slot). Drag/keyboard reorder persists
  `project_order` (partial orders tolerated; `projects://reordered`), and the keymap cycle follows it.
  Project menu: New worktree, Reveal, Collapse others / Expand all, Remove. Top bar slimmed to 36px with a
  `project / worktree` breadcrumb; `ProjectTabs` removed. User-config read-modify-write serialized behind
  `UserConfig::write_lock()`. `KEITORA_DEV_PROJECTS=N` seeds N throwaway projects for isolated runs, and the
  launcher now refuses to start when herdr resolves a server outside its isolated home (on Windows herdr
  ignores the XDG isolation and reaches the real server). Tests: `railGroups`, `railExpanded`, `railOrder`,
  `pendingIntent`, `railGroupView`, `laneState`, `dev-herdr` (vitest); `project_order`, `projects_overview`,
  `open_read_only`, `write_lock` (Rust). Visual review done in an isolated run; drag/menu edge cases owed.
- **Phase 5 — Polish & packaging. ✅ done (PR #7).** Scrollback search (xterm search addon + Cmd+F
  overlay); central keymap (`src/lib/keybindings.ts`: Cmd+↑/↓ worktree cycle, Cmd+F
  search, Cmd+K dispatch, `?` cheatsheet, `d` board dispatch; nav chords fire over a focused terminal;
  worktree number-jump + project cycle chords added later by #0002);
  native OS notification on idle/busy→`waiting` (`tauri-plugin-notification` + seeded transition hook,
  task-title body, suppressed when the pane is on-screen); env-driven Tauri v2 signed/notarized `.dmg`
  config (`bundle.macOS`, `entitlements.plist`, `scripts/build-dmg.sh`); `docs/windows-backend.md`.
  *Exit met:* installable macOS build (config + script landed; the credentialed signed build itself is
  tracked as #17). (Notification-plugin init is wiring, not new Rust logic, so no new `#[cfg(test)]`.)
- **Phase 6 — Dogfood / self-host (build Keitora with Keitora).** Turn Keitora on its own development. The harness
  is already bootstrapped (done this session): `git init`, the shared task DB at
  `~/code/keitora-worktrees/shared/`, the `keitora-tasks`/`keitora-cli` crates, and the `/workflow` skill at
  `.claude/commands/workflow.md`. Remaining:
  1. Create git worktrees `sayo`, `sol`, `idris` off the `keitora` repo (under `~/code/keitora-worktrees/`).
  2. **Multi-project support in Keitora:** let Keitora point at a chosen `task-config.json`, so the same app can
     drive different projects. Default to whichever config is passed at launch.
  3. Open all three Keitora worktree sessions inside Keitora and use the dispatcher to assign Keitora tasks to
     them — Keitora orchestrating its own construction. *Exit:* a Keitora feature shipped end-to-end using
     Keitora (board + terminals + dispatch), tracked in Keitora's own task DB.

### Future work (beyond Phase 6)

- **GitHub Projects task backend (online tracking). ✅ done (#0097).** A second `TaskBackend`
  implementation (`crates/keitora-tasks/src/github/`) backs task tracking + dispatch with a GitHub
  Projects v2 board instead of local SQLite — selected per-project via `[tasks] backend = "github"`
  + a `[tasks.github]` block. `consistency() → Eventual`. Talks to GitHub over the **`gh` CLI**
  (`GhTransport` seam → `classify` → `retry`), behind a typed `GhApi` so the contract suite runs
  network-free over a stateful `FakeGitHub`. Mapping: task id = issue number; status ↔ project
  Status column (convention + `[tasks.github.status_map]` override); `keitora:<worktree>` ownership
  label; deps + Keitora actor + branch/pr in an HTML-comment metadata block; tags/priority → labels.
  Dispatch_log + reservations + a revision counter live in a **local SQLite sidecar** (GitHub can't
  model machine-local coordination). Landed the documented #0077 trait evolutions
  (`ClaimOutcome::ClaimedUnverified`, `idempotency_key` on `create`, split `newly_unblocked_by`),
  the `Unavailable` error variant + struct `Unauthorized { remediation }`, and structured
  task-command IPC errors so retry/auth hints reach the UI. Setup:
  [docs/github-backend.md](github-backend.md). Worktree sessions
  stay local SQLite regardless of backend.

---

## 10. Risks & open questions

- **Rust/PTY edge cases** — first Tauri+portable-pty integration; budget spike time in Phase 0.
- **xterm performance** at 6 live panes — mitigate with the webgl addon + throttled writes.
- **DB change detection** — SQLite has no native notify; poll `max(updated_at)` / a revision column
  (~500ms–1s) plus a `notify` watch on `tasks.db-wal`. Reuse the same atomic-claim SQL as the CLI — never
  invent a parallel claim path.
- **Injection safety** — strictly gate `dispatch_apply` on `state == 'idle'`; never write to a busy PTY.
- **Restart persistence** — **resolved (task #0040):** the `keitora-ptyd` daemon owns PTYs and outlives the
  GUI, so a relaunch/project-switch reattaches to the live `claude` with scrollback intact (no
  `claude --continue` restart, no scrollback loss). Daemon-crash is the remaining loss window.
  (With herdr, history lives in the herdr server; see the scrollback-model note above.)
- **Windows (fast-follow)** — `portable-pty` already targets ConPTY, so the session layer is shared. Main
  Windows work is path/shell handling and packaging, not a new session backend. Validate before Phase 5.
  See [docs/windows-backend.md](windows-backend.md) for the full Windows port design and open items.
- **Board UI built fresh** — Keitora's task board is built natively against the `keitora-tasks` crate; there's
  no external component to keep in sync, so no cross-repo coupling risk.

---

## 11. Testing

- **Rust:** unit tests for `tasks.rs` mirroring the CLI's atomic-claim/deps behavior (assigned-elsewhere,
  blocked-by-unfinished, cycle rejection); a PTY smoke test (spawn, write, read echo).
- **Frontend:** component tests for the board and suggestion cards; manual matrix for terminal I/O,
  resize, restart re-attach.
- **Integration:** run Keitora alongside the existing CLI and confirm a claim in one is reflected in the other.

---

## 12. Orchestration harness — built-in Workflow tool vs the `/workflow` skill

Keitora's whole point is orchestrating Claude work, and we'll dogfood it (§9 Phase 6). Two orchestration
mechanisms are on the table; they operate at **different layers** and are complementary, not competing.

### A. Claude's built-in **Workflow tool** (code-defined fan-out engine)
A JavaScript script with deterministic control flow (loops, conditionals, `parallel`/`pipeline`) that
spawns many subagents (≤16 concurrent, ≤1000 total), runs in the background, and returns a synthesized
result. Supports structured-output schemas, adversarial-verify / judge-panel / loop-until-dry patterns,
and resume via run id.
- **Strength:** *breadth* — audits, migrations, multi-perspective review, research sweeps; anything that
  wins from parallelism and programmatic structure.
- **Shape:** one invocation → one orchestrated fan-out → a result. Ephemeral; not a long-lived process.
- **Weak spots for our use:** no built-in notion of the task DB, git/PR lifecycle, or cross-session state.

### B. A **`/workflow` skill** (prose-defined delivery lifecycle)
A markdown slash-command that drives a **stateful, human-gated** feature pipeline: plan → refine →
fracture → singletonize → loss-check → sequence → implement → test → stage → review → finalize (the
quality stages are skippable per a complexity assessment). State persists on disk in `WORKFLOW_STATE.json`
(resumable across days/sessions); each stage transition is human-approved; it's wired into the task DB
(claim/done) and branch/PR conventions, and uses sub-agents with explicit **per-stage model routing** for
cost control. Keitora's pipeline lives at `.claude/commands/workflow.md`.
- **Strength:** *depth + lifecycle* — ship one well-specified feature correctly, end-to-end, with
  documentation discipline and a human gate.
- **Shape:** a long-lived, mostly-linear process spanning many sessions.

### Side by side
| Axis | Built-in Workflow tool | `/workflow` skill |
|---|---|---|
| Defined in | JavaScript (code) | Markdown prompt (prose) |
| Control flow | Deterministic, programmatic | Model-interpreted, heuristic |
| Concurrency | Massive fan-out | Mostly linear; some per-stage parallelism |
| State / duration | One background run; resume within session | On-disk `WORKFLOW_STATE.json`; spans sessions |
| Human involvement | Fire-and-collect | Gated at every stage + review gate |
| Output | Structured data / synthesis | Merged PR + updated docs + closed task |
| Integration | Generic (agents can call anything) | Task DB, git/PR, design docs, backups |
| Cost control | Token-heavy; explicit opt-in | Per-stage model routing |
| Failure handling | `filter(Boolean)`, adversarial verify | retry stage ×3, escalate, state rollback |
| Best for | **Breadth** (audit/migrate/review/research) | **Depth** (feature idea → merge) |

### How they compose (the actual recommendation)
They nest. The `/workflow` skill is the **spine** (stateful, gated SDLC tied to the task DB and PRs); the
built-in Workflow tool is the **muscle** for the wide steps *inside* that spine — e.g. fan-out reviewers
at the test/review stage, parallel multi-file edits at implement, adversarial verification at loss-check.

**For Keitora dogfooding (Phase 6):** the `/workflow` skill at `.claude/commands/workflow.md` is the
spine (full staged pipeline, task-DB + branch conventions); call the built-in Workflow tool
for parallelizable sub-steps (and as a natural fit for the dispatcher's proposal generation). Building
Keitora this way gives a real head-to-head evaluation of both mechanisms on the same project.
