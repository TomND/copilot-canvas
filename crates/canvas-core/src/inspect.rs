//! The transcript inspector and resuming agent-created child conversations.
//!
//! Inspect shows any session's or subagent's conversation read-only (your
//! prompts, the agent's answers, its tool calls), newest at the bottom, and
//! refreshes it while it runs: nothing is started and nothing is written.
//!
//! A finished OpenCode child session (the task tool's subagents) that is
//! not running anywhere resumes into a terminal of its own as a new linked
//! subsession box (`opencode -s <child>`); the subagent box stays as it is.
//! Copilot and Claude Code subagents are part of their parent's
//! conversation and are not resumable on their own. The service refuses a
//! session that is open elsewhere right before it launches it.

use std::time::{Duration, Instant};

use crate::agents::{AgentKind, agent_of, native_id};
use crate::app::{App, AppEvent};
use crate::dialogs::{Action, Modal};
use crate::doc::{GRID_X, Node, NodeKind};
use crate::input::{KeyCode, KeyEvent, Modifiers};
use crate::protocol::AgentStatus;
use crate::rpc::Request;
use crate::transcript::{EntryKind, Transcript};

/// Entries asked for.
const LIMIT: u32 = 300;
/// Refresh while the conversation runs, and otherwise.
const LIVE_EVERY: Duration = Duration::from_millis(1500);
const IDLE_EVERY: Duration = Duration::from_secs(5);

/// The read-only transcript of a box (a modal).
#[derive(Clone, Debug)]
pub struct Inspector {
    /// The box it shows.
    pub node: String,
    pub title: String,
    /// What the backend is asked (again on every refresh).
    pub request: Request,
    pub transcript: Option<Transcript>,
    pub error: Option<String>,
    pub loading: bool,
    pub fetched_at: Option<Instant>,
    /// First visible line; `follow` keeps the newest lines in view.
    pub top: usize,
    pub follow: bool,
    /// Visible rows and all lines at the last draw (for paging).
    pub rows: usize,
    pub total: usize,
    /// Focused button.
    pub sel: usize,
}

/// A transcript line's role (the drawing picks its colour).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    User,
    Assistant,
    Tool,
    Result,
    Failed,
    Note,
    Blank,
}

/// The transcript as lines `width` cells wide: prompts after a blank line,
/// answers wrapped, a tool call per line with its result below.
pub fn lines(transcript: &Transcript, width: usize) -> Vec<(String, Line)> {
    let width = width.max(16);
    let mut out: Vec<(String, Line)> = Vec::new();
    let wrapped = |out: &mut Vec<(String, Line)>, first: &str, rest: &str, text: &str, kind: Line| {
        for (i, line) in crate::ui_side::wrap(text, width - 2).into_iter().enumerate() {
            out.push((format!("{}{line}", if i == 0 { first } else { rest }), kind));
        }
    };
    for entry in &transcript.entries {
        match entry.kind {
            EntryKind::User => {
                if !out.is_empty() {
                    out.push((String::new(), Line::Blank));
                }
                wrapped(&mut out, "❯ ", "  ", &entry.text, Line::User);
            }
            EntryKind::Assistant => wrapped(&mut out, "", "", &entry.text, Line::Assistant),
            EntryKind::Tool => {
                let call = if entry.args.is_empty() { format!("⚙ {}", entry.tool) } else { format!("⚙ {} {}", entry.tool, entry.args) };
                out.push((crate::paint::truncate(&call, width), Line::Tool));
                let (result, kind) = match (entry.running, entry.failed) {
                    (true, _) => ("… running".to_owned(), Line::Result),
                    (false, true) => (format!("✗ {}", if entry.result.is_empty() { "failed" } else { &entry.result }), Line::Failed),
                    (false, false) if entry.result.is_empty() => (String::new(), Line::Result),
                    (false, false) => (format!("→ {}", entry.result), Line::Result),
                };
                if !result.is_empty() {
                    out.push((crate::paint::truncate(&format!("  {result}"), width), kind));
                }
            }
            EntryKind::Note => out.push((crate::paint::truncate(&format!("· {}", entry.text), width), if entry.failed { Line::Failed } else { Line::Note })),
        }
    }
    out
}

/// What a key did in the inspector.
pub enum KeyOutcome {
    Keep,
    Close,
    Run(Action),
}

/// Whether a subagent's own conversation can resume on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChildResume {
    /// As a new linked subsession box that resumes this native session.
    Allowed { native: String },
    /// It was resumed before into this box.
    Existing(String),
    /// Not on its own; why.
    No(String),
}

/// The resume rule for a subagent's own conversation: only OpenCode's
/// children are sessions of their own, and one resumes only when nothing
/// could be writing to it (its parent is not running here or elsewhere, it
/// is not open elsewhere, it is not running).
pub fn child_resume_rule(kind: AgentKind, status: Option<AgentStatus>, parent_here: bool, parent_elsewhere: bool, child_elsewhere: bool) -> Result<(), String> {
    match kind {
        AgentKind::Copilot => return Err("A Copilot subagent is part of its parent's conversation: it cannot be resumed on its own (resume the parent).".into()),
        AgentKind::Claude => return Err("A Claude Code subagent is part of its parent's conversation: it cannot be resumed on its own (resume the parent).".into()),
        AgentKind::OpenCode => {}
    }
    if status == Some(AgentStatus::Working) && (parent_here || parent_elsewhere) {
        return Err("It is still running inside its parent's terminal: inspect it while it works.".into());
    }
    if parent_here {
        return Err("Its parent runs in this canvas and may continue it: stop the parent to resume this child on its own.".into());
    }
    if parent_elsewhere || child_elsewhere {
        return Err("It is open in another terminal; Copilot Canvas never starts a second copy.".into());
    }
    Ok(())
}

impl App {
    /// The transcript request for a box (`None`: no conversation, e.g. a shell).
    pub fn transcript_request(&self, id: &str) -> Option<Request> {
        let node = self.doc.node(id)?;
        let cwd = |n: &Node| n.cwd.clone().filter(|c| !c.is_empty()).or_else(|| self.summary(n).map(|s| s.cwd.clone()).filter(|c| !c.is_empty()));
        match node.kind {
            NodeKind::Session if node.command.is_none() => {
                Some(Request::Transcript { agent: agent_of(node), session: native_id(node).to_owned(), parent: None, agent_key: None, cwd: cwd(node), limit: LIMIT })
            }
            NodeKind::Agent => {
                let parent = self.doc.node(node.parent.as_deref()?)?;
                if parent.command.is_some() {
                    return None;
                }
                let key = node.agent_key.clone()?;
                Some(match agent_of(parent) {
                    // A child session of its own.
                    AgentKind::OpenCode => Request::Transcript { agent: AgentKind::OpenCode, session: key, parent: Some(native_id(parent).to_owned()), agent_key: None, cwd: None, limit: LIMIT },
                    kind => Request::Transcript { agent: kind, session: native_id(parent).to_owned(), parent: None, agent_key: Some(key), cwd: cwd(parent), limit: LIMIT },
                })
            }
            _ => None,
        }
    }

    /// Opens the read-only transcript of a box.
    pub fn inspect(&mut self, id: &str) {
        let Some(request) = self.transcript_request(id) else {
            return self.notify("This box has no agent conversation to inspect.", false);
        };
        let title = self.doc.node(id).map(|n| n.name.clone()).unwrap_or_default();
        self.modals.push(Modal::Inspector(Box::new(Inspector {
            node: id.to_owned(),
            title,
            request,
            transcript: None,
            error: None,
            loading: false,
            fetched_at: None,
            top: 0,
            follow: true,
            rows: 20,
            total: 0,
            sel: 0,
        })));
        self.fetch_transcript();
    }

    fn inspector_mut(&mut self) -> Option<&mut Inspector> {
        self.modals.iter_mut().rev().find_map(|m| if let Modal::Inspector(i) = m { Some(&mut **i) } else { None })
    }

    /// Reads the open inspector's transcript again (in the background).
    pub fn fetch_transcript(&mut self) {
        let Some(inspector) = self.inspector_mut().filter(|i| !i.loading) else { return };
        inspector.loading = true;
        let (node, request) = (inspector.node.clone(), inspector.request.clone());
        self.in_background(
            move |backend| backend.transcript(request).map_err(|e| format!("{e:#}")),
            move |result| AppEvent::Run(Box::new(move |app: &mut App| app.on_transcript(&node, result))),
        );
    }

    fn on_transcript(&mut self, node: &str, result: Result<Transcript, String>) {
        let Some(inspector) = self.inspector_mut().filter(|i| i.node == node) else { return };
        inspector.loading = false;
        inspector.fetched_at = Some(Instant::now());
        match result {
            Ok(transcript) => {
                inspector.transcript = Some(transcript);
                inspector.error = None;
            }
            Err(error) => inspector.error = Some(error),
        }
        self.needs_draw = true;
    }

    /// Refreshes an open inspector: every 1.5 s while its conversation runs, else every 5 s.
    pub fn tick_inspector(&mut self) {
        let Some(inspector) = self.inspector_mut() else { return };
        let every = if inspector.transcript.as_ref().is_some_and(|t| t.running) { LIVE_EVERY } else { IDLE_EVERY };
        if !inspector.loading && inspector.fetched_at.is_some_and(|t| t.elapsed() >= every) {
            self.fetch_transcript();
        }
    }

    /// What a subagent box's own conversation may do.
    pub fn child_resume(&self, node: &Node) -> ChildResume {
        let Some(parent) = node.parent.as_deref().and_then(|p| self.doc.node(p)) else { return ChildResume::No("Its parent session is not on the canvas.".into()) };
        let kind = agent_of(parent);
        let key = node.agent_key.clone().unwrap_or_default();
        if kind == AgentKind::OpenCode {
            if !kind.is_session_id(&key) {
                return ChildResume::No("Its OpenCode session is not known.".into());
            }
            if let Some(existing) = self.doc.nodes.iter().find(|n| n.kind == NodeKind::Session && native_id(n) == key) {
                return ChildResume::Existing(existing.id.clone());
            }
        }
        let summary = self.summary(node);
        let status = summary.and_then(|s| s.agents.iter().find(|a| a.key == key)).map(|a| a.status);
        let parent_elsewhere = self.summary(parent).is_some_and(|s| s.live) && !self.owned_alive(&parent.id);
        let child_elsewhere = self.sessions.get(&key).is_some_and(|s| s.live);
        match child_resume_rule(kind, status, self.owned_alive(&parent.id), parent_elsewhere, child_elsewhere) {
            Ok(()) => ChildResume::Allowed { native: key },
            Err(reason) => ChildResume::No(reason),
        }
    }

    /// Resumes a subagent's own conversation in a terminal of its own: a new
    /// subsession box of the parent that follows the child's session (the
    /// box from an earlier resume when there is one).
    pub fn resume_child(&mut self, id: &str) {
        let Some(node) = self.doc.node(id).cloned() else { return };
        let native = match self.child_resume(&node) {
            ChildResume::Existing(box_id) => return self.start_node(&box_id),
            ChildResume::No(reason) => return self.notify(reason, true),
            ChildResume::Allowed { native } => native,
        };
        let Some(parent) = node.parent.as_deref().and_then(|p| self.doc.node(p)).cloned() else { return };
        let description = self
            .summary(&node)
            .and_then(|s| s.agents.iter().find(|a| Some(&a.key) == node.agent_key.as_ref()))
            .map(|a| a.description.clone())
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| node.name.clone());
        let new_id = uuid::Uuid::new_v4().to_string();
        let (x, y) = self.doc.free_slot(parent.x + GRID_X + 4, parent.y, 1, NodeKind::Session);
        let cwd = parent.cwd.clone().filter(|c| !c.is_empty()).or_else(|| self.summary(&parent).map(|s| s.cwd.clone()).filter(|c| !c.is_empty()));
        self.doc.nodes.push(Node {
            id: new_id.clone(),
            kind: NodeKind::Session,
            name: crate::transcript::clip(&description, 60, true),
            x,
            y,
            cwd,
            parent: Some(parent.id.clone()),
            agent_key: None,
            source: Some("subagent".into()),
            command: None,
            fresh: false,
            automation: None,
            synced_name: None,
            reviewed_ms: None,
            mcp_off: parent.mcp_off.clone(),
            passive: None,
            no_messages: false,
            agent: Some(agent_of(&parent).id().to_owned()),
            agent_session: Some(native.clone()),
            // A resume is a use.
            last_used_ms: Some(crate::paths::now_ms()),
            stacked: false, model: None,
        });
        self.resumed_children.insert(new_id.clone());
        self.mark_dirty();
        self.notify(format!("Resuming “{}” on its own as a subsession of “{}” ({native}).", node.name, parent.name), false);
        self.start_node(&new_id);
    }

    /// The service refused to launch `id`: a just-created resumed child box
    /// goes again (the notice says why). True when it was one.
    pub fn launch_refused(&mut self, id: &str, message: &str) -> bool {
        if !self.resumed_children.remove(id) || self.terms.contains_key(id) {
            return false;
        }
        let name = self.doc.node(id).map(|n| n.name.clone()).unwrap_or_default();
        self.doc.remove_node(id);
        self.selection.retain(|s| s != id);
        self.node_errors.remove(id);
        self.mark_dirty();
        self.notify(format!("“{name}” was not resumed: {message}"), true);
        true
    }

    /// The inspector's buttons: resume (when allowed), Copy, Close.
    pub fn inspector_buttons(&self, inspector: &Inspector) -> Vec<(String, Option<Action>)> {
        let mut buttons = Vec::new();
        if let Some(node) = self.doc.node(&inspector.node) {
            match node.kind {
                NodeKind::Agent => match self.child_resume(node) {
                    ChildResume::Allowed { .. } => buttons.push(("Resume as subsession".to_owned(), Some(Action::ResumeChild(node.id.clone())))),
                    ChildResume::Existing(id) => buttons.push(("Open its subsession".to_owned(), Some(Action::Start(id)))),
                    ChildResume::No(_) => {}
                },
                _ if node.passive.is_some() => {}
                _ => {
                    if let Some(label) = self.start_plan(node).label() {
                        buttons.push((label.to_owned(), Some(Action::Start(node.id.clone()))));
                    }
                }
            }
        }
        buttons.push(("Copy".to_owned(), None));
        buttons.push(("Close".to_owned(), Some(Action::Nothing)));
        buttons
    }

    /// What the inspector says above the transcript: why a child cannot
    /// resume on its own, and the backend's note.
    pub fn inspector_note(&self, inspector: &Inspector) -> String {
        let mut notes = Vec::new();
        if let Some(node) = self.doc.node(&inspector.node).filter(|n| n.kind == NodeKind::Agent) {
            match self.child_resume(node) {
                ChildResume::No(reason) => notes.push(reason),
                ChildResume::Allowed { native } => notes.push(format!("Finished and not running anywhere: it can resume in a terminal of its own ({native}).")),
                ChildResume::Existing(_) => notes.push("Resumed before as its own subsession.".to_owned()),
            }
        }
        if let Some(note) = inspector.transcript.as_ref().map(|t| t.note.clone()).filter(|n| !n.is_empty()) {
            notes.push(note);
        }
        notes.join(" ")
    }

    /// A key in the inspector.
    pub fn inspector_key(&mut self, inspector: &mut Inspector, key: &KeyEvent) -> KeyOutcome {
        let page = inspector.rows.max(2) as i64 - 1;
        let buttons = self.inspector_buttons(inspector);
        let ctrl = key.modifiers.contains(Modifiers::CONTROL);
        let mut scroll = |delta: i64| {
            let max = inspector.total.saturating_sub(inspector.rows) as i64;
            let from = if inspector.follow { max } else { inspector.top as i64 };
            inspector.top = (from + delta).clamp(0, max.max(0)) as usize;
            inspector.follow = inspector.top as i64 >= max;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return KeyOutcome::Close,
            KeyCode::Char('c') if ctrl => self.copy_transcript(inspector),
            KeyCode::Up => scroll(-1),
            KeyCode::Down => scroll(1),
            KeyCode::PageUp => scroll(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => scroll(page),
            KeyCode::Home => {
                inspector.top = 0;
                inspector.follow = false;
            }
            KeyCode::End => inspector.follow = true,
            KeyCode::Tab => inspector.sel = (inspector.sel + 1) % buttons.len(),
            KeyCode::BackTab => inspector.sel = (inspector.sel + buttons.len() - 1) % buttons.len(),
            KeyCode::Enter => match buttons.get(inspector.sel.min(buttons.len() - 1)).map(|(_, a)| a.clone()) {
                Some(Some(Action::Nothing)) => return KeyOutcome::Close,
                Some(Some(action)) => return KeyOutcome::Run(action),
                _ => self.copy_transcript(inspector),
            },
            _ => {}
        }
        KeyOutcome::Keep
    }

    fn copy_transcript(&mut self, inspector: &Inspector) {
        let Some(text) = inspector.transcript.as_ref().map(Transcript::plain_text).filter(|t| !t.is_empty()) else {
            return self.notify("Nothing to copy yet.", false);
        };
        match self.platform.set_clipboard_text(&text) {
            Ok(()) => self.notify(format!("Copied the transcript ({} lines).", text.lines().count()), false),
            Err(error) => self.notify(format!("Copy failed: {error:#}"), true),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossbeam_channel::{Receiver, bounded, unbounded};

    use super::*;
    use crate::conn::Conn;
    use crate::doc::Doc;
    use crate::protocol::{AgentSummary, ClientMsg, SessionSummary, TermInfo};
    use crate::sync::Opened;
    use crate::transcript::Entry;

    #[test]
    fn only_opencode_children_resume_and_never_beside_a_writer() {
        use AgentKind::*;
        let finished = Some(AgentStatus::Finished);
        assert!(child_resume_rule(Copilot, finished, false, false, false).unwrap_err().contains("part of its parent's conversation"));
        assert!(child_resume_rule(Claude, finished, false, false, false).unwrap_err().contains("part of its parent's conversation"));
        assert_eq!(child_resume_rule(OpenCode, finished, false, false, false), Ok(()));
        assert!(child_resume_rule(OpenCode, Some(AgentStatus::Working), true, false, false).unwrap_err().contains("still running"));
        assert!(child_resume_rule(OpenCode, finished, true, false, false).unwrap_err().contains("stop the parent"));
        assert!(child_resume_rule(OpenCode, finished, false, true, false).unwrap_err().contains("second copy"));
        assert!(child_resume_rule(OpenCode, finished, false, false, true).unwrap_err().contains("second copy"));
        // Interrupted (its parent stopped mid-task): nothing writes to it any more.
        assert_eq!(child_resume_rule(OpenCode, Some(AgentStatus::Working), false, false, false), Ok(()));
    }

    fn fixture() -> (App, Receiver<ClientMsg>) {
        let parent: Node = serde_json::from_value(serde_json::json!({ "id": "p", "kind": "session", "name": "Parent", "x": 0, "y": 0, "cwd": ".", "agent": "opencode", "agent_session": "ses_p", "reviewed_ms": 1 })).unwrap();
        let child: Node = serde_json::from_value(serde_json::json!({ "id": "p:ses_c", "kind": "agent", "name": "explore", "x": 4, "y": 6, "parent": "p", "agent_key": "ses_c" })).unwrap();
        let (tx, sent) = bounded(256);
        let (_service, service_rx) = bounded(1);
        let (app_tx, _app_rx) = unbounded();
        let doc = Doc { nodes: vec![parent, child], ..Doc::default() };
        let mut app = App::new(Conn::detached(tx, service_rx), Arc::new(crate::rpc::Offline), Opened::detached(doc), false, ".".into(), app_tx, None);
        let agent = AgentSummary { key: "ses_c".into(), name: "explore".into(), description: "Find callers".into(), status: AgentStatus::Finished, finished_ms: Some(1) };
        app.sessions.insert("ses_p".into(), SessionSummary { id: "ses_p".into(), agents: vec![agent], tracked: true, ..Default::default() });
        (app, sent)
    }

    #[test]
    fn a_finished_child_resumes_as_a_new_linked_subsession_once() {
        let (mut app, sent) = fixture();
        assert_eq!(
            app.transcript_request("p:ses_c"),
            Some(Request::Transcript { agent: AgentKind::OpenCode, session: "ses_c".into(), parent: Some("ses_p".into()), agent_key: None, cwd: None, limit: LIMIT })
        );
        // While its parent runs here, it may be continued there.
        app.terms.insert("p".into(), TermInfo { id: "p".into(), alive: true, ..Default::default() });
        assert!(matches!(app.child_resume(app.doc.node("p:ses_c").unwrap()), ChildResume::No(r) if r.contains("stop the parent")));
        app.resume_child("p:ses_c");
        assert!(!sent.try_iter().any(|m| matches!(m, ClientMsg::Launch { .. })), "never beside its parent");
        app.terms.clear();
        app.start_node("p:ses_c");
        let launch = sent.try_iter().find_map(|m| if let ClientMsg::Launch { id, kind, .. } = m { Some((id, kind)) } else { None }).expect("a launch");
        let child = app.doc.node(&launch.0).cloned().expect("a new box");
        assert_eq!((child.kind, child.parent.as_deref(), child.agent_session.as_deref(), child.name.as_str()), (NodeKind::Session, Some("p"), Some("ses_c"), "Find callers"));
        match launch.1 {
            crate::protocol::LaunchKind::Agent(l) => assert!(l.resume && l.native_id.as_deref() == Some("ses_c") && l.agent == "opencode"),
            other => panic!("{other:?}"),
        }
        assert!(app.doc.node("p:ses_c").is_some(), "the subagent box stays");
        // A second resume goes to the same box.
        assert_eq!(app.child_resume(app.doc.node("p:ses_c").unwrap()), ChildResume::Existing(child.id.clone()));
        // The service refused it (open elsewhere): the new box goes, the notice says why.
        assert!(app.launch_refused(&child.id, "this session is already running in another terminal"));
        assert!(app.doc.node(&child.id).is_none() && app.notice.as_ref().is_some_and(|n| n.text.contains("was not resumed")));
        assert!(!app.launch_refused("p", "x"), "only resumed children");
    }

    #[test]
    fn transcript_lines_and_inspector_keys() {
        let transcript = Transcript {
            entries: vec![
                Entry { kind: EntryKind::User, text: "Fix it".into(), ..Default::default() },
                Entry { kind: EntryKind::Assistant, text: "Done.".into(), ..Default::default() },
                Entry { kind: EntryKind::Tool, tool: "bash".into(), args: "cargo test".into(), result: "exit 1".into(), failed: true, ..Default::default() },
                Entry { kind: EntryKind::Tool, tool: "grep".into(), running: true, ..Default::default() },
                Entry { kind: EntryKind::User, text: "Again".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        let shown = lines(&transcript, 40);
        let texts: Vec<&str> = shown.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts, ["❯ Fix it", "Done.", "⚙ bash cargo test", "  ✗ exit 1", "⚙ grep", "  … running", "", "❯ Again"]);
        assert_eq!(shown[3].1, Line::Failed);
        let (mut app, _sent) = fixture();
        app.inspect("p:ses_c");
        let Some(Modal::Inspector(mut inspector)) = app.modals.pop() else { panic!("inspector") };
        assert!(inspector.loading, "it reads at once");
        inspector.loading = false;
        inspector.transcript = Some(transcript);
        (inspector.rows, inspector.total) = (3, 8);
        let key = |code| KeyEvent::new(code, Modifiers::NONE);
        assert!(matches!(app.inspector_key(&mut inspector, &key(KeyCode::Up)), KeyOutcome::Keep));
        assert_eq!((inspector.top, inspector.follow), (4, false));
        app.inspector_key(&mut inspector, &key(KeyCode::End));
        assert!(inspector.follow);
        // Resume as subsession, Copy, Close.
        let labels: Vec<String> = app.inspector_buttons(&inspector).into_iter().map(|(l, _)| l).collect();
        assert_eq!(labels, ["Resume as subsession", "Copy", "Close"]);
        assert!(matches!(app.inspector_key(&mut inspector, &key(KeyCode::Enter)), KeyOutcome::Run(Action::ResumeChild(id)) if id == "p:ses_c"));
        inspector.sel = 2;
        assert!(matches!(app.inspector_key(&mut inspector, &key(KeyCode::Enter)), KeyOutcome::Close));
        assert!(matches!(app.inspector_key(&mut inspector, &key(KeyCode::Esc)), KeyOutcome::Close));
    }

}
