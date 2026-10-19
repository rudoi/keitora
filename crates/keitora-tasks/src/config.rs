//! Locating and loading the Keitora configuration.
//!
//! Two-tier TOML system:
//! - A per-user **registry** (`UserConfig`) at `dirs::config_dir()/keitora/keitora.toml`
//!   listing the known projects and the active one.
//! - A per-project **`ProjectConfig`** — a `keitora.toml` at the project root — that
//!   holds the `[project]` metadata and the `[tasks]` settings (shared DB path,
//!   worktrees, archive windows).
//!
//! `resolve()` (cwd-first, for the CLI) and `resolve_active()` (registry-first,
//! for the GUI) locate the active `ProjectConfig`. A back-compat [`Config`] shim
//! preserves the old public surface so existing consumers keep compiling.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::model::{ResolvedWorktree, WorktreeStatus};
use crate::Error;

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

pub(crate) fn default_archive_days() -> i64 {
    30
}
pub(crate) fn default_log_retention_days() -> i64 {
    90
}
fn default_backend() -> String {
    "sqlite".to_string()
}
pub(crate) fn default_worktrees_dir() -> String {
    "../keitora-worktrees".to_string()
}

/// Expand a leading `~` in a path string to the user's home directory.
///
/// `"~"` → `dirs::home_dir()`; `"~/sub/dir"` → `home_dir().join("sub/dir")`;
/// anything else is returned verbatim. Falls back to `PathBuf::from(s)` when the
/// home directory cannot be determined. `PathBuf::join` does **not** expand `~`,
/// so this must run before any `join`/`canonicalize` on a `~`-prefixed value.
pub(crate) fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(s));
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(s)
}

// ---------------------------------------------------------------------------
// CliKind — which coding CLI drives a session
// ---------------------------------------------------------------------------

/// Which coding CLI a session runs. The single switch that selects binary
/// discovery, launch argv, autonomy flag, dispatch ritual, transfer strategy,
/// liveness source, and the rail glyph — everything currently hardcoded to
/// "claude" routes through it.
///
/// Serializes/deserializes as the bare TOML strings `"claude"` / `"codex"` (e.g.
/// `cli = "codex"` on a worktree table, `default_cli = "codex"` on `UserConfig`).
/// Mirrors [`crate::model::WorktreeStatus`]'s `rename_all = "snake_case"`
/// convention, extended to `Deserialize` since — unlike `WorktreeStatus` — this
/// type must round-trip through TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliKind {
    #[default]
    Claude,
    Codex,
}

/// Claude session ids are embedded as bare shell arguments on every platform.
pub fn is_safe_claude_session_id(id: &str) -> bool {
    id.len() <= 128
        && id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

// ---------------------------------------------------------------------------
// ProviderConfig — the optional per-CLI `[claude]` / `[codex]` table
// ---------------------------------------------------------------------------

/// The optional per-CLI table (`[claude]` / `[codex]`): where to find the CLI
/// executable and whether to launch it in autonomy mode.
///
/// Resolved hierarchically by [`resolve_provider_path`] — a project `keitora.toml`
/// override wins over the per-user registry, which is populated by one-time
/// auto-discovery ([`discover_cli`]) on first GUI launch. Stored so a
/// LaunchServices-spawned app (whose inherited `PATH` is minimal) can still find
/// the CLI.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    /// Absolute path to the CLI executable. `None` until discovered or set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// When `Some(true)`, interactive sessions launch in autonomy mode (claude:
    /// `--dangerously-skip-permissions`; codex:
    /// `--dangerously-bypass-approvals-and-sandbox`). `None` means "not
    /// configured", so a project override is distinguishable from an explicit
    /// false; resolved project-over-user by [`resolve_autonomy`], mirroring `path`.
    /// `#[serde(alias = "skip_permissions")]` keeps the legacy `[claude]
    /// skip_permissions = true` key deserializing unchanged.
    #[serde(
        default,
        alias = "skip_permissions",
        skip_serializing_if = "Option::is_none"
    )]
    pub autonomy: Option<bool>,
}

impl ProviderConfig {
    /// True when nothing is configured, so the whole table can be omitted from the
    /// serialized TOML rather than written empty.
    pub fn is_empty(&self) -> bool {
        self.path.is_none() && self.autonomy.is_none()
    }
}

// ---------------------------------------------------------------------------
// CliConfig — the optional `[cli]` table
// ---------------------------------------------------------------------------

/// The optional `[cli]` table: an override path to the `keitora` CLI binary.
///
/// Used by the session-hook installer: when set (project `keitora.toml` wins over the
/// per-user registry, via [`resolve_keitora_path`]) it points hooks at a specific
/// `keitora` build — e.g. while working on keitora itself. When unset, the installer falls
/// back to a worktree's own build and then `keitora` on PATH ([`keitora_on_path`]). Mirrors
/// [`ProviderConfig`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CliConfig {
    /// Path to the `keitora` executable (absolute or `~`-prefixed). `None` = use PATH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl CliConfig {
    /// True when nothing is configured, so the whole `[cli]` table can be omitted
    /// from the serialized TOML rather than written empty.
    pub fn is_empty(&self) -> bool {
        self.path.is_none()
    }
}

// ---------------------------------------------------------------------------
// DispatchConfig — the optional `[dispatch]` table
// ---------------------------------------------------------------------------

/// The optional `[dispatch]` table: a project's new-task ritual.
///
/// `steps` are ordered lines typed into the target worktree's prompt window when
/// a task is dispatched, each submitted as its own line. Each line supports the
/// fixed placeholder set rendered by [`crate::dispatch::interpolate`]
/// (`{task_number}`, `{task_id}`, `{task_title}`, `{task_description}`,
/// `{branch}`, `{worktree}`). When empty, Keitora falls back to its built-in
/// single-line "please pick up task …" prompt.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DispatchConfig {
    /// Ordered prompt lines for a Claude session, each followed by a submit
    /// (Enter). Empty → built-in. `steps` is a deserialize-only alias of the
    /// legacy key name (old files keep loading); freshly written TOML always
    /// uses `claude_steps`.
    #[serde(default, alias = "steps")]
    pub claude_steps: Vec<String>,
    /// Same, for a Codex session.
    #[serde(default)]
    pub codex_steps: Vec<String>,
}

impl DispatchConfig {
    /// True when no steps are configured for either CLI, so the whole `[dispatch]`
    /// table can be omitted from the serialized TOML rather than written empty.
    pub fn is_empty(&self) -> bool {
        self.claude_steps.is_empty() && self.codex_steps.is_empty()
    }
}

/// A commented `[dispatch]` example appended to a freshly scaffolded `keitora.toml`
/// (only while no steps are configured) so the feature is discoverable without
/// changing behavior. Comments are ignored on load.
const DISPATCH_SCAFFOLD_HINT: &str = "\n\
    # Custom dispatch ritual: ordered lines typed into the target worktree's\n\
    # prompt window when a task is dispatched, each submitted as its own line.\n\
    # Uncomment and edit to override Keitora's built-in dispatch prompt.\n\
    # Placeholders: {task_number} {task_id} {task_title} {task_description} {branch} {worktree}\n\
    # [dispatch]\n\
    # steps = [\n\
    #   \"/clear\",\n\
    #   \"/park\",\n\
    #   \"/workflow pick up task #{task_number}: {task_title}\",\n\
    # ]\n";

// ---------------------------------------------------------------------------
// ResolveError
// ---------------------------------------------------------------------------

/// Errors returned by the resolution functions. Kept separate from the crate's
/// [`Error`] so resolution can express its three distinct outcomes.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// No `keitora.toml` found and no usable registry entry.
    /// CLI path: hard error; GUI path: unreachable (use `resolve_active`).
    #[error("no keitora.toml found — run `keitora init` to create one, or set $KEITORA_PROJECT")]
    NotFound,

    /// The user registry (`keitora.toml`) exists but lists no projects and has no
    /// active entry. GUI path: show the onboarding wizard. CLI path: surfaced as
    /// `NotFound`.
    #[error("no project registered — open Keitora and complete the onboarding wizard")]
    NeedsOnboarding,

    /// I/O or TOML parse failure.
    #[error("config error: {0}")]
    Io(String),
}

// ---------------------------------------------------------------------------
// UserConfig — the per-user project registry
// ---------------------------------------------------------------------------

/// The per-user registry of known projects, persisted at
/// `dirs::config_dir()/keitora/keitora.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
    /// Name of the active project, if any. Omitted from TOML when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// Display order of registered projects (registry keys). Keys missing from
    /// this list sort alphabetically after it; see [`UserConfig::ordered_project_names`].
    /// Declared before any table so TOML emits it as a plain value. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_order: Vec<String>,
    /// User-level `claude` location (the default across projects). Discovered
    /// once and persisted. Omitted from TOML when empty.
    #[serde(default, skip_serializing_if = "ProviderConfig::is_empty")]
    pub claude: ProviderConfig,
    /// User-level `codex` location + autonomy default. Omitted from TOML
    /// when empty.
    #[serde(default, skip_serializing_if = "ProviderConfig::is_empty")]
    pub codex: ProviderConfig,
    /// User-level default CLI: the fallback for a worktree whose entry has
    /// no explicit `cli`. `None` (unconfigured) resolves to `CliKind::Claude` via
    /// [`resolve_cli`]. Omitted from TOML when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_cli: Option<CliKind>,
    /// User-level `keitora` CLI override (the default across projects). Omitted from
    /// TOML when empty.
    #[serde(default, skip_serializing_if = "CliConfig::is_empty")]
    pub cli: CliConfig,
    /// Per-machine herdr location and settings.
    /// Omitted from TOML when at its default.
    #[serde(default, skip_serializing_if = "PtyConfig::is_empty")]
    pub pty: PtyConfig,
    /// Registered projects, keyed by name.
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectRef>,
}

/// herdr-specific settings under `[pty.herdr]`. Omitted from serialized TOML when
/// default.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct HerdrPtyConfig {
    /// Automatic restart/resume after the herdr server dies.
    #[serde(default, skip_serializing_if = "RecoveryConfig::is_default")]
    pub recovery: RecoveryConfig,
}

impl HerdrPtyConfig {
    pub fn is_empty(&self) -> bool {
        self.recovery.is_default()
    }
}

/// `[pty.herdr.recovery]` — automatic restart/resume after the herdr server dies.
/// Every field has a serde default so a partial table is valid. Durations are whole
/// seconds, sizes whole MiB. Read once at app start.
///
/// Example (user config `keitora.toml`):
/// ```toml
/// [pty.herdr.recovery]
/// headroom_base_mib = 4096
/// backoff_secs = [0, 60, 300]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RecoveryConfig {
    /// Free commit memory that must remain after the next session starts.
    pub headroom_base_mib: u64,
    /// Estimated memory cost of one more session (counted against both commit and
    /// physical memory).
    pub headroom_per_session_mib: u64,
    /// Headroom must stay sufficient continuously this long before recovering.
    pub headroom_sustain_secs: u64,
    /// Give up waiting for headroom after this long and pause recovery.
    pub headroom_cap_secs: u64,
    /// Delay between the end of one resume and the start of the next.
    pub stagger_secs: u64,
    /// How many sessions may resume at the same time.
    pub resume_concurrency: u32,
    /// Delay before restart attempt k (index `min(k-1, len-1)`), where k is the
    /// number of deaths inside the breaker window.
    pub backoff_secs: Vec<u64>,
    /// This many deaths inside `breaker_window_secs` pause recovery.
    pub breaker_deaths: u32,
    pub breaker_window_secs: u64,
    /// A server that dies sooner than this after starting is an early death.
    pub early_death_secs: u64,
    /// A death this soon after the user pressed "Resume now" pauses at once.
    pub resume_retrip_secs: u64,
    /// Continuous healthy time that clears the breaker and budget state.
    pub healthy_reset_secs: u64,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        RecoveryConfig {
            headroom_base_mib: 4096,
            headroom_per_session_mib: 600,
            headroom_sustain_secs: 60,
            headroom_cap_secs: 600,
            stagger_secs: 5,
            resume_concurrency: 3,
            backoff_secs: vec![0, 60, 300],
            breaker_deaths: 3,
            breaker_window_secs: 1800,
            early_death_secs: 30,
            resume_retrip_secs: 600,
            healthy_reset_secs: 1800,
        }
    }
}

const MIB_BYTES: u64 = 1024 * 1024;

impl RecoveryConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Clamp nonsense: an empty `backoff_secs` becomes `[0]`; `breaker_deaths` below 1
    /// becomes 1; `resume_concurrency` below 1 becomes 1; a sustain longer than the cap is cut to the cap. Everything else is
    /// taken as-is (0 is legal and means "no wait").
    pub fn normalized(&self) -> RecoveryConfig {
        let mut out = self.clone();
        if out.backoff_secs.is_empty() {
            out.backoff_secs = vec![0];
        }
        if out.breaker_deaths < 1 {
            out.breaker_deaths = 1;
        }
        if out.resume_concurrency < 1 {
            out.resume_concurrency = 1;
        }
        if out.headroom_sustain_secs > out.headroom_cap_secs {
            out.headroom_sustain_secs = out.headroom_cap_secs;
        }
        out
    }

    /// Resolved (normalized) commit margin in bytes. The one value other code reads
    /// for the commit margin; nothing re-parses the TOML or hard-codes it.
    pub fn base_margin_bytes(&self) -> u64 {
        self.normalized()
            .headroom_base_mib
            .saturating_mul(MIB_BYTES)
    }

    /// Resolved (normalized) per-session cost in bytes.
    pub fn per_session_bytes(&self) -> u64 {
        self.normalized()
            .headroom_per_session_mib
            .saturating_mul(MIB_BYTES)
    }
}

/// The optional `[pty]` table in the user registry: where the herdr executable
/// lives and how the herdr session server is configured.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PtyConfig {
    /// Explicit path to the herdr executable, so resolution does not depend on
    /// `PATH`. herdr self-updates into *versioned* directories
    /// (e.g. `~/.herdr/packages/standalone/releases/<ver>-<triple>/herdr.exe`),
    /// so a stored absolute path can go stale across updates — callers should
    /// re-validate it (see [`herdr_bin_candidates`]) and fall back to discovery.
    /// `~` is expanded on read. Omitted from TOML when unset.
    ///
    /// Example (user config `keitora.toml`):
    /// ```toml
    /// [pty]
    /// herdr_bin = "~/.herdr/packages/standalone/releases/0.9.3-x86_64-pc-windows-msvc/herdr.exe"
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_bin: Option<PathBuf>,
    /// Per-pane scrollback herdr keeps, in bytes (herdr's
    /// `[advanced] scrollback_limit_bytes`). Unset means Keitora's default (4 MB);
    /// values outside 100 000 ..= 100 000 000 fall back to the default. It applies
    /// to panes created after the herdr server next starts: existing panes, and a
    /// server that is already running, keep their old limit. Omitted from TOML when
    /// unset.
    ///
    /// Example (user config `keitora.toml`):
    /// ```toml
    /// [pty]
    /// herdr_scrollback_limit_bytes = 2000000
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_scrollback_limit_bytes: Option<u64>,
    /// herdr-specific settings under `[pty.herdr]` (a table that coexists with the
    /// flat `herdr_bin` key). Omitted from TOML when default.
    #[serde(default, skip_serializing_if = "HerdrPtyConfig::is_empty")]
    pub herdr: HerdrPtyConfig,
}

impl PtyConfig {
    /// True when nothing is configured (no explicit herdr path, no scrollback override, default herdr settings), so the whole
    /// `[pty]` table can be omitted from serialized TOML.
    pub fn is_empty(&self) -> bool {
        self.herdr_bin.is_none()
            && self.herdr_scrollback_limit_bytes.is_none()
            && self.herdr.is_empty()
    }

    /// The configured herdr path with `~` expanded, if any.
    pub fn herdr_bin_expanded(&self) -> Option<PathBuf> {
        self.herdr_bin
            .as_ref()
            .and_then(|p| p.to_str())
            .map(expand_tilde)
    }
}

/// Ordered candidate paths for the herdr executable, highest priority first:
/// 1. the explicit `[pty] herdr_bin` config (tilde-expanded),
/// 2. the `KEITORA_HERDR_BIN` environment override (`env_override`),
/// 3. any caller-supplied `fallbacks` in order (e.g. a bundled sidecar next to
///    the app exe, then a `PATH` hit).
///
/// Pure (no filesystem access) and deterministic, so it is unit-testable; the
/// caller picks the first candidate that actually exists on disk. Duplicate
/// paths are de-duplicated while preserving priority order.
pub fn herdr_bin_candidates(
    cfg: &PtyConfig,
    env_override: Option<PathBuf>,
    fallbacks: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let push = |p: PathBuf, out: &mut Vec<PathBuf>| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(p) = cfg.herdr_bin_expanded() {
        push(p, &mut out);
    }
    if let Some(p) = env_override {
        push(p, &mut out);
    }
    for p in fallbacks {
        push(p, &mut out);
    }
    out
}

/// One persisted worktree: its name and the path git reports for it.
/// `path == None` is a legacy/unmigrated entry (deserialized from a bare string,
/// or written before path-carrying entries); the repair pass backfills it from `git worktree list`.
///
/// De/serialization is **custom** (not derived) so the on-disk TOML stays
/// backward-compatible and clean:
/// - a `None` path serializes to a bare string (`"delta"`), preserving the legacy
///   form so existing configs round-trip unchanged;
/// - a `Some` path serializes to an inline table (`{ name = "delta", path = "…" }`).
///
/// Deserialization accepts either shape, via an `#[serde(untagged)]` helper enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub name: String,
    pub path: Option<PathBuf>,
    /// Which CLI drives this worktree's session. `None` ⇒ fall back to
    /// `UserConfig.default_cli` (then `CliKind::Claude`) via [`resolve_cli`].
    /// Registry-only — never written to the committed `[tasks].worktrees` file.
    pub cli: Option<CliKind>,
    /// Last/imported Codex thread to resume on restart (contract C3).
    /// Registry-only — never written to the committed `[tasks].worktrees` file.
    pub codex_thread_id: Option<String>,
    /// Claude session to resume after a Claude→Codex→Claude round-trip.
    /// Registry-only — never written to the committed `[tasks].worktrees` file.
    pub claude_session_id: Option<String>,
    /// Per-worktree diff-view base override. `None` ⇒ fall back to the project default
    /// ([`ProjectMeta::diff_base`]), then [`DiffBase::Auto`].
    /// Registry-only — never written to the committed `[tasks].worktrees` file.
    pub diff_base: Option<DiffBase>,
}

impl WorktreeEntry {
    /// A legacy / unmigrated entry: name only, path to be backfilled by repair.
    pub fn name_only(name: impl Into<String>) -> Self {
        WorktreeEntry {
            name: name.into(),
            path: None,
            cli: None,
            codex_thread_id: None,
            claude_session_id: None,
            diff_base: None,
        }
    }

    /// A fully-resolved entry carrying the path git reports for `name`.
    pub fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        WorktreeEntry {
            name: name.into(),
            path: Some(path.into()),
            cli: None,
            codex_thread_id: None,
            claude_session_id: None,
            diff_base: None,
        }
    }
}

/// Wire shape for [`WorktreeEntry`]: either a bare string (legacy, path-less) or
/// an inline `{ name, path }` table. `#[serde(untagged)]` tries each variant in
/// order, which lets a single TOML array mix both forms.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum WorktreeEntryRepr {
    /// Inline table: `{ name = "…", path = "…" }`. Listed first so a table is
    /// never mis-parsed as a string. `path` is optional so a hand-edited,
    /// path-less table (`{ name = "x" }`) still deserializes (path: None) instead
    /// of failing the whole config load.
    Table {
        name: String,
        #[serde(default)]
        path: Option<PathBuf>,
        #[serde(default)]
        cli: Option<CliKind>,
        #[serde(default)]
        codex_thread_id: Option<String>,
        #[serde(default)]
        claude_session_id: Option<String>,
        #[serde(default)]
        diff_base: Option<DiffBase>,
    },
    /// Bare string: `"delta"` → legacy, path-less.
    Name(String),
}

impl Serialize for WorktreeEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Structured fields require the table form; a bare string would drop them.
        if self.path.is_some()
            || self.cli.is_some()
            || self.codex_thread_id.is_some()
            || self.claude_session_id.is_some()
            || self.diff_base.is_some()
        {
            WorktreeEntryRepr::Table {
                name: self.name.clone(),
                path: self.path.clone(),
                cli: self.cli,
                codex_thread_id: self.codex_thread_id.clone(),
                claude_session_id: self.claude_session_id.clone(),
                diff_base: self.diff_base.clone(),
            }
            .serialize(serializer)
        } else {
            WorktreeEntryRepr::Name(self.name.clone()).serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for WorktreeEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match WorktreeEntryRepr::deserialize(deserializer)? {
            WorktreeEntryRepr::Name(name) => WorktreeEntry {
                name,
                path: None,
                cli: None,
                codex_thread_id: None,
                claude_session_id: None,
                diff_base: None,
            },
            WorktreeEntryRepr::Table {
                name,
                path,
                cli,
                codex_thread_id,
                claude_session_id,
                diff_base,
            } => WorktreeEntry {
                name,
                path,
                cli,
                codex_thread_id,
                claude_session_id,
                diff_base,
            },
        })
    }
}

/// Merge registry-only fields from existing entries by name (hazard I2).
/// Incoming values win; missing values are carried forward so `{name, path}`-only
/// settings updates cannot erase provider state.
fn merge_worktree_registry_fields(
    existing: Option<&[WorktreeEntry]>,
    incoming: &[WorktreeEntry],
) -> Vec<WorktreeEntry> {
    incoming
        .iter()
        .map(|inc| {
            let mut merged = inc.clone();
            if merged.cli.is_none()
                || merged.codex_thread_id.is_none()
                || merged.claude_session_id.is_none()
                || merged.diff_base.is_none()
            {
                if let Some(old) =
                    existing.and_then(|list| list.iter().find(|e| e.name == inc.name))
                {
                    if merged.cli.is_none() {
                        merged.cli = old.cli;
                    }
                    if merged.codex_thread_id.is_none() {
                        merged.codex_thread_id = old.codex_thread_id.clone();
                    }
                    if merged.claude_session_id.is_none() {
                        merged.claude_session_id = old.claude_session_id.clone();
                    }
                    if merged.diff_base.is_none() {
                        merged.diff_base = old.diff_base.clone();
                    }
                }
            }
            merged
        })
        .collect()
}

/// `entries` with the ones named in `order` first, in that order, then every other entry
/// in its original relative order. Names in `order` with no entry, and repeats, are
/// skipped; every entry appears exactly once, entries sharing a name staying together
/// in their original order.
pub fn reorder_entries(entries: &[WorktreeEntry], order: &[String]) -> Vec<WorktreeEntry> {
    let mut placed = vec![false; entries.len()];
    let mut out = Vec::with_capacity(entries.len());
    for name in order {
        for (i, e) in entries.iter().enumerate() {
            if !placed[i] && &e.name == name {
                placed[i] = true;
                out.push(e.clone());
            }
        }
    }
    out.extend(
        entries
            .iter()
            .zip(&placed)
            .filter(|(_, &p)| !p)
            .map(|(e, _)| e.clone()),
    );
    out
}

/// A registered project: a name (the map key) plus the absolute path to its root,
/// plus optional per-machine setting overrides. Each `Some(_)` is authoritative
/// over the committed `keitora.toml`; `None` falls back to the committed value, then
/// the scaffold default (see `effective_config`). Generalizes the `worktrees`
/// precedence to the rest of the editable project settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRef {
    pub path: PathBuf,
    /// Per-machine worktree list, each carrying a name and the path git reports for
    /// it ([`WorktreeEntry`]). `Some(list)` is authoritative (even `Some([])`,
    /// meaning "explicitly no worktrees"); `None` means "not set — fall back to the
    /// committed `[tasks].worktrees`". Omitted from TOML when `None`. Legacy bare
    /// strings deserialize to path-less entries (backfilled by the repair pass).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktrees: Option<Vec<WorktreeEntry>>,
    /// Per-machine `[tasks].db_path` override (raw; resolved against `path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_path: Option<PathBuf>,
    /// Per-machine `[project].worktrees_dir` override (raw; tilde preserved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktrees_dir: Option<String>,
    /// Per-machine `[tasks].archive_days` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_days: Option<i64>,
    /// Per-machine `[tasks].log_retention_days` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_retention_days: Option<i64>,
    /// Per-machine `[dispatch].claude_steps` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_steps: Option<Vec<String>>,
    /// Per-machine `[dispatch].codex_steps` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_dispatch_steps: Option<Vec<String>>,
    /// Per-machine `[project].diff_base` override (diff-view comparison base).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_base: Option<DiffBase>,
}

impl ProjectRef {
    /// A bare registry entry for `path` with no overrides set (all fall back to the
    /// committed file / scaffold defaults).
    pub fn new(path: PathBuf) -> Self {
        ProjectRef {
            path,
            worktrees: None,
            db_path: None,
            worktrees_dir: None,
            archive_days: None,
            log_retention_days: None,
            dispatch_steps: None,
            codex_dispatch_steps: None,
            diff_base: None,
        }
    }
}

/// Per-project setting overrides to write to a [`ProjectRef`]. The nested
/// `Option` distinguishes three intents per field:
/// - `None` — leave the existing override unchanged,
/// - `Some(None)` — clear the override (revert to committed file / scaffold default),
/// - `Some(Some(x))` — set the override to `x`.
#[derive(Debug, Default, Clone)]
pub struct ProjectSettingsPatch {
    pub db_path: Option<Option<PathBuf>>,
    pub worktrees_dir: Option<Option<String>>,
    pub archive_days: Option<Option<i64>>,
    pub log_retention_days: Option<Option<i64>>,
    pub dispatch_steps: Option<Option<Vec<String>>>,
    pub codex_dispatch_steps: Option<Option<Vec<String>>>,
    pub diff_base: Option<Option<DiffBase>>,
}

impl ProjectSettingsPatch {
    /// Apply the patch in place: only fields that are `Some(_)` touch `pref`.
    fn apply_to(&self, pref: &mut ProjectRef) {
        if let Some(v) = &self.db_path {
            pref.db_path = v.clone();
        }
        if let Some(v) = &self.worktrees_dir {
            pref.worktrees_dir = v.clone();
        }
        if let Some(v) = &self.archive_days {
            pref.archive_days = *v;
        }
        if let Some(v) = &self.log_retention_days {
            pref.log_retention_days = *v;
        }
        if let Some(v) = &self.dispatch_steps {
            pref.dispatch_steps = v.clone();
        }
        if let Some(v) = &self.codex_dispatch_steps {
            pref.codex_dispatch_steps = v.clone();
        }
        if let Some(v) = &self.diff_base {
            pref.diff_base = v.clone();
        }
    }
}

/// The path to the user registry. `$KEITORA_CONFIG_HOME` overrides the OS config
/// dir (used by tests for hermeticity, and as an escape hatch in production);
/// otherwise it is `dirs::config_dir()/keitora/keitora.toml`.
///
/// `pub(crate)` so `config_edit` writes land on exactly the file `UserConfig`
/// reads — a single source of truth for the registry path.
pub(crate) fn user_config_path() -> Result<PathBuf, ResolveError> {
    let dir = match std::env::var_os("KEITORA_CONFIG_HOME") {
        Some(home) => PathBuf::from(home),
        None => dirs::config_dir()
            .ok_or_else(|| ResolveError::Io("cannot determine the OS config dir".to_string()))?,
    };
    Ok(dir.join("keitora").join("keitora.toml"))
}

/// Serializes in-process load-modify-save cycles of the user registry.
static REGISTRY_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl UserConfig {
    /// Hold for the whole of a load → modify → save of the user registry so
    /// concurrent writers in this process cannot drop each other's changes (a
    /// lost `active`, a stale `project_order`). Not reentrant: never call a
    /// registry writer that takes it (e.g. [`UserConfig::set_skip_permissions`],
    /// `set_project_worktrees`, `set_user_claude_path`) while holding it.
    pub fn write_lock() -> std::sync::MutexGuard<'static, ()> {
        // The guarded data is `()`: a panic mid-write leaves nothing to repair.
        REGISTRY_WRITE.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Load the registry from the OS path. Returns the default (empty) registry
    /// if the file does not exist.
    pub fn load() -> Result<UserConfig, ResolveError> {
        Self::load_from(&user_config_path()?)
    }

    /// Load the registry from an explicit path (used by tests). Returns the
    /// default (empty) registry if the file does not exist.
    pub fn load_from(path: &Path) -> Result<UserConfig, ResolveError> {
        if !path.exists() {
            return Ok(UserConfig::default());
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| ResolveError::Io(format!("reading {}: {e}", path.display())))?;
        toml::from_str(&text)
            .map_err(|e| ResolveError::Io(format!("parsing {}: {e}", path.display())))
    }

    /// Insert or update a project entry. Re-registering the *same* project (same
    /// canonical path) preserves its per-machine worktree list; reusing a name for
    /// a *different* path resets the list (the old project's worktrees must not
    /// leak onto an unrelated one).
    pub fn register(&mut self, name: &str, path: PathBuf) {
        match self.projects.get_mut(name) {
            Some(entry) => {
                if canonical_key(&entry.path) != canonical_key(&path) {
                    // Different project under the same name: the old project's
                    // per-machine overrides must not leak onto an unrelated one.
                    entry.worktrees = None;
                    entry.db_path = None;
                    entry.worktrees_dir = None;
                    entry.archive_days = None;
                    entry.log_retention_days = None;
                    entry.dispatch_steps = None;
                    entry.codex_dispatch_steps = None;
                    entry.diff_base = None;
                }
                entry.path = path;
            }
            None => {
                self.projects
                    .insert(name.to_string(), ProjectRef::new(path));
            }
        }
    }

    /// Set the per-machine worktree list for the registered project(s) whose root
    /// matches `root` (canonicalized). Returns `true` if at least one project was
    /// updated and the registry saved; `false` if no registered project has that
    /// root (the caller should then fall back to the committed-file write). Updates
    /// *every* matching entry so duplicate-root registrations can't diverge.
    pub fn set_project_worktrees_by_root(
        &mut self,
        root: &Path,
        entries: &[WorktreeEntry],
    ) -> Result<bool, ResolveError> {
        let target = canonical_key(root);
        let mut matched = false;
        for pref in self.projects.values_mut() {
            if canonical_key(&pref.path) == target {
                pref.worktrees = Some(merge_worktree_registry_fields(
                    pref.worktrees.as_deref(),
                    entries,
                ));
                matched = true;
            }
        }
        if matched {
            self.save()?;
        }
        Ok(matched)
    }

    /// Apply per-project setting overrides to the registered project(s) whose
    /// root matches `root` (canonicalized). Returns `true` if at least one entry was
    /// updated and the registry saved; `false` if no registered project has that root
    /// (the caller falls back to the committed-file write). Updates *every* matching
    /// entry so duplicate-root registrations can't diverge. Mirrors
    /// [`Self::set_project_worktrees_by_root`].
    pub fn set_project_settings_by_root(
        &mut self,
        root: &Path,
        patch: ProjectSettingsPatch,
    ) -> Result<bool, ResolveError> {
        let target = canonical_key(root);
        let mut matched = false;
        for pref in self.projects.values_mut() {
            if canonical_key(&pref.path) == target {
                patch.apply_to(pref);
                matched = true;
            }
        }
        if matched {
            self.save()?;
        }
        Ok(matched)
    }

    /// Set (or clear, with `base: None`) one worktree's diff-base override for the registered
    /// project(s) whose root matches `root`. Unlike [`Self::set_project_worktrees_by_root`], this
    /// does not go through the registry-field carry-forward merge — so clearing the override
    /// actually clears it. `current` is the effective worktree list (so an override can be stamped
    /// even when the registry list was still falling back to the committed one). Returns `true`
    /// if a project matched and the registry was saved.
    pub fn set_worktree_diff_base_by_root(
        &mut self,
        root: &Path,
        worktree: &str,
        base: Option<DiffBase>,
        current: &[WorktreeEntry],
    ) -> Result<bool, ResolveError> {
        let target = canonical_key(root);
        let mut matched = false;
        for pref in self.projects.values_mut() {
            if canonical_key(&pref.path) == target {
                let mut list = pref.worktrees.clone().unwrap_or_else(|| current.to_vec());
                match list.iter_mut().find(|e| e.name == worktree) {
                    Some(e) => e.diff_base = base.clone(),
                    None => {
                        let mut entry = current
                            .iter()
                            .find(|e| e.name == worktree)
                            .cloned()
                            .unwrap_or_else(|| WorktreeEntry::name_only(worktree));
                        entry.diff_base = base.clone();
                        list.push(entry);
                    }
                }
                pref.worktrees = Some(list);
                matched = true;
            }
        }
        if matched {
            self.save()?;
        }
        Ok(matched)
    }

    /// The registered project whose root matches `root` (canonical comparison), if
    /// any. Lets callers that hold only a path (e.g. the `discover_worktrees` host
    /// command) reach `effective_config` for a project that may have no `keitora.toml`.
    pub fn project_ref_by_root(&self, root: &Path) -> Option<&ProjectRef> {
        let key = canonical_key(root);
        self.projects
            .values()
            .find(|p| canonical_key(&p.path) == key)
    }

    /// Set the active project by name.
    pub fn set_active(&mut self, name: &str) {
        self.active = Some(name.to_string());
    }

    /// Registry keys in display order: `project_order` entries that are still
    /// registered (first occurrence wins), then every remaining key alphabetically.
    pub fn ordered_project_names(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<String> = self
            .project_order
            .iter()
            .filter(|k| self.projects.contains_key(*k) && seen.insert(k.as_str()))
            .cloned()
            .collect();
        out.extend(
            self.projects
                .keys()
                .filter(|k| !seen.contains(k.as_str()))
                .cloned(),
        );
        out
    }

    /// Replace the display order with `order`, tolerating a partial list (the UI
    /// only knows the projects it could load). Unknown keys and repeats are
    /// ignored; registered keys absent from `order` keep their current relative
    /// order and follow the given ones.
    pub fn set_project_order(&mut self, order: Vec<String>) {
        let mut seen = std::collections::HashSet::new();
        let mut next: Vec<String> = order
            .into_iter()
            .filter(|k| self.projects.contains_key(k) && seen.insert(k.clone()))
            .collect();
        let rest: Vec<String> = self
            .ordered_project_names()
            .into_iter()
            .filter(|k| !seen.contains(k))
            .collect();
        next.extend(rest);
        self.project_order = next;
    }

    /// Record the user-level `claude` path (from discovery or an explicit set).
    pub fn set_claude_path(&mut self, path: impl Into<String>) {
        self.claude.path = Some(path.into());
    }

    /// The user-level `[claude] skip_permissions` setting (`None` → unset → false).
    /// This is the raw user value (seeds the Settings toggle); the *effective*
    /// launch flag is [`resolve_skip_permissions`], which lets a project override.
    pub fn skip_permissions(&self) -> bool {
        self.claude.autonomy.unwrap_or(false)
    }

    /// Set the per-machine `[claude] skip_permissions` flag and persist the
    /// registry. The desktop `set_skip_permissions` command calls this; the GUI
    /// reads it back when building the next Claude launch command.
    pub fn set_skip_permissions(enabled: bool) -> Result<(), ResolveError> {
        let _registry = UserConfig::write_lock();
        let mut cfg = UserConfig::load()?;
        cfg.claude.autonomy = Some(enabled);
        cfg.save()
    }

    /// The active project `(name, ref)` if `active` names a registered project.
    pub fn active_project(&self) -> Option<(&str, &ProjectRef)> {
        let name = self.active.as_deref()?;
        self.projects.get(name).map(|r| (name, r))
    }

    /// Serialize to TOML and write to the OS path, creating parent dirs.
    pub fn save(&self) -> Result<(), ResolveError> {
        self.save_to(&user_config_path()?)
    }

    /// Serialize to TOML and write to an explicit path (used by tests),
    /// creating parent dirs.
    pub fn save_to(&self, path: &Path) -> Result<(), ResolveError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ResolveError::Io(format!("creating {}: {e}", parent.display())))?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| ResolveError::Io(format!("serializing user config: {e}")))?;
        std::fs::write(path, text)
            .map_err(|e| ResolveError::Io(format!("writing {}: {e}", path.display())))
    }
}

// ---------------------------------------------------------------------------
// ProjectConfig — the per-project keitora.toml
// ---------------------------------------------------------------------------

/// A project's `keitora.toml`: `[project]` metadata plus `[tasks]` settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Absolute path to the directory containing `keitora.toml`. Set on load, not
    /// stored in the TOML itself.
    #[serde(skip)]
    pub root: PathBuf,
    /// The effective worktree list, stamped at the resolution boundary from the
    /// user registry's `ProjectRef.worktrees` (with the committed list as fallback).
    /// `#[serde(skip)]` so it can never leak back into the committed `keitora.toml`
    /// through `write()`/`write_initial()`. `None` on paths with no registry match.
    /// Prefer the `worktrees()` accessor over reading this directly.
    #[serde(skip)]
    pub effective_worktrees: Option<Vec<WorktreeEntry>>,
    pub project: ProjectMeta,
    pub tasks: TaskConfig,
    /// Optional per-project `claude` override. Omitted from TOML when empty.
    #[serde(default, skip_serializing_if = "ProviderConfig::is_empty")]
    pub claude: ProviderConfig,
    /// Optional per-project `codex` override. Omitted from TOML when empty.
    #[serde(default, skip_serializing_if = "ProviderConfig::is_empty")]
    pub codex: ProviderConfig,
    /// Optional per-project `keitora` CLI override (for session hooks). Omitted from
    /// TOML when empty.
    #[serde(default, skip_serializing_if = "CliConfig::is_empty")]
    pub cli: CliConfig,
    /// Optional custom dispatch ritual. Omitted from TOML when empty.
    #[serde(default, skip_serializing_if = "DispatchConfig::is_empty")]
    pub dispatch: DispatchConfig,
}

/// What the diff view compares the working tree against. Distinct from
/// [`ProjectMeta::base_branch`] (which governs *worktree creation*): this is a richer
/// spec consumed only by the diff layer. Internally tagged so it round-trips cleanly in
/// both committed TOML (`kind = "ref"`, `name = "…"`) and camelCase JSON for the UI.
///
/// - `Auto` — the diff layer's historical behavior (origin/main → main → origin/HEAD),
///   compared via merge-base. The default when nothing is configured.
/// - `Ref { name }` — compare `HEAD` against the **tip** of `name` (no merge-base).
/// - `Upstream` — the worktree branch's `@{upstream}`, compared via merge-base.
/// - `ForkPoint { parent }` — merge-base of `parent` and `HEAD`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DiffBase {
    #[default]
    Auto,
    Ref {
        name: String,
    },
    Upstream,
    ForkPoint {
        parent: String,
    },
}

/// The `[project]` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMeta {
    pub name: String,
    /// Path from `root` to the worktrees parent directory. May be relative
    /// (resolved against `root`), absolute, or `~`-prefixed. When absent,
    /// `worktrees_root()` defaults to `~/.keitora/<project name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktrees_dir: Option<String>,
    /// Branch new worktrees are created from. When absent, the base branch is
    /// auto-detected: `origin/HEAD`, then `main`, then `master`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Project default for the diff view's comparison base. When absent, falls back to
    /// [`DiffBase::Auto`]. A per-worktree override ([`WorktreeEntry::diff_base`]) wins
    /// over this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_base: Option<DiffBase>,
}

/// GitHub-specific `[tasks.github]` settings.
///
/// All fields are optional; the factory resolves owner/repo from the git remote
/// when omitted. `sidecar_path` defaults to a per-project file under the user
/// registry dir when not set (see [`TaskConfig::resolve_sidecar_path`]).
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct GitHubTaskConfig {
    pub owner: Option<String>,
    pub repo: Option<String>,
    pub project_number: Option<u64>,
    /// Explicit path to the sidecar SQLite file. Relative paths are resolved
    /// against the project root (same convention as `db_path`). When absent the
    /// sidecar is placed under the user registry dir.
    #[serde(default)]
    pub sidecar_path: Option<PathBuf>,
    /// Maps keitora status names to GitHub project column names for non-standard boards.
    /// e.g. `active` → `"In progress"`.
    #[serde(default)]
    pub status_map: BTreeMap<String, String>,
}

/// The `[tasks]` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConfig {
    /// Stored as a relative path in TOML; canonicalized against `root` on read.
    pub db_path: PathBuf,
    /// Fallback worktree list, each carrying a name and (optionally) the path git
    /// reports for it ([`WorktreeEntry`]). The per-machine `ProjectRef.worktrees`
    /// (user registry) wins when present; this committed list is read-only and never
    /// written for registered projects. `#[serde(default)]` so the committed file
    /// may omit it once the registry is the norm everywhere. Legacy bare strings
    /// deserialize to path-less entries (resolved via `worktrees_dir`, the fallback).
    #[serde(default)]
    pub worktrees: Vec<WorktreeEntry>,
    #[serde(default = "default_archive_days")]
    pub archive_days: i64,
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: i64,
    /// Backend kind. `"sqlite"` today; `"github"` future. Defaults to `"sqlite"` so
    /// existing `keitora.toml` files without this key continue to parse unchanged.
    #[serde(default = "default_backend")]
    pub backend: String,
    /// Optional `[tasks.github]` block. `None` when the key is absent so existing
    /// `keitora.toml` files without the section parse unchanged.
    #[serde(default)]
    pub github: Option<GitHubTaskConfig>,
}

impl ProjectConfig {
    /// The effective worktree **names**: the per-machine override stamped at the
    /// resolution boundary, else the committed `[tasks].worktrees` fallback, with the
    /// name derived from each entry. Every reader that only needs names should go
    /// through this (it preserves the legacy `Vec<String>` shape), not
    /// `tasks.worktrees`. For the path-aware view use [`Self::worktree_entries`].
    pub fn worktrees(&self) -> Vec<String> {
        self.worktree_entries()
            .into_iter()
            .map(|e| e.name)
            .collect()
    }

    /// The effective worktree **entries** (name + optional path): the per-machine
    /// override stamped at the resolution boundary, else the committed
    /// `[tasks].worktrees` fallback. The path-aware view backing `worktree_path`
    /// and `resolved_worktrees` — readers that need a worktree's stored path go
    /// through this rather than reconstructing from `worktrees_dir`.
    pub fn worktree_entries(&self) -> Vec<WorktreeEntry> {
        self.effective_worktrees
            .clone()
            .unwrap_or_else(|| self.tasks.worktrees.clone())
    }

    /// The effective diff-view base for `worktree_name`: the per-worktree override
    /// ([`WorktreeEntry::diff_base`]) if set, else the project default
    /// ([`ProjectMeta::diff_base`]), else [`DiffBase::Auto`]. Pass `None` for the
    /// project-level default (no specific worktree in view).
    pub fn diff_base(&self, worktree_name: Option<&str>) -> DiffBase {
        if let Some(name) = worktree_name {
            if let Some(b) = self
                .worktree_entries()
                .into_iter()
                .find(|e| e.name == name)
                .and_then(|e| e.diff_base)
            {
                return b;
            }
        }
        self.project.diff_base.clone().unwrap_or_default()
    }

    /// Stamp the effective worktree list from a known registry entry. For callers
    /// that load a `ProjectConfig` directly (outside `resolve`/`resolve_active`)
    /// but hold the matching `ProjectRef` — e.g. the project switcher listing.
    pub fn apply_registry_worktrees(&mut self, pref: &ProjectRef) {
        self.effective_worktrees = Some(effective_worktrees(pref, self));
    }

    /// Load a `ProjectConfig` from a directory containing `keitora.toml`, or from
    /// an explicit path to a `keitora.toml` file.
    ///
    /// `db_path` is resolved against `root` and canonicalized when it already
    /// exists on disk (falling back to the joined-but-uncanonicalized path so a
    /// fresh DB can still be created).
    pub fn load(path: &Path) -> Result<ProjectConfig, ResolveError> {
        let toml_path = if path.is_dir() {
            path.join("keitora.toml")
        } else {
            path.to_path_buf()
        };
        let text = std::fs::read_to_string(&toml_path)
            .map_err(|e| ResolveError::Io(format!("reading {}: {e}", toml_path.display())))?;
        let mut cfg: ProjectConfig = toml::from_str(&text)
            .map_err(|e| ResolveError::Io(format!("parsing {}: {e}", toml_path.display())))?;

        let root = toml_path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| ResolveError::Io("keitora.toml has no parent dir".to_string()))?;
        cfg.root = canonicalize_root(&root);
        cfg.tasks.db_path = normalize_db_path(&cfg.tasks.db_path, &cfg.root);

        Ok(cfg)
    }

    /// The directory that contains the worktrees: `root` joined with the
    /// configured `worktrees_dir`, normalized (canonicalized when on disk).
    pub fn worktrees_root(&self) -> PathBuf {
        let dir = self
            .project
            .worktrees_dir
            .clone()
            .unwrap_or_else(|| format!("~/.keitora/{}", self.project.name));
        let base = expand_tilde(&dir);
        let joined = if base.is_absolute() {
            base
        } else {
            self.root.join(base)
        };
        // Strip the Windows `\\?\` prefix so the worktrees root is git-compatible
        // (`git worktree add` rejects it) and compares equal to non-canonicalized
        // joins of the same path. No-op on Unix.
        strip_extended_prefix(joined.canonicalize().unwrap_or_else(|_| normalize(&joined)))
    }

    /// The name of the "main" worktree: the basename of the project root.
    pub fn main_worktree(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Resolve a worktree name to its directory. The main worktree (whose name is
    /// the project root's basename) is always the project root itself. For every
    /// other name, the matching effective entry's STORED path wins when present
    /// (the path git reports, which need not live under `worktrees_dir`);
    /// only a legacy/unmigrated entry (or a name not in the list) falls back to
    /// `worktrees_root().join(name)`, the legacy reconstruction.
    pub fn worktree_path(&self, name: &str) -> PathBuf {
        if name == self.main_worktree() {
            return self.root.clone();
        }
        if let Some(path) = self
            .worktree_entries()
            .into_iter()
            .find(|e| e.name == name)
            .and_then(|e| e.path)
        {
            return path;
        }
        self.worktrees_root().join(name)
    }

    /// Resolve the path to the GitHub sidecar SQLite file.
    ///
    /// Resolution order:
    /// 1. `[tasks.github] sidecar_path` if set — resolved against `root` when
    ///    relative (same convention as `db_path`).
    /// 2. Default: `<user-config-dir>/keitora/sidecars/<project-name>.db`, where
    ///    `<user-config-dir>` is `$KEITORA_CONFIG_HOME` or `dirs::config_dir()`.
    ///    This keeps the sidecar stable even when the project working copy is not
    ///    checked out locally (it lives next to the user registry, not in the repo).
    ///
    /// Returns `None` only when the default path cannot be computed (no home dir).
    pub fn resolve_sidecar_path(&self) -> Option<PathBuf> {
        // Explicit override takes precedence.
        if let Some(gh) = &self.tasks.github {
            if let Some(explicit) = &gh.sidecar_path {
                let raw = expand_tilde(&explicit.to_string_lossy());
                let joined = if raw.is_absolute() {
                    raw
                } else {
                    self.root.join(raw)
                };
                return Some(joined.canonicalize().unwrap_or(joined));
            }
        }

        // Default: place the sidecar next to the user registry under a
        // `sidecars/` subdirectory, keyed by project name.
        let config_dir = match std::env::var_os("KEITORA_CONFIG_HOME") {
            Some(home) => PathBuf::from(home),
            None => dirs::config_dir()?,
        };
        let path = config_dir
            .join("keitora")
            .join("sidecars")
            .join(format!("{}.db", self.project.name));
        Some(path)
    }

    /// Build the default config for a fresh project rooted at `root` (name =
    /// `root`'s basename). The single source of truth for scaffolding, shared by
    /// the CLI's `config init` and the app's onboarding so they cannot drift:
    /// repo-relative `db_path` under the conventional `<name>-worktrees/shared/`,
    /// the main worktree pre-listed, and the canonical retention defaults.
    pub fn scaffold(root: &Path) -> ProjectConfig {
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".to_string());
        let worktrees_dir = format!("~/.keitora/{name}");
        let db_path = PathBuf::from(format!("~/.keitora/{name}/shared/tasks.db"));
        ProjectConfig {
            root: root.to_path_buf(),
            effective_worktrees: None,
            project: ProjectMeta {
                name: name.clone(),
                worktrees_dir: Some(worktrees_dir),
                base_branch: None,
                diff_base: None,
            },
            tasks: TaskConfig {
                db_path,
                // The main worktree, name-only — its path is the project root, which
                // `worktree_path` maps directly (no stored path needed).
                worktrees: vec![WorktreeEntry::name_only(name)],
                archive_days: default_archive_days(),
                log_retention_days: default_log_retention_days(),
                backend: default_backend(),
                github: None,
            },
            claude: ProviderConfig::default(),
            codex: ProviderConfig::default(),
            cli: CliConfig::default(),
            dispatch: DispatchConfig::default(),
        }
    }

    /// Serialize to TOML and write `<root>/keitora.toml`. `root` is `#[serde(skip)]`,
    /// so the relative `db_path`/`worktrees_dir` are written as-is (not the
    /// canonicalized absolute form).
    pub fn write(&self) -> Result<(), ResolveError> {
        let path = self.root.join("keitora.toml");
        std::fs::write(&path, self.to_toml()?)
            .map_err(|e| ResolveError::Io(format!("writing {}: {e}", path.display())))
    }

    /// Serialize to TOML (relative paths preserved — see `write`).
    fn to_toml(&self) -> Result<String, ResolveError> {
        toml::to_string_pretty(self)
            .map_err(|e| ResolveError::Io(format!("serializing keitora.toml: {e}")))
    }

    /// Write a freshly scaffolded `keitora.toml`, appending a commented `[dispatch]`
    /// example so the feature is discoverable. Used ONLY at project creation
    /// (CLI `config init`, app onboarding) — never the generic `write`, which
    /// must not reintroduce a comment a user deleted nor clobber user comments on
    /// an ordinary rewrite. The hint is suppressed once real steps exist (they
    /// serialize as a real `[dispatch]` table instead).
    pub fn write_initial(&self) -> Result<(), ResolveError> {
        let path = self.root.join("keitora.toml");
        let mut text = self.to_toml()?;
        if self.dispatch.is_empty() {
            text.push_str(DISPATCH_SCAFFOLD_HINT);
        }
        std::fs::write(&path, text)
            .map_err(|e| ResolveError::Io(format!("writing {}: {e}", path.display())))
    }
}

/// Strip the Windows `\\?\` verbatim (extended-length) prefix that
/// `std::fs::canonicalize` adds. That prefix is rejected by `git` (e.g.
/// `git worktree add`) and makes a canonicalized path compare unequal to the
/// otherwise-identical non-canonicalized join. UNC verbatim paths (`\\?\UNC\…`)
/// are left intact. No-op on Unix / paths without the prefix.
pub fn strip_extended_prefix(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        let stripped = strip_extended_prefix_str(&s);
        if stripped.len() != s.len() {
            return PathBuf::from(stripped);
        }
    }
    p
}

/// Convert a path to its Windows 8.3 short form (no spaces), so it can be used
/// **bare** as the program in a `cmd.exe /C <path> …` line — cmd mishandles an
/// inner-quoted path, so a path containing a space (e.g. under
/// `C:\Users\First Last\…`) would otherwise be split by cmd. Falls back to the
/// input if the path doesn't exist or 8.3 is disabled on the volume. Identity on non-Windows.
#[cfg(windows)]
pub fn short_path(path: &str) -> String {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetShortPathNameW;
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a valid null-terminated UTF-16 buffer for the duration of
    // both calls; the first sizes the output, the second fills it.
    let needed = unsafe { GetShortPathNameW(PCWSTR(wide.as_ptr()), None) };
    if needed == 0 {
        return path.to_string();
    }
    let mut buf = vec![0u16; needed as usize];
    let written = unsafe { GetShortPathNameW(PCWSTR(wide.as_ptr()), Some(&mut buf)) };
    if written == 0 || written as usize >= buf.len() {
        return path.to_string();
    }
    String::from_utf16_lossy(&buf[..written as usize])
}

/// Identity on non-Windows (8.3 short paths are a Windows concept).
#[cfg(not(windows))]
pub fn short_path(path: &str) -> String {
    path.to_string()
}

/// String form of [`strip_extended_prefix`] — the **canonical** place for the
/// `\\?\`-strip rule. Other crates (the session cwd, the GUI's hook-command
/// builder) call this rather than re-deriving the surgery, so the UNC carve-out can
/// never silently drift. No-op off Windows / on a path without the prefix.
pub fn strip_extended_prefix_str(s: &str) -> String {
    #[cfg(windows)]
    {
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            if !stripped.starts_with("UNC\\") {
                return stripped.to_string();
            }
        }
    }
    s.to_string()
}

/// Lexically normalize a path (resolve `..`/`.` segments) without touching the
/// filesystem. Used when a path is not yet on disk so `canonicalize` would fail.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Interpret `$KEITORA_PROJECT` (a path to a `keitora.toml` or a directory containing
/// one) and load it. Returns `Ok(None)` when the env var is unset, and an error
/// (never a fall-through) when set but unloadable.
fn resolve_env_project() -> Result<Option<ProjectConfig>, ResolveError> {
    let Ok(val) = std::env::var("KEITORA_PROJECT") else {
        return Ok(None);
    };
    if val.is_empty() {
        return Ok(None);
    }
    ProjectConfig::load(Path::new(&val)).map(Some)
}

/// Walk up from `start`, at most 8 levels, returning the first directory that
/// contains a `keitora.toml`.
fn cwd_walk(start: &Path) -> Option<ProjectConfig> {
    let mut cur = start;
    for _ in 0..8 {
        let candidate = cur.join("keitora.toml");
        if candidate.exists() {
            if let Ok(cfg) = ProjectConfig::load(&candidate) {
                return Some(cfg);
            }
        }
        match cur.parent() {
            Some(parent) => cur = parent,
            None => break,
        }
    }
    None
}

/// A comparison key for a filesystem path: canonicalized when possible (so
/// symlinks / trailing slashes don't cause false misses), falling back to the raw
/// path when canonicalization fails (e.g. the path no longer exists).
pub(crate) fn canonical_key(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Make a project `root` absolute and git-compatible: canonicalize (resolving
/// symlinks/`.`/`..`) and strip the Windows `\\?\` extended-length prefix that
/// `canonicalize` adds (`git` rejects it, and `worktrees_root`/`worktree_path`/
/// `db_path` + every `git -C <root>` call derive from this). No-op on Unix / paths
/// without the prefix. Shared by `load()` and `effective_config` so a scaffold-built
/// root equals a loaded one (canonical-key matching depends on it).
fn canonicalize_root(root: &Path) -> PathBuf {
    strip_extended_prefix(root.canonicalize().unwrap_or_else(|_| root.to_path_buf()))
}

/// Resolve a raw `db_path` against `root`: expand a leading `~`, resolve relative
/// paths against `root`, canonicalize when it already exists (else keep the joined
/// path so a fresh DB can still be created there), and strip the Windows
/// extended-length prefix. Shared by `load()` and `effective_config`/registry
/// overrides so a scaffold or registry `db_path` lands at the same absolute place
/// `load()` would produce.
fn normalize_db_path(raw: &Path, root: &Path) -> PathBuf {
    let expanded = expand_tilde(&raw.to_string_lossy());
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        root.join(expanded)
    };
    strip_extended_prefix(joined.canonicalize().unwrap_or(joined))
}

/// The effective worktree entries for a registered project: the per-machine
/// `ProjectRef.worktrees` wins when set (`Some`, even `Some([])`); otherwise the
/// committed `[tasks].worktrees` is the fallback. Carries name + path.
fn effective_worktrees(pref: &ProjectRef, pc: &ProjectConfig) -> Vec<WorktreeEntry> {
    pref.worktrees
        .clone()
        .unwrap_or_else(|| pc.tasks.worktrees.clone())
}

/// True when `cwd` is at or under `base`, comparing canonicalized keys so symlinks
/// / trailing slashes don't cause false misses (matches the write-path matching in
/// `set_project_worktrees_by_root`).
fn path_contains(base: &Path, cwd: &Path) -> bool {
    canonical_key(cwd).starts_with(canonical_key(base))
}

/// Stamp `cfg`'s effective worktree list from the registry when `cfg.root` matches
/// a registered project (canonical comparison). No-op otherwise — the `worktrees()`
/// accessor then falls back to the committed list. This is the single choke point
/// that keeps every resolution path (env, active, registry-match, cwd-walk) and the
/// direct-load call sites consistent.
pub fn stamp_from_registry(cfg: &mut ProjectConfig, registry: &UserConfig) {
    let key = canonical_key(&cfg.root);
    if let Some(pref) = registry
        .projects
        .values()
        .find(|p| canonical_key(&p.path) == key)
    {
        // Apply the FULL per-machine override set, not just worktrees — so
        // the resolution paths that load() + stamp (env `$KEITORA_PROJECT`, cwd-walk,
        // legacy migration) honor the same registry overrides `effective_config`
        // applies on the active-project / registry-match paths.
        apply_registry_overrides(cfg, pref);
        cfg.apply_registry_worktrees(pref);
    }
}

/// Build the effective [`ProjectConfig`] for a registered project: load its
/// committed `keitora.toml` when present, else start from [`ProjectConfig::scaffold`];
/// then overlay each `Some(_)` per-machine override from the registry entry; then
/// stamp the effective worktree list. This is the single boundary that makes a
/// `keitora.toml` optional — every registry resolution path goes through it instead of
/// [`ProjectConfig::load`] directly.
pub fn effective_config(pref: &ProjectRef) -> Result<ProjectConfig, ResolveError> {
    let mut cfg = if pref.path.join("keitora.toml").exists() {
        ProjectConfig::load(&pref.path)?
    } else {
        // File-less project: defaults from scaffold, normalized exactly as load()
        // would (so the root + db_path match a loaded project's absolute forms).
        let mut c = ProjectConfig::scaffold(&pref.path);
        c.root = canonicalize_root(&pref.path);
        c.tasks.db_path = normalize_db_path(&c.tasks.db_path, &c.root);
        c
    };
    apply_registry_overrides(&mut cfg, pref);
    cfg.effective_worktrees = Some(effective_worktrees(pref, &cfg));
    Ok(cfg)
}

/// Overlay the registry entry's `Some(_)` setting overrides onto `cfg`. Deliberately
/// does NOT touch worktrees — [`effective_worktrees`] owns that (preserving the
/// `Some([])` "explicitly none" semantics).
fn apply_registry_overrides(cfg: &mut ProjectConfig, pref: &ProjectRef) {
    if let Some(db) = &pref.db_path {
        cfg.tasks.db_path = normalize_db_path(db, &cfg.root);
    }
    if let Some(dir) = &pref.worktrees_dir {
        cfg.project.worktrees_dir = Some(dir.clone());
    }
    if let Some(d) = pref.archive_days {
        cfg.tasks.archive_days = d;
    }
    if let Some(d) = pref.log_retention_days {
        cfg.tasks.log_retention_days = d;
    }
    if let Some(steps) = &pref.dispatch_steps {
        cfg.dispatch.claude_steps = steps.clone();
    }
    if let Some(steps) = &pref.codex_dispatch_steps {
        cfg.dispatch.codex_steps = steps.clone();
    }
    if let Some(b) = &pref.diff_base {
        cfg.project.diff_base = Some(b.clone());
    }
}

/// Find a registered project whose root (or one of its worktrees) contains `cwd`.
fn registry_match(registry: &UserConfig, cwd: &Path) -> Option<ProjectConfig> {
    for pref in registry.projects.values() {
        // Build the effective config once per entry; a broken/unreadable committed
        // file is skipped, not fatal. A file-less entry resolves via scaffold
        // defaults (no longer skipped), so cwd-walk can match it too.
        let Ok(cfg) = effective_config(pref) else {
            continue;
        };
        // Direct: cwd is inside the project root.
        if path_contains(&pref.path, cwd) {
            return Some(cfg);
        }
        // Worktrees: cwd is inside one of the project's linked worktrees.
        // Resolve each name through `worktree_path`, which prefers the entry's
        // STORED path and only falls back to the `worktrees_dir`
        // reconstruction for legacy/unmigrated entries — so a scattered worktree
        // whose real path is persisted is matched even when it lives nowhere near
        // `worktrees_dir`.
        if cfg
            .worktrees()
            .iter()
            .any(|wt| path_contains(&cfg.worktree_path(wt), cwd))
        {
            return Some(cfg);
        }
    }
    None
}

/// The directory to start a cwd walk from: the git toplevel, else `current_dir`.
fn cwd_start() -> PathBuf {
    git_toplevel().unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// CLI entry point: registry-first resolution.
///
/// Order: `$KEITORA_PROJECT` → registry-match → cwd-walk → `NotFound`.
pub fn resolve() -> Result<ProjectConfig, ResolveError> {
    resolve_in(&cwd_start())
}

/// Registry-first resolution starting from an explicit dir. Takes `cwd` as a
/// parameter (instead of reading the process cwd) so tests are hermetic and
/// never mutate process-global state.
///
/// The registry is consulted *before* the cwd-walk so that when `cwd` is inside
/// a registered project's linked worktree, the config is rooted at the project's
/// *main* checkout — where the committed relative `db_path`/`worktrees_dir`
/// resolve correctly. A bare cwd-walk would instead root the config at the
/// worktree and resolve those relative paths against it, doubling the path (e.g.
/// `…/keitora-worktrees/keitora-worktrees/shared/tasks.db`). The cwd-walk remains as a
/// fallback for projects that are present on disk but not yet registered.
fn resolve_in(cwd: &Path) -> Result<ProjectConfig, ResolveError> {
    let registry = UserConfig::load()?;

    if let Some(mut cfg) = resolve_env_project()? {
        stamp_from_registry(&mut cfg, &registry);
        return Ok(cfg);
    }

    if let Some(cfg) = registry_match(&registry, cwd) {
        return Ok(cfg); // already stamped inside registry_match
    }

    if let Some(mut cfg) = cwd_walk(cwd) {
        stamp_from_registry(&mut cfg, &registry);
        return Ok(cfg);
    }

    // Legacy fallback (one-release window): no keitora.toml found anywhere, but a
    // legacy task-config.json exists → migrate it to a committed keitora.toml,
    // emit the deprecation warning, and return the new config. Idempotent: a
    // pre-existing keitora.toml short-circuits inside the migration.
    if let Some(legacy_path) = crate::migration::find_legacy_config_from(cwd) {
        if legacy_path.exists() {
            return crate::migration::migrate_legacy(&legacy_path)
                .map(|(mut cfg, _wrote)| {
                    stamp_from_registry(&mut cfg, &registry);
                    cfg
                })
                .map_err(|e| ResolveError::Io(e.to_string()));
        }
    }

    Err(ResolveError::NotFound)
}

/// GUI entry point: registry-first resolution.
///
/// Order: `$KEITORA_PROJECT` → active project → registry-match → `NeedsOnboarding`.
/// Does not do a cwd-walk — the GUI has no meaningful cwd.
pub fn resolve_active() -> Result<ProjectConfig, ResolveError> {
    resolve_active_in(cwd_start)
}

/// Registry-first resolution. `cwd` is a thunk so the (subprocess-spawning)
/// `git_toplevel()` is only paid on the `registry_match` fallback — not on the
/// common GUI path where an active project is set. Also the hermetic test seam
/// (tests pass a temp dir).
fn resolve_active_in(cwd: impl FnOnce() -> PathBuf) -> Result<ProjectConfig, ResolveError> {
    let registry = UserConfig::load()?;

    if let Some(mut cfg) = resolve_env_project()? {
        stamp_from_registry(&mut cfg, &registry);
        return Ok(cfg);
    }

    if let Some((_name, pref)) = registry.active_project() {
        return effective_config(pref);
    }

    if let Some(cfg) = registry_match(&registry, &cwd()) {
        return Ok(cfg);
    }

    Err(ResolveError::NeedsOnboarding)
}

// ---------------------------------------------------------------------------
// CLI binary resolution + discovery
// ---------------------------------------------------------------------------

/// Select the per-CLI [`ProviderConfig`] on a [`ProjectConfig`] by [`CliKind`].
fn project_provider(project: &ProjectConfig, cli: CliKind) -> &ProviderConfig {
    match cli {
        CliKind::Claude => &project.claude,
        CliKind::Codex => &project.codex,
    }
}

/// Select the per-CLI [`ProviderConfig`] on a [`UserConfig`] by [`CliKind`].
fn user_provider(user: &UserConfig, cli: CliKind) -> &ProviderConfig {
    match cli {
        CliKind::Claude => &user.claude,
        CliKind::Codex => &user.codex,
    }
}

/// The effective CLI for a worktree: the worktree entry's explicit `cli`
/// wins, then the user-level `default_cli`, then the hardcoded [`CliKind::Claude`].
/// Takes no `project` argument — there is no project-level default in this design.
pub fn resolve_cli(worktree: Option<&WorktreeEntry>, user: &UserConfig) -> CliKind {
    worktree
        .and_then(|w| w.cli)
        .or(user.default_cli)
        .unwrap_or(CliKind::Claude)
}

/// The effective executable path for `cli`: a project override wins over
/// the user-level default. `None` means nothing is configured (the caller should
/// discover via [`discover_cli`]). Generalizes the legacy `resolve_claude_path`.
pub fn resolve_provider_path(
    cli: CliKind,
    project: Option<&ProjectConfig>,
    user: &UserConfig,
) -> Option<String> {
    project
        .and_then(|p| project_provider(p, cli).path.clone())
        .or_else(|| user_provider(user, cli).path.clone())
}

/// The effective autonomy flag for `cli`: a project `autonomy` override
/// (either `true` or an explicit `false`) wins over the user-level default. Unset
/// at both levels resolves to `false`. Generalizes `resolve_skip_permissions`.
pub fn resolve_autonomy(cli: CliKind, project: Option<&ProjectConfig>, user: &UserConfig) -> bool {
    project
        .and_then(|p| project_provider(p, cli).autonomy)
        .or(user_provider(user, cli).autonomy)
        .unwrap_or(false)
}

/// The effective `claude` path: a project override wins over the user-level
/// default. `None` means nothing is configured (the caller should discover).
/// Thin wrapper over [`resolve_provider_path`] for `CliKind::Claude`.
pub fn resolve_claude_path(project: Option<&ProjectConfig>, user: &UserConfig) -> Option<String> {
    resolve_provider_path(CliKind::Claude, project, user)
}

/// The effective `--dangerously-skip-permissions` flag: a project
/// `[claude] skip_permissions`/`autonomy` override (either `true` or an explicit
/// `false`) wins over the user-level default. Unset at both levels resolves to
/// `false`. Thin wrapper over [`resolve_autonomy`] for `CliKind::Claude`.
pub fn resolve_skip_permissions(project: Option<&ProjectConfig>, user: &UserConfig) -> bool {
    resolve_autonomy(CliKind::Claude, project, user)
}

/// The configured override path for the `keitora` CLI, if any: a project `[cli].path`
/// wins over the user-level default. `~` is expanded. `None` means no override is
/// configured — the caller should fall back to a local build and then [`keitora_on_path`].
/// Override-only by design, mirroring [`resolve_claude_path`] (PATH discovery is
/// [`keitora_on_path`], the analogue of [`discover_claude`]).
pub fn resolve_keitora_path(project: Option<&ProjectConfig>, user: &UserConfig) -> Option<String> {
    project
        .and_then(|p| p.cli.path.clone())
        .or_else(|| user.cli.path.clone())
        .map(|s| expand_tilde(&s).to_string_lossy().into_owned())
}

// Sentinels bracketing the captured `$PATH` so a shell's interactive greeting
// (notably fish) cannot pollute the value we parse back out.
const PATH_BEGIN: &str = "__KEITORA_PATH_BEGIN__";
const PATH_END: &str = "__KEITORA_PATH_END__";

/// Capture the user's real login `$PATH` by running their login+interactive shell
/// once. A GUI process launched via LaunchServices inherits only a minimal
/// `PATH`; the login shell sources the user's profile (homebrew, `~/.local/bin`,
/// fnm, …). Shell-agnostic: `printenv` reads the exported, colon-joined `PATH`
/// regardless of the shell's own list semantics (e.g. fish).
/// Extension to suppress the console window Windows allocates when a GUI process
/// (which has no console of its own) spawns a console-subsystem child — `git`, the
/// login shell, `claude`, etc. Without it a window flashes on every such call, and
/// the UI's worktree/session-status polling fires them rapidly (looks like a fork
/// bomb). Sets `CREATE_NO_WINDOW`; no-op off Windows. Apply just before
/// `output()`/`spawn()`. Implemented for both `std` and `tokio` commands.
pub(crate) trait CommandNoWindow {
    fn no_window(&mut self) -> &mut Self;
}

impl CommandNoWindow for Command {
    fn no_window(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

impl CommandNoWindow for tokio::process::Command {
    fn no_window(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            // Inherent method on tokio's Command (no CommandExt import).
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

pub fn login_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let script = format!("printf %s {PATH_BEGIN}; printenv PATH; printf %s {PATH_END}");
    let out = Command::new(&shell)
        .args(["-l", "-i", "-c", &script])
        .no_window()
        .output()
        .ok()?;
    parse_marked_path(&String::from_utf8_lossy(&out.stdout))
}

/// Process-wide cache of [`login_path`]. The login-shell probe is relatively
/// expensive, so resolve it once per process.
pub fn cached_login_path() -> Option<String> {
    static LOGIN_PATH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    LOGIN_PATH.get_or_init(login_path).clone()
}

/// Order-preserving, deduplicating PATH join: `extra` dirs first, then the entries
/// of `current`, dropping duplicates. Pure — unit-tested without touching the env.
/// Unix-only (the `:` separator and the extra dirs are POSIX-specific).
///
/// Named `*_dirs` to avoid colliding with the `#[cfg(windows)] pub fn prepend_path`
/// (different signature, re-exported from `lib.rs`).
#[cfg(unix)]
pub(crate) fn prepend_path_dirs(extra: &[String], current: Option<&str>) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut parts: Vec<String> = Vec::new();
    let inherited = current
        .unwrap_or("")
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    for d in extra.iter().cloned().chain(inherited) {
        if seen.insert(d.clone()) {
            parts.push(d);
        }
    }
    parts.join(":")
}

/// Common dirs a Homebrew / manually-installed tool lives in that a Finder- or
/// Dock-launched macOS app's minimal PATH omits. Read from the env at call time.
/// Shared by [`git_command`] and `gh_command`.
#[cfg(unix)]
pub(crate) fn extra_path_dirs() -> Vec<String> {
    let mut dirs = vec![
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(format!("{}/.local/bin", home.to_string_lossy()));
    }
    dirs
}

/// Pure core of [`unix_spawn_path`]: the dirs of `login` (in the user's own order,
/// so their version-manager shims keep winning), then the `extra` dirs, then
/// `inherited`, without duplicates. `None` when all three are empty.
#[cfg(unix)]
fn compose_spawn_path(
    extra: &[String],
    login: Option<&str>,
    inherited: Option<&str>,
) -> Option<String> {
    let mut dirs: Vec<String> = login
        .unwrap_or("")
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    dirs.extend(extra.iter().cloned());
    let path = prepend_path_dirs(&dirs, inherited);
    (!path.is_empty()).then_some(path)
}

/// The `PATH` herdr and the panes it starts should run with on unix: the cached
/// login shell `PATH`, then the shared extra dirs (Homebrew, `/usr/local/bin`,
/// `~/.local/bin`), then the process `PATH`, de-duplicated. A Dock-launched app
/// has only a minimal `PATH`, which would leave `gh`, `claude` and friends
/// unfindable. Runs the login shell the first time it is needed in the process
/// (see [`cached_login_path`]), so never call it on the app's setup thread.
#[cfg(unix)]
pub fn unix_spawn_path() -> Option<String> {
    let inherited = std::env::var_os("PATH");
    let inherited = inherited.as_ref().map(|s| s.to_string_lossy());
    compose_spawn_path(
        &extra_path_dirs(),
        cached_login_path().as_deref(),
        inherited.as_deref(),
    )
}

/// The PATH value a spawned `git` should run with, as a pure function for testing.
/// Always *prepends* to the inherited PATH rather than replacing it, so whatever
/// PATH already located `git` keeps locating it. (Rust resolves the program name
/// against the child's PATH at `execvp`; replacing it with a login PATH that omits
/// git's own dir would make `git` itself unfindable. `gh_command` prepends for the
/// same reason.)
/// - login present → prepend its dirs (where `git-lfs` lives) ahead of the inherited PATH.
/// - login absent → prepend the known-missing dirs to the inherited PATH.
/// - neither → `None` (set nothing; inherit unchanged).
#[cfg(unix)]
pub(crate) fn git_path_env(login: Option<String>, inherited: Option<&str>) -> Option<String> {
    match login {
        Some(p) => {
            let dirs: Vec<String> = p
                .split(':')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            Some(prepend_path_dirs(&dirs, inherited))
        }
        None => inherited.map(|cur| prepend_path_dirs(&extra_path_dirs(), Some(cur))),
    }
}

/// A `git` [`Command`] with `.no_window()` applied and (on unix) an augmented PATH so
/// the LFS filter-process resolves `git-lfs` even when Keitora is launched from the
/// Dock with a minimal PATH that omits Homebrew. Mirrors `gh_command`/`claude`.
///
/// Unix-only augmentation: on non-unix we inherit the already-correct PATH unchanged
/// (a Windows git/git-lfs installer puts itself on PATH, and the `:`/dir conventions
/// here are POSIX-specific).
pub(crate) fn git_command() -> Command {
    let mut cmd = Command::new("git");
    cmd.no_window();
    #[cfg(unix)]
    {
        // `var_os` + lossy so a non-UTF-8 PATH still augments rather than being dropped.
        let inherited = std::env::var_os("PATH");
        let inherited = inherited.as_ref().map(|s| s.to_string_lossy());
        if let Some(p) = git_path_env(cached_login_path(), inherited.as_deref()) {
            cmd.env("PATH", p);
        }
    }
    cmd
}

/// Extract the value the shell printed between [`PATH_BEGIN`]/[`PATH_END`],
/// trimming surrounding whitespace/newlines. Pure — unit-tested.
fn parse_marked_path(s: &str) -> Option<String> {
    let start = s.find(PATH_BEGIN)? + PATH_BEGIN.len();
    let rest = &s[start..];
    let end = rest.find(PATH_END)?;
    let path = rest[..end].trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// True if `p` is an existing regular file with any execute bit set (a directory
/// of that name is not).
#[cfg(unix)]
pub fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

/// True if `p` is an existing regular file. Windows has no execute bit; PATHEXT /
/// extension-based runnability resolution is a stage-050 refinement.
#[cfg(windows)]
pub fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// First existing `dir\name` across a `;`-separated Windows `PATH`, trying each
/// `name` (PATHEXT-style: e.g. `foo.exe`, `foo.cmd`, `foo.bat`) within a dir before
/// moving on. Pure (takes the PATH string explicitly) so it is unit-testable
/// without touching the process environment. Shared by [`keitora_on_path`] and
/// [`discover_claude`] so their search semantics can't drift.
#[cfg(windows)]
fn find_on_path_windows(path: &str, names: &[&str]) -> Option<PathBuf> {
    for dir in path.split(';').filter(|d| !d.is_empty()) {
        for name in names {
            let cand = Path::new(dir).join(name);
            if is_executable_file(&cand) {
                return Some(cand);
            }
        }
    }
    None
}

/// First executable named `name` in a colon-separated `path` list. `which`-style.
/// Unix-only (`:`-separated PATH); the Windows PATH search (`;` + PATHEXT) is
/// inlined in [`discover_claude`] / [`keitora_on_path`].
#[cfg(unix)]
fn find_in_path(path: &str, name: &str) -> Option<PathBuf> {
    path.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|cand| is_executable_file(cand))
}

/// The leading `major.minor.patch` of a herdr release dir name (an optional `v`
/// prefix is ignored; missing or non-numeric parts read as `None`). Pure.
fn parse_release_version(name: &str) -> Option<(u64, u64, u64)> {
    let name = name.strip_prefix('v').unwrap_or(name);
    let mut parts = name.split('.');
    let mut next = |last: bool| -> Option<u64> {
        let part = parts.next()?;
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || (!last && digits.len() != part.len()) {
            return None;
        }
        digits.parse().ok()
    };
    let major = next(false)?;
    let minor = next(false)?;
    let patch = next(true)?;
    Some((major, minor, patch))
}

/// The herdr executable's file name.
#[cfg(unix)]
const HERDR_EXE_NAME: &str = "herdr";
#[cfg(windows)]
const HERDR_EXE_NAME: &str = "herdr.exe";

/// The first herdr executable on `path` (a `PATH`-style list in the platform's
/// own syntax): `herdr` on unix, `herdr.exe` on Windows.
pub fn herdr_on_path(path: &str) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        find_in_path(path, HERDR_EXE_NAME)
    }
    #[cfg(windows)]
    {
        find_on_path_windows(path, &[HERDR_EXE_NAME])
    }
}

/// `<release>/exe_name` for each of `release_dirs`, newest version first (numeric
/// compare of the leading `major.minor.patch`; unparsable names last). Pure.
fn herdr_release_order(release_dirs: &[PathBuf], exe_name: &str) -> Vec<PathBuf> {
    let mut releases: Vec<&PathBuf> = release_dirs.iter().collect();
    let version = |p: &PathBuf| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(parse_release_version)
    };
    // Newest first; unparsable (`None`) sorts after every parsed version.
    releases.sort_by(|a, b| version(b).cmp(&version(a)).then_with(|| b.cmp(a)));
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in releases {
        let p = dir.join(exe_name);
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Where the standalone herdr installer unpacks releases:
/// `~/.herdr/packages/standalone/releases`.
fn herdr_releases_dir() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".herdr")
            .join("packages")
            .join("standalone")
            .join("releases"),
    )
}

/// The release dirs (`releases/<version>`) that exist on disk.
fn herdr_release_dirs() -> Vec<PathBuf> {
    let Some(root) = herdr_releases_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

/// Where the herdr executable of each standalone release would be, newest
/// version first, whether or not a file is there. Reads the releases directory
/// once.
pub fn herdr_release_paths() -> Vec<PathBuf> {
    herdr_release_order(&herdr_release_dirs(), HERDR_EXE_NAME)
}

/// The usual install locations a GUI launch's minimal `PATH` misses: `herdr` in
/// each shared extra dir (Homebrew, `/usr/local/bin`, `~/.local/bin`) on unix.
/// Windows has no such fixed locations.
pub fn herdr_well_known_paths() -> Vec<PathBuf> {
    #[cfg(unix)]
    {
        extra_path_dirs()
            .iter()
            .map(|d| Path::new(d).join(HERDR_EXE_NAME))
            .collect()
    }
    #[cfg(windows)]
    {
        Vec::new()
    }
}

/// Absolute path to a `keitora` binary on the user's login `$PATH`, if any. The
/// PATH-discovery analogue of [`resolve_keitora_path`]'s override; used by the
/// session-hook installer as the friendly default when no local build / override
/// applies. Mirrors [`discover_claude`] but without the well-known-location
/// fallbacks (a CLI the user installed is expected to be on PATH).
#[cfg(unix)]
pub fn keitora_on_path() -> Option<String> {
    let path = login_path()?;
    find_in_path(&path, "keitora").map(|p| p.to_string_lossy().into_owned())
}

/// Windows: search the process `PATH` (`;`-separated) for `keitora.exe`/`.cmd`/`.bat`
/// so the NSIS-installed CLI (placed on PATH by the installer) resolves for session
/// hooks. There is no login-shell PATH concept; the process PATH is authoritative.
#[cfg(windows)]
pub fn keitora_on_path() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    find_on_path_windows(
        &path.to_string_lossy(),
        &["keitora.exe", "keitora.cmd", "keitora.bat"],
    )
    .map(|p| p.to_string_lossy().into_owned())
}

/// Auto-discover the absolute path to `claude`: search the login `$PATH`, then a
/// set of well-known install locations. `None` if not found anywhere.
#[cfg(unix)]
pub fn discover_claude() -> Option<String> {
    if let Some(path) = login_path() {
        if let Some(found) = find_in_path(&path, "claude") {
            return Some(found.to_string_lossy().into_owned());
        }
    }
    // Fallbacks for when the login shell didn't expose it (unusual configs).
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude"),
    ];
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin/claude"));
        candidates.push(home.join(".claude/local/claude"));
    }
    candidates
        .into_iter()
        .find(|p| is_executable_file(p))
        .map(|p| p.to_string_lossy().into_owned())
}

/// Windows `claude` discovery (§4.8). npm's global install drops a `claude.cmd`
/// shim under `%APPDATA%\npm` (or `%LOCALAPPDATA%\npm`); fall back to a PATHEXT
/// search of the (`;`-separated) `PATH`. GUI-launched PATH is unreliable on
/// Windows just as on macOS, so the well-known npm locations are tried
/// first. `None` if not found anywhere (callers then run the bare `claude`, which
/// `cmd.exe /C` resolves via PATHEXT if it is on PATH).
#[cfg(windows)]
pub fn discover_claude() -> Option<String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    // Native installer (the `~/.local/bin` / `~/.claude/local` locations, mirroring
    // the unix fallbacks) — checked first because a GUI-launched PATH is unreliable
    // on Windows and often omits these even when the user's shell has them.
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local").join("bin").join("claude.exe"));
        candidates.push(home.join(".claude").join("local").join("claude.exe"));
    }
    // npm global shim.
    for var in ["APPDATA", "LOCALAPPDATA"] {
        if let Some(dir) = std::env::var_os(var) {
            candidates.push(PathBuf::from(dir).join("npm").join("claude.cmd"));
        }
    }
    if let Some(found) = candidates.iter().find(|p| is_executable_file(p)) {
        return Some(found.to_string_lossy().into_owned());
    }
    // PATHEXT-style search of the process PATH (`;`-separated on Windows).
    let path = std::env::var_os("PATH")?;
    find_on_path_windows(
        &path.to_string_lossy(),
        &["claude.cmd", "claude.exe", "claude.bat"],
    )
    .map(|p| p.to_string_lossy().into_owned())
}

/// Auto-discover the absolute path to the `cli` executable. `Claude`
/// delegates to [`discover_claude`] unchanged; `Codex` mirrors the same two-tier
/// strategy (login-`$PATH` search, then well-known install locations). `None` if
/// not found anywhere.
#[cfg(unix)]
pub fn discover_cli(cli: CliKind) -> Option<String> {
    match cli {
        CliKind::Claude => discover_claude(),
        CliKind::Codex => {
            if let Some(path) = login_path() {
                if let Some(found) = find_in_path(&path, "codex") {
                    return Some(found.to_string_lossy().into_owned());
                }
            }
            // Only the `~/.local/bin` fallback generalizes directly; the Homebrew
            // locations are Claude-installer-specific and are NOT carried over.
            let home = dirs::home_dir()?;
            let cand = home.join(".local/bin/codex");
            is_executable_file(&cand).then(|| cand.to_string_lossy().into_owned())
        }
    }
}

/// Windows `codex` discovery, in priority order: native-installer
/// `~/.local/bin/codex.exe`; npm global shim `%APPDATA%`/`%LOCALAPPDATA%\npm\codex.cmd`;
/// the nvm-for-windows shim (`/c/nvm4w/nodejs/codex` → `C:\nvm4w\nodejs\codex.cmd`);
/// then a PATHEXT-style search of the process `PATH`.
#[cfg(windows)]
pub fn discover_cli(cli: CliKind) -> Option<String> {
    match cli {
        CliKind::Claude => discover_claude(),
        CliKind::Codex => {
            let mut candidates: Vec<PathBuf> = Vec::new();
            // Native installer.
            if let Some(home) = dirs::home_dir() {
                candidates.push(home.join(".local").join("bin").join("codex.exe"));
            }
            // npm global shim.
            for var in ["APPDATA", "LOCALAPPDATA"] {
                if let Some(dir) = std::env::var_os(var) {
                    candidates.push(PathBuf::from(dir).join("npm").join("codex.cmd"));
                }
            }
            // nvm-for-windows shim — the `/c/nvm4w/nodejs/codex` bash path in native
            // Windows form (probes expect native paths, not the `/c/...` form).
            candidates.push(PathBuf::from(r"C:\nvm4w\nodejs\codex.cmd"));
            if let Some(found) = candidates.iter().find(|p| is_executable_file(p)) {
                return Some(found.to_string_lossy().into_owned());
            }
            // PATHEXT-style search of the process PATH (`;`-separated on Windows).
            let path = std::env::var_os("PATH")?;
            find_on_path_windows(
                &path.to_string_lossy(),
                &["codex.cmd", "codex.exe", "codex.bat"],
            )
            .map(|p| p.to_string_lossy().into_owned())
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: expose the bundled `keitora` CLI on spawned terminals' PATH
// ---------------------------------------------------------------------------

/// Windows: pick the bundled `keitora` CLI sibling to mirror onto PATH for terminals the
/// herdr server spawns. Prefers the packaged sidecar `keitora-cli.exe` (unambiguous); otherwise a
/// dev-build `keitora.exe` — but ONLY when a sibling `keitora-app.exe` is also present. That
/// guard mirrors `cli_install::locate_installable`: on a pre-rename branch `keitora.exe` is
/// the GUI app, not the CLI, and must never be exposed as `keitora`. Pure (takes the dir).
///
/// This is the **canonical Windows CLI-resolver**. The GUI installer's
/// `cli_install::{sidecar_candidate, locate_installable}` join extension-less names
/// (`"keitora"`, `"keitora-app"`) and so only match on Unix; their Windows install path is
/// deferred. When that lands, route its Windows resolution through
/// this function rather than re-deriving the `keitora-app` rename guard a third time.
#[cfg(windows)]
fn pick_cli_source(dir: &Path) -> Option<PathBuf> {
    let bundled = dir.join("keitora-cli.exe");
    if bundled.is_file() {
        return Some(bundled);
    }
    let dev = dir.join("keitora.exe");
    if dev.is_file() && dir.join("keitora-app.exe").is_file() {
        return Some(dev);
    }
    None
}

/// Windows: true if the per-user `keitora.exe` alias at `dst` is missing or stale relative to
/// `src` (differs in length or modified-time). Stat-only — no hashing on the spawn path.
/// Total: if `dst` (or `src`) metadata is unreadable, err toward refreshing rather than
/// serving a possibly-bad copy.
#[cfg(windows)]
fn alias_needs_refresh(src: &Path, dst: &Path) -> bool {
    let (Ok(s), Ok(d)) = (std::fs::metadata(src), std::fs::metadata(dst)) else {
        return true;
    };
    if s.len() != d.len() {
        return true;
    }
    match (s.modified(), d.modified()) {
        (Ok(sm), Ok(dm)) => sm != dm,
        // modified-time unavailable on this fs → lengths matched, treat as fresh.
        _ => false,
    }
}

/// Windows: build a child `PATH` with `dir` as the leading (`;`-separated) segment.
/// Idempotent — if `dir` is already the first segment of `existing`, returns it unchanged
/// so repeated spawns in one process don't accumulate duplicate entries. Pure.
#[cfg(windows)]
pub fn prepend_path(dir: &Path, existing: Option<&str>) -> String {
    let dir_s = dir.to_string_lossy();
    match existing {
        None | Some("") => dir_s.into_owned(),
        Some(p) => {
            let first = p.split(';').next().unwrap_or("");
            if !first.is_empty() && Path::new(first) == dir {
                p.to_string()
            } else {
                format!("{dir_s};{p}")
            }
        }
    }
}

/// Windows: ensure the bundled `keitora` CLI is invokable as a bare `keitora` inside terminals
/// the daemon spawns. Mirrors the sibling CLI (`keitora-cli.exe`, or a guarded dev `keitora.exe`)
/// to `%LOCALAPPDATA%\keitora\bin\keitora.exe`, refreshing only when stale, and returns that
/// `bin` directory to prepend to the child PATH. Memoized for the process lifetime (the
/// sidecar is byte-stable while the app runs), mirroring [`cached_login_path`].
/// Best-effort: returns `None` on any unrecoverable failure, and the caller then leaves
/// PATH untouched (no regression).
#[cfg(windows)]
pub fn windows_keitora_bin_dir() -> Option<PathBuf> {
    static BIN_DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    BIN_DIR.get_or_init(ensure_windows_keitora_alias).clone()
}

/// Ensure `dst` mirrors `src` inside `bin_dir`, creating the dir and refreshing only when
/// stale. `Ok(())` means `dst` is usable afterward — either freshly copied, already fresh,
/// or kept after a *recoverable* failure (a present-but-unreplaceable `dst`); `Err` means
/// there is no usable `dst`. Pure of env/global state (takes concrete paths) so the
/// copy/refresh/fallback branches are unit-testable. Split out of [`ensure_windows_keitora_alias`].
#[cfg(windows)]
fn refresh_alias(src: &Path, bin_dir: &Path, dst: &Path) -> std::io::Result<()> {
    if let Err(e) = std::fs::create_dir_all(bin_dir) {
        // A pre-existing `dst` is still usable even if we can't (re)create the dir.
        return if dst.is_file() { Ok(()) } else { Err(e) };
    }
    if !alias_needs_refresh(src, dst) {
        return Ok(());
    }
    // Copy to a temp sibling then rename over `dst`, so a concurrent reader never sees a
    // half-written exe and a locked `dst` (a running `keitora.exe`) can't be corrupted.
    let tmp = bin_dir.join("keitora.exe.new");
    match std::fs::copy(src, &tmp).and_then(|_| std::fs::rename(&tmp, dst)) {
        Ok(_) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            // Best-effort fallback: if a `dst` already exists we keep it
            // rather than fail. Note this means an app UPGRADE while a `keitora.exe` is running
            // (rename blocked) leaves the OLD copy in place until the next app restart
            // refreshes it — acceptable for a CLI shim.
            if dst.is_file() {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// The uncached body of [`windows_keitora_bin_dir`] — resolves the env-derived paths, then
/// delegates the file work to [`refresh_alias`].
#[cfg(windows)]
fn ensure_windows_keitora_alias() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let src = pick_cli_source(exe.parent()?)?;
    let bin_dir = dirs::data_local_dir()?.join("keitora").join("bin");
    let dst = bin_dir.join("keitora.exe");
    match refresh_alias(&src, &bin_dir, &dst) {
        Ok(()) => Some(bin_dir),
        Err(e) => {
            log::warn!("keitora CLI install failed: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Back-compat shim — the legacy `Config` public API
// ---------------------------------------------------------------------------

/// Back-compat view over a [`ProjectConfig`], preserving the old `Config` API so
/// existing consumers (CLI, Tauri) keep compiling unchanged.
#[derive(Debug, Clone)]
pub struct Config {
    pub db_path: String,
    pub archive_days: i64,
    pub log_retention_days: i64,
    pub worktrees: Vec<String>,
    /// The underlying project config, used by the path accessors.
    project: ProjectConfig,
}

impl Config {
    /// Build a legacy `Config` view from a resolved `ProjectConfig`.
    pub fn from_project(pc: ProjectConfig) -> Config {
        Config {
            db_path: pc.tasks.db_path.to_string_lossy().into_owned(),
            archive_days: pc.tasks.archive_days,
            log_retention_days: pc.tasks.log_retention_days,
            worktrees: pc.worktrees(),
            project: pc,
        }
    }

    /// Find and load the active project's config (cwd-first resolution),
    /// mapping resolution failures onto the crate's `Error::Config`.
    pub fn find() -> crate::Result<Config> {
        resolve()
            .map(Config::from_project)
            .map_err(|e| Error::Config(e.to_string()))
    }

    /// GUI sibling of [`Config::find`]: resolve the **active** project (registry
    /// `active_project`-first via [`resolve_active`]) rather than the cwd-based
    /// [`resolve`]. Used by the GUI commands that activate a project so that
    /// session-hook install targets the project the user selected, independent of
    /// the process cwd (which is fixed for a GUI launch and ignores the switch).
    pub fn find_active() -> crate::Result<Config> {
        resolve_active()
            .map(Config::from_project)
            .map_err(|e| Error::Config(e.to_string()))
    }

    /// The underlying resolved [`ProjectConfig`]. Lets callers derive
    /// project-scoped values (e.g. the `[cli].path` override) that match the
    /// project this `Config` was resolved from.
    pub fn project_config(&self) -> &ProjectConfig {
        &self.project
    }

    /// The backend kind from `[tasks] backend`. Returns `"sqlite"` when the
    /// field is absent (the serde default), which is always the case for existing
    /// `keitora.toml` files that have not been explicitly set.
    pub fn backend(&self) -> String {
        self.project.tasks.backend.clone()
    }

    /// The directory that contains the worktrees. Returns `Result` for API
    /// compatibility with the previous shim (this no longer fails in practice).
    pub fn worktrees_root(&self) -> crate::Result<PathBuf> {
        Ok(self.project.worktrees_root())
    }

    /// Resolve a worktree name to its directory. The main worktree (the project
    /// root's basename) maps to the project root; others live under the
    /// worktrees root.
    pub fn worktree_path(&self, name: &str) -> crate::Result<PathBuf> {
        Ok(self.project.worktree_path(name))
    }

    /// The main worktree's path — the project root, where the committed
    /// `keitora.toml` and the built `task` binary live. Replaces the old
    /// `worktree_path(MAIN_WORKTREE)` call now that the constant is gone.
    pub fn main_worktree_path(&self) -> PathBuf {
        self.project.root.clone()
    }

    /// Resolve + classify all configured worktrees, de-duplicating names
    /// (keeping the first occurrence).
    pub fn resolved_worktrees(&self) -> crate::Result<Vec<ResolvedWorktree>> {
        // Load the user config once for the `default_cli` fallback.
        self.resolved_worktrees_with(&UserConfig::load().unwrap_or_default())
    }

    /// [`Config::resolved_worktrees`] with an already-loaded user config for the
    /// `default_cli` fallback, for callers resolving many projects in one pass.
    pub fn resolved_worktrees_with(
        &self,
        user: &UserConfig,
    ) -> crate::Result<Vec<ResolvedWorktree>> {
        // Iterate the full `WorktreeEntry` list (not just names) so each entry's
        // registry-only `cli` is available for `resolve_cli`.
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for entry in self.project.worktree_entries() {
            if !seen.insert(entry.name.clone()) {
                continue; // duplicate name — keep first
            }
            let path = self.worktree_path(&entry.name)?;
            let status = classify_worktree(&path);
            let cli = resolve_cli(Some(&entry), user);
            out.push(ResolvedWorktree {
                name: entry.name,
                path,
                status,
                cli,
            });
        }
        Ok(out)
    }
}

/// Classify a worktree directory: exists? → is a dir? → is a git repo/worktree?
/// (Both a normal repo's `.git` dir and a linked worktree's `.git` file count.)
pub fn classify_worktree(path: &Path) -> WorktreeStatus {
    if !path.exists() {
        WorktreeStatus::Missing
    } else if !path.is_dir() {
        WorktreeStatus::NotDir
    } else if !path.join(".git").exists() {
        WorktreeStatus::NotGit
    } else {
        WorktreeStatus::Available
    }
}

/// The resolved path to the active project's `keitora.toml` (same resolution as
/// [`Config::find`]). Useful for watching the file for live changes.
pub fn config_path() -> crate::Result<PathBuf> {
    let pc = resolve().map_err(|e| Error::Config(e.to_string()))?;
    Ok(pc.root.join("keitora.toml"))
}

/// Best-effort current worktree name: basename of the git toplevel, else
/// `"unknown"`. Used by the CLI to derive the acting worktree.
pub fn current_worktree() -> String {
    if let Some(root) = git_toplevel() {
        if let Some(name) = root.file_name() {
            return name.to_string_lossy().into_owned();
        }
    }
    "unknown".to_string()
}

/// `git rev-parse --show-toplevel`, run in the current process cwd. Exported so
/// the CLI can resolve its own project root without re-implementing it.
pub fn git_toplevel() -> Option<PathBuf> {
    git_toplevel_in(None)
}

/// A branch-name-safe slug for the current git user: `git config user.name`'s
/// first whitespace word, lowercased, with spaces collapsed to hyphens. Falls
/// back to `"llm"` on an empty/missing name or a git failure. Used to derive the
/// `llm/<user>/<repo>/<name>` branch when creating a worktree.
pub fn git_user_slug() -> String {
    let slug = git_command()
        .args(["config", "user.name"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .and_then(|name| name.split_whitespace().next().map(str::to_string))
        .map(|word| word.to_lowercase().replace(' ', "-"))
        .unwrap_or_default();
    if slug.is_empty() {
        "llm".to_string()
    } else {
        slug
    }
}

/// `git rev-parse --show-toplevel`, run in `dir` (or the process cwd when `None`).
pub(crate) fn git_toplevel_in(dir: Option<&Path>) -> Option<PathBuf> {
    let mut cmd = git_command();
    cmd.args(["rev-parse", "--show-toplevel"]);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_ENV_LOCK;
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn prepend_path_dirs_adds_homebrew_and_dedups() {
        let extra = vec![
            "/opt/homebrew/bin".to_string(),
            "/usr/local/bin".to_string(),
        ];
        // Homebrew dir is prepended; the already-present /usr/local/bin is not duplicated.
        let out = prepend_path_dirs(&extra, Some("/usr/bin:/bin:/usr/local/bin"));
        assert_eq!(out, "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin");
    }

    #[cfg(unix)]
    #[test]
    fn prepend_path_dirs_handles_empty_current() {
        let extra = vec!["/opt/homebrew/bin".to_string()];
        assert_eq!(prepend_path_dirs(&extra, None), "/opt/homebrew/bin");
        assert_eq!(prepend_path_dirs(&extra, Some("")), "/opt/homebrew/bin");
    }

    #[cfg(unix)]
    #[test]
    fn git_path_env_prepends_login_dirs_then_inherited() {
        // Login dirs come first (so git-lfs resolves), then the inherited PATH with
        // duplicates dropped (so git stays findable wherever it already was).
        let login = Some("/opt/homebrew/bin:/usr/bin:/Users/me/.asdf/shims".to_string());
        assert_eq!(
            git_path_env(login, Some("/usr/bin:/bin")),
            Some("/opt/homebrew/bin:/usr/bin:/Users/me/.asdf/shims:/bin".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_path_env_keeps_git_findable_when_login_omits_its_dir() {
        // Regression guard: login PATH lacks /usr/bin (where git lives), but the
        // inherited PATH has it — the result must still contain /usr/bin so `git`
        // itself is resolvable, not just git-lfs.
        let login = Some("/opt/homebrew/bin".to_string());
        let out = git_path_env(login, Some("/usr/bin:/bin")).unwrap();
        assert_eq!(out, "/opt/homebrew/bin:/usr/bin:/bin");
    }

    #[cfg(unix)]
    #[test]
    fn git_path_env_login_only_when_no_inherited() {
        let login = Some("/opt/homebrew/bin:/usr/bin".to_string());
        assert_eq!(
            git_path_env(login, None),
            Some("/opt/homebrew/bin:/usr/bin".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_path_env_falls_back_to_prepended_dirs() {
        // No login PATH (probe failed): prepend the known-missing dirs to inherited.
        let out = git_path_env(None, Some("/usr/bin:/bin")).unwrap();
        assert!(out.starts_with("/opt/homebrew/bin:/usr/local/bin:"));
        assert!(out.ends_with(":/usr/bin:/bin"));
    }

    #[cfg(unix)]
    #[test]
    fn git_path_env_none_when_nothing_to_use() {
        // No login PATH and no inherited PATH → set nothing (inherit unchanged).
        assert_eq!(git_path_env(None, None), None);
    }

    #[test]
    fn diff_base_toml_roundtrip_all_variants() {
        for base in [
            DiffBase::Auto,
            DiffBase::Ref {
                name: "origin/release-5.2".into(),
            },
            DiffBase::Upstream,
            DiffBase::ForkPoint {
                parent: "develop".into(),
            },
        ] {
            let meta = ProjectMeta {
                name: "p".into(),
                worktrees_dir: None,
                base_branch: None,
                diff_base: Some(base.clone()),
            };
            let text = toml::to_string_pretty(&meta).unwrap();
            let back: ProjectMeta = toml::from_str(&text).unwrap();
            assert_eq!(back.diff_base, Some(base), "toml round-trip for {text}");
        }
    }

    #[test]
    fn diff_base_json_is_camelcase_internally_tagged() {
        let j = serde_json::to_value(DiffBase::ForkPoint {
            parent: "main".into(),
        })
        .unwrap();
        assert_eq!(j["kind"], "forkPoint");
        assert_eq!(j["parent"], "main");
        // and parses back
        let back: DiffBase = serde_json::from_value(j).unwrap();
        assert_eq!(
            back,
            DiffBase::ForkPoint {
                parent: "main".into()
            }
        );

        let r: DiffBase = serde_json::from_str(r#"{"kind":"ref","name":"x"}"#).unwrap();
        assert_eq!(r, DiffBase::Ref { name: "x".into() });
    }

    #[test]
    fn diff_base_defaults_to_auto_when_absent() {
        // A [project] table with no diff_base key parses to None → resolves to Auto.
        let meta: ProjectMeta = toml::from_str("name = \"p\"\n").unwrap();
        assert_eq!(meta.diff_base, None);
        assert_eq!(DiffBase::default(), DiffBase::Auto);
    }

    #[test]
    fn diff_base_precedence_worktree_over_project_over_auto() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("p");
        write_project(&project, "p", &["p", "feat"], "../wt");
        let mut pc = ProjectConfig::load(&project).unwrap();

        // No config anywhere → Auto.
        assert_eq!(pc.diff_base(Some("feat")), DiffBase::Auto);
        assert_eq!(pc.diff_base(None), DiffBase::Auto);

        // Project default set → applies to any worktree without an override.
        pc.project.diff_base = Some(DiffBase::Upstream);
        assert_eq!(pc.diff_base(Some("feat")), DiffBase::Upstream);
        assert_eq!(pc.diff_base(None), DiffBase::Upstream);

        // Per-worktree override wins over the project default.
        pc.effective_worktrees = Some(vec![
            WorktreeEntry::name_only("p"),
            WorktreeEntry {
                diff_base: Some(DiffBase::Ref {
                    name: "origin/x".into(),
                }),
                ..WorktreeEntry::name_only("feat")
            },
        ]);
        assert_eq!(
            pc.diff_base(Some("feat")),
            DiffBase::Ref {
                name: "origin/x".into()
            }
        );
        // A different worktree with no override still sees the project default.
        assert_eq!(pc.diff_base(Some("p")), DiffBase::Upstream);
    }

    #[test]
    fn worktree_entry_with_diff_base_roundtrips_as_table() {
        let entry = WorktreeEntry {
            diff_base: Some(DiffBase::ForkPoint {
                parent: "main".into(),
            }),
            ..WorktreeEntry::name_only("feat")
        };
        // diff_base alone forces the table form (not a bare string).
        let text = toml::to_string(&entry).unwrap();
        assert!(text.contains("diff_base"), "expected table form: {text}");
        let back: WorktreeEntry = toml::from_str(&text).unwrap();
        assert_eq!(back, entry);
    }

    fn names(entries: &[WorktreeEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    fn order_of(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reorder_entries_applies_a_full_permutation() {
        let entries = ["alpha", "bravo", "charlie"].map(WorktreeEntry::name_only);
        let out = reorder_entries(&entries, &order_of(&["charlie", "alpha", "bravo"]));
        assert_eq!(names(&out), ["charlie", "alpha", "bravo"]);
    }

    #[test]
    fn reorder_entries_appends_unnamed_entries_in_their_order() {
        let entries = ["alpha", "bravo", "charlie", "wt"].map(WorktreeEntry::name_only);
        let out = reorder_entries(&entries, &order_of(&["charlie", "alpha"]));
        assert_eq!(names(&out), ["charlie", "alpha", "bravo", "wt"]);
    }

    #[test]
    fn reorder_entries_skips_unknown_and_repeated_names() {
        let entries = ["alpha", "bravo", "charlie"].map(WorktreeEntry::name_only);
        let out = reorder_entries(
            &entries,
            &order_of(&["bravo", "gone", "bravo", "charlie", "alpha"]),
        );
        assert_eq!(names(&out), ["bravo", "charlie", "alpha"]);
    }

    #[test]
    fn reorder_entries_with_empty_order_keeps_the_list() {
        let entries = ["alpha", "bravo"].map(WorktreeEntry::name_only);
        assert_eq!(names(&reorder_entries(&entries, &[])), ["alpha", "bravo"]);
        assert!(reorder_entries(&[], &order_of(&["alpha"])).is_empty());
    }

    #[test]
    fn reorder_entries_keeps_entries_that_share_a_name() {
        let entries = [
            WorktreeEntry::new("alpha", "/tmp/a1"),
            WorktreeEntry::new("bravo", "/tmp/b"),
            WorktreeEntry::new("alpha", "/tmp/a2"),
        ];
        let out = reorder_entries(&entries, &order_of(&["alpha"]));
        assert_eq!(
            out,
            vec![entries[0].clone(), entries[2].clone(), entries[1].clone()]
        );
    }

    #[test]
    fn reorder_entries_preserves_every_field() {
        let alpha = WorktreeEntry {
            cli: Some(CliKind::Codex),
            codex_thread_id: Some("thread".into()),
            claude_session_id: Some("session".into()),
            diff_base: Some(DiffBase::Upstream),
            ..WorktreeEntry::new("alpha", "/tmp/alpha")
        };
        let bravo = WorktreeEntry::new("bravo", "/tmp/bravo");
        let out = reorder_entries(&[alpha.clone(), bravo.clone()], &order_of(&["bravo"]));
        assert_eq!(out, vec![bravo, alpha]);
    }

    #[test]
    fn merge_worktree_registry_fields_carries_forward_diff_base() {
        let old = vec![WorktreeEntry {
            diff_base: Some(DiffBase::Upstream),
            ..WorktreeEntry::new("feat", "/tmp/feat")
        }];
        // Incoming has the path but no diff_base (e.g. a plain worktree-list refresh).
        let incoming = vec![WorktreeEntry::new("feat", "/tmp/feat")];
        let merged = merge_worktree_registry_fields(Some(&old), &incoming);
        assert_eq!(merged[0].diff_base, Some(DiffBase::Upstream));
    }

    #[test]
    fn settings_patch_sets_and_clears_diff_base() {
        let mut pref = ProjectRef::new(PathBuf::from("/tmp/p"));
        // Set.
        ProjectSettingsPatch {
            diff_base: Some(Some(DiffBase::Upstream)),
            ..Default::default()
        }
        .apply_to(&mut pref);
        assert_eq!(pref.diff_base, Some(DiffBase::Upstream));
        // None → unchanged.
        ProjectSettingsPatch::default().apply_to(&mut pref);
        assert_eq!(pref.diff_base, Some(DiffBase::Upstream));
        // Some(None) → clear.
        ProjectSettingsPatch {
            diff_base: Some(None),
            ..Default::default()
        }
        .apply_to(&mut pref);
        assert_eq!(pref.diff_base, None);
    }

    #[test]
    fn set_worktree_diff_base_sets_clears_and_materializes() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path());
        let root = PathBuf::from("/code/acme");
        let mut reg = UserConfig::default();
        reg.register("acme", root.clone());
        // Registry list still None → effective list comes from `current` (committed fallback).
        let current = vec![
            WorktreeEntry::new("acme", "/code/acme"),
            WorktreeEntry::new("feat", "/code/acme-wt/feat"),
        ];

        // Set an override on `feat` → list materializes with the override stamped.
        let matched = reg
            .set_worktree_diff_base_by_root(&root, "feat", Some(DiffBase::Upstream), &current)
            .unwrap();
        assert!(matched);
        let list = reg.projects["acme"].worktrees.as_ref().unwrap();
        let feat = list.iter().find(|e| e.name == "feat").unwrap();
        assert_eq!(feat.diff_base, Some(DiffBase::Upstream));
        // The other entry is preserved, without an override.
        assert_eq!(
            list.iter().find(|e| e.name == "acme").unwrap().diff_base,
            None
        );

        // Clear it → back to None (the carry-forward merge would have kept it; this must not).
        reg.set_worktree_diff_base_by_root(&root, "feat", None, &current)
            .unwrap();
        let feat = reg.projects["acme"]
            .worktrees
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "feat")
            .unwrap()
            .clone();
        assert_eq!(
            feat.diff_base, None,
            "clearing must actually clear the override"
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn reregister_different_path_clears_diff_base_override() {
        let mut uc = UserConfig::default();
        uc.register("p", PathBuf::from("/tmp/a"));
        uc.projects.get_mut("p").unwrap().diff_base = Some(DiffBase::Upstream);
        // Same name, different path → overrides reset.
        uc.register("p", PathBuf::from("/tmp/b"));
        assert_eq!(uc.projects["p"].diff_base, None);
    }

    /// Write a minimal valid ProjectConfig TOML to `<dir>/keitora.toml`.
    fn write_project(dir: &Path, name: &str, worktrees: &[&str], worktrees_dir: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let wts = worktrees
            .iter()
            .map(|w| format!("\"{w}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let text = format!(
            "[project]\nname = \"{name}\"\nworktrees_dir = \"{worktrees_dir}\"\n\n\
             [tasks]\ndb_path = \"shared/tasks.db\"\nworktrees = [{wts}]\n"
        );
        std::fs::write(dir.join("keitora.toml"), text).unwrap();
    }

    // --- Unit 1: Resolution order ($KEITORA_PROJECT > registry > cwd-walk) ---

    #[test]
    fn resolution_order_respects_priority() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let project_a = root.join("project_a");
        let project_b = root.join("project_b");
        write_project(&project_a, "project_a", &[], "../a-worktrees");
        write_project(&project_b, "project_b", &[], "../b-worktrees");
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();

        // Registry points only at project_b. Isolate the user registry to this
        // temp dir so the test never reads real machine state (hermetic).
        std::env::set_var("KEITORA_CONFIG_HOME", root.join("user_config"));
        let reg_path = root.join("user_config/keitora/keitora.toml");
        let mut reg = UserConfig::default();
        reg.register("b", project_b.clone());
        reg.save_to(&reg_path).unwrap();

        // 1. $KEITORA_PROJECT wins over everything (even a cwd inside project_b).
        std::env::set_var("KEITORA_PROJECT", &project_a);
        assert_eq!(resolve_in(&project_b).unwrap().project.name, "project_a");
        std::env::remove_var("KEITORA_PROJECT");

        // 2. registry-match (now ahead of cwd-walk): a cwd inside the registered
        // project_b resolves via the registry.
        assert_eq!(resolve_in(&project_b).unwrap().project.name, "project_b");

        // 3. cwd-walk fallback: project_a is NOT registered, so a cwd inside it
        // falls through past the registry to the cwd-walk, which finds
        // project_a's own keitora.toml.
        assert_eq!(resolve_in(&project_a).unwrap().project.name, "project_a");

        // 4. no env, a cwd with no keitora.toml not inside any registered project →
        // NotFound. The hermetic registry only knows project_b, and `empty` is
        // outside it.
        let err = resolve_in(&empty).unwrap_err();
        assert!(matches!(err, ResolveError::NotFound), "got {err:?}");

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    // --- Unit 2: Registry round-trip ---

    #[test]
    fn registry_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("keitora/keitora.toml");

        let mut cfg = UserConfig::default();
        cfg.register("keitora", PathBuf::from("/Users/x/code/keitora"));
        cfg.register("myapp", PathBuf::from("/Users/x/code/myapp"));
        cfg.set_active("keitora");
        cfg.save_to(&path).unwrap();

        let loaded = UserConfig::load_from(&path).unwrap();
        assert_eq!(loaded.active.as_deref(), Some("keitora"));
        assert_eq!(loaded.projects.len(), 2);
        assert_eq!(
            loaded.projects["keitora"].path,
            PathBuf::from("/Users/x/code/keitora")
        );

        // active = None round-trips as absent.
        let mut none_cfg = UserConfig::default();
        none_cfg.register("keitora", PathBuf::from("/Users/x/code/keitora"));
        let none_path = tmp.path().join("keitora/none.toml");
        none_cfg.save_to(&none_path).unwrap();
        let loaded = UserConfig::load_from(&none_path).unwrap();
        assert!(loaded.active.is_none());
    }

    // --- Project display order ---

    fn registry_of(keys: &[&str]) -> UserConfig {
        let mut cfg = UserConfig::default();
        for k in keys {
            cfg.register(k, PathBuf::from(format!("/code/{k}")));
        }
        cfg
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn project_order_empty_is_alphabetical() {
        let cfg = registry_of(&["zeta", "alpha", "mid"]);
        assert_eq!(
            cfg.ordered_project_names(),
            strings(&["alpha", "mid", "zeta"])
        );
    }

    #[test]
    fn project_order_ignores_stale_and_duplicate_entries() {
        let mut cfg = registry_of(&["a", "b", "c"]);
        cfg.project_order = strings(&["c", "gone", "a", "c"]);
        assert_eq!(cfg.ordered_project_names(), strings(&["c", "a", "b"]));
    }

    #[test]
    fn project_order_appends_newly_registered_projects() {
        let mut cfg = registry_of(&["b", "a"]);
        cfg.set_project_order(strings(&["b", "a"]));
        cfg.register("d", PathBuf::from("/code/d"));
        cfg.register("c", PathBuf::from("/code/c"));
        assert_eq!(cfg.ordered_project_names(), strings(&["b", "a", "c", "d"]));
    }

    #[test]
    fn set_project_order_accepts_a_permutation() {
        let mut cfg = registry_of(&["a", "b", "c"]);
        cfg.set_project_order(strings(&["c", "a", "b"]));
        assert_eq!(cfg.project_order, strings(&["c", "a", "b"]));
        assert_eq!(cfg.ordered_project_names(), strings(&["c", "a", "b"]));
    }

    #[test]
    fn set_project_order_ignores_unknown_and_duplicate_keys() {
        let mut cfg = registry_of(&["a", "b", "c"]);
        cfg.set_project_order(strings(&["c", "x", "a", "c", "b"]));
        assert_eq!(cfg.project_order, strings(&["c", "a", "b"]));
    }

    #[test]
    fn set_project_order_keeps_missing_keys_in_current_order_after_given_ones() {
        let mut cfg = registry_of(&["a", "b", "c", "d"]);
        cfg.set_project_order(strings(&["d", "c", "b", "a"]));
        // "c" and "a" are absent (e.g. their keitora.toml failed to load): they
        // keep their current relative order (c before a) after the given keys.
        cfg.set_project_order(strings(&["b", "d"]));
        assert_eq!(cfg.project_order, strings(&["b", "d", "c", "a"]));
        assert_eq!(cfg.ordered_project_names(), strings(&["b", "d", "c", "a"]));
    }

    #[test]
    fn set_project_order_with_nothing_known_keeps_the_current_order() {
        let mut cfg = registry_of(&["a", "b"]);
        cfg.set_project_order(strings(&["b", "a"]));
        cfg.set_project_order(strings(&["gone"]));
        assert_eq!(cfg.ordered_project_names(), strings(&["b", "a"]));
    }

    #[test]
    fn project_order_round_trips_through_toml() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("keitora/keitora.toml");
        let mut cfg = registry_of(&["a", "b", "c"]);
        cfg.set_active("b");
        cfg.set_project_order(strings(&["c", "a", "b"]));
        cfg.save_to(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let order_at = text.find("project_order").expect("order serialized");
        let projects_at = text.find("[projects").expect("projects serialized");
        assert!(order_at < projects_at, "order precedes tables: {text}");

        let loaded = UserConfig::load_from(&path).unwrap();
        assert_eq!(loaded.project_order, strings(&["c", "a", "b"]));
        assert_eq!(loaded.ordered_project_names(), strings(&["c", "a", "b"]));
        assert_eq!(loaded.projects.len(), 3);
    }

    #[test]
    fn registry_write_lock_survives_a_panicking_holder() {
        let poisoned = std::thread::spawn(|| {
            let _held = UserConfig::write_lock();
            panic!("writer died mid-save");
        })
        .join();
        assert!(poisoned.is_err());
        // A later writer still gets the lock instead of panicking on the poison.
        drop(UserConfig::write_lock());
    }

    #[test]
    fn empty_project_order_is_not_serialized() {
        let cfg = registry_of(&["a"]);
        let toml = toml::to_string(&cfg).unwrap();
        assert!(!toml.contains("project_order"), "{toml}");
    }

    // --- [pty] herdr location ---

    #[test]
    fn pty_config_default_is_omitted_from_toml() {
        let cfg = UserConfig::default();
        assert!(cfg.pty.is_empty());
        let toml = toml::to_string(&cfg).unwrap();
        assert!(
            !toml.contains("[pty]"),
            "default [pty] should be omitted: {toml}"
        );
    }

    #[test]
    fn pty_config_round_trips() {
        let toml = r#"
[pty]
herdr_bin = "/opt/herdr/herdr"
"#;
        let cfg: UserConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.pty.herdr_bin, Some(PathBuf::from("/opt/herdr/herdr")));
        assert!(!cfg.pty.is_empty());

        // Re-serializing keeps the table.
        let out = toml::to_string(&cfg).unwrap();
        assert!(out.contains("[pty]"));
        assert!(out.contains("herdr_bin"), "got: {out}");
    }

    #[test]
    fn legacy_pty_backend_key_is_ignored_and_dropped_on_save() {
        let toml = r#"
[pty]
backend = "ptyd"
herdr_bin = "/opt/herdr/herdr"

[projects.proj]
path = "/work/proj"
"#;
        // A config written while the backend selector existed loads through the
        // real file path; an unknown value there must not fail the whole registry.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("keitora.toml");
        std::fs::write(&path, toml).unwrap();
        let cfg = UserConfig::load_from(&path).unwrap();
        assert_eq!(cfg.pty.herdr_bin, Some(PathBuf::from("/opt/herdr/herdr")));

        cfg.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("backend"), "backend must be dropped: {text}");

        let reloaded = UserConfig::load_from(&path).unwrap();
        assert_eq!(
            reloaded.pty.herdr_bin,
            Some(PathBuf::from("/opt/herdr/herdr"))
        );
        assert_eq!(
            reloaded.projects.get("proj").map(|p| p.path.clone()),
            Some(PathBuf::from("/work/proj"))
        );
    }

    // --- [pty.herdr.recovery] ---

    #[test]
    fn recovery_config_defaults_match_documented_values() {
        let c = RecoveryConfig::default();
        assert_eq!(c.headroom_base_mib, 4096);
        assert_eq!(c.headroom_per_session_mib, 600);
        assert_eq!(c.headroom_sustain_secs, 60);
        assert_eq!(c.headroom_cap_secs, 600);
        assert_eq!(c.stagger_secs, 5);
        assert_eq!(c.resume_concurrency, 3);
        assert_eq!(c.backoff_secs, vec![0, 60, 300]);
        assert_eq!(c.breaker_deaths, 3);
        assert_eq!(c.breaker_window_secs, 1800);
        assert_eq!(c.early_death_secs, 30);
        assert_eq!(c.resume_retrip_secs, 600);
        assert_eq!(c.healthy_reset_secs, 1800);
        assert!(c.is_default());
        assert_eq!(UserConfig::default().pty.herdr.recovery, c);
    }

    #[test]
    fn recovery_config_partial_table_fills_defaults() {
        let cfg: UserConfig = toml::from_str("[pty.herdr.recovery]\nstagger_secs = 9\n").unwrap();
        let expected = RecoveryConfig {
            stagger_secs: 9,
            ..RecoveryConfig::default()
        };
        assert_eq!(cfg.pty.herdr.recovery, expected);
        assert!(cfg.pty.herdr_bin.is_none());
    }

    #[test]
    fn recovery_config_round_trips_and_is_omitted_when_default() {
        let out = toml::to_string(&UserConfig::default()).unwrap();
        assert!(
            !out.contains("recovery"),
            "default must omit recovery: {out}"
        );
        assert!(!out.contains("[pty"), "default must omit [pty]: {out}");

        let mut cfg = UserConfig::default();
        cfg.pty.herdr.recovery.backoff_secs = vec![0, 30];
        cfg.pty.herdr.recovery.headroom_base_mib = 2048;
        cfg.pty.herdr.recovery.resume_concurrency = 6;
        let out = toml::to_string(&cfg).unwrap();
        assert!(out.contains("[pty.herdr.recovery]"), "got: {out}");
        let back: UserConfig = toml::from_str(&out).unwrap();
        assert_eq!(back.pty.herdr.recovery, cfg.pty.herdr.recovery);
        assert_eq!(back.pty.herdr.recovery.resume_concurrency, 6);
    }

    #[test]
    fn pty_config_is_empty_false_when_recovery_customized() {
        let mut pty = PtyConfig::default();
        assert!(pty.is_empty());
        pty.herdr.recovery.breaker_deaths = 5;
        assert!(!pty.herdr.is_empty());
        assert!(!pty.is_empty());
    }

    #[test]
    fn recovery_config_normalized_clamps() {
        let c = RecoveryConfig {
            backoff_secs: vec![],
            breaker_deaths: 0,
            headroom_sustain_secs: 900,
            headroom_cap_secs: 300,
            stagger_secs: 0,
            resume_concurrency: 0,
            ..RecoveryConfig::default()
        };
        let n = c.normalized();
        assert_eq!(n.backoff_secs, vec![0]);
        assert_eq!(n.breaker_deaths, 1);
        assert_eq!(n.headroom_sustain_secs, 300);
        assert_eq!(n.headroom_cap_secs, 300);
        // Zero is legal and kept.
        assert_eq!(n.stagger_secs, 0);
        assert_eq!(n.resume_concurrency, 1);
        // Large values are not clamped from above.
        let big = RecoveryConfig {
            resume_concurrency: 64,
            ..RecoveryConfig::default()
        };
        assert_eq!(big.normalized().resume_concurrency, 64);
        // Sane values pass through unchanged.
        assert_eq!(
            RecoveryConfig::default().normalized(),
            RecoveryConfig::default()
        );
    }

    #[test]
    fn recovery_config_base_margin_bytes_is_normalized_mib_to_bytes() {
        let c = RecoveryConfig::default();
        assert_eq!(c.base_margin_bytes(), 4096 * 1024 * 1024);
        assert_eq!(c.per_session_bytes(), 600 * 1024 * 1024);
        let c = RecoveryConfig {
            headroom_base_mib: 1,
            headroom_per_session_mib: 2,
            ..RecoveryConfig::default()
        };
        assert_eq!(c.base_margin_bytes(), 1024 * 1024);
        assert_eq!(c.per_session_bytes(), 2 * 1024 * 1024);
    }

    #[test]
    fn pty_config_herdr_bin_and_recovery_table_coexist() {
        let toml_text = r#"
[pty]
herdr_bin = "/opt/herdr/herdr"

[pty.herdr.recovery]
headroom_cap_secs = 120
backoff_secs = [0, 10]
"#;
        let cfg: UserConfig = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.pty.herdr_bin, Some(PathBuf::from("/opt/herdr/herdr")));
        assert_eq!(cfg.pty.herdr.recovery.headroom_cap_secs, 120);
        assert_eq!(cfg.pty.herdr.recovery.backoff_secs, vec![0, 10]);
        assert_eq!(cfg.pty.herdr.recovery.stagger_secs, 5);
        let out = toml::to_string(&cfg).unwrap();
        let back: UserConfig = toml::from_str(&out).unwrap();
        assert_eq!(back.pty.herdr_bin, cfg.pty.herdr_bin);
        assert_eq!(back.pty.herdr.recovery, cfg.pty.herdr.recovery);
    }

    #[test]
    fn pty_config_scrollback_limit_round_trips() {
        let cfg: UserConfig =
            toml::from_str("[pty]\nherdr_scrollback_limit_bytes = 2000000\n").unwrap();
        assert_eq!(cfg.pty.herdr_scrollback_limit_bytes, Some(2_000_000));
        assert!(!cfg.pty.is_empty());
        let out = toml::to_string(&cfg).unwrap();
        assert!(
            out.contains("herdr_scrollback_limit_bytes = 2000000"),
            "{out}"
        );
        let back: UserConfig = toml::from_str(&out).unwrap();
        assert_eq!(back.pty.herdr_scrollback_limit_bytes, Some(2_000_000));
    }

    #[test]
    fn pty_config_scrollback_limit_unset_is_empty_and_omitted() {
        let cfg = PtyConfig::default();
        assert!(cfg.herdr_scrollback_limit_bytes.is_none());
        assert!(cfg.is_empty());
        let out = toml::to_string(&cfg).unwrap();
        assert!(!out.contains("herdr_scrollback_limit_bytes"), "{out}");
    }

    #[test]
    fn herdr_bin_expanded_resolves_tilde() {
        let home = dirs::home_dir().expect("home dir");
        let cfg = PtyConfig {
            herdr_bin: Some(PathBuf::from("~/.herdr/herdr")),
            ..Default::default()
        };
        assert_eq!(cfg.herdr_bin_expanded(), Some(home.join(".herdr/herdr")));
    }

    #[test]
    fn herdr_bin_candidates_priority_and_dedup() {
        let cfg = PtyConfig {
            herdr_bin: Some(PathBuf::from("/cfg/herdr")),
            ..Default::default()
        };
        let got = herdr_bin_candidates(
            &cfg,
            Some(PathBuf::from("/env/herdr")),
            vec![
                PathBuf::from("/sidecar/herdr"),
                PathBuf::from("/cfg/herdr"), // duplicate of config -> dropped
                PathBuf::from("/path/herdr"),
            ],
        );
        assert_eq!(
            got,
            vec![
                PathBuf::from("/cfg/herdr"),
                PathBuf::from("/env/herdr"),
                PathBuf::from("/sidecar/herdr"),
                PathBuf::from("/path/herdr"),
            ],
        );
    }

    #[test]
    fn herdr_bin_candidates_empty_when_nothing_configured() {
        let cfg = PtyConfig::default();
        assert!(herdr_bin_candidates(&cfg, None, vec![]).is_empty());
    }

    // --- ProjectRef setting overrides ---

    #[test]
    fn project_ref_overrides_round_trip_and_omit_when_none() {
        let tmp = TempDir::new().unwrap();

        // A bare entry omits every override key from the serialized TOML.
        let mut bare = UserConfig::default();
        bare.register("plain", PathBuf::from("/code/plain"));
        let toml = toml::to_string(&bare).unwrap();
        for key in [
            "db_path",
            "worktrees_dir",
            "archive_days",
            "log_retention_days",
            "dispatch_steps",
            "worktrees",
        ] {
            assert!(
                !toml.contains(key),
                "{key} must be omitted when None; got:\n{toml}"
            );
        }

        // A fully-populated entry round-trips through save/load.
        let mut cfg = UserConfig::default();
        cfg.register("acme", PathBuf::from("/code/acme"));
        {
            let e = cfg.projects.get_mut("acme").unwrap();
            e.db_path = Some(PathBuf::from("custom/tasks.db"));
            e.worktrees_dir = Some("~/wt/acme".to_string());
            e.archive_days = Some(14);
            e.log_retention_days = Some(45);
            e.dispatch_steps = Some(vec!["/clear".to_string(), "/park".to_string()]);
        }
        let path = tmp.path().join("keitora/keitora.toml");
        cfg.save_to(&path).unwrap();
        let loaded = UserConfig::load_from(&path).unwrap();
        let e = &loaded.projects["acme"];
        assert_eq!(e.db_path.as_deref(), Some(Path::new("custom/tasks.db")));
        assert_eq!(e.worktrees_dir.as_deref(), Some("~/wt/acme"));
        assert_eq!(e.archive_days, Some(14));
        assert_eq!(e.log_retention_days, Some(45));
        assert_eq!(
            e.dispatch_steps.as_deref(),
            Some(["/clear".to_string(), "/park".to_string()].as_slice())
        );
    }

    #[test]
    fn register_resets_all_overrides_on_path_change() {
        let mut cfg = UserConfig::default();
        cfg.register("p", PathBuf::from("/code/old"));
        {
            let e = cfg.projects.get_mut("p").unwrap();
            e.worktrees = Some(vec![WorktreeEntry::name_only("a")]);
            e.db_path = Some(PathBuf::from("x.db"));
            e.worktrees_dir = Some("~/wt".to_string());
            e.archive_days = Some(7);
            e.log_retention_days = Some(7);
            e.dispatch_steps = Some(vec!["/clear".to_string()]);
        }
        // Re-registering the SAME name at a DIFFERENT path must clear every override
        // so the old project's per-machine state can't leak onto the new one.
        cfg.register("p", PathBuf::from("/code/new"));
        let e = &cfg.projects["p"];
        assert_eq!(e.path, PathBuf::from("/code/new"));
        assert!(e.worktrees.is_none());
        assert!(e.db_path.is_none());
        assert!(e.worktrees_dir.is_none());
        assert!(e.archive_days.is_none());
        assert!(e.log_retention_days.is_none());
        assert!(e.dispatch_steps.is_none());
    }

    #[test]
    fn normalize_db_path_resolves_relative_tilde_and_absolute() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();

        // Relative → joined against root (canonicalized root for a fair compare).
        let got = normalize_db_path(Path::new("shared/tasks.db"), &root);
        let want = canonicalize_root(&root).join("shared/tasks.db");
        assert_eq!(got, want, "relative db_path resolves against root");

        // Tilde → expands to the home dir (absolute, not under root).
        let got = normalize_db_path(Path::new("~/.keitora/proj/tasks.db"), &root);
        assert!(got.is_absolute(), "tilde db_path is absolute: {got:?}");
        assert!(
            !got.starts_with(&root),
            "tilde db_path is not under root: {got:?}"
        );

        // Already-absolute → kept (modulo canonicalization of an existing path).
        let abs = tmp.path().join("elsewhere/tasks.db");
        let got = normalize_db_path(&abs, &root);
        assert!(got.is_absolute());
        assert!(got.ends_with("elsewhere/tasks.db"));
    }

    // --- skip_permissions ---

    #[test]
    fn skip_permissions_defaults_false_and_omits_table() {
        // Fresh config: flag is false and the [claude] table is omitted entirely.
        let cfg = UserConfig::default();
        assert!(!cfg.skip_permissions());
        assert!(cfg.claude.is_empty());
        let toml = toml::to_string(&cfg).unwrap();
        assert!(!toml.contains("skip_permissions"), "got: {toml}");
        assert!(!toml.contains("[claude]"), "got: {toml}");
    }

    #[test]
    fn skip_permissions_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("keitora/keitora.toml");

        let mut cfg = UserConfig::default();
        cfg.claude.autonomy = Some(true);
        cfg.save_to(&path).unwrap();

        // Even with no claude path, the table is written because the flag is set.
        // Freshly written TOML uses the `autonomy` key (the legacy
        // `skip_permissions` name is a deserialize-only alias).
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("autonomy = true"), "got: {text}");

        let loaded = UserConfig::load_from(&path).unwrap();
        assert!(loaded.skip_permissions());
    }

    #[test]
    fn set_skip_permissions_persists() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path().join("user_config"));

        assert!(!UserConfig::load().unwrap().skip_permissions());
        UserConfig::set_skip_permissions(true).unwrap();
        assert!(UserConfig::load().unwrap().skip_permissions());
        UserConfig::set_skip_permissions(false).unwrap();
        assert!(!UserConfig::load().unwrap().skip_permissions());

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    // --- legacy [remote] table ---

    #[test]
    fn legacy_remote_table_still_loads_and_is_dropped_on_save() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("keitora/keitora.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "active = \"proj\"

[remote]
enabled = true
relay_url = \"https://relay.example\"
",
        )
        .unwrap();

        let cfg = UserConfig::load_from(&path).unwrap();
        assert_eq!(cfg.active.as_deref(), Some("proj"));

        cfg.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("[remote]"), "got: {text}");
        assert!(!text.contains("relay_url"), "got: {text}");
        assert!(text.contains("active = \"proj\""), "got: {text}");
    }

    #[test]
    fn resolve_skip_permissions_project_overrides_user() {
        let tmp = TempDir::new().unwrap();
        // Load a ProjectConfig whose [claude] table carries `body` (also exercises
        // deserialization of the new key). db file must exist for db_path canon.
        let load_proj = |dir: &str, claude_body: &str| {
            let p = tmp.path().join(dir);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("tasks.db"), b"").unwrap();
            std::fs::write(
                p.join("keitora.toml"),
                format!(
                    "[project]\nname = \"p\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n{claude_body}"
                ),
            )
            .unwrap();
            ProjectConfig::load(&p).unwrap()
        };

        let user_on = UserConfig {
            claude: ProviderConfig {
                path: None,
                autonomy: Some(true),
            },
            ..UserConfig::default()
        };
        let user_unset = UserConfig::default();
        let proj_true = load_proj("a", "\n[claude]\nskip_permissions = true\n");
        let proj_false = load_proj("b", "\n[claude]\nskip_permissions = false\n");
        let proj_unset = load_proj("c", "");

        // Unset everywhere → false.
        assert!(!resolve_skip_permissions(None, &user_unset));
        // User-only → user value.
        assert!(resolve_skip_permissions(None, &user_on));
        // Project-set wins over user, both directions.
        assert!(resolve_skip_permissions(Some(&proj_true), &user_unset));
        assert!(!resolve_skip_permissions(Some(&proj_false), &user_on));
        // Project unset falls through to user.
        assert!(resolve_skip_permissions(Some(&proj_unset), &user_on));
    }

    // --- Unit 3: Path canonicalization ---

    #[test]
    fn db_path_is_canonicalized_against_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("myproject");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(root.join("shared")).unwrap();
        std::fs::write(root.join("shared/tasks.db"), b"").unwrap();
        std::fs::write(
            project.join("keitora.toml"),
            "[project]\nname = \"myproject\"\n\n[tasks]\ndb_path = \"../shared/tasks.db\"\nworktrees = []\n",
        )
        .unwrap();

        let cfg = ProjectConfig::load(&project).unwrap();
        // load() strips the Windows `\\?\` prefix from db_path; mirror it (no-op on unix).
        let expected = strip_extended_prefix(root.join("shared/tasks.db").canonicalize().unwrap());
        assert_eq!(cfg.tasks.db_path, expected);
        assert!(cfg.tasks.db_path.is_absolute());
    }

    // --- Unit 4: Worktree registry-match ---

    #[test]
    fn registry_match_resolves_from_worktree_cwd() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("myproject");
        write_project(
            &project,
            "myproject",
            &["myproject", "wt-a"],
            "../myproject-worktrees",
        );
        let wt_a = root.join("myproject-worktrees/wt-a");
        std::fs::create_dir_all(&wt_a).unwrap();

        let mut reg = UserConfig::default();
        reg.register("myproject", project.clone());

        // A cwd inside wt-a (no keitora.toml in that tree) resolves via the
        // registry's knowledge of the project's worktree paths. Passed
        // explicitly — no process-global cwd mutation.
        let matched = registry_match(&reg, &wt_a).expect("registry match should resolve");
        assert_eq!(matched.project.name, "myproject");
    }

    // --- Unit 4c: effective worktree list ---

    #[test]
    fn effective_worktrees_prefers_registry_then_committed() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("myproject");
        write_project(
            &project,
            "myproject",
            &["myproject", "committed-wt"],
            "../wt",
        );
        let pc = ProjectConfig::load(&project).unwrap();

        // Registry list set (Some) → wins.
        let pref_full = ProjectRef {
            worktrees: Some(vec![
                WorktreeEntry::name_only("myproject"),
                WorktreeEntry::name_only("registry-wt"),
            ]),
            ..ProjectRef::new(project.clone())
        };
        assert_eq!(
            effective_worktrees(&pref_full, &pc),
            vec![
                WorktreeEntry::name_only("myproject"),
                WorktreeEntry::name_only("registry-wt"),
            ]
        );

        // Registry list unset (None) → committed fallback.
        let pref_none = ProjectRef::new(project.clone());
        assert_eq!(
            effective_worktrees(&pref_none, &pc),
            vec![
                WorktreeEntry::name_only("myproject"),
                WorktreeEntry::name_only("committed-wt"),
            ]
        );

        // Registry list explicitly empty (Some([])) → authoritative, NOT fallback.
        let pref_empty = ProjectRef {
            worktrees: Some(vec![]),
            ..ProjectRef::new(project.clone())
        };
        assert!(effective_worktrees(&pref_empty, &pc).is_empty());
    }

    #[test]
    fn worktree_path_prefers_stored_entry_path_over_worktrees_dir() {
        // Regression seed: an entry carrying an explicit path resolves there,
        // even when it lives nowhere near `worktrees_dir`. A name-only entry (or an
        // unknown name) falls back to the worktrees-root reconstruction.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("myproject");
        let mut pc = pc_with_worktrees_dir(&root, "myproject", Some("../myproject-worktrees"));
        let scattered = tmp.path().join("somewhere/else/feat");
        pc.effective_worktrees = Some(vec![
            WorktreeEntry::name_only("myproject"),
            WorktreeEntry::new("feat", scattered.clone()),
            WorktreeEntry::name_only("legacy"),
        ]);

        // Main worktree → root, regardless of the entry list.
        assert_eq!(pc.worktree_path("myproject"), root);
        // Entry with a stored path → that exact path (not under worktrees_dir).
        assert_eq!(pc.worktree_path("feat"), scattered);
        // Name-only entry → legacy reconstruction under the worktrees root.
        assert_eq!(
            pc.worktree_path("legacy"),
            pc.worktrees_root().join("legacy")
        );
        // Unknown name → legacy reconstruction too.
        assert_eq!(
            pc.worktree_path("unknown"),
            pc.worktrees_root().join("unknown")
        );
    }

    #[test]
    fn worktrees_accessor_falls_back_to_committed_when_unstamped() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("myproject");
        write_project(
            &project,
            "myproject",
            &["myproject", "committed-wt"],
            "../wt",
        );
        // Loaded directly (no resolver) → effective_worktrees is None → committed.
        let pc = ProjectConfig::load(&project).unwrap();
        assert_eq!(
            pc.worktrees(),
            vec!["myproject".to_string(), "committed-wt".to_string()]
        );
    }

    #[test]
    fn resolve_active_stamps_registry_worktrees() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("myproject");
        // Committed list intentionally differs from the registry list.
        write_project(
            &project,
            "myproject",
            &["myproject"],
            "../myproject-worktrees",
        );

        std::env::set_var("KEITORA_CONFIG_HOME", root.join("user_config"));
        let reg_path = root.join("user_config/keitora/keitora.toml");
        let mut reg = UserConfig::default();
        reg.register("myproject", project.clone());
        reg.projects.get_mut("myproject").unwrap().worktrees = Some(vec![
            WorktreeEntry::name_only("myproject"),
            WorktreeEntry::name_only("extra-wt"),
        ]);
        reg.set_active("myproject");
        reg.save_to(&reg_path).unwrap();

        // GUI path: the resolved config carries the registry (effective) list, and
        // the Config shim built from it sees the same list (stamp-before-shim).
        let pc = resolve_active_in(|| project.clone()).unwrap();
        assert_eq!(
            pc.worktrees(),
            vec!["myproject".to_string(), "extra-wt".to_string()]
        );
        let shim = Config::from_project(pc);
        assert_eq!(
            shim.worktrees,
            vec!["myproject".to_string(), "extra-wt".to_string()]
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn find_active_resolves_the_active_project_not_cwd() {
        // The bug: hook install used the cwd-based resolver, which ignores
        // the registry's active-project selection. `Config::find_active()` must
        // return the ACTIVE project's worktrees regardless of cwd. Two projects A
        // and B with distinct worktree lists are registered; B is active; the
        // active-project branch resolves before any cwd logic, so find_active()
        // yields B's list even though the test process cwd is inside neither.
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project_a = root.join("project_a");
        let project_b = root.join("project_b");
        write_project(
            &project_a,
            "project_a",
            &["project_a", "a-wt"],
            "../a-worktrees",
        );
        write_project(
            &project_b,
            "project_b",
            &["project_b", "b-wt"],
            "../b-worktrees",
        );

        std::env::set_var("KEITORA_CONFIG_HOME", root.join("user_config"));
        // Guard against an ambient $KEITORA_PROJECT (it wins over the active project).
        std::env::remove_var("KEITORA_PROJECT");
        let reg_path = root.join("user_config/keitora/keitora.toml");
        let mut reg = UserConfig::default();
        reg.register("project_a", project_a.clone());
        reg.register("project_b", project_b.clone());
        reg.set_active("project_b");
        reg.save_to(&reg_path).unwrap();

        let shim = Config::find_active().unwrap();
        assert_eq!(
            shim.worktrees,
            vec!["project_b".to_string(), "b-wt".to_string()],
            "find_active must resolve the active project (B), not A or cwd"
        );
        // The project accessor exposes the same resolved project.
        assert_eq!(shim.project_config().project.name, "project_b");

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn registry_match_resolves_worktree_listed_only_in_registry() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("myproject");
        // Committed list does NOT include reg-only-wt.
        write_project(
            &project,
            "myproject",
            &["myproject"],
            "../myproject-worktrees",
        );
        let reg_only = root.join("myproject-worktrees/reg-only-wt");
        std::fs::create_dir_all(&reg_only).unwrap();

        let mut reg = UserConfig::default();
        reg.register("myproject", project.clone());
        reg.projects.get_mut("myproject").unwrap().worktrees = Some(vec![
            WorktreeEntry::name_only("myproject"),
            WorktreeEntry::name_only("reg-only-wt"),
        ]);

        // cwd inside a worktree known only to the registry still resolves.
        let matched =
            registry_match(&reg, &reg_only).expect("registry-only worktree should resolve");
        assert_eq!(matched.project.name, "myproject");
        assert!(matched.worktrees().contains(&"reg-only-wt".to_string()));
    }

    #[test]
    fn stamp_from_registry_stamps_registered_else_falls_back() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().canonicalize().unwrap().join("myproject");
        write_project(&project, "myproject", &["committed"], "../wt");

        let mut reg = UserConfig::default();
        reg.register("myproject", project.clone());
        reg.projects.get_mut("myproject").unwrap().worktrees =
            Some(vec![WorktreeEntry::name_only("registry-wt")]);

        // Registered project (e.g. resolved via $KEITORA_PROJECT / cwd-walk) → stamped.
        let mut pc = ProjectConfig::load(&project).unwrap();
        stamp_from_registry(&mut pc, &reg);
        assert_eq!(pc.worktrees(), vec!["registry-wt".to_string()]);

        // Unregistered project → no-op, accessor falls back to committed.
        let other = tmp.path().canonicalize().unwrap().join("other");
        write_project(&other, "other", &["committed-other"], "../wt");
        let mut pc_other = ProjectConfig::load(&other).unwrap();
        stamp_from_registry(&mut pc_other, &reg);
        assert_eq!(pc_other.worktrees(), vec!["committed-other".to_string()]);
    }

    #[test]
    fn register_resets_worktrees_when_path_differs_keeps_when_same() {
        let mut reg = UserConfig::default();
        reg.register("proj", PathBuf::from("/a/proj"));
        reg.projects.get_mut("proj").unwrap().worktrees =
            Some(vec![WorktreeEntry::name_only("wt")]);

        // Re-register same name + same path → worktrees preserved.
        reg.register("proj", PathBuf::from("/a/proj"));
        assert_eq!(
            reg.projects["proj"].worktrees,
            Some(vec![WorktreeEntry::name_only("wt")])
        );

        // Re-register same name at a DIFFERENT path → stale worktrees reset.
        reg.register("proj", PathBuf::from("/b/proj"));
        assert_eq!(reg.projects["proj"].worktrees, None);
        assert_eq!(reg.projects["proj"].path, PathBuf::from("/b/proj"));
    }

    /// #46 regression: onboarding an UNRELATED new project must not drop another
    /// project's per-machine worktree list. Exercises the FULL on-disk
    /// load → mutate → save cycle the onboard command actually performs (not just
    /// an in-memory map insert), since the observed data-loss happened across a
    /// registry rewrite on disk.
    #[test]
    fn onboarding_a_new_project_preserves_other_projects_worktrees() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("reg.toml");

        // Project A is persisted with a per-machine worktree list.
        let mut reg = UserConfig::default();
        reg.projects.insert(
            "a".into(),
            ProjectRef {
                worktrees: Some(vec![
                    WorktreeEntry::name_only("w1"),
                    WorktreeEntry::name_only("w2"),
                ]),
                ..ProjectRef::new("/p/a".into())
            },
        );
        reg.set_active("a");
        reg.save_to(&f).unwrap();

        // Onboard B exactly as the command does: load from disk, register +
        // activate B, save back to disk.
        let mut loaded = UserConfig::load_from(&f).unwrap();
        loaded.register("b", PathBuf::from("/p/b"));
        loaded.set_active("b");
        loaded.save_to(&f).unwrap();

        // Reload from disk — A's worktrees must have survived the rewrite.
        let back = UserConfig::load_from(&f).unwrap();
        assert_eq!(
            back.projects["a"].worktrees,
            Some(vec![
                WorktreeEntry::name_only("w1"),
                WorktreeEntry::name_only("w2"),
            ]),
            "onboarding B must not drop A's per-machine worktrees"
        );
        assert_eq!(back.active.as_deref(), Some("b"));
    }

    // --- Unit 4b: registry-first resolve fixes the worktree db_path doubling ---

    #[test]
    fn resolve_from_registered_worktree_uses_main_root_db_path() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // Real-world layout: main checkout at <root>/keitora, linked worktrees under
        // <root>/keitora-worktrees/*, and the single shared DB at
        // <root>/keitora-worktrees/shared.
        let main = root.join("keitora");
        let sol = root.join("keitora-worktrees/sol");
        let shared_db = root.join("keitora-worktrees/shared/tasks.db");
        std::fs::create_dir_all(shared_db.parent().unwrap()).unwrap();
        std::fs::write(&shared_db, b"").unwrap();

        // The committed keitora.toml — identical in the main checkout and the
        // worktree — carries a relative db_path authored against the main
        // checkout. (`worktrees` lists "sol" so the registry knows it as a
        // worktree of the project.)
        let toml = "[project]\nname = \"keitora\"\nworktrees_dir = \"../keitora-worktrees\"\n\n\
                    [tasks]\ndb_path = \"../keitora-worktrees/shared/tasks.db\"\n\
                    worktrees = [\"keitora\", \"sol\"]\n";
        std::fs::create_dir_all(&main).unwrap();
        std::fs::write(main.join("keitora.toml"), toml).unwrap();
        std::fs::create_dir_all(&sol).unwrap();
        std::fs::write(sol.join("keitora.toml"), toml).unwrap();

        // Register the project's *main* root; isolate the registry to the temp dir.
        std::env::set_var("KEITORA_CONFIG_HOME", root.join("user_config"));
        let reg_path = root.join("user_config/keitora/keitora.toml");
        let mut reg = UserConfig::default();
        reg.register("keitora", main.clone());
        reg.save_to(&reg_path).unwrap();

        // Resolving from inside the sol worktree must root the config at the main
        // checkout, so the relative db_path resolves to the single shared DB —
        // NOT the doubled `…/keitora-worktrees/keitora-worktrees/shared/tasks.db` that a
        // bare cwd-walk against the worktree root would produce.
        let cfg = resolve_in(&sol).unwrap();
        // load() strips the Windows `\\?\` prefix from root/db_path; mirror it (no-op unix).
        assert_eq!(cfg.root, strip_extended_prefix(main.clone()));
        assert_eq!(
            cfg.tasks.db_path,
            strip_extended_prefix(shared_db.canonicalize().unwrap())
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    // --- Unit 5: worktree_path() shim ---

    #[test]
    fn worktree_path_shim_maps_main_and_others() {
        let pc = ProjectConfig {
            root: PathBuf::from("/Users/x/code/keitora"),
            effective_worktrees: None,
            project: ProjectMeta {
                name: "keitora".into(),
                worktrees_dir: Some("../keitora-worktrees".into()),
                base_branch: None,
                diff_base: None,
            },
            tasks: TaskConfig {
                db_path: PathBuf::from("/Users/x/code/keitora-worktrees/shared/tasks.db"),
                worktrees: vec![],
                archive_days: 30,
                log_retention_days: 90,
                backend: "sqlite".to_string(),
                github: None,
            },
            claude: ProviderConfig::default(),
            codex: ProviderConfig::default(),
            cli: CliConfig::default(),
            dispatch: DispatchConfig::default(),
        };
        let cfg = Config::from_project(pc);
        assert_eq!(
            cfg.worktree_path("keitora").unwrap(),
            PathBuf::from("/Users/x/code/keitora")
        );
        assert_eq!(
            cfg.worktree_path("sayo").unwrap(),
            PathBuf::from("/Users/x/code/keitora-worktrees/sayo")
        );
        assert_eq!(
            cfg.worktrees_root().unwrap(),
            PathBuf::from("/Users/x/code/keitora-worktrees")
        );

        // Main worktree name is the root basename, not a hard-coded "keitora".
        let pc2 = ProjectConfig {
            root: PathBuf::from("/Users/x/code/myrepo"),
            effective_worktrees: None,
            project: ProjectMeta {
                name: "myrepo".into(),
                worktrees_dir: Some("../myrepo-worktrees".into()),
                base_branch: None,
                diff_base: None,
            },
            tasks: TaskConfig {
                db_path: PathBuf::from("/Users/x/code/myrepo-worktrees/shared/tasks.db"),
                worktrees: vec![],
                archive_days: 30,
                log_retention_days: 90,
                backend: "sqlite".to_string(),
                github: None,
            },
            claude: ProviderConfig::default(),
            codex: ProviderConfig::default(),
            cli: CliConfig::default(),
            dispatch: DispatchConfig::default(),
        };
        let cfg2 = Config::from_project(pc2);
        assert_eq!(
            cfg2.worktree_path("myrepo").unwrap(),
            PathBuf::from("/Users/x/code/myrepo")
        );
        assert_eq!(
            cfg2.worktree_path("keitora").unwrap(),
            PathBuf::from("/Users/x/code/myrepo-worktrees/keitora")
        );
    }

    #[test]
    fn resolved_worktrees_dedupes_names() {
        let pc = ProjectConfig {
            root: PathBuf::from("/Users/x/code/keitora"),
            effective_worktrees: None,
            project: ProjectMeta {
                name: "keitora".into(),
                worktrees_dir: Some("../keitora-worktrees".into()),
                base_branch: None,
                diff_base: None,
            },
            tasks: TaskConfig {
                db_path: PathBuf::from("/Users/x/code/keitora-worktrees/shared/tasks.db"),
                worktrees: vec![
                    WorktreeEntry::name_only("sayo"),
                    WorktreeEntry::name_only("sayo"),
                    WorktreeEntry::name_only("sol"),
                ],
                archive_days: 30,
                log_retention_days: 90,
                backend: "sqlite".to_string(),
                github: None,
            },
            claude: ProviderConfig::default(),
            codex: ProviderConfig::default(),
            cli: CliConfig::default(),
            dispatch: DispatchConfig::default(),
        };
        let cfg = Config::from_project(pc);
        let names: Vec<String> = cfg
            .resolved_worktrees()
            .unwrap()
            .into_iter()
            .map(|w| w.name)
            .collect();
        assert_eq!(names, vec!["sayo".to_string(), "sol".to_string()]);
    }

    /// `resolved_worktrees` stamps each entry's resolved `cli`. An entry's
    /// explicit `cli` always wins over the loaded user default, so this stays
    /// deterministic regardless of the machine's `UserConfig` on disk.
    #[test]
    fn resolved_worktrees_populates_cli() {
        let codex_wt = WorktreeEntry {
            cli: Some(CliKind::Codex),
            ..WorktreeEntry::name_only("sayo")
        };
        let claude_wt = WorktreeEntry {
            cli: Some(CliKind::Claude),
            ..WorktreeEntry::name_only("sol")
        };
        let pc = ProjectConfig {
            root: PathBuf::from("/Users/x/code/keitora"),
            effective_worktrees: None,
            project: ProjectMeta {
                name: "keitora".into(),
                worktrees_dir: Some("../keitora-worktrees".into()),
                base_branch: None,
                diff_base: None,
            },
            tasks: TaskConfig {
                db_path: PathBuf::from("/Users/x/code/keitora-worktrees/shared/tasks.db"),
                worktrees: vec![codex_wt, claude_wt],
                archive_days: 30,
                log_retention_days: 90,
                backend: "sqlite".to_string(),
                github: None,
            },
            claude: ProviderConfig::default(),
            codex: ProviderConfig::default(),
            cli: CliConfig::default(),
            dispatch: DispatchConfig::default(),
        };
        let cfg = Config::from_project(pc);
        let resolved = cfg.resolved_worktrees().unwrap();
        let by_name: std::collections::HashMap<_, _> =
            resolved.into_iter().map(|w| (w.name, w.cli)).collect();
        assert_eq!(by_name.get("sayo"), Some(&CliKind::Codex));
        assert_eq!(by_name.get("sol"), Some(&CliKind::Claude));
    }

    /// `resolved_worktrees_with` takes the `default_cli` fallback from the
    /// config it is given, while an entry's explicit `cli` still wins.
    #[test]
    fn resolved_worktrees_with_uses_given_user_default() {
        let pinned = WorktreeEntry {
            cli: Some(CliKind::Claude),
            ..WorktreeEntry::name_only("sol")
        };
        let pc = ProjectConfig {
            root: PathBuf::from("/Users/x/code/keitora"),
            effective_worktrees: None,
            project: ProjectMeta {
                name: "keitora".into(),
                worktrees_dir: Some("../keitora-worktrees".into()),
                base_branch: None,
                diff_base: None,
            },
            tasks: TaskConfig {
                db_path: PathBuf::from("/Users/x/code/keitora-worktrees/shared/tasks.db"),
                worktrees: vec![WorktreeEntry::name_only("sayo"), pinned],
                archive_days: 30,
                log_retention_days: 90,
                backend: "sqlite".to_string(),
                github: None,
            },
            claude: ProviderConfig::default(),
            codex: ProviderConfig::default(),
            cli: CliConfig::default(),
            dispatch: DispatchConfig::default(),
        };
        let user = UserConfig {
            default_cli: Some(CliKind::Codex),
            ..UserConfig::default()
        };
        let resolved = Config::from_project(pc)
            .resolved_worktrees_with(&user)
            .unwrap();
        let by_name: std::collections::HashMap<_, _> =
            resolved.into_iter().map(|w| (w.name, w.cli)).collect();
        assert_eq!(by_name.get("sayo"), Some(&CliKind::Codex), "user default");
        assert_eq!(by_name.get("sol"), Some(&CliKind::Claude), "entry wins");
    }

    #[test]
    fn scaffold_has_canonical_defaults_and_tilde_db_path() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("acme");
        std::fs::create_dir_all(&root).unwrap();

        let cfg = ProjectConfig::scaffold(&root);
        assert_eq!(cfg.project.name, "acme");
        assert_eq!(
            cfg.project.worktrees_dir.as_deref(),
            Some("~/.keitora/acme"),
            "per-name under ~/.keitora"
        );
        assert_eq!(
            cfg.tasks.db_path,
            PathBuf::from("~/.keitora/acme/shared/tasks.db"),
            "shared DB under ~/.keitora/<name>/shared"
        );
        assert_eq!(cfg.tasks.worktrees, vec![WorktreeEntry::name_only("acme")]);
        // Canonical retention defaults — NOT swapped (regression guard).
        assert_eq!(cfg.tasks.archive_days, 30);
        assert_eq!(cfg.tasks.log_retention_days, 90);

        // write() keeps the literal `~` db_path on disk (expanded on read).
        cfg.write().unwrap();
        let raw = std::fs::read_to_string(root.join("keitora.toml")).unwrap();
        assert!(
            raw.contains("db_path = \"~/.keitora/acme/shared/tasks.db\""),
            "db_path must be written with the literal tilde; got:\n{raw}"
        );
        assert!(
            raw.contains("worktrees_dir = \"~/.keitora/acme\""),
            "worktrees_dir must be written with the literal tilde; got:\n{raw}"
        );
    }

    // --- set_project_settings_by_root: registry writer ---

    #[test]
    fn set_project_settings_by_root_sets_clears_and_matches() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("keitora/keitora.toml");
        let root = PathBuf::from("/code/acme");

        let mut reg = UserConfig::default();
        reg.register("acme", root.clone());
        // Pre-seed an override so we can prove Some(None) clears it.
        reg.projects.get_mut("acme").unwrap().archive_days = Some(99);
        reg.set_active("acme");
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path());
        // save_to anchors the file; set_project_settings_by_root saves via save().
        reg.save_to(&path).unwrap();

        let patch = ProjectSettingsPatch {
            db_path: Some(Some(PathBuf::from("custom/db.sqlite"))),
            worktrees_dir: Some(Some("~/wt".to_string())),
            archive_days: Some(None), // clear the pre-seeded 99
            log_retention_days: Some(Some(14)),
            dispatch_steps: Some(Some(vec!["/clear".to_string()])),
            codex_dispatch_steps: None,
            diff_base: None,
        };
        let matched = reg.set_project_settings_by_root(&root, patch).unwrap();
        assert!(matched, "matching root must report matched");

        let e = &reg.projects["acme"];
        assert_eq!(e.db_path.as_deref(), Some(Path::new("custom/db.sqlite")));
        assert_eq!(e.worktrees_dir.as_deref(), Some("~/wt"));
        assert!(e.archive_days.is_none(), "Some(None) clears the override");
        assert_eq!(e.log_retention_days, Some(14));
        assert_eq!(
            e.dispatch_steps.as_deref(),
            Some(["/clear".to_string()].as_slice())
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn set_project_settings_by_root_unmatched_returns_false_without_saving() {
        let mut reg = UserConfig::default();
        reg.register("acme", PathBuf::from("/code/acme"));
        // No KEITORA_CONFIG_HOME set: if this tried to save() it would error/write to a
        // real path. An unmatched root must short-circuit before save().
        let patch = ProjectSettingsPatch {
            archive_days: Some(Some(7)),
            ..Default::default()
        };
        let matched = reg
            .set_project_settings_by_root(Path::new("/code/other"), patch)
            .unwrap();
        assert!(!matched, "non-registered root must report unmatched");
        assert!(
            reg.projects["acme"].archive_days.is_none(),
            "no entry mutated"
        );
    }

    #[test]
    fn project_settings_patch_none_field_leaves_unchanged() {
        let mut pref = ProjectRef::new(PathBuf::from("/p"));
        pref.archive_days = Some(30);
        // A default patch (all fields None) must touch nothing.
        ProjectSettingsPatch::default().apply_to(&mut pref);
        assert_eq!(pref.archive_days, Some(30));
    }

    #[test]
    fn stamp_from_registry_applies_all_overrides_not_just_worktrees() {
        // A loaded (committed-file) config + a registry entry with full overrides:
        // stamp_from_registry must overlay every override field, not only worktrees
        // (so the env/cwd-walk resolution paths match effective_config).
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("keitora.toml"),
            "[project]\nname = \"proj\"\n\n[tasks]\ndb_path = \"committed/tasks.db\"\n\
             worktrees = [\"committed\"]\narchive_days = 30\nlog_retention_days = 90\n",
        )
        .unwrap();
        let mut cfg = ProjectConfig::load(&root).unwrap();

        let mut reg = UserConfig::default();
        reg.register("proj", cfg.root.clone());
        {
            let e = reg.projects.get_mut("proj").unwrap();
            e.archive_days = Some(7);
            e.log_retention_days = Some(14);
            e.worktrees_dir = Some("~/wt/proj".to_string());
            e.dispatch_steps = Some(vec!["/clear".to_string()]);
            e.worktrees = Some(vec![WorktreeEntry::name_only("registry-wt")]);
        }

        stamp_from_registry(&mut cfg, &reg);
        assert_eq!(cfg.tasks.archive_days, 7, "archive_days override applied");
        assert_eq!(cfg.tasks.log_retention_days, 14);
        assert_eq!(cfg.project.worktrees_dir.as_deref(), Some("~/wt/proj"));
        assert_eq!(cfg.dispatch.claude_steps, vec!["/clear".to_string()]);
        assert_eq!(cfg.worktrees(), vec!["registry-wt".to_string()]);
    }

    #[test]
    fn project_ref_by_root_matches_canonically() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let mut reg = UserConfig::default();
        reg.register("proj", root.clone());

        assert!(reg.project_ref_by_root(&root).is_some());
        // A trailing-slash / non-canonical form of the same dir still matches.
        assert!(reg.project_ref_by_root(&root.join(".")).is_some());
        assert!(reg.project_ref_by_root(tmp.path()).is_none());
    }

    // --- effective_config: keitora.toml optional + registry overlay ---

    #[test]
    fn effective_config_fileless_uses_scaffold_defaults() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("acme");
        std::fs::create_dir_all(&root).unwrap();
        // No keitora.toml is written.
        assert!(!root.join("keitora.toml").exists());

        let pref = ProjectRef::new(root.clone());
        let cfg = effective_config(&pref).unwrap();

        assert_eq!(cfg.project.name, "acme", "name = root basename");
        assert_eq!(cfg.tasks.archive_days, 30);
        assert_eq!(cfg.tasks.log_retention_days, 90);
        // db_path is the scaffold default, normalized to an absolute path exactly as
        // load() would produce — the normalization-drift guard.
        let want = normalize_db_path(
            &PathBuf::from("~/.keitora/acme/shared/tasks.db"),
            &canonicalize_root(&root),
        );
        assert_eq!(cfg.tasks.db_path, want);
        assert!(cfg.tasks.db_path.is_absolute());
        // The scaffold lists the main worktree; effective list falls through to it.
        assert_eq!(cfg.worktrees(), vec!["acme".to_string()]);
    }

    #[test]
    fn effective_config_registry_overrides_win_over_committed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("keitora.toml"),
            "[project]\nname = \"proj\"\nworktrees_dir = \"~/.keitora/proj\"\n\n\
             [tasks]\ndb_path = \"shared/tasks.db\"\nworktrees = [\"proj\"]\n\
             archive_days = 30\nlog_retention_days = 90\n\n\
             [dispatch]\nsteps = [\"/committed\"]\n",
        )
        .unwrap();

        // No overrides → committed values used.
        let bare = ProjectRef::new(root.clone());
        let cfg = effective_config(&bare).unwrap();
        assert_eq!(cfg.tasks.archive_days, 30);
        assert_eq!(cfg.tasks.log_retention_days, 90);
        assert_eq!(cfg.dispatch.claude_steps, vec!["/committed".to_string()]);

        // Overrides set → registry wins per field.
        let over = ProjectRef {
            db_path: Some(PathBuf::from("custom/db.sqlite")),
            worktrees_dir: Some("~/wt/proj".to_string()),
            archive_days: Some(7),
            log_retention_days: Some(14),
            dispatch_steps: Some(vec!["/clear".to_string(), "/park".to_string()]),
            ..ProjectRef::new(root.clone())
        };
        let cfg = effective_config(&over).unwrap();
        assert_eq!(cfg.tasks.archive_days, 7);
        assert_eq!(cfg.tasks.log_retention_days, 14);
        assert_eq!(cfg.project.worktrees_dir.as_deref(), Some("~/wt/proj"));
        assert_eq!(
            cfg.dispatch.claude_steps,
            vec!["/clear".to_string(), "/park".to_string()]
        );
        assert!(cfg.tasks.db_path.ends_with("custom/db.sqlite"));
        assert!(cfg.tasks.db_path.is_absolute());
    }

    #[test]
    fn effective_config_worktrees_semantics() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("keitora.toml"),
            "[project]\nname = \"proj\"\n\n[tasks]\ndb_path = \"t.db\"\n\
             worktrees = [\"committed\"]\n",
        )
        .unwrap();

        // None → committed fallback.
        let cfg = effective_config(&ProjectRef::new(root.clone())).unwrap();
        assert_eq!(cfg.worktrees(), vec!["committed".to_string()]);

        // Some([]) → explicitly empty, NOT the committed fallback.
        let empty = ProjectRef {
            worktrees: Some(vec![]),
            ..ProjectRef::new(root.clone())
        };
        assert!(effective_config(&empty).unwrap().worktrees().is_empty());
    }

    #[test]
    fn fileless_project_resolves_as_active() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let proj = root.join("fileless");
        std::fs::create_dir_all(&proj).unwrap();
        // No keitora.toml — registered file-less and marked active.
        std::env::set_var("KEITORA_CONFIG_HOME", root.join("user_config"));
        std::env::remove_var("KEITORA_PROJECT");
        let reg_path = root.join("user_config/keitora/keitora.toml");
        let mut reg = UserConfig::default();
        reg.register("fileless", proj.clone());
        reg.set_active("fileless");
        reg.save_to(&reg_path).unwrap();

        let cfg = resolve_active_in(|| proj.clone()).unwrap();
        assert_eq!(cfg.project.name, "fileless");
        assert!(cfg.tasks.db_path.is_absolute());

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    // --- Unit 5b: [dispatch] config section ---

    #[test]
    fn dispatch_config_is_empty_by_default_and_not_when_stepped() {
        assert!(DispatchConfig::default().is_empty());
        let cfg = DispatchConfig {
            claude_steps: vec!["/clear".to_string()],
            codex_steps: vec![],
        };
        assert!(!cfg.is_empty());
    }

    /// Writes a minimal valid project `keitora.toml` plus `extra` and returns its dir.
    fn minimal_project_toml(extra: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let base =
            "[project]\nname = \"proj\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n";
        std::fs::write(project.join("keitora.toml"), format!("{base}{extra}")).unwrap();
        (tmp, project)
    }

    #[test]
    fn legacy_daemon_table_in_project_config_is_ignored() {
        let (_tmp, project) = minimal_project_toml(
            "
[daemon]
scrollback_lines = 5000
idle_shutdown_secs = 60
",
        );
        let cfg = ProjectConfig::load(&project).unwrap();
        assert_eq!(cfg.project.name, "proj");
        assert_eq!(cfg.tasks.backend, "sqlite");
        assert!(cfg.tasks.worktrees.is_empty());
    }

    #[test]
    fn dispatch_section_round_trips() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("keitora.toml"),
            "[project]\nname = \"proj\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n\n\
             [dispatch]\nsteps = [\"/clear\", \"/park\", \"/workflow {task_number}\"]\n",
        )
        .unwrap();

        let cfg = ProjectConfig::load(&project).unwrap();
        assert_eq!(
            cfg.dispatch.claude_steps,
            vec![
                "/clear".to_string(),
                "/park".to_string(),
                "/workflow {task_number}".to_string()
            ]
        );
        assert!(!cfg.dispatch.is_empty());
    }

    #[test]
    fn no_dispatch_section_loads_empty() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("keitora.toml"),
            "[project]\nname = \"proj\"\n\n[tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n",
        )
        .unwrap();

        let cfg = ProjectConfig::load(&project).unwrap();
        assert!(cfg.dispatch.is_empty());
    }

    #[test]
    fn write_initial_appends_commented_dispatch_hint_only_when_empty() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("acme");
        std::fs::create_dir_all(&root).unwrap();

        // Creation (write_initial) with empty dispatch → commented example present,
        // and the file still parses.
        let cfg = ProjectConfig::scaffold(&root);
        cfg.write_initial().unwrap();
        let raw = std::fs::read_to_string(root.join("keitora.toml")).unwrap();
        assert!(
            raw.contains("# [dispatch]"),
            "hint should be present:\n{raw}"
        );
        let reloaded = ProjectConfig::load(&root).unwrap();
        assert!(reloaded.dispatch.is_empty());

        // A plain write() of the same empty config must NOT (re)introduce the hint —
        // it would clobber user edits / resurrect a deleted comment on every rewrite.
        cfg.write().unwrap();
        let raw_rewrite = std::fs::read_to_string(root.join("keitora.toml")).unwrap();
        assert!(
            !raw_rewrite.contains("# [dispatch]"),
            "plain write() must not append the hint:\n{raw_rewrite}"
        );

        // Configured dispatch → real table, no commented hint (even via write_initial).
        let mut configured = ProjectConfig::scaffold(&root);
        configured.dispatch.claude_steps = vec!["/clear".to_string()];
        configured.write_initial().unwrap();
        let raw2 = std::fs::read_to_string(root.join("keitora.toml")).unwrap();
        assert!(
            !raw2.contains("# [dispatch]"),
            "no hint when configured:\n{raw2}"
        );
        assert!(raw2.contains("[dispatch]"), "real section written:\n{raw2}");
        let reloaded2 = ProjectConfig::load(&root).unwrap();
        assert_eq!(reloaded2.dispatch.claude_steps, vec!["/clear".to_string()]);
    }

    // --- Unit 6: No-config case ---

    #[test]
    fn no_config_yields_not_found_and_needs_onboarding() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = TempDir::new().unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();

        std::env::remove_var("KEITORA_PROJECT");
        // Isolate the user registry from real machine state: point it at an
        // empty temp dir so the test never reads ~/Library/Application Support.
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path().join("user_config"));

        // resolve(): no env, no keitora.toml on the walk, no registry match → NotFound.
        assert!(matches!(
            resolve_in(&empty).unwrap_err(),
            ResolveError::NotFound
        ));

        // resolve_active(): no env, no active project, no registry match →
        // NeedsOnboarding (the empty cwd is not inside any registered project).
        let err = resolve_active_in(|| empty.clone()).unwrap_err();
        assert!(matches!(err, ResolveError::NeedsOnboarding), "got {err:?}");

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    #[test]
    fn classify_worktree_covers_states() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();

        assert_eq!(
            classify_worktree(&base.join("missing")),
            WorktreeStatus::Missing
        );

        let file = base.join("file");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(classify_worktree(&file), WorktreeStatus::NotDir);

        let nogit = base.join("nogit");
        std::fs::create_dir_all(&nogit).unwrap();
        assert_eq!(classify_worktree(&nogit), WorktreeStatus::NotGit);

        let git = base.join("git");
        std::fs::create_dir_all(git.join(".git")).unwrap();
        assert_eq!(classify_worktree(&git), WorktreeStatus::Available);
    }

    // --- Claude path: parsing, which-search, resolution precedence ---

    #[test]
    fn parse_marked_path_survives_greeting_noise() {
        // Simulate a fish-style interactive greeting before the bracketed value.
        let out = format!(
            "Welcome to fish, the friendly interactive shell\n{PATH_BEGIN}/opt/homebrew/bin:/usr/bin:/bin\n{PATH_END}"
        );
        assert_eq!(
            parse_marked_path(&out).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin:/bin")
        );
        // Missing markers → None (don't return garbage).
        assert_eq!(parse_marked_path("no markers here"), None);
        // Empty value between markers → None.
        assert_eq!(parse_marked_path(&format!("{PATH_BEGIN}{PATH_END}")), None);
    }

    // Exercises the unix execute-bit discriminator (`:`-separated PATH + 0o755);
    // the windows runnability/PATHEXT path is a stage-050 concern with its own test.
    #[cfg(unix)]
    #[test]
    fn find_in_path_returns_first_executable_match() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let bin1 = tmp.path().join("bin1");
        let bin2 = tmp.path().join("bin2");
        std::fs::create_dir_all(&bin1).unwrap();
        std::fs::create_dir_all(&bin2).unwrap();

        // A non-executable claude in bin1 must be skipped; the executable in bin2 wins.
        std::fs::write(bin1.join("claude"), b"#!/bin/sh\n").unwrap();
        let exe = bin2.join("claude");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path = format!("{}:{}", bin1.display(), bin2.display());
        assert_eq!(find_in_path(&path, "claude"), Some(exe));
        // Empty path segments are ignored; a missing name yields None.
        assert_eq!(find_in_path("::", "claude"), None);
    }

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[cfg(unix)]
    #[test]
    fn herdr_well_known_paths_are_herdr_in_the_shared_extra_dirs() {
        let paths = herdr_well_known_paths();
        assert_eq!(paths.len(), extra_path_dirs().len());
        assert!(paths.contains(&PathBuf::from("/opt/homebrew/bin/herdr")));
        assert!(paths.contains(&PathBuf::from("/usr/local/bin/herdr")));
        assert!(paths.iter().all(|p| p.ends_with("herdr")));
    }

    #[test]
    fn herdr_release_order_is_newest_by_version() {
        let releases = vec![
            pb("/r/0.9.3"),
            pb("/r/0.9.10"),
            pb("/r/junk"),
            pb("/r/0.10.0"),
            pb("/r/0.9.9"),
        ];
        assert_eq!(
            herdr_release_order(&releases, "herdr"),
            vec![
                pb("/r/0.10.0/herdr"),
                pb("/r/0.9.10/herdr"),
                pb("/r/0.9.9/herdr"),
                pb("/r/0.9.3/herdr"),
                pb("/r/junk/herdr"),
            ]
        );
    }

    #[test]
    fn herdr_release_order_dedups_and_names_the_platform_executable() {
        let twice = vec![pb("/r/1.0.0"), pb("/r/1.0.0")];
        assert_eq!(
            herdr_release_order(&twice, "herdr.exe"),
            vec![pb("/r/1.0.0/herdr.exe")]
        );
        assert!(herdr_release_order(&[], "herdr").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn herdr_on_path_is_the_first_executable_herdr() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let (a, b, c) = (
            tmp.path().join("a"),
            tmp.path().join("b"),
            tmp.path().join("c"),
        );
        // `a` holds a directory named herdr, `b` and `c` an executable.
        std::fs::create_dir_all(a.join("herdr")).unwrap();
        for d in [&b, &c] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("herdr"), b"x").unwrap();
            std::fs::set_permissions(d.join("herdr"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let path = format!("{}::{}:{}", a.display(), b.display(), c.display());
        assert_eq!(herdr_on_path(&path), Some(b.join("herdr")));
        assert_eq!(herdr_on_path(&a.to_string_lossy()), None);
        assert_eq!(herdr_on_path(""), None);
    }

    #[test]
    fn parse_release_version_reads_the_leading_triple() {
        assert_eq!(parse_release_version("0.9.10"), Some((0, 9, 10)));
        assert_eq!(parse_release_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_release_version("1.2.3-beta.1"), Some((1, 2, 3)));
        assert_eq!(parse_release_version("1.2"), None);
        assert_eq!(parse_release_version("latest"), None);
        assert!(parse_release_version("0.9.10") > parse_release_version("0.9.3"));
    }

    #[cfg(unix)]
    #[test]
    fn executable_predicate_rejects_a_directory_and_a_plain_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("herdr");
        std::fs::create_dir_all(&dir).unwrap();
        let plain = tmp.path().join("plain");
        std::fs::write(&plain, b"x").unwrap();
        let exe = tmp.path().join("exe");
        std::fs::write(&exe, b"x").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!is_executable_file(&dir));
        assert!(!is_executable_file(&plain));
        assert!(is_executable_file(&exe));
    }

    #[cfg(unix)]
    #[test]
    fn compose_spawn_path_orders_login_extra_inherited_without_duplicates() {
        let extra = vec!["/opt/homebrew/bin".to_string(), "/usr/bin".to_string()];
        // The user's shims lead their login PATH and must stay ahead of Homebrew.
        let got = compose_spawn_path(
            &extra,
            Some("/u/.asdf/shims:/usr/bin:/opt/homebrew/bin:/login/bin"),
            Some("/bin:/login/bin"),
        );
        assert_eq!(
            got.as_deref(),
            Some("/u/.asdf/shims:/usr/bin:/opt/homebrew/bin:/login/bin:/bin")
        );
        // An extra dir the login PATH lacks follows it, ahead of the process PATH.
        let got = compose_spawn_path(&extra, Some("/login/bin"), Some("/bin"));
        assert_eq!(
            got.as_deref(),
            Some("/login/bin:/opt/homebrew/bin:/usr/bin:/bin")
        );
        assert_eq!(
            compose_spawn_path(&extra, None, None).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        assert_eq!(compose_spawn_path(&[], None, None), None);
        assert_eq!(compose_spawn_path(&[], Some("::"), Some("")), None);
    }

    // Windows binary-resolution logic (shared by keitora_on_path + discover_claude).
    #[cfg(windows)]
    #[test]
    fn is_executable_file_windows_is_any_regular_file() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("keitora.exe");
        std::fs::write(&f, b"MZ").unwrap();
        assert!(
            is_executable_file(&f),
            "an existing regular file is runnable"
        );
        assert!(
            !is_executable_file(&tmp.path().join("missing.exe")),
            "a missing file is not runnable"
        );
        assert!(
            !is_executable_file(tmp.path()),
            "a directory is not a runnable file"
        );
    }

    #[cfg(windows)]
    #[test]
    fn find_on_path_windows_dir_then_ext_order() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let path = format!("{};{}", a.display(), b.display());
        let names = ["claude.cmd", "claude.exe", "claude.bat"];

        // Match only in the second dir → found there.
        std::fs::write(b.join("claude.cmd"), b"@echo off").unwrap();
        assert_eq!(
            find_on_path_windows(&path, &names),
            Some(b.join("claude.cmd"))
        );

        // A match in the first dir wins, regardless of its extension rank.
        std::fs::write(a.join("claude.exe"), b"MZ").unwrap();
        assert_eq!(
            find_on_path_windows(&path, &names),
            Some(a.join("claude.exe")),
            "the first PATH dir with any matching name wins"
        );

        // Empty segments ignored; no match → None.
        assert_eq!(find_on_path_windows(";;", &names), None);
    }

    // Windows CLI-alias resolution: prefer the bundled `keitora-cli.exe`, fall back to
    // a dev `keitora.exe` ONLY behind the `keitora-app.exe` rename guard.
    #[cfg(windows)]
    #[test]
    fn pick_cli_source_prefers_bundled_then_guarded_dev() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        // Empty dir → nothing to mirror.
        assert_eq!(pick_cli_source(d), None);
        // A bare dev `keitora.exe` without the `keitora-app.exe` marker is NOT trusted (it could
        // be a pre-rename GUI app).
        std::fs::write(d.join("keitora.exe"), b"MZ").unwrap();
        assert_eq!(pick_cli_source(d), None);
        // With the marker present, the dev CLI is selected.
        std::fs::write(d.join("keitora-app.exe"), b"MZ").unwrap();
        assert_eq!(pick_cli_source(d), Some(d.join("keitora.exe")));
        // The packaged sidecar wins over the dev build when both exist.
        std::fs::write(d.join("keitora-cli.exe"), b"MZ").unwrap();
        assert_eq!(pick_cli_source(d), Some(d.join("keitora-cli.exe")));
    }

    #[cfg(windows)]
    #[test]
    fn alias_needs_refresh_on_missing_or_size_change() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("keitora-cli.exe");
        let dst = tmp.path().join("keitora.exe");
        std::fs::write(&src, b"MZ-v1").unwrap();
        // dst missing → refresh.
        assert!(alias_needs_refresh(&src, &dst));
        // identical copy → no refresh.
        std::fs::copy(&src, &dst).unwrap();
        assert!(!alias_needs_refresh(&src, &dst));
        // different length → refresh.
        std::fs::write(&src, b"MZ-version-two-longer").unwrap();
        assert!(alias_needs_refresh(&src, &dst));
    }

    #[cfg(windows)]
    #[test]
    fn prepend_path_prepends_and_is_idempotent() {
        let dir = Path::new(r"C:\Users\x\AppData\Local\keitora\bin");
        // No / empty existing PATH → just the dir.
        assert_eq!(
            prepend_path(dir, None),
            r"C:\Users\x\AppData\Local\keitora\bin"
        );
        assert_eq!(
            prepend_path(dir, Some("")),
            r"C:\Users\x\AppData\Local\keitora\bin"
        );
        // Unrelated PATH → the dir goes first.
        assert_eq!(
            prepend_path(dir, Some(r"C:\Windows;C:\Windows\System32")),
            r"C:\Users\x\AppData\Local\keitora\bin;C:\Windows;C:\Windows\System32"
        );
        // Already the leading segment → returned unchanged (no accumulation on re-spawn).
        let already = r"C:\Users\x\AppData\Local\keitora\bin;C:\Windows";
        assert_eq!(prepend_path(dir, Some(already)), already);
    }

    // The copy/refresh/fallback branches of the alias install.
    #[cfg(windows)]
    #[test]
    fn refresh_alias_copies_when_missing_then_skips_when_fresh() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.exe");
        let bin = tmp.path().join("bin");
        let dst = bin.join("keitora.exe");
        std::fs::write(&src, b"V1").unwrap();
        // dst missing → creates bin dir + copies.
        refresh_alias(&src, &bin, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"V1");
        // fresh (same len + copied mtime) → no-op, and no temp left behind.
        refresh_alias(&src, &bin, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"V1");
        assert!(
            !bin.join("keitora.exe.new").exists(),
            "no orphaned temp file"
        );
    }

    #[cfg(windows)]
    #[test]
    fn refresh_alias_replaces_stale_dst() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.exe");
        let bin = tmp.path().join("bin");
        let dst = bin.join("keitora.exe");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(&dst, b"OLD-AND-LONGER").unwrap();
        std::fs::write(&src, b"NEW").unwrap();
        refresh_alias(&src, &bin, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"NEW");
    }

    #[cfg(windows)]
    #[test]
    fn refresh_alias_falls_back_to_existing_dst_when_src_unreadable() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("missing-src.exe"); // never created
        let bin = tmp.path().join("bin");
        let dst = bin.join("keitora.exe");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(&dst, b"EXISTING").unwrap();
        // src unreadable but a usable dst already exists → recoverable: keep it.
        refresh_alias(&src, &bin, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"EXISTING");
    }

    #[cfg(windows)]
    #[test]
    fn refresh_alias_errs_when_src_missing_and_no_dst() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("missing-src.exe");
        let bin = tmp.path().join("bin");
        let dst = bin.join("keitora.exe");
        // No source to copy and no prior dst → no usable alias.
        assert!(refresh_alias(&src, &bin, &dst).is_err());
        assert!(!dst.exists());
    }

    #[test]
    fn resolve_claude_path_prefers_project_then_user() {
        let mut user = UserConfig::default();
        // Nothing configured anywhere.
        assert_eq!(resolve_claude_path(None, &user), None);

        // User-level only.
        user.set_claude_path("/user/bin/claude");
        assert_eq!(
            resolve_claude_path(None, &user).as_deref(),
            Some("/user/bin/claude")
        );

        // Project override wins over the user-level default.
        let mut pc = ProjectConfig::scaffold(Path::new("/tmp/acme"));
        pc.claude.path = Some("/project/bin/claude".to_string());
        assert_eq!(
            resolve_claude_path(Some(&pc), &user).as_deref(),
            Some("/project/bin/claude")
        );
    }

    #[test]
    fn resolve_keitora_path_prefers_project_then_user_and_expands_tilde() {
        let mut user = UserConfig::default();
        // Nothing configured anywhere → no override (caller falls back to PATH).
        assert_eq!(resolve_keitora_path(None, &user), None);

        // User-level only.
        user.cli.path = Some("/user/bin/keitora".to_string());
        assert_eq!(
            resolve_keitora_path(None, &user).as_deref(),
            Some("/user/bin/keitora")
        );

        // Project override wins over the user-level default.
        let mut pc = ProjectConfig::scaffold(Path::new("/tmp/acme"));
        pc.cli.path = Some("/project/bin/keitora".to_string());
        assert_eq!(
            resolve_keitora_path(Some(&pc), &user).as_deref(),
            Some("/project/bin/keitora")
        );

        // `~` is expanded to the home directory.
        pc.cli.path = Some("~/bin/keitora".to_string());
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            resolve_keitora_path(Some(&pc), &user).as_deref(),
            Some(home.join("bin/keitora").to_string_lossy().as_ref())
        );
    }

    #[test]
    fn project_cli_table_round_trips_and_omits_when_empty() {
        // Empty `[cli]` table is omitted; a set path round-trips through TOML.
        let mut pc = ProjectConfig::scaffold(Path::new("/tmp/acme"));
        let toml_empty = toml::to_string(&pc).unwrap();
        assert!(
            !toml_empty.contains("[cli]"),
            "empty cli must be omitted; got:\n{toml_empty}"
        );

        pc.cli.path = Some("/abs/keitora".to_string());
        let toml_set = toml::to_string(&pc).unwrap();
        let back: ProjectConfig = toml::from_str(&toml_set).unwrap();
        assert_eq!(back.cli.path.as_deref(), Some("/abs/keitora"));
    }

    #[test]
    fn user_config_claude_round_trips_and_omits_when_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("keitora/keitora.toml");

        // Empty claude table is omitted from the serialized TOML.
        let empty = UserConfig::default();
        empty.save_to(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("[claude]"),
            "empty claude must be omitted; got:\n{raw}"
        );

        // A set path round-trips.
        let mut cfg = UserConfig::default();
        cfg.set_claude_path("/Users/x/.local/bin/claude");
        cfg.save_to(&path).unwrap();
        let loaded = UserConfig::load_from(&path).unwrap();
        assert_eq!(
            loaded.claude.path.as_deref(),
            Some("/Users/x/.local/bin/claude")
        );
    }

    // --- Tilde / absolute / default worktree path resolution ---

    #[test]
    fn expand_tilde_handles_tilde_absolute_and_relative() {
        let home = dirs::home_dir().expect("home dir available in test env");

        // Bare `~` → home.
        assert_eq!(expand_tilde("~"), home);
        // `~/x` → home/x (prefix is home; the literal `~` is gone).
        let tx = expand_tilde("~/x");
        assert_eq!(tx, home.join("x"));
        assert!(tx.starts_with(&home));
        // Absolute → verbatim.
        assert_eq!(expand_tilde("/abs"), PathBuf::from("/abs"));
        // Relative (no leading tilde) → verbatim.
        assert_eq!(expand_tilde("rel"), PathBuf::from("rel"));
        // A `~` not at the start is not expanded.
        assert_eq!(expand_tilde("a/~/b"), PathBuf::from("a/~/b"));
    }

    /// Build a `ProjectConfig` with an explicit `worktrees_dir` for resolution tests.
    fn pc_with_worktrees_dir(root: &Path, name: &str, dir: Option<&str>) -> ProjectConfig {
        ProjectConfig {
            root: root.to_path_buf(),
            effective_worktrees: None,
            project: ProjectMeta {
                name: name.to_string(),
                worktrees_dir: dir.map(str::to_string),
                base_branch: None,
                diff_base: None,
            },
            tasks: TaskConfig {
                db_path: PathBuf::from("shared/tasks.db"),
                worktrees: vec![],
                archive_days: 30,
                log_retention_days: 90,
                backend: "sqlite".to_string(),
                github: None,
            },
            claude: ProviderConfig::default(),
            codex: ProviderConfig::default(),
            cli: CliConfig::default(),
            dispatch: DispatchConfig::default(),
        }
    }

    #[test]
    fn worktrees_root_resolves_tilde_absolute_default_and_relative() {
        let home = dirs::home_dir().expect("home dir available in test env");
        let root = PathBuf::from("/Users/x/code/keitora");

        // Some("~/.keitora/foo") → starts with home, ends with .keitora/foo.
        let tilde =
            pc_with_worktrees_dir(&root, "keitora", Some("~/.keitora/foo")).worktrees_root();
        assert!(tilde.starts_with(&home), "expected under home: {tilde:?}");
        assert!(
            tilde.ends_with(".keitora/foo"),
            "expected .keitora/foo tail: {tilde:?}"
        );

        // Some("/tmp/abs") → used verbatim (absolute replaces root).
        let abs = pc_with_worktrees_dir(&root, "keitora", Some("/tmp/abs")).worktrees_root();
        assert_eq!(abs, PathBuf::from("/tmp/abs"));

        // None → ~/.keitora/<project name>.
        let dflt = pc_with_worktrees_dir(&root, "myproj", None).worktrees_root();
        assert_eq!(dflt, home.join(".keitora/myproj"));

        // Some("../rel") → joined under root, normalized (existing behavior).
        let rel = pc_with_worktrees_dir(&root, "keitora", Some("../rel")).worktrees_root();
        assert_eq!(rel, PathBuf::from("/Users/x/code/rel"));
    }

    #[test]
    fn scaffold_round_trips_to_expanded_absolute_paths() {
        let home = dirs::home_dir().expect("home dir available in test env");
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("acme");
        std::fs::create_dir_all(&root).unwrap();

        // scaffold() emits tilde literals; write() preserves them on disk.
        let cfg = ProjectConfig::scaffold(&root);
        cfg.write().unwrap();

        // Re-parse from disk: db_path and worktrees_root resolve to the expanded
        // absolute paths under home (the on-disk DB doesn't exist, so the joined
        // path is kept rather than canonicalized).
        let loaded = ProjectConfig::load(&root).unwrap();
        assert_eq!(
            loaded.tasks.db_path,
            home.join(".keitora/acme/shared/tasks.db")
        );
        assert_eq!(loaded.worktrees_root(), home.join(".keitora/acme"));
    }

    #[test]
    fn existing_relative_worktrees_dir_still_resolves_under_root() {
        // Regression: a committed toml with the repo's own legacy
        // `worktrees_dir = "../keitora-worktrees"` must keep resolving against root
        // (the repo's own config must be unaffected by the new default).
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("keitora");
        write_project(&project, "keitora", &["keitora"], "../keitora-worktrees");

        let cfg = ProjectConfig::load(&project).unwrap();
        assert_eq!(
            cfg.worktrees_root(),
            // worktrees_root strips the Windows `\\?\` prefix; mirror that in the
            // expected so the comparison holds on Windows (no-op on Unix).
            strip_extended_prefix(normalize(&root.join("keitora-worktrees")))
        );
    }

    #[test]
    fn legacy_pathless_entries_resolve_under_worktrees_dir() {
        // A committed config whose entries are bare names (path: None) + a
        // `worktrees_dir` resolves each non-main name under the worktrees root,
        // exactly as the legacy reconstruction did. Mirrors the keitora-repo case.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("keitora");
        write_project(
            &project,
            "keitora",
            &["keitora", "sayo"],
            "../keitora-worktrees",
        );

        let cfg = ProjectConfig::load(&project).unwrap();
        // Entries deserialized from bare strings → path: None.
        assert_eq!(
            cfg.tasks.worktrees,
            vec![
                WorktreeEntry::name_only("keitora"),
                WorktreeEntry::name_only("sayo"),
            ]
        );
        // Main worktree → root; a path-less linked name → worktrees_root/<name>.
        assert_eq!(cfg.worktree_path("keitora"), cfg.root.clone());
        assert_eq!(cfg.worktree_path("sayo"), cfg.worktrees_root().join("sayo"));
    }

    /// Run a git command in `dir`, panicking with stderr on failure.
    #[cfg(test)]
    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .no_window()
            .output()
            .expect("running git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn fileless_project_resolves_scattered_worktree_paths_as_available() {
        // The path-resolution bug, as a regression test. A file-less project (no
        // `worktrees_dir`, so the legacy reconstruction would point at
        // `~/.keitora/<name>/<wt>`) whose worktrees live at SCATTERED, non-default
        // paths. Persisted as entries carrying their explicit paths,
        // `worktree_path()` / `resolved_worktrees()` must return the STORED paths
        // and classify them Available — NOT `~/.keitora/...` → Missing.
        let tmp = TempDir::new().unwrap();
        let base = strip_extended_prefix(tmp.path().canonicalize().unwrap());

        // A real git repo as the project root (the "main" worktree).
        let root = base.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        git_in(&root, &["init", "-q"]);
        git_in(&root, &["config", "user.name", "Tester"]);
        git_in(&root, &["config", "user.email", "t@example.com"]);
        git_in(&root, &["config", "commit.gpgsign", "false"]);
        git_in(&root, &["checkout", "-q", "-b", "main"]);
        std::fs::write(root.join("README.md"), b"hi\n").unwrap();
        git_in(&root, &["add", "."]);
        git_in(&root, &["commit", "-q", "-m", "init"]);

        // Two linked worktrees at DELIBERATELY scattered locations — they share no
        // common `worktrees_dir` parent (the shared worktrees_dir parent case).
        let delta = base.join("proj-worktrees").join("delta");
        let gamma = base.join("scattered").join(".wt").join("gamma");
        std::fs::create_dir_all(delta.parent().unwrap()).unwrap();
        std::fs::create_dir_all(gamma.parent().unwrap()).unwrap();
        git_in(
            &root,
            &["worktree", "add", "-b", "wt/delta", delta.to_str().unwrap()],
        );
        git_in(
            &root,
            &["worktree", "add", "-b", "wt/gamma", gamma.to_str().unwrap()],
        );

        // A file-less project config (scaffold defaults: NO `worktrees_dir`, so the
        // legacy fallback would point at `~/.keitora/proj/<name>`). The worktree
        // entries carry their real, scattered paths.
        let mut pc = ProjectConfig::scaffold(&root);
        pc.root = canonicalize_root(&root);
        pc.effective_worktrees = Some(vec![
            WorktreeEntry::name_only("proj"), // main → root
            WorktreeEntry::new("delta", delta.clone()),
            WorktreeEntry::new("gamma", gamma.clone()),
        ]);

        // worktree_path returns the STORED scattered paths, not ~/.keitora/...
        assert_eq!(pc.worktree_path("delta"), delta);
        assert_eq!(pc.worktree_path("gamma"), gamma);
        let home_keitora = expand_tilde("~/.keitora");
        assert!(
            !pc.worktree_path("delta").starts_with(&home_keitora),
            "must NOT reconstruct under ~/.keitora"
        );

        // resolved_worktrees() classifies them Available at the stored paths.
        let shim = Config::from_project(pc);
        let resolved = shim.resolved_worktrees().unwrap();
        let by_name = |n: &str| resolved.iter().find(|w| w.name == n).unwrap();
        assert_eq!(by_name("delta").path, delta);
        assert_eq!(by_name("delta").status, WorktreeStatus::Available);
        assert_eq!(by_name("gamma").path, gamma);
        assert_eq!(by_name("gamma").status, WorktreeStatus::Available);
        // The main worktree resolves to root and is Available.
        assert_eq!(by_name("proj").path, shim.main_worktree_path());
        assert_eq!(by_name("proj").status, WorktreeStatus::Available);
    }

    #[test]
    fn registry_match_resolves_cwd_inside_scattered_worktree_path() {
        // Fix 1: a file-less project (no committed keitora.toml, so no
        // `worktrees_dir` to reconstruct from) whose registry worktree entry
        // carries an explicit, SCATTERED path that is a sibling of the project
        // root. Running from inside that scattered worktree must resolve the
        // project — `registry_match` has to honor the STORED path, not the legacy
        // `worktrees_dir`-based reconstruction (which would point at `~/.keitora/...`
        // and never contain the cwd).
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = TempDir::new().unwrap();
        let base = strip_extended_prefix(tmp.path().canonicalize().unwrap());

        // Real git repo as the project root (the "main" worktree); no keitora.toml.
        let root = base.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        git_in(&root, &["init", "-q"]);
        git_in(&root, &["config", "user.name", "Tester"]);
        git_in(&root, &["config", "user.email", "t@example.com"]);
        git_in(&root, &["config", "commit.gpgsign", "false"]);
        git_in(&root, &["checkout", "-q", "-b", "main"]);
        std::fs::write(root.join("README.md"), b"hi\n").unwrap();
        git_in(&root, &["add", "."]);
        git_in(&root, &["commit", "-q", "-m", "init"]);

        // A linked worktree at a scattered location — a SIBLING of the project
        // root, sharing no `worktrees_dir` parent with it.
        let delta = base.join("proj-worktrees").join("delta");
        std::fs::create_dir_all(delta.parent().unwrap()).unwrap();
        git_in(
            &root,
            &["worktree", "add", "-b", "wt/delta", delta.to_str().unwrap()],
        );

        // Register the file-less project and pin the worktree list (registry
        // authority) with the explicit scattered path.
        std::env::set_var("KEITORA_CONFIG_HOME", base.join("user_config"));
        let reg_path = base.join("user_config/keitora/keitora.toml");
        let mut reg = UserConfig::default();
        reg.register("proj", root.clone());
        reg.projects.get_mut("proj").unwrap().worktrees = Some(vec![
            WorktreeEntry::name_only("proj"), // main → root
            WorktreeEntry::new("delta", delta.clone()),
        ]);
        reg.save_to(&reg_path).unwrap();

        // Sanity: the legacy reconstruction does NOT contain the scattered cwd,
        // so resolution succeeds only because the stored path is honored.
        let cfg = effective_config(reg.projects.get("proj").unwrap()).unwrap();
        assert!(
            !cfg.worktrees_root().join("delta").starts_with(&delta),
            "the worktrees_dir reconstruction must not point at the scattered path"
        );

        // Resolving with cwd inside the scattered worktree returns the project.
        let resolved = resolve_in(&delta).unwrap();
        assert_eq!(resolved.project.name, "proj");
        assert_eq!(resolved.root, strip_extended_prefix(root.clone()));

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    // -------------------------------------------------------------------------
    // [tasks.github] config
    // -------------------------------------------------------------------------

    /// A `[tasks]` block with no `[tasks.github]` section parses with `github == None`.
    /// Existing keitora.toml files are unaffected.
    #[test]
    fn tasks_without_github_section_parses_github_none() {
        let toml_str = r#"
[project]
name = "rpg-game"
worktrees_dir = "~/.keitora/rpg-game"

[tasks]
db_path = "~/.keitora/rpg-game/shared/tasks.db"
"#;
        let pc: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(
            pc.tasks.github.is_none(),
            "expected github to be None when [tasks.github] is absent"
        );
        // Pre-existing fields are unaffected.
        assert_eq!(pc.tasks.archive_days, 30); // default
        assert_eq!(pc.tasks.backend, "sqlite"); // default
    }

    /// A full `[tasks.github]` block with owner/repo/project_number/status_map
    /// round-trips through TOML serialisation unchanged.
    #[test]
    fn github_task_config_round_trips() {
        use std::collections::BTreeMap;
        let toml_str = r#"
[project]
name = "rpg"
worktrees_dir = "~/.keitora/rpg"

[tasks]
db_path = "~/.keitora/rpg/shared/tasks.db"
backend = "github"

[tasks.github]
owner = "acme"
repo = "rpg-game"
project_number = 3
sidecar_path = "github-dispatch.db"

[tasks.github.status_map]
active = "In progress"
open = "Todo"
"#;
        let pc: ProjectConfig = toml::from_str(toml_str).unwrap();
        let gh = pc
            .tasks
            .github
            .as_ref()
            .expect("[tasks.github] should be Some");
        assert_eq!(gh.owner.as_deref(), Some("acme"));
        assert_eq!(gh.repo.as_deref(), Some("rpg-game"));
        assert_eq!(gh.project_number, Some(3));
        assert_eq!(gh.sidecar_path, Some(PathBuf::from("github-dispatch.db")));
        let mut expected_map = BTreeMap::new();
        expected_map.insert("active".to_string(), "In progress".to_string());
        expected_map.insert("open".to_string(), "Todo".to_string());
        assert_eq!(gh.status_map, expected_map);

        // Round-trip: serialize then re-parse; fields survive.
        let serialized = toml::to_string_pretty(&pc).unwrap();
        assert!(
            serialized.contains("owner = \"acme\""),
            "serialized toml missing owner: {serialized}"
        );
        assert!(
            serialized.contains("project_number = 3"),
            "serialized toml missing project_number: {serialized}"
        );
        assert!(
            serialized.contains("active = \"In progress\""),
            "serialized toml missing status_map entry: {serialized}"
        );

        let pc2: ProjectConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(pc.tasks.github, pc2.tasks.github);
    }

    /// Adding a `[tasks.github]` block does not disturb `db_path` or `worktrees`.
    #[test]
    fn github_section_does_not_affect_db_path_and_worktrees() {
        let toml_str = r#"
[project]
name = "acme"
worktrees_dir = "~/.keitora/acme"

[tasks]
db_path = "~/.keitora/acme/shared/tasks.db"
worktrees = ["acme", "feature"]

[tasks.github]
owner = "acme-corp"
"#;
        let pc: ProjectConfig = toml::from_str(toml_str).unwrap();
        // The db_path and worktrees are preserved exactly.
        assert!(
            pc.tasks.db_path.ends_with("acme/shared/tasks.db"),
            "db_path changed: {:?}",
            pc.tasks.db_path
        );
        assert_eq!(
            pc.tasks.worktrees,
            vec![
                WorktreeEntry::name_only("acme"),
                WorktreeEntry::name_only("feature"),
            ]
        );
        // github field is populated.
        assert!(pc.tasks.github.is_some());
    }

    /// `GitHubTaskConfig::default()` produces all-None/empty fields (required for
    /// `#[serde(default)]` to work correctly).
    #[test]
    fn github_task_config_default_is_all_none() {
        let gh = GitHubTaskConfig::default();
        assert!(gh.owner.is_none());
        assert!(gh.repo.is_none());
        assert!(gh.project_number.is_none());
        assert!(gh.sidecar_path.is_none());
        assert!(gh.status_map.is_empty());
    }

    // -------------------------------------------------------------------------
    // sidecar_path resolution
    // -------------------------------------------------------------------------

    /// When `[tasks.github] sidecar_path` is set, `resolve_sidecar_path` returns
    /// it resolved against the project root (for relative paths).
    #[test]
    fn resolve_sidecar_path_uses_explicit_when_set() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let mut pc = ProjectConfig::scaffold(&root);
        pc.root = root.clone();
        pc.tasks.github = Some(GitHubTaskConfig {
            sidecar_path: Some(PathBuf::from("dispatch.db")),
            ..Default::default()
        });

        let path = pc.resolve_sidecar_path().unwrap();
        // Relative "dispatch.db" resolves against root.
        assert_eq!(path, root.join("dispatch.db"));
    }

    /// When `sidecar_path` is absent, the default path is under the user config
    /// dir at `keitora/sidecars/<project-name>.db`.
    #[test]
    fn resolve_sidecar_path_default_uses_registry_dir() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        // Redirect KEITORA_CONFIG_HOME so the test is hermetic.
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path());

        let tmp2 = TempDir::new().unwrap();
        let root = tmp2.path().canonicalize().unwrap();
        let mut pc = ProjectConfig::scaffold(&root);
        pc.project.name = "my-project".to_string();
        // No explicit sidecar_path.
        pc.tasks.github = Some(GitHubTaskConfig::default());

        let path = pc.resolve_sidecar_path().unwrap();
        assert_eq!(
            path,
            tmp.path()
                .join("keitora")
                .join("sidecars")
                .join("my-project.db")
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    /// When `[tasks.github]` is entirely absent, `resolve_sidecar_path` still
    /// returns the default path (falling through to the registry dir branch).
    #[test]
    fn resolve_sidecar_path_works_without_github_section() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path());

        let tmp2 = TempDir::new().unwrap();
        let root = tmp2.path().canonicalize().unwrap();
        let mut pc = ProjectConfig::scaffold(&root);
        pc.project.name = "no-github".to_string();
        // github field is None (default for scaffold).
        assert!(pc.tasks.github.is_none());

        let path = pc.resolve_sidecar_path().unwrap();
        assert_eq!(
            path,
            tmp.path()
                .join("keitora")
                .join("sidecars")
                .join("no-github.db")
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    // --- WorktreeEntry serde ---

    /// (a) A TOML array mixing a bare string and a `{name,path}` table
    /// deserializes to the two expected entries.
    #[test]
    fn worktree_entry_deserialize_mixed_array() {
        #[derive(Deserialize)]
        struct Wrap {
            list: Vec<WorktreeEntry>,
        }
        let toml = r#"
            list = ["delta", { name = "gamma", path = "D:\\repos\\keitora-worktrees\\gamma" }]
        "#;
        let wrap: Wrap = toml::from_str(toml).unwrap();
        assert_eq!(
            wrap.list,
            vec![
                WorktreeEntry::name_only("delta"),
                WorktreeEntry::new("gamma", "D:\\repos\\keitora-worktrees\\gamma"),
            ]
        );
    }

    /// (a2) Fix 5: a hand-edited path-less inline table
    /// (`{ name = "x" }`) deserializes to `{name: "x", path: None}` rather than
    /// failing the whole config load (the `Table.path` field is optional).
    #[test]
    fn worktree_entry_deserialize_pathless_table() {
        #[derive(Deserialize)]
        struct Wrap {
            list: Vec<WorktreeEntry>,
        }
        let toml = r#"
            list = [{ name = "x" }]
        "#;
        let wrap: Wrap = toml::from_str(toml).unwrap();
        assert_eq!(wrap.list, vec![WorktreeEntry::name_only("x")]);
        assert_eq!(wrap.list[0].path, None);
    }

    /// (b) Round-trip a MIXED vec (a name-only + a name+path) via
    /// serialize → deserialize and assert equality. This is the linchpin risk.
    #[test]
    fn worktree_entry_roundtrip_mixed_vec() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            list: Vec<WorktreeEntry>,
        }
        let input = vec![
            WorktreeEntry::name_only("a"),
            WorktreeEntry::new("b", "/p/b"),
        ];
        let serialized = toml::to_string(&Wrap {
            list: input.clone(),
        })
        .unwrap();
        let back: Wrap = toml::from_str(&serialized).unwrap();
        assert_eq!(back.list, input, "serialized form was:\n{serialized}");
    }

    /// (c) Round-trip an all-`Some` vec → all inline tables → deserialize equal.
    #[test]
    fn worktree_entry_roundtrip_all_some() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            list: Vec<WorktreeEntry>,
        }
        let input = vec![
            WorktreeEntry::new("a", "/p/a"),
            WorktreeEntry::new("b", "D:\\repos\\b"),
        ];
        let serialized = toml::to_string(&Wrap {
            list: input.clone(),
        })
        .unwrap();
        let back: Wrap = toml::from_str(&serialized).unwrap();
        assert_eq!(back.list, input, "serialized form was:\n{serialized}");
    }

    /// (d) Round-trip a vec INSIDE a struct field with `#[serde(default)]`
    /// (mimics the real `ProjectRef`/registry serialization path) via
    /// `toml::to_string` + `toml::from_str`.
    #[test]
    fn worktree_entry_roundtrip_in_struct_field() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct ProjectRefLike {
            path: PathBuf,
            #[serde(default)]
            worktrees: Vec<WorktreeEntry>,
        }
        let input = ProjectRefLike {
            path: PathBuf::from("D:\\repos\\proj"),
            worktrees: vec![
                WorktreeEntry::name_only("delta"),
                WorktreeEntry::new("gamma", "D:\\repos\\proj-worktrees\\gamma"),
            ],
        };
        let serialized = toml::to_string(&input).unwrap();
        let back: ProjectRefLike = toml::from_str(&serialized).unwrap();
        assert_eq!(back, input, "serialized form was:\n{serialized}");

        // The `#[serde(default)]` key may also be omitted entirely.
        let omitted: ProjectRefLike = toml::from_str("path = \"D:\\\\repos\\\\x\"\n").unwrap();
        assert!(omitted.worktrees.is_empty());
    }

    // --- CliKind, ProviderConfig, per-CLI dispatch, resolvers ---

    /// I1 regression: a path-less entry carrying `cli`/`codex_thread_id` must
    /// serialize through the inline-table form (not collapse to a bare string and
    /// silently drop those fields) and round-trip equal.
    #[test]
    fn worktree_entry_pathless_with_cli_serializes_as_table() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            list: Vec<WorktreeEntry>,
        }
        let entry = WorktreeEntry {
            name: "delta".to_string(),
            path: None,
            cli: Some(CliKind::Codex),
            codex_thread_id: Some("thread-abc".to_string()),
            claude_session_id: None,
            diff_base: None,
        };
        let serialized = toml::to_string(&Wrap {
            list: vec![entry.clone()],
        })
        .unwrap();
        // Must be the table form, not a bare `"delta"` string.
        assert!(
            serialized.contains("name = \"delta\""),
            "path-less entry with cli must serialize as a table:\n{serialized}"
        );
        assert!(serialized.contains("cli = \"codex\""), "got:\n{serialized}");
        assert!(
            serialized.contains("codex_thread_id = \"thread-abc\""),
            "got:\n{serialized}"
        );
        let back: Wrap = toml::from_str(&serialized).unwrap();
        assert_eq!(
            back.list,
            vec![entry],
            "round-trip must preserve all fields"
        );
    }

    #[test]
    fn worktree_entry_pathless_with_claude_session_id_serializes_as_table() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            list: Vec<WorktreeEntry>,
        }
        let mut entry = WorktreeEntry::name_only("delta");
        entry.claude_session_id = Some("sess-uuid-1".to_string());
        let serialized = toml::to_string(&Wrap {
            list: vec![entry.clone()],
        })
        .unwrap();
        assert!(
            serialized.contains("name = \"delta\""),
            "path-less entry with claude_session_id must serialize as a table:\n{serialized}"
        );
        assert!(
            serialized.contains("claude_session_id = \"sess-uuid-1\""),
            "got:\n{serialized}"
        );
        let back: Wrap = toml::from_str(&serialized).unwrap();
        assert_eq!(
            back.list,
            vec![entry],
            "round-trip must preserve claude_session_id"
        );
    }

    /// I2: `set_project_worktrees_by_root` must preserve an existing entry's
    /// `cli`/`codex_thread_id` when the incoming entry of the same name omits them
    /// (the `host::set_worktrees` `{name, path}`-only scenario).
    #[test]
    fn set_project_worktrees_by_root_merges_registry_only_fields() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path());
        let root = PathBuf::from("/code/acme");

        let mut reg = UserConfig::default();
        reg.register("acme", root.clone());
        // Seed an existing worktree carrying registry-only CLI fields.
        reg.projects.get_mut("acme").unwrap().worktrees = Some(vec![WorktreeEntry {
            name: "gamma".to_string(),
            path: Some(PathBuf::from("/code/acme-wt/gamma")),
            cli: Some(CliKind::Codex),
            codex_thread_id: Some("keep-me".to_string()),
            claude_session_id: Some("sid-keep".to_string()),
            diff_base: None,
        }]);

        // Incoming edit carries only {name, path} — as the Settings UI would send.
        let incoming = vec![WorktreeEntry::new("gamma", "/code/acme-wt/gamma")];
        let matched = reg.set_project_worktrees_by_root(&root, &incoming).unwrap();
        assert!(matched, "matching root must report matched");

        let merged = &reg.projects["acme"].worktrees.as_ref().unwrap()[0];
        assert_eq!(merged.cli, Some(CliKind::Codex), "cli must survive rewrite");
        assert_eq!(
            merged.codex_thread_id.as_deref(),
            Some("keep-me"),
            "codex_thread_id must survive rewrite"
        );
        assert_eq!(
            merged.claude_session_id.as_deref(),
            Some("sid-keep"),
            "claude_session_id must survive rewrite"
        );

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    /// Incoming values win over the existing ones when both are present.
    #[test]
    fn set_project_worktrees_by_root_incoming_cli_wins() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        std::env::set_var("KEITORA_CONFIG_HOME", tmp.path());
        let root = PathBuf::from("/code/acme2");

        let mut reg = UserConfig::default();
        reg.register("acme2", root.clone());
        reg.projects.get_mut("acme2").unwrap().worktrees = Some(vec![WorktreeEntry {
            name: "gamma".to_string(),
            path: None,
            cli: Some(CliKind::Claude),
            codex_thread_id: None,
            claude_session_id: None,
            diff_base: None,
        }]);

        let incoming = vec![WorktreeEntry {
            name: "gamma".to_string(),
            path: None,
            cli: Some(CliKind::Codex),
            codex_thread_id: None,
            claude_session_id: None,
            diff_base: None,
        }];
        reg.set_project_worktrees_by_root(&root, &incoming).unwrap();

        let merged = &reg.projects["acme2"].worktrees.as_ref().unwrap()[0];
        assert_eq!(merged.cli, Some(CliKind::Codex), "incoming cli must win");

        std::env::remove_var("KEITORA_CONFIG_HOME");
    }

    /// Alias coverage: an existing `[claude] skip_permissions` + `[dispatch] steps`
    /// still deserialize (into `autonomy` / `claude_steps`) and the config
    /// re-serializes with the new key names, round-tripping unchanged values.
    #[test]
    fn legacy_claude_and_dispatch_aliases_deserialize() {
        let toml_text = "\
            [project]\nname = \"p\"\n\n\
            [tasks]\ndb_path = \"tasks.db\"\nworktrees = []\n\n\
            [claude]\nskip_permissions = true\n\n\
            [dispatch]\nsteps = [\"/clear\", \"/park\"]\n";
        let pc: ProjectConfig = toml::from_str(toml_text).unwrap();
        assert_eq!(pc.claude.autonomy, Some(true), "skip_permissions alias");
        assert_eq!(
            pc.dispatch.claude_steps,
            vec!["/clear".to_string(), "/park".to_string()],
            "steps alias"
        );
        // Re-serialize: values preserved, new key names emitted.
        let out = toml::to_string(&pc).unwrap();
        let back: ProjectConfig = toml::from_str(&out).unwrap();
        assert_eq!(back.claude.autonomy, Some(true));
        assert_eq!(
            back.dispatch.claude_steps,
            vec!["/clear".to_string(), "/park".to_string()]
        );
    }

    /// A fully-populated `[claude]` + `[codex]` config round-trips; an unconfigured
    /// `[codex]` table is omitted from freshly-written TOML (mirrors `[claude]`).
    #[test]
    fn codex_table_roundtrips_and_omits_when_empty() {
        let mut pc = ProjectConfig::scaffold(Path::new("/tmp/p"));
        // Unconfigured codex → omitted.
        let out_empty = toml::to_string(&pc).unwrap();
        assert!(
            !out_empty.contains("[codex]"),
            "empty [codex] must be omitted:\n{out_empty}"
        );

        pc.claude.path = Some("/bin/claude".to_string());
        pc.codex.path = Some("/bin/codex".to_string());
        pc.codex.autonomy = Some(true);
        let out = toml::to_string(&pc).unwrap();
        assert!(out.contains("[codex]"), "populated [codex] written:\n{out}");
        let back: ProjectConfig = toml::from_str(&out).unwrap();
        assert_eq!(back.codex.path.as_deref(), Some("/bin/codex"));
        assert_eq!(back.codex.autonomy, Some(true));
        assert_eq!(back.claude.path.as_deref(), Some("/bin/claude"));
    }

    /// `CliKind` serializes to the bare snake_case strings.
    #[test]
    fn cli_kind_serializes_snake_case() {
        assert_eq!(
            toml::Value::try_from(CliKind::Claude).unwrap().as_str(),
            Some("claude")
        );
        assert_eq!(
            toml::Value::try_from(CliKind::Codex).unwrap().as_str(),
            Some("codex")
        );
        assert_eq!(CliKind::default(), CliKind::Claude);
    }

    #[test]
    fn claude_session_id_accepts_only_safe_shell_tokens() {
        assert!(is_safe_claude_session_id(
            "1b4f1234-5678-90ab-cdef-1234567890ab"
        ));
        assert!(is_safe_claude_session_id("agent_session-1"));

        for invalid in [
            "",
            " ",
            " session-id",
            "session id",
            "--continue",
            "session;whoami",
            "session|whoami",
            "session&whoami",
            "session/id",
            "session.id",
        ] {
            assert!(!is_safe_claude_session_id(invalid), "accepted {invalid:?}");
        }
        assert!(!is_safe_claude_session_id(&"a".repeat(129)));
    }

    /// `resolve_cli` precedence: worktree entry `cli` → user `default_cli` → Claude.
    #[test]
    fn resolve_cli_precedence() {
        let user_default_codex = UserConfig {
            default_cli: Some(CliKind::Codex),
            ..UserConfig::default()
        };
        let user_unset = UserConfig::default();

        // Unset everywhere → Claude.
        assert_eq!(resolve_cli(None, &user_unset), CliKind::Claude);
        // User default only → user value.
        assert_eq!(resolve_cli(None, &user_default_codex), CliKind::Codex);
        // Worktree entry wins over the user default (both directions).
        let wt_codex = WorktreeEntry {
            cli: Some(CliKind::Codex),
            ..WorktreeEntry::name_only("w")
        };
        let wt_claude = WorktreeEntry {
            cli: Some(CliKind::Claude),
            ..WorktreeEntry::name_only("w")
        };
        assert_eq!(resolve_cli(Some(&wt_codex), &user_unset), CliKind::Codex);
        assert_eq!(
            resolve_cli(Some(&wt_claude), &user_default_codex),
            CliKind::Claude
        );
        // Worktree entry with no cli falls through to the user default.
        let wt_none = WorktreeEntry::name_only("w");
        assert_eq!(
            resolve_cli(Some(&wt_none), &user_default_codex),
            CliKind::Codex
        );
    }

    /// `resolve_provider_path`/`resolve_autonomy` fall through project → user →
    /// hardcoded default, indexed per-CLI.
    #[test]
    fn resolve_provider_path_and_autonomy_precedence() {
        let user = UserConfig {
            claude: ProviderConfig {
                path: Some("/user/claude".to_string()),
                autonomy: Some(false),
            },
            codex: ProviderConfig {
                path: Some("/user/codex".to_string()),
                autonomy: Some(true),
            },
            ..UserConfig::default()
        };

        // User-only: each CLI resolves to its own user value.
        assert_eq!(
            resolve_provider_path(CliKind::Claude, None, &user).as_deref(),
            Some("/user/claude")
        );
        assert_eq!(
            resolve_provider_path(CliKind::Codex, None, &user).as_deref(),
            Some("/user/codex")
        );
        assert!(!resolve_autonomy(CliKind::Claude, None, &user));
        assert!(resolve_autonomy(CliKind::Codex, None, &user));

        // Project override wins per-CLI.
        let mut proj = ProjectConfig::scaffold(Path::new("/tmp/p"));
        proj.codex.path = Some("/proj/codex".to_string());
        proj.codex.autonomy = Some(false);
        assert_eq!(
            resolve_provider_path(CliKind::Codex, Some(&proj), &user).as_deref(),
            Some("/proj/codex"),
            "project codex path wins"
        );
        assert!(
            !resolve_autonomy(CliKind::Codex, Some(&proj), &user),
            "project codex autonomy (explicit false) wins over user true"
        );
        // A project that doesn't set claude falls through to the user's claude.
        assert_eq!(
            resolve_provider_path(CliKind::Claude, Some(&proj), &user).as_deref(),
            Some("/user/claude")
        );

        // Nothing configured anywhere → None / false.
        let empty = UserConfig::default();
        assert_eq!(resolve_provider_path(CliKind::Codex, None, &empty), None);
        assert!(!resolve_autonomy(CliKind::Codex, None, &empty));
    }

    /// Per-CLI dispatch overrides on `ProjectRef` stamp onto the right field.
    #[test]
    fn codex_dispatch_override_applies() {
        let mut cfg = ProjectConfig::scaffold(Path::new("/tmp/p"));
        cfg.root = PathBuf::from("/tmp/p");
        let over = ProjectRef {
            dispatch_steps: Some(vec!["/claude-step".to_string()]),
            codex_dispatch_steps: Some(vec!["/codex-step".to_string()]),
            ..ProjectRef::new(PathBuf::from("/tmp/p"))
        };
        apply_registry_overrides(&mut cfg, &over);
        assert_eq!(cfg.dispatch.claude_steps, vec!["/claude-step".to_string()]);
        assert_eq!(cfg.dispatch.codex_steps, vec!["/codex-step".to_string()]);
    }
}
