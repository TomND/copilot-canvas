//! Canvas-wide agents on the canvas: passive sessions that watch every group
//! of this canvas (not one group), with skills from the canvas's library.
//!
//! - **Where they are**: boxes (dashed, marked ◎) in the *Canvas agents*
//!   frame, a group with source [`crate::doc::CANVAS_AGENTS`]; the top bar's
//!   **◎ Agents** entry and the frame's chips open the management panel
//!   ([`crate::side::RightDetail::CanvasAgents`]); the sidebar lists them under
//!   the frame. Every entry point shows the same box (`Passive::scope` =
//!   canvas); none of them starts it: *Open chat* is the explicit start.
//! - **What they do**: the service routes them every event of this canvas
//!   (`Routing::canvas`), keeps their schedules and runs them like group
//!   passive sessions, with their files in `<home>/canvas-agents/<canvas id>`
//!   ([`canvas_model::canvas_wide`]).
//! - **State** ([`AgentState`]): off, unavailable, working, paused, needs
//!   attention, failing (retrying), done, queued, monitoring, waiting.

use ratatui::style::Color;

use crate::app::App;
use crate::bus::PassiveView;
use canvas_model::canvas_wide as wide;
use crate::dialogs::{Action, MenuItem, Modal, PassiveField, Pick, PickPurpose, Prompt, PromptPurpose, TextInput};
use crate::doc::{GRID_X, GRID_Y, Group, Node, NodeKind, Passive, PassiveScope};
use crate::paint::{ACCENT, ATTENTION, DARK, FINISHED, GRAY, NEGATIVE, POSITIVE, SPINNER};
use crate::passive::Template;
use crate::side::RightDetail;

/// The marker of canvas-wide agents (boxes, sidebar, top bar, frame).
pub const MARK: &str = "◎";
/// The frame's name when it is created.
pub const FRAME_NAME: &str = "Canvas agents";
/// The frame's colour (an index of `GROUP_COLORS`).
pub const FRAME_COLOR: u8 = 3;
/// The colour of the ◎ marker.
pub const COLOR: Color = crate::paint::GROUP_COLORS[FRAME_COLOR as usize];

/// Templates of a new canvas-wide agent. PR management is one example, not
/// a built-in workflow: its rules are plain text you edit.
pub const TEMPLATES: [Template; 4] = [
    Template {
        name: "PR coordinator",
        about: "pull requests from every group: tracks each one and its owning session, routes review comments and build failures to it",
        subscribe: &["pr.*", "build.failed"],
        every_minutes: 0,
        daily_at: "",
        rules: "# PR coordinator\n\n\
Job: follow the pull requests that sessions anywhere on this canvas publish, and make sure each owning session hears about what needs it. \
The canvas tracks them for you (sessions hand them off with `copilot-canvas pr track`; detected `pr.created` is tracked too), checks each one read-only with gh / az, \
and sends you their events (the canvas-prs skill has the details).\n\n\
On pr.tracked: note the pull request (link, owning session id and name, group) in your memory.\n\
On pr.comment.new or pr.checks.failed: send the owning session one short, actionable follow-up with the event's `dispatch_key`: \
`copilot-canvas pr dispatch <url> --key <dispatch_key> --event <type> \"<what to do, with the links that matter>\"`. \
The canvas types it in when that session is idle and reports queued, delivered or blocked in Reports; a repeat of a key is never sent twice. \
You may mark its box: `copilot-canvas effect attention --on \"<owner session id>\" --note \"PR <number>: <in a few words>\"`.\n\
On pr.changes_requested: the same, saying in your own words what the reviewer asks.\n\
On pr.approved: `copilot-canvas effect fireworks --on \"<owner session id>\" --note \"PR <number> approved\"`. On pr.merged or pr.closed: mark it ended in your memory; send nothing.\n\n\
Report: `ok` with the number of active pull requests; `warn` when a follow-up is blocked or a pull request waits on its owner for long; `alert` when something needs Thomas.\n\
Read-only towards pull requests: never comment, reply, resolve threads, vote, push or merge yourself; follow-ups ask the owner only for what the canvas's PR permissions allow (the `[Canvas: ...]` line of each says which).\n",
    },
    Template {
        name: "Review follow-up",
        about: "after your reviews: messages each pull request's author the results (you approve first), then checks their replies hourly for re-review requests",
        subscribe: &["review.*"],
        every_minutes: 0,
        daily_at: "",
        rules: "# Review follow-up\n\n\
Job: after Thomas's review sessions finish reviewing someone's pull request, tell its author what the review found, then watch that conversation for requests for another review. \
The canvas keeps the records and the schedule; you write and send the messages and read the replies through the messaging channel configured for you (Follow-ups…: channel, tool, recipient rule, approval). \
The canvas-review-followup skill has the commands.\n\n\
On review.completed: draft a concise message to the author (the pull request link, the outcome exactly as the review states it, the actionable findings with their links; nothing the results do not say) and record it with `copilot-canvas review message <review> --draft <file>`. With approval on (the default), stop there.\n\
On review.message.approved: resolve the recipient and conversation with your channel and the recipient rule (none, several or no access: `copilot-canvas review message <review> --needs-you \"<why>\"`, never guess), send the approved text unchanged, then `copilot-canvas review message <review> --sent --conversation <id> --recipient \"<name>\"`.\n\
On review.check_due: read only the listed conversations, only the author's messages after `since`. Record each request for another review, a re-review after fixes or review follow-up with `copilot-canvas review request …`; ignore everything else; then `copilot-canvas review checked --conversation <id> --mark <last message id>` (or `--failed \"<why>\"`).\n\n\
Report: `ok` with what you sent or recorded; `alert` when you need Thomas (no access, an ambiguous recipient).\n\
Never send what Thomas has not approved, never message anyone but the review's author, never claim an approval or a fix the review did not establish, and never start, resume or do a review yourself.\n",
    },
    Template {
        name: "Cross-group digest",
        about: "once a day: a short digest of what every group did, and what waits on you",
        subscribe: &["session.done"],
        every_minutes: 0,
        daily_at: "17:00",
        rules: "# Cross-group digest\n\n\
Job: once a day, tell Thomas in a few lines what happened across all groups of this canvas.\n\n\
On timer.daily: read the canvas digest (every group's sessions, their latest work) and your memory of the day's session.done events. Write a digest (per group: what moved, what is blocked, what waits on Thomas) at the top of your memory file, and keep the last 7 days there.\n\
On session.done (from any group): note the session, its group and its summary in your memory for today's digest.\n\n\
Report: `ok` with the day's headline (\"3 groups moved, 1 waits on you\"); `alert` only when something has waited on Thomas for more than a day.\n\
Never message sessions; this agent only reports.\n",
    },
    Template {
        name: "Custom",
        about: "your own canvas-wide rules, events and schedule",
        subscribe: &[],
        every_minutes: 0,
        daily_at: "",
        rules: "# Rules\n\n\
Describe the canvas-wide job: which events from which groups matter (see `copilot-canvas events`; `pr.*` from any group), what to check, \
what counts as ok, warn or alert, which sessions to message (`copilot-canvas send --to \"<name or id>\"`), and what never to do.\n",
    },
];

/// What the canvas-agents controls ask for.
#[derive(Clone, Debug, PartialEq)]
pub enum AgentAction {
    /// The management panel (top bar, frame chip, menus).
    Panel,
    /// A new canvas-wide agent: a template, then its name.
    New,
    /// An agent's panel, its box selected and in view (never starts it).
    Show(String),
    /// Enable or disable an agent.
    Toggle(String),
    /// Stop the run in progress.
    Stop(String),
    /// Which canvas skills an agent gets.
    Skills(String),
    /// The canvas's skill library.
    Library,
    /// A new skill in the library (and assigned to this agent).
    NewSkill(Option<String>),
    /// A skill's SKILL.md (the editor locally; VS Code or the viewer remotely).
    OpenSkill(String),
    /// The canvas-agents folder.
    Folder,
    /// An event for the canvas-wide agents (`--to canvas`).
    SendEvent,
    /// The frame in view.
    Reveal,
}

/// Where a canvas-wide agent stands, for its box, the sidebar, the panels and the top bar.
#[derive(Clone, Debug, PartialEq)]
pub enum AgentState {
    /// Switched off (not routed).
    Off,
    /// It cannot run here: why.
    Unavailable(String),
    /// A run is in progress (events in it).
    Working(u32),
    /// Paused: no triggers until you resume it.
    Paused,
    /// It reported `alert`: its headline.
    Attention(String),
    /// Its last run failed; it tries again (the error).
    Failing(String),
    /// Its job is complete (`done`): schedules stop.
    Done(String),
    /// Events wait for its next run; `true`: today's budget is used.
    Queued(u32, bool),
    /// Enabled and watching its events and schedule.
    Monitoring,
    /// No events or schedule: it runs on messages and Run now only.
    Waiting,
}

impl AgentState {
    pub fn label(&self) -> String {
        match self {
            AgentState::Off => "off".into(),
            AgentState::Unavailable(why) => format!("unavailable: {why}"),
            AgentState::Working(n) => format!("working · {n} event(s)"),
            AgentState::Paused => "paused".into(),
            AgentState::Attention(headline) => format!("needs attention: {headline}"),
            AgentState::Failing(error) => format!("failed, retrying: {error}"),
            AgentState::Done(headline) if headline.is_empty() => "done".into(),
            AgentState::Done(headline) => format!("done: {headline}"),
            AgentState::Queued(n, true) => format!("{n} event(s) wait: today's run budget is used"),
            AgentState::Queued(n, false) => format!("queued · {n} event(s)"),
            AgentState::Monitoring => "monitoring".into(),
            AgentState::Waiting => "waiting for messages".into(),
        }
    }

    /// One word for lists.
    pub fn word(&self) -> &'static str {
        match self {
            AgentState::Off => "off",
            AgentState::Unavailable(_) => "unavailable",
            AgentState::Working(_) => "working",
            AgentState::Paused => "paused",
            AgentState::Attention(_) => "needs attention",
            AgentState::Failing(_) => "failing",
            AgentState::Done(_) => "done",
            AgentState::Queued(..) => "queued",
            AgentState::Monitoring => "monitoring",
            AgentState::Waiting => "waiting",
        }
    }

    pub fn icon(&self, tick: usize) -> &'static str {
        match self {
            AgentState::Off => "○",
            AgentState::Unavailable(_) | AgentState::Attention(_) => "⚠",
            AgentState::Working(_) => SPINNER[tick % SPINNER.len()],
            AgentState::Paused => "‖",
            AgentState::Failing(_) => "✗",
            AgentState::Done(_) => "✓",
            AgentState::Queued(..) => "◌",
            AgentState::Monitoring => "●",
            AgentState::Waiting => "○",
        }
    }

    pub fn color(&self) -> Color {
        match self {
            AgentState::Off => DARK,
            AgentState::Unavailable(_) | AgentState::Failing(_) => NEGATIVE,
            AgentState::Working(_) => ACCENT,
            AgentState::Paused | AgentState::Waiting => GRAY,
            AgentState::Attention(_) | AgentState::Queued(_, true) => ATTENTION,
            AgentState::Done(_) => FINISHED,
            AgentState::Queued(_, false) => ACCENT,
            AgentState::Monitoring => POSITIVE,
        }
    }

    /// Wants your eyes (the top bar's entry turns gold).
    pub fn needs_attention(&self) -> bool {
        matches!(self, AgentState::Unavailable(_) | AgentState::Attention(_) | AgentState::Failing(_) | AgentState::Queued(_, true))
    }
}

/// The state of a canvas-wide agent from its settings, what the service
/// reports (`view`), whether its open chat is working, and why it cannot run.
pub fn agent_state(passive: &Passive, view: Option<&PassiveView>, chat_working: bool, unavailable: Option<String>) -> AgentState {
    let view = view.cloned().unwrap_or_default();
    if passive.disabled {
        return AgentState::Off;
    }
    if let Some(why) = unavailable {
        return AgentState::Unavailable(why);
    }
    if view.running || chat_working {
        return AgentState::Working(view.pending.max(1));
    }
    if passive.paused {
        return AgentState::Paused;
    }
    if view.status == "alert" {
        return AgentState::Attention(view.headline);
    }
    if view.failures > 0 && !view.last_error.is_empty() {
        return AgentState::Failing(view.last_error);
    }
    if view.status == "done" {
        return AgentState::Done(view.headline);
    }
    if view.pending > 0 {
        let used = passive.max_runs_per_day > 0 && view.runs_today >= passive.max_runs_per_day;
        return AgentState::Queued(view.pending, used);
    }
    if passive.subscribe.is_empty() && passive.every_minutes == 0 && passive.daily_at.is_empty() {
        return AgentState::Waiting;
    }
    AgentState::Monitoring
}

fn slugify(name: &str) -> String {
    let slug: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let slug = slug.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    if slug.is_empty() { "agent".into() } else { slug.chars().take(40).collect() }
}

impl App {
    /// The canvas-wide agents' boxes, in the order they were added.
    pub fn canvas_agents(&self) -> Vec<&Node> {
        self.doc.nodes.iter().filter(|n| n.is_canvas_agent()).collect()
    }

    /// The canvas's canvas-agents folder on the service's machine.
    pub fn canvas_agents_dir(&self) -> Option<std::path::PathBuf> {
        let id = self.doc.canvas_id.as_deref().filter(|id| crate::doc::valid_canvas_id(id))?;
        (!self.info.home.is_empty()).then(|| wide::canvas_dir(std::path::Path::new(&self.info.home), id))
    }

    /// Why this canvas cannot have canvas-wide agents here (`None`: it can).
    pub fn canvas_agents_unavailable(&self) -> Option<String> {
        if self.demo {
            return Some("not in the demo (no agents run there)".into());
        }
        if self.doc.canvas_id.is_none() {
            return Some("the service is older than canvas-wide agents: update it (⟳ Update service on its machine) and open the canvas again".into());
        }
        if self.info.home.is_empty() {
            return Some("the service did not say where its data folder is".into());
        }
        None
    }

    /// Why this agent cannot run (`None`: it can).
    pub fn agent_unavailable(&self, node: &Node) -> Option<String> {
        if let Some(why) = self.canvas_agents_unavailable() {
            return Some(why);
        }
        let kind = crate::agents::agent_of(node);
        (!self.installed_agents.is_empty() && !self.installed_agents.contains(&kind)).then(|| format!("{} is not installed on the service's machine", kind.label()))
    }

    /// A canvas-wide agent's state.
    pub fn canvas_agent_state(&self, node: &Node) -> AgentState {
        let Some(passive) = node.passive.as_ref() else { return AgentState::Off };
        let chat = self.owned_alive(&node.id) && crate::status::session_status(self.summary(node), self.terms.get(&node.id), node.reviewed_ms) == crate::status::Status::Working;
        agent_state(passive, self.passive_view(node), chat, self.agent_unavailable(node))
    }

    /// The box status of a canvas-wide agent (counts, borders): working,
    /// needs you (it reported alert), failed (failing or unavailable),
    /// stopped (off or paused), done · review, else idle.
    pub fn canvas_agent_status(&self, node: &Node) -> crate::status::Status {
        use crate::status::Status;
        match self.canvas_agent_state(node) {
            AgentState::Working(_) => Status::Working,
            AgentState::Attention(_) => Status::Waiting,
            AgentState::Failing(_) | AgentState::Unavailable(_) => Status::Failed,
            AgentState::Off | AgentState::Paused => Status::Stopped,
            AgentState::Done(_) if self.passive_view(node).is_some_and(|v| node.reviewed_ms.is_some_and(|r| v.status_ms > r)) => Status::JustFinished,
            _ => Status::Idle,
        }
    }

    /// The frame of the canvas-wide agents, made when missing (above the
    /// canvas's content, the same id in every window making it at once).
    pub fn ensure_canvas_frame(&mut self) -> String {
        if let Some(frame) = self.doc.canvas_frame() {
            return frame.id.clone();
        }
        let (w, h) = (GRID_X + 4, GRID_Y + 3);
        let (x, y) = match self.doc.bounds() {
            Some(b) => (b.x, b.y - h - 3),
            None => (self.doc.view.x + 2, self.doc.view.y + 1),
        };
        let id = self.shared_id("g-canvas-agents", self.doc.canvas_id.as_deref().unwrap_or("canvas"));
        self.doc.groups.push(Group {
            id: id.clone(),
            name: FRAME_NAME.into(),
            x,
            y,
            w,
            h,
            color: FRAME_COLOR,
            folder: None,
            source: Some(crate::doc::CANVAS_AGENTS.into()),
            defaults: None,
        });
        // Agents whose frame was deleted belong to the new one.
        let frames: Vec<String> = self.doc.groups.iter().map(|g| g.id.clone()).collect();
        for node in self.doc.nodes.iter_mut().filter(|n| n.is_canvas_agent()) {
            if let Some(p) = node.passive.as_mut().filter(|p| !frames.contains(&p.group)) {
                p.group = id.clone();
            }
        }
        self.mark_dirty();
        id
    }

    /// A new canvas-wide agent from a template: its files on the service's
    /// machine, its box in the frame, its panel open (nothing starts).
    pub fn create_canvas_agent(&mut self, template: usize, name: &str) -> Result<String, String> {
        if let Some(why) = self.canvas_agents_unavailable() {
            return Err(format!("Canvas-wide agents are unavailable: {why}"));
        }
        let canvas = self.doc.canvas_id.clone().unwrap_or_default();
        let template = TEMPLATES.get(template).ok_or("unknown template")?;
        let name = if name.trim().is_empty() { template.name.to_owned() } else { name.trim().chars().take(80).collect() };
        let taken = self.canvas_agents().iter().filter_map(|n| n.passive.as_ref().map(|p| p.slug.clone())).collect();
        let rules = if template.name == "Custom" { template.rules.replacen("# Rules", &format!("# Rules for {name}"), 1) } else { template.rules.to_owned() };
        let canvas_name = self.replica_name();
        let files = crate::rpc::CanvasAgentFiles { canvas: canvas.clone(), canvas_name, base_slug: slugify(&name), taken, name: name.clone(), rules };
        let slug = self.backend.create_canvas_agent(files).map_err(|e| format!("{e:#}"))?;
        let dir = self.canvas_agents_dir().map(|d| wide::agent_paths(&d, &slug).dir.display().to_string());
        let frame = self.ensure_canvas_frame();
        let (_, (x, y)) = self.slot_or_overflow(&frame);
        let id = uuid::Uuid::new_v4().to_string();
        let mut node: Node = serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": name, "x": x, "y": y })).map_err(|e| e.to_string())?;
        node.kind = NodeKind::Session;
        node.cwd = dir;
        node.source = Some(crate::doc::CANVAS_AGENT.into());
        node.fresh = true;
        node.mcp_off = Some(self.doc.mcp.last_off.clone());
        node.agent = Some(self.default_agent().id().to_owned());
        node.passive = Some(Passive {
            key: uuid::Uuid::new_v4().to_string(),
            group: frame,
            slug,
            template: template.name.to_owned(),
            subscribe: template.subscribe.iter().map(|s| s.to_string()).collect(),
            every_minutes: template.every_minutes,
            daily_at: template.daily_at.to_owned(),
            rollover_runs: 20,
            // An hourly check plus the messages: more runs than the usual 24 a day.
            max_runs_per_day: if template.name == "Review follow-up" { 60 } else { 24 },
            scope: PassiveScope::Canvas,
            // The PR coordinator monitors handed-off pull requests (read-only follow-ups).
            prs: (template.name == "PR coordinator").then(canvas_model::prs::PrSettings::default),
            // The review follow-up's channel is yours to set (Follow-ups…); approval is on.
            reviews: (template.name == "Review follow-up").then(canvas_model::reviews::ReviewSettings::default),
            ..Default::default()
        });
        self.doc.nodes.push(node);
        self.mark_dirty();
        self.reveal_node(&id);
        self.select_node(&id, false);
        self.right_detail = Some(RightDetail::Passive(id.clone()));
        self.sync_routing();
        Ok(id)
    }

    /// The canvas's name for its GOALS.md: the canvas file's name.
    fn replica_name(&self) -> String {
        let path = self.doc_request.0.clone().unwrap_or_else(|| "canvas".into());
        std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "canvas".into())
    }

    /// Loads the canvas's skill library (a quick request to the backend).
    pub fn load_canvas_skills(&mut self) {
        let Some(canvas) = self.doc.canvas_id.clone().filter(|_| self.canvas_agents_unavailable().is_none()) else {
            self.canvas_skills = Some(Err(self.canvas_agents_unavailable().unwrap_or_default()));
            return;
        };
        self.canvas_skills = Some(self.backend.canvas_skills(&canvas).map_err(|e| {
            let text = format!("{e:#}");
            if text.contains("does not support") { "the service is older than canvas skills: update it".to_owned() } else { text }
        }));
    }

    /// The skills of the library (empty when they could not be read).
    pub fn skill_list(&self) -> Vec<wide::SkillInfo> {
        self.canvas_skills.as_ref().and_then(|s| s.as_ref().ok()).cloned().unwrap_or_default()
    }

    pub fn run_agents(&mut self, action: AgentAction) {
        self.needs_draw = true;
        match action {
            AgentAction::Panel => {
                self.load_canvas_skills();
                self.right_detail = Some(RightDetail::CanvasAgents);
            }
            AgentAction::New => {
                if let Some(why) = self.canvas_agents_unavailable() {
                    return self.notify(format!("Canvas-wide agents are unavailable: {why}"), true);
                }
                let items = TEMPLATES.iter().map(|t| (t.name.to_owned(), t.about.to_owned())).collect();
                self.modals.push(Modal::Pick(Pick { purpose: PickPurpose::CanvasTemplate, title: "New canvas-wide agent: template".into(), items, filter: String::new(), sel: 0, scroll: 0 }));
            }
            AgentAction::Show(id) => {
                if self.doc.node(&id).is_some() {
                    self.reveal_node(&id);
                    self.select_node(&id, false);
                    self.right_detail = Some(RightDetail::Passive(id));
                }
            }
            AgentAction::Toggle(id) => {
                let changed = self.doc.node_mut(&id).and_then(|n| n.passive.as_mut()).map(|p| {
                    p.disabled = !p.disabled;
                    p.disabled
                });
                if let Some(disabled) = changed {
                    self.mark_dirty();
                    self.sync_routing();
                    self.notify(if disabled { "Disabled: it gets no events, schedules or messages until you enable it." } else { "Enabled: it watches its events and schedule again." }, false);
                }
            }
            AgentAction::Stop(id) => self.stop_passive_run(&id),
            AgentAction::Skills(id) => self.pick_agent_skills(&id, 0),
            AgentAction::Library => self.pick_library(),
            AgentAction::NewSkill(agent) => self.modals.push(Modal::Prompt(Prompt {
                title: "New canvas skill: its name".into(),
                hint: "lowercase words with hyphens, for example pr-etiquette; its SKILL.md opens next".into(),
                input: TextInput::default(),
                purpose: PromptPurpose::NewSkill(agent),
            })),
            AgentAction::OpenSkill(name) => match self.skill_list().into_iter().find(|s| s.name == name) {
                Some(skill) => self.open_service_file(&skill.path),
                None => self.notify(format!("The canvas has no skill {name:?} (any more)."), true),
            },
            AgentAction::Folder => match self.canvas_agents_dir() {
                Some(dir) => {
                    if self.backend.path_info(&dir.display().to_string()).is_ok_and(|i| i.dir) {
                        self.open_folder(&dir.display().to_string());
                    } else {
                        self.notify("The canvas-agents folder is made with the first canvas-wide agent.", false);
                    }
                }
                None => self.notify(format!("Canvas-wide agents are unavailable: {}", self.canvas_agents_unavailable().unwrap_or_default()), true),
            },
            AgentAction::SendEvent => self.modals.push(Modal::Prompt(Prompt {
                title: "Send an event to the canvas-wide agents".into(),
                hint: "type [json data], for example: pr.published {\"pr\": 7}".into(),
                input: TextInput::default(),
                purpose: PromptPurpose::CanvasEvent,
            })),
            AgentAction::Reveal => {
                if let Some(frame) = self.doc.canvas_frame().map(|g| g.id.clone()) {
                    self.reveal_group(&frame);
                }
            }
        }
    }

    /// Ends a passive session's run in progress (the service stops its
    /// headless terminal; it counts as stopped, not failed), or stops its
    /// open chat.
    pub fn stop_passive_run(&mut self, id: &str) {
        let Some(node) = self.doc.node(id).cloned() else { return };
        let running = self.passive_view(&node).is_some_and(|v| v.running);
        if running {
            if let Some(key) = node.passive.as_ref().map(|p| p.key.clone()) {
                self.bus_action(serde_json::json!({ "action": "stop", "key": key }));
                self.notify("Stopping its run…", false);
            }
        } else if self.owned_alive(id) {
            self.stop_terminal(id, false);
            self.notify("Stopping its chat (it exits gracefully)…", false);
        } else {
            self.notify("No run in progress.", false);
        }
    }

    /// The skills picker of an agent: ✓ assigned, ○ not; Enter toggles one.
    pub fn pick_agent_skills(&mut self, id: &str, sel: usize) {
        let Some(passive) = self.doc.node(id).and_then(|n| n.passive.clone()) else { return };
        self.load_canvas_skills();
        if let Some(Err(why)) = &self.canvas_skills {
            return self.notify(format!("Cannot read the canvas skills: {why}"), true);
        }
        let library = self.skill_list();
        let mut items: Vec<(String, String)> = library
            .iter()
            .map(|s| (format!("{} {}", if passive.skills.contains(&s.name) { "✓" } else { "○" }, s.name), if s.problem.is_empty() { s.description.clone() } else { format!("⚠ {}", s.problem) }))
            .collect();
        // Assigned skills the library no longer has stay visible (they would be missing in runs).
        for missing in passive.skills.iter().filter(|s| !library.iter().any(|l| &l.name == *s)) {
            items.push((format!("✓ {missing}"), "⚠ not in the canvas library (removed?)".into()));
        }
        items.push((NEW_SKILL.into(), "a new skill in the canvas library, given to this agent".into()));
        items.push((OPEN_LIBRARY.into(), "the canvas skills folder".into()));
        let name = self.doc.node(id).map(|n| n.name.clone()).unwrap_or_default();
        self.modals.push(Modal::Pick(Pick { purpose: PickPurpose::AgentSkills(id.to_owned()), title: format!("Skills of “{name}” (Enter: give / take)"), sel: sel.min(items.len() - 1), items, filter: String::new(), scroll: 0 }));
    }

    /// The canvas's skill library: Enter opens a skill's SKILL.md.
    fn pick_library(&mut self) {
        self.load_canvas_skills();
        if let Some(Err(why)) = &self.canvas_skills {
            return self.notify(format!("Cannot read the canvas skills: {why}"), true);
        }
        let agents = self.canvas_agents().iter().filter_map(|n| n.passive.clone()).collect::<Vec<_>>();
        let mut items: Vec<(String, String)> = self
            .skill_list()
            .iter()
            .map(|s| {
                let users = agents.iter().filter(|p| p.skills.contains(&s.name)).count();
                let about = if s.problem.is_empty() { s.description.clone() } else { format!("⚠ {}", s.problem) };
                (format!("✦ {}", s.name), format!("{about} · {users} agent(s)"))
            })
            .collect();
        items.push((NEW_SKILL.into(), "a new skill in the canvas library".into()));
        items.push((OPEN_LIBRARY.into(), "the canvas skills folder".into()));
        self.modals.push(Modal::Pick(Pick { purpose: PickPurpose::CanvasSkills, title: "Canvas skills (Enter: open its SKILL.md)".into(), items, filter: String::new(), sel: 0, scroll: 0 }));
    }

    /// A choice in one of this module's pickers.
    pub fn apply_agent_pick(&mut self, purpose: PickPurpose, value: &str) {
        match purpose {
            PickPurpose::CanvasTemplate => {
                let template = TEMPLATES.iter().position(|t| t.name == value).unwrap_or(TEMPLATES.len() - 1);
                self.modals.push(Modal::Prompt(Prompt {
                    title: format!("Name the {} canvas-wide agent", TEMPLATES[template].name),
                    hint: "Enter keeps the template's name".into(),
                    input: TextInput::new(TEMPLATES[template].name),
                    purpose: PromptPurpose::CanvasAgentName(template),
                }));
            }
            PickPurpose::AgentSkills(id) if value == NEW_SKILL => self.run_agents(AgentAction::NewSkill(Some(id))),
            PickPurpose::AgentSkills(_) | PickPurpose::CanvasSkills if value == OPEN_LIBRARY => self.open_library_folder(),
            PickPurpose::AgentSkills(id) => {
                let skill = value.trim_start_matches(['✓', '○']).trim().to_owned();
                let index = self.toggle_agent_skill(&id, &skill);
                self.pick_agent_skills(&id, index);
            }
            PickPurpose::CanvasSkills if value == NEW_SKILL => self.run_agents(AgentAction::NewSkill(None)),
            PickPurpose::CanvasSkills => self.run_agents(AgentAction::OpenSkill(value.trim_start_matches('✦').trim().to_owned())),
            _ => {}
        }
    }

    /// Gives or takes a skill; answers the skill's row in the picker.
    fn toggle_agent_skill(&mut self, id: &str, skill: &str) -> usize {
        let library = self.skill_list();
        if let Some(passive) = self.doc.node_mut(id).and_then(|n| n.passive.as_mut()) {
            if let Some(at) = passive.skills.iter().position(|s| s == skill) {
                passive.skills.remove(at);
            } else if wide::valid_skill_name(skill) {
                passive.skills.push(skill.to_owned());
            }
            self.mark_dirty();
            self.sync_routing();
        }
        library.iter().position(|s| s.name == skill).unwrap_or(0)
    }

    fn open_library_folder(&mut self) {
        match self.canvas_agents_dir().map(|d| wide::skills_dir(&d)) {
            Some(dir) if self.backend.path_info(&dir.display().to_string()).is_ok_and(|i| i.dir) => self.open_folder(&dir.display().to_string()),
            Some(_) => self.notify("The canvas has no skills yet: + New skill… makes the first.", false),
            None => self.notify(format!("Canvas-wide agents are unavailable: {}", self.canvas_agents_unavailable().unwrap_or_default()), true),
        }
    }

    /// A new skill typed in a prompt: created in the library, given to
    /// `agent`, its SKILL.md opened to write its rules.
    pub fn create_skill(&mut self, agent: Option<String>, value: &str) -> Result<(), String> {
        if let Some(why) = self.canvas_agents_unavailable() {
            return Err(format!("Canvas skills are unavailable: {why}"));
        }
        let name = if wide::valid_skill_name(value.trim()) { value.trim().to_owned() } else { wide::skill_slug(value) };
        if !wide::valid_skill_name(&name) {
            return Err("type a name: lowercase words with hyphens, for example pr-etiquette".into());
        }
        let canvas = self.doc.canvas_id.clone().unwrap_or_default();
        let path = self.backend.create_canvas_skill(&canvas, &name, "").map_err(|e| format!("{e:#}"))?;
        if let Some(id) = agent
            && let Some(passive) = self.doc.node_mut(&id).and_then(|n| n.passive.as_mut())
            && !passive.skills.contains(&name)
        {
            passive.skills.push(name.clone());
            self.mark_dirty();
            self.sync_routing();
        }
        self.load_canvas_skills();
        self.notify(format!("Created the skill {name}: write its rules in its SKILL.md (its description says when it applies)."), false);
        self.open_service_file(&path);
        Ok(())
    }

    /// An event you send to the canvas-wide agents (`type [json]`).
    pub fn send_canvas_event(&mut self, text: &str) -> Result<(), String> {
        let text = text.trim();
        let (kind, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err("start with an event name such as pr.published".into());
        }
        let data = serde_json::from_str::<serde_json::Value>(rest.trim()).unwrap_or_else(|_| if rest.trim().is_empty() { serde_json::Value::Null } else { serde_json::Value::String(rest.trim().to_owned()) });
        let event = crate::bus::Event {
            kind: kind.to_owned(),
            to: crate::bus::CANVAS_TARGET.into(),
            source_name: "you".into(),
            data,
            canvas: self.doc.canvas_id.clone().unwrap_or_default(),
            ..Default::default()
        };
        self.sync_routing();
        self.bus_action(serde_json::json!({ "action": "emit", "event": event }));
        self.notify(format!("Sent {kind} to the canvas-wide agents."), false);
        Ok(())
    }

    /// The group filter of a canvas-wide agent as group names.
    pub fn agent_groups_text(&self, passive: &Passive) -> String {
        passive.groups.iter().map(|g| self.doc.group(g).map(|g| g.name.clone()).unwrap_or_else(|| format!("(removed {g})"))).collect::<Vec<_>>().join(", ")
    }

    /// Parses group names (comma-separated; empty: every group) into ids.
    pub fn parse_agent_groups(&self, value: &str) -> Result<Vec<String>, String> {
        let mut ids = Vec::new();
        for name in value.split(',').map(str::trim).filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case("all")) {
            let group = self.doc.groups.iter().find(|g| !g.is_canvas_agents() && g.name.eq_ignore_ascii_case(name)).ok_or_else(|| format!("no group is called “{name}”"))?;
            if !ids.contains(&group.id) {
                ids.push(group.id.clone());
            }
        }
        Ok(ids)
    }

    /// The frame's chips: its agents (or + agent) and the skills.
    pub fn frame_chips(&self, compact: bool) -> Vec<crate::folders::Chip> {
        let chip = |label: String, action: Action, lit: bool| crate::folders::Chip { label, action, lit };
        let count = self.canvas_agents().len();
        let mut chips = vec![match count {
            0 => chip(if compact { format!(" {MARK}+ ") } else { format!(" {MARK} + agent ") }, Action::Agents(AgentAction::New), false),
            n => chip(if compact { format!(" {MARK} {n} ") } else { format!(" {MARK} {n} · manage ") }, Action::Agents(AgentAction::Panel), true),
        }];
        chips.push(chip(if compact { " ✦ ".into() } else { " ✦ Skills ".into() }, Action::Agents(AgentAction::Library), true));
        chips
    }

    /// The frame's menu.
    pub fn frame_menu(&self, id: &str) -> (String, Vec<MenuItem>) {
        let item = |label: &str, hint: &str, action: Action, enabled: bool| MenuItem { label: label.to_owned(), hint: hint.to_owned(), action, enabled };
        let name = self.doc.group(id).map(|g| g.name.clone()).unwrap_or_default();
        let available = self.canvas_agents_unavailable().is_none();
        (
            name,
            vec![
                item("Manage canvas agents", "", Action::Agents(AgentAction::Panel), true),
                item("New canvas-wide agent…", "", Action::Agents(AgentAction::New), available),
                item("Canvas skills…", "", Action::Agents(AgentAction::Library), available),
                item("Send event to canvas agents…", "", Action::Agents(AgentAction::SendEvent), available),
                item("Open canvas-agents folder", "", Action::Agents(AgentAction::Folder), available),
                item("Session defaults for its agents…", "", Action::GroupDefaults(id.to_owned()), true),
                item("Rename frame", "R", Action::Rename(crate::dialogs::Target::Group(id.to_owned())), true),
                item("Fit to members", "", Action::FitGroup(id.to_owned()), true),
                item("Change color", "", Action::CycleGroupColor(id.to_owned()), true),
                item("Delete frame (keeps the agents)", "Del", Action::DeleteGroup(id.to_owned()), true),
            ],
        )
    }

    /// The menu items a canvas-wide agent's box adds.
    pub fn agent_menu_items(&self, node: &Node) -> Vec<MenuItem> {
        let item = |label: &str, action: Action, enabled: bool| MenuItem { label: label.to_owned(), hint: String::new(), action, enabled };
        let disabled = node.passive.as_ref().is_some_and(|p| p.disabled);
        vec![
            item(if disabled { "Enable" } else { "Disable" }, Action::Agents(AgentAction::Toggle(node.id.clone())), true),
            item("Skills…", Action::Agents(AgentAction::Skills(node.id.clone())), self.canvas_agents_unavailable().is_none()),
            item("Groups it watches…", Action::PassiveEdit(node.id.clone(), PassiveField::Groups), true),
            item("All canvas agents", Action::Agents(AgentAction::Panel), true),
        ]
    }
}

/// Picker rows that are not skills.
pub const NEW_SKILL: &str = "+ New skill…";
pub const OPEN_LIBRARY: &str = "↗ Open the skills folder";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Doc, Group};

    pub(crate) fn app(doc: Doc, demo: bool) -> (App, crossbeam_channel::Receiver<crate::protocol::ClientMsg>) {
        let (tx, sent) = crossbeam_channel::bounded(256);
        let (_service, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, _app_rx) = crossbeam_channel::unbounded();
        let app = App::new(crate::conn::Conn::detached(tx, service_rx), std::sync::Arc::new(crate::rpc::Offline), crate::sync::Opened::detached(doc), demo, ".".into(), app_tx, None);
        (app, sent)
    }

    fn agent(id: &str, frame: &str) -> Node {
        let mut node: Node = serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": id, "x": 0, "y": 0, "agent": "copilot" })).unwrap();
        node.source = Some(crate::doc::CANVAS_AGENT.into());
        node.passive = Some(Passive { key: format!("k-{id}"), group: frame.into(), slug: id.into(), subscribe: vec!["pr.*".into()], scope: PassiveScope::Canvas, max_runs_per_day: 24, ..Default::default() });
        node
    }

    #[test]
    fn the_state_machine_tells_off_paused_working_attention_and_monitoring_apart() {
        let base = Passive { subscribe: vec!["pr.*".into()], max_runs_per_day: 2, ..Default::default() };
        let view = |f: &dyn Fn(&mut PassiveView)| {
            let mut v = PassiveView::default();
            f(&mut v);
            v
        };
        let state = |p: &Passive, v: &PassiveView| agent_state(p, Some(v), false, None);
        assert_eq!(state(&base, &view(&|_| {})), AgentState::Monitoring);
        assert_eq!(agent_state(&Passive { subscribe: vec![], ..base.clone() }, None, false, None), AgentState::Waiting, "no triggers: messages and Run now only");
        assert_eq!(state(&Passive { disabled: true, ..base.clone() }, &view(&|v| v.running = true)), AgentState::Off, "off wins");
        assert_eq!(agent_state(&base, None, false, Some("x".into())), AgentState::Unavailable("x".into()));
        assert_eq!(state(&Passive { paused: true, ..base.clone() }, &view(&|v| v.running = true)), AgentState::Working(1), "a run in progress finishes first");
        assert_eq!(state(&Passive { paused: true, ..base.clone() }, &view(&|_| {})), AgentState::Paused);
        assert_eq!(agent_state(&base, None, true, None), AgentState::Working(1), "its open chat works");
        assert_eq!(state(&base, &view(&|v| (v.status, v.headline) = ("alert".into(), "PR 7 stuck".into()))), AgentState::Attention("PR 7 stuck".into()));
        assert_eq!(state(&base, &view(&|v| (v.failures, v.last_error) = (1, "exit 1".into()))), AgentState::Failing("exit 1".into()));
        assert_eq!(state(&base, &view(&|v| v.status = "done".into())), AgentState::Done(String::new()));
        assert_eq!(state(&base, &view(&|v| v.pending = 3)), AgentState::Queued(3, false));
        assert_eq!(state(&base, &view(&|v| (v.pending, v.runs_today) = (3, 2))), AgentState::Queued(3, true), "today's budget is used");
        for (s, attention) in [(AgentState::Attention(String::new()), true), (AgentState::Failing(String::new()), true), (AgentState::Monitoring, false), (AgentState::Paused, false), (AgentState::Off, false)] {
            assert_eq!(s.needs_attention(), attention, "{s:?}");
        }
        assert_eq!(AgentState::Monitoring.label(), "monitoring");
        assert!(AgentState::Unavailable("old service".into()).label().contains("old service"));
    }

    #[test]
    fn canvas_agents_need_a_canvas_id_and_their_agent_installed() {
        let mut doc = Doc::default();
        doc.groups.push(Group { id: "gf".into(), name: FRAME_NAME.into(), x: 0, y: 0, w: 40, h: 10, source: Some(crate::doc::CANVAS_AGENTS.into()), ..Default::default() });
        doc.nodes.push(agent("w", "gf"));
        let (mut app, _) = app(doc.clone(), false);
        app.info.home = r"C:\h".into();
        assert!(app.canvas_agents_unavailable().unwrap().contains("update it"), "an older service gave the canvas no id");
        assert!(matches!(app.canvas_agent_state(&app.doc.nodes[0].clone()), AgentState::Unavailable(_)));
        app.doc.canvas_id = Some("c1".into());
        assert_eq!(app.canvas_agents_unavailable(), None);
        assert_eq!(app.canvas_agents_dir(), Some(std::path::PathBuf::from(r"C:\h").join("canvas-agents").join("c1")));
        app.installed_agents = vec![crate::agents::AgentKind::OpenCode];
        assert!(app.agent_unavailable(&app.doc.nodes[0].clone()).unwrap().contains("not installed"));
        app.installed_agents = vec![crate::agents::AgentKind::Copilot];
        assert_eq!(app.canvas_agent_state(&app.doc.nodes[0].clone()), AgentState::Monitoring);
        let (demo, _) = super::tests::app(doc, true);
        assert!(demo.canvas_agents_unavailable().unwrap().contains("demo"));
    }

    #[test]
    fn group_filters_take_group_names() {
        let mut doc = Doc::default();
        doc.groups.push(Group { id: "g1".into(), name: "Payments".into(), w: 10, h: 10, ..Default::default() });
        doc.groups.push(Group { id: "gf".into(), name: FRAME_NAME.into(), w: 10, h: 10, source: Some(crate::doc::CANVAS_AGENTS.into()), ..Default::default() });
        let (app, _) = app(doc, false);
        assert_eq!(app.parse_agent_groups(" payments, Payments "), Ok(vec!["g1".to_owned()]));
        assert_eq!(app.parse_agent_groups(""), Ok(vec![]));
        assert_eq!(app.parse_agent_groups("all"), Ok(vec![]));
        assert!(app.parse_agent_groups("Canvas agents").is_err(), "not the frame");
        assert!(app.parse_agent_groups("Nope").unwrap_err().contains("Nope"));
        assert_eq!(app.agent_groups_text(&Passive { groups: vec!["g1".into()], ..Default::default() }), "Payments");
    }

    #[test]
    fn templates_subscribe_to_catalog_events_and_none_is_hardcoded() {
        let catalog: Vec<String> = crate::bus::parse_catalog(crate::bus::DEFAULT_CATALOG).into_iter().map(|e| e.name).collect();
        for template in &TEMPLATES {
            for pattern in template.subscribe {
                let prefix = pattern.strip_suffix(".*");
                assert!(catalog.iter().any(|c| c == pattern || prefix.is_some_and(|p| c.starts_with(&format!("{p}.")))), "{} subscribes to unknown {pattern}", template.name);
            }
        }
        assert!(TEMPLATES.iter().any(|t| t.name == "Custom" && t.subscribe.is_empty()));
        assert_eq!(slugify("PR coordinator (canvas)"), "pr-coordinator-canvas");
    }

    #[test]
    fn canvas_agents_are_routed_for_this_canvas_and_every_control_acts_on_the_same_spec() {
        use crate::dialogs::Action;
        use crate::protocol::ClientMsg;
        let mut doc = Doc { canvas_id: Some("c1".into()), ..Doc::default() };
        doc.groups.push(Group { id: "g1".into(), name: "Payments".into(), w: 40, h: 10, folder: Some(r"C:\g".into()), ..Default::default() });
        doc.groups.push(Group { id: "gf".into(), name: FRAME_NAME.into(), y: 20, w: 40, h: 10, source: Some(crate::doc::CANVAS_AGENTS.into()), ..Default::default() });
        let mut wide = agent("w", "gf");
        wide.passive.as_mut().unwrap().skills = vec!["pr-etiquette".into()];
        wide.passive.as_mut().unwrap().groups = vec!["g1".into()];
        doc.nodes.push(wide);
        let mut guard: Node = serde_json::from_value(serde_json::json!({ "id": "p", "kind": "session", "name": "Guard", "x": 0, "y": 0 })).unwrap();
        guard.passive = Some(Passive { key: "k-p".into(), group: "g1".into(), skills: vec!["ignored".into()], ..Default::default() });
        doc.nodes.push(guard);
        let (mut app, sent) = app(doc, false);
        app.info.home = r"C:\h".into();
        let routing = app.build_routing();
        assert_eq!(routing.canvas, "c1");
        let spec = |r: &crate::bus::Routing, key: &str| r.passive.iter().find(|p| p.key == key).cloned();
        let w = spec(&routing, "k-w").expect("routed");
        assert!(w.canvas_wide && w.skills == ["pr-etiquette"] && w.groups == ["g1"] && w.session_id == "w" && w.group == "gf");
        let p = spec(&routing, "k-p").expect("routed");
        assert!(!p.canvas_wide && p.skills.is_empty(), "a group passive session has no canvas skills");
        // Disable: no longer routed; enable: back (the routing goes to the service at once).
        app.run(Action::Agents(AgentAction::Toggle("w".into())));
        assert!(app.doc.node("w").unwrap().passive.as_ref().unwrap().disabled && spec(&app.build_routing(), "k-w").is_none());
        let routed: Vec<String> = sent.try_iter().filter_map(|m| if let ClientMsg::Routing { json } = m { Some(json) } else { None }).collect();
        assert!(routed.last().is_some_and(|j| !j.contains("k-w") && j.contains("k-p")), "{routed:?}");
        app.run(Action::Agents(AgentAction::Toggle("w".into())));
        assert!(spec(&app.build_routing(), "k-w").is_some());
        // Pause is the same setting from the box menu, the panel and the management panel.
        app.run(Action::PassivePause("w".into()));
        assert!(spec(&app.build_routing(), "k-w").unwrap().paused);
        app.run(Action::PassivePause("w".into()));
        // Stop: a run in progress is stopped through the service (not counted as a failure there).
        app.bus.passive.insert("k-w".into(), crate::bus::PassiveView { running: true, ..Default::default() });
        app.run(Action::Agents(AgentAction::Stop("w".into())));
        let stops: Vec<String> = sent.try_iter().filter_map(|m| if let ClientMsg::Bus { json } = m { Some(json) } else { None }).collect();
        assert!(stops.iter().any(|j| j.contains("\"stop\"") && j.contains("k-w")), "{stops:?}");
        // Viewing it (panel row, box, sidebar) selects the same box and launches nothing.
        app.run(Action::Agents(AgentAction::Show("w".into())));
        assert_eq!((app.selection.clone(), app.right_detail.clone()), (vec!["w".to_owned()], Some(RightDetail::Passive("w".into()))));
        app.start_node("w");
        assert!(!sent.try_iter().any(|m| matches!(m, ClientMsg::Launch { .. })), "inspecting never launches");
        // A on the canvas opens the management panel (the top bar's ◎ Agents does the same).
        app.right_detail = None;
        app.set_focus(crate::app::Focus::Canvas);
        app.on_input(vec![crate::input::InputEvent::Key(crate::input::KeyEvent::new(crate::input::KeyCode::Char('a'), crate::input::Modifiers::NONE))]);
        assert_eq!(app.right_detail, Some(RightDetail::CanvasAgents));
        // An older service gave the canvas no id: canvas-wide agents are not routed (group passive sessions are).
        app.doc.canvas_id = None;
        let routing = app.build_routing();
        assert!(routing.canvas.is_empty() && spec(&routing, "k-w").is_none() && spec(&routing, "k-p").is_some());
    }

    #[test]
    fn the_frame_is_made_once_and_gathers_orphaned_agents() {
        let mut doc = Doc { canvas_id: Some("c1".into()), ..Doc::default() };
        doc.nodes.push(agent("w", "deleted-frame"));
        let (mut app, _) = app(doc, false);
        let frame = app.ensure_canvas_frame();
        assert_eq!(app.ensure_canvas_frame(), frame, "one frame");
        assert!(app.doc.group(&frame).is_some_and(|g| g.is_canvas_agents()));
        assert_eq!(app.doc.node("w").unwrap().passive.as_ref().unwrap().group, frame, "an agent whose frame was deleted belongs to the new one");
        assert_eq!(app.group_passives(&frame).len(), 0, "canvas-wide agents are not a group's passive sessions");
        let labels: Vec<String> = app.group_chips(&app.doc.group(&frame).unwrap().clone(), false).iter().map(|c| c.label.trim().to_owned()).collect();
        assert_eq!(labels, [format!("{MARK} 1 · manage"), "✦ Skills".to_owned()]);
    }
}
