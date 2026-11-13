//! Framing shared by every transport, and connection liveness.
//!
//! A frame is a little-endian u32 length and a postcard body. On TCP, once
//! compression is negotiated, the length's high bit marks a zstd-compressed
//! body: `[u32 uncompressed length][zstd block]`. Each direction of a
//! connection is one zstd stream and every compressed frame is a flushed
//! block of it, so rows repeated across frames cost almost nothing. Bodies of
//! `COMPRESS_MIN` bytes or less are sent as they are.
//!
//! Readers take a body limit: [`PRE_AUTH_FRAME`] until the peer authenticated,
//! then [`MAX_FRAME`]. Bodies are read as they arrive (memory grows with the
//! bytes received, not with the length a peer claims), and decompression
//! stops at the size the frame announced.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Serialize, de::DeserializeOwned};

pub use crate::protocol::{MAX_FRAME, PRE_AUTH_FRAME};

/// Length bit marking a compressed body.
pub const COMPRESSED: u32 = 1 << 31;
/// Bodies up to this size are never compressed.
pub const COMPRESS_MIN: usize = 512;
const LEVEL: i32 = 3;
/// Largest zstd window a reader accepts (16 MiB; level 3 uses 2 MiB).
const WINDOW_LOG_MAX: u32 = 24;

/// Heartbeat interval of protocol-8 connections.
pub const HEARTBEAT: Duration = Duration::from_secs(2);
/// Silence after which a protocol-8 connection is dead.
pub const DEAD_AFTER: Duration = Duration::from_secs(6);
/// How long a reader busy with a message of its own ([`Liveness::busy`]) keeps
/// its connection from being judged silent: past this the reader is stuck,
/// and the connection may be closed as before.
pub const BUSY_GRACE: Duration = Duration::from_secs(60);

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

/// Reads exactly `len` bytes, growing the buffer as they arrive.
pub(crate) fn read_body(reader: impl Read, len: usize) -> io::Result<Vec<u8>> {
    let mut body = Vec::with_capacity(len.min(64 * 1024));
    reader.take(len as u64).read_to_end(&mut body)?;
    if body.len() != len {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the connection ended inside a frame"));
    }
    Ok(body)
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

    /// Sends one message; returns the bytes written. A message that cannot be
    /// encoded or is too large fails with `InvalidData` before anything is
    /// written (the connection stays usable).
    pub fn send<T: Serialize + ?Sized>(&mut self, message: &T) -> io::Result<usize> {
        // Serialized right after room for the length, so the body is not copied again.
        let mut packet = postcard::to_extend(message, vec![0u8; 4]).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let len = packet.len() - 4;
        if len > MAX_FRAME {
            return Err(invalid("frame too large"));
        }
        let packet = match &mut self.zstd {
            Some(encoder) if len > COMPRESS_MIN => {
                encoder.write_all(&packet[4..])?;
                encoder.flush()?;
                let block = std::mem::take(encoder.get_mut());
                let mut compressed = Vec::with_capacity(block.len() + 8);
                compressed.extend_from_slice(&((block.len() + 4) as u32 | COMPRESSED).to_le_bytes());
                compressed.extend_from_slice(&(len as u32).to_le_bytes());
                compressed.extend_from_slice(&block);
                compressed
            }
            _ => {
                packet[..4].copy_from_slice(&(len as u32).to_le_bytes());
                packet
            }
        };
        self.inner.write_all(&packet)?;
        self.stats.bytes_out.fetch_add(packet.len() as u64, Ordering::Relaxed);
        self.stats.frames_out.fetch_add(1, Ordering::Relaxed);
        Ok(packet.len())
    }
}

/// Collects decompressed bytes and refuses to grow past `limit` (the size
/// the frame announced), so a small block cannot expand without bound.
#[derive(Default)]
struct Bounded {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.bytes.len() + buf.len() > self.limit {
            return Err(invalid("a compressed frame expands beyond its announced size"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads frames; accepts compressed bodies once `accept_compressed` was called.
pub struct FrameReader {
    inner: Box<dyn Read + Send>,
    zstd: Option<zstd::stream::write::Decoder<'static, Bounded>>,
    stats: Arc<WireStats>,
    /// Largest body accepted.
    limit: usize,
}

impl FrameReader {
    pub fn new(inner: Box<dyn Read + Send>) -> Self {
        Self { inner, zstd: None, stats: Arc::default(), limit: MAX_FRAME }
    }

    pub fn with_stats(mut self, stats: Arc<WireStats>) -> Self {
        self.stats = stats;
        self
    }

    /// The largest body accepted (at most [`MAX_FRAME`]).
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.set_limit(limit);
        self
    }

    /// Changes the body limit: [`PRE_AUTH_FRAME`] before the handshake, [`MAX_FRAME`] after it.
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.min(MAX_FRAME);
    }

    /// Marks `liveness` whenever bytes arrive, not only when a whole message
    /// was read: a large message on a slow link (an image paste, an upload
    /// piece, a big reply) keeps its connection alive while it flows.
    pub fn with_liveness(mut self, liveness: Arc<Liveness>) -> Self {
        self.set_liveness(liveness);
        self
    }

    /// [`FrameReader::with_liveness`] on a reader already in use (after the handshake).
    pub fn set_liveness(&mut self, liveness: Arc<Liveness>) {
        let inner = std::mem::replace(&mut self.inner, Box::new(io::empty()));
        self.inner = Box::new(Heard { inner, liveness });
    }

    pub fn accept_compressed(&mut self) -> io::Result<()> {
        if self.zstd.is_none() {
            let mut decoder = zstd::stream::write::Decoder::new(Bounded::default())?;
            decoder.window_log_max(WINDOW_LOG_MAX)?;
            self.zstd = Some(decoder);
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
        if len > self.limit {
            return Err(invalid("frame too large"));
        }
        let body = read_body(&mut self.inner, len)?;
        self.stats.bytes_in.fetch_add(len as u64 + 4, Ordering::Relaxed);
        self.stats.frames_in.fetch_add(1, Ordering::Relaxed);
        let body = if compressed { self.decompress(&body)? } else { body };
        Ok(Some(postcard::from_bytes(&body).map_err(|_| body)))
    }

    fn decompress(&mut self, packet: &[u8]) -> io::Result<Vec<u8>> {
        let limit = self.limit;
        let decoder = self.zstd.as_mut().expect("checked by the caller");
        let size = packet.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or_else(|| invalid("short compressed frame"))?;
        if size > limit {
            return Err(invalid("frame too large"));
        }
        let output = decoder.get_mut();
        output.limit = size;
        output.bytes = Vec::with_capacity(size.min(1 << 20));
        decoder.write_all(&packet[4..])?;
        decoder.flush()?;
        let body = std::mem::take(&mut decoder.get_mut().bytes);
        if body.len() != size {
            return Err(invalid("compressed frame has the wrong size"));
        }
        Ok(body)
    }
}

/// A reader that marks its connection alive whenever bytes arrive.
struct Heard {
    inner: Box<dyn Read + Send>,
    liveness: Arc<Liveness>,
}

impl Read for Heard {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read > 0 {
            self.liveness.received();
        }
        Ok(read)
    }
}

/// When the peer was last heard from; a connection silent for `timeout` is
/// dead. Anything heard counts: whole messages, and with
/// [`FrameReader::with_liveness`] every byte that arrives. While this end's
/// reader is busy with a message ([`Liveness::busy`]) the silence is its own,
/// not the peer's: the connection is not dead then (for [`BUSY_GRACE`] at most).
pub struct Liveness {
    epoch: Instant,
    last_ms: AtomicU64,
    /// When this end's reader became busy (ms since `epoch`, at least 1; 0: not busy).
    busy_since_ms: AtomicU64,
    pub interval: Duration,
    pub timeout: Duration,
}

/// This end's reader is busy with a message ([`Liveness::busy`]); dropping it
/// counts as hearing the peer (what it sent meanwhile is read next).
pub struct Busy<'a> {
    liveness: &'a Liveness,
    previous: u64,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.liveness.busy_since_ms.store(self.previous, Ordering::Release);
        if self.previous == 0 {
            self.liveness.received();
        }
    }
}

impl Liveness {
    pub fn new(interval: Duration, timeout: Duration) -> Self {
        Self { epoch: Instant::now(), last_ms: AtomicU64::new(0), busy_since_ms: AtomicU64::new(0), interval, timeout }
    }

    /// Heartbeat every 2 s, dead after 6 s.
    pub fn standard() -> Self {
        Self::new(HEARTBEAT, DEAD_AFTER)
    }

    /// Anything arrived from the peer.
    pub fn received(&self) {
        self.last_ms.store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// The reader (one thread) is busy with a message until the guard drops:
    /// the peer's heartbeats wait unread meanwhile, which is not its silence.
    pub fn busy(&self) -> Busy<'_> {
        let now = (self.epoch.elapsed().as_millis() as u64).max(1);
        let previous = self.busy_since_ms.load(Ordering::Acquire);
        if previous == 0 {
            self.busy_since_ms.store(now, Ordering::Release);
        }
        Busy { liveness: self, previous }
    }

    /// How long the reader has been busy (`None`: it is not).
    pub fn busy_for(&self) -> Option<Duration> {
        match self.busy_since_ms.load(Ordering::Acquire) {
            0 => None,
            since => Some(self.epoch.elapsed().saturating_sub(Duration::from_millis(since))),
        }
    }

    pub fn silence(&self) -> Duration {
        self.epoch.elapsed().saturating_sub(Duration::from_millis(self.last_ms.load(Ordering::Relaxed)))
    }

    pub fn dead(&self) -> bool {
        if self.busy_for().is_some_and(|busy| busy < BUSY_GRACE) {
            return false;
        }
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
    fn readers_hold_unauthenticated_peers_to_the_small_limit() {
        let frame = |len: usize| {
            let mut bytes = (len as u32).to_le_bytes().to_vec();
            bytes.extend(std::iter::repeat_n(0u8, len));
            bytes
        };
        let read = |bytes: Vec<u8>, limit: usize| FrameReader::new(Box::new(io::Cursor::new(bytes))).with_limit(limit).recv_lenient::<ClientMsg>();
        assert!(read(frame(PRE_AUTH_FRAME), PRE_AUTH_FRAME).is_ok(), "a body at the limit is read");
        assert_eq!(read(frame(PRE_AUTH_FRAME + 1), PRE_AUTH_FRAME).unwrap_err().to_string(), "frame too large");
        assert!(read(frame(PRE_AUTH_FRAME + 1), MAX_FRAME).is_ok(), "after authentication the normal limit applies");
        // A claimed length is not allocated up front: the body is read as it arrives.
        let mut claimed = (MAX_FRAME as u32).to_le_bytes().to_vec();
        claimed.extend_from_slice(b"short");
        assert_eq!(read(claimed, MAX_FRAME).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        let mut reader = FrameReader::new(Box::new(io::Cursor::new(Vec::new())));
        reader.set_limit(usize::MAX);
        assert_eq!(reader.limit, MAX_FRAME, "never above MAX_FRAME");
    }

    #[test]
    fn a_compressed_frame_cannot_expand_past_its_announced_size() {
        // A block of 8 MiB of zeros compresses to a few hundred bytes.
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), LEVEL).unwrap();
        encoder.write_all(&vec![0u8; 8 << 20]).unwrap();
        encoder.flush().unwrap();
        let block = std::mem::take(encoder.get_mut());
        assert!(block.len() < 64 * 1024);
        for announced in [100u32, 1 << 20] {
            let mut packet = ((block.len() + 4) as u32 | COMPRESSED).to_le_bytes().to_vec();
            packet.extend_from_slice(&announced.to_le_bytes());
            packet.extend_from_slice(&block);
            let mut reader = FrameReader::new(Box::new(io::Cursor::new(packet)));
            reader.accept_compressed().unwrap();
            let error = reader.recv::<ServiceMsg>().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
            assert!(reader.zstd.as_mut().unwrap().get_mut().bytes.capacity() <= (1 << 20) + (256 << 10), "output stayed bounded");
        }
    }

    #[test]
    fn liveness_and_heartbeat_timing() {
        // Wide margins: sleeps overshoot on busy CI runners (macOS especially).
        let live = Liveness::new(Duration::from_millis(20), Duration::from_millis(1000));
        live.received();
        assert!(!live.dead());
        std::thread::sleep(Duration::from_millis(300));
        assert!(!live.dead());
        live.received();
        std::thread::sleep(Duration::from_millis(500));
        assert!(!live.dead(), "a message reset the silence");
        std::thread::sleep(Duration::from_millis(600));
        assert!(live.dead());
        let start = Instant::now();
        let mut beat = Beat::new(Duration::from_millis(200));
        assert!(!beat.due(start));
        assert!(beat.due(start + Duration::from_millis(260)));
        assert!(!beat.due(start + Duration::from_millis(300)));
        assert!(beat.next() > start + Duration::from_millis(400));
    }

    /// Hands out `chunk` bytes per read, `pause` apart (a slow link).
    struct Trickle {
        bytes: io::Cursor<Vec<u8>>,
        chunk: usize,
        pause: Duration,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(self.pause);
            let len = buf.len().min(self.chunk);
            self.bytes.read(&mut buf[..len])
        }
    }

    #[test]
    fn a_large_message_on_a_slow_link_keeps_its_connection_alive() {
        // About 1.2 s for one 1 MiB frame, with a 300 ms silence timeout.
        let sink = Shared::default();
        let body: String = (0..(1u32 << 20) / 8).map(|i| format!("{:08x}", i.wrapping_mul(2_654_435_761))).collect();
        FrameWriter::new(Box::new(sink.clone())).send(&ClientMsg::Rpc { id: 1, json: body }).unwrap();
        FrameWriter::new(Box::new(sink.clone())).send(&ClientMsg::Heartbeat).unwrap();
        let bytes = sink.0.lock().unwrap().clone();
        let run = |progress: bool| {
            let live = Arc::new(Liveness::new(Duration::from_millis(100), Duration::from_millis(300)));
            live.received();
            let trickle = Trickle { bytes: io::Cursor::new(bytes.clone()), chunk: 64 * 1024, pause: Duration::from_millis(70) };
            let mut reader = FrameReader::new(Box::new(trickle));
            if progress {
                reader.set_liveness(live.clone());
            }
            let (watch, done) = (live.clone(), Arc::new(std::sync::atomic::AtomicBool::new(false)));
            let stop = done.clone();
            let watcher = std::thread::spawn(move || {
                let mut died = false;
                while !stop.load(Ordering::Acquire) {
                    died |= watch.dead();
                    std::thread::sleep(Duration::from_millis(10));
                }
                died
            });
            // As the service's reader does: whole messages count too.
            while let Some(message) = reader.recv::<ClientMsg>().unwrap() {
                live.received();
                if message == ClientMsg::Heartbeat {
                    break;
                }
            }
            done.store(true, Ordering::Release);
            watcher.join().unwrap()
        };
        assert!(!run(true), "bytes arriving keep the connection alive");
        assert!(run(false), "counting whole messages only, the same transfer looks silent");
    }

    #[test]
    fn a_busy_reader_is_not_the_peers_silence() {
        let live = Liveness::new(Duration::from_millis(20), Duration::from_millis(150));
        live.received();
        {
            let _busy = live.busy();
            let _nested = live.busy();
            std::thread::sleep(Duration::from_millis(400));
            assert!(live.silence() >= Duration::from_millis(400));
            assert!(!live.dead(), "busy: not dead");
            assert!(live.busy_for().is_some());
        }
        assert!(live.busy_for().is_none());
        assert!(!live.dead(), "the end of the busy spell counts as hearing the peer");
        std::thread::sleep(Duration::from_millis(400));
        assert!(live.dead(), "then the usual timeout applies");
    }
}
