//! `.vtrec`: terminal output with its read-chunk boundaries and timing.
//!
//! JSON lines (UTF-8, LF; CRLF is tolerated). The first line is a [`Header`],
//! every further line one [`Event`]:
//!
//! ```text
//! {"vtrec":1,"cols":120,"rows":40,"program":"powershell.exe","description":"…","source":"conpty","recorded_at":"2026-10-27T10:00:00.000Z"}
//! {"t":12,"s":"\u001b[?25lPS C:\\> "}   output read whose bytes are valid UTF-8
//! {"t":3,"x":"e4b8"}                     output read as hex (any bytes)
//! {"t":0,"resize":[100,30]}              the terminal was resized
//! {"t":500,"in":"dir\r"}                 input the recorder typed ("inx": hex); not replayed
//! {"t":0,"mark":"listing"}               checkpoint: expected files keep the screen here
//! ```
//!
//! `t` is milliseconds since the previous line (for the first event: since
//! the program started). A later header line starts a new segment (the
//! service tap appends when a terminal id is reused); readers use the last.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Header {
    /// Format version ([`VERSION`]).
    pub vtrec: u32,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub program: String,
    #[serde(default)]
    pub description: String,
    /// `conpty` (the recorder), `service` (the live tap) or `synthetic`.
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub recorded_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// One read of program output.
    Output(Vec<u8>),
    Resize(u16, u16),
    /// Input the recorder wrote (documentation only).
    Input(Vec<u8>),
    /// A checkpoint whose screen the expected file keeps.
    Mark(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Milliseconds since the previous event.
    pub delta_ms: u64,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Recording {
    pub header: Header,
    pub events: Vec<Event>,
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

pub fn unhex(text: &str) -> Result<Vec<u8>> {
    ensure!(text.len().is_multiple_of(2), "odd-length hex");
    let digit = |c: u8| match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => bail!("invalid hex digit {:?}", c as char),
    };
    text.as_bytes().chunks(2).map(|pair| Ok(digit(pair[0])? << 4 | digit(pair[1])?)).collect()
}

/// `"key":value` with the readable key for UTF-8 and the hex key otherwise.
fn bytes_field(text_key: &str, hex_key: &str, bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => format!("\"{text_key}\":{}", Value::String(text.to_owned())),
        Err(_) => format!("\"{hex_key}\":\"{}\"", hex(bytes)),
    }
}

impl Event {
    pub fn output(delta_ms: u64, bytes: impl Into<Vec<u8>>) -> Self {
        Self { delta_ms, kind: Kind::Output(bytes.into()) }
    }

    /// One JSON line (without the newline).
    pub fn to_line(&self) -> String {
        let t = self.delta_ms;
        match &self.kind {
            Kind::Output(bytes) => format!("{{\"t\":{t},{}}}", bytes_field("s", "x", bytes)),
            Kind::Resize(cols, rows) => format!("{{\"t\":{t},\"resize\":[{cols},{rows}]}}"),
            Kind::Input(bytes) => format!("{{\"t\":{t},{}}}", bytes_field("in", "inx", bytes)),
            Kind::Mark(name) => format!("{{\"t\":{t},\"mark\":{}}}", Value::String(name.clone())),
        }
    }

    /// `None` for lines of unknown kinds (newer writers).
    fn from_map(map: &Map<String, Value>) -> Result<Option<Self>> {
        let delta_ms = map.get("t").and_then(Value::as_u64).unwrap_or(0);
        let text = |key: &str| map.get(key).and_then(Value::as_str);
        let kind = if let Some(s) = text("s") {
            Kind::Output(s.as_bytes().to_vec())
        } else if let Some(x) = text("x") {
            Kind::Output(unhex(x)?)
        } else if let Some(size) = map.get("resize") {
            let (cols, rows): (u16, u16) = serde_json::from_value(size.clone()).context("resize needs [cols, rows]")?;
            Kind::Resize(cols, rows)
        } else if let Some(s) = text("in") {
            Kind::Input(s.as_bytes().to_vec())
        } else if let Some(x) = text("inx") {
            Kind::Input(unhex(x)?)
        } else if let Some(name) = text("mark") {
            Kind::Mark(name.to_owned())
        } else {
            return Ok(None);
        };
        Ok(Some(Self { delta_ms, kind }))
    }
}

impl Recording {
    pub fn new(header: Header) -> Self {
        Self { header, events: Vec::new() }
    }

    /// Every segment of a file; a truncated last line (a live tap mid-write) is ignored.
    pub fn segments(text: &str) -> Result<Vec<Recording>> {
        let lines: Vec<(usize, &str)> =
            text.lines().map(str::trim_end).enumerate().filter(|(_, line)| !line.is_empty()).collect();
        let mut segments: Vec<Recording> = Vec::new();
        for (i, &(number, line)) in lines.iter().enumerate() {
            let map: Map<String, Value> = match serde_json::from_str(line) {
                Ok(map) => map,
                Err(_) if i + 1 == lines.len() && i > 0 => break,
                Err(error) => bail!("line {}: {error}", number + 1),
            };
            if map.contains_key("vtrec") {
                let header: Header = serde_json::from_value(Value::Object(map)).with_context(|| format!("line {}: header", number + 1))?;
                ensure!(header.vtrec <= VERSION, "line {}: vtrec version {} is newer than this build ({VERSION})", number + 1, header.vtrec);
                ensure!(header.cols >= 2 && header.rows >= 2, "line {}: terminal size {}x{} is too small", number + 1, header.cols, header.rows);
                segments.push(Recording::new(header));
                continue;
            }
            let Some(segment) = segments.last_mut() else { bail!("line {}: expected a vtrec header", number + 1) };
            if let Some(event) = Event::from_map(&map).with_context(|| format!("line {}", number + 1))? {
                if let Kind::Resize(cols, rows) = event.kind {
                    ensure!(cols >= 2 && rows >= 2, "line {}: resize to {cols}x{rows} is too small", number + 1);
                }
                segment.events.push(event);
            }
        }
        ensure!(!segments.is_empty(), "not a vtrec recording (no header)");
        Ok(segments)
    }

    /// The last segment.
    pub fn parse(text: &str) -> Result<Self> {
        Ok(Self::segments(text)?.pop().expect("at least one segment"))
    }

    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn to_text(&self) -> String {
        let mut out = serde_json::to_string(&self.header).expect("header serializes");
        out.push('\n');
        for event in &self.events {
            out.push_str(&event.to_line());
            out.push('\n');
        }
        out
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
        }
        std::fs::write(path, self.to_text()).with_context(|| format!("cannot write {}", path.display()))
    }

    pub fn chunks(&self) -> usize {
        self.events.iter().filter(|e| matches!(e.kind, Kind::Output(_))).count()
    }

    pub fn output_bytes(&self) -> usize {
        self.events.iter().map(|e| if let Kind::Output(b) = &e.kind { b.len() } else { 0 }).sum()
    }

    pub fn duration_ms(&self) -> u64 {
        self.events.iter().map(|e| e.delta_ms).sum()
    }

    pub fn marks(&self) -> Vec<&str> {
        self.events.iter().filter_map(|e| if let Kind::Mark(m) = &e.kind { Some(m.as_str()) } else { None }).collect()
    }

    /// All output concatenated.
    pub fn output(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.output_bytes());
        for event in &self.events {
            if let Kind::Output(bytes) = &event.kind {
                out.extend_from_slice(bytes);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header { vtrec: 1, cols: 20, rows: 5, program: "p".into(), description: "d".into(), source: "synthetic".into(), recorded_at: String::new() }
    }

    #[test]
    fn round_trips_every_kind() {
        let mut rec = Recording::new(header());
        rec.events = vec![
            Event::output(5, "\x1b[31mred\x1b[0m \"q\" \\ 中\r\n".as_bytes()),
            Event::output(1, vec![0xe4, 0xb8]),
            Event { delta_ms: 0, kind: Kind::Resize(30, 6) },
            Event { delta_ms: 7, kind: Kind::Input(b"dir\r".to_vec()) },
            Event { delta_ms: 0, kind: Kind::Input(vec![0xff]) },
            Event { delta_ms: 0, kind: Kind::Mark("m".into()) },
        ];
        let text = rec.to_text();
        assert!(text.contains("\"x\":\"e4b8\""), "{text}");
        assert!(text.contains("\"s\":\"\\u001b[31mred"), "{text}");
        assert_eq!(Recording::parse(&text).unwrap(), rec);
        assert_eq!(Recording::parse(&text.replace('\n', "\r\n")).unwrap(), rec, "CRLF tolerated");
        assert_eq!((rec.chunks(), rec.output_bytes(), rec.duration_ms(), rec.marks()), (2, 26, 13, vec!["m"]));
    }

    #[test]
    fn last_segment_wins_and_truncated_tail_is_ignored() {
        let text = "{\"vtrec\":1,\"cols\":10,\"rows\":3}\n{\"t\":1,\"s\":\"a\"}\n{\"vtrec\":1,\"cols\":12,\"rows\":4}\n{\"t\":2,\"x\":\"62\"}\n{\"t\":3,\"s\":\"tru";
        let segments = Recording::segments(text).unwrap();
        assert_eq!(segments.len(), 2);
        let last = Recording::parse(text).unwrap();
        assert_eq!((last.header.cols, last.events.len(), last.output()), (12, 1, b"b".to_vec()));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Recording::parse("{\"t\":1,\"s\":\"a\"}\n").is_err(), "no header");
        assert!(Recording::parse("{\"vtrec\":9,\"cols\":10,\"rows\":3}\n").is_err(), "newer version");
        assert!(Recording::parse("{\"vtrec\":1,\"cols\":10,\"rows\":3}\nnot json\n{\"t\":1,\"s\":\"a\"}\n").is_err());
        assert!(unhex("abc").is_err() && unhex("zz").is_err());
        assert_eq!(unhex(&hex(&[0, 1, 0xab, 0xff])).unwrap(), vec![0, 1, 0xab, 0xff]);
    }
}
