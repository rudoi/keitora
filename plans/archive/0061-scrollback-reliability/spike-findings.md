# Spike findings: scrollback depth, ED 3, agent TUI modes

Source-only. herdr v0.9.3 (vendored libghostty-vt, base `44f2a44`), Claude Code 2.1.302 (native binary), Codex CLI 0.160.0 (win32-x64 vendor binary). Nothing was executed.

## 1. Lines per MB (confidence: medium-high on the formula, medium on the constants)

**Plumbing.** `crates/ghostty-vt/src/lib.rs:860-908`: `Terminal::new(cols, rows, max_scrollback)` sets `GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES` → `PageList.limits.bytes` (`PageList.zig:665`). There is no line limit (a `scrollback_max_lines` option exists, but herdr doesn't use it).

**Accounting** (`PageList.zig`):
- `page_size` is the sum of the raw page mappings (`:433-436`). It includes the active area. Compressing a page does **not** reduce it.
- Every standard page is one pool item of `std_size = Page.layout(std_capacity)` (`:322-325`). `std_capacity` = 215 cols × 215 rows, 128 styles, 8 KiB graphemes (`page.zig:1896`).
- Rows per page: `Capacity.adjust` keeps the page size fixed and sets `rows = available / (sizeof(Row) + sizeof(Cell)*cols)`. Row and Cell are both 8 bytes (`page.zig:1970-1990`, `:2014`, `:2143`). So **bytes per row ≈ 8·cols + 8**, plus a per-page share of the metadata (styles, graphemes, hyperlinks).
- Grid area is about 371.5 KB per page and metadata adds roughly 20-30 KB, so a page is ≈ 400 KiB. The commented-out test in `page.zig:2764` says 512 KiB, but that test is stale. Treat the item size as 400-512 KiB. The lines-per-MB result is nearly the same either way, because the grid dominates.
- **Pruning:** `grow()` drops the oldest whole page when `page_size + item_size > max(bytes)` (`:3996-4000`). The effective max is `max(explicit, minMaxSize)`, where `minMaxSize` = (pages needed for the active area + 1) × item (`:6926`, `:7006-7046`). So retained history moves in steps of one page (N−1 to N pages), and the active area comes out of the same budget.
- **Rows are physical:** a wrapped line costs several rows. Pages that outgrow the standard capacity (more than 128 styles, lots of graphemes or OSC 8 links) get resized to a larger, non-pooled page. That page counts at its full size, so the real depth is lower.

**Estimate.** Assumes a page of about 400 KiB with about 373 KB of grid. Pages kept: N = ⌊B/409600⌋. Each number is N × rows-per-page, which is the upper bound including the active area. Subtract the pane height (about 50 rows) to get scrollback. The lower bound is one page less.

| Budget (decimal) | N pages | 120 cols (385 rows/pg) | 200 cols (231) | 300 cols (154) |
|---|---|---|---|---|
| 4 MB (current) | 9 | ~3,460 (min ~3,080) | ~2,080 | ~1,390 |
| 8 MB | 19 | ~7,300 | ~4,390 | ~2,930 |
| 10 MB | 24 | ~9,240 | ~5,540 | ~3,700 |
| 16 MB | 39 | ~15,000 | ~9,000 | ~6,000 |
| 20 MB | 48 | ~18,500 | ~11,100 | ~7,400 |

Rule of thumb: **lines ≈ 0.93·B / (8·cols + 8)**. That is about 960 lines/MB at 120 cols and about 385 lines/MB at 300 cols. herdr's own test (`lib.rs:4384`, 1 MB vs 10 MB at 80 cols) is consistent with this.

## 2. ED 3 / ED 2 in ghostty (confidence: high)

`Terminal.zig:3573-3701`:
- **`CSI 3 J`** → `screens.active.eraseHistory(null)`: the scrollback is erased unconditionally. The dispatch path is `stream_terminal.zig:392`.
  - There is no option to ignore it in `Terminal.zig`, `stream_terminal.zig` or the C API option list (`c/terminal.zig`).
  - herdr does not filter it: there is no `3J` handling in `src/` or the wrapper.
  - Ignoring it would mean byte-filtering the PTY stream before ghostty, or patching the vendored source.
- **`CSI 2 J`** → clears the active rows **without** pushing them to scrollback. There is one exception: on the primary screen, when the last non-empty row is an OSC 133 prompt, it calls `scrollClear()` first (the "^L at a prompt" heuristic, `:3605-3640`).
- **`CSI 22 J`** (`scroll_complete`) always pushes the screen into scrollback (`scrollClear`).

## 3. Claude Code 2.1.302 (`~/.local/share/claude/versions/2.1.302`)

DEC mode table (chunk-2v6wq4qm): `ALT_SCREEN_CLEAR:1049, MOUSE_NORMAL:1000, MOUSE_BUTTON:1002, MOUSE_ANY:1003, MOUSE_SGR:1006`. Mouse presets:
```
mvn("full")   = ?1000h ?1002h ?1003h ?1006h
mvn("scroll") = ?1000h ?1006h
rj(): CLAUDE_CODE_DISABLE_MOUSE → "off"; CLAUDE_CODE_DISABLE_MOUSE_CLICKS → "scroll"; else "full"
```
- **Mouse is enabled only while the alt screen is active** (`setAltScreenActive(n, mode)` sets `altScreenMouseTracking = n ? mode : "off"`). Confidence: high.
  - In fullscreen, ?1000h is on in both "full" and "scroll". herdr's `wheel_routing` (`pane/terminal.rs:1968`) sees `MOUSE_PRESS_RELEASE` and returns **MouseReport**, so the wheel goes to Claude, not to history.
  - With `CLAUDE_CODE_DISABLE_MOUSE=1`, the wheel becomes AlternateScroll (arrow keys), because 1007 is on by default in herdr.
  - Either way the alt screen has no host history.
- **Fullscreen (?1049h) decision** (resolution order):
  1. Off by env: `CLAUDE_CODE_NO_FLICKER=0` or `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1`.
  2. On by env: `CLAUDE_CODE_NO_FLICKER=1`.
  3. Automatic off: after a crash, under tmux -CC, or over Windows SSH.
  4. The `tui` setting: `"fullscreen"` / `"default"`, or the `/tui` command.
  5. Otherwise fresh installs and remote feature gates can turn it **on by default**.

  Keitora can force classic mode with `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1`. Confidence: high.
- **Sub-views:** `enterAlternateScreen()` (`?1049h` + 2J + H) is also used for handoffs out of classic mode, such as the external editor. The main screen is restored with `?1049l`, so this is not destructive. Confidence: medium.
- **3J emission.** `bbt = CSI 3 J`, `gv = CSI 2 J` (chunk-44f9wjs3):
  - The `clearTerminal` patch op emits `altScreen ? (2J+3J+H) : Unt(rows)`. `Unt` is cursor-home plus EL2 on each viewport row, with **no 3J**. So in 2.1.302 the classic main-screen renderer clears **without** wiping scrollback. Confidence: medium-high.
  - A second renderer (class with `nativeHistory`, up to 10,000 lines, using DECSTBM scroll regions) emits `2J+3J+H` in three cases:
    - on resize when the cols change or the rows shrink,
    - on resume after the alt screen when the size changed,
    - on `switchTranscript()`.

    It is built by a `scrollable`/`pushUp` component. I could not determine from the bundle when it is active. Confidence: low on when, high that it emits 3J.
  - The only literal `\x1B[3J` is Bun's `console.clear()`. Claude doesn't call it; its 2 callers are Bun dev-server code.
- `?1007` and the other raw `?1049h` hits come from embedded third-party programs (a nano/less-style help screen and Bun's update UI), not from Claude.

## 4. Codex 0.160.0 (`@openai/codex-win32-x64/.../bin/codex.exe`)

Every DEC private mode string in the binary: `?1004l, ?1006h/l, ?1007h/l, ?1049h/l, ?2004h/l, ?2026h/l, ?25h/l`. Confidence: high.
- **No 1000/1002/1003** at all, so Codex never enables mouse reporting. In alt-screen mode herdr uses **AlternateScroll** (wheel → arrow keys, 1007 is set explicitly). On the main screen it uses host scroll.
- **Alt screen:** `alternate_screen` config key, `--no-alt-screen` / `NO_ALT_SCREEN` flag ("Runs the TUI in inline mode, preserving terminal scrollback history"), and a `fullscreen_transcript` key.
- **3J:** one literal, `ESC[r ESC[0m ESC[H ESC[2J ESC[3J ESC[H`, next to `tui/src/custom_terminal`. This is a full clear including scrollback, probably for clear/reset and redraw. I couldn't confirm the caller from the binary. Confidence: medium.

## Implications

- A 4 MB budget gives only about 1.4k-3.5k lines on wide panes. 16-20 MB gives about 6k-18k.
- ED 3 from Claude (scroll-region renderer, alt-screen clears) and from Codex (full clear) wipes herdr history, and herdr has no switch to stop it.
- For history-preserving panes: run Claude with `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` and Codex with `--no-alt-screen`. Consider a PTY-side filter for 3J.
