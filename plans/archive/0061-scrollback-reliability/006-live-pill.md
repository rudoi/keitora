# 006 — "↓ live" indicator

**Goal:** show when the focused pane's herdr viewport is scrolled into history,
with one click back to live.

**Prerequisites:** 002, 003.

## Implementation

1. **Rust**
   - Parse `scroll: {offset_from_bottom, max_offset_from_bottom, viewport_rows}`
     (optional, default absent) into herdr's `PaneInfo` in `ops.rs`/`real.rs`. Use
     whatever pane read already returns `PaneInfo`; add a single-pane getter on
     `HerdrOps` only if none exists.
   - `PtyBackend::scroll_offset(key) -> Result<Option<u32>>` (default
     `Ok(None)`).
   - Tauri command `pty_scroll_offset`.
2. **Frontend**
   - After a scroll burst, debounce ~150 ms, then query the offset; show the pill
     when it is > 0.
   - Hide it on any input (herdr snaps back on input) and on a frame after an
     input.
   - Click / End (when the pill is shown) → `ptyScroll(down, offset)`.
   - Never poll while idle.
3. **Tests**
   - Rust: `PaneInfo` parsing with and without `scroll`, and the backend getter
     through mocked ops.
   - Frontend: the debounce/visibility logic as a pure helper.

## Validation

`cargo test --workspace` (once), `yarn test`, `yarn tsc --noEmit`.

**Commit:** `feat(terminal): show a live-tail pill while scrolled into history`
