//! The todo canvas's geometry, in screen cells (pure): board frames at their
//! world place under a pan and one of three zoom levels, their columns,
//! swimlanes and cards ([`layout`]), what is under a cell ([`Scene::hit`]),
//! where a dragged card lands ([`Scene::drop_at`], [`move_op`]) and how the
//! keyboard selection walks the cards ([`Grid`], [`step`]).
//!
//! Frames keep their place and size relative to each other at every zoom
//! (world cells divided by 1, 2 or 4); only what is inside them changes:
//! full cards at 100 %, one-line cards at 50 %, a summary at 25 %.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use canvas_model::todos::{Status, Todo, TodoBook, TodoOp, TodoType, rank_between};
use ratatui::layout::Rect;

/// World cells per screen cell at each zoom level (100 %, 50 %, 25 %).
pub const DIVS: [i32; 3] = [1, 2, 4];
/// A card's rows at 100 % (a gap row follows it).
pub const CARD_H: i32 = 4;
/// A card's rows at 50 %: its id, then its title.
pub const CARD_H_HALF: i32 = 2;

/// A rectangle of screen cells that may lie partly off screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cells {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Cells {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Cells { x, y, w, h }
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    /// The part inside `clip`, if any.
    pub fn clipped(&self, clip: Rect) -> Option<Rect> {
        let x0 = self.x.max(clip.x as i32);
        let y0 = self.y.max(clip.y as i32);
        let x1 = self.right().min(clip.right() as i32);
        let y1 = self.bottom().min(clip.bottom() as i32);
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0 as u16, y0 as u16, (x1 - x0) as u16, (y1 - y0) as u16))
    }
}

/// Where the todo canvas looks: its area on screen, the world cell at its
/// top-left corner and the zoom level (0: 100 %, 1: 50 %, 2: 25 %).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub area: Rect,
    pub pan: (i32, i32),
    pub zoom: u8,
}

impl View {
    pub fn div(&self) -> i32 {
        DIVS[(self.zoom as usize).min(DIVS.len() - 1)]
    }

    pub fn to_screen(&self, wx: i32, wy: i32) -> (i32, i32) {
        let d = self.div();
        (self.area.x as i32 + (wx - self.pan.0).div_euclid(d), self.area.y as i32 + (wy - self.pan.1).div_euclid(d))
    }

    pub fn to_world(&self, sx: i32, sy: i32) -> (i32, i32) {
        let d = self.div();
        (self.pan.0 + (sx - self.area.x as i32) * d, self.pan.1 + (sy - self.area.y as i32) * d)
    }

    /// A world length in screen cells (rounded up).
    pub fn cells(&self, n: i32) -> i32 {
        let d = self.div();
        (n.max(0) + d - 1) / d
    }
}

/// Which todos the canvas and the sidebar show.
#[derive(Clone, Copy, Debug, Default)]
pub struct Filter<'a> {
    /// Words the id, title or labels contain (case-insensitive).
    pub text: &'a str,
    pub kind: Option<TodoType>,
}

impl Filter<'_> {
    pub fn active(&self) -> bool {
        !self.text.trim().is_empty() || self.kind.is_some()
    }

    pub fn matches(&self, todo: &Todo) -> bool {
        if self.kind.is_some_and(|k| k != todo.kind) {
            return false;
        }
        let needle = self.text.trim().to_lowercase();
        needle.is_empty() || needle.split_whitespace().all(|word| todo.id.to_lowercase().contains(word) || todo.title.to_lowercase().contains(word) || todo.labels.iter().any(|l| l.to_lowercase().contains(word)))
    }
}

/// A chip on a frame's title row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Chip {
    /// `◫ group`: the first linked canvas group (its id).
    Group(String),
    /// `◫ +`: no group linked yet (links some).
    NoGroup,
    /// `✎ notes`: the board's notes folder.
    Notes(String),
    /// `✎ +`: no notes folder yet (asks for one).
    NoNotes,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub status: Status,
    /// Its left edge (a separator for all but the first) and width.
    pub x: i32,
    pub w: i32,
    pub name: String,
    pub count: usize,
    pub wip: Option<u32>,
    pub over: bool,
    pub collapsed: bool,
}

/// A swimlane: the board itself, then its sub-boards.
#[derive(Clone, Debug, PartialEq)]
pub struct Lane {
    pub board: String,
    pub name: String,
    /// Its first row is a header (`── Parser ──`) when the board has sub-boards.
    pub head: bool,
    pub y: i32,
    pub h: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub id: String,
    /// Its place in the book's `todos` the layout was made from.
    pub index: usize,
    pub rect: Cells,
    pub status: Status,
    /// The swimlane (board id) it is in.
    pub lane: String,
    /// Open and blocked (a reason, or an open todo it waits for): `⚑ blocked`.
    pub blocked: bool,
    /// Open todos waiting for it: `blocks N`.
    pub blocks: usize,
}

/// 25 %: what a board's summary shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    /// Per column: (status, count, WIP limit, over it).
    pub columns: Vec<(Status, usize, Option<u32>, bool)>,
    /// (done, accepted and not canceled).
    pub progress: (usize, usize),
    pub blocked: usize,
    pub canceled: usize,
}

/// A board as drawn.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    /// The top-level board's id.
    pub board: String,
    pub rect: Cells,
    /// Its title (` Name · PRE `) on the top row, then the chips.
    pub label: (i32, String),
    pub chips: Vec<(Cells, String, Chip)>,
    /// `n canceled` at the right end of the title row.
    pub canceled: Option<(i32, String)>,
    /// The `◢` resize corner.
    pub corner: (i32, i32),
    pub columns: Vec<Column>,
    pub lanes: Vec<Lane>,
    pub cards: Vec<Card>,
    /// `+N more` below a cell's last card that fits: (x, y, count).
    pub more: Vec<(i32, i32, usize)>,
    /// The collapsed Done column's count per lane: (x, y, count).
    pub done: Vec<(i32, i32, usize)>,
    /// 25 %: a summary instead of columns.
    pub summary: bool,
    /// The summary's numbers (25 % only).
    pub stats: Option<Stats>,
}

/// What [`layout`] needs besides the book.
#[derive(Clone, Copy, Debug)]
pub struct Input<'a> {
    pub view: View,
    pub filter: Filter<'a>,
    /// Canvas group id → name (the `◫` chips).
    pub groups: &'a BTreeMap<String, String>,
    /// A board being moved or resized: (id, dx, dy, dw, dh) in world cells.
    pub moving: Option<(&'a str, i32, i32, i32, i32)>,
}

/// The todo canvas as drawn: its frames in book order (later ones on top).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    pub view: View,
    pub frames: Vec<Frame>,
}

/// Lays out every top-level board of the book.
pub fn layout(book: &TodoBook, input: &Input) -> Scene {
    let blocking = blocking_counts(book);
    let index: HashMap<&str, usize> = book.todos.iter().enumerate().map(|(i, t)| (t.id.as_str(), i)).collect();
    let frames = book.boards.iter().filter(|b| b.parent.is_none()).filter_map(|b| frame(book, input, &b.id, &blocking, &index)).collect();
    Scene { view: input.view, frames }
}

/// How many open todos wait for each todo (`blocks N`), by its id: one
/// pass over the book (asking [`TodoBook::blocking`] per card is a pass each).
fn blocking_counts(book: &TodoBook) -> HashMap<String, usize> {
    let mut out: HashMap<String, usize> = HashMap::new();
    for todo in book.todos.iter().filter(|t| !t.status.closed()) {
        let waits: BTreeSet<&str> = todo.blocked_by().filter_map(|id| book.todo(id)).map(|t| t.id.as_str()).collect();
        for id in waits {
            *out.entry(id.to_owned()).or_default() += 1;
        }
    }
    out
}

/// Rows `n` cards take (gaps between them, none after the last).
fn rows_for(n: usize, card_h: i32, slot: i32) -> i32 {
    if n == 0 { 0 } else { n as i32 * slot - (slot - card_h) }
}

/// Shares `avail` rows between lanes: what each needs when there is room
/// (the last takes the rest), else at least `base` each in order.
fn share(need: &[i32], base: &[i32], avail: i32) -> Vec<i32> {
    let total: i32 = need.iter().sum();
    if total <= avail {
        let mut out = need.to_vec();
        if let Some(last) = out.last_mut() {
            *last += avail - total;
        }
        return out;
    }
    let mut out: Vec<i32> = base.iter().zip(need).map(|(b, n)| (*b).min(*n)).collect();
    let mut left = avail - out.iter().sum::<i32>();
    if left < 0 {
        let mut rest = avail;
        for v in out.iter_mut() {
            *v = (*v).min(rest.max(0));
            rest -= *v;
        }
        return out;
    }
    for (v, n) in out.iter_mut().zip(need) {
        let more = (n - *v).min(left);
        *v += more;
        left -= more;
    }
    out
}

fn frame(book: &TodoBook, input: &Input, id: &str, blocking: &HashMap<String, usize>, index: &HashMap<&str, usize>) -> Option<Frame> {
    let bv = book.board_view(id)?;
    let board = bv.board;
    let view = input.view;
    let (dx, dy, dw, dh) = match input.moving {
        Some((moving, dx, dy, dw, dh)) if moving == id => (dx, dy, dw, dh),
        _ => (0, 0, 0, 0),
    };
    let (x, y) = view.to_screen(board.x + dx, board.y + dy);
    let w = view.cells((board.w + dw).clamp(30, 2_000)).max(12);
    let h = view.cells((board.h + dh).clamp(8, 1_000)).max(5);
    let rect = Cells::new(x, y, w, h);
    // The title row: the name, the chips, the canceled count at the right.
    let mut limit = x + w - 1;
    let canceled = (bv.canceled > 0).then(|| format!(" {} canceled ", bv.canceled)).map(|t| (limit - 1 - crate::paint::text_width(&t) as i32, t)).filter(|(at, _)| *at > x + 14);
    if let Some((at, _)) = &canceled {
        limit = *at - 1;
    }
    let label = crate::paint::truncate(&format!(" {} · {} ", board.name, board.prefix), (limit - x - 2).max(0) as usize);
    let mut cx = x + 2 + crate::paint::text_width(&label) as i32 + 1;
    let mut chips = Vec::new();
    let group = board.canvas_groups.first().map(|g| (input.groups.get(g).cloned().unwrap_or_else(|| g.clone()), g.clone()));
    let wanted = [
        match group {
            Some((name, id)) => (format!(" ◫ {} ", crate::paint::truncate(&name, 18)), Chip::Group(id)),
            None => (" ◫ + ".to_owned(), Chip::NoGroup),
        },
        match &board.notes {
            Some(path) => (" ✎ notes ".to_owned(), Chip::Notes(path.clone())),
            None => (" ✎ + ".to_owned(), Chip::NoNotes),
        },
    ];
    for (text, chip) in wanted {
        let cw = crate::paint::text_width(&text) as i32;
        if cx + cw > limit {
            break;
        }
        chips.push((Cells::new(cx, y, cw, 1), text, chip));
        cx += cw + 1;
    }
    let mut out = Frame { board: board.id.clone(), rect, label: (x + 2, label), chips, canceled, corner: (x + w - 1, y + h - 1), columns: Vec::new(), lanes: Vec::new(), cards: Vec::new(), more: Vec::new(), done: Vec::new(), summary: view.zoom >= 2, stats: None };
    if out.summary {
        let columns = bv.columns.iter().map(|c| (c.status, c.count, c.wip, c.over)).collect();
        out.stats = Some(Stats { columns, progress: bv.progress(), blocked: bv.blocked, canceled: bv.canceled });
        return Some(out);
    }
    // Columns: Done is collapsed and half as wide.
    let (x0, iw) = (x + 1, w - 2);
    let weights: Vec<i32> = bv.columns.iter().map(|c| if c.collapsed { 1 } else { 2 }).collect();
    let total = weights.iter().sum::<i32>().max(1);
    let mut col_x = x0;
    for (i, c) in bv.columns.iter().enumerate() {
        let cw = if i + 1 == bv.columns.len() { x0 + iw - col_x } else { iw * weights[i] / total };
        out.columns.push(Column { status: c.status, x: col_x, w: cw.max(1), name: c.name.clone(), count: c.count, wip: c.wip, over: c.over, collapsed: c.collapsed });
        col_x += cw;
    }
    // Swimlanes share the rows below the column headers.
    let head = bv.lanes.len() > 1;
    let (card_h, slot) = if view.zoom == 0 { (CARD_H, CARD_H + 1) } else { (CARD_H_HALF, CARD_H_HALF + 1) };
    let shown: Vec<Vec<Vec<&Todo>>> = (0..bv.lanes.len()).map(|l| bv.columns.iter().map(|c| c.cells[l].iter().copied().filter(|t| input.filter.matches(t)).collect()).collect()).collect();
    let need: Vec<i32> = shown
        .iter()
        .map(|cells| {
            let most = bv.columns.iter().zip(cells).filter(|(c, _)| !c.collapsed).map(|(_, cell)| cell.len()).max().unwrap_or(0).max(1);
            i32::from(head) + rows_for(most, card_h, slot)
        })
        .collect();
    let base: Vec<i32> = need.iter().map(|_| i32::from(head) + card_h).collect();
    let top = y + 2;
    let heights = share(&need, &base, (y + h - 1 - top).max(0));
    let mut lane_y = top;
    for (l, (lane, lh)) in bv.lanes.iter().zip(heights).enumerate() {
        out.lanes.push(Lane { board: lane.id.clone(), name: lane.name.clone(), head, y: lane_y, h: lh });
        let room = lh - i32::from(head);
        for (column, cell) in out.columns.iter().zip(&shown[l]) {
            if column.collapsed {
                if room > 0 {
                    out.done.push((column.x + 1, lane_y + i32::from(head), cell.len()));
                }
                continue;
            }
            // Not all fit: a row stays for `+N more`.
            let fit = if rows_for(cell.len(), card_h, slot) <= room { cell.len() } else { (0..cell.len()).rev().find(|k| rows_for(*k, card_h, slot) < room).unwrap_or(0) };
            for (i, todo) in cell.iter().take(fit).enumerate() {
                let rect = Cells::new(column.x + 1, lane_y + i32::from(head) + i as i32 * slot, (column.w - 2).max(1), card_h);
                let (blocked, blocks) = (!todo.status.closed() && book.is_blocked(todo), blocking.get(&todo.id).copied().unwrap_or(0));
                let at = index.get(todo.id.as_str()).copied().unwrap_or(usize::MAX);
                out.cards.push(Card { id: todo.id.clone(), index: at, rect, status: column.status, lane: lane.id.clone(), blocked, blocks });
            }
            if fit < cell.len() && room >= 1 {
                out.more.push((column.x + 1, lane_y + lh - 1, cell.len() - fit));
            }
        }
        lane_y += lh;
    }
    Some(out)
}

/// What is under a cell of the todo canvas.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Card(String),
    /// A frame's title row (drag: move the board; right-click: its menu).
    Title(String),
    /// Its `◢` corner (drag: resize).
    Corner(String),
    Chip(String, Chip),
    /// A column of a swimlane: (top-level board, lane board, status).
    Cell { board: String, lane: String, status: Status },
    /// Anywhere else in a frame (borders, headers, a 25 % summary).
    Frame(String),
    Background,
}

/// Where a dragged card goes: a lane (board id), a column (`None`: its
/// own, dropped on a 25 % summary) and its neighbours there.
#[derive(Clone, Debug, PartialEq)]
pub struct Drop {
    pub board: String,
    pub status: Option<Status>,
    /// The shown card it goes after, and the one it goes before.
    pub after: Option<String>,
    pub before: Option<String>,
}

impl Frame {
    /// The lane and column at a cell (the rows below the column headers).
    pub fn cell_at(&self, x: i32, y: i32) -> Option<(&Lane, &Column)> {
        if self.summary {
            return None;
        }
        let column = self.columns.iter().find(|c| x >= c.x && x < c.x + c.w)?;
        let lane = self.lanes.iter().find(|l| y >= l.y && y < l.y + l.h)?;
        Some((lane, column))
    }

    /// Moves everything by whole cells.
    fn shift(&mut self, dx: i32, dy: i32) {
        self.rect.x += dx;
        self.rect.y += dy;
        self.label.0 += dx;
        for (cells, _, _) in &mut self.chips {
            cells.x += dx;
            cells.y += dy;
        }
        if let Some((x, _)) = &mut self.canceled {
            *x += dx;
        }
        self.corner = (self.corner.0 + dx, self.corner.1 + dy);
        for column in &mut self.columns {
            column.x += dx;
        }
        for lane in &mut self.lanes {
            lane.y += dy;
        }
        for card in &mut self.cards {
            card.rect.x += dx;
            card.rect.y += dy;
        }
        for at in self.more.iter_mut().chain(self.done.iter_mut()) {
            at.0 += dx;
            at.1 += dy;
        }
    }
}

impl Scene {
    /// A cell of the area the canvas was drawn in: frames go on beyond it,
    /// under the sidebar, the right pane and the bars, where nothing of
    /// theirs shows and nothing is under the pointer.
    pub fn shows(&self, x: i32, y: i32) -> bool {
        let a = self.view.area;
        x >= a.x as i32 && x < a.right() as i32 && y >= a.y as i32 && y < a.bottom() as i32
    }

    /// The same layout under another pan, when it differs by whole screen
    /// cells (a multiple of the zoom's world cells per cell, as drags, the
    /// wheel and the keys pan): every frame moves. False: lay it out again.
    pub fn repan(&mut self, pan: (i32, i32)) -> bool {
        let d = self.view.div();
        let (dx, dy) = (pan.0 - self.view.pan.0, pan.1 - self.view.pan.1);
        if dx % d != 0 || dy % d != 0 {
            return false;
        }
        for frame in &mut self.frames {
            frame.shift(-dx / d, -dy / d);
        }
        self.view.pan = pan;
        true
    }

    pub fn hit(&self, x: i32, y: i32) -> Target {
        if !self.shows(x, y) {
            return Target::Background;
        }
        for f in self.frames.iter().rev() {
            if !f.rect.contains(x, y) {
                continue;
            }
            if (x, y) == f.corner {
                return Target::Corner(f.board.clone());
            }
            if let Some((_, _, chip)) = f.chips.iter().find(|(r, _, _)| r.contains(x, y)) {
                return Target::Chip(f.board.clone(), chip.clone());
            }
            if y == f.rect.y {
                return Target::Title(f.board.clone());
            }
            if let Some(card) = f.cards.iter().find(|c| c.rect.contains(x, y)) {
                return Target::Card(card.id.clone());
            }
            if let Some((lane, column)) = f.cell_at(x, y) {
                return Target::Cell { board: f.board.clone(), lane: lane.board.clone(), status: column.status };
            }
            return Target::Frame(f.board.clone());
        }
        Target::Background
    }

    /// A shown card.
    pub fn card(&self, id: &str) -> Option<&Card> {
        self.frames.iter().flat_map(|f| &f.cards).find(|c| c.id == id)
    }

    pub fn frame(&self, board: &str) -> Option<&Frame> {
        self.frames.iter().find(|f| f.board == board)
    }

    /// Where card `moving` dropped at a cell goes (`None`: outside every
    /// frame, or outside the area: over the sidebar or a bar).
    pub fn drop_at(&self, x: i32, y: i32, moving: &str) -> Option<Drop> {
        if !self.shows(x, y) {
            return None;
        }
        let f = self.frames.iter().rev().find(|f| f.rect.contains(x, y))?;
        if f.summary {
            return Some(Drop { board: f.board.clone(), status: None, after: None, before: None });
        }
        let column = f.columns.iter().find(|c| x >= c.x && x < c.x + c.w)?;
        let (first, last) = (f.lanes.first()?, f.lanes.last()?);
        let lane = f.lanes.iter().find(|l| y >= l.y && y < l.y + l.h).unwrap_or(if y < first.y { first } else { last });
        let cards: Vec<&Card> = f.cards.iter().filter(|c| c.lane == lane.board && c.status == column.status && c.id != moving).collect();
        // A card is before the pointer when its middle is above it.
        let index = cards.iter().filter(|c| 2 * c.rect.y + c.rect.h <= 2 * y).count();
        Some(Drop { board: lane.board.clone(), status: Some(column.status), after: index.checked_sub(1).map(|i| cards[i].id.clone()), before: cards.get(index).map(|c| c.id.clone()) })
    }
}

fn by_rank(a: &&Todo, b: &&Todo) -> std::cmp::Ordering {
    a.rank.cmp(&b.rank).then(a.created_ms.cmp(&b.created_ms)).then(a.id.cmp(&b.id))
}

/// The operation that puts todo `id` where it was dropped: its new column
/// and lane, and a rank between its new neighbours in the whole cell
/// (hidden cards included). `None`: it stays where it was.
pub fn move_op(book: &TodoBook, id: &str, drop: &Drop) -> Option<TodoOp> {
    let todo = book.todo(id)?;
    // Onto the summary of its own board (25 %): it stays in its swimlane.
    if drop.status.is_none() && book.root(&todo.board).is_some_and(|root| root.id == drop.board) {
        return None;
    }
    let status = drop.status.unwrap_or(todo.status);
    let board = book.board(&drop.board).map(|b| b.id.clone())?;
    let mut cell: Vec<&Todo> = book.todos.iter().filter(|t| t.board == board && t.status == status && t.id != todo.id).collect();
    cell.sort_by(by_rank);
    let pos = |other: &Option<String>| other.as_deref().and_then(|o| cell.iter().position(|t| t.id == o));
    let at = match (pos(&drop.after), pos(&drop.before)) {
        (Some(i), _) => i + 1,
        (None, Some(i)) => i,
        (None, None) => cell.len(),
    };
    let here = cell.iter().filter(|t| by_rank(t, &todo) == std::cmp::Ordering::Less).count();
    if status == todo.status && board == todo.board && at == here {
        return None;
    }
    let prev = at.checked_sub(1).map(|i| cell[i].rank.as_str());
    let next = cell.get(at).map(|t| t.rank.as_str());
    Some(TodoOp::Move {
        id: todo.id.clone(),
        status: (status != todo.status).then_some(status),
        board: (board != todo.board).then_some(board),
        after: None,
        before: None,
        rank: Some(rank_between(prev, next)),
    })
}

/// A board's cards as the arrow keys walk them: `cells[lane][column]` (the
/// collapsed Done column has none).
#[derive(Clone, Debug, PartialEq)]
pub struct Grid {
    pub board: String,
    pub lanes: Vec<String>,
    pub columns: Vec<Status>,
    pub cells: Vec<Vec<Vec<String>>>,
}

/// A place in a [`Grid`]: a lane, a column and maybe a card there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spot {
    pub lane: usize,
    pub column: usize,
    pub card: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

/// A top-level board's grid (a sub-board: its parent's).
pub fn grid(book: &TodoBook, board: &str, filter: &Filter) -> Option<Grid> {
    let bv = book.board_view(board)?;
    let cells = (0..bv.lanes.len())
        .map(|l| bv.columns.iter().map(|c| if c.collapsed { Vec::new() } else { c.cells[l].iter().filter(|t| filter.matches(t)).map(|t| t.id.clone()).collect() }).collect())
        .collect();
    Some(Grid { board: bv.board.id.clone(), lanes: bv.lanes.iter().map(|l| l.id.clone()).collect(), columns: bv.columns.iter().map(|c| c.status).collect(), cells })
}

impl Grid {
    /// Where a card is.
    pub fn find(&self, id: &str) -> Option<Spot> {
        for (lane, row) in self.cells.iter().enumerate() {
            for (column, cell) in row.iter().enumerate() {
                if let Some(i) = cell.iter().position(|c| c == id) {
                    return Some(Spot { lane, column, card: Some(i) });
                }
            }
        }
        None
    }

    /// A lane's column (its first card, if any).
    pub fn spot(&self, lane: &str, status: Status) -> Option<Spot> {
        let lane = self.lanes.iter().position(|l| l == lane)?;
        let column = self.columns.iter().position(|s| *s == status)?;
        Some(Spot { lane, column, card: (!self.cells[lane][column].is_empty()).then_some(0) })
    }

    /// The card at a spot.
    pub fn id(&self, spot: &Spot) -> Option<&str> {
        self.cells.get(spot.lane)?.get(spot.column)?.get(spot.card?).map(String::as_str)
    }

    fn len(&self, lane: usize, column: usize) -> usize {
        self.cells.get(lane).and_then(|r| r.get(column)).map_or(0, Vec::len)
    }
}

/// The next spot from `at` (`Err(1)` / `Err(-1)`: past the board's last /
/// first column, to the next / previous board). Up and down walk a cell's
/// cards, then the same column of the lane above or below; left and right
/// keep the card's place in the next column as far as it has cards.
pub fn step(grid: &Grid, at: &Spot, dir: Dir) -> Result<Spot, i32> {
    let (lanes, columns) = (grid.lanes.len(), grid.columns.len());
    let (lane, column) = (at.lane.min(lanes.saturating_sub(1)), at.column.min(columns.saturating_sub(1)));
    let near = |lane: usize, column: usize, want: usize| -> Spot {
        let n = grid.len(lane, column);
        Spot { lane, column, card: (n > 0).then(|| want.min(n - 1)) }
    };
    Ok(match dir {
        Dir::Up => match at.card {
            Some(i) if i > 0 => Spot { lane, column, card: Some(i - 1) },
            _ if lane > 0 => near(lane - 1, column, usize::MAX),
            _ => Spot { lane, column, card: at.card },
        },
        Dir::Down => match at.card {
            Some(i) if i + 1 < grid.len(lane, column) => Spot { lane, column, card: Some(i + 1) },
            _ if lane + 1 < lanes => near(lane + 1, column, 0),
            _ => Spot { lane, column, card: at.card },
        },
        Dir::Left if column == 0 => return Err(-1),
        Dir::Left => near(lane, column - 1, at.card.unwrap_or(0)),
        Dir::Right if column + 1 >= columns => return Err(1),
        Dir::Right => near(lane, column + 1, at.card.unwrap_or(0)),
    })
}

/// The pan and zoom that show every board: the closest zoom they fit at
/// (else 25 %), with a small margin.
pub fn fit(book: &TodoBook, area: Rect) -> ((i32, i32), u8) {
    let tops: Vec<_> = book.boards.iter().filter(|b| b.parent.is_none()).collect();
    if tops.is_empty() || area.width == 0 || area.height == 0 {
        return ((-2, -1), 0);
    }
    let x0 = tops.iter().map(|b| b.x).min().unwrap_or(0);
    let y0 = tops.iter().map(|b| b.y).min().unwrap_or(0);
    let x1 = tops.iter().map(|b| b.x + b.w.max(30)).max().unwrap_or(0);
    let y1 = tops.iter().map(|b| b.y + b.h.max(8)).max().unwrap_or(0);
    for (zoom, d) in DIVS.iter().enumerate() {
        if (x1 - x0 + 4 * d) / d <= area.width as i32 && (y1 - y0 + 2 * d) / d <= area.height as i32 {
            return ((x0 - 2 * d, y0 - d), zoom as u8);
        }
    }
    ((x0 - 8, y0 - 4), 2)
}

/// The pan that shows `rect` (screen cells under `view`) with the least
/// movement: unchanged when it is in the area, else moved just enough to
/// show it with a small margin (larger than the area: its top-left).
pub fn reveal(view: &View, rect: Cells) -> (i32, i32) {
    let area = view.area;
    let shift = |pos: i32, len: i32, start: i32, size: i32, margin: i32| -> i32 {
        if len + 2 * margin > size || pos < start + margin {
            pos - (start + margin)
        } else if pos + len > start + size - margin {
            pos + len - (start + size - margin)
        } else {
            0
        }
    };
    let inside = rect.x >= area.x as i32 && rect.right() <= area.right() as i32 && rect.y >= area.y as i32 && rect.bottom() <= area.bottom() as i32;
    if inside {
        return view.pan;
    }
    let d = view.div();
    let dx = shift(rect.x, rect.w, area.x as i32, area.width as i32, 2);
    let dy = shift(rect.y, rect.h, area.y as i32, area.height as i32, 1);
    (view.pan.0 + dx * d, view.pan.1 + dy * d)
}
