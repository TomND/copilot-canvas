//! Group folders and passive sessions within reach: the chips on a group's
//! title row (folders, passive sessions, "+ passive") at every zoom, the
//! sidebar's folder rows, their menus, and opening the group's files and
//! folders. In remote mode (any `--remote`, loopback included) folders open
//! in VS Code through the server's tunnel or in the canvas's folder browser,
//! and files in VS Code or the read-only viewer: never in this machine's
//! Explorer, which would show the wrong machine.

use crate::app::App;
use crate::dialogs::{Action, MenuItem, Modal};
use crate::doc::{Group, Node};
use crate::visual::VisualAction;

/// A chip on a group's title row.
#[derive(Clone, Debug, PartialEq)]
pub struct Chip {
    pub label: String,
    pub action: Action,
    /// In the group's colour (else muted: nothing set up yet).
    pub lit: bool,
}

/// A title keeps at least this many cells next to compact chips.
const MIN_TITLE: i32 = 6;

impl Chip {
    pub fn width(&self) -> i32 {
        crate::paint::text_width(&self.label) as i32
    }
}

/// Cells the chips take (each with a gap after it).
pub fn chips_width(chips: &[Chip]) -> i32 {
    chips.iter().map(|c| c.width() + 1).sum()
}

impl App {
    /// The passive sessions of a group (by their own group, wherever their box
    /// is; the canvas-wide agents are not a group's).
    pub fn group_passives(&self, group: &str) -> Vec<&Node> {
        self.doc.nodes.iter().filter(|n| !n.is_canvas_agent() && n.passive.as_ref().is_some_and(|p| p.group == group)).collect()
    }

    /// A group's chips: passive sessions ("⟲ 2 passive", or "⟲ + passive";
    /// both open the group's passive panel), then its folders ("◧ AI docs",
    /// "◨ Outputs", or "Set folder…"). `compact`: the short forms for small sizes.
    pub fn group_chips(&self, group: &Group, compact: bool) -> Vec<Chip> {
        if group.is_canvas_agents() {
            return if self.demo { Vec::new() } else { self.frame_chips(compact) };
        }
        let mut chips = Vec::new();
        let chip = |label: &str, action: Action, lit: bool| Chip { label: label.to_owned(), action, lit };
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
            if group.folder.is_some() {
                chips.push(chip(if compact { " ◧ " } else { " ◧ AI docs " }, Action::OpenGroupFolder(group.id.clone(), false), true));
                chips.push(chip(if compact { " ◨ " } else { " ◨ Outputs " }, Action::OpenGroupFolder(group.id.clone(), true), true));
            } else {
                chips.push(chip(if compact { " ◧+ " } else { " ◧ Set folder… " }, Action::SetGroupFolder(group.id.clone()), false));
            }
        }
        chips
    }

    /// The chips that fit on a title row `row` cells wide whose title takes
    /// `title` cells: full labels when the whole title fits too, then a
    /// compact passive chip, then compact ones next to a shortened title,
    /// dropping the passive chip first.
    pub fn fit_group_chips(&self, group: &Group, title: i32, row: i32) -> Vec<Chip> {
        let room = row - 4;
        let full = self.group_chips(group, false);
        if title + chips_width(&full) <= room {
            return full;
        }
        let mut compact = self.group_chips(group, true);
        let passive_first = compact.first().is_some_and(|c| matches!(c.action, Action::NewPassive(_) | Action::Visual(VisualAction::GroupPassives(_)) | Action::Agents(_)));
        if passive_first && compact.len() == full.len() && compact.len() > 1 {
            let mixed: Vec<Chip> = compact[..1].iter().chain(&full[1..]).cloned().collect();
            if title + chips_width(&mixed) <= room {
                return mixed;
            }
        }
        while !compact.is_empty() && MIN_TITLE + chips_width(&compact) > room {
            compact.remove(0);
        }
        compact
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
    fn chips_offer_folders_and_passive_sessions_and_shrink_to_fit() {
        let mut doc = Doc::default();
        doc.groups.push(Group { id: "g".into(), name: "Payments".into(), x: 0, y: 0, w: 80, h: 10, ..Default::default() });
        let app_no_folder = app(doc.clone(), false);
        let labels = |chips: &[Chip]| chips.iter().map(|c| c.label.trim().to_owned()).collect::<Vec<_>>();
        let group = app_no_folder.doc.group("g").unwrap().clone();
        assert_eq!(labels(&app_no_folder.group_chips(&group, false)), ["⟲ + passive", "◧ Set folder…"]);
        assert_eq!(app_no_folder.group_chips(&group, false)[0].action, Action::Visual(VisualAction::GroupPassives("g".into())), "the empty chip opens the panel with its Create");
        assert_eq!(app_no_folder.group_chips(&group, false)[1].action, Action::SetGroupFolder("g".into()));
        doc.groups[0].folder = Some(r"C:\g".into());
        let mut p: Node = serde_json::from_value(serde_json::json!({ "id": "p1", "kind": "session", "name": "Rollout", "x": 2, "y": 2 })).unwrap();
        p.passive = Some(Passive { key: "k".into(), group: "g".into(), ..Default::default() });
        doc.nodes.push(p);
        let app = app(doc, false);
        let group = app.doc.group("g").unwrap().clone();
        let full = app.fit_group_chips(&group, 14, 80);
        assert_eq!(labels(&full), ["⟲ 1 passive", "◧ AI docs", "◨ Outputs"]);
        assert_eq!(full[0].action, Action::Visual(VisualAction::GroupPassives("g".into())));
        assert_eq!(labels(&app.fit_group_chips(&group, 14, 40)), ["⟲ 1", "◧", "◨"], "compact when the title would not fit");
        assert_eq!(labels(&app.fit_group_chips(&group, 14, 18)), ["◧", "◨"], "the passive chip goes first");
        assert!(app.fit_group_chips(&group, 14, 8).is_empty());
        let demo = super::tests::app(app.doc.clone(), true);
        assert_eq!(labels(&demo.group_chips(&group, false)), ["◧ AI docs", "◨ Outputs"], "no passive sessions in the demo");
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
