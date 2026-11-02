//! Text helpers: character columns, widths, word classes and the list and
//! quote prefixes that Enter continues.

use unicode_width::UnicodeWidthChar;

use super::Pos;

/// Cells a tab takes (tabs are drawn as spaces).
pub(super) const TAB_WIDTH: usize = 4;

pub(super) fn char_count(text: &str) -> usize {
    text.chars().count()
}

/// The byte index of character column `col` (the end when past it).
pub(super) fn byte_at(text: &str, col: usize) -> usize {
    if col == 0 {
        return 0;
    }
    text.char_indices().nth(col).map(|(i, _)| i).unwrap_or(text.len())
}

/// Cells a character takes: like `paint::text_width`, with tabs as
/// [`TAB_WIDTH`] and other control characters as one cell.
pub(super) fn char_width(c: char) -> usize {
    if c == '\t' {
        TAB_WIDTH
    } else if c.is_control() {
        1
    } else {
        c.width().unwrap_or(0)
    }
}

/// Text as the editor stores it: `\n` line breaks, no other control
/// characters except tabs.
pub(super) fn clean(text: &str) -> String {
    text.replace("\r\n", "\n").chars().map(|c| if c == '\r' { '\n' } else { c }).filter(|c| *c == '\n' || *c == '\t' || !c.is_control()).collect()
}

/// The position after `text` inserted at `at`.
pub(super) fn end_of(at: Pos, text: &str) -> Pos {
    match text.rfind('\n') {
        Some(i) => Pos::new(at.line + text.matches('\n').count(), char_count(&text[i + 1..])),
        None => Pos::new(at.line, at.col + char_count(text)),
    }
}

/// 0 whitespace, 1 word characters, 2 anything else.
pub(super) fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

/// The column a Ctrl+Left from `col` reaches within `chars` (0 at the start).
pub(super) fn word_left(chars: &[char], col: usize) -> usize {
    let mut i = col.min(chars.len());
    while i > 0 && class(chars[i - 1]) == 0 {
        i -= 1;
    }
    if i > 0 {
        let kind = class(chars[i - 1]);
        while i > 0 && class(chars[i - 1]) == kind {
            i -= 1;
        }
    }
    i
}

/// The column a Ctrl+Right from `col` reaches (the end of the next word).
pub(super) fn word_right(chars: &[char], col: usize) -> usize {
    let mut i = col.min(chars.len());
    while i < chars.len() && class(chars[i]) == 0 {
        i += 1;
    }
    if i < chars.len() {
        let kind = class(chars[i]);
        while i < chars.len() && class(chars[i]) == kind {
            i += 1;
        }
    }
    i
}

/// The word (run of one class) around `col`: start and end columns.
pub(super) fn word_at(chars: &[char], col: usize) -> (usize, usize) {
    if chars.is_empty() {
        return (0, 0);
    }
    let at = col.min(chars.len() - 1);
    let kind = class(chars[at]);
    let mut start = at;
    while start > 0 && class(chars[start - 1]) == kind {
        start -= 1;
    }
    let mut end = at + 1;
    while end < chars.len() && class(chars[end]) == kind {
        end += 1;
    }
    (start, end)
}

/// The column of the first non-whitespace character (the length if none).
pub(super) fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

/// A list item's or quote's prefix, for Enter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ListPrefix {
    /// Where the item's content starts (character column).
    pub end: usize,
    /// The prefix of the next line (`- `, `2. `, `- [ ] `, `> `, indented).
    pub next: String,
    /// What the line becomes when Enter ends an empty item.
    pub keep: String,
}

/// Parses `line`'s indentation, quote markers and list marker (with a task box).
pub(super) fn list_prefix(line: &str) -> Option<ListPrefix> {
    let chars: Vec<char> = line.chars().collect();
    let at = |i: usize| chars.get(i).copied();
    let mut i = 0;
    while matches!(at(i), Some(' ' | '\t')) {
        i += 1;
    }
    let indent: String = chars[..i].iter().collect();
    // Quote markers: `>` each followed by an optional space.
    let quote_start = i;
    let mut levels = Vec::new();
    while at(i) == Some('>') {
        levels.push(i);
        i += 1;
        if at(i) == Some(' ') {
            i += 1;
        }
    }
    let quote: String = chars[quote_start..i].iter().collect();
    let inner_start = i;
    if !levels.is_empty() {
        while matches!(at(i), Some(' ' | '\t')) {
            i += 1;
        }
    }
    let inner: String = chars[inner_start..i].iter().collect();
    let mut marker = None;
    if let Some(bullet @ ('-' | '*' | '+')) = at(i)
        && at(i + 1) == Some(' ')
    {
        let task = at(i + 2) == Some('[') && matches!(at(i + 3), Some(' ' | 'x' | 'X')) && at(i + 4) == Some(']') && matches!(at(i + 5), Some(' ') | None);
        if task {
            marker = Some((format!("{bullet} [ ] "), (i + 6).min(chars.len())));
        } else {
            marker = Some((format!("{bullet} "), i + 2));
        }
    } else {
        let digits = chars[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        if (1..=9).contains(&digits) && matches!(at(i + digits), Some('.' | ')')) && at(i + digits + 1) == Some(' ') {
            let number: String = chars[i..i + digits].iter().collect();
            let next = number.parse::<u64>().map(|n| n + 1).unwrap_or(1);
            marker = Some((format!("{next}{} ", chars[i + digits]), i + digits + 2));
        }
    }
    match marker {
        Some((next, end)) => {
            let keep = if levels.is_empty() { String::new() } else { format!("{indent}{quote}") };
            Some(ListPrefix { end, next: format!("{indent}{quote}{inner}{next}"), keep })
        }
        None if !levels.is_empty() => {
            let last = *levels.last().unwrap_or(&quote_start);
            let keep = if levels.len() > 1 { chars[..last].iter().collect() } else { String::new() };
            Some(ListPrefix { end: i, next: format!("{indent}{quote}{inner}"), keep })
        }
        None => None,
    }
}
