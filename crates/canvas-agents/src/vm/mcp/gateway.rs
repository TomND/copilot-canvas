//! The gateway's engine: one thread that owns every stream and server
//! process, fed by the link (`Gateway::up`, through a queue: the link's
//! reader never waits for a server) and by the servers' output.
//!
//! A stream is one guest MCP session of one box with one server. It is
//! admitted at `MCP_OPEN` (the box's launch must be in that sandbox, the
//! pack's policy must allow the server for the box's preset, the hub must
//! have the server), and its server is started at its `initialize`:
//! per (box, server) one process (`per_session`, a newer session of the same
//! box replaces the older), or one per server for every box (`shared`).
//!
//! Every message is relayed under the policy: ids are the gateway's on the
//! server's side (so `shared` sessions never see each other's answers) and
//! the guest's own on the guest's side; progress tokens likewise. The guest
//! gets `tools/list` filtered to what it may call and only the `tools`
//! capability; a `tools/call` is checked (policy, a tool the server has,
//! arguments its schema declares, no other parameters) before the server
//! sees it; cancellations pass for requests still live; server-initiated
//! requests are refused unless the policy names the method; a server's log
//! messages and everything else are dropped. Whatever goes down has the
//! server's credentials removed.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use canvas_common::sat::{MCP_CLOSE, MCP_ERROR, MCP_MESSAGE, MCP_OPEN, McpOpen, SAT_FILE_CHUNK};
use crossbeam_channel::{Receiver, Sender};
use serde_json::{Map, Value, json};

use super::jsonrpc::{self, Kind};
use super::policy::{self, ServerPolicy, Verdict};
use super::server::{self, FromServer, Process};
use super::{Down, ServerSource, ServerSpec};
use crate::paths::now_ms;
use crate::vm::launch;

/// Streams a sandbox may have open at once.
pub const MAX_STREAMS: usize = 32;
/// Requests of one stream waiting at its server.
const MAX_LIVE: usize = 64;
/// Bytes queued from the links for the engine; past it a stream's messages are refused.
pub const MAX_QUEUED: usize = 16 << 20;
/// Pages of `tools/list` the gateway reads to know a server's tools.
const MAX_PAGES: u32 = 50;
/// How long a server may take to list its tools for the gateway.
const CATALOGUE_WAIT: Duration = Duration::from_secs(60);
/// How long a server may take to answer its `initialize`.
const INIT_WAIT: Duration = Duration::from_secs(60);
/// The pack is read again after this long.
const PACK_FRESH: Duration = Duration::from_secs(5);

/// Where the gateway reads the pack (`Configured`: settings.json `vm.pack` or `ATELIER_VM_PACK`).
#[derive(Clone, Debug, PartialEq)]
pub enum PackSource {
    Configured,
    Folder(PathBuf),
}

#[derive(Clone, Debug)]
pub struct Options {
    /// A server with nothing in either direction for this long is ended.
    pub idle: Duration,
    pub pack: PackSource,
}

impl Default for Options {
    fn default() -> Options {
        Options { idle: Duration::from_secs(10 * 60), pack: PackSource::Configured }
    }
}

/// A server process the gateway runs.
#[derive(Clone, Debug, PartialEq)]
pub struct Running {
    pub server: String,
    /// The box it serves (empty: `shared`).
    pub r#box: String,
    pub pid: u32,
}

pub enum Event {
    Up { sandbox: String, stream: u64, kind: u8, bytes: Vec<u8> },
    /// A stream's messages came faster than the gateway takes them.
    Overflow { sandbox: String, stream: u64 },
    LinkEnded(String),
    SessionEnded(String),
}

type StreamKey = (String, u64);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Open,
    Initializing,
    Ready,
}

struct Stream {
    term: String,
    server: String,
    policy: ServerPolicy,
    spec: ServerSpec,
    phase: Phase,
    proc: Option<u64>,
    /// The guest's requests waiting at the server: the guest's id (as a key) → the gateway's id (as a key).
    live: HashMap<String, String>,
    /// Requests of the server waiting for the guest's answer (their ids as keys).
    asked: HashSet<String>,
    opened: Instant,
}

enum Pending {
    /// The server's `initialize`.
    Init,
    /// A request of a guest: its stream, its own id, its method, the progress token the gateway gave it.
    Guest { stream: StreamKey, id: Value, method: String, token: Option<String> },
    /// The gateway's own `tools/list`.
    Catalogue { page: u32 },
}

enum Init {
    Waiting,
    Ready(Value),
}

struct Proc {
    /// `(box, server)`; `("", server)` when shared.
    key: (String, String),
    process: Process,
    secrets: Vec<String>,
    next: u64,
    pending: HashMap<String, Pending>,
    init: Init,
    /// Streams whose `initialize` waits for the server's: their keys and ids.
    waiting: Vec<(StreamKey, Value)>,
    streams: Vec<StreamKey>,
    /// The server's tools (name → input schema) as far as known.
    tools: HashMap<String, Value>,
    tools_complete: bool,
    fetching: Option<Instant>,
    /// Calls waiting for the tools to be known.
    deferred: Vec<(StreamKey, Value)>,
    /// Progress tokens the gateway gave: its token → the stream and the guest's token.
    progress: HashMap<String, (StreamKey, Value)>,
    /// The server's requests sent to a guest: their id (as a key) → the stream.
    asks: HashMap<String, StreamKey>,
    /// The stream that sent the newest request (where a shared server's requests go).
    latest: Option<StreamKey>,
    started: Instant,
    last: Instant,
}

struct LoadedPack {
    at: Instant,
    folder: Option<PathBuf>,
    pack: Option<Arc<crate::vm::pack::Pack>>,
    errors: usize,
}

pub struct Engine {
    home: PathBuf,
    servers: Arc<dyn ServerSource>,
    down: Arc<dyn Down>,
    options: Options,
    streams: HashMap<StreamKey, Stream>,
    procs: HashMap<u64, Proc>,
    keys: HashMap<(String, String), u64>,
    next_proc: u64,
    proc_tx: Sender<FromServer>,
    pack: Option<LoadedPack>,
    running: Arc<Mutex<Vec<Running>>>,
    queued: Arc<AtomicUsize>,
}

/// Runs the engine until `events` closes (the gateway was dropped); then every server ends.
pub fn run(home: PathBuf, servers: Arc<dyn ServerSource>, down: Arc<dyn Down>, options: Options, events: Receiver<Event>, running: Arc<Mutex<Vec<Running>>>, queued: Arc<AtomicUsize>) {
    let (proc_tx, proc_rx) = crossbeam_channel::bounded::<FromServer>(1024);
    let mut engine = Engine { home, servers, down, options, streams: HashMap::new(), procs: HashMap::new(), keys: HashMap::new(), next_proc: 0, proc_tx, pack: None, running, queued };
    let tick = Duration::from_millis(100).min(engine.options.idle / 4).max(Duration::from_millis(10));
    loop {
        crossbeam_channel::select! {
            recv(events) -> event => match event {
                Ok(event) => engine.event(event),
                Err(_) => break,
            },
            recv(proc_rx) -> message => {
                if let Ok(message) = message {
                    engine.server_message(message);
                }
            },
            default(tick) => {}
        }
        engine.tick();
    }
    let all: Vec<u64> = engine.procs.keys().copied().collect();
    for proc in all {
        engine.end_proc(proc, None);
    }
}

fn params(message: &Value) -> Map<String, Value> {
    message.get("params").and_then(Value::as_object).cloned().unwrap_or_default()
}

impl Engine {
    fn event(&mut self, event: Event) {
        match event {
            Event::Up { sandbox, stream, kind, bytes } => {
                self.queued.fetch_sub(bytes.len(), Ordering::Relaxed);
                match kind {
                    MCP_OPEN => self.open(&sandbox, stream, &bytes),
                    MCP_MESSAGE => self.guest_message((sandbox, stream), &bytes),
                    MCP_CLOSE => self.close((sandbox, stream), None, false),
                    _ => {}
                }
            }
            Event::Overflow { sandbox, stream } => self.close((sandbox, stream), Some("the guest sent MCP messages faster than the hub takes them"), true),
            Event::LinkEnded(sandbox) => {
                let gone: Vec<StreamKey> = self.streams.keys().filter(|k| k.0 == sandbox).cloned().collect();
                for key in gone {
                    self.close(key, None, false);
                }
            }
            Event::SessionEnded(term) => {
                let gone: Vec<StreamKey> = self.streams.iter().filter(|(_, s)| s.term == term).map(|(k, _)| k.clone()).collect();
                for key in gone {
                    self.close(key, Some("the session ended"), true);
                }
                let procs: Vec<u64> = self.procs.iter().filter(|(_, p)| p.key.0 == term).map(|(id, _)| *id).collect();
                for proc in procs {
                    self.end_proc(proc, Some("the session ended"));
                }
            }
        }
    }

    /// Ends servers idle too long or that never answer their initialize,
    /// streams never initialized, and gives up on tool lists that take too long.
    fn tick(&mut self) {
        let now = Instant::now();
        // Idle: nothing waiting at the server and nothing either way for a while.
        let idle: Vec<u64> = self.procs.iter().filter(|(_, p)| p.pending.is_empty() && now.duration_since(p.last) > self.options.idle).map(|(id, _)| *id).collect();
        for proc in idle {
            self.end_proc(proc, Some("the MCP server was idle"));
        }
        let unused: Vec<StreamKey> = self.streams.iter().filter(|(_, s)| s.proc.is_none() && now.duration_since(s.opened) > self.options.idle).map(|(k, _)| k.clone()).collect();
        for key in unused {
            self.close(key, Some("the MCP session was never initialized"), true);
        }
        let mute: Vec<u64> = self.procs.iter().filter(|(_, p)| matches!(p.init, Init::Waiting) && now.duration_since(p.started) > INIT_WAIT).map(|(id, _)| *id).collect();
        for proc in mute {
            if let Some(waiting) = self.procs.get_mut(&proc).map(|p| std::mem::take(&mut p.waiting)) {
                for (key, id) in waiting {
                    self.answer_error(&key, id, jsonrpc::INTERNAL_ERROR, "the hub's MCP server did not answer its initialize within a minute");
                }
            }
            self.end_proc(proc, Some("the MCP server did not start"));
        }
        let slow: Vec<u64> = self.procs.iter().filter(|(_, p)| p.fetching.is_some_and(|at| now.duration_since(at) > CATALOGUE_WAIT)).map(|(id, _)| *id).collect();
        for proc in slow {
            self.catalogue_done(proc);
        }
    }

    fn send_down(&self, key: &StreamKey, kind: u8, bytes: Vec<u8>) {
        self.down.send(&key.0, key.1, kind, bytes);
    }

    /// Refuses a stream: `MCP_ERROR` with `message`, then `MCP_CLOSE`.
    fn refuse_stream(&self, key: &StreamKey, message: &str) {
        self.send_down(key, MCP_ERROR, json!({ "message": message }).to_string().into_bytes());
        self.send_down(key, MCP_CLOSE, Vec::new());
    }

    /// One JSON-RPC message to a guest, without the server's credentials and
    /// within what the link carries (a larger answer becomes an error).
    fn to_guest(&self, key: &StreamKey, message: Value) {
        let secrets = self.streams.get(key).and_then(|s| s.proc).and_then(|p| self.procs.get(&p)).map(|p| p.secrets.as_slice()).unwrap_or_default();
        let Ok(text) = serde_json::to_string(&message) else { return };
        let text = server::redact(&text, secrets).unwrap_or(text);
        let bytes = if text.len() > SAT_FILE_CHUNK {
            match message.get("id").filter(|_| message.get("method").is_none()) {
                Some(id) => jsonrpc::error(id.clone(), jsonrpc::INTERNAL_ERROR, &format!("the server's answer is too large for the VM link ({} bytes; at most {SAT_FILE_CHUNK})", text.len()), None).to_string().into_bytes(),
                None => return,
            }
        } else {
            text.into_bytes()
        };
        self.send_down(key, MCP_MESSAGE, bytes);
    }

    fn answer_error(&self, key: &StreamKey, id: Value, code: i64, message: &str) {
        self.to_guest(key, jsonrpc::error(id, code, message, None));
    }

    /// A refusal: answered as a JSON-RPC error (the server never sees the
    /// request) and recorded.
    fn refuse(&self, key: &StreamKey, id: Option<Value>, code: i64, tool: &str, refusal: &policy::Refusal) {
        if let Some(stream) = self.streams.get(key) {
            policy::record(
                &self.home,
                &policy::Record { ms: now_ms(), sandbox: key.0.clone(), r#box: stream.term.clone(), kind: "mcp".into(), server: stream.server.clone(), tool: tool.to_owned(), rule: refusal.rule.clone(), why: refusal.why.clone() },
            );
        }
        if let Some(id) = id {
            let data = json!({ "refusedBy": "mcp-policy", "rule": refusal.rule });
            self.to_guest(key, jsonrpc::error(id, code, &format!("refused by the VM's MCP policy: {}", refusal.why), Some(data)));
        }
    }

    fn pack(&mut self) -> Option<(Arc<crate::vm::pack::Pack>, usize)> {
        let folder = match &self.options.pack {
            PackSource::Configured => crate::vm::pack::configured(&self.home),
            PackSource::Folder(folder) => Some(folder.clone()),
        };
        let fresh = self.pack.as_ref().is_some_and(|p| p.at.elapsed() < PACK_FRESH && p.folder == folder);
        if !fresh {
            let (pack, errors) = match &folder {
                Some(folder) => {
                    let (pack, status) = crate::vm::pack::load(folder);
                    (Some(Arc::new(pack)), status.errors())
                }
                None => (None, 0),
            };
            self.pack = Some(LoadedPack { at: Instant::now(), folder, pack, errors });
        }
        let loaded = self.pack.as_ref()?;
        Some((loaded.pack.clone()?, loaded.errors))
    }

    // ---- streams ----

    fn open(&mut self, sandbox: &str, stream: u64, bytes: &[u8]) {
        let key: StreamKey = (sandbox.to_owned(), stream);
        if self.streams.contains_key(&key) {
            // The satellite reuses an id only after the stream ended on its side (a restart).
            self.close(key.clone(), None, false);
        }
        let Ok(open) = serde_json::from_slice::<McpOpen>(bytes) else {
            return self.refuse_stream(&key, "not an MCP stream's opening");
        };
        if self.streams.keys().filter(|k| k.0 == sandbox).count() >= MAX_STREAMS {
            return self.refuse_stream(&key, &format!("this VM has {MAX_STREAMS} MCP sessions open already"));
        }
        match self.admit(sandbox, &open) {
            Ok((policy, spec)) => {
                self.streams.insert(key, Stream { term: open.term, server: open.server, policy, spec, phase: Phase::Open, proc: None, live: HashMap::new(), asked: HashSet::new(), opened: Instant::now() });
            }
            Err((message, refusal)) => {
                if let Some(refusal) = refusal {
                    policy::record(&self.home, &policy::Record { ms: now_ms(), sandbox: sandbox.to_owned(), r#box: open.term.clone(), kind: "mcp".into(), server: open.server.clone(), tool: String::new(), rule: refusal.rule, why: refusal.why });
                }
                self.refuse_stream(&key, &message);
            }
        }
    }

    /// Whether box `open.term` of `sandbox` may use server `open.server`.
    #[allow(clippy::type_complexity)]
    fn admit(&mut self, sandbox: &str, open: &McpOpen) -> Result<(ServerPolicy, ServerSpec), (String, Option<policy::Refusal>)> {
        let plain = |text: String| Err((text, None));
        if !launch::valid_box(&open.term) || open.server.is_empty() || open.server.len() > 100 {
            return plain("not a box's MCP session".into());
        }
        let intent = match launch::load(&self.home, &open.term) {
            Ok(Some(intent)) => intent,
            _ => return plain(format!("box {} is not a VM session of this hub", open.term)),
        };
        if intent.sandbox != sandbox {
            let refusal = policy::Refusal { rule: "box of another VM".into(), why: format!("box {} does not run in this VM", open.term) };
            return Err((refusal.why.clone(), Some(refusal)));
        }
        let Some((pack, errors)) = self.pack() else { return plain("no VM pack is configured".into()) };
        if errors > 0 {
            return plain(format!("the VM pack has {errors} error(s): see `atelier vm pack check`"));
        }
        let vars = policy::Vars {
            branch: intent.placement.branch.clone().filter(|b| !b.trim().is_empty()).unwrap_or_else(|| canvas_model::vm::branch_for_session(&intent.r#box)),
            repo: intent.placement.repo.clone(),
            r#box: open.term.clone(),
        };
        let policy = match policy::for_box(&pack.mcp, intent.placement.mcp.as_deref(), &open.server, vars) {
            Ok(policy) => policy,
            Err(refusal) => return Err((format!("refused by the VM's MCP policy: {}", refusal.why), Some(refusal))),
        };
        match self.servers.spec(&open.server) {
            Some(spec @ ServerSpec::Stdio { .. }) => Ok((policy, spec)),
            Some(ServerSpec::Http { .. }) => plain(format!("{} is a remote (HTTP) MCP server in the hub's configuration; the gateway relays local servers only", open.server)),
            None => plain(format!("the hub has no MCP server named {}", open.server)),
        }
    }

    /// Ends a stream: tells the guest (`tell`, with `why` as its error) and
    /// ends its server when no other session uses it (`per_session`).
    fn close(&mut self, key: StreamKey, why: Option<&str>, tell: bool) {
        let Some(stream) = self.streams.remove(&key) else { return };
        if tell {
            if let Some(why) = why {
                self.send_down(&key, MCP_ERROR, json!({ "message": why }).to_string().into_bytes());
            }
            self.send_down(&key, MCP_CLOSE, Vec::new());
        }
        let Some(id) = stream.proc else { return };
        let Some(proc) = self.procs.get_mut(&id) else { return };
        proc.streams.retain(|k| *k != key);
        proc.waiting.retain(|(k, _)| *k != key);
        proc.deferred.retain(|(k, _)| *k != key);
        proc.pending.retain(|_, p| !matches!(p, Pending::Guest { stream, .. } if *stream == key));
        proc.progress.retain(|_, (k, _)| *k != key);
        let asks: Vec<String> = proc.asks.iter().filter(|(_, k)| **k == key).map(|(id, _)| id.clone()).collect();
        for ask in asks {
            proc.asks.remove(&ask);
            // The guest will not answer: the server is told so.
            if let Ok(id) = serde_json::from_str::<Value>(&ask) {
                proc.process.send(&jsonrpc::error(id, jsonrpc::SESSION_ENDED, "the guest's session ended", None));
            }
        }
        if proc.latest.as_ref() == Some(&key) {
            proc.latest = None;
        }
        if !stream.policy.shared {
            self.end_proc(id, None);
        }
    }

    /// Ends a server process and the streams it served (telling the guests `why`).
    fn end_proc(&mut self, id: u64, why: Option<&str>) {
        let Some(proc) = self.procs.remove(&id) else { return };
        if self.keys.get(&proc.key) == Some(&id) {
            self.keys.remove(&proc.key);
        }
        for key in &proc.streams {
            if let Some(stream) = self.streams.get_mut(key) {
                stream.proc = None;
            }
        }
        for key in proc.streams.clone() {
            self.close(key, Some(why.unwrap_or("the MCP server ended")), true);
        }
        lock(&self.running).retain(|r| r.pid != proc.process.pid);
        drop(proc);
    }

    // ---- from the guest ----

    fn guest_message(&mut self, key: StreamKey, bytes: &[u8]) {
        if !self.streams.contains_key(&key) {
            return;
        }
        let Ok(message) = serde_json::from_slice::<Value>(bytes) else {
            return self.answer_error(&key, Value::Null, jsonrpc::PARSE_ERROR, "not JSON");
        };
        if let Some(proc) = self.streams.get(&key).and_then(|s| s.proc).and_then(|p| self.procs.get_mut(&p)) {
            proc.last = Instant::now();
        }
        match jsonrpc::classify(&message) {
            Ok(Kind::Request { id, method }) => self.guest_request(key, id, &method, message),
            Ok(Kind::Notification { method }) => self.guest_notification(key, &method, message),
            Ok(Kind::Response { id }) => self.guest_response(key, id, message),
            Err(why) if message.is_array() => self.answer_error(&key, Value::Null, jsonrpc::INVALID_REQUEST, &format!("{why}: the link carries one message at a time")),
            Err(why) => self.answer_error(&key, message.get("id").filter(|i| i.is_string() || i.is_number()).cloned().unwrap_or(Value::Null), jsonrpc::INVALID_REQUEST, why),
        }
    }

    fn guest_request(&mut self, key: StreamKey, id: Value, method: &str, message: Value) {
        let Some(stream) = self.streams.get(&key) else { return };
        if stream.live.contains_key(&jsonrpc::key(&id)) {
            return self.answer_error(&key, id, jsonrpc::INVALID_REQUEST, "a request with this id is still waiting for its answer");
        }
        if stream.live.len() >= MAX_LIVE {
            return self.answer_error(&key, id, jsonrpc::INTERNAL_ERROR, "too many requests waiting at the MCP server");
        }
        let phase = stream.phase;
        match method {
            "ping" => self.to_guest(&key, jsonrpc::result(id, json!({}))),
            "initialize" if phase == Phase::Open => self.initialize(key, id, &message),
            "initialize" => self.answer_error(&key, id, jsonrpc::INVALID_REQUEST, "this session is initialized already"),
            _ if phase != Phase::Ready => self.answer_error(&key, id, jsonrpc::INVALID_REQUEST, "the session is not initialized: send initialize first"),
            "tools/list" => {
                let mut forwarded = Map::new();
                if let Some(cursor) = params(&message).get("cursor").filter(|c| c.is_string()) {
                    forwarded.insert("cursor".into(), cursor.clone());
                }
                self.forward(key, id, method, Value::Object(forwarded), None);
            }
            "tools/call" => self.call(key, id, &message),
            other => {
                let refusal = policy::Refusal { rule: policy::DEFAULT_DENY.into(), why: format!("{other} is not available in VM sessions (the gateway relays tools only)") };
                self.refuse(&key, Some(id), jsonrpc::METHOD_NOT_FOUND, other, &refusal);
            }
        }
    }

    /// A guest's `initialize`: its server is started (or joined, when shared).
    fn initialize(&mut self, key: StreamKey, id: Value, message: &Value) {
        let Some(stream) = self.streams.get_mut(&key) else { return };
        let asked = params(message);
        let version = jsonrpc::ask_version(asked.get("protocolVersion").and_then(Value::as_str).unwrap_or_default(), stream.policy.protocol.as_deref());
        let mut capabilities = Map::new();
        let offered = asked.get("capabilities").and_then(Value::as_object).cloned().unwrap_or_default();
        for (name, method) in [("roots", "roots/list"), ("sampling", "sampling/createMessage"), ("elicitation", "elicitation/create")] {
            if let Some(value) = offered.get(name)
                && stream.policy.allows_method(method)
            {
                capabilities.insert(name.into(), value.clone());
            }
        }
        let client_info = asked.get("clientInfo").filter(|c| c.is_object()).cloned().unwrap_or_else(|| json!({ "name": "atelier-mcp-gateway", "version": env!("CARGO_PKG_VERSION") }));
        stream.phase = Phase::Initializing;
        let shared = stream.policy.shared;
        let proc_key = (if shared { String::new() } else { stream.term.clone() }, stream.server.clone());
        let spec = stream.spec.clone();
        if let Some(existing) = self.keys.get(&proc_key).copied() {
            if shared {
                let proc = self.procs.get_mut(&existing).expect("a known process");
                proc.streams.push(key.clone());
                proc.last = Instant::now();
                if let Some(stream) = self.streams.get_mut(&key) {
                    stream.proc = Some(existing);
                }
                match &proc.init {
                    Init::Ready(result) => {
                        let result = result.clone();
                        if let Some(stream) = self.streams.get_mut(&key) {
                            stream.phase = Phase::Ready;
                        }
                        self.to_guest(&key, jsonrpc::result(id, result));
                    }
                    Init::Waiting => proc.waiting.push((key, id)),
                }
                return;
            }
            // A newer session of the same box and server replaces the older.
            self.end_proc(existing, Some("a newer session of this box replaced this one"));
        }
        let ServerSpec::Stdio { command, args, env } = &spec else { return };
        self.next_proc += 1;
        let proc_id = self.next_proc;
        let process = match Process::start(command, args, env, proc_id, self.proc_tx.clone()) {
            Ok(process) => process,
            Err(_) => {
                let server = self.streams.get(&key).map(|s| s.server.clone()).unwrap_or_default();
                self.answer_error(&key, id, jsonrpc::INTERNAL_ERROR, &format!("the hub could not start its MCP server {server}"));
                return self.close(key, None, true);
            }
        };
        lock(&self.running).push(Running { server: proc_key.1.clone(), r#box: proc_key.0.clone(), pid: process.pid });
        let mut proc = Proc {
            key: proc_key.clone(),
            secrets: server::secret_values(&spec),
            process,
            next: 0,
            pending: HashMap::new(),
            init: Init::Waiting,
            waiting: vec![(key.clone(), id)],
            streams: vec![key.clone()],
            tools: HashMap::new(),
            tools_complete: false,
            fetching: None,
            deferred: Vec::new(),
            progress: HashMap::new(),
            asks: HashMap::new(),
            latest: None,
            started: Instant::now(),
            last: Instant::now(),
        };
        let gid = proc.id();
        proc.pending.insert(jsonrpc::key(&gid), Pending::Init);
        proc.process.send(&jsonrpc::request(gid, "initialize", json!({ "protocolVersion": version, "capabilities": capabilities, "clientInfo": client_info })));
        self.procs.insert(proc_id, proc);
        self.keys.insert(proc_key, proc_id);
        if let Some(stream) = self.streams.get_mut(&key) {
            stream.proc = Some(proc_id);
        }
    }

    /// A guest's `tools/call`, checked before the server sees it.
    fn call(&mut self, key: StreamKey, id: Value, message: &Value) {
        let Some(stream) = self.streams.get(&key) else { return };
        let Some(params) = message.get("params").and_then(Value::as_object) else {
            return self.answer_error(&key, id, jsonrpc::INVALID_PARAMS, "tools/call takes params with a name");
        };
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
        if let Some(unknown) = params.keys().find(|k| !matches!(k.as_str(), "name" | "arguments" | "_meta")) {
            let refusal = policy::Refusal { rule: "unknown parameter".into(), why: format!("tools/call has no parameter {unknown:?} here") };
            return self.refuse(&key, Some(id), jsonrpc::INVALID_PARAMS, &name, &refusal);
        }
        if name.is_empty() {
            return self.answer_error(&key, id, jsonrpc::INVALID_PARAMS, "tools/call names a tool");
        }
        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
        let empty = Map::new();
        let Some(argument_map) = (match &arguments {
            Value::Object(map) => Some(map),
            Value::Null => Some(&empty),
            _ => None,
        }) else {
            return self.answer_error(&key, id, jsonrpc::INVALID_PARAMS, "arguments are an object");
        };
        if let Verdict::Refuse(refusal) = stream.policy.call(&name, argument_map) {
            return self.refuse(&key, Some(id), jsonrpc::REFUSED, &name, &refusal);
        }
        let Some(proc_id) = stream.proc else { return };
        let Some(proc) = self.procs.get_mut(&proc_id) else { return };
        let Some(schema) = proc.tools.get(&name).cloned() else {
            if !proc.tools_complete {
                // The server's tools are not known yet: they are read first.
                proc.deferred.push((key, message.clone()));
                if proc.fetching.is_none() {
                    self.fetch(proc_id, 0, None);
                }
                return;
            }
            let refusal = policy::Refusal { rule: "unknown tool".into(), why: format!("the server has no tool {name}") };
            return self.refuse(&key, Some(id), jsonrpc::INVALID_PARAMS, &name, &refusal);
        };
        if let Err(why) = policy::check_arguments(&schema, &arguments) {
            let refusal = policy::Refusal { rule: "unknown argument".into(), why: format!("{name}: {why}") };
            return self.refuse(&key, Some(id), jsonrpc::INVALID_PARAMS, &name, &refusal);
        }
        let mut forwarded = Map::new();
        forwarded.insert("name".into(), name.into());
        if !arguments.is_null() {
            forwarded.insert("arguments".into(), arguments);
        }
        let token = params.get("_meta").and_then(|m| m.get("progressToken")).filter(|t| t.is_string() || t.is_number()).cloned();
        self.forward(key, id, "tools/call", Value::Object(forwarded), token);
    }

    /// Sends a guest's request to its server under a gateway id (and a
    /// gateway progress token for the guest's).
    fn forward(&mut self, key: StreamKey, id: Value, method: &str, mut params: Value, token: Option<Value>) {
        let Some(proc_id) = self.streams.get(&key).and_then(|s| s.proc) else { return };
        let Some(proc) = self.procs.get_mut(&proc_id) else { return };
        let gid = proc.id();
        let ours = token.map(|token| {
            let ours = format!("p{}", proc.id());
            proc.progress.insert(jsonrpc::key(&json!(ours)), (key.clone(), token));
            params["_meta"] = json!({ "progressToken": ours });
            jsonrpc::key(&json!(ours))
        });
        proc.pending.insert(jsonrpc::key(&gid), Pending::Guest { stream: key.clone(), id: id.clone(), method: method.to_owned(), token: ours });
        proc.latest = Some(key.clone());
        let sent = proc.process.send(&jsonrpc::request(gid.clone(), method, params));
        if let Some(stream) = self.streams.get_mut(&key) {
            stream.live.insert(jsonrpc::key(&id), jsonrpc::key(&gid));
        }
        if !sent {
            self.end_proc(proc_id, Some("the MCP server stopped taking messages"));
        }
    }

    fn guest_notification(&mut self, key: StreamKey, method: &str, message: Value) {
        let Some(stream) = self.streams.get_mut(&key) else { return };
        let Some(proc_id) = stream.proc else { return };
        match method {
            // The gateway told the server itself when its initialize was answered.
            "notifications/initialized" => {}
            "notifications/cancelled" => {
                let mut params = params(&message);
                let Some(gid) = params.get("requestId").and_then(|id| stream.live.remove(&jsonrpc::key(id))) else { return };
                let Some(proc) = self.procs.get_mut(&proc_id) else { return };
                if let Some(Pending::Guest { token: Some(token), .. }) = proc.pending.remove(&gid) {
                    proc.progress.remove(&token);
                }
                let Ok(gid) = serde_json::from_str::<Value>(&gid) else { return };
                params.insert("requestId".into(), gid);
                params.retain(|k, _| k == "requestId" || k == "reason");
                proc.process.send(&jsonrpc::notification(method, Value::Object(params)));
            }
            "notifications/roots/list_changed" if stream.policy.allows_method("roots/list") && stream.phase == Phase::Ready => {
                if let Some(proc) = self.procs.get(&proc_id) {
                    proc.process.send(&jsonrpc::notification(method, json!({})));
                }
            }
            _ => {}
        }
    }

    /// A guest's answer to one of the server's requests (only those it was sent).
    fn guest_response(&mut self, key: StreamKey, id: Value, message: Value) {
        let Some(stream) = self.streams.get_mut(&key) else { return };
        if !stream.asked.remove(&jsonrpc::key(&id)) {
            return;
        }
        let Some(proc) = stream.proc.and_then(|p| self.procs.get_mut(&p)) else { return };
        if proc.asks.remove(&jsonrpc::key(&id)).is_some() {
            let mut answer = Map::new();
            for field in ["jsonrpc", "id", "result", "error"] {
                if let Some(value) = message.get(field) {
                    answer.insert(field.into(), value.clone());
                }
            }
            proc.process.send(&Value::Object(answer));
        }
    }

    // ---- from the server ----

    fn server_message(&mut self, message: FromServer) {
        let FromServer { proc: proc_id, message } = message;
        let Some(message) = message else {
            return self.end_proc(proc_id, Some("the MCP server ended"));
        };
        let Some(proc) = self.procs.get_mut(&proc_id) else { return };
        proc.last = Instant::now();
        match jsonrpc::classify(&message) {
            Ok(Kind::Response { id }) => match proc.pending.remove(&jsonrpc::key(&id)) {
                Some(Pending::Init) => self.initialized(proc_id, message),
                Some(Pending::Catalogue { page }) => self.catalogue_page(proc_id, page, &message),
                Some(Pending::Guest { stream, id, method, token }) => {
                    if let Some(token) = token {
                        proc.progress.remove(&token);
                    }
                    let mut message = message;
                    if method == "tools/list" {
                        self.learn_tools(proc_id, &message);
                        if let (Some(tools), Some(stream)) = (message.pointer_mut("/result/tools").and_then(Value::as_array_mut), self.streams.get(&stream)) {
                            tools.retain(|tool| tool.get("name").and_then(Value::as_str).is_some_and(|name| stream.policy.lists(name)));
                        }
                    }
                    let Some(live) = self.streams.get_mut(&stream) else { return };
                    live.live.remove(&jsonrpc::key(&id));
                    message["id"] = id;
                    self.to_guest(&stream, message);
                }
                None => {}
            },
            Ok(Kind::Request { id, method }) => self.server_request(proc_id, id, &method, message),
            Ok(Kind::Notification { method }) => self.server_notification(proc_id, &method, message),
            Err(_) => {}
        }
    }

    /// The server answered its `initialize`: the version is checked, the
    /// capabilities cut to tools, and every session waiting for it answered.
    fn initialized(&mut self, proc_id: u64, message: Value) {
        let Some(proc) = self.procs.get_mut(&proc_id) else { return };
        let waiting = std::mem::take(&mut proc.waiting);
        let pinned = waiting.first().and_then(|(k, _)| self.streams.get(k)).and_then(|s| s.policy.protocol.clone());
        let version = message.pointer("/result/protocolVersion").and_then(Value::as_str).unwrap_or_default().to_owned();
        let problem = match (&message.get("result"), pinned) {
            (None, _) => Some(message["error"]["message"].as_str().unwrap_or("the MCP server refused to initialize").to_owned()),
            (Some(_), _) if !jsonrpc::known(&version) => Some(format!("the MCP server speaks protocol {version:?}, which the gateway does not (it speaks {})", jsonrpc::VERSIONS.join(", "))),
            (Some(_), Some(pinned)) if pinned != version => Some(format!("the MCP server speaks protocol {version}; the VM's policy pins {pinned}")),
            _ => None,
        };
        if let Some(problem) = problem {
            for (key, id) in &waiting {
                self.answer_error(key, id.clone(), jsonrpc::INTERNAL_ERROR, &problem);
            }
            return self.end_proc(proc_id, Some(&problem));
        }
        let mut result = Map::new();
        result.insert("protocolVersion".into(), version.into());
        let tools = message.pointer("/result/capabilities/tools").filter(|t| t.is_object()).cloned().unwrap_or_else(|| json!({}));
        result.insert("capabilities".into(), json!({ "tools": tools }));
        for field in ["serverInfo", "instructions"] {
            if let Some(value) = message["result"].get(field) {
                result.insert(field.into(), value.clone());
            }
        }
        let result = Value::Object(result);
        proc.init = Init::Ready(result.clone());
        proc.process.send(&jsonrpc::notification("notifications/initialized", json!({})));
        for (key, id) in waiting {
            if let Some(stream) = self.streams.get_mut(&key) {
                stream.phase = Phase::Ready;
                self.to_guest(&key, jsonrpc::result(id, result.clone()));
            }
        }
    }

    fn learn_tools(&mut self, proc_id: u64, message: &Value) {
        let Some(proc) = self.procs.get_mut(&proc_id) else { return };
        for tool in message.pointer("/result/tools").and_then(Value::as_array).into_iter().flatten() {
            if let Some(name) = tool.get("name").and_then(Value::as_str) {
                proc.tools.insert(name.to_owned(), tool.get("inputSchema").cloned().unwrap_or_else(|| json!({ "type": "object" })));
            }
        }
    }

    /// Asks the server for a page of its tools, for the gateway itself.
    fn fetch(&mut self, proc_id: u64, page: u32, cursor: Option<Value>) {
        let Some(proc) = self.procs.get_mut(&proc_id) else { return };
        proc.fetching.get_or_insert_with(Instant::now);
        let gid = proc.id();
        proc.pending.insert(jsonrpc::key(&gid), Pending::Catalogue { page });
        let params = match cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        if !proc.process.send(&jsonrpc::request(gid, "tools/list", params)) {
            self.end_proc(proc_id, Some("the MCP server stopped taking messages"));
        }
    }

    fn catalogue_page(&mut self, proc_id: u64, page: u32, message: &Value) {
        self.learn_tools(proc_id, message);
        match message.pointer("/result/nextCursor").filter(|c| c.is_string()) {
            Some(cursor) if page + 1 < MAX_PAGES => self.fetch(proc_id, page + 1, Some(cursor.clone())),
            _ => self.catalogue_done(proc_id),
        }
    }

    /// The server's tools are known (or as known as they will be): the calls that waited go on.
    fn catalogue_done(&mut self, proc_id: u64) {
        let Some(proc) = self.procs.get_mut(&proc_id) else { return };
        proc.tools_complete = true;
        proc.fetching = None;
        proc.pending.retain(|_, p| !matches!(p, Pending::Catalogue { .. }));
        for (key, message) in std::mem::take(&mut proc.deferred) {
            if let Some(id) = message.get("id").cloned() {
                self.call(key, id, &message);
            }
        }
    }

    /// A request of the server: `ping` is answered here; `roots/list`,
    /// `sampling/createMessage` and `elicitation/create` go to the guest only
    /// when the policy names them; everything else is refused.
    fn server_request(&mut self, proc_id: u64, id: Value, method: &str, message: Value) {
        let Some(proc) = self.procs.get_mut(&proc_id) else { return };
        if method == "ping" {
            proc.process.send(&jsonrpc::result(id, json!({})));
            return;
        }
        let target = proc.latest.clone().filter(|k| proc.streams.contains(k)).or_else(|| proc.streams.first().cloned());
        let relayed = matches!(method, "roots/list" | "sampling/createMessage" | "elicitation/create");
        let allowed = relayed && target.as_ref().and_then(|k| self.streams.get(k)).is_some_and(|s| s.policy.allows_method(method) && s.phase == Phase::Ready);
        if allowed && let Some(target) = target {
            proc.asks.insert(jsonrpc::key(&id), target.clone());
            if let Some(stream) = self.streams.get_mut(&target) {
                stream.asked.insert(jsonrpc::key(&id));
            }
            return self.to_guest(&target, message);
        }
        let (code, why) = if relayed { (jsonrpc::REFUSED, format!("{method} is not allowed by the VM's MCP policy")) } else { (jsonrpc::METHOD_NOT_FOUND, format!("the gateway does not relay {method}")) };
        proc.process.send(&jsonrpc::error(id, code, &why, None));
        if relayed && let Some(target) = target {
            self.refuse(&target, None, code, method, &policy::Refusal { rule: policy::DEFAULT_DENY.into(), why });
        }
    }

    fn server_notification(&mut self, proc_id: u64, method: &str, message: Value) {
        let Some(proc) = self.procs.get_mut(&proc_id) else { return };
        match method {
            "notifications/progress" => {
                let Some((key, token)) = message.pointer("/params/progressToken").and_then(|t| proc.progress.get(&jsonrpc::key(t))).cloned() else { return };
                let mut message = message;
                message["params"]["progressToken"] = token;
                self.to_guest(&key, message);
            }
            "notifications/cancelled" => {
                let Some(id) = message.pointer("/params/requestId").cloned() else { return };
                let Some(key) = proc.asks.remove(&jsonrpc::key(&id)) else { return };
                if let Some(stream) = self.streams.get_mut(&key) {
                    stream.asked.remove(&jsonrpc::key(&id));
                }
                self.to_guest(&key, message);
            }
            "notifications/tools/list_changed" => {
                proc.tools.clear();
                proc.tools_complete = false;
                for key in proc.streams.clone() {
                    self.to_guest(&key, jsonrpc::notification(method, json!({})));
                }
            }
            // Log messages may carry what the guest must not see; resources and prompts are not offered.
            _ => {}
        }
    }
}

impl Proc {
    /// The next gateway id on this server.
    fn id(&mut self) -> Value {
        self.next += 1;
        json!(self.next)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
