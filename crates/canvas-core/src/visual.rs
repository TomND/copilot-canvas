//! Canvas visuals: dust on unused sessions, stacks of subsessions under
//! their parent and hiding every subsession (with reversible animations in
//! the GPU client), and the effects collection that passive sessions put on
//! boxes. This module holds the client state, the actions, the canvas click
//! hook and what the canvas renderers need ([`VisualFrame`]).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use canvas_model::visual::{self as vm, Dust};
use ratatui::style::{Color, Modifier, Style};

use crate::app::{App, CanvasHit, Drag};
use crate::dialogs::{Action, MenuItem};
use crate::doc::{AGENT_H, Node, NodeKind, SESSION_H, SESSION_W};
use crate::effects::Effect;
use crate::input::MouseButton;
use crate::paths::now_ms;
use crate::status::Status;
use crate::view::{Motion, RenderMode, WRect};

/// Length of a stack or hide transition (GUI).
const ANIM_MS: f32 = 300.0;
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
    /// K: stack or unstack the selected session's subsessions.
    ToggleStack,
    /// H: hide or show every subsession box.
    ToggleHideSubsessions,
    /// Select the dusty session boxes (cleanup: Del removes them from the canvas).
    SelectUnused,
    /// Clears the effects on a box (node id) or a group (`group:<id>`).
    AckEffects(String),
    /// A menu of the group's passive sessions.
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
    /// Settings › Effects (this machine).
    Effects,
}

/// A stack or hide transition: its value runs from `from` towards 1
/// (`closing`: stacking, hiding) or 0 over [`ANIM_MS`].
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
    hide: Option<Transition>,
    known_hide: Option<bool>,
    /// Effects acknowledged here until the service's snapshot drops them.
    pub acked: HashSet<String>,
    /// When this client first showed each effect (its animation's start).
    seen: HashMap<String, Instant>,
    used_at: Option<Instant>,
    /// When the session history first arrived (old boxes migrate a little later,
    /// once the summaries of every tracked session are in).
    history_at: Option<Instant>,
    effect_frame: u64,
    /// An effect is animating in view (the GUI keeps ticking).
    pub effects_live: bool,
}

impl VisualState {
    /// A stack or hide transition is running (the GUI draws every frame).
    pub fn animating(&self) -> bool {
        !self.stacks.is_empty() || self.hide.is_some()
    }
}

/// Why a box is not drawn where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conceal {
    /// In the stack of this parent.
    Stack(String),
    /// Hidden subsession of this visible ancestor.
    Hidden(String),
}

impl Conceal {
    pub fn root(&self) -> &str {
        match self {
            Conceal::Stack(root) | Conceal::Hidden(root) => root,
        }
    }
}

/// A stack's deck beneath its parent.
#[derive(Clone, Debug)]
pub struct Deck {
    pub root: String,
    /// The newest stacked session (the top card).
    pub top: String,
    /// Stacked sessions, oldest first.
    pub sessions: Vec<String>,
    /// The top card (world units).
    pub rect: WRect,
    /// Opacity (fades in while stacking).
    pub alpha: f32,
}

impl Deck {
    /// Cards drawn behind the top one.
    pub fn back(&self) -> u8 {
        self.sessions.len().saturating_sub(1).min(2) as u8
    }

    /// The deck with its back cards (they peek out right and below).
    pub fn bounds(&self) -> WRect {
        let back = self.back() as f32;
        WRect::new(self.rect.x, self.rect.y, self.rect.w + back, self.rect.h + back)
    }
}

/// What the canvas shows differently this frame.
#[derive(Default)]
pub struct VisualFrame {
    /// Boxes in a stack or hidden (target state: no clicks reach them).
    pub concealed: HashMap<String, Conceal>,
    /// GPU: boxes in a transition, with their rect and opacity this frame.
    pub moving: HashMap<String, (WRect, f32)>,
    pub decks: Vec<Deck>,
    /// Visible ancestors of hidden subsessions → those sessions.
    pub hidden: HashMap<String, Vec<String>>,
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

/// Windows' "Show animations in Windows" (SPI_GETCLIENTAREAANIMATION).
pub fn system_animations() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW};
    let mut on: i32 = 1;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL to the pointer.
    let ok = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&mut on as *mut i32).cast(), 0) };
    ok == 0 || on != 0
}

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

    // ---------- stacks and hidden subsessions ----------

    /// Where a stack's deck goes: beneath its parent and the parent's subagents.
    pub fn deck_rect(&self, root: &Node) -> WRect {
        let mut bottom = root.y + SESSION_H;
        for agent in self.doc.nodes.iter().filter(|n| n.kind == NodeKind::Agent && n.parent.as_deref() == Some(root.id.as_str())) {
            let below = agent.y >= root.y && agent.x < root.x + SESSION_W && agent.x + crate::doc::AGENT_W > root.x;
            if below {
                bottom = bottom.max(agent.y + AGENT_H);
            }
        }
        WRect::new(root.x as f32 + 2.0, bottom as f32 + 1.0, SESSION_W as f32, SESSION_H as f32)
    }

    /// Starts transitions for stacks and the hide toggle that changed since
    /// the last frame (also when another window changed them).
    fn sync_transitions(&mut self) {
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
        let hide = self.doc.ui.hide_subsessions;
        if animate && self.visual.known_hide.is_some_and(|known| known != hide) {
            let from = self.visual.hide.map(|t| t.value_at(now)).unwrap_or(if hide { 0.0 } else { 1.0 });
            self.visual.hide = Some(Transition { closing: hide, start: now, from });
        }
        self.visual.known_hide = Some(hide);
        if self.visual.hide.is_some_and(|t| t.done_at(now)) {
            self.visual.hide = None;
        }
    }

    /// Boxes in a stack or hidden right now (no transitions): clicks and
    /// rubber bands pass them by.
    pub fn concealed_now(&self) -> HashMap<String, Conceal> {
        let mut out = HashMap::new();
        if self.doc.nodes.iter().any(|n| n.stacked) {
            out.extend(vm::stacked_under(&self.doc).into_iter().map(|(id, root)| (id, Conceal::Stack(root))));
        }
        if self.doc.ui.hide_subsessions {
            for (id, top) in vm::hidden_subsessions(&self.doc) {
                out.insert(id, Conceal::Hidden(top));
            }
        }
        out
    }

    /// Where a stacked or hidden box shows: its stack's deck or its visible
    /// ancestor (`None`: it shows itself).
    pub fn shown_center(&self, id: &str) -> Option<(i32, i32)> {
        match self.concealed_now().get(id)? {
            Conceal::Stack(root) => {
                let deck = self.deck_rect(self.doc.node(root)?);
                Some(((deck.x + deck.w / 2.0) as i32, (deck.y + deck.h / 2.0) as i32))
            }
            Conceal::Hidden(top) => Some(self.doc.node(top)?.rect().center()),
        }
    }

    /// What the canvas draws differently this frame: concealed boxes, decks,
    /// hidden counts, and (GUI) boxes in a transition.
    pub fn visual_frame(&mut self) -> VisualFrame {
        self.sync_transitions();
        let mut frame = VisualFrame::default();
        let stacks = self.doc.nodes.iter().any(|n| n.stacked) || !self.visual.stacks.is_empty();
        let hiding = self.doc.ui.hide_subsessions || self.visual.hide.is_some();
        if !stacks && !hiding {
            return frame;
        }
        let now = Instant::now();
        let under = if stacks { vm::stacked_under(&self.doc) } else { HashMap::new() };
        for (id, root) in &under {
            frame.concealed.insert(id.clone(), Conceal::Stack(root.clone()));
        }
        let hidden = if hiding { vm::hidden_subsessions(&self.doc) } else { HashMap::new() };
        if self.doc.ui.hide_subsessions {
            for (id, top) in &hidden {
                frame.concealed.insert(id.clone(), Conceal::Hidden(top.clone()));
                if self.doc.node(id).is_some_and(|n| n.kind == NodeKind::Session) {
                    frame.hidden.entry(top.clone()).or_default().push(id.clone());
                }
            }
        }
        // Decks: stacked parents (and parents unstacking), unless an outer
        // stack or the hide toggle covers them.
        let mut roots: Vec<String> = self.doc.nodes.iter().filter(|n| n.stacked).map(|n| n.id.clone()).collect();
        let unstacking: Vec<String> = self.visual.stacks.keys().filter(|id| !roots.contains(id)).cloned().collect();
        roots.extend(unstacking);
        for root_id in roots {
            if under.contains_key(&root_id) {
                continue;
            }
            let Some(root) = self.doc.node(&root_id) else { continue };
            let sessions = vm::descendants(&self.doc, &root_id);
            let Some(top) = sessions.last().cloned() else { continue };
            let value = self.visual.stacks.get(&root_id).map(|t| t.value_at(now)).unwrap_or(if root.stacked { 1.0 } else { 0.0 });
            let rect = self.deck_rect(root);
            if value < 1.0 {
                // Cards fly between their own places and the deck.
                for id in vm::stack_members(&self.doc, &root_id) {
                    if let Some(node) = self.doc.node(&id) {
                        let home = WRect::from(node.rect());
                        let target = WRect::new(rect.x, rect.y, home.w, home.h);
                        frame.moving.insert(id, (lerp(home, target, ease(value)), 1.0 - ease(value)));
                    }
                }
            }
            if value > 0.0 && !(self.doc.ui.hide_subsessions && hidden.contains_key(&root_id)) {
                frame.decks.push(Deck { root: root_id, top, sessions, rect, alpha: ease(value) });
            }
        }
        // Hidden subsessions shrink into (or grow out of) their visible ancestor.
        if let Some(t) = self.visual.hide {
            let value = ease(t.value_at(now));
            for (id, top) in &hidden {
                let (Some(node), Some(top)) = (self.doc.node(id), self.doc.node(top)) else { continue };
                if under.contains_key(id) {
                    continue;
                }
                let home = WRect::from(node.rect());
                let anchor = WRect::from(top.rect());
                let (cx, cy) = (anchor.x + anchor.w / 2.0, anchor.y + anchor.h / 2.0);
                let small = WRect::new(cx - home.w * 0.2, cy - home.h * 0.2, home.w * 0.4, home.h * 0.4);
                frame.moving.insert(id.clone(), (lerp(home, small, value), 1.0 - value));
            }
        }
        frame
    }

    /// The most important status among sessions (for a stack or hidden
    /// subsessions): needs you, failed, done · review, working, else `None`.
    pub fn aggregate_status(&self, ids: &[String]) -> Option<Status> {
        let rank = |s: Status| match s {
            Status::Waiting => 5,
            Status::Failed => 4,
            Status::JustFinished => 3,
            Status::Working => 2,
            _ => 0,
        };
        ids.iter().filter_map(|id| self.doc.node(id)).map(|n| self.node_status(n)).filter(|s| rank(*s) > 0).max_by_key(|s| rank(*s))
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
        self.mark_dirty();
        if stacked {
            self.notify(format!("Stacked {count} subsession(s) under “{name}”. Double-click the stack or press K to unstack."), false);
        } else {
            self.notify(format!("Unstacked {count} subsession(s) of “{name}”."), false);
        }
    }

    /// The stack (or stacked parent) a selected box belongs to.
    fn stack_target(&mut self) -> Option<(String, bool)> {
        let id = self.selection.last().cloned()?;
        let frame = self.visual_frame();
        if let Some(Conceal::Stack(root)) = frame.concealed.get(&id) {
            return Some((root.clone(), false));
        }
        let session = self.session_of(&id)?;
        if !vm::descendants(&self.doc, &session).is_empty() {
            let stacked = self.doc.node(&session).is_some_and(|n| n.stacked);
            return Some((session, !stacked));
        }
        let parent = self.doc.node(&session)?.parent.clone().filter(|p| self.doc.node(p).is_some())?;
        let stacked = self.doc.node(&parent).is_some_and(|n| n.stacked);
        Some((parent, !stacked))
    }

    pub fn toggle_hide_subsessions(&mut self) {
        self.doc.ui.hide_subsessions = !self.doc.ui.hide_subsessions;
        let count = vm::hidden_subsessions(&self.doc).keys().filter(|id| self.doc.node(id).is_some_and(|n| n.kind == NodeKind::Session)).count();
        if self.doc.ui.hide_subsessions {
            // A hidden box leaves the selection (its terminal stays open).
            let hidden = vm::hidden_subsessions(&self.doc);
            self.selection.retain(|id| !hidden.contains_key(id));
            self.notify(format!("Hid {count} subsession(s); H shows them again. They still count in the sidebar and the toolbar."), false);
        } else {
            self.notify(format!("Showing {count} subsession(s) again."), false);
        }
        self.mark_dirty();
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
    fn effects_on(&self, ids: &[String]) -> Vec<String> {
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

    /// An effect is in view (the GUI animates it).
    fn effects_in_view(&self) -> bool {
        let c = self.layout.canvas;
        if c.width == 0 {
            return false;
        }
        let (x0, y0) = self.to_world_f(c.x as f32, c.y as f32);
        let (x1, y1) = self.to_world_f(c.right() as f32, c.bottom() as f32);
        self.active_effects().iter().any(|e| {
            let rect = if e.target.is_empty() { self.doc.group(&e.group).map(|g| g.rect()) } else { self.doc.node(&e.target).map(|n| n.rect()) };
            rect.is_some_and(|r| (r.x as f32) < x1 + 8.0 && (r.x + r.w) as f32 > x0 - 8.0 && (r.y as f32) < y1 + 6.0 && (r.y + r.h) as f32 > y0 - 6.0)
        })
    }

    // ---------- the canvas click hook ----------

    /// The stack whose deck is under a cell.
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

    /// Mouse down on the canvas, before the usual handling: stacks (click
    /// views the top card, double-click unstacks, drag moves the parent with
    /// its stack, right-click its menu) and effects (a click acknowledges
    /// them and only selects the box: it never starts a session). True when
    /// the click was handled here.
    pub fn visual_mouse_down(&mut self, button: MouseButton, x: u16, y: u16, double: bool, additive: bool) -> bool {
        let frame = self.visual_frame();
        if let Some(deck) = self.deck_at(&frame, x, y) {
            match button {
                MouseButton::Left if double => {
                    self.set_stacked(&deck.root, false);
                }
                MouseButton::Left => {
                    let acks = self.effects_on(&deck.sessions);
                    self.ack_effects(acks);
                    self.select_node(&deck.top, additive);
                    self.drag = Some(Drag::Nodes { ids: vec![deck.root.clone()], start: (x, y), applied: (0, 0), moved: false, click: None });
                }
                MouseButton::Right => {
                    self.select_node(&deck.top, false);
                    let menu = self.stack_menu(&deck);
                    self.open_menu(x, y, menu);
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
                let ids = self.selection.clone();
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

    fn stack_menu(&self, deck: &Deck) -> (String, Vec<MenuItem>) {
        let top = self.doc.node(&deck.top).map(|n| n.name.clone()).unwrap_or_default();
        let root = self.doc.node(&deck.root).map(|n| n.name.clone()).unwrap_or_default();
        let mut items = vec![item("Unstack subsessions", "K · double-click", VisualAction::Stack(deck.root.clone(), false), true)];
        items.push(MenuItem { label: format!("Open “{top}”"), hint: "Enter".into(), action: Action::Open(deck.top.clone()), enabled: true });
        items.push(item(if self.doc.ui.hide_subsessions { "Show all subsessions" } else { "Hide all subsessions" }, "H", VisualAction::ToggleHideSubsessions, true));
        (format!("Stack of {} under “{root}”", deck.sessions.len()), items)
    }

    // ---------- menus ----------

    /// The session menu's stack and effect items.
    pub fn visual_node_items(&self, id: &str) -> Vec<MenuItem> {
        let mut items = Vec::new();
        let Some(node) = self.doc.node(id).filter(|n| n.kind == NodeKind::Session) else { return items };
        if !vm::descendants(&self.doc, id).is_empty() {
            let label = if node.stacked { "Unstack subsessions" } else { "Stack subsessions" };
            items.push(item(label, "K", VisualAction::Stack(id.to_owned(), !node.stacked), true));
        }
        if self.has_effect(id) {
            items.push(item("Clear effect", "click", VisualAction::AckEffects(id.to_owned()), true));
        }
        items
    }

    /// The canvas menu's items: hide or show subsessions, select unused sessions.
    pub fn visual_canvas_items(&self) -> Vec<MenuItem> {
        let ids: HashSet<&str> = self.doc.nodes.iter().map(|n| n.id.as_str()).collect();
        let any = self.doc.nodes.iter().any(|n| vm::is_subsession(&ids, n));
        let days = self.doc.dust_after_days;
        vec![
            item(if self.doc.ui.hide_subsessions { "Show all subsessions" } else { "Hide all subsessions" }, "H", VisualAction::ToggleHideSubsessions, any || self.doc.ui.hide_subsessions),
            item(&format!("Select unused sessions (older than {days} d)"), "", VisualAction::SelectUnused, days > 0),
        ]
    }

    pub fn run_visual(&mut self, action: VisualAction) {
        self.needs_draw = true;
        match action {
            VisualAction::Stack(id, stacked) => self.set_stacked(&id, stacked),
            VisualAction::ToggleStack => match self.stack_target() {
                Some((root, stacked)) => self.set_stacked(&root, stacked),
                None => self.notify("Select a session with subsessions to stack them (K).", false),
            },
            VisualAction::ToggleHideSubsessions => self.toggle_hide_subsessions(),
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
            VisualAction::GroupPassives(group) => self.group_passives_menu(&group),
            VisualAction::GroupFolders(group) => self.group_folders_menu(&group),
            VisualAction::Browse(path) => self.browse_remote(&path),
            VisualAction::PassiveFile(id, which) => self.open_passive_file(&id, which),
        }
    }

    // ---------- settings ----------

    /// The Settings view's dust and effects rows.
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
        vec![row(VisualSetting::Dust, "Dust unused sessions after", dust), row(VisualSetting::Effects, "Effects", effects)]
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
        app.visual.history_at = Some(Instant::now() - HISTORY_SETTLE);
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
