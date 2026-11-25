//! herdr's release manifest (`https://herdr.dev/latest.json`): defensive parsing, picking
//! the release to install, and the host allowlist every download must satisfy.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use super::floor::{HERDR_FLOOR, HERDR_PROTOCOL};

/// Where the release manifest lives.
pub const MANIFEST_URL: &str = "https://herdr.dev/latest.json";

/// Hosts a manifest or asset request may end up on after redirects.
pub const ALLOWED_HOSTS: &[&str] = &[
    "herdr.dev",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// The platforms herdr publishes assets for that Keitora supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    WindowsX86_64,
    MacosAarch64,
    MacosX86_64,
}

impl Platform {
    /// The platform this build runs on, or `None` for an unsupported OS/arch.
    pub fn current() -> Option<Platform> {
        Self::from_consts(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// Map `std::env::consts::{OS, ARCH}` values to a platform.
    pub fn from_consts(os: &str, arch: &str) -> Option<Platform> {
        match (os, arch) {
            ("windows", "x86_64") => Some(Platform::WindowsX86_64),
            ("macos", "aarch64") => Some(Platform::MacosAarch64),
            ("macos", "x86_64") => Some(Platform::MacosX86_64),
            _ => None,
        }
    }

    /// The manifest's asset key for this platform.
    pub fn key(self) -> &'static str {
        match self {
            Platform::WindowsX86_64 => "windows-x86_64",
            Platform::MacosAarch64 => "macos-aarch64",
            Platform::MacosX86_64 => "macos-x86_64",
        }
    }

    pub fn is_windows(self) -> bool {
        matches!(self, Platform::WindowsX86_64)
    }

    /// The herdr executable's file name on this platform.
    pub fn exe_name(self) -> &'static str {
        if self.is_windows() {
            "herdr.exe"
        } else {
            "herdr"
        }
    }
}

/// One installable herdr release for one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: semver::Version,
    pub protocol: u64,
    /// The `https://` asset URL for the platform.
    pub url: String,
    /// Lowercase 64-hex sha256 of the asset.
    pub sha256: String,
}

/// The manifest reduced to one platform: every valid release entry plus the top-level
/// "latest" version (what the vendor install script would install).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub platform: Platform,
    /// The top-level `version`, when it parses.
    pub latest: Option<semver::Version>,
    /// Valid releases for `platform`, newest first, one per version.
    pub releases: Vec<Release>,
}

impl Manifest {
    /// Whether `release` is the manifest's top-level latest version.
    pub fn is_latest(&self, release: &Release) -> bool {
        self.latest.as_ref() == Some(&release.version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// Not JSON, or not the expected shape at the top level.
    Malformed(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Malformed(why) => write!(f, "release manifest unreadable: {why}"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// Why a network request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetError {
    /// DNS or connect failure: no route to the host.
    Offline,
    /// TLS handshake or proxy failure.
    Tls(String),
    Timeout,
    /// A non-success HTTP status.
    Http(u16),
    /// The request (or a redirect) left https or the host allowlist.
    HostNotAllowed(String),
    /// Local I/O while writing a download.
    Io(String),
    Other(String),
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::Offline => f.write_str("offline"),
            NetError::Tls(m) => write!(f, "TLS or proxy error: {m}"),
            NetError::Timeout => f.write_str("timed out"),
            NetError::Http(code) => write!(f, "HTTP {code}"),
            NetError::HostNotAllowed(u) => write!(f, "refused to download from {u}"),
            NetError::Io(m) => write!(f, "write failed: {m}"),
            NetError::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for NetError {}

/// HTTP access, behind a trait so tests never touch the network.
pub trait Net: Send + Sync {
    /// GET `url` and return the body as text.
    fn get_text(&self, url: &str) -> Result<String, NetError>;
    /// GET `url` into `dest`, reporting `(bytes so far, total if known)`.
    fn download(
        &self,
        url: &str,
        dest: &Path,
        on_bytes: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), NetError>;
}

// Raw shapes. Every field is optional and unknown fields are ignored, so a manifest
// that grows new keys keeps parsing; each entry is validated on its own.
#[derive(Deserialize)]
struct RawManifest {
    version: Option<String>,
    protocol: Option<u64>,
    #[serde(default)]
    assets: BTreeMap<String, String>,
    #[serde(default)]
    sha256: BTreeMap<String, String>,
    /// Kept as raw values so one malformed entry drops only itself.
    #[serde(default)]
    releases: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct RawRelease {
    protocol: Option<u64>,
    #[serde(default)]
    assets: BTreeMap<String, String>,
    #[serde(default)]
    sha256: BTreeMap<String, String>,
}

/// A 64-hex sha256, lowercased; `None` for anything else.
fn normalize_sha256(s: &str) -> Option<String> {
    let s = s.trim();
    (s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

fn is_https(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| u.scheme() == "https" && u.host_str().is_some())
}

/// Build a release for `key` from one entry, or `None` if any part is missing or invalid.
fn release_from(
    version: &str,
    protocol: Option<u64>,
    assets: &BTreeMap<String, String>,
    sha256: &BTreeMap<String, String>,
    key: &str,
) -> Option<Release> {
    let version = semver::Version::parse(version.trim().trim_start_matches('v')).ok()?;
    let url = assets.get(key)?.trim();
    if !is_https(url) {
        return None;
    }
    Some(Release {
        version,
        protocol: protocol?,
        url: url.to_string(),
        sha256: normalize_sha256(sha256.get(key)?)?,
    })
}

/// Parse `json` for `platform`. Invalid entries (http asset, bad sha256, unparseable
/// version, missing protocol) are dropped individually; only an unreadable document is
/// an error. Never panics.
pub fn parse_manifest(json: &str, platform: Platform) -> Result<Manifest, ManifestError> {
    let raw: RawManifest =
        serde_json::from_str(json).map_err(|e| ManifestError::Malformed(e.to_string()))?;
    let key = platform.key();
    let mut releases: Vec<Release> = raw
        .releases
        .iter()
        .filter_map(|(ver, raw)| {
            let r = RawRelease::deserialize(raw).ok()?;
            release_from(ver, r.protocol, &r.assets, &r.sha256, key)
        })
        .collect();
    // The top-level block describes the latest release; count it even if the
    // `releases` map omits it.
    if let Some(ver) = raw.version.as_deref() {
        if let Some(top) = release_from(ver, raw.protocol, &raw.assets, &raw.sha256, key) {
            if !releases.iter().any(|r| r.version == top.version) {
                releases.push(top);
            }
        }
    }
    releases.sort_by(|a, b| b.version.cmp(&a.version));
    releases.dedup_by(|a, b| a.version == b.version);
    let latest = raw
        .version
        .as_deref()
        .and_then(|v| semver::Version::parse(v.trim().trim_start_matches('v')).ok());
    Ok(Manifest {
        platform,
        latest,
        releases,
    })
}

/// The newest stable release that speaks [`HERDR_PROTOCOL`], is at least
/// [`HERDR_FLOOR`] and at least `installed` (never a downgrade). `None` means no
/// compatible release exists for the platform.
pub fn select_release(m: &Manifest, installed: Option<&semver::Version>) -> Option<Release> {
    m.releases
        .iter()
        .filter(|r| r.protocol == HERDR_PROTOCOL)
        .filter(|r| r.version.pre.is_empty())
        .filter(|r| r.version >= HERDR_FLOOR)
        .filter(|r| installed.is_none_or(|i| r.version >= *i))
        .max_by(|a, b| a.version.cmp(&b.version))
        .cloned()
}

/// [`select_release`] for the herdr in use. The never-downgrade rule only holds while
/// that herdr speaks Keitora's protocol or an older one: a herdr speaking a newer
/// protocol (`newer_protocol`) is replaced by the newest compatible release even when
/// that release is older, installed as a separate Keitora-managed copy.
pub fn select_for(
    m: &Manifest,
    installed: Option<&semver::Version>,
    newer_protocol: bool,
) -> Option<Release> {
    select_release(m, if newer_protocol { None } else { installed })
}

/// `https` and a host on [`ALLOWED_HOSTS`]. Applied to the final URL after redirects.
pub fn host_allowed(url: &str) -> bool {
    host_allowed_in(url, ALLOWED_HOSTS)
}

/// [`host_allowed`] against an explicit allowlist (exact host match, default port).
pub fn host_allowed_in(url: &str, allowed: &[&str]) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    if u.scheme() != "https" || u.port().is_some_and(|p| p != 443) {
        return false;
    }
    match u.host_str() {
        Some(host) => allowed.iter().any(|a| a.eq_ignore_ascii_case(host)),
        None => false,
    }
}

/// Fail with [`NetError::HostNotAllowed`] unless `url` passes `allowed`.
pub fn check_url(url: &str, allowed: &dyn Fn(&str) -> bool) -> Result<(), NetError> {
    if allowed(url) {
        Ok(())
    } else {
        Err(NetError::HostNotAllowed(url.to_string()))
    }
}

/// How long a manifest fetch may take.
const TEXT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an asset download may take end to end.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Map a reqwest failure from its traits and its error-chain text. TLS and proxy
/// trouble shows up as a connect error whose chain names it.
pub fn classify_request_error(
    is_timeout: bool,
    is_connect: bool,
    status: Option<u16>,
    chain: &str,
) -> NetError {
    if is_timeout {
        return NetError::Timeout;
    }
    if let Some(code) = status {
        return NetError::Http(code);
    }
    let lower = chain.to_ascii_lowercase();
    if ["certificate", "tls", "handshake", "proxy"]
        .iter()
        .any(|k| lower.contains(k))
    {
        return NetError::Tls(chain.to_string());
    }
    if is_connect {
        return NetError::Offline;
    }
    NetError::Other(chain.to_string())
}

fn request_error(e: &reqwest::Error) -> NetError {
    let mut chain = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        chain.push_str(": ");
        chain.push_str(&s.to_string());
        src = s.source();
    }
    classify_request_error(
        e.is_timeout(),
        e.is_connect(),
        e.status().map(|s| s.as_u16()),
        &chain,
    )
}

/// The real [`Net`] over the shared blocking HTTP client. Call only from a blocking
/// thread. Both the requested URL and the final URL after redirects must pass the
/// allowlist; there is no plain-HTTP fallback.
pub struct HttpNet {
    allowed: Box<dyn Fn(&str) -> bool + Send + Sync>,
}

impl Default for HttpNet {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpNet {
    /// Restricted to [`host_allowed`].
    pub fn new() -> Self {
        HttpNet {
            allowed: Box::new(host_allowed),
        }
    }

    /// With a custom URL policy (the loopback end-to-end test).
    #[cfg(test)]
    pub fn with_policy(allowed: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        HttpNet {
            allowed: Box::new(allowed),
        }
    }

    fn get(&self, url: &str, timeout: Duration) -> Result<reqwest::blocking::Response, NetError> {
        check_url(url, &*self.allowed)?;
        let client =
            crate::updater::source::http_client().map_err(|e| NetError::Other(e.to_string()))?;
        let resp = client
            .get(url)
            .timeout(timeout)
            .send()
            .map_err(|e| request_error(&e))?;
        check_url(resp.url().as_str(), &*self.allowed)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(NetError::Http(status.as_u16()));
        }
        Ok(resp)
    }
}

impl Net for HttpNet {
    fn get_text(&self, url: &str) -> Result<String, NetError> {
        self.get(url, TEXT_TIMEOUT)?
            .text()
            .map_err(|e| request_error(&e))
    }

    fn download(
        &self,
        url: &str,
        dest: &Path,
        on_bytes: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), NetError> {
        let mut resp = self.get(url, DOWNLOAD_TIMEOUT)?;
        let total = resp.content_length();
        let mut file = std::fs::File::create(dest).map_err(|e| NetError::Io(e.to_string()))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut done = 0u64;
        on_bytes(0, total);
        loop {
            let n = match resp.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    return Err(NetError::Timeout)
                }
                Err(e) => return Err(NetError::Other(e.to_string())),
            };
            file.write_all(&buf[..n])
                .map_err(|e| NetError::Io(e.to_string()))?;
            done += n as u64;
            on_bytes(done, total);
        }
        file.flush().map_err(|e| NetError::Io(e.to_string()))?;
        Ok(())
    }
}

/// A scripted [`Net`] for tests.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Clone)]
    pub enum Reply {
        Body(Vec<u8>),
        /// Served after a redirect to this URL (checked against the allowlist).
        RedirectTo(String, Vec<u8>),
        Fail(NetError),
    }

    /// Answers by exact URL; unknown URLs are a 404. Applies [`host_allowed`] to the
    /// requested and final URLs, like the real client.
    #[derive(Default)]
    pub struct FakeNet {
        pub replies: Mutex<HashMap<String, Reply>>,
        pub requests: Mutex<Vec<String>>,
    }

    impl FakeNet {
        pub fn with(self, url: &str, reply: Reply) -> Self {
            self.replies.lock().unwrap().insert(url.to_string(), reply);
            self
        }

        pub fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }

        fn body(&self, url: &str) -> Result<Vec<u8>, NetError> {
            self.requests.lock().unwrap().push(url.to_string());
            check_url(url, &host_allowed)?;
            match self.replies.lock().unwrap().get(url).cloned() {
                None => Err(NetError::Http(404)),
                Some(Reply::Body(b)) => Ok(b),
                Some(Reply::RedirectTo(to, b)) => {
                    check_url(&to, &host_allowed)?;
                    Ok(b)
                }
                Some(Reply::Fail(e)) => Err(e),
            }
        }
    }

    impl Net for FakeNet {
        fn get_text(&self, url: &str) -> Result<String, NetError> {
            String::from_utf8(self.body(url)?).map_err(|e| NetError::Other(e.to_string()))
        }

        fn download(
            &self,
            url: &str,
            dest: &Path,
            on_bytes: &mut dyn FnMut(u64, Option<u64>),
        ) -> Result<(), NetError> {
            let body = self.body(url)?;
            let total = body.len() as u64;
            on_bytes(0, Some(total));
            std::fs::write(dest, &body).map_err(|e| NetError::Io(e.to_string()))?;
            on_bytes(total, Some(total));
            Ok(())
        }
    }
}

#[cfg(test)]
pub(crate) const FIXTURE: &str = include_str!("fixtures/latest.json");

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> semver::Version {
        semver::Version::parse(s).unwrap()
    }

    fn fixture(p: Platform) -> Manifest {
        parse_manifest(FIXTURE, p).unwrap()
    }

    #[test]
    fn platform_keys_and_consts() {
        assert_eq!(Platform::WindowsX86_64.key(), "windows-x86_64");
        assert_eq!(Platform::MacosAarch64.key(), "macos-aarch64");
        assert_eq!(Platform::MacosX86_64.key(), "macos-x86_64");
        assert_eq!(
            Platform::from_consts("windows", "x86_64"),
            Some(Platform::WindowsX86_64)
        );
        assert_eq!(
            Platform::from_consts("macos", "aarch64"),
            Some(Platform::MacosAarch64)
        );
        assert_eq!(Platform::from_consts("linux", "x86_64"), None);
        assert_eq!(Platform::from_consts("windows", "aarch64"), None);
        assert_eq!(Platform::WindowsX86_64.exe_name(), "herdr.exe");
        assert_eq!(Platform::MacosX86_64.exe_name(), "herdr");
    }

    #[test]
    fn fixture_parses_with_top_level_latest() {
        let m = fixture(Platform::WindowsX86_64);
        assert_eq!(m.latest, Some(v("0.10.0")));
        let versions: Vec<String> = m.releases.iter().map(|r| r.version.to_string()).collect();
        // 0.9.7 has no windows asset.
        assert_eq!(versions, ["0.10.0", "0.9.6", "0.9.5", "0.9.4", "0.9.3"]);
    }

    #[test]
    fn selection_skips_a_protocol_23_latest() {
        let m = fixture(Platform::MacosAarch64);
        let r = select_release(&m, None).unwrap();
        assert_eq!(r.version, v("0.9.7"));
        assert_eq!(r.protocol, 22);
        assert!(r
            .url
            .starts_with("https://github.com/herdrdev/herdr/releases/download/v0.9.7/"));
        assert!(!m.is_latest(&r));
    }

    #[test]
    fn selection_skips_a_release_missing_the_platform_asset() {
        let r = select_release(&fixture(Platform::WindowsX86_64), None).unwrap();
        assert_eq!(r.version, v("0.9.6"));
        assert!(r.url.ends_with(".zip"));
    }

    #[test]
    fn selection_never_downgrades() {
        let m = fixture(Platform::WindowsX86_64);
        assert_eq!(
            select_release(&m, Some(&v("0.9.5"))).unwrap().version,
            v("0.9.6")
        );
        assert_eq!(
            select_release(&m, Some(&v("0.9.6"))).unwrap().version,
            v("0.9.6")
        );
        assert_eq!(select_release(&m, Some(&v("0.9.8"))), None);
        // Installed below the floor still gets the newest compatible release.
        assert_eq!(
            select_release(&m, Some(&v("0.9.3"))).unwrap().version,
            v("0.9.6")
        );
    }

    #[test]
    fn a_newer_protocol_herdr_may_be_replaced_by_an_older_compatible_release() {
        // macOS latest is 0.10.0 at protocol 23; the newest protocol-22 release is 0.9.7.
        let m = fixture(Platform::MacosAarch64);
        assert_eq!(
            select_for(&m, Some(&v("0.10.0")), false),
            None,
            "no downgrade"
        );
        let r = select_for(&m, Some(&v("0.10.0")), true).unwrap();
        assert_eq!(r.version, v("0.9.7"));
        assert_eq!(r.protocol, 22);
        // Otherwise it is plain selection.
        assert_eq!(
            select_for(&m, Some(&v("0.9.5")), false),
            select_release(&m, Some(&v("0.9.5")))
        );
    }

    #[test]
    fn selected_equal_to_latest_is_reported() {
        let json = r#"{
            "version": "0.9.6", "protocol": 22,
            "releases": { "0.9.6": { "protocol": 22,
                "assets": {"windows-x86_64": "https://github.com/herdrdev/herdr/releases/download/v0.9.6/h.zip"},
                "sha256": {"windows-x86_64": "1111111111111111111111111111111111111111111111111111111111111111"} } }
        }"#;
        let m = parse_manifest(json, Platform::WindowsX86_64).unwrap();
        let r = select_release(&m, None).unwrap();
        assert!(m.is_latest(&r));
    }

    #[test]
    fn top_level_block_counts_when_releases_map_omits_it() {
        let json = r#"{
            "version": "0.9.6", "protocol": 22,
            "assets": {"macos-x86_64": "https://github.com/herdrdev/herdr/releases/download/v0.9.6/h"},
            "sha256": {"macos-x86_64": "2222222222222222222222222222222222222222222222222222222222222222"}
        }"#;
        let m = parse_manifest(json, Platform::MacosX86_64).unwrap();
        assert_eq!(select_release(&m, None).unwrap().version, v("0.9.6"));
    }

    #[test]
    fn no_compatible_release_for_platform() {
        let json = r#"{"version": "0.9.6", "releases": {}}"#;
        let m = parse_manifest(json, Platform::MacosAarch64).unwrap();
        assert_eq!(select_release(&m, None), None);
    }

    #[test]
    fn uppercase_sha_is_normalised() {
        let r = select_release(&fixture(Platform::WindowsX86_64), None).unwrap();
        assert_eq!(
            r.sha256,
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
    }

    fn one_release(url: &str, sha: &str) -> String {
        format!(
            r#"{{"releases": {{"0.9.6": {{"protocol": 22, "extra": [1, 2],
                "assets": {{"windows-x86_64": "{url}"}},
                "sha256": {{"windows-x86_64": "{sha}"}} }} }} }}"#
        )
    }

    const GOOD_URL: &str = "https://github.com/herdrdev/herdr/releases/download/v0.9.6/h.zip";
    const GOOD_SHA: &str = "3333333333333333333333333333333333333333333333333333333333333333";

    #[test]
    fn a_malformed_entry_drops_only_itself() {
        let json = format!(
            r#"{{"releases": {{"0.9.7": "junk", "0.9.8": {{"protocol": "x"}},
                "0.9.6": {{"protocol": 22, "assets": {{"windows-x86_64": "{GOOD_URL}"}},
                "sha256": {{"windows-x86_64": "{GOOD_SHA}"}} }} }} }}"#
        );
        let m = parse_manifest(&json, Platform::WindowsX86_64).unwrap();
        assert_eq!(m.releases.len(), 1);
        assert_eq!(m.releases[0].version, v("0.9.6"));
    }

    #[test]
    fn valid_entry_with_unknown_fields_is_kept() {
        let m = parse_manifest(&one_release(GOOD_URL, GOOD_SHA), Platform::WindowsX86_64).unwrap();
        assert_eq!(m.releases.len(), 1);
        assert_eq!(m.latest, None);
    }

    #[test]
    fn http_asset_url_is_rejected() {
        let url = "http://github.com/herdrdev/herdr/releases/download/v0.9.6/h.zip";
        let m = parse_manifest(&one_release(url, GOOD_SHA), Platform::WindowsX86_64).unwrap();
        assert!(m.releases.is_empty());
    }

    #[test]
    fn short_or_non_hex_sha_is_rejected() {
        for sha in [
            "abc123",
            "zz33333333333333333333333333333333333333333333333333333333333333",
            "33333333333333333333333333333333333333333333333333333333333333333",
        ] {
            let m = parse_manifest(&one_release(GOOD_URL, sha), Platform::WindowsX86_64).unwrap();
            assert!(m.releases.is_empty(), "{sha}");
        }
    }

    #[test]
    fn malformed_json_is_an_error() {
        for json in ["", "not json", "{", "[1,2,3]", r#"{"releases": 5}"#] {
            assert!(
                parse_manifest(json, Platform::WindowsX86_64).is_err(),
                "{json:?}"
            );
        }
    }

    #[test]
    fn prerelease_entries_are_never_selected() {
        let json = r#"{"releases": {"0.9.9-rc.1": {"protocol": 22,
            "assets": {"windows-x86_64": "https://github.com/herdrdev/herdr/releases/download/v0.9.9-rc.1/h.zip"},
            "sha256": {"windows-x86_64": "4444444444444444444444444444444444444444444444444444444444444444"} } } }"#;
        let m = parse_manifest(json, Platform::WindowsX86_64).unwrap();
        assert_eq!(m.releases.len(), 1);
        assert_eq!(select_release(&m, None), None);
    }

    #[test]
    fn host_allowlist() {
        for url in [
            "https://herdr.dev/latest.json",
            "https://github.com/herdrdev/herdr/releases/download/v0.9.6/h.zip",
            "https://objects.githubusercontent.com/x/y",
            "https://release-assets.githubusercontent.com/x/y?sig=1",
            "https://HERDR.dev/latest.json",
            "https://github.com:443/a",
        ] {
            assert!(host_allowed(url), "{url}");
        }
        for url in [
            "http://herdr.dev/latest.json",
            "https://example.com/latest.json",
            "https://herdr.dev.example.com/latest.json",
            "https://notherdr.dev/x",
            "https://github.com.example.com/x",
            "https://evil-github.com/x",
            "https://herdr.dev@example.com/x",
            "https://herdr.dev:8443/latest.json",
            "ftp://herdr.dev/x",
            "not a url",
            "",
        ] {
            assert!(!host_allowed(url), "{url}");
        }
    }

    #[test]
    fn redirect_to_a_non_allowlisted_host_is_rejected() {
        use fake::{FakeNet, Reply};
        let asset = "https://github.com/herdrdev/herdr/releases/download/v0.9.6/h.zip";
        let net = FakeNet::default()
            .with(
                asset,
                Reply::RedirectTo("https://example.com/h.zip".into(), b"zip".to_vec()),
            )
            .with(
                MANIFEST_URL,
                Reply::RedirectTo(
                    "https://release-assets.githubusercontent.com/latest.json".into(),
                    b"{}".to_vec(),
                ),
            );
        let tmp = tempfile::tempdir().unwrap();
        let err = net
            .download(asset, &tmp.path().join("a"), &mut |_, _| {})
            .unwrap_err();
        assert_eq!(
            err,
            NetError::HostNotAllowed("https://example.com/h.zip".into())
        );
        assert!(!tmp.path().join("a").exists());
        // A redirect that stays on the allowlist is fine.
        assert_eq!(net.get_text(MANIFEST_URL).unwrap(), "{}");
        // A plain-HTTP request is refused before it is sent anywhere.
        assert!(matches!(
            net.get_text("http://herdr.dev/latest.json"),
            Err(NetError::HostNotAllowed(_))
        ));
    }

    #[test]
    fn check_url_uses_the_policy() {
        assert!(check_url("https://herdr.dev/latest.json", &host_allowed).is_ok());
        assert_eq!(
            check_url("https://example.com/x", &host_allowed),
            Err(NetError::HostNotAllowed("https://example.com/x".into()))
        );
    }

    #[test]
    fn request_errors_classify() {
        assert_eq!(
            classify_request_error(true, true, None, "operation timed out"),
            NetError::Timeout
        );
        assert_eq!(
            classify_request_error(false, false, Some(404), "not found"),
            NetError::Http(404)
        );
        assert!(matches!(
            classify_request_error(
                false,
                true,
                None,
                "error: invalid peer certificate: UnknownIssuer"
            ),
            NetError::Tls(_)
        ));
        assert!(matches!(
            classify_request_error(false, true, None, "proxy connect failed"),
            NetError::Tls(_)
        ));
        assert_eq!(
            classify_request_error(false, true, None, "dns error: no such host"),
            NetError::Offline
        );
        assert!(matches!(
            classify_request_error(false, false, None, "connection reset"),
            NetError::Other(_)
        ));
    }

    #[test]
    fn host_allowed_in_uses_the_given_list() {
        assert!(host_allowed_in("https://example.com/a", &["example.com"]));
        assert!(!host_allowed_in("https://herdr.dev/a", &["example.com"]));
    }
}
