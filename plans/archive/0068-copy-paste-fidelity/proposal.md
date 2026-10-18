# 0068 — Terminal copy/paste fidelity

## Goal

1. **Paste:** a paste of any size into a Keitora pane (especially Claude Code) arrives
   whole, as one paste — never just its tail, never as a stream of Enter keys.
2. **Copy:** copying a selection yields the session's text, not one hard line per screen
   row (soft-wrapped lines are rejoined).

## Symptoms (reported, Windows)

- Pasting a ~25-line block into Claude Code left only the last ~2 lines in the input box;
  a ~40-line block kept only the text after the final `:` of a mid-block line. The
  surviving tail is contiguous and its length varies per paste.
- Copied text has line breaks at screen-row boundaries; sometimes doubled newlines.

## Root causes

### Paste — pastes are never bracketed

- Output reaches xterm only as herdr `terminal.frame` viewport paints
  (`src-tauri/src/backend/herdr/bridge.rs`). herdr's ANSI encoder emits `?2026`/`?25`,
  never `?2004h`, so **xterm never enters bracketed-paste mode** even though Claude Code
  enabled it on the real pane.
- So `term.paste(text)` sends `line1\rline2\r…` with no `ESC[200~ … ESC[201~` wrapper
  (xterm `Clipboard.ts` `paste` brackets only when its own `bracketedPasteMode` is set).
  herdr's attach input path (`server/pane_input.rs` `apply_terminal_attach_input`) treats a
  message as a paste only when it is **exactly one complete bracketed block**
  (`raw_input.rs` `complete_text_bracketed_paste`: starts with `ESC[200~`, first `ESC[201~`
  is the very end, valid UTF-8); otherwise it writes the raw bytes to the pane.
- Claude Code then sees a large burst of typed input split across several console reads,
  each `\r` an Enter. Its paste heuristics treat bursts separately and the last one wins —
  hence "only the second half".
- The comments in `TerminalPane.tsx` (around `pasteClipboard`, the Ctrl+V branch of the key
  handler, and the file-drop effect) claiming `term.paste()` "respects bracketed-paste
  mode" are false in this architecture; `input.rs` lists bracketed paste as deferred.
- Secondary: the writer's text coalescing (`writer.rs` `Run::absorb` / `absorb_into`) can
  glue neighbouring keystrokes onto a bracketed block when the writer is backed up, which
  would defeat herdr's "exactly one block" detection. The CLI fallback
  (`WriterIo::fallback` in `mod.rs` → `input.rs` `translate_input`) turns `ESC[200~` into
  `esc` + literal `[200~` and every `\r` into an `enter` key.
- Every call site that pastes today goes through `term.paste()`: clipboard text, clipboard
  image path, and file drop (three sites in `TerminalPane.tsx`). On **non-Windows** there is
  a fourth, implicit path: the native `paste` DOM event reaches xterm's own handler
  (`handlePasteEvent` → `paste`), which is also unbracketed. Today's native-paste
  suppression is Windows-only, so macOS/Linux paste (Cmd/Ctrl+V, middle-click, context menu)
  has the same bug and must be covered.

### How herdr treats a complete block (verified in 0.9.3 source)

- `try_send_paste` (`pane.rs`) first runs `prepare_paste_text_for_pty` (Windows only:
  `\r\n`→`\n` then `\n`→`\r\n`; a lone `\r` is left alone; no-op elsewhere), then wraps in
  `ESC[200~ … ESC[201~` **only if the pane's real bracketed-paste mode is on**, else sends
  the bare text. That mode is read from herdr's own terminal emulator, which is fed by the
  real child's output — so it reflects what Claude Code / the shell actually enabled, not
  anything xterm in Keitora knows.
- Consequence: sending a complete block to a pane whose app never enabled `?2004`
  (plain `cmd.exe`, many PowerShell setups) is safe — the markers are stripped and the text
  arrives bare, so embedded line breaks act as Enter exactly as typed text would. That is
  the correct behaviour for a shell.
- herdr rejects any single input message above 1 MiB (`client_transport.rs`
  `MAX_INPUT_PAYLOAD`): an oversized complete paste is rejected with an error to the client,
  but an oversized non-paste message **disconnects the client**.
- `herdr pane send-text` (the CLI fallback target) is **not** a paste: the handler writes the
  text as raw bytes with no bracketing and no newline handling. `pane run` appends Enter,
  `agent prompt` submits; the CLI exposes nothing that sends an unsubmitted, mode-aware
  paste.

### Copy — herdr frames carry no soft-wrap information

- xterm runs with `scrollback: 0` and only ever holds herdr's painted viewport. A repaint
  positions each row explicitly, so `BufferLine.isWrapped` is never set and xterm's
  `getSelection()` (`SelectionService.selectionText`) emits a separator after every row.
- The separator is `\r\n` on Windows, `\n` elsewhere. xterm builds each row with
  `translateBufferLineToString(trimRight=true)`; whether herdr paints trailing blanks as
  erased cells or as literal spaces was not verified, so the new builder trims trailing
  spaces itself rather than relying on cell emptiness.
- Claude Code also hard-wraps its own prose to the pane width; those breaks are real
  newlines in its output and no terminal-side fix undoes them. Blank rows between
  paragraphs are likewise real screen rows. Because blank rows are kept exactly (decision
  1), **this change will not remove "doubled" blank lines that are genuine blank screen
  rows**; the doubled-newline symptom's exact cause was not isolated. The smoke test must
  check it, and if it persists the follow-up is a collapse option, not part of this task.
- Copy flows through two paths that must both change: the explicit Ctrl+C handler
  (Windows only, `clipboardActionForKey`) and xterm's native `copy` DOM listener, which is
  bound on `term.element` in the bubble phase (`Terminal.ts` `_initGlobal`) and is the only
  copy path off Windows (Cmd/Ctrl+C, context-menu copy). On Windows both fire for one Ctrl+C
  unless the keydown is `preventDefault`ed.

## Scope

**In**
- **Paste pipeline, one entry point.** A frontend `sendPaste(text)` used by clipboard text
  paste, clipboard image-path paste, file drop, and the native paste event on non-Windows.
  It applies the same gate as typing (`inputBlocked(statusRef.current)`), snaps the live-tail
  pill (`liveTailRef.noteInput()`), normalises newlines (`\r\n`/`\n` → `\r`, xterm parity;
  herdr leaves a lone `\r` untouched on Windows), and calls a new `ptyPaste` IPC. An empty
  paste is a no-op. The frontend does **not** add markers.
- **Backend owns bracketing.** A dedicated paste job (not marker-sniffing inside
  `pty_write`) so a typed/injected chunk can never be mistaken for a paste and so the
  coalescer never sees it as text. The backend wraps the payload in `ESC[200~ … ESC[201~`
  after removing any embedded `ESC[200~`/`ESC[201~` (an embedded end marker would break
  herdr's "exactly one block" test and let the tail run as keystrokes), and enforces the size
  caps below.
- **Writer:** the paste job is never merged with neighbours in either direction; ordering
  with typed keys is preserved (same queue).
- **CLI fallback:** sends the bracketed block as one `pane send-text` with real ESC bytes
  (raw bytes reach the pane; this is correct for Claude Code and other `?2004` apps, and is
  the best the 0.9.3 CLI allows). Known limitation: if the pane's app never enabled `?2004`,
  the markers show up as stray characters. Accepted because the fallback only runs when no
  bridge came up within the writer's wait, and the primary target is bracketed agents;
  noted in Risks. Windows newline preparation is not applied by `send-text`, so the text
  keeps the frontend's `\r` separators as is.
- **Size caps.** Bridge path: refuse a paste whose payload exceeds 1,000,000 bytes (below
  herdr's 1 MiB limit) and log. Fallback path: 24 KB, measured as UTF-16 units of the
  payload plus one extra unit per `"` and per `\` (the worst-case argv quoting expansion),
  so ordinary text hits 24 KB and quote-heavy text cannot blow past the Windows
  `CreateProcess` 32,767-unit command line. Above either cap: refuse and log, never send a
  partial paste. Surface the refusal to the user if an existing toast mechanism fits
  (see Open questions).
- **Native paste handling on every platform.** Replace the Windows-only suppression with a
  capture-phase listener on the host that, on all platforms, stops xterm's handlers. On
  Windows it stays a pure suppressor (the explicit Ctrl+V path owns paste; the native event
  is a duplicate and cannot be told apart from a context-menu paste, which stays
  unsupported as today). Off Windows it reads `clipboardData` text synchronously and feeds
  `sendPaste`; image paste off Windows stays out of scope (it never worked there).
  `ignoreBracketedPasteMode` is not needed and not set: nothing calls `term.paste()` any
  more, and xterm's own bracketed mode stays off, so there is no double-wrap if herdr ever
  starts forwarding `?2004`.
- **Image-path paste and file drop are bracketed pastes too** (a real terminal delivers a
  dropped path as a paste, and Claude Code's image-path detection keys off the paste event).
  The existing trailing space stays inside the block; confirm in the smoke test that
  Claude Code still attaches the image.
- **Copy text builder** over the xterm buffer, used by both copy paths. Rules:
  - A row joins onto the next when `isWrapped` is set on the next row, **or** it is "filled":
    its last cell holds a non-blank glyph (a wide glyph spanning the last two cells counts),
    or its last cell is empty because a wide glyph wrapped (next row's first cell has
    width 2 — the case `bufferRowText`'s `joinsNext` already handles).
  - **Never join onto a blank next row** (that would swallow a blank line and violate
    decision 1), and never join across rows outside the selection.
  - Do not treat a row as filled when its last glyph is a box-drawing/block character
    (U+2500–U+259F): Claude Code's full-width separator lines and bordered panels would
    otherwise glue the next row onto them.
  - Each logical line is right-trimmed of spaces; non-breaking spaces become spaces (xterm
    parity); partial first/last rows are cut by cell column (a wide glyph is included when
    either of its cells is in range).
  - Line separator follows platform convention (`\r\n` on Windows, `\n` elsewhere), as
    xterm's own text does today, passed in as a parameter so tests are platform-free.
  - Blank rows are kept exactly as on screen (no collapsing).
- **Selection source:** `term.getSelectionPosition()` (ordered, 0-based; `end.x` exclusive;
  with `scrollback: 0` the `y` values are viewport rows, which is also how
  `TerminalSearch.tsx` uses it). Alt+drag makes xterm select a rectangular block, which the
  public API does not expose; track an Alt-held mousedown on the host (non-macOS) and fall
  back to xterm's own `getSelection()` for that selection.
- **Wiring:** the Ctrl+C branch uses the builder and now also `preventDefault`s the keydown
  (so the native copy event does not fire a second time); an empty result still falls
  through so xterm sends `\x03` (SIGINT). A capture-phase `copy` listener on the host (an
  ancestor of `term.element`) sets `clipboardData` to the builder's text, `preventDefault`s
  and `stopPropagation`s so xterm's bubble-phase listener never runs; it does nothing when
  `!term.hasSelection()`.
- Fix the misleading comments listed under Root causes, and the stale `ptyWrite`
  references in `terminalFileDrop.ts`' header.
- Update `terminalClipboard.ts` (`suppressNativePaste` semantics) and its test.

**Out**
- Undoing Claude Code's own hard wrapping of prose (not recoverable at the terminal).
- Collapsing blank rows (decision 1).
- Exact-text copy via herdr `pane read --source recent-unwrapped` (fragile row alignment;
  revisit if the heuristic is not good enough).
- Upstream herdr changes (forward `?2004`, mark wrapped rows, a CLI paste that does not
  submit) — file as follow-ups.
- Other writers: dispatch and handoff injection use `write_line` → `herdr agent prompt`,
  which herdr already brackets and submits itself (`HerdrBackend::write_line`); nothing to
  change. No other frontend caller of `ptyWrite` writes pasted text.

## Technical design

### Paste
- `src/lib/terminalPaste.ts` (pure): `preparePasteText(text)` — newline normalisation only.
- `src/lib/ipc.ts`: `ptyPaste(project, id, text)` beside `ptyWrite`.
- `TerminalPane.tsx`: `sendPaste(text)` as above; the three `term.paste(...)` sites
  (clipboard text, image path, file drop) call it. The explicit Ctrl+V handler is
  unchanged; the host `paste` listener is reworked as described.
- `src-tauri/src/host.rs`: `pty_paste` command (registered next to `pty_write` in the
  invoke handler) → a new `PtyBackend::paste` method → `WriteJob::Paste`. `PtyBackend`
  gains the method, so the herdr, fake, and unavailable backends implement it (the herdr one
  applies the same recovery gate `write` does: `admit_session`, then enqueue).
- `writer.rs`: `WriteJob::Paste` and a `Run::Paste` whose `absorb` always refuses, and which
  nothing absorbs (today's `absorb` already hands back any non-matching kind). Bracketing and
  the bridge cap live where the job is built.
- Fallback: `SessionIo` needs a paste-aware fallback entry (the current
  `fallback(&[u8])` translates bytes into `send-text`/`send-keys` ops and would mangle a
  block); it sends one `send-text` and applies the fallback cap.

### Copy
- `src/lib/selectionText.ts` (pure over `IBuffer`): `selectionToText(buffer, cols, range,
  eol)`, built on `bufferRowCells` (needed for column-bounded partial rows; `bufferRowText`
  has no column bounds) and the `joinsNext` handling in `bufferText.ts`.
- `TerminalPane.tsx`: Ctrl+C and the capture-phase `copy` listener both call it with
  `term.getSelectionPosition()` and `term.buffer.active`.

## Tests
- `terminalPaste.test.ts`: `"a\r\nb\nc"` → `"a\rb\rc"`; empty stays empty.
- `terminalClipboard.test.ts`: updated native-paste policy.
- `selectionText.test.ts` (fake buffer like `bufferText.test.ts`): wrapped rows join;
  full-width rows join; short rows break; a full row never joins a blank next row; blank
  rows preserved exactly; box-drawing last glyph does not join; wide glyph at the last two
  cells and wrapped-wide padding both join; trailing spaces trimmed; partial first/last row
  ranges; eol parameter.
- `writer.rs`: held writer with `Text("x")`, `Paste`, `Text("y")` → three separate
  `terminal.input` lines, middle one exactly the bracketed block; extend the existing
  coalescing test pattern. Paste containing embedded `ESC[201~`/`ESC[200~` → markers
  stripped, still one block; over-cap paste → nothing sent.
- Fallback: paste → one `send-text` containing the bracketed block, zero `enter`/`esc`
  keys; oversized (including quote-heavy) paste refused.
- No test may touch a running herdr (fake bridge/ops only).

## Risks
- Relies on herdr 0.9.3 behaviour "one complete bracketed block = paste, re-wrapped per
  the pane's real mode". Verified in source; safe for shells without bracketed paste
  because herdr strips the markers there.
- The CLI fallback cannot be mode-aware (see Scope); markers may appear as stray text in
  a non-`?2004` pane on that rare path.
- Full-width-join heuristic will wrongly join a real line that exactly fills the width.
  Acceptable trade-off (decision 2); box-drawing rows and blank next rows are excluded to
  limit damage; tunable later.
- Overriding native `copy` must not break Ctrl+C → SIGINT when nothing is selected.
- Intercepting native paste off Windows changes macOS/Linux behaviour that was previously
  left to xterm; needs a manual check there (middle-click paste included).
- Smoke checks owed on a real app: multi-line paste into Claude Code and into a plain
  shell; image paste and file drop still attach; copy of wrapped prose, separator lines
  and a blank-row-heavy block.

## Decisions
1. Blank rows: kept exact — no collapsing. The fix is the wrap joins only.
2. Full-width-join heuristic: on by default (occasional false join accepted).
3. CLI-fallback paste cap: 24 KB; above it the paste is refused and logged, never sent
   partially.
8. (user) A refused paste (either cap) shows a visible toast via the existing
   `useToast()` (`src/components/Toast.tsx`), naming the size and the limit, in addition
   to the log line. The bridge cap is checked synchronously in `pty_paste` so the IPC
   rejects and the pane toasts directly; the fallback cap is only known on the writer
   thread, so the writer emits a per-session "paste refused" event the pane listens for.
9. (user) The CLI fallback sends the raw-marker block even to a non-`?2004` pane
   (accepted limitation; the fallback only runs when no bridge is up).
4. (refine) Dedicated paste IPC/job; the backend owns bracketing, the frontend only
   normalises newlines.
5. (refine) Image-path paste and file drop are bracketed pastes.
6. (refine) Native paste is intercepted on all platforms; Windows keeps its explicit
   Ctrl+V path.
7. (refine) Copy line separator follows platform convention, as xterm's does today.

## Open questions
None outstanding.

## Complexity
Small–medium: ~8–10 files across frontend + `herdr` backend, one subsystem (terminal
input/output). Recommend skipping Fracture / Singletonize / Loss check.
