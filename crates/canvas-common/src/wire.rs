//! Framing shared by every transport, and connection liveness.
//!
//! A frame is a little-endian u32 length and a postcard body. On TCP, once
//! compression is negotiated, the length's high bit marks a zstd-compressed
//! body: `[u32 uncompressed length][zstd block]`. Each direction of a
//! connection is one zstd stream and every compressed frame is a flushed
//! block of it, so rows repeated across frames cost almost nothing. Bodies of
//! `COMPRESS_MIN` bytes or less are sent as they are.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Serialize, de::DeserializeOwned};

use crate::protocol::MAX_FRAME;

/// Length bit marking a compressed body.
pub const COMPRESSED: u32 = 1 << 31;
/// Bodies up to this size are never compressed.
pub const COMPRESS_MIN: usize = 512;
const LEVEL: i32 = 3;

/// Heartbeat interval of protocol-8 connections.
pub const HEARTBEAT: Duration = Duration::from_secs(2);
/// Silence after which a protocol-8 connection is dead.
pub const DEAD_AFTER: Duration = Duration::from_secs(6);

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

/// Ends a connection: blocked reads and writes on it return.
#[derive(Clone)]
pub struct Closer(Arc<dyn Fn() + Send + Sync>);

impl Closer {
    pub fn new(close: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(close))
    }

    pub fn close(&self) {
        (self.0)()
    }
}

/// Bytes and frames on the wire (after compression).
#[derive(Default, Debug)]
pub struct WireStats {
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub frames_in: AtomicU64,
    pub frames_out: AtomicU64,
}

/// Writes frames; compresses large bodies once `compress` was called.
pub struct FrameWriter {
    inner: Box<dyn Write + Send>,
    zstd: Option<zstd::stream::write::Encoder<'static, Vec<u8>>>,
    stats: Arc<WireStats>,
}

impl FrameWriter {
    pub fn new(inner: Box<dyn Write + Send>) -> Self {
        Self { inner, zstd: None, stats: Arc::default() }
    }

    pub fn with_stats(mut self, stats: Arc<WireStats>) -> Self {
        self.stats = stats;
        self
    }

    /// Compresses every later body above `COMPRESS_MIN` (TCP, negotiated).
    pub fn compress(&mut self) -> io::Result<()> {
        if self.zstd.is_none() {
            self.zstd = Some(zstd::stream::write::Encoder::new(Vec::new(), LEVEL)?);
        }
        Ok(())
    }

    pub fn compressing(&self) -> bool {
        self.zstd.is_some()
    }

    /// Sends one message; returns the bytes written.
    pub fn send<T: Serialize>(&mut self, message: &T) -> io::Result<usize> {
        let body = postcard::to_stdvec(message).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if body.len() > MAX_FRAME {
            return Err(invalid("frame too large"));
        }
        let packet = match &mut self.zstd {
            Some(encoder) if body.len() > COMPRESS_MIN => {
                encoder.write_all(&body)?;
                encoder.flush()?;
                let block = std::mem::take(encoder.get_mut());
                let mut packet = Vec::with_capacity(block.len() + 8);
                packet.extend_from_slice(&((block.len() + 4) as u32 | COMPRESSED).to_le_bytes());
                packet.extend_from_slice(&(body.len() as u32).to_le_bytes());
                packet.extend_from_slice(&block);
                packet
            }
            _ => {
                let mut packet = Vec::with_capacity(body.len() + 4);
                packet.extend_from_slice(&(body.len() as u32).to_le_bytes());
                packet.extend_from_slice(&body);
                packet
            }
        };
        self.inner.write_all(&packet)?;
        self.stats.bytes_out.fetch_add(packet.len() as u64, Ordering::Relaxed);
        self.stats.frames_out.fetch_add(1, Ordering::Relaxed);
        Ok(packet.len())
    }
}

/// Reads frames; accepts compressed bodies once `accept_compressed` was called.
pub struct FrameReader {
    inner: Box<dyn Read + Send>,
    zstd: Option<zstd::stream::write::Decoder<'static, Vec<u8>>>,
    stats: Arc<WireStats>,
}

impl FrameReader {
    pub fn new(inner: Box<dyn Read + Send>) -> Self {
        Self { inner, zstd: None, stats: Arc::default() }
    }

    pub fn with_stats(mut self, stats: Arc<WireStats>) -> Self {
        self.stats = stats;
        self
    }

    pub fn accept_compressed(&mut self) -> io::Result<()> {
        if self.zstd.is_none() {
            self.zstd = Some(zstd::stream::write::Decoder::new(Vec::new())?);
        }
        Ok(())
    }

    /// `Ok(None)` at a clean end of stream.
    pub fn recv<T: DeserializeOwned>(&mut self) -> io::Result<Option<T>> {
        match self.recv_lenient()? {
            None => Ok(None),
            Some(Ok(message)) => Ok(Some(message)),
            Some(Err(_)) => Err(invalid("undecodable frame")),
        }
    }

    /// Like `recv`, but a complete frame that does not decode (a peer of
    /// another protocol version) is returned as `Some(Err(body))`.
    pub fn recv_lenient<T: DeserializeOwned>(&mut self) -> io::Result<Option<Result<T, Vec<u8>>>> {
        let mut header = [0u8; 4];
        match self.inner.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if matches!(error.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe) => return Ok(None),
            Err(error) => return Err(error),
        }
        let raw = u32::from_le_bytes(header);
        let compressed = raw & COMPRESSED != 0 && self.zstd.is_some();
        let len = if compressed { (raw & !COMPRESSED) as usize } else { raw as usize };
        if len > MAX_FRAME {
            return Err(invalid("frame too large"));
        }
        let mut body = vec![0u8; len];
        self.inner.read_exact(&mut body)?;
        self.stats.bytes_in.fetch_add(len as u64 + 4, Ordering::Relaxed);
        self.stats.frames_in.fetch_add(1, Ordering::Relaxed);
        if compressed {
            body = self.decompress(&body)?;
        }
        Ok(Some(postcard::from_bytes(&body).map_err(|_| body)))
    }

    fn decompress(&mut self, packet: &[u8]) -> io::Result<Vec<u8>> {
        let decoder = self.zstd.as_mut().expect("checked by the caller");
        let size = packet.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or_else(|| invalid("short compressed frame"))?;
        if size > MAX_FRAME {
            return Err(invalid("frame too large"));
        }
        decoder.write_all(&packet[4..])?;
        decoder.flush()?;
        let body = std::mem::take(decoder.get_mut());
        if body.len() != size {
            return Err(invalid("compressed frame has the wrong size"));
        }
        Ok(body)
    }
}

/// When the peer was last heard from; a connection silent for `timeout` is dead.
pub struct Liveness {
    epoch: Instant,
    last_ms: AtomicU64,
    pub interval: Duration,
    pub timeout: Duration,
}

impl Liveness {
    pub fn new(interval: Duration, timeout: Duration) -> Self {
        Self { epoch: Instant::now(), last_ms: AtomicU64::new(0), interval, timeout }
    }

    /// Heartbeat every 2 s, dead after 6 s.
    pub fn standard() -> Self {
        Self::new(HEARTBEAT, DEAD_AFTER)
    }

    /// Anything arrived from the peer.
    pub fn received(&self) {
        self.last_ms.store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    pub fn silence(&self) -> Duration {
        self.epoch.elapsed().saturating_sub(Duration::from_millis(self.last_ms.load(Ordering::Relaxed)))
    }

    pub fn dead(&self) -> bool {
        self.silence() >= self.timeout
    }
}

/// Schedules outgoing heartbeats.
pub struct Beat {
    next: Instant,
    interval: Duration,
}

impl Beat {
    pub fn new(interval: Duration) -> Self {
        Self { next: Instant::now() + interval, interval }
    }

    /// When the next heartbeat is due.
    pub fn next(&self) -> Instant {
        self.next
    }

    /// True (and rescheduled) when a heartbeat is due at `now`.
    pub fn due(&mut self, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now + self.interval;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClientMsg, Frame8, ServiceMsg};

    /// An in-memory pipe end that appends to a shared buffer.
    #[derive(Clone, Default)]
    struct Shared(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn big_frame(n: u32) -> ServiceMsg {
        let mut frame: Frame8 = crate::protocol::tests::sample_frame8();
        for y in 0..40u16 {
            let text = format!("● step {n:>6} reading src/module_{:03}.rs · tool result ok — lorem ipsum dolor", (n + y as u32) % 997);
            frame.changed.push((y, vec![crate::protocol::Run8 { col: 0, text, fg: 2 + y as u32 % 6, ..Default::default() }]));
        }
        ServiceMsg::Frame8(frame)
    }

    fn round_trip(compress: bool) -> (Vec<ServiceMsg>, usize) {
        let sink = Shared::default();
        let mut writer = FrameWriter::new(Box::new(sink.clone()));
        if compress {
            writer.compress().unwrap();
        }
        let messages: Vec<ServiceMsg> = (0..20).map(big_frame).chain([ServiceMsg::Heartbeat { input_ack: 5 }]).collect();
        let mut wire = 0;
        for message in &messages {
            wire += writer.send(message).unwrap();
        }
        let bytes = sink.0.lock().unwrap().clone();
        assert_eq!(bytes.len(), wire);
        let mut reader = FrameReader::new(Box::new(io::Cursor::new(bytes)));
        if compress {
            reader.accept_compressed().unwrap();
        }
        let mut out = Vec::new();
        while let Some(message) = reader.recv::<ServiceMsg>().unwrap() {
            out.push(message);
        }
        assert_eq!(out, messages);
        (out, wire)
    }

    #[test]
    fn frames_round_trip_with_and_without_compression() {
        let (_, plain) = round_trip(false);
        let (_, packed) = round_trip(true);
        assert!(packed * 4 < plain, "streaming zstd shrinks repetitive frames: {packed} vs {plain} bytes");
    }

    #[test]
    fn small_bodies_stay_uncompressed_and_plain_readers_reject_compressed_frames() {
        let sink = Shared::default();
        let mut writer = FrameWriter::new(Box::new(sink.clone()));
        writer.compress().unwrap();
        writer.send(&ClientMsg::Heartbeat).unwrap();
        let small = sink.0.lock().unwrap().clone();
        assert_eq!(u32::from_le_bytes(small[..4].try_into().unwrap()) & COMPRESSED, 0);
        writer.send(&big_frame(1)).unwrap();
        let all = sink.0.lock().unwrap().clone();
        let mut plain = FrameReader::new(Box::new(io::Cursor::new(all)));
        assert_eq!(plain.recv::<ClientMsg>().unwrap(), Some(ClientMsg::Heartbeat));
        assert!(plain.recv::<ServiceMsg>().is_err(), "a reader that did not negotiate compression refuses it");
    }

    #[test]
    fn oversized_and_truncated_frames_are_errors() {
        let mut bytes = ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0; 16]);
        assert!(FrameReader::new(Box::new(io::Cursor::new(bytes.clone()))).recv::<ClientMsg>().is_err());
        let mut compressed = FrameReader::new(Box::new(io::Cursor::new(bytes)));
        compressed.accept_compressed().unwrap();
        assert!(compressed.recv::<ClientMsg>().is_err());
        let mut forged = ((8u32) | COMPRESSED).to_le_bytes().to_vec();
        forged.extend_from_slice(&((MAX_FRAME as u32) + 1).to_le_bytes());
        forged.extend_from_slice(&[0; 4]);
        let mut reader = FrameReader::new(Box::new(io::Cursor::new(forged)));
        reader.accept_compressed().unwrap();
        assert!(reader.recv::<ClientMsg>().is_err(), "claimed size above the limit");
        // A frame cut in its body is an error, not a message.
        let sink = Shared::default();
        FrameWriter::new(Box::new(sink.clone())).send(&big_frame(2)).unwrap();
        let mut cut = sink.0.lock().unwrap().clone();
        cut.truncate(cut.len() / 2);
        assert!(FrameReader::new(Box::new(io::Cursor::new(cut))).recv::<ServiceMsg>().is_err());
        assert_eq!(FrameReader::new(Box::new(io::Cursor::new(Vec::new()))).recv::<ServiceMsg>().unwrap(), None);
    }

    #[test]
    fn liveness_and_heartbeat_timing() {
        let live = Liveness::new(Duration::from_millis(20), Duration::from_millis(60));
        live.received();
        assert!(!live.dead());
        std::thread::sleep(Duration::from_millis(30));
        assert!(!live.dead());
        live.received();
        std::thread::sleep(Duration::from_millis(40));
        assert!(!live.dead(), "a message reset the silence");
        std::thread::sleep(Duration::from_millis(40));
        assert!(live.dead());
        let mut beat = Beat::new(Duration::from_millis(20));
        let start = Instant::now();
        assert!(!beat.due(start));
        assert!(beat.due(start + Duration::from_millis(25)));
        assert!(!beat.due(start + Duration::from_millis(30)));
        assert!(beat.next() > start + Duration::from_millis(40));
    }
}
