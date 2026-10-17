//! `ops` — the seam between the herdr backend and the `herdr` binary.
//!
//! Every way the backend talks to herdr (control-plane CLI calls, the detached
//! server spawn, the per-pane bridge spawn, and locating the server process) goes
//! through the object-safe [`HerdrOps`] trait. Production uses
//! [`super::real::RealHerdr`]; tests substitute a scripted in-memory double, so no
//! test ever needs a real herdr.
//!
//! Control calls return the typed [`HerdrError`]; [`From<HerdrError>`] maps it to
//! the backend's [`PtyBackendError`] with the same variants and messages the
//! backend has always reported.

use serde::{Deserialize, Serialize};

use crate::backend::PtyBackendError;

use super::cli::{map_error, AgentInfo, IntegrationStatus, PaneIdentity, PaneInfo, ServerStatus};
use super::env::{ExeDiscovery, HerdrEnv};

/// A failed herdr invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HerdrError {
    /// Deadline expired (the child was killed).
    Timeout,
    /// The `herdr` process could not be created. `os_error` is
    /// `io::Error::raw_os_error()` (a Win32 code on Windows, an errno on unix);
    /// `message` is the `io::Error` text.
    SpawnFailed {
        os_error: Option<i32>,
        message: String,
    },
    /// The process ran, exited non-zero, and printed no `{"error":..}` body.
    Exit { code: Option<i32>, stderr: String },
    /// stdout was not the expected shape. The lenient parsers treat a missing
    /// array as empty, so this is only produced for a missing required object.
    Parse { message: String },
    /// herdr answered with an `{"error":{"code","message"}}` body (even with a
    /// zero exit).
    Api { code: String, message: String },
}

impl HerdrError {
    /// herdr's error code, for an [`HerdrError::Api`] error.
    pub fn code(&self) -> Option<&str> {
        match self {
            HerdrError::Api { code, .. } => Some(code),
            _ => None,
        }
    }

    /// Whether herdr reported the target (pane, workspace, agent, ...) as gone.
    pub fn is_not_found(&self) -> bool {
        matches!(self, HerdrError::Api { code, .. } if code.ends_with("_not_found"))
    }

    /// Whether the machine could not run the CLI (out of memory / commit / quota).
    /// This is evidence about the machine, never about whether the server is up.
    ///
    /// Two shapes: the process could not be created at all (`SpawnFailed` with a
    /// Win32 error from `CreateProcessW`, or `EAGAIN`/`ENOMEM` on unix), or it was
    /// created and then failed to initialise, which surfaces as an `Exit` whose code
    /// is an NTSTATUS such as `STATUS_DLL_INIT_FAILED`.
    pub fn is_resource_pressure(&self) -> bool {
        // ERROR_NOT_ENOUGH_MEMORY, ERROR_OUTOFMEMORY, ERROR_NO_SYSTEM_RESOURCES,
        // ERROR_PAGEFILE_QUOTA, ERROR_COMMITMENT_LIMIT, ERROR_NOT_ENOUGH_QUOTA.
        #[cfg(windows)]
        const SPAWN_PRESSURE: &[i32] = &[8, 14, 1450, 1454, 1455, 1816];
        // EAGAIN, ENOMEM.
        #[cfg(not(windows))]
        const SPAWN_PRESSURE: &[i32] = &[11, 12];
        // STATUS_DLL_INIT_FAILED, STATUS_COMMITMENT_LIMIT, STATUS_NO_MEMORY,
        // STATUS_INSUFFICIENT_RESOURCES.
        const EXIT_PRESSURE: &[u32] = &[0xC000_0142, 0xC000_012D, 0xC000_0017, 0xC000_009A];
        match self {
            HerdrError::SpawnFailed {
                os_error: Some(os), ..
            } => SPAWN_PRESSURE.contains(os),
            HerdrError::Exit { code: Some(c), .. } => EXIT_PRESSURE.contains(&(*c as u32)),
            _ => false,
        }
    }

    /// Whether the `herdr` executable itself could not be found when spawning it
    /// (`ENOENT`; on Windows `ERROR_FILE_NOT_FOUND` or `ERROR_PATH_NOT_FOUND`).
    /// Like resource pressure, this is evidence about the machine, never about
    /// whether the server is up.
    pub fn is_exe_missing(&self) -> bool {
        #[cfg(windows)]
        const MISSING: &[i32] = &[2, 3];
        #[cfg(not(windows))]
        const MISSING: &[i32] = &[2];
        matches!(
            self,
            HerdrError::SpawnFailed { os_error: Some(os), .. } if MISSING.contains(os)
        )
    }
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HerdrError::Timeout => write!(f, "herdr call timed out"),
            HerdrError::SpawnFailed { os_error, message } => match os_error {
                Some(os) => write!(f, "herdr spawn failed (os error {os}): {message}"),
                None => write!(f, "herdr spawn failed: {message}"),
            },
            HerdrError::Exit { code, stderr } => match code {
                Some(c) => write!(f, "herdr exited with exit code {c}: {}", stderr.trim()),
                None => write!(f, "herdr exited with unknown status: {}", stderr.trim()),
            },
            HerdrError::Parse { message } => write!(f, "herdr: {message}"),
            HerdrError::Api { code, message } => write!(f, "herdr error {code}: {message}"),
        }
    }
}

impl std::error::Error for HerdrError {}

impl From<HerdrError> for PtyBackendError {
    fn from(e: HerdrError) -> Self {
        match e {
            HerdrError::Api { code, message } => map_error(&code, &message),
            HerdrError::SpawnFailed { message, .. } => {
                PtyBackendError::Other(format!("herdr spawn failed: {message}"))
            }
            HerdrError::Exit { code, stderr } => {
                let status = match code {
                    Some(c) => format!("exit code: {c}"),
                    None => "unknown status".to_string(),
                };
                PtyBackendError::Other(format!("herdr exited with {status}: {}", stderr.trim()))
            }
            // A call that ran out of time looks like an unreachable server to
            // callers, which already retry or queue on `NotConnected`.
            HerdrError::Timeout => PtyBackendError::NotConnected,
            HerdrError::Parse { message } => PtyBackendError::Other(format!("herdr: {message}")),
        }
    }
}

/// Everything the backend needs from herdr. Object-safe; shared across threads as
/// `Arc<dyn HerdrOps>`.
pub trait HerdrOps: Send + Sync {
    /// The resolved exe + isolated env + home.
    fn env(&self) -> &HerdrEnv;

    /// Search for the herdr executable again and use what is found from now on
    /// (it may have been installed, or configured, since launch). The default
    /// has nothing to search for and reports the current executable as found.
    fn rediscover(&self) -> ExeDiscovery {
        ExeDiscovery::explicit(self.env().exe.clone())
    }

    // Control plane: each is one `herdr ...` invocation.
    fn workspace_create(
        &self,
        cwd: &str,
        env: &[(String, String)],
    ) -> Result<PaneIdentity, HerdrError>;
    fn pane_run(&self, pane_id: &str, command: &[&str]) -> Result<(), HerdrError>;
    fn pane_send_text(&self, pane_id: &str, text: &str) -> Result<(), HerdrError>;
    fn pane_send_keys(&self, pane_id: &str, keys: &[String]) -> Result<(), HerdrError>;
    fn pane_close(&self, pane_id: &str) -> Result<(), HerdrError>;
    fn pane_read(&self, pane_id: &str, source: &str, format: &str) -> Result<String, HerdrError>;
    fn pane_list(&self) -> Result<Vec<PaneInfo>, HerdrError>;
    /// One pane's identity (`pane get`); `None` when herdr's answer has no pane.
    fn pane_get(&self, pane_id: &str) -> Result<Option<PaneInfo>, HerdrError>;
    /// `agent_not_ready` / "blocked during startup" are folded to `Ok` (the agent
    /// launched but is not ready yet).
    fn agent_start(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
        timeout_ms: u64,
    ) -> Result<(), HerdrError>;
    fn agent_prompt(&self, pane_id: &str, text: &str) -> Result<(), HerdrError>;
    // See `cli::AgentInfo`: no production caller lists agents now.
    #[cfg_attr(not(test), allow(dead_code))]
    fn agent_list(&self) -> Result<Vec<AgentInfo>, HerdrError>;
    fn integration_status(&self, target: &str) -> Result<IntegrationStatus, HerdrError>;
    fn integration_install(&self, target: &str) -> Result<(), HerdrError>;
    fn status_server(&self) -> Result<ServerStatus, HerdrError>;
    fn server_stop(&self) -> Result<(), HerdrError>;

    // Processes.
    /// Spawn the detached `herdr server`.
    fn spawn_server(&self) -> std::io::Result<Box<dyn ServerProcess>>;
    /// Spawn the per-pane `terminal session control` bridge.
    fn spawn_bridge(
        &self,
        terminal_id: &str,
        cols: u16,
        rows: u16,
    ) -> std::io::Result<BridgeProcess>;

    // OS-level identity.
    /// The running server's OS pid (cached scan), for stats sampling.
    fn server_pid(&self, sys: &mut sysinfo::System) -> Option<u32>;
    /// Pid + start time of the running server; changes whenever the server
    /// process is replaced.
    fn server_instance(&self) -> Option<ServerInstance>;
}

/// How a process exited. `code` is `ExitStatus::code()` (on Windows an NTSTATUS
/// comes back as a negative i32); `signal` is the unix signal, always `None` on
/// Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// Handle to the spawned `herdr server`. Whoever blocks in [`Self::wait`] owns the
/// box, so other threads stop a live server by pid; [`Self::kill`] is for cleanup
/// while the starter still holds the box.
pub trait ServerProcess: Send {
    fn pid(&self) -> u32;
    /// Non-blocking; `Some` once the process has exited.
    fn try_wait(&mut self) -> std::io::Result<Option<ExitInfo>>;
    /// Blocks until the process exits.
    fn wait(&mut self) -> std::io::Result<ExitInfo>;
    /// Best-effort terminate.
    fn kill(&mut self) -> std::io::Result<()>;
}

/// The bridge child process (or its in-memory stand-in).
pub trait BridgeChild: Send {
    fn kill(&mut self);
    fn wait(&mut self);
}

/// The pipes and child of a spawned bridge.
pub struct BridgeProcess {
    pub stdin: Box<dyn std::io::Write + Send>,
    pub stdout: Box<dyn std::io::Read + Send>,
    pub stderr: Option<Box<dyn std::io::Read + Send>>,
    pub child: Box<dyn BridgeChild>,
}

/// Identity of one running herdr server process. `start_time` is seconds since
/// the epoch (as sysinfo reports it); 0 when it could not be read (and a pid of
/// 0 means nothing is known).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInstance {
    pub pid: u32,
    pub start_time: u64,
}

impl ServerInstance {
    /// Whether `other` is the same server process, as far as can be told:
    /// `None` when either side is unknown (pid 0); by pid alone when either
    /// start time is unknown (a pid never changes for one process, so a
    /// different pid is always another server).
    pub fn same_process(&self, other: &ServerInstance) -> Option<bool> {
        if self.pid == 0 || other.pid == 0 {
            return None;
        }
        if self.start_time == 0 || other.start_time == 0 {
            return Some(self.pid == other.pid);
        }
        Some(self == other)
    }
}

/// Monotonic count of server instances the keymap has seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct Generation(pub u64);

impl Generation {
    pub fn next(self) -> Self {
        Generation(self.0 + 1)
    }
}

/// The name of one [`HerdrOps`] operation; the test double records and scripts
/// calls by it.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    WorkspaceCreate,
    PaneRun,
    PaneSendText,
    PaneSendKeys,
    PaneClose,
    PaneRead,
    PaneList,
    PaneGet,
    AgentStart,
    AgentPrompt,
    AgentList,
    IntegrationStatus,
    IntegrationInstall,
    StatusServer,
    ServerStop,
    SpawnServer,
    SpawnBridge,
    ServerPid,
    ServerInstance,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(code: &str, message: &str) -> HerdrError {
        HerdrError::Api {
            code: code.into(),
            message: message.into(),
        }
    }

    fn other(e: HerdrError) -> String {
        match PtyBackendError::from(e) {
            PtyBackendError::Other(m) => m,
            v => panic!("expected Other, got {v:?}"),
        }
    }

    #[test]
    fn from_herdr_error_preserves_messages() {
        assert!(matches!(
            PtyBackendError::from(api("pane_not_found", "pane w9:p9 not found")),
            PtyBackendError::NoSuchSession
        ));
        assert!(matches!(
            PtyBackendError::from(api("workspace_not_found", "nope")),
            PtyBackendError::NoSuchSession
        ));
        assert_eq!(
            other(api("invalid_key", "unsupported key ctrl-c")),
            "herdr: unsupported key ctrl-c"
        );
        assert_eq!(
            other(HerdrError::SpawnFailed {
                os_error: Some(1455),
                message: "paging file too small".into(),
            }),
            "herdr spawn failed: paging file too small"
        );
        assert_eq!(
            other(HerdrError::Exit {
                code: Some(2),
                stderr: "  bad args\n".into(),
            }),
            "herdr exited with exit code: 2: bad args"
        );
        assert_eq!(
            other(HerdrError::Exit {
                code: None,
                stderr: "killed".into(),
            }),
            "herdr exited with unknown status: killed"
        );
        assert!(matches!(
            PtyBackendError::from(HerdrError::Timeout),
            PtyBackendError::NotConnected
        ));
        assert_eq!(
            other(HerdrError::Parse {
                message: "workspace create: missing root_pane".into(),
            }),
            "herdr: workspace create: missing root_pane"
        );
    }

    #[test]
    fn herdr_error_code_and_is_not_found() {
        let e = api("pane_not_found", "gone");
        assert_eq!(e.code(), Some("pane_not_found"));
        assert!(e.is_not_found());

        let e = api("agent_start_failed", "exited");
        assert_eq!(e.code(), Some("agent_start_failed"));
        assert!(!e.is_not_found());

        for e in [
            HerdrError::Timeout,
            HerdrError::SpawnFailed {
                os_error: None,
                message: "x".into(),
            },
            HerdrError::Exit {
                code: Some(1),
                stderr: "pane_not_found".into(),
            },
            HerdrError::Parse {
                message: "x".into(),
            },
        ] {
            assert_eq!(e.code(), None);
            assert!(!e.is_not_found(), "{e:?} is not an api not-found");
        }
    }

    #[test]
    fn is_resource_pressure_table() {
        let spawn = |os| HerdrError::SpawnFailed {
            os_error: Some(os),
            message: "x".into(),
        };
        let exit = |c: u32| HerdrError::Exit {
            code: Some(c as i32),
            stderr: String::new(),
        };
        #[cfg(windows)]
        {
            for os in [8, 14, 1450, 1454, 1455, 1816] {
                assert!(spawn(os).is_resource_pressure(), "os error {os}");
            }
        }
        #[cfg(not(windows))]
        {
            assert!(spawn(11).is_resource_pressure());
            assert!(spawn(12).is_resource_pressure());
        }
        // File not found / access denied are ordinary spawn failures.
        assert!(!spawn(2).is_resource_pressure());
        assert!(!spawn(5).is_resource_pressure());
        assert!(!HerdrError::SpawnFailed {
            os_error: None,
            message: "x".into(),
        }
        .is_resource_pressure());
        for c in [0xC000_0142, 0xC000_012D, 0xC000_0017, 0xC000_009A] {
            assert!(exit(c).is_resource_pressure(), "exit {c:#X}");
        }
        assert!(!exit(1).is_resource_pressure());
        assert!(!exit(0xC000_0409).is_resource_pressure());
        assert!(!HerdrError::Exit {
            code: None,
            stderr: String::new(),
        }
        .is_resource_pressure());
        assert!(!HerdrError::Timeout.is_resource_pressure());
        assert!(!HerdrError::Parse {
            message: "x".into(),
        }
        .is_resource_pressure());
        assert!(!api("server_busy", "x").is_resource_pressure());
    }

    #[test]
    fn is_exe_missing_table() {
        let spawn = |os| HerdrError::SpawnFailed {
            os_error: os,
            message: "x".into(),
        };
        // ENOENT / ERROR_FILE_NOT_FOUND.
        assert!(spawn(Some(2)).is_exe_missing());
        // ERROR_PATH_NOT_FOUND on Windows; ESRCH elsewhere, which is not it.
        assert_eq!(spawn(Some(3)).is_exe_missing(), cfg!(windows));
        // Access denied, out of memory, no code at all.
        for os in [Some(5), Some(12), Some(13), Some(1455), None] {
            assert!(!spawn(os).is_exe_missing(), "os error {os:?}");
        }
        // herdr ran: whatever it said, the executable exists.
        assert!(!HerdrError::Exit {
            code: Some(2),
            stderr: "No such file or directory".into(),
        }
        .is_exe_missing());
        assert!(!HerdrError::Timeout.is_exe_missing());
        assert!(!HerdrError::Parse {
            message: "x".into(),
        }
        .is_exe_missing());
        assert!(!api("pane_not_found", "x").is_exe_missing());
        // The two machine-evidence classes do not overlap.
        assert!(!spawn(Some(2)).is_resource_pressure());
    }

    #[test]
    fn herdr_error_display() {
        assert_eq!(HerdrError::Timeout.to_string(), "herdr call timed out");
        assert_eq!(
            HerdrError::SpawnFailed {
                os_error: Some(1455),
                message: "paging file too small".into(),
            }
            .to_string(),
            "herdr spawn failed (os error 1455): paging file too small"
        );
        assert_eq!(
            HerdrError::Exit {
                code: Some(1),
                stderr: " boom 
"
                .into(),
            }
            .to_string(),
            "herdr exited with exit code 1: boom"
        );
        assert_eq!(
            HerdrError::Parse {
                message: "bad".into(),
            }
            .to_string(),
            "herdr: bad"
        );
        assert_eq!(
            api("pane_not_found", "gone").to_string(),
            "herdr error pane_not_found: gone"
        );
        let boxed: Box<dyn std::error::Error> = Box::new(HerdrError::Timeout);
        assert_eq!(boxed.to_string(), "herdr call timed out");
    }

    #[test]
    fn generation_next_increments() {
        assert_eq!(Generation::default(), Generation(0));
        assert_eq!(Generation(4).next(), Generation(5));
        assert!(Generation(1) < Generation(2));
    }

    #[test]
    fn same_process_compares_as_far_as_the_identities_go() {
        let i = |pid, start_time| ServerInstance { pid, start_time };
        assert_eq!(i(5, 100).same_process(&i(5, 100)), Some(true));
        assert_eq!(
            i(5, 100).same_process(&i(5, 200)),
            Some(false),
            "pid reused"
        );
        assert_eq!(i(5, 100).same_process(&i(6, 100)), Some(false));
        assert_eq!(i(5, 0).same_process(&i(5, 100)), Some(true), "pid only");
        assert_eq!(i(5, 100).same_process(&i(6, 0)), Some(false));
        assert_eq!(i(0, 0).same_process(&i(5, 100)), None, "unknown");
        assert_eq!(i(5, 100).same_process(&i(0, 0)), None);
    }

    #[test]
    fn server_instance_and_generation_round_trip_json() {
        let i = ServerInstance {
            pid: 12345,
            start_time: 1_759_700_000,
        };
        let s = serde_json::to_string(&i).unwrap();
        assert_eq!(s, r#"{"pid":12345,"start_time":1759700000}"#);
        assert_eq!(serde_json::from_str::<ServerInstance>(&s).unwrap(), i);
        let g: Generation = serde_json::from_str("7").unwrap();
        assert_eq!(g, Generation(7));
    }
}
