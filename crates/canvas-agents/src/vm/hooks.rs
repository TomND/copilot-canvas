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

/// The pack's hooks as a [`Provider`](super::provider::Provider) (`provider:
/// hooks`): `claim` makes a sandbox with the repository on its branch (the
/// labels `atelier.repo`, `atelier.url`, `atelier.branch`, `atelier.base`
/// say which), `exec` runs commands, files go through `atelier satellite
/// --write-file / --read-file` in the guest, `suspend`, `resume`, `delete`
/// and `image` map to their hooks. A hooks pack has no warm pool and lists
/// nothing (the store knows its sandboxes).
pub struct HooksProvider {
    pub hooks: Hooks,
}

impl super::provider::Provider for HooksProvider {
    fn name(&self) -> &'static str {
        "hooks"
    }

    fn create(&self, spec: &super::provider::SandboxSpec) -> Result<super::provider::Sandbox> {
        let label = |k: &str| spec.labels.get(k).cloned().unwrap_or_default();
        let answer = self.call(
            "claim",
            &serde_json::json!({
                "box": label("atelier.box"), "repo": label("atelier.repo"), "url": label("atelier.url"), "branch": label("atelier.branch"), "base": label("atelier.base"),
                "size": label("atelier.size"), "cpu": format!("{}m", spec.cpu_milli), "memory": format!("{}Mi", spec.memory_mi), "image": spec.image,
                "labels": spec.labels.iter().filter(|(k, _)| *k != "atelier.url").collect::<std::collections::BTreeMap<_, _>>(),
            }),
        )?;
        let id = answer["sandbox"].as_str().filter(|s| !s.is_empty()).context("the claim hook named no sandbox")?.to_owned();
        let mut labels = spec.labels.clone();
        if let Some(cwd) = answer["cwd"].as_str() {
            labels.insert("atelier.cwd".into(), cwd.to_owned());
        }
        Ok(super::provider::Sandbox { id, state: "Running".into(), labels })
    }

    fn get(&self, id: &str) -> Result<Option<super::provider::Sandbox>> {
        Ok(Some(super::provider::Sandbox { id: id.to_owned(), state: "Running".into(), labels: Default::default() }))
    }

    fn list(&self, _labels: &super::provider::Labels) -> Result<Vec<super::provider::Sandbox>> {
        Ok(Vec::new())
    }

    fn exec(&self, id: &str, argv: &[String], stdin: &[u8]) -> Result<super::provider::ExecResult> {
        let (code, stdout) = Hooks::exec(&self.hooks, id, argv, stdin)?;
        Ok(super::provider::ExecResult { code, stdout, stderr: String::new() })
    }

    fn put_file(&self, id: &str, path: &str, bytes: &[u8], mode: u32) -> Result<()> {
        let argv = ["atelier", "satellite", "--write-file", path, "--mode", &format!("{mode:o}")].map(str::to_owned);
        let (code, _) = Hooks::exec(&self.hooks, id, &argv, bytes)?;
        anyhow::ensure!(code == 0, "cannot write {path} in the guest (exit code {code})");
        Ok(())
    }

    fn get_file(&self, id: &str, path: &str, max: u64) -> Result<Vec<u8>> {
        let (code, bytes) = Hooks::exec(&self.hooks, id, &["atelier".into(), "satellite".into(), "--read-file".into(), path.to_owned()], b"")?;
        anyhow::ensure!(code == 0, "cannot read {path} in the guest");
        anyhow::ensure!(bytes.len() as u64 <= max, "{path} is larger than {max} bytes");
        Ok(bytes)
    }

    fn suspend(&self, id: &str) -> Result<()> {
        self.call("suspend", &serde_json::json!({ "sandbox": id })).map(|_| ())
    }

    fn resume(&self, id: &str) -> Result<()> {
        self.call("resume", &serde_json::json!({ "sandbox": id })).map(|_| ())
    }

    fn delete(&self, id: &str) -> Result<()> {
        self.call("delete", &serde_json::json!({ "sandbox": id })).map(|_| ())
    }

    fn set_egress(&self, _id: &str, _policy: &super::provider::EgressPolicy) -> Result<()> {
        // A hooks pack applies its own egress in its claim hook.
        Ok(())
    }

    fn egress_decisions(&self, _id: &str) -> Result<Vec<super::provider::EgressDecision>> {
        Ok(Vec::new())
    }

    fn image_build(&self, base: &str, name: &str) -> Result<super::provider::ImageRef> {
        let answer = self.call("image", &serde_json::json!({ "base": base, "name": name }))?;
        Ok(super::provider::ImageRef { id: answer["image"].as_str().unwrap_or(name).to_owned(), name: name.to_owned(), created_ms: crate::paths::now_ms() })
    }

    fn image_commit(&self, _id: &str, _name: &str) -> Result<super::provider::ImageRef> {
        bail!("a hooks pack builds its images in its image hook")
    }

    fn image_delete(&self, _image: &super::provider::ImageRef) -> Result<()> {
        Ok(())
    }

    fn images(&self) -> Result<Vec<super::provider::ImageRef>> {
        Ok(Vec::new())
    }

    fn secret_set(&self, _name: &str, _key: &str, _value: &super::provider::Secret) -> Result<()> {
        bail!("a hooks pack keeps its secrets itself")
    }
}

impl HooksProvider {
    fn call(&self, name: &str, input: &Value) -> Result<Value> {
        self.hooks.call(name, input)
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
