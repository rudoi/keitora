# 003 — Frontend wheel + keyboard scroll

**Goal:** every wheel tick and Shift+PgUp/PgDn over a pane becomes a `ptyScroll`.
xterm does no scrolling or arrow emulation of its own.

**Prerequisites:** 002.

## Implementation

1. **`src/lib/termFit.ts`**
   - Generalise `accumulateWheelLines` (pixel-only today) to take `deltaMode`:
     - pixel → divide by the row height;
     - line → as-is;
     - page → × rows.
   - Keep the fractional remainder and the extreme-delta guard. Keep the existing
     call signature working, or update all callers and tests.
   - Add a pure `wheelCell(offsetX, offsetY, cellW, cellH, cols, rows) ->
     {column,row}` (0-based, clamped).
2. **`TerminalPane.tsx`**
   - Remove the `onWheelNormalize` capture listener (add + remove) and its
     alt-buffer special case.
   - Install `term.attachCustomWheelEventHandler(e => …)`: compute lines with
     `accumulateWheelLines`, accumulate into a per-pane pending
     `{up/down, lines, column,row}`, and flush once per `requestAnimationFrame`
     via `ptyScroll`. A direction change flushes the pending batch first.
     `preventDefault()` and return `false`.
   - Ignore horizontal-only wheel.
   - Ctrl+wheel stays unhandled (zoom keybindings, if any, keep working).
   - Only send while the pane status allows input (same gate as `onData` →
     `ptyWrite`).
   - `scrollback: 5000` → `100`. Remove the now-meaningless sticky-tail
     re-pin logic (`isAtBottom` / `scrollToBottom` around the fit path), or reduce
     it to the minimum still needed; `isAtBottom` stays if other callers use it.
   - `keitora:debug-wheel`: log every wheel event (deltaMode, deltaY, lines,
     cell, direction), not just alt-buffer ones.
3. **Keyboard:** Shift+PageUp / Shift+PageDown → `ptyScroll(up|down, rows - 1)`
   at the cell under the cursor (or 0,0). Wire through `src/lib/keybindings.ts`
   the way other pane keys are handled, so the key isn't also sent to the app.

## Tests (vitest)

- `accumulateWheelLines`: pixel/line/page modes, remainder carry, sign/direction,
  extreme-delta guard.
- `wheelCell`: inside, edges, clamping.
- Keybinding: Shift+PgUp/PgDn map to the scroll action (follow the existing
  keybindings tests).

## Validation

`yarn test`, `yarn tsc --noEmit`, biome via the pre-commit hook. No app launch.

**Commit:** `feat(terminal): route wheel and Shift+PgUp/PgDn to herdr scrollback`
