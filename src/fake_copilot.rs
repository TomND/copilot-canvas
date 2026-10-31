//! A stand-in Copilot CLI for the GUI parity suite: `copilot-canvas.exe`
//! copied as `copilot.exe` (or `opencode.exe` for the suite's OpenCode
//! boxes) and run with `COPILOT_CANVAS_FAKE_COPILOT=1`
//! (both are required). It answers the configuration listings the canvas
//! reads (two MCP servers, no plugins or skills) and, started as a session,
//! draws a Copilot-like screen (your message, a reply, an input line) that
//! echoes keys until Ctrl+D or Ctrl+C. Nothing is sent anywhere.

use std::io::Write;

use anyhow::Result;

/// The environment variable that turns a `copilot.exe` copy into the fake.
pub const ENV: &str = "COPILOT_CANVAS_FAKE_COPILOT";
/// The MCP servers it lists.
pub const MCP_SERVERS: [&str; 2] = ["github", "playwright"];
/// The models its `--help` offers (`opencode models` lists them with a provider).
pub const MODELS: [&str; 3] = ["claude-opus-5.5", "gpt-parity-5", "claude-sonnet-parity"];
/// Your message as the fake shows it.
pub const USER_MESSAGE: &str = "Parity check: tint my message";

/// This process is the fake (its name is `copilot`, or `opencode` / `claude`
/// for the parity suite's other agents, and the variable is set).
pub fn active() -> bool {
    std::env::var_os(ENV).is_some()
        && std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| ["copilot", "opencode", "claude"].iter().any(|n| s.to_string_lossy().eq_ignore_ascii_case(n))))
            .unwrap_or(false)
}

pub fn run(args: &[String]) -> Result<()> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = match words.as_slice() {
        ["mcp", "list", ..] => format!("{{\"mcpServers\":{{\"{}\":{{\"command\":\"gh-mcp\"}},\"{}\":{{\"command\":\"npx\",\"args\":[\"@playwright/mcp\"]}}}}}}", MCP_SERVERS[0], MCP_SERVERS[1]),
        ["plugin" | "skill" | "instruction", "list", ..] => "[]".into(),
        ["config", "--list", ..] => "{\"model\":\"fake-model\",\"mouse\":true}".into(),
        ["config", ..] | ["plugin" | "skill" | "mcp", ..] => String::new(),
        ["--version"] => "0.0.0 (canvas parity fake)".into(),
        ["--help"] => format!(
            "Usage: copilot [options]\n\nOptions:\n  --model <model>  Set the AI model to use (choices: {})\n  --allow-all      Allow all tools\n",
            MODELS.iter().map(|m| format!("\"{m}\"")).collect::<Vec<_>>().join(", ")
        ),
        ["models", ..] => MODELS.iter().map(|m| format!("github-copilot/{m}")).collect::<Vec<_>>().join("\n"),
        // A passive run: Copilot and Claude Code `-p PROMPT`, OpenCode `run PROMPT`.
        ["run", ..] => return headless(args, args.get(1).cloned().unwrap_or_default()),
        _ if args.iter().any(|a| a == "-p") => return headless(args, args.windows(2).find(|w| w[0] == "-p").map(|w| w[1].clone()).unwrap_or_default()),
        _ => return session(args),
    };
    println!("{out}");
    Ok(())
}

/// The skills a run's plugins carry: each `--plugin-dir`'s `skills/` (Copilot,
/// Claude Code) and OpenCode's `skills.paths`, without the canvas's own.
pub fn plugin_skills(args: &[String], opencode_config: Option<&str>) -> Vec<String> {
    let mut folders: Vec<std::path::PathBuf> = args.windows(2).filter(|w| w[0] == "--plugin-dir").map(|w| std::path::Path::new(&w[1]).join("skills")).collect();
    if let Some(config) = opencode_config.and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok()) {
        folders.extend(config["skills"]["paths"].as_array().into_iter().flatten().filter_map(|p| p.as_str()).map(std::path::PathBuf::from));
    }
    let mut skills: Vec<String> = folders
        .iter()
        .filter_map(|f| std::fs::read_dir(f).ok())
        .flat_map(|d| d.flatten().filter(|e| e.path().join("SKILL.md").is_file()).map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>())
        .filter(|s| !s.starts_with("canvas-"))
        .collect();
    skills.sort();
    skills.dedup();
    skills
}

/// The trigger events of a run's prompt: (type, data) each.
pub fn triggers(prompt: &str) -> Vec<(String, serde_json::Value)> {
    let Some(start) = prompt.find("Trigger (").and_then(|at| prompt[at..].find("):\n").map(|i| at + i + 3)) else { return Vec::new() };
    let end = prompt[start..].find("\n\nWhen finished:").map(|i| start + i).unwrap_or(prompt.len());
    let items: Vec<serde_json::Value> = serde_json::from_str(&prompt[start..end]).unwrap_or_default();
    items
        .into_iter()
        .filter_map(|item| {
            let kind = item["type"].as_str()?.to_owned();
            let data = item["data"].as_str().and_then(|d| serde_json::from_str(d).ok()).unwrap_or(serde_json::Value::Null);
            Some((kind, data))
        })
        .collect()
}

/// A conversation id as a file name (the stand-in `outbox` channel).
pub fn outbox_name(conversation: &str) -> String {
    conversation.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// What the stand-in does for its triggers, as an agent following the
/// canvas skills would: a follow-up for each new-comments or failed-checks
/// event of a tracked pull request, with its dispatch key (canvas-prs); a
/// draft for a completed review, sending an approved one through the
/// `outbox` channel (a folder: `<conversation>.out.jsonl`), and reading the
/// authors' replies (`<conversation>.in.jsonl`) for review requests on a
/// check (canvas-review-followup). Answers what it did.
fn follow_skills(home: &std::path::Path, prompt: &str) -> Vec<String> {
    use canvas_agents::reviews_cli::{self, MessageArgs};
    let mut done = Vec::new();
    let mut note = |what: &str, result: anyhow::Result<()>| match result {
        Ok(()) => done.push(what.to_owned()),
        Err(error) => done.push(format!("{what} refused: {error:#}")),
    };
    for (kind, data) in triggers(prompt) {
        let text = |key: &str| data[key].as_str().unwrap_or_default().to_owned();
        match kind.as_str() {
            "pr.comment.new" | "pr.checks.failed" => {
                let message = if kind == "pr.comment.new" {
                    let links: Vec<&str> = data["comments"].as_array().into_iter().flatten().filter_map(|c| c["url"].as_str()).take(3).collect();
                    format!("Review and address the {} new review comment(s) on {}: {}", data["count"].as_u64().unwrap_or(0), text("short"), links.join(" "))
                } else {
                    let names: Vec<&str> = data["failed"].as_array().into_iter().flatten().filter_map(|c| c["name"].as_str()).collect();
                    format!("Investigate the failing check(s) {} and fix them", names.join(", "))
                };
                note(&format!("dispatched {kind}"), canvas_agents::prs_cli::dispatch(home, &text("url"), data["dispatch_key"].as_str().map(str::to_owned), Some(kind.clone()), &message));
            }
            "review.completed" => {
                let results: String = text("results").chars().take(200).collect();
                let draft = format!("Hi {}, I finished reviewing {}: {}. {results}", text("author"), text("url"), text("outcome"));
                let file = std::path::PathBuf::from(format!("draft-{}.md", text("review")));
                let written = std::fs::write(&file, draft).map_err(anyhow::Error::from);
                note("drafted", written.and_then(|()| reviews_cli::message(home, &text("review"), MessageArgs { draft: Some(file.display().to_string()), ..Default::default() })));
            }
            "review.message.approved" if text("channel") == "outbox" => {
                let conversation = format!("outbox:{}", if text("author_id").is_empty() { text("author") } else { text("author_id") });
                let line = serde_json::json!({ "to": text("author"), "text": text("draft") }).to_string();
                let path = std::path::Path::new(&text("tool")).join(format!("{}.out.jsonl", outbox_name(&conversation)));
                let sent = std::fs::OpenOptions::new().create(true).append(true).open(&path).and_then(|mut f| writeln!(f, "{line}")).map_err(anyhow::Error::from);
                note("sent", sent.and_then(|()| reviews_cli::message(home, &text("review"), MessageArgs { sent: true, conversation: Some(conversation), recipient: Some(text("author")), ..Default::default() })));
            }
            "review.message.approved" => note("needs you", reviews_cli::message(home, &text("review"), MessageArgs { needs_you: Some(format!("the stand-in agent has no {} channel", text("channel"))), ..Default::default() })),
            "review.check_due" => {
                for c in data["conversations"].as_array().cloned().unwrap_or_default() {
                    let id = c["conversation"].as_str().unwrap_or_default().to_owned();
                    let since = c["since"].as_str().unwrap_or_default().to_owned();
                    let path = std::path::Path::new(&text("tool")).join(format!("{}.in.jsonl", outbox_name(&id)));
                    let replies: Vec<serde_json::Value> = std::fs::read_to_string(&path).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
                    // Only the author's messages after the last check's mark.
                    let start = replies.iter().position(|m| m["id"] == since.as_str()).map(|i| i + 1).unwrap_or(0);
                    let mut mark = since.clone();
                    for message in &replies[start.min(replies.len())..] {
                        let (message_id, words) = (message["id"].as_str().unwrap_or_default(), message["text"].as_str().unwrap_or_default());
                        if words.to_lowercase().contains("review") {
                            note("requested", reviews_cli::request(home, Some(id.clone()), Some(message_id.to_owned()), Some("re-review".into()), Some(words.to_owned()), None));
                        }
                        mark = message_id.to_owned();
                    }
                    note("checked", reviews_cli::checked(home, Some(id), (!mark.is_empty()).then_some(mark), None));
                }
            }
            _ => {}
        }
    }
    done
}

/// A passive run of the stand-in: after a moment (so the canvas sees it
/// working) it does what the canvas skills ask for its triggers
/// ([`follow_skills`]) and reports `ok` with what it got, the trigger's event
/// types, the skills its plugins carry and what it did, through the canvas's
/// own `status` command (as an agent following the prompt would), and
/// appends the run to `fake-runs.log` in the folder it runs in. Nothing is
/// sent anywhere.
fn headless(args: &[String], prompt: String) -> Result<()> {
    let wait = std::env::var("COPILOT_CANVAS_FAKE_RUN_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1_500);
    std::thread::sleep(std::time::Duration::from_millis(wait));
    let kinds: Vec<&str> = prompt.split("\"type\": \"").skip(1).filter_map(|rest| rest.split('"').next()).collect();
    let skills = plugin_skills(args, std::env::var("OPENCODE_CONFIG_CONTENT").ok().as_deref());
    let home = std::env::var_os("COPILOT_CANVAS_HOME").map(std::path::PathBuf::from);
    let did = home.as_deref().map(|h| follow_skills(h, &prompt)).unwrap_or_default();
    let mut headline = format!("fake run of {} · skills: {}", if kinds.is_empty() { "nothing".to_owned() } else { kinds.join(",") }, if skills.is_empty() { "none".to_owned() } else { skills.join(",") });
    if !did.is_empty() {
        headline.push_str(&format!(" · {}", did.join("; ")));
    }
    let first = prompt.lines().next().unwrap_or_default().to_owned();
    if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open("fake-runs.log") {
        let _ = writeln!(log, "{headline} | {first}");
    }
    if let Some(home) = home {
        canvas_agents::cli::status(&home, "ok", &headline)?;
    }
    println!("{headline}");
    Ok(())
}

/// The model a session was started with (`--model` or `-m`), as the fake shows it.
pub fn model_line(args: &[String]) -> String {
    let model = args.windows(2).find(|w| w[0] == "--model" || w[0] == "-m").map(|w| w[1].as_str());
    format!("Model: {}", model.unwrap_or("none"))
}

/// A Copilot-like screen in mouse mode that echoes what you type and
/// reports mouse clicks (`MOUSE <report>`), reading raw VT input as Copilot does.
fn session(args: &[String]) -> Result<()> {
    use std::io::Read;
    canvas_sys::console::raw_vt_modes();
    let mut out = std::io::stdout();
    let disabled: Vec<&str> = args.windows(2).filter(|w| w[0] == "--disable-mcp-server").map(|w| w[1].as_str()).collect();
    // Mouse reports (SGR), as Copilot asks for them.
    write!(out, "\x1b[?1000h\x1b[?1006h")?;
    write!(out, "Fake Copilot (canvas parity)\r\n")?;
    write!(out, "MCP off: {} · {}\r\n\r\n", if disabled.is_empty() { "none".to_owned() } else { disabled.join(",") }, model_line(args))?;
    write!(out, " ❯ {USER_MESSAGE}\r\n   and its second line\r\n\r\n")?;
    write!(out, " ● Fake reply: nothing was sent anywhere.\r\n\r\n")?;
    write!(out, "❯ ")?;
    out.flush()?;
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 4096];
    loop {
        let n = stdin.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        if text.contains('\u{4}') || text.contains('\u{3}') {
            break;
        }
        if let Some(at) = text.find("\x1b[<") {
            let report: String = text[at + 3..].chars().take_while(|c| c.is_ascii_digit() || *c == ';' || *c == 'M' || *c == 'm').collect();
            write!(out, "\r\nMOUSE {report}\r\n❯ ")?;
        } else if !text.starts_with('\x1b') {
            write!(out, "{}", text.replace('\r', "\r\n❯ "))?;
        }
        out.flush()?;
    }
    write!(out, "\x1b[?1000l\x1b[?1006l")?;
    out.flush()?;
    Ok(())
}

