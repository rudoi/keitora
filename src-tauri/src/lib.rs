// Keitora — Tauri core.
//
// Desktop is the authoritative (and only) host — PTY sessions, worktree/project
// commands, dispatch, task/config watchers. All of that lives in `host`
// (+ `dispatch`, `tasks`, `hooks`).

mod activity;
mod backend;
mod bg_notifier;
mod cli_install;
mod clipboard_image;
mod codex_env;
mod codex_hooks;

mod db_watch;
mod diff;
mod dispatch;
mod headroom;
mod hooks;
mod host;
mod latency_probe;
mod lifecycle;
mod logging;
mod paths;
mod perf;
mod session_toast;
mod tasks;
mod terminal_links;
mod transfer;
mod transfer_registry;
mod worktree_open;

/// Desktop entry: the full host app (PTY backend, board, dispatch, watchers).
pub fn run() {
    host::run();
}
