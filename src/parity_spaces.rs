//! The parity suite's checks of the spaces (docs/NOTEBOOK-TASKS.md) on the
//! sandbox canvas: the Notebook (row 48), Tasks with a book the CLI and the
//! sandbox service keep (49), the canvas's ✎ / ☑ chips and ☑ badges (50),
//! switching spaces, Go to and the history (47), and HTML notes in the
//! WebView2 view served from the notes origin (41d). Todos come from
//! `copilot-canvas todo …` as you and as canvas sessions (an agent's
//! proposal and claim); nothing starts a session.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use canvas_model::todos::{Status, TodoBook};
use serde_json::{Value, json};

use super::prs::cli;
use super::{Gui, Kind, Row, Sandbox, check, check_kind};
use crate::selftest::{Ui, wait_doc};

/// The main area's columns (170 columns, the default sidebar and right pane).
const MAIN: (u16, u16) = (34, 95);

fn launches(sb: &Sandbox) -> usize {
    std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default().matches("launched ").count()
}

fn canvas_id(sb: &Sandbox) -> Result<String> {
    sb.doc()?.canvas_id.context("the service gave the sandbox canvas an id")
}

/// The sandbox canvas's todo book as the service stored it, once `ok`.
fn wait_book(sb: &Sandbox, what: &str, ok: impl Fn(&TodoBook) -> bool) -> Result<TodoBook> {
    let canvas = canvas_id(sb)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let book = canvas_agents::todos_cli::load(&sb.instance.home, &canvas);
        if ok(&book) {
            return Ok(book);
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {what}: {}", book.todos.iter().map(|t| format!("{} {:?}", t.id, t.status)).collect::<Vec<_>>().join(", "));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn status(book: &TodoBook, id: &str) -> Option<Status> {
    book.todo(id).map(|t| t.status)
}

/// Polls `get` until it gives something.
fn wait_for<T>(what: &str, secs: u64, get: impl Fn() -> Option<T>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(value) = get() {
            return Ok(value);
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {what}");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// The centre of the first click target whose hit (its debug text) contains `needle`.
fn hit(gui: &Gui, needle: &str) -> Option<(f64, f64)> {
    let tree = gui.tree().ok()?;
    let hits = tree["hits"].as_array()?;
    hits.iter().find(|h| h["hit"].as_str().is_some_and(|s| s.contains(needle))).and_then(|h| center(&h["rect"]))
}

fn center(rect: &Value) -> Option<(f64, f64)> {
    let r: Vec<f64> = rect.as_array()?.iter().filter_map(Value::as_f64).collect();
    (r.len() == 4).then(|| (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0))
}

/// A chip of a canvas group (its label starts with `glyph`): (label, attention, rect `[x, y, w, h]`).
fn chip_rect(tree: &Value, group: &str, glyph: &str) -> Option<(String, bool, [f64; 4])> {
    let group = tree["canvas"]["groups"].as_array()?.iter().find(|g| g["id"] == group)?;
    let chip = group["chips"].as_array()?.iter().find(|c| c["label"].as_str().is_some_and(|l| l.starts_with(glyph)))?;
    let r: Vec<f64> = chip["rect"].as_array()?.iter().filter_map(Value::as_f64).collect();
    Some((chip["label"].as_str().unwrap_or_default().to_owned(), chip["attention"] == true, r.try_into().ok()?))
}

/// A chip of a canvas group (its label starts with `glyph`): (label, attention, centre).
fn chip(gui: &Gui, group: &str, glyph: &str) -> Option<(String, bool, (f64, f64))> {
    let (label, attention, r) = chip_rect(&gui.tree().ok()?, group, glyph)?;
    Some((label, attention, (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0)))
}

/// A chip as [`chip`] that `ok` accepts, whole inside the canvas's area (a
/// chip partly outside it has no click target) and where it was a poll
/// before (a view that fits moves for a moment). *Show all nodes* (`f`)
/// brings the group into view; Ctrl+arrows pan a chip that sticks out (chips
/// keep their size at every zoom, so they can overhang a fitted frame, as in
/// a window narrowed by the details pane).
fn visible_chip(gui: &Gui, group: &str, glyph: &str, ok: impl Fn(&str, bool) -> bool) -> Result<(String, bool, (f64, f64))> {
    let last = std::cell::Cell::new(None);
    wait_for(&format!("the {glyph} chip of {group} in view"), 10, || {
        let tree = gui.tree().ok()?;
        let (label, attention, r) = chip_rect(&tree, group, glyph).filter(|(label, attention, _)| ok(label, *attention))?;
        let a: Vec<f64> = tree["canvas"]["area"].as_array()?.iter().filter_map(Value::as_f64).collect();
        if a.len() != 4 {
            return None;
        }
        let at = (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
        let near = at.0 > a[0] && at.0 < a[0] + a[2] && at.1 > a[1] && at.1 < a[1] + a[3];
        let pan = if r[0] < a[0] {
            Some("Left")
        } else if r[0] + r[2] > a[0] + a[2] {
            Some("Right")
        } else if r[1] < a[1] {
            Some("Up")
        } else if r[1] + r[3] > a[1] + a[3] {
            Some("Down")
        } else {
            None
        };
        match pan {
            None => return (last.replace(Some(at)) == Some(at)).then_some((label, attention, at)),
            Some(dir) if near => {
                let _ = gui.key(dir, &["ctrl"]);
            }
            Some(_) => {
                let _ = gui.chord('f', &[]);
            }
        }
        last.set(None);
        None
    })
}

/// `needle` in the main area (cell centre).
fn in_main(gui: &Gui, needle: &str) -> Option<(f64, f64)> {
    gui.driver.find_in(needle, MAIN.0, MAIN.1).map(|(x, y)| (x as f64 + 0.5, y as f64 + 0.5))
}

fn click_main(gui: &Gui, needle: &str) -> Result<()> {
    let at = in_main(gui, needle).with_context(|| format!("{needle:?} in the main area\n{}", gui.screen()))?;
    gui.click(at, &[])
}

fn click_text(gui: &Gui, needle: &str) -> Result<()> {
    let at = gui.find(needle).with_context(|| format!("{needle:?} on screen\n{}", gui.screen()))?;
    gui.click(at, &[])
}

fn notebook(t: &str) -> bool {
    t.contains("✎ EXPLORER")
}

fn tasks(t: &str) -> bool {
    t.contains("☑ TASKS")
}

/// Ctrl+P, a query, Enter (from the canvas's keyboard: a terminal keeps Ctrl+P).
fn go_to(gui: &mut Gui, query: &str) -> Result<()> {
    gui.canvas_focus()?;
    gui.chord('p', &["ctrl"])?;
    gui.wait("Go to", 10, |t| t.contains("Go to ─"))?;
    gui.send(query)?;
    gui.wait(&format!("{query:?} in Go to"), 10, |t| t.contains(query))?;
    gui.key("Enter", &[])?;
    gui.wait("Go to closed", 10, |t| !t.contains("Go to ─"))
}

/// The HTML view's state (`webview` hook) once `ok`.
fn web(gui: &Gui, what: &str, secs: u64, ok: impl Fn(&Value) -> bool) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let state = gui.req(json!({ "cmd": "webview" }))?;
        if ok(&state) {
            return Ok(state);
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {what}: {state}");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Runs script in the shown page (host script) and parses its JSON answer.
fn eval(gui: &Gui, script: &str) -> Result<Value> {
    let answer = gui.req(json!({ "cmd": "webview", "eval": script }))?;
    anyhow::ensure!(answer["ok"] == true, "eval failed: {answer}");
    Ok(serde_json::from_str(answer["result"].as_str().unwrap_or("null")).unwrap_or(answer["result"].clone()))
}

/// The pane's file (`HtmlSource::File`) path, if it shows one.
fn pane_file(state: &Value) -> String {
    state["pane"]["file"]["path"].as_str().unwrap_or_default().to_owned()
}

fn wait_file(path: &Path, what: &str, ok: impl Fn(&str) -> bool) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if ok(&text) {
            return Ok(text);
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {what}: {text:?}");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

pub(super) fn rows(sb: &Sandbox, gui: &mut Gui, rows: &mut Vec<Row>) {
    check(rows, "48", "New: the Notebook: explorer (a group's folder, ● / ◇), Human | All | AI filter, reader (Rendered in the HTML view, Source, Live), editor (typing, Ctrl+S), quick capture, changes on disk (reload, conflict)", "notebook_explorer_reader_editor", || notebook_check(sb, gui));
    check(rows, "49", "New: Tasks: the todo canvas (boards, columns, cards) of a book the CLI wrote, a todo's details, A accepts a proposal, a dragged card changes status; the service keeps every change", "tasks_canvas_details_accept_drag", || tasks_check(sb, gui));
    check(rows, "50", "New: canvas links: a group's ✎ Notes and ☑ chips (counts, gold while proposals wait) and a session's ☑ ID badge open the Notebook and Tasks; nothing starts", "chips_and_badges_navigate", || chips_check(sb, gui));
    check(rows, "47", "New: spaces: toolbar tabs and Ctrl+1/2/3; Go to (Ctrl+P) over groups, sessions, notes and todos; back and forward (Alt+← / Alt+→, Alt+, / Alt+.); nothing starts", "spaces_goto_and_history", || spaces_check(sb, gui));
    check_kind(rows, "41d", "New: HTML notes in the GUI's WebView2 view from the notes origin (relative images and styles), scripts off unless allowed, #note: / #todo: links, Print / PDF", "html_note_pane", Kind::Only("GUI only: the TUI shows HTML notes as formatted text"), || html_note_check(sb, gui));
    // Trackpads (MAC-LINUX-PLAN D15), where scrolling is pixel-precise (as T1 and T2).
    if cfg!(target_os = "macos") || super::linux_wayland() {
        check_kind(rows, "T3", "Trackpad in the Notebook and Tasks: a note scrolls row by row, the todo canvas pans cell by cell and steps its zoom (Command + scroll, pinch), smart zoom fits the boards; the session canvas never moves", "trackpad_spaces", Kind::Only("GUI only: a terminal gives the TUI wheel notches"), || {
            let result = trackpad_spaces(sb, gui);
            let back = gui.chord('1', &["ctrl"]);
            let summary = result?;
            back.map(|_| summary)
        });
    }
}

/// T3: the Notebook's editor and the todo canvas under a trackpad.
/// Ctrl+`digit` and the space's own view in the sidebar (`seen`): an earlier
/// row may have left the sidebar on another view (41b leaves Reports), which
/// Alt+1 (slot 1, the space's view) replaces.
fn space_with_view(gui: &mut Gui, digit: char, what: &str, seen: fn(&str) -> bool) -> Result<()> {
    gui.chord(digit, &["ctrl"])?;
    if gui.wait(what, 3, seen).is_err() {
        gui.chord('1', &["alt"])?;
        gui.wait(what, 10, seen)?;
    }
    Ok(())
}

fn trackpad_spaces(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    let dir = sb.group_folder.join("human-outputs");
    std::fs::create_dir_all(&dir)?;
    let lines: String = (0..120).map(|i| format!("Long line {i}\n")).collect();
    std::fs::write(dir.join("long.md"), format!("# Long\n{lines}"))?;
    gui.canvas_focus()?;
    gui.chord('1', &["ctrl"])?;
    let view = gui.wait_tree("the canvas", 10, |t| !t["canvas"].is_null())?["view"].clone();
    let tree = gui.tree()?;
    let ch = tree["cell"][1].as_f64().unwrap_or(1.0);
    let cw = tree["cell"][0].as_f64().unwrap_or(1.0);
    let spaces = |gui: &Gui| gui.tree().map(|t| t["spaces"].clone());
    let px = |dx: f64, dy: f64, phase: &str| json!({ "phase": phase, "dx": dx * cw, "dy": dy * ch });
    // The note in Source (the editor; Rendered Markdown is the HTML view, which scrolls itself).
    space_with_view(gui, '2', "the Notebook", notebook)?;
    let at = hit(gui, "Notebook(Refresh)").context("the explorer's ⟳")?;
    gui.click(at, &[])?;
    if gui.wait("long.md in the explorer", 5, |t| t.contains("long.md")).is_err() {
        click_side(gui, "human-outputs")?;
        gui.wait("long.md in the explorer", 15, |t| t.contains("long.md"))?;
    }
    click_side(gui, "long.md")?;
    gui.chord('e', &["ctrl"])?;
    gui.wait("the note in Source", 15, |t| t.contains("Long line 3"))?;
    let first = |gui: &Gui| spaces(gui).ok().and_then(|s| s["notebook"]["first_line"].as_u64());
    let start = first(gui).context("the editor's first line")?;
    let page = in_main(gui, "Long line 3").context("the note's text")?;
    // Fingers up 6.5 rows: 6 lines (the half waits); down 6: back.
    super::gesture(gui, "scroll_px", page, px(0.0, 0.0, "began"))?;
    for _ in 0..13 {
        super::gesture(gui, "scroll_px", page, px(0.0, -0.5, "changed"))?;
    }
    wait_for("the note 6 lines down", 5, || (first(gui) == Some(start + 6)).then_some(()))?;
    super::gesture(gui, "scroll_px", page, px(0.0, 6.0, "changed"))?;
    super::gesture(gui, "scroll_px", page, px(0.0, 0.0, "ended"))?;
    wait_for("the note back at its top", 5, || (first(gui) == Some(start)).then_some(()))?;
    // The todo canvas: whole cells on both axes, the content following the fingers.
    std::thread::sleep(Duration::from_millis(400));
    space_with_view(gui, '3', "Tasks", tasks)?;
    let s = spaces(gui)?;
    let main: Vec<f64> = s["main"].as_array().into_iter().flatten().filter_map(Value::as_f64).collect();
    anyhow::ensure!(main.len() == 4, "the todo canvas's area: {s}");
    let board = (main[0] + main[2] - 3.5, main[1] + main[3] - 4.5);
    let tasks_view = |gui: &Gui| -> Result<((i64, i64), u64)> {
        let t = spaces(gui)?["tasks"].clone();
        Ok(((t["pan"][0].as_i64().unwrap_or(0), t["pan"][1].as_i64().unwrap_or(0)), t["zoom"].as_u64().unwrap_or(0)))
    };
    let (pan, zoom) = tasks_view(gui)?;
    let d = canvas_core::tasks::layout::DIVS[(zoom as usize).min(2)] as i64;
    super::gesture(gui, "scroll_px", board, px(0.0, 0.0, "began"))?;
    super::gesture(gui, "scroll_px", board, px(-1.25, -0.75, "changed"))?;
    super::gesture(gui, "scroll_px", board, px(-1.25, -0.75, "changed"))?;
    super::gesture(gui, "scroll_px", board, px(-0.5, -0.5, "changed"))?;
    super::gesture(gui, "scroll_px", board, px(0.0, 0.0, "ended"))?;
    let (moved, _) = tasks_view(gui)?;
    anyhow::ensure!(moved == (pan.0 + 3 * d, pan.1 + 2 * d), "panned (3, 2) cells: {pan:?} → {moved:?} (×{d})");
    // Command + scroll: a zoom level per half a doubling (out, or in at the last level).
    std::thread::sleep(Duration::from_millis(400));
    let out = zoom < 2;
    let command = if cfg!(target_os = "macos") { "super" } else { "ctrl" };
    let mut scroll = px(0.0, if out { -8.0 } else { 8.0 }, "began");
    scroll["mods"] = json!([command]);
    super::gesture(gui, "scroll_px", board, scroll)?;
    super::gesture(gui, "scroll_px", board, px(0.0, 0.0, "ended"))?;
    let stepped = tasks_view(gui)?.1;
    anyhow::ensure!(stepped == if out { zoom + 1 } else { zoom - 1 }, "Command + scroll stepped the zoom: {zoom} → {stepped}");
    let mut summary = format!("a note in Source 6 rows down and back from 6.5 and 6 rows of movement; the todo canvas panned (3, 2) cells from (2.5, 1.5) + (0.5, 0.5); Command + scroll zoom {zoom} → {stepped}");
    if cfg!(target_os = "macos") {
        // Pinching back; smart zoom fits the boards as F does.
        std::thread::sleep(Duration::from_millis(400));
        super::gesture(gui, "pinch", board, json!({ "phase": "began", "delta": 0.0 }))?;
        super::gesture(gui, "pinch", board, json!({ "phase": "changed", "delta": if out { 0.5 } else { -0.35 } }))?;
        super::gesture(gui, "pinch", board, json!({ "phase": "ended", "delta": 0.0 }))?;
        anyhow::ensure!(tasks_view(gui)?.1 == zoom, "the pinch stepped it back to {zoom}: {:?}", tasks_view(gui)?);
        gui.send("f")?;
        let fitted = tasks_view(gui)?;
        super::gesture(gui, "scroll_px", board, px(0.0, 0.0, "began"))?;
        super::gesture(gui, "scroll_px", board, px(4.0, 2.0, "changed"))?;
        super::gesture(gui, "scroll_px", board, px(0.0, 0.0, "ended"))?;
        anyhow::ensure!(tasks_view(gui)? != fitted, "panned away");
        super::gesture(gui, "smart_zoom", board, json!({}))?;
        anyhow::ensure!(tasks_view(gui)? == fitted, "smart zoom fits as F: {fitted:?} vs {:?}", tasks_view(gui)?);
        summary.push_str(&format!(", a pinch back to {zoom}; smart zoom = F {fitted:?}"));
    }
    let now = gui.tree()?["view"].clone();
    anyhow::ensure!(now == view, "the session canvas moved: {view} → {now}");
    Ok(format!("{summary}; the session canvas never moved"))
}

/// With its own GUI over `--remote loopback`.
pub(super) fn late_rows(sb: &Sandbox, rows: &mut Vec<Row>) {
    check(rows, "48b", "New: the Notebook and Tasks in remote mode: listing, reading and saving notes and the todo book through the service (--remote loopback)", "remote_notebook_and_tasks", || remote_check(sb));
}

fn remote_check(sb: &Sandbox) -> Result<String> {
    let dir = sb.group_folder.join("human-outputs");
    std::fs::create_dir_all(&dir)?;
    let note = dir.join("remote.md");
    std::fs::write(&note, "# Remote note\n\nWritten on the service's machine.\n")?;
    let before = launches(sb);
    let mut gui = sb.start(&["--remote", "loopback"], "gui-remote-spaces.log")?;
    gui.wait("the canvas over TCP", 25, |t| t.contains("Parity group"))?;
    // The board (when this row runs alone), once the window routes its canvas.
    if !canvas_agents::todos_cli::load(&sb.instance.home, &canvas_id(sb)?).boards.iter().any(|b| b.prefix == "PAR") {
        wait_for("the canvas routed (todo board add)", 10, || cli(sb, "", &["todo", "board", "add", "Parity board", "--prefix", "PAR"]).ok())?;
        cli(sb, "", &["todo", "add", "Parity task one", "--type", "feature", "--board", "PAR", "--status", "todo"])?;
    }
    let (_, _, at) = visible_chip(&gui, "g-parity", "✎", |_, _| true)?;
    gui.click(at, &[])?;
    gui.wait("the group's folder listed through the service", 15, |t| notebook(t) && t.contains("human-outputs"))?;
    if gui.driver.find_in("remote.md", crate::selftest::SIDEBAR.0, crate::selftest::SIDEBAR.1).is_none() {
        click_side(&gui, "human-outputs")?;
    }
    gui.wait("the note in the explorer", 15, |t| t.contains("remote.md"))?;
    click_side(&gui, "remote.md")?;
    gui.chord('e', &["ctrl"])?;
    gui.wait("its text read through the service", 15, |t| t.contains("# Remote note") && t.contains("Written on the service's machine."))?;
    gui.key("End", &["ctrl"])?;
    gui.send("Saved over TCP.")?;
    gui.chord('s', &["ctrl"])?;
    wait_file(&note, "the note saved through the service", |t| t.contains("Saved over TCP."))?;
    gui.chord('3', &["ctrl"])?;
    gui.wait("the todo book through the service", 15, |t| tasks(t) && t.contains("Parity board · PAR") && t.contains("PAR-001"))?;
    let log = std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default();
    anyhow::ensure!(log.contains("connected (gui, protocol 8"), "a remote GUI");
    anyhow::ensure!(launches(sb) == before, "the remote Notebook or Tasks started a session");
    let _ = gui.req(json!({ "cmd": "quit" }));
    let _ = gui.driver.wait_exit(Duration::from_secs(10));
    Ok("over loopback TCP: the ✎ chip listed the group's folder, remote.md read in Source and saved (Ctrl+S) on the service's machine; the board and its todos in Tasks".into())
}

/// `needle` in the sidebar, clicked.
fn click_side(gui: &Gui, needle: &str) -> Result<()> {
    let (x, y) = gui.driver.find_in(needle, crate::selftest::SIDEBAR.0, crate::selftest::SIDEBAR.1).with_context(|| format!("{needle:?} in the sidebar\n{}", gui.screen()))?;
    gui.click((x as f64 + 0.5, y as f64 + 0.5), &[])
}

/// A segment of the reader's `Rendered │ Source │ Live` switch.
fn view_switch(gui: &Gui, label: &str) -> Result<()> {
    let (x, y) = in_main(gui, &format!("│ {label}")).with_context(|| format!("the {label} switch\n{}", gui.screen()))?;
    gui.click((x + 2.0, y), &[])
}

/// The main area's text.
fn main_text(gui: &Gui) -> String {
    gui.screen().lines().map(|l| l.chars().skip(MAIN.0 as usize).take((MAIN.1 - MAIN.0) as usize).collect::<String>()).collect::<Vec<_>>().join("\n")
}

fn notebook_check(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    let folder = &sb.group_folder;
    for sub in ["ai-docs", "human-outputs"] {
        std::fs::create_dir_all(folder.join(sub))?;
    }
    std::fs::write(folder.join("ai-docs").join("plan.md"), "---\naudience: ai\nauthor: ai\n---\n# Parity plan\n\nSteps for the next session.\n")?;
    let summary = folder.join("human-outputs").join("summary.md");
    std::fs::write(&summary, "# Parity summary\n\nSee [[plan]] and PAR-001.\n")?;
    let before = launches(sb);
    gui.canvas_focus()?;
    gui.chord('1', &["ctrl"])?;
    gui.wait_tree("the canvas", 10, |t| !t["canvas"].is_null())?;
    // The group's ✎ Notes chip: its folder (outside the notebook root) is an explorer root.
    let (_, _, at) = visible_chip(gui, "g-parity", "✎", |_, _| true)?;
    gui.click(at, &[])?;
    gui.wait("the group's folder in the explorer", 10, |t| notebook(t) && t.contains("◫ Parity group") && t.contains("ai-docs") && t.contains("human-outputs"))?;
    click_side(gui, "ai-docs")?;
    gui.wait("the agents' note (◇)", 10, |t| t.contains("◇ plan.md"))?;
    click_side(gui, "human-outputs")?;
    gui.wait("the note for you (●)", 10, |t| t.contains("● summary.md"))?;
    click_side(gui, " Human ")?;
    gui.wait("Human: the agents' notes hidden", 10, |t| t.contains("AI hidden") && !t.contains("plan.md"))?;
    click_side(gui, " All ")?;
    gui.wait("All: shown again", 10, |t| t.contains("◇ plan.md") && !t.contains("AI hidden"))?;
    // Rendered: the Markdown note as a page in the HTML view.
    click_side(gui, "summary.md")?;
    web(gui, "the summary in the HTML view", 30, |s| {
        let id = s["pane"]["id"].as_str().unwrap_or_default().to_owned();
        id.contains("summary.md") && s["visible"] == true && s["views"][0]["shown"] == id.as_str() && s["views"][0]["loading"] == false && s["pane"]["covered"] == false
    })?;
    let seen = eval(gui, "JSON.stringify({h1: (document.querySelector('h1') || {}).textContent, note: !!document.querySelector('a[href=\"#note:plan\"]'), todo: !!document.querySelector('a[href=\"#todo:PAR-001\"]'), scripts: document.scripts.length})")?;
    anyhow::ensure!(seen["h1"] == "Parity summary" && seen["note"] == true && seen["todo"] == true && seen["scripts"] == 0, "the rendered page: {seen}");
    // Source: the editor (cells); the HTML view hides.
    gui.chord('e', &["ctrl"])?;
    gui.wait("Source", 10, |t| t.contains("# Parity summary") && t.contains("See [[plan]] and PAR-001."))?;
    web(gui, "the HTML view hidden in Source", 10, |s| s["visible"] == false)?;
    gui.key("End", &["ctrl"])?;
    gui.send("Typed in parity.")?;
    gui.chord('s', &["ctrl"])?;
    wait_file(&summary, "the typed line saved (Ctrl+S)", |t| t.contains("Typed in parity."))?;
    view_switch(gui, "Live")?;
    gui.wait("Live: the heading without its #", 10, |_| {
        let main = main_text(gui);
        main.contains("Parity summary") && !main.contains("# Parity summary")
    })?;
    view_switch(gui, "Source")?;
    gui.wait("Source again", 10, |t| t.contains("# Parity summary"))?;
    // A clean note follows its file; an edited one asks (the revisions are polled every 2 s).
    std::fs::write(&summary, "# Parity summary\n\nSee [[plan]] and PAR-001.\nTyped in parity.\nChanged outside.\n")?;
    gui.wait("the change on disk shown (nothing typed since the save)", 10, |t| t.contains("Changed outside."))?;
    gui.key("End", &["ctrl"])?;
    gui.send(" Mine.")?;
    std::fs::write(&summary, "# Parity summary\n\nTheirs won.\n")?;
    gui.wait("the conflict banner", 15, |t| t.contains("This note changed on disk while you edited it.") && t.contains("Keep mine") && t.contains("Take theirs"))?;
    click_text(gui, "Take theirs")?;
    gui.wait("their text in the editor", 10, |t| t.contains("Theirs won.") && !t.contains("Mine."))?;
    std::thread::sleep(Duration::from_millis(1500));
    let text = std::fs::read_to_string(&summary)?;
    anyhow::ensure!(text == "# Parity summary\n\nTheirs won.\n", "Take theirs kept their file: {text:?}");
    // Quick capture (Alt+N): a note in the Inbox, in Source mode.
    let inbox = sb.instance.home.join("notebook").join("Inbox");
    let count = || std::fs::read_dir(&inbox).map(|d| d.flatten().count()).unwrap_or(0);
    let notes = count();
    gui.chord('n', &["alt"])?;
    gui.wait("the quick note in Source mode", 10, |t| t.contains("audience: human") && t.contains("Inbox"))?;
    anyhow::ensure!(count() == notes + 1, "Alt+N wrote one note into {}", inbox.display());
    anyhow::ensure!(launches(sb) == before, "the Notebook started a session");
    Ok("✎ chip → the group's folder (● summary.md, ◇ plan.md); Human hid the agents' notes; Rendered in the HTML view (wiki and todo links); Source, Ctrl+S, Live; reloaded a clean note, Take theirs on a conflict; Alt+N → Inbox".into())
}

fn tasks_check(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    let before = launches(sb);
    cli(sb, "", &["todo", "board", "add", "Parity board", "--prefix", "PAR"])?;
    cli(sb, "", &["todo", "board", "link", "PAR", "--group", "Parity group"])?;
    cli(sb, "", &["todo", "add", "Parity task one", "--type", "feature", "--board", "PAR", "--status", "todo"])?;
    cli(sb, "", &["todo", "add", "Parity task two", "--type", "bug", "--board", "PAR", "--status", "backlog"])?;
    // An agent's own idea (a canvas session's environment) is a proposal.
    cli(sb, &sb.sessions.idle, &["todo", "propose", "Parity idea", "--type", "improvement", "--why", "Found while testing", "--board", "PAR"])?;
    wait_book(sb, "three todos, one proposed", |b| b.todos.len() == 3 && status(b, "PAR-003") == Some(Status::Proposed))?;
    gui.canvas_focus()?;
    gui.chord('3', &["ctrl"])?;
    gui.wait("the board on the todo canvas", 10, |t| tasks(t) && t.contains("Parity board · PAR") && in_main(gui, "PAR-003").is_some())?;
    click_main(gui, "PAR-003")?;
    gui.wait("the proposal's details", 10, |t| t.contains("PAR-003 · improvement") && t.contains("Parity idea"))?;
    gui.send("a")?;
    wait_book(sb, "the proposal accepted at the service", |b| status(b, "PAR-003") == Some(Status::Backlog))?;
    // PAR-002 (Backlog) dragged into the Todo column.
    gui.wait("the Todo column", 10, |_| in_main(gui, "│Todo ").or_else(|| in_main(gui, "│ Todo ")).is_some() && in_main(gui, "PAR-002").is_some())?;
    let from = in_main(gui, "PAR-002").context("PAR-002's card")?;
    let to = in_main(gui, "│Todo ").or_else(|| in_main(gui, "│ Todo ")).context("the Todo column")?;
    gui.drag("left", from, (to.0 + 4.0, to.1 + 3.0), &[])?;
    wait_book(sb, "the drag at the service", |b| status(b, "PAR-002") == Some(Status::Todo))?;
    gui.wait("its details", 10, |t| t.contains("PAR-002 · bug") && t.contains("Status   Todo ▾"))?;
    // Esc closes them; ↓ selects the first card in view and Enter opens its details.
    gui.key("Esc", &[])?;
    gui.wait("no details", 10, |t| !t.contains("Status   "))?;
    gui.key("Down", &[])?;
    gui.key("Enter", &[])?;
    gui.wait("a card's details (Enter)", 10, |t| t.contains("Status   ") && t.contains("Priority "))?;
    anyhow::ensure!(launches(sb) == before, "Tasks started a session");
    Ok("board, 2 todos and an agent's proposal from the CLI on the todo canvas; A → accepted at the service; PAR-002 dragged Backlog → Todo at the service; Esc, ↓, Enter → details".into())
}

/// The box named `name` in the tree.
fn tree_box(tree: &Value, name: &str) -> Value {
    tree["canvas"]["boxes"].as_array().and_then(|b| b.iter().find(|b| b["name"] == name).cloned()).unwrap_or_default()
}

fn chips_check(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    let before = launches(sb);
    // A canvas session claims a todo (its box shows ☑ PAR-001); an agent proposes another.
    cli(sb, &sb.sessions.stopped, &["todo", "claim", "PAR-001"])?;
    cli(sb, &sb.sessions.idle, &["todo", "propose", "Parity second idea", "--type", "chore", "--why", "Seen in the logs", "--board", "PAR"])?;
    wait_book(sb, "the claim and the second proposal", |b| status(b, "PAR-001") == Some(Status::InProgress) && status(b, "PAR-004") == Some(Status::Proposed))?;
    gui.canvas_focus()?;
    gui.chord('1', &["ctrl"])?;
    gui.wait_tree("the canvas", 10, |t| !t["canvas"].is_null())?;
    go_to(gui, "Stopped one")?;
    gui.wait_tree("the session's ☑ PAR-001 badge", 10, |t| tree_box(t, "Stopped one")["badge"] == "PAR-001")?;
    // The badge is drawn from 30 % up: at 100 %, the box in the middle (earlier rows may have zoomed out).
    gui.canvas_focus()?;
    for _ in 0..12 {
        if gui.tree()?["canvas"]["scale"].as_f64().unwrap_or(0.0) >= 0.99 {
            break;
        }
        gui.send("+")?;
        std::thread::sleep(Duration::from_millis(120));
    }
    gui.send("c")?;
    let gold = wait_for("the ☑ chip in gold with the proposal", 10, || chip(gui, "g-parity", "☑").filter(|(_, attention, _)| *attention))?;
    let at = wait_for("the badge's click target", 10, || hit(gui, "OpenTodo(\"PAR-001\")"))?;
    gui.click(at, &[])?;
    gui.wait("the claimed card's details in Tasks", 10, |t| tasks(t) && t.contains("PAR-001 · feature") && t.contains("Stopped one"))?;
    // Declined (you, through the CLI): the chip is no longer gold; it opens the group's board.
    cli(sb, "", &["todo", "decline", "PAR-004", "--reason", "not now"])?;
    gui.canvas_focus()?;
    gui.chord('1', &["ctrl"])?;
    let (label, _, at) = visible_chip(gui, "g-parity", "☑", |label, attention| !attention && !label.contains('◇'))?;
    gui.click(at, &[])?;
    gui.wait("the group's board in Tasks", 10, |t| tasks(t) && t.contains("Parity board · PAR"))?;
    gui.canvas_focus()?;
    gui.chord('1', &["ctrl"])?;
    let (_, _, at) = visible_chip(gui, "g-parity", "✎", |_, _| true)?;
    gui.click(at, &[])?;
    gui.wait("the group's folder in the Notebook", 10, |t| notebook(t) && t.contains("◫ Parity group"))?;
    anyhow::ensure!(launches(sb) == before, "a chip or badge started a session");
    Ok(format!("☑ PAR-001 badge → its card; chip {:?} (gold) → after the decline {label:?} → the board; ✎ Notes → the folder", gold.0))
}

fn spaces_check(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    let before = launches(sb);
    let stopped = json!([sb.sessions.stopped]);
    gui.canvas_focus()?;
    gui.chord('1', &["ctrl"])?;
    gui.wait_tree("the canvas", 10, |t| !t["canvas"].is_null())?;
    let tabs = gui.tree()?["hits"].as_array().map(|h| h.iter().filter(|h| h["hit"].as_str().is_some_and(|s| s.contains("Toolbar(Space("))).count()).unwrap_or(0);
    anyhow::ensure!(tabs == 3, "three toolbar tabs, found {tabs}");
    let at = hit(gui, "Space(Notebook)").context("the ✎ tab")?;
    gui.click(at, &[])?;
    gui.wait("the Notebook (its tab)", 10, notebook)?;
    gui.chord('3', &["ctrl"])?;
    gui.wait("Tasks (Ctrl+3)", 10, tasks)?;
    gui.chord('2', &["ctrl"])?;
    gui.wait("the Notebook (Ctrl+2)", 10, notebook)?;
    gui.chord('1', &["ctrl"])?;
    gui.wait_tree("the canvas (Ctrl+1)", 10, |t| !t["canvas"].is_null())?;
    // Go to: a note, a todo, a session, a group.
    go_to(gui, "Parity summary")?;
    gui.wait("the note", 10, |t| notebook(t) && t.contains("summary.md"))?;
    go_to(gui, "PAR-002")?;
    gui.wait("the todo", 10, |t| tasks(t) && t.contains("PAR-002 · bug"))?;
    go_to(gui, "Stopped one")?;
    gui.wait_tree("the session viewed on the canvas", 10, |t| !t["canvas"].is_null() && t["selection"] == stopped)?;
    go_to(gui, "Parity group")?;
    let selected = |t: &Value| t["canvas"]["groups"].as_array().is_some_and(|g| g.iter().any(|g| g["id"] == "g-parity" && g["selected"] == true));
    gui.wait_tree("the group selected", 10, selected)?;
    // Back and forward (a session that runs has its terminal take the keyboard: back to the canvas's first).
    gui.key("Left", &["alt"])?;
    gui.wait_tree("back to the session (Alt+←)", 10, |t| !t["canvas"].is_null() && t["selection"] == stopped && !selected(t))?;
    gui.canvas_focus()?;
    gui.key("Left", &["alt"])?;
    gui.wait("back to the todo", 10, |t| tasks(t) && t.contains("PAR-002 · bug"))?;
    gui.chord(',', &["alt"])?;
    gui.wait("back to the note (Alt+,)", 10, |t| notebook(t) && t.contains("summary.md"))?;
    gui.chord('.', &["alt"])?;
    gui.wait("forward to the todo (Alt+.)", 10, |t| tasks(t) && t.contains("PAR-002 · bug"))?;
    gui.key("Right", &["alt"])?;
    gui.wait_tree("forward to the session (Alt+→)", 10, |t| !t["canvas"].is_null() && t["selection"] == stopped)?;
    std::thread::sleep(Duration::from_millis(500));
    anyhow::ensure!(launches(sb) == before, "a jump started a session");
    Ok("3 toolbar tabs; ✎ tab, Ctrl+3, Ctrl+2, Ctrl+1; Go to a note, a todo, a session (viewed, not started), a group; Alt+← ×2, Alt+, back, Alt+. and Alt+→ forward".into())
}

/// A 4 × 4 green PNG.
fn write_png(path: &Path) -> Result<()> {
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(std::fs::File::create(path)?), 4, 4);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&[0, 200, 0, 255].repeat(16))?;
    Ok(())
}

/// The page shown has loaded in `view` (plain or scripts) from the notes origin.
fn page_loaded(s: &Value, view: &str) -> bool {
    let index = if view == "plain" { 0 } else { 1 };
    let id = s["pane"]["id"].as_str().unwrap_or_default();
    pane_file(s).ends_with("page.html") && s["view"] == view && s["visible"] == true && s["views"][index]["shown"] == id && s["views"][index]["loading"] == false
}

/// An HTML note's page comes from the notes origin: WebView2 serves
/// `https://notes.canvas.invalid/` itself; WebKit (Linux, macOS) lets no
/// handler serve https, so its views load the same host through the
/// `canvas-notes:` scheme.
fn notes_page(href: &str) -> bool {
    let href = href.to_ascii_lowercase();
    ["https://notes.canvas.invalid/", "canvas-notes://notes.canvas.invalid/"].iter().any(|origin| href.starts_with(origin))
}

const PAGE_SEEN: &str = "JSON.stringify({title: document.title, href: location.href, color: getComputedStyle(document.getElementById('h')).color, image: document.getElementById('i').naturalWidth, scripts: document.scripts.length})";

fn html_note_check(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    // Anything the page tried to fetch would reach this listener.
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let dir = sb.group_folder.join("human-outputs");
    std::fs::write(dir.join("style.css"), "h1 { color: #ff00ff; }\n")?;
    write_png(&dir.join("pixel.png"))?;
    std::fs::write(
        dir.join("page.html"),
        format!(
            r##"<!DOCTYPE html><html><head><title>Parity page</title><link rel="stylesheet" href="style.css"></head>
<body><h1 id="h">Parity page</h1><img id="i" src="pixel.png" alt="pixel"><p><a id="n" href="#note:summary">the summary</a> and <a id="t" href="#todo:PAR-001">PAR-001</a></p>
<script>document.title = 'SCRIPT-RAN'; fetch('http://127.0.0.1:{port}/fetch').catch(function () {{}});</script></body></html>"##
        ),
    )?;
    gui.canvas_focus()?;
    gui.chord('2', &["ctrl"])?;
    gui.wait("the Notebook", 10, notebook)?;
    // The explorer lists the folder again (⟳); the note opens Rendered, from the notes origin.
    let at = hit(gui, "Notebook(Refresh)").context("the explorer's ⟳")?;
    gui.click(at, &[])?;
    gui.wait("the page in the explorer", 15, |t| t.contains("page.html"))?;
    click_side(gui, "page.html")?;
    web(gui, "the page in the plain view", 30, |s| page_loaded(s, "plain"))?;
    let seen = eval(gui, PAGE_SEEN)?;
    anyhow::ensure!(seen["href"].as_str().is_some_and(|h| notes_page(h) && h.ends_with("page.html")), "served from the notes origin: {seen}");
    anyhow::ensure!(seen["color"] == "rgb(255, 0, 255)" && seen["image"] == 4, "its relative style and image: {seen}");
    anyhow::ensure!(seen["scripts"] == 0 && seen["title"] == "Parity page", "scripts are off by default: {seen}");
    let state = web(gui, "the files served", 10, |s| {
        let served = s["served_request_uris"].to_string();
        served.contains("style.css") && served.contains("pixel.png")
    })?;
    // Allowed for this file: the scripts view (sandboxed, offline) runs it.
    click_main(gui, "Scripts: off")?;
    wait_doc(&sb.canvas, "the file allowed to run scripts", |d| d.notebook_scripts.iter().any(|p| p.ends_with("page.html")))?;
    web(gui, "the page in the scripts view", 30, |s| page_loaded(s, "scripts") && s["pane"]["file"]["scripts"] == true)?;
    let ran = wait_for("its script to run", 10, || eval(gui, PAGE_SEEN).ok().filter(|v| v["title"] == "SCRIPT-RAN"))?;
    anyhow::ensure!(ran["color"] == "rgb(255, 0, 255)" && ran["image"] == 4, "the scripts view serves the same files: {ran}");
    click_main(gui, "Scripts: on")?;
    wait_doc(&sb.canvas, "scripts off again", |d| d.notebook_scripts.is_empty())?;
    web(gui, "the plain view again", 30, |s| page_loaded(s, "plain") && s["pane"]["file"]["scripts"] == false)?;
    let seen = eval(gui, PAGE_SEEN)?;
    anyhow::ensure!(seen["title"] == "Parity page", "scripts off again: {seen}");
    // #todo: and #note: links navigate the app.
    let links = state["app_links"].as_u64().unwrap_or(0);
    eval(gui, "document.getElementById('t').click(); 'clicked'")?;
    gui.wait("#todo:PAR-001 → its card in Tasks", 10, |t| tasks(t) && t.contains("PAR-001 · feature"))?;
    gui.key("Left", &["alt"])?;
    web(gui, "back to the page", 30, |s| page_loaded(s, "plain"))?;
    eval(gui, "document.getElementById('n').click(); 'clicked'")?;
    gui.wait("#note:summary → the note", 10, |t| notebook(t) && main_text(gui).contains("› summary.md"))?;
    let state = web(gui, "the app links counted", 10, |s| s["app_links"].as_u64().unwrap_or(0) >= links + 2)?;
    // Print / PDF: WebView2's print dialog for the page once it has loaded.
    gui.key("Left", &["alt"])?;
    web(gui, "back to the page", 30, |s| page_loaded(s, "plain"))?;
    gui.req(json!({ "cmd": "webview", "print": true }))?;
    web(gui, "the print dialog shown", 10, |s| s["print_pending"] == false)?;
    let mut printed = "print dialog without error".to_owned();
    if cfg!(target_os = "macos") {
        // macOS saves the page as a PDF in Downloads (a GUI under test: the sandbox), never over a file.
        let tree = gui.wait_tree("the PDF saved", 15, |t| t["notice"]["text"].as_str().is_some_and(|n| n.contains("PDF")))?;
        let notice = tree["notice"].clone();
        anyhow::ensure!(notice["error"] != true, "printing failed: {notice}");
        let pdf = std::fs::read(sb.root.join("Parity page.pdf")).unwrap_or_default();
        anyhow::ensure!(pdf.starts_with(b"%PDF"), "no PDF at {}: {notice}", sb.root.join("Parity page.pdf").display());
        printed = format!("saved Parity page.pdf in Downloads ({} KB)", pdf.len() / 1024);
    }
    std::thread::sleep(Duration::from_millis(800));
    let notice = gui.tree()?["notice"].clone();
    anyhow::ensure!(notice["error"] != true, "printing failed: {notice}");
    // Linux has no print dialog for its offscreen view: the page is saved as a PDF (under the sandbox's home).
    if cfg!(target_os = "linux") {
        let state = web(gui, "the page saved as a PDF", 30, |s| s["printed"].is_string()).map_err(|e| e.context(format!("notice: {}", gui.tree().map(|t| t["notice"].clone()).unwrap_or_default())))?;
        let path = state["printed"].as_str().unwrap_or_default().to_owned();
        let pdf = std::fs::read(&path).unwrap_or_default();
        anyhow::ensure!(pdf.starts_with(b"%PDF"), "{path}: {} bytes, not a PDF", pdf.len());
        printed = format!("saved as a PDF ({} KB)", pdf.len() / 1024);
    }
    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok((_, from)) => bail!("a note's page reached the network ({from})"),
        Err(e) => bail!("listener: {e}"),
    }
    gui.chord('1', &["ctrl"])?;
    web(gui, "the view hidden on the canvas", 10, |s| s["visible"] == false)?;
    Ok(format!(
        "page.html from the notes origin with its style.css and pixel.png; scripts off (0 kept), on for the file (it ran, sandboxed), off again; #todo: and #note: links → {} app links; {printed}; 0 connections to the listener",
        state["app_links"]
    ))
}
