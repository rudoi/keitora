//! The manual route: copyable install commands, a link to herdr's install guide,
//! validation of a herdr the user browsed to, and the plain-text "Copy details" report.

use std::path::Path;

use serde::Serialize;

use super::floor::HERDR_FLOOR;
use super::manifest::Platform;
use super::preflight::{ManualReason, Tier};
use super::{binary_version, FailureKind, Proc, VersionError};

/// herdr's install guide.
pub const INSTALL_GUIDE_URL: &str = "https://herdr.dev/docs/install/";

/// One copyable command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManualCommand {
    /// Where to paste it ("PowerShell", "Terminal").
    pub shell: String,
    pub command: String,
}

/// What the manual route shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManualRoute {
    pub reason: Option<ManualReason>,
    pub commands: Vec<ManualCommand>,
    pub link: String,
}

/// The manual route for `platform` (all platforms' commands when it is unsupported).
pub fn manual_route(platform: Option<Platform>, reason: Option<ManualReason>) -> ManualRoute {
    let windows = ManualCommand {
        shell: "PowerShell".into(),
        command:
            r#"powershell -ExecutionPolicy Bypass -c "irm https://herdr.dev/install.ps1 | iex""#
                .into(),
    };
    let macos = ManualCommand {
        shell: "Terminal".into(),
        command: "curl -fsSL https://herdr.dev/install.sh | sh".into(),
    };
    let commands = match platform {
        Some(p) if p.is_windows() => vec![windows],
        Some(_) => vec![macos],
        None => vec![macos, windows],
    };
    ManualRoute {
        reason,
        commands,
        link: INSTALL_GUIDE_URL.into(),
    }
}

/// Why a browsed-to file can't be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseError {
    NotFound,
    /// It ran (or failed to) but is not a recognisable herdr.
    NotHerdr(String),
    TooOld {
        found: semver::Version,
    },
}

impl std::fmt::Display for BrowseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrowseError::NotFound => f.write_str("that file does not exist"),
            BrowseError::NotHerdr(why) => write!(f, "that doesn't look like herdr ({why})"),
            BrowseError::TooOld { found } => write!(
                f,
                "that herdr is {found}; Keitora needs {HERDR_FLOOR} or newer"
            ),
        }
    }
}

/// Check a herdr the user picked: the file exists, its `--version` names herdr, and the
/// version is at least the floor.
pub fn validate_browse_path(path: &Path, proc_: &dyn Proc) -> Result<semver::Version, BrowseError> {
    if !path.is_file() {
        return Err(BrowseError::NotFound);
    }
    let (version, text) = binary_version(proc_, path).map_err(|e| {
        BrowseError::NotHerdr(match e {
            VersionError::Unparseable(_) => "no version reported".to_string(),
            other => other.to_string(),
        })
    })?;
    if !text.to_ascii_lowercase().contains("herdr") {
        return Err(BrowseError::NotHerdr(format!("--version printed {text:?}")));
    }
    if version < HERDR_FLOOR {
        return Err(BrowseError::TooOld { found: version });
    }
    Ok(version)
}

/// Inputs to the "Copy details" report. Only what is passed in appears in it.
pub struct DetailsInput<'a> {
    pub app_version: &'a str,
    pub platform: Option<Platform>,
    pub tier: Option<Tier>,
    pub failure: Option<(FailureKind, &'a str)>,
    pub herdr_path: Option<&'a str>,
    pub log_tail: &'a [String],
}

/// Most log lines the report carries.
pub const DETAILS_LOG_LINES: usize = 40;

/// The plain-text report a user can paste into an issue.
pub fn details_report(d: &DetailsInput<'_>) -> String {
    let wire = |v: serde_json::Value| v.as_str().map(str::to_string).unwrap_or_default();
    let mut s = String::from("Keitora herdr setup details\n");
    s.push_str(&format!("keitora: {}\n", d.app_version));
    s.push_str(&format!(
        "platform: {}\n",
        d.platform.map_or("unsupported", |p| p.key())
    ));
    s.push_str(&format!(
        "tier: {}\n",
        d.tier
            .and_then(|t| serde_json::to_value(t).ok())
            .map_or_else(|| "none".to_string(), wire)
    ));
    match d.failure {
        Some((kind, message)) => s.push_str(&format!(
            "failure: {} ({message})\n",
            serde_json::to_value(kind).map(wire).unwrap_or_default()
        )),
        None => s.push_str("failure: none\n"),
    }
    s.push_str(&format!("herdr: {}\n", d.herdr_path.unwrap_or("not found")));
    s.push_str(&format!("floor: {HERDR_FLOOR}\n"));
    let skip = d.log_tail.len().saturating_sub(DETAILS_LOG_LINES);
    s.push_str("log:\n");
    for line in &d.log_tail[skip..] {
        s.push_str("  ");
        s.push_str(line);
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::fakes::FakeProc;
    use crate::deps::ProcError;

    #[test]
    fn manual_route_per_platform() {
        let r = manual_route(Some(Platform::WindowsX86_64), Some(ManualReason::Offline));
        assert_eq!(r.reason, Some(ManualReason::Offline));
        assert_eq!(r.commands.len(), 1);
        assert!(r.commands[0]
            .command
            .contains("irm https://herdr.dev/install.ps1 | iex"));
        assert_eq!(r.link, INSTALL_GUIDE_URL);
        let r = manual_route(Some(Platform::MacosAarch64), None);
        assert_eq!(
            r.commands[0].command,
            "curl -fsSL https://herdr.dev/install.sh | sh"
        );
        assert_eq!(manual_route(None, None).commands.len(), 2);
        let j = serde_json::to_value(manual_route(
            Some(Platform::MacosX86_64),
            Some(ManualReason::NoCompatibleRelease),
        ))
        .unwrap();
        assert_eq!(j["reason"], "no_compatible_release");
        assert_eq!(j["commands"][0]["shell"], "Terminal");
    }

    fn file() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("herdr.exe");
        std::fs::write(&p, b"bin").unwrap();
        (tmp, p)
    }

    #[test]
    fn browse_ok() {
        let (_tmp, p) = file();
        let proc_ = FakeProc::version("herdr 0.9.6");
        assert_eq!(
            validate_browse_path(&p, &proc_),
            Ok(semver::Version::new(0, 9, 6))
        );
        assert_eq!(proc_.calls()[0].0.program, p);
    }

    #[test]
    fn browse_too_old() {
        let (_tmp, p) = file();
        assert_eq!(
            validate_browse_path(&p, &FakeProc::version("herdr 0.9.3")),
            Err(BrowseError::TooOld {
                found: semver::Version::new(0, 9, 3)
            })
        );
    }

    #[test]
    fn browse_not_herdr() {
        let (_tmp, p) = file();
        for proc_ in [
            FakeProc::version("git version 2.45.1"),
            FakeProc::version("usage: tool"),
            FakeProc::new(|_| Err(ProcError::Spawn("not a valid application".into()))),
        ] {
            assert!(
                matches!(
                    validate_browse_path(&p, &proc_),
                    Err(BrowseError::NotHerdr(_))
                ),
                "{:?}",
                validate_browse_path(&p, &proc_)
            );
        }
    }

    #[test]
    fn browse_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let proc_ = FakeProc::version("herdr 0.9.6");
        assert_eq!(
            validate_browse_path(&tmp.path().join("nope"), &proc_),
            Err(BrowseError::NotFound)
        );
        // A directory is not a binary either.
        assert_eq!(
            validate_browse_path(tmp.path(), &proc_),
            Err(BrowseError::NotFound)
        );
        assert!(proc_.calls().is_empty());
    }

    #[test]
    fn details_report_contents() {
        let log: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        let r = details_report(&DetailsInput {
            app_version: "0.5.0",
            platform: Some(Platform::WindowsX86_64),
            tier: Some(Tier::Script),
            failure: Some((FailureKind::BlockedByAV, "would not run")),
            herdr_path: Some(r"C:\Users\x\.keitora\bin\herdr\0.9.6\herdr.exe"),
            log_tail: &log,
        });
        assert!(r.contains("keitora: 0.5.0\n"));
        assert!(r.contains("platform: windows-x86_64\n"));
        assert!(r.contains("tier: script\n"));
        assert!(r.contains("failure: blocked_by_av (would not run)\n"));
        assert!(r.contains(r"herdr: C:\Users\x\.keitora\bin\herdr\0.9.6\herdr.exe"));
        assert!(r.contains("floor: 0.9.5\n"));
        assert!(r.contains("  line 49\n"));
        assert!(r.contains("  line 10\n"));
        assert!(!r.contains("  line 9\n"), "only the last lines");

        // Nothing beyond the inputs: no home dir or other paths sneak in.
        let r = details_report(&DetailsInput {
            app_version: "0.5.0",
            platform: None,
            tier: None,
            failure: None,
            herdr_path: None,
            log_tail: &[],
        });
        assert_eq!(
            r,
            "Keitora herdr setup details\nkeitora: 0.5.0\nplatform: unsupported\ntier: none\n\
             failure: none\nherdr: not found\nfloor: 0.9.5\nlog:\n"
        );
    }
}
