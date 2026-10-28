//! Copilot Canvas: a spatial session manager for agent CLIs.

mod agent_prompt;
mod bench;
mod fake_copilot;
mod gui_bench;
mod hardening;
mod kitty_demo;
mod pair_probe;
mod parity;
mod remote_cli;
mod remote_probe;
mod selftest;
mod soak;
mod standin;

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
  copilot-canvas --tui [options]        open the terminal client (whatever the client setting says)
  copilot-canvas config [client|agent|engine [VALUE]]
                                        machine settings: client tui|gui|auto (which one
                                        `copilot-canvas` opens), agent copilot|opencode|claude|auto,
                                        engine ghostty|alacritty|auto
  copilot-canvas --remote loopback      open the canvas over TCP to the local service (remote mode
                                        without a tunnel; starts the service listening on loopback)
  copilot-canvas --remote NAME          open the canvas on a paired service (see REMOTE; runs
                                        `devtunnel connect` for a tunnel and reconnects through it)
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
                                        the GPU client: first paint, frame cost (full and damage frames),
                                        panning, keystroke to present, idle CPU and memory (over its test hooks)
  copilot-canvas bench --client gui --stress [--sessions N --agents M]
                                        N live sessions from busy synthetic histories (M subagents each)
  copilot-canvas bench --client gui --perf-canvas FILE|synthetic
                                        panning a copy of a canvas file at 100, 50 and 25 %
  copilot-canvas bench --client gui --burn N
                                        keystroke to present while N sessions burn CPU, with and
                                        without the priority manager
  copilot-canvas selftest [--client tui|gui] [--remote loopback]
                                        drive the real UI end to end (TUI: in a hidden pseudoconsole;
                                        GUI: over its test hooks)
  copilot-canvas selftest --client gui --parity
                                        one GUI check per row of the parity matrix (PLAN §8), with a
                                        table; demo shells and a stand-in Copilot CLI only
  copilot-canvas selftest --client gui --hardening
                                        GPU device loss, DPI change, window placement, the UI Automation
                                        tree, the crash log, software rendering, visual regressions
  copilot-canvas selftest --remote faults
                                        drop, cut and delay the TCP connection while typing; check
                                        reconnect, resync and that no keystroke is lost or doubled
  copilot-canvas selftest --remote soak [--minutes N] [--drop-every SECONDS] [--client tui|gui]
                                        a client connected through a fault-injecting proxy for N minutes:
                                        drops, outages, cuts and delays; exactly-once typing, reconnect and
                                        resync times, memory and handle growth, bandwidth; JSON report
  copilot-canvas selftest --remote paired
                                        pairing, a Dev Tunnel stand-in, the canvas through it, drops,
                                        a killed tunnel child, revocation, wrong secrets
  copilot-canvas selftest agent [--agent copilot|opencode|claude]
                                        the real agent TUI through the canvas; nothing is submitted
  copilot-canvas selftest agent --agent opencode --prompt [--model ID] [--client tui|gui] [--steps LIST]
                                        tiny prompts to a free OpenCode model (ids ending in -free;
                                        default opencode/deepseek-v4.1-flash-free) through the canvas:
                                        re-keying, status, wheel, push, subagent, question, resume,
                                        permission, passive (LIST: some of them, comma-separated);
                                        deletes every session it created

REMOTE (docs/REMOTE.md; agents on the Cloud PC, the canvas at home over a private Dev Tunnel)
  on the Cloud PC:
  copilot-canvas remote tunnel-setup [--tunnel ID] [--port PORT]
                                        write the CopilotCanvas-Tunnel automation, set remote.listen,
                                        print the one-time devtunnel commands
  copilot-canvas remote pair [--name NAME]
                                        print a one-time pairing code (10 minutes, single use)
  copilot-canvas remote clients         paired clients (last seen, connected, revoked)
  copilot-canvas remote revoke ID|NAME  revoke a client and drop its connections
  at home:
  copilot-canvas remote add NAME (--tunnel ID [--port PORT] | --addr HOST:PORT) --code CODE
                 [--vscode-tunnel NAME] [--client-name NAME]
                                        pair with the service; the secret is stored with DPAPI
  copilot-canvas remote servers         paired services
  copilot-canvas remote forget NAME     remove a paired service from this machine

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
  --remote TARGET      connect over TCP: `loopback`, a paired server's name or HOST:PORT (see above)
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
    tui: bool,
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
        tui: false,
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
            "--tui" => args.tui = true,
            "--instance" => args.instance = Some(value("--instance")?),
            "--home" => args.home = Some(PathBuf::from(value("--home")?)),
            "--sessions-dir" => args.sessions_dir = Some(PathBuf::from(value("--sessions-dir")?)),
            "--canvas" => args.canvas = Some(PathBuf::from(value("--canvas")?)),
            "--cwd" => args.cwd = Some(PathBuf::from(value("--cwd")?)),
            "--remote" => args.remote = Some(value("--remote")?),
            "--yes" | "--force" | "--parity" | "--stress" | "--hardening" | "--prompt" => args.flags.push(arg.trim_start_matches("--").to_owned()),
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

/// Whether `copilot-canvas` (no command) opens the GPU client: the `client`
/// setting (`COPILOT_CANVAS_CLIENT`, else settings.json): `gui`, `tui`
/// (the default until the dogfood week ends, PLAN Phase 7) or `auto` (the GUI
/// unless this is a remote desktop or SSH session, where the TUI is better,
/// PLAN D4). The GUI must be next to this program.
fn wants_gui(home: Option<&std::path::Path>) -> bool {
    let home = home.map(std::path::Path::to_path_buf).or_else(|| std::env::var_os("COPILOT_CANVAS_HOME").map(PathBuf::from)).or_else(|| dirs::home_dir().map(|h| h.join(".copilot-canvas")));
    let setting = home.map(|h| canvas_common::settings::Settings::load(&h)).unwrap_or_default().client().unwrap_or_default();
    let gui_present = std::env::current_exe().map(|e| e.with_file_name("copilot-canvas-gui.exe").is_file()).unwrap_or(false);
    match setting.trim().to_ascii_lowercase().as_str() {
        "gui" => gui_present,
        "auto" => gui_present && !remote_session(),
        _ => false,
    }
}

/// A remote desktop (Cloud PC over RDP: no GPU) or an SSH session.
fn remote_session() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
    // SAFETY: a plain query.
    let rdp = unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0;
    rdp || std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_CLIENT").is_some()
}

/// `copilot-canvas config [KEY [VALUE]]`: machine settings (settings.json).
fn config(home: &std::path::Path, key: Option<&str>, value: Option<&str>) -> Result<()> {
    // A malformed file is reported, never overwritten.
    let mut settings = canvas_common::settings::Settings::load_checked(home)?;
    let Some(key) = key else {
        println!("client  {}", settings.client.as_deref().unwrap_or("tui (default)"));
        println!("agent   {}", settings.agent.as_deref().unwrap_or("auto (first installed)"));
        println!("engine  {}", settings.terminal.engine.as_deref().unwrap_or("auto (ghostty when built in)"));
        println!("listen  {}", settings.remote.listen.as_deref().unwrap_or("off"));
        println!("({})", canvas_common::settings::path(home).display());
        return Ok(());
    };
    let unset = |v: &str| matches!(v, "default" | "unset" | "none");
    let value = value.with_context(|| format!("config {key} needs a value"))?.trim().to_ascii_lowercase();
    match key {
        "client" => {
            anyhow::ensure!(matches!(value.as_str(), "tui" | "gui" | "auto") || unset(&value), "client is tui, gui or auto");
            settings.client = (!unset(&value)).then_some(value);
        }
        "agent" => {
            anyhow::ensure!(value == "auto" || unset(&value) || canvas_agents::agents::AgentKind::parse(&value).is_some(), "agent is copilot, opencode, claude or auto");
            settings.agent = (!(value == "auto" || unset(&value))).then(|| canvas_agents::agents::AgentKind::parse(&value).map(|k| k.id().to_owned()).unwrap_or(value));
        }
        "engine" => {
            anyhow::ensure!(matches!(value.as_str(), "ghostty" | "alacritty" | "auto") || unset(&value), "engine is ghostty, alacritty or auto");
            settings.terminal.engine = (!(value == "auto" || unset(&value))).then_some(value);
            println!("New terminals use it after the service restarts (copilot-canvas service stop).");
        }
        other => bail!("unknown setting {other:?} (client, agent, engine)"),
    }
    settings.save(home)?;
    println!("{key} = {}", match key {
        "client" => settings.client.clone(),
        "agent" => settings.agent.clone(),
        _ => settings.terminal.engine.clone(),
    }.unwrap_or_else(|| "default".into()));
    Ok(())
}

/// `copilot-canvas --gui …`: runs the GPU client next to this program with
/// the same options, and exits with its exit code.
fn launch_gui() -> Result<()> {
    let exe = std::env::current_exe()?.with_file_name("copilot-canvas-gui.exe");
    anyhow::ensure!(exe.is_file(), "the GPU client {} is missing", exe.display());
    let args: Vec<String> = std::env::args().skip(1).filter(|a| a != "--gui" && a != "--tui").collect();
    if canvas_core::platform::other_client_dryrun().is_some() {
        return canvas_core::platform::spawn_client(&exe, &args, false);
    }
    let status = std::process::Command::new(&exe).args(&args).status().with_context(|| format!("cannot start {}", exe.display()))?;
    std::process::exit(status.code().unwrap_or(1));
}

fn real_main() -> Result<()> {
    let args = parse()?;
    if args.command.is_empty() && !args.tui && (args.gui || wants_gui(args.home.as_deref())) {
        return launch_gui();
    }
    // The tunnel stand-in touches no data folder.
    if let [first, rest @ ..] = args.command.as_slice()
        && first == "devtunnel-standin"
    {
        return standin::run(&rest.iter().map(String::as_str).collect::<Vec<_>>());
    }
    let instance = paths::Instance::new(args.instance.clone(), args.home.clone(), args.sessions_dir.clone())?;
    let option = |name: &str| args.rest.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let options = |name: &str| args.rest.iter().filter(|(k, _)| k == name).map(|(_, v)| v.clone()).collect::<Vec<_>>();
    let flag = |name: &str| args.flags.iter().any(|f| f == name);
    let dir = || factory_cli::configured_dir(&instance.home, option("dir"));
    let owned = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let port = |name: &str| -> Result<Option<u16>> { option(name).map(|p| p.parse::<u16>().with_context(|| format!("--{name} takes a port number"))).transpose() };
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
        ["config"] => config(&instance.home, None, None),
        ["config", key] => config(&instance.home, Some(key), None),
        ["config", key, value] => config(&instance.home, Some(key), Some(value)),
        ["doctor"] => selftest::doctor(&instance),
        ["remote", "pair"] => remote_cli::pair(&instance, option("name")),
        ["remote", "clients"] => remote_cli::clients(&instance),
        ["remote", "revoke", client] => remote_cli::revoke(&instance, client),
        ["remote", "tunnel-setup"] => remote_cli::tunnel_setup(&instance, &dir(), option("tunnel"), port("port")?),
        ["remote", "add", name] => remote_cli::add(
            &instance,
            remote_cli::AddArgs {
                name: name.to_string(),
                tunnel: option("tunnel"),
                port: port("port")?,
                addr: option("addr"),
                code: option("code").context("--code CODE is required (from `copilot-canvas remote pair` on the service's machine)")?,
                vscode_tunnel: option("vscode-tunnel"),
                client_name: option("client-name"),
            },
        ),
        ["remote", "servers"] => remote_cli::servers(&instance),
        ["remote", "forget", name] => remote_cli::forget(&instance, name),
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
            if flag("hardening") {
                anyhow::ensure!(client == selftest::Client::Gui, "--hardening checks the GUI: add --client gui");
                return hardening::run();
            }
            match args.remote.as_deref() {
                None => selftest::run(None, client),
                Some("loopback") => selftest::run(Some("loopback"), client),
                Some("faults") => remote_probe::faults(&std::env::current_exe()?),
                Some("paired") => pair_probe::run(&std::env::current_exe()?),
                Some("soak") => soak::run(soak::Options {
                    minutes: option("minutes").map(|v| v.parse()).transpose()?.unwrap_or(20.0),
                    drop_every: option("drop-every").map(|v| v.parse()).transpose()?.unwrap_or(30),
                    gui: client == selftest::Client::Gui,
                    json: option("json").map(PathBuf::from),
                }),
                Some(other) => bail!("selftest --remote takes `loopback`, `faults`, `paired` or `soak`, not {other:?}"),
            }
        }
        ["selftest", "agent"] if flag("prompt") => {
            let agent = option("agent").unwrap_or_else(|| "opencode".into());
            anyhow::ensure!(
                canvas_agents::agents::AgentKind::parse(&agent) == Some(canvas_agents::agents::AgentKind::OpenCode),
                "--prompt sends prompts, and only to OpenCode with a free model: use --agent opencode"
            );
            agent_prompt::run(agent_prompt::Options {
                model: option("model"),
                client: match option("client").as_deref() {
                    None | Some("tui") => selftest::Client::Tui,
                    Some("gui") => selftest::Client::Gui,
                    Some(other) => bail!("selftest --client takes `tui` or `gui`, not {other:?}"),
                },
                steps: option("steps").map(|s| s.split(',').map(|p| p.trim().to_owned()).filter(|p| !p.is_empty()).collect()),
            })
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
            probe: if flag("stress") {
                gui_bench::Probe::Stress {
                    sessions: option("sessions").map(|v| v.parse()).transpose()?.unwrap_or(100),
                    agents: option("agents").map(|v| v.parse()).transpose()?.unwrap_or(12),
                }
            } else if let Some(file) = option("perf-canvas") {
                gui_bench::Probe::PerfCanvas(file)
            } else if let Some(burn) = option("burn") {
                gui_bench::Probe::Burn(burn.parse()?)
            } else {
                gui_bench::Probe::Standard
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
        ["echo"] => bench::echo(option("tag").unwrap_or_default()),
        ["burn"] => bench::burn(),
        ["kitty-demo"] => kitty_demo::run(None),
        ["kitty-demo", mode] => kitty_demo::run(Some(mode)),
        other => bail!("unknown command {:?}\n\n{USAGE}", other.join(" ")),
    }
}
