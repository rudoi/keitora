# 0077 — Antigravity CLI (`agy`) as a third agent provider: document index

The proposal is `plans/0077-antigravity-provider.md` (unchanged). Verified CLI facts are in `facts.md` in this
directory (`agy` 1.5.18, herdr 0.9.5, Windows, 2026-11-26). The docs below split the proposal along subsystem
boundaries; each is self-contained.

## Goal

Everywhere Keitora supports `claude` and `codex` as a worktree's agent, it also supports Google's Antigravity
CLI (`agy`): launch and resume, busy/waiting/idle state, doctor and agent updates, and its own settings
group. A worktree can be switched to or from Antigravity at any time; the conversation does not carry over
and the user is warned. Every hard-wired "claude or codex" becomes "one of the providers".

## Decisions (user, 2026-11-26)

1. **Hook location.** Hooks for Antigravity go in the global `~/.gemini/config/hooks.json`, and only act
   inside Keitora panes.
2. **Waiting state.** herdr's `blocked` status counts as `waiting` for every provider, Claude and Codex
   included.
3. **Workspace trust.** Keitora pre-seeds workspace trust for `agy`.
4. **Transfer and switching.** Transfer to/from Antigravity is out of scope; switching is in scope, with a
   warning.
5. **Test install.** The user installed `agy` for the spike and smoke test.
6. **Glyph.** Use the Antigravity mark.
7. **Naming.** "Antigravity" everywhere: config key `[antigravity]`, serde `"antigravity"`, UI label.

## Doc map

| Doc | Subsystem |
|---|---|
| `01-provider-model.md` | `CliKind::Antigravity`, `ProviderSpec`, `CliKind::ALL`, TS `providers.ts`, wire-shape maps |
| `02-persistence.md` | config structs, dispatch override, `config_edit` generics, registry `ProviderIds`, settings IPC, SQLite CHECK migration |
| `03-launch-resume-trust.md` | discovery, launch args, id-safety rename, availability refresh, trust seeding, herdr integration gating, resume, fake herdr |
| `04-session-state.md` | global hook merge, `KEITORA_BIN`, `keitora session --stdin-payload`, `HINT_WAITING`, install triggers, write guards |
| `05-updates-doctor.md` | version probe, release manifest, install kind, doctor row |
| `06-agent-switch.md` | transfer pair guard, `switch_worktree_cli`, menu model, reducer, overlay |
| `07-ui.md` | glyph, settings group, prompt chrome, harness |
| `08-isolation-docs.md` | dev isolation, docs, follow-up task |

## Cross-doc dependency order

1. 01 (everything depends on the provider model)
2. 02 (persistence; needs 01)
3. 03 (discovery, launch, trust, resume; needs 01, 02)
4. 04 (session state; needs 01, shares triggers and guards with 03)
5. 05 (updates and doctor; needs 01, 02)
6. 06 (switching; needs 01-04)
7. 07 (UI; needs 01, 02, 05, 06)
8. 08 (isolation and docs; last, verifies guards from 03/04)

## Risks (whole effort)

- Unverified facts: `trustedWorkspaces` element shape, herdr session source name, interactive hook trust.
  Each sits behind one module or a tolerant matcher; the smoke test checks each.
- Shared global files (`~/.gemini/config/hooks.json`, `settings.json`): name-keyed merge, skip unparseable,
  write only on change, `KEITORA_BIN` guard, isolated dev runs skip writes.
- `blocked` -> `waiting` changes Claude and Codex lanes (approved); reconcile tests pin the table.
- Downgrade breakage on `"antigravity"`: accepted, documented in release notes.
- Fast-moving CLI: arguments and facts in one table and one test matrix.
- Regressions in the generalization: guarded by existing transfer, herdr arg matrix, resume and hook
  installer suites.

## Complexity

Large: about 65 files across `keitora-tasks`, `keitora-cli`, `src-tauri`, the frontend, a DB migration, two
new global-file mergers, a new switch command and docs.

## Dependencies

- `agy` >= 1.5 installed on the dev machine (done).
- herdr >= 0.9.5 for `--kind agy` and `antigravity-cli` (already the floor).

## Conventions used by the docs

- Line numbers were verified against the code at the time of hardening and carry a leading `:`; treat them as
  hints and locate by symbol if they drifted.
- "Doc N" refers to the numbered file in this directory. Each doc lists its files to touch, exact new names and
  signatures, and test cases as `name — assertion`.
- Placeholder names only in tests and fixtures (AGENTS.md): `proj`, `alpha`, `/home/u`, `C:/Users/x`.

## Open issues

These are places where the proposal is silent, ambiguous or in tension with the code. Each doc follows the
"Chosen" default; confirm or overrule before implementing the affected doc.

1. **`agy -c` can still run as a fallback (doc 03).** The proposal says the no-stored-id fallback is `Fresh`
   because `agy -c` in a directory with no history is unverified. But `next_fallback`
   (`backend/herdr/agent.rs:102`) turns a failed `Resume` into `Continue`, which launches `agy -c`, and the
   launch-args table lists `Continue -> -c`. Chosen: leave `next_fallback` generic (Resume -> Continue -> Fresh)
   so a failed resume still degrades like Claude and Codex; the smoke test must confirm `agy -c` behaviour. If
   `-c` turns out to misbehave with no history, change `next_fallback` to skip `Continue` for Antigravity.
2. **Settings DTOs: user-level `dispatch_steps` (doc 02).** The proposal types both settings DTOs as
   `Record<CliKind, {path, autonomy, dispatch_steps}>`, but dispatch steps exist only at the project level
   (`DispatchConfig` is a `ProjectConfig` field; `UserConfig` has none). Chosen: the project DTO carries
   `{path, autonomy, dispatch_steps}` and the user DTO `{path, autonomy}`.
3. **`did not start` text is Codex-only today (doc 01).** The proposal has `spawnFailedCopy` and the backend's
   `"{} did not start"` (`strict.rs:384`) both use the spec's `binary`. The backend text comes from the strict
   launch, which applies to Codex only (`STRICT_CLI`), so an Antigravity launch never produces
   `agy did not start`; an Antigravity spawn failure falls into the generic `Failed to launch agy — is it on
   PATH?` line. Chosen: the frontend change (use `cliBinary`) is made as specified and is forward-compatible; no
   backend change.
4. **Official install one-liners are not in `facts.md` (docs 05, 08).** The doctor hint and the `Unknown`
   install guidance need the exact official `install.ps1` / `install.sh` commands, and the README install text
   wants the install page URL. `facts.md` records the script names and locations but not the URL. Chosen:
   `AGY_INSTALL_HINT_WINDOWS` / `AGY_INSTALL_HINT_UNIX` are constants the implementer fills from the official
   install page (the source of `install.ps1` / `install.sh`), and the README sentence omits a URL until then.
5. **herdr integration gate is evaluated once per process (doc 03).** `Supervisor::ensure_ready` is latched, so
   an `agy` installed after Keitora started does not get `antigravity-cli` installed until the next start or
   `refresh_integration` (which runs after a herdr upgrade). The proposal's availability refresh covers the UI
   only. Chosen: accept the limitation; session ids for such a worktree then come from the registry/`Fresh`
   fallback. A possible extension (not specified) is to call the backend's `refresh_integration()` from
   `switch_worktree_cli` and `save_user_settings` when the target or default is Antigravity.
6. **Antigravity glyph source (doc 07).** The Simple Icons set is not installed in the repo and `facts.md` does
   not say whether it has an Antigravity icon. Chosen: the implementer tries the pinned Simple Icons package
   first, else ships the documented simplified placeholder path and records it; the user may want to supply the
   real mark.
7. **`transfer_worktree` early guard loads the project twice (doc 06).** The guard needs the source cli before
   the long work, and the per-direction functions load the project again. Chosen: accept the duplicate load
   (a small file read) rather than refactor the transfer functions.
8. **Switch reducer phase mechanics (doc 06).** The proposal says `begin {mode: "switch"}` "enters `restarting`
   directly" (so the 120-second backstop applies) and that the `switched` outcome produces the restart effect,
   but the existing reducer ignores `outcome` events outside the `importing` phase. Chosen: a `mode` field on
   `TransferEntry`; a switch entry is "awaiting its outcome" while `restarting` with `result === null`, and
   `outcome` / `failed` are accepted in that state. No new phase value.
9. **Claude availability gating can strand a user (doc 07).** The proposal makes Claude's settings group show the
   "not found" note and disable its inputs when Claude is not discovered, and drops "Claude is never
   unavailable". A user whose `claude` is not discoverable and not yet configured then cannot type a path in the
   disabled input (Codex already behaves this way today). Chosen: follow the proposal; `available[cli]` counts a
   configured path as available (`resolve_provider_path(...).is_some() || discover_cli_cached(...)`), and
   `ensure_claude_path` persists the discovered Claude path at startup, so only a machine where Claude cannot
   be found at all is affected.
10. **`available` uses the cached discovery (doc 02).** Today `get_settings` calls the uncached `discover_cli`
    (a login-shell round trip on unix each time); the proposal's "30-second miss cache" requires
    `discover_cli_cached`, whose hits are kept for the process lifetime. Chosen: use the cached form; an `agy`
    that is uninstalled while Keitora runs keeps reading as available until restart.


## Open issue resolutions (orchestrator, auto-advance)

Decisions below override the "Chosen" defaults above where they differ; implementers follow these.

1. **Overruled.** For `CliKind::Antigravity`, `next_fallback` maps a failed `Resume` straight to `Fresh` (skip
   `Continue`), consistent with the no-id `Fresh` fallback. Claude/Codex chains are unchanged. Add an
   `agent.rs` test pinning the Antigravity chain.
2. Accepted (project DTO has `dispatch_steps`, user DTO does not).
3. Accepted.
4. **Resolved.** Constants (verified by downloading both scripts on 2026-11-26):
   - `AGY_INSTALL_HINT_WINDOWS = "irm https://antigravity.google/cli/install.ps1 | iex"`
   - `AGY_INSTALL_HINT_UNIX = "curl -fsSL https://antigravity.google/cli/install.sh | bash"`
   - Release manifest base: `https://antigravity-cli-auto-updater-974169037036.us-central1.run.app`.
   README may link `https://antigravity.google/product/antigravity-cli/`.
5. **Extended.** `switch_worktree_cli` (target Antigravity) and `save_user_settings` (`default_cli =
   antigravity`) call the backend's existing `refresh_integration()` path after the agy hook install, so a
   late `agy` install gets herdr's `antigravity-cli` integration without restart. Guarded by the same
   `cfg!(test)` / `NO_INTEGRATION_ENV` rules.
6. Accepted (try Simple Icons first, else a simplified single-path mark with its source noted).
7. Accepted.
8. Accepted (`mode` field on `TransferEntry`).
9. **Overruled.** Path inputs (user and project) stay **enabled for every provider regardless of
   availability** — typing a path is how a user fixes "not found". Only the autonomy toggle and dispatch
   editor are disabled when `available[cli]` is false. This relaxes today's Codex behaviour slightly; intended.
   `available[cli]` = configured path resolves OR cached discovery hits.
10. Accepted.
