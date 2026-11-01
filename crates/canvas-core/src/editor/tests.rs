//! Editor tests: text model, keys, undo, lists, find.

use super::*;
use crate::input::{KeyCode, KeyEvent, Modifiers};

pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, Modifiers::NONE)
}

pub(super) fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), Modifiers::CONTROL)
}

pub(super) fn with(code: KeyCode, modifiers: Modifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

pub(super) fn press(editor: &mut Editor, codes: &[KeyCode]) {
    for code in codes {
        editor.key(&key(*code));
    }
}

pub(super) fn type_text(editor: &mut Editor, text: &str) {
    for c in text.chars() {
        if c == '\n' {
            editor.key(&key(KeyCode::Enter));
        } else {
            editor.key(&key(KeyCode::Char(c)));
        }
    }
}

#[test]
fn typing_and_line_endings_round_trip() {
    let mut editor = Editor::new("one\r\ntwo");
    assert_eq!(editor.text(), "one\r\ntwo");
    assert_eq!(editor.line_count(), 2);
    editor.key(&key(KeyCode::End));
    assert_eq!(editor.key(&key(KeyCode::Char('!'))), Outcome::Changed);
    editor.key(&key(KeyCode::Enter));
    editor.insert_text("a\nb");
    assert_eq!(editor.text(), "one!\r\na\r\nb\r\ntwo");
    assert!(editor.is_dirty());
    editor.mark_saved();
    assert!(!editor.is_dirty());
    let mut lf = Editor::new("x\ny\n");
    assert_eq!(lf.line_count(), 3);
    lf.insert_text("p\r\nq\rr");
    assert_eq!(lf.text(), "p\nq\nrx\ny\n", "pasted CRLF and CR become the file's line ending");
}

#[test]
fn empty_and_out_of_range_positions_do_not_panic() {
    let mut editor = Editor::new("");
    assert_eq!(editor.line_count(), 1);
    assert_eq!(editor.line(5), "");
    press(&mut editor, &[KeyCode::Left, KeyCode::Up, KeyCode::Backspace, KeyCode::Delete, KeyCode::Home, KeyCode::End, KeyCode::PageDown, KeyCode::Down, KeyCode::Right]);
    editor.set_cursor(Pos::new(99, 99));
    assert_eq!(editor.cursor(), Pos::new(0, 0));
    editor.goto_line(1000);
    assert_eq!(editor.cursor(), Pos::new(0, 0));
    assert_eq!(editor.selected_text(), None);
    editor.select_all();
    assert_eq!(editor.selection(), None, "an empty buffer has nothing to select");
    let mut multi = Editor::new("héllo wörld\n日本語");
    multi.set_cursor(Pos::new(1, 99));
    assert_eq!(multi.cursor(), Pos::new(1, 3));
    multi.key(&key(KeyCode::Backspace));
    assert_eq!(multi.line(1), "日本");
    multi.set_cursor(Pos::new(0, 2));
    multi.key(&key(KeyCode::Delete));
    assert_eq!(multi.line(0), "hélo wörld");
}

#[test]
fn arrows_cross_lines_and_selection_collapses() {
    let mut editor = Editor::new("ab\ncd");
    editor.set_cursor(Pos::new(0, 2));
    editor.key(&key(KeyCode::Right));
    assert_eq!(editor.cursor(), Pos::new(1, 0));
    editor.key(&key(KeyCode::Left));
    assert_eq!(editor.cursor(), Pos::new(0, 2));
    editor.key(&with(KeyCode::Left, Modifiers::SHIFT));
    editor.key(&with(KeyCode::Left, Modifiers::SHIFT));
    assert_eq!(editor.selected_text().as_deref(), Some("ab"));
    editor.key(&key(KeyCode::Right));
    assert_eq!((editor.cursor(), editor.selection()), (Pos::new(0, 2), None), "Right collapses to the end");
    editor.key(&with(KeyCode::Down, Modifiers::SHIFT));
    assert_eq!(editor.selected_text().as_deref(), Some("\ncd"));
    editor.key(&key(KeyCode::Left));
    assert_eq!(editor.cursor(), Pos::new(0, 2), "Left collapses to the start");
    editor.key(&key(KeyCode::Up));
    assert_eq!(editor.cursor(), Pos::new(0, 0), "Up on the first line goes to its start");
    editor.key(&key(KeyCode::Down));
    editor.key(&key(KeyCode::Down));
    assert_eq!(editor.cursor(), Pos::new(1, 2), "Down on the last line goes to its end");
}

#[test]
fn word_moves_and_word_deletes() {
    let mut editor = Editor::new("foo bar_baz, qux\nnext");
    let ctrl_key = |code| with(code, Modifiers::CONTROL);
    editor.key(&ctrl_key(KeyCode::Right));
    assert_eq!(editor.cursor().col, 3);
    editor.key(&ctrl_key(KeyCode::Right));
    assert_eq!(editor.cursor().col, 11, "underscores are word characters");
    editor.key(&ctrl_key(KeyCode::Right));
    assert_eq!(editor.cursor().col, 12, "punctuation is a word of its own");
    editor.key(&ctrl_key(KeyCode::Right));
    editor.key(&ctrl_key(KeyCode::Right));
    assert_eq!(editor.cursor(), Pos::new(1, 0), "at the line end it goes to the next line");
    editor.key(&ctrl_key(KeyCode::Left));
    assert_eq!(editor.cursor(), Pos::new(0, 16));
    editor.key(&ctrl_key(KeyCode::Left));
    assert_eq!(editor.cursor(), Pos::new(0, 13));
    editor.key(&with(KeyCode::Left, Modifiers::CONTROL | Modifiers::SHIFT));
    assert_eq!(editor.selected_text().as_deref(), Some(", "));
    editor.set_cursor(Pos::new(0, 11));
    editor.key(&ctrl_key(KeyCode::Backspace));
    assert_eq!(editor.line(0), "foo , qux");
    editor.set_cursor(Pos::new(0, 0));
    editor.key(&ctrl_key(KeyCode::Delete));
    assert_eq!(editor.line(0), " , qux");
    editor.set_cursor(Pos::new(1, 0));
    editor.key(&ctrl_key(KeyCode::Backspace));
    assert_eq!(editor.text(), " , quxnext", "at a line start it joins the lines");
}

#[test]
fn select_all_type_copy_cut_and_paste() {
    let mut editor = Editor::new("alpha\nbeta");
    assert_eq!(editor.key(&ctrl('a')), Outcome::Handled);
    assert_eq!(editor.key(&ctrl('c')), Outcome::Copy("alpha\nbeta".into()));
    editor.key(&key(KeyCode::Char('x')));
    assert_eq!(editor.text(), "x");
    editor.insert_text("1\n2\n3");
    assert_eq!(editor.text(), "x1\n2\n3");
    assert_eq!(editor.cursor(), Pos::new(2, 1));
    editor.key(&with(KeyCode::Home, Modifiers::SHIFT));
    editor.key(&with(KeyCode::Up, Modifiers::SHIFT));
    assert_eq!(editor.key(&ctrl('x')), Outcome::Cut("2\n3".into()));
    assert_eq!(editor.text(), "x1\n");
    assert_eq!(editor.key(&ctrl('c')), Outcome::Handled, "nothing selected: nothing copied");
    assert_eq!(editor.key(&ctrl('x')), Outcome::Handled);
    assert_eq!(editor.key(&ctrl('v')), Outcome::Unhandled, "the caller pastes");
    assert_eq!(editor.key(&ctrl('s')), Outcome::Save);
    assert_eq!(editor.key(&ctrl('f')), Outcome::Find);
    assert_eq!(editor.key(&key(KeyCode::Esc)), Outcome::Unhandled);
    assert_eq!(editor.key(&ctrl('n')), Outcome::Unhandled);
    assert_eq!(editor.key(&with(KeyCode::Tab, Modifiers::CONTROL)), Outcome::Unhandled);
    assert_eq!(editor.key(&with(KeyCode::Char('q'), Modifiers::ALT)), Outcome::Unhandled);
    assert_eq!(editor.key(&with(KeyCode::Char('@'), Modifiers::CONTROL | Modifiers::ALT)), Outcome::Changed, "AltGr text is typed");
    assert_eq!(editor.key(&KeyEvent::with_kind(KeyCode::Char('z'), Modifiers::NONE, crate::input::KeyEventKind::Release)), Outcome::Unhandled);
    assert_eq!(editor.text(), "x1\n@");
}

#[test]
fn read_only_refuses_edits_but_copies() {
    let mut editor = Editor::new("keep me");
    editor.set_read_only(true);
    assert!(editor.read_only());
    editor.key(&key(KeyCode::Char('x')));
    press(&mut editor, &[KeyCode::Enter, KeyCode::Backspace, KeyCode::Delete, KeyCode::Tab, KeyCode::BackTab]);
    assert_eq!(editor.insert_text("paste"), Outcome::Handled);
    editor.replace_before_cursor(2, "zz");
    editor.key(&ctrl('a'));
    assert_eq!(editor.key(&ctrl('x')), Outcome::Copy("keep me".into()), "cutting copies");
    assert_eq!(editor.key(&ctrl('c')), Outcome::Copy("keep me".into()));
    assert_eq!(editor.key(&ctrl('z')), Outcome::Handled);
    assert_eq!(editor.text(), "keep me");
    assert!(!editor.is_dirty());
    editor.key(&key(KeyCode::Right));
    assert_eq!(editor.cursor(), Pos::new(0, 7), "moving works");
}

#[test]
fn undo_groups_typing_and_deletes_and_restores_the_selection() {
    let mut editor = Editor::new("start");
    editor.key(&key(KeyCode::End));
    type_text(&mut editor, " one two");
    editor.key(&key(KeyCode::Enter));
    type_text(&mut editor, "three");
    assert_eq!(editor.text(), "start one two\nthree");
    assert!(editor.can_undo() && !editor.can_redo());
    assert_eq!(editor.key(&ctrl('z')), Outcome::Changed);
    assert_eq!(editor.text(), "start one two\n", "the typing after the newline is one step");
    editor.key(&ctrl('z'));
    assert_eq!(editor.text(), "start one two", "the newline is its own step");
    editor.key(&ctrl('z'));
    assert_eq!(editor.text(), "start", "consecutive typing is one step");
    assert!(!editor.is_dirty(), "back to the loaded text: clean");
    assert_eq!(editor.key(&ctrl('z')), Outcome::Handled, "nothing more to undo");
    assert_eq!(editor.key(&ctrl('y')), Outcome::Changed);
    assert_eq!(editor.text(), "start one two");
    assert!(editor.is_dirty());
    editor.key(&with(KeyCode::Char('Z'), Modifiers::CONTROL | Modifiers::SHIFT));
    assert_eq!(editor.text(), "start one two\n", "Ctrl+Shift+Z redoes");
    editor.key(&ctrl('y'));
    assert_eq!(editor.text(), "start one two\nthree");
    assert_eq!(editor.cursor(), Pos::new(1, 5));
    // Deletes group; moving the caret starts a new step.
    press(&mut editor, &[KeyCode::Backspace, KeyCode::Backspace]);
    editor.key(&key(KeyCode::Home));
    editor.key(&key(KeyCode::Delete));
    assert_eq!(editor.line(1), "hr");
    editor.key(&ctrl('z'));
    assert_eq!(editor.line(1), "thr");
    editor.key(&ctrl('z'));
    assert_eq!(editor.line(1), "three");
    assert_eq!(editor.cursor(), Pos::new(1, 5));
    // Typing over a selection; undo brings the selection back.
    editor.set_cursor(Pos::new(0, 6));
    editor.key(&with(KeyCode::End, Modifiers::SHIFT));
    type_text(&mut editor, "XY");
    assert_eq!(editor.line(0), "start XY");
    editor.key(&ctrl('z'));
    assert_eq!(editor.line(0), "start one two");
    assert_eq!(editor.selection(), Some((Pos::new(0, 6), Pos::new(0, 13))));
    editor.key(&ctrl('y'));
    assert_eq!((editor.line(0), editor.cursor(), editor.selection()), ("start XY", Pos::new(0, 8), None));
}

#[test]
fn saved_revision_and_reload_clear_history() {
    let mut editor = Editor::new("a");
    editor.key(&key(KeyCode::End));
    type_text(&mut editor, "bc");
    editor.mark_saved();
    type_text(&mut editor, "d");
    assert!(editor.is_dirty());
    editor.key(&ctrl('z'));
    assert_eq!(editor.text(), "abc", "saving ends the typing group");
    assert!(!editor.is_dirty(), "undo back to the saved text is clean");
    editor.key(&ctrl('z'));
    assert!(editor.is_dirty(), "undoing past the save is dirty again");
    editor.key(&ctrl('y'));
    assert!(!editor.is_dirty());
    editor.insert_text("paste");
    editor.insert_text("more");
    editor.key(&ctrl('z'));
    assert_eq!(editor.text(), "abcpaste", "each paste is its own step");
    editor.reload("abc\nnew line");
    assert!(!editor.is_dirty() && !editor.can_undo() && !editor.can_redo());
    assert_eq!(editor.cursor(), Pos::new(0, 3), "reload keeps the cursor where it can");
    editor.set_text("fresh");
    assert_eq!((editor.cursor(), editor.is_dirty(), editor.can_undo()), (Pos::new(0, 0), false, false));
}

#[test]
fn enter_continues_lists_tasks_and_quotes() {
    let cases = [
        ("- item", "- item\n- "),
        ("  * nested", "  * nested\n  * "),
        ("+ plus", "+ plus\n+ "),
        ("1. one", "1. one\n2. "),
        ("9) nine", "9) nine\n10) "),
        ("- [x] done", "- [x] done\n- [ ] "),
        ("- [ ] open", "- [ ] open\n- [ ] "),
        ("> quote", "> quote\n> "),
        ("> > deep", "> > deep\n> > "),
        ("> - in quote", "> - in quote\n> - "),
        ("plain", "plain\n"),
        ("---", "---\n"),
        ("- - -", "- - -\n"),
    ];
    for (line, expected) in cases {
        let mut editor = Editor::new(line);
        editor.key(&key(KeyCode::End));
        editor.key(&key(KeyCode::Enter));
        assert_eq!(editor.text(), expected, "{line:?}");
        assert_eq!(editor.cursor(), Pos::new(1, editor.line(1).chars().count()));
    }
    let ends = [("- ", ""), ("  - [ ] ", ""), ("3. ", ""), ("> ", ""), ("> > ", "> "), ("> - ", "> ")];
    for (line, expected) in ends {
        let mut editor = Editor::new(line);
        editor.key(&key(KeyCode::End));
        editor.key(&key(KeyCode::Enter));
        assert_eq!(editor.text(), expected, "Enter on the empty item {line:?} removes its marker");
    }
    let mut middle = Editor::new("- abcdef");
    middle.set_cursor(Pos::new(0, 5));
    middle.key(&key(KeyCode::Enter));
    assert_eq!(middle.text(), "- abc\n- def");
    let mut marker = Editor::new("- item");
    marker.set_cursor(Pos::new(0, 1));
    marker.key(&key(KeyCode::Enter));
    assert_eq!(marker.text(), "-\n item", "inside the marker Enter just breaks the line");
    let mut code = Editor::new("```\n- not a list");
    code.set_cursor(Pos::new(1, 99));
    code.key(&key(KeyCode::Enter));
    assert_eq!(code.text(), "```\n- not a list\n", "no continuation in code blocks");
}

#[test]
fn tab_indents_and_outdents_lines_and_selections() {
    let mut editor = Editor::new("one\ntwo\n\nthree");
    editor.set_cursor(Pos::new(0, 1));
    assert_eq!(editor.key(&key(KeyCode::Tab)), Outcome::Changed);
    assert_eq!((editor.line(0), editor.cursor()), ("  one", Pos::new(0, 3)));
    editor.key(&key(KeyCode::BackTab));
    assert_eq!((editor.line(0), editor.cursor()), ("one", Pos::new(0, 1)));
    assert_eq!(editor.key(&key(KeyCode::BackTab)), Outcome::Handled, "nothing to outdent");
    editor.select_all();
    editor.key(&key(KeyCode::Tab));
    assert_eq!(editor.text(), "  one\n  two\n\n  three", "empty lines stay empty");
    assert_eq!(editor.selected_text().as_deref(), Some("one\n  two\n\n  three"));
    editor.key(&key(KeyCode::Tab));
    editor.key(&with(KeyCode::Tab, Modifiers::SHIFT));
    assert_eq!(editor.text(), "  one\n  two\n\n  three");
    editor.key(&ctrl('z'));
    editor.key(&ctrl('z'));
    assert_eq!(editor.text(), "  one\n  two\n\n  three", "each indent is one step");
    editor.key(&ctrl('z'));
    assert_eq!(editor.text(), "one\ntwo\n\nthree");
    // A selection ending at a line start leaves that line alone.
    editor.set_cursor(Pos::new(0, 0));
    editor.key(&with(KeyCode::Down, Modifiers::SHIFT));
    editor.key(&key(KeyCode::Tab));
    assert_eq!(editor.text(), "  one\ntwo\n\nthree");
    let mut tabbed = Editor::new("\tx");
    tabbed.key(&key(KeyCode::BackTab));
    assert_eq!(tabbed.text(), "x");
}

#[test]
fn find_next_previous_and_wrap_with_non_ascii_text() {
    // 'İ' lowercases to two characters: the matches after it must still line up.
    let mut editor = Editor::new("İstanbul straße\nSTRASSE and Straße\nnone here\nstraße");
    assert_eq!(editor.find_status(), None);
    assert!(!editor.find_next(true), "no query: nothing to find");
    editor.set_find(Some("STRAßE"));
    assert_eq!(editor.find_status(), Some((0, 3)));
    assert!(editor.find_next(true));
    assert_eq!(editor.selection(), Some((Pos::new(0, 9), Pos::new(0, 15))));
    assert_eq!(editor.selected_text().as_deref(), Some("straße"));
    assert_eq!(editor.find_status(), Some((1, 3)));
    editor.find_next(true);
    assert_eq!(editor.selection(), Some((Pos::new(1, 12), Pos::new(1, 18))));
    editor.find_next(true);
    assert_eq!(editor.selection(), Some((Pos::new(3, 0), Pos::new(3, 6))));
    editor.find_next(true);
    assert_eq!(editor.find_status(), Some((1, 3)), "wraps to the first match");
    editor.find_next(false);
    assert_eq!(editor.find_status(), Some((3, 3)), "backwards wraps to the last");
    editor.find_next(false);
    assert_eq!(editor.find_status(), Some((2, 3)));
    editor.set_find(Some("i\u{307}stan"));
    assert_eq!(editor.find_status(), Some((0, 1)));
    editor.find_next(true);
    assert_eq!(editor.selected_text().as_deref(), Some("İstan"), "a match inside an expanded lowercase maps back to whole characters");
    editor.set_find(Some("zzz"));
    assert!(!editor.find_next(true));
    assert_eq!(editor.find_status(), Some((0, 0)));
    editor.set_find(None);
    assert_eq!(editor.find_status(), None);
    editor.set_find(Some(""));
    assert_eq!(editor.find_status(), None, "an empty query clears find");
}

#[test]
fn find_matches_follow_edits() {
    let mut editor = Editor::new("cat\ndog\ncat cat");
    editor.set_find(Some("cat"));
    assert_eq!(editor.find_status(), Some((0, 3)));
    editor.set_cursor(Pos::new(1, 0));
    editor.insert_text("cat\nnew\n");
    assert_eq!(editor.find_status(), Some((0, 4)));
    assert_eq!(editor.text(), "cat\ncat\nnew\ndog\ncat cat");
    editor.find_next(true);
    assert_eq!(editor.selection(), Some((Pos::new(4, 0), Pos::new(4, 3))), "matches after the edit moved down");
    editor.key(&key(KeyCode::Backspace));
    assert_eq!(editor.find_status(), Some((0, 3)));
    editor.key(&ctrl('z'));
    assert_eq!(editor.find_status(), Some((3, 4)), "undo brings the match and its selection back");
    editor.select_all();
    editor.key(&key(KeyCode::Delete));
    assert_eq!(editor.find_status(), Some((0, 0)));
    editor.set_text("Cat");
    assert_eq!(editor.find_status(), Some((0, 1)), "new text is searched again");
}

#[test]
fn completion_prefix_and_replacement() {
    let mut editor = Editor::new("see [[Ove");
    editor.set_cursor(Pos::new(0, 9));
    assert_eq!(editor.completion_prefix().as_deref(), Some("Ove"));
    editor.replace_before_cursor(3, "Overview]]");
    assert_eq!(editor.text(), "see [[Overview]]");
    assert_eq!(editor.cursor(), Pos::new(0, 16));
    assert_eq!(editor.completion_prefix(), None);
    editor.key(&ctrl('z'));
    assert_eq!(editor.text(), "see [[Ove", "the completion is one undo step");
    let mut start = Editor::new("[[");
    start.set_cursor(Pos::new(0, 2));
    assert_eq!(start.completion_prefix().as_deref(), Some(""));
    start.replace_before_cursor(99, "x");
    assert_eq!(start.text(), "x", "too many characters is clamped to the line start");
    let mut wide = Editor::new("日本 [[ノー");
    wide.set_cursor(Pos::new(0, 7));
    assert_eq!(wide.completion_prefix().as_deref(), Some("ノー"));
    wide.set_cursor(Pos::new(0, 2));
    assert_eq!(wide.completion_prefix(), None);
}

#[test]
fn double_click_selects_a_word_and_home_end_toggle() {
    let mut editor = Editor::new("    indented words here");
    let area = Rect::new(0, 0, 40, 5);
    editor.double_click(area, 13, 0);
    assert_eq!(editor.selected_text().as_deref(), Some("words"));
    editor.double_click(area, 1, 0);
    assert_eq!(editor.selected_text().as_deref(), Some("    "), "spaces select as a run");
    editor.set_cursor(Pos::new(0, 10));
    editor.key(&key(KeyCode::Home));
    assert_eq!(editor.cursor().col, 0);
    editor.key(&key(KeyCode::Home));
    assert_eq!(editor.cursor().col, 4, "Home again: the first non-space character");
    editor.key(&key(KeyCode::Home));
    assert_eq!(editor.cursor().col, 0, "and back");
    editor.key(&key(KeyCode::End));
    assert_eq!(editor.cursor().col, 23);
    editor.key(&with(KeyCode::Home, Modifiers::SHIFT));
    assert_eq!(editor.selected_text().as_deref(), Some("    indented words here"));
    editor.key(&with(KeyCode::End, Modifiers::CONTROL));
    editor.key(&with(KeyCode::Home, Modifiers::CONTROL | Modifiers::SHIFT));
    assert_eq!(editor.selection(), Some((Pos::new(0, 0), Pos::new(0, 23))));
}

#[test]
fn undo_of_pastes_cuts_and_ime_text() {
    let mut editor = Editor::new("one\r\ntwo");
    editor.set_cursor(Pos::new(0, 3));
    editor.insert_text("A\nB\nC");
    assert_eq!(editor.text(), "oneA\r\nB\r\nC\r\ntwo");
    editor.insert_text("é");
    editor.insert_text("!");
    editor.key(&key(KeyCode::Char('?')));
    editor.key(&ctrl('z'));
    assert_eq!(editor.text(), "oneA\r\nB\r\nC\r\ntwo", "single characters from IME commits group with typing");
    editor.key(&ctrl('z'));
    assert_eq!((editor.text(), editor.cursor()), ("one\r\ntwo".to_owned(), Pos::new(0, 3)));
    editor.key(&ctrl('a'));
    assert_eq!(editor.key(&ctrl('x')), Outcome::Cut("one\ntwo".into()), "copied text uses \\n");
    assert_eq!(editor.text(), "");
    editor.key(&ctrl('z'));
    assert_eq!((editor.text(), editor.selection()), ("one\r\ntwo".to_owned(), Some((Pos::new(0, 0), Pos::new(1, 3)))));
    assert!(!editor.is_dirty());
    assert_eq!(editor.insert_text(""), Outcome::Handled);
    assert_eq!(editor.insert_text("\u{1b}"), Outcome::Handled, "control characters are dropped");
    assert!(!editor.can_redo() || editor.redo());
}

#[test]
fn dropped_undo_steps_keep_the_dirty_state_honest() {
    let mut editor = Editor::new("");
    for _ in 0..2010 {
        editor.insert_text("ab");
    }
    let mut undone = 0;
    while editor.undo() {
        undone += 1;
    }
    assert!(undone < 2010, "old steps are dropped");
    assert!(!editor.text().is_empty());
    assert!(editor.is_dirty(), "the loaded text cannot be reached: still dirty");
}

#[test]
fn amend_joins_the_last_step_and_apply_is_a_step_of_its_own() {
    let mut editor = Editor::new("# Log\nStarted.");
    editor.set_cursor(Pos::new(1, 8));
    type_text(&mut editor, " One");
    assert!(editor.amend("---\nauthor: mixed\n---\n# Log\nStarted. One"));
    assert_eq!(editor.cursor(), Pos::new(4, 12), "the caret stays on its text");
    let same = editor.text();
    assert!(!editor.amend(&same), "nothing to change");
    type_text(&mut editor, "!");
    assert!(editor.undo());
    assert_eq!(editor.text(), "# Log\nStarted.", "the typing and the app's lines go together");
    assert!(!editor.can_undo());
    assert!(editor.redo());
    assert_eq!((editor.text().as_str(), editor.cursor()), ("---\nauthor: mixed\n---\n# Log\nStarted. One!", Pos::new(4, 13)));
    assert!(editor.is_dirty());
    // `apply`: a step of its own.
    assert!(editor.apply("---\nauthor: mixed\nreviewed: true\n---\n# Log\nStarted. One!"));
    assert_eq!(editor.cursor(), Pos::new(5, 13));
    assert!(editor.undo());
    assert_eq!(editor.text(), "---\nauthor: mixed\n---\n# Log\nStarted. One!");
    // Lines added or removed at the end, removed in the middle, all replaced; CRLF kept.
    let cases = [("a\nb", "a\nb\nc"), ("a\nb\nc", "a"), ("a\nb\nc", "a\nc"), ("a\nb", "x"), ("", "x\n"), ("x\n", "")];
    for (from, to) in cases {
        let mut editor = Editor::new(from);
        editor.set_cursor(Pos::new(9, 9));
        assert!(editor.apply(to), "{from:?} → {to:?}");
        assert_eq!(editor.text(), to);
        assert_eq!(editor.clamp(editor.cursor()), editor.cursor());
        assert!(editor.undo());
        assert_eq!(editor.text(), from);
    }
    let mut crlf = Editor::new("a\r\nb");
    assert!(crlf.apply("a\r\nc"));
    assert_eq!(crlf.text(), "a\r\nc");
    let mut read_only = Editor::new("a");
    read_only.set_read_only(true);
    assert!(!read_only.amend("b") && !read_only.apply("b"));
}
