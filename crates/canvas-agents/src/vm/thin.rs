//! The thin lane's hub side: every few seconds each running VM box's guest
//! is drained over the pack's `exec` hook (`atelier satellite --lite`): its
//! agent history goes into the hub's mirror (`<home>/vm/mirrors/<sandbox>/`,
//! read by the history index like the agent's own folder), and the canvas
//! commands its agents ran are run here, as that box, by this very program
//! (`atelier <words>` with the box's identity), so they follow exactly the
//! rules a local session's commands follow. Their answers go back with the
//! next drain.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use canvas_model::vm::{Blob, CliOp, OpAnswer, VmState};
use serde::{Deserialize, Serialize};

use super::lite::{DrainRequest, DrainResponse, FileState, Queued, RELAYED, valid_op_id};

/// Options a relayed command may never carry (they choose another home,
/// instance or canvas file of this machine).
const GLOBAL_OPTIONS: [&str; 6] = ["home", "instance", "sessions-dir", "cwd", "remote", "env"];

/// The hub's mirror of a sandbox's history.
pub fn mirror_dir(home: &Path, sandbox: &str) -> PathBuf {
    home.join("vm").join("mirrors").join(sandbox)
}

/// Every mirror's Copilot root (`<home>/vm/mirrors/<sandbox>/copilot`).
pub fn copilot_mirrors(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home.join("vm").join("mirrors")) else { return Vec::new() };
    let mut roots: Vec<PathBuf> = entries.flatten().map(|e| e.path().join("copilot")).filter(|p| p.is_dir()).collect();
    roots.sort();
    roots
}

/// What the hub has of a sandbox's history and the answers it owes it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct MirrorState {
    pub files: BTreeMap<String, FileState>,
    pub answers: Vec<(String, OpAnswer)>,
}

fn state_path(home: &Path, sandbox: &str) -> PathBuf {
    mirror_dir(home, sandbox).join("state.json")
}

/// Takes in a drain's history pieces: appends that continue the mirror,
/// whole files replaced, anything else (a name that is not a session's
/// file, a gap) left for the next drain to send again.
pub fn apply_history(home: &Path, sandbox: &str, state: &mut MirrorState, response: &DrainResponse) -> Result<usize> {
    use std::io::Write;
    let mut taken = 0;
    for piece in &response.history {
        let parts: Vec<&str> = piece.file.split('/').collect();
        let ok = canvas_model::vm::sync_name_ok(&piece.file)
            && matches!(parts.as_slice(), ["copilot", session, "events.jsonl" | "workspace.yaml"] if crate::index::is_session_id(session));
        if !ok {
            continue;
        }
        let path = mirror_dir(home, sandbox).join(&piece.file);
        let have = state.files.get(&piece.file).copied().unwrap_or_default();
        if piece.whole {
            crate::paths::write_atomic(&path, &piece.bytes)?;
            state.files.insert(piece.file.clone(), FileState { ident: piece.ident, offset: piece.bytes.len() as u64 });
        } else if piece.offset == 0 || (piece.ident == have.ident && piece.offset == have.offset) {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = if piece.offset == 0 { std::fs::File::create(&path)? } else { std::fs::OpenOptions::new().append(true).open(&path)? };
            file.write_all(&piece.bytes)?;
            state.files.insert(piece.file.clone(), FileState { ident: piece.ident, offset: piece.offset + piece.bytes.len() as u64 });
        } else {
            continue;
        }
        taken += 1;
    }
    Ok(taken)
}

/// The command line that runs a relayed operation here (`None`: refused, with why).
pub fn argv_of(op: &CliOp, files: &BTreeMap<String, PathBuf>, outputs: &[(String, PathBuf)]) -> Result<Vec<String>> {
    let verb = op.words.first().map(String::as_str).unwrap_or_default();
    anyhow::ensure!(RELAYED.contains(&verb), "`{verb}` is not run for VM sessions");
    anyhow::ensure!(!(op.words.len() >= 2 && op.words[0] == "note" && op.words[1] == "path"), "`note path` is not run for VM sessions");
    let name_ok = |n: &str| !n.is_empty() && n.len() <= 40 && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && !n.starts_with('-');
    let mut argv = Vec::new();
    for (i, word) in op.words.iter().enumerate() {
        anyhow::ensure!(!word.starts_with("--") && !word.contains('\0'), "a word that reads as an option");
        // `todo import FILE`: the file it carried.
        if i == 2 && op.words[..2] == ["todo", "import"] {
            argv.push(files.get(word).context("the imported file was not carried")?.display().to_string());
        } else {
            argv.push(word.clone());
        }
    }
    for (key, value) in &op.options {
        anyhow::ensure!(name_ok(key) && !GLOBAL_OPTIONS.contains(&key.as_str()), "option --{key} is not run for VM sessions");
        anyhow::ensure!(!value.contains('\0'), "a value with a NUL");
        let value = if super::lite::FILE_OPTIONS.contains(&key.as_str()) {
            files.get(value).with_context(|| format!("the file of --{key} was not carried"))?.display().to_string()
        } else if let Some((_, path)) = outputs.iter().find(|(name, _)| name == value).filter(|_| key == "canvas" || key == "out") {
            path.display().to_string()
        } else {
            value.clone()
        };
        argv.extend([format!("--{key}"), value]);
    }
    for flag in &op.flags {
        anyhow::ensure!(name_ok(flag), "flag --{flag} is not run for VM sessions");
        argv.push(format!("--{flag}"));
    }
    Ok(argv)
}

/// Runs a relayed operation here as box `term` (its agent `agent`): this
/// program with the operation's command line, the box's identity and this
/// service's home, the files it carried in a folder of their own.
pub fn execute(home: &Path, exe: &Path, op_id: &str, term: &str, agent: &str, op: &CliOp) -> OpAnswer {
    let refused = |text: String| OpAnswer { ok: false, state: "refused".into(), text, outputs: Vec::new() };
    let work = home.join("vm").join("ops").join("tmp").join(op_id);
    let result = (|| -> Result<OpAnswer> {
        std::fs::create_dir_all(&work)?;
        let mut files = BTreeMap::new();
        for (n, blob) in op.inputs.iter().enumerate() {
            let path = work.join(format!("in-{n}"));
            std::fs::write(&path, &blob.bytes)?;
            files.insert(blob.name.clone(), path);
        }
        let outputs: Vec<(String, PathBuf)> = super::lite::output_options(op).into_iter().enumerate().map(|(n, name)| (name, work.join(format!("out-{n}")))).collect();
        let argv = argv_of(op, &files, &outputs)?;
        let mut cmd = Command::new(exe);
        cmd.args(&argv).current_dir(&work).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for key in ["ATELIER_PASSIVE", "ATELIER_REMOTE", "ATELIER_HOP", "ATELIER_INSTANCE", "AUTOMATION_NAME", "AUTOMATION_STATUS_FILE"] {
            cmd.env_remove(key);
            if let Some(old) = canvas_common::env::legacy_name(key) {
                cmd.env_remove(old);
            }
        }
        cmd.env("ATELIER_HOME", home).env("ATELIER_SESSION", term);
        if let Some(kind) = canvas_model::agent::AgentKind::parse(agent) {
            cmd.envs(crate::agents::session_agent_env(kind));
        }
        canvas_sys::spawn::CommandExt::no_window(&mut cmd);
        let child = cmd.spawn().context("cannot run the command")?;
        let out = wait_with_timeout(child, Duration::from_secs(40))?;
        let text = if out.status.success() { String::from_utf8_lossy(&out.stdout).into_owned() } else { String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("Error: ").to_owned() };
        let outputs = outputs.iter().filter_map(|(name, path)| std::fs::read(path).ok().map(|bytes| Blob { name: name.clone(), bytes })).collect();
        Ok(OpAnswer { ok: out.status.success(), state: if out.status.success() { "applied".into() } else { "refused".into() }, text, outputs })
    })();
    let _ = std::fs::remove_dir_all(&work);
    result.unwrap_or_else(|error| refused(format!("{error:#}")))
}

fn wait_with_timeout(mut child: std::process::Child, limit: Duration) -> Result<std::process::Output> {
    use std::io::Read;
    let mut stdout = child.stdout.take().context("stdout")?;
    let mut stderr = child.stderr.take().context("stderr")?;
    let out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = (&mut stdout).take(4 << 20).read_to_end(&mut b);
        b
    });
    let err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = (&mut stderr).take(64 << 10).read_to_end(&mut b);
        b
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the command did not finish within {} s", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(std::process::Output { status, stdout: out.join().unwrap_or_default(), stderr: err.join().unwrap_or_default() })
}

/// The answer an operation already got (a drain may report it twice).
fn stored_answer(home: &Path, sandbox: &str, id: &str) -> Option<OpAnswer> {
    std::fs::read(home.join("vm").join("ops").join(sandbox).join(format!("{id}.json"))).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

/// One drain of one sandbox: history into the mirror, operations run as
/// their box (only boxes of this sandbox), answers queued for the next drain.
pub fn poll(home: &Path, exe: &Path, hooks: &super::hooks::Hooks, sandbox: &str, boxes: &HashMap<String, String>, log: &dyn Fn(String)) -> Result<()> {
    let path = state_path(home, sandbox);
    let mut state: MirrorState = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let request = DrainRequest { files: state.files.clone(), answers: state.answers.clone(), max_bytes: 4 << 20 };
    let (code, stdout) = hooks.exec(sandbox, &["atelier".into(), "satellite".into(), "--lite".into()], &serde_json::to_vec(&request)?)?;
    anyhow::ensure!(code == 0, "the guest's drain failed (exit code {code})");
    let response: DrainResponse = serde_json::from_slice(&stdout).context("the guest's drain printed no drain")?;
    // Delivered: the guest stored them.
    state.answers.clear();
    apply_history(home, sandbox, &mut state, &response)?;
    for Queued { id, term, op, .. } in response.ops {
        if !valid_op_id(&id) {
            continue;
        }
        let answer = match stored_answer(home, sandbox, &id) {
            Some(answer) => answer,
            None => {
                let answer = match boxes.get(&term) {
                    Some(agent) => {
                        let answer = execute(home, exe, &id, &term, agent, &op);
                        log(format!("VM {sandbox}: box {term} ran `atelier {}` ({})", op.words.first().cloned().unwrap_or_default(), answer.state));
                        answer
                    }
                    None => OpAnswer { ok: false, state: "refused".into(), text: "this command does not come from a session of this VM".into(), outputs: Vec::new() },
                };
                crate::paths::write_atomic(&home.join("vm").join("ops").join(sandbox).join(format!("{id}.json")), &serde_json::to_vec(&answer)?)?;
                answer
            }
        };
        state.answers.push((id, answer));
    }
    crate::paths::write_atomic(&path, &serde_json::to_vec(&state)?)?;
    Ok(())
}

/// The thin lane's loop in the service: drains every running VM box's
/// sandbox every `poll_seconds` of the pack (nothing without a pack).
pub fn run(home: PathBuf, log: impl Fn(String) + Send + 'static, wake: impl Fn() + Send + 'static) {
    let mut last: HashMap<String, Instant> = HashMap::new();
    let mut failing: HashMap<String, String> = HashMap::new();
    let exe = std::env::current_exe().unwrap_or_default();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let Some(folder) = super::pack::configured(&home) else { continue };
        let intents: Vec<_> = super::launch::all(&home).into_iter().filter(|i| !i.sandbox.is_empty() && i.state == VmState::Running).collect();
        if intents.is_empty() {
            continue;
        }
        let (pack, status) = super::pack::load(&folder);
        if status.errors() > 0 || !pack.hooks.contains_key("exec") {
            continue;
        }
        let hooks = super::hooks::Hooks::of(&pack);
        let mut sandboxes: HashMap<String, HashMap<String, String>> = HashMap::new();
        for intent in &intents {
            sandboxes.entry(intent.sandbox.clone()).or_default().insert(intent.r#box.clone(), intent.agent.clone());
        }
        let every = Duration::from_secs(pack.poll_seconds);
        for (sandbox, boxes) in sandboxes {
            if last.get(&sandbox).is_some_and(|at| at.elapsed() < every) {
                continue;
            }
            last.insert(sandbox.clone(), Instant::now());
            match poll(&home, &exe, &hooks, &sandbox, &boxes, &log) {
                Ok(()) => {
                    if failing.remove(&sandbox).is_some() {
                        log(format!("VM {sandbox}: reachable again"));
                    }
                    wake();
                }
                Err(error) => {
                    let text = format!("{error:#}");
                    if failing.insert(sandbox.clone(), text.clone()).as_ref() != Some(&text) {
                        log(format!("VM {sandbox}: cannot drain it: {}", super::hooks::first_line(&text)));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::lite::Piece;

    #[test]
    fn relayed_commands_become_this_programs_command_lines() {
        let files: BTreeMap<String, PathBuf> = [("-".to_owned(), PathBuf::from("/t/in-0"))].into_iter().collect();
        let op = CliOp { words: vec!["todo".into(), "add".into(), "Fix it".into()], options: vec![("type".into(), "bug".into()), ("body".into(), "-".into())], flags: vec!["mine".into()], inputs: vec![] };
        assert_eq!(argv_of(&op, &files, &[]).unwrap(), ["todo", "add", "Fix it", "--type", "bug", "--body", "/t/in-0", "--mine"]);
        let refuse = |op: CliOp| argv_of(&op, &files, &[]).unwrap_err().to_string();
        assert!(refuse(CliOp { words: vec!["service".into(), "stop".into()], ..Default::default() }).contains("not run"));
        assert!(refuse(CliOp { words: vec!["emit".into(), "--home".into()], ..Default::default() }).contains("option"));
        assert!(refuse(CliOp { words: vec!["emit".into()], options: vec![("home".into(), "/x".into())], ..Default::default() }).contains("--home"));
        assert!(refuse(CliOp { words: vec!["todo".into(), "add".into()], options: vec![("body".into(), "/etc/passwd".into())], ..Default::default() }).contains("not carried"), "never a file of this machine");
        assert!(refuse(CliOp { words: vec!["note".into(), "path".into()], ..Default::default() }).contains("note path"));
        let export = CliOp { words: vec!["todo".into(), "export".into()], options: vec![("canvas".into(), "/guest/b.canvas".into())], ..Default::default() };
        assert_eq!(argv_of(&export, &files, &[("/guest/b.canvas".into(), PathBuf::from("/t/out-0"))]).unwrap(), ["todo", "export", "--canvas", "/t/out-0"]);
    }

    #[test]
    fn mirrors_take_appends_that_continue_them_and_session_files_only() {
        let home = std::env::temp_dir().join(format!("atelier-mirror-{}", uuid::Uuid::new_v4()));
        let id = "12345678-1234-1234-1234-123456789abc";
        let piece = |file: &str, offset: u64, bytes: &[u8], whole: bool| Piece { file: file.into(), ident: 7, offset, whole, bytes: bytes.to_vec() };
        let events = format!("copilot/{id}/events.jsonl");
        let mut state = MirrorState::default();
        let response = DrainResponse {
            version: 1,
            history: vec![
                piece(&events, 0, b"a\n", false),
                piece(&format!("copilot/{id}/workspace.yaml"), 0, b"cwd: /w\n", true),
                piece(&format!("copilot/{id}/inuse.1.lock"), 0, b"", false),
                piece("copilot/../../x/events.jsonl", 0, b"x", false),
                piece("copilot/not-a-session/events.jsonl", 0, b"x", false),
            ],
            ops: vec![],
        };
        assert_eq!(apply_history(&home, "sbx-1", &mut state, &response).unwrap(), 2);
        let mirror = mirror_dir(&home, "sbx-1").join("copilot").join(id);
        assert_eq!(std::fs::read(mirror.join("events.jsonl")).unwrap(), b"a\n");
        assert!(!mirror.join("inuse.1.lock").exists() && !home.join("vm").join("x").exists());
        // A gap is not taken (the guest sends it again); a continuation is.
        let gap = DrainResponse { history: vec![piece(&events, 5, b"z\n", false)], ..Default::default() };
        assert_eq!(apply_history(&home, "sbx-1", &mut state, &gap).unwrap(), 0);
        let next = DrainResponse { history: vec![piece(&events, 2, b"b\n", false)], ..Default::default() };
        assert_eq!(apply_history(&home, "sbx-1", &mut state, &next).unwrap(), 1);
        assert_eq!(std::fs::read(mirror.join("events.jsonl")).unwrap(), b"a\nb\n");
        assert_eq!(copilot_mirrors(&home), [mirror_dir(&home, "sbx-1").join("copilot")]);
        let _ = std::fs::remove_dir_all(home);
    }
}
