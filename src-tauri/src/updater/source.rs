//! Release discovery over HTTPS: the unauthenticated GitHub "latest release" API plus the
//! small `.sig` asset. Synchronous on purpose — callers run it on the schedule thread or in
//! `spawn_blocking`.

use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

const DEFAULT_OWNER: &str = "rudoi";
const DEFAULT_REPO: &str = "keitora";
/// `owner/repo` override, honoured in every build. Safe: every download must still verify
/// against the embedded public key.
const REPO_ENV: &str = "KEITORA_UPDATE_REPO";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// Connect, DNS or timeout failure.
    Offline,
    /// 403/429 from the API's unauthenticated rate limit.
    RateLimited,
    Http(u16),
    /// The response body could not be read.
    Body,
    /// The HTTP client could not be built.
    Client,
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::Offline => f.write_str("offline"),
            SourceError::RateLimited => f.write_str("rate limited by GitHub, try again later"),
            SourceError::Http(code) => write!(f, "HTTP {code}"),
            SourceError::Body => f.write_str("unreadable response"),
            SourceError::Client => f.write_str("HTTP client unavailable"),
        }
    }
}

impl std::error::Error for SourceError {}

pub trait ReleaseSource: Send + Sync {
    /// Raw JSON of the latest stable release, or `None` when the repo has none yet (404).
    fn latest(&self) -> Result<Option<String>, SourceError>;
    /// Fetch a small text asset (the `.sig` next to an installer).
    fn fetch_text(&self, url: &str) -> Result<String, SourceError>;
}

/// Install `ring` as the process-wide rustls provider. reqwest is built with
/// `rustls-no-provider`, so building a client panics until one is installed. An `Err` means
/// one already is, which is fine.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Map an HTTP status (and the `x-ratelimit-remaining` header) to success or an error kind.
pub fn classify(status: u16, ratelimit_remaining: Option<&str>) -> Result<(), SourceError> {
    match status {
        200..=299 => Ok(()),
        429 => Err(SourceError::RateLimited),
        403 if ratelimit_remaining.map(str::trim) == Some("0") => Err(SourceError::RateLimited),
        code => Err(SourceError::Http(code)),
    }
}

/// Parse `owner/repo`: exactly two non-empty segments.
pub fn parse_repo(s: &str) -> Option<(String, String)> {
    let (owner, repo) = s.trim().split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// The real source: GitHub's REST API over a single reused blocking client.
pub struct GithubSource {
    user_agent: String,
    latest_url: String,
    /// Built on first use, on a blocking thread (never on the setup thread or inside an
    /// async task: the blocking client owns a runtime), then reused.
    client: OnceLock<Result<reqwest::blocking::Client, SourceError>>,
}

impl GithubSource {
    pub fn new(app_version: &str) -> Self {
        let (owner, repo) = Self::repo();
        GithubSource {
            user_agent: format!("Keitora/{app_version}"),
            latest_url: format!("https://api.github.com/repos/{owner}/{repo}/releases/latest"),
            client: OnceLock::new(),
        }
    }

    /// The shared client. Requires [`install_crypto_provider`] to have run.
    fn client(&self) -> Result<&reqwest::blocking::Client, SourceError> {
        self.client
            .get_or_init(|| {
                reqwest::blocking::Client::builder()
                    .user_agent(self.user_agent.as_str())
                    .timeout(REQUEST_TIMEOUT)
                    .build()
                    .map_err(|e| {
                        log::error!("[updater] http client build failed: {e}");
                        SourceError::Client
                    })
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// The repo to read releases from: `KEITORA_UPDATE_REPO` when valid, else the default.
    pub fn repo() -> (String, String) {
        if let Ok(raw) = std::env::var(REPO_ENV) {
            match parse_repo(&raw) {
                Some(pair) => return pair,
                None => log::warn!("[updater] ignoring malformed {REPO_ENV}={raw:?}"),
            }
        }
        (DEFAULT_OWNER.to_string(), DEFAULT_REPO.to_string())
    }
}

fn send_error(e: reqwest::Error) -> SourceError {
    match e.status() {
        Some(status) => SourceError::Http(status.as_u16()),
        None => SourceError::Offline,
    }
}

fn read_text(resp: reqwest::blocking::Response) -> Result<String, SourceError> {
    resp.text().map_err(|e| {
        if e.is_timeout() {
            SourceError::Offline
        } else {
            SourceError::Body
        }
    })
}

fn check_status(resp: &reqwest::blocking::Response) -> Result<(), SourceError> {
    let remaining = resp
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok());
    classify(resp.status().as_u16(), remaining)
}

impl ReleaseSource for GithubSource {
    fn latest(&self) -> Result<Option<String>, SourceError> {
        let resp = self
            .client()?
            .get(&self.latest_url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .map_err(send_error)?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        check_status(&resp)?;
        read_text(resp).map(Some)
    }

    fn fetch_text(&self, url: &str) -> Result<String, SourceError> {
        let resp = self.client()?.get(url).send().map_err(send_error)?;
        check_status(&resp)?;
        read_text(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_success_and_not_found() {
        assert_eq!(classify(200, None), Ok(()));
        assert_eq!(classify(204, Some("12")), Ok(()));
        assert_eq!(classify(404, None), Err(SourceError::Http(404)));
    }

    #[test]
    fn classify_rate_limits() {
        assert_eq!(classify(403, Some("0")), Err(SourceError::RateLimited));
        assert_eq!(classify(403, Some("12")), Err(SourceError::Http(403)));
        assert_eq!(classify(403, None), Err(SourceError::Http(403)));
        assert_eq!(classify(429, None), Err(SourceError::RateLimited));
        assert_eq!(classify(429, Some("0")), Err(SourceError::RateLimited));
    }

    #[test]
    fn classify_server_errors() {
        assert_eq!(classify(500, None), Err(SourceError::Http(500)));
        assert_eq!(classify(502, Some("0")), Err(SourceError::Http(502)));
    }

    #[test]
    fn parse_repo_accepts_owner_and_repo() {
        assert_eq!(
            parse_repo("me/my-game"),
            Some(("me".to_string(), "my-game".to_string()))
        );
        assert_eq!(
            parse_repo(" me/proj \n"),
            Some(("me".to_string(), "proj".to_string()))
        );
    }

    #[test]
    fn parse_repo_rejects_malformed() {
        for bad in ["", "a", "a/b/c", "a/", "/b", "/"] {
            assert_eq!(parse_repo(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn errors_display_as_short_kinds() {
        assert_eq!(SourceError::Offline.to_string(), "offline");
        assert_eq!(SourceError::Http(500).to_string(), "HTTP 500");
    }
}
