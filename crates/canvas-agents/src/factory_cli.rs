//! `copilot-canvas automations …`: manage manifest-defined scheduled tasks.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::factory::{self, Manifest, Principal, Report};
use crate::taskschd;

/// Where the manifests live: `--dir`, `COPILOT_CANVAS_AUTOMATIONS`, the
/// canvas setting, or `<home>/automations`.
pub fn configured_dir(home: &Path, explicit: Option<String>) -> PathBuf {
    if let Some(dir) = explicit.filter(|d| !d.trim().is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("COPILOT_CANVAS_AUTOMATIONS").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let canvas: Option<serde_json::Value> = std::fs::read(home.join("canvas.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    if let Some(dir) = canvas.as_ref().and_then(|c| c["automations_dir"].as_str()).filter(|d| !d.trim().is_empty()) {
        return PathBuf::from(dir);
    }
    home.join("automations")
}

fn load(dir: &Path) -> Result<factory::Repo> {
    if !dir.is_dir() {
        bail!("no automations folder at {} (import your tasks first: copilot-canvas automations import)", dir.display());
    }
    Ok(factory::load_repo(dir))
}

fn find<'a>(repo: &'a factory::Repo, name: &str) -> Result<&'a Manifest> {
    repo.manifests.iter().find(|m| m.name.eq_ignore_ascii_case(name)).with_context(|| format!("no automation named {name:?} in {}", repo.dir.display()))
}

fn ago(ms: u64) -> String {
    if ms == 0 { "—".into() } else { crate::paths::format_rfc3339_ms(ms) }
}

pub fn list(dir: &Path) -> Result<()> {
    let repo = load(dir)?;
    let tasks = taskschd::list().unwrap_or_default();
    let live = factory::live(&repo, &tasks);
    println!("{:<30} {:<24} {:<9} {:<26} {:<8} REPORT", "NAME", "FAMILY", "STATE", "LAST RUN", "RESULT");
    for l in &live {
        let m = &l.manifest;
        let state = match &l.task {
            None => "missing".to_owned(),
            Some(t) if !t.enabled => "disabled".to_owned(),
            Some(t) => t.state.clone(),
        };
        let (last, result) = match l.runs.iter().find(|r| !r.ended.is_empty()) {
            Some(r) => (r.started.chars().take(25).collect::<String>(), r.exit.map(|c| c.to_string()).unwrap_or_else(|| "?".into())),
            None => ("—".into(), "—".into()),
        };
        let report = l.report.as_ref().map(|r| format!("{} · {}", r.state, r.headline)).unwrap_or_default();
        println!("{:<30} {:<24} {:<9} {:<26} {:<8} {}", m.name, m.family.chars().take(24).collect::<String>(), state, last, result, report.chars().take(80).collect::<String>());
        for d in &l.drift {
            println!("    drift: {d}");
        }
    }
    for e in &repo.errors {
        println!("⚠ {e}");
    }
    Ok(())
}

pub fn show(dir: &Path, name: &str) -> Result<()> {
    let repo = load(dir)?;
    let m = find(&repo, name)?;
    let tasks = taskschd::list().unwrap_or_default();
    let live = factory::live(&repo, &tasks).into_iter().find(|l| l.manifest.name == m.name).context("not found")?;
    println!("{} ({})\nmanifest: {}\ncommand:  {} {}\nfolder:   {}\nschedule: {}", m.name, m.family, m.path.display(), m.command, m.args, m.cwd, m.schedule.iter().map(factory::Trigger::text).collect::<Vec<_>>().join(" · "));
    match &live.task {
        Some(t) => println!("task:     {} · enabled {} · last {} (result {:#x}) · next {}", t.state, t.enabled, ago(t.last_run_ms), t.last_result, ago(t.next_run_ms)),
        None => println!("task:     not registered"),
    }
    for d in &live.drift {
        println!("drift:    {d}");
    }
    println!("launcher log: {}\nstatus file:  {}", live.log_path.display(), live.status_path.display());
    for log in &live.logs {
        println!("workload log: {}", log.display());
    }
    for doc in &m.docs {
        println!("doc:      {doc}");
    }
    if let Some(r) = &live.report {
        println!("\nREPORT {} · {} ({})\n{}", r.state, r.headline, r.at, r.summary);
        for l in &r.links {
            println!("  {} → {}", l.title, l.url);
        }
        for x in &r.metrics {
            println!("  {}: {} {}", x.name, x.value, x.state);
        }
    }
    println!("\nRECENT RUNS");
    for r in live.runs.iter().take(10) {
        println!("  {}  →  {}  exit {}  {}", r.started, if r.ended.is_empty() { "running" } else { &r.ended }, r.exit.map(|c| c.to_string()).unwrap_or_else(|| "—".into()), r.error);
    }
    Ok(())
}

fn family_of(name: &str) -> String {
    name.split(['-', '_']).next().unwrap_or(name).to_owned()
}

fn slug(text: &str) -> String {
    let s: String = text.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-")
}

/// Writes manifests for existing tasks (the ones run through a launcher, or
/// the named ones). Existing manifest files are kept unless `force`.
pub fn import(dir: &Path, names: &[String], family: Option<String>, force: bool) -> Result<()> {
    let tasks = taskschd::list()?;
    let offset = factory::local_offset_minutes();
    let picked: Vec<&taskschd::TaskInfo> = if names.is_empty() {
        tasks.values().filter(|t| t.xml.contains(".py&quot;") || t.xml.contains(".py\"")).filter(|t| t.xml.contains(".json")).collect()
    } else {
        names.iter().map(|n| tasks.get(&n.to_lowercase()).with_context(|| format!("no task named {n:?}"))).collect::<Result<_>>()?
    };
    if picked.is_empty() {
        bail!("no tasks to import (name them: copilot-canvas automations import NAME…)");
    }
    std::fs::create_dir_all(dir)?;
    let existing = factory::load_repo(dir);
    let mut defaults_written = dir.join(factory::DEFAULTS_FILE).exists();
    let mut written = 0;
    for task in picked {
        let imported = factory::import_xml(&task.name, &task.xml, offset);
        let mut m = imported.manifest.clone();
        m.family = family.clone().unwrap_or_else(|| family_of(&m.name));
        if let Some(have) = existing.manifests.iter().find(|x| x.name.eq_ignore_ascii_case(&m.name))
            && !force
        {
            println!("= {} (manifest exists: {})", m.name, have.path.display());
            continue;
        }
        if !defaults_written && let Some(d) = &imported.defaults {
            let text = format!(
                "# Shared settings for the automations in this folder (Copilot Canvas).\n# launcher: hidden launcher script run with `python` (windowless pythonw.exe)\n# state: launcher folder with tasks/, logs/, status/, backups/\nlauncher: '{}'\npython: '{}'\nstate: '{}'\n",
                d.launcher.replace('\'', "''"),
                d.python.replace('\'', "''"),
                d.state.display().to_string().replace('\'', "''")
            );
            std::fs::write(dir.join(factory::DEFAULTS_FILE), text)?;
            defaults_written = true;
        }
        let folder = dir.join(slug(&m.family));
        std::fs::create_dir_all(&folder)?;
        let path = folder.join(format!("{}.yaml", m.name));
        m.path = path.clone();
        std::fs::write(&path, factory::manifest_yaml(&m))?;
        written += 1;
        println!("+ {} → {}", m.name, path.display());
        for u in &imported.unsupported {
            println!("    warning: trigger not representable, left out: {u}");
        }
    }
    println!("{written} manifest(s) written in {}. Review them, then: copilot-canvas automations apply", dir.display());
    Ok(())
}

/// Brings Task Scheduler in line with the manifests. Without `yes` it only
/// shows what would change.
pub fn apply(dir: &Path, names: &[String], yes: bool) -> Result<()> {
    let repo = load(dir)?;
    if !repo.errors.is_empty() {
        for e in &repo.errors {
            eprintln!("⚠ {e}");
        }
        bail!("fix the manifests above first");
    }
    let tasks = taskschd::list()?;
    let offset = factory::local_offset_minutes();
    let stamp = crate::paths::rfc3339_now().replace([':', '.'], "-");
    let backups = repo.defaults.state.join("backups").join(format!("{stamp}-canvas"));
    let mut pending = 0;
    for m in repo.manifests.iter().filter(|m| names.is_empty() || names.iter().any(|n| n.eq_ignore_ascii_case(&m.name))) {
        let existing = tasks.get(&m.name.to_lowercase());
        let imported = existing.map(|t| factory::import_xml(&m.name, &t.xml, offset));
        let current = Principal::current();
        let who = Principal {
            user_id: imported.as_ref().map(|i| i.user_id.clone()).filter(|u| !u.is_empty()).unwrap_or(current.user_id),
            logon_user: imported.as_ref().map(|i| i.logon_user.clone()).filter(|u| !u.is_empty()).unwrap_or(current.logon_user),
        };
        let mut changes = match &imported {
            None => vec!["new task".to_owned()],
            Some(i) => factory::drift(m, &repo.defaults, i),
        };
        let json_path = repo.defaults.task_json(&m.name);
        let json = factory::task_json(m, &repo.defaults);
        let uses_launcher = !repo.defaults.launcher.is_empty();
        if uses_launcher && std::fs::read_to_string(&json_path).ok().as_deref() != Some(json.as_str()) {
            changes.push(format!("launcher settings {} (name, status file, manifest)", if json_path.exists() { "updated" } else { "created" }));
        }
        if changes.is_empty() {
            println!("= {}", m.name);
            continue;
        }
        pending += 1;
        println!("{} {}", if yes { "→" } else { "~" }, m.name);
        for c in &changes {
            println!("    {c}");
        }
        if !yes {
            continue;
        }
        if let Some(t) = existing {
            std::fs::create_dir_all(&backups)?;
            std::fs::write(backups.join(format!("{}.xml", m.name)), &t.xml)?;
        }
        if uses_launcher {
            std::fs::create_dir_all(json_path.parent().context("task folder")?)?;
            std::fs::write(&json_path, &json)?;
        }
        taskschd::register(&m.name, &factory::task_xml(m, &repo.defaults, &who, offset))?;
        let after = taskschd::xml(&m.name)?.context("the task did not register")?;
        let left = factory::drift(m, &repo.defaults, &factory::import_xml(&m.name, &after, offset));
        if left.is_empty() {
            println!("    applied");
        } else {
            println!("    applied, but still differs: {}", left.join("; "));
        }
    }
    let unmanaged: Vec<&String> = tasks
        .values()
        .filter(|t| !repo.defaults.launcher.is_empty() && t.xml.to_lowercase().contains(&repo.defaults.launcher.to_lowercase()))
        .map(|t| &t.name)
        .filter(|n| !repo.manifests.iter().any(|m| m.name.eq_ignore_ascii_case(n)))
        .collect();
    for name in unmanaged {
        println!("? {name}: uses the launcher but has no manifest (left alone; import it to manage it)");
    }
    if yes {
        if pending > 0 && backups.exists() {
            println!("Previous task definitions saved in {}", backups.display());
        }
    } else if pending > 0 {
        println!("{pending} task(s) would change. Apply with: copilot-canvas automations apply --yes");
    } else {
        println!("Task Scheduler matches the manifests.");
    }
    Ok(())
}

pub fn run(dir: &Path, name: &str) -> Result<()> {
    let repo = load(dir)?;
    let m = find(&repo, name)?;
    taskschd::run(&m.name)?;
    println!("Started {}. Follow it with: copilot-canvas automations show \"{}\"", m.name, m.name);
    Ok(())
}

/// Sets `enabled:` in the manifest (the source of truth) and on the task.
pub fn set_enabled(dir: &Path, name: &str, enabled: bool) -> Result<()> {
    let repo = load(dir)?;
    let m = find(&repo, name)?;
    set_manifest_enabled(&m.path, enabled)?;
    taskschd::set_enabled(&m.name, enabled)?;
    println!("{} {}.", m.name, if enabled { "enabled" } else { "disabled" });
    Ok(())
}

pub fn set_manifest_enabled(path: &Path, enabled: bool) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let mut found = false;
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            if !found && line.starts_with("enabled:") {
                found = true;
                format!("enabled: {enabled}")
            } else {
                line.to_owned()
            }
        })
        .collect();
    if !found {
        let at = lines.iter().position(|l| l.starts_with("name:")).map(|i| i + 1).unwrap_or(0);
        lines.insert(at, format!("enabled: {enabled}"));
    }
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    std::fs::write(path, lines.join(newline) + newline)?;
    Ok(())
}

pub struct ReportArgs {
    pub state: String,
    pub headline: String,
    pub summary: Option<String>,
    pub summary_file: Option<String>,
    pub links: Vec<String>,
    pub metrics: Vec<String>,
    pub name: Option<String>,
}

/// Writes the running automation's status report.
pub fn report(dir: &Path, args: ReportArgs) -> Result<()> {
    let state = args.state.to_ascii_lowercase();
    if !matches!(state.as_str(), "ok" | "warn" | "alert" | "failed") {
        bail!("state must be ok, warn, alert or failed");
    }
    let path = match (std::env::var_os("AUTOMATION_STATUS_FILE"), &args.name) {
        (_, Some(name)) => factory::load_repo(dir).defaults.status_path(name),
        (Some(path), None) => PathBuf::from(path),
        (None, None) => bail!("run this inside an automation (AUTOMATION_STATUS_FILE) or pass --name NAME"),
    };
    let summary = match (&args.summary_file, &args.summary) {
        (Some(file), _) => std::fs::read_to_string(file).with_context(|| format!("cannot read {file}"))?,
        (None, Some(text)) => text.replace("\\n", "\n"),
        (None, None) => String::new(),
    };
    let links = args
        .links
        .iter()
        .map(|l| match l.split_once('=') {
            Some((t, u)) => factory::Link { title: t.trim().into(), url: u.trim().into() },
            None => factory::Link { title: l.clone(), url: l.clone() },
        })
        .collect();
    let metrics = args
        .metrics
        .iter()
        .map(|m| {
            let mut parts = m.splitn(3, '=');
            factory::Metric { name: parts.next().unwrap_or_default().trim().into(), value: parts.next().unwrap_or_default().trim().into(), state: parts.next().unwrap_or_default().trim().into() }
        })
        .collect();
    let report = Report {
        state,
        headline: args.headline.chars().take(300).collect(),
        summary,
        links,
        metrics,
        at: crate::paths::rfc3339_now(),
        run: std::env::var("AUTOMATION_RUN_ID").unwrap_or_default(),
    };
    factory::write_report(&path, &report)?;
    println!("Reported {} to {}", report.state, path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabling_edits_only_the_enabled_line() {
        let path = std::env::temp_dir().join(format!("cc-en-{}.yaml", uuid::Uuid::new_v4()));
        std::fs::write(&path, "name: 'A'\r\nenabled: true\r\n# enabled: in a comment\r\n").unwrap();
        set_manifest_enabled(&path, false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "name: 'A'\r\nenabled: false\r\n# enabled: in a comment\r\n");
        std::fs::write(&path, "name: 'A'\nfamily: 'F'\n").unwrap();
        set_manifest_enabled(&path, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "name: 'A'\nenabled: true\nfamily: 'F'\n");
        let _ = std::fs::remove_file(path);
    }
}
