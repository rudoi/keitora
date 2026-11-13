//! Keitora CLI (`keitora`) — a thin command layer over the `keitora-tasks` library.
//! Task tracking (`keitora task …`), session state (`keitora session …`), and project
//! configuration (`keitora config …`) from the terminal.

use std::error::Error;

use clap::{Parser, Subcommand};
use keitora_tasks::{
    current_worktree, find_legacy_config, migrate_legacy, resolve, BackendError, ClaimOutcome,
    Config, DispatchLogEntry, DispatchRequest, ListFilter, LocalStore, ProjectConfig, ResolveError,
    Task, TaskBackend, TaskUpdate, UserConfig, WorktreeSession, PRIORITIES, STATUSES,
};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "keitora",
    about = "Keitora — task tracking, sessions, and config",
    version
)]
struct Cli {
    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Cross-worktree task tracking (list, claim, done, …)
    #[command(subcommand)]
    Task(TaskCmd),
    /// Record this worktree's Claude session state (used by Claude Code hooks)
    Session {
        /// One of: idle | busy | waiting | offline
        state: String,
    },
    /// Manage project configuration
    #[command(subcommand)]
    Config(ConfigCmd),
}

#[derive(Subcommand)]
enum TaskCmd {
    /// List tasks (open + active by default)
    List {
        /// Include archived (old done/cancelled) tasks
        #[arg(long)]
        all: bool,
        /// Only tasks assigned to this worktree
        #[arg(long)]
        mine: bool,
        /// Include backlog-status tasks
        #[arg(long)]
        backlog: bool,
    },
    /// Create a task
    New {
        title: String,
        #[arg(short = 'd', long = "desc")]
        desc: Option<String>,
        #[arg(short = 'p', long = "priority", default_value = "normal")]
        priority: String,
        /// Comma-separated tags
        #[arg(short = 't', long = "tags")]
        tags: Option<String>,
    },
    /// Show task details + activity log
    Show { id: i64 },
    /// Claim a task for this worktree
    Claim { id: i64 },
    /// Release a claim
    Unclaim { id: i64 },
    /// Mark a task done: a human's step, after review. An agent finishes with `review`
    Done {
        id: i64,
        /// Mark it done from inside an agent session anyway
        #[arg(long)]
        force: bool,
    },
    /// Hand a finished task over for review; it stays in this worktree's lane until a
    /// human marks it done. Records the current git branch if the task has none
    Review {
        id: i64,
        /// The pull request with the work, shown on the board card
        #[arg(long = "pr")]
        pr: Option<String>,
    },
    /// Mark a task cancelled
    Cancel { id: i64 },
    /// Punt a task to the backlog (hidden from the board and from dispatch)
    Backlog { id: i64 },
    /// Bring a backlog, done or cancelled task back to open, unassigned
    #[command(visible_alias = "unbacklog")]
    Reopen { id: i64 },
    /// Update task fields
    Update {
        id: i64,
        #[arg(short = 't', long = "title")]
        title: Option<String>,
        #[arg(short = 'd', long = "desc")]
        desc: Option<String>,
        #[arg(short = 'p', long = "priority")]
        priority: Option<String>,
        #[arg(short = 's', long = "status")]
        status: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long = "pr")]
        pr: Option<String>,
    },
    /// Search tasks by title/description
    Find { term: String },
    /// Kanban board grouped by worktree
    Board {
        #[arg(long)]
        backlog: bool,
    },
    /// Add a dependency: <task_id> is blocked by <depends_on>
    Block { task_id: i64, depends_on: i64 },
    /// Remove a dependency
    Unblock { task_id: i64, depends_on: i64 },
    /// Show the dependency tree
    Deps { id: i64 },
    /// Permanently delete a task.
    #[command(visible_alias = "rm")]
    Delete { id: i64 },
    /// Ask the running Keitora app to dispatch a task, as a board drop would: the task
    /// is assigned to the worktree's lane at once and sent when that session is idle
    /// and the card is at the front of the lane (until then it is pending there).
    /// Without --worktree the app picks one from the dispatch pool: an idle session
    /// first, else the shortest queue. Needs the app running, with this project
    /// registered in it.
    Dispatch {
        id: i64,
        /// The worktree to dispatch onto (default: auto-pick from the dispatch pool)
        #[arg(long)]
        worktree: Option<String>,
        /// Wait up to this many seconds for the app's outcome (0: don't wait)
        #[arg(long, default_value_t = 0)]
        wait: u64,
        /// After the prompt is sent, keep waiting (within --wait) until the worktree's
        /// agent claims the task; exit non-zero if it doesn't
        #[arg(long)]
        until_claimed: bool,
    },
    /// Stream session-state changes, dispatch-log entries and dispatch requests
    /// (one line per change; JSON lines with --json) until interrupted
    Watch {
        /// Only this worktree's changes
        #[arg(long)]
        worktree: Option<String>,
        /// Poll interval in milliseconds
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Scaffold a new keitora.toml in the current project root
    Init,
    /// Migrate a legacy task-config.json to keitora.toml
    Migrate,
    /// List registered projects and show which is active
    Projects,
    /// Set the active project by name
    Use {
        /// The name of a registered project (as shown by `config projects`)
        name: String,
    },
}

// ---- ANSI helpers ----
fn pri_marker(p: &str) -> &'static str {
    match p {
        "urgent" => "\x1b[31m■\x1b[0m",
        "high" => "\x1b[33m■\x1b[0m",
        "normal" => "\x1b[36m■\x1b[0m",
        "low" => "\x1b[90m■\x1b[0m",
        _ => "■",
    }
}
fn status_label(s: &str) -> String {
    let c = match s {
        "open" => "\x1b[37mopen\x1b[0m",
        "active" => "\x1b[32mactive\x1b[0m",
        "review" => "\x1b[33mreview\x1b[0m",
        "backlog" => "\x1b[35mbacklog\x1b[0m",
        "done" => "\x1b[90mdone\x1b[0m",
        "cancelled" => "\x1b[90mcancelled\x1b[0m",
        other => other,
    };
    c.to_string()
}

async fn print_task_row(backend: &dyn TaskBackend, t: &Task) -> Result<(), Box<dyn Error>> {
    let blocked = backend.blocked_by(t.id).await?;
    let blocked_str = if blocked.is_empty() {
        String::new()
    } else {
        let ids = blocked
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!(" \x1b[31m[blocked by {ids}]\x1b[0m")
    };
    let assignee = t
        .assigned_to
        .as_deref()
        .map(|a| format!("\x1b[35m@{a}\x1b[0m"))
        .unwrap_or_default();
    let tags = if t.tags.is_empty() {
        String::new()
    } else {
        format!(" \x1b[90m[{}]\x1b[0m", t.tags.join(","))
    };
    println!(
        "  {} #{}  {}  {}  {}{}{}",
        pri_marker(&t.priority),
        t.id,
        status_label(&t.status),
        t.title,
        assignee,
        tags,
        blocked_str
    );
    Ok(())
}

fn validate(value: &str, allowed: &[&str], label: &str) -> Result<(), Box<dyn Error>> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "invalid {label} \"{value}\" — must be one of: {}",
            allowed.join(", ")
        )
        .into())
    }
}

/// The actor identity used for CLI-originated writes: the current git worktree name.
/// Matches the behaviour of `Store::open` which baked in `current_worktree()` at
/// construction time.
fn cli_actor() -> String {
    current_worktree()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    // Config subcommands do not need the store; short-circuit before resolve().
    if let Cmd::Config(ref sub) = cli.cmd {
        return run_config(sub, cli.json);
    }

    let config = resolve()
        .map(Config::from_project)
        .map_err(|e| -> Box<dyn Error> {
            match e {
                ResolveError::NotFound | ResolveError::NeedsOnboarding => {
                    "no keitora.toml found — run `keitora config init` to create one, \
                     or `keitora config migrate` if you have a legacy task-config.json"
                        .into()
                }
                ResolveError::Io(msg) => msg.into(),
            }
        })?;

    // Task/dispatch commands use the async TaskBackend.
    // Session commands use the synchronous LocalStore.
    let backend = keitora_tasks::open_backend(config.project_config())?;
    let local = LocalStore::open(&config.db_path)?;

    match cli.cmd {
        Cmd::Config(_) => unreachable!("handled above"),
        Cmd::Session { state } => {
            // The hook entry point — runs on every turn, so keep it cheap: one
            // targeted query for the active task id, no full list scan.
            let me = cli_actor();
            let current = local.active_task_id(&me).ok().flatten();
            local.session_upsert(&me, &state, current, None)?;
            emit_ok(cli.json, &format!("  {me} session → {state}"))?;
        }
        Cmd::Task(task) => run_task(task, backend.as_ref(), &local, &config, cli.json).await?,
    }

    Ok(())
}

/// Dispatch the `keitora task …` subcommands. Split from `run` so the top-level
/// `Cmd` stays a flat task/session/config tree.
async fn run_task(
    task: TaskCmd,
    backend: &dyn TaskBackend,
    local: &LocalStore,
    config: &Config,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let me = cli_actor();

    match task {
        TaskCmd::List { all, mine, backlog } => {
            let filter = ListFilter {
                include_archived: all,
                include_backlog: backlog,
                assigned_to: if mine { Some(me.to_string()) } else { None },
                archive_days: config.archive_days,
            };
            let tasks = backend.list(&filter).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else if tasks.is_empty() {
                println!("  No tasks found.");
            } else {
                for t in &tasks {
                    print_task_row(backend, &t.task).await?;
                }
            }
        }

        TaskCmd::New {
            title,
            desc,
            priority,
            tags,
        } => {
            validate(&priority, &PRIORITIES, "priority")?;
            let tag_vec: Vec<String> = tags
                .map(|s| {
                    s.split(',')
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let task = backend
                .create(
                    &me,
                    &title,
                    desc.as_deref().unwrap_or(""),
                    &priority,
                    &tag_vec,
                    None,
                )
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else {
                println!("  Created task #{}: {}", task.id, task.title);
            }
        }

        TaskCmd::Show { id } => {
            let twd = backend
                .get(id)
                .await?
                .ok_or_else(|| format!("task #{id} not found"))?;
            if json {
                let logs = backend.logs(id, 20).await?;
                let mut v = serde_json::to_value(&twd)?;
                v.as_object_mut()
                    .unwrap()
                    .insert("logs".into(), serde_json::to_value(logs)?);
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                let t = &twd.task;
                println!("\n  #{}  {}", t.id, t.title);
                println!(
                    "  Status: {}  Priority: {}",
                    status_label(&t.status),
                    t.priority
                );
                if let Some(a) = &t.assigned_to {
                    println!("  Assigned: @{a}");
                }
                if !t.description.is_empty() {
                    println!("  Description: {}", t.description);
                }
                if !t.tags.is_empty() {
                    println!("  Tags: {}", t.tags.join(", "));
                }
                if let Some(b) = &t.branch {
                    println!("  Branch: {b}");
                }
                if let Some(p) = &t.pr_url {
                    println!("  PR: {p}");
                }
                if !twd.blocked_by.is_empty() {
                    let ids = twd
                        .blocked_by
                        .iter()
                        .map(|b| format!("#{b}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("  \x1b[31mBlocked by: {ids}\x1b[0m");
                }
                if !twd.blocks.is_empty() {
                    let ids = twd
                        .blocks
                        .iter()
                        .map(|b| format!("#{b}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("  Blocks: {ids}");
                }
                println!("  Created: {}  Updated: {}", t.created_at, t.updated_at);
                let logs = backend.logs(id, 20).await?;
                if !logs.is_empty() {
                    println!("\n  Activity:");
                    for l in logs {
                        println!(
                            "    {}  @{}  {}  {}",
                            l.created_at, l.worktree, l.action, l.detail
                        );
                    }
                }
                println!();
            }
        }

        TaskCmd::Claim { id } => {
            let (task, msg) = match backend.claim(id, &me).await? {
                ClaimOutcome::Claimed(t) => (t, format!("  Claimed task #{id} for @{me}")),
                ClaimOutcome::AlreadyYours(t) => {
                    (t, format!("  Task #{id} is already yours (@{me})"))
                }
                ClaimOutcome::ClaimedUnverified(t) => (
                    t,
                    format!(
                        "  Claim submitted for #{id} — verify it stuck (eventually-consistent backend)"
                    ),
                ),
            };
            emit_task_msg(json, &task, &msg)?;
        }
        TaskCmd::Unclaim { id } => {
            backend.unclaim(id, &me).await?;
            emit_ok(json, &format!("  Released task #{id}"))?;
        }
        TaskCmd::Done { id, force } => {
            if !force && keitora_tasks::in_agent_session(|k| std::env::var(k).ok()) {
                return Err(agent_done_refusal(id).into());
            }
            let outcome = backend.set_done(id, &me).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "task": outcome.task, "unblocked": outcome.unblocked })
                    )?
                );
            } else {
                println!("  Completed task #{id}");
                if !outcome.unblocked.is_empty() {
                    let ids = outcome
                        .unblocked
                        .iter()
                        .map(|u| format!("#{u}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("  Unblocked: {ids}");
                }
            }
        }
        TaskCmd::Review { id, pr } => {
            let mut task = backend.set_review(id, &me).await?;
            let upd = review_update(task.branch.as_deref(), current_branch().as_deref(), pr);
            if !upd.is_empty() {
                backend.update(id, &me, &upd).await?;
                task = backend.require(id).await?;
            }
            emit_task_msg(
                json,
                &task,
                &format!("  Task #{id} is ready for review; a human marks it done"),
            )?;
        }
        TaskCmd::Cancel { id } => {
            backend.cancel(id, &me).await?;
            emit_ok(json, &format!("  Cancelled task #{id}"))?;
        }
        TaskCmd::Backlog { id } => {
            backend.backlog(id, &me).await?;
            emit_ok(json, &format!("  Moved task #{id} to backlog"))?;
        }
        TaskCmd::Reopen { id } => {
            let task = backend.reopen(id, &me).await?;
            emit_task_msg(json, &task, &format!("  Reopened task #{id}"))?;
        }

        TaskCmd::Update {
            id,
            title,
            desc,
            priority,
            status,
            branch,
            pr,
        } => {
            if let Some(p) = &priority {
                validate(p, &PRIORITIES, "priority")?;
            }
            if let Some(s) = &status {
                validate(s, &STATUSES, "status")?;
            }
            let upd = TaskUpdate {
                title,
                description: desc,
                priority,
                status,
                branch,
                pr_url: pr,
            };
            backend.update(id, &me, &upd).await?;
            emit_ok(json, &format!("  Updated task #{id}"))?;
        }

        TaskCmd::Find { term } => {
            let tasks = backend.find(&term, config.archive_days).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else if tasks.is_empty() {
                println!("  No tasks matching \"{term}\"");
            } else {
                for t in &tasks {
                    print_task_row(backend, t).await?;
                }
            }
        }

        TaskCmd::Board { backlog } => {
            let filter = ListFilter {
                include_archived: false,
                include_backlog: backlog,
                assigned_to: None,
                archive_days: config.archive_days,
            };
            let tasks = backend.list(&filter).await?;

            // Columns: open, each worktree, [backlog], done.
            let mut order: Vec<String> = vec!["open".into()];
            order.extend(config.worktrees.iter().cloned());
            if backlog {
                order.push("backlog".into());
            }
            order.push("done".into());

            let mut columns: std::collections::BTreeMap<String, Vec<Task>> =
                std::collections::BTreeMap::new();
            for col in &order {
                columns.insert(col.clone(), Vec::new());
            }
            for twd in tasks {
                let t = twd.task;
                let col = if t.status == "done" || t.status == "cancelled" {
                    "done".to_string()
                } else if t.status == "backlog" {
                    if backlog {
                        "backlog".to_string()
                    } else {
                        continue;
                    }
                } else if let Some(a) = &t.assigned_to {
                    if columns.contains_key(a) {
                        a.clone()
                    } else {
                        "open".to_string()
                    }
                } else {
                    "open".to_string()
                };
                columns.get_mut(&col).unwrap().push(t);
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&columns)?);
            } else {
                for col in &order {
                    let list = &columns[col];
                    if col == "done" && list.is_empty() {
                        continue;
                    }
                    println!("\n  \x1b[1m{}\x1b[0m ({})", col.to_uppercase(), list.len());
                    for t in list {
                        print_task_row(backend, t).await?;
                    }
                }
                println!();
            }
        }

        TaskCmd::Block {
            task_id,
            depends_on,
        } => {
            // The backend returns `Conflict("dependency already exists")` for a
            // duplicate edge (the universal contract — see backend_tests). Absorb that
            // one as idempotent success ("already blocked"); other conflicts (self-dep,
            // cycle) stay errors.
            match backend.add_dep(task_id, depends_on, &me).await {
                Ok(()) => emit_ok(
                    json,
                    &format!("  Task #{task_id} now blocked by #{depends_on}"),
                )?,
                Err(BackendError::Conflict(msg)) if msg.contains("already exists") => emit_ok(
                    json,
                    &format!("  Task #{task_id} already blocked by #{depends_on}"),
                )?,
                Err(e) => return Err(e.into()),
            }
        }
        TaskCmd::Unblock {
            task_id,
            depends_on,
        } => {
            backend.remove_dep(task_id, depends_on, &me).await?;
            emit_ok(
                json,
                &format!("  Removed dependency: #{task_id} no longer blocked by #{depends_on}"),
            )?;
        }
        TaskCmd::Deps { id } => {
            let nodes = backend.deps_tree(id).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&nodes)?);
            } else {
                let t = backend
                    .get(id)
                    .await?
                    .ok_or_else(|| format!("task #{id} not found"))?;
                println!("\n  Dependencies for #{id}: {}", t.task.title);
                if nodes.is_empty() {
                    println!("  (none)");
                } else {
                    for n in nodes {
                        let indent = "  ".repeat(n.depth as usize);
                        let mark = if n.status == "done" {
                            "\x1b[32m✓\x1b[0m"
                        } else {
                            "\x1b[31m○\x1b[0m"
                        };
                        println!("  {indent}{mark} #{} {} ({})", n.id, n.title, n.status);
                    }
                }
                println!();
            }
        }
        TaskCmd::Delete { id } => {
            backend.delete(id).await?;
            emit_ok(json, &format!("  Deleted task #{id}"))?;
        }
        TaskCmd::Dispatch {
            id,
            worktree,
            wait,
            until_claimed,
        } => {
            cmd_task_dispatch(
                backend,
                local,
                config,
                &me,
                id,
                worktree.as_deref(),
                wait,
                until_claimed,
                json,
            )
            .await?
        }
        TaskCmd::Watch {
            worktree,
            interval_ms,
        } => {
            cmd_task_watch(
                backend,
                local,
                config,
                worktree.as_deref(),
                interval_ms,
                json,
            )
            .await?
        }
    }

    Ok(())
}

// ---- task dispatch / watch ----

/// How often `task dispatch --wait` re-reads its request row.
const DISPATCH_WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// `keitora task dispatch`: write a dispatch request into the project's `tasks.db`
/// for the running app to execute through its own dispatch path (the CLI never types
/// into a pane itself, so it can't race the app's writer). The request is a
/// short-lived message, not a queue entry: the app assigns the task to the lane (the
/// lane is the queue) and sends it if the lane is free, or refuses it. `worktree:
/// None` asks the app to auto-pick from the dispatch pool. Checks what it can up front
/// (the project is registered with the app, the worktree name and pool, the task),
/// then optionally waits for the outcome.
#[allow(clippy::too_many_arguments)]
async fn cmd_task_dispatch(
    backend: &dyn TaskBackend,
    local: &LocalStore,
    config: &Config,
    me: &str,
    id: i64,
    worktree: Option<&str>,
    wait: u64,
    until_claimed: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if until_claimed && wait == 0 {
        return Err("--until-claimed needs --wait <secs> to bound how long it waits".into());
    }
    // The app only reads requests from the DBs of projects in its registry; anywhere
    // else a request would sit unread until it expired.
    let root = &config.project_config().root;
    let registry = UserConfig::load().map_err(|e| e.to_string())?;
    if registry.project_ref_by_root(root).is_none() {
        return Err(unregistered_message(root).into());
    }
    if let Some(worktree) = worktree {
        if !config.worktrees.is_empty() && !config.worktrees.iter().any(|w| w == worktree) {
            return Err(format!(
                "unknown worktree \"{worktree}\" — must be one of: {}",
                config.worktrees.join(", ")
            )
            .into());
        }
        if !config.project_config().in_dispatch_pool(worktree) {
            return Err(pool_refusal_message(worktree).into());
        }
    } else if config.project_config().dispatch_pool().is_empty() {
        return Err(
            "no worktree is in the dispatch pool; add one in the app's worktree menu \
             (or pass --worktree)"
                .into(),
        );
    }
    let twd = backend
        .get(id)
        .await?
        .ok_or_else(|| format!("task #{id} not found"))?;
    // Task-side checks; an auto-pick judges them against no particular lane.
    let target = worktree.unwrap_or("");
    let check = if worktree.is_none() && twd.task.status == "open" {
        // An auto-pick of a card pending on a pool lane retries it there.
        twd.task.assigned_to.clone().unwrap_or_default()
    } else {
        target.to_string()
    };
    if let Some(reason) = keitora_tasks::dispatch::task_refusal(&twd, &check) {
        return Err(reason.into());
    }

    let mut req = local.dispatch_request_add(id, target, me)?;
    // One budget for the whole wait: the app's outcome, then (with --until-claimed)
    // the agent's claim.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait);
    if wait > 0 {
        while !req.is_resolved() && std::time::Instant::now() < deadline {
            std::thread::sleep(DISPATCH_WAIT_POLL);
            req = local
                .dispatch_request_get(req.id)?
                .ok_or_else(|| format!("dispatch request {} vanished", req.id))?;
        }
        // Withdraw a request nobody picked up, so it can't fire later unexpectedly.
        if req.status == "pending"
            && local.dispatch_request_cancel(
                req.id,
                &format!("no running Keitora app picked it up within {wait}s"),
            )?
        {
            req = local.dispatch_request_get(req.id)?.unwrap_or(req);
        }
    }

    // `applied` means the task is on the lane: sent, or pending there until the lane
    // frees up. --until-claimed waits on until the agent in that worktree has taken it.
    let worktree = req.worktree.clone();
    let sent = req.status == "applied"
        && backend
            .dispatched_reservations()
            .await?
            .iter()
            .any(|r| r.task_id == id && r.worktree == worktree);
    let mut claim = None;
    if until_claimed && req.status == "applied" {
        let mut progress = ClaimProgress::Waiting;
        loop {
            if let Some(twd) = backend.get(id).await? {
                progress = claim_progress(&twd.task, &worktree);
            }
            if progress != ClaimProgress::Waiting || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(DISPATCH_WAIT_POLL);
        }
        claim = Some(progress);
    }

    let shown = if worktree.is_empty() {
        "(auto)".to_string()
    } else {
        worktree.clone()
    };
    if json {
        let mut v = serde_json::to_value(&req)?;
        if let Some(obj) = v.as_object_mut() {
            if req.status == "applied" {
                obj.insert("sent".into(), json!(sent));
            }
            if let Some(p) = &claim {
                obj.insert("claimed".into(), json!(*p == ClaimProgress::Claimed));
            }
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("{}", dispatch_request_message(&req, wait > 0, sent));
        if claim == Some(ClaimProgress::Claimed) {
            println!("  Claimed by @{worktree}");
        }
    }
    match req.status.as_str() {
        "refused" => Err(format!("dispatch of #{id} to @{shown} refused: {}", req.reason).into()),
        "running" => {
            Err(format!("dispatch of #{id} to @{shown} still running after {wait}s").into())
        }
        _ => match claim {
            Some(ClaimProgress::Waiting) => Err(format!(
                "dispatch of #{id} to @{shown} applied, but not claimed within {wait}s"
            )
            .into()),
            Some(ClaimProgress::Lost(why)) => {
                Err(format!("dispatch of #{id} to @{shown} applied, but {why}").into())
            }
            _ => Ok(()),
        },
    }
}

/// The refusal for a worktree outside the dispatch pool (mirrors the app's).
fn pool_refusal_message(worktree: &str) -> String {
    format!(
        "@{worktree} is not in the dispatch pool, so it takes no tasks; add it from its \
         worktree menu in the app"
    )
}

/// Where a dispatched task stands, for `--until-claimed`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaimProgress {
    /// Still reserved to the worktree, not yet claimed.
    Waiting,
    /// The worktree's agent claimed it (it is active there, or already in review or
    /// done there).
    Claimed,
    /// It went somewhere else; waiting longer won't make it claimed here.
    Lost(String),
}

/// Pure: classify `task` after a dispatch onto `worktree`.
fn claim_progress(task: &Task, worktree: &str) -> ClaimProgress {
    let here = task.assigned_to.as_deref() == Some(worktree);
    match (task.status.as_str(), here) {
        ("active" | "review" | "done", true) => ClaimProgress::Claimed,
        ("open", true) => ClaimProgress::Waiting,
        ("active", false) => ClaimProgress::Lost(format!(
            "it was claimed by @{}",
            task.assigned_to.as_deref().unwrap_or("someone else")
        )),
        ("open", false) => ClaimProgress::Lost("it was released".to_string()),
        (status, _) => ClaimProgress::Lost(format!("it is now {status}")),
    }
}

/// The fields `task review` records with the hand-off: the PR, when given, and the
/// current git branch when the task has none yet (a detached HEAD has no branch).
fn review_update(
    task_branch: Option<&str>,
    head_branch: Option<&str>,
    pr: Option<String>,
) -> TaskUpdate {
    let branch = match (task_branch, head_branch) {
        (None, Some(b)) if !b.is_empty() => Some(b.to_string()),
        _ => None,
    };
    TaskUpdate {
        branch,
        pr_url: pr.filter(|p| !p.trim().is_empty()),
        ..TaskUpdate::default()
    }
}

/// The branch checked out in the current directory, or `None` (detached, or not a repo).
fn current_branch() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// Why `task done` refuses inside an agent session: the agent's work goes to review,
/// and a human marks it done.
fn agent_done_refusal(id: i64) -> String {
    format!(
        "this looks like an agent session, and agents don't mark tasks done: run \
         `./keitora task review {id}` and stop. A human marks it done after review \
         (a human running this from an agent session can pass --force)"
    )
}

/// The error for a `task dispatch` from a project the app hasn't registered.
fn unregistered_message(root: &std::path::Path) -> String {
    format!(
        "the project at {} isn't registered with Keitora, so the app would never pick up \
         the request — add it in the app first (or run from a registered project)",
        root.display()
    )
}

/// The human-readable line for a dispatch request's current state. `waited` is
/// whether the caller waited for the outcome (a bare `pending` is expected otherwise);
/// `sent` is whether an applied request's prompt reached the session (else the task
/// is pending on the lane, and the reason says why).
fn dispatch_request_message(req: &DispatchRequest, waited: bool, sent: bool) -> String {
    let id = req.task_id;
    let wt = if req.worktree.is_empty() {
        "(auto-pick)"
    } else {
        &req.worktree
    };
    let suffix = if req.reason.is_empty() {
        String::new()
    } else {
        format!(" ({})", req.reason)
    };
    match req.status.as_str() {
        "applied" if sent => format!("  Dispatched #{id} to @{wt}{suffix}"),
        "applied" => format!("  Assigned #{id} to @{wt}{suffix}"),
        "refused" => format!("  Refused #{id} → @{wt}{suffix}"),
        "pending" if !waited => format!(
            "  Requested dispatch of #{id} to @{wt} (request {}); the running Keitora app applies it",
            req.id
        ),
        other => format!("  Request {} for #{id} → @{wt} is {other}{suffix}", req.id),
    }
}

/// What `task watch` has already reported, so each poll emits only what changed.
#[derive(Debug, Default)]
struct WatchState {
    /// Per worktree: the last reported `(state, current_task_id)`. Heartbeat-only
    /// re-upserts don't change it, so they aren't reported.
    sessions: std::collections::HashMap<String, (String, Option<i64>)>,
    /// Highest dispatch-log id already reported (or present when the watch started).
    last_log_id: i64,
    /// Per request id: the last reported status.
    requests: std::collections::HashMap<i64, String>,
}

impl WatchState {
    /// Start from the current dispatch log and requests without reporting them, so a
    /// watch streams what happens from now on. Sessions are left empty: the first
    /// poll reports every session's current state as a baseline.
    fn seeded(log: &[DispatchLogEntry], requests: &[DispatchRequest]) -> Self {
        WatchState {
            sessions: Default::default(),
            last_log_id: log.iter().map(|e| e.id).max().unwrap_or(0),
            requests: requests.iter().map(|r| (r.id, r.status.clone())).collect(),
        }
    }

    /// One poll: the events (JSON objects tagged with `"event"`) for every change
    /// since the last poll, oldest first within each kind, limited to `worktree` when
    /// given. Sessions come first, then dispatch-log entries, then requests.
    fn step(
        &mut self,
        sessions: &[WorktreeSession],
        log: &[DispatchLogEntry],
        requests: &[DispatchRequest],
        worktree: Option<&str>,
    ) -> Vec<serde_json::Value> {
        let wanted = |w: &str| worktree.is_none_or(|f| f == w);
        let mut out = Vec::new();
        for s in sessions.iter().filter(|s| wanted(&s.worktree)) {
            let now = (s.state.clone(), s.current_task_id);
            if self.sessions.get(&s.worktree) != Some(&now) {
                self.sessions.insert(s.worktree.clone(), now);
                out.push(tagged("session", serde_json::to_value(s)));
            }
        }
        let mut fresh: Vec<&DispatchLogEntry> =
            log.iter().filter(|e| e.id > self.last_log_id).collect();
        fresh.sort_by_key(|e| e.id);
        for e in fresh {
            self.last_log_id = self.last_log_id.max(e.id);
            if wanted(&e.worktree) {
                out.push(tagged("dispatch", serde_json::to_value(e)));
            }
        }
        let mut reqs: Vec<&DispatchRequest> = requests.iter().collect();
        reqs.sort_by_key(|r| r.id);
        for r in reqs {
            if self.requests.get(&r.id) != Some(&r.status) {
                self.requests.insert(r.id, r.status.clone());
                if wanted(&r.worktree) {
                    out.push(tagged("request", serde_json::to_value(r)));
                }
            }
        }
        out
    }
}

/// `value` (a serialized row) with an `"event": kind` field added.
fn tagged(kind: &str, value: serde_json::Result<serde_json::Value>) -> serde_json::Value {
    let mut v = value.unwrap_or_else(|_| json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert("event".into(), json!(kind));
    }
    v
}

/// The human-readable line for one `task watch` event.
fn watch_line(ev: &serde_json::Value) -> String {
    let s = |k: &str| ev.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let n = |k: &str| ev.get(k).and_then(|v| v.as_i64());
    let line = match s("event").as_str() {
        "session" => {
            let task = n("current_task_id")
                .map(|t| format!("  (task #{t})"))
                .unwrap_or_default();
            format!("  @{}  {}{task}", s("worktree"), s("state"))
        }
        "dispatch" => format!(
            "  #{} → @{}  {}  {}",
            n("task_id").unwrap_or(0),
            s("worktree"),
            s("decision"),
            s("rationale")
        ),
        "request" => format!(
            "  request {}: #{} → @{}  {}  {}",
            n("id").unwrap_or(0),
            n("task_id").unwrap_or(0),
            s("worktree"),
            s("status"),
            s("reason")
        ),
        _ => ev.to_string(),
    };
    line.trim_end().to_string()
}

/// How many recent dispatch-log entries / requests each `task watch` poll reads.
const WATCH_WINDOW: i64 = 100;

/// `keitora task watch`: poll the project's `tasks.db` and print each change in
/// `worktree_sessions`, the dispatch log and the dispatch requests, one line each
/// (JSON lines with `--json`), until interrupted or stdout closes. Session states are
/// what the agent hooks last wrote; only the app reconciles them with live panes.
async fn cmd_task_watch(
    backend: &dyn TaskBackend,
    local: &LocalStore,
    config: &Config,
    worktree: Option<&str>,
    interval_ms: u64,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    use std::io::Write;

    let interval = std::time::Duration::from_millis(interval_ms.max(50));
    let mut state = WatchState::seeded(
        &backend.dispatch_log_list(WATCH_WINDOW).await?,
        &local.dispatch_requests_list(WATCH_WINDOW)?,
    );
    loop {
        // Scope sessions to this project's worktrees, as the app does; the table
        // holds a row for every worktree that ever ran a session.
        let sessions: Vec<_> = local
            .sessions_list()?
            .into_iter()
            .filter(|s| config.worktrees.is_empty() || config.worktrees.contains(&s.worktree))
            .collect();
        let events = state.step(
            &sessions,
            &backend.dispatch_log_list(WATCH_WINDOW).await?,
            &local.dispatch_requests_list(WATCH_WINDOW)?,
            worktree,
        );
        let mut out = std::io::stdout().lock();
        for ev in &events {
            let line = if json { ev.to_string() } else { watch_line(ev) };
            // A closed pipe (e.g. `| head`) ends the watch cleanly.
            if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                return Ok(());
            }
        }
        drop(out);
        std::thread::sleep(interval);
    }
}

// ---- config subcommand dispatch ----

fn run_config(sub: &ConfigCmd, json: bool) -> Result<(), Box<dyn Error>> {
    match sub {
        ConfigCmd::Init => cmd_config_init(json),
        ConfigCmd::Migrate => cmd_config_migrate(json),
        ConfigCmd::Projects => cmd_config_projects(json),
        ConfigCmd::Use { name } => cmd_config_use(name, json),
    }
}

fn cmd_config_init(json: bool) -> Result<(), Box<dyn Error>> {
    // 1. Locate project root via git toplevel.
    let project_root = keitora_tasks::git_toplevel().ok_or("not inside a git repository")?;

    // 2. Candidate keitora.toml path.
    let toml_path = project_root.join("keitora.toml");

    // 3. Error if it already exists.
    if toml_path.exists() {
        return Err(format!(
            "keitora.toml already exists at {} \
             — run `keitora config migrate` if you have a legacy task-config.json",
            toml_path.display()
        )
        .into());
    }

    // 4–5. Scaffold via the shared helper so the CLI and the app's onboarding
    //      produce identical configs (correct retention defaults + shared DB
    //      path under <name>-worktrees/shared/, not a divergent flat tasks.db).
    let pc = ProjectConfig::scaffold(&project_root);
    let name = pc.project.name.clone();
    pc.write_initial().map_err(|e| e.to_string())?;

    // 6–9. Register in user config; set active if not yet set.
    let mut user_cfg = UserConfig::load().map_err(|e| e.to_string())?;
    user_cfg.register(&name, project_root.clone());
    let set_active = user_cfg.active.is_none();
    if set_active {
        user_cfg.set_active(&name);
    }
    user_cfg.save().map_err(|e| e.to_string())?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "path": toml_path.to_string_lossy(),
                "name": name,
                "set_active": set_active,
            }))?
        );
    } else {
        println!("  Initialized keitora.toml at {}", toml_path.display());
        println!("  Registered project \"{name}\" in user config");
        if set_active {
            println!("  Active project set to \"{name}\"");
        }
    }

    Ok(())
}

fn cmd_config_migrate(json: bool) -> Result<(), Box<dyn Error>> {
    // 1. Locate legacy task-config.json.
    let legacy_path = find_legacy_config().ok_or("no task-config.json found")?;
    if !legacy_path.exists() {
        return Err("no task-config.json found".into());
    }

    // 2. Migrate (idempotent). `wrote` is the accurate signal — true when a new
    //    keitora.toml was written, false on the idempotent no-op — so we don't have
    //    to infer it from registry state (which misfires on name collisions or a
    //    stale registry after a manual delete).
    let (pc, wrote) = migrate_legacy(&legacy_path).map_err(|e| format!("migration failed: {e}"))?;
    let toml_path = pc.root.join("keitora.toml");
    let already_existed = !wrote;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "path": toml_path.to_string_lossy(),
                "already_existed": already_existed,
            }))?
        );
    } else if already_existed {
        println!(
            "  keitora.toml already exists at {} — nothing to do",
            toml_path.display()
        );
    } else {
        println!("  Migrated task-config.json → {}", toml_path.display());
    }

    Ok(())
}

fn cmd_config_projects(json: bool) -> Result<(), Box<dyn Error>> {
    let user_cfg = UserConfig::load().map_err(|e| e.to_string())?;

    if json {
        let projects: serde_json::Map<String, serde_json::Value> = user_cfg
            .projects
            .iter()
            .map(|(k, v)| (k.clone(), json!({ "path": v.path.to_string_lossy() })))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "active": user_cfg.active,
                "projects": projects,
            }))?
        );
    } else if user_cfg.projects.is_empty() {
        println!("  No projects registered. Run `keitora config init` to register this project.");
    } else {
        for (name, pref) in &user_cfg.projects {
            let is_active = user_cfg.active.as_deref() == Some(name.as_str());
            let marker = if is_active { "*" } else { " " };
            // Path column tab-aligned 16 chars from start of name.
            println!("  {marker} {name:<16}{}", pref.path.display());
        }
    }

    Ok(())
}

fn cmd_config_use(name: &str, json: bool) -> Result<(), Box<dyn Error>> {
    let mut user_cfg = UserConfig::load().map_err(|e| e.to_string())?;

    if !user_cfg.projects.contains_key(name) {
        return Err(format!(
            "unknown project \"{name}\" — run `keitora config projects` to see registered names"
        )
        .into());
    }

    user_cfg.set_active(name);
    user_cfg.save().map_err(|e| e.to_string())?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "active": name }))?
        );
    } else {
        println!("  Active project set to \"{name}\"");
    }

    Ok(())
}

fn emit_ok(json: bool, msg: &str) -> Result<(), Box<dyn Error>> {
    if json {
        println!("{}", serde_json::to_string(&json!({ "ok": true }))?);
    } else {
        println!("{msg}");
    }
    Ok(())
}

fn emit_task_msg(json: bool, task: &Task, msg: &str) -> Result<(), Box<dyn Error>> {
    if json {
        println!("{}", serde_json::to_string_pretty(task)?);
    } else {
        println!("{msg}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The `config` subcommand wiring is the new surface in this binary; the
    // heavy logic (scaffold defaults, migration) lives in (and is tested by)
    // `keitora-tasks`. Guard the clap parsing so the subcommands can't silently
    // change shape.
    #[test]
    fn parses_config_subcommands() {
        match Cli::try_parse_from(["keitora", "config", "use", "myproj"])
            .unwrap()
            .cmd
        {
            Cmd::Config(ConfigCmd::Use { name }) => assert_eq!(name, "myproj"),
            _ => panic!("expected `config use`"),
        }
        assert!(matches!(
            Cli::try_parse_from(["keitora", "config", "init"])
                .unwrap()
                .cmd,
            Cmd::Config(ConfigCmd::Init)
        ));
        assert!(matches!(
            Cli::try_parse_from(["keitora", "config", "migrate"])
                .unwrap()
                .cmd,
            Cmd::Config(ConfigCmd::Migrate)
        ));
        assert!(matches!(
            Cli::try_parse_from(["keitora", "config", "projects"])
                .unwrap()
                .cmd,
            Cmd::Config(ConfigCmd::Projects)
        ));
        // `config use` requires a project name.
        assert!(Cli::try_parse_from(["keitora", "config", "use"]).is_err());
    }

    // Task verbs now live under the `task` subcommand; `session` and `config`
    // are siblings, not task verbs.
    #[test]
    fn parses_task_and_sibling_subcommands() {
        assert!(matches!(
            Cli::try_parse_from(["keitora", "task", "list"])
                .unwrap()
                .cmd,
            Cmd::Task(TaskCmd::List { .. })
        ));
        match Cli::try_parse_from(["keitora", "task", "claim", "7"])
            .unwrap()
            .cmd
        {
            Cmd::Task(TaskCmd::Claim { id }) => assert_eq!(id, 7),
            _ => panic!("expected `task claim`"),
        }
        assert!(matches!(
            Cli::try_parse_from(["keitora", "task", "review", "7"])
                .unwrap()
                .cmd,
            Cmd::Task(TaskCmd::Review { id: 7, pr: None })
        ));
        match Cli::try_parse_from([
            "keitora",
            "task",
            "review",
            "7",
            "--pr",
            "https://example.com/pr/1",
        ])
        .unwrap()
        .cmd
        {
            Cmd::Task(TaskCmd::Review { id, pr }) => {
                assert_eq!(id, 7);
                assert_eq!(pr.as_deref(), Some("https://example.com/pr/1"));
            }
            _ => panic!("expected `task review --pr`"),
        }
        assert!(matches!(
            Cli::try_parse_from(["keitora", "task", "done", "7"])
                .unwrap()
                .cmd,
            Cmd::Task(TaskCmd::Done {
                id: 7,
                force: false
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["keitora", "task", "done", "7", "--force"])
                .unwrap()
                .cmd,
            Cmd::Task(TaskCmd::Done { id: 7, force: true })
        ));
        match Cli::try_parse_from(["keitora", "session", "idle"])
            .unwrap()
            .cmd
        {
            Cmd::Session { state } => assert_eq!(state, "idle"),
            _ => panic!("expected `session`"),
        }
        // `--json` is global: valid before or after the subcommand path.
        assert!(Cli::try_parse_from(["keitora", "--json", "task", "list"]).is_ok());
        assert!(Cli::try_parse_from(["keitora", "task", "list", "--json"]).is_ok());
        // Old flat form must NOT parse anymore (clean break — no `task` alias).
        assert!(Cli::try_parse_from(["keitora", "list"]).is_err());
    }

    #[test]
    fn parses_task_dispatch_and_watch() {
        match Cli::try_parse_from(["keitora", "task", "dispatch", "7", "--worktree", "alpha"])
            .unwrap()
            .cmd
        {
            Cmd::Task(TaskCmd::Dispatch {
                id,
                worktree,
                wait,
                until_claimed,
            }) => {
                assert_eq!(
                    (id, worktree.as_deref(), wait, until_claimed),
                    (7, Some("alpha"), 0, false)
                );
            }
            _ => panic!("expected `task dispatch`"),
        }
        match Cli::try_parse_from([
            "keitora",
            "task",
            "dispatch",
            "7",
            "--worktree",
            "alpha",
            "--wait",
            "30",
            "--json",
        ])
        .unwrap()
        .cmd
        {
            Cmd::Task(TaskCmd::Dispatch { wait, .. }) => assert_eq!(wait, 30),
            _ => panic!("expected `task dispatch`"),
        }
        // There is no `--queue`: the board is the queue.
        assert!(Cli::try_parse_from([
            "keitora",
            "task",
            "dispatch",
            "7",
            "--worktree",
            "alpha",
            "--queue"
        ])
        .is_err());
        // No worktree: the app auto-picks one from the pool.
        match Cli::try_parse_from(["keitora", "task", "dispatch", "7"])
            .unwrap()
            .cmd
        {
            Cmd::Task(TaskCmd::Dispatch { worktree, .. }) => assert_eq!(worktree, None),
            _ => panic!("expected `task dispatch`"),
        }
        // Backlog in and out.
        assert!(matches!(
            Cli::try_parse_from(["keitora", "task", "reopen", "7"])
                .unwrap()
                .cmd,
            Cmd::Task(TaskCmd::Reopen { id: 7 })
        ));
        assert!(matches!(
            Cli::try_parse_from(["keitora", "task", "unbacklog", "7"])
                .unwrap()
                .cmd,
            Cmd::Task(TaskCmd::Reopen { id: 7 })
        ));

        match Cli::try_parse_from(["keitora", "task", "watch"])
            .unwrap()
            .cmd
        {
            Cmd::Task(TaskCmd::Watch {
                worktree,
                interval_ms,
            }) => assert_eq!((worktree, interval_ms), (None, 1000)),
            _ => panic!("expected `task watch`"),
        }
        match Cli::try_parse_from(["keitora", "task", "watch", "--worktree", "bravo", "--json"])
            .unwrap()
            .cmd
        {
            Cmd::Task(TaskCmd::Watch { worktree, .. }) => {
                assert_eq!(worktree.as_deref(), Some("bravo"))
            }
            _ => panic!("expected `task watch`"),
        }
    }

    fn session(worktree: &str, state: &str, task: Option<i64>, beat: &str) -> WorktreeSession {
        WorktreeSession {
            project: String::new(),
            worktree: worktree.into(),
            state: state.into(),
            current_task_id: task,
            pid: None,
            last_heartbeat: beat.into(),
        }
    }

    fn log_entry(id: i64, task_id: i64, worktree: &str, decision: &str) -> DispatchLogEntry {
        DispatchLogEntry {
            id,
            task_id,
            worktree: worktree.into(),
            decision: decision.into(),
            rationale: String::new(),
            created_at: "2026-01-01 00:00:00".into(),
        }
    }

    fn request(id: i64, worktree: &str, status: &str) -> DispatchRequest {
        DispatchRequest {
            id,
            task_id: 5,
            worktree: worktree.into(),
            requested_by: "me".into(),
            status: status.into(),
            reason: String::new(),
            created_at: "2026-01-01 00:00:00".into(),
            resolved_at: None,
        }
    }

    fn kinds(events: &[serde_json::Value]) -> Vec<String> {
        events
            .iter()
            .map(|e| e["event"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn watch_reports_a_session_baseline_then_only_real_changes() {
        let mut w = WatchState::seeded(&[], &[]);
        let first = w.step(
            &[
                session("alpha", "idle", None, "t1"),
                session("bravo", "busy", Some(3), "t1"),
            ],
            &[],
            &[],
            None,
        );
        assert_eq!(kinds(&first), vec!["session", "session"]);
        assert_eq!(first[1]["worktree"], "bravo");
        assert_eq!(first[1]["current_task_id"], 3);
        // A heartbeat-only re-upsert is not a change.
        let beat = w.step(
            &[
                session("alpha", "idle", None, "t2"),
                session("bravo", "busy", Some(3), "t2"),
            ],
            &[],
            &[],
            None,
        );
        assert!(beat.is_empty());
        // A state change or a task-pointer change is.
        let changed = w.step(
            &[
                session("alpha", "busy", None, "t3"),
                session("bravo", "busy", None, "t3"),
            ],
            &[],
            &[],
            None,
        );
        assert_eq!(kinds(&changed), vec!["session", "session"]);
        assert_eq!(changed[0]["state"], "busy");
    }

    #[test]
    fn watch_streams_only_new_log_entries_and_request_transitions() {
        // Present at start: not reported.
        let mut w = WatchState::seeded(
            &[log_entry(4, 1, "alpha", "queued")],
            &[request(1, "alpha", "applied")],
        );
        // The log arrives newest first; events come out oldest first.
        let ev = w.step(
            &[],
            &[
                log_entry(6, 2, "alpha", "dispatched"),
                log_entry(5, 1, "alpha", "dispatched"),
                log_entry(4, 1, "alpha", "queued"),
            ],
            &[
                request(2, "alpha", "pending"),
                request(1, "alpha", "applied"),
            ],
            None,
        );
        assert_eq!(kinds(&ev), vec!["dispatch", "dispatch", "request"]);
        assert_eq!(ev[0]["id"], 5);
        assert_eq!(ev[1]["id"], 6);
        assert_eq!(ev[2]["status"], "pending");
        // Same data again: nothing new. A status change is.
        let again = w.step(
            &[],
            &[log_entry(6, 2, "alpha", "dispatched")],
            &[request(2, "alpha", "pending")],
            None,
        );
        assert!(again.is_empty());
        let moved = w.step(&[], &[], &[request(2, "alpha", "refused")], None);
        assert_eq!(kinds(&moved), vec!["request"]);
        assert_eq!(moved[0]["status"], "refused");
    }

    #[test]
    fn watch_filters_to_one_worktree() {
        let mut w = WatchState::seeded(&[], &[]);
        let ev = w.step(
            &[
                session("alpha", "idle", None, "t"),
                session("bravo", "idle", None, "t"),
            ],
            &[
                log_entry(2, 1, "bravo", "dispatched"),
                log_entry(1, 1, "alpha", "dispatched"),
            ],
            &[
                request(1, "bravo", "pending"),
                request(2, "alpha", "pending"),
            ],
            Some("alpha"),
        );
        assert_eq!(kinds(&ev), vec!["session", "dispatch", "request"]);
        assert!(ev.iter().all(|e| e["worktree"] == "alpha"));
        // A filtered-out log entry still advances the cursor (not re-reported later).
        assert_eq!(w.last_log_id, 2);
    }

    #[test]
    fn watch_lines_are_readable() {
        let ev = tagged(
            "session",
            serde_json::to_value(session("alpha", "busy", Some(3), "t")),
        );
        assert_eq!(watch_line(&ev), "  @alpha  busy  (task #3)");
        let ev = tagged(
            "dispatch",
            serde_json::to_value(log_entry(1, 5, "alpha", "queued")),
        );
        assert_eq!(watch_line(&ev), "  #5 → @alpha  queued");
        let mut r = request(9, "alpha", "refused");
        r.reason = "task #5 is done".into();
        let ev = tagged("request", serde_json::to_value(&r));
        assert_eq!(
            watch_line(&ev),
            "  request 9: #5 → @alpha  refused  task #5 is done"
        );
        assert_eq!(ev["event"], "request");
    }

    #[test]
    fn dispatch_request_messages() {
        let mut r = request(3, "alpha", "pending");
        assert!(dispatch_request_message(&r, false, false)
            .contains("Requested dispatch of #5 to @alpha"));
        assert_eq!(
            dispatch_request_message(&r, true, false),
            "  Request 3 for #5 → @alpha is pending"
        );
        r.status = "applied".into();
        assert_eq!(
            dispatch_request_message(&r, true, true),
            "  Dispatched #5 to @alpha"
        );
        r.reason = "pending: @alpha is busy".into();
        assert_eq!(
            dispatch_request_message(&r, true, false),
            "  Assigned #5 to @alpha (pending: @alpha is busy)"
        );
        r.status = "refused".into();
        r.reason = "@alpha is not in the dispatch pool".into();
        assert_eq!(
            dispatch_request_message(&r, true, false),
            "  Refused #5 → @alpha (@alpha is not in the dispatch pool)"
        );
        let auto = request(4, "", "pending");
        assert!(dispatch_request_message(&auto, false, false).contains("@(auto-pick)"));
    }

    #[test]
    fn unregistered_message_names_the_project_root() {
        let msg = unregistered_message(std::path::Path::new("D:/repos/proj"));
        assert!(msg.contains("D:/repos/proj"), "{msg}");
        assert!(msg.contains("isn't registered with Keitora"), "{msg}");
    }

    #[test]
    fn parses_until_claimed() {
        match Cli::try_parse_from([
            "keitora",
            "task",
            "dispatch",
            "7",
            "--worktree",
            "alpha",
            "--wait",
            "60",
            "--until-claimed",
        ])
        .unwrap()
        .cmd
        {
            Cmd::Task(TaskCmd::Dispatch {
                wait,
                until_claimed,
                ..
            }) => assert!(until_claimed && wait == 60),
            _ => panic!("expected `task dispatch`"),
        }
    }

    fn task_in(status: &str, assigned_to: Option<&str>) -> Task {
        Task {
            id: 5,
            title: "t".into(),
            description: String::new(),
            status: status.into(),
            priority: "normal".into(),
            assigned_to: assigned_to.map(str::to_string),
            created_by: None,
            tags: vec![],
            branch: None,
            pr_url: None,
            created_at: "2026-01-01 00:00:00".into(),
            updated_at: "2026-01-01 00:00:00".into(),
        }
    }

    #[test]
    fn review_records_the_pr_and_a_missing_branch() {
        let upd = review_update(
            None,
            Some("feat/x"),
            Some("https://example.com/pr/1".into()),
        );
        assert_eq!(upd.branch.as_deref(), Some("feat/x"));
        assert_eq!(upd.pr_url.as_deref(), Some("https://example.com/pr/1"));
        // A branch already on the task is kept; a blank --pr and a detached HEAD add nothing.
        let upd = review_update(Some("feat/old"), Some("feat/x"), Some("  ".into()));
        assert!(upd.is_empty(), "{upd:?}");
        assert!(review_update(None, None, None).is_empty());
    }

    #[test]
    fn agent_done_refusal_points_at_review() {
        let msg = agent_done_refusal(7);
        assert!(msg.contains("./keitora task review 7"), "{msg}");
        assert!(msg.contains("--force"), "{msg}");
    }

    #[test]
    fn claim_progress_waits_for_the_worktree_to_claim() {
        // Reserved here, not yet claimed: keep waiting.
        assert_eq!(
            claim_progress(&task_in("open", Some("alpha")), "alpha"),
            ClaimProgress::Waiting
        );
        // Claimed here (or already finished here): done.
        assert_eq!(
            claim_progress(&task_in("active", Some("alpha")), "alpha"),
            ClaimProgress::Claimed
        );
        assert_eq!(
            claim_progress(&task_in("review", Some("alpha")), "alpha"),
            ClaimProgress::Claimed
        );
        assert_eq!(
            claim_progress(&task_in("done", Some("alpha")), "alpha"),
            ClaimProgress::Claimed
        );
        // Anything else won't turn into a claim here: stop with the reason.
        assert_eq!(
            claim_progress(&task_in("active", Some("bravo")), "alpha"),
            ClaimProgress::Lost("it was claimed by @bravo".into())
        );
        assert_eq!(
            claim_progress(&task_in("open", None), "alpha"),
            ClaimProgress::Lost("it was released".into())
        );
        assert_eq!(
            claim_progress(&task_in("cancelled", Some("alpha")), "alpha"),
            ClaimProgress::Lost("it is now cancelled".into())
        );
    }
}
