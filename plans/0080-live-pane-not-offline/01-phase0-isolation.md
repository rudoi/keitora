# Phase 0: isolation hardening

Part of 0080; see `00-overview.md`. Prerequisite for every live spike (`spikes.md`, Spike 0).

## Purpose

Live spikes must never touch the user's running herdr. Phase 0 makes the isolated launcher
(`yarn dev:isolated`, `scripts/dev-herdr.mjs`) and the app's own herdr spawns drop every
inherited `HERDR_*` variable (these are what let a process started from inside a herdr pane
reach the *outer* server whatever `XDG_*` says), and re-verifies isolation immediately before
the app starts.

## Verified facts (checked in code and in the herdr 0.9.5 binary)

- `scripts/dev-herdr.mjs` exports `herdrDevEnv(base, { keitoraHome, configHome, herdrBin })`
  (about line 329). It spreads `base` and then overrides `KEITORA_*`, `XDG_*` and
  `HERDR_CONFIG_PATH`. It removes nothing, so every inherited `HERDR_*` flows through to
  `spawn(appBin, ...)`, and from there to the herdr server and CLI calls.
- `isolationVerdict(status, herdrHome, platform)` (about line 396) and
  `mayStopOnTeardown(status, herdrHome, platform)` (about line 422) are pure and exported.
  `herdrStatus(herdrBin, env)` (about line 428) is private and returns the parsed
  `herdr status server --json`, or `null`.
- In `main()` the verdict runs once, before any state is purged (about line 539). Then come
  the registry seeding, `cargo build -p keitora -p keitora-cli` (minutes) and the Vite wait.
  `spawn(appBin, ...)` (about line 643) follows with no further check. That gap is the bug
  the re-check closes.
- `src-tauri/src/backend/herdr/env.rs`: `HerdrEnv::apply` (about line 664) calls
  `cmd.env_remove` for each name in `HerdrEnv.env_remove`, then sets `HerdrEnv.env` and `PATH`.
  `env_remove` is built in `HerdrEnv::with_options` (about line 642) from contaminated
  pass-through `XDG_*` names only. Nothing removes `HERDR_*`. The current doc claim that
  `apply` "removes `HERDR_SOCKET_PATH`/`HERDR_SESSION`" described the intended change, not
  existing behaviour.
- `HerdrEnvOptions` (about line 167) has `from_process()` (production) and `from_lookup()`
  (`#[cfg(test)]`, pure). Both must fill any new field.
- The `HERDR_*` names present in the 0.9.5 `herdr.exe` strings: `HERDR_SOCKET_PATH`,
  `HERDR_CLIENT_SOCKET_PATH`, `HERDR_SESSION`, `HERDR_ENV`, `HERDR_WORKSPACE_ID`,
  `HERDR_TAB_ID`, `HERDR_PANE_ID`, `HERDR_BIN_PATH`, `HERDR_CONFIG_PATH`, `HERDR_HOME`,
  `HERDR_STARTUP_CWD`, `HERDR_REATTACH_COMMAND`, `HERDR_CHANNEL`, `HERDR_LOG`,
  `HERDR_INSTALL_DIR`, plus plugin and debug names (`HERDR_PLUGIN_*`, `HERDR_WINDOWS_*`, ...).
  `research-herdr.md` item 7 confirms every pane exports the first nine. Nothing in
  `src-tauri` or `crates` reads any `HERDR_*` name except `HERDR_CONFIG_PATH`, which Keitora
  sets itself.
- Tests: `yarn test` runs `vitest run`, which already picks up `scripts/dev-herdr.test.ts`
  (an ES-module test importing `./dev-herdr.mjs`). One file: `yarn vitest run scripts/dev-herdr.test.ts`.
  Rust: `cargo test -p keitora backend::herdr::env` (check the package name in
  `src-tauri/Cargo.toml` first; `cargo build -p keitora` is what the launcher uses).

## Design

### What to strip

Every environment variable whose name starts with `HERDR_` (ASCII case-insensitive, because
Windows env names are case-insensitive), **except `HERDR_CONFIG_PATH`**, which Keitora sets
itself right afterwards. A prefix rule is used instead of a fixed list so a variable a future
herdr exports cannot leak. `KEITORA_HERDR_BIN` and `KEITORA_HERDR_NO_INTEGRATION` do not start
with `HERDR_` and are untouched.

### Where

1. **Launcher (`scripts/dev-herdr.mjs`).**
   - New pure export `inheritedHerdrVars(base)`: returns the sorted names in `base` that
     match `/^HERDR_/i` and are not `HERDR_CONFIG_PATH` (case-insensitive). Names only.
   - `herdrDevEnv` builds its result from `base` with those keys deleted first, then applies
     the existing overrides. It must not mutate `base` (an existing test asserts this).
   - `main()` prints one line after computing `isolatedEnv`:
     `[dev:herdr] stripped inherited herdr env: NAME1, NAME2` (names only, never values; skip
     the line when empty).
2. **App (`env.rs`).**
   - New pure helper `herdr_var_names(names: impl IntoIterator<Item = OsString>) -> Vec<String>`:
     UTF-8 names starting with `HERDR_` (ASCII case-insensitive), excluding `HERDR_CONFIG_PATH`
     (case-insensitive), sorted, deduplicated.
   - New field `HerdrEnvOptions.inherited_herdr: Vec<String>`. `from_process()` fills it with
     `herdr_var_names(std::env::vars_os().map(|(k, _)| k))`. `from_lookup()` fills it with
     an empty `Vec` (a test builds its own).
   - `with_options` appends `opts.inherited_herdr` to `env_remove`, after the `XDG_*` names.
     `HerdrEnv::apply` then removes them from every herdr server, CLI and bridge command with
     no further change (it already loops `env_remove` before setting `env`).
   - Update the doc comment of `HerdrEnv.env_remove` ("contaminated `XDG_*` values and every
     inherited `HERDR_*` name").
3. **Pane env is untouched.** herdr itself sets `HERDR_*` for each pane; the server process
   env is what gets cleaned.

### Re-check before launching the app

In `main()`, immediately before `const app = spawn(appBin, ...)` (after the Vite wait), run
the same check again with the same `herdrStatus(herdrBin, isolatedEnv)` call:

```js
const recheck = isolationVerdict(herdrStatus(herdrBin, isolatedEnv), herdrHome);
if (!recheck.ok) {
  console.error(`[dev:herdr] refusing to launch (re-check): ${recheck.reason}.`);
  console.error("[dev:herdr] the app was not started; nothing was stopped.");
  vite.kill();
  killPort(1420);
  process.exit(1);
}
```

`shutdown` is defined after this point and is deliberately not used: it would run the herdr
`server stop` path, and nothing of ours is running yet. `vite.kill()` and `killPort(1420)` are
the same two calls `shutdown` uses. Both verdicts pass only when no server is reachable yet
(`running: false`) and any reported socket lies under `<KEITORA_HOME>/herdr`.

## Files touched

- `scripts/dev-herdr.mjs` (`inheritedHerdrVars`, `herdrDevEnv`, `main` print + re-check)
- `scripts/dev-herdr.test.ts`
- `src-tauri/src/backend/herdr/env.rs` (`herdr_var_names`, `HerdrEnvOptions.inherited_herdr`,
  `with_options`, doc comments, existing tests below)

## Reports fixed

None directly; it gates the spikes that inform Phases 1-2, 4 and 5, and removes a likely cause
of "the isolated launcher adopts the real herdr on Windows".

## Regression tests

`scripts/dev-herdr.test.ts`, in `describe("herdrDevEnv")` unless noted:
- `strips every inherited HERDR_* variable, case-insensitively` (base has `HERDR_SOCKET_PATH`,
  `herdr_session`, `HERDR_PANE_ID`, `HERDR_CLIENT_SOCKET_PATH`; none survive).
- `replaces an inherited HERDR_CONFIG_PATH with the isolated one` (not removed, not kept).
- `leaves KEITORA_HERDR_BIN and unrelated variables alone` (`PATH`, `FOO`, and a name like
  `NOT_HERDR_X`).
- `does not mutate the base environment when stripping` (extend the existing no-mutation test
  or add one with a `HERDR_*` key).
- `describe("inheritedHerdrVars")`: `lists sorted names only` and `ignores HERDR_CONFIG_PATH`
  and non-prefixed names.
- The pre-spawn re-check lives in `main()` and is not unit-testable; it is covered by Spike 0
  step 5 and by reading the code. `isolationVerdict`'s own tests already cover the verdict.

`env.rs` (`mod tests`):
- `herdr_var_names_selects_prefixed_names_only` (case-insensitive, sorted, deduped, skips
  `HERDR_CONFIG_PATH`, skips a non-UTF-8 name).
- `with_options_removes_inherited_herdr_vars_from_every_command`: build `HerdrEnvOptions` via
  `opts(vec![])`, set `o.inherited_herdr = vec!["HERDR_SESSION".into(), "HERDR_SOCKET_PATH".into()]`,
  call `apply` on a `Command`, and assert both appear in `get_envs()` with a `None` value.
- `apply_never_removes_the_pinned_config_path`: with `HERDR_CONFIG_PATH` in `inherited_herdr`
  by mistake it is still `Some(..)` in the command (guards the exclusion).
- Update existing tests that this change affects:
  - `with_options_sets_three_keys_and_creates_only_config_and_state` asserts
    `env.env_remove == ["XDG_DATA_HOME", "XDG_RUNTIME_DIR"]`; it stays true because
    `opts(...)`'s `inherited_herdr` is empty, so no edit is needed. Verify.
  - `apply_sets_pairs_and_removes_names` asserts `got.len() == 4`; same reasoning, still
    true. Verify.

## Dependencies

None. Must land before any live spike runs (`spikes.md`). If Spike 0 still fails after it lands,
all live spikes stop and Phases 1-2 are built on the fake backend.

## Singletonize decisions

- **Prefix rule over a fixed list.** The launcher and the app both strip by `HERDR_` prefix
  (minus `HERDR_CONFIG_PATH`). The binary exports plugin and debug variables the plan did
  not list; a list would rot with each herdr release.
- **Strip at the server-process level only** (`HerdrEnv::apply` and the launcher env). Pane
  env is herdr's to set.
- **Rust enumeration lives in `from_process`, not `apply`,** so `apply` stays a dumb loop and
  the helper is a pure function with a table test.
- **The re-check does not call `shutdown`.** Nothing of ours is running before the app
  starts, and `shutdown` would run `herdr server stop` on a server we cannot yet prove is ours.
- **Stripping is necessary but not proof.** The overview's note that herdr "does not honour
  the XDG isolation on every platform (Windows)" may have causes besides `HERDR_*`. The
  verdict checks and Spike 0 stay as the gate regardless.
