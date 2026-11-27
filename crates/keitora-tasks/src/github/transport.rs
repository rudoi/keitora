//! The injectable `gh`-CLI boundary and the raw-output → [`BackendError`] classifier.
//!
//! `GitHubBackend` (a later stage) talks to GitHub **only** through the
//! [`GhTransport`] trait, never by spawning `gh` directly. That makes the network
//! backend unit-testable: a fake transport returns canned JSON, and the single
//! place that interprets raw `gh` output — [`classify`] — is a pure function with
//! an exhaustive test table.

use async_trait::async_trait;
use std::time::Duration;

use crate::backend::{BackendError, BackendResult};
use crate::config::CommandNoWindow;

/// A single `gh` invocation.
#[derive(Debug, Clone)]
pub(crate) struct GhRequest {
    /// Arguments passed to `gh`, e.g.
    /// `["api","graphql","-f","query=...","-f","owner=..."]`.
    pub args: Vec<String>,
    /// Optional stdin payload (used for large GraphQL bodies).
    pub stdin: Option<Vec<u8>>,
    /// `true` => this call mutates server state, so partial GraphQL errors
    /// (`200 + errors[]`) are **not** blind-retryable: the write may have
    /// applied. See [`classify`].
    pub mutation: bool,
}

/// The injectable boundary between the GitHub backend and the `gh` CLI.
#[async_trait]
pub(crate) trait GhTransport: Send + Sync {
    /// Run a `gh` invocation and return raw stdout (usually JSON) on success,
    /// or a classified [`BackendError`] on failure.
    async fn run(&self, req: GhRequest) -> BackendResult<Vec<u8>>;
}

// --------------------------------------------------------------------------
// Real implementation
// --------------------------------------------------------------------------

/// The remediation string attached to scope/auth failures.
const SCOPE_REMEDIATION: &str = "run `gh auth refresh -s project`";

/// A `gh` [`tokio::process::Command`] with `.no_window()` applied and an augmented
/// PATH so the backend finds `gh` whether Keitora is launched from a terminal (full
/// PATH) or the Dock (minimal PATH without Homebrew). Without the PATH fix, GUI
/// launches report `gh` "not on PATH".
///
/// The PATH augmentation is unix-only: the extra dirs and the `:` PATH separator
/// are POSIX-specific, and blindly re-joining a Windows `;`-separated PATH (with
/// `C:\…` entries) on `:` would corrupt it. On non-unix we inherit the
/// already-correct PATH unchanged.
#[cfg(unix)]
fn gh_command() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("gh");
    // `var_os` + lossy so a non-UTF-8 PATH still augments (rather than dropping the
    // inherited PATH, which would strip `gh`'s own `git` subprocess resolution).
    let inherited = std::env::var_os("PATH");
    let inherited = inherited.as_ref().map(|s| s.to_string_lossy());
    cmd.env(
        "PATH",
        crate::config::prepend_path_dirs(&crate::config::extra_path_dirs(), inherited.as_deref()),
    );
    // After the PATH, so the AppImage cleanup sees (and cleans) it.
    cmd.no_window();
    cmd
}

/// Non-unix: inherit the platform's (already-correct) PATH; see the unix variant.
/// `.no_window()` suppresses the console window Windows would otherwise flash for
/// every `gh` spawn from the GUI.
#[cfg(not(unix))]
fn gh_command() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("gh");
    cmd.no_window();
    cmd
}

/// Real [`GhTransport`] over `tokio::process::Command` (program `gh`).
pub(crate) struct GhCli {
    /// Set once the one-time preflight (binary present + authenticated) has passed,
    /// so subsequent calls skip it.
    preflighted: std::sync::atomic::AtomicBool,
}

impl GhCli {
    /// Construct a `GhCli`. The `gh` presence / auth preflight runs lazily on the
    /// first [`GhTransport::run`] (so construction stays cheap & infallible).
    pub(crate) fn new() -> Self {
        Self {
            preflighted: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Verify `gh` is on PATH and authenticated. Runs at most once per instance.
    ///
    /// Missing binary (spawn `ENOENT`) or "not logged in" → [`BackendError::Unavailable`]
    /// with the remedy text. This is a *runtime prerequisite* failure, distinct from
    /// `Config` (bad toml) and `Unauthorized` (creds present but rejected).
    async fn preflight(&self) -> BackendResult<()> {
        use std::sync::atomic::Ordering;
        if self.preflighted.load(Ordering::Acquire) {
            return Ok(());
        }

        let out = gh_command().args(["auth", "status"]).output().await;

        match out {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BackendError::Unavailable(
                    "the `gh` CLI is not installed or not on PATH — install it from \
                     https://cli.github.com and run `gh auth login`"
                        .to_string(),
                ));
            }
            Err(e) => {
                // Some other spawn failure (permissions, etc.). Treat as a runtime
                // prerequisite problem rather than masquerading as a network error.
                return Err(BackendError::Unavailable(format!(
                    "failed to run `gh auth status`: {e}"
                )));
            }
            Ok(o) if !o.status.success() => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                return Err(BackendError::Unavailable(format!(
                    "`gh` is not authenticated — run `gh auth login`{}",
                    if stderr.trim().is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", stderr.trim())
                    }
                )));
            }
            Ok(_) => {}
        }

        self.preflighted.store(true, Ordering::Release);
        Ok(())
    }
}

#[async_trait]
impl GhTransport for GhCli {
    async fn run(&self, req: GhRequest) -> BackendResult<Vec<u8>> {
        self.preflight().await?;

        let mut cmd = gh_command();
        cmd.args(&req.args);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        if req.stdin.is_some() {
            cmd.stdin(std::process::Stdio::piped());
        } else {
            cmd.stdin(std::process::Stdio::null());
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // `gh` vanished between preflight and now (unlikely but defensive).
                return Err(BackendError::Unavailable(
                    "the `gh` CLI is not installed or not on PATH — install it from \
                     https://cli.github.com and run `gh auth login`"
                        .to_string(),
                ));
            }
            Err(e) => {
                return Err(BackendError::Transient(format!(
                    "failed to spawn `gh`: {e}"
                )))
            }
        };

        if let Some(bytes) = req.stdin {
            if let Some(mut sink) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                // Best-effort: a broken pipe here surfaces as a process error below.
                let _ = sink.write_all(&bytes).await;
                let _ = sink.shutdown().await;
            }
        }

        let output = match child.wait_with_output().await {
            Ok(o) => o,
            Err(e) => {
                return Err(BackendError::Transient(format!(
                    "failed to await `gh`: {e}"
                )))
            }
        };

        if output.status.success() {
            return Ok(output.stdout);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(classify(
            output.status.code(),
            &output.stdout,
            &stderr,
            req.mutation,
        ))
    }
}

// --------------------------------------------------------------------------
// Classifier (the contract-critical table)
// --------------------------------------------------------------------------

/// Map raw `gh` signals → [`BackendError`].
///
/// Pure & deterministic so it can be unit-tested in isolation. `gh`'s exact
/// output format is not a stable contract, so this is **best-effort string/JSON
/// sniffing**; the assumptions are documented inline and flagged for live
/// verification in the live-testing stage.
///
/// The invariant that prevents data corruption: **a mutation whose write may have
/// applied is never returned as a blind-retryable error.** Concretely, GraphQL
/// `200 + errors[]` on a mutation → [`BackendError::Permanent`] (the backend then
/// does write-then-verify), but the same on a read → [`BackendError::Transient`]
/// (no side effect; safe to retry).
///
/// `exit_code` is `None` when the process was killed by a signal.
pub(crate) fn classify(
    exit_code: Option<i32>,
    stdout: &[u8],
    stderr: &str,
    mutation: bool,
) -> BackendError {
    let stderr_l = stderr.to_lowercase();

    // --- Runtime prerequisite: gh missing / not authenticated ------------
    // (Normally caught at preflight; handled here too for transports that
    // skip preflight or for surprising mid-run states.)
    if stderr_l.contains("command not found")
        || stderr_l.contains("executable file not found")
        || stderr_l.contains("no such file or directory")
    {
        return BackendError::Unavailable(
            "the `gh` CLI is not installed or not on PATH — install it from \
             https://cli.github.com and run `gh auth login`"
                .to_string(),
        );
    }
    if stderr_l.contains("not logged into any github hosts")
        || stderr_l.contains("gh auth login")
        || stderr_l.contains("authentication required")
    {
        return BackendError::Unavailable(
            "`gh` is not authenticated — run `gh auth login`".to_string(),
        );
    }

    // --- Auth: present but insufficient scope / rejected -----------------
    // GraphQL: errors[].type == INSUFFICIENT_SCOPES; REST: 401/403 scope hints.
    // Assumption: `gh` echoes the missing-scope error to stderr (and/or JSON).
    let stdout_str = String::from_utf8_lossy(stdout);
    let json = serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()
        .or_else(|| serde_json::from_str::<serde_json::Value>(stdout_str.trim()).ok());

    if stderr_l.contains("insufficient_scopes")
        || stdout_str.to_lowercase().contains("insufficient_scopes")
        || stderr_l.contains("insufficient scope")
        || stderr_l.contains("missing required scopes")
        || stderr_l.contains("requires the following scopes")
        || ((http_status(stderr) == Some(401) || http_status(stderr) == Some(403))
            && (stderr_l.contains("scope") || stderr_l.contains("token")))
        || stderr_l.contains("bad credentials")
    {
        return BackendError::Unauthorized {
            message: format!(
                "GitHub rejected the request for insufficient authorization: {}",
                first_line(stderr)
            ),
            remediation: Some(SCOPE_REMEDIATION.to_string()),
        };
    }

    // --- Rate limiting ----------------------------------------------------
    // Secondary limit: HTTP 429 / "secondary rate limit" + `Retry-After: N`.
    // Primary limit: `X-RateLimit-Remaining: 0` + a reset time.
    let is_429 = http_status(stderr) == Some(429);
    let is_secondary = stderr_l.contains("secondary rate limit");
    let is_primary = stderr_l.contains("api rate limit exceeded")
        || (header_value(stderr, "x-ratelimit-remaining").as_deref() == Some("0"));
    if is_429 || is_secondary || is_primary {
        let retry_after = parse_retry_after(stderr).or_else(|| parse_ratelimit_reset(stderr));
        return BackendError::RateLimited { retry_after };
    }

    // --- HTTP status mapping ---------------------------------------------
    if let Some(status) = http_status(stderr) {
        match status {
            // 5xx → transient server error (retry).
            500..=599 => {
                return BackendError::Transient(format!("HTTP {status}: {}", first_line(stderr)))
            }
            // 404 / could-not-resolve → not found.
            404 => return not_found(stderr),
            // 409 issue-body edit conflict → transient (re-read + retry).
            409 => {
                return BackendError::Transient(format!(
                    "HTTP 409 conflict: {}",
                    first_line(stderr)
                ))
            }
            // 422/400 validation → permanent.
            422 | 400 => {
                return BackendError::Permanent(format!("HTTP {status}: {}", first_line(stderr)))
            }
            _ => {}
        }
    }

    // "Could not resolve to a Repository/Issue/..." (GraphQL NOT_FOUND without an
    // explicit HTTP status line in stderr).
    if stderr_l.contains("could not resolve to")
        || stdout_str.to_lowercase().contains("could not resolve to")
        || stderr_l.contains("\"type\":\"not_found\"")
        || stdout_str
            .to_lowercase()
            .contains("\"type\": \"not_found\"")
        || stdout_str.to_lowercase().contains("\"type\":\"not_found\"")
    {
        return not_found(stderr);
    }

    // --- Network: timeout / connection reset → Timeout -------------------
    if stderr_l.contains("timeout")
        || stderr_l.contains("timed out")
        || stderr_l.contains("etimedout")
        || stderr_l.contains("deadline exceeded")
        || stderr_l.contains("connection reset")
        || stderr_l.contains("connection refused")
        || stderr_l.contains("i/o timeout")
        || stderr_l.contains("no such host")
        || stderr_l.contains("dial tcp")
    {
        return BackendError::Timeout;
    }

    // --- GraphQL 200 + errors[] ------------------------------------------
    // `gh api graphql` exits non-zero on GraphQL errors but still prints the
    // `{"errors":[...]}` body to stdout. If we can parse an `errors[]` array,
    // route by mutation-vs-read.
    if let Some(errs) = json.as_ref().and_then(graphql_errors) {
        if !errs.is_empty() {
            let msg = graphql_error_message(&errs);
            // Scope errors inside the GraphQL body (defensive — usually caught above).
            if errs.iter().any(|e| {
                e.get("type")
                    .and_then(|t| t.as_str())
                    .map(|t| t.eq_ignore_ascii_case("INSUFFICIENT_SCOPES"))
                    .unwrap_or(false)
            }) {
                return BackendError::Unauthorized {
                    message: format!("GitHub rejected the request: {msg}"),
                    remediation: Some(SCOPE_REMEDIATION.to_string()),
                };
            }
            // NOT_FOUND inside the GraphQL body.
            if errs.iter().any(|e| {
                e.get("type")
                    .and_then(|t| t.as_str())
                    .map(|t| t.eq_ignore_ascii_case("NOT_FOUND"))
                    .unwrap_or(false)
            }) {
                return not_found(&msg);
            }
            return if mutation {
                // The write may have applied → NOT blind-retryable. Backend
                // reconciles via write-then-verify.
                BackendError::Permanent(format!("GraphQL mutation error: {msg}"))
            } else {
                // Read path: no side effect, safe to retry.
                BackendError::Transient(format!("GraphQL read error: {msg}"))
            };
        }
    }

    // --- JSON shape problems ---------------------------------------------
    // We only reach here with non-empty stdout that *looked* like it should be
    // JSON. Truncated/garbled JSON → Transient (retry). Valid-but-wrong-schema
    // is the caller's concern (the deserializer raises Serde→Permanent), but if
    // `gh` itself reports a malformed/unexpected response we can disambiguate:
    if !stdout.is_empty() {
        if json.is_none() && looks_like_json(stdout_str.trim()) {
            // Started like JSON but failed to parse → truncated/garbled.
            return BackendError::Transient("truncated or malformed JSON response".to_string());
        }
        if json.is_some() {
            // Valid JSON but no recognizable error array and a non-zero exit:
            // unexpected schema → permanent.
            return BackendError::Permanent(format!(
                "unexpected `gh` response (exit {}): {}",
                exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                truncate(&stdout_str, 200)
            ));
        }
    }

    // --- Fallback ---------------------------------------------------------
    // Unknown non-zero exit with no recognizable signal. Conservatively
    // permanent so we never blind-retry an opaque failure (especially a mutation).
    BackendError::Permanent(format!(
        "`gh` failed (exit {}): {}",
        exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into()),
        first_line(stderr)
    ))
}

// --------------------------------------------------------------------------
// Classifier helpers
// --------------------------------------------------------------------------

/// Build a `NotFound`. The trait's `NotFound` carries an `i64` id; we don't have
/// the logical id at the transport layer, so we surface a `Permanent` only when no
/// id is recoverable. Here we emit `NotFound(-1)` as a sentinel — the backend layer
/// maps this onto the real id it requested. Assumption documented for
/// live verification.
fn not_found(detail: &str) -> BackendError {
    // No numeric id available at the transport seam; the backend re-wraps with the
    // id it asked for. Use a sentinel so the variant is correct for the test table.
    let _ = detail;
    BackendError::NotFound(-1)
}

/// First non-empty line of a message, trimmed.
fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Truncate a string to `max` chars (best-effort, char-boundary safe).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

/// Does the trimmed text look like it was intended to be JSON?
fn looks_like_json(s: &str) -> bool {
    s.starts_with('{') || s.starts_with('[')
}

/// Best-effort: find an `HTTP <status>` token in `gh`'s stderr.
///
/// `gh api` prints lines like `gh: ... (HTTP 404)` or `HTTP 422: Unprocessable`.
/// Assumption flagged for live verification.
fn http_status(stderr: &str) -> Option<i32> {
    let lower = stderr.to_lowercase();
    let idx = lower.find("http ")?;
    let rest = &stderr[idx + 5..];
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Extract a header value (case-insensitive header name) from a dump of HTTP
/// response headers in stderr, e.g. `Retry-After: 30` or `X-RateLimit-Reset: ...`.
fn header_value(stderr: &str, name: &str) -> Option<String> {
    let name_l = name.to_lowercase();
    for line in stderr.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().to_lowercase() == name_l {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Parse a `Retry-After: N` header (seconds) into a `Duration`.
fn parse_retry_after(stderr: &str) -> Option<Duration> {
    let v = header_value(stderr, "retry-after")?;
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Derive a wait from `X-RateLimit-Reset` (a Unix epoch second) relative to now.
/// Returns `None` if the reset is in the past or unparseable.
fn parse_ratelimit_reset(stderr: &str) -> Option<Duration> {
    let v = header_value(stderr, "x-ratelimit-reset")?;
    let reset = v.trim().parse::<u64>().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    reset.checked_sub(now).map(Duration::from_secs)
}

/// Pull the `errors` array out of a parsed `gh api graphql` JSON body.
fn graphql_errors(json: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    json.get("errors")
        .and_then(|e| e.as_array())
        .map(|a| a.to_vec())
}

/// Join GraphQL error messages for a human-readable summary.
fn graphql_error_message(errs: &[serde_json::Value]) -> String {
    let msgs: Vec<String> = errs
        .iter()
        .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
        .map(|s| s.to_string())
        .collect();
    if msgs.is_empty() {
        "unknown GraphQL error".to_string()
    } else {
        msgs.join("; ")
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_unavailable(e: &BackendError) {
        assert!(matches!(e, BackendError::Unavailable(_)), "got {e:?}");
    }

    #[test]
    fn classify_command_not_found_unavailable() {
        let e = classify(Some(127), b"", "gh: command not found", false);
        assert_unavailable(&e);
    }

    #[test]
    fn classify_not_logged_in_unavailable() {
        let e = classify(
            Some(1),
            b"",
            "You are not logged into any GitHub hosts. Run gh auth login to authenticate.",
            false,
        );
        assert_unavailable(&e);
    }

    #[test]
    fn classify_insufficient_scopes_unauthorized_with_remediation() {
        let body = r#"{"errors":[{"type":"INSUFFICIENT_SCOPES","message":"Your token has not been granted the required scopes."}]}"#;
        let e = classify(Some(1), body.as_bytes(), "", false);
        match e {
            BackendError::Unauthorized { remediation, .. } => {
                assert_eq!(remediation.as_deref(), Some(SCOPE_REMEDIATION));
            }
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    #[test]
    fn classify_http_403_scope_unauthorized() {
        let e = classify(
            Some(1),
            b"",
            "gh: This API operation needs the `project` scope. (HTTP 403)",
            false,
        );
        match e {
            BackendError::Unauthorized { remediation, .. } => {
                assert_eq!(remediation.as_deref(), Some(SCOPE_REMEDIATION));
            }
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    #[test]
    fn classify_429_retry_after_rate_limited() {
        let stderr = "gh: API rate limit exceeded (HTTP 429)\nRetry-After: 42\n";
        let e = classify(Some(1), b"", stderr, false);
        match e {
            BackendError::RateLimited { retry_after } => {
                assert_eq!(retry_after, Some(Duration::from_secs(42)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_secondary_limit_retry_after() {
        let stderr = "You have exceeded a secondary rate limit.\nRetry-After: 7\n";
        let e = classify(Some(1), b"", stderr, true);
        match e {
            BackendError::RateLimited { retry_after } => {
                assert_eq!(retry_after, Some(Duration::from_secs(7)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_primary_limit_from_reset() {
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 30;
        let stderr = format!(
            "gh: API rate limit exceeded\nX-RateLimit-Remaining: 0\nX-RateLimit-Reset: {future}\n"
        );
        let e = classify(Some(1), b"", &stderr, false);
        match e {
            BackendError::RateLimited { retry_after } => {
                let d = retry_after.expect("expected a reset-derived duration");
                // ~30s out; allow slack for test execution time.
                assert!(
                    d <= Duration::from_secs(30) && d >= Duration::from_secs(20),
                    "got {d:?}"
                );
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_5xx_transient() {
        let e = classify(Some(1), b"", "gh: Server Error (HTTP 503)", false);
        assert!(matches!(e, BackendError::Transient(_)), "got {e:?}");
    }

    #[test]
    fn classify_timeout() {
        let e = classify(
            Some(1),
            b"",
            "Post https://api.github.com/graphql: dial tcp: i/o timeout",
            false,
        );
        assert!(matches!(e, BackendError::Timeout), "got {e:?}");
    }

    #[test]
    fn classify_connection_reset_timeout() {
        let e = classify(Some(1), b"", "read: connection reset by peer", true);
        assert!(matches!(e, BackendError::Timeout), "got {e:?}");
    }

    #[test]
    fn classify_404_not_found() {
        let e = classify(Some(1), b"", "gh: Not Found (HTTP 404)", false);
        assert!(matches!(e, BackendError::NotFound(_)), "got {e:?}");
    }

    #[test]
    fn classify_could_not_resolve_not_found() {
        let body = r#"{"errors":[{"type":"NOT_FOUND","message":"Could not resolve to a Repository with the name 'x/y'."}]}"#;
        let e = classify(Some(1), body.as_bytes(), "", false);
        assert!(matches!(e, BackendError::NotFound(_)), "got {e:?}");
    }

    #[test]
    fn classify_409_transient() {
        let e = classify(Some(1), b"", "gh: Conflict (HTTP 409)", true);
        assert!(matches!(e, BackendError::Transient(_)), "got {e:?}");
    }

    #[test]
    fn classify_422_permanent() {
        let e = classify(
            Some(1),
            b"",
            "gh: Validation Failed (HTTP 422): label does not exist",
            true,
        );
        assert!(matches!(e, BackendError::Permanent(_)), "got {e:?}");
    }

    #[test]
    fn classify_400_permanent() {
        let e = classify(Some(1), b"", "gh: Bad Request (HTTP 400)", false);
        assert!(matches!(e, BackendError::Permanent(_)), "got {e:?}");
    }

    #[test]
    fn classify_graphql_errors_on_read_transient() {
        let body = r#"{"data":null,"errors":[{"message":"Something failed transiently"}]}"#;
        let e = classify(Some(1), body.as_bytes(), "", false);
        assert!(matches!(e, BackendError::Transient(_)), "got {e:?}");
    }

    #[test]
    fn classify_graphql_errors_on_mutation_permanent() {
        // The invariant: a mutation's partial error is NEVER blind-retryable.
        let body = r#"{"data":null,"errors":[{"message":"field already set"}]}"#;
        let e = classify(Some(1), body.as_bytes(), "", true);
        assert!(matches!(e, BackendError::Permanent(_)), "got {e:?}");
        assert!(!e.is_retryable());
    }

    #[test]
    fn classify_truncated_json_transient() {
        let e = classify(Some(1), b"{\"data\": {\"node\": ", "", false);
        assert!(matches!(e, BackendError::Transient(_)), "got {e:?}");
    }

    #[test]
    fn classify_valid_json_wrong_schema_permanent() {
        // Valid JSON, no errors[] array, non-zero exit → unexpected schema.
        let e = classify(Some(1), br#"{"unexpected":"shape"}"#, "", false);
        assert!(matches!(e, BackendError::Permanent(_)), "got {e:?}");
    }

    #[test]
    fn classify_unknown_failure_is_permanent_not_retryable() {
        let e = classify(Some(2), b"", "gh: something weird happened", true);
        assert!(matches!(e, BackendError::Permanent(_)), "got {e:?}");
        assert!(!e.is_retryable());
    }
}
