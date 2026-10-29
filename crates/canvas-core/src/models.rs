//! Choosing models: the new-session dialog's Model row, Settings' default
//! model of each agent, and the picker both open (the agent's own model list,
//! the initial default, the agent's own default, or an id typed by hand).
//! Resolution and ids live in [`canvas_model::models`].

use canvas_model::models::{self, Effective, Source};

use crate::agents::{AgentKind, agent_of};
use crate::app::App;
use crate::dialogs::{Modal, ModelTarget, NewSession, Pick, PickPurpose, Prompt, PromptPurpose, TextInput};
use crate::doc::{Group, Node};

/// Picker entry: type an id by hand.
pub const TYPE_ID: &str = "Type a model id…";
/// Picker entry: no model flag (the agent decides).
pub const AGENT_DEFAULT: &str = "(agent default)";
/// Picker entry in Settings: forget the choice.
pub const NOT_SET: &str = "(not set)";

/// A model id for people (the agent's own default when empty).
pub fn shown(model: &str) -> String {
    if model.trim().is_empty() { AGENT_DEFAULT.to_owned() } else { model.trim().to_owned() }
}

/// Why a new session gets its model, in a few words.
pub fn source_label(source: &Source, kind: AgentKind) -> String {
    match source {
        Source::Group(name) => format!("from group “{name}”"),
        Source::Remembered => format!("remembered default for {}", kind.label()),
        Source::Initial => format!("initial default ({})", models::INITIAL_LABEL),
    }
}

/// A Settings model row: (where it comes from, the id that applies). `set`
/// is the model in the defaults shown (the canvas's, or a group's when not
/// `canvas`), `remembered` the canvas's.
pub fn settings_value(set: Option<&str>, remembered: Option<&str>, kind: AgentKind, canvas: bool) -> (String, String) {
    match (set, canvas) {
        (Some(model), true) => ("chosen".into(), shown(model)),
        (Some(model), false) => ("group's".into(), shown(model)),
        (None, true) => ("initial".into(), models::initial(kind).into()),
        (None, false) => ("canvas".into(), shown(remembered.unwrap_or(models::initial(kind)))),
    }
}

/// What the new-session dialog's Model row shows.
#[derive(Clone, Debug, PartialEq)]
pub struct DialogModel {
    /// The id (empty: the agent's own default; `None`: unknown, a fork of a
    /// session whose model was not recorded).
    pub model: Option<String>,
    pub note: String,
}

impl App {
    fn dialog_group(&self, dialog: &NewSession) -> Option<&Group> {
        dialog.group.as_deref().and_then(|g| self.doc.group(g))
    }

    /// The default a new session from `dialog` gets (no choice made in it).
    pub fn dialog_default(&self, dialog: &NewSession) -> Effective {
        models::effective(dialog.agent, &self.doc.defaults, self.dialog_group(dialog))
    }

    /// The model a session created from `dialog` starts with, and why.
    pub fn dialog_model(&self, dialog: &NewSession) -> DialogModel {
        let kind = dialog.agent;
        if dialog.forks() {
            let model = dialog.parent.as_deref().and_then(|p| self.doc.node(p)).and_then(|n| n.model.clone());
            return DialogModel { model, note: "the parent's: a fork continues its conversation".into() };
        }
        let default = self.dialog_default(dialog);
        match &dialog.model {
            Some(model) => {
                let note = match &default.source {
                    Source::Group(name) => format!("this session only (group “{name}” has its own)"),
                    _ => format!("your choice; becomes the default for {}", kind.label()),
                };
                DialogModel { model: Some(model.clone()), note }
            }
            None => DialogModel { note: source_label(&default.source, kind), model: Some(default.model) },
        }
    }

    /// What a session created from `dialog` records as its model, and the
    /// choice to remember as its agent's default. A choice made in a group
    /// that sets its own model applies to that session only.
    pub fn new_session_model(&self, dialog: &NewSession, group: Option<&Group>) -> (Option<String>, Option<String>) {
        let kind = dialog.agent;
        let default = models::effective(kind, &self.doc.defaults, group);
        if dialog.forks() {
            let parent = dialog.parent.as_deref().and_then(|p| self.doc.node(p)).and_then(|n| n.model.clone());
            return (parent.filter(|_| !self.demo), None);
        }
        let remember = |choice: &String| {
            let overridden = matches!(default.source, Source::Group(_));
            (!overridden && self.doc.defaults.model(kind) != Some(choice.as_str())).then(|| choice.clone())
        };
        match &dialog.model {
            Some(choice) => ((!self.demo).then(|| choice.clone()), remember(choice)),
            None => ((!self.demo).then_some(default.model), None),
        }
    }

    /// Opens the model picker for the new-session dialog (on top) or for a
    /// default in Settings.
    pub fn open_model_picker(&mut self, target: ModelTarget) {
        let (kind, current, extra, title) = match &target {
            ModelTarget::Dialog => {
                let Some(Modal::NewSession(dialog)) = self.modals.last() else { return };
                if dialog.forks() {
                    return self.notify("A fork continues its parent's conversation and model; choose “Start fresh” to pick a model.", false);
                }
                let current = self.dialog_model(dialog).model.unwrap_or_default();
                let default = self.dialog_default(dialog);
                let extra = (dialog.model.is_some()).then(|| (shown(&default.model), source_label(&default.source, dialog.agent)));
                (dialog.agent, Some(current), extra, format!("Model of the new {} session", dialog.agent.label()))
            }
            ModelTarget::Default(kind, group) => {
                let (defaults, scope) = match group.as_deref().and_then(|g| self.doc.group(g)) {
                    Some(g) => (g.defaults.clone().unwrap_or_default(), format!("group “{}”", g.name)),
                    None => (self.doc.defaults.clone(), "new sessions".to_owned()),
                };
                let note = if group.is_some() { "use the canvas default".to_owned() } else { format!("{} (initial default)", models::initial(*kind)) };
                (*kind, defaults.model(*kind).map(str::to_owned), Some((NOT_SET.to_owned(), note)), format!("{} model for {scope}", kind.label()))
            }
        };
        let mut items: Vec<(String, String)> = Vec::new();
        let mut push = |id: String, note: String| {
            if !items.iter().any(|(i, _)| *i == id) {
                items.push((id, note));
            }
        };
        if let Some(current) = current {
            push(shown(&current), "current".into());
        }
        if let Some((id, note)) = extra {
            push(id, note);
        }
        push(TYPE_ID.into(), format!("any id {} accepts", kind.label()));
        push(models::initial(kind).into(), format!("{} (initial default)", models::INITIAL_LABEL));
        push(AGENT_DEFAULT.into(), format!("no model flag: {} decides", kind.label()));
        match self.agent_configs.get(&kind) {
            Some(config) => {
                for id in &config.models {
                    push(id.clone(), format!("listed by {}", kind.label()));
                }
            }
            None => self.refresh_config_for(kind),
        }
        self.modals.push(Modal::Pick(Pick { purpose: PickPurpose::Model(target), title, items, filter: String::new(), sel: 0, scroll: 0 }));
    }

    /// Applies a picked or typed model: to the dialog's session, or as a
    /// default of the canvas or a group.
    pub fn apply_model_choice(&mut self, target: ModelTarget, value: &str) -> Result<(), String> {
        let value = value.trim();
        if value == TYPE_ID {
            let (kind, current) = match &target {
                ModelTarget::Dialog => match self.modals.last() {
                    Some(Modal::NewSession(dialog)) => (dialog.agent, self.dialog_model(dialog).model.unwrap_or_default()),
                    _ => return Ok(()),
                },
                ModelTarget::Default(kind, _) => (*kind, String::new()),
            };
            self.modals.push(Modal::Prompt(Prompt {
                title: format!("{} model id", kind.label()),
                hint: format!("as {} names it, e.g. {}", kind.label(), models::initial(kind)),
                input: TextInput::new(&current),
                purpose: PromptPurpose::Model(target),
            }));
            return Ok(());
        }
        let model = match value {
            NOT_SET => None,
            AGENT_DEFAULT | "" => Some(String::new()),
            id if models::valid_id(id) => Some(id.to_owned()),
            id => return Err(format!("“{id}” is not a model id (one word, not starting with -)")),
        };
        match target {
            ModelTarget::Dialog => {
                let Some(Modal::NewSession(dialog)) = self.modals.last() else { return Ok(()) };
                let default = self.dialog_default(dialog).model;
                // Picking the default again is no choice of its own.
                let model = model.filter(|m| *m != default);
                if let Some(Modal::NewSession(dialog)) = self.modals.last_mut() {
                    dialog.model = model;
                    dialog.error = None;
                }
            }
            ModelTarget::Default(kind, group) => {
                match group.as_deref().and_then(|g| self.doc.group_mut(g)) {
                    Some(group) => {
                        let mut defaults = group.defaults.clone().unwrap_or_default();
                        defaults.set_model(kind, model.as_deref());
                        group.defaults = (!defaults.is_empty()).then_some(defaults);
                    }
                    None => self.doc.defaults.set_model(kind, model.as_deref()),
                }
                self.mark_dirty();
            }
        }
        self.needs_draw = true;
        Ok(())
    }

    /// The details panel's model line of a session box (empty: nothing to say).
    pub fn session_model_text(&self, node: &Node) -> String {
        if node.command.is_some() {
            return String::new();
        }
        let kind = agent_of(node);
        match node.model.as_deref() {
            Some(model) if node.fresh => format!("{} (chosen; on start)", shown(model)),
            Some(model) => format!("{} · kept on resume", shown(model)),
            None if node.fresh => {
                let default = models::effective(kind, &self.doc.defaults, self.doc.group_of(node));
                format!("{} ({}; on start)", shown(&default.model), source_label(&default.source, kind))
            }
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialogs::Action;
    use crate::doc::{Doc, LaunchDefaults};

    fn app(doc: Doc) -> App {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let (_service, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, _app_rx) = crossbeam_channel::unbounded();
        let mut app = App::new(crate::conn::Conn::detached(tx, service_rx), std::sync::Arc::new(crate::rpc::Offline), crate::sync::Opened::detached(doc), false, ".".into(), app_tx, None);
        app.installed_agents = crate::agents::ALL.to_vec();
        app.detected_agent = AgentKind::OpenCode;
        app.agent_configs.insert(AgentKind::OpenCode, crate::config::Snapshot { models: vec!["opencode/big-pickle".into(), "github-copilot/gpt-6-sol".into()], ..Default::default() });
        app
    }

    fn dialog(app: &App) -> &NewSession {
        match app.modals.last() {
            Some(Modal::NewSession(dialog)) => dialog,
            other => panic!("no dialog on top: {other:?}"),
        }
    }

    fn pick(app: &mut App, value: &str) {
        let Some(Modal::Pick(pick)) = app.modals.pop() else { panic!("no picker") };
        assert!(pick.items.iter().any(|(id, _)| id == value) || value == TYPE_ID, "{value} offered in {:?}", pick.items);
        let PickPurpose::Model(target) = pick.purpose else { panic!("not a model picker") };
        app.apply_model_choice(target, value).unwrap();
    }

    #[test]
    fn the_dialog_shows_the_effective_model_and_remembers_a_choice_for_its_agent() {
        let mut app = app(Doc::default());
        app.run(Action::NewSession { at: None, parent: None });
        let shown = app.dialog_model(dialog(&app));
        assert_eq!(shown.model.as_deref(), Some("github-copilot/claude-opus-5.5"));
        assert!(shown.note.contains("initial default"), "{}", shown.note);
        // The picker lists OpenCode's models; the choice applies to this session.
        app.open_model_picker(ModelTarget::Dialog);
        pick(&mut app, "github-copilot/gpt-6-sol");
        let shown = app.dialog_model(dialog(&app));
        assert_eq!((shown.model.as_deref(), shown.note.contains("becomes the default")), (Some("github-copilot/gpt-6-sol"), true));
        let (model, remember) = app.new_session_model(dialog(&app), None);
        assert_eq!((model.as_deref(), remember.as_deref()), (Some("github-copilot/gpt-6-sol"), Some("github-copilot/gpt-6-sol")));
        // Changing the agent drops a choice made for another agent.
        let mut d = dialog(&app).clone();
        app.cycle_dialog_agent(&mut d, true);
        assert_eq!((d.agent, d.model.as_ref()), (AgentKind::Claude, None));
        assert_eq!(app.dialog_model(&d).model.as_deref(), Some("claude-opus-5-5"));
    }

    #[test]
    fn creating_the_session_records_its_model_and_remembers_the_choice() {
        let mut app = app(Doc::default());
        app.run(Action::NewSession { at: None, parent: None });
        app.open_model_picker(ModelTarget::Dialog);
        pick(&mut app, "opencode/big-pickle");
        let d = dialog(&app).clone();
        app.modals.pop();
        // What submitting records (the dialog's submit needs a backend for its folder; parity row 38 runs it end to end).
        let (model, remember) = app.new_session_model(&d, None);
        assert_eq!((model.as_deref(), remember.as_deref()), (Some("opencode/big-pickle"), Some("opencode/big-pickle")));
        // Once remembered, the next dialog shows it as the default; picking it again is no new choice.
        app.doc.defaults.set_model(AgentKind::OpenCode, remember.as_deref());
        app.run(Action::NewSession { at: None, parent: None });
        let shown = app.dialog_model(dialog(&app));
        assert_eq!((shown.model.as_deref(), shown.note.as_str()), (Some("opencode/big-pickle"), "remembered default for OpenCode"));
        app.open_model_picker(ModelTarget::Dialog);
        pick(&mut app, "opencode/big-pickle");
        assert_eq!(dialog(&app).model, None);
        assert_eq!(app.new_session_model(dialog(&app), None), (Some("opencode/big-pickle".into()), None));
    }

    #[test]
    fn a_group_override_wins_and_a_choice_there_applies_to_that_session_only() {
        let mut overrides = LaunchDefaults::default();
        overrides.set_model(AgentKind::OpenCode, Some("opencode/big-pickle"));
        let mut doc = Doc::default();
        doc.groups.push(Group { id: "g".into(), name: "Team".into(), x: -50, y: -50, w: 500, h: 300, defaults: Some(overrides), ..Default::default() });
        doc.defaults.set_model(AgentKind::OpenCode, Some("github-copilot/gpt-6-sol"));
        let mut app = app(doc);
        app.run(Action::NewSession { at: Some((2, 2)), parent: None });
        assert_eq!(dialog(&app).group.as_deref(), Some("g"), "the new box lands in the group");
        let shown = app.dialog_model(dialog(&app));
        assert_eq!((shown.model.as_deref(), shown.note.as_str()), (Some("opencode/big-pickle"), "from group “Team”"));
        app.open_model_picker(ModelTarget::Dialog);
        pick(&mut app, AGENT_DEFAULT);
        assert!(app.dialog_model(dialog(&app)).note.contains("this session only"));
        let group = app.doc.group("g").cloned();
        assert_eq!(app.new_session_model(dialog(&app), group.as_ref()), (Some(String::new()), None), "the agent's own default, not remembered");
        assert_eq!(app.doc.defaults.model(AgentKind::OpenCode), Some("github-copilot/gpt-6-sol"), "the canvas default is untouched");
    }

    #[test]
    fn settings_change_and_reset_each_agents_default() {
        let mut app = app(Doc::default());
        app.open_model_picker(ModelTarget::Default(AgentKind::Claude, None));
        pick(&mut app, TYPE_ID);
        let Some(Modal::Prompt(prompt)) = app.modals.pop() else { panic!("a prompt for the id") };
        assert!(app.apply_prompt(prompt.purpose.clone(), "-x".into()).is_err(), "never an option");
        app.apply_prompt(prompt.purpose, " sonnet ".into()).unwrap();
        assert_eq!(app.doc.defaults.model(AgentKind::Claude), Some("sonnet"));
        assert_eq!(settings_value(app.doc.defaults.model(AgentKind::Claude), None, AgentKind::Claude, true), ("chosen".into(), "sonnet".into()));
        app.open_model_picker(ModelTarget::Default(AgentKind::Claude, None));
        pick(&mut app, NOT_SET);
        assert_eq!(app.doc.defaults.model(AgentKind::Claude), None);
        assert_eq!(settings_value(None, None, AgentKind::Claude, true), ("initial".into(), "claude-opus-5-5".into()));
        assert_eq!(settings_value(None, Some("opus"), AgentKind::Claude, false), ("canvas".into(), "opus".into()), "a group without an override shows what applies");
        // A group's override, then back to the canvas default.
        app.doc.groups.push(Group { id: "g".into(), name: "Team".into(), w: 10, h: 10, ..Default::default() });
        app.open_model_picker(ModelTarget::Default(AgentKind::OpenCode, Some("g".into())));
        pick(&mut app, "github-copilot/gpt-6-sol");
        assert_eq!(app.doc.group("g").and_then(|g| g.defaults.as_ref()).and_then(|d| d.model(AgentKind::OpenCode)), Some("github-copilot/gpt-6-sol"));
        app.open_model_picker(ModelTarget::Default(AgentKind::OpenCode, Some("g".into())));
        pick(&mut app, NOT_SET);
        assert_eq!(app.doc.group("g").and_then(|g| g.defaults.clone()), None, "an empty override is dropped");
    }

    #[test]
    fn the_details_panel_names_a_boxs_model() {
        let mut app = app(Doc::default());
        let mut node: Node = serde_json::from_value(serde_json::json!({ "id": "n", "kind": "session", "name": "n", "x": 0, "y": 0, "agent": "claude", "fresh": true })).unwrap();
        assert_eq!(app.session_model_text(&node), "claude-opus-5-5 (initial default (Claude Opus 5.5); on start)");
        node.fresh = false;
        assert_eq!(app.session_model_text(&node), "", "not recorded: nothing to say");
        node.model = Some("claude-opus-5-5".into());
        app.doc.defaults.set_model(AgentKind::Claude, Some("sonnet"));
        assert_eq!(app.session_model_text(&node), "claude-opus-5-5 · kept on resume", "a new default does not change it");
    }
}
