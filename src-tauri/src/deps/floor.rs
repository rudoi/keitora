//! The herdr version floor: parsing version strings, judging a running server against
//! the minimum version and protocol Keitora needs, and the shared slot the status loop
//! writes that judgement into.

use std::sync::Mutex;

use serde::Serialize;

use crate::backend::herdr::cli::ServerStatus;

/// The oldest herdr Keitora supports.
pub const HERDR_FLOOR: semver::Version = semver::Version::new(0, 9, 5);
/// The herdr client/server protocol Keitora speaks.
pub const HERDR_PROTOCOL: u64 = 22;

/// How the herdr in use compares with what Keitora needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FloorState {
    Ok,
    /// Older than [`HERDR_FLOOR`]; Keitora keeps working and offers an upgrade.
    TooOld {
        found: String,
    },
    /// A protocol Keitora can't speak; pane binding is refused until it is fixed.
    /// `newer`: herdr is ahead of Keitora (`found > want`), so upgrading herdr cannot
    /// help: Keitora needs updating, or a compatible Keitora-managed herdr installed.
    ProtocolMismatch {
        found: u64,
        want: u64,
        newer: bool,
    },
    /// The binary on disk is new enough but the running server is still the old one.
    /// Computed by the dependency service, never stored in a [`FloorSlot`].
    RestartToFinish {
        binary: String,
        server: String,
    },
    /// Nothing known yet, or the version could not be read. Never blocks.
    Unknown,
}

/// Parse a herdr version from either a bare `x.y.z` (what `status server --json`
/// reports) or a prefixed form such as `herdr 0.9.3` (what `herdr --version` prints).
/// The first whitespace-separated token that parses as semver wins; a leading `v` and
/// trailing punctuation are tolerated. `None` when nothing parses.
pub fn parse_version(s: &str) -> Option<semver::Version> {
    s.split_whitespace().find_map(|tok| {
        let tok = tok.trim_matches(|c: char| matches!(c, ',' | ';' | '(' | ')' | '"'));
        let tok = tok.strip_prefix('v').unwrap_or(tok);
        semver::Version::parse(tok).ok()
    })
}

/// Judge the running server from its `status server --json` reply.
pub fn judge(status: &ServerStatus) -> FloorState {
    judge_version(status.version.as_deref(), status.protocol)
}

/// Protocol first (present and not [`HERDR_PROTOCOL`] is a mismatch; absent is not),
/// then version (below [`HERDR_FLOOR`] is too old), else ok. A missing or unparseable
/// version with no mismatch is `Unknown`.
pub fn judge_version(version: Option<&str>, protocol: Option<u64>) -> FloorState {
    if let Some(found) = protocol {
        if found != HERDR_PROTOCOL {
            return FloorState::ProtocolMismatch {
                found,
                want: HERDR_PROTOCOL,
                newer: found > HERDR_PROTOCOL,
            };
        }
    }
    match version.and_then(parse_version) {
        None => FloorState::Unknown,
        Some(v) if v < HERDR_FLOOR => FloorState::TooOld {
            found: v.to_string(),
        },
        Some(_) => FloorState::Ok,
    }
}

type Observer = Box<dyn Fn(&FloorState) + Send + Sync>;

/// The latest floor judgement of the running server, shared between the status loop
/// (writer) and the dependency service (single observer).
pub struct FloorSlot {
    state: Mutex<FloorState>,
    /// The version string the running server last reported (with the judgement).
    server_version: Mutex<Option<String>>,
    observer: Mutex<Option<Observer>>,
}

impl Default for FloorSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl FloorSlot {
    /// A slot that starts at [`FloorState::Unknown`] with no observer.
    pub fn new() -> Self {
        FloorSlot {
            state: Mutex::new(FloorState::Unknown),
            server_version: Mutex::new(None),
            observer: Mutex::new(None),
        }
    }

    pub fn get(&self) -> FloorState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Store `next`; the observer runs only when it differs from the current state. The
    /// state lock is released before the observer runs, so the observer may call
    /// [`FloorSlot::get`] but must not call back into the backend.
    pub fn set(&self, next: FloorState) {
        {
            let mut cur = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if *cur == next {
                return;
            }
            *cur = next.clone();
        }
        let observer = self.observer.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = observer.as_ref() {
            f(&next);
        }
    }

    /// Judge a running server's status and remember the version it reported.
    pub fn observe(&self, status: &ServerStatus) {
        *self
            .server_version
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = status.version.clone();
        self.set(judge(status));
    }

    /// The version the running server last reported, if any.
    pub fn server_version(&self) -> Option<String> {
        self.server_version
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Forget the judgement: the server it described was stopped or is being replaced,
    /// and the next one is judged afresh by its own probe.
    pub fn reset(&self) {
        *self
            .server_version
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.set(FloorState::Unknown);
    }

    /// Register the observer, replacing any previous one.
    pub fn on_change(&self, f: Observer) {
        *self.observer.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    /// `Some(user-facing message)` iff the state is `ProtocolMismatch`.
    pub fn blocks_panes(&self) -> Option<String> {
        match self.get() {
            FloorState::ProtocolMismatch {
                found,
                want,
                newer: true,
            } => Some(format!(
                "herdr speaks protocol {found}, newer than this Keitora (protocol {want}). \
                 Update Keitora, or install a compatible herdr from Settings > Dependencies."
            )),
            FloorState::ProtocolMismatch { found, want, .. } => Some(format!(
                "herdr speaks protocol {found}; Keitora needs protocol {want}. Upgrade herdr \
                 from Settings > Dependencies."
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn v(s: &str) -> semver::Version {
        semver::Version::parse(s).unwrap()
    }

    #[test]
    fn floor_constants() {
        assert_eq!(HERDR_FLOOR, v("0.9.5"));
        assert_eq!(HERDR_PROTOCOL, 22);
    }

    #[test]
    fn parse_version_accepts_bare_and_prefixed_forms() {
        assert_eq!(parse_version("0.9.5"), Some(v("0.9.5")));
        assert_eq!(parse_version("  0.10.1\n"), Some(v("0.10.1")));
        // Recorded `herdr --version` output.
        assert_eq!(parse_version("herdr 0.9.3"), Some(v("0.9.3")));
        assert_eq!(parse_version("herdr 0.9.3\r\n"), Some(v("0.9.3")));
        assert_eq!(parse_version("herdr v0.9.6 (abc1234)"), Some(v("0.9.6")));
        assert_eq!(parse_version("herdr 1.0.0-rc.1"), Some(v("1.0.0-rc.1")));
    }

    #[test]
    fn parse_version_rejects_garbage() {
        for s in ["", "herdr", "0.9", "not a version", "herdr x.y.z", "9"] {
            assert_eq!(parse_version(s), None, "{s:?}");
        }
    }

    #[test]
    fn judge_version_table() {
        let mismatch = |found| FloorState::ProtocolMismatch {
            found,
            want: 22,
            newer: found > 22,
        };
        let cases: &[(Option<&str>, Option<u64>, FloorState)] = &[
            // Protocol is judged before version.
            (Some("0.9.1"), Some(23), mismatch(23)),
            (Some("0.10.0"), Some(21), mismatch(21)),
            (None, Some(23), mismatch(23)),
            (
                Some("0.9.3"),
                Some(22),
                FloorState::TooOld {
                    found: "0.9.3".into(),
                },
            ),
            (Some("0.9.5"), Some(22), FloorState::Ok),
            (Some("0.10.2"), Some(22), FloorState::Ok),
            // A missing protocol is not a mismatch.
            (Some("0.9.5"), None, FloorState::Ok),
            (
                Some("herdr 0.9.4"),
                None,
                FloorState::TooOld {
                    found: "0.9.4".into(),
                },
            ),
            (None, Some(22), FloorState::Unknown),
            (None, None, FloorState::Unknown),
            (Some("garbage"), Some(22), FloorState::Unknown),
        ];
        for (ver, proto, want) in cases {
            assert_eq!(&judge_version(*ver, *proto), want, "{ver:?} {proto:?}");
        }
    }

    #[test]
    fn judge_reads_server_status() {
        let status = ServerStatus {
            running: true,
            version: Some("0.9.5".into()),
            protocol: Some(22),
            socket: None,
        };
        assert_eq!(judge(&status), FloorState::Ok);
        let old = ServerStatus {
            version: Some("0.9.4".into()),
            ..status.clone()
        };
        assert!(matches!(judge(&old), FloorState::TooOld { .. }));
    }

    #[test]
    fn floor_state_serializes_with_type_tag() {
        let j = serde_json::to_value(FloorState::ProtocolMismatch {
            found: 23,
            want: 22,
            newer: true,
        })
        .unwrap();
        assert_eq!(
            j,
            serde_json::json!({"type": "protocol_mismatch", "found": 23, "want": 22, "newer": true})
        );
        let j = serde_json::to_value(FloorState::RestartToFinish {
            binary: "0.9.6".into(),
            server: "0.9.4".into(),
        })
        .unwrap();
        assert_eq!(j["type"], "restart_to_finish");
        assert_eq!(
            serde_json::to_value(FloorState::Ok).unwrap(),
            serde_json::json!({"type": "ok"})
        );
    }

    #[test]
    fn slot_starts_unknown_and_observer_fires_only_on_change() {
        let slot = FloorSlot::default();
        assert_eq!(slot.get(), FloorState::Unknown);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        {
            let calls = calls.clone();
            let seen = seen.clone();
            slot.on_change(Box::new(move |s| {
                calls.fetch_add(1, Ordering::SeqCst);
                seen.lock().unwrap().push(s.clone());
            }));
        }
        slot.set(FloorState::Unknown); // unchanged
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        slot.set(FloorState::Ok);
        slot.set(FloorState::Ok);
        slot.set(FloorState::Ok);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let old = FloorState::TooOld {
            found: "0.9.3".into(),
        };
        slot.set(old.clone());
        slot.set(old.clone());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(*seen.lock().unwrap(), vec![FloorState::Ok, old.clone()]);
        assert_eq!(slot.get(), old);
    }

    #[test]
    fn observer_may_read_the_slot() {
        let slot = Arc::new(FloorSlot::new());
        let read = Arc::new(Mutex::new(None));
        {
            let inner = slot.clone();
            let read = read.clone();
            slot.on_change(Box::new(move |_| {
                *read.lock().unwrap() = Some(inner.get());
            }));
        }
        slot.set(FloorState::Ok);
        assert_eq!(*read.lock().unwrap(), Some(FloorState::Ok));
    }

    #[test]
    fn blocks_panes_only_for_protocol_mismatch() {
        let slot = FloorSlot::new();
        assert_eq!(slot.blocks_panes(), None);
        slot.set(FloorState::Ok);
        assert_eq!(slot.blocks_panes(), None);
        slot.set(FloorState::TooOld {
            found: "0.9.3".into(),
        });
        assert_eq!(slot.blocks_panes(), None);
        slot.set(FloorState::ProtocolMismatch {
            found: 21,
            want: 22,
            newer: false,
        });
        assert_eq!(
            slot.blocks_panes().as_deref(),
            Some(
                "herdr speaks protocol 21; Keitora needs protocol 22. Upgrade herdr from \
                 Settings > Dependencies."
            )
        );
        // A herdr ahead of Keitora is not fixed by upgrading herdr.
        slot.set(FloorState::ProtocolMismatch {
            found: 23,
            want: 22,
            newer: true,
        });
        assert_eq!(
            slot.blocks_panes().as_deref(),
            Some(
                "herdr speaks protocol 23, newer than this Keitora (protocol 22). Update \
                 Keitora, or install a compatible herdr from Settings > Dependencies."
            )
        );
    }

    #[test]
    fn judge_marks_a_newer_protocol() {
        assert_eq!(
            judge_version(Some("0.12.0"), Some(23)),
            FloorState::ProtocolMismatch {
                found: 23,
                want: 22,
                newer: true
            }
        );
        assert_eq!(
            judge_version(Some("0.8.0"), Some(21)),
            FloorState::ProtocolMismatch {
                found: 21,
                want: 22,
                newer: false
            }
        );
    }

    #[test]
    fn observe_records_the_server_version_and_reset_forgets_it() {
        let slot = FloorSlot::new();
        let status = ServerStatus {
            running: true,
            version: Some("0.12.0".into()),
            protocol: Some(23),
            socket: None,
        };
        slot.observe(&status);
        assert!(slot.blocks_panes().is_some());
        assert_eq!(slot.server_version().as_deref(), Some("0.12.0"));
        slot.reset();
        assert_eq!(slot.get(), FloorState::Unknown);
        assert_eq!(slot.server_version(), None);
        assert_eq!(slot.blocks_panes(), None, "a reset slot never blocks");
    }
}
