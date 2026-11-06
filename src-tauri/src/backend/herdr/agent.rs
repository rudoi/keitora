//! `agent` — pure helpers for herdr's first-class agent model.
//!
//! herdr launches agents by **kind** (`agent.start --kind claude|codex|…`) rather
//! than a shell command string. These helpers map Keitora's `CliKind` to a herdr
//! kind, build the extra args herdr appends to the agent's canonical executable,
//! translate herdr's `agent_status` into Keitora's busy `state_hint`, and derive a
//! stable, unique agent name. All pure and unit-tested.
//!
//! Wired into `HerdrBackend::spawn_agent` / `list_sessions`.

use keitora_tasks::{is_safe_claude_session_id, CliKind};

use crate::backend::{SessionKey, HINT_BUSY};

use super::ops::HerdrError;
use super::resume::ResumeDecision;

/// Keitora `CliKind` → the herdr `--kind` value.
pub fn herdr_kind(cli: CliKind) -> &'static str {
    match cli {
        CliKind::Claude => "claude",
        CliKind::Codex => "codex",
    }
}

/// Extra args herdr appends to the agent's canonical executable (`agent.start … -- <args>`).
///
/// The normal launch flags plus the resume choice (there is no shell `||` fallback:
/// an `agent.start` argv can't express one, so a failed resume is retried by the
/// caller with [`next_fallback`]):
/// - claude: `--dangerously-skip-permissions` when `autonomy`, then `--resume <id>`
///   / `--continue` / nothing.
/// - codex: the subcommand first (`resume <id>` / `resume --last` / nothing), then
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
            if is_safe_claude_session_id(id) {
                Some(Some(id.as_str()))
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
    }
    args
}

/// The next, weaker launch after `d` failed: resume → continue → fresh → none.
pub fn next_fallback(d: &ResumeDecision) -> Option<ResumeDecision> {
    match d {
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

/// Map a herdr `agent_status` (+ `launch_pending`) to Keitora's `state_hint`.
///
/// A still-launching, `working`, or `blocked` agent is BUSY; `idle`/`done`/unknown
/// (and any unrecognized status) are not. `apply_reconcile` only ever upgrades an
/// `idle` session to busy, so this can withhold a dispatch but never force one.
pub fn agent_status_to_state_hint(status: &str, launch_pending: bool) -> Option<String> {
    if launch_pending {
        return Some(HINT_BUSY.to_string());
    }
    match status {
        "working" | "blocked" => Some(HINT_BUSY.to_string()),
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
            vec!["--dangerously-bypass-hook-trust"]
        );
        assert_eq!(
            build_agent_args(CliKind::Codex, true, &FRESH),
            vec![
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
                vec!["resume", id, "--dangerously-bypass-hook-trust"],
            ),
            (
                codex,
                true,
                ResumeDecision::Continue,
                vec![
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
                vec!["resume", "--last", "--dangerously-bypass-hook-trust"],
            ),
            (
                codex,
                true,
                FRESH,
                vec![
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--dangerously-bypass-hook-trust",
                ],
            ),
            (codex, false, FRESH, vec!["--dangerously-bypass-hook-trust"]),
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
            vec!["resume", "--last", "--dangerously-bypass-hook-trust"]
        );
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
    fn status_map_busy_vs_idle() {
        let busy = Some(HINT_BUSY.to_string());
        assert_eq!(agent_status_to_state_hint("working", false), busy);
        assert_eq!(agent_status_to_state_hint("blocked", false), busy);
        // launch_pending is busy regardless of the status string
        assert_eq!(agent_status_to_state_hint("idle", true), busy);
        // idle/done/unknown are not busy
        assert_eq!(agent_status_to_state_hint("idle", false), None);
        assert_eq!(agent_status_to_state_hint("done", false), None);
        assert_eq!(agent_status_to_state_hint("unknown", false), None);
        assert_eq!(agent_status_to_state_hint("something-new", false), None);
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
    fn fnv1a32_known_vector() {
        // Empty input → FNV-1a 32 offset basis.
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
    }
}
