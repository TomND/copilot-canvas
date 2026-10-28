//! Inline images through ConPTY. Windows' pseudoconsole drops APC sequences,
//! so Kitty graphics commands (`ESC _ G … ESC \`) written by a program never
//! reach the engine, while OSC sequences pass through. [`InlineImages`] turns
//! iTerm2's inline image protocol (`OSC 1337 ; File=inline=1;… : <base64>`
//! ending in BEL or ST), which travels as an OSC, into the equivalent Kitty
//! graphics command before the bytes reach the engine. The cursor stays where
//! it was (`C=1`): ConPTY does not know the image's size, so its cursor is
//! the one that counts, and programs leave room below the image themselves.
//! Only PNG files are translated (the engine decodes PNG); other formats,
//! downloads (`inline=0`) and bodies that are not strictly base64 are
//! dropped. Sequences longer than [`MAX_SEQUENCE`] are dropped as they
//! stream. A sequence split across reads is held and only the new bytes are
//! appended and searched each time, so a long image costs linear time.

use std::borrow::Cow;

use canvas_common::protocol::HOST_CELL_PX;

const PREFIX: &[u8] = b"\x1b]1337;File=";
/// The longest inline image sequence kept (base64 of a 24 MiB file).
pub const MAX_SEQUENCE: usize = 32 << 20;
/// Base64 bytes per Kitty graphics chunk (the protocol's limit).
const CHUNK: usize = 4096;
const ESC: u8 = 0x1b;

#[derive(Default)]
pub struct InlineImages {
    /// The start of a sequence (or of a possible prefix) not yet complete.
    held: Vec<u8>,
    /// Dropping an oversized sequence until its terminator.
    skipping: bool,
    /// While skipping, the last byte seen was ESC (`ESC \` may span two reads).
    skip_esc: bool,
    cols: u16,
    rows: u16,
}

impl InlineImages {
    /// The terminal size, for widths and heights given in percent.
    pub fn set_size(&mut self, cols: u16, rows: u16) {
        (self.cols, self.rows) = (cols, rows);
    }

    /// `bytes` with every complete inline image sequence replaced by a Kitty
    /// graphics command; incomplete ones are held for the next call.
    pub fn filter<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        if self.held.is_empty() && !self.skipping && !bytes.windows(6).any(|w| w == &PREFIX[..6]) && !ends_with_partial(bytes) {
            return Cow::Borrowed(bytes);
        }
        let mut out = Vec::with_capacity(bytes.len());
        let mut rest = bytes;
        if self.skipping {
            match self.skip(rest) {
                Some(end) => rest = &rest[end..],
                None => return Cow::Owned(out),
            }
        }
        if !self.held.is_empty() {
            if self.held.len() < PREFIX.len() {
                // A possible prefix (a few bytes): scan it again with the new bytes.
                let mut joined = std::mem::take(&mut self.held);
                joined.extend_from_slice(rest);
                self.scan(&joined, &mut out);
                return Cow::Owned(out);
            }
            match self.extend_held(rest, &mut out) {
                Some(end) => rest = &rest[end..],
                None => return Cow::Owned(out),
            }
        }
        self.scan(rest, &mut out);
        Cow::Owned(out)
    }

    /// Skips the rest of an oversized sequence; the index after its end, or
    /// `None` when `data` ends inside it.
    fn skip(&mut self, data: &[u8]) -> Option<usize> {
        if data.is_empty() {
            return None;
        }
        if std::mem::take(&mut self.skip_esc) && data[0] == b'\\' {
            self.skipping = false;
            return Some(1);
        }
        match terminator(data, 0) {
            Some((_, end)) => {
                self.skipping = false;
                Some(end)
            }
            None => {
                self.skip_esc = data.last() == Some(&ESC);
                None
            }
        }
    }

    /// Continues the held sequence with `data`: searches only the new bytes
    /// for its end. Returns the index after the end, or `None` when `data`
    /// ends inside it (then it is appended, or the sequence is skipped once
    /// it grows past [`MAX_SEQUENCE`]).
    fn extend_held(&mut self, data: &[u8], out: &mut Vec<u8>) -> Option<usize> {
        if data.is_empty() {
            return None;
        }
        // `ESC \` split across reads: the held ESC starts the terminator.
        let split = self.held.last() == Some(&ESC) && data[0] == b'\\';
        let found = if split { Some((0, 1)) } else { terminator(data, 0) };
        let Some((body_end, end)) = found else {
            if self.held.len() + data.len() > MAX_SEQUENCE {
                self.held = Vec::new();
                self.skipping = true;
                self.skip_esc = data.last() == Some(&ESC);
            } else {
                self.held.extend_from_slice(data);
            }
            return None;
        };
        let mut sequence = std::mem::take(&mut self.held);
        if split {
            sequence.pop();
        } else {
            sequence.extend_from_slice(&data[..body_end]);
        }
        if sequence.len() <= PREFIX.len() + MAX_SEQUENCE
            && let Some(kitty) = self.translate(&sequence[PREFIX.len()..])
        {
            out.extend_from_slice(&kitty);
        }
        Some(end)
    }

    /// Copies `data` to `out`, translating complete sequences and holding (or
    /// skipping) an incomplete one at the end.
    fn scan(&mut self, data: &[u8], out: &mut Vec<u8>) {
        let mut i = 0;
        while i < data.len() {
            let Some(offset) = data[i..].iter().position(|&b| b == ESC) else {
                out.extend_from_slice(&data[i..]);
                return;
            };
            let start = i + offset;
            out.extend_from_slice(&data[i..start]);
            let rest = &data[start..];
            if rest.len() < PREFIX.len() {
                if PREFIX.starts_with(rest) {
                    self.held = rest.to_vec();
                    return;
                }
                out.push(ESC);
                i = start + 1;
                continue;
            }
            if !rest.starts_with(PREFIX) {
                out.push(ESC);
                i = start + 1;
                continue;
            }
            match terminator(data, start + PREFIX.len()) {
                Some((body_end, end)) => {
                    if let Some(kitty) = self.translate(&data[start + PREFIX.len()..body_end]) {
                        out.extend_from_slice(&kitty);
                    }
                    i = end;
                }
                None if rest.len() > MAX_SEQUENCE => {
                    self.skipping = true;
                    self.skip_esc = data.last() == Some(&ESC);
                    return;
                }
                None => {
                    self.held = rest.to_vec();
                    return;
                }
            }
        }
    }

    /// The Kitty command for one `File=` body (`args:base64`); `None` unless
    /// the body is an inline PNG in strict base64 (letters, digits, `+/=`
    /// and whitespace only, so nothing else reaches the Kitty command).
    fn translate(&self, body: &[u8]) -> Option<Vec<u8>> {
        let colon = body.iter().position(|&b| b == b':')?;
        let args = std::str::from_utf8(&body[..colon]).ok()?;
        let payload = &body[colon + 1..];
        if !payload.iter().all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=') || b.is_ascii_whitespace()) {
            return None;
        }
        let data: Vec<u8> = payload.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
        let (mut inline, mut width, mut height, mut keep_aspect) = (false, None, None, true);
        for arg in args.split(';') {
            let (key, value) = arg.split_once('=').unwrap_or((arg, ""));
            match key {
                "inline" => inline = value == "1",
                "width" => width = cells(value, HOST_CELL_PX.0, self.cols),
                "height" => height = cells(value, HOST_CELL_PX.1, self.rows),
                "preserveAspectRatio" => keep_aspect = value != "0",
                _ => {}
            }
        }
        // PNG files only: base64 of the PNG signature.
        if !inline || !data.starts_with(b"iVBORw0KGgo") {
            return None;
        }
        if keep_aspect && width.is_some() && height.is_some() {
            height = None;
        }
        let mut control = String::from("a=T,f=100,q=2,C=1");
        if let Some(c) = width {
            control.push_str(&format!(",c={c}"));
        }
        if let Some(r) = height {
            control.push_str(&format!(",r={r}"));
        }
        let chunks: Vec<&[u8]> = data.chunks(CHUNK).collect();
        let mut out = Vec::with_capacity(data.len() + chunks.len() * 16 + control.len());
        for (index, chunk) in chunks.iter().enumerate() {
            let more = u8::from(index + 1 < chunks.len());
            out.extend_from_slice(b"\x1b_G");
            if index == 0 {
                out.extend_from_slice(control.as_bytes());
                out.extend_from_slice(format!(",m={more};").as_bytes());
            } else {
                out.extend_from_slice(format!("m={more};").as_bytes());
            }
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\x1b\\");
        }
        Some(out)
    }
}

/// The end of an OSC body from `from`: (body end, index after the terminator).
fn terminator(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i < data.len() {
        match data[i] {
            0x07 => return Some((i, i + 1)),
            0x1b if data.get(i + 1) == Some(&b'\\') => return Some((i, i + 2)),
            0x1b if i + 1 == data.len() => return None,
            0x18 | 0x1a => return Some((i, i + 1)),
            _ => i += 1,
        }
    }
    None
}

/// The chunk ends in what could be the start of [`PREFIX`].
fn ends_with_partial(bytes: &[u8]) -> bool {
    let tail = &bytes[bytes.len().saturating_sub(PREFIX.len() - 1)..];
    tail.iter().position(|&b| b == 0x1b).is_some_and(|p| PREFIX.starts_with(&tail[p..]))
}

/// iTerm2 size (`N` cells, `Npx`, `N%`, `auto`) in cells.
fn cells(value: &str, cell_px: u32, total: u16) -> Option<u32> {
    let value = value.trim();
    let cells = if let Some(px) = value.strip_suffix("px") {
        px.parse::<u32>().ok()?.div_ceil(cell_px)
    } else if let Some(percent) = value.strip_suffix('%') {
        (percent.parse::<u32>().ok()? * total.max(1) as u32).div_ceil(100)
    } else {
        value.parse::<u32>().ok()?
    };
    (cells > 0).then_some(cells.min(1000))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUg";

    fn sequence(args: &str) -> String {
        format!("\x1b]1337;File={args}:{PNG}\x07")
    }

    #[test]
    fn inline_pngs_become_kitty_commands_and_other_bytes_pass() {
        let mut bridge = InlineImages::default();
        bridge.set_size(80, 24);
        let input = format!("a\x1b[31mb{}c", sequence("inline=1;width=8;height=2;preserveAspectRatio=0"));
        let out = bridge.filter(input.as_bytes()).into_owned();
        assert_eq!(String::from_utf8(out).unwrap(), format!("a\x1b[31mb\x1b_Ga=T,f=100,q=2,C=1,c=8,r=2,m=0;{PNG}\x1b\\c"));
        assert!(matches!(bridge.filter(b"plain \x1b[0m text"), Cow::Borrowed(_)), "no copy without images");
    }

    #[test]
    fn sequences_split_across_reads_are_joined() {
        let mut bridge = InlineImages::default();
        let full = sequence("inline=1;width=100px");
        let bytes = full.as_bytes();
        let mut out = Vec::new();
        for piece in [&bytes[..3], &bytes[3..15], &bytes[15..bytes.len() - 1], &bytes[bytes.len() - 1..]] {
            out.extend_from_slice(&bridge.filter(piece));
        }
        assert_eq!(String::from_utf8(out).unwrap(), format!("\x1b_Ga=T,f=100,q=2,C=1,c=13,m=0;{PNG}\x1b\\"));
    }

    #[test]
    fn downloads_other_formats_and_oversized_sequences_are_dropped() {
        let mut bridge = InlineImages::default();
        assert_eq!(&*bridge.filter(format!("x{}y", sequence("inline=0")).as_bytes()), b"xy");
        assert_eq!(&*bridge.filter(b"\x1b]1337;File=inline=1:R0lGODlh\x1b\\z"), b"z", "a GIF");
        assert_eq!(&*bridge.filter(b"\x1b]1337;SetMark\x07"), b"\x1b]1337;SetMark\x07", "other OSC 1337 commands pass");
        let mut big = b"\x1b]1337;File=inline=1:".to_vec();
        big.extend(std::iter::repeat_n(b'A', MAX_SEQUENCE + 10));
        assert!(bridge.filter(&big).is_empty());
        assert_eq!(&*bridge.filter(b"AAAA\x07after"), b"after", "the rest of an oversized sequence is skipped");
    }

    #[test]
    fn long_images_are_sent_in_chunks() {
        let mut bridge = InlineImages::default();
        let data = format!("{PNG}{}", "A".repeat(CHUNK * 2));
        let out = String::from_utf8(bridge.filter(format!("\x1b]1337;File=inline=1:{data}\x1b\\").as_bytes()).into_owned()).unwrap();
        assert_eq!(out.matches("\x1b_G").count(), 3);
        assert!(out.starts_with("\x1b_Ga=T,f=100,q=2,C=1,m=1;") && out.contains("\x1b_Gm=1;") && out.contains("\x1b_Gm=0;"));
    }

    #[test]
    fn a_long_sequence_in_small_reads_takes_linear_time() {
        let mut stream = format!("lead\x1b]1337;File=inline=1:{PNG}").into_bytes();
        stream.extend(std::iter::repeat_n(b'A', 4 << 20));
        stream.extend_from_slice(b"\x1b\\after");
        let mut bridge = InlineImages::default();
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        for chunk in stream.chunks(4096) {
            out.extend_from_slice(&bridge.filter(chunk));
        }
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(2), "4 MiB in 4 KiB reads took {elapsed:?}");
        assert!(out.starts_with(b"lead\x1b_Ga=T,f=100,q=2,C=1,m=1;") && out.ends_with(b"\x1b\\after"), "translated");
        assert_eq!(out.windows(3).filter(|w| w == b"\x1b_G").count(), ((4 << 20) + PNG.len()).div_ceil(CHUNK));
        assert!(bridge.held.is_empty() && bridge.held.capacity() == 0, "nothing is held afterwards");
    }

    #[test]
    fn the_string_terminator_may_be_split_across_reads() {
        let mut bridge = InlineImages::default();
        let mut out = bridge.filter(format!("\x1b]1337;File=inline=1:{PNG}\x1b").as_bytes()).into_owned();
        out.extend_from_slice(&bridge.filter(b"\\tail"));
        assert_eq!(String::from_utf8(out).unwrap(), format!("\x1b_Ga=T,f=100,q=2,C=1,m=0;{PNG}\x1b\\tail"));
        // While skipping an oversized sequence too.
        let mut big = b"\x1b]1337;File=inline=1:".to_vec();
        big.extend(std::iter::repeat_n(b'A', MAX_SEQUENCE + 10));
        big.push(0x1b);
        assert!(bridge.filter(&big).is_empty());
        assert_eq!(&*bridge.filter(b"\\after"), b"after", "the ESC that ended the last read began the terminator");
    }

    #[test]
    fn only_strict_base64_bodies_are_translated() {
        let mut bridge = InlineImages::default();
        let with = |payload: &str| format!("\x1b]1337;File=inline=1:{payload}\x07z");
        assert_eq!(&*bridge.filter(with(&format!("{PNG}\x1b[31mAAAA")).as_bytes()), b"z", "an escape sequence inside the body");
        assert_eq!(&*bridge.filter(with(&format!("{PNG};a=d")).as_bytes()), b"z", "Kitty control characters");
        assert_eq!(&*bridge.filter(with(&format!("{PNG}%%")).as_bytes()), b"z");
        let wrapped = String::from_utf8(bridge.filter(with("iVBORw0K\r\n  GgoAAAA==").as_bytes()).into_owned()).unwrap();
        assert_eq!(wrapped, "\x1b_Ga=T,f=100,q=2,C=1,m=0;iVBORw0KGgoAAAA==\x1b\\z", "line breaks and padding are fine");
    }
}
