//! `roots` — each session pane's root processes (its shell pid plus the
//! foreground pids herdr reports), fetched from herdr in the background and
//! cached per server generation.
//!
//! A `pane process-info` call costs a CLI spawn (~350 ms), so the fetches run
//! strictly one at a time on their own thread ([`fetch_loop`]), never on the
//! sampler's. The sampler only reads the cache: the panes the fetch thread last
//! listed, their [`Root`]s, and which workspaces are still resolving.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::activity::lock;
use crate::backend::herdr::cli::PaneProcessInfo;

use super::attribute::{ProcSample, Root};
use super::{PerfPane, PerfSource};

/// Re-fetch a pane's foreground pids this often (its shell pid never changes).
pub const ROOT_REFRESH: Duration = Duration::from_secs(60);
/// Retry a pane whose fetch failed after this long; each further consecutive
/// failure doubles the wait, up to [`FETCH_RETRY_MAX`].
pub const FETCH_RETRY: Duration = Duration::from_secs(10);
/// The longest wait before retrying a pane that keeps failing.
pub const FETCH_RETRY_MAX: Duration = Duration::from_secs(300);
/// The fetch thread's pause when nothing needed fetching.
pub const IDLE_WAIT: Duration = Duration::from_secs(1);

/// One pane's fetched roots.
#[derive(Debug, Clone, PartialEq)]
pub struct RootEntry {
    pub shell_pid: u32,
    pub foreground_pids: Vec<u32>,
    /// The shell's start time, recorded the first time a sample sees its pid.
    pub shell_start: Option<u64>,
    /// Each foreground pid's start time, recorded the first time a sample sees
    /// it.
    pub foreground_starts: HashMap<u32, u64>,
    pub fetched_at: Instant,
    /// The server generation the entry was fetched under.
    pub generation: u64,
}

/// A pane's run of consecutive failed fetches.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Failure {
    at: Instant,
    /// Consecutive failures, at least 1.
    count: u32,
}

/// How long to wait before retrying a pane after `count` consecutive failures:
/// [`FETCH_RETRY`], doubling with each further failure, capped at
/// [`FETCH_RETRY_MAX`].
pub fn retry_after(count: u32) -> Duration {
    let doublings = count.saturating_sub(1).min(16);
    FETCH_RETRY
        .saturating_mul(1 << doublings)
        .min(FETCH_RETRY_MAX)
}

/// The fetched roots by pane id, the recent failures, and the panes the fetch
/// thread last listed. Shared between the fetch thread and the sampler; no lock
/// is ever held across a herdr call.
#[derive(Debug, Default)]
pub struct RootCache {
    entries: Mutex<HashMap<String, RootEntry>>,
    failures: Mutex<HashMap<String, Failure>>,
    panes: Mutex<Vec<PerfPane>>,
}

impl RootCache {
    /// The pane to fetch next: one with no entry (or an entry of another
    /// generation) first, in `panes` order; else the entry fetched longest ago,
    /// once that is at least [`ROOT_REFRESH`] ago. Panes still backing off after
    /// a failed fetch ([`retry_after`]) are skipped.
    pub fn next_to_fetch(
        &self,
        panes: &[PerfPane],
        generation: u64,
        now: Instant,
    ) -> Option<String> {
        let backing_off: HashSet<String> = lock(&self.failures)
            .iter()
            .filter(|(_, f)| now.saturating_duration_since(f.at) < retry_after(f.count))
            .map(|(id, _)| id.clone())
            .collect();
        let entries = lock(&self.entries);
        let candidates = panes.iter().filter(|p| !backing_off.contains(&p.pane_id));
        let mut oldest: Option<(&str, Instant)> = None;
        for pane in candidates {
            match entries.get(&pane.pane_id) {
                Some(e) if e.generation == generation => {
                    if now.saturating_duration_since(e.fetched_at) < ROOT_REFRESH {
                        continue;
                    }
                    if oldest.is_none_or(|(_, at)| e.fetched_at < at) {
                        oldest = Some((&pane.pane_id, e.fetched_at));
                    }
                }
                _ => return Some(pane.pane_id.clone()),
            }
        }
        oldest.map(|(id, _)| id.to_string())
    }

    /// Record a successful fetch (ending any failure backoff). A re-fetch that
    /// finds the same shell, or a foreground pid it already had, keeps the start
    /// time already bound to it.
    pub fn store(&self, pane_id: &str, info: PaneProcessInfo, generation: u64, now: Instant) {
        lock(&self.failures).remove(pane_id);
        let mut entries = lock(&self.entries);
        let prev = entries.get(pane_id).filter(|e| e.generation == generation);
        let shell_start = prev
            .filter(|e| e.shell_pid == info.shell_pid)
            .and_then(|e| e.shell_start);
        let foreground_starts: HashMap<u32, u64> = prev
            .map(|e| {
                e.foreground_starts
                    .iter()
                    .filter(|(pid, _)| info.foreground_pids.contains(pid))
                    .map(|(&pid, &start)| (pid, start))
                    .collect()
            })
            .unwrap_or_default();
        entries.insert(
            pane_id.to_string(),
            RootEntry {
                shell_pid: info.shell_pid,
                foreground_pids: info.foreground_pids,
                shell_start,
                foreground_starts,
                fetched_at: now,
                generation,
            },
        );
    }

    /// Note a failed fetch: the pane is not retried until [`retry_after`] its
    /// consecutive failures has passed.
    pub fn record_failure(&self, pane_id: &str, now: Instant) {
        let mut failures = lock(&self.failures);
        let count = failures.get(pane_id).map_or(0, |f| f.count);
        failures.insert(
            pane_id.to_string(),
            Failure {
                at: now,
                count: count.saturating_add(1),
            },
        );
    }

    /// Forget a pane's entry (herdr no longer knows the pane).
    pub fn remove(&self, pane_id: &str) {
        lock(&self.entries).remove(pane_id);
    }

    /// Drop every entry fetched under another server generation.
    pub fn invalidate_generation(&self, generation: u64) {
        lock(&self.entries).retain(|_, e| e.generation == generation);
    }

    /// Bind each entry to its processes: the first sample that sees a root pid
    /// records its start time. A later sample where the shell pid is gone, or
    /// started at another time (a reused pid), drops the entry so it is fetched
    /// again; a foreground pid that is gone or reused is dropped from the entry's
    /// roots. A pid that has never been seen is kept.
    pub fn bind_and_validate(&self, procs: &[ProcSample]) {
        let start_of: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, p.start)).collect();
        lock(&self.entries).retain(|_, e| {
            if !bind_pid(e.shell_pid, &mut e.shell_start, &start_of) {
                return false;
            }
            let starts = &mut e.foreground_starts;
            e.foreground_pids.retain(|&pid| {
                let mut bound = starts.get(&pid).copied();
                let live = bind_pid(pid, &mut bound, &start_of);
                if !live {
                    starts.remove(&pid);
                } else if let Some(start) = bound {
                    starts.insert(pid, start);
                }
                live
            });
            true
        });
    }

    /// The panes the fetch thread last listed, and the entries and failures of
    /// panes no longer among them dropped.
    pub fn set_panes(&self, panes: Vec<PerfPane>) {
        let listed: HashSet<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
        lock(&self.entries).retain(|id, _| listed.contains(id.as_str()));
        lock(&self.failures).retain(|id, _| listed.contains(id.as_str()));
        drop(listed);
        *lock(&self.panes) = panes;
    }

    /// The panes the fetch thread last listed.
    pub fn panes(&self) -> Vec<PerfPane> {
        lock(&self.panes).clone()
    }

    /// One root per workspace with at least one fetched pane, in workspace
    /// order: every fetched pane's shell pid, then their foreground pids.
    pub fn roots(&self, panes: &[PerfPane]) -> Vec<Root> {
        let entries = lock(&self.entries);
        let mut by_ws: BTreeMap<&str, (Vec<u32>, Vec<u32>)> = BTreeMap::new();
        for pane in panes {
            let Some(e) = entries.get(&pane.pane_id) else {
                continue;
            };
            let (shells, foreground) = by_ws.entry(pane.workspace_id.as_str()).or_default();
            shells.push(e.shell_pid);
            foreground.extend(e.foreground_pids.iter().copied());
        }
        by_ws
            .into_iter()
            .map(|(ws, (shells, foreground))| {
                let mut pids: Vec<u32> = Vec::with_capacity(shells.len() + foreground.len());
                for pid in shells.into_iter().chain(foreground) {
                    if !pids.contains(&pid) {
                        pids.push(pid);
                    }
                }
                Root {
                    workspace_id: ws.to_string(),
                    pids,
                }
            })
            .collect()
    }

    /// The workspaces with a pane whose roots are not fetched yet.
    pub fn resolving(&self, panes: &[PerfPane]) -> HashSet<String> {
        let entries = lock(&self.entries);
        panes
            .iter()
            .filter(|p| !entries.contains_key(&p.pane_id))
            .map(|p| p.workspace_id.clone())
            .collect()
    }

    #[cfg(test)]
    fn entry(&self, pane_id: &str) -> Option<RootEntry> {
        lock(&self.entries).get(pane_id).cloned()
    }
}

/// Bind `pid` to the process `start_of` reports for it: record its start time
/// when none is `bound` yet. False when a bound pid is gone or started at
/// another time (a reused pid).
fn bind_pid(pid: u32, bound: &mut Option<u64>, start_of: &HashMap<u32, u64>) -> bool {
    match (*bound, start_of.get(&pid)) {
        (None, Some(&start)) => {
            *bound = Some(start);
            true
        }
        (None, None) => true,
        (Some(b), Some(&start)) => b == start,
        (Some(_), None) => false,
    }
}

/// One step of the fetch thread: list the panes (publishing them to the
/// cache), then fetch at most one pane's roots. Returns whether it asked herdr
/// for a pane's roots. A pane herdr no longer knows loses its entry; any failure
/// backs the pane off ([`retry_after`]), so a gone pane that is still listed
/// (the listing is cached) is not asked about on every step.
pub fn fetch_step(source: &dyn PerfSource, cache: &RootCache, now: Instant) -> bool {
    let panes = source.panes();
    let generation = source.generation();
    cache.invalidate_generation(generation);
    let next = cache.next_to_fetch(&panes, generation, now);
    cache.set_panes(panes);
    let Some(pane_id) = next else {
        return false;
    };
    match source.pane_process_info(&pane_id) {
        Ok(info) => cache.store(&pane_id, info, generation, now),
        Err(e) => {
            if e.is_not_found() {
                cache.remove(&pane_id);
            }
            cache.record_failure(&pane_id, now);
        }
    }
    true
}

/// The fetch thread: [`fetch_step`] back to back while there is work, pausing
/// [`IDLE_WAIT`] when there is none, until `stop` is set.
pub fn fetch_loop(source: &dyn PerfSource, cache: &RootCache, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        if !fetch_step(source, cache, Instant::now()) {
            std::thread::sleep(IDLE_WAIT);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    use super::*;
    use crate::backend::herdr::fake::FakeHerdr;
    use crate::backend::herdr::ops::{HerdrError, HerdrOps, Op};
    use crate::backend::SessionKey;

    /// A [`PerfSource`] whose panes and generation the test sets, answering
    /// `pane_process_info` from a [`FakeHerdr`].
    struct TestSource {
        fake: Arc<FakeHerdr>,
        panes: Mutex<Vec<PerfPane>>,
        generation: AtomicU64,
    }

    impl TestSource {
        fn new(panes: Vec<PerfPane>) -> Self {
            TestSource {
                fake: FakeHerdr::new(),
                panes: Mutex::new(panes),
                generation: AtomicU64::new(1),
            }
        }

        fn info_calls(&self) -> Vec<String> {
            self.fake
                .calls()
                .into_iter()
                .filter(|c| c.op == Op::PaneProcessInfo)
                .map(|c| c.args)
                .collect()
        }
    }

    impl PerfSource for TestSource {
        fn panes(&self) -> Vec<PerfPane> {
            lock(&self.panes).clone()
        }

        fn pane_process_info(&self, pane_id: &str) -> Result<PaneProcessInfo, HerdrError> {
            self.fake.pane_process_info(pane_id)
        }

        fn server_pid(&self, _sys: &mut sysinfo::System) -> Option<u32> {
            None
        }

        fn generation(&self) -> u64 {
            self.generation.load(Ordering::SeqCst)
        }

        fn low_headroom(&self) -> bool {
            false
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

    fn info(shell_pid: u32, foreground_pids: &[u32]) -> PaneProcessInfo {
        PaneProcessInfo {
            shell_pid,
            foreground_pids: foreground_pids.to_vec(),
        }
    }

    fn proc_(pid: u32, start: u64) -> ProcSample {
        ProcSample {
            pid,
            ppid: None,
            start,
            name: format!("p{pid}"),
            cpu_percent: 0.0,
            mem_bytes: 0,
            disk_read_bps: 0,
            disk_write_bps: 0,
        }
    }

    #[test]
    fn fetches_only_missing_panes_one_per_step() {
        let src = TestSource::new(vec![
            pane("w1:p1", "w1", "alpha"),
            pane("w2:p1", "w2", "bravo"),
        ]);
        src.fake.set_process_info("w1:p1", info(100, &[110]));
        src.fake.set_process_info("w2:p1", info(200, &[]));
        let cache = RootCache::default();
        let t0 = Instant::now();

        assert!(fetch_step(&src, &cache, t0));
        assert_eq!(src.info_calls(), vec!["w1:p1"], "one fetch per step");
        assert!(fetch_step(&src, &cache, t0));
        assert_eq!(src.info_calls(), vec!["w1:p1", "w2:p1"]);
        assert!(!fetch_step(&src, &cache, t0), "nothing missing");
        assert!(!fetch_step(
            &src,
            &cache,
            t0 + ROOT_REFRESH - Duration::from_secs(1)
        ));
        assert_eq!(src.info_calls().len(), 2);
        assert_eq!(cache.panes(), src.panes(), "the listed panes are published");
    }

    #[test]
    fn refreshes_the_oldest_entry_after_root_refresh() {
        let src = TestSource::new(vec![
            pane("w1:p1", "w1", "alpha"),
            pane("w2:p1", "w2", "bravo"),
        ]);
        let cache = RootCache::default();
        let t0 = Instant::now();
        cache.store("w2:p1", info(200, &[]), 1, t0);
        cache.store("w1:p1", info(100, &[]), 1, t0 + Duration::from_secs(5));

        assert_eq!(
            cache.next_to_fetch(&src.panes(), 1, t0 + Duration::from_secs(59)),
            None
        );
        assert_eq!(
            cache.next_to_fetch(&src.panes(), 1, t0 + ROOT_REFRESH + Duration::from_secs(10)),
            Some("w2:p1".into()),
            "the entry fetched longest ago"
        );

        src.fake.set_process_info("w2:p1", info(200, &[210]));
        let later = t0 + ROOT_REFRESH + Duration::from_secs(10);
        assert!(fetch_step(&src, &cache, later));
        let e = cache.entry("w2:p1").unwrap();
        assert_eq!(e.foreground_pids, vec![210]);
        assert_eq!(e.fetched_at, later);
    }

    #[test]
    fn a_generation_change_drops_entries() {
        let src = TestSource::new(vec![pane("w1:p1", "w1", "alpha")]);
        src.fake.set_process_info("w1:p1", info(100, &[]));
        let cache = RootCache::default();
        let t0 = Instant::now();
        assert!(fetch_step(&src, &cache, t0));
        assert!(cache.entry("w1:p1").is_some());

        src.generation.store(2, Ordering::SeqCst);
        cache.invalidate_generation(2);
        assert!(cache.entry("w1:p1").is_none());
        assert_eq!(
            cache.next_to_fetch(&src.panes(), 2, t0),
            Some("w1:p1".into())
        );
        assert!(fetch_step(&src, &cache, t0));
        assert_eq!(cache.entry("w1:p1").unwrap().generation, 2);
    }

    #[test]
    fn an_entry_of_another_generation_is_refetched_first() {
        let cache = RootCache::default();
        let t0 = Instant::now();
        let panes = vec![pane("w1:p1", "w1", "alpha"), pane("w2:p1", "w2", "bravo")];
        cache.store("w1:p1", info(100, &[]), 1, t0);
        cache.store("w2:p1", info(200, &[]), 2, t0);
        assert_eq!(cache.next_to_fetch(&panes, 2, t0), Some("w1:p1".into()));
    }

    #[test]
    fn pid_reuse_drops_the_entry() {
        let cache = RootCache::default();
        let t0 = Instant::now();
        cache.store("w1:p1", info(100, &[]), 1, t0);
        cache.store("w2:p1", info(200, &[]), 1, t0);

        // Not seen yet: kept, unbound.
        cache.bind_and_validate(&[proc_(200, 50)]);
        assert_eq!(cache.entry("w1:p1").unwrap().shell_start, None);
        assert_eq!(cache.entry("w2:p1").unwrap().shell_start, Some(50));

        cache.bind_and_validate(&[proc_(100, 40), proc_(200, 50)]);
        assert_eq!(cache.entry("w1:p1").unwrap().shell_start, Some(40));

        // pid 100 reused by a later process; pid 200 gone.
        cache.bind_and_validate(&[proc_(100, 90)]);
        assert!(cache.entry("w1:p1").is_none(), "different start");
        assert!(cache.entry("w2:p1").is_none(), "shell gone");
    }

    #[test]
    fn a_reused_foreground_pid_is_dropped_from_the_roots() {
        let cache = RootCache::default();
        let t0 = Instant::now();
        let panes = vec![pane("w1:p1", "w1", "alpha")];
        cache.store("w1:p1", info(100, &[110, 120, 130]), 1, t0);

        // 110 and 120 are bound; 130 has not been seen yet.
        cache.bind_and_validate(&[proc_(100, 40), proc_(110, 41), proc_(120, 42)]);
        let e = cache.entry("w1:p1").unwrap();
        assert_eq!(e.foreground_starts, HashMap::from([(110, 41), (120, 42)]));

        // 110 reused by a later process, 120 gone, 130 still unseen.
        cache.bind_and_validate(&[proc_(100, 40), proc_(110, 95)]);
        let e = cache.entry("w1:p1").expect("the shell is unchanged");
        assert_eq!(e.foreground_pids, vec![130]);
        assert!(e.foreground_starts.is_empty());
        assert_eq!(
            cache.roots(&panes),
            vec![Root {
                workspace_id: "w1".into(),
                pids: vec![100, 130],
            }],
            "the reused pid is not claimed as a root"
        );

        // A re-fetch keeps the binding of a foreground pid it already had.
        cache.bind_and_validate(&[proc_(100, 40), proc_(130, 43)]);
        cache.store("w1:p1", info(100, &[130, 140]), 1, t0 + ROOT_REFRESH);
        let e = cache.entry("w1:p1").unwrap();
        assert_eq!(e.foreground_starts, HashMap::from([(130, 43)]));
    }

    #[test]
    fn a_refetch_of_the_same_shell_keeps_its_start() {
        let cache = RootCache::default();
        let t0 = Instant::now();
        cache.store("w1:p1", info(100, &[]), 1, t0);
        cache.bind_and_validate(&[proc_(100, 40)]);
        cache.store("w1:p1", info(100, &[120]), 1, t0 + ROOT_REFRESH);
        assert_eq!(cache.entry("w1:p1").unwrap().shell_start, Some(40));
        cache.store("w1:p1", info(101, &[]), 1, t0 + ROOT_REFRESH);
        assert_eq!(
            cache.entry("w1:p1").unwrap().shell_start,
            None,
            "a new shell"
        );
    }

    #[test]
    fn retry_after_doubles_up_to_the_cap() {
        let secs = Duration::from_secs;
        let schedule: Vec<Duration> = (1..=8).map(retry_after).collect();
        assert_eq!(
            schedule,
            vec![
                secs(10),
                secs(20),
                secs(40),
                secs(80),
                secs(160),
                secs(300),
                secs(300),
                secs(300)
            ]
        );
        assert_eq!(retry_after(0), FETCH_RETRY);
        assert_eq!(retry_after(u32::MAX), FETCH_RETRY_MAX);
    }

    #[test]
    fn repeated_failures_back_off_longer_and_a_success_resets() {
        let src = TestSource::new(vec![pane("w1:p1", "w1", "alpha")]);
        let cache = RootCache::default();
        let panes = src.panes();
        let t0 = Instant::now();

        cache.record_failure("w1:p1", t0);
        let t1 = t0 + FETCH_RETRY;
        assert_eq!(cache.next_to_fetch(&panes, 1, t1), Some("w1:p1".into()));
        cache.record_failure("w1:p1", t1);
        let second = retry_after(2);
        assert_eq!(second, Duration::from_secs(20));
        assert_eq!(
            cache.next_to_fetch(&panes, 1, t1 + second - Duration::from_secs(1)),
            None,
            "the second failure waits twice as long"
        );
        assert_eq!(
            cache.next_to_fetch(&panes, 1, t1 + second),
            Some("w1:p1".into())
        );

        // A success clears the run: the next failure starts over at FETCH_RETRY.
        cache.store("w1:p1", info(100, &[]), 1, t1 + second);
        cache.remove("w1:p1");
        let t2 = t1 + second;
        cache.record_failure("w1:p1", t2);
        assert_eq!(
            cache.next_to_fetch(&panes, 1, t2 + FETCH_RETRY),
            Some("w1:p1".into())
        );
    }

    #[test]
    fn a_failure_backs_off_for_fetch_retry() {
        let src = TestSource::new(vec![pane("w1:p1", "w1", "alpha")]);
        src.fake.set_process_info("w1:p1", info(100, &[]));
        src.fake.fail_next(Op::PaneProcessInfo, HerdrError::Timeout);
        let cache = RootCache::default();
        let t0 = Instant::now();

        assert!(fetch_step(&src, &cache, t0));
        assert!(cache.entry("w1:p1").is_none());
        assert!(!fetch_step(
            &src,
            &cache,
            t0 + FETCH_RETRY - Duration::from_secs(1)
        ));
        assert_eq!(src.info_calls().len(), 1, "backing off");
        assert!(fetch_step(&src, &cache, t0 + FETCH_RETRY));
        assert!(cache.entry("w1:p1").is_some(), "retried and stored");
    }

    #[test]
    fn pane_not_found_removes_the_entry() {
        let src = TestSource::new(vec![pane("w1:p1", "w1", "alpha")]);
        let cache = RootCache::default();
        let t0 = Instant::now();
        // An old-but-current entry due for a refresh; the fake has no info for
        // the pane, so it answers `pane_not_found`.
        cache.store("w1:p1", info(100, &[]), 1, t0);
        assert!(fetch_step(&src, &cache, t0 + ROOT_REFRESH));
        assert!(cache.entry("w1:p1").is_none());
        assert!(
            !fetch_step(&src, &cache, t0 + ROOT_REFRESH + Duration::from_secs(1)),
            "not asked again straight away"
        );
        assert_eq!(src.info_calls().len(), 1);
    }

    #[test]
    fn unlisted_panes_are_forgotten() {
        let src = TestSource::new(vec![pane("w1:p1", "w1", "alpha")]);
        let cache = RootCache::default();
        let t0 = Instant::now();
        cache.store("w1:p1", info(100, &[]), 1, t0);
        cache.store("w9:p1", info(900, &[]), 1, t0);
        cache.record_failure("w8:p1", t0);
        assert!(!fetch_step(&src, &cache, t0));
        assert!(cache.entry("w1:p1").is_some());
        assert!(cache.entry("w9:p1").is_none());
        assert!(lock(&cache.failures).is_empty());
    }

    #[test]
    fn roots_group_by_workspace_and_resolving_lists_unfetched() {
        let cache = RootCache::default();
        let t0 = Instant::now();
        let panes = vec![
            pane("w2:p1", "w2", "bravo"),
            pane("w1:p1", "w1", "alpha"),
            pane("w1:p2", "w1", "alpha"),
            pane("w3:p1", "w3", "charlie"),
        ];
        cache.store("w2:p1", info(200, &[210]), 1, t0);
        cache.store("w1:p1", info(100, &[110, 120]), 1, t0);
        cache.store("w1:p2", info(130, &[120]), 1, t0);
        assert_eq!(
            cache.roots(&panes),
            vec![
                Root {
                    workspace_id: "w1".into(),
                    pids: vec![100, 130, 110, 120],
                },
                Root {
                    workspace_id: "w2".into(),
                    pids: vec![200, 210],
                },
            ]
        );
        assert_eq!(cache.resolving(&panes), HashSet::from(["w3".to_string()]));

        let half = vec![pane("w1:p1", "w1", "alpha"), pane("w1:p3", "w1", "alpha")];
        assert_eq!(cache.resolving(&half), HashSet::from(["w1".to_string()]));
    }

    #[test]
    fn fetch_loop_exits_when_stopped() {
        let src = TestSource::new(Vec::new());
        let cache = RootCache::default();
        let stop = AtomicBool::new(true);
        fetch_loop(&src, &cache, &stop);
        assert!(src.info_calls().is_empty());
    }
}
