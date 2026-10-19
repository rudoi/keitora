//! Desktop host: the authoritative Keitora app surface — PTY sessions, worktree /
//! project commands, the task/config watchers, and the desktop `run()` entry.
//!
//! `lib.rs` declares this module unconditionally; desktop is the only target.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use base64::Engine as _;

use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::backend::herdr::recovery::RecoveryHandle;
use crate::backend::unavailable::UnavailableBackend;
use crate::backend::{
    session_hint, AgentLaunch, AttachResult, DaemonStats, OutputSink, PtyBackend, PtyBackendError,
    ScrollDirection, ScrollSource, SessionHint, SessionKey, SharedBackend, StatusPayload,
};
use crate::{bg_notifier, codex_hooks, dispatch, hooks, tasks};

/// A configured worktree as the rail sees it: the resolved worktree plus whether
/// the user terminated its session. Tauri-layer only.
#[derive(Serialize)]
struct WorktreeView {
    #[serde(flatten)]
    inner: keitora_tasks::ResolvedWorktree,
    /// The session stays down (no auto-spawn, no resume) until launched.
    terminated: bool,
}

/// List the configured worktrees, each resolved to a path and classified.
/// `ResolvedWorktree` serializes `path` as a string, which is what the UI wants.
///
/// Tolerates the "no active project yet" state: registry-first resolution that
/// fails (e.g. `NeedsOnboarding`) yields an empty list rather than an error, so
/// the rail renders empty until onboarding completes.
#[tauri::command(async)]
fn worktrees(client: State<SharedBackend>) -> Result<Vec<WorktreeView>, String> {
    worktrees_in(
        client.inner().as_ref(),
        keitora_tasks::resolve_active().ok(),
    )
}

/// [`worktrees`] for the active project `active` (`None`: no active project yet,
/// an empty rail).
fn worktrees_in(
    client: &dyn PtyBackend,
    active: Option<keitora_tasks::ProjectConfig>,
) -> Result<Vec<WorktreeView>, String> {
    let Some(pc) = active else {
        return Ok(Vec::new());
    };
    let project = pc.project.name.clone();
    let resolved = keitora_tasks::Config::from_project(pc)
        .resolved_worktrees()
        .map_err(|e| e.to_string())?;
    Ok(worktree_views(client, &project, resolved))
}

/// Resolved worktrees for `project` with the backend's terminated flag attached.
fn worktree_views(
    client: &dyn PtyBackend,
    project: &str,
    resolved: Vec<keitora_tasks::ResolvedWorktree>,
) -> Vec<WorktreeView> {
    let terminated = client.terminated_keys(project);
    resolved
        .into_iter()
        .map(|inner| WorktreeView {
            terminated: terminated.contains(&inner.name),
            inner,
        })
        .collect()
}

/// The data shape returned to the frontend for a registered project. Tauri-layer
/// only — deliberately not defined in `keitora-tasks`. `root`/`db_path` are plain
/// strings so `serde_json` serializes them without platform path quirks.
#[derive(serde::Serialize)]
pub struct ProjectSummary {
    /// The registry key (name) for this project.
    pub name: String,
    /// The resolved `[project].name` — the namespace sessions, activity and the
    /// daemon key on. Usually equal to `name`, but not guaranteed (a file-less
    /// project resolves to its root basename; a committed file can set any name).
    pub resolved_name: String,
    /// Absolute path to the project root (the directory containing `keitora.toml`).
    pub root: String,
    /// Whether this is the currently active project.
    pub active: bool,
    /// Absolute path to the SQLite task database.
    pub db_path: String,
    /// Worktree names (as listed in `ProjectConfig.tasks.worktrees`).
    pub worktrees: Vec<String>,
    /// Backend kind string from `[tasks] backend` (e.g. `"sqlite"`, `"github"`).
    pub backend: String,
}

/// Build a `ProjectSummary` from a resolved `ProjectConfig`. The registry name is
/// `pc.project.name`; `active` is supplied by the caller (who knows the registry
/// state). Kept as a free function so it is unit-testable.
fn project_summary(pc: &keitora_tasks::ProjectConfig, active: bool) -> ProjectSummary {
    ProjectSummary {
        name: pc.project.name.clone(),
        resolved_name: pc.project.name.clone(),
        root: pc.root.display().to_string(),
        active,
        db_path: pc.tasks.db_path.display().to_string(),
        worktrees: pc.worktrees(),
        backend: pc.tasks.backend.clone(),
    }
}

/// The active project, or `None` when none is configured. `None` (not `Err`)
/// signals the frontend to enter the onboarding flow.
#[tauri::command(async)]
fn current_project() -> Result<Option<ProjectSummary>, String> {
    match keitora_tasks::resolve_active() {
        Ok(pc) => Ok(Some(project_summary(&pc, true))),
        Err(_) => Ok(None),
    }
}

/// Every registered project in display order, with the active one flagged.
/// Projects whose `keitora.toml` fails to load are skipped (a stale registry entry
/// should not break the whole list).
#[tauri::command(async)]
fn list_projects() -> Result<Vec<ProjectSummary>, String> {
    let user_cfg = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
    Ok(project_summaries(&user_cfg))
}

/// [`list_projects`] over an already-loaded registry.
fn project_summaries(user_cfg: &keitora_tasks::UserConfig) -> Vec<ProjectSummary> {
    let active_name = user_cfg.active.as_deref().unwrap_or("");
    let mut result = Vec::new();
    for name in user_cfg.ordered_project_names() {
        // effective_config makes a `keitora.toml` optional: a file-less registry entry
        // resolves via scaffold defaults + overrides instead of being skipped. A
        // genuinely broken committed file is still skipped, not fatal.
        let pc = match keitora_tasks::effective_config(&user_cfg.projects[&name]) {
            Ok(pc) => pc,
            Err(_) => continue,
        };
        let mut summary = project_summary(&pc, name == active_name);
        // Prefer the registry key as the display name (it is what set_active uses);
        // `resolved_name` keeps the session namespace.
        summary.name = name;
        result.push(summary);
    }
    result
}

/// One registered project's rail state: its worktrees and reconciled session
/// states, in the same wire shapes as the `worktrees` and `sessions_list` commands.
#[derive(serde::Serialize)]
pub struct ProjectOverview {
    /// Registry key (what `set_active_project` takes).
    pub key: String,
    /// Resolved `[project].name` — the namespace sessions and activity key on.
    pub resolved_name: String,
    worktrees: Vec<WorktreeView>,
    /// Session rows with `project` stamped to `resolved_name`.
    pub sessions: Vec<keitora_tasks::WorktreeSession>,
    /// Each worktree's active task, so every project's lanes show their task line.
    pub active_tasks: Vec<keitora_tasks::ActiveTask>,
}

/// Worktrees and session states for every registered project in display order,
/// so lanes of non-active projects can show real status. Reads only the backend's
/// caches (no server query). A project whose config or worktrees fail to resolve
/// is skipped.
#[tauri::command(async)]
fn projects_overview(client: State<SharedBackend>) -> Result<Vec<ProjectOverview>, String> {
    let user_cfg = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
    let backend = client.inner().as_ref();
    let live = backend.live_set_snapshot();
    let bg = crate::backend::cached_background_set(backend);
    Ok(project_overviews(backend, &user_cfg, &live, &bg))
}

/// [`projects_overview`] over an already-loaded registry and live/background sets.
fn project_overviews(
    client: &dyn PtyBackend,
    user_cfg: &keitora_tasks::UserConfig,
    live: &HashSet<SessionKey>,
    bg: &HashSet<SessionKey>,
) -> Vec<ProjectOverview> {
    user_cfg
        .ordered_project_names()
        .into_iter()
        .filter_map(|key| {
            let pc = keitora_tasks::effective_config(&user_cfg.projects[&key]).ok()?;
            project_overview(client, user_cfg, &key, pc, live, bg).ok()
        })
        .collect()
}

/// One project's [`ProjectOverview`]. Session state is read through a read-only
/// handle on the project's task DB: it never creates or initialises the file
/// (listing a project must not leave files behind) and waits only briefly on a
/// lock. A missing, locked or unreadable DB yields the worktrees with no sessions
/// or tasks rather than stalling or failing the overview.
fn project_overview(
    client: &dyn PtyBackend,
    user_cfg: &keitora_tasks::UserConfig,
    key: &str,
    pc: keitora_tasks::ProjectConfig,
    live: &HashSet<SessionKey>,
    bg: &HashSet<SessionKey>,
) -> Result<ProjectOverview, String> {
    let resolved = pc.project.name.clone();
    let local = keitora_tasks::LocalStore::open_read_only(&pc.tasks.db_path.to_string_lossy());
    let mut sessions = local
        .as_ref()
        .map_err(|e| e.to_string())
        .and_then(|local| crate::tasks::reconciled_sessions_in(local, live, bg, &resolved, &pc))
        .unwrap_or_default();
    let active_tasks = local
        .as_ref()
        .ok()
        .and_then(|local| local.active_tasks().ok())
        .unwrap_or_default();
    for s in &mut sessions {
        s.project = resolved.clone();
    }
    // The registry is already loaded; resolve against it rather than re-reading it per
    // project.
    let worktrees = keitora_tasks::Config::from_project(pc)
        .resolved_worktrees_with(user_cfg)
        .map_err(|e| e.to_string())?;
    let worktrees = worktree_views(client, &resolved, worktrees);
    Ok(ProjectOverview {
        key: key.to_string(),
        resolved_name: resolved,
        worktrees,
        sessions,
        active_tasks,
    })
}

/// Persist a new project display order and tell the frontend to reload the
/// project list (`projects://reordered`). `order` may be partial (see `UserConfig::set_project_order`).
#[tauri::command(async)]
fn reorder_projects(app: AppHandle, order: Vec<String>) -> Result<(), String> {
    {
        let _registry = keitora_tasks::UserConfig::write_lock();
        let mut user_cfg = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
        user_cfg.set_project_order(order);
        user_cfg.save().map_err(|e| e.to_string())?;
    }
    // Only the order changed: a dedicated event so project-scoped consumers of
    // `project://changed` (project reload, diff refresh) are not re-run.
    let _ = app.emit("projects://reordered", ());
    Ok(())
}

/// Persist a new worktree order for the project registered as `project` and tell the
/// frontend to reload its worktree lists. `order` may be partial (see
/// `keitora_tasks::reorder_entries`).
#[tauri::command(async)]
fn reorder_worktrees(app: AppHandle, project: String, order: Vec<String>) -> Result<(), String> {
    keitora_tasks::reorder_project_worktrees(&project, &order).map_err(|e| e.to_string())?;
    let _ = app.emit("worktrees://changed", ());
    Ok(())
}

/// Switch the active project by registry name. Emits the project/worktrees/tasks/
/// sessions change events directly so the frontend reseeds immediately, instead of
/// waiting on the config-file watcher's ~200 ms debounce (the source of the visible
/// switch lag). The config watcher still fires shortly after — the duplicate reload
/// is idempotent.
#[tauri::command(async)]
fn set_active_project(app: AppHandle, name: String) -> Result<(), String> {
    {
        let _registry = keitora_tasks::UserConfig::write_lock();
        let mut user_cfg = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
        if !user_cfg.projects.contains_key(&name) {
            return Err(format!("No project named '{name}' is registered."));
        }
        user_cfg.set_active(&name);
        user_cfg.save().map_err(|e| e.to_string())?;
    }
    // The active project changed: drop the cached `(name, db_path)` so the next
    // PTY command re-resolves against the new project, (re)install its worktrees'
    // session hooks off-thread, and kick off the repair pass off-thread. Pass the
    // resolved root so `on_project_activated` needn't re-read the registry.
    let root = keitora_tasks::resolve_active()
        .map_err(|e| e.to_string())?
        .root;
    on_project_activated(app.clone(), root);
    // Re-point the tasks/sessions + config watchers at the new project. This runs
    // AFTER invalidate + save so `resolve_active()` (read inside the calls) returns
    // the new project. Stops the old watchers (no stale events from the old project)
    // and removes the 30 s poll lag for the new one.
    restart_project_watchers(&app);
    // Reseed the whole project-scoped UI now — don't wait for the fs watcher.
    let _ = app.emit("project://changed", ());
    let _ = app.emit("worktrees://changed", ());
    let _ = app.emit("tasks://changed", ());
    let _ = app.emit("sessions://changed", ());
    Ok(())
}

/// Onboard a folder as a Keitora project: validate it is a git repo, scaffold a
/// `keitora.toml` if absent (migrating a legacy `task-config.json` in place if one
/// is present), then register + activate it.
#[tauri::command(async)]
fn onboard_project(app: AppHandle, path: String) -> Result<ProjectSummary, String> {
    use std::path::Path;

    let root = Path::new(&path);

    // 1. Validate folder exists and is a git repository.
    if !root.is_dir() {
        return Err(format!("Path is not a directory: {path}"));
    }
    if !root.join(".git").exists() {
        return Err(format!("Path is not a git repository (no .git): {path}"));
    }

    // 2. Determine the project name. A `keitora.toml` is NOT required: a
    //    project is stored in the user registry and resolves via scaffold defaults
    //    when no committed file exists. Three cases, in order:
    //      a. committed `keitora.toml` present → use its name (committed tier wins).
    //      b. in-tree legacy `task-config.json` → migrate it (writes a `keitora.toml`,
    //         preserving worktrees/db pointer) then use the resulting name.
    //      c. neither → register file-less; the name is the root basename.
    // The folder basename — the file-less project name, and the fallback when a
    // committed file isn't where we expect.
    let basename = || {
        root.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| format!("cannot derive a project name from path: {path}"))
    };
    let toml_path = root.join("keitora.toml");
    let project_name = if toml_path.exists() {
        keitora_tasks::ProjectConfig::load(root)
            .map_err(|e| e.to_string())?
            .project
            .name
    } else if let Some(legacy) = find_in_tree_legacy(root) {
        keitora_tasks::migrate_legacy(&legacy).map_err(|e| e.to_string())?;
        // migrate_legacy may derive a project root other than `root` for unusual
        // layouts; if the keitora.toml didn't land here, fall back to a file-less
        // registration named for the picked folder rather than erroring out.
        if root.join("keitora.toml").exists() {
            keitora_tasks::ProjectConfig::load(root)
                .map_err(|e| e.to_string())?
                .project
                .name
        } else {
            basename()?
        }
    } else {
        basename()?
    };

    // 3. Register + activate. (Migration already registers; re-registering with the
    //    same name/path is idempotent.)
    let user_cfg = {
        let _registry = keitora_tasks::UserConfig::write_lock();
        let mut user_cfg = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
        user_cfg.register(&project_name, root.to_path_buf());
        user_cfg.set_active(&project_name);
        user_cfg.save().map_err(|e| e.to_string())?;
        user_cfg
    };
    // Onboarding activates the project — drop the stale cached active project,
    // install session hooks off-thread, and kick off the repair pass off-thread
    // (emits `worktrees://changed` + `project://changed` if it backfills anything,
    // so the rail reflects correct paths without a restart).
    on_project_activated(app.clone(), root.to_path_buf());

    // 4. Build the returned summary from the effective config so it reflects the
    //    just-registered entry (scaffold defaults for a file-less project, the
    //    committed file otherwise, plus any per-machine overrides + worktrees).
    let pref = user_cfg
        .projects
        .get(&project_name)
        .ok_or_else(|| "project missing from registry after register".to_string())?;
    let pc = keitora_tasks::effective_config(pref).map_err(|e| e.to_string())?;
    // Onboarding activates the project — re-point the tasks + config watchers at it
    // so live updates work immediately (not just after a later explicit switch).
    restart_project_watchers(&app);
    // Pick up the newly-registered project without an app restart: spawn its bg
    // notifier watcher. Additive — existing project watchers are unaffected.
    bg_notifier::start_bg_notifier(app);
    Ok(project_summary(&pc, true))
}

/// The first in-tree legacy `task-config.json` at a conventional location
/// (`<root>/keitora-worktrees/shared/` or `<root>/shared/`), if any. Used to offer
/// migration on folder-pick onboarding — migration is the one onboarding
/// path that still writes a `keitora.toml`.
fn find_in_tree_legacy(root: &std::path::Path) -> Option<std::path::PathBuf> {
    [
        root.join("keitora-worktrees")
            .join("shared")
            .join("task-config.json"),
        root.join("shared").join("task-config.json"),
    ]
    .into_iter()
    .find(|p| p.exists())
}

/// Detect (without migrating) a legacy `task-config.json`, returning its path so
/// onboarding can *offer* import rather than silently writing a `keitora.toml`.
#[tauri::command(async)]
fn detect_legacy() -> Option<String> {
    keitora_tasks::find_legacy_config()
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Migrate a legacy `task-config.json` (located via `find_legacy_config`) into a
/// committed `keitora.toml`, registering + activating the project. Returns `None`
/// (not `Err`) when no legacy config is found.
#[tauri::command(async)]
fn migrate_legacy(app: AppHandle) -> Result<Option<ProjectSummary>, String> {
    let legacy = match keitora_tasks::find_legacy_config() {
        Some(p) if p.exists() => p,
        _ => return Ok(None),
    };
    // migrate_legacy registers + activates the project as part of migration.
    let (pc, _wrote) = keitora_tasks::migrate_legacy(&legacy).map_err(|e| e.to_string())?;
    // Migration registers + activates the project — drop the stale cached active
    // project, install session hooks off-thread, and kick off the repair pass
    // off-thread (emits `worktrees://changed` + `project://changed` if it backfills
    // anything, so the rail reflects correct paths without a restart).
    on_project_activated(app.clone(), pc.root.clone());
    // Re-point the tasks + config watchers at the newly-active migrated project.
    restart_project_watchers(&app);
    Ok(Some(project_summary(&pc, true)))
}

/// Discover the git worktrees under `project_root` (via `git worktree list`) for
/// the onboarding multiselect. `already_tracked` reflects the project's current
/// `[tasks].worktrees`, so the UI can pre-check sensibly.
#[tauri::command(async)]
fn discover_worktrees(
    project_root: String,
) -> Result<Vec<keitora_tasks::WorktreeCandidate>, String> {
    let root = std::path::PathBuf::from(&project_root);
    // "Already tracked" reflects the effective worktree list. Resolve via the
    // registry entry's effective_config so a FILE-LESS project (no keitora.toml) still
    // reports its registry worktrees instead of an empty list; fall back to
    // a direct load for an unregistered project that has a committed file.
    let reg = keitora_tasks::UserConfig::load().unwrap_or_default();
    let tracked = reg
        .project_ref_by_root(&root)
        .and_then(|pref| keitora_tasks::effective_config(pref).ok())
        .or_else(|| keitora_tasks::ProjectConfig::load(&root).ok())
        .map(|pc| pc.worktrees())
        .unwrap_or_default();
    keitora_tasks::discover_worktrees(&root, &tracked).map_err(|e| e.to_string())
}

/// Write the chosen worktree entries (name + path) into the active project's
/// config (`[tasks].worktrees`), then notify the UI so the rail/board refresh.
#[tauri::command(async)]
fn set_worktrees(
    app: AppHandle,
    client: State<SharedBackend>,
    entries: Vec<keitora_tasks::WorktreeEntry>,
) -> Result<(), String> {
    let pc = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let before: Vec<String> = pc.worktree_entries().into_iter().map(|e| e.name).collect();
    keitora_tasks::set_project_worktrees(&pc.root, &entries).map_err(|e| e.to_string())?;
    // A worktree dropped from the list is not resumed by recovery. This is the
    // only place a worktree leaves a project today; any future removal command
    // must cancel the same way.
    cancel_removed_worktrees(
        client.recovery().as_ref(),
        &pc.project.name,
        &before,
        &entries,
    );
    // Newly re-discovered / adopted worktrees have no session hooks yet — install
    // them for the active project so their status lights populate. Off-thread.
    thread::spawn(hooks::install_active_session_hooks);
    let _ = app.emit("project://changed", ());
    let _ = app.emit("worktrees://changed", ());
    Ok(())
}

/// Tell recovery to forget every worktree of `project` in `before` that is not in
/// `after`.
fn cancel_removed_worktrees(
    recovery: Option<&RecoveryHandle>,
    project: &str,
    before: &[String],
    after: &[keitora_tasks::WorktreeEntry],
) {
    let Some(recovery) = recovery else {
        return;
    };
    for name in before {
        if !after.iter().any(|e| &e.name == name) {
            recovery.cancel(&SessionKey {
                project: project.to_string(),
                worktree: name.clone(),
            });
        }
    }
}

/// Create a new git worktree named `name` under the active project, install
/// session hooks into it, then notify the UI so the rail/board refresh.
#[tauri::command(async)]
fn worktree_create(
    app: AppHandle,
    name: String,
) -> Result<keitora_tasks::ResolvedWorktree, String> {
    let pc = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let wt = keitora_tasks::create_worktree(&pc, &name).map_err(|e| e.to_string())?;
    // Resolve the `keitora` binary for the new worktree: PATH → [cli].path override →
    // its own build (none yet on a fresh worktree, and only a last resort).
    let user = keitora_tasks::UserConfig::load().unwrap_or_default();
    let override_path = keitora_tasks::resolve_keitora_path(Some(&pc), &user);
    let path_keitora = keitora_tasks::keitora_on_path();
    if let Some(bin) =
        hooks::resolve_keitora_bin(&wt.path, override_path.as_deref(), path_keitora.as_deref())
    {
        hooks::install_hooks_for(&wt.path, &bin);
        // Codex worktrees also get a worktree-local `.codex/hooks.json`,
        // gated on `CliKind::Codex` so a hand-run `codex` in a Claude worktree is
        // never prompted to trust our hooks.
        if wt.cli == keitora_tasks::CliKind::Codex {
            codex_hooks::install_codex_hooks_for(&wt.path, &bin);
        }
    }
    let _ = app.emit("project://changed", ());
    let _ = app.emit("worktrees://changed", ());
    Ok(wt)
}

// ---------------------------------------------------------------------------
// Settings — read a full snapshot of the active project + user config, and
// write the editable fields back surgically (via `keitora_tasks::config_edit`).
// Tauri-layer DTOs only; fields serialize snake_case to match `ProjectSummary`.
// ---------------------------------------------------------------------------

/// The editable + display fields of the active project's `keitora.toml`.
/// `db_path` is the resolved absolute path (display-only); `worktrees_dir` is the
/// raw configured value (`None` → the `~/.keitora/<name>` default); `worktrees` is
/// read-only here (managed from the rail).
#[derive(serde::Serialize)]
pub struct SettingsProject {
    pub name: String,
    pub root: String,
    pub db_path: String,
    pub worktrees_dir: Option<String>,
    pub archive_days: i64,
    pub log_retention_days: i64,
    pub claude_path: Option<String>,
    /// Project-level `[claude] skip_permissions`. `true` means this
    /// project forces the flag on regardless of the user toggle; the drawer
    /// surfaces a note so the effective state isn't a surprise.
    pub skip_permissions: bool,
    pub dispatch_steps: Vec<String>,
    /// Project-level `[codex].path` override. Mirrors `claude_path`.
    pub codex_path: Option<String>,
    /// Project-level `[codex].autonomy`. Mirrors `skip_permissions`.
    pub codex_autonomy: bool,
    /// Project-level `[dispatch].codex_steps` override. Mirrors `dispatch_steps`.
    pub codex_dispatch_steps: Vec<String>,
    /// Project default diff-view base (`[project].diff_base`). `None` ⇒ Auto. Per-worktree
    /// overrides win over this and are set from the diff header, not here.
    pub diff_base: Option<keitora_tasks::DiffBase>,
    pub worktrees: Vec<String>,
    /// Whether a committed `<root>/keitora.toml` exists. When `false`, settings
    /// live only in the user registry; the drawer offers "Commit to project file" and
    /// disables the project-level Claude override (which requires a committed file).
    pub has_committed_file: bool,
}

/// One registered project in the user registry, for the user-section list.
#[derive(serde::Serialize)]
pub struct SettingsUserProject {
    pub name: String,
    pub root: String,
    pub active: bool,
}

/// The editable fields of the user `keitora.toml` plus the project registry.
#[derive(serde::Serialize)]
pub struct SettingsUser {
    pub claude_path: Option<String>,
    pub active: Option<String>,
    pub projects: Vec<SettingsUserProject>,
    /// Whether interactive Claude sessions launch with
    /// `--dangerously-skip-permissions` (persisted in `[claude] skip_permissions`
    /// of the user keitora.toml). Seeds the toggle in SettingsDrawer on open.
    pub skip_permissions: bool,
    /// User-level `[codex].path` default. Mirrors `claude_path`.
    pub codex_path: Option<String>,
    /// User-level `[codex].autonomy` default. Mirrors `skip_permissions`.
    pub codex_autonomy: bool,
    /// User-level default CLI: `UserConfig.default_cli`, falling back to
    /// `CliKind::Claude` when unset. Serializes `"claude"` / `"codex"`.
    pub default_cli: keitora_tasks::CliKind,
    /// Whether a `codex` binary was found on this machine
    /// (`discover_cli(CliKind::Codex).is_some()`). Gates the Codex group's
    /// enabled state in the drawer.
    pub codex_available: bool,
}

/// Everything the settings drawer needs in one round-trip.
#[derive(serde::Serialize)]
pub struct SettingsSnapshot {
    pub project: SettingsProject,
    pub user: SettingsUser,
}

/// Load the active project's config + the user registry into a single snapshot.
/// Errors (mapped to `String`) when there is no active project — the drawer only
/// opens with one active.
#[tauri::command(async)]
fn get_settings() -> Result<SettingsSnapshot, String> {
    let pc = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let user_cfg = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
    let active_name = user_cfg.active.clone();
    let projects = user_cfg
        .projects
        .iter()
        .map(|(name, r)| SettingsUserProject {
            name: name.clone(),
            root: r.path.display().to_string(),
            active: active_name.as_deref() == Some(name.as_str()),
        })
        .collect();
    Ok(SettingsSnapshot {
        project: SettingsProject {
            name: pc.project.name.clone(),
            root: pc.root.display().to_string(),
            db_path: pc.tasks.db_path.display().to_string(),
            worktrees_dir: pc.project.worktrees_dir.clone(),
            archive_days: pc.tasks.archive_days,
            log_retention_days: pc.tasks.log_retention_days,
            claude_path: pc.claude.path.clone(),
            skip_permissions: pc.claude.autonomy.unwrap_or(false),
            dispatch_steps: pc.dispatch.claude_steps.clone(),
            codex_path: pc.codex.path.clone(),
            codex_autonomy: pc.codex.autonomy.unwrap_or(false),
            codex_dispatch_steps: pc.dispatch.codex_steps.clone(),
            diff_base: pc.project.diff_base.clone(),
            // Effective list: a file-less project's committed `tasks.worktrees` is
            // empty, so read through `worktrees()` (registry/scaffold-aware).
            worktrees: pc.worktrees(),
            has_committed_file: pc.root.join("keitora.toml").exists(),
        },
        user: SettingsUser {
            claude_path: user_cfg.claude.path.clone(),
            active: active_name,
            projects,
            skip_permissions: user_cfg.skip_permissions(),
            codex_path: user_cfg.codex.path.clone(),
            codex_autonomy: user_cfg.codex.autonomy.unwrap_or(false),
            default_cli: user_cfg.default_cli.unwrap_or_default(),
            codex_available: keitora_tasks::discover_cli(keitora_tasks::CliKind::Codex).is_some(),
        },
    })
}

/// Build the registry `ProjectSettingsPatch` from the drawer's full form state.
/// The drawer always sends every field, so each maps to a definite intent:
/// `worktrees_dir`/`dispatch_steps` clear (`Some(None)`) when empty, else set;
/// retention fields always set. `db_path` is not editable from the drawer, so it is
/// left untouched (`None`). `claude_path` is intentionally excluded — a project-level
/// Claude override requires a committed file (decision 4) and is handled only on the
/// committed-file fallback path. Validates the day floors up front. Pure + testable.
fn build_settings_patch(
    archive_days: i64,
    log_retention_days: i64,
    worktrees_dir: Option<String>,
    dispatch_steps: &[String],
    codex_dispatch_steps: &[String],
    diff_base: Option<keitora_tasks::DiffBase>,
) -> Result<keitora_tasks::ProjectSettingsPatch, String> {
    // Same day-floor + trim/clean rules as the committed-file write path — shared
    // from keitora_tasks so the two cannot diverge.
    keitora_tasks::check_days("archive_days", archive_days).map_err(|e| e.to_string())?;
    keitora_tasks::check_days("log_retention_days", log_retention_days)
        .map_err(|e| e.to_string())?;
    Ok(keitora_tasks::ProjectSettingsPatch {
        db_path: None,
        // Empty worktrees_dir clears the override (Some(None)) → falls back to the
        // committed value / the `~/.keitora/<name>` default. There is no meaningful
        // "empty worktrees_dir" value; empty means "use the default".
        worktrees_dir: Some(keitora_tasks::normalize_worktrees_dir(
            worktrees_dir.as_deref(),
        )),
        archive_days: Some(Some(archive_days)),
        log_retention_days: Some(Some(log_retention_days)),
        // An empty step list is an EXPLICIT value — Some(Some([])), not a clear — so
        // emptying the dispatch editor actually clears dispatch instead of reverting
        // to the committed `[dispatch]` steps (the registry stays authoritative once
        // a project's settings have been edited).
        dispatch_steps: Some(Some(keitora_tasks::clean_dispatch_steps(dispatch_steps))),
        // Codex dispatch steps mirror `dispatch_steps` exactly — same
        // explicit-empty-clears semantics.
        codex_dispatch_steps: Some(Some(keitora_tasks::clean_dispatch_steps(
            codex_dispatch_steps,
        ))),
        // The drawer always sends the project default, so set it explicitly (None ⇒ Auto, which
        // is stored as a cleared override so it reverts to the committed/scaffold value).
        diff_base: Some(diff_base),
    })
}

/// Persist the editable project fields, then notify the UI. By default these live in
/// the user registry: the patch is written to the active project's
/// `ProjectRef` so no `keitora.toml` is required. An unregistered project (resolved via
/// cwd/`$KEITORA_PROJECT`) falls back to the surgical committed-file write — which is
/// also the only path that persists the project-level `claude_path` override.
#[tauri::command(async)]
#[allow(clippy::too_many_arguments)]
fn save_project_settings(
    app: AppHandle,
    archive_days: i64,
    log_retention_days: i64,
    worktrees_dir: Option<String>,
    claude_path: Option<String>,
    codex_path: Option<String>,
    dispatch_steps: Vec<String>,
    codex_dispatch_steps: Vec<String>,
    diff_base: Option<keitora_tasks::DiffBase>,
) -> Result<(), String> {
    let pc = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let patch = build_settings_patch(
        archive_days,
        log_retention_days,
        worktrees_dir.clone(),
        &dispatch_steps,
        &codex_dispatch_steps,
        diff_base,
    )?;
    let matched = {
        let _registry = keitora_tasks::UserConfig::write_lock();
        let mut user_cfg = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
        user_cfg
            .set_project_settings_by_root(&pc.root, patch)
            .map_err(|e| e.to_string())?
    };
    if matched {
        // The registry holds the task/dispatch settings. The project-level claude/
        // codex overrides are committed-file-only (decision 4), so persist them to
        // keitora.toml when one exists — otherwise they'd be silently dropped. For a
        // file-less project the drawer disables the fields, so a stray value is ignored.
        if pc.root.join("keitora.toml").exists() {
            keitora_tasks::set_project_claude_path(&pc.root, claude_path)
                .map_err(|e| e.to_string())?;
            keitora_tasks::set_project_codex_path(&pc.root, codex_path)
                .map_err(|e| e.to_string())?;
        }
    } else {
        // Unregistered project: surgical committed-file write (also carries the
        // project-level claude_path/codex_path, which are not stored in the
        // registry). Autonomy is round-tripped from the current file value (this
        // form has no autonomy field yet, so it must not be perturbed).
        keitora_tasks::write_project_settings(
            &pc.root,
            archive_days,
            log_retention_days,
            worktrees_dir,
            claude_path,
            codex_path,
            pc.claude.autonomy,
            pc.codex.autonomy,
            &dispatch_steps,
            &codex_dispatch_steps,
        )
        .map_err(|e| e.to_string())?;
    }
    // The project-default diff base may have changed; drop every worktree's cached base so the
    // next diff fetch re-resolves instead of serving a stale base for up to the cache TTL.
    crate::diff::invalidate_all_bases();
    let _ = app.emit("project://changed", ());
    Ok(())
}

/// Set (or clear, with `base: None`) the per-worktree diff-base override for `worktree` in the
/// active project, then invalidate the diff caches so the change reflects immediately. Registry-
/// only; an unregistered project has nowhere to persist it (returns Ok without persisting).
#[tauri::command(async)]
fn set_worktree_diff_base(
    app: AppHandle,
    worktree: String,
    base: Option<keitora_tasks::DiffBase>,
) -> Result<(), String> {
    let pc = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let current = pc.worktree_entries();
    {
        let _registry = keitora_tasks::UserConfig::write_lock();
        let mut user_cfg = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
        user_cfg
            .set_worktree_diff_base_by_root(&pc.root, &worktree, base, &current)
            .map_err(|e| e.to_string())?;
    }
    // Reflect the change now rather than after the resolved-base TTL.
    crate::diff::invalidate_base(&pc.worktree_path(&worktree));
    let _ = app.emit("project://changed", ());
    Ok(())
}

/// Commit the active project's effective settings to a committed `<root>/keitora.toml`
/// so they can be version-controlled. Seeds a fresh scaffold (relative/tilde
/// paths) when no file exists, then writes the effective retention/worktrees-dir/
/// claude/dispatch values surgically. The committed file is the shareable default;
/// per-machine registry overrides continue to win locally.
#[tauri::command(async)]
fn commit_settings_to_project(app: AppHandle) -> Result<(), String> {
    let pc = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    if pc.root.join("keitora.toml").exists() {
        // Existing file: surgically flush the effective values (preserves comments),
        // including the Codex group so committing a file doesn't silently
        // drop it.
        keitora_tasks::write_project_settings(
            &pc.root,
            pc.tasks.archive_days,
            pc.tasks.log_retention_days,
            pc.project.worktrees_dir.clone(),
            pc.claude.path.clone(),
            pc.codex.path.clone(),
            pc.claude.autonomy,
            pc.codex.autonomy,
            &pc.dispatch.claude_steps,
            &pc.dispatch.codex_steps,
        )
        .map_err(|e| e.to_string())?;
    } else {
        // New file: seed a scaffold (relative/tilde `db_path`, committed-fallback
        // worktrees) carrying the current effective values, written in ONE pass —
        // no scaffold-then-overwrite window. `pc.tasks.db_path` is the resolved
        // ABSOLUTE path, so deliberately keep the scaffold's relative form here.
        let mut seed = keitora_tasks::ProjectConfig::scaffold(&pc.root);
        seed.tasks.archive_days = pc.tasks.archive_days;
        seed.tasks.log_retention_days = pc.tasks.log_retention_days;
        seed.project.worktrees_dir = pc.project.worktrees_dir.clone();
        seed.dispatch.claude_steps = pc.dispatch.claude_steps.clone();
        seed.dispatch.codex_steps = pc.dispatch.codex_steps.clone();
        seed.claude.path = pc.claude.path.clone();
        seed.codex.path = pc.codex.path.clone();
        seed.write_initial().map_err(|e| e.to_string())?;
    }
    let _ = app.emit("project://changed", ());
    Ok(())
}

/// Write the user-level `claude.path`/`codex.path` and `default_cli` to the user
/// `keitora.toml`, then notify the UI (so claude/codex-path consumers refresh).
/// `None`/empty path clears that CLI's override.
#[tauri::command(async)]
fn save_user_settings(
    app: AppHandle,
    claude_path: Option<String>,
    codex_path: Option<String>,
    default_cli: keitora_tasks::CliKind,
) -> Result<(), String> {
    keitora_tasks::set_user_claude_path(claude_path).map_err(|e| e.to_string())?;
    keitora_tasks::set_user_codex_path(codex_path).map_err(|e| e.to_string())?;
    {
        // After the path setters: they take the registry lock themselves.
        let _registry = keitora_tasks::UserConfig::write_lock();
        let mut user_cfg = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
        user_cfg.default_cli = Some(default_cli);
        user_cfg.save().map_err(|e| e.to_string())?;
    }
    let _ = app.emit("project://changed", ());
    Ok(())
}

/// Persist the user-level `[claude] autonomy` / `[codex] autonomy` flag: `cli` selects which provider's autonomy flag is
/// written, so this single command replaces what would otherwise be a parallel
/// `set_bypass_approvals` command. Persist-only: no `project://changed` emit,
/// since nothing reacts at runtime — the flag is read only when building the next
/// session's launch command. The drawer prompts the user to restart the daemon so
/// running sessions respawn with the new args.
#[tauri::command(async)]
fn set_skip_permissions(cli: keitora_tasks::CliKind, enabled: bool) -> Result<(), String> {
    let _registry = keitora_tasks::UserConfig::write_lock();
    let mut user_cfg = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
    match cli {
        keitora_tasks::CliKind::Claude => user_cfg.claude.autonomy = Some(enabled),
        keitora_tasks::CliKind::Codex => user_cfg.codex.autonomy = Some(enabled),
    }
    user_cfg.save().map_err(|e| e.to_string())
}

/// Remove a project from the user registry (clearing `active` if it pointed at
/// it), then notify the UI. Removing the active project drops the app into
/// onboarding; the frontend disables that case.
#[tauri::command(async)]
fn remove_project(
    app: AppHandle,
    client: State<SharedBackend>,
    name: String,
) -> Result<(), String> {
    // Session keys carry the project's resolved name: read it before the entry goes.
    let resolved = keitora_tasks::UserConfig::load()
        .map(|user| resolved_project_name(&user, &name))
        .unwrap_or_else(|_| name.clone());
    keitora_tasks::remove_project_from_registry(&name).map_err(|e| e.to_string())?;
    // Nothing of a removed project is resumed by recovery.
    if let Some(recovery) = client.recovery() {
        recovery.cancel_project(&resolved);
    }
    // Removing the active project clears `active` in the registry — drop the
    // cached `(name, db_path)` so the next resolve reflects the new state.
    invalidate_active_project();
    // Re-point (or, if the active project was the one removed, tear down) the tasks +
    // config watchers so they stop watching a removed project's files. Runs after
    // invalidate so the watchers resolve the post-removal active state.
    restart_project_watchers(&app);
    let _ = app.emit("project://changed", ());
    // Re-snapshot the registry so a removed project stops being (re)watched on
    // the next setup; existing watchers for surviving projects are unaffected.
    bg_notifier::start_bg_notifier(app);
    Ok(())
}

/// The resolved name (`[project].name`) of the project registered as
/// `registry_name`; the registry name itself when it cannot be resolved.
fn resolved_project_name(user: &keitora_tasks::UserConfig, registry_name: &str) -> String {
    user.projects
        .get(registry_name)
        .and_then(|pref| keitora_tasks::effective_config(pref).ok())
        .map(|pc| pc.project.name)
        .unwrap_or_else(|| registry_name.to_string())
}

/// The user's real login `$PATH`, resolved once per process. A GUI launch
/// (LaunchServices) inherits only a minimal PATH; we capture the login shell's
/// PATH so spawned PTYs and the headless dispatcher can find `claude` and the
/// tools it (and the user) invoke.
///
/// Delegates to `keitora_tasks`'s shared cache so the whole process resolves the
/// (expensive) login shell exactly once — `claude` spawns here and `git` spawns
/// inside `keitora-tasks` share the one cached value rather than each forking it.
fn cached_login_path() -> Option<String> {
    keitora_tasks::cached_login_path()
}

/// Locate a `keitora` binary to install onto PATH: the binary next to the app
/// executable (bundled `externalBin` sidecar in a packaged `.app`, or
/// `target/debug/keitora` under `tauri dev`), else the main worktree's build tree.
fn locate_keitora_binary() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(p) = crate::cli_install::sidecar_candidate(&exe) {
            return Some(p);
        }
    }
    keitora_tasks::Config::find()
        .ok()
        .and_then(|c| crate::cli_install::locate_installable(&c.main_worktree_path()))
}

/// Detect whether the `keitora` CLI is reachable on PATH, and — if not — whether a
/// binary can be linked. Drives the first-run install prompt (frontend polls this on
/// mount). Pure decision logic lives in `cli_install`.
#[tauri::command(async)]
fn keitora_cli_status() -> crate::cli_install::CliStatus {
    use crate::cli_install::{self, Action, CliStatus};
    let on_path = keitora_tasks::keitora_on_path();
    let located = locate_keitora_binary();
    match cli_install::decide(on_path.as_deref(), located.as_deref()) {
        Action::AlreadyOnPath(p) => CliStatus {
            state: "on_path".into(),
            path: Some(p),
            note: None,
        },
        Action::Install(src) => CliStatus {
            state: "installable".into(),
            path: Some(src.to_string_lossy().into_owned()),
            note: None,
        },
        Action::NoBinary => CliStatus {
            state: "missing".into(),
            path: None,
            note: None,
        },
    }
}

/// Symlink a located `keitora` binary into `~/.local/bin` so it (and session hooks in
/// any project) can resolve it on PATH. No-op-safe: if `keitora` is already on PATH it
/// reports `on_path` without touching anything. Returns the resulting status.
#[tauri::command(async)]
fn install_keitora_cli() -> Result<crate::cli_install::CliStatus, String> {
    use crate::cli_install::{self, CliStatus};
    if let Some(p) = keitora_tasks::keitora_on_path() {
        return Ok(CliStatus {
            state: "on_path".into(),
            path: Some(p),
            note: None,
        });
    }
    let located =
        locate_keitora_binary().ok_or_else(|| "no keitora binary found to install".to_string())?;
    let link = cli_install::default_link_path()
        .ok_or_else(|| "cannot determine home directory".to_string())?;
    let made = cli_install::install_symlink(&located, &link)?;
    let note = made.parent().and_then(|dir| {
        if cli_install::dir_on_path(dir, cached_login_path().as_deref()) {
            None
        } else {
            Some(format!(
                "{} is not on your PATH — add it to your shell profile to run `keitora` directly",
                dir.display()
            ))
        }
    });
    Ok(CliStatus {
        state: "installed".into(),
        path: Some(made.to_string_lossy().into_owned()),
        note,
    })
}

/// One-time startup: if no `claude` path is configured (neither the active
/// project nor the user registry), discover it and persist to the user registry
/// so subsequent launches skip discovery. Best-effort — failures are non-fatal
/// (we fall back to the inherited PATH at spawn time).
fn ensure_claude_path() {
    let Ok(user) = keitora_tasks::UserConfig::load() else {
        return;
    };
    let project = keitora_tasks::resolve_active().ok();
    if keitora_tasks::resolve_claude_path(project.as_ref(), &user).is_some() {
        return; // already configured — nothing to discover
    }
    if let Some(found) = keitora_tasks::discover_claude() {
        // Discovery is slow; re-read under the lock so a write that landed
        // meanwhile is not overwritten with the stale snapshot.
        let _registry = keitora_tasks::UserConfig::write_lock();
        if let Ok(mut user) = keitora_tasks::UserConfig::load() {
            user.set_claude_path(found);
            let _ = user.save();
        }
    }
}

/// Process-global cache of the resolved active project's `(name, db_path)`.
///
/// `resolve_active()` reads + parses `keitora.toml` from disk; without this cache
/// every `pty_write`/`pty_resize`/`pty_kill` (i.e. every keystroke) would hit
/// disk. Worse, if the active project changed mid-session the rebuilt
/// [`SessionKey`] would no longer match the live daemon session, so writes to a
/// perfectly valid session would fail. Caching pins `(name, db_path)` for a
/// session's lifetime; [`set_active_project`] invalidates it on a project switch.
static ACTIVE_PROJECT: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Clear the cached active project so the next `active_project()` re-resolves
/// from disk. Called whenever the active project changes.
fn invalidate_active_project() {
    *ACTIVE_PROJECT.lock().unwrap() = None;
}

/// Side effects for "a (different) project just became active": drop the cached
/// active project, (re)install session hooks for the now-active project's
/// worktrees off-thread, and kick off a best-effort repair pass (also off-thread)
/// that backfills missing worktree paths from `git worktree list`. Both spawns run
/// after the caller has persisted the new active project, so `resolve_active()`
/// inside them sees the new project.
///
/// `project_root` is the already-resolved root of the newly-active project, passed
/// in by the caller to avoid a redundant `resolve_active()` read just for the path.
///
/// Used by every command that activates a project (`set_active_project`,
/// `onboard_project`, `migrate_legacy`) — NOT by `remove_project` (a deactivation:
/// it keeps the bare `invalidate_active_project`) nor `set_worktrees` (no activation
/// change: it spawns the install directly).
fn on_project_activated(app: AppHandle, project_root: std::path::PathBuf) {
    invalidate_active_project();
    thread::spawn(hooks::install_active_session_hooks);
    // Best-effort off-thread repair: backfill any path-less worktree entries from
    // `git worktree list` so the rail shows correct paths immediately on activation
    // (decision #2 — the only repair trigger). On `Ok(true)` (something changed),
    // emit `worktrees://changed` + `project://changed` so every caller — including
    // `onboard_project` and `migrate_legacy` — gets a UI refresh without each
    // needing its own emit. Log and ignore errors; must NOT fail/panic activation.
    thread::spawn(
        move || match keitora_tasks::repair_project_worktree_paths(&project_root) {
            Ok(true) => {
                let _ = app.emit("worktrees://changed", ());
                let _ = app.emit("project://changed", ());
            }
            Ok(false) => {}
            Err(e) => eprintln!("[keitora] repair_project_worktree_paths: {e}"),
        },
    );
}

/// Stop handle for the single live tasks/sessions watcher thread.
///
/// `start_tasks_watcher` is restartable: each call replaces this sender (dropping
/// the previous one). The previous watcher thread observes its stop receiver
/// disconnect (or a sent `()`) and exits, so the watcher always follows the
/// active project after a switch without leaking threads. See
/// [`replace_stop`] for the swap and [`start_tasks_watcher`] for the
/// stop-aware loop.
static TASKS_WATCHER_STOP: Mutex<Option<mpsc::SyncSender<()>>> = Mutex::new(None);

/// Stop handle for the single live config-file watcher thread. Mirrors
/// [`TASKS_WATCHER_STOP`]: `start_config_watcher` replaces this on each call, so the
/// watcher follows the active project's `keitora.toml` across project switches without
/// leaking the previous thread. See [`start_config_watcher`].
static CONFIG_WATCHER_STOP: Mutex<Option<mpsc::SyncSender<()>>> = Mutex::new(None);

/// Install `new_tx` as `slot`'s live stop sender, returning the previous one (if
/// any) so the caller can decide its fate. Dropping the returned sender (the common
/// case — callers just let it drop) disconnects the *previous* watcher thread's stop
/// receiver, signalling it to exit on its next loop iteration.
///
/// Slot-parameterized so the tasks and config watchers share one swap primitive, and
/// pulled out as a standalone function so the sender-replacement semantics can be
/// unit-tested without spawning real `notify` watchers or filesystem threads.
fn replace_stop(
    slot: &Mutex<Option<mpsc::SyncSender<()>>>,
    new_tx: mpsc::SyncSender<()>,
) -> Option<mpsc::SyncSender<()>> {
    slot.lock().unwrap().replace(new_tx)
}

/// Resolve the active project's `(name, db_path)` from the registry, caching the
/// result. Used to build the [`SessionKey`] the daemon namespaces sessions by,
/// and to tell the daemon which task DB to mark offline on child exit. Errors
/// (mapped to `String`) when no project is active.
///
/// Reads the [`ACTIVE_PROJECT`] cache first; on a miss it resolves from disk and
/// populates the cache. The cache is invalidated by [`set_active_project`], so a
/// session keeps a stable key for its whole lifetime (no per-keystroke disk I/O,
/// no mid-session key drift).
fn active_project() -> Result<(String, String), String> {
    {
        let cache = ACTIVE_PROJECT.lock().unwrap();
        if let Some(cached) = cache.as_ref() {
            return Ok(cached.clone());
        }
    }
    let pc = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let resolved = (
        pc.project.name.clone(),
        pc.tasks.db_path.to_string_lossy().into_owned(),
    );
    *ACTIVE_PROJECT.lock().unwrap() = Some(resolved.clone());
    Ok(resolved)
}

/// Map a [`PtyBackendError`] into the `String` error the webview commands return.
fn daemon_err(e: PtyBackendError) -> String {
    e.to_string()
}

/// Map a [`PtyBackendError`] to a STABLE, machine-matchable leading token for the
/// frontend's `doAttach` catch (`TerminalPane.tsx`). The error's `Display` is
/// prose ("herdr unreachable" / "no live session"), which the frontend cannot
/// reliably `String#includes`-match — so `pty_attach` returns these tokens
/// instead. Keep these EXACTLY in sync with the `msg.includes(...)` checks in
/// `doAttach`:
///   - `NotConnected`   → retry the attach (client still connecting)
///   - `NoLiveSession`  → fall back to `spawn()` (authoritative miss)
///   - `Recovering`     → the server is down or being recovered (never spawn)
///   - `Terminated`     → the user terminated the session (never spawn)
///   - anything else    → `Internal: <display>` (treated as a spawn fallback)
fn attach_err_kind(e: &PtyBackendError) -> String {
    match e {
        PtyBackendError::NotConnected => "NotConnected".to_string(),
        PtyBackendError::NoSuchSession => "NoLiveSession".to_string(),
        PtyBackendError::RecoveryPending | PtyBackendError::RecoveryPaused => {
            RECOVERING_TOKEN.to_string()
        }
        PtyBackendError::Terminated => TERMINATED_TOKEN.to_string(),
        other => format!("Internal: {other}"),
    }
}

/// The one error token the frontend sees for a spawn/attach refused while the
/// server is down or being recovered.
const RECOVERING_TOKEN: &str = "Recovering";

/// The one error token the frontend sees for a spawn/attach refused because the
/// user terminated the session.
const TERMINATED_TOKEN: &str = "Terminated";

/// The takeover size the frontend measured: `Some((cols, rows))` only when it sent both
/// and both are at least 1. Anything else means "unknown" and the backend picks one.
fn requested_size(cols: Option<u16>, rows: Option<u16>) -> Option<(u16, u16)> {
    match (cols, rows) {
        (Some(c), Some(r)) if c >= 1 && r >= 1 => Some((c, r)),
        _ => None,
    }
}

/// Map a `pty_spawn` error: a server under recovery is the `Recovering` token, a
/// terminated session the `Terminated` token; everything else keeps its
/// [`daemon_err`] text.
fn spawn_err_kind(e: PtyBackendError) -> String {
    match e {
        PtyBackendError::RecoveryPending | PtyBackendError::RecoveryPaused => {
            RECOVERING_TOKEN.to_string()
        }
        PtyBackendError::Terminated => TERMINATED_TOKEN.to_string(),
        other => daemon_err(other),
    }
}

/// Inner implementation of PTY spawn, callable from the Tauri command wrapper.
/// The daemon owns the PTY now; this builds the launch command + key and forwards
/// to it.
///
/// Spawn the session command in `cwd`, keyed by `id` (the worktree name).
/// `command` overrides the launch command; the default resumes Claude (falling
/// back to a fresh session). The backend owns the PTY: it delivers output and
/// the exit to the pane attached to the session (see `pty_attach`), and marks the
/// worktree offline on child exit. Returns the daemon-side generation stamp for the new session.
// The Tauri command's parameters, passed through one to one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_pty(
    client: &dyn PtyBackend,
    project: String,
    id: String,
    cwd: String,
    rows: Option<u16>,
    cols: Option<u16>,
    command: Option<String>,
    fresh: bool,
) -> Result<u64, String> {
    let size = requested_size(cols, rows);
    // The PTY identity is the explicit `project` param; `active_project()` is kept
    // only for the `db_path` the daemon needs to mark the worktree offline on exit.
    let (_, db_path) = active_project()?;
    let key = SessionKey {
        project,
        worktree: id,
    };

    // An explicit `command` goes straight to `spawn`. The default path builds an
    // `AgentLaunch` from persisted state and goes through `spawn_agent`; the backend
    // derives the pane PATH from `launch.path` (see `backend::herdr::env::pane_env`).
    if let Some(command) = command {
        return client
            .spawn(key, cwd, db_path, size, Some(command))
            .map_err(spawn_err_kind);
    }
    // Re-derive the default launch from persisted state on EVERY spawn.
    // This runs on first launch, hard-restart, daemon-crash re-attach, and
    // app relaunch alike — the frontend's `ptySpawn` never carries a command — so
    // it is the ONLY place a transferred Codex worktree's identity (its `cli` +
    // persisted `codex_thread_id`) is reasserted. If it did not, every restart of
    // a transferred worktree would silently revert to a fresh Codex session (or
    // to Claude). There is no session-manager-side cache to lean on instead.
    // The same resolver recovery resumes with; a key it cannot place falls back
    // to the active project's settings.
    let launch = resolve_agent_launch(&key)
        .map(|(_, launch)| launch)
        .unwrap_or_else(|| {
            let project = keitora_tasks::resolve_active().ok();
            agent_launch_in(project.as_ref(), &key.worktree)
        });
    let launch = AgentLaunch { fresh, ..launch };

    client
        .spawn_agent(key, cwd, db_path, size, launch)
        .map_err(spawn_err_kind)
}

/// The agent launch for `worktree` from its registry entry in `project` (its
/// `cli` and stored session id) and the user/project config (agent path,
/// autonomy). Never `fresh`.
fn agent_launch_in(project: Option<&keitora_tasks::ProjectConfig>, worktree: &str) -> AgentLaunch {
    let user = keitora_tasks::UserConfig::load().unwrap_or_default();
    agent_launch_with(&user, project, worktree)
}

/// [`agent_launch_in`] with the user config already loaded.
fn agent_launch_with(
    user: &keitora_tasks::UserConfig,
    project: Option<&keitora_tasks::ProjectConfig>,
    worktree: &str,
) -> AgentLaunch {
    // The worktree's registry entry (found by name), carrying its `cli` and
    // `codex_thread_id`.
    let entry = project.and_then(|p| {
        p.worktree_entries()
            .into_iter()
            .find(|e| e.name == worktree)
    });
    let cli = keitora_tasks::resolve_cli(entry.as_ref(), user);
    let path =
        keitora_tasks::resolve_provider_path(cli, project, user).unwrap_or_else(|| match cli {
            keitora_tasks::CliKind::Claude => "claude".to_string(),
            keitora_tasks::CliKind::Codex => "codex".to_string(),
        });
    let autonomy = keitora_tasks::resolve_autonomy(cli, project, user);
    let resume_target = match cli {
        keitora_tasks::CliKind::Claude => entry.and_then(|e| e.claude_session_id.clone()),
        keitora_tasks::CliKind::Codex => entry.and_then(|e| e.codex_thread_id.clone()),
    };
    AgentLaunch {
        cli,
        path,
        autonomy,
        resume_target,
        fresh: false,
    }
}

/// The worktree path and agent launch for `key` in any registered project, as a
/// spawn from its pane would use. `None` when no registered project lists the
/// worktree.
pub(crate) fn resolve_agent_launch(key: &SessionKey) -> Option<(String, AgentLaunch)> {
    let user = keitora_tasks::UserConfig::load().ok()?;
    launch_for(&user, key)
}

/// [`resolve_agent_launch`] over a loaded registry: the project is matched by
/// its resolved name (`[project].name`, the name session keys carry), not the
/// registry map key; the launch uses that project's own settings.
fn launch_for(user: &keitora_tasks::UserConfig, key: &SessionKey) -> Option<(String, AgentLaunch)> {
    for pref in user.projects.values() {
        let Ok(pc) = keitora_tasks::effective_config(pref) else {
            continue;
        };
        if pc.project.name != key.project {
            continue;
        }
        let listed = key.worktree == pc.main_worktree()
            || pc.worktree_entries().iter().any(|e| e.name == key.worktree);
        if !listed {
            continue;
        }
        let cwd = pc
            .worktree_path(&key.worktree)
            .to_string_lossy()
            .into_owned();
        return Some((cwd, agent_launch_with(user, Some(&pc), &key.worktree)));
    }
    None
}

/// Whether `key`'s worktree still exists: a registered project whose resolved
/// name (`[project].name`, the name session keys carry) is `key.project` lists an
/// available worktree named `key.worktree`. A user config that cannot be read
/// answers `true`, so a transient read error never drops a session.
pub(crate) fn worktree_exists(key: &SessionKey) -> bool {
    let Ok(cfg) = keitora_tasks::UserConfig::load() else {
        return true;
    };
    let mut listed = Vec::new();
    for pref in cfg.projects.values() {
        let Ok(pc) = keitora_tasks::effective_config(pref) else {
            continue;
        };
        let project = pc.project.name.clone();
        if project != key.project {
            continue;
        }
        let Ok(wts) = keitora_tasks::Config::from_project(pc).resolved_worktrees() else {
            continue;
        };
        listed.extend(wts.into_iter().map(|w| (project.clone(), w.name, w.status)));
    }
    worktree_listed(&listed, key)
}

/// Pure half of [`worktree_exists`]: is `key` among the listed
/// `(project, worktree, status)` triples as an available worktree?
fn worktree_listed(
    listed: &[(String, String, keitora_tasks::WorktreeStatus)],
    key: &SessionKey,
) -> bool {
    listed.iter().any(|(p, w, s)| {
        *p == key.project && *w == key.worktree && *s == keitora_tasks::WorktreeStatus::Available
    })
}

/// Tauri command wrapper for `spawn_pty`. Resolves the managed backend and
/// delegates to the inner `pub(crate)` function.
#[tauri::command(async)]
#[allow(clippy::too_many_arguments)]
fn pty_spawn(
    client: State<SharedBackend>,
    project: String,
    id: String,
    cwd: String,
    rows: Option<u16>,
    cols: Option<u16>,
    command: Option<String>,
    fresh: Option<bool>,
) -> Result<u64, String> {
    spawn_pty(
        client.inner().as_ref(),
        project,
        id,
        cwd,
        rows,
        cols,
        command,
        fresh.unwrap_or(false),
    )
}

/// Attach to an existing live daemon session (re-attach after a reload/respawn).
/// Returns the backend-side generation plus the base64-encoded initial screen for
/// the webview to replay into its xterm.
#[derive(Serialize)]
struct AttachPayload {
    gen: u64,
    /// Base64 of the raw VT scrollback bytes (the webview `atob`s + writes it).
    scrollback: String,
    /// Pass back to `pty_detach` so a late detach cannot end a newer attach.
    attach_id: u64,
    /// Whether `pty_repaint` really repaints this session; a pane whose backend
    /// cannot repaint must never drop output it has not written.
    repaints: bool,
}

impl From<AttachResult> for AttachPayload {
    fn from(r: AttachResult) -> Self {
        AttachPayload {
            gen: r.gen,
            scrollback: base64::engine::general_purpose::STANDARD.encode(&r.scrollback),
            attach_id: r.attach_id,
            repaints: r.repaints,
        }
    }
}

/// One message on a pane's output channel (the TS `OutputMsg`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum OutputMsg {
    /// Base64 VT bytes; `full` marks a whole-viewport repaint.
    Frame { data: String, full: bool },
    /// The session's process ended.
    Exit,
}

/// An [`OutputSink`] over the webview's per-attach channel. A failed send (the
/// webview went away) is dropped: the pane is gone and its detach follows.
struct ChannelSink(Channel<OutputMsg>);

impl OutputSink for ChannelSink {
    fn frame(&self, b64: String, full: bool) {
        let _ = self.0.send(OutputMsg::Frame { data: b64, full });
    }
    fn exit(&self) {
        let _ = self.0.send(OutputMsg::Exit);
    }
}

/// Attach the webview to a live daemon session for `id` (the worktree name). The
/// session's output and exit arrive on `on_output` from now on.
#[tauri::command(async)]
fn pty_attach(
    client: State<SharedBackend>,
    project: String,
    id: String,
    rows: Option<u16>,
    cols: Option<u16>,
    on_output: Channel<OutputMsg>,
) -> Result<AttachPayload, String> {
    let key = SessionKey {
        project,
        worktree: id,
    };
    client
        .attach(
            key,
            requested_size(cols, rows),
            Arc::new(ChannelSink(on_output)),
        )
        .map(Into::into)
        .map_err(|e| attach_err_kind(&e))
}

/// Detach the webview from a session; the daemon keeps it live. With
/// `attach_id`, only that attach is detached: a newer attach is left alone.
#[tauri::command(async)]
fn pty_detach(
    client: State<SharedBackend>,
    project: String,
    id: String,
    attach_id: Option<u64>,
) -> Result<(), String> {
    let key = SessionKey {
        project,
        worktree: id,
    };
    client.detach(key, attach_id).map_err(daemon_err)
}

/// Write bytes into a live PTY session keyed by `id` (via the daemon). The shared
/// write path reused by `pty_write` (user keystrokes) and dispatch/transfer
/// injection.
pub(crate) fn inject(
    client: &dyn PtyBackend,
    project: &str,
    id: &str,
    data: &str,
) -> Result<(), String> {
    let key = SessionKey {
        project: project.to_string(),
        worktree: id.to_string(),
    };
    client.write(&key, data.as_bytes()).map_err(daemon_err)
}

/// Paste `text` into the session keyed by `id` as one unit. The backend
/// brackets it; a refusal comes back as the error's `Display`
/// (`PasteTooLarge: {bytes} > {limit}` when over the size cap).
pub(crate) fn paste_text(
    client: &dyn PtyBackend,
    project: &str,
    id: &str,
    text: &str,
) -> Result<(), String> {
    let key = SessionKey {
        project: project.to_string(),
        worktree: id.to_string(),
    };
    client.paste(&key, text).map_err(daemon_err)
}

/// Paste text into the PTY as one bracketed unit (via the backend).
#[tauri::command]
fn pty_paste(
    client: State<SharedBackend>,
    project: String,
    id: String,
    text: String,
) -> Result<(), String> {
    paste_text(client.inner().as_ref(), &project, &id, &text)
}

/// Write user keystrokes to the PTY (via the daemon).
#[tauri::command]
fn pty_write(
    client: State<SharedBackend>,
    project: String,
    id: String,
    data: String,
    t0: Option<f64>,
) -> Result<(), String> {
    if t0.is_some() {
        crate::latency_probe::on_command_entry(
            &SessionKey {
                project: project.clone(),
                worktree: id.clone(),
            },
            t0,
        );
    }
    inject(client.inner().as_ref(), &project, &id, &data)
}

/// Save pasted clipboard image bytes to a temp file, returning its absolute path
/// (unquoted — the frontend quotes it before injecting into the PTY).
///
/// Takes base64 rather than a `Vec<u8>`: Tauri's JSON IPC would otherwise carry a
/// screenshot as an array of millions of decimal numbers (a 4K PNG is 4-8 MB, i.e.
/// tens of MB of JSON to build, stringify and parse on the UI thread). Same encoding
/// the PTY output/scrollback path already uses.
#[tauri::command(async)]
fn save_clipboard_image(bytes_b64: String, ext: String) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(bytes_b64.as_bytes())
        .map_err(|e| format!("bad base64: {e}"))?;
    crate::clipboard_image::save_image_to_temp(&bytes, &ext)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| e.to_string())
}

/// Inner implementation of PTY resize, callable from the Tauri command wrapper.
pub(crate) fn resize_pty(
    client: &dyn PtyBackend,
    project: &str,
    id: &str,
    rows: u16,
    cols: u16,
) -> Result<(), String> {
    let key = SessionKey {
        project: project.to_string(),
        worktree: id.to_string(),
    };
    client.resize(&key, cols, rows).map_err(daemon_err)
}

/// Resize the PTY to match the xterm viewport (via the daemon).
#[tauri::command(async)]
fn pty_resize(
    client: State<SharedBackend>,
    project: String,
    id: String,
    rows: u16,
    cols: u16,
) -> Result<(), String> {
    resize_pty(client.inner().as_ref(), &project, &id, rows, cols)
}

/// Scroll a session's viewport by `lines` from `source` with the pointer at the
/// 0-based cell `column`,`row` (see [`PtyBackend::scroll`]): a wheel tick over the
/// pane or a history page key.
#[tauri::command(async)]
#[allow(clippy::too_many_arguments)]
fn pty_scroll(
    client: State<SharedBackend>,
    project: String,
    id: String,
    direction: ScrollDirection,
    source: ScrollSource,
    lines: u16,
    column: u16,
    row: u16,
) -> Result<(), String> {
    let key = SessionKey {
        project,
        worktree: id,
    };
    client
        .scroll(&key, direction, source, lines, column, row)
        .map_err(daemon_err)
}

/// How many lines above the live tail a session's viewport is scrolled (see
/// [`PtyBackend::scroll_offset`]); `None` when the backend cannot tell. Async: it
/// runs a herdr CLI call.
#[tauri::command(async)]
fn pty_scroll_offset(
    client: State<SharedBackend>,
    project: String,
    id: String,
) -> Result<Option<u32>, String> {
    let key = SessionKey {
        project,
        worktree: id,
    };
    client.scroll_offset(&key).map_err(daemon_err)
}

/// The last `lines` lines of a session's history as plain text (see
/// [`PtyBackend::read_history`]). Async: it runs a herdr CLI call.
#[tauri::command(async)]
fn pty_read_history(
    client: State<SharedBackend>,
    project: String,
    id: String,
    lines: u32,
) -> Result<String, String> {
    let key = SessionKey {
        project,
        worktree: id,
    };
    client.read_history(&key, lines).map_err(daemon_err)
}

/// Push one fresh full-viewport repaint frame for a session (see [`PtyBackend::repaint`]).
/// The frontend calls this once its xterm/WebGL renderer is live, so an idle restored pane
/// paints on boot instead of showing a blank terminal with a blinking block cursor.
#[tauri::command(async)]
fn pty_repaint(client: State<SharedBackend>, project: String, id: String) -> Result<(), String> {
    let key = SessionKey {
        project,
        worktree: id,
    };
    client.repaint(&key).map_err(daemon_err)
}

/// Reap a live PTY session on demand, identified by `id` (the worktree name) and
/// the generation the caller spawned (`gen`). The daemon hangs up the child
/// (SIGHUP), gen-gated so a stale unmount-kill cannot terminate a newer session
/// that reused the same id; the daemon then reports the exit to the attached pane
/// and marks the worktree offline. If the daemon is unreachable the kill is queued and delivered
/// on reconnect (or times out — see [`PtyBackend::kill`]).
#[tauri::command(async)]
fn pty_kill(
    client: State<SharedBackend>,
    project: String,
    id: String,
    gen: u64,
) -> Result<(), String> {
    let key = SessionKey {
        project,
        worktree: id,
    };
    client.kill(&key, gen).map_err(daemon_err)
}

/// Terminate a session: it stays down across app restarts and recovery until
/// [`session_launch`]. `gen` is the generation the pane holds, if any. The flag
/// is persisted before anything is killed, so a terminate that fails to persist
/// leaves the session running.
#[tauri::command(async)]
fn session_terminate(
    client: State<SharedBackend>,
    app: tauri::AppHandle,
    project: String,
    id: String,
    gen: Option<u64>,
) -> Result<(), String> {
    let key = SessionKey {
        project,
        worktree: id,
    };
    let result = terminate_session(client.inner().as_ref(), &key, gen);
    // A failed kill still leaves the flag set: the rail must show it.
    if !matches!(result, Err(TerminateError::NotPersisted(_))) {
        let _ = app.emit("worktrees://changed", ());
        let _ = app.emit("sessions://changed", ());
    }
    result.map_err(|e| match e {
        TerminateError::NotPersisted(m) | TerminateError::Kill(m) => m,
    })
}

/// Why [`terminate_session`] failed.
#[derive(Debug, PartialEq)]
enum TerminateError {
    /// The flag could not be stored; nothing else was done.
    NotPersisted(String),
    /// The flag is stored, but the kill failed.
    Kill(String),
}

/// [`session_terminate`] without the events: set the flag, then kill whatever
/// is live (see [`PtyBackend::kill_live`]; `gen` is the generation the caller
/// last saw) and drop the key from recovery. A kill (or list) refused because
/// the server is recovering still succeeds: recovery never resumes a
/// terminated key.
fn terminate_session(
    client: &dyn PtyBackend,
    key: &SessionKey,
    gen: Option<u64>,
) -> Result<(), TerminateError> {
    client
        .set_terminated(key, true)
        .map_err(|e| TerminateError::NotPersisted(daemon_err(e)))?;
    match client.kill_live(key, gen) {
        Ok(()) => Ok(()),
        Err(PtyBackendError::RecoveryPending | PtyBackendError::RecoveryPaused) => Ok(()),
        Err(e) => Err(TerminateError::Kill(daemon_err(e))),
    }
}

/// Launch a terminated session again: clear the flag, then spawn it the normal
/// way (`fresh` false resumes the last conversation). A spawn that then fails
/// leaves the flag cleared, and the pane shows the spawn failure as usual.
#[tauri::command(async)]
#[allow(clippy::too_many_arguments)]
fn session_launch(
    client: State<SharedBackend>,
    app: tauri::AppHandle,
    project: String,
    id: String,
    cwd: String,
    rows: Option<u16>,
    cols: Option<u16>,
    fresh: Option<bool>,
) -> Result<u64, String> {
    let backend = client.inner().as_ref();
    let key = SessionKey {
        project: project.clone(),
        worktree: id.clone(),
    };
    launch_session(
        backend,
        &key,
        || {
            let _ = app.emit("worktrees://changed", ());
        },
        || {
            spawn_pty(
                backend,
                project,
                id,
                cwd,
                rows,
                cols,
                None,
                fresh.unwrap_or(false),
            )
        },
    )
}

/// [`session_launch`] with the event (`cleared`) and the spawn injected: the
/// spawn runs only once the flag is cleared.
fn launch_session(
    client: &dyn PtyBackend,
    key: &SessionKey,
    cleared: impl FnOnce(),
    spawn: impl FnOnce() -> Result<u64, String>,
) -> Result<u64, String> {
    client.set_terminated(key, false).map_err(daemon_err)?;
    cleared();
    spawn()
}

/// The `daemon://status` snapshot for the settings drawer / status pill. Same
/// shape as the pushed `daemon://status` event.
#[tauri::command(async)]
fn daemon_status(client: State<SharedBackend>) -> StatusPayload {
    client.status_payload_snapshot()
}

/// The one-time "panes now use your normal tool folders" notice: the old
/// isolated folders left in place, or `None` (nothing to say, or already said).
#[tauri::command(async)]
fn take_xdg_leak_notice(client: State<SharedBackend>) -> Option<Vec<String>> {
    client.take_xdg_leak_notice()
}

/// Where herdr server recovery stands. Same shape as the pushed
/// `daemon://recovery` event.
#[tauri::command(async)]
fn daemon_recovery(client: State<SharedBackend>) -> crate::backend::herdr::gate::RecoveryPayload {
    client.recovery_snapshot()
}

/// The banner's "Resume now" / "Resume all". A no-op without recovery.
#[tauri::command(async)]
fn recovery_resume_now(client: State<SharedBackend>) {
    resume_now_in(client.inner().as_ref());
}

fn resume_now_in(client: &dyn PtyBackend) {
    if let Some(recovery) = client.recovery() {
        recovery.resume_now();
    }
}

/// A suspended pane became active (or its Resume was clicked): resume it next.
#[tauri::command(async)]
fn recovery_focus(client: State<SharedBackend>, project: String, worktree: String) {
    focus_in(client.inner().as_ref(), project, worktree);
}

fn focus_in(client: &dyn PtyBackend, project: String, worktree: String) {
    if let Some(recovery) = client.recovery() {
        recovery.focus(&SessionKey { project, worktree });
    }
}

/// "Start fresh conversation" on a recovering or suspended pane: `true` when
/// recovery took it over; `false` means start it fresh the ordinary way.
#[tauri::command(async)]
fn recovery_start_fresh(client: State<SharedBackend>, project: String, worktree: String) -> bool {
    start_fresh_in(client.inner().as_ref(), project, worktree)
}

fn start_fresh_in(client: &dyn PtyBackend, project: String, worktree: String) -> bool {
    client
        .recovery()
        .is_some_and(|r| r.start_fresh(&SessionKey { project, worktree }))
}

/// Live daemon process stats (CPU / memory / uptime / version) for the rail daemon card:
/// the backend's last sample (a cache read; updates arrive as `daemon://stats`). Only the
/// herdr backend fills it; any other backend returns the `running: false` default.
#[tauri::command(async)]
fn daemon_stats(client: State<SharedBackend>) -> DaemonStats {
    client.daemon_stats()
}

/// Stop the daemon (Settings → Daemon "Stop"). Sends `Shutdown` and engages the
/// pause gate so the daemon stays down (start-on-demand is suppressed) until an
/// explicit restart or a new pane is opened.
#[tauri::command(async)]
fn daemon_shutdown(client: State<SharedBackend>) -> Result<(), String> {
    client.shutdown().map_err(daemon_err)
}

/// Restart the daemon (Settings → Daemon "Restart"). Clears the pause gate and
/// sends `Shutdown`; auto-reconnect respawns a fresh daemon immediately.
#[tauri::command(async)]
fn daemon_restart(client: State<SharedBackend>) -> Result<(), String> {
    client.restart().map_err(daemon_err)
}

/// List live daemon sessions as project/worktree hints (background-session count
/// + Settings display). Groups/counts happen on the frontend.
#[tauri::command(async)]
fn daemon_list_sessions(client: State<SharedBackend>) -> Result<Vec<SessionHint>, String> {
    client
        .list_sessions()
        .map(|v| v.iter().map(session_hint).collect())
        .map_err(daemon_err)
}

/// "Quitting now" latch, managed as Tauri state. Set by the quit commands before
/// `app.exit(0)` so the `CloseRequested` handler doesn't re-prevent the close that
/// our own programmatic exit triggers (the `prevent_close` re-entrancy risk).
#[derive(Default)]
struct Quitting(Arc<AtomicBool>);

/// Pure decision for the `CloseRequested` guard: prevent the window close (to show
/// the quit-confirm modal) only when we're not already quitting AND the daemon
/// holds at least one live session. Factored out so it's unit-testable without a
/// real `tauri::WindowEvent`.
fn should_prevent_close(quitting: bool, live_sessions: usize) -> bool {
    !quitting && live_sessions > 0
}

/// Quit the GUI but leave the daemon + its live sessions running. Sets the
/// `quitting` latch first so the `CloseRequested` guard allows the exit through.
#[tauri::command]
fn app_quit_keep(app: AppHandle, quitting: State<Quitting>) {
    quitting.0.store(true, Ordering::SeqCst);
    app.exit(0);
}

/// Stop all daemon sessions, then quit the GUI. Best-effort shutdown (ignore the
/// error — we're exiting regardless). Sets the `quitting` latch first.
#[tauri::command]
fn app_quit_stop(app: AppHandle, client: State<SharedBackend>, quitting: State<Quitting>) {
    quitting.0.store(true, Ordering::SeqCst);
    let _ = client.shutdown();
    app.exit(0);
}

/// Watch the active project's `keitora.toml` and emit `project://changed` (+ the legacy
/// `worktrees://changed` alias) when it changes, so the rail updates live without a
/// restart. Watches the project root dir (to survive editors' replace-on-save) but
/// filters events to the `keitora.toml` family (so frequent `tasks.db` writes don't
/// trigger it). Debounced ~200ms.
///
/// Runs on the shared [`crate::db_watch::watch_dir`] loop (stop channel +
/// restartability): re-callable on a project switch, follows the new project's config
/// file, and never leaks the previous thread — its stop sender lives in
/// [`CONFIG_WATCHER_STOP`], swapped via [`replace_stop`] like the tasks watcher.
///
/// Resolves the root via [`keitora_tasks::resolve_active`] (the GUI entry point that
/// honours the active project), NOT `config_path()` → `resolve()`, which is cwd-based:
/// in a bundled app the process cwd is not inside any project, so `resolve()` would
/// return `NotFound` (no watcher at all), and on a project switch it would resolve the
/// launch-time project rather than the newly-active one. Mirrors `start_tasks_watcher`.
fn start_config_watcher(app: AppHandle) {
    // Stop-then-maybe-start (see `start_tasks_watcher`): supersede the previous
    // watcher first so a call with no active project stops the stale thread.
    let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(0);
    drop(replace_stop(&CONFIG_WATCHER_STOP, stop_tx));

    let dir = match keitora_tasks::resolve_active() {
        Ok(pc) => pc.root, // `keitora.toml` lives at the project root
        Err(_) => return,  // no active project → previous watcher stopped above
    };
    let file = "keitora.toml".to_string();

    thread::spawn(move || {
        crate::db_watch::watch_dir(dir, file, Duration::from_millis(200), stop_rx, move || {
            // Dual-emit: `project://changed` is the new event name; the
            // `worktrees://changed` alias is kept one release for the existing
            // frontend subscriber and removed once it migrates.
            let _ = app.emit("project://changed", ());
            let _ = app.emit("worktrees://changed", ());
        });
    });
}

/// (Re)point the project-scoped filesystem watchers (tasks/sessions + config) at the
/// currently-active project. Called from every project-activation path so live
/// updates follow the active project without an app restart, and from `remove_project`
/// so a removed active project's watchers are torn down. Each underlying call
/// supersedes its previous watcher thread via the stop-sender swap (no leak); with no
/// active project both calls stop the prior watcher and start nothing.
fn restart_project_watchers(app: &AppHandle) {
    start_tasks_watcher(app.clone());
    start_config_watcher(app.clone());
}

/// Payload for `tasks://changed`: a cheap revision marker the frontend dedupes on.
#[derive(Clone, Serialize)]
struct TasksChanged {
    revision: String,
}

/// Watch the task DB directory and emit `tasks://changed` when the DB (or its WAL)
/// is written, so the board updates live without a restart. Mirrors
/// `start_config_watcher` but anchors on `db_path` (not the config file) and
/// filters to the `tasks.db*` family. Debounced ~300ms; carries a revision marker
/// so the frontend can dedupe redundant events.
fn start_tasks_watcher(app: AppHandle) {
    // Stop-then-maybe-start: supersede the previous watcher FIRST (replacing the
    // global stop sender drops the old one, disconnecting the old thread's `stop_rx`
    // so it exits on its next loop). A rendezvous channel (cap 0) is enough — we only
    // care about connect/disconnect, never buffering. Doing this before the resolve
    // means a call with no active project (e.g. after the active project is removed)
    // stops the stale watcher instead of leaking it.
    let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(0);
    drop(replace_stop(&TASKS_WATCHER_STOP, stop_tx));

    let db_path = match keitora_tasks::resolve_active() {
        Ok(pc) => pc.tasks.db_path.clone(),
        Err(_) => return, // no active project → previous watcher stopped above
    };
    let dir = match db_path.parent() {
        Some(p) => p.to_path_buf(),
        None => return,
    };
    // Match tasks.db, tasks.db-wal, tasks.db-shm — any write to the DB family.
    let db_file = match db_path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => return,
    };
    let db_path_str = db_path.to_string_lossy().to_string();

    thread::spawn(move || {
        // Two long-lived handles: `SqliteBackend` for the task revision (task +
        // dispatch log cursors), `LocalStore` for the session revision (state-based,
        // no task backend needed). Both point at the same file (WAL-safe). Neither
        // needs a tokio runtime — `revision_blocking` / `session_revision` are sync.
        let backend = keitora_tasks::SqliteBackend::open(&db_path_str).ok();
        let local = keitora_tasks::LocalStore::open(&db_path_str).ok();
        let mut last_revision = String::new();
        let mut last_session_revision = String::new();
        // Emit `event` only when `rev` differs from the last value we sent.
        let emit_if_changed = |event: &str, rev: String, last: &mut String| {
            if rev != *last {
                *last = rev.clone();
                let _ = app.emit(event, TasksChanged { revision: rev });
            }
        };
        // Shared notify-setup + poll/stop/debounce loop; on each debounced burst:
        // Tasks: log-id+count marker. Sessions: state-based marker (same DB file)
        // so heartbeat-only writes don't spam the UI.
        crate::db_watch::watch_dir(
            dir,
            db_file,
            Duration::from_millis(300),
            stop_rx,
            move || {
                let trev = backend
                    .as_ref()
                    .and_then(|b| b.revision_blocking().ok())
                    .unwrap_or_else(|| "?".to_string());
                emit_if_changed("tasks://changed", trev, &mut last_revision);
                let srev = local
                    .as_ref()
                    .and_then(|l| l.session_revision().ok())
                    .unwrap_or_default();
                emit_if_changed("sessions://changed", srev, &mut last_session_revision);
            },
        );
    });
}

/// Build the PTY backend: [`crate::backend::herdr::HerdrBackend`], or, when it
/// cannot be constructed (herdr unresolvable, user config unreadable), an
/// [`UnavailableBackend`] that reports why, so the app still starts.
fn select_pty_backend(app: AppHandle) -> SharedBackend {
    let herdr =
        crate::backend::herdr::HerdrBackend::new(app.clone()).map(|b| Arc::new(b) as SharedBackend);
    choose_backend(herdr, Arc::new(app))
}

/// The backend to run given the outcome of constructing herdr: herdr itself, or
/// an [`UnavailableBackend`] carrying the error, emitting through `sink`. Pure
/// apart from logging, so the fallback is unit-tested.
fn choose_backend(
    herdr: Result<SharedBackend, PtyBackendError>,
    sink: Arc<dyn crate::backend::EmitSink>,
) -> SharedBackend {
    match herdr {
        Ok(b) => {
            log::info!("pty backend: herdr");
            b
        }
        Err(e) => {
            log::error!("pty backend: herdr unavailable ({e}); sessions are disabled");
            Arc::new(UnavailableBackend::new(sink, e.to_string()))
        }
    }
}

pub fn run() {
    crate::logging::init();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        // Serializes dispatch_apply so concurrent dispatches can't double-inject.
        .manage(crate::dispatch::DispatchGate::default());

    builder
        .setup(|app| {
            // The PTY backend owns all PTY state now: register it as managed state
            // (behind the `PtyBackend` trait) and start its connect-with-backoff +
            // server-push loop (which delivers pane output to the attached panes and
            // emits `daemon://status`). That is `HerdrBackend`; if herdr cannot be
            // constructed, `UnavailableBackend` reports why so the app still starts.
            let backend: SharedBackend = select_pty_backend(app.handle().clone());
            backend.start();
            app.manage(backend);
            // Shared sysinfo System for the rail daemon card's 2s stats poll.
            // Per-worktree activity caches (workflow slots, branch cache, UBT engine).
            app.manage(crate::activity::ActivityState::default());
            // Performance overlay: the sampler's lease, latest snapshot and wake channel.
            app.manage(crate::perf::PerfState::default());
            // Diff view: per-file LRU cache, the per-worktree cancellation registry for
            // in-flight per-file `git diff` jobs, the single-scan summary snapshotter, and
            // the refcounted filesystem watchers that drive change detection.
            app.manage(crate::diff::cache::DiffCache::default());
            app.manage(crate::diff::jobs::DiffJobs::default());
            app.manage(crate::diff::snapshot::Snapshotter::default());
            app.manage(crate::diff::watch::DiffWatchers::default());
            // "Quitting now" latch read by the CloseRequested guard below.
            app.manage(Quitting::default());
            // Point the tasks + config watchers at the launch-time active project.
            restart_project_watchers(app.handle());
            // Background-project notifications: one watcher per
            // registered project DB, fires OS notifications for non-active
            // projects' sessions that need input.
            bg_notifier::start_bg_notifier(app.handle().clone());
            // Off the setup thread: per-worktree file I/O shouldn't delay the window.
            // Idempotent + Claude Code hot-reloads, so timing doesn't matter.
            thread::spawn(hooks::install_session_hooks);
            // Resolve + persist the `claude` path once (runs a login shell), so a
            // LaunchServices-spawned app can find it. Still needed: the agent launch
            // for herdr panes (`agent_launch_with`) takes the persisted path, else
            // falls back to a bare `claude`, and Settings shows it. Off-thread: don't
            // block the window on a shell round-trip.
            thread::spawn(ensure_claude_path);
            Ok(())
        })
        // When the window is closed while the daemon holds live sessions,
        // intercept and let the React quit-confirm modal decide (keep sessions vs
        // stop all). The `quitting` latch (set by app_quit_keep/app_quit_stop)
        // lets our own programmatic `app.exit(0)` close through without re-prompting.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let quitting = app
                    .try_state::<Quitting>()
                    .map(|q| q.0.load(Ordering::SeqCst))
                    .unwrap_or(false);
                let live = app
                    .try_state::<SharedBackend>()
                    .map(|c| c.live_set_snapshot().len())
                    .unwrap_or(0);
                if should_prevent_close(quitting, live) {
                    api.prevent_close();
                    let _ = window.emit("app://close-requested", live);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            pty_spawn,
            pty_attach,
            pty_detach,
            pty_write,
            pty_paste,
            crate::latency_probe::latency_probe_enabled,
            save_clipboard_image,
            crate::terminal_links::open_terminal_file,
            pty_resize,
            pty_scroll,
            pty_scroll_offset,
            pty_read_history,
            pty_repaint,
            pty_kill,
            session_terminate,
            session_launch,
            daemon_status,
            take_xdg_leak_notice,
            daemon_recovery,
            recovery_resume_now,
            recovery_focus,
            recovery_start_fresh,
            daemon_stats,
            daemon_shutdown,
            daemon_restart,
            daemon_list_sessions,
            crate::activity::worktree_activity,
            crate::perf::perf_subscribe,
            crate::perf::perf_unsubscribe,
            crate::perf::perf_snapshot,
            app_quit_keep,
            app_quit_stop,
            worktrees,
            current_project,
            list_projects,
            projects_overview,
            set_active_project,
            reorder_projects,
            reorder_worktrees,
            onboard_project,
            detect_legacy,
            migrate_legacy,
            discover_worktrees,
            set_worktrees,
            worktree_create,
            keitora_cli_status,
            install_keitora_cli,
            get_settings,
            save_project_settings,
            set_worktree_diff_base,
            commit_settings_to_project,
            save_user_settings,
            set_skip_permissions,
            remove_project,
            tasks::task_actor,
            tasks::sessions_list,
            tasks::tasks_revision,
            tasks::board_snapshot,
            tasks::tasks_list,
            tasks::task_get,
            tasks::task_claim,
            tasks::task_done,
            tasks::task_unclaim,
            tasks::task_update,
            tasks::task_new,
            tasks::task_release,
            tasks::task_delete,
            dispatch::dispatch_apply,
            crate::transfer::preview_claude_transfer,
            crate::transfer::transfer_worktree,
            crate::transfer::inject_handoff_summary,
            crate::diff::worktree_diff_summary,
            crate::diff::worktree_diff_committed,
            crate::diff::worktree_file_diff,
            crate::diff::worktree_diff_revision,
            crate::diff::worktree_diff_base,
            crate::diff::worktree_diff_watch_start,
            crate::diff::worktree_diff_watch_stop
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::{requested_size, OutputMsg, PtyBackendError};

    #[test]
    fn requested_size_needs_both_dimensions_at_least_one() {
        assert_eq!(requested_size(Some(100), Some(30)), Some((100, 30)));
        assert_eq!(requested_size(Some(100), None), None);
        assert_eq!(requested_size(None, Some(30)), None);
        assert_eq!(requested_size(None, None), None);
        assert_eq!(requested_size(Some(0), Some(30)), None);
        assert_eq!(requested_size(Some(100), Some(0)), None);
    }

    /// The output channel's wire shape, which the TS `OutputMsg` mirrors.
    #[test]
    fn output_msg_wire_shape() {
        assert_eq!(
            serde_json::to_value(OutputMsg::Frame {
                data: "aGk=".into(),
                full: true
            })
            .unwrap(),
            serde_json::json!({"type": "frame", "data": "aGk=", "full": true})
        );
        assert_eq!(
            serde_json::to_value(OutputMsg::Exit).unwrap(),
            serde_json::json!({"type": "exit"})
        );
    }

    #[test]
    fn attach_err_kind_tokens() {
        let kind = super::attach_err_kind;
        assert_eq!(kind(&PtyBackendError::NotConnected), "NotConnected");
        assert_eq!(kind(&PtyBackendError::NoSuchSession), "NoLiveSession");
        assert_eq!(kind(&PtyBackendError::RecoveryPending), "Recovering");
        assert_eq!(kind(&PtyBackendError::RecoveryPaused), "Recovering");
        assert_eq!(kind(&PtyBackendError::Terminated), "Terminated");
        assert_eq!(
            kind(&PtyBackendError::Other("boom".into())),
            "Internal: boom"
        );
    }

    #[test]
    fn spawn_err_kind_tokens() {
        let kind = super::spawn_err_kind;
        assert_eq!(kind(PtyBackendError::RecoveryPending), "Recovering");
        assert_eq!(kind(PtyBackendError::RecoveryPaused), "Recovering");
        assert_eq!(kind(PtyBackendError::Terminated), "Terminated");
        assert_eq!(kind(PtyBackendError::NotConnected), "herdr unreachable");
        assert_eq!(kind(PtyBackendError::NoSuchSession), "no live session");
        assert_eq!(kind(PtyBackendError::Other("boom".into())), "boom");
    }

    #[test]
    fn paste_text_forwards_the_paste_and_maps_a_refusal() {
        use crate::backend::fake::{Call, FakeBackend};
        use crate::backend::SessionKey;
        let fake = FakeBackend::new();
        assert_eq!(super::paste_text(&fake, "proj", "alpha", "a\rb"), Ok(()));
        assert_eq!(
            fake.calls(),
            vec![Call::Paste {
                key: SessionKey {
                    project: "proj".into(),
                    worktree: "alpha".into(),
                },
                text: "a\rb".into(),
            }]
        );
        fake.push_paste(Err(PtyBackendError::PasteTooLarge {
            bytes: 1_048_577,
            limit: 1_048_576,
        }));
        assert_eq!(
            super::paste_text(&fake, "proj", "alpha", "big"),
            Err("PasteTooLarge: 1048577 > 1048576".to_string())
        );
    }

    mod terminate_and_launch {
        use super::super::{launch_session, terminate_session, worktrees_in, TerminateError};
        use crate::backend::fake::{Call, FakeBackend};
        use crate::backend::{PtyBackend, PtyBackendError, SessionKey, SessionSummary};
        use keitora_tasks::{ProjectRef, WorktreeEntry};
        use std::cell::Cell;

        fn key() -> SessionKey {
            SessionKey {
                project: "proj".into(),
                worktree: "alpha".into(),
            }
        }

        fn summary(worktree: &str, gen: u64) -> SessionSummary {
            SessionSummary {
                key: SessionKey {
                    project: "proj".into(),
                    worktree: worktree.into(),
                },
                gen,
                state_hint: None,
            }
        }

        fn kills(fake: &FakeBackend) -> Vec<u64> {
            fake.calls()
                .into_iter()
                .filter_map(|c| match c {
                    Call::Kill { gen, .. } => Some(gen),
                    _ => None,
                })
                .collect()
        }

        #[test]
        fn terminate_sets_the_flag_before_killing_the_given_generation() {
            let fake = FakeBackend::new();
            assert_eq!(terminate_session(&fake, &key(), Some(4)), Ok(()));
            assert!(fake.is_terminated(&key()));
            let calls = fake.calls();
            assert_eq!(
                calls[0],
                Call::SetTerminated {
                    key: key(),
                    terminated: true
                }
            );
            assert_eq!(kills(&fake), vec![4]);
            assert!(
                !calls.contains(&Call::ListSessions),
                "a given generation needs no lookup"
            );
        }

        #[test]
        fn terminate_without_a_generation_resolves_it_from_the_cached_sessions() {
            let fake = FakeBackend::new();
            fake.push_list_sessions(Ok(vec![summary("beta", 2), summary("alpha", 7)]));
            assert_eq!(terminate_session(&fake, &key(), None), Ok(()));
            assert_eq!(kills(&fake), vec![7]);
        }

        #[test]
        fn terminate_of_a_session_that_is_not_live_skips_the_kill() {
            let fake = FakeBackend::new();
            fake.push_list_sessions(Ok(vec![summary("beta", 2)]));
            assert_eq!(terminate_session(&fake, &key(), None), Ok(()));
            assert!(fake.is_terminated(&key()));
            assert!(kills(&fake).is_empty());
            // The session list itself refused during recovery: no kill either.
            let fake = FakeBackend::new();
            fake.push_list_sessions(Err(PtyBackendError::RecoveryPending));
            assert_eq!(terminate_session(&fake, &key(), None), Ok(()));
            assert!(fake.is_terminated(&key()));
            assert!(kills(&fake).is_empty());
        }

        #[test]
        fn terminate_during_recovery_still_sets_the_flag_and_succeeds() {
            for refusal in [
                PtyBackendError::RecoveryPending,
                PtyBackendError::RecoveryPaused,
            ] {
                let fake = FakeBackend::new();
                fake.push_kill(Err(refusal));
                assert_eq!(terminate_session(&fake, &key(), Some(3)), Ok(()));
                assert!(fake.is_terminated(&key()));
                assert_eq!(kills(&fake), vec![3]);
            }
        }

        #[test]
        fn a_failed_kill_is_reported_with_the_flag_still_set() {
            let fake = FakeBackend::new();
            fake.push_kill(Err(PtyBackendError::Other("pane close failed".into())));
            assert_eq!(
                terminate_session(&fake, &key(), Some(3)),
                Err(TerminateError::Kill("pane close failed".into()))
            );
            assert!(fake.is_terminated(&key()));
        }

        #[test]
        fn a_terminate_that_cannot_persist_kills_nothing() {
            let fake = FakeBackend::new();
            fake.fail_set_terminated();
            assert!(matches!(
                terminate_session(&fake, &key(), Some(3)),
                Err(TerminateError::NotPersisted(_))
            ));
            assert!(kills(&fake).is_empty());
        }

        #[test]
        fn launch_clears_the_flag_and_then_spawns() {
            let fake = FakeBackend::new();
            fake.insert_terminated(key());
            let notified = Cell::new(false);
            let gen = launch_session(
                &fake,
                &key(),
                || notified.set(true),
                || {
                    assert!(notified.get(), "the rail hears of it before the spawn");
                    // The spawn the command makes: refused while still terminated.
                    fake.spawn(key(), "/w".into(), "/db".into(), Some((80, 24)), None)
                        .map_err(|e| e.to_string())
                },
            );
            assert_eq!(gen, Ok(1));
            assert!(!fake.is_terminated(&key()));
            assert_eq!(
                fake.calls()[0],
                Call::SetTerminated {
                    key: key(),
                    terminated: false
                }
            );
        }

        #[test]
        fn a_launch_that_cannot_clear_the_flag_does_not_spawn() {
            let fake = FakeBackend::new();
            fake.insert_terminated(key());
            fake.fail_set_terminated();
            let spawned = Cell::new(false);
            let out = launch_session(
                &fake,
                &key(),
                || {},
                || {
                    spawned.set(true);
                    Ok(1)
                },
            );
            assert!(out.is_err());
            assert!(!spawned.get());
            assert!(fake.is_terminated(&key()));
        }

        #[test]
        fn worktrees_carry_the_terminated_flag() {
            let tmp = tempfile::TempDir::new().unwrap();
            let dir = tmp.path().join("proj-dir");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("keitora.toml"),
                "[project]\nname = \"proj\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n",
            )
            .unwrap();
            let mut pref = ProjectRef::new(dir);
            pref.worktrees = Some(
                ["alpha", "beta"]
                    .iter()
                    .map(|w| WorktreeEntry {
                        name: (*w).into(),
                        path: Some(tmp.path().join(w)),
                        cli: None,
                        codex_thread_id: None,
                        claude_session_id: None,
                        diff_base: None,
                    })
                    .collect(),
            );
            let pc = keitora_tasks::effective_config(&pref).unwrap();

            let fake = FakeBackend::new();
            fake.insert_terminated(key());
            // The same worktree name in another project is not this one.
            fake.insert_terminated(SessionKey {
                project: "other".into(),
                worktree: "beta".into(),
            });
            let views = worktrees_in(&fake, Some(pc)).unwrap();
            let json = serde_json::to_value(&views).unwrap();
            let flags: Vec<(String, bool)> = json
                .as_array()
                .unwrap()
                .iter()
                .map(|v| {
                    (
                        v["name"].as_str().unwrap().to_string(),
                        v["terminated"].as_bool().unwrap(),
                    )
                })
                .collect();
            assert!(flags.contains(&("alpha".to_string(), true)), "{flags:?}");
            assert!(flags.contains(&("beta".to_string(), false)), "{flags:?}");
            // The resolved worktree's own fields stay flat beside the flag.
            assert!(json[0].get("path").is_some() && json[0].get("status").is_some());
        }

        #[test]
        fn worktrees_without_an_active_project_are_empty() {
            let fake = FakeBackend::new();
            fake.insert_terminated(key());
            assert!(worktrees_in(&fake, None).unwrap().is_empty());
        }
    }

    mod recovery_hooks {
        use crate::backend::fake::FakeBackend;
        use crate::backend::herdr::recovery::machine::Event;
        use crate::backend::herdr::recovery::recording_handle;
        use crate::backend::SessionKey;
        use keitora_tasks::{ProjectRef, UserConfig, WorktreeEntry};
        use std::path::Path;

        fn key(p: &str, w: &str) -> SessionKey {
            SessionKey {
                project: p.into(),
                worktree: w.into(),
            }
        }

        fn entry(name: &str, path: &Path) -> WorktreeEntry {
            WorktreeEntry {
                name: name.into(),
                path: Some(path.to_path_buf()),
                cli: None,
                codex_thread_id: None,
                claude_session_id: None,
                diff_base: None,
            }
        }

        /// A project dir whose `keitora.toml` names it `name`, plus `extra`.
        fn project(root: &Path, dir: &str, name: &str, extra: &str) -> std::path::PathBuf {
            let dir = root.join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            let text = format!(
                "[project]\nname = \"{name}\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n{extra}"
            );
            std::fs::write(dir.join("keitora.toml"), text).unwrap();
            dir
        }

        /// A registry of two projects: `alpha` (registered as "first", autonomy
        /// on) with worktree `wa`, and `beta` (registered as "second") with `wb`.
        fn two_projects(root: &Path) -> UserConfig {
            let a = project(root, "a-dir", "alpha", "\n[claude]\nautonomy = true\n");
            let b = project(root, "b-dir", "beta", "");
            let mut user = UserConfig::default();
            let mut ra = ProjectRef::new(a.clone());
            ra.worktrees = Some(vec![entry("wa", &root.join("wt-a"))]);
            let mut rb = ProjectRef::new(b.clone());
            rb.worktrees = Some(vec![entry("wb", &root.join("wt-b"))]);
            user.projects.insert("first".into(), ra);
            user.projects.insert("second".into(), rb);
            user
        }

        #[test]
        fn the_launch_resolver_finds_any_registered_project() {
            let tmp = tempfile::TempDir::new().unwrap();
            let user = two_projects(tmp.path());

            let (cwd, launch) = super::super::launch_for(&user, &key("alpha", "wa")).unwrap();
            assert_eq!(Path::new(&cwd), tmp.path().join("wt-a"));
            assert!(launch.autonomy, "alpha's own settings");
            assert!(!launch.fresh);

            let (cwd, launch) = super::super::launch_for(&user, &key("beta", "wb")).unwrap();
            assert_eq!(Path::new(&cwd), tmp.path().join("wt-b"));
            assert!(!launch.autonomy, "beta's own settings");
        }

        #[test]
        fn the_launch_resolver_keys_by_resolved_name_and_knows_its_limits() {
            let tmp = tempfile::TempDir::new().unwrap();
            let user = two_projects(tmp.path());
            let launch_for = super::super::launch_for;
            assert!(
                launch_for(&user, &key("first", "wa")).is_none(),
                "registry key"
            );
            assert!(
                launch_for(&user, &key("alpha", "nope")).is_none(),
                "unknown worktree"
            );
            assert!(
                launch_for(&user, &key("alpha", "wb")).is_none(),
                "other project's"
            );
            assert!(
                launch_for(&user, &key("gamma", "wa")).is_none(),
                "unknown project"
            );
            // The main worktree is the project root.
            let (cwd, _) = launch_for(&user, &key("alpha", "a-dir")).unwrap();
            assert_eq!(
                std::fs::canonicalize(&cwd).unwrap(),
                std::fs::canonicalize(tmp.path().join("a-dir")).unwrap()
            );
        }

        #[test]
        fn a_removed_project_is_cancelled_by_its_resolved_name() {
            let tmp = tempfile::TempDir::new().unwrap();
            let user = two_projects(tmp.path());
            assert_eq!(super::super::resolved_project_name(&user, "first"), "alpha");
            assert_eq!(
                super::super::resolved_project_name(&user, "unknown"),
                "unknown"
            );
            let (handle, rx) = recording_handle();
            handle.cancel_project(&super::super::resolved_project_name(&user, "second"));
            assert_eq!(
                rx.try_iter().collect::<Vec<_>>(),
                vec![Event::CancelProject("beta".into())]
            );
        }

        #[test]
        fn set_worktrees_cancels_only_the_removed_ones() {
            let tmp = tempfile::TempDir::new().unwrap();
            let (handle, rx) = recording_handle();
            let before = vec!["keep".to_string(), "gone".to_string(), "also".to_string()];
            let after = vec![entry("keep", tmp.path()), entry("new", tmp.path())];
            super::super::cancel_removed_worktrees(Some(&handle), "p", &before, &after);
            assert_eq!(
                rx.try_iter().collect::<Vec<_>>(),
                vec![
                    Event::Cancel(key("p", "gone")),
                    Event::Cancel(key("p", "also"))
                ]
            );
            // Without recovery there is nothing to tell.
            super::super::cancel_removed_worktrees(None, "p", &before, &after);
        }

        #[test]
        fn recovery_commands_degrade_without_recovery() {
            let fake = FakeBackend::new();
            super::super::resume_now_in(&fake);
            super::super::focus_in(&fake, "p".into(), "w".into());
            assert!(!super::super::start_fresh_in(&fake, "p".into(), "w".into()));
        }
    }

    #[test]
    fn worktree_listed_matches_project_and_available_worktree() {
        use keitora_tasks::WorktreeStatus;
        let listed = vec![
            (
                "proj".to_string(),
                "gamma".to_string(),
                WorktreeStatus::Available,
            ),
            (
                "proj".to_string(),
                "gone".to_string(),
                WorktreeStatus::Missing,
            ),
        ];
        let key = |p: &str, w: &str| super::SessionKey {
            project: p.into(),
            worktree: w.into(),
        };
        assert!(super::worktree_listed(&listed, &key("proj", "gamma")));
        assert!(
            !super::worktree_listed(&listed, &key("proj", "gone")),
            "dir removed"
        );
        assert!(
            !super::worktree_listed(&listed, &key("proj", "new")),
            "not configured"
        );
        assert!(
            !super::worktree_listed(&listed, &key("other", "gamma")),
            "other project"
        );
    }

    use super::should_prevent_close;
    use super::{SessionKey, SharedBackend};
    use crate::backend::EmitSink;
    use base64::Engine as _;
    use std::sync::Mutex;

    // --- PTY backend selection ----------------------------------

    /// Capturing [`EmitSink`] for the backend-selection tests.
    #[derive(Default)]
    struct CaptureSink(Mutex<Vec<(String, serde_json::Value)>>);
    impl EmitSink for CaptureSink {
        fn emit(&self, event: &str, payload: serde_json::Value) {
            self.0.lock().unwrap().push((event.to_string(), payload));
        }
    }

    #[test]
    fn choose_backend_keeps_a_constructed_herdr() {
        let herdr: SharedBackend = Arc::new(crate::backend::fake::FakeBackend::new());
        let sink = Arc::new(CaptureSink::default());
        let chosen = super::choose_backend(Ok(Arc::clone(&herdr)), sink.clone());
        assert!(Arc::ptr_eq(&chosen, &herdr));
        assert!(sink.0.lock().unwrap().is_empty());
    }

    #[test]
    fn choose_backend_falls_back_to_unavailable_with_the_error() {
        // A herdr env that cannot be set up and an unreadable user config both
        // reach here as the construction error (a herdr executable that is not
        // found does not: the herdr backend reports that itself).
        for err in [
            "herdr socket path is too long",
            "loading user config: bad toml",
        ] {
            let sink = Arc::new(CaptureSink::default());
            let chosen =
                super::choose_backend(Err(PtyBackendError::Other(err.to_string())), sink.clone());
            let status = chosen.status_payload_snapshot();
            assert_eq!(status.state, "Disconnected");
            assert!(status.reason.as_deref().unwrap().contains(err));
            assert!(!status.restartable, "only a relaunch helps");
            assert!(matches!(
                chosen.spawn(SessionKey { project: "p".into(), worktree: "w".into() }, "/w".into(), "/db".into(), Some((80, 24)), None),
                Err(PtyBackendError::Other(m)) if m == format!("herdr unavailable: {err}")
            ));
            // It is not started yet: the caller starts it, which emits the status.
            assert!(sink.0.lock().unwrap().is_empty());
            chosen.start();
            let events = sink.0.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].0, "daemon://status");
        }
    }

    #[test]
    fn prevents_close_when_live_sessions_and_not_quitting() {
        assert!(should_prevent_close(false, 1));
        assert!(should_prevent_close(false, 5));
    }

    #[test]
    fn allows_close_when_no_live_sessions() {
        assert!(!should_prevent_close(false, 0));
    }

    #[test]
    fn allows_close_when_already_quitting() {
        // The re-entrancy guard: our own programmatic exit must not re-prevent,
        // even with live sessions still present.
        assert!(!should_prevent_close(true, 3));
        assert!(!should_prevent_close(true, 0));
    }

    /// The PTY output contract: we emit STANDARD base64 (the alphabet + padding
    /// the frontend's `atob` decodes). Guards against an accidental url-safe swap.
    #[test]
    fn base64_standard_round_trips_arbitrary_bytes() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let enc = base64::engine::general_purpose::STANDARD.encode(&data);
        let dec = base64::engine::general_purpose::STANDARD
            .decode(&enc)
            .unwrap();
        assert_eq!(dec, data);
        assert_eq!(
            enc.len() % 4,
            0,
            "standard base64 is padded to a multiple of 4"
        );
    }

    /// `project_summary` maps a `ProjectConfig` onto the frontend struct: name from
    /// `project.name`, paths stringified, worktrees carried over, `active` from the
    /// caller. Guards the field paths against the hierarchical-config rename.
    #[test]
    fn project_summary_maps_fields() {
        use keitora_tasks::{
            DispatchConfig, ProjectConfig, ProjectMeta, ProviderConfig, TaskConfig, WorktreeEntry,
        };
        use std::path::PathBuf;

        let pc = ProjectConfig {
            root: PathBuf::from("/Users/x/code/keitora"),
            effective_worktrees: None,
            project: ProjectMeta {
                name: "keitora".into(),
                worktrees_dir: Some("../keitora-worktrees".into()),
                base_branch: None,
                diff_base: None,
            },
            tasks: TaskConfig {
                db_path: PathBuf::from("/Users/x/code/keitora-worktrees/shared/tasks.db"),
                worktrees: vec![
                    WorktreeEntry::name_only("keitora"),
                    WorktreeEntry::name_only("sayo"),
                ],
                archive_days: 30,
                log_retention_days: 90,
                backend: "sqlite".to_string(),
                github: None,
            },
            claude: ProviderConfig::default(),
            codex: ProviderConfig::default(),
            cli: keitora_tasks::CliConfig::default(),
            dispatch: DispatchConfig::default(),
        };

        let s = super::project_summary(&pc, true);
        assert_eq!(s.name, "keitora");
        assert_eq!(s.resolved_name, "keitora");
        assert_eq!(s.root, "/Users/x/code/keitora");
        assert_eq!(s.db_path, "/Users/x/code/keitora-worktrees/shared/tasks.db");
        assert_eq!(s.worktrees, vec!["keitora".to_string(), "sayo".to_string()]);
        assert!(s.active);

        let inactive = super::project_summary(&pc, false);
        assert!(!inactive.active);
    }

    /// A file-less project: `effective_config` over a `ProjectRef` pointing
    /// at a dir with no `keitora.toml` yields scaffold defaults, and `project_summary`
    /// maps them — name = basename, an absolute db_path, the main worktree listed.
    #[test]
    fn project_summary_from_fileless_effective_config() {
        use keitora_tasks::{effective_config, ProjectRef};

        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("acme");
        std::fs::create_dir_all(&root).unwrap();
        assert!(!root.join("keitora.toml").exists());

        let pc = effective_config(&ProjectRef::new(root.clone())).unwrap();
        let s = super::project_summary(&pc, true);
        assert_eq!(s.name, "acme");
        assert!(
            std::path::Path::new(&s.db_path).is_absolute(),
            "db_path must be absolute: {}",
            s.db_path
        );
        assert_eq!(s.worktrees, vec!["acme".to_string()]);
        assert!(s.active);
    }

    /// `project_summaries` follows the registry's display order, reports the
    /// registry key as `name` while keeping the resolved name, and skips entries
    /// whose committed config is broken.
    #[test]
    fn project_summary_list_follows_order_and_keeps_resolved_name() {
        use keitora_tasks::{ProjectRef, UserConfig};

        let tmp = tempfile::TempDir::new().unwrap();
        let dir = |name: &str| {
            let p = tmp.path().join(name);
            std::fs::create_dir_all(&p).unwrap();
            p
        };
        let broken = dir("broken");
        std::fs::write(broken.join("keitora.toml"), "not = [valid toml").unwrap();

        let mut user = UserConfig::default();
        user.projects
            .insert("alpha".into(), ProjectRef::new(dir("alpha-dir")));
        user.projects
            .insert("beta".into(), ProjectRef::new(dir("beta-dir")));
        user.projects.insert("bad".into(), ProjectRef::new(broken));
        user.set_project_order(vec!["beta".into(), "bad".into(), "alpha".into()]);
        user.set_active("alpha");

        let s = super::project_summaries(&user);
        let keys: Vec<&str> = s.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(keys, ["beta", "alpha"]);
        assert_eq!(s[0].resolved_name, "beta-dir");
        assert_eq!(s[1].resolved_name, "alpha-dir");
        assert!(!s[0].active);
        assert!(s[1].active);
    }

    /// A file-less project rooted at `<tmp>/<dir>` whose task DB lives at
    /// `<tmp>/<dir>.db` (kept inside the temp dir, never under the real home).
    fn overview_project(tmp: &std::path::Path, dir: &str) -> keitora_tasks::ProjectRef {
        let root = tmp.join(dir);
        std::fs::create_dir_all(&root).unwrap();
        let mut pref = keitora_tasks::ProjectRef::new(root);
        pref.db_path = Some(tmp.join(format!("{dir}.db")));
        pref
    }

    /// Key and resolved name are reported separately, worktrees carry the
    /// backend's terminated flag and resolve against the given registry, and a
    /// project with no task DB yields no sessions without creating the DB file.
    #[test]
    fn project_overview_without_db_lists_worktrees_and_creates_nothing() {
        use crate::backend::fake::FakeBackend;
        use crate::backend::SessionKey;
        use std::collections::HashSet;

        let tmp = tempfile::TempDir::new().unwrap();
        let pref = overview_project(tmp.path(), "acme");
        let pc = keitora_tasks::effective_config(&pref).unwrap();
        let db = pc.tasks.db_path.clone();
        assert!(!db.exists());

        let fake = FakeBackend::new();
        fake.insert_terminated(SessionKey {
            project: "acme".into(),
            worktree: "acme".into(),
        });
        // The worktrees resolve against the registry passed in (its `default_cli`),
        // not one re-read from disk.
        let user = keitora_tasks::UserConfig {
            default_cli: Some(keitora_tasks::CliKind::Codex),
            ..keitora_tasks::UserConfig::default()
        };
        let o = super::project_overview(
            &fake,
            &user,
            "registry-key",
            pc,
            &HashSet::new(),
            &HashSet::new(),
        )
        .unwrap();

        assert_eq!(o.key, "registry-key");
        assert_eq!(o.resolved_name, "acme");
        assert_eq!(o.worktrees.len(), 1);
        assert_eq!(o.worktrees[0].inner.name, "acme");
        assert!(o.worktrees[0].terminated);
        assert_eq!(o.worktrees[0].inner.cli, keitora_tasks::CliKind::Codex);
        assert!(o.sessions.is_empty());
        assert!(!db.exists(), "listing must not create the task DB");
    }

    /// With a task DB, sessions are scoped to the project's configured
    /// worktrees, reconciled against the live/background sets under the resolved
    /// name, and stamped with it.
    #[test]
    fn project_overview_with_db_scopes_and_stamps_sessions() {
        use crate::backend::fake::FakeBackend;
        use crate::backend::SessionKey;
        use std::collections::HashSet;

        let tmp = tempfile::TempDir::new().unwrap();
        let pref = overview_project(tmp.path(), "acme");
        let pc = keitora_tasks::effective_config(&pref).unwrap();
        {
            let store =
                keitora_tasks::LocalStore::open(&pc.tasks.db_path.to_string_lossy()).unwrap();
            store.session_upsert("acme", "idle", None, None).unwrap();
            store
                .session_upsert("elsewhere", "busy", None, None)
                .unwrap();
        }
        let key = SessionKey {
            project: "acme".into(),
            worktree: "acme".into(),
        };
        let live: HashSet<_> = [key.clone()].into();
        let bg: HashSet<_> = [key].into();

        let o = super::project_overview(
            &FakeBackend::new(),
            &keitora_tasks::UserConfig::default(),
            "registry-key",
            pc,
            &live,
            &bg,
        )
        .unwrap();
        assert_eq!(o.sessions.len(), 1, "only configured worktrees");
        let s = &o.sessions[0];
        assert_eq!(s.worktree, "acme");
        assert_eq!(s.project, "acme");
        assert_eq!(s.state, "busy", "idle + background work reconciles to busy");
        assert!(!o.worktrees[0].terminated);
        assert!(o.active_tasks.is_empty());
    }

    /// Every project's overview carries its worktrees' active tasks, not just the
    /// active project's.
    #[test]
    fn project_overview_carries_active_tasks() {
        use crate::backend::fake::FakeBackend;
        use keitora_tasks::TaskBackend;
        use std::collections::HashSet;

        let tmp = tempfile::TempDir::new().unwrap();
        let pref = overview_project(tmp.path(), "acme");
        let pc = keitora_tasks::effective_config(&pref).unwrap();
        let db = pc.tasks.db_path.to_string_lossy().to_string();
        let id = tauri::async_runtime::block_on(async {
            let b = keitora_tasks::SqliteBackend::open(&db).unwrap();
            let t = b
                .create("acme", "ship it", "", "normal", &[], None)
                .await
                .unwrap();
            b.claim(t.id, "acme").await.unwrap();
            t.id
        });

        let o = super::project_overview(
            &FakeBackend::new(),
            &keitora_tasks::UserConfig::default(),
            "registry-key",
            pc,
            &HashSet::new(),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(
            o.active_tasks,
            vec![keitora_tasks::ActiveTask {
                worktree: "acme".into(),
                id,
                title: "ship it".into()
            }]
        );
    }

    /// A task DB that can't be read (here: not SQLite at all) costs the project
    /// its session states, not its place in the overview.
    #[test]
    fn project_overview_with_unreadable_db_keeps_worktrees_without_sessions() {
        use crate::backend::fake::FakeBackend;
        use std::collections::HashSet;

        let tmp = tempfile::TempDir::new().unwrap();
        let pref = overview_project(tmp.path(), "acme");
        let pc = keitora_tasks::effective_config(&pref).unwrap();
        std::fs::write(&pc.tasks.db_path, b"definitely not a sqlite database file").unwrap();

        let o = super::project_overview(
            &FakeBackend::new(),
            &keitora_tasks::UserConfig::default(),
            "registry-key",
            pc,
            &HashSet::new(),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(o.worktrees.len(), 1);
        assert!(o.sessions.is_empty());
    }

    /// The overview loop follows display order and skips a project whose
    /// committed config is broken instead of failing the whole list.
    #[test]
    fn project_overviews_follow_order_and_skip_broken_projects() {
        use crate::backend::fake::FakeBackend;
        use keitora_tasks::UserConfig;
        use std::collections::HashSet;

        let tmp = tempfile::TempDir::new().unwrap();
        let broken = overview_project(tmp.path(), "broken");
        std::fs::write(broken.path.join("keitora.toml"), "not = [valid toml").unwrap();

        let mut user = UserConfig::default();
        user.projects
            .insert("alpha".into(), overview_project(tmp.path(), "alpha-dir"));
        user.projects
            .insert("beta".into(), overview_project(tmp.path(), "beta-dir"));
        user.projects.insert("bad".into(), broken);
        user.set_project_order(vec!["beta".into(), "bad".into(), "alpha".into()]);

        let all =
            super::project_overviews(&FakeBackend::new(), &user, &HashSet::new(), &HashSet::new());
        let keys: Vec<(&str, &str)> = all
            .iter()
            .map(|o| (o.key.as_str(), o.resolved_name.as_str()))
            .collect();
        assert_eq!(keys, [("beta", "beta-dir"), ("alpha", "alpha-dir")]);
    }

    /// `build_settings_patch` maps the drawer form → registry patch:
    /// `db_path` untouched; populated values set; empty `worktrees_dir` clears
    /// (`Some(None)` → default); empty `dispatch_steps`/`codex_dispatch_steps` is an
    /// EXPLICIT empty override (`Some(Some([]))`, not a clear) so the user's deletion
    /// sticks; sub-1 day counts rejected before any write. `codex_dispatch_steps`
    /// mirrors `dispatch_steps` exactly — verified with independent values so
    /// the two can't be accidentally swapped.
    #[test]
    fn build_settings_patch_maps_form_and_validates() {
        // Populated form.
        let p = super::build_settings_patch(
            14,
            45,
            Some("  ~/wt  ".to_string()),
            &["/clear".to_string(), "  ".to_string(), "/park".to_string()],
            &["/codex-clear".to_string(), "  ".to_string()],
            Some(keitora_tasks::DiffBase::Upstream),
        )
        .unwrap();
        assert!(p.db_path.is_none(), "db_path is not drawer-editable");
        assert_eq!(p.worktrees_dir, Some(Some("~/wt".to_string())), "trimmed");
        assert_eq!(p.archive_days, Some(Some(14)));
        assert_eq!(p.log_retention_days, Some(Some(45)));
        assert_eq!(
            p.diff_base,
            Some(Some(keitora_tasks::DiffBase::Upstream)),
            "project diff base is set explicitly from the form"
        );
        assert_eq!(
            p.dispatch_steps,
            Some(Some(vec!["/clear".to_string(), "/park".to_string()])),
            "whitespace-only step dropped"
        );
        assert_eq!(
            p.codex_dispatch_steps,
            Some(Some(vec!["/codex-clear".to_string()])),
            "codex steps cleaned independently of claude steps"
        );

        // Empty worktrees_dir → clear (Some(None) → default). Empty dispatch steps →
        // EXPLICIT empty override (Some(Some([]))), so clearing the editor actually
        // clears dispatch instead of reverting to committed steps. Applies to both
        // CLIs independently.
        let p =
            super::build_settings_patch(30, 90, Some("   ".to_string()), &[], &[], None).unwrap();
        assert_eq!(p.worktrees_dir, Some(None));
        assert_eq!(p.dispatch_steps, Some(Some(vec![])));
        assert_eq!(p.codex_dispatch_steps, Some(Some(vec![])));
        assert_eq!(p.diff_base, Some(None), "None form value clears to Auto");

        // Day floors rejected before any write.
        assert!(super::build_settings_patch(0, 90, None, &[], &[], None).is_err());
        assert!(super::build_settings_patch(30, 0, None, &[], &[], None).is_err());
    }

    // ── Watcher stop/replace logic ────────────────────────────────────────────
    //
    // `start_tasks_watcher` / `start_config_watcher` spawn a real `notify` watcher +
    // thread and resolve the active project from disk, so they can't be unit-tested
    // directly. The stop/replace mechanism they rely on is pulled into `replace_stop`
    // (a pure swap) and the stop-aware loop's exit-on-drop contract — both tested
    // here without touching `notify` or the filesystem.

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    /// The core stop primitive: a thread polling `try_recv()` on a rendezvous
    /// channel must observe `Disconnected` once the sender is dropped, and exit.
    /// This is exactly what the watcher loop's stop check (`stop_rx.try_recv()`)
    /// depends on — verified here in isolation.
    #[test]
    fn watcher_stop_sender_exits_on_drop() {
        let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(0);
        let saw_disconnect = Arc::new(AtomicBool::new(false));
        let flag = saw_disconnect.clone();

        let handle = std::thread::spawn(move || loop {
            match stop_rx.try_recv() {
                Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                    flag.store(true, Ordering::SeqCst);
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        });

        // Drop the sender → the polling thread must see Disconnected and exit.
        drop(stop_tx);

        // Join within a bound; if the thread leaked this would hang the test.
        let start = std::time::Instant::now();
        while !handle.is_finished() {
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "watcher stop thread did not exit within 1s after sender drop",
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.join().unwrap();
        assert!(
            saw_disconnect.load(Ordering::SeqCst),
            "thread should have observed the sender disconnect",
        );
    }

    /// `replace_stop` installs the new sender and returns the previous one so the
    /// caller can drop it (stopping the old thread). After replacement the slot holds
    /// a *live* (connected) sender, and the returned previous sender, once dropped,
    /// disconnects its receiver. This is the exact handoff `start_tasks_watcher` /
    /// `start_config_watcher` perform on every restart, and is what bounds live
    /// threads to ≤ 1 superseded watcher per switch (no leak). Parameterized over the
    /// slot so both watchers' restart contracts are covered by one helper.
    fn assert_replace_stop_swaps(slot: &super::Mutex<Option<mpsc::SyncSender<()>>>) {
        // Start from a known-clean slot (no live watcher runs in unit tests, but
        // be defensive against any prior state so the test is order-independent).
        slot.lock().unwrap().take();

        // First install: slot was just cleared, so no previous sender.
        let (tx1, rx1) = mpsc::sync_channel::<()>(0);
        let prev0 = super::replace_stop(slot, tx1);
        assert!(
            prev0.is_none(),
            "install after clear should report no prior sender"
        );

        // Slot now holds a live sender.
        assert!(
            slot.lock().unwrap().is_some(),
            "slot should hold the live stop sender after install",
        );
        // rx1 is still connected (its sender lives in the slot).
        assert_eq!(rx1.try_recv(), Err(mpsc::TryRecvError::Empty));

        // Second install (a restart) replaces the sender and returns the old one.
        let (tx2, _rx2) = mpsc::sync_channel::<()>(0);
        let prev1 = super::replace_stop(slot, tx2);
        assert!(prev1.is_some(), "restart must return the previous sender");

        // Dropping the previous sender disconnects rx1 → the old thread would exit.
        drop(prev1);
        assert_eq!(
            rx1.try_recv(),
            Err(mpsc::TryRecvError::Disconnected),
            "old watcher's receiver must disconnect once the previous sender is dropped",
        );

        // Clean up the slot so this test leaves no live sender behind.
        slot.lock().unwrap().take();
    }

    #[test]
    fn tasks_watcher_stop_sender_replaced_on_restart() {
        assert_replace_stop_swaps(&super::TASKS_WATCHER_STOP);
    }

    /// `start_config_watcher` is restartable the same way as the tasks watcher (so a
    /// project switch re-points it at the new `keitora.toml` without leaking the prior
    /// thread). The restart handoff goes through the same `replace_stop` swap, here
    /// against the config slot.
    #[test]
    fn config_watcher_stop_sender_replaced_on_restart() {
        assert_replace_stop_swaps(&super::CONFIG_WATCHER_STOP);
    }
}
