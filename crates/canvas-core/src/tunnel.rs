//! A service this machine paired with (`copilot-canvas remote add`, then
//! `--remote NAME`): its address and credentials and, for a Dev Tunnel, the
//! `devtunnel connect` child that forwards the tunnel's port here. The child
//! is started on the first connection, started again when it died (every
//! reconnect asks for the address), restarted when connections through it
//! keep failing, and killed with this process (a kill-on-close job).
//!
//! `devtunnel connect ID` has no option for the local port: it listens on
//! each tunnel port's own number, or the next free one (up to ten higher)
//! when that is taken, and says where (Dev Tunnels CLI 1.0.2115):
//!
//! ```text
//! Connected to tunnel: copilot-canvas.usw2
//! Forwarding from 127.0.0.1:47601 to host port 47600.
//! Forwarding from [::1]:47601 to host port 47600.
//! ```
//!
//! The address comes from those lines. If the wording ever differs, the
//! client only connects to a port the child itself (or a process it
//! started) listens on, never to whatever else answers on the tunnel's port.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use canvas_common::remote::{ServerRecord, Servers};
use canvas_sys::process::KillOnClose;
use canvas_sys::spawn::CommandExt;

/// How long `devtunnel connect` may take to forward the port.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Without a "Forwarding from" line, the child's own listeners are looked for after this long.
const PROBE_AFTER: Duration = Duration::from_secs(8);
/// How often the child's listeners are looked for.
const PROBE_EVERY: Duration = Duration::from_millis(500);
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
        let secret = canvas_sys::secret::open(home, &record.secret).with_context(|| format!("cannot decrypt the secret of {:?} (paired by another user?)", record.name))?;
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
    ready_timeout: Duration,
    probe_after: Duration,
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
    probed: Option<Instant>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A "Forwarding from 127.0.0.1:47601 to host port 47600." line (the Dev
/// Tunnels SDK's wording, possibly after a prefix such as "SSH: ") as
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
        "localhost" | "0.0.0.0" => "127.0.0.1".to_owned(),
        "[::]" => "[::1]".to_owned(),
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

/// A loopback address that one of `owners` (the tunnel child and the
/// processes it started) listens on: the tunnel's own `port` when it is one
/// of them, else the only port they listen on (IPv4 preferred). Listeners of
/// any other process are never chosen.
fn owned_forward(listeners: &[(SocketAddr, u32)], owners: &[u32], port: u16) -> Option<String> {
    let owned: Vec<SocketAddr> = listeners.iter().filter(|(addr, pid)| owners.contains(pid) && (addr.ip().is_loopback() || addr.ip().is_unspecified())).map(|(addr, _)| *addr).collect();
    let mut ports: Vec<u16> = owned.iter().map(SocketAddr::port).collect();
    ports.sort_unstable();
    ports.dedup();
    let chosen = if ports.contains(&port) {
        port
    } else {
        match ports.as_slice() {
            [only] => *only,
            _ => return None,
        }
    };
    Some(if owned.iter().any(|a| a.port() == chosen && a.is_ipv4()) { format!("127.0.0.1:{chosen}") } else { format!("[::1]:{chosen}") })
}

/// The tunnel child and the processes it started (those that started after it).
fn child_tree(pid: u32) -> Vec<u32> {
    let born = canvas_sys::process::process_start_ms(pid);
    let mut tree = vec![pid];
    tree.extend(canvas_sys::process::descendants(pid).into_iter().filter(|d| born.is_some_and(|b| canvas_sys::process::process_start_ms(*d).is_some_and(|s| s >= b))));
    tree
}

/// A hint for a `devtunnel connect` that ended, from its last lines.
fn exit_hint(tail: &str) -> &'static str {
    let lower = tail.to_ascii_lowercase();
    if lower.contains("login required") || lower.contains("login token expired") || lower.contains("not logged in") {
        " Sign in once with `devtunnel user login -g` (GitHub) or `devtunnel user login` (Microsoft), as the account that created the tunnel."
    } else if lower.contains("tunnel not found") {
        " Check the tunnel id (`devtunnel list` on the service's machine) and add the server again with the right `--tunnel`."
    } else {
        ""
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
        Self { id: id.to_owned(), port, ready_timeout: READY_TIMEOUT, probe_after: PROBE_AFTER, state: Mutex::new(TunnelState::default()) }
    }

    fn spawn(&self) -> Result<Running> {
        anyhow::ensure!(canvas_common::remote::valid_tunnel_id(&self.id), "{:?} is not a Dev Tunnel id; add the server again", self.id);
        let cli = canvas_common::remote::devtunnel().context(
            "devtunnel.exe was not found: install the Dev Tunnels CLI (`winget install Microsoft.devtunnel`) or set COPILOT_CANVAS_DEVTUNNEL",
        )?;
        let mut child = Command::new(&cli.program)
            .args(&cli.prefix)
            .args(["connect", &self.id])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .no_window()
            .die_with_parent()
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
        Ok(Running { child, _job: job, output, started: Instant::now(), probed: None })
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
        let deadline = Instant::now() + self.ready_timeout;
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
                bail!("`devtunnel connect {}` exited ({status}): {}{}", self.id, if tail.is_empty() { "no output" } else { &tail }, exit_hint(&tail));
            }
            // Output this build does not recognize: a port the child itself listens on.
            if running.started.elapsed() >= self.probe_after && running.probed.is_none_or(|at| at.elapsed() >= PROBE_EVERY) {
                running.probed = Some(Instant::now());
                let owners = child_tree(running.child.id());
                if let Some(address) = canvas_sys::net::listeners().ok().and_then(|listeners| owned_forward(&listeners, &owners, self.port)) {
                    return Ok(address);
                }
            }
            if Instant::now() >= deadline {
                state.child = None;
                bail!(
                    "`devtunnel connect {}` did not forward port {} within {} s: {}",
                    self.id,
                    self.port,
                    self.ready_timeout.as_secs(),
                    if tail.is_empty() { "no output" } else { &tail }
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines of the real CLI (1.0.2115; strings from its binary).
    #[test]
    fn forwarding_lines_of_the_real_cli_are_parsed_and_the_tunnel_port_is_picked() {
        assert_eq!(parse_forward("Forwarding from 127.0.0.1:47601 to host port 47600."), Some(("127.0.0.1".into(), 47601, 47600)));
        assert_eq!(parse_forward("SSH: Forwarding from [::1]:47600 to host port 47600."), Some(("[::1]".into(), 47600, 47600)));
        assert_eq!(parse_forward("forwarding from localhost:5000 to host port 5000"), Some(("127.0.0.1".into(), 5000, 5000)));
        for other in [
            "Connected to tunnel: copilot-canvas.usw2",
            "Listening on port 47600 failed: Only one usage of each socket address (protocol/network address/port) is normally permitted.",
            "Incrementing port and trying again",
            "Using local interface address 127.0.0.1 instead of 0.0.0.0.",
            "Connection to port 47600 is E2E encrypted between tunnel client and tunnel host.",
            "Ready to accept connections for tunnel: copilot-canvas.usw2.",
            "Login required.",
        ] {
            assert_eq!(parse_forward(other), None, "{other}");
        }
        let forwards = vec![("[::1]".to_owned(), 47600, 47600), ("127.0.0.1".to_owned(), 47601, 47600), ("127.0.0.1".to_owned(), 22, 22)];
        assert_eq!(pick_forward(&forwards, 47600).as_deref(), Some("127.0.0.1:47601"), "IPv4 first");
        assert_eq!(pick_forward(&forwards, 9999), None, "several other ports: none is ours");
        assert_eq!(pick_forward(&forwards[..2], 9999).as_deref(), Some("127.0.0.1:47601"), "the only forwarded port");
        assert_eq!(pick_forward(&[], 47600), None);
        assert!(exit_hint("Login required.").contains("devtunnel user login -g"));
        assert!(exit_hint("Tunnel not found: copilot-canvas").contains("devtunnel list"));
        assert_eq!(exit_hint("something else"), "");
    }

    #[test]
    fn only_listeners_of_the_tunnel_child_are_used_without_a_forwarding_line() {
        let v4 = |port: u16| SocketAddr::from(([127, 0, 0, 1], port));
        let v6 = |port: u16| SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
        let lan = SocketAddr::from(([192, 0, 2, 5], 47600));
        // A squatter (pid 9) holds the tunnel's port; the child (pid 5) moved to 47601.
        let listeners = [(v4(47600), 9), (v4(47601), 5), (v6(47601), 5), (lan, 5)];
        assert_eq!(owned_forward(&listeners, &[5], 47600).as_deref(), Some("127.0.0.1:47601"));
        assert_eq!(owned_forward(&listeners, &[5, 6], 47600).as_deref(), Some("127.0.0.1:47601"));
        assert_eq!(owned_forward(&listeners[..1], &[5], 47600), None, "the squatter's port is never used");
        assert_eq!(owned_forward(&[(v4(47600), 5), (v4(22), 5)], &[5], 47600).as_deref(), Some("127.0.0.1:47600"), "the tunnel's own port first");
        assert_eq!(owned_forward(&[(v4(1000), 5), (v4(22), 5)], &[5], 47600), None, "several other ports: none is ours");
        assert_eq!(owned_forward(&[(v6(47600), 5)], &[5], 47600).as_deref(), Some("[::1]:47600"));
        assert_eq!(owned_forward(&[(SocketAddr::from(([0, 0, 0, 0], 47600)), 5)], &[5], 47600).as_deref(), Some("127.0.0.1:47600"), "a wildcard listener");
    }

    #[test]
    fn direct_servers_connect_to_their_address() {
        let record = ServerRecord { name: "x".into(), addr: Some("127.0.0.1:5".into()), client_id: "pc-1".into(), ..Default::default() };
        let server = Server::new(record, vec![1; 32]);
        assert_eq!(server.address().unwrap(), "127.0.0.1:5");
        assert_eq!(server.tunnel_pid(), None);
        let none = Server::new(ServerRecord { name: "y".into(), ..Default::default() }, vec![]);
        assert!(none.address().is_err());
        let injected = Server::new(ServerRecord { name: "z".into(), tunnel: Some("--help".into()), port: 1, ..Default::default() }, vec![]);
        assert!(injected.address().unwrap_err().to_string().contains("not a Dev Tunnel id"), "never passed to devtunnel");
    }

    /// The only test that sets `COPILOT_CANVAS_DEVTUNNEL` (`cmd.exe` plays the
    /// CLI on Windows, `/bin/sh` elsewhere: `os(windows, unix)` picks the script).
    #[test]
    fn the_tunnel_child_forwards_restarts_and_reports_its_failures() {
        let os = |windows: &str, unix: &str| if cfg!(windows) { windows.to_owned() } else { unix.to_owned() };
        let set = |script: &str| {
            let cli = if cfg!(windows) { format!("cmd.exe|/d|/c|{script} & rem") } else { format!("/bin/sh|-c|{script}") };
            // SAFETY: no other test in this binary reads the variable.
            unsafe { std::env::set_var(canvas_common::remote::DEVTUNNEL_ENV, cli) };
        };
        let tunnel = |script: &str| {
            set(script);
            Server::new(ServerRecord { name: "t".into(), tunnel: Some("fake".into()), port: 47600, client_id: "pc-1".into(), ..Default::default() }, vec![])
        };
        let server = tunnel(&os(
            "echo Connected to tunnel: fake.usw2& echo Forwarding from 127.0.0.1:5555 to host port 47600.& ping -n 30 127.0.0.1",
            "echo Connected to tunnel: fake.usw2; echo Forwarding from 127.0.0.1:5555 to host port 47600.; sleep 30",
        ));
        assert_eq!(server.address().unwrap(), "127.0.0.1:5555");
        let first = server.tunnel_pid().expect("running");
        assert_eq!(server.address().unwrap(), "127.0.0.1:5555", "a running child is reused");
        assert_eq!(server.tunnel_pid(), Some(first));
        server.failed();
        server.failed();
        server.address().unwrap();
        let second = server.tunnel_pid().expect("restarted");
        assert_ne!(second, first, "a child whose connections keep failing is restarted");
        assert!(!canvas_sys::process::alive(first));
        server.stop_tunnel();
        assert!(!canvas_sys::process::alive(second), "stopping kills the child");
        let failing = tunnel(&os("echo Login required.& exit 3", "echo Login required.; exit 3"));
        let error = failing.address().unwrap_err().to_string();
        assert!(error.contains("Login required") && error.contains("devtunnel user login -g"), "{error}");

        // Silent children: the tunnel's port answering is not enough, the child must own the listener.
        let quick = |port: u16| Tunnel { ready_timeout: Duration::from_secs(20), probe_after: Duration::from_millis(200), ..Tunnel::new("fake", port) };
        let squatted = canvas_common::tcp::listen_loopback("127.0.0.1:0").unwrap();
        let squatted_port = squatted.local_addr().unwrap().port();
        set(&os("ping -n 30 127.0.0.1 >nul", "sleep 30"));
        let silent = Tunnel { ready_timeout: Duration::from_secs(2), ..quick(squatted_port) };
        let error = silent.address().unwrap_err().to_string();
        assert!(error.contains("did not forward"), "a port this test (not the child) listens on is never used: {error}");
        drop(squatted);
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let owned_port = free.local_addr().unwrap().port();
        drop(free);
        // No quotes: cmd.exe and PowerShell would each read them their own way.
        set(&os(
            &format!("powershell -NoProfile -Command $l=[Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback,{owned_port});$l.Start();Start-Sleep 30"),
            &format!("exec perl -MIO::Socket::INET -e 'my $s = IO::Socket::INET->new(LocalAddr => \"127.0.0.1\", LocalPort => {owned_port}, Listen => 1) or die; sleep 30'"),
        ));
        let owned = quick(owned_port);
        let found = owned.address();
        unsafe { std::env::remove_var(canvas_common::remote::DEVTUNNEL_ENV) };
        assert_eq!(found.unwrap(), format!("127.0.0.1:{owned_port}"), "a listener of the child's own process tree is used");
        lock(&owned.state).child = None;
    }
}
