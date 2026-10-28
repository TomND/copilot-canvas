//! Copilot Canvas: a spatial session manager for agent CLIs.

mod bench;
mod selftest;

use canvas_agents::{automations, cli, factory_cli, index, service, vscode};
#[cfg(test)]
use canvas_agents::{bus, copilot_cfg, taskschd};
use canvas_common::{paths, protocol, win};

/// The TUI front end plus the shared core (`client::conn`, `client::doc`, …).
mod client {
    pub use canvas_core::*;
    pub use canvas_tui::{Options, run};
}

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

const USAGE: &str = "\
Copilot Canvas — spatial session manager for GitHub Copilot CLI

USAGE
  copilot-canvas [options]              open the canvas (starts the terminal service if needed)
  copilot-canvas --demo                 safe demo with native PowerShell panes, no Copilot calls
  copilot-canvas service status|stop    inspect or stop the terminal service (stop ends its sessions)
  copilot-canvas service start          start it in the background (for a logon task: passive sessions
                                        and the automation doctor then run without the canvas open)
  copilot-canvas doctor                 check Copilot, ConPTY, session history and the service
  copilot-canvas bench [--terminals N] [--seconds S] [--json FILE]
                                        measure service/terminal/rendering performance (no Copilot)
  copilot-canvas selftest               drive the real UI end to end in a hidden pseudoconsole

GOLDEN CORPUS (recorded terminal output vs. each engine's screen; see corpus/README.md)
  copilot-canvas record (--out FILE | --entry NAME) [--cols N] [--rows M] [--input SCRIPT] [--seconds S]
                 [--env K=V]… -- PROGRAM [ARGS…]
                                        record a program in a pseudoconsole (scripted input optional)
  copilot-canvas record --session ID (--out FILE | --entry NAME) [--dir DIR]
                                        copy a live terminal's recording (service run with COPILOT_CANVAS_RECORD)
  copilot-canvas corpus list|synth|show NAME|diff [NAME…]
  copilot-canvas corpus check|bless [--engine NAME] [NAME…]
                                        compare every engine with the expected screens, or rewrite them

EVENTS (run by sessions on the canvas; see the canvas-events skill)
  copilot-canvas events                 this session's group: events, passive sessions, sessions
  copilot-canvas emit TYPE [--data JSON] [--to NAME]
  copilot-canvas send --to NAME TEXT    message a session (typed in when it is idle)
  copilot-canvas status ok|warn|alert|done HEADLINE      (passive sessions)
  copilot-canvas followup --in 24h|--at 09:00 NOTE        (passive sessions)

AUTOMATIONS (scheduled tasks defined by manifests; folder: --dir, COPILOT_CANVAS_AUTOMATIONS,
             the canvas setting, or <home>/automations)
  copilot-canvas automations [list]     every automation: task state, last result, report, drift
  copilot-canvas automations show NAME  schedule, runs, report, logs and docs of one automation
  copilot-canvas automations import [NAME…] [--family F] [--force]
                                        write manifests for existing tasks (default: launcher tasks)
  copilot-canvas automations apply [NAME…] [--yes]
                                        show (or with --yes make) the Task Scheduler changes
  copilot-canvas automations run|enable|disable NAME
  copilot-canvas automations report ok|warn|alert|failed HEADLINE [--summary TEXT | --summary-file F]
                 [--link TITLE=URL]… [--metric NAME=VALUE[=STATE]]… [--name NAME]
                                        status report of the running automation (AUTOMATION_STATUS_FILE)

OPTIONS
  --instance NAME      separate service + canvas namespace (default: default)
  --home DIR           data directory (default: ~/.copilot-canvas, or COPILOT_CANVAS_HOME)
  --sessions-dir DIR   Copilot session history to index (default: ~/.copilot/session-state)
  --canvas FILE        canvas file (default: <home>/canvas.json)
  --cwd DIR            default folder for new sessions (default: current folder)
  -h, --help · -V, --version
";

struct Args {
    command: Vec<String>,
    instance: Option<String>,
    home: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
    canvas: Option<PathBuf>,
    cwd: Option<PathBuf>,
    demo: bool,
    perf: bool,
    rest: Vec<(String, String)>,
    flags: Vec<String>,
}

fn parse() -> Result<Args> {
    let mut args = Args { command: Vec::new(), instance: None, home: None, sessions_dir: None, canvas: None, cwd: None, demo: false, perf: false, rest: Vec::new(), flags: Vec::new() };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let mut value = |name: &str| iter.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("copilot-canvas {}", paths::build_id());
                std::process::exit(0);
            }
            "--demo" => args.demo = true,
            "--perf" => args.perf = true,
            "--instance" => args.instance = Some(value("--instance")?),
            "--home" => args.home = Some(PathBuf::from(value("--home")?)),
            "--sessions-dir" => args.sessions_dir = Some(PathBuf::from(value("--sessions-dir")?)),
            "--canvas" => args.canvas = Some(PathBuf::from(value("--canvas")?)),
            "--cwd" => args.cwd = Some(PathBuf::from(value("--cwd")?)),
            "--yes" | "--force" => args.flags.push(arg.trim_start_matches("--").to_owned()),
            "--" => {
                // `record … -- PROGRAM ARGS…`: the rest belongs to the program.
                args.command.push(arg);
                args.command.extend(iter.by_ref());
            }
            flag if flag.starts_with("--") => {
                let v = value(flag)?;
                args.rest.push((flag.trim_start_matches("--").to_owned(), v));
            }
            word => args.command.push(word.to_owned()),
        }
    }
    Ok(args)
}

fn main() {
    if let Err(error) = real_main() {
        eprintln!("copilot-canvas: {error:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let args = parse()?;
    let instance = paths::Instance::new(args.instance.clone(), args.home.clone(), args.sessions_dir.clone())?;
    let option = |name: &str| args.rest.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let options = |name: &str| args.rest.iter().filter(|(k, _)| k == name).map(|(_, v)| v.clone()).collect::<Vec<_>>();
    let flag = |name: &str| args.flags.iter().any(|f| f == name);
    let dir = || factory_cli::configured_dir(&instance.home, option("dir"));
    let owned = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match args.command.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => {
            let cwd = args.cwd.clone().map(Ok).unwrap_or_else(std::env::current_dir)?;
            client::run(client::Options {
                instance,
                demo: args.demo,
                perf: args.perf,
                canvas: args.canvas.clone(),
                cwd: paths::absolute(&cwd).to_string_lossy().into_owned(),
            })
        }
        ["service"] => service::run(instance),
        ["service", "start"] => {
            if client::conn::connect(&instance, false).is_ok() {
                println!("The service is already running.");
                return Ok(());
            }
            client::conn::spawn_service(&instance)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(300));
                if client::conn::connect(&instance, false).is_ok() {
                    println!("Service started.");
                    return Ok(());
                }
            }
            bail!("the service did not start within 20 s")
        }
        ["service", "stop"] => {
            let conn = client::conn::connect(&instance, false).context("no service is running")?;
            conn.tx.send(protocol::ClientMsg::Shutdown)?;
            std::thread::sleep(std::time::Duration::from_millis(300));
            println!("Stop requested; Copilot sessions get a few seconds to exit gracefully.");
            Ok(())
        }
        ["service", "status"] => match client::conn::connect(&instance, false) {
            Ok(conn) => {
                conn.tx.send(protocol::ClientMsg::Stats)?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while std::time::Instant::now() < deadline {
                    if let Ok(protocol::ServiceMsg::Stats(stats)) = conn.rx.recv_timeout(std::time::Duration::from_millis(200)) {
                        let report = serde_json::json!({
                            "running": true,
                            "build": conn.service_build,
                            "pid": stats.pid,
                            "terminals": stats.terminals,
                            "alive": stats.alive,
                            "private_mb": stats.private_bytes / (1024 * 1024),
                            "working_set_mb": stats.working_set_bytes / (1024 * 1024),
                            "cpu_ms": stats.cpu_ms,
                            "uptime_s": stats.uptime_ms / 1000,
                        });
                        println!("{}", serde_json::to_string_pretty(&report)?);
                        return Ok(());
                    }
                }
                bail!("the service did not answer")
            }
            Err(_) => {
                println!("{{\"running\": false}}");
                Ok(())
            }
        },
        ["events"] => cli::events(&instance.home),
        ["emit", kind] => cli::emit(&instance.home, kind, option("data"), option("to"), option("group")),
        ["send", text @ ..] => cli::send(&instance.home, &option("to").unwrap_or_default(), &text.join(" ")),
        ["status", state, headline @ ..] => cli::status(&instance.home, state, &headline.join(" ")),
        ["followup", note @ ..] => cli::followup(&instance.home, option("in"), option("at"), &note.join(" ")),
        ["automations"] | ["automations", "list"] => factory_cli::list(&dir()),
        ["automations", "show", name] => factory_cli::show(&dir(), name),
        ["automations", "import", names @ ..] => factory_cli::import(&dir(), &owned(names), option("family"), flag("force")),
        ["automations", "apply", names @ ..] => factory_cli::apply(&dir(), &owned(names), flag("yes")),
        ["automations", "run", name] => factory_cli::run(&dir(), name),
        ["automations", "enable", name] => factory_cli::set_enabled(&dir(), name, true),
        ["automations", "disable", name] => factory_cli::set_enabled(&dir(), name, false),
        ["automations", "report", state, headline @ ..] => factory_cli::report(
            &dir(),
            factory_cli::ReportArgs {
                state: state.to_string(),
                headline: headline.join(" "),
                summary: option("summary"),
                summary_file: option("summary-file"),
                links: options("link"),
                metrics: options("metric"),
                name: option("name"),
            },
        ),
        ["doctor"] => selftest::doctor(&instance),
        ["selftest"] => selftest::run(),
        ["selftest", "copilot"] => selftest::copilot(),
        ["record", rest @ ..] => canvas_corpus::cli::record(&instance.home, rest, &args.rest, args.cwd.as_deref()),
        ["corpus", rest @ ..] => canvas_corpus::cli::corpus(rest, &args.rest),
        ["bench"] => bench::run(
            option("terminals").map(|v| v.parse()).transpose()?.unwrap_or(100),
            option("seconds").map(|v| v.parse()).transpose()?.unwrap_or(10),
            option("json").map(PathBuf::from),
            option("burn").map(|v| v.parse()).transpose()?.unwrap_or(0),
        ),
        ["noise"] => bench::noise(option("rate").map(|v| v.parse()).transpose()?.unwrap_or(20)),
        ["echo"] => bench::echo(),
        ["burn"] => bench::burn(),
        other => bail!("unknown command {:?}\n\n{USAGE}", other.join(" ")),
    }
}
