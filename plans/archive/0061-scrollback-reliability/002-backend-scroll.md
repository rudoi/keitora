# 002 — Backend scroll path

**Goal:** a `pty_scroll` command that delivers `terminal.scroll` to a pane's herdr
bridge, in order with input and resizes.

**Prerequisites:** none.

## Implementation

1. **`bridge.rs`**
   - `ScrollDirection { Up, Down }` (serialises `"up"`/`"down"`).
   - `scroll_line(direction, lines, column, row) -> String`: builds
     `{"type":"terminal.scroll","direction":..,"lines":..,"column":..,"row":..}\n`
     with serde. `source` is omitted (herdr defaults it to `wheel`). `lines` is
     `u16`, ≥ 1.
   - `send_scroll_to(stdin, ..)` next to `send_resize_to`.
   - Module doc: one sentence noting `terminal.scroll` is verified against the
     herdr 0.9.3 source and that herdr routes it (app mouse report /
     alternate-scroll / host history).
2. **`writer.rs`**
   - `WriteJob::Scroll { direction, lines, column, row }`.
   - In the run loop, coalesce consecutive queued `Scroll` jobs with the same
     direction, summing `lines` with a saturating add capped at `u16::MAX`. Use the
     latest `column`/`row`. Opposite directions are not merged.
   - Send through the watched write like input does. **On failure: log at debug
     and drop the job.** Never `restart_bridge`. If the stdin is closed/dead, take
     the same path input takes (`drop_bridge`).
3. **`backend/mod.rs`**
   - `PtyBackend::scroll(&self, key, direction, lines, column, row)` with a
     default `Ok(())` body (like `repaint`).
   - Re-export the direction type for host use.
4. **`herdr/mod.rs`**
   - `HerdrBackend::scroll` enqueues `WriteJob::Scroll` exactly as `resize`
     enqueues `Resize`. No live bridge → `Ok(())` (a wheel over a dead pane is not
     an error).
5. **`host.rs`**
   - `#[tauri::command(async)] pty_scroll(project, id, direction: String, lines:
     u16, column: u16, row: u16)` → parse direction (`"up"`/`"down"`, else
     error) → `backend.scroll(..)`.
   - Register it in the `invoke_handler` list.
6. **`src/lib/ipc.ts`**
   - `ptyScroll(project, id, direction: "up" | "down", lines, column, row)`.

## Tests

- `bridge.rs`: `scroll_line` JSON shape for both directions (parse it back with
  `serde_json`).
- `writer.rs`:
  - consecutive same-direction scrolls coalesce into one line with the summed
    `lines`;
  - opposite directions stay separate;
  - saturation at `u16::MAX`;
  - a failed scroll write does not restart the bridge.
  Use the existing test bridge/`Seen` harness.
- `herdr/mod.rs`: end-to-end through `FakeHerdr`. `scroll` on an attached pane
  writes a `terminal.scroll` stdin line (cf. `resize_writes_a_resize_line…`).
  `scroll` on an unknown pane is `Ok`.
- `ipc.test.ts`: `ptyScroll` invokes `pty_scroll` with the right args.

## Validation

`cargo check`; `cargo test --workspace` (non-gated, once); `yarn test`;
`yarn tsc --noEmit`.

**Commit:** `feat(pty): send wheel scroll to herdr panes`
