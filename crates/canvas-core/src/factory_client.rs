//! Manifest-managed automations (scheduled tasks) on the canvas: loading,
//! their status, boxes arranged by family, actions (run now, enable/disable,
//! open files, ask the doctor), what the service watches, and the default
//! "Doctor" passive session that investigates failures.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::app::{App, AppEvent};
use crate::automation::{AutoLoad, Automation};
use crate::bus::AutoRoute;
use crate::doc::{GRID_X, GRID_Y, Node, NodeKind, Rect, SESSION_H, SESSION_W};
use crate::status::Status;

/// The event for a background load.
pub fn loaded(load: anyhow::Result<AutoLoad>) -> AppEvent {
    match load {
        Ok(load) => AppEvent::Automations(load),
        Err(error) => AppEvent::AutomationsFailed(format!("{error:#}")),
    }
}

/// Index of the doctor in `passive::TEMPLATES`.
pub const DOCTOR_TEMPLATE: usize = 3;
const PER_ROW: i32 = 6;

/// Status of a manifest-managed automation.
pub fn task_status(a: &Automation) -> Option<Status> {
    let live = a.factory.as_ref()?;
    let latest = live.runs.first();
    let running = latest.is_some_and(|r| r.ended.is_empty() && r.error.is_empty()) || live.task.as_ref().is_some_and(|t| t.state == "running");
    if running {
        return Some(Status::Working);
    }
    let finished = live.runs.iter().find(|r| !r.ended.is_empty());
    let failed_run = finished.is_some_and(|r| !live.manifest.succeeded(r));
    let report = live.report.as_ref().filter(|r| finished.is_some_and(|f| crate::automation::report_is_for(r, &f.started)));
    if failed_run || report.is_some_and(|r| matches!(r.state.as_str(), "failed" | "alert")) {
        return Some(Status::Failed);
    }
    if live.task.is_none() || !a.enabled {
        return Some(Status::Stopped);
    }
    if report.is_some_and(|r| r.state == "warn") {
        return Some(Status::Waiting);
    }
    let recent = finished.and_then(|f| crate::paths::parse_rfc3339_ms(&f.ended)).is_some_and(|t| crate::paths::now_ms().saturating_sub(t) < 10 * 60_000);
    Some(if recent { Status::JustFinished } else { Status::Idle })
}

/// Box subtitle: what happened last and what happens next.
pub fn task_subtitle(a: &Automation) -> Option<String> {
    let live = a.factory.as_ref()?;
    if live.task.is_none() {
        return Some("not registered · run apply".into());
    }
    let next = if a.enabled && !a.next_run_at.is_empty() { format!("next {}", crate::side::when(&a.next_run_at)) } else if a.enabled { a.schedule.clone() } else { "disabled".into() };
    match live.runs.iter().find(|r| !r.ended.is_empty()) {
        Some(r) if !live.manifest.succeeded(r) => Some(format!("{} {} · {next}", r.exit.map(|c| format!("exit {c}")).unwrap_or_else(|| "no exit".into()), crate::side::when(&r.ended))),
        _ => match live.report.as_ref().filter(|r| !r.headline.is_empty()) {
            Some(report) => Some(format!("{} · {next}", report.headline)),
            None => Some(format!("{} · {next}", a.project)),
        },
    }
}

impl App {
    /// The automation manifest folder in use (resolved by the backend).
    pub fn automations_dir(&self) -> PathBuf {
        PathBuf::from(&self.automations_folder)
    }

    /// Loads Copilot's automations and the manifest-managed tasks in the background.
    pub fn load_automations(&mut self) {
        if self.automations_loading {
            return;
        }
        self.automations_loading = true;
        self.automations_at = std::time::Instant::now();
        let (dir, tasks) = (self.doc.automations_dir.clone(), !self.demo);
        self.in_background(move |backend| backend.automations(dir, tasks), loaded);
    }

    pub fn on_automations(&mut self, load: AutoLoad) {
        self.automations_loading = false;
        self.automations_factory_ok = load.factory_loaded;
        if !load.dir.is_empty() {
            self.automations_folder = load.dir;
        }
        self.automations = load.list;
        self.automations_error = if load.errors.is_empty() { None } else { Some(load.errors.join(" · ")) };
        self.sync_automation_nodes();
        self.needs_draw = true;
    }

    /// Manifest-managed automations whose latest run failed.
    pub fn failing_automations(&self) -> Vec<&Automation> {
        self.automations.iter().filter(|a| task_status(a) == Some(Status::Failed)).collect()
    }

    /// Places new task boxes in rows per family (six to a row) inside the group.
    pub fn place_task_boxes(&mut self, group_id: &str, mut seeds: Vec<(String, String, String)>) {
        let Some(group) = self.doc.group(group_id).cloned() else { return };
        seeds.sort_by_key(|s| (s.2.to_lowercase(), s.1.to_lowercase()));
        let rect = group.rect();
        let family_of = |app: &App, key: &str| app.automation(key).map(|a| a.project.clone()).unwrap_or_default();
        let mut rows: BTreeMap<String, Vec<(i32, i32)>> = BTreeMap::new();
        let mut bottom = None::<i32>;
        for node in self.doc.nodes.iter().filter(|n| rect.contains(n.rect().center().0, n.rect().center().1)) {
            bottom = Some(bottom.unwrap_or(i32::MIN).max(node.y + node.rect().h));
            if let Some(key) = node.automation.as_deref().filter(|k| k.starts_with("task:")) {
                rows.entry(family_of(self, key)).or_default().push((node.x, node.y));
            }
        }
        for (key, name, family) in seeds {
            let wanted = match rows.get(&family) {
                Some(list) => {
                    let (x0, y0) = (list.iter().map(|p| p.0).min().unwrap_or(group.x + 2), list.iter().map(|p| p.1).min().unwrap_or(group.y + 2));
                    let n = list.len() as i32;
                    (x0 + (n % PER_ROW) * GRID_X, y0 + (n / PER_ROW) * GRID_Y)
                }
                None => (group.x + 2, bottom.map(|b| b + 3).unwrap_or(group.y + 2)),
            };
            let target = Rect { x: wanted.0, y: wanted.1, w: SESSION_W, h: SESSION_H };
            let collides = self.doc.nodes.iter().any(|n| n.rect().intersects(&target)) || self.doc.groups.iter().any(|g| g.id != group.id && g.rect().intersects(&target));
            let (x, y) = if collides { self.slot_or_overflow(group_id).1 } else { wanted };
            let id = self.shared_id("auto", &key);
            self.doc.nodes.push(Node {
                id,
                kind: NodeKind::Automation,
                name,
                x,
                y,
                cwd: None,
                parent: None,
                agent_key: None,
                source: Some("automation".into()),
                command: None,
                fresh: false,
                automation: Some(key),
                synced_name: None,
                reviewed_ms: None,
                mcp_off: None,
                passive: None,
                no_messages: false,
                agent: None,
                agent_session: None,
            });
            rows.entry(family).or_default().push((x, y));
            bottom = Some(bottom.unwrap_or(i32::MIN).max(y + SESSION_H));
            if let Some(g) = self.doc.group_mut(group_id) {
                g.w = g.w.max(x + SESSION_W + 2 - g.x);
                g.h = g.h.max(y + SESSION_H + 2 - g.y);
            }
        }
        self.mark_dirty();
    }

    /// Family labels above each row of task boxes in an automations group
    /// (canvas coordinates): (x, y, family).
    pub fn family_labels(&self, group: &crate::doc::Group) -> Vec<(i32, i32, String)> {
        let rect = group.rect();
        let mut first: BTreeMap<String, (i32, i32)> = BTreeMap::new();
        for node in self.doc.nodes.iter().filter(|n| n.kind == NodeKind::Automation && rect.contains(n.rect().center().0, n.rect().center().1)) {
            let Some(a) = node.automation.as_deref().and_then(|k| self.automation(k)).filter(|a| a.factory.is_some()) else { continue };
            let entry = first.entry(a.project.clone()).or_insert((node.x, node.y));
            if (node.y, node.x) < (entry.1, entry.0) {
                *entry = (node.x, node.y);
            }
        }
        first.into_iter().map(|(family, (x, y))| (x, y - 1, family)).collect()
    }

    /// The default doctor: created once in the automations group when it holds
    /// manifest-managed automations (removing it is respected).
    pub fn ensure_doctor(&mut self, group_id: &str) {
        if self.doc.doctor_created || self.demo {
            return;
        }
        if self.doc.nodes.iter().any(|n| n.passive.as_ref().is_some_and(|p| p.group == group_id && p.template == crate::passive::TEMPLATES[DOCTOR_TEMPLATE].name)) {
            self.doc.doctor_created = true;
            return;
        }
        if self.doc.group(group_id).is_some_and(|g| g.folder.is_none()) {
            let Ok(folder) = self.backend.doctor_folder() else { return };
            if let Some(g) = self.doc.group_mut(group_id) {
                g.folder = Some(folder);
            }
        }
        let (selection, detail, selected) = (self.selection.clone(), self.right_detail.clone(), self.doc.selected.clone());
        match self.create_passive(group_id, DOCTOR_TEMPLATE, "Doctor") {
            Ok(_) => {
                self.doc.doctor_created = true;
                self.notify("Added the automation Doctor: it investigates and fixes failed automations (see its rules).", false);
            }
            Err(error) => self.automations_error = Some(format!("could not add the doctor: {error}")),
        }
        self.selection = selection;
        self.right_detail = detail;
        self.doc.selected = selected;
        self.mark_dirty();
    }

    /// What the service watches: task boxes and their group.
    pub fn automation_routes(&self) -> Vec<AutoRoute> {
        self.doc
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Automation)
            .filter_map(|n| {
                let a = n.automation.as_deref().and_then(|k| self.automation(k))?;
                let live = a.factory.as_ref()?;
                let group = self.doc.group_of(n)?;
                let m = &live.manifest;
                Some(AutoRoute {
                    name: m.name.clone(),
                    family: m.family.clone(),
                    group: group.id.clone(),
                    log: live.log_path.display().to_string(),
                    status: live.status_path.display().to_string(),
                    manifest: m.path.display().to_string(),
                    command: format!("{} {}", m.command, m.args).trim().to_owned(),
                    ai: m.ai,
                    docs: m.docs.clone(),
                    logs: m.logs.clone(),
                    success_codes: m.success_codes.clone(),
                })
            })
            .collect()
    }

    pub fn run_automation(&mut self, key: &str) {
        let Some(a) = self.automation(key).cloned() else { return };
        if a.factory.is_none() {
            return self.notify("Copilot automations run from VS Code or the Copilot app.", false);
        }
        let name = a.name.clone();
        self.notify(format!("Starting {name}…"), false);
        let label = format!("Run {name}");
        self.in_background(move |backend| backend.task_run(&name).map(|_| String::new()).map_err(|e| format!("{e:#}")), move |result| AppEvent::CliDone { label, result });
        self.automations_at = std::time::Instant::now() - std::time::Duration::from_secs(12);
    }

    /// Enables or disables a task: the manifest (source of truth) and the task.
    pub fn toggle_task(&mut self, a: &Automation) {
        let Some(live) = a.factory.as_ref() else { return };
        let (name, path, enabled) = (a.name.clone(), live.manifest.path.display().to_string(), !a.enabled);
        self.notify(format!("{} {name}…", if enabled { "Enabling" } else { "Disabling" }), false);
        let label = format!("{} {name}", if enabled { "Enabled" } else { "Disabled" });
        self.in_background(
            move |backend| backend.task_set_enabled(&name, &path, enabled).map(|_| String::new()).map_err(|e| format!("{e:#}")),
            move |result| AppEvent::CliDone { label, result },
        );
        self.automations_at = std::time::Instant::now() - std::time::Duration::from_secs(13);
    }

    /// 0 manifest · 1 launcher log · 2 newest workload log · 3 status report.
    pub fn open_automation_file(&mut self, key: &str, which: u8) {
        let Some(live) = self.automation(key).and_then(|a| a.factory.clone()) else { return };
        let target = match which {
            0 => live.manifest.path.display().to_string(),
            1 => live.log_path.display().to_string(),
            2 => live.logs.first().map(|p| p.display().to_string()).unwrap_or_default(),
            _ => live.status_path.display().to_string(),
        };
        self.open_target(&target);
    }

    /// Wakes the group's doctor for this automation (`automation.check`).
    pub fn ask_doctor(&mut self, key: &str) {
        let Some(a) = self.automation(key).cloned() else { return };
        let Some(route) = self.automation_routes().into_iter().find(|r| r.name == a.name) else {
            return self.notify("Put this automation's box in a group with a doctor first.", false);
        };
        let has_doctor = self.doc.nodes.iter().any(|n| n.passive.as_ref().is_some_and(|p| p.group == route.group && p.subscribe.iter().any(|s| s == "automation.check" || s == "automation.*" || s == "*")));
        if !has_doctor {
            return self.notify("No passive session in this group reacts to automation.check (add the Doctor template).", false);
        }
        let logs: Vec<String> = self.backend.newest_matches(route.logs.clone()).unwrap_or_default();
        self.bus_action(serde_json::json!({"action": "emit", "event": {
            "type": "automation.check",
            "group": route.group,
            "source_name": "you",
            "data": {
                "name": route.name, "family": route.family, "request": "Thomas asked for a health check of this automation",
                "manifest": route.manifest, "command": route.command, "uses_ai": route.ai, "launcher_log": route.log,
                "status_file": route.status, "workload_logs": logs, "docs": route.docs,
                "rerun": format!("copilot-canvas automations run \"{}\"", route.name),
            }
        }}));
        self.notify(format!("Asked the doctor to check {}.", a.name), false);
    }
}
