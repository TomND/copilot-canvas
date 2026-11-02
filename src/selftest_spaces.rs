//! The self-test's steps for the spaces (docs/NOTEBOOK-TASKS.md), through
//! the same [`Ui`] as every step (the TUI and the GUI alike): switching
//! between the canvas, the Notebook and Tasks (toolbar tabs, Ctrl+1/2/3),
//! the navigation history (Alt+, / Alt+.), Go to (Ctrl+P), the Notebook
//! (quick capture, typing and saving, the explorer, the Human | All | AI
//! filter, Source / Live / Rendered), Tasks (the demo boards: accepting a
//! proposal, dragging a card, details), the canvas's ✎ / ☑ chips and ☑
//! badges; on a canvas with an id, todos written through the CLI and the
//! sandbox's service. Nothing starts a session.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::{CANVAS, ESC_KEY, PR_OWNER_ID, SIDEBAR, Ui, ctrl_digit, step};
use crate::paths::Instance;

const SECS: Duration = Duration::from_secs(10);

fn launches(instance: &Instance) -> usize {
    std::fs::read_to_string(instance.log_path()).unwrap_or_default().matches("launched ").count()
}

fn on_canvas(t: &str) -> bool {
    t.contains("SESSIONS") && !t.contains("☑ TASKS") && !t.contains("✎ EXPLORER")
}

fn in_notebook(t: &str) -> bool {
    t.contains("✎ EXPLORER")
}

fn in_tasks(t: &str) -> bool {
    t.contains("☑ TASKS")
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
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The cell of `needle` in screen row `y`, starting within columns `[from, to)`.
fn in_row<U: Ui>(ui: &U, y: u16, needle: &str, from: u16, to: u16) -> Option<(u16, u16)> {
    let text = ui.text();
    let line = text.lines().nth(y as usize)?;
    line.match_indices(needle).find_map(|(byte, _)| {
        let x = crate::client::paint::text_width(&line[..byte]) as u16;
        (x >= from && x < to).then_some((x, y))
    })
}

/// A toolbar tab (its glyph in the top row).
fn tab<U: Ui>(ui: &U, glyph: &str) -> Result<(u16, u16)> {
    in_row(ui, 0, glyph, 0, u16::MAX).with_context(|| format!("the toolbar's {glyph} tab\n{}", ui.text()))
}

/// A chip on the title row of the canvas group whose title starts with
/// `title` (`glyph`, right of the title).
fn title_chip<U: Ui>(ui: &U, title: &str, glyph: &str) -> Result<(u16, u16)> {
    let (x, y) = ui.locate(&format!("▣ {title}"), CANVAS)?;
    in_row(ui, y, glyph, x + 2, CANVAS.1).with_context(|| format!("{glyph} on the title row of {title:?}\n{}", ui.text()))
}

/// The title row (on the canvas) of the group whose title starts with `title`.
fn title_row(text: &str, title: &str) -> String {
    columns(text, CANVAS).lines().find(|l| l.contains(&format!("▣ {title}"))).unwrap_or_default().to_owned()
}

/// Ctrl+P, a query, Enter: Go to goes to its best match.
fn go_to<U: Ui>(ui: &mut U, query: &str) -> Result<()> {
    ui.send(b"\x10")?;
    ui.wait("Go to", SECS, |t| t.contains("Go to ─"))?;
    ui.send(query.as_bytes())?;
    ui.wait(&format!("{query:?} typed into Go to"), SECS, |t| t.contains(query))?;
    ui.send(b"\r")?;
    ui.wait("Go to closed", SECS, |t| !t.contains("Go to ─"))
}

/// The notes of a folder.
fn notes(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "md")).collect()
}

/// The demo canvas's steps (they start and end with a terminal that has the keyboard).
pub(super) fn steps<U: Ui>(ui: &mut U, instance: &Instance, canvas: &Path, client: &str) -> Result<()> {
    switch_steps(ui)?;
    let note = notebook_steps(ui, instance, client)?;
    filter_steps(ui, canvas)?;
    goto_steps(ui, instance, &note)?;
    tasks_steps(ui, instance)?;
    ui.send(&ctrl_digit(1))?;
    ui.wait("the canvas", SECS, on_canvas)?;
    super::back_to_terminal(ui)
}

fn switch_steps<U: Ui>(ui: &mut U) -> Result<()> {
    step("spaces: the toolbar's tabs and Ctrl+1/2/3 switch between the canvas, the Notebook and Tasks; Alt+, / Alt+. go back and forward");
    ui.send(b"\x1d")?;
    ui.wait("canvas focus", SECS, |t| on_canvas(t) && !t.contains("Typing goes to the terminal"))?;
    let (x, y) = tab(ui, "✎")?;
    ui.click(x, y)?;
    ui.wait("the Notebook (its toolbar tab)", SECS, |t| in_notebook(t) && t.contains("Human │ All │ AI"))?;
    ui.send(&ctrl_digit(3))?;
    ui.wait("Tasks (Ctrl+3)", SECS, |t| in_tasks(t) && t.contains("NEEDS YOU"))?;
    ui.send(&ctrl_digit(1))?;
    ui.wait("the canvas (Ctrl+1)", SECS, on_canvas)?;
    ui.send(b"\x1b,")?;
    ui.wait("back to Tasks (Alt+,)", SECS, in_tasks)?;
    ui.send(b"\x1b,")?;
    ui.wait("back to the Notebook (Alt+,)", SECS, in_notebook)?;
    ui.send(b"\x1b.")?;
    ui.wait("forward to Tasks (Alt+.)", SECS, in_tasks)?;
    ui.send(&ctrl_digit(2))?;
    ui.wait("the Notebook (Ctrl+2)", SECS, in_notebook)?;
    let (x, y) = tab(ui, "◫")?;
    ui.click(x, y)?;
    ui.wait("the canvas (its toolbar tab)", SECS, on_canvas)
}

/// Alt+N, typing, Ctrl+S, the explorer, Live, Source, Rendered. Returns the note.
fn notebook_steps<U: Ui>(ui: &mut U, instance: &Instance, client: &str) -> Result<PathBuf> {
    step("notebook: Alt+N writes a quick note into the Inbox and opens it in Source mode; typed text is saved (Ctrl+S) and the explorer lists it; Live hides the Markdown syntax, Ctrl+E renders it");
    let inbox = instance.home.join("notebook").join("Inbox");
    let before = notes(&inbox);
    ui.send(b"\x1bn")?;
    ui.wait("the quick note in Source mode", SECS, |t| in_notebook(t) && t.contains("audience: human") && t.contains("Rendered │ Source │ Live"))?;
    let note = wait_for("the quick note's file in the Inbox", 10, || notes(&inbox).into_iter().find(|p| !before.contains(p)))?;
    let name = note.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    ui.wait("the quick note in the explorer", SECS, |t| t.contains("▾ Inbox") && t.contains(&name))?;
    ui.locate(&name, SIDEBAR)?;
    ui.send(b"# Quick selftest note\r")?;
    ui.send(b"Typed in the notebook")?;
    ui.send(b"\x13")?;
    wait_for("the typed text saved", 10, || std::fs::read_to_string(&note).ok().filter(|t| t.contains("# Quick selftest note") && t.contains("Typed in the notebook")))?;
    ui.wait("the text in the editor", SECS, |t| t.contains("# Quick selftest note") && t.contains("Typed in the notebook"))?;
    // Live: lines without the caret show formatted (the heading without its #).
    let (x, y) = ui.locate("│ Live", CANVAS)?;
    ui.click(x + 3, y)?;
    ui.wait("Live: the heading without its #", SECS, |t| {
        let main = columns(t, CANVAS);
        main.contains("Quick selftest note") && !main.contains("# Quick selftest note") && main.contains("Typed in the notebook")
    })?;
    let (x, y) = ui.locate("│ Source", CANVAS)?;
    ui.click(x + 3, y)?;
    ui.wait("Source: the heading's # again", SECS, |t| columns(t, CANVAS).contains("# Quick selftest note"))?;
    // Ctrl+E: Rendered (the TUI formats it in cells; the GUI shows its HTML pane there).
    ui.send(b"\x05")?;
    ui.wait("Rendered", SECS, |t| {
        let main = columns(t, CANVAS);
        !main.contains("# Quick selftest note") && !main.contains("audience: human") && (client != "tui" || main.contains("Typed in the notebook"))
    })?;
    Ok(note)
}

/// The screen's columns `[from, to)`, row by row.
fn columns(text: &str, (from, to): (u16, u16)) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let mut x = 0u16;
        for c in line.chars() {
            if x >= from && x < to {
                out.push(c);
            }
            x += crate::client::paint::text_width(&c.to_string()) as u16;
        }
        out.push('\n');
    }
    out
}

/// The group's ✎ chip, the explorer and the Human | All | AI filter.
fn filter_steps<U: Ui>(ui: &mut U, canvas: &Path) -> Result<()> {
    step("notebook: Go to flies to a group; its ✎ Notes chip opens its folder in the explorer; the Human filter hides an agent's ai-docs note (\"N AI hidden\"), All shows it again");
    let doc = super::load(canvas)?;
    let folder = doc.groups.iter().find(|g| g.name == "Selftest group").and_then(|g| g.folder.clone()).context("the group's folder")?;
    // An agent's note for other agents (classified by its folder).
    std::fs::write(Path::new(&folder).join("ai-docs").join("selftest-plan.md"), "# Selftest plan\n\nContext for the next session.\n")?;
    ui.send(&ctrl_digit(1))?;
    ui.wait("the canvas", SECS, on_canvas)?;
    go_to(ui, "Selftest group")?;
    ui.wait("the canvas at the group", SECS, |t| on_canvas(t) && t.contains("▣ Selftest"))?;
    let (x, y) = title_chip(ui, "Selftest", "✎")?;
    ui.click(x, y)?;
    ui.wait("its folder in the explorer", SECS, |t| in_notebook(t) && t.contains("ai-docs") && t.contains("human-outputs"))?;
    let (x, y) = ui.locate("ai-docs", SIDEBAR)?;
    ui.click(x, y)?;
    ui.wait("the agent's note, marked ◇", SECS, |t| t.contains("◇ selftest-plan.md"))?;
    let (x, y) = ui.locate(" Human ", SIDEBAR)?;
    ui.click(x + 1, y)?;
    ui.wait("Human: the agent's note hidden", SECS, |t| t.contains("AI hidden") && !t.contains("selftest-plan.md"))?;
    let (x, y) = ui.locate(" All ", SIDEBAR)?;
    ui.click(x + 1, y)?;
    ui.wait("All: shown again", SECS, |t| t.contains("◇ selftest-plan.md") && !t.contains("AI hidden"))
}

/// Go to a note, a todo and a session; back and forward between them.
fn goto_steps<U: Ui>(ui: &mut U, instance: &Instance, note: &Path) -> Result<()> {
    step("Go to (Ctrl+P) jumps to a note, a todo and a session without starting anything; Alt+, / Alt+. walk back and forward through them");
    let before = launches(instance);
    let name = note.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    go_to(ui, "Quick selftest")?;
    ui.wait("the note in the Notebook", SECS, |t| in_notebook(t) && t.contains(&name))?;
    go_to(ui, "WI-005")?;
    ui.wait("the todo's details in Tasks", SECS, |t| in_tasks(t) && t.contains("WI-005 · bug") && t.contains("Flaky reconnect test"))?;
    go_to(ui, "Spawned tracker")?;
    ui.wait("the session's details on the canvas", SECS, |t| on_canvas(t) && t.contains("Spawned tracker"))?;
    ui.send(b"\x1b,")?;
    ui.wait("back to the todo (Alt+,)", SECS, |t| in_tasks(t) && t.contains("WI-005 · bug"))?;
    ui.send(b"\x1b,")?;
    ui.wait("back to the note", SECS, |t| in_notebook(t) && t.contains(&name))?;
    ui.send(b"\x1b.")?;
    ui.wait("forward to the todo (Alt+.)", SECS, |t| in_tasks(t) && t.contains("WI-005 · bug"))?;
    std::thread::sleep(Duration::from_millis(500));
    anyhow::ensure!(launches(instance) == before, "Go to or the history started a session");
    Ok(())
}

/// The demo book: a box's ☑ badge, the group's ☑ chip, accepting a
/// proposal, dragging a card to another column, details.
fn tasks_steps<U: Ui>(ui: &mut U, instance: &Instance) -> Result<()> {
    step("tasks: a box's ☑ WI-002 badge opens its card; the group's ☑ chip opens its board; A accepts a proposal; a card dragged to another column changes its status; Enter opens a card's details");
    let before = launches(instance);
    ui.send(&ctrl_digit(1))?;
    ui.wait("the canvas", SECS, on_canvas)?;
    super::back_to_terminal(ui)?;
    super::canvas_at(ui, 100)?;
    let (x, y) = ui.locate("☑ WI-002", CANVAS)?;
    ui.click(x + 2, y)?;
    ui.wait("the claimed todo's card and details", SECS, |t| in_tasks(t) && t.contains("WI-002 · feature") && t.contains("Gentle momentum when panning"))?;
    ui.send(&ctrl_digit(1))?;
    ui.wait("the canvas", SECS, on_canvas)?;
    super::zoom_to(ui, 50)?;
    let (x, y) = title_chip(ui, "Canvas", "☑")?;
    ui.click(x, y)?;
    ui.wait("the group's board", SECS, |t| in_tasks(t) && t.contains("Canvas App · WI") && t.contains("WI-006"))?;
    let (x, y) = ui.locate("WI-006", CANVAS)?;
    ui.click(x, y)?;
    ui.wait("the proposal's details", SECS, |t| t.contains("WI-006 · improvement"))?;
    ui.send(b"a")?;
    ui.wait("the proposal accepted", SECS, |t| t.contains("Accepted WI-006") && !t.contains("◇ WI-006"))?;
    // WI-007 (Backlog) dragged into the Todo column.
    let (cx, cy) = ui.locate("WI-007", CANVAS)?;
    let (tx, ty) = ui.find_in("│Todo ", CANVAS.0, CANVAS.1).or_else(|| ui.find_in("│ Todo ", CANVAS.0, CANVAS.1)).with_context(|| format!("the Todo column\n{}", ui.text()))?;
    ui.drag(0, (cx, cy), (tx + 4, ty + 3))?;
    ui.wait("WI-007 in Todo", SECS, |t| t.contains("WI-007 ·") && t.contains("Status   Todo ▾"))?;
    // Esc clears the selection; ↓ selects the first card in view, Enter shows its details.
    ui.send(ESC_KEY)?;
    ui.wait("no details", SECS, |t| !t.contains("Status   "))?;
    ui.send(b"\x1b[B")?;
    ui.send(b"\r")?;
    ui.wait("a card's details (Enter)", SECS, |t| t.contains("Status   ") && t.contains("Priority "))?;
    anyhow::ensure!(launches(instance) == before, "Tasks started a session");
    Ok(())
}

/// On a canvas with an id: todos through the CLI and the sandbox service.
pub(super) fn todo_cli_steps<U: Ui>(ui: &mut U, instance: &Instance) -> Result<()> {
    step("todos through the CLI (todo board add, board link, add; a session's todo claim) reach the windows through the service: the group's ☑ chip counts them and opens the board; the session's ☑ badge opens the card; nothing starts");
    let home = &instance.home;
    let before = launches(instance);
    super::canvas_cli(home, &["todo", "board", "add", "Selftest board", "--prefix", "STB"])?;
    super::canvas_cli(home, &["todo", "board", "link", "STB", "--group", "Team"])?;
    let out = super::canvas_cli(home, &["todo", "add", "Write the selftest", "--type", "chore", "--board", "STB", "--status", "todo"])?;
    anyhow::ensure!(out.contains("STB-001"), "todo add: {out}");
    // The PR owner's session claims it (as an agent would, from its terminal).
    let claim = std::process::Command::new(std::env::current_exe()?).args(["todo", "claim", "STB-001", "--home", &home.display().to_string()]).env("COPILOT_CANVAS_SESSION", PR_OWNER_ID).output()?;
    anyhow::ensure!(claim.status.success(), "todo claim: {}{}", String::from_utf8_lossy(&claim.stdout), String::from_utf8_lossy(&claim.stderr));
    ui.send(&ctrl_digit(1))?;
    ui.wait("the canvas", SECS, on_canvas)?;
    go_to(ui, "Team")?;
    ui.wait("the group's ☑ chip counting the todo", SECS, |t| {
        let row = title_row(t, "Team");
        row.contains("☑ 1") || row.contains("☑1")
    })?;
    let (x, y) = title_chip(ui, "Team", "☑")?;
    ui.click(x, y)?;
    ui.wait("the board from the CLI on the todo canvas", SECS, |t| in_tasks(t) && t.contains("Selftest board · STB") && t.contains("STB-001"))?;
    ui.send(&ctrl_digit(1))?;
    ui.wait("the canvas", SECS, on_canvas)?;
    go_to(ui, "PR owner")?;
    ui.wait("the PR owner's details", SECS, |t| on_canvas(t) && t.contains("PR owner"))?;
    super::zoom_to(ui, 100)?;
    ui.send(b"c")?;
    let (x, y) = wait_for("the PR owner's ☑ STB-001 badge", 10, || ui.find_in("☑ STB-001", CANVAS.0, CANVAS.1))?;
    ui.click(x + 2, y)?;
    ui.wait("the claimed card's details", SECS, |t| in_tasks(t) && t.contains("STB-001 · chore") && t.contains("Write the selftest") && t.contains("PR owner"))?;
    ui.send(&ctrl_digit(1))?;
    ui.wait("the canvas", SECS, on_canvas)?;
    anyhow::ensure!(launches(instance) == before, "the todo steps started a session");
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn win32_input_mode_presses_become_the_gui_drivers_keys() {
        // Ctrl+2 and Esc pressed; a release (Kd 0) is no key.
        let bytes = [super::ctrl_digit(2), super::ESC_KEY.to_vec(), b"\x1b[50;3;50;0;8;1_".to_vec()].concat();
        assert_eq!(super::super::vt_events(&bytes), [json!({ "char": "2", "mods": ["ctrl"] }), json!({ "key": "Esc", "mods": [] })]);
    }
}
