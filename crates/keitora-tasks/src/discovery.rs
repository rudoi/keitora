//! Worktree discovery for onboarding.
//!
//! Lists a project's git worktrees (`git worktree list --porcelain`) so the UI
//! can offer them in a multiselect, and writes the chosen set back into the
//! project `keitora.toml` — surgically, via `toml_edit`, so the committed
//! relative `db_path`, the other fields, and formatting are preserved. (We must
//! NOT serialize `ProjectConfig` back: its `db_path` is canonicalized to an
//! absolute path on load, which must never be written to the committed file.)

use std::path::{Path, PathBuf};
// Only the `#[cfg(test)]` git helpers spawn `git` directly; production code goes
// through `crate::config::git_command()`.
#[cfg(test)]
use std::process::Command;

use crate::config::{
    canonical_key, classify_worktree, effective_config, git_user_slug, reorder_entries,
    ProjectConfig, UserConfig, WorktreeEntry,
};
use crate::model::{ResolvedWorktree, WorktreeCandidate};
use crate::{Error, Result};

/// Parse `git worktree list --porcelain` output into candidates. Pure (no I/O),
/// so it is unit-testable. `tracked` is the project's current `[tasks].worktrees`
/// used to set `already_tracked`.
///
/// Porcelain records are blank-line separated; each begins with `worktree <path>`
/// followed by `HEAD <sha>` and either `branch refs/heads/<name>` or `detached`.
pub fn parse_worktree_porcelain(output: &str, tracked: &[String]) -> Vec<WorktreeCandidate> {
    let mut out = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;

    // Flush the record accumulated so far (if any) into `out`.
    let mut flush = |path: &mut Option<PathBuf>, branch: &mut Option<String>| {
        if let Some(p) = path.take() {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let already_tracked = tracked.iter().any(|t| t == &name);
            out.push(WorktreeCandidate {
                name,
                path: p,
                branch: branch.take(),
                already_tracked,
            });
        } else {
            *branch = None;
        }
    };

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            // A new record starts — flush the previous one first.
            flush(&mut path, &mut branch);
            path = Some(PathBuf::from(rest.trim()));
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(
                rest.trim()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(rest.trim())
                    .to_string(),
            );
        }
        // `HEAD`, `detached`, `bare`, etc. carry no fields we surface.
    }
    flush(&mut path, &mut branch);
    out
}

/// Discover the git worktrees under `project_root` by shelling out to git.
pub fn discover_worktrees(
    project_root: &Path,
    tracked: &[String],
) -> Result<Vec<WorktreeCandidate>> {
    let out = crate::config::git_command()
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_root)
        .output()
        .map_err(|e| Error::Config(format!("running `git worktree list`: {e}")))?;
    if !out.status.success() {
        return Err(Error::Config(format!(
            "`git worktree list` failed in {}: {}",
            project_root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(parse_worktree_porcelain(&text, tracked))
}

/// Persist a project's worktree list (name + path entries). The per-machine
/// list lives in the user registry (`ProjectRef.worktrees`): for a registered
/// project we write there and the committed `keitora.toml` is left untouched. For an
/// unregistered project (fresh clone, not yet onboarded) we fall back to the
/// surgical committed-file rewrite so existing behavior is preserved.
///
/// A registered project keeps its current order: worktrees already listed stay where
/// they are, new ones follow in `entries` order, and ones absent from `entries` leave
/// the list. The current list is read under the registry lock, so a concurrent reorder
/// in this process is never undone.
pub fn set_project_worktrees(project_root: &Path, entries: &[WorktreeEntry]) -> Result<()> {
    let _registry = UserConfig::write_lock();
    let mut registry = UserConfig::load().map_err(|e| Error::Config(e.to_string()))?;
    let ordered = match registry.project_ref_by_root(project_root) {
        Some(pref) => {
            // The registry list when it has one; the committed file only otherwise.
            let current = match &pref.worktrees {
                Some(list) => list.clone(),
                None => effective_config(pref)
                    .map_err(|e| Error::Config(e.to_string()))?
                    .worktree_entries(),
            };
            let names: Vec<String> = current.into_iter().map(|e| e.name).collect();
            reorder_entries(entries, &names)
        }
        None => entries.to_vec(),
    };
    if registry
        .set_project_worktrees_by_root(project_root, &ordered)
        .map_err(|e| Error::Config(e.to_string()))?
    {
        return Ok(());
    }
    set_committed_worktrees(project_root, entries)
}

/// Write the worktree list of the project registered as `project` in the order `order`
/// (worktree names; may be partial, see [`reorder_entries`]). The list starts from the
/// project's effective entries, so a project whose list came from its committed file
/// gets that list, reordered, in the registry.
pub fn reorder_project_worktrees(project: &str, order: &[String]) -> Result<()> {
    let _registry = UserConfig::write_lock();
    let mut registry = UserConfig::load().map_err(|e| Error::Config(e.to_string()))?;
    let pref = registry
        .projects
        .get(project)
        .ok_or_else(|| Error::Config(format!("No project named '{project}' is registered.")))?;
    let current = effective_config(pref)
        .map_err(|e| Error::Config(e.to_string()))?
        .worktree_entries();
    let root = pref.path.clone();
    registry
        .set_project_worktrees_by_root(&root, &reorder_entries(&current, order))
        .map_err(|e| Error::Config(e.to_string()))?;
    Ok(())
}

/// Surgically set `[tasks].worktrees` in `<project_root>/keitora.toml` to `entries`,
/// preserving every other field (notably the relative `db_path`), comments, and
/// formatting. Uses `toml_edit`. Each entry with a `path` is written as an inline
/// table `{ name = "…", path = "…" }`; a path-less entry stays a bare string so a
/// legacy/unmigrated worktree round-trips unchanged. Fallback for unregistered
/// projects only.
fn set_committed_worktrees(project_root: &Path, entries: &[WorktreeEntry]) -> Result<()> {
    let file = project_root.join("keitora.toml");
    let text = std::fs::read_to_string(&file)
        .map_err(|e| Error::Config(format!("reading {}: {e}", file.display())))?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| Error::Config(format!("parsing {}: {e}", file.display())))?;

    let mut arr = toml_edit::Array::new();
    for e in entries {
        match &e.path {
            Some(path) => {
                let mut inline = toml_edit::InlineTable::new();
                inline.insert("name", e.name.as_str().into());
                inline.insert("path", path.to_string_lossy().as_ref().into());
                arr.push(toml_edit::Value::InlineTable(inline));
            }
            None => arr.push(e.name.as_str()),
        }
    }
    doc["tasks"]["worktrees"] = toml_edit::value(arr);

    std::fs::write(&file, doc.to_string())
        .map_err(|e| Error::Config(format!("writing {}: {e}", file.display())))
}

/// Create — or ADOPT — a git worktree for `name` and register it.
///
/// If `pc.worktree_path(name)` does not exist, a new linked worktree is created:
/// branches off the resolved base branch (see [`resolve_base_branch`]) into
/// `llm/<user-slug>/<project>/<name>`.
///
/// If the path already exists, it is ADOPTED **iff it is a linked git worktree of
/// this project** (skipping branch/`git worktree add`) — otherwise the call fails
/// (a foreign directory, or the main checkout). Adoption tracks the existing
/// worktree **as-is**: its checked-out branch is intentionally NOT required to be
/// the `llm/...` shape (the point is to track a worktree that already exists);
/// callers that assume that branch naming should not rely on it for adopted ones.
///
/// Either way `name` is appended to the effective worktree list (registry for a
/// registered project, else the committed `[tasks].worktrees`). Fails without
/// partial config writes on any validation error.
pub fn create_worktree(pc: &ProjectConfig, name: &str) -> Result<ResolvedWorktree> {
    // 1. Validate the name.
    if name.is_empty() {
        return Err(Error::Config("worktree name must not be empty".to_string()));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.starts_with('.') {
        return Err(Error::Config(format!(
            "invalid worktree name {name:?}: must not contain '/', '\\', '..', or start with '.'"
        )));
    }
    if pc.worktrees().iter().any(|w| w == name) {
        return Err(Error::Config(format!(
            "worktree {name:?} is already configured"
        )));
    }

    // 2. Decide CREATE vs ADOPT. If the target path already exists, it's only
    //    acceptable when it is itself a git worktree of THIS project — then we
    //    simply start tracking it (adopt). A directory that exists but is not a
    //    worktree of this project is a hard error (we won't touch a foreign dir).
    let path = pc.worktree_path(name);
    let adopting = if path.exists() {
        if is_project_worktree(&pc.root, &path)? {
            true
        } else {
            return Err(Error::Config(format!(
                "path already exists and is not a worktree of this project: {}",
                path.display()
            )));
        }
    } else {
        false
    };

    // 3–6. Create the linked worktree. Skipped entirely when adopting an
    //      already-existing worktree of this project.
    if !adopting {
        // 3. Resolve the base branch (config → origin/HEAD → main/master). The
        //    helper guarantees the returned ref exists.
        let base = resolve_base_branch(pc)?;

        // 4. The derived branch must not already exist.
        let branch = format!("llm/{}/{}/{}", git_user_slug(), pc.project.name, name);
        if git_ref_exists(&pc.root, &branch)? {
            return Err(Error::Config(format!("branch {branch} already exists")));
        }

        // 5. Ensure the worktree parent dir exists (e.g. `~/.keitora/<project>`).
        let parent = path.parent().ok_or_else(|| {
            Error::Config(format!("target path has no parent: {}", path.display()))
        })?;
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Config(format!("creating {}: {e}", parent.display())))?;

        // 6. Create the worktree + branch off the resolved base branch.
        let out = crate::config::git_command()
            .arg("-C")
            .arg(&pc.root)
            .args(["worktree", "add", "-b", &branch])
            .arg(&path)
            .arg(&base)
            .output()
            .map_err(|e| Error::Config(format!("running `git worktree add`: {e}")))?;
        if !out.status.success() {
            return Err(Error::Config(format!(
                "`git worktree add` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
    }

    // 7. Register the worktree ADDITIVELY. Re-read the CURRENT effective list
    //    fresh (the registry may have changed since `pc` was resolved — and the
    //    adopt path is reachable precisely when on-disk state and `pc` diverge),
    //    so persisting never clobbers worktrees added since. `set_project_worktrees`
    //    overwrites the stored list, so the merge has to happen here. The new entry
    //    carries the created/adopted PATH so it never needs reconstruction.
    //    Read via `effective_entries_for` (registry-aware, file-less safe): a
    //    registered project with no committed `keitora.toml` resolves through the
    //    registry's scaffold defaults instead of hard-failing on a missing file;
    //    the git worktree is already on disk by now, so a load error here
    //    would strand it untracked.
    let mut entries = effective_entries_for(&pc.root)?;
    if !entries.iter().any(|e| e.name == name) {
        entries.push(WorktreeEntry::new(name, path.clone()));
    }
    set_project_worktrees(&pc.root, &entries)?;

    // 8. Return the resolved + classified worktree. The freshly-created entry
    //    carries no explicit `cli`, so its resolved CLI is the user default.
    let user = UserConfig::load().unwrap_or_default();
    Ok(ResolvedWorktree {
        name: name.to_string(),
        path: path.clone(),
        status: classify_worktree(&path),
        cli: crate::config::resolve_cli(None, &user),
    })
}

/// Backfill missing worktree paths on a project's persisted entries from
/// `git worktree list`, then persist (migrate-on-write). The repair pass for the
/// Path-resolution fix: a config carried over from before path-carrying entries (or a
/// legacy bare-string list) has entries with `path: None`, which `worktree_path`
/// would resolve via `worktrees_dir` reconstruction; this stamps each with the
/// path git actually reports so resolution is authoritative.
///
/// - Loads the project's effective entries (registry-aware). If every entry
///   already has a path → `Ok(false)` (idempotent — activation never rewrites a
///   config that's already migrated).
/// - Runs `discover_worktrees` and builds a name → path map.
/// - Fills each `path: None` entry from the map; entries git no longer reports are
///   left `None` (we never INVENT a path from `worktrees_dir`).
/// - Persists via [`set_project_worktrees`] and returns `Ok(true)` when anything
///   changed.
pub fn repair_project_worktree_paths(project_root: &Path) -> Result<bool> {
    // Load the effective entries: registry-stamped when the project is registered,
    // else straight from the committed file (or scaffold defaults for a file-less
    // project). Mirrors how resolution would see the list.
    let mut entries = effective_entries_for(project_root)?;

    // Idempotent fast path: nothing to backfill once every entry has a path.
    if entries.iter().all(|e| e.path.is_some()) {
        return Ok(false);
    }

    // Map name → path from `git worktree list` (the only authority for an existing
    // worktree's location). `tracked` is unused for the map, so pass empty.
    let map: std::collections::HashMap<String, PathBuf> = discover_worktrees(project_root, &[])?
        .into_iter()
        .map(|c| (c.name, c.path))
        .collect();

    let mut changed = false;
    for e in entries.iter_mut() {
        if e.path.is_none() {
            if let Some(path) = map.get(&e.name) {
                e.path = Some(path.clone());
                changed = true;
            }
            // Still-unknown (git no longer reports it): leave None — do NOT invent
            // a path from `worktrees_dir`.
        }
    }

    if changed {
        persist_repaired_worktrees(project_root, &entries)?;
    }
    Ok(changed)
}

/// Persist the backfilled `entries` for a repair, PRESERVING the project's current
/// storage tier (Fix 4). Unlike [`set_project_worktrees`] — which routes
/// registry-first and would write `Some(list)`, silently promoting a committed-
/// authoritative project (`ProjectRef.worktrees == None`) into a per-machine
/// registry override — repair must not change *which* store is authoritative; it
/// only fills in missing paths.
///
/// - Registry entry already `Some(_)` → write the registry (as `set_project_worktrees`
///   would). The registry is already the authority; keep it there.
/// - Registry entry `None` (committed-authoritative) → write the COMMITTED file,
///   leaving `ProjectRef.worktrees` as `None`. The committed `keitora.toml` exists in
///   this case (the project is registered AND relying on its committed list).
/// - Unregistered project → write the committed file (same as today).
fn persist_repaired_worktrees(project_root: &Path, entries: &[WorktreeEntry]) -> Result<()> {
    let registry = UserConfig::load().map_err(|e| Error::Config(e.to_string()))?;
    let registry_authoritative = registry
        .project_ref_by_root(project_root)
        .is_some_and(|pref| pref.worktrees.is_some());
    if registry_authoritative {
        // Registry already owns the list — write it back there.
        set_project_worktrees(project_root, entries)
    } else {
        // Committed-authoritative (registered with `worktrees == None`) or
        // unregistered: backfill the committed file and leave the registry alone.
        set_committed_worktrees(project_root, entries)
    }
}

/// The effective worktree entries for `project_root`: registry-stamped when the
/// project is registered (so a per-machine override wins), else read straight from
/// the committed file / scaffold defaults. The read seam for
/// [`repair_project_worktree_paths`].
fn effective_entries_for(project_root: &Path) -> Result<Vec<WorktreeEntry>> {
    let registry = UserConfig::load().map_err(|e| Error::Config(e.to_string()))?;
    if let Some(pref) = registry.project_ref_by_root(project_root) {
        let pc = effective_config(pref).map_err(|e| Error::Config(e.to_string()))?;
        return Ok(pc.worktree_entries());
    }
    // Unregistered: load the committed file directly (no registry override).
    let pc = ProjectConfig::load(project_root).map_err(|e| Error::Config(e.to_string()))?;
    Ok(pc.worktree_entries())
}

/// Pure path-matching for [`is_project_worktree`]: is `candidate` one of
/// `worktree_paths` and NOT the main worktree (`root`)? Canonical comparison via
/// [`canonical_key`]. The main worktree (the project checkout itself, always the
/// first record of `git worktree list`) is never adoptable as a linked worktree.
fn is_linked_worktree(root: &Path, candidate: &Path, worktree_paths: &[PathBuf]) -> bool {
    let target = canonical_key(candidate);
    if target == canonical_key(root) {
        return false; // the main checkout is not a linked worktree to adopt
    }
    worktree_paths.iter().any(|p| canonical_key(p) == target)
}

/// True if `candidate` is one of `project_root`'s LINKED git worktrees (excludes
/// the main worktree). Shells to `git worktree list`; the comparison logic is the
/// pure, unit-tested [`is_linked_worktree`]. Used by [`create_worktree`] to decide
/// whether an existing directory at a target path can be ADOPTED (our own linked
/// worktree) or must be rejected (a foreign directory, or the main checkout).
fn is_project_worktree(project_root: &Path, candidate: &Path) -> Result<bool> {
    let paths: Vec<PathBuf> = discover_worktrees(project_root, &[])?
        .into_iter()
        .map(|w| w.path)
        .collect();
    Ok(is_linked_worktree(project_root, candidate, &paths))
}

/// True if `git -C <root> rev-parse --verify --quiet <refname>` succeeds (the
/// ref resolves). A git invocation failure (not just a non-existent ref) is a
/// hard error, mirroring the rest of this module.
fn git_ref_exists(root: &Path, refname: &str) -> Result<bool> {
    let out = crate::config::git_command()
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", refname])
        .output()
        .map_err(|e| Error::Config(format!("running `git rev-parse {refname}`: {e}")))?;
    Ok(out.status.success())
}

/// Resolve the branch new worktrees should be created from.
///
/// Order (first existing ref wins):
/// 1. An explicit `[project] base_branch` in `keitora.toml`. If set but the ref
///    is missing, that is a hard error (the user named a branch that isn't
///    there) — we do not silently fall through.
/// 2. The remote default branch via `git symbolic-ref refs/remotes/origin/HEAD`
///    (e.g. `origin/main` → `main`), preferring a local branch of that name
///    and falling back to the `origin/<name>` remote-tracking ref when no
///    local branch exists. A missing symref is a non-fatal miss.
/// 3. The conventional defaults `main`, then `master`.
///
/// Errors with an actionable message when nothing resolves.
fn resolve_base_branch(pc: &ProjectConfig) -> Result<String> {
    // 1. Explicit override.
    if let Some(configured) = pc.project.base_branch.as_deref() {
        if git_ref_exists(&pc.root, configured)? {
            return Ok(configured.to_string());
        }
        return Err(Error::Config(format!(
            "configured base branch {configured:?} not found in {}",
            pc.root.display()
        )));
    }

    // 2. Remote default branch (origin/HEAD), stripped to its short name.
    //    Prefer a local branch by that name; otherwise fall back to the
    //    remote-tracking ref so bare-clone / worktree-only setups (where the
    //    default branch was never checked out locally) still resolve.
    if let Some(branch) = git_origin_head(&pc.root)? {
        if git_ref_exists(&pc.root, &branch)? {
            return Ok(branch);
        }
        let remote_ref = format!("origin/{branch}");
        if git_ref_exists(&pc.root, &remote_ref)? {
            return Ok(remote_ref);
        }
    }

    // 3. Conventional defaults.
    for candidate in ["main", "master"] {
        if git_ref_exists(&pc.root, candidate)? {
            return Ok(candidate.to_string());
        }
    }

    Err(Error::Config(
        "could not resolve a base branch (tried origin/HEAD, main, master); \
         set `base_branch` under [project] in keitora.toml"
            .to_string(),
    ))
}

/// Read the remote default branch from `refs/remotes/origin/HEAD` and return its
/// short name (e.g. `main`). Returns `Ok(None)` when no such symref exists
/// (local-only repo, or `origin/HEAD` never set) — a non-fatal miss.
fn git_origin_head(root: &Path) -> Result<Option<String>> {
    let out = crate::config::git_command()
        .arg("-C")
        .arg(root)
        .args([
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        .output()
        .map_err(|e| Error::Config(format!("running `git symbolic-ref origin/HEAD`: {e}")))?;
    if !out.status.success() {
        return Ok(None);
    }
    // e.g. "origin/main" → "main"; tolerate a bare name just in case.
    let full = String::from_utf8_lossy(&out.stdout);
    let name = full.trim();
    let short = name.strip_prefix("origin/").unwrap_or(name);
    if short.is_empty() {
        Ok(None)
    } else {
        Ok(Some(short.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const SAMPLE: &str = "\
worktree /Users/x/code/keitora
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /Users/x/code/keitora-worktrees/sayo
HEAD 2222222222222222222222222222222222222222
branch refs/heads/llm/user/keitora/0015-config

worktree /Users/x/code/keitora-worktrees/detachy
HEAD 3333333333333333333333333333333333333333
detached
";

    #[test]
    fn parses_names_branches_and_tracked_flag() {
        let got = parse_worktree_porcelain(SAMPLE, &["sayo".to_string()]);
        assert_eq!(got.len(), 3);

        assert_eq!(got[0].name, "keitora");
        assert_eq!(got[0].branch.as_deref(), Some("main"));
        assert!(!got[0].already_tracked);

        assert_eq!(got[1].name, "sayo");
        assert_eq!(
            got[1].branch.as_deref(),
            Some("llm/user/keitora/0015-config")
        );
        assert!(got[1].already_tracked, "sayo is in `tracked`");

        // Detached worktree → no branch, name from basename.
        assert_eq!(got[2].name, "detachy");
        assert_eq!(got[2].branch, None);
    }

    #[test]
    fn empty_output_yields_no_candidates() {
        assert!(parse_worktree_porcelain("", &[]).is_empty());
    }

    #[test]
    fn set_worktrees_preserves_relative_db_path_and_other_fields() {
        // Exercises the UNREGISTERED fallback: an empty hermetic registry means
        // set_project_worktrees writes the committed keitora.toml surgically.
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::env::set_var("KEITORA_CONFIG_HOME", root.join("user_config"));
        std::fs::write(
            root.join("keitora.toml"),
            "[project]\nname = \"keitora\"\nworktrees_dir = \"../keitora-worktrees\"\n\n\
             [tasks]\ndb_path = \"../keitora-worktrees/shared/tasks.db\"\nworktrees = [\"old\"]\n\
             archive_days = 30\nlog_retention_days = 90\n",
        )
        .unwrap();

        set_project_worktrees(
            root,
            &[
                WorktreeEntry::name_only("keitora"),
                WorktreeEntry::name_only("sayo"),
            ],
        )
        .unwrap();

        let written = std::fs::read_to_string(root.join("keitora.toml")).unwrap();
        let doc = written.parse::<toml_edit::DocumentMut>().unwrap();
        // worktrees replaced… (name-only entries stay bare strings).
        let wts: Vec<String> = doc["tasks"]["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(wts, vec!["keitora", "sayo"]);
        // …while db_path stays RELATIVE (never canonicalized to absolute) and
        // the other fields survive.
        assert_eq!(
            doc["tasks"]["db_path"].as_str(),
            Some("../keitora-worktrees/shared/tasks.db")
        );
        assert_eq!(doc["project"]["name"].as_str(), Some("keitora"));
        assert_eq!(doc["tasks"]["archive_days"].as_integer(), Some(30));

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    // --- create_worktree -----------------------------------------------------

    use crate::config::{ProjectConfig, UserConfig};
    use crate::model::WorktreeStatus;
    use crate::TEST_ENV_LOCK;

    /// Run a git command in `dir`, panicking with stderr on failure.
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("running git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Create a temp git repo (init, identity, one commit) with NO `keitora.toml`.
    /// Returns the `TempDir` (kept alive by the caller), the Windows-prefix-stripped
    /// `base` temp path, and the repo `root` (`base/repo`). Shared by `temp_repo`
    /// (which layers a committed `keitora.toml` on top) and the file-less
    /// registered-project test, which registers via the user registry
    /// instead.
    ///
    /// `with_main` controls whether the initial commit lands on `main`; when
    /// false the branch is left as `master` so the base-branch-missing case can
    /// be exercised.
    fn init_repo(with_main: bool) -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        // `git worktree add` rejects the `\\?\` extended-length prefix that
        // `canonicalize()` adds on Windows ("could not create leading
        // directories"); strip it back to a plain `C:\…` path. No-op on Unix.
        #[cfg(windows)]
        let base = {
            let s = base.to_string_lossy().into_owned();
            PathBuf::from(s.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(s))
        };
        let root = base.join("repo");
        std::fs::create_dir_all(&root).unwrap();

        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.name", "Test User"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        // commit.gpgsign off so CI/local signing config can't break the commit.
        git(&root, &["config", "commit.gpgsign", "false"]);

        if with_main {
            git(&root, &["checkout", "-q", "-b", "main"]);
        } else {
            git(&root, &["checkout", "-q", "-b", "master"]);
        }
        std::fs::write(root.join("README.md"), b"hi\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "init"]);

        (tmp, base, root)
    }

    /// Build a temp git repo with an initial commit on `main`, plus a scaffolded
    /// `keitora.toml` whose `worktrees_dir` is an ABSOLUTE temp path (so no `~`
    /// expansion is involved). Returns the TempDir (kept alive by the caller),
    /// the repo root, and the loaded `ProjectConfig`.
    ///
    /// `with_main` controls whether the initial commit lands on `main`; when
    /// false the branch is left as `master` so the base-branch-missing case can
    /// be exercised.
    fn temp_repo(with_main: bool) -> (TempDir, PathBuf, ProjectConfig) {
        let (tmp, base, root) = init_repo(with_main);

        // worktrees_dir points at an absolute temp path outside the repo.
        let wt_dir = base.join("worktrees");
        let toml = format!(
            "[project]\nname = \"repo\"\nworktrees_dir = \"{}\"\n\n\
             [tasks]\ndb_path = \"shared/tasks.db\"\nworktrees = [\"repo\"]\n",
            // Escape backslashes so a Windows absolute path is a valid TOML basic
            // string (parses back to the original path); no-op on Unix.
            wt_dir.display().to_string().replace('\\', "\\\\")
        );
        std::fs::write(root.join("keitora.toml"), toml).unwrap();

        let pc = ProjectConfig::load(&root).unwrap();
        (tmp, root, pc)
    }

    /// Regression: a project registered in the user registry but with NO
    /// repo-local `keitora.toml` must still create worktrees — resolving through
    /// the registry (scaffold defaults) instead of hard-failing on the missing
    /// file. Before the fix, `create_worktree`'s step-7 re-read went through
    /// `ProjectConfig::load`, which errored on the absent file AFTER `git worktree
    /// add` had already run, stranding an untracked worktree on disk.
    #[test]
    fn create_worktree_succeeds_without_committed_keitora_toml() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_tmp, base, root) = init_repo(true);
        assert!(
            !root.join("keitora.toml").exists(),
            "precondition: the project has no committed config"
        );

        // Hermetic registry with the project registered. The scaffold default
        // `worktrees_dir` is `~/.keitora/<name>`, which would resolve against the
        // tester's REAL home; override it with an absolute temp path so the
        // worktree lands inside the sandbox. `register()` takes no such arg, so
        // set it on the `ProjectRef` directly before saving.
        std::env::set_var("KEITORA_CONFIG_HOME", base.join("user_config"));
        let wt_dir = base.join("worktrees");
        let mut reg = UserConfig::default();
        reg.register("repo", root.clone());
        reg.projects.get_mut("repo").unwrap().worktrees_dir =
            Some(wt_dir.to_string_lossy().into_owned());
        reg.save().unwrap();

        // Resolve `pc` exactly as `resolve_active()` would for the active project:
        // a file-less registered project → `effective_config` (scaffold + overrides).
        let pc = effective_config(reg.projects.get("repo").unwrap()).unwrap();

        let rw = create_worktree(&pc, "feature-x").unwrap();

        // The worktree was created on disk, under the override dir (not real home).
        assert_eq!(rw.name, "feature-x");
        assert_eq!(rw.status, WorktreeStatus::Available);
        assert!(rw.path.exists());
        assert!(
            rw.path.starts_with(&wt_dir),
            "worktree {:?} should land under the override dir {:?}",
            rw.path,
            wt_dir
        );

        // No `keitora.toml` was created — registration went to the REGISTRY.
        assert!(
            !root.join("keitora.toml").exists(),
            "create_worktree must not scaffold a committed file"
        );
        let reloaded = UserConfig::load().unwrap();
        let entries = reloaded.projects["repo"]
            .worktrees
            .clone()
            .expect("registry recorded the worktree list");
        let entry = entries
            .iter()
            .find(|e| e.name == "feature-x")
            .expect("feature-x registered in the user registry");
        assert_eq!(
            entry.path.as_deref(),
            Some(rw.path.as_path()),
            "the registered entry carries the created path"
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn create_worktree_happy_path() {
        let (_tmp, root, pc) = temp_repo(true);

        let rw = create_worktree(&pc, "feature-x").unwrap();

        // Returned path matches worktree_path and is an Available git worktree.
        assert_eq!(rw.name, "feature-x");
        assert_eq!(rw.path, pc.worktree_path("feature-x"));
        assert!(rw.path.exists());
        assert_eq!(rw.status, WorktreeStatus::Available);
        assert_eq!(classify_worktree(&rw.path), WorktreeStatus::Available);

        // keitora.toml gained the name (alongside the pre-existing "repo"); the new
        // entry persists name + the created PATH, while the pre-existing
        // name-only "repo" stays a bare string.
        assert_eq!(committed_worktrees(&root), vec!["repo", "feature-x"]);
        assert_eq!(
            committed_worktree_path(&root, "feature-x"),
            Some(pc.worktree_path("feature-x").to_string_lossy().into_owned()),
            "the created worktree persists its path as an inline table"
        );
        assert_eq!(
            committed_worktree_path(&root, "repo"),
            None,
            "the pre-existing name-only entry stays a bare string"
        );

        // The branch exists with the expected shape. (Slug pinning is avoided in
        // case CI git config differs; we assert prefix/suffix instead.)
        assert!(git_ref_exists(&root, "main").unwrap());
        let candidates = discover_worktrees(&root, &pc.worktrees()).unwrap();
        let created = candidates
            .iter()
            .find(|c| c.name == "feature-x")
            .expect("git worktree list shows feature-x");
        let branch = created.branch.as_deref().expect("worktree has a branch");
        assert!(branch.starts_with("llm/"), "branch was {branch}");
        assert!(branch.ends_with("/repo/feature-x"), "branch was {branch}");
        assert!(git_ref_exists(&root, branch).unwrap());
    }

    #[test]
    fn create_worktree_adopts_existing_project_worktree() {
        let (_tmp, root, pc) = temp_repo(true);
        // First call creates "dup" as a real linked worktree + branch.
        create_worktree(&pc, "dup").unwrap();
        // `pc` is stale (in-memory list is still ["repo"], not "dup"), so the
        // step-1 "already configured" guard does not short-circuit — this
        // simulates a worktree that exists on disk but isn't tracked yet. The
        // second call must ADOPT it (not error, not create a second worktree).
        let rw = create_worktree(&pc, "dup").expect("adopt existing project worktree");
        assert_eq!(rw.name, "dup");
        assert_eq!(rw.status, WorktreeStatus::Available);
        // Adoption skips `git worktree add`, so there is still exactly one "dup".
        let count = discover_worktrees(&root, &[])
            .unwrap()
            .iter()
            .filter(|c| c.name == "dup")
            .count();
        assert_eq!(count, 1, "adoption must not create a duplicate worktree");
    }

    #[test]
    fn create_worktree_rejects_name_already_configured() {
        let (_tmp, _root, pc) = temp_repo(true);
        // "repo" is already in [tasks].worktrees → rejected before any git work.
        assert!(create_worktree(&pc, "repo").is_err());
    }

    #[test]
    fn create_worktree_rejects_invalid_names() {
        let (_tmp, root, pc) = temp_repo(true);
        for bad in ["a/b", "..", ".hidden", ""] {
            assert!(
                create_worktree(&pc, bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        // No fs/git side effects: config untouched, no worktrees added.
        let written = std::fs::read_to_string(root.join("keitora.toml")).unwrap();
        let doc = written.parse::<toml_edit::DocumentMut>().unwrap();
        let wts: Vec<String> = doc["tasks"]["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(wts, vec!["repo"]);
        assert_eq!(discover_worktrees(&root, &[]).unwrap().len(), 1);
    }

    #[test]
    fn create_worktree_rejects_existing_foreign_path() {
        let (_tmp, _root, pc) = temp_repo(true);
        // A plain directory at the target path that is NOT one of this project's
        // git worktrees must be rejected — we never touch a foreign directory.
        let path = pc.worktree_path("taken");
        std::fs::create_dir_all(&path).unwrap();
        let err = create_worktree(&pc, "taken").unwrap_err();
        assert!(
            format!("{err}").contains("not a worktree of this project"),
            "expected a foreign-path error, got: {err}"
        );
    }

    #[test]
    fn is_linked_worktree_excludes_main_and_matches_linked() {
        let root = Path::new("/p/repo");
        let wt_a = PathBuf::from("/p/wt/a");
        // `git worktree list` emits the main worktree (root) AND linked ones.
        let paths = vec![root.to_path_buf(), wt_a.clone()];
        // The main checkout is never adoptable, even though git lists it.
        assert!(!is_linked_worktree(root, root, &paths));
        // A linked worktree matches.
        assert!(is_linked_worktree(root, &wt_a, &paths));
        // An unrelated path does not.
        assert!(!is_linked_worktree(root, Path::new("/p/wt/other"), &paths));
    }

    #[test]
    fn create_worktree_succeeds_on_master_repo() {
        // Repo whose default branch is `master` (no `main`). Resolution falls
        // through to `master`, so creation succeeds rather than erroring.
        let (_tmp, root, pc) = temp_repo(false);

        let rw = create_worktree(&pc, "feature-x").unwrap();
        assert!(rw.path.exists());

        // The new branch was cut from master's tip.
        let created = discover_worktrees(&root, &pc.worktrees())
            .unwrap()
            .into_iter()
            .find(|c| c.name == "feature-x")
            .expect("git worktree list shows feature-x");
        let branch = created.branch.as_deref().expect("worktree has a branch");
        assert!(branch.ends_with("/repo/feature-x"), "branch was {branch}");
        assert_eq!(
            rev_parse(&root, branch),
            rev_parse(&root, "master"),
            "feature-x should branch from master's tip"
        );
    }

    #[test]
    fn resolve_base_prefers_main_then_master() {
        let (_tmp, _root, pc_main) = temp_repo(true);
        assert_eq!(resolve_base_branch(&pc_main).unwrap(), "main");

        let (_tmp2, _root2, pc_master) = temp_repo(false);
        assert_eq!(resolve_base_branch(&pc_master).unwrap(), "master");
    }

    #[test]
    fn resolve_base_falls_back_to_remote_tracking_ref() {
        // origin/HEAD names a branch that exists only as a remote-tracking ref
        // (no local branch of that name) — as in a bare-clone / worktree-only
        // setup. Resolution should return `origin/<name>` rather than failing.
        let (_tmp, root, pc) = temp_repo(false); // local `master` only
        git(&root, &["update-ref", "refs/remotes/origin/trunk", "HEAD"]);
        git(
            &root,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/trunk",
            ],
        );
        // No local `trunk`, so step 2 must use the remote-tracking ref. It also
        // takes precedence over the `master` fallback in step 3.
        assert_eq!(resolve_base_branch(&pc).unwrap(), "origin/trunk");
    }

    #[test]
    fn resolve_base_honors_explicit_config() {
        // On a `master` repo, an explicit base wins over the fallback chain.
        let (_tmp, root, mut pc) = temp_repo(false);
        git(&root, &["branch", "dev"]);
        pc.project.base_branch = Some("dev".to_string());
        assert_eq!(resolve_base_branch(&pc).unwrap(), "dev");
    }

    #[test]
    fn resolve_base_errors_on_missing_explicit_config() {
        let (_tmp, _root, mut pc) = temp_repo(true);
        pc.project.base_branch = Some("nonexistent".to_string());
        let err = resolve_base_branch(&pc).unwrap_err();
        assert!(
            err.to_string().contains("nonexistent"),
            "error should name the missing branch, got: {err}"
        );
    }

    /// `git rev-parse <ref>` → trimmed SHA, asserting success.
    fn rev_parse(root: &Path, refname: &str) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", refname])
            .output()
            .expect("running git rev-parse");
        assert!(out.status.success(), "rev-parse {refname} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    // --- write redirect to the user registry -------------------------

    /// Read the committed `[tasks].worktrees` NAMES from a project's keitora.toml,
    /// accepting either a bare string (name-only entry) or an inline
    /// `{ name, path }` table (path-carrying entry).
    fn committed_worktrees(root: &Path) -> Vec<String> {
        let text = std::fs::read_to_string(root.join("keitora.toml")).unwrap();
        let doc = text.parse::<toml_edit::DocumentMut>().unwrap();
        committed_worktree_names(&doc)
    }

    /// Names from a parsed committed worktrees array (bare string or inline table).
    fn committed_worktree_names(doc: &toml_edit::DocumentMut) -> Vec<String> {
        doc["tasks"]["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                if let Some(s) = v.as_str() {
                    s.to_string()
                } else {
                    v.as_inline_table()
                        .and_then(|t| t.get("name"))
                        .and_then(|n| n.as_str())
                        .expect("worktree entry is a string or {name,path} table")
                        .to_string()
                }
            })
            .collect()
    }

    /// The committed worktree entry for `name`, if any, with its `path` value (the
    /// inline-table `path` field as a string), so tests can assert the persisted
    /// path of a created worktree.
    fn committed_worktree_path(root: &Path, name: &str) -> Option<String> {
        let text = std::fs::read_to_string(root.join("keitora.toml")).unwrap();
        let doc = text.parse::<toml_edit::DocumentMut>().unwrap();
        let found = doc["tasks"]["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|v| {
                let t = v.as_inline_table()?;
                if t.get("name").and_then(|n| n.as_str()) == Some(name) {
                    t.get("path").and_then(|p| p.as_str()).map(str::to_string)
                } else {
                    None
                }
            });
        found
    }

    #[test]
    fn set_project_worktrees_writes_registry_and_leaves_committed_file_untouched() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("keitora.toml"),
            "[project]\nname = \"repo\"\nworktrees_dir = \"../wt\"\n\n\
             [tasks]\ndb_path = \"shared/tasks.db\"\nworktrees = [\"repo\"]\n",
        )
        .unwrap();

        // Hermetic registry with the project registered (empty worktrees).
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path().join("user_config"));
        let mut reg = UserConfig::default();
        reg.register("repo", root.clone());
        reg.save().unwrap();

        set_project_worktrees(
            &root,
            &[
                WorktreeEntry::name_only("repo"),
                WorktreeEntry::name_only("extra"),
            ],
        )
        .unwrap();

        // Registry got the list…
        let reloaded = UserConfig::load().unwrap();
        assert_eq!(
            reloaded.projects["repo"].worktrees,
            Some(vec![
                WorktreeEntry::name_only("repo"),
                WorktreeEntry::name_only("extra"),
            ])
        );
        // …and the committed file is untouched (still just "repo").
        assert_eq!(committed_worktrees(&root), vec!["repo".to_string()]);

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn set_project_worktrees_keeps_the_registered_order() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path().join("user_config"));
        let mut reg = UserConfig::default();
        reg.register("repo", root.clone());
        reg.projects.get_mut("repo").unwrap().worktrees = Some(
            ["charlie", "alpha", "bravo"]
                .map(WorktreeEntry::name_only)
                .to_vec(),
        );
        reg.save().unwrap();

        // Discovery order, with "bravo" untracked and "delta", "wt" new.
        set_project_worktrees(
            &root,
            &["alpha", "charlie", "wt", "delta"].map(WorktreeEntry::name_only),
        )
        .unwrap();

        let reloaded = UserConfig::load().unwrap();
        assert_eq!(
            reloaded.projects["repo"].worktrees,
            Some(
                ["charlie", "alpha", "wt", "delta"]
                    .map(WorktreeEntry::name_only)
                    .to_vec()
            )
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn reorder_project_worktrees_writes_the_order_and_keeps_fields() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path().join("user_config"));
        let alpha = WorktreeEntry {
            cli: Some(crate::config::CliKind::Codex),
            codex_thread_id: Some("thread".into()),
            claude_session_id: Some("session".into()),
            diff_base: Some(crate::config::DiffBase::Upstream),
            ..WorktreeEntry::new("alpha", root.join("alpha"))
        };
        let mut reg = UserConfig::default();
        reg.register("proj", root.clone());
        reg.projects.get_mut("proj").unwrap().worktrees = Some(vec![
            alpha.clone(),
            WorktreeEntry::new("bravo", root.join("bravo")),
            WorktreeEntry::new("charlie", root.join("charlie")),
        ]);
        reg.save().unwrap();

        reorder_project_worktrees("proj", &["charlie".into(), "alpha".into()]).unwrap();

        let list = UserConfig::load().unwrap().projects["proj"]
            .worktrees
            .clone()
            .unwrap();
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["charlie", "alpha", "bravo"]);
        assert_eq!(list[1], alpha);

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn reorder_project_worktrees_copies_a_committed_list_into_the_registry() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("keitora.toml"),
            "[project]\nname = \"proj\"\nworktrees_dir = \"../wt\"\n\n\
             [tasks]\ndb_path = \"shared/tasks.db\"\nworktrees = [\"alpha\", \"bravo\"]\n",
        )
        .unwrap();
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path().join("user_config"));
        let mut reg = UserConfig::default();
        reg.register("proj", root.clone());
        reg.save().unwrap();

        reorder_project_worktrees("proj", &["bravo".into()]).unwrap();

        assert_eq!(
            UserConfig::load().unwrap().projects["proj"].worktrees,
            Some(vec![
                WorktreeEntry::name_only("bravo"),
                WorktreeEntry::name_only("alpha"),
            ])
        );
        assert_eq!(
            committed_worktrees(&root),
            vec!["alpha".to_string(), "bravo".to_string()]
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn reorder_project_worktrees_rejects_an_unknown_project() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path().join("user_config"));

        let err = reorder_project_worktrees("other-proj", &[]).unwrap_err();
        assert!(err
            .to_string()
            .contains("No project named 'other-proj' is registered."));

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn set_project_worktrees_falls_back_to_committed_for_unregistered() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("keitora.toml"),
            "[project]\nname = \"repo\"\nworktrees_dir = \"../wt\"\n\n\
             [tasks]\ndb_path = \"shared/tasks.db\"\nworktrees = [\"old\"]\n",
        )
        .unwrap();

        // Empty hermetic registry → no match → committed-file fallback.
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path().join("user_config"));

        set_project_worktrees(
            &root,
            &[WorktreeEntry::name_only("a"), WorktreeEntry::name_only("b")],
        )
        .unwrap();

        assert_eq!(
            committed_worktrees(&root),
            vec!["a".to_string(), "b".to_string()]
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn set_project_worktrees_by_root_returns_false_for_unknown_root() {
        let mut reg = UserConfig::default();
        reg.register("repo", PathBuf::from("/some/registered/path"));
        let updated = reg
            .set_project_worktrees_by_root(
                Path::new("/a/different/path"),
                &[WorktreeEntry::name_only("x")],
            )
            .unwrap();
        assert!(!updated);
    }

    #[test]
    fn create_worktree_registered_writes_registry_not_committed_file() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_tmp, root, pc) = temp_repo(true); // committed worktrees = ["repo"]

        // Register the project so the write goes to the hermetic registry.
        std::env::set_var(
            "KEITORA_CONFIG_HOME",
            root.parent().unwrap().join("user_config"),
        );
        let mut reg = UserConfig::default();
        reg.register("repo", root.clone());
        reg.save().unwrap();

        create_worktree(&pc, "feat").unwrap();

        // Registry got [repo (name-only), feat (with its created PATH)] — the
        // pre-existing committed "repo" stays name-only; the new entry carries the
        // path git reports for it.
        let reloaded = UserConfig::load().unwrap();
        assert_eq!(
            reloaded.projects["repo"].worktrees,
            Some(vec![
                WorktreeEntry::name_only("repo"),
                WorktreeEntry::new("feat", pc.worktree_path("feat")),
            ])
        );
        // …committed file still only "repo".
        assert_eq!(committed_worktrees(&root), vec!["repo".to_string()]);

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn create_worktree_places_under_worktrees_dir_and_persists_path() {
        // A created worktree is still PLACED under `worktrees_dir` (the
        // placement policy is unchanged), and the persisted entry now carries that
        // path so resolution never has to reconstruct it.
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_tmp, root, pc) = temp_repo(true);
        // Unregistered → write goes to the committed file (hermetic registry).
        std::env::set_var(
            "KEITORA_CONFIG_HOME",
            root.parent().unwrap().join("user_config"),
        );

        let rw = create_worktree(&pc, "feature-y").unwrap();

        // Placement: under the configured worktrees_dir (the absolute temp path),
        // i.e. worktrees_root()/feature-y, exactly as before path-carrying entries.
        let expected = pc.worktrees_root().join("feature-y");
        assert_eq!(rw.path, expected, "placed under worktrees_dir");
        assert!(rw.path.exists());
        assert_eq!(rw.status, WorktreeStatus::Available);

        // Persistence: the committed entry carries path == the created path.
        assert_eq!(
            committed_worktree_path(&root, "feature-y"),
            Some(expected.to_string_lossy().into_owned()),
            "persisted entry carries the created path"
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn repair_backfills_pathless_entries_and_is_idempotent() {
        // Repair pass. A committed config with bare-name (path: None) entries
        // + a real git-worktree-populated repo: the first repair call backfills the
        // paths from `git worktree list`, persists them, and returns true; the
        // second call is a no-op (returns false, rewrites nothing).
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_tmp, root, pc) = temp_repo(true);
        // Unregistered → committed-file write path (hermetic registry).
        std::env::set_var(
            "KEITORA_CONFIG_HOME",
            root.parent().unwrap().join("user_config"),
        );

        // Create two real linked worktrees, then REWRITE the committed list as bare
        // names (path: None) to simulate a legacy / legacy config.
        create_worktree(&pc, "alpha").unwrap();
        create_worktree(&pc, "bravo").unwrap();
        set_project_worktrees(
            &root,
            &[
                WorktreeEntry::name_only("repo"),
                WorktreeEntry::name_only("alpha"),
                WorktreeEntry::name_only("bravo"),
            ],
        )
        .unwrap();
        // Sanity: the committed entries are path-less.
        assert_eq!(committed_worktree_path(&root, "alpha"), None);

        // First repair: backfills alpha/bravo from `git worktree list`, persists.
        assert!(
            repair_project_worktree_paths(&root).unwrap(),
            "first repair must report a change"
        );
        // alpha/bravo now carry their on-disk worktree paths. Compare canonically
        // (git may report a canonicalized / `\\?\`-prefixed form on Windows).
        let want_alpha = pc.worktree_path("alpha");
        let want_bravo = pc.worktree_path("bravo");
        assert_eq!(
            committed_worktree_path(&root, "alpha").map(|p| canonical_key(Path::new(&p))),
            Some(canonical_key(&want_alpha)),
            "alpha path backfilled"
        );
        assert_eq!(
            committed_worktree_path(&root, "bravo").map(|p| canonical_key(Path::new(&p))),
            Some(canonical_key(&want_bravo))
        );

        // Capture the file, run repair again: must be a NO-OP (false, no rewrite).
        let after_first = std::fs::read_to_string(root.join("keitora.toml")).unwrap();
        assert!(
            !repair_project_worktree_paths(&root).unwrap(),
            "second repair must be idempotent (no change)"
        );
        let after_second = std::fs::read_to_string(root.join("keitora.toml")).unwrap();
        assert_eq!(
            after_first, after_second,
            "idempotent repair must not rewrite the file"
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn repair_committed_authoritative_writes_committed_not_registry() {
        // Fix 4: a REGISTERED project whose `ProjectRef.worktrees == None`
        // is committed-authoritative (it relies on the committed `[tasks].worktrees`).
        // Repair must backfill the paths into the COMMITTED file and must NOT
        // promote the registry entry to `Some(_)` (which would silently flip the
        // project from committed-shared to machine-local-pinned).
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_tmp, root, pc) = temp_repo(true);

        // Register the project but leave its worktree override unset (None).
        std::env::set_var(
            "KEITORA_CONFIG_HOME",
            root.parent().unwrap().join("user_config"),
        );
        let mut reg = UserConfig::default();
        reg.register("repo", root.clone());
        reg.save().unwrap();
        assert_eq!(
            UserConfig::load().unwrap().projects["repo"].worktrees,
            None,
            "precondition: registry entry is committed-authoritative (None)"
        );

        // Two real linked worktrees. Because the project is registered, the
        // additive writes inside create_worktree route to the registry — so reset
        // the registry override back to None afterwards to restore the
        // committed-authoritative state we want to exercise.
        create_worktree(&pc, "alpha").unwrap();
        create_worktree(&pc, "bravo").unwrap();
        {
            let mut reg = UserConfig::load().unwrap();
            reg.projects.get_mut("repo").unwrap().worktrees = None;
            reg.save().unwrap();
        }
        // Rewrite the COMMITTED list as bare names (path: None) to simulate a
        // legacy committed config that repair must backfill.
        set_committed_worktrees(
            &root,
            &[
                WorktreeEntry::name_only("repo"),
                WorktreeEntry::name_only("alpha"),
                WorktreeEntry::name_only("bravo"),
            ],
        )
        .unwrap();
        assert_eq!(committed_worktree_path(&root, "alpha"), None);
        assert_eq!(
            UserConfig::load().unwrap().projects["repo"].worktrees,
            None,
            "registry reset to committed-authoritative (None)"
        );

        // Repair backfills.
        assert!(
            repair_project_worktree_paths(&root).unwrap(),
            "repair must report a change"
        );

        // The paths landed in the COMMITTED file…
        let want_alpha = pc.worktree_path("alpha");
        assert_eq!(
            committed_worktree_path(&root, "alpha").map(|p| canonical_key(Path::new(&p))),
            Some(canonical_key(&want_alpha)),
            "alpha path backfilled into the committed file"
        );
        // …and the registry entry stays None (NOT promoted to Some).
        assert_eq!(
            UserConfig::load().unwrap().projects["repo"].worktrees,
            None,
            "registry must stay committed-authoritative (None) after repair"
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn repair_registry_authoritative_writes_registry() {
        // Fix 4, companion case: a registered project whose
        // `ProjectRef.worktrees == Some(_)` is registry-authoritative. Repair must
        // continue to write the REGISTRY (the committed file is left untouched).
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_tmp, root, pc) = temp_repo(true);

        std::env::set_var(
            "KEITORA_CONFIG_HOME",
            root.parent().unwrap().join("user_config"),
        );
        let mut reg = UserConfig::default();
        reg.register("repo", root.clone());
        reg.save().unwrap();

        // Two real linked worktrees; pin a path-less list into the REGISTRY (Some)
        // so the entry is registry-authoritative with paths to backfill.
        create_worktree(&pc, "alpha").unwrap();
        create_worktree(&pc, "bravo").unwrap();
        set_project_worktrees(
            &root,
            &[
                WorktreeEntry::name_only("repo"),
                WorktreeEntry::name_only("alpha"),
                WorktreeEntry::name_only("bravo"),
            ],
        )
        .unwrap();
        assert!(
            UserConfig::load().unwrap().projects["repo"]
                .worktrees
                .is_some(),
            "precondition: registry entry is authoritative (Some)"
        );
        // Committed file is the temp_repo default (just "repo"); capture it.
        let committed_before = std::fs::read_to_string(root.join("keitora.toml")).unwrap();

        // Repair backfills.
        assert!(
            repair_project_worktree_paths(&root).unwrap(),
            "repair must report a change"
        );

        // The paths landed in the REGISTRY…
        let want_alpha = pc.worktree_path("alpha");
        let reg_alpha = UserConfig::load().unwrap().projects["repo"]
            .worktrees
            .clone()
            .unwrap()
            .into_iter()
            .find(|e| e.name == "alpha")
            .and_then(|e| e.path);
        assert_eq!(
            reg_alpha.map(|p| canonical_key(&p)),
            Some(canonical_key(&want_alpha)),
            "alpha path backfilled into the registry"
        );
        // …and the committed file is untouched.
        assert_eq!(
            std::fs::read_to_string(root.join("keitora.toml")).unwrap(),
            committed_before,
            "registry-authoritative repair must not touch the committed file"
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }
}
