# Loss report — Claude → Codex transfer hardening

Compared the refined proposal (both refine rounds) against the fractured, singletonized docs.

**Preserved:** ~145 of ~150 items. These include:
- every root cause R1–R8;
- every architecture bullet A–G;
- all 11 import test scenarios;
- the strict-launch and Default-regression scenarios;
- persistence and resume-row handling;
- CodexEnv resolution order;
- the dispatch refusal;
- scope, risks and decisions.

**Modified (justified):**
- JSONL selection prefers the newest non-`agent-*` transcript. The persisted session id is only logged, because it goes stale.
- Import teardown runs detached after the outcome. Cancel and timeout kill inline.
- The frontend `restarting` backstop is 120 s.
- "Agent never appeared" checks pane process-info before failing.
- The Codex→Claude dispatch-gate narrowing was added.
- The strict chain is bounded by its 100 s budget.
- The prompt-ready check was dropped, because herdr has no such signal.
- The stale-exit `gen` field is conditional on a reproduction.
- Version-gated flags were replaced by a flag-dropping fallback step.

**Intentionally dropped:**
- The seeded handoff prompt for fresh Codex. Fresh Codex starts blank.
- A "keep Claude" option on a failed import.
- Sha-based ledger matching.

**Unintentionally dropped → restored:**
- Pane-side restart tests: `restartPlan`, `shouldStartTransfer`.
- Running "Copy diagnostics after every smoke transfer". The imported toast now offers it too.

**Later change (code review):** Codex-not-found keeps the worktree on Claude instead of switching to a fresh Codex that cannot start.
