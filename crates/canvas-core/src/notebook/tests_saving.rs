//! Tests of saving against everything that can happen meanwhile: renames,
//! files gone from disk, quitting, closing and reopening, answers that
//! arrive late, conflicts settled from the Compare dialog.

use std::time::{Duration, Instant};

use canvas_model::notebook as nb;

use super::tabs::{self, View};
use super::tests::{Disk, Fixture, fixture, note, type_text};
use super::*;
use crate::doc::Doc;
use crate::input::{KeyCode, Modifiers, MouseButton, MouseEventKind};

fn rev(text: &str) -> String {
    nb::rev_of(text.as_bytes())
}

/// Every tab's autosave is due now.
fn due(f: &mut Fixture) {
    for tab in &mut f.app.notebook.tabs {
        tab.changed_at = Instant::now().checked_sub(Duration::from_secs(2));
    }
}

fn shown_tab_idle(app: &App) -> bool {
    app.notebook.tab().is_some_and(|t| !t.saving)
}

#[test]
fn a_rename_waits_for_a_save_and_saves_wait_for_the_rename() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("a.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    type_text(&mut f, "1");
    due(&mut f);
    tabs::autosave(&mut f.app);
    assert!(f.tab().saving);
    assert_eq!(ops::rename(&mut f.app, &note("a.md"), "b"), Err("a.md is being saved: try again in a moment.".into()));
    f.settle(shown_tab_idle);
    // While the rename is on its way nothing is saved (it would write the old path again).
    ops::rename(&mut f.app, &note("a.md"), "b").unwrap();
    assert!(f.tab().moving);
    type_text(&mut f, "2");
    due(&mut f);
    tabs::autosave(&mut f.app);
    assert!(!f.tab().saving, "no save while moving");
    f.settle(|app| app.notebook.tab().is_some_and(|t| !t.moving && t.path == note("b.md")));
    f.autosave();
    assert_eq!(disk.text(&note("b.md")).as_deref(), Some("12# A\n"));
    assert_eq!(disk.text(&note("a.md")), None, "the old path is not written again");
}

#[test]
fn answers_find_their_tab_after_it_moved() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("a.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    type_text(&mut f, "1");
    due(&mut f);
    tabs::autosave(&mut f.app);
    // The tab follows a rename while its write is on its way: the answer still reaches it.
    ops::retarget(&mut f.app, &note("a.md"), &note("b.md"));
    f.settle(shown_tab_idle);
    assert!(!f.tab().unsaved && f.tab().save_error.is_none() && f.tab().rev.as_deref() == Some(rev("1# A\n").as_str()));
    // A poll's read on its way when the tab moves: the tab polls again afterwards.
    disk.put(&note("b.md"), "# B\n");
    tabs::on_rev(&mut f.app, &note("b.md"), Some(rev("# B\n")));
    assert!(f.tab().reading);
    ops::retarget(&mut f.app, &note("b.md"), &note("c.md"));
    f.settle(|app| app.notebook.tab().is_some_and(|t| !t.reading));
}

#[test]
fn a_note_deleted_on_disk_is_written_again_and_a_failing_save_says_so_once() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("a.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    type_text(&mut f, "x");
    disk.files.lock().unwrap().remove(&note("a.md"));
    due(&mut f);
    tabs::autosave(&mut f.app);
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.save_error.is_some()));
    assert!(f.app.notice.as_ref().is_some_and(|n| n.text.starts_with("Could not save a.md")));
    assert!(f.app.notebook.poll_at.is_none(), "polling looks at once");
    // Tried again (autosave alone): not said again.
    f.app.notice = None;
    let past = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
    f.app.notebook.tab_mut().unwrap().save_error.as_mut().unwrap().1 = past;
    tabs::autosave(&mut f.app);
    f.settle(shown_tab_idle);
    assert!(f.app.notice.is_none() && f.tab().save_error.is_some());
    // The poll says the file is gone; the next try writes it anew.
    f.app.notebook.poll_at = None;
    tick(&mut f.app);
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.missing));
    f.app.notebook.tab_mut().unwrap().save_error.as_mut().unwrap().1 = past;
    tabs::autosave(&mut f.app);
    f.settle(shown_tab_idle);
    assert_eq!(disk.text(&note("a.md")).as_deref(), Some("x# A\n"));
    assert!(!f.tab().missing && !f.tab().unsaved && f.tab().save_error.is_none());
}

#[test]
fn quitting_writes_over_a_save_on_its_way_and_keeps_a_conflicted_buffer_in_a_copy() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n"), (&note("b.md"), "# B\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    // b.md: changed here and there, the conflict not settled.
    f.open(&note("b.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    type_text(&mut f, "x");
    disk.put(&note("b.md"), "theirs\n");
    f.app.notebook.poll_at = None;
    tick(&mut f.app);
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.conflict.is_some()));
    // a.md: a save on its way (landed, its answer not read yet) and typing after it.
    f.open(&note("a.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    type_text(&mut f, "1");
    due(&mut f);
    tabs::autosave(&mut f.app);
    let deadline = Instant::now() + Duration::from_secs(5);
    while disk.text(&note("a.md")).as_deref() != Some("1# A\n") {
        assert!(Instant::now() < deadline, "the first write landed");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(f.tab().saving);
    type_text(&mut f, "2");
    f.app.save_now();
    assert_eq!(disk.text(&note("a.md")).as_deref(), Some("12# A\n"), "written on top of the save on its way");
    assert_eq!(disk.text(&note("b.md")).as_deref(), Some("theirs\n"), "their version stays");
    let files = disk.files.lock().unwrap().clone();
    let copy = files.iter().find(|(path, _)| path.starts_with(&note("b (unsaved "))).expect("a copy of yours beside it");
    assert_eq!(copy.1, "x# B\n");
    assert!(copy.0.ends_with(").md") && f.app.save_error.is_none());
}

#[test]
fn reopening_a_note_whose_closing_save_runs_takes_that_tab_back() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("a.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    type_text(&mut f, "!");
    f.key(KeyCode::Char('w'), Modifiers::CONTROL);
    assert!(f.app.notebook.tabs.is_empty() && f.app.notebook.closing.len() == 1);
    f.app.navigate(Space::Notebook, NavTarget::Note(note("a.md")));
    assert!(f.app.notebook.closing.is_empty(), "the closing tab came back");
    assert_eq!((f.app.notebook.tabs.len(), f.tab().text().as_str()), (1, "!# A\n"));
    f.settle(shown_tab_idle);
    assert!(!f.tab().unsaved && f.app.notebook.tabs.len() == 1);
    assert_eq!(disk.text(&note("a.md")).as_deref(), Some("!# A\n"));
}

#[test]
fn a_read_started_before_the_tab_moved_on_is_not_applied() {
    let disk = Disk::with(&[(&note("a.md"), "A\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("a.md"));
    disk.put(&note("a.md"), "B\n");
    tabs::on_rev(&mut f.app, &note("a.md"), Some(rev("B\n")));
    assert!(f.tab().reading);
    // Before its answer the buffer moved on (Keep mine was written, say).
    let tab = f.app.notebook.tab_mut().unwrap();
    tab.editor.set_text("C\n");
    tab.rev = Some(rev("C\n"));
    f.settle(|app| app.notebook.tab().is_some_and(|t| !t.reading));
    assert_eq!(f.tab().text(), "C\n", "an older version does not replace it");
}

#[test]
fn typing_while_mark_reviewed_is_written_keeps_both() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("a.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    run(&mut f.app, Action::MarkReviewed(note("a.md")));
    f.key(KeyCode::End, Modifiers::CONTROL);
    type_text(&mut f, "x");
    f.settle(|app| app.notice.as_ref().is_some_and(|n| n.text == "a.md marked reviewed."));
    assert!(f.tab().conflict.is_none(), "the mark is no conflict with your typing");
    assert_eq!(f.tab().text(), "---\nreviewed: true\n---\n# A\nx");
    f.autosave();
    assert_eq!(disk.text(&note("a.md")).as_deref(), Some("---\nreviewed: true\n---\n# A\nx"));
}

#[test]
fn the_compare_dialog_settles_the_note_it_compared() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n"), (&note("b.md"), "# B\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("b.md"));
    f.open(&note("a.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    type_text(&mut f, "mine ");
    disk.put(&note("a.md"), "theirs\n");
    f.app.notebook.poll_at = None;
    tick(&mut f.app);
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.conflict.is_some()));
    dialogs::compare(&mut f.app);
    let Some(crate::dialogs::Modal::Notebook(mut dialog)) = f.app.modals.pop() else { panic!("the compare dialog") };
    // Another tab shows meanwhile (a closed tab whose save failed came back, say).
    f.app.notebook.active = f.app.notebook.tab_index(&note("b.md")).unwrap();
    let press = crate::input::MouseEvent::at_cell(MouseEventKind::Down(MouseButton::Left), 0, 0, Modifiers::NONE);
    dialogs::mouse(&mut f.app, &mut dialog, press, Some(crate::dialogs::Hit::Button(0)));
    f.settle(|app| app.notebook.tabs.iter().all(|t| !t.saving));
    assert_eq!(disk.text(&note("a.md")).as_deref(), Some("mine # A\n"), "Keep mine wrote the compared note");
    assert_eq!(disk.text(&note("b.md")).as_deref(), Some("# B\n"));
    let a = f.app.notebook.tab_index(&note("a.md")).unwrap();
    assert!(f.app.notebook.tabs[a].conflict.is_none());
}

#[test]
fn an_agents_note_becomes_mixed_with_your_first_change_and_undo_takes_both_back() {
    let disk = Disk::with(&[(&note("ai-docs/log.md"), "# Log\nStarted.")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("ai-docs/log.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    f.key(KeyCode::End, Modifiers::CONTROL);
    type_text(&mut f, " One");
    assert_eq!(f.tab().text(), "---\nauthor: mixed\n---\n# Log\nStarted. One");
    assert_eq!(f.tab().provenance.author, nb::Author::Mixed);
    f.autosave();
    assert_eq!(disk.text(&note("ai-docs/log.md")).as_deref(), Some("---\nauthor: mixed\n---\n# Log\nStarted. One"));
    // Saving keeps the history: one Ctrl+Z takes the typing and the line back, Ctrl+Y both again.
    f.key(KeyCode::Char('z'), Modifiers::CONTROL);
    assert_eq!(f.tab().text(), "# Log\nStarted.");
    f.key(KeyCode::Char('y'), Modifiers::CONTROL);
    assert_eq!(f.tab().text(), "---\nauthor: mixed\n---\n# Log\nStarted. One");
    assert_eq!(f.tab().editor.cursor(), crate::editor::Pos::new(4, 12), "the caret after the typing");
    // Undone, then typed again: the line comes back with the new change.
    f.key(KeyCode::Char('z'), Modifiers::CONTROL);
    assert_eq!(f.tab().provenance.author, nb::Author::Ai);
    type_text(&mut f, "!");
    assert_eq!(f.tab().text(), "---\nauthor: mixed\n---\n# Log\nStarted.!");
    f.autosave();
    assert_eq!(disk.text(&note("ai-docs/log.md")).as_deref(), Some("---\nauthor: mixed\n---\n# Log\nStarted.!"));
}

#[test]
fn find_matches_what_was_typed_in_any_script() {
    let disk = Disk::with(&[(&note("a.md"), "και οδος\nΟΔΟΣ one\n"), (&note("b.md"), "# B\n")]);
    let mut f = fixture(disk, Doc::default());
    f.open(&note("a.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    f.key(KeyCode::Char('f'), Modifiers::CONTROL);
    type_text(&mut f, "ΟΔΟΣ");
    f.key(KeyCode::Enter, Modifiers::NONE);
    let find = f.app.notebook.find.clone().unwrap();
    assert_eq!(tabs::find_count(f.app.notebook.tab_mut().unwrap(), &find), (1, 1));
    assert_eq!(f.tab().editor.selection().map(|(s, _)| s.line), Some(1), "the word as typed, not the final-sigma form");
    // Spaces are part of the query.
    f.key(KeyCode::Char('u'), Modifiers::CONTROL);
    type_text(&mut f, "οδος ");
    f.key(KeyCode::Enter, Modifiers::NONE);
    let find = f.app.notebook.find.clone().unwrap();
    assert_eq!(tabs::find_count(f.app.notebook.tab_mut().unwrap(), &find).1, 0);
    // Another tab and back: the bar closed, its matches are no longer lit.
    f.open(&note("b.md"));
    f.open(&note("a.md"));
    assert!(f.app.notebook.find.is_none() && f.tab().editor.find_status().is_none());
}

#[test]
fn known_notes_forget_deleted_ones_and_follow_renames() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n")]);
    let mut f = fixture(disk, Doc::default());
    f.app.notebook.known.push(Known { path: note("gone.md"), name: "gone.md".into(), title: String::new(), ai: false });
    f.app.switch_space(Space::Notebook);
    tick(&mut f.app);
    f.settle(|app| app.notebook.info.is_some());
    tick(&mut f.app);
    f.settle(|app| !app.notebook.known_loading && app.notebook.known.iter().any(|k| k.name == "a.md"));
    assert!(!f.app.notebook.known.iter().any(|k| k.name == "gone.md"), "the walk replaced the list");
    ops::retarget(&mut f.app, &note("a.md"), &note("Inbox/b.md"));
    assert!(f.app.notebook.known.iter().any(|k| k.path == note("Inbox/b.md") && k.name == "b.md"));
    assert!(!goto_items(&f.app).iter().any(|i| i.target == NavTarget::Note(note("a.md"))));
}

#[test]
fn the_info_panel_reads_a_note_once_per_version_and_keeps_backlinks_bounded() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n## Part\nSee [[b]].\n"), (&note("b.md"), "# B\nSee [[a]].\n")]);
    let mut f = fixture(disk, Doc::default());
    f.open(&note("a.md"));
    let area = ratatui::layout::Rect::new(0, 0, 40, 30);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    draw_info(&mut f.app, &mut buf, area);
    let first = f.app.notebook.info_cache.clone().expect("read once").1;
    draw_info(&mut f.app, &mut buf, area);
    assert!(std::sync::Arc::ptr_eq(&first, &f.app.notebook.info_cache.clone().unwrap().1), "not again for the same version");
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    type_text(&mut f, "x");
    draw_info(&mut f.app, &mut buf, area);
    assert!(!std::sync::Arc::ptr_eq(&first, &f.app.notebook.info_cache.clone().unwrap().1), "again after a change");
    // Backlinks of many notes seen: the map stays small.
    for i in 0..100 {
        f.app.notebook.backlinks.insert(format!("c:\\nb\\old{i}.md"), Backlinks { at: Some(Instant::now()), ..Backlinks::default() });
    }
    f.app.notebook.backlinks.remove(&norm(&note("a.md")));
    draw_info(&mut f.app, &mut buf, area);
    f.settle(|app| app.notebook.backlinks.get(&norm(&note("a.md"))).is_some_and(|b| !b.loading));
    assert!(f.app.notebook.backlinks.len() <= 2, "{} kept", f.app.notebook.backlinks.len());
    assert_eq!(f.app.notebook.backlinks[&norm(&note("a.md"))].hits.len(), 1, "b.md links here");
}

#[test]
fn an_html_note_is_saved_before_it_shows_rendered() {
    let disk = Disk::with(&[(&note("page.html"), "<h1>P</h1>\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("page.html"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    assert_eq!(f.tab().view, View::Source);
    type_text(&mut f, "<p>x</p>");
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    assert!(f.tab().view == View::Rendered && f.tab().saving, "written now: the HTML view shows the file");
    f.settle(shown_tab_idle);
    assert_eq!(disk.text(&note("page.html")).as_deref(), Some("<p>x</p><h1>P</h1>\n"));
}

#[test]
fn wiki_links_prefer_the_file_name_in_any_script_and_ties_do_not_depend_on_order() {
    let known = |path: &str, title: &str| Known { path: path.into(), name: file_name(path), title: title.into(), ai: false };
    let list = vec![known(r"C:\nb\x.md", "Ärger"), known(r"C:\nb\Groups\Docs\Ärger.md", "Notes")];
    assert_eq!(tabs::resolve(&list, "ärger", None).as_deref(), Some(r"C:\nb\Groups\Docs\Ärger.md"), "the name before a title");
    let mut twins = vec![known(r"C:\nb\B\plan.md", ""), known(r"C:\nb\A\plan.md", "")];
    let one = tabs::resolve(&twins, "plan", None);
    twins.reverse();
    assert_eq!(tabs::resolve(&twins, "plan", None), one);
    assert_eq!(one.as_deref(), Some(r"C:\nb\A\plan.md"));
}
