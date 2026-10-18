//! `attribute` — pure attribution of OS processes to sessions, worktrees and the
//! non-session buckets. No I/O: the sampler hands in plain [`ProcSample`]s.
//!
//! Order, per process: a session's pane tree; then the herdr server's subtree;
//! then Keitora's own subtree; then (resolved elsewhere, from the cmdline / cwd
//! read for unclaimed processes) a worktree inferred by path; otherwise Other.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::activity::progress::ubt::{
    extract_project_from_args, is_build_mode, is_component_prefix,
};
use crate::backend::herdr::keymap::normalize_either_separator;
use crate::backend::SessionKey;

use super::{PerfHistory, PerfPane, PerfProc, PerfRow, PerfRowKind, TOP_PROCS};

/// One process as sampled this tick.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcSample {
    pub pid: u32,
    pub ppid: Option<u32>,
    /// Start time, whole seconds since the epoch.
    pub start: u64,
    pub name: String,
    pub cpu_percent: f32,
    pub mem_bytes: u64,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
}

/// The root processes of one session workspace: the shell pid of each pane
/// first, then the foreground pids herdr reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub workspace_id: String,
    pub pids: Vec<u32>,
}

/// Who a process was claimed by through the process tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// A session, by its herdr workspace id.
    Session(String),
    Herdr,
    Keitora,
}

/// A registered worktree, for path inference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRoot {
    /// The resolved project name.
    pub project: String,
    pub worktree: String,
    pub path: String,
}

impl WorktreeRoot {
    /// The session key this worktree's processes roll up under.
    pub fn session(&self) -> SessionKey {
        SessionKey {
            project: self.project.clone(),
            worktree: self.worktree.clone(),
        }
    }
}

/// Claim processes through the process tree: each session root's descendants
/// (roots in ascending `workspace_id` order), then the herdr server's, then
/// Keitora's own (herdr first, because the server may be Keitora's child). A
/// process is claimed once. A parent link counts only when the parent is in the
/// snapshot and started no later than the child: Windows keeps stale parent pids
/// and reuses pids.
pub fn claim_owners(
    procs: &[ProcSample],
    roots: &[Root],
    herdr_pid: Option<u32>,
    keitora_pid: u32,
) -> HashMap<u32, Owner> {
    let start_of: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, p.start)).collect();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs {
        let Some(ppid) = p.ppid else { continue };
        if ppid == p.pid {
            continue;
        }
        if let Some(&parent_start) = start_of.get(&ppid) {
            if p.start >= parent_start {
                children.entry(ppid).or_default().push(p.pid);
            }
        }
    }

    let mut owners: HashMap<u32, Owner> = HashMap::new();
    let mut ordered: Vec<&Root> = roots.iter().collect();
    ordered.sort_by(|a, b| a.workspace_id.cmp(&b.workspace_id));
    for root in ordered {
        for &pid in &root.pids {
            claim_tree(
                pid,
                &Owner::Session(root.workspace_id.clone()),
                &start_of,
                &children,
                &mut owners,
            );
        }
    }
    if let Some(pid) = herdr_pid {
        claim_tree(pid, &Owner::Herdr, &start_of, &children, &mut owners);
    }
    claim_tree(
        keitora_pid,
        &Owner::Keitora,
        &start_of,
        &children,
        &mut owners,
    );
    owners
}

/// Claim `pid` (when it is in the snapshot) and its unclaimed descendants for
/// `owner`. The walk stops at a process another owner already holds, so that
/// process's own subtree stays with it.
fn claim_tree(
    pid: u32,
    owner: &Owner,
    start_of: &HashMap<u32, u64>,
    children: &HashMap<u32, Vec<u32>>,
    owners: &mut HashMap<u32, Owner>,
) {
    if !start_of.contains_key(&pid) {
        return;
    }
    let mut stack = vec![pid];
    while let Some(pid) = stack.pop() {
        if owners.contains_key(&pid) {
            continue;
        }
        owners.insert(pid, owner.clone());
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }
}

/// The worktree whose path is the longest component-prefix of `path` (both
/// normalised, compared case-insensitively). Two different worktrees sharing
/// that longest path are ambiguous: `None`.
pub fn match_root(path: &str, roots: &[WorktreeRoot]) -> Option<usize> {
    let path = normalize_either_separator(path);
    let mut best: Option<(usize, usize)> = None; // (index, path length)
    let mut tied = false;
    for (i, root) in roots.iter().enumerate() {
        let root_path = normalize_either_separator(&root.path);
        if !is_component_prefix(&path, &root_path) {
            continue;
        }
        let len = root_path.len();
        match best {
            Some((_, best_len)) if len < best_len => {}
            Some((j, best_len)) if len == best_len => {
                let same = roots[j].project == root.project && roots[j].worktree == root.worktree;
                tied |= !same;
            }
            _ => {
                best = Some((i, len));
                tied = false;
            }
        }
    }
    match best {
        Some((i, _)) if !tied => Some(i),
        _ => None,
    }
}

/// Infer the worktree a process outside every pane works in, from its argv and
/// cwd: a UBT build's `-Project=`; else its cwd; else the absolute paths passed
/// as `-flag=path` / `/flag=path` arguments, when every one that matches names
/// the same worktree.
pub fn infer_worktree(argv: &[String], cwd: Option<&str>, roots: &[WorktreeRoot]) -> Option<usize> {
    if is_build_mode(argv) {
        if let Some(i) = extract_project_from_args(argv).and_then(|p| match_root(&p, roots)) {
            return Some(i);
        }
    }
    if let Some(i) = cwd.and_then(|c| match_root(c, roots)) {
        return Some(i);
    }
    let mut found: Option<usize> = None;
    for arg in argv {
        if !(arg.starts_with('-') || arg.starts_with('/')) {
            continue;
        }
        let Some((_, value)) = arg.split_once('=') else {
            continue;
        };
        let value = strip_quote_pair(value.trim()).trim();
        if !looks_absolute(value) {
            continue;
        }
        let Some(i) = match_root(value, roots) else {
            continue;
        };
        match found {
            Some(j) if j != i => return None,
            _ => found = Some(i),
        }
    }
    found
}

/// Strip one surrounding pair of `"` or `'`, if present.
fn strip_quote_pair(s: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// `X:\`, `X:/`, a `\\` UNC path, or a leading `/`.
fn looks_absolute(s: &str) -> bool {
    let b = s.as_bytes();
    let drive =
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    drive || s.starts_with(r"\\") || s.starts_with('/')
}

/// One row's values for a tick, before its history is attached.
#[derive(Debug, Clone, PartialEq)]
pub struct RowAccum {
    pub key: String,
    pub kind: PerfRowKind,
    pub project: Option<String>,
    pub worktree: Option<String>,
    pub resolving: bool,
    pub cpu_percent: f32,
    pub mem_bytes: u64,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    pub proc_count: u32,
    pub inferred_count: u32,
    pub top_procs: Vec<PerfProc>,
    /// `(index into the tick's processes, inferred)` for each process added;
    /// [`finish_top_procs`](Self::finish_top_procs) turns the busiest into
    /// `top_procs`.
    candidates: Vec<(usize, bool)>,
}

impl RowAccum {
    fn new(key: String, kind: PerfRowKind) -> Self {
        RowAccum {
            key,
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
            top_procs: Vec::new(),
            candidates: Vec::new(),
        }
    }

    fn worktree(session: &SessionKey) -> Self {
        let mut row = RowAccum::new(
            format!("worktree:{}/{}", session.project, session.worktree),
            PerfRowKind::Worktree,
        );
        row.project = Some(session.project.clone());
        row.worktree = Some(session.worktree.clone());
        row
    }

    /// Add `procs[idx]` (passed as `p`) to the totals and the top-process candidates.
    fn add(&mut self, idx: usize, p: &ProcSample, inferred: bool) {
        self.cpu_percent += p.cpu_percent;
        self.mem_bytes += p.mem_bytes;
        self.disk_read_bps += p.disk_read_bps;
        self.disk_write_bps += p.disk_write_bps;
        self.proc_count += 1;
        if inferred {
            self.inferred_count += 1;
        }
        self.candidates.push((idx, inferred));
    }

    /// Add a worktree row's totals to its project row.
    fn absorb(&mut self, wt: &RowAccum) {
        self.cpu_percent += wt.cpu_percent;
        self.mem_bytes += wt.mem_bytes;
        self.disk_read_bps += wt.disk_read_bps;
        self.disk_write_bps += wt.disk_write_bps;
        self.proc_count += wt.proc_count;
        self.inferred_count += wt.inferred_count;
        self.resolving |= wt.resolving;
    }

    /// Fill `top_procs` from the added candidates: the busiest [`TOP_PROCS`]
    /// (cpu, then memory, then lowest pid), busiest first. Only those survivors
    /// have their names copied.
    fn finish_top_procs(&mut self, procs: &[ProcSample]) {
        let busier = |a: &(usize, bool), b: &(usize, bool)| {
            let (a, b) = (&procs[a.0], &procs[b.0]);
            b.cpu_percent
                .total_cmp(&a.cpu_percent)
                .then(b.mem_bytes.cmp(&a.mem_bytes))
                .then(a.pid.cmp(&b.pid))
        };
        let mut candidates = std::mem::take(&mut self.candidates);
        if candidates.len() > TOP_PROCS {
            candidates.select_nth_unstable_by(TOP_PROCS - 1, busier);
            candidates.truncate(TOP_PROCS);
        }
        candidates.sort_by(busier);
        self.top_procs = candidates
            .into_iter()
            .map(|(i, inferred)| {
                let p = &procs[i];
                PerfProc {
                    pid: p.pid,
                    name: p.name.clone(),
                    cpu_percent: p.cpu_percent,
                    mem_bytes: p.mem_bytes,
                    disk_read_bps: p.disk_read_bps,
                    disk_write_bps: p.disk_write_bps,
                    inferred,
                }
            })
            .collect();
    }

    /// The finished row with its history attached.
    pub fn into_row(self, history: PerfHistory) -> PerfRow {
        PerfRow {
            key: self.key,
            kind: self.kind,
            project: self.project,
            worktree: self.worktree,
            resolving: self.resolving,
            cpu_percent: self.cpu_percent,
            mem_bytes: self.mem_bytes,
            disk_read_bps: self.disk_read_bps,
            disk_write_bps: self.disk_write_bps,
            proc_count: self.proc_count,
            inferred_count: self.inferred_count,
            history,
            top_procs: self.top_procs,
        }
    }
}

/// Sum every process into its row and derive the project rows.
///
/// `inferred` holds the worktree inferred for an unclaimed `(pid, start)`; `resolving` the workspaces with a pane whose roots are not
/// known yet. A worktree row is emitted when its session has a pane in `panes`
/// or it received a process; a project row when it has a worktree row (its
/// values are their sums, and it lists no processes); the `keitora`, `herdr` and
/// `other` rows always. Rows come out as each project (by name) followed by its
/// worktrees (by name), then `keitora`, `herdr`, `other`.
pub fn rollup(
    procs: &[ProcSample],
    owners: &HashMap<u32, Owner>,
    inferred: &HashMap<(u32, u64), SessionKey>,
    panes: &[PerfPane],
    resolving: &HashSet<String>,
) -> Vec<RowAccum> {
    let mut session_of: HashMap<&str, &SessionKey> = HashMap::new();
    let mut worktrees: BTreeMap<(String, String), RowAccum> = BTreeMap::new();
    for pane in panes {
        session_of.insert(pane.workspace_id.as_str(), &pane.session);
        let key = (pane.session.project.clone(), pane.session.worktree.clone());
        let row = worktrees
            .entry(key)
            .or_insert_with(|| RowAccum::worktree(&pane.session));
        row.resolving |= resolving.contains(&pane.workspace_id);
    }
    let mut keitora = RowAccum::new("keitora".into(), PerfRowKind::Keitora);
    let mut herdr = RowAccum::new("herdr".into(), PerfRowKind::Herdr);
    let mut other = RowAccum::new("other".into(), PerfRowKind::Other);

    for (idx, p) in procs.iter().enumerate() {
        match owners.get(&p.pid) {
            Some(Owner::Herdr) => {
                herdr.add(idx, p, false);
                continue;
            }
            Some(Owner::Keitora) => {
                keitora.add(idx, p, false);
                continue;
            }
            Some(Owner::Session(ws)) => {
                if let Some(session) = session_of.get(ws.as_str()) {
                    let key = (session.project.clone(), session.worktree.clone());
                    worktrees
                        .entry(key)
                        .or_insert_with(|| RowAccum::worktree(session))
                        .add(idx, p, false);
                    continue;
                }
                // A workspace no pane belongs to any more: treat as unclaimed.
            }
            None => {}
        }
        match inferred.get(&(p.pid, p.start)) {
            Some(session) => {
                let key = (session.project.clone(), session.worktree.clone());
                worktrees
                    .entry(key)
                    .or_insert_with(|| RowAccum::worktree(session))
                    .add(idx, p, true);
            }
            None => other.add(idx, p, false),
        }
    }

    let mut out = Vec::new();
    let mut current: Option<RowAccum> = None;
    let mut pending: Vec<RowAccum> = Vec::new();
    for ((project, _), mut wt) in worktrees {
        wt.finish_top_procs(procs);
        if current.as_ref().and_then(|c| c.project.as_deref()) != Some(project.as_str()) {
            if let Some(done) = current.take() {
                out.push(done);
                out.append(&mut pending);
            }
            let mut row = RowAccum::new(format!("project:{project}"), PerfRowKind::Project);
            row.project = Some(project);
            current = Some(row);
        }
        if let Some(row) = current.as_mut() {
            row.absorb(&wt);
        }
        pending.push(wt);
    }
    if let Some(done) = current.take() {
        out.push(done);
        out.append(&mut pending);
    }
    for mut row in [keitora, herdr, other] {
        row.finish_top_procs(procs);
        out.push(row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_(pid: u32, ppid: Option<u32>, start: u64) -> ProcSample {
        ProcSample {
            pid,
            ppid,
            start,
            name: format!("p{pid}"),
            cpu_percent: 0.0,
            mem_bytes: 0,
            disk_read_bps: 0,
            disk_write_bps: 0,
        }
    }

    fn busy(pid: u32, cpu: f32, mem: u64) -> ProcSample {
        ProcSample {
            cpu_percent: cpu,
            mem_bytes: mem,
            ..proc_(pid, None, 100)
        }
    }

    fn root(ws: &str, pids: &[u32]) -> Root {
        Root {
            workspace_id: ws.into(),
            pids: pids.to_vec(),
        }
    }

    fn session(ws: &str) -> Owner {
        Owner::Session(ws.into())
    }

    fn wt(project: &str, worktree: &str, path: &str) -> WorktreeRoot {
        WorktreeRoot {
            project: project.into(),
            worktree: worktree.into(),
            path: path.into(),
        }
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn key(project: &str, worktree: &str) -> SessionKey {
        SessionKey {
            project: project.into(),
            worktree: worktree.into(),
        }
    }

    fn pane(pane_id: &str, ws: &str, project: &str, worktree: &str) -> PerfPane {
        PerfPane {
            pane_id: pane_id.into(),
            workspace_id: ws.into(),
            session: key(project, worktree),
        }
    }

    const KEITORA: u32 = 1;

    // ---- claim_owners

    #[test]
    fn tree_walk_claims_descendants() {
        let procs = vec![
            proc_(10, None, 100),
            proc_(11, Some(10), 101),
            proc_(12, Some(11), 102),
            proc_(13, Some(11), 102),
            proc_(20, None, 100),
        ];
        let owners = claim_owners(&procs, &[root("w1", &[10])], None, KEITORA);
        for pid in [10, 11, 12, 13] {
            assert_eq!(owners.get(&pid), Some(&session("w1")), "pid {pid}");
        }
        assert_eq!(owners.get(&20), None);
        assert_eq!(owners.len(), 4);
    }

    #[test]
    fn start_time_guard_rejects_an_older_child_and_accepts_equal_start() {
        let procs = vec![
            proc_(10, None, 100),
            // A stale parent pid: started before its "parent", so it is not one.
            proc_(11, Some(10), 99),
            proc_(12, Some(10), 100),
            proc_(13, Some(11), 120),
        ];
        let owners = claim_owners(&procs, &[root("w1", &[10])], None, KEITORA);
        assert_eq!(owners.get(&10), Some(&session("w1")));
        assert_eq!(owners.get(&12), Some(&session("w1")), "equal start allowed");
        assert_eq!(owners.get(&11), None, "older than its parent");
        assert_eq!(owners.get(&13), None, "nor its descendants");
    }

    #[test]
    fn a_missing_parent_is_not_followed() {
        // 11's parent 99 is not in the snapshot; 99 is a root but absent.
        let procs = vec![proc_(11, Some(99), 100), proc_(12, Some(11), 101)];
        let owners = claim_owners(&procs, &[root("w1", &[99])], None, KEITORA);
        assert!(owners.is_empty(), "{owners:?}");
    }

    #[test]
    fn pane_tree_beats_the_herdr_and_keitora_subtrees() {
        // Keitora (1) → herdr server (5) → pane shell (10) → agent (11);
        // herdr also has a ConPTY host (6).
        let procs = vec![
            proc_(KEITORA, None, 50),
            proc_(5, Some(KEITORA), 60),
            proc_(6, Some(5), 61),
            proc_(10, Some(5), 70),
            proc_(11, Some(10), 71),
        ];
        let owners = claim_owners(&procs, &[root("w1", &[10])], Some(5), KEITORA);
        assert_eq!(owners.get(&10), Some(&session("w1")));
        assert_eq!(owners.get(&11), Some(&session("w1")));
        assert_eq!(owners.get(&5), Some(&Owner::Herdr));
        assert_eq!(owners.get(&6), Some(&Owner::Herdr));
        assert_eq!(owners.get(&KEITORA), Some(&Owner::Keitora));
    }

    #[test]
    fn herdr_subtree_beats_keitora_when_the_server_is_keitoras_child() {
        let procs = vec![
            proc_(KEITORA, None, 50),
            proc_(2, Some(KEITORA), 51), // a WebView child
            proc_(3, Some(KEITORA), 52), // a bridge
            proc_(5, Some(KEITORA), 60), // the herdr server
            proc_(6, Some(5), 61),
        ];
        let owners = claim_owners(&procs, &[], Some(5), KEITORA);
        assert_eq!(owners.get(&5), Some(&Owner::Herdr));
        assert_eq!(owners.get(&6), Some(&Owner::Herdr));
        for pid in [KEITORA, 2, 3] {
            assert_eq!(owners.get(&pid), Some(&Owner::Keitora), "pid {pid}");
        }
    }

    #[test]
    fn an_absent_herdr_pid_claims_nothing() {
        let procs = vec![proc_(KEITORA, None, 50), proc_(7, None, 60)];
        let owners = claim_owners(&procs, &[], Some(5), KEITORA);
        assert_eq!(owners.get(&7), None);
        assert_eq!(owners.get(&KEITORA), Some(&Owner::Keitora));
        assert_eq!(owners.len(), 1);
    }

    #[test]
    fn a_multi_root_workspace_claims_a_foreground_pid_outside_the_shell_tree() {
        // Shell 10; the agent 30 was started by something else (Git Bash), so
        // only herdr's foreground list ties it to the pane.
        let procs = vec![
            proc_(10, None, 100),
            proc_(11, Some(10), 101),
            proc_(29, None, 90),
            proc_(30, Some(29), 102),
            proc_(31, Some(30), 103),
        ];
        let owners = claim_owners(&procs, &[root("w1", &[10, 30])], None, KEITORA);
        for pid in [10, 11, 30, 31] {
            assert_eq!(owners.get(&pid), Some(&session("w1")), "pid {pid}");
        }
        assert_eq!(owners.get(&29), None, "the launcher is not in the pane");
    }

    #[test]
    fn a_pid_is_claimed_once_and_roots_go_in_workspace_order() {
        // w2's shell 10 has a child 11 that w1 also reports as a foreground pid.
        // w1 sorts first, so 11 (and its child 12) stay with w1 even though w2
        // is listed first and 11 is in w2's shell tree.
        let procs = vec![
            proc_(10, None, 100),
            proc_(11, Some(10), 101),
            proc_(12, Some(11), 102),
            proc_(20, None, 100),
        ];
        let roots = [root("w2", &[10]), root("w1", &[20, 11])];
        let owners = claim_owners(&procs, &roots, None, KEITORA);
        assert_eq!(owners.get(&10), Some(&session("w2")));
        assert_eq!(owners.get(&20), Some(&session("w1")));
        assert_eq!(owners.get(&11), Some(&session("w1")));
        assert_eq!(owners.get(&12), Some(&session("w1")));

        // Same answer whatever order the roots arrive in.
        let reversed = [root("w1", &[20, 11]), root("w2", &[10])];
        assert_eq!(claim_owners(&procs, &reversed, None, KEITORA), owners);
    }

    #[test]
    fn a_parent_cycle_terminates() {
        // Pid reuse can make two processes each other's parent at equal starts.
        let procs = vec![proc_(10, Some(11), 100), proc_(11, Some(10), 100)];
        let owners = claim_owners(&procs, &[root("w1", &[10])], None, KEITORA);
        assert_eq!(owners.len(), 2);
        // A process naming itself as parent is not its own child.
        let selfish = vec![proc_(10, Some(10), 100)];
        let owners = claim_owners(&selfish, &[root("w1", &[10])], None, KEITORA);
        assert_eq!(owners.len(), 1);
    }

    // ---- match_root

    #[test]
    fn match_root_takes_the_longest_component_prefix() {
        let roots = [
            wt("proj", "main", "D:/repos/proj"),
            wt("proj", "alpha", "D:/repos/proj/wt/alpha"),
        ];
        assert_eq!(
            match_root("D:/repos/proj/wt/alpha/Source/x.cpp", &roots),
            Some(1)
        );
        assert_eq!(match_root("D:/repos/proj/Source", &roots), Some(0));
        assert_eq!(match_root("D:/repos/proj", &roots), Some(0));
        assert_eq!(match_root("D:/repos/other-proj", &roots), None);
        assert_eq!(match_root("", &roots), None);
        assert_eq!(match_root("D:/repos/proj/x", &[]), None);
        assert_eq!(roots[1].session(), key("proj", "alpha"));
    }

    #[test]
    fn match_root_rejects_a_sibling_prefix() {
        let roots = [wt("proj", "wt", "D:/repos/proj/wt")];
        assert_eq!(match_root("D:/repos/proj/wt2/Source", &roots), None);
        assert_eq!(match_root("D:/repos/proj/wt/Source", &roots), Some(0));
    }

    #[test]
    fn match_root_normalises_case_and_separators() {
        let roots = [wt("proj", "alpha", r"C:\Users\x\proj\alpha\")];
        assert_eq!(match_root("c:/users/X/PROJ/alpha/Saved", &roots), Some(0));
        assert_eq!(match_root(r"\\?\C:\Users\x\proj\alpha", &roots), Some(0));
        assert_eq!(match_root(r"C:\Users\x\proj\alpha\Source", &roots), Some(0));
    }

    #[test]
    fn match_root_prefers_the_nested_worktree() {
        let roots = [
            wt("proj", "alpha", "D:/repos/proj/alpha/nested"),
            wt("proj", "main", "D:/repos/proj"),
            wt("proj", "outer", "D:/repos/proj/alpha"),
        ];
        assert_eq!(match_root("D:/repos/proj/alpha/nested/a", &roots), Some(0));
        assert_eq!(match_root("D:/repos/proj/alpha/b", &roots), Some(2));
        assert_eq!(match_root("D:/repos/proj/c", &roots), Some(1));
    }

    #[test]
    fn match_root_equal_length_tie_is_none() {
        let roots = [
            wt("proj", "alpha", "D:/repos/shared"),
            wt("other-proj", "bravo", "D:/repos/shared"),
        ];
        assert_eq!(match_root("D:/repos/shared/x", &roots), None);
        // The same worktree registered twice is not a disagreement.
        let dup = [
            wt("proj", "alpha", "D:/repos/shared"),
            wt("proj", "alpha", r"D:\repos\shared"),
        ];
        assert_eq!(match_root("D:/repos/shared/x", &dup), Some(0));
        // A longer match after a tie still wins outright.
        let deeper = [
            wt("proj", "alpha", "D:/repos/shared"),
            wt("other-proj", "bravo", "D:/repos/shared"),
            wt("proj", "charlie", "D:/repos/shared/c"),
        ];
        assert_eq!(match_root("D:/repos/shared/c/x", &deeper), Some(2));
    }

    // ---- infer_worktree

    fn infer_roots() -> Vec<WorktreeRoot> {
        vec![
            wt("proj", "alpha", "C:/Users/x/proj/alpha"),
            wt("proj", "bravo", "C:/Users/x/proj/bravo"),
            wt("proj", "my game", "C:/Users/x/My Games/proj"),
        ]
    }

    #[test]
    fn infer_uses_a_ubt_build_project() {
        let roots = infer_roots();
        let ubt = argv(&[
            "UnrealBuildTool.exe",
            "MyGameEditor",
            "Win64",
            "Development",
            "-Project=C:/Users/x/proj/alpha/MyGame.uproject",
        ]);
        // The project wins over the cwd.
        assert_eq!(
            infer_worktree(&ubt, Some("C:/Users/x/proj/bravo"), &roots),
            Some(0)
        );
        let explicit_build = argv(&[
            "dotnet",
            "UnrealBuildTool.dll",
            "-Mode=Build",
            r"-Project=C:\Users\x\proj\bravo\MyGame.uproject",
        ]);
        assert_eq!(infer_worktree(&explicit_build, None, &roots), Some(1));
    }

    #[test]
    fn infer_reads_a_quoted_project_path_with_spaces() {
        let roots = infer_roots();
        let ubt = argv(&[
            "UnrealBuildTool.exe",
            r#"-Project="C:\Users\x\My Games\proj\MyGame.uproject""#,
        ]);
        assert_eq!(infer_worktree(&ubt, None, &roots), Some(2));
    }

    #[test]
    fn infer_ignores_the_project_of_a_non_build_mode() {
        let roots = infer_roots();
        let query = argv(&[
            "UnrealBuildTool.exe",
            "-Mode=QueryTargets",
            "-Project=C:/Users/x/proj/alpha/MyGame.uproject",
        ]);
        assert_eq!(
            infer_worktree(&query, Some("C:/Users/x/proj/bravo/Intermediate"), &roots),
            Some(1),
            "the cwd decides, not the project"
        );
    }

    #[test]
    fn infer_falls_back_to_the_cwd() {
        let roots = infer_roots();
        assert_eq!(
            infer_worktree(
                &argv(&["cl.exe", "x.cpp"]),
                Some(r"C:\Users\x\proj\bravo\Source"),
                &roots
            ),
            Some(1)
        );
        // A build project outside every worktree falls through to the cwd too.
        let elsewhere = argv(&["UnrealBuildTool.exe", "-Project=D:/scratch/Other.uproject"]);
        assert_eq!(
            infer_worktree(&elsewhere, Some("C:/Users/x/proj/alpha"), &roots),
            Some(0)
        );
    }

    #[test]
    fn infer_falls_back_to_cmdline_paths() {
        let roots = infer_roots();
        let worker = argv(&[
            "MSBuild.exe",
            "/nodemode:1",
            "-Log=C:/Users/x/proj/alpha/Saved/Logs/a.log",
            r#"/p:OutDir="C:\Users\x\proj\alpha\Binaries""#,
            "-Mode=Worker",
            "-Relative=Saved/Logs",
            "/bare",
            "C:/Users/x/proj/bravo/positional.txt",
        ]);
        assert_eq!(
            infer_worktree(&worker, Some("C:/Windows/System32"), &roots),
            Some(0),
            "positional paths and relative values are not considered"
        );
        let unc = argv(&["tool.exe", r"-In=\\srv\share\a", "-Out=/Users/me/elsewhere"]);
        assert_eq!(
            infer_worktree(&unc, None, &roots),
            None,
            "no worktree matches"
        );
        assert_eq!(infer_worktree(&[], None, &roots), None);
    }

    #[test]
    fn infer_disagreeing_cmdline_paths_is_none() {
        let roots = infer_roots();
        let worker = argv(&[
            "tool.exe",
            "-In=C:/Users/x/proj/alpha/a",
            "-Unmatched=D:/scratch/b",
            "-Out=C:/Users/x/proj/bravo/b",
        ]);
        assert_eq!(infer_worktree(&worker, None, &roots), None);
        let agreeing = argv(&[
            "tool.exe",
            "-In=C:/Users/x/proj/bravo/a",
            "-Unmatched=D:/scratch/b",
            "-Out=C:/Users/x/proj/bravo/b",
        ]);
        assert_eq!(infer_worktree(&agreeing, None, &roots), Some(1));
    }

    #[test]
    fn looks_absolute_forms() {
        for s in [r"C:\x", "c:/x", r"\\srv\share", "/usr/x"] {
            assert!(looks_absolute(s), "{s}");
        }
        for s in ["C:", "C:x", "x/y", "", "1:/x", "Saved/Logs"] {
            assert!(!looks_absolute(s), "{s}");
        }
    }

    // ---- rollup

    fn find<'a>(rows: &'a [RowAccum], key: &str) -> &'a RowAccum {
        rows.iter()
            .find(|r| r.key == key)
            .unwrap_or_else(|| panic!("no row {key} in {:?}", keys(rows)))
    }

    fn keys(rows: &[RowAccum]) -> Vec<&str> {
        rows.iter().map(|r| r.key.as_str()).collect()
    }

    #[test]
    fn rollup_sums_rows_and_projects() {
        let panes = vec![
            pane("w1:p1", "w1", "proj", "alpha"),
            pane("w2:p1", "w2", "proj", "bravo"),
        ];
        let procs = vec![
            ProcSample {
                disk_read_bps: 100,
                disk_write_bps: 10,
                ..busy(10, 1.5, 1000)
            },
            ProcSample {
                disk_read_bps: 50,
                disk_write_bps: 5,
                ..busy(11, 2.25, 500)
            },
            busy(20, 0.5, 300),
            busy(5, 0.25, 200),
            busy(KEITORA, 1.0, 400),
            busy(99, 4.0, 900),
        ];
        let owners = HashMap::from([
            (10, session("w1")),
            (11, session("w1")),
            (20, session("w2")),
            (5, Owner::Herdr),
            (KEITORA, Owner::Keitora),
        ]);
        let rows = rollup(&procs, &owners, &HashMap::new(), &panes, &HashSet::new());
        assert_eq!(
            keys(&rows),
            vec![
                "project:proj",
                "worktree:proj/alpha",
                "worktree:proj/bravo",
                "keitora",
                "herdr",
                "other"
            ]
        );

        let alpha = find(&rows, "worktree:proj/alpha");
        assert_eq!(alpha.kind, PerfRowKind::Worktree);
        assert_eq!(alpha.project.as_deref(), Some("proj"));
        assert_eq!(alpha.worktree.as_deref(), Some("alpha"));
        assert_eq!(alpha.cpu_percent, 3.75);
        assert_eq!(alpha.mem_bytes, 1500);
        assert_eq!(alpha.disk_read_bps, 150);
        assert_eq!(alpha.disk_write_bps, 15);
        assert_eq!(alpha.proc_count, 2);
        assert_eq!(alpha.inferred_count, 0);

        let project = find(&rows, "project:proj");
        assert_eq!(project.kind, PerfRowKind::Project);
        assert_eq!(project.project.as_deref(), Some("proj"));
        assert_eq!(project.worktree, None);
        assert_eq!(project.cpu_percent, 4.25, "alpha + bravo");
        assert_eq!(project.mem_bytes, 1800);
        assert_eq!(project.disk_read_bps, 150);
        assert_eq!(project.proc_count, 3);
        assert!(
            project.top_procs.is_empty(),
            "project rows list no processes"
        );

        let herdr = find(&rows, "herdr");
        assert_eq!(
            (herdr.cpu_percent, herdr.mem_bytes, herdr.proc_count),
            (0.25, 200, 1)
        );
        assert_eq!(herdr.project, None);
        let keitora = find(&rows, "keitora");
        assert_eq!((keitora.cpu_percent, keitora.proc_count), (1.0, 1));

        let other = find(&rows, "other");
        assert_eq!(
            (other.cpu_percent, other.mem_bytes, other.proc_count),
            (4.0, 900, 1)
        );
        assert_eq!(other.top_procs[0].pid, 99);
    }

    #[test]
    fn rollup_counts_inferred_processes() {
        let panes = vec![pane("w1:p1", "w1", "proj", "alpha")];
        let procs = vec![
            busy(10, 1.0, 100),
            ProcSample {
                start: 555,
                ..busy(40, 2.0, 200)
            },
            busy(41, 0.5, 50),
            busy(42, 0.25, 25),
        ];
        let owners = HashMap::from([(10, session("w1"))]);
        let inferred = HashMap::from([
            ((40, 555), key("proj", "alpha")),
            // No session for bravo: the row exists because it got a process.
            ((41, 100), key("proj", "bravo")),
            // Wrong start time: a reused pid, not the process inferred.
            ((42, 999), key("proj", "alpha")),
        ]);
        let rows = rollup(&procs, &owners, &inferred, &panes, &HashSet::new());

        let alpha = find(&rows, "worktree:proj/alpha");
        assert_eq!(alpha.proc_count, 2);
        assert_eq!(alpha.inferred_count, 1);
        assert_eq!(alpha.cpu_percent, 3.0);
        let flags: Vec<(u32, bool)> = alpha
            .top_procs
            .iter()
            .map(|p| (p.pid, p.inferred))
            .collect();
        assert_eq!(flags, vec![(40, true), (10, false)]);

        let bravo = find(&rows, "worktree:proj/bravo");
        assert_eq!((bravo.proc_count, bravo.inferred_count), (1, 1));
        assert!(!bravo.resolving);

        assert_eq!(find(&rows, "project:proj").inferred_count, 2);
        let other = find(&rows, "other");
        assert_eq!(other.proc_count, 1);
        assert!(!other.top_procs[0].inferred);
        assert_eq!(other.inferred_count, 0);
    }

    #[test]
    fn rollup_keeps_an_inferred_worktree_name_with_a_slash() {
        // With and without a session for it, the name is kept whole.
        let panes = vec![pane("w1:p1", "w1", "proj", "feature/x")];
        let procs = vec![busy(40, 1.0, 10), busy(41, 1.0, 10)];
        let inferred = HashMap::from([
            ((40, 100), key("proj", "feature/x")),
            ((41, 100), key("proj", "fix/y")),
        ]);
        let rows = rollup(&procs, &HashMap::new(), &inferred, &panes, &HashSet::new());
        let row = find(&rows, "worktree:proj/feature/x");
        assert_eq!(row.project.as_deref(), Some("proj"));
        assert_eq!(row.worktree.as_deref(), Some("feature/x"));
        assert_eq!(row.inferred_count, 1);
        let row = find(&rows, "worktree:proj/fix/y");
        assert_eq!(row.project.as_deref(), Some("proj"));
        assert_eq!(row.worktree.as_deref(), Some("fix/y"));
        assert_eq!(row.inferred_count, 1);
    }

    #[test]
    fn rollup_orders_and_caps_top_procs() {
        let mut procs: Vec<ProcSample> = (0..12).map(|i| busy(100 + i, 0.0, 10)).collect();
        procs.push(busy(50, 3.0, 1));
        procs.push(busy(60, 3.0, 5)); // same cpu, more memory: first
        procs.push(busy(70, 1.0, 7));
        procs.push(busy(71, 1.0, 7)); // full tie: lower pid first
        let rows = rollup(
            &procs,
            &HashMap::new(),
            &HashMap::new(),
            &[],
            &HashSet::new(),
        );
        let other = find(&rows, "other");
        assert_eq!(other.proc_count, 16);
        assert_eq!(other.top_procs.len(), TOP_PROCS);
        let pids: Vec<u32> = other.top_procs.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![60, 50, 70, 71, 100, 101, 102, 103]);
    }

    #[test]
    fn rollup_row_emission_rules() {
        // No panes, no processes: only the three fixed rows, zeroed.
        let rows = rollup(&[], &HashMap::new(), &HashMap::new(), &[], &HashSet::new());
        assert_eq!(keys(&rows), vec!["keitora", "herdr", "other"]);
        for row in &rows {
            assert_eq!(
                (row.cpu_percent, row.mem_bytes, row.proc_count),
                (0.0, 0, 0)
            );
            assert!(row.top_procs.is_empty());
        }

        // A session with a pane but no processes still has its rows, and a
        // resolving workspace marks both its worktree and its project.
        let panes = vec![
            pane("w1:p1", "w1", "proj", "alpha"),
            pane("w1:p2", "w1", "proj", "alpha"),
            pane("w3:p1", "w3", "other-proj", "main"),
        ];
        let resolving = HashSet::from(["w3".to_string()]);
        let rows = rollup(&[], &HashMap::new(), &HashMap::new(), &panes, &resolving);
        assert_eq!(
            keys(&rows),
            vec![
                "project:other-proj",
                "worktree:other-proj/main",
                "project:proj",
                "worktree:proj/alpha",
                "keitora",
                "herdr",
                "other"
            ]
        );
        assert!(find(&rows, "worktree:other-proj/main").resolving);
        assert!(find(&rows, "project:other-proj").resolving);
        assert!(!find(&rows, "worktree:proj/alpha").resolving);
        assert!(!find(&rows, "project:proj").resolving);
        assert_eq!(find(&rows, "worktree:proj/alpha").proc_count, 0);
    }

    #[test]
    fn rollup_sends_unknown_workspaces_and_the_rest_to_other() {
        let panes = vec![pane("w1:p1", "w1", "proj", "alpha")];
        let procs = vec![busy(10, 1.0, 1), busy(11, 2.0, 2), busy(12, 0.5, 4)];
        // w9 has no pane any more (a stale root): its process is unclaimed.
        let owners = HashMap::from([(10, session("w1")), (11, session("w9"))]);
        let rows = rollup(&procs, &owners, &HashMap::new(), &panes, &HashSet::new());
        let other = find(&rows, "other");
        assert_eq!(other.proc_count, 2);
        assert_eq!(other.cpu_percent, 2.5);
        assert_eq!(other.mem_bytes, 6);
        assert!(!rows.iter().any(|r| r.key.contains("w9")));
    }

    #[test]
    fn into_row_attaches_history() {
        let mut acc = RowAccum::new("other".into(), PerfRowKind::Other);
        let procs = [busy(7, 1.5, 64)];
        acc.add(0, &procs[0], false);
        acc.finish_top_procs(&procs);
        let history = PerfHistory {
            cpu: vec![0.5, 1.5],
            mem: vec![32, 64],
        };
        let row = acc.into_row(history.clone());
        assert_eq!(row.key, "other");
        assert_eq!(row.kind, PerfRowKind::Other);
        assert_eq!(row.cpu_percent, 1.5);
        assert_eq!(row.mem_bytes, 64);
        assert_eq!(row.proc_count, 1);
        assert_eq!(row.history, history);
        assert_eq!(row.top_procs.len(), 1);
    }
}
