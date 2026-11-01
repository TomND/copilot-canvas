//! Editor tests: wrapping, scrolling, mouse, Live mode, drawing, large text.

use super::tests::{key, press, with};
use super::*;
use crate::input::{KeyCode, Modifiers};

use ratatui::style::{Color, Modifier};

use crate::paint::{ACCENT, ATTENTION, ATTENTION_BG, MUTED, PURPLE};

fn draw(editor: &mut Editor, width: u16, height: u16) -> (Buffer, Option<(u16, u16)>) {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    let caret = editor.draw(&mut buf, area, true);
    (buf, caret)
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_owned()).collect::<String>().trim_end().to_owned()
}

fn rows_of(text: &str, width: usize) -> Vec<usize> {
    let chars: Vec<char> = text.chars().collect();
    let widths: Vec<u8> = chars.iter().map(|c| text::char_width(*c) as u8).collect();
    layout::wrap(&chars, &widths, width)
}

#[test]
fn soft_wrap_breaks_after_spaces_and_measures_wide_characters() {
    assert_eq!(rows_of("hello world foo", 6), [0, 6, 12]);
    assert_eq!(rows_of("abcdefgh", 3), [0, 3, 6], "long words break hard");
    assert_eq!(rows_of("日本語テキスト", 5), [0, 2, 4, 6], "wide characters take two cells");
    assert_eq!(rows_of("ab 日本語", 4), [0, 3, 5]);
    assert_eq!(rows_of("a     b", 3), [0, 6], "spaces hang past the edge");
    assert_eq!(rows_of("", 3), [0]);
    assert_eq!(rows_of("中", 1), [0], "a wide character in one cell does not loop");
    assert_eq!(rows_of("e\u{301}e\u{301}e\u{301}", 2), [0, 4], "combining marks take no cell");
    let mut editor = Editor::new("hello world foo\n日本語テキスト");
    let (buf, _) = draw(&mut editor, 6, 8);
    assert_eq!((0..6).map(|y| row_text(&buf, y)).collect::<Vec<_>>(), ["hello", "world", "foo", "日本語", "テキス", "ト"]);
    let layout = editor.layout(1, false);
    assert_eq!((layout.row_of(4), layout.x_of(4)), (1, 2), "キ is the second cell pair of row 1");
    assert_eq!(layout.at_x(1, 3), 4, "the second cell of a wide character maps to it");
}

#[test]
fn up_and_down_move_by_visual_rows_keeping_the_column() {
    let mut editor = Editor::new("0123456789\nab\n0123456789");
    draw(&mut editor, 4, 10);
    editor.set_cursor(Pos::new(0, 6));
    editor.key(&key(KeyCode::Down));
    assert_eq!(editor.cursor(), Pos::new(0, 10), "the short last row: its end");
    editor.key(&key(KeyCode::Down));
    assert_eq!(editor.cursor(), Pos::new(1, 2));
    editor.key(&key(KeyCode::Down));
    assert_eq!(editor.cursor(), Pos::new(2, 2), "the preferred column comes back");
    editor.key(&key(KeyCode::Up));
    editor.key(&key(KeyCode::Up));
    editor.key(&key(KeyCode::Up));
    assert_eq!(editor.cursor(), Pos::new(0, 6));
    editor.key(&with(KeyCode::Up, Modifiers::SHIFT));
    assert_eq!(editor.selected_text().as_deref(), Some("2345"), "Shift+Up selects back to the same column a row up");
    let (_, caret) = draw(&mut editor, 4, 10);
    assert_eq!(caret, Some((2, 0)));
}

#[test]
fn home_and_end_use_the_visual_row_first() {
    let mut editor = Editor::new("  hello world foo");
    draw(&mut editor, 8, 5);
    assert_eq!(editor.layout(0, false).rows, [0, 8, 14]);
    editor.set_cursor(Pos::new(0, 10));
    editor.key(&key(KeyCode::Home));
    assert_eq!(editor.cursor().col, 8, "start of the visual row");
    editor.key(&key(KeyCode::Home));
    assert_eq!(editor.cursor().col, 2, "then the first non-space character");
    editor.key(&key(KeyCode::Home));
    assert_eq!(editor.cursor().col, 0, "then the line start");
    editor.set_cursor(Pos::new(0, 9));
    editor.key(&key(KeyCode::End));
    assert_eq!(editor.cursor().col, 13, "end of the visual row: before its break");
    editor.key(&key(KeyCode::End));
    assert_eq!(editor.cursor().col, 17, "then the line end");
    let (_, caret) = draw(&mut editor, 8, 5);
    assert_eq!(caret, Some((3, 2)));
}

#[test]
fn page_keys_and_scrolling_keep_the_caret_visible() {
    let text: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
    let mut editor = Editor::new(&text.join("\n"));
    draw(&mut editor, 20, 10);
    editor.key(&key(KeyCode::PageDown));
    assert_eq!(editor.cursor(), Pos::new(10, 0));
    let (_, caret) = draw(&mut editor, 20, 10);
    assert_eq!((editor.first_visible_line(), caret), (10, Some((0, 0))));
    editor.key(&key(KeyCode::PageDown));
    editor.key(&key(KeyCode::PageUp));
    assert_eq!(editor.cursor(), Pos::new(10, 0));
    editor.scroll(5);
    assert_eq!((editor.first_visible_line(), editor.cursor()), (15, Pos::new(15, 0)), "the caret moves into view");
    editor.scroll(-3);
    assert_eq!((editor.first_visible_line(), editor.cursor()), (12, Pos::new(15, 0)), "a visible caret stays");
    editor.scroll(1000);
    assert_eq!(editor.first_visible_line(), 90, "the last line stays at the bottom");
    let (_, caret) = draw(&mut editor, 20, 10);
    assert_eq!((editor.cursor(), caret), (Pos::new(90, 0), Some((0, 0))));
    editor.key(&with(KeyCode::End, Modifiers::CONTROL));
    let (buf, caret) = draw(&mut editor, 20, 10);
    assert_eq!((editor.first_visible_line(), caret), (90, Some((7, 9))));
    assert_eq!(row_text(&buf, 9), "line 99");
    editor.key(&with(KeyCode::Home, Modifiers::CONTROL));
    let (buf, caret) = draw(&mut editor, 20, 10);
    assert_eq!((editor.first_visible_line(), caret, row_text(&buf, 0)), (0, Some((0, 0)), "line 0".to_owned()));
    // With a selection, scrolling the caret along grows it.
    editor.key(&with(KeyCode::Down, Modifiers::SHIFT));
    editor.scroll(20);
    assert_eq!(editor.selection(), Some((Pos::new(0, 0), Pos::new(20, 0))));
    editor.goto_line(50);
    let (buf, _) = draw(&mut editor, 20, 10);
    assert_eq!((editor.first_visible_line(), row_text(&buf, 0)), (50, "line 50".to_owned()));
}

#[test]
fn draws_source_styles_selection_caret_and_gutter() {
    let mut editor = Editor::new("# Title\nsome **bold** text\n`code` [[Note]]");
    editor.set_cursor(Pos::new(1, 3));
    let (buf, caret) = draw(&mut editor, 24, 4);
    assert_eq!(caret, Some((3, 1)));
    assert_eq!((row_text(&buf, 0), row_text(&buf, 1), row_text(&buf, 2)), ("# Title".into(), "some **bold** text".into(), "`code` [[Note]]".into()));
    assert_eq!(buf[(0, 0)].fg, MUTED, "heading marks are dimmed");
    assert_eq!((buf[(2, 0)].fg, buf[(2, 0)].modifier.contains(Modifier::BOLD)), (ACCENT, true));
    assert_eq!(buf[(5, 1)].fg, MUTED, "emphasis markers are dimmed");
    assert!(buf[(7, 1)].modifier.contains(Modifier::BOLD));
    assert!(!buf[(0, 1)].modifier.contains(Modifier::BOLD));
    assert_eq!(buf[(1, 2)].fg, PURPLE, "inline code");
    assert!(buf[(9, 2)].modifier.contains(Modifier::UNDERLINED), "wiki links are underlined");
    assert_eq!(editor.draw(&mut Buffer::empty(Rect::new(0, 0, 24, 4)), Rect::new(0, 0, 24, 4), false), None, "no caret when not focused");
    // Selection is reversed; a selected line break shows as one cell.
    editor.set_cursor(Pos::new(0, 2));
    editor.key(&with(KeyCode::Down, Modifiers::SHIFT));
    let (buf, _) = draw(&mut editor, 24, 4);
    assert!(buf[(2, 0)].modifier.contains(Modifier::REVERSED) && buf[(7, 0)].modifier.contains(Modifier::REVERSED));
    assert!(!buf[(1, 0)].modifier.contains(Modifier::REVERSED) && !buf[(2, 1)].modifier.contains(Modifier::REVERSED));
    // Line numbers.
    editor.set_line_numbers(true);
    editor.set_cursor(Pos::new(2, 0));
    let (buf, caret) = draw(&mut editor, 24, 4);
    assert_eq!((row_text(&buf, 0), caret), ("  1 # Title".to_owned(), Some((4, 2))));
    editor.click(Rect::new(0, 0, 24, 4), 6, 1, false);
    assert_eq!(editor.cursor(), Pos::new(1, 2), "clicks skip the gutter");
    editor.click(Rect::new(0, 0, 24, 4), 1, 0, false);
    assert_eq!(editor.cursor(), Pos::new(0, 0), "a click in the gutter is the line start");
}

#[test]
fn find_matches_are_highlighted_the_current_one_stronger() {
    let mut editor = Editor::new("ab ÄB ab\nx");
    editor.set_find(Some("äb"));
    let (buf, _) = draw(&mut editor, 20, 3);
    assert_eq!((buf[(3, 0)].bg, buf[(4, 0)].bg, buf[(0, 0)].bg), (ATTENTION_BG, ATTENTION_BG, Color::Reset));
    editor.set_find(Some("ab"));
    editor.find_next(true);
    let (buf, _) = draw(&mut editor, 20, 3);
    assert_eq!((buf[(0, 0)].bg, buf[(6, 0)].bg), (ATTENTION, ATTENTION_BG));
    assert!(!buf[(0, 0)].modifier.contains(Modifier::REVERSED), "the current match is not also reversed");
}

#[test]
fn placeholder_small_and_odd_areas_do_not_panic() {
    let mut editor = Editor::new("");
    editor.set_placeholder("Write here");
    let (buf, caret) = draw(&mut editor, 6, 2);
    assert_eq!((row_text(&buf, 0), caret), ("Write…".to_owned(), Some((0, 0))));
    assert_eq!(buf[(0, 0)].fg, MUTED);
    editor.insert_text("x");
    let (buf, _) = draw(&mut editor, 6, 2);
    assert_eq!(row_text(&buf, 0), "x");
    let mut big = Editor::new("中文 text\twith tab\n\u{7}bell");
    for (w, h) in [(0, 0), (0, 5), (5, 0), (1, 1), (2, 1), (3, 2)] {
        let (_, caret) = draw(&mut big, w, h);
        assert!(w * h > 0 || caret.is_none());
    }
    big.set_line_numbers(true);
    draw(&mut big, 3, 2);
    // The area is bigger than the buffer, and the mouse is far outside it.
    let mut small = Buffer::empty(Rect::new(0, 0, 4, 2));
    big.draw(&mut small, Rect::new(2, 1, 30, 10), true);
    big.click(Rect::new(2, 1, 30, 10), 500, 500, false);
    big.drag(Rect::new(2, 1, 30, 10), 0, 0);
    big.double_click(Rect::new(0, 0, 0, 0), 9, 9);
    big.scroll(-1000);
    big.scroll(1000);
    big.set_line_numbers(false);
    let (buf, _) = draw(&mut big, 30, 3);
    assert_eq!(row_text(&buf, 0), "中文 text    with tab", "tabs draw as spaces");
    assert_eq!(buf[(0, 1)].symbol(), "·", "control characters show as a dot");
}

#[test]
fn live_mode_hides_syntax_with_offset_maps() {
    let cases: &[(&str, &str)] = &[
        ("## Heading", "Heading"),
        ("some **bold** and *em* and `code`", "some bold and em and code"),
        ("[text](http://x.y/(a)) after", "text after"),
        ("[[Note|alias]] and [[Other]]", "alias and Other"),
        ("![[embed.png]] ![alt](a.png)", "embed.png alt"),
        ("- [ ] task", "☐ task"),
        ("- [x] done", "☑ done"),
        ("  - item", "  • item"),
        ("> quote **b**", "▎ quote b"),
        ("> > deep", "▎ ▎ deep"),
        ("1. one", "1. one"),
        ("***", "───"),
        ("\\*not em\\*", "*not em*"),
        ("~~gone~~ ***both*** __strong__ _em_", "gone both strong em"),
        ("snake_case_name 2 * 3 * 4 **unclosed", "snake_case_name 2 * 3 * 4 **unclosed"),
        ("``a ` b`` x", "a ` b x"),
        ("visit https://example.com.", "visit https://example.com."),
        ("#tag not heading", "#tag not heading"),
    ];
    for (raw, shown) in cases {
        let display = markdown::live(raw, markdown::LineKind::Text);
        assert_eq!(display.chars.iter().collect::<String>(), *shown, "{raw:?}");
        assert_eq!(display.raw.len(), display.chars.len());
        assert!(display.raw.windows(2).all(|w| w[0] <= w[1]), "the map is ordered: {raw:?}");
        assert!(display.raw.iter().all(|r| *r < raw.chars().count()));
    }
    let raw_of = |raw: &str, shown_index: usize| markdown::live(raw, markdown::LineKind::Text).raw[shown_index];
    assert_eq!(raw_of("## Heading", 0), 3);
    assert_eq!(raw_of("some **bold**", 5), 7);
    assert_eq!(raw_of("[[Note|alias]]", 0), 7);
    assert_eq!(raw_of("[[Note]]", 3), 5);
    assert_eq!((raw_of("- [ ] task", 0), raw_of("- [ ] task", 1), raw_of("- [ ] task", 2)), (0, 5, 6));
    assert_eq!((raw_of("- item", 0), raw_of("- item", 2)), (0, 2));
    let url = markdown::live("visit https://example.com.", markdown::LineKind::Text);
    assert!(url.styles[8].add_modifier.contains(Modifier::UNDERLINED) && !url.styles[25].add_modifier.contains(Modifier::UNDERLINED));
    let code = markdown::live("# not a heading", markdown::LineKind::Code);
    assert_eq!(code.chars.iter().collect::<String>(), "# not a heading", "code lines show as they are");
}

#[test]
fn block_styles_and_live_find_highlights() {
    let mut editor = Editor::new("---\na: b\n---\n> quote\n- item\n```\ncode\n```\n***\ntext");
    editor.set_cursor(Pos::new(9, 0));
    let (buf, _) = draw(&mut editor, 20, 12);
    assert_eq!(buf[(0, 1)].fg, MUTED, "front matter is dimmed");
    assert_eq!((buf[(0, 3)].fg, buf[(2, 3)].fg), (MUTED, crate::paint::GRAY), "quote marker and text");
    assert_eq!(buf[(0, 4)].fg, ACCENT, "list markers are coloured");
    assert_eq!(buf[(0, 5)].fg, MUTED, "fences are dimmed");
    assert_eq!((buf[(0, 6)].fg, buf[(0, 6)].bg), (crate::paint::GRAY, crate::paint::PANEL_HI), "code has a background");
    assert_eq!(buf[(0, 8)].fg, crate::paint::FAINT, "rules are dimmed");
    editor.set_mode(Mode::Live);
    editor.set_find(Some("item"));
    let (buf, _) = draw(&mut editor, 20, 12);
    assert_eq!((row_text(&buf, 3), row_text(&buf, 4), row_text(&buf, 8)), ("▎ quote".to_owned(), "• item".to_owned(), "───".to_owned()));
    assert_eq!((buf[(1, 4)].bg, buf[(2, 4)].bg, buf[(5, 4)].bg), (Color::Reset, ATTENTION_BG, ATTENTION_BG), "the match shows where its raw text is");
}

#[test]
fn live_mode_shows_the_cursor_line_raw_and_maps_clicks() {
    let text = "---\ntitle: x\n---\n# Head\n**bold** and [link](url) end\n```\n# not heading\n```\n- [ ] task";
    let mut editor = Editor::new(text);
    editor.set_mode(Mode::Live);
    assert_eq!(editor.mode(), Mode::Live);
    let area = Rect::new(0, 0, 40, 10);
    editor.set_cursor(Pos::new(8, 0));
    let (buf, caret) = draw(&mut editor, 40, 10);
    let rows: Vec<String> = (0..9).map(|y| row_text(&buf, y)).collect();
    assert_eq!(rows, ["---", "title: x", "---", "Head", "bold and link end", "```", "# not heading", "```", "- [ ] task"]);
    assert_eq!(caret, Some((0, 8)));
    assert_eq!(buf[(0, 1)].fg, MUTED, "front matter is dimmed");
    assert!(buf[(0, 3)].modifier.contains(Modifier::BOLD), "the heading keeps its style without its marks");
    // A click on "link" puts the caret on the raw "link"; the line then shows raw.
    editor.click(area, 9, 4, false);
    assert_eq!(editor.cursor(), Pos::new(4, 14));
    let (buf, caret) = draw(&mut editor, 40, 10);
    assert_eq!((row_text(&buf, 4), caret), ("**bold** and [link](url) end".to_owned(), Some((14, 4))));
    assert_eq!(row_text(&buf, 8), "☐ task", "the old cursor line is formatted again");
    editor.click(area, 30, 3, false);
    assert_eq!(editor.cursor(), Pos::new(3, 6), "past the end of a formatted line: its end");
    editor.click(area, 0, 8, false);
    assert_eq!(editor.cursor(), Pos::new(8, 0));
    editor.set_cursor(Pos::new(3, 0));
    draw(&mut editor, 40, 10);
    editor.click(area, 2, 8, false);
    assert_eq!(editor.cursor(), Pos::new(8, 6), "a click on a task's text skips the hidden box syntax");
    // Selected lines show raw; Up/Down onto a formatted line show it raw.
    editor.set_cursor(Pos::new(3, 0));
    editor.key(&with(KeyCode::Down, Modifiers::SHIFT));
    assert!(!editor.is_formatted(3) && !editor.is_formatted(4) && editor.is_formatted(8));
    let (buf, _) = draw(&mut editor, 40, 10);
    assert_eq!((row_text(&buf, 3), row_text(&buf, 4)), ("# Head".to_owned(), "**bold** and [link](url) end".to_owned()));
    editor.key(&key(KeyCode::Down));
    editor.key(&key(KeyCode::Down));
    assert!(editor.is_formatted(4));
    // Formatted lines wrap by what they show.
    editor.set_cursor(Pos::new(0, 0));
    draw(&mut editor, 10, 10);
    assert_eq!(editor.shown_layout(4).rows, [0, 9], "\"bold and \" / \"link end\"");
    assert_eq!(editor.layout(4, false).rows, [0, 9, 13, 23], "raw it takes more rows");
    editor.set_mode(Mode::Source);
    assert!(!editor.is_formatted(4));
}

#[test]
fn drag_selects_and_scrolls_past_the_edges() {
    let text: Vec<String> = (0..30).map(|i| format!("row {i}")).collect();
    let mut editor = Editor::new(&text.join("\n"));
    let area = Rect::new(2, 1, 20, 5);
    let mut buf = Buffer::empty(Rect::new(0, 0, 30, 10));
    editor.draw(&mut buf, area, true);
    editor.click(area, 3, 2, false);
    assert_eq!(editor.cursor(), Pos::new(1, 1));
    editor.drag(area, 5, 3);
    assert_eq!(editor.selected_text().as_deref(), Some("ow 1\nrow"));
    editor.drag(area, 5, 40);
    assert_eq!(editor.cursor(), Pos::new(5, 3), "below the area: the row after the view");
    editor.draw(&mut buf, area, true);
    assert_eq!(editor.first_visible_line(), 1);
    editor.click(area, 4, 1, true);
    assert_eq!(editor.selection(), Some((Pos::new(1, 1), Pos::new(1, 2))), "Shift-click moves the selection end");
}

#[test]
fn large_text_stays_responsive() {
    let lines: Vec<String> = (0..50_000).map(|i| format!("line {i} with **some** words and a [[link]] here")).collect();
    let text = lines.join("\n");
    let started = std::time::Instant::now();
    let mut editor = Editor::new(&text);
    editor.set_mode(Mode::Live);
    let area = Rect::new(0, 0, 60, 30);
    let mut buf = Buffer::empty(area);
    editor.draw(&mut buf, area, true);
    editor.key(&with(KeyCode::End, Modifiers::CONTROL));
    editor.draw(&mut buf, area, true);
    assert_eq!(editor.first_visible_line(), 50_000 - 30);
    editor.goto_line(25_000);
    for i in 0..300 {
        editor.key(&key(if i % 50 == 49 { KeyCode::Enter } else { KeyCode::Char('x') }));
        editor.draw(&mut buf, area, true);
    }
    press(&mut editor, &[KeyCode::PageDown, KeyCode::PageUp, KeyCode::Up, KeyCode::Down]);
    editor.scroll(400);
    editor.draw(&mut buf, area, true);
    editor.set_find(Some("line 49999 "));
    assert!(editor.find_next(true));
    editor.draw(&mut buf, area, true);
    assert_eq!(editor.cursor().line, 50_005);
    for _ in 0..300 {
        editor.key(&ctrl_z());
    }
    assert_eq!(editor.text(), text);
    assert!(!editor.is_dirty());
    let elapsed = started.elapsed();
    assert!(elapsed.as_secs() < 20, "took {elapsed:?}");
}

fn ctrl_z() -> crate::input::KeyEvent {
    with(KeyCode::Char('z'), Modifiers::CONTROL)
}

#[test]
fn one_huge_line_of_wide_characters_stays_usable() {
    let line: String = "日本語 **wörds** [[link]] `code` ".repeat(12_000);
    let mut editor = Editor::new(&format!("# Head\n{line}\nend"));
    let started = std::time::Instant::now();
    for mode in [Mode::Source, Mode::Live] {
        editor.set_mode(mode);
        let (_, caret) = draw(&mut editor, 50, 12);
        assert!(caret.is_some());
        editor.set_cursor(Pos::new(1, 200_000));
        press(&mut editor, &[KeyCode::End, KeyCode::Home, KeyCode::Down, KeyCode::Up, KeyCode::PageDown, KeyCode::PageUp]);
        draw(&mut editor, 50, 12);
        editor.click(Rect::new(0, 0, 50, 12), 7, 5, false);
        editor.key(&key(KeyCode::Char('é')));
        editor.set_find(Some("WÖRDS"));
        assert!(editor.find_next(true));
        assert!(editor.find_status().is_some_and(|(_, n)| n >= 11_998), "every match but where é was typed");
        draw(&mut editor, 50, 12);
    }
    assert!(started.elapsed().as_secs() < 20, "took {:?}", started.elapsed());
}

/// A small deterministic random source for the stress test.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[test]
fn random_editing_with_wide_and_combining_characters_never_breaks_the_buffer() {
    const CHARS: &[char] = &['a', 'b', ' ', 'é', '日', '\u{301}', '#', '*', '_', '`', '[', ']', '(', ')', '|', '-', '>', '\t', '~', 'İ', 'Σ', 'ς', '1', '.', 'h'];
    let keys = [KeyCode::Enter, KeyCode::Backspace, KeyCode::Delete, KeyCode::Left, KeyCode::Right, KeyCode::Up, KeyCode::Down, KeyCode::Home, KeyCode::End, KeyCode::PageUp, KeyCode::PageDown, KeyCode::Tab, KeyCode::BackTab];
    let mods = [Modifiers::NONE, Modifiers::SHIFT, Modifiers::CONTROL, Modifiers::CONTROL | Modifiers::SHIFT];
    for seed in 1..=8u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let start = "---\ntitle: x\n---\n# Head 日本\n- [ ] task **bold** [[link|alias]]\n```\ncode\n```\n> quote *em*\n";
        let mut editor = Editor::new(start);
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 12));
        let mut area = Rect::new(1, 1, 20, 8);
        for _ in 0..1500 {
            match rng.below(14) {
                0..=3 => {
                    editor.key(&key(KeyCode::Char(CHARS[rng.below(CHARS.len())])));
                }
                4..=6 => {
                    editor.key(&with(keys[rng.below(keys.len())], mods[rng.below(mods.len())]));
                }
                7 => {
                    let text: String = (0..rng.below(6)).map(|_| if rng.below(5) == 0 { '\n' } else { CHARS[rng.below(CHARS.len())] }).collect();
                    editor.insert_text(&text);
                }
                8 => {
                    let c = ['z', 'y', 'a', 'x', 'c'][rng.below(5)];
                    editor.key(&with(KeyCode::Char(c), if rng.below(3) == 0 { Modifiers::CONTROL | Modifiers::SHIFT } else { Modifiers::CONTROL }));
                }
                9 => editor.set_mode(if rng.below(2) == 0 { Mode::Live } else { Mode::Source }),
                10 => {
                    area = Rect::new(rng.below(3) as u16, rng.below(3) as u16, 1 + rng.below(30) as u16, 1 + rng.below(9) as u16);
                    editor.set_line_numbers(rng.below(4) == 0);
                    editor.draw(&mut buf, area, true);
                }
                11 => {
                    let (x, y) = (rng.below(42) as u16, rng.below(14) as u16);
                    match rng.below(4) {
                        0 => editor.click(area, x, y, rng.below(2) == 0),
                        1 => editor.drag(area, x, y),
                        2 => editor.double_click(area, x, y),
                        _ => editor.scroll(rng.below(9) as i32 - 4),
                    }
                }
                12 => {
                    let query: String = (0..1 + rng.below(2)).map(|_| CHARS[rng.below(CHARS.len())]).collect();
                    editor.set_find(Some(&query));
                    editor.find_next(rng.below(2) == 0);
                }
                _ => {
                    if let Some(prefix) = editor.completion_prefix() {
                        editor.replace_before_cursor(prefix.chars().count(), "Note]]");
                    }
                }
            }
            let cursor = editor.cursor();
            assert_eq!(editor.clamp(cursor), cursor, "seed {seed}: the cursor stays inside the text");
            if let Some(find) = editor.find.clone() {
                let mut fresh = Vec::new();
                for (i, line) in editor.lines.iter().enumerate() {
                    find::line_matches(line, i, &find.lower, &mut fresh);
                }
                assert_eq!(find.matches, fresh, "seed {seed}: find matches follow the edits");
            }
            editor.draw(&mut buf, area, true);
        }
        // The block kinds kept while editing match a fresh pass.
        let mut state = markdown::BlockState::default();
        for i in 0..editor.line_count() {
            let (kind, next) = markdown::step(state, i, &editor.lines[i].clone());
            state = next;
            assert_eq!(editor.kind(i), kind, "seed {seed}: line {i}'s block kind");
        }
        let end = editor.text();
        let mut steps = 0;
        while editor.undo() {
            steps += 1;
        }
        assert_eq!(editor.text(), start, "seed {seed}: undoing everything restores the text");
        for _ in 0..steps {
            assert!(editor.redo());
        }
        assert_eq!(editor.text(), end, "seed {seed}: redoing as many steps comes back");
    }
}
