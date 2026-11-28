# 0080 drive state reliability: implementation entry point

Read `00-overview.md` first (phase index, decisions D1-D8, reproductions, cross-doc
reconciliation), then the subsystem document each stage names. Also read `AGENTS.md`,
`spikes.md` and `loss-report.md`. Where `00-overview.md` and a phase document differ, the
owning phase document wins (see "Cross-doc reconciliation" in the overview); inside a phase
document, its "Singletonize decisions" section wins over the prose above it.

Docs (all in `plans/0080-live-pane-not-offline/`):

| Doc | Phase | Subsystem |
|---|---|---|
| `00-overview.md` | - | goal, reports A1/A2/B/C/R, decisions, failure modes, repros, reconciliation |
| `01-phase0-isolation.md` | 0 | launcher and app strip inherited `HERDR_*`; pre-spawn re-check |
| `02-engine-core.md` | 1 | session engine, `status.db`, consumer migration, drive gate |
| `03-turns-and-cli.md` | 2 | delivery baseline, turn predicate, `drive_turns`, human turns, CLI `wait`/`drives`/`read` |
| `04-request-pipeline.md` | 3 | passes, sweep, watcher supervision, `wait_reason`, presence, CLI timeout diagnosis |
| `05-hook-placement.md` | 4 | launch prep, Codex bypass, Claude `--settings`, agy, stale-writer guard, hook log, Stop hardening, hook facts |
| `06-reply-fallbacks.md` | 5 | Claude transcript and Codex rollout reply text |
| `07-self-diagnostics.md` | 6 | health monitor, findings, Settings, glyph, `keitora status` |
| `spikes.md` | - | live spike procedures, abort criteria, decision table, results template |
| `loss-report.md` | - | requirement trace; read to avoid re-dropping the restored items |
| `research-*.md` | - | evidence only; not normative |

Branch: `llm/andrew/keitora/0080-live-pane-not-offline` (already checked out; do not create
another). One PR, phases 0-6.

## Execution model

Implementation is **sequential**: do the stages in order, one commit each. Do not start a
stage until the previous one is committed and its validation is green. Each stage is sized for
one sub-agent in one pass over roughly one subsystem; it summarises the work and names the
exact doc sections holding the full contract. Read those sections before coding.

Stages marked **PHASE BOUNDARY** end a phase. After the boundary commit the **orchestrator**
(not the stage's sub-agent) runs `cargo test --workspace` and `yarn test`, then `/simplify`,
then `/code-review high`, and fixes findings in follow-up commits before the next phase starts.
Stage files only mark the boundary. Record the red-run output of each repro (below) in the
phase's `/code-review` notes.

Stages marked **SPIKE-GATED** depend on a spike result. They must not start until
`spike-results.md` exists and its "Decisions taken" lines cover the spike they name. Stage
`s04` is run **by the orchestrator, live, not by a sub-agent** (see "Spikes").

## Stages

| # | File | Phase | Goal | Gate |
|---|---|---|---|---|
| s01 | `s01-launcher-isolation.md` | 0 | strip `HERDR_*` in launcher and app env; pre-spawn re-check | PHASE BOUNDARY |
| s02 | `s02-home-and-status-store.md` | 1 | `keitora_home()` moves; `status.db` store in keitora-tasks | |
| s03 | `s03-agent-info-and-fakes.md` | 1 | `AgentInfo` fields; fake herdr/backend `agent list` support | |
| s04 | `s04-spikes.md` | - | orchestrator-run live spikes S0, S1, S3, S3b, S5 (S2 optional) | ORCHESTRATOR, blocks s05+ |
| s05 | `s05-engine-derive.md` | 1 | engine model and pure `derive` | SPIKE-GATED (S1) |
| s06 | `s06-engine-driver-and-publisher.md` | 1 | `SessionEngine`, evidence, status-loop wiring, `StatusPublisher` | SPIKE-GATED (S1) |
| s07 | `s07-read-consumers.md` | 1 | read-side consumers move to the engine; A1/A2 repros red | |
| s08 | `s08-drive-runner-gate.md` | 1 | `Awake`/`SendGate`, wake/send one answer; repros green | |
| s09 | `s09-frontend-latch-and-stale.md` | 1 | latch guard, `is-stale`, ipc types | PHASE BOUNDARY |
| s10 | `s10-turn-store.md` | 2 | delivery columns, `drive_turns`, store API; B repro red | |
| s11 | `s11-delivery-baseline.md` | 2 | baseline capture in `deliver_send`; settling release | |
| s12 | `s12-engine-turns.md` | 2 | turn predicate, ledger, reply collector, `drive_turns` writer | SPIKE-GATED (S1) |
| s13 | `s13-cli-status-readers.md` | 2 | CLI `wait`/`drives`/`read`/gate on `status.db`; B repro green | PHASE BOUNDARY |
| s14 | `s14-request-store-columns.md` | 3 | `seen_at`/`wait_reason` columns and store; C/R repros red | |
| s15 | `s15-watcher-supervision.md` | 3 | watcher health, registry, restart | |
| s16 | `s16-request-passes.md` | 3 | `request_pass`, reads-first, `wait_reason`, pass log; C green | |
| s17 | `s17-pipeline-and-presence.md` | 3 | `keitora-pipeline` thread, sweep, presence, window report; R green | |
| s18 | `s18-cli-timeout-diagnosis.md` | 3 | CLI timeout decision table, README | PHASE BOUNDARY |
| s19 | `s19-hook-log-and-stale-guard.md` | 4 | hook error log, `session_id` guard | |
| s20 | `s20-stop-capture-hardening.md` | 4 | Stop payload fixtures, one turn per payload, `turn_id` dedupe | |
| s21 | `s21-launch-prep-and-providers.md` | 4 | `launch_prep`, Codex bypass on every step, `prepare_launch` all providers | |
| s22 | `s22-agy-hooks.md` | 4 | agy absolute path, marker env, re-assert | detail from S3b (non-blocking) |
| s23 | `s23-claude-launch-settings.md` | 4 | Claude per-launch `--settings` plus guard (Branch A) or recorded skip (Branch B) | SPIKE-GATED (S3) |
| s24 | `s24-hook-facts.md` | 4 | `hook_facts`, `codex_trust` | PHASE BOUNDARY |
| s25 | `s25-transcript-parsers.md` | 5 | pure transcript parsers and synthetic fixtures; reply repro red | |
| s26 | `s26-transcript-locators.md` | 5 | app locators, `TranscriptSource` wired into the collector | |
| s27 | `s27-captured-fixtures.md` | 5 | swap in scrubbed captured fixtures (Claude, Codex) | SPIKE-GATED (S5) |
| s27b | `s27b-agy-transcript-reader.md` | 5 | agy reply reader from session files, or docs/tests for `none` | SPIKE-GATED (S5-agy), PHASE BOUNDARY |
| s28 | `s28-health-core.md` | 6 | pure `evaluate`, reconcile, remedies | |
| s29 | `s29-health-driver-and-ipc.md` | 6 | driver, pipeline tick, IPC, verb integration | |
| s30 | `s30-health-frontend.md` | 6 | glyph, provider, Settings section | |
| s31 | `s31-keitora-status-and-docs.md` | 6 | `keitora status`, stall/timeout health lines, docs, final verification | PHASE BOUNDARY |

## Spikes

`s04` sits after the stages that do not depend on herdr semantics (`s01` Phase 0 is its hard
prerequisite; `s02`, `s03` are pure plumbing and fakes) and before the first stage that does.
Rules for the orchestrator:

- Get the human's explicit OK first (`00-overview.md` "Open question"; `spikes.md` header).
  Real agent logins and quota are used.
- Run S0 first, every session, and at the end. Any S0 failure stops all live work: build
  s05+ on the fake backend with schema-derived semantics (decision table row "S0 fails").
- Create `spike-results.md` from the template in `spikes.md`; apply the decision table; write
  one "Decisions taken" line per applied row. Raw captures stay under `$DEV_HOME`, never
  committed. Commit only `spike-results.md`, scrubbed to placeholders.
- **If a spike result changes a design** (a row of the decision table that says "no change"
  does not count): stop, return to the refine step for that phase document, edit the doc and
  its "Singletonize decisions", re-check the cross-doc reconciliation list, commit the doc
  change (`plan(...)`), then continue. Do not code around a doc that is now wrong.
- Spike-gated stages: s05/s06 (S1: `interactive_ready` presence, `/clear` identity), s12 (S1:
  primary predicate clause, 30 s stall), s21 (S1: Codex-without-bypass), s23 (S3), s27 (S5), s27b (S5-agy).
  s22 takes its detail from S3b but has a default branch, so it is not blocked.

## Test rigor

- Every stage that adds Rust logic ships `#[cfg(test)]` tests **in the same commit**; every
  stage that adds frontend logic ships vitest tests in the same commit. Untested new logic is
  not done. The tests a stage names must exist under those names (or the closest names the
  phase doc's own test list gives).
- **Repro tests are written RED first.** Each `repro_*` test (`00-overview.md`
  "Reproductions", verbatim names) is written and run against the unmodified code of its phase
  and must fail on the assertion named there; the stage that writes it records the failing
  assertion text in its report, commits it `#[ignore = "RED until <what lands>"]` (no phase or task label) if the fix lands
  in a later stage of the same phase, and the fixing stage removes the `#[ignore]`. A phase is
  not done while an ignored repro remains.
- Per stage run `cargo test -p <crate>` for each touched crate (`keitora-tasks`,
  `keitora-cli`, and the app package `keitora` in `src-tauri`) plus `cargo check` for the same;
  stages touching the frontend also run `yarn test` and `npx tsc --noEmit`. Phase boundaries:
  `cargo test --workspace`, `yarn test`, `yarn lint`, `npx tsc --noEmit`.
- **No test may** contact a running herdr daemon, touch the real Keitora home or the real home
  directory (`~/.claude`, `~/.codex`, `~/.gemini`, `%APPDATA%`), or spawn a real agent. Use
  temp dirs, temp-file SQLite, the fake herdr backend, a fake clock and injected paths.
  Never read `dirs::home_dir()` or `keitora_home()` in a tested path without an override.
- **Never** run gated herdr tests (anything needing `KEITORA_HERDR_BIN`), `yarn dev:herdr`, or
  `yarn dev:isolated`, in any stage but the orchestrator's `s04`. Avoid heavy runs beyond the
  commands above; the machine is shared with a running Keitora and herdr.
- Never kill processes by image name; never touch the user's running Keitora or herdr.

## Rules from AGENTS.md (repeated)

- **No plan or task identifiers in source code or commit messages**: no `#NNNN`, no
  `Stage N`, no `Phase N`, no `plans/0080-...` paths in comments or test names. The plan docs
  say "02", "S5", "T4"; never carry those labels into code, comments, test names or fixtures.
  (This applies to `#[ignore = "..."]` reasons too: write `RED until the engine consumers
  land`, not `RED until phase 1`, in the committed attribute.)
- **Placeholder names only**: projects `my-game`/`proj`, worktrees `alpha`/`bravo`/`charlie`,
  users `me`/`x`, paths `C:/Users/x/...`, `/Users/me/...`, `D:/repos/proj`, hosts
  `example.com`. No real project, person, company, machine or private-tool name in fixtures,
  comments, captured transcripts or commit text. Scrub any pasted real output.
- Commits: plain Conventional Commits `type(scope): description`, no trailing task tag. End
  each message with the session's configured trailer:
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Before each commit: `git grep -nE '#[0-9]{3,4}' -- <changed files>` and
  `git log -1 --format=%B | grep -E '#[0-9]'` must both be empty (plan docs under `plans/` are
  exempt).
- Do not push. Do not `git add` anything under `.harness/`.
