//! Tauri commands for agent CLI updates. Every command that probes, reads the backend or
//! writes config runs on a blocking thread.

use keitora_tasks::CliKind;
use tauri::{AppHandle, Manager, State};

use super::{
    live_sessions, spawn_watcher, AgentUpdates, AgentUpdatesSnapshot, RunPlan, UpdateSession,
};
use crate::backend::{SessionKey, SharedBackend};

/// Run `f` with the app on a blocking thread.
async fn blocking<T: Send + 'static>(
    app: AppHandle,
    f: impl FnOnce(AppHandle) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || f(app))
        .await
        .map_err(|e| format!("agent update task failed: {e}"))?
}

fn backend(app: &AppHandle) -> SharedBackend {
    app.state::<SharedBackend>().inner().clone()
}

/// The current picture; seeds the UI on mount and after a webview reload.
#[tauri::command(async)]
pub fn agent_updates_status(updates: State<'_, AgentUpdates>) -> AgentUpdatesSnapshot {
    updates.snapshot()
}

/// Check both CLIs. Unless `force`, a check right after another answers with its result.
/// Also emits `agent-updates://status` when it probes.
#[tauri::command(async)]
pub async fn agent_updates_check(
    app: AppHandle,
    force: bool,
) -> Result<AgentUpdatesSnapshot, String> {
    blocking(app, move |app| Ok(app.state::<AgentUpdates>().check(force))).await
}

/// Skip `version` of `cli`: persisted in `[updates] agent_skip_{codex,claude}`, then
/// re-assessed. A newer release shows the banner again.
#[tauri::command(async)]
pub async fn agent_update_skip(
    app: AppHandle,
    cli: CliKind,
    version: String,
) -> Result<AgentUpdatesSnapshot, String> {
    blocking(app, move |app| {
        keitora_tasks::UserConfig::update(|cfg| match cli {
            CliKind::Codex => cfg.updates.agent_skip_codex = Some(version),
            CliKind::Claude => cfg.updates.agent_skip_claude = Some(version),
        })
        .map_err(|e| e.to_string())?;
        Ok(app.state::<AgentUpdates>().skip_changed(cli))
    })
    .await
}

/// Start updating `cli`. The command is chosen here from the assessed install kind (the
/// frontend never sends one); refused when Keitora has no command for this install. Any
/// earlier update pane is closed, then the reserved pane is opened as a plain shell and
/// the update is typed into it, written for that pane's shell (PowerShell, POSIX or
/// fish), before this replies. The reply is what runs (display text) and where; the
/// dialog's pane only attaches. A watcher reads the pane's history until the exit line
/// and then emits `agent-updates://run`.
#[tauri::command(async)]
pub async fn agent_update_run(app: AppHandle, cli: CliKind) -> Result<RunPlan, String> {
    blocking(app, move |app| {
        let updates = app.state::<AgentUpdates>();
        if updates.snapshot().checked_at.is_none() {
            updates.check(false);
        }
        let backend = backend(&app);
        let ticket = updates.start_run(&*backend, cli, cfg!(windows))?;
        let plan = ticket.plan.clone();
        spawn_watcher(app.clone(), backend, ticket);
        Ok(plan)
    })
    .await
}

/// Close the update pane and stop watching it. Safe to call when there is none.
#[tauri::command(async)]
pub async fn agent_update_close(app: AppHandle) -> Result<(), String> {
    blocking(app, |app| {
        app.state::<AgentUpdates>().close();
        backend(&app)
            .kill_live(&SessionKey::agent_update(), None)
            .map_err(|e| e.to_string())
    })
    .await
}

/// The live sessions running `cli` (a stop-first prompt lists them before an update).
#[tauri::command(async)]
pub async fn agent_update_live_sessions(
    app: AppHandle,
    cli: CliKind,
) -> Result<Vec<UpdateSession>, String> {
    blocking(app, move |app| {
        Ok(live_sessions(&backend(&app), cli)?
            .iter()
            .map(UpdateSession::from)
            .collect())
    })
    .await
}

/// Stop every live session running `cli` so its executable can be replaced. Emits
/// `agent-updates://stop` first (so mounted panes know why they ended), then kills each
/// session. Returns the stopped keys, which are remembered for the restart.
#[tauri::command(async)]
pub async fn agent_update_stop(app: AppHandle, cli: CliKind) -> Result<Vec<UpdateSession>, String> {
    blocking(app, move |app| {
        let backend = backend(&app);
        let keys = live_sessions(&backend, cli)?;
        app.state::<AgentUpdates>().stop(cli, &keys);
        for key in &keys {
            if let Err(e) = backend.kill_live(key, None) {
                log::warn!("[agent-updates] couldn't stop {key}: {e}");
            }
        }
        Ok(keys.iter().map(UpdateSession::from).collect())
    })
    .await
}

/// Ask the frontend to restart `cli`'s sessions (the ones stopped for the update plus
/// the ones still running) through `agent-updates://restart`. Never restarts anything
/// itself. Returns the keys.
#[tauri::command(async)]
pub async fn agent_update_restart(
    app: AppHandle,
    cli: CliKind,
) -> Result<Vec<UpdateSession>, String> {
    blocking(app, move |app| {
        let live = live_sessions(&backend(&app), cli)?;
        Ok(app.state::<AgentUpdates>().restart(cli, &live))
    })
    .await
}
