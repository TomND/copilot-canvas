//! Markdown for the editor, line by line: the block a line belongs to (front
//! matter, fenced code), its syntax pieces, the Source mode styles and the
//! Live mode display (syntax hidden, with a map back to raw columns).

use ratatui::style::{Modifier, Style};

use crate::paint::{ACCENT, BLUE, FAINT, GRAY, MUTED, PANEL_HI, POSITIVE, PURPLE, TEXT};

/// Lines longer than this are formatted up to here; the rest is plain.
const MAX_INLINE: usize = 4000;
/// How far a closing delimiter is looked for.
const WINDOW: usize = 400;
/// Nesting of emphasis and links followed.
const MAX_DEPTH: usize = 6;

const BOLD: u8 = 1;
const ITALIC: u8 = 1 << 1;
const CODE: u8 = 1 << 2;
const LINK: u8 = 1 << 3;
const STRIKE: u8 = 1 << 4;

/// The block a line belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) enum LineKind {
    #[default]
    Text,
    /// Front matter, including its `---` lines.
    Front,
    /// A code fence's opening or closing line.
    Fence,
    /// Inside a fenced code block.
    Code,
}

/// What the lines so far leave open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) struct BlockState {
    /// Inside front matter (it starts with `---` on the first line).
    front: bool,
    /// An open fence: its character and length.
    fence: Option<(char, usize)>,
}

/// The kind of line `index` (`state` is the state after the line before)
/// and the state after it.
pub(super) fn step(state: BlockState, index: usize, line: &str) -> (LineKind, BlockState) {
    let mut next = state;
    if index == 0 && line.trim_end() == "---" {
        next.front = true;
        return (LineKind::Front, next);
    }
    if state.front {
        if matches!(line.trim_end(), "---" | "...") {
            next.front = false;
        }
        return (LineKind::Front, next);
    }
    let body = line.trim_start_matches(' ');
    if line.len() - body.len() > 3 {
        return (if state.fence.is_some() { LineKind::Code } else { LineKind::Text }, next);
    }
    let first = body.chars().next().unwrap_or(' ');
    let run = body.chars().take_while(|c| *c == first).count();
    match state.fence {
        Some((ch, len)) => {
            if first == ch && run >= len && body[run * ch.len_utf8()..].trim().is_empty() {
                next.fence = None;
                return (LineKind::Fence, next);
            }
            (LineKind::Code, next)
        }
        None if matches!(first, '`' | '~') && run >= 3 && !(first == '`' && body[run..].contains('`')) => {
            next.fence = Some((first, run));
            (LineKind::Fence, next)
        }
        None => (LineKind::Text, next),
    }
}

/// A thematic break (`---`, `***`, `___`, spaces allowed).
pub(super) fn is_rule(chars: &[char]) -> bool {
    let mut marks = chars.iter().filter(|c| !c.is_whitespace());
    let Some(&first) = marks.next() else { return false };
    matches!(first, '-' | '*' | '_') && chars.iter().all(|c| *c == first || *c == ' ' || *c == '\t') && chars.iter().filter(|c| **c == first).count() >= 3
}

/// Colours of shown syntax (list markers, quote bars, rules).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Accent,
    Muted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// Content (with `flags`).
    Text,
    /// Syntax hidden in Live mode, dimmed in Source mode.
    Syntax,
    /// Syntax shown in both modes (ordered list numbers).
    Mark(Tone),
    /// Syntax replaced by a glyph in Live mode (bullets, task boxes, quotes).
    Glyph(&'static str, Tone),
    /// A thematic break: a line in Live mode.
    Rule,
}

#[derive(Clone, Copy, Debug)]
struct Piece {
    start: usize,
    end: usize,
    role: Role,
    flags: u8,
}

/// Line-wide formatting.
#[derive(Clone, Copy, Debug, Default)]
struct LineInfo {
    heading: usize,
    quote: bool,
    done: bool,
}

/// An inline construct found at a position.
enum Span {
    /// `\*`: the backslash is syntax.
    Escape,
    /// A code span: the backtick run's length and where the closing run starts.
    Code(usize, usize),
    /// `[[target|alias]]` (with `!` before: open is 3): open length, where
    /// `]]` starts and the `|` (when there is an alias).
    Wiki(usize, usize, Option<usize>),
    /// `[text](url)` (`![alt](src)`: open is 2): open length, `]` and `)`.
    Link(usize, usize, usize),
    /// Emphasis: delimiter length, where the closer starts, flags.
    Emph(usize, usize, u8),
    /// A bare http(s) address ending here.
    Url(usize),
    /// Delimiters without a match: plain text of this length.
    Skip(usize),
}

struct Parser<'a> {
    c: &'a [char],
    out: Vec<Piece>,
}

impl Parser<'_> {
    fn push(&mut self, start: usize, end: usize, role: Role, flags: u8) {
        if end > start {
            self.out.push(Piece { start, end, role, flags });
        }
    }

    fn run(&self, i: usize, end: usize) -> usize {
        let ch = self.c[i];
        self.c[i..end].iter().take_while(|c| **c == ch).count()
    }

    fn inline(&mut self, start: usize, end: usize, flags: u8, depth: usize) {
        if depth > MAX_DEPTH {
            self.push(start, end, Role::Text, flags);
            return;
        }
        let (mut i, mut text) = (start, start);
        while i < end {
            let Some(span) = self.detect(i, end) else {
                i += 1;
                continue;
            };
            if let Span::Skip(len) = span {
                i += len.max(1);
                continue;
            }
            self.push(text, i, Role::Text, flags);
            i = self.emit(span, i, end, flags, depth).max(i + 1);
            text = i;
        }
        self.push(text, end, Role::Text, flags);
    }

    fn emit(&mut self, span: Span, i: usize, end: usize, flags: u8, depth: usize) -> usize {
        match span {
            Span::Escape => {
                self.push(i, i + 1, Role::Syntax, flags);
                self.push(i + 1, i + 2, Role::Text, flags);
                i + 2
            }
            Span::Code(run, close) => {
                self.push(i, i + run, Role::Syntax, flags | CODE);
                self.push(i + run, close, Role::Text, flags | CODE);
                self.push(close, close + run, Role::Syntax, flags | CODE);
                close + run
            }
            Span::Wiki(open, close, pipe) => {
                let shown = match pipe {
                    Some(p) => {
                        self.push(i, p + 1, Role::Syntax, flags);
                        p + 1
                    }
                    None => {
                        self.push(i, i + open, Role::Syntax, flags);
                        i + open
                    }
                };
                self.push(shown, close, Role::Text, flags | LINK);
                self.push(close, close + 2, Role::Syntax, flags);
                close + 2
            }
            Span::Link(open, bracket, paren) => {
                self.push(i, i + open, Role::Syntax, flags);
                self.inline(i + open, bracket, flags | LINK, depth + 1);
                self.push(bracket, paren + 1, Role::Syntax, flags | LINK);
                paren + 1
            }
            Span::Emph(len, close, flag) => {
                self.push(i, i + len, Role::Syntax, flags);
                self.inline(i + len, close, flags | flag, depth + 1);
                self.push(close, close + len, Role::Syntax, flags);
                close + len
            }
            Span::Url(stop) => {
                self.push(i, stop, Role::Text, flags | LINK);
                stop
            }
            Span::Skip(len) => (i + len).min(end),
        }
    }

    fn detect(&self, i: usize, end: usize) -> Option<Span> {
        let c = self.c;
        let at = |j: usize| (j < end).then(|| c[j]);
        match c[i] {
            '\\' => at(i + 1).is_some_and(|n| n.is_ascii_punctuation()).then_some(Span::Escape),
            '`' => {
                let run = self.run(i, end);
                let mut k = i + run;
                while k < end.min(i + run + WINDOW) {
                    if c[k] == '`' {
                        let other = self.run(k, end);
                        if other == run {
                            return Some(Span::Code(run, k));
                        }
                        k += other;
                    } else {
                        k += 1;
                    }
                }
                Some(Span::Skip(run))
            }
            '!' if at(i + 1) == Some('[') => self.bracket(i, 1, end),
            '[' => self.bracket(i, 0, end),
            '*' | '_' => Some(self.emphasis(i, end)),
            '~' if at(i + 1) == Some('~') => {
                if at(i + 2).is_none_or(char::is_whitespace) || at(i + 2) == Some('~') {
                    return Some(Span::Skip(self.run(i, end)));
                }
                let limit = end.min(i + 3 + WINDOW).saturating_sub(1);
                (i + 3..limit).find(|&k| c[k] == '~' && c[k + 1] == '~' && !c[k - 1].is_whitespace()).map(|k| Span::Emph(2, k, STRIKE)).or(Some(Span::Skip(2)))
            }
            'h' if i == 0 || !c[i - 1].is_alphanumeric() => {
                let starts = |p: &str| p.chars().enumerate().all(|(n, ch)| at(i + n) == Some(ch));
                if !(starts("https://") || starts("http://")) {
                    return None;
                }
                let mut stop = (i..end).find(|&k| c[k].is_whitespace()).unwrap_or(end);
                while stop > i && matches!(c[stop - 1], '.' | ',' | ';' | ':' | '!' | '?' | ')' | '"' | '\'' | '>') {
                    stop -= 1;
                }
                (stop > i + 8).then_some(Span::Url(stop))
            }
            _ => None,
        }
    }

    /// `[[wiki]]`, `![[embed]]`, `[text](url)` or `![alt](src)` at `i`
    /// (`bang`: 1 when `i` is the `!`).
    fn bracket(&self, i: usize, bang: usize, end: usize) -> Option<Span> {
        let c = self.c;
        let open = i + bang;
        let limit = end.min(open + 2 + WINDOW);
        if open + 1 < end && c[open + 1] == '[' {
            let start = open + 2;
            let mut k = start;
            while k + 1 < limit {
                match c[k] {
                    '[' => return None,
                    ']' if c[k + 1] == ']' && k > start => {
                        let pipe = (start..k).find(|&p| c[p] == '|').filter(|&p| p > start && p + 1 < k);
                        return Some(Span::Wiki(bang + 2, k, pipe));
                    }
                    ']' => return None,
                    _ => k += 1,
                }
            }
            return None;
        }
        let (mut depth, mut k) = (0usize, open + 1);
        let bracket = loop {
            if k >= limit {
                return None;
            }
            match c[k] {
                '[' => depth += 1,
                ']' if depth == 0 => break k,
                ']' => depth -= 1,
                '\\' => k += 1,
                _ => {}
            }
            k += 1;
        };
        if bracket + 1 >= end || c[bracket + 1] != '(' {
            return None;
        }
        let (mut depth, mut k) = (0usize, bracket + 2);
        let limit = end.min(bracket + 2 + WINDOW);
        let paren = loop {
            if k >= limit {
                return None;
            }
            match c[k] {
                '(' => depth += 1,
                ')' if depth == 0 => break k,
                ')' => depth -= 1,
                _ => {}
            }
            k += 1;
        };
        Some(Span::Link(bang + 1, bracket, paren))
    }

    /// `*em*`, `_em_`, `**strong**`, `***both***` at `i`.
    fn emphasis(&self, i: usize, end: usize) -> Span {
        let c = self.c;
        let ch = c[i];
        let run = self.run(i, end);
        let after = (i + run < end).then(|| c[i + run]);
        if after.is_none_or(char::is_whitespace) || (ch == '_' && i > 0 && c[i - 1].is_alphanumeric()) {
            return Span::Skip(run);
        }
        let limit = end.min(i + run + WINDOW);
        for len in (1..=run.min(3)).rev() {
            let mut k = i + len + 1;
            while k < limit {
                if c[k] != ch {
                    k += 1;
                    continue;
                }
                let other = self.run(k, end);
                let word_after = ch == '_' && k + other < end && c[k + other].is_alphanumeric();
                if other == len && !c[k - 1].is_whitespace() && !word_after {
                    return Span::Emph(len, k, [ITALIC, BOLD, BOLD | ITALIC][len - 1]);
                }
                k += other;
            }
        }
        Span::Skip(run)
    }
}

/// Splits a line into pieces covering every character in order.
fn analyze(chars: &[char], kind: LineKind) -> (LineInfo, Vec<Piece>) {
    let n = chars.len();
    let mut info = LineInfo::default();
    let mut p = Parser { c: chars, out: Vec::new() };
    if kind != LineKind::Text {
        p.push(0, n, Role::Text, 0);
        return (info, p.out);
    }
    let mut i = 0;
    // Quote markers, each `>` with up to three spaces before it.
    loop {
        let mut j = i;
        while j < n && chars[j] == ' ' && j - i < 3 {
            j += 1;
        }
        if j >= n || chars[j] != '>' {
            break;
        }
        p.push(i, j, Role::Text, 0);
        let end = if chars.get(j + 1) == Some(&' ') { j + 2 } else { j + 1 };
        p.push(j, end, Role::Glyph(if end - j == 2 { "▎ " } else { "▎" }, Tone::Muted), 0);
        info.quote = true;
        i = end;
    }
    if is_rule(&chars[i..]) {
        p.push(i, n, Role::Rule, 0);
        return (info, p.out);
    }
    let mut j = i;
    while j < n && chars[j] == ' ' && j - i < 3 {
        j += 1;
    }
    let hashes = chars[j..].iter().take_while(|c| **c == '#').count();
    if (1..=6).contains(&hashes) && chars.get(j + hashes).is_none_or(|c| *c == ' ') {
        let end = (j + hashes + 1).min(n);
        p.push(i, end, Role::Syntax, 0);
        info.heading = hashes;
        i = end;
    } else {
        let mut j = i;
        while j < n && matches!(chars[j], ' ' | '\t') {
            j += 1;
        }
        let at = |k: usize| chars.get(k).copied();
        let digits = chars[j..].iter().take_while(|c| c.is_ascii_digit()).count();
        if matches!(at(j), Some('-' | '*' | '+')) && at(j + 1) == Some(' ') {
            p.push(i, j, Role::Text, 0);
            let task = at(j + 2) == Some('[') && matches!(at(j + 3), Some(' ' | 'x' | 'X')) && at(j + 4) == Some(']') && matches!(at(j + 5), Some(' ') | None);
            if task {
                info.done = at(j + 3) != Some(' ');
                let end = (j + 6).min(n);
                p.push(j, end, Role::Glyph(if info.done { "☑ " } else { "☐ " }, Tone::Accent), 0);
                i = end;
            } else {
                p.push(j, j + 2, Role::Glyph("• ", Tone::Accent), 0);
                i = j + 2;
            }
        } else if (1..=9).contains(&digits) && matches!(at(j + digits), Some('.' | ')')) && at(j + digits + 1) == Some(' ') {
            p.push(i, j, Role::Text, 0);
            p.push(j, j + digits + 2, Role::Mark(Tone::Accent), 0);
            i = j + digits + 2;
        }
    }
    let stop = n.min(MAX_INLINE).max(i);
    p.inline(i, stop, 0, 0);
    p.push(stop, n, Role::Text, 0);
    (info, p.out)
}

fn heading_style(level: usize) -> Style {
    let color = match level {
        1 => ACCENT,
        2 => POSITIVE,
        3 => PURPLE,
        _ => TEXT,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// The style of a line's content.
fn base_style(kind: LineKind, info: &LineInfo) -> Style {
    match kind {
        LineKind::Front | LineKind::Fence => Style::default().fg(MUTED),
        LineKind::Code => Style::default().fg(GRAY).bg(PANEL_HI),
        LineKind::Text if info.heading > 0 => heading_style(info.heading),
        LineKind::Text if info.done => Style::default().fg(MUTED).add_modifier(Modifier::CROSSED_OUT),
        LineKind::Text if info.quote => Style::default().fg(GRAY).add_modifier(Modifier::ITALIC),
        LineKind::Text => Style::default().fg(TEXT),
    }
}

fn piece_style(base: Style, piece: &Piece) -> Style {
    let tone = |t: Tone| Style::default().fg(if t == Tone::Accent { ACCENT } else { MUTED });
    match piece.role {
        Role::Syntax => base.fg(MUTED).remove_modifier(Modifier::CROSSED_OUT | Modifier::UNDERLINED),
        Role::Mark(t) | Role::Glyph(_, t) => tone(t),
        Role::Rule => Style::default().fg(FAINT),
        Role::Text => {
            let mut s = base;
            if piece.flags & BOLD != 0 {
                s = s.add_modifier(Modifier::BOLD);
            }
            if piece.flags & ITALIC != 0 {
                s = s.add_modifier(Modifier::ITALIC);
            }
            if piece.flags & STRIKE != 0 {
                s = s.add_modifier(Modifier::CROSSED_OUT);
            }
            if piece.flags & CODE != 0 {
                s = s.fg(PURPLE).bg(PANEL_HI);
            }
            if piece.flags & LINK != 0 {
                s = s.fg(BLUE).add_modifier(Modifier::UNDERLINED);
            }
            s
        }
    }
}

/// Source mode: the style of every character of `line`.
pub(super) fn source_styles(line: &str, kind: LineKind) -> Vec<Style> {
    let chars: Vec<char> = line.chars().collect();
    let (info, pieces) = analyze(&chars, kind);
    let base = base_style(kind, &info);
    let mut styles = vec![base; chars.len()];
    for piece in &pieces {
        let style = piece_style(base, piece);
        styles[piece.start..piece.end].fill(style);
    }
    styles
}

/// A line as Live mode shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Display {
    pub chars: Vec<char>,
    /// The raw column of each shown character.
    pub raw: Vec<usize>,
    pub styles: Vec<Style>,
}

/// Live mode: `line` formatted, its syntax hidden or replaced by glyphs.
pub(super) fn live(line: &str, kind: LineKind) -> Display {
    let chars: Vec<char> = line.chars().collect();
    let (info, pieces) = analyze(&chars, kind);
    let base = base_style(kind, &info);
    let mut out = Display::default();
    for piece in &pieces {
        let style = piece_style(base, piece);
        let mut put = |ch: char, raw: usize| {
            out.chars.push(ch);
            out.raw.push(raw);
            out.styles.push(style);
        };
        match piece.role {
            Role::Syntax => {}
            Role::Text | Role::Mark(_) => (piece.start..piece.end).for_each(|k| put(chars[k], k)),
            Role::Rule => (piece.start..piece.end).for_each(|k| put('─', k)),
            Role::Glyph(glyph, _) => {
                let count = glyph.chars().count();
                let last = piece.end - 1;
                for (k, ch) in glyph.chars().enumerate() {
                    let raw = if count == piece.end - piece.start { piece.start + k } else if k + 1 == count && count > 1 { last } else { (piece.start + k).min(last) };
                    put(ch, raw);
                }
            }
        }
    }
    out
}
