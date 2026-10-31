//! Fonts, the cell grid's metrics and a cache of shaped text.
//!
//! The grid font is Cascadia Mono (Consolas when missing on Windows, SF Mono
//! or Menlo on macOS), sized so one advance is a whole number of physical
//! pixels: text runs then stay on the cell grid. Canvas labels use the
//! system's UI font (Segoe UI, SF). Emoji, CJK and symbols come from fallback
//! fonts that cosmic-text picks per character. Which files are loaded is the
//! OS module's choice (`crate::os::fonts`).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use glyphon::cosmic_text::{Ellipsize, EllipsizeHeightLimit};
use glyphon::{Attrs, Buffer, Family, FontSystem, Metrics as TextMetrics, Shaping, Style, Weight, Wrap};

/// Logical font size of the cell grid.
pub const BASE_FONT_PX: f32 = 14.0;
/// Cell height per px of font size.
const LINE: f32 = 1.3;
pub struct Fonts {
    pub system: FontSystem,
    /// The grid font's family name.
    pub mono: String,
    /// The canvas labels' family name.
    pub ui: String,
    /// Its advance per px of font size.
    pub advance: f32,
}

/// Loads the fonts (run on a thread at start, while the window and GPU come up).
pub fn load() -> Fonts {
    let mut db = glyphon::fontdb::Database::new();
    let sources = crate::os::fonts::sources();
    for path in &sources.files {
        if path.is_file() {
            let _ = db.load_font_file(path);
        }
    }
    // Folders of fonts installed for this user only (Cascadia Mono often is).
    for dir in &sources.dirs {
        db.load_fonts_dir(dir);
    }
    let has = |db: &glyphon::fontdb::Database, name: &str| db.faces().any(|f| f.families.iter().any(|(n, _)| n == name));
    if !sources.mono.iter().any(|name| has(&db, name)) {
        db.load_system_fonts();
    }
    let mono = sources.mono.iter().copied().find(|name| has(&db, name)).unwrap_or("monospace").to_owned();
    let ui = sources.ui.iter().copied().find(|name| has(&db, name)).or(sources.ui.first().copied()).unwrap_or("sans-serif").to_owned();
    let mut system = FontSystem::new_with_locale_and_db("en-US".into(), db);
    let advance = {
        let mut b = Buffer::new(&mut system, TextMetrics::new(100.0, 120.0));
        b.set_text("MMMMMMMMMM", &Attrs::new().family(Family::Name(&mono)), Shaping::Advanced, None);
        b.shape_until_scroll(&mut system, false);
        b.layout_runs().next().map(|r| r.line_w / 1000.0).filter(|a| *a > 0.2).unwrap_or(0.6)
    };
    Fonts { system, mono, ui, advance }
}

/// The cell grid in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grid {
    /// DPI scale (physical px per logical px).
    pub scale: f32,
    pub cell_w: f32,
    pub cell_h: f32,
    /// Font size whose advance is exactly `cell_w`.
    pub font_px: f32,
}

impl Grid {
    pub fn new(advance: f32, scale: f32) -> Self {
        let cell_w = (advance * BASE_FONT_PX * scale).round().max(4.0);
        let font_px = cell_w / advance;
        let cell_h = (font_px * LINE).round().max(8.0);
        Self { scale, cell_w, cell_h, font_px }
    }

    /// Whole cells in a surface of `width` × `height` physical pixels.
    pub fn cells(&self, width: u32, height: u32) -> (u16, u16) {
        ((width as f32 / self.cell_w).floor().clamp(1.0, 4000.0) as u16, (height as f32 / self.cell_h).floor().clamp(1.0, 4000.0) as u16)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Face {
    Mono,
    Ui,
}

/// What to shape.
#[derive(Clone, Copy, Debug)]
pub struct Spec<'a> {
    pub text: &'a str,
    pub face: Face,
    pub bold: bool,
    pub italic: bool,
    /// Font size (px).
    pub size: f32,
    /// Ellipsize to this width (px).
    pub max_width: Option<f32>,
}

impl<'a> Spec<'a> {
    pub fn mono(text: &'a str, size: f32) -> Self {
        Self { text, face: Face::Mono, bold: false, italic: false, size, max_width: None }
    }

    pub fn ui(text: &'a str, size: f32) -> Self {
        Self { text, face: Face::Ui, bold: false, italic: false, size, max_width: None }
    }

    pub fn bold(self, bold: bool) -> Self {
        Self { bold, ..self }
    }

    pub fn italic(self, italic: bool) -> Self {
        Self { italic, ..self }
    }

    pub fn fit(self, width: f32) -> Self {
        Self { max_width: Some(width.max(0.0)), ..self }
    }

    fn key(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.text.hash(&mut h);
        (self.face, self.bold, self.italic, (self.size * 16.0).round() as u32).hash(&mut h);
        self.max_width.map(|w| (w * 4.0).round() as u32).hash(&mut h);
        h.finish()
    }
}

struct Entry {
    buffer: Buffer,
    width: f32,
    used: u64,
}

/// Shaped text by content, face and size; entries unused for a while are dropped.
#[derive(Default)]
pub struct ShapeCache {
    entries: HashMap<u64, Entry>,
    frame: u64,
    pub shaped: u64,
}

impl ShapeCache {
    /// Shapes `spec` once; returns its key and width (px).
    pub fn get(&mut self, fonts: &mut Fonts, spec: Spec) -> (u64, f32) {
        let key = spec.key();
        let frame = self.frame;
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.used = frame;
            return (key, entry.width);
        }
        let family = match spec.face {
            Face::Mono => Family::Name(&fonts.mono),
            Face::Ui => Family::Name(&fonts.ui),
        };
        let mut attrs = Attrs::new().family(family);
        if spec.bold {
            attrs = attrs.weight(if spec.face == Face::Ui { Weight::SEMIBOLD } else { Weight::BOLD });
        }
        if spec.italic {
            attrs = attrs.style(Style::Italic);
        }
        let size = spec.size.max(1.0);
        let mut buffer = Buffer::new(&mut fonts.system, TextMetrics::new(size, (size * LINE).round().max(1.0)));
        buffer.set_wrap(Wrap::None);
        if let Some(width) = spec.max_width {
            buffer.set_ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)));
            buffer.set_size(Some(width), None);
        }
        buffer.set_text(spec.text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut fonts.system, false);
        let width = buffer.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
        self.shaped += 1;
        self.entries.insert(key, Entry { buffer, width, used: frame });
        (key, width)
    }

    pub fn buffer(&self, key: u64) -> Option<&Buffer> {
        self.entries.get(&key).map(|e| &e.buffer)
    }

    /// Marks an entry used this frame (text kept on the GPU from earlier frames).
    pub fn touch(&mut self, key: u64) {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.used = self.frame;
        }
    }

    /// Ends a frame: entries unused for 300 frames go.
    pub fn end_frame(&mut self) {
        self.frame += 1;
        if self.frame.is_multiple_of(120) || self.entries.len() > 40_000 {
            let frame = self.frame;
            self.entries.retain(|_, e| frame - e.used < 300);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Whether the grid font draws `c` in one cell (so it can share a run with
/// its neighbours); anything else is placed on its own.
pub fn in_mono_run(c: char) -> bool {
    matches!(c, ' '..='~' | '\u{a0}'..='\u{17f}' | '\u{2010}'..='\u{2027}')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grid and label fonts this OS names are found (not the generic
    /// fallbacks), and their advance is a monospace one.
    #[test]
    #[cfg(any(windows, target_os = "macos"))]
    fn the_named_fonts_are_found() {
        let fonts = load();
        let families: Vec<String> = fonts.system.db().faces().flat_map(|f| f.families.iter().map(|(n, _)| n.clone())).collect();
        eprintln!("grid font {:?}, label font {:?}", fonts.mono, fonts.ui);
        assert_ne!(fonts.mono, "monospace", "families: {families:?}");
        assert!(families.contains(&fonts.ui), "label font {:?} among {families:?}", fonts.ui);
        assert!((0.4..0.8).contains(&fonts.advance), "advance {}", fonts.advance);
    }

    #[test]
    fn grid_cells_are_whole_pixels_and_fit_the_surface() {
        let grid = Grid::new(0.5859, 1.25);
        assert_eq!(grid.cell_w, 10.0);
        assert!((grid.font_px * 0.5859 - 10.0).abs() < 1e-3, "one advance is one cell");
        assert_eq!(grid.cell_h, grid.cell_h.round());
        assert_eq!(grid.cells(1700, 1012), (170, (1012.0 / grid.cell_h) as u16));
        assert!(in_mono_run('a') && in_mono_run('…') && in_mono_run('é') && !in_mono_run('中') && !in_mono_run('✓') && !in_mono_run('─'));
    }
}
