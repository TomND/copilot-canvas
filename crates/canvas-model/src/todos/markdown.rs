//! The owner's TODOs.md format: [`import_markdown`] reads it,
//! [`mirror_markdown`] writes it.
//!
//! ```text
//! - [ ] **WI-019: [[WI-019 - Gentle momentum when panning]]** - text (bot: blocked 2026-10-30: why)
//! - [x] **WI-001: [[WI-001 - Easier group selection]]** - text. Done 2026-10-29 in `da03e6f`.
//! ## Detailed requirements
//! ### WI-013 - Title          (bullets below become the todo's body)
//! ## Relationships
//! - WI-004 follows WI-007: …  (a relation and a body line)
//! ```
//!
//! The mirror adds `(canvas: in progress; bug; high; blocked: why)` notes
//! and `- [?]` proposals under `## Proposed`, which the importer reads back.

use super::ops::date;
use super::*;

/// What [`import_markdown`] found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Imported {
    /// In file order, ids as written (empty: the book gives one).
    pub todos: Vec<Todo>,
    /// The prefix asked for, else the one most ids use.
    pub prefix: String,
    /// Lines it could not place.
    pub warnings: Vec<String>,
}

/// What a `##` heading starts.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Section {
    /// Todo items; the heading may say their status (`## Done`, `## Proposed`).
    Items(Option<Status>),
    Details,
    Relationships,
}

fn section(heading: &str) -> Section {
    let h = heading.trim().trim_matches(['#', ' ', ':']).to_lowercase();
    let starts = |words: &[&str]| words.iter().any(|w| h.starts_with(w));
    if h.contains("detailed requirements") || starts(&["details", "requirements", "specs"]) {
        Section::Details
    } else if starts(&["relationship", "relations", "dependencies"]) {
        Section::Relationships
    } else if starts(&["proposed", "proposals", "ideas"]) {
        Section::Items(Some(Status::Proposed))
    } else if starts(&["done", "completed", "finished"]) {
        Section::Items(Some(Status::Done))
    } else if starts(&["canceled", "cancelled", "won't do", "wont do", "dropped"]) {
        Section::Items(Some(Status::Canceled))
    } else if starts(&["backlog", "later", "someday"]) {
        Section::Items(Some(Status::Backlog))
    } else if starts(&["in progress", "doing", "working"]) {
        Section::Items(Some(Status::InProgress))
    } else if starts(&["in review", "review"]) {
        Section::Items(Some(Status::InReview))
    } else {
        Section::Items(None)
    }
}

/// `- [x] rest` → (indent, mark, rest).
fn checkbox(line: &str) -> Option<(usize, char, &str)> {
    let indent: usize = line.chars().take_while(|c| c.is_whitespace()).map(|c| if c == '\t' { 4 } else { 1 }).sum();
    let rest = line.trim_start();
    let rest = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")).or_else(|| rest.strip_prefix("+ "))?;
    let rest = rest.strip_prefix('[')?;
    let mut chars = rest.chars();
    let mark = chars.next()?;
    let rest = chars.as_str().strip_prefix(']')?;
    matches!(mark, ' ' | 'x' | 'X' | '?' | '-' | '~' | '/' | '>').then(|| (indent, mark, rest.strip_prefix(' ').unwrap_or(rest)))
}

/// A todo id at the start (as written, uppercase prefix), and the rest.
fn leading_id(text: &str) -> Option<(String, &str)> {
    let text = text.trim_start();
    let end = text.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).unwrap_or(text.len());
    let candidate = text[..end].trim_end_matches('-');
    let prefix_upper = candidate.split('-').next().is_some_and(|p| p.bytes().all(|b| b.is_ascii_uppercase()));
    (prefix_upper && parse_id(candidate).is_some()).then(|| (candidate.to_owned(), &text[candidate.len()..]))
}

/// Every todo id in a line, in order.
fn ids_in(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(|c: char| c.is_ascii_uppercase()) {
        let before_ok = start == 0 || !rest[..start].ends_with(|c: char| c.is_ascii_alphanumeric());
        match leading_id(&rest[start..]).filter(|_| before_ok) {
            Some((id, after)) => {
                ids.push(id);
                rest = after;
            }
            None => rest = &rest[start + 1..],
        }
    }
    ids
}

/// The title inside `[[ID - Title]]` (or `[[…|alias]]`), without the id.
fn wiki_title(text: &str, id: &str) -> Option<String> {
    let start = text.find("[[")? + 2;
    let end = text[start..].find("]]")? + start;
    let inner = &text[start..end];
    let inner = inner.split_once('|').map_or(inner, |(_, alias)| alias);
    let title = match leading_id(inner) {
        Some((found, rest)) if same_id(&found, id) || id.is_empty() => rest,
        _ => inner,
    };
    Some(title.trim().trim_start_matches([':', '-', '—', '–']).trim().to_owned()).filter(|t| !t.is_empty())
}

/// Takes `(key …)` out of a text (parentheses inside are matched): the
/// rest, and what was inside after the key.
fn take_note(text: &str, key: &str) -> (String, Option<String>) {
    let Some(start) = text.find(&format!("({key}")).or_else(|| {
        let mut upper = key.to_owned();
        upper[..1].make_ascii_uppercase();
        text.find(&format!("({upper}"))
    }) else {
        return (text.to_owned(), None);
    };
    let mut depth = 0;
    let mut end = text.len();
    for (i, c) in text[start..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let inner = text[start + 1 + key.len()..end].trim().to_owned();
    let after = text.get(end + 1..).unwrap_or("");
    let rest = format!("{} {}", text[..start].trim_end(), after.trim_start()).trim().to_owned();
    (rest, Some(inner))
}

/// `2026-10-29` at the start → (date, rest).
fn take_date(text: &str) -> (Option<String>, &str) {
    let text = text.trim_start();
    let looks = text.len() >= 10 && text.is_char_boundary(10) && text[..10].char_indices().all(|(i, c)| if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() });
    if looks { (Some(text[..10].to_owned()), &text[10..]) } else { (None, text) }
}

fn date_ms(date: &str) -> Option<u64> {
    crate::paths::parse_rfc3339_ms(&format!("{date}T12:00:00Z"))
}

/// How an item ended: `Done 2026-10-29 in `da03e6f`.` or `Canceled 2026-10-30.`
struct Ended {
    status: Status,
    ms: Option<u64>,
    commit: Option<String>,
}

fn take_ended(text: &str) -> (String, Option<Ended>) {
    for (word, status) in [("Done ", Status::Done), ("Canceled ", Status::Canceled), ("Cancelled ", Status::Canceled)] {
        let mut from = 0;
        while let Some(found) = text[from..].find(word) {
            let at = from + found;
            from = at + word.len();
            let (Some(day), mut rest) = take_date(&text[at + word.len()..]) else { continue };
            let mut commit = None;
            if let Some(after) = rest.strip_prefix(" in `")
                && let Some(end) = after.find('`')
            {
                commit = Some(after[..end].to_owned());
                rest = &after[end + 1..];
            }
            let rest = rest.strip_prefix('.').unwrap_or(rest);
            let kept = format!("{} {}", text[..at].trim_end(), rest.trim_start()).trim().to_owned();
            return (kept, Some(Ended { status, ms: date_ms(&day), commit }));
        }
    }
    (text.to_owned(), None)
}

/// A type from a title's words (the format has none).
fn guess_type(title: &str) -> TodoType {
    let lower = title.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).collect();
    let any = |list: &[&str]| words.iter().any(|w| list.contains(w));
    if any(&["bug", "fix", "fixes", "crash", "crashes", "broken", "fails", "regression", "error", "wrong"]) {
        TodoType::Bug
    } else if any(&["docs", "document", "documentation", "readme"]) {
        TodoType::Docs
    } else if any(&["faster", "perf", "performance", "speed", "optimize", "optimise", "latency", "memory"]) {
        TodoType::Optimization
    } else if any(&["investigate", "research", "spike", "explore"]) {
        TodoType::Research
    } else {
        TodoType::Feature
    }
}
/// One `- [ ] …` item.
fn parse_item(mark: char, rest: &str, default: Option<Status>, board: &str) -> Todo {
    let (head, tail) = match rest.strip_prefix("**") {
        Some(after) => match after.find("**") {
            Some(end) => (&after[..end], &after[end + 2..]),
            None => (after, ""),
        },
        None => [" - ", " — ", " – "].iter().find_map(|sep| rest.split_once(sep)).unwrap_or((rest, "")),
    };
    let (id, named) = match leading_id(head) {
        Some((id, named)) => (id, named),
        None => (String::new(), head),
    };
    let named = named.trim().trim_start_matches([':', '-', '—', '–']).trim();
    let mut title = wiki_title(named, &id).unwrap_or_else(|| named.trim_matches(['[', ']']).trim().to_owned());
    let tail = tail.trim().trim_start_matches(['-', '—', '–', ':']).trim();
    let (tail, bot) = take_note(tail, "bot:");
    let (tail, canvas) = take_note(&tail, "canvas:");
    let (tail, ended) = take_ended(&tail);
    let mut status = match mark {
        'x' | 'X' => Status::Done,
        '?' => Status::Proposed,
        '-' | '~' => Status::Canceled,
        '/' | '>' => Status::InProgress,
        _ => default.filter(|s| !s.closed()).unwrap_or(Status::Todo),
    };
    let mut todo = Todo { id, board: board.to_owned(), ..Default::default() };
    let mut kind = None;
    if let Some(ended) = &ended {
        if status == Status::Todo || status.closed() {
            status = ended.status;
        }
        todo.done_ms = ended.ms;
        if let Some(commit) = &ended.commit {
            todo.log.push(LogEntry { ms: ended.ms.unwrap_or_default(), kind: LogKind::History, text: format!("done in `{commit}`"), by: Actor::bot("import") });
        }
    }
    if let Some(note) = &canvas {
        // `(canvas: in progress; bug; high; claimed by X; blocked: why)` from a mirror.
        let (head, blocked) = match note.find("blocked:") {
            Some(at) => (&note[..at], Some(note[at + 8..].trim().to_owned())),
            None => (note.as_str(), None),
        };
        for part in head.split([';', ',']).map(str::trim).filter(|p| !p.is_empty()) {
            if let Some(s) = Status::parse(part).filter(|s| s.open()) {
                if status == Status::Todo {
                    status = s;
                }
            } else if let Some(k) = TodoType::parse(part) {
                kind = Some(k);
            } else if let Some(p) = Priority::parse(part) {
                todo.priority = p;
            }
        }
        todo.blocked = blocked.filter(|b| !b.is_empty());
    }
    if let Some(note) = &bot {
        let lower = note.to_lowercase();
        if let Some(rest) = note.get(.."blocked".len()).filter(|w| w.eq_ignore_ascii_case("blocked")).map(|_| &note["blocked".len()..]) {
            let (day, rest) = take_date(rest);
            let reason = rest.trim().trim_start_matches(':').trim();
            let reason = if reason.is_empty() { "blocked by the TODO bot".to_owned() } else { reason.to_owned() };
            todo.blocked = Some(format!("{reason} (bot{})", day.map(|d| format!(", {d}")).unwrap_or_default()));
        } else if ["in progress", "working", "claimed", "started"].iter().any(|w| lower.starts_with(w)) && status == Status::Todo {
            status = Status::InProgress;
        }
        todo.log.push(LogEntry { kind: LogKind::Note, text: format!("bot: {note}"), by: Actor::bot("TODO bot"), ..Default::default() });
    }
    if title.is_empty() {
        let first = tail.split(". ").next().unwrap_or_default();
        title = if first.is_empty() { "Untitled".to_owned() } else { first.chars().take(MAX_TITLE).collect() };
    }
    todo.title = title.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_TITLE).collect();
    todo.kind = kind.unwrap_or_else(|| guess_type(&todo.title));
    todo.body = tail.clone();
    if status == Status::Proposed {
        todo.proposal = Some(Proposal { why: tail, ms: 0 });
    }
    if status.closed() {
        todo.resolution = Some(if status == Status::Done { Resolution::Done } else { Resolution::WontDo });
    } else {
        todo.done_ms = None;
    }
    todo.status = status;
    todo
}

/// Reads the owner's TODOs.md format into todos of `board`: items keep
/// their ids, `### ID - Title` sections under `## Detailed requirements`
/// become their bodies, `## Relationships` bullets relations (`related`,
/// or `blocked by` / `duplicate of` / `discovered from` when the line says
/// so) and a body line. Nested checkboxes become checklist items.
pub fn import_markdown(text: &str, board: &str, prefix: &str) -> Imported {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut out = Imported::default();
    let mut current = Section::Items(None);
    let mut last: Option<usize> = None;
    let mut details: Vec<(String, Vec<String>)> = Vec::new();
    let mut relationships: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        if let Some(heading) = line.strip_prefix("## ") {
            current = section(heading);
            last = None;
            continue;
        }
        if line.starts_with("# ") {
            last = None;
            continue;
        }
        match current {
            Section::Details => match line.strip_prefix("### ") {
                Some(heading) => match leading_id(heading) {
                    Some((id, _)) => details.push((id, Vec::new())),
                    None => out.warnings.push(format!("a details section without a todo id: {heading:?}")),
                },
                None => {
                    if let Some((_, lines)) = details.last_mut() {
                        lines.push(line.to_owned());
                    }
                }
            },
            Section::Relationships => {
                let item = line.trim_start().strip_prefix("- ").or_else(|| line.trim_start().strip_prefix("* "));
                if let Some(item) = item.map(str::trim).filter(|i| !i.is_empty()) {
                    if ids_in(item).len() >= 2 {
                        relationships.push(item.to_owned());
                    } else {
                        out.warnings.push(format!("a relationship without two todo ids: {item:?}"));
                    }
                }
            }
            Section::Items(default) => {
                if line.trim_start().starts_with('#') {
                    // A sub-board's heading in a mirror.
                    last = None;
                    continue;
                }
                if let Some((indent, mark, rest)) = checkbox(line) {
                    if indent >= 2
                        && let Some(i) = last
                    {
                        out.todos[i].checklist.push(CheckItem { text: rest.trim().to_owned(), done: matches!(mark, 'x' | 'X') });
                        continue;
                    }
                    out.todos.push(parse_item(mark, rest, default, board));
                    last = Some(out.todos.len() - 1);
                } else if let Some(i) = last.filter(|_| raw.starts_with([' ', '\t']) && !line.trim().is_empty()) {
                    let body = &mut out.todos[i].body;
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(line.trim());
                } else if !line.trim().is_empty() {
                    last = None;
                }
            }
        }
    }
    for (id, lines) in details {
        let text = lines.join("\n").trim_matches('\n').trim_end().to_owned();
        match out.todos.iter_mut().find(|t| same_id(&t.id, &id)) {
            Some(todo) if !text.is_empty() => todo.body = if todo.body.is_empty() { text } else { format!("{}\n\n{text}", todo.body) },
            Some(_) => {}
            None => out.warnings.push(format!("details for {id}, which is not in the list")),
        }
    }
    let mut noted: BTreeSet<String> = BTreeSet::new();
    for line in relationships {
        let ids = ids_in(&line);
        let (subject, other) = (&ids[0], &ids[1]);
        let lower = line.to_lowercase();
        let kind = if lower.contains("blocked by") || lower.contains("depends on") || lower.contains("waits for") {
            RelKind::BlockedBy
        } else if lower.contains("duplicate of") {
            RelKind::DuplicateOf
        } else if lower.contains("discovered from") {
            RelKind::DiscoveredFrom
        } else {
            RelKind::Related
        };
        let Some(todo) = out.todos.iter_mut().find(|t| same_id(&t.id, subject)) else {
            out.warnings.push(format!("a relationship of {subject}, which is not in the list"));
            continue;
        };
        if !todo.relations.iter().any(|r| r.kind == kind && same_id(&r.id, other)) {
            todo.relations.push(Relation { kind, id: other.clone() });
        }
        let first = noted.insert(todo.id.clone());
        let separator = if todo.body.is_empty() { "" } else if first { "\n\n" } else { "\n" };
        todo.body = format!("{}{separator}{line}", todo.body);
    }
    out.prefix = if valid_prefix(&prefix.trim().to_ascii_uppercase()) {
        prefix.trim().to_ascii_uppercase()
    } else {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for (p, _) in out.todos.iter().filter_map(|t| parse_id(&t.id)) {
            *counts.entry(p).or_default() += 1;
        }
        counts.into_iter().max_by_key(|(_, n)| *n).map(|(p, _)| p).unwrap_or_default()
    };
    for todo in &mut out.todos {
        within_limits(todo, &mut out.warnings);
    }
    out
}

/// The first `max` characters of a text.
fn chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Cuts what a file gave a todo to the limits the book keeps for every
/// change (a body, checklist, reasons, relations), saying what was cut.
fn within_limits(todo: &mut Todo, warnings: &mut Vec<String>) {
    let name = if todo.id.is_empty() { format!("{:?}", todo.title) } else { todo.id.clone() };
    if todo.body.len() > MAX_BODY {
        let mut end = MAX_BODY;
        while !todo.body.is_char_boundary(end) {
            end -= 1;
        }
        todo.body.truncate(end);
        warnings.push(format!("the body of {name} was cut to {} KB", MAX_BODY / 1024));
    }
    if todo.checklist.len() > MAX_CHECKLIST {
        todo.checklist.truncate(MAX_CHECKLIST);
        warnings.push(format!("{name} keeps its first {MAX_CHECKLIST} checklist items"));
    }
    for item in &mut todo.checklist {
        item.text = chars(&item.text, MAX_TEXT);
    }
    todo.checklist.retain(|c| !c.text.trim().is_empty());
    if todo.relations.len() > MAX_RELATIONS {
        todo.relations.truncate(MAX_RELATIONS);
        warnings.push(format!("{name} keeps its first {MAX_RELATIONS} relations"));
    }
    todo.blocked = todo.blocked.as_deref().map(|b| chars(&b.split_whitespace().collect::<Vec<_>>().join(" "), 500)).filter(|b| !b.is_empty());
    if let Some(proposal) = &mut todo.proposal {
        proposal.why = chars(&proposal.why, MAX_TEXT);
    }
    for entry in &mut todo.log {
        entry.text = chars(&entry.text, MAX_TEXT);
    }
    super::ops::trim_log(&mut todo.log);
}
/// A body's first paragraph as one line, and the rest.
fn split_body(body: &str) -> (String, String) {
    let body = body.trim();
    let (first, rest) = body.split_once("\n\n").unwrap_or((body, ""));
    (first.split_whitespace().collect::<Vec<_>>().join(" "), rest.trim().to_owned())
}

/// One todo as a TODOs.md item.
fn item_line(todo: &Todo) -> String {
    let mark = match todo.status {
        Status::Done => 'x',
        Status::Canceled => '-',
        Status::Proposed => '?',
        _ => ' ',
    };
    let mut line = format!("- [{mark}] **{id}: [[{id} - {title}]]**", id = todo.id, title = todo.title.replace(['[', ']', '|'], ""));
    let (mut text, _) = split_body(&todo.body);
    if text.is_empty() && todo.status == Status::Proposed {
        text = todo.proposal.as_ref().map(|p| p.why.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
    }
    let mut notes: Vec<String> = Vec::new();
    if matches!(todo.status, Status::Backlog | Status::InProgress | Status::InReview) {
        notes.push(todo.status.label().to_lowercase());
    }
    if todo.kind != guess_type(&todo.title) {
        notes.push(todo.kind.word().to_owned());
    }
    if todo.priority != Priority::None {
        notes.push(todo.priority.word().to_owned());
    }
    if let Some(claim) = todo.claimed_by.as_ref().filter(|_| !todo.status.closed()) {
        notes.push(format!("claimed by {}", claim.by.label()));
    }
    if let Some(blocked) = &todo.blocked {
        notes.push(format!("blocked: {blocked}"));
    }
    if !notes.is_empty() {
        text = format!("{text} (canvas: {})", notes.join("; ")).trim().to_owned();
    }
    if let (true, Some(ms)) = (todo.status.closed(), todo.done_ms) {
        let commit = todo.log.iter().rev().find_map(|l| l.text.strip_prefix("done in `").map(|c| format!(" in `{}", c))).unwrap_or_default();
        if !text.is_empty() && !text.ends_with(['.', '!', '?']) {
            text.push('.');
        }
        let word = if todo.status == Status::Done { "Done" } else { "Canceled" };
        text = format!("{text} {word} {}{commit}.", date(ms)).trim().to_owned();
    }
    if !text.is_empty() {
        line.push_str(" - ");
        line.push_str(&text);
    }
    line
}

/// The book in the owner's TODOs.md format (a generated file): each board
/// with its open items (`- [ ]`), then done (`- [x] … Done DATE.`) and
/// canceled (`- [-]`) ones, sub-boards as `###` sections; proposals under
/// `## Proposed` (`- [?]`); the rest of each body under `## Detailed
/// requirements`; the relations under `## Relationships`.
pub fn mirror_markdown(book: &TodoBook) -> String {
    let mut out = String::from(
        "# TODOs\n\n> Generated by Atelier from the canvas's todo boards; do not edit this file (it is rewritten after every change). Change todos with `atelier todo …` or in the Tasks space.\n",
    );
    let order = |s: Status| match s {
        Status::InProgress => 0,
        Status::InReview => 1,
        Status::Todo => 2,
        Status::Backlog => 3,
        Status::Done => 4,
        _ => 5,
    };
    for root in book.boards.iter().filter(|b| b.parent.is_none()) {
        out.push_str(&format!("\n## {} ({})\n", root.name, root.prefix));
        let mut lanes = vec![root];
        lanes.extend(book.sub_boards(&root.id));
        for lane in lanes {
            let mut todos: Vec<&Todo> = book.todos.iter().filter(|t| t.board == lane.id && t.status != Status::Proposed).collect();
            if todos.is_empty() {
                continue;
            }
            todos.sort_by(|a, b| order(a.status).cmp(&order(b.status)).then_with(|| if a.status.closed() { a.done_ms.cmp(&b.done_ms).then(a.id.cmp(&b.id)) } else { by_rank(a, b) }));
            if lane.parent.is_some() {
                out.push_str(&format!("\n### {}\n", lane.name));
            }
            out.push('\n');
            for todo in todos {
                out.push_str(&item_line(todo));
                out.push('\n');
            }
        }
    }
    let mut proposed: Vec<&Todo> = book.todos.iter().filter(|t| t.status == Status::Proposed).collect();
    if !proposed.is_empty() {
        proposed.sort_by_key(|t| (t.proposal.as_ref().map_or(t.created_ms, |p| p.ms), t.created_ms));
        out.push_str("\n## Proposed\n\n");
        for todo in proposed {
            out.push_str(&item_line(todo));
            out.push('\n');
        }
    }
    let details: Vec<(&Todo, String)> = book.todos.iter().map(|t| (t, split_body(&t.body).1)).filter(|(_, rest)| !rest.is_empty()).collect();
    if !details.is_empty() {
        out.push_str("\n## Detailed requirements\n");
        for (todo, rest) in details {
            out.push_str(&format!("\n### {} - {}\n\n{rest}\n", todo.id, todo.title));
        }
    }
    let relations: Vec<String> = book.todos.iter().flat_map(|t| t.relations.iter().map(move |r| format!("- {} {} {}", t.id, r.kind.label(), r.id))).collect();
    if !relations.is_empty() {
        out.push_str("\n## Relationships\n\n");
        out.push_str(&relations.join("\n"));
        out.push('\n');
    }
    out
}
