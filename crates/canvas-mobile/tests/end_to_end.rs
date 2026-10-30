//! The phone's core against the mock service over a real WebSocket: pairing,
//! the handshake, the canvas, a real shell typed into at the phone's size,
//! renames, folder listings, reconnecting.
#![cfg(unix)]

use std::io::Read;
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use canvas_common::ws;
use canvas_mobile::{Client, ClientConfig, ConnState, CoreEvent, KeyCode, Listener, Pairing, StatusKind, Transport};
use crossbeam_channel::{Receiver, Sender, unbounded};

/// A WebSocket to the mock, as the app's `URLSessionWebSocketTask` would be.
struct WsTransport {
    writer: Mutex<ws::WsWriter>,
    stream: TcpStream,
}

impl Transport for WsTransport {
    fn send(&self, data: Vec<u8>) {
        use std::io::Write;
        let _ = self.writer.lock().unwrap().write_all(&data);
    }
    fn close(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// Connects and returns the transport and the stream of received messages.
fn websocket(addr: &str, received: impl Fn(Option<Vec<u8>>) + Send + 'static) -> Arc<WsTransport> {
    let stream = TcpStream::connect(addr).unwrap();
    let mut reader: Box<dyn Read + Send> = Box::new(stream.try_clone().unwrap());
    let mut writer: Box<dyn std::io::Write + Send> = Box::new(stream.try_clone().unwrap());
    ws::connect(&mut reader, &mut writer, addr, "/", &[("X-Tunnel-Authorization", "tunnel test")]).unwrap();
    let (mut r, w) = ws::split(ws::Role::Client, reader, writer);
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => {
                    received(None);
                    break;
                }
                Ok(n) => received(Some(buf[..n].to_vec())),
            }
        }
    });
    Arc::new(WsTransport { writer: Mutex::new(w), stream })
}

struct Events(Sender<CoreEvent>);

impl Listener for Events {
    fn on_event(&self, event: CoreEvent) {
        let _ = self.0.send(event);
    }
}

fn wait_for(events: &Receiver<CoreEvent>, what: &str, mut ok: impl FnMut(&CoreEvent) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(event) = events.recv_timeout(Duration::from_millis(100))
            && ok(&event)
        {
            return;
        }
    }
    panic!("timed out waiting for {what}");
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn screen_text(client: &Client, id: &str) -> String {
    let screen = client.take_screen(id.into(), true).unwrap();
    let mut rows = vec![String::new(); screen.rows as usize];
    for row in screen.changed {
        let mut line = String::new();
        for run in row.runs {
            while line.chars().count() < run.col as usize {
                line.push(' ');
            }
            for cell in run.cells {
                line.push_str(&cell);
            }
        }
        rows[row.y as usize] = line;
    }
    rows.join("\n")
}

fn attach(client: &Arc<Client>, addr: &str) {
    let feed = client.clone();
    let transport = websocket(addr, move |data| match data {
        Some(bytes) => feed.receive(bytes),
        None => feed.transport_closed("closed".into()),
    });
    client.attach(transport);
}

#[test]
fn the_phone_pairs_connects_and_works_a_real_shell_through_a_websocket() {
    let state = std::env::temp_dir().join(format!("canvas-mock-test-{}", std::process::id()));
    let cwd = std::env::temp_dir().display().to_string();
    let handle = canvas_mock::serve(canvas_mock::Config { state_dir: state, ..canvas_mock::Config::local("127.0.0.1:0", &cwd) }).unwrap();
    let addr = handle.addr.to_string();

    // Pairing: a wrong code is refused before the phone proves anything; the right one gives a secret.
    let pair = |code: &str| {
        let pairing: Arc<Mutex<Option<Arc<Pairing>>>> = Arc::default();
        let slot = pairing.clone();
        let transport = websocket(&addr, move |data| {
            if let Some(p) = slot.lock().unwrap().as_ref() {
                match data {
                    Some(bytes) => p.receive(bytes),
                    None => p.transport_closed(),
                }
            }
        });
        let p = Pairing::new(transport);
        *pairing.lock().unwrap() = Some(p.clone());
        p.run(code.into(), "test-phone".into())
    };
    assert!(pair("ZZZZ-ZZZZ-ZZZZ").is_err());
    let paired = pair("abcd-efgh-jkmn").unwrap();
    assert!(paired.client_id.starts_with("pc-") && paired.secret.len() == 32);

    let (tx, events) = unbounded();
    let config = ClientConfig { client_name: "iphone-test".into(), remote_id: paired.client_id.clone(), secret: paired.secret.clone(), build: "test".into() };
    let client = Client::new(config, Arc::new(Events(tx)));
    attach(&client, &addr);
    wait_for(&events, "online", |e| matches!(e, CoreEvent::Connection { state: ConnState::Online, .. }));
    wait_until("the canvas", || client.overview().sessions.len() == 7);
    let overview = client.overview();
    assert_eq!(overview.groups.len(), 2);
    let fix = overview.sessions.iter().find(|s| s.id == "mock-fix").unwrap();
    assert_eq!(fix.subagents.len(), 1, "subagents from the session's history");
    assert_eq!(overview.sessions.iter().find(|s| s.id == "mock-docs").unwrap().loop_label, "every 15m");
    wait_until("the service's agents", || !client.overview().agents.is_empty());

    // Open a session: it starts a shell at the phone's size, typing works.
    client.open_session("mock-loose".into(), 50, 16).unwrap();
    client.open_terminal("mock-loose".into(), true, 50, 16);
    wait_until("the shell", || client.overview().sessions.iter().any(|s| s.id == "mock-loose" && s.alive));
    wait_until("a screen", || client.take_screen("mock-loose".into(), false).is_some_and(|s| s.cols == 50 && s.rows == 16));
    client.send_text("mock-loose".into(), "echo hello-from-$((40+2))\n".into());
    wait_until("the echo", || screen_text(&client, "mock-loose").contains("hello-from-42"));
    // Ctrl+C reaches the shell as a key (the service encodes it).
    client.send_key("mock-loose".into(), KeyCode::Char { text: "c".into() }, canvas_mobile::MOD_CTRL);
    // Rotating the phone resizes the terminal.
    client.resize_terminal("mock-loose".into(), 90, 12);
    wait_until("the new size", || client.take_screen("mock-loose".into(), false).is_some_and(|s| s.cols == 90 && s.rows == 12));
    // A prompt from the composer: pasted, then Enter.
    client.submit("mock-loose".into(), "echo composed".into());
    wait_until("the composed prompt", || screen_text(&client, "mock-loose").contains("composed"));

    // Renames go to the shared canvas.
    client.rename_session("mock-fix".into(), "Fix the flaky test".into()).unwrap();
    wait_until("the rename", || client.overview().sessions.iter().any(|s| s.name == "Fix the flaky test"));
    // The service's folders.
    let listing = client.list_folder(Some(cwd.clone())).unwrap();
    assert_eq!(listing.entries.first().map(String::as_str), Some(".."));

    // The connection drops: offline, then a new WebSocket resyncs.
    client.detach();
    wait_for(&events, "offline", |e| matches!(e, CoreEvent::Connection { state: ConnState::Offline, .. }));
    client.send_text("mock-loose".into(), "x".into());
    wait_for(&events, "input blocked offline", |e| matches!(e, CoreEvent::Notice { text, .. } if text.contains("Offline")));
    attach(&client, &addr);
    wait_for(&events, "online again", |e| matches!(e, CoreEvent::Connection { state: ConnState::Online, .. }));
    wait_until("the shell after reconnecting", || screen_text(&client, "mock-loose").contains("composed"));

    // Enter in an agent's terminal makes it work, then finish.
    client.open_session("mock-review".into(), 50, 16).unwrap();
    client.open_terminal("mock-review".into(), true, 50, 16);
    wait_until("the agent's shell", || client.overview().sessions.iter().any(|s| s.id == "mock-review" && s.alive));
    client.send_key("mock-review".into(), KeyCode::Enter, 0);
    wait_until("working", || client.overview().sessions.iter().any(|s| s.id == "mock-review" && s.status == StatusKind::Working));
    // Watching it, the finished turn is reviewed at once (idle, not done · review).
    wait_until("idle", || client.overview().sessions.iter().any(|s| s.id == "mock-review" && s.status == StatusKind::Idle));

    // Stopping ends the terminal.
    client.stop_session("mock-loose".into());
    wait_until("stopped", || client.overview().sessions.iter().any(|s| s.id == "mock-loose" && !s.alive));
}

/// Pairs a phone with the mock at `addr` (the test code).
fn pair_phone(addr: &str) -> canvas_mobile::PairResult {
    let pairing: Arc<Mutex<Option<Arc<Pairing>>>> = Arc::default();
    let slot = pairing.clone();
    let transport = websocket(addr, move |data| {
        if let Some(p) = slot.lock().unwrap().as_ref() {
            match data {
                Some(bytes) => p.receive(bytes),
                None => p.transport_closed(),
            }
        }
    });
    let p = Pairing::new(transport);
    *pairing.lock().unwrap() = Some(p.clone());
    p.run("ABCD-EFGH-JKMN".into(), "test-phone".into()).unwrap()
}

/// A paired phone, connected.
fn phone(addr: &str) -> (Arc<Client>, Receiver<CoreEvent>) {
    let paired = pair_phone(addr);
    let (tx, events) = unbounded();
    let config = ClientConfig { client_name: "iphone-test".into(), remote_id: paired.client_id, secret: paired.secret, build: "test".into() };
    let client = Client::new(config, Arc::new(Events(tx)));
    attach(&client, addr);
    wait_for(&events, "online", |e| matches!(e, CoreEvent::Connection { state: ConnState::Online, .. }));
    (client, events)
}

#[test]
fn the_reports_inbox_reaches_phones_and_reports_open_as_safe_pages() {
    let state = std::env::temp_dir().join(format!("canvas-mock-reports-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    let cwd = std::env::temp_dir().display().to_string();
    let handle = canvas_mock::serve(canvas_mock::Config { state_dir: state.clone(), ..canvas_mock::Config::local("127.0.0.1:0", &cwd) }).unwrap();
    let addr = handle.addr.to_string();
    let (client, events) = phone(&addr);

    // The inbox comes with the connection: one row per thread, the newest first
    // (named by the canvas's boxes once the canvas is in too).
    wait_until("the canvas", || client.overview().sessions.len() == 7);
    wait_until("the inbox", || client.reports().loaded && client.reports().threads.len() == 3);
    let inbox = client.reports();
    assert_eq!(inbox.threads[0].id, "mock-brief");
    let pr = inbox.threads.iter().find(|t| t.latest.thread == "pr:42").unwrap();
    assert_eq!((pr.items.len(), pr.unread), (2, 1), "two messages, one unread");
    let brief = &inbox.threads[0].latest;
    assert_eq!((brief.source_name.as_str(), brief.source_box.as_str(), brief.format), ("Drift guard", "mock-drift", canvas_mobile::ReportFormat::Html));
    assert_eq!(inbox.unread, 3);

    // An HTML report: the service's raw page, sanitized for the web view.
    let page = client.read_report("mock-brief".into()).unwrap();
    assert!(page.html.contains("Content-Security-Policy") && page.html.contains("Payments health") && page.html.contains("<svg"), "{}", page.html);
    assert!(!page.html.contains("<script") && !page.html.contains("SCRIPT RAN") && !page.html.contains("127.0.0.1:9"), "{}", page.html);
    let markdown = client.read_report("mock-root-cause".into()).unwrap();
    assert!(markdown.html.contains("<h1>Flaky payment test: root cause</h1>") && markdown.html.contains("<table>"));
    assert!(client.read_report("nope".into()).is_err());

    // Read here: at once, and on every other phone once the service has it.
    client.mark_reports(vec!["mock-brief".into()], true).unwrap();
    assert_eq!(client.reports().unread, 2);
    let (other, _other_events) = phone(&addr);
    wait_until("the read state on another phone", || other.reports().threads.iter().any(|t| t.id == "mock-brief" && t.unread == 0));

    // A report published now: announced while the app is open.
    std::fs::write(state.join("publish").join("deploy-finished.md"), "# Deploy finished\n\nAll green.").unwrap();
    let mut arrived = String::new();
    wait_for(&events, "a new report", |e| match e {
        CoreEvent::ReportArrived { id, title, source, .. } if title == "deploy finished" && source == "Mac stand-in" => {
            arrived = id.clone();
            true
        }
        _ => false,
    });
    assert_eq!(client.reports().threads[0].id, arrived);
    // Deleted from one phone: gone on the other.
    client.delete_reports(vec![arrived.clone()]).unwrap();
    wait_until("the delete on another phone", || !other.reports().threads.iter().any(|t| t.id == arrived));
    client.mark_all_reports_read().unwrap();
    wait_until("everything read", || other.reports().unread == 0);
    let _ = std::fs::remove_dir_all(state);
}
