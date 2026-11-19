//! Machine reads for agent CLI updates: where each CLI is, its installed version, and
//! the latest release (Codex's own `version.json` when fresh, else the npm registry).
//! [`Prober`] is the seam the update state is built on, so tests never run a real CLI
//! or touch the network.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use keitora_tasks::{CliKind, UserConfig};

use super::model;
use crate::codex_env::{self, VersionCache};

const CODEX_LATEST_URL: &str = "https://registry.npmjs.org/@openai/codex/latest";
const CLAUDE_LATEST_URL: &str = "https://registry.npmjs.org/@anthropic-ai/claude-code/latest";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything an update check reads from the machine.
pub trait Prober: Send + Sync {
    /// The user config, loaded once per check.
    fn config(&self) -> UserConfig;
    /// The CLI's executable (configured in `cfg`, else discovered), or `None` when not
    /// found.
    fn exe(&self, cli: CliKind, cfg: &UserConfig) -> Option<String>;
    /// `exe` with symlinks followed, `/`-separated; `exe` itself when it cannot be
    /// resolved.
    fn resolve(&self, exe: &str) -> String;
    /// Whether `path` is an existing file.
    fn is_file(&self, path: &str) -> bool;
    /// `<exe> --version`, cached per exe.
    fn installed(&self, cli: CliKind, exe: &str) -> Option<String>;
    /// Forget the cached version of `exe` (after an update ran).
    fn invalidate(&self, cli: CliKind, exe: &str);
    /// The latest published version, or `None` when it cannot be learned.
    fn latest(&self, cli: CliKind) -> Option<String>;
}

/// The latest version of `cli`: for Codex, its `version.json` cache when that is less
/// than a day old; otherwise the npm registry through `fetch` (URL to body).
pub fn latest_with(
    cli: CliKind,
    version_json: impl FnOnce() -> Option<String>,
    now: chrono::DateTime<chrono::Utc>,
    fetch: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let url = match cli {
        CliKind::Codex => {
            if let Some(v) = version_json().and_then(|t| model::parse_codex_version_json(&t, now)) {
                return Some(v);
            }
            CODEX_LATEST_URL
        }
        CliKind::Claude => CLAUDE_LATEST_URL,
    };
    fetch(url).and_then(|body| model::parse_npm_latest(&body))
}

/// The real machine.
pub struct MachineProber;

/// The `claude --version` results, shared so an update can invalidate them.
static CLAUDE_VERSION_CACHE: OnceLock<VersionCache> = OnceLock::new();

/// `PATH` for a version probe: the executable's own directory first, then the usual
/// spawn path.
fn probe_path(exe: &str) -> Option<String> {
    let (base, sep) = codex_env::base_spawn_path();
    codex_env::path_for_exe(exe, base.as_deref(), sep)
}

/// One GET over the app's shared blocking client; `None` on any failure. Sends no user
/// data.
fn npm_fetch(url: &str) -> Option<String> {
    let client = crate::updater::source::http_client()
        .map_err(|e| log::warn!("[agent-updates] {e}"))
        .ok()?;
    let resp = client
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .header("Accept", "application/json")
        .send()
        .map_err(|e| log::info!("[agent-updates] registry fetch failed: {e}"))
        .ok()?;
    if !resp.status().is_success() {
        log::info!("[agent-updates] registry answered HTTP {}", resp.status());
        return None;
    }
    resp.text().ok()
}

impl Prober for MachineProber {
    fn config(&self) -> UserConfig {
        UserConfig::load().unwrap_or_default()
    }

    fn exe(&self, cli: CliKind, cfg: &UserConfig) -> Option<String> {
        let project = keitora_tasks::resolve_active().ok();
        keitora_tasks::resolve_provider_path(cli, project.as_ref(), cfg)
            .filter(|p| !p.trim().is_empty())
            .or_else(|| keitora_tasks::discover_cli_cached(cli))
    }

    fn resolve(&self, exe: &str) -> String {
        std::fs::canonicalize(exe)
            .map(|p| {
                keitora_tasks::strip_extended_prefix_str(&p.to_string_lossy()).replace('\\', "/")
            })
            .unwrap_or_else(|_| exe.to_string())
    }

    fn is_file(&self, path: &str) -> bool {
        std::path::Path::new(path).is_file()
    }

    fn installed(&self, cli: CliKind, exe: &str) -> Option<String> {
        let path = probe_path(exe);
        match cli {
            CliKind::Codex => codex_env::codex_version(exe, path.as_deref()),
            CliKind::Claude => codex_env::cached_version_with(
                CLAUDE_VERSION_CACHE.get_or_init(Default::default),
                exe,
                path.as_deref(),
                Instant::now(),
                &|exe, path| {
                    codex_env::probe_cli_version(exe, path, model::parse_claude_version_output)
                },
            ),
        }
    }

    fn invalidate(&self, cli: CliKind, exe: &str) {
        match cli {
            CliKind::Codex => codex_env::invalidate_codex_version(exe),
            CliKind::Claude => codex_env::invalidate_version(
                CLAUDE_VERSION_CACHE.get_or_init(Default::default),
                exe,
            ),
        }
    }

    fn latest(&self, cli: CliKind) -> Option<String> {
        let version_json = || {
            let env = std::env::var("CODEX_HOME").ok();
            let (home, _) = codex_env::codex_home_dir(env.as_deref(), dirs::home_dir().as_deref())?;
            std::fs::read_to_string(home.join("version.json")).ok()
        };
        latest_with(cli, version_json, chrono::Utc::now(), &npm_fetch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::cell::RefCell;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .with_ymd_and_hms(2026, 11, 18, 12, 0, 0)
            .unwrap()
    }

    const FRESH: &str = r#"{"latest_version":"0.163.0","last_checked_at":"2026-11-18T08:00:00Z"}"#;
    const STALE: &str = r#"{"latest_version":"0.150.0","last_checked_at":"2026-11-01T08:00:00Z"}"#;

    #[test]
    fn codex_latest_prefers_a_fresh_version_json() {
        let urls = RefCell::new(Vec::new());
        let fetch = |url: &str| {
            urls.borrow_mut().push(url.to_string());
            Some(r#"{"version":"0.164.0"}"#.to_string())
        };
        assert_eq!(
            latest_with(CliKind::Codex, || Some(FRESH.into()), now(), &fetch).as_deref(),
            Some("0.163.0")
        );
        assert!(urls.borrow().is_empty(), "no fetch when the cache is fresh");

        assert_eq!(
            latest_with(CliKind::Codex, || Some(STALE.into()), now(), &fetch).as_deref(),
            Some("0.164.0")
        );
        assert_eq!(
            latest_with(CliKind::Codex, || None, now(), &fetch).as_deref(),
            Some("0.164.0")
        );
        assert_eq!(*urls.borrow(), vec![CODEX_LATEST_URL, CODEX_LATEST_URL]);
    }

    #[test]
    fn claude_latest_reads_the_registry_only() {
        let urls = RefCell::new(Vec::new());
        let fetch = |url: &str| {
            urls.borrow_mut().push(url.to_string());
            Some(r#"{"version":"2.1.330"}"#.to_string())
        };
        let read = RefCell::new(false);
        assert_eq!(
            latest_with(
                CliKind::Claude,
                || {
                    *read.borrow_mut() = true;
                    Some(FRESH.into())
                },
                now(),
                &fetch
            )
            .as_deref(),
            Some("2.1.330")
        );
        assert!(!*read.borrow(), "Codex's cache is not Claude's");
        assert_eq!(*urls.borrow(), vec![CLAUDE_LATEST_URL]);
    }

    #[test]
    fn latest_is_none_when_offline_or_garbled() {
        assert_eq!(
            latest_with(CliKind::Claude, || None, now(), &|_| None),
            None
        );
        assert_eq!(
            latest_with(CliKind::Codex, || None, now(), &|_| Some("<html>".into())),
            None
        );
    }
}
