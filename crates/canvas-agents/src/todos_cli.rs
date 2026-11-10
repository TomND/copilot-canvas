//! `atelier todo …`: agents and you read and change the canvas's todo
//! boards (docs/NOTEBOOK-TASKS.md §3.3).
//!
//! Reads (`prime`, `boards`, `list`, `show`, `ready`, `mirror`, `export`)
//! read the stored book of the routed canvas (`<home>/bus/routing.json` →
//! `<home>/canvas-agents/<canvas id>/todos/book.json`). Writes post a
//! `canvas.todo` request into the service's bus inbox and wait for its
//! answer while the service runs (the `atelier pr` pattern); the
//! caller is `ATELIER_SESSION` (no session: you, or the automation
//! named by `AUTOMATION_NAME`), as for `atelier pr`: the service
//! trusts what the request says (any process running as you could write it),
//! so the rules guide agents and are no sandbox. `--format json` prints JSON
//! with `schemaVersion`.
//!
//! The global `--canvas FILE` is `todo export`'s output; every other verb
//! leaves it to the client (it names the canvas file a window opens).

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use canvas_model::todos::{self, Actor, AddPolicy, LinkKind, NewTodo, Priority, RelKind, SCHEMA_VERSION, Status, Todo, TodoBook, TodoOp, TodoType, TodoUpdate};
use serde_json::json;

use crate::bus::Routing;
use crate::prs_cli::{Answer, ask_from};

/// The bus event of a request (handled by the service, never routed to agents).
pub const REQUEST: &str = "canvas.todo";

/// The `canvas-todos` skill shipped in the canvas plugin.
pub const SKILL: &str = "---
name: canvas-todos
description: Work with the Atelier todo boards - find ready todos, claim one, record progress, submit finished work for review with its pull request, add todos the user asks for and propose your own ideas. Use when the canvas or your group has todo boards, when the user mentions todos, tasks, a backlog, a board or an id like DEC-031, when asked to pick up or continue work, when you finish work a todo tracks, and when you find work worth doing later.
---

# Atelier todos

The canvas keeps todo boards that the user sees in its Tasks space; todos have
ids like `DEC-031`. Change them only with `atelier todo …`: never edit
the book's files or a `TODOs.md` mirror (it is generated and rewritten).

1. Run `atelier todo prime` first: the boards linked to your group, your
   claims (the user may assign todos to you), what is ready, and these rules with the commands.
2. Work on a todo: `todo ready` → `todo show ID` → `todo claim ID` (never one
   another session claimed) → `todo note ID \"what you found or decided\"` and
   `todo check ID N` as you go → `todo submit ID --pr URL [--summary FILE]` when
   it is finished. Submitting puts it in review: Done is the user's (or a merged
   pull request's), so never run `todo done`.
3. New work: `todo add \"TITLE\" --type bug|feature|improvement|optimization|chore|docs|research`
   when the user asked for it; `todo propose \"TITLE\" --type T --why \"…\"` when it
   is your own idea (the user accepts or declines it); add `--discovered-from ID`
   for work you found while doing another todo.
4. Blocked: `todo update ID --blocked \"why\"` (`--blocked none` when it goes on)
   or `todo relate ID --blocked-by OTHER`. Stopping before it is done:
   `todo release ID` with a note saying where you left it.
5. `--format json` on `list`, `show`, `ready` and `boards` prints JSON (`schemaVersion` 1).

A refusal exits non-zero and says why: agents never accept, decline, delete,
cancel or close todos, and never take, move or close a todo another session claimed.
";

const USAGE: &str = "usage: atelier todo prime
       atelier todo boards [--tree] [--format json]
       atelier todo list [--board B] [--status S] [--type T] [--mine] [--all] [--format json]
       atelier todo show ID [--format json]
       atelier todo ready [--board B] [--format json]
       atelier todo add TITLE --type T [--board B] [--priority P] [--status backlog|todo] [--body FILE|-] [--parent ID] [--label L]… [--check TEXT]…
       atelier todo propose TITLE --type T --why TEXT [--board B] [--discovered-from ID] [--priority P] [--body FILE|-]
       atelier todo update ID [--title T] [--type T] [--priority P] [--blocked WHY|none] [--blocker yes|no] [--body FILE|-] [--label L|-L]…
       atelier todo move ID --status S [--board B]
       atelier todo claim ID [--session NAME|ID] [--force] · todo release ID
       atelier todo note ID TEXT · todo check ID N [--undo] · todo check ID --add TEXT
       atelier todo link ID (--pr URL | --note PATH | --session ID | --url URL | --file PATH)
       atelier todo relate ID (--blocked-by ID | --related ID | --duplicate-of ID)
       atelier todo submit ID [--pr URL] [--summary FILE|-]
       atelier todo accept ID [--status backlog|todo|in_progress|in_review] · todo decline ID [--reason TEXT]
       atelier todo done ID · todo cancel ID [--reason TEXT] · todo reopen ID · todo delete ID
       atelier todo board add NAME [--prefix P] [--parent B] · todo board link B [--group NAME|ID]
       atelier todo board unlink B --group NAME|ID · todo board delete B [--with-todos]
       atelier todo import FILE [--board B] [--prefix P] · todo mirror [--out FILE | --set FILE|off] · todo export --canvas FILE [--board B]";

/// The usage line of one verb (`board add`, `check`, …).
fn usage(verb: &str) -> String {
    let line = USAGE.lines().find(|l| l.contains(&format!("todo {verb} ")) || l.trim_end().ends_with(&format!("todo {verb}"))).unwrap_or(USAGE);
    format!("usage: {}", line.trim_start_matches("usage:").trim())
}

/// Longest text file a command reads (bodies, summaries).
const MAX_FILE: u64 = todos::MAX_BODY as u64;
/// Longest file `todo import` reads.
const MAX_IMPORT: u64 = 4 * 1024 * 1024;

pub fn todos_dir(home: &Path, canvas: &str) -> PathBuf {
    crate::canvas_wide::canvas_dir(home, canvas).join("todos")
}

pub fn book_path(home: &Path, canvas: &str) -> PathBuf {
    todos_dir(home, canvas).join("book.json")
}

/// The book as stored (read only; empty when missing or unreadable).
pub fn load(home: &Path, canvas: &str) -> TodoBook {
    try_load(home, canvas).unwrap_or_default()
}

/// The book as stored: empty when there is none yet, an error when the
/// file cannot be read or is not a book this version understands (it must
/// then never be overwritten with an empty one).
pub fn try_load(home: &Path, canvas: &str) -> std::result::Result<TodoBook, String> {
    let path = book_path(home, canvas);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("the todo book {} cannot be read ({e}): fix or move it (is this atelier older than the one that wrote it?)", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TodoBook::default()),
        Err(e) => Err(format!("the todo book {} cannot be read: {e}", path.display())),
    }
}

/// Who runs the command (from the environment the canvas sets).
#[derive(Clone, Debug, Default)]
pub struct Caller {
    /// `ATELIER_SESSION` (empty: you).
    pub session: String,
    /// `AUTOMATION_NAME` outside a session: a scheduled task, such as the TODO bot.
    pub bot: String,
}

impl Caller {
    pub fn from_env() -> Self {
        let env = |name: &str| canvas_common::env::var(name).map(|v| v.trim().to_owned()).unwrap_or_default();
        Caller { session: env("ATELIER_SESSION"), bot: env("AUTOMATION_NAME") }
    }

    /// The caller as the book records it.
    fn actor(&self, routing: &Routing) -> Actor {
        if !self.session.is_empty() {
            let name = routing.passive_by_session(&self.session).map(|p| p.name.clone()).or_else(|| routing.names.get(&self.session).cloned()).unwrap_or_default();
            return Actor::agent(&self.session, &name);
        }
        if self.bot.is_empty() { Actor::human() } else { Actor::bot(&self.bot) }
    }
}

/// Runs `atelier todo WORDS…`: `options` are the `--name value` pairs,
/// `flags` the boolean flags (without `--`).
pub fn run(home: &Path, words: &[&str], options: &[(String, String)], flags: &[String]) -> Result<()> {
    let out = execute(home, words, options, flags, &Caller::from_env(), &mut std::io::stdin().lock())?;
    println!("{out}");
    Ok(())
}

/// A command's options with the global `--canvas FILE` (parsed by the
/// program for every command): `todo export`'s output; other verbs ignore it.
pub fn options_with_canvas(words: &[&str], options: &[(String, String)], canvas: Option<&Path>) -> Vec<(String, String)> {
    let mut options = options.to_vec();
    if words.first() == Some(&"export") {
        options.extend(canvas.map(|c| ("canvas".to_owned(), c.display().to_string())));
    }
    options
}

/// A command's options and flags.
struct Args<'a> {
    verb: String,
    options: &'a [(String, String)],
    flags: &'a [String],
}

impl Args<'_> {
    fn get(&self, name: &str) -> Option<&str> {
        self.options.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.options.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect()
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    /// Refuses options and flags the verb does not take, and repeated ones it takes once.
    fn check(&self, options: &[&str], repeated: &[&str], flags: &[&str]) -> Result<()> {
        for (name, _) in self.options {
            if !options.contains(&name.as_str()) && !repeated.contains(&name.as_str()) {
                bail!("todo {} does not take --{name}\n{}", self.verb, usage(&self.verb));
            }
            if !repeated.contains(&name.as_str()) && self.all(name).len() > 1 {
                bail!("todo {} takes --{name} once\n{}", self.verb, usage(&self.verb));
            }
        }
        if let Some(flag) = self.flags.iter().find(|f| !flags.contains(&f.as_str())) {
            bail!("todo {} does not take --{flag}\n{}", self.verb, usage(&self.verb));
        }
        Ok(())
    }

    /// `--format json|text`.
    fn json(&self) -> Result<bool> {
        match self.get("format").map(str::to_ascii_lowercase).as_deref() {
            None | Some("text") => Ok(false),
            Some("json") => Ok(true),
            Some(other) => bail!("--format is json or text, not {other:?}"),
        }
    }

    fn usage_error(&self, why: &str) -> anyhow::Error {
        anyhow::anyhow!("{why}\n{}", usage(&self.verb))
    }
}

/// A todo id from the command line (`dec-31` → `DEC-31`; the service finds `DEC-031`).
fn todo_id(text: &str) -> Result<String> {
    let text = text.trim();
    todos::parse_id(text).map(|_| text.to_ascii_uppercase()).with_context(|| format!("{text:?} is not a todo id (like DEC-031)"))
}

fn parse_with<T>(value: &str, what: &str, words: &[&str], parse: impl Fn(&str) -> Option<T>) -> Result<T> {
    parse(value).with_context(|| format!("--{what} is one of {}, not {value:?}", words.join(", ")))
}

fn type_words() -> Vec<&'static str> {
    TodoType::ALL.iter().map(|t| t.word()).collect()
}

fn priority_words() -> Vec<&'static str> {
    Priority::ALL.iter().map(|p| p.word()).collect()
}

fn status_words() -> Vec<&'static str> {
    Status::ALL.iter().map(|s| s.word()).collect()
}

/// A text file, or standard input for `-` (UTF-8, at most `max` bytes).
fn read_text(source: &str, what: &str, max: u64, stdin: &mut dyn Read) -> Result<String> {
    let mut bytes = Vec::new();
    if source == "-" {
        stdin.take(max + 1).read_to_end(&mut bytes).with_context(|| format!("cannot read the {what} from standard input"))?;
    } else {
        std::fs::File::open(source).and_then(|f| f.take(max + 1).read_to_end(&mut bytes)).with_context(|| format!("cannot read the {what} file {source}"))?;
    }
    if bytes.len() as u64 > max {
        bail!("the {what} is larger than {} KB: put the details in a note and link it (todo link ID --note PATH)", max / 1024);
    }
    let text = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("the {what} is not UTF-8 text"))?;
    Ok(text.trim_start_matches('\u{feff}').to_owned())
}

/// A path as the service (on this machine) finds it.
fn absolute(path: &str) -> Result<String> {
    let path = path.trim();
    if path.is_empty() {
        bail!("an empty path");
    }
    Ok(std::path::absolute(path).with_context(|| format!("{path:?} is not a usable path"))?.display().to_string())
}

/// A session of the routed canvas named on the command line (its id or
/// name, as `atelier send --to` finds it): (its id, its name).
fn session_of(routing: &Routing, text: &str) -> Result<(String, String)> {
    let text = text.trim();
    if text.is_empty() {
        bail!("--session takes a session's name or id");
    }
    let id = match routing.resolve_from(text, "", "") {
        Ok(crate::bus::Target::Session(id)) => id,
        Ok(crate::bus::Target::Passive(key)) => routing.passive.iter().find(|p| p.key == key).map(|p| p.session_id.clone()).with_context(|| format!("no session {text:?}"))?,
        Err(why) => bail!("{why} on the canvas Atelier has open"),
    };
    let name = routing.passive_by_session(&id).map(|p| p.name.clone()).or_else(|| routing.names.get(&id).cloned()).unwrap_or_else(|| id.clone());
    Ok((id, name))
}
/// The routed canvas, its stored book, and the caller's group.
struct Place {
    routing: Routing,
    canvas: String,
    book: TodoBook,
    /// The caller's canvas group (none for canvas-wide agents: their group is their frame).
    group: Option<String>,
    /// The caller is a canvas-wide agent.
    wide: bool,
}

fn place(home: &Path, caller: &Caller) -> Result<Place> {
    let routing = crate::cli::routing(home);
    if !crate::doc::valid_canvas_id(&routing.canvas) {
        bail!("no canvas with an id is open in Atelier (open it with an up-to-date client)");
    }
    let book = try_load(home, &routing.canvas).map_err(|e| anyhow::anyhow!(e))?;
    let wide = routing.passive_by_session(&caller.session).is_some_and(|p| p.canvas_wide);
    let group = (!caller.session.is_empty() && !wide).then(|| routing.group_of(&caller.session)).flatten();
    Ok(Place { canvas: routing.canvas.clone(), book, group, wide, routing })
}

impl Place {
    fn group_name(&self, id: &str) -> String {
        self.routing.group(id).map(|g| g.name.clone()).unwrap_or_else(|| id.to_owned())
    }

    /// A board named on the command line (id, name, `Parent/Child`, prefix).
    fn board(&self, text: &str) -> Result<&todos::Board> {
        self.book.find_board(text).with_context(|| {
            let names: Vec<&str> = self.book.boards.iter().filter(|b| b.parent.is_none()).map(|b| b.name.as_str()).collect();
            if names.is_empty() { format!("no board {text:?}: this canvas has no todo boards yet") } else { format!("no board {text:?} (boards: {})", names.join(", ")) }
        })
    }

    /// The canvas's top-level boards' names.
    fn board_names(&self) -> Vec<&str> {
        self.book.boards.iter().filter(|b| b.parent.is_none()).map(|b| b.name.as_str()).collect()
    }

    /// The board writes go to without `--board`: the first linked to the
    /// caller's group; outside a group (canvas-wide agents, you, an
    /// automation) the canvas's only board when it has exactly one.
    fn default_board(&self) -> Option<&todos::Board> {
        match self.group.as_deref() {
            Some(group) => self.book.default_board(Some(group)),
            None => match self.book.boards.iter().filter(|b| b.parent.is_none()).collect::<Vec<_>>()[..] {
                [only] => Some(only),
                _ => None,
            },
        }
    }

    /// The board a write goes to: `--board`, else [`Place::default_board`].
    fn write_board(&self, asked: Option<&str>) -> Result<String> {
        if let Some(asked) = asked.map(str::trim).filter(|b| !b.is_empty()) {
            return Ok(self.board(asked)?.id.clone());
        }
        match self.default_board() {
            Some(board) => Ok(board.id.clone()),
            None => {
                let names = self.board_names();
                let why = match self.group {
                    None if self.wide => "you are the canvas-wide agent and the canvas has several todo boards",
                    None => "you are in no canvas group and the canvas has several todo boards",
                    Some(_) => "no todo board is linked to your canvas group",
                };
                if names.is_empty() {
                    bail!("the canvas has no todo boards: create one with atelier todo board add NAME")
                }
                bail!("{why}: name one with --board ({})", names.join(", "))
            }
        }
    }
}

/// The command's output (printed by [`run`]).
pub fn execute(home: &Path, words: &[&str], options: &[(String, String)], flags: &[String], caller: &Caller, stdin: &mut dyn Read) -> Result<String> {
    let verb = match words {
        ["board", second, ..] => format!("board {second}"),
        [first, ..] => (*first).to_owned(),
        [] => bail!("{USAGE}"),
    };
    let args = Args { verb, options, flags };
    match words {
        ["prime"] => {
            args.check(&[], &[], &[])?;
            prime(&place(home, caller)?, caller)
        }
        ["boards"] => {
            args.check(&["format"], &[], &["tree"])?;
            boards(&place(home, caller)?, args.flag("tree"), args.json()?)
        }
        ["list"] => {
            args.check(&["board", "status", "type", "format"], &[], &["mine", "all"])?;
            list(&place(home, caller)?, caller, &args)
        }
        ["show", id] => {
            args.check(&["format"], &[], &[])?;
            show(&place(home, caller)?, &todo_id(id)?, args.json()?)
        }
        ["ready"] => {
            args.check(&["board", "format"], &[], &[])?;
            ready(&place(home, caller)?, args.get("board"), args.json()?)
        }
        ["mirror"] => {
            args.check(&["out", "set"], &[], &[])?;
            mirror(home, caller, &args)
        }
        ["export"] => {
            args.check(&["canvas", "board"], &[], &[])?;
            let file = args.get("canvas").ok_or_else(|| args.usage_error("todo export needs --canvas FILE (where to write the JSON Canvas)"))?;
            let place = place(home, caller)?;
            let canvas = todos::json_canvas(&place.book, args.get("board")).map_err(|e| anyhow::anyhow!(e))?;
            let path = absolute(file)?;
            crate::paths::write_atomic(Path::new(&path), serde_json::to_string_pretty(&canvas)?.as_bytes()).with_context(|| format!("cannot write {path}"))?;
            Ok(format!("Wrote {} node(s) to {path} (JSON Canvas 1.0).", canvas["nodes"].as_array().map_or(0, Vec::len)))
        }
        _ => write(home, words, &args, caller, stdin),
    }
}
/// Why a todo is blocked: its reason and the open todos it waits for.
fn blocked_why(book: &TodoBook, todo: &Todo) -> String {
    let mut why: Vec<String> = todo.blocked.iter().cloned().collect();
    why.extend(book.blockers(todo).iter().map(|b| format!("waits for {} ({})", b.id, b.status.word().replace('_', " "))));
    why.join("; ")
}

/// A todo in one line: `DEC-012  in progress · bug · high  Title  (claimed by X; ⚑ blocked: …)`.
fn line(book: &TodoBook, todo: &Todo, with_board: bool) -> String {
    let mut facts = vec![todo.status.word().replace('_', " "), todo.kind.word().to_owned()];
    if todo.priority != Priority::None {
        facts.push(todo.priority.word().to_owned());
    }
    let mut notes = Vec::new();
    if with_board {
        notes.push(format!("board {}", board_label(book, &todo.board)));
    }
    if let Some(claim) = &todo.claimed_by {
        notes.push(format!("claimed by {}", claim.by.label()));
    }
    if todo.blocker {
        notes.push("‼ blocker".into());
    }
    if book.is_blocked(todo) {
        notes.push(format!("⚑ blocked: {}", blocked_why(book, todo)));
    }
    let (done, all) = todo.checked();
    if all > 0 {
        notes.push(format!("☑ {done}/{all}"));
    }
    format!("  {:<8} {}  {}{}", todo.id, facts.join(" · "), todo.title, if notes.is_empty() { String::new() } else { format!("  ({})", notes.join("; ")) })
}

/// `Decomps` or `Decomps › Parser`.
fn board_label(book: &TodoBook, board: &str) -> String {
    match book.board(board) {
        Some(b) => match b.parent.as_deref().and_then(|p| book.board(p)) {
            Some(parent) => format!("{} › {}", parent.name, b.name),
            None => b.name.clone(),
        },
        None => board.to_owned(),
    }
}

/// A todo for `--format json`: as stored, with what the book derives.
fn todo_json(book: &TodoBook, todo: &Todo) -> serde_json::Value {
    let mut value = serde_json::to_value(todo).unwrap_or_default();
    value["board_name"] = board_label(book, &todo.board).into();
    value["is_blocked"] = book.is_blocked(todo).into();
    value["is_ready"] = book.is_ready(todo).into();
    value
}

fn pretty(value: serde_json::Value) -> Result<String> {
    Ok(serde_json::to_string_pretty(&value)?)
}

/// (open, in progress, proposed, done) of a board and its swimlanes.
fn counts(book: &TodoBook, board: &str) -> (usize, usize, usize, usize) {
    let todos: Vec<&Todo> = book.todos_in(board).collect();
    let count = |s: Status| todos.iter().filter(|t| t.status == s).count();
    (todos.iter().filter(|t| t.status.open()).count(), count(Status::InProgress), count(Status::Proposed), count(Status::Done))
}

fn policy_words(board: &todos::Board) -> String {
    let adds = if board.policy.agent_adds == AddPolicy::Proposed { "agents' todos become proposals" } else { "agents add todos" };
    let close = if board.policy.agents_may_close { "agents may close todos" } else { "closing is the user's" };
    format!("{adds}; {close}")
}

/// `todo prime`: what an agent needs to work with the boards of its group.
fn prime(place: &Place, caller: &Caller) -> Result<String> {
    let book = &place.book;
    let me = caller.actor(&place.routing);
    let mut out = Vec::new();
    out.push(match (&me.session, place.group.as_deref()) {
        (Some(_), _) if place.wide => format!("Atelier todos · you are the canvas-wide agent {:?}: every board of the canvas is yours to read.", me.label()),
        (Some(_), Some(group)) => format!("Atelier todos · you are {:?} in the canvas group {:?}.", me.label(), place.group_name(group)),
        (Some(_), None) => format!("Atelier todos · you are {:?}, in no canvas group.", me.label()),
        (None, _) => format!("Atelier todos · you run outside a canvas session: changes are made as {}.", if caller.bot.is_empty() { "you (the user)".to_owned() } else { format!("the automation {:?}", caller.bot) }),
    });
    let linked: Vec<&todos::Board> = match place.group.as_deref() {
        _ if place.wide => book.boards.iter().filter(|b| b.parent.is_none()).collect(),
        Some(group) => book.boards_of_group(group),
        None => Vec::new(),
    };
    if book.boards.is_empty() {
        out.push("\nThis canvas has no todo boards yet. When the user wants one: atelier todo board add NAME (it is linked to your group).".into());
    } else if linked.is_empty() {
        let names: Vec<String> = book.boards.iter().filter(|b| b.parent.is_none()).map(|b| format!("{} ({})", b.name, b.prefix)).collect();
        match place.default_board().filter(|_| place.group.is_none()) {
            Some(board) => out.push(format!("\nBoards on this canvas: {}. Default board for todo add and todo propose: {} (the only one).", names.join(", "), board.name)),
            None if place.group.is_none() => out.push(format!("\nBoards on this canvas: {}. Name one with --board when you add or propose.", names.join(", "))),
            None => out.push(format!("\nNo board is linked to your group. Boards on this canvas: {}. Name one with --board, or ask the user to link one (atelier todo board link B).", names.join(", "))),
        }
    } else {
        out.push(if place.wide { "\nBoards of the canvas:".into() } else { "\nBoards linked to your group:".to_owned() });
        for board in &linked {
            let (open, in_progress, proposed, done) = counts(book, &board.id);
            let subs: Vec<String> = book.sub_boards(&board.id).iter().map(|b| b.name.clone()).collect();
            let root = book.root(&board.id).unwrap_or(board);
            out.push(format!(
                "  {} ({}){} · {open} open, {in_progress} in progress, {proposed} proposed, {done} done · {}",
                board_label(book, &board.id),
                board.prefix,
                if subs.is_empty() { String::new() } else { format!(" · swimlanes: {}", subs.join(", ")) },
                policy_words(root)
            ));
        }
        match place.default_board() {
            Some(board) => out.push(format!("Default board for todo add and todo propose: {}.", board_label(book, &board.id))),
            None => out.push(format!("Name the board with --board when you add or propose ({}).", place.board_names().join(", "))),
        }
    }
    let mine: Vec<&Todo> = book.todos.iter().filter(|t| !t.status.closed() && t.claimed_by.as_ref().is_some_and(|c| c.by.same(&me))).collect();
    if !mine.is_empty() {
        out.push("\nYour claims:".into());
        out.extend(mine.iter().map(|t| line(book, t, true)));
    }
    let ready = book.ready_for_group(place.group.as_deref());
    if !ready.is_empty() {
        out.push(format!("\nReady ({} in all; atelier todo ready lists them):", ready.len()));
        out.extend(ready.iter().take(5).map(|t| line(book, t, true)));
    }
    out.push(format!(
        "\nStatuses: proposed → backlog → todo → in_progress → in_review → done (or canceled).\nTypes: {}. Priorities: {}.",
        type_words().join(", "),
        priority_words().join(", ")
    ));
    out.push(
        "\nHow to work:
1. atelier todo ready → todo show ID → todo claim ID (never one another session claimed).
2. As you go: todo note ID \"what you found or decided\" · todo check ID N (checklist item N is done).
3. Finished: todo submit ID --pr URL [--summary FILE] puts it in review. Done is the user's (or a merged pull request's): never run todo done.
4. New work: todo add \"TITLE\" --type T when the user asked for it; todo propose \"TITLE\" --type T --why \"…\" for your own ideas; --discovered-from ID for work found while doing another todo.
5. Blocked: todo update ID --blocked \"why\" (--blocked none when it goes on) or todo relate ID --blocked-by OTHER. Stopping: todo note, then todo release ID.
Never edit TODOs.md mirrors or the book's files: change todos only with atelier todo.
Commands: todo boards · todo list [--board B] [--status S] [--mine] · todo show ID · todo ready · todo add · todo propose · todo update · todo move ID --status S · todo claim / release ID · todo note ID TEXT · todo check ID N · todo link ID --pr URL · todo relate ID --blocked-by ID · todo submit ID --pr URL (atelier --help lists every option)."
            .into(),
    );
    Ok(out.join("\n"))
}

/// `todo boards [--tree]`.
fn boards(place: &Place, tree: bool, json: bool) -> Result<String> {
    let book = &place.book;
    if json {
        let boards: Vec<serde_json::Value> = book
            .boards
            .iter()
            .map(|b| {
                let (open, in_progress, proposed, done) = counts(book, &b.id);
                let mut value = serde_json::to_value(b).unwrap_or_default();
                value["group_names"] = b.canvas_groups.iter().map(|g| place.group_name(g)).collect::<Vec<_>>().into();
                value["counts"] = json!({ "open": open, "in_progress": in_progress, "proposed": proposed, "done": done });
                value["ready"] = book.ready(Some(&b.id)).len().into();
                value
            })
            .collect();
        return pretty(json!({ "schemaVersion": SCHEMA_VERSION, "kind": "todo-boards", "canvas": place.canvas, "boards": boards, "mirror": book.settings.mirror }));
    }
    if book.boards.is_empty() {
        return Ok("This canvas has no todo boards yet (atelier todo board add NAME).".into());
    }
    let mut out = vec![format!("Todo boards of canvas {}:", place.canvas)];
    for board in book.boards.iter().filter(|b| b.parent.is_none()) {
        let (open, in_progress, proposed, done) = counts(book, &board.id);
        let groups: Vec<String> = board.canvas_groups.iter().map(|g| place.group_name(g)).collect();
        let subs = book.sub_boards(&board.id);
        out.push(format!(
            "  {} ({}) · {open} open, {in_progress} in progress, {proposed} proposed, {done} done · {}{}{} · {}",
            board.name,
            board.prefix,
            if groups.is_empty() { "no canvas group".to_owned() } else { format!("groups: {}", groups.join(", ")) },
            board.wip.map(|w| format!(" · WIP {w}")).unwrap_or_default(),
            if tree || subs.is_empty() { String::new() } else { format!(" · swimlanes: {}", subs.iter().map(|b| b.name.as_str()).collect::<Vec<_>>().join(", ")) },
            policy_words(board)
        ));
        if tree {
            for sub in subs {
                let (open, in_progress, _, done) = counts(book, &sub.id);
                let groups: Vec<String> = sub.canvas_groups.iter().map(|g| place.group_name(g)).collect();
                out.push(format!("    └ {} · {open} open, {in_progress} in progress, {done} done{}", sub.name, if groups.is_empty() { String::new() } else { format!(" · groups: {}", groups.join(", ")) }));
            }
        }
    }
    if let Some(mirror) = &book.settings.mirror {
        out.push(format!("Mirrored to {mirror} (generated: do not edit)."));
    }
    Ok(out.join("\n"))
}

/// `todo list`: open todos of the caller's group's boards (every board when
/// it has none), `--board`, `--status`, `--type`, `--mine`; `--all`: every
/// board and the done and canceled ones too.
fn list(place: &Place, caller: &Caller, args: &Args) -> Result<String> {
    let book = &place.book;
    let json = args.json()?;
    let status = args.get("status").map(|s| parse_with(s, "status", &status_words(), Status::parse)).transpose()?;
    let kind = args.get("type").map(|t| parse_with(t, "type", &type_words(), TodoType::parse)).transpose()?;
    let all = args.flag("all");
    let (scope, what): (Option<Vec<String>>, String) = match args.get("board") {
        Some(text) => {
            let board = place.board(text)?;
            (Some(book.lane_ids(&board.id)), format!(" on {}", board_label(book, &board.id)))
        }
        None if all => (None, String::new()),
        None => match place.group.as_deref().map(|g| book.group_board_ids(g)).filter(|ids| !ids.is_empty()) {
            Some(ids) => (Some(ids.into_iter().collect()), " on your group's boards".into()),
            None => (None, String::new()),
        },
    };
    let me = caller.actor(&place.routing);
    let order = |s: Status| Status::ALL.iter().position(|x| *x == s).unwrap_or(0);
    let board_order = |b: &str| book.boards.iter().position(|x| x.id == b || Some(x.id.as_str()) == book.board(b).and_then(|y| y.parent.as_deref())).unwrap_or(usize::MAX);
    let mut todos: Vec<&Todo> = book
        .todos
        .iter()
        .filter(|t| scope.as_ref().is_none_or(|s| s.contains(&t.board)))
        .filter(|t| match status {
            Some(s) => t.status == s,
            None => all || !t.status.closed(),
        })
        .filter(|t| kind.is_none_or(|k| t.kind == k))
        .filter(|t| !args.flag("mine") || t.claimed_by.as_ref().is_some_and(|c| c.by.same(&me)))
        .collect();
    todos.sort_by(|a, b| board_order(&a.board).cmp(&board_order(&b.board)).then(order(a.status).cmp(&order(b.status))).then(a.rank.cmp(&b.rank)).then(a.id.cmp(&b.id)));
    if json {
        return pretty(json!({ "schemaVersion": SCHEMA_VERSION, "kind": "todo-list", "todos": todos.iter().map(|t| todo_json(book, t)).collect::<Vec<_>>() }));
    }
    if todos.is_empty() {
        return Ok(format!("No todos{what}{}.", if all || status.is_some() { "" } else { " that are open (--all shows done and canceled ones)" }));
    }
    let many_boards = todos.iter().map(|t| book.root(&t.board).map(|r| r.id.as_str()).unwrap_or_default()).collect::<std::collections::BTreeSet<_>>().len() > 1;
    let mut out = vec![format!("{} todo(s){what}:", todos.len())];
    out.extend(todos.iter().map(|t| line(book, t, many_boards || book.board(&t.board).is_some_and(|b| b.parent.is_some()))));
    Ok(out.join("\n"))
}
/// `todo show ID`.
fn show(place: &Place, id: &str, json: bool) -> Result<String> {
    let book = &place.book;
    let todo = book.todo(id).with_context(|| format!("no todo {id} on this canvas (atelier todo list --all lists them)"))?;
    let blocking: Vec<&Todo> = book.blocking(&todo.id);
    if json {
        let mut value = json!({ "schemaVersion": SCHEMA_VERSION, "kind": "todo", "todo": todo_json(book, todo) });
        value["blockers"] = book.blockers(todo).iter().map(|t| t.id.clone()).collect::<Vec<_>>().into();
        value["blocking"] = blocking.iter().map(|t| t.id.clone()).collect::<Vec<_>>().into();
        return pretty(value);
    }
    let date = |ms: u64| crate::paths::format_rfc3339_ms(ms).chars().take(16).collect::<String>().replace('T', " ");
    let mut facts = vec![format!("board {}", board_label(book, &todo.board)), todo.status.word().replace('_', " "), todo.kind.word().to_owned()];
    if todo.priority != Priority::None {
        facts.push(todo.priority.word().to_owned());
    }
    if let Some(resolution) = todo.resolution {
        facts.push(format!("resolution {}", resolution.word().replace('_', " ")));
    }
    if todo.blocker {
        facts.push("‼ blocker".into());
    }
    let mut out = vec![format!("{} · {}", todo.id, todo.title), format!("  {}", facts.join(" · "))];
    let mut who = vec![format!("created by {} {}", todo.created_by.label(), date(todo.created_ms))];
    if let Some(claim) = &todo.claimed_by {
        who.insert(0, format!("claimed by {} since {}", claim.by.label(), date(claim.ms)));
    }
    if let Some(done) = todo.done_ms {
        who.push(format!("closed {}", date(done)));
    }
    out.push(format!("  {}", who.join(" · ")));
    if book.is_blocked(todo) {
        out.push(format!("  ⚑ blocked: {}", blocked_why(book, todo)));
    } else if book.is_ready(todo) {
        out.push("  ready: nothing blocks it and nobody claimed it".into());
    }
    if !blocking.is_empty() {
        out.push(format!("  blocks: {}", blocking.iter().map(|t| t.id.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if let Some(proposal) = &todo.proposal {
        out.push(format!("  proposed because: {}", proposal.why));
    }
    if let Some(parent) = todo.parent.as_deref() {
        out.push(format!("  parent: {parent}{}", book.todo(parent).map(|p| format!(" {}", p.title)).unwrap_or_default()));
    }
    if !todo.labels.is_empty() {
        out.push(format!("  labels: {}", todo.labels.join(", ")));
    }
    if let Some(note) = &todo.note {
        out.push(format!("  spec note: {note}"));
    }
    if !todo.checklist.is_empty() {
        let (done, all) = todo.checked();
        out.push(format!("Checklist ({done}/{all}):"));
        out.extend(todo.checklist.iter().enumerate().map(|(i, c)| format!("  {}. [{}] {}", i + 1, if c.done { "x" } else { " " }, c.text)));
    }
    if !todo.relations.is_empty() {
        out.push("Relations:".into());
        out.extend(todo.relations.iter().map(|r| format!("  {} {}{}", r.kind.label(), r.id, book.todo(&r.id).map(|t| format!(" {} ({})", t.title, t.status.word().replace('_', " "))).unwrap_or_default())));
    }
    if !todo.links.is_empty() {
        out.push("Links:".into());
        out.extend(todo.links.iter().map(|l| format!("  {} {}{}", l.kind.word(), l.target, if l.title.is_empty() || l.title == l.target { String::new() } else { format!(" ({})", l.title) })));
    }
    if !todo.body.is_empty() {
        out.push("Body:".into());
        out.extend(todo.body.lines().map(|l| format!("  {l}")));
    }
    if !todo.log.is_empty() {
        const SHOWN: usize = 15;
        out.push(format!("Log{}:", if todo.log.len() > SHOWN { format!(" (the last {SHOWN} of {})", todo.log.len()) } else { String::new() }));
        for entry in todo.log.iter().skip(todo.log.len().saturating_sub(SHOWN)) {
            let kind = if entry.kind == todos::LogKind::Note { "note: " } else { "" };
            out.push(format!("  {} · {} · {kind}{}", date(entry.ms), entry.by.label(), entry.text.replace('\n', "\n      ")));
        }
    }
    Ok(out.join("\n"))
}

/// `todo ready [--board B]`: of the caller's group's boards (every board when it has none).
fn ready(place: &Place, board: Option<&str>, json: bool) -> Result<String> {
    let book = &place.book;
    let (list, what) = match board {
        Some(text) => {
            let board = place.board(text)?;
            (book.ready(Some(&board.id)), format!(" on {}", board_label(book, &board.id)))
        }
        None => (book.ready_for_group(place.group.as_deref()), String::new()),
    };
    if json {
        return pretty(json!({ "schemaVersion": SCHEMA_VERSION, "kind": "todo-ready", "todos": list.iter().map(|t| todo_json(book, t)).collect::<Vec<_>>() }));
    }
    if list.is_empty() {
        return Ok(format!("Nothing is ready{what} (ready: status todo, not blocked, not claimed). atelier todo list shows the rest."));
    }
    let mut out = vec![format!("Ready to work on{what}, most urgent first (claim one with atelier todo claim ID):")];
    out.extend(list.iter().map(|t| line(book, t, true)));
    Ok(out.join("\n"))
}

/// `todo mirror`: the book in the TODOs.md format (printed, or `--out FILE`);
/// `--set FILE|off`: the file the service keeps rewriting.
fn mirror(home: &Path, caller: &Caller, args: &Args) -> Result<String> {
    if let Some(set) = args.get("set") {
        if args.get("out").is_some() {
            return Err(args.usage_error("todo mirror takes --out or --set, not both"));
        }
        let off = ["off", "none", ""].contains(&set.trim().to_ascii_lowercase().as_str());
        let path = if off { None } else { Some(absolute(set)?) };
        let pending = "The mirror setting is handed to Atelier; the service is not running (or busy): it applies it when it takes the request.";
        return send(home, caller, TodoOp::SetMirror { path }, false, pending);
    }
    let place = place(home, caller)?;
    let text = todos::mirror_markdown(&place.book);
    match args.get("out") {
        Some(out) => {
            let path = absolute(out)?;
            crate::paths::write_atomic(Path::new(&path), text.as_bytes()).with_context(|| format!("cannot write {path}"))?;
            Ok(format!("Wrote the {} todo(s) to {path} (a snapshot: atelier todo mirror --set FILE keeps a file up to date).", place.book.todos.len()))
        }
        None => Ok(text),
    }
}

/// Posts an operation for the service and says what came of it.
fn send(home: &Path, caller: &Caller, op: TodoOp, json: bool, pending: &str) -> Result<String> {
    let mut data = serde_json::to_value(&op)?;
    if caller.session.is_empty() && !caller.bot.is_empty() {
        data["bot"] = caller.bot.clone().into();
    }
    answered(ask_from(home, REQUEST, &caller.session, data)?, json, pending)
}

/// The service's answer as output (a refusal is an error: the command exits non-zero).
fn answered(answer: Option<Answer>, json: bool, pending: &str) -> Result<String> {
    match answer {
        Some(answer) if answer.ok => match (json, answer.data) {
            (true, Some(data)) => pretty(data),
            (true, None) => pretty(json!({ "schemaVersion": SCHEMA_VERSION, "kind": "todo-change", "ok": true, "message": answer.message })),
            (false, _) => Ok(answer.message),
        },
        Some(answer) => bail!("Atelier refused: {}", answer.message),
        None if json => pretty(json!({ "schemaVersion": SCHEMA_VERSION, "kind": "todo-change", "ok": null, "queued": true, "message": pending })),
        None => Ok(pending.to_owned()),
    }
}
/// The changes: checked here, applied by the service.
fn write(home: &Path, words: &[&str], args: &Args, caller: &Caller, stdin: &mut dyn Read) -> Result<String> {
    let id_of = |id: &str| todo_id(id).map_err(|e| args.usage_error(&e.to_string()));
    let text_of = |words: &[&str]| words.join(" ").trim().to_owned();
    let priority = || args.get("priority").map(|p| parse_with(p, "priority", &priority_words(), Priority::parse)).transpose();
    let kind = |required: bool| -> Result<Option<TodoType>> {
        match args.get("type") {
            Some(t) => parse_with(t, "type", &type_words(), TodoType::parse).map(Some),
            None if required => Err(args.usage_error(&format!("todo {} needs --type ({})", args.verb, type_words().join(", ")))),
            None => Ok(None),
        }
    };
    let body = |stdin: &mut dyn Read| args.get("body").map(|b| read_text(b, "body", MAX_FILE, stdin)).transpose();
    let one_of = |names: &[&str]| -> Result<(String, String)> {
        let given: Vec<(String, String)> = names.iter().filter_map(|n| args.get(n).map(|v| ((*n).to_owned(), v.to_owned()))).collect();
        match given.as_slice() {
            [one] => Ok(one.clone()),
            _ => Err(args.usage_error(&format!("todo {} needs exactly one of --{}", args.verb, names.join(", --")))),
        }
    };
    let (op, what): (TodoOp, String) = match words {
        ["add", title @ ..] | ["propose", title @ ..] => {
            let propose = words[0] == "propose";
            if propose {
                args.check(&["type", "why", "board", "discovered-from", "priority", "body", "format"], &[], &[])?;
            } else {
                args.check(&["type", "board", "priority", "status", "body", "parent", "format"], &["label", "check"], &[])?;
            }
            let title = text_of(title);
            if title.is_empty() {
                return Err(args.usage_error(&format!("todo {} needs a title", args.verb)));
            }
            let kind = kind(true)?.unwrap_or_default();
            let status = match args.get("status") {
                Some(s) => match Status::parse(s) {
                    Some(s @ (Status::Backlog | Status::Todo)) => Some(s),
                    _ => bail!("--status is backlog or todo, not {s:?} (move it later with atelier todo move ID --status S)"),
                },
                None => None,
            };
            let why = args.get("why").map(str::trim).filter(|w| !w.is_empty()).map(str::to_owned);
            if propose && why.is_none() {
                return Err(args.usage_error("todo propose needs --why: say why it is worth doing"));
            }
            let checklist: Vec<String> = args.all("check").iter().map(|c| c.trim().to_owned()).collect();
            if checklist.iter().any(String::is_empty) {
                bail!("--check takes the text of a checklist item");
            }
            let place = place(home, caller)?;
            let new = NewTodo {
                board: place.write_board(args.get("board"))?,
                title: title.clone(),
                kind,
                priority: priority()?,
                status,
                body: body(stdin)?.unwrap_or_default(),
                parent: args.get("parent").map(id_of).transpose()?,
                labels: args.all("label").iter().map(|l| (*l).to_owned()).collect(),
                checklist,
                propose,
                why,
                discovered_from: args.get("discovered-from").map(id_of).transpose()?,
                ..Default::default()
            };
            (TodoOp::Add(Box::new(new)), format!("The todo “{title}”"))
        }
        ["update", id] => {
            args.check(&["title", "type", "priority", "blocked", "blocker", "body", "format"], &["label"], &[])?;
            let mut update = TodoUpdate { id: id_of(id)?, ..Default::default() };
            if let Some(title) = args.get("title") {
                if title.trim().is_empty() {
                    bail!("--title takes the new title");
                }
                update.title = Some(title.to_owned());
            }
            update.kind = kind(false)?;
            update.priority = priority()?;
            update.blocked = args.get("blocked").map(|b| if b.trim().is_empty() { "none".to_owned() } else { b.to_owned() });
            update.blocker = match args.get("blocker").map(|b| b.trim().to_ascii_lowercase()) {
                None => None,
                Some(b) if ["yes", "true", "on"].contains(&b.as_str()) => Some(true),
                Some(b) if ["no", "false", "off"].contains(&b.as_str()) => Some(false),
                Some(b) => bail!("--blocker is yes or no, not {b:?}"),
            };
            update.body = body(stdin)?;
            for label in args.all("label") {
                match label.strip_prefix('-') {
                    Some(gone) => update.remove_labels.push(gone.to_owned()),
                    None => update.add_labels.push(label.to_owned()),
                }
            }
            if update == (TodoUpdate { id: update.id.clone(), ..Default::default() }) {
                return Err(args.usage_error("todo update needs something to change"));
            }
            let what = format!("The update of {}", update.id);
            (TodoOp::Update(Box::new(update)), what)
        }
        ["move", id] => {
            args.check(&["status", "board", "format"], &[], &[])?;
            let status = args.get("status").ok_or_else(|| args.usage_error("todo move needs --status S"))?;
            let status = parse_with(status, "status", &status_words(), Status::parse)?;
            let board = match args.get("board") {
                Some(b) => Some(place(home, caller)?.board(b)?.id.clone()),
                None => None,
            };
            let id = id_of(id)?;
            (TodoOp::Move { id: id.clone(), status: Some(status), board, after: None, before: None, rank: None }, format!("Moving {id}"))
        }
        ["claim", id] => {
            args.check(&["session", "format"], &[], &["force"])?;
            let id = id_of(id)?;
            let session = match args.get("session") {
                Some(_) if !caller.session.is_empty() => bail!("agents claim todos only for themselves: run todo claim {id} without --session (ask the user to assign it to another session)"),
                Some(text) => Some(session_of(&place(home, caller)?.routing, text)?),
                None => None,
            };
            let what = match &session {
                Some((_, name)) => format!("The claim of {id} for {name}"),
                None => format!("The claim of {id}"),
            };
            (TodoOp::Claim { id, force: args.flag("force"), session: session.map(|(id, _)| id) }, what)
        }
        ["release", id] => {
            args.check(&["format"], &[], &[])?;
            let id = id_of(id)?;
            (TodoOp::Release { id: id.clone() }, format!("Releasing {id}"))
        }
        ["note", id, text @ ..] => {
            args.check(&["format"], &[], &[])?;
            let text = text_of(text);
            if text.is_empty() {
                return Err(args.usage_error("todo note needs the text of the note"));
            }
            let id = id_of(id)?;
            (TodoOp::Note { id: id.clone(), text }, format!("The note on {id}"))
        }
        ["check", id, rest @ ..] => {
            args.check(&["add", "format"], &[], &["undo"])?;
            let id = id_of(id)?;
            match (rest, args.get("add")) {
                ([], Some(text)) if !args.flag("undo") => {
                    if text.trim().is_empty() {
                        bail!("--add takes the text of the checklist item");
                    }
                    (TodoOp::AddCheck { id: id.clone(), text: text.to_owned() }, format!("The checklist item for {id}"))
                }
                ([n], None) => {
                    let n: usize = n.parse().ok().filter(|n| *n >= 1).ok_or_else(|| args.usage_error(&format!("{n:?} is not a checklist item number (1, 2, …)")))?;
                    (TodoOp::Check { id: id.clone(), index: n - 1, done: !args.flag("undo") }, format!("Checking item {n} of {id}"))
                }
                _ => return Err(args.usage_error("todo check needs an item number (N), or --add TEXT")),
            }
        }
        ["link", id] => {
            args.check(&["pr", "note", "session", "url", "file", "format"], &[], &[])?;
            let (option, target) = one_of(&["pr", "note", "session", "url", "file"])?;
            let kind = LinkKind::parse(&option).unwrap_or(LinkKind::Url);
            let target = match kind {
                LinkKind::Note | LinkKind::File => absolute(&target)?,
                LinkKind::Pr => canvas_model::prs::PrRef::parse(&target).map_err(|e| anyhow::anyhow!(e))?.url(),
                // A session's name or id: its id (what the window opens).
                LinkKind::Session => session_of(&place(home, caller)?.routing, &target)?.0,
                _ => target,
            };
            let id = id_of(id)?;
            (TodoOp::Link { id: id.clone(), kind, target, title: None }, format!("The link on {id}"))
        }
        ["relate", id] => {
            args.check(&["blocked-by", "related", "duplicate-of", "format"], &[], &[])?;
            let (option, other) = one_of(&["blocked-by", "related", "duplicate-of"])?;
            let kind = RelKind::parse(&option).unwrap_or(RelKind::Related);
            let id = id_of(id)?;
            (TodoOp::Relate { id: id.clone(), kind, other: id_of(&other)? }, format!("The relation of {id}"))
        }
        ["submit", id] => {
            args.check(&["pr", "summary", "format"], &[], &[])?;
            let pr = args.get("pr").map(|p| canvas_model::prs::PrRef::parse(p).map(|p| p.url()).map_err(|e| anyhow::anyhow!(e))).transpose()?;
            let summary = args.get("summary").map(|s| read_text(s, "summary", todos::MAX_TEXT as u64, stdin)).transpose()?;
            let id = id_of(id)?;
            (TodoOp::Submit { id: id.clone(), pr, summary }, format!("Submitting {id}"))
        }
        ["accept", id] => {
            args.check(&["status", "format"], &[], &[])?;
            let status = args.get("status").map(|s| parse_with(s, "status", &["backlog", "todo", "in_progress", "in_review"], |s| Status::parse(s).filter(|s| s.open()))).transpose()?;
            let id = id_of(id)?;
            (TodoOp::Accept { id: id.clone(), status }, format!("Accepting {id}"))
        }
        ["decline", id] | ["cancel", id] => {
            args.check(&["reason", "format"], &[], &[])?;
            let id = id_of(id)?;
            let reason = args.get("reason").map(str::to_owned);
            if words[0] == "decline" {
                (TodoOp::Decline { id: id.clone(), reason }, format!("Declining {id}"))
            } else {
                (TodoOp::Cancel { id: id.clone(), reason, resolution: None }, format!("Canceling {id}"))
            }
        }
        ["done", id] | ["reopen", id] | ["delete", id] => {
            args.check(&["format"], &[], &[])?;
            let id = id_of(id)?;
            match words[0] {
                "done" => (TodoOp::Done { id: id.clone(), note: None }, format!("Marking {id} done")),
                "reopen" => (TodoOp::Reopen { id: id.clone() }, format!("Reopening {id}")),
                _ => (TodoOp::Delete { id: id.clone() }, format!("Deleting {id}")),
            }
        }
        ["board", "add", name @ ..] => {
            args.check(&["prefix", "parent", "format"], &[], &[])?;
            let name = text_of(name);
            if name.is_empty() {
                return Err(args.usage_error("todo board add needs the board's name"));
            }
            if let Some(prefix) = args.get("prefix").filter(|p| !todos::valid_prefix(&p.trim().to_ascii_uppercase())) {
                bail!("--prefix is 2 to 5 letters A-Z (like DEC), not {prefix:?}");
            }
            let parent = match args.get("parent") {
                Some(p) => Some(place(home, caller)?.board(p)?.id.clone()),
                None => None,
            };
            let op = TodoOp::AddBoard { name: name.clone(), prefix: args.get("prefix").map(str::to_owned), parent, groups: Vec::new(), x: None, y: None };
            (op, format!("The board {name:?}"))
        }
        ["board", "link", board] => {
            args.check(&["group", "format"], &[], &[])?;
            let place = place(home, caller)?;
            let board = place.board(board)?.id.clone();
            let group = match (args.get("group"), place.group.clone()) {
                (Some(text), _) => {
                    let text = text.trim();
                    place.routing.group(text).or_else(|| place.routing.groups.iter().find(|g| g.name.eq_ignore_ascii_case(text))).map(|g| g.id.clone()).with_context(|| {
                        format!("no canvas group {text:?} (groups: {})", place.routing.groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>().join(", "))
                    })?
                }
                (None, Some(group)) => group,
                (None, None) => return Err(args.usage_error("name the canvas group with --group NAME|ID (you are in no group)")),
            };
            (TodoOp::LinkBoard { board: board.clone(), group, unlink: false }, format!("Linking the board {board}"))
        }
        ["board", "unlink", board] => {
            args.check(&["group", "format"], &[], &[])?;
            let place = place(home, caller)?;
            let found = place.board(board)?;
            let text = args.get("group").map(str::trim).filter(|g| !g.is_empty()).ok_or_else(|| args.usage_error("todo board unlink needs --group NAME|ID (the canvas group to unlink)"))?;
            // A group of the canvas, or one the board still names that left the canvas.
            let group = place.routing.group(text).or_else(|| place.routing.groups.iter().find(|g| g.name.eq_ignore_ascii_case(text))).map(|g| g.id.clone()).or_else(|| found.canvas_groups.iter().find(|g| *g == text).cloned());
            let group = group.with_context(|| {
                let linked: Vec<String> = found.canvas_groups.iter().map(|g| place.group_name(g)).collect();
                format!("no canvas group {text:?} (the board is linked to: {})", if linked.is_empty() { "none".to_owned() } else { linked.join(", ") })
            })?;
            let board = found.id.clone();
            (TodoOp::LinkBoard { board: board.clone(), group, unlink: true }, format!("Unlinking the board {board}"))
        }
        ["board", "delete", board] => {
            args.check(&["format"], &[], &["with-todos"])?;
            let board = place(home, caller)?.board(board)?.id.clone();
            (TodoOp::DeleteBoard { board: board.clone(), with_todos: args.flag("with-todos") }, format!("Deleting the board {board}"))
        }
        ["import", file] => {
            args.check(&["board", "prefix", "format"], &[], &[])?;
            let text = read_text(file, "file", MAX_IMPORT, stdin)?;
            if let Some(prefix) = args.get("prefix").filter(|p| !todos::valid_prefix(&p.trim().to_ascii_uppercase())) {
                bail!("--prefix is 2 to 5 letters A-Z (like WI), not {prefix:?}");
            }
            let board = match args.get("board").map(str::trim).filter(|b| !b.is_empty()) {
                Some(board) => board.to_owned(),
                None => place(home, caller)?.write_board(None).map_err(|_| args.usage_error("name the board to import into with --board NAME (it is created when missing)"))?,
            };
            if todos::import_markdown(&text, "", "").todos.is_empty() {
                bail!("{file} has no todo items (lines like - [ ] **WI-019: [[WI-019 - Title]]** - text)");
            }
            (TodoOp::Import { board: board.clone(), prefix: args.get("prefix").map(str::to_owned), text }, format!("The import into {board:?}"))
        }
        _ => bail!("{USAGE}"),
    };
    let json = args.json()?;
    let pending = format!("{what} is handed to Atelier; the service is not running (or busy): it is applied when the service takes it (atelier todo list shows the book).");
    send(home, caller, op, json, &pending)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{self, Event, RouteGroup};

    fn opts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn flags(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// A home whose routed canvas `c1` has the groups Pay (g1, session s1) and
    /// Docs (g2, session s2), and a book: Decomps (DEC, linked to Pay) with a
    /// Parser swimlane and three todos.
    fn home() -> PathBuf {
        let home = std::env::temp_dir().join(format!("cc-todos-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(home.join("bus")).unwrap();
        let routing = Routing {
            canvas: "c1".into(),
            groups: vec![RouteGroup { id: "g1".into(), name: "Pay".into(), folder: None }, RouteGroup { id: "g2".into(), name: "Docs".into(), folder: None }],
            members: [("s1".to_owned(), "g1".to_owned()), ("s2".to_owned(), "g2".to_owned())].into(),
            names: [("s1".to_owned(), "Fix flaky test".to_owned()), ("s2".to_owned(), "Write docs".to_owned()), ("w".to_owned(), "Coordinator".to_owned())].into(),
            passive: vec![bus::PassiveSpec { key: "kw".into(), session_id: "w".into(), name: "Coordinator".into(), group: "frame".into(), canvas_wide: true, ..Default::default() }],
            ..Default::default()
        };
        std::fs::write(home.join("bus").join("routing.json"), serde_json::to_string(&routing).unwrap()).unwrap();
        let mut book = TodoBook::default();
        let ctx = todos::Ctx { now: 1_793_000_000_000, group: Some("g1".into()), groups: [("g1".to_owned(), "Pay".to_owned())].into(), ..Default::default() };
        let you = Actor::human();
        let ops = [
            TodoOp::AddBoard { name: "Decomps".into(), prefix: None, parent: None, groups: vec!["g1".into()], x: None, y: None },
            TodoOp::AddBoard { name: "Parser".into(), prefix: None, parent: Some("Decomps".into()), groups: vec![], x: None, y: None },
            TodoOp::Add(Box::new(NewTodo { board: "Decomps".into(), title: "Crash on load".into(), kind: TodoType::Bug, priority: Some(Priority::High), status: Some(Status::Todo), checklist: vec!["repro".into()], ..Default::default() })),
            TodoOp::Add(Box::new(NewTodo { board: "Parser".into(), title: "String table".into(), status: Some(Status::Todo), ..Default::default() })),
            TodoOp::Add(Box::new(NewTodo { board: "Decomps".into(), title: "Old idea".into(), ..Default::default() })),
            TodoOp::Claim { id: "DEC-002".into(), force: false, session: None },
            TodoOp::Relate { id: "DEC-003".into(), kind: RelKind::BlockedBy, other: "DEC-001".into() },
        ];
        for op in ops {
            let actor = if matches!(op, TodoOp::Claim { .. }) { Actor::agent("s1", "Fix flaky test") } else { you.clone() };
            book.apply(op, &actor, &ctx).unwrap();
        }
        crate::paths::write_atomic(&book_path(&home, "c1"), serde_json::to_string(&book).unwrap().as_bytes()).unwrap();
        home
    }

    fn agent(session: &str) -> Caller {
        Caller { session: session.into(), bot: String::new() }
    }

    fn run(home: &Path, words: &[&str], options: &[(&str, &str)], flag_names: &[&str], caller: &Caller) -> Result<String> {
        execute(home, words, &opts(options), &flags(flag_names), caller, &mut std::io::empty())
    }

    fn posted(home: &Path) -> Vec<Event> {
        let Ok(dir) = std::fs::read_dir(bus::inbox_dir(home)) else { return Vec::new() };
        let mut events: Vec<(PathBuf, Event)> = dir.flatten().map(|f| (f.path(), serde_json::from_slice(&std::fs::read(f.path()).unwrap()).unwrap())).collect();
        events.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, _) in &events {
            std::fs::remove_file(path).unwrap();
        }
        events.into_iter().map(|(_, e)| e).collect()
    }

    #[test]
    fn arguments_are_checked_before_anything_is_posted() {
        let home = home();
        let me = agent("s1");
        let error = |words: &[&str], options: &[(&str, &str)], flag_names: &[&str]| run(&home, words, options, flag_names, &me).unwrap_err().to_string();
        assert!(error(&["add", "Fix it"], &[], &[]).contains("needs --type (bug, feature"));
        assert!(error(&["add"], &[("type", "bug")], &[]).contains("needs a title"));
        assert!(error(&["add", "x"], &[("type", "bugfix")], &[]).contains("--type is one of bug, feature"));
        assert!(error(&["add", "x"], &[("type", "bug"), ("status", "done")], &[]).contains("--status is backlog or todo"));
        assert!(error(&["add", "x"], &[("type", "bug"), ("why", "y")], &[]).starts_with("todo add does not take --why\nusage: atelier todo add TITLE"));
        assert!(error(&["add", "x"], &[("type", "bug"), ("type", "feature")], &[]).contains("takes --type once"));
        assert!(error(&["add", "x"], &[("type", "bug"), ("board", "Nope")], &[]).contains("no board \"Nope\" (boards: Decomps)"));
        assert!(error(&["propose", "x"], &[("type", "bug")], &[]).contains("needs --why"));
        assert!(error(&["claim", "DEC"], &[], &[]).contains("is not a todo id"));
        assert!(error(&["claim", "DEC-1"], &[], &["mine"]).contains("does not take --mine"));
        assert!(error(&["update", "DEC-1"], &[], &[]).contains("needs something to change"));
        assert!(error(&["update", "DEC-1"], &[("blocker", "maybe")], &[]).contains("--blocker is yes or no"));
        assert!(error(&["move", "DEC-1"], &[], &[]).contains("needs --status"));
        assert!(error(&["move", "DEC-1"], &[("status", "later")], &[]).contains("--status is one of proposed, backlog"));
        assert!(error(&["check", "DEC-1", "0"], &[], &[]).contains("not a checklist item number"));
        assert!(error(&["check", "DEC-1"], &[], &[]).contains("needs an item number"));
        assert!(error(&["check", "DEC-1", "1"], &[("add", "x")], &[]).contains("needs an item number (N), or --add TEXT"));
        assert!(error(&["link", "DEC-1"], &[("pr", "https://github.com/o/r/pull/1"), ("url", "https://x")], &[]).contains("exactly one of --pr, --note"));
        assert!(error(&["link", "DEC-1"], &[("pr", "https://github.com/o/r/issues/1")], &[]).contains("not a pull request link"));
        assert!(error(&["relate", "DEC-1"], &[], &[]).contains("exactly one of --blocked-by"));
        assert!(error(&["relate", "DEC-1"], &[("blocked-by", "nope")], &[]).contains("not a todo id"));
        assert!(error(&["note", "DEC-1"], &[], &[]).contains("needs the text"));
        assert!(error(&["board", "add", "X"], &[("prefix", "X1")], &[]).contains("--prefix is 2 to 5 letters"));
        assert!(error(&["board", "link", "Decomps"], &[("group", "Nope")], &[]).contains("no canvas group \"Nope\" (groups: Pay, Docs)"));
        assert!(error(&["export"], &[], &[]).contains("needs --canvas FILE"));
        assert!(error(&["mirror"], &[("out", "a.md"), ("set", "b.md")], &[]).contains("--out or --set, not both"));
        assert!(error(&["list"], &[("format", "xml")], &[]).contains("--format is json or text"));
        assert!(error(&["explode"], &[], &[]).starts_with("usage: atelier todo prime"));
        assert!(error(&["show", "DEC-099"], &[], &[]).contains("no todo DEC-099"));
        assert!(run(&home, &["board", "link", "Decomps"], &[], &[], &Caller::default()).unwrap_err().to_string().contains("you are in no group"));
        // A session in a group without a board must name one.
        assert!(run(&home, &["add", "x"], &[("type", "docs")], &[], &agent("s2")).unwrap_err().to_string().contains("no todo board is linked to your canvas group: name one with --board (Decomps)"));
        assert!(posted(&home).is_empty(), "refused commands post nothing");
        assert_eq!(usage("check"), "usage: atelier todo note ID TEXT · todo check ID N [--undo] · todo check ID --add TEXT");
        assert_eq!(usage("board link"), "usage: atelier todo board add NAME [--prefix P] [--parent B] · todo board link B [--group NAME|ID]");
        assert!(usage("accept").contains("todo accept ID [--status backlog|todo|in_progress|in_review]"), "{}", usage("accept"));
        assert_eq!(usage("board delete"), "usage: atelier todo board unlink B --group NAME|ID · todo board delete B [--with-todos]");
        assert!(usage("delete").contains("todo delete ID") && usage("done").contains("todo done ID"));
        // Boards: unlink names the group; delete takes --with-todos only.
        assert!(error(&["board", "unlink", "Decomps"], &[], &[]).contains("needs --group NAME|ID"));
        assert!(error(&["board", "unlink", "Decomps"], &[("group", "Nope")], &[]).contains("no canvas group \"Nope\" (the board is linked to: Pay)"));
        assert!(error(&["board", "delete", "Decomps"], &[], &["force"]).contains("does not take --force"));
        assert!(error(&["board", "delete", "Nope"], &[], &[]).contains("no board \"Nope\""));
        assert!(error(&["board", "delete"], &[], &[]).starts_with("usage: atelier todo prime"));
        assert!(error(&["link", "DEC-1"], &[("session", "Nobody")], &[]).contains("\"Nobody\""), "a session to link is one of the canvas");
        assert!(posted(&home).is_empty(), "refused commands post nothing");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_global_canvas_option_is_only_exports_file() {
        let options = opts(&[("board", "DEC")]);
        let file = Path::new(r"C:\out\board.canvas");
        assert_eq!(options_with_canvas(&["list"], &options, Some(file)), options, "the canvas file a window opens: ignored");
        assert_eq!(options_with_canvas(&["export"], &options, Some(file)), opts(&[("board", "DEC"), ("canvas", r"C:\out\board.canvas")]));
        assert_eq!(options_with_canvas(&["export"], &options, None), options);
        // So `atelier --canvas X todo list` lists.
        let home = home();
        let listed = execute(&home, &["list"], &options_with_canvas(&["list"], &[], Some(file)), &[], &Caller::default(), &mut std::io::empty()).unwrap();
        assert!(listed.starts_with("3 todo(s)"), "{listed}");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn reads_report_a_book_that_cannot_be_read() {
        let home = home();
        std::fs::write(book_path(&home, "c1"), "{\"boards\": [").unwrap();
        for words in [&["list"][..], &["boards"], &["prime"], &["show", "DEC-1"], &["mirror"]] {
            let error = run(&home, words, &[], &[], &Caller::default()).unwrap_err().to_string();
            assert!(error.contains("cannot be read"), "{words:?}: {error}");
        }
        assert_eq!(load(&home, "c1"), TodoBook::default(), "load stays lenient (for views)");
        std::fs::remove_file(book_path(&home, "c1")).unwrap();
        assert!(run(&home, &["list"], &[], &[], &Caller::default()).unwrap().starts_with("No todos"), "no book yet: empty");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn writes_go_to_the_service_as_the_caller() {
        let home = home();
        let me = agent("s1");
        // No service runs here: each request waits in the inbox and the command says so.
        let out = run(&home, &["add", "Fix", "the", "loader"], &[("type", "bug"), ("priority", "urgent")], &[], &me).unwrap();
        assert!(out.starts_with("The todo “Fix the loader” is handed to Atelier; the service is not running"), "{out}");
        let event = posted(&home).remove(0);
        assert_eq!((event.kind.as_str(), event.source.as_str()), (REQUEST, "s1"));
        let op: TodoOp = serde_json::from_value(event.data.clone()).unwrap();
        assert_eq!(op, TodoOp::Add(Box::new(NewTodo { board: "decomps".into(), title: "Fix the loader".into(), kind: TodoType::Bug, priority: Some(Priority::Urgent), ..Default::default() })), "the group's board");
        assert!(event.data["request"].as_str().is_some_and(|r| r.len() == 36));
        let body = home.join("body.md");
        std::fs::write(&body, "\u{feff}Steps:\n1. open\n").unwrap();
        // One command at a time (inbox files written in the same millisecond sort at random).
        let one = |words: &[&str], options: &[(&str, &str)], flag_names: &[&str], caller: &Caller| -> Event {
            run(&home, words, options, flag_names, caller).unwrap();
            let mut events = posted(&home);
            assert_eq!(events.len(), 1, "{words:?} posts one request");
            events.remove(0)
        };
        let op = |words: &[&str], options: &[(&str, &str)], flag_names: &[&str]| -> TodoOp { serde_json::from_value(one(words, options, flag_names, &me).data).unwrap() };
        let with_body = op(&["add", "With body"], &[("type", "feature"), ("board", "Decomps/Parser"), ("status", "todo"), ("body", body.to_str().unwrap()), ("label", "ui"), ("label", "parser"), ("check", "a"), ("check", "b"), ("parent", "dec-1")], &[]);
        let TodoOp::Add(with_body) = with_body else { panic!("{with_body:?}") };
        assert_eq!((with_body.board.as_str(), with_body.body.as_str(), with_body.status, with_body.parent.as_deref()), ("decomps-parser", "Steps:\n1. open\n", Some(Status::Todo), Some("DEC-1")));
        assert_eq!((with_body.labels.clone(), with_body.checklist.clone()), (vec!["ui".to_owned(), "parser".to_owned()], vec!["a".to_owned(), "b".to_owned()]));
        let proposed = op(&["propose", "Cache it"], &[("type", "optimization"), ("why", "Saves a day"), ("discovered-from", "DEC-2")], &[]);
        assert!(matches!(&proposed, TodoOp::Add(p) if p.propose && p.why.as_deref() == Some("Saves a day") && p.discovered_from.as_deref() == Some("DEC-2") && p.board == "decomps"), "{proposed:?}");
        let update = op(&["update", "DEC-1"], &[("blocked", "waiting for logs"), ("blocker", "yes"), ("label", "ui"), ("label", "-old")], &[]);
        assert!(matches!(&update, TodoOp::Update(u) if u.blocked.as_deref() == Some("waiting for logs") && u.blocker == Some(true) && u.add_labels == ["ui"] && u.remove_labels == ["old"]), "{update:?}");
        assert_eq!(op(&["move", "DEC-1"], &[("status", "in-review"), ("board", "parser")], &[]), TodoOp::Move { id: "DEC-1".into(), status: Some(Status::InReview), board: Some("decomps-parser".into()), after: None, before: None, rank: None });
        assert_eq!(op(&["claim", "DEC-1"], &[], &["force"]), TodoOp::Claim { id: "DEC-1".into(), force: true, session: None });
        // You assign a todo to a session by name or id; agents claim only for themselves.
        assert_eq!(serde_json::from_value::<TodoOp>(one(&["claim", "DEC-1"], &[("session", "write DOCS")], &[], &Caller::default()).data).unwrap(), TodoOp::Claim { id: "DEC-1".into(), force: false, session: Some("s2".into()) });
        assert_eq!(serde_json::from_value::<TodoOp>(one(&["claim", "DEC-1"], &[("session", "Coordinator")], &["force"], &Caller::default()).data).unwrap(), TodoOp::Claim { id: "DEC-1".into(), force: true, session: Some("w".into()) });
        assert!(run(&home, &["claim", "DEC-1"], &[("session", "Nobody")], &[], &Caller::default()).unwrap_err().to_string().contains("no session or passive session is called \"Nobody\""));
        assert!(run(&home, &["claim", "DEC-1"], &[("session", "s2")], &[], &me).unwrap_err().to_string().contains("agents claim todos only for themselves"));
        assert!(posted(&home).is_empty(), "refused claims post nothing");
        assert_eq!(op(&["release", "dec-1"], &[], &[]), TodoOp::Release { id: "DEC-1".into() });
        assert_eq!(op(&["note", "DEC-1", "Found", "it"], &[], &[]), TodoOp::Note { id: "DEC-1".into(), text: "Found it".into() });
        assert_eq!(op(&["check", "DEC-1", "2"], &[], &["undo"]), TodoOp::Check { id: "DEC-1".into(), index: 1, done: false });
        assert_eq!(op(&["check", "DEC-1"], &[("add", "Write a test")], &[]), TodoOp::AddCheck { id: "DEC-1".into(), text: "Write a test".into() });
        let link = op(&["link", "DEC-1"], &[("note", "notes/spec.md")], &[]);
        assert!(matches!(&link, TodoOp::Link { kind: LinkKind::Note, target, .. } if Path::new(target).is_absolute() && target.ends_with("spec.md")), "{link:?}");
        assert_eq!(op(&["relate", "DEC-1"], &[("duplicate-of", "DEC-3")], &[]), TodoOp::Relate { id: "DEC-1".into(), kind: RelKind::DuplicateOf, other: "DEC-3".into() });
        assert_eq!(op(&["submit", "DEC-1"], &[("pr", "https://github.com/o/r/pull/4/files")], &[]), TodoOp::Submit { id: "DEC-1".into(), pr: Some("https://github.com/o/r/pull/4".into()), summary: None });
        assert_eq!(op(&["board", "add", "Release", "train"], &[("prefix", "rel")], &[]), TodoOp::AddBoard { name: "Release train".into(), prefix: Some("rel".into()), parent: None, groups: vec![], x: None, y: None });
        let linked: TodoOp = serde_json::from_value(one(&["board", "link", "DEC"], &[], &[], &agent("s2")).data).unwrap();
        assert_eq!(linked, TodoOp::LinkBoard { board: "decomps".into(), group: "g2".into(), unlink: false }, "an agent links to its own group");
        // You unlink a group by name (or one that left the canvas, by its id) and delete a board, with its todos when you say so.
        let you = Caller::default();
        let unlinked: TodoOp = serde_json::from_value(one(&["board", "unlink", "dec"], &[("group", "pay")], &[], &you).data).unwrap();
        assert_eq!(unlinked, TodoOp::LinkBoard { board: "decomps".into(), group: "g1".into(), unlink: true });
        let mut book = load(&home, "c1");
        book.boards[0].canvas_groups.push("g-gone".into());
        crate::paths::write_atomic(&book_path(&home, "c1"), serde_json::to_string(&book).unwrap().as_bytes()).unwrap();
        let unlinked: TodoOp = serde_json::from_value(one(&["board", "unlink", "Decomps"], &[("group", "g-gone")], &[], &you).data).unwrap();
        assert_eq!(unlinked, TodoOp::LinkBoard { board: "decomps".into(), group: "g-gone".into(), unlink: true });
        assert_eq!(serde_json::from_value::<TodoOp>(one(&["board", "delete", "Decomps/Parser"], &[], &[], &you).data).unwrap(), TodoOp::DeleteBoard { board: "decomps-parser".into(), with_todos: false });
        assert_eq!(serde_json::from_value::<TodoOp>(one(&["board", "delete", "DEC"], &[], &["with-todos"], &you).data).unwrap(), TodoOp::DeleteBoard { board: "decomps".into(), with_todos: true });
        // A linked session is stored by its id, whatever name it was given by.
        assert_eq!(op(&["link", "DEC-1"], &[("session", "write docs")], &[]), TodoOp::Link { id: "DEC-1".into(), kind: LinkKind::Session, target: "s2".into(), title: None });
        // You, and an automation outside a session (the TODO bot).
        let accepted = one(&["accept", "DEC-4"], &[("status", "todo")], &[], &you);
        assert_eq!((accepted.source.as_str(), accepted.data.get("bot")), ("", None));
        assert_eq!(serde_json::from_value::<TodoOp>(accepted.data).unwrap(), TodoOp::Accept { id: "DEC-4".into(), status: Some(Status::Todo) });
        assert!(matches!(serde_json::from_value::<TodoOp>(one(&["decline", "DEC-4"], &[("reason", "no")], &[], &you).data).unwrap(), TodoOp::Decline { reason: Some(r), .. } if r == "no"));
        assert!(matches!(serde_json::from_value::<TodoOp>(one(&["done", "DEC-4"], &[], &[], &you).data).unwrap(), TodoOp::Done { .. }));
        let bot = Caller { session: String::new(), bot: "TODO bot".into() };
        let json = run(&home, &["claim", "WI-019"], &[("format", "json")], &[], &bot).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!((value["schemaVersion"].as_u64(), value["queued"].as_bool(), value["ok"].is_null()), (Some(1), Some(true), true));
        let claimed = posted(&home).remove(0);
        assert_eq!((claimed.source.as_str(), claimed.data["bot"].as_str()), ("", Some("TODO bot")));
        // Import: the file's items, into a board named or the group's.
        let file = home.join("TODOs.md");
        std::fs::write(&file, "- [ ] **WI-019: [[WI-019 - Momentum]]** - glide\n").unwrap();
        let imported = op(&["import", file.to_str().unwrap()], &[], &[]);
        assert!(matches!(&imported, TodoOp::Import { board, text, .. } if board == "decomps" && text.contains("WI-019")), "{imported:?}");
        std::fs::write(&file, "nothing here\n").unwrap();
        assert!(run(&home, &["import", file.to_str().unwrap()], &[("board", "X")], &[], &me).unwrap_err().to_string().contains("has no todo items"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn reads_come_from_the_stored_book() {
        let home = home();
        let me = agent("s1");
        let prime = run(&home, &["prime"], &[], &[], &me).unwrap();
        for text in ["you are \"Fix flaky test\" in the canvas group \"Pay\"", "Decomps (DEC) · swimlanes: Parser · 3 open, 1 in progress, 0 proposed, 0 done", "Default board for todo add and todo propose: Decomps.", "Your claims:", "DEC-002", "Ready (1 in all", "todo submit ID --pr URL", "never run todo done", "Types: bug, feature"] {
            assert!(prime.contains(text), "prime says {text:?}:\n{prime}");
        }
        assert!(run(&home, &["prime"], &[], &[], &agent("s2")).unwrap().contains("No board is linked to your group. Boards on this canvas: Decomps (DEC)"));
        let wide = run(&home, &["prime"], &[], &[], &agent("w")).unwrap();
        assert!(wide.contains("you are the canvas-wide agent \"Coordinator\"") && wide.contains("Boards of the canvas:\n  Decomps (DEC)") && wide.contains("Default board for todo add and todo propose: Decomps."), "{wide}");
        // The canvas's only board is where a canvas-wide agent (or you) adds without --board.
        run(&home, &["add", "x"], &[("type", "bug")], &[], &agent("w")).unwrap();
        run(&home, &["add", "y"], &[("type", "bug")], &[], &Caller::default()).unwrap();
        let added: Vec<TodoOp> = posted(&home).into_iter().map(|e| serde_json::from_value(e.data).unwrap()).collect();
        assert!(added.iter().all(|op| matches!(op, TodoOp::Add(new) if new.board == "decomps")) && added.len() == 2, "{added:?}");
        assert!(run(&home, &["prime"], &[], &[], &Caller::default()).unwrap().contains("Default board for todo add and todo propose: Decomps (the only one)."));
        // With several boards it asks which one.
        let mut book = load(&home, "c1");
        book.apply(TodoOp::AddBoard { name: "Release".into(), prefix: None, parent: None, groups: vec![], x: None, y: None }, &Actor::human(), &todos::Ctx::at(1)).unwrap();
        crate::paths::write_atomic(&book_path(&home, "c1"), serde_json::to_string(&book).unwrap().as_bytes()).unwrap();
        let error = run(&home, &["add", "x"], &[("type", "bug")], &[], &agent("w")).unwrap_err().to_string();
        assert!(error.contains("you are the canvas-wide agent and the canvas has several todo boards: name one with --board (Decomps, Release)"), "{error}");
        assert!(run(&home, &["add", "x"], &[("type", "bug")], &[], &Caller::default()).unwrap_err().to_string().contains("in no canvas group and the canvas has several todo boards"));
        assert!(run(&home, &["prime"], &[], &[], &agent("w")).unwrap().contains("Name the board with --board when you add or propose (Decomps, Release)."));
        assert!(posted(&home).is_empty());
        assert_eq!(run(&home, &["ready"], &[], &[], &agent("w")).unwrap().lines().count(), 2, "every board's ready todos");
        let list = run(&home, &["list"], &[], &[], &me).unwrap();
        assert!(list.starts_with("3 todo(s) on your group's boards:"), "{list}");
        assert!(list.contains("DEC-001  todo · bug · high  Crash on load  (☑ 0/1)") && list.contains("DEC-003  backlog · feature  Old idea  (⚑ blocked: waits for DEC-001 (todo))"), "{list}");
        assert!(list.contains("DEC-002  in progress · feature  String table  (board Decomps › Parser; claimed by Fix flaky test)"), "{list}");
        assert_eq!(run(&home, &["list"], &[], &["mine"], &me).unwrap().lines().count(), 2);
        assert!(run(&home, &["list"], &[("status", "done")], &[], &me).unwrap().starts_with("No todos on your group's boards."));
        let json: serde_json::Value = serde_json::from_str(&run(&home, &["list"], &[("type", "bug"), ("format", "json")], &[], &me).unwrap()).unwrap();
        assert_eq!((json["schemaVersion"].as_u64(), json["kind"].as_str(), json["todos"].as_array().map(Vec::len)), (Some(1), Some("todo-list"), Some(1)));
        assert_eq!((json["todos"][0]["id"].as_str(), json["todos"][0]["is_ready"].as_bool(), json["todos"][0]["board_name"].as_str()), (Some("DEC-001"), Some(true), Some("Decomps")));
        let show = run(&home, &["show", "dec-3"], &[], &[], &me).unwrap();
        assert!(show.starts_with("DEC-003 · Old idea\n  board Decomps · backlog · feature"), "{show}");
        assert!(show.contains("⚑ blocked: waits for DEC-001 (todo)") && show.contains("Relations:\n  blocked by DEC-001 Crash on load (todo)") && show.contains("Log:"), "{show}");
        let shown: serde_json::Value = serde_json::from_str(&run(&home, &["show", "DEC-1"], &[("format", "json")], &[], &me).unwrap()).unwrap();
        assert_eq!((shown["kind"].as_str(), shown["blocking"][0].as_str(), shown["todo"]["type"].as_str()), (Some("todo"), Some("DEC-003"), Some("bug")));
        assert!(run(&home, &["ready"], &[], &[], &me).unwrap().contains("DEC-001"));
        assert!(run(&home, &["ready"], &[("board", "Parser")], &[], &me).unwrap().starts_with("Nothing is ready on Decomps › Parser"));
        let boards = run(&home, &["boards"], &[], &["tree"], &me).unwrap();
        assert!(boards.contains("Decomps (DEC) · 3 open, 1 in progress, 0 proposed, 0 done · groups: Pay") && boards.contains("    └ Parser · 1 open, 1 in progress, 0 done"), "{boards}");
        let boards: serde_json::Value = serde_json::from_str(&run(&home, &["boards"], &[("format", "json")], &[], &me).unwrap()).unwrap();
        assert_eq!((boards["kind"].as_str(), boards["boards"][0]["group_names"][0].as_str(), boards["boards"][0]["counts"]["open"].as_u64()), (Some("todo-boards"), Some("Pay"), Some(3)));
        assert!(run(&home, &["mirror"], &[], &[], &me).unwrap().contains("- [ ] **DEC-001: [[DEC-001 - Crash on load]]**"));
        let out = home.join("mirror.md");
        run(&home, &["mirror"], &[("out", out.to_str().unwrap())], &[], &me).unwrap();
        assert!(std::fs::read_to_string(&out).unwrap().starts_with("# TODOs"));
        let canvas = home.join("board.canvas");
        assert!(run(&home, &["export"], &[("canvas", canvas.to_str().unwrap()), ("board", "DEC")], &[], &me).unwrap().contains("(JSON Canvas 1.0)"));
        let exported: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&canvas).unwrap()).unwrap();
        assert!(exported["nodes"].as_array().unwrap().iter().any(|n| n["id"] == "todo-DEC-001"));
        assert!(posted(&home).is_empty(), "reads post nothing");
        // No canvas routed: reads say so.
        std::fs::remove_file(home.join("bus").join("routing.json")).unwrap();
        assert!(run(&home, &["list"], &[], &[], &me).unwrap_err().to_string().contains("no canvas with an id"));
        let _ = std::fs::remove_dir_all(home);
    }
}
