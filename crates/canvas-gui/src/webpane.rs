//! The web pane's decisions, kept apart from WebView2 so they are plain
//! functions with tests: how an HTML note's file maps to a URL of the notes
//! origin and back (the origin is the note's explorer root, so relative
//! links resolve as on disk; only the note's own folder and the folders
//! below it are served), what a request is answered, what a clicked link
//! does, when the page's own URL may move the app, which keys the page hands
//! to the canvas, when a dialog covers the pane, and the browser process's
//! arguments. `webview.rs` only wires these to the view.

// macOS and Linux wire these to their own views (MAC-LINUX-PLAN D9).
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use canvas_core::input::{KeyCode, KeyEvent, Modifiers};
use canvas_core::report_html::notes::{self, ORIGIN};
use canvas_core::spaces::{NavTarget, Space};
use canvas_model::notebook::{self, Kind, LinkTarget};
use ratatui::layout::Rect;

/// Arguments for the views' browser process: no background traffic of its
/// own, and no network for anything that could get past
/// `WebResourceRequested`. Every request a page makes is answered there
/// first (the notes origin's files from the backend, everything else 403),
/// so the notes origin, a `.invalid` host, never needs DNS, the network or
/// this proxy. For whatever might pass by it: no DNS prefetching, a proxy
/// that answers nothing (port 9, discard) also for loopback addresses
/// (`<-loopback>` drops Chromium's implicit bypass: no local listener is
/// reachable either), WebRTC only through that proxy.
pub const BROWSER_ARGS: &str = "--disable-background-networking --disable-component-update --disable-sync --no-first-run --disable-default-apps --dns-prefetch-disable --proxy-server=127.0.0.1:9 --proxy-bypass-list=<-loopback> --force-webrtc-ip-handling-policy=disable_non_proxied_udp";

/// A path's names, split at either separator (empty ones left out).
fn parts(path: &str) -> Vec<&str> {
    path.split(['/', '\\']).filter(|p| !p.is_empty()).collect()
}

/// A Windows path (a drive letter or backslashes): names compare without
/// case and join with `\`.
fn windows_like(path: &str) -> bool {
    path.contains('\\') || path.as_bytes().get(1) == Some(&b':')
}

/// Two names are the same (without case for Windows paths).
fn same_name(a: &str, b: &str, win: bool) -> bool {
    if win { a.to_lowercase() == b.to_lowercase() } else { a == b }
}

/// An absolute folder (`C:\…`, `\\server\…`, `/…`) without `.` or `..` names.
pub fn root_ok(root: &str) -> bool {
    let b = root.as_bytes();
    let absolute = root.starts_with('/') || root.starts_with("\\\\") || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/'));
    absolute && !parts(root).iter().any(|p| matches!(*p, "." | ".."))
}

/// Windows device names: never files, whatever their extension.
const DEVICES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2", "LPT3",
    "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
];

/// A file or folder name a URL may name: not empty, `.` or `..`; no
/// separator, drive or stream colon, wildcard or control character; no
/// trailing dot or space (Windows drops them, so `.. ` would become `..`);
/// not a device name.
pub fn name_ok(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default().trim_end();
    !name.is_empty()
        && !name.ends_with(['.', ' '])
        && !name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        && !DEVICES.iter().any(|d| stem.eq_ignore_ascii_case(d))
}

/// Percent-encodes a name for a URL path (only `A-Z a-z 0-9 - . _ ~` stay).
fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A URL path segment decoded strictly (`None`: a broken escape or not UTF-8).
fn decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = |at: usize| bytes.get(at).and_then(|b| (*b as char).to_digit(16));
            let (high, low) = (hex(i + 1)?, hex(i + 2)?);
            out.push((high * 16 + low) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// What follows `https://notes.canvas.invalid/` in `uri` (scheme and host
/// compared without case; a port, user or other host is not the origin).
fn origin_rest(uri: &str) -> Option<&str> {
    let prefix = ORIGIN.len() + 1;
    (uri.len() >= prefix && uri.is_char_boundary(prefix) && uri[..prefix].eq_ignore_ascii_case(&format!("{ORIGIN}/"))).then(|| &uri[prefix..])
}

/// `uri` is on the notes origin.
pub fn is_origin(uri: &str) -> bool {
    origin_rest(uri).is_some() || uri.eq_ignore_ascii_case(ORIGIN)
}

/// The URL `path` (a file inside `root`) is served at (`None`: not inside
/// `root`, or a name a URL cannot carry safely).
pub fn url_for(root: &str, path: &str) -> Option<String> {
    if !root_ok(root) {
        return None;
    }
    let (base, all) = (parts(root), parts(path));
    if all.len() <= base.len() {
        return None;
    }
    let win = windows_like(root) || windows_like(path);
    if !base.iter().zip(&all).all(|(a, b)| same_name(a, b, win)) {
        return None;
    }
    let rest = &all[base.len()..];
    rest.iter().all(|name| name_ok(name)).then(|| format!("{ORIGIN}/{}", rest.iter().map(|n| encode(n)).collect::<Vec<_>>().join("/")))
}

/// The file inside `root` a URL of the notes origin names (query and
/// fragment ignored). `None`: another origin or scheme, an empty or broken
/// path, or a name that could leave `root` or name no plain file.
pub fn file_for(root: &str, uri: &str) -> Option<String> {
    if !root_ok(root) {
        return None;
    }
    let rest = origin_rest(uri)?;
    let path = rest.split(['?', '#']).next().unwrap_or_default();
    let names = path.split('/').map(decode).collect::<Option<Vec<String>>>()?;
    if names.is_empty() || !names.iter().all(|n| name_ok(n)) {
        return None;
    }
    let sep = if windows_like(root) { "\\" } else { "/" };
    let base = root.trim_end_matches(['\\', '/']);
    Some(format!("{base}{sep}{}", names.join(sep)))
}

/// The same file (either separator; without case for Windows paths).
pub fn same_file(a: &str, b: &str) -> bool {
    let (pa, pb) = (parts(a), parts(b));
    let win = windows_like(a) || windows_like(b);
    pa.len() == pb.len() && pa.iter().zip(&pb).all(|(x, y)| same_name(x, y, win))
}

/// `path` is in the folder `main` is in or in a folder below it.
pub fn in_folder_of(main: &str, path: &str) -> bool {
    let (main_parts, all) = (parts(main), parts(path));
    let Some((_, folder)) = main_parts.split_last() else { return false };
    let win = windows_like(main) || windows_like(path);
    all.len() > folder.len() && folder.iter().zip(&all).all(|(a, b)| same_name(a, b, win))
}

/// What a view serves while it shows an HTML note: the explorer root the
/// note is in (the origin's root: its URLs and links resolve against it, so
/// `../other.html` opens that note in the app), the note and whether its
/// scripts may run. Of the root only the note's own folder and the folders
/// below it are served ([`classify`]): pictures, styles and fonts next to
/// the note, never its siblings' or the rest of a mount.
#[derive(Clone, Debug, PartialEq)]
pub struct Serving {
    pub root: String,
    pub main: String,
    pub scripts: bool,
}

/// How a request of the view is answered.
#[derive(Clone, Debug, PartialEq)]
pub enum Resource {
    /// A file of the note's folder or one below it, read through the backend
    /// (then 200 or 404); `main`: the note itself.
    File { path: String, main: bool },
    /// 403: another origin or scheme, a path that would leave the root or is
    /// not in the note's folder (or below), not a GET, a document other than
    /// the note (frames), or no note served.
    Refused,
}

/// Classifies a request: `document` when it loads a document (the page or a frame).
pub fn classify(uri: &str, method: &str, document: bool, serving: Option<&Serving>) -> Resource {
    let Some(serving) = serving else { return Resource::Refused };
    if !method.eq_ignore_ascii_case("GET") {
        return Resource::Refused;
    }
    let Some(path) = file_for(&serving.root, uri) else { return Resource::Refused };
    let main = same_file(&path, &serving.main);
    if (document && !main) || !in_folder_of(&serving.main, &path) {
        return Resource::Refused;
    }
    Resource::File { path, main }
}

/// A response: status, reason, header lines (`Name: value` joined by CRLF), body.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub status: i32,
    pub reason: &'static str,
    pub headers: String,
    pub body: Vec<u8>,
}

/// The answer to everything refused (as reports have always had it).
pub fn refused() -> Reply {
    Reply { status: 403, reason: "Blocked by Copilot Canvas", headers: String::new(), body: Vec::new() }
}

/// Requests a window's views may wait on at once (their files being read):
/// beyond, [`busy`] at once, so a page cannot queue reads without end.
pub const MAX_PENDING: usize = 256;

/// One more request may wait for its file (`pending` already do).
pub fn admit(pending: usize) -> bool {
    pending < MAX_PENDING
}

/// The answer to a request beyond [`MAX_PENDING`].
pub fn busy() -> Reply {
    Reply { status: 503, reason: "Busy", headers: "Cache-Control: no-store".into(), body: Vec::new() }
}

/// The answer for a file of the note's folder once it was read (or not):
/// 200 with its content type, the note's policy, `nosniff` and no caching;
/// the note itself is sanitized ([`notes::page`]) unless its scripts may
/// run (then it is served as it is, the policy's sandbox still applies).
/// A missing or unreadable file is 404 (for the note, a page that says why).
pub fn reply(path: &str, main: bool, scripts: bool, read: Result<Vec<u8>, String>) -> Reply {
    let read = read.and_then(|bytes| if bytes.len() as u64 > notebook::MAX_BYTES { Err("the file is larger than 16 MiB".into()) } else { Ok(bytes) });
    let (status, reason, content_type, body) = match read {
        Ok(bytes) if main && !scripts && Kind::of(path) == Kind::Html => (200, "OK", "text/html; charset=utf-8", notes::page(&String::from_utf8_lossy(&bytes)).into_bytes()),
        Ok(bytes) => (200, "OK", notes::content_type(path), bytes),
        Err(why) if main => (404, "Not Found", "text/html; charset=utf-8", notes::message_page(&format!("{path} could not be read: {why}")).into_bytes()),
        Err(_) => (404, "Not Found", "text/plain; charset=utf-8", Vec::new()),
    };
    let mut headers = format!(
        "Content-Type: {content_type}\r\nContent-Security-Policy: {}\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-DNS-Prefetch-Control: off",
        notes::csp(scripts)
    );
    if scripts {
        // The sandboxed page has an opaque origin: its fonts and module scripts are cross-origin loads.
        headers.push_str("\r\nAccess-Control-Allow-Origin: *");
    }
    Reply { status, reason, headers, body }
}

/// The navigation the canvas itself starts next in a view (allowed once).
#[derive(Clone, Debug, PartialEq)]
pub enum Expect {
    /// A page given as a string (`NavigateToString`, the plain view only).
    Page,
    /// `about:blank` (the scripts view unloading its page: its scripts may
    /// still run then, so nothing else counts as this load).
    Blank,
    /// An HTML note (its file) on the notes origin.
    File(String),
}

/// `uri` is the navigation the view expects (`root`: the folder it serves).
pub fn expected(expect: &Expect, uri: &str, root: Option<&str>) -> bool {
    match expect {
        Expect::Page => uri.starts_with("data:") || uri.starts_with("about:"),
        Expect::Blank => uri == "about:blank",
        Expect::File(main) => root.and_then(|root| file_for(root, uri)).is_some_and(|file| same_file(&file, main)),
    }
}

/// What a link of the page does.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkAction {
    /// A place in the app: `#note:Name` (the note's name), `#todo:ID`, or
    /// another file under the note's root (its absolute path).
    App(Space, NavTarget),
    /// An http(s) or mailto link: the canvas asks before the browser opens it.
    Browser(String),
    /// Anything else (refused, nothing happens).
    Ignore,
}

/// The app place a `#note:` / `#todo:` fragment of the page's own URL
/// (`about:`, `data:` or the notes origin) names.
pub fn fragment_target(uri: &str) -> Option<(Space, NavTarget)> {
    let own = uri.starts_with("about:") || uri.starts_with("data:") || is_origin(uri);
    let (_, fragment) = uri.split_once('#').filter(|_| own)?;
    Some(match notebook::link_target(&format!("#{fragment}"))? {
        LinkTarget::Note(name) => (Space::Notebook, NavTarget::Note(name)),
        LinkTarget::Todo(id) => (Space::Tasks, NavTarget::Todo(id)),
    })
}

/// What a navigation the page asked for does (`root`: the explorer root of
/// the HTML note shown, if one is).
pub fn link_action(uri: &str, root: Option<&str>) -> LinkAction {
    if let Some((space, target)) = fragment_target(uri) {
        return LinkAction::App(space, target);
    }
    if is_origin(uri) {
        return match root.and_then(|root| file_for(root, uri)) {
            Some(path) => LinkAction::App(Space::Notebook, NavTarget::Note(path)),
            None => LinkAction::Ignore,
        };
    }
    let lower = uri.to_ascii_lowercase();
    if (lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")) && canvas_core::reports_model::check_url(uri).is_ok() {
        return LinkAction::Browser(uri.to_owned());
    }
    LinkAction::Ignore
}

/// `uri` only moves to a fragment of the document at `current` (an in-page
/// anchor: the page scrolls, nothing loads).
pub fn same_document(current: &str, uri: &str) -> bool {
    let base = |u: &str| u.split_once('#').map_or(u, |(b, _)| b).to_owned();
    !current.is_empty() && uri.contains('#') && base(current) == base(uri)
}

/// A same-document change of the page's URL to an app link is followed:
/// always in a view without scripts (only a click changes it); in one with
/// scripts only while it is shown and has the keyboard (the user clicked
/// into it), at most once a second, so a script cannot drive the app.
pub fn fragment_allowed(scripts: bool, visible: bool, focused: bool, since_last: Option<Duration>) -> bool {
    visible && (!scripts || (focused && since_last.is_none_or(|d| d >= Duration::from_secs(1))))
}

/// A view's app links that come from its own URL's fragment (`SourceChanged`):
/// whether it has the keyboard and when it last followed one.
#[derive(Clone, Debug, Default)]
pub struct AppLinks {
    focused: bool,
    last: Option<Instant>,
}

impl AppLinks {
    /// The view got (`true`) or lost the keyboard.
    pub fn focus(&mut self, got: bool) {
        self.focused = got;
    }

    pub fn focused(&self) -> bool {
        self.focused
    }

    /// Whether a fragment change at `now` is followed ([`fragment_allowed`]).
    /// Following one costs the view the keyboard (the window gives it back
    /// to the canvas), so in the scripts view a script gets at most one app
    /// link per click into its page, never a stream of them (a note that
    /// links to itself would otherwise reload and follow it again each second).
    pub fn follow(&mut self, scripts: bool, visible: bool, now: Instant) -> bool {
        let allowed = fragment_allowed(scripts, visible, self.focused, self.last.map(|at| now.saturating_duration_since(at)));
        if allowed {
            (self.last, self.focused) = (Some(now), false);
        }
        allowed
    }
}

/// Virtual keys the page hands over.
pub const VK_TAB: u32 = 0x09;
pub const VK_ESCAPE: u32 = 0x1B;
pub const VK_LEFT: u32 = 0x25;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_F6: u32 = 0x75;

/// What a key pressed in the page does.
#[derive(Clone, Debug, PartialEq)]
pub enum WebKey {
    /// Esc: a report closes; from a notebook pane the keyboard goes back to the canvas.
    Escape,
    /// A global key for the canvas (the page does not get it).
    Forward(KeyEvent),
}

/// The keys the page hands to the canvas: Esc, F6, Ctrl+], Ctrl+1/2/3,
/// Ctrl+P, Alt+←/→ and Alt+, / Alt+. (back, forward), Ctrl+E, Ctrl+W,
/// Ctrl+Tab, Ctrl+Shift+Tab, Ctrl+N, Ctrl+Shift+N, Ctrl+S, Ctrl+F, Alt+N.
/// Everything else stays in the page (scrolling, selection, Ctrl+C). `vk`
/// is the virtual key, `ch` the character the keyboard layout gives it
/// unmodified (for `]`, `,` and `.`). Ctrl with Alt is AltGr (typing),
/// never one of these.
pub fn web_key(vk: u32, ch: Option<char>, ctrl: bool, shift: bool, alt: bool) -> Option<WebKey> {
    let mut mods = Modifiers::NONE;
    if ctrl {
        mods |= Modifiers::CONTROL;
    }
    if shift {
        mods |= Modifiers::SHIFT;
    }
    if alt {
        mods |= Modifiers::ALT;
    }
    let forward = |code| Some(WebKey::Forward(KeyEvent::new(code, mods)));
    let letter = (0x41..=0x5A).contains(&vk).then(|| (vk as u8 as char).to_ascii_lowercase());
    let digit = (0x31..=0x33).contains(&vk).then_some(vk as u8 as char);
    let ctrl_only = ctrl && !alt;
    match vk {
        VK_ESCAPE => Some(WebKey::Escape),
        VK_F6 if !alt => forward(KeyCode::F(6)),
        VK_TAB if ctrl_only => forward(if shift { KeyCode::BackTab } else { KeyCode::Tab }),
        VK_LEFT if alt && !ctrl && !shift => forward(KeyCode::Left),
        VK_RIGHT if alt && !ctrl && !shift => forward(KeyCode::Right),
        _ if alt && !ctrl && !shift && letter.is_none() && matches!(ch, Some(',' | '.')) => forward(KeyCode::Char(ch?)),
        _ if ctrl_only && !shift && digit.is_some() => forward(KeyCode::Char(digit?)),
        _ if ctrl_only && !shift && letter.is_none() && ch == Some(']') => forward(KeyCode::Char(']')),
        _ => match letter? {
            c @ ('p' | 'e' | 'w' | 's' | 'f') if ctrl_only && !shift => forward(KeyCode::Char(c)),
            'n' if ctrl_only => forward(KeyCode::Char(if shift { 'N' } else { 'n' })),
            'n' if alt && !ctrl && !shift => forward(KeyCode::Char('n')),
            _ => None,
        },
    }
}

/// The pane may show: no dialog or menu overlaps it, or the top one holds
/// it whole (a report's own dialog: the core publishes a pane only for the
/// top dialog or when none is open).
pub fn pane_clear(pane: Rect, modal: Option<Rect>) -> bool {
    let Some(m) = modal else { return true };
    let overlaps = pane.x < m.right() && m.x < pane.right() && pane.y < m.bottom() && m.y < pane.bottom();
    let holds = m.x <= pane.x && m.y <= pane.y && pane.right() <= m.right() && pane.bottom() <= m.bottom();
    !overlaps || holds
}

/// Where *Print / PDF* saves the page when the WebView2 runtime has no print
/// dialog: `<dir>/<title>.pdf`, else `-2`, `-3`… Each name is claimed with
/// `claim` (it creates the file only when there is none: `AlreadyExists`
/// tries the next), so no file is ever overwritten, not even one made
/// between a look and the write. `Err`: another error, or 1,000 names taken.
pub fn pdf_path(dir: &Path, title: &str, mut claim: impl FnMut(&Path) -> std::io::Result<()>) -> Result<PathBuf, String> {
    let stem = notebook::safe_name(title, "note");
    for n in 1..=1000 {
        let path = dir.join(if n == 1 { format!("{stem}.pdf") } else { format!("{stem}-{n}.pdf") });
        match claim(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("{} cannot be created: {error}", path.display())),
        }
    }
    Err(format!("{} has no free name for {stem}.pdf", dir.display()))
}

/// Creates `path` only when nothing is there yet (`AlreadyExists` else).
pub fn claim_new(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new().write(true).create_new(true).open(path).map(drop)
}

#[cfg(test)]
mod tests;
