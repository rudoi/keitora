# 0067 — UI fixes (rail label, terminal search, modal layering) + remove dispatch suggestions

## Goal

Four small, independent fixes delivered together:

1. **Rail footer label** — `Settings` → `settings`, matching `perf` and `shortcuts`.
2. **Ctrl+F search** — today Ctrl+F opens two boxes (the pane's own search bar *and*
   WebView2's native find bar) and neither finds anything. Make there be exactly one box,
   and make it search the pane's real (herdr-held) history.
3. **Modal layering** — pane overlays (kill & restart, etc.) paint above the "Quit
   Keitora?" dialog. Introduce a shared z-layer scale so every dialog sits above pane
   overlays and the quit dialog is always topmost.
4. **Remove dispatch suggestions** — delete the unused Cmd+K "suggest" feature end to end.
   Manual board dispatch (the `d` menu → send task to a worktree) stays.

## Scope

**In:** the four items above, their tests, harness mocks, and docs that describe the
removed feature.

**Out:**
- Manual dispatch (`dispatch_apply`, `DispatchGate`, reservations, `DispatchMenu`,
  `[dispatch] claude_steps/codex_steps`).
- Deleting historical `dispatch_log` rows with `decision='suggested'` — harmless, left in place.
- Making xterm hold scrollback itself (herdr owns history by design).
- The pre-existing `keitora.toml.example` `steps` vs `claude_steps` mismatch (separate task).

## Architecture / Technical design

### 1. Rail label
`src/components/Rail.tsx:690` — change the span text. The `title="Settings"` tooltip stays
sentence-case like the other footer tooltips.

### 2. Terminal search

**Why two boxes.** `TerminalPane.tsx:996-1000` opens `TerminalSearch` from xterm's
`attachCustomKeyEventHandler` and returns `false` — which only stops xterm processing, it
does not `preventDefault` the DOM event (the comment claiming otherwise is wrong). The
window keymap (`useKeybindings`, `keybindings.ts:443-463`) skips `search.open` because it is
not `global` and focus is in xterm's textarea. So the event reaches WebView2, which opens its
own find bar (DOM-only, can't see canvas text).

**Why nothing is found.**
- `scrollback: 0` (`TerminalPane.tsx:917`): the xterm buffer is only the visible screen;
  herdr holds history.
- `onDidChangeResults` (`TerminalSearch.tsx:24`) never fires because no `decorations`
  option is passed (`@xterm/addon-search` 0.16 only reports results with decorations), so
  the counter stays blank and nothing is highlighted.

**Design.**
- *Suppress native find:* call `e.preventDefault()` in the pane's Ctrl/Cmd+F branch (and
  gate that branch on `!shiftKey && !altKey` — today `e.key === "f"` also matches
  Ctrl+Shift+F). In `useKeybindings`, add a separate block — *before* the `inEditable` skip
  (`keybindings.ts:455`) and independent of the `search.open` binding's `when` — that always
  `preventDefault`s a plain Ctrl/Cmd+F, so the WebView2 find bar can never open (including
  from the `TerminalSearch` input itself). Nothing else in the app binds Ctrl+F; the accepted
  cost is that native find is also gone in DiffView/Settings (it found nothing useful there
  either). Fix the misleading comment at `TerminalPane.tsx:999` and the `search.open`
  comment at `keybindings.ts:215`.
- *History source:* new Tauri command `pty_read_history(project, id, lines)` (handler next
  to `pty_scroll_offset`, `host.rs:1624`; registered at `host.rs:2135`; IPC wrapper in
  `src/lib/ipc.ts` next to the scroll-offset wrapper). It calls a new defaulted
  `PtyBackend::read_history` (trait at `backend/mod.rs:466`; default returns an
  "unsupported" error, like `scroll`/`scroll_offset`, so `fake.rs` and `unavailable.rs` need
  no change). Only `HerdrBackend` (`herdr/mod.rs:2147`) implements it: `gate.admit_session`,
  resolve the pane via `self.keymap.get(key)` as `scroll_offset` does, then
  `pane_read(pane, "recent", "text", Some(lines))`.
- `HerdrOps::pane_read` gains `lines: Option<u32>` (`ops.rs:166`); implementors are exactly
  `RealHerdr` (`real.rs:304`, appends `--lines N`) and `FakeHerdr` (`fake.rs:799`). The
  existing attach caller (`herdr/mod.rs:2335`) passes `None`. `FakeHerdr` gains a seeding
  hook (e.g. `set_pane_text(pane, text)`) and records `lines` in its op detail string so the
  backend method is unit-testable.
- `recent` (wrapped, at the pane's current width) is used rather than `recent-unwrapped` so
  that one returned line == one screen row, which is what scroll offsets count.
- *Matching (pure, unit-tested TS module `src/lib/historySearch.ts`):* given history text,
  query, case/regex flags → ordered list of matches `{ line, col, len }`; given a match, the
  total line count and the viewport row count → the target scroll offset (match centred,
  clamped at 0 = live tail). Invalid regex → "invalid regex" state, no throw.
- *Navigation:* on open / query change, fetch history (debounced) and compute matches;
  Enter/↓ = next older-to-newer per the usual find convention (start from the newest match,
  nearest the live tail); Shift+Enter/↑ = previous. Jump: read current
  `pty_scroll_offset`, `pty_scroll` up/down by the difference, then, once the repaint frame
  lands, highlight the match on the now-visible screen at the computed row with
  `term.registerDecoration` (or `term.select(col, row, len)`). The xterm `SearchAddon` is
  dropped: it only sees the visible buffer, and its `noScroll` is not public API in 0.16.
  Counter shows `i / N` from our own match list.
- *Close:* clears decorations and returns the pane to live (offset 0), as Esc does today.
- *Drift:* output streaming between fetch and jump shifts line positions. Re-fetch history on
  each Enter when the pane has produced output since the last fetch; jumps are best-effort.
- History size: request a bounded line count (e.g. 20 000) to keep the IPC payload sane.

### 3. Modal layering
Define named layers once (CSS custom properties / Tailwind `z-*` classes, documented in
`App.css`):

| layer | z | members |
|---|---|---|
| in-pane / in-rail | ≤ 15, rail drop line 1000 (local stacking) | pane overlays (`App.css:726`, `App.tsx:1684`), search bar (`App.css:806`), `.rail-drop-line` |
| app chrome | 40–100 (unchanged) | DiffView (`App.tsx:1255`, internal `z-40/50`), PerfOverlay (`App.css:2064`), `.fontsize-hud` 90, terminal link tooltip (`terminalLinks.ts:209`) |
| floating | 50 (unchanged) | Radix menus/popovers (`Rail.tsx:237`, `LaneTagFilter`, `DispatchMenu`) — none are portaled from inside a dialog today |
| modal | 1500 | every Radix `Dialog` overlay **and** content: `AddProjectDialog`, `ConfirmDialog`, `CreateWorktreeDialog`, `RestartDaemonDialog`, `TaskFormDialog`, `SettingsDrawer` (Overlay `:488` has none, Content `:490` is `z-50` today; its inner `z-10` overlay stays local); plus the non-Radix kbd cheatsheet `.kbd-backdrop` (`KeybindingHelp.tsx:49`, `App.css:935`, 100 today) |
| project-switch blocker | 2000 (unchanged) | `.switch-overlay` |
| toasts | 2500 | `Toast.tsx:61`, `AttentionBanners.tsx:85` (`z-50` today) — must sit above modals so errors raised by dialog actions are visible |
| quit | 3000 | `QuitConfirmModal` overlay + content |

Layers are defined once as CSS custom properties on `:root` (`--z-modal`, `--z-toast`,
`--z-quit`) and consumed from Tailwind via arbitrary values (`z-[var(--z-modal)]`), so the
scale lives in one documented place. Fix the stale `App.css:1036` comment (rail menus are
`z-50`, not 1000). Note in the scale comment: any future popover portaled from inside a
dialog must use a layer above `--z-modal`.

### 4. Remove dispatch suggestions
Delete (inventory verified during research; bounds re-checked while editing):
- **Rust** `src-tauri/src/dispatch.rs`: `Proposal`, `SCHEMA`, `API_TIMEOUT_MS`,
  `SUGGEST_TIMEOUT`, `parse_proposals`, `build_prompt`, `TaskCtx`/`WorktreeCtx`,
  `dispatch_suggest`, `run_claude_bounded`, `run_codex_bounded`, `parse_codex_proposals`,
  `recent_tags_for`, and their tests; rewrite the module header; prune imports
  (`dispatch.rs:11-13` — `Read`, `Command`, `Stdio`, `Instant`, `Duration`) that only the
  bounded runners used, so clippy stays clean. Unregister `dispatch_suggest` at
  `host.rs:2190`. No crate dependency is suggest-only (`SCHEMA` is a const string).
- **Frontend**: `Proposal`/`dispatchSuggest` in `src/lib/dispatch.ts`; delete
  `DispatchStrip.tsx`, `ProposalCard.tsx`, `ConfidencePip.tsx` (used only by these and
  Inspector); strip the proposal panel, props and header comment (`:4`) from
  `Inspector.tsx`; in `App.tsx` remove state (~425), `idleCount`/`onSuggest`/
  `onProposalSpent` (~657-692), ctx wiring (898, 911), the strip (~1168) and Inspector
  props (~1200); remove `dispatch.suggest` (Cmd+K) and the `onSuggest` ctx field
  (`keybindings.ts:31`) + the `keybindings.test.ts:165` mock; check `KeybindingHelp`'s Board
  group still renders; stale comments (`keybindings.ts:5,87,434`,
  `usePrefersReducedMotion.ts:2`).
- **Harness**: `dispatch_suggest` mock in `.harness/app.tsx:338`.
- **keitora-tasks**: the `'suggested'` decision string stays readable (old rows) but doc
  comments stop describing it as live (`backend.rs:525`, `model.rs:127`). Fixtures that use
  it (`dispatch.rs:253`, `backend_tests.rs:492,587,609,647,713`, `github/backend.rs:1632`,
  `github/sidecar.rs:435,443`) stay — they test that old rows are still handled.
- **Docs**: README "Suggestions" bullet (`:23`), `docs/windows-backend.md` §2c (`:114`),
  `docs/build-plan.md` §7 and references (74, 88, 131, 170, 252-261, 296, 330, 608) — mark
  removed rather than rewrite history.

## Dependencies
None external. `herdr pane read --lines N` exists in herdr 0.9.3 (checked via `--help`).

## Risks
- **History/offset alignment** — whether herdr's `recent` text rows line up exactly with
  `scroll_offset` units (trailing blank rows, wrapped lines, alt-screen TUIs). Mitigation:
  pure offset math is unit-tested against fixtures; behaviour is verified in a live smoke by
  the user. Alt-screen apps (full-screen TUIs) have no scrollback; search then covers only
  what `recent` returns.
- **Testing must not touch the running herdr** — all new Rust tests go through the fake
  herdr ops; no gated herdr runs on this machine.
- Removing Cmd+K frees the chord; nothing else is bound to it.

## Open questions
- None blocking. Exact history line cap (20 000 proposed) can be tuned in implementation.

## Complexity
**Medium by file count (~20 files), but four independent, well-scoped parts.** Recommend
skipping Fracture / Singletonize / Loss check — this single proposal is the plan.
