//! The view model for front ends that draw the canvas and terminals
//! themselves (the GPU client). `ui::render` in [`RenderMode::Gpu`] fills the
//! chrome (toolbar, sidebar, panels, tile frames, status bar) into a cell
//! buffer and describes the canvas and the terminal tiles here; in
//! [`RenderMode::Cells`] it draws everything as characters (the TUI).

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use crate::status::Status;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RenderMode {
    /// Everything is drawn as characters (the TUI).
    #[default]
    Cells,
    /// The canvas and terminal tiles are described by models and drawn by the
    /// front end at any scale; the chrome stays a grid of cells.
    Gpu,
}

/// A rectangle in world units (fractional while dragging).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl WRect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn offset(self, (dx, dy): (f32, f32)) -> Self {
        Self { x: self.x + dx, y: self.y + dy, ..self }
    }
}

impl From<crate::doc::Rect> for WRect {
    fn from(r: crate::doc::Rect) -> Self {
        Self::new(r.x as f32, r.y as f32, r.w as f32, r.h as f32)
    }
}

/// Outline of a box, as the TUI draws it at full zoom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Border {
    /// Rounded light (sessions).
    Round,
    /// Square light (subagents).
    Light,
    /// Heavy (selected; needs you).
    Heavy,
    /// Double (done · review, automations).
    Double,
    /// Dashed (passive sessions; unselected groups).
    Dashed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxKind {
    Session,
    /// A subagent (inside its parent session's terminal).
    Agent,
    Automation,
    Passive,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoxModel {
    pub id: String,
    pub kind: BoxKind,
    /// World rectangle (with the visual drag offset).
    pub rect: WRect,
    /// "⟲ ", "◇ ", "◷ ", "↳ " or empty.
    pub prefix: &'static str,
    pub name: String,
    pub status: Status,
    /// Status icon at this frame (spinners turn).
    pub icon: &'static str,
    /// Status label ("working", "done · review", "last run failed", …).
    pub label: &'static str,
    /// The full status line: icon, label, loop, or the passive headline.
    pub line: String,
    /// Status colour (bold for attention states).
    pub line_style: Style,
    pub name_style: Style,
    /// Open in another terminal (CLI or VS Code).
    pub elsewhere: bool,
    pub subtitle: String,
    pub border: Border,
    /// Colour and weight of the outline (bold: attention, selection; dim: plain).
    pub border_style: Style,
    /// Body fill (tinted for attention states).
    pub fill: Color,
    /// Coloured border and tint for states that want your eyes (needs you,
    /// done · review, a failed automation): (border, tint).
    pub attention: Option<(Color, Color)>,
    pub selected: bool,
    /// Its terminal is the focused one.
    pub focused_term: bool,
    /// Its terminal is the active tile (the halo goes around it).
    pub active: bool,
    /// "every 15m" for looping boxes.
    pub every: Option<String>,
    /// Queued messages (passive bus).
    pub mailbox: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    /// Parent → subagent (thin solid).
    Subagent,
    /// Parent → subsession (dashed accent).
    Subsession,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EdgeModel {
    pub kind: EdgeKind,
    pub color: Color,
    /// The parent and child boxes.
    pub from: WRect,
    pub to: WRect,
    /// Orthogonal route in world units, from the parent's edge to the child's.
    pub path: Vec<(f32, f32)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroupModel {
    pub id: String,
    pub rect: WRect,
    pub name: String,
    /// "▣" or "◷" (automations).
    pub icon: &'static str,
    pub members: usize,
    pub color: Color,
    /// Outline (bold when selected or a drop target, else dim).
    pub style: Style,
    pub selected: bool,
    /// A dragged box would land in this group.
    pub drop_target: bool,
    pub automations: bool,
    /// Folder chips on the title row (clickable; dim until a folder is set).
    pub chips: Vec<ChipModel>,
    pub has_folder: bool,
    /// Automation family labels: (world x, world y, name).
    pub families: Vec<(f32, f32, String)>,
}

/// A clickable chip on a group's title row ("◧ AI docs", "◨ Outputs").
#[derive(Clone, Debug, PartialEq)]
pub struct ChipModel {
    pub label: &'static str,
    /// Opens human-outputs (else ai-docs).
    pub outputs: bool,
    pub rect: WRect,
}

/// A looping box's loopback arc with its moving dot.
#[derive(Clone, Debug, PartialEq)]
pub struct LoopModel {
    pub rect: WRect,
    /// Position of the lit dot along the arc, 0..1.
    pub phase: f32,
    /// The box has the active-terminal halo (the arc goes around it).
    pub halo: bool,
}

/// An event in flight from one box to another.
#[derive(Clone, Debug, PartialEq)]
pub struct PulseModel {
    pub from: WRect,
    pub to: WRect,
    pub path: Vec<(f32, f32)>,
    /// Progress 0..1.
    pub t: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CanvasModel {
    /// The canvas on screen (cells).
    pub area: Rect,
    /// World point at the area's top-left corner.
    pub origin: (f32, f32),
    /// Cells per world unit.
    pub scale: f32,
    /// The dot grid (hidden while panning).
    pub grid: bool,
    /// Largest group first (drawn first).
    pub groups: Vec<GroupModel>,
    pub edges: Vec<EdgeModel>,
    /// Selected boxes last (drawn on top).
    pub boxes: Vec<BoxModel>,
    pub loops: Vec<LoopModel>,
    pub pulses: Vec<PulseModel>,
    /// The active terminal's box: (rect, heavy while typing).
    pub halo: Option<(WRect, bool)>,
    /// Rubber band in screen cells (fractional): x0, y0, x1, y1.
    pub rubber: Option<(f32, f32, f32, f32)>,
    /// The canvas is empty: show the getting-started hint.
    pub empty: bool,
    /// Spinner and animation frame (80 ms steps).
    pub tick: usize,
}

impl CanvasModel {
    /// World → screen (fractional cells).
    pub fn to_screen(&self, wx: f32, wy: f32) -> (f32, f32) {
        (self.area.x as f32 + (wx - self.origin.0) * self.scale, self.area.y as f32 + (wy - self.origin.1) * self.scale)
    }

    /// A world rectangle on screen (fractional cells).
    pub fn rect_to_screen(&self, r: WRect) -> WRect {
        let (x, y) = self.to_screen(r.x, r.y);
        WRect::new(x, y, r.w * self.scale, r.h * self.scale)
    }

    /// The visible world rectangle.
    pub fn visible(&self) -> WRect {
        WRect::new(self.origin.0, self.origin.1, self.area.width as f32 / self.scale, self.area.height as f32 / self.scale)
    }
}

/// A terminal tile the front end draws from `App::views[id]`.
#[derive(Clone, Debug, PartialEq)]
pub struct TileModel {
    pub id: String,
    /// The frame (drawn in the chrome) and the terminal grid inside it.
    pub outer: Rect,
    pub inner: Rect,
    pub focused: bool,
    pub alive: bool,
    /// Draw the cursor (focused, live terminal).
    pub cursor: bool,
    /// Tint your own messages (Copilot sessions).
    pub tint: bool,
    pub title: String,
}

/// What a frame shows besides the chrome buffer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    /// Gpu mode: the canvas to draw under the chrome.
    pub canvas: Option<CanvasModel>,
    pub tiles: Vec<TileModel>,
    /// Text cursor (a focused field or terminal), in cells.
    pub cursor: Option<(u16, u16)>,
    /// The top-most dialog or menu.
    pub modal: Option<Rect>,
    pub banner: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, CanvasHit, Drag};
    use crate::conn::Conn;
    use crate::doc::{Doc, Store};
    use ratatui::buffer::Buffer;

    fn app(doc: Doc) -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("cc-view-{}", uuid::Uuid::new_v4()));
        let store = Store::open(&dir.join("canvas.json")).unwrap();
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let (_service, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, _app_rx) = crossbeam_channel::unbounded();
        let mut app = App::new(Conn::detached(tx, service_rx), store, doc, true, ".".into(), app_tx, dir.clone());
        app.layout.canvas = Rect::new(34, 1, 70, 30);
        (app, dir)
    }

    #[test]
    fn the_model_paints_what_the_tui_draws_at_full_zoom() {
        let (mut app, dir) = app(crate::demo::demo_doc("."));
        app.selection = vec!["demo-review".into()];
        app.pan_quiet_until = None;
        let area = app.layout.canvas;
        let mut cells = Buffer::empty(Rect::new(0, 0, 110, 32));
        crate::ui_canvas::draw_canvas(&mut app, &mut cells, area);
        app.render_mode = RenderMode::Gpu;
        app.view_scale = 1.0;
        let model = crate::ui_canvas::canvas_model(&mut app, area);
        assert_eq!(model.boxes.len(), 4);
        assert_eq!(model.edges.len(), 2);
        assert_eq!(model.edges.iter().filter(|e| e.kind == EdgeKind::Subsession).count(), 1);
        assert!(model.boxes.last().is_some_and(|b| b.id == "demo-review" && b.selected && b.border == Border::Heavy), "selected boxes come last");
        let mut painted = Buffer::empty(Rect::new(0, 0, 110, 32));
        crate::ui_canvas::paint_model(&model, &mut painted);
        assert_eq!(painted, cells);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn continuous_zoom_steps_in_powers_of_two_and_keeps_the_point_under_the_pointer() {
        let (mut app, dir) = app(crate::demo::demo_doc("."));
        app.render_mode = RenderMode::Gpu;
        let c = app.layout.canvas;
        let center = ((c.x + c.width / 2) as i32, (c.y + c.height / 2) as i32);
        app.zoom_at(center.0, center.1, 1);
        assert_eq!(app.view_scale, 0.5);
        app.zoom_at(center.0, center.1, 1);
        assert_eq!(app.view_scale, 0.25);
        app.zoom_at(center.0, center.1, -1);
        assert_eq!((app.view_scale, app.doc.view.zoom), (0.5, 1), "the document keeps the nearest discrete zoom");
        app.view_scale = 0.37;
        app.zoom_at(center.0, center.1, -1);
        assert_eq!(app.view_scale, 0.5, "from between levels to the next one");
        let before = app.to_world_f(50.3, 12.7);
        app.zoom_by(50.3, 12.7, 1.3);
        let after = app.to_world_f(50.3, 12.7);
        assert!((before.0 - after.0).abs() < 1e-3 && (before.1 - after.1).abs() < 1e-3, "{before:?} {after:?}");
        app.zoom_by(0.0, 0.0, 1000.0);
        assert_eq!(app.view_scale, crate::app::MAX_SCALE);
        app.fit_all();
        assert!(app.view_scale <= 1.0 && app.view_scale >= 0.5, "the demo fits at {}", app.view_scale);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pixel_hit_testing_uses_the_precise_pointer() {
        let (mut app, dir) = app(crate::demo::demo_doc("."));
        app.render_mode = RenderMode::Gpu;
        app.view_scale = 0.5;
        app.view_frac = (0.25, 0.0);
        // demo-main is at world (2, 2), 30 x 4: on screen x from 34 + (2 - (-2 + 0.25)) * 0.5 = 35.875.
        let (x, y) = app.to_screen_f(2.0, 2.0);
        assert!((x - 35.875).abs() < 1e-4 && (y - 2.5).abs() < 1e-4);
        assert!(matches!(app.canvas_hit_f((x + 0.05, y + 0.05)), CanvasHit::Node(id) if id == "demo-main"));
        assert!(!matches!(app.canvas_hit_f((x - 0.05, y + 0.05)), CanvasHit::Node(_)), "a hair left of the box");
        // The group's title row stays clickable at small scales.
        let (gx, gy) = app.to_screen_f(30.0, 0.0);
        assert!(matches!(app.canvas_hit_f((gx, gy + 0.3)), CanvasHit::GroupTitle(id) if id == "g-demo"));
        // Cell-based callers get the cell centre unless the pointer is in that cell.
        app.pointer = (40.9, 3.2);
        assert_eq!(app.point_in(40, 3), (40.9, 3.2));
        assert_eq!(app.point_in(41, 3), (41.5, 3.5));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn drags_move_whole_units_and_show_the_rest_as_an_offset() {
        let (mut app, dir) = app(crate::demo::demo_doc("."));
        app.render_mode = RenderMode::Gpu;
        app.layout.canvas = Rect::new(0, 0, 100, 40);
        let down = crate::input::MouseEvent::at(crate::input::MouseEventKind::Down(crate::input::MouseButton::Left), 10.5, 4.5, crate::input::Modifiers::NONE);
        app.on_mouse(down);
        assert!(matches!(&app.drag, Some(Drag::Nodes { ids, .. }) if ids == &["demo-main".to_owned()]));
        let drag = crate::input::MouseEvent::at(crate::input::MouseEventKind::Drag(crate::input::MouseButton::Left), 16.8, 6.2, crate::input::Modifiers::NONE);
        app.on_mouse(drag);
        let node = app.doc.node("demo-main").unwrap();
        assert_eq!((node.x, node.y), (8, 4), "moved by the rounded distance (6.3, 1.7)");
        let (ox, oy) = app.drag_offset();
        assert!((ox - 0.3).abs() < 1e-4 && (oy + 0.3).abs() < 1e-4, "({ox}, {oy})");
        let area = app.layout.canvas;
        let model = crate::ui_canvas::canvas_model(&mut app, area);
        let main = model.boxes.iter().find(|b| b.id == "demo-main").unwrap();
        let agent = model.boxes.iter().find(|b| b.id == "demo-main:explore").unwrap();
        assert!((main.rect.x - 8.3).abs() < 1e-4 && (agent.rect.x - 12.3).abs() < 1e-4, "the subagent follows smoothly");
        let up = crate::input::MouseEvent::at(crate::input::MouseEventKind::Up(crate::input::MouseButton::Left), 16.8, 6.2, crate::input::Modifiers::NONE);
        app.on_mouse(up);
        assert!(app.drag.is_none() && app.drag_offset() == (0.0, 0.0));
        let _ = std::fs::remove_dir_all(dir);
    }
}