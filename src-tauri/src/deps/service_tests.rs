//! Service-level tests: prompt rules, the install mutex, upgrade orchestration over a
//! fake backend (and once over the herdr backend with a fake herdr), `set_herdr_bin`,
//! the doctor, and the wire shapes. Nothing here spawns a process, touches the
//! network, or reads the real home or config.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use keitora_tasks::{CliKind, UserConfig};
use serde_json::json;

use super::doctor::{parse_git_version, DoctorAction, DoctorStatus};
use super::fakes::FakeFs;
use super::floor::{FloorSlot, FloorState};
use super::manifest::fake::{FakeNet, Reply};
use super::manifest::{Net, NetError, Platform, MANIFEST_URL};
use super::preflight::UpgradeRoute;
use super::service::{PromptKind, RunState, Timing, UpgradePrompt};
use super::*;
use crate::agent_updates::probe::Prober;
use crate::backend::fake::{Call, FakeBackend};
use crate::backend::herdr::cli::ServerStatus;
use crate::backend::herdr::exe_missing::NOT_FOUND_CODE;
use crate::backend::herdr::gate::RecoveryPayload;
use crate::backend::herdr::recovery::RecoveryPhase;
use crate::backend::{PtyBackend, PtyBackendError, SharedBackend};

const ASSET_URL: &str = "https://github.com/example/herdr/releases/download/v0.9.6/herdr-mac";
const ASSET: &[u8] = b"pretend this is herdr 0.9.6";

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A one-release manifest for `macos-aarch64` whose latest is 0.9.6.
fn manifest() -> String {
    let sha = sha256_hex(ASSET);
    json!({
        "version": "0.9.6",
        "protocol": 22,
        "assets": { "macos-aarch64": ASSET_URL },
        "sha256": { "macos-aarch64": sha },
        "releases": {
            "0.9.6": {
                "protocol": 22,
                "assets": { "macos-aarch64": ASSET_URL },
                "sha256": { "macos-aarch64": sha }
            }
        }
    })
    .to_string()
}

fn fake_net() -> FakeNet {
    FakeNet::default()
        .with(MANIFEST_URL, Reply::Body(manifest().into_bytes()))
        .with(ASSET_URL, Reply::Body(ASSET.to_vec()))
}

// ── Fakes ──

/// Shares a fake between the service (which takes ownership of a `Box`) and the test.
struct Shared<T>(Arc<T>);

impl<T: Proc> Proc for Shared<T> {
    fn run(
        &self,
        spec: &ProcSpec,
        timeout: Duration,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ProcOutput, ProcError> {
        self.0.run(spec, timeout, on_line)
    }
}

impl<T: Net> Net for Shared<T> {
    fn get_text(&self, url: &str) -> Result<String, NetError> {
        self.0.get_text(url)
    }
    fn download(
        &self,
        url: &str,
        dest: &Path,
        on_bytes: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), NetError> {
        self.0.download(url, dest, on_bytes)
    }
}

impl<T: DepsSink> DepsSink for Shared<T> {
    fn emit_status(&self, snapshot: &DepsSnapshot) {
        self.0.emit_status(snapshot)
    }
    fn emit_progress(&self, event: &ProgressEvent) {
        self.0.emit_progress(event)
    }
}

impl<T: HerdrConfig> HerdrConfig for Shared<T> {
    fn pinned(&self) -> Option<PathBuf> {
        self.0.pinned()
    }
    fn set_pinned(&self, path: Option<PathBuf>) -> Result<(), String> {
        self.0.set_pinned(path)
    }
    fn current_exe(&self) -> Option<PathBuf> {
        self.0.current_exe()
    }
}

type ScriptFn = Box<dyn Fn() -> Result<ProcOutput, ProcError> + Send + Sync>;

/// A gate a fake call parks on: the test sees it entered, then lets it go.
#[derive(Default)]
struct Latch {
    state: Mutex<(bool, bool)>, // (entered, open)
    cv: Condvar,
}

impl Latch {
    fn park(&self) {
        let mut st = self.state.lock().unwrap();
        st.0 = true;
        self.cv.notify_all();
        while !st.1 {
            st = self.cv.wait(st).unwrap();
        }
    }
    fn wait_entered(&self) {
        let mut st = self.state.lock().unwrap();
        while !st.0 {
            st = self.cv.wait_timeout(st, Duration::from_secs(20)).unwrap().0;
        }
    }
    fn open(&self) {
        self.state.lock().unwrap().1 = true;
        self.cv.notify_all();
    }
}

/// Answers `--version` per program (default `herdr 0.9.6`), `git --version`, and the
/// vendor script (`sh` / `powershell`) from a closure. Records every spec.
struct ScriptProc {
    versions: Mutex<HashMap<PathBuf, String>>,
    git: Mutex<Result<ProcOutput, ProcError>>,
    script: Mutex<ScriptFn>,
    calls: Mutex<Vec<ProcSpec>>,
    /// When set, a herdr `--version` parks on it.
    hold_versions: Mutex<Option<Arc<Latch>>>,
}

impl ScriptProc {
    fn new() -> Self {
        ScriptProc {
            versions: Mutex::default(),
            git: Mutex::new(Ok(fakes::ok("git version 2.43.0.windows.1\n"))),
            script: Mutex::new(Box::new(|| Err(ProcError::NotFound))),
            calls: Mutex::default(),
            hold_versions: Mutex::default(),
        }
    }

    fn version_of(&self, exe: &Path, out: &str) {
        self.versions
            .lock()
            .unwrap()
            .insert(exe.to_path_buf(), out.to_string());
    }

    fn on_script(&self, f: impl Fn() -> Result<ProcOutput, ProcError> + Send + Sync + 'static) {
        *self.script.lock().unwrap() = Box::new(f);
    }

    fn programs(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.program.display().to_string())
            .collect()
    }
}

impl Proc for ScriptProc {
    fn run(
        &self,
        spec: &ProcSpec,
        _timeout: Duration,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ProcOutput, ProcError> {
        self.calls.lock().unwrap().push(spec.clone());
        let program = spec.program.display().to_string();
        let out = if program == "git" {
            self.git.lock().unwrap().clone()
        } else if program == "sh" || program == "powershell" {
            (self.script.lock().unwrap())()
        } else if program == "curl" {
            Ok(fakes::ok(""))
        } else {
            let hold = self.hold_versions.lock().unwrap().clone();
            if let Some(latch) = hold {
                latch.park();
            }
            let v = self
                .versions
                .lock()
                .unwrap()
                .get(&spec.program)
                .cloned()
                .unwrap_or_else(|| "herdr 0.9.6".into());
            Ok(fakes::ok(&v))
        }?;
        for line in out.stdout.lines() {
            on_line(line);
        }
        Ok(out)
    }
}

/// The pin in memory; `current_exe` is the pin, else `discovered`.
#[derive(Default)]
struct MemConfig {
    pinned: Mutex<Option<PathBuf>>,
    discovered: Mutex<Option<PathBuf>>,
    writes: Mutex<Vec<Option<PathBuf>>>,
}

impl HerdrConfig for MemConfig {
    fn pinned(&self) -> Option<PathBuf> {
        self.pinned.lock().unwrap().clone()
    }
    fn set_pinned(&self, path: Option<PathBuf>) -> Result<(), String> {
        self.writes.lock().unwrap().push(path.clone());
        *self.pinned.lock().unwrap() = path;
        Ok(())
    }
    fn current_exe(&self) -> Option<PathBuf> {
        self.pinned()
            .or_else(|| self.discovered.lock().unwrap().clone())
    }
}

#[derive(Debug, Clone)]
enum Ev {
    Status(Box<DepsSnapshot>),
    Progress(ProgressEvent),
}

#[derive(Default)]
struct RecSink {
    events: Mutex<Vec<Ev>>,
    changed: Condvar,
}

impl DepsSink for RecSink {
    fn emit_status(&self, snapshot: &DepsSnapshot) {
        self.events
            .lock()
            .unwrap()
            .push(Ev::Status(Box::new(snapshot.clone())));
        self.changed.notify_all();
    }
    fn emit_progress(&self, event: &ProgressEvent) {
        self.events
            .lock()
            .unwrap()
            .push(Ev::Progress(event.clone()));
        self.changed.notify_all();
    }
}

impl RecSink {
    fn statuses(&self) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, Ev::Status(_)))
            .count()
    }

    fn progress(&self) -> Vec<ProgressEvent> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Ev::Progress(p) => Some(p.clone()),
                _ => None,
            })
            .collect()
    }

    /// Wait for the run's terminal progress event and the status published after
    /// it; returns that status's run.
    fn wait_finished(&self, run_id: u64) -> RunState {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut events = self.events.lock().unwrap();
        loop {
            let terminal_at = events.iter().position(
                |e| matches!(e, Ev::Progress(p) if p.run_id == run_id && p.stage.is_terminal()),
            );
            if let Some(i) = terminal_at {
                let after = events[i..].iter().find_map(|e| match e {
                    Ev::Status(s) if !matches!(s.run, RunState::Running { .. }) => {
                        Some(s.run.clone())
                    }
                    _ => None,
                });
                if let Some(run) = after {
                    return run;
                }
            }
            let left = deadline
                .checked_duration_since(Instant::now())
                .expect("the run did not finish in time");
            events = self.changed.wait_timeout(events, left).unwrap().0;
        }
    }
}

/// Finds claude only.
struct FakeProber;

impl Prober for FakeProber {
    fn config(&self) -> UserConfig {
        UserConfig::default()
    }
    fn exe(&self, cli: CliKind, _cfg: &UserConfig) -> Option<String> {
        (cli == CliKind::Claude).then(|| "/opt/tools/claude".to_string())
    }
    fn resolve(&self, exe: &str) -> String {
        exe.to_string()
    }
    fn is_file(&self, _path: &str) -> bool {
        true
    }
    fn installed(&self, _cli: CliKind, _exe: &str) -> Option<String> {
        Some("2.1.0".into())
    }
    fn invalidate(&self, _cli: CliKind, _exe: &str) {}
    fn latest(&self, _cli: CliKind) -> Option<String> {
        None
    }
}

// ── Rig ──

struct Rig {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    khome: PathBuf,
    floor: Arc<FloorSlot>,
    cfg: Arc<MemConfig>,
    sink: Arc<RecSink>,
    proc_: Arc<ScriptProc>,
    net: Arc<FakeNet>,
    svc: DepsService,
}

struct Opts {
    platform: Option<Platform>,
    release_build: bool,
    net: FakeNet,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            platform: Some(Platform::MacosAarch64),
            release_build: false,
            net: fake_net(),
        }
    }
}

impl Rig {
    fn over(backend: SharedBackend, floor: Arc<FloorSlot>, opts: Opts) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let khome = tmp.path().join("keitora");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&khome).unwrap();
        let cfg = Arc::new(MemConfig::default());
        let sink = Arc::new(RecSink::default());
        let proc_ = Arc::new(ScriptProc::new());
        let net = Arc::new(opts.net);
        let ctx = DepsContext {
            keitora_home: khome.clone(),
            home: Some(home.clone()),
            env: Vec::new(),
            release_build: opts.release_build,
            app_version: "0.5.0".into(),
            config: Box::new(Shared(Arc::clone(&cfg))),
            prober: Box::new(FakeProber),
        };
        let svc = DepsService::new(
            backend,
            Arc::clone(&floor),
            Box::new(Shared(Arc::clone(&net))),
            Box::new(Shared(Arc::clone(&proc_))),
            Box::new(FakeFs::default()),
            Box::new(Shared(Arc::clone(&sink))),
            opts.platform,
            ctx,
        );
        Rig {
            _tmp: tmp,
            home,
            khome,
            floor,
            cfg,
            sink,
            proc_,
            net,
            svc,
        }
    }

    /// Over a [`FakeBackend`] whose floor slot is the rig's.
    fn new(opts: Opts) -> (Rig, Arc<FakeBackend>) {
        let backend = Arc::new(FakeBackend::new());
        let floor = Arc::new(FloorSlot::new());
        backend.set_floor_slot(Arc::clone(&floor));
        let rig = Rig::over(Arc::clone(&backend) as SharedBackend, floor, opts);
        (rig, backend)
    }

    /// A file at `path` that the fake `--version` reports as `version`.
    fn binary(&self, path: &Path, version: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"bin").unwrap();
        self.proc_.version_of(path, &format!("herdr {version}"));
    }

    /// A managed herdr `version` in use (pinned).
    fn managed_in_use(&self, version: &str) -> PathBuf {
        let exe = tier2::managed_root(&self.khome).join(version).join("herdr");
        self.binary(&exe, version);
        *self.cfg.pinned.lock().unwrap() = Some(exe.clone());
        exe
    }

    fn install(&self, tier: Option<Tier>) -> RunState {
        self.svc.start_install(tier, false).unwrap();
        let run_id = match self.svc.snapshot().run {
            RunState::Running { run_id, .. }
            | RunState::Done { run_id, .. }
            | RunState::Failed { run_id, .. } => run_id,
            RunState::Idle => panic!("no run started"),
        };
        self.sink.wait_finished(run_id)
    }

    fn managed_exe(&self, version: &str) -> PathBuf {
        tier2::managed_root(&self.khome).join(version).join("herdr")
    }
}

fn too_old(v: &str) -> FloorState {
    FloorState::TooOld { found: v.into() }
}

fn mismatch() -> FloorState {
    FloorState::ProtocolMismatch {
        found: 23,
        want: 22,
        newer: true,
    }
}

/// A running server's `status server` reply.
fn server(version: &str, protocol: u64) -> ServerStatus {
    ServerStatus {
        running: true,
        version: Some(version.into()),
        protocol: Some(protocol),
        socket: None,
    }
}

/// Restart waits short enough for tests.
fn quick(retry_limit: Duration) -> Timing {
    Timing {
        settle: Duration::from_millis(100),
        retry_poll: Duration::from_millis(10),
        retry_limit,
    }
}

/// Wait until `pred` holds (20 s at most).
fn eventually(what: &str, pred: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !pred() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn recovery(phase: RecoveryPhase, reason_code: Option<&'static str>) -> RecoveryPayload {
    RecoveryPayload {
        phase,
        reason_code,
        ..RecoveryPayload::healthy()
    }
}

fn count(calls: &[Call], want: &Call) -> usize {
    calls.iter().filter(|c| *c == want).count()
}

// ── Prompt ──

#[test]
fn a_too_old_server_prompts_once_per_version_per_session() {
    let (r, _) = Rig::new(Opts::default());
    assert_eq!(r.svc.snapshot().prompt, None);
    r.floor.set(too_old("0.9.3"));
    let soft = UpgradePrompt {
        kind: PromptKind::TooOld,
        found: "0.9.3".into(),
        hard: false,
    };
    assert_eq!(r.svc.snapshot().prompt, Some(soft.clone()));
    r.svc.snooze_prompt();
    assert_eq!(r.svc.snapshot().prompt, None, "Later clears it");

    // Reconnects and recoveries bring the same version back: no second prompt.
    r.floor.set(FloorState::Unknown);
    r.floor.set(too_old("0.9.3"));
    assert_eq!(r.svc.snapshot().prompt, None);
    // A different old version is a new question.
    r.floor.set(too_old("0.9.4"));
    assert_eq!(r.svc.snapshot().prompt.unwrap().found, "0.9.4");
    r.floor.set(FloorState::Ok);
    assert_eq!(r.svc.snapshot().prompt, None, "fixed: nothing to ask");
}

#[test]
fn a_protocol_mismatch_prompt_is_hard_and_not_snoozable() {
    let (r, _) = Rig::new(Opts::default());
    r.floor.set(mismatch());
    let hard = UpgradePrompt {
        kind: PromptKind::ProtocolMismatch,
        found: "23".into(),
        hard: true,
    };
    assert_eq!(r.svc.snapshot().prompt, Some(hard.clone()));
    r.svc.snooze_prompt();
    assert_eq!(
        r.svc.snapshot().prompt,
        Some(hard),
        "no Later for a mismatch"
    );
    // Downgraded to merely old: the hard prompt goes, a soft one replaces it.
    r.floor.set(too_old("0.9.3"));
    assert!(!r.svc.snapshot().prompt.unwrap().hard);
}

#[test]
fn identical_floor_sets_do_not_refire() {
    let (r, _) = Rig::new(Opts::default());
    r.floor.set(too_old("0.9.3"));
    let after_first = r.sink.statuses();
    assert_eq!(after_first, 1, "the change is published");
    for _ in 0..5 {
        r.floor.set(too_old("0.9.3"));
    }
    assert_eq!(r.sink.statuses(), after_first);
    r.svc.snooze_prompt();
    r.floor.set(too_old("0.9.3"));
    assert_eq!(r.svc.snapshot().prompt, None);
}

// ── Snapshot ──

#[test]
fn herdr_installed_follows_the_not_found_reason_code() {
    let (r, backend) = Rig::new(Opts::default());
    assert!(r.svc.snapshot().herdr_installed);
    backend.set_recovery(recovery(RecoveryPhase::Healthy, Some(NOT_FOUND_CODE)));
    assert!(!r.svc.snapshot().herdr_installed);
    backend.set_recovery(recovery(RecoveryPhase::Paused, Some("start_failed")));
    assert!(
        r.svc.snapshot().herdr_installed,
        "other causes are not 'missing'"
    );
}

#[test]
fn restart_to_finish_when_the_pinned_binary_is_newer_than_the_server() {
    let (r, _) = Rig::new(Opts::default());
    r.managed_in_use("0.9.6");
    r.floor.set(too_old("0.9.3"));
    assert_eq!(
        r.svc.snapshot().floor,
        too_old("0.9.3"),
        "not rechecked yet"
    );
    let snap = r.svc.recheck();
    assert_eq!(
        snap.floor,
        FloorState::RestartToFinish {
            binary: "0.9.6".into(),
            server: "0.9.3".into()
        }
    );
    assert_eq!(
        snap.prompt, None,
        "no upgrade prompt for an installed upgrade"
    );
    // An equally old binary is still just too old.
    r.managed_in_use("0.9.3");
    assert_eq!(r.svc.recheck().floor, too_old("0.9.3"));
}

/// A protocol mismatch whose fix is already pinned (a compatible managed copy) only
/// needs a restart: the hard prompt yields to "Restart to finish".
#[test]
fn restart_to_finish_covers_a_protocol_mismatch_with_a_compatible_pin() {
    let (r, _) = Rig::new(Opts::default());
    r.floor.observe(&server("0.12.0", 23));
    assert!(r.svc.snapshot().prompt.unwrap().hard);
    r.managed_in_use("0.9.6");
    let snap = r.svc.recheck();
    assert_eq!(
        snap.floor,
        FloorState::RestartToFinish {
            binary: "0.9.6".into(),
            server: "0.12.0".into()
        }
    );
    assert_eq!(snap.prompt, None, "the hard prompt yields");
    // Pinning the very binary the server runs changes nothing.
    r.managed_in_use("0.12.0");
    assert_eq!(r.svc.recheck().floor, mismatch());
}

/// A pin changed while an ok server runs is finished by a restart too.
#[test]
fn restart_to_finish_covers_an_ok_server_whose_pin_changed() {
    let (r, _) = Rig::new(Opts::default());
    r.floor.observe(&server("0.9.5", 22));
    r.managed_in_use("0.9.5");
    assert_eq!(r.svc.recheck().floor, FloorState::Ok);
    r.managed_in_use("0.9.6");
    assert_eq!(
        r.svc.recheck().floor,
        FloorState::RestartToFinish {
            binary: "0.9.6".into(),
            server: "0.9.5".into()
        }
    );
}

#[test]
fn snapshot_reports_the_route_and_live_sessions() {
    let (r, backend) = Rig::new(Opts::default());
    backend.insert_live(crate::backend::SessionKey {
        project: "proj".into(),
        worktree: "main".into(),
    });
    r.managed_in_use("0.9.3");
    let snap = r.svc.recheck();
    assert_eq!(snap.live_sessions, 1);
    assert_eq!(snap.route, Some(UpgradeRoute::Direct));
    assert_eq!(snap.platform.as_deref(), Some("macos-aarch64"));
    assert!(snap.pinned_bin.unwrap().ends_with("herdr"));
}

// ── Install mutex ──

/// A [`Net`] whose downloads wait until released.
struct GatedNet {
    inner: FakeNet,
    open: Mutex<bool>,
    cv: Condvar,
}

impl GatedNet {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

impl Net for GatedNet {
    fn get_text(&self, url: &str) -> Result<String, NetError> {
        self.inner.get_text(url)
    }
    fn download(
        &self,
        url: &str,
        dest: &Path,
        on_bytes: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), NetError> {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.cv.wait(open).unwrap();
        }
        drop(open);
        self.inner.download(url, dest, on_bytes)
    }
}

#[test]
fn concurrent_installs_share_one_run_with_one_terminal_event() {
    let backend = Arc::new(FakeBackend::new());
    let floor = Arc::new(FloorSlot::new());
    let gated = Arc::new(GatedNet {
        inner: fake_net(),
        open: Mutex::new(false),
        cv: Condvar::new(),
    });
    // The rig's own net is unused; the service gets the gated one.
    let r = Rig::over(
        Arc::clone(&backend) as SharedBackend,
        floor,
        Opts::default(),
    );
    let ctx = DepsContext {
        keitora_home: r.khome.clone(),
        home: Some(r.home.clone()),
        env: Vec::new(),
        release_build: false,
        app_version: "0.5.0".into(),
        config: Box::new(Shared(Arc::clone(&r.cfg))),
        prober: Box::new(FakeProber),
    };
    let svc = DepsService::new(
        Arc::clone(&backend) as SharedBackend,
        Arc::clone(&r.floor),
        Box::new(Shared(Arc::clone(&gated))),
        Box::new(Shared(Arc::clone(&r.proc_))),
        Box::new(FakeFs::default()),
        Box::new(Shared(Arc::clone(&r.sink))),
        Some(Platform::MacosAarch64),
        ctx,
    );

    let threads: Vec<_> = (0..4)
        .map(|_| {
            let svc = svc.clone();
            std::thread::spawn(move || svc.start_install(None, false))
        })
        .collect();
    for t in threads {
        t.join().unwrap().unwrap();
    }
    svc.start_install(Some(Tier::Direct), false)
        .expect("attaches to the running run");
    assert!(matches!(
        svc.snapshot().run,
        RunState::Running { run_id: 1, .. }
    ));
    gated.release();
    let run = r.sink.wait_finished(1);
    assert!(matches!(run, RunState::Done { run_id: 1, .. }), "{run:?}");

    let progress = r.sink.progress();
    assert!(progress.iter().all(|p| p.run_id == 1), "one run only");
    let terminals = progress.iter().filter(|p| p.stage.is_terminal()).count();
    assert_eq!(terminals, 1, "{progress:?}");
    assert!(
        progress.windows(2).all(|w| w[0].stage <= w[1].stage),
        "stages only move forward: {progress:?}"
    );
    assert_eq!(progress.last().unwrap().stage, Stage::Done);
    let stages: Vec<Stage> = progress.iter().map(|p| p.stage).collect();
    for s in [
        Stage::Checking,
        Stage::Downloading,
        Stage::Installing,
        Stage::Verifying,
        Stage::Restarting,
    ] {
        assert!(stages.contains(&s), "{s:?} missing from {stages:?}");
    }

    // A finished run frees the mutex: the next request starts run 2.
    svc.start_install(None, false).unwrap();
    assert!(matches!(r.sink.wait_finished(2), RunState::Done { .. }));
}

// ── Upgrade orchestration ──

#[test]
fn a_managed_upgrade_downloads_then_repins_restarts_once_and_refreshes_without_a_stop() {
    let (r, backend) = Rig::new(Opts::default());
    backend.set_connected(true);
    let old = r.managed_in_use("0.9.3");
    r.floor.set(too_old("0.9.3"));
    r.svc.recheck();

    let run = r.install(None);
    let new = r.managed_exe("0.9.6");
    assert_eq!(
        run,
        RunState::Done {
            run_id: 1,
            path: new.display().to_string(),
            restart_pending: false
        }
    );
    assert_eq!(r.cfg.pinned(), Some(new.clone()));
    assert!(new.is_file());
    assert!(old.is_file(), "the previous version is kept for rollback");
    let calls = backend.calls();
    let restart = calls.iter().position(|c| *c == Call::Restart).unwrap();
    let refresh = calls
        .iter()
        .position(|c| *c == Call::RefreshIntegration)
        .unwrap();
    assert!(restart < refresh, "{calls:?}");
    // The explicit stop owes nothing a resume: an upgrade never uses it.
    assert_eq!(count(&calls, &Call::Shutdown), 0, "{calls:?}");
    assert_eq!(count(&calls, &Call::Restart), 1);
    assert_eq!(r.net.requests(), vec![MANIFEST_URL, ASSET_URL]);
    // The new binary is pinned and newer than the still-old server.
    assert!(matches!(
        r.svc.snapshot().floor,
        FloorState::RestartToFinish { .. }
    ));
}

/// Recovery busy when the swap happens (resuming sessions, or a restart someone else
/// asked for): the upgrade waits for it to settle, then restarts onto the new binary
/// exactly once; nothing is deferred behind that restart.
#[test]
fn an_upgrade_while_recovery_resumes_waits_for_it_to_settle_then_restarts_once() {
    let (r, backend) = Rig::new(Opts::default());
    r.svc.set_timing(Timing {
        settle: Duration::from_secs(10),
        retry_poll: Duration::from_millis(10),
        retry_limit: Duration::from_secs(20),
    });
    backend.set_connected(true);
    backend.set_recovery(recovery(RecoveryPhase::Resuming, None));
    r.managed_in_use("0.9.3");
    let new = r.managed_exe("0.9.6");
    // Recovery settles a little after the new binary is pinned.
    let settler = {
        let cfg = Arc::clone(&r.cfg);
        let backend = Arc::clone(&backend);
        let new = new.clone();
        std::thread::spawn(move || {
            eventually("the swap", || cfg.pinned() == Some(new.clone()));
            std::thread::sleep(Duration::from_millis(150));
            backend.set_recovery(recovery(RecoveryPhase::Healthy, None));
        })
    };
    let run = r.install(None);
    settler.join().unwrap();
    assert!(
        matches!(
            run,
            RunState::Done {
                restart_pending: false,
                ..
            }
        ),
        "{run:?}"
    );
    // Long enough for a stray deferred restart to show up.
    std::thread::sleep(Duration::from_millis(200));
    let calls = backend.calls();
    assert_eq!(count(&calls, &Call::Shutdown), 0, "{calls:?}");
    assert_eq!(count(&calls, &Call::Restart), 1, "{calls:?}");
    assert_eq!(count(&calls, &Call::RefreshIntegration), 1);
}

/// Recovery that stays busy past the wait: the restart is deferred, never dropped,
/// and made once recovery settles.
#[test]
fn a_restart_recovery_is_too_busy_for_is_made_once_it_settles() {
    let (r, backend) = Rig::new(Opts::default());
    r.svc.set_timing(quick(Duration::from_secs(20)));
    backend.set_connected(true);
    backend.set_recovery(recovery(RecoveryPhase::Resuming, None));
    r.managed_in_use("0.9.3");
    let run = r.install(None);
    assert!(
        matches!(
            run,
            RunState::Done {
                restart_pending: true,
                ..
            }
        ),
        "{run:?}"
    );
    assert_eq!(
        count(&backend.calls(), &Call::Restart),
        0,
        "it would be a no-op"
    );
    assert_eq!(r.cfg.pinned(), Some(r.managed_exe("0.9.6")));

    // The old server still runs (nothing stopped it); recovery then settles and the
    // deferred restart moves it onto the new binary.
    backend.set_recovery(recovery(RecoveryPhase::Healthy, None));
    eventually("the deferred restart", || {
        count(&backend.calls(), &Call::RefreshIntegration) == 1
    });
    assert_eq!(count(&backend.calls(), &Call::Restart), 1);
    eventually("the run to stop waiting", || {
        matches!(
            r.svc.snapshot().run,
            RunState::Done {
                restart_pending: false,
                ..
            }
        )
    });
}

/// A deferred restart that never gets its chance ends the run failed, so herdr is
/// never left down without saying so.
#[test]
fn a_deferred_restart_that_never_settles_fails_the_run() {
    let (r, backend) = Rig::new(Opts::default());
    r.svc.set_timing(quick(Duration::from_millis(100)));
    backend.set_connected(true);
    backend.set_recovery(recovery(RecoveryPhase::Resuming, None));
    r.managed_in_use("0.9.3");
    r.install(None);
    eventually("the run to fail", || {
        matches!(
            r.svc.snapshot().run,
            RunState::Failed {
                kind: FailureKind::Incomplete,
                ..
            }
        )
    });
    assert_eq!(count(&backend.calls(), &Call::Restart), 0);
}

#[test]
fn a_failed_restart_is_an_incomplete_install() {
    let (r, backend) = Rig::new(Opts::default());
    backend.push_restart(Err(PtyBackendError::Other("herdr was not found".into())));
    let run = r.install(None);
    match run {
        RunState::Failed { kind, message, .. } => {
            assert_eq!(kind, FailureKind::Incomplete);
            assert!(message.contains("herdr was not found"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(count(&backend.calls(), &Call::RefreshIntegration), 0);
}

/// The new binary will not start during an upgrade: the previous pin is put back,
/// the old binary restarted, and the run fails saying so.
#[test]
fn a_restart_failure_during_an_upgrade_restores_the_previous_pin() {
    let (r, backend) = Rig::new(Opts::default());
    backend.set_connected(true);
    let old = r.managed_in_use("0.9.3");
    backend.push_restart(Err(PtyBackendError::Other("herdr was not found".into())));
    let run = r.install(None);
    match run {
        RunState::Failed { kind, message, .. } => {
            assert_eq!(kind, FailureKind::Incomplete);
            assert!(message.contains("herdr was not found"), "{message}");
            assert!(message.contains("previous herdr"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.cfg.pinned(), Some(old), "the previous pin is back");
    let calls = backend.calls();
    assert_eq!(count(&calls, &Call::Shutdown), 0, "{calls:?}");
    assert_eq!(
        count(&calls, &Call::Restart),
        2,
        "the failed restart, then the old binary: {calls:?}"
    );
}

/// A restart that was deferred (recovery busy) is dropped once a direct restart
/// takes effect before it fires: herdr restarts once, not twice.
#[test]
fn a_deferred_restart_does_not_fire_after_a_direct_restart() {
    let (r, backend) = Rig::new(Opts::default());
    r.svc.set_timing(Timing {
        settle: Duration::from_millis(50),
        retry_poll: Duration::from_millis(400),
        retry_limit: Duration::from_secs(20),
    });
    backend.set_connected(true);
    backend.set_recovery(recovery(RecoveryPhase::Resuming, None));
    r.managed_in_use("0.9.3");
    let run = r.install(None);
    assert!(
        matches!(
            run,
            RunState::Done {
                restart_pending: true,
                ..
            }
        ),
        "{run:?}"
    );
    assert_eq!(count(&backend.calls(), &Call::Restart), 0);

    // Recovery settles and "Restart to finish" restarts directly before the retry
    // thread's next look.
    backend.set_recovery(recovery(RecoveryPhase::Healthy, None));
    r.svc.restart_to_finish().unwrap();
    assert_eq!(count(&backend.calls(), &Call::Restart), 1);
    assert!(
        matches!(
            r.svc.snapshot().run,
            RunState::Done {
                restart_pending: false,
                ..
            }
        ),
        "the direct restart finished the run"
    );
    // Several retry polls later, still one restart.
    std::thread::sleep(Duration::from_millis(1000));
    let calls = backend.calls();
    assert_eq!(count(&calls, &Call::Restart), 1, "{calls:?}");
    assert_eq!(count(&calls, &Call::RefreshIntegration), 1, "{calls:?}");
}

#[test]
fn a_fresh_install_skips_the_stop() {
    let (r, backend) = Rig::new(Opts::default());
    backend.set_recovery(recovery(RecoveryPhase::Healthy, Some(NOT_FOUND_CODE)));
    assert!(!r.svc.snapshot().herdr_installed);
    let run = r.install(None);
    assert!(matches!(run, RunState::Done { .. }), "{run:?}");
    let calls = backend.calls();
    assert_eq!(count(&calls, &Call::Shutdown), 0);
    assert_eq!(count(&calls, &Call::Restart), 1);
    assert_eq!(count(&calls, &Call::RefreshIntegration), 1);
    assert_eq!(r.cfg.pinned(), Some(r.managed_exe("0.9.6")));
    assert!(matches!(
        r.svc.snapshot().run,
        RunState::Done { run_id: 1, .. }
    ));
}

#[test]
fn an_external_herdr_is_never_replaced_but_a_managed_copy_is_offered() {
    let (r, backend) = Rig::new(Opts::default());
    backend.set_connected(true);
    let external = PathBuf::from("/opt/homebrew/bin/herdr");
    r.proc_.version_of(&external, "herdr 0.9.3");
    *r.cfg.discovered.lock().unwrap() = Some(external.clone());
    let snap = r.svc.recheck();
    assert_eq!(
        snap.route,
        Some(UpgradeRoute::External {
            instruction: "brew upgrade herdr".into()
        })
    );

    let err = r.svc.start_install(None, false).unwrap_err();
    assert_eq!(err, "brew upgrade herdr");
    assert_eq!(r.svc.snapshot().run, RunState::Idle);
    assert!(r.net.requests().is_empty());
    assert!(backend.calls().iter().all(|c| *c != Call::Shutdown));

    // "Install a Keitora-managed copy instead": tier 2 + pin.
    let run = r.install(Some(Tier::Direct));
    assert!(matches!(run, RunState::Done { .. }), "{run:?}");
    assert_eq!(r.cfg.pinned(), Some(r.managed_exe("0.9.6")));
    assert_eq!(r.svc.snapshot().route, Some(UpgradeRoute::Direct));
}

/// A vendor-installed herdr in `~/.local/bin` that Keitora installed itself.
fn standalone_rig(script_ok: bool) -> (Rig, Arc<FakeBackend>, PathBuf) {
    let (r, backend) = Rig::new(Opts {
        release_build: true,
        ..Opts::default()
    });
    backend.set_connected(true);
    let exe = r.home.join(".local").join("bin").join("herdr");
    r.binary(&exe, "0.9.3");
    let record = json!({ "keitora_installed": [exe.display().to_string()] });
    std::fs::write(r.khome.join("deps-installed.json"), record.to_string()).unwrap();
    *r.cfg.pinned.lock().unwrap() = Some(exe.clone());
    let proc_ = Arc::clone(&r.proc_);
    let target = exe.clone();
    r.proc_.on_script(move || {
        if script_ok {
            proc_.version_of(&target, "herdr 0.9.6");
            Ok(fakes::ok("installed herdr 0.9.6\n"))
        } else {
            Ok(ProcOutput {
                code: Some(1),
                stdout: "curl: (6) could not resolve host\n".into(),
                stderr: String::new(),
            })
        }
    });
    (r, backend, exe)
}

/// The script runs beside the running server, so a failed one leaves that server
/// and its pin alone: nothing is stopped or restarted.
#[test]
fn a_failed_script_upgrade_leaves_the_running_herdr_alone() {
    let (r, backend, exe) = standalone_rig(false);
    assert_eq!(r.svc.recheck().route, Some(UpgradeRoute::ScriptThenDirect));
    let run = r.install(None);
    match &run {
        RunState::Failed { kind, next, .. } => {
            assert_eq!(*kind, FailureKind::Other);
            assert_eq!(*next, NextStep::TryDirect, "a direct download is offered");
        }
        other => panic!("{other:?}"),
    }
    assert!(r.proc_.programs().contains(&"sh".to_string()));
    assert_eq!(r.cfg.pinned(), Some(exe), "the old pin stands");
    let calls = backend.calls();
    assert_eq!(count(&calls, &Call::Shutdown), 0, "{calls:?}");
    assert_eq!(count(&calls, &Call::Restart), 0, "{calls:?}");
    assert!(r.svc.details_report().contains("tier: script"));
    assert!(r
        .svc
        .details_report()
        .contains("curl: (6) could not resolve host"));
}

#[test]
fn a_successful_script_upgrade_repins_and_restarts() {
    let (r, backend, exe) = standalone_rig(true);
    r.svc.recheck();
    let run = r.install(None);
    assert_eq!(
        run,
        RunState::Done {
            run_id: 1,
            path: exe.display().to_string(),
            restart_pending: false
        }
    );
    assert_eq!(r.cfg.pinned(), Some(exe));
    let calls = backend.calls();
    assert_eq!(count(&calls, &Call::Shutdown), 0);
    assert_eq!(count(&calls, &Call::Restart), 1);
    assert_eq!(count(&calls, &Call::RefreshIntegration), 1);
    assert!(r.net.requests() == vec![MANIFEST_URL], "no download");
}

#[test]
fn the_script_never_overwrites_a_users_herdr_without_consent() {
    let (r, backend) = Rig::new(Opts {
        release_build: true,
        ..Opts::default()
    });
    backend.set_recovery(recovery(RecoveryPhase::Healthy, Some(NOT_FOUND_CODE)));
    let users = r.home.join(".local").join("bin").join("herdr");
    r.binary(&users, "0.9.3");
    let snap = r.svc.recheck();
    assert_eq!(snap.overwrite_target, Some(users.display().to_string()));

    // Not confirmed: a managed copy beside it, the script never runs.
    let run = r.install(None);
    assert!(matches!(run, RunState::Done { .. }), "{run:?}");
    assert!(!r.proc_.programs().contains(&"sh".to_string()));
    assert_eq!(r.cfg.pinned(), Some(r.managed_exe("0.9.6")));

    // Confirmed: the script replaces it, and Keitora remembers it installed it.
    *r.cfg.pinned.lock().unwrap() = None;
    let proc_ = Arc::clone(&r.proc_);
    let target = users.clone();
    r.proc_.on_script(move || {
        proc_.version_of(&target, "herdr 0.9.6");
        Ok(fakes::ok("ok\n"))
    });
    r.svc.recheck();
    r.svc.start_install(None, true).unwrap();
    let run = r.sink.wait_finished(2);
    assert!(matches!(run, RunState::Done { .. }), "{run:?}");
    assert!(r.proc_.programs().contains(&"sh".to_string()));
    assert_eq!(r.cfg.pinned(), Some(users));
    assert_eq!(r.svc.snapshot().overwrite_target, None);
}

#[test]
fn offline_and_unsupported_platforms_fail_to_the_manual_route() {
    let (r, _) = Rig::new(Opts {
        net: FakeNet::default().with(MANIFEST_URL, Reply::Fail(NetError::Offline)),
        ..Opts::default()
    });
    match r.install(None) {
        RunState::Failed { kind, next, .. } => {
            assert_eq!(kind, FailureKind::Offline);
            assert_eq!(next, NextStep::Manual);
        }
        other => panic!("{other:?}"),
    }
    let manual = r.svc.snapshot().manual;
    assert_eq!(manual.reason, Some(super::preflight::ManualReason::Offline));
    assert_eq!(manual.commands.len(), 1);

    let (r, _) = Rig::new(Opts {
        platform: None,
        ..Opts::default()
    });
    assert!(matches!(
        r.install(None),
        RunState::Failed {
            kind: FailureKind::Other,
            ..
        }
    ));
    assert!(r.net.requests().is_empty());
    assert!(r.svc.start_install(Some(Tier::Manual), false).is_err());
}

/// The real herdr backend over a fake herdr with live sessions: the upgrade restarts
/// herdr once through the resume-preserving restart, never the explicit stop, so every
/// live session is owed a resume and resumed on the new server; recovery never starts
/// a server of its own.
#[test]
fn an_upgrade_over_the_herdr_backend_resumes_every_live_session() {
    use crate::backend::herdr::agent::{agent_name, herdr_kind};
    use crate::backend::herdr::fake::FakeHerdr;
    use crate::backend::herdr::ops::Op;
    use crate::backend::herdr::{HerdrBackend, LaunchResolver};
    use crate::backend::{AgentLaunch, LaunchPolicy, SessionKey};

    let launcher: LaunchResolver = Arc::new(|k: &SessionKey| {
        Some((
            format!("/repos/{}", k.worktree),
            AgentLaunch {
                cli: CliKind::Claude,
                path: "claude".into(),
                autonomy: false,
                resume_target: None,
                fresh: false,
                policy: LaunchPolicy::Default,
            },
        ))
    });
    let fake = FakeHerdr::new();
    fake.set_server_version(Some("0.9.3"), Some(22));
    let backend = Arc::new(HerdrBackend::booted_over_fake(&fake, Some(launcher)));
    let keys: Vec<SessionKey> = ["alpha", "bravo", "charlie"]
        .iter()
        .map(|w| SessionKey {
            project: "proj".into(),
            worktree: (*w).into(),
        })
        .collect();
    for k in &keys {
        backend
            .spawn(
                k.clone(),
                format!("/repos/{}", k.worktree),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
    }
    backend.note_server_for_test(&fake);
    let floor = backend.floor_slot().expect("the herdr backend has a floor");
    let r = Rig::over(
        Arc::clone(&backend) as SharedBackend,
        floor,
        Opts::default(),
    );
    r.managed_in_use("0.9.3");
    r.floor.set(too_old("0.9.3"));
    r.svc.recheck();
    assert_eq!(backend.status_payload_snapshot().state, "Connected");
    assert_eq!(backend.live_set_snapshot().len(), keys.len());
    assert_eq!(r.svc.snapshot().live_sessions, keys.len());

    let run = r.install(None);
    assert!(
        matches!(
            run,
            RunState::Done {
                restart_pending: false,
                ..
            }
        ),
        "{run:?}"
    );
    let count_op = |op: Op| fake.calls().iter().filter(|c| c.op == op).count();
    let agents_started = || count_op(Op::AgentStart);
    eventually("every session to resume", || {
        agents_started() >= keys.len()
            && backend.recovery_snapshot().phase == RecoveryPhase::Healthy
    });
    std::thread::sleep(Duration::from_millis(200));

    // One restart: one stop of the old server, one new server.
    assert_eq!(count_op(Op::ServerStop), 1, "exactly one restart");
    assert_eq!(count_op(Op::SpawnServer), 1, "exactly one restart");
    // Every live session was owed a resume and got it.
    let mut started: Vec<String> = fake
        .calls()
        .into_iter()
        .filter(|c| c.op == Op::AgentStart)
        .map(|c| c.args.split(' ').next().unwrap_or_default().to_string())
        .collect();
    started.sort();
    let mut want: Vec<String> = keys
        .iter()
        .map(|k| agent_name(herdr_kind(CliKind::Claude), k))
        .collect();
    want.sort();
    assert_eq!(started, want);
    assert!(
        backend.owed_resume_for_test().is_empty(),
        "nothing left owed"
    );
    assert_eq!(backend.live_set_snapshot().len(), keys.len());
    // Not the explicit stop: herdr is not reported as stopped by the user.
    assert_eq!(backend.status_payload_snapshot().state, "Connected");
    assert_eq!(r.cfg.pinned(), Some(r.managed_exe("0.9.6")));
}

/// A herdr ahead of Keitora is never "upgraded": Install installs the newest
/// compatible release as a managed copy, even though it is older than the herdr in
/// use and that herdr came from a package manager.
#[test]
fn a_newer_protocol_herdr_gets_an_older_compatible_managed_copy() {
    let (r, backend) = Rig::new(Opts::default());
    backend.set_connected(true);
    let external = PathBuf::from("/opt/homebrew/bin/herdr");
    r.proc_.version_of(&external, "herdr 0.12.0");
    *r.cfg.discovered.lock().unwrap() = Some(external);
    r.floor.observe(&server("0.12.0", 23));
    let snap = r.svc.recheck();
    assert!(matches!(snap.route, Some(UpgradeRoute::External { .. })));
    assert_eq!(snap.floor, mismatch());

    let run = r.install(None);
    let managed = r.managed_exe("0.9.6");
    assert!(
        matches!(&run, RunState::Done { path, .. } if *path == managed.display().to_string()),
        "{run:?}"
    );
    assert_eq!(r.cfg.pinned(), Some(managed));
    assert_eq!(count(&backend.calls(), &Call::Restart), 1);
}

// ── Restart to finish ──

#[test]
fn restart_to_finish_restarts_and_refreshes_the_integration() {
    let (r, backend) = Rig::new(Opts::default());
    backend.set_connected(true);
    r.floor.set(too_old("0.9.3"));
    r.managed_in_use("0.9.6");
    assert!(matches!(
        r.svc.recheck().floor,
        FloorState::RestartToFinish { .. }
    ));
    r.svc.restart_to_finish().unwrap();
    let calls = backend.calls();
    let restart = calls.iter().position(|c| *c == Call::Restart).unwrap();
    let refresh = calls
        .iter()
        .position(|c| *c == Call::RefreshIntegration)
        .unwrap();
    assert!(restart < refresh, "{calls:?}");
    assert_eq!(count(&calls, &Call::Shutdown), 0);
}

// ── set_herdr_bin ──

/// An install can't start while a pin is being validated and written, so it can't
/// repin underneath it.
#[test]
fn set_herdr_bin_holds_off_installs_until_it_has_written() {
    let (r, _) = Rig::new(Opts::default());
    let exe = r.home.join("tools").join("herdr");
    r.binary(&exe, "0.9.6");
    let latch = Arc::new(Latch::default());
    *r.proc_.hold_versions.lock().unwrap() = Some(Arc::clone(&latch));
    let pinning = {
        let svc = r.svc.clone();
        let exe = exe.clone();
        std::thread::spawn(move || svc.set_herdr_bin(Some(exe)))
    };
    latch.wait_entered();
    let err = r.svc.start_install(None, false).unwrap_err();
    assert!(err.contains("being changed"), "{err}");
    assert_eq!(r.svc.snapshot().run, RunState::Idle);
    assert!(
        r.svc.set_herdr_bin(None).is_err(),
        "one pin change at a time"
    );
    *r.proc_.hold_versions.lock().unwrap() = None;
    latch.open();
    pinning.join().unwrap().unwrap();
    assert_eq!(r.cfg.pinned(), Some(exe));
    // Released: installs start again.
    assert!(matches!(
        r.install(Some(Tier::Direct)),
        RunState::Done { .. }
    ));
}

#[test]
fn set_herdr_bin_without_a_server_pins_restarts_and_refreshes() {
    let (r, backend) = Rig::new(Opts::default());
    let exe = r.home.join("tools").join("herdr");
    r.binary(&exe, "0.9.6");
    let snap = r.svc.set_herdr_bin(Some(exe.clone())).unwrap();
    assert_eq!(snap.pinned_bin, Some(exe.display().to_string()));
    let calls = backend.calls();
    assert_eq!(count(&calls, &Call::Restart), 1);
    assert_eq!(count(&calls, &Call::RefreshIntegration), 1);

    // Clearing the pin goes back to discovery, restarting again.
    let snap = r.svc.set_herdr_bin(None).unwrap();
    assert_eq!(snap.pinned_bin, None);
    assert_eq!(count(&backend.calls(), &Call::Restart), 2);
}

#[test]
fn set_herdr_bin_with_a_server_only_pins_and_shows_restart_to_finish() {
    let (r, backend) = Rig::new(Opts::default());
    backend.set_connected(true);
    r.floor.set(too_old("0.9.3"));
    let exe = r.home.join("tools").join("herdr");
    r.binary(&exe, "0.9.6");
    let snap = r.svc.set_herdr_bin(Some(exe.clone())).unwrap();
    assert_eq!(r.cfg.pinned(), Some(exe));
    assert_eq!(
        snap.floor,
        FloorState::RestartToFinish {
            binary: "0.9.6".into(),
            server: "0.9.3".into()
        }
    );
    assert_eq!(count(&backend.calls(), &Call::Restart), 0);
}

#[test]
fn set_herdr_bin_rejects_an_invalid_path_before_writing() {
    let (r, backend) = Rig::new(Opts::default());
    let old = r.home.join("old").join("herdr");
    r.binary(&old, "0.9.3");
    let not_herdr = r.home.join("tool");
    std::fs::write(&not_herdr, b"x").unwrap();
    r.proc_.version_of(&not_herdr, "tool 1.2.3");
    for (path, why) in [
        (r.home.join("missing").join("herdr"), "does not exist"),
        (old, "0.9.5 or newer"),
        (not_herdr, "doesn't look like herdr"),
    ] {
        let err = r.svc.set_herdr_bin(Some(path)).unwrap_err();
        assert!(err.contains(why), "{err}");
    }
    assert!(r.cfg.writes.lock().unwrap().is_empty(), "nothing written");
    assert_eq!(count(&backend.calls(), &Call::Restart), 0);
}

// ── Doctor ──

#[test]
fn doctor_reports_each_tool() {
    let (r, _) = Rig::new(Opts::default());
    r.managed_in_use("0.9.3");
    r.floor.set(too_old("0.9.3"));
    let rows = r.svc.doctor();
    let tools: Vec<&str> = rows.iter().map(|r| r.tool.as_str()).collect();
    assert_eq!(tools, ["herdr", "git", "claude", "codex"]);
    assert_eq!(rows[0].status, DoctorStatus::TooOld);
    assert_eq!(rows[0].action, Some(DoctorAction::Upgrade));
    assert_eq!(rows[0].version.as_deref(), Some("0.9.3"));
    assert_eq!(rows[0].floor.as_deref(), Some("0.9.5"));
    assert_eq!(rows[1].status, DoctorStatus::Ok);
    assert_eq!(rows[1].version.as_deref(), Some("2.43.0"));
    assert_eq!(rows[1].floor, None);
    assert_eq!(rows[2].status, DoctorStatus::Ok);
    assert_eq!(rows[2].path.as_deref(), Some("/opt/tools/claude"));
    assert_eq!(rows[3].status, DoctorStatus::Missing);

    let spec = r
        .proc_
        .calls
        .lock()
        .unwrap()
        .iter()
        .find(|s| s.program == Path::new("git"))
        .cloned()
        .unwrap();
    assert_eq!(spec.args, ["--version"]);
}

#[test]
fn doctor_reports_missing_herdr_and_git() {
    let (r, _) = Rig::new(Opts::default());
    *r.proc_.git.lock().unwrap() = Err(ProcError::NotFound);
    let rows = r.svc.doctor();
    assert_eq!(rows[0].status, DoctorStatus::Missing);
    assert_eq!(rows[0].action, Some(DoctorAction::Install));
    assert_eq!(rows[1].status, DoctorStatus::Missing);
    assert_eq!(rows[1].version, None);

    // With no server judgement yet, a new-enough binary is fine.
    r.managed_in_use("0.9.6");
    assert_eq!(r.svc.doctor()[0].status, DoctorStatus::Ok);
}

#[test]
fn git_version_parsing() {
    assert_eq!(
        parse_git_version("git version 2.43.0.windows.1\n").as_deref(),
        Some("2.43.0")
    );
    assert_eq!(
        parse_git_version("git version 2.39.3 (Apple Git-146)").as_deref(),
        Some("2.39.3")
    );
    assert_eq!(parse_git_version("usage: git"), None);
}

// ── Wire shapes ──

#[test]
fn snapshot_and_event_json_shapes() {
    let (r, _) = Rig::new(Opts::default());
    r.managed_in_use("0.9.3");
    r.floor.set(too_old("0.9.3"));
    let snap = serde_json::to_value(r.svc.recheck()).unwrap();
    let pinned = r.managed_exe("0.9.3").display().to_string();
    assert_eq!(
        snap,
        json!({
            "herdr_installed": true,
            "floor": { "type": "too_old", "found": "0.9.3" },
            "pinned_bin": pinned,
            "platform": "macos-aarch64",
            "run": { "type": "idle" },
            "prompt": { "kind": "too_old", "found": "0.9.3", "hard": false },
            "live_sessions": 0,
            "route": { "type": "direct" },
            "overwrite_target": null,
            "manual": {
                "reason": null,
                "commands": [{
                    "shell": "Terminal",
                    "command": "curl -fsSL https://herdr.dev/install.sh | sh"
                }],
                "link": "https://herdr.dev/docs/install/"
            }
        })
    );

    let running = RunState::Running {
        run_id: 3,
        tier: Tier::Direct,
        stage: Stage::Downloading,
        upgrade: true,
    };
    assert_eq!(
        serde_json::to_value(running).unwrap(),
        json!({ "type": "running", "run_id": 3, "tier": "direct",
                "stage": "downloading", "upgrade": true })
    );
    let done = RunState::Done {
        run_id: 3,
        path: "/x/herdr".into(),
        restart_pending: true,
    };
    assert_eq!(
        serde_json::to_value(done).unwrap(),
        json!({ "type": "done", "run_id": 3, "path": "/x/herdr", "restart_pending": true })
    );
    let failed = RunState::Failed {
        run_id: 3,
        kind: FailureKind::BlockedByAV,
        message: "m".into(),
        next: NextStep::TryDirect,
    };
    assert_eq!(
        serde_json::to_value(failed).unwrap(),
        json!({ "type": "failed", "run_id": 3, "kind": "blocked_by_av",
                "message": "m", "next": "try_direct" })
    );
    let hard = UpgradePrompt {
        kind: PromptKind::ProtocolMismatch,
        found: "23".into(),
        hard: true,
    };
    assert_eq!(
        serde_json::to_value(hard).unwrap(),
        json!({ "kind": "protocol_mismatch", "found": "23", "hard": true })
    );
    let ev = ProgressEvent {
        run_id: 3,
        stage: Stage::Downloading,
        downloaded: 10,
        total: Some(20),
        line: None,
    };
    assert_eq!(
        serde_json::to_value(ev).unwrap(),
        json!({ "run_id": 3, "stage": "downloading", "downloaded": 10,
                "total": 20, "line": null })
    );
    let route = UpgradeRoute::External {
        instruction: "brew upgrade herdr".into(),
    };
    assert_eq!(
        serde_json::to_value(route).unwrap(),
        json!({ "type": "external", "instruction": "brew upgrade herdr" })
    );
    assert_eq!(
        serde_json::to_value(UpgradeRoute::ScriptThenDirect).unwrap(),
        json!({ "type": "script_then_direct" })
    );
    // The install command's `tier` argument deserializes from the same names.
    assert_eq!(
        serde_json::from_value::<Tier>(json!("direct")).unwrap(),
        Tier::Direct
    );
    assert_eq!(STATUS_EVENT, "deps://status");
    assert_eq!(PROGRESS_EVENT, "deps://progress");
}

/// The command flow the frontend drives, through the service API.
#[test]
fn command_smoke_through_the_service() {
    let (r, backend) = Rig::new(Opts::default());
    backend.set_recovery(recovery(RecoveryPhase::Healthy, Some(NOT_FOUND_CODE)));
    let snap = r.svc.snapshot();
    assert!(!snap.herdr_installed);
    assert_eq!(snap.run, RunState::Idle);
    assert_eq!(r.svc.doctor()[0].status, DoctorStatus::Missing);

    assert!(matches!(r.install(None), RunState::Done { .. }));
    backend.set_recovery(RecoveryPayload::healthy());
    let snap = r.svc.recheck();
    assert!(snap.herdr_installed);
    assert_eq!(
        snap.pinned_bin,
        Some(r.managed_exe("0.9.6").display().to_string())
    );
    r.svc.snooze_prompt();
    let details = r.svc.details_report();
    assert!(details.contains("tier: direct"), "{details}");
    assert!(details.contains("failure: none"), "{details}");
    assert!(details.contains("platform: macos-aarch64"), "{details}");
}
