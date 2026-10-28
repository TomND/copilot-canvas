//! The golden corpus as part of `cargo test --workspace`: every entry must
//! match its expected screens on every engine of the build, and no engine may
//! panic on mutated or re-chunked corpus streams.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use canvas_common::protocol::{Input, Key};
use canvas_corpus::replay::new_emulator;
use canvas_corpus::store::{Corpus, Outcome, repo_dir};
use canvas_corpus::vtrec::Kind;
use canvas_corpus::{available_engines, synth};
use canvas_host::emulator::{Emulator, Engine, Scroll};

fn corpus() -> Corpus {
    Corpus::locate(Some(&repo_dir())).expect("the repository's corpus folder")
}

#[test]
fn every_entry_matches_every_available_engine() {
    let corpus = corpus();
    let names = corpus.names().unwrap();
    assert!(names.len() >= 10, "corpus entries: {names:?}");
    let mut failures = Vec::new();
    for name in &names {
        for engine in available_engines() {
            match corpus.check(name, engine).unwrap() {
                Outcome::Match { .. } => {}
                Outcome::Mismatch(diff) => failures.push(format!("{name} [{}]:\n    {}", engine.name(), diff.join("\n    "))),
                Outcome::Missing => failures.push(format!("{name} [{}]: no expected.{}.json", engine.name(), engine.name())),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "golden corpus differs; review, then `copilot-canvas corpus bless` (corpus/README.md):\n{}",
        failures.join("\n")
    );
}

#[test]
fn entries_are_consistent() {
    let corpus = corpus();
    for name in corpus.names().unwrap() {
        let meta = corpus.meta(&name).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        let recording = corpus.recording(&name).unwrap();
        assert!(!meta.description.is_empty(), "{name}: description");
        assert!(["recorded", "session", "synthetic"].contains(&meta.source.as_str()), "{name}: source {:?}", meta.source);
        assert_eq!((meta.cols, meta.rows), (recording.header.cols, recording.header.rows), "{name}: meta size");
        assert!(recording.chunks() > 0, "{name}: no output");
        if meta.source == "recorded" {
            assert!(corpus.entry_dir(&name).join("input.script").is_file() || meta.notes.contains("no input"), "{name}: input.script");
        }
    }
}

#[test]
fn synthetic_streams_match_the_generator() {
    let corpus = corpus();
    for entry in synth::all() {
        let stored = corpus.recording(entry.name).unwrap_or_else(|e| panic!("{}: {e:#} (run `copilot-canvas corpus synth`)", entry.name));
        let generated = entry.recording();
        assert!(stored.events == generated.events && stored.header == generated.header, "{}: stored stream is stale; run `copilot-canvas corpus synth {0}` and bless", entry.name);
    }
}

/// splitmix64: deterministic and dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn range(&mut self, low: u16, high: u16) -> u16 {
        low + self.below((high - low + 1) as usize) as u16
    }
}

fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

/// Hostile fragments spliced into streams.
const FRAGMENTS: &[&[u8]] = &[
    b"\x1b",
    b"\x1b[",
    b"\x1b]",
    b"\x1bP",
    b"\x1b\\",
    b"\x07",
    b"\x18",
    b"\x1a",
    b"\x1b[?1049h",
    b"\x1b[?1049l",
    b"\x1b[?47h",
    b"\x1b[?1048h",
    b"\x1b[65535;65535H",
    b"\x1b[4294967296b",
    b"\x1b[99999999999m",
    b"\x1b[3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3;3m",
    b"\x1b[38:2::1:2:3m",
    b"\x1b[58;5;999m",
    b"\x1b]8;;",
    b"\x1b]8;id=x;https://x\x07",
    b"\x1b]52;c;!!!\x07",
    b"\x1b]52;c;?\x07",
    b"\x1b]9;4;9;999\x07",
    b"\x1b]777;notify\x07",
    b"\x1b]0;\xff\xfe\x07",
    b"\x1b[?2026h",
    b"\x1b[?2026l",
    b"\x1b[>1u",
    b"\x1b[>31u",
    b"\x1b[<99u",
    b"\x1b[=31;3u",
    b"\x1b[?u",
    b"\x1b[5 q",
    b"\x1b[99 q",
    b"\x1b[r",
    b"\x1b[2;1r",
    b"\x1b[1;65535r",
    b"\x1b#8",
    b"\x1b[1000@",
    b"\x1b[1000P",
    b"\x1b[1000L",
    b"\x1b[1000M",
    b"\x1b[1000X",
    b"\x1b[999S",
    b"\x1b[999T",
    b"\x1bM\x1bM\x1bM",
    b"\x1b[6n",
    b"\x1b[c",
    b"\x1b[14t",
    b"\x1b[22;0t\x1b[23;0t",
    b"\x1bc",
    b"\x1b[!p",
    b"\x1b[?6h",
    b"\x1b[?7l",
    b"\x1b[4h",
    b"\x1b(0\x0e\x0f",
    b"\t\t\t\t\t\t\t\t\t\t\t\t\t",
    b"\x08\x08\x08\x08",
    b"\xe4\xb8",
    b"\xf0\x9f\x91\xa9\xe2\x80\x8d",
    b"\xcc\x81\xcc\x81\xcc\x81",
    b"\xef\xb8\x8f",
    b"\xe4\xb8\xad\xe4\xb8\xad\xe4\xb8\xad\xe4\xb8\xad",
    b"\r\n",
];

const KEYS: [Key; 12] = [Key::Enter, Key::Tab, Key::BackTab, Key::Backspace, Key::Esc, Key::Up, Key::Left, Key::Home, Key::PageDown, Key::Delete, Key::F(5), Key::Char('x')];

fn mutate(rng: &mut Rng, mut bytes: Vec<u8>) -> Vec<u8> {
    for _ in 0..1 + rng.below(24) {
        let at = rng.below(bytes.len() + 1);
        match rng.below(5) {
            0 if !bytes.is_empty() => {
                let i = rng.below(bytes.len());
                bytes[i] ^= 1 << rng.below(8);
            }
            1 if !bytes.is_empty() => {
                let end = (at + 1 + rng.below(64)).min(bytes.len());
                bytes.drain(at.min(end)..end);
            }
            2 if !bytes.is_empty() => {
                let start = rng.below(bytes.len());
                let end = (start + 1 + rng.below(256)).min(bytes.len());
                let copy = bytes[start..end].to_vec();
                bytes.splice(at..at, copy);
            }
            _ => {
                let fragment = FRAGMENTS[rng.below(FRAGMENTS.len())];
                bytes.splice(at..at, fragment.iter().copied());
            }
        }
    }
    bytes
}

fn noise(rng: &mut Rng, len: usize) -> Vec<u8> {
    const ALPHABET: &[u8] = b"\x1b\x1b\x1b[[[]]P\\;;:?>=<!$ 0123456789mHJKhlrstuABCDqbcnpGPXLM@#87\x07\x08\t\n\r\x18azAZ";
    (0..len)
        .map(|_| match rng.below(8) {
            0 => rng.next() as u8,
            _ => ALPHABET[rng.below(ALPHABET.len())],
        })
        .collect()
}

/// Feeds `bytes` in random chunks with random resizes, scrolls, snapshots and input encodings.
fn exercise(engine: Engine, rng: &mut Rng, bytes: &[u8]) {
    let mut emu: Box<dyn Emulator> = new_emulator(engine, rng.range(20, 160), rng.range(5, 50)).unwrap();
    let mut events = canvas_corpus::replay::EventLog::default();
    let mut rest = bytes;
    while !rest.is_empty() {
        let size = match rng.below(4) {
            0 => 1 + rng.below(8),
            1 => 1 + rng.below(64),
            2 => 1 + rng.below(1024),
            _ => 1 + rng.below(16 * 1024),
        };
        let (chunk, tail) = rest.split_at(size.min(rest.len()));
        rest = tail;
        emu.feed(chunk, &mut events);
        match rng.below(48) {
            0 => emu.resize(rng.range(20, 200), rng.range(5, 60)),
            1 => emu.scroll(Scroll::Delta(rng.below(40) as i32 - 20)),
            2 => emu.scroll(if rng.below(2) == 0 { Scroll::Top } else { Scroll::Bottom }),
            3 => {
                let snapshot = emu.snapshot();
                assert_eq!(snapshot.lines.len(), snapshot.rows as usize);
            }
            4 => {
                let key = KEYS[rng.below(KEYS.len())];
                let _ = emu.encode(&Input::Key { key, mods: rng.below(8) as u8 });
                let _ = emu.encode(&Input::Paste("a\r\nb\x1b[201~c".into()));
            }
            5 => {
                let _ = emu.encode_wheel(rng.below(2) == 0, rng.range(0, 300), rng.range(0, 300), rng.below(8) as u8);
                let _ = emu.encode_click(rng.below(3) as u8, rng.range(0, 300), rng.range(0, 300), rng.below(8) as u8);
            }
            6 if emu.sync_deadline().is_some() => emu.end_sync(&mut events),
            _ => {}
        }
    }
    if emu.sync_deadline().is_some() {
        emu.end_sync(&mut events);
    }
    let _ = emu.snapshot();
}

#[test]
fn engines_survive_mutated_streams() {
    let corpus = corpus();
    let streams: Vec<(String, Vec<u8>)> = corpus
        .names()
        .unwrap()
        .into_iter()
        .map(|name| {
            let recording = corpus.recording(&name).unwrap();
            assert!(recording.events.iter().any(|e| matches!(e.kind, Kind::Output(_))));
            (name, recording.output())
        })
        .collect();
    let budget = Duration::from_secs(std::env::var("CORPUS_FUZZ_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(10));
    let rounds: u64 = std::env::var("CORPUS_FUZZ_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(48);
    let mut failures = Vec::new();
    let mut runs = 0usize;
    for engine in available_engines() {
        let started = Instant::now();
        'rounds: for round in 0..rounds {
            for (name, bytes) in &streams {
                if round > 0 && started.elapsed() > budget {
                    break 'rounds;
                }
                let seed = fnv(name) ^ round.wrapping_mul(0x2545_f491_4f6c_dd1d);
                let mut rng = Rng(seed);
                let input = if round % 3 == 2 { noise(&mut rng, 8 * 1024) } else { mutate(&mut rng, bytes.clone()) };
                runs += 1;
                if catch_unwind(AssertUnwindSafe(|| exercise(engine, &mut rng, &input))).is_err() {
                    failures.push(format!("{} panicked on {name} round {round} (seed {seed:#x})", engine.name()));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{} of {runs} runs panicked:\n{}", failures.len(), failures.join("\n"));
}

/// Found by `engines_survive_mutated_streams`: alacritty_terminal 0.26 raises
/// `display_offset` when a scroll region that does not start at the top
/// scrolls (`Grid::scroll_up`), although no line enters the history. With the
/// view scrolled back, the adapter's snapshot then indexes past the scrollback:
/// a debug assertion, stale rows or an out-of-bounds panic in release. Fixed: the
/// alacritty adapter clamps the offset to the history after every feed.
#[test]
fn scrolled_view_survives_output_in_a_scroll_region() {
    for engine in available_engines() {
        let mut emu = new_emulator(engine, 40, 10).unwrap();
        let mut events = canvas_corpus::replay::EventLog::default();
        for i in 0..15 {
            emu.feed(format!("line {i}\r\n").as_bytes(), &mut events);
        }
        emu.scroll(Scroll::Delta(3));
        emu.feed(b"\x1b[2;5r\x1b[5;1H\n\n\n\n\n\n\n\n", &mut events);
        let snapshot = emu.snapshot();
        assert!(snapshot.display_offset <= snapshot.history, "{}: offset {} > history {}", engine.name(), snapshot.display_offset, snapshot.history);
    }
}
