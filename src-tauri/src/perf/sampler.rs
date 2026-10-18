//! `sampler` — the thread that samples every process while the overlay holds the
//! lease, and the pure steps it is built from.
//!
//! Each tick: one full process refresh; per-process rates from the measured
//! interval (a process seen for the first time reports zero, since sysinfo's
//! first delta is its lifetime total); attribution through the pane trees the
//! [`RootCache`] knows; path inference for a capped number of new unclaimed
//! processes (cached by pid and start time); then the rollup, the history rings
//! and the snapshot the overlay pulls on [`SNAPSHOT_EVENT`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::backend::herdr::real::cmd_of;
use crate::backend::{normalize_cpu, EmitSink, SessionKey};

use super::attribute::{claim_owners, infer_worktree, rollup, Owner, ProcSample, WorktreeRoot};
use super::history::HistoryStore;
use super::roots::{fetch_loop, RootCache};
use super::{
    lock, Lease, PerfMachine, PerfPane, PerfShared, PerfSnapshot, PerfSource, FIRST_TICK_DELAY,
    MAX_INFER_PER_TICK, SNAPSHOT_EVENT, TICK, TICK_LOW_HEADROOM,
};

/// How long to wait before looking for the herdr server again after a lookup
/// found none (each miss costs a full process refresh with command lines).
const SERVER_PID_RETRY: Duration = Duration::from_secs(30);

/// Whether this tick should look up the herdr server's pid: always while the
/// last lookup found one (the warm path is cheap), otherwise when none has run
/// yet or the last miss is [`SERVER_PID_RETRY`] old. `since_last` is the time
/// since the last lookup.
fn server_pid_due(last: Option<u32>, since_last: Option<Duration>) -> bool {
    last.is_some() || since_last.is_none_or(|d| d >= SERVER_PID_RETRY)
}

/// The registered worktrees, read when a tick has processes to infer.
pub type WorktreesFn = Arc<dyn Fn() -> Vec<WorktreeRoot> + Send + Sync>;

/// Everything the sampler thread runs on.
pub struct Sampler {
    pub source: Arc<dyn PerfSource>,
    pub sink: Arc<dyn EmitSink>,
    pub worktrees: WorktreesFn,
    pub shared: Arc<PerfShared>,
    /// Signalled to cut the current wait short.
    pub wake: Receiver<()>,
}

/// One process as sysinfo reports it, before rates and zeroing.
#[derive(Debug, Clone, PartialEq)]
pub struct RawProc {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub start: u64,
    pub name: String,
    /// Percent of one core (sysinfo's `cpu_usage`).
    pub cpu_usage: f32,
    pub mem_bytes: u64,
    /// Bytes read / written since the previous refresh.
    pub disk_read: u64,
    pub disk_written: u64,
}

impl RawProc {
    fn of(p: &sysinfo::Process) -> Self {
        let disk = p.disk_usage();
        RawProc {
            pid: p.pid().as_u32(),
            ppid: p.parent().map(Pid::as_u32),
            start: p.start_time(),
            name: p.name().to_string_lossy().into_owned(),
            cpu_usage: p.cpu_usage(),
            mem_bytes: p.memory(),
            disk_read: disk.read_bytes,
            disk_written: disk.written_bytes,
        }
    }
}

/// `bytes` over `elapsed_secs`, rounded; zero for a non-positive interval.
fn per_second(bytes: u64, elapsed_secs: f64) -> u64 {
    if elapsed_secs > 0.0 {
        (bytes as f64 / elapsed_secs).round() as u64
    } else {
        0
    }
}

/// Turn raw readings into samples: CPU normalised to the whole machine, disk
/// bytes into per-second rates over the measured `elapsed_secs`. A `(pid,
/// start)` not in `seen` reports zero CPU and disk; `seen` becomes this tick's
/// set.
pub fn sample_procs(
    raw: Vec<RawProc>,
    elapsed_secs: f64,
    cores: usize,
    seen: &mut HashSet<(u32, u64)>,
) -> Vec<ProcSample> {
    let next_seen: HashSet<(u32, u64)> = raw.iter().map(|r| (r.pid, r.start)).collect();
    let samples = raw
        .into_iter()
        .map(|r| {
            let first_sight = !seen.contains(&(r.pid, r.start));
            let (cpu_percent, disk_read_bps, disk_write_bps) = if first_sight {
                (0.0, 0, 0)
            } else {
                (
                    normalize_cpu(r.cpu_usage, cores),
                    per_second(r.disk_read, elapsed_secs),
                    per_second(r.disk_written, elapsed_secs),
                )
            };
            ProcSample {
                pid: r.pid,
                ppid: r.ppid,
                start: r.start,
                name: r.name,
                cpu_percent,
                mem_bytes: r.mem_bytes,
                disk_read_bps,
                disk_write_bps,
            }
        })
        .collect();
    *seen = next_seen;
    samples
}

/// The unclaimed processes whose worktree has not been inferred yet, lowest pid
/// first, at most `max`. A process claimed by a workspace that no listed pane
/// belongs to counts as unclaimed, as it does in the rollup.
pub fn infer_candidates(
    procs: &[ProcSample],
    owners: &HashMap<u32, Owner>,
    panes: &[PerfPane],
    cached: &HashMap<(u32, u64), Option<SessionKey>>,
    max: usize,
) -> Vec<(u32, u64)> {
    let listed: HashSet<&str> = panes.iter().map(|p| p.workspace_id.as_str()).collect();
    let mut out: Vec<(u32, u64)> = procs
        .iter()
        .filter(|p| match owners.get(&p.pid) {
            None => true,
            Some(Owner::Session(ws)) => !listed.contains(ws.as_str()),
            Some(Owner::Herdr | Owner::Keitora) => false,
        })
        .map(|p| (p.pid, p.start))
        .filter(|key| !cached.contains_key(key))
        .collect();
    out.sort_unstable();
    out.truncate(max);
    out
}

/// Drop inferred results of processes no longer in the sample.
pub fn prune_inferred(cached: &mut HashMap<(u32, u64), Option<SessionKey>>, procs: &[ProcSample]) {
    let live: HashSet<(u32, u64)> = procs.iter().map(|p| (p.pid, p.start)).collect();
    cached.retain(|key, _| live.contains(key));
}

/// The inferred worktrees, without the processes that matched none.
pub fn inferred_sessions(
    cached: &HashMap<(u32, u64), Option<SessionKey>>,
) -> HashMap<(u32, u64), SessionKey> {
    cached
        .iter()
        .filter_map(|(key, session)| session.clone().map(|s| (*key, s)))
        .collect()
}

/// How long to wait before the next tick.
pub fn tick_wait(low_headroom: bool) -> Duration {
    if low_headroom {
        TICK_LOW_HEADROOM
    } else {
        TICK
    }
}

/// What the sampler does after a wake cuts its wait short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStep {
    /// Stop waiting: the lease is no longer active, or the wait is over.
    Proceed,
    /// Keep waiting this much longer: the wake left the lease active (a stale
    /// wake from a release that a renewal has since overtaken).
    Wait(Duration),
}

/// After a wake: proceed when the lease is inactive (so the loop stops at once)
/// or the `deadline` has passed; otherwise wait out the rest of the tick.
pub fn after_wake(lease_active: bool, deadline: Instant, now: Instant) -> WaitStep {
    let left = deadline.saturating_duration_since(now);
    if !lease_active || left.is_zero() {
        WaitStep::Proceed
    } else {
        WaitStep::Wait(left)
    }
}

/// Wait `wait` before the next tick, returning early only when a wake finds the
/// lease inactive. Wakes that queued up meanwhile are drained, so one stale wake
/// cannot cut the next wait short too.
fn wait_tick(wake: &Receiver<()>, wait: Duration, lease: &Lease) {
    let deadline = Instant::now() + wait;
    let mut left = wait;
    loop {
        match wake.recv_timeout(left) {
            Ok(()) => while wake.try_recv().is_ok() {},
            Err(RecvTimeoutError::Timeout) => return,
            Err(RecvTimeoutError::Disconnected) => {
                // The waker is gone (never while this sampler is the running
                // one); keep the cadence rather than spin.
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                return;
            }
        }
        let now = Instant::now();
        match after_wake(lease.is_active(now), deadline, now) {
            WaitStep::Proceed => return,
            WaitStep::Wait(d) => left = d,
        }
    }
}

/// Resets the shared state when the sampler thread exits, however it exits (a
/// panic included): stops the root-fetch thread and, while this sampler still
/// owns the running flag, clears the latest snapshot and the flag so the next
/// subscription starts a fresh sampler.
struct ExitGuard {
    shared: Arc<PerfShared>,
    stop: Arc<AtomicBool>,
    /// This sampler holds `running`. False only while a normal stop has handed
    /// it back, so the guard never clears a flag a newer sampler has taken.
    owns_running: bool,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if self.owns_running {
            *lock(&self.shared.latest) = None;
            self.shared.running.store(false, Ordering::SeqCst);
        }
    }
}

/// One tick's inputs to [`build_snapshot`].
pub struct TickInput<'a> {
    pub at_ms: u64,
    /// sysinfo's global CPU usage.
    pub cpu_percent: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub procs: &'a [ProcSample],
    pub owners: &'a HashMap<u32, Owner>,
    pub inferred: &'a HashMap<(u32, u64), SessionKey>,
    pub panes: &'a [PerfPane],
    pub resolving: &'a HashSet<String>,
}

/// Roll a tick up into its snapshot, recording each row's sample in `history`
/// (rows missing this tick get a zero sample there) and attaching the rings.
/// The machine disk rates are summed over every process.
pub fn build_snapshot(input: &TickInput<'_>, history: &mut HistoryStore) -> PerfSnapshot {
    let accums = rollup(
        input.procs,
        input.owners,
        input.inferred,
        input.panes,
        input.resolving,
    );
    let mut present: HashSet<String> = HashSet::with_capacity(accums.len());
    for row in &accums {
        history.push(&row.key, row.cpu_percent, row.mem_bytes);
        present.insert(row.key.clone());
    }
    history.end_tick(&present);
    let rows = accums
        .into_iter()
        .map(|row| {
            let h = history.history(&row.key);
            row.into_row(h)
        })
        .collect();
    PerfSnapshot {
        at_ms: input.at_ms,
        machine: PerfMachine {
            cpu_percent: input.cpu_percent,
            mem_used: input.mem_used,
            mem_total: input.mem_total,
            disk_read_bps: input.procs.iter().map(|p| p.disk_read_bps).sum(),
            disk_write_bps: input.procs.iter().map(|p| p.disk_write_bps).sum(),
        },
        rows,
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What a full refresh reads for every process.
fn sample_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_disk_usage()
}

fn refresh(sys: &mut System) {
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, sample_kind());
    sys.refresh_cpu_usage();
    sys.refresh_memory();
}

/// Read the command line and cwd of `todo` (only those not read before) and
/// infer each one's worktree; a process that is gone, replaced or unreadable
/// infers none, and that is cached too.
fn infer(
    sys: &mut System,
    todo: Vec<(u32, u64)>,
    worktrees: &WorktreesFn,
    cached: &mut HashMap<(u32, u64), Option<SessionKey>>,
) {
    if todo.is_empty() {
        return;
    }
    let roots = worktrees();
    if roots.is_empty() {
        // Nothing to match against yet: cache nothing, so these are inferred
        // once worktrees are registered.
        return;
    }
    let pids: Vec<Pid> = todo.iter().map(|&(pid, _)| Pid::from_u32(pid)).collect();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        false,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_cwd(UpdateKind::OnlyIfNotSet),
    );
    for (pid, start) in todo {
        let session = sys
            .process(Pid::from_u32(pid))
            .filter(|p| p.start_time() == start)
            .and_then(|p| {
                let cwd = p.cwd().map(|c| c.to_string_lossy().into_owned());
                infer_worktree(&cmd_of(p), cwd.as_deref(), &roots)
            })
            .map(|i| roots[i].session());
        cached.insert((pid, start), session);
    }
}

/// The sampler thread. Starts the root-fetch thread, primes sysinfo, then ticks
/// until the lease lapses: it then clears the latest snapshot, marks itself
/// stopped and stops the fetch thread (through an [`ExitGuard`], so a panic does
/// the same). A renewal that lands while it is stopping keeps it running
/// instead.
pub fn run(job: Sampler) {
    let Sampler {
        source,
        sink,
        worktrees,
        shared,
        wake,
    } = job;

    let cache = Arc::new(RootCache::default());
    let stop = Arc::new(AtomicBool::new(false));
    let mut guard = ExitGuard {
        shared: Arc::clone(&shared),
        stop: Arc::clone(&stop),
        owns_running: true,
    };
    {
        let (source, cache, stop) = (Arc::clone(&source), Arc::clone(&cache), Arc::clone(&stop));
        let fetcher = std::thread::Builder::new()
            .name("perf-roots".into())
            .spawn(move || fetch_loop(&*source, &cache, &stop));
        if let Err(e) = fetcher {
            log::warn!("perf: root fetch thread failed to start: {e}");
        }
    }

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let keitora_pid = std::process::id();
    // `sys` only ever does the sampling refreshes; `ident_sys` is lent to the
    // server-pid lookup, whose cold path refreshes every process's cmd and exe.
    let mut sys = System::new();
    let mut ident_sys = System::new();
    let mut history = HistoryStore::default();
    let mut inferred: HashMap<(u32, u64), Option<SessionKey>> = HashMap::new();

    refresh(&mut sys);
    let mut last_refresh = Instant::now();
    let mut seen: HashSet<(u32, u64)> = sys
        .processes()
        .values()
        .map(|p| (p.pid().as_u32(), p.start_time()))
        .collect();
    let mut herdr_pid: Option<u32> = None;
    let mut server_checked: Option<Instant> = None;
    let mut wait = FIRST_TICK_DELAY;

    loop {
        wait_tick(&wake, wait, &shared.lease);
        if !shared.lease.is_active(Instant::now()) {
            *lock(&shared.latest) = None;
            guard.owns_running = false;
            shared.running.store(false, Ordering::SeqCst);
            let renewed = shared.lease.is_active(Instant::now())
                && shared
                    .running
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok();
            if !renewed {
                break;
            }
            guard.owns_running = true;
        }
        let started = Instant::now();

        refresh(&mut sys);
        let elapsed = last_refresh.elapsed().as_secs_f64();
        last_refresh = Instant::now();
        let raw: Vec<RawProc> = sys.processes().values().map(RawProc::of).collect();
        let procs = sample_procs(raw, elapsed, cores, &mut seen);

        if server_pid_due(herdr_pid, server_checked.map(|at| at.elapsed())) {
            herdr_pid = source.server_pid(&mut ident_sys);
            server_checked = Some(Instant::now());
        }
        cache.invalidate_generation(source.generation());
        cache.bind_and_validate(&procs);
        let panes = cache.panes();
        let roots = cache.roots(&panes);
        let resolving = cache.resolving(&panes);
        let owners = claim_owners(&procs, &roots, herdr_pid, keitora_pid);

        prune_inferred(&mut inferred, &procs);
        let todo = infer_candidates(&procs, &owners, &panes, &inferred, MAX_INFER_PER_TICK);
        infer(&mut sys, todo, &worktrees, &mut inferred);
        let inferred_now = inferred_sessions(&inferred);

        let snapshot = build_snapshot(
            &TickInput {
                at_ms: unix_ms(),
                cpu_percent: sys.global_cpu_usage(),
                mem_used: sys.used_memory(),
                mem_total: sys.total_memory(),
                procs: &procs,
                owners: &owners,
                inferred: &inferred_now,
                panes: &panes,
                resolving: &resolving,
            },
            &mut history,
        );
        *lock(&shared.latest) = Some(snapshot);
        sink.emit(SNAPSHOT_EVENT, serde_json::Value::Null);
        log::debug!("perf: tick took {} ms", started.elapsed().as_millis());

        wait = tick_wait(source.low_headroom());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SessionKey;
    use crate::perf::{PerfHistory, PerfProc, PerfRow, PerfRowKind, HISTORY_LEN};

    fn raw(pid: u32, start: u64, cpu_usage: f32, disk_read: u64, disk_written: u64) -> RawProc {
        RawProc {
            pid,
            ppid: None,
            start,
            name: format!("p{pid}"),
            cpu_usage,
            mem_bytes: 1000,
            disk_read,
            disk_written,
        }
    }

    fn sample(
        pid: u32,
        ppid: Option<u32>,
        cpu: f32,
        mem: u64,
        read: u64,
        write: u64,
    ) -> ProcSample {
        ProcSample {
            pid,
            ppid,
            start: 100,
            name: format!("p{pid}"),
            cpu_percent: cpu,
            mem_bytes: mem,
            disk_read_bps: read,
            disk_write_bps: write,
        }
    }

    fn pane(pane_id: &str, ws: &str, worktree: &str) -> PerfPane {
        PerfPane {
            pane_id: pane_id.into(),
            workspace_id: ws.into(),
            session: SessionKey {
                project: "proj".into(),
                worktree: worktree.into(),
            },
        }
    }

    fn proc_entry(p: &ProcSample, inferred: bool) -> PerfProc {
        PerfProc {
            pid: p.pid,
            name: p.name.clone(),
            cpu_percent: p.cpu_percent,
            mem_bytes: p.mem_bytes,
            disk_read_bps: p.disk_read_bps,
            disk_write_bps: p.disk_write_bps,
            inferred,
        }
    }

    #[test]
    fn first_sight_reports_zero_then_rates_over_the_measured_interval() {
        let mut seen = HashSet::from([(10, 5)]);
        let got = sample_procs(
            vec![raw(10, 5, 400.0, 3000, 1000), raw(11, 6, 50.0, 9000, 9000)],
            2.0,
            4,
            &mut seen,
        );
        assert_eq!(got[0].cpu_percent, 100.0, "400% of a core over 4 cores");
        assert_eq!((got[0].disk_read_bps, got[0].disk_write_bps), (1500, 500));
        assert_eq!(got[0].mem_bytes, 1000);
        assert_eq!(
            (
                got[1].cpu_percent,
                got[1].disk_read_bps,
                got[1].disk_write_bps
            ),
            (0.0, 0, 0),
            "first sight: sysinfo's first delta is a lifetime total"
        );
        assert_eq!(got[1].mem_bytes, 1000, "memory is not a delta");
        assert_eq!(seen, HashSet::from([(10, 5), (11, 6)]));

        // Next tick: 11 now has a rate; a reused pid 10 (new start) is new again.
        let got = sample_procs(
            vec![raw(10, 9, 40.0, 100, 0), raw(11, 6, 40.0, 2500, 0)],
            2.5,
            4,
            &mut seen,
        );
        assert_eq!((got[0].cpu_percent, got[0].disk_read_bps), (0.0, 0));
        assert_eq!((got[1].cpu_percent, got[1].disk_read_bps), (10.0, 1000));
        assert_eq!(seen, HashSet::from([(10, 9), (11, 6)]), "gone keys dropped");
    }

    #[test]
    fn a_zero_interval_reads_no_disk_rate() {
        let mut seen = HashSet::from([(10, 5)]);
        let got = sample_procs(vec![raw(10, 5, 0.0, 3000, 3000)], 0.0, 1, &mut seen);
        assert_eq!((got[0].disk_read_bps, got[0].disk_write_bps), (0, 0));
    }

    #[test]
    fn server_pid_lookup_backs_off_only_after_a_miss() {
        let secs = |n| Some(Duration::from_secs(n));
        assert!(server_pid_due(None, None), "first tick looks");
        assert!(
            server_pid_due(Some(7), secs(0)),
            "a found server is rechecked every tick"
        );
        assert!(!server_pid_due(None, secs(1)), "a miss waits");
        assert!(!server_pid_due(None, secs(29)));
        assert!(
            server_pid_due(None, secs(30)),
            "and retries after the interval"
        );
    }

    #[test]
    fn low_headroom_stretches_the_wait() {
        assert_eq!(tick_wait(false), TICK);
        assert_eq!(tick_wait(true), TICK_LOW_HEADROOM);
    }

    #[test]
    fn a_wake_with_the_lease_active_keeps_waiting_out_the_tick() {
        let t0 = Instant::now();
        let deadline = t0 + TICK;
        assert_eq!(
            after_wake(true, deadline, t0 + Duration::from_millis(50)),
            WaitStep::Wait(TICK - Duration::from_millis(50)),
            "a stale wake after a renewal does not cut the tick short"
        );
        assert_eq!(
            after_wake(false, deadline, t0 + Duration::from_millis(50)),
            WaitStep::Proceed,
            "a release stops the wait at once"
        );
        assert_eq!(after_wake(true, deadline, deadline), WaitStep::Proceed);
        assert_eq!(
            after_wake(true, deadline, deadline + Duration::from_secs(1)),
            WaitStep::Proceed
        );
    }

    #[test]
    fn wait_tick_ignores_stale_wakes_and_drains_them() {
        let wait = Duration::from_millis(150);
        let lease = Lease::default();
        let (tx, rx) = std::sync::mpsc::channel();

        lease.renew(Instant::now());
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        let t0 = Instant::now();
        wait_tick(&rx, wait, &lease);
        assert!(
            t0.elapsed() >= wait,
            "waited out the tick despite the wakes"
        );
        assert!(rx.try_recv().is_err(), "queued wakes were drained");

        lease.release();
        tx.send(()).unwrap();
        let t0 = Instant::now();
        wait_tick(&rx, Duration::from_secs(30), &lease);
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "a release returns at once"
        );
    }

    fn snapshot() -> PerfSnapshot {
        PerfSnapshot {
            at_ms: 0,
            machine: PerfMachine {
                cpu_percent: 0.0,
                mem_used: 0,
                mem_total: 0,
                disk_read_bps: 0,
                disk_write_bps: 0,
            },
            rows: Vec::new(),
        }
    }

    #[test]
    fn the_exit_guard_resets_the_state_when_the_sampler_panics() {
        let shared = Arc::new(PerfShared::default());
        shared.running.store(true, Ordering::SeqCst);
        *lock(&shared.latest) = Some(snapshot());
        let stop = Arc::new(AtomicBool::new(false));

        let (s, st) = (Arc::clone(&shared), Arc::clone(&stop));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = ExitGuard {
                shared: s,
                stop: st,
                owns_running: true,
            };
            panic!("sampler blew up");
        }));
        assert!(result.is_err());
        assert!(stop.load(Ordering::SeqCst), "the fetch thread is stopped");
        assert!(
            !shared.running.load(Ordering::SeqCst),
            "a new subscribe can start"
        );
        assert!(lock(&shared.latest).is_none());
    }

    #[test]
    fn a_guard_that_handed_running_back_leaves_it_alone() {
        // A normal stop already cleared `running`, and a newer sampler took it.
        let shared = Arc::new(PerfShared::default());
        shared.running.store(true, Ordering::SeqCst);
        *lock(&shared.latest) = Some(snapshot());
        let stop = Arc::new(AtomicBool::new(false));
        drop(ExitGuard {
            shared: Arc::clone(&shared),
            stop: Arc::clone(&stop),
            owns_running: false,
        });
        assert!(stop.load(Ordering::SeqCst));
        assert!(shared.running.load(Ordering::SeqCst));
        assert!(lock(&shared.latest).is_some());
    }

    #[test]
    fn infer_caches_nothing_while_no_worktree_is_registered() {
        let mut cached = HashMap::new();
        let none: WorktreesFn = Arc::new(Vec::new);
        infer(
            &mut System::new(),
            vec![(10, 100), (11, 100)],
            &none,
            &mut cached,
        );
        assert!(cached.is_empty(), "retried once worktrees exist");
        let todo = infer_candidates(
            &[sample(10, None, 0.0, 0, 0, 0)],
            &HashMap::new(),
            &[],
            &cached,
            64,
        );
        assert_eq!(todo, vec![(10, 100)]);
    }

    #[test]
    fn infer_candidates_are_new_unclaimed_lowest_pid_first() {
        let procs: Vec<ProcSample> = [50, 40, 30, 20, 10, 60, 70]
            .iter()
            .map(|&pid| sample(pid, None, 0.0, 0, 0, 0))
            .collect();
        let owners = HashMap::from([
            (10, Owner::Session("w1".into())),
            (20, Owner::Session("w9".into())), // no listed pane: unclaimed
            (30, Owner::Herdr),
            (40, Owner::Keitora),
        ]);
        let panes = vec![pane("w1:p1", "w1", "alpha")];
        let cached = HashMap::from([((60, 100), None)]);
        assert_eq!(
            infer_candidates(&procs, &owners, &panes, &cached, 64),
            vec![(20, 100), (50, 100), (70, 100)]
        );
        assert_eq!(
            infer_candidates(&procs, &owners, &panes, &cached, 2),
            vec![(20, 100), (50, 100)],
            "capped; the rest wait for a later tick"
        );
    }

    #[test]
    fn inferred_cache_prunes_dead_processes_and_drops_misses() {
        let alpha = SessionKey {
            project: "proj".into(),
            worktree: "alpha".into(),
        };
        let bravo = SessionKey {
            project: "proj".into(),
            worktree: "bravo".into(),
        };
        let mut cached = HashMap::from([
            ((10, 100), Some(alpha.clone())),
            ((11, 100), None),
            ((12, 100), Some(bravo)),
            ((10, 50), Some(alpha.clone())),
        ]);
        let procs = vec![
            sample(10, None, 0.0, 0, 0, 0),
            sample(11, None, 0.0, 0, 0, 0),
        ];
        prune_inferred(&mut cached, &procs);
        assert_eq!(
            cached,
            HashMap::from([((10, 100), Some(alpha.clone())), ((11, 100), None)])
        );
        assert_eq!(
            inferred_sessions(&cached),
            HashMap::from([((10, 100), alpha)])
        );
    }

    #[test]
    fn build_snapshot_yields_the_exact_snapshot() {
        let shell = sample(10, None, 2.0, 100, 10, 20);
        let agent = sample(11, Some(10), 6.0, 300, 0, 40);
        let worker = sample(20, None, 1.0, 50, 5, 0);
        let server = sample(30, None, 0.5, 70, 0, 0);
        let app = sample(1, None, 1.5, 200, 0, 0);
        let stray = sample(40, None, 3.0, 10, 1, 1);
        let procs = vec![
            shell.clone(),
            agent.clone(),
            worker.clone(),
            server.clone(),
            app.clone(),
            stray.clone(),
        ];
        let owners = HashMap::from([
            (10, Owner::Session("w1".into())),
            (11, Owner::Session("w1".into())),
            (30, Owner::Herdr),
            (1, Owner::Keitora),
        ]);
        let inferred = HashMap::from([(
            (20, 100),
            SessionKey {
                project: "proj".into(),
                worktree: "alpha".into(),
            },
        )]);
        let panes = vec![pane("w1:p1", "w1", "alpha"), pane("w2:p1", "w2", "bravo")];
        let resolving = HashSet::from(["w2".to_string()]);
        let mut history = HistoryStore::default();
        // A row from an earlier tick that is gone now.
        history.push("worktree:proj/gone", 9.0, 9);

        let snap = build_snapshot(
            &TickInput {
                at_ms: 1_700_000_000_000,
                cpu_percent: 42.0,
                mem_used: 4000,
                mem_total: 8000,
                procs: &procs,
                owners: &owners,
                inferred: &inferred,
                panes: &panes,
                resolving: &resolving,
            },
            &mut history,
        );

        let one = |cpu: f32, mem: u64| PerfHistory {
            cpu: vec![cpu],
            mem: vec![mem],
        };
        let row = |key: &str, kind: PerfRowKind| PerfRow {
            key: key.into(),
            kind,
            project: None,
            worktree: None,
            resolving: false,
            cpu_percent: 0.0,
            mem_bytes: 0,
            disk_read_bps: 0,
            disk_write_bps: 0,
            proc_count: 0,
            inferred_count: 0,
            history: one(0.0, 0),
            top_procs: Vec::new(),
        };
        let want = PerfSnapshot {
            at_ms: 1_700_000_000_000,
            machine: PerfMachine {
                cpu_percent: 42.0,
                mem_used: 4000,
                mem_total: 8000,
                disk_read_bps: 16,
                disk_write_bps: 61,
            },
            rows: vec![
                PerfRow {
                    project: Some("proj".into()),
                    resolving: true,
                    cpu_percent: 9.0,
                    mem_bytes: 450,
                    disk_read_bps: 15,
                    disk_write_bps: 60,
                    proc_count: 3,
                    inferred_count: 1,
                    history: one(9.0, 450),
                    ..row("project:proj", PerfRowKind::Project)
                },
                PerfRow {
                    project: Some("proj".into()),
                    worktree: Some("alpha".into()),
                    cpu_percent: 9.0,
                    mem_bytes: 450,
                    disk_read_bps: 15,
                    disk_write_bps: 60,
                    proc_count: 3,
                    inferred_count: 1,
                    history: one(9.0, 450),
                    top_procs: vec![
                        proc_entry(&agent, false),
                        proc_entry(&shell, false),
                        proc_entry(&worker, true),
                    ],
                    ..row("worktree:proj/alpha", PerfRowKind::Worktree)
                },
                PerfRow {
                    project: Some("proj".into()),
                    worktree: Some("bravo".into()),
                    resolving: true,
                    ..row("worktree:proj/bravo", PerfRowKind::Worktree)
                },
                PerfRow {
                    cpu_percent: 1.5,
                    mem_bytes: 200,
                    proc_count: 1,
                    history: one(1.5, 200),
                    top_procs: vec![proc_entry(&app, false)],
                    ..row("keitora", PerfRowKind::Keitora)
                },
                PerfRow {
                    cpu_percent: 0.5,
                    mem_bytes: 70,
                    proc_count: 1,
                    history: one(0.5, 70),
                    top_procs: vec![proc_entry(&server, false)],
                    ..row("herdr", PerfRowKind::Herdr)
                },
                PerfRow {
                    cpu_percent: 3.0,
                    mem_bytes: 10,
                    disk_read_bps: 1,
                    disk_write_bps: 1,
                    proc_count: 1,
                    history: one(3.0, 10),
                    top_procs: vec![proc_entry(&stray, false)],
                    ..row("other", PerfRowKind::Other)
                },
            ],
        };
        assert_eq!(snap, want);

        // The gone row's ring got a zero sample and is still kept.
        assert_eq!(
            history.history("worktree:proj/gone"),
            PerfHistory {
                cpu: vec![9.0, 0.0],
                mem: vec![9, 0],
            }
        );
    }

    #[test]
    fn build_snapshot_appends_to_the_history_and_caps_it() {
        let procs = vec![sample(40, None, 1.0, 10, 0, 0)];
        let owners = HashMap::new();
        let inferred = HashMap::new();
        let resolving = HashSet::new();
        let mut history = HistoryStore::default();
        let mut last = None;
        for _ in 0..HISTORY_LEN + 5 {
            last = Some(build_snapshot(
                &TickInput {
                    at_ms: 0,
                    cpu_percent: 0.0,
                    mem_used: 0,
                    mem_total: 0,
                    procs: &procs,
                    owners: &owners,
                    inferred: &inferred,
                    panes: &[],
                    resolving: &resolving,
                },
                &mut history,
            ));
        }
        let snap = last.unwrap();
        let other = snap.rows.iter().find(|r| r.key == "other").unwrap();
        assert_eq!(other.history.cpu, vec![1.0; HISTORY_LEN]);
        assert_eq!(other.history.mem, vec![10; HISTORY_LEN]);
        let keys: Vec<&str> = snap.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["keitora", "herdr", "other"]);
    }
}
