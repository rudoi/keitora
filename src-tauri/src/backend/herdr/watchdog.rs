//! `watchdog` — notice that the herdr server died.
//!
//! Two sources report a death, both through the one [`Watchdog`] front end:
//! - **Child**: the server Keitora started. A waiter thread blocks in
//!   `ServerProcess::wait()` and calls [`Watchdog::child_exited`] when it returns.
//! - **Adopted**: a server some earlier Keitora started (not our child). The status
//!   loop feeds each `herdr status server` result to [`Watchdog::report_probe`]; after
//!   [`ADOPTED_FAIL_THRESHOLD`] consecutive failures it is declared dead. A call that
//!   only failed to answer in time (or failed oddly) while the server's process is
//!   still running is a slow server, not a dead one, and does not count. An
//!   authoritative "not running" seen elsewhere goes through
//!   [`Watchdog::report_down_now`].
//!
//! Each death is sent once as `(Generation, DeathEvent)` on a channel whose single
//! consumer (the backend's death pump) records it and notes the death. A
//! deliberate stop calls [`Watchdog::expect_stop`] first, which silences every
//! report until the next server is noted with [`Watchdog::note_instance`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::headroom::{Headroom, HeadroomSnapshot};
use crate::lifecycle::ServerDeathRecord;

use super::cli::ServerStatus;
use super::ops::{ExitInfo, Generation, HerdrError, ServerInstance};

/// Consecutive failed probes before an adopted server is declared dead.
pub const ADOPTED_FAIL_THRESHOLD: u32 = 3;

/// Which observer noticed the death.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeathSource {
    /// Our own child's `wait()` returned.
    Child,
    /// An adopted server stopped answering (or reported itself not running).
    Adopted,
}

impl DeathSource {
    pub fn as_str(self) -> &'static str {
        match self {
            DeathSource::Child => "child",
            DeathSource::Adopted => "adopted",
        }
    }
}

/// One observed server death.
#[derive(Clone, Debug, PartialEq)]
pub struct DeathEvent {
    /// Child only: `ExitStatus::code()`. `None` for an adopted server, or a unix
    /// death by signal.
    pub exit_code: Option<i32>,
    /// Unix only: the terminating signal. Always `None` on Windows.
    pub signal: Option<i32>,
    pub source: DeathSource,
    pub uptime: Duration,
    /// The machine's headroom just after the death (best-effort evidence).
    pub headroom_at_death: Option<HeadroomSnapshot>,
    pub instance: ServerInstance,
    pub detected_at: SystemTime,
}

/// User-facing exit text: an NTSTATUS (negative as `i32`) as `0xC0000409`, a small
/// code as `exit N`, a unix signal as `signal N`, otherwise `unknown`.
pub fn format_exit(exit_code: Option<i32>, signal: Option<i32>) -> String {
    match (exit_code, signal) {
        (Some(c), _) if (c as u32) >= 0x8000_0000 => format!("0x{:08X}", c as u32),
        (Some(c), _) => format!("exit {c}"),
        (None, Some(s)) => format!("signal {s}"),
        (None, None) => "unknown".to_string(),
    }
}

impl From<&DeathEvent> for ServerDeathRecord {
    fn from(ev: &DeathEvent) -> Self {
        ServerDeathRecord {
            pid: Some(ev.instance.pid),
            exit_code: ev.exit_code,
            signal: ev.signal,
            source: ev.source.as_str(),
            uptime_secs: Some(ev.uptime.as_secs()),
            headroom: ev.headroom_at_death,
        }
    }
}

/// One adopted-server probe result, reduced to what liveness cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeSample {
    /// The server answered and is running.
    Up,
    /// herdr answered that no server is running.
    Refused,
    /// The status call ran out of time.
    Timeout,
    /// The CLI ran and failed for a reason that is not machine pressure.
    ExitFail,
    /// Evidence about the machine or the CLI, not the server: neither counts nor resets.
    Ignored,
}

pub fn classify_sample(r: &Result<ServerStatus, HerdrError>) -> ProbeSample {
    match r {
        Ok(s) if s.running => ProbeSample::Up,
        Ok(_) => ProbeSample::Refused,
        Err(HerdrError::Timeout) => ProbeSample::Timeout,
        Err(e @ HerdrError::Exit { .. }) if !e.is_resource_pressure() => ProbeSample::ExitFail,
        // herdr ran and printed an error body (on stderr with a non-zero exit, or
        // on stdout): the status probe failed just as a bare non-zero exit does.
        Err(HerdrError::Api { .. }) => ProbeSample::ExitFail,
        Err(HerdrError::Exit { .. })
        | Err(HerdrError::SpawnFailed { .. })
        | Err(HerdrError::Parse { .. }) => ProbeSample::Ignored,
    }
}

/// K-of-N failure counter for an adopted server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdoptedProbe {
    k: u32,
    fails: u32,
}

impl AdoptedProbe {
    pub fn new(k: u32) -> Self {
        AdoptedProbe { k, fails: 0 }
    }

    /// Feed one sample; `true` = declare the server dead (the counter then resets).
    pub fn observe(&mut self, s: ProbeSample) -> bool {
        match s {
            ProbeSample::Up => {
                self.fails = 0;
                false
            }
            ProbeSample::Refused | ProbeSample::Timeout | ProbeSample::ExitFail => {
                self.fails += 1;
                if self.fails >= self.k {
                    self.fails = 0;
                    true
                } else {
                    false
                }
            }
            ProbeSample::Ignored => false,
        }
    }
}

/// The server instance currently watched.
#[derive(Clone, Copy, Debug)]
struct Noted {
    instance: ServerInstance,
    /// Its death was already sent; further reports for it are duplicates.
    reported: bool,
}

/// Reads the current server generation.
pub type GenerationFn = Arc<dyn Fn() -> Generation + Send + Sync>;

/// The single front end for server-death signals.
pub struct Watchdog {
    tx: Sender<(Generation, DeathEvent)>,
    probe: Mutex<AdoptedProbe>,
    expecting_stop: AtomicBool,
    instance: Mutex<Option<Noted>>,
    headroom: Arc<dyn Headroom>,
    generation: GenerationFn,
}

/// Stand-in identity when the server's pid could not be resolved.
pub const UNKNOWN_INSTANCE: ServerInstance = ServerInstance {
    pid: 0,
    start_time: 0,
};

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Watchdog {
    pub fn new(
        tx: Sender<(Generation, DeathEvent)>,
        headroom: Arc<dyn Headroom>,
        generation: GenerationFn,
    ) -> Self {
        Watchdog {
            tx,
            probe: Mutex::new(AdoptedProbe::new(ADOPTED_FAIL_THRESHOLD)),
            expecting_stop: AtomicBool::new(false),
            instance: Mutex::new(None),
            headroom,
            generation,
        }
    }

    /// The current server generation (what a death report is stamped with).
    pub fn generation(&self) -> Generation {
        (self.generation)()
    }

    /// The server instance being watched, if any.
    pub fn noted_instance(&self) -> Option<ServerInstance> {
        self.instance.lock().unwrap().map(|n| n.instance)
    }

    /// Whether the watched server's death was already reported (cleared when
    /// the next server is noted).
    pub fn death_reported(&self) -> bool {
        self.instance.lock().unwrap().is_some_and(|n| n.reported)
    }

    /// Adopted-server probe input. After K consecutive failures sends one
    /// `DeathEvent{source: Adopted}`. `pid_alive`: the server's process was
    /// seen running when the call failed, so a timeout or an odd CLI failure
    /// neither counts nor resets (only a refusal or a vanished process can
    /// make an answering-slowly server dead).
    pub fn report_probe(&self, r: &Result<ServerStatus, HerdrError>, pid_alive: bool) {
        if self.expecting_stop.load(Ordering::SeqCst) {
            return;
        }
        let sample = match classify_sample(r) {
            ProbeSample::Timeout | ProbeSample::ExitFail if pid_alive => ProbeSample::Ignored,
            s => s,
        };
        let dead = self.probe.lock().unwrap().observe(sample);
        if dead {
            self.send_adopted();
        }
    }

    /// An authoritative "not running": send the adopted death at once, without K.
    pub fn report_down_now(&self) {
        if self.expecting_stop.load(Ordering::SeqCst) {
            return;
        }
        self.send_adopted();
    }

    /// Our child exited unexpectedly. Called by the child waiter only.
    pub fn child_exited(
        &self,
        generation: Generation,
        instance: ServerInstance,
        spawned_at: Instant,
        exit: ExitInfo,
    ) {
        if self.expecting_stop.load(Ordering::SeqCst) || !self.claim(instance) {
            return;
        }
        self.send(
            generation,
            DeathEvent {
                exit_code: exit.code,
                signal: exit.signal,
                source: DeathSource::Child,
                uptime: spawned_at.elapsed(),
                headroom_at_death: self.headroom.sample(),
                instance,
                detected_at: SystemTime::now(),
            },
        );
    }

    /// A deliberate stop is coming: suppress every report until [`Self::note_instance`].
    pub fn expect_stop(&self) {
        self.expecting_stop.store(true, Ordering::SeqCst);
    }

    /// The stop announced with [`Self::expect_stop`] was never issued: report
    /// again, with the watched server and its probe count as they were.
    pub fn stop_not_issued(&self) {
        self.expecting_stop.store(false, Ordering::SeqCst);
    }

    /// Whether a requested stop is in effect (no server noted since).
    pub fn stop_expected(&self) -> bool {
        self.expecting_stop.load(Ordering::SeqCst)
    }

    /// A server instance was started or adopted: watch it, lift the stop
    /// suppression and reset the probe counter.
    pub fn note_instance(&self, i: ServerInstance) {
        *self.instance.lock().unwrap() = Some(Noted {
            instance: i,
            reported: false,
        });
        *self.probe.lock().unwrap() = AdoptedProbe::new(ADOPTED_FAIL_THRESHOLD);
        self.expecting_stop.store(false, Ordering::SeqCst);
    }

    /// Mark `instance`'s death as reported; `false` if it already was.
    fn claim(&self, instance: ServerInstance) -> bool {
        let mut noted = self.instance.lock().unwrap();
        match noted.as_mut() {
            Some(n) if n.instance == instance => !std::mem::replace(&mut n.reported, true),
            _ => true,
        }
    }

    fn send_adopted(&self) {
        let instance = self.noted_instance().unwrap_or(UNKNOWN_INSTANCE);
        if !self.claim(instance) {
            return;
        }
        let uptime = match instance.start_time {
            0 => Duration::ZERO,
            t => Duration::from_secs(now_epoch_secs().saturating_sub(t)),
        };
        self.send(
            self.generation(),
            DeathEvent {
                exit_code: None,
                signal: None,
                source: DeathSource::Adopted,
                uptime,
                headroom_at_death: self.headroom.sample(),
                instance,
                detected_at: SystemTime::now(),
            },
        );
    }

    fn send(&self, generation: Generation, ev: DeathEvent) {
        log::warn!(
            "herdr: server {} died ({}, source {})",
            ev.instance.pid,
            format_exit(ev.exit_code, ev.signal),
            ev.source.as_str()
        );
        // The pump is gone only when the backend is being dropped.
        let _ = self.tx.send((generation, ev));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::headroom::{FakeHeadroom, MIB};
    use std::sync::mpsc::{self, Receiver};

    fn snap() -> HeadroomSnapshot {
        HeadroomSnapshot {
            commit_avail: 2048 * MIB,
            phys_avail: 1024 * MIB,
            pressure: None,
        }
    }

    fn watchdog(gen: u64) -> (Watchdog, Receiver<(Generation, DeathEvent)>) {
        let (tx, rx) = mpsc::channel();
        let wd = Watchdog::new(
            tx,
            Arc::new(FakeHeadroom::constant(snap())),
            Arc::new(move || Generation(gen)),
        );
        (wd, rx)
    }

    fn up() -> Result<ServerStatus, HerdrError> {
        Ok(ServerStatus {
            running: true,
            version: None,
            protocol: None,
            socket: None,
        })
    }

    fn refused() -> Result<ServerStatus, HerdrError> {
        Ok(ServerStatus {
            running: false,
            version: None,
            protocol: None,
            socket: None,
        })
    }

    const INST: ServerInstance = ServerInstance {
        pid: 4321,
        start_time: 1_700_000_000,
    };

    #[test]
    fn format_exit_renders_ntstatus_codes_and_signals() {
        assert_eq!(format_exit(Some(0xC000_0409u32 as i32), None), "0xC0000409");
        assert_eq!(format_exit(Some(1), None), "exit 1");
        assert_eq!(format_exit(Some(0), None), "exit 0");
        assert_eq!(format_exit(None, Some(9)), "signal 9");
        assert_eq!(format_exit(None, None), "unknown");
    }

    #[test]
    fn classify_sample_maps_each_result_shape() {
        assert_eq!(classify_sample(&up()), ProbeSample::Up);
        assert_eq!(classify_sample(&refused()), ProbeSample::Refused);
        assert_eq!(
            classify_sample(&Err(HerdrError::Timeout)),
            ProbeSample::Timeout
        );
        assert_eq!(
            classify_sample(&Err(HerdrError::Exit {
                code: Some(1),
                stderr: String::new(),
            })),
            ProbeSample::ExitFail
        );
        assert_eq!(
            classify_sample(&Err(HerdrError::Exit {
                code: Some(0xC000_0142u32 as i32),
                stderr: String::new(),
            })),
            ProbeSample::Ignored,
            "a CLI that could not initialise is machine pressure"
        );
        assert_eq!(
            classify_sample(&Err(HerdrError::SpawnFailed {
                os_error: Some(2),
                message: "x".into(),
            })),
            ProbeSample::Ignored
        );
        assert_eq!(
            classify_sample(&Err(HerdrError::Parse {
                message: "x".into(),
            })),
            ProbeSample::Ignored
        );
        assert_eq!(
            classify_sample(&Err(HerdrError::Api {
                code: "x".into(),
                message: "y".into(),
            })),
            ProbeSample::ExitFail,
            "an error body is a failed probe, like a bare non-zero exit"
        );
    }

    #[test]
    fn adopted_probe_needs_k_consecutive_failures() {
        let mut p = AdoptedProbe::new(3);
        assert!(!p.observe(ProbeSample::Timeout));
        assert!(!p.observe(ProbeSample::Timeout));
        assert!(!p.observe(ProbeSample::Up), "an answer resets the count");
        assert!(!p.observe(ProbeSample::Timeout));
        assert!(!p.observe(ProbeSample::Timeout));
        assert!(!p.observe(ProbeSample::Up));

        // K refusals: dead once, then the counter starts over.
        assert!(!p.observe(ProbeSample::Refused));
        assert!(!p.observe(ProbeSample::Refused));
        assert!(p.observe(ProbeSample::Refused));
        assert!(!p.observe(ProbeSample::Refused));
        assert!(!p.observe(ProbeSample::ExitFail));
        assert!(p.observe(ProbeSample::Refused));
    }

    #[test]
    fn adopted_probe_ignored_neither_counts_nor_resets() {
        let mut p = AdoptedProbe::new(3);
        assert!(!p.observe(ProbeSample::Timeout));
        assert!(!p.observe(ProbeSample::Ignored));
        assert!(!p.observe(ProbeSample::Timeout));
        assert!(p.observe(ProbeSample::Timeout));

        let mut p = AdoptedProbe::new(3);
        for _ in 0..10 {
            assert!(!p.observe(ProbeSample::Ignored));
        }
    }

    #[test]
    fn report_probe_sends_one_adopted_death_after_k_failures() {
        let (wd, rx) = watchdog(7);
        wd.note_instance(INST);
        wd.report_probe(&Err(HerdrError::Timeout), false);
        wd.report_probe(&Err(HerdrError::Timeout), false);
        assert!(rx.try_recv().is_err());
        wd.report_probe(&refused(), false);
        let (gen, ev) = rx.try_recv().expect("declared dead");
        assert_eq!(gen, Generation(7));
        assert_eq!(ev.source, DeathSource::Adopted);
        assert_eq!(ev.instance, INST);
        assert_eq!(ev.exit_code, None);
        assert_eq!(ev.headroom_at_death, Some(snap()));
        assert!(ev.uptime > Duration::ZERO, "uptime from the start time");

        // The same server is not reported twice.
        for _ in 0..6 {
            wd.report_probe(&refused(), false);
        }
        wd.report_down_now();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_missing_herdr_executable_is_never_a_death() {
        let missing = || -> Result<ServerStatus, HerdrError> {
            Err(HerdrError::SpawnFailed {
                os_error: Some(2),
                message: "No such file or directory".into(),
            })
        };
        assert!(missing().unwrap_err().is_exe_missing());
        let (wd, rx) = watchdog(7);
        wd.note_instance(INST);
        // Two real failures, then any number of calls that could not be made:
        // they neither count towards the threshold nor reset it.
        wd.report_probe(&refused(), false);
        wd.report_probe(&refused(), false);
        for _ in 0..(ADOPTED_FAIL_THRESHOLD * 3) {
            wd.report_probe(&missing(), false);
        }
        assert!(rx.try_recv().is_err(), "the server was not seen down");
        assert!(!wd.death_reported());
        wd.report_probe(&refused(), false);
        assert!(rx.try_recv().is_ok(), "the third refusal still counts");
    }

    #[test]
    fn a_stop_that_was_not_issued_resumes_reporting_for_the_same_server() {
        let (wd, rx) = watchdog(7);
        wd.note_instance(INST);
        wd.expect_stop();
        wd.report_down_now();
        assert!(rx.try_recv().is_err());
        wd.stop_not_issued();
        assert!(!wd.stop_expected());
        assert_eq!(wd.noted_instance(), Some(INST));
        wd.report_down_now();
        assert_eq!(rx.try_recv().expect("reported again").1.instance, INST);
    }

    #[test]
    fn report_probe_running_answer_resets_the_counter() {
        let (wd, rx) = watchdog(1);
        wd.note_instance(INST);
        wd.report_probe(&Err(HerdrError::Timeout), false);
        wd.report_probe(&Err(HerdrError::Timeout), false);
        wd.report_probe(&up(), false);
        wd.report_probe(&Err(HerdrError::Timeout), false);
        wd.report_probe(&Err(HerdrError::Timeout), false);
        assert!(rx.try_recv().is_err());
        wd.report_probe(&Err(HerdrError::Timeout), false);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn failures_while_the_process_runs_neither_count_nor_reset() {
        let (wd, rx) = watchdog(1);
        wd.note_instance(INST);
        let exit_fail = Err(HerdrError::Exit {
            code: Some(1),
            stderr: String::new(),
        });
        wd.report_probe(&Err(HerdrError::Timeout), false);
        for _ in 0..10 {
            wd.report_probe(&Err(HerdrError::Timeout), true);
            wd.report_probe(&exit_fail, true);
        }
        assert!(rx.try_recv().is_err(), "a slow server is not dead");
        // The earlier count survived; refusals count whatever the pid says.
        wd.report_probe(&refused(), true);
        assert!(rx.try_recv().is_err());
        wd.report_probe(&refused(), true);
        assert!(rx.try_recv().is_ok(), "the third failure");
    }

    #[test]
    fn report_down_now_sends_without_waiting_for_k() {
        let (wd, rx) = watchdog(2);
        wd.report_down_now();
        let (gen, ev) = rx.try_recv().expect("sent at once");
        assert_eq!(gen, Generation(2));
        assert_eq!(ev.source, DeathSource::Adopted);
        assert_eq!(ev.instance, UNKNOWN_INSTANCE, "nothing was noted");
        assert_eq!(ev.uptime, Duration::ZERO);
    }

    #[test]
    fn expect_stop_suppresses_every_report_until_the_next_instance() {
        let (wd, rx) = watchdog(3);
        wd.note_instance(INST);
        wd.expect_stop();
        for _ in 0..5 {
            wd.report_probe(&refused(), false);
        }
        wd.report_down_now();
        wd.child_exited(
            Generation(3),
            INST,
            Instant::now(),
            ExitInfo {
                code: Some(0),
                signal: None,
            },
        );
        assert!(rx.try_recv().is_err(), "a requested stop is not a death");

        let next = ServerInstance {
            pid: 4322,
            start_time: 1_700_000_100,
        };
        wd.note_instance(next);
        wd.report_down_now();
        let (_, ev) = rx.try_recv().expect("suppression lifted");
        assert_eq!(ev.instance, next);
    }

    #[test]
    fn child_exit_is_reported_once_and_dedups_the_adopted_report() {
        let (wd, rx) = watchdog(5);
        wd.note_instance(INST);
        let started = Instant::now() - Duration::from_secs(90);
        let exit = ExitInfo {
            code: Some(0xC000_0409u32 as i32),
            signal: None,
        };
        wd.child_exited(Generation(5), INST, started, exit);
        let (gen, ev) = rx.try_recv().expect("child death");
        assert_eq!(gen, Generation(5));
        assert_eq!(ev.source, DeathSource::Child);
        assert_eq!(ev.exit_code, Some(0xC000_0409u32 as i32));
        assert!(ev.uptime >= Duration::from_secs(90));
        assert_eq!(ev.headroom_at_death, Some(snap()));

        wd.child_exited(Generation(5), INST, started, exit);
        wd.report_down_now();
        assert!(rx.try_recv().is_err(), "one death per server instance");
    }

    #[test]
    fn death_reported_follows_the_watched_server() {
        let (wd, _rx) = watchdog(1);
        assert!(!wd.death_reported(), "nothing watched");
        wd.report_down_now();
        assert!(
            !wd.death_reported(),
            "an unknown server's death is not flagged"
        );
        wd.note_instance(INST);
        assert!(!wd.death_reported());
        wd.report_down_now();
        assert!(wd.death_reported());
        wd.note_instance(ServerInstance {
            pid: 4322,
            start_time: 1_700_000_100,
        });
        assert!(!wd.death_reported(), "the next server");
    }

    #[test]
    fn death_record_maps_the_event_fields() {
        let ev = DeathEvent {
            exit_code: Some(0xC000_0409u32 as i32),
            signal: None,
            source: DeathSource::Child,
            uptime: Duration::from_millis(61_500),
            headroom_at_death: Some(snap()),
            instance: INST,
            detected_at: SystemTime::now(),
        };
        assert_eq!(
            ServerDeathRecord::from(&ev),
            ServerDeathRecord {
                pid: Some(4321),
                exit_code: Some(0xC000_0409u32 as i32),
                signal: None,
                source: "child",
                uptime_secs: Some(61),
                headroom: Some(snap()),
            }
        );
        let adopted = DeathEvent {
            source: DeathSource::Adopted,
            exit_code: None,
            signal: Some(9),
            headroom_at_death: None,
            ..ev
        };
        let rec = ServerDeathRecord::from(&adopted);
        assert_eq!(rec.source, "adopted");
        assert_eq!(rec.signal, Some(9));
        assert_eq!(rec.headroom, None);
    }
}
