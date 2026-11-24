//! The install/upgrade sequence one run drives.
//!
//! The new herdr is always installed beside the one in use, with the server still
//! running: nothing is stopped before the swap. Then the pin is switched and the
//! backend restarted onto the new binary exactly once through `PtyBackend::restart`
//! (which keeps every live session owed a resume), followed by an integration
//! refresh. The explicit stop, `PtyBackend::shutdown`, is never part of a run: it is
//! the user's "stop herdr", which owes nothing a resume. A fresh install (no server)
//! takes the same path.

use std::path::PathBuf;

use super::manifest::{parse_manifest, select_for, NetError, Release, MANIFEST_URL};
use super::preflight::{
    choose_tier, ManifestOutcome, ManualReason, PreflightInput, Tier, TierChoice, UpgradeRoute,
};
use super::service::Inner;
use super::tier1::{self, ScriptHost};
use super::{tier2, FailureKind, InstallFailure, Installed, Stage};

/// How a run ended.
pub(super) enum Outcome {
    Done {
        path: PathBuf,
        restart_pending: bool,
    },
    Failed {
        tier: Tier,
        failure: InstallFailure,
        /// Whether a direct download could still be offered.
        direct_viable: bool,
    },
}

fn failed(tier: Tier, kind: FailureKind, message: impl Into<String>) -> Outcome {
    Outcome::Failed {
        tier,
        failure: InstallFailure::new(kind, message),
        direct_viable: false,
    }
}

/// A manual reason as the failure the run ends with.
fn manual_failure(reason: ManualReason) -> Outcome {
    let (kind, message) = match reason {
        ManualReason::UnsupportedPlatform => (
            FailureKind::Other,
            "Keitora can't install herdr on this platform",
        ),
        ManualReason::Offline => (FailureKind::Offline, "herdr.dev could not be reached"),
        ManualReason::ManifestUnavailable => (
            FailureKind::Network,
            "herdr's release list could not be read",
        ),
        ManualReason::NoCompatibleRelease => (
            FailureKind::NoCompatibleRelease,
            "no herdr release is compatible with this Keitora",
        ),
    };
    failed(Tier::Manual, kind, message)
}

/// The manifest fetched and reduced to a selection.
struct Selection {
    outcome: ManifestOutcome,
    release: Option<Release>,
}

impl Inner {
    /// Drive run `run_id` to its end.
    pub(super) fn run(
        &self,
        run_id: u64,
        requested: Option<Tier>,
        upgrade: bool,
        confirm_overwrite: bool,
    ) {
        let outcome = self.drive(run_id, requested, upgrade, confirm_overwrite);
        self.finish(run_id, outcome);
    }

    fn select(&self, platform: super::manifest::Platform) -> Selection {
        let installed = self.lock().machine.current_version.clone();
        // A herdr ahead of Keitora is replaced by an older compatible release.
        let newer = self.newer_protocol();
        let text = match self.net.get_text(MANIFEST_URL) {
            Ok(t) => t,
            Err(NetError::Offline) => {
                return Selection {
                    outcome: ManifestOutcome::Offline,
                    release: None,
                }
            }
            Err(e) => {
                log::warn!("[deps] release manifest fetch failed: {e}");
                return Selection {
                    outcome: ManifestOutcome::Unavailable,
                    release: None,
                };
            }
        };
        match parse_manifest(&text, platform) {
            Ok(m) => {
                let release = select_for(&m, installed.as_ref(), newer);
                Selection {
                    outcome: ManifestOutcome::Loaded {
                        selected: release.clone(),
                        selected_is_latest: release.as_ref().is_some_and(|r| m.is_latest(r)),
                    },
                    release,
                }
            }
            Err(e) => {
                log::warn!("[deps] {e}");
                Selection {
                    outcome: ManifestOutcome::Unavailable,
                    release: None,
                }
            }
        }
    }

    /// The tier for this run. The script host is only probed when the script could
    /// be chosen at all (never in a dev build).
    fn choose(
        &self,
        platform: super::manifest::Platform,
        sel: &Selection,
        requested: Option<Tier>,
        route: Option<&UpgradeRoute>,
    ) -> Result<Tier, ManualReason> {
        let mut input = PreflightInput {
            platform: Some(platform),
            manifest: sel.outcome.clone(),
            release_build: self.ctx.release_build,
            keitora_home_set: self.ctx.env_set("KEITORA_HOME"),
            herdr_bin_env_set: self.ctx.env_set("KEITORA_HERDR_BIN"),
            script_host_ok: false,
        };
        let script_possible = requested != Some(Tier::Direct)
            && route != Some(&UpgradeRoute::Direct)
            && input.release_build
            && !input.keitora_home_set
            && !input.herdr_bin_env_set
            && matches!(
                sel.outcome,
                ManifestOutcome::Loaded {
                    selected_is_latest: true,
                    ..
                }
            );
        if script_possible {
            input.script_host_ok =
                tier1::probe_script_host(platform, &*self.proc_) == ScriptHost::Usable;
        }
        match choose_tier(&input) {
            TierChoice::Manual(reason) => Err(reason),
            TierChoice::Use(_) if !script_possible => Ok(Tier::Direct),
            TierChoice::Use(tier) => Ok(tier),
        }
    }

    fn drive(
        &self,
        run_id: u64,
        requested: Option<Tier>,
        upgrade: bool,
        confirm_overwrite: bool,
    ) -> Outcome {
        let Some(platform) = self.platform else {
            self.set_manual_reason(ManualReason::UnsupportedPlatform);
            return manual_failure(ManualReason::UnsupportedPlatform);
        };
        self.refresh_machine();
        let machine = self.lock().machine.clone();
        let sel = self.select(platform);
        let mut tier = match self.choose(platform, &sel, requested, machine.route.as_ref()) {
            Ok(t) => t,
            Err(reason) => {
                self.set_manual_reason(reason);
                return manual_failure(reason);
            }
        };
        // Never let the vendor script overwrite a herdr the user installed themselves
        // without asking: install a managed copy beside it instead.
        if tier == Tier::Script && machine.overwrite_target.is_some() && !confirm_overwrite {
            log::info!("[deps] not overwriting the user's herdr; installing a managed copy");
            tier = Tier::Direct;
        }
        self.set_tier(run_id, tier);
        match tier {
            Tier::Direct => {
                let release = sel.release.expect("a tier was chosen, so a release exists");
                self.direct(run_id, platform, &release, upgrade)
            }
            _ => {
                // The script replaces the vendor install in place: an upgrade must
                // end on a different version than that install had.
                let previous = (upgrade && machine.route == Some(UpgradeRoute::ScriptThenDirect))
                    .then_some(machine.current_version)
                    .flatten();
                self.script(run_id, platform, upgrade, previous, sel.release.is_some())
            }
        }
    }

    /// Tier 2: download beside the running server, then repin and restart.
    fn direct(
        &self,
        run_id: u64,
        platform: super::manifest::Platform,
        release: &Release,
        upgrade: bool,
    ) -> Outcome {
        let req = tier2::Request {
            release,
            platform,
            keitora_home: &self.ctx.keitora_home,
        };
        let installed = match tier2::install(
            &*self.net,
            &*self.proc_,
            &*self.fs,
            &req,
            &mut |stage, done, total| self.progress(run_id, stage, done, total, None),
        ) {
            Ok(i) => i,
            Err(failure) => {
                return Outcome::Failed {
                    tier: Tier::Direct,
                    failure,
                    direct_viable: false,
                }
            }
        };
        let outcome = self.swap(run_id, Tier::Direct, &installed, upgrade);
        if matches!(outcome, Outcome::Done { .. }) {
            let removed = tier2::prune(&*self.fs, &self.ctx.keitora_home, Some(&installed.path));
            if !removed.is_empty() {
                log::info!(
                    "[deps] pruned {} old managed herdr version(s)",
                    removed.len()
                );
            }
        }
        outcome
    }

    /// Tier 1: the vendor script, run while the server keeps serving. It never
    /// touches the running binary: on Windows it writes a new release directory, on
    /// macOS it replaces `~/.local/bin/herdr` by rename, which a running process
    /// survives. A failed script changed nothing Keitora runs, so there is nothing to
    /// restore.
    fn script(
        &self,
        run_id: u64,
        platform: super::manifest::Platform,
        upgrade: bool,
        previous: Option<semver::Version>,
        direct_viable: bool,
    ) -> Outcome {
        let Some(home) = self.ctx.home.clone() else {
            return failed(
                Tier::Script,
                FailureKind::Other,
                "your home folder could not be found",
            );
        };
        self.progress(run_id, Stage::Installing, 0, None, None);
        let req = tier1::Request {
            platform,
            home: &home,
            keitora_home: &self.ctx.keitora_home,
            env: &self.ctx.env,
            previous: previous.as_ref(),
        };
        let result = tier1::run(&*self.proc_, &req, &mut |line| self.log_line(run_id, line));
        let installed = match result {
            Ok(i) => i,
            Err(failure) => {
                return Outcome::Failed {
                    tier: Tier::Script,
                    failure,
                    direct_viable,
                }
            }
        };
        if !platform.is_windows() {
            self.record_installed(&installed.path);
        }
        self.progress(run_id, Stage::Verifying, 0, None, None);
        self.swap(run_id, Tier::Script, &installed, upgrade)
    }

    /// Pin the installed binary and restart onto it, exactly once. The restart is
    /// `PtyBackend::restart`, the "Restart daemon" path, which marks every live
    /// session owed a resume; the explicit stop (`PtyBackend::shutdown`) is never
    /// used, since it owes nothing a resume. If the restart cannot bring the new
    /// binary up during an upgrade, the previous pin is put back and the old binary
    /// restarted.
    fn swap(&self, run_id: u64, tier: Tier, installed: &Installed, upgrade: bool) -> Outcome {
        let old_pin = self.ctx.config.pinned();
        if let Err(e) = self.ctx.config.set_pinned(Some(installed.path.clone())) {
            // Nothing changed: the old server keeps serving.
            return failed(
                tier,
                FailureKind::Other,
                format!("could not save the herdr path: {e}"),
            );
        }
        self.progress(run_id, Stage::Restarting, 0, None, None);
        match self.restart_backend() {
            Ok(restart_pending) => Outcome::Done {
                path: installed.path.clone(),
                restart_pending,
            },
            Err(e) if upgrade => {
                self.restore(old_pin);
                failed(
                    tier,
                    FailureKind::Incomplete,
                    format!(
                        "herdr {} is installed but did not start ({e}); the previous herdr \
                         was restarted instead",
                        installed.version
                    ),
                )
            }
            Err(e) => failed(
                tier,
                FailureKind::Incomplete,
                format!(
                    "herdr {} is installed but did not start: {e}",
                    installed.version
                ),
            ),
        }
    }

    /// Put `old_pin` back and restart the binary it names.
    fn restore(&self, old_pin: Option<PathBuf>) {
        if let Err(e) = self.ctx.config.set_pinned(old_pin) {
            log::warn!("[deps] could not restore the previous herdr path: {e}");
            return;
        }
        match self.restart_backend() {
            Ok(true) => log::warn!("[deps] the previous herdr restarts once recovery settles"),
            Ok(false) => log::info!("[deps] restarted the previous herdr"),
            Err(e) => log::warn!("[deps] the previous herdr did not restart: {e}"),
        }
    }
}
