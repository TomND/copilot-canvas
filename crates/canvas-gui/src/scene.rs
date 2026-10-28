//! A frame as layers of quads, images and text, in drawing order. Each layer
//! draws its `under` quads, its images below the text, its text, its images
//! above the text, then its `over` quads.

use std::sync::Arc;

use crate::quads::{Quad, Rgba};
use crate::text::{Fonts, ShapeCache, Spec};

/// Chrome outside the canvas and the terminal grids.
pub const BASE: usize = 0;
/// The canvas background, dot grid, groups and connectors.
pub const CANVAS: usize = 1;
/// Boxes with their labels, in [`BOX_LEVELS`] layers: a box that overlaps an
/// earlier one goes one level up, so its fill covers the labels below it.
pub const BOXES: usize = 2;
pub const BOX_LEVELS: usize = 3;
/// Selected boxes, badges, pulses and the rubber band above the canvas.
pub const CANVAS_TOP: usize = BOXES + BOX_LEVELS;
/// Terminal grids.
pub const TILES: usize = CANVAS_TOP + 1;
/// Chrome cells drawn over the canvas or a terminal (markers, hints).
pub const CHROME_TOP: usize = TILES + 1;
/// Dialogs and menus.
pub const OVERLAY: usize = CHROME_TOP + 1;
/// IME preedit, the banner, the paste preview and the perf overlay.
pub const HUD: usize = OVERLAY + 1;
pub const LAYERS: usize = HUD + 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextItem {
    /// Shape cache key.
    pub key: u64,
    /// Top-left (physical px).
    pub x: f32,
    pub y: f32,
    pub color: [u8; 3],
    /// x0, y0, x1, y1.
    pub clip: [f32; 4],
    /// Extra scale (canvas labels shaped at one size and drawn at another).
    pub scale: f32,
}

/// An image drawn as a textured rectangle.
#[derive(Clone, Debug)]
pub struct ImageItem {
    /// Texture cache key: unique per image content.
    pub key: u64,
    pub width: u32,
    pub height: u32,
    /// Straight RGBA rows.
    pub rgba: Arc<Vec<u8>>,
    /// Destination x, y, w, h (physical px).
    pub rect: [f32; 4],
    /// The part of the image drawn: u0, v0, u1, v1 (0..1).
    pub uv: [f32; 4],
    pub clip: [f32; 4],
}

#[derive(Default)]
pub struct Layer {
    pub under: Vec<Quad>,
    /// Images below the layer's text (Kitty z < 0).
    pub images_under: Vec<ImageItem>,
    pub texts: Vec<TextItem>,
    /// Images above the text.
    pub images_over: Vec<ImageItem>,
    pub over: Vec<Quad>,
}

pub struct Scene {
    pub layers: [Layer; LAYERS],
    pub width: u32,
    pub height: u32,
    pub clear: [u8; 3],
}

impl Scene {
    pub fn new(width: u32, height: u32, clear: [u8; 3]) -> Self {
        Self { layers: Default::default(), width, height, clear }
    }
}

/// Builds a scene: shapes text through the cache and records where it goes.
pub struct Painter<'a> {
    pub scene: &'a mut Scene,
    pub cache: &'a mut ShapeCache,
    pub fonts: &'a mut Fonts,
}

impl Painter<'_> {
    pub fn under(&mut self, layer: usize, quad: Quad) {
        self.scene.layers[layer].under.push(quad);
    }

    pub fn over(&mut self, layer: usize, quad: Quad) {
        self.scene.layers[layer].over.push(quad);
    }

    /// An image below (`above_text`: above) the layer's text.
    pub fn image(&mut self, layer: usize, above_text: bool, item: ImageItem) {
        let layer = &mut self.scene.layers[layer];
        if above_text { layer.images_over.push(item) } else { layer.images_under.push(item) }
    }

    /// Shapes text without placing it; returns its key and width (px).
    pub fn shape(&mut self, spec: Spec) -> (u64, f32) {
        self.cache.get(self.fonts, spec)
    }

    /// Text at a top-left position; returns its width (px).
    pub fn text(&mut self, layer: usize, spec: Spec, x: f32, y: f32, color: [u8; 3], clip: [f32; 4]) -> f32 {
        let (key, width) = self.cache.get(self.fonts, spec);
        self.scene.layers[layer].texts.push(TextItem { key, x, y, color, clip, scale: 1.0 });
        width
    }

    /// Places shaped text (`shape`) drawn `scale` times its shaped size.
    #[allow(clippy::too_many_arguments)]
    pub fn place(&mut self, layer: usize, key: u64, x: f32, y: f32, scale: f32, color: [u8; 3], clip: [f32; 4]) {
        self.scene.layers[layer].texts.push(TextItem { key, x, y, color, clip, scale });
    }
}

/// A filled rectangle with edges on whole pixels (no seams between neighbours).
pub fn cell_rect(x0: f32, y0: f32, x1: f32, y1: f32, color: Rgba, clip: [f32; 4]) -> Quad {
    let (x0, y0, x1, y1) = (x0.round(), y0.round(), x1.round(), y1.round());
    Quad::rect(x0, y0, x1 - x0, y1 - y0, color, clip)
}
