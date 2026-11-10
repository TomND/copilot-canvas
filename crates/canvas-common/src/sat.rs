//! The satellite link (docs/VM-SESSIONS.md §6): the messages between the
//! hub (the service that owns the canvas) and the satellite that runs in a
//! VM session's guest. They travel inside `ClientMsg::Sat` (satellite → hub)
//! and `ServiceMsg::Sat` (hub → satellite) over an ordinary protocol-8
//! connection whose credential is a satellite's (`handshake::Credential::
//! Satellite`); no other connection may send or receive them.
//!
//! Both enums are append-only (postcard encodes the variant index; a test
//! pins them) and versioned on their own ([`SAT_PROTOCOL`]), so hubs and
//! satellites of different builds work together within the range. Anything
//! that may grow later is a JSON part (`json`), because a postcard variant
//! can never gain a field.
//!
//! Terminal output carries `(generation, seq)`: `seq` is the byte offset of
//! the chunk's first byte in that generation's output, so a reconnect asks
//! for "everything from offset N" (`SatDown::Resync`). Names (`root`, `file`,
//! `name`) are relative and checked by both ends; roots are assigned by the
//! hub (`WelcomeInfo`), never chosen by the guest.

use serde::{Deserialize, Serialize};

/// The satellite protocol this build speaks (`SatUp::Hello::version`).
pub const SAT_PROTOCOL: u32 = 1;
/// The oldest satellite protocol a hub of this build accepts.
pub const SAT_PROTOCOL_MIN: u32 = 1;
/// Terminal input and output travel in chunks of at most this many bytes
/// (they go ahead of everything else on the link).
pub const SAT_IO_CHUNK: usize = 16 << 10;
/// History, files, notes and MCP payloads travel in pieces of at most this many bytes.
pub const SAT_FILE_CHUNK: usize = 1 << 20;

/// `SatUp::Mcp` / `SatDown::Mcp` kinds.
pub const MCP_OPEN: u8 = 0;
pub const MCP_MESSAGE: u8 = 1;
pub const MCP_CLOSE: u8 = 2;
pub const MCP_ERROR: u8 = 3;

/// `SatDown::Signal` signals.
pub const SIG_INT: u8 = 2;
pub const SIG_HUP: u8 = 1;
pub const SIG_TERM: u8 = 15;
pub const SIG_KILL: u8 = 9;

/// Satellite → hub (inside `ClientMsg::Sat`). Append-only.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum SatUp {
    /// The first message after the handshake: the satellite protocol, `CAP_*`
    /// style bits of its own (none yet) and a `HelloInfo` as JSON.
    Hello { version: u32, caps: u32, json: String },
    /// Input up to `seq` of this generation was handed to the PTY.
    InputAck { term: String, generation: u64, seq: u64 },
    /// Program output; `seq` is the offset of `bytes[0]` in the generation's output.
    Output { term: String, generation: u64, seq: u64, bytes: Vec<u8> },
    /// The screen at output offset `seq` as a VT repaint (a gap reset: the
    /// hub's ring no longer reaches back to what the hub has).
    Snapshot { term: String, generation: u64, seq: u64, cols: u16, rows: u16, vt: Vec<u8> },
    /// The program exited (`json`: details such as a signal or an error).
    Exit { term: String, generation: u64, code: u32, json: String },
    /// Liveness of the guest's processes and agent sessions (JSON `ProcReport`).
    Proc { json: String },
    /// Bytes appended to a history file at `offset`; `ident` tells files of
    /// the same name apart (a rotated or replaced file starts again at 0).
    History { root: String, file: String, ident: u64, offset: u64, bytes: Vec<u8> },
    /// A canvas operation a guest command asked for (`CliOp` JSON), run as
    /// terminal `term`; answered with `SatDown::OpResult` of the same id.
    Op { id: String, term: String, json: String },
    /// A note written in a read-write root (`base_rev`: the revision it was
    /// based on; `None`: a new file); answered with `SatDown::NoteResult`.
    Note { id: String, root: String, name: String, base_rev: Option<String>, bytes: Vec<u8> },
    /// An MCP stream ([`MCP_OPEN`] … [`MCP_ERROR`]); `bytes` is one JSON-RPC message or a JSON header.
    Mcp { stream: u64, kind: u8, bytes: Vec<u8> },
    /// Resources and git state (JSON `SatStats`).
    Stats { json: String },
    /// The answer to `SatDown::Control` of the same id.
    ControlResult { id: u64, json: String },
}

/// Hub → satellite (inside `ServiceMsg::Sat`). Append-only.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum SatDown {
    /// The answer to `SatUp::Hello`: the sandbox, its roots, limits and the
    /// terminals the hub expects (JSON `WelcomeInfo`).
    Welcome { json: String },
    /// Start a program under a PTY as generation `generation` of `term`.
    Spawn { term: String, generation: u64, argv: Vec<String>, env: Vec<(String, String)>, cwd: String, cols: u16, rows: u16 },
    /// Input numbered per (term, generation); applied at most once.
    Input { term: String, generation: u64, seq: u64, bytes: Vec<u8> },
    Resize { term: String, generation: u64, cols: u16, rows: u16 },
    Signal { term: String, generation: u64, signal: u8 },
    Stop { term: String, generation: u64, force: bool },
    /// The hub has the output up to offset `have`: replay from there, or
    /// send a `SatUp::Snapshot` when the ring no longer holds it.
    Resync { term: String, generation: u64, have: u64 },
    /// The answer to `SatUp::Op` (`OpAnswer` JSON).
    OpResult { id: String, json: String },
    /// A piece of a file of a hub-assigned root (`rev`: the whole file's revision).
    File { root: String, name: String, rev: String, offset: u64, last: bool, bytes: Vec<u8> },
    /// A file the hub no longer has in that root.
    Delete { root: String, name: String },
    /// The list of a root's files (JSON); files not listed are removed.
    Manifest { root: String, json: String },
    Mcp { stream: u64, kind: u8, bytes: Vec<u8> },
    /// A request to the satellite (JSON `ControlOp`), answered with `SatUp::ControlResult`.
    Control { id: u64, json: String },
    /// The answer to `SatUp::Note` (JSON: stored with its hub path and
    /// revision, or a conflict with the hub's text).
    NoteResult { id: String, json: String },
}

/// Whether `rel` may name a file inside a synced root: relative, `/`
/// separated, every component a plain name that no operating system reads
/// as something else (no `.` or `..`, no drive or stream colon, no
/// backslash, no control characters, no trailing dot or space, no Windows
/// device name, no `.git` folder or its 8.3 alias), at most 32 components and 1,024 bytes.
pub fn name_ok(rel: &str) -> bool {
    if rel.is_empty() || rel.len() > 1024 || rel.starts_with('/') {
        return false;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    parts.len() <= 32 && parts.iter().all(|part| component_ok(part))
}

fn component_ok(part: &str) -> bool {
    if part.is_empty() || part == "." || part == ".." || part.ends_with(['.', ' ']) || part.len() > 255 {
        return false;
    }
    if part.chars().any(|c| c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return false;
    }
    let stem = part.split('.').next().unwrap_or(part).trim_end().to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit());
    let git = part.eq_ignore_ascii_case(".git") || part.to_ascii_uppercase().starts_with("GIT~");
    !device && !git
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn satellite_variants_keep_their_indices() {
        let up = [
            (0, SatUp::Hello { version: SAT_PROTOCOL, caps: 0, json: "{}".into() }),
            (1, SatUp::InputAck { term: "t".into(), generation: 1, seq: 2 }),
            (2, SatUp::Output { term: "t".into(), generation: 1, seq: 0, bytes: b"hi".to_vec() }),
            (3, SatUp::Snapshot { term: "t".into(), generation: 1, seq: 9, cols: 80, rows: 24, vt: b"\x1bc".to_vec() }),
            (4, SatUp::Exit { term: "t".into(), generation: 1, code: 0, json: "{}".into() }),
            (5, SatUp::Proc { json: "{}".into() }),
            (6, SatUp::History { root: "copilot".into(), file: "s/events.jsonl".into(), ident: 7, offset: 0, bytes: b"{}\n".to_vec() }),
            (7, SatUp::Op { id: "o".into(), term: "t".into(), json: "{}".into() }),
            (8, SatUp::Note { id: "n".into(), root: "notes/g".into(), name: "a.md".into(), base_rev: None, bytes: b"x".to_vec() }),
            (9, SatUp::Mcp { stream: 1, kind: MCP_MESSAGE, bytes: b"{}".to_vec() }),
            (10, SatUp::Stats { json: "{}".into() }),
            (11, SatUp::ControlResult { id: 3, json: "{}".into() }),
        ];
        for (index, message) in up {
            let bytes = postcard::to_stdvec(&message).unwrap();
            assert_eq!(bytes[0], index, "{message:?}");
            assert_eq!(postcard::from_bytes::<SatUp>(&bytes).unwrap(), message);
        }
        let down = [
            (0, SatDown::Welcome { json: "{}".into() }),
            (1, SatDown::Spawn { term: "t".into(), generation: 1, argv: vec!["copilot".into()], env: vec![("A".into(), "b".into())], cwd: "/work".into(), cols: 80, rows: 24 }),
            (2, SatDown::Input { term: "t".into(), generation: 1, seq: 1, bytes: b"x".to_vec() }),
            (3, SatDown::Resize { term: "t".into(), generation: 1, cols: 100, rows: 30 }),
            (4, SatDown::Signal { term: "t".into(), generation: 1, signal: SIG_INT }),
            (5, SatDown::Stop { term: "t".into(), generation: 1, force: false }),
            (6, SatDown::Resync { term: "t".into(), generation: 1, have: 42 }),
            (7, SatDown::OpResult { id: "o".into(), json: "{}".into() }),
            (8, SatDown::File { root: "plugin".into(), name: "skills/a/SKILL.md".into(), rev: "ab".into(), offset: 0, last: true, bytes: b"x".to_vec() }),
            (9, SatDown::Delete { root: "plugin".into(), name: "old.md".into() }),
            (10, SatDown::Manifest { root: "plugin".into(), json: "[]".into() }),
            (11, SatDown::Mcp { stream: 1, kind: MCP_CLOSE, bytes: Vec::new() }),
            (12, SatDown::Control { id: 3, json: "{}".into() }),
            (13, SatDown::NoteResult { id: "n".into(), json: "{}".into() }),
        ];
        for (index, message) in down {
            let bytes = postcard::to_stdvec(&message).unwrap();
            assert_eq!(bytes[0], index, "{message:?}");
            assert_eq!(postcard::from_bytes::<SatDown>(&bytes).unwrap(), message);
        }
    }

    #[test]
    fn the_largest_pieces_fit_a_frame() {
        let output = crate::protocol::ClientMsg::Sat(Box::new(SatUp::History { root: "r".repeat(64), file: "f".repeat(1024), ident: u64::MAX, offset: u64::MAX, bytes: vec![0xff; SAT_FILE_CHUNK] }));
        assert!(postcard::to_stdvec(&output).unwrap().len() < crate::protocol::MAX_FRAME);
        let input = crate::protocol::ServiceMsg::Sat(Box::new(SatDown::Input { term: "t".repeat(100), generation: u64::MAX, seq: u64::MAX, bytes: vec![0xff; SAT_IO_CHUNK] }));
        assert!(postcard::to_stdvec(&input).unwrap().len() < SAT_IO_CHUNK + 256);
    }

    #[test]
    fn synced_names_are_plain_relative_paths() {
        for good in ["a.md", "ai-docs/plan.md", "skills/review/SKILL.md", "s/0f3a/events.jsonl", ".claude-plugin/plugin.json", "a b/c-d_e.txt"] {
            assert!(name_ok(good), "{good}");
        }
        for bad in [
            "", "/etc/passwd", "../x", "a/../b", "a/./b", "a//b", r"a\b", "C:/x", "c:x", "x:stream", "nul.md", "NUL", "con.txt", "com1", "LPT9.log", "GIT~1/config", ".git/config", "a/.GIT/hooks/x",
            "trailing.", "trailing ", "a\u{7}b", "a\nb", "q?", "s*", "x|y",
        ] {
            assert!(!name_ok(bad), "{bad:?}");
        }
        assert!(!name_ok(&"a/".repeat(40)), "too deep");
        assert!(!name_ok(&"x".repeat(1025)), "too long");
        assert!(name_ok("com10.txt") && name_ok("console.log"), "only the device names themselves");
    }
}
