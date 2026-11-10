//! VM spikes (docs/VM-SESSIONS.md §10, Phase 0): checks only real
//! sandboxes can answer, run by the owner with a real pack (`atelier vm spike
//! s2|s3|s4`), each printing one JSON object with no value of the pack. The
//! same commands run on `vm::local` in `atelier selftest vm`, so their code is
//! checked in CI. S1, S5 and S7 come with the satellite link, the MCP gateway
//! and the failure injection (docs/VM-STATUS.md).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use std::path::Path;

use super::pack::Pack;
use super::provider::{Labels, Provider};

/// `atelier satellite --self-check`: what the guest's Atelier needs works
/// (a PTY with the alacritty engine, its data folder); one JSON object.
pub fn self_check(home: &std::path::Path) -> Value {
    let started = Instant::now();
    let home_ok = std::fs::create_dir_all(home).is_ok() && std::fs::write(home.join(".self-check"), b"ok").is_ok() && std::fs::remove_file(home.join(".self-check")).is_ok();
    let pty = pty_check().map_err(|e| format!("{e:#}"));
    json!({
        "ok": home_ok && pty.is_ok(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "build": canvas_common::paths::build_id(),
        "home_writable": home_ok,
        "pty": pty.as_ref().map(|_| "ok".to_owned()).unwrap_or_else(|e| e.clone()),
        "ms": started.elapsed().as_millis() as u64,
    })
}

/// A shell in a PTY with the alacritty engine prints a line we can read back.
fn pty_check() -> Result<()> {
    use canvas_host::emulator::Engine;
    use canvas_host::term::{Hooks as TermHooks, Spawn, Terminal};
    let (pump, _pump_rx) = crossbeam_channel::bounded(1);
    let (exited, _exited_rx) = crossbeam_channel::unbounded();
    let (clipboard, _clip) = crossbeam_channel::bounded(1);
    let (changed, _changed) = crossbeam_channel::bounded(16);
    let (program, args): (&str, Vec<String>) = if cfg!(windows) { ("cmd", vec!["/d".into(), "/c".into(), "echo atelier-pty-ok".into()]) } else { ("sh", vec!["-c".into(), "echo atelier-pty-ok; sleep 1".into()]) };
    let cwd = std::env::temp_dir().display().to_string();
    let terminal = Terminal::spawn(
        Spawn { id: "self-check", program, args: &args, env: &[], cwd: &cwd, cols: 80, rows: 10, copilot: false, engine: Engine::Alacritty },
        TermHooks { pump, exited, clipboard, bytes: Default::default(), changed, notify: None },
    )?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if terminal.snapshot().is_some_and(|s| s.text().contains("atelier-pty-ok")) {
            terminal.force_kill();
            return Ok(());
        }
        if Instant::now() > deadline {
            terminal.force_kill();
            bail!("no output from a shell in a PTY within 10 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn p(values: &mut [u64], q: f64) -> u64 {
    values.sort_unstable();
    values.get(((values.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(0)
}

/// Makes a sandbox for a spike through the pack's provider (the pack's
/// first repository, its default size; a `hooks` claim clones it on
/// `atelier/spike`), runs `body` in it, deletes it unless `keep`.
fn with_sandbox(pack: &Pack, provider: &dyn Provider, keep: bool, body: impl FnOnce(&str) -> Result<Value>) -> Result<Value> {
    let repo = pack.repos.first().context("the pack has no repository")?;
    let size = pack.size(None).context("the pack has no size")?;
    let mut labels: Labels = [("atelier.role", "spike".to_owned()), ("atelier.box", format!("spike-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])), ("atelier.repo", repo.key.clone())].into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    if provider.name() == "hooks" {
        for (k, v) in [("atelier.url", &repo.url), ("atelier.branch", &"atelier/spike".to_owned()), ("atelier.base", &repo.default_branch), ("atelier.size", &size.name)] {
            labels.insert(k.into(), v.clone());
        }
    }
    let started = Instant::now();
    let sandbox = provider.create(&super::manager::spec(pack, &size.name, None, labels)?)?.id;
    let claim_ms = started.elapsed().as_millis() as u64;
    let result = body(&sandbox);
    if !keep {
        let _ = provider.delete(&sandbox);
    }
    let mut value = result?;
    value["claim_ms"] = claim_ms.into();
    value["provider"] = provider.name().into();
    Ok(value)
}

/// `atelier vm spike NAME [--agent A] [--runs N] [--keep]`: one JSON object.
pub fn run(home: &Path, pack: &Pack, name: &str, agent: Option<&str>, runs: u32, keep: bool) -> Result<Value> {
    let provider = super::open_provider(home, pack)?;
    let provider = provider.as_ref();
    if provider.name() == "hooks" {
        let hooks = super::hooks::Hooks::of(pack);
        anyhow::ensure!(hooks.has("claim") && hooks.has("exec") && hooks.has("delete"), "the spikes need the pack's claim, exec and delete hooks");
    }
    let exec = |sandbox: &str, argv: &[&str]| -> Result<(i64, String)> {
        let out = provider.exec(sandbox, &argv.iter().map(|s| s.to_string()).collect::<Vec<_>>(), b"")?;
        Ok((out.code, String::from_utf8_lossy(&out.stdout).into_owned()))
    };
    match name {
        // S2: the agent CLI runs in a guest (its sign-in is checked by starting a VM box).
        "s2" => {
            let agent = agent.unwrap_or("copilot");
            anyhow::ensure!(canvas_model::agent::AgentKind::parse(agent).is_some(), "--agent is copilot, opencode or claude");
            with_sandbox(pack, provider, keep, |sandbox| {
                let started = Instant::now();
                let (code, out) = exec(sandbox, &[agent, "--version"])?;
                Ok(json!({ "spike": "s2", "agent": agent, "ok": code == 0, "exit_code": code, "version": super::hooks::first_line(&out), "ms": started.elapsed().as_millis() as u64 }))
            })
        }
        // S3: this build runs in the stock image (the pack's satellite uploaded first).
        "s3" => with_sandbox(pack, provider, keep, |sandbox| {
            let uploaded = super::manager::install_satellite(pack, provider, sandbox)?;
            let (code, out) = exec(sandbox, &["atelier", "satellite", "--self-check"])?;
            let check: Value = serde_json::from_str(out.trim()).unwrap_or_else(|_| json!({ "ok": false, "output": super::hooks::first_line(&out) }));
            Ok(json!({ "spike": "s3", "ok": code == 0 && check["ok"] == true, "exit_code": code, "uploaded": uploaded, "self_check": check }))
        }),
        // S4 (without a model): claim to a command answered in the guest, p50 and p95.
        "s4" => {
            let runs = runs.clamp(1, 50);
            let mut times = Vec::new();
            for _ in 0..runs {
                let started = Instant::now();
                with_sandbox(pack, provider, false, |sandbox| {
                    let (code, _) = exec(sandbox, &["atelier", "--version"])?;
                    anyhow::ensure!(code == 0, "atelier did not answer in the guest");
                    Ok(json!({}))
                })?;
                times.push(started.elapsed().as_millis() as u64);
            }
            let (p50, p95) = (p(&mut times.clone(), 0.5), p(&mut times, 0.95));
            Ok(json!({ "spike": "s4", "ok": true, "provider": provider.name(), "runs": runs, "p50_ms": p50, "p95_ms": p95, "target": "first prompt p50 <= 5 s, p95 <= 10 s (this is claim to a guest command; the prompt adds the agent's start)" }))
        }
        "s1" | "s5" | "s7" => bail!("spike {name} needs the satellite link (later phases; docs/VM-STATUS.md)"),
        other => bail!("unknown spike {other:?} (s2, s3, s4)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        assert_eq!(p(&mut [5, 1, 3], 0.5), 3);
        assert_eq!(p(&mut [10, 20, 30, 40, 50, 60, 70, 80, 90, 100], 0.95), 100);
        assert_eq!(p(&mut [], 0.5), 0);
    }

    #[test]
    fn this_build_passes_its_own_self_check() {
        let home = std::env::temp_dir().join(format!("atelier-self-check-{}", uuid::Uuid::new_v4()));
        let check = self_check(&home);
        assert_eq!(check["pty"], "ok", "{check}");
        assert_eq!(check["ok"], true, "{check}");
        let _ = std::fs::remove_dir_all(home);
    }
}
