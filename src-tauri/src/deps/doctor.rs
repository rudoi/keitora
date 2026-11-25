//! The doctor: one row per external tool Keitora shells out to (herdr, git, claude,
//! codex), with where it is, its version, and what the UI can offer. Installs nothing.

use std::time::Duration;

use keitora_tasks::CliKind;
use serde::Serialize;

use super::floor::{FloorState, HERDR_FLOOR};
use super::service::Inner;
use super::{ProcError, ProcSpec};

/// Bound on `git --version`.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Ok,
    Missing,
    TooOld,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorAction {
    Install,
    Upgrade,
    Browse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorRow {
    /// `"herdr"`, `"git"`, `"claude"` or `"codex"`.
    pub tool: String,
    pub path: Option<String>,
    pub version: Option<String>,
    /// The minimum version (herdr only; the other rows are informational).
    pub floor: Option<String>,
    pub status: DoctorStatus,
    pub action: Option<DoctorAction>,
}

/// The version in `git --version` output (`git version 2.43.0.windows.1` -> `2.43.0`).
pub fn parse_git_version(out: &str) -> Option<String> {
    let rest = out.trim().strip_prefix("git version")?.trim();
    let token = rest.split_whitespace().next()?;
    let numeric: Vec<&str> = token
        .split('.')
        .take_while(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .take(3)
        .collect();
    (!numeric.is_empty()).then(|| numeric.join("."))
}

fn herdr_row(inner: &Inner) -> DoctorRow {
    inner.refresh_machine();
    let machine = inner.lock().machine.clone();
    let floor = Some(HERDR_FLOOR.to_string());
    let Some(path) = machine.current else {
        return DoctorRow {
            tool: "herdr".into(),
            path: None,
            version: None,
            floor,
            status: DoctorStatus::Missing,
            action: Some(DoctorAction::Install),
        };
    };
    let version = machine.current_version.as_ref().map(|v| v.to_string());
    // The running server is authoritative; the binary decides only without one.
    let (status, action) = match inner.floor.get() {
        FloorState::Ok => (DoctorStatus::Ok, None),
        FloorState::TooOld { .. } | FloorState::ProtocolMismatch { .. } => {
            (DoctorStatus::TooOld, Some(DoctorAction::Upgrade))
        }
        FloorState::RestartToFinish { .. } | FloorState::Unknown => {
            match &machine.current_version {
                Some(v) if *v >= HERDR_FLOOR => (DoctorStatus::Ok, None),
                Some(_) => (DoctorStatus::TooOld, Some(DoctorAction::Upgrade)),
                None => (DoctorStatus::Unknown, Some(DoctorAction::Browse)),
            }
        }
    };
    DoctorRow {
        tool: "herdr".into(),
        path: Some(path.display().to_string()),
        version,
        floor,
        status,
        action,
    }
}

fn git_row(inner: &Inner) -> DoctorRow {
    let result = inner.proc_.run(
        &ProcSpec::new("git", &["--version"]),
        GIT_TIMEOUT,
        &mut |_| {},
    );
    let (status, version) = match result {
        Err(ProcError::NotFound) => (DoctorStatus::Missing, None),
        Err(_) => (DoctorStatus::Unknown, None),
        Ok(out) if !out.success() => (DoctorStatus::Unknown, None),
        Ok(out) => match parse_git_version(&out.stdout) {
            Some(v) => (DoctorStatus::Ok, Some(v)),
            None => (DoctorStatus::Unknown, None),
        },
    };
    DoctorRow {
        tool: "git".into(),
        path: None,
        version,
        floor: None,
        status,
        action: None,
    }
}

fn cli_row(inner: &Inner, cli: CliKind, cfg: &keitora_tasks::UserConfig) -> DoctorRow {
    let prober = &inner.ctx.prober;
    let tool = match cli {
        CliKind::Claude => "claude",
        CliKind::Codex => "codex",
    };
    let Some(exe) = prober.exe(cli, cfg) else {
        return DoctorRow {
            tool: tool.into(),
            path: None,
            version: None,
            floor: None,
            status: DoctorStatus::Missing,
            action: None,
        };
    };
    let version = prober.installed(cli, &exe);
    DoctorRow {
        tool: tool.into(),
        status: if version.is_some() {
            DoctorStatus::Ok
        } else {
            DoctorStatus::Unknown
        },
        path: Some(exe),
        version,
        floor: None,
        action: None,
    }
}

/// The doctor rows, in display order. The probes are independent, so they run at once.
pub(super) fn rows(inner: &Inner) -> Vec<DoctorRow> {
    let cfg = inner.ctx.prober.config();
    let cfg = &cfg;
    std::thread::scope(|s| {
        let probes = [
            s.spawn(|| herdr_row(inner)),
            s.spawn(|| git_row(inner)),
            s.spawn(|| cli_row(inner, CliKind::Claude, cfg)),
            s.spawn(|| cli_row(inner, CliKind::Codex, cfg)),
        ];
        probes
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    })
}
