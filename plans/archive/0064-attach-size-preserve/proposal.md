# 0064 — Attach never shrinks a session to a guessed size

## Goal

Stop Claude Code's input box from getting "pinned to the top" of a pane. The live
screen should never be left with history pushed above it and blank rows below just
because Keitora attached before it knew the pane's real size.

## Symptom

Sometimes the Claude Code input box sits at the top of the pane with blank rows below.
Wheel-scrolling up puts it back at the bottom, but the next keystroke snaps it to the top
again. A forced window resize fixes it until the next time.

## Root cause

1. **Attach takes over at an unmeasured size.** The `Terminal` is built with no
   rows/cols (`TerminalPane.tsx` mount effect), so it starts at xterm's **default 80×24**.
   `doAttach` calls `ptyAttach(projectName, name, term.rows, term.cols, …)` *before*
   `term.open` + `refitAndResize`. Those run only after the attach resolves. So **every**
   attach (hidden or visible, at boot, reconnect or recovery) takes over at 80×24, and
   the real fit lands a few frames later. `attachAfterSpawn` and `spawn`
   (`sessionLaunch` / `ptySpawn`, plus the `ptyResize` right after) do the same.
   `restart()` / `launch()` on a never-shown pane open the terminal without a fit and
   spawn at 80×24 as well. `HerdrBackend::attach` (`src-tauri/src/backend/herdr/mod.rs`)
   passes the size to `start_bridge`, which runs `herdr terminal session control <id>
   --takeover --cols N --rows N` (`real.rs`). herdr resizes the pane to the controller's
   size.
2. **Shrink, then grow, leaves the screen wrong.** When ghostty's screen shrinks, its
   top rows go into history. When it grows back and the cursor is not on the last row
   (Claude Code's cursor is in the input box, above its footer), ghostty adds blank rows
   at the bottom and does *not* pull history back down. This is in the herdr source
   (v0.9.3), `PageList.zig` resize: about lines 2829–2867.
3. **Claude Code redraws relative to the cursor.** Its renderer only rewrites the bottom
   "dynamic" region with cursor-up/erase-line, so the input box stays where the cursor
   is, near the top, until a full clear-and-reprint. A real resize (SIGWINCH) triggers
   one, which is why resizing the window fixes it.
4. **Typing "snaps back" by design.** herdr resets the history scroll offset on input
   (`pane_input.rs` `scroll_reset`). The live viewport is the thing that's wrong; the
   scroll is behaving correctly. Nothing is "fighting".

The frame encoding (`render_ansi.rs`) was ruled out: diffs are always taken against the
frame actually sent, and frame size always equals pane size. The Windows
recent-output fallback never feeds rendering.

## Scope

**In**
- No takeover, spawn or resize ever sends herdr a size the frontend never measured.
- Visible panes are opened and fitted **before** they attach, so they take over at their
  real size.
- The backend picks the takeover size when the frontend has none: the remembered size,
  else the pane's current rows from herdr.
- Boot resume uses the remembered size before the `RESUME_COLS/ROWS` constant.
- Rust unit tests (fake herdr) and frontend unit tests for the new rules.

**Out**
- Recovering a session that's already in the bad state. A real resize or the next full
  Claude Code redraw fixes it; no "force redraw" button.
- ghostty's grow-without-pulling-history behaviour (upstream, and correct for shells).
- The ESC[3J history loss already tracked separately.
- Persisting sizes across Keitora restarts (the herdr pane's own rows cover cold boot).

## Architecture

The frontend reports what it **knows**, and the backend decides what size a takeover
uses:

```
frontend: measured? ── yes ─▶ attach(Some(cols, rows))
                     └─ no ──▶ attach(None)              ("keep the pane's size")
                                        │
backend:  size = measured
              ?? last_size[key]                          (any committed size this run)
              ?? (pane rows from pane_list, cols from DEFAULT)
              ?? DEFAULT (80×24, today's behaviour)
```

## Technical Design

### 1. Frontend: measure before attaching, never send a guess
- **Open + fit before attach.** In `doAttach`, when the host is measurable
  (`clientWidth/Height > 0`), call `term.open(host)` first (if not already open) and take
  a size from `fit.proposeDimensions()` (it needs `term.element`, so open first). If it's
  sane, `fit.fit()` and attach with that size. The existing "write scrollback before
  showing" order still holds: the scrollback is written after the attach resolves,
  into a terminal that's already sized. If the proposal isn't sane yet (renderer cell
  metrics still 0), attach with `None` and let the post-attach `refitAndResize` send
  the real size.
- **A "measured" flag** per `Terminal` (a ref), set when a sane size has been committed
  by `refitAndResize`'s `commit` or by the pre-attach fit. Hidden panes (host 0×0)
  stay unmeasured until first shown.
- `ptyAttach`, `ptySpawn` and `sessionLaunch` send `rows/cols` only when measured, and
  `null` otherwise. This covers `attachAfterSpawn`, `spawn`, `restart()` and
  `launch()`.
- The `ptyResize` right after a spawn is skipped when unmeasured.
- **`refitAndResize` give-up path** (`MAX_FIT_FRAMES`): commit the loop's `lastSane`
  proposal. If there was none, commit nothing rather than shipping `term.rows/cols`,
  which may still be the default.
- Put the "which size do we send" decision in a pure helper in `src/lib/termFit.ts`
  (e.g. `measuredSize(proposed, measuredFlag) → Dims | null`) so vitest covers it.

### 2. IPC: optional size
- `src/lib/ipc.ts`: `ptyAttach`, `ptySpawn` and `sessionLaunch` take `rows/cols` as
  `number | null`. `ptyResize` stays required.
- `src-tauri/src/host.rs`: `pty_attach`, `pty_spawn`, `session_launch` (and `spawn_pty`)
  take `rows: Option<u16>, cols: Option<u16>`. This follows the existing optional args
  (`fresh: Option<bool>`, `attach_id: Option<u64>`, `gen: Option<u64>`). They collapse to
  `Option<(u16, u16)>`, which is `None` unless both are present and ≥ 1.
- The `PtyBackend` trait (`src-tauri/src/backend/mod.rs`) carries `size: Option<(u16,
  u16)>` on `attach` / `spawn` / `spawn_agent`. That needs updating in the three
  implementors: `HerdrBackend`, `backend/fake.rs` `FakeBackend` (and its recorded
  `Call::Spawn/SpawnAgent/Attach`), and `backend/unavailable.rs`. `resize` keeps
  `u16, u16`.
- **No 0×0 sentinel.** `.max(1)` already turns 0 into a 1×1 takeover, which is far
  worse than 80×24.

### 3. Backend: remember sizes per session
- `HerdrBackend` gets `last_size: Mutex<HashMap<SessionKey, (u16, u16)>>`. It's recorded
  from every measured size: `resize` (**before** its "no bridge → no-op" early return,
  so a resize that races an attach is still remembered), a measured `attach`, and a
  measured `spawn` / `spawn_agent`. Stale entries are simply overwritten by the next
  measured size, so there's no cleanup on kill.
- A helper `takeover_size(key, requested, pane_rows) -> (u16, u16)` implements the
  chain in the Architecture section. It's used by:
  - `attach` → `start_bridge` (pane rows come from the `pane_list` result `attach`
    already fetches, so there's no extra herdr call),
  - `spawn` / `spawn_agent_locked` (only matters when a live attach restarts the
    bridge; `workspace_create` takes no size),
  - `resume_locked`: `last_size[key]` before `RESUME_COLS/ROWS`.
- The writer's `restart_bridge` / repaint paths take their size from the bridge or
  the resize being sent, never from the frontend. They're unchanged.

### 4. Parse the pane's rows
- `src-tauri/src/backend/herdr/cli.rs`: `PaneIdentity` (alias `PaneInfo`) gains
  `viewport_rows: Option<u16>`, parsed from `scroll.viewport_rows` in
  `parse_pane_identity`. herdr's `pane list` builds each entry with `pane_info`, which
  fills `scroll` (herdr `src/app/creation.rs` `collect_panes_for_workspace` →
  `pane_info`), so `pane_list` carries it. It's absent when the runtime has no metrics,
  hence the `Option`.
- Update the `PaneIdentity` literals and `herdr/fake.rs` (`pane_get` / `pane_list`) so
  the fake can report rows.

### 5. Tests
- **Rust (`herdr/mod.rs`, `fake::FakeHerdr`):**
  - an attach with `None` after a `resize` takes over at the resized size;
  - an attach with `None` and nothing remembered takes over with the pane's
    `viewport_rows` and default cols;
  - an attach with `None` and no rows anywhere falls back to 80×24;
  - a measured attach records the size;
  - a `resize` with no bridge still records it;
  - `resume_locked` uses `last_size` over `RESUME_*`.
- **`cli.rs`:** `viewport_rows` parses; it's `None` when `scroll` is missing.
- **`host.rs`:** the `Option` collapse (one of the two missing → `None`; 0 → `None`).
- **Frontend (vitest):** the `termFit.ts` helper; `ipc.test.ts` sends `null` sizes
  through.
- Existing tests that pass `80, 24` (about 60 in `herdr/mod.rs`) become `Some((80, 24))`.
  That's mechanical.

## Dependencies
- herdr v0.9.3 `terminal session control --takeover --cols --rows`. Keitora always
  sends a size there, so the takeover must be given a good one.
- `pane list` → `scroll.viewport_rows` (herdr API schema `PaneScrollInfo`).

## Risks
- **Cols on cold boot** are a guess (80) when the frontend can't measure. A cols
  mismatch reflows without pushing rows into history, and the first fit corrects it.
  That's acceptable, and it only affects hidden panes at a cold boot.
- **Open-before-attach** changes the mount order: the terminal is visible (empty)
  while the attach is in flight. The existing "attaching" overlay/spinner covers it;
  check that the first paint shows no flash.
- **Trait signature churn** touches many test call sites; it's mechanical, but keep it
  in its own commit.
- **Live verification:** the user's machine runs a real herdr; the smoke test must
  follow the "don't disturb the running herdr" rule. Unit tests with the fake herdr
  carry correctness; the live check is a manual step.

## Resolved questions
1. "Unknown size" in IPC is `Option` (`null` from TS), **not** a 0×0 sentinel.
2. Cold boot uses the herdr pane's own rows from `pane_list`, after the in-memory
   `last_size`. There's no shared "last measured pane size" in the frontend: grid view
   shows panes of different sizes. No persistence.
3. An unmeasured spawn doesn't wait for a fit: it uses the fallback chain, and the first
   fit sends the real size.

## Complexity
**Small/medium**, one subsystem (pane sizing): `TerminalPane.tsx`, `termFit.ts`
(+test), `ipc.ts` (+test), `host.rs`, `backend/mod.rs`, `backend/fake.rs`,
`backend/unavailable.rs`, `herdr/mod.rs`, `herdr/cli.rs`, `herdr/fake.rs`. Skip
Fracture, Singletonize and Loss check; the plan stays one document.
