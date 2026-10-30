//! The byte stream between the app's WebSocket and the protocol: the app
//! pushes what it receives into an [`Inbox`] (read by the protocol's reader
//! thread) and sends what [`TransportWriter`] writes. One WebSocket message
//! per protocol frame on the way out.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The app's side of a connection (a `URLSessionWebSocketTask`).
#[uniffi::export(with_foreign)]
pub trait Transport: Send + Sync {
    /// Sends one binary WebSocket message.
    fn send(&self, data: Vec<u8>);
    /// Closes the connection (the app then reports `transport_closed`).
    fn close(&self);
}

#[derive(Default)]
struct InboxState {
    bytes: VecDeque<u8>,
    closed: bool,
    deadline: Option<Instant>,
}

/// Bytes received and not read yet.
#[derive(Default)]
pub struct Inbox {
    state: Mutex<InboxState>,
    ready: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Inbox {
    pub fn push(&self, data: &[u8]) {
        let mut state = lock(&self.state);
        if !state.closed {
            state.bytes.extend(data);
        }
        self.ready.notify_all();
    }

    /// The connection ended: readers get the rest, then the end of the stream.
    pub fn close(&self) {
        lock(&self.state).closed = true;
        self.ready.notify_all();
    }

    pub fn closed(&self) -> bool {
        lock(&self.state).closed
    }

    /// Reads fail with `TimedOut` once `within` passed (`None`: no deadline).
    pub fn set_deadline(&self, within: Option<Duration>) {
        lock(&self.state).deadline = within.map(|d| Instant::now() + d);
        self.ready.notify_all();
    }
}

/// Reads an [`Inbox`], blocking until bytes arrive or it closes.
pub struct InboxReader(pub Arc<Inbox>);

impl Read for InboxReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut state = lock(&self.0.state);
        loop {
            if !state.bytes.is_empty() {
                let n = buf.len().min(state.bytes.len());
                for (slot, byte) in buf.iter_mut().zip(state.bytes.drain(..n)) {
                    *slot = byte;
                }
                return Ok(n);
            }
            if state.closed {
                return Ok(0);
            }
            let wait = match state.deadline {
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "the service did not answer in time"));
                    }
                    left.min(Duration::from_secs(1))
                }
                None => Duration::from_secs(1),
            };
            state = self.0.ready.wait_timeout(state, wait).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// Writes through the app's transport: every `write` is one message.
pub struct TransportWriter(pub Arc<dyn Transport>);

impl Write for TransportWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !buf.is_empty() {
            self.0.send(buf.to_vec());
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_get_pushed_bytes_then_the_end() {
        let inbox = Arc::new(Inbox::default());
        let mut reader = InboxReader(inbox.clone());
        let writer = inbox.clone();
        let feeder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            writer.push(b"hello ");
            writer.push(b"world");
            writer.close();
        });
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        feeder.join().unwrap();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn a_deadline_ends_a_silent_peer() {
        let inbox = Arc::new(Inbox::default());
        inbox.set_deadline(Some(Duration::from_millis(50)));
        let error = InboxReader(inbox).read(&mut [0u8; 4]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
