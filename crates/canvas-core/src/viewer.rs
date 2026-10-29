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
    /// A folder listed with its files: entries (name, folder) to open.
    pub entries: Vec<(String, bool)>,
    /// The highlighted entry (browsing).
    pub pick: usize,
    /// Listed with its files (the entries open).
    pub listed: bool,
    /// The keyboard is on the buttons (Tab) rather than the entries.
    pub on_buttons: bool,
}

/// What a key did.
#[derive(Debug, PartialEq)]
pub enum ViewerKey {
    Keep,
    Close,
    /// Run this button.
    Button(&'static str),
    /// Open this entry of the folder (browsing).
    Open(usize),
    /// Up to the parent folder (browsing).
    Up,
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

    /// A folder's subfolders (as an older service lists them).
    pub fn set_entries(&mut self, entries: Vec<String>) {
        self.set_listing(entries, false);
    }

    /// A folder's entries: subfolders only, or (`typed`) `name\` subfolders
    /// and files (the browser: entries open with Enter or a click).
    pub fn set_listing(&mut self, entries: Vec<String>, typed: bool) {
        let mut lines = Vec::new();
        self.entries.clear();
        self.listed = typed;
        for entry in entries.into_iter().filter(|e| e != "..") {
            let (name, dir) = match entry.strip_suffix(['\\', '/']) {
                Some(name) => (name.to_owned(), true),
                None => (entry, !typed),
            };
            lines.push(if dir { format!("▸ {name}") } else { format!("  {name}") });
            if typed {
                self.entries.push((name, dir));
            }
        }
        if lines.is_empty() {
            lines.push(if typed { "(empty folder)".into() } else { "(no subfolders)".into() });
        }
        self.note = Some(if typed {
            "A folder on the service's machine: Enter or a click opens an entry (files read-only), Backspace goes up.".into()
        } else {
            "A folder on the service's machine (subfolders only). Copy its path, or open it in VS Code through a tunnel.".into()
        });
        self.pick = self.pick.min(self.entries.len().saturating_sub(1));
        self.lines = Some(lines);
    }

    /// Browsing a folder with its files (entries open).
    pub fn browsing(&self) -> bool {
        self.folder && self.listed
    }

    /// The path of entry `index` (or of the parent folder for `None`).
    pub fn entry_path(&self, index: Option<usize>) -> Option<(String, bool)> {
        let base = self.path.trim_end_matches(['\\', '/']);
        match index {
            Some(i) => self.entries.get(i).map(|(name, dir)| (format!("{base}\\{name}"), *dir)),
            None => {
                let (parent, _) = base.rsplit_once(['\\', '/'])?;
                let parent = if parent.ends_with(':') { format!("{parent}\\") } else { parent.to_owned() };
                Some((parent, true))
            }
        }
    }

    /// Shows another folder (its entries load next).
    pub fn navigate(&mut self, path: String) {
        *self = Viewer { path, folder: true, vscode: None, rows: self.rows, ..Default::default() };
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
        if self.browsing() {
            // Browsing: the arrows pick an entry, Enter opens it, Tab goes to the buttons.
            let len = self.entries.len();
            let pick = |delta: i64, viewer: &mut Self| {
                viewer.on_buttons = false;
                if len > 0 {
                    viewer.pick = (viewer.pick as i64 + delta).clamp(0, len as i64 - 1) as usize;
                    let rows = viewer.rows.max(1);
                    if viewer.pick < viewer.top {
                        viewer.top = viewer.pick;
                    } else if viewer.pick >= viewer.top + rows {
                        viewer.top = viewer.pick + 1 - rows;
                    }
                }
            };
            match key.code {
                KeyCode::Up => {
                    pick(-1, self);
                    return ViewerKey::Keep;
                }
                KeyCode::Down => {
                    pick(1, self);
                    return ViewerKey::Keep;
                }
                KeyCode::PageUp => {
                    pick(-page, self);
                    return ViewerKey::Keep;
                }
                KeyCode::PageDown => {
                    pick(page, self);
                    return ViewerKey::Keep;
                }
                KeyCode::Backspace => return ViewerKey::Up,
                KeyCode::Enter if !self.on_buttons && len > 0 => return ViewerKey::Open(self.pick),
                KeyCode::Tab | KeyCode::BackTab if !self.on_buttons => {
                    self.on_buttons = true;
                    return ViewerKey::Keep;
                }
                _ => {}
            }
        }
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
