//! The pack's hooks (`hooks/<name>`): external commands for the thin lane
//! (docs/VM-SESSIONS.md §10, Phase M). Each reads one JSON object on its
//! standard input and prints one JSON object (`{"ok": true, …}` or
//! `{"ok": false, "error": "…"}`) on its standard output. They run as you,
//! with a time limit; their output is never logged (it may name the pack's
//! infrastructure), only the first line of an error, shortened.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

/// Largest output a hook may print (an `exec` drain carries history).
const MAX_STDOUT: u64 = 24 << 20;
const MAX_STDERR: u64 = 64 << 10;

/// How long each hook may take by default.
pub fn timeout(name: &str) -> Duration {
    Duration::from_secs(match name {
        "claim" | "image" => 600,
        "resume" | "suspend" | "delete" => 180,
        _ => 120,
    })
}

/// The command that runs a hook file on this machine.
fn command(path: &Path) -> Command {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase());
    match ext.as_deref() {
        Some("ps1") => {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]).arg(path);
            c
        }
        // No arguments follow: nothing for cmd.exe to re-parse.
        Some("cmd" | "bat") => {
            let mut c = Command::new("cmd");
            c.args(["/d", "/c"]).arg(path);
            c
        }
        Some("sh") => {
            let mut c = Command::new("sh");
            c.arg(path);
            c
        }
        _ => Command::new(path),
    }
}

/// The first line of `text`, at most 200 characters (for errors and logs).
pub fn first_line(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let short: String = line.chars().take(200).collect();
    if short.len() < line.len() { format!("{short}…") } else { short }
}

/// Runs hook `name` (its file `path`) with `input`; its answer object.
pub fn run(name: &str, path: &Path, input: &Value, limit: Duration, env: &[(String, String)]) -> Result<Value> {
    let mut cmd = command(path);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    canvas_sys::spawn::CommandExt::no_window(&mut cmd);
    let mut child = cmd.spawn().with_context(|| format!("cannot run the {name} hook"))?;
    let body = serde_json::to_vec(input)?;
    let mut stdin = child.stdin.take().context("hook stdin")?;
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&body);
    });
    let mut stdout = child.stdout.take().context("hook stdout")?;
    let mut stderr = child.stderr.take().context("hook stderr")?;
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stdout).take(MAX_STDOUT + 1).read_to_end(&mut bytes);
        // Drain the rest so the hook never blocks on a full pipe.
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stderr).take(MAX_STDERR).read_to_end(&mut bytes);
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        bytes
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = canvas_sys::process::kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            bail!("the {name} hook did not finish within {} s", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let _ = writer.join();
    let stdout = out.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
    if stdout.len() as u64 > MAX_STDOUT {
        bail!("the {name} hook printed more than {} MB", MAX_STDOUT >> 20);
    }
    let answer: Option<Value> = std::str::from_utf8(&stdout).ok().and_then(|text| text.lines().rev().map(str::trim).find(|l| l.starts_with('{')).and_then(|l| serde_json::from_str(l).ok()));
    match answer {
        Some(answer) if answer["ok"] == Value::Bool(true) && status.success() => Ok(answer),
        Some(answer) if answer["ok"] == Value::Bool(false) => Err(anyhow!("the {name} hook failed: {}", first_line(answer["error"].as_str().unwrap_or("no reason given")))),
        _ if !status.success() => Err(anyhow!("the {name} hook failed ({status}): {}", first_line(&stderr))),
        _ => Err(anyhow!("the {name} hook printed no answer (one JSON object with \"ok\")")),
    }
}

/// The hooks of a pack.
#[derive(Clone, Debug, Default)]
pub struct Hooks {
    pub files: std::collections::BTreeMap<String, PathBuf>,
    /// Environment every hook gets (the pack's folder).
    pub env: Vec<(String, String)>,
}

impl Hooks {
    pub fn of(pack: &super::pack::Pack) -> Self {
        Hooks { files: pack.hooks.clone(), env: vec![("ATELIER_VM_PACK".into(), pack.folder.display().to_string())] }
    }

    pub fn has(&self, name: &str) -> bool {
        self.files.contains_key(name)
    }

    pub fn call(&self, name: &str, input: &Value) -> Result<Value> {
        let path = self.files.get(name).with_context(|| format!("the pack has no {name} hook"))?;
        run(name, path, input, timeout(name), &self.env)
    }

    /// `exec`: runs `argv` in the sandbox with `stdin`; (exit code, stdout).
    pub fn exec(&self, sandbox: &str, argv: &[String], stdin: &[u8]) -> Result<(i64, Vec<u8>)> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let answer = self.call("exec", &serde_json::json!({ "sandbox": sandbox, "argv": argv, "stdin": b64.encode(stdin) }))?;
        let stdout = b64.decode(answer["stdout"].as_str().unwrap_or_default()).map_err(|_| anyhow!("the exec hook's stdout is not base64"))?;
        Ok((answer["code"].as_i64().unwrap_or(0), stdout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hook script for this machine that prints `body` (and `exit`s with `code`).
    fn hook(dir: &Path, name: &str, body: &str, code: i32) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        if cfg!(windows) {
            let path = dir.join(format!("{name}.cmd"));
            std::fs::write(&path, format!("@echo off\r\nmore > nul\r\necho {body}\r\nexit /b {code}\r\n")).unwrap();
            path
        } else {
            let path = dir.join(format!("{name}.sh"));
            std::fs::write(&path, format!("cat > /dev/null\necho '{body}'\nexit {code}\n")).unwrap();
            path
        }
    }

    #[test]
    fn hooks_answer_json_and_fail_with_their_first_line() {
        let dir = std::env::temp_dir().join(format!("atelier-hooks-{}", uuid::Uuid::new_v4()));
        let ok = hook(&dir, "claim", r#"{"ok": true, "sandbox": "sbx-1"}"#, 0);
        let answer = run("claim", &ok, &serde_json::json!({"box": "b"}), Duration::from_secs(30), &[]).unwrap();
        assert_eq!(answer["sandbox"], "sbx-1");
        let refused = hook(&dir, "resume", r#"{"ok": false, "error": "quota reached"}"#, 0);
        assert!(run("resume", &refused, &Value::Null, Duration::from_secs(30), &[]).unwrap_err().to_string().ends_with("quota reached"));
        let silent = hook(&dir, "delete", "not json", 0);
        assert!(run("delete", &silent, &Value::Null, Duration::from_secs(30), &[]).unwrap_err().to_string().contains("no answer"));
        let failed = hook(&dir, "suspend", "nothing", 3);
        assert!(run("suspend", &failed, &Value::Null, Duration::from_secs(30), &[]).unwrap_err().to_string().contains("failed"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_hanging_hook_is_ended() {
        let dir = std::env::temp_dir().join(format!("atelier-hooks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image.sh");
        std::fs::write(&path, "sleep 30\n").unwrap();
        let started = Instant::now();
        assert!(run("image", &path, &Value::Null, Duration::from_millis(500), &[]).unwrap_err().to_string().contains("did not finish"));
        assert!(started.elapsed() < Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn errors_are_one_short_line() {
        assert_eq!(first_line("\n  boom: x\nsecond"), "boom: x");
        assert_eq!(first_line(&"y".repeat(300)).chars().count(), 201);
    }
}
