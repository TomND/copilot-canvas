//! Wire protocol between the TUI client and the terminal service.
//!
//! Frames are a little-endian u32 length followed by a postcard-encoded message.
//! Enums are append-only: postcard encodes the variant index.
//!
//! Version negotiation: a protocol-7 client opens with `Hello` and is served
//! protocol-7 shapes only; a protocol-8 client opens with `Hello8` and gets
//! `Frame8`, heartbeats and input acknowledgements.

use std::io::{self, Read, Write};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::Sha256;

pub const PROTOCOL: u32 = 8;
/// Oldest service protocol this client still speaks (with `Hello`).
pub const PROTOCOL_MIN: u32 = 7;
/// Largest frame body.
pub const MAX_FRAME: usize = 32 * 1024 * 1024;
/// Largest frame body read before the peer authenticated.
pub const PRE_AUTH_FRAME: usize = 16 * 1024;
/// Longest build id and client name in a handshake.
pub const MAX_HELLO_FIELD: usize = 256;

/// `ClientInfo::caps`: the client applies `Frame8` (else it gets protocol-7 frames).
pub const CAP_FRAME8: u32 = 1;
/// The client reads zstd-compressed frames on the TCP transport.
pub const CAP_COMPRESS: u32 = 1 << 1;
/// The client renders Kitty graphics: it gets `ServiceMsg::Image` and
/// `ServiceMsg::Placements` for its subscribed terminals.
pub const CAP_IMAGES: u32 = 1 << 2;
/// The client shows desktop notifications: it gets `ServiceMsg::Notification`.
pub const CAP_NOTIFY: u32 = 1 << 3;
/// The client reaches the service through Dev Tunnels web forwarding (the
/// iPhone client) and wants fresh tunnel access tokens: it gets
/// `ServiceMsg::TunnelAccess`.
pub const CAP_TUNNEL_ACCESS: u32 = 1 << 4;

/// `ServiceMsg::Image::format`: raw RGBA rows.
pub const IMAGE_RGBA: u8 = 0;
/// `ServiceMsg::Image::format`: a PNG file.
pub const IMAGE_PNG: u8 = 1;
/// Pixel size of a cell in the host's image geometry (`Placement` offsets and sizes).
pub const HOST_CELL_PX: (u32, u32) = (8, 16);

/// Who a protocol-8 client is (`Hello8`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct ClientInfo {
    /// Front end name, e.g. "tui".
    pub name: String,
    /// `CAP_*` bits.
    pub caps: u32,
    /// 16 random bytes naming this client session across reconnects (input dedupe).
    pub session: Vec<u8>,
    /// Paired remote client id; `None` authenticates with the service token.
    pub remote_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ClientMsg {
    Hello { protocol: u32, build: String, nonce: Vec<u8> },
    Auth { proof: Vec<u8> },
    /// Sessions whose live activity the client wants indexed (its canvas members).
    Track { ids: Vec<String> },
    RefreshSessions,
    Launch { id: String, kind: LaunchKind, cwd: String, cols: u16, rows: u16 },
    /// Full replacement of the visible terminal set.
    Subscribe { ids: Vec<String> },
    Input { id: String, input: Input },
    Resize { id: String, cols: u16, rows: u16 },
    Scroll { id: String, delta: i32 },
    Stop { id: String, force: bool },
    Forget { id: String },
    Shutdown,
    Stats,
    /// Protocol 3: mouse wheel over a terminal at a cell of its pane (0-based).
    /// The service forwards it to the app (mouse report, or PageUp/PageDown on
    /// the alternate screen) or scrolls the scrollback.
    Wheel { id: String, up: bool, col: u16, row: u16, mods: u8 },
    /// A click (press + release) at a cell; reaches the app only when it asked
    /// for mouse reports. `button`: 0 left, 1 middle, 2 right.
    Click { id: String, col: u16, row: u16, button: u8, mods: u8 },
    /// Groups, members and passive sessions (`bus::Routing` as JSON).
    Routing { json: String },
    /// Event bus request (run now, resume, emit, discard mail) as JSON.
    Bus { json: String },
    /// Protocol 8: opens the handshake (instead of `Hello`).
    Hello8 { protocol: u32, build: String, nonce: Vec<u8>, client: ClientInfo },
    /// Protocol 8: sent every 2 s; 6 s of silence means the connection is dead.
    Heartbeat,
    /// Protocol 8: terminal input numbered per client session; the service
    /// applies each `seq` at most once and acknowledges the highest applied.
    Input8 { id: String, input: Input, seq: u64 },
    /// Protocol 8: `Subscribe` plus the focused tile (frames at a higher rate remotely).
    Subscribe8 { ids: Vec<String>, focused: Option<String> },
    /// Protocol 8: opens a canvas document owned by the service: `path` (on
    /// the service's machine; `None`: its default canvas), created from the
    /// demo layout when `demo`. Answered with `ServiceMsg::Doc`; the client
    /// then gets every `DocPatch` of it.
    DocOpen { path: Option<String>, demo: bool, cwd: String },
    /// Protocol 8: changes to an open document (`canvas_model::sync::Op`
    /// list as JSON) made on top of version `base`.
    DocEdit { key: String, base: u64, ops_json: String },
    /// Protocol 8: a backend request (`canvas_model::rpc::Request` as JSON),
    /// answered with `ServiceMsg::RpcReply` carrying the same `id`.
    Rpc { id: u64, json: String },
    /// Pairing of older clients (a proof of the code before the service
    /// proved anything); refused: clients open with `PairStart` now.
    Pair { code_proof: Vec<u8>, name: String, nonce: Vec<u8> },
    /// Protocol 8, TCP only: opens a connection that pairs a new remote client
    /// (instead of `Hello8`); `name` names the client in `remote clients`.
    /// Answered with `ServiceMsg::PairChallenge` (see [`crate::pairing`]).
    PairStart { name: String, nonce: Vec<u8> },
    /// Protocol 8: the client's proof of the pairing code, sent once the
    /// service proved it knows the code. Answered with `ServiceMsg::Paired`
    /// (or `Error`), then the service closes the connection.
    PairProof { proof: Vec<u8> },
    /// Protocol 8: this client takes over the size of terminal `id` (tmux
    /// "latest"): the terminal takes the size this client asked for with
    /// `Resize` until another client types in it or claims it. Typing claims
    /// a terminal too; a client that never sent `Resize` for it (the iPhone's
    /// mirror mode) leaves the size alone.
    Claim { id: String },
    /// Protocol 8, paired clients: this phone's APNs device token (empty:
    /// unregister), `environment` "sandbox" or "production", the app's
    /// bundle id and what the phone calls this machine. The service pushes
    /// alerts (a session needs you or finished) and renewed tunnel access
    /// tokens to it while it is not connected.
    PushRegister { token: String, environment: String, bundle: String, label: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum LaunchKind {
    /// Start or resume a Copilot CLI session with this session UUID.
    Copilot { new_name: Option<String>, prompt: Option<String>, resume: bool },
    /// Arbitrary program (demo shells, benchmarks).
    Command { program: String, args: Vec<String> },
    /// Protocol 2: Copilot with extra CLI arguments and environment (appended
    /// last so protocol-1 messages keep their encoding).
    CopilotWith { new_name: Option<String>, prompt: Option<String>, resume: bool, args: Vec<String>, env: Vec<(String, String)> },
    /// Protocol 8: any agent CLI; the backend builds the command line.
    Agent(Box<AgentLaunch>),
}

/// What to launch, independent of the agent CLI's flags (protocol 8). The
/// backend turns it into a command line with the agent's provider.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct AgentLaunch {
    /// "copilot", "opencode" or "claude".
    pub agent: String,
    /// The agent's own session id when it differs from the terminal id.
    pub native_id: Option<String>,
    /// Continue an existing conversation (false: start a new one).
    pub resume: bool,
    /// Start as a fork of this session (agents that fork natively).
    pub fork_from: Option<String>,
    pub name: Option<String>,
    /// First prompt, typed into the new session.
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub custom_agent: Option<String>,
    pub context: Option<String>,
    /// Run tools without asking (permissions = allow-all).
    pub allow_all: bool,
    pub add_dirs: Vec<String>,
    /// MCP servers to turn off for this session.
    pub mcp_off: Vec<String>,
    /// The canvas group's folder (ai-docs, human-outputs, instructions).
    pub group_folder: Option<String>,
    pub group_name: Option<String>,
    /// Passive run: one non-interactive turn with this prompt.
    pub headless_prompt: Option<String>,
    /// Extra environment from the canvas.
    pub env: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Input {
    Key { key: Key, mods: u8 },
    Paste(String),
    Focus(bool),
    Raw(Vec<u8>),
}

pub const MOD_SHIFT: u8 = 1;
pub const MOD_ALT: u8 = 2;
pub const MOD_CTRL: u8 = 4;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ServiceMsg {
    Challenge { protocol: u32, build: String, nonce: Vec<u8>, proof: Vec<u8> },
    Ready { pid: u32 },
    Terminals(Vec<TermInfo>),
    TerminalGone(String),
    Frame(Frame),
    Sessions { full: bool, upsert: Vec<SessionSummary>, remove: Vec<String> },
    Clipboard(String),
    Error { id: Option<String>, message: String },
    Stats(ServiceStats),
    /// A Copilot session started from inside another session's terminal
    /// (e.g. `copilot -p …` run by an agent): link it as its subsession.
    Subsession { parent: String, child: String, name: String, cwd: String },
    /// Passive sessions, mailbox and recent events (`bus::Snapshot` as JSON).
    Bus { json: String },
    /// One event with the sessions it was delivered to (for the canvas pulse).
    BusEvent { json: String },
    /// Protocol 8: a frame of protocol-8 rows (instead of `Frame`).
    Frame8(Frame8),
    /// Protocol 8: sent every 2 s; `input_ack` is the highest applied `Input8::seq`.
    Heartbeat { input_ack: u64 },
    /// Protocol 8: authentication succeeded (instead of `Ready`); `input_ack` is
    /// the last input of this client session applied so far (0 if unknown).
    Ready8 { pid: u32, input_ack: u64 },
    /// Protocol 8: the agent session a terminal runs became known (OpenCode
    /// creates its session on the first prompt): terminal id -> the agent's id.
    AgentSession { id: String, native: String },
    /// Protocol 8: a whole canvas document (`canvas_model::sync::DocReply` as
    /// JSON) at `version`: the answer to `DocOpen`, or sent again when the
    /// client must resynchronize.
    Doc { key: String, version: u64, json: String },
    /// Protocol 8: an edit the service applied to an open document, sent to
    /// every client that opened it; `origin` is the editing connection's id.
    DocPatch { key: String, version: u64, origin: u64, ops_json: String },
    /// Protocol 8: the document was saved up to `version`, or `error` says why not.
    DocSaved { key: String, version: u64, error: Option<String> },
    /// Protocol 8: the answer to `ClientMsg::Rpc` (`canvas_model::rpc::Response` as JSON).
    RpcReply { id: u64, json: String },
    /// Protocol 8: the answer to `ClientMsg::Pair`: the new client's id, the
    /// service's nonce and the client secret sealed under a key derived from
    /// the code and both nonces (`pairing::seal`).
    Paired { client_id: String, nonce: Vec<u8>, secret: Vec<u8> },
    /// Protocol 8, `CAP_IMAGES`: the pixels of one image of a terminal
    /// (`format`: `IMAGE_PNG` or `IMAGE_RGBA`). Sent once per client and image
    /// id before a placement uses it, and again when its pixels change or it
    /// is placed again after no placement used it.
    Image { term: String, image_id: u32, width: u32, height: u32, rgba_or_png: Vec<u8>, format: u8 },
    /// Protocol 8, `CAP_IMAGES`: every image placement in a terminal's
    /// viewport, replacing the previous list. The client may forget images
    /// that no placement of the terminal refers to.
    Placements { term: String, placements: Vec<Placement> },
    /// Protocol 8, `CAP_NOTIFY`: a terminal's program sent a desktop
    /// notification (OSC 777 or OSC 9).
    Notification { term: String, title: String, body: String },
    /// Protocol 8: the answer to `PairStart`: the service's nonce and a proof
    /// of each open code, padded with random ones to `pairing::MAX_OPEN_CODES`.
    PairChallenge { nonce: Vec<u8>, proofs: Vec<Vec<u8>> },
    /// Protocol 8: the service refuses this client before authentication.
    /// `proof` is an HMAC with the client's revocation key when it is a
    /// revoked paired client (only that client can check it), else random.
    Refusal { proof: Vec<u8> },
    /// Protocol 8, TCP: the service's build, told once the client authenticated.
    Welcome { build: String },
    /// Protocol 8, `CAP_TUNNEL_ACCESS`: a Dev Tunnels `connect` access token
    /// for the tunnel in front of this service (sent on connect and whenever
    /// it is renewed), its expiry, and the tunnel port's web-forwarding
    /// address (`wss://…devtunnels.ms`; empty when unknown).
    TunnelAccess { url: String, token: String, expires_ms: u64 },
}

/// Where an image is drawn in a terminal's viewport (`ServiceMsg::Placements`).
/// Offsets and sizes are pixels of the host's cell geometry ([`HOST_CELL_PX`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Placement {
    pub image_id: u32,
    /// Top-left cell; negative when the image starts left of or above the viewport.
    pub col: i32,
    pub row: i32,
    /// Cells covered.
    pub cols: u32,
    pub rows: u32,
    /// Offset into the top-left cell and the drawn size (host pixels).
    pub x_off: u32,
    pub y_off: u32,
    pub width: u32,
    pub height: u32,
    /// The part of the image to draw (image pixels).
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    /// Kitty's z-index: negative below the text, else above; lower first.
    pub z: i32,
}

/// Index of the first `ServiceMsg` variant protocol-7 clients cannot decode.
pub const SERVICE_V7_VARIANTS: u8 = 12;

impl ServiceMsg {
    /// The message in the shape a client of `protocol` reads (`frame8`: it set
    /// `CAP_FRAME8`). `None`: nothing to send (a protocol-8 heartbeat to a
    /// protocol-7 client).
    pub fn shaped(self, protocol: u32, frame8: bool) -> Option<ServiceMsg> {
        match self {
            ServiceMsg::Frame8(frame) if protocol < 8 || !frame8 => Some(ServiceMsg::Frame(frame.to_v7())),
            ServiceMsg::Heartbeat { .. } if protocol < 8 => None,
            ServiceMsg::Ready8 { pid, .. } if protocol < 8 => Some(ServiceMsg::Ready { pid }),
            ServiceMsg::AgentSession { .. }
            | ServiceMsg::Doc { .. }
            | ServiceMsg::DocPatch { .. }
            | ServiceMsg::DocSaved { .. }
            | ServiceMsg::RpcReply { .. }
            | ServiceMsg::Paired { .. }
            | ServiceMsg::Image { .. }
            | ServiceMsg::Placements { .. }
            | ServiceMsg::Notification { .. }
            | ServiceMsg::PairChallenge { .. }
            | ServiceMsg::Refusal { .. }
            | ServiceMsg::Welcome { .. }
            | ServiceMsg::TunnelAccess { .. }
                if protocol < 8 =>
            {
                None
            }
            other => Some(other),
        }
    }

    /// [`ServiceMsg::shaped`] for a client with `caps` (`CAP_*`): images go
    /// only to clients with `CAP_IMAGES`, notifications only to clients with
    /// `CAP_NOTIFY` (a protocol-8 client of an older build cannot decode them).
    pub fn shaped_for(self, protocol: u32, caps: u32) -> Option<ServiceMsg> {
        match &self {
            ServiceMsg::Image { .. } | ServiceMsg::Placements { .. } if caps & CAP_IMAGES == 0 => None,
            ServiceMsg::Notification { .. } if caps & CAP_NOTIFY == 0 => None,
            ServiceMsg::TunnelAccess { .. } if caps & CAP_TUNNEL_ACCESS == 0 => None,
            _ => self.shaped(protocol, caps & CAP_FRAME8 != 0),
        }
    }
}

/// Index of `ServiceMsg::Image` (postcard encodes it before the fields).
const IMAGE_VARIANT: u32 = 21;

/// A PNG `ServiceMsg::Image` serialized from borrowed bytes (the same bytes on
/// the wire), so one encoded image can be queued for many clients without a
/// copy per client.
pub struct ImageRef<'a> {
    pub term: &'a str,
    pub image_id: u32,
    pub width: u32,
    pub height: u32,
    pub png: &'a [u8],
}

/// Bytes serialized in one piece (postcard writes them as a sequence of `u8` would be).
struct Bytes<'a>(&'a [u8]);

impl Serialize for Bytes<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.0)
    }
}

impl Serialize for ImageRef<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStructVariant;
        let mut variant = serializer.serialize_struct_variant("ServiceMsg", IMAGE_VARIANT, "Image", 6)?;
        variant.serialize_field("term", self.term)?;
        variant.serialize_field("image_id", &self.image_id)?;
        variant.serialize_field("width", &self.width)?;
        variant.serialize_field("height", &self.height)?;
        variant.serialize_field("rgba_or_png", &Bytes(self.png))?;
        variant.serialize_field("format", &IMAGE_PNG)?;
        variant.end()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct ServiceStats {
    pub pid: u32,
    pub terminals: u32,
    pub alive: u32,
    pub working_set_bytes: u64,
    pub private_bytes: u64,
    pub cpu_ms: u64,
    pub uptime_ms: u64,
    pub bytes_parsed: u64,
    pub frames_sent: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct TermInfo {
    pub id: String,
    pub alive: bool,
    pub exit_code: Option<u32>,
    pub title: String,
    pub pid: u32,
    pub started_ms: u64,
    pub last_output_ms: u64,
    pub copilot: bool,
    pub error: Option<String>,
    /// Last OSC 9;4 progress state: 0 none, 1 value, 2 error, 3 busy, 4 paused.
    pub progress: u8,
    /// Bells and OSC 777 notifications so far.
    pub bells: u32,
    /// The program reports progress (Copilot: busy at turn start, cleared at turn end).
    pub progress_seen: bool,
    /// When the progress state last changed (service clock, ms).
    pub progress_ms: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Run {
    pub col: u16,
    pub text: String,
    pub fg: u32,
    pub bg: u32,
    pub flags: u16,
    /// Protocol 3: OSC 8 hyperlink target of these cells.
    pub link: Option<String>,
}

pub const COLOR_DEFAULT: u32 = 0;
pub const COLOR_RGB: u32 = 1 << 24;

pub const F_BOLD: u16 = 1;
pub const F_ITALIC: u16 = 2;
pub const F_UNDERLINE: u16 = 4;
pub const F_INVERSE: u16 = 8;
pub const F_DIM: u16 = 16;
pub const F_HIDDEN: u16 = 32;
pub const F_STRIKE: u16 = 64;

/// Protocol 8 cell flags (`Run8::flags`): the protocol-7 bits plus these.
pub const F8_OVERLINE: u32 = 1 << 7;
pub const F8_BLINK: u32 = 1 << 8;
/// Underline style (bits 9..11); `F_UNDERLINE` is also set for any style.
pub const F8_UL_SHIFT: u32 = 9;
pub const F8_UL_MASK: u32 = 7 << F8_UL_SHIFT;
pub const UL_SINGLE: u32 = 1;
pub const UL_DOUBLE: u32 = 2;
pub const UL_CURLY: u32 = 3;
pub const UL_DOTTED: u32 = 4;
pub const UL_DASHED: u32 = 5;

/// Terminal modes (`Frame8::modes`, `Snapshot::modes`).
pub const MODE_MOUSE: u32 = 1;
pub const MODE_MOUSE_DRAG: u32 = 1 << 1;
pub const MODE_MOUSE_MOTION: u32 = 1 << 2;
pub const MODE_SGR_MOUSE: u32 = 1 << 3;
pub const MODE_UTF8_MOUSE: u32 = 1 << 4;
pub const MODE_BRACKETED_PASTE: u32 = 1 << 5;
pub const MODE_ALT_SCREEN: u32 = 1 << 6;
pub const MODE_APP_CURSOR: u32 = 1 << 7;
pub const MODE_FOCUS: u32 = 1 << 8;
pub const MODE_SHOW_CURSOR: u32 = 1 << 9;
pub const MODE_APP_KEYPAD: u32 = 1 << 10;
/// Kitty keyboard protocol flags (5 bits) start here; bit 0 = disambiguate.
pub const MODE_KITTY_SHIFT: u32 = 16;
pub const MODE_KITTY_DISAMBIGUATE: u32 = 1 << MODE_KITTY_SHIFT;

/// Cursor shapes (`Frame8::cursor_shape`).
pub const CURSOR_BLOCK: u8 = 0;
pub const CURSOR_UNDERLINE: u8 = 1;
pub const CURSOR_BAR: u8 = 2;
pub const CURSOR_BLOCK_HOLLOW: u8 = 3;

/// Protocol 8: a run of cells with one style. Each cell is one grapheme
/// cluster; widths come from the engine, never from the client's tables.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Run8 {
    pub col: u16,
    /// The cells' code points in order.
    pub text: String,
    /// Cells of more than one code point: (cell index in this run, extra code points).
    pub clusters: Vec<(u16, u8)>,
    /// Every cell of this run is two columns wide (the next column is its spacer).
    pub wide: bool,
    pub fg: u32,
    pub bg: u32,
    pub flags: u32,
    /// Underline colour (`COLOR_DEFAULT`: the foreground).
    pub underline: u32,
    /// Hyperlink (OSC 8): index + 1 into the frame's `links`; 0 = none.
    pub link: u32,
}

impl Run8 {
    /// The run's cells (grapheme clusters) in order.
    pub fn cells(&self) -> Vec<&str> {
        let mut out = Vec::new();
        let mut extra = self.clusters.iter().copied().peekable();
        let mut rest = self.text.as_str();
        let mut index = 0u16;
        while let Some(first) = rest.chars().next() {
            let mut len = first.len_utf8();
            if let Some(&(cell, more)) = extra.peek()
                && cell == index
            {
                extra.next();
                for c in rest[len..].chars().take(more as usize) {
                    len += c.len_utf8();
                }
            }
            out.push(&rest[..len]);
            rest = &rest[len..];
            index += 1;
        }
        out
    }

    /// Columns the run covers.
    pub fn width(&self) -> u16 {
        let cells = self.text.chars().count() - self.clusters.iter().map(|(_, n)| *n as usize).sum::<usize>();
        cells as u16 * if self.wide { 2 } else { 1 }
    }
}

/// Protocol 8 frame: rows of `Run8`, cursor shape and the terminal's modes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Frame8 {
    pub id: String,
    pub cols: u16,
    pub rows: u16,
    pub full: bool,
    pub changed: Vec<(u16, Vec<Run8>)>,
    pub cursor: Option<(u16, u16)>,
    pub cursor_shape: u8,
    pub cursor_blink: bool,
    pub display_offset: u32,
    pub history: u32,
    pub modes: u32,
    /// Hyperlink targets referenced by `Run8::link` in this frame's rows.
    pub links: Vec<String>,
}

impl Frame8 {
    /// The same frame for a protocol-7 client.
    pub fn to_v7(&self) -> Frame {
        Frame {
            id: self.id.clone(),
            cols: self.cols,
            rows: self.rows,
            full: self.full,
            changed: self.changed.iter().map(|(y, row)| (*y, runs_v7(row, &self.links))).collect(),
            cursor: self.cursor,
            display_offset: self.display_offset,
            history: self.history,
        }
    }
}

/// Protocol-7 runs of a protocol-8 row (adjacent runs that differ only in
/// what protocol 7 cannot express are merged, as protocol-7 hosts did).
pub fn runs_v7(row: &[Run8], links: &[String]) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::with_capacity(row.len());
    let mut end = 0u16;
    for run in row {
        let flags = (run.flags & 0x7f) as u16;
        let link = (run.link > 0).then(|| links.get(run.link as usize - 1).cloned()).flatten();
        match out.last_mut() {
            Some(last) if end == run.col && last.fg == run.fg && last.bg == run.bg && last.flags == flags && last.link == link => {
                last.text.push_str(&run.text)
            }
            _ => out.push(Run { col: run.col, text: run.text.clone(), fg: run.fg, bg: run.bg, flags, link }),
        }
        end = run.col + run.width();
    }
    out
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Frame {
    pub id: String,
    pub cols: u16,
    pub rows: u16,
    pub full: bool,
    pub changed: Vec<(u16, Vec<Run>)>,
    pub cursor: Option<(u16, u16)>,
    pub display_offset: u32,
    pub history: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Activity {
    #[default]
    Unknown,
    Working,
    Idle,
    Failed,
    Stopped,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Working,
    Finished,
    Failed,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentSummary {
    pub key: String,
    pub name: String,
    pub description: String,
    pub status: AgentStatus,
    pub finished_ms: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionSummary {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub branch: String,
    pub repository: String,
    pub git_root: String,
    pub client: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub tracked: bool,
    pub live: bool,
    pub activity: Activity,
    pub pending: u16,
    pub finished_ms: Option<u64>,
    pub last_event_ms: u64,
    pub truncated: bool,
    pub agents: Vec<AgentSummary>,
    /// Shortest active recurring schedule (`/every`, `manage_schedule`) in ms; 0 = none.
    pub loop_ms: u64,
}

pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, message: &T) -> io::Result<()> {
    let body = postcard::to_stdvec(message).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut packet = Vec::with_capacity(body.len() + 4);
    packet.extend_from_slice(&(body.len() as u32).to_le_bytes());
    packet.extend_from_slice(&body);
    writer.write_all(&packet)
}

/// Returns `Ok(None)` on a clean end of stream.
pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> io::Result<Option<T>> {
    match read_frame_lenient(reader)? {
        None => Ok(None),
        Some(Ok(message)) => Ok(Some(message)),
        Some(Err(_)) => Err(io::Error::new(io::ErrorKind::InvalidData, "undecodable frame")),
    }
}

/// Like `read_frame`, but a complete frame that does not decode (a service of
/// another protocol version) is returned as `Some(Err(..))`: the stream stays
/// in sync, so the caller can skip it.
pub fn read_frame_lenient<R: Read, T: DeserializeOwned>(reader: &mut R) -> io::Result<Option<Result<T, Vec<u8>>>> {
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if matches!(error.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe) => return Ok(None),
        Err(error) => return Err(error),
    }
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let body = crate::wire::read_body(reader, len)?;
    Ok(Some(postcard::from_bytes(&body).map_err(|_| body)))
}

/// Protocol-3 `TermInfo` (before progress/bell fields), to read an older
/// service's terminal list while upgrading it.
#[derive(Deserialize)]
struct TermInfoV3 {
    id: String,
    alive: bool,
    exit_code: Option<u32>,
    title: String,
    pid: u32,
    started_ms: u64,
    last_output_ms: u64,
    copilot: bool,
    error: Option<String>,
}

/// The first variants of protocol-3 `ServiceMsg` (postcard encodes the index).
#[derive(Deserialize)]
#[allow(dead_code)]
enum ServiceMsgV3 {
    Challenge { protocol: u32, build: String, nonce: Vec<u8>, proof: Vec<u8> },
    Ready { pid: u32 },
    Terminals(Vec<TermInfoV3>),
}

/// The terminal list in a frame from a protocol-3 service, if it is one.
pub fn decode_v3_terminals(body: &[u8]) -> Option<Vec<TermInfo>> {
    match postcard::from_bytes::<ServiceMsgV3>(body).ok()? {
        ServiceMsgV3::Terminals(list) => Some(
            list.into_iter()
                .map(|t| TermInfo {
                    id: t.id,
                    alive: t.alive,
                    exit_code: t.exit_code,
                    title: t.title,
                    pid: t.pid,
                    started_ms: t.started_ms,
                    last_output_ms: t.last_output_ms,
                    copilot: t.copilot,
                    error: t.error,
                    ..Default::default()
                })
                .collect(),
        ),
        _ => None,
    }
}

type HmacSha256 = Hmac<Sha256>;

pub fn nonce() -> Vec<u8> {
    let mut bytes = vec![0u8; 32];
    getrandom::fill(&mut bytes).expect("operating system random generator failed");
    bytes
}

pub fn prove(token: &[u8], role: &str, nonce: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(token).expect("HMAC accepts any key length");
    mac.update(role.as_bytes());
    mac.update(nonce);
    mac.finalize().into_bytes().to_vec()
}

pub fn verify(token: &[u8], role: &str, nonce: &[u8], proof: &[u8]) -> bool {
    let mut mac = HmacSha256::new_from_slice(token).expect("HMAC accepts any key length");
    mac.update(role.as_bytes());
    mac.update(nonce);
    mac.verify_slice(proof).is_ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_detect_eof() {
        let message = ServiceMsg::Frame(Frame {
            id: "a".into(),
            cols: 80,
            rows: 24,
            full: true,
            changed: vec![(3, vec![Run { col: 2, text: "héllo 中".into(), fg: 5, bg: COLOR_RGB | 0x112233, flags: F_BOLD, link: Some("https://example.com/a".into()) }])],
            cursor: Some((1, 2)),
            display_offset: 0,
            history: 10,
        });
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &message).unwrap();
        write_frame(&mut buffer, &ClientMsg::Shutdown).unwrap();
        let mut reader = buffer.as_slice();
        assert_eq!(read_frame::<_, ServiceMsg>(&mut reader).unwrap(), Some(message));
        assert_eq!(read_frame::<_, ClientMsg>(&mut reader).unwrap(), Some(ClientMsg::Shutdown));
        assert_eq!(read_frame::<_, ClientMsg>(&mut reader).unwrap(), None);
    }

    #[test]
    fn oversized_frames_are_rejected() {
        let mut bytes = ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0; 16]);
        assert!(read_frame::<_, ClientMsg>(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn proofs_bind_token_role_and_nonce() {
        let token = b"secret-token";
        let n = nonce();
        let proof = prove(token, "server", &n);
        assert!(verify(token, "server", &n, &proof));
        assert!(!verify(token, "client", &n, &proof));
        assert!(!verify(b"other", "server", &n, &proof));
        assert!(!verify(token, "server", &nonce(), &proof));
    }

    pub(crate) fn sample_frame8() -> Frame8 {
        Frame8 {
            id: "t".into(),
            cols: 20,
            rows: 2,
            full: true,
            changed: vec![
                (0, vec![Run8 { col: 0, text: "e\u{301}x".into(), clusters: vec![(0, 1)], fg: 3, flags: F_BOLD as u32 | F8_OVERLINE, link: 1, ..Default::default() }]),
                (1, vec![Run8 { col: 2, text: "中".into(), wide: true, underline: COLOR_RGB | 0xff0000, flags: F_UNDERLINE as u32 | (UL_CURLY << F8_UL_SHIFT), ..Default::default() }]),
            ],
            cursor: Some((1, 1)),
            cursor_shape: CURSOR_BAR,
            cursor_blink: true,
            display_offset: 0,
            history: 7,
            modes: MODE_BRACKETED_PASTE | MODE_KITTY_DISAMBIGUATE,
            links: vec!["https://example.com/x".into()],
        }
    }

    #[test]
    fn protocol8_variants_round_trip_at_appended_indices() {
        let info = ClientInfo { name: "tui".into(), caps: CAP_FRAME8 | CAP_COMPRESS, session: vec![7; 16], remote_id: Some("home-pc".into()) };
        let client = [
            (17, ClientMsg::Hello8 { protocol: PROTOCOL, build: "b".into(), nonce: vec![1; 32], client: info }),
            (18, ClientMsg::Heartbeat),
            (19, ClientMsg::Input8 { id: "t".into(), input: Input::Key { key: Key::Char('x'), mods: MOD_CTRL }, seq: 1 << 40 }),
            (20, ClientMsg::Subscribe8 { ids: vec!["a".into(), "b".into()], focused: Some("b".into()) }),
            (21, ClientMsg::DocOpen { path: Some(r"C:\c.json".into()), demo: false, cwd: r"C:\w".into() }),
            (22, ClientMsg::DocEdit { key: "canvas-1".into(), base: 7, ops_json: "[]".into() }),
            (23, ClientMsg::Rpc { id: 9, json: "\"Info\"".into() }),
            (24, ClientMsg::Pair { code_proof: vec![3; 32], name: "home-pc".into(), nonce: vec![4; 32] }),
            (25, ClientMsg::PairStart { name: "home-pc".into(), nonce: vec![4; 32] }),
            (26, ClientMsg::PairProof { proof: vec![5; 32] }),
            (27, ClientMsg::Claim { id: "t".into() }),
            (28, ClientMsg::PushRegister { token: "ab".repeat(32), environment: "sandbox".into(), bundle: "com.x".into(), label: "work".into() }),
        ];
        for (index, message) in client {
            let bytes = postcard::to_stdvec(&message).unwrap();
            assert_eq!(bytes[0], index, "{message:?}");
            assert_eq!(postcard::from_bytes::<ClientMsg>(&bytes).unwrap(), message);
        }
        let service = [
            (SERVICE_V7_VARIANTS, ServiceMsg::Frame8(sample_frame8())),
            (13, ServiceMsg::Heartbeat { input_ack: 42 }),
            (14, ServiceMsg::Ready8 { pid: 9, input_ack: 3 }),
            (15, ServiceMsg::AgentSession { id: "t".into(), native: "ses_x".into() }),
            (16, ServiceMsg::Doc { key: "canvas-1".into(), version: 1, json: "{}".into() }),
            (17, ServiceMsg::DocPatch { key: "canvas-1".into(), version: 2, origin: 3, ops_json: "[]".into() }),
            (18, ServiceMsg::DocSaved { key: "canvas-1".into(), version: 2, error: Some("disk full".into()) }),
            (19, ServiceMsg::RpcReply { id: 9, json: "\"Done\"".into() }),
            (20, ServiceMsg::Paired { client_id: "pc-1".into(), nonce: vec![5; 32], secret: vec![6; 64] }),
            (21, sample_image()),
            (22, sample_placements()),
            (23, ServiceMsg::Notification { term: "t".into(), title: "Build".into(), body: "done".into() }),
            (24, ServiceMsg::PairChallenge { nonce: vec![1; 32], proofs: vec![vec![2; 32]; 3] }),
            (25, ServiceMsg::Refusal { proof: vec![3; 32] }),
            (26, ServiceMsg::Welcome { build: "0.3.0-abc".into() }),
            (27, ServiceMsg::TunnelAccess { url: "wss://copilot-canvas-47600.usw2.devtunnels.ms".into(), token: "eyJ.x.y".into(), expires_ms: 9 }),
        ];
        for (index, message) in service {
            let bytes = postcard::to_stdvec(&message).unwrap();
            assert_eq!(bytes[0], index, "{message:?}");
            assert_eq!(postcard::from_bytes::<ServiceMsg>(&bytes).unwrap(), message);
        }
        // Protocol-7 indices are unchanged.
        assert_eq!(postcard::to_stdvec(&ServiceMsg::BusEvent { json: String::new() }).unwrap()[0], SERVICE_V7_VARIANTS - 1);
        assert_eq!(postcard::to_stdvec(&ClientMsg::Bus { json: String::new() }).unwrap()[0], 16);
    }

    #[test]
    fn protocol7_clients_only_get_protocol7_shapes() {
        let all = [
            ServiceMsg::Ready { pid: 1 },
            ServiceMsg::Frame8(sample_frame8()),
            ServiceMsg::Heartbeat { input_ack: 1 },
            ServiceMsg::Ready8 { pid: 1, input_ack: 1 },
            ServiceMsg::Clipboard("x".into()),
            ServiceMsg::AgentSession { id: "t".into(), native: "n".into() },
            ServiceMsg::Doc { key: "k".into(), version: 1, json: "{}".into() },
            ServiceMsg::DocPatch { key: "k".into(), version: 2, origin: 1, ops_json: "[]".into() },
            ServiceMsg::DocSaved { key: "k".into(), version: 2, error: None },
            ServiceMsg::RpcReply { id: 1, json: "\"Done\"".into() },
            ServiceMsg::Paired { client_id: "pc-1".into(), nonce: vec![1; 32], secret: vec![2; 64] },
            sample_image(),
            sample_placements(),
            ServiceMsg::Notification { term: "t".into(), title: "T".into(), body: "B".into() },
            ServiceMsg::PairChallenge { nonce: vec![1; 32], proofs: Vec::new() },
            ServiceMsg::Refusal { proof: vec![1; 32] },
            ServiceMsg::Welcome { build: "b".into() },
            ServiceMsg::TunnelAccess { url: String::new(), token: "t".into(), expires_ms: 1 },
        ];
        for message in all.clone() {
            let Some(shaped) = message.shaped(7, true) else { continue };
            let bytes = postcard::to_stdvec(&shaped).unwrap();
            assert!(bytes[0] < SERVICE_V7_VARIANTS, "a protocol-7 client got {shaped:?}");
        }
        for message in &all[5..] {
            assert_eq!(message.clone().shaped(7, true), None, "{message:?} is protocol 8 only");
            assert_eq!(message.clone().shaped(8, true).as_ref(), Some(message));
        }
        assert_eq!(ServiceMsg::Heartbeat { input_ack: 1 }.shaped(7, true), None);
        let ServiceMsg::Frame(frame) = ServiceMsg::Frame8(sample_frame8()).shaped(7, true).unwrap() else { panic!("frame") };
        assert_eq!(frame.changed[0].1[0].link.as_deref(), Some("https://example.com/x"));
        assert_eq!(frame.changed[1].1[0].text, "中");
        // Protocol 8 without CAP_FRAME8 also reads protocol-7 frames.
        assert!(matches!(ServiceMsg::Frame8(sample_frame8()).shaped(8, false), Some(ServiceMsg::Frame(_))));
        assert!(matches!(ServiceMsg::Frame8(sample_frame8()).shaped(8, true), Some(ServiceMsg::Frame8(_))));
        assert_eq!(ServiceMsg::Heartbeat { input_ack: 1 }.shaped(8, true), Some(ServiceMsg::Heartbeat { input_ack: 1 }));
    }

    fn sample_image() -> ServiceMsg {
        ServiceMsg::Image { term: "t".into(), image_id: 3, width: 2, height: 1, rgba_or_png: vec![1, 2, 3, 255, 4, 5, 6, 255], format: IMAGE_RGBA }
    }

    fn sample_placements() -> ServiceMsg {
        let placement = Placement { image_id: 3, col: 1, row: -2, cols: 8, rows: 4, width: 64, height: 64, src_w: 2, src_h: 1, z: -1, ..Default::default() };
        ServiceMsg::Placements { term: "t".into(), placements: vec![placement] }
    }

    #[test]
    fn images_and_notifications_only_reach_clients_that_asked_for_them() {
        let v8 = CAP_FRAME8 | CAP_COMPRESS;
        for message in [sample_image(), sample_placements()] {
            assert_eq!(message.clone().shaped_for(8, v8), None, "no CAP_IMAGES");
            assert_eq!(message.clone().shaped_for(8, v8 | CAP_NOTIFY), None);
            assert_eq!(message.clone().shaped_for(7, v8 | CAP_IMAGES), None, "protocol 7");
            assert_eq!(message.clone().shaped_for(8, v8 | CAP_IMAGES).as_ref(), Some(&message));
        }
        let note = ServiceMsg::Notification { term: "t".into(), title: "T".into(), body: "B".into() };
        assert_eq!(note.clone().shaped_for(8, v8 | CAP_IMAGES), None);
        assert_eq!(note.clone().shaped_for(8, v8 | CAP_NOTIFY).as_ref(), Some(&note));
        let access = ServiceMsg::TunnelAccess { url: String::new(), token: "t".into(), expires_ms: 1 };
        assert_eq!(access.clone().shaped_for(8, v8 | CAP_NOTIFY | CAP_IMAGES), None, "only for clients that asked");
        assert_eq!(access.clone().shaped_for(7, CAP_TUNNEL_ACCESS), None);
        assert_eq!(access.clone().shaped_for(8, v8 | CAP_TUNNEL_ACCESS).as_ref(), Some(&access));
        // Everything else is shaped as before.
        assert!(matches!(ServiceMsg::Frame8(sample_frame8()).shaped_for(8, 0), Some(ServiceMsg::Frame(_))));
        assert!(matches!(ServiceMsg::Frame8(sample_frame8()).shaped_for(8, CAP_FRAME8), Some(ServiceMsg::Frame8(_))));
    }

    #[test]
    fn borrowed_images_serialize_like_image_messages() {
        let png: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        let owned = ServiceMsg::Image { term: "term-1".into(), image_id: 9, width: 300, height: 2, rgba_or_png: png.clone(), format: IMAGE_PNG };
        let borrowed = ImageRef { term: "term-1", image_id: 9, width: 300, height: 2, png: &png };
        let bytes = postcard::to_stdvec(&borrowed).unwrap();
        assert_eq!(bytes, postcard::to_stdvec(&owned).unwrap());
        assert_eq!(postcard::from_bytes::<ServiceMsg>(&bytes).unwrap(), owned);
    }

    #[test]
    fn legacy_reads_refuse_oversized_and_truncated_bodies() {
        let mut huge = ((MAX_FRAME as u32) - 1).to_le_bytes().to_vec();
        huge.extend_from_slice(&[0; 8]);
        let error = read_frame::<_, ClientMsg>(&mut huge.as_slice()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "a claimed length does not allocate it up front");
    }
}
