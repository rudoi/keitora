//! `perf` — the performance overlay's backend: which sessions, worktrees and
//! projects are using the machine's CPU, memory and disk right now.
//!
//! OS processes are attributed to Keitora sessions through their herdr panes'
//! process trees, rolled up to worktree and project, and kept in short in-memory
//! histories for sparklines. Processes outside every pane fall into three
//! non-session buckets: **Keitora** (the app and what it spawned), **herdr**
//! (the server and its non-pane children) and **Other**.
//!
//! - [`attribute`] — pure attribution: process trees, path inference, rollup.
//! - [`history`] — the per-row sample rings behind the sparklines.
//! - [`roots`] — each session pane's root processes, fetched from herdr in the
//!   background.
//! - [`sampler`] — the sampler thread and the pure snapshot builder it drives.
//!
//! Sampling is demand-driven: it runs only while the overlay holds a [`Lease`].
//! herdr is reached only through [`PerfSource`], so everything here is testable
//! without it.

pub mod attribute;
pub mod history;
pub mod roots;
pub mod sampler;

use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::activity::{lock, ActivityState};
use crate::backend::herdr::cli::PaneProcessInfo;
use crate::backend::herdr::ops::HerdrError;
use crate::backend::{EmitSink, SessionKey, SharedBackend};

use sampler::{Sampler, WorktreesFn};

/// Time between samples while the overlay is open.
pub const TICK: Duration = Duration::from_secs(2);
/// Time between samples while the machine is low on headroom.
pub const TICK_LOW_HEADROOM: Duration = Duration::from_secs(5);
/// Wait after the priming refresh before the first real sample.
pub const FIRST_TICK_DELAY: Duration = Duration::from_secs(1);
/// How long one subscription keeps the sampler alive without a renewal.
pub const LEASE: Duration = Duration::from_secs(10);
/// Samples of history kept per row (5 minutes at [`TICK`]).
pub const HISTORY_LEN: usize = 150;
/// Processes listed under a row.
pub const TOP_PROCS: usize = 8;
/// Most unattributed processes whose command line / cwd are read in one tick.
pub const MAX_INFER_PER_TICK: usize = 64;
/// Emitted (payload `null`) when a new snapshot is ready to pull.
pub const SNAPSHOT_EVENT: &str = "perf://snapshot";

/// One sample of the whole breakdown, as the overlay pulls it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PerfSnapshot {
    /// When the sample was taken, unix milliseconds.
    pub at_ms: u64,
    pub machine: PerfMachine,
    pub rows: Vec<PerfRow>,
}

/// Machine-wide totals for the overlay header.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PerfMachine {
    /// sysinfo's global CPU usage, whole-machine percent.
    pub cpu_percent: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    /// Summed over every process's per-second deltas.
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
}

/// What a [`PerfRow`] stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfRowKind {
    Project,
    Worktree,
    Keitora,
    Herdr,
    Other,
}

/// One row of the overlay table.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PerfRow {
    /// `"project:{p}"`, `"worktree:{p}/{w}"`, `"keitora"`, `"herdr"` or `"other"`.
    pub key: String,
    pub kind: PerfRowKind,
    /// The resolved project name; set for project and worktree rows.
    pub project: Option<String>,
    /// Set for worktree rows.
    pub worktree: Option<String>,
    /// The session has a pane whose root processes are not known yet.
    pub resolving: bool,
    /// Whole-machine percent, summed over the row's processes.
    pub cpu_percent: f32,
    pub mem_bytes: u64,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    pub proc_count: u32,
    /// Processes attributed by path rather than by a pane's process tree.
    pub inferred_count: u32,
    pub history: PerfHistory,
    /// Empty for project rows; at most [`TOP_PROCS`] for the rest.
    pub top_procs: Vec<PerfProc>,
}

/// A row's recent samples, oldest first, at most [`HISTORY_LEN`] each.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PerfHistory {
    pub cpu: Vec<f32>,
    pub mem: Vec<u64>,
}

/// One process listed under a row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PerfProc {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub mem_bytes: u64,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    /// Attributed by path, not by a pane's process tree.
    pub inferred: bool,
}

/// One herdr pane of a workspace that a session owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfPane {
    pub pane_id: String,
    pub workspace_id: String,
    pub session: SessionKey,
}

/// What the sampler needs from herdr; the seam that keeps this module testable
/// without it.
pub trait PerfSource: Send + Sync {
    /// Every pane of every workspace that a current-generation keymap entry owns,
    /// joined to its session.
    fn panes(&self) -> Vec<PerfPane>;
    fn pane_process_info(&self, pane_id: &str) -> Result<PaneProcessInfo, HerdrError>;
    fn server_pid(&self, sys: &mut sysinfo::System) -> Option<u32>;
    /// The keymap's server generation.
    fn generation(&self) -> u64;
    /// Whether the status loop last found the machine low on headroom.
    fn low_headroom(&self) -> bool;
}

/// The source for a backend without herdr: no panes, no server.
pub struct NoSource;

impl PerfSource for NoSource {
    fn panes(&self) -> Vec<PerfPane> {
        Vec::new()
    }

    fn pane_process_info(&self, _pane_id: &str) -> Result<PaneProcessInfo, HerdrError> {
        Err(HerdrError::Timeout)
    }

    fn server_pid(&self, _sys: &mut sysinfo::System) -> Option<u32> {
        None
    }

    fn generation(&self) -> u64 {
        0
    }

    fn low_headroom(&self) -> bool {
        false
    }
}

/// Keeps the sampler running while the overlay is open: each renewal extends it
/// to [`LEASE`] from `now`. There is one webview with one overlay, so a single
/// idempotent lease does what a refcount would.
#[derive(Debug, Default)]
pub struct Lease {
    until: Mutex<Option<Instant>>,
}

impl Lease {
    pub fn renew(&self, now: Instant) {
        *lock(&self.until) = Some(now + LEASE);
    }

    pub fn release(&self) {
        *lock(&self.until) = None;
    }

    pub fn is_active(&self, now: Instant) -> bool {
        matches!(*lock(&self.until), Some(until) if now < until)
    }
}

/// What the perf commands and the sampler thread share.
#[derive(Default)]
pub struct PerfShared {
    pub lease: Lease,
    /// The most recent snapshot; cleared when the sampler stops.
    pub latest: Mutex<Option<PerfSnapshot>>,
    /// Whether a sampler thread is running.
    pub running: AtomicBool,
    /// Wakes the running sampler out of its wait (so a release stops it at once).
    wake: Mutex<Option<Sender<()>>>,
}

/// Managed state: a handle on [`PerfShared`], which the sampler thread keeps a
/// clone of.
#[derive(Default)]
pub struct PerfState(Arc<PerfShared>);

impl PerfState {
    fn shared(&self) -> Arc<PerfShared> {
        Arc::clone(&self.0)
    }
}

impl Deref for PerfState {
    type Target = PerfShared;

    fn deref(&self) -> &PerfShared {
        &self.0
    }
}

/// Keep the sampler running for another [`LEASE`], starting it when it is not
/// running. The overlay calls this while it is open.
#[tauri::command(async)]
pub fn perf_subscribe(app: AppHandle, state: State<PerfState>, backend: State<SharedBackend>) {
    state.lease.renew(Instant::now());
    if state
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let source: Arc<dyn PerfSource> = backend.perf_source().unwrap_or_else(|| Arc::new(NoSource));
    let sink: Arc<dyn EmitSink> = Arc::new(app.clone());
    let handle = app.clone();
    let worktrees: WorktreesFn = Arc::new(move || {
        handle
            .try_state::<ActivityState>()
            .map(|activity| activity.worktree_roots())
            .unwrap_or_default()
    });
    let (wake_tx, wake_rx) = mpsc::channel();
    *lock(&state.wake) = Some(wake_tx);
    let job = Sampler {
        source,
        sink,
        worktrees,
        shared: state.shared(),
        wake: wake_rx,
    };
    let spawned = std::thread::Builder::new()
        .name("perf-sampler".into())
        .spawn(move || sampler::run(job));
    if let Err(e) = spawned {
        log::warn!("perf: sampler thread failed to start: {e}");
        state.running.store(false, Ordering::SeqCst);
    }
}

/// Release the lease: the sampler stops now instead of when the lease runs out.
#[tauri::command(async)]
pub fn perf_unsubscribe(state: State<PerfState>) {
    state.lease.release();
    if let Some(wake) = lock(&state.wake).as_ref() {
        let _ = wake.send(());
    }
}

/// The latest snapshot; `None` while the sampler is not running or has not
/// sampled yet.
#[tauri::command(async)]
pub fn perf_snapshot(state: State<PerfState>) -> Option<PerfSnapshot> {
    lock(&state.latest).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_wire_shape() {
        let snap = PerfSnapshot {
            at_ms: 1_700_000_000_123,
            machine: PerfMachine {
                cpu_percent: 12.5,
                mem_used: 4096,
                mem_total: 8192,
                disk_read_bps: 10,
                disk_write_bps: 20,
            },
            rows: vec![
                PerfRow {
                    key: "worktree:proj/alpha".into(),
                    kind: PerfRowKind::Worktree,
                    project: Some("proj".into()),
                    worktree: Some("alpha".into()),
                    resolving: true,
                    cpu_percent: 2.5,
                    mem_bytes: 1024,
                    disk_read_bps: 1,
                    disk_write_bps: 2,
                    proc_count: 3,
                    inferred_count: 1,
                    history: PerfHistory {
                        cpu: vec![0.5, 2.5],
                        mem: vec![512, 1024],
                    },
                    top_procs: vec![PerfProc {
                        pid: 42,
                        name: "claude".into(),
                        cpu_percent: 2.5,
                        mem_bytes: 1024,
                        disk_read_bps: 1,
                        disk_write_bps: 2,
                        inferred: true,
                    }],
                },
                PerfRow {
                    key: "other".into(),
                    kind: PerfRowKind::Other,
                    project: None,
                    worktree: None,
                    resolving: false,
                    cpu_percent: 0.0,
                    mem_bytes: 0,
                    disk_read_bps: 0,
                    disk_write_bps: 0,
                    proc_count: 0,
                    inferred_count: 0,
                    history: PerfHistory::default(),
                    top_procs: Vec::new(),
                },
            ],
        };
        let want = serde_json::json!({
            "at_ms": 1_700_000_000_123u64,
            "machine": {
                "cpu_percent": 12.5,
                "mem_used": 4096,
                "mem_total": 8192,
                "disk_read_bps": 10,
                "disk_write_bps": 20,
            },
            "rows": [
                {
                    "key": "worktree:proj/alpha",
                    "kind": "worktree",
                    "project": "proj",
                    "worktree": "alpha",
                    "resolving": true,
                    "cpu_percent": 2.5,
                    "mem_bytes": 1024,
                    "disk_read_bps": 1,
                    "disk_write_bps": 2,
                    "proc_count": 3,
                    "inferred_count": 1,
                    "history": {"cpu": [0.5, 2.5], "mem": [512, 1024]},
                    "top_procs": [{
                        "pid": 42,
                        "name": "claude",
                        "cpu_percent": 2.5,
                        "mem_bytes": 1024,
                        "disk_read_bps": 1,
                        "disk_write_bps": 2,
                        "inferred": true,
                    }],
                },
                {
                    "key": "other",
                    "kind": "other",
                    "project": null,
                    "worktree": null,
                    "resolving": false,
                    "cpu_percent": 0.0,
                    "mem_bytes": 0,
                    "disk_read_bps": 0,
                    "disk_write_bps": 0,
                    "proc_count": 0,
                    "inferred_count": 0,
                    "history": {"cpu": [], "mem": []},
                    "top_procs": [],
                },
            ],
        });
        assert_eq!(serde_json::to_value(&snap).unwrap(), want);
    }

    #[test]
    fn row_kinds_serialize_snake_case() {
        let kinds = [
            (PerfRowKind::Project, "project"),
            (PerfRowKind::Worktree, "worktree"),
            (PerfRowKind::Keitora, "keitora"),
            (PerfRowKind::Herdr, "herdr"),
            (PerfRowKind::Other, "other"),
        ];
        for (kind, name) in kinds {
            assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(name));
        }
    }

    #[test]
    fn lease_renews_expires_and_releases() {
        let t0 = Instant::now();
        let lease = Lease::default();
        assert!(!lease.is_active(t0), "inactive until renewed");

        lease.renew(t0);
        assert!(lease.is_active(t0));
        assert!(lease.is_active(t0 + LEASE - Duration::from_millis(1)));
        assert!(!lease.is_active(t0 + LEASE), "expires after LEASE");

        // A renewal extends it from the renewal time.
        lease.renew(t0 + Duration::from_secs(8));
        assert!(lease.is_active(t0 + LEASE));
        assert!(lease.is_active(t0 + Duration::from_secs(17)));
        assert!(!lease.is_active(t0 + Duration::from_secs(18)));

        lease.release();
        assert!(!lease.is_active(t0 + Duration::from_secs(9)), "released");
        lease.release();
        assert!(!lease.is_active(t0), "release is idempotent");
    }

    #[test]
    fn no_source_reports_nothing() {
        let src = NoSource;
        assert!(src.panes().is_empty());
        assert_eq!(src.pane_process_info("w1:p1"), Err(HerdrError::Timeout));
        assert_eq!(src.server_pid(&mut sysinfo::System::new()), None);
        assert_eq!(src.generation(), 0);
        assert!(!src.low_headroom());
    }

    #[test]
    fn perf_state_starts_empty() {
        let state = PerfState::default();
        assert!(!state.lease.is_active(Instant::now()));
        assert!(state.latest.lock().unwrap().is_none());
        assert!(!state.running.load(std::sync::atomic::Ordering::SeqCst));
    }
}
