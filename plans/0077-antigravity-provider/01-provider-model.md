# 01 — Provider model

## Goal

Replace every hard-wired "claude or codex" in Rust and TypeScript with "one of the providers". Add a third
`CliKind` variant, `Antigravity`, a per-provider spec table, and `CliKind::ALL`. After this doc, a fourth
provider is a table entry plus the arms the compiler flags. No behaviour change for Claude and Codex.

## Context

- Whole effort: Google's Antigravity CLI (`agy`) becomes a third agent provider next to `claude` and `codex`.
  Verified CLI facts are in `plans/0077-antigravity-provider/facts.md` (`agy` 1.5.18, herdr 0.9.5, Windows,
  2026-11-26).
- Decision 7: the name is "Antigravity" everywhere: config key `[antigravity]`, serde value `"antigravity"`,
  UI label "Antigravity".
- Decision 6: the glyph is the Antigravity mark (drawn in doc 07).
- Out of scope: Gemini CLI as a provider; conversation transfer to or from Antigravity (doc 06 covers
  switching); behaviour changes for Claude and Codex except `blocked` -> `waiting` (doc 04).
- This is step 1 of the implementation outline. When it lands, `cargo test --workspace` and `yarn test` must
  pass with no Claude or Codex behaviour change; the Antigravity arms may be inert stubs that later docs fill.
- Verified code state (before this work):
  - `CliKind` is at `crates/keitora-tasks/src/config.rs:72-78` with `#[derive(Debug, Clone, Copy, PartialEq,
    Eq, Hash, Default, Serialize, Deserialize)]`, `#[serde(rename_all = "snake_case")]`, `#[default] Claude`.
  - `herdr_kind(cli)` is `src-tauri/src/backend/herdr/agent.rs:20`; `agent_names_for` is `agent.rs:174`
    returning `[String; 2]`.
  - `cliTitle` and `AGENT_CLIS` live in `src/lib/agentUpdates.ts:18-23` and `cliLabel` in
    `src/lib/transferPhase.ts:221`.

## Files to touch

Rust:

- `crates/keitora-tasks/src/config.rs` (enum, `ALL`, `ProviderSpec`, `spec()`, tests)
- `crates/keitora-tasks/src/lib.rs` (re-export `ProviderSpec`, `CLI_COUNT`)
- `src-tauri/src/backend/herdr/agent.rs` (`herdr_kind`, `agent_names_for`, test at `:610-620`)
- `src-tauri/src/backend/herdr/keymap.rs` (`named_agent_pane` at `:852`; tests at `:1574,1592,1615,1637,1671`)
- `src-tauri/src/backend/herdr/mod.rs` (`evict_key_panes` at `:858-867`)
- `src-tauri/src/agent_updates/model.rs`, `src-tauri/src/agent_updates/mod.rs`, `src-tauri/src/deps/doctor.rs`
  (iterate `CliKind::ALL`; the full change is in doc 05, this doc only requires they compile)
- Every other non-test exhaustive `match` on `CliKind` the compiler flags. Known sites:
  `config.rs:2014,2022` (`project_provider`, `user_provider`), `config.rs:2558,2643` (`discover_cli`),
  `local_store.rs:45-63`, `dispatch.rs:236` (`select_ritual_steps`), `host.rs:927,1451-1461`,
  `agent_updates/mod.rs:149,156,208`, `agent_updates/commands.rs:52`, `agent_updates/probe.rs:47,118,133`,
  `agent_updates/model.rs:109`, `deps/doctor.rs:125`. Docs 02-05 give the Antigravity arm for each.

TypeScript:

- `src/lib/ipc.ts` (`CliKind` at `:23`)
- new `src/lib/providers.ts` and `src/lib/providers.test.ts`
- `src/components/ProviderGlyph.tsx`, `src/components/Rail.tsx`, `src/lib/agentUpdates.ts`,
  `src/components/SettingsDrawer.tsx`, `src/App.tsx`, `src/lib/laneState.ts`, `src/lib/transferPhase.ts`,
  `src/components/AgentUpdatesProvider.tsx`, `src/components/DepsSection.tsx`,
  `src/components/AgentUpdateBanner.tsx`, `src/components/AgentUpdateDialog.tsx`,
  `src/components/TerminalPane.tsx`, `src/components/TransferOverlay.tsx` (import changes only where a name moves)

## Design

### Rust: `CliKind`, `CLI_COUNT`, `ALL`, `ProviderSpec`

In `crates/keitora-tasks/src/config.rs`:

1. Add the variant `Antigravity` after `Codex`. Keep `#[default] Claude`. Add `PartialOrd, Ord` to the derive
   list. (Needed so `BTreeMap<CliKind, _>` works; the derived order is declaration order: Claude, Codex,
   Antigravity.)
2. Update the doc comment above the enum: bare TOML strings are `"claude"` / `"codex"` / `"antigravity"`.
3. Add exactly:

   ```rust
   /// Number of providers; the length of [`CliKind::ALL`].
   pub const CLI_COUNT: usize = 3;

   /// Static facts about one provider.
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub struct ProviderSpec {
       pub cli: CliKind,
       /// Display name: "Claude" / "Codex" / "Antigravity".
       pub title: &'static str,
       /// Executable name: "claude" / "codex" / "agy".
       pub binary: &'static str,
       /// herdr `agent start --kind` value: "claude" / "codex" / "agy".
       pub herdr_kind: &'static str,
       /// herdr `integration install` target: "claude" / "codex" / "antigravity-cli".
       pub herdr_integration: &'static str,
       /// Agent names herdr may report in `agent_session.agent`.
       pub herdr_agent_names: &'static [&'static str],
   }

   impl CliKind {
       /// Display order, used everywhere providers are listed.
       pub const ALL: [CliKind; CLI_COUNT] = [CliKind::Claude, CliKind::Codex, CliKind::Antigravity];
       pub fn spec(self) -> &'static ProviderSpec { &SPECS[self as usize] }
   }
   ```

   `SPECS: [ProviderSpec; CLI_COUNT]` is a private `static` indexed by discriminant (declaration order, so
   Claude = 0, Codex = 1, Antigravity = 2). Values:

   | cli | title | binary | herdr_kind | herdr_integration | herdr_agent_names |
   |---|---|---|---|---|---|
   | Claude | `Claude` | `claude` | `claude` | `claude` | `["claude"]` |
   | Codex | `Codex` | `codex` | `codex` | `codex` | `["codex"]` |
   | Antigravity | `Antigravity` | `agy` | `agy` | `antigravity-cli` | `["antigravity_cli", "antigravity-cli", "agy"]` |

4. Re-export `ProviderSpec` and `CLI_COUNT` from `crates/keitora-tasks/src/lib.rs` in the existing
   `pub use config::{ … }` list (it already re-exports `CliKind`).
5. Launch arguments stay code in `build_agent_args_opts` (doc 03); they are not in the spec.

### Rust: code that iterates providers

- `src-tauri/src/backend/herdr/agent.rs`:
  - `pub fn herdr_kind(cli: CliKind) -> &'static str { cli.spec().herdr_kind }` (keep the function; many
    callers use it, including `agent_updates/model.rs:10` and `strict.rs`).
  - `pub fn agent_names_for(key: &SessionKey) -> [String; CLI_COUNT]` returning
    `CliKind::ALL.map(|c| agent_name(c.spec().herdr_kind, key))`. Callers: `herdr/mod.rs:860`
    (`evict_key_panes`; it passes `&names` to `close_name_holders(…, names: &[String])` at `:872-877`, which
    accepts the 3-element array unchanged), `herdr/mod.rs:7785` (a test) and `keymap.rs:852` (`names.contains(n)` works on
    arrays unchanged).
  - Test destructurings `let [c, x] = agent_names_for(&k)` (`agent.rs:615`) and `let [claude, _] = ...`
    (`keymap.rs:1574,1592,1615,1637,1671`) become `let [c, x, _] = …` and `let [claude, ..] = …`
    respectively (slice patterns on arrays).
- `AgentUpdates::check` (`agent_updates/mod.rs:316`), the doctor rows (`deps/doctor.rs:157`) and the
  supervisor integration targets (`backend/herdr/supervisor.rs:243`) iterate `CliKind::ALL`; their Antigravity
  behaviour is specified in docs 05 and 03.
- Every other `match cli` stays an exhaustive match with no `_ =>` wildcard added, so the compiler finds each
  site.

### Frontend: `ipc.ts` and `providers.ts`

1. `src/lib/ipc.ts:23` becomes `export type CliKind = "claude" | "codex" | "antigravity";`.
2. New `src/lib/providers.ts` (pure, no React) exports exactly:

   ```ts
   export const CLI_KINDS: readonly CliKind[] = ["claude", "codex", "antigravity"];
   export function cliTitle(cli: CliKind): string;   // "Claude" | "Codex" | "Antigravity"
   export function cliBinary(cli: CliKind): string;  // "claude" | "codex" | "agy"
   /** The only definition of the pairs the backend can transfer. */
   export const TRANSFER_PAIRS: readonly (readonly [from: CliKind, to: CliKind])[] =
     [["claude", "codex"], ["codex", "claude"]];
   export function transferTargets(from: CliKind): CliKind[]; // targets t with [from, t] in TRANSFER_PAIRS
   export function switchTargets(from: CliKind): CliKind[];   // CLI_KINDS minus from minus transferTargets(from)
   ```

   `cliTitle`/`cliBinary` are `Record<CliKind, string>` lookups so a missing provider is a type error.
3. Move `cliTitle` out of `src/lib/agentUpdates.ts` (delete it there) and delete `AGENT_CLIS` (`:18`). Update
   importers to `import { cliTitle } from "../lib/providers"`: `AgentUpdateBanner.tsx:6`,
   `AgentUpdateDialog.tsx:13`, `TerminalPane.tsx:13`, `transferPhase.ts:7`. `SettingsDrawer.tsx:11,383`
   imports `CLI_KINDS` instead of `AGENT_CLIS`.
4. Route the current ternary sites through the helpers:
   - `ProviderGlyph.tsx:24,28` -> `const GLYPHS: Record<CliKind, { label: string; path: string }>` (the
     Antigravity path is drawn in doc 07).
   - `Rail.tsx:393,416` -> menu model in doc 06.
   - `agentUpdates.ts:18-43`: `bannerRows` iterates `CLI_KINDS`; `updatePromptCliOf` returns
     `CLI_KINDS.find((k) => k === named) ?? null` (so it accepts the backend's `AgentUpdatePrompt: <cli>`
     token for any provider); every user-visible copy that interpolates the raw id uses `cliTitle(cli)` instead:
     `:87` (`guidanceNote`), `:97` (`updateDisabledReason`, `row.cli`), `:122-123`, `:163`, `:173` and `:178`.
   - `SettingsDrawer.tsx:383-385` (raw `{cli}` label -> `cliTitle(cli)`) and `:645-654` (`toggleAutonomy`
     branches; doc 07).
   - `App.tsx:660-688` (`onTransfer` branches on `targetCli === "codex"`; stays a two-way branch because
     transfer has exactly two pairs; doc 06 adds `onSwitch`).
   - `laneState.ts:77-104`: doc 06.
   - `transferPhase.ts`: delete `cliLabel` (`:221`) and use `cliTitle`; `freshToastMessage` (`:231-237`) stays
     Codex-specific on purpose (the fresh outcome only exists for Claude->Codex) and keeps its text;
     `MID_TURN_NOTE` (`:300`) becomes `export function midTurnNote(from: CliKind): string` returning
     `` `${cliTitle(from)} is in the middle of a turn. That in-flight turn will not be carried over.` `` and its
     one caller (`App.tsx` `onTransfer`, the `midTurn` branch, currently `:671`) passes `"claude"`;
     `spawnFailedCopy` (`:308-320`) uses `cliBinary(cli)` in both the generic `Failed to launch ${…} — is it on
     PATH?` and the `startsWith(`${cliBinary(cli)} did not start`)` test (the backend's `"{} did not start"` text
     in `strict.rs:384` uses `agent::herdr_kind(STRICT_CLI)`, which equals the binary for Codex).
   - `AgentUpdatesProvider.tsx:294-298`: doc 05.
   - `DepsSection.tsx:108`: renders `{cliTitleOrTool(row.tool)}`; implement as a local
     `toolTitle(tool: DoctorRow["tool"])` returning `cliTitle(tool)` for `claude|codex|antigravity` and the
     tool string unchanged for `herdr|git`.
   - `codexDispatchEditor` prop (`SettingsDrawer.tsx:443,458`, `App.tsx:1269`): doc 07.

### Wire shapes that are fixed pairs become maps keyed by provider

- `AgentUpdatesSnapshot { codex, claude, … }` -> `{ agents: Record<CliKind, AgentUpdateEntry>, … }` (Rust
  `BTreeMap<CliKind, AgentUpdateEntry>`, serialized as a JSON object keyed `"claude"`, `"codex"`,
  `"antigravity"`). Details in doc 05.
- `SettingsUser.codex_available` -> `available: Record<CliKind, boolean>`. Details in doc 02.
- TOML keeps one table per provider: `[claude]`, `[codex]`, `[antigravity]`.

## Tests required

Rust (`crates/keitora-tasks/src/config.rs` `mod tests`):

- `cli_kind_serde_round_trips_all_variants` — each of `claude`, `codex`, `antigravity` round-trips through
  `toml` as `cli = "<name>"`; deserializing `"gemini"` is an error.
- `cli_kind_all_is_display_order` — `CliKind::ALL == [Claude, Codex, Antigravity]` and
  `CliKind::ALL.len() == CLI_COUNT`.
- `provider_specs_match_the_table` — for each `c in ALL`: `c.spec().cli == c`; the title, binary, herdr kind,
  integration and agent-name values equal the table above (Antigravity names exactly
  `["antigravity_cli","antigravity-cli","agy"]`).
- `cli_kind_orders_by_declaration` — `Claude < Codex < Antigravity` (guards the `BTreeMap` order).

Rust (`agent.rs` `mod tests`):

- `kind_mapping` (existing) gains `assert_eq!(herdr_kind(CliKind::Antigravity), "agy")`.
- `agent_names_for_has_one_name_per_kind_with_a_shared_suffix` (existing, `:610`) now destructures three names
  and asserts the third starts with `agy-` and shares the 8-hex-digit suffix with the other two.

TypeScript (`src/lib/providers.test.ts`):

- `CLI_KINDS is claude, codex, antigravity in that order`.
- `cliTitle maps every provider` — `Claude`/`Codex`/`Antigravity`.
- `cliBinary maps every provider` — `claude`/`codex`/`agy`.
- `transferTargets` — `claude -> ["codex"]`, `codex -> ["claude"]`, `antigravity -> []`.
- `switchTargets` — `claude -> ["antigravity"]`, `codex -> ["antigravity"]`, `antigravity -> ["claude","codex"]`.
- `TRANSFER_PAIRS is the only source` — `transferTargets` and `switchTargets` together with `from` cover
  `CLI_KINDS` exactly once for each `from`.

TypeScript (`src/lib/agentUpdates.test.ts`): the order test at `:71-73` is updated to expect `CLI_KINDS` order
(claude, codex, antigravity) instead of codex first; a new case asserts `updatePromptCliOf("AgentUpdatePrompt:
antigravity") === "antigravity"` and `updatePromptCliOf("AgentUpdatePrompt: nope") === null`.

TypeScript (`src/lib/transferPhase.test.ts`): `spawnFailedCopy("antigravity", null).message` is
`Failed to launch agy — is it on PATH?`; `midTurnNote("codex")` starts with `Codex is in the middle of a turn`.

## Cross-references

- 02 persistence: keyed settings DTOs. 03: supervisor integration list gate, discovery. README Open issue 3 covers the `did not start` text. 05: snapshot map and
  doctor. 06: menu uses `transferTargets` / `switchTargets`. 07: glyph record, settings render prop.
