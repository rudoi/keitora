//! `agent` — pure helpers for herdr's first-class agent model.
//!
//! herdr launches agents by **kind** (`agent.start --kind claude|codex|…`) rather
//! than a shell command string. These helpers map Keitora's `CliKind` to a herdr
//! kind, build the extra args herdr appends to the agent's canonical executable,
//! translate herdr's `agent_status` into Keitora's busy `state_hint`, and derive a
//! stable, unique agent name. All pure and unit-tested.
//!
//! Wired into `HerdrBackend::spawn_agent` / `list_sessions`.

use keitora_tasks::{is_safe_session_id, CliKind, CLI_COUNT};

use crate::backend::{SessionKey, HINT_BUSY, HINT_WAITING};

use super::ops::HerdrError;
use super::resume::ResumeDecision;

/// Keitora `CliKind` → the herdr `--kind` value.
pub fn herdr_kind(cli: CliKind) -> &'static str {
    cli.spec().herdr_kind
}

/// Global codex flags that turn off its startup update check. Without them a codex
/// that finds a newer release parks on an interactive "Update available" screen and
/// never becomes ready, which a launch would read as a failure. Unknown `-c` keys
/// are accepted silently, so a codex without this key is unaffected.
pub const CODEX_NO_UPDATE_CHECK: [&str; 2] = ["-c", "check_for_update_on_startup=false"];

/// Extra args herdr appends to the agent's canonical executable (`agent.start … -- <args>`).
///
/// The normal launch flags plus the resume choice (there is no shell `||` fallback:
/// an `agent.start` argv can't express one, so a failed resume is retried by the
/// caller with [`next_fallback`]):
/// - claude: `--dangerously-skip-permissions` when `autonomy`, then `--resume <id>`
///   / `--continue` / nothing.
/// - agy: `--dangerously-skip-permissions` when `autonomy`, then `--conversation <id>`
///   / `-c` / nothing.
/// - codex: [`CODEX_NO_UPDATE_CHECK`] first (global flags precede any subcommand),
///   then the subcommand (`resume <id>` / `resume --last` / nothing), then
///   `--dangerously-bypass-approvals-and-sandbox` when `autonomy`, and ALWAYS
///   `--dangerously-bypass-hook-trust` last (trusts Keitora's `.codex/hooks.json`).
///
/// An id that is not a plain session id (it is passed as a bare argument) degrades
/// the resume to a continue.
pub fn build_agent_args(cli: CliKind, autonomy: bool, resume: &ResumeDecision) -> Vec<String> {
    build_agent_args_opts(cli, autonomy, resume, true)
}

/// [`build_agent_args`] with the optional codex `--dangerously-bypass-hook-trust`
/// flag under the caller's control. It is the only optional flag: the autonomy
/// flag is user policy and is never dropped, and claude args ignore `hook_trust`.
pub fn build_agent_args_opts(
    cli: CliKind,
    autonomy: bool,
    resume: &ResumeDecision,
    hook_trust: bool,
) -> Vec<String> {
    let resume = match resume {
        ResumeDecision::Resume { cli: for_cli, id } => {
            debug_assert_eq!(*for_cli, cli, "resume decision for another cli");
            if is_safe_session_id(id) {
                Some(Some(id.as_str()))
            } else if cli == CliKind::Antigravity {
                // An unusable id starts fresh; `-c` is not trusted for this cli.
                None
            } else {
                Some(None)
            }
        }
        ResumeDecision::Continue => Some(None),
        ResumeDecision::Fresh => None,
    };
    let mut args: Vec<String> = Vec::new();
    match cli {
        CliKind::Claude => {
            if autonomy {
                args.push("--dangerously-skip-permissions".into());
            }
            match resume {
                Some(Some(id)) => args.extend(["--resume".into(), id.to_string()]),
                Some(None) => args.push("--continue".into()),
                None => {}
            }
        }
        CliKind::Codex => {
            args.extend(CODEX_NO_UPDATE_CHECK.map(String::from));
            match resume {
                Some(Some(id)) => args.extend(["resume".into(), id.to_string()]),
                Some(None) => args.extend(["resume".into(), "--last".into()]),
                None => {}
            }
            if autonomy {
                args.push("--dangerously-bypass-approvals-and-sandbox".into());
            }
            if hook_trust {
                args.push("--dangerously-bypass-hook-trust".into());
            }
        }
        CliKind::Antigravity => {
            if autonomy {
                args.push("--dangerously-skip-permissions".into());
            }
            match resume {
                Some(Some(id)) => args.extend(["--conversation".into(), id.to_string()]),
                Some(None) => args.push("-c".into()),
                None => {}
            }
        }
    }
    args
}

/// The next, weaker launch after `d` failed: resume → continue → fresh → none.
pub fn next_fallback(d: &ResumeDecision) -> Option<ResumeDecision> {
    match d {
        // A failed Antigravity resume skips `-c` and starts fresh.
        ResumeDecision::Resume {
            cli: CliKind::Antigravity,
            ..
        } => Some(ResumeDecision::Fresh),
        ResumeDecision::Resume { .. } => Some(ResumeDecision::Continue),
        ResumeDecision::Continue => Some(ResumeDecision::Fresh),
        ResumeDecision::Fresh => None,
    }
}

/// Did `agent start` fail because the agent exited before becoming interactive
/// (or herdr lost its name the same way)? Only such a launch is retried with a
/// weaker resume choice; a timeout or any other error is not.
pub fn is_early_exit(e: &HerdrError) -> bool {
    matches!(
        e.code(),
        Some("agent_start_failed" | "agent_name_not_found")
    )
}

/// Whether `screen` is codex's startup update screen (the only CLI with one): an
/// "Update available!" heading over "Update now" / "Skip" / "Skip until next
/// version" choices. The heading and both distinctive choices must all be present,
/// so transcript text that merely mentions an update rarely matches.
pub fn is_codex_update_screen(screen: &str) -> bool {
    screen.contains("Update available")
        && screen.contains("Skip until next version")
        && screen.contains("Update now")
}

/// Whether a failed `agent start` may have left codex on its update screen: herdr's
/// own agent-startup timeout, answered by herdr. A transport failure (the CLI call
/// itself timing out, herdr unreachable, output that would not parse) says nothing
/// about the pane, so the pane is not read for one.
pub fn start_error_may_show_update_screen(e: &HerdrError) -> bool {
    match e {
        HerdrError::Api { code, .. } if code == "timeout" => true,
        HerdrError::Api { .. } | HerdrError::Exit { .. } => super::is_timeout_text(&e.to_string()),
        HerdrError::Timeout | HerdrError::SpawnFailed { .. } | HerdrError::Parse { .. } => false,
    }
}

/// Did `agent start` fail because another pane still holds the agent's name?
pub fn is_name_taken(e: &HerdrError) -> bool {
    e.code() == Some("agent_name_taken")
}

/// Map a herdr `agent_status` (+ `launch_pending`) to Keitora's `state_hint`.
///
/// A still-launching or `working` agent is BUSY; a `blocked` one is WAITING on the
/// user; `idle`/`done`/unknown (and any unrecognized status) carry no hint.
/// A hint only ever moves a live session away from `idle` (to busy or waiting), so
/// it can withhold work but never force it.
pub fn agent_status_to_state_hint(status: &str, launch_pending: bool) -> Option<String> {
    if launch_pending {
        return Some(HINT_BUSY.to_string());
    }
    match status {
        "working" => Some(HINT_BUSY.to_string()),
        "blocked" => Some(HINT_WAITING.to_string()),
        _ => None, // idle, done, unknown, or anything unrecognized
    }
}

/// A stable, unique herdr agent name for a session key.
///
/// herdr requires agent names matching `[a-z][a-z0-9_-]{0,31}` and unique among live
/// agents. `name: kind` alone would collide across worktrees, so we suffix an
/// FNV-1a/32 hash of `"project/worktree"`: `{kind}-{hash:08x}` (e.g. `claude-1a2b3c4d`).
pub fn agent_name(kind: &str, key: &SessionKey) -> String {
    let id = format!("{}/{}", key.project, key.worktree);
    format!("{kind}-{:08x}", fnv1a32(id.as_bytes()))
}

/// Every agent name (one per `CliKind`, in [`CliKind::ALL`] order) a session key can
/// hold, sharing the hash suffix: `[claude-<hash>, codex-<hash>, agy-<hash>]`.
pub fn agent_names_for(key: &SessionKey) -> [String; CLI_COUNT] {
    CliKind::ALL.map(|c| agent_name(c.spec().herdr_kind, key))
}

/// FNV-1a 32-bit hash (stable across runs/platforms; no external dep).
fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_mapping() {
        assert_eq!(herdr_kind(CliKind::Claude), "claude");
        assert_eq!(herdr_kind(CliKind::Codex), "codex");
        assert_eq!(herdr_kind(CliKind::Antigravity), "agy");
    }

    #[test]
    fn antigravity_args_and_fallback_chain() {
        let args = |autonomy, d: &ResumeDecision| {
            build_agent_args_opts(CliKind::Antigravity, autonomy, d, true)
        };
        assert_eq!(
            args(true, &resume(CliKind::Antigravity, "abc-1")),
            ["--dangerously-skip-permissions", "--conversation", "abc-1"]
        );
        assert_eq!(args(false, &ResumeDecision::Continue), ["-c"]);
        assert!(args(false, &FRESH).is_empty());
        // An unusable id starts fresh instead of `-c`.
        assert!(args(false, &resume(CliKind::Antigravity, "a b")).is_empty());
        // A failed resume skips `-c`; Claude and Codex still pass through `Continue`.
        assert_eq!(
            next_fallback(&resume(CliKind::Antigravity, "abc-1")),
            Some(FRESH)
        );
        assert_eq!(
            next_fallback(&resume(CliKind::Claude, "abc-1")),
            Some(ResumeDecision::Continue)
        );
        assert_eq!(
            next_fallback(&resume(CliKind::Codex, "abc-1")),
            Some(ResumeDecision::Continue)
        );
        assert_eq!(next_fallback(&ResumeDecision::Continue), Some(FRESH));
    }

    const FRESH: ResumeDecision = ResumeDecision::Fresh;

    fn resume(cli: CliKind, id: &str) -> ResumeDecision {
        ResumeDecision::Resume { cli, id: id.into() }
    }

    #[test]
    fn claude_args_autonomy_toggles_skip_permissions() {
        assert!(build_agent_args(CliKind::Claude, false, &FRESH).is_empty());
        assert_eq!(
            build_agent_args(CliKind::Claude, true, &FRESH),
            vec!["--dangerously-skip-permissions"]
        );
    }

    #[test]
    fn codex_args_always_bypass_hook_trust_plus_autonomy() {
        // Hook-trust bypass is unconditional; approvals/sandbox bypass is autonomy-gated.
        assert_eq!(
            build_agent_args(CliKind::Codex, false, &FRESH),
            vec![
                "-c",
                "check_for_update_on_startup=false",
                "--dangerously-bypass-hook-trust",
            ]
        );
        assert_eq!(
            build_agent_args(CliKind::Codex, true, &FRESH),
            vec![
                "-c",
                "check_for_update_on_startup=false",
                "--dangerously-bypass-approvals-and-sandbox",
                "--dangerously-bypass-hook-trust",
            ]
        );
    }

    #[test]
    fn build_agent_args_matrix() {
        let id = "0b9e6c3e-1f2a-4c5d-8e7f-a1b2c3d4e5f6";
        let claude = CliKind::Claude;
        let codex = CliKind::Codex;
        let ag = CliKind::Antigravity;
        let cases: Vec<(CliKind, bool, ResumeDecision, Vec<&str>)> = vec![
            (
                claude,
                true,
                resume(claude, id),
                vec!["--dangerously-skip-permissions", "--resume", id],
            ),
            (claude, false, resume(claude, id), vec!["--resume", id]),
            (
                claude,
                true,
                ResumeDecision::Continue,
                vec!["--dangerously-skip-permissions", "--continue"],
            ),
            (claude, false, ResumeDecision::Continue, vec!["--continue"]),
            (claude, true, FRESH, vec!["--dangerously-skip-permissions"]),
            (claude, false, FRESH, vec![]),
            (
                codex,
                true,
                resume(codex, id),
                vec![
                    "-c",
                    "check_for_update_on_startup=false",
                    "resume",
                    id,
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--dangerously-bypass-hook-trust",
                ],
            ),
            (
                codex,
                false,
                resume(codex, id),
                vec![
                    "-c",
                    "check_for_update_on_startup=false",
                    "resume",
                    id,
                    "--dangerously-bypass-hook-trust",
                ],
            ),
            (
                codex,
                true,
                ResumeDecision::Continue,
                vec![
                    "-c",
                    "check_for_update_on_startup=false",
                    "resume",
                    "--last",
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--dangerously-bypass-hook-trust",
                ],
            ),
            (
                codex,
                false,
                ResumeDecision::Continue,
                vec![
                    "-c",
                    "check_for_update_on_startup=false",
                    "resume",
                    "--last",
                    "--dangerously-bypass-hook-trust",
                ],
            ),
            (
                codex,
                true,
                FRESH,
                vec![
                    "-c",
                    "check_for_update_on_startup=false",
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--dangerously-bypass-hook-trust",
                ],
            ),
            (
                codex,
                false,
                FRESH,
                vec![
                    "-c",
                    "check_for_update_on_startup=false",
                    "--dangerously-bypass-hook-trust",
                ],
            ),
            (
                ag,
                true,
                resume(ag, id),
                vec!["--dangerously-skip-permissions", "--conversation", id],
            ),
            (ag, false, resume(ag, id), vec!["--conversation", id]),
            (
                ag,
                true,
                ResumeDecision::Continue,
                vec!["--dangerously-skip-permissions", "-c"],
            ),
            (ag, false, ResumeDecision::Continue, vec!["-c"]),
            (ag, true, FRESH, vec!["--dangerously-skip-permissions"]),
            (ag, false, FRESH, vec![]),
        ];
        for (cli, autonomy, decision, want) in cases {
            assert_eq!(
                build_agent_args(cli, autonomy, &decision),
                want,
                "{cli:?} autonomy={autonomy} {decision:?}"
            );
        }
    }

    /// Every decision a launch can carry, for the matrix tests.
    fn all_decisions() -> Vec<ResumeDecision> {
        vec![
            ResumeDecision::Fresh,
            ResumeDecision::Continue,
            resume(CliKind::Claude, "sess-1"),
            resume(CliKind::Codex, "thr-1"),
            resume(CliKind::Codex, "a b; --last"),
        ]
    }

    fn decision_cli(d: &ResumeDecision, fallback: CliKind) -> CliKind {
        match d {
            ResumeDecision::Resume { cli, .. } => *cli,
            _ => fallback,
        }
    }

    #[test]
    fn build_agent_args_opts_true_equals_legacy() {
        for cli in [CliKind::Claude, CliKind::Codex] {
            for autonomy in [false, true] {
                for d in all_decisions() {
                    if decision_cli(&d, cli) != cli {
                        continue;
                    }
                    assert_eq!(
                        build_agent_args(cli, autonomy, &d),
                        build_agent_args_opts(cli, autonomy, &d, true),
                        "{cli:?} autonomy={autonomy} {d:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn build_agent_args_opts_false_drops_only_hook_trust() {
        const HOOK: &str = "--dangerously-bypass-hook-trust";
        for cli in [CliKind::Claude, CliKind::Codex] {
            for autonomy in [false, true] {
                for d in all_decisions() {
                    if decision_cli(&d, cli) != cli {
                        continue;
                    }
                    let with = build_agent_args_opts(cli, autonomy, &d, true);
                    let without = build_agent_args_opts(cli, autonomy, &d, false);
                    let expect: Vec<String> = with
                        .iter()
                        .filter(|a| a.as_str() != HOOK)
                        .cloned()
                        .collect();
                    assert_eq!(without, expect, "{cli:?} autonomy={autonomy} {d:?}");
                    if cli == CliKind::Claude {
                        assert_eq!(with, without, "claude never carries the flag");
                    } else {
                        assert_eq!(with.len(), without.len() + 1);
                        assert_eq!(
                            autonomy,
                            without
                                .iter()
                                .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"),
                            "the autonomy flag is never dropped"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn unsafe_resume_id_degrades_to_continue() {
        let bad = "abc; rm -rf /";
        assert_eq!(
            build_agent_args(CliKind::Claude, false, &resume(CliKind::Claude, bad)),
            vec!["--continue"]
        );
        assert_eq!(
            build_agent_args(CliKind::Codex, false, &resume(CliKind::Codex, "--last")),
            vec![
                "-c",
                "check_for_update_on_startup=false",
                "resume",
                "--last",
                "--dangerously-bypass-hook-trust",
            ]
        );
    }

    /// For every codex launch the update-check override leads (global flags must
    /// precede `resume`) and hook trust stays last; claude never carries it.
    #[test]
    fn codex_update_check_prefix_leads_and_hook_trust_stays_last() {
        for autonomy in [false, true] {
            for d in all_decisions() {
                if decision_cli(&d, CliKind::Codex) == CliKind::Codex {
                    let args = build_agent_args(CliKind::Codex, autonomy, &d);
                    assert_eq!(&args[..2], CODEX_NO_UPDATE_CHECK, "{autonomy} {d:?}");
                    assert_eq!(
                        args.last().map(String::as_str),
                        Some("--dangerously-bypass-hook-trust"),
                        "{autonomy} {d:?}"
                    );
                    let without = build_agent_args_opts(CliKind::Codex, autonomy, &d, false);
                    assert_eq!(&without[..2], CODEX_NO_UPDATE_CHECK, "{autonomy} {d:?}");
                }
                if decision_cli(&d, CliKind::Claude) == CliKind::Claude {
                    let args = build_agent_args(CliKind::Claude, autonomy, &d);
                    assert!(!args.iter().any(|a| a.contains("check_for_update")));
                }
            }
        }
    }

    /// The codex startup update screen, as `pane read` returns it (scrubbed).
    const CODEX_UPDATE_SCREEN: &str = "  \u{2728} Update available! 0.160.0 -> 0.163.0

  Release notes: https://example.com/releases/latest

\u{203a} 1. Update now (runs `npm install -g @openai/codex`)
  2. Skip
  3. Skip until next version

  Press enter to continue
";

    /// An ordinary codex start screen.
    const CODEX_NORMAL_SCREEN: &str = ">_ OpenAI Codex (v0.160.0)

 model:     gpt-5   /model to change
 directory: ~/repos/proj

 To get started, describe a task or try one of these commands:
\u{203a}
";

    #[test]
    fn codex_update_screen_table() {
        let cases: &[(&str, &str, bool)] = &[
            ("update screen", CODEX_UPDATE_SCREEN, true),
            (
                "heading and skip-until only",
                "Update available!\n3. Skip until next version",
                false,
            ),
            (
                "heading and update-now only",
                "Update available!\n1. Update now",
                false,
            ),
            (
                "all three, anywhere on the screen",
                "Update available! 0.1.0 -> 0.2.0\n1. Update now\n3. Skip until next version",
                true,
            ),
            (
                "partial: heading only",
                "  Update available! 0.160.0 -> 0.163.0",
                false,
            ),
            (
                "partial: choices only",
                "1. Update now\n2. Skip\n3. Skip until next version",
                false,
            ),
            ("normal codex screen", CODEX_NORMAL_SCREEN, false),
            (
                "unrelated",
                "$ cargo build\n   Compiling proj v0.1.0",
                false,
            ),
            ("empty", "", false),
        ];
        for (label, screen, want) in cases {
            assert_eq!(is_codex_update_screen(screen), *want, "{label}");
        }
    }

    #[test]
    fn only_herdrs_startup_timeout_may_show_the_update_screen() {
        let api = |code: &str, message: &str| HerdrError::Api {
            code: code.into(),
            message: message.into(),
        };
        assert!(start_error_may_show_update_screen(&api("timeout", "")));
        assert!(start_error_may_show_update_screen(&api(
            "",
            "timed out waiting for agent startup"
        )));
        assert!(!start_error_may_show_update_screen(&api(
            "pane_not_found",
            "gone"
        )));
        assert!(!start_error_may_show_update_screen(&HerdrError::Timeout));
        assert!(!start_error_may_show_update_screen(
            &HerdrError::SpawnFailed {
                os_error: Some(2),
                message: "not found".into(),
            }
        ));
        assert!(!start_error_may_show_update_screen(&HerdrError::Parse {
            message: "bad".into(),
        }));
        assert!(!start_error_may_show_update_screen(&HerdrError::Exit {
            code: Some(1),
            stderr: "connection refused".into(),
        }));
    }

    #[test]
    fn early_exit_is_only_the_two_startup_codes() {
        let api = |code: &str| HerdrError::Api {
            code: code.into(),
            message: String::new(),
        };
        assert!(is_early_exit(&api("agent_start_failed")));
        assert!(is_early_exit(&api("agent_name_not_found")));
        assert!(!is_early_exit(&api("timeout")));
        assert!(!is_early_exit(&api("pane_not_found")));
        assert!(!is_early_exit(&HerdrError::Timeout));
    }

    #[test]
    fn next_fallback_chain() {
        let r = resume(CliKind::Claude, "x");
        assert_eq!(next_fallback(&r), Some(ResumeDecision::Continue));
        assert_eq!(next_fallback(&ResumeDecision::Continue), Some(FRESH));
        assert_eq!(next_fallback(&FRESH), None);
    }

    #[test]
    fn next_fallback_claude_and_codex_chains_pass_through_continue() {
        for cli in [CliKind::Claude, CliKind::Codex] {
            let mut chain = vec![resume(cli, "x")];
            while let Some(next) = next_fallback(chain.last().unwrap()) {
                chain.push(next);
            }
            assert_eq!(
                chain,
                vec![resume(cli, "x"), ResumeDecision::Continue, FRESH],
                "{cli:?}"
            );
        }
    }

    #[test]
    fn next_fallback_antigravity_skips_continue() {
        let id = "0b9e6c3e-1f2a-4c5d-8e7f-a1b2c3d4e5f6";
        let r = resume(CliKind::Antigravity, id);
        assert_eq!(next_fallback(&r), Some(FRESH));
        assert_eq!(next_fallback(&FRESH), None);
    }

    #[test]
    fn antigravity_ignores_hook_trust() {
        let on = build_agent_args_opts(CliKind::Antigravity, false, &FRESH, true);
        let off = build_agent_args_opts(CliKind::Antigravity, false, &FRESH, false);
        assert_eq!(on, off);
        assert!(on.is_empty());
    }

    #[test]
    fn unsafe_antigravity_id_degrades_to_fresh() {
        let r = resume(CliKind::Antigravity, "a b");
        assert!(build_agent_args(CliKind::Antigravity, false, &r).is_empty());
        assert_eq!(
            build_agent_args(CliKind::Antigravity, true, &r),
            vec!["--dangerously-skip-permissions"]
        );
    }

    #[test]
    fn agent_status_maps_blocked_to_waiting() {
        let busy = Some(HINT_BUSY.to_string());
        assert_eq!(agent_status_to_state_hint("working", false), busy);
        assert_eq!(
            agent_status_to_state_hint("blocked", false),
            Some(HINT_WAITING.to_string())
        );
        // launch_pending is busy regardless of the status string
        assert_eq!(agent_status_to_state_hint("idle", true), busy);
        assert_eq!(agent_status_to_state_hint("blocked", true), busy);
        assert_eq!(agent_status_to_state_hint("bogus", true), busy);
        // idle/done/unknown are not busy
        assert_eq!(agent_status_to_state_hint("idle", false), None);
        assert_eq!(agent_status_to_state_hint("done", false), None);
        assert_eq!(agent_status_to_state_hint("unknown", false), None);
        assert_eq!(agent_status_to_state_hint("something-new", false), None);
        assert_eq!(agent_status_to_state_hint("bogus", false), None);
    }

    #[test]
    fn agent_name_is_stable_unique_and_valid() {
        let k1 = SessionKey {
            project: "proj".into(),
            worktree: "gamma".into(),
        };
        let k2 = SessionKey {
            project: "proj".into(),
            worktree: "beta".into(),
        };
        let n1 = agent_name("claude", &k1);
        // Stable across calls.
        assert_eq!(n1, agent_name("claude", &k1));
        // Distinct worktrees → distinct names (no collision).
        assert_ne!(n1, agent_name("claude", &k2));
        // Matches herdr's name grammar: ^[a-z][a-z0-9_-]{0,31}$.
        assert!(n1.len() <= 32, "name too long: {n1}");
        let mut chars = n1.chars();
        assert!(chars.next().unwrap().is_ascii_lowercase());
        assert!(n1
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'));
    }

    #[test]
    fn agent_names_for_has_one_name_per_kind_with_a_shared_suffix() {
        let k = SessionKey {
            project: "proj".into(),
            worktree: "alpha".into(),
        };
        let [c, x, a] = agent_names_for(&k);
        assert_eq!(c, agent_name("claude", &k));
        assert_eq!(x, agent_name("codex", &k));
        assert!(a.starts_with("agy-"));
        let suffix = c.split_once('-').unwrap().1;
        assert_eq!(suffix.len(), 8);
        assert_eq!(suffix, x.split_once('-').unwrap().1);
        assert_eq!(suffix, a.split_once('-').unwrap().1);
    }

    #[test]
    fn is_name_taken_matches_only_its_code() {
        let err = |code: &str| HerdrError::Api {
            code: code.into(),
            message: "m".into(),
        };
        assert!(is_name_taken(&err("agent_name_taken")));
        assert!(!is_name_taken(&err("agent_start_failed")));
        assert!(!is_name_taken(&HerdrError::Timeout));
    }

    #[test]
    fn fnv1a32_known_vector() {
        // Empty input → FNV-1a 32 offset basis.
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
    }
}
