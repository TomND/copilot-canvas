//! The parity suite's check of the ✦ Skills view and group skill folders
//! (row 52) on the sandbox canvas: a project skill where the parity sessions
//! run (`.claude/skills`, read by all three agents) and a group's skill
//! folder, added with the group menu's *Skill folders…* (typed, checked on
//! the service's machine), both listed with the agents that load them; the
//! *Agent* row shows one agent at a time (the lower sections, PERSONAL and
//! CANVAS LIBRARY, are checked there: with every agent the owner's own
//! personal skills can push them below the first screen); viewing starts
//! nothing. The row takes the folder off again and removes what it wrote.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::{Gui, Row, Sandbox, check, group_menu};
use crate::selftest::{SIDEBAR, wait_doc};

/// Short names: a skill's row keeps its agents' tags in a narrow sidebar.
const PROJECT_SKILL: &str = "pa-proj";
const GROUP_SKILL: &str = "pa-grp";

fn launches(sb: &Sandbox) -> usize {
    std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default().matches("launched ").count()
}

/// `dir/<name>/SKILL.md` with front matter.
fn skill(dir: &Path, name: &str, description: &str) -> Result<()> {
    std::fs::create_dir_all(dir.join(name))?;
    std::fs::write(dir.join(name).join("SKILL.md"), format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\n"))?;
    Ok(())
}

/// The parity group's skill folders as the canvas file has them.
fn group_skill_dirs(doc: &crate::client::doc::Doc) -> Vec<String> {
    doc.group("g-parity").and_then(|g| g.defaults.as_ref()).map(|d| d.skill_dirs.clone()).unwrap_or_default()
}

/// A screen line's part from `needle` on, when the needle starts in the
/// sidebar's columns, up to the sidebar's border.
fn in_sidebar<'a>(line: &'a str, needle: &str) -> Option<&'a str> {
    let at = line.find(needle)?;
    if line[..at].chars().count() >= SIDEBAR.1 as usize {
        return None;
    }
    line[at + needle.len()..].split('│').next()
}

/// A screen line up to the sidebar's border.
fn sidebar(line: &str) -> &str {
    line.split('│').next().unwrap_or(line)
}

/// The agents' tags of a skill's row in the sidebar (`all`, `CP OC`, …).
fn tags(screen: &str, name: &str) -> Option<String> {
    let needle = format!("✦ {name} ");
    screen.lines().find_map(|line| in_sidebar(line, &needle)).map(|row| row.trim().to_owned())
}

/// The sidebar's *Agent* row's label (`All agents`, `GitHub Copilot`, …).
fn agent_label(screen: &str) -> Option<String> {
    let row = screen.lines().find_map(|line| in_sidebar(line, "Agent: "))?;
    Some(row.split("  ").next()?.trim().to_owned())
}

pub(super) fn rows(sb: &Sandbox, gui: &mut Gui, rows: &mut Vec<Row>) {
    check(
        rows,
        "52",
        "New: the ✦ Skills view lists every skill a session can get (canvas plugin, group skill folders, projects, personal, canvas library) with the agents that load each; group skill folders (group menu › Skill folders…); viewing starts nothing",
        "skills_view_projects_and_group_folders",
        || skills(sb, gui),
    );
}

fn skills(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    let project = sb.work.join(".claude").join("skills");
    skill(&project, PROJECT_SKILL, "Parity: a project's own skill")?;
    let folder = sb.root.join("parity-skills");
    skill(&folder, GROUP_SKILL, "Parity: a group's skill")?;
    let result = view_and_folders(sb, gui, &folder);
    // Off again (Del in the editor), so later rows see the parity group as before.
    let cleanup = (|| -> Result<()> {
        if group_skill_dirs(&sb.doc()?).is_empty() {
            return Ok(());
        }
        group_menu(gui, "Skill folders…")?;
        gui.wait("the skill folders editor", 5, |t| t.contains("Skill folders · “Parity group”"))?;
        gui.key("Delete", &[])?;
        wait_doc(&sb.canvas, "the folder taken off", |d| group_skill_dirs(d).is_empty())?;
        gui.key("Esc", &[])?;
        gui.canvas_focus()
    })();
    // Only what the row wrote (the work folder's .claude only when it is left empty).
    let _ = std::fs::remove_dir_all(project.join(PROJECT_SKILL));
    let _ = std::fs::remove_dir(&project);
    let _ = std::fs::remove_dir(sb.work.join(".claude"));
    let _ = std::fs::remove_dir_all(&folder);
    let text = result?;
    cleanup?;
    Ok(text)
}

fn view_and_folders(sb: &Sandbox, gui: &mut Gui, folder: &Path) -> Result<String> {
    let before = launches(sb);
    // The group menu's Skill folders…: a folder that is not there is refused, this one added.
    group_menu(gui, "Skill folders…")?;
    gui.wait("the skill folders editor", 5, |t| t.contains("Skill folders · “Parity group”") && t.contains("Add a folder"))?;
    gui.send(&sb.root.join("nowhere").display().to_string())?;
    gui.send("\r")?;
    // The reason comes first: a long path is cut off at the editor's edge.
    gui.wait("a missing folder refused", 10, |t| t.contains("does not exist on the service's machine"))?;
    anyhow::ensure!(group_skill_dirs(&sb.doc()?).is_empty(), "a missing folder was added");
    gui.send("\x15")?;
    gui.send(&folder.display().to_string())?;
    gui.send("\r")?;
    let doc = wait_doc(&sb.canvas, "the group's skill folder", |d| group_skill_dirs(d).len() == 1)?;
    anyhow::ensure!(crate::client::doc::same_dir(&group_skill_dirs(&doc)[0], &folder.display().to_string()), "the folder as typed: {:?}", group_skill_dirs(&doc));
    gui.wait("its skill counted", 10, |t| t.contains("· 1 skill") && !t.contains("Checking on the service's machine"))?;
    gui.key("Esc", &[])?;
    gui.wait("the editor closed", 5, |t| !t.contains("Skill folders · “Parity group”"))?;
    // The Skills view: the top sections with every agent, each skill with the agents that load it.
    gui.chord('5', &["alt"])?;
    gui.wait("the Skills view", 30, |t| {
        ["Agent: All agents", "CANVAS", "canvas-events", "GROUP SKILL FOLDERS", "Parity group", "PROJECTS"].iter().all(|s| t.contains(s))
            && tags(t, GROUP_SKILL).is_some()
            && tags(t, PROJECT_SKILL).is_some()
            && !t.contains("Loading the skill sources")
    })?;
    let screen = gui.screen();
    let (group_tags, project_tags) = (tags(&screen, GROUP_SKILL).unwrap_or_default(), tags(&screen, PROJECT_SKILL).unwrap_or_default());
    anyhow::ensure!(group_tags.ends_with("all") && project_tags.ends_with("all"), "every agent loads both ({group_tags:?}, {project_tags:?}):\n{screen}");
    // One agent at a time (A): each still loads both; PERSONAL and CANVAS LIBRARY show
    // (with one agent the list is short); then every agent again.
    let mut seen = Vec::new();
    let mut lower = false;
    for _ in 0..5 {
        let shown = agent_label(&gui.screen()).unwrap_or_default();
        gui.send("a")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let label = loop {
            let label = agent_label(&gui.screen()).unwrap_or_default();
            if !label.is_empty() && label != shown {
                break label;
            }
            anyhow::ensure!(Instant::now() < deadline, "A did not switch the agent (still {shown:?}):\n{}", gui.screen());
            std::thread::sleep(Duration::from_millis(100));
        };
        if label == "All agents" {
            break;
        }
        gui.wait(&format!("{label}'s skills"), 10, |t| tags(t, GROUP_SKILL).is_some() && tags(t, PROJECT_SKILL).is_some())?;
        let shown_now = gui.screen();
        let list: Vec<&str> = shown_now.lines().map(sidebar).collect();
        lower |= ["PERSONAL", "CANVAS LIBRARY"].iter().all(|s| list.iter().any(|row| row.contains(s)));
        seen.push(label);
    }
    anyhow::ensure!(agent_label(&gui.screen()).as_deref() == Some("All agents"), "back to every agent:\n{}", gui.screen());
    anyhow::ensure!(!seen.is_empty(), "A showed no single agent");
    anyhow::ensure!(lower, "PERSONAL and CANVAS LIBRARY never showed ({seen:?}):\n{}", gui.screen());
    anyhow::ensure!(launches(sb) == before, "viewing the skills started a session");
    gui.chord('1', &["alt"])?;
    gui.canvas_focus()?;
    Ok(format!(
        "{GROUP_SKILL} (group menu › Skill folders…, a missing folder refused) and {PROJECT_SKILL} (.claude/skills where the sessions run) listed for every agent and for {}; nothing launched",
        seen.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_read_from_the_sidebar_only() {
        // As the skills-view snapshot draws them; the details pane repeats a name further right.
        let screen = [
            " ◷   ⇄ Agent: All agents       A │  ╎ │ ■ stopped   ││ ✦ pa-grp  ",
            " ⧉   ✦ pa-grp                all │  ✦ pa-proj  x   ││",
            "     ✦ pa-proj         ≈ CP OC │",
            "    PERSONAL                     │",
        ]
        .join("\n");
        assert_eq!(agent_label(&screen).as_deref(), Some("All agents"));
        assert_eq!(tags(&screen, GROUP_SKILL).as_deref(), Some("all"));
        assert_eq!(tags(&screen, PROJECT_SKILL).as_deref(), Some("≈ CP OC"));
        assert_eq!(tags(&screen, "missing"), None);
        assert_eq!(sidebar(screen.lines().nth(3).unwrap()).trim(), "PERSONAL");
    }
}
