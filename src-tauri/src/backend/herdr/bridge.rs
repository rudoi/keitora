//! `bridge` — one `herdr terminal session control` child per attached pane
//!
//! The bridge is the output/input stream for a mounted pane. It spawns
//! `herdr terminal session control <terminal_id> --takeover --cols N --rows N`,
//! which emits NDJSON `terminal.frame` objects on stdout (base64 ANSI viewport)
//! and reads input on stdin. **herdr detaches the moment stdin hits EOF**
//! (verified), so the bridge holds the child's stdin open for the pane's whole
//! life; closing it (on `detach`/`kill`/drop) is the clean teardown.
//!
//! Each `terminal.frame` goes to the pane's [`OutputSink`] with the frame's base64
//! string untouched (the frontend decodes it once). A `terminal.closed` is the
//! sink's `exit`; a stream that just ends is an exit only when the exit authority
//! confirms the pane really ended (not a server death or recovery).
//!
//! The bridge is resized in place with a `terminal.resize` line on its stdin
//! ([`resize_line`]). herdr marks the pane for repaint on every resize, so its next
//! frame is `full`: a same-size resize is how a pane asks for a fresh viewport.
//! `terminal.resize` is not in herdr's docs; it is verified against the herdr
//! 0.9.3 source (protocol 22), and a failed write falls back to a restart.
//!
//! A wheel tick or page key is a `terminal.scroll` line ([`scroll_line`]), also
//! verified against the herdr 0.9.3 source; herdr routes it itself (a mouse report
//! or a PageUp/PageDown key to an app that handles scrolling, arrow keys under
//! alternate-scroll, else its own history).

use std::borrow::Cow;
use std::io::{BufRead, BufReader, Write};

use base64::Engine as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::JoinHandle;

use serde::Deserialize;

use crate::backend::{OutputSink, PtyBackendError, ScrollDirection, ScrollSource};

use super::ops::{BridgeChild, HerdrOps};

/// A bridge's stdin, shared so input writes need not hold the backend's `bridges`
/// map lock. `None` once closed (closing it is what detaches herdr).
pub type BridgeStdin = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

/// Decides whether a control stream that ended without `terminal.closed` is a
/// real pane exit. Runs on the reader thread, only in that rare case.
pub type ExitAuthority = Arc<dyn Fn() -> bool + Send + Sync>;

/// A parsed control-stream line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameEvent {
    /// `terminal.frame`: `bytes` is base64 ANSI; `full` marks a repaint; `seq`
    /// numbers the frames of one stream.
    Frame {
        bytes: String,
        full: bool,
        seq: Option<u64>,
    },
    /// `terminal.closed`: the pane's terminal ended.
    Closed,
    /// A control line we don't act on.
    Other,
}

/// One control-stream line as herdr writes it, tagged on `type`. Only the fields
/// the bridge uses are read; the rest (`encoding`, `width`, …) are skipped. The
/// one allocation is `bytes`, which is handed on as is.
#[derive(Deserialize)]
struct ControlLine<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(default)]
    bytes: Option<String>,
    #[serde(default)]
    full: bool,
    #[serde(default)]
    seq: Option<u64>,
}

/// Parse one NDJSON control-stream line. `None` for blank/unparseable lines,
/// including a `terminal.frame` without `bytes`.
pub fn parse_frame_line(line: &str) -> Option<FrameEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let parsed: ControlLine<'_> = serde_json::from_str(line).ok()?;
    match parsed.kind.as_ref() {
        "terminal.frame" => Some(FrameEvent::Frame {
            bytes: parsed.bytes?,
            full: parsed.full,
            seq: parsed.seq,
        }),
        "terminal.closed" => Some(FrameEvent::Closed),
        _ => Some(FrameEvent::Other),
    }
}

/// `(expected, got)` when `seq` does not follow `last`: frames were skipped (or
/// the stream restarted its count). Frames without a `seq` are never a gap.
pub fn seq_gap(last: Option<u64>, seq: Option<u64>) -> Option<(u64, u64)> {
    let (last, seq) = (last?, seq?);
    let expected = last.wrapping_add(1);
    (seq != expected).then_some((expected, seq))
}

/// One `terminal.input` payload. `Text` goes as `text` (no base64 either way);
/// `Bytes` carries input that is not UTF-8, base64-encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeInput<'a> {
    Text(&'a str),
    Bytes(&'a [u8]),
}

/// The NDJSON `terminal.input` line (newline included) for `input`. Built with
/// `serde_json` so quotes, newlines and control characters are escaped correctly.
pub fn input_line(input: BridgeInput<'_>) -> String {
    #[derive(serde::Serialize)]
    struct InputFrame<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        bytes: Option<String>,
    }
    let frame = match input {
        BridgeInput::Text(text) => InputFrame {
            kind: "terminal.input",
            text: Some(text),
            bytes: None,
        },
        BridgeInput::Bytes(data) => InputFrame {
            kind: "terminal.input",
            text: None,
            bytes: Some(base64::engine::general_purpose::STANDARD.encode(data)),
        },
    };
    // Serializing a struct of strings cannot fail.
    let mut line = serde_json::to_string(&frame).unwrap_or_default();
    line.push('\n');
    line
}

/// The NDJSON `terminal.resize` line (newline included) for a `cols`×`rows`
/// viewport. herdr rejects a zero size, so callers clamp to at least 1.
pub fn resize_line(cols: u16, rows: u16) -> String {
    format!("{{\"type\":\"terminal.resize\",\"cols\":{cols},\"rows\":{rows}}}\n")
}

/// The NDJSON `terminal.scroll` line (newline included): `lines` (at least 1;
/// herdr rejects 0) toward `direction` from `source`, with the pointer at the
/// 0-based cell `column`,`row`.
pub fn scroll_line(
    direction: ScrollDirection,
    source: ScrollSource,
    lines: u16,
    column: u16,
    row: u16,
) -> String {
    let direction = direction.as_str();
    let source = source.as_str();
    let lines = lines.max(1);
    format!(
        "{{\"type\":\"terminal.scroll\",\"direction\":\"{direction}\",\"source\":\"{source}\",\"lines\":{lines},\"column\":{column},\"row\":{row}}}\n"
    )
}

/// The most lines a wheel scroll is split into one-line commands for (see
/// [`scroll_payload`]). Above it (only a jump to the live tail) one command goes.
pub const WHEEL_SPLIT_MAX: u16 = 128;

/// The bridge stdin bytes for one (possibly merged) scroll. herdr turns one wheel
/// command into ONE app event (a mouse report or an arrow key) whatever its
/// `lines`, so a wheel scroll of up to [`WHEEL_SPLIT_MAX`] lines goes as that many
/// `lines:1` commands: an app reading the mouse gets one report per line, and
/// herdr's own history scroll adds them up the same. A larger wheel scroll and
/// every page-key scroll (one keypress per page) go as one command.
pub fn scroll_payload(
    direction: ScrollDirection,
    source: ScrollSource,
    lines: u16,
    column: u16,
    row: u16,
) -> String {
    let lines = lines.max(1);
    if source == ScrollSource::Wheel && lines <= WHEEL_SPLIT_MAX {
        scroll_line(direction, source, 1, column, row).repeat(usize::from(lines))
    } else {
        scroll_line(direction, source, lines, column, row)
    }
}

/// Writes one `terminal.input` frame to a bridge stdin handle. Free function (not
/// a `&self` method) so a caller can clone the `Arc` handle, release the
/// bridges-map lock, and only THEN do the blocking write: a backed-up pipe on one
/// pane must not stall every other bridge operation.
pub fn send_input_to(
    stdin: &Mutex<Option<Box<dyn Write + Send>>>,
    input: BridgeInput<'_>,
) -> Result<(), PtyBackendError> {
    write_line_to(stdin, &input_line(input), "input")
}

/// Writes one `terminal.resize` frame to a bridge stdin handle (see
/// [`send_input_to`] for why this is a free function). herdr skips a line it
/// cannot use without answering, so `Ok` means only that the line was written.
pub fn send_resize_to(
    stdin: &Mutex<Option<Box<dyn Write + Send>>>,
    cols: u16,
    rows: u16,
) -> Result<(), PtyBackendError> {
    write_line_to(stdin, &resize_line(cols, rows), "resize")
}

/// Writes one scroll to a bridge stdin handle: the `terminal.scroll` line(s) of
/// [`scroll_payload`] in one write (see [`send_input_to`] for why this is a free
/// function). herdr answers nothing, so `Ok` means only that it was written.
pub fn send_scroll_to(
    stdin: &Mutex<Option<Box<dyn Write + Send>>>,
    direction: ScrollDirection,
    source: ScrollSource,
    lines: u16,
    column: u16,
    row: u16,
) -> Result<(), PtyBackendError> {
    let payload = scroll_payload(direction, source, lines, column, row);
    write_line_to(stdin, &payload, "scroll")
}

fn write_line_to(
    stdin: &Mutex<Option<Box<dyn Write + Send>>>,
    line: &str,
    what: &str,
) -> Result<(), PtyBackendError> {
    let mut guard = stdin.lock().unwrap();
    let s = guard
        .as_mut()
        .ok_or_else(|| PtyBackendError::Other("herdr bridge: stdin closed".into()))?;
    s.write_all(line.as_bytes())
        .and_then(|_| s.flush())
        .map_err(|e| PtyBackendError::Other(format!("herdr bridge {what} write: {e}")))
}

/// A running per-pane I/O bridge.
pub struct Bridge {
    child: Box<dyn BridgeChild>,
    /// Held open for the pane's life — setting it to `None` is what detaches herdr.
    /// Shared (`Arc<Mutex<…>>`) so input writes can happen without holding the
    /// backend's `bridges` map lock (see `send_input_to`).
    stdin: BridgeStdin,
    reader: Option<JoinHandle<()>>,
    /// Set by [`Bridge::stop`] before teardown so the reader thread does NOT report
    /// an exit for an intentional stop. Without it, every
    /// bridge restart (attach, a failed resize) would flap the pane to "ended".
    stopping: Arc<AtomicBool>,
    /// Set while a replacement bridge takes the terminal over (see
    /// [`Bridge::mute_exit`]): the stream ending then is not a pane exit.
    muted: Arc<AtomicBool>,
    /// The viewport size: the `--takeover` size, then the last in-place resize.
    /// Lets `resize` skip an unchanged size, and a restart keep the size.
    cols: u16,
    rows: u16,
    /// The session generation (keymap `gen`) this bridge streams. A writer bound
    /// to another generation must not write into it.
    session_gen: u64,
}

impl Bridge {
    /// Spawn the control bridge for `terminal_id` and start the frame reader
    /// thread, delivering frames and the exit to `out` (the pane's sink when the
    /// bridge starts; a reattach starts a new bridge with the new sink). An
    /// unexpected end of the stream is reported as an exit only when
    /// `exit_is_authoritative` says so.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        ops: &dyn HerdrOps,
        terminal_id: &str,
        cols: u16,
        rows: u16,
        project: &str,
        worktree: &str,
        out: Arc<dyn OutputSink>,
        exit_is_authoritative: ExitAuthority,
    ) -> Result<Bridge, PtyBackendError> {
        let process = ops
            .spawn_bridge(terminal_id, cols, rows)
            .map_err(|e| PtyBackendError::Other(format!("herdr bridge spawn failed: {e}")))?;
        let child = process.child;
        let stdin: BridgeStdin = Arc::new(Mutex::new(Some(process.stdin)));
        let stdout = process.stdout;
        // Drain stderr to the log: a control child that dies immediately
        // (e.g. a bad/stale terminal id) used to vanish silently into `Stdio::null()`.
        if let Some(stderr) = process.stderr {
            let tid = terminal_id.to_string();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        log::warn!("herdr bridge [{tid}] stderr: {line}");
                    }
                }
            });
        }

        let stopping = Arc::new(AtomicBool::new(false));
        let reader_stopping = Arc::clone(&stopping);
        let muted = Arc::new(AtomicBool::new(false));
        let reader_muted = Arc::clone(&muted);
        let probe_project = project.to_string();
        let probe_worktree = worktree.to_string();
        let reader = std::thread::spawn(move || {
            let mut saw_closed = false;
            let mut last_seq = None;
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                match parse_frame_line(&line) {
                    Some(FrameEvent::Frame { bytes, full, seq }) => {
                        if let Some((expected, got)) = seq_gap(last_seq, seq) {
                            log::debug!(
                                "herdr bridge [{probe_project}/{probe_worktree}] frame seq gap: expected {expected}, got {got}"
                            );
                        }
                        last_seq = seq.or(last_seq);
                        crate::latency_probe::on_frame(&probe_project, &probe_worktree);
                        // `bytes` is already base64 ANSI: the frontend decodes it.
                        out.frame(bytes, full);
                    }
                    Some(FrameEvent::Closed) => {
                        saw_closed = true;
                        out.exit();
                        break;
                    }
                    _ => {}
                }
            }
            // Unexpected EOF (the control child exited WITHOUT a `terminal.closed`, e.g. a
            // dead/stale terminal id) must still surface as an exit so the pane leaves its
            // blinking-cursor limbo — UNLESS this is an intentional stop
            // (resize/kill/replace), where an exit would wrongly flap the pane to "ended",
            // or the stream ended because the server died or is being recovered (the
            // authority says no), where the session is resumed instead of ended.
            if !saw_closed
                && !reader_stopping.load(Ordering::SeqCst)
                && !reader_muted.load(Ordering::SeqCst)
                && exit_is_authoritative()
            {
                out.exit();
            }
        });

        Ok(Bridge {
            child,
            stdin,
            reader: Some(reader),
            stopping,
            muted,
            cols,
            rows,
            session_gen: 0,
        })
    }

    /// While a replacement bridge is starting: the end of this bridge's stream
    /// (herdr handing the terminal to the replacement) is not a pane exit. Undo
    /// with [`ExitMute::unmute`] if the replacement does not take over.
    pub fn mute_exit(&self) -> ExitMute {
        self.muted.store(true, Ordering::SeqCst);
        ExitMute(Arc::clone(&self.muted))
    }

    /// Tag the bridge with the session generation it streams.
    pub fn for_session(mut self, gen: u64) -> Self {
        self.session_gen = gen;
        self
    }

    /// The session generation this bridge streams.
    pub fn session_gen(&self) -> u64 {
        self.session_gen
    }

    /// The bridge's viewport size.
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Record a size sent to herdr with an in-place resize.
    pub fn set_size(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
    }

    /// A clonable handle to this bridge's stdin, for `send_input_to` without holding
    /// the backend's `bridges` map lock.
    pub fn stdin_handle(&self) -> BridgeStdin {
        Arc::clone(&self.stdin)
    }

    /// Close stdin (clean detach) and kill the child; the reader thread ends when
    /// stdout closes.
    pub fn stop(&mut self) {
        // Mark this teardown intentional BEFORE closing stdout so the reader suppresses
        // the EOF-driven exit.
        self.stopping.store(true, Ordering::SeqCst);
        // Setting stdin to None sends EOF → herdr detaches cleanly. A writer blocked
        // on a full pipe holds the stdin lock, so then kill the child first: the
        // broken pipe fails that write and frees the lock instead of hanging here.
        let closed = match self.stdin.try_lock() {
            Ok(mut guard) => {
                *guard = None;
                true
            }
            Err(TryLockError::Poisoned(p)) => {
                *p.into_inner() = None;
                true
            }
            Err(TryLockError::WouldBlock) => false,
        };
        self.child.kill();
        if !closed {
            *self.stdin.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
        self.child.wait();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

/// A bridge's exit muted by [`Bridge::mute_exit`].
pub struct ExitMute(Arc<AtomicBool>);

impl ExitMute {
    /// The replacement did not take over: the stream ending is an exit again.
    pub fn unmute(self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::herdr::fake::{CaptureOutput, FakeHerdr, OutputEvent};
    use serde_json::Value;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn frame(text: &str) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(text);
        format!(r#"{{"type":"terminal.frame","bytes":"{b64}","full":true}}"#)
    }

    fn start(fake: &FakeHerdr, sink: &Arc<CaptureOutput>) -> Bridge {
        start_with(fake, sink, Arc::new(|| true))
    }

    fn start_with(fake: &FakeHerdr, sink: &Arc<CaptureOutput>, authority: ExitAuthority) -> Bridge {
        Bridge::start(
            fake,
            "term_1",
            80,
            24,
            "proj",
            "wt",
            Arc::clone(sink) as Arc<dyn OutputSink>,
            authority,
        )
        .unwrap()
    }

    fn has_exit(e: &[OutputEvent]) -> bool {
        e.contains(&OutputEvent::Exit)
    }

    #[test]
    fn unexpected_eof_without_authority_is_not_an_exit() {
        let fake = FakeHerdr::new();
        let sink = Arc::new(CaptureOutput::default());
        let asked = Arc::new(AtomicBool::new(false));
        let a = Arc::clone(&asked);
        let mut bridge = start_with(
            &fake,
            &sink,
            Arc::new(move || {
                a.store(true, Ordering::SeqCst);
                false
            }),
        );
        fake.kill_server(Some(1));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !asked.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the authority was never asked");
            std::thread::sleep(Duration::from_millis(2));
        }
        bridge.stop();
        assert_eq!(sink.exits(), 0, "{:?}", sink.events());
    }

    #[test]
    fn a_closed_frame_is_an_exit_whatever_the_authority_says() {
        let fake = FakeHerdr::new();
        let sink = Arc::new(CaptureOutput::default());
        let mut bridge = start_with(&fake, &sink, Arc::new(|| false));
        fake.push_frame("term_1", r#"{"type":"terminal.closed","reason":"exited"}"#);
        assert!(sink.wait_for(has_exit));
        bridge.stop();
        assert_eq!(sink.exits(), 1);
    }

    #[test]
    fn bridge_emits_exit_on_unexpected_eof() {
        let fake = FakeHerdr::new();
        let sink = Arc::new(CaptureOutput::default());
        let mut bridge = start(&fake, &sink);
        fake.push_frame("term_1", &frame("hi"));
        assert!(sink.wait_for(|e| !e.is_empty()));
        // The server dies: the control stream ends without a `terminal.closed`.
        fake.kill_server(Some(1));
        assert!(
            sink.wait_for(has_exit),
            "an unexpected EOF surfaces as an exit: {:?}",
            sink.events()
        );
        bridge.stop();
        assert_eq!(sink.exits(), 1);
    }

    #[test]
    fn a_muted_bridge_whose_stream_ends_reports_no_exit_until_unmuted() {
        for unmute in [false, true] {
            let fake = FakeHerdr::new();
            let sink = Arc::new(CaptureOutput::default());
            let mut bridge = start(&fake, &sink);
            let mute = bridge.mute_exit();
            if unmute {
                mute.unmute();
            }
            // The stream ends without a close (as when a replacement takes over).
            fake.kill_server(Some(1));
            bridge.reader.take().unwrap().join().unwrap();
            assert_eq!(sink.exits(), usize::from(unmute), "{:?}", sink.events());
            bridge.stop();
        }
    }

    #[test]
    fn bridge_stop_suppresses_exit() {
        let fake = FakeHerdr::new();
        let sink = Arc::new(CaptureOutput::default());
        let mut bridge = start(&fake, &sink);
        fake.push_frame("term_1", &frame("hi"));
        assert!(sink.wait_for(|e| !e.is_empty()));
        // stop() joins the reader, so every event it would deliver is already captured.
        bridge.stop();
        assert_eq!(sink.events().len(), 1, "{:?}", sink.events());
        assert_eq!(sink.frames(), vec!["hi".to_string()]);
    }

    #[test]
    fn frames_reach_the_sink_untouched_with_their_full_flag() {
        let fake = FakeHerdr::new();
        let sink = Arc::new(CaptureOutput::default());
        let mut bridge = start(&fake, &sink);
        let b64 = base64::engine::general_purpose::STANDARD.encode("partial");
        fake.push_frame(
            "term_1",
            &format!(r#"{{"type":"terminal.frame","bytes":"{b64}","full":false,"seq":7}}"#),
        );
        fake.push_frame("term_1", &frame("whole"));
        fake.push_frame("term_1", r#"{"type":"terminal.closed","reason":"exited"}"#);
        assert!(sink.wait_for(has_exit));
        bridge.stop();
        let whole = base64::engine::general_purpose::STANDARD.encode("whole");
        assert_eq!(
            sink.events(),
            vec![
                OutputEvent::Frame { b64, full: false },
                OutputEvent::Frame {
                    b64: whole,
                    full: true
                },
                OutputEvent::Exit,
            ]
        );
    }

    #[test]
    fn bridge_input_reaches_the_bridge_stdin() {
        let fake = FakeHerdr::new();
        let sink = Arc::new(CaptureOutput::default());
        let mut bridge = start(&fake, &sink);
        send_input_to(&bridge.stdin_handle(), BridgeInput::Bytes(b"ls\r")).unwrap();
        send_input_to(&bridge.stdin_handle(), BridgeInput::Text("pwd")).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"ls\r");
        assert_eq!(
            fake.bridge_input("term_1"),
            format!(
                "{{\"type\":\"terminal.input\",\"bytes\":\"{b64}\"}}\n\
                 {{\"type\":\"terminal.input\",\"text\":\"pwd\"}}\n"
            )
        );
        bridge.stop();
        assert!(
            send_input_to(&bridge.stdin_handle(), BridgeInput::Text("x")).is_err(),
            "stdin closed after stop"
        );
    }

    #[test]
    fn resize_lines_reach_the_bridge_stdin() {
        let fake = FakeHerdr::new();
        let sink = Arc::new(CaptureOutput::default());
        let mut bridge = start(&fake, &sink);
        send_resize_to(&bridge.stdin_handle(), 120, 40).unwrap();
        let input = fake.bridge_input("term_1");
        let body = input.strip_suffix('\n').expect("newline-terminated");
        let v: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type": "terminal.resize", "cols": 120, "rows": 40})
        );
        bridge.stop();
        assert!(
            send_resize_to(&bridge.stdin_handle(), 80, 24).is_err(),
            "stdin closed after stop"
        );
    }

    #[test]
    fn scroll_lines_carry_direction_source_lines_and_cell() {
        for (direction, word) in [(ScrollDirection::Up, "up"), (ScrollDirection::Down, "down")] {
            for (source, src) in [
                (ScrollSource::Wheel, "wheel"),
                (ScrollSource::PageKey, "page_key"),
            ] {
                let line = scroll_line(direction, source, 3, 12, 7);
                let body = line.strip_suffix('\n').expect("newline-terminated");
                assert!(!body.contains('\n'), "one line: {line:?}");
                let v: Value = serde_json::from_str(body).unwrap();
                assert_eq!(
                    v,
                    serde_json::json!({
                        "type": "terminal.scroll",
                        "direction": word,
                        "source": src,
                        "lines": 3,
                        "column": 12,
                        "row": 7,
                    })
                );
            }
        }
        // herdr rejects zero lines.
        let line = scroll_line(ScrollDirection::Up, ScrollSource::Wheel, 0, 0, 0);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["lines"], 1);
    }

    /// Each line of a scroll payload, parsed.
    fn payload_lines(payload: &str) -> Vec<Value> {
        assert!(payload.ends_with('\n'), "newline-terminated: {payload:?}");
        payload
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn a_wheel_scroll_goes_as_one_line_commands_up_to_the_split_max() {
        let wheel_up = |n: u16| scroll_payload(ScrollDirection::Up, ScrollSource::Wheel, n, 4, 2);
        let one_line = scroll_line(ScrollDirection::Up, ScrollSource::Wheel, 1, 4, 2);
        assert_eq!(wheel_up(5), one_line.repeat(5));
        let lines = payload_lines(&wheel_up(WHEEL_SPLIT_MAX));
        assert_eq!(lines.len(), usize::from(WHEEL_SPLIT_MAX));
        assert!(lines
            .iter()
            .all(|v| v["lines"] == 1 && v["source"] == "wheel"));
        // Past the max (a jump to the live tail) it is one command.
        assert_eq!(
            wheel_up(WHEEL_SPLIT_MAX + 1),
            scroll_line(
                ScrollDirection::Up,
                ScrollSource::Wheel,
                WHEEL_SPLIT_MAX + 1,
                4,
                2
            )
        );
        assert_eq!(payload_lines(&wheel_up(u16::MAX)).len(), 1);
        // Zero lines is one one-line command.
        assert_eq!(wheel_up(0), one_line);
    }

    #[test]
    fn a_page_key_scroll_is_always_one_command() {
        for n in [1, 3, WHEEL_SPLIT_MAX + 7] {
            let payload = scroll_payload(ScrollDirection::Down, ScrollSource::PageKey, n, 1, 1);
            assert_eq!(
                payload,
                scroll_line(ScrollDirection::Down, ScrollSource::PageKey, n, 1, 1)
            );
            let lines = payload_lines(&payload);
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0]["source"], "page_key");
            assert_eq!(lines[0]["lines"], n);
        }
    }

    #[test]
    fn scroll_lines_reach_the_bridge_stdin() {
        let fake = FakeHerdr::new();
        let sink = Arc::new(CaptureOutput::default());
        let mut bridge = start(&fake, &sink);
        let stdin = bridge.stdin_handle();
        send_scroll_to(&stdin, ScrollDirection::Down, ScrollSource::Wheel, 3, 0, 0).unwrap();
        send_scroll_to(&stdin, ScrollDirection::Up, ScrollSource::PageKey, 9, 0, 0).unwrap();
        assert_eq!(
            fake.bridge_input("term_1"),
            scroll_payload(ScrollDirection::Down, ScrollSource::Wheel, 3, 0, 0)
                + &scroll_line(ScrollDirection::Up, ScrollSource::PageKey, 9, 0, 0)
        );
        bridge.stop();
        let stdin = bridge.stdin_handle();
        assert!(
            send_scroll_to(&stdin, ScrollDirection::Up, ScrollSource::Wheel, 1, 0, 0).is_err(),
            "stdin closed after stop"
        );
    }

    /// Parses one input line, checking it is a single newline-terminated line.
    fn parse_input(line: &str) -> Value {
        let body = line.strip_suffix('\n').expect("newline-terminated");
        assert!(!body.contains('\n'), "one line: {line:?}");
        let v: Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["type"], "terminal.input");
        v
    }

    #[test]
    fn text_input_escapes_quotes_newlines_and_control_chars() {
        let text = "say \"hi\"\\ \r\n\t\u{1b}[A\u{0}\u{7f} é ✓";
        let v = parse_input(&input_line(BridgeInput::Text(text)));
        assert_eq!(v["text"], text);
        assert!(v.get("bytes").is_none());
    }

    #[test]
    fn bytes_input_is_base64() {
        let data = [0xff, 0x00, b'"', b'\n', 0x80];
        let v = parse_input(&input_line(BridgeInput::Bytes(&data)));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(v["bytes"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, data);
        assert!(v.get("text").is_none());
    }

    type KillFlag = Arc<(Mutex<bool>, std::sync::Condvar)>;

    /// A stdin whose write blocks (a full pipe) until the child is killed.
    struct BlockedStdin {
        entered: mpsc::Sender<()>,
        killed: KillFlag,
    }
    impl Write for BlockedStdin {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.entered.send(());
            let (lock, cv) = &*self.killed;
            let mut killed = lock.lock().unwrap();
            while !*killed {
                killed = cv.wait(killed).unwrap();
            }
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct KillableChild(KillFlag);
    impl BridgeChild for KillableChild {
        fn kill(&mut self) {
            let (lock, cv) = &*self.0;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        fn wait(&mut self) {}
    }

    #[test]
    fn stop_does_not_hang_on_a_writer_blocked_in_a_full_pipe() {
        let killed: KillFlag = Arc::default();
        let (entered_tx, entered_rx) = mpsc::channel();
        let stdin: BridgeStdin = Arc::new(Mutex::new(Some(Box::new(BlockedStdin {
            entered: entered_tx,
            killed: Arc::clone(&killed),
        }))));
        let mut bridge = Bridge {
            child: Box::new(KillableChild(Arc::clone(&killed))),
            stdin: Arc::clone(&stdin),
            reader: None,
            stopping: Arc::new(AtomicBool::new(false)),
            muted: Arc::new(AtomicBool::new(false)),
            cols: 80,
            rows: 24,
            session_gen: 0,
        };
        let writer = std::thread::spawn(move || send_input_to(&stdin, BridgeInput::Text("x")));
        // Once inside the write, the writer holds the stdin lock.
        entered_rx.recv().unwrap();
        bridge.stop();
        assert!(writer.join().unwrap().is_err(), "the blocked write fails");
        assert!(bridge.stdin.lock().unwrap().is_none(), "stdin closed");
    }

    #[test]
    fn parse_terminal_frame_decodes_bytes() {
        // Exactly the shape herdr 0.9.3 emits on the control stream.
        let payload = base64::engine::general_purpose::STANDARD.encode("HELLO_HERDR_SLICE");
        let line = format!(
            r#"{{"type":"terminal.frame","bytes":"{payload}","encoding":"ansi","full":true,"width":80,"height":24,"seq":1}}"#
        );
        let ev = parse_frame_line(&line).expect("parsed");
        match ev {
            FrameEvent::Frame { bytes, full, seq } => {
                assert!(full);
                assert_eq!(seq, Some(1));
                assert_eq!(bytes, payload, "the base64 is passed through as is");
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(bytes)
                    .unwrap();
                assert_eq!(String::from_utf8(decoded).unwrap(), "HELLO_HERDR_SLICE");
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[test]
    fn parse_frame_defaults_full_and_seq_when_absent() {
        assert_eq!(
            parse_frame_line(r#"{"type":"terminal.frame","bytes":"aGk="}"#),
            Some(FrameEvent::Frame {
                bytes: "aGk=".into(),
                full: false,
                seq: None,
            })
        );
        assert_eq!(
            parse_frame_line(r#"{"seq":42,"full":false,"bytes":"","type":"terminal.frame"}"#),
            Some(FrameEvent::Frame {
                bytes: String::new(),
                full: false,
                seq: Some(42),
            })
        );
    }

    #[test]
    fn parse_frame_ignores_unknown_fields() {
        let line = r#"{"type":"terminal.frame","bytes":"aGk=","full":true,"cursor":{"x":1,"y":2},"title":"zsh","extra":[1,2]}"#;
        assert_eq!(
            parse_frame_line(line),
            Some(FrameEvent::Frame {
                bytes: "aGk=".into(),
                full: true,
                seq: None,
            })
        );
    }

    #[test]
    fn parse_malformed_frames() {
        // A frame without bytes, or with bytes / full / seq of the wrong type.
        assert_eq!(
            parse_frame_line(r#"{"type":"terminal.frame","full":true}"#),
            None
        );
        assert_eq!(
            parse_frame_line(r#"{"type":"terminal.frame","bytes":12}"#),
            None
        );
        assert_eq!(
            parse_frame_line(r#"{"type":"terminal.frame","bytes":"aGk=","full":"yes"}"#),
            None
        );
        assert_eq!(
            parse_frame_line(r#"{"type":"terminal.frame","bytes":"aGk=","seq":-1}"#),
            None
        );
        // Truncated JSON and a line without a type.
        assert_eq!(
            parse_frame_line(r#"{"type":"terminal.frame","bytes":"aG"#),
            None
        );
        assert_eq!(parse_frame_line(r#"{"bytes":"aGk="}"#), None);
        assert_eq!(parse_frame_line("[1,2,3]"), None);
    }

    #[test]
    fn parse_terminal_closed() {
        let line = r#"{"type":"terminal.closed","reason":"detached"}"#;
        assert_eq!(parse_frame_line(line), Some(FrameEvent::Closed));
    }

    #[test]
    fn parse_blank_and_garbage_lines() {
        assert_eq!(parse_frame_line("   "), None);
        assert_eq!(parse_frame_line("not json"), None);
        // An unknown control object parses but is a no-op Other.
        assert_eq!(
            parse_frame_line(r#"{"type":"terminal.bell"}"#),
            Some(FrameEvent::Other)
        );
    }

    #[test]
    fn seq_gaps_are_found_only_between_numbered_frames() {
        assert_eq!(seq_gap(None, Some(5)), None, "the first frame");
        assert_eq!(seq_gap(Some(4), Some(5)), None);
        assert_eq!(seq_gap(Some(4), Some(7)), Some((5, 7)));
        assert_eq!(
            seq_gap(Some(9), Some(1)),
            Some((10, 1)),
            "a restarted count"
        );
        assert_eq!(seq_gap(Some(4), None), None);
        assert_eq!(seq_gap(Some(u64::MAX), Some(0)), None, "wraps");
    }
}
