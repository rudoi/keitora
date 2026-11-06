//! Process-wide bookkeeping for in-flight Claude ⇄ Codex transfers.
//!
//! One map keyed by `(project, worktree)` holds, per worktree:
//!
//!   * the **transferring marker** that dispatch refuses on while a transfer (and
//!     the pane restart that follows it) is in progress,
//!   * the **cancel flag** the blocking import polls,
//!   * the **diagnostics trail**: a bounded, sanitized list of structured entries
//!     that `transfer_diagnostics` renders (home-scrubbed) for the user to copy.
//!
//! Every method locks the one `std::sync::Mutex`, mutates, unlocks and returns: no
//! lock is held across IO or an `.await`, and a poisoned lock is recovered rather
//! than propagated (a panic elsewhere must never wedge dispatch). Marker expiry is
//! evaluated lazily on access; there is no timer task. Expiries are only ever
//! computed as `Instant + Duration` (subtracting from an `Instant` can panic on a
//! machine that booted recently).
//!
//! Every time-dependent method has a `*_at(.., now)` twin so tests can drive the
//! clock from a fixed base instant.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

/// Marker lifetime from `begin` while importing (above the import's overall deadline).
pub const IMPORT_MARKER_TTL: Duration = Duration::from_secs(300);
/// Marker lifetime from `mark_committed` (above the frontend's restart backstop).
pub const COMMITTED_MARKER_TTL: Duration = Duration::from_secs(180);
/// Trail entries kept per worktree; the oldest are dropped first and counted.
pub const MAX_TRAIL_ENTRIES: usize = 300;
/// Cap, in chars, on any single string value inside an entry's detail.
pub const MAX_TRAIL_STRING_CHARS: usize = 2000;
/// Nesting depth past which a detail subtree is replaced wholesale.
const MAX_DETAIL_DEPTH: usize = 8;

const REDACTED: &str = "[redacted]";
const DEPTH_LIMIT: &str = "[depth limit]";

/// Normalized (lowercase, no `_`/`-`) object keys whose values never reach the
/// log or the trail.
const SENSITIVE_KEYS: &[&str] = &[
    "prompt",
    "content",
    "text",
    "summary",
    "transcript",
    "firstuserline",
    "token",
    "accesstoken",
    "apikey",
    "secret",
    "password",
    "authorization",
];

type Key = (String, String); // (project, worktree)

fn key(project: &str, worktree: &str) -> Key {
    (project.to_string(), worktree.to_string())
}

// ---------------------------------------------------------------------------
// Trail types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrailStage {
    Env,
    Import,
    Classify,
    Persist,
    Launch,
    Verify,
    Settle,
}

impl TrailStage {
    /// The same strings as the serde form; used in log lines and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            TrailStage::Env => "env",
            TrailStage::Import => "import",
            TrailStage::Classify => "classify",
            TrailStage::Persist => "persist",
            TrailStage::Launch => "launch",
            TrailStage::Verify => "verify",
            TrailStage::Settle => "settle",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrailEntry {
    /// Milliseconds since the transfer's `begin` (monotonic); set by the registry.
    pub at_ms: u64,
    pub stage: TrailStage,
    /// Structured detail; sanitized before it is stored.
    pub detail: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrailSnapshot {
    pub entries: Vec<TrailEntry>,
    pub dropped: u32,
    pub committed: bool,
    pub transferring: bool,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

struct RegistryEntry {
    /// Generation: a guard only ever releases the transfer it began.
    id: u64,
    cancel: Arc<AtomicBool>,
    /// Origin of trail `at_ms` and of `TransferGuard::elapsed_ms`.
    started: Instant,
    /// Persist is about to run (or ran); cancel is a no-op afterwards.
    committed: bool,
    /// The marker dispatch refuses on.
    transferring: bool,
    /// Lazy backstop for a marker nobody settles.
    marker_expires_at: Instant,
    trail: VecDeque<TrailEntry>,
    /// Entries evicted from the front of `trail`.
    dropped: u32,
}

impl RegistryEntry {
    fn live_at(&self, now: Instant) -> bool {
        self.transferring && now < self.marker_expires_at
    }

    /// Append an already-sanitized detail, evicting the oldest past the cap.
    fn push(&mut self, stage: TrailStage, detail: Value, now: Instant) {
        let at_ms = now
            .checked_duration_since(self.started)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.trail.push_back(TrailEntry {
            at_ms,
            stage,
            detail,
        });
        if self.trail.len() > MAX_TRAIL_ENTRIES {
            self.trail.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    /// A registry-emitted entry: its own log line plus the in-memory push.
    fn push_logged(
        &mut self,
        level: log::Level,
        key: &Key,
        stage: TrailStage,
        detail: Value,
        now: Instant,
    ) {
        log::log!(
            target: "transfer",
            level,
            "{}",
            format_trail_line(&key.0, &key.1, stage, &detail)
        );
        self.push(stage, detail, now);
    }
}

struct Inner {
    map: Mutex<HashMap<Key, RegistryEntry>>,
    next_id: AtomicU64,
}

/// Cheap, cloneable handle; every clone of [`TransferRegistry::global`] shares one map.
#[derive(Clone)]
pub struct TransferRegistry(Arc<Inner>);

/// `begin` refused: a live transfer is already registered for the worktree.
#[derive(Debug, PartialEq, Eq)]
pub struct AlreadyTransferring;

static GLOBAL: OnceLock<TransferRegistry> = OnceLock::new();

impl Default for TransferRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TransferRegistry {
    /// An isolated, empty registry.
    pub fn new() -> Self {
        TransferRegistry(Arc::new(Inner {
            map: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }))
    }

    /// The process-wide registry (a clone of the same handle on every call).
    pub fn global() -> Self {
        GLOBAL.get_or_init(Self::new).clone()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Key, RegistryEntry>> {
        self.0.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn begin(
        &self,
        project: &str,
        worktree: &str,
    ) -> Result<TransferGuard<'_>, AlreadyTransferring> {
        self.begin_at(project, worktree, Instant::now())
    }

    /// Register a new transfer, replacing any previous entry (and its trail) for
    /// the worktree, unless a live one is still registered.
    pub(crate) fn begin_at(
        &self,
        project: &str,
        worktree: &str,
        now: Instant,
    ) -> Result<TransferGuard<'_>, AlreadyTransferring> {
        let k = key(project, worktree);
        let mut map = self.lock();
        if let Some(existing) = map.get(&k) {
            if existing.live_at(now) {
                return Err(AlreadyTransferring);
            }
            if existing.transferring {
                log::warn!(target: "transfer", "transfer: {project}/{worktree} stale marker replaced");
            }
        }
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        map.insert(
            k.clone(),
            RegistryEntry {
                id,
                cancel: Arc::clone(&cancel),
                started: now,
                committed: false,
                transferring: true,
                marker_expires_at: now + IMPORT_MARKER_TTL,
                trail: VecDeque::new(),
                dropped: 0,
            },
        );
        Ok(TransferGuard {
            registry: self,
            key: k,
            id,
            started: now,
            cancel,
        })
    }

    /// Ask the in-flight import to stop. Honoured (returns `true`) only while the
    /// transfer is live and not yet committed.
    pub fn request_cancel(&self, project: &str, worktree: &str) -> bool {
        let now = Instant::now();
        let k = key(project, worktree);
        let mut map = self.lock();
        let Some(entry) = map.get_mut(&k) else {
            return false;
        };
        if !entry.live_at(now) || entry.committed {
            return false;
        }
        entry.cancel.store(true, Ordering::SeqCst);
        entry.push_logged(
            log::Level::Info,
            &k,
            TrailStage::Import,
            json!({ "cancel_requested": true }),
            now,
        );
        true
    }

    /// The commit point, atomic with the cancel check: `true` = committed (cancel
    /// is a no-op from now on and the marker lives until settled); `false` = a
    /// cancel was requested or nothing live is registered, and nothing changed.
    pub fn mark_committed(&self, project: &str, worktree: &str) -> bool {
        self.mark_committed_at(project, worktree, Instant::now())
    }

    pub(crate) fn mark_committed_at(&self, project: &str, worktree: &str, now: Instant) -> bool {
        let mut map = self.lock();
        let Some(entry) = map.get_mut(&key(project, worktree)) else {
            return false;
        };
        if !entry.live_at(now) || entry.cancel.load(Ordering::SeqCst) {
            return false;
        }
        entry.committed = true;
        entry.marker_expires_at = now + COMMITTED_MARKER_TTL;
        true
    }

    /// Whether the worktree is mid-transfer. An expired marker is cleared here
    /// (recorded as a `backstop` settle).
    pub fn is_transferring(&self, project: &str, worktree: &str) -> bool {
        self.is_transferring_at(project, worktree, Instant::now())
    }

    pub(crate) fn is_transferring_at(&self, project: &str, worktree: &str, now: Instant) -> bool {
        let k = key(project, worktree);
        let mut map = self.lock();
        let Some(entry) = map.get_mut(&k) else {
            return false;
        };
        if !entry.transferring {
            return false;
        }
        if now >= entry.marker_expires_at {
            entry.transferring = false;
            entry.push_logged(
                log::Level::Warn,
                &k,
                TrailStage::Settle,
                json!({ "reason": "backstop" }),
                now,
            );
            return false;
        }
        true
    }

    /// Clear a committed marker; idempotent. The trail is kept until the next
    /// `begin`. An uncommitted entry is left alone: until `mark_committed` the
    /// import phase is owned by its [`TransferGuard`], so a settle (for example one
    /// sent after a refused `begin`) cannot release a transfer still importing.
    pub fn settle(&self, project: &str, worktree: &str) {
        let now = Instant::now();
        let k = key(project, worktree);
        let mut map = self.lock();
        let Some(entry) = map.get_mut(&k) else {
            return;
        };
        if !entry.transferring || !entry.committed {
            return;
        }
        entry.transferring = false;
        let committed = entry.committed;
        entry.push_logged(
            log::Level::Info,
            &k,
            TrailStage::Settle,
            json!({ "reason": "settled", "committed": committed }),
            now,
        );
    }

    /// Sanitize `detail` and append it to the worktree's trail (in-memory only;
    /// emitters use [`trail`]/[`trail_warn`] which also log).
    #[cfg(test)]
    pub fn push_trail(&self, project: &str, worktree: &str, stage: TrailStage, detail: Value) {
        self.push_trail_at(project, worktree, stage, detail, Instant::now())
    }

    #[cfg(test)]
    pub(crate) fn push_trail_at(
        &self,
        project: &str,
        worktree: &str,
        stage: TrailStage,
        detail: Value,
        now: Instant,
    ) {
        self.push_trail_sanitized_at(project, worktree, stage, sanitize_detail(detail), now)
    }

    /// Push an already-sanitized detail. No entry ⇒ dropped (never creates one);
    /// late entries after `settle` are still accepted.
    fn push_trail_sanitized_at(
        &self,
        project: &str,
        worktree: &str,
        stage: TrailStage,
        detail: Value,
        now: Instant,
    ) {
        let mut map = self.lock();
        if let Some(entry) = map.get_mut(&key(project, worktree)) {
            entry.push(stage, detail, now);
        }
    }

    /// The worktree's trail, or `None` when there is no entry or it is empty.
    pub fn last_trail(&self, project: &str, worktree: &str) -> Option<TrailSnapshot> {
        let map = self.lock();
        let entry = map.get(&key(project, worktree))?;
        if entry.trail.is_empty() {
            return None;
        }
        Some(TrailSnapshot {
            entries: entry.trail.iter().cloned().collect(),
            dropped: entry.dropped,
            committed: entry.committed,
            transferring: entry.transferring,
        })
    }

    /// Sanitize `detail` once, write the log line and push to this registry.
    /// Production code calls the free [`trail`]/[`trail_warn`] helpers.
    pub fn trail_on(
        &self,
        level: log::Level,
        project: &str,
        worktree: &str,
        stage: TrailStage,
        detail: Value,
    ) {
        let detail = sanitize_detail(detail);
        log::log!(
            target: "transfer",
            level,
            "{}",
            format_trail_line(project, worktree, stage, &detail)
        );
        self.push_trail_sanitized_at(project, worktree, stage, detail, Instant::now());
    }
}

/// Held by the transfer command for its whole duration. Releases the marker on
/// drop unless the transfer was committed (then the marker waits for `settle`
/// or the backstop). Holds no lock, so it is `Send` and may live across `.await`s.
pub struct TransferGuard<'a> {
    registry: &'a TransferRegistry,
    key: Key,
    id: u64,
    started: Instant,
    cancel: Arc<AtomicBool>,
}

impl TransferGuard<'_> {
    /// The cancel flag captured at `begin`, polled by the blocking import.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// Milliseconds since `begin`.
    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

impl Drop for TransferGuard<'_> {
    fn drop(&mut self) {
        let mut map = self.registry.lock();
        let Some(entry) = map.get_mut(&self.key) else {
            return;
        };
        if entry.id != self.id || entry.committed || !entry.transferring {
            return;
        }
        entry.transferring = false;
        entry.push_logged(
            log::Level::Info,
            &self.key,
            TrailStage::Settle,
            json!({ "reason": "guard_drop", "committed": false }),
            Instant::now(),
        );
    }
}

// ---------------------------------------------------------------------------
// Trail helpers (the only way emitters write the trail)
// ---------------------------------------------------------------------------

/// Log (info) and record one trail entry on the process-wide registry.
pub fn trail(project: &str, worktree: &str, stage: TrailStage, detail: Value) {
    TransferRegistry::global().trail_on(log::Level::Info, project, worktree, stage, detail)
}

/// Log (warn) and record one trail entry on the process-wide registry.
pub fn trail_warn(project: &str, worktree: &str, stage: TrailStage, detail: Value) {
    TransferRegistry::global().trail_on(log::Level::Warn, project, worktree, stage, detail)
}

/// `transfer: <project>/<worktree> [<stage>] <compact json>` — a trail log message.
pub(crate) fn format_trail_line(
    project: &str,
    worktree: &str,
    stage: TrailStage,
    detail: &Value,
) -> String {
    format!(
        "transfer: {project}/{worktree} [{}] {detail}",
        stage.as_str()
    )
}

// ---------------------------------------------------------------------------
// Sanitizing
// ---------------------------------------------------------------------------

fn is_sensitive_key(k: &str) -> bool {
    let normalized: String = k
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    SENSITIVE_KEYS.contains(&normalized.as_str())
}

/// Replace the values of sensitive keys (anywhere in the tree) with `"[redacted]"`;
/// the keys stay so a reader sees the field existed.
pub(crate) fn redact_keys(v: Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, val)| {
                    let val = if is_sensitive_key(&k) {
                        Value::String(REDACTED.to_string())
                    } else {
                        redact_keys(val)
                    };
                    (k, val)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_keys).collect()),
        other => other,
    }
}

fn cap_string(s: String) -> String {
    let total = s.chars().count();
    if total <= MAX_TRAIL_STRING_CHARS {
        return s;
    }
    let mut out: String = s.chars().take(MAX_TRAIL_STRING_CHARS).collect();
    out.push_str(&format!(
        "…[truncated {} chars]",
        total - MAX_TRAIL_STRING_CHARS
    ));
    out
}

fn cap_and_limit(v: Value, depth: usize) -> Value {
    if depth > MAX_DETAIL_DEPTH {
        return Value::String(DEPTH_LIMIT.to_string());
    }
    match v {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, val)| (k, cap_and_limit(val, depth + 1)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|val| cap_and_limit(val, depth + 1))
                .collect(),
        ),
        Value::String(s) => Value::String(cap_string(s)),
        other => other,
    }
}

/// Write-time sanitizing shared by the log line and the trail: redact sensitive
/// keys, cap long strings (by chars) and replace over-deep subtrees.
pub(crate) fn sanitize_detail(v: Value) -> Value {
    cap_and_limit(redact_keys(v), 0)
}

// ---------------------------------------------------------------------------
// Diagnostics rendering
// ---------------------------------------------------------------------------

/// The spellings of `home` to replace with `~`, ASCII-lowercased, longest first.
/// Empty when there is no usable home (missing, empty, or `/`-like).
fn home_candidates(home: Option<&str>) -> Vec<String> {
    let Some(home) = home else {
        return Vec::new();
    };
    let h = home.trim_end_matches(['/', '\\']);
    if h.chars().count() < 2 {
        return Vec::new();
    }
    let fwd = h.replace('\\', "/");
    let back = h.replace('/', "\\");
    let mut cands: Vec<String> = vec![
        h.to_string(),
        format!("\\\\?\\{back}"),
        format!("//?/{fwd}"),
        fwd,
        back,
    ]
    .into_iter()
    .map(|c| c.to_ascii_lowercase())
    .collect();
    cands.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    cands.dedup();
    cands
}

fn ends_home_component(next: Option<char>) -> bool {
    match next {
        None => true,
        Some('/') | Some('\\') => true,
        Some(c) => !c.is_alphanumeric() && !matches!(c, '_' | '-' | '.'),
    }
}

/// Replace every occurrence of a home candidate (ASCII case-insensitive) that ends
/// at a path-component boundary with `~`.
fn scrub_with(s: &str, cands: &[String]) -> String {
    if cands.is_empty() {
        return s.to_string();
    }
    // ASCII lowercasing preserves byte offsets, so indices map 1:1 onto `s`.
    let lower = s.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'outer: while i < s.len() {
        for cand in cands {
            let end = i + cand.len();
            if bytes[i..].starts_with(cand.as_bytes())
                && ends_home_component(s[end..].chars().next())
            {
                out.push('~');
                i = end;
                continue 'outer;
            }
        }
        let c = s[i..]
            .chars()
            .next()
            .expect("i is a char boundary below len");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Scrub `home` out of one string (see [`render_diagnostics`] for the rules).
#[cfg(test)]
pub(crate) fn scrub_home(s: &str, home: Option<&str>) -> String {
    scrub_with(s, &home_candidates(home))
}

fn scrub_value(v: Value, cands: &[String]) -> Value {
    match v {
        Value::String(s) => Value::String(scrub_with(&s, cands)),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, val)| (k, scrub_value(val, cands)))
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|v| scrub_value(v, cands)).collect())
        }
        other => other,
    }
}

/// The user-copyable diagnostics text for one worktree's trail. `home` is
/// replaced with `~` in every detail string value (before JSON escaping) and in
/// the header; sensitive keys are redacted again in case an entry skipped
/// write-time sanitizing.
pub(crate) fn render_diagnostics(
    project: &str,
    worktree: &str,
    snap: &TrailSnapshot,
    home: Option<&str>,
) -> String {
    let cands = home_candidates(home);
    let mut out = String::new();
    out.push_str("keitora transfer diagnostics\n");
    out.push_str(&format!(
        "app: keitora {} ({}/{})\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    out.push_str(&scrub_with(
        &format!("transfer: {project}/{worktree}"),
        &cands,
    ));
    out.push('\n');
    out.push_str(&format!(
        "state: transferring={} committed={}\n",
        snap.transferring, snap.committed
    ));
    out.push_str(&format!("entries: {}", snap.entries.len()));
    if snap.dropped > 0 {
        out.push_str(&format!(" ({} earlier entries dropped)", snap.dropped));
    }
    out.push('\n');
    for e in &snap.entries {
        let detail = scrub_value(redact_keys(e.detail.clone()), &cands);
        let json = serde_json::to_string(&detail).unwrap_or_else(|_| "null".to_string());
        out.push_str(&format!("+{}ms [{}] {}\n", e.at_ms, e.stage.as_str(), json));
    }
    out
}

/// The diagnostics text for `project`/`worktree`, `None` when there is no trail.
pub(crate) fn diagnostics_text(
    registry: &TransferRegistry,
    project: &str,
    worktree: &str,
    home: Option<&str>,
) -> Option<String> {
    registry
        .last_trail(project, worktree)
        .map(|snap| render_diagnostics(project, worktree, &snap, home))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn stages(r: &TransferRegistry, p: &str, w: &str) -> Vec<TrailStage> {
        r.last_trail(p, w)
            .map(|s| s.entries.iter().map(|e| e.stage).collect())
            .unwrap_or_default()
    }

    #[test]
    fn begin_twice_is_refused() {
        let r = TransferRegistry::new();
        let g = r.begin("proj-a", "alpha").expect("first begin");
        let flag = g.cancel_flag();
        assert_eq!(r.begin("proj-a", "alpha").err(), Some(AlreadyTransferring));
        // The first transfer is untouched: still live, same cancel flag, cancellable.
        assert!(r.is_transferring("proj-a", "alpha"));
        assert!(r.request_cancel("proj-a", "alpha"));
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn begin_after_guard_drop_succeeds() {
        let r = TransferRegistry::new();
        drop(r.begin("proj-a", "alpha").unwrap());
        assert!(r.begin("proj-a", "alpha").is_ok());
    }

    #[test]
    fn begin_after_settle_succeeds() {
        let r = TransferRegistry::new();
        let g = r.begin("proj-a", "alpha").unwrap();
        assert!(r.mark_committed("proj-a", "alpha"));
        drop(g);
        assert!(r.is_transferring("proj-a", "alpha"));
        r.settle("proj-a", "alpha");
        assert!(r.begin("proj-a", "alpha").is_ok());
    }

    #[test]
    fn cancel_only_before_commit() {
        let r = TransferRegistry::new();
        let g = r.begin("proj-a", "alpha").unwrap();
        assert!(r.request_cancel("proj-a", "alpha"));
        assert!(g.cancel_flag().load(Ordering::SeqCst));
        drop(g);

        let g2 = r.begin("proj-a", "alpha").unwrap();
        assert!(r.mark_committed("proj-a", "alpha"));
        assert!(!r.request_cancel("proj-a", "alpha"));
        assert!(!g2.cancel_flag().load(Ordering::SeqCst));
    }

    #[test]
    fn cancel_without_transfer_is_false() {
        let r = TransferRegistry::new();
        assert!(!r.request_cancel("proj-a", "alpha"));
    }

    #[test]
    fn guard_drop_releases_marker_unless_committed() {
        let r = TransferRegistry::new();
        drop(r.begin("proj-a", "alpha").unwrap());
        assert!(!r.is_transferring("proj-a", "alpha"));

        let g = r.begin("proj-a", "alpha").unwrap();
        assert!(r.mark_committed("proj-a", "alpha"));
        drop(g);
        assert!(r.is_transferring("proj-a", "alpha"));
    }

    #[test]
    fn settle_is_idempotent() {
        let r = TransferRegistry::new();
        let g = r.begin("proj-a", "alpha").unwrap();
        assert!(r.mark_committed("proj-a", "alpha"));
        drop(g);
        r.settle("proj-a", "alpha");
        r.settle("proj-a", "alpha");
        assert_eq!(stages(&r, "proj-a", "alpha"), vec![TrailStage::Settle]);
        assert!(!r.is_transferring("proj-a", "alpha"));
    }

    #[test]
    fn settle_does_not_clear_an_uncommitted_transfer() {
        let r = TransferRegistry::new();
        let g = r.begin("proj-a", "alpha").unwrap();
        // A second transfer was refused and its caller settles anyway.
        assert_eq!(r.begin("proj-a", "alpha").err(), Some(AlreadyTransferring));
        r.settle("proj-a", "alpha");
        assert!(
            r.is_transferring("proj-a", "alpha"),
            "the import still owns it"
        );
        assert!(r.request_cancel("proj-a", "alpha"), "still cancellable");
        assert!(!stages(&r, "proj-a", "alpha").contains(&TrailStage::Settle));
        // The guard still releases it; the backstop still applies.
        drop(g);
        assert!(!r.is_transferring("proj-a", "alpha"));

        let base = Instant::now();
        let _g = r.begin_at("proj-a", "alpha", base).unwrap();
        r.settle("proj-a", "alpha");
        assert!(r.is_transferring_at("proj-a", "alpha", base + secs(1)));
        assert!(!r.is_transferring_at("proj-a", "alpha", base + secs(301)));
    }

    #[test]
    fn marker_expires_after_import_ttl() {
        let r = TransferRegistry::new();
        let base = Instant::now();
        let _g = r.begin_at("proj-a", "alpha", base).unwrap();
        assert!(r.is_transferring_at("proj-a", "alpha", base + secs(299)));
        assert!(!r.is_transferring_at("proj-a", "alpha", base + secs(301)));
        let snap = r.last_trail("proj-a", "alpha").unwrap();
        let last = snap.entries.last().unwrap();
        assert_eq!(last.stage, TrailStage::Settle);
        assert_eq!(last.detail, json!({ "reason": "backstop" }));
        assert!(r.begin_at("proj-a", "alpha", base + secs(302)).is_ok());
    }

    #[test]
    fn marker_expires_after_commit_ttl() {
        let r = TransferRegistry::new();
        let base = Instant::now();
        let _g = r.begin_at("proj-a", "alpha", base).unwrap();
        assert!(r.mark_committed_at("proj-a", "alpha", base + secs(100)));
        assert!(r.is_transferring_at("proj-a", "alpha", base + secs(279)));
        assert!(!r.is_transferring_at("proj-a", "alpha", base + secs(281)));
    }

    #[test]
    fn mark_committed_returns_false_after_cancel_and_does_not_commit() {
        let r = TransferRegistry::new();
        let g = r.begin("proj-a", "alpha").unwrap();
        assert!(r.request_cancel("proj-a", "alpha"));
        assert!(!r.mark_committed("proj-a", "alpha"));
        assert!(!r.last_trail("proj-a", "alpha").unwrap().committed);
        drop(g);
        assert!(!r.is_transferring("proj-a", "alpha"));
    }

    #[test]
    fn mark_committed_true_then_cancel_is_a_noop() {
        let r = TransferRegistry::new();
        let g = r.begin("proj-a", "alpha").unwrap();
        assert!(r.mark_committed("proj-a", "alpha"));
        assert!(!r.request_cancel("proj-a", "alpha"));
        assert!(!g.cancel_flag().load(Ordering::SeqCst));
    }

    #[test]
    fn mark_committed_without_an_entry_is_false() {
        let r = TransferRegistry::new();
        assert!(!r.mark_committed("proj-a", "alpha"));
    }

    #[test]
    fn transfer_guard_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<TransferGuard<'static>>();
    }

    #[test]
    fn begin_replaces_trail_and_settle_keeps_it() {
        let r = TransferRegistry::new();
        let g = r.begin("proj-a", "alpha").unwrap();
        r.push_trail(
            "proj-a",
            "alpha",
            TrailStage::Env,
            json!({ "exe": "codex" }),
        );
        r.push_trail("proj-a", "alpha", TrailStage::Import, json!({ "id": "i1" }));
        assert!(r.mark_committed("proj-a", "alpha"));
        drop(g);
        r.settle("proj-a", "alpha");
        assert_eq!(
            stages(&r, "proj-a", "alpha"),
            vec![TrailStage::Env, TrailStage::Import, TrailStage::Settle]
        );
        let _g = r.begin("proj-a", "alpha").unwrap();
        assert!(r.last_trail("proj-a", "alpha").is_none());
    }

    #[test]
    fn keys_are_independent() {
        let r = TransferRegistry::new();
        let ga = r.begin("proj-a", "alpha").unwrap();
        let gb = r.begin("proj-b", "alpha").unwrap();
        r.push_trail(
            "proj-a",
            "alpha",
            TrailStage::Env,
            json!({ "exe": "codex" }),
        );
        assert!(r.request_cancel("proj-a", "alpha"));
        assert!(ga.cancel_flag().load(Ordering::SeqCst));
        assert!(!gb.cancel_flag().load(Ordering::SeqCst));
        assert!(r.last_trail("proj-b", "alpha").is_none());
        drop(ga);
        assert!(!r.is_transferring("proj-a", "alpha"));
        assert!(r.is_transferring("proj-b", "alpha"));
    }

    #[test]
    fn stale_guard_does_not_release_a_newer_transfer() {
        let r = TransferRegistry::new();
        let base = Instant::now();
        let a = r.begin_at("proj-a", "alpha", base).unwrap();
        let b = r.begin_at("proj-a", "alpha", base + secs(301)).unwrap();
        drop(a);
        assert!(r.is_transferring_at("proj-a", "alpha", base + secs(302)));
        drop(b);
    }

    #[test]
    fn poisoned_mutex_does_not_wedge() {
        let r = TransferRegistry::new();
        let r2 = r.clone();
        let joined = std::thread::spawn(move || {
            let _held = r2.0.map.lock().unwrap();
            panic!("poison the registry lock");
        })
        .join();
        assert!(joined.is_err());
        assert!(r.0.map.is_poisoned());
        assert!(!r.is_transferring("proj-a", "alpha"));
        let _g = r
            .begin("proj-a", "alpha")
            .expect("begin on a poisoned lock");
        assert!(r.is_transferring("proj-a", "alpha"));
    }

    #[test]
    fn trail_is_capped_and_counts_drops() {
        let r = TransferRegistry::new();
        let _g = r.begin("proj-a", "alpha").unwrap();
        for i in 0..(MAX_TRAIL_ENTRIES + 5) {
            r.push_trail("proj-a", "alpha", TrailStage::Verify, json!({ "n": i }));
        }
        let snap = r.last_trail("proj-a", "alpha").unwrap();
        assert_eq!(snap.entries.len(), MAX_TRAIL_ENTRIES);
        assert_eq!(snap.dropped, 5);
        assert_eq!(snap.entries[0].detail, json!({ "n": 5 }));
    }

    #[test]
    fn push_trail_without_entry_is_dropped() {
        let r = TransferRegistry::new();
        r.push_trail(
            "proj-a",
            "alpha",
            TrailStage::Env,
            json!({ "exe": "codex" }),
        );
        assert!(r.last_trail("proj-a", "alpha").is_none());
        assert!(r.lock().is_empty());
    }

    #[test]
    fn trail_at_ms_is_monotonic_from_begin() {
        let r = TransferRegistry::new();
        let base = Instant::now();
        let _g = r.begin_at("proj-a", "alpha", base).unwrap();
        r.push_trail_at("proj-a", "alpha", TrailStage::Env, json!({}), base + ms(10));
        r.push_trail_at(
            "proj-a",
            "alpha",
            TrailStage::Import,
            json!({}),
            base + ms(25),
        );
        let at: Vec<u64> = r
            .last_trail("proj-a", "alpha")
            .unwrap()
            .entries
            .iter()
            .map(|e| e.at_ms)
            .collect();
        assert_eq!(at, vec![10, 25]);
    }

    #[test]
    fn sanitize_detail_redacts_prompt_and_secret_keys() {
        let out = sanitize_detail(json!({
            "outer": {
                "prompt": "do the thing",
                "Content": "body",
                "first_user_line": "hello",
                "apiKey": "k",
                "message": "kept"
            }
        }));
        assert_eq!(
            out,
            json!({
                "outer": {
                    "prompt": "[redacted]",
                    "Content": "[redacted]",
                    "first_user_line": "[redacted]",
                    "apiKey": "[redacted]",
                    "message": "kept"
                }
            })
        );
    }

    #[test]
    fn sanitize_detail_caps_long_strings_by_chars() {
        let long: String = "é".repeat(5000);
        let out = sanitize_detail(json!({ "stderr_tail": long }));
        let s = out["stderr_tail"].as_str().unwrap();
        let expected = format!("{}…[truncated 3000 chars]", "é".repeat(2000));
        assert_eq!(s, expected);

        let exact: String = "é".repeat(2000);
        let out = sanitize_detail(json!({ "stderr_tail": exact.clone() }));
        assert_eq!(out["stderr_tail"].as_str().unwrap(), exact);
    }

    #[test]
    fn sanitize_detail_depth_limit() {
        let mut v = json!("leaf");
        for _ in 0..12 {
            v = json!({ "a": v });
        }
        let out = sanitize_detail(v);
        // Walk down: objects until the limit, then the marker string.
        let mut cur = &out;
        let mut depth = 0;
        while let Some(next) = cur.get("a") {
            cur = next;
            depth += 1;
        }
        assert_eq!(cur, &json!(DEPTH_LIMIT));
        assert_eq!(depth, MAX_DETAIL_DEPTH + 1);
    }

    #[test]
    fn trail_on_pushes_sanitized_entry_and_line_format() {
        let r = TransferRegistry::new();
        let _g = r.begin("proj-a", "alpha").unwrap();
        r.trail_on(
            log::Level::Info,
            "proj-a",
            "alpha",
            TrailStage::Env,
            json!({ "exe": "codex", "prompt": "x" }),
        );
        let snap = r.last_trail("proj-a", "alpha").unwrap();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].stage, TrailStage::Env);
        assert_eq!(
            snap.entries[0].detail,
            json!({ "exe": "codex", "prompt": "[redacted]" })
        );
        assert_eq!(
            format_trail_line(
                "proj-a",
                "alpha",
                TrailStage::Env,
                &json!({ "exe": "codex" })
            ),
            r#"transfer: proj-a/alpha [env] {"exe":"codex"}"#
        );
    }

    fn snap(entries: Vec<TrailEntry>, dropped: u32) -> TrailSnapshot {
        TrailSnapshot {
            entries,
            dropped,
            committed: true,
            transferring: false,
        }
    }

    fn entry(at_ms: u64, stage: TrailStage, detail: Value) -> TrailEntry {
        TrailEntry {
            at_ms,
            stage,
            detail,
        }
    }

    #[test]
    fn render_diagnostics_header_and_order() {
        let s = snap(
            vec![
                entry(0, TrailStage::Env, json!({ "exe": "codex" })),
                entry(120, TrailStage::Classify, json!({ "result": "imported" })),
            ],
            0,
        );
        let out = render_diagnostics("proj-a", "alpha", &s, None);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "keitora transfer diagnostics");
        assert_eq!(
            lines[1],
            format!(
                "app: keitora {} ({}/{})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        );
        assert_eq!(lines[2], "transfer: proj-a/alpha");
        assert_eq!(lines[3], "state: transferring=false committed=true");
        assert_eq!(lines[4], "entries: 2");
        assert_eq!(lines[5], r#"+0ms [env] {"exe":"codex"}"#);
        assert_eq!(lines[6], r#"+120ms [classify] {"result":"imported"}"#);
        assert_eq!(lines.len(), 7);
        assert!(out.ends_with('\n'));

        let dropped = render_diagnostics("proj-a", "alpha", &snap(s.entries.clone(), 3), None);
        assert!(dropped
            .lines()
            .any(|l| l == "entries: 2 (3 earlier entries dropped)"));
    }

    #[test]
    fn scrub_home_unix_path() {
        let home = Some("/Users/me");
        assert_eq!(
            scrub_home("/Users/me/.claude/projects/x.jsonl", home),
            "~/.claude/projects/x.jsonl"
        );
        assert_eq!(scrub_home("/Users/me", home), "~");
        assert_eq!(scrub_home("/Users/medium/x", home), "/Users/medium/x");
    }

    #[test]
    fn scrub_home_windows_variants() {
        let home = Some(r"C:\Users\me");
        assert_eq!(scrub_home(r"C:\Users\me\.codex", home), r"~\.codex");
        assert_eq!(scrub_home("C:/Users/me/.codex", home), "~/.codex");
        assert_eq!(scrub_home(r"\\?\C:\Users\me\.codex", home), r"~\.codex");
        assert_eq!(scrub_home(r"c:\users\ME\.codex", home), r"~\.codex");
        assert_eq!(
            scrub_home(r"C:\Windows;C:\Users\me\bin;C:/Users/me/.local/bin", home),
            r"C:\Windows;~\bin;~/.local/bin"
        );
    }

    #[test]
    fn scrub_home_noop_cases() {
        let s = "/Users/me/.codex and C:\\Users\\me";
        assert_eq!(scrub_home(s, None), s);
        assert_eq!(scrub_home(s, Some("")), s);
        assert_eq!(scrub_home(s, Some("/")), s);
    }

    #[test]
    fn render_diagnostics_scrubs_values_before_json_escaping() {
        let s = snap(
            vec![entry(
                0,
                TrailStage::Env,
                json!({ "exe": "C:\\Users\\me\\bin\\codex.cmd" }),
            )],
            0,
        );
        let out = render_diagnostics("proj-a", "alpha", &s, Some(r"C:\Users\me"));
        assert!(out.contains(r#""exe":"~\\bin\\codex.cmd""#), "{out}");
        assert!(!out.contains(r"me\\"), "{out}");
    }

    #[test]
    fn render_diagnostics_redacts_late_unsanitized_entries() {
        let r = TransferRegistry::new();
        let _g = r.begin("proj-a", "alpha").unwrap();
        // The raw internal path skips write-time sanitizing.
        r.push_trail_sanitized_at(
            "proj-a",
            "alpha",
            TrailStage::Import,
            json!({ "prompt": "x" }),
            Instant::now(),
        );
        let out = diagnostics_text(&r, "proj-a", "alpha", None).unwrap();
        assert!(out.contains(r#"{"prompt":"[redacted]"}"#), "{out}");
        assert!(!out.contains(r#""x""#), "{out}");
    }

    #[test]
    fn transfer_diagnostics_none_without_trail() {
        let r = TransferRegistry::new();
        assert_eq!(
            diagnostics_text(&r, "proj-a", "alpha", Some("/Users/me")),
            None
        );
        let _g = r.begin("proj-a", "alpha").unwrap();
        assert_eq!(
            diagnostics_text(&r, "proj-a", "alpha", Some("/Users/me")),
            None
        );
    }
}
