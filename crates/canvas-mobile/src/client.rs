//! The client object the app holds: one per paired service. It survives
//! reconnects (the app opens a new WebSocket and calls `attach` again); each
//! connection runs on its own reader and writer threads.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use canvas_common::handshake::{self, Refused};
use canvas_common::protocol::{self, CAP_COMPRESS, CAP_FRAME8, CAP_NOTIFY, CAP_TUNNEL_ACCESS, ClientInfo, ClientMsg, Input, Key, MAX_FRAME, PRE_AUTH_FRAME, ServiceMsg};
use canvas_common::wire::{Beat, FrameReader, FrameWriter, Liveness};
use canvas_model::rpc::{Request, Response};
use crossbeam_channel::{RecvTimeoutError, Sender, bounded, unbounded};

use crate::link::{Inbox, InboxReader, Transport, TransportWriter};
use crate::model::{ConnState, CoreError, CoreEvent, FolderListing, KeyCode, NewSession, Overview, Screen};
use crate::state::{Effect, State};

/// What the app tells the core about itself and its pairing.
#[derive(uniffi::Record, Clone, Debug)]
pub struct ClientConfig {
    /// Shown in the service's log ("iphone").
    pub client_name: String,
    /// The id the service gave this phone when it paired (`pc-…`).
    pub remote_id: String,
    /// The phone's secret (32 bytes, from the Keychain).
    pub secret: Vec<u8>,
    /// The app's version.
    pub build: String,
}

/// Receives the core's events (on a background thread).
#[uniffi::export(with_foreign)]
pub trait Listener: Send + Sync {
    fn on_event(&self, event: CoreEvent);
}

/// How long the handshake may take (the tunnel's relay adds round trips).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

fn describe(error: &CoreError) -> String {
    match error {
        CoreError::Failed { message } => message.clone(),
    }
}

fn now_ms() -> u64 {
    canvas_common::paths::now_ms()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The current connection.
struct Link {
    generation: u64,
    inbox: Arc<Inbox>,
    transport: Arc<dyn Transport>,
    /// Messages for the writer thread, once authenticated.
    out: Option<Sender<ClientMsg>>,
}

struct Shared {
    config: ClientConfig,
    listener: Arc<dyn Listener>,
    state: Mutex<State>,
    /// Names this phone's input across reconnects (applied at most once).
    session: [u8; 16],
    /// Last input sequence number sent, and the highest the service applied.
    sent: AtomicU64,
    acked: AtomicU64,
    generation: AtomicU64,
    link: Mutex<Option<Link>>,
    rpc_next: AtomicU64,
    rpc_pending: Mutex<HashMap<u64, Sender<String>>>,
    /// When "offline: input not sent" was last said.
    offline_said: Mutex<Option<Instant>>,
    /// This phone's notification registration, sent on every connection.
    push: Mutex<Option<ClientMsg>>,
}

/// A paired service: connections, state and commands.
#[derive(uniffi::Object)]
pub struct Client {
    shared: Arc<Shared>,
}

#[uniffi::export]
impl Client {
    #[uniffi::constructor]
    pub fn new(config: ClientConfig, listener: Arc<dyn Listener>) -> Arc<Self> {
        let session: [u8; 16] = protocol::nonce()[..16].try_into().expect("16 bytes");
        Arc::new(Self {
            shared: Arc::new(Shared {
                config,
                listener,
                state: Mutex::new(State::default()),
                session,
                sent: AtomicU64::new(0),
                acked: AtomicU64::new(0),
                generation: AtomicU64::new(0),
                link: Mutex::new(None),
                rpc_next: AtomicU64::new(1),
                rpc_pending: Mutex::new(HashMap::new()),
                offline_said: Mutex::new(None),
                push: Mutex::new(None),
            }),
        })
    }

    // ---------- connection ----------

    /// A WebSocket to the service is open: authenticate on it and serve.
    /// Replaces the previous connection.
    pub fn attach(&self, transport: Arc<dyn Transport>) {
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let inbox = Arc::new(Inbox::default());
        let previous = lock(&self.shared.link).replace(Link { generation, inbox: inbox.clone(), transport: transport.clone(), out: None });
        if let Some(previous) = previous {
            previous.inbox.close();
            previous.transport.close();
        }
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("canvas-link".into())
            .spawn(move || run_link(shared, generation, inbox, transport))
            .expect("spawn the connection thread");
    }

    /// Bytes the WebSocket received (binary messages, in order).
    pub fn receive(&self, data: Vec<u8>) {
        if let Some(link) = lock(&self.shared.link).as_ref() {
            link.inbox.push(&data);
        }
    }

    /// The WebSocket closed (or failed).
    pub fn transport_closed(&self, reason: String) {
        let link = lock(&self.shared.link).take();
        if let Some(link) = link {
            link.inbox.close();
            self.shared.disconnected(link.generation, ConnState::Offline, reason);
        }
    }

    /// The app goes to the background: close the connection.
    pub fn detach(&self) {
        let link = lock(&self.shared.link).take();
        if let Some(link) = link {
            link.inbox.close();
            link.transport.close();
            self.shared.disconnected(link.generation, ConnState::Offline, "paused".into());
        }
    }

    pub fn conn_state(&self) -> ConnState {
        lock(&self.shared.state).conn
    }

    // ---------- reading ----------

    pub fn overview(&self) -> Overview {
        lock(&self.shared.state).overview(now_ms())
    }

    /// The screen of a terminal: what changed since it was last taken (`full`: all of it).
    pub fn take_screen(&self, id: String, full: bool) -> Option<Screen> {
        lock(&self.shared.state).take_screen(&id, full)
    }

    /// The text of a selection (viewport cells, inclusive; soft-wrapped rows rejoin).
    pub fn selection_text(&self, id: String, from_col: u16, from_row: u16, to_col: u16, to_row: u16) -> String {
        lock(&self.shared.state).selection_text(&id, (from_col, from_row), (to_col, to_row))
    }

    // ---------- terminals ----------

    /// Shows a terminal. `fit`: at the phone's size (`cols`×`rows`; the
    /// terminal follows this phone until another client types in it), else
    /// mirrored at the desktop's size.
    pub fn open_terminal(&self, id: String, fit: bool, cols: u16, rows: u16) {
        let effects = lock(&self.shared.state).open_terminal(&id, fit.then_some((cols, rows)), now_ms());
        self.shared.run(effects);
    }

    pub fn resize_terminal(&self, id: String, cols: u16, rows: u16) {
        let effects = lock(&self.shared.state).resize_terminal(&id, cols, rows);
        self.shared.run(effects);
    }

    pub fn set_fit(&self, id: String, fit: bool, cols: u16, rows: u16) {
        let effects = lock(&self.shared.state).set_fit(&id, fit.then_some((cols, rows)));
        self.shared.run(effects);
    }

    pub fn close_terminal(&self) {
        let effects = lock(&self.shared.state).close_terminal();
        self.shared.run(effects);
    }

    /// A key with modifiers (`MOD_SHIFT` 1, `MOD_ALT` 2, `MOD_CTRL` 4).
    pub fn send_key(&self, id: String, key: KeyCode, mods: u8) {
        let key = match key {
            KeyCode::Char { text } => match text.chars().next() {
                Some(c) => Key::Char(c),
                None => return,
            },
            KeyCode::Enter => Key::Enter,
            KeyCode::Tab => Key::Tab,
            KeyCode::BackTab => Key::BackTab,
            KeyCode::Backspace => Key::Backspace,
            KeyCode::Esc => Key::Esc,
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Left => Key::Left,
            KeyCode::Right => Key::Right,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::Insert => Key::Insert,
            KeyCode::Delete => Key::Delete,
            KeyCode::F { n } => Key::F(n),
        };
        // Characters carry their shifted form already.
        let mods = if matches!(key, Key::Char(_)) { mods & !protocol::MOD_SHIFT } else { mods };
        self.shared.input(&id, Input::Key { key, mods });
    }

    /// Typed text (the software keyboard, dictation): one key per character;
    /// a newline is Enter.
    pub fn send_text(&self, id: String, text: String) {
        for c in text.chars() {
            let key = match c {
                '\n' | '\r' => Key::Enter,
                '\t' => Key::Tab,
                c => Key::Char(c),
            };
            if !self.shared.input(&id, Input::Key { key, mods: 0 }) {
                return;
            }
        }
    }

    /// Pasted text (bracketed when the program asked for it).
    pub fn paste(&self, id: String, text: String) {
        self.shared.input(&id, Input::Paste(text));
    }

    /// A prompt written in the app: pasted, then Enter.
    pub fn submit(&self, id: String, text: String) {
        if self.shared.input(&id, Input::Paste(text)) {
            self.shared.input(&id, Input::Key { key: Key::Enter, mods: 0 });
        }
    }

    /// A wheel notch over a cell (the program scrolls its own view when it
    /// reports the mouse; else the scrollback moves).
    pub fn wheel(&self, id: String, up: bool, col: u16, row: u16) {
        self.shared.send_online(ClientMsg::Wheel { id, up, col, row, mods: 0 });
    }

    /// A tap reaches the program when it reports the mouse.
    pub fn click(&self, id: String, col: u16, row: u16) {
        self.shared.send_online(ClientMsg::Click { id, col, row, button: 0, mods: 0 });
    }

    /// Scrolls the scrollback (positive: towards history; shared with every client).
    pub fn scroll(&self, id: String, delta: i32) {
        self.shared.send_online(ClientMsg::Scroll { id, delta });
    }

    // ---------- sessions ----------

    /// Starts or resumes a session's terminal at `cols`×`rows` (blocking: it
    /// checks the folder on the service's machine first).
    pub fn open_session(&self, id: String, cols: u16, rows: u16) -> Result<(), CoreError> {
        let folder = lock(&self.shared.state).launch_folder(&id);
        if let Some(folder) = folder
            && !lock(&self.shared.state).running().contains(&id)
            && let Ok(Response::Path(info)) = self.shared.call(Request::PathInfo { path: folder.clone() })
            && !info.dir
        {
            return Err(CoreError::msg(format!("Folder not found on the service's machine: {folder}")));
        }
        let effects = lock(&self.shared.state).launch(&id, cols, rows, None, now_ms()).map_err(CoreError::msg)?;
        self.shared.run(effects);
        Ok(())
    }

    /// Creates a session box and starts it (blocking: checks the folder).
    /// Returns its id.
    pub fn new_session(&self, request: NewSession, cols: u16, rows: u16) -> Result<String, CoreError> {
        let folder = request.cwd.trim().trim_matches('"').to_owned();
        if folder.is_empty() {
            return Err(CoreError::msg("Choose a folder."));
        }
        let info = match self.shared.call(Request::PathInfo { path: folder.clone() })? {
            Response::Path(info) => info,
            other => return Err(CoreError::msg(format!("unexpected answer: {other:?}"))),
        };
        if !info.dir {
            return Err(CoreError::msg(format!("Folder not found on the service's machine: {folder}")));
        }
        let prompt = Some(request.prompt.trim().to_owned()).filter(|p| !p.is_empty());
        let fork = if request.fork { lock(&self.shared.state).fork_kind(&request.parent) } else { None };
        let id = lock(&self.shared.state).add_session(&request, &info.absolute).map_err(CoreError::msg)?;
        let fork_from = match fork {
            // Copilot: the service copies the parent's history to the new id; the copy resumes.
            Some(crate::state::Fork::Copy { parent, .. }) => {
                let name = Some(request.name.trim().to_owned()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Fork".into());
                if let Err(error) = self.shared.call(Request::Fork { parent, child: id.clone(), name, cwd: Some(info.absolute.clone()) }) {
                    // The box stays (fresh): opening it starts a new conversation.
                    return Err(CoreError::msg(format!("The fork failed: {}", describe(&error))));
                }
                if let Some(node) = lock(&self.shared.state).doc.node_mut(&id) {
                    node.fresh = false;
                }
                None
            }
            Some(crate::state::Fork::Native { parent }) => Some(parent),
            None => None,
        };
        let effects = lock(&self.shared.state).launch_fork(&id, cols, rows, prompt, fork_from, now_ms()).map_err(CoreError::msg)?;
        self.shared.run(effects);
        Ok(id)
    }

    pub fn stop_session(&self, id: String) {
        let effects = lock(&self.shared.state).stop(&id);
        self.shared.run(effects);
    }

    pub fn rename_session(&self, id: String, name: String) -> Result<(), CoreError> {
        let effects = lock(&self.shared.state).rename(&id, &name).map_err(CoreError::msg)?;
        self.shared.run(effects);
        Ok(())
    }

    pub fn remove_session(&self, id: String) -> Result<(), CoreError> {
        let effects = lock(&self.shared.state).remove(&id).map_err(CoreError::msg)?;
        self.shared.run(effects);
        Ok(())
    }

    pub fn mark_reviewed(&self, id: String) {
        let effects = lock(&self.shared.state).mark_reviewed(&id, now_ms());
        self.shared.run(effects);
    }

    // ---------- notifications ----------

    /// This phone's APNs device token (hex; empty: no notifications), the
    /// environment ("sandbox" for development builds), the app's bundle id
    /// and what the phone calls this machine. Sent now and on every connection.
    pub fn set_push(&self, token: String, environment: String, bundle: String, label: String) {
        let message = ClientMsg::PushRegister { token, environment, bundle, label };
        *lock(&self.shared.push) = Some(message.clone());
        self.shared.send(message);
    }

    // ---------- passive sessions ----------

    pub fn run_passive(&self, id: String) -> Result<(), CoreError> {
        let effects = lock(&self.shared.state).run_passive(&id).map_err(CoreError::msg)?;
        self.shared.run(effects);
        Ok(())
    }

    pub fn set_passive_paused(&self, id: String, paused: bool) -> Result<(), CoreError> {
        let effects = lock(&self.shared.state).set_paused(&id, paused).map_err(CoreError::msg)?;
        self.shared.run(effects);
        Ok(())
    }

    /// Drops the messages waiting to be typed into a session.
    pub fn discard_mail(&self, id: String) {
        let effects = lock(&self.shared.state).discard_mail(&id);
        self.shared.run(effects);
    }

    // ---------- subsessions ----------

    /// Whether a subsession can fork this session's conversation (asks the
    /// service whether a Copilot session has history: blocking).
    pub fn can_fork(&self, id: String) -> bool {
        let fork = lock(&self.shared.state).fork_kind(&id);
        match fork {
            Some(crate::state::Fork::Copy { agent, parent }) => matches!(self.shared.call(Request::HasHistory { agent, id: parent }), Ok(Response::Flag(true))),
            Some(crate::state::Fork::Native { .. }) => true,
            None => false,
        }
    }

    // ---------- images ----------

    /// Puts a picture (PNG) on the clipboard of the service's machine, then
    /// sends Ctrl+V so the agent attaches it (blocking: the upload).
    pub fn paste_image(&self, id: String, png: Vec<u8>) -> Result<(), CoreError> {
        if png.len() > 20 << 20 {
            return Err(CoreError::msg("The picture is too large (at most 20 MB)."));
        }
        self.shared.call(Request::SetClipboardImage { png })?;
        self.shared.input(&id, Input::Key { key: Key::Char('v'), mods: protocol::MOD_CTRL });
        Ok(())
    }

    // ---------- the service's machine ----------

    /// Sub-folders of a folder on the service's machine (`None`: the drives).
    pub fn list_folder(&self, folder: Option<String>) -> Result<FolderListing, CoreError> {
        match self.shared.call(Request::ListDir { dir: folder.clone() })? {
            Response::Texts(entries) => Ok(FolderListing { folder: folder.unwrap_or_default(), entries }),
            other => Err(CoreError::msg(format!("unexpected answer: {other:?}"))),
        }
    }

    /// The start of a text file on the service's machine (read-only viewer).
    pub fn read_text(&self, path: String) -> Result<String, CoreError> {
        match self.shared.call(Request::ReadText { path, max_bytes: 512 * 1024 })? {
            Response::Text(text) => Ok(text),
            other => Err(CoreError::msg(format!("unexpected answer: {other:?}"))),
        }
    }
}

impl Shared {
    fn emit(&self, event: CoreEvent) {
        self.listener.on_event(event);
    }

    /// Carries out effects: messages go out (while online), events to the app.
    fn run(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Send(message) => {
                    self.send(message);
                }
                Effect::Emit(event) => self.emit(event),
            }
        }
    }

    /// Queues a message; false when offline.
    fn send(&self, message: ClientMsg) -> bool {
        let link = lock(&self.link);
        match link.as_ref().and_then(|l| l.out.as_ref()) {
            Some(out) => out.send(message).is_ok(),
            None => false,
        }
    }

    /// Like `send`, but says so when the message could not go out.
    fn send_online(&self, message: ClientMsg) -> bool {
        let sent = self.send(message);
        if !sent {
            self.say_offline();
        }
        sent
    }

    /// "Offline: input not sent." (at most every 3 s).
    fn say_offline(&self) {
        let mut said = lock(&self.offline_said);
        if said.is_none_or(|at| at.elapsed() > Duration::from_secs(3)) {
            *said = Some(Instant::now());
            drop(said);
            self.emit(CoreEvent::Notice { text: "Offline: input not sent.".into(), error: true });
        }
    }

    /// Terminal input: numbered (applied at most once), never queued offline.
    fn input(&self, id: &str, input: Input) -> bool {
        let message = match input {
            Input::Focus(_) => ClientMsg::Input { id: id.to_owned(), input },
            input => {
                let link = lock(&self.link);
                let Some(out) = link.as_ref().and_then(|l| l.out.as_ref()) else {
                    drop(link);
                    self.say_offline();
                    return false;
                };
                // Numbered under the link lock: sequence numbers leave in order.
                let seq = self.sent.fetch_add(1, Ordering::AcqRel) + 1;
                return out.send(ClientMsg::Input8 { id: id.to_owned(), input, seq }).is_ok();
            }
        };
        self.send_online(message)
    }

    /// A backend request (blocking, with the request's own timeout).
    fn call(&self, request: Request) -> Result<Response, CoreError> {
        let id = self.rpc_next.fetch_add(1, Ordering::Relaxed);
        let json = serde_json::to_string(&request).map_err(|e| CoreError::msg(e.to_string()))?;
        let (tx, rx) = bounded::<String>(1);
        lock(&self.rpc_pending).insert(id, tx);
        if !self.send(ClientMsg::Rpc { id, json }) {
            lock(&self.rpc_pending).remove(&id);
            return Err(CoreError::msg("Offline: the service cannot be asked now."));
        }
        let answer = rx.recv_timeout(request.timeout());
        lock(&self.rpc_pending).remove(&id);
        let json = answer.map_err(|_| CoreError::msg(format!("The service did not answer ({}).", request.name())))?;
        match serde_json::from_str::<Response>(&json) {
            Ok(Response::Error(message)) => Err(CoreError::msg(message)),
            Ok(response) => Ok(response),
            Err(error) => Err(CoreError::msg(format!("unreadable answer from the service: {error}"))),
        }
    }

    /// A connection ended (if it is still the current one).
    fn disconnected(&self, generation: u64, state: ConnState, message: String) {
        {
            let mut link = lock(&self.link);
            if link.as_ref().is_some_and(|l| l.generation == generation) {
                *link = None;
            } else if link.is_some() {
                // A newer connection already replaced this one.
                return;
            }
        }
        // Calls waiting on this connection fail now.
        lock(&self.rpc_pending).clear();
        let effects = {
            let mut s = lock(&self.state);
            if s.conn == ConnState::Revoked {
                return;
            }
            s.on_disconnected(state, message)
        };
        self.run(effects);
    }
}

/// Authenticates on a new transport, then serves it until it ends.
fn run_link(shared: Arc<Shared>, generation: u64, inbox: Arc<Inbox>, transport: Arc<dyn Transport>) {
    {
        let mut state = lock(&shared.state);
        state.conn = ConnState::Connecting;
    }
    shared.emit(CoreEvent::Connection { state: ConnState::Connecting, message: String::new() });
    match serve_link(&shared, generation, &inbox, &transport) {
        Ok(()) => shared.disconnected(generation, ConnState::Offline, "the connection closed".into()),
        Err(error) => {
            let revoked = error.downcast_ref::<Refused>().is_some_and(|r| r.verified);
            let message = match error.downcast_ref::<Refused>() {
                Some(refused) if refused.verified => "This phone was revoked on the service: pair it again.".to_owned(),
                Some(_) => "The service refused this phone (unknown or revoked); pair it again if this persists.".to_owned(),
                None => format!("{error:#}"),
            };
            shared.disconnected(generation, if revoked { ConnState::Revoked } else { ConnState::Offline }, message);
            if revoked {
                let mut state = lock(&shared.state);
                state.conn = ConnState::Revoked;
            }
        }
    }
    inbox.close();
    transport.close();
}

fn serve_link(shared: &Arc<Shared>, generation: u64, inbox: &Arc<Inbox>, transport: &Arc<dyn Transport>) -> Result<()> {
    let mut reader = FrameReader::new(Box::new(InboxReader(inbox.clone()))).with_limit(PRE_AUTH_FRAME);
    let mut writer = FrameWriter::new(Box::new(TransportWriter(transport.clone())));
    inbox.set_deadline(Some(HANDSHAKE_TIMEOUT));
    let info = ClientInfo {
        name: shared.config.client_name.clone(),
        caps: CAP_FRAME8 | CAP_COMPRESS | CAP_NOTIFY | CAP_TUNNEL_ACCESS,
        session: shared.session.to_vec(),
        remote_id: Some(shared.config.remote_id.clone()),
    };
    handshake::open(&mut reader, &mut writer, &shared.config.secret, &shared.config.build, Some(info))?;
    reader.set_limit(MAX_FRAME);
    let ack = match reader.recv::<ServiceMsg>()? {
        Some(ServiceMsg::Ready8 { input_ack, .. }) => input_ack,
        _ => bail!("the service refused this phone after the handshake"),
    };
    reader.accept_compressed()?;
    writer.compress()?;
    inbox.set_deadline(None);
    // Keystrokes of an earlier connection the service never applied are reported, not resent.
    let sent = shared.sent.load(Ordering::Acquire);
    shared.acked.store(ack, Ordering::Release);
    let lost = sent.saturating_sub(ack);
    if lost > 0 {
        shared.sent.store(ack.max(sent), Ordering::Release);
    }

    let liveness = Arc::new(Liveness::standard());
    liveness.received();
    let (tx, rx) = unbounded::<ClientMsg>();
    {
        let mut link = lock(&shared.link);
        match link.as_mut() {
            Some(link) if link.generation == generation => link.out = Some(tx),
            _ => bail!("replaced by a newer connection"),
        }
    }
    // The writer: queued messages, a heartbeat every 2 s, and the watchdog.
    let watch = liveness.clone();
    let (writer_inbox, writer_transport) = (inbox.clone(), transport.clone());
    std::thread::Builder::new().name("canvas-writer".into()).spawn(move || {
        let mut beat = Beat::new(watch.interval);
        loop {
            match rx.recv_timeout(Duration::from_millis(500).min(beat.next().saturating_duration_since(Instant::now()))) {
                Ok(message) => {
                    if writer.send(&message).is_err_and(|e| e.kind() != std::io::ErrorKind::InvalidData) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if beat.due(Instant::now()) && writer.send(&ClientMsg::Heartbeat).is_err() {
                break;
            }
            if watch.dead() || writer_inbox.closed() {
                break;
            }
        }
        // Silence (or the end): the connection is over.
        writer_inbox.close();
        writer_transport.close();
    })?;

    let effects = lock(&shared.state).on_connected();
    shared.run(effects);
    if let Some(register) = lock(&shared.push).clone() {
        shared.send(register);
    }
    if lost > 0 {
        shared.emit(CoreEvent::Notice { text: format!("Reconnected. {lost} keystroke(s) were not delivered."), error: true });
    }
    // What the service's machine is (agents, home folder) for new sessions.
    {
        let shared = shared.clone();
        std::thread::spawn(move || {
            if let Ok(Response::Info(info)) = shared.call(Request::Info) {
                lock(&shared.state).info = Some(info);
                shared.emit(CoreEvent::Overview);
            }
        });
    }

    while let Some(message) = reader.recv::<ServiceMsg>().context("the connection broke")? {
        liveness.received();
        match message {
            ServiceMsg::Heartbeat { input_ack } => {
                shared.acked.fetch_max(input_ack, Ordering::AcqRel);
            }
            ServiceMsg::RpcReply { id, json } => {
                if let Some(waiting) = lock(&shared.rpc_pending).remove(&id) {
                    let _ = waiting.send(json);
                }
            }
            ServiceMsg::Ready8 { .. } | ServiceMsg::Welcome { .. } => {}
            other => {
                let effects = lock(&shared.state).on_service(other, now_ms());
                shared.run(effects);
            }
        }
    }
    Ok(())
}

/// Pairs this phone with a service over a fresh WebSocket.
#[derive(uniffi::Object)]
pub struct Pairing {
    inbox: Arc<Inbox>,
    transport: Arc<dyn Transport>,
}

#[uniffi::export]
impl Pairing {
    #[uniffi::constructor]
    pub fn new(transport: Arc<dyn Transport>) -> Arc<Self> {
        Arc::new(Self { inbox: Arc::new(Inbox::default()), transport })
    }

    pub fn receive(&self, data: Vec<u8>) {
        self.inbox.push(&data);
    }

    pub fn transport_closed(&self) {
        self.inbox.close();
    }

    /// Runs the pairing with `code` (blocking; the key derivation takes a
    /// moment by design). `name`: how the service lists this phone.
    pub fn run(&self, code: String, name: String) -> Result<crate::model::PairResult, CoreError> {
        let code = canvas_common::pairing::normalize_code(&code).ok_or_else(|| CoreError::msg("A pairing code has 12 letters and digits (ABCD-EFGH-JKMN)."))?;
        let mut reader = FrameReader::new(Box::new(InboxReader(self.inbox.clone()))).with_limit(PRE_AUTH_FRAME);
        let mut writer = FrameWriter::new(Box::new(TransportWriter(self.transport.clone())));
        self.inbox.set_deadline(Some(Duration::from_secs(30)));
        let result = handshake::pair(&mut reader, &mut writer, &code, &name);
        self.inbox.close();
        self.transport.close();
        let (client_id, secret) = result?;
        Ok(crate::model::PairResult { client_id, secret })
    }
}
