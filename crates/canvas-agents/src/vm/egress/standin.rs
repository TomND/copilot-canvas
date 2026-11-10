//! The egress stand-in of a `vm::local` sandbox (the lead's hidden `atelier
//! egress-standin --root ROOT --sandbox ID`): [`serve`](super::serve) on
//! `127.0.0.1:0`, with the sandbox's `egress.json` as its policy (read for
//! every request, so `Provider::set_egress` applies at once), the provider's
//! secrets (held outside every sandbox: [`Local::secret`]) as the values it
//! injects, and its decisions in the sandbox's `egress-decisions.jsonl` (what
//! `Provider::egress_decisions` reads). It runs outside the sandbox's
//! processes, so a suspend does not freeze it, and ends with the sandbox.
//!
//! What a local process cannot show is isolation: a guest process that
//! ignores `HTTP_PROXY` is not stopped here (the platform's egress does that).

use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};

use super::{Recorder, Secrets};
use crate::vm::local::{self, Local};
use crate::vm::provider::{EgressDecision, Provider, Secret};

struct LocalSecrets(Arc<Local>);

impl Secrets for LocalSecrets {
    fn value(&self, name: &str, key: &str) -> Option<Secret> {
        self.0.secret(name, key).ok().flatten()
    }
}

struct LocalRecorder {
    local: Arc<Local>,
    sandbox: String,
}

impl Recorder for LocalRecorder {
    fn record(&self, decision: EgressDecision) {
        let _ = self.local.log_egress(&self.sandbox, &decision);
    }
}

/// The environment that sends a guest's HTTP and HTTPS through the stand-in
/// on `port` (every spelling tools read); `no_proxy`: the hosts it reaches
/// directly (the hub's own address on loopback, say). It is always set, so
/// none of the hub's own proxy settings reaches the guest.
pub fn proxy_env(port: u16, no_proxy: &str) -> Vec<(String, String)> {
    let url = format!("http://127.0.0.1:{port}");
    let mut env: Vec<(String, String)> = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"].iter().map(|name| (name.to_string(), url.clone())).collect();
    env.extend(["NO_PROXY", "no_proxy"].iter().map(|name| (name.to_string(), no_proxy.to_owned())));
    env
}

/// A stand-in serving one sandbox from a thread of this process.
pub struct Standin {
    port: u16,
    local: Arc<Local>,
    sandbox: String,
    thread: JoinHandle<Result<()>>,
}

impl Standin {
    /// Listens on `127.0.0.1:0` for the sandbox `sandbox` of the `vm::local`
    /// provider on `root` (which must exist).
    pub fn start(root: &Path, sandbox: &str) -> Result<Standin> {
        ensure!(local::valid_id(sandbox), "not a sandbox id: {sandbox:?}");
        let local = Arc::new(Local::with_faults(root.to_path_buf(), "", 0)?);
        local.get(sandbox)?;
        let listener = canvas_common::tcp::listen_loopback("127.0.0.1:0").context("the egress stand-in cannot listen on loopback")?;
        let port = listener.local_addr()?.port();
        let (read, id) = (local.clone(), sandbox.to_owned());
        let secrets: Arc<dyn Secrets> = Arc::new(LocalSecrets(local.clone()));
        let recorder: Arc<dyn Recorder> = Arc::new(LocalRecorder { local: local.clone(), sandbox: sandbox.to_owned() });
        let thread = std::thread::Builder::new().name("egress-standin".into()).spawn(move || super::serve(listener, Arc::new(move || read.egress(&id).ok().flatten()), secrets, recorder))?;
        Ok(Standin { port, local, sandbox: sandbox.to_owned(), thread })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// [`proxy_env`] for this stand-in.
    pub fn env(&self, no_proxy: &str) -> Vec<(String, String)> {
        proxy_env(self.port, no_proxy)
    }

    /// Whether its sandbox is gone (deleted); a read that fails is not proof.
    fn sandbox_gone(&self) -> bool {
        matches!(Provider::get(&*self.local, &self.sandbox), Ok(None))
    }
}

/// `atelier egress-standin --root ROOT --sandbox ID`: serves the sandbox until
/// it is deleted, then returns. The port goes to `port_file` (a line) once it
/// listens.
pub fn standin(root: &Path, sandbox: &str, port_file: &Path) -> Result<()> {
    let standin = Standin::start(root, sandbox)?;
    canvas_common::paths::write_atomic(port_file, format!("{}\n", standin.port).as_bytes()).with_context(|| format!("cannot write {}", port_file.display()))?;
    loop {
        std::thread::sleep(Duration::from_millis(250));
        if standin.thread.is_finished() {
            return standin.thread.join().map_err(|_| anyhow!("the egress stand-in stopped"))?;
        }
        if standin.sandbox_gone() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::provider::{EgressPolicy, EgressRule, HeaderFromSecret, SandboxSpec};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::Mutex;
    use std::time::Instant;

    /// This test binary as a process of a sandbox: a tool that honours
    /// `HTTP_PROXY` sends `GET URL` through it (with an `Authorization` of its
    /// own), prints the answer on one line, then its whole environment.
    fn guest(url: &str) -> Vec<String> {
        let exe = std::env::current_exe().unwrap().display().to_string();
        [exe.as_str(), "vm::egress::standin::tests::guest_process", "--exact", "--ignored", "--nocapture", "--test-threads=1", &format!("guest:{url}")].map(str::to_owned).to_vec()
    }

    #[test]
    #[ignore = "a guest process the stand-in test runs in a sandbox"]
    fn guest_process() {
        let Some(url) = std::env::args().find_map(|a| a.strip_prefix("guest:").map(str::to_owned)) else { return };
        let proxy = std::env::var("HTTP_PROXY").unwrap();
        let mut stream = TcpStream::connect(proxy.trim_start_matches("http://").trim_end_matches('/')).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let host = url.trim_start_matches("http://").split('/').next().unwrap();
        write!(stream, "GET {url} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer guest-has-none\r\nConnection: close\r\n\r\n").unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        let mut out = std::io::stdout().lock();
        writeln!(out, "answer={}", answer.replace("\r\n", "|")).unwrap();
        for (k, v) in std::env::vars_os() {
            writeln!(out, "env:{}={}", k.to_string_lossy(), v.to_string_lossy()).unwrap();
        }
    }

    /// A host on loopback that records every request's head and answers `ok`.
    fn upstream() -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                log.lock().unwrap().push(head);
                let _ = (&stream).write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });
        (port, seen)
    }

    fn files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                out.extend(files(&entry.path()));
            } else if kind.is_file() {
                out.push(entry.path());
            }
        }
        out
    }

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("atelier-egress-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn a_guest_reaches_only_what_its_policy_allows_and_never_holds_the_secret() {
        let root = temp_root();
        let local = Local::with_faults(root.clone(), "", 0).unwrap();
        let (port, seen) = upstream();
        let forge = EgressRule {
            name: "forge".into(),
            host: "127.0.0.1".into(),
            path: Some("/repos/contoso/app".into()),
            methods: vec!["GET".into()],
            action: "Transform".into(),
            headers: vec![HeaderFromSecret { name: "Authorization".into(), secret: "forge-token".into(), key: "value".into(), format: "Bearer {}".into() }],
        };
        let sandbox = Provider::create(&local, &SandboxSpec { egress: Some(EgressPolicy { default_action: "Deny".into(), rules: vec![forge] }), ..Default::default() }).unwrap();
        let value = format!("s3cret-{}", uuid::Uuid::new_v4().simple());
        Provider::secret_set(&local, "forge-token", "value", &Secret::new(value.clone().into_bytes())).unwrap();
        let standin = Standin::start(&root, &sandbox.id).unwrap();
        let run = |url: &str| -> String {
            let mut cmd = local.command(&sandbox.id, &guest(url), None, &standin.env("")).unwrap();
            cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
            let child = local.spawn(&sandbox.id, &mut cmd).unwrap();
            String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned()
        };
        let answer = |out: &str| out.lines().find_map(|l| l.split_once("answer=").map(|(_, a)| a)).unwrap_or_else(|| panic!("no answer in {out}")).to_owned();

        // A forbidden host and a forbidden path: refused by the stand-in, never reached.
        let host = run("http://forbidden.example.com/repos/contoso/app");
        assert!(answer(&host).starts_with("HTTP/1.1 403") && answer(&host).ends_with("||denied by default"), "{host}");
        let path = run(&format!("http://127.0.0.1:{port}/repos/other"));
        assert!(answer(&path).starts_with("HTTP/1.1 403"), "{path}");
        // An allowed one: the host sees the injected header, not the guest's.
        let allowed = run(&format!("http://127.0.0.1:{port}/repos/contoso/app/pulls"));
        assert!(answer(&allowed).starts_with("HTTP/1.1 200") && answer(&allowed).ends_with("|ok"), "{allowed}");
        let heads = seen.lock().unwrap().clone();
        assert_eq!(heads.len(), 1, "{heads:?}");
        assert!(heads[0].starts_with("GET /repos/contoso/app/pulls HTTP/1.1\r\n") && heads[0].contains(&format!("Authorization: Bearer {value}\r\n")) && !heads[0].contains("guest-has-none"), "{}", heads[0].replace(&value, "<value>"));

        // The guest never holds the value: not in its environment or output, not in a file of the sandbox.
        for out in [&host, &path, &allowed] {
            assert!(out.contains(&format!("env:HTTP_PROXY=http://127.0.0.1:{}", standin.port())) && !out.contains(&value));
        }
        for file in files(&root.join("sandboxes")) {
            assert!(!String::from_utf8_lossy(&std::fs::read(&file).unwrap()).contains(&value), "{} holds the value", file.display());
        }
        let decisions = Provider::egress_decisions(&local, &sandbox.id).unwrap();
        let summary: Vec<(&str, &str, &str, bool)> = decisions.iter().map(|d| (d.host.as_str(), d.method.as_str(), d.path.as_str(), d.allowed)).collect();
        assert_eq!(summary, [("forbidden.example.com", "GET", "/repos/contoso/app", false), ("127.0.0.1", "GET", "/repos/other", false), ("127.0.0.1", "GET", "/repos/contoso/app/pulls", true)]);

        // A policy set afterwards applies to the next request.
        Provider::set_egress(&local, &sandbox.id, &EgressPolicy { default_action: "Deny".into(), rules: Vec::new() }).unwrap();
        assert!(answer(&run(&format!("http://127.0.0.1:{port}/repos/contoso/app/pulls"))).starts_with("HTTP/1.1 403"));
        assert_eq!(seen.lock().unwrap().len(), 1);
        Provider::delete(&local, &sandbox.id).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_entry_writes_its_port_and_ends_with_its_sandbox() {
        let root = temp_root();
        let local = Local::with_faults(root.clone(), "", 0).unwrap();
        let sandbox = Provider::create(&local, &SandboxSpec::default()).unwrap();
        let port_file = root.join("egress-standin.port");
        let (r, id, file) = (root.clone(), sandbox.id.clone(), port_file.clone());
        let entry = std::thread::spawn(move || standin(&r, &id, &file));
        let deadline = Instant::now() + Duration::from_secs(20);
        let port: u16 = loop {
            if let Some(port) = std::fs::read_to_string(&port_file).ok().and_then(|t| t.trim().parse().ok()) {
                break port;
            }
            assert!(Instant::now() < deadline && !entry.is_finished(), "no port file");
            std::thread::sleep(Duration::from_millis(20));
        };
        // No policy yet: everything is refused.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        stream.write_all(b"GET http://example.com/ HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 403") && out.ends_with("denied: this sandbox has no egress policy\n"), "{out}");
        assert_eq!(Provider::egress_decisions(&local, &sandbox.id).unwrap().len(), 1);
        assert_eq!(proxy_env(port, "127.0.0.1").iter().filter(|(k, v)| k.eq_ignore_ascii_case("https_proxy") && *v == format!("http://127.0.0.1:{port}")).count(), 2);

        Provider::delete(&local, &sandbox.id).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !entry.is_finished() {
            assert!(Instant::now() < deadline, "the stand-in outlived its sandbox");
            std::thread::sleep(Duration::from_millis(20));
        }
        entry.join().unwrap().unwrap();
        assert!(standin(&root, "sbx-000000000000", &port_file).is_err(), "no such sandbox");
        assert!(standin(&root, "../sandboxes", &port_file).is_err(), "not an id");
        let _ = std::fs::remove_dir_all(root);
    }
}
