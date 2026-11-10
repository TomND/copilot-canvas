//! The gateway with the stand-in server (this test binary, `standin_process`)
//! as the hub's server, the example pack's policy, and a recording link.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use canvas_common::sat::{MCP_CLOSE, MCP_ERROR, MCP_MESSAGE, MCP_OPEN, McpOpen, SatUp};
use canvas_model::vm::{VmPlacement, VmState};
use crossbeam_channel::{Receiver, Sender};
use serde_json::{Value, json};

use super::*;
use crate::vm::launch;

const A: &str = "sbx-aaaaaaaaaaaa";
const B: &str = "sbx-bbbbbbbbbbbb";
const BOX1: &str = "0b1e5ca1-0000-4000-8000-00000000a001";
const BOX2: &str = "0b1e5ca1-0000-4000-8000-00000000a002";
const BOX3: &str = "0b1e5ca1-0000-4000-8000-00000000a003";
const BRANCH: &str = "atelier/fix-login";
const TOKEN: &str = "standin-credential-0123456789";
const LOG_ENV: &str = "ATELIER_MCP_TEST_STANDIN_LOG";

/// This test binary as the hub's MCP server (the gateway starts it).
#[test]
#[ignore = "the stand-in MCP server the gateway's tests run"]
fn standin_process() {
    let Ok(log) = std::env::var(LOG_ENV) else { return };
    let (stdin, stdout) = (std::io::stdin(), std::io::stdout());
    standin::run(stdin.lock(), stdout.lock(), Path::new(&log)).unwrap();
}

/// The hub's configuration as the tests see it: `forge` is the stand-in.
struct Standin {
    log: PathBuf,
    leak: bool,
}

impl ServerSource for Standin {
    fn spec(&self, server: &str) -> Option<ServerSpec> {
        match server {
            "forge" => {
                let exe = std::env::current_exe().ok()?.display().to_string();
                let args = ["vm::mcp::tests::standin_process", "--exact", "--ignored", "--nocapture", "--quiet", "--test-threads=1"].map(str::to_owned).to_vec();
                let mut env = vec![(LOG_ENV.to_owned(), self.log.display().to_string()), (standin::TOKEN_ENV.to_owned(), TOKEN.to_owned())];
                if self.leak {
                    env.push((standin::LEAK_ENV.to_owned(), "1".to_owned()));
                }
                Some(ServerSpec::Stdio { command: exe, args, env })
            }
            "remote" => Some(ServerSpec::Http { url: "https://mcp.example.com/mcp".into(), headers: vec![("Authorization".into(), format!("Bearer {TOKEN}"))] }),
            _ => None,
        }
    }
}

type Sent = (String, u64, u8, Vec<u8>);

/// The link as the gateway sees it: what it sends a guest is recorded.
struct Link {
    tx: Sender<Sent>,
    all: Mutex<Vec<Vec<u8>>>,
}

impl Down for Link {
    fn send(&self, sandbox: &str, stream: u64, kind: u8, bytes: Vec<u8>) -> bool {
        self.all.lock().unwrap().push(bytes.clone());
        let _ = self.tx.send((sandbox.to_owned(), stream, kind, bytes));
        true
    }
}

struct Harness {
    home: PathBuf,
    log: PathBuf,
    gateway: Arc<Gateway>,
    link: Arc<Link>,
    rx: Receiver<Sent>,
    stash: Vec<Sent>,
}

/// The example pack with `policy` as its `mcp-policy.yaml` (`None`: the example's).
fn pack(home: &Path, policy: Option<&str>) -> PathBuf {
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/vm-pack");
    let folder = home.join("pack");
    std::fs::create_dir_all(folder.join("egress")).unwrap();
    for name in ["vm.yaml", "repos.yaml", "secrets.yaml", "mcp-policy.yaml", "profile.yaml", "paths.yaml", "egress/forge.yaml"] {
        std::fs::copy(example.join(name), folder.join(name)).unwrap();
    }
    if let Some(policy) = policy {
        std::fs::write(folder.join("mcp-policy.yaml"), policy).unwrap();
    }
    let (_, status) = crate::vm::pack::load(&folder);
    assert_eq!(status.errors(), 0, "{:?}", status.findings);
    folder
}

impl Harness {
    fn new(policy: Option<&str>, leak: bool, idle: Duration) -> Harness {
        let home = std::env::temp_dir().join(format!("atelier-mcp-gateway-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let folder = pack(&home, policy);
        let log = home.join("standin.jsonl");
        let (tx, rx) = crossbeam_channel::unbounded();
        let link = Arc::new(Link { tx, all: Mutex::default() });
        let options = Options { idle, pack: PackSource::Folder(folder) };
        let gateway = Gateway::with_options(&home, Arc::new(Standin { log: log.clone(), leak }), link.clone(), options);
        Harness { home, log, gateway, link, rx, stash: Vec::new() }
    }

    fn example() -> Harness {
        Harness::new(None, false, Duration::from_secs(600))
    }

    /// Box `term` launched on `sandbox` with MCP preset `preset`.
    fn launch(&self, term: &str, sandbox: &str, preset: Option<&str>) {
        let placement = VmPlacement { repo: "contoso-app".into(), branch: Some(BRANCH.into()), mcp: preset.map(str::to_owned), ..Default::default() };
        let mut intent = launch::Intent { r#box: term.into(), agent: "copilot".into(), placement, sandbox: sandbox.into(), state: VmState::Running, ..Default::default() };
        launch::save(&self.home, &mut intent).unwrap();
    }

    fn open(&self, sandbox: &str, stream: u64, term: &str, server: &str) {
        let open = McpOpen { term: term.into(), server: server.into() };
        self.gateway.up(sandbox, stream, MCP_OPEN, serde_json::to_vec(&open).unwrap());
    }

    fn send(&self, sandbox: &str, stream: u64, message: &Value) {
        self.gateway.up(sandbox, stream, MCP_MESSAGE, message.to_string().into_bytes());
    }

    /// The gateway's next message to stream `stream` of `sandbox`: its kind and JSON (`Null` for a close).
    fn next(&mut self, sandbox: &str, stream: u64) -> (u8, Value) {
        let mine = |s: &Sent| s.0 == sandbox && s.1 == stream;
        if let Some(at) = self.stash.iter().position(mine) {
            let (_, _, kind, bytes) = self.stash.remove(at);
            return (kind, serde_json::from_slice(&bytes).unwrap_or(Value::Null));
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let sent = self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap_or_else(|_| panic!("nothing for {sandbox}/{stream}"));
            if mine(&sent) {
                return (sent.2, serde_json::from_slice(&sent.3).unwrap_or(Value::Null));
            }
            self.stash.push(sent);
        }
    }

    /// The next JSON-RPC message to a stream.
    fn message(&mut self, sandbox: &str, stream: u64) -> Value {
        let (kind, message) = self.next(sandbox, stream);
        assert_eq!(kind, MCP_MESSAGE, "{message}");
        message
    }

    fn request(&mut self, sandbox: &str, stream: u64, id: Value, method: &str, params: Value) -> Value {
        self.send(sandbox, stream, &jsonrpc::request(id, method, params));
        self.message(sandbox, stream)
    }

    fn call(&mut self, sandbox: &str, stream: u64, id: Value, tool: &str, arguments: Value) -> Value {
        self.request(sandbox, stream, id, "tools/call", json!({ "name": tool, "arguments": arguments }))
    }

    /// Opens stream `stream` and initializes it.
    fn initialize(&mut self, sandbox: &str, stream: u64, term: &str, capabilities: Value) -> Value {
        self.open(sandbox, stream, term, "forge");
        let answer = self.request(sandbox, stream, json!(1), "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": capabilities, "clientInfo": { "name": "copilot", "version": "1" } }));
        assert!(answer.get("result").is_some(), "{answer}");
        self.send(sandbox, stream, &jsonrpc::notification("notifications/initialized", json!({})));
        answer
    }

    /// The refusal that closes a stream: its message.
    fn refused(&mut self, sandbox: &str, stream: u64) -> String {
        let (kind, error) = self.next(sandbox, stream);
        assert_eq!(kind, MCP_ERROR, "{error}");
        assert_eq!(self.next(sandbox, stream).0, MCP_CLOSE);
        error["message"].as_str().unwrap_or_default().to_owned()
    }

    /// What the stand-in received.
    fn logged(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    fn calls(&self) -> Vec<Value> {
        self.logged().into_iter().filter(|m| m["method"] == "tools/call").collect()
    }

    fn refusals(&self) -> Vec<Value> {
        std::fs::read_to_string(policy::refusals_path(&self.home)).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    fn pids(&self, term: &str) -> Vec<u32> {
        self.gateway.running().iter().filter(|r| r.r#box == term).map(|r| r.pid).collect()
    }

    /// Whether the credential never reached the guest.
    fn clean(&self) -> bool {
        self.link.all.lock().unwrap().iter().all(|bytes| !String::from_utf8_lossy(bytes).contains(TOKEN))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn wait_until(what: &str, ok: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ok() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn text(answer: &Value) -> String {
    answer.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn names(answer: &Value) -> Vec<String> {
    answer.pointer("/result/tools").and_then(Value::as_array).into_iter().flatten().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect()
}

#[test]
fn the_example_policy_lists_allows_and_refuses_and_the_server_never_sees_a_refusal() {
    let mut h = Harness::example();
    h.launch(BOX1, A, Some("forge-readonly"));
    h.open(A, 1, BOX1, "forge");
    let init = h.request(A, 1, json!("init-1"), "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": { "roots": { "listChanged": true }, "sampling": {} }, "clientInfo": { "name": "copilot", "version": "1" } }));
    assert_eq!(init["id"], "init-1", "the guest's ids are kept");
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["capabilities"], json!({ "tools": { "listChanged": true } }), "tools only: no resources, prompts, logging or completions");
    assert_eq!(init["result"]["serverInfo"]["name"], "atelier-mcp-standin");
    h.send(A, 1, &jsonrpc::notification("notifications/initialized", json!({})));

    // tools/list, page by page, filtered to what the box may call.
    let page = h.request(A, 1, json!(2), "tools/list", json!({}));
    assert_eq!(names(&page), ["list_pull_requests", "create_pull_request"]);
    let cursor = page["result"]["nextCursor"].clone();
    let rest = h.request(A, 1, json!(3), "tools/list", json!({ "cursor": cursor }));
    assert!(names(&rest).is_empty(), "vote is denied and add_comment is named nowhere: {rest}");

    let vote = h.call(A, 1, json!(4), "vote", json!({ "number": 1, "vote": 10 }));
    assert_eq!((vote["id"].clone(), vote["error"]["code"].clone(), vote["error"]["data"]["rule"].clone()), (json!(4), json!(jsonrpc::REFUSED), json!("deny vote")));
    let update = h.call(A, 1, json!(5), "update_pull_request", json!({ "number": 1, "state": "closed" }));
    assert_eq!(update["error"]["data"]["rule"], "deny update_pull_request");
    let comment = h.call(A, 1, json!(6), "add_comment", json!({ "number": 1, "body": "hi" }));
    assert_eq!(comment["error"]["data"]["rule"], policy::DEFAULT_DENY);

    let draft = json!({ "draft": true, "head": BRANCH, "base": "main", "title": "Fix the login page" });
    let created = h.call(A, 1, json!("c-1"), "create_pull_request", draft.clone());
    assert_eq!(created["id"], "c-1");
    assert!(text(&created).contains("created draft pull request #3") && text(&created).contains("credential: present"), "{created}");
    let other_head = h.call(A, 1, json!(7), "create_pull_request", json!({ "draft": true, "head": "main", "base": "main", "title": "Sneaky" }));
    assert_eq!(other_head["error"]["code"], jsonrpc::REFUSED);
    assert!(other_head["error"]["message"].as_str().unwrap().contains(&format!("this session's branch, \"{BRANCH}\"")), "{other_head}");
    let not_draft = h.call(A, 1, json!(8), "create_pull_request", json!({ "draft": false, "head": BRANCH, "base": "main", "title": "Ready" }));
    assert_eq!(not_draft["error"]["code"], jsonrpc::REFUSED);
    let mut extra = draft.clone();
    extra["force"] = json!(true);
    let unknown_argument = h.call(A, 1, json!(9), "create_pull_request", extra);
    assert_eq!(unknown_argument["error"]["code"], jsonrpc::INVALID_PARAMS);
    assert!(unknown_argument["error"]["message"].as_str().unwrap().contains("unknown argument \"force\""), "{unknown_argument}");
    let unknown_parameter = h.request(A, 1, json!(10), "tools/call", json!({ "name": "list_pull_requests", "arguments": {}, "task": { "ttl": 1 } }));
    assert!(unknown_parameter["error"]["message"].as_str().unwrap().contains("no parameter \"task\""), "{unknown_parameter}");
    let resources = h.request(A, 1, json!(11), "resources/list", json!({}));
    assert_eq!(resources["error"]["code"], jsonrpc::METHOD_NOT_FOUND);
    let listed = h.call(A, 1, json!(12), "list_pull_requests", json!({ "state": "open" }));
    assert!(text(&listed).starts_with("open pull requests"), "{listed}");
    assert_eq!(h.request(A, 1, json!(13), "ping", json!({}))["result"], json!({}));

    // The stand-in saw only what was allowed, with the guest's capabilities cut to what the policy allows.
    let logged = h.logged();
    let calls: Vec<String> = h.calls().iter().map(|c| c["params"]["name"].as_str().unwrap().to_owned()).collect();
    assert_eq!(calls, ["create_pull_request", "list_pull_requests"], "{logged:#?}");
    assert_eq!(h.calls()[0]["params"]["arguments"]["head"], BRANCH);
    assert!(!logged.iter().any(|m| m["method"] == "resources/list"));
    let initialize = logged.iter().find(|m| m["method"] == "initialize").unwrap();
    assert_eq!(initialize["params"]["capabilities"], json!({}), "no roots or sampling for a policy that allows neither");
    assert_eq!(logged.iter().filter(|m| m["method"] == "notifications/initialized").count(), 1);

    // Every refusal is recorded, never with an argument's value.
    let refusals = h.refusals();
    let tools: Vec<&str> = refusals.iter().map(|r| r["tool"].as_str().unwrap()).collect();
    assert_eq!(tools, ["vote", "update_pull_request", "add_comment", "create_pull_request", "create_pull_request", "create_pull_request", "list_pull_requests", "resources/list"]);
    assert!(refusals.iter().all(|r| r["sandbox"] == A && r["box"] == BOX1 && r["server"] == "forge" && r["kind"] == "mcp"));
    let recorded = std::fs::read_to_string(policy::refusals_path(&h.home)).unwrap();
    assert!(!recorded.contains("Sneaky") && !recorded.contains("Ready") && !recorded.contains("\"ttl\""), "{recorded}");
    assert!(h.clean(), "the credential never reached the guest");
}

#[test]
fn a_call_before_any_list_waits_for_the_tools_and_progress_and_cancellations_pass_for_live_requests() {
    let mut h = Harness::example();
    h.launch(BOX1, A, Some("forge-readonly"));
    h.open(A, 1, BOX1, "forge");
    let early = h.request(A, 1, json!(1), "tools/list", json!({}));
    assert!(early["error"]["message"].as_str().unwrap().contains("initialize first"));
    h.initialize(A, 1, BOX1, json!({}));
    // No tools/list yet: the gateway reads the server's tools itself (both pages) first.
    h.send(A, 1, &jsonrpc::request(json!(10), "tools/call", json!({ "name": "list_pull_requests", "arguments": { "progress": true }, "_meta": { "progressToken": "tok-1" } })));
    for step in 1..=2 {
        let progress = h.message(A, 1);
        assert_eq!((progress["method"].as_str(), progress["params"]["progressToken"].as_str(), progress["params"]["progress"].as_i64()), (Some("notifications/progress"), Some("tok-1"), Some(step)), "the guest's own token");
    }
    let answer = h.message(A, 1);
    assert_eq!(answer["id"], 10);
    let lists: Vec<Value> = h.logged().into_iter().filter(|m| m["method"] == "tools/list").collect();
    assert_eq!(lists.len(), 2, "both pages");
    let sent = h.calls().pop().unwrap();
    assert_ne!(sent["params"]["_meta"]["progressToken"], "tok-1", "the server sees the gateway's token");

    // A held call, cancelled: the cancellation reaches the server with the server's id for it.
    h.send(A, 1, &jsonrpc::request(json!(11), "tools/call", json!({ "name": "list_pull_requests", "arguments": { "hold": true } })));
    wait_until("the held call reached the server", || h.calls().len() == 2);
    h.send(A, 1, &jsonrpc::notification("notifications/cancelled", json!({ "requestId": 99, "reason": "not live" })));
    h.send(A, 1, &jsonrpc::notification("notifications/cancelled", json!({ "requestId": 11, "reason": "the user pressed Esc" })));
    wait_until("the cancellation reached the server", || h.logged().iter().any(|m| m["method"] == "notifications/cancelled"));
    let held = h.calls().pop().unwrap();
    let cancels: Vec<Value> = h.logged().into_iter().filter(|m| m["method"] == "notifications/cancelled").collect();
    assert_eq!(cancels.len(), 1, "a cancellation of a request that is not live does not pass: {cancels:?}");
    assert_eq!(cancels[0]["params"]["requestId"], held["id"]);
    assert_eq!(cancels[0]["params"]["reason"], "the user pressed Esc");
    // The session goes on.
    assert_eq!(h.request(A, 1, json!(12), "ping", json!({}))["id"], 12);
    assert!(h.clean());
}

#[test]
fn server_initiated_requests_are_refused_unless_the_policy_names_them() {
    let mut h = Harness::example();
    h.launch(BOX1, A, Some("forge-readonly"));
    h.initialize(A, 1, BOX1, json!({ "roots": {} }));
    let refused = h.call(A, 1, json!(20), "list_pull_requests", json!({ "ask": "roots/list" }));
    assert_eq!(refused["id"], 20, "the guest got no request first");
    assert!(text(&refused).starts_with("roots/list refused: roots/list is not allowed"), "{refused}");
    assert!(h.refusals().iter().any(|r| r["tool"] == "roots/list" && r["box"] == BOX1));

    let policy = "presets:\n  forge-readonly: [forge]\nservers:\n  forge:\n    rules:\n      - allow: list_pull_requests\n      - allow: roots/list\n";
    let mut h = Harness::new(Some(policy), false, Duration::from_secs(600));
    h.launch(BOX1, A, Some("forge-readonly"));
    h.initialize(A, 1, BOX1, json!({ "roots": { "listChanged": true }, "sampling": {} }));
    let initialize = h.logged().into_iter().find(|m| m["method"] == "initialize").unwrap();
    assert_eq!(initialize["params"]["capabilities"], json!({ "roots": { "listChanged": true } }), "roots allowed, sampling not");
    h.send(A, 1, &jsonrpc::request(json!(21), "tools/call", json!({ "name": "list_pull_requests", "arguments": { "ask": "roots/list" } })));
    let asked = h.message(A, 1);
    assert_eq!((asked["method"].as_str(), asked["id"].as_str()), (Some("roots/list"), Some("standin-1")));
    h.send(A, 1, &jsonrpc::result(json!("standin-99"), json!({ "roots": [] })));
    h.send(A, 1, &jsonrpc::result(json!("standin-1"), json!({ "roots": [{ "uri": "file:///home/agent/work" }] })));
    let answered = h.message(A, 1);
    assert_eq!(answered["id"], 21);
    assert!(text(&answered).starts_with("roots/list answered"), "{answered}");
    assert!(!h.logged().iter().any(|m| m["id"] == "standin-99"), "an answer to nothing the server asked never reaches it");
    let sampling = h.call(A, 1, json!(22), "list_pull_requests", json!({ "ask": "sampling/createMessage" }));
    assert!(text(&sampling).contains("refused"), "{sampling}");
}

#[test]
fn one_process_per_session_and_none_after_it_ends() {
    let mut h = Harness::example();
    h.launch(BOX1, A, Some("forge-readonly"));
    h.launch(BOX2, A, Some("forge-readonly"));
    h.open(A, 1, BOX1, "forge");
    assert!(h.gateway.running().is_empty(), "nothing starts before the session's initialize");
    let answer = h.request(A, 1, json!(1), "initialize", json!({ "protocolVersion": "2025-06-18" }));
    assert!(answer.get("result").is_some());
    h.initialize(A, 2, BOX2, json!({}));
    let (one, two) = (h.pids(BOX1), h.pids(BOX2));
    assert!(one.len() == 1 && two.len() == 1 && one != two, "one process per box: {:?}", h.gateway.running());
    // A newer session of the same box replaces the older and its process.
    h.initialize(A, 3, BOX1, json!({}));
    assert!(h.refused(A, 1).contains("replaced"));
    let newer = h.pids(BOX1);
    assert!(newer.len() == 1 && newer != one);
    wait_until("the replaced process ended", || !canvas_sys::process::alive(one[0]));
    // The box's program ends: its sessions and their processes end.
    h.gateway.session_ended(BOX1);
    assert!(h.refused(A, 3).contains("session ended"));
    wait_until("the session's process ended", || h.pids(BOX1).is_empty() && !canvas_sys::process::alive(newer[0]));
    assert_eq!(h.pids(BOX2), two, "the other box's session goes on");
    // The guest closes a stream: its process ends.
    h.gateway.up(A, 2, MCP_CLOSE, Vec::new());
    wait_until("the closed stream's process ended", || h.gateway.running().is_empty() && !canvas_sys::process::alive(two[0]));
    // The link ends: everything of that sandbox ends, without a word down the dead link.
    h.initialize(A, 4, BOX2, json!({}));
    let last = h.pids(BOX2);
    let sent = h.link.all.lock().unwrap().len();
    h.gateway.link_ended(A);
    wait_until("the link's processes ended", || h.gateway.running().is_empty() && !canvas_sys::process::alive(last[0]));
    assert_eq!(h.link.all.lock().unwrap().len(), sent);
    // Messages for a stream that ended are ignored.
    h.send(A, 4, &jsonrpc::request(json!(9), "ping", json!({})));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(h.link.all.lock().unwrap().len(), sent);
}

#[test]
fn an_idle_server_ends_with_its_session() {
    let mut h = Harness::new(None, false, Duration::from_millis(400));
    h.launch(BOX1, A, Some("forge-readonly"));
    h.initialize(A, 1, BOX1, json!({}));
    let pid = h.pids(BOX1);
    assert!(h.refused(A, 1).contains("idle"));
    wait_until("the idle process ended", || h.gateway.running().is_empty() && !canvas_sys::process::alive(pid[0]));
    h.open(A, 2, BOX1, "forge");
    assert!(h.refused(A, 2).contains("never initialized"), "a stream never initialized ends too");
}

#[test]
fn a_stream_is_refused_for_another_sandboxs_box_and_for_servers_the_box_may_not_use() {
    let policy = "presets:\n  forge-readonly: [forge]\n  wide: [forge, remote, missing]\nservers:\n  forge:\n    rules:\n      - allow: list_pull_requests\n  remote:\n    rules: []\n  missing:\n    rules: []\n";
    let mut h = Harness::new(Some(policy), false, Duration::from_secs(600));
    h.launch(BOX1, A, Some("forge-readonly"));
    h.launch(BOX2, A, None);
    h.launch(BOX3, A, Some("wide"));
    h.open(B, 1, BOX1, "forge");
    assert!(h.refused(B, 1).contains("does not run in this VM"));
    assert!(h.refusals().iter().any(|r| r["sandbox"] == B && r["box"] == BOX1 && r["rule"] == "box of another VM"));
    h.open(A, 2, "0b1e5ca1-0000-4000-8000-00000000ffff", "forge");
    assert!(h.refused(A, 2).contains("not a VM session of this hub"));
    h.open(A, 3, "../x", "forge");
    assert!(h.refused(A, 3).contains("not a box"));
    h.open(A, 4, BOX2, "forge");
    assert!(h.refused(A, 4).contains("without an MCP preset"));
    h.open(A, 5, BOX1, "github");
    assert!(h.refused(A, 5).contains("does not include the server \"github\""));
    h.open(A, 6, BOX3, "remote");
    assert!(h.refused(A, 6).contains("remote (HTTP)"));
    h.open(A, 7, BOX3, "missing");
    assert!(h.refused(A, 7).contains("has no MCP server named missing"));
    h.gateway.up(A, 8, MCP_OPEN, b"not json".to_vec());
    assert!(h.refused(A, 8).contains("opening"));
    // A message before any open, and one for a refused stream: nothing.
    h.send(A, 1, &jsonrpc::request(json!(1), "initialize", json!({})));
    h.open(A, 9, BOX1, "forge");
    let answer = h.request(A, 9, json!(1), "initialize", json!({ "protocolVersion": "2025-06-18" }));
    assert!(answer.get("result").is_some(), "the box's own sandbox and preset: allowed");
    assert_eq!(h.gateway.running().len(), 1, "only the allowed session started a server");
    assert!(h.clean(), "the remote server's header never reached the guest");
}

#[test]
fn a_pinned_protocol_version_framing_and_large_messages() {
    let policy = "presets:\n  forge-readonly: [forge]\nservers:\n  forge:\n    protocol: \"2025-06-18\"\n    rules:\n      - allow: create_pull_request\n        when:\n          head: \"{{box.branch}}\"\n";
    let mut h = Harness::new(Some(policy), false, Duration::from_secs(600));
    h.launch(BOX1, A, Some("forge-readonly"));
    h.open(A, 1, BOX1, "forge");
    let answer = h.request(A, 1, json!(1), "initialize", json!({ "protocolVersion": "2025-11-25" }));
    assert_eq!(answer["result"]["protocolVersion"], "2025-06-18", "the pinned version, not the guest's");
    let initialize = h.logged().into_iter().find(|m| m["method"] == "initialize").unwrap();
    assert_eq!(initialize["params"]["protocolVersion"], "2025-06-18");
    let again = h.request(A, 1, json!(2), "initialize", json!({ "protocolVersion": "2025-06-18" }));
    assert!(again["error"]["message"].as_str().unwrap().contains("initialized already"));
    // One message at a time on the link: a batch is refused, so is what is not JSON.
    h.gateway.up(A, 1, MCP_MESSAGE, json!([jsonrpc::request(json!(3), "ping", json!({}))]).to_string().into_bytes());
    let batch = h.message(A, 1);
    assert_eq!((batch["id"].clone(), batch["error"]["code"].clone()), (Value::Null, json!(jsonrpc::INVALID_REQUEST)));
    h.gateway.up(A, 1, MCP_MESSAGE, b"{ nope".to_vec());
    assert_eq!(h.message(A, 1)["error"]["code"], jsonrpc::PARSE_ERROR);
    // A large call crosses whole; an answer larger than the link carries becomes an error.
    let title = "t".repeat(600 << 10);
    let created = h.call(A, 1, json!(4), "create_pull_request", json!({ "head": BRANCH, "base": "main", "title": title }));
    assert!(text(&created).contains(&title), "{}", &text(&created)[..100]);
    let wide = "\u{7f}".repeat(200 << 10);
    let too_large = h.call(A, 1, json!(5), "create_pull_request", json!({ "head": BRANCH, "base": "main", "title": wide }));
    assert_eq!((too_large["id"].clone(), too_large["error"]["code"].clone()), (json!(5), json!(jsonrpc::INTERNAL_ERROR)));
    assert!(too_large["error"]["message"].as_str().unwrap().contains("too large"));
    assert_eq!(h.calls().len(), 2, "the server got both");
}

/// `mode: shared` (VM-56 turns it on only after these pass): initialize,
/// notifications, cancellation, the server's credential (its auth context)
/// and server-initiated requests, with sessions of several boxes and VMs on
/// one process.
#[test]
fn shared_mode_one_process_for_every_session_with_answers_kept_apart() {
    let policy = "presets:\n  forge-readonly: [forge]\nservers:\n  forge:\n    mode: shared\n    rules:\n      - allow: list_pull_requests\n";
    let mut h = Harness::new(Some(policy), false, Duration::from_secs(600));
    h.launch(BOX1, A, Some("forge-readonly"));
    h.launch(BOX2, A, Some("forge-readonly"));
    h.launch(BOX3, B, Some("forge-readonly"));
    // The same ids on every session, and the same stream id on two VMs.
    h.initialize(A, 1, BOX1, json!({}));
    h.initialize(A, 2, BOX2, json!({}));
    h.initialize(B, 1, BOX3, json!({}));
    let running = h.gateway.running();
    assert_eq!(running.len(), 1, "{running:?}");
    assert_eq!(running[0].r#box, "", "a shared server is no box's");
    let logged = h.logged();
    assert_eq!(logged.iter().filter(|m| m["method"] == "initialize").count(), 1, "the server is initialized once");
    assert_eq!(logged.iter().filter(|m| m["method"] == "notifications/initialized").count(), 1);
    // Progress with the same token on two sessions: each gets its own.
    for (sandbox, stream) in [(A, 1), (A, 2), (B, 1)] {
        h.send(sandbox, stream, &jsonrpc::request(json!(5), "tools/call", json!({ "name": "list_pull_requests", "arguments": { "progress": true }, "_meta": { "progressToken": "t" } })));
    }
    for (sandbox, stream) in [(A, 1), (A, 2), (B, 1)] {
        for step in 1..=2 {
            let progress = h.message(sandbox, stream);
            assert_eq!((progress["params"]["progressToken"].as_str(), progress["params"]["progress"].as_i64()), (Some("t"), Some(step)));
        }
        let answer = h.message(sandbox, stream);
        assert_eq!(answer["id"], 5);
        assert!(text(&answer).contains("credential: present"), "the hub's credential serves every session: {answer}");
    }
    let tokens: std::collections::HashSet<String> = h.calls().iter().map(|c| c["params"]["_meta"]["progressToken"].to_string()).collect();
    assert_eq!(tokens.len(), 3, "the server saw three tokens");
    // Two held calls with the same id: a cancellation on one reaches only its own.
    for (sandbox, stream) in [(A, 1), (A, 2)] {
        h.send(sandbox, stream, &jsonrpc::request(json!(6), "tools/call", json!({ "name": "list_pull_requests", "arguments": { "hold": true } })));
    }
    wait_until("both held calls reached the server", || h.calls().len() == 5);
    let held: Vec<Value> = h.calls()[3..].iter().map(|c| c["id"].clone()).collect();
    assert_ne!(held[0], held[1], "the server sees ids of the gateway's");
    h.send(A, 2, &jsonrpc::notification("notifications/cancelled", json!({ "requestId": 6 })));
    wait_until("the cancellation reached the server", || h.logged().iter().any(|m| m["method"] == "notifications/cancelled"));
    let cancelled = h.logged().into_iter().find(|m| m["method"] == "notifications/cancelled").unwrap();
    let second = h.calls().into_iter().find(|c| c["id"] == cancelled["params"]["requestId"]).unwrap();
    assert_eq!(second["id"], held[1], "{cancelled}");
    // A server-initiated request is refused for a shared server too.
    let asked = h.call(B, 1, json!(7), "list_pull_requests", json!({ "ask": "elicitation/create" }));
    assert!(text(&asked).contains("elicitation/create refused"), "{asked}");
    // Sessions end; the shared process stays.
    let pid = running[0].pid;
    h.gateway.up(A, 1, MCP_CLOSE, Vec::new());
    h.gateway.session_ended(BOX2);
    assert!(h.refused(A, 2).contains("session ended"));
    h.gateway.link_ended(B);
    assert_eq!(h.gateway.running().iter().map(|r| r.pid).collect::<Vec<_>>(), [pid], "shared: one process per server, kept");
    h.initialize(A, 4, BOX1, json!({}));
    let again = h.call(A, 4, json!(8), "list_pull_requests", json!({}));
    assert_eq!(again["id"], 8);
    assert_eq!(h.logged().iter().filter(|m| m["method"] == "initialize").count(), 1, "a new session joins the running server");
    assert!(h.clean());
}

#[test]
fn the_servers_credential_never_reaches_the_guest_even_when_the_server_repeats_it() {
    let mut h = Harness::new(None, true, Duration::from_secs(600));
    h.launch(BOX1, A, Some("forge-readonly"));
    h.initialize(A, 1, BOX1, json!({}));
    let listed = h.call(A, 1, json!(2), "list_pull_requests", json!({}));
    assert!(text(&listed).contains("credential: [redacted]"), "{listed}");
    let created = h.call(A, 1, json!(3), "create_pull_request", json!({ "draft": true, "head": BRANCH, "base": "main", "title": "T" }));
    assert!(text(&created).contains("[redacted]"));
    assert!(std::fs::read_to_string(&h.log).unwrap().contains("initialize"), "the stand-in ran");
    assert!(h.clean(), "not in any message down the link (the server's log notification was dropped)");
}

/// The guest's proxy, the link (channels) and the gateway with the stand-in, over HTTP.
#[test]
fn an_mcp_client_in_the_guest_uses_the_hubs_server_through_the_proxy_and_the_gateway() {
    use crate::vm::satellite::mcp_proxy::{McpProxy, client};

    struct Guest(Arc<McpProxy>, Mutex<Vec<Vec<u8>>>);
    impl Down for Guest {
        fn send(&self, _sandbox: &str, stream: u64, kind: u8, bytes: Vec<u8>) -> bool {
            self.1.lock().unwrap().push(bytes.clone());
            self.0.down(stream, kind, bytes);
            true
        }
    }
    let home = std::env::temp_dir().join(format!("atelier-mcp-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let folder = pack(&home, None);
    let proxy = McpProxy::start(0).unwrap();
    let guest = Arc::new(Guest(proxy.clone(), Mutex::default()));
    let log = home.join("standin.jsonl");
    let gateway = Gateway::with_options(&home, Arc::new(Standin { log: log.clone(), leak: true }), guest.clone(), Options { idle: Duration::from_secs(600), pack: PackSource::Folder(folder) });
    let (up, links) = crossbeam_channel::bounded::<SatUp>(1024);
    proxy.attach(Some(up));
    {
        let gateway = gateway.clone();
        std::thread::spawn(move || {
            for message in links {
                if let SatUp::Mcp { stream, kind, bytes } = message {
                    gateway.up(A, stream, kind, bytes);
                }
            }
        });
    }
    let placement = VmPlacement { repo: "contoso-app".into(), branch: Some(BRANCH.into()), mcp: Some("forge-readonly".into()), ..Default::default() };
    launch::save(&home, &mut launch::Intent { r#box: BOX1.into(), agent: "copilot".into(), placement, sandbox: A.into(), state: VmState::Running, ..Default::default() }).unwrap();

    let port = proxy.port();
    let path = format!("/mcp/{BOX1}/forge");
    let init = client::post(port, &path, None, &jsonrpc::request(json!(0), "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "copilot", "version": "1" } })));
    assert_eq!(init.status, 200, "{}", init.text());
    let session = init.header("Mcp-Session-Id").unwrap().to_owned();
    assert_eq!(init.json()["result"]["capabilities"], json!({ "tools": { "listChanged": true } }));
    assert_eq!(client::post(port, &path, Some(&session), &jsonrpc::notification("notifications/initialized", json!({}))).status, 202);
    let mut answers = Vec::new();
    let list = client::post(port, &path, Some(&session), &jsonrpc::request(json!(1), "tools/list", json!({})));
    assert_eq!(names(&list.json()), ["list_pull_requests", "create_pull_request"]);
    let vote = client::post(port, &path, Some(&session), &jsonrpc::request(json!(2), "tools/call", json!({ "name": "vote", "arguments": { "number": 1, "vote": 10 } })));
    assert_eq!((vote.status, vote.json()["error"]["code"].clone()), (200, json!(jsonrpc::REFUSED)));
    let progress = client::post(port, &path, Some(&session), &jsonrpc::request(json!(3), "tools/call", json!({ "name": "list_pull_requests", "arguments": { "progress": true }, "_meta": { "progressToken": 7 } })));
    assert_eq!(progress.header("Content-Type"), Some("text/event-stream"), "progress first: the answer streams");
    let events = progress.events();
    assert_eq!((events.len(), events[0]["params"]["progressToken"].clone(), events[2]["id"].clone()), (3, json!(7), json!(3)));
    assert!(text(&events[2]).contains("credential: [redacted]"));
    answers.extend([init, list, vote, progress]);
    // DELETE ends the session and its server.
    let pid = gateway.running()[0].pid;
    assert_eq!(client::request(port, "DELETE", &path, &[("Mcp-Session-Id", &session)], b"").status, 200);
    wait_until("the session's server ended", || gateway.running().is_empty() && !canvas_sys::process::alive(pid));
    assert_eq!(client::post(port, &path, Some(&session), &jsonrpc::request(json!(4), "ping", json!({}))).status, 404);
    // Another box's path, or a box of another VM: refused with the hub's reason.
    let foreign = client::post(port, &format!("/mcp/{BOX2}/forge"), None, &jsonrpc::request(json!(0), "initialize", json!({ "protocolVersion": "2025-06-18" })));
    assert_eq!(foreign.status, 403);
    assert!(foreign.json()["error"]["message"].as_str().unwrap().contains("not a VM session of this hub"));
    for answer in &answers {
        assert!(!answer.text().contains(TOKEN), "the credential is in none of the proxy's answers");
    }
    assert!(guest.1.lock().unwrap().iter().all(|b| !String::from_utf8_lossy(b).contains(TOKEN)));
    proxy.attach(None);
    let _ = std::fs::remove_dir_all(home);
}
