# 0076 loss check: dependency setup

Baseline: original `372d012` (225 lines). Final: `plans/0076-dependency-setup.md` at `6253117` (630 lines). User requirements (a)-(h) are authoritative. Refinement history: one cycle (floor 0.9.5, prompt on upgrade, auto refine).

## User requirements

| Req | Status | Justification in final doc |
|---|---|---|
| (a) setup as part of initial install | modified | Reinterpreted as in-app first run; Decisions: "No NSIS/dmg installer hooks" (follows (c)) |
| (b) herdr only; git/claude/codex detected | preserved | Decisions row 1; Doctor |
| (c) in-app: onboarding + rail card + Settings; never silent | preserved | Decisions "In-app", "Never installs silently" |
| (d) tiered: script, sha256 download, manual + browse | preserved | Tier 1 / Tier 2 / Manual |
| (e) floor 0.9.5 / protocol 22; newest compatible | preserved | Release selection; Version floor |
| (f) prompt upgrade on older herdr | preserved | Upgrade prompt (soft TooOld, once per version and session) |
| (g) onboarding skippable | preserved | Decisions "Onboarding" |
| (h) macOS Intel | preserved | Decisions "Platforms"; `MacosX86_64` in Platform enum |

## Original proposal

| Original item | Status | Justification / location |
|---|---|---|
| Goal, `discover_exe` unchanged, managed root `<KEITORA_HOME>/bin/herdr/<ver>/` | preserved | Goal; Discovery; Tier 2 |
| Discovery step ranking managed root before PATH | modified | Replaced by pinning `[pty] herdr_bin`; "no discovery reordering" |
| Git floor >= 2.23 | intentionally dropped | Doctor: "informational (no floor in v1)" |
| claude/codex presence and version | preserved | Doctor reuses `MachineProber` |
| Floor 0.9.3 (open Q3) | modified | Floor 0.9.5, per (e) |
| Install whatever manifest calls latest | modified | Newest protocol-22 release >= floor and >= installed; tier 1 only when it equals latest |
| Below floor: flag and offer, never force | modified | Soft prompt; `ProtocolMismatch` is hard, justified under Severity |
| Version parse of `herdr 0.9.x (protocol N)` form | modified | Bare and prefixed forms; protocol from `status server --json` |
| Tier choice as pure function; offline to Manual | preserved | `preflight.rs` |
| Tier 1 commands, env scrub, 300 s timeout | preserved | Tier 1 |
| Tier 1 cancelable | intentionally dropped | Scope Out |
| Tier 1 exit 0 without herdr = Incomplete | preserved | Tier 1 success rule |
| Tier 2 https, sha256 before unpack, chmod, atomic rename, reuse | preserved | Tier 2 (adds zip `enclosed_name` and size cap) |
| Tier 2 keep two newest + in-use | modified | In-use plus one previous |
| Manual (commands, Browse validated), Copy details | preserved | Manual; `deps_set_herdr_bin`; `details_report` |
| Failure kinds incl. ProxyOrTls, Io | modified | ProxyOrTls folded into Network; Io became Other |
| Progress stages, forward-only, one terminal event | preserved | Stages; Testing |
| Byte-accurate progress; daily newer-version check | intentionally dropped | Scope Out |
| Update path by layout (managed, standalone, brew/custom = instructions only) | UNINTENTIONALLY DROPPED | No layout guard in final; see below |
| Onboarding step (skippable); rail card; restart confirmation | preserved | Frontend; Upgrade sequence step 2 |
| Settings section, lowercase | modified | Title case "Dependencies", matching `Daemon` / `Updates` titles |
| Doctor rows with floor column | UNINTENTIONALLY DROPPED | `DoctorRow` has no floor field; see below |
| Settings rows + Install/Update/Browse + Re-check | preserved | Settings; "Clear (use discovery)" added |
| Discovery-ordering test | modified | Replaced by pin precedence; justified by no reordering |
| Frontend component tests with mocked invokes | modified | No DOM env in vitest; pure helpers plus `.harness/`, per Testing |
| Tier, manifest, floor, progress, after_failure tests | preserved | Testing |
| Dev home skips tier 1 | preserved (stricter) | Tier 1 requires `KEITORA_HOME` and `KEITORA_HERDR_BIN` unset |
| Locked Windows binary; manifest drift; corporate policy; integration side effect | preserved | Upgrade sequence; Release selection; Risks |
| Docs, Linux out, herdr update check off, deps (reqwest, sha2, zip) | preserved | Scope; Risks; Dependencies |

## UNINTENTIONALLY DROPPED

1. **Update path by install layout.** The original forbade auto-replacing a below-floor herdr that is brew-managed or at a custom path (instructions only). The final never mentions this. Its Upgrade sequence and Tier choice ignore where the existing herdr lives, so it would install a second herdr and pin it. Fix: add the layout rule, or state the policy.
2. **TooOldAfterInstall.** Tier 1 says failure is "`Incomplete` / `TooOldAfterInstall`", but the `FailureKind` enum omits it. Fix: add the variant to the enum, or map it to `Incomplete` in the text.
3. **Doctor floor field.** The original row carried `floor`; `DoctorRow` now has only tool, path, version, status, action. Herdr's floor is conveyed through `TooOld`, but the floor value is not shown. Fix: add the field, or record the drop.

Test gaps only, not drops: no listed test for macOS bare-binary unpack or the "no asset" tier row.
