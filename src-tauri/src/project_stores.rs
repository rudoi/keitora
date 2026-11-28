//! Shared, cached views of the project registry and of each project's `tasks.db`.
//!
//! - [`registry`]: the registered projects (registry key + effective config) and the
//!   active project, read from the user config at most once per [`REGISTRY_TTL`] (or
//!   right after [`invalidate_registry`]) instead of once per caller and event.
//! - [`ReadStores`]: one long-lived read-only `LocalStore` per DB path, shared by the
//!   session engine's evidence, the rail's drive reads, the background notifier and
//!   the drive pass. Handles of DBs no longer in the registry are dropped whenever the
//!   registry is re-read.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use crate::locks::lock;
use std::time::{Duration, Instant};

use keitora_tasks::{LocalStore, ProjectConfig, UserConfig};

/// How long a registry snapshot is reused before the user config is read again.
pub const REGISTRY_TTL: Duration = Duration::from_secs(5);

// ── Registry snapshot ───────────────────────────────────────────────────────────

/// The registered projects and the active one, as of one user-config read.
#[derive(Debug, Default)]
pub struct Registry {
    /// `(registry key, effective config)` of every project worth watching; see
    /// [`watched_projects`].
    pub projects: Vec<(String, ProjectConfig)>,
    /// The active project's registry key.
    pub active: Option<String>,
}

impl Registry {
    pub fn from_user_config(cfg: &UserConfig) -> Self {
        Registry {
            projects: watched_projects(cfg),
            active: cfg.active.clone(),
        }
    }

    /// The project whose resolved `[project].name` is `name`.
    pub fn by_name(&self, name: &str) -> Option<&ProjectConfig> {
        self.projects
            .iter()
            .map(|(_, pc)| pc)
            .find(|pc| pc.project.name == name)
    }

    /// Whether `key` (a registry key) is the active project.
    pub fn is_active(&self, key: &str) -> bool {
        self.active.as_deref() == Some(key)
    }

    fn db_paths(&self) -> HashSet<PathBuf> {
        self.projects
            .iter()
            .map(|(_, pc)| pc.tasks.db_path.clone())
            .collect()
    }
}

/// The registered projects to watch, as `(registry key, effective config)`.
/// Uses `effective_config`, not `ProjectConfig::load`, so a project registered
/// without a committed `keitora.toml` counts too (its DB is the scaffold default).
/// An entry whose root is gone is stale and skipped, as is one whose config can't be
/// resolved.
pub fn watched_projects(user_cfg: &UserConfig) -> Vec<(String, ProjectConfig)> {
    user_cfg
        .projects
        .iter()
        .filter(|(_, pref)| pref.path.is_dir())
        .filter_map(|(name, pref)| {
            keitora_tasks::effective_config(pref)
                .ok()
                .map(|pc| (name.clone(), pc))
        })
        .collect()
}

#[derive(Default)]
struct RegistryCache {
    snapshot: Option<(Arc<Registry>, Instant)>,
    /// Bumped by every invalidation, so a read that raced one is not cached.
    generation: u64,
}

static REGISTRY: Mutex<RegistryCache> = Mutex::new(RegistryCache {
    snapshot: None,
    generation: 0,
});

/// The current registry snapshot (re-read at most once per [`REGISTRY_TTL`]). An
/// unreadable user config reads as no projects. A re-read also drops the shared
/// store handles of DBs that left the registry.
pub fn registry() -> Arc<Registry> {
    let generation = {
        let cache = lock(&REGISTRY);
        if let Some((snap, at)) = &cache.snapshot {
            if at.elapsed() < REGISTRY_TTL {
                return Arc::clone(snap);
            }
        }
        cache.generation
    };
    // Config I/O runs with no lock held.
    let fresh = Arc::new(
        UserConfig::load()
            .map(|cfg| Registry::from_user_config(&cfg))
            .unwrap_or_default(),
    );
    let keep = fresh.db_paths();
    shared().retain(|p| keep.contains(p));
    let mut cache = lock(&REGISTRY);
    if cache.generation == generation {
        cache.snapshot = Some((Arc::clone(&fresh), Instant::now()));
    }
    fresh
}

/// Forget the registry snapshot: the next [`registry`] call re-reads the user config.
/// Called wherever the registry or the active project changes.
pub fn invalidate_registry() {
    let mut cache = lock(&REGISTRY);
    cache.snapshot = None;
    cache.generation = cache.generation.wrapping_add(1);
}

// ── Shared read-only stores ─────────────────────────────────────────────────────

/// One cached read-only handle.
pub struct ReadStore {
    pub store: LocalStore,
    /// Unique per opened connection: a reopened handle never matches an earlier id,
    /// so a caller's cached `data_version` from the old connection is never reused.
    pub id: u64,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Read-only handles keyed by DB path, each behind its own lock (the map lock is only
/// held to look a handle up).
#[derive(Default)]
pub struct ReadStores {
    entries: Mutex<HashMap<PathBuf, Arc<Mutex<ReadStore>>>>,
}

static SHARED: LazyLock<Arc<ReadStores>> = LazyLock::new(Arc::default);

/// The process-wide store cache.
pub fn shared() -> Arc<ReadStores> {
    Arc::clone(&SHARED)
}

impl ReadStores {
    /// Run `f` over the cached handle of `path`, opening it read-only (never creating
    /// the file) when there is none. A missing file or an error from opening or from
    /// `f` drops the handle, so the next call reopens it (the file may have been
    /// replaced).
    pub fn with<T>(
        &self,
        path: &Path,
        f: impl FnOnce(&ReadStore) -> keitora_tasks::Result<T>,
    ) -> keitora_tasks::Result<T> {
        if !path.exists() {
            lock(&self.entries).remove(path);
            return Err(keitora_tasks::Error::Config(format!(
                "{} does not exist",
                path.display()
            )));
        }
        let entry = {
            let mut entries = lock(&self.entries);
            match entries.get(path) {
                Some(e) => Arc::clone(e),
                None => {
                    let store = LocalStore::open_read_only(&path.to_string_lossy())?;
                    let e = Arc::new(Mutex::new(ReadStore {
                        store,
                        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                    }));
                    entries.insert(path.to_path_buf(), Arc::clone(&e));
                    e
                }
            }
        };
        let r = f(&lock(&entry));
        if r.is_err() {
            let mut entries = lock(&self.entries);
            if entries.get(path).is_some_and(|e| Arc::ptr_eq(e, &entry)) {
                entries.remove(path);
            }
        }
        r
    }

    /// Drop the handles of paths `keep` rejects.
    pub fn retain(&self, keep: impl Fn(&Path) -> bool) {
        lock(&self.entries).retain(|p, _| keep(p));
    }

    /// Whether a handle for `path` is cached.
    pub fn contains(&self, path: &Path) -> bool {
        lock(&self.entries).contains_key(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_in(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        LocalStore::open(&p.to_string_lossy()).unwrap();
        p
    }

    #[test]
    fn one_handle_per_path_reused_until_an_error_or_eviction() {
        let tmp = tempfile::tempdir().unwrap();
        let a = db_in(tmp.path(), "a.db");
        let b = db_in(tmp.path(), "b.db");
        let stores = ReadStores::default();

        let id_a = stores.with(&a, |s| Ok(s.id)).unwrap();
        assert_eq!(
            stores.with(&a, |s| Ok(s.id)).unwrap(),
            id_a,
            "handle reused"
        );
        let id_b = stores.with(&b, |s| Ok(s.id)).unwrap();
        assert_ne!(id_a, id_b);

        // An error from the read drops the handle; the next call reopens it.
        let err: keitora_tasks::Result<()> =
            stores.with(&a, |_| Err(keitora_tasks::Error::Config("boom".into())));
        assert!(err.is_err());
        assert!(!stores.contains(&a));
        let reopened = stores.with(&a, |s| Ok(s.id)).unwrap();
        assert_ne!(reopened, id_a, "a reopened handle gets a new id");

        // Eviction keeps only the paths asked for.
        stores.retain(|p| p == b.as_path());
        assert!(!stores.contains(&a));
        assert!(stores.contains(&b));
    }

    #[test]
    fn a_missing_db_is_never_created_and_drops_its_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.db");
        let stores = ReadStores::default();
        assert!(stores.with(&missing, |_| Ok(())).is_err());
        assert!(!missing.exists());
        assert!(!stores.contains(&missing));
    }

    #[test]
    fn registry_finds_projects_by_resolved_name_and_knows_the_active_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("keitora.toml"),
            "[project]\nname = \"my-game\"\n\n[tasks]\ndb_path = \"tasks.db\"\n",
        )
        .unwrap();
        let mut cfg = UserConfig::default();
        cfg.register("proj", root);
        cfg.set_active("proj");

        let reg = Registry::from_user_config(&cfg);
        assert_eq!(reg.by_name("my-game").unwrap().project.name, "my-game");
        assert!(reg.by_name("proj").is_none(), "looked up by resolved name");
        assert!(reg.is_active("proj"));
        assert!(!reg.is_active("my-game"));
    }
}
