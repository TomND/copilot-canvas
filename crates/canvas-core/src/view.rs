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
    /// Dust of an unused session: 0 none … 3 heavy (static, never animated).
    pub dust: u8,
    /// Opacity while a stack or hidden subsession animates (1: opaque).
    pub alpha: f32,
    /// This box is the top card of a stack of subsessions.
    pub deck: Option<DeckInfo>,
    /// Subsessions hidden into this box: (count, attention colour of the
    /// most important one).
    pub hidden: Option<(u32, Option<Color>)>,
}

/// A stack of subsessions drawn beneath their parent.
#[derive(Clone, Debug, PartialEq)]
pub struct DeckInfo {
    /// The stacked parent.
    pub root: String,
    /// Stacked sessions (the top card included).
    pub count: u32,
    /// Cards drawn behind the top one (0 … 2).
    pub back: u8,
}

/// An effect from the effects collection, drawn on its anchor.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectModel {
    pub id: String,
    /// The registry name (`fireworks`, `attention`, `sparkle`, `alert`).
    pub kind: &'static str,
    /// The anchor: a box, a stack's top card or a group's title row.
    pub rect: WRect,
    pub group: bool,
    /// Glyph and note ("✦ PR !12 approved").
    pub badge: String,
    /// Animation time (ms since this client first showed it).
    pub age_ms: u64,
}

/// How effects are drawn on this machine (Settings › Effects).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Motion {
    /// Animated (GPU particles and rings; TUI badges).
    #[default]
    On,
    /// Static rings and badges, no animation (also for stacks).
    Reduced,
    /// Badges only.
    Off,
}

impl Motion {
    pub fn name(self) -> &'static str {
        match self {
            Motion::On => "on",
            Motion::Reduced => "reduced motion",
            Motion::Off => "off",
        }
    }
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
    /// Clickable chips on the title row: folders, passive sessions.
    pub chips: Vec<ChipModel>,
    pub has_folder: bool,
    /// Automation family labels: (world x, world y, name).
    pub families: Vec<(f32, f32, String)>,
}

/// A clickable chip on a group's title row ("◧ AI docs", "⟲ 2", "+ passive").
#[derive(Clone, Debug, PartialEq)]
pub struct ChipModel {
    pub label: String,
    pub action: crate::dialogs::Action,
    /// Where it is (world units); `fixed`: sized in screen cells (zoomed
    /// out, so it stays readable), else scaled with the zoom.
    pub rect: WRect,
    pub fixed: bool,
    /// Drawn in the group's colour (else muted: nothing set up yet).
    pub lit: bool,
}

/// A looping box's loopback arc. The lit dot's place is a function of the
/// clock (`crate::loops`), so front ends draw it at any instant.
#[derive(Clone, Debug, PartialEq)]
pub struct LoopModel {
    /// The looping box.
    pub id: String,
    pub rect: WRect,
    /// The box has the active-terminal halo (the arc goes around it).
    pub halo: bool,
    /// When the loop started (the arc grows in) or, with `ending`, stopped
    /// (it fades out), on the animation clock (ms); `None`: steady.
    pub changed_ms: Option<u64>,
    pub ending: bool,
}

impl LoopModel {
    /// The lap fraction (0..1) of the dot at `clock_ms`.
    pub fn lap_at(&self, clock_ms: u64) -> f32 {
        crate::loops::lap(clock_ms, crate::loops::seed(&self.id))
    }

    /// How much of the arc shows at `clock_ms` (0..1).
    pub fn grow_at(&self, clock_ms: u64) -> f32 {
        crate::loops::grow(clock_ms, self.changed_ms, self.ending)
    }
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
    /// Effects on boxes and groups.
    pub effects: Vec<EffectModel>,
    pub motion: Motion,
    /// The animation clock when the model was built (ms).
    pub clock_ms: u64,
    /// Animations stand still at `clock_ms` (the `freeze` hook, tests).
    pub frozen: bool,
}

impl CanvasModel {
    /// The clock to draw animations at: now, or `clock_ms` when frozen.
    pub fn anim_clock(&self) -> u64 {
        if self.frozen { self.clock_ms } else { crate::paths::now_ms() }
    }

    /// Loop dots move on their own (motion on, a running clock): the front
    /// end redraws them at its own pace.
    pub fn loops_move(&self) -> bool {
        self.motion == Motion::On && !self.frozen && !self.loops.is_empty()
    }

    /// Some loop is moving, growing in or fading out at `clock_ms`.
    pub fn loops_busy(&self, clock_ms: u64) -> bool {
        self.loops_move() || self.loops.iter().any(|l| crate::loops::changing(clock_ms, l.changed_ms))
    }
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

// ---------- stable text dump (snapshot tests) ----------

/// A number with at most three decimals: "2", "8.3", "-0.25".
fn num(v: f32) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".to_owned() } else { s.to_owned() }
}

fn wrect(r: WRect) -> String {
    format!("{},{} {}x{}", num(r.x), num(r.y), num(r.w), num(r.h))
}

fn cells(r: Rect) -> String {
    format!("{},{} {}x{}", r.x, r.y, r.width, r.height)
}

fn path(points: &[(f32, f32)]) -> String {
    points.iter().map(|(x, y)| format!("({},{})", num(*x), num(*y))).collect::<Vec<_>>().join(" ")
}

/// A colour as `#rrggbb`, `idx<n>` or its lower-case name.
pub fn color_name(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(i) => format!("idx{i}"),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// A style as `[fg #… bg #… +BOLD -DIM]` (`[]`: the default style).
pub fn style_name(style: Style) -> String {
    use ratatui::style::Modifier;
    const MODIFIERS: [(Modifier, &str); 9] = [
        (Modifier::BOLD, "BOLD"),
        (Modifier::DIM, "DIM"),
        (Modifier::ITALIC, "ITALIC"),
        (Modifier::UNDERLINED, "UNDERLINED"),
        (Modifier::SLOW_BLINK, "SLOW_BLINK"),
        (Modifier::RAPID_BLINK, "RAPID_BLINK"),
        (Modifier::REVERSED, "REVERSED"),
        (Modifier::HIDDEN, "HIDDEN"),
        (Modifier::CROSSED_OUT, "CROSSED_OUT"),
    ];
    let mut parts = Vec::new();
    if let Some(fg) = style.fg {
        parts.push(format!("fg {}", color_name(fg)));
    }
    if let Some(bg) = style.bg {
        parts.push(format!("bg {}", color_name(bg)));
    }
    for (modifier, name) in MODIFIERS {
        if style.add_modifier.contains(modifier) {
            parts.push(format!("+{name}"));
        }
        if style.sub_modifier.contains(modifier) {
            parts.push(format!("-{name}"));
        }
    }
    format!("[{}]", parts.join(" "))
}

impl CanvasModel {
    /// A stable, line-oriented text dump (snapshot tests): world units with
    /// at most three decimals, colours as `#rrggbb`, in drawing order.
    pub fn dump(&self) -> String {
        let mut out = vec![format!(
            "canvas area={} origin={},{} scale={} grid={} tick={} empty={}",
            cells(self.area),
            num(self.origin.0),
            num(self.origin.1),
            num(self.scale),
            self.grid,
            self.tick,
            self.empty
        )];
        for g in &self.groups {
            out.push(format!(
                "group {} {:?} icon={} members={} rect={} color={} style={} selected={} drop_target={} automations={} folder={}",
                g.id,
                g.name,
                g.icon,
                g.members,
                wrect(g.rect),
                color_name(g.color),
                style_name(g.style),
                g.selected,
                g.drop_target,
                g.automations,
                g.has_folder
            ));
            for chip in &g.chips {
                out.push(format!("  chip {:?} action={:?} rect={} fixed={} lit={}", chip.label, chip.action, wrect(chip.rect), chip.fixed, chip.lit));
            }
            for (x, y, family) in &g.families {
                out.push(format!("  family {:?} at={},{}", family, num(*x), num(*y)));
            }
        }
        for e in &self.edges {
            out.push(format!("edge {:?} color={} from={} to={} path={}", e.kind, color_name(e.color), wrect(e.from), wrect(e.to), path(&e.path)));
        }
        for b in &self.boxes {
            out.push(format!("box {} {:?} rect={} prefix={:?} name={:?}", b.id, b.kind, wrect(b.rect), b.prefix, b.name));
            out.push(format!(
                "  status={:?} icon={:?} label={:?} line={:?} subtitle={:?} elsewhere={} every={:?} mailbox={}",
                b.status, b.icon, b.label, b.line, b.subtitle, b.elsewhere, b.every, b.mailbox
            ));
            out.push(format!(
                "  border={:?} border_style={} fill={} attention={} name_style={} line_style={}",
                b.border,
                style_name(b.border_style),
                color_name(b.fill),
                b.attention.map(|(c, t)| format!("{}/{}", color_name(c), color_name(t))).unwrap_or_else(|| "none".into()),
                style_name(b.name_style),
                style_name(b.line_style)
            ));
            out.push(format!("  selected={} focused_term={} active={}", b.selected, b.focused_term, b.active));
            // Visual extras only when present (older snapshots stay as they were).
            if b.dust > 0 {
                out.push(format!("  dust={}", b.dust));
            }
            if b.alpha < 1.0 {
                out.push(format!("  alpha={}", num(b.alpha)));
            }
            if let Some(deck) = &b.deck {
                out.push(format!("  deck root={} count={} back={}", deck.root, deck.count, deck.back));
            }
            if let Some((count, color)) = b.hidden {
                out.push(format!("  hidden={count} color={}", color.map(color_name).unwrap_or_else(|| "none".into())));
            }
        }
        for e in &self.effects {
            out.push(format!("effect {} {} rect={} group={} badge={:?} age_ms={}", e.id, e.kind, wrect(e.rect), e.group, e.badge, e.age_ms));
        }
        if !self.effects.is_empty() {
            out.push(format!("motion {:?}", self.motion));
        }
        if !self.loops.is_empty() {
            out.push(format!("loops motion={:?} frozen={}", self.motion, self.frozen));
        }
        for l in &self.loops {
            // The dot's lap only on a fixed clock (else it is the time of the run).
            let lap = if self.frozen { format!(" lap={}", num(l.lap_at(self.clock_ms))) } else { String::new() };
            out.push(format!("loop {} rect={} halo={}{lap} grow={} ending={}", l.id, wrect(l.rect), l.halo, num(l.grow_at(self.clock_ms)), l.ending));
        }
        for p in &self.pulses {
            out.push(format!("pulse from={} to={} t={} path={}", wrect(p.from), wrect(p.to), num(p.t), path(&p.path)));
        }
        if let Some((rect, typing)) = self.halo {
            out.push(format!("halo rect={} typing={typing}", wrect(rect)));
        }
        if let Some((x0, y0, x1, y1)) = self.rubber {
            out.push(format!("rubber {},{} .. {},{}", num(x0), num(y0), num(x1), num(y1)));
        }
        out.join("\n")
    }
}

impl TileModel {
    /// One line for the snapshot dump.
    pub fn dump(&self) -> String {
        format!(
            "tile {} outer={} inner={} focused={} alive={} cursor={} tint={} title={:?}",
            self.id,
            cells(self.outer),
            cells(self.inner),
            self.focused,
            self.alive,
            self.cursor,
            self.tint,
            self.title
        )
    }
}

impl View {
    /// The whole view model as stable text (snapshot tests).
    pub fn dump(&self) -> String {
        let mut out = vec![format!(
            "view cursor={} modal={} banner={:?}",
            self.cursor.map(|(x, y)| format!("{x},{y}")).unwrap_or_else(|| "none".into()),
            self.modal.map(cells).unwrap_or_else(|| "none".into()),
            self.banner
        )];
        match &self.canvas {
            Some(canvas) => out.push(canvas.dump()),
            None => out.push("canvas none".into()),
        }
        out.extend(self.tiles.iter().map(TileModel::dump));
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, CanvasHit, Drag};
    use crate::conn::Conn;
    use crate::doc::Doc;
    use ratatui::buffer::Buffer;

    fn app(doc: Doc) -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("cc-view-{}", uuid::Uuid::new_v4()));
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let (_service, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, _app_rx) = crossbeam_channel::unbounded();
        let mut app = App::new(Conn::detached(tx, service_rx), std::sync::Arc::new(crate::rpc::Offline), crate::sync::Opened::detached(doc), true, ".".into(), app_tx, None);
        app.layout.canvas = Rect::new(34, 1, 70, 30);
        (app, dir)
    }

    #[test]
    fn dump_numbers_and_styles_are_stable() {
        assert_eq!([num(2.0), num(8.3), num(-0.25), num(-0.0001), num(0.70000005), num(10.0)], ["2", "8.3", "-0.25", "0", "0.7", "10"]);
        use ratatui::style::Modifier;
        let style = Style::default().fg(Color::Rgb(1, 2, 255)).add_modifier(Modifier::BOLD | Modifier::DIM).remove_modifier(Modifier::ITALIC);
        assert_eq!(style_name(style), "[fg #0102ff +BOLD +DIM -ITALIC]");
        assert_eq!(style_name(Style::default()), "[]");
        assert_eq!(color_name(Color::Indexed(7)), "idx7");
        assert_eq!(color_name(Color::DarkGray), "darkgray");
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