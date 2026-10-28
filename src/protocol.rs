//! Wire protocol between the TUI client and the terminal service.
//!
//! Frames are a little-endian u32 length followed by a postcard-encoded message.

use std::io::{self, Read, Write};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::Sha256;

pub const PROTOCOL: u32 = 7;
const MAX_FRAME: usize = 32 * 1024 * 1024;

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
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
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
mod tests {
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
}
