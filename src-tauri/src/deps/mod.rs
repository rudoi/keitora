//! External dependency setup: finding, judging, installing and upgrading herdr, and
//! reporting on the other tools Keitora shells out to.
//!
//! The pure decisions live in [`floor`], [`manifest`] and [`preflight`]; the installers
//! in [`tier1`] (vendor script), [`tier2`] (direct download) and [`manual`]; the managed
//! [`DepsService`] (snapshot, prompt, install mutex) in [`service`], the run sequence in
//! [`upgrade`], and the tool report in [`doctor`]. This module holds the shared run
//! vocabulary (stages, failure kinds), failure routing, the small [`Proc`] / [`Fs`]
//! traits the installers run through so tests can fake them, and app startup.

pub mod commands;
pub mod doctor;
pub mod floor;
pub mod manifest;
pub mod manual;
pub mod preflight;
pub mod service;
#[cfg(test)]
mod service_tests;
pub mod tier1;
pub mod tier2;
mod upgrade;

use std::fmt;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::Serialize;

use manifest::NetError;
pub use preflight::Tier;
pub use service::{
    DepsContext, DepsService, DepsSink, DepsSnapshot, HerdrConfig, ProgressEvent, PROGRESS_EVENT,
    STATUS_EVENT,
};

/// Progress stages of one install run. Forward-only: a run never moves to an earlier
/// stage, and ends in exactly one of `Done` / `Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Checking,
    Downloading,
    Installing,
    Verifying,
    Restarting,
    Done,
    Failed,
}

impl Stage {
    pub fn is_terminal(self) -> bool {
        matches!(self, Stage::Done | Stage::Failed)
    }
}

/// Why an install run failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Offline,
    /// Any other network failure, including TLS interception and proxies.
    Network,
    /// The script host is locked down (Constrained Language Mode, AppLocker).
    PolicyBlocked,
    Checksum,
    /// The downloaded binary would not run, typically quarantined by antivirus.
    #[serde(rename = "blocked_by_av")]
    BlockedByAV,
    Timeout,
    /// The installer reported success but no usable herdr was found afterwards.
    Incomplete,
    /// The install finished but the resulting herdr is still below the floor.
    TooOldAfterInstall,
    NoCompatibleRelease,
    Other,
}

/// What the UI offers after a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NextStep {
    /// Offer "Try direct download".
    TryDirect,
    /// Show manual instructions and the Browse picker.
    Manual,
}

/// After a failed run on `tier`: a failed script run falls back to a direct download
/// when one is viable and could plausibly succeed (not offline, and a compatible
/// release exists); everything else goes to manual.
pub fn after_failure(kind: FailureKind, tier: Tier, direct_viable: bool) -> NextStep {
    let direct_could_help = !matches!(
        kind,
        FailureKind::Offline | FailureKind::NoCompatibleRelease
    );
    if tier == Tier::Script && direct_viable && direct_could_help {
        NextStep::TryDirect
    } else {
        NextStep::Manual
    }
}

/// A failed install, classified for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallFailure {
    pub kind: FailureKind,
    pub message: String,
}

impl InstallFailure {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        InstallFailure {
            kind,
            message: message.into(),
        }
    }
}

/// A successful install: the herdr executable to pin and the version it reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub path: PathBuf,
    pub version: semver::Version,
}

impl From<&NetError> for FailureKind {
    /// Offline stays offline; TLS/proxy, HTTP and refused hosts are network failures; a
    /// local write failure is not the network's fault.
    fn from(e: &NetError) -> Self {
        match e {
            NetError::Offline => FailureKind::Offline,
            NetError::Timeout => FailureKind::Timeout,
            NetError::Tls(_)
            | NetError::Http(_)
            | NetError::HostNotAllowed(_)
            | NetError::Other(_) => FailureKind::Network,
            NetError::Io(_) => FailureKind::Other,
        }
    }
}

// ── App startup ──

/// Emits to every webview.
struct AppSink(tauri::AppHandle);

impl DepsSink for AppSink {
    fn emit_status(&self, snapshot: &DepsSnapshot) {
        use tauri::Emitter;
        let _ = self.0.emit(STATUS_EVENT, snapshot);
    }
    fn emit_progress(&self, event: &ProgressEvent) {
        use tauri::Emitter;
        let _ = self.0.emit(PROGRESS_EVENT, event);
    }
}

/// The real pin (the global `keitora.toml`) and discovery.
struct UserHerdrConfig;

impl HerdrConfig for UserHerdrConfig {
    fn pinned(&self) -> Option<PathBuf> {
        keitora_tasks::UserConfig::load()
            .ok()
            .and_then(|c| c.pty.herdr_bin)
    }

    fn set_pinned(&self, path: Option<PathBuf>) -> Result<(), String> {
        keitora_tasks::UserConfig::set_herdr_bin(path).map_err(|e| e.to_string())
    }

    fn current_exe(&self) -> Option<PathBuf> {
        use crate::backend::herdr::env::{discover_exe, LoginSearch};
        let pty = keitora_tasks::UserConfig::load()
            .map(|c| c.pty)
            .unwrap_or_default();
        let d = discover_exe(&pty, LoginSearch::Deferred);
        if !d.found {
            return None;
        }
        // A bare `herdr` means "on PATH"; the candidate that hit is the absolute one.
        if d.exe.is_absolute() {
            Some(d.exe)
        } else {
            d.searched.last().cloned()
        }
    }
}

/// Build the managed [`DepsService`] over the already-managed backend, then (off the
/// setup thread) delete stray staging dirs and read the herdr binary's version.
pub fn startup(app: tauri::AppHandle) {
    use tauri::Manager;
    let backend = app.state::<crate::backend::SharedBackend>().inner().clone();
    let floor = backend.floor_slot().unwrap_or_default();
    let ctx = DepsContext {
        keitora_home: crate::paths::keitora_home(),
        home: dirs::home_dir(),
        env: std::env::vars().collect(),
        release_build: !cfg!(debug_assertions),
        app_version: app.package_info().version.to_string(),
        config: Box::new(UserHerdrConfig),
        prober: Box::new(crate::agent_updates::probe::MachineProber),
    };
    let service = DepsService::new(
        backend,
        floor,
        Box::new(manifest::HttpNet::new()),
        Box::new(RealProc),
        Box::new(RealFs),
        Box::new(AppSink(app.clone())),
        manifest::Platform::current(),
        ctx,
    );
    app.manage(service.clone());
    let spawned = std::thread::Builder::new()
        .name("deps-startup".into())
        .spawn(move || {
            let removed = tier2::cleanup_staging(&RealFs, &crate::paths::keitora_home());
            if removed > 0 {
                log::info!("[deps] removed {removed} leftover staging dir(s)");
            }
            service.recheck();
        });
    if let Err(e) = spawned {
        log::warn!("[deps] could not start the startup check: {e}");
    }
}

// ── Processes ──

/// One child process to run. The environment is inherited, minus `env_remove`, plus
/// `env_set`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env_remove: Vec<String>,
    pub env_set: Vec<(String, String)>,
}

impl ProcSpec {
    pub fn new(program: impl Into<PathBuf>, args: &[&str]) -> Self {
        ProcSpec {
            program: program.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            ..ProcSpec::default()
        }
    }
}

/// A finished child process.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcOutput {
    /// `None` when the process was ended by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl ProcOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcError {
    /// The program does not exist.
    NotFound,
    /// Any other spawn or wait failure (access denied, policy blocks, ...).
    Spawn(String),
    /// Still running at the deadline; it was killed.
    Timeout,
}

impl fmt::Display for ProcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcError::NotFound => f.write_str("program not found"),
            ProcError::Spawn(m) => write!(f, "could not run: {m}"),
            ProcError::Timeout => f.write_str("timed out"),
        }
    }
}

/// Run a child to completion under a deadline, behind a trait so tests never spawn.
pub trait Proc: Send + Sync {
    /// Run `spec`, calling `on_line` for every stdout/stderr line as it arrives, and
    /// return the exit code and full output. Killed at `timeout`.
    fn run(
        &self,
        spec: &ProcSpec,
        timeout: Duration,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ProcOutput, ProcError>;
}

/// How long a `--version` probe may take.
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(15);

/// Why a binary's `--version` could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionError {
    Run(ProcError),
    /// Ran but exited non-zero.
    Exit(Option<i32>),
    /// Ran but printed no recognisable version.
    Unparseable(String),
}

impl fmt::Display for VersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VersionError::Run(e) => write!(f, "--version failed: {e}"),
            VersionError::Exit(Some(c)) => write!(f, "--version exited with code {c}"),
            VersionError::Exit(None) => f.write_str("--version was terminated"),
            VersionError::Unparseable(out) => write!(f, "--version printed {out:?}"),
        }
    }
}

/// Run `<exe> --version` and parse it. Returns the version and the trimmed output.
pub fn binary_version(
    proc_: &dyn Proc,
    exe: &Path,
) -> Result<(semver::Version, String), VersionError> {
    let out = proc_
        .run(
            &ProcSpec::new(exe, &["--version"]),
            VERSION_TIMEOUT,
            &mut |_| {},
        )
        .map_err(VersionError::Run)?;
    if !out.success() {
        return Err(VersionError::Exit(out.code));
    }
    let text = out.stdout.trim().to_string();
    match floor::parse_version(&text) {
        Some(v) => Ok((v, text)),
        None => Err(VersionError::Unparseable(text)),
    }
}

/// The real [`Proc`]: `std::process` with the console window hidden on Windows.
pub struct RealProc;

/// Poll interval while waiting for output or exit.
const PROC_POLL: Duration = Duration::from_millis(50);
/// How long to keep draining pipes after the child exited (a grandchild may hold them).
const PROC_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Read `pipe` line by line on its own thread, sending `(is_stdout, line)`.
fn pipe_lines(
    pipe: Option<impl Read + Send + 'static>,
    is_stdout: bool,
    tx: mpsc::Sender<(bool, String)>,
) {
    let Some(pipe) = pipe else { return };
    std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim_end_matches(['\n', '\r']).to_string();
                    if tx.send((is_stdout, line)).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// Append one captured line to the right stream of `out`.
fn record_line(out: &mut ProcOutput, is_stdout: bool, line: &str) {
    let dst = if is_stdout {
        &mut out.stdout
    } else {
        &mut out.stderr
    };
    dst.push_str(line);
    dst.push('\n');
}

impl Proc for RealProc {
    fn run(
        &self,
        spec: &ProcSpec,
        timeout: Duration,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ProcOutput, ProcError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args);
        for k in &spec.env_remove {
            cmd.env_remove(k);
        }
        for (k, v) in &spec.env_set {
            cmd.env(k, v);
        }
        crate::backend::herdr::configure_command(&mut cmd);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => ProcError::NotFound,
            _ => ProcError::Spawn(e.to_string()),
        })?;
        let (tx, rx) = mpsc::channel();
        pipe_lines(child.stdout.take(), true, tx.clone());
        pipe_lines(child.stderr.take(), false, tx);

        let mut out = ProcOutput::default();
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Ok((is_stdout, line)) = rx.recv_timeout(PROC_POLL) {
                on_line(&line);
                record_line(&mut out, is_stdout, &line);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProcError::Spawn(e.to_string()));
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProcError::Timeout);
            }
        };
        let drain_until = Instant::now() + PROC_DRAIN_GRACE;
        while let Some(left) = drain_until.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok((is_stdout, line)) => {
                    on_line(&line);
                    record_line(&mut out, is_stdout, &line);
                }
                Err(_) => break,
            }
        }
        out.code = status.code();
        Ok(out)
    }
}

// ── Filesystem ──

/// Total bytes a release archive may unpack to.
pub const MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnpackError {
    /// An entry that would escape the destination (`..`, absolute) or is a symlink.
    Unsafe(String),
    /// The archive unpacks to more than the cap.
    TooLarge,
    /// Not a readable zip.
    Corrupt(String),
    Io(String),
}

impl fmt::Display for UnpackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnpackError::Unsafe(name) => write!(f, "refused unsafe archive entry {name:?}"),
            UnpackError::TooLarge => f.write_str("archive is larger than allowed"),
            UnpackError::Corrupt(m) => write!(f, "archive unreadable: {m}"),
            UnpackError::Io(m) => write!(f, "unpack failed: {m}"),
        }
    }
}

/// The filesystem operations the installers need, behind a trait so failures can be
/// injected in tests.
pub trait Fs: Send + Sync {
    fn create_dir_all(&self, p: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_dir_all(&self, p: &Path) -> io::Result<()>;
    fn remove_file(&self, p: &Path) -> io::Result<()>;
    /// The entries of directory `p` (full paths).
    fn list(&self, p: &Path) -> io::Result<Vec<PathBuf>>;
    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()>;
    fn read_to_string(&self, p: &Path) -> io::Result<String>;
    fn exists(&self, p: &Path) -> bool;
    fn is_dir(&self, p: &Path) -> bool;
    /// Lowercase hex sha256 of the file at `p`.
    fn sha256(&self, p: &Path) -> io::Result<String>;
    /// Mark `p` executable (`0o755`); a no-op on Windows.
    fn set_executable(&self, p: &Path) -> io::Result<()>;
    /// Safely unpack the zip at `archive` into the existing directory `dest`.
    fn unpack_zip(&self, archive: &Path, dest: &Path, max_bytes: u64) -> Result<(), UnpackError>;
}

/// The real [`Fs`] over `std::fs`.
pub struct RealFs;

impl Fs for RealFs {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        std::fs::create_dir_all(p)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn remove_dir_all(&self, p: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(p)
    }
    fn remove_file(&self, p: &Path) -> io::Result<()> {
        std::fs::remove_file(p)
    }
    fn list(&self, p: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(p)?.map(|e| e.map(|e| e.path())).collect()
    }
    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(p, bytes)
    }
    fn read_to_string(&self, p: &Path) -> io::Result<String> {
        std::fs::read_to_string(p)
    }
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }
    fn is_dir(&self, p: &Path) -> bool {
        p.is_dir()
    }
    fn sha256(&self, p: &Path) -> io::Result<String> {
        use sha2::{Digest, Sha256};
        let mut file = std::fs::File::open(p)?;
        let mut hasher = Sha256::new();
        io::copy(&mut file, &mut hasher)?;
        Ok(hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }
    fn set_executable(&self, p: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
        }
        #[cfg(not(unix))]
        {
            let _ = p;
            Ok(())
        }
    }
    fn unpack_zip(&self, archive: &Path, dest: &Path, max_bytes: u64) -> Result<(), UnpackError> {
        let file = std::fs::File::open(archive).map_err(|e| UnpackError::Io(e.to_string()))?;
        tier2::unpack_zip(BufReader::new(file), dest, max_bytes)
    }
}

/// Test doubles for [`Proc`] and [`Fs`].
#[cfg(test)]
pub(crate) mod fakes {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    type Responder = Box<dyn Fn(&ProcSpec) -> Result<ProcOutput, ProcError> + Send + Sync>;

    /// A [`Proc`] that answers from a closure and records every spec it was asked to
    /// run (with its timeout). Each stdout line is also streamed to `on_line`.
    pub struct FakeProc {
        respond: Responder,
        pub calls: Mutex<Vec<(ProcSpec, Duration)>>,
    }

    impl FakeProc {
        pub fn new(
            respond: impl Fn(&ProcSpec) -> Result<ProcOutput, ProcError> + Send + Sync + 'static,
        ) -> Self {
            FakeProc {
                respond: Box::new(respond),
                calls: Mutex::new(Vec::new()),
            }
        }

        /// Every program answers `--version` with `stdout` and exit 0.
        pub fn version(stdout: &str) -> Self {
            let stdout = stdout.to_string();
            FakeProc::new(move |_| Ok(ok(&stdout)))
        }

        pub fn calls(&self) -> Vec<(ProcSpec, Duration)> {
            self.calls.lock().unwrap().clone()
        }
    }

    /// Exit 0 with `stdout`.
    pub fn ok(stdout: &str) -> ProcOutput {
        ProcOutput {
            code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    impl Proc for FakeProc {
        fn run(
            &self,
            spec: &ProcSpec,
            timeout: Duration,
            on_line: &mut dyn FnMut(&str),
        ) -> Result<ProcOutput, ProcError> {
            self.calls.lock().unwrap().push((spec.clone(), timeout));
            let out = (self.respond)(spec)?;
            for line in out.stdout.lines() {
                on_line(line);
            }
            Ok(out)
        }
    }

    /// [`RealFs`] (point it at a temp dir) with injectable failures.
    #[derive(Default)]
    pub struct FakeFs {
        pub fail_rename: AtomicBool,
    }

    impl Fs for FakeFs {
        fn create_dir_all(&self, p: &Path) -> io::Result<()> {
            RealFs.create_dir_all(p)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            if self.fail_rename.load(Ordering::SeqCst) {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected"));
            }
            RealFs.rename(from, to)
        }
        fn remove_dir_all(&self, p: &Path) -> io::Result<()> {
            RealFs.remove_dir_all(p)
        }
        fn remove_file(&self, p: &Path) -> io::Result<()> {
            RealFs.remove_file(p)
        }
        fn list(&self, p: &Path) -> io::Result<Vec<PathBuf>> {
            RealFs.list(p)
        }
        fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()> {
            RealFs.write(p, bytes)
        }
        fn read_to_string(&self, p: &Path) -> io::Result<String> {
            RealFs.read_to_string(p)
        }
        fn exists(&self, p: &Path) -> bool {
            RealFs.exists(p)
        }
        fn is_dir(&self, p: &Path) -> bool {
            RealFs.is_dir(p)
        }
        fn sha256(&self, p: &Path) -> io::Result<String> {
            RealFs.sha256(p)
        }
        fn set_executable(&self, p: &Path) -> io::Result<()> {
            RealFs.set_executable(p)
        }
        fn unpack_zip(
            &self,
            archive: &Path,
            dest: &Path,
            max_bytes: u64,
        ) -> Result<(), UnpackError> {
            RealFs.unpack_zip(archive, dest, max_bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::{ok, FakeProc};
    use super::*;

    #[test]
    fn net_errors_classify_into_failure_kinds() {
        let cases = [
            (NetError::Offline, FailureKind::Offline),
            (NetError::Timeout, FailureKind::Timeout),
            (NetError::Tls("bad cert".into()), FailureKind::Network),
            (NetError::Http(503), FailureKind::Network),
            (
                NetError::HostNotAllowed("https://example.com/x".into()),
                FailureKind::Network,
            ),
            (NetError::Other("reset".into()), FailureKind::Network),
            (NetError::Io("disk full".into()), FailureKind::Other),
        ];
        for (err, want) in cases {
            assert_eq!(FailureKind::from(&err), want, "{err:?}");
        }
    }

    #[test]
    fn binary_version_table() {
        let exe = Path::new("/opt/x/herdr");
        let p = FakeProc::version("herdr 0.9.3\n");
        assert_eq!(
            binary_version(&p, exe).unwrap(),
            (semver::Version::new(0, 9, 3), "herdr 0.9.3".to_string())
        );
        let (spec, timeout) = p.calls()[0].clone();
        assert_eq!(spec.program, exe);
        assert_eq!(spec.args, ["--version"]);
        assert_eq!(timeout, VERSION_TIMEOUT);

        let p = FakeProc::new(|_| {
            Ok(ProcOutput {
                code: Some(1),
                ..ProcOutput::default()
            })
        });
        assert_eq!(binary_version(&p, exe), Err(VersionError::Exit(Some(1))));
        let p = FakeProc::new(|_| Err(ProcError::Spawn("access denied".into())));
        assert!(matches!(
            binary_version(&p, exe),
            Err(VersionError::Run(ProcError::Spawn(_)))
        ));
        let p = FakeProc::new(|_| Ok(ok("usage: tool [opts]")));
        assert!(matches!(
            binary_version(&p, exe),
            Err(VersionError::Unparseable(_))
        ));
    }

    #[test]
    fn real_fs_sha256_is_lowercase_hex() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("abc");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            RealFs.sha256(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    const ALL_KINDS: [FailureKind; 10] = [
        FailureKind::Offline,
        FailureKind::Network,
        FailureKind::PolicyBlocked,
        FailureKind::Checksum,
        FailureKind::BlockedByAV,
        FailureKind::Timeout,
        FailureKind::Incomplete,
        FailureKind::TooOldAfterInstall,
        FailureKind::NoCompatibleRelease,
        FailureKind::Other,
    ];

    #[test]
    fn after_failure_table() {
        for kind in ALL_KINDS {
            for tier in [Tier::Script, Tier::Direct, Tier::Manual] {
                for direct_viable in [false, true] {
                    let want = if tier == Tier::Script
                        && direct_viable
                        && !matches!(
                            kind,
                            FailureKind::Offline | FailureKind::NoCompatibleRelease
                        ) {
                        NextStep::TryDirect
                    } else {
                        NextStep::Manual
                    };
                    assert_eq!(
                        after_failure(kind, tier, direct_viable),
                        want,
                        "{kind:?} {tier:?} {direct_viable}"
                    );
                }
            }
        }
        // Spot checks that pin the intent independently of the loop above.
        assert_eq!(
            after_failure(FailureKind::PolicyBlocked, Tier::Script, true),
            NextStep::TryDirect
        );
        assert_eq!(
            after_failure(FailureKind::Incomplete, Tier::Script, false),
            NextStep::Manual
        );
        assert_eq!(
            after_failure(FailureKind::Checksum, Tier::Direct, true),
            NextStep::Manual
        );
        assert_eq!(
            after_failure(FailureKind::Offline, Tier::Script, true),
            NextStep::Manual
        );
    }

    #[test]
    fn stages_are_ordered_forward() {
        let order = [
            Stage::Checking,
            Stage::Downloading,
            Stage::Installing,
            Stage::Verifying,
            Stage::Restarting,
            Stage::Done,
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]));
        assert!(Stage::Done.is_terminal() && Stage::Failed.is_terminal());
        assert!(!Stage::Restarting.is_terminal());
    }

    #[test]
    fn wire_names_are_snake_case() {
        let names: Vec<String> = ALL_KINDS
            .iter()
            .map(|k| {
                serde_json::to_value(k)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            names,
            [
                "offline",
                "network",
                "policy_blocked",
                "checksum",
                "blocked_by_av",
                "timeout",
                "incomplete",
                "too_old_after_install",
                "no_compatible_release",
                "other"
            ]
        );
        assert_eq!(
            serde_json::to_value(NextStep::TryDirect).unwrap(),
            "try_direct"
        );
        assert_eq!(
            serde_json::to_value(Stage::Restarting).unwrap(),
            "restarting"
        );
    }
}
