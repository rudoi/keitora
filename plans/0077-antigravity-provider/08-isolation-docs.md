# 08 — Dev isolation and docs

## Goal

An isolated dev run must never write the real `~/.gemini`, and the docs describe Antigravity as a supported
agent. File the follow-up task for conversation transfer.

## Context

- `herdr integration install antigravity-cli`, Keitora's agy hook writer (doc 04) and trust seeder (doc 03) all
  write under the real `~/.gemini`. `agy` has no config-dir override.
- `scripts/dev-herdr.mjs` isolates herdr and `CLAUDE_CONFIG_DIR` but not `HOME`. It already sets
  `KEITORA_HERDR_NO_INTEGRATION: "1"` (`scripts/dev-herdr.mjs:343`), and `scripts/dev-herdr.test.ts:267-269`
  asserts it.
- Keitora's own writers (docs 03/04) honour that same marker through `global_writes::allowed()`; there is no
  second marker.
- Step 8 of the implementation outline.
- Verified code state (before this work):
  - `scripts/dev-herdr.mjs:341-342` carries the comment `// `herdr integration install` writes the REAL
    ~/.claude / ~/.codex (global, not sandboxable by XDG_*): never let a dev run install it.`
  - `scripts/dev-herdr.test.ts:267` has the test title `"opts out of herdr integration install (it writes the
    real ~/.claude/~/.codex)"`.
  - `src-tauri/src/backend/herdr/supervisor.rs:36-39` documents `NO_INTEGRATION_ENV` as writing the user's real
    `~/.claude` / `~/.codex` (updated in doc 03).
  - `README.md`: `:15-16` (each worktree runs `claude` or `codex`), `:19` (status from Claude Code hooks),
    `:37-41` (install section, Codex optional), `:107` and `:109` (Claude Code session, hooks), `:119`
    (troubleshooting). `CONTRIBUTING.md:38` ("agent hooks into your real `~/.claude` or `~/.codex`").
    `docs/windows-backend.md:22` (`claude` install requirement) and `:64` ("the configured `claude`/`codex`
    directory"). `docs/build-plan.md` phase entries are `- **#00NN — Title. ✅ done (PR #N).**` bullets; the
    last is the one starting at `:581`; the agent CLI updates entry is `:535-557`.
  - `.gitignore` is unchanged; nothing is written into worktrees.

## Files to touch

- `scripts/dev-herdr.mjs`, `scripts/dev-herdr.test.ts`
- `README.md`, `CONTRIBUTING.md`, `docs/windows-backend.md`, `docs/build-plan.md`
- the `keitora task` CLI (one command, below); no source files

## Design

### Dev isolation

1. `scripts/dev-herdr.mjs:341-342` comment becomes: `// `herdr integration install` and Keitora's own agent
   hook/trust writers touch the REAL ~/.claude, ~/.codex and ~/.gemini (global, not sandboxable by XDG_*):
   never let a dev run write them.` The value on `:343` (`KEITORA_HERDR_NO_INTEGRATION: "1"`) is unchanged.
2. `scripts/dev-herdr.test.ts:267` test title becomes `"opts out of herdr integration and Keitora's global agent
   writers (they write the real ~/.claude, ~/.codex and ~/.gemini)"`; the assertion
   `herdrDevEnv({}, opts).KEITORA_HERDR_NO_INTEGRATION === "1"` is unchanged.
3. Each Keitora writer has a guard test showing it refuses under the marker (listed in docs 03/04:
   `apply_trust_at_refuses_when_not_allowed`, `apply_hooks_at_refuses_when_not_allowed`,
   `allowed_with_refuses_when_marker_set`). No new marker or env var is added.
4. Procedure note (build-plan entry, "Still owed"): no dev-isolated run happens on the user's machine without
   first checking that `KEITORA_HERDR_NO_INTEGRATION` reaches the app process and that `~/.gemini` has no new
   files afterwards.

### Docs

- `README.md`:
  - `:15-16`: "…terminal running `claude`, `codex` or `agy` (Antigravity)." (keep the wording style of the
    sentence; the `(or …)` becomes a three-item list).
  - `:19`: "Keitora gets these states from the agents' hooks (Claude Code and Codex hooks, and Antigravity's
    global hook), which it installs for you."
  - `:37-41`: the install section says Claude Code is required and Codex and Antigravity are optional; add the
    Antigravity install pointer as "install the Antigravity CLI (`agy`) with its official installer" and links
    `https://antigravity.google/product/antigravity-cli/` (README Open issue 4 resolution), and the same sentence that a fresh
    `agy` install is noticed without restarting Keitora.
  - `:107`, `:109`: "session" wording gains "(or its Codex / Antigravity equivalent)"; `:109` says hooks are
    installed for each agent, and that Antigravity's hook lives in the global `~/.gemini/config/hooks.json` and
    only acts inside Keitora panes.
  - `:119`: "Check that the worktree's agent (`claude`, `codex` or `agy`) runs in a terminal opened in that
    worktree."
- `CONTRIBUTING.md:38`: "agent hooks into your real `~/.claude`, `~/.codex` or `~/.gemini`".
- `docs/windows-backend.md:22`: add a bullet after it: "**`agy`** (optional): installed by the official
  PowerShell installer to `%LOCALAPPDATA%\agy\bin\agy.exe`; Keitora checks that folder directly because the
  installer does not update the PATH of running processes." `:64`: "(then the configured
  `claude`/`codex`/`agy` directory)".
- `docs/build-plan.md`: add, after the last phase entry (the bullet starting at `:581`), an entry
  `- **#0077 — Antigravity CLI as a third agent provider. ✅ done (PR #N).**` (the plan number belongs in the
  docs, not in code or commit messages) with sub-bullets for: the provider table and keyed wire shapes; discovery,
  trust seeding and resume; the global hook, `keitora session --stdin-payload --if-fully-idle` and `blocked` ->
  `waiting` for all providers; updates and doctor; agent switching; UI; dev isolation. End with a "Still owed"
  list: live `agy` smoke test (needs an Antigravity sign-in first; interactive trust prompt with a seeded `trustedWorkspaces` string entry; global
  hook firing in the interactive TUI; `agent_session.source == "herdr:antigravity_cli"`; `agy -c` behaviour with
  no history; a pending-update launch), the `agy` prompt-chrome detector (doc 07), real-app smoke of the switch
  flow, macOS/Linux discovery, and the dev-isolated run check above. Add to the agent CLI updates entry
  (`:535-557`) one trailing line: "Antigravity joined the same machinery later (release manifest, `agy update`);
  see its entry."
- Release notes (when the release is cut): "Configs that name `antigravity` do not parse in older Keitora
  versions; downgrading after using Antigravity needs those entries removed." (Downgrade risk, doc 02.)
- Follow-up task for conversation transfer to/from Antigravity (summary handoff through the existing
  `inject_handoff_summary` path, possibly import later over `agy acp`): file it with the task CLI as
  `keitora task new "Antigravity conversation transfer" -d "Summary handoff via inject_handoff_summary, possibly
  import over agy acp; switching already exists." -p normal` (verified against `TaskCmd::New` in
  `crates/keitora-cli/src/main.rs:58-67`: positional title, `-d/--desc`, `-p/--priority`, `-t/--tags`).
- `.gitignore` is unchanged.

## Tests required

- `scripts/dev-herdr.test.ts`: the renamed test passes; add `it("never lets an isolated run clear the
  integration opt-out when the parent env sets it empty")`: `herdrDevEnv({ KEITORA_HERDR_NO_INTEGRATION: "" },
  opts).KEITORA_HERDR_NO_INTEGRATION === "1"` (guards against an inherited empty value turning the guard off).
- Rust guard tests from docs 03/04 (`global_writes`, trust writer, hooks writer) run in `cargo test
  --workspace`.
- A docs check (no code): `git grep -n "~/.claude" -- README.md CONTRIBUTING.md docs` shows no sentence that
  names `~/.claude` / `~/.codex` as the only global agent homes without `~/.gemini`.
- `git grep -nE '#[0-9]{3,4}' -- <changed source files>` is empty (AGENTS.md rule); `docs/build-plan.md` is
  exempt only because it is a plan document.

## Cross-references

- 02 (downgrade risk), 03 (trust writer, `global_writes`), 04 (hook writer, install triggers), 06 (transfer
  follow-up), 07 (prompt-chrome entry in the docs).
