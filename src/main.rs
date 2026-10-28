//! Copilot Canvas: a spatial session manager for agent CLIs.

mod bench;
mod fake_copilot;
mod gui_bench;
mod kitty_demo;
mod parity;
mod remote_probe;
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
Copilot Canvas — spatial session manager for agent CLIs (GitHub Copilot, OpenCode, Claude Code)

USAGE
  copilot-canvas [options]              open the canvas (starts the terminal service if needed)
  copilot-canvas --demo                 safe demo with native PowerShell panes, no Copilot calls
  copilot-canvas --gui [options]        open the GPU client (copilot-canvas-gui.exe next to this program)
  copilot-canvas --remote loopback      open the canvas over TCP to the local service (remote mode
                                        without a tunnel; starts the service listening on loopback)
  copilot-canvas --remote HOST:PORT     open the canvas on a remote service (a forwarded tunnel port;
                                        credentials: COPILOT_CANVAS_REMOTE_ID, COPILOT_CANVAS_REMOTE_SECRET)
  copilot-canvas service status|stop    inspect or stop the terminal service (stop ends its sessions)
  copilot-canvas service start          start it in the background (for a logon task: passive sessions
                                        and the automation doctor then run without the canvas open)
  copilot-canvas doctor                 check Copilot, ConPTY, session history and the service
  copilot-canvas bench [--terminals N] [--seconds S] [--json FILE] [--remote loopback]
                                        measure service/terminal/rendering performance (no Copilot);
                                        --remote loopback: over TCP, with and without compression
  copilot-canvas bench --client gui [--seconds S] [--json FILE] [--remote loopback]
                                        the GPU client: first paint, frame cost, panning, keystroke
                                        to present, idle CPU and memory (over its test hooks)
  copilot-canvas selftest [--client tui|gui] [--remote loopback]
                                        drive the real UI end to end (TUI: in a hidden pseudoconsole;
                                        GUI: over its test hooks)
  copilot-canvas selftest --client gui --parity
                                        one GUI check per row of the parity matrix (PLAN §8), with a
                                        table; demo shells and a stand-in Copilot CLI only
  copilot-canvas selftest --remote faults
                                        drop, cut and delay the TCP connection while typing; check
                                        reconnect, resync and that no keystroke is lost or doubled

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
  --canvas FILE        canvas file (default: <home>/canvas.json; with --remote, a path on the service's machine)
  --cwd DIR            default folder for new sessions (default: current folder)
  --remote TARGET      connect over TCP: `loopback` or HOST:PORT (see above)
  -h, --help · -V, --version
";

struct Args {
    command: Vec<String>,
    instance: Option<String>,
    home: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
    canvas: Option<PathBuf>,
    cwd: Option<PathBuf>,
    remote: Option<String>,
    demo: bool,
    perf: bool,
    gui: bool,
    rest: Vec<(String, String)>,
    flags: Vec<String>,
}

fn parse() -> Result<Args> {
    let mut args = Args {
        command: Vec::new(),
        instance: None,
        home: None,
        sessions_dir: None,
        canvas: None,
        cwd: None,
        remote: None,
        demo: false,
        perf: false,
        gui: false,
        rest: Vec::new(),
        flags: Vec::new(),
    };
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
            "--gui" => args.gui = true,
            "--instance" => args.instance = Some(value("--instance")?),
            "--home" => args.home = Some(PathBuf::from(value("--home")?)),
            "--sessions-dir" => args.sessions_dir = Some(PathBuf::from(value("--sessions-dir")?)),
            "--canvas" => args.canvas = Some(PathBuf::from(value("--canvas")?)),
            "--cwd" => args.cwd = Some(PathBuf::from(value("--cwd")?)),
            "--remote" => args.remote = Some(value("--remote")?),
            "--yes" | "--force" | "--parity" => args.flags.push(arg.trim_start_matches("--").to_owned()),
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
    if fake_copilot::active() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        std::process::exit(match fake_copilot::run(&args) {
            Ok(()) => 0,
            Err(_) => 1,
        });
    }
    if let Err(error) = real_main() {
        eprintln!("copilot-canvas: {error:#}");
        std::process::exit(1);
    }
}

/// `copilot-canvas --gui …`: runs the GPU client next to this program with
/// the same options, and exits with its exit code.
fn launch_gui() -> Result<()> {
    let exe = std::env::current_exe()?.with_file_name("copilot-canvas-gui.exe");
    anyhow::ensure!(exe.is_file(), "the GPU client {} is missing", exe.display());
    let args: Vec<String> = std::env::args().skip(1).filter(|a| a != "--gui").collect();
    let status = std::process::Command::new(&exe).args(&args).status().with_context(|| format!("cannot start {}", exe.display()))?;
    std::process::exit(status.code().unwrap_or(1));
}

fn real_main() -> Result<()> {
    let args = parse()?;
    if args.gui && args.command.is_empty() {
        return launch_gui();
    }
    let instance = paths::Instance::new(args.instance.clone(), args.home.clone(), args.sessions_dir.clone())?;
    let option = |name: &str| args.rest.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let options = |name: &str| args.rest.iter().filter(|(k, _)| k == name).map(|(_, v)| v.clone()).collect::<Vec<_>>();
    let flag = |name: &str| args.flags.iter().any(|f| f == name);
    let dir = || factory_cli::configured_dir(&instance.home, option("dir"));
    let owned = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match args.command.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => {
            let cwd = args.cwd.clone().map(Ok).unwrap_or_else(std::env::current_dir)?;
            let local_backend = std::sync::Arc::new(canvas_agents::backend::LocalBackend::new(canvas_agents::backend::Ctx::new(&instance)));
            client::run(client::Options {
                instance,
                demo: args.demo,
                perf: args.perf,
                canvas: args.canvas.clone(),
                cwd: paths::absolute(&cwd).to_string_lossy().into_owned(),
                remote: args.remote.clone(),
                local_backend,
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
        ["selftest"] => {
            let client = match option("client").as_deref() {
                None | Some("tui") => selftest::Client::Tui,
                Some("gui") => selftest::Client::Gui,
                Some(other) => bail!("selftest --client takes `tui` or `gui`, not {other:?}"),
            };
            if flag("parity") {
                anyhow::ensure!(client == selftest::Client::Gui, "--parity checks the GUI: add --client gui");
                return parity::run(args.remote.as_deref());
            }
            match args.remote.as_deref() {
                None => selftest::run(None, client),
                Some("loopback") => selftest::run(Some("loopback"), client),
                Some("faults") => remote_probe::faults(&std::env::current_exe()?),
                Some(other) => bail!("selftest --remote takes `loopback` or `faults`, not {other:?}"),
            }
        }
        ["selftest", "agent"] => selftest::agent(option("agent").as_deref()),
        ["selftest", "copilot"] => selftest::agent(Some("copilot")),
        ["record", rest @ ..] => canvas_corpus::cli::record(&instance.home, rest, &args.rest, args.cwd.as_deref()),
        ["corpus", rest @ ..] => canvas_corpus::cli::corpus(rest, &args.rest),
        ["bench"] if option("client").as_deref() == Some("gui") => gui_bench::run(gui_bench::Options {
            seconds: option("seconds").map(|v| v.parse()).transpose()?.unwrap_or(5),
            json: option("json").map(PathBuf::from),
            remote: match args.remote.as_deref() {
                None => false,
                Some("loopback") => true,
                Some(other) => bail!("bench --remote takes `loopback`, not {other:?}"),
            },
        }),
        ["bench"] => bench::run(bench::Options {
            terminals: option("terminals").map(|v| v.parse()).transpose()?.unwrap_or(100),
            seconds: option("seconds").map(|v| v.parse()).transpose()?.unwrap_or(10),
            json: option("json").map(PathBuf::from),
            burn: option("burn").map(|v| v.parse()).transpose()?.unwrap_or(0),
            remote: match args.remote.as_deref() {
                None => false,
                Some("loopback") => true,
                Some(other) => bail!("bench --remote takes `loopback`, not {other:?}"),
            },
        }),
        ["noise"] => bench::noise(option("rate").map(|v| v.parse()).transpose()?.unwrap_or(20)),
        ["echo"] => bench::echo(),
        ["burn"] => bench::burn(),
        ["kitty-demo"] => kitty_demo::run(None),
        ["kitty-demo", mode] => kitty_demo::run(Some(mode)),
        other => bail!("unknown command {:?}\n\n{USAGE}", other.join(" ")),
    }
}
