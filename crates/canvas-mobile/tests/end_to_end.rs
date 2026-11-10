//! The phone's core against the mock service over a real WebSocket: pairing,
//! the handshake, the canvas, a real shell typed into at the phone's size,
//! renames, folder listings, reconnecting, the reports inbox, the todo
//! boards and the notebook.
#![cfg(unix)]

use std::io::Read;
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use canvas_common::ws;
use canvas_mobile::{Client, ClientConfig, ConnState, CoreError, CoreEvent, KeyCode, Listener, NoteFilter, NoteKind, NoteLink, Pairing, StatusKind, TodoDraft, TodoKind, TodoPriority, TodoStatus, Transport};
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

#[test]
fn todo_boards_and_the_notebook_reach_phones_and_changes_reach_every_phone() {
    let state = std::env::temp_dir().join(format!("canvas-mock-notes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    let cwd = std::env::temp_dir().display().to_string();
    let handle = canvas_mock::serve(canvas_mock::Config { state_dir: state.clone(), ..canvas_mock::Config::local("127.0.0.1:0", &cwd) }).unwrap();
    let addr = handle.addr.to_string();
    let (client, _events) = phone(&addr);
    let (other, _other_events) = phone(&addr);

    // The boards come with the bus snapshot, on the mock's canvas.
    wait_until("the canvas", || client.overview().sessions.len() == 7);
    wait_until("the todo boards", || client.todos().loaded);
    let todos = client.todos();
    assert_eq!(todos.boards.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), ["Canvas App", "Decomps"]);
    let momentum = todos.todos.iter().find(|t| t.id == "WI-002").unwrap();
    assert_eq!((momentum.claimed_by.as_str(), momentum.claimed_session.as_str()), ("Fix flaky test", "mock-fix"));
    assert!(todos.groups.iter().any(|g| g.group == "g-payments" && g.proposed == 1));
    // Accepted here at once, on the other phone once the service has it.
    let change = client.accept_todo("WI-006".into(), Some(TodoStatus::Todo)).unwrap();
    assert!(change.id == "WI-006" && change.message.starts_with("Accepted WI-006"), "{change:?}");
    assert_eq!(client.todos().todos.iter().find(|t| t.id == "WI-006").map(|t| t.status), Some(TodoStatus::Todo));
    wait_until("the accepted todo on the other phone", || other.todos().todos.iter().any(|t| t.id == "WI-006" && t.status == TodoStatus::Todo));
    assert!(client.accept_todo("WI-006".into(), None).is_err(), "not a proposal any more: refused here");
    let draft = TodoDraft { board: "Decomps".into(), title: "Fuzz the parser".into(), kind: TodoKind::Research, priority: TodoPriority::Low, status: None, body: String::new(), blocker: false };
    assert_eq!(client.add_todo(draft).unwrap().id, "DEC-009");
    client.done_todo("WI-004".into()).unwrap();
    let both = |phone: &Client| phone.todo("DEC-009".into()).is_some() && phone.todo("WI-004".into()).is_some_and(|t| t.item.status == TodoStatus::Done);
    assert!(both(&client) && client.todo("WI-006".into()).is_some_and(|t| t.item.status == TodoStatus::Todo), "here at once, the accepted todo still accepted");
    wait_until("the new and finished todos on the other phone", || both(&other));

    // The notebook: its root, folders with provenance, notes as safe pages.
    let notebook = client.notebook().unwrap();
    assert_eq!(notebook.roots.len(), 1, "the mock's groups have no folders of their own");
    let root = std::path::Path::new(&notebook.root);
    let path = |parts: &[&str]| parts.iter().fold(root.to_path_buf(), |p, part| p.join(part)).display().to_string();
    let top = client.list_notes(notebook.root.clone(), NoteFilter::All, false).unwrap();
    assert!(top.entries.iter().any(|e| e.name == "Welcome.md" && e.title == "Welcome to the notebook"), "{top:?}");
    let outputs = client.list_notes(path(&["Groups", "Payments", "human-outputs"]), NoteFilter::Human, false).unwrap();
    assert_eq!(outputs.entries.iter().map(|e| e.kind).collect::<Vec<_>>(), [NoteKind::Html, NoteKind::Markdown]);
    let docs = client.list_notes(path(&["Groups", "Payments", "ai-docs"]), NoteFilter::Human, false).unwrap();
    assert_eq!((docs.entries.len(), docs.hidden), (0, 2), "notes for agents left out");
    let plan_path = path(&["Groups", "Payments", "human-outputs", "Release plan.md"]);
    let plan = client.read_note(plan_path.clone()).unwrap();
    assert!(plan.html.contains("<h1>Release plan</h1>") && plan.html.contains("src=\"data:image/svg+xml;base64,"), "{}", plan.html);
    assert_eq!((plan.provenance.session_name.as_str(), plan.provenance.reviewed), ("Fix flaky test", false));
    let dashboard = client.read_note(path(&["Groups", "Payments", "human-outputs", "Payments dashboard.html"])).unwrap();
    assert!(dashboard.html.contains("Payments dashboard") && dashboard.html.contains("src=\"data:image/svg+xml;base64,"), "{}", dashboard.html);
    assert!(!dashboard.html.contains("<script") && !dashboard.html.contains("SCRIPT RAN") && !dashboard.html.contains("127.0.0.1:9"), "{}", dashboard.html);
    let chart = client.read_note(path(&["pictures", "chart.svg"])).unwrap();
    assert!(chart.kind == NoteKind::Image && chart.html.contains("<img src=\"data:image/svg+xml;base64,"));
    assert_eq!(canvas_mobile::note_link_target("#note:Release%20plan".into()), Some(NoteLink::Note { name: "Release plan".into() }));
    assert_eq!(client.resolve_note("Release plan".into()).unwrap(), Some(plan_path.clone()));
    assert!(client.search_notes("refunds never".into(), 20).unwrap().iter().any(|h| h.path.ends_with("GOALS.md") && h.line > 0));

    // Mark reviewed: written based on the note's revision, seen by the next read.
    client.mark_note_reviewed(plan_path.clone()).unwrap();
    let marked = other.read_note(plan_path.clone()).unwrap();
    assert!(marked.provenance.reviewed && marked.rev != plan.rev && marked.text.contains("reviewed: true"));
    assert_eq!(client.note_revisions(vec![plan_path, path(&["missing.md"])]).unwrap(), [Some(marked.rev), None]);
    assert!(client.mark_note_reviewed(path(&["pictures", "chart.svg"])).is_err(), "pictures carry no mark");
    let _ = std::fs::remove_dir_all(state);
}

#[test]
fn phones_edit_notes_and_todos_and_a_note_changed_elsewhere_is_never_overwritten_unasked() {
    let state = std::env::temp_dir().join(format!("canvas-mock-edits-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    let cwd = std::env::temp_dir().display().to_string();
    let handle = canvas_mock::serve(canvas_mock::Config { state_dir: state.clone(), ..canvas_mock::Config::local("127.0.0.1:0", &cwd) }).unwrap();
    let addr = handle.addr.to_string();
    let (client, _events) = phone(&addr);
    let (other, _other_events) = phone(&addr);
    wait_until("the canvas", || client.overview().sessions.len() == 7 && other.overview().sessions.len() == 7);

    // A note's source, saved on its revision; the other phone reads the change.
    let notebook = client.notebook().unwrap();
    let root = std::path::Path::new(&notebook.root);
    let path = |parts: &[&str]| parts.iter().fold(root.to_path_buf(), |p, part| p.join(part)).display().to_string();
    let idea = path(&["Inbox", "Idea - offline mode.md"]);
    let source = client.note_source(idea.clone()).unwrap();
    assert!(source.editable && source.why_not.is_empty() && source.kind == NoteKind::Markdown && source.text.starts_with("# Idea"), "{source:?}");
    let mine = format!("{}\nWritten on the phone.\n", source.text);
    let saved = client.save_note(idea.clone(), mine.clone(), source.rev.clone()).unwrap();
    assert_eq!(saved.text, mine, "your note stays yours");
    let page = other.read_note(idea.clone()).unwrap();
    assert!(page.text.contains("Written on the phone.") && page.rev == saved.rev && page.html.contains("Written on the phone."));

    // The other phone, still on the old revision: a conflict with the text now, nothing written.
    let theirs = format!("{}\nFrom the other phone.\n", source.text);
    match other.save_note(idea.clone(), theirs.clone(), source.rev.clone()) {
        Err(CoreError::Conflict { text, rev, .. }) => {
            assert!(text.contains("Written on the phone.") && rev == saved.rev);
            // Keep mine: saved on the revision it conflicted with, after asking.
            other.save_note(idea.clone(), theirs.clone(), rev).unwrap();
        }
        other => panic!("a conflict: {other:?}"),
    }
    assert_eq!(client.note_source(idea.clone()).unwrap().text, theirs);
    // An agent's note becomes mixed with your change.
    let log = path(&["Groups", "Payments", "ai-docs", "investigation-log.md"]);
    let source = client.note_source(log.clone()).unwrap();
    let saved = client.save_note(log.clone(), format!("{}- 10:00 Checked from the phone.\n", source.text), source.rev).unwrap();
    assert!(saved.text.starts_with("---\nauthor: mixed\n---\n") && client.read_note(log).unwrap().provenance.author == canvas_mobile::NoteAuthor::Mixed, "{}", saved.text);
    let chart = client.note_source(path(&["pictures", "chart.svg"])).unwrap();
    assert!(!chart.editable && chart.why_not.contains("Pictures"));
    assert!(client.save_note(path(&["pictures", "chart.svg"]), "<svg/>".into(), chart.rev).is_err());

    // New notes and folders, renames and the trash.
    let inbox = path(&["Inbox"]);
    let made = client.create_note(inbox.clone(), "Phone note".into(), String::new()).unwrap();
    assert_eq!(made, path(&["Inbox", "Phone note.md"]));
    let made_page = client.read_note(made.clone()).unwrap();
    assert!(made_page.title == "Phone note" && made_page.provenance.audience == canvas_mobile::NoteAudience::Human && made_page.provenance.author == canvas_mobile::NoteAuthor::Human && !made_page.provenance.inferred);
    assert!(made_page.text.contains("created: "), "{}", made_page.text);
    assert!(client.create_note(inbox.clone(), "Phone note.md".into(), String::new()).unwrap_err().to_string().contains("exists already"), "never over another note");
    let folder = client.create_folder(inbox.clone(), "Trips".into()).unwrap();
    assert!(std::path::Path::new(&folder).is_dir());
    let renamed = client.rename_note(made.clone(), "Trip ideas".into()).unwrap();
    assert_eq!(renamed, path(&["Inbox", "Trip ideas.md"]));
    let listed = |dir: &str| other.list_notes(dir.into(), NoteFilter::All, false).unwrap().entries.into_iter().map(|e| e.name).collect::<Vec<_>>();
    assert!(listed(&inbox).contains(&"Trip ideas.md".to_owned()) && !listed(&inbox).contains(&"Phone note.md".to_owned()));
    client.delete_note(renamed.clone()).unwrap();
    client.delete_note(folder).unwrap();
    assert!(!listed(&inbox).iter().any(|n| n == "Trip ideas.md" || n == "Trips"), "in the trash");
    assert!(std::fs::read_dir(path(&[".trash"])).unwrap().count() == 2);
    assert!(client.delete_note(notebook.root.clone()).is_err() && client.rename_note(notebook.root.clone(), "x".into()).is_err(), "the root stays");

    // Todos: fields, the checklist, cancel and reopen, release. The mock sends
    // its book after each change, so books from before the later changes
    // arrive while they are on their way: each change shows here at once and
    // stays (the next one may name a checklist item by its place), and the
    // other phone follows.
    wait_until("the todo boards", || client.todos().loaded && other.todos().loaded);
    let edit = canvas_mobile::TodoEdit { title: Some("Glide after a flick".into()), priority: Some(TodoPriority::Urgent), labels: Some(vec!["ui".into()]), blocker: Some(true), ..Default::default() };
    assert!(client.update_todo("WI-002".into(), edit).unwrap().message.starts_with("Updated WI-002"));
    client.add_check("WI-002".into(), "Try it on the phone".into()).unwrap();
    client.edit_check("WI-002".into(), 0, "Glide on release".into()).unwrap();
    client.remove_check("WI-002".into(), 1).unwrap();
    client.check_todo("WI-002".into(), 3, true).unwrap();
    let edited = |phone: &Client| {
        phone.todo("WI-002".into()).is_some_and(|t| {
            let checks: Vec<(&str, bool)> = t.checklist.iter().map(|c| (c.text.as_str(), c.done)).collect();
            t.item.title == "Glide after a flick" && t.item.blocker && t.item.labels == ["ui"] && checks.len() == 4 && checks[0].0 == "Glide on release" && checks[3] == ("Try it on the phone", true)
        })
    };
    assert!(edited(&client), "every edit here at once");
    wait_until("the edits on the other phone", || edited(&other));
    client.cancel_todo("WI-004".into(), "Not needed".into()).unwrap();
    assert!(client.todo("WI-004".into()).unwrap().actions.reopen);
    client.reopen_todo("WI-004".into()).unwrap();
    client.release_todo("WI-003".into()).unwrap();
    let settled = |phone: &Client| {
        phone.todo("WI-004".into()).is_some_and(|t| t.item.status == TodoStatus::Todo && t.log.iter().any(|l| l.text.contains("Not needed"))) && phone.todo("WI-003".into()).is_some_and(|t| t.item.claimed_by.is_empty())
    };
    assert!(settled(&client), "canceled, reopened and released here at once");
    wait_until("canceled, reopened and released on the other phone", || settled(&other));
    assert!(settled(&client) && edited(&client), "and here still, the service's books having them all");
    // The service refuses what the book does: a removed item beyond the list.
    assert!(client.remove_check("WI-002".into(), 40).is_err());
    let _ = std::fs::remove_dir_all(state);
}
