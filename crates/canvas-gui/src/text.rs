//! Fonts, the cell grid's metrics and a cache of shaped text.
//!
//! The grid font is Cascadia Mono (Consolas when missing), sized so one
//! advance is a whole number of physical pixels: text runs then stay on the
//! cell grid. Canvas labels use Segoe UI. Emoji, CJK and symbols come from
//! fallback fonts (Segoe UI Symbol and Emoji, Microsoft YaHei, Yu Gothic,
//! Malgun Gothic) that cosmic-text picks per character.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use glyphon::cosmic_text::{Ellipsize, EllipsizeHeightLimit};
use glyphon::{Attrs, Buffer, Family, FontSystem, Metrics as TextMetrics, Shaping, Style, Weight, Wrap};

/// Logical font size of the cell grid.
pub const BASE_FONT_PX: f32 = 14.0;
/// Cell height per px of font size.
const LINE: f32 = 1.3;
/// Font for canvas labels.
pub const UI_FONT: &str = "Segoe UI";

/// Fonts loaded from these files only (scanning every system font costs
/// hundreds of milliseconds at start).
const FONT_FILES: [&str; 16] = [
    "CascadiaMono.ttf",
    "CascadiaMonoItalic.ttf",
    "consola.ttf",
    "consolab.ttf",
    "consolai.ttf",
    "segoeui.ttf",
    "segoeuib.ttf",
    "segoeuii.ttf",
    "seguisb.ttf",
    "seguisym.ttf",
    "seguiemj.ttf",
    "msyh.ttc",
    "msyhbd.ttc",
    "YuGothR.ttc",
    "malgun.ttf",
    "seguihis.ttf",
];

pub struct Fonts {
    pub system: FontSystem,
    /// The grid font's family name.
    pub mono: String,
    /// Its advance per px of font size.
    pub advance: f32,
}

/// Loads the fonts (run on a thread at start, while the window and GPU come up).
pub fn load() -> Fonts {
    let mut db = glyphon::fontdb::Database::new();
    let windows = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into()).join("Fonts");
    for file in FONT_FILES {
        let path = windows.join(file);
        if path.is_file() {
            let _ = db.load_font_file(&path);
        }
    }
    // Fonts installed for this user only (Cascadia Mono often is).
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        db.load_fonts_dir(std::path::PathBuf::from(local).join(r"Microsoft\Windows\Fonts"));
    }
    let has = |db: &glyphon::fontdb::Database, name: &str| db.faces().any(|f| f.families.iter().any(|(n, _)| n == name));
    if !has(&db, "Cascadia Mono") && !has(&db, "Consolas") {
        db.load_system_fonts();
    }
    let mono = ["Cascadia Mono", "Consolas"].into_iter().find(|name| has(&db, name)).unwrap_or("monospace").to_owned();
    let mut system = FontSystem::new_with_locale_and_db("en-US".into(), db);
    let advance = {
        let mut b = Buffer::new(&mut system, TextMetrics::new(100.0, 120.0));
        b.set_text("MMMMMMMMMM", &Attrs::new().family(Family::Name(&mono)), Shaping::Advanced, None);
        b.shape_until_scroll(&mut system, false);
        b.layout_runs().next().map(|r| r.line_w / 1000.0).filter(|a| *a > 0.2).unwrap_or(0.6)
    };
    Fonts { system, mono, advance }
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
            Face::Ui => Family::Name(UI_FONT),
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
