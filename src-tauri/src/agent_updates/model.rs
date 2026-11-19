//! Pure decisions for agent CLI updates: version parsing and comparison, how a CLI was
//! installed and the command that updates it, the update assessment, the run's sentinel
//! line and its outcome, and the check schedule. Nothing here touches the machine.

use std::time::Duration;

use keitora_tasks::CliKind;
use serde::Serialize;

use crate::backend::herdr::agent::herdr_kind;
use crate::backend::SessionKey;

/// Marker the update run echoes, followed by the update command's exit code.
pub const SENTINEL: &str = "__KEITORA_AGENT_UPDATE_DONE:";

/// How long a `latest_version` in Codex's own `version.json` is trusted.
const VERSION_JSON_MAX_AGE_HOURS: i64 = 24;

/// How the CLI executable was installed, judged from its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    Npm,
    Brew,
    Native,
    Unknown,
}

/// Where a CLI stands against its latest release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentUpdateState {
    /// No check has completed since launch.
    Unchecked,
    /// The CLI was not found on this machine.
    NotInstalled,
    /// The installed or latest version could not be determined.
    Unknown,
    UpToDate,
    /// `command` is what Keitora runs; `None` means only guidance can be shown.
    Available {
        installed: String,
        latest: String,
        command: Option<String>,
    },
    /// The user skipped `latest`; a newer release shows the banner again.
    Skipped {
        latest: String,
    },
}

/// How an update run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunOutcome {
    Updated {
        from: Option<String>,
        to: String,
    },
    /// The command succeeded but the version did not change (a package manager that
    /// lags behind the release, for example).
    Unchanged {
        version: Option<String>,
    },
    Failed {
        code: i32,
        hint: Option<String>,
    },
    /// No exit line appeared before the watch gave up.
    TimedOut,
}

/// The per-CLI facts the install and update rules read.
struct CliInfo {
    /// The display name used in hints.
    title: &'static str,
    /// The CLI's own install directory under the home directory, with a trailing `/`.
    native_dir: &'static str,
    npm: &'static str,
    brew: &'static str,
    /// The CLI's own updater, for a native install.
    native_update: &'static str,
    /// Whether Keitora runs `native_update` itself; otherwise it is only suggested.
    native_update_runs: bool,
}

const CLAUDE_INFO: CliInfo = CliInfo {
    title: "Claude",
    native_dir: ".claude/local/",
    npm: "npm install -g @anthropic-ai/claude-code@latest",
    brew: "brew upgrade --cask claude-code",
    native_update: "claude update",
    native_update_runs: true,
};

/// Codex's own updater could not be verified, so it is only suggested.
const CODEX_INFO: CliInfo = CliInfo {
    title: "Codex",
    native_dir: ".codex/bin/",
    npm: "npm install -g @openai/codex@latest",
    brew: "brew upgrade --cask codex",
    native_update: "codex update",
    native_update_runs: false,
};

fn info(cli: CliKind) -> &'static CliInfo {
    match cli {
        CliKind::Claude => &CLAUDE_INFO,
        CliKind::Codex => &CODEX_INFO,
    }
}

/// The display name used in hints.
pub fn cli_title(cli: CliKind) -> &'static str {
    info(cli).title
}

/// `/`-separated, lower-cased, without a trailing `/`.
fn normalize(path: &str) -> String {
    path.replace('\\', "/")
        .to_lowercase()
        .trim_end_matches('/')
        .to_string()
}

/// Classify how `cli` was installed from `exe_path` (as configured or discovered), the
/// path it resolves to once symlinks are followed (`resolved`; the same path when it
/// is no link or cannot be resolved) and the user's home directory. The resolved path
/// is classified first, since a launcher such as `/opt/homebrew/bin/codex` may link
/// into an npm `node_modules` tree; when it says nothing, the path as found is (a
/// native launcher links into a versions directory no rule knows).
pub fn install_kind(
    cli: CliKind,
    exe_path: &str,
    resolved: &str,
    home: Option<&str>,
) -> InstallKind {
    match classify_path(cli, resolved, home) {
        InstallKind::Unknown => classify_path(cli, exe_path, home),
        kind => kind,
    }
}

/// [`install_kind`] for one path. The first matching rule wins: npm-style paths, then
/// Homebrew, then the CLI's own native installer, else `Unknown`.
fn classify_path(cli: CliKind, exe_path: &str, home: Option<&str>) -> InstallKind {
    let path = normalize(exe_path);
    const NPM_MARKERS: [&str; 6] = [
        "/node_modules/",
        "/npm/",
        "/nvm/",
        "/nvm4w/",
        "/.nvm/",
        "/nodejs/",
    ];
    if NPM_MARKERS.iter().any(|m| path.contains(m)) {
        return InstallKind::Npm;
    }
    if path.starts_with("/opt/homebrew/")
        || path.contains("/caskroom/")
        || path.contains("/cellar/")
    {
        return InstallKind::Brew;
    }
    if let Some(home) = home.map(normalize).filter(|h| !h.is_empty()) {
        let local_bin = format!("{home}/.local/bin/{}", herdr_kind(cli));
        if let Some(rest) = path.strip_prefix(&local_bin) {
            if rest.is_empty() || rest == ".exe" {
                return InstallKind::Native;
            }
        }
        if path.starts_with(&format!("{home}/{}", info(cli).native_dir)) {
            return InstallKind::Native;
        }
    }
    InstallKind::Unknown
}

/// How `cli` installed as some [`InstallKind`] is updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCommands {
    /// What Keitora runs, or `None` when it only shows guidance (an unknown install, or
    /// a native install whose updater Keitora does not run).
    pub run: Option<String>,
    /// The command to show the user: what Keitora would run, else a suggestion for their
    /// own terminal.
    pub guidance: String,
}

/// The update commands for `cli` installed as `kind`. `npm` is the npm that belongs
/// to an npm install (found beside it, see [`npm_candidates`]): it is invoked by its
/// absolute path, so the update lands in the same Node as the CLI rather than in
/// whichever `npm` comes first on `PATH`. Without one, the bare `npm` is used.
pub fn update_commands(cli: CliKind, kind: InstallKind, npm: Option<&str>) -> UpdateCommands {
    let info = info(cli);
    let (cmd, runs) = match kind {
        InstallKind::Npm => (info.npm, true),
        InstallKind::Brew => (info.brew, true),
        InstallKind::Native => (info.native_update, info.native_update_runs),
        InstallKind::Unknown => (info.npm, false),
    };
    let cmd = match (kind, npm, cmd.strip_prefix("npm ")) {
        (InstallKind::Npm, Some(npm), Some(rest)) => {
            format!("{} {rest}", posix_quote(&npm.replace('\\', "/")))
        }
        _ => cmd.to_string(),
    };
    UpdateCommands {
        run: runs.then(|| cmd.clone()),
        guidance: cmd,
    }
}

/// `s` single-quoted for a POSIX shell (the update pane's shell; Git Bash on
/// Windows): each `'` inside becomes `'\''`.
fn posix_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Where the npm that owns an npm-installed CLI may be, most likely first, each
/// `/`-separated (the caller keeps the first that exists): beside the executable as
/// found (an nvm or Node `bin` directory, or the Windows Node directory), beside the
/// path it resolves to, and with the Node prefix whose `node_modules` the resolved
/// path lies in. `npm.cmd` on Windows, else `npm`.
pub fn npm_candidates(exe_path: &str, resolved: &str, windows: bool) -> Vec<String> {
    let npm = if windows { "npm.cmd" } else { "npm" };
    let exe_path = exe_path.replace('\\', "/");
    let resolved = resolved.replace('\\', "/");
    let parent = |p: &str| p.rfind('/').map(|i| p[..i].to_string());
    let mut dirs: Vec<String> = [parent(&exe_path), parent(&resolved)]
        .into_iter()
        .flatten()
        .filter(|d| !d.is_empty())
        .collect();
    if let Some(i) = resolved.to_lowercase().find("/node_modules/") {
        let prefix = &resolved[..i];
        // unix: `<root>/lib/node_modules`, npm in `<root>/bin`; Windows:
        // `<prefix>/node_modules`, npm.cmd in `<prefix>` itself.
        dirs.push(if windows {
            prefix.to_string()
        } else {
            format!("{}/bin", prefix.strip_suffix("/lib").unwrap_or(prefix))
        });
    }
    let mut out: Vec<String> = Vec::new();
    for dir in dirs {
        let candidate = format!("{dir}/{npm}");
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// `claude --version` output (`"2.1.328 (Claude Code)"`) to its version: the first
/// token of the first non-empty line that starts with `N.N.N`.
pub fn parse_claude_version_output(stdout: &str) -> Option<String> {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    line.split_whitespace()
        .find(|tok| crate::codex_env::starts_with_semver(tok))
        .map(str::to_string)
}

/// Codex's `version.json` cache (`{"latest_version", "last_checked_at", …}`): the latest
/// version when it was checked less than a day before `now`, else `None`.
pub fn parse_codex_version_json(text: &str, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let latest = v.get("latest_version")?.as_str()?.trim();
    parse_version(latest)?;
    let checked = v.get("last_checked_at")?.as_str()?;
    let checked = chrono::DateTime::parse_from_rfc3339(checked).ok()?;
    let age = now.signed_duration_since(checked);
    let fresh = age < chrono::Duration::hours(VERSION_JSON_MAX_AGE_HOURS)
        && age > -chrono::Duration::hours(1);
    fresh.then(|| latest.to_string())
}

/// An npm registry `…/latest` document to its `version`.
pub fn parse_npm_latest(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let version = v.get("version")?.as_str()?.trim();
    parse_version(version)?;
    Some(version.to_string())
}

/// Lenient semver: surrounding space and a leading `v` are ignored, and a missing patch
/// (`1.2`) reads as `1.2.0`.
pub fn parse_version(s: &str) -> Option<semver::Version> {
    let s = s.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    semver::Version::parse(s).ok().or_else(|| {
        let mut parts = s.split('.');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(a), Some(b), None) => semver::Version::parse(&format!("{a}.{b}.0")).ok(),
            _ => None,
        }
    })
}

/// Assess one CLI. `found` is whether its executable was found; `command` is what
/// Keitora would run to update it.
pub fn assess(
    found: bool,
    installed: Option<&str>,
    latest: Option<&str>,
    skipped: Option<&str>,
    command: Option<String>,
) -> AgentUpdateState {
    if !found {
        return AgentUpdateState::NotInstalled;
    }
    let (Some(installed), Some(latest)) = (installed, latest) else {
        return AgentUpdateState::Unknown;
    };
    let (Some(have), Some(newest)) = (parse_version(installed), parse_version(latest)) else {
        return AgentUpdateState::Unknown;
    };
    if newest <= have {
        return AgentUpdateState::UpToDate;
    }
    if skipped.and_then(parse_version).is_some_and(|s| s == newest) {
        return AgentUpdateState::Skipped {
            latest: latest.to_string(),
        };
    }
    AgentUpdateState::Available {
        installed: installed.to_string(),
        latest: latest.to_string(),
        command,
    }
}

/// The shell line typed into the update pane: the update, then the sentinel with its
/// exit code. POSIX syntax (the pane shell is Git Bash on Windows).
pub fn build_run_command(command: &str) -> String {
    format!("{command}; echo \"{SENTINEL}$?\"")
}

/// The exit code from the last sentinel line in `history`. The echoed command line also
/// contains the marker, but it does not start a line and ends in `$?"`, so it never
/// matches.
pub fn parse_sentinel(history: &str) -> Option<i32> {
    history.lines().rev().find_map(|line| {
        line.trim()
            .strip_prefix(SENTINEL)
            .and_then(|rest| rest.trim().parse::<i32>().ok())
    })
}

/// A hint for a failed run, from the pane's recent output.
pub fn failure_hint(cli: CliKind, history: &str) -> Option<String> {
    if history.contains("EBUSY") || history.contains("EPERM") {
        return Some(format!("close running {} sessions first", cli_title(cli)));
    }
    if history.contains("EACCES") {
        return Some(
            "needs elevated permissions; run the command with sudo in your own terminal"
                .to_string(),
        );
    }
    None
}

/// The outcome of a run that exited with `code`, given the version before and after.
pub fn run_outcome(
    cli: CliKind,
    code: i32,
    before: Option<&str>,
    after: Option<&str>,
    history: &str,
) -> RunOutcome {
    if code != 0 {
        return RunOutcome::Failed {
            code,
            hint: failure_hint(cli, history),
        };
    }
    match after {
        Some(to) if before != Some(to) => RunOutcome::Updated {
            from: before.map(str::to_string),
            to: to.to_string(),
        },
        _ => RunOutcome::Unchanged {
            version: after.or(before).map(str::to_string),
        },
    }
}

/// Watch polls at the fast interval before the watcher backs off.
const WATCH_FAST_POLLS: u32 = 10;
const WATCH_FAST: Duration = Duration::from_millis(500);
const WATCH_SLOW: Duration = Duration::from_secs(2);

/// The wait after the update pane's `poll`-th read (from 0): fast while a quick update
/// is likely to finish, then backed off.
pub fn watch_delay(poll: u32) -> Duration {
    if poll < WATCH_FAST_POLLS {
        WATCH_FAST
    } else {
        WATCH_SLOW
    }
}

/// Whether a periodic check is due `since_last` after the previous one, every `hours`
/// hours. `0` means checks run at app start only.
pub fn check_due(since_last: Duration, hours: u32) -> bool {
    hours != 0 && since_last >= Duration::from_secs(u64::from(hours) * 60 * 60)
}

/// The sessions among `keys` whose agent is `cli`, judged by `cli_of` (the launch a
/// spawn would use). Keys it cannot place are left out.
pub fn sessions_of(
    keys: impl IntoIterator<Item = SessionKey>,
    cli: CliKind,
    cli_of: impl Fn(&SessionKey) -> Option<CliKind>,
) -> Vec<SessionKey> {
    keys.into_iter()
        .filter(|k| !k.is_reserved() && cli_of(k) == Some(cli))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const HOME_WIN: &str = "C:\\Users\\x";
    const HOME_MAC: &str = "/Users/me";
    const HOME_LINUX: &str = "/home/u";

    #[test]
    fn claude_version_output_variants() {
        let cases: &[(&str, Option<&str>)] = &[
            ("2.1.328 (Claude Code)\n", Some("2.1.328")),
            ("\n  2.1.328 (Claude Code)", Some("2.1.328")),
            ("claude 2.2.0-beta.1", Some("2.2.0-beta.1")),
            ("Claude Code", None),
            ("", None),
            ("v2", None),
        ];
        for (out, want) in cases {
            assert_eq!(
                parse_claude_version_output(out).as_deref(),
                *want,
                "{out:?}"
            );
        }
    }

    fn at(h: u32, m: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 11, 18, h, m, 0).unwrap()
    }

    #[test]
    fn codex_version_json_trusted_only_when_fresh() {
        let doc = |checked: &str| {
            format!(
                r#"{{"latest_version":"0.163.0","last_checked_at":"{checked}","dismissed_version":null}}"#
            )
        };
        let now = at(12, 0);
        assert_eq!(
            parse_codex_version_json(&doc("2026-11-18T06:00:00.123456Z"), now).as_deref(),
            Some("0.163.0")
        );
        assert_eq!(
            parse_codex_version_json(&doc("2026-11-17T12:30:00Z"), now).as_deref(),
            Some("0.163.0")
        );
        assert_eq!(
            parse_codex_version_json(&doc("2026-11-17T11:59:00Z"), now),
            None,
            "older than a day"
        );
        assert_eq!(
            parse_codex_version_json(&doc("2026-11-19T12:00:00Z"), now),
            None,
            "far in the future"
        );
        assert_eq!(parse_codex_version_json(&doc("yesterday"), now), None);
        assert_eq!(
            parse_codex_version_json(r#"{"last_checked_at":"2026-11-18T06:00:00Z"}"#, now),
            None
        );
        assert_eq!(
            parse_codex_version_json(
                r#"{"latest_version":"soon","last_checked_at":"2026-11-18T06:00:00Z"}"#,
                now
            ),
            None
        );
        assert_eq!(parse_codex_version_json("not json", now), None);
    }

    #[test]
    fn npm_latest_document() {
        assert_eq!(
            parse_npm_latest(r#"{"name":"@openai/codex","version":"0.163.0"}"#).as_deref(),
            Some("0.163.0")
        );
        assert_eq!(parse_npm_latest(r#"{"version":"garbage"}"#), None);
        assert_eq!(parse_npm_latest(r#"{"name":"x"}"#), None);
        assert_eq!(parse_npm_latest("<html>"), None);
    }

    #[test]
    fn lenient_version_parsing() {
        let v = |s: &str| parse_version(s).map(|v| v.to_string());
        assert_eq!(v("0.163.0").as_deref(), Some("0.163.0"));
        assert_eq!(v(" v1.2.3 ").as_deref(), Some("1.2.3"));
        assert_eq!(v("1.2").as_deref(), Some("1.2.0"));
        assert_eq!(v("0.149.0-alpha.1").as_deref(), Some("0.149.0-alpha.1"));
        assert_eq!(v("1"), None);
        assert_eq!(v("x.y.z"), None);
    }

    #[test]
    fn install_kind_by_path() {
        use CliKind::{Claude, Codex};
        use InstallKind::*;
        let cases: &[(CliKind, &str, &str, InstallKind)] = &[
            // Windows
            (
                Codex,
                "C:\\Users\\x\\AppData\\Roaming\\npm\\codex.cmd",
                HOME_WIN,
                Npm,
            ),
            (Codex, "C:\\nvm4w\\nodejs\\codex.cmd", HOME_WIN, Npm),
            (
                Claude,
                "C:\\Users\\x\\.local\\bin\\claude.exe",
                HOME_WIN,
                Native,
            ),
            (
                Codex,
                "C:\\Users\\x\\.local\\bin\\codex.exe",
                HOME_WIN,
                Native,
            ),
            (
                Codex,
                "C:\\Users\\x\\.codex\\bin\\codex.exe",
                HOME_WIN,
                Native,
            ),
            (Claude, "C:\\Tools\\claude.exe", HOME_WIN, Unknown),
            // macOS
            (Codex, "/opt/homebrew/bin/codex", HOME_MAC, Brew),
            (
                Codex,
                "/usr/local/Caskroom/codex/0.163.0/codex",
                HOME_MAC,
                Brew,
            ),
            (
                Claude,
                "/usr/local/Cellar/claude-code/2.1.0/bin/claude",
                HOME_MAC,
                Brew,
            ),
            (Claude, "/Users/me/.local/bin/claude", HOME_MAC, Native),
            (Claude, "/Users/me/.claude/local/claude", HOME_MAC, Native),
            (
                Codex,
                "/Users/me/.nvm/versions/node/v22.0.0/bin/codex",
                HOME_MAC,
                Npm,
            ),
            (Codex, "/usr/local/bin/codex", HOME_MAC, Unknown),
            // An npm global under a brew node still lives in node_modules.
            (
                Codex,
                "/opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js",
                HOME_MAC,
                Npm,
            ),
            // Linux
            (Claude, "/home/u/.local/bin/claude", HOME_LINUX, Native),
            (
                Claude,
                "/home/u/.local/bin/claude-wrapper",
                HOME_LINUX,
                Unknown,
            ),
            (
                Codex,
                "/home/u/.npm-global/lib/node_modules/.bin/codex",
                HOME_LINUX,
                Npm,
            ),
            (Codex, "/usr/bin/codex", HOME_LINUX, Unknown),
            // The other CLI's native dir is not native for this one.
            (Codex, "/home/u/.claude/local/codex", HOME_LINUX, Unknown),
        ];
        for (cli, path, home, want) in cases {
            assert_eq!(
                install_kind(*cli, path, path, Some(home)),
                *want,
                "{cli:?} {path}"
            );
        }
        let native = "/home/u/.local/bin/claude";
        assert_eq!(
            install_kind(Claude, native, native, None),
            Unknown,
            "no home, no native rule"
        );
    }

    #[test]
    fn install_kind_follows_the_resolved_path() {
        use CliKind::{Claude, Codex};
        use InstallKind::*;
        let cases: &[(CliKind, &str, &str, InstallKind)] = &[
            // A brew-prefix launcher linking into an npm global is npm...
            (
                Codex,
                "/opt/homebrew/bin/codex",
                "/opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js",
                Npm,
            ),
            // ...and one linking into a cask is brew.
            (
                Codex,
                "/opt/homebrew/bin/codex",
                "/opt/homebrew/Caskroom/codex/0.163.0/codex",
                Brew,
            ),
            (
                Codex,
                "/usr/local/bin/codex",
                "/usr/local/lib/node_modules/@openai/codex/bin/codex.js",
                Npm,
            ),
            // A native launcher links into a versions dir no rule knows: the path as
            // found decides.
            (
                Claude,
                "/Users/me/.local/bin/claude",
                "/Users/me/.local/share/claude/versions/2.1.328",
                Native,
            ),
            (Codex, "/usr/bin/codex", "/opt/tools/codex", Unknown),
        ];
        for (cli, path, resolved, want) in cases {
            assert_eq!(
                install_kind(*cli, path, resolved, Some(HOME_MAC)),
                *want,
                "{cli:?} {path} -> {resolved}"
            );
        }
    }

    #[test]
    fn npm_is_looked_for_beside_the_cli_and_in_its_node_prefix() {
        assert_eq!(
            npm_candidates(
                "/Users/me/.nvm/versions/node/v22.0.0/bin/codex",
                "/Users/me/.nvm/versions/node/v22.0.0/lib/node_modules/@openai/codex/bin/codex.js",
                false,
            ),
            vec![
                "/Users/me/.nvm/versions/node/v22.0.0/bin/npm",
                "/Users/me/.nvm/versions/node/v22.0.0/lib/node_modules/@openai/codex/bin/npm",
            ],
            "the prefix bin is the launcher's own directory: listed once"
        );
        assert_eq!(
            npm_candidates(
                "/usr/local/bin/codex",
                "/opt/node/lib/node_modules/@openai/codex/bin/codex.js",
                false,
            ),
            vec![
                "/usr/local/bin/npm",
                "/opt/node/lib/node_modules/@openai/codex/bin/npm",
                "/opt/node/bin/npm",
            ]
        );
        assert_eq!(
            npm_candidates(
                "C:\\nvm4w\\nodejs\\codex.cmd",
                "C:\\nvm4w\\nodejs\\codex.cmd",
                true
            ),
            vec!["C:/nvm4w/nodejs/npm.cmd"]
        );
        assert_eq!(
            npm_candidates(
                "C:\\Users\\x\\AppData\\Roaming\\npm\\codex.cmd",
                "C:\\Users\\x\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex\\bin\\codex.js",
                true,
            ),
            vec![
                "C:/Users/x/AppData/Roaming/npm/npm.cmd",
                "C:/Users/x/AppData/Roaming/npm/node_modules/@openai/codex/bin/npm.cmd",
            ]
        );
        assert!(npm_candidates("codex", "codex", false).is_empty());
    }

    #[test]
    fn an_npm_update_runs_the_installs_own_npm() {
        use CliKind::{Claude, Codex};
        use InstallKind::*;
        // Found: invoked by its quoted absolute path, with forward slashes.
        let cmds = update_commands(
            Codex,
            Npm,
            Some("/Users/me/.nvm/versions/node/v22.0.0/bin/npm"),
        );
        assert_eq!(
            cmds.run.as_deref(),
            Some("'/Users/me/.nvm/versions/node/v22.0.0/bin/npm' install -g @openai/codex@latest")
        );
        assert_eq!(Some(cmds.guidance.as_str()), cmds.run.as_deref());
        // Spaces and quotes survive the POSIX shell.
        assert_eq!(
            update_commands(Claude, Npm, Some("C:\\Program Files\\nodejs\\npm.cmd"))
                .run
                .as_deref(),
            Some("'C:/Program Files/nodejs/npm.cmd' install -g @anthropic-ai/claude-code@latest")
        );
        assert_eq!(
            update_commands(Codex, Npm, Some("/opt/it's node/bin/npm"))
                .run
                .as_deref(),
            Some(r"'/opt/it'\''s node/bin/npm' install -g @openai/codex@latest")
        );
        // Not found: the bare npm on PATH.
        assert_eq!(
            update_commands(Codex, Npm, None).run.as_deref(),
            Some("npm install -g @openai/codex@latest")
        );
        // Other install kinds ignore it.
        assert_eq!(
            update_commands(Codex, Brew, Some("/x/npm")).run.as_deref(),
            Some("brew upgrade --cask codex")
        );
        assert_eq!(
            update_commands(Codex, Unknown, Some("/x/npm")).guidance,
            "npm install -g @openai/codex@latest"
        );
    }

    #[test]
    fn update_command_per_install_kind() {
        use CliKind::{Claude, Codex};
        use InstallKind::*;
        let cases: &[(CliKind, InstallKind, Option<&str>, &str)] = &[
            (
                Codex,
                Npm,
                Some("npm install -g @openai/codex@latest"),
                "npm install -g @openai/codex@latest",
            ),
            (
                Codex,
                Brew,
                Some("brew upgrade --cask codex"),
                "brew upgrade --cask codex",
            ),
            (Codex, Native, None, "codex update"),
            (Codex, Unknown, None, "npm install -g @openai/codex@latest"),
            (
                Claude,
                Npm,
                Some("npm install -g @anthropic-ai/claude-code@latest"),
                "npm install -g @anthropic-ai/claude-code@latest",
            ),
            (
                Claude,
                Brew,
                Some("brew upgrade --cask claude-code"),
                "brew upgrade --cask claude-code",
            ),
            (Claude, Native, Some("claude update"), "claude update"),
            (
                Claude,
                Unknown,
                None,
                "npm install -g @anthropic-ai/claude-code@latest",
            ),
        ];
        for (cli, kind, run, shown) in cases {
            let cmds = update_commands(*cli, *kind, None);
            assert_eq!(cmds.run.as_deref(), *run, "{cli:?} {kind:?}");
            assert_eq!(cmds.guidance, *shown, "{cli:?} {kind:?}");
        }
    }

    #[test]
    fn assessment_transitions() {
        let cmd = || Some("npm install -g @openai/codex@latest".to_string());
        assert_eq!(
            assess(false, None, Some("0.163.0"), None, cmd()),
            AgentUpdateState::NotInstalled
        );
        assert_eq!(
            assess(true, None, Some("0.163.0"), None, cmd()),
            AgentUpdateState::Unknown,
            "version probe failed"
        );
        assert_eq!(
            assess(true, Some("0.160.0"), None, None, cmd()),
            AgentUpdateState::Unknown,
            "offline"
        );
        assert_eq!(
            assess(true, Some("garbage"), Some("0.163.0"), None, cmd()),
            AgentUpdateState::Unknown
        );
        assert_eq!(
            assess(true, Some("0.163.0"), Some("0.163.0"), None, cmd()),
            AgentUpdateState::UpToDate
        );
        assert_eq!(
            assess(true, Some("0.164.0"), Some("0.163.0"), None, cmd()),
            AgentUpdateState::UpToDate,
            "ahead of the registry"
        );
        assert_eq!(
            assess(true, Some("0.149.0-alpha.1"), Some("0.149.0"), None, cmd()),
            AgentUpdateState::Available {
                installed: "0.149.0-alpha.1".into(),
                latest: "0.149.0".into(),
                command: cmd(),
            },
            "a pre-release is older than its release"
        );
        assert_eq!(
            assess(true, Some("0.160.0"), Some("0.163.0"), None, None),
            AgentUpdateState::Available {
                installed: "0.160.0".into(),
                latest: "0.163.0".into(),
                command: None,
            }
        );
        assert_eq!(
            assess(
                true,
                Some("0.160.0"),
                Some("0.163.0"),
                Some("0.163.0"),
                cmd()
            ),
            AgentUpdateState::Skipped {
                latest: "0.163.0".into()
            }
        );
        // A newer release than the skipped one re-arms the banner.
        assert_eq!(
            assess(
                true,
                Some("0.160.0"),
                Some("0.164.0"),
                Some("0.163.0"),
                cmd()
            ),
            AgentUpdateState::Available {
                installed: "0.160.0".into(),
                latest: "0.164.0".into(),
                command: cmd(),
            }
        );
        // A garbled skip is ignored.
        assert!(matches!(
            assess(true, Some("0.160.0"), Some("0.163.0"), Some("later"), cmd()),
            AgentUpdateState::Available { .. }
        ));
    }

    #[test]
    fn state_wire_shape() {
        assert_eq!(
            serde_json::to_value(AgentUpdateState::Available {
                installed: "0.160.0".into(),
                latest: "0.163.0".into(),
                command: None,
            })
            .unwrap(),
            serde_json::json!({"type": "available", "installed": "0.160.0", "latest": "0.163.0", "command": null})
        );
        assert_eq!(
            serde_json::to_value(AgentUpdateState::NotInstalled).unwrap(),
            serde_json::json!({"type": "not_installed"})
        );
        assert_eq!(
            serde_json::to_value(RunOutcome::Failed {
                code: 1,
                hint: None
            })
            .unwrap(),
            serde_json::json!({"type": "failed", "code": 1, "hint": null})
        );
        assert_eq!(
            serde_json::to_value(InstallKind::Npm).unwrap(),
            serde_json::json!("npm")
        );
    }

    #[test]
    fn run_command_appends_the_sentinel() {
        assert_eq!(
            build_run_command("claude update"),
            "claude update; echo \"__KEITORA_AGENT_UPDATE_DONE:$?\""
        );
    }

    #[test]
    fn sentinel_parsing() {
        let typed = "$ claude update; echo \"__KEITORA_AGENT_UPDATE_DONE:$?\"";
        let cases: &[(&str, Option<i32>)] = &[
            ("", None),
            (typed, None),
            (&format!("{typed}\nUpdating…"), None),
            (
                &format!("{typed}\nUpdated\n__KEITORA_AGENT_UPDATE_DONE:0\n$ "),
                Some(0),
            ),
            (
                &format!("{typed}\n  __KEITORA_AGENT_UPDATE_DONE:127  \r\n"),
                Some(127),
            ),
            ("__KEITORA_AGENT_UPDATE_DONE:-1", Some(-1)),
            // A wrapped command line can start a row with the marker; it still ends in `$?"`.
            ("__KEITORA_AGENT_UPDATE_DONE:$?\"", None),
            ("__KEITORA_AGENT_UPDATE_DONE:", None),
            ("__KEITORA_AGENT_UPDATE_DONE:1x", None),
            ("noise __KEITORA_AGENT_UPDATE_DONE:0", None),
            // The last exit line wins.
            (
                "__KEITORA_AGENT_UPDATE_DONE:1\n__KEITORA_AGENT_UPDATE_DONE:0",
                Some(0),
            ),
        ];
        for (text, want) in cases {
            assert_eq!(parse_sentinel(text), *want, "{text:?}");
        }
    }

    #[test]
    fn failure_hints() {
        assert_eq!(
            failure_hint(CliKind::Codex, "npm error code EBUSY").as_deref(),
            Some("close running Codex sessions first")
        );
        assert_eq!(
            failure_hint(CliKind::Claude, "npm error code EPERM").as_deref(),
            Some("close running Claude sessions first")
        );
        assert_eq!(
            failure_hint(CliKind::Codex, "npm error code EACCES").as_deref(),
            Some("needs elevated permissions; run the command with sudo in your own terminal")
        );
        assert_eq!(failure_hint(CliKind::Codex, "ebusy, lowercase"), None);
        assert_eq!(failure_hint(CliKind::Codex, "network timeout"), None);
    }

    #[test]
    fn run_outcomes() {
        use CliKind::Codex;
        assert_eq!(
            run_outcome(Codex, 0, Some("0.160.0"), Some("0.163.0"), ""),
            RunOutcome::Updated {
                from: Some("0.160.0".into()),
                to: "0.163.0".into()
            }
        );
        assert_eq!(
            run_outcome(Codex, 0, None, Some("0.163.0"), ""),
            RunOutcome::Updated {
                from: None,
                to: "0.163.0".into()
            }
        );
        assert_eq!(
            run_outcome(Codex, 0, Some("0.160.0"), Some("0.160.0"), ""),
            RunOutcome::Unchanged {
                version: Some("0.160.0".into())
            }
        );
        assert_eq!(
            run_outcome(Codex, 0, Some("0.160.0"), None, ""),
            RunOutcome::Unchanged {
                version: Some("0.160.0".into())
            }
        );
        assert_eq!(
            run_outcome(Codex, 1, Some("0.160.0"), Some("0.160.0"), "EBUSY"),
            RunOutcome::Failed {
                code: 1,
                hint: Some("close running Codex sessions first".into())
            }
        );
    }

    #[test]
    fn watch_polls_fast_then_backs_off() {
        let ms = |n: u64| Duration::from_millis(n);
        assert_eq!(watch_delay(0), ms(500));
        assert_eq!(watch_delay(9), ms(500));
        assert_eq!(watch_delay(10), ms(2000));
        assert_eq!(watch_delay(500), ms(2000));
    }

    #[test]
    fn interval_schedule() {
        let h = |n: u64| Duration::from_secs(n * 60 * 60);
        assert!(!check_due(h(1000), 0), "0 means app start only");
        assert!(!check_due(h(5), 6));
        assert!(check_due(h(6), 6));
        assert!(check_due(h(7), 6));
        assert!(check_due(h(1), 1));
        assert!(!check_due(Duration::from_secs(3599), 1));
        assert!(check_due(h(168), 168));
    }

    #[test]
    fn sessions_of_filters_by_cli_and_drops_unplaced_and_reserved() {
        let key = |p: &str, w: &str| SessionKey {
            project: p.into(),
            worktree: w.into(),
        };
        let keys = vec![
            key("proj", "alpha"),
            key("proj", "bravo"),
            key("other-proj", "main"),
            SessionKey::agent_update(),
        ];
        let cli_of = |k: &SessionKey| match k.worktree.as_str() {
            "alpha" | "agent-update" => Some(CliKind::Codex),
            "bravo" => Some(CliKind::Claude),
            _ => None,
        };
        assert_eq!(
            sessions_of(keys.clone(), CliKind::Codex, cli_of),
            vec![key("proj", "alpha")]
        );
        assert_eq!(
            sessions_of(keys, CliKind::Claude, cli_of),
            vec![key("proj", "bravo")]
        );
    }
}
