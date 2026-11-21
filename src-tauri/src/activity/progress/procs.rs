//! `procs.rs` — live Unreal Build Tool process evidence for the build-progress engine.
//!
//! The shared UBT log alone cannot tell a cancelled build (no `Result:` ever written) from a
//! quiet link step, nor which worktree a build whose header was missed belongs to. The
//! [`UbtProbe`] answers "which build-mode UBT processes are alive, and for which project",
//! read-only, from the process list. [`SysinfoProbe`] keeps that cheap: it refreshes only
//! process names, reads command lines only for UBT candidates, and caches its answer for
//! [`CACHE_FOR`]. The engine only asks while a run is live or the log was written recently.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::ubt;
use crate::backend::herdr::real::cmd_of;

/// A probe answer is reused for at least this long (a call inside the window returns the
/// previous snapshot with the same `seq`).
pub const CACHE_FOR: Duration = Duration::from_secs(2);

/// One live build-mode UBT process.
#[derive(Debug, Clone, PartialEq)]
pub struct UbtProc {
    pub pid: u32,
    /// The normalized `-Project=` path; `None` = unknown (command line unreadable, or no
    /// `-Project=` argument).
    pub project: Option<String>,
}

/// One probe answer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProbeSnapshot {
    /// Increases by one on every real refresh (not when answered from the cache), so the
    /// engine can tell a new observation from a repeated one.
    pub seq: u64,
    /// The live build-mode UBT processes; `None` when the process list could not be read.
    pub procs: Option<Vec<UbtProc>>,
}

/// Source of live UBT process evidence (a fake in tests).
pub trait UbtProbe: Send + Sync {
    fn snapshot(&self) -> ProbeSnapshot;
}

/// The real probe: a read-only scan of the process list via `sysinfo`.
pub struct SysinfoProbe {
    state: Mutex<ProbeState>,
}

struct ProbeState {
    sys: sysinfo::System,
    cache: SnapshotCache,
}

impl Default for SysinfoProbe {
    fn default() -> Self {
        SysinfoProbe {
            state: Mutex::new(ProbeState {
                sys: sysinfo::System::new(),
                cache: SnapshotCache::default(),
            }),
        }
    }
}

impl UbtProbe for SysinfoProbe {
    fn snapshot(&self) -> ProbeSnapshot {
        let mut state = crate::activity::lock(&self.state);
        let ProbeState { sys, cache } = &mut *state;
        cache.get_or_refresh(Instant::now(), || scan(sys))
    }
}

/// The last snapshot and when it was taken.
#[derive(Default)]
struct SnapshotCache {
    seq: u64,
    last: Option<(Instant, ProbeSnapshot)>,
}

impl SnapshotCache {
    /// The cached snapshot while it is younger than [`CACHE_FOR`]; otherwise a fresh one
    /// from `refresh` under the next `seq`.
    fn get_or_refresh(
        &mut self,
        now: Instant,
        refresh: impl FnOnce() -> Option<Vec<UbtProc>>,
    ) -> ProbeSnapshot {
        if let Some((at, snap)) = &self.last {
            if now.saturating_duration_since(*at) < CACHE_FOR {
                return snap.clone();
            }
        }
        self.seq += 1;
        let snap = ProbeSnapshot {
            seq: self.seq,
            procs: refresh(),
        };
        self.last = Some((now, snap.clone()));
        snap
    }
}

/// What a process name makes of a process before its command line is read.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Candidate {
    /// `UnrealBuildTool[.exe]`: a UBT by name.
    UbtExe,
    /// `dotnet[.exe]`: a UBT only if it hosts `UnrealBuildTool.dll`.
    Dotnet,
}

/// Classify a process name (case-insensitive, with or without `.exe`).
fn candidate_of(name: &str) -> Option<Candidate> {
    let lower = name.to_ascii_lowercase();
    let base = lower.strip_suffix(".exe").unwrap_or(&lower);
    match base {
        "unrealbuildtool" => Some(Candidate::UbtExe),
        "dotnet" => Some(Candidate::Dotnet),
        _ => None,
    }
}

/// Turn a candidate and its argv into a live build-mode UBT, or `None` (not UBT, or not a
/// build). Pure, so it is unit-testable. A UBT by name whose argv is unreadable (empty)
/// still counts, with an unknown project; a `dotnet` with no readable argv cannot be told
/// apart from any other .NET process and is skipped.
fn classify(pid: u32, candidate: Candidate, args: &[String]) -> Option<UbtProc> {
    if candidate == Candidate::Dotnet {
        let hosts_ubt = args
            .iter()
            .any(|a| a.to_ascii_lowercase().ends_with("unrealbuildtool.dll"));
        if !hosts_ubt {
            return None;
        }
    }
    if args.is_empty() {
        return Some(UbtProc { pid, project: None });
    }
    if !ubt::is_build_mode(args) {
        return None;
    }
    Some(UbtProc {
        pid,
        project: ubt::extract_project_from_args(args),
    })
}

/// Read-only scan: refresh every process's name, then the command line of candidates only.
/// `None` when the process list cannot be read on this platform.
fn scan(sys: &mut sysinfo::System) -> Option<Vec<UbtProc>> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, UpdateKind};

    if !sysinfo::IS_SUPPORTED_SYSTEM {
        return None;
    }
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    if sys.processes().is_empty() {
        return None; // a readable process list is never empty (we are in it)
    }
    let candidates: Vec<(sysinfo::Pid, Candidate)> = sys
        .processes()
        .values()
        .filter_map(|p| candidate_of(&p.name().to_string_lossy()).map(|c| (p.pid(), c)))
        .collect();
    if candidates.is_empty() {
        return Some(Vec::new());
    }
    let pids: Vec<sysinfo::Pid> = candidates.iter().map(|(pid, _)| *pid).collect();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        false,
        ProcessRefreshKind::nothing()
            .without_tasks()
            .with_cmd(UpdateKind::OnlyIfNotSet),
    );
    Some(
        candidates
            .into_iter()
            .filter_map(|(pid, c)| {
                let p = sys.process(pid)?;
                classify(pid.as_u32(), c, &cmd_of(p))
            })
            .collect(),
    )
}

/// A probe whose snapshot the test sets; counts how often the engine asked.
#[cfg(test)]
pub(crate) struct FakeProbe {
    snap: Mutex<ProbeSnapshot>,
    calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl FakeProbe {
    /// A probe that cannot read processes (`procs: None`).
    pub(crate) fn unavailable() -> FakeProbe {
        FakeProbe::with(None)
    }

    /// A probe that sees exactly `procs`.
    pub(crate) fn live(procs: Vec<UbtProc>) -> FakeProbe {
        FakeProbe::with(Some(procs))
    }

    fn with(procs: Option<Vec<UbtProc>>) -> FakeProbe {
        FakeProbe {
            snap: Mutex::new(ProbeSnapshot { seq: 1, procs }),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// A new observation of `procs` (next `seq`).
    pub(crate) fn set(&self, procs: Option<Vec<UbtProc>>) {
        let mut s = self.snap.lock().unwrap();
        s.seq += 1;
        s.procs = procs;
    }

    /// A new observation of the same processes (next `seq`).
    pub(crate) fn bump(&self) {
        self.snap.lock().unwrap().seq += 1;
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
impl UbtProbe for FakeProbe {
    fn snapshot(&self) -> ProbeSnapshot {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.snap.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::herdr::keymap::normalize_either_separator;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn candidate_names_ignore_case_and_exe_suffix() {
        assert_eq!(candidate_of("UnrealBuildTool.exe"), Some(Candidate::UbtExe));
        assert_eq!(candidate_of("unrealbuildtool"), Some(Candidate::UbtExe));
        assert_eq!(candidate_of("dotnet.exe"), Some(Candidate::Dotnet));
        assert_eq!(candidate_of("DOTNET"), Some(Candidate::Dotnet));
        assert_eq!(candidate_of("UnrealEditor.exe"), None);
        assert_eq!(candidate_of("ShaderCompileWorker.exe"), None);
    }

    #[test]
    fn build_mode_ubt_maps_its_project() {
        let args = argv(&[
            r"C:\UE\Engine\Binaries\DotNET\UnrealBuildTool\UnrealBuildTool.exe",
            "MyGameEditor",
            "Win64",
            "Development",
            r"-Project=C:\work\alpha\MyGame.uproject",
        ]);
        assert_eq!(
            classify(7, Candidate::UbtExe, &args),
            Some(UbtProc {
                pid: 7,
                project: Some(normalize_either_separator(r"C:\work\alpha\MyGame.uproject")),
            })
        );
        // `-Mode=Build` is a build too.
        let mut explicit = args.clone();
        explicit.push("-Mode=Build".into());
        assert!(classify(7, Candidate::UbtExe, &explicit).is_some());
    }

    #[test]
    fn query_mode_ubt_is_ignored() {
        let args = argv(&[
            "UnrealBuildTool.exe",
            "-Mode=QueryTargets",
            r"-Project=C:\work\alpha\MyGame.uproject",
        ]);
        assert_eq!(classify(7, Candidate::UbtExe, &args), None);
    }

    #[test]
    fn unreadable_or_projectless_ubt_is_unknown() {
        assert_eq!(
            classify(7, Candidate::UbtExe, &[]),
            Some(UbtProc {
                pid: 7,
                project: None
            })
        );
        assert_eq!(
            classify(
                7,
                Candidate::UbtExe,
                &argv(&["UnrealBuildTool.exe", "-Progress"])
            ),
            Some(UbtProc {
                pid: 7,
                project: None
            })
        );
    }

    #[test]
    fn dotnet_counts_only_when_hosting_ubt() {
        let hosted = argv(&[
            "dotnet.exe",
            r"C:\UE\Engine\Binaries\DotNET\UnrealBuildTool\UnrealBuildTool.dll",
            r"-Project=C:\work\beta\MyGame.uproject",
        ]);
        assert_eq!(
            classify(9, Candidate::Dotnet, &hosted).and_then(|p| p.project),
            Some(normalize_either_separator(r"C:\work\beta\MyGame.uproject"))
        );
        assert_eq!(
            classify(
                9,
                Candidate::Dotnet,
                &argv(&["dotnet.exe", "build", "App.csproj"])
            ),
            None
        );
        assert_eq!(classify(9, Candidate::Dotnet, &[]), None);
    }

    #[test]
    fn cache_reuses_a_snapshot_within_the_window() {
        let mut cache = SnapshotCache::default();
        let t0 = Instant::now();
        let first = cache.get_or_refresh(t0, || Some(Vec::new()));
        assert_eq!(first.seq, 1);
        // Inside the window: same snapshot, refresh not called.
        let again = cache.get_or_refresh(t0 + Duration::from_millis(1_900), || {
            panic!("must answer from the cache")
        });
        assert_eq!(again, first);
        // Past the window: a fresh snapshot under the next seq.
        let fresh = cache.get_or_refresh(t0 + CACHE_FOR, || None);
        assert_eq!(
            fresh,
            ProbeSnapshot {
                seq: 2,
                procs: None
            }
        );
    }
}
