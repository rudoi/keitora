//! Machine reads for agent CLI updates: where each CLI is, its installed version, and
//! the latest release (Codex's own `version.json` when fresh, else the npm registry;
//! Antigravity's release manifest).
//! [`Prober`] is the seam the update state is built on, so tests never run a real CLI
//! or touch the network.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use keitora_tasks::{CliKind, UserConfig};

use super::model;
use crate::codex_env::{self, VersionCache};

const CODEX_LATEST_URL: &str = "https://registry.npmjs.org/@openai/codex/latest";
const CLAUDE_LATEST_URL: &str = "https://registry.npmjs.org/@anthropic-ai/claude-code/latest";
/// Antigravity's release manifests, one `<platform>.json` per platform.
const AGY_MANIFEST_BASE: &str =
    "https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests";
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

/// The release manifest platform key for `std::env::consts::{OS, ARCH}` values; `None`
/// for anything unsupported. musl is not told apart (its manifest has the same version).
pub fn manifest_platform_key(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("windows", "x86_64") => "windows_amd64",
        ("windows", "aarch64") => "windows_arm64",
        ("macos", "x86_64") => "darwin_amd64",
        ("macos", "aarch64") => "darwin_arm64",
        ("linux", "x86_64") => "linux_amd64",
        ("linux", "aarch64") => "linux_arm64",
        _ => return None,
    })
}

/// `<AGY_MANIFEST_BASE>/<platform>.json` for this machine.
pub fn manifest_url() -> Option<String> {
    manifest_platform_key(std::env::consts::OS, std::env::consts::ARCH)
        .map(|key| format!("{AGY_MANIFEST_BASE}/{key}.json"))
}

/// The latest version of `cli`: for Codex, its `version.json` cache when that is less
/// than a day old; for Antigravity, its release manifest; otherwise the npm registry,
/// each through `fetch` (URL to body).
pub fn latest_with(
    cli: CliKind,
    version_json: impl FnOnce() -> Option<String>,
    now: chrono::DateTime<chrono::Utc>,
    fetch: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let url: &str = match cli {
        CliKind::Antigravity => {
            return manifest_url()
                .and_then(|u| fetch(&u))
                .and_then(|body| model::parse_manifest_latest(&body));
        }
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

/// The `agy --version` results, shared so an update can invalidate them.
static AGY_VERSION_CACHE: OnceLock<VersionCache> = OnceLock::new();

/// `PATH` for a version probe: the executable's own directory first, then the usual
/// spawn path.
fn probe_path(exe: &str) -> Option<String> {
    let (base, sep) = codex_env::base_spawn_path();
    codex_env::path_for_exe(exe, base.as_deref(), sep)
}

/// One GET over the app's shared blocking client (the npm registry or Antigravity's
/// release manifest); `None` on any failure. Sends no user data.
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
                &|exe, path| codex_env::probe_cli_version(exe, path, model::parse_first_semver),
            ),
            CliKind::Antigravity => codex_env::cached_version_with(
                AGY_VERSION_CACHE.get_or_init(Default::default),
                exe,
                path.as_deref(),
                Instant::now(),
                &|exe, path| codex_env::probe_cli_version(exe, path, model::parse_first_semver),
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
            CliKind::Antigravity => {
                codex_env::invalidate_version(AGY_VERSION_CACHE.get_or_init(Default::default), exe)
            }
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

    #[test]
    fn manifest_platform_key_maps_supported_targets() {
        let cases = [
            ("windows", "x86_64", Some("windows_amd64")),
            ("windows", "aarch64", Some("windows_arm64")),
            ("macos", "x86_64", Some("darwin_amd64")),
            ("macos", "aarch64", Some("darwin_arm64")),
            ("linux", "x86_64", Some("linux_amd64")),
            ("linux", "aarch64", Some("linux_arm64")),
            ("windows", "x86", None),
            ("freebsd", "x86_64", None),
        ];
        for (os, arch, want) in cases {
            assert_eq!(manifest_platform_key(os, arch), want, "{os} {arch}");
        }
    }

    #[test]
    fn agy_latest_reads_the_manifest_only() {
        let urls = RefCell::new(Vec::new());
        let fetch = |url: &str| {
            urls.borrow_mut().push(url.to_string());
            Some(r#"{"version":"1.5.18","url":"u","sha512":"s"}"#.to_string())
        };
        let read = RefCell::new(false);
        let latest = latest_with(
            CliKind::Antigravity,
            || {
                *read.borrow_mut() = true;
                Some(FRESH.into())
            },
            now(),
            &fetch,
        );
        assert!(!*read.borrow(), "Codex's cache is not Antigravity's");
        if manifest_url().is_none() {
            // A target with no manifest learns no latest version and fetches nothing.
            assert_eq!(latest, None);
            assert!(urls.borrow().is_empty());
            return;
        }
        assert_eq!(latest.as_deref(), Some("1.5.18"));
        let urls = urls.borrow();
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with(AGY_MANIFEST_BASE), "{}", urls[0]);
        assert!(urls[0].ends_with(".json"), "{}", urls[0]);
    }

    #[test]
    fn agy_latest_is_none_when_the_fetch_fails() {
        assert_eq!(
            latest_with(CliKind::Antigravity, || None, now(), &|_| None),
            None
        );
    }
}
