//! The parity suite's checks of the Reports inbox (rows 41–41c): a session
//! publishes a Markdown report and a message through `copilot-canvas report
//! publish` (its environment, as an agent would); the activity bar counts
//! them, a toast is recorded; the inbox lists them and the viewer shows them
//! formatted and marks them read at the service; viewing the source session
//! starts nothing (row 41). An HTML report shows in the GUI's WebView2 view
//! with its own styles while scripts never run, nothing is fetched (a local
//! listener sees no connection) and navigation is blocked, with and without
//! the canvas's sanitizer (row 41b). A client over `--remote loopback` lists
//! and opens the reports through the service (row 41c).

use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{Gui, Kind, Row, Sandbox, check, check_kind, png_rgb};
use crate::selftest::Ui;

/// Publishes through the CLI as the canvas session `session` would (its
/// environment); answers the report's id.
pub(crate) fn publish(exe: &Path, home: &Path, session: &str, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(exe)
        .args(["report", "publish"])
        .args(args)
        .args(["--home", &home.display().to_string()])
        .env("COPILOT_CANVAS_SESSION", session)
        .env_remove("AUTOMATION_NAME")
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    anyhow::ensure!(output.status.success(), "report publish failed: {}{}", stdout, String::from_utf8_lossy(&output.stderr));
    anyhow::ensure!(stdout.contains("Report stored"), "not stored at once: {stdout}");
    report_id(&stdout).with_context(|| format!("no report id in {stdout:?}"))
}

/// The id in "… (0001792…-abcd1234)."
pub(crate) fn report_id(text: &str) -> Option<String> {
    let start = text.rfind('(')? + 1;
    let end = start + text[start..].find(')')?;
    Some(text[start..end].to_owned())
}

/// When the service says a report is read (its `state.json`).
pub(crate) fn read_at_service(home: &Path, id: &str) -> bool {
    std::fs::read(home.join("reports").join("state.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).is_some_and(|state| state["read"][id].as_u64().is_some_and(|ms| ms > 0))
}

fn wait_for(what: &str, secs: u64, ok: impl Fn() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        if Instant::now() > deadline {
            bail!("timed out waiting for {what}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn launches(sb: &Sandbox) -> usize {
    std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default().matches("launched ").count()
}

/// The view's state (`webview` hook) once `ok`.
fn web_state(gui: &Gui, what: &str, secs: u64, ok: impl Fn(&Value) -> bool) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let state = gui.req(json!({ "cmd": "webview" }))?;
        if ok(&state) {
            return Ok(state);
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {what}: {state}");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn eval(gui: &Gui, script: &str) -> Result<Value> {
    let answer = gui.req(json!({ "cmd": "webview", "eval": script }))?;
    anyhow::ensure!(answer["ok"] == true, "eval failed: {answer}");
    Ok(answer["result"].clone())
}

/// Opens a report from the inbox by a click on its row.
fn open_from_inbox(gui: &mut Gui, title: &str) -> Result<()> {
    gui.canvas_focus()?;
    gui.chord('9', &["alt"])?;
    gui.wait("the inbox", 10, |t| t.contains("REPORTS") && t.contains(title))?;
    let at = gui.find(title).with_context(|| format!("{title:?} in the inbox"))?;
    gui.click(at, &[])?;
    gui.wait("the viewer", 10, |t| t.contains(&format!("· {title}")) && t.contains("(read-only)"))
}

pub(super) fn rows(sb: &Sandbox, gui: &mut Gui, rows: &mut Vec<Row>) {
    check(rows, "41", "New: Reports inbox: sessions publish reports and messages (copilot-canvas report publish); an unread count on the activity bar, a notice and a toast; the viewer formats Markdown and marks it read at the service; viewing its session starts nothing", "reports_inbox_and_viewer", || {
        gui.canvas_focus()?;
        let before = launches(sb);
        let brief = sb.root.join("parity-brief.md");
        std::fs::write(&brief, "# Parity brief\n\n## Needs you\n\n- **PR 41** review requested: [open](https://github.com/org/repo/pull/41)\n- build `main` green\n\n| Area | State |\n|---|---|\n| Rollout | ring 2 |\n\n> nothing else\n")?;
        let session = sb.sessions.stopped.clone();
        let brief_id = publish(&sb.exe, &sb.instance.home, &session, &["--markdown", &brief.display().to_string(), "--kind", "warn", "--pr", "https://github.com/org/repo/pull/41"])?;
        let message_id = publish(&sb.exe, &sb.instance.home, &session, &["--message", "PR 41 received 12 new comments; dispatched the owning session", "--kind", "queued", "--thread", "pr-41"])?;
        gui.wait("two unread on the activity bar", 10, |t| t.contains("✉2"))?;
        gui.wait("the notice", 10, |t| t.contains("New report from Stopped one"))?;
        let platform = gui.poll_platform("a toast for each new report", 10, &|p| {
            let toasts = p["toasts"].as_array().cloned().unwrap_or_default();
            ["New report: Parity brief", "New report: PR 41 received"].iter().all(|t| toasts.iter().any(|r| r["title"].as_str().is_some_and(|s| s.starts_with(t))))
        })?;
        let statuses: Vec<String> = platform["toasts"].as_array().into_iter().flatten().filter(|t| t["title"].as_str().is_some_and(|s| s.starts_with("New report"))).map(|t| t["status"].as_str().unwrap_or_default().to_owned()).collect();
        open_from_inbox(gui, "Parity brief")?;
        gui.wait("the brief formatted", 10, |t| t.contains("Needs you") && t.contains("• PR 41 review requested") && t.contains("Area    │ State") && t.contains("▎ nothing else"))?;
        gui.wait("one unread left", 10, |t| t.contains("✉1"))?;
        wait_for("the brief read at the service", 10, || read_at_service(&sb.instance.home, &brief_id))?;
        anyhow::ensure!(!read_at_service(&sb.instance.home, &message_id), "the message is still unread");
        // Viewing the session it came from only views it.
        let at = gui.find("View “Stopped one”").context("the View button")?;
        gui.click(at, &[])?;
        gui.wait_tree("the session's details, the viewer closed", 10, |t| t["modals"].as_u64() == Some(0))?;
        gui.wait("its details panel", 10, |t| t.contains("Stopped one") && t.contains("▸ Resume"))?;
        // The message: one click on its row reads it.
        open_from_inbox(gui, "PR 41 received")?;
        gui.wait("the message", 10, |t| t.contains("dispatched the owning session") && t.contains("queued"))?;
        wait_for("the message read at the service", 10, || read_at_service(&sb.instance.home, &message_id))?;
        gui.wait("nothing unread", 10, |t| !t.contains("✉1") && !t.contains("✉2"))?;
        gui.key("Esc", &[])?;
        anyhow::ensure!(launches(sb) == before, "reading reports launched something");
        Ok(format!("2 published as Stopped one, ✉2 → ✉1 → none, toasts {statuses:?}, read at the service, nothing launched"))
    });

    check_kind(
        rows,
        "41b",
        "New: HTML reports in the GUI's WebView2 view: their own styles; scripts never run, nothing is fetched, navigation blocked (sanitized and raw)",
        "html_report_locked_down",
        Kind::Only("GUI only: the TUI shows HTML as formatted text (snapshot report-viewer-html)"),
        || html_check(sb, gui),
    );
}

fn html_check(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    // Anything the page tried to fetch would reach this listener.
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let page = sb.root.join("parity-triage.html");
    std::fs::write(
        &page,
        format!(
            r#"<html><head><title>Parity triage</title><style>h1{{color:#ff00ff;font-size:44px}} .bg{{background:url(http://127.0.0.1:{port}/css.png)}}</style><link rel="stylesheet" href="http://127.0.0.1:{port}/x.css"></head>
<body><h1 id="t">Parity triage</h1><p class="bg">Four <b>new</b> bugs.</p>
<script>document.title='SCRIPT-RAN';document.getElementById('t').textContent='SCRIPT-RAN';fetch('http://127.0.0.1:{port}/fetch')</script>
<img src="http://127.0.0.1:{port}/img.png" onerror="document.title='ONERROR-RAN'"><a id="l" href="https://example.com/">a link</a></body></html>"#
        ),
    )?;
    let id = publish(&sb.exe, &sb.instance.home, &sb.sessions.stopped, &["--html", &page.display().to_string()])?;
    open_from_inbox(gui, "Parity triage")?;
    let state = web_state(gui, "the HTML view showing the report", 30, |s| s["report"] == id.as_str() && s["visible"] == true && s["loads"].as_u64().unwrap_or(0) >= 1)?;
    if state["phase"].as_str().is_some_and(|p| p.starts_with("failed")) {
        bail!("WebView2 failed: {state}");
    }
    let tree = gui.tree()?;
    let pane = tree["html"].clone();
    anyhow::ensure!(pane["id"] == id.as_str(), "the view model carries the page: {pane}");
    gui.wait("the viewer says how HTML is shown", 5, |t| t.contains("HTML view: scripts off, nothing loaded from the network"))?;
    // The sanitized page: no script elements, no network image, its own styling.
    let seen = eval(gui, "JSON.stringify({title: document.title, h1: document.getElementById('t').textContent, scripts: document.scripts.length, images: Array.from(document.images).map(i => i.getAttribute('src')), color: getComputedStyle(document.getElementById('t')).color, csp: !!document.querySelector('meta[http-equiv=\"Content-Security-Policy\"]')})")?;
    let seen: Value = serde_json::from_str(seen.as_str().unwrap_or("{}"))?;
    anyhow::ensure!(seen["title"] != "SCRIPT-RAN" && seen["title"] != "ONERROR-RAN" && seen["h1"] == "Parity triage", "a script ran: {seen}");
    anyhow::ensure!(seen["scripts"] == 0 && seen["images"] == json!([null]) && seen["csp"] == true, "the page was not sanitized: {seen}");
    anyhow::ensure!(seen["color"] == "rgb(255, 0, 255)", "its own style: {seen}");
    // What the view shows, as pixels: the magenta heading.
    let shot = std::env::var_os("CANVAS_GUI_SCREENS").map(PathBuf::from).unwrap_or_else(|| sb.root.clone()).join("report-html-view.png");
    let capture = gui.req(json!({ "cmd": "webview", "capture": shot }))?;
    anyhow::ensure!(capture["ok"] == true, "capture: {capture}");
    let (_, pixels) = png_rgb(&shot)?;
    let magenta = pixels.iter().filter(|p| p[0] > 200 && p[1] < 60 && p[2] > 200).count();
    anyhow::ensure!(magenta > 100, "no magenta heading in the capture ({magenta} pixels)");
    // The window as the desktop composes it: the view sits over the viewer's content area.
    let composed = window_check(gui, sb, &pane)?;
    // The view's own lock-down, without the sanitizer: a raw page with a script and fetches.
    let loads = web_state(gui, "the state", 5, |_| true)?["loads"].as_u64().unwrap_or(0);
    let raw = format!(r#"<html><body><h1 id="r">RAW</h1><script>document.title='RAW-SCRIPT-RAN'</script><img src="http://127.0.0.1:{port}/raw.png"><img src="http://127.0.0.1:{port}/raw2.png" onerror="document.title='RAW-ONERROR'"></body></html>"#);
    let probe = gui.req(json!({ "cmd": "webview", "probe_raw": raw }))?;
    anyhow::ensure!(probe["ok"] == true, "raw probe: {probe}");
    web_state(gui, "the raw page", 15, |s| s["loads"].as_u64().unwrap_or(0) > loads)?;
    let raw_seen = eval(gui, "JSON.stringify({title: document.title, h1: document.getElementById('r').textContent, scripts: document.scripts.length})")?;
    let raw_seen: Value = serde_json::from_str(raw_seen.as_str().unwrap_or("{}"))?;
    anyhow::ensure!(raw_seen["scripts"] == 1 && raw_seen["h1"] == "RAW", "the raw page: {raw_seen}");
    anyhow::ensure!(raw_seen["title"] != "RAW-SCRIPT-RAN" && raw_seen["title"] != "RAW-ONERROR", "a script ran with scripts off: {raw_seen}");
    let state = web_state(gui, "the blocked requests", 10, |s| s["blocked_requests"].as_u64().unwrap_or(0) >= 1)?;
    // A navigation (as a script would start it) is cancelled: the page stays.
    let _ = eval(gui, "location.href = 'https://example.com/navigated'; 'asked'")?;
    web_state(gui, "the blocked navigation", 10, |s| s["blocked_navigations"].as_array().is_some_and(|n| n.iter().any(|u| u.as_str().is_some_and(|u| u.contains("example.com/navigated")))))?;
    let still = eval(gui, "document.getElementById('r') ? 'stayed' : 'left'")?;
    anyhow::ensure!(still == "stayed", "the page navigated away: {still}");
    std::thread::sleep(Duration::from_millis(1500));
    match listener.accept() {
        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
        Ok((_, from)) => bail!("the HTML view reached the network ({from})"),
        Err(e) => bail!("listener: {e}"),
    }
    // Esc closes the report; the view hides.
    gui.canvas_focus()?;
    web_state(gui, "the view hidden", 10, |s| s["visible"] == false)?;
    Ok(format!(
        "WebView2 {}: styles applied ({magenta} magenta px), {composed}, 0 scripts kept, raw script not run, {} request(s) answered 403 and 0 reached the listener, navigation blocked",
        state["phase"].as_str().unwrap_or_default(),
        state["blocked_requests"]
    ))
}

/// The composed window (`PrintWindow`): magenta from the view inside the
/// viewer's content area and none outside it. A blank capture (the desktop
/// composes nothing, e.g. while the workstation is locked) is reported, not failed.
fn window_check(gui: &Gui, sb: &Sandbox, pane: &Value) -> Result<String> {
    let path = std::env::var_os("CANVAS_GUI_SCREENS").map(PathBuf::from).unwrap_or_else(|| sb.root.clone()).join("report-html-window.png");
    let shot = gui.req(json!({ "cmd": "webview", "window_capture": path }))?;
    anyhow::ensure!(shot["ok"] == true, "window capture: {shot}");
    let (width, pixels) = png_rgb(&path)?;
    if pixels.iter().all(|p| p[0] < 8 && p[1] < 8 && p[2] < 8) {
        return Ok("window capture blank (nothing composed: workstation locked?)".into());
    }
    let (cw, ch) = (shot["cell_w"].as_f64().unwrap_or(10.0), shot["cell_h"].as_f64().unwrap_or(20.0));
    let rect: Vec<f64> = pane["rect"].as_array().map(|r| r.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
    anyhow::ensure!(rect.len() == 4, "the pane's rect: {pane}");
    let (x0, y0, x1, y1) = ((rect[0] * cw) as usize, (rect[1] * ch) as usize, ((rect[0] + rect[2]) * cw) as usize, ((rect[1] + rect[3]) * ch) as usize);
    let (mut inside, mut outside) = (0, 0);
    for (i, p) in pixels.iter().enumerate() {
        if p[0] > 200 && p[1] < 60 && p[2] > 200 {
            let (x, y) = (i % width, i / width);
            if x >= x0 && x < x1 && y >= y0 && y < y1 { inside += 1 } else { outside += 1 }
        }
    }
    anyhow::ensure!(inside > 100 && outside == 0, "the view is not where the viewer's content is: {inside} magenta px inside {x0},{y0}..{x1},{y1}, {outside} outside");
    Ok(format!("composed over the content area ({inside} px)"))
}

pub(super) fn late_rows(sb: &Sandbox, rows: &mut Vec<Row>) {
    check(rows, "41c", "New: remote mode lists and opens reports through the service (--remote loopback)", "remote_reports_inbox", || {
        let id = publish(&sb.exe, &sb.instance.home, &sb.sessions.stopped, &["--message", "Remote parity message: 3 PRs merged", "--kind", "delivered"])?;
        let mut gui = sb.start(&["--remote", "loopback"], "gui-remote-reports.log")?;
        gui.wait("canvas over TCP", 25, |t| t.contains("Parity group"))?;
        gui.wait("its unread count", 10, |t| t.contains("✉1"))?;
        open_from_inbox(&mut gui, "Remote parity message")?;
        gui.wait("the message through the service", 10, |t| t.contains("3 PRs merged") && t.contains("delivered"))?;
        wait_for("read at the service", 10, || read_at_service(&sb.instance.home, &id))?;
        let log = std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default();
        anyhow::ensure!(log.contains("connected (gui, protocol 8"), "a remote GUI");
        let _ = gui.req(json!({ "cmd": "quit" }));
        let _ = gui.driver.wait_exit(Duration::from_secs(10));
        Ok("listed (✉1) and opened over loopback TCP; read at the service".into())
    });
}
