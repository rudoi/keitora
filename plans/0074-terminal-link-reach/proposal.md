# 0074 — Terminal links: wrapped links span rows; open files outside the worktree

## Goal

Two link bugs in agent (chat) panes:

1. **Wrapped links break.** A URL or file path that runs past the right edge onto the next
   row only underlines and opens the fragment on the hovered row, so the link is useless.
2. **Files outside the worktree won't open.** Clicking a file link whose target lies outside
   the pane's worktree fails with "File is outside this terminal's worktree". The click is a
   deliberate user action, so containment is not a meaningful guard.

## Root causes

1. herdr repaints the screen, so xterm never marks rows as soft-wrapped (`isWrapped` is
   always false for herdr panes — the same loss copy fidelity worked around). Both link
   paths rely on `isWrapped`:
   - `createFileLinkProvider` (`src/lib/terminalLinks.ts`) joins only `isWrapped` rows.
   - `WebLinksAddon` (`@xterm/addon-web-links`) does the same internally.
   On top of that, agent TUIs (Claude Code, Codex) hard-wrap their own output under a gutter,
   so a continuation row starts after leading indentation (e.g. column 2), not column 0.
2. `validate_canonical_target` (`src-tauri/src/terminal_links.rs`) requires
   `target.starts_with(root)`.

## Scope

**In**
- One link provider for both web and file links, scanning the *logical line* around the
  hovered row, where rows join by:
  - `isWrapped` on the next row → join verbatim (real soft wrap), or
  - the row is **filled to its last cell** (the existing `isFilled` heuristic from copy,
    including its box-drawing and wide-glyph rules) and the next row is non-blank → join,
    **dropping the next row's leading spaces** (agent gutter).
- A link's range spans from its first to its last cell, so hover underlines the whole link
  across rows and any row's fragment activates the full text.
- Backend: drop the worktree-containment check. Relative paths still resolve against the
  pane's worktree (`..` may now leave it); absolute paths open anywhere local.

**Kept (user decision)**: the target must exist and be a regular file; executable/script/
shortcut extensions and binary/shebang headers are refused; UNC/network paths, device names,
ADS and drive-relative syntax are refused. Additionally refuse a *canonical* target that
resolves to a UNC/network location (containment used to imply this).

**Out**
- `~` expansion, OSC 8 changes, per-row link highlights, configurable join behaviour.

## Technical design

### Frontend (`src/lib/`)
- Move `isFilled` (+ `isBoxGlyph`) from `selectionText.ts` to `bufferText.ts` as an export;
  `selectionText.ts` imports it (no behaviour change for copy).
- New pure helper in `terminalLinks.ts`: build the logical line around row `y` from an
  abstract row reader (text, cell map, `wrapped`, `filled`) so it is unit-testable without a
  live xterm buffer. Output: joined text + parallel `xs`/`ys` cell maps (as today), with
  dropped indentation simply absent from the text. Bounded by the existing `maxRows` cap in
  both directions.
- Link detection on the joined text: web URLs via a regex equivalent to `WebLinksAddon`'s
  (`https?://` followed by URL characters, trailing punctuation trimmed) plus
  `findFileReferences` (which already rejects web candidates). Overlaps resolve to the web
  match. Only links whose span includes row `y` are returned.
- Replace `WebLinksAddon` with the unified provider; remove `@xterm/addon-web-links` from
  `package.json` if nothing else uses it. The OSC 8 `linkHandler` is unchanged.
- The ILink `range` runs first-cell → last-cell; on a gutter-indented continuation the
  indent cells inside the range get underlined too (accepted cosmetic cost of one range).

### Backend (`src-tauri/src/terminal_links.rs`)
- `validate_canonical_target(target)` loses the `root` parameter and the `starts_with`
  check; gains a refusal for UNC/verbatim-UNC canonical targets on Windows.
- `validate_target` keeps the network-worktree refusal only for relative paths (an absolute
  path no longer depends on the worktree).

## Tests
- TS (`terminalLinks.test.ts`): logical-line building — soft wrap, filled-row join with
  gutter skip, no join for non-filled rows / blank next row / box-glyph edges, hovering the
  continuation row; web URL across rows; file path across rows; overlap web-vs-file;
  existing `selectionText` tests stay green after the `isFilled` move.
- Rust: replace the "reject traversal/sibling/outside" expectations with acceptance of
  outside absolute and `../` paths; keep missing-file, executable, alias-target tests; add
  a canonical-UNC refusal test (Windows-only, pure path check).

## Risks
- **False joins:** a link ending exactly at the last column of a word-wrapped paragraph
  will absorb the next row's first word. Accepted (user chose gutter-skipping); bounded to
  links that end exactly at the edge.
- Removing containment widens what a click can open; mitigated by the kept executable and
  network refusals.

## Refinement (automated review)

Decisions folded in after verifying the design against the code:

1. **Join predicate per boundary.** Row `r` joins `r+1` when `r+1.isWrapped` (verbatim) or
   when `r` is filled (`isFilled`) and `r+1` is non-blank (indent dropped). Scanning *up*
   from the hovered row tests the row above; scanning *down* tests the current row. Both
   share the `maxRows` cap (kept at `ceil(65536/cols)`, looser than the addon's 2048-char
   window — accepted). `bufferRowCells`' `joinsNext` gets the real join decision, not
   "not the last row", so wide-glyph padding at a non-joined edge reads correctly.
2. **Indent skipping** slices the first `k` units off both `rowText` and its `cellAt` map;
   the per-row sentinel never enters `xs`/`ys`.
3. **False joins on files get a fallback.** A file link whose span crosses a *heuristic*
   (filled-row) boundary also carries the prefix that ends at that boundary, when the prefix
   classifies as a file on its own. Activation tries the joined path first and, if the open
   fails, the prefix. Web links have no fallback (accepted risk, user decision).
4. **Web URL detection** copies `WebLinksAddon`'s `strictUrlRegex` verbatim and keeps its
   `isUrl` host-consistency check; `classifyTerminalLink` still re-validates with `new URL`.
5. **Tests that change:** `terminalLinks.test.ts` provider indices (`providers[0]` web /
   `providers[1]` file collapse to one provider), the "wires web addon" test's disposal
   counts; `terminalFileIpc.test.ts`'s mocked "outside" rejection message becomes a neutral
   error. New TS cases: filled-row URL with gutter, filled-row file path, filled row followed
   by prose (fallback used), non-filled row no join, box-glyph edge no join, wide glyph at a
   filled edge, hovering the continuation row.
6. **Backend specifics.** Canonical target refusal matches `Prefix::UNC | VerbatimUNC |
   DeviceNS`. The UNC-*root* refusal applies only to relative paths. Before opening, strip
   the `\\?\` verbatim-disk prefix (`dunce::simplified`, already in the lockfile) since
   some handlers choke on it. Rust tests: traversal/sibling/symlink-escape cases move from
   "rejected" to "accepted"; `validate_canonical_target` loses `root`; executable-alias stays
   rejected; add a canonical-UNC refusal test (Windows) and an absolute-outside acceptance.
7. **Docs:** `docs/build-plan.md` entry for clickable links drops "validates local worktree
   containment"; add an entry for this work.

## Known limitations (out of scope)
- `~/…` paths are not expanded (they resolve to a literal `~` under the worktree and fail).
- MSYS-style `/c/Users/…` and rooted `/tmp/x` on Windows resolve against the worktree's
  drive.

## Open questions
- None outstanding.

## Complexity
Small (~6 files, terminal-links subsystem front + back). Skip Fracture, Singletonize, Loss
check.
