// Keitora — Tauri core.
//
// Desktop is the authoritative (and only) host — PTY sessions, worktree/project
// commands, dispatch, task/config watchers. All of that lives in `host`
// (+ `dispatch`, `tasks`, `hooks`).

mod activity;
mod agent_switch;
mod agent_updates;
mod antigravity_hooks;
mod antigravity_trust;
mod backend;
mod bg_notifier;
mod cli_install;
mod clipboard_image;
mod codex_env;
mod codex_hooks;

mod db_watch;
mod deps;
mod diff;
mod dispatch;
mod drive;
mod engine;
mod global_writes;
mod headroom;
mod health;
mod hooks;
mod host;
mod latency_probe;
mod lifecycle;
mod locks;
mod logging;
mod paths;
mod perf;
mod pipeline;
mod project_stores;
mod session_toast;
mod tasks;
mod terminal_links;
mod transcript;
mod transfer;
mod transfer_registry;
mod updater;
mod watchers;
mod worktree_open;

/// Desktop entry: the full host app (PTY backend, board, dispatch, watchers).
pub fn run() {
    host::run();
}
