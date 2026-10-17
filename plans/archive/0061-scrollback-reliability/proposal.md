# 0061 — Terminal scrollback reliability

## Goal

Make the mouse wheel scroll terminal history reliably in every pane, and give
panes deep history (≈10k lines), without growing webview memory when 10–15
panes are open. Also explain (and fix, where it is ours) history loss when
returning from a Claude Code sub-view.

## Problem (root cause)

Both symptoms come from one architectural fact: **Keitora's xterm never sees a
scrolling byte stream.** The herdr bridge (`src-tauri/src/backend/herdr/bridge.rs`)
runs `herdr terminal session control <id> --takeover`, whose stdout is
`terminal.frame` lines: `{seq, encoding, width, height, full, bytes}` — base64
ANSI *renders of herdr's viewport* (full redraw or incremental diff). Rows are
painted in place; nothing scrolls off xterm's top, so xterm's `scrollback: 5000`
(`src/components/TerminalPane.tsx:811`) stays essentially empty. The real
history lives in herdr's libghostty screen, capped by `scrollback_limit_bytes`
(`backend/herdr/env.rs:142`, default 4 MB). Keitora never scrolls it.

- **"Very little scrollback, even live"** — xterm's buffer only holds what the
  current frame drew. herdr's history is unreachable from the UI. (A second
  suspect: herdr's limit is libghostty *page memory*, not ANSI bytes. At wide
  pane widths, 4 MB may hold only a few thousand lines. Measure it, see Spike.)
- **"Wheel does nothing"** — in xterm's normal buffer the wheel scrolls xterm's
  own (empty) history; otherwise xterm turns it into arrow keys sent as
  `terminal.input`. The `onWheelNormalize` capture listener
  (`TerminalPane.tsx:996-1050`) only reshapes the alt-buffer pixel path. No path
  ever sends herdr a scroll command, so herdr's history and an agent TUI's own
  scrolling are both unreachable.

## What herdr already provides (verified, herdr v0.9.3 source)

Source: `github.com/ogulcancelik/herdr` @ `v0.9.3` — `src/client/terminal_sessions.rs`,
`src/server/pane_input.rs`, `src/pane/terminal.rs`, `docs/.../socket-api.mdx`.

- **`terminal.scroll`** on the control stdin:
  `{"type":"terminal.scroll","direction":"up"|"down","lines":N,"source":"wheel"|"page_key","column":C,"row":R,"modifiers":M}`.
  `direction` and `lines` (u16, > 0) are required. `source` defaults to `wheel`.
  `column`/`row` are 0-based cells (default 0,0). `modifiers` is a u8 bitmask.
  Errors are only `warn!`-logged server-side; nothing comes back on stdout.
- **herdr routes the wheel itself** (`wheel_routing()`, `terminal.rs:1961-1982`),
  exactly like tmux:
  1. app has mouse reporting on → encoded wheel report to the app at
     (`column`,`row`) — e.g. an agent TUI scrolling its own transcript;
  2. else alt screen + alternate-scroll mode → arrow keys to the app;
  3. else → herdr scrolls its own history.
  So Keitora does not need to know the app's mouse mode. It always sends
  `terminal.scroll`.
- **Snap-back:** every `terminal.input` calls `scroll_reset()` first, so typing
  returns to the live tail for free.
- **Scroll position:** not in frames. Readable as `PaneInfo.scroll =
  {offset_from_bottom, max_offset_from_bottom, viewport_rows}` (socket API),
  plus a `pane.scroll_changed` subscription. Keitora has no subscription plumbing
  today.
- The viewport is **shared** across clients of a terminal (one client in practice).
- **Config:** `herdr server reload-config` applies most settings live; the
  scrollback limit is read at pane creation, so existing panes keep their old cap.
- **No special handling of `ESC[3J`** in herdr; libghostty most likely erases
  history on it.

## Approach

**herdr owns history; xterm is a viewport.** Every wheel tick over a pane
becomes a `terminal.scroll` on that pane's bridge. herdr decides where it goes
(app or history) and streams the result back as ordinary frames. This fits the
10–15 pane constraint: history stays in the herdr daemon, bounded per pane and
allocated lazily, never loaded into the webview. Only the visible viewport is
ever rendered, so an idle pane's history costs the UI nothing, and nothing has
to be loaded or unloaded on switch.

## Scope

### In
1. **Scroll command path (backend).** `PtyBackend::scroll(key, direction,
   lines, column, row)` with a default no-op body (like `repaint`), so
   `FakeBackend`/`UnavailableBackend` need no change. HerdrBackend enqueues a new
   `WriteJob::Scroll` on the per-session writer (`writer.rs`). It does not
   write stdin directly, so it keeps the writer's ordering and stall watchdog.
   Consecutive same-direction scroll jobs coalesce (sum `lines`, cap at u16),
   mirroring how `Resize` coalesces. `bridge::scroll_line` + `send_scroll_to`
   build and write the NDJSON. **A failed scroll write is dropped and logged.
   It must not restart the bridge** (unlike resize). `drop_bridge` happens only
   if stdin is dead. New `#[tauri::command(async)] pty_scroll` in `host.rs`
   (registered in the `invoke_handler` list at `host.rs:2073`), `ptyScroll` in
   `src/lib/ipc.ts`.
2. **Wheel handling (frontend).** Replace the `onWheelNormalize` capture
   listener with `term.attachCustomWheelEventHandler` returning `false`
   (xterm does neither viewport scroll nor arrow emulation). Convert
   pixel/line/page deltas to whole lines. Extend `accumulateWheelLines`
   (`src/lib/termFit.ts:55`, pixel-only today) to handle line and page modes. Map
   the pointer to a 0-based cell for `column`/`row`. Coalesce per animation frame
   and send one `ptyScroll` per frame. Keep the existing guard for malformed or
   extreme deltas.
3. **Keyboard scroll.** Shift+PgUp / Shift+PgDn send `terminal.scroll` of
   `rows - 1` lines (`source: "wheel"`, same routing). Registered in
   `src/lib/keybindings.ts`.
4. **Shrink xterm's own history.** `scrollback: 5000` → a small value (≈100). It
   holds nothing useful, and this cuts per-pane webview memory across many panes.
   Remove or neutralise the now-meaningless `isAtBottom` sticky-tail logic
   (`TerminalPane.tsx:307-345`). Cmd+F keeps searching the visible viewport only.
   That is already effectively true today, and is documented as a limitation.
5. **History depth.** Pick a new `DEFAULT_HERDR_SCROLLBACK_LIMIT_BYTES` from the
   spike's lines-per-MB measurement, targeting ≈10k lines at a typical wide pane.
   It fits the existing clamp range (`env.rs:145`, up to 100 MB); hard ceiling 20 MB.
   The `[pty] herdr_scrollback_limit_bytes` override stays. Update the env tests
   that assert the literal `4000000` (`env.rs:1916`, `:1949`). It applies to panes
   created after the herdr server restarts (existing comment at `env.rs:171`).
   Release notes say so.
6. **"↓ live" indicator.** While the focused pane is scrolled into history, show
   a small pill; click or End sends `scroll down` by `offset_from_bottom`. Source
   of truth: a debounced (~150 ms after the last scroll) read of the pane's
   `PaneInfo.scroll` through `HerdrOps`. It is authoritative, and correctly stays
   hidden when the wheel went to a mouse-mode app. It is cleared on input (herdr
   snaps back). This is the last stage and can be dropped.
7. **History lost on Claude Code sub-view return.** Opening a Claude Code
   sub-view (e.g. a dynamic-workflow view) and pressing ← often leaves the input
   box at the *top* of an empty pane. Keitora never resets xterm on view changes
   (only `restart`/`launch` call `term.clear()`). The leading hypothesis is
   upstream: Claude Code emits `ESC[2J`/`ESC[3J` and repaints only its live
   region, and libghostty erases history on `ESC[3J`. Settle it in the spike,
   then:
   - **ours** (e.g. a refit-triggered resize or full frame landing mid-switch):
     fix in this work;
   - **herdr** (clear-scrollback handling): file or record a follow-up task (herdr
     patches stay deferred);
   - **Claude Code**: document, and note that scrolling via herdr won't recover
     erased history.
8. **Diagnostics.** `keitora:debug-wheel` logs every wheel event: delta mode,
   computed lines, cell position, and the command sent. Replaces the alt-only log.
9. **Tests (no test touches a real herdr).**
   - Rust: `scroll_line` JSON shape, writer coalescing of scroll jobs, the
     drop-not-restart failure path, and an end-to-end scroll through `FakeHerdr`
     asserting the stdin line (cf. `resize_writes_a_resize_line…`, `mod.rs:3372`).
     Also the default-limit update.
   - Frontend (`yarn test`): pixel/line/page → lines accumulation, direction
     sign, cell mapping, `ptyScroll` ipc wrapper (`ipc.test.ts` pattern).

### Out
- Searching full history (Cmd+F over herdr's history). Follow-up: on-demand
  `pane read --source recent --lines N` into a search-only buffer.
- Raw-byte streaming so xterm builds history natively (bridge redesign).
- `pane.scroll_changed` subscription plumbing. Polling after scroll is enough.
- Patching herdr / libghostty (`ESC[3J` behaviour).

## Spike (first implementation stage)

The protocol and routing are settled by source. What remains is behavioural,
measured on real panes:
1. **Lines per MB.** Read `PaneInfo.scroll.max_offset_from_bottom` and the pane
   width for a few long-lived panes, to size the default limit. This is a
   read-only query of the running herdr, done only with the user's OK; otherwise
   use an isolated herdr instance.
2. **Claude Code sub-view capture.** In an isolated herdr running Claude Code:
   open a workflow view, press ←, and record the pane output for `ESC[?1049h/l`,
   `ESC[2J`, `ESC[3J` and `ESC[H`. Compare `max_offset_from_bottom` before and after.
   Also note whether Claude Code enables mouse reporting (that decides whether the
   wheel scrolls herdr history or Claude Code's own view).
3. Confirm Codex's mode (mouse reporting / alt screen) for the same reason.

The isolated herdr must be truly isolated: the Windows `dev:isolated` launcher
is known to adopt and stop the real daemon, so don't use it.

## Dependencies

herdr ≥ 0.9.3 control protocol (`terminal.scroll`). No new crates or npm packages.

## Risks

- **Undocumented protocol.** `terminal.scroll` is not in herdr's user docs. It is
  verified against the v0.9.3 source and pinned by builder tests. Unknown fields
  are ignored server-side, so additive changes are safe.
- **App-captured wheel.** If Claude Code enables mouse reporting, the wheel
  scrolls Claude Code's own view, not herdr history. That is correct terminal
  behaviour, but the user should know which they get. The spike tells us.
- **Memory.** A bigger limit multiplies across panes in the daemon (15 × N MB
  worst case). It is allocated only as history grows, outside the webview. The
  daemon has OOM history, so the 20 MB ceiling is deliberate.
- **Erased history isn't recoverable.** If an app sends `ESC[3J`, history is gone
  in herdr. This work can't bring it back.
- **Trackpad flood.** Mitigated by per-frame (frontend) and writer-side coalescing.
- **Selection while scrolled.** xterm selection is over the painted viewport, and
  a repaint during a drag may reset it. Acceptable; checked in the smoke test.
