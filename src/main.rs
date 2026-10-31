//! Copilot Canvas: a spatial session manager for agent CLIs.

mod agent_prompt;
mod bench;
mod fake_copilot;
mod fake_gh;
mod golden;
mod gui_bench;
mod hardening;
mod kitty_demo;
mod pair_probe;
mod parity;
mod paste_probe;
mod remote_cli;
mod remote_probe;
mod selftest;
mod soak;
mod standin;
#[cfg(windows)]
mod uia;
#[cfg(not(windows))]
#[path = "uia_none.rs"]
mod uia;

use canvas_agents::{automations, cli, factory_cli, index, prs_cli, reports, reviews_cli, service, vscode};
#[cfg(test)]
use canvas_agents::{bus, copilot_cfg, taskschd};
use canvas_common::{paths, protocol};
use canvas_sys::process as win;

/// The TUI front end plus the shared core (`client::conn`, `client::doc`, …).
mod client {
    pub use canvas_core::*;
    pub use canvas_tui::{Options, run};
}

use std::path::{Path, PathBuf};

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
  copilot-canvas bench --client gui --effects   CPU and frame rate with effects in view, out of view, cleared
  copilot-canvas bench --client gui --loops     CPU and frame rate with looping sessions in view, panning, out of view, reduced motion
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
  copilot-canvas selftest --client gui --golden [--update]
                                        golden screenshots of scripted GUI states compared with
                                        tests/golden/gui/*.png (tolerant image diff); --update rewrites them
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
  copilot-canvas selftest agent --paste [--agent copilot|opencode|claude]
                                        a multi-line paste into the real agent's input box arrives as
                                        lines and sends nothing (OpenCode with a free model as every
                                        default; Claude Code in a throw-away configuration)
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
  copilot-canvas remote pair [--name NAME] [--qr [--url wss://TUNNEL-PORT.REGION.devtunnels.ms]]
                                        print a one-time pairing code (10 minutes, single use);
                                        --qr: also a QR code for the iPhone app (docs/IOS-PLAN.md)
  copilot-canvas remote clients         paired clients (last seen, connected, revoked)
  copilot-canvas remote push-key AuthKey_ID.p8 --team TEAMID [--key-id ID]
                                        the APNs key for iPhone notifications (developer.apple.com › Keys)
  copilot-canvas remote push-test       a test notification to every registered iPhone
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
  copilot-canvas events                 this session's group: events, passive sessions, sessions, and the
                                        canvas-wide agents (a canvas-wide agent: every group of the canvas)
  copilot-canvas emit TYPE [--data JSON] [--to NAME]
                                        --to NAME: that passive session or canvas-wide agent only;
                                        --to canvas: the canvas-wide agents only
  copilot-canvas send --to NAME TEXT    message a session (typed in when it is idle): your group's
                                        sessions and the canvas-wide agents (they reach every session)
  copilot-canvas status ok|warn|alert|done HEADLINE      (passive sessions)
  copilot-canvas followup --in 24h|--at 09:00 NOTE        (passive sessions)
  copilot-canvas effect fireworks|attention|sparkle|alert --on NAME [--note TEXT]
                                        an effect on a box of this session's group (or --on group;
                                        canvas-wide agents: any box) until someone clicks it;
                                        effect clear --on NAME takes it off;
                                        effect alone lists the effects collection

REPORTS (the canvas inbox, Reports in the activity bar; see the canvas-reports skill)
  copilot-canvas report publish (--markdown FILE|- | --html FILE|- | --message TEXT) [--title TITLE]
                 [--kind info|ok|warn|alert|queued|delivered|failed] [--pr URL] [--link TITLE=URL]…
                 [--session NAME|ID] [--thread KEY] [--source NAME]
                                        publish a report or a short message (at most 2 MiB; links are
                                        http(s) or mailto). Inside a canvas session it comes from that
                                        session; elsewhere name the producer with --source. Prints
                                        whether it was stored or queued (the service is not running);
                                        exits non-zero with the reason when it is refused
  copilot-canvas report list [--unread] [--limit N]
                                        stored, queued and refused reports

PULL REQUESTS (a canvas-wide PR coordinator follows them; see the canvas-prs skill)
  copilot-canvas pr track URL [--session NAME|ID] [--title TEXT]
                                        hand a GitHub or Azure DevOps pull request to the canvas: it is
                                        tracked for this session (or --session), checked read-only with
                                        gh / az for new review comments, failing checks and its end
  copilot-canvas pr untrack URL         stop tracking it (its owner, canvas-wide agents, you)
  copilot-canvas pr check [URL]         check now (one, or every active one)
  copilot-canvas pr dispatch URL --key KEY [--event TYPE] TEXT
                                        a follow-up for its owning session (PR coordinators and you):
                                        one per key, typed in when the session is idle; queued,
                                        delivered, blocked and failed show in Reports
  copilot-canvas pr list [--format json]
                                        tracked pull requests, their monitor and follow-ups

REVIEW FOLLOW-UPS (a canvas-wide agent messages the authors of PRs you reviewed; canvas-review-followup skill)
  copilot-canvas review handoff --pr URL --author NAME [--author-id ID] --results FILE|- [--notes FILE]
                 [--outcome approved|changes-requested|commented] [--title T] [--link TITLE=URL]… [--session NAME|ID]
                                        a finished review: the agent drafts the message to its author
                                        (you approve it first by default); --notes stay private
  copilot-canvas review message REVIEW --draft FILE|- | --sent --conversation ID [--recipient NAME]
                 | --needs-you WHY | --failed WHY   the agent's message to the author (one per review)
  copilot-canvas review checked [--conversation ID] [--mark TOKEN] [--failed WHY]
                                        the agent's hourly check of the watched conversations
  copilot-canvas review request --conversation ID --message-id ID --kind re-review|another-review|follow-up
                 --text TEXT [--pr URL]   an author's request: recorded once, routed to the review session
  copilot-canvas review done REQUEST    the review session finished a request without a new hand-off
  copilot-canvas review list [--format json]
                                        reviews, their messages, watched conversations and requests

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
            "--yes" | "--force" | "--parity" | "--stress" | "--hardening" | "--prompt" | "--golden" | "--update" | "--paste" | "--plain" | "--kitty" | "--keys" | "--raw" | "--effects" | "--loops" | "--unread" | "--sent" | "--qr" => {
                args.flags.push(arg.trim_start_matches("--").to_owned())
            }
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
    if let Some(cli) = fake_gh::active() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        std::process::exit(fake_gh::run(cli, &args));
    }
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
    let gui_present = std::env::current_exe().map(|e| e.with_file_name(canvas_sys::host::exe_name("copilot-canvas-gui")).is_file()).unwrap_or(false);
    match setting.trim().to_ascii_lowercase().as_str() {
        "gui" => gui_present,
        "auto" => gui_present && !remote_session(),
        _ => false,
    }
}

/// A remote desktop (Cloud PC over RDP: no GPU), an SSH session, or (Linux)
/// no display (MAC-LINUX-PLAN §5.9; macOS Screen Sharing cannot be told).
fn remote_session() -> bool {
    canvas_sys::host::remote_session()
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
    // So does the paste probe.
    if args.command.first().map(String::as_str) == Some("paste-probe") {
        let flag = |name: &str| args.flags.iter().any(|f| f == name);
        let flags: Vec<&str> = ["plain", "kitty", "keys", "raw"].into_iter().filter(|f| flag(f)).collect();
        let option = |name: &str| args.rest.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        return match option("drive") {
            Some(text) => paste_probe::drive(text, &flags, option("program"), option("ready")),
            None => paste_probe::run(flag("plain"), flag("kitty"), flag("keys"), flag("raw")),
        };
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
        ["effect"] | ["effect", "list"] => cli::effect_list(),
        ["effect", kind] => cli::effect(&instance.home, kind, option("on"), option("note"), option("kind")),
        ["report", "publish"] => {
            let args = reports::PublishArgs {
                title: option("title"),
                markdown: option("markdown"),
                html: option("html"),
                message: option("message"),
                kind: option("kind"),
                links: options("link"),
                pr: option("pr"),
                session: option("session"),
                thread: option("thread"),
                source: option("source"),
            };
            let outcome = reports::publish(&instance.home, args, &reports::Identity::from_env(), std::time::Duration::from_secs(10))?;
            println!("{}", outcome.describe());
            Ok(())
        }
        ["report", "list"] => {
            let limit = option("limit").map(|v| v.parse::<usize>().context("--limit takes a number")).transpose()?.unwrap_or(50);
            println!("{}", reports::list(&instance.home, flag("unread"), limit)?);
            Ok(())
        }
        ["report", ..] => bail!("usage: copilot-canvas report publish (--markdown FILE | --html FILE | --message TEXT) [--title T] [options] · report list (see copilot-canvas --help)"),
        ["pr", "track", url] => prs_cli::track(&instance.home, url, option("session"), option("title")),
        ["pr", "untrack", url] => prs_cli::untrack(&instance.home, url),
        ["pr", "check"] => prs_cli::check(&instance.home, None),
        ["pr", "check", url] => prs_cli::check(&instance.home, Some(url)),
        ["pr", "dispatch", url, text @ ..] => prs_cli::dispatch(&instance.home, url, option("key"), option("event"), &text.join(" ")),
        ["pr", "list"] => {
            println!("{}", prs_cli::list(&instance.home, option("format").is_some_and(|f| f.eq_ignore_ascii_case("json")))?);
            Ok(())
        }
        ["pr", ..] => bail!("usage: copilot-canvas pr track URL [--session NAME|ID] [--title T] · pr untrack URL · pr check [URL] · pr dispatch URL --key KEY [--event TYPE] TEXT · pr list [--format json] (see copilot-canvas --help)"),
        ["review", "handoff"] => reviews_cli::handoff(
            &instance.home,
            reviews_cli::HandoffArgs {
                pr: option("pr"),
                author: option("author"),
                author_id: option("author-id"),
                results: option("results"),
                notes: option("notes"),
                outcome: option("outcome"),
                title: option("title"),
                links: options("link"),
                session: option("session"),
            },
        ),
        ["review", "message", review] => reviews_cli::message(
            &instance.home,
            review,
            reviews_cli::MessageArgs { draft: option("draft"), sent: flag("sent"), conversation: option("conversation"), recipient: option("recipient"), needs_you: option("needs-you"), failed: option("failed") },
        ),
        ["review", "checked"] => reviews_cli::checked(&instance.home, option("conversation"), option("mark"), option("failed")),
        ["review", "request"] => reviews_cli::request(&instance.home, option("conversation"), option("message-id"), option("kind"), option("text"), option("pr")),
        ["review", "done", request] => reviews_cli::done(&instance.home, request),
        ["review", "list"] => {
            println!("{}", reviews_cli::list(&instance.home, option("format").is_some_and(|f| f.eq_ignore_ascii_case("json")))?);
            Ok(())
        }
        ["review", ..] => bail!("usage: copilot-canvas review handoff --pr URL --author NAME --results FILE … · review message REVIEW (--draft FILE | --sent --conversation ID | --needs-you WHY | --failed WHY) · review checked · review request … · review done REQUEST · review list (see copilot-canvas --help)"),
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
        ["remote", "pair"] => remote_cli::pair(&instance, option("name"), flag("qr"), option("url")),
        ["remote", "clients"] => remote_cli::clients(&instance),
        ["remote", "push-key", file] => remote_cli::push_key(&instance, Path::new(file), option("key-id"), option("team")),
        ["remote", "push-test"] => remote_cli::push_test(&instance),
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
            if flag("golden") {
                anyhow::ensure!(client == selftest::Client::Gui, "--golden checks the GUI: add --client gui");
                return golden::run(flag("update"));
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
        ["selftest", "agent"] if flag("paste") => selftest::agent_paste(option("agent").as_deref()),
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
            } else if flag("effects") {
                gui_bench::Probe::Effects
            } else if flag("loops") {
                gui_bench::Probe::Loops
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
