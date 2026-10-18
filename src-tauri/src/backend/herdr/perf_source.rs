//! `perf_source` — the performance overlay's view of herdr: which panes belong to
//! sessions, each pane's process roots, the server pid and the headroom mode.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::activity::lock;
use crate::backend::SessionKey;
use crate::perf::{PerfPane, PerfSource};

use super::cli::{PaneInfo, PaneProcessInfo};
use super::keymap::{HerdrPane, KeyMap};
use super::ops::{Generation, HerdrError, HerdrOps};
use super::status_loop::{self, SharedStatusCache};

/// [`PerfSource`] over the herdr backend's ops, keymap and status cache.
pub struct HerdrPerfSource {
    ops: Arc<dyn HerdrOps>,
    keymap: Arc<KeyMap>,
    status_cache: SharedStatusCache,
    /// When the pane list was last asked for, and what it gave.
    last_list: Mutex<Option<(Instant, Vec<PaneInfo>)>>,
}

impl HerdrPerfSource {
    pub fn new(
        ops: Arc<dyn HerdrOps>,
        keymap: Arc<KeyMap>,
        status_cache: SharedStatusCache,
    ) -> Self {
        HerdrPerfSource {
            ops,
            keymap,
            status_cache,
            last_list: Mutex::new(None),
        }
    }

    /// How long a pane list is reused before it is asked for again: the status
    /// loop's session-listing ages, so a down herdr is not asked every step.
    fn list_every(&self) -> Duration {
        if self.low_headroom() {
            status_loop::LOW_SESSIONS_MAX_AGE
        } else {
            status_loop::SESSIONS_MAX_AGE
        }
    }

    /// [`PerfSource::panes`] at `now`. The session listing is asked at most once
    /// per [`Self::list_every`], whether or not the last ask succeeded; in
    /// between the last answer is reused (re-joined to the current keymap).
    fn panes_at(&self, now: Instant) -> Vec<PerfPane> {
        let current = self.keymap.generation();
        let entries = self.keymap.entries();
        let every = self.list_every();
        let mut last = lock(&self.last_list);
        let fresh = match last.as_ref() {
            Some((at, list)) if now.saturating_duration_since(*at) < every => list.clone(),
            _ => {
                let list =
                    status_loop::session_panes(&*self.ops, &self.keymap, &self.status_cache, now)
                        .unwrap_or_else(|_| {
                            status_loop::cached_session_panes(&self.status_cache)
                                .unwrap_or_default()
                        });
                *last = Some((now, list.clone()));
                list
            }
        };
        drop(last);
        join_panes(&entries, current, &fresh)
    }
}

impl PerfSource for HerdrPerfSource {
    /// The session listing's pane list (asked for at most every few seconds;
    /// see [`HerdrPerfSource::panes_at`]), kept to the workspaces a
    /// current-generation keymap entry owns. A failed list falls back to the
    /// last cached one.
    fn panes(&self) -> Vec<PerfPane> {
        self.panes_at(Instant::now())
    }

    fn pane_process_info(&self, pane_id: &str) -> Result<PaneProcessInfo, HerdrError> {
        self.ops.pane_process_info(pane_id)
    }

    fn server_pid(&self, sys: &mut sysinfo::System) -> Option<u32> {
        self.ops.server_pid(sys)
    }

    fn generation(&self) -> u64 {
        self.keymap.generation().0
    }

    fn low_headroom(&self) -> bool {
        lock(&self.status_cache).low
    }
}

/// Every listed pane whose workspace a keymap entry of the `current` server
/// generation owns, joined to that entry's session (in list order). An agent may
/// split extra panes into a session's workspace, so panes are matched by
/// workspace, not by the keymap's own pane id.
fn join_panes(
    entries: &[(SessionKey, HerdrPane)],
    current: Generation,
    list: &[PaneInfo],
) -> Vec<PerfPane> {
    let session_of: HashMap<&str, &SessionKey> = entries
        .iter()
        .filter(|(_, pane)| pane.server_generation == current)
        .map(|(key, pane)| (pane.workspace_id.as_str(), key))
        .collect();
    list.iter()
        .filter_map(|p| {
            let session = session_of.get(p.workspace_id.as_str())?;
            Some(PerfPane {
                pane_id: p.pane_id.clone(),
                workspace_id: p.workspace_id.clone(),
                session: (*session).clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::herdr::fake::FakeHerdr;
    use crate::backend::herdr::ops::Op;
    use crate::backend::herdr::status_loop::StatusCache;

    fn key(project: &str, worktree: &str) -> SessionKey {
        SessionKey {
            project: project.into(),
            worktree: worktree.into(),
        }
    }

    fn source(fake: &Arc<FakeHerdr>, keymap: &Arc<KeyMap>) -> HerdrPerfSource {
        let ops: Arc<dyn HerdrOps> = fake.clone();
        HerdrPerfSource::new(
            ops,
            Arc::clone(keymap),
            Arc::new(Mutex::new(StatusCache::default())),
        )
    }

    #[test]
    fn panes_keep_only_current_generation_workspaces() {
        let fake = FakeHerdr::new();
        let keymap = Arc::new(KeyMap::new());
        let stale = fake.add_pane("C:/Users/x/proj/bravo", None);
        keymap.record(
            key("proj", "bravo"),
            stale.workspace_id.clone(),
            stale.pane_id.clone(),
            stale.terminal_id.clone(),
            "C:/Users/x/proj/bravo".into(),
        );
        keymap.set_generation(Generation(1));
        let live = fake.add_pane("C:/Users/x/proj/alpha", None);
        keymap.record(
            key("proj", "alpha"),
            live.workspace_id.clone(),
            live.pane_id.clone(),
            live.terminal_id.clone(),
            "C:/Users/x/proj/alpha".into(),
        );
        // A pane no session owns.
        fake.add_pane("C:/Users/x/other-proj/main", None);

        let src = source(&fake, &keymap);
        assert_eq!(
            src.panes(),
            vec![PerfPane {
                pane_id: live.pane_id.clone(),
                workspace_id: live.workspace_id.clone(),
                session: key("proj", "alpha"),
            }]
        );
        assert_eq!(src.generation(), 1);
    }

    #[test]
    fn panes_reuse_the_cached_pane_list() {
        let fake = FakeHerdr::new();
        let keymap = Arc::new(KeyMap::new());
        let src = source(&fake, &keymap);
        src.panes();
        src.panes();
        let lists = fake.calls().iter().filter(|c| c.op == Op::PaneList).count();
        assert_eq!(lists, 1, "the second call is served from the status cache");
    }

    #[test]
    fn a_failing_pane_list_is_asked_at_most_once_per_interval() {
        let fake = FakeHerdr::new();
        let keymap = Arc::new(KeyMap::new());
        let src = source(&fake, &keymap);
        let lists = || fake.calls().iter().filter(|c| c.op == Op::PaneList).count();
        let t0 = Instant::now();

        fake.fail_next(Op::PaneList, HerdrError::Timeout);
        assert!(src.panes_at(t0).is_empty());
        assert_eq!(lists(), 1);
        assert!(src.panes_at(t0 + Duration::from_secs(1)).is_empty());
        assert!(src
            .panes_at(t0 + status_loop::SESSIONS_MAX_AGE - Duration::from_millis(1))
            .is_empty());
        assert_eq!(
            lists(),
            1,
            "the failure is reused, herdr is not asked again"
        );

        let t1 = t0 + status_loop::SESSIONS_MAX_AGE;
        fake.fail_next(Op::PaneList, HerdrError::Timeout);
        src.panes_at(t1);
        assert_eq!(lists(), 2, "asked again once the interval has passed");

        // Low headroom stretches the interval.
        lock(&src.status_cache).low = true;
        src.panes_at(t1 + status_loop::SESSIONS_MAX_AGE);
        src.panes_at(t1 + status_loop::LOW_SESSIONS_MAX_AGE - Duration::from_millis(1));
        assert_eq!(lists(), 2);
        src.panes_at(t1 + status_loop::LOW_SESSIONS_MAX_AGE);
        assert_eq!(lists(), 3);
    }

    #[test]
    fn panes_between_asks_rejoin_the_last_list_to_the_keymap() {
        let fake = FakeHerdr::new();
        let keymap = Arc::new(KeyMap::new());
        let src = source(&fake, &keymap);
        let live = fake.add_pane("C:/Users/x/proj/alpha", None);
        let t0 = Instant::now();
        assert!(src.panes_at(t0).is_empty(), "no session owns the pane yet");

        keymap.record(
            key("proj", "alpha"),
            live.workspace_id.clone(),
            live.pane_id.clone(),
            live.terminal_id.clone(),
            "C:/Users/x/proj/alpha".into(),
        );
        assert_eq!(
            src.panes_at(t0 + Duration::from_secs(1)),
            vec![PerfPane {
                pane_id: live.pane_id.clone(),
                workspace_id: live.workspace_id.clone(),
                session: key("proj", "alpha"),
            }]
        );
        let lists = fake.calls().iter().filter(|c| c.op == Op::PaneList).count();
        assert_eq!(lists, 1);
    }

    #[test]
    fn low_headroom_survives_a_poisoned_status_cache() {
        let fake = FakeHerdr::new();
        let keymap = Arc::new(KeyMap::new());
        let src = source(&fake, &keymap);
        lock(&src.status_cache).low = true;
        let cache = Arc::clone(&src.status_cache);
        let _ = std::thread::spawn(move || {
            let _held = cache.lock().unwrap();
            panic!("poison the status cache");
        })
        .join();
        assert!(src.status_cache.is_poisoned());
        assert!(src.low_headroom());
    }

    #[test]
    fn join_panes_takes_every_pane_of_an_owned_workspace() {
        let entries = vec![
            (
                key("proj", "alpha"),
                HerdrPane {
                    workspace_id: "w1".into(),
                    pane_id: "w1:p1".into(),
                    terminal_id: "term_1".into(),
                    gen: 1,
                    cwd: "C:/Users/x/proj/alpha".into(),
                    server_generation: Generation(2),
                },
            ),
            (
                key("proj", "bravo"),
                HerdrPane {
                    workspace_id: "w2".into(),
                    pane_id: "w2:p1".into(),
                    terminal_id: "term_2".into(),
                    gen: 1,
                    cwd: "C:/Users/x/proj/bravo".into(),
                    server_generation: Generation(1),
                },
            ),
        ];
        let pane = |ws: &str, id: &str| PaneInfo {
            workspace_id: ws.into(),
            pane_id: id.into(),
            ..PaneInfo::default()
        };
        let list = vec![
            pane("w1", "w1:p1"),
            pane("w2", "w2:p1"),
            pane("w1", "w1:p2"),
            pane("w3", "w3:p1"),
        ];
        let got = join_panes(&entries, Generation(2), &list);
        assert_eq!(
            got,
            vec![
                PerfPane {
                    pane_id: "w1:p1".into(),
                    workspace_id: "w1".into(),
                    session: key("proj", "alpha"),
                },
                PerfPane {
                    pane_id: "w1:p2".into(),
                    workspace_id: "w1".into(),
                    session: key("proj", "alpha"),
                },
            ]
        );
        assert!(join_panes(&entries, Generation(3), &list).is_empty());
    }

    #[test]
    fn process_info_server_pid_and_headroom_pass_through() {
        let fake = FakeHerdr::new();
        let keymap = Arc::new(KeyMap::new());
        let src = source(&fake, &keymap);
        let info = PaneProcessInfo {
            shell_pid: 4100,
            foreground_pids: vec![4200],
        };
        fake.set_process_info("w1:p1", info.clone());
        assert_eq!(src.pane_process_info("w1:p1"), Ok(info));
        assert!(src.pane_process_info("w9:p9").unwrap_err().is_not_found());
        assert_eq!(src.server_pid(&mut sysinfo::System::new()), Some(1000));
        assert!(!src.low_headroom());
        lock(&src.status_cache).low = true;
        assert!(src.low_headroom());
    }
}
