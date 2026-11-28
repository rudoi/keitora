//! `keitora status [<wt>]`: what the running app knows about this project, read from
//! `status.db` (the app's heartbeat, herdr link, presence, engine views and health
//! findings) and the project's `tasks.db` (hook rows, drives, turns). Read-only: it
//! never talks to herdr and never writes. Without a fresh app it falls back to the
//! hook rows (`source: "legacy"`).
//!
//! The same findings are attached (at most two, `warn`/`error` only) to `wait`
//! stalls and request timeouts, through [`health_notes`].

use std::collections::HashMap;
use std::path::Path;

use keitora_tasks::{
    age_text, clock_text, freshness, sqlite_utc_to_ms, AppProject, CliKind, DriveTurn, EngineRead,
    FindingRow, Freshness, LocalStore, ProjectStatus, StatusReader, StatusSession, WorktreeSession,
    WriterRow,
};
use serde_json::{json, Value};

use crate::drive::{ExitError, Output};

/// How many turns `status <wt>` shows.
pub const STATUS_TURNS: usize = 5;
/// How many findings a stall or timeout carries.
pub const HEALTH_NOTES_MAX: usize = 2;

/// What `status` reads, injected so tests use temp DBs and a fixed clock.
pub struct StatusInput<'a> {
    /// The project's `tasks.db`.
    pub local: &'a LocalStore,
    /// The project's `[project].name` (its key in `status.db`).
    pub project: String,
    /// The configured worktrees, in order.
    pub worktrees: Vec<String>,
    pub worktree_cli: &'a dyn Fn(&str) -> Option<CliKind>,
    /// The app's `status.db`; `None` reads as "no app".
    pub status_db: Option<&'a Path>,
    pub now_ms: i64,
}

// ---- helpers ----

/// RFC 3339 UTC, whole seconds.
fn rfc3339(ms: i64) -> Value {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| json!(t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)))
        .unwrap_or(Value::Null)
}

/// Whole seconds from `at_ms` to `now_ms` (never negative).
fn age_s(now_ms: i64, at_ms: i64) -> i64 {
    (now_ms - at_ms).max(0) / 1000
}

fn severity_rank(s: &str) -> u8 {
    match s {
        "error" => 2,
        "warn" => 1,
        _ => 0,
    }
}

/// Active findings, highest severity first, then newest.
fn sort_active(mut rows: Vec<FindingRow>) -> Vec<FindingRow> {
    rows.retain(|f| f.cleared_at.is_none());
    rows.sort_by(|a, b| {
        severity_rank(&b.severity)
            .cmp(&severity_rank(&a.severity))
            .then(b.last_seen.cmp(&a.last_seen))
    });
    rows
}

/// One finding in `--json`. The window's "copy as JSON" (`statusJson` in
/// src/lib/health.ts) prints the same keys; change both together (each side's tests
/// pin the shape).
fn finding_json(f: &FindingRow) -> Value {
    json!({
        "project": f.project,
        "worktree": f.worktree,
        "check_id": f.check_id,
        "severity": f.severity,
        "message": f.message,
        "first_seen": rfc3339(f.first_seen),
        "last_seen": rfc3339(f.last_seen),
        "occurrences": f.occurrences,
        "remedy": f.remedy,
    })
}

/// `status.db`'s reader and how fresh its writer row is, stale or not (`None`:
/// missing or unreadable).
fn open_status(path: Option<&Path>, now_ms: i64) -> Option<(StatusReader, Freshness)> {
    let reader = StatusReader::open(path?).ok().flatten()?;
    let fresh = freshness(reader.writer().ok().flatten(), now_ms);
    Some((reader, fresh))
}

/// [`open_status`] only when a live app writes it (`None`: stale, missing or
/// unreadable).
fn fresh_reader(path: Option<&Path>, now_ms: i64) -> Option<(StatusReader, Freshness)> {
    open_status(path, now_ms).filter(|(_, f)| !matches!(f, Freshness::Stale { .. }))
}

/// The active `warn`/`error` findings of `project`'s `worktree`, at most
/// [`HEALTH_NOTES_MAX`], highest severity then newest; `None` unless `status.db` is
/// fresh. What a `wait` stall and a request timeout carry.
pub fn health_notes(
    status_db: Option<&Path>,
    project: &str,
    worktree: &str,
    now_ms: i64,
) -> Option<Vec<FindingRow>> {
    let (reader, _) = fresh_reader(status_db, now_ms)?;
    let rows = reader.findings(project, Some(worktree)).ok()?;
    let mut rows = sort_active(rows);
    rows.retain(|f| severity_rank(&f.severity) >= 1);
    rows.truncate(HEALTH_NOTES_MAX);
    Some(rows)
}

/// Add `notes` to `out`: a `findings` array (JSON) and one `  health: {message}` line
/// each (human). `None` (no fresh app) adds nothing.
pub fn attach_health(out: &mut Output, notes: Option<Vec<FindingRow>>) {
    let Some(notes) = notes else {
        return;
    };
    if let Some(obj) = out.value.as_object_mut() {
        obj.insert(
            "findings".into(),
            notes
                .iter()
                .map(|f| json!({"check_id": f.check_id, "severity": f.severity, "message": f.message}))
                .collect(),
        );
    }
    for f in &notes {
        out.human.push_str(&format!("\n  health: {}", f.message));
    }
}

// ---- status ----

/// Everything `status` gathered, for its JSON and human forms.
struct Report {
    /// The live app's writer row; `None`: no live app (the legacy view).
    writer: Option<WriterRow>,
    /// The live app has a usable herdr snapshot.
    engine_ready: bool,
    /// The writer's last heartbeat, live or not.
    heartbeat_at: Option<i64>,
    app_project: Option<AppProject>,
    rows: Vec<Row>,
    findings: Vec<FindingRow>,
    /// `status <wt>`'s recent turns; `None` for the whole project.
    turns: Option<Vec<DriveTurn>>,
}

/// One worktree's line, as gathered.
struct Row {
    name: String,
    provider: Option<&'static str>,
    state: String,
    view: Option<StatusSession>,
    /// The engine can't vouch for `state` (herdr not ready, or a stale view).
    stale: bool,
    hook: Option<WorktreeSession>,
    driven_by: Option<String>,
    findings: Vec<String>,
}

/// One worktree's `(state, view, stale)` from what the store says of it and its hook
/// row. A fresh view is the answer (none: `offline`); while the engine can't vouch
/// for it (herdr not ready, a stale view) the hook row stands in, else the old view,
/// marked stale; with no live app, the hook row as written.
fn row_state(
    read: EngineRead,
    hook: Option<&WorktreeSession>,
) -> (String, Option<StatusSession>, bool) {
    match read {
        EngineRead::Fresh { view, .. } => {
            let state = view.as_ref().map_or("offline", |v| v.state.as_str());
            (state.to_string(), view, false)
        }
        EngineRead::NotReady { view, .. } => {
            let state = hook
                .map(|h| h.state.as_str())
                .or(view.as_ref().map(|v| v.state.as_str()))
                .unwrap_or("offline");
            (state.to_string(), view, true)
        }
        EngineRead::Stale { .. } => {
            let state = hook.map_or("offline", |h| h.state.as_str());
            (state.to_string(), None, false)
        }
    }
}

/// Run `keitora status`, for `filter` (one worktree) or the whole project.
pub fn status(input: &StatusInput, filter: Option<&str>) -> Result<Output, ExitError> {
    if let Some(wt) = filter {
        if !input.worktrees.is_empty() && !input.worktrees.iter().any(|w| w == wt) {
            return Err(ExitError::new(
                4,
                format!("no worktree named {wt} in project {}", input.project),
            ));
        }
    }
    let now = input.now_ms;
    let reader = input
        .status_db
        .and_then(|p| StatusReader::open(p).ok().flatten());
    let project_status = reader
        .as_ref()
        .map_or(ProjectStatus::Stale { heartbeat_at: None }, |r| {
            r.project_status(&input.project, now)
        });
    let (writer, engine_ready, heartbeat_at) = match &project_status {
        ProjectStatus::Fresh { writer, .. } => {
            (Some(writer.clone()), true, Some(writer.heartbeat_at))
        }
        ProjectStatus::NotReady { writer, .. } => {
            (Some(writer.clone()), false, Some(writer.heartbeat_at))
        }
        ProjectStatus::Stale { heartbeat_at } => (None, false, *heartbeat_at),
    };
    // No live app: nothing in the store is read.
    let reader = reader.filter(|_| writer.is_some());

    let hooks: HashMap<String, WorktreeSession> = input
        .local
        .sessions_list()?
        .into_iter()
        .map(|s| (s.worktree.clone(), s))
        .collect();
    let drivers: HashMap<String, String> = input
        .local
        .drives_list()?
        .into_iter()
        .map(|d| (d.driven, d.driver))
        .collect();
    let mut findings: Vec<FindingRow> = Vec::new();
    if let Some(r) = &reader {
        findings.extend(r.findings("", None).unwrap_or_default());
        findings.extend(r.findings(&input.project, None).unwrap_or_default());
    }
    let mut findings = sort_active(findings);
    if let Some(wt) = filter {
        findings.retain(|f| f.project == input.project && f.worktree == wt);
    }

    let names: Vec<String> = match filter {
        Some(wt) => vec![wt.to_string()],
        None => input.worktrees.clone(),
    };
    let rows: Vec<Row> = names
        .iter()
        .map(|name| {
            let hook = hooks.get(name).cloned();
            let (state, view, stale) = row_state(project_status.engine_read(name), hook.as_ref());
            Row {
                name: name.clone(),
                provider: (input.worktree_cli)(name).map(|c| c.spec().config_key),
                state,
                view,
                stale,
                hook,
                driven_by: drivers.get(name).cloned(),
                findings: findings
                    .iter()
                    .filter(|f| f.project == input.project && &f.worktree == name)
                    .map(|f| f.check_id.clone())
                    .collect(),
            }
        })
        .collect();

    let app_project = reader
        .as_ref()
        .and_then(|r| r.app_project(&input.project).ok().flatten());
    let turns = match filter {
        Some(wt) => Some(input.local.drive_turns_recent(wt, STATUS_TURNS)?),
        None => None,
    };

    let report = Report {
        writer,
        engine_ready,
        heartbeat_at,
        app_project,
        rows,
        findings,
        turns,
    };
    Ok(Output {
        value: status_json(input, &report),
        human: status_human(input, &report),
        code: 0,
    })
}

fn status_json(input: &StatusInput, r: &Report) -> Value {
    let now = input.now_ms;
    let app = match &r.writer {
        Some(w) => json!({
            "running": true,
            "pid": w.pid,
            "version": w.version,
            "started_at": rfc3339(w.started_at),
            "heartbeat_age_s": age_s(now, w.heartbeat_at),
            "herdr_link": w.herdr_link,
            "snapshot_age_s": w.last_snapshot_at.map(|t| age_s(now, t)),
            "engine_ready": r.engine_ready,
        }),
        None => json!({
            "running": false,
            "heartbeat_age_s": r.heartbeat_at.map(|t| age_s(now, t)),
        }),
    };
    let project = match &r.app_project {
        Some(p) => json!({
            "name": input.project,
            "open": p.open,
            "watcher": {
                "state": p.watcher_state,
                "last_event_age_s": p.watcher_last_event_at.map(|t| age_s(now, t)),
                "last_pass_age_s": p.last_pass_at.map(|t| age_s(now, t)),
                "restarts": p.restarts,
            },
            "pending_requests": p.pending_count,
            "oldest_pending_age_s": p.oldest_pending_at.map(|t| age_s(now, t)),
        }),
        None => json!({
            "name": input.project,
            "open": false,
            "watcher": null,
            "pending_requests": null,
            "oldest_pending_age_s": null,
        }),
    };
    let worktrees: Vec<Value> = r
        .rows
        .iter()
        .map(|row| {
            let v = row.view.as_ref();
            json!({
                "name": row.name,
                "provider": row.provider,
                "state": row.state,
                "agent": v.map(|v| v.agent.clone()),
                "herdr_status": v.and_then(|v| v.herdr_status.clone()),
                "settled": v.map(|v| v.settled),
                "stale": row.stale || v.is_some_and(|v| v.stale),
                "pane_id": v.and_then(|v| v.pane_id.clone()),
                "agent_session": v.and_then(|v| v.agent_session.clone()),
                "driven_by": row.driven_by,
                "last_change_age_s": v.map(|v| age_s(now, v.last_change_at)),
                "hook": row.hook.as_ref().map(|h| json!({
                    "state": h.state,
                    "last_write_age_s": sqlite_utc_to_ms(&h.last_heartbeat).map(|t| age_s(now, t)),
                })),
                "findings": row.findings,
            })
        })
        .collect();
    let mut value = json!({
        "source": if r.writer.is_some() { "status_db" } else { "legacy" },
        "app": app,
        "project": project,
        "worktrees": worktrees,
        "findings": r.findings.iter().map(finding_json).collect::<Vec<_>>(),
    });
    if let Some(turns) = &r.turns {
        value["detail"] = json!({
            "turns": turns.iter().map(|t| json!({
                "kind": t.kind,
                "signal": t.signal,
                "reply_source": t.reply_source,
                "completed_at": sqlite_utc_to_ms(&t.completed_at).map_or(Value::Null, rfc3339),
            })).collect::<Vec<_>>(),
        });
    }
    value
}

/// The `herdr: ...` part of a worktree line; `stale` when the engine can't vouch
/// for it.
fn herdr_text(v: Option<&StatusSession>, stale: bool) -> String {
    match v {
        Some(v) if v.agent != "absent" => {
            let status = v.herdr_status.as_deref().unwrap_or("unknown");
            let mut s = format!("herdr: {status}");
            if v.agent == "starting" {
                s.push_str(" (starting)");
            } else if v.settled {
                s.push_str(" (settled)");
            }
            if stale || v.stale {
                s.push_str(" [stale]");
            }
            s
        }
        None if stale => "herdr: not ready [stale]".to_string(),
        _ => "herdr: no agent".to_string(),
    }
}

fn status_human(input: &StatusInput, report: &Report) -> String {
    let now = input.now_ms;
    let heartbeat_at = report.heartbeat_at;
    let Report {
        writer,
        app_project,
        rows,
        findings,
        turns,
        ..
    } = report;
    let mut out = Vec::new();
    out.push(match turns {
        Some(_) => format!(
            "keitora status @{} (project {})",
            rows.first().map_or("", |r| r.name.as_str()),
            input.project
        ),
        None => format!("keitora status (project {})", input.project),
    });
    match writer {
        Some(w) => {
            let snapshot = w
                .last_snapshot_at
                .map_or("no snapshot yet".to_string(), |t| {
                    format!("snapshot {} ago", age_text(age_s(now, t)))
                });
            out.push(format!(
                "  app       running (pid {}, v{}), herdr {}, {snapshot}",
                w.pid, w.version, w.herdr_link
            ));
            out.push(match app_project {
                Some(p) => {
                    let pending = match p.pending_count {
                        1 => "1 pending request".to_string(),
                        n => format!("{n} pending requests"),
                    };
                    format!(
                        "  project   {}; watcher {}; {pending}",
                        if p.open {
                            "open in window"
                        } else {
                            "not open in the window"
                        },
                        p.watcher_state
                    )
                }
                None => "  project   not reported by the app yet".to_string(),
            });
        }
        None => {
            let why = match heartbeat_at {
                Some(t) => format!("last update {} ago", age_text(age_s(now, t))),
                None => "no status reported on this machine".to_string(),
            };
            out.push(format!(
                "  app       not running ({why}); showing hook rows only"
            ));
        }
    }
    out.push(if turns.is_some() {
        "  worktree".to_string()
    } else {
        "  worktrees".to_string()
    });
    let name_w = rows.iter().map(|r| r.name.len() + 1).max().unwrap_or(0);
    let state_w = rows.iter().map(|r| r.state.len()).max().unwrap_or(0);
    let prov_w = rows
        .iter()
        .map(|r| r.provider.map_or(1, str::len))
        .max()
        .unwrap_or(0);
    for r in rows {
        let mut line = format!(
            "    {:<name_w$}  {:<state_w$}  {:<prov_w$}",
            format!("@{}", r.name),
            r.state,
            r.provider.unwrap_or("?"),
        );
        if writer.is_some() {
            line.push_str(&format!("  {}", herdr_text(r.view.as_ref(), r.stale)));
        }
        if let Some(d) = &r.driven_by {
            line.push_str(&format!("   driven by @{d}"));
        }
        if let Some(h) = &r.hook {
            if writer.is_none() || h.state != r.state {
                let at = sqlite_utc_to_ms(&h.last_heartbeat)
                    .map(|t| format!(", last write {} ago", age_text(age_s(now, t))))
                    .unwrap_or_default();
                line.push_str(&format!("   hooks: {}{at}", h.state));
            }
        }
        out.push(line.trim_end().to_string());
    }
    if writer.is_some() {
        out.push(format!("  findings ({})", findings.len()));
        for f in findings {
            if turns.is_some() {
                out.push(format!(
                    "    {:<5} {}  {}",
                    f.severity, f.check_id, f.message
                ));
                let mut facts = format!(
                    "          first seen {}, {}x",
                    clock_text(f.first_seen),
                    f.occurrences
                );
                if !f.remedy.is_empty() {
                    facts.push_str(&format!(", fixed by: {}", f.remedy));
                    if let Some(at) = f.remedy_at {
                        facts.push_str(&format!(" at {}", clock_text(at)));
                    }
                }
                out.push(facts);
            } else {
                let scope = match (f.project.as_str(), f.worktree.as_str()) {
                    ("", _) => "app".to_string(),
                    (p, "") => p.to_string(),
                    (_, w) => format!("@{w}"),
                };
                out.push(format!(
                    "    {:<5} {scope}  {}  {}",
                    f.severity, f.check_id, f.message
                ));
            }
        }
    }
    if let Some(turns) = turns {
        out.push(format!("  turns (last {})", turns.len()));
        for t in turns {
            let at = sqlite_utc_to_ms(&t.completed_at)
                .map_or_else(|| t.completed_at.clone(), clock_text);
            out.push(format!(
                "    {at}  {:<6} {:<5} reply: {}",
                t.kind, t.signal, t.reply_source
            ));
        }
    }
    out.join("\n")
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod tests;
