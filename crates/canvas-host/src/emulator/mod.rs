//! The terminal engine boundary (L1). An [`Emulator`] parses a program's
//! output, keeps the screen and scrollback, reports its modes and encodes
//! keyboard, mouse and paste input the way the program asked for.
//!
//! Engines need not be `Send` (libghostty-vt is not): each hosted terminal
//! owns its emulator on one dedicated thread.

use std::time::Instant;

use canvas_common::protocol::{Input, Placement, Run, Run8, runs_v7};

pub mod alacritty;
#[cfg(feature = "ghostty")]
pub mod ghostty;
pub mod inline;

/// Engine-side happenings, delivered while feeding output.
pub trait Events {
    /// Bytes the terminal answers to the program (cursor reports, colour queries).
    fn reply(&mut self, bytes: Vec<u8>);
    /// Window title (OSC 0/2); `None` resets it.
    fn title(&mut self, title: Option<String>);
    fn bell(&mut self);
    /// OSC 52 clipboard write.
    fn clipboard(&mut self, text: String);
    /// OSC 9;4 progress state: 0 none, 1 value, 2 error, 3 busy, 4 paused.
    fn progress(&mut self, state: u8);
    /// OSC 777 / OSC 9 desktop notification.
    fn notify(&mut self);
    /// The text of the notification `notify` reported (engines that know it).
    fn notification(&mut self, _title: String, _body: String) {}
}

/// Collects events for callers that handle them after feeding.
#[derive(Default, Debug, PartialEq)]
pub struct Collected {
    pub replies: Vec<Vec<u8>>,
    pub title: Option<Option<String>>,
    pub bells: u32,
    pub clipboard: Vec<String>,
    pub progress: Vec<u8>,
    pub notifications: u32,
    /// (title, body) of notifications with text.
    pub texts: Vec<(String, String)>,
}

impl Events for Collected {
    fn reply(&mut self, bytes: Vec<u8>) {
        self.replies.push(bytes);
    }
    fn title(&mut self, title: Option<String>) {
        self.title = Some(title);
    }
    fn bell(&mut self) {
        self.bells += 1;
    }
    fn clipboard(&mut self, text: String) {
        self.clipboard.push(text);
    }
    fn progress(&mut self, state: u8) {
        self.progress.push(state);
    }
    fn notify(&mut self) {
        self.notifications += 1;
    }
    fn notification(&mut self, title: String, body: String) {
        self.texts.push((title, body));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// Lines towards history (positive) or the live screen (negative).
    Delta(i32),
    Top,
    Bottom,
}

/// The visible viewport.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Vec<Run8>>,
    /// Hyperlink targets referenced by `Run8::link`.
    pub links: Vec<String>,
    /// Shown only on the live screen and when the program shows it.
    pub cursor: Option<(u16, u16)>,
    pub cursor_shape: u8,
    pub cursor_blink: bool,
    pub display_offset: u32,
    pub history: u32,
    pub modes: u32,
    /// Kitty graphics placements in the viewport (drawing order not implied).
    pub images: Vec<SnapshotImage>,
}

/// One image placement of a snapshot and the generation of its image's pixels.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SnapshotImage {
    pub placement: Placement,
    /// Changes whenever the image's pixels change (never reused).
    pub generation: u64,
}

/// An image's pixels as straight RGBA rows.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageData {
    pub width: u32,
    pub height: u32,
    pub generation: u64,
    pub rgba: Vec<u8>,
}

impl Snapshot {
    /// One row in protocol-7 shape.
    pub fn row_v7(&self, y: usize) -> Vec<Run> {
        self.lines.get(y).map(|row| runs_v7(row, &self.links)).unwrap_or_default()
    }

    /// Plain text of the viewport (rows joined by `\n`, cells at their columns).
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|row| {
                let mut line = String::new();
                let mut col = 0u16;
                for run in row {
                    if run.col > col {
                        line.push_str(&" ".repeat((run.col - col) as usize));
                    }
                    line.push_str(&run.text);
                    col = run.col + run.width();
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Set once Ghostty failed to start: later terminals use alacritty.
static GHOSTTY_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Which engine a terminal runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Alacritty,
    Ghostty,
}

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Engine::Alacritty => "alacritty",
            Engine::Ghostty => "ghostty",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "alacritty" => Some(Engine::Alacritty),
            "ghostty" => Some(Engine::Ghostty),
            _ => None,
        }
    }

    /// Whether this build includes the engine and it has not failed to start.
    pub fn available(self) -> bool {
        match self {
            Engine::Alacritty => true,
            Engine::Ghostty => cfg!(feature = "ghostty") && !GHOSTTY_FAILED.load(std::sync::atomic::Ordering::Acquire),
        }
    }

    /// Ghostty when this build includes it, else alacritty.
    pub fn default_engine() -> Self {
        if Engine::Ghostty.available() { Engine::Ghostty } else { Engine::Alacritty }
    }
}

pub struct Config {
    pub cols: u16,
    pub rows: u16,
    pub scrollback: usize,
    /// Answer kitty keyboard protocol queries (Copilot uses it).
    pub kitty_keyboard: bool,
}

/// A terminal engine.
pub trait Emulator {
    fn engine(&self) -> Engine;
    /// Parses program output.
    fn feed(&mut self, bytes: &[u8], events: &mut dyn Events);
    fn resize(&mut self, cols: u16, rows: u16);
    fn snapshot(&self) -> Snapshot;
    fn scroll(&mut self, scroll: Scroll);
    fn display_offset(&self) -> u32;
    /// `MODE_*` bits.
    fn modes(&self) -> u32;
    /// Keys, paste and focus as the program expects them.
    fn encode(&mut self, input: &Input) -> Vec<u8>;
    /// A wheel notch, or `None` when the wheel should scroll the scrollback.
    fn encode_wheel(&mut self, up: bool, col: u16, row: u16, mods: u8) -> Option<Vec<u8>>;
    /// Press + release, or `None` when the program did not ask for mouse reports.
    fn encode_click(&mut self, button: u8, col: u16, row: u16, mods: u8) -> Option<Vec<u8>>;
    /// End of a synchronized update (DEC 2026) whose end marker may never come.
    fn sync_deadline(&self) -> Option<Instant>;
    fn end_sync(&mut self, events: &mut dyn Events);
    /// The pixels of a Kitty graphics image (engines with image support).
    fn image(&mut self, _id: u32) -> Option<ImageData> {
        None
    }
}

/// Creates an engine. `Ghostty` falls back to alacritty when this build has
/// no Ghostty engine or it fails to start.
pub fn create(engine: Engine, config: &Config) -> Box<dyn Emulator> {
    #[cfg(feature = "ghostty")]
    if engine == Engine::Ghostty {
        match ghostty::GhosttyEmulator::new(config) {
            Ok(emulator) => return Box::new(emulator),
            Err(error) => {
                GHOSTTY_FAILED.store(true, std::sync::atomic::Ordering::Release);
                eprintln!("ghostty engine unavailable ({error}); using alacritty");
            }
        }
    }
    let _ = engine;
    Box::new(alacritty::AlacrittyEmulator::new(config))
}

/// Feeds bytes and returns the replies and events (tests, the self-test, the corpus).
pub fn feed_collect(emulator: &mut dyn Emulator, bytes: &[u8]) -> Collected {
    let mut events = Collected::default();
    emulator.feed(bytes, &mut events);
    events
}

/// RGBA rows as a PNG file (fast compression: images are sent as they arrive).
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(rgba).ok()?;
    writer.finish().ok()?;
    Some(out)
}
