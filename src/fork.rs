//! Forks a Copilot CLI session on disk, the way Copilot's own `/fork` does, so a
//! subsession starts with its parent's conversation without touching the
//! (possibly running) parent: the event history is copied with only the first
//! `session.start` event re-stamped, `workspace.yaml` gets the new id, name and
//! times (remote-control `mc_*` fields dropped), and `checkpoints/` is copied.
//! Like `/fork`, the session database, files and rewind snapshots are not.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

pub fn fork_session(sessions_dir: &Path, parent: &str, child: &str, name: &str, cwd: Option<&str>) -> Result<()> {
    let from = sessions_dir.join(parent);
    let to = sessions_dir.join(child);
    if !from.join("events.jsonl").is_file() {
        bail!("the parent session has no history to fork");
    }
    if to.exists() {
        bail!("session {child} already exists");
    }
    std::fs::create_dir_all(&to)?;
    let result = (|| -> Result<()> {
        let now = crate::paths::rfc3339_now();
        copy_events(&from.join("events.jsonl"), &to.join("events.jsonl"), child, &now)?;
        let yaml = std::fs::read_to_string(from.join("workspace.yaml")).unwrap_or_default();
        std::fs::write(to.join("workspace.yaml"), fork_yaml(&yaml, child, name, cwd, &now))?;
        if from.join("checkpoints").is_dir() {
            copy_dir(&from.join("checkpoints"), &to.join("checkpoints"))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&to);
    }
    result
}

/// Copies complete event lines (the parent may be writing the last one).
fn copy_events(from: &Path, to: &Path, child: &str, now: &str) -> Result<()> {
    let reader = BufReader::new(std::fs::File::open(from).context("cannot read the parent's history")?);
    let mut writer = BufWriter::new(std::fs::File::create(to)?);
    let mut first = true;
    for line in reader.split(b'\n') {
        let line = line?;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if first {
            first = false;
            let mut event: serde_json::Value = serde_json::from_slice(&line).context("unreadable first event")?;
            if event["type"] != "session.start" {
                bail!("the parent's history does not start with session.start");
            }
            event["timestamp"] = now.into();
            if let Some(data) = event["data"].as_object_mut() {
                data.insert("sessionId".into(), child.into());
                data.insert("startTime".into(), now.into());
                data.insert("alreadyInUse".into(), false.into());
                data.remove("remoteSteerable");
            }
            serde_json::to_writer(&mut writer, &event)?;
        } else if serde_json::from_slice::<serde::de::IgnoredAny>(&line).is_ok() {
            writer.write_all(&line)?;
        } else {
            // A partially written last line.
            continue;
        }
        writer.write_all(b"\n")?;
    }
    if first {
        bail!("the parent session has no history to fork");
    }
    writer.flush()?;
    Ok(())
}

fn yaml_quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"").replace(['\r', '\n'], " "))
}

fn fork_yaml(parent: &str, child: &str, name: &str, cwd: Option<&str>, now: &str) -> String {
    let mut out = Vec::new();
    let mut lines = parent.lines().peekable();
    while let Some(line) = lines.next() {
        let key = line.split(':').next().unwrap_or_default();
        let replaced = match key {
            "id" => Some(format!("id: {child}")),
            "name" => Some(format!("name: {}", yaml_quote(name))),
            "cwd" if cwd.is_some() => Some(format!("cwd: {}", cwd.unwrap_or_default())),
            "user_named" => Some("user_named: true".into()),
            "fork_count" => Some("fork_count: 0".into()),
            "created_at" => Some(format!("created_at: {now}")),
            "updated_at" => Some(format!("updated_at: {now}")),
            k if k.starts_with("mc_") => Some(String::new()),
            _ => None,
        };
        let Some(replaced) = replaced else {
            out.push(line.to_owned());
            continue;
        };
        // Drop the old value's continuation lines (block scalars such as `name: |-`).
        while lines.peek().is_some_and(|next| next.starts_with(' ') || next.starts_with('\t')) {
            lines.next();
        }
        if !replaced.is_empty() {
            out.push(replaced);
        }
    }
    for (key, value) in [("id", child.to_owned()), ("name", yaml_quote(name)), ("user_named", "true".into()), ("created_at", now.to_owned())] {
        if !out.iter().any(|l| l.split(':').next() == Some(key)) {
            out.push(format!("{key}: {value}"));
        }
    }
    out.join("\n") + "\n"
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT: &str = "11111111-1111-1111-1111-111111111111";
    const CHILD: &str = "22222222-2222-2222-2222-222222222222";

    #[test]
    fn forks_history_metadata_and_checkpoints_like_copilot() {
        let root = std::env::temp_dir().join(format!("cc-forktest-{}", uuid::Uuid::new_v4()));
        let parent = root.join(PARENT);
        std::fs::create_dir_all(parent.join("checkpoints")).unwrap();
        std::fs::write(parent.join("checkpoints").join("index.md"), "cp").unwrap();
        std::fs::write(parent.join("session.db"), "db").unwrap();
        std::fs::write(
            parent.join("workspace.yaml"),
            format!("id: {PARENT}\ncwd: C:\\repo\nname: |-\n  long\n  name\nuser_named: false\nfork_count: 2\ncreated_at: 2026-01-01T00:00:00Z\nmc_task_id: x\nbranch: main\n"),
        )
        .unwrap();
        let start = format!(r#"{{"type":"session.start","data":{{"sessionId":"{PARENT}","startTime":"2026-01-01T00:00:00Z","alreadyInUse":true,"remoteSteerable":true}},"id":"e1","timestamp":"2026-01-01T00:00:00Z","parentId":null}}"#);
        let message = r#"{"type":"user.message","data":{"content":"hello parent"},"id":"e2","parentId":"e1"}"#;
        std::fs::write(parent.join("events.jsonl"), format!("{start}\n{message}\n{{\"type\":\"assistant.mess")).unwrap();

        fork_session(&root, PARENT, CHILD, "Child \"one\"", Some(r"C:\wt")).unwrap();

        let child = root.join(CHILD);
        let events = std::fs::read_to_string(child.join("events.jsonl")).unwrap();
        let lines: Vec<&str> = events.lines().collect();
        assert_eq!(lines.len(), 2, "partial last line dropped");
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["data"]["sessionId"], CHILD);
        assert_eq!(first["data"]["alreadyInUse"], false);
        assert!(first["data"].get("remoteSteerable").is_none());
        assert_eq!(first["id"], "e1", "event ids are kept");
        assert_eq!(lines[1], message, "later events copied byte for byte");
        let yaml = std::fs::read_to_string(child.join("workspace.yaml")).unwrap();
        assert!(yaml.contains(&format!("id: {CHILD}")) && yaml.contains(r#"name: "Child \"one\"""#), "{yaml}");
        assert!(yaml.contains(r"cwd: C:\wt") && yaml.contains("user_named: true") && yaml.contains("fork_count: 0") && yaml.contains("branch: main"));
        assert!(!yaml.contains("mc_task_id") && !yaml.contains("  long") && !yaml.contains("2026-01-01"), "{yaml}");
        assert_eq!(std::fs::read_to_string(child.join("checkpoints").join("index.md")).unwrap(), "cp");
        assert!(!child.join("session.db").exists(), "like /fork, the session database is not copied");
        assert!(fork_session(&root, PARENT, CHILD, "again", None).is_err(), "never overwrites");
        assert!(std::fs::read_to_string(parent.join("events.jsonl")).unwrap().contains(PARENT), "parent untouched");
        let stamp = crate::paths::format_rfc3339_ms(1_792_705_390_216);
        assert_eq!(crate::paths::parse_rfc3339_ms(&stamp), Some(1_792_705_390_216), "{stamp}");
        let _ = std::fs::remove_dir_all(root);
    }
}
