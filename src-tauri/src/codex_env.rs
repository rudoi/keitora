// Codex environment resolution shared by the transfer import and the strict pane launch.
//
// Everything that reads the machine (configured path, discovery, PATH, `CODEX_HOME`, the
// home directory) goes through [`CodexEnvProbe`], so [`resolve_codex_env`] is a pure
// function that tests on any OS. The `codex --version` probe is injectable and cached.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use keitora_tasks::{CliKind, ProjectConfig, UserConfig};

/// Where the resolved `codex` executable came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExeSource {
    Configured,
    Discovered,
    BareName,
}

/// Where the resolved Codex home directory came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexHomeSource {
    /// Reported by Codex itself in the `initialize` result.
    Initialize,
    /// The `CODEX_HOME` environment variable.
    EnvVar,
    /// `~/.codex`.
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexEnv {
    /// Absolute path when configured or discovered, else `"codex"`.
    pub exe: String,
    pub exe_source: ExeSource,
    /// Value to set as `PATH` on the import child; `None` leaves the inherited value.
    pub path: Option<String>,
    /// Resolved before spawn (env var, else `~/.codex`); see [`CodexEnv::with_initialize_home`].
    pub codex_home: PathBuf,
    pub codex_home_source: CodexHomeSource,
    /// e.g. `"0.166.0"`; `None` if the probe failed.
    pub version: Option<String>,
}

/// Everything resolution reads from the machine, injectable for tests.
pub struct CodexEnvProbe<'a> {
    /// `keitora_tasks::resolve_provider_path(Codex, Some(&project), &user)`.
    pub configured: Option<String>,
    /// Production: `keitora_tasks::discover_cli_cached(Codex)`.
    pub discover: &'a dyn Fn() -> Option<String>,
    /// Unix: `keitora_tasks::unix_spawn_path()`; Windows: the process `PATH`.
    pub base_path: Option<String>,
    /// `std::env::var("CODEX_HOME").ok()`.
    pub env_codex_home: Option<String>,
    /// `dirs::home_dir()`.
    pub home_dir: Option<PathBuf>,
    /// `':'` on unix, `';'` on Windows.
    pub path_sep: char,
}

/// Pure resolution of the executable, `PATH` and `CODEX_HOME`. `version` is left `None`;
/// fill it with [`CodexEnv::with_version`].
pub fn resolve_codex_env(p: &CodexEnvProbe) -> Result<CodexEnv, String> {
    let configured = p.configured.clone().filter(|s| !s.trim().is_empty());
    let (exe, exe_source) = match configured {
        Some(c) => (c, ExeSource::Configured),
        None => match (p.discover)().filter(|s| !s.trim().is_empty()) {
            Some(d) => (d, ExeSource::Discovered),
            None => ("codex".to_string(), ExeSource::BareName),
        },
    };

    let exe_dir = Path::new(&exe)
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .filter(|d| !d.is_empty());
    let path = compose_path(exe_dir.as_deref(), p.base_path.as_deref(), p.path_sep);

    let env_home = p.env_codex_home.as_deref().filter(|s| !s.is_empty());
    let (codex_home, codex_home_source) = match env_home {
        Some(h) => (PathBuf::from(h), CodexHomeSource::EnvVar),
        None => match &p.home_dir {
            Some(h) => (h.join(".codex"), CodexHomeSource::Default),
            None => return Err("could not resolve home directory for CODEX_HOME".to_string()),
        },
    };

    Ok(CodexEnv {
        exe,
        exe_source,
        path,
        codex_home,
        codex_home_source,
        version: None,
    })
}

/// Build the probe from the real machine. Runs the login shell on unix the first time, so
/// never call it on the app's setup thread.
pub fn probe_codex_env_from_machine(
    project: &ProjectConfig,
    user: &UserConfig,
) -> CodexEnvProbe<'static> {
    #[cfg(unix)]
    let (base_path, path_sep) = (keitora_tasks::unix_spawn_path(), ':');
    #[cfg(not(unix))]
    let (base_path, path_sep) = (
        std::env::var_os("PATH").map(|s| s.to_string_lossy().into_owned()),
        ';',
    );
    CodexEnvProbe {
        configured: keitora_tasks::resolve_provider_path(CliKind::Codex, Some(project), user),
        discover: &|| keitora_tasks::discover_cli_cached(CliKind::Codex),
        base_path,
        env_codex_home: std::env::var("CODEX_HOME").ok(),
        home_dir: dirs::home_dir(),
        path_sep,
    }
}

impl CodexEnv {
    pub fn with_version(mut self, v: Option<String>) -> CodexEnv {
        self.version = v;
        self
    }

    /// Apply the `codexHome` reported by `initialize`; empty or `None` leaves it unchanged.
    /// The result is for the ledger lookup only: the child was already spawned with the
    /// pre-initialize value.
    pub fn with_initialize_home(mut self, home: Option<&str>) -> CodexEnv {
        if let Some(h) = home.filter(|h| !h.is_empty()) {
            self.codex_home = PathBuf::from(h);
            self.codex_home_source = CodexHomeSource::Initialize;
        }
        self
    }
}

/// Executable directory first, then the entries of `base` in order; duplicates and empty
/// entries dropped. `None` when nothing is left.
pub fn compose_path(exe_dir: Option<&str>, base: Option<&str>, sep: char) -> Option<String> {
    let mut seen: Vec<&str> = Vec::new();
    let candidates = exe_dir
        .into_iter()
        .chain(base.into_iter().flat_map(|b| b.split(sep)));
    for entry in candidates {
        if !entry.is_empty() && !seen.contains(&entry) {
            seen.push(entry);
        }
    }
    if seen.is_empty() {
        None
    } else {
        Some(seen.join(&sep.to_string()))
    }
}

/// Extra environment for the strict pane launch: `CODEX_HOME` only when the user set it
/// explicitly, so the pane agrees with the import child. Otherwise Codex applies its default.
pub fn strict_pane_extra_env(env_codex_home: Option<String>) -> Vec<(String, String)> {
    match env_codex_home {
        Some(v) if !v.is_empty() => vec![("CODEX_HOME".to_string(), v)],
        _ => Vec::new(),
    }
}

/// First non-empty line, last whitespace-separated token that starts with `N.N.N`
/// (`"codex-cli 0.166.0"` gives `"0.166.0"`; a pre-release suffix is kept).
pub fn parse_codex_version_output(stdout: &str) -> Option<String> {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    line.split_whitespace()
        .rev()
        .find(|tok| starts_with_semver(tok))
        .map(str::to_string)
}

fn starts_with_semver(tok: &str) -> bool {
    let mut parts = tok.splitn(3, '.');
    let digits = |s: Option<&str>, trailing_ok: bool| match s {
        Some(s) => {
            let n = s.chars().take_while(char::is_ascii_digit).count();
            n > 0 && (trailing_ok || n == s.len())
        }
        None => false,
    };
    digits(parts.next(), false) && digits(parts.next(), false) && digits(parts.next(), true)
}

type VersionCache = Mutex<HashMap<String, (Instant, Option<String>)>>;

/// Cache-aware version lookup: a `Some` is kept per exe for the process, a `None` for
/// [`keitora_tasks::DISCOVER_NONE_TTL`] (as CLI discovery does), then retried. `probe`
/// runs outside the lock.
fn codex_version_with(
    cache: &VersionCache,
    exe: &str,
    path: Option<&str>,
    now: Instant,
    probe: &dyn Fn(&str, Option<&str>) -> Option<String>,
) -> Option<String> {
    if let Some((at, found)) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(exe) {
        match found {
            Some(v) => return Some(v.clone()),
            None if now.saturating_duration_since(*at) < keitora_tasks::DISCOVER_NONE_TTL => {
                return None
            }
            None => {}
        }
    }
    let found = probe(exe, path);
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(exe.to_string(), (now, found.clone()));
    found
}

/// `codex --version`, cached per exe (a miss for 30 s), bounded to 5 s.
pub fn codex_version(exe: &str, path: Option<&str>) -> Option<String> {
    static CACHE: OnceLock<VersionCache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    codex_version_with(cache, exe, path, Instant::now(), &probe_codex_version)
}

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

fn probe_codex_version(exe: &str, path: Option<&str>) -> Option<String> {
    let mut cmd = crate::transfer::codex_command(exe, &["--version"]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(p) = path {
        cmd.env("PATH", p);
    }
    crate::backend::herdr::configure_command(&mut cmd);
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + VERSION_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    parse_codex_version_output(&out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn none() -> Option<String> {
        None
    }

    fn probe<'a>(
        configured: Option<&str>,
        discover: &'a dyn Fn() -> Option<String>,
        base_path: Option<&str>,
        env_home: Option<&str>,
        home: Option<&str>,
    ) -> CodexEnvProbe<'a> {
        CodexEnvProbe {
            configured: configured.map(str::to_string),
            discover,
            base_path: base_path.map(str::to_string),
            env_codex_home: env_home.map(str::to_string),
            home_dir: home.map(PathBuf::from),
            path_sep: ':',
        }
    }

    #[test]
    fn exe_prefers_configured_over_discovered() {
        let d = || Some("/disc/bin/codex".to_string());
        let env = resolve_codex_env(&probe(
            Some("/cfg/bin/codex"),
            &d,
            None,
            None,
            Some("/home/u"),
        ))
        .unwrap();
        assert_eq!(env.exe, "/cfg/bin/codex");
        assert_eq!(env.exe_source, ExeSource::Configured);
    }

    #[test]
    fn exe_discovered_when_unconfigured() {
        let d = || Some("/disc/bin/codex".to_string());
        let env = resolve_codex_env(&probe(None, &d, None, None, Some("/home/u"))).unwrap();
        assert_eq!(env.exe, "/disc/bin/codex");
        assert_eq!(env.exe_source, ExeSource::Discovered);
        assert_eq!(env.path.as_deref(), Some("/disc/bin"));
    }

    #[test]
    fn exe_falls_back_to_bare_codex() {
        let env = resolve_codex_env(&probe(None, &none, Some("/usr/bin"), None, Some("/home/u")))
            .unwrap();
        assert_eq!(env.exe, "codex");
        assert_eq!(env.exe_source, ExeSource::BareName);
        assert_eq!(env.path.as_deref(), Some("/usr/bin"), "no exe dir is added");
    }

    #[test]
    fn path_puts_exe_dir_first_then_base_deduped() {
        assert_eq!(
            compose_path(Some("/opt/x/bin"), Some("/usr/bin:/opt/x/bin:/bin"), ':').as_deref(),
            Some("/opt/x/bin:/usr/bin:/bin")
        );
        assert_eq!(compose_path(None, None, ':'), None);
        assert_eq!(
            compose_path(Some(""), Some(":/a::/b:"), ':').as_deref(),
            Some("/a:/b")
        );
        assert_eq!(
            compose_path(Some("C:/x/bin"), Some("C:/win;C:/x/bin;;C:/tools"), ';').as_deref(),
            Some("C:/x/bin;C:/win;C:/tools")
        );
    }

    #[test]
    fn path_uses_base_path_verbatim_when_exe_is_bare() {
        let env = resolve_codex_env(&probe(None, &none, Some("/a:/b:/c"), None, Some("/home/u")))
            .unwrap();
        assert_eq!(env.path.as_deref(), Some("/a:/b:/c"));
    }

    #[test]
    fn codex_home_precedence_initialize_then_env_then_default() {
        let env =
            resolve_codex_env(&probe(None, &none, None, Some("/x/ch"), Some("/home/u"))).unwrap();
        assert_eq!(env.codex_home, PathBuf::from("/x/ch"));
        assert_eq!(env.codex_home_source, CodexHomeSource::EnvVar);

        let default = resolve_codex_env(&probe(None, &none, None, None, Some("/home/u"))).unwrap();
        assert_eq!(default.codex_home, PathBuf::from("/home/u").join(".codex"));
        assert_eq!(default.codex_home_source, CodexHomeSource::Default);

        // An empty env var is ignored.
        let empty =
            resolve_codex_env(&probe(None, &none, None, Some(""), Some("/home/u"))).unwrap();
        assert_eq!(empty.codex_home_source, CodexHomeSource::Default);

        // Initialize wins over both, but empty/None leaves the value unchanged.
        let init = env.clone().with_initialize_home(Some("/init/home"));
        assert_eq!(init.codex_home, PathBuf::from("/init/home"));
        assert_eq!(init.codex_home_source, CodexHomeSource::Initialize);
        assert_eq!(env.clone().with_initialize_home(Some("")), env);
        assert_eq!(env.clone().with_initialize_home(None), env);
    }

    #[test]
    fn codex_home_errors_without_home_and_env() {
        let err = resolve_codex_env(&probe(None, &none, None, None, None)).unwrap_err();
        assert!(err.contains("home directory"), "{err}");
        let err = resolve_codex_env(&probe(None, &none, None, Some(""), None)).unwrap_err();
        assert!(err.contains("CODEX_HOME"), "{err}");
        // An explicit env var needs no home directory.
        assert!(resolve_codex_env(&probe(None, &none, None, Some("/x"), None)).is_ok());
    }

    #[test]
    fn strict_pane_extra_env_empty_and_set() {
        assert!(strict_pane_extra_env(None).is_empty());
        assert!(strict_pane_extra_env(Some(String::new())).is_empty());
        assert_eq!(
            strict_pane_extra_env(Some("/x/codex-home".into())),
            vec![("CODEX_HOME".to_string(), "/x/codex-home".to_string())]
        );
    }

    #[test]
    fn parse_codex_version_output_variants() {
        assert_eq!(
            parse_codex_version_output("codex-cli 0.166.0\n").as_deref(),
            Some("0.166.0")
        );
        assert_eq!(
            parse_codex_version_output("\n  codex 0.149.0-alpha.1").as_deref(),
            Some("0.149.0-alpha.1")
        );
        assert_eq!(parse_codex_version_output("garbage here"), None);
        assert_eq!(parse_codex_version_output(""), None);
        assert_eq!(parse_codex_version_output("v1.2"), None);
    }

    #[test]
    fn codex_version_caches_some_per_exe() {
        let cache = VersionCache::default();
        let now = Instant::now();
        let calls = Cell::new(0);
        let found = |_: &str, _: Option<&str>| {
            calls.set(calls.get() + 1);
            Some("0.166.0".to_string())
        };
        assert_eq!(
            codex_version_with(&cache, "/a/codex", None, now, &found).as_deref(),
            Some("0.166.0")
        );
        let much_later = now + keitora_tasks::DISCOVER_NONE_TTL * 10;
        assert_eq!(
            codex_version_with(&cache, "/a/codex", Some("/p"), much_later, &found).as_deref(),
            Some("0.166.0")
        );
        assert_eq!(calls.get(), 1, "a hit is kept for the process");
        // A different exe has its own entry.
        codex_version_with(&cache, "/b/codex", None, now, &found);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn codex_version_remembers_a_miss_until_the_ttl_passes() {
        let cache = VersionCache::default();
        let base = Instant::now();
        let calls = Cell::new(0);
        let missing = |_: &str, _: Option<&str>| {
            calls.set(calls.get() + 1);
            None
        };
        let found = |_: &str, _: Option<&str>| {
            calls.set(calls.get() + 1);
            Some("0.166.0".to_string())
        };
        assert_eq!(
            codex_version_with(&cache, "/c/codex", None, base, &missing),
            None
        );
        // Within the TTL the miss is answered from the cache, even if the exe now works.
        let soon = base + Duration::from_secs(10);
        assert_eq!(
            codex_version_with(&cache, "/c/codex", None, soon, &found),
            None
        );
        assert_eq!(calls.get(), 1, "a recent miss is not re-probed");
        // Past the TTL it is probed again.
        let later = base + keitora_tasks::DISCOVER_NONE_TTL + Duration::from_secs(1);
        assert_eq!(
            codex_version_with(&cache, "/c/codex", None, later, &found).as_deref(),
            Some("0.166.0")
        );
        assert_eq!(calls.get(), 2);
    }
}
