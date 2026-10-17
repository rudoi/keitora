# 001 — Spike (source only)

**Goal:** answer the proposal's spike questions without running herdr.

**Implementation:** read the herdr v0.9.3 + ghostty source and the installed Claude
Code / Codex bundles. Record results in `spike-findings.md`:
1. Rows retained per MB of `scrollback_limit_bytes` vs pane width.
2. ghostty's `ESC[3J` / `ESC[2J` behaviour, and whether herdr exposes any control.
3. Whether Claude Code / Codex enable mouse reporting, use the alt screen, or emit
   `ESC[3J`.

**Validation:** every claim cites a source file/line or a bundle snippet, with a
confidence level.

**Commit:** `docs(plan): record scrollback spike findings`
