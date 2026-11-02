//! Changing the text: the one splice every edit goes through, undo
//! recording, and the editing keys.

use super::history::{Edit, Kind};
use super::markdown::{self, LineKind};
use super::text::{byte_at, char_count, end_of, list_prefix};
use super::{Editor, LineCache, Outcome, Pos};

impl Editor {
    /// Replaces `start..end` with `text` (`\n` breaks lines) without
    /// recording it; answers the end of the inserted text.
    fn splice(&mut self, start: Pos, end: Pos, text: &str) -> Pos {
        let (a, b) = (self.clamp(start), self.clamp(end));
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let tail = {
            let last = &self.lines[end.line];
            last[byte_at(last, end.col)..].to_owned()
        };
        let mut parts = text.split('\n');
        let mut line = {
            let first = &self.lines[start.line];
            let head = &first[..byte_at(first, start.col)];
            let mut line = String::with_capacity(head.len() + text.len() + tail.len());
            line.push_str(head);
            line
        };
        line.push_str(parts.next().unwrap_or(""));
        let mut new_lines = Vec::new();
        for part in parts {
            new_lines.push(std::mem::replace(&mut line, part.to_owned()));
        }
        let after = Pos::new(start.line + new_lines.len(), char_count(&line));
        line.push_str(&tail);
        new_lines.push(line);
        let (removed, added) = (end.line - start.line + 1, new_lines.len());
        let cache_ok = self.cache.len() == self.lines.len();
        self.lines.splice(start.line..=end.line, new_lines);
        if cache_ok {
            self.cache.splice(start.line..=end.line, std::iter::repeat_with(LineCache::default).take(added));
        } else {
            self.cache = vec![LineCache::default(); self.lines.len()];
        }
        self.kinds.truncate(start.line);
        self.view.shown.clear();
        self.find_lines_changed(start.line, removed, added);
        after
    }

    /// Replaces `start..end` with `text`, recorded in the open undo step;
    /// answers the end of the inserted text.
    pub(super) fn edit(&mut self, start: Pos, end: Pos, text: &str) -> Pos {
        let (a, b) = (self.clamp(start), self.clamp(end));
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        if start == end && text.is_empty() {
            return start;
        }
        let removed = self.range_text(start, end);
        let after = self.splice(start, end, text);
        self.history.record(Edit { at: start, removed, inserted: text.to_owned() });
        after
    }

    /// Runs `f` as one change of `kind` (an undo step, or part of the
    /// previous one when it groups).
    pub(super) fn change(&mut self, kind: Kind, f: impl FnOnce(&mut Self)) -> Outcome {
        if self.anchor == Some(self.cursor) {
            self.anchor = None;
        }
        self.history.begin(kind, (self.cursor, self.anchor));
        f(self);
        self.cursor = self.clamp(self.cursor);
        self.anchor = self.anchor.map(|a| self.clamp(a)).filter(|a| *a != self.cursor);
        self.goal = None;
        if self.history.finish((self.cursor, self.anchor)) { Outcome::Changed } else { Outcome::Handled }
    }

    /// Removes the selection (if any); answers whether it did.
    pub(super) fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        self.edit(start, end, "");
        self.cursor = start;
        self.anchor = None;
        true
    }

    pub(super) fn insert_at_cursor(&mut self, text: &str) {
        self.cursor = self.edit(self.cursor, self.cursor, text);
    }

    pub(super) fn type_char(&mut self, c: char) -> Outcome {
        if self.read_only {
            return Outcome::Handled;
        }
        let mut buf = [0u8; 4];
        let text: &str = c.encode_utf8(&mut buf);
        self.change(Kind::Type, |e| {
            e.delete_selection();
            e.insert_at_cursor(text);
        })
    }

    /// Enter: continues a list item or quote; on an empty item removes its marker.
    pub(super) fn newline(&mut self) -> Outcome {
        if self.read_only {
            return Outcome::Handled;
        }
        self.change(Kind::Other, |e| {
            e.delete_selection();
            let Pos { line, col } = e.cursor;
            let kind = e.kind(line);
            let chars: Vec<char> = e.lines[line].chars().collect();
            let prefix = if kind == LineKind::Text && !markdown::is_rule(&chars) { list_prefix(&e.lines[line]) } else { None };
            match prefix {
                Some(prefix) if col >= prefix.end => {
                    if chars[prefix.end.min(chars.len())..].iter().all(|c| c.is_whitespace()) {
                        e.cursor = e.edit(Pos::new(line, 0), Pos::new(line, chars.len()), &prefix.keep);
                    } else {
                        let cursor = e.cursor;
                        e.cursor = e.edit(cursor, cursor, &format!("\n{}", prefix.next));
                    }
                }
                _ => e.insert_at_cursor("\n"),
            }
        })
    }

    /// Tab / Shift+Tab: indents or outdents the line, or every selected
    /// line, by two spaces.
    pub(super) fn indent(&mut self, out: bool) -> Outcome {
        if self.read_only {
            return Outcome::Handled;
        }
        let (first, last) = match self.selection() {
            Some((s, e)) => (s.line, if e.col == 0 && e.line > s.line { e.line - 1 } else { e.line }),
            None => (self.cursor.line, self.cursor.line),
        };
        self.change(Kind::Other, |e| {
            for line in first..=last {
                let shift = |pos: &mut Pos, by: isize| {
                    if pos.line == line {
                        pos.col = pos.col.saturating_add_signed(by);
                    }
                };
                if out {
                    let text = &e.lines[line];
                    let n = if text.starts_with('\t') { 1 } else { text.chars().take(2).take_while(|c| *c == ' ').count() };
                    if n == 0 {
                        continue;
                    }
                    e.edit(Pos::new(line, 0), Pos::new(line, n), "");
                    shift(&mut e.cursor, -(n as isize));
                    if let Some(anchor) = e.anchor.as_mut() {
                        shift(anchor, -(n as isize));
                    }
                } else {
                    if last > first && e.lines[line].is_empty() {
                        continue;
                    }
                    e.edit(Pos::new(line, 0), Pos::new(line, 0), "  ");
                    shift(&mut e.cursor, 2);
                    if let Some(anchor) = e.anchor.as_mut() {
                        shift(anchor, 2);
                    }
                }
            }
        })
    }

    /// Backspace (Ctrl: the word before the caret).
    pub(super) fn backspace(&mut self, word: bool) -> Outcome {
        self.delete_toward(false, word)
    }

    /// Delete (Ctrl: the word after the caret).
    pub(super) fn delete_forward(&mut self, word: bool) -> Outcome {
        self.delete_toward(true, word)
    }

    fn delete_toward(&mut self, right: bool, word: bool) -> Outcome {
        if self.read_only {
            return Outcome::Handled;
        }
        if self.selection().is_some() {
            return self.change(Kind::Other, |e| {
                e.delete_selection();
            });
        }
        let target = self.step_pos(self.cursor, right, word);
        if target == self.cursor {
            self.anchor = None;
            return Outcome::Handled;
        }
        let kind = if word { Kind::Other } else { Kind::Delete };
        self.change(kind, |e| {
            e.anchor = None;
            let cursor = e.cursor;
            e.edit(cursor, target, "");
            e.cursor = cursor.min(target);
        })
    }

    /// Ctrl+X: cuts the selection (read only: copies it).
    pub(super) fn cut(&mut self) -> Outcome {
        let Some(text) = self.selected_text() else { return Outcome::Handled };
        if self.read_only {
            return Outcome::Copy(text);
        }
        self.change(Kind::Other, |e| {
            e.delete_selection();
        });
        Outcome::Cut(text)
    }

    /// Replaces the text with `text` by one edit of the lines that differ;
    /// the caret, the selection and the view stay on the same text. `join`:
    /// the edit joins the last undo step (else, or when undone steps wait,
    /// it is a step of its own). Answers whether the text changed.
    pub(super) fn replace_changed_lines(&mut self, text: &str, join: bool) -> bool {
        if self.read_only {
            return false;
        }
        let new: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
        let old = &self.lines;
        let prefix = old.iter().zip(&new).take_while(|(a, b)| a.as_str() == **b).count();
        let room = old.len().min(new.len()) - prefix;
        let suffix = old.iter().rev().zip(new.iter().rev()).take(room).take_while(|(a, b)| a.as_str() == **b).count();
        let (removed, added) = (old.len() - prefix - suffix, new.len() - prefix - suffix);
        if removed == 0 && added == 0 {
            return false;
        }
        let body = new[prefix..prefix + added].join("\n");
        let end_of_line = |i: usize| Pos::new(i, char_count(&old[i]));
        let (start, end, insert) = if removed > 0 && added > 0 {
            (Pos::new(prefix, 0), end_of_line(prefix + removed - 1), body)
        } else if removed == 0 && prefix < old.len() {
            (Pos::new(prefix, 0), Pos::new(prefix, 0), format!("{body}\n"))
        } else if removed == 0 {
            (end_of_line(prefix - 1), end_of_line(prefix - 1), format!("\n{body}"))
        } else if prefix + removed < old.len() {
            (Pos::new(prefix, 0), Pos::new(prefix + removed, 0), String::new())
        } else {
            (end_of_line(prefix - 1), end_of_line(old.len() - 1), String::new())
        };
        // Positions after the changed lines move with them; inside, they stay on the line.
        let shift = |pos: Pos| -> Pos {
            if pos.line < prefix {
                pos
            } else if pos.line >= prefix + removed {
                Pos::new(pos.line + added - removed, pos.col)
            } else {
                Pos::new(prefix + (pos.line - prefix).min(added.saturating_sub(1)), pos.col)
            }
        };
        let (cursor, anchor) = (shift(self.cursor), self.anchor.map(shift));
        let top = (shift(Pos::new(self.top.0, 0)).line, if self.top.0 < prefix || self.top.0 >= prefix + removed { self.top.1 } else { 0 });
        if !(join && self.history.reopen()) {
            self.history.begin(Kind::Other, (self.cursor, self.anchor));
        }
        self.edit(start, end, &insert);
        self.cursor = self.clamp(cursor);
        self.anchor = anchor.map(|a| self.clamp(a)).filter(|a| *a != self.cursor);
        self.top = (top.0.min(self.lines.len() - 1), top.1);
        self.goal = None;
        self.history.finish((self.cursor, self.anchor));
        true
    }

    /// Takes back (or does again) one step; answers whether there was one.
    pub(super) fn apply_history(&mut self, redo: bool) -> bool {
        let step = if redo { self.history.pop_redo() } else { self.history.pop_undo() };
        let Some(step) = step else { return false };
        if redo {
            for edit in &step.edits {
                self.splice(edit.at, end_of(edit.at, &edit.removed), &edit.inserted);
            }
            (self.cursor, self.anchor) = step.after;
            self.history.redone(step);
        } else {
            for edit in step.edits.iter().rev() {
                self.splice(edit.at, end_of(edit.at, &edit.inserted), &edit.removed);
            }
            (self.cursor, self.anchor) = step.before;
            self.history.undone(step);
        }
        self.cursor = self.clamp(self.cursor);
        self.anchor = self.anchor.map(|a| self.clamp(a));
        self.goal = None;
        true
    }
}
