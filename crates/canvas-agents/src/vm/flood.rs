//! A satellite's link is held to limits (plan VM-24): a guest may be
//! compromised, so nothing it sends may cost the hub more than a link's
//! share. Each message has a size cap (over it: the link is closed), the
//! bytes a link brings in are paced (beyond the rate the hub stops reading
//! and TCP slows the guest), reports are taken at most so often (extras are
//! dropped), and a link holds at most so many MCP streams.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use canvas_common::sat::{SAT_FILE_CHUNK, SAT_IO_CHUNK, SatUp};

/// Bytes a link may bring in per second, and at once.
const RATE: f64 = (32 << 20) as f64;
const BURST: f64 = (64 << 20) as f64;
/// Reports taken at most this often.
const PROC_EVERY: Duration = Duration::from_millis(500);
const STATS_EVERY: Duration = Duration::from_secs(1);
/// MCP streams one link may hold open.
pub const MCP_STREAMS: usize = 32;

/// What a message may weigh at most.
pub fn size(up: &SatUp) -> Result<usize> {
    let (what, size, cap) = match up {
        SatUp::Hello { json, .. } => ("a Hello", json.len(), 1 << 20),
        SatUp::InputAck { term, .. } => ("an input acknowledgement", term.len(), 4 << 10),
        SatUp::Output { term, bytes, .. } => ("output", term.len() + bytes.len(), 2 * SAT_IO_CHUNK + (4 << 10)),
        SatUp::Snapshot { term, vt, .. } => ("a screen", term.len() + vt.len(), 8 << 20),
        SatUp::Exit { term, json, .. } => ("an exit", term.len() + json.len(), 64 << 10),
        SatUp::Proc { json } => ("a process report", json.len(), 256 << 10),
        SatUp::Stats { json } => ("a stats report", json.len(), 256 << 10),
        SatUp::History { root, file, bytes, .. } => ("history", root.len() + file.len() + bytes.len(), 2 * SAT_FILE_CHUNK),
        SatUp::Op { id, term, json } => ("an operation", id.len() + term.len() + json.len(), 24 << 20),
        SatUp::Note { id, root, name, bytes, .. } => ("a note", id.len() + root.len() + name.len() + bytes.len(), 2 * SAT_FILE_CHUNK),
        SatUp::Mcp { bytes, .. } => ("an MCP message", bytes.len(), SAT_FILE_CHUNK + (64 << 10)),
        SatUp::ControlResult { json, .. } => ("a control's answer", json.len(), 30 << 20),
    };
    if size > cap {
        bail!("the satellite sent {what} of {size} bytes (at most {cap})");
    }
    Ok(size)
}

pub struct Flood {
    tokens: f64,
    at: Instant,
    rate: f64,
    burst: f64,
    last_proc: Option<Instant>,
    last_stats: Option<Instant>,
}

impl Default for Flood {
    fn default() -> Self {
        Flood::with_rate(RATE, BURST)
    }
}

impl Flood {
    pub fn with_rate(rate: f64, burst: f64) -> Self {
        Flood { tokens: burst, at: Instant::now(), rate, burst, last_proc: None, last_stats: None }
    }

    /// Takes `bytes` from the link's allowance: how long to wait before reading on.
    pub fn take(&mut self, bytes: usize, now: Instant) -> Duration {
        self.tokens = (self.tokens + now.saturating_duration_since(self.at).as_secs_f64() * self.rate).min(self.burst);
        self.at = now;
        self.tokens -= bytes as f64;
        if self.tokens >= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(-self.tokens / self.rate) }
    }

    /// Whether a process report is taken now (else it is dropped).
    pub fn proc_due(&mut self, now: Instant) -> bool {
        due(&mut self.last_proc, PROC_EVERY, now)
    }

    /// Whether a stats report is taken now (else it is dropped).
    pub fn stats_due(&mut self, now: Instant) -> bool {
        due(&mut self.last_stats, STATS_EVERY, now)
    }
}

fn due(last: &mut Option<Instant>, every: Duration, now: Instant) -> bool {
    if last.is_some_and(|at| now.saturating_duration_since(at) < every) {
        return false;
    }
    *last = Some(now);
    true
}

/// The MCP streams a link holds open.
#[derive(Default)]
pub struct Streams(HashSet<u64>);

impl Streams {
    /// A new stream; `false` when the link holds [`MCP_STREAMS`] already (or this one).
    pub fn open(&mut self, stream: u64) -> bool {
        self.0.len() < MCP_STREAMS && self.0.insert(stream)
    }

    pub fn close(&mut self, stream: u64) {
        self.0.remove(&stream);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_capped_by_kind() {
        assert!(size(&SatUp::Output { term: "t".into(), generation: 1, seq: 0, bytes: vec![0; SAT_IO_CHUNK] }).is_ok());
        assert!(size(&SatUp::Output { term: "t".into(), generation: 1, seq: 0, bytes: vec![0; 3 * SAT_IO_CHUNK] }).unwrap_err().to_string().contains("output of"));
        assert!(size(&SatUp::Proc { json: "x".repeat(300 << 10) }).is_err());
        assert!(size(&SatUp::Snapshot { term: "t".into(), generation: 1, seq: 0, cols: 80, rows: 24, vt: vec![0; 9 << 20] }).is_err());
    }

    #[test]
    fn a_link_is_paced_past_its_burst_and_reports_are_thinned() {
        let start = Instant::now();
        let mut flood = Flood::with_rate(1000.0, 2000.0);
        assert_eq!(flood.take(1500, start), Duration::ZERO);
        assert_eq!(flood.take(1000, start), Duration::from_millis(500), "500 bytes over: half a second");
        assert_eq!(flood.take(0, start + Duration::from_millis(500)), Duration::ZERO, "refilled meanwhile");
        assert!(flood.proc_due(start) && !flood.proc_due(start + Duration::from_millis(100)) && flood.proc_due(start + Duration::from_millis(600)));
        assert!(flood.stats_due(start) && !flood.stats_due(start + Duration::from_millis(500)));
    }

    #[test]
    fn a_link_holds_a_bounded_number_of_mcp_streams() {
        let mut streams = Streams::default();
        for id in 0..MCP_STREAMS as u64 {
            assert!(streams.open(id));
        }
        assert!(!streams.open(99), "full");
        assert!(!streams.open(3), "and never the same id twice");
        streams.close(3);
        assert!(streams.open(99));
    }
}
