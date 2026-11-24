//! Tauri commands for dependency setup. Everything that probes a binary, reads config
//! or talks to the backend runs on a blocking thread.

use std::path::PathBuf;

use tauri::State;

use super::doctor::DoctorRow;
use super::{DepsService, DepsSnapshot, Tier};

/// Run `f` with a handle to the service on a blocking thread.
async fn blocking<T: Send + 'static>(
    deps: &State<'_, DepsService>,
    f: impl FnOnce(DepsService) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let deps = deps.inner().clone();
    tauri::async_runtime::spawn_blocking(move || f(deps))
        .await
        .map_err(|e| format!("dependency task failed: {e}"))?
}

/// The current snapshot; seeds the UI on mount and after a webview reload.
#[tauri::command(async)]
pub async fn deps_status(deps: State<'_, DepsService>) -> Result<DepsSnapshot, String> {
    blocking(&deps, |d| Ok(d.snapshot())).await
}

/// Re-read the pinned herdr and its version; also emits `deps://status`.
#[tauri::command(async)]
pub async fn deps_recheck(deps: State<'_, DepsService>) -> Result<DepsSnapshot, String> {
    blocking(&deps, |d| Ok(d.recheck())).await
}

#[tauri::command(async)]
pub async fn deps_doctor(deps: State<'_, DepsService>) -> Result<Vec<DoctorRow>, String> {
    blocking(&deps, |d| Ok(d.doctor())).await
}

/// Start (or attach to) an install/upgrade; progress arrives as events.
#[tauri::command(async)]
pub async fn deps_install(
    deps: State<'_, DepsService>,
    tier: Option<Tier>,
    confirm_overwrite: Option<bool>,
) -> Result<(), String> {
    let confirm = confirm_overwrite.unwrap_or(false);
    blocking(&deps, move |d| d.start_install(tier, confirm)).await
}

#[tauri::command(async)]
pub async fn deps_snooze_upgrade(deps: State<'_, DepsService>) -> Result<(), String> {
    blocking(&deps, |d| {
        d.snooze_prompt();
        Ok(())
    })
    .await
}

/// Pin the herdr at `path` (validated first), or clear the pin with `None`.
#[tauri::command(async)]
pub async fn deps_set_herdr_bin(
    deps: State<'_, DepsService>,
    path: Option<String>,
) -> Result<DepsSnapshot, String> {
    blocking(&deps, move |d| d.set_herdr_bin(path.map(PathBuf::from))).await
}

/// The plain-text "Copy details" report.
#[tauri::command(async)]
pub async fn deps_details(deps: State<'_, DepsService>) -> Result<String, String> {
    blocking(&deps, |d| Ok(d.details_report())).await
}

/// "Restart to finish": restart herdr onto the pinned binary and refresh the agent
/// integration; also emits `deps://status`.
#[tauri::command(async)]
pub async fn deps_restart_to_finish(deps: State<'_, DepsService>) -> Result<DepsSnapshot, String> {
    blocking(&deps, |d| d.restart_to_finish()).await
}
