//! Synthetic canvas shared by both spikes: 30 groups, 300 sessions and 3,000
//! subagent boxes with connectors, laid out like the real app; the camera
//! (pan, continuous zoom around a point) and culling.

#![allow(dead_code)]

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    pub fn inset(&self, d: f32) -> Rect {
        Rect::new(self.x + d, self.y + d, (self.w - 2.0 * d).max(0.0), (self.h - 2.0 * d).max(0.0))
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        Rect::new(x, y, (self.right().min(o.right()) - x).max(0.0), (self.bottom().min(o.bottom()) - y).max(0.0))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Working,
    NeedsYou,
    Done,
    Idle,
    Failed,
}

impl Status {
    pub fn color(self) -> u32 {
        match self {
            Status::Working => 0x4fa3ff,
            Status::NeedsYou => 0xffb02e,
            Status::Done => 0x3fb950,
            Status::Idle => 0x8b949e,
            Status::Failed => 0xf85149,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Status::Working => "working",
            Status::NeedsYou => "needs you",
            Status::Done => "done · review",
            Status::Idle => "idle",
            Status::Failed => "failed",
        }
    }
}

pub struct Group {
    pub rect: Rect,
    pub title: String,
    pub color: u32,
}

pub struct Session {
    pub rect: Rect,
    pub title: String,
    pub status_line: String,
    pub status: Status,
}

pub struct Agent {
    pub rect: Rect,
    pub label: String,
    pub session: usize,
    pub status: Status,
}

/// A connector piece: axis-aligned elbow segments (subagents) or a straight
/// diagonal (session → subsession links, dashed).
pub struct Edge {
    pub a: (f32, f32),
    pub b: (f32, f32),
    pub dashed: bool,
    pub color: u32,
    pub width: f32,
    pub bounds: Rect,
}

impl Edge {
    pub fn axis_aligned(&self) -> bool {
        self.a.0 == self.b.0 || self.a.1 == self.b.1
    }
}

pub struct Scene {
    pub groups: Vec<Group>,
    pub sessions: Vec<Session>,
    pub agents: Vec<Agent>,
    pub edges: Vec<Edge>,
    pub bounds: Rect,
}

/// Label sizes (logical px at zoom 1).
pub const GROUP_TITLE_PX: f32 = 16.0;
pub const SESSION_TITLE_PX: f32 = 13.0;
pub const SESSION_STATUS_PX: f32 = 11.0;
pub const AGENT_PX: f32 = 11.0;
/// Text smaller than this on screen is not drawn (level of detail).
pub const MIN_TEXT_PX: f32 = 4.5;

pub const BG: u32 = 0x1b1c22;
pub const SESSION_FILL: u32 = 0x2a2c36;
pub const AGENT_FILL: u32 = 0x22242c;
pub const AGENT_BORDER: u32 = 0x4d5260;
pub const TEXT: u32 = 0xe6e6e6;
pub const TEXT_DIM: u32 = 0x9aa0aa;
pub const EDGE: u32 = 0x5b6170;
pub const EDGE_DASHED: u32 = 0x9d8fe6;

const GROUP_COLORS: [u32; 8] = [0x58a6ff, 0xd2a8ff, 0x3fb950, 0xffa657, 0xff7b72, 0x79c0ff, 0xe3b341, 0x56d4dd];
const TOPICS: [&str; 10] =
    ["fix flaky build", "auth refactor", "docs sweep", "perf triage", "release notes", "ui polish", "db migration", "api review", "test coverage", "dependency bump"];
const AGENT_KINDS: [&str; 5] = ["Explorer", "Task", "Code review", "General", "Research"];

pub const GROUP_COLS: usize = 6;
pub const GROUP_ROWS: usize = 5;
pub const SESSIONS_PER_GROUP: usize = 10;
pub const AGENTS_PER_SESSION: usize = 10;

const SESSION_W: f32 = 240.0;
const SESSION_H: f32 = 58.0;
const AGENT_W: f32 = 176.0;
const AGENT_H: f32 = 22.0;
const AGENT_STEP: f32 = 28.0;
const CELL_W: f32 = 280.0;
const CELL_H: f32 = 370.0;
const GROUP_PAD: f32 = 24.0;
const GROUP_TITLE_H: f32 = 40.0;
const GROUP_GAP: f32 = 90.0;

pub fn build() -> Scene {
    let mut scene = Scene { groups: Vec::new(), sessions: Vec::new(), agents: Vec::new(), edges: Vec::new(), bounds: Rect::default() };
    let group_w = GROUP_PAD * 2.0 + CELL_W * 5.0;
    let group_h = GROUP_TITLE_H + GROUP_PAD + CELL_H * 2.0;
    let statuses = [Status::Working, Status::Idle, Status::NeedsYou, Status::Done, Status::Working, Status::Failed, Status::Idle];
    for g in 0..GROUP_COLS * GROUP_ROWS {
        let gx = (g % GROUP_COLS) as f32 * (group_w + GROUP_GAP);
        let gy = (g / GROUP_COLS) as f32 * (group_h + GROUP_GAP);
        scene.groups.push(Group {
            rect: Rect::new(gx, gy, group_w, group_h),
            title: format!("Group {:02} · {}", g + 1, TOPICS[g % TOPICS.len()]),
            color: GROUP_COLORS[g % GROUP_COLORS.len()],
        });
        for s in 0..SESSIONS_PER_GROUP {
            let index = scene.sessions.len();
            let sx = gx + GROUP_PAD + (s % 5) as f32 * CELL_W;
            let sy = gy + GROUP_TITLE_H + (s / 5) as f32 * CELL_H;
            let status = statuses[index % statuses.len()];
            let rect = Rect::new(sx, sy, SESSION_W, SESSION_H);
            scene.sessions.push(Session {
                rect,
                title: format!("Session {:03} · {}", index + 1, TOPICS[(index * 7) % TOPICS.len()]),
                status_line: format!("● {} · {} agents · main", status.label(), AGENTS_PER_SESSION),
                status,
            });
            // Trunk down the left side, a branch to each subagent.
            let trunk_x = sx + 14.0;
            let first_y = sy + SESSION_H + 14.0;
            let last_cy = first_y + (AGENTS_PER_SESSION - 1) as f32 * AGENT_STEP + AGENT_H / 2.0;
            push_edge(&mut scene.edges, (trunk_x, sy + SESSION_H), (trunk_x, last_cy), false, EDGE, 1.5);
            for a in 0..AGENTS_PER_SESSION {
                let ay = first_y + a as f32 * AGENT_STEP;
                let ax = sx + 34.0;
                let dashed = a % 4 == 3;
                let agent_status = if a < 3 { Status::Working } else if a % 5 == 4 { Status::Failed } else { Status::Done };
                scene.agents.push(Agent {
                    rect: Rect::new(ax, ay, AGENT_W, AGENT_H),
                    label: format!("{} {} · {}", AGENT_KINDS[a % AGENT_KINDS.len()], a + 1, agent_status.label()),
                    session: index,
                    status: agent_status,
                });
                let cy = ay + AGENT_H / 2.0;
                push_edge(&mut scene.edges, (trunk_x, cy), (ax, cy), dashed, if dashed { EDGE_DASHED } else { EDGE }, 1.5);
            }
            // One session per group spawned a subsession: a dashed diagonal link.
            if s % 5 == 1 && s + 4 < SESSIONS_PER_GROUP {
                let to = Rect::new(gx + GROUP_PAD + ((s + 4) % 5) as f32 * CELL_W, gy + GROUP_TITLE_H + ((s + 4) / 5) as f32 * CELL_H, SESSION_W, SESSION_H);
                push_edge(&mut scene.edges, (sx + SESSION_W, sy + SESSION_H / 2.0), (to.x + SESSION_W / 2.0, to.y), true, EDGE_DASHED, 1.5);
            }
        }
    }
    let w = GROUP_COLS as f32 * (group_w + GROUP_GAP) - GROUP_GAP;
    let h = GROUP_ROWS as f32 * (group_h + GROUP_GAP) - GROUP_GAP;
    scene.bounds = Rect::new(0.0, 0.0, w, h);
    scene
}

fn push_edge(edges: &mut Vec<Edge>, a: (f32, f32), b: (f32, f32), dashed: bool, color: u32, width: f32) {
    let x0 = a.0.min(b.0) - width;
    let y0 = a.1.min(b.1) - width;
    let bounds = Rect::new(x0, y0, (a.0 - b.0).abs() + 2.0 * width, (a.1 - b.1).abs() + 2.0 * width);
    edges.push(Edge { a, b, dashed, color, width, bounds });
}

/// Indices of the items intersecting a world rectangle.
#[derive(Default)]
pub struct Visible {
    pub groups: Vec<usize>,
    pub sessions: Vec<usize>,
    pub agents: Vec<usize>,
    pub edges: Vec<usize>,
}

impl Visible {
    pub fn boxes(&self) -> usize {
        self.groups.len() + self.sessions.len() + self.agents.len()
    }
}

impl Scene {
    pub fn visible(&self, world: &Rect, out: &mut Visible) {
        out.groups.clear();
        out.sessions.clear();
        out.agents.clear();
        out.edges.clear();
        out.groups.extend(self.groups.iter().enumerate().filter(|(_, g)| g.rect.intersects(world)).map(|(i, _)| i));
        out.sessions.extend(self.sessions.iter().enumerate().filter(|(_, s)| s.rect.intersects(world)).map(|(i, _)| i));
        out.agents.extend(self.agents.iter().enumerate().filter(|(_, a)| a.rect.intersects(world)).map(|(i, _)| i));
        out.edges.extend(self.edges.iter().enumerate().filter(|(_, e)| e.bounds.intersects(world)).map(|(i, _)| i));
    }
}

/// World position at the viewport's top-left corner, and the zoom factor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub x: f32,
    pub y: f32,
    pub zoom: f32,
}

pub const MIN_ZOOM: f32 = 0.05;
pub const MAX_ZOOM: f32 = 4.0;

impl Camera {
    pub fn to_screen(&self, vp: &Rect, p: (f32, f32)) -> (f32, f32) {
        (vp.x + (p.0 - self.x) * self.zoom, vp.y + (p.1 - self.y) * self.zoom)
    }
    pub fn rect_to_screen(&self, vp: &Rect, r: &Rect) -> Rect {
        let (x, y) = self.to_screen(vp, (r.x, r.y));
        Rect::new(x, y, r.w * self.zoom, r.h * self.zoom)
    }
    pub fn visible_world(&self, vp: &Rect) -> Rect {
        Rect::new(self.x, self.y, vp.w / self.zoom, vp.h / self.zoom)
    }
    /// Drag by a screen-space delta.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.x -= dx / self.zoom;
        self.y -= dy / self.zoom;
    }
    /// Multiplies the zoom, keeping the world point under `at` (screen) fixed.
    pub fn zoom_at(&mut self, vp: &Rect, at: (f32, f32), factor: f32) {
        let wx = self.x + (at.0 - vp.x) / self.zoom;
        let wy = self.y + (at.1 - vp.y) / self.zoom;
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.x = wx - (at.0 - vp.x) / self.zoom;
        self.y = wy - (at.1 - vp.y) / self.zoom;
    }
}

/// Initial view: the first group at zoom 0.6.
pub fn initial_camera() -> Camera {
    Camera { x: -40.0, y: -40.0, zoom: 0.6 }
}

/// Camera path for the measurement: a Lissajous pan across the world with the
/// zoom oscillating (log-space) between 0.12 and 1.4, period 2.5 s.
pub fn animated_camera(t: f64, world: &Rect, vp: &Rect) -> Camera {
    let t = t as f32;
    let zoom = (0.12f32.ln() + (1.4f32.ln() - 0.12f32.ln()) * (0.5 + 0.5 * (t * std::f32::consts::TAU / 2.5).sin())).exp();
    let cx = world.x + world.w * (0.5 + 0.45 * (t * 0.7).sin());
    let cy = world.y + world.h * (0.5 + 0.45 * (t * 0.53).cos());
    Camera { x: cx - vp.w / zoom / 2.0, y: cy - vp.h / zoom / 2.0, zoom }
}

/// On-screen label size, quantized to 0.25 px so continuous zoom does not
/// rasterize a new glyph size every frame.
pub fn text_px(base: f32, zoom: f32) -> f32 {
    (base * zoom * 4.0).round() / 4.0
}

pub fn rgb_f32(c: u32) -> [f32; 3] {
    [((c >> 16) & 0xff) as f32 / 255.0, ((c >> 8) & 0xff) as f32 / 255.0, (c & 0xff) as f32 / 255.0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_has_the_spike_sizes() {
        let scene = build();
        assert_eq!(scene.groups.len(), 30);
        assert_eq!(scene.sessions.len(), 300);
        assert_eq!(scene.agents.len(), 3000);
        assert_eq!(scene.edges.iter().filter(|e| e.dashed).count(), 600 + 30, "2 dashed subagent links per session + 1 subsession link per group");
        assert!(scene.edges.iter().any(|e| !e.axis_aligned()));
    }

    #[test]
    fn zoom_keeps_the_point_under_the_cursor() {
        let vp = Rect::new(10.0, 20.0, 800.0, 600.0);
        let mut cam = Camera { x: 100.0, y: 50.0, zoom: 0.5 };
        let at = (300.0, 200.0);
        let before = (cam.x + (at.0 - vp.x) / cam.zoom, cam.y + (at.1 - vp.y) / cam.zoom);
        cam.zoom_at(&vp, at, 1.7);
        let after = (cam.x + (at.0 - vp.x) / cam.zoom, cam.y + (at.1 - vp.y) / cam.zoom);
        assert!((before.0 - after.0).abs() < 1e-3 && (before.1 - after.1).abs() < 1e-3);
    }
}
