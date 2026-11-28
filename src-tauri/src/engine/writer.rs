//! `StatusPublisher`: the app's one writer connection to `status.db`.
//!
//! It owns the [`StatusWriter`], the lease, and the `keitora-status-heartbeat` thread
//! (every [`HEARTBEAT_EVERY`], independent of herdr and of the status loop, so the CLI
//! sees a live app even with herdr missing). Session rows are written on change only;
//! throttled `app_projects` columns and `findings.last_seen` moves are queued and
//! flushed inside the next heartbeat's transaction. Without the lease (standby) or a
//! usable file, nothing is written and every call is a no-op; the heartbeat retries.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use keitora_tasks::{
    AppProject, AppProjectTouch, Beat, FindingKey, FindingWrite, LeaseOutcome, StatusSession,
    StatusWriter, WriterIdentity, WriterOpen,
};

use super::HerdrLink;

/// How often the heartbeat thread beats.
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);

/// Whether this instance writes `status.db`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseState {
    /// This instance holds the lease and writes.
    Owned,
    /// Another live instance holds it (or holds an unusable file open).
    Standby {
        pid: i64,
        version: String,
        heartbeat_at: i64,
    },
    /// No usable file yet (it could not be opened or recreated); retried each beat.
    Closed,
}

struct Inner {
    writer: Option<StatusWriter>,
    lease: LeaseState,
    link: (&'static str, i64),
    last_snapshot_at: Option<i64>,
    /// The full current row set, rewritten after a takeover.
    rows: Vec<StatusSession>,
    touches: BTreeMap<String, AppProjectTouch>,
    last_seen: BTreeMap<FindingKey, i64>,
}

/// See the module docs.
pub struct StatusPublisher {
    path: PathBuf,
    me: WriterIdentity,
    inner: Mutex<Inner>,
    stop: AtomicBool,
}

impl StatusPublisher {
    /// Open `path`, take the lease when free, and start the heartbeat thread.
    pub fn start(path: PathBuf, version: &str) -> Arc<StatusPublisher> {
        let now = keitora_tasks::now_ms();
        let pid = std::process::id();
        let me = WriterIdentity {
            instance_id: format!("{pid:x}-{now:x}-{:016x}", rand::random::<u64>()),
            pid: i64::from(pid),
            version: version.to_string(),
            started_at: now,
        };
        let p = Self::open(path, me, now);
        spawn_heartbeat(Arc::downgrade(&p));
        p
    }

    /// Open `path` and take the lease when free, without a heartbeat thread (the
    /// caller beats).
    pub fn open(path: PathBuf, me: WriterIdentity, now_ms: i64) -> Arc<StatusPublisher> {
        let p = Arc::new(StatusPublisher {
            path,
            me,
            inner: Mutex::new(Inner {
                writer: None,
                lease: LeaseState::Closed,
                link: (HerdrLink::Starting.as_str(), now_ms),
                last_snapshot_at: None,
                rows: Vec::new(),
                touches: BTreeMap::new(),
                last_seen: BTreeMap::new(),
            }),
            stop: AtomicBool::new(false),
        });
        p.beat(now_ms);
        p
    }

    /// Whether this instance writes.
    pub fn lease(&self) -> LeaseState {
        self.inner.lock().unwrap().lease.clone()
    }

    /// One heartbeat: (re)open the file and try the lease when not owned, else
    /// refresh it and flush the queued columns in one transaction.
    pub fn beat(&self, now_ms: i64) {
        let mut st = self.inner.lock().unwrap();
        if st.writer.is_none() {
            match StatusWriter::open_or_recreate(&self.path, now_ms) {
                Ok(WriterOpen::Ready(w)) => st.writer = Some(w),
                Ok(WriterOpen::Held {
                    pid,
                    version,
                    heartbeat_at,
                }) => {
                    st.set_lease(LeaseState::Standby {
                        pid,
                        version,
                        heartbeat_at,
                    });
                    return;
                }
                Err(e) => {
                    log::warn!("status.db: cannot open {}: {e}", self.path.display());
                    st.set_lease(LeaseState::Closed);
                    return;
                }
            }
        }
        if st.lease != LeaseState::Owned {
            self.acquire(&mut st, now_ms);
            return;
        }
        // Moved out for the write; put back unless it lands.
        let touches: Vec<(String, AppProjectTouch)> =
            std::mem::take(&mut st.touches).into_iter().collect();
        let last_seen: Vec<(FindingKey, i64)> =
            std::mem::take(&mut st.last_seen).into_iter().collect();
        let beat = Beat {
            link: Some(st.link),
            last_snapshot_at: st.last_snapshot_at,
            touches: &touches,
            last_seen: &last_seen,
        };
        let id = self.me.instance_id.clone();
        let r = st
            .writer
            .as_mut()
            .map(|w| w.beat(&id, now_ms, &beat))
            .unwrap_or(Ok(false));
        if !matches!(r, Ok(true)) {
            st.touches = touches.into_iter().collect();
            st.last_seen = last_seen.into_iter().collect();
        }
        match r {
            Ok(true) => {}
            Ok(false) => {
                log::warn!("status.db: lease lost; standing by");
                // Find out who holds it (and take it back if it expired).
                self.acquire(&mut st, now_ms);
            }
            Err(e) => st.fail("heartbeat", &e),
        }
    }

    fn acquire(&self, st: &mut Inner, now_ms: i64) {
        let Some(w) = st.writer.as_mut() else {
            return;
        };
        match w.try_acquire(&self.me, now_ms) {
            Ok(LeaseOutcome::Owned) => {
                log::info!("status.db: lease taken ({})", self.me.instance_id);
                st.set_lease(LeaseState::Owned);
                // Taking the lease reset the writer row and marked every row stale:
                // write what this instance knows now.
                let id = self.me.instance_id.clone();
                let (link, since) = st.link;
                let snapshot = st.last_snapshot_at;
                let rows = st.rows.clone();
                let Some(w) = st.writer.as_mut() else {
                    return;
                };
                let r = w
                    .set_herdr_link(&id, link, since)
                    .and_then(|_| match snapshot {
                        Some(at) => w.set_last_snapshot_at(&id, at),
                        None => Ok(true),
                    })
                    .and_then(|_| w.publish_sessions(&rows, now_ms));
                if let Err(e) = r {
                    st.fail("takeover write", &e);
                }
            }
            Ok(LeaseOutcome::Standby {
                pid,
                version,
                heartbeat_at,
            }) => st.set_lease(LeaseState::Standby {
                pid,
                version,
                heartbeat_at,
            }),
            Err(e) => st.fail("lease", &e),
        }
    }

    /// Make the `sessions` table equal `rows` (the full current set), writing only
    /// changed rows.
    pub fn publish(&self, rows: Vec<StatusSession>, now_ms: i64) {
        let mut st = self.inner.lock().unwrap();
        let st = &mut *st;
        st.rows = rows;
        if st.lease != LeaseState::Owned {
            return;
        }
        let r = st
            .writer
            .as_mut()
            .map(|w| w.publish_sessions(&st.rows, now_ms));
        match r {
            Some(Ok(_)) => st.check_owner(),
            Some(Err(e)) => st.fail("session write", &e),
            None => {}
        }
    }

    /// Record the herdr link (written at once when it changed).
    pub fn set_link(&self, link: HerdrLink, since_ms: i64) {
        let mut st = self.inner.lock().unwrap();
        let link = (link.as_str(), since_ms);
        if st.link == link {
            return;
        }
        st.link = link;
        if st.lease != LeaseState::Owned {
            return;
        }
        let id = self.me.instance_id.clone();
        let r = st
            .writer
            .as_mut()
            .map(|w| w.set_herdr_link(&id, link.0, link.1));
        st.settle(r, "link write");
    }

    /// Record the time of the last successful snapshot (written with the next beat).
    pub fn set_last_snapshot(&self, at_ms: i64) {
        self.inner.lock().unwrap().last_snapshot_at = Some(at_ms);
    }

    /// Write a project's `app_projects` row at once. `true` when it was written
    /// (`false` in standby or on a failed write: the caller tries again later).
    pub fn app_project_set(&self, row: &AppProject) -> bool {
        let mut st = self.inner.lock().unwrap();
        if st.lease != LeaseState::Owned {
            return false;
        }
        let r = st.writer.as_mut().map(|w| w.app_project_set(row));
        let written = matches!(r, Some(Ok(true)));
        st.settle(r, "app_projects write");
        written
    }

    /// Delete a project's `app_projects` row at once.
    pub fn app_project_delete(&self, project: &str) {
        let mut st = self.inner.lock().unwrap();
        st.touches.remove(project);
        if st.lease != LeaseState::Owned {
            return;
        }
        let r = st.writer.as_mut().map(|w| w.app_project_delete(project));
        st.settle(r, "app_projects delete");
    }

    /// Queue throttled `app_projects` columns for the next beat.
    pub fn app_project_touch(&self, project: &str, touch: AppProjectTouch) {
        let mut st = self.inner.lock().unwrap();
        if st.lease != LeaseState::Owned {
            return;
        }
        st.touches
            .entry(project.to_string())
            .or_default()
            .merge(touch);
    }

    /// Apply finding writes: `last_seen`-only moves are queued for the next beat,
    /// the rest are written at once.
    pub fn findings_apply(&self, writes: &[FindingWrite]) {
        let mut st = self.inner.lock().unwrap();
        if st.lease != LeaseState::Owned {
            return;
        }
        let mut now = Vec::new();
        for w in writes {
            match w {
                FindingWrite::LastSeen { key, last_seen } => {
                    let at = st.last_seen.entry(key.clone()).or_insert(*last_seen);
                    *at = (*at).max(*last_seen);
                }
                FindingWrite::Upsert(f) => {
                    st.last_seen.remove(&FindingKey {
                        project: f.project.clone(),
                        worktree: f.worktree.clone(),
                        check_id: f.check_id.clone(),
                    });
                    now.push(w.clone());
                }
                FindingWrite::Delete(key) => {
                    st.last_seen.remove(key);
                    now.push(w.clone());
                }
                FindingWrite::Clear => {
                    st.last_seen.clear();
                    now.push(w.clone());
                }
            }
        }
        if now.is_empty() {
            return;
        }
        let r = st.writer.as_mut().map(|w| w.findings_apply(&now));
        st.settle(r, "findings write");
    }

    /// Give the lease up and stop the heartbeat (clean shutdown).
    pub fn release(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let mut st = self.inner.lock().unwrap();
        if st.lease != LeaseState::Owned {
            return;
        }
        let id = self.me.instance_id.clone();
        if let Some(w) = st.writer.as_mut() {
            if let Err(e) = w.release(&id) {
                log::warn!("status.db: releasing the lease failed: {e}");
            }
        }
        st.set_lease(LeaseState::Closed);
    }
}

impl Inner {
    fn set_lease(&mut self, lease: LeaseState) {
        if self.lease != lease {
            if let LeaseState::Standby { pid, version, .. } = &lease {
                log::warn!("status.db: held by pid {pid} (v{version}); this instance stands by");
            }
            self.lease = lease;
        }
        if self.lease != LeaseState::Owned {
            self.touches.clear();
            self.last_seen.clear();
        }
    }

    /// After a write that reports `Ok(false)` (lease lost) or an error.
    fn settle(&mut self, r: Option<keitora_tasks::Result<bool>>, what: &str) {
        match r {
            Some(Ok(true)) | None => {}
            Some(Ok(false)) => {
                log::warn!("status.db: lease lost ({what}); standing by");
                self.set_lease(LeaseState::Closed);
            }
            Some(Err(e)) => self.fail(what, &e),
        }
    }

    /// The writer lost the lease inside a write: stand by until the next beat.
    fn check_owner(&mut self) {
        if self.writer.as_ref().is_some_and(|w| w.owner().is_none()) {
            log::warn!("status.db: lease lost; standing by");
            self.set_lease(LeaseState::Closed);
        }
    }

    /// A write failed: drop the connection so the next beat reopens (and, for a
    /// corrupt file, recreates) it.
    fn fail(&mut self, what: &str, e: &keitora_tasks::Error) {
        log::warn!("status.db: {what} failed: {e}; reopening on the next heartbeat");
        self.writer = None;
        self.set_lease(LeaseState::Closed);
    }
}

fn spawn_heartbeat(p: Weak<StatusPublisher>) {
    const SLICE: Duration = Duration::from_millis(250);
    let r = std::thread::Builder::new()
        .name("keitora-status-heartbeat".into())
        .spawn(move || loop {
            let mut left = HEARTBEAT_EVERY;
            while !left.is_zero() {
                let step = left.min(SLICE);
                std::thread::sleep(step);
                left -= step;
                match p.upgrade() {
                    Some(p) if !p.stop.load(Ordering::SeqCst) => {}
                    _ => return,
                }
            }
            match p.upgrade() {
                Some(p) if !p.stop.load(Ordering::SeqCst) => p.beat(keitora_tasks::now_ms()),
                _ => return,
            }
        });
    if let Err(e) = r {
        log::error!("status.db: could not start the heartbeat thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keitora_tasks::{FindingRow, StatusReader};
    use std::path::Path;

    const T0: i64 = 1_800_000_000_000;

    fn me(id: &str) -> WriterIdentity {
        WriterIdentity {
            instance_id: id.into(),
            pid: 7,
            version: "0.0.0-test".into(),
            started_at: T0,
        }
    }

    fn row(wt: &str, state: &str) -> StatusSession {
        StatusSession {
            project: "proj".into(),
            worktree: wt.into(),
            state: state.into(),
            agent: "present".into(),
            agent_kind: Some("claude".into()),
            herdr_status: Some("idle".into()),
            settled: true,
            stale: false,
            interactive_ready: Some(true),
            pane_id: Some("w1:p1".into()),
            terminal_id: Some("term-1".into()),
            agent_session: None,
            state_change_seq: Some(3),
            completion_seq: None,
            brokered: false,
            escalated_reason: None,
            last_change_at: T0,
        }
    }

    fn reader(path: &Path) -> StatusReader {
        StatusReader::open(path).unwrap().expect("status.db")
    }

    fn data_version_changes(path: &Path, f: impl FnOnce()) -> bool {
        let r = reader(path);
        let before = r.data_version().unwrap();
        f();
        r.data_version().unwrap() != before
    }

    #[test]
    fn publishes_changed_rows_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.db");
        let p = StatusPublisher::open(path.clone(), me("a"), T0);
        assert_eq!(p.lease(), LeaseState::Owned);
        p.publish(vec![row("alpha", "idle")], T0);
        assert_eq!(
            reader(&path).sessions("proj").unwrap(),
            vec![row("alpha", "idle")]
        );
        assert!(
            !data_version_changes(&path, || p.publish(vec![row("alpha", "idle")], T0 + 1)),
            "an unchanged row costs no write"
        );
        assert!(data_version_changes(&path, || p.publish(vec![row("alpha", "busy")], T0 + 2)));
        p.publish(vec![], T0 + 3);
        assert!(reader(&path).sessions("proj").unwrap().is_empty());
    }

    #[test]
    fn heartbeat_refreshes_the_lease_and_writes_link_and_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.db");
        let p = StatusPublisher::open(path.clone(), me("a"), T0);
        p.set_link(HerdrLink::Up, T0 + 1);
        let w = reader(&path).writer().unwrap().unwrap();
        assert_eq!((w.herdr_link.as_str(), w.herdr_link_since), ("up", T0 + 1));
        p.set_last_snapshot(T0 + 2);
        p.beat(T0 + 5_000);
        let w = reader(&path).writer().unwrap().unwrap();
        assert_eq!(w.heartbeat_at, T0 + 5_000);
        assert_eq!(w.last_snapshot_at, Some(T0 + 2));
        assert_eq!(w.instance_id, "a");
    }

    #[test]
    fn a_lease_loser_writes_nothing_until_the_holder_goes_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.db");
        let a = StatusPublisher::open(path.clone(), me("a"), T0);
        a.publish(vec![row("alpha", "idle")], T0);
        let b = StatusPublisher::open(path.clone(), me("b"), T0 + 1_000);
        assert!(matches!(b.lease(), LeaseState::Standby { pid: 7, .. }));
        assert!(!data_version_changes(&path, || {
            b.publish(vec![row("alpha", "busy")], T0 + 1_000);
            b.set_link(HerdrLink::Down, T0 + 1_000);
            b.findings_apply(&[FindingWrite::Upsert(finding(T0))]);
            b.beat(T0 + 5_000);
        }));
        // The holder stops beating: the standby takes over and writes its own rows.
        b.beat(T0 + 16_000);
        assert_eq!(b.lease(), LeaseState::Owned);
        let r = reader(&path);
        assert_eq!(r.writer().unwrap().unwrap().instance_id, "b");
        assert_eq!(r.writer().unwrap().unwrap().herdr_link, "down");
        assert_eq!(r.sessions("proj").unwrap(), vec![row("alpha", "busy")]);
        // The old holder notices on its next beat and stands by.
        a.beat(T0 + 17_000);
        assert!(matches!(a.lease(), LeaseState::Standby { .. }));
        assert!(!data_version_changes(&path, || a.publish(vec![row("alpha", "idle")], T0 + 17_000)));
    }

    fn finding(last_seen: i64) -> FindingRow {
        FindingRow {
            project: "proj".into(),
            worktree: "alpha".into(),
            check_id: "hooks_missing".into(),
            severity: "warn".into(),
            message: "m".into(),
            detail: "{}".into(),
            first_seen: T0,
            last_seen,
            occurrences: 1,
            cleared_at: None,
            remedy: String::new(),
            remedy_at: None,
        }
    }

    #[test]
    fn queued_last_seen_and_touches_flush_with_the_beat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.db");
        let p = StatusPublisher::open(path.clone(), me("a"), T0);
        p.findings_apply(&[FindingWrite::Upsert(finding(T0))]);
        p.app_project_set(&AppProject {
            project: "proj".into(),
            registry_key: "proj".into(),
            db_path: "D:/repos/proj/tasks.db".into(),
            open: true,
            open_reported_at: None,
            watcher_state: "running".into(),
            watcher_started_at: None,
            watcher_last_event_at: None,
            last_pass_at: None,
            last_pass_trigger: None,
            pending_count: 0,
            oldest_pending_at: None,
            restarts: 0,
            last_error: String::new(),
            last_error_at: None,
        });
        let key = FindingKey {
            project: "proj".into(),
            worktree: "alpha".into(),
            check_id: "hooks_missing".into(),
        };
        let r = reader(&path);
        assert!(!data_version_changes(&path, || {
            p.findings_apply(&[FindingWrite::LastSeen {
                key: key.clone(),
                last_seen: T0 + 60_000,
            }]);
            p.app_project_touch(
                "proj",
                AppProjectTouch {
                    last_pass_at: Some(T0 + 10),
                    ..Default::default()
                },
            );
        }));
        assert_eq!(r.findings("proj", None).unwrap()[0].last_seen, T0);
        p.beat(T0 + 5_000);
        assert_eq!(r.findings("proj", None).unwrap()[0].last_seen, T0 + 60_000);
        assert_eq!(
            r.app_project("proj").unwrap().unwrap().last_pass_at,
            Some(T0 + 10)
        );
        // Flushed once: the next beat only touches the heartbeat.
        p.findings_apply(&[FindingWrite::Delete(key)]);
        p.beat(T0 + 10_000);
        assert!(r.findings("proj", None).unwrap().is_empty());
    }

    #[test]
    fn a_corrupt_file_is_recreated_at_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.db");
        std::fs::write(&path, b"this is not a sqlite database, not at all").unwrap();
        let p = StatusPublisher::open(path.clone(), me("a"), T0);
        assert_eq!(p.lease(), LeaseState::Owned);
        p.publish(vec![row("alpha", "idle")], T0);
        assert_eq!(reader(&path).sessions("proj").unwrap().len(), 1);
    }

    #[test]
    fn release_gives_the_lease_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.db");
        let p = StatusPublisher::open(path.clone(), me("a"), T0);
        p.release();
        assert_eq!(reader(&path).writer().unwrap(), None);
        assert_eq!(p.lease(), LeaseState::Closed);
    }

    #[test]
    fn start_names_a_fresh_instance_and_beats_on_its_own_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.db");
        let p = StatusPublisher::start(path.clone(), "9.9.9");
        let w = reader(&path).writer().unwrap().unwrap();
        assert_eq!(w.version, "9.9.9");
        assert_eq!(w.pid, i64::from(std::process::id()));
        assert!(w
            .instance_id
            .starts_with(&format!("{:x}-", std::process::id())));
        p.release();
    }
}
