//! A todo's details in the right pane (`RightDetail::Todo`, or the
//! selected todo when nothing else takes the pane): its fields with
//! pickers, the actions, proposal, checklist, body, links, relations and
//! log. With nothing selected the pane shows the selected board and the keys.

use canvas_model::todos::{LinkKind, LogKind, Status, Todo, TodoBook, TodoOp, TodoUpdate};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::book::{self, book, type_color, type_glyph};
use super::{Action, Ask, Hit};
use crate::app::App;
use crate::dialogs::Hit as AppHit;
use crate::input::MouseEventKind;
use crate::paint::{ACCENT, ATTENTION, BLUE, FAINT, GRAY, LIGHT, MUTED, NEGATIVE, PANEL_HI, PURPLE, TEXT, bold, fg, outline, put, text_width, truncate};

/// A piece of a line: text, style and maybe what a click does.
struct Seg {
    text: String,
    style: Style,
    hit: Option<Hit>,
}

fn seg(text: impl Into<String>, style: Style) -> Seg {
    Seg { text: text.into(), style, hit: None }
}

fn link(text: impl Into<String>, style: Style, hit: Hit) -> Seg {
    Seg { text: text.into(), style, hit: Some(hit) }
}

fn act(action: Action) -> Hit {
    Hit::Do(action)
}

fn op(op: TodoOp) -> Hit {
    Hit::Do(Action::Op(Box::new(op)))
}

/// The lines of a panel before it is drawn.
struct Out {
    rows: Vec<Vec<Seg>>,
    width: usize,
}

impl Out {
    fn line(&mut self, segs: Vec<Seg>) {
        self.rows.push(segs);
    }

    fn blank(&mut self) {
        self.rows.push(Vec::new());
    }

    /// Words wrapped to the width, each line clickable when `hit` is given.
    fn wrap(&mut self, text: &str, style: Style, hit: Option<Hit>) {
        for line in text.lines() {
            for part in super::canvas::wrap(line, self.width, usize::MAX) {
                self.rows.push(vec![Seg { text: part, style, hit: hit.clone() }]);
            }
            if line.trim().is_empty() {
                self.rows.push(Vec::new());
            }
        }
    }

    /// A section's heading, with a button at its end.
    fn head(&mut self, text: &str, button: Option<(&str, Hit)>) {
        self.blank();
        let mut segs = vec![seg(text, bold(GRAY))];
        if let Some((label, hit)) = button {
            segs.push(seg("  ", fg(TEXT)));
            segs.push(link(format!(" {label} "), fg(ACCENT).bg(PANEL_HI), hit));
        }
        self.line(segs);
    }

    /// `label` in a fixed column, then the value pieces.
    fn field(&mut self, label: &str, mut value: Vec<Seg>) {
        let mut segs = vec![seg(format!("{label:<9}"), fg(MUTED))];
        segs.append(&mut value);
        self.line(segs);
    }

    /// Buttons flowing over as many lines as they need.
    fn buttons(&mut self, buttons: Vec<(String, Hit, bool)>) {
        let mut row: Vec<Seg> = Vec::new();
        let mut used = 0;
        for (label, hit, primary) in buttons {
            let text = format!(" {label} ");
            let w = text_width(&text) + 1;
            if used > 0 && used + w > self.width {
                self.rows.push(std::mem::take(&mut row));
                used = 0;
            }
            let style = if primary { bold(ratatui::style::Color::Black).bg(ACCENT) } else { fg(TEXT).bg(PANEL_HI) };
            row.push(link(text, style, hit));
            row.push(seg(" ", fg(TEXT)));
            used += w;
        }
        if !row.is_empty() {
            self.rows.push(row);
        }
    }
}

/// `2026-10-26 09:00` (UTC).
fn date(ms: u64) -> String {
    let text = crate::paths::format_rfc3339_ms(ms);
    format!("{} {}", &text[..10], &text[11..16])
}

/// Draws the lines from the scroll offset and registers their clicks.
fn paint(app: &mut App, buf: &mut Buffer, area: Rect, out: Out) {
    outline(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, &LIGHT, fg(FAINT), area);
    let inner = Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), area.height.saturating_sub(2));
    let max = out.rows.len().saturating_sub(inner.height as usize);
    let scroll = (app.tasks.detail_scroll as usize).min(max);
    app.tasks.detail_scroll = scroll as u16;
    for (i, row) in out.rows.iter().skip(scroll).take(inner.height as usize).enumerate() {
        let y = inner.y as i32 + i as i32;
        let mut x = inner.x as i32;
        for piece in row {
            let room = (inner.right() as i32 - x).max(0) as usize;
            let text = truncate(&piece.text, room);
            let end = put(buf, x, y, &text, piece.style, inner);
            if let Some(hit) = &piece.hit
                && end > x
            {
                app.layout.hits.push((Rect::new(x as u16, y as u16, (end - x) as u16, 1), AppHit::Tasks(hit.clone())));
            }
            x = end;
        }
    }
    if max > 0 {
        let more = if scroll < max { format!(" ↓ {}/{} ", scroll + 1, max + 1) } else { format!(" ↑ {}/{} ", scroll + 1, max + 1) };
        put(buf, area.right() as i32 - 2 - text_width(&more) as i32, area.bottom() as i32 - 1, &more, fg(MUTED), area);
    }
}

pub fn wheel(app: &mut App, kind: MouseEventKind) {
    match kind {
        MouseEventKind::ScrollUp => scroll(app, -3),
        MouseEventKind::ScrollDown => scroll(app, 3),
        _ => {}
    }
}

/// Scrolls the details by `rows` (positive: down).
pub fn scroll(app: &mut App, rows: i32) {
    app.tasks.detail_scroll = app.tasks.detail_scroll.saturating_add_signed(rows.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
    app.needs_draw = true;
}

/// `RightDetail::Todo(id)`.
pub fn draw(app: &mut App, buf: &mut Buffer, area: Rect, id: &str) -> Option<(u16, u16)> {
    let width = area.width.saturating_sub(4) as usize;
    let mut out = Out { rows: Vec::new(), width };
    match book(app).map(|b| (b, b.todo(id))) {
        Some((b, Some(todo))) => todo_lines(app, b, todo, &mut out),
        Some((_, None)) => {
            out.line(vec![seg(format!("☑ {id}"), bold(TEXT)), seg("   ", fg(TEXT)), link(" ✕ ", fg(GRAY).bg(PANEL_HI), act(Action::CloseDetail))]);
            out.blank();
            out.wrap("This todo is not on the canvas's boards (any more).", fg(MUTED), None);
        }
        None => {
            out.line(vec![seg(format!("☑ {id}"), bold(TEXT))]);
            out.blank();
            out.wrap("The todo boards are not loaded yet.", fg(MUTED), None);
        }
    }
    paint(app, buf, area, out);
    None
}

/// The right pane with nothing else in it: the selected todo's details,
/// else the selected board and the keys.
pub fn draw_info(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    if let Some(id) = app.tasks.selected.clone().filter(|id| book(app).is_some_and(|b| b.todo(id).is_some())) {
        return draw(app, buf, area, &id);
    }
    let width = area.width.saturating_sub(4) as usize;
    let mut out = Out { rows: Vec::new(), width };
    let board = app.tasks.board.clone().or_else(|| book(app).and_then(|b| b.boards.iter().find(|x| x.parent.is_none())).map(|x| x.id.clone()));
    if let Some(bv) = board.as_deref().and_then(|id| book(app).and_then(|b| b.board_view(id))) {
        let board = bv.board;
        out.line(vec![seg(format!("▦ {}", board.name), bold(TEXT)), seg(format!(" · {}", board.prefix), fg(MUTED)), seg("  ", fg(TEXT)), link(" ≡ ", fg(GRAY).bg(PANEL_HI), act(Action::BoardMenu(board.id.clone())))]);
        let (done, all) = bv.progress();
        out.line(vec![seg(format!("{done} of {all} done"), fg(GRAY)), seg(if bv.blocked > 0 { format!(" · ⚑ {} blocked", bv.blocked) } else { String::new() }, fg(NEGATIVE))]);
        let groups: Vec<String> = board.canvas_groups.iter().map(|g| app.doc.group(g).map(|x| x.name.clone()).unwrap_or_else(|| g.clone())).collect();
        out.field("Groups", vec![link(if groups.is_empty() { "none (link…)".into() } else { groups.join(", ") }, fg(ACCENT), act(Action::LinkGroups(board.id.clone())))]);
        out.field("Agents", vec![seg(match board.policy.agent_adds {
            canvas_model::todos::AddPolicy::Direct => "add todos directly",
            canvas_model::todos::AddPolicy::Proposed => "propose todos",
        }, fg(TEXT)), seg(if board.policy.agents_may_close { ", may close them" } else { ", never close them" }, fg(TEXT))]);
        for column in &bv.columns {
            let count = match column.wip {
                Some(wip) => format!("{}/{wip}", column.count),
                None => column.count.to_string(),
            };
            out.field(&truncate(&column.name, 8), vec![seg(count, if column.over { bold(ATTENTION) } else { fg(TEXT) })]);
        }
        let lanes: Vec<String> = bv.lanes.iter().skip(1).map(|l| l.name.clone()).collect();
        if !lanes.is_empty() {
            out.field("Lanes", vec![seg(lanes.join(", "), fg(TEXT))]);
        }
        out.buttons(vec![("+ New todo".into(), act(Action::NewTodo { lane: board.id.clone(), status: Status::Backlog }), true), ("+ New board".into(), act(Action::NewBoard), false)]);
    } else {
        out.line(vec![seg("☑ TODO BOARDS", bold(TEXT))]);
        out.blank();
        out.wrap(if book(app).is_some() { "No board selected." } else { "The todo boards are not loaded yet." }, fg(MUTED), None);
    }
    out.head("KEYS", None);
    for (keys, what) in [
        ("Click · arrows", "select a card (details here)"),
        ("Enter · Space", "details · peek"),
        ("Drag a card", "another column, lane, board, place"),
        ("N · A · D · Del", "new · accept · decline · cancel"),
        ("E · R", "edit body · rename"),
        ("Drag title · ◢", "move · resize a board"),
        ("Wheel · Ctrl+arrows", "pan (Shift+wheel: sideways)"),
        ("+ - · Alt+wheel · F", "zoom · fit all boards"),
        ("/ · right-click", "filter · menus"),
    ] {
        out.line(vec![seg(format!("{:<20}", app.key_policy.label(keys)), fg(TEXT)), seg(what, fg(MUTED))]);
    }
    paint(app, buf, area, out);
    None
}

/// A todo's details.
fn todo_lines(app: &App, b: &TodoBook, todo: &Todo, out: &mut Out) {
    let id = todo.id.clone();
    let color = type_color(todo.kind);
    let status = b.root(&todo.board).map(|r| r.status_name(todo.status)).unwrap_or_else(|| todo.status.label().to_owned());
    // The header: id, type, blocker; its menu and close.
    let mut head = vec![seg(format!("{} ", type_glyph(todo.kind)), bold(color)), seg(id.clone(), bold(TEXT)), seg(format!(" · {}", todo.kind.word()), fg(GRAY))];
    if todo.blocker {
        head.push(seg(" · ‼ blocker", bold(NEGATIVE)));
    }
    let used: usize = head.iter().map(|s| text_width(&s.text)).sum();
    head.push(seg(" ".repeat(out.width.saturating_sub(used + 8).max(1)), fg(TEXT)));
    head.push(link(" ≡ ", fg(GRAY).bg(PANEL_HI), act(Action::TodoMenu(id.clone()))));
    head.push(seg(" ", fg(TEXT)));
    head.push(link(" ✕ ", fg(GRAY).bg(PANEL_HI), act(Action::CloseDetail)));
    out.line(head);
    out.wrap(&todo.title, bold(TEXT), Some(act(Action::Ask(Ask::RenameTodo(id.clone())))));
    out.blank();
    let picker = |text: String, action: Action| link(format!("{text} ▾"), fg(TEXT).bg(PANEL_HI), act(action));
    let status_style = if todo.status == Status::Proposed { bold(ATTENTION) } else { fg(TEXT) };
    if todo.status == Status::Proposed {
        out.field("Status", vec![seg(status, status_style)]);
    } else {
        out.field("Status", vec![picker(status, Action::PickStatus(id.clone()))]);
    }
    let priority = if todo.priority.marks().is_empty() { todo.priority.word().to_owned() } else { format!("{} {}", todo.priority.marks(), todo.priority.word()) };
    out.field("Priority", vec![picker(priority, Action::PickPriority(id.clone()))]);
    out.field("Type", vec![picker(format!("{} {}", type_glyph(todo.kind), todo.kind.word()), Action::PickType(id.clone()))]);
    let lane = b.board(&todo.board).map(|lane| match lane.parent.as_deref().and_then(|p| b.board(p)) {
        Some(parent) => format!("{} › {}", parent.name, lane.name),
        None => lane.name.clone(),
    });
    out.field("Board", vec![link(lane.unwrap_or_else(|| todo.board.clone()), fg(ACCENT), act(Action::OpenBoard(todo.board.clone())))]);
    if let Some(claim) = &todo.claimed_by {
        let name = book::session_name(app, todo);
        let mut value = match claim.by.session.clone() {
            Some(session) => vec![link(name, fg(ACCENT), act(Action::ViewSession(session.clone())))],
            None => vec![seg(name, fg(TEXT))],
        };
        match claim.by.session.as_deref() {
            Some(s) if book::session_working(app, s) => value.push(seg(" ⟳ working", fg(TEXT))),
            Some(s) if book::session_gone(app, s) => value.push(seg(" ⚠ gone or stopped", fg(NEGATIVE))),
            _ => {}
        }
        value.push(seg(format!(" · {}", date(claim.ms)), fg(MUTED)));
        out.field("Claimed", value);
    }
    let blockers: Vec<String> = b.blockers(todo).iter().map(|t| t.id.clone()).collect();
    let blocked = match (&todo.blocked, blockers.is_empty()) {
        (Some(why), _) => vec![seg(format!("⚑ {why} "), fg(NEGATIVE)), link(" edit ", fg(ACCENT).bg(PANEL_HI), act(Action::Ask(Ask::Blocked(id.clone()))))],
        (None, false) => vec![seg(format!("⚑ waits for {} ", blockers.join(", ")), fg(NEGATIVE)), link(" reason… ", fg(ACCENT).bg(PANEL_HI), act(Action::Ask(Ask::Blocked(id.clone()))))],
        (None, true) => vec![seg("no ", fg(TEXT)), link(" set… ", fg(ACCENT).bg(PANEL_HI), act(Action::Ask(Ask::Blocked(id.clone()))))],
    };
    out.field("Blocked", blocked);
    let flip = op(TodoOp::Update(Box::new(TodoUpdate { id: id.clone(), blocker: Some(!todo.blocker), ..Default::default() })));
    out.field("Blocker", vec![seg(if todo.blocker { "yes ‼ " } else { "no " }, fg(TEXT)), link(if todo.blocker { " unmark " } else { " mark " }, fg(ACCENT).bg(PANEL_HI), flip)]);
    out.field("Created", vec![seg(format!("{} · {}", todo.created_by.label(), date(todo.created_ms)), fg(MUTED)), seg(if todo.ai_created() { " ◇" } else { "" }, fg(PURPLE))]);
    if !todo.labels.is_empty() {
        out.field("Labels", vec![seg(todo.labels.join(", "), fg(TEXT))]);
    }
    // What can be done now.
    out.blank();
    let (proposal, closed) = (todo.status == Status::Proposed, todo.status.closed());
    let mut buttons: Vec<(String, Hit, bool)> = Vec::new();
    if proposal {
        buttons.push(("Accept".into(), op(TodoOp::Accept { id: id.clone(), status: None }), true));
        buttons.push(("Decline".into(), op(TodoOp::Decline { id: id.clone(), reason: None }), false));
    }
    if !proposal && !closed {
        buttons.push(("Start a session on this…".into(), act(Action::StartSession(id.clone())), !proposal));
    }
    buttons.push(("Open spec note".into(), act(Action::SpecNote(id.clone())), false));
    if !proposal && !closed {
        buttons.push(("Claim for…".into(), act(Action::ClaimFor(id.clone())), false));
    }
    if todo.claimed_by.is_some() {
        buttons.push(("Release".into(), op(TodoOp::Release { id: id.clone() }), false));
    }
    if !proposal && !closed {
        buttons.push(("Done".into(), op(TodoOp::Done { id: id.clone(), note: None }), false));
    }
    if !closed {
        buttons.push(("Cancel".into(), act(Action::ConfirmCancel(id.clone())), false));
    }
    if closed {
        buttons.push(("Reopen".into(), op(TodoOp::Reopen { id: id.clone() }), false));
    }
    out.buttons(buttons);
    more_lines(app, b, todo, out);
}

/// The proposal, checklist, body, links, relations and log.
fn more_lines(app: &App, b: &TodoBook, todo: &Todo, out: &mut Out) {
    let id = todo.id.clone();
    if let Some(proposal) = &todo.proposal {
        out.head(if todo.status == Status::Proposed { "PROPOSED · WHY" } else { "PROPOSAL · WHY" }, None);
        out.wrap(if proposal.why.trim().is_empty() { "(no reason given)" } else { &proposal.why }, fg(PURPLE), None);
    }
    let (done, all) = todo.checked();
    out.head(&format!("CHECKLIST {done}/{all}"), Some(("+ add", act(Action::Ask(Ask::AddCheck(id.clone()))))));
    for (index, item) in todo.checklist.iter().enumerate() {
        let toggle = op(TodoOp::Check { id: id.clone(), index, done: !item.done });
        let (mark, style) = if item.done { ("☑ ", fg(MUTED).add_modifier(Modifier::CROSSED_OUT)) } else { ("☐ ", fg(TEXT)) };
        out.line(vec![link(mark, fg(ACCENT), toggle.clone()), link(truncate(&item.text, out.width.saturating_sub(2)), style, toggle)]);
    }
    out.head("BODY", Some(("edit", act(Action::EditBody(id.clone())))));
    if todo.body.trim().is_empty() {
        out.line(vec![seg("(empty)", fg(MUTED))]);
    } else {
        for line in crate::richtext::layout(&crate::richtext::markdown(&todo.body), out.width.max(10)) {
            let segs = line
                .spans
                .into_iter()
                .map(|(text, style, url)| match url.filter(|u| crate::links::is_url(u)) {
                    Some(url) => link(text, style.fg(BLUE).add_modifier(Modifier::UNDERLINED), act(Action::OpenUrl(url))),
                    None => seg(text, style),
                })
                .collect();
            out.line(segs);
        }
    }
    let note_linked = todo.note.as_ref().is_some_and(|n| todo.links.iter().any(|l| l.kind == LinkKind::Note && l.target == *n));
    if !todo.links.is_empty() || todo.note.is_some() {
        out.head("LINKS", None);
    }
    if let Some(note) = todo.note.as_ref().filter(|_| !note_linked) {
        out.line(vec![link(format!("✎ {}", file_name(note)), fg(ACCENT), act(Action::OpenNote(note.clone())))]);
    }
    for l in &todo.links {
        let title = if l.title.is_empty() { l.target.clone() } else { l.title.clone() };
        out.line(match l.kind {
            LinkKind::Note => vec![link(format!("✎ {}", file_name(&l.target)), fg(ACCENT), act(Action::OpenNote(l.target.clone()))), seg(if l.title.is_empty() { String::new() } else { format!(" ({})", l.title) }, fg(MUTED))],
            LinkKind::Session => {
                let name = app.doc.node(&l.target).map(|n| n.name.clone()).unwrap_or(title);
                vec![link(format!("◫ {name}"), fg(ACCENT), act(Action::ViewSession(l.target.clone())))]
            }
            LinkKind::Pr => {
                let state = app.prs_view().and_then(|v| v.prs.iter().find(|p| p.url == l.target)).map(|p| format!(" {}", p.state.word())).unwrap_or_default();
                vec![link(format!("↗ {title}"), fg(ACCENT), act(Action::OpenUrl(l.target.clone()))), seg(state, fg(GRAY))]
            }
            LinkKind::Url => vec![link(format!("↗ {title}"), fg(ACCENT), act(Action::OpenUrl(l.target.clone())))],
            LinkKind::Report => vec![seg(format!("▤ report {title}"), fg(TEXT))],
            LinkKind::File => vec![seg(format!("▢ {title}"), fg(TEXT))],
        });
    }
    let blocking = b.blocking(&todo.id);
    if !todo.relations.is_empty() || !blocking.is_empty() {
        out.head("RELATIONS", None);
    }
    let related = |kind: &str, other: &str| -> Vec<Seg> {
        match b.todo(other) {
            Some(t) => vec![seg(format!("{kind} "), fg(MUTED)), link(t.id.clone(), bold(ACCENT), Hit::Todo(t.id.clone())), seg(format!(" {} · {}", t.title, t.status.label().to_lowercase()), fg(if t.status.closed() { MUTED } else { TEXT }))],
            None => vec![seg(format!("{kind} {other} (gone)"), fg(MUTED))],
        }
    };
    for relation in &todo.relations {
        out.line(related(relation.kind.label(), &relation.id));
    }
    for other in blocking {
        out.line(related("blocks", &other.id));
    }
    if !todo.log.is_empty() {
        out.head("LOG", None);
    }
    for entry in todo.log.iter().rev().take(40) {
        let style = if entry.kind == LogKind::Note { fg(TEXT) } else { fg(MUTED) };
        let text = format!("{} {}: {}", &date(entry.ms)[5..], entry.by.label(), entry.text);
        out.wrap(&text, style, None);
    }
    if todo.log.len() > 40 {
        out.line(vec![seg(format!("… {} older", todo.log.len() - 40), fg(MUTED))]);
    }
}

/// A path's last part.
fn file_name(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_owned()
}
