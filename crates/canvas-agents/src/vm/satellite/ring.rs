//! A terminal's recent raw output with absolute offsets: what the satellite
//! replays when the hub reconnects (`SatDown::Resync { have }`).

use std::collections::VecDeque;

pub struct Ring {
    /// The output offset of `bytes[0]`.
    start: u64,
    bytes: VecDeque<u8>,
    cap: usize,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Self { start: 0, bytes: VecDeque::new(), cap: cap.max(1) }
    }

    /// Appends output; the oldest bytes go once it holds more than its cap.
    pub fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        let over = self.bytes.len().saturating_sub(self.cap);
        if over > 0 {
            self.bytes.drain(..over);
            self.start += over as u64;
        }
    }

    /// The most it holds.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// The offset after the last byte (all the output so far).
    pub fn end(&self) -> u64 {
        self.start + self.bytes.len() as u64
    }

    /// The output from offset `have` on; `None` when the ring no longer
    /// reaches back to it (or `have` is beyond the end).
    pub fn since(&self, have: u64) -> Option<Vec<u8>> {
        if have < self.start || have > self.end() {
            return None;
        }
        Some(self.bytes.range((have - self.start) as usize..).copied().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_newest_bytes_with_their_offsets() {
        let mut ring = Ring::new(8);
        ring.push(b"hello");
        assert_eq!((ring.end(), ring.since(0).unwrap()), (5, b"hello".to_vec()));
        assert_eq!(ring.since(5).unwrap(), b"");
        assert_eq!(ring.since(6), None, "beyond the end");
        ring.push(b" world");
        assert_eq!(ring.end(), 11);
        assert_eq!(ring.since(3).unwrap(), b"lo world");
        assert_eq!(ring.since(2), None, "no longer held");
        assert_eq!(ring.since(9).unwrap(), b"ld");
    }
}
