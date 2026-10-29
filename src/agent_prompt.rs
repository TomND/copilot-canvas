//! `copilot-canvas selftest agent --agent opencode --prompt`: OpenCode driven
//! through the canvas with tiny prompts to a free model.
//!
//! The canvas creates the sessions (Settings defaults, the new-session
//! dialog), and every check reads what a person sees (box and tile labels,
//! the terminal) or what the service recorded (its log, the canvas file, the
//! event bus). Prompts go only to a model whose id ends in `-free`, and only
//! into a TUI whose command line carries `-m` with that model. Every OpenCode
//! session the check creates is deleted at the end.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use canvas_agents::agents::{self, AgentKind, Provide, opencode_history};
use rusqlite::OptionalExtension;

use crate::client::doc::{Doc, Group, Node, NodeKind, Passive};
use crate::paths::{Instance, now_ms};
use crate::selftest::{Client, Console, GuiDriver, SIDEBAR, Ui, agent_ui_up, load, step, wait_doc};

/// The model unless `--model` names another free one.
pub const DEFAULT_MODEL: &str = "opencode/deepseek-v4.1-flash-free";
/// Optional steps (`--steps` picks some; the first prompt always runs).
/// `busystop` / `streamstop` (not in the default list) stop the box while a
/// tool runs / while the answer streams, instead of when it is idle.
pub const STEPS: [&str; 7] = ["wheel", "push", "subagent", "question", "resume", "permission", "passive"];
const EXTRA_STEPS: [&str; 2] = ["busystop", "streamstop"];

const MAIN: &str = "Canvas prompt check";
const ASK: &str = "Canvas permission check";
const PASSIVE: &str = "Canvas passive check";
const PASSIVE_GROUP: &str = "Passive check";
/// Columns right of the canvas: terminal tiles and detail panels.
const RIGHT: (u16, u16) = (95, 170);
/// How long one prompt may take with a free model.
const TURN: Duration = Duration::from_secs(240);

const PROMPT_STATUS: &str = "Use the bash tool to run: sleep 6 ; then reply with just OK";
const PROMPT_LONG: &str = "Use the bash tool to run: sleep 40 ; then reply with just OK";
const PROMPT_STREAM: &str = "Reply with 400 lines: the word canvasline immediately followed by the line number, from 1 to 400, one per line, nothing else.";
const PROMPT_ROWS: &str = "Reply with 80 lines: the word canvasrow immediately followed by the line number, from 1 to 80, one per line, nothing else.";
/// `;` separates commands in every shell OpenCode may run (bash, PowerShell 5.1 has no `&&`).
const PROMPT_PUSH: &str = "Use the bash tool once to run this exact command: `cd repo; git push origin main` and then reply with just DONE";
const PROMPT_TASK: &str = "Use the task tool once to start a general subagent with the prompt: Reply with just OK. When it returns, reply with just DONE.";
const PROMPT_QUESTION: &str = "Use the question tool once to ask me: Which canvas color? with the options Red and Blue. After my answer, reply with just DONE.";
const PROMPT_ASK: &str = "Use the bash tool to run: echo canvasask ; then reply with just DONE";

pub struct Options {
    pub model: Option<String>,
    pub client: Client,
    pub steps: Option<Vec<String>>,
}

/// Refuses every model whose id does not end in `-free`.
pub fn ensure_free(model: &str) -> Result<()> {
    anyhow::ensure!(
        !model.is_empty() && model.trim() == model && !model.starts_with('-') && model.ends_with("-free"),
        "refusing to send prompts to {model:?}: only free models (ids ending in -free) may get prompts"
    );
    Ok(())
}

/// The `-m` / `--model` value of a command line.
fn model_arg(command_line: &str) -> Option<&str> {
    let mut words = command_line.split_whitespace();
    while let Some(word) = words.next() {
        if word == "-m" || word == "--model" {
            return words.next().map(|w| w.trim_matches('"'));
        }
    }
    None
}

/// Whether a prompt may be typed into a TUI started with `command_line`.
pub(crate) fn may_prompt(command_line: &str, model: &str) -> Result<()> {
    ensure_free(model)?;
    match model_arg(command_line) {
        Some(arg) if arg == model => Ok(()),
        other => bail!("refusing to submit a prompt: the TUI runs with model {other:?}, not {model}: {command_line}"),
    }
}

/// What OpenCode started by this check sees besides the canvas's settings:
/// no network refresh or update, tools that ask unless the session runs with
/// `--auto`, and the free model as every default (title generation and
/// subagents included) in case a launch lacked `-m`.
pub(crate) fn opencode_env(model: &str) -> Vec<(&'static str, String)> {
    let config = serde_json::json!({
        "model": model,
        "small_model": model,
        "agent": { "general": { "model": model }, "explore": { "model": model } },
    });
    vec![
        ("OPENCODE_DISABLE_MODELS_FETCH", "1".into()),
        ("OPENCODE_DISABLE_AUTOUPDATE", "1".into()),
        ("OPENCODE_PERMISSION", r#"{"bash":"ask"}"#.into()),
        ("OPENCODE_CONFIG_CONTENT", config.to_string()),
    ]
}

struct Paths {
    root: PathBuf,
    work: PathBuf,
    home: PathBuf,
    group: PathBuf,
    remote: PathBuf,
}

pub fn run(options: Options) -> Result<()> {
    let model = options.model.clone().unwrap_or_else(|| DEFAULT_MODEL.to_owned());
    ensure_free(&model)?;
    let steps: Vec<String> = options.steps.clone().unwrap_or_else(|| STEPS.iter().map(|s| (*s).to_owned()).collect());
    for name in &steps {
        anyhow::ensure!(STEPS.contains(&name.as_str()) || EXTRA_STEPS.contains(&name.as_str()), "unknown step {name:?} (steps: {}, {})", STEPS.join(", "), EXTRA_STEPS.join(", "));
    }
    let program = AgentKind::OpenCode.provider().program().context("OpenCode is not installed on this machine")?;
    // SAFETY: no other thread runs yet. An enclosing OpenCode (this check
    // started from an OpenCode session) must not leak into the ones it starts.
    unsafe {
        std::env::remove_var("OPENCODE");
        std::env::remove_var("OPENCODE_PID");
    }
    let env = opencode_env(&model);
    let cli_env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let listed = agents::config::run(&program, &["models", "opencode"], &cli_env[..2], Duration::from_secs(90))?;
    anyhow::ensure!(listed.lines().any(|l| l.trim() == model), "{model} is not one of OpenCode's models (`opencode models opencode`)");

    let root = std::env::temp_dir().join(format!("copilot-canvas-prompt-{}", uuid::Uuid::new_v4()));
    let paths = Paths { work: root.join("work"), home: root.join("home"), group: root.join("passive-group"), remote: root.join("remote.git"), root };
    prepare(&paths)?;
    let instance = Instance::new(Some(format!("ocprompt{}", std::process::id())), Some(paths.home.clone()), None)?;
    println!("Copilot Canvas real-agent check with prompts: OpenCode ({program}), model {model} (free); steps: status{}", steps.iter().map(|s| format!(", {s}")).collect::<String>());
    let started_ms = now_ms();
    let ui_env: Vec<(&str, PathBuf)> = env.iter().map(|(k, v)| (*k, PathBuf::from(v))).collect();
    let ui_env: Vec<(&str, &Path)> = ui_env.iter().map(|(k, v)| (*k, v.as_path())).collect();
    let exe = std::env::current_exe()?;
    let result = match options.client {
        Client::Tui => scenario(&instance, &paths, &model, &steps, &ui_env, |args, env| Console::start_with_env(&exe, args, env, 170, 46)),
        Client::Gui => {
            let log = paths.root.join("gui-stderr.log");
            scenario(&instance, &paths, &model, &steps, &ui_env, |args, env| GuiDriver::start(args, env, (170, 46), &log))
        }
    };
    if let Err(error) = &result {
        eprintln!("FAILED: {error:#}");
        eprintln!("--- service log (tail) ---\n{}", tail(&std::fs::read_to_string(instance.log_path()).unwrap_or_default(), 40));
    }
    if let Ok(conn) = crate::client::conn::try_connect(&instance) {
        let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
        std::thread::sleep(Duration::from_secs(7));
    }
    let cleanup = cleanup(&program, &cli_env, &model, &[&paths.work, &paths.group], started_ms);
    if std::env::var_os("CANVAS_SELFTEST_KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&paths.root);
    } else {
        eprintln!("artifacts kept in {}", paths.root.display());
    }
    result.and(cleanup)?;
    println!("PASS");
    Ok(())
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// The work folder (with a Git repository whose `origin` is a local bare
/// repository), the canvas home (OpenCode as the agent, a group with a
/// passive OpenCode session) and the passive session's rules.
fn prepare(paths: &Paths) -> Result<()> {
    let repo = paths.work.join("repo");
    for dir in [&repo, &paths.home, &paths.group] {
        std::fs::create_dir_all(dir)?;
    }
    let git = |dir: &Path, args: &[&str]| -> Result<()> {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output()?;
        anyhow::ensure!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        Ok(())
    };
    git(&paths.root, &["init", "-q", "--bare", "-b", "main", &paths.remote.display().to_string()])?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    std::fs::write(repo.join("README.md"), "canvas push check\n")?;
    git(&repo, &["add", "README.md"])?;
    git(&repo, &["-c", "user.name=selftest", "-c", "user.email=selftest@example.invalid", "commit", "-q", "-m", "init"])?;
    git(&repo, &["remote", "add", "origin", &paths.remote.display().to_string()])?;
    canvas_common::settings::Settings { agent: Some("opencode".into()), ..Default::default() }.save(&paths.home)?;
    // The passive session: a group with a folder and a box that runs OpenCode.
    let slug = "canvas-passive-check";
    let rules = paths.group.join(".canvas").join("passive").join(slug);
    std::fs::create_dir_all(&rules)?;
    std::fs::write(
        rules.join("rules.md"),
        format!("# Rules for {PASSIVE}\n\nThis passive session is a self-test. On every run do only this: run `copilot-canvas status ok \"canvas passive ok\"` with the bash tool, then append the line `- self-test run` to your run log. Do not read or change anything else.\n"),
    )?;
    let mcp_off: Vec<String> = agents::config::snapshot(AgentKind::OpenCode).map(|s| s.mcp.into_iter().map(|m| m.name).collect()).unwrap_or_default();
    let group = Group { id: "passive-check".into(), name: PASSIVE_GROUP.into(), x: 150, y: 0, w: 36, h: 12, folder: Some(paths.group.display().to_string()), ..Default::default() };
    let mut node: Node = serde_json::from_value(serde_json::json!({ "id": uuid::Uuid::new_v4().to_string(), "kind": "session", "name": PASSIVE, "x": 152, "y": 4 }))?;
    node.cwd = Some(paths.group.display().to_string());
    node.source = Some("passive".into());
    node.fresh = true;
    node.agent = Some("opencode".into());
    node.mcp_off = Some(mcp_off);
    node.passive = Some(Passive { key: uuid::Uuid::new_v4().to_string(), group: group.id.clone(), slug: slug.into(), template: "Custom".into(), rollover_runs: 20, max_runs_per_day: 24, ..Default::default() });
    let doc = Doc { nodes: vec![node], groups: vec![group], ..Doc::default() };
    std::fs::write(paths.home.join("canvas.json"), serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

fn service_log(instance: &Instance) -> String {
    std::fs::read_to_string(instance.log_path()).unwrap_or_default()
}

/// Waits for a service log line containing `needle`; returns that line.
fn wait_log(instance: &Instance, what: &str, needle: &str, timeout: Duration) -> Result<String> {
    wait_log_after(instance, what, needle, 0, timeout)
}

/// Lines of the service log containing `needle`.
fn log_count(instance: &Instance, needle: &str) -> usize {
    service_log(instance).lines().filter(|l| l.contains(needle)).count()
}

/// Waits for a service log line containing `needle` after the first `skip`
/// such lines; returns it.
fn wait_log_after(instance: &Instance, what: &str, needle: &str, skip: usize, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(line) = service_log(instance).lines().filter(|l| l.contains(needle)).nth(skip) {
            return Ok(line.to_owned());
        }
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what} (no service log line with {needle:?})");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The command line of the program the service launched for terminal `id`.
pub(crate) fn launched_command_line(instance: &Instance, id: &str) -> Result<(u32, String)> {
    let marker = format!("launched {id} ");
    let pid = service_log(instance)
        .lines()
        .rev()
        .find(|l| l.contains(&marker))
        .and_then(|l| l.rsplit(" pid ").next())
        .and_then(|p| p.trim().parse::<u32>().ok())
        .with_context(|| format!("no launch of {id} in the service log"))?;
    let query = format!("(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').CommandLine");
    let out = std::process::Command::new("powershell.exe").args(["-NoProfile", "-Command", &query]).output()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    anyhow::ensure!(!line.is_empty(), "cannot read the command line of pid {pid}");
    Ok((pid, line))
}

/// Keyboard focus on the canvas (not a terminal or the sidebar). The hint
/// line says where typing goes; a notice hides it for a few seconds.
fn canvas_focus<U: Ui>(ui: &mut U) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let text = ui.text();
        if text.contains("empty-drag pan") {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for canvas focus\n--- screen ---\n{text}");
        if text.contains("Typing goes to the terminal") {
            ui.send(b"\x1d")?;
        } else if text.contains("Tab canvas") {
            ui.send(b"\t")?;
        } else if text.contains("Esc back") {
            ui.send(b"\x1b1")?;
        }
        // Let the hint line catch up before deciding again.
        std::thread::sleep(Duration::from_millis(700));
    }
}

/// Opens a session's terminal from its sidebar row (you look at it).
fn open_terminal<U: Ui>(ui: &mut U, name: &str) -> Result<()> {
    canvas_focus(ui)?;
    ui.send(b"\x1b1")?;
    ui.wait("sessions view", Duration::from_secs(5), |t| t.contains(name))?;
    let (x, y) = ui.locate(name, SIDEBAR)?;
    ui.click(x, y)?;
    let header = format!("{name} · ");
    ui.wait("its terminal with keyboard focus", Duration::from_secs(10), |t| t.contains("Typing goes to the terminal") && t.contains(&header))
}

/// Sets a new-session default in Settings (OpenCode's model, typed into its
/// picker, or a choice).
fn set_default<U: Ui>(ui: &mut U, canvas: &Path, field: &str, value: &str) -> Result<Doc> {
    canvas_focus(ui)?;
    ui.send(b"\x1b8")?;
    ui.wait("settings view", Duration::from_secs(5), |t| t.contains("New-session defaults"))?;
    let (x, y) = ui.locate(field, SIDEBAR)?;
    ui.click(x, y)?;
    let model = field == "OpenCode model";
    if model {
        // The picker takes any id typed into its filter (a listed one is picked).
        ui.wait("model picker", Duration::from_secs(5), |t| t.contains("OpenCode model for new sessions") && t.contains("Type a model id"))?;
        ui.send(value.as_bytes())?;
    } else {
        // Permissions: allow-all, then ask (filtering by "ask" also matches allow-all's "asking").
        ui.wait("choices", Duration::from_secs(5), |t| t.contains("allow-all") && t.contains("the agent asks"))?;
        ui.send(b"\x1b[B")?;
    }
    ui.send(b"\r")?;
    let doc = wait_doc(canvas, &format!("{field} = {value} saved"), |d| match model {
        true => d.defaults.model(AgentKind::OpenCode) == Some(value),
        false => d.defaults.permissions.as_deref() == Some(value),
    })?;
    ui.send(b"\x1b1")?;
    Ok(doc)
}

/// A new OpenCode session from the new-session dialog (MCP servers off, so
/// nothing depends on them); returns its box id once the TUI is up.
fn new_session<U: Ui>(ui: &mut U, canvas: &Path, name: &str) -> Result<String> {
    canvas_focus(ui)?;
    ui.send(b"n")?;
    ui.wait("new session dialog with OpenCode chosen", Duration::from_secs(5), |t| t.contains("New session") && t.contains("◉ OpenCode"))?;
    ui.send(name.as_bytes())?;
    // Name → Folder → Browse… → Recent → Agent → Model → MCP servers.
    for _ in 0..6 {
        ui.send(b"\t")?;
    }
    ui.send(b"\r")?;
    ui.wait("MCP picker", Duration::from_secs(30), |t| t.contains("MCP servers for the new session") && !t.contains("MCP servers.\u{2026}") && !t.contains("Loading"))?;
    ui.send(b"n")?;
    ui.send(b"\r")?;
    ui.wait("back in the new session dialog", Duration::from_secs(5), |t| t.contains("New session") && !t.contains("MCP servers for the new session"))?;
    ui.send(b"\t\t")?;
    ui.send(b"\r")?;
    let header = format!("{name} · ");
    ui.wait("the OpenCode UI", Duration::from_secs(90), |t| t.contains(&header) && agent_ui_up(AgentKind::OpenCode, t))?;
    // OpenCode draws its home screen in steps.
    std::thread::sleep(Duration::from_secs(2));
    load(canvas)?.nodes.iter().find(|n| n.name == name).map(|n| n.id.clone()).context("the new session's box")
}

/// Types a prompt into the focused TUI and submits it, after checking that
/// the TUI runs with the free model.
fn submit<U: Ui>(ui: &mut U, command_line: &str, model: &str, prompt: &str) -> Result<(Instant, u64)> {
    may_prompt(command_line, model)?;
    anyhow::ensure!(ui.text().contains("Typing goes to the terminal"), "the terminal does not have keyboard focus");
    ui.send(prompt.as_bytes())?;
    let start: String = prompt.chars().take(20).collect();
    ui.wait("the prompt in OpenCode's input", Duration::from_secs(15), |t| t.contains(&start))?;
    ui.send(b"\r")?;
    Ok((Instant::now(), now_ms()))
}

fn db() -> Result<rusqlite::Connection> {
    opencode_history::open(&opencode_history::db_path())
}

/// The turn started at `since_ms` ended: the newest message is a finished
/// assistant message (`Some(error name)`, empty when it worked).
fn turn_end(session: &str, since_ms: u64) -> Result<Option<String>> {
    let row = db()?
        .query_row(
            "SELECT json_extract(data, '$.role'), json_extract(data, '$.time.completed'), json_extract(data, '$.finish'), json_extract(data, '$.error.name'), time_created \
             FROM message WHERE session_id = ?1 ORDER BY time_created DESC, id DESC LIMIT 1",
            [session],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, i64>(4)?)),
        )
        .optional()?;
    Ok(match row {
        Some((Some(role), Some(_), finish, error, created)) if role == "assistant" && created as u64 + 1_000 >= since_ms && finish.as_deref() != Some("tool-calls") => {
            Some(error.unwrap_or_default())
        }
        _ => None,
    })
}

fn wait_turn(session: &str, since_ms: u64) -> Result<Duration> {
    let started = Instant::now();
    loop {
        if let Some(error) = turn_end(session, since_ms)? {
            anyhow::ensure!(error.is_empty() || error == "MessageAbortedError", "the turn failed: {error}");
            return Ok(started.elapsed());
        }
        anyhow::ensure!(started.elapsed() < TURN, "the turn did not end within {} s", TURN.as_secs());
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Child (subagent) sessions of a session.
fn children(session: &str) -> Result<Vec<String>> {
    let connection = db()?;
    let mut statement = connection.prepare("SELECT id FROM session WHERE parent_id = ?1 ORDER BY time_created")?;
    let rows = statement.query_map([session], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// One bash tool call of a session.
struct BashCall {
    status: String,
    command: String,
    exit: Option<i64>,
    /// The start of its output.
    output: String,
}

impl std::fmt::Display for BashCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` ({}, exit {:?}): {:?}", self.command, self.status, self.exit, self.output)
    }
}

/// Bash tool calls of a session.
fn bash_calls(session: &str) -> Result<Vec<BashCall>> {
    let connection = db()?;
    let mut statement = connection.prepare(
        "SELECT json_extract(data, '$.state.status'), json_extract(data, '$.state.input.command'), json_extract(data, '$.state.metadata.exit'), \
         substr(json_extract(data, '$.state.output'), 1, 300) FROM part \
         WHERE session_id = ?1 AND json_extract(data, '$.type') = 'tool' AND json_extract(data, '$.tool') = 'bash'",
    )?;
    let rows = statement.query_map([session], |r| {
        Ok(BashCall {
            status: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            command: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            exit: r.get(2)?,
            output: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Models (provider/model) and the cost of a session's assistant messages.
fn usage(session: &str) -> Result<(Vec<String>, f64, usize)> {
    let connection = db()?;
    let mut statement = connection.prepare(
        "SELECT json_extract(data, '$.providerID'), json_extract(data, '$.modelID'), json_extract(data, '$.cost') FROM message \
         WHERE session_id = ?1 AND json_extract(data, '$.role') = 'assistant'",
    )?;
    let rows: Vec<(Option<String>, Option<String>, Option<f64>)> = statement.query_map([session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut models: Vec<String> = rows.iter().map(|(p, m, _)| format!("{}/{}", p.as_deref().unwrap_or("?"), m.as_deref().unwrap_or("?"))).collect();
    models.sort();
    models.dedup();
    Ok((models, rows.iter().filter_map(|(_, _, c)| *c).sum(), rows.len()))
}

/// The labels the canvas shows for a session over time (from its tile header).
struct Seen {
    started: Instant,
    events: Vec<(String, Duration)>,
}

impl Seen {
    fn new(started: Instant) -> Self {
        Self { started, events: Vec::new() }
    }

    fn note(&mut self, what: &str) -> bool {
        if self.events.iter().any(|(w, _)| w == what) {
            return false;
        }
        self.events.push((what.to_owned(), self.started.elapsed()));
        true
    }

    fn at(&self, what: &str) -> Option<Duration> {
        self.events.iter().find(|(w, _)| w == what).map(|(_, d)| *d)
    }

    fn line(&self) -> String {
        self.events.iter().map(|(w, d)| format!("{w} at {:.1} s", d.as_secs_f64())).collect::<Vec<_>>().join(", ")
    }
}

/// The status label in a session's tile header.
fn tile_status(text: &str, name: &str) -> Option<String> {
    let marker = format!("{name} · ");
    let line = text.lines().find(|l| l.contains(&marker))?;
    let rest = &line[line.find(&marker)? + marker.len()..];
    ["needs you", "done · review", "working", "idle", "running", "preparing", "stopped", "failed", "open elsewhere", "unknown"]
        .into_iter()
        .find(|label| rest.starts_with(label))
        .map(str::to_owned)
}

/// What the service tells a client about a terminal and its agent session
/// (for diagnosing a status the canvas shows).
fn service_view(instance: &Instance, terminal: &str, native: &str) -> String {
    let Ok(conn) = crate::client::conn::try_connect(instance) else { return "the service does not answer".into() };
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(1_500);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match conn.rx.recv_timeout(left) {
            Ok(crate::protocol::ServiceMsg::Terminals(terms)) => {
                for t in terms.iter().filter(|t| t.id == terminal) {
                    out.push(format!("terminal alive={} progress={} seen={} at={} title={:?}", t.alive, t.progress, t.progress_seen, t.progress_ms, t.title));
                }
            }
            Ok(crate::protocol::ServiceMsg::Sessions { upsert, .. }) => {
                for s in upsert.iter().filter(|s| s.id == native) {
                    out.push(format!(
                        "summary activity={:?} finished={:?} pending={} live={} last_event={} agents={:?}",
                        s.activity,
                        s.finished_ms,
                        s.pending,
                        s.live,
                        s.last_event_ms,
                        s.agents.iter().map(|a| format!("{}:{:?}", a.name, a.status)).collect::<Vec<_>>()
                    ));
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out.join("; ")
}

/// After a turn ended, the box must stop showing working (idle while you
/// watch it, done · review otherwise); returns how long that took.
fn settle<U: Ui>(ui: &U, instance: &Instance, name: &str, terminal: &str, native: &str) -> Result<Duration> {
    let started = Instant::now();
    let watched = ui.text().contains("Typing goes to the terminal");
    let wanted = |s: &str| s == "idle" || (!watched && s == "done · review");
    match ui.wait("the box to settle after the turn", Duration::from_secs(20), |t| tile_status(t, name).is_some_and(|s| wanted(&s))) {
        Ok(()) => Ok(started.elapsed()),
        Err(error) => {
            let canvas = load(&instance.home.join("canvas.json")).ok();
            let reviewed = canvas.as_ref().and_then(|d| d.node(terminal)).and_then(|n| n.reviewed_ms);
            bail!("{error:#}\n--- watched {watched}; now {} reviewed {reviewed:?}; service view: {}", now_ms(), service_view(instance, terminal, native))
        }
    }
}

fn scenario<U: Ui>(instance: &Instance, paths: &Paths, model: &str, steps: &[String], env: &[(&str, &Path)], start: impl Fn(&[String], &[(&str, &Path)]) -> Result<U>) -> Result<()> {
    let args: Vec<String> = vec![
        "--instance".into(),
        instance.name.clone(),
        "--home".into(),
        instance.home.to_string_lossy().into_owned(),
        "--cwd".into(),
        paths.work.to_string_lossy().into_owned(),
    ];
    let canvas = instance.home.join("canvas.json");
    let mut ui = start(&args, env)?;
    ui.wait("the canvas", Duration::from_secs(30), |t| t.contains("Copilot Canvas") && t.contains(PASSIVE))?;

    step(&format!("Settings → OpenCode model sets {model} for new sessions"));
    set_default(&mut ui, &canvas, "OpenCode model", model)?;

    step("a new OpenCode session from the dialog runs the real TUI with -m <free model> and --auto");
    let main = new_session(&mut ui, &canvas, MAIN)?;
    let (pid, command_line) = launched_command_line(instance, &main)?;
    may_prompt(&command_line, model)?;
    anyhow::ensure!(command_line.split_whitespace().any(|a| a == "--auto"), "allow-all (the default) means --auto: {command_line}");
    wait_log(instance, "the launch with its model", &format!("terminal {main}: opencode new session, model {model}"), Duration::from_secs(5))?;
    println!("    pid {pid}: …{}", command_line.split_once("opencode.exe").map(|(_, rest)| rest.trim_start_matches('"')).unwrap_or(&command_line));

    step("the service talks to the TUI's server only after checking its listener belongs to the TUI");
    wait_log(instance, "the TUI's folder from its server", &format!("terminal {main} works in "), Duration::from_secs(30))?;
    anyhow::ensure!(!service_log(instance).contains("not sent there"), "the listener check refused the TUI's own server:\n{}", service_log(instance));
    let node = load(&canvas)?.nodes.into_iter().find(|n| n.id == main).context("box")?;
    anyhow::ensure!(node.agent_session.is_none(), "no OpenCode session exists before the first prompt: {:?}", node.agent_session);

    step("first prompt: the box follows the new ses_ session; working → done · review → idle after you look");
    let (sent, sent_ms) = submit(&mut ui, &command_line, model, PROMPT_STATUS)?;
    // Look away, so the finished turn waits for review.
    canvas_focus(&mut ui)?;
    let mut seen = Seen::new(sent);
    let mut native = None;
    let deadline = sent + TURN;
    while seen.at("done · review").is_none() {
        anyhow::ensure!(Instant::now() < deadline, "no \"done · review\" within {} s (seen: {})\n--- screen ---\n{}", TURN.as_secs(), seen.line(), ui.text());
        if native.is_none()
            && let Some(id) = load(&canvas).ok().and_then(|d| d.nodes.into_iter().find(|n| n.id == main)).and_then(|n| n.agent_session)
        {
            seen.note("re-keyed");
            native = Some(id);
        }
        if let Some(label) = tile_status(&ui.text(), MAIN) {
            seen.note(&label);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let native = match native {
        Some(native) => native,
        None => wait_doc(&canvas, "the box's ses_ id", |d| d.node(&main).is_some_and(|n| n.agent_session.is_some()))?
            .node(&main)
            .and_then(|n| n.agent_session.clone())
            .context("the box never got its ses_ id")?,
    };
    anyhow::ensure!(canvas_agents::agents::opencode::is_session_id(&native), "re-keyed to {native:?}");
    let created = opencode_history::discover_new_sessions(&paths.work.display().to_string(), sent_ms.saturating_sub(5_000));
    anyhow::ensure!(created.first().map(|(id, _)| id) == Some(&native), "the box follows {native}, the folder's new sessions are {created:?}");
    wait_log(instance, "the service learning the session", &format!("terminal {main} runs agent session {native}"), Duration::from_secs(5))?;
    seen.at("working").context(format!("the box never showed working: {}", seen.line()))?;
    open_terminal(&mut ui, MAIN)?;
    let looked = Instant::now();
    ui.wait("idle once looked at", Duration::from_secs(10), |t| tile_status(t, MAIN).as_deref() == Some("idle"))?;
    println!("    {MAIN} → {native}; {} (Enter at 0 s); idle {:.1} s after opening it", seen.line(), looked.elapsed().as_secs_f64());

    if steps.iter().any(|s| s == "wheel") {
        step("the mouse wheel over the OpenCode terminal scrolls OpenCode's own view");
        let (_, ms) = submit(&mut ui, &command_line, model, PROMPT_ROWS)?;
        let took = wait_turn(&native, ms)?;
        settle(&ui, instance, MAIN, &main, &native)?;
        std::thread::sleep(Duration::from_millis(800));
        let rows = |text: &str| -> Vec<(u32, (u16, u16))> {
            text.lines()
                .enumerate()
                .flat_map(|(y, line)| {
                    line.match_indices("canvasrow").filter_map(move |(byte, _)| {
                        let digits: String = line[byte + 9..].chars().take_while(char::is_ascii_digit).collect();
                        let x = crate::client::paint::text_width(&line[..byte]) as u16;
                        digits.parse().ok().filter(|_| x >= RIGHT.0).map(|n| (n, (x, y as u16)))
                    })
                })
                .collect()
        };
        let before = rows(&ui.text());
        let lowest = before.iter().map(|(n, _)| *n).min().with_context(|| format!("no canvasrow lines visible\n--- screen ---\n{}", ui.text()))?;
        let highest = before.iter().map(|(n, _)| *n).max().unwrap_or(0);
        anyhow::ensure!(lowest > 1, "the whole answer fits on screen (rows {lowest}..{highest}); nothing to scroll\n{}", ui.text());
        let (x, y) = before.iter().find(|(n, _)| *n == highest).map(|(_, at)| *at).unwrap_or((RIGHT.0 + 4, 20));
        for _ in 0..3 {
            ui.send(format!("\x1b[<64;{};{}M", x + 2, y + 1).as_bytes())?;
            std::thread::sleep(Duration::from_millis(400));
        }
        ui.wait("earlier lines after wheel up", Duration::from_secs(10), |t| rows(t).iter().map(|(n, _)| *n).min().is_some_and(|n| n < lowest))?;
        let after = rows(&ui.text()).iter().map(|(n, _)| *n).min().unwrap_or(0);
        println!("    answer took {:.1} s; rows {lowest}..{highest} visible; 3 wheel notches up: first visible row {after}", took.as_secs_f64());
        for _ in 0..6 {
            ui.send(format!("\x1b[<65;{};{}M", x + 2, y + 1).as_bytes())?;
        }
    }

    if steps.iter().any(|s| s == "push") {
        step("git push from OpenCode's bash tool becomes a pr.pushed event of the box");
        let needle = format!("detected pr.pushed in {native}");
        // A free model sometimes runs something else than the command it was
        // given: then it is asked once more. A push that ran but was not
        // detected fails at once.
        let mut attempt = 0;
        let (line, started, ms) = 'attempts: loop {
            attempt += 1;
            let (_, ms) = submit(&mut ui, &command_line, model, PROMPT_PUSH)?;
            let started = Instant::now();
            let mut ended: Option<Instant> = None;
            loop {
                if let Some(line) = service_log(instance).lines().find(|l| l.contains(&needle)) {
                    break 'attempts (line.to_owned(), started, ms);
                }
                if ended.is_none() && turn_end(&native, ms)?.is_some() {
                    ended = Some(Instant::now());
                }
                // The history is read about once a second: a few seconds after the turn are enough.
                if ended.is_some_and(|t| t.elapsed() > Duration::from_secs(8)) || started.elapsed() > TURN {
                    let calls: Vec<String> = bash_calls(&native)?.iter().map(ToString::to_string).collect();
                    let pushed = std::process::Command::new("git").arg("-C").arg(&paths.remote).args(["rev-parse", "main"]).output()?.status.success();
                    if !pushed && attempt < 2 && !calls.iter().any(|c| c.contains("git push")) {
                        println!("    the model did not run the push (its bash calls: {}); asking once more", calls.join("; "));
                        settle(&ui, instance, MAIN, &main, &native)?;
                        continue 'attempts;
                    }
                    bail!("no pr.pushed for {native} (the remote has main: {pushed}); its bash calls: {}", calls.join("; "));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        };
        let detected = started.elapsed();
        anyhow::ensure!(line.contains(&format!("(terminal {main})")), "the event comes from the box: {line}");
        wait_turn(&native, ms)?;
        settle(&ui, instance, MAIN, &main, &native)?;
        let pushed = std::process::Command::new("git").arg("-C").arg(&paths.remote).args(["rev-parse", "main"]).output()?;
        anyhow::ensure!(pushed.status.success(), "the bare remote has no main branch: the push did not happen");
        let events = std::fs::read_to_string(instance.home.join("bus").join("log.jsonl")).unwrap_or_default();
        let event = events.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).find(|e| e["type"] == "pr.pushed").context("pr.pushed is not in the bus log")?;
        anyhow::ensure!(event["source"] == main.as_str(), "the bus event's source is the box: {event}");
        println!("    detected {:.1} s after Enter: branch {} remote {}", detected.as_secs_f64(), event["data"]["branch"], event["data"]["remote"]);
    }

    let mut child: Option<(String, String)> = None;
    if steps.iter().any(|s| s == "subagent") {
        step("a task-tool subagent of OpenCode becomes a subagent box under the session; its transcript opens read-only");
        let (sent, ms) = submit(&mut ui, &command_line, model, PROMPT_TASK)?;
        let mut boxed: Option<Duration> = None;
        let deadline = sent + TURN;
        while turn_end(&native, ms)?.is_none() {
            if boxed.is_none() && load(&canvas)?.nodes.iter().any(|n| n.kind == NodeKind::Agent && n.parent.as_deref() == Some(main.as_str())) {
                boxed = Some(sent.elapsed());
            }
            anyhow::ensure!(Instant::now() < deadline, "the subagent prompt did not finish within {} s", TURN.as_secs());
            std::thread::sleep(Duration::from_millis(300));
        }
        let kids = children(&native)?;
        settle(&ui, instance, MAIN, &main, &native)?;
        if kids.is_empty() {
            println!("    the free model did not use the task tool (no child session): nothing to show");
        } else {
            let doc = wait_doc(&canvas, "a subagent box per child session", |d| kids.iter().all(|k| d.node(&format!("{main}:{k}")).is_some_and(|n| n.kind == NodeKind::Agent)))?;
            let names: Vec<String> = kids.iter().filter_map(|k| doc.node(&format!("{main}:{k}"))).map(|n| n.name.clone()).collect();
            let first = boxed.map(|d| format!("{:.1} s after Enter", d.as_secs_f64())).unwrap_or_else(|| "after the turn".into());
            println!("    {} child session(s) {kids:?} → subagent box(es) {names:?}, the first {first}", kids.len());
            // Inspect the child from its box (selected from the sidebar): nothing starts.
            let launches = log_count(instance, "launched ");
            open_terminal(&mut ui, MAIN)?;
            canvas_focus(&mut ui)?;
            let (x, y) = ui.locate(&names[0], SIDEBAR)?;
            ui.click(x, y)?;
            ui.wait("the subagent's details", Duration::from_secs(10), |t| t.contains("Inspect transcript"))?;
            let (ix, iy) = ui.locate("Inspect transcript", RIGHT)?;
            ui.click(ix + 1, iy)?;
            ui.wait("the child's transcript", Duration::from_secs(15), |t| t.contains("Transcript · ") && t.contains("❯ ") && !t.contains("Loading…"))?;
            let shown = ui.text().lines().filter(|l| l.contains("❯ ") || l.contains("⚙ ")).count();
            ui.send(b"\x1b")?;
            ui.wait("the inspector closed", Duration::from_secs(5), |t| !t.contains("Transcript · "))?;
            anyhow::ensure!(log_count(instance, "launched ") == launches, "inspecting launched something");
            println!("    the child's transcript opened read-only ({shown} prompt/tool line(s) on screen), nothing launched");
            child = Some((kids[0].clone(), names[0].clone()));
            // Back in the session's terminal for the next prompt.
            open_terminal(&mut ui, MAIN)?;
        }
    }

    if steps.iter().any(|s| s == "question") {
        step("OpenCode's question tool shows the box as needs you; Esc dismisses the question");
        let (sent, ms) = submit(&mut ui, &command_line, model, PROMPT_QUESTION)?;
        canvas_focus(&mut ui)?;
        let mut seen = Seen::new(sent);
        let deadline = sent + TURN;
        while seen.at("needs you").is_none() && turn_end(&native, ms)?.is_none() {
            anyhow::ensure!(Instant::now() < deadline, "neither a question nor the end of the turn ({})", seen.line());
            if let Some(label) = tile_status(&ui.text(), MAIN) {
                seen.note(&label);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if seen.at("needs you").is_none() {
            println!("    the free model did not use the question tool ({}): nothing to answer", seen.line());
            open_terminal(&mut ui, MAIN)?;
        } else {
            open_terminal(&mut ui, MAIN)?;
            ui.wait("OpenCode's question", Duration::from_secs(10), |t| t.contains("Which canvas color"))?;
            // OpenCode's question dialog maps Escape to "Reject question".
            ui.send(b"\x1b")?;
            ui.wait("the question dismissed", Duration::from_secs(10), |t| !t.contains("Which canvas color?") || tile_status(t, MAIN).is_some_and(|s| s != "needs you"))?;
            wait_turn(&native, ms)?;
            let after = settle(&ui, instance, MAIN, &main, &native)?;
            println!("    {} (Enter at 0 s); dismissed with Esc; {:?} {:.1} s after the turn ended", seen.line(), tile_status(&ui.text(), MAIN).unwrap_or_default(), after.as_secs_f64());
        }
    }

    if steps.iter().any(|s| s == "resume" || s == "busystop" || s == "streamstop") {
        let busy = steps.iter().any(|s| s == "busystop" || s == "streamstop");
        if busy {
            step("stopping the box mid-turn (graceful), then opening it again resumes the session; the box is not left working");
            let prompt = if steps.iter().any(|s| s == "streamstop") { PROMPT_STREAM } else { PROMPT_LONG };
            let (sent, _) = submit(&mut ui, &command_line, model, prompt)?;
            ui.wait("working", Duration::from_secs(60), |t| tile_status(t, MAIN).as_deref() == Some("working"))?;
            if prompt == PROMPT_STREAM {
                ui.wait("the answer streaming", Duration::from_secs(60), |t| t.contains("canvasline5"))?;
            }
            // The tool or the answer is still running: the turn is in progress.
            std::thread::sleep(Duration::from_secs(3).saturating_sub(sent.elapsed()));
        } else {
            step("stopping the box and opening it again resumes the same OpenCode session (-s <ses_ id>)");
        }
        canvas_focus(&mut ui)?;
        let exits = log_count(instance, &format!("terminal {main} exited with "));
        ui.send(b"x")?;
        ui.wait("stop dialog", Duration::from_secs(5), |t| t.contains(&format!("Stop “{MAIN}”?")))?;
        let asked = Instant::now();
        ui.send(b"\r")?;
        let stopped = wait_log_after(instance, "the stop", &format!("terminal {main} exited with "), exits, Duration::from_secs(20))?;
        let took = asked.elapsed();
        anyhow::ensure!(stopped.contains("Some(0)") && !stopped.contains("killed"), "OpenCode did not exit gracefully: {stopped}");
        if let Some((kid, name)) = child.as_ref().filter(|_| !busy) {
            resume_child(&mut ui, instance, &canvas, &main, kid, name)?;
        }
        let launches = log_count(instance, &format!("launched {main} "));
        let (x, y) = ui.locate(MAIN, SIDEBAR)?;
        // A click only shows the stopped box's details (Resume); the double-click resumes it.
        ui.click(x, y)?;
        ui.wait("its details panel offers Resume", Duration::from_secs(10), |t| t.contains("▸ Resume"))?;
        std::thread::sleep(Duration::from_millis(500));
        anyhow::ensure!(log_count(instance, &format!("launched {main} ")) == launches, "a click relaunched the session");
        ui.double_click(x, y)?;
        wait_log_after(instance, "the relaunch", &format!("launched {main} "), launches, Duration::from_secs(20))?;
        wait_log(instance, "a resume", &format!("terminal {main}: opencode resume"), Duration::from_secs(5))?;
        let header = format!("{MAIN} · ");
        // Text of this conversation's prompts or answers.
        let earlier = |t: &str| ["reply with just", "canvasline", "canvasrow"].iter().any(|m| t.contains(m));
        ui.wait("the resumed conversation", Duration::from_secs(60), |t| t.contains(&header) && agent_ui_up(AgentKind::OpenCode, t) && earlier(t))?;
        let (_, resumed) = launched_command_line(instance, &main)?;
        let words: Vec<&str> = resumed.split_whitespace().collect();
        anyhow::ensure!(words.windows(2).any(|w| w[0] == "-s" && w[1] == native), "resumed without -s {native}: {resumed}");
        let doc = load(&canvas)?;
        anyhow::ensure!(doc.node(&main).and_then(|n| n.agent_session.as_deref()) == Some(native.as_str()), "the box still follows {native}");
        ui.wait("status of the resumed session", Duration::from_secs(15), |t| tile_status(t, MAIN).is_some_and(|s| s == "idle" || s == "done · review"))?;
        println!(
            "    stopped in {:.1} s ({}); reopened with -s {native}; status {:?}",
            took.as_secs_f64(),
            stopped.split("exited with ").nth(1).unwrap_or_default().trim(),
            tile_status(&ui.text(), MAIN).unwrap_or_default()
        );
    }

    if steps.iter().any(|s| s == "permission") {
        step("Settings → Permissions: ask; a permission prompt shows the box as needs you; Esc rejects it");
        set_default(&mut ui, &canvas, "Permissions", "ask")?;
        let ask = new_session(&mut ui, &canvas, ASK)?;
        let (_, ask_line) = launched_command_line(instance, &ask)?;
        may_prompt(&ask_line, model)?;
        anyhow::ensure!(!ask_line.split_whitespace().any(|a| a == "--auto"), "ask means no --auto: {ask_line}");
        wait_log(instance, "the TUI's folder from its server", &format!("terminal {ask} works in "), Duration::from_secs(30))?;
        let (sent, _) = submit(&mut ui, &ask_line, model, PROMPT_ASK)?;
        canvas_focus(&mut ui)?;
        let mut seen = Seen::new(sent);
        let deadline = sent + TURN;
        while seen.at("needs you").is_none() {
            anyhow::ensure!(Instant::now() < deadline, "the box never showed needs you ({})\n--- screen ---\n{}", seen.line(), ui.text());
            if let Some(label) = tile_status(&ui.text(), ASK) {
                seen.note(&label);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // The canvas file is written shortly after the box changes.
        let ask_native = wait_doc(&canvas, "the permission box's ses_ id", |d| d.node(&ask).is_some_and(|n| n.agent_session.is_some()))?
            .node(&ask)
            .and_then(|n| n.agent_session.clone())
            .context("the permission box never got its ses_ id")?;
        open_terminal(&mut ui, ASK)?;
        ui.wait("OpenCode's permission dialog", Duration::from_secs(10), |t| t.contains("Permission required"))?;
        let mut rejected = 0;
        let deadline = Instant::now() + TURN;
        loop {
            if ui.text().contains("Permission required") {
                // OpenCode's dialog maps Escape to "Reject".
                ui.send(b"\x1b")?;
                rejected += 1;
                ui.wait("the dialog closed", Duration::from_secs(10), |t| !t.contains("Permission required"))?;
            }
            if turn_end(&ask_native, 0)?.is_some() || rejected >= 3 {
                break;
            }
            anyhow::ensure!(Instant::now() < deadline, "the rejected turn did not end");
            std::thread::sleep(Duration::from_millis(300));
        }
        let ran: Vec<String> = bash_calls(&ask_native)?.into_iter().filter(|call| call.status == "completed").map(|call| call.command).collect();
        anyhow::ensure!(ran.is_empty(), "a rejected command ran: {ran:?}");
        wait_turn(&ask_native, 0)?;
        settle(&ui, instance, ASK, &ask, &ask_native)?;
        println!("    {ASK} → {ask_native}; {} (Enter at 0 s); rejected {rejected} prompt(s) with Esc, no command ran", seen.line());
    }

    if steps.iter().any(|s| s == "passive") {
        step("a passive OpenCode session runs headless (opencode run -m <free model>) and reports its status");
        let routing: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(instance.home.join("bus").join("routing.json"))?)?;
        let spec = routing["passive"].as_array().and_then(|a| a.iter().find(|p| p["name"] == PASSIVE)).context("the passive session is not routed")?;
        let passive_model = spec["model"].as_str().unwrap_or_default();
        ensure_free(passive_model).context("the passive session's runs would not use the free model")?;
        anyhow::ensure!(passive_model == model && spec["agent"] == "opencode", "routed as {spec}");
        let key = spec["key"].as_str().unwrap_or_default().to_owned();
        let id = spec["session_id"].as_str().unwrap_or_default().to_owned();
        canvas_focus(&mut ui)?;
        ui.send(b"\x1b1")?;
        let (x, y) = ui.locate(PASSIVE, SIDEBAR)?;
        ui.click(x, y)?;
        ui.wait("the passive panel", Duration::from_secs(5), |t| t.contains(&format!("⟲ {PASSIVE}")) && t.contains("Run now"))?;
        let (rx, ry) = ui.locate("Run now", RIGHT)?;
        let clicked = Instant::now();
        ui.click(rx + 1, ry)?;
        wait_log(instance, "the passive run", &format!("passive {PASSIVE} run started"), Duration::from_secs(30))?;
        wait_log(instance, "its headless launch", &format!("terminal {id}: opencode headless run, model {model}"), Duration::from_secs(5))?;
        let started = clicked.elapsed();
        let state_path = instance.home.join("bus").join("state.json");
        let deadline = Instant::now() + TURN;
        let state = loop {
            let state: serde_json::Value = std::fs::read_to_string(&state_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            let s = &state["passive"][&key];
            if s["running"] == false && s["last_run_ms"].as_u64().unwrap_or(0) > 0 || s["failures"].as_u64().unwrap_or(0) > 0 {
                break s.clone();
            }
            anyhow::ensure!(Instant::now() < deadline, "the passive run did not finish within {} s: {s}", TURN.as_secs());
            std::thread::sleep(Duration::from_millis(500));
        };
        let finished = clicked.elapsed();
        anyhow::ensure!(state["last_error"].as_str().unwrap_or_default().is_empty(), "the run failed: {}", state["last_error"]);
        let exit = wait_log(instance, "the run's exit", &format!("terminal {id} exited with "), Duration::from_secs(5))?;
        // `opencode run` names its session itself: the service learns it from the folder.
        let learned = wait_log(instance, "the passive run's session", &format!("terminal {id} runs agent session "), Duration::from_secs(20)).ok();
        let native = learned.as_deref().and_then(|l| l.rsplit(' ').next()).map(str::to_owned);
        let in_folder = opencode_history::discover_new_sessions(&paths.group.display().to_string(), now_ms().saturating_sub(900_000));
        anyhow::ensure!(native.is_some() && in_folder.first().map(|(s, _)| s) == native.as_ref(), "the passive box follows {native:?}; the run created {in_folder:?}");
        wait_doc(&canvas, "the passive box's ses_ id", |d| d.node(&id).is_some_and(|n| n.agent_session == native))?;
        let status = (state["status"].as_str().unwrap_or_default().to_owned(), state["headline"].as_str().unwrap_or_default().to_owned());
        if status.0.is_empty() {
            println!("    run started {:.1} s and finished {:.1} s after Run now ({}); session {native:?}; no status was reported", started.as_secs_f64(), finished.as_secs_f64(), exit.trim());
            bail!("the passive run did not report a status (copilot-canvas status …)");
        }
        ui.wait("the reported status on the canvas", Duration::from_secs(10), |t| t.contains(&format!("{} · {}", status.0, status.1)) || t.contains(&status.1))?;
        println!(
            "    run started {:.1} s and finished {:.1} s after Run now; session {}; status {} \"{}\"",
            started.as_secs_f64(),
            finished.as_secs_f64(),
            native.as_deref().unwrap_or("?"),
            status.0,
            status.1
        );
    }

    step("stop all and quit (OpenCode exits gracefully)");
    if std::env::var_os("CANVAS_SELFTEST_DEBUG").is_some() {
        println!("--- screen before quitting ---\n{}", ui.text());
        let reviewed = load(&canvas).ok().and_then(|d| d.node(&main).and_then(|n| n.reviewed_ms));
        println!("--- now {} reviewed {reviewed:?}; service view: {}", now_ms(), service_view(instance, &main, &native));
    }
    let exits = log_count(instance, &format!("terminal {main} exited with "));
    canvas_focus(&mut ui)?;
    ui.send(b"q")?;
    ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Stop all and quit"))?;
    ui.send(b"\x1b[C\r")?;
    ui.wait_exit(Duration::from_secs(20))?;
    let exited = wait_log_after(instance, "the TUI's exit", &format!("terminal {main} exited with "), exits, Duration::from_secs(15))?;
    let code = exited.split(&format!("terminal {main} exited with ")).nth(1).unwrap_or_default().trim().to_owned();
    anyhow::ensure!(code.starts_with("Some(0)") && !code.contains("killed"), "OpenCode did not exit gracefully ({code})");
    println!("    exited with {code}");
    let log = service_log(instance);
    for line in log.lines().filter(|l| l.contains("opencode:") || l.contains("panicked") || l.contains("history panicked")) {
        println!("    service log: {line}");
    }
    anyhow::ensure!(!log.contains("panicked"), "the service log reports a panic");
    Ok(())
}

/// With its parent stopped, a finished child session resumes in a terminal
/// of its own (`opencode -s <child>`, no prompt) as a new subsession box
/// linked to the parent; it is stopped again right after.
fn resume_child<U: Ui>(ui: &mut U, instance: &Instance, canvas: &Path, main: &str, kid: &str, name: &str) -> Result<()> {
    step("with its parent stopped, the finished child resumes in a terminal of its own as a linked subsession (opencode -s <child>)");
    canvas_focus(ui)?;
    let (x, y) = ui.locate(MAIN, SIDEBAR)?;
    ui.click(x, y)?;
    ui.wait("the stopped parent's details", Duration::from_secs(10), |t| t.contains("▸ Resume"))?;
    let (sx, sy) = ui.locate(name, SIDEBAR)?;
    ui.click(sx, sy)?;
    ui.wait("Resume as subsession", Duration::from_secs(10), |t| t.contains("Resume as subsession"))?;
    let (rx, ry) = ui.locate("Resume as subsession", RIGHT)?;
    ui.click(rx + 1, ry)?;
    let doc = wait_doc(canvas, "the child's own subsession box", |d| d.nodes.iter().any(|n| n.agent_session.as_deref() == Some(kid) && n.parent.as_deref() == Some(main)))?;
    let resumed = doc.nodes.iter().find(|n| n.agent_session.as_deref() == Some(kid)).map(|n| (n.id.clone(), n.name.clone())).context("the resumed box")?;
    wait_log(instance, "the child's resume", &format!("terminal {}: opencode resume", resumed.0), Duration::from_secs(20))?;
    let (_, line) = launched_command_line(instance, &resumed.0)?;
    let words: Vec<&str> = line.split_whitespace().collect();
    anyhow::ensure!(words.windows(2).any(|w| w[0] == "-s" && w[1] == kid), "resumed without -s {kid}: {line}");
    // The box takes the child's title once OpenCode reports it; a child's
    // view in OpenCode has "Parent up" in place of the prompt's key hints.
    let name = |fallback: &str| load(canvas).ok().and_then(|d| d.node(&resumed.0).map(|n| n.name.clone())).unwrap_or_else(|| fallback.to_string());
    ui.wait("its own OpenCode", Duration::from_secs(60), |t| t.contains(&format!("{} · ", name(&resumed.1))) && (t.contains("Parent up") || agent_ui_up(AgentKind::OpenCode, t)))?;
    let resumed = (resumed.0.clone(), name(&resumed.1));
    anyhow::ensure!(doc.node(&format!("{main}:{kid}")).is_some(), "the subagent box stays");
    // Stopped again (no prompt was sent), so only the parent resumes next.
    canvas_focus(ui)?;
    let exits = log_count(instance, &format!("terminal {} exited with ", resumed.0));
    ui.send(b"x")?;
    ui.wait("stop dialog", Duration::from_secs(5), |t| t.contains(&format!("Stop “{}”?", resumed.1)))?;
    ui.send(b"\r")?;
    let stopped = wait_log_after(instance, "the child's stop", &format!("terminal {} exited with ", resumed.0), exits, Duration::from_secs(20))?;
    println!("    child {kid} resumed as “{}” with -s, then stopped ({})", resumed.1, stopped.split("exited with ").nth(1).unwrap_or_default().trim());
    Ok(())
}

/// Deletes every OpenCode session created in `folders` since `started_ms`
/// (`opencode session delete`), after reporting the models it used and its
/// cost; fails when one used another model, cost anything, or stays.
pub(crate) fn cleanup(program: &str, env: &[(&str, &str)], model: &str, folders: &[&Path], started_ms: u64) -> Result<()> {
    let mut roots: Vec<String> = Vec::new();
    for folder in folders {
        roots.extend(opencode_history::discover_new_sessions(&folder.display().to_string(), started_ms.saturating_sub(5_000)).into_iter().map(|(id, _)| id));
    }
    if roots.is_empty() {
        println!("  · no OpenCode session to delete");
        return Ok(());
    }
    let mut problems = Vec::new();
    let mut all = Vec::new();
    for root in &roots {
        all.push(root.clone());
        all.extend(children(root).unwrap_or_default());
    }
    println!("  · OpenCode sessions created: {}", all.len());
    for id in &all {
        match usage(id) {
            Ok((models, cost, messages)) => {
                println!("    {id}: {messages} answer(s), model(s) {models:?}, cost {cost}");
                if models.iter().any(|m| m != model) || cost > 0.0 {
                    problems.push(format!("{id} used {models:?} (cost {cost})"));
                }
            }
            Err(error) => problems.push(format!("cannot read {id}: {error:#}")),
        }
    }
    let path = opencode_history::db_path();
    for id in &roots {
        if let Err(error) = agents::config::run(program, &["session", "delete", id], env, Duration::from_secs(60)) {
            problems.push(format!("opencode session delete {id}: {error:#}"));
        }
    }
    for id in &all {
        if opencode_history::session_exists(&path, id) {
            // A child the parent's deletion left behind.
            let _ = agents::config::run(program, &["session", "delete", id], env, Duration::from_secs(60));
        }
    }
    let left: Vec<&String> = all.iter().filter(|id| opencode_history::session_exists(&path, id)).collect();
    if left.is_empty() {
        println!("  · deleted (verified gone): {}", all.join(", "));
    } else {
        problems.push(format!("still in OpenCode's database: {left:?}"));
    }
    anyhow::ensure!(problems.is_empty(), "{}", problems.join("; "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_go_only_to_free_models_the_tui_runs_with() {
        for good in ["opencode/deepseek-v4.1-flash-free", "opencode/glm-5.3-flash-free"] {
            assert!(ensure_free(good).is_ok(), "{good}");
        }
        for bad in ["opencode/big-pickle", "github-copilot/claude-opus-5.5", "", "-free", " opencode/x-free", "opencode/free-not"] {
            assert!(ensure_free(bad).is_err(), "{bad:?}");
        }
        let line = r#""C:\oc\opencode.exe" --auto -m opencode/x-free --port 5"#;
        assert_eq!(model_arg(line), Some("opencode/x-free"));
        assert!(may_prompt(line, "opencode/x-free").is_ok());
        assert!(may_prompt(line, "opencode/y-free").is_err(), "another model than the TUI runs");
        assert!(may_prompt(r#""C:\oc\opencode.exe" --port 5"#, "opencode/x-free").is_err(), "no -m: OpenCode's own default could be paid");
        assert!(may_prompt(r#"opencode.exe -m opencode/big-pickle"#, "opencode/big-pickle").is_err());
    }

    #[test]
    fn tile_headers_name_the_status() {
        let screen = "│ x\n ╭ ⠋ Canvas prompt check · working · OC | Hi ─╮\n";
        assert_eq!(tile_status(screen, MAIN).as_deref(), Some("working"));
        assert_eq!(tile_status(" ✓ Canvas prompt check · done · review ", MAIN).as_deref(), Some("done · review"));
        assert_eq!(tile_status(" ◆ Canvas permission check · needs you ", ASK).as_deref(), Some("needs you"));
        assert_eq!(tile_status("nothing", MAIN), None);
    }
}
