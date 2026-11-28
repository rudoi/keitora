//! The window's view of the health monitor: `health_list` and the
//! `health://changed` event. Both read the monitor's in-memory findings, never the
//! `status.db` table (that is the CLI's).

use keitora_tasks::FindingRow;
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Manager, Runtime};

use super::driver::HealthList;

/// Emitted with the full list whenever a finding is inserted, changed, cleared or
/// dropped.
pub const HEALTH_CHANGED: &str = "health://changed";

/// The monitor's list, managed as app state.
pub struct HealthState(pub HealthList);

/// One finding as the window reads it.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HealthFinding {
    pub project: String,
    pub worktree: String,
    pub check_id: String,
    pub severity: String,
    pub message: String,
    pub detail: Value,
    pub first_seen: i64,
    pub last_seen: i64,
    pub occurrences: i64,
    /// Set for a finding that cleared in the last day.
    pub cleared_at: Option<i64>,
    pub remedy: String,
    pub remedy_at: Option<i64>,
}

impl HealthFinding {
    /// `r` with its already-parsed `detail`.
    pub fn from_row(r: &FindingRow, detail: Value) -> HealthFinding {
        HealthFinding {
            project: r.project.clone(),
            worktree: r.worktree.clone(),
            check_id: r.check_id.clone(),
            severity: r.severity.clone(),
            message: r.message.clone(),
            detail,
            first_seen: r.first_seen,
            last_seen: r.last_seen,
            occurrences: r.occurrences,
            cleared_at: r.cleared_at,
            remedy: r.remedy.clone(),
            remedy_at: r.remedy_at,
        }
    }
}

/// Active and recently cleared findings (empty before the monitor starts).
#[tauri::command]
pub fn health_list<R: Runtime>(app: AppHandle<R>) -> Vec<HealthFinding> {
    app.try_state::<HealthState>()
        .map(|s| s.0.lock().unwrap().clone())
        .unwrap_or_default()
}
