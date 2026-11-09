//! Making and configuring a canvas-wide agent like a session: the *New
//! canvas agent* dialog and its instructions editor.
//!
//! - **The dialog** (`Modal::NewAgent`): its name, its agent CLI and model
//!   (the defaults new sessions get), its instructions (saved as its
//!   `rules.md`), what wakes it (nothing by default: it runs only when you
//!   talk to it), the groups it watches (all), its canvas skills and an
//!   optional first message. *Start from…* prefills the fields from a
//!   template; templates are a convenience, never required. *Create &
//!   start* creates it and opens its chat (an explicit start); *Create* only
//!   creates it (a first message then goes to it as a message: a run answers).
//! - **Its instructions** (`Modal::AgentRules`): `rules.md` in a multi-line
//!   field, read and written through the backend (so it works remotely) and
//!   never over a change made meanwhile without a second Save.
//! - **The pickers** both share with the agent's settings form
//!   (`AgentTarget`): what wakes it, the groups it watches, its skills.

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::agents::AgentKind;
use crate::app::{App, AppEvent};
use crate::canvas_wide::TEMPLATES;
use crate::dialogs::{Action, AgentTarget, Hit, Modal, Pick, PickPurpose, Prompt, PromptPurpose, TextInput};
use crate::doc::Passive;
use crate::editor::{Editor, Outcome as EditorOutcome};
use crate::input::{KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind};
use crate::modals::Outcome;
use crate::paint::{ACCENT, DOUBLE, GRAY, MODAL, MUTED, NEGATIVE, PANEL_HI, SELECT, TEXT, bold, fg, fill, outline, put, truncate};
use crate::ui_modals::{Ctx, centered, field, panel};

/// *Start from…*: no template.
pub const BLANK: &str = "Blank";
/// The wakes picker's first row: no events and no schedule.
pub const ONLY_TALK: &str = "Only when you talk to it";
/// The wakes picker's rows for a schedule and for typed events.
pub const EVERY: &str = "Every …";
pub const DAILY: &str = "Daily at …";
pub const OTHER_EVENTS: &str = "+ Other events…";
/// The groups picker's first row.
pub const ALL_GROUPS: &str = "Every group of this canvas";
/// The instructions' placeholder.
const INSTRUCTIONS_HINT: &str = "What it does and how: what to check, whom to message, what never to do";

/// What wakes a canvas-wide agent on its own: events, an interval, a daily
/// time. None of them: it runs only when you talk to it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Wakes {
    pub events: Vec<String>,
    pub every_minutes: u32,
    pub daily_at: String,
}

impl Wakes {
    pub fn of(passive: &Passive) -> Self {
        Wakes { events: passive.subscribe.clone(), every_minutes: passive.every_minutes, daily_at: passive.daily_at.clone() }
    }

    pub fn set(&self, passive: &mut Passive) {
        passive.subscribe = self.events.clone();
        passive.every_minutes = self.every_minutes;
        passive.daily_at = self.daily_at.clone();
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.every_minutes == 0 && self.daily_at.trim().is_empty()
    }

    /// `pr.*, build.failed · every 30m · daily 09:00`, or [`ONLY_TALK`].
    pub fn text(&self) -> String {
        let mut parts = Vec::new();
        if !self.events.is_empty() {
            parts.push(self.events.join(", "));
        }
        if self.every_minutes > 0 {
            parts.push(format!("every {}", crate::app::short_interval(self.every_minutes as u64 * 60_000)));
        }
        if !self.daily_at.trim().is_empty() {
            parts.push(format!("daily {}", self.daily_at.trim()));
        }
        if parts.is_empty() { ONLY_TALK.to_owned() } else { parts.join(" · ") }
    }

    /// Ticks or unticks one event name.
    fn toggle(&mut self, event: &str) {
        match self.events.iter().position(|e| e == event) {
            Some(at) => {
                self.events.remove(at);
            }
            None => self.events.push(event.to_owned()),
        }
    }
}

/// Everything a new canvas-wide agent is made from.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentSpec {
    pub name: String,
    pub agent: AgentKind,
    /// Its own model (`None`: the default new sessions of its agent get).
    pub model: Option<String>,
    /// Its rules.md.
    pub rules: String,
    pub wakes: Wakes,
    /// Group ids it watches (empty: every group).
    pub groups: Vec<String>,
    pub skills: Vec<String>,
    /// The template it started from (empty: none).
    pub template: String,
    pub prs: Option<canvas_model::prs::PrSettings>,
    pub reviews: Option<canvas_model::reviews::ReviewSettings>,
    pub max_runs_per_day: u32,
}

impl AgentSpec {
    /// An agent called `name` with nothing set: no triggers, every group,
    /// the default model, rules to write.
    pub fn named(name: &str, agent: AgentKind) -> Self {
        AgentSpec {
            name: name.to_owned(),
            agent,
            model: None,
            rules: String::new(),
            wakes: Wakes::default(),
            groups: Vec::new(),
            skills: Vec::new(),
            template: String::new(),
            prs: None,
            reviews: None,
            max_runs_per_day: 24,
        }
    }

    /// Its rules.md: the instructions as typed (a starting text when empty).
    pub fn rules_text(&self) -> String {
        let text = self.rules.trim_end();
        if text.trim().is_empty() {
            return format!(
                "# Rules for {}\n\nDescribe the job: which events from which groups matter (see `atelier events`; `pr.*` from any group), what to check, \
                 what counts as ok, warn or alert, which sessions to message (`atelier send --to \"<name or id>\"`), and what never to do.\n",
                self.name
            );
        }
        format!("{text}\n")
    }
}

/// The fields of the dialog, in their Tab order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Template,
    Name,
    Agent,
    Model,
    Instructions,
    Wakes,
    Watches,
    Skills,
    Message,
    Start,
    Create,
    Cancel,
}

const FIELDS: [Field; 12] =
    [Field::Template, Field::Name, Field::Agent, Field::Model, Field::Instructions, Field::Wakes, Field::Watches, Field::Skills, Field::Message, Field::Start, Field::Create, Field::Cancel];

impl Field {
    /// The text fields (Space types there; elsewhere it activates).
    fn is_text(self) -> bool {
        matches!(self, Field::Name | Field::Message | Field::Instructions)
    }
}

/// The *New canvas agent* dialog.
#[derive(Clone, Debug)]
pub struct NewAgent {
    /// The template it started from (an index of [`TEMPLATES`]; `None`: blank).
    pub template: Option<usize>,
    pub name: TextInput,
    pub agent: AgentKind,
    /// The model chosen here (`Some("")`: the agent's own default); `None`:
    /// the default new sessions of its agent get.
    pub model: Option<String>,
    pub instructions: Editor,
    pub wakes: Wakes,
    pub groups: Vec<String>,
    pub skills: Vec<String>,
    pub message: TextInput,
    pub focus: Field,
    pub error: Option<String>,
    /// Settings a template brings that have no field here.
    pub prs: Option<canvas_model::prs::PrSettings>,
    pub reviews: Option<canvas_model::reviews::ReviewSettings>,
    pub max_runs_per_day: u32,
    /// The name and instructions a template filled in (another template
    /// replaces them unless you changed them).
    filled: (String, String),
    /// Where the instructions field was drawn (mouse clicks place the caret).
    pub editor_rect: Rect,
}

impl NewAgent {
    pub fn new(agent: AgentKind) -> Self {
        let mut instructions = Editor::new("");
        instructions.set_placeholder(INSTRUCTIONS_HINT);
        NewAgent {
            template: None,
            name: TextInput::default(),
            agent,
            model: None,
            instructions,
            wakes: Wakes::default(),
            groups: Vec::new(),
            skills: Vec::new(),
            message: TextInput::default(),
            focus: Field::Name,
            error: None,
            prs: None,
            reviews: None,
            max_runs_per_day: 24,
            filled: (String::new(), String::new()),
            editor_rect: Rect::default(),
        }
    }

    /// Prefills the dialog from a template (`None`: blank). A name or
    /// instructions you changed stay; triggers and settings are the template's.
    pub fn start_from(&mut self, template: Option<usize>) {
        let found = template.and_then(|t| TEMPLATES.get(t));
        let (name, rules) = found.map(|t| (t.name.to_owned(), t.rules.to_owned())).unwrap_or_default();
        let (old_name, old_rules) = self.filled.clone();
        if self.name.text.trim().is_empty() || self.name.text == old_name {
            self.name.set(&name);
        }
        let current = self.instructions.text();
        if current.trim().is_empty() || current == old_rules {
            self.instructions.set_text(&rules);
        }
        self.filled = (name, rules);
        self.template = found.map(|_| template.unwrap_or_default());
        self.wakes = found.map(|t| Wakes { events: t.subscribe.iter().map(|s| s.to_string()).collect(), every_minutes: t.every_minutes, daily_at: t.daily_at.to_owned() }).unwrap_or_default();
        let named = found.map(|t| t.name).unwrap_or_default();
        // The PR coordinator monitors handed-off pull requests (read-only follow-ups);
        // the review follow-up's channel is yours to set, approval on, and it checks hourly.
        self.prs = (named == "PR coordinator").then(canvas_model::prs::PrSettings::default);
        self.reviews = (named == "Review follow-up").then(canvas_model::reviews::ReviewSettings::default);
        self.max_runs_per_day = if named == "Review follow-up" { 60 } else { 24 };
        self.error = None;
    }

    /// What creating it makes.
    pub fn spec(&self) -> AgentSpec {
        AgentSpec {
            name: self.name.text.trim().chars().take(80).collect(),
            agent: self.agent,
            model: self.model.clone(),
            rules: self.instructions.text(),
            wakes: self.wakes.clone(),
            groups: self.groups.clone(),
            skills: self.skills.clone(),
            template: self.template.and_then(|t| TEMPLATES.get(t)).map(|t| t.name.to_owned()).unwrap_or_default(),
            prs: self.prs.clone(),
            reviews: self.reviews.clone(),
            max_runs_per_day: self.max_runs_per_day,
        }
    }

    fn cycle(&mut self, forward: bool) {
        let at = FIELDS.iter().position(|f| *f == self.focus).unwrap_or(0);
        self.focus = FIELDS[if forward { (at + 1) % FIELDS.len() } else { (at + FIELDS.len() - 1) % FIELDS.len() }];
    }

    fn cycle_agent(&mut self, forward: bool) {
        let all = crate::agents::ALL;
        let at = all.iter().position(|k| *k == self.agent).unwrap_or(0);
        self.agent = all[if forward { (at + 1) % all.len() } else { (at + all.len() - 1) % all.len() }];
        // Model ids are the agent's own: a choice for another agent does not carry over.
        self.model = None;
    }
}

/// The new canvas agent dialog on top, or under the picker on top.
pub fn dialog_mut(app: &mut App) -> Option<&mut NewAgent> {
    app.modals.iter_mut().rev().find_map(|m| if let Modal::NewAgent(d) = m { Some(&mut **d) } else { None })
}

pub fn dialog(app: &App) -> Option<&NewAgent> {
    app.modals.iter().rev().find_map(|m| if let Modal::NewAgent(d) = m { Some(&**d) } else { None })
}

/// Opens the dialog (nothing is created until Create).
pub fn open(app: &mut App) {
    if let Some(why) = app.canvas_agents_unavailable() {
        return app.notify(format!("Canvas-wide agents are unavailable: {why}"), true);
    }
    app.load_canvas_skills();
    let agent = app.default_agent();
    if !app.agent_configs.contains_key(&agent) {
        app.refresh_config_for(agent);
    }
    app.modals.push(Modal::NewAgent(Box::new(NewAgent::new(agent))));
    app.needs_draw = true;
}

/// Creates the agent; with `start`, opens its chat (the explicit start),
/// else a first message goes to it as a message.
fn submit(app: &mut App, dialog: &mut NewAgent, start: bool) -> Outcome {
    let spec = dialog.spec();
    if spec.name.is_empty() {
        dialog.error = Some("Give it a name.".into());
        dialog.focus = Field::Name;
        return Outcome::Keep;
    }
    if !app.demo && !app.installed_agents.is_empty() && !app.installed_agents.contains(&spec.agent) {
        dialog.error = Some(format!("{} is not installed on the service's machine.", spec.agent.label()));
        dialog.focus = Field::Agent;
        return Outcome::Keep;
    }
    let message = dialog.message.text.trim().to_owned();
    match app.create_canvas_agent(spec) {
        Ok(id) => {
            if start {
                app.open_agent_chat(&id, Some(&message));
            } else if !message.is_empty()
                && let Err(error) = app.message_passive(&id, &message)
            {
                app.notify(error, true);
            }
            Outcome::Close
        }
        Err(error) => {
            dialog.error = Some(error);
            Outcome::Keep
        }
    }
}

/// A key in the dialog.
pub fn key(app: &mut App, dialog: &mut NewAgent, key: KeyEvent) -> Outcome {
    let ctrl = key.modifiers.contains(Modifiers::CONTROL);
    match key.code {
        KeyCode::Esc => return Outcome::Close,
        KeyCode::Tab => {
            dialog.cycle(true);
            return Outcome::Keep;
        }
        KeyCode::BackTab => {
            dialog.cycle(false);
            return Outcome::Keep;
        }
        // Ctrl+Enter creates and starts it from any field (Enter makes new lines in the instructions).
        KeyCode::Enter if ctrl => return submit(app, dialog, true),
        _ => {}
    }
    if dialog.focus == Field::Instructions {
        match dialog.instructions.key(&key) {
            EditorOutcome::Copy(text) | EditorOutcome::Cut(text) => app.copy_to_clipboard(&text),
            EditorOutcome::Unhandled if matches!(key.code, KeyCode::Up | KeyCode::Down) => dialog.cycle(key.code == KeyCode::Down),
            _ => {}
        }
        dialog.error = None;
        return Outcome::Keep;
    }
    match key.code {
        KeyCode::Up => {
            dialog.cycle(false);
            return Outcome::Keep;
        }
        KeyCode::Down => {
            dialog.cycle(true);
            return Outcome::Keep;
        }
        _ => {}
    }
    let activate = key.code == KeyCode::Enter || (key.code == KeyCode::Char(' ') && !dialog.focus.is_text());
    let target = AgentTarget::Dialog;
    match dialog.focus {
        Field::Agent if activate || matches!(key.code, KeyCode::Left | KeyCode::Right) => {
            dialog.cycle_agent(key.code != KeyCode::Left);
            if !app.agent_configs.contains_key(&dialog.agent) {
                app.refresh_config_for(dialog.agent);
            }
            dialog.error = None;
            Outcome::Keep
        }
        Field::Template if activate => Outcome::KeepAndRun(Action::Agents(crate::canvas_wide::AgentAction::Pick(PickPurpose::AgentTemplate))),
        Field::Model if activate => Outcome::KeepAndRun(Action::Agents(crate::canvas_wide::AgentAction::Model(None))),
        Field::Wakes if activate => Outcome::KeepAndRun(Action::Agents(crate::canvas_wide::AgentAction::Pick(PickPurpose::AgentWakes(target)))),
        Field::Watches if activate => Outcome::KeepAndRun(Action::Agents(crate::canvas_wide::AgentAction::Pick(PickPurpose::AgentGroups(target)))),
        Field::Skills if activate => Outcome::KeepAndRun(Action::Agents(crate::canvas_wide::AgentAction::Pick(PickPurpose::AgentSkills(target)))),
        Field::Cancel if activate => Outcome::Close,
        Field::Create if activate => submit(app, dialog, false),
        // Enter on a text field (or Create & start) creates and starts it, as in the new-session dialog.
        _ if key.code == KeyCode::Enter => submit(app, dialog, true),
        Field::Start if activate => submit(app, dialog, true),
        Field::Name => {
            if dialog.name.handle(&key) {
                dialog.error = None;
            }
            Outcome::Keep
        }
        Field::Message => {
            dialog.message.handle(&key);
            Outcome::Keep
        }
        _ => Outcome::Keep,
    }
}

/// Pasted text goes into the focused text field.
pub fn paste(dialog: &mut NewAgent, text: &str) {
    match dialog.focus {
        Field::Instructions => {
            dialog.instructions.insert_text(text);
        }
        Field::Message => dialog.message.insert(text),
        _ => {
            dialog.focus = Field::Name;
            dialog.name.insert(text);
        }
    }
    dialog.error = None;
}

/// The mouse in the dialog: its fields (`Hit::ListRow(index in the Tab
/// order)`), the instructions field, and its buttons.
pub fn mouse(app: &mut App, dialog: &mut NewAgent, mouse: MouseEvent, hit: Option<Hit>) -> Outcome {
    let inside = |r: Rect| r.width > 0 && crate::paint::rect_contains(r, mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if inside(dialog.editor_rect) => {
            dialog.instructions.scroll(if mouse.kind == MouseEventKind::ScrollUp { -3 } else { 3 });
            return Outcome::Keep;
        }
        MouseEventKind::Drag(MouseButton::Left) if dialog.focus == Field::Instructions && dialog.editor_rect.width > 0 => {
            dialog.instructions.drag(dialog.editor_rect, mouse.column, mouse.row);
            return Outcome::Keep;
        }
        MouseEventKind::Down(MouseButton::Left) => {}
        _ => return Outcome::Keep,
    }
    if inside(dialog.editor_rect) {
        dialog.focus = Field::Instructions;
        dialog.instructions.click(dialog.editor_rect, mouse.column, mouse.row, mouse.modifiers.contains(Modifiers::SHIFT));
        return Outcome::Keep;
    }
    match hit {
        Some(Hit::ListRow(i)) if i < FIELDS.len() => {
            dialog.focus = FIELDS[i];
            if !dialog.focus.is_text() {
                return key(app, dialog, KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
            }
            Outcome::Keep
        }
        _ => Outcome::Keep,
    }
}

/// A dialog button that focuses its field when clicked (`Hit::ListRow`).
#[allow(clippy::too_many_arguments)]
fn field_button(buf: &mut Buffer, ctx: &mut Ctx, x: i32, y: i32, label: &str, focused: bool, primary: bool, field: Field, clip: Rect) -> i32 {
    let style = if focused {
        Style::default().fg(ratatui::style::Color::Black).bg(if primary { ACCENT } else { GRAY }).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(if primary { ACCENT } else { TEXT }).bg(PANEL_HI)
    };
    let end = put(buf, x, y, &format!(" {label} "), style, clip);
    ctx.hit(Rect::new(x as u16, y as u16, (end - x).max(0) as u16, 1), Hit::ListRow(index(field)));
    end + 2
}

fn index(field: Field) -> usize {
    FIELDS.iter().position(|f| *f == field).unwrap_or(0)
}

/// A one-line value that opens a picker (`▾`), like the new-session dialog's Model row.
#[allow(clippy::too_many_arguments)]
fn picker_row(buf: &mut Buffer, ctx: &mut Ctx, x: i32, y: i32, width: i32, text: &str, focused: bool, field: Field, clip: Rect) {
    let style = if focused { Style::default().fg(TEXT).bg(PANEL_HI).add_modifier(Modifier::UNDERLINED) } else { Style::default().fg(TEXT).bg(PANEL_HI) };
    fill(buf, x, y, width, 1, style, clip);
    put(buf, x, y, &truncate(text, width.max(0) as usize), style, clip);
    ctx.hit(Rect::new(x as u16, y as u16, width.max(0) as u16, 1), Hit::ListRow(index(field)));
}

/// The groups a canvas-wide agent watches, by name (every group when none).
pub fn groups_text(app: &App, groups: &[String]) -> String {
    if groups.is_empty() {
        return "every group of this canvas".into();
    }
    groups.iter().map(|g| app.doc.group(g).map(|g| g.name.clone()).unwrap_or_else(|| format!("(removed {g})"))).collect::<Vec<_>>().join(", ")
}

/// Draws the dialog (its instructions field keeps where it scrolled).
pub fn draw(app: &App, buf: &mut Buffer, screen: Rect, dialog: &mut NewAgent, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    let rect = centered(screen, 96, 28);
    let title = "New canvas agent";
    panel(buf, rect, title);
    outline(buf, rect.x as i32, rect.y as i32, rect.width as i32, rect.height as i32, &DOUBLE, fg(ACCENT).bg(MODAL), rect);
    put(buf, rect.x as i32 + 2, rect.y as i32, &format!(" {title} "), bold(TEXT).bg(MODAL), rect);
    let label_x = rect.x as i32 + 3;
    let field_x = rect.x as i32 + 18;
    let field_w = rect.width as i32 - 21;
    let focus = dialog.focus;
    let label = |buf: &mut Buffer, y: i32, text: &str, focused: bool| {
        put(buf, label_x, y, text, if focused { bold(ACCENT).bg(MODAL) } else { fg(GRAY).bg(MODAL) }, rect);
    };
    let mut cursor = None;
    let mut y = rect.y as i32 + 2;

    label(buf, y, "Start from", focus == Field::Template);
    let template = dialog.template.and_then(|t| TEMPLATES.get(t)).map(|t| t.name).unwrap_or(BLANK);
    picker_row(buf, ctx, field_x, y, field_w, &format!("{template}  ▾ · optional: a template prefills the fields below"), focus == Field::Template, Field::Template, rect);
    y += 2;

    label(buf, y, "Name", focus == Field::Name);
    cursor = field(buf, field_x, y, field_w, &dialog.name, focus == Field::Name, "what to call it, for example Release watch", rect).or(cursor);
    ctx.hit(Rect::new(field_x as u16, y as u16, field_w.max(0) as u16, 1), Hit::ListRow(index(Field::Name)));
    y += 2;

    label(buf, y, "Agent", focus == Field::Agent);
    {
        let style = if focus == Field::Agent { bold(SELECT).bg(MODAL) } else { fg(TEXT).bg(MODAL) };
        let mut x = field_x;
        for kind in crate::agents::ALL {
            let on = kind == dialog.agent;
            let missing = !app.demo && !app.installed_agents.contains(&kind);
            let text = format!("{} {}{}", if on { "◉" } else { "○" }, kind.label(), if missing { " (not installed)" } else { "" });
            x = put(buf, x, y, &text, if missing && !on { fg(MUTED).bg(MODAL) } else { style }, rect) + 3;
        }
        ctx.hit(Rect::new(field_x as u16, y as u16, (x - field_x).max(1) as u16, 1), Hit::ListRow(index(Field::Agent)));
    }
    y += 2;

    label(buf, y, "Model", focus == Field::Model);
    let shown = app.new_agent_model(dialog);
    let model = crate::models::shown(&shown.model.unwrap_or_default());
    picker_row(buf, ctx, field_x, y, field_w, &format!("{model}  ▾ · {}", shown.note), focus == Field::Model, Field::Model, rect);
    y += 2;

    label(buf, y, "Instructions", focus == Field::Instructions);
    put(buf, label_x, y + 1, "(rules.md)", fg(MUTED).bg(MODAL), rect);
    let area = Rect::new(field_x as u16, y as u16, field_w.max(1) as u16, 7).intersection(rect);
    fill(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, Style::default().bg(PANEL_HI), rect);
    cursor = dialog.instructions.draw(buf, area, focus == Field::Instructions).or(cursor);
    dialog.editor_rect = area;
    y += 8;

    label(buf, y, "Wakes on", focus == Field::Wakes);
    picker_row(buf, ctx, field_x, y, field_w, &format!("{}  ▾", dialog.wakes.text()), focus == Field::Wakes, Field::Wakes, rect);
    y += 1;
    label(buf, y, "Watches", focus == Field::Watches);
    picker_row(buf, ctx, field_x, y, field_w, &format!("{}  ▾", groups_text(app, &dialog.groups)), focus == Field::Watches, Field::Watches, rect);
    y += 1;
    label(buf, y, "Skills", focus == Field::Skills);
    let skills = if dialog.skills.is_empty() { "none (from the canvas skill library)".to_owned() } else { dialog.skills.join(", ") };
    picker_row(buf, ctx, field_x, y, field_w, &format!("{skills}  ▾"), focus == Field::Skills, Field::Skills, rect);
    y += 2;

    label(buf, y, "First message", focus == Field::Message);
    cursor = field(buf, field_x, y, field_w, &dialog.message, focus == Field::Message, "optional: what to ask it first (in its chat; with Create, a run answers it)", rect).or(cursor);
    ctx.hit(Rect::new(field_x as u16, y as u16, field_w.max(0) as u16, 1), Hit::ListRow(index(Field::Message)));
    y += 2;

    let (hint, style) = match &dialog.error {
        Some(error) => (error.clone(), fg(NEGATIVE)),
        None => ("Tab/↑↓ move · Enter: create & start (a new line in the instructions) · Esc: cancel".to_owned(), fg(MUTED)),
    };
    put(buf, label_x, y, &truncate(&hint, rect.width as usize - 6), style.bg(MODAL), rect);
    let y = rect.bottom() as i32 - 2;
    let x = field_button(buf, ctx, label_x, y, "Create & start", focus == Field::Start, true, Field::Start, rect);
    let x = field_button(buf, ctx, x, y, "Create", focus == Field::Create, false, Field::Create, rect);
    let x = field_button(buf, ctx, x, y, "Cancel", focus == Field::Cancel, false, Field::Cancel, rect);
    put(buf, x, y, &truncate("Create & start opens its chat; Create only adds it", (rect.right() as i32 - x - 2).max(0) as usize), fg(MUTED).bg(MODAL), rect);
    (rect, cursor)
}

// ---------- the pickers the dialog and the settings form share ----------

fn mark(on: bool) -> &'static str {
    if on { "✓" } else { "○" }
}

/// A picker row's name without its ✓ / ○.
fn unmarked(value: &str) -> &str {
    value.trim_start_matches(['✓', '○']).trim()
}

fn passive_mut<'a>(app: &'a mut App, id: &str) -> Option<&'a mut Passive> {
    app.doc.node_mut(id).and_then(|n| n.passive.as_mut())
}

/// An agent's settings changed: the canvas and the service's routing follow.
fn changed(app: &mut App, target: &AgentTarget) {
    if let AgentTarget::Node(_) = target {
        app.mark_dirty();
        app.sync_routing();
    }
    app.needs_draw = true;
}

fn target_name(app: &App, target: &AgentTarget) -> String {
    match target {
        AgentTarget::Dialog => "the new agent".into(),
        AgentTarget::Node(id) => format!("“{}”", app.doc.node(id).map(|n| n.name.clone()).unwrap_or_default()),
    }
}

pub fn wakes_of(app: &App, target: &AgentTarget) -> Option<Wakes> {
    match target {
        AgentTarget::Dialog => dialog(app).map(|d| d.wakes.clone()),
        AgentTarget::Node(id) => app.doc.node(id).and_then(|n| n.passive.as_ref()).map(Wakes::of),
    }
}

pub fn set_wakes(app: &mut App, target: &AgentTarget, wakes: Wakes) {
    match target {
        AgentTarget::Dialog => {
            if let Some(d) = dialog_mut(app) {
                d.wakes = wakes;
            }
        }
        AgentTarget::Node(id) => {
            if let Some(p) = passive_mut(app, id) {
                wakes.set(p);
            }
        }
    }
    changed(app, target);
}

pub fn groups_of(app: &App, target: &AgentTarget) -> Option<Vec<String>> {
    match target {
        AgentTarget::Dialog => dialog(app).map(|d| d.groups.clone()),
        AgentTarget::Node(id) => app.doc.node(id).and_then(|n| n.passive.as_ref()).map(|p| p.groups.clone()),
    }
}

fn set_groups(app: &mut App, target: &AgentTarget, groups: Vec<String>) {
    match target {
        AgentTarget::Dialog => {
            if let Some(d) = dialog_mut(app) {
                d.groups = groups;
            }
        }
        AgentTarget::Node(id) => {
            if let Some(p) = passive_mut(app, id) {
                p.groups = groups;
            }
        }
    }
    changed(app, target);
}

pub fn skills_of(app: &App, target: &AgentTarget) -> Option<Vec<String>> {
    match target {
        AgentTarget::Dialog => dialog(app).map(|d| d.skills.clone()),
        AgentTarget::Node(id) => app.doc.node(id).and_then(|n| n.passive.as_ref()).map(|p| p.skills.clone()),
    }
}

pub fn set_skills(app: &mut App, target: &AgentTarget, skills: Vec<String>) {
    match target {
        AgentTarget::Dialog => {
            if let Some(d) = dialog_mut(app) {
                d.skills = skills;
            }
        }
        AgentTarget::Node(id) => {
            if let Some(p) = passive_mut(app, id) {
                p.skills = skills;
            }
        }
    }
    changed(app, target);
}

/// *Start from…*: Blank or a template.
pub fn pick_template(app: &mut App) {
    let mut items = vec![(BLANK.to_owned(), "no template: your own instructions, woken only when you talk to it".to_owned())];
    items.extend(TEMPLATES.iter().map(|t| (t.name.to_owned(), t.about.to_owned())));
    app.modals.push(Modal::Pick(Pick { purpose: PickPurpose::AgentTemplate, title: "Start from (prefills the new agent)".into(), items, filter: String::new(), sel: 0, scroll: 0 }));
}

/// Event families and names a canvas-wide agent can wake on: families
/// first, then the default catalog's events.
fn wake_events() -> Vec<(String, String)> {
    let mut events: Vec<(String, String)> = [
        ("pr.*", "every pull request event of any group"),
        ("review.*", "review follow-ups (handed-off reviews, approvals, hourly checks)"),
        ("todo.*", "the todo boards' events"),
        ("*", "every event of the canvas"),
    ]
    .iter()
    .map(|(n, w)| (n.to_string(), w.to_string()))
    .collect();
    events.extend(crate::bus::parse_catalog(crate::bus::DEFAULT_CATALOG).into_iter().map(|e| (e.name, e.when)));
    events
}

/// What wakes an agent (Enter ticks or unticks a row; the schedule rows ask).
pub fn pick_wakes(app: &mut App, target: AgentTarget, sel: usize) {
    let Some(wakes) = wakes_of(app, &target) else { return };
    let mut items = vec![
        (format!("{} {ONLY_TALK}", mark(wakes.is_empty())), "no events or schedule: messages, its chat and Run now".to_owned()),
        (format!("{EVERY} {}", if wakes.every_minutes > 0 { crate::app::short_interval(wakes.every_minutes as u64 * 60_000) } else { "off".into() }), "an interval, for example 30m or 2h".to_owned()),
        (format!("{DAILY} {}", if wakes.daily_at.trim().is_empty() { "off" } else { wakes.daily_at.trim() }), "a local time, for example 09:00".to_owned()),
        (OTHER_EVENTS.to_owned(), "type event names (comma-separated; prefix.* matches a family)".to_owned()),
    ];
    let known = wake_events();
    for event in wakes.events.iter().filter(|e| !known.iter().any(|(k, _)| k == *e)) {
        items.push((format!("✓ {event}"), "typed".into()));
    }
    items.extend(known.into_iter().map(|(name, when)| (format!("{} {name}", mark(wakes.events.contains(&name))), when)));
    let title = format!("Wakes {} on (Enter: tick / untick)", target_name(app, &target));
    app.modals.push(Modal::Pick(Pick { purpose: PickPurpose::AgentWakes(target), title, sel: sel.min(items.len() - 1), items, filter: String::new(), scroll: 0 }));
}

/// The groups an agent watches (every group, or the ones ticked).
pub fn pick_groups(app: &mut App, target: AgentTarget, sel: usize) {
    let Some(groups) = groups_of(app, &target) else { return };
    let mut items = vec![(format!("{} {ALL_GROUPS}", mark(groups.is_empty())), "groups made later too".to_owned())];
    for group in app.doc.groups.iter().filter(|g| !g.is_canvas_agents() && !g.is_automations()) {
        items.push((format!("{} {}", mark(groups.contains(&group.id)), group.name), group.folder.clone().unwrap_or_default()));
    }
    for gone in groups.iter().filter(|g| app.doc.group(g).is_none()) {
        items.push((format!("✓ {gone}"), "this group was removed".into()));
    }
    let title = format!("Groups {} watches (Enter: tick / untick)", target_name(app, &target));
    app.modals.push(Modal::Pick(Pick { purpose: PickPurpose::AgentGroups(target), title, sel: sel.min(items.len() - 1), items, filter: String::new(), scroll: 0 }));
}

/// The agent CLI of a canvas-wide agent on the canvas.
pub fn pick_cli(app: &mut App, id: &str) {
    let Some(node) = app.doc.node(id) else { return };
    let current = crate::agents::agent_of(node);
    let items = crate::agents::ALL
        .iter()
        .map(|kind| {
            let note = if *kind == current { "now: its conversation continues" } else if !app.installed_agents.is_empty() && !app.installed_agents.contains(kind) { "not installed on the service's machine" } else { "a fresh conversation (its files stay)" };
            (format!("{} {}", if *kind == current { "◉" } else { "○" }, kind.label()), note.to_owned())
        })
        .collect();
    let title = format!("Agent CLI of “{}”", node.name);
    app.modals.push(Modal::Pick(Pick { purpose: PickPurpose::AgentCli(id.to_owned()), title, items, filter: String::new(), sel: 0, scroll: 0 }));
}

/// A choice in one of this module's pickers.
pub fn apply_pick(app: &mut App, purpose: PickPurpose, value: &str) {
    match purpose {
        PickPurpose::AgentTemplate => {
            let template = TEMPLATES.iter().position(|t| t.name == value);
            if let Some(d) = dialog_mut(app) {
                d.start_from(template);
            }
        }
        PickPurpose::AgentWakes(target) => {
            let sel = pick_index(app, &target, value);
            if value.starts_with(EVERY.trim_end_matches(" …")) {
                return ask(app, PromptPurpose::AgentEvery(target), "Wakes every", "an interval: 30m, 2h, 1d · empty or off: none", "");
            }
            if value.starts_with(DAILY.trim_end_matches(" …")) {
                return ask(app, PromptPurpose::AgentDaily(target), "Wakes daily at", "a local time HH:MM, for example 09:00 · empty or off: none", "");
            }
            if value == OTHER_EVENTS {
                return ask(app, PromptPurpose::AgentEvents(target), "Wakes on these events too", "event names, comma-separated (see atelier events); prefix.* matches a family", "");
            }
            let Some(mut wakes) = wakes_of(app, &target) else { return };
            match unmarked(value) {
                ONLY_TALK => wakes = Wakes::default(),
                event => wakes.toggle(event),
            }
            set_wakes(app, &target, wakes);
            pick_wakes(app, target, sel);
        }
        PickPurpose::AgentGroups(target) => {
            let Some(mut groups) = groups_of(app, &target) else { return };
            let name = unmarked(value);
            if name == ALL_GROUPS {
                groups.clear();
            } else if let Some(at) = groups.iter().position(|g| g == name || app.doc.group(g).is_some_and(|g| g.name == name)) {
                groups.remove(at);
            } else if let Some(group) = app.doc.groups.iter().find(|g| !g.is_canvas_agents() && g.name == name) {
                groups.push(group.id.clone());
            }
            let sel = std::iter::once(ALL_GROUPS.to_owned()).chain(app.doc.groups.iter().filter(|g| !g.is_canvas_agents() && !g.is_automations()).map(|g| g.name.clone())).position(|n| n == name).unwrap_or(0);
            set_groups(app, &target, groups);
            pick_groups(app, target, sel);
        }
        PickPurpose::AgentCli(id) => {
            let chosen = crate::agents::ALL.into_iter().find(|k| unmarked(value.trim_start_matches(['◉', '○'])) == k.label());
            if let Some(kind) = chosen
                && let Err(error) = app.set_agent_cli(&id, kind)
            {
                app.notify(error, true);
            }
        }
        _ => {}
    }
}

/// The row a wakes picker's value is on (so the picker opens there again).
fn pick_index(app: &App, target: &AgentTarget, value: &str) -> usize {
    let Some(wakes) = wakes_of(app, target) else { return 0 };
    let name = unmarked(value);
    if name == ONLY_TALK {
        return 0;
    }
    let known = wake_events();
    let typed = wakes.events.iter().filter(|e| !known.iter().any(|(k, _)| k == *e)).count();
    known.iter().position(|(k, _)| k == name).map(|i| 4 + typed + i).or_else(|| wakes.events.iter().filter(|e| !known.iter().any(|(k, _)| k == *e)).position(|e| e == name).map(|i| 4 + i)).unwrap_or(0)
}

fn ask(app: &mut App, purpose: PromptPurpose, title: &str, hint: &str, value: &str) {
    app.modals.push(Modal::Prompt(Prompt { title: title.into(), hint: hint.into(), input: TextInput::new(value), purpose }));
}

/// A schedule or events typed in one of this module's prompts.
pub fn apply_prompt(app: &mut App, purpose: PromptPurpose, value: &str) -> Result<(), String> {
    let value = value.trim();
    let off = value.is_empty() || ["off", "none", "0"].iter().any(|o| value.eq_ignore_ascii_case(o));
    let (target, row) = match purpose {
        PromptPurpose::AgentEvery(target) => {
            let minutes = if off {
                0
            } else {
                let text = value.to_ascii_lowercase();
                let text = text.trim_start_matches("every").trim();
                let ms = crate::bus::parse_duration(text).or_else(|| text.parse::<u64>().ok().map(|m| m * 60_000)).ok_or_else(|| format!("not an interval: {value} (for example 30m or 2h)"))?;
                ((ms / 60_000).max(1)) as u32
            };
            let mut wakes = wakes_of(app, &target).ok_or("it was removed")?;
            wakes.every_minutes = minutes;
            set_wakes(app, &target, wakes);
            (target, 1)
        }
        PromptPurpose::AgentDaily(target) => {
            let daily = if off {
                String::new()
            } else {
                let text = value.to_ascii_lowercase();
                let text = text.trim_start_matches("daily").trim().trim_start_matches("at").trim();
                let minutes = crate::bus::parse_hhmm(text).ok_or_else(|| format!("not a time: {value} (HH:MM, for example 09:00)"))?;
                format!("{:02}:{:02}", minutes / 60, minutes % 60)
            };
            let mut wakes = wakes_of(app, &target).ok_or("it was removed")?;
            wakes.daily_at = daily;
            set_wakes(app, &target, wakes);
            (target, 2)
        }
        PromptPurpose::AgentEvents(target) => {
            let mut wakes = wakes_of(app, &target).ok_or("it was removed")?;
            for name in value.split([',', ' ']).map(str::trim).filter(|s| !s.is_empty()) {
                if !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '*')) {
                    return Err(format!("not an event name: {name}"));
                }
                if !wakes.events.iter().any(|e| e == name) {
                    wakes.events.push(name.to_owned());
                }
            }
            set_wakes(app, &target, wakes);
            (target, 3)
        }
        _ => return Ok(()),
    };
    pick_wakes(app, target, row);
    Ok(())
}

// ---------- its instructions (rules.md) ----------

/// The editor of a canvas-wide agent's instructions (its rules.md).
#[derive(Clone, Debug)]
pub struct RulesEditor {
    /// The agent's box.
    pub id: String,
    pub name: String,
    /// rules.md on the service's machine, and the canvas-agents folder it is in.
    pub path: String,
    pub root: String,
    pub editor: Editor,
    /// The revision it was read at (`None`: not read yet, or a new file).
    pub rev: Option<String>,
    pub loading: bool,
    pub saving: bool,
    pub error: Option<String>,
    /// Esc with unsaved changes asks once; a second Esc discards them.
    pub discard: bool,
    pub editor_rect: Rect,
}

pub fn rules_editor(app: &App) -> Option<&RulesEditor> {
    app.modals.iter().rev().find_map(|m| if let Modal::AgentRules(r) = m { Some(&**r) } else { None })
}

fn rules_editor_mut<'a>(app: &'a mut App, id: &str) -> Option<&'a mut RulesEditor> {
    app.modals.iter_mut().rev().find_map(|m| if let Modal::AgentRules(r) = m { Some(&mut **r) } else { None }).filter(|r| r.id == id)
}

/// Opens an agent's instructions in the editor (read through the backend).
pub fn open_rules(app: &mut App, id: &str) {
    let Some(node) = app.doc.node(id).cloned() else { return };
    let (Some(paths), Some(root)) = (app.passive_paths(&node), app.canvas_agents_path()) else {
        return app.notify(format!("Its files are unavailable: {}", app.canvas_agents_unavailable().unwrap_or_else(|| "it has no folder".into())), true);
    };
    let mut editor = Editor::new("");
    editor.set_placeholder(INSTRUCTIONS_HINT);
    let path = paths.rules.display().to_string();
    app.modals.push(Modal::AgentRules(Box::new(RulesEditor {
        id: id.to_owned(),
        name: node.name.clone(),
        path: path.clone(),
        root,
        editor,
        rev: None,
        loading: true,
        saving: false,
        error: None,
        discard: false,
        editor_rect: Rect::default(),
    })));
    load_rules(app, id, path);
}

fn load_rules(app: &mut App, id: &str, path: String) {
    let id = id.to_owned();
    app.in_background(
        move |backend| match backend.note_read(&path) {
            Ok(note) => Ok((note.text, Some(note.rev))),
            // An older service: read it plainly (saving then says the service is too old).
            Err(error) if format!("{error:#}").contains("does not support") => backend.read_text(&path, 1 << 20).map(|t| (t, None)).map_err(|e| format!("{e:#}")),
            Err(error) => Err(format!("{error:#}")),
        },
        move |result| {
            AppEvent::Run(Box::new(move |app: &mut App| {
                if let Ok((text, _)) = &result {
                    remember_rules(app, &id, text);
                }
                let Some(open) = rules_editor_mut(app, &id) else { return };
                open.loading = false;
                match result {
                    Ok((text, rev)) => {
                        open.editor.set_text(&text);
                        open.rev = rev;
                        open.error = None;
                    }
                    Err(error) => open.error = Some(format!("Cannot read rules.md: {error}")),
                }
                app.needs_draw = true;
            }))
        },
    );
}

/// Keeps an agent's instructions for its settings row.
fn remember_rules(app: &mut App, id: &str, text: &str) {
    app.agent_ui.rules.insert(id.to_owned(), (text.to_owned(), Instant::now()));
    app.agent_ui.rules_loading.remove(id);
}

/// Reads an agent's rules.md for its settings row when it has not been read
/// in the last 10 s (in the background; the drawing never waits).
pub fn want_rules(app: &mut App, id: &str) {
    if app.agent_ui.rules_loading.contains(id) || app.agent_ui.rules.get(id).is_some_and(|(_, at)| at.elapsed() < std::time::Duration::from_secs(10)) {
        return;
    }
    let Some(path) = app.doc.node(id).and_then(|n| app.passive_paths(n)).map(|p| p.rules.display().to_string()) else { return };
    app.agent_ui.rules_loading.insert(id.to_owned());
    let id = id.to_owned();
    app.in_background(
        move |backend| backend.read_text(&path, 64 * 1024).map_err(|e| format!("{e:#}")),
        move |result| {
            AppEvent::Run(Box::new(move |app: &mut App| {
                remember_rules(app, &id, &result.unwrap_or_default());
                app.needs_draw = true;
            }))
        },
    )
}

/// The instructions' first words for the settings row.
pub fn rules_summary(app: &App, id: &str) -> String {
    match app.agent_ui.rules.get(id) {
        Some((text, _)) => {
            let words: Vec<&str> = text.lines().map(|l| l.trim().trim_start_matches('#').trim()).filter(|l| !l.is_empty()).take(3).collect();
            if words.is_empty() { "(empty) · a click edits them".into() } else { words.join(" — ") }
        }
        None => "rules.md · a click edits them".into(),
    }
}

/// Saves the editor's text (never over a change made meanwhile: that asks
/// for a second Save).
fn save_rules(app: &mut App, rules: &mut RulesEditor) {
    if rules.loading || rules.saving {
        return;
    }
    rules.saving = true;
    rules.error = None;
    let (id, path, root, text, base) = (rules.id.clone(), rules.path.clone(), rules.root.clone(), rules.editor.text(), rules.rev.clone());
    app.in_background(
        move |backend| backend.note_write(&path, &text, base.as_deref(), &[root]).map(|saved| (saved, text)).map_err(|e| {
            let text = format!("{e:#}");
            if text.contains("does not support") { "the service is older than this editor: update it, or Open rules.md".to_owned() } else { text }
        }),
        move |result| {
            AppEvent::Run(Box::new(move |app: &mut App| {
                app.needs_draw = true;
                match result {
                    Ok((canvas_model::notebook::Saved::Written(_), text)) => {
                        remember_rules(app, &id, &text);
                        let at = app.modals.iter().rposition(|m| matches!(m, Modal::AgentRules(r) if r.id == id));
                        if let Some(at) = at {
                            app.modals.remove(at);
                        }
                        app.notify("Saved its instructions (rules.md): its next run and chat read them.", false);
                    }
                    Ok((canvas_model::notebook::Saved::Conflict(current), _)) => {
                        if let Some(open) = rules_editor_mut(app, &id) {
                            open.saving = false;
                            open.rev = Some(current.rev);
                            open.error = Some("rules.md changed meanwhile: Save again to replace it with yours, or Reload to take it.".into());
                        }
                    }
                    Err(error) => {
                        if let Some(open) = rules_editor_mut(app, &id) {
                            open.saving = false;
                            open.error = Some(format!("Not saved: {error}"));
                        }
                    }
                }
            }))
        },
    );
}

const RULES_BUTTONS: [&str; 4] = ["Save", "Reload", "Open rules.md", "Cancel"];

fn rules_button(app: &mut App, rules: &mut RulesEditor, index: usize) -> Outcome {
    match index {
        0 => {
            save_rules(app, rules);
            Outcome::Keep
        }
        1 => {
            rules.loading = true;
            rules.error = None;
            let (id, path) = (rules.id.clone(), rules.path.clone());
            load_rules(app, &id, path);
            Outcome::Keep
        }
        2 => Outcome::KeepAndRun(Action::PassiveOpen(rules.id.clone(), 0)),
        _ => Outcome::Close,
    }
}

/// A key in the instructions editor: Ctrl+S saves, Esc closes (asking once
/// when there are unsaved changes), the rest edits.
pub fn rules_key(app: &mut App, rules: &mut RulesEditor, key: KeyEvent) -> Outcome {
    if key.code == KeyCode::Esc {
        if rules.editor.is_dirty() && !rules.discard {
            rules.discard = true;
            rules.error = Some("Unsaved changes: Esc again discards them, Ctrl+S saves.".into());
            return Outcome::Keep;
        }
        return Outcome::Close;
    }
    rules.discard = false;
    if rules.loading {
        return Outcome::Keep;
    }
    match rules.editor.key(&key) {
        EditorOutcome::Save => save_rules(app, rules),
        EditorOutcome::Copy(text) | EditorOutcome::Cut(text) => app.copy_to_clipboard(&text),
        _ => {}
    }
    Outcome::Keep
}

pub fn rules_paste(rules: &mut RulesEditor, text: &str) {
    if !rules.loading {
        rules.editor.insert_text(text);
    }
}

pub fn rules_mouse(app: &mut App, rules: &mut RulesEditor, mouse: MouseEvent, hit: Option<Hit>) -> Outcome {
    let inside = |r: Rect| r.width > 0 && crate::paint::rect_contains(r, mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            rules.editor.scroll(if mouse.kind == MouseEventKind::ScrollUp { -3 } else { 3 });
            Outcome::Keep
        }
        MouseEventKind::Drag(MouseButton::Left) if rules.editor_rect.width > 0 => {
            rules.editor.drag(rules.editor_rect, mouse.column, mouse.row);
            Outcome::Keep
        }
        MouseEventKind::Down(MouseButton::Left) if inside(rules.editor_rect) => {
            rules.editor.click(rules.editor_rect, mouse.column, mouse.row, mouse.modifiers.contains(Modifiers::SHIFT));
            Outcome::Keep
        }
        MouseEventKind::Down(MouseButton::Left) => match hit {
            Some(Hit::Button(i)) => rules_button(app, rules, i),
            _ => Outcome::Keep,
        },
        _ => Outcome::Keep,
    }
}

/// Draws the instructions editor.
pub fn draw_rules(buf: &mut Buffer, screen: Rect, rules: &mut RulesEditor, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    let rect = centered(screen, 100, screen.height.saturating_sub(4).min(32));
    panel(buf, rect, &truncate(&format!("Instructions of “{}”", rules.name), 80));
    let (x, width) = (rect.x as i32 + 2, rect.width as i32 - 4);
    put(buf, x, rect.y as i32 + 1, &crate::paint::truncate_left(&format!("rules.md · {}", rules.path), width.max(0) as usize), fg(MUTED).bg(MODAL), rect);
    let area = Rect::new(x as u16, rect.y + 2, width.max(1) as u16, rect.height.saturating_sub(6)).intersection(rect);
    fill(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, Style::default().bg(PANEL_HI), rect);
    let mut cursor = None;
    if rules.loading {
        put(buf, x + 1, area.y as i32, "Reading rules.md…", fg(MUTED).bg(PANEL_HI), rect);
    } else {
        cursor = rules.editor.draw(buf, area, true);
    }
    rules.editor_rect = area;
    let y = rect.bottom() as i32 - 3;
    let (line, style) = match &rules.error {
        Some(error) => (error.clone(), fg(NEGATIVE)),
        None if rules.saving => ("Saving…".to_owned(), fg(GRAY)),
        None => ("Ctrl+S saves · Esc closes · its next run and chat read the saved text".to_owned(), fg(MUTED)),
    };
    put(buf, x, y, &truncate(&line, width.max(0) as usize), style.bg(MODAL), rect);
    let mut bx = x;
    for (i, label) in RULES_BUTTONS.iter().enumerate() {
        bx = crate::ui_modals::button(buf, ctx, bx, y + 1, label, false, i == 0, i, rect);
    }
    (rect, cursor)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use canvas_model::rpc::{Backend, BackendInfo, Request, Response};

    use super::*;
    use crate::canvas_wide::AgentAction;
    use crate::doc::{Doc, Group};
    use crate::protocol::{ClientMsg, LaunchKind};

    /// A service machine for canvas-wide agents: it makes their files
    /// (recording what it was asked), lists one canvas skill, and keeps one
    /// rules.md that can change behind the editor's back.
    #[derive(Default)]
    pub(crate) struct Agents {
        pub requests: Mutex<Vec<Request>>,
        /// rules.md: (text, revision).
        pub rules: Mutex<(String, String)>,
    }

    impl Backend for Agents {
        fn call(&self, request: Request) -> Result<Response> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(match request {
                Request::Info => Response::Info(BackendInfo { home: r"C:\h".into(), installed: crate::agents::ALL.to_vec(), ..Default::default() }),
                Request::CreateCanvasAgent { files } => {
                    *self.rules.lock().unwrap() = (files.rules.clone(), "r1".into());
                    Response::Text(files.base_slug)
                }
                Request::CanvasSkills { .. } => Response::Skills(vec![canvas_model::canvas_wide::SkillInfo { name: "pr-etiquette".into(), description: "PR feedback".into(), path: r"C:\h\canvas-agents\c1\skills\pr-etiquette\SKILL.md".into(), problem: String::new() }]),
                Request::NoteRead { .. } => {
                    let (text, rev) = self.rules.lock().unwrap().clone();
                    Response::Note(canvas_model::notebook::NoteText { text, rev })
                }
                Request::ReadText { .. } => Response::Text(self.rules.lock().unwrap().0.clone()),
                Request::NoteWrite { text, base, .. } => {
                    let mut rules = self.rules.lock().unwrap();
                    if base.as_deref() != Some(rules.1.as_str()) {
                        Response::NoteConflict(canvas_model::notebook::NoteText { text: rules.0.clone(), rev: rules.1.clone() })
                    } else {
                        let rev = format!("r{}", rules.1[1..].parse::<u32>().unwrap_or(0) + 1);
                        *rules = (text, rev.clone());
                        Response::NoteSaved { rev }
                    }
                }
                _ => Response::Error("not here".into()),
            })
        }
    }

    pub(crate) struct Rig {
        pub app: App,
        pub sent: crossbeam_channel::Receiver<ClientMsg>,
        pub events: crossbeam_channel::Receiver<AppEvent>,
        pub backend: Arc<Agents>,
    }

    /// A canvas with an id, a group "Payments" and the remembered Copilot model.
    pub(crate) fn rig() -> Rig {
        let (tx, sent) = crossbeam_channel::bounded(512);
        let (_keep, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, events) = crossbeam_channel::unbounded();
        let backend = Arc::new(Agents::default());
        let mut doc = Doc { canvas_id: Some("c1".into()), ..Doc::default() };
        doc.groups.push(Group { id: "g1".into(), name: "Payments".into(), x: 0, y: 0, w: 40, h: 10, ..Default::default() });
        doc.defaults.set_model(AgentKind::Copilot, Some("gpt-remembered"));
        let mut app = App::new(crate::conn::Conn::detached(tx, service_rx), backend.clone(), crate::sync::Opened::detached(doc), false, ".".into(), app_tx, None);
        app.detected_agent = AgentKind::Copilot;
        Rig { app, sent, events, backend }
    }

    pub(crate) fn settle(rig: &mut Rig, done: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done(&rig.app) {
            assert!(std::time::Instant::now() < deadline, "background work did not finish");
            if let Ok(event) = rig.events.recv_timeout(std::time::Duration::from_millis(50)) {
                rig.app.on_app_event(event);
            }
        }
    }

    fn press(app: &mut App, code: KeyCode) {
        app.modal_key(KeyEvent::new(code, Modifiers::NONE));
    }

    fn the_dialog(app: &App) -> NewAgent {
        dialog(app).cloned().expect("the new agent dialog")
    }

    fn focus(app: &mut App, field: Field) {
        if let Some(d) = dialog_mut(app) {
            d.focus = field;
        }
    }

    /// The agent launches sent so far: (box id, its launch).
    pub(crate) fn launches(sent: &crossbeam_channel::Receiver<ClientMsg>) -> Vec<(String, crate::protocol::AgentLaunch)> {
        sent.try_iter()
            .filter_map(|m| match m {
                ClientMsg::Launch { id, kind: LaunchKind::Agent(launch), .. } => Some((id, *launch)),
                _ => None,
            })
            .collect()
    }

    fn created(r: &Rig) -> Option<canvas_model::rpc::CanvasAgentFiles> {
        r.backend.requests.lock().unwrap().iter().rev().find_map(|q| if let Request::CreateCanvasAgent { files } = q { Some(files.clone()) } else { None })
    }

    #[test]
    fn the_dialog_makes_an_agent_from_its_fields_with_no_triggers_every_group_and_the_remembered_model() {
        let mut r = rig();
        r.app.run(Action::Agents(AgentAction::New));
        let d = the_dialog(&r.app);
        assert_eq!((d.agent, d.focus, d.wakes.is_empty(), d.groups.is_empty(), d.template), (AgentKind::Copilot, Field::Name, true, true, None));
        let shown = r.app.new_agent_model(&d);
        assert_eq!((shown.model.as_deref(), shown.note.as_str()), (Some("gpt-remembered"), "remembered default for GitHub Copilot"));
        r.app.modal_paste("Release watch");
        focus(&mut r.app, Field::Instructions);
        r.app.modal_paste("Watch the release");
        // Enter in the instructions is a new line, never Create.
        press(&mut r.app, KeyCode::Enter);
        r.app.modal_paste("and tell me when ring 2 fails");
        assert!(dialog(&r.app).is_some(), "still open");
        focus(&mut r.app, Field::Create);
        press(&mut r.app, KeyCode::Enter);
        assert!(r.app.modals.is_empty(), "Create closes it: {:?}", the_error(&r.app));
        let files = created(&r).expect("its files were asked for");
        assert_eq!((files.name.as_str(), files.rules.as_str(), files.canvas.as_str()), ("Release watch", "Watch the release\nand tell me when ring 2 fails\n", "c1"));
        let node = r.app.doc.nodes.iter().find(|n| n.name == "Release watch").cloned().expect("its box");
        let passive = node.passive.clone().unwrap();
        assert!(node.is_canvas_agent() && node.fresh && !passive.has_triggers() && passive.groups.is_empty() && passive.model.is_none() && passive.template.is_empty());
        assert_eq!((node.agent.as_deref(), node.cwd.as_deref()), (Some("copilot"), Some(r"C:\h\canvas-agents\c1\agents\release-watch")));
        assert_eq!(r.app.right_detail, Some(crate::side::RightDetail::Passive(node.id.clone())), "its settings show");
        let spec = r.app.build_routing().passive.into_iter().find(|p| p.session_id == node.id).expect("routed");
        assert_eq!((spec.model.as_str(), spec.subscribe.len(), spec.every_minutes, spec.daily_at.as_str()), ("gpt-remembered", 0, 0, ""));
        assert!(launches(&r.sent).is_empty(), "Create starts nothing");
        assert_eq!(r.app.canvas_agent_state(&node), crate::canvas_wide::AgentState::Waiting);
        // A name is required; Escape closes without making anything.
        r.app.run(Action::Agents(AgentAction::New));
        focus(&mut r.app, Field::Create);
        press(&mut r.app, KeyCode::Enter);
        assert_eq!(the_dialog(&r.app).error.as_deref(), Some("Give it a name."));
        press(&mut r.app, KeyCode::Esc);
        assert!(r.app.modals.is_empty() && r.app.canvas_agents().len() == 1);
    }

    fn the_error(app: &App) -> Option<String> {
        dialog(app).and_then(|d| d.error.clone())
    }

    #[test]
    fn start_from_prefills_the_fields_and_keeps_what_you_changed() {
        let mut r = rig();
        r.app.run(Action::Agents(AgentAction::New));
        r.app.run(Action::Agents(AgentAction::Pick(PickPurpose::AgentTemplate)));
        let Some(Modal::Pick(pick)) = r.app.modals.last().cloned() else { panic!("the templates") };
        assert_eq!(pick.items.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), [BLANK, "PR coordinator", "Review follow-up", "Cross-group digest"]);
        r.app.modals.pop();
        r.app.apply_agent_pick(PickPurpose::AgentTemplate, "PR coordinator");
        let d = the_dialog(&r.app);
        assert_eq!((d.name.text.as_str(), d.wakes.events.clone(), d.prs.is_some(), d.max_runs_per_day), ("PR coordinator", vec!["pr.*".to_owned(), "build.failed".to_owned()], true, 24));
        assert!(d.instructions.text().starts_with("# PR coordinator"));
        // Your own name stays; untouched instructions follow the template.
        dialog_mut(&mut r.app).unwrap().name.set("My reviews");
        r.app.apply_agent_pick(PickPurpose::AgentTemplate, "Review follow-up");
        let d = the_dialog(&r.app);
        assert_eq!((d.name.text.as_str(), d.wakes.events.clone(), d.prs.is_none(), d.reviews.is_some(), d.max_runs_per_day), ("My reviews", vec!["review.*".to_owned()], true, true, 60));
        assert!(d.instructions.text().starts_with("# Review follow-up"));
        // Instructions you edited stay too; Blank clears the triggers and settings only.
        focus(&mut r.app, Field::Instructions);
        r.app.modal_paste("Mine. ");
        r.app.apply_agent_pick(PickPurpose::AgentTemplate, BLANK);
        let d = the_dialog(&r.app);
        assert!(d.instructions.text().starts_with("Mine. # Review follow-up"), "{}", d.instructions.text());
        assert!(d.wakes.is_empty() && d.reviews.is_none() && d.template.is_none());
        r.app.apply_agent_pick(PickPurpose::AgentTemplate, "Cross-group digest");
        assert_eq!(the_dialog(&r.app).wakes, Wakes { events: vec!["session.done".into()], every_minutes: 0, daily_at: "17:00".into() });
        // What it makes carries the template's name and settings.
        let spec = the_dialog(&r.app).spec();
        assert_eq!((spec.name.as_str(), spec.template.as_str(), spec.wakes.daily_at.as_str()), ("My reviews", "Cross-group digest", "17:00"));
    }

    #[test]
    fn create_and_start_opens_its_chat_with_who_it_is_and_the_first_message() {
        let mut r = rig();
        r.app.run(Action::Agents(AgentAction::New));
        r.app.modal_paste("Digest");
        focus(&mut r.app, Field::Message);
        r.app.modal_paste("What happened today?");
        // Enter on a field creates and starts it (as in the new-session dialog).
        press(&mut r.app, KeyCode::Enter);
        assert!(r.app.modals.is_empty(), "{:?}", the_error(&r.app));
        let id = r.app.canvas_agents()[0].id.clone();
        let launched = launches(&r.sent);
        assert_eq!(launched.len(), 1, "the explicit start launches its chat");
        let (at, launch) = &launched[0];
        let prompt = launch.prompt.clone().unwrap_or_default();
        assert_eq!(at, &id);
        assert!(prompt.starts_with("[Atelier · canvas-wide agent \"Digest\"]") && prompt.contains(r"C:\h\canvas-agents\c1\agents\digest\rules.md") && prompt.contains("only when the user talks to you"), "{prompt}");
        assert!(prompt.ends_with("What happened today?"), "{prompt}");
        assert!(!launch.resume && launch.headless_prompt.is_none() && launch.model.as_deref() == Some("gpt-remembered"));
        assert!(launch.add_dirs.iter().any(|d| d == r"C:\h\canvas-agents\c1"), "the canvas's goals and skills: {:?}", launch.add_dirs);
        let node = r.app.doc.node(&id).unwrap();
        assert!(!node.fresh && node.model.as_deref() == Some("gpt-remembered"), "its conversation is begun and records its model");
        assert_eq!(r.app.focused_term.as_deref(), Some(id.as_str()), "its chat tile has the keyboard");
    }

    #[test]
    fn settings_edits_reach_the_document_the_routing_and_its_rules_file() {
        let mut r = rig();
        let mut spec = AgentSpec::named("Watcher", AgentKind::Copilot);
        spec.rules = "# Watch\nOld rules".into();
        let id = r.app.create_canvas_agent(spec).unwrap();
        let target = AgentTarget::Node(id.clone());
        let passive = |app: &App, id: &str| app.doc.node(id).and_then(|n| n.passive.clone()).unwrap();
        let routed = |app: &App, id: &str| app.build_routing().passive.into_iter().find(|p| p.session_id == id).unwrap();
        // Wakes on: an event ticked, an interval, a daily time; then only when you talk to it again.
        r.app.apply_agent_pick(PickPurpose::AgentWakes(target.clone()), "○ pr.*");
        assert!(matches!(r.app.modals.last(), Some(Modal::Pick(p)) if p.items.iter().any(|(v, _)| v == "✓ pr.*")), "the picker shows it ticked");
        r.app.modals.clear();
        r.app.apply_agent_pick(PickPurpose::AgentWakes(target.clone()), "Every … off");
        let Some(Modal::Prompt(prompt)) = r.app.modals.pop() else { panic!("the interval prompt") };
        assert!(r.app.apply_prompt(prompt.purpose.clone(), "soon".into()).is_err());
        r.app.apply_prompt(prompt.purpose, "30m".into()).unwrap();
        r.app.apply_prompt(PromptPurpose::AgentDaily(target.clone()), "9:05".into()).unwrap();
        r.app.apply_prompt(PromptPurpose::AgentEvents(target.clone()), "deploy.done, rollout.*".into()).unwrap();
        r.app.modals.clear();
        let spec = routed(&r.app, &id);
        assert_eq!((spec.subscribe, spec.every_minutes, spec.daily_at.as_str()), (vec!["pr.*".to_owned(), "deploy.done".to_owned(), "rollout.*".to_owned()], 30, "09:05"));
        assert_eq!(Wakes::of(&passive(&r.app, &id)).text(), "pr.*, deploy.done, rollout.* · every 30m · daily 09:05");
        r.app.apply_agent_pick(PickPurpose::AgentWakes(target.clone()), &format!("○ {ONLY_TALK}"));
        r.app.modals.clear();
        assert!(!passive(&r.app, &id).has_triggers());
        // Watches: one group, then every group again.
        r.app.apply_agent_pick(PickPurpose::AgentGroups(target.clone()), "○ Payments");
        assert_eq!(routed(&r.app, &id).groups, ["g1"]);
        r.app.apply_agent_pick(PickPurpose::AgentGroups(target.clone()), &format!("○ {ALL_GROUPS}"));
        r.app.modals.clear();
        assert!(routed(&r.app, &id).groups.is_empty());
        // Skills from the library.
        r.app.apply_agent_pick(PickPurpose::AgentSkills(target.clone()), "○ pr-etiquette");
        r.app.modals.clear();
        assert_eq!(routed(&r.app, &id).skills, ["pr-etiquette"]);
        // Its own model: its runs and its current conversation.
        r.app.apply_model_choice(crate::dialogs::ModelTarget::Agent(id.clone()), "gpt-6-sol").unwrap();
        let spec = routed(&r.app, &id);
        assert_eq!((spec.model.as_str(), spec.session_model.as_deref(), passive(&r.app, &id).model.as_deref()), ("gpt-6-sol", Some("gpt-6-sol"), Some("gpt-6-sol")));
        assert!(r.app.agent_model_text(r.app.doc.node(&id).unwrap()).starts_with("gpt-6-sol · its own"));
        // Permissions apply to PR follow-ups: refused without PR coordination, kept with it.
        assert!(r.app.apply_passive_setting(&id, crate::dialogs::PassiveField::Permissions, "push").is_err());
        r.app.apply_passive_setting(&id, crate::dialogs::PassiveField::Prs, "on every 5m").unwrap();
        r.app.apply_passive_setting(&id, crate::dialogs::PassiveField::Permissions, "push, reply").unwrap();
        assert_eq!(routed(&r.app, &id).prs.map(|p| p.allow), Some(vec!["push".to_owned(), "reply".to_owned()]));
        assert!(r.app.apply_passive_setting(&id, crate::dialogs::PassiveField::Permissions, "delete").is_err());
        // Its instructions: read through the backend, edited, written with the revision read.
        r.app.run(Action::Agents(AgentAction::Instructions(id.clone())));
        settle(&mut r, |app| rules_editor(app).is_some_and(|e| !e.loading));
        assert_eq!(rules_editor(&r.app).unwrap().editor.text(), "# Watch\nOld rules\n");
        r.app.modal_key(KeyEvent::new(KeyCode::End, Modifiers::CONTROL));
        r.app.modal_paste("New line.");
        r.app.modal_key(KeyEvent::new(KeyCode::Char('s'), Modifiers::CONTROL));
        settle(&mut r, |app| rules_editor(app).is_none());
        assert_eq!(r.backend.rules.lock().unwrap().0, "# Watch\nOld rules\nNew line.");
        let wrote = r.backend.requests.lock().unwrap().iter().rev().find_map(|q| if let Request::NoteWrite { base, roots, path, .. } = q { Some((base.clone(), roots.clone(), path.clone())) } else { None }).unwrap();
        assert_eq!(wrote, (Some("r1".to_owned()), vec![r"C:\h\canvas-agents\c1".to_owned()], r"C:\h\canvas-agents\c1\agents\watcher\rules.md".to_owned()));
        assert_eq!(rules_summary(&r.app, &id), "Watch — Old rules — New line.");
        // Changed behind its back: the save asks for a second one.
        r.app.run(Action::Agents(AgentAction::Instructions(id.clone())));
        settle(&mut r, |app| rules_editor(app).is_some_and(|e| !e.loading));
        *r.backend.rules.lock().unwrap() = ("Someone else's".into(), "r9".into());
        r.app.modal_paste("x");
        r.app.modal_key(KeyEvent::new(KeyCode::Char('s'), Modifiers::CONTROL));
        settle(&mut r, |app| rules_editor(app).is_some_and(|e| !e.saving));
        assert!(rules_editor(&r.app).unwrap().error.as_deref().unwrap_or_default().contains("changed meanwhile"));
        assert_eq!(r.backend.rules.lock().unwrap().0, "Someone else's", "nothing written over it");
        r.app.modal_key(KeyEvent::new(KeyCode::Char('s'), Modifiers::CONTROL));
        settle(&mut r, |app| rules_editor(app).is_none());
        assert!(r.backend.rules.lock().unwrap().0.starts_with('x'), "the second Save is yours");
        // Another agent CLI: a fresh conversation in a new box id, its files kept.
        let key = passive(&r.app, &id).key;
        r.app.apply_agent_pick(PickPurpose::AgentCli(id.clone()), "○ Claude Code");
        let node = r.app.canvas_agents()[0].clone();
        let p = node.passive.clone().unwrap();
        assert!(node.id != id && node.fresh && node.agent.as_deref() == Some("claude") && p.episodes == [id.clone()] && p.key != key && p.model.is_none() && p.slug == "watcher");
    }

    #[test]
    fn chat_and_messages_never_make_a_second_writer_beside_a_run() {
        let mut r = rig();
        let id = r.app.create_canvas_agent(AgentSpec::named("Helper", AgentKind::Copilot)).unwrap();
        r.app.doc.node_mut(&id).unwrap().fresh = false;
        let key = r.app.doc.node(&id).and_then(|n| n.passive.as_ref()).unwrap().key.clone();
        let _ = r.sent.try_iter().count();
        // A headless run answers: its terminal is the agent CLI's, the chat waits.
        r.app.bus.passive.insert(key.clone(), crate::bus::PassiveView { running: true, session_id: id.clone(), ..Default::default() });
        r.app.terms.insert(id.clone(), crate::protocol::TermInfo { id: id.clone(), alive: true, ..Default::default() });
        r.app.start_node(&id);
        assert!(launches(&r.sent).is_empty() && r.app.focus != crate::app::Focus::Terminal, "neither launched nor typed into");
        assert!(r.app.agent_ui.chat_after_run.contains(&id) && r.app.notice.as_ref().is_some_and(|n| n.text.contains("its chat opens when the run ends")));
        assert!(matches!(r.app.start_plan(r.app.doc.node(&id).unwrap()), crate::interact::StartPlan::Blocked(_)));
        // The run ends, but its terminal is still there: still waiting.
        r.app.bus.passive.insert(key.clone(), crate::bus::PassiveView { running: false, session_id: id.clone(), last_run_ms: 1, ..Default::default() });
        r.app.open_waiting_chats();
        assert!(launches(&r.sent).is_empty());
        // Its terminal gone: the chat opens and resumes the run's conversation.
        r.app.terms.insert(id.clone(), crate::protocol::TermInfo { id: id.clone(), alive: false, exit_code: Some(0), ..Default::default() });
        r.app.on_tick();
        let launched = launches(&r.sent);
        assert_eq!(launched.len(), 1);
        assert!(launched[0].1.resume && launched[0].1.prompt.is_none(), "the same conversation, nothing typed for you");
        assert!(r.app.agent_ui.chat_after_run.is_empty());
        // Its chat open: Chat focuses it; a message goes to the service, which types it in when idle.
        r.app.terms.insert(id.clone(), crate::protocol::TermInfo { id: id.clone(), alive: true, ..Default::default() });
        r.app.run(Action::Agents(AgentAction::Chat(id.clone())));
        assert!(launches(&r.sent).is_empty() && r.app.focused_term.as_deref() == Some(id.as_str()));
        r.app.message_passive(&id, "status?").unwrap();
        assert!(r.app.notice.as_ref().is_some_and(|n| n.text.contains("typed into its chat when the chat is idle")));
        let emitted: Vec<String> = r.sent.try_iter().filter_map(|m| if let ClientMsg::Bus { json } = m { Some(json) } else { None }).collect();
        assert!(emitted.iter().any(|j| j.contains("\"message\"") && j.contains("status?") && j.contains(&id)), "{emitted:?}");
        // No chat: a run answers; disabled: refused (it is not routed).
        r.app.terms.remove(&id);
        r.app.message_passive(&id, "and now?").unwrap();
        assert!(r.app.notice.as_ref().is_some_and(|n| n.text.contains("a run answers it")));
        r.app.run(Action::Agents(AgentAction::Toggle(id.clone())));
        assert!(r.app.message_passive(&id, "hello").unwrap_err().contains("disabled"));
    }

    #[test]
    fn an_agent_with_no_triggers_never_runs_on_its_own() {
        let mut r = rig();
        let id = r.app.create_canvas_agent(AgentSpec::named("Quiet", AgentKind::Copilot)).unwrap();
        r.app.doc.nodes.push(serde_json::from_value(serde_json::json!({ "id": "s1", "kind": "session", "name": "Worker", "x": 2, "y": 2 })).unwrap());
        let routing = r.app.build_routing();
        let spec = routing.passive.iter().find(|p| p.session_id == id).cloned().unwrap();
        // Events of every kind from a session of the canvas: none reaches it.
        for kind in ["pr.published", "pr.comment.new", "session.done", "build.failed"] {
            let event = crate::bus::Event { kind: kind.into(), source: "s1".into(), group: "g1".into(), canvas: "c1".into(), ..Default::default() };
            assert!(routing.subscribers(&event).is_empty(), "{kind} woke it");
        }
        // No schedule, whatever the time.
        let mut state = crate::bus::PassiveState::default();
        for hour in 0..24 {
            let now = 1_000_000 + hour as u64 * 3_600_000;
            let local = crate::bus::Local { year: 2026, month: 11, day: 2, hour, minute: 30 };
            assert!(state.due_timers(&spec, now, local).is_empty());
        }
        assert!(!state.ready(&spec, 10_000_000), "nothing to run");
        // Talking to it does: a message from you (or a session) reaches it.
        assert_eq!(routing.resolve_from("Quiet", "", ""), Ok(crate::bus::Target::Passive(spec.key.clone())));
        assert_eq!(routing.resolve_from("Quiet", "s1", "g1"), Ok(crate::bus::Target::Passive(spec.key)));
    }
}
