//! Modal dialog state. Input handling lives in `app.rs`; drawing in `ui.rs`.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::agents::AgentKind;
use crate::side::{DefaultField, SideView};
use crate::config::Kind;
use crate::git::GitInfo;
use crate::input::{KeyCode, KeyEvent, Modifiers};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextInput {
    pub text: String,
    /// Cursor position in characters.
    pub cursor: usize,
}

impl TextInput {
    pub fn new(text: &str) -> Self {
        Self { text: text.to_owned(), cursor: text.chars().count() }
    }

    fn byte_index(&self, chars: usize) -> usize {
        self.text.char_indices().nth(chars).map(|(i, _)| i).unwrap_or(self.text.len())
    }

    pub fn set(&mut self, text: &str) {
        self.text = text.to_owned();
        self.cursor = text.chars().count();
    }

    pub fn insert(&mut self, text: &str) {
        let clean: String = text.chars().map(|c| if c == '\n' || c == '\t' || c == '\r' { ' ' } else { c }).filter(|c| !c.is_control()).collect();
        let at = self.byte_index(self.cursor);
        self.text.insert_str(at, &clean);
        self.cursor += clean.chars().count();
        if self.text.len() > 4096 {
            self.text.truncate(self.text.char_indices().take_while(|(i, _)| *i < 4096).last().map(|(i, c)| i + c.len_utf8()).unwrap_or(0));
            self.cursor = self.cursor.min(self.text.chars().count());
        }
    }

    /// Applies an editing key. Returns true when the key was consumed.
    pub fn handle(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(Modifiers::CONTROL);
        let len = self.text.chars().count();
        match key.code {
            KeyCode::Char('u') if ctrl => {
                self.text.clear();
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let chars: Vec<char> = self.text.chars().collect();
                let mut start = self.cursor;
                while start > 0 && chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && chars[start - 1] != ' ' && chars[start - 1] != '\\' && chars[start - 1] != '/' {
                    start -= 1;
                }
                let (a, b) = (self.byte_index(start), self.byte_index(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor = start;
            }
            KeyCode::Char(c) if !ctrl || key.modifiers.contains(Modifiers::ALT) => self.insert(&c.to_string()),
            KeyCode::Backspace if self.cursor > 0 => {
                let (a, b) = (self.byte_index(self.cursor - 1), self.byte_index(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < len => {
                let (a, b) = (self.byte_index(self.cursor), self.byte_index(self.cursor + 1));
                self.text.replace_range(a..b, "");
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    /// Visible slice for a field `width` cells wide and the cursor's column in it.
    pub fn view(&self, width: usize) -> (String, usize) {
        let chars: Vec<char> = self.text.chars().collect();
        let width = width.max(1);
        let start = self.cursor.saturating_sub(width - 1);
        let visible: String = chars[start..].iter().take(width).collect();
        (visible, self.cursor - start)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Node(String),
    Group(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Open(String),
    OpenGroup(String),
    NewSession { at: Option<(i32, i32)>, parent: Option<String> },
    Rename(Target),
    GroupSelection,
    NewGroupAt(i32, i32),
    DeleteGroup(String),
    FitGroup(String),
    CycleGroupColor(String),
    RemoveNode(String),
    Stop { id: String, force: bool },
    Copy(String),
    ImportCopilot,
    SyncVs,
    UndoSync,
    Quit,
    CopySelection,
    Paste(String),
    Migrate,
    View(SideView),
    RemoveConfig(Kind, String),
    ToggleConfig(Kind, String, bool),
    UpdatePlugin(String),
    CloseDetail,
    ToggleAutomation(String),
    /// Start a manifest-managed automation now.
    RunAutomation(String),
    /// 0 manifest · 1 launcher log · 2 newest workload log · 3 status report.
    OpenAutomationFile(String, u8),
    /// Open a file or URL with its default program (outputs, report links).
    OpenTarget(String),
    /// Wake the group's doctor for this automation.
    AskDoctor(String),
    /// Show the Automations view (toolbar failure badge).
    ShowAutomations,
    OpenPath(String),
    SetGroupFolder(String),
    OpenGroupFolder(String, bool),
    GroupDefaults(String),
    FitAll,
    Zoom(i32),
    ToggleTiled,
    CloseTile(String),
    Detach,
    StopAllAndQuit,
    McpServers(String),
    /// Passive sessions and events.
    NewPassive(String),
    PassiveDetail(String),
    /// Open the passive session's conversation in a terminal.
    PassiveChat(String),
    PassiveRun(String),
    PassivePause(String),
    PassiveEdit(String, PassiveField),
    /// 0 rules, 1 memory, 2 log, 3 folder.
    PassiveOpen(String, u8),
    SendEvent(String),
    OpenEvents(String),
    MessagePassive(String),
    ToggleMessages(String),
    DiscardMail(String),
    UpdateService,
    RestartService,
    Help,
    Nothing,
}

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label: String,
    pub hint: String,
    pub action: Action,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct Menu {
    pub x: u16,
    pub y: u16,
    pub title: String,
    pub items: Vec<MenuItem>,
    pub sel: usize,
}

#[derive(Clone, Debug)]
pub struct Confirm {
    pub title: String,
    pub body: Vec<String>,
    pub buttons: Vec<(String, Action)>,
    pub sel: usize,
}

#[derive(Clone, Debug)]
pub struct Rename {
    pub target: Target,
    pub input: TextInput,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GitState {
    Unknown,
    Checking(u64),
    NotRepo,
    Repo(GitInfo),
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NsField {
    Name,
    Folder,
    Browse,
    Recent,
    Mode,
    Base,
    NewBranch,
    WtPath,
    Agent,
    Context,
    Mcp,
    Prompt,
    Create,
    Cancel,
}

#[derive(Clone, Debug)]
pub struct NewSession {
    pub parent: Option<String>,
    pub at: Option<(i32, i32)>,
    pub name: TextInput,
    pub folder: TextInput,
    pub prompt: TextInput,
    pub git: GitState,
    pub probed: String,
    pub folder_changed_at: Option<std::time::Instant>,
    pub worktree: bool,
    pub base: String,
    pub new_branch: TextInput,
    pub branch_edited: bool,
    pub wt_path: TextInput,
    pub path_edited: bool,
    pub focus: NsField,
    pub error: Option<String>,
    /// MCP servers the new session starts without.
    pub mcp_off: Vec<String>,
    /// A subsession of a session with history can fork its conversation.
    pub can_fork: bool,
    /// Start with the parent's conversation (like Copilot's `/fork`).
    pub fork: bool,
    /// The agent CLI the session runs.
    pub agent: AgentKind,
}

impl NewSession {
    pub fn fields(&self) -> Vec<NsField> {
        let mut fields = vec![NsField::Name, NsField::Folder, NsField::Browse, NsField::Recent];
        if matches!(self.git, GitState::Repo(_)) {
            fields.push(NsField::Mode);
            if self.worktree {
                fields.extend([NsField::Base, NsField::NewBranch, NsField::WtPath]);
            }
        }
        fields.push(NsField::Agent);
        if self.can_fork {
            fields.push(NsField::Context);
        }
        fields.extend([NsField::Mcp, NsField::Prompt, NsField::Create, NsField::Cancel]);
        fields
    }

    pub fn cycle(&mut self, forward: bool) {
        let fields = self.fields();
        let index = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward { (index + 1) % fields.len() } else { (index + fields.len() - 1) % fields.len() };
        self.focus = fields[next];
    }
}

#[derive(Clone, Debug)]
pub struct FolderBrowser {
    /// `None` lists drives.
    pub dir: Option<PathBuf>,
    pub entries: Vec<String>,
    pub filter: String,
    pub sel: usize,
    pub scroll: usize,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickPurpose {
    /// Template for a new passive session in this group.
    PassiveTemplate(String),
    RecentFolder,
    Branch,
    Default(DefaultField, Option<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptPurpose {
    AddSkill,
    AddMcp,
    InstallPlugin,
    CopilotSetting(String),
    NewCopilotSetting,
    Default(DefaultField, Option<String>),
    SyncWindow,
    AutomationsDir,
    GroupFolder(String),
    /// Save these "off" servers as a named MCP preset.
    McpPreset(Vec<String>),
    /// Name of a new passive session (group, template).
    PassiveName(String, usize),
    /// Edit a passive session setting (node id).
    Passive(String, PassiveField),
    /// `type [json]` to emit in a group.
    SendEvent(String),
    /// Message typed by you to a passive session (node id).
    MessagePassive(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PassiveField {
    Events,
    Schedule,
    Filter,
    Limits,
}

/// Whose MCP servers the picker edits.
#[derive(Clone, Debug, PartialEq)]
pub enum McpTarget {
    NewSession,
    Node(String),
}

/// Checklist of MCP servers with presets.
#[derive(Clone, Debug)]
pub struct McpPicker {
    pub target: McpTarget,
    /// Whose servers are listed (the session's agent).
    pub agent: AgentKind,
    pub off: Vec<String>,
    pub sel: usize,
}

#[derive(Clone, Debug)]
pub struct Prompt {
    pub title: String,
    pub hint: String,
    pub input: TextInput,
    pub purpose: PromptPurpose,
}

#[derive(Clone, Debug)]
pub struct Migrate {
    pub items: Option<Vec<crate::migrate::Item>>,
    pub chosen: Vec<bool>,
    pub sel: usize,
    pub applying: bool,
    pub result: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
pub struct Pick {
    pub purpose: PickPurpose,
    pub title: String,
    pub items: Vec<(String, String)>,
    pub filter: String,
    pub sel: usize,
    pub scroll: usize,
}

impl Pick {
    pub fn visible(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| needle.is_empty() || self.items[i].0.to_lowercase().contains(&needle) || self.items[i].1.to_lowercase().contains(&needle))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grouping {
    None,
    Folder,
    Client,
}

impl Grouping {
    pub fn label(self) -> &'static str {
        match self {
            Grouping::None => "no groups",
            Grouping::Folder => "group by folder",
            Grouping::Client => "group by agent / client",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Grouping::None => Grouping::Folder,
            Grouping::Folder => Grouping::Client,
            Grouping::Client => Grouping::None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ImportCopilot {
    pub filter: TextInput,
    pub sel: usize,
    pub scroll: usize,
    pub chosen: HashSet<String>,
    pub grouping: Grouping,
}

#[derive(Clone, Debug)]
pub enum Modal {
    Help,
    Menu(Menu),
    Confirm(Confirm),
    Rename(Rename),
    NewSession(Box<NewSession>),
    Folder(FolderBrowser),
    Pick(Pick),
    ImportCopilot(ImportCopilot),
    Prompt(Prompt),
    Migrate(Migrate),
    Mcp(McpPicker),
}

/// Clickable regions registered while drawing.
#[derive(Clone, Debug, PartialEq)]
pub enum Hit {
    MenuItem(usize),
    Button(usize),
    Field(NsField),
    ListRow(usize),
    Toggle(&'static str),
    Chip(usize),
    Toolbar(Action),
    TileHeader(String),
    TileClose(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, Modifiers::NONE)
    }

    #[test]
    fn text_input_edits_at_cursor_with_unicode() {
        let mut input = TextInput::new("héllo");
        input.handle(&press(KeyCode::Left));
        input.handle(&press(KeyCode::Backspace));
        assert_eq!(input.text, "hélo");
        input.handle(&press(KeyCode::Home));
        input.handle(&press(KeyCode::Char('¡')));
        assert_eq!(input.text, "¡hélo");
        input.handle(&KeyEvent::new(KeyCode::Char('w'), Modifiers::CONTROL));
        assert_eq!(input.text, "hélo");
        input.insert("a\nb");
        assert!(!input.text.contains('\n'));
        assert!(!input.handle(&press(KeyCode::Enter)));
    }

    #[test]
    fn text_view_keeps_cursor_visible() {
        let input = TextInput::new("abcdefghij");
        let (view, cursor) = input.view(5);
        assert_eq!(view, "ghij");
        assert_eq!(cursor, 4);
    }

    #[test]
    fn worktree_fields_appear_only_for_repositories() {
        let mut dialog = NewSession {
            parent: None,
            at: None,
            name: TextInput::default(),
            folder: TextInput::default(),
            prompt: TextInput::default(),
            git: GitState::NotRepo,
            probed: String::new(),
            folder_changed_at: None,
            worktree: true,
            base: String::new(),
            new_branch: TextInput::default(),
            branch_edited: false,
            wt_path: TextInput::default(),
            path_edited: false,
            focus: NsField::Name,
            error: None,
            mcp_off: Vec::new(),
            can_fork: false,
            fork: false,
            agent: AgentKind::OpenCode,
        };
        assert!(!dialog.fields().contains(&NsField::Base));
        assert!(dialog.fields().contains(&NsField::Agent), "every session chooses its agent");
        dialog.git = GitState::Repo(GitInfo { root: PathBuf::from("C:\\r"), branch: None, branches: vec![], local_count: 0 });
        assert!(dialog.fields().contains(&NsField::WtPath));
        // Worktree fields come before the agent (the Tab order selftests rely on).
        let fields = dialog.fields();
        assert!(fields.iter().position(|f| *f == NsField::WtPath) < fields.iter().position(|f| *f == NsField::Agent));
        dialog.focus = NsField::Cancel;
        dialog.cycle(true);
        assert_eq!(dialog.focus, NsField::Name);
    }
}
