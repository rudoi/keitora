# 05 — Agent updates and doctor

## Goal

Antigravity appears in the agent-update machinery (version probe, latest version, install kind, update
command) and in the doctor, with the update snapshot keyed by provider.

## Context

- Verified facts (`facts.md`):
  - `agy --version` prints exactly `1.5.18` (a bare semver) on stdout and exits 0.
  - `agy` self-updates in the background on every launch; no blocking update prompt was seen (unverified for a
    genuinely pending update).
  - `agy update` exists and takes no flags.
  - Release manifest: `GET https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/<platform>.json`
    returns `{"version":"1.5.18","url":"…","sha512":"…"}`. Platform keys: `windows_amd64`, `windows_arm64`,
    `darwin_amd64`, `darwin_arm64`, `linux_amd64`, `linux_arm64`, plus `linux_<arch>_musl` on musl. A hyphenated
    key (`windows-amd64`) returns 404.
  - Install locations: Windows `%LOCALAPPDATA%\agy\bin\agy.exe`; unix `~/.local/bin/agy`.
- Out of scope: an in-app installer for `agy`; the doctor shows the official install command instead.
- From doc 01: `CliKind::ALL`, `ProviderSpec`, and the decision that `AgentUpdatesSnapshot` becomes a map keyed
  by provider. From doc 02: `UpdatesConfig.agent_skip_antigravity`.
- Step 5 of the implementation outline.
- Verified code state (before this work), all under `src-tauri/src` unless noted:
  - `agent_updates/model.rs`: `CliInfo` `:74-85`, `CLAUDE_INFO` `:87`, `CODEX_INFO` `:97`, `info()` `:106`,
    `cli_title()` `:114`, `normalize` `:~118` (lower-cases and `/`-separates), `classify_path` `:~145-172`
    (the native rule is `path.starts_with("{home}/{native_dir}")` on the normalized, lower-cased path, plus
    `{home}/.local/bin/<herdr_kind>[.exe]`), `update_commands` `:~215-250`,
    `parse_claude_version_output` `:372` (test use `:590`).
  - `agent_updates/probe.rs`: `latest_with` `:40-56`, `CLAUDE_VERSION_CACHE` `:62`, `npm_fetch` `:77`,
    `MachineProber::installed` `:~108-128`, `invalidate` `:~130-139`, `latest` `:~141`.
  - `agent_updates/mod.rs`: `AgentUpdatesSnapshot` `:~121-135` (fields `codex`, `claude`, `checked_at`,
    `stopped`, `running`), `Default` `:~136-146`, `entry_mut` / `entry` `:~148-160`, `skipped_version` `:~206`,
    `probe_entry` `:~263`, `check` `:316-335`.
  - `agent_updates/commands.rs`: `agent_update_skip` `:48-58`, the `match cli` at `:51-54`.
  - `deps/doctor.rs`: `DoctorRow` `:33-42`, `herdr_row` `:57`, `git_row` `:96`, `cli_row` `:121`, `rows`
    `:~150-170` (four-element probe array). `deps/service_tests.rs:1471-1478` asserts the tool order
    `["herdr","git","claude","codex"]`.
  - Frontend: `ipc.ts` `DoctorRow` `:1391`, `AgentUpdatesSnapshot` `:1490`; `agentUpdates.ts` `bannerRows`;
    `AgentUpdatesProvider.tsx:294-298` builds `versions` from `snapshot.codex` / `snapshot.claude`;
    `lib/deps.ts:470-535` `settingsModel`; `components/DepsSection.tsx:108`.

## Files to touch

- `src-tauri/src/agent_updates/model.rs`, `probe.rs`, `mod.rs`, `commands.rs`
- `src-tauri/src/deps/doctor.rs`, `src-tauri/src/deps/service_tests.rs`
- `src-tauri/src/codex_env.rs` (only if `probe_cli_version`'s parser type needs the renamed function; it takes a
  `fn(&str) -> Option<String>`, so no change is expected)
- `src/lib/ipc.ts`, `src/lib/agentUpdates.ts`, `src/lib/agentUpdates.test.ts`, `src/lib/deps.ts`,
  `src/lib/deps.test.ts`, `src/components/AgentUpdatesProvider.tsx`, `src/components/DepsSection.tsx`
- `.harness/app.tsx` (untracked mock harness; doc 07 lists the edit)

## Design

### Version parsing

Rename `parse_claude_version_output` to `parse_first_semver` (definition `model.rs:372`, test call `:590`,
`probe.rs:125`). Behaviour unchanged: first token starting with `N.N.N` on the first non-empty line. Update the
doc comment to mention both `claude --version` (`"2.1.328 (Claude Code)"`) and `agy --version` (`"1.5.18"`).
`MachineProber::installed` (`probe.rs`) gains an arm:

```rust
CliKind::Antigravity => codex_env::cached_version_with(
    AGY_VERSION_CACHE.get_or_init(Default::default), exe, path.as_deref(), Instant::now(),
    &|exe, path| codex_env::probe_cli_version(exe, path, model::parse_first_semver)),
```

with `static AGY_VERSION_CACHE: OnceLock<VersionCache> = OnceLock::new();` next to `CLAUDE_VERSION_CACHE`
(`probe.rs:62`), and `invalidate` gains `CliKind::Antigravity => codex_env::invalidate_version(AGY_VERSION_CACHE
.get_or_init(Default::default), exe)`.

### Latest version: `ReleaseManifest` source

In `probe.rs`:

```rust
const AGY_MANIFEST_BASE: &str = "https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests";

/// Manifest platform key for `std::env::consts::{OS, ARCH}` values; `None` for anything unsupported.
/// windows+x86_64 -> windows_amd64, windows+aarch64 -> windows_arm64, macos+x86_64 -> darwin_amd64,
/// macos+aarch64 -> darwin_arm64, linux+x86_64 -> linux_amd64, linux+aarch64 -> linux_arm64.
pub fn manifest_platform_key(os: &str, arch: &str) -> Option<&'static str>;
/// `<AGY_MANIFEST_BASE>/<platform>.json` for this machine.
pub fn manifest_url() -> Option<String>; // manifest_platform_key(consts::OS, consts::ARCH).map(...)
```

musl is not detected (the glibc key gives the same version).

`latest_with` (`probe.rs:40`) keeps its signature `(cli, version_json, now, fetch)` and gains an early arm for
Antigravity: `CliKind::Antigravity => return manifest_url().and_then(|u| fetch(&u)).and_then(|b|
model::parse_manifest_latest(&b))`. (The `let url = match cli { … }` becomes `let url: &str`; the new arm
returns before it.) The manifest fetch reuses `npm_fetch` (it sends no user data and sets
`Accept: application/json`); `latest()` already passes `&npm_fetch`.

New in `model.rs`: `pub fn parse_manifest_latest(body: &str) -> Option<String>` — parse JSON, take string field
`version`, return `parse_first_semver(version)` (so it is validated as a semver).

### Install kind and update command

`CliInfo` (`model.rs:74`) gets the Antigravity entry:

```rust
const ANTIGRAVITY_INFO: CliInfo = CliInfo {
    title: "Antigravity",
    // Lower-case: `classify_path` compares against the normalized (lower-cased) path.
    native_dir: if cfg!(windows) { "appdata/local/agy/bin/" } else { ".local/bin/" },
    npm: "",
    brew: "",
    native_update: "agy update",
    native_update_runs: true, // takes no flags and is non-interactive
};
```

`info()` (`:106`) gains `CliKind::Antigravity => &ANTIGRAVITY_INFO`; `cli_title` should return
`cli.spec().title` (keep the function; the doc says its title comes from the spec) and the three `title:`
fields are deleted or kept consistent by a test.

`classify_path` already treats `<home>/.local/bin/<herdr_kind>[.exe]` as `Native`, which covers
`~/.local/bin/agy` on unix and Windows; the `native_dir` rule above covers
`%LOCALAPPDATA%\agy\bin\agy.exe` (`C:/Users/x/AppData/Local/agy/bin/agy.exe` normalizes to
`c:/users/x/appdata/local/agy/bin/agy.exe`, which starts with `c:/users/x/appdata/local/agy/bin/`). On unix
`native_dir` `".local/bin/"` is safe because the only binary Keitora classifies for this provider is `agy`.

`update_commands` (`model.rs:~215`): the existing `match kind` chooses `(info.npm | info.brew |
info.native_update | info.npm, runs)`. Add, immediately after the `let (cmd, runs) = …`, an early return:
`if cmd.is_empty() { return UpdateCommands { run: None, guidance: install_hint(cli).unwrap_or_default().to_string() }; }`.
So for Antigravity: `Native` -> run `agy update` (program `agy`, args `update`); `Npm`, `Brew` and `Unknown` ->
no run, guidance is the official install command. Add

```rust
/// Official install one-liner for a CLI Keitora does not install itself, for this OS. `Some` only for Antigravity.
pub fn install_hint(cli: CliKind) -> Option<&'static str>;
const AGY_INSTALL_HINT_WINDOWS: &str = "irm https://antigravity.google/cli/install.ps1 | iex";
const AGY_INSTALL_HINT_UNIX: &str = "curl -fsSL https://antigravity.google/cli/install.sh | bash";
```

returning the Windows constant when `cfg!(windows)` else the unix one. The values are the official installer
commands, verified by downloading both scripts on 2026-11-26 (README Open issue 4 resolution). They are not in
`facts.md`.

### Startup update screen

None was seen, so no detector. If the smoke test finds one, add a detector next to `is_codex_update_screen`
(`agent.rs:~128`) and extend `codex_update_prompt` (`herdr/mod.rs:~826`) accordingly.

### Snapshot keyed by provider

`mod.rs`:

- `pub struct AgentUpdatesSnapshot { pub agents: BTreeMap<CliKind, AgentUpdateEntry>, pub checked_at:
  Option<String>, pub stopped: Vec<StoppedSession>, pub running: Option<CliKind> }` (derive set unchanged).
  `BTreeMap` needs `CliKind: Ord` (doc 01). JSON: `{"agents":{"claude":{…},"codex":{…},"antigravity":{…}},
  "checked_at":…,"stopped":[…],"running":…}`.
- `Default` fills `agents` with `AgentUpdateEntry::unchecked(cli)` for every `cli in CliKind::ALL`.
- `entry(&self, cli) -> &AgentUpdateEntry` is `&self.agents[&cli]`; `entry_mut` is `self.agents.get_mut(&cli)`
  with `.expect("every provider has an entry")` (the map is always fully populated by `Default` and `check`).
- `skipped_version` (`:206`) gains `CliKind::Antigravity => cfg.updates.agent_skip_antigravity.as_deref()`.
- `check` (`:316-335`) probes every provider concurrently:
  `thread::scope(|s| { let handles: Vec<_> = CliKind::ALL.iter().map(|&cli| s.spawn(move || self.probe_entry(cli,
  &cfg))).collect(); handles.into_iter().map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
  .collect::<Vec<_>>() })`, then stores each result by `entry.cli` and logs `[agent-updates] checked:` with one
  `{cli:?} {state:?}` per provider.
- `commands.rs:51-54` (`agent_update_skip`): the `match cli` gains `CliKind::Antigravity =>
  cfg.updates.agent_skip_antigravity = Some(version)`; update the doc comment (`[updates]
  agent_skip_{codex,claude,antigravity}`).

Frontend:

- `ipc.ts:1490`: `AgentUpdatesSnapshot { agents: Record<CliKind, AgentUpdateEntry>; checked_at: string | null;
  stopped: StoppedSession[]; running: CliKind | null }`.
- `agentUpdates.ts`: `bannerRows` reads `snapshot.agents[cli]` while iterating `CLI_KINDS` (doc 01); the
  "Codex first" comments at `:17` and `:62` become "providers in `CLI_KINDS` order".
- `AgentUpdatesProvider.tsx:294-298`: `versions` becomes
  `useMemo(() => Object.fromEntries(CLI_KINDS.map((c) => [c, snapshot?.agents[c]?.installed ?? undefined])) as
  Partial<Record<CliKind, string>>, [snapshot])` (the context type at `:75` already is
  `Partial<Record<CliKind, string>>`).
- Any other reader of `snapshot.codex` / `snapshot.claude` (grep `\.codex\b|\.claude\b` in `src/`) switches to
  `snapshot.agents[…]`.

### Doctor

- `deps/doctor.rs`:
  - `DoctorRow` gains `pub hint: Option<String>` (doc: "install command shown for a missing agent CLI that
    Keitora does not install itself; Antigravity only"). Every existing `DoctorRow { … }` literal
    (`herdr_row` `:62,86,…`, `git_row` `:111`, `cli_row` `:128,138`) gains `hint: None` except the missing
    branch of `cli_row`, which sets `hint: crate::agent_updates::install_hint(cli).map(String::from)`.
  - `cli_row`'s `tool` match gains `CliKind::Antigravity => "antigravity"` (Claude `"claude"`, Codex
    `"codex"`). Update the `tool` doc comment to list `"antigravity"`.
  - `rows()` builds its probe list from `CliKind::ALL` instead of two literal lines:
    `s.spawn(|| herdr_row(inner)), s.spawn(|| git_row(inner))` then one `s.spawn(|| cli_row(inner, cli, cfg))`
    per `cli in CliKind::ALL`, collected into a `Vec` (the array of four becomes a vector of five handles), in
    that order: herdr, git, claude, codex, antigravity.
  - Update the module doc (`:1-2`) to list the three agent CLIs.
- `deps/service_tests.rs:1471-1478`: expected tools `["herdr","git","claude","codex","antigravity"]`; the
  existing assertions on rows 0-3 are unchanged.
- `ipc.ts:1392` `tool: "herdr" | "git" | "claude" | "codex" | "antigravity"`; add `hint: string | null` to
  `DoctorRow`.
- `lib/deps.ts`: `SettingsRow` gains `hint: string | null` (`:470-477`); `settingsModel` copies `row.hint` into
  every produced row (`base` at `:515` gains `hint: row.hint`); `rowDetail` is unchanged (a missing agent is
  `"not found"`, tone `muted`).
- `components/DepsSection.tsx:108` prints `toolTitle(row.tool)` (doc 01) and, under the path line, when
  `row.hint` is set and `row.detail === "not found"`, renders
  `<span className="break-all font-mono text-[10.5px] text-tx-3 select-text">{row.hint}</span>`.
- `deps.test.ts` fixtures (`:408-505`) gain `hint: null` on every `DoctorRow` literal.

### Config

`agent_skip_antigravity` is defined in doc 02 and wired in `commands.rs` above.

## Tests required

`agent_updates/model.rs`:

- `parse_first_semver_reads_agy_output` — `"1.5.18\n"` -> `Some("1.5.18")`; `"1.5.18"` -> same; `""` -> `None`;
  `"agy"` -> `None`. The existing Claude fixtures (`"2.1.328 (Claude Code)"`) still pass under the new name.
- `parse_manifest_latest_reads_version` — `{"version":"1.5.18","url":"u","sha512":"s"}` -> `Some("1.5.18")`.
- `parse_manifest_latest_rejects_bad_bodies` — `{}` , `{"version":3}`, `{"version":"x"}` and `not json` -> `None`.
- `install_kind_native_for_agy_windows_path` — `install_kind(Antigravity,
  "C:\\Users\\x\\AppData\\Local\\agy\\bin\\agy.exe", same, Some("C:\\Users\\x"))` is `Native` when
  compiled for Windows (guard the case with `#[cfg(windows)]`) and the
  `~/.local/bin/agy` case `install_kind(Antigravity, "/home/u/.local/bin/agy", same, Some("/home/u"))` is `Native`
  on every OS.
- `agy_has_no_npm_or_brew_update` — `update_commands(Antigravity, Npm, None).run == None`, same for `Brew` and
  `Unknown`, and each `guidance` equals `install_hint(Antigravity).unwrap()`.
- `agy_native_update_runs_agy_update` — `update_commands(Antigravity, Native, None).run` has `program ==
  "agy"`, `args == "update"`, `program_is_path == false`, and `guidance == "agy update"`.
- `cli_titles_come_from_the_spec` — for every `cli in ALL`, `cli_title(cli) == cli.spec().title`.
- `install_hint_is_only_for_antigravity` — `install_hint(Claude) == None`, `install_hint(Codex) == None`,
  `install_hint(Antigravity)` is `Some` and non-empty.

`agent_updates/probe.rs`:

- `manifest_platform_key_maps_supported_targets` — the six (os, arch) pairs above map to the six keys;
  `("windows","x86")`, `("freebsd","x86_64")` -> `None`.
- `agy_latest_reads_the_manifest_only` — `latest_with(Antigravity, || None, now(), &fetch)` where `fetch`
  records URLs and returns the manifest body: result `Some("1.5.18")`, exactly one URL fetched, it starts with
  `AGY_MANIFEST_BASE` and ends with `.json`; the version-json closure is never called.
- `agy_latest_is_none_when_the_fetch_fails` — `fetch` returns `None` -> `None`.

`agent_updates/mod.rs` (fake prober, existing rig):

- `snapshot_default_has_an_unchecked_entry_per_provider` — `Default` has keys claude, codex, antigravity, each
  `AgentUpdateState::Unchecked`.
- `check_probes_every_provider` — a fake prober that reports `agy` installed `1.5.17` with latest `1.5.18`
  produces `agents[Antigravity].state == Available { installed: "1.5.17", latest: "1.5.18", command:
  Some("agy update") }` when its exe is `/home/u/.local/bin/agy`; with an unknown path the state is `Available`
  with `command: None` and `guidance` equal to the install hint.
- `skip_changed_uses_the_antigravity_skip` — with `agent_skip_antigravity = Some("1.5.18")` the entry becomes
  `Skipped { latest: "1.5.18" }`.
- `snapshot_wire_shape_is_keyed_by_provider` — `serde_json::to_value(snapshot)["agents"]` is an object with
  exactly the keys `claude`, `codex`, `antigravity`, and each value's `cli` equals its key (replaces the
  assertion at `:875`, `statuses[0]["claude"]["cli"]` -> `statuses[0]["agents"]["claude"]["cli"]`). Existing
  tests that read `snap.codex` / `snap.claude` (`:843-886` and later) use `snap.agents[&CliKind::…]`.

`agent_updates/commands.rs`: `agent_update_skip_persists_antigravity` (config round trip on a temp registry).

`deps/service_tests.rs`: `doctor_reports_each_tool` updated as above, plus `doctor_antigravity_row_missing_has_a_hint`
— with no discoverable `agy` the fifth row has `status == Missing`, `path == None` and `hint == Some(…)` equal to
`install_hint(Antigravity)`; the Claude and Codex rows have `hint == None`.

TypeScript:

- `agentUpdates.test.ts`: `bannerRows` order is claude, codex, antigravity for a snapshot where all three are
  `available`; `bannerRows` copy for Antigravity reads `Antigravity 1.5.18 is available (you have 1.5.17)`.
- `deps.test.ts`: `settingsModel` row for a missing `antigravity` has `detail "not found"`, `tone "muted"` and
  carries `hint`.
- A wire-shape test (`ipc.test.ts`) that a JSON literal in the Rust shape type-checks as `AgentUpdatesSnapshot`.

## Cross-references

- 01 (spec, snapshot map, `CLI_KINDS`, `toolTitle`), 02 (`agent_skip_antigravity`), 07 (UI "agent cli updates"
  list renders `cliTitle`; `.harness` snapshot shape).
