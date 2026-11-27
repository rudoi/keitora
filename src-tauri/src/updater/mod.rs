//! In-app updates: discover releases on GitHub, decide whether one applies to this build,
//! and hand the signed artifact to `tauri-plugin-updater` to verify and install.
//!
//! All decisions (status, sticky/frozen candidate, phase transitions, schedule, cleanup
//! matching, the macOS location guard) live here as plain methods and functions over a
//! [`ReleaseSource`] and a [`StatusSink`], so they test without an `AppHandle`. The plugin
//! glue is in [`install`]; the Tauri commands and the macOS menu are in [`commands`].

pub mod commands;
pub mod install;
pub mod release;
pub mod source;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use semver::Version;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use release::{assess, Assessment, Candidate, Platform};
use source::{GithubSource, ReleaseSource, SourceError};

pub const STATUS_EVENT: &str = "updater://status";
pub const PROGRESS_EVENT: &str = "updater://progress";

const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const PROGRESS_THROTTLE: Duration = Duration::from_millis(250);

/// What the frontend shows about updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UpdateStatus {
    /// No check has completed since launch.
    Unchecked,
    UpToDate,
    Available {
        version: String,
        notes: String,
        release_url: String,
        published_at: Option<String>,
    },
    Failed {
        message: String,
    },
    /// No update artifacts are published for this platform.
    Unsupported {
        /// For the user: why, and where this copy gets its updates instead.
        message: &'static str,
    },
}

impl UpdateStatus {
    fn unsupported() -> Self {
        UpdateStatus::Unsupported {
            message: release::unsupported_message(std::env::consts::OS),
        }
    }

    fn available(c: &Candidate) -> Self {
        UpdateStatus::Available {
            version: c.version.to_string(),
            notes: c.notes.clone(),
            release_url: c.release_url.clone(),
            published_at: c.published_at.clone(),
        }
    }

    /// Outcome kind for logs; never the notes or a response body.
    fn log_kind(&self) -> String {
        match self {
            UpdateStatus::Unchecked => "unchecked".into(),
            UpdateStatus::UpToDate => "up_to_date".into(),
            UpdateStatus::Available { version, .. } => format!("available {version}"),
            UpdateStatus::Failed { message } => format!("failed: {message}"),
            UpdateStatus::Unsupported { .. } => "unsupported".into(),
        }
    }
}

/// The download/install phase as the frontend sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseKind {
    Idle,
    Downloading,
    Ready,
    Applying,
}

/// `updater://status` payload: the status fields plus whether a user asked for the check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusEvent {
    #[serde(flatten)]
    pub status: UpdateStatus,
    pub manual: bool,
}

/// `updater://progress` payload, sent on (throttled) chunks and on every phase change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProgressEvent {
    pub phase: PhaseKind,
    pub downloaded: u64,
    pub total: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

/// `update_status` reply: seeds the UI after a mount or a webview reload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusSnapshot {
    pub status: UpdateStatus,
    pub phase: PhaseKind,
    pub progress: Option<Progress>,
    pub current_version: String,
}

/// Where status and progress events go: the webview in the app, a recorder in tests.
pub trait StatusSink: Send + Sync {
    fn emit_status(&self, event: &StatusEvent);
    fn emit_progress(&self, event: &ProgressEvent);
}

struct AppSink(AppHandle);

impl StatusSink for AppSink {
    fn emit_status(&self, event: &StatusEvent) {
        let _ = self.0.emit(STATUS_EVENT, event);
    }
    fn emit_progress(&self, event: &ProgressEvent) {
        let _ = self.0.emit(PROGRESS_EVENT, event);
    }
}

/// A downloaded, verified package, held in memory until it is installed.
struct Package<P> {
    version: String,
    update: P,
    bytes: Arc<Vec<u8>>,
}

enum Phase<P> {
    Idle,
    Downloading,
    Ready(Package<P>),
    Applying(Package<P>),
}

impl<P> Phase<P> {
    fn kind(&self) -> PhaseKind {
        match self {
            Phase::Idle => PhaseKind::Idle,
            Phase::Downloading => PhaseKind::Downloading,
            Phase::Ready(_) => PhaseKind::Ready,
            Phase::Applying(_) => PhaseKind::Applying,
        }
    }
}

struct Inner<P> {
    last: UpdateStatus,
    candidate: Option<Candidate>,
    phase: Phase<P>,
    progress: Option<Progress>,
    last_progress_emit: Option<Instant>,
    /// Completed checks, so a check that waited on another one can reuse its result.
    checks_done: u64,
}

impl<P> Inner<P> {
    /// Fold a fresh assessment into the candidate and derive the status to report.
    /// Sticky: a failure after `available` keeps reporting it. Frozen: while a download is
    /// in flight or waiting to be installed, the candidate is never replaced or cleared.
    fn resolve(&mut self, fresh: Assessment) -> UpdateStatus {
        let frozen = !matches!(self.phase, Phase::Idle);
        match fresh {
            Assessment::Available(c) if !frozen => self.candidate = Some(c),
            Assessment::UpToDate if !frozen => self.candidate = None,
            Assessment::Failed(message) if self.candidate.is_none() => {
                return UpdateStatus::Failed { message }
            }
            _ => {}
        }
        self.candidate
            .as_ref()
            .map_or(UpdateStatus::UpToDate, UpdateStatus::available)
    }

    fn progress_event(&self) -> ProgressEvent {
        let p = self.progress.unwrap_or(Progress {
            downloaded: 0,
            total: None,
        });
        ProgressEvent {
            phase: self.phase.kind(),
            downloaded: p.downloaded,
            total: p.total,
        }
    }
}

/// The updater's managed state. `P` is the plugin's `Update` in the app; tests use a
/// stand-in, since an `Update` can only be made by the plugin.
pub struct Updater<P = tauri_plugin_updater::Update> {
    inner: Mutex<Inner<P>>,
    check_lock: Mutex<()>,
    /// Callers blocked on `check_lock` (observable so overlap is testable).
    waiting: AtomicUsize,
    source: Box<dyn ReleaseSource>,
    sink: Box<dyn StatusSink>,
    current: Version,
    platform: Option<Platform>,
}

impl<P: Clone> Updater<P> {
    pub fn new(
        source: Box<dyn ReleaseSource>,
        sink: Box<dyn StatusSink>,
        current: Version,
        platform: Option<Platform>,
    ) -> Self {
        let last = if platform.is_some() {
            UpdateStatus::Unchecked
        } else {
            UpdateStatus::unsupported()
        };
        Updater {
            inner: Mutex::new(Inner {
                last,
                candidate: None,
                phase: Phase::Idle,
                progress: None,
                last_progress_emit: None,
                checks_done: 0,
            }),
            check_lock: Mutex::new(()),
            waiting: AtomicUsize::new(0),
            source,
            sink,
            current,
            platform,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner<P>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit_status(&self, status: &UpdateStatus, manual: bool) {
        self.sink.emit_status(&StatusEvent {
            status: status.clone(),
            manual,
        });
    }

    /// Check for a release. Overlapping checks serialize, and one that waited on another
    /// returns that check's result instead of asking again. Background checks emit only on a
    /// change; manual ones always emit.
    pub fn check(&self, manual: bool) -> UpdateStatus {
        let seen = self.lock().checks_done;
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let _turn = self
            .check_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.waiting.fetch_sub(1, Ordering::SeqCst);

        let reused = {
            let inner = self.lock();
            (inner.checks_done != seen).then(|| inner.last.clone())
        };
        if let Some(status) = reused {
            if manual {
                self.emit_status(&status, true);
            }
            return status;
        }

        let fresh = self.platform.map(|platform| match self.source.latest() {
            Ok(None) => Assessment::UpToDate,
            Ok(Some(json)) => assess(&json, &self.current, platform),
            Err(e) => Assessment::Failed(e.to_string()),
        });

        let (status, changed) = {
            let mut inner = self.lock();
            let status = match fresh {
                Some(fresh) => inner.resolve(fresh),
                None => UpdateStatus::unsupported(),
            };
            inner.checks_done += 1;
            let changed = inner.last != status;
            inner.last = status.clone();
            (status, changed)
        };
        log::info!(
            "[updater] {} check: {}",
            if manual { "manual" } else { "background" },
            status.log_kind()
        );
        if manual || changed {
            self.emit_status(&status, manual);
        }
        status
    }

    /// Whether an update is being installed right now (the window must not close mid-swap).
    pub fn is_applying(&self) -> bool {
        self.lock().phase.kind() == PhaseKind::Applying
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        let inner = self.lock();
        StatusSnapshot {
            status: inner.last.clone(),
            phase: inner.phase.kind(),
            progress: inner.progress,
            current_version: self.current.to_string(),
        }
    }

    /// Fetch a small text asset (the `.sig`) through the release source.
    pub fn fetch_text(&self, url: &str) -> Result<String, SourceError> {
        self.source.fetch_text(url)
    }

    /// Start a download of the current candidate. `Ok(None)` means it is already downloaded.
    pub fn begin_download(&self) -> Result<Option<Candidate>, String> {
        let event = {
            let mut inner = self.lock();
            let Some(candidate) = inner.candidate.clone() else {
                return Err("No update is available.".into());
            };
            match inner.phase {
                Phase::Idle => {}
                Phase::Ready(_) => return Ok(None),
                Phase::Downloading => return Err("The update is already downloading.".into()),
                Phase::Applying(_) => return Err("The update is already being applied.".into()),
            }
            inner.phase = Phase::Downloading;
            inner.progress = Some(Progress {
                downloaded: 0,
                total: None,
            });
            inner.last_progress_emit = None;
            (inner.progress_event(), candidate)
        };
        self.sink.emit_progress(&event.0);
        Ok(Some(event.1))
    }

    /// Account for one downloaded chunk; emits at most every 250 ms.
    pub fn record_progress(&self, chunk: usize, total: Option<u64>, now: Instant) {
        let event = {
            let mut inner = self.lock();
            if !matches!(inner.phase, Phase::Downloading) {
                return;
            }
            let p = inner.progress.get_or_insert(Progress {
                downloaded: 0,
                total: None,
            });
            p.downloaded += chunk as u64;
            p.total = total;
            let due = match inner.last_progress_emit {
                Some(at) => now.duration_since(at) >= PROGRESS_THROTTLE,
                None => true,
            };
            if !due {
                return;
            }
            inner.last_progress_emit = Some(now);
            inner.progress_event()
        };
        self.sink.emit_progress(&event);
    }

    /// The download verified: hold the package until it is applied.
    pub fn finish_download(&self, version: String, update: P, bytes: Arc<Vec<u8>>) {
        let event = {
            let mut inner = self.lock();
            inner.phase = Phase::Ready(Package {
                version,
                update,
                bytes,
            });
            inner.progress_event()
        };
        self.sink.emit_progress(&event);
    }

    pub fn fail_download(&self) {
        let event = {
            let mut inner = self.lock();
            inner.phase = Phase::Idle;
            inner.progress = None;
            inner.progress_event()
        };
        self.sink.emit_progress(&event);
    }

    /// Move a ready package to `Applying`, handing out what the installer needs. The
    /// package stays in the phase so a failed install can return to `Ready`.
    pub fn begin_apply(&self) -> Result<(P, Arc<Vec<u8>>), String> {
        let (out, event) = {
            let mut inner = self.lock();
            let pkg = match std::mem::replace(&mut inner.phase, Phase::Idle) {
                Phase::Ready(pkg) => pkg,
                other => {
                    let message = match other {
                        Phase::Downloading => "The update is still downloading.",
                        Phase::Applying(_) => "The update is already being applied.",
                        _ => "The update hasn't been downloaded yet.",
                    };
                    inner.phase = other;
                    return Err(message.into());
                }
            };
            log::info!("[updater] applying {}", pkg.version);
            let out = (pkg.update.clone(), Arc::clone(&pkg.bytes));
            inner.phase = Phase::Applying(pkg);
            (out, inner.progress_event())
        };
        self.sink.emit_progress(&event);
        Ok(out)
    }

    /// The install failed (or was refused): back to `Ready` so the user can retry.
    pub fn fail_apply(&self) {
        let event = {
            let mut inner = self.lock();
            match std::mem::replace(&mut inner.phase, Phase::Idle) {
                Phase::Applying(pkg) => inner.phase = Phase::Ready(pkg),
                other => inner.phase = other,
            }
            inner.progress_event()
        };
        self.sink.emit_progress(&event);
    }
}

/// Sleep before the `n`th scheduled check: 30 s after launch, then every 6 h.
pub fn delay_before(n: u32) -> Duration {
    if n == 0 {
        FIRST_CHECK_DELAY
    } else {
        CHECK_INTERVAL
    }
}

/// Background checks run only in release builds on a platform with update artifacts.
pub fn schedule_enabled(debug_build: bool, platform: Option<Platform>) -> bool {
    !debug_build && platform.is_some()
}

/// The `[updates] auto_check` gate, re-read before every scheduled check. An unreadable
/// config falls back to the default (on).
pub fn auto_check_enabled<E>(config: Result<keitora_tasks::UserConfig, E>) -> bool {
    config.map_or(true, |c| c.updates.auto_check())
}

/// The plugin's Windows temp folders, `{app}-{version}-updater-XXXX`, left behind because
/// the process exits mid-install.
pub fn is_plugin_leftover(name: &str, app_name: &str) -> bool {
    name.starts_with(&format!("{app_name}-")) && name.contains("-updater-")
}

/// Running from a mounted dmg or under App Translocation, where the bundle can't be
/// replaced in place.
pub fn is_unsafe_mac_location(exe: &Path) -> bool {
    let path = exe.to_string_lossy();
    path.starts_with("/Volumes/") || path.contains("/AppTranslocation/")
}

/// Best-effort removal of leftover plugin temp folders in `dir`. Returns how many went.
fn cleanup_leftovers(dir: &Path, app_name: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| is_plugin_leftover(&e.file_name().to_string_lossy(), app_name))
        .filter(|e| std::fs::remove_dir_all(e.path()).is_ok())
        .count()
}

/// Register the updater: crypto provider, managed state, leftover cleanup and (in release
/// builds on supported platforms) the background schedule.
pub fn startup(app: AppHandle) {
    source::install_crypto_provider();
    let info = app.package_info();
    let current = info.version.clone();
    let app_name = info.name.clone();
    let platform = Platform::current();
    let updater: Updater = Updater::new(
        Box::new(GithubSource::new(&current.to_string())),
        Box::new(AppSink(app.clone())),
        current,
        platform,
    );
    app.manage(updater);

    thread::spawn(move || {
        let removed = cleanup_leftovers(&std::env::temp_dir(), &app_name);
        if removed > 0 {
            log::info!("[updater] removed {removed} leftover installer folder(s)");
        }
    });

    if schedule_enabled(cfg!(debug_assertions), platform) {
        spawn_schedule(app);
    }
}

/// Lives until the process exits; `AppHandle` has no weak form to stop on.
fn spawn_schedule(app: AppHandle) {
    let spawned = thread::Builder::new()
        .name("updater-schedule".into())
        .spawn(move || {
            let mut n: u32 = 0;
            loop {
                thread::sleep(delay_before(n));
                n = n.saturating_add(1);
                if auto_check_enabled(keitora_tasks::UserConfig::load()) {
                    app.state::<Updater>().check(false);
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("[updater] couldn't start the update schedule: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::time::Duration;

    type Reply = Result<Option<String>, SourceError>;
    /// Signals entry into `latest`, then waits for a release.
    type Gate = (Mutex<mpsc::Sender<()>>, Mutex<mpsc::Receiver<()>>);

    #[derive(Default)]
    struct FakeSource {
        replies: Mutex<VecDeque<Reply>>,
        calls: Arc<AtomicUsize>,
        gate: Option<Gate>,
    }

    impl ReleaseSource for FakeSource {
        fn latest(&self) -> Reply {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some((entered, release)) = &self.gate {
                entered.lock().unwrap().send(()).unwrap();
                release.lock().unwrap().recv().unwrap();
            }
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected latest() call")
        }
        fn fetch_text(&self, _url: &str) -> Result<String, SourceError> {
            Ok("c2ln".into())
        }
    }

    #[derive(Default)]
    struct Recorded {
        statuses: Mutex<Vec<StatusEvent>>,
        progress: Mutex<Vec<ProgressEvent>>,
    }

    struct FakeSink(Arc<Recorded>);

    impl StatusSink for FakeSink {
        fn emit_status(&self, event: &StatusEvent) {
            self.0.statuses.lock().unwrap().push(event.clone());
        }
        fn emit_progress(&self, event: &ProgressEvent) {
            self.0.progress.lock().unwrap().push(*event);
        }
    }

    impl Recorded {
        fn statuses(&self) -> Vec<StatusEvent> {
            self.statuses.lock().unwrap().clone()
        }
        fn progress(&self) -> Vec<ProgressEvent> {
            self.progress.lock().unwrap().clone()
        }
    }

    type TestUpdater = Updater<String>;

    fn source(replies: Vec<Reply>) -> FakeSource {
        FakeSource {
            replies: Mutex::new(replies.into()),
            ..FakeSource::default()
        }
    }

    fn updater_with(src: FakeSource, platform: Option<Platform>) -> (TestUpdater, Arc<Recorded>) {
        let rec = Arc::new(Recorded::default());
        let u = Updater::new(
            Box::new(src),
            Box::new(FakeSink(Arc::clone(&rec))),
            Version::parse("0.4.0").unwrap(),
            platform,
        );
        (u, rec)
    }

    fn updater(replies: Vec<Reply>) -> (TestUpdater, Arc<Recorded>) {
        updater_with(source(replies), Some(Platform::WindowsX64))
    }

    fn release_json(ver: &str) -> Reply {
        let exe = format!("Keitora_{ver}_x64-setup.exe");
        let assets: Vec<_> = [exe.clone(), format!("{exe}.sig")]
            .iter()
            .map(|n| serde_json::json!({"name": n, "browser_download_url": format!("https://example.com/{n}")}))
            .collect();
        Ok(Some(
            serde_json::json!({
                "tag_name": format!("v{ver}"),
                "body": "notes",
                "published_at": "2026-11-01T12:00:00Z",
                "html_url": format!("https://example.com/releases/v{ver}"),
                "assets": assets,
            })
            .to_string(),
        ))
    }

    fn available(ver: &str) -> UpdateStatus {
        UpdateStatus::Available {
            version: ver.into(),
            notes: "notes".into(),
            release_url: format!("https://example.com/releases/v{ver}"),
            published_at: Some("2026-11-01T12:00:00Z".into()),
        }
    }

    fn event(status: UpdateStatus, manual: bool) -> StatusEvent {
        StatusEvent { status, manual }
    }

    fn version_of(c: &Candidate) -> String {
        c.version.to_string()
    }

    // ---- status and checks ----

    #[test]
    fn background_checks_emit_only_on_change() {
        let (u, rec) = updater(vec![
            release_json("0.4.1"),
            release_json("0.4.1"),
            release_json("0.4.0"),
        ]);
        assert_eq!(u.check(false), available("0.4.1"));
        assert_eq!(u.check(false), available("0.4.1"));
        assert_eq!(u.check(false), UpdateStatus::UpToDate);
        assert_eq!(
            rec.statuses(),
            vec![
                event(available("0.4.1"), false),
                event(UpdateStatus::UpToDate, false)
            ]
        );
    }

    #[test]
    fn manual_checks_always_emit() {
        let (u, rec) = updater(vec![release_json("0.4.0"), release_json("0.4.0")]);
        u.check(true);
        u.check(true);
        assert_eq!(
            rec.statuses(),
            vec![
                event(UpdateStatus::UpToDate, true),
                event(UpdateStatus::UpToDate, true)
            ]
        );
    }

    #[test]
    fn a_failure_after_available_keeps_reporting_available() {
        let (u, rec) = updater(vec![release_json("0.4.1"), Err(SourceError::Offline)]);
        u.check(false);
        assert_eq!(u.check(false), available("0.4.1"));
        assert_eq!(rec.statuses().len(), 1, "no change, no emit");
    }

    #[test]
    fn a_failure_without_a_candidate_is_failed() {
        let (u, _) = updater(vec![Err(SourceError::Offline)]);
        assert_eq!(
            u.check(false),
            UpdateStatus::Failed {
                message: "offline".into()
            }
        );
    }

    #[test]
    fn not_found_means_up_to_date() {
        let (u, _) = updater(vec![Ok(None)]);
        assert_eq!(u.check(true), UpdateStatus::UpToDate);
    }

    #[test]
    fn rate_limited_is_a_soft_failure() {
        let (u, rec) = updater(vec![Err(SourceError::RateLimited)]);
        let status = u.check(true);
        assert!(
            matches!(&status, UpdateStatus::Failed { message } if message.contains("rate limited")),
            "{status:?}"
        );
        assert_eq!(rec.statuses(), vec![event(status, true)]);
    }

    #[test]
    fn a_bad_release_is_failed_and_logged_as_such() {
        let (u, _) = updater(vec![Ok(Some("{nope".into()))]);
        assert_eq!(
            u.check(false),
            UpdateStatus::Failed {
                message: "malformed release".into()
            }
        );
    }

    #[test]
    fn unsupported_platform_never_asks_the_source() {
        let src = FakeSource::default();
        let calls = Arc::clone(&src.calls);
        let (u, rec) = updater_with(src, None);
        assert_eq!(u.snapshot().status, UpdateStatus::unsupported());
        assert_eq!(u.check(true), UpdateStatus::unsupported());
        assert_eq!(
            rec.statuses(),
            vec![event(UpdateStatus::unsupported(), true)]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_supported_platform_starts_unchecked() {
        let (u, _) = updater(vec![]);
        assert_eq!(
            u.snapshot(),
            StatusSnapshot {
                status: UpdateStatus::Unchecked,
                phase: PhaseKind::Idle,
                progress: None,
                current_version: "0.4.0".into(),
            }
        );
    }

    #[test]
    fn overlapping_checks_share_one_request() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let src = FakeSource {
            replies: Mutex::new(vec![release_json("0.4.1")].into()),
            gate: Some((Mutex::new(entered_tx), Mutex::new(release_rx))),
            ..FakeSource::default()
        };
        let calls = Arc::clone(&src.calls);
        let (u, rec) = updater_with(src, Some(Platform::WindowsX64));
        let u = Arc::new(u);

        let first = {
            let u = Arc::clone(&u);
            thread::spawn(move || u.check(false))
        };
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first check reached the source");
        let second = {
            let u = Arc::clone(&u);
            thread::spawn(move || u.check(true))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while u.waiting.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "second check never queued");
            thread::sleep(Duration::from_millis(1));
        }
        release_tx.send(()).unwrap();

        assert_eq!(first.join().unwrap(), available("0.4.1"));
        assert_eq!(second.join().unwrap(), available("0.4.1"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the waiting check reused the result"
        );
        assert_eq!(
            rec.statuses(),
            vec![
                event(available("0.4.1"), false),
                event(available("0.4.1"), true)
            ]
        );
    }

    // ---- candidate freezing ----

    #[test]
    fn the_candidate_is_frozen_while_downloading_and_ready() {
        let (u, _) = updater(vec![
            release_json("0.4.1"),
            release_json("0.4.2"),
            release_json("0.4.0"),
            release_json("0.4.3"),
        ]);
        u.check(false);
        let c = u.begin_download().unwrap().unwrap();
        assert_eq!(version_of(&c), "0.4.1");

        assert_eq!(u.check(false), available("0.4.1"), "newer release ignored");
        assert_eq!(u.check(false), available("0.4.1"), "not cleared either");

        u.finish_download("0.4.1".into(), "pkg".into(), Arc::new(vec![1]));
        assert_eq!(
            u.check(false),
            available("0.4.1"),
            "still frozen when ready"
        );
        assert_eq!(u.begin_download(), Ok(None), "ready is ready");
    }

    #[test]
    fn the_candidate_moves_on_once_idle_again() {
        let (u, _) = updater(vec![release_json("0.4.1"), release_json("0.4.2")]);
        u.check(false);
        u.begin_download().unwrap();
        u.fail_download();
        assert_eq!(u.check(false), available("0.4.2"));
        assert_eq!(version_of(&u.begin_download().unwrap().unwrap()), "0.4.2");
    }

    // ---- phases ----

    #[test]
    fn download_needs_a_candidate() {
        let (u, rec) = updater(vec![]);
        assert_eq!(u.begin_download(), Err("No update is available.".into()));
        assert!(rec.progress().is_empty());
    }

    #[test]
    fn download_and_apply_are_refused_in_the_wrong_phase() {
        let (u, _) = updater(vec![release_json("0.4.1")]);
        u.check(false);
        assert_eq!(
            u.begin_apply(),
            Err("The update hasn't been downloaded yet.".into())
        );
        u.begin_download().unwrap();
        assert_eq!(
            u.begin_download(),
            Err("The update is already downloading.".into())
        );
        assert_eq!(
            u.begin_apply(),
            Err("The update is still downloading.".into())
        );
        u.finish_download("0.4.1".into(), "pkg".into(), Arc::new(vec![7]));
        u.begin_apply().unwrap();
        assert_eq!(
            u.begin_download(),
            Err("The update is already being applied.".into())
        );
        assert_eq!(
            u.begin_apply(),
            Err("The update is already being applied.".into())
        );
        assert_eq!(u.snapshot().phase, PhaseKind::Applying);
    }

    #[test]
    fn a_full_download_and_apply_walks_the_phases() {
        let (u, rec) = updater(vec![release_json("0.4.1")]);
        u.check(false);
        u.begin_download().unwrap();
        let t0 = Instant::now();
        u.record_progress(100, Some(1000), t0);
        u.record_progress(100, Some(1000), t0 + Duration::from_millis(100));
        u.record_progress(100, Some(1000), t0 + Duration::from_millis(300));
        assert_eq!(
            u.snapshot().progress,
            Some(Progress {
                downloaded: 300,
                total: Some(1000)
            })
        );
        u.finish_download("0.4.1".into(), "pkg".into(), Arc::new(vec![1, 2, 3]));
        assert!(!u.is_applying(), "ready is not applying");
        let (pkg, bytes) = u.begin_apply().unwrap();
        assert!(u.is_applying());
        assert_eq!(pkg, "pkg");
        assert_eq!(*bytes, vec![1, 2, 3]);
        u.fail_apply();
        assert!(!u.is_applying());
        assert_eq!(
            u.snapshot().phase,
            PhaseKind::Ready,
            "retry keeps the bytes"
        );
        let (_, again) = u.begin_apply().unwrap();
        assert!(Arc::ptr_eq(&bytes, &again));

        let p = |phase, downloaded, total| ProgressEvent {
            phase,
            downloaded,
            total,
        };
        assert_eq!(
            rec.progress(),
            vec![
                p(PhaseKind::Downloading, 0, None),
                p(PhaseKind::Downloading, 100, Some(1000)),
                // the 100 ms chunk is throttled
                p(PhaseKind::Downloading, 300, Some(1000)),
                p(PhaseKind::Ready, 300, Some(1000)),
                p(PhaseKind::Applying, 300, Some(1000)),
                p(PhaseKind::Ready, 300, Some(1000)),
                p(PhaseKind::Applying, 300, Some(1000)),
            ]
        );
    }

    #[test]
    fn a_failed_download_resets_to_idle_without_progress() {
        let (u, rec) = updater(vec![release_json("0.4.1")]);
        u.check(false);
        u.begin_download().unwrap();
        u.record_progress(10, None, Instant::now());
        u.fail_download();
        let snap = u.snapshot();
        assert_eq!(snap.phase, PhaseKind::Idle);
        assert_eq!(snap.progress, None);
        assert_eq!(
            rec.progress().last(),
            Some(&ProgressEvent {
                phase: PhaseKind::Idle,
                downloaded: 0,
                total: None
            })
        );
    }

    #[test]
    fn progress_outside_a_download_is_ignored() {
        let (u, rec) = updater(vec![]);
        u.record_progress(10, Some(10), Instant::now());
        assert!(rec.progress().is_empty());
        assert_eq!(u.snapshot().progress, None);
    }

    #[test]
    fn fail_apply_outside_applying_keeps_the_phase() {
        let (u, _) = updater(vec![]);
        u.fail_apply();
        assert_eq!(u.snapshot().phase, PhaseKind::Idle);
    }

    // ---- wire shapes ----

    #[test]
    fn status_event_wire_shape() {
        assert_eq!(
            serde_json::to_value(event(available("0.4.1"), true)).unwrap(),
            serde_json::json!({
                "type": "available",
                "version": "0.4.1",
                "notes": "notes",
                "release_url": "https://example.com/releases/v0.4.1",
                "published_at": "2026-11-01T12:00:00Z",
                "manual": true,
            })
        );
        assert_eq!(
            serde_json::to_value(event(
                UpdateStatus::Failed {
                    message: "offline".into()
                },
                false
            ))
            .unwrap(),
            serde_json::json!({"type": "failed", "message": "offline", "manual": false})
        );
        assert_eq!(
            serde_json::to_value(UpdateStatus::UpToDate).unwrap(),
            serde_json::json!({"type": "up_to_date"})
        );
    }

    #[test]
    fn snapshot_and_progress_wire_shapes() {
        let snap = StatusSnapshot {
            status: UpdateStatus::Unchecked,
            phase: PhaseKind::Downloading,
            progress: Some(Progress {
                downloaded: 5,
                total: None,
            }),
            current_version: "0.4.0".into(),
        };
        assert_eq!(
            serde_json::to_value(snap).unwrap(),
            serde_json::json!({
                "status": {"type": "unchecked"},
                "phase": "downloading",
                "progress": {"downloaded": 5, "total": null},
                "current_version": "0.4.0",
            })
        );
        assert_eq!(
            serde_json::to_value(ProgressEvent {
                phase: PhaseKind::Ready,
                downloaded: 9,
                total: Some(9)
            })
            .unwrap(),
            serde_json::json!({"phase": "ready", "downloaded": 9, "total": 9})
        );
    }

    // ---- pure helpers ----

    #[test]
    fn schedule_waits_30s_then_6h() {
        assert_eq!(delay_before(0), Duration::from_secs(30));
        assert_eq!(delay_before(1), Duration::from_secs(6 * 3600));
        assert_eq!(delay_before(u32::MAX), Duration::from_secs(6 * 3600));
    }

    #[test]
    fn schedule_runs_only_in_release_builds_on_supported_platforms() {
        assert!(schedule_enabled(false, Some(Platform::WindowsX64)));
        assert!(schedule_enabled(false, Some(Platform::MacArm64)));
        assert!(!schedule_enabled(true, Some(Platform::WindowsX64)));
        assert!(!schedule_enabled(false, None));
    }

    #[test]
    fn auto_check_gate_follows_the_setting() {
        let mut cfg = keitora_tasks::UserConfig::default();
        assert!(auto_check_enabled::<()>(Ok(cfg.clone())));
        cfg.updates.auto_check = Some(false);
        assert!(!auto_check_enabled::<()>(Ok(cfg.clone())));
        cfg.updates.auto_check = Some(true);
        assert!(auto_check_enabled::<()>(Ok(cfg)));
        assert!(auto_check_enabled(Err("unreadable")));
    }

    #[test]
    fn leftover_matcher() {
        assert!(is_plugin_leftover(
            "Keitora-0.4.0-updater-AbC123",
            "Keitora"
        ));
        assert!(!is_plugin_leftover("Keitora-foo", "Keitora"));
        assert!(!is_plugin_leftover("Other-0.4.0-updater-x", "Keitora"));
        assert!(!is_plugin_leftover("KeitoraX-0.4.0-updater-x", "Keitora"));
    }

    #[test]
    fn cleanup_removes_only_leftover_folders() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str| std::fs::create_dir(dir.path().join(name)).unwrap();
        mk("Keitora-0.4.0-updater-AbC123");
        mk("Keitora-0.4.1-updater-x");
        mk("Keitora-foo");
        mk("Other-0.4.0-updater-x");
        std::fs::write(dir.path().join("Keitora-0.4.0-updater-file"), b"x").unwrap();
        assert_eq!(cleanup_leftovers(dir.path(), "Keitora"), 2);
        let mut left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "Keitora-0.4.0-updater-file",
                "Keitora-foo",
                "Other-0.4.0-updater-x"
            ]
        );
        assert_eq!(cleanup_leftovers(&dir.path().join("missing"), "Keitora"), 0);
    }

    #[test]
    fn mac_location_guard() {
        let p = Path::new;
        assert!(is_unsafe_mac_location(p(
            "/Volumes/Keitora/Keitora.app/Contents/MacOS/keitora-app"
        )));
        assert!(is_unsafe_mac_location(p(
            "/private/var/folders/x/T/AppTranslocation/ABC/d/Keitora.app/Contents/MacOS/keitora-app"
        )));
        assert!(!is_unsafe_mac_location(p(
            "/Applications/Keitora.app/Contents/MacOS/keitora-app"
        )));
        assert!(!is_unsafe_mac_location(p(
            "/Users/me/Applications/Keitora.app/Contents/MacOS/keitora-app"
        )));
        assert!(!is_unsafe_mac_location(p("/Users/me/Volumes/keitora-app")));
    }
}

#[cfg(test)]
mod config_tests {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use serde_json::Value;

    /// The literal the config may carry until a real key is generated.
    const PUBKEY_PLACEHOLDER: &str = "REPLACE_WITH_UPDATER_PUBKEY";

    fn tauri_conf() -> Value {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tauri.conf.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Tauri stores the minisign public key file text base64-encoded; the key line inside is
    /// base64 of `Ed` + 8-byte key id + 32-byte key.
    fn is_minisign_pubkey(b64: &str) -> bool {
        let Ok(text) = STANDARD.decode(b64.trim()) else {
            return false;
        };
        let Ok(text) = String::from_utf8(text) else {
            return false;
        };
        let Some(line) = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("untrusted comment:"))
        else {
            return false;
        };
        STANDARD
            .decode(line)
            .is_ok_and(|key| key.len() == 42 && key.starts_with(b"Ed"))
    }

    #[test]
    fn minisign_pubkey_check_rejects_junk() {
        assert!(!is_minisign_pubkey(""));
        assert!(!is_minisign_pubkey("not base64!"));
        assert!(!is_minisign_pubkey(
            &STANDARD.encode("untrusted comment: x\nAAAA\n")
        ));
    }

    #[test]
    fn updater_config_is_wired_for_in_app_updates() {
        let conf = tauri_conf();
        let updater = &conf["plugins"]["updater"];

        let pubkey = updater["pubkey"].as_str().expect("updater pubkey present");
        assert!(
            pubkey == PUBKEY_PLACEHOLDER || is_minisign_pubkey(pubkey),
            "pubkey must be a minisign public key or the placeholder"
        );
        assert_eq!(updater["windows"]["installMode"], "passive");
        assert!(
            updater.get("endpoints").is_none(),
            "endpoints are set in code, never in config"
        );
    }

    #[test]
    fn updater_artifacts_are_left_to_the_ci_overlay() {
        let bundle = &tauri_conf()["bundle"];
        assert!(
            bundle.get("createUpdaterArtifacts").is_none(),
            "a committed flag would make every local build need the private key"
        );
        let targets: Vec<&str> = bundle["targets"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(!targets.contains(&"app"), "the release overlay adds `app`");
    }
}
