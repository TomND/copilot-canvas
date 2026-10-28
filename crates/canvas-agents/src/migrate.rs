//! VS Code → Copilot CLI migration: MCP servers, instructions, prompt files
//! (as skills), custom agents/chat modes, and skill folders.
//!
//! `plan` is read-only and classifies every item; `apply` performs only the
//! selected *new* items, backs up Copilot config first, and never overwrites.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::copilot_cfg::{McpServer, Skill};

pub use canvas_model::migrate::{Action, Item, ItemKind, Status};

/// Removes `//` and `/* */` comments and trailing commas (VS Code JSONC).
pub fn strip_jsonc(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut in_string) = (0, false);
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else {
            out.push(c);
            i += 1;
        }
    }
    // Trailing commas before } or ].
    let mut cleaned = String::with_capacity(out.len());
    let bytes: Vec<char> = out.chars().collect();
    let mut in_string = false;
    for (index, &c) in bytes.iter().enumerate() {
        if c == '"' && (index == 0 || bytes[index - 1] != '\\') {
            in_string = !in_string;
        }
        if c == ',' && !in_string {
            let next = bytes[index + 1..].iter().find(|ch| !ch.is_whitespace());
            if matches!(next, Some('}') | Some(']')) {
                continue;
            }
        }
        cleaned.push(c);
    }
    cleaned
}

fn read_jsonc(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&strip_jsonc(&text)).ok()
}

fn split_frontmatter(text: &str) -> (HashMap<String, String>, &str) {
    let mut fields = HashMap::new();
    let Some(rest) = text.strip_prefix("---") else { return (fields, text) };
    let rest = rest.trim_start_matches(['\r', '\n']);
    let Some(end) = rest.find("\n---") else { return (fields, text) };
    for line in rest[..end].lines() {
        if let Some((key, value)) = line.split_once(':') {
            fields.insert(key.trim().to_owned(), value.trim().trim_matches(['"', '\'']).to_owned());
        }
    }
    let body = &rest[end + 4..];
    (fields, body.trim_start_matches(['\r', '\n']))
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').chars().take(64).collect()
}

fn same_file(path: &Path, content: &str) -> Option<bool> {
    let existing = std::fs::read_to_string(path).ok()?;
    Some(existing.replace("\r\n", "\n").trim() == content.replace("\r\n", "\n").trim())
}

fn mcp_item(name: &str, server: &serde_json::Value, existing: &[McpServer]) -> (Status, Option<Action>, String) {
    let flat = server.to_string();
    let transport = server.get("type").and_then(|t| t.as_str()).unwrap_or(if server.get("url").is_some() { "http" } else { "stdio" });
    let target = match server.get("url").and_then(|u| u.as_str()) {
        Some(url) => url.to_owned(),
        None => {
            let command = server.get("command").and_then(|c| c.as_str()).unwrap_or_default();
            let args: Vec<&str> = server.get("args").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
            std::iter::once(command).chain(args).collect::<Vec<_>>().join(" ")
        }
    };
    if let Some(other) = existing.iter().find(|s| s.name.eq_ignore_ascii_case(name)) {
        return if other.target == target {
            (Status::AlreadyPresent, None, target)
        } else {
            (Status::Conflict(format!("Copilot already has “{}” ({})", other.name, other.source)), None, target)
        };
    }
    for (needle, what) in [("${input:", "prompted inputs"), ("${workspaceFolder", "workspace variables"), ("${env:", "environment references"), ("envFile", "an env file")] {
        if flat.contains(needle) {
            return (Status::Manual(format!("uses {what}; add it with `copilot mcp add`")), None, target);
        }
    }
    if server.get("cwd").is_some() {
        return (Status::Manual("needs a working directory, which `copilot mcp add` cannot set".into()), None, target);
    }
    let mut args = vec!["mcp".to_owned(), "add".to_owned()];
    match transport {
        "http" | "sse" => {
            let Some(url) = server.get("url").and_then(|u| u.as_str()) else {
                return (Status::Manual("remote server without a URL".into()), None, target);
            };
            args.extend(["--transport".into(), transport.into()]);
            if let Some(headers) = server.get("headers").and_then(|h| h.as_object()) {
                for (key, value) in headers {
                    args.extend(["--header".into(), format!("{key}: {}", value.as_str().unwrap_or_default())]);
                }
            }
            args.extend([name.to_owned(), url.to_owned()]);
        }
        _ => {
            let Some(command) = server.get("command").and_then(|c| c.as_str()) else {
                return (Status::Manual("local server without a command".into()), None, target);
            };
            if let Some(env) = server.get("env").and_then(|e| e.as_object()) {
                for (key, value) in env {
                    args.extend(["--env".into(), format!("{key}={}", value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string()))]);
                }
            }
            args.extend([name.to_owned(), "--".into(), command.to_owned()]);
            if let Some(list) = server.get("args").and_then(|a| a.as_array()) {
                args.extend(list.iter().map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())));
            }
        }
    }
    (Status::New, Some(Action::Cli(args)), target)
}

/// Directories (with their product label) that hold VS Code user data.
pub fn vscode_user_dirs() -> Vec<(String, PathBuf)> {
    let Some(appdata) = dirs::config_dir() else { return Vec::new() };
    let mut dirs = Vec::new();
    for product in ["Code", "Code - Insiders"] {
        let user = appdata.join(product).join("User");
        if !user.is_dir() {
            continue;
        }
        let label = if product.contains("Insiders") { "Insiders" } else { "Code" };
        dirs.push((label.to_owned(), user.clone()));
        if let Ok(profiles) = std::fs::read_dir(user.join("profiles")) {
            for profile in profiles.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
                let path = profile.path();
                if path.join("mcp.json").is_file() || path.join("prompts").is_dir() || path.join("settings.json").is_file() {
                    dirs.push((format!("{label} profile {}", profile.file_name().to_string_lossy()), path));
                }
            }
        }
    }
    dirs
}

fn user_locations(settings: &serde_json::Value, key: &str) -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let expand = |p: &str| -> Option<PathBuf> {
        let path = if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) { home.join(rest) } else { PathBuf::from(p) };
        path.is_absolute().then_some(path)
    };
    match settings.get(key) {
        Some(serde_json::Value::Object(map)) => map.iter().filter(|(_, on)| on.as_bool() != Some(false)).filter_map(|(p, _)| expand(p)).collect(),
        Some(serde_json::Value::Array(list)) => list.iter().filter_map(|v| v.as_str()).filter_map(expand).collect(),
        _ => Vec::new(),
    }
}

/// Builds the migration preview. Read-only.
pub fn plan(sources: &[(String, PathBuf)], copilot_home: &Path, existing_mcp: &[McpServer], existing_skills: &[Skill]) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    let mut push = |mut item: Item| {
        if let Some(index) = items.iter().position(|i| i.kind == item.kind && i.name.eq_ignore_ascii_case(&item.name)) {
            let same = {
                let existing = &items[index];
                existing.action == item.action || (existing.target == item.target && existing.status == item.status)
            };
            if same {
                let existing = &mut items[index];
                if !existing.products.contains(&item.products) {
                    existing.products = format!("{}, {}", existing.products, item.products);
                }
            } else {
                let products = items[index].products.clone();
                item.status = Status::Conflict(format!("differs from the {products} copy"));
                item.action = None;
                items.push(item);
            }
            return;
        }
        items.push(item);
    };
    for (product, dir) in sources {
        if let Some(mcp) = read_jsonc(&dir.join("mcp.json")) {
            for (name, server) in mcp.get("servers").and_then(|s| s.as_object()).into_iter().flatten() {
                let (status, action, target) = mcp_item(name, server, existing_mcp);
                push(Item { kind: ItemKind::Mcp, name: name.clone(), products: product.clone(), source: dir.join("mcp.json").display().to_string(), target, status, action });
            }
        }
        let settings = read_jsonc(&dir.join("settings.json")).unwrap_or(serde_json::Value::Null);
        let mut prompt_dirs = vec![dir.join("prompts")];
        prompt_dirs.extend(user_locations(&settings, "chat.instructionsFilesLocations"));
        prompt_dirs.extend(user_locations(&settings, "chat.promptFilesLocations"));
        prompt_dirs.extend(user_locations(&settings, "chat.modeFilesLocations"));
        prompt_dirs.extend(user_locations(&settings, "chat.agentFilesLocations"));
        let mut seen = std::collections::HashSet::new();
        for prompt_dir in prompt_dirs {
            let Ok(entries) = std::fs::read_dir(&prompt_dir) else { continue };
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if !seen.insert(path.clone()) || !path.is_file() {
                    continue;
                }
                let file = entry.file_name().to_string_lossy().into_owned();
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let source = path.display().to_string();
                if file.ends_with(".instructions.md") {
                    let target = copilot_home.join("instructions").join(&file);
                    let (status, action) = match same_file(&target, &text) {
                        Some(true) => (Status::AlreadyPresent, None),
                        Some(false) => (Status::Conflict("a different file with this name exists".into()), None),
                        None => (Status::New, Some(Action::Write { path: target.clone(), content: text.clone() })),
                    };
                    push(Item { kind: ItemKind::Instruction, name: file.trim_end_matches(".instructions.md").to_owned(), products: product.clone(), source, target: target.display().to_string(), status, action });
                } else if file.ends_with(".prompt.md") {
                    let (fields, body) = split_frontmatter(&text);
                    let base = file.trim_end_matches(".prompt.md");
                    let name = slug(fields.get("name").map(String::as_str).unwrap_or(base));
                    let description = fields
                        .get("description")
                        .cloned()
                        .filter(|d| !d.is_empty())
                        .unwrap_or_else(|| format!("Migrated from the VS Code prompt file {file}"));
                    let content = format!("---\nname: {name}\ndescription: {}\n---\n\n{body}", description.replace('\n', " "));
                    let target = copilot_home.join("skills").join(&name).join("SKILL.md");
                    let already = existing_skills.iter().any(|s| s.name.eq_ignore_ascii_case(&name));
                    let (status, action) = match same_file(&target, &content) {
                        Some(true) => (Status::AlreadyPresent, None),
                        Some(false) => (Status::Conflict("a skill with this name exists".into()), None),
                        None if already => (Status::Conflict("a skill with this name exists".into()), None),
                        None => (Status::New, Some(Action::Write { path: target.clone(), content })),
                    };
                    push(Item { kind: ItemKind::PromptSkill, name, products: product.clone(), source, target: target.display().to_string(), status, action });
                } else if file.ends_with(".agent.md") || file.ends_with(".chatmode.md") {
                    let base = file.trim_end_matches(".agent.md").trim_end_matches(".chatmode.md");
                    let target = copilot_home.join("agents").join(format!("{base}.agent.md"));
                    let (status, action) = match same_file(&target, &text) {
                        Some(true) => (Status::AlreadyPresent, None),
                        Some(false) => (Status::Conflict("a different agent with this name exists".into()), None),
                        None => (Status::New, Some(Action::Write { path: target.clone(), content: text.clone() })),
                    };
                    push(Item { kind: ItemKind::Agent, name: base.to_owned(), products: product.clone(), source, target: target.display().to_string(), status, action });
                }
            }
        }
        for skill_dir in user_locations(&settings, "chat.agentSkillsLocations") {
            if !skill_dir.is_dir() {
                continue;
            }
            let text = skill_dir.display().to_string();
            let registered = existing_skills.iter().any(|s| Path::new(&s.path).starts_with(&skill_dir))
                || skill_dir.starts_with(copilot_home.join("skills"));
            let (status, action) = if registered {
                (Status::AlreadyPresent, None)
            } else {
                (Status::New, Some(Action::Cli(vec!["skill".into(), "add".into(), text.clone()])))
            };
            push(Item { kind: ItemKind::SkillFolder, name: text.clone(), products: product.clone(), source: "settings.json".into(), target: text, status, action });
        }
    }
    items.sort_by_key(|i| (i.kind as u8, !matches!(i.status, Status::New), i.name.to_lowercase()));
    items
}

#[derive(Debug)]
pub struct Outcome {
    pub name: String,
    pub result: Result<String, String>,
}

/// Applies the chosen items. Copilot's config files are backed up first;
/// files are only created, never replaced.
pub fn apply(items: &[&Item], copilot_home: &Path, backup_root: &Path, run_cli: &dyn Fn(&[String]) -> Result<String>) -> Result<(PathBuf, Vec<Outcome>)> {
    let stamp = chrono_stamp();
    let backup = backup_root.join(format!("copilot-config-{stamp}"));
    std::fs::create_dir_all(&backup).context("cannot create the backup folder")?;
    for file in ["mcp-config.json", "settings.json", "config.json"] {
        let source = copilot_home.join(file);
        if source.is_file() {
            std::fs::copy(&source, backup.join(file)).with_context(|| format!("cannot back up {file}"))?;
        }
    }
    let mut outcomes = Vec::new();
    let mut manifest = Vec::new();
    for item in items {
        let result = match &item.action {
            _ if item.status != Status::New => Err("not a new item".to_owned()),
            None => Err("nothing to do".to_owned()),
            Some(Action::Write { path, content }) => (|| -> Result<String> {
                anyhow::ensure!(!path.exists(), "{} already exists", path.display());
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
                std::io::Write::write_all(&mut file, content.as_bytes())?;
                manifest.push(format!("created {}", path.display()));
                Ok(format!("created {}", path.display()))
            })()
            .map_err(|e| format!("{e:#}")),
            Some(Action::Cli(args)) => run_cli(args)
                .map(|out| {
                    manifest.push(format!("ran copilot {}", redact(args)));
                    out.lines().last().unwrap_or("done").to_owned()
                })
                .map_err(|e| format!("{e:#}")),
        };
        outcomes.push(Outcome { name: format!("{} {}", item.kind.label(), item.name), result });
    }
    std::fs::write(backup.join("migration-log.txt"), manifest.join("\n"))?;
    Ok((backup, outcomes))
}

/// Hides environment values and header values in logs.
pub fn redact(args: &[String]) -> String {
    let mut out = Vec::new();
    let mut hide_next = false;
    for arg in args {
        if hide_next {
            let key = arg.split(['=', ':']).next().unwrap_or("");
            out.push(format!("{key}=***"));
            hide_next = false;
        } else {
            hide_next = arg == "--env" || arg == "--header";
            out.push(arg.clone());
        }
    }
    out.join(" ")
}

fn chrono_stamp() -> String {
    let ms = crate::paths::now_ms();
    format!("{}-{:03}", ms / 1000, ms % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_comments_and_trailing_commas_are_removed() {
        let text = "{\n // comment\n \"a\": \"http://x//y\", /* block */ \"b\": [1,2,],\n}";
        let value: serde_json::Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(value["a"], "http://x//y");
        assert_eq!(value["b"], serde_json::json!([1, 2]));
    }

    #[test]
    fn plans_and_applies_without_overwriting() {
        let root = std::env::temp_dir().join(format!("cc-migrate-{}", uuid::Uuid::new_v4()));
        let code = root.join("Code").join("User");
        let insiders = root.join("Insiders").join("User");
        let home = root.join("copilot");
        for dir in [code.join("prompts"), insiders.join("prompts"), home.join("instructions"), root.join("skills-extra").join("one")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(
            code.join("mcp.json"),
            r#"{ // servers
              "servers": {
                "new-local": {"type": "stdio", "command": "node", "args": ["s.js"], "env": {"TOKEN": "secret"}},
                "new-remote": {"type": "http", "url": "https://x.example/mcp"},
                "same": {"type": "stdio", "command": "kusto", "args": ["--a"]},
                "clash": {"type": "stdio", "command": "other"},
                "prompted": {"type": "stdio", "command": "x", "env": {"K": "${input:key}"}},
              }
            }"#,
        )
        .unwrap();
        std::fs::write(insiders.join("mcp.json"), r#"{"servers": {"same": {"type": "stdio", "command": "kusto", "args": ["--a"]}}}"#).unwrap();
        std::fs::write(code.join("prompts").join("team.instructions.md"), "be nice").unwrap();
        std::fs::write(home.join("instructions").join("team.instructions.md"), "be nice\n").unwrap();
        std::fs::write(code.join("prompts").join("fresh.instructions.md"), "new rules").unwrap();
        std::fs::write(code.join("prompts").join("Fix Bug.prompt.md"), "---\ndescription: Fix a bug\n---\nFind and fix it.").unwrap();
        std::fs::write(code.join("prompts").join("planner.chatmode.md"), "---\ndescription: plan\n---\nPlan.").unwrap();
        let skills_dir = root.join("skills-extra");
        std::fs::write(code.join("settings.json"), format!("{{\"chat.agentSkillsLocations\": {{\"{}\": true}}}}", skills_dir.display().to_string().replace('\\', "\\\\"))).unwrap();
        let existing = vec![
            McpServer { name: "same".into(), source: "user".into(), transport: "stdio".into(), target: "kusto --a".into(), enabled: true },
            McpServer { name: "clash".into(), source: "plugin: p".into(), transport: "stdio".into(), target: "different".into(), enabled: true },
        ];
        let sources = vec![("Code".to_owned(), code.clone()), ("Insiders".to_owned(), insiders.clone())];
        let items = plan(&sources, &home, &existing, &[]);
        let find = |kind: ItemKind, name: &str| items.iter().find(|i| i.kind == kind && i.name == name).unwrap_or_else(|| panic!("{name} missing: {items:#?}"));
        assert_eq!(find(ItemKind::Mcp, "new-local").status, Status::New);
        assert_eq!(find(ItemKind::Mcp, "new-remote").action, Some(Action::Cli(vec!["mcp".into(), "add".into(), "--transport".into(), "http".into(), "new-remote".into(), "https://x.example/mcp".into()])));
        assert_eq!(find(ItemKind::Mcp, "same").status, Status::AlreadyPresent);
        assert_eq!(find(ItemKind::Mcp, "same").products, "Code, Insiders");
        assert!(matches!(find(ItemKind::Mcp, "clash").status, Status::Conflict(_)));
        assert!(matches!(find(ItemKind::Mcp, "prompted").status, Status::Manual(_)));
        assert_eq!(find(ItemKind::Instruction, "team").status, Status::AlreadyPresent);
        assert_eq!(find(ItemKind::Instruction, "fresh").status, Status::New);
        assert_eq!(find(ItemKind::PromptSkill, "fix-bug").status, Status::New);
        assert_eq!(find(ItemKind::Agent, "planner").status, Status::New);
        assert_eq!(find(ItemKind::SkillFolder, &skills_dir.display().to_string()).status, Status::New);

        let calls = std::cell::RefCell::new(Vec::new());
        let runner = |args: &[String]| -> Result<String> {
            calls.borrow_mut().push(args.to_vec());
            Ok("added".into())
        };
        // Someone creates the agent between preview and apply: it must not be overwritten.
        std::fs::create_dir_all(home.join("agents")).unwrap();
        std::fs::write(home.join("agents").join("planner.agent.md"), "user's own").unwrap();
        let chosen: Vec<&Item> = items.iter().filter(|i| i.status == Status::New).collect();
        let (backup, outcomes) = apply(&chosen, &home, &root.join("backups"), &runner).unwrap();
        assert!(backup.join("migration-log.txt").is_file());
        assert_eq!(std::fs::read_to_string(home.join("agents").join("planner.agent.md")).unwrap(), "user's own");
        assert!(outcomes.iter().any(|o| o.name.contains("planner") && o.result.is_err()));
        let skill = std::fs::read_to_string(home.join("skills").join("fix-bug").join("SKILL.md")).unwrap();
        assert!(skill.starts_with("---\nname: fix-bug\ndescription: Fix a bug\n---"));
        assert!(skill.contains("Find and fix it."));
        assert_eq!(std::fs::read_to_string(home.join("instructions").join("fresh.instructions.md")).unwrap(), "new rules");
        assert_eq!(calls.borrow().len(), 3, "two MCP adds and one skill folder");
        let log = std::fs::read_to_string(backup.join("migration-log.txt")).unwrap();
        assert!(log.contains("TOKEN=***") && !log.contains("secret"), "secrets are redacted: {log}");
        std::fs::remove_dir_all(root).unwrap();
    }
}
