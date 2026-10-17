//! `env` — resolve the herdr executable and build the isolated environment for
//! Keitora's own herdr server, plus the environment panes start with.
//!
//! Keitora must not collide with the user's personal herdr server, so every herdr
//! process the backend spawns (server, CLI calls, bridges) runs with
//! `XDG_CONFIG_HOME` + `XDG_STATE_HOME` + `HERDR_CONFIG_PATH` pointed at a
//! Keitora-owned dir under [`crate::paths::keitora_home`]
//! (`<KEITORA_HOME>/herdr`). herdr derives its socket, sessions, log and state
//! from those two `XDG_*` dirs only, so this fully isolates the instance. herdr
//! never reads `XDG_DATA_HOME` / `XDG_CACHE_HOME` / `XDG_RUNTIME_DIR`, so Keitora
//! leaves them alone (they flow through from its own environment).
//!
//! Panes inherit the server's environment, so without help every tool running in a
//! pane (`gh`, `claude`, ...) would put its config, caches and logins under the
//! isolated dirs. Panes therefore get the user's original `XDG_*` values
//! ([`OriginalXdg`], captured once when the backend is built); see
//! [`pane_xdg_env`] for what an unset original turns into.
//!
//! The written config disables onboarding and self-update
//! (`onboarding = false`, `[update] version_check = false, manifest_check = false`)
//! so the pinned server never phones home or rewrites its own binary.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use keitora_tasks::{
    herdr_bin_candidates, herdr_on_path, herdr_release_paths, herdr_well_known_paths,
    is_executable_file, PtyConfig,
};

use crate::backend::PtyBackendError;

/// Env var the dev/test harness uses to point at a specific herdr build (the
/// dev/test override). Highest priority after the explicit `[pty] herdr_bin`.
pub const HERDR_BIN_ENV: &str = "KEITORA_HERDR_BIN";

/// What a pane gets for an `XDG_*` var the user left unset (see [`pane_xdg_env`]).
#[derive(Clone, Copy)]
enum PaneDefault {
    /// Not set on the herdr server, so not passed: the pane inherits "unset".
    /// When the process carries a discarded (isolated) value for one of these it
    /// is removed from herdr commands.
    Passthrough,
    /// The spec default (relative to the home dir) on every platform.
    Spec(&'static str),
    /// The spec default where XDG is native (Linux/BSD), else an empty string.
    SpecOnXdgNative(&'static str),
}

/// The `XDG_*` vars, in a fixed order, with what panes get when one is unset.
const XDG_VARS: [(&str, PaneDefault); 5] = [
    ("XDG_CONFIG_HOME", PaneDefault::SpecOnXdgNative(".config")),
    ("XDG_STATE_HOME", PaneDefault::Spec(".local/state")),
    ("XDG_DATA_HOME", PaneDefault::Passthrough),
    ("XDG_CACHE_HOME", PaneDefault::Passthrough),
    ("XDG_RUNTIME_DIR", PaneDefault::Passthrough),
];

/// Whether XDG is the platform's native convention (Linux/BSD).
const XDG_NATIVE: bool = cfg!(all(unix, not(target_os = "macos")));

/// The user's own `XDG_*` values, as Keitora's process saw them at startup, with
/// anything that points into a Keitora herdr home discarded.
///
/// Keitora never sets `XDG_*` on its own process, so the process env is the user's
/// original one, except when Keitora itself was started from a Keitora pane (a dev
/// workflow): then it already carries the isolated values, which must not be passed
/// on as "original". A value under the Keitora root is therefore treated as unset.
/// Residual: a dev instance with a different `KEITORA_HOME` started from a pane of
/// the real instance still sees the real instance's isolated paths (they are not
/// under the dev root); accepted, it only affects dev runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OriginalXdg {
    pub config_home: Option<String>,
    pub state_home: Option<String>,
    pub data_home: Option<String>,
    pub cache_home: Option<String>,
    pub runtime_dir: Option<String>,
}

/// One captured `XDG_*` value: absent, empty, non-UTF-8 or under `keitora_root`
/// reads as `None`.
fn sanitize(value: Option<OsString>, keitora_root: &Path) -> Option<String> {
    let s = value?.into_string().ok()?;
    if s.is_empty() || Path::new(&s).starts_with(keitora_root) {
        return None;
    }
    Some(s)
}

impl OriginalXdg {
    /// Build from an env lookup (pure: tests inject the lookup, production passes
    /// the process env). See [`OriginalXdg`] for what is discarded.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>, keitora_root: &Path) -> Self {
        let get = |name: &str| sanitize(lookup(name), keitora_root);
        OriginalXdg {
            config_home: get("XDG_CONFIG_HOME"),
            state_home: get("XDG_STATE_HOME"),
            data_home: get("XDG_DATA_HOME"),
            cache_home: get("XDG_CACHE_HOME"),
            runtime_dir: get("XDG_RUNTIME_DIR"),
        }
    }

    /// Capture from the process environment.
    pub fn capture(keitora_root: &Path) -> Self {
        Self::from_lookup(|k| std::env::var_os(k), keitora_root)
    }

    /// The `XDG_*` names the lookup has a non-empty value for that
    /// [`Self::from_lookup`] discards (non-UTF-8 or under `keitora_root`).
    pub fn contaminated(
        lookup: impl Fn(&str) -> Option<OsString>,
        keitora_root: &Path,
    ) -> Vec<&'static str> {
        XDG_VARS
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| {
                let raw = lookup(name);
                let set = raw.as_ref().is_some_and(|v| !v.is_empty());
                set && sanitize(raw, keitora_root).is_none()
            })
            .collect()
    }

    /// The values in [`XDG_VARS`] order.
    fn values(&self) -> [&Option<String>; 5] {
        [
            &self.config_home,
            &self.state_home,
            &self.data_home,
            &self.cache_home,
            &self.runtime_dir,
        ]
    }
}

/// herdr's per-pane scrollback unless `[pty] herdr_scrollback_limit_bytes` says
/// otherwise; it is the only history a pane keeps. The limit is ghostty page memory
/// (about `8 * cols` bytes a row, visible screen included), not text: 16 MB holds
/// roughly 15k rows at 120 columns and 9k at 200. It is allocated only as history
/// grows, and applies to panes created after the herdr server next starts.
pub const DEFAULT_HERDR_SCROLLBACK_LIMIT_BYTES: u64 = 16_000_000;

/// Accepted range for a configured scrollback limit.
const SCROLLBACK_RANGE: std::ops::RangeInclusive<u64> = 100_000..=100_000_000;

/// The scrollback limit to write: the configured value when it is in range, else
/// the default (an out-of-range value is logged).
pub fn clamp_scrollback(configured: Option<u64>) -> u64 {
    match configured {
        None => DEFAULT_HERDR_SCROLLBACK_LIMIT_BYTES,
        Some(v) if SCROLLBACK_RANGE.contains(&v) => v,
        Some(_) => {
            log::warn!("herdr: scrollback_limit_bytes out of range, using default");
            DEFAULT_HERDR_SCROLLBACK_LIMIT_BYTES
        }
    }
}

/// Inputs to [`HerdrEnv::with_options`] that come from outside the herdr home.
/// `Default` is deliberately not derived: callers choose between the process
/// environment ([`Self::from_process`]) and an injected one.
#[derive(Clone, Debug)]
pub struct HerdrEnvOptions {
    /// The user's original `XDG_*` values, handed to panes.
    pub original_xdg: OriginalXdg,
    /// `XDG_*` names the process carried a discarded value for (see
    /// [`OriginalXdg::contaminated`]); the pass-through ones are removed from herdr
    /// commands so the isolated value does not reach the server.
    pub contaminated: Vec<&'static str>,
    /// herdr's per-pane scrollback, written into its config. Applies to panes
    /// created after the server next starts.
    pub scrollback_limit_bytes: u64,
    /// Where the `PATH` every herdr process (and so every pane) runs with comes
    /// from.
    pub path: PathSource,
}

impl HerdrEnvOptions {
    /// Build from an injected env lookup (pure).
    #[cfg(test)]
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>, keitora_root: &Path) -> Self {
        HerdrEnvOptions {
            original_xdg: OriginalXdg::from_lookup(&lookup, keitora_root),
            contaminated: OriginalXdg::contaminated(&lookup, keitora_root),
            scrollback_limit_bytes: DEFAULT_HERDR_SCROLLBACK_LIMIT_BYTES,
            path: PathSource::None,
        }
    }

    /// Build from the process environment, discarding values under
    /// [`crate::paths::keitora_home`]. Runs nothing: the `PATH` is resolved the
    /// first time it is used (see [`PathSource::spawn_path`]).
    pub fn from_process() -> Self {
        let root = crate::paths::keitora_home();
        HerdrEnvOptions {
            original_xdg: OriginalXdg::capture(&root),
            contaminated: OriginalXdg::contaminated(|k| std::env::var_os(k), &root),
            scrollback_limit_bytes: DEFAULT_HERDR_SCROLLBACK_LIMIT_BYTES,
            path: PathSource::spawn_path(),
        }
    }
}

/// Where the `PATH` herdr and its panes run with comes from.
#[derive(Clone)]
pub enum PathSource {
    /// No `PATH` entry: herdr inherits the process `PATH` (always on Windows).
    None,
    /// A known value.
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(String),
    /// Resolved the first time it is asked for, then kept (clones share it).
    #[cfg_attr(windows, allow(dead_code))]
    Lazy(LazyPath),
}

/// A `PATH` worked out on first use (see [`PathSource::Lazy`]).
#[derive(Clone)]
pub struct LazyPath {
    value: Arc<OnceLock<Option<String>>>,
    resolve: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// Whether resolving runs the user's login shell.
    login_shell: bool,
}

impl PathSource {
    /// A `PATH` that `resolve` works out on first use.
    #[cfg(test)]
    pub fn lazy(resolve: impl Fn() -> Option<String> + Send + Sync + 'static) -> Self {
        PathSource::Lazy(LazyPath {
            value: Arc::new(OnceLock::new()),
            resolve: Arc::new(resolve),
            login_shell: false,
        })
    }

    /// The production source. Unix: [`keitora_tasks::unix_spawn_path`], which runs
    /// the user's login shell, so it is resolved on first use (the first herdr
    /// command, off the setup thread) and never while the backend is built.
    /// Windows: none, the process `PATH` is kept.
    pub fn spawn_path() -> Self {
        #[cfg(unix)]
        {
            PathSource::Lazy(LazyPath {
                value: Arc::new(OnceLock::new()),
                resolve: Arc::new(keitora_tasks::unix_spawn_path),
                login_shell: true,
            })
        }
        #[cfg(windows)]
        {
            PathSource::None
        }
    }

    /// Whether asking for the value would run the user's login shell.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn runs_login_shell(&self) -> bool {
        matches!(self, PathSource::Lazy(lazy) if lazy.login_shell && lazy.value.get().is_none())
    }

    /// The `PATH`, resolving a lazy one now if this is its first use.
    pub fn get(&self) -> Option<&str> {
        match self {
            PathSource::None => None,
            PathSource::Fixed(path) => Some(path),
            PathSource::Lazy(lazy) => lazy.value.get_or_init(|| (lazy.resolve)()).as_deref(),
        }
    }
}

impl std::fmt::Debug for PathSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathSource::None => f.write_str("None"),
            PathSource::Fixed(path) => f.debug_tuple("Fixed").field(path).finish(),
            // Never resolves: printing must not run the login shell.
            PathSource::Lazy(lazy) => f.debug_tuple("Lazy").field(&lazy.value.get()).finish(),
        }
    }
}

/// The longest unix socket path the platform allows (`sun_path` less the NUL):
/// 103 on macOS, 107 elsewhere.
#[cfg(target_os = "macos")]
const SOCKET_PATH_LIMIT: usize = 103;
#[cfg(all(unix, not(target_os = "macos")))]
const SOCKET_PATH_LIMIT: usize = 107;

/// Check that herdr's longest socket, `<home>/config/herdr/herdr.sock.agent`, fits
/// in `limit` bytes; a longer one makes every herdr call fail inside herdr. Pure.
/// Only unix has a socket path limit; elsewhere this is reached from tests alone.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn check_socket_path(home: &Path, limit: usize) -> Result<(), String> {
    let sock = home.join("config").join("herdr").join("herdr.sock.agent");
    let len = sock.as_os_str().len();
    if len <= limit {
        return Ok(());
    }
    Err(format!(
        "herdr socket path {} is {len} bytes, over the {limit}-byte unix socket limit; \
         set KEITORA_HOME to a shorter path",
        sock.display()
    ))
}

/// A resolved herdr runtime: the executable to spawn plus the isolated env to
/// apply to every herdr command (via [`HerdrEnv::apply`]).
#[derive(Clone, Debug)]
pub struct HerdrEnv {
    /// The herdr executable (absolute, or a bare name resolved via `PATH`).
    pub exe: PathBuf,
    /// `(key, value)` env overrides applied to every spawned herdr process (the
    /// `PATH` is separate: see [`Self::path_source`]).
    pub env: Vec<(String, String)>,
    /// Where the `PATH` of every spawned herdr process comes from; read through
    /// [`Self::path`].
    pub path_source: PathSource,
    /// The Keitora-owned herdr home (`<KEITORA_HOME>/herdr`). The XDG subdirs live
    /// under it; Keitora's own sidecar state (the persisted keymap) lives directly in
    /// it, outside the dirs herdr owns.
    pub home: PathBuf,
    /// The user's original `XDG_*` values, captured once; panes get these.
    pub original_xdg: OriginalXdg,
    /// Env names removed from every spawned herdr process (contaminated
    /// pass-through `XDG_*` values).
    pub env_remove: Vec<String>,
    /// How [`Self::exe`] was found (or that it was not).
    pub discovery: ExeDiscovery,
}

/// The outcome of searching for the herdr executable (see [`discover_exe`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExeDiscovery {
    /// The chosen executable: a path the user named, the bare `herdr` when it is
    /// on the process `PATH` (or when nothing was found, so a hit at spawn time
    /// still works), else the absolute path it was discovered at.
    pub exe: PathBuf,
    /// The absolute candidates tried, in priority order: all of them when nothing
    /// was found, else the named ones that were passed over and the one that hit.
    pub searched: Vec<PathBuf>,
    /// The entries of `searched` to name in a short message: the configured
    /// path, the `KEITORA_HERDR_BIN` path, then the well-known install locations.
    pub notable: Vec<PathBuf>,
    /// Whether a candidate passed the executable-file check.
    pub found: bool,
    /// Whether the user named `exe` (`[pty] herdr_bin` or `KEITORA_HERDR_BIN`)
    /// rather than it being discovered. Only a named path identifies the running
    /// server by its exact location: a discovered binary can move under a server
    /// that keeps running (a self-update, `brew upgrade`).
    pub explicit: bool,
    /// Nothing was found, and the login-shell `PATH` has not been searched yet
    /// (the launch search runs on the setup thread, which must not wait for the
    /// user's shell). The boot thread searches again, in full.
    pub login_deferred: bool,
}

impl ExeDiscovery {
    /// An explicitly supplied executable (tests, harnesses): nothing was searched.
    pub fn explicit(exe: PathBuf) -> Self {
        ExeDiscovery {
            searched: vec![exe.clone()],
            notable: vec![exe.clone()],
            exe,
            found: true,
            explicit: true,
            login_deferred: false,
        }
    }
}

/// The herdr executable's file name.
#[cfg(unix)]
const HERDR_EXE_NAME: &str = "herdr";
#[cfg(windows)]
const HERDR_EXE_NAME: &str = "herdr.exe";

/// Whether a search may run the user's login shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginSearch {
    /// Not now: the caller is the setup thread. A search that finds nothing is
    /// marked [`ExeDiscovery::login_deferred`].
    Deferred,
    /// Yes, when every earlier step missed.
    Allowed,
}

/// The login-shell `PATH` step of one search.
enum LoginPath<'a> {
    /// There is no such step (Windows).
    #[cfg_attr(unix, allow(dead_code))]
    Unavailable,
    /// Skipped for now (see [`LoginSearch::Deferred`]).
    #[cfg_attr(windows, allow(dead_code))]
    Deferred,
    /// Call this for the login `PATH`, only when every earlier step missed.
    #[cfg_attr(windows, allow(dead_code))]
    Search(&'a dyn Fn() -> Option<String>),
}

/// Everything [`discover_exe_with`] reads from outside, so tests inject it.
struct DiscoveryInputs<'a> {
    /// The `KEITORA_HERDR_BIN` value.
    env_override: Option<PathBuf>,
    /// The process `PATH`.
    process_path: Option<String>,
    /// The well-known install locations (unix).
    well_known: Vec<PathBuf>,
    /// The standalone releases' executables, newest first. Called at most once.
    releases: &'a dyn Fn() -> Vec<PathBuf>,
    login_path: LoginPath<'a>,
    /// The first herdr executable on a `PATH`-style list.
    find_on_path: &'a dyn Fn(&str) -> Option<PathBuf>,
    is_executable: &'a dyn Fn(&Path) -> bool,
}

/// Find the herdr executable. The first hit wins, and a hit is an executable
/// file:
/// 1. `[pty] herdr_bin`,
/// 2. `KEITORA_HERDR_BIN`,
/// 3. `herdr` on the process `PATH`: the result is the bare name `herdr`, as
///    before any discovery existed,
/// 4. the well-known install locations (unix),
/// 5. the newest standalone release,
/// 6. `herdr` on the login-shell `PATH` (unix, and only with
///    [`LoginSearch::Allowed`]): the one step that runs the login shell, reached
///    only when everything above missed.
///
/// When none hits, `exe` is the bare `herdr` and `found` is false.
pub fn discover_exe(cfg: &PtyConfig, login: LoginSearch) -> ExeDiscovery {
    #[cfg(unix)]
    let login_path = match login {
        LoginSearch::Deferred => LoginPath::Deferred,
        LoginSearch::Allowed => LoginPath::Search(&keitora_tasks::cached_login_path),
    };
    #[cfg(windows)]
    let login_path = {
        let _ = login;
        LoginPath::Unavailable
    };
    discover_exe_with(
        cfg,
        DiscoveryInputs {
            env_override: std::env::var_os(HERDR_BIN_ENV).map(PathBuf::from),
            process_path: std::env::var_os("PATH").map(|p| p.to_string_lossy().into_owned()),
            well_known: herdr_well_known_paths(),
            releases: &herdr_release_paths,
            login_path,
            find_on_path: &herdr_on_path,
            is_executable: &is_executable_file,
        },
    )
}

/// `herdr` in each directory of a `PATH`-style list, for "searched" messages.
fn path_candidates(path: &str) -> Vec<PathBuf> {
    std::env::split_paths(path)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(HERDR_EXE_NAME))
        .collect()
}

/// The absolute paths of `paths`, in order, without duplicates.
fn absolute_once(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        if p.is_absolute() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// A search that hit: `exe` is what gets spawned, `at` where the file was seen,
/// `passed` the named paths tried before it. Only those are reported; the full
/// list of places is built for a miss alone.
fn hit(passed: &[PathBuf], exe: PathBuf, at: PathBuf, explicit: bool) -> ExeDiscovery {
    let searched = absolute_once(passed.iter().cloned().chain([at]));
    ExeDiscovery {
        exe,
        notable: searched.clone(),
        searched,
        found: true,
        explicit,
        login_deferred: false,
    }
}

/// [`discover_exe`] over injected inputs.
fn discover_exe_with(cfg: &PtyConfig, inputs: DiscoveryInputs<'_>) -> ExeDiscovery {
    let DiscoveryInputs {
        env_override,
        process_path,
        well_known,
        releases,
        login_path,
        find_on_path,
        is_executable,
    } = inputs;
    // What the user named: the config path, then the env override.
    let named = herdr_bin_candidates(cfg, env_override, Vec::new());
    if let Some(i) = named.iter().position(|p| is_executable(p)) {
        return hit(&named[..i], named[i].clone(), named[i].clone(), true);
    }
    if let Some(at) = process_path.as_deref().and_then(find_on_path) {
        return hit(&named, PathBuf::from("herdr"), at, false);
    }
    if let Some(exe) = well_known.iter().find(|p| is_executable(p)) {
        return hit(&named, exe.clone(), exe.clone(), false);
    }
    let releases = releases();
    if let Some(exe) = releases.iter().find(|p| is_executable(p)) {
        return hit(&named, exe.clone(), exe.clone(), false);
    }
    let login = match &login_path {
        LoginPath::Search(login) => login(),
        LoginPath::Unavailable | LoginPath::Deferred => None,
    };
    if let Some(exe) = login.as_deref().and_then(find_on_path) {
        return hit(&named, exe.clone(), exe, false);
    }
    // Nothing: say everywhere that was looked, in the order it was.
    let on_path = |path: &Option<String>| path.as_deref().map(path_candidates).unwrap_or_default();
    let searched = absolute_once(
        named
            .iter()
            .cloned()
            .chain(on_path(&process_path))
            .chain(well_known.iter().cloned())
            .chain(releases)
            .chain(on_path(&login)),
    );
    let mut notable = absolute_once(named.into_iter().chain(well_known));
    notable.retain(|p| searched.contains(p));
    ExeDiscovery {
        exe: PathBuf::from("herdr"),
        searched,
        notable,
        found: false,
        explicit: false,
        login_deferred: matches!(login_path, LoginPath::Deferred),
    }
}

/// herdr's server log: `<home>/config/herdr/herdr-server.log` (herdr resolves its
/// config dir as `<XDG_CONFIG_HOME>/herdr`, and Keitora sets
/// `XDG_CONFIG_HOME = <home>/config`).
pub fn herdr_log_path(home: &Path) -> PathBuf {
    home.join("config").join("herdr").join("herdr-server.log")
}

impl HerdrEnv {
    /// The `PATH` herdr and its panes run with, when Keitora sets one (unix).
    /// The first call resolves it, which in production runs the user's login
    /// shell: every caller is about to run a herdr command, so this is never
    /// reached on the setup thread.
    pub fn path(&self) -> Option<&str> {
        self.path_source.get()
    }

    /// herdr's server log for this env (see [`herdr_log_path`]).
    pub fn log_path(&self) -> PathBuf {
        herdr_log_path(&self.home)
    }

    /// Path of the persisted `SessionKey -> HerdrPane` keymap
    /// (`<KEITORA_HOME>/herdr/keymap.json`). Derived from [`Self::home`] so
    /// it is stable across app restarts for a given `KEITORA_HOME`.
    pub fn keymap_path(&self) -> PathBuf {
        self.home.join("keymap.json")
    }

    /// Path of the global resume DB (`<KEITORA_HOME>/herdr/resume.db`): which agent
    /// conversation each worktree should resume, next to the keymap.
    pub fn resume_db_path(&self) -> PathBuf {
        self.home.join("resume.db")
    }

    /// Resolve the herdr exe with [`discover_exe`], then build the isolated env
    /// rooted at `<KEITORA_HOME>/herdr`. Runs on the setup thread, so it never
    /// runs the login shell: the search leaves that step to the boot thread and
    /// the `PATH` is resolved on first use.
    pub fn resolve(cfg: &PtyConfig) -> Result<HerdrEnv, PtyBackendError> {
        let discovery = discover_exe(cfg, LoginSearch::Deferred);
        let exe = discovery.exe.clone();
        let home = crate::paths::keitora_home().join("herdr");
        let mut opts = HerdrEnvOptions::from_process();
        opts.scrollback_limit_bytes = clamp_scrollback(cfg.herdr_scrollback_limit_bytes);
        let mut env = Self::with_options(exe, home, opts)?;
        env.discovery = discovery;
        Ok(env)
    }

    /// Build an isolated env rooted at `home` for an explicit `exe`, capturing the
    /// original `XDG_*` values from the process and the default scrollback. Used by
    /// the real-herdr integration tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_exe_and_home(exe: PathBuf, home: PathBuf) -> Result<HerdrEnv, PtyBackendError> {
        // No spawn `PATH`: this must never run the login shell, and the gated real
        // herdr tests want the process `PATH` as is.
        let mut opts = HerdrEnvOptions::from_process();
        opts.path = PathSource::None;
        Self::with_options(exe, home, opts)
    }

    /// Build an isolated env rooted at `home` for an explicit `exe`. Creates the
    /// `config/` and `state/` dirs and writes the Keitora herdr `config.toml`.
    /// Existing `data/`, `cache/` and `runtime/` dirs from older versions are left
    /// alone.
    pub fn with_options(
        exe: PathBuf,
        home: PathBuf,
        opts: HerdrEnvOptions,
    ) -> Result<HerdrEnv, PtyBackendError> {
        let io = |e: std::io::Error| PtyBackendError::Other(format!("herdr env setup: {e}"));

        #[cfg(unix)]
        check_socket_path(&home, SOCKET_PATH_LIMIT).map_err(PtyBackendError::Other)?;

        let config_home = home.join("config");
        let state_home = home.join("state");
        for d in [&config_home, &state_home] {
            std::fs::create_dir_all(d).map_err(io)?;
        }

        // herdr reads `<XDG_CONFIG_HOME>/herdr/config.toml`; HERDR_CONFIG_PATH pins
        // the exact file as belt-and-suspenders.
        let config_file = config_home.join("herdr").join("config.toml");
        std::fs::create_dir_all(config_file.parent().unwrap()).map_err(io)?;
        std::fs::write(&config_file, herdr_config_toml(opts.scrollback_limit_bytes)).map_err(io)?;

        let to_s = |p: &PathBuf| p.to_string_lossy().into_owned();
        let env = vec![
            ("XDG_CONFIG_HOME".to_string(), to_s(&config_home)),
            ("XDG_STATE_HOME".to_string(), to_s(&state_home)),
            ("HERDR_CONFIG_PATH".to_string(), to_s(&config_file)),
        ];
        let env_remove = XDG_VARS
            .into_iter()
            .filter(|(name, default)| {
                matches!(default, PaneDefault::Passthrough) && opts.contaminated.contains(name)
            })
            .map(|(name, _)| name.to_string())
            .collect();
        Ok(HerdrEnv {
            discovery: ExeDiscovery::explicit(exe.clone()),
            exe,
            env,
            path_source: opts.path,
            home,
            original_xdg: opts.original_xdg,
            env_remove,
        })
    }

    /// Apply this env to a herdr `Command`: remove [`Self::env_remove`], then set
    /// every [`Self::env`] pair and the `PATH` ([`Self::path`]). Every herdr
    /// server, CLI and bridge spawn site must call this (before handing the
    /// command to a launcher), so they all talk to the same isolated instance.
    pub fn apply(&self, cmd: &mut std::process::Command) {
        for name in &self.env_remove {
            cmd.env_remove(name);
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        if let Some(path) = self.path() {
            cmd.env("PATH", path);
        }
    }
}

/// The `XDG_*` part of every pane's env (`--env K=V`), in [`XDG_VARS`] order.
/// Every var the user set is passed through. Panes inherit the herdr server's
/// environment, and herdr's launch env can only set values, never unset them, so
/// an unset original is handled per var:
///
/// Never use an empty value where a tool might join it into a path. Claude Code
/// does exactly that: with `XDG_DATA_HOME`, `XDG_CACHE_HOME` and `XDG_STATE_HOME`
/// empty it installed into relative `claude/versions`, `claude/staging` and
/// `claude/locks` dirs in the pane's cwd.
///
/// - `XDG_DATA_HOME` / `XDG_CACHE_HOME` / `XDG_RUNTIME_DIR` are left out. The
///   server this Keitora starts never carries them, so the pane inherits "unset".
///   A server started by an older Keitora, before they were stripped, still
///   carries isolated values. Its panes inherit them until it restarts.
/// - `XDG_STATE_HOME` must override the server's isolated value and gets the
///   spec default (`~/.local/state`) on every platform. That is where Claude Code
///   keeps its state when the var is unset, on Windows too.
/// - `XDG_CONFIG_HOME` must override the server's isolated value too. On
///   Linux/BSD it gets the spec default, which means the same as unset. On macOS
///   and Windows a set value makes tools such as `gh` or lazygit move their
///   config and logins away from the platform dir, so it gets an empty string
///   there instead.
///
/// With no home dir, a default falls back to an empty string. Never contains a
/// `HERDR_*` var: herdr sets its own per pane.
pub fn pane_xdg_env(original: &OriginalXdg) -> Vec<(String, String)> {
    pane_xdg_env_for(original, XDG_NATIVE, dirs::home_dir().as_deref())
}

/// [`pane_xdg_env`] with the platform and home dir injected.
fn pane_xdg_env_for(
    original: &OriginalXdg,
    xdg_native: bool,
    home: Option<&Path>,
) -> Vec<(String, String)> {
    let spec = |rel: &str| {
        home.map(|h| h.join(rel).to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    XDG_VARS
        .into_iter()
        .zip(original.values())
        .filter_map(|((name, default), v)| {
            let value = match (v, default) {
                (Some(v), _) => v.clone(),
                (None, PaneDefault::Passthrough) => return None,
                (None, PaneDefault::Spec(rel)) => spec(rel),
                (None, PaneDefault::SpecOnXdgNative(rel)) if xdg_native => spec(rel),
                (None, PaneDefault::SpecOnXdgNative(_)) => String::new(),
            };
            Some((name.to_string(), value))
        })
        .collect()
}

/// Per-pane env (`--env K=V`) for a herdr agent pane: a `PATH` entry first, then
/// [`pane_xdg_env`]. herdr's `agent.start` resolves the agent binary via the pane's
/// `PATH`, so the configured executable's directory is prepended to it — otherwise
/// a configured/non-PATH `claude`/`codex` would not be found. `exe` is the resolved
/// agent path ([`crate::backend::AgentLaunch::path`]).
///
/// On Windows the bundled CLI ships as `keitora-cli.exe` beside the app, which is
/// not on any pane's `PATH`, so agents could not run a bare `keitora`. The per-user
/// `keitora.exe` mirror dir ([`keitora_tasks::windows_keitora_bin_dir`]) is
/// prepended ahead of the exe dir. A bare exe name with no alias dir adds no `PATH`,
/// so the pane inherits herdr's own.
///
/// Unix: `herdr_path` (the `PATH` herdr itself runs with, [`HerdrEnv::path`]) is
/// the base, and a `PATH` is always set, even for a bare agent name, so a server
/// started by an older Keitora with a minimal `PATH` still gives new panes a good
/// one. Windows: the base is the process `PATH` and `herdr_path` is ignored.
///
/// Wired into `HerdrBackend::spawn_agent`.
pub fn pane_env(
    exe: &str,
    original: &OriginalXdg,
    herdr_path: Option<&str>,
) -> Vec<(String, String)> {
    let alias = keitora_alias_dir();
    let mut env = Vec::new();
    if let Some(path) = pane_path_with(
        exe_dir(exe).as_deref(),
        || base_path(herdr_path),
        alias.as_deref(),
        PATH_SEP,
        cfg!(unix),
    ) {
        env.push(("PATH".to_string(), path));
    }
    env.extend(pane_xdg_env(original));
    env
}

/// Env for a plain shell pane: [`pane_xdg_env`], plus on unix the `PATH` herdr runs
/// with (so the shell does not start from a stale server's minimal one).
pub fn pane_shell_env(original: &OriginalXdg, herdr_path: Option<&str>) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if cfg!(unix) {
        if let Some(path) = herdr_path {
            env.push(("PATH".to_string(), path.to_string()));
        }
    }
    env.extend(pane_xdg_env(original));
    env
}

#[cfg(windows)]
const PATH_SEP: char = ';';
#[cfg(unix)]
const PATH_SEP: char = ':';

/// The PATH a pane's own `PATH` entry extends: the process `PATH` on Windows.
#[cfg(windows)]
fn base_path(_herdr_path: Option<&str>) -> Option<String> {
    std::env::var("PATH").ok()
}

/// The PATH a pane's own `PATH` entry extends: the one herdr runs with.
#[cfg(unix)]
fn base_path(herdr_path: Option<&str>) -> Option<String> {
    herdr_path.map(str::to_string)
}

/// The dir holding the bare `keitora` alias for panes. Windows only: the mirror is
/// memoized for the process inside `keitora_tasks`, so this is cheap per spawn.
/// Test builds never resolve it, because resolving copies the CLI into the real
/// `%LOCALAPPDATA%`.
#[cfg(all(windows, not(test)))]
fn keitora_alias_dir() -> Option<PathBuf> {
    keitora_tasks::windows_keitora_bin_dir()
}

/// No alias dir on unix (the mirror is Windows-only), nor in test builds, which
/// must not touch `%LOCALAPPDATA%`.
#[cfg(any(unix, test))]
fn keitora_alias_dir() -> Option<PathBuf> {
    None
}

/// A pane's `PATH`: the alias dir, then the exe dir, then `base`, with order kept
/// and later duplicates dropped. With neither an alias dir nor an exe dir this is
/// `None` (inherit herdr's `PATH`), unless `always` is set, in which case it is
/// `base` alone (`None` still when that is missing too).
fn pane_path_with(
    exe_dir: Option<&str>,
    base: impl FnOnce() -> Option<String>,
    alias_dir: Option<&Path>,
    sep: char,
    always: bool,
) -> Option<String> {
    let alias = alias_dir.map(|d| d.to_string_lossy().into_owned());
    let front: Vec<&str> = alias.as_deref().into_iter().chain(exe_dir).collect();
    if front.is_empty() && !always {
        return None;
    }
    let base = base();
    if front.is_empty() && base.is_none() {
        return None;
    }
    Some(prepend_dirs_to_path(&front, base.as_deref(), sep))
}

/// [`pane_path_with`] for the "inherit when there is nothing to prepend" case.
#[cfg(test)]
fn pane_path(
    exe_dir: Option<&str>,
    base: impl FnOnce() -> Option<String>,
    alias_dir: Option<&Path>,
    sep: char,
) -> Option<String> {
    pane_path_with(exe_dir, base, alias_dir, sep, false)
}

/// Directory component of an agent executable path, or `None` for a bare name.
fn exe_dir(exe: &str) -> Option<String> {
    let parent = std::path::Path::new(exe).parent()?;
    let s = parent.to_string_lossy();
    (!s.is_empty()).then(|| s.into_owned())
}

/// Order-preserving PATH join with `dirs` first and any later duplicate of an
/// entry dropped. Pure (explicit `sep`) so both platforms' separators are
/// unit-testable.
fn prepend_dirs_to_path(dirs: &[&str], base: Option<&str>, sep: char) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in dirs.iter().copied().chain(base.unwrap_or("").split(sep)) {
        if !p.is_empty() && !parts.contains(&p) {
            parts.push(p);
        }
    }
    parts.join(&sep.to_string())
}

/// The subdirs older Keitora versions pointed panes' `XDG_*` at.
const LEAK_SUBDIRS: [&str; 4] = ["config", "state", "data", "cache"];

/// Tool folders left behind in the old isolated dirs: the immediate directory
/// entries of `<home>/{config,state,data,cache}` other than herdr's own, as sorted
/// `"<sub>/<name>"` strings (forward slashes on every OS). Only lists directory
/// names: it never opens, reads or recurses into anything.
pub fn leaked_dirs(home: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for sub in LEAK_SUBDIRS {
        let Ok(entries) = std::fs::read_dir(home.join(sub)) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != "herdr" {
                out.push(format!("{sub}/{name}"));
            }
        }
    }
    out.sort();
    out
}

/// Marker recording that the old-folders notice was handled, next to the keymap.
fn xdg_note_marker(home: &Path) -> PathBuf {
    home.join("xdg-note-shown")
}

/// Take the one-time notice that pane tools now use the user's normal folders:
/// the leftover tool folders ([`leaked_dirs`]) the first time this runs for a
/// herdr home, `None` afterwards. The marker is written before answering, so the
/// user is never asked twice; if it cannot be written the notice is skipped
/// (`None`) rather than repeated on every launch. A home with nothing left behind
/// (a fresh install) answers `None` too.
pub fn take_xdg_leak_notice(home: &Path) -> Option<Vec<String>> {
    let marker = xdg_note_marker(home);
    if marker.exists() {
        return None;
    }
    let list = leaked_dirs(home);
    if let Err(e) = std::fs::write(&marker, "1") {
        log::warn!(
            "herdr: could not write xdg note marker {}: {e}",
            marker.display()
        );
        return None;
    }
    if list.is_empty() {
        return None;
    }
    log::info!("herdr: xdg leak note shown dirs={list:?}");
    Some(list)
}

/// The Keitora-owned herdr config: no onboarding, no self-update / manifest checks,
/// and no agent relaunch on restore. herdr still restores its panes after a server
/// restart, as bare shells; Keitora closes them and resumes the agents itself with
/// its own launch flags. Per-pane scrollback is capped at `scrollback_limit_bytes`
/// (read by herdr when the server starts, for panes it creates afterwards).
/// Rewritten on every resolve, so an existing file picks up changes the first time
/// a new Keitora runs (and hand edits to it do not survive a restart).
fn herdr_config_toml(scrollback_limit_bytes: u64) -> String {
    format!(
        "\
onboarding = false

[update]
version_check = false
manifest_check = false

[session]
resume_agents_on_restore = false

[advanced]
scrollback_limit_bytes = {scrollback_limit_bytes}
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pty_cfg(herdr_bin: Option<&str>) -> PtyConfig {
        PtyConfig {
            herdr_bin: herdr_bin.map(PathBuf::from),
            ..PtyConfig::default()
        }
    }

    /// The search over a fake machine. Unix paths, so unix only.
    #[cfg(unix)]
    mod discovery {
        use super::*;

        /// A fake machine for one search: `files` are the executable files, the
        /// rest is where to look. Counts how often the releases and the login
        /// shell are asked.
        struct Machine {
            files: Vec<&'static str>,
            env: Option<&'static str>,
            process_path: Option<&'static str>,
            well_known: Vec<&'static str>,
            releases: Vec<&'static str>,
            login: Option<&'static str>,
            release_reads: std::cell::Cell<u32>,
            login_runs: std::cell::Cell<u32>,
        }

        impl Machine {
            fn with(files: &[&'static str]) -> Self {
                Machine {
                    files: files.to_vec(),
                    env: None,
                    process_path: Some("/proc/a:/proc/b"),
                    well_known: vec!["/opt/homebrew/bin/herdr", "/usr/local/bin/herdr"],
                    releases: vec!["/r/0.10.0/herdr", "/r/0.9.3/herdr"],
                    login: Some("/login/a:/login/b"),
                    release_reads: Default::default(),
                    login_runs: Default::default(),
                }
            }

            fn search_with(&self, herdr_bin: Option<&str>, allow_login: bool) -> ExeDiscovery {
                let is_executable = |p: &Path| self.files.iter().any(|f| Path::new(f) == p);
                let find_on_path = |path: &str| {
                    path.split(':')
                        .map(|d| Path::new(d).join("herdr"))
                        .find(|p| is_executable(p))
                };
                let releases = || {
                    self.release_reads.set(self.release_reads.get() + 1);
                    self.releases.iter().map(PathBuf::from).collect()
                };
                let login = || {
                    self.login_runs.set(self.login_runs.get() + 1);
                    self.login.map(str::to_string)
                };
                discover_exe_with(
                    &pty_cfg(herdr_bin),
                    DiscoveryInputs {
                        env_override: self.env.map(PathBuf::from),
                        process_path: self.process_path.map(str::to_string),
                        well_known: self.well_known.iter().map(PathBuf::from).collect(),
                        releases: &releases,
                        login_path: if allow_login {
                            LoginPath::Search(&login)
                        } else {
                            LoginPath::Deferred
                        },
                        find_on_path: &find_on_path,
                        is_executable: &is_executable,
                    },
                )
            }

            fn search(&self, herdr_bin: Option<&str>) -> ExeDiscovery {
                self.search_with(herdr_bin, true)
            }
        }

        fn pbs(items: &[&str]) -> Vec<PathBuf> {
            items.iter().map(PathBuf::from).collect()
        }

        /// Every place a herdr can be, lowest priority first.
        const EVERYWHERE: [&str; 6] = [
            "/login/b/herdr",
            "/r/0.9.3/herdr",
            "/usr/local/bin/herdr",
            "/proc/b/herdr",
            "/env/herdr",
            "/cfg/herdr",
        ];

        #[test]
        fn takes_the_first_hit_in_priority_order() {
            let mut m = Machine::with(&EVERYWHERE);
            m.env = Some("/env/herdr");
            // (what is spawned, where it was seen, named by the user?)
            let want = [
                ("/cfg/herdr", "/cfg/herdr", true),
                ("/env/herdr", "/env/herdr", true),
                // On the process PATH: the bare name, as before discovery existed.
                ("herdr", "/proc/b/herdr", false),
                ("/usr/local/bin/herdr", "/usr/local/bin/herdr", false),
                ("/r/0.9.3/herdr", "/r/0.9.3/herdr", false),
                ("/login/b/herdr", "/login/b/herdr", false),
            ];
            for (exe, at, explicit) in want {
                let d = m.search(Some("/cfg/herdr"));
                assert_eq!(d.exe, PathBuf::from(exe));
                assert!(d.found && !d.login_deferred, "{exe}");
                assert_eq!(d.explicit, explicit, "{exe}");
                assert_eq!(d.searched.last(), Some(&PathBuf::from(at)), "{exe}");
                // The winner goes away; the next search falls one step further.
                m.files.pop();
            }
            let d = m.search(Some("/cfg/herdr"));
            assert_eq!((d.exe, d.found), (PathBuf::from("herdr"), false));
        }

        #[test]
        fn runs_the_login_shell_only_when_every_other_step_misses() {
            // One hit per earlier step: the shell is never asked.
            for file in &EVERYWHERE[1..] {
                let mut m = Machine::with(&[*file]);
                m.env = Some("/env/herdr");
                assert!(m.search(Some("/cfg/herdr")).found, "{file}");
                assert_eq!(m.login_runs.get(), 0, "{file}");
            }
            let m = Machine::with(&["/login/b/herdr"]);
            assert!(m.search(None).found);
            assert_eq!(m.login_runs.get(), 1);
            let m = Machine::with(&[]);
            assert!(!m.search(None).found);
            assert_eq!(m.login_runs.get(), 1, "asked once, also for the message");
        }

        #[test]
        fn reads_the_releases_once_and_only_when_it_gets_that_far() {
            for file in ["/cfg/herdr", "/proc/a/herdr", "/opt/homebrew/bin/herdr"] {
                let m = Machine::with(&[file]);
                assert!(m.search(Some("/cfg/herdr")).found);
                assert_eq!(m.release_reads.get(), 0, "{file}");
            }
            let cases: [&[&'static str]; 3] = [&["/r/0.9.3/herdr"], &["/login/a/herdr"], &[]];
            for files in cases {
                let m = Machine::with(files);
                m.search(None);
                assert_eq!(m.release_reads.get(), 1, "{files:?}");
            }
        }

        #[test]
        fn prefers_the_newest_release_and_the_first_path_dir() {
            let m = Machine::with(&["/r/0.10.0/herdr", "/r/0.9.3/herdr"]);
            assert_eq!(m.search(None).exe, PathBuf::from("/r/0.10.0/herdr"));
            let m = Machine::with(&["/login/a/herdr", "/login/b/herdr"]);
            assert_eq!(m.search(None).exe, PathBuf::from("/login/a/herdr"));
        }

        #[test]
        fn skips_a_configured_path_that_is_not_executable() {
            let m = Machine::with(&["/usr/local/bin/herdr"]);
            let d = m.search(Some("/cfg/herdr"));
            assert_eq!(d.exe, PathBuf::from("/usr/local/bin/herdr"));
            assert!(d.found && !d.explicit);
            // Only what the user named and the hit: no full list for a hit.
            assert_eq!(d.searched, pbs(&["/cfg/herdr", "/usr/local/bin/herdr"]));
        }

        #[test]
        fn not_found_is_the_bare_name_with_everything_searched_in_order() {
            let mut m = Machine::with(&[]);
            m.env = Some("/env/herdr");
            let d = m.search(Some("/cfg/herdr"));
            assert_eq!(d.exe, PathBuf::from("herdr"));
            assert!(!d.found && !d.explicit && !d.login_deferred);
            assert_eq!(
                d.searched,
                pbs(&[
                    "/cfg/herdr",
                    "/env/herdr",
                    "/proc/a/herdr",
                    "/proc/b/herdr",
                    "/opt/homebrew/bin/herdr",
                    "/usr/local/bin/herdr",
                    "/r/0.10.0/herdr",
                    "/r/0.9.3/herdr",
                    "/login/a/herdr",
                    "/login/b/herdr",
                ])
            );
            // Named in the short message: what the user named, then the usual places.
            assert_eq!(
                d.notable,
                pbs(&[
                    "/cfg/herdr",
                    "/env/herdr",
                    "/opt/homebrew/bin/herdr",
                    "/usr/local/bin/herdr",
                ])
            );
        }

        #[test]
        fn with_the_login_search_deferred_never_runs_the_shell() {
            // Only the login PATH has it: the launch search leaves that for later.
            let m = Machine::with(&["/login/a/herdr"]);
            let d = m.search_with(None, false);
            assert_eq!((d.exe.clone(), d.found), (PathBuf::from("herdr"), false));
            assert!(d.login_deferred);
            assert_eq!(m.login_runs.get(), 0);
            assert!(!d.searched.iter().any(|p| p.starts_with("/login")));
            // A hit elsewhere leaves nothing to finish.
            let m = Machine::with(&["/proc/a/herdr"]);
            let d = m.search_with(None, false);
            assert!(d.found && !d.login_deferred);
            assert_eq!(m.login_runs.get(), 0);
        }
    }

    #[test]
    fn discover_exe_without_a_login_step_or_any_path_finds_nothing() {
        // As on Windows with nothing installed: no login step, no usual places.
        let no_releases = || Vec::new();
        let d = discover_exe_with(
            &pty_cfg(None),
            DiscoveryInputs {
                env_override: None,
                process_path: None,
                well_known: Vec::new(),
                releases: &no_releases,
                login_path: LoginPath::Unavailable,
                find_on_path: &|_| panic!("there is no PATH to scan"),
                is_executable: &|_| false,
            },
        );
        assert_eq!((d.exe.clone(), d.found), (PathBuf::from("herdr"), false));
        assert!(!d.login_deferred && !d.explicit);
        assert!(d.searched.is_empty() && d.notable.is_empty());
    }

    #[test]
    fn with_exe_and_home_never_runs_the_login_shell() {
        let tmp = tempfile::tempdir().unwrap();
        let env =
            HerdrEnv::with_exe_and_home(PathBuf::from("/x/herdr"), tmp.path().join("h")).unwrap();
        assert!(matches!(env.path_source, PathSource::None));
        assert!(!env.path_source.runs_login_shell());
        assert_eq!(env.path(), None);
        // What production builds instead: on unix the source that would run it.
        assert_eq!(PathSource::spawn_path().runs_login_shell(), cfg!(unix));
        assert_eq!(
            HerdrEnvOptions::from_process().path.runs_login_shell(),
            cfg!(unix)
        );
    }

    #[test]
    fn a_lazy_path_is_resolved_on_first_use_and_only_once() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let runs = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&runs);
        let mut o = opts(vec![]);
        o.path = PathSource::lazy(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Some("/lazy/bin:/usr/bin".to_string())
        });
        let tmp = tempfile::tempdir().unwrap();
        let env = HerdrEnv::with_options(PathBuf::from("herdr"), tmp.path().join("h"), o).unwrap();
        // Building, cloning and printing the env resolve nothing.
        let copy = env.clone();
        let _ = format!("{env:?}");
        assert_eq!(runs.load(Ordering::SeqCst), 0);

        let mut cmd = std::process::Command::new("herdr");
        env.apply(&mut cmd);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(cmd
            .get_envs()
            .any(|(k, v)| k == "PATH" && v == Some(std::ffi::OsStr::new("/lazy/bin:/usr/bin"))));
        // Later uses, and clones, reuse the value.
        assert_eq!(env.path(), Some("/lazy/bin:/usr/bin"));
        assert_eq!(copy.path(), Some("/lazy/bin:/usr/bin"));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn explicit_env_records_a_found_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let env =
            HerdrEnv::with_exe_and_home(PathBuf::from("/x/herdr"), tmp.path().join("h")).unwrap();
        assert_eq!(
            env.discovery,
            ExeDiscovery::explicit(PathBuf::from("/x/herdr"))
        );
    }

    #[test]
    fn prepend_dirs_to_path_prepends_and_dedups() {
        assert_eq!(
            prepend_dirs_to_path(&["/opt/bin"], Some("/usr/bin:/opt/bin:/bin"), ':'),
            "/opt/bin:/usr/bin:/bin"
        );
        assert_eq!(
            prepend_dirs_to_path(&[r"C:\tools"], Some(r"C:\Windows;C:\tools;"), ';'),
            r"C:\tools;C:\Windows"
        );
        assert_eq!(
            prepend_dirs_to_path(&["/a", "/b", "/a"], Some("/b:/c:/a"), ':'),
            "/a:/b:/c"
        );
    }

    #[test]
    fn prepend_dirs_to_path_handles_missing_base() {
        assert_eq!(prepend_dirs_to_path(&["/opt/bin"], None, ':'), "/opt/bin");
        assert_eq!(
            prepend_dirs_to_path(&["/opt/bin"], Some(""), ':'),
            "/opt/bin"
        );
    }

    const ALIAS: &str = r"C:\Users\u\AppData\Local\keitora\bin";
    const EXE_DIR: &str = r"C:\agents\bin";
    const BASE: &str = r"C:\Windows;C:\tools";

    fn base() -> Option<String> {
        Some(BASE.to_string())
    }

    #[test]
    fn pane_path_bare_exe_without_alias_is_none_and_skips_base() {
        let path = pane_path(None, || panic!("base must not be read"), None, ';');
        assert_eq!(path, None);
    }

    #[test]
    fn pane_path_bare_exe_with_alias_puts_the_alias_first() {
        let path = pane_path(None, base, Some(Path::new(ALIAS)), ';');
        assert_eq!(path.as_deref(), Some(format!("{ALIAS};{BASE}").as_str()));
    }

    #[test]
    fn pane_path_orders_alias_then_exe_dir_then_base() {
        let path = pane_path(Some(EXE_DIR), base, Some(Path::new(ALIAS)), ';');
        assert_eq!(
            path.as_deref(),
            Some(format!("{ALIAS};{EXE_DIR};{BASE}").as_str())
        );
    }

    #[test]
    fn pane_path_without_alias_is_exe_dir_then_base() {
        let path = pane_path(Some(EXE_DIR), base, None, ';');
        assert_eq!(path.as_deref(), Some(format!("{EXE_DIR};{BASE}").as_str()));
    }

    #[test]
    fn pane_path_dedups_an_alias_equal_to_the_exe_dir_or_in_base() {
        let path = pane_path(Some(ALIAS), base, Some(Path::new(ALIAS)), ';');
        assert_eq!(path.as_deref(), Some(format!("{ALIAS};{BASE}").as_str()));
        let with_alias = || Some(format!("{BASE};{ALIAS}"));
        let path = pane_path(None, with_alias, Some(Path::new(ALIAS)), ';');
        assert_eq!(path.as_deref(), Some(format!("{ALIAS};{BASE}").as_str()));
    }

    #[test]
    fn exe_dir_none_for_bare_name() {
        assert_eq!(exe_dir("claude"), None);
        assert_eq!(exe_dir(""), None);
    }

    fn sample_original() -> OriginalXdg {
        OriginalXdg {
            config_home: Some("/u/.config".into()),
            state_home: None,
            data_home: Some("/u/.local/share".into()),
            cache_home: None,
            runtime_dir: Some("/run/user/1".into()),
        }
    }

    #[test]
    fn pane_env_bare_exe_is_exactly_the_xdg_pairs_on_windows() {
        if cfg!(unix) {
            return;
        }
        let original = sample_original();
        assert_eq!(pane_env("claude", &original, None), pane_xdg_env(&original));
        assert_eq!(pane_env("claude", &original, None).len(), 4);
    }

    #[cfg(unix)]
    #[test]
    fn pane_env_bare_exe_on_unix_carries_the_herdr_path_then_xdg() {
        let original = sample_original();
        let env = pane_env("claude", &original, Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(
            env[0],
            ("PATH".to_string(), "/opt/homebrew/bin:/usr/bin".to_string())
        );
        assert_eq!(env[1..], pane_xdg_env(&original)[..]);
        assert_eq!(env.len(), 5);
        // Without a herdr PATH there is nothing to build on.
        assert_eq!(pane_env("claude", &original, None), pane_xdg_env(&original));
    }

    #[cfg(unix)]
    #[test]
    fn pane_env_exe_dir_goes_ahead_of_the_herdr_path_without_duplicates() {
        let env = pane_env(
            "/usr/bin/claude",
            &sample_original(),
            Some("/opt/homebrew/bin:/usr/bin"),
        );
        assert_eq!(env[0].1, "/usr/bin:/opt/homebrew/bin");
    }

    #[cfg(unix)]
    #[test]
    fn pane_shell_env_on_unix_leads_with_the_herdr_path() {
        let original = sample_original();
        let env = pane_shell_env(&original, Some("/a:/b"));
        assert_eq!(env[0], ("PATH".to_string(), "/a:/b".to_string()));
        assert_eq!(env[1..], pane_xdg_env(&original)[..]);
        assert_eq!(pane_shell_env(&original, None), pane_xdg_env(&original));
    }

    #[cfg(unix)]
    #[test]
    fn pane_path_always_builds_on_the_base_for_a_bare_name() {
        let got = pane_path_with(None, || Some("/a:/b".into()), None, ':', true);
        assert_eq!(got.as_deref(), Some("/a:/b"));
        assert_eq!(pane_path_with(None, || None, None, ':', true), None);
    }

    #[test]
    fn pane_env_prepends_exe_dir_to_path_then_xdg() {
        #[cfg(windows)]
        let exe = r"C:\agents\bin\claude.exe";
        #[cfg(unix)]
        let exe = "/agents/bin/claude";
        let original = sample_original();
        let env = pane_env(exe, &original, Some("/base/bin"));
        assert_eq!(env.len(), 5);
        assert_eq!(env[0].0, "PATH");
        #[cfg(windows)]
        assert!(env[0].1.starts_with(r"C:\agents\bin"), "{}", env[0].1);
        #[cfg(unix)]
        assert!(env[0].1.starts_with("/agents/bin"), "{}", env[0].1);
        assert_eq!(env[1..], pane_xdg_env(&original)[..]);
    }

    fn pairs(want: &[(&str, &str)]) -> Vec<(String, String)> {
        want.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    const HOME: &str = "/home/u";

    fn home_join(rel: &str) -> String {
        Path::new(HOME).join(rel).to_string_lossy().into_owned()
    }

    #[test]
    fn pane_xdg_env_passes_set_values_through_in_order() {
        let all = OriginalXdg {
            config_home: Some("/a".into()),
            state_home: Some("/b".into()),
            data_home: Some("/c".into()),
            cache_home: Some("/d".into()),
            runtime_dir: Some("/e".into()),
        };
        let want = pairs(&[
            ("XDG_CONFIG_HOME", "/a"),
            ("XDG_STATE_HOME", "/b"),
            ("XDG_DATA_HOME", "/c"),
            ("XDG_CACHE_HOME", "/d"),
            ("XDG_RUNTIME_DIR", "/e"),
        ]);
        for native in [true, false] {
            for home in [Some(Path::new(HOME)), None] {
                assert_eq!(pane_xdg_env_for(&all, native, home), want);
            }
        }
    }

    #[test]
    fn pane_xdg_env_on_xdg_native_defaults_config_and_state_and_omits_the_rest() {
        let home = Some(Path::new(HOME));
        assert_eq!(
            pane_xdg_env_for(&sample_original(), true, home),
            pairs(&[
                ("XDG_CONFIG_HOME", "/u/.config"),
                ("XDG_STATE_HOME", &home_join(".local/state")),
                ("XDG_DATA_HOME", "/u/.local/share"),
                ("XDG_RUNTIME_DIR", "/run/user/1"),
            ])
        );
        assert_eq!(
            pane_xdg_env_for(&OriginalXdg::default(), true, home),
            pairs(&[
                ("XDG_CONFIG_HOME", &home_join(".config")),
                ("XDG_STATE_HOME", &home_join(".local/state")),
            ])
        );
    }

    #[test]
    fn pane_xdg_env_off_xdg_native_empties_config_but_still_defaults_state() {
        // Claude Code keeps its locks under $XDG_STATE_HOME/claude; an empty
        // value put them in the pane's cwd, on Windows too.
        let home = Some(Path::new(HOME));
        assert_eq!(
            pane_xdg_env_for(&OriginalXdg::default(), false, home),
            pairs(&[
                ("XDG_CONFIG_HOME", ""),
                ("XDG_STATE_HOME", &home_join(".local/state")),
            ])
        );
    }

    #[test]
    fn pane_xdg_env_without_a_home_empties_the_defaults() {
        for native in [true, false] {
            assert_eq!(
                pane_xdg_env_for(&OriginalXdg::default(), native, None),
                pairs(&[("XDG_CONFIG_HOME", ""), ("XDG_STATE_HOME", "")])
            );
        }
    }

    #[test]
    fn pane_xdg_env_never_sets_data_cache_or_runtime_when_unset() {
        // An empty XDG_DATA_HOME / XDG_CACHE_HOME made Claude Code's updater
        // install into a relative `claude/` dir in the pane's cwd.
        let env = pane_xdg_env(&OriginalXdg::default());
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["XDG_CONFIG_HOME", "XDG_STATE_HOME"]);
        assert!(env.iter().all(|(k, _)| !k.starts_with("HERDR_")));
    }

    #[test]
    fn xdg_is_native_only_off_macos_and_windows() {
        assert_eq!(XDG_NATIVE, !cfg!(windows) && !cfg!(target_os = "macos"));
    }

    fn lookup_from(pairs: Vec<(&'static str, OsString)>) -> impl Fn(&str) -> Option<OsString> {
        move |k: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.clone())
        }
    }

    fn root() -> PathBuf {
        #[cfg(windows)]
        return PathBuf::from(r"C:\Users\x\.keitora");
        #[cfg(unix)]
        return PathBuf::from("/home/x/.keitora");
    }

    fn under_root(rel: &str) -> OsString {
        root().join(rel).into_os_string()
    }

    #[test]
    fn original_xdg_passes_all_set_values_through() {
        let lookup = lookup_from(vec![
            ("XDG_CONFIG_HOME", "/a".into()),
            ("XDG_STATE_HOME", "/b".into()),
            ("XDG_DATA_HOME", "/c".into()),
            ("XDG_CACHE_HOME", "/d".into()),
            ("XDG_RUNTIME_DIR", "/e".into()),
        ]);
        let o = OriginalXdg::from_lookup(&lookup, &root());
        assert_eq!(
            o,
            OriginalXdg {
                config_home: Some("/a".into()),
                state_home: Some("/b".into()),
                data_home: Some("/c".into()),
                cache_home: Some("/d".into()),
                runtime_dir: Some("/e".into()),
            }
        );
        assert!(OriginalXdg::contaminated(&lookup, &root()).is_empty());
    }

    #[test]
    fn original_xdg_unset_and_empty_read_as_none() {
        let lookup = lookup_from(vec![("XDG_CONFIG_HOME", "".into())]);
        assert_eq!(
            OriginalXdg::from_lookup(&lookup, &root()),
            OriginalXdg::default()
        );
        // Unset or empty is not contamination.
        assert!(OriginalXdg::contaminated(&lookup, &root()).is_empty());
    }

    #[test]
    fn original_xdg_discards_values_under_the_keitora_root() {
        let outside = if cfg!(windows) {
            r"C:\Users\x\.config"
        } else {
            "/home/x/.config"
        };
        let lookup = lookup_from(vec![
            ("XDG_CONFIG_HOME", under_root("herdr/config")),
            ("XDG_STATE_HOME", outside.into()),
            ("XDG_CACHE_HOME", under_root("herdr/cache")),
            ("XDG_RUNTIME_DIR", under_root("other-home/herdr/runtime")),
        ]);
        let o = OriginalXdg::from_lookup(&lookup, &root());
        assert_eq!(o.config_home, None);
        assert_eq!(o.state_home.as_deref(), Some(outside));
        assert_eq!(o.data_home, None);
        assert_eq!(o.cache_home, None);
        assert_eq!(o.runtime_dir, None);
        assert_eq!(
            OriginalXdg::contaminated(&lookup, &root()),
            ["XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"]
        );
    }

    #[test]
    fn original_xdg_prefix_match_is_by_path_component() {
        // `.keitora-other` is not under `.keitora`.
        let sibling = format!("{}-other", root().display());
        let lookup = lookup_from(vec![("XDG_CONFIG_HOME", sibling.clone().into())]);
        let o = OriginalXdg::from_lookup(&lookup, &root());
        assert_eq!(o.config_home, Some(sibling));
    }

    #[cfg(unix)]
    #[test]
    fn original_xdg_non_utf8_reads_as_none_and_is_contaminated() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![0x2f, 0xff, 0xfe]);
        let lookup = lookup_from(vec![("XDG_DATA_HOME", bad)]);
        assert_eq!(OriginalXdg::from_lookup(&lookup, &root()).data_home, None);
        assert_eq!(
            OriginalXdg::contaminated(&lookup, &root()),
            ["XDG_DATA_HOME"]
        );
    }

    fn opts(pairs: Vec<(&'static str, OsString)>) -> HerdrEnvOptions {
        HerdrEnvOptions::from_lookup(lookup_from(pairs), &root())
    }

    #[test]
    fn with_options_sets_three_keys_and_creates_only_config_and_state() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("herdr");
        let o = opts(vec![
            ("XDG_CONFIG_HOME", "/u/cfg".into()),
            ("XDG_DATA_HOME", under_root("herdr/data")),
            ("XDG_CACHE_HOME", "/u/cache".into()),
            ("XDG_RUNTIME_DIR", under_root("herdr/runtime")),
            ("XDG_STATE_HOME", under_root("herdr/state")),
        ]);
        let env = HerdrEnv::with_options(PathBuf::from("herdr"), home.clone(), o).unwrap();

        let keys: Vec<&str> = env.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "HERDR_CONFIG_PATH"]
        );
        let mut dirs: Vec<String> = std::fs::read_dir(&home)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        dirs.sort();
        assert_eq!(dirs, ["config", "state"]);
        // Only the contaminated pass-through vars are removed; a contaminated
        // XDG_STATE_HOME is overridden by the isolated value anyway.
        assert_eq!(env.env_remove, ["XDG_DATA_HOME", "XDG_RUNTIME_DIR"]);
        assert_eq!(env.original_xdg.config_home.as_deref(), Some("/u/cfg"));
        assert_eq!(env.original_xdg.state_home, None);
        assert_eq!(env.keymap_path(), home.join("keymap.json"));
    }

    #[cfg(unix)]
    #[test]
    fn with_options_adds_the_spawn_path_and_apply_sets_it() {
        let tmp = tempfile::tempdir().unwrap();
        let mut o = opts(vec![]);
        o.path = PathSource::Fixed("/opt/homebrew/bin:/usr/local/bin:/usr/bin".to_string());
        let env = HerdrEnv::with_options(PathBuf::from("herdr"), tmp.path().join("h"), o).unwrap();
        assert_eq!(
            env.path(),
            Some("/opt/homebrew/bin:/usr/local/bin:/usr/bin")
        );
        let mut cmd = std::process::Command::new("herdr");
        env.apply(&mut cmd);
        let set: Vec<_> = cmd
            .get_envs()
            .filter(|(k, _)| *k == "PATH")
            .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(
            set,
            vec![Some(
                "/opt/homebrew/bin:/usr/local/bin:/usr/bin".to_string()
            )]
        );
    }

    #[test]
    fn with_options_without_a_spawn_path_adds_no_path_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let env =
            HerdrEnv::with_options(PathBuf::from("herdr"), tmp.path().join("h"), opts(vec![]))
                .unwrap();
        assert_eq!(env.path(), None);
    }

    /// A home whose socket path is exactly `len` bytes long.
    fn home_with_socket_len(len: usize) -> PathBuf {
        let tail = "/config/herdr/herdr.sock.agent".len();
        PathBuf::from(format!("/{}", "h".repeat(len - tail - 1)))
    }

    #[test]
    fn check_socket_path_accepts_the_limit_and_rejects_one_over() {
        let at = home_with_socket_len(103);
        assert!(check_socket_path(&at, 103).is_ok());
        let under = home_with_socket_len(102);
        assert!(check_socket_path(&under, 103).is_ok());
        let over = home_with_socket_len(104);
        let err = check_socket_path(&over, 103).unwrap_err();
        assert!(err.contains("104 bytes"), "{err}");
        assert!(err.contains("103"), "{err}");
        assert!(err.contains("herdr.sock.agent"), "{err}");
        assert!(err.contains("KEITORA_HOME"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn with_options_rejects_a_home_over_the_socket_limit_without_creating_it() {
        let home = home_with_socket_len(SOCKET_PATH_LIMIT + 1);
        let err =
            HerdrEnv::with_options(PathBuf::from("herdr"), home.clone(), opts(vec![])).unwrap_err();
        assert!(err.to_string().contains("KEITORA_HOME"), "{err}");
        assert!(!home.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_typical_temp_home_fits_the_socket_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("herdr");
        assert!(
            check_socket_path(&home, SOCKET_PATH_LIMIT).is_ok(),
            "temp dir {} is too long for tests",
            tmp.path().display()
        );
    }

    #[test]
    fn with_options_leaves_old_dirs_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join("data").join("claude")).unwrap();
        let env =
            HerdrEnv::with_options(PathBuf::from("herdr"), home.clone(), opts(vec![])).unwrap();
        assert!(home.join("data").join("claude").is_dir());
        assert!(!home.join("cache").exists());
        assert!(env.env_remove.is_empty());
    }

    #[test]
    fn apply_sets_pairs_and_removes_names() {
        let tmp = tempfile::tempdir().unwrap();
        let o = opts(vec![("XDG_CACHE_HOME", under_root("herdr/cache"))]);
        let env =
            HerdrEnv::with_options(PathBuf::from("herdr"), tmp.path().to_path_buf(), o).unwrap();
        let mut cmd = std::process::Command::new("herdr");
        env.apply(&mut cmd);
        let got: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for (k, v) in &env.env {
            assert!(got.contains(&(k.clone(), Some(v.clone()))), "missing {k}");
        }
        assert!(
            got.contains(&("XDG_CACHE_HOME".to_string(), None)),
            "{got:?}"
        );
        assert_eq!(got.len(), 4);
    }

    fn assert_herdr_log_path(home: &Path) {
        let p = herdr_log_path(home);
        assert!(p.starts_with(home), "{}", p.display());
        assert!(
            p.ends_with(Path::new("config").join("herdr").join("herdr-server.log")),
            "{}",
            p.display()
        );
    }

    #[cfg(windows)]
    #[test]
    fn herdr_log_path_under_windows_home() {
        assert_herdr_log_path(Path::new(r"C:\Users\x\.keitora\herdr"));
    }

    #[cfg(unix)]
    #[test]
    fn herdr_log_path_under_unix_home() {
        assert_herdr_log_path(Path::new("/home/x/.keitora/herdr"));
    }

    #[test]
    fn with_exe_and_home_writes_config_and_builds_env() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().to_path_buf();
        let env = HerdrEnv::with_options(PathBuf::from("herdr"), tmp.clone(), opts(vec![]))
            .expect("env builds");

        // Only what herdr reads is overridden.
        let keys: Vec<&str> = env.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "HERDR_CONFIG_PATH"]
        );
        for absent in ["XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"] {
            assert!(!keys.contains(&absent), "unexpected {absent}");
        }

        // The config file was written with the self-update kill switches.
        let cfg_path = tmp.join("config").join("herdr").join("config.toml");
        let written = std::fs::read_to_string(&cfg_path).expect("config written");
        assert!(written.contains("onboarding = false"));
        assert!(written.contains("version_check = false"));
        assert!(written.contains("manifest_check = false"));
        assert!(written.contains("[session]"));
        assert!(written.contains("resume_agents_on_restore = false"));

        // The keymap sidecar resolves directly under the herdr home.
        assert_eq!(env.keymap_path(), tmp.join("keymap.json"));

        // herdr's log lives beside the config Keitora writes.
        assert_eq!(env.log_path().parent(), cfg_path.parent());
        let log = env.log_path();
        assert!(log.is_absolute(), "{}", log.display());
        assert!(!log.to_string_lossy().is_empty());
        assert!(log.to_string_lossy().ends_with("herdr-server.log"));
    }

    /// Every path under `root`, relative, sorted (dirs and files).
    fn tree(root: &Path) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                if e.file_type().unwrap().is_dir() {
                    walk(root, &p, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    /// A herdr home with leftover tool folders (empty dirs only), herdr's own
    /// folder, a stray file and no `cache/`.
    fn leaky_home() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let h = tmp.path();
        for d in ["config/gh", "config/herdr", "state/gh", "data/claude"] {
            std::fs::create_dir_all(h.join(d)).unwrap();
        }
        std::fs::write(h.join("config").join("x.txt"), "").unwrap();
        tmp
    }

    #[test]
    fn leaked_dirs_lists_tool_dirs_only() {
        let home = leaky_home();
        assert_eq!(
            leaked_dirs(home.path()),
            ["config/gh", "data/claude", "state/gh"]
        );
    }

    #[test]
    fn leaked_dirs_of_a_missing_home_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(leaked_dirs(&tmp.path().join("absent")).is_empty());
    }

    #[test]
    fn take_xdg_leak_notice_answers_once_and_only_writes_the_marker() {
        let home = leaky_home();
        let before = tree(home.path());
        assert_eq!(
            take_xdg_leak_notice(home.path()),
            Some(vec![
                "config/gh".to_string(),
                "data/claude".to_string(),
                "state/gh".to_string()
            ])
        );
        assert!(xdg_note_marker(home.path()).is_file());
        assert_eq!(take_xdg_leak_notice(home.path()), None);

        let mut want = before;
        want.push("xdg-note-shown".to_string());
        want.sort();
        assert_eq!(tree(home.path()), want);
    }

    #[test]
    fn take_xdg_leak_notice_on_a_clean_home_is_none_but_marks() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(take_xdg_leak_notice(tmp.path()), None);
        assert!(xdg_note_marker(tmp.path()).is_file());
    }

    #[test]
    fn take_xdg_leak_notice_with_an_unwritable_marker_is_none() {
        let home = leaky_home();
        // A directory where the marker file should go: the existence check sees
        // it as already shown, and a write would fail either way.
        std::fs::create_dir(xdg_note_marker(home.path())).unwrap();
        assert_eq!(take_xdg_leak_notice(home.path()), None);
    }

    #[test]
    fn take_xdg_leak_notice_when_the_marker_write_fails_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        // The home itself is missing, so the marker cannot be written.
        assert_eq!(take_xdg_leak_notice(&tmp.path().join("absent")), None);
    }

    #[test]
    fn existing_config_is_rewritten_on_resolve() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("config").join("herdr").join("config.toml");
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        std::fs::write(&cfg_path, "onboarding = false\n").unwrap();
        HerdrEnv::with_options(
            PathBuf::from("herdr"),
            tmp.path().to_path_buf(),
            opts(vec![]),
        )
        .expect("env builds");
        assert_eq!(
            std::fs::read_to_string(&cfg_path).unwrap(),
            herdr_config_toml(DEFAULT_HERDR_SCROLLBACK_LIMIT_BYTES)
        );
    }

    /// The lines of `section` (`""` = top level) in a flat TOML text.
    fn section<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
        let mut current = "";
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if let Some(h) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                current = h;
            } else if current == name && !line.is_empty() {
                out.push(line);
            }
        }
        out
    }

    #[test]
    fn herdr_config_toml_puts_each_key_in_its_section() {
        let text = herdr_config_toml(4_000_000);
        assert_eq!(section(&text, ""), ["onboarding = false"]);
        assert_eq!(
            section(&text, "update"),
            ["version_check = false", "manifest_check = false"]
        );
        assert_eq!(
            section(&text, "session"),
            ["resume_agents_on_restore = false"]
        );
        assert_eq!(
            section(&text, "advanced"),
            ["scrollback_limit_bytes = 4000000"]
        );
    }

    #[test]
    fn herdr_config_toml_writes_a_custom_scrollback() {
        let text = herdr_config_toml(250_000);
        assert_eq!(
            section(&text, "advanced"),
            ["scrollback_limit_bytes = 250000"]
        );
    }

    #[test]
    fn with_options_writes_the_configured_scrollback_over_an_old_config() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("config").join("herdr").join("config.toml");
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        std::fs::write(&cfg_path, herdr_config_toml(10_000_000)).unwrap();
        let mut o = opts(vec![]);
        o.scrollback_limit_bytes = 2_000_000;
        HerdrEnv::with_options(PathBuf::from("herdr"), tmp.path().to_path_buf(), o).unwrap();
        assert_eq!(
            std::fs::read_to_string(&cfg_path).unwrap(),
            herdr_config_toml(2_000_000)
        );
    }

    #[test]
    fn clamp_scrollback_accepts_only_the_range() {
        let d = DEFAULT_HERDR_SCROLLBACK_LIMIT_BYTES;
        assert_eq!(d, 16_000_000);
        assert_eq!(clamp_scrollback(None), d);
        assert_eq!(clamp_scrollback(Some(2_000_000)), 2_000_000);
        assert_eq!(clamp_scrollback(Some(100_000)), 100_000);
        assert_eq!(clamp_scrollback(Some(100_000_000)), 100_000_000);
        assert_eq!(clamp_scrollback(Some(99_999)), d);
        assert_eq!(clamp_scrollback(Some(100_000_001)), d);
        assert_eq!(clamp_scrollback(Some(0)), d);
    }
}
