//! `vm::aca`: Azure Container Apps Sandboxes as a [`Provider`], through the
//! data plane's REST API (docs/VM-SESSIONS-PLAN.md Appendix A) with a cached
//! bearer token ([`token`]); no CLI on the hot path. Sandboxes are created
//! with the platform's idle suspend off (the canvas suspends them itself,
//! plan §5.9) and never with an auto-delete policy.
//!
//! Every call is blocking and bounded. Errors name the operation, the status
//! and the service's reason; never the token, a secret, a request body or
//! the pack's values (the subscription, groups and endpoint show as
//! `<subscription>`, … ). Tests run against an in-process HTTP stand-in
//! ([`rest::standin`]); nothing here is ever tested against Azure.

pub mod model;
pub mod rest;
pub mod token;

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

use self::model::Stdin;
use self::rest::{Call, Method, Payload, Reply, Rest};
pub use self::token::{Cache, CommandSource, Token, TokenSource};
use super::pack::{DEFAULT_API_VERSION, Pack};
use super::provider::{EgressDecision, EgressPolicy, ExecResult, ImageRef, Labels, Provider, Sandbox, SandboxSpec, Secret};

/// How long a sandbox may take to reach a state (running, suspended).
const WAIT: Duration = Duration::from_secs(180);
/// How often its state is read meanwhile.
const POLL: Duration = Duration::from_secs(1);
const EXEC_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const FILE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Larger input goes to `exec` through a file, not the command line.
const INLINE_STDIN: usize = 48 << 10;
/// Pages followed in one list.
const MAX_PAGES: usize = 100;

/// The `aca` provider: one sandbox group of the pack.
pub struct Aca {
    rest: Rest,
    /// `/subscriptions/{sub}/resourceGroups/{rg}/sandboxGroups/{group}`.
    group: String,
    poll: Duration,
    wait: Duration,
}

impl std::fmt::Debug for Aca {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the group's names or the endpoint: they are the pack's.
        f.debug_struct("Aca").field("rest", &self.rest).finish_non_exhaustive()
    }
}

impl Aca {
    /// The provider for the pack's `aca` settings, with tokens from `token`.
    pub fn new(pack: &Pack, token: Box<dyn TokenSource>) -> Result<Aca> {
        let aca = &pack.aca;
        let group = format!(
            "/subscriptions/{}/resourceGroups/{}/sandboxGroups/{}",
            model::segment("aca.subscription", &aca.subscription)?,
            model::segment("aca.resource_group", &aca.resource_group)?,
            model::segment("aca.sandbox_group", &aca.sandbox_group)?
        );
        let api_version = Some(aca.api_version.trim()).filter(|v| !v.is_empty()).unwrap_or(DEFAULT_API_VERSION);
        let hidden = vec![(aca.subscription.clone(), "<subscription>"), (aca.resource_group.clone(), "<resource-group>"), (aca.sandbox_group.clone(), "<sandbox-group>")];
        let rest = Rest::new(rest::endpoint(aca)?, api_version, Cache::new(token), hidden)?;
        Ok(Aca { rest, group, poll: POLL, wait: WAIT })
    }

    /// The provider with the pack's token command (`auth.command`, else the Azure CLI).
    pub fn from_pack(pack: &Pack) -> Result<Aca> {
        Aca::new(pack, Box::new(CommandSource::of(&pack.auth)))
    }

    /// Fetches a token in the background now, so the first call does not wait for the command.
    pub fn warm(&self) {
        self.rest.token.warm();
    }

    fn sandbox_path(&self, id: &str) -> Result<String> {
        model::segment("the sandbox id", id).map_err(|_| anyhow!("not a sandbox id: {id:?}"))?;
        Ok(format!("{}/sandboxes/{id}", self.group))
    }

    fn url(&self, path: &str) -> String {
        self.rest.url(path, &[])
    }

    /// Every item of a list, following `nextLink` on this endpoint only.
    fn all(&self, op: &str, mut url: String) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        for _ in 0..MAX_PAGES {
            let reply = self.rest.send(Call::new(op, Method::GET, url))?;
            let (page, next) = model::page(&reply.json(op)?).map_err(|e| anyhow!("{op}: {e}"))?;
            items.extend(page);
            match next {
                None => return Ok(items),
                Some(link) => url = self.rest.next_url(&link).ok_or_else(|| anyhow!("{op}: the next page is not on the sandbox service's endpoint (not followed)"))?,
            }
        }
        bail!("{op}: more than {MAX_PAGES} pages")
    }

    /// Reads the sandbox until `done` holds. `Failed` or `Deleting`, a
    /// missing sandbox or the time limit end the wait with an error.
    fn wait_until(&self, op: &str, id: &str, what: &str, done: fn(&Sandbox) -> bool) -> Result<Sandbox> {
        let deadline = Instant::now() + self.wait;
        loop {
            let Some(sandbox) = self.get(id)? else { bail!("{op}: sandbox {id} is gone") };
            if done(&sandbox) {
                return Ok(sandbox);
            }
            if matches!(sandbox.state.to_ascii_lowercase().as_str(), "failed" | "deleting") {
                bail!("{op}: sandbox {id} is {} instead of {what}", sandbox.state);
            }
            if Instant::now() >= deadline {
                bail!("{op}: sandbox {id} is still {} after {} s (waiting for {what})", if sandbox.state.is_empty() { "in no state" } else { &sandbox.state }, self.wait.as_secs());
            }
            std::thread::sleep(self.poll);
        }
    }

    /// After a 409 on `stop`/`resume`: done already (`Ok(true)`), on its way
    /// (`Ok(false)`: wait), or the conflict is the answer.
    fn after_conflict(&self, op: &str, id: &str, reply: &Reply, done: fn(&Sandbox) -> bool, on_the_way: &[&str]) -> Result<bool> {
        match self.get(id)? {
            Some(sandbox) if done(&sandbox) => Ok(true),
            Some(sandbox) if on_the_way.iter().any(|s| sandbox.state.eq_ignore_ascii_case(s)) => Ok(false),
            _ => Err(self.rest.failure(op, reply, &[])),
        }
    }
}

impl Provider for Aca {
    fn name(&self) -> &'static str {
        "aca"
    }

    fn create(&self, spec: &SandboxSpec) -> Result<Sandbox> {
        let op = "create a sandbox";
        let body = model::create_body(spec)?;
        let reply = self.rest.send(Call::new(op, Method::PUT, self.url(&format!("{}/sandboxes", self.group))).json(&body).once())?;
        let created = model::sandbox(&reply.json(op)?).map_err(|e| anyhow!("{op}: {e}"))?;
        self.sandbox_path(&created.id).map_err(|e| anyhow!("{op}: {e}"))?;
        if created.running() {
            return Ok(created);
        }
        let op = format!("create sandbox {}", created.id);
        self.wait_until(&op, &created.id, "Running", Sandbox::running).or_else(|error| {
            // Nobody else knows this sandbox: never leave it running.
            let outcome = match self.delete(&created.id) {
                Ok(()) => "it was deleted",
                Err(_) => "deleting it failed too: it may still exist",
            };
            bail!("{error:#} ({outcome})")
        })
    }

    fn get(&self, id: &str) -> Result<Option<Sandbox>> {
        let path = self.sandbox_path(id)?;
        let op = format!("get sandbox {id}");
        let reply = self.rest.send(Call::new(&op, Method::GET, self.url(&path)).accept(&[404]))?;
        if reply.status == 404 {
            return Ok(None);
        }
        model::sandbox(&reply.json(&op)?).map(Some).map_err(|e| anyhow!("{op}: {e}"))
    }

    fn list(&self, labels: &Labels) -> Result<Vec<Sandbox>> {
        let op = "list sandboxes";
        let filter = model::label_filter(labels)?;
        let query: Vec<(&str, &str)> = if filter.is_empty() { Vec::new() } else { vec![("labels", &filter)] };
        let mut out = Vec::new();
        for item in self.all(op, self.rest.url(&format!("{}/sandboxes", self.group), &query))? {
            let sandbox = model::sandbox(&item).map_err(|e| anyhow!("{op}: {e}"))?;
            // The service filters; a sandbox listed with labels must match here too.
            if sandbox.labels.is_empty() || labels.iter().all(|(k, v)| sandbox.labels.get(k) == Some(v)) {
                out.push(sandbox);
            }
        }
        Ok(out)
    }

    fn exec(&self, id: &str, argv: &[String], stdin: &[u8]) -> Result<ExecResult> {
        let path = self.sandbox_path(id)?;
        let op = format!("exec in {id}");
        let file = (stdin.len() > INLINE_STDIN).then(|| format!("/tmp/atelier-stdin-{}", uuid::Uuid::new_v4().simple()));
        let input = match &file {
            Some(file) => Stdin::File(file),
            None if stdin.is_empty() => Stdin::None,
            None => Stdin::Inline(stdin),
        };
        let command = model::shell_command(argv, input)?;
        if let Some(file) = &file {
            self.put_file(id, file, stdin)?;
        }
        let body = model::exec_body(&command);
        let reply = self.rest.send(Call::new(&op, Method::POST, self.url(&format!("{path}/executeShellCommand"))).json(&body).once().timeout(EXEC_TIMEOUT))?;
        model::exec_result(&reply.json(&op)?).map_err(|e| anyhow!("{op}: {e}"))
    }

    fn put_file(&self, id: &str, path: &str, bytes: &[u8]) -> Result<()> {
        let sandbox = self.sandbox_path(id)?;
        let op = format!("write a file in {id}");
        let url = self.rest.url(&format!("{sandbox}/files"), &[("path", model::guest_path(path)?)]);
        self.rest.send(Call::new(&op, Method::PUT, url).payload(Payload::Bytes(bytes)).timeout(FILE_TIMEOUT))?;
        Ok(())
    }

    fn get_file(&self, id: &str, path: &str, max: u64) -> Result<Vec<u8>> {
        let sandbox = self.sandbox_path(id)?;
        let op = format!("read a file in {id}");
        let url = self.rest.url(&format!("{sandbox}/files"), &[("path", model::guest_path(path)?)]);
        Ok(self.rest.send(Call::new(&op, Method::GET, url).max(max).timeout(FILE_TIMEOUT))?.body)
    }

    fn suspend(&self, id: &str) -> Result<()> {
        let path = self.sandbox_path(id)?;
        let op = format!("suspend {id}");
        let reply = self.rest.send(Call::new(&op, Method::POST, self.url(&format!("{path}/stop"))).payload(Payload::Empty).accept(&[409]))?;
        if reply.status == 409 && self.after_conflict(&op, id, &reply, Sandbox::suspended, &["Stopping", "Suspending"])? {
            return Ok(());
        }
        self.wait_until(&op, id, "Suspended", Sandbox::suspended).map(drop)
    }

    fn resume(&self, id: &str) -> Result<()> {
        let path = self.sandbox_path(id)?;
        let op = format!("resume {id}");
        let reply = self.rest.send(Call::new(&op, Method::POST, self.url(&format!("{path}/resume"))).payload(Payload::Empty).accept(&[409]))?;
        if reply.status == 409 && self.after_conflict(&op, id, &reply, Sandbox::running, &["Resuming", "Starting"])? {
            return Ok(());
        }
        self.wait_until(&op, id, "Running", Sandbox::running).map(drop)
    }

    fn delete(&self, id: &str) -> Result<()> {
        let path = self.sandbox_path(id)?;
        let op = format!("delete sandbox {id}");
        self.rest.send(Call::new(&op, Method::DELETE, self.url(&path)).accept(&[404]))?;
        Ok(())
    }

    fn set_egress(&self, id: &str, policy: &EgressPolicy) -> Result<()> {
        let path = self.sandbox_path(id)?;
        let op = format!("set the egress policy of {id}");
        let body = model::egress_body(policy)?;
        self.rest.send(Call::new(&op, Method::POST, self.url(&format!("{path}/egresspolicy"))).json(&body))?;
        Ok(())
    }

    fn egress_decisions(&self, id: &str) -> Result<Vec<EgressDecision>> {
        let path = self.sandbox_path(id)?;
        let op = format!("read the egress decisions of {id}");
        Ok(self.all(&op, self.url(&format!("{path}/egress-decisions")))?.iter().map(model::decision).collect())
    }

    fn image_build(&self, base: &str, name: &str) -> Result<ImageRef> {
        let op = format!("build image {name}");
        let body = model::image_build_body(base, name)?;
        let reply = self.rest.send(Call::new(&op, Method::PUT, self.url(&format!("{}/diskimages", self.group))).json(&body).once())?;
        model::image(&reply.json(&op)?).map_err(|e| anyhow!("{op}: {e}"))
    }

    fn image_commit(&self, id: &str, name: &str) -> Result<ImageRef> {
        let path = self.sandbox_path(id)?;
        let op = format!("commit {id} as image {name}");
        let body = model::commit_body(name)?;
        let reply = self.rest.send(Call::new(&op, Method::POST, self.url(&format!("{path}/commit"))).json(&body).once().timeout(FILE_TIMEOUT))?;
        model::image(&reply.json(&op)?).map_err(|e| anyhow!("{op}: {e}"))
    }

    fn image_delete(&self, image: &ImageRef) -> Result<()> {
        let id = model::segment("the image id", &image.id).map_err(|_| anyhow!("not an image id: {:?}", image.id))?;
        let op = format!("delete image {id}");
        self.rest.send(Call::new(&op, Method::DELETE, self.url(&format!("{}/diskimages/{id}", self.group))).accept(&[404]))?;
        Ok(())
    }

    fn images(&self) -> Result<Vec<ImageRef>> {
        let op = "list images";
        self.all(op, self.url(&format!("{}/diskimages", self.group)))?.iter().map(|item| model::image(item).map_err(|e| anyhow!("{op}: {e}"))).collect()
    }

    fn secret_set(&self, name: &str, key: &str, value: &Secret) -> Result<()> {
        let name = model::segment("the secret's name", name).map_err(|_| anyhow!("not a secret name: {name:?}"))?;
        let op = format!("set secret {name}");
        let body = model::secret_body(key, value)?;
        let shown = std::str::from_utf8(value.expose()).unwrap_or_default();
        self.rest.send(Call::new(&op, Method::PUT, self.url(&format!("{}/secrets/{name}", self.group))).payload(Payload::Secret(&body)).hide(shown))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use base64::Engine as _;
    use serde_json::json;

    use super::rest::standin::{GROUP, Reply, StandIn};
    use super::*;
    use crate::vm::provider::{EgressRule, HeaderFromSecret};

    const TOKEN: &str = "tok-0123456789abcdef";
    const SUBSCRIPTION: &str = "00000000-0000-0000-0000-000000000000";

    fn pack(url: &str) -> Pack {
        let aca = crate::vm::pack::Aca { subscription: SUBSCRIPTION.into(), resource_group: "contoso-rg".into(), sandbox_group: "contoso-sandboxes".into(), region: "eastus2".into(), api_version: DEFAULT_API_VERSION.into(), endpoint: Some(url.into()) };
        Pack { aca, ..Default::default() }
    }

    /// The provider on `url` with a counted token source, fast polls and retries.
    fn provider(url: &str) -> (Aca, Arc<AtomicU64>) {
        let calls = Arc::new(AtomicU64::new(0));
        let counter = calls.clone();
        let source = move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Token::new(TOKEN, canvas_common::paths::now_ms() + 3_600_000))
        };
        let mut aca = Aca::new(&pack(url), Box::new(source)).unwrap();
        aca.poll = Duration::from_millis(5);
        aca.wait = Duration::from_secs(5);
        aca.rest.retry.base = Duration::from_millis(5);
        (aca, calls)
    }

    /// A stand-in answering `METHOD path` (the group written `G`) with its
    /// replies in turn, the last one again once the others are used.
    fn scripted(routes: Vec<(&str, Vec<Reply>)>) -> StandIn {
        let routes: Mutex<BTreeMap<String, VecDeque<Reply>>> = Mutex::new(routes.into_iter().map(|(k, v)| (k.to_owned(), v.into())).collect());
        StandIn::start(move |req| {
            let mut routes = routes.lock().unwrap();
            match routes.get_mut(&req.line()) {
                Some(queue) if queue.len() > 1 => queue.pop_front().unwrap(),
                Some(queue) if !queue.is_empty() => queue[0].clone(),
                _ => Reply::text(418, &format!("no route for {}", req.line())),
            }
        })
    }

    fn sandbox(id: &str, state: &str) -> Reply {
        Reply::json(200, json!({ "id": id, "state": state, "labels": { "atelier.box": "b1" } }))
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn create_sends_the_sdk_body_and_waits_until_running() {
        let standin = scripted(vec![
            ("PUT G/sandboxes", vec![Reply::json(201, json!({ "id": "sbx-1", "state": "Creating" }))]),
            ("GET G/sandboxes/sbx-1", vec![sandbox("sbx-1", "Creating"), sandbox("sbx-1", "Running")]),
        ]);
        let (aca, calls) = provider(&standin.url);
        let spec = SandboxSpec {
            image: Some("img-7".into()),
            cpu_milli: 2000,
            memory_mi: 4096,
            disk_gi: 20,
            labels: [("atelier.box".into(), "b1".into()), ("atelier.hub".into(), "h1".into())].into(),
            env: vec![("TZ".into(), "UTC".into())],
            egress: Some(EgressPolicy { default_action: "Deny".into(), rules: vec![EgressRule { name: "git".into(), host: "example.com".into(), action: "Allow".into(), ..Default::default() }] }),
        };
        let created = aca.create(&spec).unwrap();
        assert_eq!((created.id.as_str(), created.running(), created.labels["atelier.box"].as_str()), ("sbx-1", true, "b1"));
        assert_eq!(standin.lines(), ["PUT G/sandboxes", "GET G/sandboxes/sbx-1", "GET G/sandboxes/sbx-1"]);
        let put = &standin.requests()[0];
        assert_eq!(
            put.json(),
            json!({
                "sourcesRef": { "diskImage": { "id": "img-7" } },
                "resources": { "cpu": "2000m", "memory": "4096Mi", "disk": "20Gi" },
                "lifecycle": { "autoSuspendPolicy": { "enabled": false, "interval": 300, "mode": "Memory" } },
                "labels": { "atelier.box": "b1", "atelier.hub": "h1" },
                "environment": { "TZ": "UTC" },
                "egressPolicy": { "defaultAction": "Deny", "rules": [{ "name": "git", "match": { "host": "example.com" }, "action": { "type": "Allow" } }] },
            })
        );
        assert!(!String::from_utf8_lossy(&put.body).contains("autoDelete"));
        assert_eq!(put.header("content-type"), Some("application/json"));
        for request in standin.requests() {
            assert_eq!(request.param("api-version"), Some(DEFAULT_API_VERSION));
            assert_eq!(request.header("authorization"), Some(format!("Bearer {TOKEN}").as_str()));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one token for every request");
        assert_eq!(aca.name(), "aca");
    }

    #[test]
    fn a_sandbox_that_fails_to_start_is_deleted_and_the_error_says_so() {
        let standin = scripted(vec![
            ("PUT G/sandboxes", vec![Reply::json(200, json!({ "id": "sbx-2", "state": "Creating" }))]),
            ("GET G/sandboxes/sbx-2", vec![sandbox("sbx-2", "Failed")]),
            ("DELETE G/sandboxes/sbx-2", vec![Reply::status(202)]),
        ]);
        let (aca, _) = provider(&standin.url);
        let error = aca.create(&SandboxSpec { cpu_milli: 500, memory_mi: 1024, ..Default::default() }).unwrap_err().to_string();
        assert!(error.contains("sbx-2 is Failed instead of Running") && error.ends_with("(it was deleted)"), "{error}");
        assert_eq!(standin.lines().last().map(String::as_str), Some("DELETE G/sandboxes/sbx-2"));
        // An answer whose id is not a name is refused before anything else is sent.
        let standin = scripted(vec![("PUT G/sandboxes", vec![Reply::json(200, json!({ "id": "bad?id", "state": "Creating" }))])]);
        let (aca, _) = provider(&standin.url);
        assert!(aca.create(&SandboxSpec { cpu_milli: 500, memory_mi: 1024, ..Default::default() }).unwrap_err().to_string().contains("not a sandbox id"));
        assert_eq!(standin.lines().len(), 1);
    }

    #[test]
    fn list_filters_by_labels_and_follows_next_links_on_the_same_endpoint_only() {
        let standin = StandIn::start(|req| {
            let item = |id: &str, hub: &str| json!({ "id": id, "state": "Running", "labels": { "atelier.hub": hub, "atelier.role": "pool" } });
            if req.param("page") == Some("2") {
                Reply::json(200, json!({ "value": [item("sbx-3", "h1"), item("sbx-4", "h2")] }))
            } else {
                let next = format!("http://{}{GROUP}/sandboxes?api-version={DEFAULT_API_VERSION}&page=2", req.header("host").unwrap());
                Reply::json(200, json!({ "value": [item("sbx-1", "h1")], "nextLink": next }))
            }
        });
        let (aca, _) = provider(&standin.url);
        let labels: Labels = [("atelier.hub".into(), "h1".into()), ("atelier.role".into(), "pool".into())].into();
        let found: Vec<String> = aca.list(&labels).unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(found, ["sbx-1", "sbx-3"], "sbx-4 does not carry the labels");
        let requests = standin.requests();
        assert_eq!(standin.lines(), ["GET G/sandboxes", "GET G/sandboxes"]);
        assert_eq!(requests[0].param("labels"), Some("atelier.hub=h1,atelier.role=pool"));
        assert!(requests.iter().all(|r| r.header("authorization").is_some()));
        // A next page elsewhere is never asked for (the token would go with it).
        let elsewhere = StandIn::start(|_| Reply::json(200, json!({ "value": [], "nextLink": "https://elsewhere.example.com/subscriptions/x?page=2" })));
        let (aca, _) = provider(&elsewhere.url);
        assert!(aca.list(&Labels::new()).unwrap_err().to_string().contains("not followed"));
        assert_eq!(elsewhere.requests().len(), 1);
        assert_eq!(elsewhere.requests()[0].param("labels"), None);
        // A bare list is an answer too; a label with a separator is refused before any request.
        let bare = StandIn::start(|_| Reply::json(200, json!([{ "id": "sbx-9", "state": "Suspended" }])));
        let (aca, _) = provider(&bare.url);
        assert_eq!(aca.list(&Labels::new()).unwrap()[0].id, "sbx-9");
        assert!(aca.list(&[("a".into(), "x,y".into())].into()).is_err());
        assert_eq!(bare.requests().len(), 1);
    }

    #[test]
    fn exec_quotes_every_word_so_the_shell_reads_none() {
        let standin = StandIn::start(|_| Reply::json(200, json!({ "exitCode": 3, "stdout": "out", "stderr": "err" })));
        let (aca, _) = provider(&standin.url);
        let argv = strings(&["printf", "[%s]\\n", "a b", "it's", "$(echo pwned)", "x; echo pwned", "\"q\"", "`echo pwned`", "*", ""]);
        let result = aca.exec("sbx-1", &argv, b"").unwrap();
        assert_eq!((result.code, result.stdout.as_slice(), result.stderr.as_str()), (3, b"out".as_slice(), "err"));
        let request = &standin.requests()[0];
        assert_eq!(request.line(), "POST G/sandboxes/sbx-1/executeShellCommand");
        let body = request.json();
        assert_eq!(body.as_object().map(|o| o.len()), Some(1), "only the command: {body}");
        let command = body["command"].as_str().unwrap();
        assert_eq!(command, r#"'printf' '[%s]\n' 'a b' 'it'\''s' '$(echo pwned)' 'x; echo pwned' '"q"' '`echo pwned`' '*' '' < /dev/null"#);
        #[cfg(unix)]
        {
            let out = std::process::Command::new("/bin/sh").args(["-c", command]).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), "[a b]\n[it's]\n[$(echo pwned)]\n[x; echo pwned]\n[\"q\"]\n[`echo pwned`]\n[*]\n[]\n");
        }
    }

    #[test]
    fn exec_input_goes_through_base64_or_a_file() {
        let standin = scripted(vec![
            ("POST G/sandboxes/sbx-1/executeShellCommand", vec![Reply::json(200, json!({ "exitCode": 0, "stdout": "", "stderr": "" }))]),
            ("PUT G/sandboxes/sbx-1/files", vec![Reply::status(201)]),
        ]);
        let (aca, _) = provider(&standin.url);
        let stdin = b"hello 'world'\n$(echo pwned) \x00\xff";
        aca.exec("sbx-1", &strings(&["cat"]), stdin).unwrap();
        let command = standin.requests()[0].json()["command"].as_str().unwrap().to_owned();
        let b64 = base64::engine::general_purpose::STANDARD.encode(stdin);
        assert_eq!(command, format!("printf %s '{b64}' | base64 -d | 'cat'"));
        #[cfg(unix)]
        {
            let works = std::process::Command::new("/bin/sh").args(["-c", "printf %s aGk= | base64 -d"]).output().is_ok_and(|o| o.stdout == b"hi");
            if works {
                let out = std::process::Command::new("/bin/sh").args(["-c", &command]).output().unwrap();
                assert_eq!(out.stdout, stdin);
            }
        }
        // Larger input is written to a file first, then read and removed by the command.
        let big = vec![b'x'; INLINE_STDIN + 1];
        aca.exec("sbx-1", &strings(&["wc", "-c"]), &big).unwrap();
        let requests = standin.requests();
        assert_eq!(requests[1].line(), "PUT G/sandboxes/sbx-1/files");
        let file = requests[1].param("path").unwrap().to_owned();
        assert!(file.starts_with("/tmp/atelier-stdin-") && requests[1].body == big);
        assert_eq!(requests[1].header("content-type"), Some("application/octet-stream"));
        assert_eq!(requests[2].json()["command"], format!("'wc' '-c' < '{file}'; code=$?; rm -f '{file}'; exit $code"));
    }

    #[test]
    fn throttling_waits_for_retry_after_even_for_calls_that_run_once() {
        let turn = Arc::new(AtomicU64::new(0));
        let seen = turn.clone();
        let standin = StandIn::start(move |req| {
            if seen.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
                Reply::json(429, json!({ "error": { "code": "TooManyRequests", "message": "slow down" } })).header("Retry-After", if req.method == "GET" { "1" } else { "0" })
            } else if req.method == "GET" {
                sandbox("sbx-1", "Running")
            } else {
                Reply::json(200, json!({ "exitCode": 0, "stdout": "", "stderr": "" }))
            }
        });
        let (aca, _) = provider(&standin.url);
        assert!(aca.get("sbx-1").unwrap().unwrap().running());
        let requests = standin.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].at.duration_since(requests[0].at) >= Duration::from_secs(1), "Retry-After: 1 is honoured");
        aca.exec("sbx-1", &strings(&["true"]), b"").unwrap();
        assert_eq!(standin.requests().len(), 4, "a 429 means the service did not run it: tried again");
        // A wait longer than the call's budget is not made.
        let slow = StandIn::start(|_| Reply::json(429, json!({ "error": { "code": "TooManyRequests" } })).header("Retry-After", "120"));
        let (aca, _) = provider(&slow.url);
        let started = Instant::now();
        assert!(aca.get("sbx-1").unwrap_err().to_string().contains("HTTP 429 TooManyRequests"));
        assert!(started.elapsed() < Duration::from_secs(5) && slow.requests().len() == 1);
    }

    #[test]
    fn server_errors_are_retried_for_reads_and_settings_but_never_for_creates_or_commands() {
        let turn = Arc::new(AtomicU64::new(0));
        let seen = turn.clone();
        let burst = StandIn::start(move |_| match seen.fetch_add(1, Ordering::SeqCst) {
            0 => Reply::status(503),
            1 => Reply::json(500, json!({ "error": { "code": "InternalError", "message": "oops" } })),
            2 => Reply::text(502, "<html>bad gateway</html>"),
            _ => sandbox("sbx-1", "Running"),
        });
        let (aca, _) = provider(&burst.url);
        assert!(aca.get("sbx-1").unwrap().is_some());
        assert_eq!(burst.requests().len(), 4);
        let down = StandIn::start(|_| Reply::json(503, json!({ "error": { "code": "ServiceUnavailable", "message": "try later" } })));
        let (aca, _) = provider(&down.url);
        let error = aca.get("sbx-1").unwrap_err().to_string();
        assert_eq!(error, "get sandbox sbx-1: HTTP 503 ServiceUnavailable: try later");
        assert_eq!(down.requests().len(), 4, "at most four tries");
        assert!(aca.exec("sbx-1", &strings(&["git", "push"]), b"").unwrap_err().to_string().contains("HTTP 503"));
        assert!(aca.create(&SandboxSpec { cpu_milli: 500, memory_mi: 1024, ..Default::default() }).is_err());
        assert!(aca.image_commit("sbx-1", "n").is_err());
        assert_eq!(down.requests().len(), 7, "exec, create and commit were sent once each");
    }

    #[test]
    fn resume_and_suspend_wait_for_the_state_and_a_409_on_a_running_sandbox_is_success() {
        let conflict = || Reply::json(409, json!({ "error": { "code": "Conflict", "message": "The sandbox is running" } }));
        let standin = scripted(vec![("POST G/sandboxes/sbx-1/resume", vec![conflict()]), ("GET G/sandboxes/sbx-1", vec![sandbox("sbx-1", "Running")])]);
        let (aca, _) = provider(&standin.url);
        aca.resume("sbx-1").unwrap();
        assert_eq!(standin.lines(), ["POST G/sandboxes/sbx-1/resume", "GET G/sandboxes/sbx-1"]);
        assert_eq!(standin.requests()[0].header("content-length"), Some("0"));
        let standin = scripted(vec![("POST G/sandboxes/sbx-1/resume", vec![Reply::status(202)]), ("GET G/sandboxes/sbx-1", vec![sandbox("sbx-1", "Resuming"), sandbox("sbx-1", "Running")])]);
        let (aca, _) = provider(&standin.url);
        aca.resume("sbx-1").unwrap();
        assert_eq!(standin.lines().len(), 3);
        let standin = scripted(vec![("POST G/sandboxes/sbx-1/resume", vec![conflict()]), ("GET G/sandboxes/sbx-1", vec![sandbox("sbx-1", "Deleting")])]);
        let (aca, _) = provider(&standin.url);
        assert_eq!(aca.resume("sbx-1").unwrap_err().to_string(), "resume sbx-1: HTTP 409 Conflict: The sandbox is running");
        let standin = scripted(vec![("POST G/sandboxes/sbx-1/stop", vec![Reply::status(202)]), ("GET G/sandboxes/sbx-1", vec![sandbox("sbx-1", "Stopping"), sandbox("sbx-1", "Suspended")])]);
        let (aca, _) = provider(&standin.url);
        aca.suspend("sbx-1").unwrap();
        assert_eq!(standin.lines(), ["POST G/sandboxes/sbx-1/stop", "GET G/sandboxes/sbx-1", "GET G/sandboxes/sbx-1"]);
        let standin = scripted(vec![("POST G/sandboxes/sbx-1/stop", vec![Reply::status(202)]), ("GET G/sandboxes/sbx-1", vec![sandbox("sbx-1", "Running")])]);
        let (mut aca, _) = provider(&standin.url);
        aca.wait = Duration::from_millis(50);
        assert!(aca.suspend("sbx-1").unwrap_err().to_string().contains("still Running after 0 s"));
        let standin = scripted(vec![("POST G/sandboxes/sbx-1/stop", vec![Reply::status(200)]), ("GET G/sandboxes/sbx-1", vec![Reply::status(404)])]);
        let (aca, _) = provider(&standin.url);
        assert_eq!(aca.suspend("sbx-1").unwrap_err().to_string(), "suspend sbx-1: sandbox sbx-1 is gone");
    }

    #[test]
    fn delete_and_get_take_a_missing_sandbox_calmly() {
        let standin = scripted(vec![("DELETE G/sandboxes/gone", vec![Reply::status(404)]), ("DELETE G/sandboxes/sbx-1", vec![Reply::status(204)]), ("GET G/sandboxes/gone", vec![Reply::status(404)]), ("DELETE G/sandboxes/locked", vec![Reply::json(403, json!({ "error": { "code": "AuthorizationFailed", "message": "no" } }))])]);
        let (aca, _) = provider(&standin.url);
        aca.delete("gone").unwrap();
        aca.delete("sbx-1").unwrap();
        assert_eq!(aca.get("gone").unwrap(), None);
        let error = aca.delete("locked").unwrap_err().to_string();
        assert!(error.starts_with("delete sandbox locked: HTTP 403 AuthorizationFailed: no (does your account"), "{error}");
        assert_eq!(standin.requests().len(), 4, "a 403 is not tried again");
    }

    #[test]
    fn malformed_answers_are_errors_that_do_not_repeat_them() {
        let standin = scripted(vec![
            ("GET G/sandboxes/sbx-1", vec![Reply::text(200, "{\"id\": \"sbx-1\", private-detail")]),
            ("POST G/sandboxes/sbx-1/executeShellCommand", vec![Reply::json(200, json!({ "stdout": "x" }))]),
            ("GET G/sandboxes", vec![Reply::json(200, json!({ "value": "private-detail" }))]),
        ]);
        let (aca, _) = provider(&standin.url);
        let error = aca.get("sbx-1").unwrap_err().to_string();
        assert_eq!(error, "get sandbox sbx-1: the answer is not JSON");
        assert_eq!(aca.exec("sbx-1", &strings(&["true"]), b"").unwrap_err().to_string(), "exec in sbx-1: the answer has no exitCode");
        let error = aca.list(&Labels::new()).unwrap_err().to_string();
        assert!(error.starts_with("list sandboxes: ") && !error.contains("private-detail"), "{error}");
    }

    #[test]
    fn files_go_raw_with_the_path_in_the_query_and_reads_are_bounded() {
        let standin = scripted(vec![("PUT G/sandboxes/sbx-1/files", vec![Reply::status(201)]), ("GET G/sandboxes/sbx-1/files", vec![Reply::text(200, "0123456789")])]);
        let (aca, _) = provider(&standin.url);
        aca.put_file("sbx-1", "/home/agent/a b&c=d.json", b"{\"x\":1}").unwrap();
        assert_eq!(aca.get_file("sbx-1", "/home/agent/notes.md", 10).unwrap(), b"0123456789");
        assert!(aca.get_file("sbx-1", "/home/agent/notes.md", 9).unwrap_err().to_string().contains("larger than 9 bytes"));
        let requests = standin.requests();
        assert_eq!((requests[0].param("path"), requests[0].body.as_slice()), (Some("/home/agent/a b&c=d.json"), b"{\"x\":1}".as_slice()));
        assert_eq!(requests[1].param("path"), Some("/home/agent/notes.md"));
        for bad in ["relative.txt", "/a/../../etc/passwd"] {
            assert!(aca.put_file("sbx-1", bad, b"x").is_err() && aca.get_file("sbx-1", bad, 10).is_err());
        }
        assert_eq!(standin.requests().len(), 3);
    }

    #[test]
    fn egress_policies_and_decisions() {
        let standin = scripted(vec![
            ("POST G/sandboxes/sbx-1/egresspolicy", vec![Reply::status(200)]),
            ("GET G/sandboxes/sbx-1/egress-decisions", vec![Reply::json(200, json!({ "value": [{ "timestamp": "2026-11-09T02:00:00Z", "host": "evil.example.com", "method": "POST", "path": "/x", "action": "Deny" }, { "timestamp": 1_794_189_601, "host": "example.com", "method": "GET", "path": "/", "allowed": true }] }))]),
        ]);
        let (aca, _) = provider(&standin.url);
        let policy = EgressPolicy {
            default_action: "deny".into(),
            rules: vec![EgressRule { name: "forge-api".into(), host: "api.example.com".into(), path: Some("/repos/*".into()), methods: vec!["GET".into()], action: "Transform".into(), headers: vec![HeaderFromSecret { name: "Authorization".into(), secret: "forge-token".into(), key: "value".into(), format: "Bearer {}".into() }] }],
        };
        aca.set_egress("sbx-1", &policy).unwrap();
        assert_eq!(
            standin.requests()[0].json(),
            json!({ "defaultAction": "Deny", "rules": [{
                "name": "forge-api",
                "match": { "host": "api.example.com", "path": "/repos/*", "methods": ["GET"] },
                "action": { "type": "Transform", "headers": [{ "operation": "Set", "name": "Authorization", "valueRef": { "secretRef": { "secretId": "forge-token", "secretKey": "value", "format": "Bearer {}" } } }] },
            }] })
        );
        let decisions = aca.egress_decisions("sbx-1").unwrap();
        assert_eq!(decisions.iter().map(|d| (d.ms, d.host.as_str(), d.allowed)).collect::<Vec<_>>(), [(1_794_189_600_000, "evil.example.com", false), (1_794_189_601_000, "example.com", true)]);
        let mut bad = policy;
        bad.rules[0].action = "rewrite".into();
        assert!(aca.set_egress("sbx-1", &bad).is_err());
        assert_eq!(standin.requests().len(), 2);
    }

    #[test]
    fn images_are_built_committed_listed_and_deleted() {
        let image = json!({ "id": "img-1", "labels": { "name": "contoso-app-1" }, "createdAt": "2026-11-09T02:00:00Z" });
        let standin = scripted(vec![
            ("PUT G/diskimages", vec![Reply::json(201, image.clone())]),
            ("POST G/sandboxes/sbx-1/commit", vec![Reply::json(200, json!({ "diskImage": { "id": "img-2", "labels": { "name": "contoso-app-2" } } }))]),
            ("GET G/diskimages", vec![Reply::json(200, json!({ "value": [image] }))]),
            ("DELETE G/diskimages/img-1", vec![Reply::status(404)]),
        ]);
        let (aca, _) = provider(&standin.url);
        let built = aca.image_build("ubuntu", "contoso-app-1").unwrap();
        assert_eq!(built, ImageRef { id: "img-1".into(), name: "contoso-app-1".into(), created_ms: 1_794_189_600_000 });
        let committed = aca.image_commit("sbx-1", "contoso-app-2").unwrap();
        assert_eq!((committed.id.as_str(), committed.name.as_str()), ("img-2", "contoso-app-2"));
        assert_eq!(aca.images().unwrap(), std::slice::from_ref(&built));
        aca.image_delete(&built).unwrap();
        let requests = standin.requests();
        assert_eq!(requests[0].json(), json!({ "image": { "base": "ubuntu" }, "labels": { "name": "contoso-app-1" } }));
        assert_eq!(requests[1].json(), json!({ "labels": { "name": "contoso-app-2" } }));
        assert!(aca.image_delete(&ImageRef { id: "x?y".into(), ..Default::default() }).is_err());
        assert_eq!(standin.requests().len(), 4);
    }

    #[test]
    fn secrets_go_only_in_their_body_and_never_into_errors() {
        let standin = scripted(vec![
            ("PUT G/secrets/forge-token", vec![Reply::status(200)]),
            ("PUT G/secrets/tunnel-token", vec![Reply::json(400, json!({ "error": { "code": "BadRequest", "message": "value s3cr3t-tunnel is too short" } }))]),
        ]);
        let (aca, _) = provider(&standin.url);
        aca.secret_set("forge-token", "value", &Secret::new(b"s3cr3t-\"value\"".to_vec())).unwrap();
        let request = &standin.requests()[0];
        assert_eq!(String::from_utf8_lossy(&request.body), r#"{"values":{"value":"s3cr3t-\"value\""}}"#);
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert!(request.query.iter().all(|(_, v)| !v.contains("s3cr3t")));
        let error = aca.secret_set("tunnel-token", "value", &Secret::new(b"s3cr3t-tunnel".to_vec())).unwrap_err().to_string();
        assert_eq!(error, "set secret tunnel-token: HTTP 400 BadRequest: value (hidden) is too short");
        assert!(aca.secret_set("../x", "value", &Secret::new(b"v".to_vec())).is_err());
        assert_eq!(standin.requests().len(), 2);
    }

    #[test]
    fn the_token_and_the_pack_values_never_show() {
        let standin = scripted(vec![
            ("GET G/sandboxes/sbx-1", vec![Reply::json(401, json!({ "error": { "code": "InvalidAuthenticationToken", "message": format!("The token {TOKEN} is not valid for this audience") } }))]),
            ("GET G/sandboxes", vec![Reply::json(404, json!({ "error": { "code": "ResourceGroupNotFound", "message": format!("Resource group 'Contoso-RG' of subscription {SUBSCRIPTION} (contoso-sandboxes) was not found") } }))]),
        ]);
        let (aca, calls) = provider(&standin.url);
        let mut errors = vec![aca.get("sbx-1").unwrap_err().to_string(), aca.list(&Labels::new()).unwrap_err().to_string()];
        assert!(errors[0].starts_with("get sandbox sbx-1: HTTP 401 InvalidAuthenticationToken: The token (hidden) is not valid"), "{}", errors[0]);
        assert_eq!(errors[1], "list sandboxes: HTTP 404 ResourceGroupNotFound: Resource group '<resource-group>' of subscription <subscription> (<sandbox-group>) was not found");
        for _ in 0..10 {
            let _ = aca.get("sbx-1");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one command run for many requests");
        // Nothing listens on this port: a network error, without the URL.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let (offline, _) = provider(&format!("http://{closed}"));
        errors.push(offline.get("sbx-1").unwrap_err().to_string());
        assert!(errors[2].starts_with("get sandbox sbx-1: cannot connect"), "{}", errors[2]);
        // A source that fails, and a provider's Debug.
        let failing = Aca::new(&pack(&standin.url), Box::new(|| -> Result<Token> { bail!("az: please run az login") })).unwrap();
        errors.push(failing.get("sbx-1").unwrap_err().to_string());
        assert_eq!(errors[3], "get sandbox sbx-1: no access token: az: please run az login");
        errors.push(format!("{aca:?}"));
        for error in &errors {
            assert!(!error.contains(TOKEN) && !error.contains(SUBSCRIPTION) && !error.to_lowercase().contains("contoso-rg") && !error.contains(&closed.to_string()), "{error}");
        }
    }

    #[test]
    fn names_are_checked_before_anything_is_sent() {
        let standin = StandIn::start(|_| Reply::status(200));
        let (aca, _) = provider(&standin.url);
        assert!(aca.get("../x").unwrap_err().to_string().contains("not a sandbox id"));
        for id in ["a/b", "a\\b", "a?b", "a#b", "", "a%2e"] {
            assert!(aca.exec(id, &strings(&["true"]), b"").is_err() && aca.delete(id).is_err() && aca.resume(id).is_err(), "{id:?}");
        }
        assert!(aca.exec("sbx-1", &[], b"").is_err());
        assert!(standin.requests().is_empty());
        let mut bad = pack(&standin.url);
        bad.aca.subscription = "a/b".into();
        assert_eq!(Aca::new(&bad, Box::new(|| Ok(Token::new("t", 0)))).unwrap_err().to_string(), "aca.subscription is not a valid name (no / \\ ? # % .. or control characters)");
        let mut plain = pack("http://sandbox.example.com");
        assert!(Aca::new(&plain, Box::new(|| Ok(Token::new("t", 0)))).unwrap_err().to_string().contains("https"));
        plain.aca.endpoint = None;
        plain.aca.api_version = "2026-02-01-preview&x=y".into();
        assert!(Aca::new(&plain, Box::new(|| Ok(Token::new("t", 0)))).is_err());
        plain.aca.api_version = String::new();
        assert_eq!(Aca::from_pack(&plain).unwrap().rest.url("/x", &[("labels", "a=b,c")]), "https://management.eastus2.azuredevcompute.io/x?api-version=2026-02-01-preview&labels=a%3Db%2Cc");
    }
}
