//! A read-only text viewer for files and folders on a remote service's
//! machine (PLAN.md §6): terminal links, automation files and passive-session
//! files that cannot be opened on this machine. Scrollable, monospace (the
//! cell grid), Esc closes; "Open in VS Code" when the server has a VS Code
//! tunnel, "Copy path" always. Nothing is executed. Drawn by `ui_modals`.

use crate::input::{KeyCode, KeyEvent, Modifiers};

/// Largest file shown (from its start).
pub const MAX_VIEW_BYTES: u64 = 2 << 20;
/// Columns a tab advances to.
const TAB: usize = 4;

pub const OPEN_IN_VSCODE: &str = "Open in VS Code";
pub const COPY_PATH: &str = "Copy path";
pub const CLOSE: &str = "Close";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Viewer {
    /// Absolute path on the service's machine.
    pub path: String,
    pub folder: bool,
    /// Text lines, or a folder's entries; `None` while loading.
    pub lines: Option<Vec<String>>,
    /// Shown above the text (e.g. that only the start of a large file is shown).
    pub note: Option<String>,
    pub error: Option<String>,
    /// First visible line and column.
    pub top: usize,
    pub left: usize,
    /// The line a link pointed at (1-based), shown near the top once loaded.
    pub line: Option<u32>,
    /// The file or folder in VS Code through its tunnel.
    pub vscode: Option<String>,
    /// Focused button.
    pub sel: usize,
    /// Text rows at the last draw (for paging).
    pub rows: usize,
}

/// What a key did.
#[derive(Debug, PartialEq)]
pub enum ViewerKey {
    Keep,
    Close,
    /// Run this button.
    Button(&'static str),
}

fn printable(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '\t' => {
                let pad = TAB - out.chars().count() % TAB;
                out.extend(std::iter::repeat_n(' ', pad));
            }
            c if c.is_control() => out.push('·'),
            c => out.push(c),
        }
    }
    out
}

impl Viewer {
    pub fn new(path: String, folder: bool, line: Option<u32>, vscode: Option<String>) -> Self {
        Self { path, folder, line, vscode, rows: 20, ..Default::default() }
    }

    pub fn buttons(&self) -> Vec<&'static str> {
        let mut buttons = Vec::with_capacity(3);
        if self.vscode.is_some() {
            buttons.push(OPEN_IN_VSCODE);
        }
        buttons.extend([COPY_PATH, CLOSE]);
        buttons
    }

    /// The file's text (`truncated`: only its first `MAX_VIEW_BYTES`).
    pub fn set_text(&mut self, text: &str, truncated: bool) {
        let lines: Vec<String> = text.lines().map(printable).collect();
        self.note = truncated.then(|| format!("Only the first {} MB are shown.", MAX_VIEW_BYTES >> 20));
        if let Some(line) = self.line.filter(|l| *l > 0) {
            self.top = (line as usize - 1).saturating_sub(3).min(lines.len().saturating_sub(1));
        }
        self.lines = Some(lines);
    }

    /// A folder's subfolders (as the backend lists them).
    pub fn set_entries(&mut self, entries: Vec<String>) {
        let mut lines: Vec<String> = entries.into_iter().filter(|e| e != "..").map(|e| format!("▸ {e}")).collect();
        if lines.is_empty() {
            lines.push("(no subfolders)".into());
        }
        self.note = Some("A folder on the service's machine (subfolders only). Copy its path, or open it in VS Code through a tunnel.".into());
        self.lines = Some(lines);
    }

    fn len(&self) -> usize {
        self.lines.as_ref().map_or(0, Vec::len)
    }

    pub fn scroll(&mut self, delta: i64) {
        let max = self.len().saturating_sub(self.rows.max(1)) as i64;
        self.top = (self.top as i64 + delta).clamp(0, max.max(0)) as usize;
    }

    /// Handles a key.
    pub fn key(&mut self, key: &KeyEvent) -> ViewerKey {
        let page = self.rows.max(2) as i64 - 1;
        let buttons = self.buttons();
        let ctrl = key.modifiers.contains(Modifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return ViewerKey::Close,
            KeyCode::Char('c') if ctrl => return ViewerKey::Button(COPY_PATH),
            KeyCode::Enter => return ViewerKey::Button(buttons[self.sel.min(buttons.len() - 1)]),
            KeyCode::Tab => self.sel = (self.sel + 1) % buttons.len(),
            KeyCode::BackTab => self.sel = (self.sel + buttons.len() - 1) % buttons.len(),
            KeyCode::Up => self.scroll(-1),
            KeyCode::Down => self.scroll(1),
            KeyCode::PageUp => self.scroll(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll(page),
            KeyCode::Home if ctrl => self.top = 0,
            KeyCode::End if ctrl => self.scroll(i64::MAX / 2),
            KeyCode::Home => self.left = 0,
            KeyCode::Left => self.left = self.left.saturating_sub(8),
            KeyCode::Right => self.left += 8,
            _ => {}
        }
        ViewerKey::Keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, Modifiers::NONE)
    }

    #[test]
    fn text_is_made_printable_and_scrolls_within_bounds() {
        let mut viewer = Viewer::new(r"C:\x\log.txt".into(), false, Some(40), None);
        let text: String = (1..=100).map(|n| format!("line {n}\tend\x1b\n")).collect();
        viewer.set_text(&text, true);
        let lines = viewer.lines.as_ref().unwrap();
        assert_eq!(lines.len(), 100);
        assert_eq!(lines[0], "line 1  end·", "tabs expand to the next stop, controls are shown, not run");
        assert_eq!(viewer.top, 36, "a linked line shows near the top");
        assert!(viewer.note.as_deref().unwrap().contains("first 2 MB"));
        viewer.rows = 10;
        viewer.key(&press(KeyCode::PageDown));
        assert_eq!(viewer.top, 45);
        viewer.key(&KeyEvent::new(KeyCode::End, Modifiers::CONTROL));
        assert_eq!(viewer.top, 90, "the last page");
        viewer.key(&press(KeyCode::Down));
        assert_eq!(viewer.top, 90);
        viewer.key(&KeyEvent::new(KeyCode::Home, Modifiers::CONTROL));
        assert_eq!(viewer.top, 0);
        viewer.key(&press(KeyCode::Up));
        assert_eq!(viewer.top, 0);
    }

    #[test]
    fn buttons_depend_on_the_vscode_tunnel_and_keys_choose_them() {
        let mut viewer = Viewer::new(r"C:\x".into(), true, None, None);
        assert_eq!(viewer.buttons(), [COPY_PATH, CLOSE]);
        assert_eq!(viewer.key(&press(KeyCode::Enter)), ViewerKey::Button(COPY_PATH));
        viewer.key(&press(KeyCode::Tab));
        assert_eq!(viewer.key(&press(KeyCode::Enter)), ViewerKey::Button(CLOSE));
        assert_eq!(viewer.key(&press(KeyCode::Esc)), ViewerKey::Close);
        assert_eq!(viewer.key(&KeyEvent::new(KeyCode::Char('c'), Modifiers::CONTROL)), ViewerKey::Button(COPY_PATH));
        viewer.set_entries(vec!["..".into(), "src".into()]);
        assert_eq!(viewer.lines.as_deref(), Some(&["▸ src".to_owned()][..]));
        let tunnel = Viewer::new(r"C:\x".into(), false, None, Some("vscode://vscode-remote/tunnel+t/C:/x".into()));
        assert_eq!(tunnel.buttons(), [OPEN_IN_VSCODE, COPY_PATH, CLOSE]);
    }
}
