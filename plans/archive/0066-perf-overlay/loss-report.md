# 0066 perf overlay: loss check

Compared: original `1978499`, refine `384e3aa`, current `plans/0066-perf-overlay.md` (HEAD 62ecdcb).

## User feedback (refine cycle 1)

| Requirement | Status | Where in current |
|---|---|---|
| No network IO, no header total | preserved | Scope Out; Decisions |
| Toggle = rail footer button + Cmd+Shift+P (Ctrl on Windows) | preserved (modified from orig "app chrome" + "Ctrl+Shift+P proposed") | Scope; Frontend Toggle; Decisions |
| Fixed panel over terminal area, not draggable/resizable | preserved | Frontend; Scope Out |
| `Other` expands into its top processes | preserved | Scope In; Frontend |
| CPU %, memory, disk IO | preserved | Scope Metrics |
| Sortable by busiest | preserved (default cpu desc) | Frontend `perf.ts` |
| ~2 s live, short sparkline history | preserved (TICK 2 s, HISTORY_LEN 150) | Goal; Technical Design |
| Attribution = Keitora sessions + path heuristics | preserved | Attribution order; Path inference |

## Original proposal and refine: scope, design, risks

| Item | Status | Justification in current doc |
|---|---|---|
| Goal: project > worktree > session, side-by-side comparison | preserved | Goal |
| Buckets Keitora / herdr / Other | preserved, modified | herdr wording changed to "non-pane children"; Keitora now includes WebView children. Attribution step 2 explains order |
| Memory = resident memory | modified | Working-set caveat and double-count risk added |
| Path heuristics (-Project=, cwd, cmdline) | preserved, modified | Exact rules, ambiguity -> None, caps added |
| ~5 min history per row | preserved | 150 samples |
| Demand-driven sampling | preserved | Scope; Lifecycle lease |
| Out: GPU, Job Objects, herdr patch, persisted history, unregistered worktrees, subagent split | preserved | Scope Out |
| Out: draggable panel, Esc-to-close, headless host | preserved (added in refine) | Scope Out |
| Architecture diagram (RootCache, sampler, attribute, PerfSnapshot, hook) | modified | Diagram now claim_owners / rollup; event payload null, UI pulls `perf_snapshot` |
| Attribution order 1-4, start-time PID-reuse guard | preserved | Attribution order |
| Session -> worktree 1:1, group by workspace id | preserved | Attribution order (last paragraph) |
| `attribute()` single pure fn | modified | Split into claim_owners + rollup; design detail |
| Lifecycle refcount + 10 s lease | modified | No refcount: one webview, one overlay |
| Low-memory headroom stretches tick to 5 s | preserved | Sampler step h |
| RootCache invalidation on generation change, ~350 ms CLI cost | preserved | roots.rs |
| Tests: attribute, roots, history, lease, TS view-model | preserved and expanded | Testing |
| Manual real-app smoke with UBT + Claude session | preserved | Testing (manual) |
| Dependencies: herdr >= 0.7, sysinfo 0.37 | preserved | Dependencies |
| Risks: short-lived undercount, detached workers -> Other, sampler cost, cwd reads, root latency | preserved | Risks |
| Open Q 4: Other expandable | resolved | Decisions |
| Open Q 2: shortcut and button | resolved | Decisions |
| Open Q 3: fixed panel | resolved | Decisions |

## Intentionally dropped

- **Network IO (feature and header total).** User decision. Original Open Q1 also floated an "elevated / ETW path later" follow-up. That note is not recorded in current. The feature drop covers it, but add it back if the follow-up matters.
- **Original Open Questions section.** Replaced by "Decisions (refine)". Resolved, not lost.
- **Original `Other` as a single total, not itemised.** Superseded by user feedback.

## UNINTENTIONALLY dropped

**None.**

Watch item (not counted): the elevated/ETW network follow-up note from original Open Q1 is gone (see Intentionally dropped).
