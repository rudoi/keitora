# 0078 loss report (Stage 4)

Compares the original proposal (`git show ca54732:plans/0078-cross-worktree-driving.md`), the refine
cycle 1 proposal (`git show cba45e8:...`), the auto-refined top-level proposal
(`plans/0078-cross-worktree-driving.md`, refine cycle 2 plus "Implementation notes") and the final
documents `plans/0078-cross-worktree-driving/00-overview.md` to `05-ui.md`.

Placeholder names only: driver `alpha`, driven `bravo`/`charlie`.

## 1. User requirements (a)-(l)

| Req | Requirement (short) | Status | Where in final docs |
|---|---|---|---|
| (a) | One worktree's session drives others via the `keitora` CLI | preserved | 00 Scope In; 02 CLI surface (drive, send, wait, read, prompts, answer, reset, swap) |
| (b) | Drivers can swap a driven worktree between providers | modified | 02 `swap` (context-carrying, enters `pending_confirm` for a human UI confirm); 02 `reset --to` (no confirm); 04 `swap` flow; 00 Resolved decision 3. A driver cannot finish a `swap` alone. |
| (c) | Drivers can prompt driven worktrees directly via CLI | preserved | 02 `send` (idle only, `--queue`, `--wait`, `-` for stdin); 04 `send` |
| (d) | Rail shows the driving relationship | preserved | 05 Rail UI: `⇠ alpha` badge, `drives N` chip, `N driven` header, state table |
| (e) | Driver sees driven prompts, responds, and escalates | preserved | 02 `prompts`, `answer <wt> <id> allow\|deny\|answer\|escalate --reason`; 03 gate and decision table; 00 Idea 3 |
| (f) | `keitora drive <wt>` and `keitora release <wt>`, not `drive attach`; other verbs top-level | preserved | 02 "Rules common" and clap structure; 00 Resolved decision 1 |
| (g) | 5-minute broker timeout | modified | 00 Resolved 2 and 03 Singletonize 1: default 300 s, plus per-drive `--escalate-after` (1..=540). Fixed hook timeout is 600 s, so the broker caps at 540 s. Timeout is a backstop; offline, released or paused drivers escalate at once. |
| (h) | Reset and swap in one step for fresh conversations | preserved | 02 `reset <wt> [--to PROVIDER]`; 04 `reset` (persist, kill, fresh spawn); 00 Scope In |
| (i) | Driven agent is NOT told it is driven | preserved | 00 Out list and Resolved decision 4 |
| (j) | One level only; one driver may drive many | preserved | 00 Scope In; 01 `drive_bind` checks (3) and (4); 02 `drive` |
| (k) | `send` not held back for the human typing; UX warns | preserved | 00 Resolved 6; 05 "Driven-pane banner" (non-blocking overlay; `Take over` and `Release` buttons) |
| (l) | Human can always override | preserved | 05 Take over, Release, Pause/Resume (lane menu and banner); 01 take-over and pause semantics; typing never blocked |

## 2. Preserved (proposal items carried into the final docs)

- Goal: prompt, wait and read, see prompts and answer or escalate, swap providers.
- Scope In: drive and release binding with one driver per driven worktree; prompt brokering where
  unanswered prompts escalate and are never auto-approved; free-form prompting; rail badge with
  "deciding" vs "needs you" states; Take over and Release; human override.
- Scope Out, all five items: cross-project, push-notify, standing auto-answer policy, Codex
  `request_user_input` brokering, inter-worktree authentication.
- Paradigm ideas 1-4: CLI never touches panes; prompts brokered at the hook; escalation as a
  first-class answer; relationship is app-visible state.
- CLI verbs: `drive`, `release [--all]`, `drives`, `send [--wait] [--queue]`, `wait`, `read`,
  `prompts`, `answer`, `swap --to`. All in 02.
- Data: `drives`, `drive_prompts`, `drive_requests` (generalises `dispatch_requests`).
  Last message and turn live in `session_replies` (see M-list).
- Hooks: `PermissionRequest` (Claude and Codex), `PreToolUse` on `AskUserQuestion`, `Stop`
  capture, fail-safe to escalate, never allow (03).
- App: send, read and swap runner; activity fields `driven_by`, `drives`, `pending_prompts`;
  attention sub-state "waiting on driver"; toast "alpha escalated: <reason>".
- Rail: badge, chip, header count, context menu. Harness mocks behind `?drive=` (05).
- Dependencies, Risks 1-6 (Risk 7 added), two spikes gating 03, Complexity, driver guidance (02).
- Open questions 5 (driven not told) and 6 (flat only) resolved as one-level and not-told.

## 3. Modified (with justification found in the docs)

- **M1 swap needs a human confirm.** Irreversible Claude to Codex import. 00 Resolved 3; 04 `swap`.
  Weakens (b), so confirm this is acceptable.
- **M2 Broker timeout.** Proposal fixed 5 min. Final: 300 s default, overridable 1..=540. Cap comes
  from the fixed 600 s hook timeout (03 Singletonize 1; 01 constants).
- **M3 Stale rule.** Proposal said "broker timeout + 30 s" in the hook. Final: stale is
  `timeout_s + 30` in SQL, hook timeout fixed at 600 s (01 Tables and Stale; 03 Singletonize 1).
- **M4 Claude `AskUserQuestion` escalate.** Proposal emitted `permissionDecision: ask`. Final prints
  nothing, so the native question shows. `ask` is kept only as the spike-(a) fallback for tool
  permissions (03 Singletonize 2; 00 Risk 1).
- **M5 Codex `request_user_input`.** Proposal had `wait` return `{kind: unbrokered}` and a rail
  "waiting (unbrokered)" state. Final uses a `stalled` wait event, with `read` as the follow-up, and
  adds no rail state. Justified because herdr reports the agent as busy (00 Risk 4; 02 `wait`;
  05 Singletonize 7). Counted as dropped in section 4 (D1).
- **M6 Headless transfer.** Proposal moved the transfer controller into the backend. Final keeps it
  in the frontend and drives it through `drive://restart` and `drive://swap` window events, with
  `drive_request_resolve` reporting back (04 "Window listener"). Justification is only in the
  top-level file's "Implementation notes" ("Transfer controller stays in the frontend"), not in
  00 or 04. Documentation gap.
- **M7 Steal note.** Proposal wanted a rail note on `--force`. Final is inconsistent; see U1.
- **M8 CLI shapes.** `deny <msg>` and `escalate <reason>` became `--reason` flags. `send --wait`
  means "applied", not "turn finished", and `wait` is the turn primitive (02 Singletonize 3, 5, 6).
  `read --last` and `send -` added.
- **M9 Request table.** `send` was to sit "next to dispatch_requests". Final uses a separate
  `drive_requests` table because `dispatch_requests.task_id` is NOT NULL (01 Principle).
- **M10 Turn storage.** `worktree_sessions` gains `last_message` and `turn` in the proposal. Final
  uses a side table `session_replies`, so `session_upsert` cannot clobber it (01 Principle).
- **M11 Gate poll interval.** Proposal said ~250 ms. Final is 500 ms with `PRAGMA data_version`
  gating the real `SELECT` (03 Polling loop; 00 Risk 6; 02 `wait`).
- **M12 Pause.** Proposal said "Pause driver makes send and answer refuse". Final also refuses
  reset and swap, and `read`, `wait` and `prompts` still work (01 Pause; 02 Singletonize 10).
- **M13 Cycles.** Proposal rejected cycles. Final's one-level rule covers this, so cycles cannot
  exist (00 Scope In; 01 `drive_bind`).
- **M14 New scope limit (addition).** `send`, `read`, `reset` and `swap` need the project open and
  warm in the Keitora window. Not in the proposal. 00 Scope In (last bullet); 04 Scope limit and
  Singletonize 1. The user should confirm this.
- **M15 Stop capture (addition).** Proposal read `last_assistant_message` only. Final adds a
  transcript-file fallback for Claude and Codex (03 "Stop capture").
- **M16 Driven-hook check and upgrade warning (addition).** `keitora drive` warns when the driven
  worktree's hook file lacks the gate entry (02 `drive` step 2; 03 "Older CLI").

## 4. Intentionally dropped (reason in the docs)

- D1 Codex `request_user_input` rail state and `unbrokered` wait event. Herdr reports a blocked agent
  as busy, so Keitora cannot detect it (00 Risk 4; 02 `stalled`).
- D2 Telling the driven agent it is driven. "No clear gain" (00 Out; Resolved 4).
- D3 Chains and flat-or-chain question. One level only (00 Resolved 5).
- D4 Later verbs `stop`, `launch`, `interrupt`. Deferred as "possible later additions" (02 end).
- D5 Per-drive timeout as an open question. Resolved in favour of adding it (see M2).
- D6 `--force` stealing without a rail note is NOT a deliberate drop; see U1.
- D7 Proposal's `pty_read_history` for the "unbrokered" tail. Subsumed by `read` (02) and not
  needed once D1 is dropped.

## 5. Unintentionally dropped or inconsistent

- U1 **Steal rail note (inconsistent).** Proposal Scope: "`--force` steals it, with a note in the
  rail". 00 Resolved 7 keeps "a rail note". 05 Singletonize 5 says the steal note shows in the pane
  banner only, "not on the rail". The two docs disagree. Pick one, most likely the rail note, which
  is cheap if `forced_from` is already in activity, or update 00.
- U2 **"scope changes" dropped from the escalation contract (unintentional).** Proposal Idea 3 lists
  destructive, credentials, scope changes and ambiguity. 02 "Driver guidance" (and 00 Driver
  guidance) says destructive, credential, external-side-effect and ambiguous. Scope changes is gone
  and external-side-effect is new. Restore "scope changes" in both.
- U3 **Transfer-controller rationale missing from 00 and 04 (documentation gap).** See M6.

## 6. Other observations (not losses)

- `reset` CLI default timeout is 60 s (02) while the app's restart window is 120 s (04). A driver can
  see exit 2 while the reset still succeeds. The 02 text already says that a request still in flight
  is left alone, so this only needs a sentence in the driver guidance.
- The overlay banner hides the top terminal row (05 Singletonize 4), which is an accepted cost. Worth
  a visual check during the harness screenshots.
- No doc changes were made by this stage. No commit, no build.
