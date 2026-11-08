//! `reconcile` — bring the keymap in line with the herdr server actually running.
//!
//! Each herdr server process is one [`ServerInstance`]. When the instance is the
//! one the keymap was last reconciled against (or the keymap has no baseline yet),
//! the live panes are re-adopted by cwd, exactly as an app restart over a surviving
//! server needs. When the server was replaced, the panes herdr restored are bare
//! shells with new ids: they are closed, the server generation moves on, and the
//! sessions that were running are returned for a resume. Reconcile never starts an
//! agent itself and never adopts a restored shell as if it were the agent.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use crate::backend::{PtyBackendError, SessionKey};

use super::bridge::Bridge;
use super::cli::{AgentInfo, PaneInfo};
use super::keymap::{self, HerdrPane, KeyMap, LivePane};
use super::ops::{Generation, HerdrOps, ServerInstance};
use super::resume::{observation_pairs, ResumeStore};

/// A just-restarted herdr server answers its socket before it has finished
/// restoring panes, so `pane_list` can transiently return empty. Poll it a few
/// times; if it stays empty the persisted keymap is left untouched rather than
/// wiped (clobbering a not-yet-restored map would duplicate workspaces).
const RECONCILE_POLL_ATTEMPTS: u32 = 12;
/// Delay between [`RECONCILE_POLL_ATTEMPTS`] polls (~3 s total budget).
const RECONCILE_POLL_INTERVAL_MS: u64 = 250;
/// A restore counts as settled once this many answers in a row list the same
/// panes (herdr may restore a session's panes one by one).
const SETTLE_POLLS: u32 = 3;
/// After closing restored shells, poll until herdr no longer lists them.
const WAIT_GONE_INTERVAL_MS: u64 = 200;
/// Budget of [`WAIT_GONE_INTERVAL_MS`] polls (~5 s).
const WAIT_GONE_ATTEMPTS: u32 = 25;

/// Sleep between polls. Unit tests drive a fake that answers instantly, so they
/// only yield briefly instead of waiting out the real intervals.
fn pause(ms: u64) {
    let ms = if cfg!(test) { ms.min(2) } else { ms };
    std::thread::sleep(Duration::from_millis(ms));
}

/// How the running server relates to the one the keymap knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lineage {
    /// The same server process.
    SameInstance,
    /// The server process was replaced.
    NewInstance,
    /// The keymap has no recorded instance (first run, or a legacy keymap).
    UnknownBaseline,
}

/// Pure. `persisted` is the keymap's instance, `observed` the probe result. When
/// the running instance cannot be determined the non-destructive answer
/// (`SameInstance`, adopt) is chosen.
/// An identity whose start time is unknown is matched by pid alone; a wholly
/// unknown remembered identity is no baseline.
pub fn classify(persisted: Option<ServerInstance>, observed: Option<ServerInstance>) -> Lineage {
    match (persisted, observed) {
        (_, None) => Lineage::SameInstance,
        (_, Some(o)) if o.pid == 0 => Lineage::SameInstance,
        (None, Some(_)) => Lineage::UnknownBaseline,
        (Some(p), Some(o)) => match p.same_process(&o) {
            Some(true) => Lineage::SameInstance,
            Some(false) => Lineage::NewInstance,
            None => Lineage::UnknownBaseline,
        },
    }
}

/// The identity to remember for `observed`: never a less complete one for the
/// same process than the keymap already has.
fn identity_to_keep(persisted: Option<ServerInstance>, observed: ServerInstance) -> ServerInstance {
    match persisted {
        Some(p)
            if p.start_time != 0
                && observed.start_time == 0
                && p.same_process(&observed) == Some(true) =>
        {
            p
        }
        _ => observed,
    }
}

/// Pure. After a new instance: did the agents survive (a live handoff, which
/// herdr only does on unix)? True iff `allow_handoff` and at least one persisted
/// pane id is live and still reports an agent.
pub fn is_handoff(
    allow_handoff: bool,
    persisted: &[(SessionKey, HerdrPane)],
    live: &[PaneInfo],
) -> bool {
    allow_handoff
        && persisted.iter().any(|(_, p)| {
            live.iter()
                .any(|l| l.pane_id == p.pane_id && l.agent.is_some())
        })
}

/// Pure. The keys owed a resume: the explicit recovery set plus every persisted
/// key, minus the keys whose agent the user had quit. Sorted, no duplicates.
pub fn keys_to_resume(
    sbr: &[SessionKey],
    persisted: &[(SessionKey, HerdrPane)],
    quit: &HashSet<SessionKey>,
) -> Vec<SessionKey> {
    let all: BTreeSet<(String, String)> = sbr
        .iter()
        .chain(persisted.iter().map(|(k, _)| k))
        .filter(|k| !quit.contains(*k))
        .map(|k| (k.project.clone(), k.worktree.clone()))
        .collect();
    all.into_iter()
        .map(|(project, worktree)| SessionKey { project, worktree })
        .collect()
}

/// What [`reconcile`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// Same lineage: the live panes were re-adopted.
    Adopted,
    /// These sessions (sorted) are owed a resume: a replaced server's restored
    /// shells were closed, or an adopted server has no pane for them.
    NewGeneration { to_resume: Vec<SessionKey> },
}

/// The backend parts [`reconcile`] works on.
pub struct ReconcileCtx<'a> {
    pub ops: &'a dyn HerdrOps,
    pub keymap: &'a KeyMap,
    pub live: &'a Mutex<HashSet<SessionKey>>,
    pub store: &'a ResumeStore,
    pub bridges: &'a Mutex<HashMap<SessionKey, Bridge>>,
    /// `false` => the project is no longer registered or the worktree is gone
    /// from its config.
    pub worktree_exists: &'a dyn Fn(&SessionKey) -> bool,
}

/// Poll `pane_list` until some pane reports a cwd (restore may still be
/// running), within [`RECONCILE_POLL_ATTEMPTS`]. `None` on a transport error.
fn poll_restored_panes(ops: &dyn HerdrOps) -> Option<Vec<PaneInfo>> {
    let mut panes = Vec::new();
    for attempt in 0..RECONCILE_POLL_ATTEMPTS {
        match ops.pane_list() {
            Ok(p) => {
                panes = p;
                if panes.iter().any(|p| p.cwd.is_some()) {
                    break;
                }
            }
            Err(e) => {
                log::warn!(
                    "herdr: reconcile pane_list failed: {}",
                    PtyBackendError::from(e)
                );
                return None;
            }
        }
        if attempt + 1 < RECONCILE_POLL_ATTEMPTS {
            pause(RECONCILE_POLL_INTERVAL_MS);
        }
    }
    Some(panes)
}

/// Poll `pane_list` until the restore has settled: every cwd in `expected`
/// is listed, or [`SETTLE_POLLS`] answers in a row list the same non-empty
/// set of panes. A transport error is retried within the budget; an error on
/// the last attempt (or no answer at all) fails, since closing shells against
/// a partial list would orphan the rest. A budget spent on answers that never
/// settle returns the last one.
fn poll_settled_panes(
    ops: &dyn HerdrOps,
    expected: &HashSet<String>,
) -> Result<Vec<PaneInfo>, String> {
    let mut last: Result<Vec<PaneInfo>, String> = Err("no pane list".into());
    let mut previous: Option<BTreeSet<String>> = None;
    let mut same = 0u32;
    for attempt in 0..RECONCILE_POLL_ATTEMPTS {
        match ops.pane_list() {
            Ok(panes) => {
                let found: HashSet<String> = panes
                    .iter()
                    .filter_map(|p| p.cwd.as_deref().map(keymap::normalize_cwd))
                    .collect();
                if expected.iter().all(|c| found.contains(c)) {
                    return Ok(panes);
                }
                let ids: BTreeSet<String> = panes
                    .iter()
                    .filter(|p| p.cwd.is_some())
                    .map(|p| p.pane_id.clone())
                    .collect();
                same = if !ids.is_empty() && previous.as_ref() == Some(&ids) {
                    same + 1
                } else {
                    1
                };
                if same >= SETTLE_POLLS {
                    return Ok(panes);
                }
                previous = Some(ids);
                last = Ok(panes);
            }
            Err(e) => {
                let e = PtyBackendError::from(e).to_string();
                log::warn!("herdr: reconcile pane_list failed: {e}");
                last = Err(e);
            }
        }
        if attempt + 1 < RECONCILE_POLL_ATTEMPTS {
            pause(RECONCILE_POLL_INTERVAL_MS);
        }
    }
    last
}

/// herdr's live agents, so reconcile can tell a session's named agent from a
/// shell in the same cwd. A failure degrades to none (plain cwd matching).
fn live_agents(ops: &dyn HerdrOps) -> Vec<AgentInfo> {
    ops.agent_list().unwrap_or_else(|e| {
        log::warn!("herdr: reconcile agent_list failed, matching by cwd only: {e}");
        Vec::new()
    })
}

/// Only panes that report a cwd can be matched to a worktree.
fn with_cwd(panes: &[PaneInfo]) -> Vec<LivePane> {
    panes
        .iter()
        .filter_map(|p| {
            p.cwd.as_ref().map(|cwd| LivePane {
                workspace_id: p.workspace_id.clone(),
                pane_id: p.pane_id.clone(),
                terminal_id: p.terminal_id.clone(),
                cwd: cwd.clone(),
            })
        })
        .collect()
}

/// Re-adopt the live panes into the keymap by cwd (the duplicate-workspace fix).
/// Best-effort: an empty keymap or a `pane_list` error is a no-op, and so is a
/// poll that finds no live panes (a server mid-restore), which must not wipe the
/// persisted map. Adopted entries join the current server generation, and the
/// live panes' recorded sessions are fed to the resume store.
pub fn adopt_live_panes(
    ops: &dyn HerdrOps,
    keymap: &KeyMap,
    live: &Mutex<HashSet<SessionKey>>,
    store: &ResumeStore,
) {
    let persisted = keymap.entries();
    if persisted.is_empty() {
        return; // nothing to adopt
    }
    let Some(panes) = poll_restored_panes(ops) else {
        return;
    };
    adopt_from(ops, &panes, &persisted, keymap, live, store);
}

/// The adopt step over an already-polled pane list.
fn adopt_from(
    ops: &dyn HerdrOps,
    panes: &[PaneInfo],
    persisted: &[(SessionKey, HerdrPane)],
    keymap: &KeyMap,
    live: &Mutex<HashSet<SessionKey>>,
    store: &ResumeStore,
) {
    let live_in = with_cwd(panes);
    if live_in.is_empty() {
        log::warn!(
            "herdr: reconcile found no live panes with a cwd after polling; \
             keeping the persisted keymap ({} entr{}) untouched",
            persisted.len(),
            if persisted.len() == 1 { "y" } else { "ies" }
        );
        return;
    }
    let generation = keymap.generation();
    let agents = live_agents(ops);
    let kind_of = |k: &SessionKey| store.last_cli(k);
    let reconciled: Vec<(SessionKey, HerdrPane)> =
        keymap::reconcile_panes(persisted, &live_in, &agents, &kind_of, true)
            .into_iter()
            .map(|(k, mut p)| {
                p.server_generation = generation;
                (k, p)
            })
            .collect();
    let adopted: Vec<SessionKey> = reconciled.iter().map(|(k, _)| k.clone()).collect();
    // Snapshot-CAS merge: a session spawned while we polled is left intact; a
    // gone-and-unchanged session is dropped.
    keymap.adopt_reconciled(persisted, reconciled);
    // Add the adopted keys to the live set and drop only snapshot keys the keymap
    // no longer holds. Never clear the whole set: that would drop a session spawned
    // concurrently.
    {
        let mut l = live.lock().unwrap();
        for k in &adopted {
            l.insert(k.clone());
        }
        for (k, _) in persisted {
            if keymap.get(k).is_none() {
                l.remove(k);
            }
        }
    }
    store.observe(&observation_pairs(&keymap.entries(), panes));
    log::info!(
        "herdr: restore reconcile adopted {} live pane(s) by cwd",
        adopted.len()
    );
}

/// Poll until none of `closed` is listed any more; on timeout warn and go on
/// (the keymap no longer references them and a spawn creates new workspaces).
fn wait_until_gone(ops: &dyn HerdrOps, closed: &HashSet<String>) {
    if closed.is_empty() {
        return;
    }
    for attempt in 0..WAIT_GONE_ATTEMPTS {
        if let Ok(panes) = ops.pane_list() {
            if !panes.iter().any(|p| closed.contains(&p.pane_id)) {
                return;
            }
        }
        if attempt + 1 < WAIT_GONE_ATTEMPTS {
            pause(WAIT_GONE_INTERVAL_MS);
        }
    }
    log::warn!(
        "herdr: {} closed restored pane(s) still listed after waiting; continuing",
        closed.len()
    );
}

/// Reconcile the keymap against the running server `observed`.
/// `next_generation` is the generation a new lineage is stamped with: callers
/// that know the server was replaced bump first and pass the bumped value.
/// Fails (changing nothing) when the replaced server's panes cannot be listed.
pub fn reconcile(
    ctx: &ReconcileCtx,
    next_generation: Generation,
    observed: ServerInstance,
) -> Result<ReconcileOutcome, String> {
    match classify(ctx.keymap.instance(), Some(observed)) {
        Lineage::SameInstance | Lineage::UnknownBaseline => {
            adopt_live_panes(ctx.ops, ctx.keymap, ctx.live, ctx.store);
            if observed.pid != 0 {
                let keep = identity_to_keep(ctx.keymap.instance(), observed);
                ctx.keymap.set_instance(Some(keep));
            }
            Ok(ReconcileOutcome::Adopted)
        }
        Lineage::NewInstance => {
            reconcile_lineage(ctx, next_generation, observed, replaced_survival())
        }
    }
}

/// Whether the panes a reconcile finds are still the remembered sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Survival {
    /// The same server process: its panes are the sessions.
    Known,
    /// Unknown: they are only if a remembered pane still runs its agent (a live
    /// handoff, or a server that never really went away).
    IfAgentsLive,
    /// A replaced server: its panes are restored shells.
    Never,
}

/// A different server instance hands its agents over only on unix.
fn replaced_survival() -> Survival {
    if cfg!(unix) {
        Survival::IfAgentsLive
    } else {
        Survival::Never
    }
}

/// The recovery controller's reconcile: the server was just (re)started or
/// found replaced, and every session it interrupted is in the recovery set.
/// Unlike [`reconcile`], an unknown baseline never adopts the panes as they
/// are: only a server known to be the same process, or panes whose agents
/// still run, are adopted; otherwise the panes are restored shells and are
/// replaced. Either way every session in the recovery set ends up returned
/// for a resume, running again, or resolved.
pub fn reconcile_restarted(
    ctx: &ReconcileCtx,
    next_generation: Generation,
    observed: ServerInstance,
) -> Result<ReconcileOutcome, String> {
    let survival = match classify(ctx.keymap.instance(), Some(observed)) {
        Lineage::SameInstance if observed.pid != 0 => Survival::Known,
        Lineage::NewInstance => replaced_survival(),
        Lineage::SameInstance | Lineage::UnknownBaseline => Survival::IfAgentsLive,
    };
    reconcile_lineage(ctx, next_generation, observed, survival)
}

/// Bring the keymap onto `observed` in `next_generation`.
fn reconcile_lineage(
    ctx: &ReconcileCtx,
    next_generation: Generation,
    observed: ServerInstance,
    survival: Survival,
) -> Result<ReconcileOutcome, String> {
    let entries = ctx.keymap.entries();
    let expected: HashSet<String> = entries
        .iter()
        .filter(|(_, p)| p.server_generation != next_generation)
        .map(|(_, p)| keymap::held_cwd(&p.cwd))
        .collect();
    // Nothing changes before the restore is known: a partial list would leave
    // the unlisted shells open with nothing referencing them.
    let panes = poll_settled_panes(ctx.ops, &expected)?;
    // Sessions already recorded in this generation whose pane runs (resumed,
    // or started again by the user) are what they should be: left alone.
    let listed: HashSet<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
    let (current, persisted): (Vec<_>, Vec<_>) = entries.into_iter().partition(|(_, p)| {
        p.server_generation == next_generation && listed.contains(p.pane_id.as_str())
    });
    let current_keys: HashSet<SessionKey> = current.iter().map(|(k, _)| k.clone()).collect();
    ctx.live
        .lock()
        .unwrap()
        .extend(current_keys.iter().cloned());
    let current_panes: HashSet<&str> = current.iter().map(|(_, p)| p.pane_id.as_str()).collect();
    let panes: Vec<PaneInfo> = panes
        .iter()
        .filter(|p| !current_panes.contains(p.pane_id.as_str()))
        .cloned()
        .collect();

    let adopt = match survival {
        Survival::Known => true,
        Survival::IfAgentsLive => is_handoff(true, &persisted, &panes),
        Survival::Never => false,
    };
    if adopt {
        log::info!("herdr: the server still holds the sessions' panes; adopting them");
        adopt_from(ctx.ops, &panes, &persisted, ctx.keymap, ctx.live, ctx.store);
        if observed.pid != 0 {
            let keep = identity_to_keep(ctx.keymap.instance(), observed);
            ctx.keymap.set_instance(Some(keep));
        }
        // What the adopted panes do not cover is still owed.
        let mut to_resume = Vec::new();
        for k in ctx.keymap.should_be_running() {
            let running = ctx.keymap.get_current(&k).is_some();
            if running || ctx.store.skip_resume(&k) || !(ctx.worktree_exists)(&k) {
                ctx.keymap.resolve_recovered(&k);
            } else {
                to_resume.push(k);
            }
        }
        return Ok(if to_resume.is_empty() {
            ReconcileOutcome::Adopted
        } else {
            ReconcileOutcome::NewGeneration { to_resume }
        });
    }

    // a. Which sessions are owed a resume, and which worktrees are gone. A
    // session running again, whose agent was quit or that was terminated is not
    // owed one.
    let sbr = ctx.keymap.should_be_running();
    let quit: HashSet<SessionKey> = sbr
        .iter()
        .chain(persisted.iter().map(|(k, _)| k))
        .filter(|k| ctx.store.skip_resume(k))
        .cloned()
        .collect();
    for k in &sbr {
        if current_keys.contains(k) || quit.contains(k) {
            ctx.keymap.resolve_recovered(k);
        }
    }
    let (to_resume, dropped): (Vec<SessionKey>, Vec<SessionKey>) =
        keys_to_resume(&sbr, &persisted, &quit)
            .into_iter()
            .filter(|k| !current_keys.contains(k))
            .partition(|k| (ctx.worktree_exists)(k));
    for k in &dropped {
        ctx.keymap.resolve_recovered(k);
    }
    // Persisted before anything is closed, so a crash mid-reconcile still knows
    // what to resume.
    ctx.keymap.begin_recovery(&to_resume);
    let gone: HashSet<&SessionKey> = to_resume.iter().chain(dropped.iter()).collect();

    // b. The old bridges stream the replaced server's terminals.
    super::stop_bridges_except_in(ctx.bridges, &current_keys);

    // c. Close every restored shell of the resumed and dropped sessions (a
    // worktree can have been restored more than once); a session whose agent was
    // quit keeps one shell, adopted in the new generation.
    let live_in = with_cwd(&panes);
    let agents = live_agents(ctx.ops);
    let kind_of = |k: &SessionKey| ctx.store.last_cli(k);
    let keep: Vec<(SessionKey, HerdrPane)> =
        keymap::reconcile_panes(&persisted, &live_in, &agents, &kind_of, false)
            .into_iter()
            .filter(|(k, _)| !gone.contains(k))
            .map(|(k, pane)| {
                (
                    k,
                    HerdrPane {
                        server_generation: next_generation,
                        ..pane
                    },
                )
            })
            .collect();
    let kept_panes: HashSet<&str> = keep.iter().map(|(_, p)| p.pane_id.as_str()).collect();
    let gone_cwds: HashSet<String> = persisted
        .iter()
        .filter(|(k, _)| gone.contains(k))
        .map(|(_, p)| keymap::held_cwd(&p.cwd))
        .collect();
    let mut closed = HashSet::new();
    for lp in &live_in {
        if kept_panes.contains(lp.pane_id.as_str())
            || !gone_cwds.contains(&keymap::normalize_cwd(&lp.cwd))
        {
            continue;
        }
        match ctx.ops.pane_close(&lp.pane_id) {
            Ok(()) => {}
            Err(e) if e.is_not_found() => {}
            Err(e) => log::warn!(
                "herdr: closing restored pane {} ({}) failed: {}",
                lp.pane_id,
                lp.cwd,
                PtyBackendError::from(e)
            ),
        }
        closed.insert(lp.pane_id.clone());
    }

    // d. Adopt the kept shells; everything else from the snapshot leaves the map.
    let kept: Vec<SessionKey> = keep.iter().map(|(k, _)| k.clone()).collect();
    ctx.keymap.adopt_reconciled(&persisted, keep);
    {
        let mut l = ctx.live.lock().unwrap();
        for k in &gone {
            l.remove(*k);
        }
        for (k, _) in &persisted {
            if ctx.keymap.get(k).is_none() {
                l.remove(k);
            }
        }
        for k in kept {
            l.insert(k);
        }
    }

    // e. Wait until herdr no longer lists the closed panes.
    wait_until_gone(ctx.ops, &closed);

    // f. The new lineage (an unidentified server leaves the instance unknown).
    if observed.pid != 0 {
        let keep = identity_to_keep(ctx.keymap.instance(), observed);
        ctx.keymap.set_instance(Some(keep));
    }
    ctx.keymap.set_generation(next_generation);
    log::info!(
        "herdr: server replaced; closed {} restored shell(s), {} session(s) to resume, {} dropped",
        closed.len(),
        to_resume.len(),
        dropped.len()
    );
    Ok(ReconcileOutcome::NewGeneration { to_resume })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::backend::herdr::cli::{AgentSession, AgentSessionKind};
    use crate::backend::herdr::fake::FakeHerdr;
    use crate::backend::herdr::ops::Op;

    fn key(w: &str) -> SessionKey {
        SessionKey {
            project: "p".into(),
            worktree: w.into(),
        }
    }

    fn inst(pid: u32) -> ServerInstance {
        ServerInstance {
            pid,
            start_time: 1_000_000,
        }
    }

    fn session(id: &str) -> AgentSession {
        AgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            kind: AgentSessionKind::Id,
            value: id.into(),
        }
    }

    fn entry(pane_id: &str) -> HerdrPane {
        HerdrPane {
            workspace_id: "w".into(),
            pane_id: pane_id.into(),
            terminal_id: "t".into(),
            gen: 1,
            cwd: "/c".into(),
            server_generation: Generation(0),
        }
    }

    /// A fake with one agent pane per worktree, a keymap mapping them, and the
    /// keymap's baseline set to the fake's current instance.
    struct Setup {
        fake: Arc<FakeHerdr>,
        keymap: KeyMap,
        live: Mutex<HashSet<SessionKey>>,
        store: ResumeStore,
        bridges: Mutex<HashMap<SessionKey, Bridge>>,
    }

    impl Setup {
        fn new(worktrees: &[&str]) -> Setup {
            Self::build(worktrees, false)
        }

        /// Like [`Setup::new`], but the first workspace id is already used up,
        /// so a restore (which numbers from `w1` again) hands the old ids to
        /// other worktrees.
        fn with_gap(worktrees: &[&str]) -> Setup {
            Self::build(worktrees, true)
        }

        fn build(worktrees: &[&str], gap: bool) -> Setup {
            let fake = FakeHerdr::new();
            if gap {
                let p = fake.add_pane("/scratch", None);
                fake.pane_close(&p.pane_id).unwrap();
            }
            let keymap = KeyMap::with_path(fake.env().keymap_path());
            let live = Mutex::new(HashSet::new());
            for w in worktrees {
                let cwd = format!("/repos/{w}");
                let p = fake.add_pane(&cwd, Some(("claude", session(&format!("s-{w}")))));
                keymap.record(key(w), p.workspace_id, p.pane_id, p.terminal_id, cwd);
                live.lock().unwrap().insert(key(w));
            }
            keymap.set_instance(fake.server_instance());
            let store = ResumeStore::open(&fake.env().resume_db_path()).unwrap();
            Setup {
                fake,
                keymap,
                live,
                store,
                bridges: Mutex::new(HashMap::new()),
            }
        }

        fn run(&self, exists: &dyn Fn(&SessionKey) -> bool) -> ReconcileOutcome {
            self.try_run(exists).expect("reconciled")
        }

        fn try_run(
            &self,
            exists: &dyn Fn(&SessionKey) -> bool,
        ) -> Result<ReconcileOutcome, String> {
            let ctx = ReconcileCtx {
                ops: &*self.fake,
                keymap: &self.keymap,
                live: &self.live,
                store: &self.store,
                bridges: &self.bridges,
                worktree_exists: exists,
            };
            let observed = self.fake.server_instance().expect("server up");
            reconcile(&ctx, self.keymap.generation().next(), observed)
        }

        /// The controller's reconcile after a death noted for every session:
        /// all of them owed a resume, the generation bumped.
        fn after_death(&self, exists: &dyn Fn(&SessionKey) -> bool) -> ReconcileOutcome {
            let owed: Vec<SessionKey> = self.keymap.entries().into_iter().map(|(k, _)| k).collect();
            self.keymap.begin_recovery(&owed);
            let generation = self.keymap.note_death_bump();
            self.restarted(generation, exists)
        }

        fn restarted(
            &self,
            generation: Generation,
            exists: &dyn Fn(&SessionKey) -> bool,
        ) -> ReconcileOutcome {
            let ctx = ReconcileCtx {
                ops: &*self.fake,
                keymap: &self.keymap,
                live: &self.live,
                store: &self.store,
                bridges: &self.bridges,
                worktree_exists: exists,
            };
            let observed = self.fake.server_instance().expect("server up");
            reconcile_restarted(&ctx, generation, observed).expect("reconciled")
        }

        /// Kill the server and bring a new instance up (shells restored).
        fn restart_server(&self) {
            self.fake.kill_server(Some(1));
            let _ = self.fake.spawn_server().unwrap();
        }

        fn ops(&self) -> Vec<Op> {
            self.fake.calls().into_iter().map(|c| c.op).collect()
        }

        fn quit(&self, w: &str) {
            keitora_tasks::LocalStore::open(&self.fake.env().resume_db_path().to_string_lossy())
                .unwrap()
                .resume_observe(
                    "p",
                    w,
                    keitora_tasks::CliKind::Claude,
                    Some("q"),
                    "herdr:claude",
                    false,
                    1,
                )
                .unwrap();
        }

        fn terminate(&self, w: &str) {
            self.store.set_terminated(&key(w), true).unwrap();
        }
    }

    fn always(_: &SessionKey) -> bool {
        true
    }

    #[test]
    fn classify_compares_instances() {
        assert_eq!(
            classify(Some(inst(1)), Some(inst(1))),
            Lineage::SameInstance
        );
        assert_eq!(classify(Some(inst(1)), Some(inst(2))), Lineage::NewInstance);
        assert_eq!(classify(None, Some(inst(1))), Lineage::UnknownBaseline);
        assert_eq!(classify(Some(inst(1)), None), Lineage::SameInstance);
        assert_eq!(classify(None, None), Lineage::SameInstance);
    }

    #[test]
    fn classify_matches_an_identity_without_a_start_time_by_pid() {
        let pid_only = |pid| ServerInstance { pid, start_time: 0 };
        assert_eq!(
            classify(Some(pid_only(1)), Some(inst(1))),
            Lineage::SameInstance
        );
        assert_eq!(
            classify(Some(inst(1)), Some(pid_only(1))),
            Lineage::SameInstance
        );
        assert_eq!(
            classify(Some(pid_only(1)), Some(inst(2))),
            Lineage::NewInstance
        );
        assert_eq!(
            classify(Some(pid_only(0)), Some(inst(1))),
            Lineage::UnknownBaseline,
            "nothing known about the remembered server"
        );
        assert_eq!(
            classify(Some(inst(1)), Some(pid_only(0))),
            Lineage::SameInstance,
            "nothing known about the running server: adopt"
        );
    }

    #[test]
    fn a_remembered_identity_without_a_start_time_adopts_the_same_server() {
        let s = Setup::new(&["a"]);
        let live = s.fake.server_instance().unwrap();
        s.keymap.set_instance(Some(ServerInstance {
            pid: live.pid,
            start_time: 0,
        }));
        assert_eq!(s.run(&always), ReconcileOutcome::Adopted);
        assert!(s.fake.closed_pane_ids().is_empty(), "nothing closed");
        assert!(s.keymap.get(&key("a")).is_some());
        assert_eq!(s.keymap.instance(), Some(live), "the full identity is kept");
    }

    #[test]
    fn a_pid_only_observation_does_not_replace_a_known_identity() {
        let s = Setup::new(&["a"]);
        let live = s.fake.server_instance().unwrap();
        let ctx = ReconcileCtx {
            ops: &*s.fake,
            keymap: &s.keymap,
            live: &s.live,
            store: &s.store,
            bridges: &s.bridges,
            worktree_exists: &always,
        };
        let observed = ServerInstance {
            pid: live.pid,
            start_time: 0,
        };
        let outcome = reconcile(&ctx, s.keymap.generation().next(), observed);
        assert_eq!(outcome, Ok(ReconcileOutcome::Adopted));
        assert_eq!(s.keymap.instance(), Some(live));
    }

    #[test]
    fn is_handoff_needs_a_live_agent_pane_with_the_same_id() {
        let persisted = vec![(key("a"), entry("w1:p1"))];
        let agent = PaneInfo {
            pane_id: "w1:p1".into(),
            agent: Some("claude".into()),
            ..Default::default()
        };
        let shell = PaneInfo {
            pane_id: "w1:p1".into(),
            ..Default::default()
        };
        let other = PaneInfo {
            pane_id: "w2:p1".into(),
            agent: Some("claude".into()),
            ..Default::default()
        };
        assert!(is_handoff(true, &persisted, std::slice::from_ref(&agent)));
        assert!(!is_handoff(false, &persisted, &[agent]), "not allowed");
        assert!(!is_handoff(true, &persisted, &[shell]), "a bare shell");
        assert!(!is_handoff(true, &persisted, &[other]), "a different id");
    }

    #[test]
    fn keys_to_resume_unions_and_drops_quit_keys() {
        let persisted = vec![(key("b"), entry("1")), (key("c"), entry("2"))];
        let quit: HashSet<SessionKey> = [key("c")].into_iter().collect();
        assert_eq!(
            keys_to_resume(&[key("d"), key("b")], &persisted, &quit),
            vec![key("b"), key("d")]
        );
    }

    #[test]
    fn same_instance_adopts_without_closing_anything() {
        let s = Setup::new(&["a", "b"]);
        let before = s.keymap.get(&key("a")).unwrap();
        assert_eq!(s.run(&always), ReconcileOutcome::Adopted);
        assert!(!s.ops().contains(&Op::PaneClose), "zero pane_close calls");
        assert_eq!(s.keymap.get(&key("a")).unwrap(), before, "panes untouched");
        assert_eq!(s.fake.panes().len(), 2);
        assert_eq!(s.keymap.instance(), s.fake.server_instance());
    }

    #[test]
    fn same_instance_adopts_the_named_agent_over_a_shell_in_its_cwd() {
        let s = Setup::new(&[]);
        let cwd = "/repos/a";
        // A restore left a plain shell and the session's named agent in one cwd,
        // the shell listed first; the keymap still holds a pane that is gone.
        let shell = s.fake.add_pane(cwd, None);
        let [name, _] = crate::backend::herdr::agent::agent_names_for(&key("a"));
        let agent = s.fake.seed_named_agent(&name, "claude", cwd);
        s.keymap.record(
            key("a"),
            "w0".into(),
            "gone".into(),
            "t0".into(),
            cwd.into(),
        );
        assert_eq!(s.run(&always), ReconcileOutcome::Adopted);
        let adopted = s.keymap.get(&key("a")).unwrap();
        assert_eq!(adopted.pane_id, agent.pane_id, "the named agent is adopted");
        assert_ne!(adopted.pane_id, shell.pane_id);
        assert!(
            !s.ops().contains(&Op::PaneClose),
            "the stray shell stays open"
        );
        // A second reconcile keeps it.
        assert_eq!(s.run(&always), ReconcileOutcome::Adopted);
        assert_eq!(s.keymap.get(&key("a")).unwrap().pane_id, agent.pane_id);
    }

    #[test]
    fn unknown_baseline_adopts_and_records_the_instance() {
        let s = Setup::new(&["a"]);
        s.keymap.set_instance(None);
        s.restart_server();
        assert_eq!(s.run(&always), ReconcileOutcome::Adopted);
        assert!(!s.ops().contains(&Op::PaneClose));
        assert_eq!(s.keymap.instance(), s.fake.server_instance());
        assert_eq!(
            s.keymap.get(&key("a")).unwrap().pane_id,
            s.fake.panes()[0].pane_id,
            "re-adopted by cwd"
        );
    }

    #[test]
    fn new_instance_closes_restored_shells_and_returns_keys_to_resume() {
        let s = Setup::with_gap(&["b", "a", "quit"]);
        s.quit("quit");
        let old_ids: Vec<String> = s.fake.panes().into_iter().map(|p| p.pane_id).collect();
        let earlier_closes = s.fake.closed_pane_ids().len();
        s.restart_server();
        let restored: HashMap<String, String> = s
            .fake
            .panes()
            .into_iter()
            .map(|p| (p.cwd.clone().unwrap(), p.pane_id))
            .collect();
        // Restore renumbers: the old ids now name other worktrees' shells.
        assert_ne!(restored["/repos/b"], old_ids[0]);
        assert_eq!(restored["/repos/b"], "w1:p1");
        assert_eq!(
            restored["/repos/quit"], old_ids[1],
            "an old id now names another shell"
        );
        // The recovery set must be on disk before the first close.
        let path = s.fake.env().keymap_path();
        let seen = Arc::new(Mutex::new(None));
        let seen2 = Arc::clone(&seen);
        s.fake.on_call(
            Op::PaneClose,
            Arc::new(move || {
                let mut g = seen2.lock().unwrap();
                if g.is_none() {
                    *g = Some(KeyMap::with_path(path.clone()).should_be_running());
                }
            }),
        );
        let generation = s.keymap.generation();

        let out = s.run(&always);

        assert_eq!(
            out,
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("a"), key("b")]
            }
        );
        assert_eq!(
            seen.lock().unwrap().clone(),
            Some(vec![key("a"), key("b")]),
            "should_be_running persisted before the first pane_close"
        );
        let mut closed = s.fake.closed_pane_ids().split_off(earlier_closes);
        closed.sort();
        let mut want = vec![restored["/repos/a"].clone(), restored["/repos/b"].clone()];
        want.sort();
        assert_eq!(closed, want, "exactly the resumable keys' restored shells");
        assert!(s.keymap.get(&key("a")).is_none());
        assert!(s.keymap.get(&key("b")).is_none());
        let q = s
            .keymap
            .get_current(&key("quit"))
            .expect("quit key stays adopted");
        assert_eq!(q.pane_id, restored["/repos/quit"]);
        assert_eq!(
            s.live.lock().unwrap().iter().cloned().collect::<Vec<_>>(),
            vec![key("quit")]
        );
        let reloaded = KeyMap::with_path(s.fake.env().keymap_path());
        assert_eq!(reloaded.instance(), s.fake.server_instance());
        assert_eq!(reloaded.generation(), generation.next());
        assert_eq!(reloaded.should_be_running(), vec![key("a"), key("b")]);
        // The wait-until-gone poll ran after the closes.
        let ops = s.ops();
        let last_close = ops.iter().rposition(|o| *o == Op::PaneClose).unwrap();
        assert!(ops[last_close + 1..].contains(&Op::PaneList));
    }

    #[test]
    fn new_instance_waits_while_closed_panes_linger() {
        let s = Setup::new(&["a"]);
        s.restart_server();
        s.fake.close_linger_polls(3);
        let out = s.run(&always);
        assert!(matches!(out, ReconcileOutcome::NewGeneration { .. }));
        let ops = s.ops();
        let close = ops.iter().position(|o| *o == Op::PaneClose).unwrap();
        let polls = ops[close + 1..]
            .iter()
            .filter(|o| **o == Op::PaneList)
            .count();
        assert_eq!(polls, 4, "three lingering answers, then gone");
    }

    #[test]
    fn wait_until_gone_timeout_still_returns_new_generation() {
        let s = Setup::new(&["a"]);
        s.restart_server();
        s.fake.close_linger_polls(1000);
        let out = s.run(&always);
        assert_eq!(
            out,
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("a")]
            }
        );
        let ops = s.ops();
        let close = ops.iter().position(|o| *o == Op::PaneClose).unwrap();
        let polls = ops[close + 1..]
            .iter()
            .filter(|o| **o == Op::PaneList)
            .count();
        assert_eq!(polls, WAIT_GONE_ATTEMPTS as usize);
    }

    #[test]
    fn removed_worktree_is_closed_and_dropped() {
        let s = Setup::new(&["a", "gone"]);
        s.keymap.begin_recovery(&[key("gone")]);
        s.restart_server();
        let restored_gone = s
            .fake
            .panes()
            .into_iter()
            .find(|p| p.cwd.as_deref() == Some("/repos/gone"))
            .unwrap()
            .pane_id;
        let out = s.run(&|k: &SessionKey| k.worktree != "gone");
        assert_eq!(
            out,
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("a")]
            }
        );
        assert!(s.fake.closed_pane_ids().contains(&restored_gone));
        assert!(s.keymap.get(&key("gone")).is_none());
        assert!(!s.live.lock().unwrap().contains(&key("gone")));
        assert_eq!(s.keymap.should_be_running(), vec![key("a")]);
    }

    /// A worktree reached through a symlink: herdr reports the restored
    /// shell's resolved directory, and it is still found and closed.
    #[cfg(unix)]
    #[test]
    fn a_restored_shell_of_a_symlinked_worktree_is_closed_and_resumed() {
        let tmp = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(tmp.path()).unwrap().join("wt");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("ln");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let s = Setup::new(&[]);
        let p = s.fake.add_pane(
            &real.to_string_lossy(),
            Some(("claude", session("s-linked"))),
        );
        s.keymap.record(
            key("linked"),
            p.workspace_id,
            p.pane_id,
            p.terminal_id,
            link.to_string_lossy().into_owned(),
        );
        s.live.lock().unwrap().insert(key("linked"));
        s.keymap.set_instance(s.fake.server_instance());

        s.restart_server();
        let restored: Vec<String> = s.fake.panes().into_iter().map(|p| p.pane_id).collect();
        assert_eq!(restored.len(), 1);
        let out = s.run(&always);

        assert_eq!(
            out,
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("linked")]
            }
        );
        assert_eq!(s.fake.closed_pane_ids(), restored, "the shell is closed");
        let polls = s.ops().iter().filter(|o| **o == Op::PaneList).count();
        assert_eq!(polls, 2, "found at once, then seen gone: {:?}", s.ops());
    }

    #[test]
    fn new_instance_waits_through_a_slow_restore() {
        let s = Setup::new(&["a"]);
        s.fake.restore_delay_polls(5);
        s.restart_server();
        let out = s.run(&always);
        assert_eq!(
            out,
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("a")]
            }
        );
        assert_eq!(
            s.fake.closed_pane_ids().len(),
            1,
            "the restored shell was found"
        );
        let ops = s.ops();
        let close = ops.iter().position(|o| *o == Op::PaneClose).unwrap();
        let polls_before = ops[..close].iter().filter(|o| **o == Op::PaneList).count();
        assert_eq!(
            polls_before, 6,
            "five empty answers, then the restored pane"
        );
    }

    #[test]
    fn a_pane_list_that_keeps_failing_fails_the_reconcile_and_changes_nothing() {
        let s = Setup::new(&["a"]);
        let before = s.keymap.entries();
        let instance = s.keymap.instance();
        let generation = s.keymap.generation();
        s.restart_server();
        for _ in 0..RECONCILE_POLL_ATTEMPTS {
            s.fake.fail_next(
                Op::PaneList,
                crate::backend::herdr::ops::HerdrError::Timeout,
            );
        }
        assert!(s.try_run(&always).is_err());
        assert!(!s.ops().contains(&Op::PaneClose));
        assert_eq!(s.keymap.entries(), before, "entries kept");
        assert_eq!(s.keymap.instance(), instance);
        assert_eq!(s.keymap.generation(), generation);
        assert!(s.keymap.should_be_running().is_empty());
        // Retried later against a server that answers, it closes the shell.
        assert_eq!(
            s.run(&always),
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("a")]
            }
        );
        assert!(s.fake.panes().is_empty(), "the restored shell is closed");
    }

    #[test]
    fn a_transient_pane_list_failure_is_retried() {
        let s = Setup::new(&["a"]);
        s.restart_server();
        s.fake.fail_next(
            Op::PaneList,
            crate::backend::herdr::ops::HerdrError::Timeout,
        );
        assert_eq!(
            s.run(&always),
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("a")]
            }
        );
        assert!(s.fake.panes().is_empty(), "the restored shell is closed");
    }

    #[test]
    fn a_gradual_restore_is_waited_for_and_every_shell_closed() {
        let s = Setup::new(&["a", "b", "c"]);
        s.fake.restore_gradually(true);
        s.restart_server();
        assert_eq!(
            s.run(&always),
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("a"), key("b"), key("c")]
            }
        );
        assert_eq!(s.fake.closed_pane_ids().len(), 3);
        assert!(
            s.fake.panes().is_empty(),
            "no orphan shell: {:?}",
            s.fake.panes()
        );
    }

    #[test]
    fn every_restored_shell_of_a_resumed_worktree_is_closed() {
        let s = Setup::new(&["a"]);
        // A second pane in the same worktree that no session maps.
        s.fake.add_pane("/repos/a", None);
        s.restart_server();
        assert_eq!(s.fake.panes().len(), 2);
        s.run(&always);
        assert!(s.fake.panes().is_empty(), "{:?}", s.fake.panes());
    }

    #[test]
    fn a_restore_that_stops_short_settles_and_goes_on() {
        // One of the two worktrees never comes back: the reconcile waits for the
        // list to stop changing, not for the budget.
        let s = Setup::new(&["a", "b"]);
        s.restart_server();
        let b = s
            .fake
            .panes()
            .into_iter()
            .find(|p| p.cwd.as_deref() == Some("/repos/b"))
            .unwrap();
        s.fake.pane_close(&b.pane_id).unwrap();
        let polls = || s.ops().iter().filter(|o| **o == Op::PaneList).count();
        let before = polls();
        let out = s.run(&always);
        assert_eq!(
            out,
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("a"), key("b")]
            }
        );
        let ops = s.ops();
        let close = ops.iter().rposition(|o| *o == Op::PaneClose).unwrap();
        let polled = ops[..close].iter().filter(|o| **o == Op::PaneList).count();
        assert_eq!(polled - before, SETTLE_POLLS as usize);
        assert!(s.fake.panes().is_empty());
    }

    #[test]
    fn after_a_restart_an_unknown_baseline_replaces_the_restored_shells() {
        let s = Setup::new(&["a", "b"]);
        s.keymap.set_instance(None);
        s.restart_server();
        assert_eq!(
            s.after_death(&always),
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("a"), key("b")]
            }
        );
        assert!(
            s.fake.panes().is_empty(),
            "restored shells closed, not adopted"
        );
        assert!(s.keymap.entries().is_empty());
        assert_eq!(s.keymap.instance(), s.fake.server_instance());
        assert_eq!(s.keymap.should_be_running(), vec![key("a"), key("b")]);
    }

    #[test]
    fn after_a_restart_running_agents_on_an_unknown_server_are_adopted() {
        let s = Setup::new(&["a"]);
        s.keymap.set_instance(None);
        // The panes kept their ids and agents.
        s.fake.handoff();
        assert_eq!(s.after_death(&always), ReconcileOutcome::Adopted);
        assert!(!s.ops().contains(&Op::PaneClose));
        assert!(s.keymap.get_current(&key("a")).is_some());
        assert!(s.keymap.should_be_running().is_empty(), "nothing left owed");
    }

    #[test]
    fn the_same_server_after_a_restart_resumes_only_what_has_no_pane() {
        let s = Setup::new(&["a", "b", "quit", "gone"]);
        s.quit("quit");
        let owed: Vec<SessionKey> = ["a", "b", "quit", "gone"].iter().map(|w| key(w)).collect();
        s.keymap.begin_recovery(&owed);
        // b's pane went away.
        let b = s.keymap.get(&key("b")).unwrap();
        s.fake.pane_close(&b.pane_id).unwrap();
        let generation = s.keymap.note_death_bump();
        let out = s.restarted(generation, &|k: &SessionKey| k.worktree != "gone");
        assert_eq!(
            out,
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("b")]
            }
        );
        assert!(s.keymap.get_current(&key("a")).is_some(), "a adopted");
        assert_eq!(
            s.keymap.should_be_running(),
            vec![key("b")],
            "the rest resolved"
        );
    }

    #[test]
    fn the_same_server_owes_no_resume_to_a_terminated_session() {
        let s = Setup::new(&["a", "b", "term"]);
        s.terminate("term");
        let owed: Vec<SessionKey> = ["a", "b", "term"].iter().map(|w| key(w)).collect();
        s.keymap.begin_recovery(&owed);
        // b's and term's panes went away.
        for w in ["b", "term"] {
            let p = s.keymap.get(&key(w)).unwrap();
            s.fake.pane_close(&p.pane_id).unwrap();
        }
        let generation = s.keymap.note_death_bump();
        let out = s.restarted(generation, &always);
        assert_eq!(
            out,
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("b")]
            }
        );
        assert_eq!(s.keymap.should_be_running(), vec![key("b")]);
    }

    #[test]
    fn a_new_instance_owes_no_resume_to_a_terminated_session() {
        let s = Setup::with_gap(&["b", "a", "term"]);
        s.terminate("term");
        s.restart_server();
        let out = s.run(&always);
        assert_eq!(
            out,
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("a"), key("b")]
            }
        );
        assert_eq!(
            KeyMap::with_path(s.fake.env().keymap_path()).should_be_running(),
            vec![key("a"), key("b")]
        );
    }

    #[test]
    fn a_repeated_reconcile_leaves_sessions_of_the_new_generation_alone() {
        let s = Setup::new(&["a", "b"]);
        s.restart_server();
        let first = s.after_death(&always);
        assert_eq!(
            first,
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("a"), key("b")]
            }
        );
        // a is resumed on the new server; then the same server is reconciled
        // again (a retried run).
        let p = s
            .fake
            .add_pane("/repos/a", Some(("claude", session("s-a2"))));
        s.keymap.record(
            key("a"),
            p.workspace_id,
            p.pane_id.clone(),
            p.terminal_id,
            "/repos/a".into(),
        );
        s.keymap.resolve_recovered(&key("a"));
        let generation = s.keymap.generation();
        assert_eq!(
            s.restarted(generation, &always),
            ReconcileOutcome::NewGeneration {
                to_resume: vec![key("b")]
            }
        );
        assert!(
            !s.fake.closed_pane_ids().contains(&p.pane_id),
            "a's pane kept"
        );
        assert_eq!(s.keymap.get_current(&key("a")).unwrap().pane_id, p.pane_id);
    }

    #[test]
    fn handoff_keeps_the_lineage_on_unix() {
        let s = Setup::new(&["a"]);
        let before = s.keymap.get(&key("a")).unwrap();
        s.fake.handoff();
        let out = s.run(&always);
        if cfg!(unix) {
            assert_eq!(out, ReconcileOutcome::Adopted);
            assert!(!s.ops().contains(&Op::PaneClose));
            assert_eq!(s.keymap.get(&key("a")).unwrap().pane_id, before.pane_id);
        } else {
            // No handoff exists off unix: a new instance is a restart.
            assert_eq!(
                out,
                ReconcileOutcome::NewGeneration {
                    to_resume: vec![key("a")]
                }
            );
        }
        assert_eq!(s.keymap.instance(), s.fake.server_instance());
    }

    #[test]
    fn stale_generation_entries_are_absent_after_a_new_generation() {
        let s = Setup::new(&["a", "quit"]);
        s.quit("quit");
        let stale = s.keymap.get(&key("quit")).unwrap();
        s.restart_server();
        let _ = s.run(&always);
        assert!(s.keymap.get_current(&key("a")).is_none());
        // An entry stamped with the older generation is rejected.
        s.keymap
            .adopt_reconciled(&s.keymap.entries(), vec![(key("quit"), stale.clone())]);
        assert!(stale.server_generation < s.keymap.generation());
        assert!(s.keymap.get(&key("quit")).is_some());
        assert!(s.keymap.get_current(&key("quit")).is_none());
    }
}
