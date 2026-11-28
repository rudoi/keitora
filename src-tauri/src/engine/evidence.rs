//! The app's [`Evidence`]: each registered project's hook rows, read through the
//! shared read-only `LocalStore` of its `tasks.db` and reused while the DB's
//! `data_version` holds.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use keitora_tasks::LocalStore;

use super::model::{DriveEvidence, HookEvidence, ProjectEvidence};
use super::Evidence;
use crate::project_stores::ReadStores;

/// A project's `tasks.db` path and configured worktrees, looked up by its resolved
/// `[project].name`.
pub type ProjectLookup = Box<dyn Fn(&str) -> Option<(String, Vec<String>)> + Send + Sync>;

/// A project's `tasks.db` path and configured worktrees.
type Located = (String, Vec<String>);

/// How long a registry lookup is reused before the registry is read again.
const LOOKUP_TTL: Duration = Duration::from_secs(10);

/// What one DB contributes to a project's evidence.
#[derive(Clone, Default)]
struct DbEvidence {
    rows: HashMap<String, HookEvidence>,
    drive: Arc<DriveEvidence>,
}

/// One DB's evidence as last read, with the handle and `data_version` it was read at.
struct CachedRows {
    store_id: u64,
    version: i64,
    rows: DbEvidence,
}

/// Registry-backed evidence. See the module docs.
pub struct RegistryEvidence {
    lookup: ProjectLookup,
    /// Resolved project name -> (db path, configured worktrees, when looked up).
    projects: Mutex<HashMap<String, (Option<Located>, Instant)>>,
    /// Read-only handles by `tasks.db` path.
    stores: Arc<ReadStores>,
    /// Last read rows by `tasks.db` path; dropped with their handle.
    rows: Mutex<HashMap<String, CachedRows>>,
    /// Full row reads performed; lets tests observe the skip.
    #[cfg(test)]
    reads: std::sync::atomic::AtomicUsize,
}

impl RegistryEvidence {
    /// Evidence over the user's project registry and the shared store cache.
    pub fn new() -> Self {
        Self::with_parts(Box::new(registry_lookup), crate::project_stores::shared())
    }

    /// Evidence over an injected project lookup and a private store cache.
    #[cfg(test)]
    pub fn with_lookup(lookup: ProjectLookup) -> Self {
        Self::with_parts(lookup, Arc::default())
    }

    fn with_parts(lookup: ProjectLookup, stores: Arc<ReadStores>) -> Self {
        RegistryEvidence {
            lookup,
            projects: Mutex::new(HashMap::new()),
            stores,
            rows: Mutex::new(HashMap::new()),
            #[cfg(test)]
            reads: Default::default(),
        }
    }

    fn resolve(&self, project: &str) -> Option<(String, Vec<String>)> {
        let now = Instant::now();
        let mut projects = self.projects.lock().unwrap();
        if let Some((hit, at)) = projects.get(project) {
            if now.saturating_duration_since(*at) < LOOKUP_TTL {
                return hit.clone();
            }
        }
        let hit = (self.lookup)(project);
        projects.insert(project.to_string(), (hit.clone(), now));
        hit
    }

    /// `db_path`'s evidence: the cached one while the handle and its `data_version`
    /// are unchanged, else a fresh read.
    fn rows_of(&self, db_path: &str) -> keitora_tasks::Result<DbEvidence> {
        let mut cache = self.rows.lock().unwrap();
        let r = self.stores.with(Path::new(db_path), |rs| {
            let version = rs.store.data_version()?;
            if let Some(c) = cache.get(db_path) {
                if c.store_id == rs.id && c.version == version {
                    return Ok(c.rows.clone());
                }
            }
            #[cfg(test)]
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let rows = read_rows(&rs.store)?;
            cache.insert(
                db_path.to_string(),
                CachedRows {
                    store_id: rs.id,
                    version,
                    rows: rows.clone(),
                },
            );
            Ok(rows)
        });
        // Rows whose handle is gone (failed, missing, or left the registry) go too.
        cache.retain(|p, _| self.stores.contains(Path::new(p)));
        r
    }
}

impl Evidence for RegistryEvidence {
    fn project(&self, project: &str) -> Option<ProjectEvidence> {
        let (db_path, configured) = self.resolve(project)?;
        let db = match self.rows_of(&db_path) {
            Ok(db) => db,
            Err(e) => {
                log::debug!("engine: no hook rows for {project} ({db_path}): {e}");
                return None;
            }
        };
        Some(ProjectEvidence {
            db_path,
            configured,
            rows: db.rows,
            drive: db.drive,
        })
    }
}

/// One DB's evidence: the hook rows keyed by worktree, and the drive state (all
/// empty on a DB without the drive tables or the delivery columns).
fn read_rows(store: &LocalStore) -> keitora_tasks::Result<DbEvidence> {
    let ids = store.session_ids()?;
    let rows = store
        .sessions_list()?
        .into_iter()
        .map(|s| {
            let ev = HookEvidence {
                written_ms: keitora_tasks::sqlite_utc_to_ms(&s.last_heartbeat),
                session_id: ids.get(&s.worktree).cloned(),
                state: s.state,
                brokered: s.brokered,
                escalated_reason: s.escalated_reason,
            };
            (s.worktree, ev)
        })
        .collect();
    let drive = DriveEvidence {
        deliveries: store.drive_deliveries()?,
        driven: store.drives_list()?.into_iter().map(|d| d.driven).collect(),
        replies: store
            .drive_session_replies()?
            .into_iter()
            .map(|r| (r.worktree.clone(), r))
            .collect(),
    };
    Ok(DbEvidence {
        rows,
        drive: Arc::new(drive),
    })
}

/// Find `project` (a resolved `[project].name`) in the cached registry snapshot.
fn registry_lookup(project: &str) -> Option<(String, Vec<String>)> {
    crate::project_stores::registry()
        .by_name(project)
        .map(|pc| {
            (
                pc.tasks.db_path.to_string_lossy().to_string(),
                pc.worktrees(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn evidence_over(db: &str, lookups: Arc<AtomicU32>) -> RegistryEvidence {
        let db = db.to_string();
        RegistryEvidence::with_lookup(Box::new(move |p| {
            lookups.fetch_add(1, Ordering::SeqCst);
            (p == "proj").then(|| (db.clone(), vec!["alpha".into(), "bravo".into()]))
        }))
    }

    #[test]
    fn reads_hook_rows_with_their_write_time() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("tasks.db");
        let db = db.to_string_lossy().to_string();
        let w = LocalStore::open(&db).unwrap();
        w.session_upsert("alpha", "busy", None, None).unwrap();

        let lookups = Arc::new(AtomicU32::new(0));
        let ev = evidence_over(&db, Arc::clone(&lookups));
        let p = ev.project("proj").expect("evidence");
        assert_eq!(p.db_path, db);
        assert_eq!(p.configured, vec!["alpha".to_string(), "bravo".to_string()]);
        let row = &p.rows["alpha"];
        assert_eq!(row.state, "busy");
        assert!(row.written_ms.is_some_and(|ms| ms > 0));
        assert_eq!(row.session_id, None, "an id-less write carries no session");
        assert!(!row.brokered);

        // A later write is seen through the cached handle; the registry is not read again.
        w.session_upsert("bravo", "waiting", None, None).unwrap();
        let p = ev.project("proj").unwrap();
        assert_eq!(p.rows["bravo"].state, "waiting");
        assert_eq!(lookups.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unknown_project_or_missing_db_has_no_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.db").to_string_lossy().to_string();
        let ev = evidence_over(&missing, Arc::default());
        assert!(ev.project("other-proj").is_none());
        assert!(
            ev.project("proj").is_none(),
            "a missing DB is never created"
        );
        assert!(!dir.path().join("nope.db").exists());
    }

    #[test]
    fn unchanged_db_reuses_its_rows_and_a_write_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("tasks.db").to_string_lossy().to_string();
        let w = LocalStore::open(&db).unwrap();
        w.session_upsert("alpha", "busy", None, None).unwrap();

        let ev = evidence_over(&db, Arc::default());
        let reads = |ev: &RegistryEvidence| ev.reads.load(Ordering::SeqCst);
        assert_eq!(ev.project("proj").unwrap().rows["alpha"].state, "busy");
        assert_eq!(reads(&ev), 1);
        // Nothing written: the cached rows come back without a read.
        assert_eq!(ev.project("proj").unwrap().rows["alpha"].state, "busy");
        assert_eq!(reads(&ev), 1, "no re-read while data_version holds");
        // A write from another connection is read.
        w.session_upsert("alpha", "waiting", None, None).unwrap();
        assert_eq!(ev.project("proj").unwrap().rows["alpha"].state, "waiting");
        assert_eq!(reads(&ev), 2);
    }
}
