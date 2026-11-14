//! Canvas visuals: dust on unused sessions, decks beneath sessions (stacks
//! of subsessions and folded subagent boxes) and hiding subsessions (`H`)
//! and subagent boxes (`J`) (with reversible animations in the GPU client),
//! and the effects collection that passive sessions put on boxes. This module holds
//! the client state, the actions, the canvas click hook and what the canvas
//! renderers need ([`VisualFrame`]).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use canvas_model::visual::{self as vm, Dust};
use ratatui::style::{Color, Modifier, Style};

use crate::app::{App, CanvasHit, Drag};
use crate::dialogs::{Action, MenuItem};
use crate::doc::{Node, NodeKind};
use crate::effects::Effect;
use crate::input::MouseButton;
use crate::paths::now_ms;
use crate::status::Status;
use crate::view::{Motion, RenderMode, WRect};

/// Length of a deck or hide transition (GUI).
const ANIM_MS: f32 = 300.0;
/// A deck's menu offers to hide at most this many of its subagent boxes.
const DECK_HIDE_ITEMS: usize = 6;
/// How often `last_used_ms` is brought up to date.
const USED_EVERY: Duration = Duration::from_secs(5);
/// Boxes without a recorded use start from their history this long after it arrived.
const HISTORY_SETTLE: Duration = Duration::from_secs(10);

/// Dusty box fill per level (light, medium, heavy): the panel colour drained
/// of its blue, a little warmer as dust settles.
pub const DUST_FILL: [Color; 3] = [Color::Rgb(26, 28, 30), Color::Rgb(29, 29, 28), Color::Rgb(33, 31, 27)];
/// Dusty outline per level.
pub const DUST_EDGE: [Color; 3] = [Color::Rgb(74, 77, 80), Color::Rgb(70, 68, 63), Color::Rgb(66, 61, 52)];

/// What a click on the canvas, a menu or a key asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum VisualAction {
    /// Stack (`true`) or unstack a session's subsessions.
    Stack(String, bool),
    /// Fold (`true`) a session's subagent boxes into its deck, or show them.
    FoldSubagents(String, bool),
    /// Fold (`true`) or unfold everything of a session: its subagent boxes
    /// and its subsessions.
    FoldAll(String, bool),
    /// A click on a subagent deck: its session's details with the subagents.
    ViewSubagents(String),
    /// K: unfold the selected session's deck when anything is folded, else
    /// fold its subagent boxes and subsessions.
    ToggleStack,
    /// H: hide or show subsessions (with their subagent boxes) on this window's canvas.
    ToggleHideSubsessions,
    /// J: hide or show every subagent box on this window's canvas.
    ToggleHideSubagents,
    /// Hides these subsessions and subagent boxes on their own (the canvas's
    /// hidden area); other boxes are left alone.
    Hide(Vec<String>),
    /// Shows these boxes of the hidden area again, where they were.
    Show(Vec<String>),
    /// The list of hidden boxes: one box's (its marker, its menu) or the
    /// whole canvas's grouped by parent (the toolbar's counters; `true`:
    /// the subagent counter, its switch first).
    HiddenList(Option<String>, bool),
    /// Select the dusty session boxes (cleanup: Del removes them from the canvas).
    SelectUnused,
    /// Clears the effects on a box (node id) or a group (`group:<id>`).
    AckEffects(String),
    /// The panel of the group's passive sessions.
    GroupPassives(String),
    /// A menu of the group's folders.
    GroupFolders(String),
    /// A folder of the service's machine in the canvas's browser (remote mode).
    Browse(String),
    /// Opens a passive session's file: 4 group context, 5 GOALS.md.
    PassiveFile(String, u8),
}

/// Settings rows of this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualSetting {
    /// Settings › Dust unused sessions after (canvas).
    Dust,
    /// Settings › Subagents: stacked or shown (canvas).
    Subagents,
    /// Settings › Effects (this machine).
    Effects,
    /// Settings › Working glow around working sessions (this machine).
    WorkingGlow,
}

/// A deck or hide transition: its value runs from `from` towards 1
/// (`closing`: stacking, folding, hiding) or 0 over [`ANIM_MS`].
#[derive(Clone, Copy, Debug)]
pub struct Transition {
    pub closing: bool,
    pub start: Instant,
    pub from: f32,
}

impl Transition {
    pub fn value_at(&self, now: Instant) -> f32 {
        let step = now.saturating_duration_since(self.start).as_secs_f32() * 1000.0 / ANIM_MS;
        if self.closing { (self.from + step).min(1.0) } else { (self.from - step).max(0.0) }
    }

    pub fn done_at(&self, now: Instant) -> bool {
        let value = self.value_at(now);
        if self.closing { value >= 1.0 } else { value <= 0.0 }
    }
}

/// Ease in and out (cubic).
pub fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

fn lerp(a: WRect, b: WRect, t: f32) -> WRect {
    WRect::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t, a.w + (b.w - a.w) * t, a.h + (b.h - a.h) * t)
}

/// The client's visual state.
#[derive(Default)]
pub struct VisualState {
    /// A fixed clock (ms) for tests and snapshots; `None`: the system clock.
    pub clock_ms: Option<u64>,
    /// How effects (and transitions) are drawn; `None` until loaded.
    pub motion: Option<Motion>,
    stacks: HashMap<String, Transition>,
    known_stacks: Option<HashSet<String>>,
    /// Sessions whose subagent boxes fold or unfold, and the sessions whose
    /// subagent boxes were folded at the last frame.
    folds: HashMap<String, Transition>,
    known_folds: Option<HashSet<String>>,
    /// Boxes being hidden or shown, each with the box it shrinks into; the
    /// hidden boxes, the boxes in a deck and every box at the last frame.
    hides: HashMap<String, (Transition, String)>,
    known_hidden: Option<HashMap<String, String>>,
    known_decked: HashSet<String>,
    known_ids: HashSet<String>,
    /// Effects acknowledged here until the service's snapshot drops them.
    pub acked: HashSet<String>,
    /// When this client first showed each effect (its animation's start).
    seen: HashMap<String, Instant>,
    used_at: Option<Instant>,
    /// When the session history first arrived (old boxes migrate a little later,
    /// once the summaries of every tracked session are in).
    pub(crate) history_at: Option<Instant>,
    effect_frame: u64,
    /// An effect is animating in view (the GUI keeps ticking).
    pub effects_live: bool,
    /// Loops that start or stop (their arcs grow in or fade out).
    pub loops: crate::loops::LoopState,
    /// The working glow's fades in and out, by box.
    pub glow: crate::glow::Fades,
    /// *Settings › Working glow* on this machine (stored as `grid_pulse`);
    /// `None` until loaded.
    pub grid_pulse: Option<bool>,
}

impl VisualState {
    /// A deck or hide transition is running (the GUI draws every frame).
    pub fn animating(&self) -> bool {
        !self.stacks.is_empty() || !self.folds.is_empty() || !self.hides.is_empty()
    }

    /// How long ago the session history first arrived.
    pub fn history_since(&self) -> Option<Duration> {
        self.history_at.map(|t| t.elapsed())
    }
}

/// Why a box is not drawn where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conceal {
    /// In the deck beneath this session (stacked or folded).
    Stack(String),
    /// Hidden into this visible ancestor (`H`, `J`).
    Hidden(String),
}

impl Conceal {
    pub fn root(&self) -> &str {
        match self {
            Conceal::Stack(root) | Conceal::Hidden(root) => root,
        }
    }
}

/// A deck beneath its session: its stacked subsessions and/or its folded
/// subagent boxes.
#[derive(Clone, Debug)]
pub struct Deck {
    pub root: String,
    /// The top card: the newest stacked session, else the newest subagent box.
    pub top: String,
    /// Stacked sessions, oldest first.
    pub sessions: Vec<String>,
    /// The session's own folded subagent boxes, oldest first.
    pub agents: Vec<String>,
    /// Everything it holds (its status and effects come from these).
    pub members: Vec<String>,
    /// The most important state of what it holds (`App::aggregate_status`).
    pub status: Option<Status>,
    /// Running subagent boxes among them.
    pub running: usize,
    /// The top card (world units).
    pub rect: WRect,
    /// Opacity (fades in while folding).
    pub alpha: f32,
}

impl Deck {
    /// Cards drawn behind the top one.
    pub fn back(&self) -> u8 {
        vm::deck_back(self.sessions.len() + self.agents.len()) as u8
    }

    /// The deck with its back cards (they peek out right and below).
    pub fn bounds(&self) -> WRect {
        let back = self.back() as f32;
        WRect::new(self.rect.x, self.rect.y, self.rect.w + back, self.rect.h + back)
    }

    /// Only folded subagent boxes (a slim card; a click lists them).
    pub fn agents_only(&self) -> bool {
        self.sessions.is_empty()
    }
}

/// What the canvas shows differently this frame.
#[derive(Default)]
pub struct VisualFrame {
    /// Boxes in a deck or hidden (target state: no clicks reach them).
    pub concealed: HashMap<String, Conceal>,
    /// GPU: boxes in a transition, with their rect and opacity this frame.
    pub moving: HashMap<String, (WRect, f32)>,
    pub decks: Vec<Deck>,
    /// Visible ancestors of hidden subsessions and subagent boxes → those boxes.
    pub hidden: HashMap<String, Vec<String>>,
    /// Their badges: how many of each kind and the most important state.
    pub hidden_badges: HashMap<String, crate::view::Hidden>,
}

impl VisualFrame {
    /// Not drawn this frame (concealed and not animating).
    pub fn skip(&self, id: &str) -> bool {
        self.concealed.contains_key(id) && !self.moving.contains_key(id)
    }

    pub fn deck_of(&self, root: &str) -> Option<&Deck> {
        self.decks.iter().find(|d| d.root == root)
    }
}

/// How a dusty box differs.
#[derive(Clone, Debug, PartialEq)]
pub struct DustLook {
    pub dust: Dust,
    pub fill: Color,
    pub edge: Style,
    /// " · unused 12d"
    pub suffix: String,
}

/// The system's animation setting (Windows "Show animations", macOS and
/// Linux "reduce motion" turned off).
pub use canvas_sys::host::system_animations;

/// Settings › Effects as stored ("on", "reduced", "off"; `None`: follow Windows).
pub fn motion_from(setting: Option<&str>, animations: bool) -> Motion {
    match setting {
        Some("on") => Motion::On,
        Some("reduced") => Motion::Reduced,
        Some("off") => Motion::Off,
        _ if animations => Motion::On,
        _ => Motion::Reduced,
    }
}

fn item(label: &str, hint: &str, action: VisualAction, enabled: bool) -> MenuItem {
    MenuItem { label: label.to_owned(), hint: hint.to_owned(), action: Action::Visual(action), enabled }
}

/// The `H` item's label (subsessions and what is under them).
pub fn hide_subsessions_label(hidden: bool) -> &'static str {
    if hidden { "Show subsessions" } else { "Hide subsessions" }
}

/// The `J` item's label (every subagent box).
pub fn hide_subagents_label(hidden: bool) -> &'static str {
    if hidden { "Show subagents" } else { "Hide subagents" }
}

/// "2 subsession(s) and 3 subagent box(es)", without a zero part.
fn kinds_text(sessions: usize, agents: usize) -> String {
    match (sessions, agents) {
        (s, 0) => format!("{s} subsession(s)"),
        (0, a) => format!("{a} subagent box(es)"),
        (s, a) => format!("{s} subsession(s) and {a} subagent box(es)"),
    }
}

/// How much a box's state matters on a deck or a hidden-box badge (0: not
/// at all): needs you, failed, done · review, working, a subagent that just
/// finished (a running one outranks it).
fn status_rank(node: &Node, status: Status) -> u8 {
    match status {
        Status::Waiting => 5,
        Status::Failed => 4,
        Status::JustFinished if node.kind != NodeKind::Agent => 3,
        Status::Working => 2,
        Status::JustFinished => 1,
        _ => 0,
    }
}

/// A hidden-box badge's colour: the attention colour of its most important
/// state, vermillion for a failure, else none (neutral).
fn badge_color(status: Status) -> Option<Color> {
    use crate::status::StatusStyle;
    status.attention().map(|(c, _)| c).or((status == Status::Failed).then_some(crate::paint::NEGATIVE))
}

impl App {
    /// The clock dust is measured with.
    pub fn visual_now(&self) -> u64 {
        self.visual.clock_ms.unwrap_or_else(now_ms)
    }

    /// How effects are drawn here (Settings › Effects; Windows' setting by default).
    pub fn motion(&self) -> Motion {
        self.visual.motion.unwrap_or_else(|| motion_from(None, system_animations()))
    }

    fn settings_home(&self) -> Option<std::path::PathBuf> {
        self.instance.as_ref().map(|i| i.home.clone())
    }

    fn load_motion(&mut self) {
        let settings = self.settings_home().map(|home| crate::settings::Settings::load(&home)).unwrap_or_default();
        self.visual.motion = Some(motion_from(settings.effects().as_deref(), system_animations()));
    }

    fn load_grid_pulse(&mut self) {
        let settings = self.settings_home().map(|home| crate::settings::Settings::load(&home)).unwrap_or_default();
        self.visual.grid_pulse = Some(settings.grid_pulse());
    }

    // ---------- dust ----------

    /// The dust of a session box: idle or stopped, unused for days, with
    /// nothing important going on (working, needs you, done · review,
    /// failed, open elsewhere, looping, an effect: never dusty).
    pub fn dust_of(&self, node: &Node, status: Status) -> Option<(Dust, u64)> {
        if node.kind != NodeKind::Session || node.passive.is_some() || self.doc.dust_after_days == 0 {
            return None;
        }
        let last = node.last_used_ms?;
        if !matches!(status, Status::Idle | Status::Stopped | Status::Unknown) {
            return None;
        }
        let dust = vm::dust_for_age(self.visual_now().saturating_sub(last), self.doc.dust_after_days);
        if dust == Dust::None || self.live_elsewhere(node) || self.node_loop(node).is_some() || self.has_effect(&node.id) {
            return None;
        }
        Some((dust, last))
    }

    /// The dusty look of a box (fill, outline, age text), if it is dusty.
    pub fn dust_look(&self, node: &Node, status: Status) -> Option<DustLook> {
        let (dust, last) = self.dust_of(node, status)?;
        let i = (dust.level() as usize).clamp(1, 3) - 1;
        Some(DustLook {
            dust,
            fill: DUST_FILL[i],
            edge: Style::default().fg(DUST_EDGE[i]),
            suffix: format!(" · {}", vm::unused_text(last, self.visual_now())),
        })
    }

    /// The dusty session boxes that are drawn (for Select unused sessions).
    pub fn dusty_sessions(&mut self) -> Vec<String> {
        let frame = self.visual_frame();
        self.doc.nodes.iter().filter(|n| !frame.concealed.contains_key(&n.id) && self.dust_of(n, self.node_status(n)).is_some()).map(|n| n.id.clone()).collect()
    }

    /// Records when sessions were last used: started, typed into, terminal
    /// output, history events (looking at them does not count). Boxes from
    /// before this was recorded start from their history (or when you last
    /// looked at them).
    pub fn sync_last_used(&mut self) {
        let now = now_ms();
        let mut changes = Vec::new();
        for node in self.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session && n.passive.is_none()) {
            let typed = self.last_input_at.get(&node.id).map(|t| now.saturating_sub(t.elapsed().as_millis() as u64)).unwrap_or(0);
            let history = self.summary(node).map(|s| s.last_event_ms.max(s.updated_ms)).unwrap_or(0);
            let term = self.terms.get(&node.id).map(|t| t.started_ms.max(t.last_output_ms)).unwrap_or(0);
            let seen = typed.max(history).max(term).min(now);
            let next = match node.last_used_ms {
                // Wait for the history before the first value.
                None if !self.demo && !self.visual.history_at.is_some_and(|t| t.elapsed() >= HISTORY_SETTLE) => None,
                None => vm::migrated_last_used(node, seen),
                recorded => vm::next_last_used(recorded, seen),
            };
            if let Some(at) = next {
                changes.push((node.id.clone(), at));
            }
        }
        if changes.is_empty() {
            return;
        }
        for (id, at) in changes {
            if let Some(node) = self.doc.node_mut(&id) {
                node.last_used_ms = Some(at);
            }
        }
        self.mark_dirty();
    }

    // ---------- decks and hidden boxes ----------

    /// Where a session's deck is drawn as the document and this window's
    /// switches stand: a stack of subsessions beneath it and its shown
    /// subagent boxes, else a slim card for its folded subagent boxes
    /// (`canvas_model::visual::deck_rect`).
    pub fn deck_rect(&self, root: &Node) -> WRect {
        self.deck_rect_in(root, &self.conceal_maps().0)
    }

    /// [`App::deck_rect`] with the decks' boxes (`under`, from [`App::conceal_maps`]).
    pub(crate) fn deck_rect_in(&self, root: &Node, under: &HashMap<String, String>) -> WRect {
        let sessions = root.stacked && vm::descendants(&self.doc, &root.id).iter().any(|id| under.get(id) == Some(&root.id));
        WRect::from(vm::deck_rect(&self.doc, root, sessions))
    }

    /// The boxes in a deck (id → the session it lies beneath) and the hidden
    /// ones (id → the box whose marker counts them), as the document and
    /// this window's switches (`H`, `J`) stand. A hidden box is never in a
    /// deck; one hidden into a box held by a deck counts on the deck's session.
    pub fn conceal_maps(&self) -> (HashMap<String, String>, HashMap<String, String>) {
        let mut under = vm::stacked_under(&self.doc);
        let mut hidden = vm::hidden_boxes(&self.doc, self.doc.ui.hide_subsessions, self.doc.ui.hide_subagents);
        if !hidden.is_empty() {
            under.retain(|id, _| !hidden.contains_key(id));
            for top in hidden.values_mut() {
                if let Some(root) = under.get(top.as_str()) {
                    *top = root.clone();
                }
            }
        }
        (under, hidden)
    }

    /// Starts transitions for stacks, folded subagent boxes and hidden boxes
    /// that changed since the last frame (also when another window changed
    /// them); returns [`App::conceal_maps`].
    fn sync_transitions(&mut self) -> (HashMap<String, String>, HashMap<String, String>) {
        let animate = self.render_mode == RenderMode::Gpu && self.motion() == Motion::On;
        let now = Instant::now();
        let stacked: HashSet<String> = self.doc.nodes.iter().filter(|n| n.stacked).map(|n| n.id.clone()).collect();
        if animate && let Some(known) = &self.visual.known_stacks {
            let changes: Vec<(String, bool)> = stacked.difference(known).map(|id| (id.clone(), true)).chain(known.difference(&stacked).filter(|id| self.doc.node(id).is_some()).map(|id| (id.clone(), false))).collect();
            for (id, closing) in changes {
                let from = self.visual.stacks.get(&id).map(|t| t.value_at(now)).unwrap_or(if closing { 0.0 } else { 1.0 });
                self.visual.stacks.insert(id, Transition { closing, start: now, from });
            }
        }
        self.visual.known_stacks = Some(stacked);
        self.visual.stacks.retain(|id, t| !t.done_at(now) && self.doc.node(id).is_some());
        // Every session's fold state (a session's first subagent box arrives folded without a transition).
        let folded: HashSet<String> = self.doc.nodes.iter().filter(|n| vm::subagents_folded(&self.doc, n)).map(|n| n.id.clone()).collect();
        if animate && let Some(known) = &self.visual.known_folds {
            let changes: Vec<(String, bool)> = folded.difference(known).map(|id| (id.clone(), true)).chain(known.difference(&folded).filter(|id| self.doc.node(id).is_some()).map(|id| (id.clone(), false))).collect();
            for (id, closing) in changes {
                if vm::subagents_of(&self.doc, &id).is_empty() {
                    continue;
                }
                let from = self.visual.folds.get(&id).map(|t| t.value_at(now)).unwrap_or(if closing { 0.0 } else { 1.0 });
                self.visual.folds.insert(id, Transition { closing, start: now, from });
            }
        }
        self.visual.known_folds = Some(folded);
        self.visual.folds.retain(|id, t| !t.done_at(now) && self.doc.node(id).is_some());
        // Hidden boxes shrink into the box that counts them (and grow out of
        // it); boxes that were or are in a deck, and new ones, switch at once.
        let (under, hidden) = self.conceal_maps();
        let changes: Vec<(String, bool, String)> = match &self.visual.known_hidden {
            Some(known) if animate => {
                let visual = &self.visual;
                let hiding = hidden.iter().filter(|(id, _)| !known.contains_key(*id) && !visual.known_decked.contains(*id) && visual.known_ids.contains(*id)).map(|(id, top)| (id.clone(), true, top.clone()));
                let showing = known.iter().filter(|(id, _)| !hidden.contains_key(*id) && !under.contains_key(*id) && self.doc.node(id).is_some()).map(|(id, top)| (id.clone(), false, top.clone()));
                hiding.chain(showing).collect()
            }
            _ => Vec::new(),
        };
        for (id, closing, top) in changes {
            let from = self.visual.hides.get(&id).map(|(t, _)| t.value_at(now)).unwrap_or(if closing { 0.0 } else { 1.0 });
            self.visual.hides.insert(id, (Transition { closing, start: now, from }, top));
        }
        self.visual.known_hidden = Some(hidden.clone());
        self.visual.known_decked = under.keys().cloned().collect();
        if animate {
            self.visual.known_ids = self.doc.nodes.iter().map(|n| n.id.clone()).collect();
        }
        self.visual.hides.retain(|id, (t, _)| !t.done_at(now) && self.doc.node(id).is_some());
        (under, hidden)
    }

    /// Boxes in a deck or hidden right now (no transitions): clicks and
    /// rubber bands pass them by.
    pub fn concealed_now(&self) -> HashMap<String, Conceal> {
        let (under, hidden) = self.conceal_maps();
        let mut out: HashMap<String, Conceal> = under.into_iter().map(|(id, root)| (id, Conceal::Stack(root))).collect();
        out.extend(hidden.into_iter().map(|(id, top)| (id, Conceal::Hidden(top))));
        out
    }

    /// Where a box in a deck or hidden shows: its deck or its visible
    /// ancestor (`None`: it shows itself).
    pub fn shown_center(&self, id: &str) -> Option<(i32, i32)> {
        let (under, hidden) = self.conceal_maps();
        if let Some(top) = hidden.get(id) {
            return Some(self.doc.node(top)?.rect().center());
        }
        let deck = self.deck_rect_in(self.doc.node(under.get(id)?)?, &under);
        Some(((deck.x + deck.w / 2.0) as i32, (deck.y + deck.h / 2.0) as i32))
    }

    /// What the canvas draws differently this frame: concealed boxes, decks,
    /// hidden counts, and (GUI) boxes in a transition.
    pub fn visual_frame(&mut self) -> VisualFrame {
        let (under, hidden) = self.sync_transitions();
        let mut frame = VisualFrame::default();
        let moving = !self.visual.stacks.is_empty() || !self.visual.folds.is_empty();
        if under.is_empty() && hidden.is_empty() && !moving && self.visual.hides.is_empty() {
            return frame;
        }
        let now = Instant::now();
        for (id, root) in &under {
            frame.concealed.insert(id.clone(), Conceal::Stack(root.clone()));
        }
        for node in self.doc.nodes.iter().filter(|n| hidden.contains_key(&n.id)) {
            let top = &hidden[&node.id];
            frame.concealed.insert(node.id.clone(), Conceal::Hidden(top.clone()));
            frame.hidden.entry(top.clone()).or_default().push(node.id.clone());
        }
        // Decks: sessions with something folded (and those still folding or
        // unfolding), unless an outer deck holds them or they are hidden. A
        // deck shows what is not hidden: one whose boxes are all hidden goes.
        let mut roots: Vec<String> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for node in &self.doc.nodes {
            if let Some(root) = under.get(&node.id).filter(|r| seen.insert(r.as_str())) {
                roots.push(root.clone());
            }
        }
        for id in self.visual.stacks.keys().chain(self.visual.folds.keys()) {
            if !roots.contains(id) {
                roots.push(id.clone());
            }
        }
        let by_id: HashMap<&str, &Node> = self.doc.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let mut held: HashMap<String, vm::DeckContents> = vm::decks_of(&self.doc, &under).into_iter().map(|d| (d.root.clone(), d)).collect();
        let shown = |ids: Vec<String>| -> Vec<String> { ids.into_iter().filter(|id| !hidden.contains_key(id)).collect() };
        for root_id in roots {
            if under.contains_key(&root_id) || hidden.contains_key(&root_id) {
                continue;
            }
            let Some(root) = by_id.get(root_id.as_str()).copied() else { continue };
            let moving = self.visual.stacks.contains_key(&root_id) || self.visual.folds.contains_key(&root_id);
            if !moving {
                // At rest: what the document says it holds.
                let Some(c) = held.remove(&root_id) else { continue };
                let Some(top) = c.sessions.last().or(c.agents.last()).cloned() else { continue };
                let rect = WRect::from(vm::deck_rect(&self.doc, root, !c.sessions.is_empty()));
                frame.decks.push(Deck { root: root_id, top, sessions: c.sessions, agents: c.agents, members: c.members, status: None, running: 0, rect, alpha: 1.0 });
                continue;
            }
            let stacked = self.visual.stacks.get(&root_id).map(|t| t.value_at(now)).unwrap_or(if root.stacked { 1.0 } else { 0.0 });
            let folded = self.visual.folds.get(&root_id).map(|t| t.value_at(now)).unwrap_or(if vm::subagents_folded(&self.doc, root) { 1.0 } else { 0.0 });
            let sessions = if stacked > 0.0 { shown(vm::descendants(&self.doc, &root_id)) } else { Vec::new() };
            let agents = if folded > 0.0 { shown(vm::subagents_of(&self.doc, &root_id)) } else { Vec::new() };
            let Some(top) = sessions.last().or(agents.last()).cloned() else { continue };
            let rect = WRect::from(vm::deck_rect(&self.doc, root, !sessions.is_empty()));
            // Cards fly between their own places and the deck.
            let mut fly = |ids: &[String], value: f32| {
                for id in ids {
                    if let Some(node) = by_id.get(id.as_str()) {
                        let home = WRect::from(node.rect());
                        let target = WRect::new(rect.x, rect.y, home.w, home.h);
                        frame.moving.insert(id.clone(), (lerp(home, target, ease(value)), 1.0 - ease(value)));
                    }
                }
            };
            let mut members = if sessions.is_empty() { Vec::new() } else { shown(vm::stack_members(&self.doc, &root_id)) };
            if stacked < 1.0 {
                fly(&members, stacked);
            }
            if folded < 1.0 {
                fly(&agents, folded);
            }
            members.extend(agents.iter().cloned());
            let value = if sessions.is_empty() { folded } else if agents.is_empty() { stacked } else { stacked.max(folded) };
            let alpha = ease(value);
            if alpha > 0.0 {
                frame.decks.push(Deck { root: root_id, top, sessions, agents, members, status: None, running: 0, rect, alpha });
            }
        }
        self.deck_and_hidden_states(&mut frame, &hidden);
        // Boxes being hidden shrink into the box that counts them; shown ones grow out of it.
        for (id, (t, top)) in &self.visual.hides {
            let (Some(node), Some(top)) = (by_id.get(id.as_str()), by_id.get(top.as_str())) else { continue };
            if under.contains_key(id) {
                continue;
            }
            let value = ease(t.value_at(now));
            let home = WRect::from(node.rect());
            let anchor = WRect::from(top.rect());
            let (cx, cy) = (anchor.x + anchor.w / 2.0, anchor.y + anchor.h / 2.0);
            let small = WRect::new(cx - home.w * 0.2, cy - home.h * 0.2, home.w * 0.4, home.h * 0.4);
            frame.moving.insert(id.clone(), (lerp(home, small, value), 1.0 - value));
        }
        frame
    }

    /// The most important status among boxes (a deck, hidden boxes): needs
    /// you, failed, done · review, working, a subagent that just finished,
    /// else `None`. A running subagent outranks one that just finished.
    pub fn aggregate_status(&self, ids: &[String]) -> Option<Status> {
        ids.iter()
            .filter_map(|id| self.doc.node(id))
            .map(|n| {
                let status = self.node_status(n);
                (status_rank(n, status), status)
            })
            .filter(|(r, _)| *r > 0)
            .max_by_key(|(r, _)| *r)
            .map(|(_, s)| s)
    }

    /// Fills in the state of what each deck holds (`aggregate_status`, and
    /// how many of its subagent boxes run) and the badge of each visible
    /// ancestor's hidden boxes, in one pass over the boxes.
    fn deck_and_hidden_states(&self, frame: &mut VisualFrame, hidden: &HashMap<String, String>) {
        let hiding = !frame.hidden.is_empty();
        if frame.decks.is_empty() && !hiding {
            return;
        }
        let in_deck: HashMap<&str, usize> = frame.decks.iter().enumerate().flat_map(|(i, d)| d.members.iter().map(move |m| (m.as_str(), i))).collect();
        let mut decks: Vec<(u8, Option<Status>, usize)> = vec![(0, None, 0); frame.decks.len()];
        // Visible ancestor → (sessions, subagent boxes, rank, status).
        let mut badges: HashMap<String, (u32, u32, u8, Option<Status>)> = HashMap::new();
        for node in &self.doc.nodes {
            let deck = in_deck.get(node.id.as_str()).copied();
            let top = if hiding { hidden.get(&node.id) } else { None };
            if deck.is_none() && top.is_none() {
                continue;
            }
            let status = self.node_status(node);
            let rank = status_rank(node, status);
            if let Some(i) = deck {
                let d = &mut decks[i];
                if rank > d.0 {
                    (d.0, d.1) = (rank, Some(status));
                }
                d.2 += usize::from(node.kind == NodeKind::Agent && status == Status::Working);
            }
            if let Some(top) = top {
                let b = badges.entry(top.clone()).or_insert((0, 0, 0, None));
                if node.kind == NodeKind::Agent {
                    b.1 += 1;
                } else {
                    b.0 += 1;
                }
                if rank > b.2 {
                    (b.2, b.3) = (rank, Some(status));
                }
            }
        }
        for (deck, (_, status, running)) in frame.decks.iter_mut().zip(decks) {
            deck.status = status;
            deck.running = running;
        }
        frame.hidden_badges = badges
            .into_iter()
            .map(|(top, (sessions, agents, _, status))| (top, crate::view::Hidden { sessions, agents, color: status.and_then(badge_color) }))
            .collect();
    }

    /// Stacks or unstacks a session's subsessions (their positions stay).
    pub fn set_stacked(&mut self, root: &str, stacked: bool) {
        let count = vm::descendants(&self.doc, root).len();
        let Some(node) = self.doc.node_mut(root) else { return };
        if stacked && count == 0 {
            node.stacked = false;
            return self.notify("This session has no subsessions to stack.", false);
        }
        if node.stacked == stacked {
            return;
        }
        node.stacked = stacked;
        let name = node.name.clone();
        self.after_fold(stacked);
        if stacked {
            self.notify(format!("Stacked {count} subsession(s) under “{name}”. Double-click the stack or press K to unstack."), false);
        } else {
            self.notify(format!("Unstacked {count} subsession(s) of “{name}”."), false);
        }
    }

    /// Folds a session's subagent boxes into its deck or shows them (its own
    /// choice, kept in the canvas). Their positions stay; a box that never
    /// had a place of its own gets one when it is first shown.
    pub fn set_subagents_folded(&mut self, root: &str, folded: bool) {
        let count = vm::subagents_of(&self.doc, root).len();
        let Some(node) = self.doc.node(root).filter(|n| n.kind == NodeKind::Session) else { return };
        if count == 0 {
            return self.notify("This session has no subagent boxes.", false);
        }
        let (was, name) = (vm::subagents_folded(&self.doc, node), node.name.clone());
        if let Some(node) = self.doc.node_mut(root) {
            node.subagents_shown = Some(!folded);
        }
        self.after_fold(folded);
        if was == folded {
            return;
        }
        if folded {
            self.notify(format!("Stacked {count} subagent box(es) under “{name}”. Double-click the deck or press K to show them."), false);
        } else {
            self.notify(format!("Showing {count} subagent box(es) of “{name}”; K stacks them again."), false);
        }
    }

    /// Folds (`true`) or unfolds a session's subagent boxes and its
    /// subsessions together (K, a double-click on its deck).
    pub fn set_all_folded(&mut self, root: &str, folded: bool) {
        let agents = vm::subagents_of(&self.doc, root).len();
        let sessions = vm::descendants(&self.doc, root).len();
        let Some(node) = self.doc.node_mut(root).filter(|n| n.kind == NodeKind::Session) else { return };
        if agents == 0 && sessions == 0 {
            return self.notify("This session has no subagent boxes or subsessions to stack (K).", false);
        }
        if agents > 0 {
            node.subagents_shown = Some(!folded);
        }
        if sessions > 0 {
            node.stacked = folded;
        }
        let name = node.name.clone();
        self.after_fold(folded);
        let mut parts = Vec::new();
        if agents > 0 {
            parts.push(format!("{agents} subagent box(es)"));
        }
        if sessions > 0 {
            parts.push(format!("{sessions} subsession(s)"));
        }
        let what = parts.join(" and ");
        if folded {
            self.notify(format!("Stacked {what} under “{name}”. Double-click the deck or press K to unfold."), false);
        } else {
            self.notify(format!("Unfolded {what} of “{name}”; K stacks them again."), false);
        }
    }

    /// After folding or unfolding: boxes now in a deck leave the selection
    /// (their place follows their session), shown ones that never had a
    /// place get one.
    fn after_fold(&mut self, folded: bool) {
        if folded {
            let under = vm::stacked_under(&self.doc);
            self.selection.retain(|id| !under.contains_key(id));
        } else {
            self.place_shown_subagents();
        }
        self.mark_dirty();
    }

    /// Something of this session is folded (its subagent boxes, its subsessions).
    pub fn anything_folded(&self, root: &str) -> bool {
        let Some(node) = self.doc.node(root) else { return false };
        let agents = !vm::subagents_of(&self.doc, root).is_empty() && vm::subagents_folded(&self.doc, node);
        agents || (node.stacked && !vm::descendants(&self.doc, root).is_empty())
    }

    /// The session K acts on: the deck that holds the selected box, the
    /// selected session (a subagent box's session) when it has subagent boxes
    /// or subsessions, else its parent.
    fn fold_target(&self) -> Option<String> {
        let id = self.selection.last().cloned()?;
        if let Some(root) = vm::stacked_under(&self.doc).remove(&id) {
            return Some(root);
        }
        let has = |id: &str| !vm::subagents_of(&self.doc, id).is_empty() || !vm::descendants(&self.doc, id).is_empty();
        let session = self.session_of(&id)?;
        if has(&session) {
            return Some(session);
        }
        self.doc.node(&session)?.parent.clone().filter(|p| self.doc.node(p).is_some() && has(p))
    }

    /// K: unfolds everything of the selected session when anything is
    /// folded, else folds its subagent boxes and subsessions.
    pub fn toggle_fold(&mut self) {
        match self.fold_target() {
            Some(root) => {
                let folded = self.anything_folded(&root);
                self.set_all_folded(&root, !folded);
            }
            None => self.notify("Select a session with subagents or subsessions to stack or unstack them (K).", false),
        }
    }

    /// A click on a subagent deck: its session's details panel, which lists
    /// the subagents (viewing never starts anything).
    pub fn view_subagents(&mut self, root: &str) {
        if self.doc.node(root).is_none() {
            return;
        }
        self.select_node(root, false);
        self.right_detail = Some(crate::side::RightDetail::Session(root.to_owned()));
        self.needs_draw = true;
    }

    /// The boxes of `ids` a drag or an arrow key moves: those drawn on their
    /// own (a box in a deck or hidden follows its session).
    pub(crate) fn movable(&self, ids: &[String]) -> Vec<String> {
        let (under, hidden) = self.conceal_maps();
        ids.iter().filter(|id| !under.contains_key(*id) && !hidden.contains_key(*id)).cloned().collect()
    }

    /// H: hides or shows subsessions (with their subagent boxes) on this window's canvas.
    pub fn toggle_hide_subsessions(&mut self) {
        let on = !self.doc.ui.hide_subsessions;
        self.set_hide_switch(true, on);
    }

    /// J: hides or shows every subagent box (folded ones too) on this window's canvas.
    pub fn toggle_hide_subagents(&mut self) {
        let on = !self.doc.ui.hide_subagents;
        self.set_hide_switch(false, on);
    }

    /// Sets `H` (`subsessions`) or `J`: a preference of this window, kept with
    /// its view. Boxes keep their places; hidden ones leave the selection
    /// (their terminals stay open).
    fn set_hide_switch(&mut self, subsessions: bool, on: bool) {
        let before: HashSet<String> = self.conceal_maps().1.into_keys().collect();
        if subsessions {
            self.doc.ui.hide_subsessions = on;
        } else {
            self.doc.ui.hide_subagents = on;
        }
        let after = self.conceal_maps().1;
        let changed: Vec<&String> = if on { after.keys().filter(|id| !before.contains(*id)).collect() } else { before.iter().filter(|id| !after.contains_key(*id)).collect() };
        let agents = changed.iter().filter(|id| self.doc.node(id).is_some_and(|n| n.kind == NodeKind::Agent)).count();
        let what = kinds_text(changed.len() - agents, agents);
        let (key, kind) = if subsessions { ("H", "Subsessions") } else { ("J", "Subagent boxes") };
        self.selection.retain(|id| !after.contains_key(id));
        let text = match (on, changed.is_empty()) {
            (true, false) => format!("Hid {what}; {key} shows them again (the parents' markers and the toolbar count them)."),
            (true, true) => format!("{kind} are hidden ({key} shows them)."),
            (false, false) => format!("Showing {what} again."),
            (false, true) => format!("{kind} are shown ({key} hides them)."),
        };
        self.notify(text, false);
        self.mark_dirty();
    }

    /// A subsession or subagent box: it can be hidden on its own.
    pub fn can_hide(&self, id: &str) -> bool {
        self.doc.node(id).is_some_and(|n| vm::can_hide(&self.doc, n))
    }

    /// Hides boxes on their own or shows them again (`Node::hidden`: the
    /// canvas's hidden area, in the document for every window and client).
    /// Only subsessions and subagent boxes; places are kept, terminals keep
    /// running, nothing starts or stops, and it is no undo step (a toggle,
    /// as stacking is).
    pub fn set_hidden(&mut self, ids: &[String], hidden: bool) {
        let ids: Vec<String> = ids.iter().filter(|id| self.can_hide(id) && self.doc.node(id).is_some_and(|n| n.hidden != hidden)).cloned().collect();
        if ids.is_empty() {
            return;
        }
        for id in &ids {
            if let Some(node) = self.doc.node_mut(id) {
                node.hidden = hidden;
            }
        }
        let name = |app: &App, id: &str| app.doc.node(id).map(|n| n.name.clone()).unwrap_or_default();
        let concealed = self.conceal_maps().1;
        if hidden {
            self.selection.retain(|id| !concealed.contains_key(id));
            let text = match ids.as_slice() {
                [id] => {
                    let top = concealed.get(id).map(|t| name(self, t)).unwrap_or_default();
                    format!("Hid “{}”: the hidden marker on “{top}” lists it (a click there shows it again).", name(self, id))
                }
                _ => format!("Hid {} boxes: their parents' hidden markers list them (a click there shows them again).", ids.len()),
            };
            self.notify(text, false);
        } else {
            // A box shown for the first time since it arrived folded gets a slot of its own.
            self.place_shown_subagents();
            let still = ids.iter().filter(|id| concealed.contains_key(*id)).count();
            let mut text = match ids.as_slice() {
                [id] => format!("Showing “{}” again, where it was.", name(self, id)),
                _ => format!("Showing {} boxes again, where they were.", ids.len()),
            };
            if still > 0 {
                text.push_str(&format!(" {still} still hidden by a switch (H subsessions, J subagents)."));
            }
            if let [id] = ids.as_slice() {
                self.flash = Some((id.clone(), Instant::now()));
            }
            self.notify(text, false);
        }
        self.mark_dirty();
    }

    /// The switch (`true`: H, `false`: J) that hides a box no flag of its
    /// own or of a box above it hides, if one does.
    pub(crate) fn hidden_by_switch(&self, id: &str) -> Option<bool> {
        if vm::hidden_area(&self.doc).contains_key(id) || !self.conceal_maps().1.contains_key(id) {
            return None;
        }
        Some(!vm::hidden_boxes(&self.doc, false, self.doc.ui.hide_subagents).contains_key(id))
    }

    /// A menu's items to hide or show one box (or, on a box of a
    /// multi-selection, the selection's subsessions and subagent boxes) and
    /// to list the boxes hidden into it.
    pub fn hide_items(&self, id: &str) -> Vec<MenuItem> {
        let mut items = Vec::new();
        let Some(node) = self.doc.node(id) else { return items };
        let many = self.selection.len() > 1 && self.selection.iter().any(|s| s == id);
        if node.hidden && self.can_hide(id) {
            items.push(item("Show", "", VisualAction::Show(vec![id.to_owned()]), true));
        } else if many {
            let ids: Vec<String> = self.selection.iter().filter(|s| self.can_hide(s) && self.doc.node(s).is_some_and(|n| !n.hidden)).cloned().collect();
            if !ids.is_empty() {
                items.push(item(&format!("Hide {}", ids.len()), "", VisualAction::Hide(ids), true));
            }
        } else if self.can_hide(id) {
            items.push(item("Hide", "", VisualAction::Hide(vec![id.to_owned()]), true));
        }
        match self.hidden_by_switch(id) {
            Some(true) => items.push(item(hide_subsessions_label(true), "H", VisualAction::ToggleHideSubsessions, true)),
            Some(false) => items.push(item(hide_subagents_label(true), "J", VisualAction::ToggleHideSubagents, true)),
            None => {}
        }
        let count = self.conceal_maps().1.values().filter(|top| *top == id).count();
        if count > 0 {
            items.push(item(&format!("Show hidden ({count})…"), "", VisualAction::HiddenList(Some(id.to_owned()), false), true));
        }
        items
    }

    /// The list of hidden boxes as a menu: the boxes hidden into `parent`
    /// (its marker, its menu), or the whole canvas's grouped by the box that
    /// counts them (the toolbar's counters). Each box hidden on its own has
    /// Show, each group Show all; the switches that hide the rest are there
    /// too (`agents_first`: J's before H's). Nothing here starts anything.
    pub fn hidden_list(&self, parent: Option<&str>, agents_first: bool) -> (String, Vec<MenuItem>) {
        let hidden = self.conceal_maps().1;
        let order: HashMap<&str, usize> = self.doc.nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
        let name = |id: &str| self.doc.node(id).map(|n| crate::paint::truncate(&n.name, 40)).unwrap_or_default();
        // Boxes hidden on their own, by the box that counts them (document order).
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        for node in self.doc.nodes.iter().filter(|n| n.hidden && vm::can_hide(&self.doc, n)) {
            let Some(top) = hidden.get(&node.id).filter(|top| parent.is_none_or(|p| p == top.as_str())) else { continue };
            match groups.iter_mut().find(|(t, _)| t == top) {
                Some((_, ids)) => ids.push(node.id.clone()),
                None => groups.push((top.clone(), vec![node.id.clone()])),
            }
        }
        groups.sort_by_key(|(top, _)| order.get(top.as_str()).copied().unwrap_or(usize::MAX));
        let row = |id: &str, indent: &str| {
            let node = self.doc.node(id);
            let prefix = node.map(crate::ui_canvas::box_prefix).unwrap_or_default();
            let hint = node.map(|n| (n, self.node_status(n))).filter(|(n, s)| status_rank(n, *s) > 0).map(|(n, s)| crate::ui_canvas::box_label(n, s)).unwrap_or_default();
            item(&format!("{indent}Show {prefix}{}", name(id)), hint, VisualAction::Show(vec![id.to_owned()]), true)
        };
        let all: Vec<String> = groups.iter().flat_map(|(_, ids)| ids.iter().cloned()).collect();
        let mut items = Vec::new();
        // The switches: on the canvas's list both, on a box's those that hide some of its boxes.
        let (h, j) = (self.doc.ui.hide_subsessions, self.doc.ui.hide_subagents);
        let (area, without_h) = (vm::hidden_area(&self.doc), vm::hidden_boxes(&self.doc, false, j));
        let counted: Vec<&String> = hidden.keys().filter(|id| !area.contains_key(*id) && parent.is_none_or(|p| hidden.get(*id).is_some_and(|t| t == p))).collect();
        let by_h = h && counted.iter().any(|id| !without_h.contains_key(*id));
        let by_j = j && counted.iter().any(|id| without_h.contains_key(*id));
        let mut switches = vec![
            (parent.is_none() || by_h, item(hide_subsessions_label(h), "H", VisualAction::ToggleHideSubsessions, true)),
            (parent.is_none() || by_j, item(hide_subagents_label(j), "J", VisualAction::ToggleHideSubagents, true)),
        ];
        if agents_first {
            switches.reverse();
        }
        items.extend(switches.into_iter().filter(|(show, _)| *show).map(|(_, item)| item));
        match parent {
            Some(_) => {
                if all.len() > 1 {
                    items.push(item(&format!("Show all ({})", all.len()), "", VisualAction::Show(all.clone()), true));
                }
                items.extend(all.iter().map(|id| row(id, "")));
            }
            None => {
                if all.len() > 1 {
                    items.push(item(&format!("Show all hidden ({})", all.len()), "", VisualAction::Show(all.clone()), true));
                }
                for (top, ids) in &groups {
                    let hint = if ids.len() == 1 { "show it".to_owned() } else { format!("show all {}", ids.len()) };
                    items.push(item(&format!("▸ {}", name(top)), &hint, VisualAction::Show(ids.clone()), true));
                    items.extend(ids.iter().map(|id| row(id, "  ")));
                }
            }
        }
        if items.is_empty() {
            items.push(item("Nothing is hidden here", "", VisualAction::HiddenList(None, false), false));
        }
        let title = match parent {
            Some(p) => format!("Hidden in “{}”", self.doc.node(p).map(|n| crate::paint::truncate(&n.name, 24)).unwrap_or_default()),
            None => "Hidden boxes".to_owned(),
        };
        (title, items)
    }

    // ---------- effects ----------

    /// Effects to show: alive, not acknowledged here, on a box or group that exists.
    pub fn active_effects(&self) -> Vec<&Effect> {
        let now = self.visual_now().max(now_ms());
        self.bus
            .effects
            .iter()
            .filter(|e| e.alive(now) && !self.visual.acked.contains(&e.id))
            .filter(|e| if e.target.is_empty() { self.doc.group(&e.group).is_some() } else { self.doc.node(&e.target).is_some() })
            .collect()
    }

    pub fn has_effect(&self, id: &str) -> bool {
        self.bus.effects.iter().any(|e| e.target == id && !self.visual.acked.contains(&e.id))
    }

    /// Acknowledges effects: gone here at once, and from every client once
    /// the service has them.
    pub fn ack_effects(&mut self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        self.visual.acked.extend(ids.iter().cloned());
        self.bus_action(serde_json::json!({ "action": "effect_ack", "ids": ids }));
        self.needs_draw = true;
    }

    /// Effects on a box or on the boxes of a stack.
    pub(crate) fn effects_on(&self, ids: &[String]) -> Vec<String> {
        self.active_effects().into_iter().filter(|e| ids.contains(&e.target)).map(|e| e.id.clone()).collect()
    }

    /// The world rect an effect is drawn on: its box (or the stack or
    /// visible ancestor that holds it), or its group.
    pub fn effect_anchor(&self, effect: &Effect, frame: &VisualFrame, rect_of: &dyn Fn(&Node) -> WRect) -> Option<(WRect, bool)> {
        if effect.target.is_empty() {
            let group = self.doc.group(&effect.group)?;
            return Some((WRect::from(group.rect()), true));
        }
        let node = self.doc.node(&effect.target)?;
        match frame.concealed.get(&node.id) {
            Some(Conceal::Stack(root)) => frame.deck_of(root).map(|d| (d.rect, false)).or_else(|| self.doc.node(root).map(|r| (rect_of(r), false))),
            Some(Conceal::Hidden(top)) => self.doc.node(top).map(|t| (rect_of(t), false)),
            None => Some((frame.moving.get(&node.id).map(|(r, _)| *r).unwrap_or_else(|| rect_of(node)), false)),
        }
    }

    /// Notes when this client first shows each effect (its animation starts there).
    pub fn prepare_effects(&mut self) {
        let now = Instant::now();
        let ids: Vec<String> = self.active_effects().into_iter().map(|e| e.id.clone()).collect();
        for id in ids {
            self.visual.seen.entry(id).or_insert(now);
        }
    }

    /// Effects as the canvas draws them (after [`App::prepare_effects`]).
    pub fn effect_models(&self, frame: &VisualFrame, rect_of: &dyn Fn(&Node) -> WRect) -> Vec<crate::view::EffectModel> {
        let now = Instant::now();
        let mut out = Vec::new();
        for effect in self.active_effects() {
            let Some((rect, group)) = self.effect_anchor(effect, frame, rect_of) else { continue };
            let seen = self.visual.seen.get(&effect.id).copied().unwrap_or(now);
            let age_ms = match self.frozen_tick {
                // A fixed frame mid-animation (golden screenshots).
                Some(tick) => 1_100 + tick as u64 * 80,
                None => now.saturating_duration_since(seen).as_millis() as u64,
            };
            let kind = effect.spec().map(|k| k.name).unwrap_or("sparkle");
            out.push(crate::view::EffectModel { id: effect.id.clone(), kind, rect, group, badge: effect.badge(), age_ms });
        }
        out
    }

    /// An effect is in view (the GUI animates it): on its box, or on the deck
    /// or visible ancestor that holds the box.
    fn effects_in_view(&self) -> bool {
        let c = self.layout.canvas;
        let effects = self.active_effects();
        if c.width == 0 || effects.is_empty() {
            return false;
        }
        let (x0, y0) = self.to_world_f(c.x as f32, c.y as f32);
        let (x1, y1) = self.to_world_f(c.right() as f32, c.bottom() as f32);
        let concealed = self.concealed_now();
        effects.iter().any(|e| {
            let rect = match concealed.get(&e.target) {
                _ if e.target.is_empty() => self.doc.group(&e.group).map(|g| g.rect()),
                Some(Conceal::Stack(root)) => self.doc.node(root).map(|r| vm::deck_rect(&self.doc, r, r.stacked)),
                Some(Conceal::Hidden(top)) => self.doc.node(top).map(|n| n.rect()),
                None => self.doc.node(&e.target).map(|n| n.rect()),
            };
            rect.is_some_and(|r| (r.x as f32) < x1 + 8.0 && (r.x + r.w) as f32 > x0 - 8.0 && (r.y as f32) < y1 + 6.0 && (r.y + r.h) as f32 > y0 - 6.0)
        })
    }

    // ---------- the canvas click hook ----------

    /// The deck under a cell.
    fn deck_at(&self, frame: &VisualFrame, x: u16, y: u16) -> Option<Deck> {
        frame.decks.iter().rev().find(|deck| {
            let b = deck.bounds();
            if self.render_mode == RenderMode::Gpu {
                let (wx, wy) = self.to_world_f(self.point_in(x, y).0, self.point_in(x, y).1);
                wx >= b.x && wx < b.right() && wy >= b.y && wy < b.bottom()
            } else {
                let (sx, sy) = self.to_screen(b.x as i32, b.y as i32);
                let (w, h) = self.scaled(b.w as i32, b.h as i32, 4, 1);
                (x as i32) >= sx && (x as i32) < sx + w && (y as i32) >= sy && (y as i32) < sy + h
            }
        }).cloned()
    }

    /// The session of the deck under a cell.
    pub(crate) fn deck_root_at(&mut self, x: u16, y: u16) -> Option<String> {
        let frame = self.visual_frame();
        self.deck_at(&frame, x, y).map(|deck| deck.root)
    }

    /// The deck beneath a session, as drawn now.
    pub(crate) fn deck_of(&mut self, root: &str) -> Option<Deck> {
        self.visual_frame().deck_of(root).cloned()
    }

    /// Mouse down on the canvas, before the usual handling: decks (a click
    /// views them: a stack's top card, a subagent deck's list in its
    /// session's details; a double-click unfolds them; a drag moves the
    /// session with its deck; a right-click opens their menu) and effects (a
    /// click acknowledges them and only selects the box: it never starts a
    /// session). True when the click was handled here.
    pub fn visual_mouse_down(&mut self, button: MouseButton, x: u16, y: u16, double: bool, additive: bool) -> bool {
        let frame = self.visual_frame();
        if let Some(deck) = self.deck_at(&frame, x, y) {
            match button {
                MouseButton::Left if double => {
                    self.set_all_folded(&deck.root, false);
                }
                MouseButton::Left => {
                    let acks = self.effects_on(&deck.members);
                    self.ack_effects(acks);
                    if deck.agents_only() {
                        self.view_subagents(&deck.root);
                    } else {
                        self.select_node(&deck.top, additive);
                    }
                    self.drag = Some(Drag::Nodes { ids: vec![deck.root.clone()], start: (x, y), applied: (0, 0), moved: false, click: None });
                }
                MouseButton::Right => {
                    // Its menu when the button comes up here; a drag selects by area.
                    self.drag = Some(Drag::Rubber { start: (x, y), now: (x, y), additive, right: true, click: Some(crate::app::CanvasPress::Deck(deck.root.clone())) });
                }
                MouseButton::Middle => return false,
            }
            return true;
        }
        if button != MouseButton::Left {
            return false;
        }
        match self.canvas_hit(x, y) {
            CanvasHit::Node(id) => {
                let acks = self.effects_on(std::slice::from_ref(&id));
                if acks.is_empty() {
                    return false;
                }
                self.ack_effects(acks);
                if additive || !self.selection.contains(&id) {
                    self.select_node(&id, additive);
                }
                let ids = self.movable(&self.selection);
                self.drag = Some(Drag::Nodes { ids, start: (x, y), applied: (0, 0), moved: false, click: None });
                true
            }
            CanvasHit::GroupTitle(id) | CanvasHit::GroupResize(id) => {
                self.ack_group_effects(&id);
                false
            }
            CanvasHit::GroupBody(_) | CanvasHit::Empty => {
                let (wx, wy) = self.to_world_f(self.point_in(x, y).0, self.point_in(x, y).1);
                let group = self
                    .doc
                    .groups
                    .iter()
                    .filter(|g| wx >= g.x as f32 && wx < (g.x + g.w) as f32 && wy >= g.y as f32 && wy < (g.y + g.h) as f32)
                    .min_by_key(|g| (g.w as i64) * (g.h as i64))
                    .map(|g| g.id.clone());
                if let Some(group) = group {
                    self.ack_group_effects(&group);
                }
                false
            }
        }
    }

    fn ack_group_effects(&mut self, group: &str) {
        let ids: Vec<String> = self.active_effects().into_iter().filter(|e| e.target.is_empty() && e.group == group).map(|e| e.id.clone()).collect();
        self.ack_effects(ids);
    }

    /// The deck's menu: unfold it (all, or each part), view or open what it
    /// holds, hide the kinds it holds (H, J).
    pub(crate) fn stack_menu(&self, deck: &Deck) -> (String, Vec<MenuItem>) {
        let root = self.doc.node(&deck.root).map(|n| n.name.clone()).unwrap_or_default();
        let (agents, sessions) = (!deck.agents.is_empty(), !deck.sessions.is_empty());
        let mut items = Vec::new();
        if agents && sessions {
            items.push(item("Unfold all", "K · double-click", VisualAction::FoldAll(deck.root.clone(), false), true));
        }
        if agents {
            items.push(item("Show subagents", if sessions { "" } else { "K · double-click" }, VisualAction::FoldSubagents(deck.root.clone(), false), true));
        }
        if sessions {
            items.push(item("Unstack subsessions", if agents { "" } else { "K · double-click" }, VisualAction::Stack(deck.root.clone(), false), true));
            let top = self.doc.node(&deck.top).map(|n| n.name.clone()).unwrap_or_default();
            items.push(MenuItem { label: format!("Open “{top}”"), hint: "Enter".into(), action: Action::Open(deck.top.clone()), enabled: true });
        } else {
            items.push(item("List subagents", "click", VisualAction::ViewSubagents(deck.root.clone()), true));
        }
        // Its subagent boxes one by one into the hidden area (the newest first; the details list has them all).
        for id in deck.agents.iter().rev().take(DECK_HIDE_ITEMS) {
            let name = self.doc.node(id).map(|n| crate::paint::truncate(&n.name, 32)).unwrap_or_default();
            items.push(item(&format!("Hide ◇ {name}"), "", VisualAction::Hide(vec![id.clone()]), true));
        }
        if sessions {
            items.push(item(hide_subsessions_label(self.doc.ui.hide_subsessions), "H", VisualAction::ToggleHideSubsessions, true));
        }
        if agents {
            items.push(item(hide_subagents_label(self.doc.ui.hide_subagents), "J", VisualAction::ToggleHideSubagents, true));
        }
        let title = match (deck.sessions.len(), deck.agents.len()) {
            (0, a) => format!("{a} subagent box(es) of “{root}”"),
            (s, 0) => format!("Stack of {s} under “{root}”"),
            (s, a) => format!("Stack of {s} and {a} subagent box(es) under “{root}”"),
        };
        (title, items)
    }

    // ---------- menus ----------

    /// The session menu's deck and effect items: its subagent boxes and its
    /// subsessions, each stacked or shown on their own, or both at once (K).
    pub fn visual_node_items(&self, id: &str) -> Vec<MenuItem> {
        let mut items = Vec::new();
        let Some(node) = self.doc.node(id).filter(|n| n.kind == NodeKind::Session) else { return items };
        let agents = !vm::subagents_of(&self.doc, id).is_empty();
        let sessions = !vm::descendants(&self.doc, id).is_empty();
        let k = if agents && sessions { "" } else { "K" };
        if agents {
            let folded = vm::subagents_folded(&self.doc, node);
            items.push(item(if folded { "Show subagents" } else { "Stack subagents" }, k, VisualAction::FoldSubagents(id.to_owned(), !folded), true));
        }
        if sessions {
            let label = if node.stacked { "Unstack subsessions" } else { "Stack subsessions" };
            items.push(item(label, k, VisualAction::Stack(id.to_owned(), !node.stacked), true));
        }
        if agents && sessions {
            let folded = self.anything_folded(id);
            items.push(item(if folded { "Unfold all" } else { "Stack all" }, "K", VisualAction::FoldAll(id.to_owned(), !folded), true));
        }
        if self.has_effect(id) {
            items.push(item("Clear effect", "click", VisualAction::AckEffects(id.to_owned()), true));
        }
        items
    }

    /// The canvas menu's items: hide or show subsessions (H) and subagent
    /// boxes (J), the list of hidden boxes, select unused sessions.
    pub fn visual_canvas_items(&self) -> Vec<MenuItem> {
        let ids: HashSet<&str> = self.doc.nodes.iter().map(|n| n.id.as_str()).collect();
        let subsessions = self.doc.nodes.iter().any(|n| vm::is_subsession(&ids, n));
        let agents = self.doc.nodes.iter().any(|n| n.kind == NodeKind::Agent);
        let (h, j) = (self.doc.ui.hide_subsessions, self.doc.ui.hide_subagents);
        let hidden = self.conceal_maps().1.len();
        let days = self.doc.dust_after_days;
        vec![
            item(hide_subsessions_label(h), "H", VisualAction::ToggleHideSubsessions, subsessions || h),
            item(hide_subagents_label(j), "J", VisualAction::ToggleHideSubagents, agents || j),
            item(&format!("Hidden boxes ({hidden})…"), "", VisualAction::HiddenList(None, false), hidden > 0),
            item(&format!("Select unused sessions (older than {days} d)"), "", VisualAction::SelectUnused, days > 0),
        ]
    }

    pub fn run_visual(&mut self, action: VisualAction) {
        self.needs_draw = true;
        match action {
            VisualAction::Stack(id, stacked) => self.set_stacked(&id, stacked),
            VisualAction::FoldSubagents(id, folded) => self.set_subagents_folded(&id, folded),
            VisualAction::FoldAll(id, folded) => self.set_all_folded(&id, folded),
            VisualAction::ViewSubagents(id) => self.view_subagents(&id),
            VisualAction::ToggleStack => self.toggle_fold(),
            VisualAction::ToggleHideSubsessions => self.toggle_hide_subsessions(),
            VisualAction::ToggleHideSubagents => self.toggle_hide_subagents(),
            VisualAction::Hide(ids) => self.set_hidden(&ids, true),
            VisualAction::Show(ids) => self.set_hidden(&ids, false),
            VisualAction::HiddenList(parent, agents_first) => {
                // Beneath where it was asked for (a counter, a marker, a menu).
                let (x, y) = (self.pointer.0.max(0.0) as u16, (self.pointer.1.max(0.0) as u16).saturating_add(1));
                let menu = self.hidden_list(parent.as_deref(), agents_first);
                self.open_menu(x, y, menu);
            }
            VisualAction::SelectUnused => {
                let ids = self.dusty_sessions();
                let days = self.doc.dust_after_days;
                self.selected_group = None;
                if ids.is_empty() {
                    self.selection.clear();
                    return self.notify(format!("No session has been unused for {days}+ days."), false);
                }
                self.notify(format!("{} unused session(s) selected (older than {days} d). Remove them from the canvas with Del; their history is kept.", ids.len()), false);
                self.selection = ids;
            }
            VisualAction::AckEffects(target) => {
                let ids: Vec<String> = match target.strip_prefix("group:") {
                    Some(group) => self.active_effects().into_iter().filter(|e| e.target.is_empty() && e.group == group).map(|e| e.id.clone()).collect(),
                    None => self.effects_on(std::slice::from_ref(&target)),
                };
                self.ack_effects(ids);
            }
            VisualAction::GroupPassives(group) => self.open_group_passives(&group),
            VisualAction::GroupFolders(group) => self.group_folders_menu(&group),
            VisualAction::Browse(path) => self.browse_remote(&path),
            VisualAction::PassiveFile(id, which) => self.open_passive_file(&id, which),
        }
    }

    // ---------- settings ----------

    /// The Settings view's dust, subagents, effects and working glow rows.
    pub fn visual_setting_rows(&self) -> Vec<crate::side::ListRow> {
        let days = self.doc.dust_after_days;
        let dust = if days == 0 { "off".to_owned() } else { format!("{days} day{}", if days == 1 { "" } else { "s" }) };
        let effects = match self.visual.motion {
            Some(motion) => motion.name().to_owned(),
            None => self.motion().name().to_owned(),
        };
        let row = |setting, label: &str, right: String| crate::side::ListRow {
            item: crate::side::ItemRef::Visual(setting),
            label: label.to_owned(),
            right,
            icon: "›",
            color: crate::paint::TEXT,
        };
        let subagents = if self.doc.subagents_shown { "shown" } else { "stacked" };
        let glow = if self.working_glow_setting() { "on" } else { "off" };
        vec![
            row(VisualSetting::Dust, "Dust unused sessions after", dust),
            row(VisualSetting::Subagents, "Subagents", subagents.to_owned()),
            row(VisualSetting::Effects, "Effects", effects),
            row(VisualSetting::WorkingGlow, "Working glow", glow.to_owned()),
        ]
    }

    /// Enter or Space on a settings row: the next choice.
    pub fn visual_setting(&mut self, setting: VisualSetting) {
        match setting {
            VisualSetting::Dust => {
                let choices = vm::DUST_CHOICES;
                let next = choices.iter().position(|d| *d == self.doc.dust_after_days).map(|i| (i + 1) % choices.len()).unwrap_or(2);
                self.doc.dust_after_days = choices[next];
                self.mark_dirty();
            }
            VisualSetting::Subagents => {
                // The canvas default; a session's own choice (K, its menu) wins.
                self.doc.subagents_shown = !self.doc.subagents_shown;
                let shown = self.doc.subagents_shown;
                self.after_fold(!shown);
                let own = self.doc.nodes.iter().filter(|n| n.subagents_shown.is_some_and(|s| s != shown) && !vm::subagents_of(&self.doc, &n.id).is_empty()).count();
                let keep = if own > 0 { format!(" ({own} session(s) keep their own choice)") } else { String::new() };
                self.notify(format!("Subagent boxes are {} by default{keep}.", if shown { "shown on their own" } else { "stacked beneath their session" }), false);
            }
            VisualSetting::Effects => {
                let next = match self.motion() {
                    Motion::On => Motion::Reduced,
                    Motion::Reduced => Motion::Off,
                    Motion::Off => Motion::On,
                };
                self.visual.motion = Some(next);
                let stored = match next {
                    Motion::On => "on",
                    Motion::Reduced => "reduced",
                    Motion::Off => "off",
                };
                if let Some(home) = self.settings_home() {
                    let saved = crate::settings::Settings::load_checked(&home).and_then(|mut s| {
                        s.effects = Some(stored.to_owned());
                        s.save(&home).map_err(Into::into)
                    });
                    if let Err(error) = saved {
                        self.notify(format!("Cannot save the effects setting: {error:#}"), true);
                    }
                }
                self.needs_draw = true;
            }
            VisualSetting::WorkingGlow => {
                let on = !self.working_glow_setting();
                self.visual.grid_pulse = Some(on);
                let saved = self.settings_home().map(|home| {
                    crate::settings::Settings::load_checked(&home).and_then(|mut s| {
                        s.grid_pulse = Some(on);
                        s.save(&home).map_err(Into::into)
                    })
                });
                match saved {
                    Some(Err(error)) => self.notify(format!("Cannot save the working glow setting: {error:#}"), true),
                    _ if !on => self.notify("Working glow off: the canvas's dots stay still around working sessions.", false),
                    _ if self.motion() == Motion::Off => self.notify("Working glow on: it shows while Effects are on (now off).", false),
                    _ if self.motion() == Motion::Reduced => self.notify("Working glow on: with reduced motion, a still glow around working sessions.", false),
                    _ => self.notify("Working glow on: the dots around a working session glow and pulse outwards.", false),
                }
                self.needs_draw = true;
            }
        }
    }

    // ---------- every tick ----------

    /// Housekeeping: transitions, effect frames (12.5 fps, the spinners'
    /// clock, while one is in view and animated), acknowledgements the
    /// service applied, last use.
    pub fn visual_tick(&mut self) {
        if self.visual.motion.is_none() && self.instance.is_some() {
            self.load_motion();
        }
        if self.visual.grid_pulse.is_none() && self.instance.is_some() {
            self.load_grid_pulse();
        }
        self.sync_transitions();
        if self.visual.animating() {
            self.needs_draw = true;
        }
        let live: HashSet<String> = self.bus.effects.iter().map(|e| e.id.clone()).collect();
        self.visual.acked.retain(|id| live.contains(id));
        self.visual.seen.retain(|id, _| live.contains(id));
        self.visual.effects_live = self.render_mode == RenderMode::Gpu && self.motion() == Motion::On && self.frozen_tick.is_none() && self.effects_in_view();
        // On the spinners' 80 ms clock, so effects and other animations share frames.
        if self.visual.effects_live && self.tick as u64 != self.visual.effect_frame {
            self.visual.effect_frame = self.tick as u64;
            self.needs_draw = true;
        }
        if self.sessions_loaded && self.visual.history_at.is_none() {
            self.visual.history_at = Some(Instant::now());
        }
        if self.visual.used_at.is_none_or(|t| t.elapsed() >= USED_EVERY) {
            self.visual.used_at = Some(Instant::now());
            self.sync_last_used();
        }
    }

    /// The GUI keeps ticking quickly (a transition or an animated effect).
    pub fn visual_busy(&self) -> bool {
        self.visual.animating() || self.visual.effects_live
    }
}

/// The badge style of an effect kind (colour-blind safe).
pub fn effect_style(kind: &str) -> Style {
    let bg = match kind {
        "fireworks" => crate::paint::POSITIVE,
        "attention" => crate::paint::ATTENTION,
        "alert" => crate::paint::NEGATIVE,
        _ => crate::paint::LOOP_LIT,
    };
    Style::default().fg(crate::paint::PANEL).bg(bg).add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Doc;

    fn app(doc: Doc) -> App {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let (_service, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, _app_rx) = crossbeam_channel::unbounded();
        let mut app = App::new(crate::conn::Conn::detached(tx, service_rx), std::sync::Arc::new(crate::rpc::Offline), crate::sync::Opened::detached(doc), false, ".".into(), app_tx, None);
        app.layout.canvas = ratatui::layout::Rect::new(0, 1, 120, 40);
        app.visual.motion = Some(Motion::On);
        app
    }

    fn session(id: &str, parent: Option<&str>, x: i32, y: i32) -> Node {
        let mut node: Node = serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": id, "x": x, "y": y })).unwrap();
        node.parent = parent.map(str::to_owned);
        node
    }

    fn agent(id: &str, parent: &str, x: i32, y: i32) -> Node {
        let mut node = session(id, Some(parent), x, y);
        node.kind = NodeKind::Agent;
        node.agent_key = id.split(':').nth(1).map(str::to_owned);
        node
    }

    fn agent_summary(key: &str, status: crate::protocol::AgentStatus) -> crate::protocol::AgentSummary {
        crate::protocol::AgentSummary { key: key.into(), name: key.into(), description: String::new(), status, finished_ms: Some(now_ms() - 1_000) }
    }

    /// A live session `p` (0,0) whose history lists these subagents.
    fn with_agents(app: &mut App, agents: Vec<crate::protocol::AgentSummary>) {
        let summary = crate::protocol::SessionSummary { id: "p".into(), activity: crate::protocol::Activity::Idle, live: true, tracked: true, agents, ..Default::default() };
        app.sessions.insert("p".into(), summary);
    }

    #[test]
    fn subagents_fold_into_a_slim_deck_by_default_that_shows_their_state() {
        use crate::protocol::AgentStatus;
        // A canvas from before: three subagent boxes in a column under p, no choice saved.
        let mut doc = Doc::default();
        doc.nodes.push(session("p", None, 0, 0));
        for (i, key) in ["a", "b", "c"].iter().enumerate() {
            doc.nodes.push(agent(&format!("p:{key}"), "p", 4, 5 + i as i32 * 4));
        }
        let mut app = app(doc);
        with_agents(&mut app, vec![agent_summary("a", AgentStatus::Finished), agent_summary("b", AgentStatus::Working), agent_summary("c", AgentStatus::Finished)]);
        let frame = app.visual_frame();
        assert_eq!(frame.concealed.get("p:b"), Some(&Conceal::Stack("p".into())), "folded by default");
        let deck = frame.deck_of("p").expect("a deck").clone();
        assert!(deck.agents_only() && deck.agents.len() == 3 && deck.back() == 2);
        assert_eq!(deck.rect, WRect::new(4.0, 5.0, 24.0, 3.0), "a slim card where the first subagent box goes");
        assert_eq!(app.aggregate_status(&deck.members), Some(Status::Working), "a running subagent outranks those that just finished");
        let model = crate::ui_canvas::deck_model(&app, &deck, deck.rect, &crate::ui_canvas::Boxes::new(&app)).unwrap();
        assert_eq!((model.prefix, model.name.as_str(), model.label), ("◇ ", "3 subagents", "working"));
        assert!(model.line.ends_with("working · 1 of 3"), "{}", model.line);
        assert_eq!(model.deck.as_ref().and_then(|d| d.badge()), None, "the title counts them");
        assert_eq!(app.node_status(app.doc.node("p").unwrap()), Status::Working, "the session stays working while one runs");
        // A failure is the most important state.
        with_agents(&mut app, vec![agent_summary("a", AgentStatus::Failed), agent_summary("b", AgentStatus::Working), agent_summary("c", AgentStatus::Finished)]);
        assert_eq!(app.aggregate_status(&deck.members), Some(Status::Failed));
        // A click views (the session's details list them); nothing starts.
        let (dx, dy) = app.to_screen(5, 6);
        assert!(app.visual_mouse_down(MouseButton::Left, dx as u16, dy as u16, false, false));
        assert_eq!(app.selection, ["p"]);
        assert_eq!(app.right_detail, Some(crate::side::RightDetail::Session("p".into())));
        assert!(app.launching.is_empty() && matches!(&app.drag, Some(Drag::Nodes { ids, click: None, .. }) if ids == &["p".to_owned()]), "dragging moves the session");
        app.drag = None;
        // A double-click shows them, at exactly their places.
        assert!(app.visual_mouse_down(MouseButton::Left, dx as u16, dy as u16, true, false));
        assert_eq!(app.doc.node("p").unwrap().subagents_shown, Some(true));
        assert!(app.visual_frame().decks.is_empty());
        assert_eq!(app.doc.nodes.iter().map(|n| (n.x, n.y)).collect::<Vec<_>>(), [(0, 0), (4, 5), (4, 9), (4, 13)]);
        assert!(app.launching.is_empty());
    }

    #[test]
    fn k_unfolds_everything_folded_else_folds_everything() {
        let mut doc = Doc::default();
        // p: subagent boxes and a subsession; q: a subsession only; r: subagent boxes only.
        doc.nodes.push(session("p", None, 0, 0));
        doc.nodes.push(agent("p:a", "p", 4, 5));
        doc.nodes.push(session("c", Some("p"), 40, 0));
        doc.nodes.push(session("q", None, 0, 20));
        doc.nodes.push(session("d", Some("q"), 40, 20));
        doc.nodes.push(session("r", None, 0, 40));
        doc.nodes.push(agent("r:a", "r", 4, 45));
        let mut app = app(doc);
        let state = |app: &App, id: &str| {
            let n = app.doc.node(id).unwrap();
            (vm::subagents_folded(&app.doc, n), n.stacked)
        };
        let k = |app: &mut App, id: &str| {
            app.selection = vec![id.to_owned()];
            app.run_visual(VisualAction::ToggleStack);
        };
        // Both: the subagent boxes are folded (the default), so K unfolds everything …
        k(&mut app, "p");
        assert_eq!(state(&app, "p"), (false, false));
        // … and then folds everything into one deck.
        k(&mut app, "p");
        assert_eq!(state(&app, "p"), (true, true));
        let deck = app.visual_frame().deck_of("p").cloned().unwrap();
        assert_eq!((deck.sessions.clone(), deck.agents.clone(), deck.top.as_str()), (vec!["c".to_owned()], vec!["p:a".to_owned()], "c"));
        assert_eq!(deck.rect, WRect::new(2.0, 5.0, 30.0, 4.0), "a stack's card beneath the session");
        k(&mut app, "p");
        assert_eq!(state(&app, "p"), (false, false));
        // A stack of subsessions only (no subagent boxes): K stacks and unstacks it.
        k(&mut app, "q");
        assert!(state(&app, "q").1);
        k(&mut app, "d");
        assert!(!state(&app, "q").1, "K on a subsession acts on its parent");
        // Subagent boxes only: K shows them and stacks them again.
        k(&mut app, "r");
        assert!(!state(&app, "r").0);
        k(&mut app, "r:a");
        assert!(state(&app, "r").0, "K on a subagent box stacks its session's boxes");
        // A folded box selected from the sidebar: K unfolds its deck.
        k(&mut app, "r:a");
        assert!(!state(&app, "r").0);
        // Nothing to fold.
        app.doc.nodes.push(session("lone", None, 80, 0));
        k(&mut app, "lone");
        assert!(app.notice.as_ref().is_some_and(|n| n.text.contains("Select a session with subagents or subsessions")));
        // The menus offer each part separately where both exist, and both at once (K).
        let labels = |app: &App, id: &str| app.visual_node_items(id).iter().map(|i| (i.label.clone(), i.hint.clone())).collect::<Vec<_>>();
        let pair = |label: &str, hint: &str| (label.to_owned(), hint.to_owned());
        assert_eq!(labels(&app, "p"), [pair("Stack subagents", ""), pair("Stack subsessions", ""), pair("Stack all", "K")]);
        assert_eq!(labels(&app, "q"), [pair("Stack subsessions", "K")]);
        assert_eq!(labels(&app, "r"), [pair("Stack subagents", "K")]);
        app.run_visual(VisualAction::FoldAll("p".into(), true));
        let deck = app.visual_frame().deck_of("p").cloned().unwrap();
        let (title, items) = app.stack_menu(&deck);
        assert_eq!(title, "Stack of 1 and 1 subagent box(es) under “p”");
        assert_eq!(items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["Unfold all", "Show subagents", "Unstack subsessions", "Open “c”", "Hide ◇ p:a", "Hide subsessions", "Hide subagents"]);
        assert!(app.launching.is_empty(), "folding never starts anything");
    }

    #[test]
    fn new_subagents_arrive_folded_and_get_a_slot_when_first_shown() {
        use crate::protocol::AgentStatus;
        let mut doc = Doc::default();
        doc.nodes.push(session("p", None, 2, 2));
        doc.nodes.push(session("other", None, 2, 11));
        doc.groups.push(crate::doc::Group { id: "g".into(), name: "G".into(), x: 0, y: 0, w: 40, h: 18, ..Default::default() });
        let mut app = app(doc);
        with_agents(&mut app, vec![agent_summary("a1", AgentStatus::Working), agent_summary("a2", AgentStatus::Finished)]);
        app.sync_agents();
        let a1 = app.doc.node("p:a1").unwrap().clone();
        assert!(a1.unplaced && (a1.x, a1.y) == (5, 2), "on its session, without a place of its own");
        let g = |app: &App| app.doc.group("g").map(|g| (g.x, g.y, g.w, g.h)).unwrap();
        assert_eq!(g(&app), (0, 0, 40, 18), "no group grows for a folded box");
        assert_eq!(app.doc.group_of(&a1).map(|g| g.id.as_str()), Some("g"));
        // Shown: a slot of its own under its session (around the deck and the other box), and the group grows.
        app.run_visual(VisualAction::FoldSubagents("p".into(), false));
        let at = |app: &App, id: &str| app.doc.node(id).map(|n| (n.x, n.y, n.unplaced)).unwrap();
        assert_eq!((at(&app, "p:a1"), at(&app, "p:a2")), ((6, 7, false), (6, 15, false)), "below p, past the other session");
        assert!(g(&app).3 > 18, "the group grew for them: {:?}", g(&app));
        // Stacked again: the places stay, the group too (Fit to members shrinks it).
        let grown = g(&app);
        app.run_visual(VisualAction::FoldSubagents("p".into(), true));
        assert_eq!((at(&app, "p:a1"), g(&app)), ((6, 7, false), grown));
        app.run(crate::dialogs::Action::FitGroup("g".into()));
        assert_eq!(g(&app), (0, 0, 34, 16), "around p, its deck and the other session");
        // A new subagent of a session that shows its boxes gets a slot at once.
        app.run_visual(VisualAction::FoldSubagents("p".into(), false));
        with_agents(&mut app, vec![agent_summary("a1", AgentStatus::Working), agent_summary("a2", AgentStatus::Finished), agent_summary("a3", AgentStatus::Working)]);
        app.sync_agents();
        assert!(!at(&app, "p:a3").2 && at(&app, "p:a3").1 > 15);
    }

    #[test]
    fn the_canvas_default_follows_settings_and_a_sessions_own_choice_wins() {
        let mut doc = Doc::default();
        doc.nodes.push(session("p", None, 0, 0));
        doc.nodes.push(agent("p:a", "p", 4, 5));
        doc.nodes.push(session("q", None, 40, 0));
        doc.nodes.push(agent("q:a", "q", 44, 5));
        let mut app = app(doc);
        let folded = |app: &App, id: &str| vm::subagents_folded(&app.doc, app.doc.node(id).unwrap());
        let row = |app: &App| app.visual_setting_rows().into_iter().find(|r| r.label == "Subagents").map(|r| r.right).unwrap();
        assert_eq!((row(&app), folded(&app, "p"), folded(&app, "q")), ("stacked".to_owned(), true, true));
        app.visual_setting(VisualSetting::Subagents);
        assert_eq!((row(&app), folded(&app, "p"), folded(&app, "q")), ("shown".to_owned(), false, false));
        app.run_visual(VisualAction::FoldSubagents("p".into(), true));
        assert!(folded(&app, "p") && !folded(&app, "q"), "p's own choice wins over the default");
        app.visual_setting(VisualSetting::Subagents);
        assert!(folded(&app, "p") && folded(&app, "q"));
        app.run_visual(VisualAction::FoldSubagents("q".into(), false));
        assert!(!folded(&app, "q"), "q's own choice wins over the default");
        assert!(app.notice.is_some() && app.dirty_at.is_some(), "a canvas edit (saved, sent to every window)");
    }

    fn screen(app: &mut App) -> String {
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 160, 45));
        crate::ui::render(app, &mut buf, ratatui::layout::Rect::new(0, 0, 160, 45), None);
        (0..45).map(|y| (0..160).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n").collect()
    }

    #[test]
    fn h_hides_subsessions_and_j_subagent_boxes_each_on_its_own() {
        let mut doc = Doc::default();
        doc.nodes.push(session("p", None, 0, 0));
        doc.nodes.push(agent("p:a", "p", 4, 5));
        doc.nodes.push(agent("p:b", "p", 4, 9));
        doc.nodes.push(session("c", Some("p"), 40, 0));
        doc.nodes.push(agent("c:x", "c", 44, 5));
        let mut app = app(doc);
        app.run_visual(VisualAction::FoldSubagents("c".into(), false));
        assert_eq!(app.visual_frame().decks.len(), 1, "p's deck");
        let places = |app: &App| app.doc.nodes.iter().map(|n| (n.x, n.y)).collect::<Vec<_>>();
        let before = places(&app);
        // H: the subsession and its subagent box; p's folded subagent boxes keep their deck.
        app.selection = vec!["c".into()];
        app.run_visual(VisualAction::ToggleHideSubsessions);
        let frame = app.visual_frame();
        assert_eq!(frame.hidden.get("p").map(Vec::len), Some(2), "the subsession and its subagent box");
        assert!(frame.skip("c") && frame.skip("c:x") && !frame.skip("p"));
        assert_eq!(frame.deck_of("p").map(|d| d.agents.len()), Some(2), "p's own subagent boxes stay in their deck");
        assert!(app.selection.is_empty(), "a hidden box leaves the selection");
        let text = screen(&mut app);
        let toolbar = text.lines().next().unwrap_or_default().to_owned();
        assert!(text.contains("⋯ 1 · ◇ 1 hidden") && toolbar.contains(" ↳ 1 hidden ") && !toolbar.contains('◇'), "{text}");
        assert_eq!(app.visual_canvas_items().iter().map(|i| i.label.as_str()).take(2).collect::<Vec<_>>(), ["Show subsessions", "Hide subagents"]);
        // J too: every subagent box, folded ones included (p's deck goes).
        app.run_visual(VisualAction::ToggleHideSubagents);
        let frame = app.visual_frame();
        assert!(frame.decks.is_empty(), "a deck of folded subagent boxes only goes with them");
        assert_eq!(frame.hidden.get("p").map(Vec::len), Some(4));
        let text = screen(&mut app);
        let toolbar = text.lines().next().unwrap_or_default().to_owned();
        assert!(text.contains("⋯ 1 · ◇ 3 hidden") && toolbar.contains(" ↳ 1 hidden ") && toolbar.contains(" ◇ 3 hidden "), "{text}");
        // H off: J alone keeps every subagent box hidden, each into its own session.
        app.run_visual(VisualAction::ToggleHideSubsessions);
        let frame = app.visual_frame();
        assert!(!frame.skip("c") && frame.skip("c:x") && frame.skip("p:a"));
        assert_eq!((frame.hidden.get("p").map(Vec::len), frame.hidden.get("c").map(Vec::len)), (Some(2), Some(1)));
        let toolbar = screen(&mut app).lines().next().unwrap_or_default().to_owned();
        assert!(toolbar.contains(" ◇ 3 hidden ") && !toolbar.contains('↳'), "the toolbar counts every hidden subagent box: {toolbar}");
        assert_eq!(app.visual_canvas_items()[1].label, "Show subagents");
        // J off: everything back in place, p's deck too; a toggle never moves a box nor starts anything.
        app.run_visual(VisualAction::ToggleHideSubagents);
        let frame = app.visual_frame();
        assert!(frame.hidden.is_empty() && frame.deck_of("p").is_some() && !frame.concealed.contains_key("c:x"));
        assert_eq!(places(&app), before);
        assert!(!screen(&mut app).lines().next().unwrap_or_default().contains(" hidden "), "no counters once nothing is hidden");
        assert!(app.launching.is_empty() && app.undo.is_empty(), "never an undo step");
    }

    #[test]
    fn a_deck_hides_what_is_hidden_and_shows_the_rest() {
        let mut doc = Doc::default();
        doc.nodes.push(session("p", None, 0, 0));
        doc.nodes.push(agent("p:a", "p", 4, 5));
        doc.nodes.push(session("c", Some("p"), 40, 0));
        let mut app = app(doc);
        app.run_visual(VisualAction::FoldAll("p".into(), true));
        let deck = app.visual_frame().deck_of("p").cloned().unwrap();
        assert_eq!((deck.sessions.len(), deck.agents.len()), (1, 1), "a stack and a folded subagent box in one deck");
        // H: the stack's subsession hides; the deck shows the folded subagent box as a slim card.
        app.run_visual(VisualAction::ToggleHideSubsessions);
        let frame = app.visual_frame();
        let deck = frame.deck_of("p").cloned().unwrap();
        assert!(deck.agents_only() && deck.agents == ["p:a"] && frame.skip("c"));
        assert_eq!(deck.rect, WRect::new(4.0, 5.0, 24.0, 3.0));
        assert_eq!(frame.hidden_badges.get("p").map(|h| (h.sessions, h.agents)), Some((1, 0)));
        // J instead: the deck holds the stack alone.
        app.run_visual(VisualAction::ToggleHideSubsessions);
        app.run_visual(VisualAction::ToggleHideSubagents);
        let frame = app.visual_frame();
        let deck = frame.deck_of("p").cloned().unwrap();
        assert!(deck.agents.is_empty() && deck.sessions == ["c"] && frame.skip("p:a"));
        let (title, items) = app.stack_menu(&deck);
        assert_eq!(title, "Stack of 1 under “p”");
        assert!(items.iter().any(|i| i.label == "Hide subsessions") && !items.iter().any(|i| i.hint == "J"), "only what the deck holds");
    }

    #[test]
    fn hide_switches_are_kept_with_the_windows_view_and_old_prefs_load() {
        let old: crate::sync::ClientPrefs = serde_json::from_str(r#"{"view":{"x":0,"y":0},"ui":{"sidebar_width":30,"sidebar_visible":true,"terminal_percent":55,"tiled":false,"hide_subsessions":true}}"#).unwrap();
        assert!(old.ui.hide_subsessions && !old.ui.hide_subagents, "an owner's prefs file: subsessions hidden, subagents not");
        let mut prefs = old.clone();
        prefs.ui.hide_subagents = true;
        let text = serde_json::to_string(&prefs).unwrap();
        assert!(text.contains("\"hide_subagents\":true"), "{text}");
        assert_eq!(serde_json::from_str::<crate::sync::ClientPrefs>(&text).unwrap(), prefs);
        let unset = serde_json::to_string(&crate::sync::ClientPrefs::default()).unwrap();
        assert!(!unset.contains("hide_subagents") && !unset.contains("hide_subsessions"), "unset switches are not written: {unset}");
    }

    #[test]
    fn effects_on_a_folded_box_show_on_its_deck_and_folding_animates_in_the_gpu() {
        let mut doc = Doc::default();
        doc.nodes.push(session("p", None, 0, 0));
        doc.nodes.push(agent("p:a", "p", 4, 9));
        let mut app = app(doc);
        app.bus.effects.push(Effect { id: "e1".into(), kind: "sparkle".into(), group: "g".into(), target: "p:a".into(), ms: 1, until_ms: u64::MAX, ..Default::default() });
        let frame = app.visual_frame();
        let anchor = app.effect_anchor(&app.bus.effects[0], &frame, &|n: &Node| WRect::from(n.rect()));
        assert_eq!(anchor, Some((WRect::new(4.0, 5.0, 24.0, 3.0), false)), "the deck, not the box's saved place");
        // The GPU animates folding and unfolding (and ends idle); reduced motion switches at once.
        app.render_mode = RenderMode::Gpu;
        app.visual_frame();
        app.run_visual(VisualAction::FoldSubagents("p".into(), false));
        let frame = app.visual_frame();
        assert!(app.visual.animating() && frame.moving.contains_key("p:a"), "the box flies out of the deck");
        std::thread::sleep(Duration::from_millis(320));
        let frame = app.visual_frame();
        assert!(!app.visual.animating() && frame.moving.is_empty() && frame.decks.is_empty());
        app.visual.motion = Some(Motion::Reduced);
        app.run_visual(VisualAction::FoldSubagents("p".into(), true));
        let frame = app.visual_frame();
        assert!(!app.visual.animating() && frame.moving.is_empty() && frame.deck_of("p").is_some_and(|d| d.alpha == 1.0));
    }

    #[test]
    fn transitions_run_both_ways_and_reverse_from_where_they_are() {
        let start = Instant::now();
        let closing = Transition { closing: true, start, from: 0.0 };
        assert_eq!(closing.value_at(start), 0.0);
        assert!((closing.value_at(start + Duration::from_millis(150)) - 0.5).abs() < 1e-3);
        assert!(closing.done_at(start + Duration::from_millis(300)));
        let back = Transition { closing: false, start, from: 0.5 };
        assert!(back.done_at(start + Duration::from_millis(150)), "reversing half way takes half the time");
        assert_eq!((ease(0.0), ease(1.0)), (0.0, 1.0));
        assert!(ease(0.25) < 0.25 && ease(0.75) > 0.75, "eased in and out");
        assert_eq!(motion_from(Some("off"), true), Motion::Off);
        assert_eq!(motion_from(None, false), Motion::Reduced, "reduced motion when Windows' animations are off");
        assert_eq!(motion_from(None, true), Motion::On);
    }

    #[test]
    fn stacks_conceal_children_show_a_deck_and_aggregate_status() {
        let mut doc = Doc::default();
        doc.nodes.push(session("p", None, 0, 0));
        doc.nodes.push(session("a", Some("p"), 40, 0));
        doc.nodes.push(session("b", Some("p"), 40, 8));
        let mut app = app(doc);
        assert!(app.visual_frame().decks.is_empty());
        app.run_visual(VisualAction::Stack("p".into(), true));
        let frame = app.visual_frame();
        assert_eq!(frame.concealed.get("a"), Some(&Conceal::Stack("p".into())));
        let deck = frame.deck_of("p").expect("a deck");
        assert_eq!((deck.top.as_str(), deck.sessions.len(), deck.back()), ("b", 2, 1), "newest on top");
        assert_eq!(deck.rect, WRect::new(2.0, 5.0, 30.0, 4.0), "beneath the parent");
        assert!(frame.moving.is_empty(), "the TUI has no transitions");
        app.terms.insert("a".into(), crate::protocol::TermInfo { id: "a".into(), alive: false, exit_code: Some(3), ..Default::default() });
        assert_eq!(app.aggregate_status(&deck.sessions), Some(Status::Failed), "a stacked failure shows on the stack");
        // A click on the deck views the top card; a double-click unstacks.
        let (dx, dy) = app.to_screen(3, 6);
        assert!(app.visual_mouse_down(MouseButton::Left, dx as u16, dy as u16, false, false));
        assert_eq!(app.selection, ["b"]);
        assert!(app.launching.is_empty() && matches!(&app.drag, Some(Drag::Nodes { ids, click: None, .. }) if ids == &["p".to_owned()]), "no launch; dragging moves the parent");
        app.drag = None;
        assert!(app.visual_mouse_down(MouseButton::Left, dx as u16, dy as u16, true, false));
        assert!(!app.doc.node("p").unwrap().stacked);
        assert_eq!(app.doc.node("a").map(|n| (n.x, n.y)), Some((40, 0)), "back where it was");
    }

    #[test]
    fn gpu_transitions_follow_changes_from_any_window_and_reverse() {
        let mut doc = Doc::default();
        doc.nodes.push(session("p", None, 0, 0));
        doc.nodes.push(session("a", Some("p"), 40, 0));
        let mut app = app(doc);
        app.render_mode = RenderMode::Gpu;
        app.visual_frame();
        // Another window stacks it (a patch): this one animates too.
        app.doc.node_mut("p").unwrap().stacked = true;
        let frame = app.visual_frame();
        assert!(app.visual.animating());
        let (rect, alpha) = frame.moving.get("a").copied().expect("flying into the deck");
        assert!(rect.x <= 40.0 && alpha <= 1.0);
        assert!(frame.deck_of("p").is_none_or(|d| d.alpha < 1.0));
        // Reversed mid-way: it flies back from where it is.
        app.doc.node_mut("p").unwrap().stacked = false;
        app.visual_frame();
        let t = app.visual.stacks.get("p").copied().unwrap();
        assert!(!t.closing && t.from <= 1.0);
        std::thread::sleep(Duration::from_millis(320));
        let frame = app.visual_frame();
        assert!(!app.visual.animating() && frame.moving.is_empty() && frame.decks.is_empty(), "idle again: no continuous redraw");
    }

    #[test]
    fn hiding_animates_in_the_gpu_both_ways_and_ends_idle() {
        let mut doc = Doc::default();
        doc.nodes.push(session("p", None, 0, 0));
        doc.nodes.push(session("a", Some("p"), 40, 0));
        doc.nodes.push(agent("p:x", "p", 4, 5));
        let mut app = app(doc);
        app.render_mode = RenderMode::Gpu;
        app.visual_frame();
        app.run_visual(VisualAction::ToggleHideSubsessions);
        let frame = app.visual_frame();
        assert!(app.visual.animating() && frame.moving.contains_key("a") && !frame.skip("a"), "the subsession shrinks into its parent");
        app.run_visual(VisualAction::ToggleHideSubagents);
        let frame = app.visual_frame();
        assert!(frame.skip("p:x") && !frame.moving.contains_key("p:x"), "a box from a deck switches at once");
        // A box that arrives while its kind is hidden never flies.
        app.doc.nodes.push(session("b", Some("p"), 40, 8));
        let frame = app.visual_frame();
        assert!(frame.skip("b") && !frame.moving.contains_key("b"));
        std::thread::sleep(Duration::from_millis(320));
        let frame = app.visual_frame();
        assert!(!app.visual.animating() && frame.moving.is_empty() && frame.skip("a"));
        // Shown again: it grows out of its parent, then all is idle.
        app.run_visual(VisualAction::ToggleHideSubsessions);
        let frame = app.visual_frame();
        assert!(frame.moving.get("a").is_some_and(|(_, alpha)| *alpha < 1.0) && !frame.concealed.contains_key("a"));
        std::thread::sleep(Duration::from_millis(320));
        assert!(app.visual_frame().moving.is_empty() && !app.visual.animating());
    }

    #[test]
    fn hiding_subsessions_counts_them_on_the_parent_and_keeps_positions() {
        let mut doc = Doc::default();
        doc.nodes.push(session("p", None, 0, 0));
        doc.nodes.push(session("a", Some("p"), 40, 0));
        doc.nodes.push(session("a2", Some("a"), 80, 0));
        let mut app = app(doc);
        app.selection = vec!["a".into()];
        app.run_visual(VisualAction::ToggleHideSubsessions);
        let frame = app.visual_frame();
        assert_eq!(frame.hidden.get("p").map(Vec::len), Some(2));
        assert!(frame.skip("a") && frame.skip("a2") && !frame.skip("p"));
        assert!(app.selection.is_empty(), "a hidden box leaves the selection");
        app.run_visual(VisualAction::ToggleHideSubsessions);
        assert!(app.visual_frame().concealed.is_empty());
        assert_eq!(app.doc.node("a2").map(|n| (n.x, n.y)), Some((80, 0)), "shown again in place");
    }

    #[test]
    fn dust_gathers_on_idle_sessions_only_and_unused_ones_can_be_selected() {
        let day = 86_400_000;
        let mut doc = Doc::default();
        for (id, age) in [("fresh", 1), ("old", 20), ("ancient", 90), ("busy", 90)] {
            let mut n = session(id, None, 0, 0);
            n.last_used_ms = Some(100 * day - age * day);
            doc.nodes.push(n);
        }
        doc.nodes.push(session("never", None, 0, 0));
        let mut app = app(doc);
        app.visual.clock_ms = Some(100 * day);
        let summary = crate::protocol::SessionSummary { id: "busy".into(), activity: crate::protocol::Activity::Working, live: true, tracked: true, ..Default::default() };
        app.sessions.insert("busy".into(), summary);
        let dust = |app: &App, id: &str| {
            let node = app.doc.node(id).unwrap();
            app.dust_of(node, app.node_status(node)).map(|(d, _)| d)
        };
        assert_eq!(dust(&app, "fresh"), None);
        assert_eq!(dust(&app, "old"), Some(Dust::Medium));
        assert_eq!(dust(&app, "ancient"), Some(Dust::Heavy));
        assert_eq!(dust(&app, "busy"), None, "working is never dusty");
        assert_eq!(dust(&app, "never"), None, "no recorded use yet");
        let look = app.dust_look(app.doc.node("old").unwrap(), Status::Idle).unwrap();
        assert_eq!(look.suffix, " · unused 20d");
        app.run_visual(VisualAction::SelectUnused);
        assert_eq!(app.selection, ["old", "ancient"]);
        app.doc.dust_after_days = 0;
        assert_eq!(dust(&app, "ancient"), None, "off");
        app.visual_setting(VisualSetting::Dust);
        assert_eq!(app.doc.dust_after_days, 1, "off → 1 → 3 → 7 → 14");
    }

    #[test]
    fn last_used_follows_starts_typing_and_history_but_not_looking() {
        let mut doc = Doc::default();
        let mut n = session("s", None, 0, 0);
        n.reviewed_ms = Some(5_000);
        doc.nodes.push(n);
        let mut app = app(doc);
        app.sync_last_used();
        assert_eq!(app.doc.node("s").unwrap().last_used_ms, None, "waits for the history");
        app.sessions_loaded = true;
        app.visual.history_at = Some(Instant::now());
        app.sync_last_used();
        assert_eq!(app.doc.node("s").unwrap().last_used_ms, None, "the summaries are still coming in");
        app.visual.history_at = Some(Instant::now().checked_sub(HISTORY_SETTLE).unwrap_or_else(Instant::now));
        app.sync_last_used();
        assert_eq!(app.doc.node("s").unwrap().last_used_ms, Some(5_000), "migrated from when you last looked");
        app.select_node("s", false);
        app.sync_last_used();
        assert_eq!(app.doc.node("s").unwrap().last_used_ms, Some(5_000), "selecting is not use");
        let started = now_ms() - 1_000;
        app.terms.insert("s".into(), crate::protocol::TermInfo { id: "s".into(), alive: true, started_ms: started, ..Default::default() });
        app.sync_last_used();
        assert_eq!(app.doc.node("s").unwrap().last_used_ms, Some(started), "a start is use");
    }

    #[test]
    fn clicking_an_effect_acknowledges_it_without_launching() {
        let mut doc = Doc::default();
        doc.nodes.push(session("s", None, 0, 0));
        let mut app = app(doc);
        let effect = Effect { id: "e1".into(), kind: "fireworks".into(), group: "g".into(), target: "s".into(), ms: 1, until_ms: u64::MAX, ..Default::default() };
        app.bus.effects.push(effect);
        assert!(app.has_effect("s"));
        let frame = app.visual_frame();
        let models = app.effect_models(&frame, &|n: &Node| WRect::from(n.rect()));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].badge, "✦ approved");
        let (x, y) = app.to_screen(2, 1);
        assert!(app.visual_mouse_down(MouseButton::Left, x as u16, y as u16, false, false));
        assert!(app.visual.acked.contains("e1") && !app.has_effect("s"));
        assert_eq!(app.selection, ["s"]);
        assert!(app.launching.is_empty() && matches!(&app.drag, Some(Drag::Nodes { click: None, .. })), "selected, never started");
        // The service dropped it: nothing left to remember.
        app.bus.effects.clear();
        app.visual_tick();
        assert!(app.visual.acked.is_empty());
    }
}

/// Boxes hidden on their own (the canvas's hidden area, `Node::hidden`).
#[cfg(test)]
mod hidden_area_tests {
    use super::VisualAction;
    use crate::app::App;
    use crate::dialogs::{Action, Hit, Modal};
    use crate::input::{Modifiers, MouseButton};
    use crate::protocol::ClientMsg;
    use crate::sync::Op;
    use crate::undo::fixture::*;

    const NONE: Modifiers = Modifiers::NONE;

    /// Draws a frame at the fixture's size (it registers the click targets).
    fn render(app: &mut App) -> String {
        let area = ratatui::layout::Rect::new(0, 0, 200, 60);
        let (mut buf, mut overlay) = (ratatui::buffer::Buffer::empty(area), ratatui::buffer::Buffer::empty(area));
        let gpu = app.render_mode == crate::view::RenderMode::Gpu;
        crate::ui::render(app, &mut buf, area, if gpu { Some(&mut overlay) } else { None });
        (0..60).map(|y| (0..200).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n").collect()
    }

    fn labels(app: &App) -> Vec<String> {
        match app.modals.last() {
            Some(Modal::Menu(menu)) => menu.items.iter().map(|i| i.label.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// Runs the open menu's item `label`.
    fn pick(app: &mut App, label: &str) {
        let Some(Modal::Menu(menu)) = app.modals.pop() else { panic!("no menu is open") };
        let item = menu.items.iter().find(|i| i.label == label).unwrap_or_else(|| panic!("no {label:?} in {:?}", menu.items.iter().map(|i| &i.label).collect::<Vec<_>>()));
        assert!(item.enabled, "{label} is enabled");
        app.run(item.action.clone());
    }

    /// Clicks the middle of the first click target that runs `action`.
    fn click_hit(app: &mut App, action: VisualAction) {
        let hit = Hit::Toolbar(Action::Visual(action.clone()));
        let rect = app.layout.hits.iter().find(|(_, h)| *h == hit).map(|(r, _)| *r).unwrap_or_else(|| panic!("no click target for {action:?}"));
        click(app, MouseButton::Left, (rect.x as f32 + rect.width as f32 / 2.0, rect.y as f32 + 0.5), NONE);
    }

    fn hidden_flag(app: &App, id: &str) -> bool {
        app.doc.node(id).is_some_and(|n| n.hidden)
    }

    #[test]
    fn a_subsession_hidden_from_its_menu_is_listed_on_its_parents_marker_and_shown_in_place() {
        for gpu in [false, true] {
            let (mut app, sent) = app(gpu);
            let before = layout(&app);
            // Right-click the subsession: Hide.
            click(&mut app, MouseButton::Right, (45.5, 3.5), NONE);
            assert!(labels(&app).contains(&"Hide".to_owned()), "{:?}", labels(&app));
            pick(&mut app, "Hide");
            assert!(hidden_flag(&app, "c") && !hidden_flag(&app, "c:x"), "the flag is on the box itself (gpu {gpu})");
            let frame = app.visual_frame();
            assert!(frame.concealed.contains_key("c") && frame.concealed.contains_key("c:x") && !frame.concealed.contains_key("p"), "it takes its subagent box along");
            assert_eq!(frame.hidden_badges.get("p").map(|h| (h.sessions, h.agents)), Some((1, 1)));
            assert!(app.selection.is_empty(), "a hidden box leaves the selection");
            assert!(notice(&app).starts_with("Hid “c”"), "{}", notice(&app));
            // The document has it, for every window (the right-click also made it recent).
            app.flush_doc();
            let ops: Vec<Op> = sent.try_iter().find_map(|m| if let ClientMsg::DocEdit { ops_json, .. } = m { serde_json::from_str(&ops_json).ok() } else { None }).expect("an edit went out");
            assert!(ops.iter().any(|op| matches!(op, Op::NodeFields(n, f) if n.id == "c" && n.hidden && f.contains(&"hidden".to_owned()))), "{ops:?}");
            // The toolbar counts it; the parent's marker lists it.
            let screen = render(&mut app);
            assert!(screen.lines().next().unwrap_or_default().contains(" ↳ 1 hidden "), "{screen}");
            click_hit(&mut app, VisualAction::HiddenList(Some("p".into()), false));
            assert_eq!(labels(&app), ["Show ↳ c"], "gpu {gpu}");
            pick(&mut app, "Show ↳ c");
            assert!(!hidden_flag(&app, "c") && !app.visual_frame().concealed.contains_key("c"));
            assert_eq!(layout(&app), before, "back exactly where it was (gpu {gpu})");
            assert!(app.undo.is_empty(), "a toggle, never an undo step");
            assert_eq!(starts_or_stops(&sent), 0, "hiding and showing never start or stop anything");
        }
    }

    #[test]
    fn a_selection_hides_its_subsessions_and_subagent_boxes_and_the_toolbar_lists_them_by_parent() {
        let (mut app, sent) = app(false);
        app.selection = vec!["p:helper".into(), "c".into(), "o".into()];
        click(&mut app, MouseButton::Right, (45.5, 3.5), NONE);
        pick(&mut app, "Hide 2");
        assert!(hidden_flag(&app, "p:helper") && hidden_flag(&app, "c") && !hidden_flag(&app, "o"), "a top-level session stays");
        assert_eq!(app.selection, ["o"]);
        let screen = render(&mut app);
        let toolbar = screen.lines().next().unwrap_or_default().to_owned();
        assert!(toolbar.contains(" ↳ 1 hidden ") && toolbar.contains(" ◇ 2 hidden "), "{toolbar}");
        assert!(screen.contains("⋯ 1 · ◇ 2 hidden"), "{screen}");
        // The subagent counter: the switches (J first), then the boxes grouped by parent.
        click_hit(&mut app, VisualAction::HiddenList(None, true));
        assert_eq!(labels(&app), ["Hide subagents", "Hide subsessions", "Show all hidden (2)", "▸ p", "  Show ◇ p:helper", "  Show ↳ c"]);
        let Some(Modal::Menu(menu)) = app.modals.last() else { panic!("the list") };
        assert_eq!((menu.title.as_str(), menu.items[3].hint.as_str()), ("Hidden boxes", "show all 2"));
        pick(&mut app, "▸ p");
        assert!(!hidden_flag(&app, "p:helper") && !hidden_flag(&app, "c"));
        assert!(!render(&mut app).lines().next().unwrap_or_default().contains(" hidden "), "no counters once nothing is hidden");
        assert_eq!(starts_or_stops(&sent), 0);
    }

    #[test]
    fn a_hidden_box_that_needs_you_shows_on_its_parents_marker() {
        let (mut app, _sent) = app(false);
        app.set_hidden(&["c".into()], true);
        let mut waiting = crate::protocol::SessionSummary { id: "c".into(), activity: crate::protocol::Activity::Working, live: true, tracked: true, pending: 1, ..Default::default() };
        app.sessions.insert("c".into(), waiting.clone());
        let attention = super::badge_color(crate::status::Status::Waiting);
        assert!(attention.is_some());
        assert_eq!(app.visual_frame().hidden_badges.get("p").and_then(|h| h.color), attention, "hiding never hides attention");
        waiting.pending = 0;
        waiting.activity = crate::protocol::Activity::Failed;
        waiting.live = false;
        app.sessions.insert("c".into(), waiting);
        assert_eq!(app.visual_frame().hidden_badges.get("p").and_then(|h| h.color), super::badge_color(crate::status::Status::Failed), "a failure too");
        // The hidden list names its state; its own menu and the sidebar's offer Show.
        let (_, items) = app.hidden_list(Some("p"), false);
        assert_eq!(items.iter().map(|i| (i.label.as_str(), i.hint.as_str())).collect::<Vec<_>>(), [("Show ↳ c", "failed")]);
        assert!(app.node_menu("c").1.iter().any(|i| i.label == "Show" && i.action == Action::Visual(VisualAction::Show(vec!["c".into()]))));
        assert!(app.node_menu("p").1.iter().any(|i| i.label == "Show hidden (2)…"));
    }

    #[test]
    fn a_folded_subagent_box_hides_from_its_deck_or_the_details_list() {
        let (mut app, sent) = app_on(folded_doc(), false);
        let deck = app.visual_frame().deck_of("p").cloned().expect("p's deck");
        let (_, items) = app.stack_menu(&deck);
        let hide = items.iter().find(|i| i.label == "Hide ◇ p:helper").expect("the deck's menu hides a subagent box");
        app.run(hide.action.clone());
        let frame = app.visual_frame();
        assert!(frame.deck_of("p").is_none() && frame.skip("p:helper"), "its only box hidden: the deck goes");
        assert_eq!(frame.hidden_badges.get("p").map(|h| (h.sessions, h.agents)), Some((0, 1)));
        // The subsession's details list its subagent box with Hide (the parent's now says Show).
        app.view_subagents("c");
        render(&mut app);
        let hit = Hit::Toolbar(Action::Visual(VisualAction::Hide(vec!["c:x".into()])));
        assert!(app.layout.hits.iter().any(|(_, h)| *h == hit), "Hide per row");
        click_hit(&mut app, VisualAction::Hide(vec!["c:x".into()]));
        assert!(hidden_flag(&app, "c:x") && app.visual_frame().deck_of("c").is_none());
        app.view_subagents("p");
        render(&mut app);
        assert!(app.layout.hits.iter().any(|(_, h)| *h == Hit::Toolbar(Action::Visual(VisualAction::Show(vec!["p:helper".into()])))), "a hidden row offers Show");
        // Shown again: back into its deck (its session folds its subagent boxes).
        app.run(Action::Visual(VisualAction::Show(vec!["p:helper".into()])));
        assert_eq!(app.visual_frame().deck_of("p").map(|d| d.agents.clone()), Some(vec!["p:helper".to_owned()]));
        assert_eq!(starts_or_stops(&sent), 0);
    }

    #[test]
    fn the_sidebar_lists_a_hidden_box_marked_and_viewing_it_starts_nothing() {
        let (mut app, sent) = app(false);
        app.set_hidden(&["c".into()], true);
        let screen = render(&mut app);
        let sidebar = app.layout.sidebar;
        let row = screen.lines().map(|l| l.chars().skip(sidebar.x as usize).take(sidebar.width as usize).collect::<String>()).find(|l| l.contains("↳") && l.contains(" c")).expect("its sidebar row");
        assert!(row.trim_end().ends_with("hidden"), "{row:?}");
        let index = app.side_rows.iter().position(|r| r.kind == crate::app::SideKind::Session("c".into())).expect("listed");
        app.view_side_row(index);
        assert_eq!(app.selection, ["c"], "viewed (its details)");
        assert!(app.launching.is_empty() && starts_or_stops(&sent) == 0, "never started");
        assert!(app.visual_frame().skip("c"), "still hidden");
    }

    #[test]
    fn a_hidden_box_moves_with_its_parent_and_one_undo_puts_both_back() {
        for gpu in [false, true] {
            let (mut app, _sent) = app(gpu);
            app.set_hidden(&["c".into()], true);
            let before = layout(&app);
            drag(&mut app, MouseButton::Left, (5.5, 3.5), (8.5, 4.5), NONE);
            assert_eq!((at(&app, "p"), at(&app, "c"), at(&app, "c:x")), ((5, 3), (43, 3), (47, 9)), "gpu {gpu}");
            assert_eq!(at(&app, "o"), (2, 16));
            ctrl(&mut app, 'z');
            assert_eq!(layout(&app), before, "gpu {gpu}");
            // Arrow keys move it with its parent too; a hidden box itself is never moved alone.
            app.select_node("p", false);
            key(&mut app, crate::input::KeyCode::Right, NONE);
            assert_eq!(at(&app, "c"), (42, 2));
            app.selection = vec!["c".into()];
            key(&mut app, crate::input::KeyCode::Right, NONE);
            assert_eq!(at(&app, "c"), (42, 2), "gpu {gpu}");
        }
    }
}
