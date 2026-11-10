//! The secrets refresher (docs/VM-SESSIONS.md §5, `secrets.yaml`): every
//! group secret the pack lists is set on the provider from its command's
//! standard output, at the start and then on its interval. The value is
//! held only in memory as a [`Secret`] (zeroized when dropped), never logged,
//! never written to disk; it reaches guests only through egress injection.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::pack::{Pack, SecretSpec};
use super::provider::{Provider, Secret};

/// Largest value a secret's command may print.
const MAX_VALUE: u64 = 64 << 10;
/// How long a secret's command may take.
const LIMIT: Duration = Duration::from_secs(60);

/// Runs a secret's command (as the user, through the platform's shell): its
/// standard output, trimmed. Its standard error is dropped (it may echo the value).
pub fn value(spec: &SecretSpec) -> Result<Secret> {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/d", "/c", &spec.command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", &spec.command]);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    canvas_sys::spawn::CommandExt::no_window(&mut cmd);
    let mut child = cmd.spawn().with_context(|| format!("cannot run the command of secret {}", spec.name))?;
    let mut stdout = child.stdout.take().context("stdout")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stdout).take(MAX_VALUE + 1).read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("the command of secret {} did not finish within {} s", spec.name, LIMIT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut bytes = reader.join().unwrap_or_default();
    anyhow::ensure!(status.success(), "the command of secret {} failed ({status})", spec.name);
    anyhow::ensure!(bytes.len() as u64 <= MAX_VALUE, "the command of secret {} printed more than {} KB", spec.name, MAX_VALUE >> 10);
    while bytes.last().is_some_and(|b| b.is_ascii_whitespace()) {
        bytes.pop();
    }
    let start = bytes.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(bytes.len());
    let value = Secret::new(bytes.split_off(start));
    for byte in bytes.iter_mut() {
        *byte = 0;
    }
    anyhow::ensure!(!value.expose().is_empty(), "the command of secret {} printed nothing", spec.name);
    Ok(value)
}

/// When each secret was last set.
#[derive(Default)]
pub struct Refresher {
    last: HashMap<String, Instant>,
    failing: HashMap<String, String>,
}

impl Refresher {
    /// Sets every secret that is due on `provider` (a hooks pack keeps its secrets itself).
    pub fn run(&mut self, pack: &Pack, provider: &dyn Provider, log: &dyn Fn(String)) {
        if provider.name() == "hooks" {
            return;
        }
        self.last.retain(|name, _| pack.secrets.iter().any(|s| s.name == *name));
        for spec in &pack.secrets {
            let every = Duration::from_millis(spec.every_ms.max(60_000));
            if self.last.get(&spec.name).is_some_and(|at| at.elapsed() < every) {
                continue;
            }
            self.last.insert(spec.name.clone(), Instant::now());
            match value(spec).and_then(|value| provider.secret_set(&spec.name, &spec.key, &value)) {
                Ok(()) => {
                    if self.failing.remove(&spec.name).is_some() {
                        log(format!("VM secret {}: set again", spec.name));
                    }
                }
                Err(error) => {
                    let text = super::hooks::first_line(&format!("{error:#}"));
                    if self.failing.insert(spec.name.clone(), text.clone()).as_ref() != Some(&text) {
                        log(format!("VM secret {}: {text}", spec.name));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &str) -> SecretSpec {
        SecretSpec { name: "forge-token".into(), command: command.into(), key: "value".into(), every_ms: 3_600_000 }
    }

    #[test]
    fn values_come_from_standard_output_and_are_never_logged() {
        let print = if cfg!(windows) { "echo  s3cret-value " } else { "printf '  s3cret-value \\n'" };
        assert_eq!(value(&spec(print)).unwrap().expose(), b"s3cret-value");
        let quiet = if cfg!(windows) { "echo s3cret-value 1>&2" } else { "echo s3cret-value >&2" };
        assert!(value(&spec(quiet)).unwrap_err().to_string().contains("printed nothing"));
        assert!(value(&spec("exit 3")).unwrap_err().to_string().contains("failed"));
        let provider = super::super::fake::FakeProvider::new("fake");
        let pack = Pack { secrets: vec![spec(print), SecretSpec { name: "broken".into(), ..spec("exit 1") }], ..Default::default() };
        let logged = std::sync::Mutex::new(Vec::new());
        let log = |line: String| logged.lock().unwrap().push(line);
        let mut refresher = Refresher::default();
        refresher.run(&pack, &provider, &log);
        refresher.run(&pack, &provider, &log);
        assert_eq!(provider.calls().iter().filter(|c| *c == "secret_set forge-token").count(), 1, "once per interval");
        let lines = logged.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "one line for the failing secret: {lines:?}");
        assert!(lines.iter().all(|l| !l.contains("s3cret")));
        let hooks = super::super::fake::FakeProvider::new("hooks");
        Refresher::default().run(&pack, &hooks, &log);
        assert!(hooks.calls().is_empty(), "a hooks pack keeps its secrets itself");
    }
}
