# 0080 loss report (Stage 4)

Compares the user requirements (a)-(l) and the two baselines against the final docs.
Baselines: `7b6756c` (refined proposal), `d253a74` (A-D version). Final: `00`-`07` and `spikes.md`.
Placeholder names only. Read-only review: no builds, no commits.

## 1. Requirements (a)-(l)

| Req | Status | Where in final docs |
|---|---|---|
| (a) live agent never reported offline (agy/macOS; reset --to codex; wake vs drives) | Preserved; macOS not reproduced | `00` A1/A2 rows and failure modes; `02` S5 derive (presence from `agent list`). Spikes run on Windows (D1), so no Mac repro. See Modified. |
| (b) Codex completion must reach `wait` | Preserved | `00` B; `03` T5 predicate (`completion_seq`, `reply_source: none`); `03` T7 idle-no-change stall at 30 s; `06` transcript fallback |
| (c) unclaimed requests; distinguish unclaimed from absent app | Preserved | `04` triggers Recheck/Sweep/Engine/OpenSet; `04` CLI decision table rows 2-8 (`app_stale`, `project_not_open`, `watcher_silent`, `waiting`) |
| (d) remote trace; open set, DB path, last poll, request id; switch/reopen tests; open-set diagnostic | Preserved, with a change | `04` pass-log line (`open=`, `db=`, `trigger=`, request id) and `last_pass_at`; `00` R tests. Outside-open-set request is refused, not waited (Modified). |
| (e) per issue: smallest repro, root cause, regression test | Partly | Root cause and regression tests in `00` table and each phase doc. No "smallest reproduction" step anywhere. See Unintentional #1. |
| (f) more reliable architecture across providers | Preserved | `00` Key finding: `agent list` polling (D2); hooks demoted to evidence (D8); engine (`02`) |
| (g) continuous self-diagnostics | Preserved | `07` health monitor; `keitora-pipeline` thread (`02` S14); Settings Health section; `keitora status` |
| (h) one PR; `/simplify` + `/code-review high` per phase | Preserved | `00` intro and D3 |
| (i) human-typed turn as separate `human_turn` | Preserved | `00` D5; `03` T5 and event JSON; `06` ref |
| (j) status.db unless research finds better (justified) | Preserved, justified | `00` D4 compares presence JSON, table in `tasks.db`, per-project `status.db`, and global `status.db` |
| (k) spikes via isolated launcher; sequencing; Phase 0 first | Preserved, Windows venue | `01` Phase 0; `spikes.md` header and "do not start until human says yes"; `00` D1 |
| (l) hook failures logged; doctor checks | Preserved | `05` hook log (`logs/hooks.log`, rotating); `05` section 8 `hook_facts`; `07` `hooks_missing`, `agy_plumbing` |

## 2. Baseline items

### Preserved
- Presence overrides a missing or stale `offline` row; bare shell stays offline (`00` A1 tests, `02` S5).
- Synthesized `idle` row for a present agent with no row (`02` S10, "View but no row").
- Stale-writer guard via `session_id` (`02` S5 rule 0, `05` section 7, tests).
- Hooks and trust installed on every provider change and launch (`05` section 1-3; `00` A2).
- Trust bypass carried on every strict step (`05`; test `every_strict_step_carries_the_hook_trust_bypass`).
- agy absolute path plus re-assert after herdr rewrites `hooks.json` (`05` section 4).
- Hook log under Keitora home, rotating (`05`, `hook_log.rs`).
- Doctor and Settings hook-health checks (`07`).
- Worktree names outside configured set are reported, not silently filtered (`unconfigured_sessions`, `07`).
- `wait` cannot hang; `Stalled` on idle with no reply (`03` T7).
- `seen_at` / `wait_reason` on `drive_requests`; expiry carries the reason (`04`).
- Self-rescheduling pass (2 s) and safety sweep (10 s), deduped and bounded (`04`).
- Reads not blocked behind a gated send (`04`, `00` C test).
- Watcher supervision; store-open failure recorded, not swallowed (`04`).
- macOS WAL watcher test in CI (`04` line ~495).
- Risk mitigations for synthesized idle: busy veto, `send_settling`, herdr refuses `blocked` (`00` failure modes).

### Modified (justified)
- Presence JSON file became global `status.db` (`00` D4).
- `SessionSummary.agent: bool` became the engine `View`; `SessionSummary` itself unchanged (`02` S4, S10).
- Stall rule: 20 s idle grace became 30 s no-activity since delivery, keyed on `completion_seq`; reason text changed (`03` T7).
- Strict launch: trust bypass is kept on every step (fix), not only flagged (`05`).
- Outside-open-set request: immediate refusal, not a wait state (`02` decision 12, `04`).
- Codex project trust: decided by spike (`spikes.md` Codex step 10, S3) rather than a `config.toml` write (`00` open question resolved by spike).
- Mac diagnostics questions replaced by doctor facts (`translocated`, `path_ok`, `key_present`); Mac captures and repro dropped under D1 (Windows venue).
- Turn completion from herdr, not the Stop hook (`03`).

### Intentionally dropped
- Codex `notify` (D6; would replace the user's own `notify`). Spike 4 dropped with it.
- Herdr push subscription (D2; follow-up behind Spike 2).
- CLI shelling out to `herdr agent wait` (D7).
- Presence JSON file (D4).
- Wait reason "project not open in the window" (`02` decision 12).
- Stop-hook completion as the source of truth (replaced by herdr signal).
- agy prompt brokering (out of scope in both baselines).

### UNINTENTIONALLY DROPPED
1. **Smallest reproduction per report** (baseline A/B/C): the word "reproduc" appears in no final doc. Only regression tests remain. Violates (e).
2. **Guard that a first-seen synthesized idle must not latch "finished" attention** (`d253a74` A). `02` has no such rule; `attention.ts` is only asserted unchanged.
3. **Stop-capture hardening** (`7b6756c` B): documented Codex Stop payload fixtures, exactly one turn per `--stdin` payload (null, garbage, empty), dedupe of repeated Stop by `turn_id`. `05`/`06` cover only extraction and transcripts.
4. **Codex project-trust doctor check** (`7b6756c` open question 1, "detect and report"): `hooks_missing` checks only `trust_bypass_last_launch`, not trust in `~/.codex/config.toml`.

## 3. Summary
Requirements (a)-(l) are all addressed. Four baseline items were lost without a decision: the smallest-repro step (e), the attention latch guard, Stop-capture hardening, and the Codex trust doctor check. Items 2-4 should be added to the phase docs (`02`, `05`/`06`, `07`) before implementation.

## 4. Restorations

The four unintentionally dropped items (and the two Modified items they touch) are restored in
the docs below. Docs only; no code, builds or commits.

| Item | Restored in | What |
|---|---|---|
| 1. Smallest reproduction per report (req. (e)) | `00-overview.md` "Reproductions"; test lists of `02`, `03`, `04`, `05`, `06` | One named `repro_*` fixture test per report (A1, A2, B, C, R) with its step list, the red assertion on the old code, and the phase where it is written RED first. `05`/`06` carry the hook-side and reply-side halves. The Cross-phase testing rules point at it. |
| 2. First-seen synthesized idle must not latch "finished" | `02-engine-core.md` S12 "Latch guard" (+ S12 table row, S13 tests) | Five-clause rule (first-seen, rowless synthesized idle, `offline -> idle`, `starting -> idle`, stale). `WorktreeSession.starting` plus a one-line input mapping in `attention.ts` / `bg_notifier.rs`; latch bodies unchanged. Tests in `bg_notifier.rs` and `attention.test.ts`. |
| 3. Stop-capture hardening | `05-hook-placement.md` section 9 (+ tests, files touched); `06` cross-reference | Documented Codex Stop payload fixtures (`last_assistant_message` string and `null`), exactly one turn per `--stdin` invocation for null/garbage/empty/oversize/timeout, dedupe of a repeated Stop by `turn_id` (`session_replies.last_turn_id`, `session_reply_record_for_turn`, `payload_turn_id`). |
| 4. Codex project-trust doctor check | `07-self-diagnostics.md` check `codex_project_untrusted` (+ tests); `05-hook-placement.md` section 8 `CodexFacts.project_trust`, `keitora_tasks::codex_project_trust` | Read-only parse of `~/.codex/config.toml` `[projects."<path>"] trust_level`; report-only, no remedy and no global write; message explains why trust matters despite the per-launch bypass (hand-launched codex, herdr's session-id hook). |
| (a) macOS not reproduced | `spikes.md` "Optional, user-run: macOS reproduction checklist for A1" | Seven-step user-run checklist (AppTranslocation, `hooks.json` `keitora` key, `KEITORA_BIN` in the pane, hook log, first-prompt behaviour, herdr `agent list`) feeding `AgyFacts` (05) and `agy_plumbing` (07). Still not run by an agent (D1). |

Status after restoration: requirements (a)-(l) preserved; "Unintentionally dropped" is empty.
The macOS reproduction remains optional and human-run.
