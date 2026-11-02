//! Find: case-insensitive matches (correct when lowercasing changes a
//! line's length), kept up to date while editing.

use super::{Editor, Find, Pos};

/// Lowercases character by character (as lines are compared).
fn lower(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).collect()
}

/// Appends the matches of `query` (lowercased) in `line` (line `index`).
pub(super) fn line_matches(line: &str, index: usize, query: &str, out: &mut Vec<(Pos, Pos)>) {
    if query.is_empty() || line.is_empty() {
        return;
    }
    if line.is_ascii() {
        let low = line.to_ascii_lowercase();
        out.extend(low.match_indices(query).map(|(at, m)| (Pos::new(index, at), Pos::new(index, at + m.len()))));
        return;
    }
    // Where each character's lowercase starts in the lowercased line.
    let mut low = String::with_capacity(line.len());
    let mut starts = Vec::new();
    for c in line.chars() {
        starts.push(low.len());
        low.extend(c.to_lowercase());
    }
    let mut last_end = 0;
    for (at, m) in low.match_indices(query) {
        let start = starts.partition_point(|&s| s <= at).saturating_sub(1);
        let end = starts.partition_point(|&s| s < at + m.len()).max(start + 1);
        if start < last_end {
            continue;
        }
        last_end = end;
        out.push((Pos::new(index, start), Pos::new(index, end)));
    }
}

impl Editor {
    pub(super) fn find_set(&mut self, query: Option<&str>) {
        let Some(query) = query.filter(|q| !q.is_empty()) else {
            self.find = None;
            return;
        };
        let query = lower(query);
        if self.find.as_ref().is_some_and(|f| f.lower == query) {
            return;
        }
        self.find = Some(Find { lower: query, matches: Vec::new() });
        self.find_refresh();
    }

    /// Finds every match again (new query or new text).
    pub(super) fn find_refresh(&mut self) {
        let Some(find) = self.find.as_mut() else { return };
        find.matches.clear();
        for (i, line) in self.lines.iter().enumerate() {
            line_matches(line, i, &find.lower, &mut find.matches);
        }
    }

    /// Lines `first..first + removed` became `first..first + added`.
    pub(super) fn find_lines_changed(&mut self, first: usize, removed: usize, added: usize) {
        let Some(find) = self.find.as_mut() else { return };
        let lo = find.matches.partition_point(|m| m.0.line < first);
        let hi = find.matches.partition_point(|m| m.0.line < first + removed);
        if added != removed {
            for m in &mut find.matches[hi..] {
                m.0.line = m.0.line + added - removed;
                m.1.line = m.1.line + added - removed;
            }
        }
        let mut fresh = Vec::new();
        for i in first..(first + added).min(self.lines.len()) {
            line_matches(&self.lines[i], i, &find.lower, &mut fresh);
        }
        find.matches.splice(lo..hi, fresh);
    }

    /// The matches on line `index`.
    pub(super) fn line_matches_of(&self, index: usize) -> &[(Pos, Pos)] {
        let Some(find) = &self.find else { return &[] };
        let lo = find.matches.partition_point(|m| m.0.line < index);
        let hi = find.matches.partition_point(|m| m.0.line <= index);
        &find.matches[lo..hi]
    }

    /// Selects the next (or previous) match, wrapping around.
    pub(super) fn find_step(&mut self, forward: bool) -> bool {
        let Some(find) = &self.find else { return false };
        let matches = &find.matches;
        let selected = self.selection();
        let from = selected.map_or(self.cursor, |s| s.0);
        let pick = if forward {
            let i = if selected.is_some() { matches.partition_point(|m| m.0 <= from) } else { matches.partition_point(|m| m.0 < from) };
            matches.get(i).or(matches.first())
        } else {
            let i = matches.partition_point(|m| m.0 < from);
            if i == 0 { matches.last() } else { matches.get(i - 1) }
        };
        let Some(&(start, end)) = pick else { return false };
        self.anchor = Some(start);
        self.cursor = end;
        self.goal = None;
        self.history.seal();
        true
    }
}
