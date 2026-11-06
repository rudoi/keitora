//! An in-process stand-in for `codex app-server`, for driving `AppServer` through
//! `from_streams` without spawning anything.
//!
//! The client writes to a [`ChannelWriter`]; a pump thread here assembles those
//! bytes into lines, records each parsed JSON message, and hands it to a handler
//! that answers through [`FakeOut`]. When the client closes its writer the pump
//! ends and drops the output senders, so the client's readers see EOF exactly as
//! they would on a real child exit.

use std::io::{Read, Write};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

/// A `Read` over a byte channel: blocks on `recv`, returns `Ok(0)` once every
/// sender is gone.
pub(super) struct ChannelReader {
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    pos: usize,
}

impl ChannelReader {
    fn new(rx: Receiver<Vec<u8>>) -> Self {
        ChannelReader {
            rx,
            pending: Vec::new(),
            pos: 0,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.pending.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.pending = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len() - self.pos);
        buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// A `Write` into a byte channel: one send per write; flush is a no-op. A write
/// after the fake has gone away fails with `BrokenPipe`, like a dead child's stdin.
pub(super) struct ChannelWriter(Sender<Vec<u8>>);

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .send(buf.to_vec())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The fake's output side: the client's stdout and stderr. Either is `None` once
/// [`close`](FakeOut::close)d.
pub(super) struct FakeOut {
    stdout: Mutex<Option<Sender<Vec<u8>>>>,
    stderr: Mutex<Option<Sender<Vec<u8>>>>,
}

fn send(slot: &Mutex<Option<Sender<Vec<u8>>>>, bytes: Vec<u8>) {
    if let Some(tx) = slot.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        let _ = tx.send(bytes);
    }
}

impl FakeOut {
    fn line(&self, v: Value) {
        send(&self.stdout, format!("{v}\n").into_bytes());
    }

    pub fn respond(&self, id: &Value, result: Value) {
        self.line(json!({ "id": id, "result": result }));
    }

    pub fn respond_error(&self, id: &Value, code: i64, message: &str) {
        self.line(json!({ "id": id, "error": { "code": code, "message": message } }));
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.line(json!({ "method": method, "params": params }));
    }

    /// A server-initiated request; `id` may be a string.
    pub fn server_request(&self, id: Value, method: &str) {
        self.line(json!({ "id": id, "method": method, "params": {} }));
    }

    pub fn stderr(&self, text: &str) {
        send(&self.stderr, format!("{text}\n").into_bytes());
    }

    /// Drop stdout and stderr, like a child that exited: the client's readers see
    /// EOF while its writes still succeed.
    pub fn close(&self) {
        self.stdout.lock().unwrap_or_else(|p| p.into_inner()).take();
        self.stderr.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
}

#[derive(Default)]
struct HandleState {
    received: Mutex<Vec<Value>>,
    teardown_grace: Mutex<Option<Duration>>,
}

/// Inspection handle, cloneable and usable after the client is dropped.
#[derive(Clone, Default)]
pub(super) struct FakeHandle(Arc<HandleState>);

impl FakeHandle {
    /// Every JSON line the client wrote, in order.
    pub fn received(&self) -> Vec<Value> {
        self.0
            .received
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// `Some(grace)` once the teardown closure ran.
    pub fn teardown_grace(&self) -> Option<Duration> {
        *self
            .0
            .teardown_grace
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// The teardown closure to pass to `from_streams`; records its grace argument.
    pub fn teardown(&self) -> Box<dyn FnOnce(Duration) + Send> {
        let state = self.0.clone();
        Box::new(move |grace| {
            *state
                .teardown_grace
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(grace);
        })
    }
}

/// How the scripted fake answers one request.
pub(super) enum Reply {
    Result(Value),
    Error(i64, &'static str),
    /// Never answer.
    Silent,
}

/// A scripted conversation: `initialize`, then one `externalAgentConfig/import`.
pub(super) struct Script {
    pub initialize: Reply,
    pub import: Reply,
    /// Notifications written before the import response.
    pub before_import_response: Vec<(String, Value)>,
    /// Notifications written after the import response.
    pub after_import_response: Vec<(String, Value)>,
    /// Side effect at import time (append to a file, etc.).
    pub on_import: Option<Box<dyn FnMut() + Send>>,
    pub stderr_at_start: Option<String>,
}

impl Script {
    /// The common happy path: import answers `{importId}` and is followed by a
    /// completion recording `target` as the imported session thread.
    pub fn default_success(import_id: &str, target: &str) -> Script {
        Script {
            initialize: Reply::Result(json!({ "userAgent": "codex-fake/0.0" })),
            import: Reply::Result(json!({ "importId": import_id })),
            before_import_response: Vec::new(),
            after_import_response: vec![(
                super::EXTERNAL_AGENT_IMPORT_COMPLETED.to_string(),
                json!({
                    "importId": import_id,
                    "itemTypeResults": [{
                        "itemType": "SESSIONS",
                        "successes": [{ "target": target }],
                        "failures": []
                    }]
                }),
            )],
            on_import: None,
            stderr_at_start: None,
        }
    }
}

/// The client-facing streams plus an inspection handle.
pub(super) struct FakeServer {
    /// The client's stdout.
    pub reader: ChannelReader,
    /// The client's stderr.
    pub stderr: ChannelReader,
    /// The client's stdin.
    pub writer: ChannelWriter,
    pub handle: FakeHandle,
}

impl FakeServer {
    /// A fake that calls `handler` for every JSON line the client writes.
    pub fn new(handler: impl FnMut(&Value, &FakeOut) + Send + 'static) -> FakeServer {
        Self::start(None, handler)
    }

    /// A fake that plays `script`.
    pub fn scripted(script: Script) -> FakeServer {
        let Script {
            initialize,
            import,
            before_import_response,
            after_import_response,
            mut on_import,
            stderr_at_start,
        } = script;
        let answer = |reply: &Reply, id: &Value, out: &FakeOut| match reply {
            Reply::Result(v) => out.respond(id, v.clone()),
            Reply::Error(code, message) => out.respond_error(id, *code, message),
            Reply::Silent => {}
        };
        Self::start(stderr_at_start, move |msg, out| {
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            match msg.get("method").and_then(Value::as_str) {
                Some("initialize") => answer(&initialize, &id, out),
                Some("externalAgentConfig/import") => {
                    if let Some(f) = on_import.as_mut() {
                        f();
                    }
                    for (m, p) in &before_import_response {
                        out.notify(m, p.clone());
                    }
                    answer(&import, &id, out);
                    for (m, p) in &after_import_response {
                        out.notify(m, p.clone());
                    }
                }
                _ => {}
            }
        })
    }

    fn start(
        stderr_at_start: Option<String>,
        mut handler: impl FnMut(&Value, &FakeOut) + Send + 'static,
    ) -> FakeServer {
        let (stdout_tx, stdout_rx) = channel::<Vec<u8>>();
        let (stderr_tx, stderr_rx) = channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = channel::<Vec<u8>>();
        let out = FakeOut {
            stdout: Mutex::new(Some(stdout_tx)),
            stderr: Mutex::new(Some(stderr_tx)),
        };
        if let Some(text) = stderr_at_start {
            out.stderr(&text);
        }
        let handle = FakeHandle::default();
        let state = handle.0.clone();
        std::thread::spawn(move || {
            let mut buf: Vec<u8> = Vec::new();
            // Ends when the client drops its writer; `out` drops with it.
            while let Ok(chunk) = stdin_rx.recv() {
                buf.extend_from_slice(&chunk);
                while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=nl).collect();
                    let Ok(v) = serde_json::from_slice::<Value>(&line) else {
                        continue;
                    };
                    state
                        .received
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(v.clone());
                    handler(&v, &out);
                }
            }
        });
        FakeServer {
            reader: ChannelReader::new(stdout_rx),
            stderr: ChannelReader::new(stderr_rx),
            writer: ChannelWriter(stdin_tx),
            handle,
        }
    }
}
