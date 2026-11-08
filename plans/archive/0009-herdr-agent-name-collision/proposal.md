# 0009 — herdr `agent_name_taken` when an orphaned pane holds the agent name

## Goal

Starting an agent in a worktree must never fail because some *other* herdr pane
still holds that worktree's agent name, and Keitora's keymap must track the
worktree's real agent pane rather than a stray shell. Today a Codex → Claude
transfer (and a plain fresh Claude start) can fail with:

```
herdr exited with exit code: 1: {"error":{"code":"agent_name_taken","message":
"agent name claude-<h> is already used; candidates: terminal_id=… pane_id=wN:p1
workspace_id=wN tab_id=wN:t1 cwd=/Users/me/code/proj status=Idle"},"id":"cli:agent:start"}
```

## Findings (Stage 1 research + refine review)

### 1. herdr error codes are lost in production (foundational)

`RealHerdr::run` (`backend/herdr/real.rs:111`) only looks for an `{"error":…}`
body on **stdout**. Probed on macOS against herdr 0.9.3: `agent start` and
`pane close` print the error body on **stderr**, with an empty stdout and exit 1:

```
$ herdr pane close w0:p0        # exit 1, stdout empty
stderr: {"error":{"code":"pane_not_found","message":"pane w0:p0 not found"},"id":"cli:pane:close"}
```

So every herdr error becomes `HerdrError::Exit { stderr }` with
`code() == None`. The user's message, `herdr exited with exit code: 1: {…}`, is
exactly the `Exit → PtyBackendError::Other` rendering (`ops.rs:128`). The module
doc comment (`real.rs:6`) claims the stdout shape was "verified against herdr
0.9.3", but that does not hold on macOS. Everything keyed on a code is dead in
production:

- `agent_start_result_from` folding `agent_not_ready` to `Ok` (`cli.rs:219`);
- `agent::is_early_exit`, so the resume → continue → fresh fallback never runs;
- `HerdrError::is_not_found`, so `close_session` returns an error for a pane that is already gone;
- `map_error` code → `PtyBackendError` mapping (`ops.rs`), e.g. `pane_not_found → NoSuchSession`.

The fake (`fake.rs`) returns `HerdrError::Api` directly, so the test suite
exercises the intended behaviour and never the real one.

### 2. Orphan source: reconcile adopts the wrong pane by cwd

Live evidence on the affected machine (keymap, `agent list`, the Keitora log):

- the orphan `claude-<h>` pane was created with a batch of 13 named agents, one per
  worktree, when the herdr server started, more than a day before the transfers;
- the log shows `restore reconcile adopted 13 live pane(s) by cwd`;
- many keymap entries point at panes with **no agent** (plain shells) or at an
  unnamed agent, while the named Keitora agent in the same cwd is unmapped.

`keymap::reconcile_panes` (`keymap.rs:675`) gives each persisted entry the
**first unused live pane with a matching cwd**. It does not prefer the entry's
own `pane_id`, and it knows nothing about agents. When a cwd has two panes (a
herdr-restored shell plus the agent), the shell can steal the mapping. The real
agent is then an ignored "extra live pane" that still holds the key's name.

Sequence for the reported failure: boot reconcile maps the worktree to a shell →
Claude → Codex replaces the *shell* (keymap pane) with `codex-<h>`, while the
real `claude-<h>` survives → Codex → Claude closes `codex-<h>` and runs `agent start
claude-<h>`, which fails with `agent_name_taken`.

### 3. Eviction only goes through the keymap, and close failures are discarded

Both `agent_start` launch paths evict the previous pane only via `keymap.get(key)`,
with `let _ = pane_close(..)`:

- default: `Spawner::spawn_agent_locked`, `backend/herdr/mod.rs:851-858`;
- strict (transfer → Codex): `spawn_agent_transfer_strict_with`, `strict.rs:459-466`,
  a copy of the same block, inside the per-step loop.

Agent names are deterministic (`agent::agent_name`, `agent.rs:132`):
`{kind}-{fnv1a32("project/worktree"):08x}`. So any pane the keymap lost keeps
blocking that kind's launches in that worktree for good.

### Other launch / close paths (checked)

- `HerdrBackend::spawn` (`mod.rs:2238`) types a launch line via `pane_run`, never
  calls `agent_start`, so it cannot hit `agent_name_taken`. Out of scope.
- `resume_locked` → `spawn_agent_locked`, so recovery is covered.
- `close_session` (`mod.rs:1944`) uses `get_current` and skips the close for an
  entry from a previous server generation, but still removes it. This is a minor
  forgetting path, handled by the name-based eviction below.

## Scope

**In**

1. **Parse herdr error bodies from stderr** (and stdout) in `RealHerdr::run` and
   `run_raw`. Fix the `real.rs` doc comment.
2. **Agent-aware reconcile:** `reconcile_panes` prefers, in order: the entry's
   exact `pane_id` (same server), then the live pane holding the key's expected
   agent name, then any cwd match. This also heals the existing bad mappings on
   the next boot/reconcile.
3. **Name-based eviction** in one shared `Spawner::evict_key_panes`, used by both
   `agent_start` paths in place of the duplicated keymap block.
4. **`agent_name_taken` recovery:** close the holder(s) named by herdr, then retry
   the attempt once.
5. **Logging:** failed eviction closes and duplicate panes in one cwd during
   reconcile log at `warn`.
6. **Fake-herdr fidelity:** the fake enforces name uniqueness, reports
   `name`/`cwd` in `agent_list`, and builds name-taken errors shaped like herdr's.
   This makes the bug reproducible.

**Out**

- Changing the naming scheme. Names stay stable, because herdr-owned restore
  identifies agents by them.
- The pane-typed `spawn` path (no `agent_start`).
- A background sweeper that closes unmapped Keitora-named agents across all
  worktrees. Item 2 re-adopts them, and item 3 evicts any stragglers on launch.

## Technical design

### A. stderr error bodies — `real.rs`

Add a pure `error_body(stdout, stderr) -> Option<HerdrError>`: parse each
trimmed stream as JSON (stdout first) and, if it has an `error` object, return
`api_error_from_value`. `run` checks it before treating stdout as the result.
`run_raw` checks stderr on a non-zero exit before falling back to `exit_error`.
An `Exit` is returned only when neither stream carries an error body.

Behaviour change: real errors now map through `map_error`, matching what the
fake-backed tests already assume. Code that string-matched `Exit` text is checked
in Stage 6. The only one found so far is `agent_start_result_from`'s
"blocked during startup" test on the `PtyBackendError` string, which still works.
Revisit the `attach` comment (`mod.rs:~2380`) that says `run_raw` discards the code.

### B. `AgentInfo` gains `name` and `cwd` — `cli.rs`

`AgentInfo` is currently test-only: `#[cfg_attr(not(test), allow(dead_code))]` on
it, `parse_agent_info`, `parse_agent_list`, and `HerdrOps::agent_list`, plus
"no production caller" comments. This change gives it production callers, so
remove those attributes and comments.

- Add `name: Option<String>` (absent for agents herdr didn't start by name) and
  `cwd: Option<String>` (herdr's pane cwd; ignore `foreground_cwd`, which follows
  the shell).
- Both are observed in live `agent list` output.
- Add a scrubbed fixture of the real shape to `cli.rs`'s parser test.
- Update the whole-struct asserts at `cli.rs:513-520`.

### C. Pure helpers — `agent.rs`

- `agent_names_for(key) -> [String; 2]`: one name per `CliKind`, sharing the hash suffix.
- `name_taken_holders(err) -> Vec<String>`: for an `Api` error with code
  `agent_name_taken`, every `pane_id=<id>` in the message's candidates. Empty
  otherwise.

### D. Agent-aware reconcile — `keymap.rs`

`reconcile_panes` gains `agents: &[AgentInfo]` (an empty slice keeps today's
behaviour). It also needs each entry's expected agent name, which comes from the
key and the kind the keymap or launcher records. If the keymap has no kind,
accept either of `agent_names_for(key)`.

Match order per entry, using only unused live panes and normalized cwd equality:

1. same `pane_id` and same cwd;
2. the pane holding one of the key's names (prefer the key's current kind);
3. first cwd match, as today.

Log at `warn` when a cwd has more than one live pane. Call sites in `reconcile.rs`
fetch `agent_list` alongside `pane_list`. An `agent_list` error degrades to `&[]`
with a `warn`.

### E. `Spawner::evict_key_panes(key, cwd)` — `mod.rs`

Replaces the block in both launch paths and runs at the start of every attempt.

1. Keymap pane (as today): stop the bridge, close the pane, drop the keymap entry
   and live membership. Log a close error other than not-found at `warn`.
2. `ops.agent_list()`. For each agent whose `name ∈ agent_names_for(key)`, whose
   normalized `cwd` matches `held_cwd(cwd)`, and whose pane **is not mapped by the
   keymap to any other key** → `pane_close`. Not-found counts as success.
3. If anything was closed by name, poll `agent_list` (bounded about 1 s, `Quick`)
   until those names are gone. herdr may free the name slightly after the close.

An `agent_list` error skips steps 2 and 3 with a `warn`. Section F still recovers.

The cwd guard plus the "not mapped to another key" guard together rule out
closing another worktree's pane on a 32-bit hash collision.

### F. `agent_name_taken` recovery

- **Default path** (`spawn_agent_locked`): when `agent.start` returns
  `agent_name_taken`, close the fresh pane, close every holder from
  `name_taken_holders` that isn't mapped to another key, and run the loop again.
  The next attempt re-runs eviction and the shell-ready wait. Do not retry in the
  same pane, because a failed `agent.start` isn't known to leave the shell clean.
  The resume decision is unchanged. Allow one name-taken recovery per launch; a
  second returns the error, as today.
- **Strict path:** `strict_step` (`strict.rs:557`) currently re-issues
  `agent.start` on errors it doesn't classify (`strict.rs:~576`). Match
  `agent_name_taken` before that arm and return it to the outer chain loop, which
  then recovers the same way, once per launch.

### G. Fake herdr — `fake.rs`

- Store the started agent's name in a `name → pane_id` side map in the fake state,
  so `cli::PaneIdentity` stays unchanged.
- In `agent_start`, before popping the script: if another live pane holds `name`,
  return an `agent_name_taken` `Api` error whose message has a herdr-shaped
  `candidates: … pane_id=<id> …` list. The same pane re-using its own name is
  allowed, because existing tests restart `"n"` on one pane (`fake.rs:1294-1339`).
- Success arms record the name. `pane_close` and pane death free it.
- `agent_list` reports `name` and `cwd`.
- Add helpers to seed an orphan pane holding a name and to delay freeing a name.

## Tests

- `real.rs`/`cli.rs`:
  - `error_body` finds the error on stderr (real 0.9.3 shape) and on stdout;
  - `None` for a non-error JSON body or non-JSON text;
  - stdout wins when both streams have one.
- `cli.rs`: `parse_agent_info` reads `name`/`cwd` from the scrubbed real fixture and tolerates their absence.
- `agent.rs`:
  - `agent_names_for` covers both kinds with one suffix;
  - `name_taken_holders` parses one and several candidates, and returns empty for other codes and for a message without `pane_id=`.
- `keymap.rs` `reconcile_panes`, with two panes in one cwd:
  - the entry's own `pane_id` wins;
  - otherwise the named agent wins over a shell listed first;
  - the other kind's name is used only when the expected name is absent;
  - no agents → today's behaviour.
- `reconcile.rs`: a same-server adopt over a fake restore that doubled panes adopts the named agent, not the shell.
- `fake.rs`:
  - a duplicate name on another pane → `agent_name_taken` with candidates;
  - the same pane may re-use its own name;
  - closing the holder frees the name.
- `mod.rs`, backend against the fake:
  - an unmapped orphan holding `claude-<h>` → a Claude spawn succeeds and the orphan is closed;
  - Codex → Claude with the Claude orphan alive → succeeds and leaves exactly one agent for the key;
  - another worktree's agent, a user's unnamed agent, and a name-matching pane mapped to another key are never closed;
  - `agent_list` failing → the spawn still succeeds via name-taken recovery;
  - a name that frees only after a delay → still succeeds;
  - a second `agent_name_taken` → error returned and the fresh pane closed;
  - an early-exit `Api` error still drives the resume fallback, as a regression guard.
- `strict.rs`:
  - the strict chain evicts a stale `claude-<h>` before launching Codex;
  - `strict_step` does not re-issue on `agent_name_taken`.

## Dependencies

None. Uses the existing `HerdrOps` methods (`agent_list`, `pane_list`, `pane_close`).

## Risks

- **Error-mapping change in A is global.** Real errors start flowing through
  `map_error`, and herdr paths that never saw codes in production now do. This
  matches what the fake-backed suite assumes, but run the app and smoke-test
  launch, attach, kill, transfer and restore in Stage 7, in a sandbox project
  rather than the live one.
- **Closing a pane the user cares about.** Only panes with a Keitora-generated name
  for *this* key, in this key's cwd, not mapped to another key, and only right
  before launching that key's replacement.
- **Reconcile re-adoption changes what existing users see.** On next boot,
  worktrees whose keymap points at a shell switch to the named agent pane that
  holds their real conversation. That is the intended fix, but the previously
  mapped shells become unmapped extras that stay open (harmless and closable).
- **Latency.** One or two `Quick` `agent list` calls per launch attempt and per reconcile.

## Open questions

1. Where does reconcile get the key's current kind? Options are the keymap entry
   (no kind field today), the launcher, or the resume store. The fallback,
   accepting either name, is safe but may adopt a stale other-kind agent over the
   current one when both exist. The resolution belongs in Stage 6, not the design.
2. Should the previously stolen shell panes be closed during reconcile when the
   named agent is adopted instead? Not doing so is the conservative default.
