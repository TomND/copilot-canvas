//! Group folders, notes, todos and passive sessions within reach: the chips
//! on a group's title row (passive sessions, notes, todos) at every zoom, the
//! sidebar's folder rows, their menus, and opening the group's files and
//! folders. In remote mode (any `--remote`, loopback included) folders open
//! in VS Code through the server's tunnel or in the canvas's folder browser,
//! and files in VS Code or the read-only viewer: never in this machine's
//! Explorer, which would show the wrong machine.

use crate::app::App;
use crate::dialogs::{Action, MenuItem, Modal};
use crate::doc::{Group, Node};
use crate::tasks::TodoCounts;
use crate::visual::VisualAction;

/// A chip on a group's title row.
#[derive(Clone, Debug, PartialEq)]
pub struct Chip {
    pub label: String,
    pub action: Action,
    /// In the group's colour (else muted: nothing set up yet).
    pub lit: bool,
    /// In gold: something waits for you (todo proposals).
    pub attention: bool,
}

/// A title keeps at least this many cells next to compact chips.
const MIN_TITLE: i32 = 6;

impl Chip {
    pub fn width(&self) -> i32 {
        crate::paint::text_width(&self.label) as i32
    }

    /// How much the chip matters when a narrow title row has to shorten or
    /// drop chips (the least first): proposals waiting, then the notes, the
    /// todos and the passive sessions in use, then the ones that only offer
    /// to set something up.
    pub fn rank(&self) -> u8 {
        match &self.action {
            Action::Tasks(_) if self.attention => 6,
            Action::Notebook(_) if self.lit => 5,
            Action::Tasks(_) if self.lit => 4,
            _ if self.lit => 3,
            Action::Notebook(_) => 2,
            Action::Tasks(_) => 1,
            _ => 0,
        }
    }
}

/// Cells the chips take (each with a gap after it).
pub fn chips_width(chips: &[Chip]) -> i32 {
    chips.iter().map(|c| c.width() + 1).sum()
}

/// A group's todos chip: `☑ 4 · 2▸ · 1◇` (open · in progress · proposed,
/// gold while proposals wait; compact `☑4`), or `☑ +` when no board is linked
/// (it offers to create or link one).
pub fn todo_chip(group: &str, counts: Option<TodoCounts>, compact: bool) -> Chip {
    use crate::tasks::Action as Tasks;
    match counts {
        None => Chip { label: if compact { " ☑+ " } else { " ☑ + " }.into(), action: Action::Tasks(Tasks::LinkBoards(group.to_owned())), lit: false, attention: false },
        Some(c) => {
            let label = if compact {
                format!(" ☑{} ", c.open)
            } else {
                let mut label = format!(" ☑ {}", c.open);
                if c.in_progress > 0 {
                    label.push_str(&format!(" · {}▸", c.in_progress));
                }
                if c.proposed > 0 {
                    label.push_str(&format!(" · {}◇", c.proposed));
                }
                label.push(' ');
                label
            };
            Chip { label, action: Action::Tasks(Tasks::OpenGroup(group.to_owned())), lit: true, attention: c.proposed > 0 }
        }
    }
}

impl App {
    /// The passive sessions of a group (by their own group, wherever their box
    /// is; the canvas-wide agents are not a group's).
    pub fn group_passives(&self, group: &str) -> Vec<&Node> {
        self.doc.nodes.iter().filter(|n| !n.is_canvas_agent() && n.passive.as_ref().is_some_and(|p| p.group == group)).collect()
    }

    /// A group's chips: passive sessions ("⟲ 2 passive", or "⟲ + passive";
    /// both open the group's passive panel), its notes ("✎ Notes" opens the
    /// Notebook at its folder, "✎ Set folder…" before it has one) and its
    /// todos ([`todo_chip`]). `compact`: the short forms for small sizes.
    pub fn group_chips(&self, group: &Group, compact: bool) -> Vec<Chip> {
        if group.is_canvas_agents() {
            return if self.demo { Vec::new() } else { self.frame_chips(compact) };
        }
        let mut chips = Vec::new();
        let chip = |label: &str, action: Action, lit: bool| Chip { label: label.to_owned(), action, lit, attention: false };
        let passives = self.group_passives(&group.id).len();
        if !self.demo && (passives > 0 || !group.is_automations()) {
            let panel = Action::Visual(VisualAction::GroupPassives(group.id.clone()));
            chips.push(match passives {
                0 => chip(if compact { " ⟲+ " } else { " ⟲ + passive " }, panel, false),
                n if compact => chip(&format!(" ⟲ {n} "), panel, true),
                n => chip(&format!(" ⟲ {n} passive "), panel, true),
            });
        }
        if !group.is_automations() {
            use crate::notebook::Action as Notebook;
            if group.folder.is_some() {
                let lit = crate::notebook::group_has_notes(self, &group.id);
                chips.push(chip(if compact { " ✎ " } else { " ✎ Notes " }, Action::Notebook(Notebook::OpenGroup(group.id.clone())), lit));
            } else {
                chips.push(chip(if compact { " ✎+ " } else { " ✎ Set folder… " }, Action::Notebook(Notebook::LinkFolder(group.id.clone())), false));
            }
            chips.push(todo_chip(&group.id, crate::tasks::group_counts(self, &group.id), compact));
        }
        chips
    }

    /// The chips that fit on a title row `row` cells wide whose title takes
    /// `title` cells: full labels when the whole title fits too; else the
    /// least important chips ([`Chip::rank`]) go compact one by one while the
    /// whole title still fits; else, all compact, the chips that only offer
    /// to set something up go (least important first, never the last one)
    /// when that keeps the whole title; else the rest stay next to a shortened title (at least
    /// `MIN_TITLE` cells), dropping the least important first.
    pub fn fit_group_chips(&self, group: &Group, title: i32, row: i32) -> Vec<Chip> {
        let room = row - 4;
        let full = self.group_chips(group, false);
        if title + chips_width(&full) <= room {
            return full;
        }
        let compact = self.group_chips(group, true);
        // Least important first (the leftmost of equals first).
        let mut order: Vec<usize> = (0..full.len()).collect();
        order.sort_by_key(|&i| (full[i].rank(), i));
        if compact.len() == full.len() {
            let mut mixed = full.clone();
            for &i in &order {
                mixed[i] = compact[i].clone();
                if title + chips_width(&mixed) <= room {
                    return mixed;
                }
            }
        }
        let mut kept: Vec<(usize, Chip)> = compact.into_iter().enumerate().collect();
        let width = |kept: &[(usize, Chip)]| kept.iter().map(|(_, c)| c.width() + 1).sum::<i32>();
        // Offers (nothing set up yet) give way to the whole title, but the
        // last chip stays (next to a shortened title, below).
        let offers: Vec<usize> = order.iter().copied().filter(|&i| !full[i].lit && !full[i].attention).collect();
        let mut dropped = kept.clone();
        for i in offers {
            if dropped.len() <= 1 {
                break;
            }
            dropped.retain(|(at, _)| *at != i);
            if title + width(&dropped) <= room {
                return dropped.into_iter().map(|(_, c)| c).collect();
            }
        }
        for i in order {
            if MIN_TITLE + width(&kept) <= room {
                break;
            }
            kept.retain(|(at, _)| *at != i);
        }
        kept.into_iter().map(|(_, c)| c).collect()
    }

    /// Opens the panel of a group's passive sessions (each opens its own
    /// panel from there; none starts).
    pub fn open_group_passives(&mut self, group: &str) {
        if self.doc.group(group).is_some_and(|g| g.is_canvas_agents()) {
            return self.run_agents(crate::canvas_wide::AgentAction::Panel);
        }
        self.right_detail = Some(crate::side::RightDetail::GroupPassives(group.to_owned()));
        self.needs_draw = true;
    }

    /// A menu of the group's folders.
    pub fn group_folders_menu(&mut self, group: &str) {
        let Some(g) = self.doc.group(group).cloned() else { return };
        let set = g.folder.is_some();
        let item = |label: &str, action: Action, enabled: bool| MenuItem { label: label.to_owned(), hint: String::new(), action, enabled };
        let mut items = vec![
            item("Open AI docs (ai-docs)", Action::OpenGroupFolder(group.to_owned(), false), set),
            item("Open human outputs", Action::OpenGroupFolder(group.to_owned(), true), set),
        ];
        if let Some(folder) = &g.folder {
            items.push(item("Open the group folder", Action::OpenPath(folder.clone()), true));
            let goals = std::path::Path::new(folder).join(crate::doc::AI_DOCS).join("GOALS.md");
            items.push(item("Open GOALS.md", Action::Visual(VisualAction::Browse(goals.display().to_string())), true));
        }
        items.push(item(if set { "Change folder…" } else { "Set folder…" }, Action::SetGroupFolder(group.to_owned()), true));
        let title = g.folder.clone().unwrap_or_else(|| format!("“{}” has no folder yet", g.name));
        let (x, y) = self.menu_spot();
        self.open_menu(x, y, (title, items));
    }

    /// Where a menu opened without a click goes: the last click, else the canvas.
    fn menu_spot(&self) -> (u16, u16) {
        match self.last_click {
            Some((_, x, y)) => (x, y),
            None => (self.layout.canvas.x + 2, self.layout.canvas.y + 1),
        }
    }

    /// The service is reached over TCP (remote mode, `--remote loopback`
    /// included): its folders and files open in the canvas.
    pub fn service_is_remote(&self) -> bool {
        self.target.as_ref().is_some_and(|t| t.is_remote())
    }

    /// A file of the service's machine (passive files, GOALS.md, the event
    /// catalog). Locally: VS Code at the file, as before. Remote: VS Code
    /// through the server's tunnel, else the read-only viewer.
    pub fn open_service_file(&mut self, path: &str) {
        if self.service_is_remote() {
            self.open_remote(path, false);
        } else {
            self.open_link(path);
        }
    }

    /// `Browse`: a folder in the browser or a file in the viewer (remote), or
    /// as `open_folder` / `open_service_file` locally.
    pub fn browse_remote(&mut self, path: &str) {
        if self.backend.path_info(path).is_ok_and(|info| info.dir) {
            self.open_folder(path);
        } else {
            self.open_service_file(path);
        }
    }

    /// A passive session's group context (4) or the group's GOALS.md (5).
    pub fn open_passive_file(&mut self, id: &str, which: u8) {
        let Some(paths) = self.doc.node(id).and_then(|n| self.passive_paths(n)) else { return };
        let path = if which == 4 { paths.context } else { paths.goals };
        self.open_service_file(&path.display().to_string());
    }

    /// The sidebar's folder row of a group: its folder menu.
    pub fn side_folder(&mut self, group: &str) {
        self.group_folders_menu(group);
    }

    /// A folder entry picked in the browser: a file opens in the viewer on top.
    pub fn view_service_file(&mut self, path: String) {
        let uri = self.target.as_ref().and_then(|t| t.vscode_tunnel()).map(|tunnel| crate::links::vscode_tunnel_uri(&tunnel, &path, None));
        self.show_viewer(path, false, None, uri);
    }

    /// Loads a folder into the browser it is shown in (after navigating).
    pub fn load_browser(&mut self, path: String) {
        let key = path.clone();
        self.in_background(
            move |backend| list_entries(backend, &path),
            move |loaded| {
                crate::app::AppEvent::Run(Box::new(move |app: &mut App| {
                    for modal in app.modals.iter_mut() {
                        if let Modal::Viewer(viewer) = modal
                            && viewer.path == key
                            && viewer.lines.is_none()
                        {
                            match &loaded {
                                Ok((entries, typed)) => viewer.set_listing(entries.clone(), *typed),
                                Err(error) => viewer.error = Some(format!("Cannot list it: {error}")),
                            }
                        }
                    }
                }))
            },
        );
    }
}

/// A folder's subfolders (`name\`) and files, with `..` first (`true`); a
/// service that only lists subfolders answers those (`false`).
pub fn list_entries(backend: &dyn crate::rpc::Backend, dir: &str) -> Result<(Vec<String>, bool), String> {
    match backend.list_entries(dir) {
        Ok(entries) => Ok((entries, true)),
        Err(error) if error.to_string().contains("does not support") || error.to_string().contains("unexpected") => {
            backend.list_dir(Some(dir.to_owned())).map(|entries| (entries, false)).map_err(|e| format!("{e:#}"))
        }
        Err(error) => Err(format!("{error:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Doc, Passive};

    fn app(doc: Doc, demo: bool) -> App {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let (_service, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, _app_rx) = crossbeam_channel::unbounded();
        App::new(crate::conn::Conn::detached(tx, service_rx), std::sync::Arc::new(crate::rpc::Offline), crate::sync::Opened::detached(doc), demo, ".".into(), app_tx, None)
    }

    #[test]
    fn chips_offer_notes_todos_and_passive_sessions_and_shrink_to_fit() {
        let mut doc = Doc::default();
        doc.groups.push(Group { id: "g".into(), name: "Payments".into(), x: 0, y: 0, w: 80, h: 10, ..Default::default() });
        let app_no_folder = app(doc.clone(), false);
        let labels = |chips: &[Chip]| chips.iter().map(|c| c.label.trim().to_owned()).collect::<Vec<_>>();
        let group = app_no_folder.doc.group("g").unwrap().clone();
        let chips = app_no_folder.group_chips(&group, false);
        assert_eq!(labels(&chips), ["⟲ + passive", "✎ Set folder…", "☑ +"]);
        assert_eq!(chips[0].action, Action::Visual(VisualAction::GroupPassives("g".into())), "the empty chip opens the panel with its Create");
        assert_eq!(chips[1].action, Action::Notebook(crate::notebook::Action::LinkFolder("g".into())));
        assert_eq!(chips[2].action, Action::Tasks(crate::tasks::Action::LinkBoards("g".into())), "no board yet: create or link one");
        assert!(chips.iter().all(|c| !c.lit && !c.attention));
        doc.groups[0].folder = Some(r"C:\g".into());
        let mut p: Node = serde_json::from_value(serde_json::json!({ "id": "p1", "kind": "session", "name": "Rollout", "x": 2, "y": 2 })).unwrap();
        p.passive = Some(Passive { key: "k".into(), group: "g".into(), ..Default::default() });
        doc.nodes.push(p);
        let mut app = app(doc, false);
        let group = app.doc.group("g").unwrap().clone();
        let full = app.fit_group_chips(&group, 14, 80);
        assert_eq!(labels(&full), ["⟲ 1 passive", "✎ Notes", "☑ +"]);
        assert_eq!(full[1].action, Action::Notebook(crate::notebook::Action::OpenGroup("g".into())));
        assert!(full[0].lit && full[1].lit && !full[2].lit);
        // Narrower: the least important chip goes compact first, the title stays whole.
        assert_eq!(labels(&app.fit_group_chips(&group, 14, 47)), ["⟲ 1 passive", "✎ Notes", "☑+"]);
        assert_eq!(labels(&app.fit_group_chips(&group, 14, 39)), ["⟲ 1", "✎ Notes", "☑+"]);
        assert_eq!(labels(&app.fit_group_chips(&group, 14, 33)), ["⟲ 1", "✎", "☑+"], "compact when the title would not fit");
        // Then they go, the least important first, next to a shortened title.
        assert_eq!(labels(&app.fit_group_chips(&group, 14, 24)), ["⟲ 1", "✎"]);
        assert_eq!(labels(&app.fit_group_chips(&group, 14, 19)), ["✎"]);
        assert!(app.fit_group_chips(&group, 14, 9).is_empty());
        // A linked board: its counts, gold while proposals wait; it stays longest.
        app.bus.todos = Some(crate::tasks::tests::book_with("g", 4, 2, 1));
        let full = app.fit_group_chips(&group, 14, 80);
        assert_eq!(labels(&full), ["⟲ 1 passive", "✎ Notes", "☑ 4 · 2▸ · 1◇"]);
        assert!(full[2].lit && full[2].attention);
        assert_eq!(full[2].action, Action::Tasks(crate::tasks::Action::OpenGroup("g".into())));
        assert_eq!(labels(&app.fit_group_chips(&group, 14, 24)), ["✎", "☑4"]);
        assert_eq!(labels(&app.fit_group_chips(&group, 14, 18)), ["☑4"]);
        app.bus.todos = Some(crate::tasks::tests::book_with("g", 3, 0, 0));
        let chips = app.group_chips(&group, false);
        assert_eq!((chips[2].label.trim(), chips[2].attention), ("☑ 3", false));
        let demo = super::tests::app(app.doc.clone(), true);
        assert_eq!(labels(&demo.group_chips(&group, false)), ["✎ Notes", "☑ +"], "no passive sessions in the demo");
    }

    #[test]
    fn todo_chips_follow_the_counts() {
        use crate::tasks::TodoCounts;
        let label = |counts: Option<TodoCounts>, compact: bool| todo_chip("g", counts, compact).label.trim().to_owned();
        assert_eq!(label(None, false), "☑ +");
        assert_eq!(label(None, true), "☑+");
        let counts = |open, in_progress, proposed| Some(TodoCounts { open, in_progress, proposed });
        assert_eq!(label(counts(4, 2, 1), false), "☑ 4 · 2▸ · 1◇");
        assert_eq!(label(counts(4, 0, 1), false), "☑ 4 · 1◇");
        assert_eq!(label(counts(0, 0, 0), false), "☑ 0");
        assert_eq!(label(counts(12, 3, 0), true), "☑12");
        assert!(todo_chip("g", counts(0, 0, 2), true).attention, "gold compact too");
        assert!(!todo_chip("g", counts(5, 1, 0), false).attention);
        for chip in [todo_chip("g", None, false), todo_chip("g", counts(4, 2, 1), false)] {
            assert!(chip.label.chars().all(|c| unicode_width::UnicodeWidthChar::width(c) == Some(1)), "{}", chip.label);
        }
    }

    #[test]
    fn every_group_shows_where_its_passive_sessions_belong() {
        use crate::app::SideKind;
        use crate::side::RightDetail;
        let mut doc = Doc::default();
        doc.groups.push(Group { id: "g".into(), name: "Payments".into(), x: 0, y: 0, w: 80, h: 10, folder: Some(r"C:\g".into()), ..Default::default() });
        doc.groups.push(Group { id: "e".into(), name: "Empty".into(), x: 0, y: 20, w: 80, h: 10, ..Default::default() });
        let mut p: Node = serde_json::from_value(serde_json::json!({ "id": "p1", "kind": "session", "name": "Rollout", "x": 2, "y": 2 })).unwrap();
        p.passive = Some(Passive { key: "k".into(), group: "g".into(), ..Default::default() });
        doc.nodes.push(p);
        let mut app = app(doc, false);
        let rows = crate::ui::build_side_rows(&app);
        let at = |kind: SideKind| rows.iter().position(|r| r.kind == kind).expect("row");
        let header = at(SideKind::Passives("g".into()));
        assert_eq!((rows[header].label.as_str(), rows[header].count), ("Passive sessions", Some(1)));
        let session = at(SideKind::Session("p1".into()));
        assert!(session == header + 1 && rows[session].depth == rows[header].depth + 1, "listed beneath its header");
        let empty = at(SideKind::Passives("e".into()));
        assert_eq!((rows[empty].label.as_str(), rows[empty].count), ("Passive: none · + add…", Some(0)));
        // A click on the empty group's row opens its panel, which offers Create; nothing starts.
        app.side_rows = rows;
        app.view_side_row(empty);
        assert_eq!(app.right_detail, Some(RightDetail::GroupPassives("e".into())));
        assert!(app.modals.is_empty(), "viewing makes nothing");
        app.run(Action::Visual(VisualAction::GroupPassives("g".into())));
        assert_eq!(app.right_detail, Some(RightDetail::GroupPassives("g".into())), "the chip and the menu open it too");
        let (title, items) = app.group_menu("g");
        assert_eq!(title, "Payments");
        assert!(items.iter().any(|i| i.action == Action::Visual(VisualAction::GroupPassives("g".into()))), "the group menu names it");
        // Canvas-wide agents are not a group's: their frame's chip leads to ◎ Agents.
        assert_eq!(app.group_passives("e").len(), 0);
    }

    #[test]
    fn remote_mode_includes_loopback_for_the_groups_files() {
        let mut app = app(Doc::default(), false);
        assert!(!app.service_is_remote());
        let instance = crate::paths::Instance::new(Some("t".into()), Some(std::env::temp_dir().join("cc-folders-test")), None).unwrap();
        app.target = Some(crate::conn::ConnTarget::Loopback(instance.clone()));
        assert!(app.service_is_remote() && !app.files_elsewhere(), "loopback is remote mode for the group's files, not for terminal links");
        app.target = Some(crate::conn::ConnTarget::Local(instance));
        assert!(!app.service_is_remote());
    }
}
