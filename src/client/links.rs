//! Clickable links: detection in rendered text, and opening them safely.
//!
//! Terminal output is untrusted, so opening never executes anything: web and
//! mail links go to their registered handler, folders open in File Explorer,
//! files open in VS Code (or are revealed in Explorer).

use std::path::PathBuf;

use anyhow::{Context, Result};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

/// A link on screen.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    /// One rect per screen row (wrapped links span several).
    pub rects: Vec<Rect>,
    pub target: String,
    /// Inside a terminal pane (opened with Ctrl+click there).
    pub terminal: bool,
}

const SCHEMES: [&str; 4] = ["https://", "http://", "mailto:", "file://"];

fn url_char(c: char) -> bool {
    c.is_ascii_graphic() && !matches!(c, '<' | '>' | '"' | '\'' | '`' | '{' | '}' | '|' | '\\' | '^')
}

fn path_char(c: char) -> bool {
    // Box drawing, block elements and the truncation ellipsis end a path.
    !c.is_whitespace() && !matches!(c, '<' | '>' | '"' | '|' | '*' | '?' | '…') && !('\u{2500}'..='\u{259F}').contains(&c)
}

fn trim_end(chars: &[char], start: usize, mut end: usize) -> usize {
    while end > start {
        let c = chars[end - 1];
        let unbalanced = |open: char, close: char| {
            c == close && chars[start..end].iter().filter(|&&x| x == open).count() < chars[start..end].iter().filter(|&&x| x == close).count()
        };
        if matches!(c, '.' | ',' | ';' | ':' | '!' | '?') || unbalanced('(', ')') || unbalanced('[', ']') {
            end -= 1;
        } else {
            break;
        }
    }
    end
}

/// URLs and absolute Windows paths in a line: (start, end) char indices and target.
pub fn find(chars: &[char]) -> Vec<(usize, usize, String)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // Cheap reject: links start with h, m, f, a drive letter or a backslash.
        if !c.is_ascii_alphabetic() && c != '\\' {
            i += 1;
            continue;
        }
        let boundary = i == 0 || !chars[i - 1].is_alphanumeric();
        let starts = |s: &str| s.chars().enumerate().all(|(k, p)| chars.get(i + k).is_some_and(|c| c.to_ascii_lowercase() == p));
        if boundary && let Some(scheme) = SCHEMES.iter().find(|s| starts(s)) {
            let mut end = i + scheme.len();
            while end < chars.len() && url_char(chars[end]) {
                end += 1;
            }
            let end = trim_end(chars, i, end);
            if end > i + scheme.len() + 1 {
                found.push((i, end, chars[i..end].iter().collect()));
                i = end;
                continue;
            }
        }
        // C:\… (drive paths) and \\server\share paths.
        let drive = boundary && chars[i].is_ascii_alphabetic() && chars.get(i + 1) == Some(&':') && chars.get(i + 2) == Some(&'\\');
        let unc = boundary && chars[i] == '\\' && chars.get(i + 1) == Some(&'\\') && chars.get(i + 2).is_some_and(|c| c.is_alphanumeric());
        if drive || unc {
            let mut end = i + 2;
            while end < chars.len() && path_char(chars[end]) {
                end += 1;
            }
            let truncated = chars.get(end) == Some(&'…');
            let end = trim_end(chars, i, end);
            if !truncated && end > i + 3 {
                found.push((i, end, chars[i..end].iter().collect()));
                i = end;
                continue;
            }
        }
        i += 1;
    }
    found
}

impl Link {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.rects.iter().any(|r| x >= r.x && x < r.right() && y >= r.y && y < r.bottom())
    }
}

/// Finds links in a screen area of the rendered buffer and underlines them.
/// In terminals, a row that fills the full width continues on the next row
/// (soft wrap), so wrapped URLs and paths are found whole.
pub fn scan(buf: &mut Buffer, area: Rect, terminal: bool, skip: &[Rect], out: &mut Vec<Link>) {
    let area = area.intersection(buf.area);
    let mut y = area.y;
    let mut chars = Vec::with_capacity(area.width as usize);
    let mut cells: Vec<(u16, u16)> = Vec::with_capacity(area.width as usize);
    while y < area.bottom() {
        // One char per visible glyph, remembering its cell.
        chars.clear();
        cells.clear();
        loop {
            for x in area.x..area.right() {
                let symbol = buf[(x, y)].symbol();
                if symbol.is_empty() {
                    continue;
                }
                chars.push(symbol.chars().next().unwrap_or(' '));
                cells.push((x, y));
            }
            let full = area.width > 0 && !buf[(area.right() - 1, y)].symbol().trim().is_empty();
            y += 1;
            if !(terminal && full && y < area.bottom()) {
                break;
            }
        }
        for (start, end, target) in find(&chars) {
            let mut rects: Vec<Rect> = Vec::new();
            for &(x, row) in &cells[start..end] {
                match rects.last_mut() {
                    Some(r) if r.y == row => r.width = x + 1 - r.x,
                    _ => rects.push(Rect::new(x, row, 1, 1)),
                }
            }
            let taken = |r: &Rect| skip.iter().any(|s| s.intersects(*r)) || out.iter().any(|l| l.rects.iter().any(|o| o.intersects(*r)));
            if rects.iter().any(taken) {
                continue;
            }
            for &(x, row) in &cells[start..end] {
                buf[(x, row)].modifier.insert(Modifier::UNDERLINED);
            }
            out.push(Link { rects, target, terminal });
        }
    }
}

/// `path:line[:col]` → (path, line).
fn split_line(target: &str) -> (&str, Option<u32>) {
    let mut path = target;
    let mut line = None;
    for _ in 0..2 {
        if let Some((head, tail)) = path.rsplit_once(':')
            && head.len() > 2
            && !tail.is_empty()
            && tail.chars().all(|c| c.is_ascii_digit())
        {
            line = tail.parse().ok().or(line);
            path = head;
        }
    }
    (path, line)
}

fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.get(7..)?;
    let decoded = percent_encoding::percent_decode_str(rest).decode_utf8_lossy().into_owned();
    let local = decoded.strip_prefix('/').filter(|p| p.as_bytes().get(1) == Some(&b':')).map(str::to_owned).unwrap_or_else(|| format!("//{decoded}"));
    Some(PathBuf::from(local.replace('/', "\\")))
}

/// What a click opens: a URL for its handler, a folder, or a file (+ line).
#[derive(Debug, PartialEq)]
pub enum Target {
    Url(String),
    Folder(PathBuf),
    File(PathBuf, Option<u32>),
}

pub fn resolve(target: &str) -> Result<Target> {
    anyhow::ensure!(target.len() < 4096 && !target.chars().any(|c| c.is_control() || c == '"'), "unsupported link");
    let lower = target.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:") {
        return Ok(Target::Url(target.to_owned()));
    }
    let (raw, line) = split_line(target);
    let path = if lower.starts_with("file://") { file_url_to_path(raw).context("bad file link")? } else { PathBuf::from(raw) };
    anyhow::ensure!(path.is_absolute(), "only absolute paths can be opened");
    let meta = std::fs::metadata(&path).with_context(|| format!("{} does not exist", path.display()))?;
    Ok(if meta.is_dir() { Target::Folder(path) } else { Target::File(path, line) })
}

fn vscode_installed() -> bool {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        std::process::Command::new("reg.exe")
            .args(["query", r"HKCR\vscode\shell\open\command"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Hands a URL or path to its registered handler (never runs it as a program).
fn shell_open(target: &str) -> Result<()> {
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (verb, file) = (wide("open"), wide(target));
    // SAFETY: null-terminated UTF-16 strings that outlive the call.
    let result = unsafe {
        CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    };
    anyhow::ensure!(result as usize > 32, "Windows could not open {target}");
    Ok(())
}

/// Opens a link; returns a short description for the status bar.
pub fn open(target: &str) -> Result<String> {
    match resolve(target)? {
        Target::Url(url) => {
            spawn_open(url.clone());
            Ok(format!("Opened {url}"))
        }
        Target::Folder(path) => {
            std::process::Command::new("explorer.exe").arg(&path).spawn().context("cannot open File Explorer")?;
            Ok(format!("Opened {}", path.display()))
        }
        Target::File(path, line) if vscode_installed() => {
            let mut url = format!("vscode://file/{}", path.display().to_string().replace('\\', "/"));
            if let Some(line) = line {
                url.push_str(&format!(":{line}"));
            }
            spawn_open(url);
            Ok(format!("Opened {} in VS Code", path.display()))
        }
        Target::File(path, _) => {
            std::process::Command::new("explorer.exe").arg(format!("/select,{}", path.display())).spawn().context("cannot open File Explorer")?;
            Ok(format!("Revealed {}", path.display()))
        }
    }
}

fn spawn_open(target: String) {
    // ShellExecute may block on slow handlers; keep the UI responsive.
    std::thread::spawn(move || {
        let _ = shell_open(&target);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(line: &str) -> Vec<String> {
        find(&line.chars().collect::<Vec<_>>()).into_iter().map(|(_, _, t)| t).collect()
    }

    #[test]
    fn finds_urls_and_windows_paths_with_sensible_edges() {
        assert_eq!(targets("see https://github.com/org/repo/pull/12."), vec!["https://github.com/org/repo/pull/12"]);
        assert_eq!(targets("(https://en.wikipedia.org/wiki/Foo_(bar)) and"), vec!["https://en.wikipedia.org/wiki/Foo_(bar)"]);
        assert_eq!(targets("│ https://x.io/a│"), vec!["https://x.io/a"]);
        assert_eq!(targets(r"Edited q:\src\app\main.rs:42, then"), vec![r"q:\src\app\main.rs:42"]);
        assert_eq!(targets(r"at \\server\share\file.txt"), vec![r"\\server\share\file.txt"]);
        assert!(targets(r"C:\Users\THOMAS~1\…\work").is_empty(), "truncated paths are not links");
        assert!(targets("nohttp://x and https:// alone").is_empty());
        assert_eq!(targets("mailto:me@example.com"), vec!["mailto:me@example.com"]);
    }

    #[test]
    fn scan_maps_links_to_screen_cells_and_underlines_them() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 2));
        buf.set_string(2, 0, "go 中 https://a.io/x", ratatui::style::Style::default());
        buf.set_string(0, 1, "skip https://b.io", ratatui::style::Style::default());
        let mut out = Vec::new();
        scan(&mut buf, Rect::new(0, 0, 40, 2), false, &[Rect::new(0, 1, 40, 1)], &mut out);
        assert_eq!(out.len(), 1, "{out:?}");
        // "go " (3) + wide char (2 cells) + space → URL starts at column 2 + 6.
        assert_eq!((out[0].rects.clone(), out[0].target.as_str()), (vec![Rect::new(8, 0, 14, 1)], "https://a.io/x"));
        assert!(buf[(8, 0)].modifier.contains(Modifier::UNDERLINED));
        assert!(!buf[(7, 0)].modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn terminal_links_continue_across_soft_wrapped_rows() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 3));
        let style = ratatui::style::Style::default();
        buf.set_string(0, 0, "x https://", style);
        buf.set_string(0, 1, "a.io/long", style);
        buf.set_string(0, 2, "next", style);
        let mut out = Vec::new();
        scan(&mut buf, Rect::new(0, 0, 10, 3), true, &[], &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target, "https://a.io/long");
        assert_eq!(out[0].rects, vec![Rect::new(2, 0, 8, 1), Rect::new(0, 1, 9, 1)]);
        assert!(out[0].contains(0, 1) && !out[0].contains(0, 2));
        // Outside terminals rows are independent.
        let mut out = Vec::new();
        scan(&mut buf, Rect::new(0, 0, 10, 3), false, &[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn resolves_files_folders_and_rejects_unsafe_targets() {
        let dir = std::env::temp_dir().join(format!("cc-links-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a b.rs");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(resolve(&dir.display().to_string()).unwrap(), Target::Folder(dir.clone()));
        assert_eq!(resolve(&format!("{}:12:3", file.display())).unwrap(), Target::File(file.clone(), Some(12)));
        let url = format!("file:///{}", file.display().to_string().replace('\\', "/").replace(' ', "%20"));
        assert_eq!(resolve(&url).unwrap(), Target::File(file.clone(), None));
        assert_eq!(resolve("https://github.com").unwrap(), Target::Url("https://github.com".into()));
        assert!(resolve(r"relative\path.rs").is_err());
        assert!(resolve(r"C:\definitely\missing\x.rs").is_err());
        assert!(resolve("javascript:alert(1)").is_err());
        assert!(resolve("https://x.io/\"a").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
