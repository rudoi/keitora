//! `~/.keitora/` root resolution shared by the app's on-disk state (logs, the herdr
//! home). The implementation lives in `keitora-tasks` so the CLI resolves the same root.

pub use keitora_tasks::keitora_home;
