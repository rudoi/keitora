# 0077 loss report: proposal vs subsystem docs

## Summary

Sources checked: the proposal at HEAD, its two earlier versions (`84873fc`, `44487e1`), the Decisions list and
the Open issue resolutions in `README.md`. Targets: `01` to `08` and `README.md`.

- The subsystem docs carry nearly all of the proposal. Most requirements sit where the doc map says they should.
- Four places contradicted or left out an Open issue resolution or HEAD requirement. All four are fixed.
- Three requirements were unintentionally dropped. All three are fixed.
- Six differences are justified modifications, and seven items are intentional drops. Both are recorded below.
- Preserved: about 230 requirement lines (counted by group, so approximate).

## Non-preserved items

| # | Class | Requirement (source) | Where it lives / action |
|---|---|---|---|
| 1 | UNINTENTIONALLY DROPPED (fixed) | Open issue 5 resolution: `switch_worktree_cli` (target Antigravity) and `save_user_settings` (`default_cli = antigravity`) call the existing `refresh_integration()` so a late `agy` install needs no restart. | Was missing. Added to `03` (herdr gating), `04` (install triggers 4 and 5), `06` (step 6). |
| 2 | UNINTENTIONALLY DROPPED (fixed) | Open issue 4 resolution: the install hint constants and the README install link. | `05` had `…` placeholders. Filled in with the verified one-liners. `08` now links `https://antigravity.google/product/antigravity-cli/`. |
| 3 | UNINTENTIONALLY DROPPED (fixed) | v1 Dependencies: an Antigravity sign-in is needed for an interactive session. | Missing from all docs. Added to the `08` "Still owed" list. |
| 4 | CONTRADICTION (fixed) | Open issue 1 resolution: a failed `Resume` for Antigravity goes straight to `Fresh`, and `agent.rs` gets a test for it. | `03` said `next_fallback` was "unchanged" and had no test. Now an Antigravity arm plus test `next_fallback_antigravity_skips_continue`. |
| 5 | CONTRADICTION (fixed, needs confirmation) | Open issue 1 spirit: no `-c` for Antigravity. | `03` tested that an unsafe id gives `-c`. Changed to `Fresh` (no args). Confirm this is wanted, because `build_agent_args` currently maps every unsafe id to `Some(None)`. |
| 6 | CONTRADICTION (fixed) | Open issue 9 resolution: path inputs stay enabled for every provider; only autonomy and the dispatch editor are disabled when unavailable. | `07` disabled path inputs when unavailable. Fixed the `ProviderSettingsGroup` spec, the `providerGroupState` tests and the manual check. |
| 7 | CONTRADICTION (fixed) | HEAD proposal, unix discovery: `~/.local/bin/agy` first, then the login `$PATH`. | `03` had the login `$PATH` first (the v1 order). Reordered. |
| 8 | MODIFIED (justified) | v1 verification spike (step 0) and facts appendix. | Now `facts.md`. Decisions are baked into the docs. |
| 9 | MODIFIED (justified) | HEAD: `freshToastMessage` "hard-coded to Codex". | `01` keeps it Codex-only on purpose: the fresh outcome exists only for Claude to Codex. |
| 10 | MODIFIED (justified) | v1/v2 `KEITORA_PANE` pane guard. | `04` uses `KEITORA_BIN` plus a per-pane variable. The binary path is not baked into the shared global file. |
| 11 | MODIFIED (justified) | v1 `agy -c` existence check for the no-id case. | `03` uses `Fresh`. `brain/` is shared by every workspace, so it cannot show whether this cwd has history. |
| 12 | MODIFIED (justified) | Settings DTO `dispatch_steps` at both levels (HEAD). | `02` keeps it at project level only. Open issue 2 accepted this. |
| 13 | INTENTIONALLY DROPPED | v1 workspace `.agents/hooks.json` plus `.git/info/exclude` option. | Rejected by Decision 1. |
| 14 | INTENTIONALLY DROPPED | v1 statusline `tool_confirmation_pending` as a waiting source. | Rejected in v1 (it would take over the user's statusline). Decision 2 chose `blocked`. |
| 15 | INTENTIONALLY DROPPED | v1 winget install and `winget upgrade` guidance for `Unknown` installs. | Superseded by HEAD: `Unknown` shows the official one-liner. |
| 16 | INTENTIONALLY DROPPED | v1 trust bypass flag, if agy gates hooks behind trust. | Superseded. Still open as an unverified fact: `04` notes it, and `08` lists the TUI hook check as owed. |
| 17 | INTENTIONALLY DROPPED | v1 Open Questions 1 to 7. | Answered by Decisions 1 to 7 in the proposal and the README. |
| 18 | INTENTIONALLY DROPPED | v1 "122 transfer tests" count. | Stale numeric baseline. Replaced by the existing-suites guard in `01`. |
| 19 | INTENTIONALLY DROPPED | Gemini CLI as a provider; in-app `agy` installer; behaviour changes to Claude and Codex except `blocked` to `waiting`. | Out of scope in `01` and `05`. |

## Preserved items

About 230 requirement lines from the HEAD proposal and the v1 and v2 versions are PRESERVED in the target docs.
They cover the provider table, the keyed wire shapes, persistence and the SQLite migration, discovery, launch args,
id safety, trust seeding, the write guard, resume, the hook file and merge, the stdin filter, `blocked` to
`waiting`, updates and the doctor, switching, the UI, dev isolation, docs, the release notes, the follow-up task,
and all seven Decisions. The Open issue resolutions 2, 3, 6, 7, 8 and 10 are reflected as written.
