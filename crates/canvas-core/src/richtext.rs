//! Formatted text for the report viewer, in cells (both front ends): Markdown
//! (pulldown-cmark) and an HTML subset become blocks of styled spans
//! (headings, paragraphs with emphasis, code, lists, quotes, tables, rules),
//! laid out at a width as lines. Nothing in a report is run or fetched:
//! images show as their alt text, and links are spans that open only on an
//! explicit click.

use ratatui::style::{Color, Modifier, Style};

use crate::paint::{ACCENT, ATTENTION, BLUE, FAINT, GRAY, MUTED, PANEL_HI, PURPLE, TEXT, text_width};

const BOLD: u8 = 1;
const ITALIC: u8 = 1 << 1;
const CODE: u8 = 1 << 2;
const STRIKE: u8 = 1 << 3;
const UNDER: u8 = 1 << 4;
const MARK: u8 = 1 << 5;

/// A run of text with one style (`marks`: the bits above) and maybe a link.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub marks: u8,
    pub link: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading(u8, Vec<Span>),
    Para(Vec<Span>),
    Code(String),
    Quote(Vec<Block>),
    List { ordered: bool, start: u64, items: Vec<Vec<Block>> },
    Table { head: Vec<Vec<Span>>, rows: Vec<Vec<Vec<Span>>> },
    Rule,
}

/// A laid-out line: (text, style, link) pieces.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub spans: Vec<(String, Style, Option<String>)>,
}

impl Line {
    pub fn text(&self) -> String {
        self.spans.iter().map(|(t, _, _)| t.as_str()).collect()
    }
}

enum Frame {
    Root(Vec<Block>),
    Quote(Vec<Block>),
    List { ordered: bool, start: u64, items: Vec<Vec<Block>> },
    Item(Vec<Block>),
    Table(TableFrame),
}

/// A table being read: its head, rows, the row and cell in progress.
#[derive(Default)]
struct TableFrame {
    head: Vec<Vec<Span>>,
    rows: Vec<Vec<Vec<Span>>>,
    row: Vec<Vec<Span>>,
    in_head: bool,
    /// The row has header cells only so far.
    row_th: bool,
}

/// Builds blocks from parser events (Markdown and HTML alike).
struct Builder {
    frames: Vec<Frame>,
    inline: Vec<Span>,
    marks: Vec<(String, u8)>,
    links: Vec<(String, Option<String>)>,
    code: Option<String>,
    heading: Option<u8>,
    cell: bool,
    /// HTML outside `<pre>`: runs of white space are one space.
    collapse: bool,
}

/// A link a click may open (http(s) or mailto); anything else is plain text.
fn safe_link(url: &str) -> Option<String> {
    canvas_model::reports::check_url(url.trim()).ok().map(|_| url.trim().to_owned())
}

fn has_text(spans: &[Span]) -> bool {
    spans.iter().any(|s| !s.text.trim().is_empty())
}

/// Leading and trailing white space of a run of spans goes.
fn trim(mut spans: Vec<Span>) -> Vec<Span> {
    while spans.first().is_some_and(|s| s.text.trim().is_empty() && s.text != "\n") {
        spans.remove(0);
    }
    while spans.last().is_some_and(|s| s.text.trim().is_empty()) {
        spans.pop();
    }
    if let Some(first) = spans.first_mut() {
        first.text = first.text.trim_start().to_owned();
    }
    if let Some(last) = spans.last_mut() {
        last.text = last.text.trim_end().to_owned();
    }
    spans
}

impl Builder {
    fn new(collapse: bool) -> Self {
        Builder { frames: vec![Frame::Root(Vec::new())], inline: Vec::new(), marks: Vec::new(), links: Vec::new(), code: None, heading: None, cell: false, collapse }
    }

    fn mark(&self) -> u8 {
        self.marks.iter().fold(0, |all, (_, m)| all | m)
    }

    fn link(&self) -> Option<String> {
        self.links.iter().rev().find_map(|(_, l)| l.clone())
    }

    fn push_mark(&mut self, tag: &str, mark: u8) {
        self.marks.push((tag.to_owned(), mark));
    }

    fn pop_mark(&mut self, tag: &str) {
        if let Some(at) = self.marks.iter().rposition(|(t, _)| t == tag) {
            self.marks.truncate(at);
        }
    }

    fn push_link(&mut self, tag: &str, url: Option<String>) {
        self.links.push((tag.to_owned(), url));
    }

    fn pop_link(&mut self, tag: &str) {
        if let Some(at) = self.links.iter().rposition(|(t, _)| t == tag) {
            self.links.truncate(at);
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(code) = &mut self.code {
            code.push_str(text);
            return;
        }
        let mut text = text.to_owned();
        if self.collapse {
            let ends_space = self.inline.last().is_none_or(|s| s.text.ends_with([' ', '\n']));
            let words: Vec<&str> = text.split_whitespace().collect();
            let lead = text.starts_with(char::is_whitespace) && !ends_space;
            let tail = text.ends_with(char::is_whitespace) && !words.is_empty();
            text = format!("{}{}{}", if lead { " " } else { "" }, words.join(" "), if tail { " " } else { "" });
            if text.is_empty() {
                return;
            }
        }
        let (marks, link) = (self.mark(), self.link());
        match self.inline.last_mut() {
            Some(last) if last.marks == marks && last.link == link && last.text != "\n" => last.text.push_str(&text),
            _ => self.inline.push(Span { text, marks, link }),
        }
    }

    fn hard_break(&mut self) {
        if let Some(code) = &mut self.code {
            code.push('\n');
        } else {
            self.inline.push(Span { text: "\n".into(), marks: 0, link: None });
        }
    }

    fn push_block(&mut self, block: Block) {
        match self.frames.last_mut() {
            Some(Frame::Root(blocks) | Frame::Quote(blocks) | Frame::Item(blocks)) => blocks.push(block),
            Some(Frame::List { items, .. }) => items.push(vec![block]),
            // Stray content in a table (outside its cells) is left out.
            Some(Frame::Table(_)) | None => {}
        }
    }

    /// The text so far becomes a paragraph (or a heading).
    fn flush(&mut self) {
        if self.cell || self.code.is_some() {
            return;
        }
        let spans = trim(std::mem::take(&mut self.inline));
        if !has_text(&spans) {
            return;
        }
        match self.heading {
            Some(level) => self.push_block(Block::Heading(level, spans)),
            None => self.push_block(Block::Para(spans)),
        }
    }

    fn start_heading(&mut self, level: u8) {
        self.flush();
        self.heading = Some(level.clamp(1, 6));
    }

    fn end_heading(&mut self) {
        self.flush();
        self.heading = None;
    }

    fn start_code(&mut self) {
        self.flush();
        self.code = Some(String::new());
    }

    fn end_code(&mut self) {
        if let Some(code) = self.code.take() {
            let code = code.trim_end_matches(['\n', '\r']).trim_start_matches(['\n', '\r']).to_owned();
            if !code.is_empty() {
                self.push_block(Block::Code(code));
            }
        }
    }

    fn open(&mut self, frame: Frame) {
        self.flush();
        self.frames.push(frame);
    }

    /// Closes frames down to (and including) the innermost one `is` matches.
    fn close(&mut self, is: impl Fn(&Frame) -> bool) {
        if !self.frames.iter().skip(1).any(&is) {
            return;
        }
        self.flush();
        while self.frames.len() > 1 {
            let frame = self.frames.pop().expect("more than the root");
            let done = is(&frame);
            self.finish_frame(frame);
            if done {
                break;
            }
        }
    }

    fn finish_frame(&mut self, frame: Frame) {
        match frame {
            Frame::Quote(blocks) => {
                if !blocks.is_empty() {
                    self.push_block(Block::Quote(blocks));
                }
            }
            Frame::List { ordered, start, items } => {
                if !items.is_empty() {
                    self.push_block(Block::List { ordered, start, items });
                }
            }
            Frame::Item(blocks) => match self.frames.last_mut() {
                Some(Frame::List { items, .. }) => items.push(blocks),
                _ => {
                    for block in blocks {
                        self.push_block(block);
                    }
                }
            },
            Frame::Table(table) => {
                if !table.head.is_empty() || !table.rows.is_empty() {
                    self.push_block(Block::Table { head: table.head, rows: table.rows });
                }
            }
            Frame::Root(_) => {}
        }
    }

    fn start_item(&mut self) {
        // An item without its list (HTML) gets one.
        if !matches!(self.frames.last(), Some(Frame::List { .. })) {
            self.close(|f| matches!(f, Frame::Item(_)));
            if !matches!(self.frames.last(), Some(Frame::List { .. })) {
                self.open(Frame::List { ordered: false, start: 1, items: Vec::new() });
            }
        }
        self.open(Frame::Item(Vec::new()));
    }

    fn table(&mut self) -> Option<&mut TableFrame> {
        self.frames.iter_mut().rev().find_map(|f| match f {
            Frame::Table(table) => Some(table),
            _ => None,
        })
    }

    fn start_row(&mut self) {
        self.end_row();
        if let Some(table) = self.table() {
            table.row_th = true;
        }
    }

    fn end_row(&mut self) {
        self.end_cell();
        if let Some(table) = self.table()
            && !table.row.is_empty()
        {
            let cells = std::mem::take(&mut table.row);
            if (table.in_head || table.row_th) && table.head.is_empty() && table.rows.is_empty() {
                table.head = cells;
            } else {
                table.rows.push(cells);
            }
            table.row_th = false;
        }
    }

    fn start_cell(&mut self, header: bool) {
        self.end_cell();
        if self.table().is_none() {
            return;
        }
        if !header && let Some(table) = self.table() {
            table.row_th = false;
        }
        self.inline.clear();
        self.cell = true;
    }

    fn end_cell(&mut self) {
        if !self.cell {
            return;
        }
        self.cell = false;
        let spans = trim(std::mem::take(&mut self.inline));
        if let Some(table) = self.table() {
            table.row.push(spans);
        }
    }

    fn finish(mut self) -> Vec<Block> {
        self.end_code();
        self.end_cell();
        self.flush();
        while self.frames.len() > 1 {
            let frame = self.frames.pop().expect("more than the root");
            self.finish_frame(frame);
        }
        match self.frames.pop() {
            Some(Frame::Root(blocks)) => blocks,
            _ => Vec::new(),
        }
    }
}

/// Markdown (CommonMark with tables, strikethrough and task lists).
pub fn markdown(text: &str) -> Vec<Block> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let mut b = Builder::new(false);
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => b.flush(),
                Tag::Heading { level, .. } => b.start_heading(level as u8),
                Tag::BlockQuote(_) => b.open(Frame::Quote(Vec::new())),
                Tag::CodeBlock(_) => b.start_code(),
                Tag::List(start) => b.open(Frame::List { ordered: start.is_some(), start: start.unwrap_or(1), items: Vec::new() }),
                Tag::Item => b.start_item(),
                Tag::Table(_) => b.open(Frame::Table(TableFrame::default())),
                Tag::TableHead => {
                    if let Some(table) = b.table() {
                        table.in_head = true;
                    }
                }
                Tag::TableRow => b.start_row(),
                Tag::TableCell => b.start_cell(false),
                Tag::Emphasis => b.push_mark("em", ITALIC),
                Tag::Strong => b.push_mark("strong", BOLD),
                Tag::Strikethrough => b.push_mark("s", STRIKE),
                Tag::Link { dest_url, .. } => b.push_link("a", safe_link(&dest_url)),
                Tag::Image { .. } => {
                    b.push_mark("img", ITALIC);
                    b.text("[image: ");
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => b.flush(),
                TagEnd::Heading(_) => b.end_heading(),
                TagEnd::BlockQuote(_) => b.close(|f| matches!(f, Frame::Quote(_))),
                TagEnd::CodeBlock => b.end_code(),
                TagEnd::List(_) => b.close(|f| matches!(f, Frame::List { .. })),
                TagEnd::Item => b.close(|f| matches!(f, Frame::Item(_))),
                TagEnd::Table => {
                    b.end_row();
                    b.close(|f| matches!(f, Frame::Table(_)));
                }
                TagEnd::TableHead => {
                    b.end_row();
                    if let Some(table) = b.table() {
                        table.in_head = false;
                    }
                }
                TagEnd::TableRow => b.end_row(),
                TagEnd::TableCell => b.end_cell(),
                TagEnd::Emphasis => b.pop_mark("em"),
                TagEnd::Strong => b.pop_mark("strong"),
                TagEnd::Strikethrough => b.pop_mark("s"),
                TagEnd::Link => b.pop_link("a"),
                TagEnd::Image => {
                    b.text("]");
                    b.pop_mark("img");
                }
                _ => {}
            },
            Event::Text(text) => b.text(&text),
            Event::Code(code) => {
                b.push_mark("code", CODE);
                b.text(&code);
                b.pop_mark("code");
            }
            // HTML inside Markdown: its formatting, through the same reader.
            Event::Html(html) | Event::InlineHtml(html) => html_into(&mut b, &html),
            Event::SoftBreak => b.text(" "),
            Event::HardBreak => b.hard_break(),
            Event::Rule => {
                b.flush();
                b.push_block(Block::Rule);
            }
            Event::TaskListMarker(done) => b.text(if done { "☑ " } else { "☐ " }),
            Event::FootnoteReference(name) => b.text(&format!("[{name}]")),
            Event::InlineMath(math) | Event::DisplayMath(math) => b.text(&math),
        }
    }
    b.finish()
}

/// An HTML document's formatting (no scripts, styles or images are used).
pub fn html(text: &str) -> Vec<Block> {
    let mut b = Builder::new(true);
    html_into(&mut b, text);
    b.finish()
}

/// Elements whose content is not shown in the formatted text.
const HIDDEN: &[&str] = &["script", "style", "head", "title", "template", "noscript", "svg", "iframe", "object", "embed", "applet", "select", "textarea", "button", "math", "frameset", "dialog"];
/// Elements that start and end a paragraph.
const BLOCKS: &[&str] = &[
    "p", "div", "section", "article", "header", "footer", "main", "nav", "aside", "figure", "figcaption", "address", "center", "details", "summary", "dl", "dd", "form", "fieldset", "legend", "hgroup", "caption",
];

fn html_into(b: &mut Builder, text: &str) {
    let collapse = b.collapse;
    b.collapse = true;
    let mut hidden: Vec<String> = Vec::new();
    for token in crate::report_html::tokens(text) {
        use crate::report_html::Token;
        match token {
            Token::Start { name, attrs, self_closing } => {
                if !hidden.is_empty() || HIDDEN.contains(&name.as_str()) {
                    if !self_closing && (hidden.is_empty() || hidden.last() == Some(&name)) {
                        hidden.push(name);
                    }
                    continue;
                }
                let attr = |key: &str| attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
                match name.as_str() {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => b.start_heading(name.as_bytes()[1] - b'0'),
                    "br" => b.hard_break(),
                    "hr" => {
                        b.flush();
                        b.push_block(Block::Rule);
                    }
                    "b" | "strong" => b.push_mark(&name, BOLD),
                    "i" | "em" | "cite" | "var" | "dfn" | "q" => b.push_mark(&name, ITALIC),
                    "u" | "ins" => b.push_mark(&name, UNDER),
                    "s" | "strike" | "del" => b.push_mark(&name, STRIKE),
                    "code" | "kbd" | "samp" | "tt" => b.push_mark(&name, CODE),
                    "mark" => b.push_mark(&name, MARK),
                    "a" => b.push_link("a", attr("href").and_then(|h| safe_link(&h))),
                    "pre" => {
                        b.start_code();
                        b.collapse = false;
                    }
                    "blockquote" => b.open(Frame::Quote(Vec::new())),
                    "ul" => b.open(Frame::List { ordered: false, start: 1, items: Vec::new() }),
                    "ol" => b.open(Frame::List { ordered: true, start: attr("start").and_then(|s| s.trim().parse().ok()).unwrap_or(1), items: Vec::new() }),
                    "li" => b.start_item(),
                    "table" => b.open(Frame::Table(TableFrame::default())),
                    "thead" => {
                        if let Some(table) = b.table() {
                            table.in_head = true;
                        }
                    }
                    "tbody" | "tfoot" => {
                        b.end_row();
                        if let Some(table) = b.table() {
                            table.in_head = false;
                        }
                    }
                    "tr" => b.start_row(),
                    "td" => b.start_cell(false),
                    "th" => b.start_cell(true),
                    "img" => {
                        let alt = attr("alt").map(|a| a.trim().to_owned()).filter(|a| !a.is_empty()).unwrap_or_else(|| "image".into());
                        b.push_mark("img", ITALIC);
                        b.text(&format!("[{alt}]"));
                        b.pop_mark("img");
                    }
                    "input" if attr("type").is_some_and(|t| t.eq_ignore_ascii_case("checkbox")) => b.text(if attr("checked").is_some() { "☑ " } else { "☐ " }),
                    "dt" => {
                        b.flush();
                        b.push_mark("dt", BOLD);
                    }
                    other if BLOCKS.contains(&other) => b.flush(),
                    _ => {}
                }
            }
            Token::End(name) => {
                if !hidden.is_empty() {
                    if hidden.last() == Some(&name) {
                        hidden.pop();
                    }
                    continue;
                }
                match name.as_str() {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => b.end_heading(),
                    "b" | "strong" | "i" | "em" | "cite" | "var" | "dfn" | "q" | "u" | "ins" | "s" | "strike" | "del" | "code" | "kbd" | "samp" | "tt" | "mark" => b.pop_mark(&name),
                    "a" => b.pop_link("a"),
                    "pre" => {
                        b.end_code();
                        b.collapse = true;
                    }
                    "blockquote" => b.close(|f| matches!(f, Frame::Quote(_))),
                    "ul" | "ol" => b.close(|f| matches!(f, Frame::List { .. })),
                    "li" => b.close(|f| matches!(f, Frame::Item(_))),
                    "table" => {
                        b.end_row();
                        b.close(|f| matches!(f, Frame::Table(_)));
                    }
                    "thead" => {
                        b.end_row();
                        if let Some(table) = b.table() {
                            table.in_head = false;
                        }
                    }
                    "tr" => b.end_row(),
                    "td" | "th" => b.end_cell(),
                    "dt" => {
                        b.pop_mark("dt");
                        b.flush();
                    }
                    other if BLOCKS.contains(&other) => b.flush(),
                    _ => {}
                }
            }
            Token::Text(text) if hidden.is_empty() => b.text(&canvas_model::reports::decode_entities(text)),
            Token::Text(_) => {}
        }
    }
    b.collapse = collapse;
}

/// A plain message: its lines as paragraphs.
pub fn message(text: &str) -> Vec<Block> {
    text.replace("\r\n", "\n")
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let mut spans = Vec::new();
            for (i, line) in p.lines().enumerate() {
                if i > 0 {
                    spans.push(Span { text: "\n".into(), marks: 0, link: None });
                }
                spans.extend(linkify(line));
            }
            Block::Para(spans)
        })
        .collect()
}

/// Plain text with its http(s) addresses as links.
fn linkify(line: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    for (i, word) in line.split(' ').enumerate() {
        if i > 0 {
            plain.push(' ');
        }
        let url = word.trim_end_matches(['.', ',', ';', ')', '!', '?']);
        match (url.starts_with("http://") || url.starts_with("https://")).then(|| safe_link(url)).flatten() {
            Some(link) => {
                if !plain.is_empty() {
                    spans.push(Span { text: std::mem::take(&mut plain), marks: 0, link: None });
                }
                spans.push(Span { text: url.to_owned(), marks: 0, link: Some(link) });
                plain.push_str(&word[url.len()..]);
            }
            None => plain.push_str(word),
        }
    }
    if !plain.is_empty() {
        spans.push(Span { text: plain, marks: 0, link: None });
    }
    spans
}

// ---------- layout ----------

fn style(marks: u8, link: bool, base: Style) -> Style {
    let mut s = base;
    if marks & BOLD != 0 {
        s = s.add_modifier(Modifier::BOLD);
    }
    if marks & ITALIC != 0 {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if marks & UNDER != 0 {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    if marks & STRIKE != 0 {
        s = s.add_modifier(Modifier::CROSSED_OUT);
    }
    if marks & CODE != 0 {
        s = s.fg(PURPLE).bg(PANEL_HI);
    }
    if marks & MARK != 0 {
        s = s.fg(Color::Black).bg(ATTENTION);
    }
    if link {
        s = s.fg(BLUE).add_modifier(Modifier::UNDERLINED);
    }
    s
}

type Prefix = Vec<(String, Style)>;

fn prefix_width(prefix: &Prefix) -> usize {
    prefix.iter().map(|(t, _)| text_width(t)).sum()
}

struct Out {
    lines: Vec<Line>,
    width: usize,
}

impl Out {
    fn line(&mut self, prefix: &Prefix, spans: Vec<(String, Style, Option<String>)>) {
        let mut line = Line { spans: prefix.iter().map(|(t, s)| (t.clone(), *s, None)).collect() };
        line.spans.extend(spans);
        self.lines.push(line);
    }

    fn blank(&mut self, prefix: &Prefix) {
        if self.lines.last().is_some_and(|l| l.text().trim().is_empty()) || self.lines.is_empty() {
            return;
        }
        // A quote's bar continues over its blank lines.
        let bars: Prefix = prefix.iter().filter(|(t, _)| t.contains('▎')).cloned().collect();
        self.line(&bars, Vec::new());
    }

    /// Word-wraps spans after the prefixes (`first` on the first line).
    fn wrap(&mut self, spans: &[Span], base: Style, first: &Prefix, rest: &Prefix) {
        // Words: pieces of text glued together (styles change inside a word
        // without a space), each with whether a space comes before it.
        enum Atom {
            Break,
            Word(Vec<(String, Style, Option<String>)>, bool),
        }
        let mut atoms: Vec<Atom> = Vec::new();
        let mut space = false;
        for span in spans {
            if span.text == "\n" {
                atoms.push(Atom::Break);
                space = false;
                continue;
            }
            let s = style(span.marks, span.link.is_some(), base);
            let mut chunk = String::new();
            let flush = |chunk: &mut String, space: &mut bool, atoms: &mut Vec<Atom>| {
                if chunk.is_empty() {
                    return;
                }
                let piece = (std::mem::take(chunk), s, span.link.clone());
                match atoms.last_mut() {
                    Some(Atom::Word(parts, _)) if !*space => parts.push(piece),
                    _ => atoms.push(Atom::Word(vec![piece], *space)),
                }
                *space = false;
            };
            for c in span.text.chars() {
                if c == ' ' || c == '\t' {
                    flush(&mut chunk, &mut space, &mut atoms);
                    space = true;
                } else {
                    chunk.push(c);
                }
            }
            flush(&mut chunk, &mut space, &mut atoms);
        }
        let mut prefix = first;
        let mut line: Vec<(String, Style, Option<String>)> = Vec::new();
        let mut x = prefix_width(prefix);
        for atom in atoms {
            let parts = match atom {
                Atom::Break => {
                    self.line(prefix, std::mem::take(&mut line));
                    prefix = rest;
                    x = prefix_width(prefix);
                    continue;
                }
                Atom::Word(parts, space_before) => {
                    let w: usize = parts.iter().map(|p| text_width(&p.0)).sum();
                    let gap = space_before && !line.is_empty();
                    if !line.is_empty() && x + usize::from(gap) + w > self.width {
                        self.line(prefix, std::mem::take(&mut line));
                        prefix = rest;
                        x = prefix_width(prefix);
                    } else if gap {
                        // A space inside a link belongs to the link.
                        let same_link = parts[0].2.is_some() && line.last().is_some_and(|l| l.2 == parts[0].2);
                        line.push((" ".into(), if same_link { parts[0].1 } else { base }, if same_link { parts[0].2.clone() } else { None }));
                        x += 1;
                    }
                    parts
                }
            };
            for (mut text, s, link) in parts {
                // Text longer than the line is cut into pieces.
                while x + text_width(&text) > self.width && prefix_width(rest) + 2 < self.width {
                    let room = self.width.saturating_sub(x);
                    if room == 0 {
                        self.line(prefix, std::mem::take(&mut line));
                        prefix = rest;
                        x = prefix_width(prefix);
                        continue;
                    }
                    let mut cut = 0;
                    let mut used = 0;
                    for c in text.chars() {
                        let cw = text_width(c.encode_utf8(&mut [0; 4]));
                        if used + cw > room && cut > 0 {
                            break;
                        }
                        cut += c.len_utf8();
                        used += cw;
                    }
                    let tail = text.split_off(cut);
                    line.push((text, s, link.clone()));
                    self.line(prefix, std::mem::take(&mut line));
                    prefix = rest;
                    x = prefix_width(prefix);
                    text = tail;
                }
                x += text_width(&text);
                line.push((text, s, link));
            }
        }
        if !line.is_empty() || std::ptr::eq(prefix, first) {
            self.line(prefix, line);
        }
    }

    fn blocks(&mut self, blocks: &[Block], first: &Prefix, rest: &Prefix, tight: bool) {
        for (i, block) in blocks.iter().enumerate() {
            let lead = if i == 0 { first } else { rest };
            if i > 0 && !tight {
                self.blank(rest);
            }
            self.block(block, lead, rest);
        }
    }

    fn block(&mut self, block: &Block, first: &Prefix, rest: &Prefix) {
        let width = self.width;
        match block {
            Block::Heading(level, spans) => {
                let base = match level {
                    1 => Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                    2 | 3 => Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    _ => Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                };
                let start = self.lines.len();
                self.wrap(spans, base, first, rest);
                if *level <= 2 {
                    let widest = self.lines[start..].iter().map(|l| text_width(&l.text())).max().unwrap_or(0).saturating_sub(prefix_width(rest));
                    let (glyph, color) = if *level == 1 { ("━", ACCENT) } else { ("─", FAINT) };
                    self.line(rest, vec![(glyph.repeat(widest.max(3)), Style::default().fg(color), None)]);
                }
            }
            Block::Para(spans) => self.wrap(spans, Style::default().fg(TEXT), first, rest),
            Block::Code(code) => {
                let room = width.saturating_sub(prefix_width(rest) + 2).max(8);
                let code_style = Style::default().fg(GRAY).bg(PANEL_HI);
                for (i, raw) in code.lines().enumerate() {
                    let line = raw.replace('\t', "    ");
                    let mut chunks: Vec<String> = Vec::new();
                    let mut current = String::new();
                    for c in line.chars() {
                        if text_width(&current) + text_width(&c.to_string()) > room {
                            chunks.push(std::mem::take(&mut current));
                        }
                        current.push(c);
                    }
                    chunks.push(current);
                    for (j, chunk) in chunks.into_iter().enumerate() {
                        let pad = room.saturating_sub(text_width(&chunk));
                        let lead = if i == 0 && j == 0 { first } else { rest };
                        self.line(lead, vec![(format!(" {chunk}{} ", " ".repeat(pad)), code_style, None)]);
                    }
                }
            }
            Block::Quote(blocks) => {
                let bar = ("▎ ".to_owned(), Style::default().fg(MUTED));
                let mut q_first = first.clone();
                q_first.push(bar.clone());
                let mut q_rest = rest.clone();
                q_rest.push(bar);
                self.blocks(blocks, &q_first, &q_rest, false);
            }
            Block::List { ordered, start, items } => {
                let depth = rest.iter().filter(|(t, _)| t.trim().is_empty() || t.contains('•') || t.contains('◦') || t.contains('▪') || t.ends_with(". ")).count();
                let bullet = ["• ", "◦ ", "▪ "][depth.min(2)];
                let numbers: Vec<String> = (0..items.len()).map(|i| format!("{}. ", start + i as u64)).collect();
                let wide = numbers.iter().map(|n| text_width(n)).max().unwrap_or(2);
                for (i, item) in items.iter().enumerate() {
                    let lead = if i == 0 { first } else { rest };
                    let marker = if *ordered { format!("{:>w$}", numbers[i], w = wide) } else { bullet.to_owned() };
                    let mut i_first = lead.clone();
                    i_first.push((marker.clone(), Style::default().fg(ACCENT)));
                    let mut i_rest = rest.clone();
                    i_rest.push((" ".repeat(text_width(&marker)), Style::default()));
                    if item.is_empty() {
                        self.line(&i_first, Vec::new());
                    }
                    let tight = item.iter().all(|b| matches!(b, Block::Para(_) | Block::List { .. }));
                    self.blocks(item, &i_first, &i_rest, tight);
                }
            }
            Block::Table { head, rows } => self.table(head, rows, first, rest),
            Block::Rule => {
                let room = width.saturating_sub(prefix_width(rest)).max(3);
                self.line(first, vec![("─".repeat(room), Style::default().fg(FAINT), None)]);
            }
        }
    }

    fn table(&mut self, head: &[Vec<Span>], rows: &[Vec<Vec<Span>>], first: &Prefix, rest: &Prefix) {
        let columns = rows.iter().map(Vec::len).chain(std::iter::once(head.len())).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let cell_text = |cell: &[Span]| cell.iter().map(|s| s.text.replace('\n', " ")).collect::<String>();
        let mut natural = vec![1usize; columns];
        for row in std::iter::once(head).chain(rows.iter().map(Vec::as_slice)) {
            for (i, cell) in row.iter().enumerate() {
                natural[i] = natural[i].max(text_width(&cell_text(cell)));
            }
        }
        let room = self.width.saturating_sub(prefix_width(rest) + 3 * (columns - 1)).max(columns * 3);
        let mut widths = natural.clone();
        while widths.iter().sum::<usize>() > room {
            // Narrow the widest column one cell at a time.
            let Some((widest, _)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else { break };
            if widths[widest] <= 3 {
                break;
            }
            widths[widest] -= 1;
        }
        let sep = Style::default().fg(FAINT);
        let render = |out: &mut Out, row: &[Vec<Span>], bold: bool, lead: &Prefix| {
            let mut spans: Vec<(String, Style, Option<String>)> = Vec::new();
            for (i, w) in widths.iter().enumerate() {
                if i > 0 {
                    spans.push((" │ ".into(), sep, None));
                }
                let mut used = 0;
                if let Some(cell) = row.get(i) {
                    for span in cell {
                        let text = span.text.replace('\n', " ");
                        let mut s = style(span.marks, span.link.is_some(), Style::default().fg(TEXT));
                        if bold {
                            s = s.add_modifier(Modifier::BOLD);
                        }
                        let left = w.saturating_sub(used);
                        if left == 0 {
                            break;
                        }
                        let shown = if text_width(&text) > left { crate::paint::truncate(&text, left) } else { text };
                        used += text_width(&shown);
                        spans.push((shown, s, span.link.clone()));
                    }
                }
                if used < *w {
                    spans.push((" ".repeat(w - used), Style::default(), None));
                }
            }
            out.line(lead, spans);
        };
        let mut lead = first;
        if !head.is_empty() {
            render(self, head, true, lead);
            lead = rest;
            let line: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            self.line(rest, vec![(line.join("─┼─"), sep, None)]);
        }
        for row in rows {
            render(self, row, false, lead);
            lead = rest;
        }
    }
}

/// The blocks as lines `width` cells wide (a blank line between blocks).
pub fn layout(blocks: &[Block], width: usize) -> Vec<Line> {
    let mut out = Out { lines: Vec::new(), width: width.max(16) };
    out.blocks(blocks, &Vec::new(), &Vec::new(), false);
    out.lines
}

/// The lines as plain text (copying a report).
pub fn plain(lines: &[Line]) -> String {
    lines.iter().map(|l| l.text().trim_end().to_owned()).collect::<Vec<_>>().join("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.text().trim_end().to_owned()).collect()
    }

    fn span(text: &str, marks: u8) -> Span {
        Span { text: text.into(), marks, link: None }
    }

    #[test]
    fn markdown_keeps_headings_emphasis_lists_code_tables_links_and_quotes() {
        let md = "# Morning brief\n\nSome **bold** and *italic* and `code` and ~~gone~~ text, see [the PR](https://github.com/o/r/pull/1) or [local](file:///C:/x).\n\n## Needs you\n\n- first\n- second\n  - nested\n1. one\n2. two\n\n```rust\nfn main() {}\n```\n\n> quoted **text**\n\n| Area | State |\n|---|---|\n| Build | green |\n\n---\n\n- [x] done\n- [ ] open\n";
        let blocks = markdown(md);
        assert_eq!(blocks[0], Block::Heading(1, vec![span("Morning brief", 0)]));
        let Block::Para(spans) = &blocks[1] else { panic!("{:?}", blocks[1]) };
        assert!(spans.contains(&span("bold", BOLD)) && spans.contains(&span("italic", ITALIC)) && spans.contains(&span("code", CODE)) && spans.contains(&span("gone", STRIKE)));
        assert_eq!(spans.iter().find(|s| s.text == "the PR").unwrap().link.as_deref(), Some("https://github.com/o/r/pull/1"));
        assert!(spans.iter().any(|s| s.text.contains("local") && s.link.is_none()), "only web links are links");
        assert!(matches!(&blocks[3], Block::List { ordered: false, items, .. } if items.len() == 2 && matches!(items[1].last(), Some(Block::List { .. }))));
        assert!(matches!(&blocks[4], Block::List { ordered: true, start: 1, items } if items.len() == 2));
        assert_eq!(blocks[5], Block::Code("fn main() {}".into()));
        assert!(matches!(&blocks[6], Block::Quote(inner) if matches!(&inner[0], Block::Para(s) if s.contains(&span("text", BOLD)))));
        assert_eq!(blocks[7], Block::Table { head: vec![vec![span("Area", 0)], vec![span("State", 0)]], rows: vec![vec![vec![span("Build", 0)], vec![span("green", 0)]]] });
        assert_eq!(blocks[8], Block::Rule);
        let lines = texts(&layout(&blocks, 60));
        let at = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle:?} in {lines:#?}"));
        assert_eq!(lines[0], "Morning brief");
        assert_eq!(lines[1], "━━━━━━━━━━━━━", "a level-1 heading is underlined");
        assert!(lines[at("first")].starts_with("• first") && lines[at("nested")].starts_with("  ◦ nested"));
        assert!(lines.iter().any(|l| l == "1. one") && lines.iter().any(|l| l == "2. two"), "{lines:#?}");
        assert!(lines[at("fn main")].starts_with(" fn main() {}"));
        assert!(lines[at("quoted")].starts_with("▎ quoted text"));
        assert_eq!(lines[at("Area")], "Area  │ State");
        assert_eq!(lines[at("Area") + 1], "──────┼──────");
        assert_eq!(lines[at("Build")], "Build │ green");
        assert!(lines[at("done")].starts_with("• ☑ done") && lines[at("open")].starts_with("• ☐ open"));
        // Styles reach the laid-out spans; the link keeps its address.
        let all: Vec<(String, Style, Option<String>)> = layout(&blocks, 60).into_iter().flat_map(|l| l.spans).collect();
        assert!(all.iter().any(|(t, s, _)| t == "bold" && s.add_modifier.contains(Modifier::BOLD)));
        assert!(all.iter().any(|(t, s, l)| t == "PR" && s.add_modifier.contains(Modifier::UNDERLINED) && l.as_deref() == Some("https://github.com/o/r/pull/1")));
    }

    #[test]
    fn paragraphs_wrap_at_the_width_and_long_words_are_cut() {
        let lines = texts(&layout(&markdown("alpha beta gamma delta epsilon zeta eta theta iota kappa"), 20));
        assert_eq!(lines, ["alpha beta gamma", "delta epsilon zeta", "eta theta iota kappa"]);
        assert!(lines.iter().all(|l| text_width(l) <= 20));
        let long = texts(&layout(&markdown(&"x".repeat(50)), 20));
        assert_eq!(long, ["x".repeat(20), "x".repeat(20), "x".repeat(10)]);
        // Styles glued to a word stay glued: no space appears.
        let glued = texts(&layout(&markdown("a**b**c d"), 20));
        assert_eq!(glued, ["abc d"]);
    }

    #[test]
    fn html_gets_the_same_formatting_and_hides_scripts_and_styles() {
        let page = "<html><head><title>T</title><style>p{color:red}</style></head><body>\n<h1>Daily   triage</h1>\n<p>Two <b>new</b>\n  bugs &amp; <a href=\"https://x.example/1\">one PR</a>.<br>Next line</p>\n<script>alert('x')</script>\n<ul><li>a</li><li>b<ol start=3><li>c</li></ol></li></ul>\n<table><tr><th>K</th><th>V</th></tr><tr><td>x</td><td><i>y</i></td></tr></table>\n<pre>  keep   spaces\nline 2</pre><blockquote>note</blockquote><img src=\"data:image/png;base64,AA\" alt=\"chart\"><hr></body></html>";
        let lines = texts(&layout(&html(page), 50));
        assert_eq!(lines[0], "Daily triage");
        let joined = lines.join("\n");
        assert!(joined.contains("Two new bugs & one PR.\nNext line"), "{joined}");
        assert!(!joined.contains("alert") && !joined.contains("color:red") && !lines.iter().any(|l| l == "T"), "{joined}");
        assert!(joined.contains("• a") && joined.contains("• b") && joined.contains("  3. c"), "{joined}");
        assert!(joined.contains("K │ V") && joined.contains("x │ y"), "{joined}");
        assert!(joined.contains("   keep   spaces") && joined.contains("▎ note") && joined.contains("[chart]"), "{joined}");
        let blocks = html("<p>see <a href=\"javascript:alert(1)\">this</a></p>");
        let Block::Para(spans) = &blocks[0] else { panic!() };
        assert!(spans.iter().all(|s| s.link.is_none()), "a script URL is no link");
    }

    #[test]
    fn messages_keep_their_lines_and_link_web_addresses() {
        let blocks = message("PR 12 received 12 new comments; dispatched the owning session.\nSee https://github.com/o/r/pull/12.");
        let lines = layout(&blocks, 80);
        assert_eq!(texts(&lines), ["PR 12 received 12 new comments; dispatched the owning session.", "See https://github.com/o/r/pull/12."]);
        assert!(lines[1].spans.iter().any(|(t, _, l)| t == "https://github.com/o/r/pull/12" && l.is_some()));
        assert_eq!(plain(&lines), "PR 12 received 12 new comments; dispatched the owning session.\r\nSee https://github.com/o/r/pull/12.");
    }
}
