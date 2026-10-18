//! `real` — the production [`HerdrOps`]: shells out to the `herdr` binary.
//!
//! Every control-plane op runs `herdr <group> <cmd> …` with the isolated env from
//! [`HerdrEnv`], captures stdout, and parses the single JSON object herdr prints.
//! Control commands wrap their payload as `{"id":…,"result":{…}}`; errors print
//! `{"error":{"code","message"},"id":…}` with a non-zero exit (both verified
//! against herdr 0.9.3). `herdr status server --json` is the one flat
//! (un-wrapped) shape. The pure parsers live in [`super::cli`].
//!
//! Also owns the two long-lived spawns (the detached server and the per-pane
//! control bridge) and locating the running server process by scan.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Mutex, RwLock};
use std::time::Instant;

use serde_json::Value;

use super::cli::{
    agent_start_result_from, api_error_from_value, parse_agent_list, parse_integration_status,
    parse_pane_identity, parse_pane_process_info, AgentInfo, IntegrationStatus, PaneIdentity,
    PaneInfo, PaneProcessInfo, ServerStatus,
};
use super::env::{discover_exe, ExeDiscovery, HerdrEnv, LoginSearch};
use super::exec::{run_with_deadline, CallClass};
use super::ops::{
    BridgeChild, BridgeProcess, ExitInfo, HerdrError, HerdrOps, ServerInstance, ServerProcess,
};

/// The production herdr client, bound to one resolved [`HerdrEnv`].
pub struct RealHerdr {
    env: HerdrEnv,
    /// The executable every herdr process is spawned from. Starts as
    /// `env.exe`; [`HerdrOps::rediscover`] replaces it (`env.exe` keeps the
    /// launch-time value).
    exe: RwLock<ChosenExe>,
    /// Cached herdr-server OS pid. Seeded by [`HerdrOps::spawn_server`] with the child
    /// we spawned; otherwise resolved by scanning for our herdr binary running
    /// `server` (e.g. the server survived an app restart). Re-validated cheaply on
    /// each lookup and re-scanned only if it no longer matches.
    pid_cache: Mutex<Option<u32>>,
    /// Process table used by [`HerdrOps::server_instance`].
    sys: Mutex<sysinfo::System>,
}

/// The herdr executable in use, and how it was chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ChosenExe {
    path: PathBuf,
    /// Whether the user named it (see [`ExeDiscovery::explicit`]).
    explicit: bool,
}

impl RealHerdr {
    pub fn new(env: HerdrEnv) -> Self {
        RealHerdr {
            exe: RwLock::new(ChosenExe {
                path: env.exe.clone(),
                explicit: env.discovery.explicit,
            }),
            env,
            pid_cache: Mutex::new(None),
            sys: Mutex::new(sysinfo::System::new()),
        }
    }

    /// The herdr executable in use now.
    fn exe(&self) -> PathBuf {
        self.exe.read().unwrap().path.clone()
    }

    /// How to recognise the running server's process as this herdr.
    fn server_exe(&self) -> ServerExe {
        let exe = self.exe.read().unwrap();
        ServerExe::resolve(&exe.path, exe.explicit)
    }

    /// Run `discover` and spawn from the executable it reports from now on (the
    /// server is then recognised the way that executable was chosen).
    fn rediscover_with(&self, discover: impl FnOnce() -> ExeDiscovery) -> ExeDiscovery {
        let discovery = discover();
        let chosen = ChosenExe {
            path: discovery.exe.clone(),
            explicit: discovery.explicit,
        };
        let mut exe = self.exe.write().unwrap();
        if *exe != chosen {
            log::info!(
                "herdr: executable is now {} (was {})",
                chosen.path.display(),
                exe.path.display()
            );
            *exe = chosen;
        }
        discovery
    }

    /// A `herdr <args>` command with the isolated env and non-detached spawn flags.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(self.exe());
        cmd.args(args);
        self.env.apply(&mut cmd);
        super::configure_command(&mut cmd);
        cmd
    }

    /// Run `herdr <args>` with the isolated env under `class`'s deadline; return
    /// the parsed JSON value. A `{"error":{…}}` body (even with a zero exit) is an
    /// [`HerdrError::Api`]; a non-zero exit with no error body is an
    /// [`HerdrError::Exit`]; an overrun is [`HerdrError::Timeout`].
    fn run(&self, class: CallClass, args: &[&str]) -> Result<Value, HerdrError> {
        let output = self.run_timed(class, args)?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        if !trimmed.is_empty() {
            if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                if let Some(err) = value.get("error") {
                    return Err(api_error_from_value(err));
                }
                return Ok(value);
            }
        }
        if output.status.success() {
            // Some commands (e.g. `server stop`) succeed with no stdout.
            return Ok(Value::Null);
        }
        Err(exit_error(output.status, &output.stderr))
    }

    /// Run a herdr command and return raw stdout. Used for commands like
    /// `pane read` that print rendered terminal content directly (NOT JSON), so
    /// [`Self::run`]'s JSON parse would mangle them.
    fn run_raw(&self, class: CallClass, args: &[&str]) -> Result<String, HerdrError> {
        let output = self.run_timed(class, args)?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(exit_error(output.status, &output.stderr))
        }
    }

    /// Run `herdr <args>` under `class`'s deadline and log how long it took,
    /// whether it succeeded or not, naming only its subcommand.
    fn run_timed(&self, class: CallClass, args: &[&str]) -> Result<Output, HerdrError> {
        let started = Instant::now();
        let output = run_with_deadline(self.command(args), class.deadline());
        log::debug!(
            "herdr cli {} took {} ms",
            subcommand(args),
            started.elapsed().as_millis()
        );
        output
    }
}

/// The subcommand of a herdr call for the timing log: at most its first two
/// arguments (`agent start`, `pane list`), never a value passed after them.
fn subcommand(args: &[&str]) -> String {
    args.iter().take(2).copied().collect::<Vec<_>>().join(" ")
}

/// The argv of `herdr pane process-info` for `pane_id`. herdr does not default
/// the leaf to the calling pane, so `--pane` is always passed.
fn pane_process_info_args(pane_id: &str) -> [&str; 4] {
    ["pane", "process-info", "--pane", pane_id]
}

fn exit_error(status: ExitStatus, stderr: &[u8]) -> HerdrError {
    HerdrError::Exit {
        code: status.code(),
        stderr: String::from_utf8_lossy(stderr).into_owned(),
    }
}

fn exit_info(status: ExitStatus) -> ExitInfo {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal = None;
    ExitInfo {
        code: status.code(),
        signal,
    }
}

/// The spawned `herdr server` child.
struct RealServerProcess(Child);

impl ServerProcess for RealServerProcess {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitInfo>> {
        Ok(self.0.try_wait()?.map(exit_info))
    }

    fn wait(&mut self) -> std::io::Result<ExitInfo> {
        self.0.wait().map(exit_info)
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.0.kill()
    }
}

/// The spawned `terminal session control` child.
struct RealBridgeChild(Child);

impl BridgeChild for RealBridgeChild {
    fn kill(&mut self) {
        let _ = self.0.kill();
    }

    fn wait(&mut self) {
        let _ = self.0.wait();
    }
}

impl HerdrOps for RealHerdr {
    fn env(&self) -> &HerdrEnv {
        &self.env
    }

    /// Reload the user config and search for herdr again, in full (see
    /// [`discover_exe`]): when nothing else has it, this runs the user's login
    /// shell, so it is never called on the setup thread. An unreadable config
    /// is searched as an empty one.
    fn rediscover(&self) -> ExeDiscovery {
        self.rediscover_with(|| {
            let pty = keitora_tasks::UserConfig::load()
                .map(|cfg| cfg.pty)
                .unwrap_or_else(|e| {
                    log::warn!("herdr: could not reload the user config to find herdr: {e}");
                    Default::default()
                });
            discover_exe(&pty, LoginSearch::Allowed)
        })
    }

    /// `herdr workspace create --cwd <cwd> [--env K=V …]` → the root pane identity.
    fn workspace_create(
        &self,
        cwd: &str,
        env: &[(String, String)],
    ) -> Result<PaneIdentity, HerdrError> {
        let mut args: Vec<String> = vec![
            "workspace".into(),
            "create".into(),
            "--cwd".into(),
            cwd.into(),
        ];
        for (k, v) in env {
            args.push("--env".into());
            args.push(format!("{k}={v}"));
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let value = self.run(CallClass::Create, &argv)?;
        parse_pane_identity(value.pointer("/result/root_pane").unwrap_or(&Value::Null)).ok_or_else(
            || HerdrError::Parse {
                message: "workspace create: missing root_pane".into(),
            },
        )
    }

    /// `herdr pane run <pane_id> <command…>` — types/runs a command in the pane.
    fn pane_run(&self, pane_id: &str, command: &[&str]) -> Result<(), HerdrError> {
        let mut args: Vec<&str> = vec!["pane", "run", pane_id];
        args.extend_from_slice(command);
        self.run(CallClass::Quick, &args).map(|_| ())
    }

    /// `herdr pane send-text <pane_id> <text>` — literal UTF-8, no trailing Enter.
    fn pane_send_text(&self, pane_id: &str, text: &str) -> Result<(), HerdrError> {
        self.run(CallClass::Quick, &["pane", "send-text", pane_id, text])
            .map(|_| ())
    }

    /// `herdr pane send-keys <pane_id> <key…>` — named keys.
    fn pane_send_keys(&self, pane_id: &str, keys: &[String]) -> Result<(), HerdrError> {
        let mut args: Vec<&str> = vec!["pane", "send-keys", pane_id];
        args.extend(keys.iter().map(String::as_str));
        self.run(CallClass::Quick, &args).map(|_| ())
    }

    /// `herdr pane close <pane_id>`. A `pane_not_found` error is folded to `Ok` by
    /// the caller (the pane is already gone, which is the desired post-state).
    fn pane_close(&self, pane_id: &str) -> Result<(), HerdrError> {
        self.run(CallClass::Quick, &["pane", "close", pane_id])
            .map(|_| ())
    }

    /// `herdr pane read <pane_id> --source <s> --format <f>` → rendered text/ANSI.
    ///
    /// Verified against herdr 0.9.3: `pane read` prints the rendered content to
    /// stdout as RAW bytes (NOT a JSON envelope), so this uses [`RealHerdr::run_raw`].
    /// Used by `attach` to paint the current screen synchronously (`--format ansi`
    /// `--source visible`), which avoids the async-first-frame race.
    fn pane_read(&self, pane_id: &str, source: &str, format: &str) -> Result<String, HerdrError> {
        self.run_raw(
            CallClass::Quick,
            &[
                "pane", "read", pane_id, "--source", source, "--format", format,
            ],
        )
    }

    /// `herdr pane list` → the herdr identities of all live panes.
    fn pane_list(&self) -> Result<Vec<PaneInfo>, HerdrError> {
        let value = self.run(CallClass::Quick, &["pane", "list"])?;
        let panes = value
            .pointer("/result/panes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(panes.iter().filter_map(parse_pane_identity).collect())
    }

    /// `herdr pane get <pane_id>` → that pane's identity (the same pane object as
    /// a `pane list` entry, at `/result/pane`).
    fn pane_get(&self, pane_id: &str) -> Result<Option<PaneInfo>, HerdrError> {
        let value = self.run(CallClass::Quick, &["pane", "get", pane_id])?;
        Ok(value.pointer("/result/pane").and_then(parse_pane_identity))
    }

    /// `herdr pane process-info --pane <pane_id>` → the pane's shell pid and
    /// foreground pids (at `/result/process_info`).
    fn pane_process_info(&self, pane_id: &str) -> Result<PaneProcessInfo, HerdrError> {
        let value = self.run(CallClass::Quick, &pane_process_info_args(pane_id))?;
        parse_pane_process_info(&value).ok_or_else(|| HerdrError::Parse {
            message: "pane process-info: unexpected shape".into(),
        })
    }

    /// `herdr agent start <name> --kind <kind> --pane <pane_id> --timeout <ms>
    /// [-- <args…>]`.
    ///
    /// Semantics (verified in the probe): an `agent_not_ready` result (or a
    /// "blocked during startup" message) means the agent LAUNCHED but is not yet
    /// ready — that is NOT a failure and returns `Ok(())`. A `*_not_found` code
    /// (e.g. `pane_not_found`) or a transport / non-zero-exit failure is a real
    /// `Err`, as is any other error code; the caller may re-check via `agent_list`.
    fn agent_start(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
        timeout_ms: u64,
    ) -> Result<(), HerdrError> {
        let timeout = timeout_ms.to_string();
        let mut argv: Vec<&str> = vec![
            "agent",
            "start",
            name,
            "--kind",
            kind,
            "--pane",
            pane_id,
            "--timeout",
            &timeout,
        ];
        if !args.is_empty() {
            argv.push("--");
            argv.extend(args.iter().map(String::as_str));
        }
        match self.run(CallClass::AgentStart, &argv) {
            Ok(_) => Ok(()),
            Err(err) => agent_start_result_from(err),
        }
    }

    /// `herdr agent prompt <pane_id> -- <text>` — fire-and-forget (no `--wait`).
    /// `agent_blocked` / `agent_not_ready` come back as [`HerdrError::Api`]; the
    /// backend maps them to "queue the line" via [`super::cli::agent_prompt_error_from`].
    fn agent_prompt(&self, pane_id: &str, text: &str) -> Result<(), HerdrError> {
        self.run(CallClass::Quick, &["agent", "prompt", pane_id, "--", text])
            .map(|_| ())
    }

    /// `herdr agent list` → every live agent (`/result/agents`). Errors propagate
    /// (as in `pane_list`); a missing `agents` array yields an empty Vec.
    fn agent_list(&self) -> Result<Vec<AgentInfo>, HerdrError> {
        let value = self.run(CallClass::Quick, &["agent", "list"])?;
        Ok(parse_agent_list(&value))
    }

    /// Status of one agent integration (`claude` / `codex`). READ-ONLY.
    ///
    /// Probed against herdr 0.9.3: `herdr integration status` takes NO target
    /// argument (`integration status claude` prints usage, exit 2) and prints plain
    /// text, one line per integration, `<name>: <state> (<hook path>)`, e.g.
    /// `claude: not installed (C:\Users\u\.claude\hooks\herdr-agent-state.ps1)`.
    /// Every integration was "not installed" on the probe machine, so the exact
    /// installed / outdated wording was NOT observed; [`parse_integration_status`]
    /// matches "installed" / "outdated" / "out of date" tolerantly and maps anything
    /// unrecognised to `Unknown` (which never triggers an install).
    fn integration_status(&self, target: &str) -> Result<IntegrationStatus, HerdrError> {
        let out = self.run_raw(CallClass::IntegrationQuery, &["integration", "status"])?;
        Ok(parse_integration_status(&out, target))
    }

    /// `herdr integration install <target>`. WRITES the user's real global agent
    /// config (`~/.claude`, `~/.codex`) and is not sandboxable by the isolated env;
    /// only `Supervisor::ensure_ready` may call it, behind the
    /// `KEITORA_HERDR_NO_INTEGRATION` opt-out. Never call from tests.
    fn integration_install(&self, target: &str) -> Result<(), HerdrError> {
        self.run_raw(
            CallClass::IntegrationInstall,
            &["integration", "install", target],
        )
        .map(|_| ())
    }

    /// `herdr status server --json` → the running-server status (flat shape).
    fn status_server(&self) -> Result<ServerStatus, HerdrError> {
        let value = self.run(CallClass::Status, &["status", "server", "--json"])?;
        Ok(ServerStatus {
            running: value
                .get("running")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            version: value
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_string),
            protocol: value.get("protocol").and_then(Value::as_u64),
            socket: value
                .get("socket")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    /// `herdr server stop`.
    fn server_stop(&self) -> Result<(), HerdrError> {
        self.run(CallClass::Stop, &["server", "stop"]).map(|_| ())
    }

    /// Spawn `herdr server` detached: null stdio, its own process group, job
    /// breakaway, and no console (`DETACHED_PROCESS`, so no hidden conhost is
    /// created alongside it).
    fn spawn_server(&self) -> std::io::Result<Box<dyn ServerProcess>> {
        let mut cmd = Command::new(self.exe());
        cmd.arg("server");
        self.env.apply(&mut cmd);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = crate::backend::launch::spawn_detached(
            &mut cmd,
            crate::backend::launch::LaunchProfile { breakaway: true },
        )?;
        // Seed the pid lookup with the child we know is our server.
        *self.pid_cache.lock().unwrap() = Some(child.id());
        Ok(Box::new(RealServerProcess(child)))
    }

    /// Spawn `herdr terminal session control <terminal_id> --takeover --cols N
    /// --rows N` with piped stdio.
    fn spawn_bridge(
        &self,
        terminal_id: &str,
        cols: u16,
        rows: u16,
    ) -> std::io::Result<BridgeProcess> {
        let mut cmd = Command::new(self.exe());
        cmd.args([
            "terminal",
            "session",
            "control",
            terminal_id,
            "--takeover",
            "--cols",
            &cols.to_string(),
            "--rows",
            &rows.to_string(),
        ]);
        self.env.apply(&mut cmd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        super::configure_command(&mut cmd);

        let mut child = cmd.spawn()?;
        let piped = |what: &str| std::io::Error::other(format!("herdr bridge: no {what}"));
        let stdin = child.stdin.take().ok_or_else(|| piped("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| piped("stdout"))?;
        let stderr = child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>);
        Ok(BridgeProcess {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr,
            child: Box::new(RealBridgeChild(child)),
        })
    }

    /// OS pid of the running herdr server, for the rail card's stats. Prefers the
    /// cached pid (seeded with the child we spawned), re-validated cheaply by
    /// refreshing just that one process; a full rescan runs only when the cache is
    /// empty or the cached process no longer matches (e.g. the server restarted).
    ///
    /// Returns `None` (card shows `—`) rather than guess when the scan is ambiguous —
    /// two `herdr server` processes share our binary (e.g. a real server plus an
    /// isolated-dev one), so there is no reliable way to tell which one backs the
    /// socket we attached to. Showing nothing is better than showing another
    /// server's CPU/mem.
    fn server_pid(&self, sys: &mut sysinfo::System) -> Option<u32> {
        // Canonicalize our own exe once, not per candidate process.
        let target = self.server_exe();
        let ident = sysinfo::ProcessRefreshKind::nothing()
            .with_cmd(sysinfo::UpdateKind::OnlyIfNotSet)
            .with_exe(sysinfo::UpdateKind::OnlyIfNotSet);

        // Hot path: re-validate the cached pid by refreshing just that one process.
        let cached = *self.pid_cache.lock().unwrap();
        if let Some(pid) = cached {
            let spid = sysinfo::Pid::from_u32(pid);
            sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[spid]), true, ident);
            if let Some(p) = sys.process(spid) {
                if target.matches(p.exe()) && is_server_cmd(&cmd_of(p)) {
                    return Some(pid);
                }
            }
            // Cache is stale (process gone or replaced): fall through to a full rescan.
        }

        // Cold path: scan every process for our `herdr server`. Take the single match; if
        // more than one matches we cannot attribute the stats, so leave them blank.
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, ident);
        let mut matched = sys
            .processes()
            .values()
            .filter(|p| target.matches(p.exe()) && is_server_cmd(&cmd_of(p)))
            .map(|p| p.pid().as_u32());
        let found = match (matched.next(), matched.next()) {
            (Some(pid), None) => Some(pid),
            (Some(_), Some(_)) => {
                log::warn!(
                    "herdr: multiple `herdr server` processes match our binary; cannot \
                     attribute daemon stats to one — leaving CPU/mem/uptime blank"
                );
                None
            }
            _ => {
                // The caller already confirmed the server is up, so an unresolved pid here is
                // usually an unreadable command line (another session/elevation). Logged so a
                // persistently-blank card is diagnosable rather than silent.
                log::debug!("herdr: server pid not resolved by scan; stats will be blank");
                None
            }
        };
        *self.pid_cache.lock().unwrap() = found;
        found
    }

    /// Pid + start time of the running server; `None` if the pid cannot be resolved
    /// or the process vanished.
    fn server_instance(&self) -> Option<ServerInstance> {
        let mut sys = self.sys.lock().unwrap();
        let pid = self.server_pid(&mut sys)?;
        let start_time = sys.process(sysinfo::Pid::from_u32(pid))?.start_time();
        Some(ServerInstance { pid, start_time })
    }
}

/// A precomputed "is this our herdr binary?" matcher. Resolving it once (canonicalizing our
/// own exe a single time) lets the cold-path process scan compare every candidate cheaply,
/// instead of re-canonicalizing our exe on every iteration.
#[derive(Debug, PartialEq, Eq)]
enum ServerExe {
    /// An absolute path the user named → compare canonical paths (so `.`/symlink
    /// spellings still match).
    Canon(std::path::PathBuf),
    /// A bare `PATH` name ("herdr") or a discovered path → compare
    /// case-insensitively by file stem.
    Stem(String),
}

impl ServerExe {
    /// `explicit`: the user named `our_exe` (config or `KEITORA_HERDR_BIN`). A
    /// discovered executable is matched by stem even when its path is absolute:
    /// the path can change under a server that keeps running (herdr updating
    /// itself into a new release directory, `brew upgrade` moving the Cellar
    /// target), and the server must still be recognised.
    fn resolve(our_exe: &Path, explicit: bool) -> Self {
        if explicit && our_exe.is_absolute() {
            ServerExe::Canon(
                std::fs::canonicalize(our_exe).unwrap_or_else(|_| our_exe.to_path_buf()),
            )
        } else {
            ServerExe::Stem(
                our_exe
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        }
    }

    /// Does `proc_exe` refer to the same herdr binary we drive? Pure given `self`.
    fn matches(&self, proc_exe: Option<&Path>) -> bool {
        let Some(proc_exe) = proc_exe else {
            return false;
        };
        match self {
            ServerExe::Canon(target) => {
                &std::fs::canonicalize(proc_exe).unwrap_or_else(|_| proc_exe.to_path_buf())
                    == target
            }
            ServerExe::Stem(stem) => proc_exe
                .file_stem()
                .map(|s| s.to_string_lossy().eq_ignore_ascii_case(stem))
                .unwrap_or(false),
        }
    }
}

/// Is this command line the detached `herdr server` (argv after the program is exactly
/// `server`), as opposed to a per-pane `terminal session control --takeover` bridge on the
/// same binary? Pure, so it is unit-testable. An empty/unreadable argv is not a match — we
/// would rather leave the stats blank than attribute them to a bridge.
fn is_server_cmd(cmd: &[String]) -> bool {
    let args: Vec<&str> = cmd.iter().skip(1).map(String::as_str).collect();
    args.first() == Some(&"server") && !args.contains(&"terminal")
}

/// Collect a process's argv as owned `String`s (sysinfo yields `OsString`s).
pub(crate) fn cmd_of(p: &sysinfo::Process) -> Vec<String> {
    p.cmd()
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_subcommand_names_at_most_two_arguments() {
        assert_eq!(
            subcommand(&["agent", "start", "secret-name", "--pane", "w1:p1"]),
            "agent start"
        );
        assert_eq!(subcommand(&["pane", "list"]), "pane list");
        assert_eq!(subcommand(&["server"]), "server");
        assert_eq!(subcommand(&[]), "");
    }

    #[test]
    fn pane_process_info_passes_the_pane_flag() {
        assert_eq!(
            pane_process_info_args("w1:p2"),
            ["pane", "process-info", "--pane", "w1:p2"]
        );
        // The timing log names the subcommand, never the pane.
        assert_eq!(
            subcommand(&pane_process_info_args("w1:p2")),
            "pane process-info"
        );
    }

    /// The server-pid scan command matcher: our detached `herdr server` matches, but the
    /// `terminal session control --takeover` bridges (same binary) and an empty/unreadable
    /// argv do not — so the rail card samples the server, never a bridge or a guess.
    #[test]
    fn is_server_cmd_matches_only_the_detached_server() {
        let argv = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // The real detached server (what `spawn_server` launches).
        assert!(is_server_cmd(&argv(&["herdr", "server"])));

        // A per-pane takeover bridge shares the binary but must NOT match.
        assert!(!is_server_cmd(&argv(&[
            "herdr",
            "terminal",
            "session",
            "control",
            "term_abc",
            "--takeover",
        ])));

        // No subcommand, or an empty/unreadable argv → no match (we leave stats blank).
        assert!(!is_server_cmd(&argv(&["herdr"])));
        assert!(!is_server_cmd(&[]));
    }

    /// A rediscovery swaps the executable every later spawn uses; the env the
    /// client was built from is left as it was.
    #[test]
    fn rediscover_replaces_the_executable_commands_are_spawned_from() {
        let tmp = tempfile::tempdir().unwrap();
        let env =
            HerdrEnv::with_exe_and_home(PathBuf::from("herdr"), tmp.path().join("h")).unwrap();
        let real = RealHerdr::new(env);
        assert_eq!(real.command(&["status"]).get_program(), "herdr");

        let found = ExeDiscovery {
            exe: PathBuf::from("/opt/tools/herdr"),
            searched: vec![PathBuf::from("/opt/tools/herdr")],
            notable: Vec::new(),
            found: true,
            explicit: false,
            login_deferred: false,
        };
        assert_eq!(real.rediscover_with(|| found.clone()), found);
        assert_eq!(real.exe(), PathBuf::from("/opt/tools/herdr"));
        assert_eq!(real.command(&["status"]).get_program(), "/opt/tools/herdr");
        assert_eq!(real.env().exe, PathBuf::from("herdr"));

        // Not found again: back to the bare name, so a `PATH` hit still works.
        let missing = ExeDiscovery {
            exe: PathBuf::from("herdr"),
            searched: vec![PathBuf::from("/opt/tools/herdr")],
            notable: Vec::new(),
            found: false,
            explicit: false,
            login_deferred: false,
        };
        assert!(!real.rediscover_with(|| missing).found);
        assert_eq!(real.command(&["status"]).get_program(), "herdr");
    }

    /// How the server is recognised follows how the executable in use was
    /// chosen, across a rediscovery in either direction.
    #[test]
    fn rediscover_swaps_how_the_server_is_recognised() {
        #[cfg(windows)]
        let (named, discovered) = (r"C:\cfg\herdr.exe", r"C:\rel\0.9.3\herdr.exe");
        #[cfg(not(windows))]
        let (named, discovered) = ("/nonexistent/cfg/herdr", "/nonexistent/rel/0.9.3/herdr");
        let tmp = tempfile::tempdir().unwrap();
        let env = HerdrEnv::with_exe_and_home(PathBuf::from(named), tmp.path().join("h")).unwrap();
        let real = RealHerdr::new(env);
        // Named by the user: that exact file.
        assert_eq!(real.server_exe(), ServerExe::Canon(PathBuf::from(named)));

        let mut d = ExeDiscovery::explicit(PathBuf::from(discovered));
        d.explicit = false;
        real.rediscover_with(|| d);
        assert_eq!(real.server_exe(), ServerExe::Stem("herdr".into()));

        real.rediscover_with(|| ExeDiscovery::explicit(PathBuf::from(named)));
        assert_eq!(real.server_exe(), ServerExe::Canon(PathBuf::from(named)));
    }

    /// `ServerExe` compares by canonical path for an absolute target the user named and by
    /// case-insensitive file stem for a bare `PATH` name ("herdr"), after resolving our own
    /// exe just once.
    #[test]
    fn server_exe_matches_absolute_and_bare_name() {
        // Bare-name target: match on stem, case-insensitively, ignoring directory.
        let bare = ServerExe::resolve(Path::new("herdr"), false);
        // A bare name the user gave is still only a name.
        assert_eq!(ServerExe::resolve(Path::new("herdr"), true), bare);
        assert!(bare.matches(Some(Path::new("/usr/bin/herdr"))));
        assert!(bare.matches(Some(Path::new("/usr/bin/HERDR"))));
        assert!(!bare.matches(Some(Path::new("/usr/bin/notherdr"))));
        assert!(!bare.matches(None));
        #[cfg(windows)]
        {
            assert!(bare.matches(Some(Path::new(r"C:\anywhere\HERDR.exe"))));
            assert!(!bare.matches(Some(Path::new(r"C:\x\notherdr.exe"))));
        }

        // Absolute target that does not exist on disk: canonicalize falls back to the raw
        // path, so an exact-string match still holds and a different path does not.
        #[cfg(windows)]
        let (abs_path, same, other) = (r"C:\h\herdr.exe", r"C:\h\herdr.exe", r"C:\h\other.exe");
        #[cfg(not(windows))]
        let (abs_path, same, other) = (
            "/nonexistent/h/herdr",
            "/nonexistent/h/herdr",
            "/nonexistent/h/other",
        );
        let abs = ServerExe::resolve(Path::new(abs_path), true);
        assert!(abs.matches(Some(Path::new(same))));
        assert!(!abs.matches(Some(Path::new(other))));
    }

    /// A discovered executable is recognised by name, not by where it was found:
    /// the server keeps running while its binary moves (a self-update into a new
    /// release directory, `brew upgrade`).
    #[test]
    fn server_exe_matches_a_discovered_path_by_stem() {
        #[cfg(windows)]
        let (found_at, running, other) = (
            r"C:\Users\u\.herdr\packages\standalone\releases\0.9.4-x\herdr.exe",
            r"C:\Users\u\.herdr\packages\standalone\releases\0.9.3-x\herdr.exe",
            r"C:\Users\u\bin\other.exe",
        );
        #[cfg(not(windows))]
        let (found_at, running, other) = (
            "/opt/homebrew/Cellar/herdr/0.9.4/bin/herdr",
            "/opt/homebrew/Cellar/herdr/0.9.3/bin/herdr",
            "/opt/homebrew/bin/other",
        );
        let discovered = ServerExe::resolve(Path::new(found_at), false);
        assert_eq!(discovered, ServerExe::Stem("herdr".into()));
        assert!(discovered.matches(Some(Path::new(running))));
        assert!(discovered.matches(Some(Path::new(found_at))));
        assert!(!discovered.matches(Some(Path::new(other))));
        // The same path named by the user is that exact file only.
        let named = ServerExe::resolve(Path::new(found_at), true);
        assert!(named.matches(Some(Path::new(found_at))));
        assert!(!named.matches(Some(Path::new(running))));
    }
}
