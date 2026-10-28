//! A service this machine paired with (`copilot-canvas remote add`, then
//! `--remote NAME`): its address and credentials and, for a Dev Tunnel, the
//! `devtunnel connect` child that forwards the tunnel's port here. The child
//! is started on the first connection, started again when it died (every
//! reconnect asks for the address), restarted when connections through it
//! keep failing, and killed with this process (a kill-on-close job).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use canvas_common::remote::{ServerRecord, Servers};
use canvas_common::win::KillOnClose;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How long `devtunnel connect` may take to forward the port.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Without a "Forwarding from" line, the port is probed after this long.
const PROBE_AFTER: Duration = Duration::from_secs(8);
/// Failed connections through a live child before it is restarted.
const RESTART_AFTER_FAILURES: u32 = 2;
/// Output lines kept for error messages.
const KEEP_LINES: usize = 30;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// A paired service.
pub struct Server {
    pub record: ServerRecord,
    secret: Vec<u8>,
    tunnel: Option<Tunnel>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").field("name", &self.record.name).field("where", &self.record.describe()).finish()
    }
}

impl Server {
    /// The server `name` in `<home>/remote/servers.json` (`None`: not there).
    pub fn load(home: &Path, name: &str) -> Result<Option<Arc<Server>>> {
        let servers = Servers::load(home)?;
        let Some(record) = servers.find(name).cloned() else { return Ok(None) };
        let secret = record.secret()?;
        Ok(Some(Self::new(record, secret)))
    }

    pub fn new(record: ServerRecord, secret: Vec<u8>) -> Arc<Server> {
        let tunnel = record.tunnel.clone().map(|id| Tunnel::new(&id, record.port));
        Arc::new(Server { record, secret, tunnel })
    }

    pub fn name(&self) -> &str {
        &self.record.name
    }

    pub fn client_id(&self) -> &str {
        &self.record.client_id
    }

    pub fn secret(&self) -> &[u8] {
        &self.secret
    }

    /// `HOST:PORT` to connect to now: the direct address, or the port the
    /// tunnel child forwards (started or restarted as needed).
    pub fn address(&self) -> Result<String> {
        match (&self.tunnel, &self.record.addr) {
            (Some(tunnel), _) => tunnel.address(),
            (None, Some(addr)) => Ok(addr.clone()),
            (None, None) => bail!("the server {:?} has neither a tunnel nor an address; add it again", self.record.name),
        }
    }

    /// A connection through the current address failed.
    pub fn failed(&self) {
        if let Some(tunnel) = &self.tunnel {
            lock(&tunnel.state).failures += 1;
        }
    }

    /// A connection succeeded.
    pub fn succeeded(&self) {
        if let Some(tunnel) = &self.tunnel {
            lock(&tunnel.state).failures = 0;
        }
    }

    /// The `devtunnel connect` child's process id, while it runs.
    pub fn tunnel_pid(&self) -> Option<u32> {
        let tunnel = self.tunnel.as_ref()?;
        let mut state = lock(&tunnel.state);
        let running = state.child.as_mut()?;
        running.child.try_wait().ok().flatten().is_none().then(|| running.child.id())
    }

    /// Stops the tunnel child (it is started again on the next connection).
    pub fn stop_tunnel(&self) {
        if let Some(tunnel) = &self.tunnel {
            lock(&tunnel.state).child = None;
        }
    }
}

/// `devtunnel connect ID`, kept running.
struct Tunnel {
    id: String,
    /// The tunnel's port (the service's remote port).
    port: u16,
    state: Mutex<TunnelState>,
}

#[derive(Default)]
struct TunnelState {
    child: Option<Running>,
    failures: u32,
}

#[derive(Default)]
struct Output {
    lines: VecDeque<String>,
    /// (local host, local port, tunnel port) from "Forwarding from …" lines.
    forwards: Vec<(String, u16, u16)>,
}

struct Running {
    child: Child,
    _job: Option<KillOnClose>,
    output: Arc<Mutex<Output>>,
    started: Instant,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A "Forwarding from 127.0.0.1:47601 to host port 47600." line as
/// (local host, local port, tunnel port).
fn parse_forward(line: &str) -> Option<(String, u16, u16)> {
    let lower = line.to_ascii_lowercase();
    let rest = &lower[lower.find("forwarding from ")? + "forwarding from ".len()..];
    let (local, remote) = rest.split_once(" to ")?;
    let (host, port) = local.trim().rsplit_once(':')?;
    let port: u16 = port.trim().parse().ok()?;
    let after = &remote[remote.find("port")? + 4..];
    let digits: String = after.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    let host = match host {
        "localhost" => "127.0.0.1".to_owned(),
        other => other.to_owned(),
    };
    Some((host, port, digits.parse().ok()?))
}

/// The local address forwarding `port` (IPv4 preferred); a single forwarded
/// port is used even when it is another one.
fn pick_forward(forwards: &[(String, u16, u16)], port: u16) -> Option<String> {
    let address = |(host, local, _): &(String, u16, u16)| format!("{host}:{local}");
    let matching: Vec<&(String, u16, u16)> = forwards.iter().filter(|f| f.2 == port).collect();
    if let Some(found) = matching.iter().find(|f| !f.0.starts_with('[')).or(matching.first()) {
        return Some(address(found));
    }
    let mut ports: Vec<u16> = forwards.iter().map(|f| f.2).collect();
    ports.dedup();
    match ports.as_slice() {
        [_] => forwards.iter().find(|f| !f.0.starts_with('[')).or(forwards.first()).map(address),
        _ => None,
    }
}

fn read_lines(source: impl Read + Send + 'static, output: Arc<Mutex<Output>>) {
    std::thread::Builder::new()
        .name("devtunnel-output".into())
        .spawn(move || {
            for line in BufReader::new(source).lines() {
                let Ok(line) = line else { break };
                let mut output = lock(&output);
                if let Some(forward) = parse_forward(&line) {
                    output.forwards.push(forward);
                }
                output.lines.push_back(line);
                if output.lines.len() > KEEP_LINES {
                    output.lines.pop_front();
                }
            }
        })
        .ok();
}

impl Tunnel {
    fn new(id: &str, port: u16) -> Self {
        Self { id: id.to_owned(), port, state: Mutex::new(TunnelState::default()) }
    }

    fn spawn(&self) -> Result<Running> {
        let cli = canvas_common::remote::devtunnel().context(
            "devtunnel.exe was not found: install the Dev Tunnels CLI (`winget install Microsoft.devtunnel`) or set COPILOT_CANVAS_DEVTUNNEL",
        )?;
        let mut child = Command::new(&cli.program)
            .args(&cli.prefix)
            .args(["connect", &self.id])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .with_context(|| format!("cannot start {}", cli.program.display()))?;
        let job = KillOnClose::new(&child).ok();
        let output = Arc::new(Mutex::new(Output::default()));
        if let Some(stdout) = child.stdout.take() {
            read_lines(stdout, output.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            read_lines(stderr, output.clone());
        }
        Ok(Running { child, _job: job, output, started: Instant::now() })
    }

    /// The forwarded address; starts the child when it is not running (or
    /// died, or kept failing) and waits until it forwards the port.
    fn address(&self) -> Result<String> {
        let mut state = lock(&self.state);
        let exited = state.child.as_mut().is_some_and(|r| r.child.try_wait().ok().flatten().is_some());
        if exited || state.failures >= RESTART_AFTER_FAILURES {
            state.child = None;
            state.failures = 0;
        }
        if state.child.is_none() {
            state.child = Some(self.spawn()?);
        }
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let running = state.child.as_mut().expect("started above");
            let (forward, tail) = {
                let output = lock(&running.output);
                (pick_forward(&output.forwards, self.port), output.lines.iter().rev().take(4).rev().cloned().collect::<Vec<_>>().join(" "))
            };
            if let Some(address) = forward {
                return Ok(address);
            }
            if let Ok(Some(status)) = running.child.try_wait() {
                // Let the readers catch the last lines.
                std::thread::sleep(Duration::from_millis(100));
                let tail = lock(&running.output).lines.iter().rev().take(4).rev().cloned().collect::<Vec<_>>().join(" ");
                state.child = None;
                let hint = if tail.to_ascii_lowercase().contains("login") {
                    " Sign in once with `devtunnel user login -g` (GitHub) or `devtunnel user login`."
                } else {
                    ""
                };
                bail!("`devtunnel connect {}` exited ({status}): {}{hint}", self.id, if tail.is_empty() { "no output" } else { &tail });
            }
            // An unrecognized output format: the tunnel's own port answering is enough.
            if running.started.elapsed() >= PROBE_AFTER
                && let Ok(addr) = format!("127.0.0.1:{}", self.port).parse()
                && std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
            {
                return Ok(format!("127.0.0.1:{}", self.port));
            }
            if Instant::now() >= deadline {
                state.child = None;
                bail!("`devtunnel connect {}` did not forward port {} within {} s: {}", self.id, self.port, READY_TIMEOUT.as_secs(), if tail.is_empty() { "no output" } else { &tail });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarding_lines_are_parsed_and_the_tunnel_port_is_picked() {
        assert_eq!(parse_forward("Forwarding from 127.0.0.1:47601 to host port 47600."), Some(("127.0.0.1".into(), 47601, 47600)));
        assert_eq!(parse_forward("SSH: Forwarding from [::1]:22 to host port 22."), Some(("[::1]".into(), 22, 22)));
        assert_eq!(parse_forward("forwarding from localhost:5000 to host port 5000"), Some(("127.0.0.1".into(), 5000, 5000)));
        assert_eq!(parse_forward("Connected to tunnel: copilot-canvas.usw2"), None);
        let forwards = vec![("[::1]".to_owned(), 47600, 47600), ("127.0.0.1".to_owned(), 47601, 47600), ("127.0.0.1".to_owned(), 22, 22)];
        assert_eq!(pick_forward(&forwards, 47600).as_deref(), Some("127.0.0.1:47601"), "IPv4 first");
        assert_eq!(pick_forward(&forwards, 9999), None, "several other ports: none is ours");
        assert_eq!(pick_forward(&forwards[..2], 9999).as_deref(), Some("127.0.0.1:47601"), "the only forwarded port");
        assert_eq!(pick_forward(&[], 47600), None);
    }

    #[test]
    fn direct_servers_connect_to_their_address() {
        let record = ServerRecord { name: "x".into(), addr: Some("127.0.0.1:5".into()), client_id: "pc-1".into(), ..Default::default() };
        let server = Server::new(record, vec![1; 32]);
        assert_eq!(server.address().unwrap(), "127.0.0.1:5");
        assert_eq!(server.tunnel_pid(), None);
        let none = Server::new(ServerRecord { name: "y".into(), ..Default::default() }, vec![]);
        assert!(none.address().is_err());
    }

    /// The only test that sets `COPILOT_CANVAS_DEVTUNNEL` (`cmd.exe` plays the CLI).
    #[test]
    fn the_tunnel_child_forwards_restarts_and_reports_its_failures() {
        let tunnel = |script: &str| {
            // SAFETY: no other test in this binary reads the variable.
            unsafe { std::env::set_var(canvas_common::remote::DEVTUNNEL_ENV, format!("cmd.exe|/d|/c|{script} & rem")) };
            Server::new(ServerRecord { name: "t".into(), tunnel: Some("fake".into()), port: 47600, client_id: "pc-1".into(), ..Default::default() }, vec![])
        };
        let server = tunnel("echo Connected to tunnel: fake.usw2& echo Forwarding from 127.0.0.1:5555 to host port 47600.& ping -n 30 127.0.0.1");
        assert_eq!(server.address().unwrap(), "127.0.0.1:5555");
        let first = server.tunnel_pid().expect("running");
        assert_eq!(server.address().unwrap(), "127.0.0.1:5555", "a running child is reused");
        assert_eq!(server.tunnel_pid(), Some(first));
        server.failed();
        server.failed();
        server.address().unwrap();
        let second = server.tunnel_pid().expect("restarted");
        assert_ne!(second, first, "a child whose connections keep failing is restarted");
        assert!(!canvas_common::win::alive(first));
        server.stop_tunnel();
        assert!(!canvas_common::win::alive(second), "stopping kills the child");
        let failing = tunnel("echo Login required.& exit 3");
        let error = failing.address().unwrap_err().to_string();
        unsafe { std::env::remove_var(canvas_common::remote::DEVTUNNEL_ENV) };
        assert!(error.contains("Login required") && error.contains("devtunnel user login -g"), "{error}");
    }
}
