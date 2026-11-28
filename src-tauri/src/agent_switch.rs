//! Switching a worktree's agent.
//!
//! A switch changes which provider a worktree runs without carrying the
//! conversation over: the live pane is stopped, the new provider is persisted
//! with its stored conversation id cleared, and the frontend restarts the pane
//! fresh. Transfers (which do carry the conversation) stay limited to the
//! Claude/Codex pairs; a switch covers every other pair.

use keitora_tasks::{CliKind, FieldIntent, ProviderIds};
use tauri::State;

use crate::backend::{ResumeRecord, SessionKey, SharedBackend};
use crate::dispatch::DispatchGate;
use crate::transfer::{
    commit_then_persist, is_transfer_pair, load_project, persist_under_gate, persist_worktree_cli,
    record_resume, refuse_terminated, release_session_with, source_cli, with_recovery_lock,
    CommitCancel, Committed, TransferOutcome, WRITER_RELEASE_POLL, WRITER_RELEASE_TIMEOUT,
};
use crate::transfer_registry::TransferRegistry;

/// Refusals that need no I/O, checked in order: the same provider, a transfer
/// pair (those use Transfer), then an unavailable target.
pub(crate) fn check_switch(
    current: CliKind,
    target: CliKind,
    target_available: bool,
) -> Result<(), String> {
    if target == current {
        return Err(format!(
            "this worktree already runs {}",
            current.spec().title
        ));
    }
    if is_transfer_pair(current, target) {
        return Err(format!(
            "switching between {} and {} isn't supported; use Transfer",
            current.spec().title,
            target.spec().title
        ));
    }
    if !target_available {
        return Err(format!("{} isn't installed", target.spec().title));
    }
    Ok(())
}

/// What a switch persists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwitchPlan {
    pub cli: CliKind,
    pub ids: ProviderIds,
    pub resume: ResumeRecord,
}

/// The persist plan for a switch to `target`: the target's stored id is
/// cleared (so a stale id from an earlier switch is never resumed), the
/// source's id is kept, and the resume-store row is cleared.
pub(crate) fn switch_plan(target: CliKind) -> SwitchPlan {
    SwitchPlan {
        cli: target,
        ids: ProviderIds::keep().with(target, FieldIntent::Clear),
        resume: ResumeRecord::Cleared,
    }
}

/// Switch the worktree to `target_cli`, starting it fresh. Stops the live pane,
/// persists the new provider and returns [`TransferOutcome::Switched`]; the
/// frontend restarts the pane and then calls `transfer_settled`.
///
/// `Err` means nothing changed: a transfer or switch is already running, the
/// session is terminated, the target is the current provider or a transfer
/// pair, the target isn't installed, the live pane could not be stopped, or
/// persisting failed.
#[tauri::command]
pub async fn switch_worktree_cli(
    project: String,
    worktree: String,
    target_cli: CliKind,
    gate: State<'_, DispatchGate>,
    client: State<'_, SharedBackend>,
    registry: State<'_, TransferRegistry>,
    app: tauri::AppHandle,
) -> Result<TransferOutcome, String> {
    // Declared first so it outlives everything; dropping it releases the marker
    // unless the switch committed.
    let _guard = registry
        .begin(&project, &worktree)
        .map_err(|_| "a transfer or switch is already in progress for this worktree".to_string())?;
    let key = SessionKey {
        project: project.clone(),
        worktree: worktree.clone(),
    };
    refuse_terminated(client.inner().as_ref(), &key, "switching")?;

    let (user, config) = load_project(&project).await?;
    let current = source_cli(&user, &config, &worktree);
    // Claude is never unavailable: its launch falls back to the bare `claude`.
    let available = target_cli == CliKind::Claude
        || keitora_tasks::resolve_provider_path(target_cli, Some(&config), &user).is_some()
        || discover_target(target_cli);
    check_switch(current, target_cli, available)?;

    let backend = client.inner().clone();
    let registry = registry.inner().clone();
    // Recovery must not resume this session while the switch rewrites it.
    with_recovery_lock(client.recovery(), &key, async {
        let release_backend = backend.clone();
        let release_key = key.clone();
        tauri::async_runtime::spawn_blocking(move || {
            release_session_with(
                release_backend.as_ref(),
                &release_key,
                WRITER_RELEASE_TIMEOUT,
                WRITER_RELEASE_POLL,
            )
        })
        .await
        .map_err(|e| format!("stopping the session panicked: {e}"))??;

        let plan = switch_plan(target_cli);
        let persist_backend = backend.clone();
        let persist_key = key.clone();
        let committed = persist_under_gate(gate.inner(), move || {
            commit_then_persist(
                &registry,
                &persist_key.project,
                &persist_key.worktree,
                CommitCancel::Ignore,
                || {
                    persist_worktree_cli(&config, &persist_key.worktree, plan.cli, plan.ids)?;
                    record_resume(persist_backend.as_ref(), &persist_key, plan.resume);
                    Ok(())
                },
            )
        })
        .await?;
        match committed {
            Committed::Done(r) => r,
            // Unreachable with `Ignore`.
            Committed::Cancelled => Err("switch was cancelled".to_string()),
        }
    })
    .await?;

    // The frontend restarts the pane on `Switched`; that spawn writes the target's
    // hooks (and an agy worktree's trust) before the agent starts. herdr's
    // integration for a provider found late (an `agy` installed after start-up) is
    // installed here, before the outcome is returned.
    if target_cli.spec().integration_on_switch {
        let refresh = tauri::async_runtime::spawn_blocking(move || backend.refresh_integration());
        if let Err(e) = refresh.await {
            log::warn!("refreshing herdr's integration after the switch panicked: {e}");
        }
    }

    use tauri::Emitter as _;
    let _ = app.emit("worktrees://changed", ());
    Ok(TransferOutcome::Switched {
        from: current,
        to: target_cli,
    })
}

/// Whether `cli` is discovered on this machine: the cached discovery in the app,
/// nothing under test (tests never run a login shell).
fn discover_target(cli: CliKind) -> bool {
    #[cfg(test)]
    {
        let _ = cli;
        false
    }
    #[cfg(not(test))]
    {
        keitora_tasks::discover_cli_cached(cli).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeBackend;

    #[test]
    fn check_switch_refuses_same_cli() {
        assert_eq!(
            check_switch(CliKind::Claude, CliKind::Claude, true),
            Err("this worktree already runs Claude".to_string())
        );
    }

    #[test]
    fn check_switch_refuses_transfer_pairs() {
        assert_eq!(
            check_switch(CliKind::Claude, CliKind::Codex, true),
            Err("switching between Claude and Codex isn't supported; use Transfer".to_string())
        );
        assert_eq!(
            check_switch(CliKind::Codex, CliKind::Claude, true),
            Err("switching between Codex and Claude isn't supported; use Transfer".to_string())
        );
    }

    #[test]
    fn check_switch_refuses_unavailable_target() {
        assert_eq!(
            check_switch(CliKind::Claude, CliKind::Antigravity, false),
            Err("Antigravity isn't installed".to_string())
        );
    }

    #[test]
    fn check_switch_allows_the_four_switch_pairs() {
        for (from, to) in [
            (CliKind::Claude, CliKind::Antigravity),
            (CliKind::Codex, CliKind::Antigravity),
            (CliKind::Antigravity, CliKind::Claude),
            (CliKind::Antigravity, CliKind::Codex),
        ] {
            assert_eq!(check_switch(from, to, true), Ok(()), "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn check_switch_checks_same_before_pair_before_availability() {
        assert_eq!(
            check_switch(CliKind::Claude, CliKind::Claude, false),
            Err("this worktree already runs Claude".to_string())
        );
        assert_eq!(
            check_switch(CliKind::Codex, CliKind::Claude, false),
            Err("switching between Codex and Claude isn't supported; use Transfer".to_string())
        );
    }

    #[test]
    fn switch_plan_clears_target_id_and_keeps_source() {
        let plan = switch_plan(CliKind::Antigravity);
        assert_eq!(plan.cli, CliKind::Antigravity);
        assert_eq!(plan.ids.antigravity, FieldIntent::Clear);
        assert_eq!(plan.ids.claude, FieldIntent::Keep);
        assert_eq!(plan.ids.codex, FieldIntent::Keep);
        assert_eq!(plan.resume, ResumeRecord::Cleared);

        let plan = switch_plan(CliKind::Claude);
        assert_eq!(plan.cli, CliKind::Claude);
        assert_eq!(plan.ids.claude, FieldIntent::Clear);
        assert_eq!(plan.ids.codex, FieldIntent::Keep);
        assert_eq!(plan.ids.antigravity, FieldIntent::Keep);
    }

    #[test]
    fn switch_refuses_a_terminated_lane() {
        let fake = FakeBackend::new();
        let key = SessionKey {
            project: "proj".into(),
            worktree: "alpha".into(),
        };
        assert_eq!(refuse_terminated(&fake, &key, "switching"), Ok(()));
        fake.insert_terminated(key.clone());
        assert_eq!(
            refuse_terminated(&fake, &key, "switching"),
            Err("session is terminated; launch it before switching".to_string())
        );
    }

    #[test]
    fn switch_refuses_while_a_transfer_is_running() {
        let r = TransferRegistry::new();
        let _held = r.begin("proj", "alpha").unwrap();
        assert!(r.begin("proj", "alpha").is_err());
        assert!(r.begin("proj", "bravo").is_ok());
    }

    #[test]
    fn switch_stops_the_live_pane_before_persisting() {
        use crate::backend::fake::Call;
        use crate::backend::SessionSummary;
        use std::time::Duration;
        let fake = FakeBackend::new();
        let key = SessionKey {
            project: "proj".into(),
            worktree: "alpha".into(),
        };
        fake.push_list_sessions(Ok(vec![SessionSummary {
            key: key.clone(),
            gen: 3,
            state_hint: None,
        }]));
        fake.insert_live(key.clone());
        fake.flip_live_after(1, key.clone());
        fake.push_kill(Ok(()));
        release_session_with(
            &fake,
            &key,
            Duration::from_millis(150),
            Duration::from_millis(10),
        )
        .unwrap();
        let kills: Vec<_> = fake
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Kill { .. }))
            .collect();
        assert_eq!(kills.len(), 1);
    }

    #[test]
    fn transfer_outcome_switched_serializes_as_documented() {
        assert_eq!(
            serde_json::to_value(TransferOutcome::Switched {
                from: CliKind::Claude,
                to: CliKind::Antigravity,
            })
            .unwrap(),
            serde_json::json!({"kind": "switched", "from": "claude", "to": "antigravity"})
        );
    }
}
