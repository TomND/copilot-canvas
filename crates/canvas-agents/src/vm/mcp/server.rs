//! The hub's MCP servers: where their definitions come from (the hub's
//! Copilot configuration, [`CopilotServers`]) and a running local server
//! ([`Process`]): a child of the service speaking JSON-RPC on stdio, one
//! message per line, with the hub's own environment and credentials. The
//! guest never sees its command line, environment or error output.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Sender, bounded};
use serde_json::Value;

use super::{ServerSource, ServerSpec};

/// The servers of a Copilot CLI configuration file (`mcp-config.json`,
/// `mcpServers`): `type` `local`/`stdio` (or none) with `command`, `args`,
/// `env`; `http`/`sse` with `url`, `headers`. `${VAR}` in `env` values and
/// headers is filled from the service's environment. A server marked
/// `disabled: true` (or `enabled: false`) is not offered. The file is read
/// at each session's start, so edits apply to the next session.
pub struct CopilotServers {
    path: PathBuf,
}

impl CopilotServers {
    pub fn new(path: impl Into<PathBuf>) -> CopilotServers {
        CopilotServers { path: path.into() }
    }

    /// This machine's Copilot configuration (`<COPILOT_HOME>/mcp-config.json`, else `~/.copilot/…`).
    pub fn of_this_machine() -> CopilotServers {
        CopilotServers::new(crate::copilot_cfg::copilot_home().join("mcp-config.json"))
    }
}

impl ServerSource for CopilotServers {
    fn spec(&self, server: &str) -> Option<ServerSpec> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let config: Value = serde_json::from_str(&text).ok()?;
        entry(config.get("mcpServers")?.get(server)?)
    }
}

/// One `mcpServers` entry.
pub fn entry(entry: &Value) -> Option<ServerSpec> {
    if entry.get("disabled").and_then(Value::as_bool) == Some(true) || entry.get("enabled").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let pairs = |key: &str| -> Option<Vec<(String, String)>> {
        match entry.get(key) {
            None | Some(Value::Null) => Some(Vec::new()),
            Some(Value::Object(map)) => Some(map.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), expand(v)))).collect()),
            Some(_) => None,
        }
    };
    let kind = entry.get("type").and_then(Value::as_str).unwrap_or(if entry.get("url").is_some() { "http" } else { "stdio" });
    match kind {
        "local" | "stdio" => {
            let command = entry.get("command")?.as_str()?.trim().to_owned();
            if command.is_empty() {
                return None;
            }
            let args = match entry.get("args") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(items)) => items.iter().map(|a| a.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>()?,
                Some(_) => return None,
            };
            Some(ServerSpec::Stdio { command, args, env: pairs("env")? })
        }
        "http" | "sse" => {
            let url = entry.get("url")?.as_str()?.trim().to_owned();
            (!url.is_empty()).then_some(())?;
            Some(ServerSpec::Http { url, headers: pairs("headers")? })
        }
        _ => None,
    }
}

/// `${VAR}` (or `${env:VAR}`) filled from this process's environment (empty when unset).
fn expand(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = after[..end].trim();
        let name = name.strip_prefix("env:").unwrap_or(name);
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            out.push_str(&std::env::var(name).unwrap_or_default());
        } else {
            out.push_str(&rest[start..start + 2 + end + 1]);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Whether a variable or header named `name` holds a credential (its value
/// is kept from the guest even if a server repeats it).
fn secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.split(|c: char| !c.is_ascii_alphanumeric()).any(|word| {
        matches!(word, "TOKEN" | "SECRET" | "KEY" | "APIKEY" | "PASSWORD" | "PASSWD" | "PASS" | "PAT" | "AUTH" | "AUTHORIZATION" | "CREDENTIAL" | "CREDENTIALS" | "COOKIE" | "BEARER")
    }) || upper.ends_with("TOKEN")
        || upper.ends_with("APIKEY")
}

/// The values the gateway removes from whatever a server sends the guest:
/// the server's credentials (its `env` and headers under secret-looking
/// names) and the service's own (its environment), longest first.
pub fn secret_values(spec: &ServerSpec) -> Vec<String> {
    let given = match spec {
        ServerSpec::Stdio { env, .. } => env.clone(),
        ServerSpec::Http { headers, .. } => headers.iter().map(|(k, v)| (k.clone(), v.split_whitespace().last().unwrap_or(v).to_owned())).chain(headers.iter().cloned()).collect(),
    };
    let mut values: Vec<String> = given.into_iter().chain(std::env::vars()).filter(|(k, v)| secret_name(k) && v.trim().len() >= 8).map(|(_, v)| v.trim().to_owned()).collect();
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    values.dedup();
    values
}

/// `text` without any of `secrets` (each becomes `[redacted]`).
pub fn redact(text: &str, secrets: &[String]) -> Option<String> {
    let mut out: Option<String> = None;
    for secret in secrets {
        let escaped = serde_json::to_string(secret).unwrap_or_default();
        let escaped = escaped.trim_matches('"');
        for form in [secret.as_str(), escaped] {
            if !form.is_empty() && out.as_deref().unwrap_or(text).contains(form) {
                out = Some(out.as_deref().unwrap_or(text).replace(form, "[redacted]"));
            }
        }
    }
    out
}

/// What a server's reader reports: a message, or the end of its output.
pub struct FromServer {
    pub proc: u64,
    pub message: Option<Value>,
}

/// A server's line may be this long; a longer one ends the server.
const MAX_LINE: usize = 16 << 20;

/// A running local server.
pub struct Process {
    pub pid: u32,
    stdin: Option<Sender<Vec<u8>>>,
    child: Option<Child>,
}

impl Process {
    /// Starts `command` with `args` and `env` (added to the service's
    /// environment). Its messages go to `events` as process `proc`.
    pub fn start(command: &str, args: &[String], env: &[(String, String)], proc: u64, events: Sender<FromServer>) -> Result<Process> {
        use canvas_sys::spawn::CommandExt;
        let mut cmd = Command::new(program(command));
        cmd.args(args).envs(env.iter().map(|(k, v)| (k, v))).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        if let Some(home) = dirs::home_dir().filter(|h| h.is_dir()) {
            cmd.current_dir(home);
        }
        // A process group of its own: what it starts is ended with it.
        cmd.spawn_flags(canvas_sys::spawn::NO_WINDOW | canvas_sys::spawn::NEW_PROCESS_GROUP).die_with_parent();
        let mut child = canvas_sys::spawn::spawn_tied(cmd).context("cannot start the MCP server")?;
        let pid = child.id();
        let (stdin, stdout) = (child.stdin.take().context("no stdin")?, child.stdout.take().context("no stdout")?);
        let (tx, rx) = bounded::<Vec<u8>>(256);
        std::thread::Builder::new().name("mcp-server-in".into()).spawn(move || {
            let mut stdin = stdin;
            for mut line in rx {
                line.push(b'\n');
                if stdin.write_all(&line).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        })?;
        std::thread::Builder::new().name("mcp-server-out".into()).spawn(move || {
            read_lines(stdout, |message| events.send(FromServer { proc, message: Some(message) }).is_ok());
            let _ = events.send(FromServer { proc, message: None });
        })?;
        Ok(Process { pid, stdin: Some(tx), child: Some(child) })
    }

    /// Sends one message (`false`: the server does not take it: it ended or stopped reading).
    pub fn send(&self, message: &Value) -> bool {
        let Ok(line) = serde_json::to_vec(message) else { return false };
        self.stdin.as_ref().is_some_and(|tx| tx.try_send(line).is_ok())
    }
}

impl Drop for Process {
    /// Ends the server: its input closes (a well-behaved server exits), and
    /// within 2 s whatever is left of its process tree and group is killed.
    /// The child is reaped only after that, so its pid cannot have been
    /// reused when the tree is killed.
    fn drop(&mut self) {
        self.stdin = None;
        let Some(mut child) = self.child.take() else { return };
        let pid = self.pid;
        let _ = std::thread::Builder::new().name("mcp-server-end".into()).spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && canvas_sys::process::alive(pid) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = canvas_sys::process::kill_tree(pid);
            let _ = child.kill();
            let _ = child.wait();
        });
    }
}

/// Reads one JSON message per line (a batch is split; a line that is not
/// JSON-RPC is skipped: some servers print banners) until the end, an
/// error, a line over [`MAX_LINE`] or `take` returning false.
pub fn read_lines(output: impl Read, mut take: impl FnMut(Value) -> bool) {
    let mut reader = BufReader::with_capacity(64 << 10, output);
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut too_long = false;
        loop {
            let available = match reader.fill_buf() {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            if available.is_empty() {
                if line.is_empty() {
                    return;
                }
                break;
            }
            let (used, end) = match available.iter().position(|b| *b == b'\n') {
                Some(at) => (at + 1, true),
                None => (available.len(), false),
            };
            if line.len() + used > MAX_LINE {
                too_long = true;
            } else {
                line.extend_from_slice(&available[..used]);
            }
            reader.consume(used);
            if end || too_long {
                break;
            }
        }
        if too_long {
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else { continue };
        let messages = match value {
            Value::Array(items) => items,
            single @ Value::Object(_) => vec![single],
            _ => continue,
        };
        for message in messages {
            if !take(message) {
                return;
            }
        }
    }
}

/// The program to run for `command`: on Windows a bare name is looked up on
/// `PATH` with `.exe`, `.cmd`, `.bat` and `.com` (`npx` is `npx.cmd`).
fn program(command: &str) -> PathBuf {
    if cfg!(windows) && std::path::Path::new(command).extension().is_none() && !command.contains(['/', '\\']) {
        for ext in [".exe", ".cmd", ".bat", ".com"] {
            if let Some(found) = crate::service::find_on_path(&format!("{command}{ext}")) {
                return found;
            }
        }
    }
    PathBuf::from(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn copilots_servers_are_read_from_its_configuration() {
        let dir = std::env::temp_dir().join(format!("atelier-mcp-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp-config.json");
        let config = json!({ "mcpServers": {
            "forge": { "type": "local", "command": "forge-mcp", "args": ["--stdio"], "env": { "FORGE_TOKEN": "${ATELIER_MCP_TEST_UNSET_VAR}x", "MODE": "ro", "N": 3 }, "tools": ["*"] },
            "bare": { "command": "node", "args": ["server.js"] },
            "remote": { "type": "http", "url": "https://mcp.example.com/mcp", "headers": { "Authorization": "Bearer abc" } },
            "events": { "type": "sse", "url": "https://mcp.example.com/sse" },
            "off": { "command": "x", "disabled": true },
            "badargs": { "command": "x", "args": [1] },
            "nocommand": { "type": "stdio" },
            "odd": { "type": "websocket", "url": "wss://x" }
        }});
        std::fs::write(&path, config.to_string()).unwrap();
        let servers = CopilotServers::new(&path);
        assert_eq!(servers.spec("forge"), Some(ServerSpec::Stdio { command: "forge-mcp".into(), args: vec!["--stdio".into()], env: vec![("FORGE_TOKEN".into(), "x".into()), ("MODE".into(), "ro".into())] }));
        assert_eq!(servers.spec("bare"), Some(ServerSpec::Stdio { command: "node".into(), args: vec!["server.js".into()], env: Vec::new() }));
        assert_eq!(servers.spec("remote"), Some(ServerSpec::Http { url: "https://mcp.example.com/mcp".into(), headers: vec![("Authorization".into(), "Bearer abc".into())] }));
        assert!(matches!(servers.spec("events"), Some(ServerSpec::Http { .. })));
        for absent in ["off", "badargs", "nocommand", "odd", "missing"] {
            assert_eq!(servers.spec(absent), None, "{absent}");
        }
        assert_eq!(CopilotServers::new(dir.join("none.json")).spec("forge"), None);
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(servers.spec("forge"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn placeholders_and_secrets() {
        let path = std::env::var("PATH").unwrap_or_default();
        assert_eq!(expand("a${PATH}b"), format!("a{path}b"));
        assert_eq!(expand("${env:PATH}"), path);
        assert_eq!(expand("${bad name}${"), "${bad name}${");
        assert!(secret_name("GITHUB_PERSONAL_ACCESS_TOKEN") && secret_name("ATELIER_MCP_STANDIN_TOKEN") && secret_name("OPENAI_API_KEY") && secret_name("Authorization") && secret_name("GHTOKEN"));
        assert!(!secret_name("PATH") && !secret_name("MODE") && !secret_name("KEYBOARD_LAYOUT") && !secret_name("HOST"));
        let spec = ServerSpec::Stdio { command: "x".into(), args: Vec::new(), env: vec![("FORGE_TOKEN".into(), "tok-1234567890".into()), ("SHORT_TOKEN".into(), "abc".into()), ("MODE".into(), "readonly-mode".into())] };
        let secrets = secret_values(&spec);
        assert!(secrets.contains(&"tok-1234567890".to_owned()) && !secrets.contains(&"abc".to_owned()) && !secrets.contains(&"readonly-mode".to_owned()));
        let http = ServerSpec::Http { url: "u".into(), headers: vec![("Authorization".into(), "Bearer sekrit-value-1".into())] };
        assert!(secret_values(&http).contains(&"sekrit-value-1".to_owned()), "the token of a Bearer header alone too");
        assert_eq!(redact("{\"text\":\"t tok-1234567890 t\"}", &secrets).as_deref(), Some("{\"text\":\"t [redacted] t\"}"));
        assert_eq!(redact("clean", &secrets), None);
        let quoted = vec!["a\"b\\c-long-value".to_owned()];
        let text = serde_json::to_string(&json!({ "x": "a\"b\\c-long-value" })).unwrap();
        assert_eq!(redact(&text, &quoted).as_deref(), Some("{\"x\":\"[redacted]\"}"), "its JSON form too");
    }

    #[test]
    fn lines_are_messages_batches_are_split_and_banners_skipped() {
        let input = b"starting server v1\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n[{\"jsonrpc\":\"2.0\",\"method\":\"a\"},{\"jsonrpc\":\"2.0\",\"method\":\"b\"}]\n\n42\n{\"jsonrpc\":\"2.0\",\"method\":\"last\"}";
        let mut got = Vec::new();
        read_lines(&input[..], |m| {
            got.push(m);
            true
        });
        let methods: Vec<String> = got.iter().map(|m| m.get("method").and_then(Value::as_str).unwrap_or("response").to_owned()).collect();
        assert_eq!(methods, ["response", "a", "b", "last"]);
        let mut long = vec![b'x'; MAX_LINE + 10];
        long.extend_from_slice(b"\n{\"jsonrpc\":\"2.0\",\"method\":\"after\"}\n");
        let mut count = 0;
        read_lines(&long[..], |_| {
            count += 1;
            true
        });
        assert_eq!(count, 0, "a line over the bound ends the server's output");
    }
}
