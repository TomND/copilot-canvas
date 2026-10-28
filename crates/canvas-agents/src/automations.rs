//! Automations shared by Copilot and VS Code (`~/.copilot/data.db`):
//! scheduled workflows with their run history, and per-session `/every` schedules.
//! Read-only except for toggling a workflow's `enabled` flag. Scheduled tasks
//! defined by manifests (`factory`) are merged in as `task:<name>`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

pub use canvas_model::automation::{Automation, Run};

pub fn db_path() -> PathBuf {
    crate::copilot_cfg::copilot_home().join("data.db")
}

fn columns(connection: &Connection, table: &str) -> Vec<String> {
    let Ok(mut statement) = connection.prepare(&format!("PRAGMA table_info(\"{table}\")")) else { return Vec::new() };
    statement.query_map([], |row| row.get::<_, String>(1)).map(|rows| rows.filter_map(|r| r.ok()).collect()).unwrap_or_default()
}

fn text(row: &rusqlite::Row<'_>, index: usize) -> String {
    match row.get::<_, rusqlite::types::Value>(index) {
        Ok(rusqlite::types::Value::Text(s)) => s,
        Ok(rusqlite::types::Value::Integer(i)) => i.to_string(),
        Ok(rusqlite::types::Value::Real(r)) => r.to_string(),
        _ => String::new(),
    }
}

/// Human-readable schedule from whichever columns the installed schema has.
fn describe_schedule(fields: &std::collections::HashMap<String, String>) -> String {
    let get = |k: &str| fields.get(k).map(String::as_str).unwrap_or("");
    if !get("cron_expression").is_empty() {
        return format!("cron {}", get("cron_expression"));
    }
    let time = match (get("schedule_hour"), get("schedule_minute")) {
        ("", _) => String::new(),
        (h, "") => format!(" at {h:0>2}:00"),
        (h, m) => format!(" at {h:0>2}:{m:0>2}"),
    };
    match get("interval") {
        "minutes" => match get("every_minutes") {
            "" => "every few minutes".into(),
            m => format!("every {m} min"),
        },
        "daily" => format!("daily{time}"),
        "weekly" => format!("weekly{}{time}", if get("schedule_day").is_empty() { String::new() } else { format!(" on day {}", get("schedule_day")) }),
        "once" => format!("once{time}"),
        "" => "manual".into(),
        other => format!("{other}{time}"),
    }
}

fn open_read(path: &Path) -> Result<Connection> {
    let uri = format!("file:{}?mode=ro", path.to_string_lossy().replace('\\', "/"));
    let connection = Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    Ok(connection)
}

/// Loads all automations. A missing database means none.
pub fn load(path: &Path) -> Result<Vec<Automation>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let connection = open_read(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut out = Vec::new();
    let workflow_cols = columns(&connection, "workflows");
    if !workflow_cols.is_empty() {
        let projects: std::collections::HashMap<String, String> = connection
            .prepare("SELECT id, name FROM projects")
            .and_then(|mut s| s.query_map([], |r| Ok((text(r, 0), text(r, 1)))).map(|rows| rows.filter_map(|r| r.ok()).collect()))
            .unwrap_or_default();
        let select = workflow_cols.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ");
        let mut statement = connection.prepare(&format!("SELECT {select} FROM workflows ORDER BY name COLLATE NOCASE"))?;
        let rows = statement.query_map([], |row| {
            Ok(workflow_cols.iter().enumerate().map(|(i, c)| (c.clone(), text(row, i))).collect::<std::collections::HashMap<_, _>>())
        })?;
        let run_cols = columns(&connection, "workflow_runs");
        for fields in rows.filter_map(|r| r.ok()) {
            let get = |k: &str| fields.get(k).cloned().unwrap_or_default();
            let id = get("id");
            let mut runs = Vec::new();
            if !run_cols.is_empty() {
                let pick = |c: &str| if run_cols.iter().any(|x| x == c) { format!("\"{c}\"") } else { "''".into() };
                let sql = format!(
                    "SELECT {}, {}, {}, {}, {}, {}, {} FROM workflow_runs WHERE task_id = ?1 ORDER BY COALESCE(started_at, '') DESC LIMIT 10",
                    pick("id"), pick("status"), pick("trigger"), pick("started_at"), pick("completed_at"), pick("error_message"), pick("session_id")
                );
                if let Ok(mut s) = connection.prepare(&sql) {
                    runs = s
                        .query_map([&id], |r| {
                            Ok(Run { id: text(r, 0), status: text(r, 1), trigger: text(r, 2), started_at: text(r, 3), completed_at: text(r, 4), error: text(r, 5), session_id: text(r, 6) })
                        })
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                        .unwrap_or_default();
                }
            }
            let project = projects.get(&get("project_id")).cloned().unwrap_or_default();
            out.push(Automation {
                key: format!("wf:{id}"),
                name: get("name"),
                prompt: get("prompt"),
                kind: if get("code_review_automation_id").is_empty() { "workflow".into() } else { "code review".into() },
                enabled: get("enabled") != "0" && !get("enabled").eq_ignore_ascii_case("false"),
                toggleable: workflow_cols.iter().any(|c| c == "enabled"),
                schedule: describe_schedule(&fields),
                model: get("model"),
                reasoning_effort: get("reasoning_effort"),
                mode: get("mode"),
                project,
                workspace: get("workspace_type"),
                last_run_at: get("last_run_at"),
                next_run_at: get("next_run_at"),
                updated_at: get("updated_at"),
                runs,
                session_id: String::new(),
                factory: None,
            });
        }
    }
    let session_cols = columns(&connection, "session_automations");
    if !session_cols.is_empty() {
        let select = session_cols.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ");
        let mut statement = connection.prepare(&format!("SELECT {select} FROM session_automations"))?;
        let rows = statement.query_map([], |row| {
            Ok(session_cols.iter().enumerate().map(|(i, c)| (c.clone(), text(row, i))).collect::<std::collections::HashMap<_, _>>())
        })?;
        for fields in rows.filter_map(|r| r.ok()) {
            let get = |k: &str| fields.get(k).cloned().unwrap_or_default();
            let session_id = get("session_id");
            let title: Option<String> = connection
                .query_row("SELECT title FROM sessions WHERE id = ?1", [&session_id], |r| r.get(0))
                .optional()
                .ok()
                .flatten();
            let name = title.filter(|t| !t.is_empty()).unwrap_or_else(|| format!("Session {}", &session_id[..session_id.len().min(8)]));
            out.push(Automation {
                key: format!("session:{session_id}"),
                name: format!("{name} (session schedule)"),
                prompt: get("prompt"),
                kind: "session schedule".into(),
                enabled: true,
                toggleable: false,
                schedule: describe_schedule(&fields),
                last_run_at: get("last_run_at"),
                next_run_at: get("next_run_at"),
                updated_at: get("updated_at"),
                session_id,
                ..Default::default()
            });
        }
    }
    Ok(out)
}

/// Copilot's automations plus the manifest-managed scheduled tasks in `dir`.
/// Problems with one source are returned as messages; the rest still loads.
pub fn load_all(db: &Path, dir: Option<&Path>) -> (Vec<Automation>, Vec<String>) {
    let mut errors = Vec::new();
    let mut out = load(db).unwrap_or_else(|e| {
        errors.push(format!("Copilot automations: {e:#}"));
        Vec::new()
    });
    if let Some(dir) = dir.filter(|d| d.is_dir()) {
        let repo = crate::factory::load_repo(dir);
        errors.extend(repo.errors.iter().cloned());
        let tasks = crate::taskschd::list().unwrap_or_else(|e| {
            errors.push(format!("Task Scheduler: {e:#}"));
            Default::default()
        });
        let mut tasks_out: Vec<Automation> = crate::factory::live(&repo, &tasks).into_iter().map(from_live).collect();
        tasks_out.sort_by_key(|a| (a.project.to_lowercase(), a.name.to_lowercase()));
        out.extend(tasks_out);
    }
    (out, errors)
}

fn from_live(live: crate::factory::Live) -> Automation {
    let m = &live.manifest;
    let ms = |ms: u64| if ms == 0 { String::new() } else { crate::paths::format_rfc3339_ms(ms) };
    let runs = live
        .runs
        .iter()
        .map(|r| Run {
            id: r.started.clone(),
            status: match (r.ended.is_empty(), r.exit) {
                (true, _) if r.error.is_empty() => "running".into(),
                _ if m.succeeded(r) => "ok".into(),
                _ => "failed".into(),
            },
            trigger: String::new(),
            started_at: r.started.clone(),
            completed_at: r.ended.clone(),
            error: match (r.exit, r.error.is_empty()) {
                _ if m.succeeded(r) => String::new(),
                (Some(code), true) => format!("exit code {code}"),
                (Some(code), false) => format!("exit code {code}: {}", r.error),
                (None, _) => r.error.clone(),
            },
            session_id: String::new(),
        })
        .collect();
    let enabled = live.task.as_ref().map(|t| t.enabled).unwrap_or(m.enabled);
    Automation {
        key: format!("task:{}", m.name),
        name: m.name.clone(),
        prompt: format!("{} {}", m.command, m.args).trim().to_owned(),
        kind: if m.ai { "scheduled task · AI".into() } else { "scheduled task".into() },
        enabled,
        toggleable: live.task.is_some(),
        schedule: m.schedule.iter().map(crate::factory::Trigger::text).collect::<Vec<_>>().join(" · "),
        model: String::new(),
        reasoning_effort: String::new(),
        mode: String::new(),
        project: m.family.clone(),
        workspace: m.cwd.clone(),
        last_run_at: live.runs.first().map(|r| r.started.clone()).unwrap_or_else(|| ms(live.task.as_ref().map(|t| t.last_run_ms).unwrap_or(0))),
        next_run_at: ms(live.task.as_ref().filter(|t| t.enabled).map(|t| t.next_run_ms).unwrap_or(0)),
        updated_at: String::new(),
        runs,
        session_id: String::new(),
        factory: Some(Box::new(live)),
    }
}

/// Enables or disables a workflow. Only the `enabled` flag (and `updated_at`
/// when present) changes, inside a transaction.
pub fn set_enabled(path: &Path, key: &str, enabled: bool) -> Result<()> {
    let id = key.strip_prefix("wf:").context("only workflows can be enabled or disabled")?;
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    let has_updated = columns(&connection, "workflows").iter().any(|c| c == "updated_at");
    let tx = connection.transaction()?;
    let changed = if has_updated {
        tx.execute(
            "UPDATE workflows SET enabled = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?2",
            rusqlite::params![enabled as i64, id],
        )?
    } else {
        tx.execute("UPDATE workflows SET enabled = ?1 WHERE id = ?2", rusqlite::params![enabled as i64, id])?
    };
    anyhow::ensure!(changed == 1, "automation not found");
    tx.commit()?;
    Ok(())
}

/// Creates a database with the schema Copilot uses (subset) and sample rows
/// (unit tests and the end-to-end selftest).
pub fn write_sample_db(path: &Path) -> Result<()> {
    let db = Connection::open(path)?;
    db.execute_batch(
        "CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT);
         CREATE TABLE workflows (id TEXT PRIMARY KEY, name TEXT NOT NULL, prompt TEXT NOT NULL, model TEXT, reasoning_effort TEXT,
           project_id TEXT, interval TEXT, schedule_hour INTEGER, schedule_day INTEGER, enabled INTEGER NOT NULL DEFAULT 1,
           created_at TEXT, updated_at TEXT, last_run_at TEXT, next_run_at TEXT, mode TEXT, schedule_minute INTEGER,
           workspace_type TEXT, workspace_id TEXT, cron_expression TEXT, context_tier TEXT, code_review_automation_id TEXT);
         CREATE TABLE workflow_runs (id TEXT PRIMARY KEY, task_id TEXT NOT NULL, status TEXT NOT NULL, trigger TEXT,
           workspace_id TEXT, session_id TEXT, started_at TEXT, completed_at TEXT, error_message TEXT, archived_at TEXT, taken_over_at TEXT);
         CREATE TABLE session_automations (session_id TEXT PRIMARY KEY, prompt TEXT NOT NULL, interval TEXT NOT NULL, every_minutes INTEGER,
           schedule_hour INTEGER, schedule_day INTEGER, last_run_at TEXT, next_run_at TEXT, updated_at TEXT);
         INSERT INTO projects VALUES ('p1', 'contoso');
         INSERT INTO sessions VALUES ('s1', 'Watch builds');
         INSERT INTO workflows (id, name, prompt, model, reasoning_effort, project_id, interval, schedule_hour, schedule_minute, enabled, last_run_at, next_run_at, mode, updated_at)
           VALUES ('w1', 'Morning triage', 'Summarize new bugs', 'gpt-x', 'high', 'p1', 'daily', 8, 30, 1, '2026-10-20T15:30:00Z', '2026-10-21T15:30:00Z', 'autopilot', '2026-10-01T00:00:00Z');
         INSERT INTO workflows (id, name, prompt, interval, enabled, cron_expression) VALUES ('w2', 'Cron job', 'Check', 'custom', 0, '0 * * * *');
         INSERT INTO workflow_runs (id, task_id, status, trigger, started_at, completed_at) VALUES ('r1', 'w1', 'completed', 'schedule', '2026-10-20T15:30:00Z', '2026-10-20T15:32:00Z');
         INSERT INTO workflow_runs (id, task_id, status, trigger, started_at, error_message) VALUES ('r2', 'w1', 'failed', 'manual', '2026-10-19T15:30:00Z', 'boom');
         INSERT INTO session_automations VALUES ('s1', 'check CI', 'minutes', 15, NULL, NULL, '2026-10-20T16:00:00Z', '2026-10-20T16:15:00Z', NULL);",
    )?;
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn fixture(path: &Path) {
        write_sample_db(path).unwrap();
    }

    #[test]
    fn loads_workflows_runs_and_session_schedules() {
        let dir = std::env::temp_dir().join(format!("cc-auto-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.db");
        fixture(&path);
        let list = load(&path).unwrap();
        assert_eq!(list.len(), 3);
        let morning = list.iter().find(|a| a.key == "wf:w1").unwrap();
        assert_eq!((morning.schedule.as_str(), morning.project.as_str(), morning.mode.as_str()), ("daily at 08:30", "contoso", "autopilot"));
        assert_eq!(morning.runs.len(), 2);
        assert_eq!(morning.last_status(), "completed", "newest run first");
        assert_eq!(morning.runs[1].error, "boom");
        let cron = list.iter().find(|a| a.key == "wf:w2").unwrap();
        assert!(!cron.enabled);
        assert_eq!(cron.schedule, "cron 0 * * * *");
        let session = list.iter().find(|a| a.key == "session:s1").unwrap();
        assert_eq!(session.schedule, "every 15 min");
        assert!(session.name.starts_with("Watch builds"));
        assert!(!session.toggleable);

        set_enabled(&path, "wf:w2", true).unwrap();
        set_enabled(&path, "wf:w1", false).unwrap();
        let list = load(&path).unwrap();
        assert!(list.iter().find(|a| a.key == "wf:w2").unwrap().enabled);
        let morning = list.iter().find(|a| a.key == "wf:w1").unwrap();
        assert!(!morning.enabled);
        assert_ne!(morning.updated_at, "2026-10-01T00:00:00Z", "updated_at refreshed");
        assert!(set_enabled(&path, "session:s1", false).is_err());
        assert!(set_enabled(&path, "wf:missing", false).is_err());
        assert_eq!(load(&dir.join("absent.db")).unwrap(), Vec::new());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
