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

/// Every mirror's Claude Code root (`<home>/vm/mirrors/<sandbox>/claude`).
pub fn claude_mirrors(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home.join("vm").join("mirrors")) else { return Vec::new() };
    let mut roots: Vec<PathBuf> = entries.flatten().map(|e| e.path().join("claude")).filter(|p| p.is_dir()).collect();
    roots.sort();
    roots
}

/// A history file a guest may send (its name in the mirror): Copilot's
/// `copilot/<session>/events.jsonl|workspace.yaml`; Claude Code's
/// `claude/projects/<dir>/<session>.jsonl`, its subagents'
/// `claude/projects/<dir>/<session>/subagents/<agent>.jsonl|.meta.json` and
/// its status files `claude/sessions/<pid>.json`.
pub fn mirror_name_ok(file: &str) -> bool {
    use crate::index::is_session_id;
    let dir_ok = |d: &str| !d.is_empty() && d.len() <= 200 && d.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let agent_ok = |n: &str| n.strip_suffix(".jsonl").or_else(|| n.strip_suffix(".meta.json")).is_some_and(|stem| !stem.is_empty() && stem.len() <= 80 && stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    let parts: Vec<&str> = file.split('/').collect();
    canvas_model::vm::sync_name_ok(file)
        && match parts.as_slice() {
            ["copilot", session, "events.jsonl" | "workspace.yaml"] => is_session_id(session),
            ["claude", "projects", dir, name] => dir_ok(dir) && name.strip_suffix(".jsonl").is_some_and(is_session_id),
            ["claude", "projects", dir, session, "subagents", name] => dir_ok(dir) && is_session_id(session) && agent_ok(name),
            ["claude", "sessions", name] => name.strip_suffix(".json").is_some_and(|pid| pid.parse::<u32>().is_ok()),
            _ => false,
        }
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

/// Drops a mirror's record of the files under `prefix` (they moved to
/// another sandbox's mirror: a resume on a new VM).
pub fn forget_files(home: &Path, sandbox: &str, prefix: &str) {
    let path = state_path(home, sandbox);
    let Some(mut state) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<MirrorState>(&b).ok()) else { return };
    state.files.retain(|name, _| !name.starts_with(prefix));
    if let Ok(bytes) = serde_json::to_vec(&state) {
        let _ = crate::paths::write_atomic(&path, &bytes);
    }
}

/// Takes in a drain's history pieces: appends that continue the mirror,
/// whole files replaced, anything else (a name that is not a session's
/// file, a gap) left for the next drain to send again.
pub fn apply_history(home: &Path, sandbox: &str, state: &mut MirrorState, response: &DrainResponse) -> Result<usize> {
    use std::io::Write;
    let mut taken = 0;
    for piece in &response.history {
        if !mirror_name_ok(&piece.file) {
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
pub fn stored_answer(home: &Path, sandbox: &str, id: &str) -> Option<OpAnswer> {
    std::fs::read(home.join("vm").join("ops").join(sandbox).join(format!("{id}.json"))).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

/// Keeps operation `id`'s answer, so it is never run twice.
pub fn store_answer(home: &Path, sandbox: &str, id: &str, answer: &OpAnswer) -> Result<()> {
    crate::paths::write_atomic(&home.join("vm").join("ops").join(sandbox).join(format!("{id}.json")), &serde_json::to_vec(answer)?)?;
    Ok(())
}

/// A drain through the provider: `atelier satellite --lite` run in the guest.
pub fn exec_drain(provider: &dyn super::provider::Provider, sandbox: &str, request: &DrainRequest) -> Result<DrainResponse> {
    let out = provider.exec(sandbox, &["atelier".into(), "satellite".into(), "--lite".into()], &serde_json::to_vec(request)?)?;
    anyhow::ensure!(out.code == 0, "the guest's drain failed (exit code {})", out.code);
    serde_json::from_slice(&out.stdout).context("the guest's drain printed no drain")
}

/// One drain of one sandbox (through `drain`: the satellite's link, else
/// the provider): history into the mirror, operations run as their box
/// (only boxes of this sandbox), answers queued for the next drain.
pub fn poll(home: &Path, exe: &Path, drain: &dyn Fn(&DrainRequest) -> Result<DrainResponse>, sandbox: &str, boxes: &HashMap<String, String>, log: &dyn Fn(String)) -> Result<()> {
    let path = state_path(home, sandbox);
    let mut state: MirrorState = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let request = DrainRequest { files: state.files.clone(), answers: state.answers.clone(), max_bytes: 4 << 20, skip_ops: false };
    let response = drain(&request)?;
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
                store_answer(home, sandbox, &id, &answer)?;
                answer
            }
        };
        state.answers.push((id, answer));
    }
    crate::paths::write_atomic(&path, &serde_json::to_vec(&state)?)?;
    Ok(())
}

/// The thin lane's drains, run by the service's VM loop: each running VM
/// box's sandbox every `poll_seconds` of the pack.
#[derive(Default)]
pub struct Drains {
    last: HashMap<String, Instant>,
    failing: HashMap<String, String>,
}

impl Drains {
    /// Drains what is due; true when something was drained (the index wakes).
    /// A sandbox whose satellite is linked is drained through the link; the
    /// hooks' sandboxes through the provider; another provider's sandbox
    /// waits for its satellite (no command per poll on a cloud's API).
    pub fn run(&mut self, home: &Path, pack: &super::pack::Pack, provider: &dyn super::provider::Provider, links: &super::link::Links, log: &dyn Fn(String)) -> bool {
        let exe = std::env::current_exe().unwrap_or_default();
        let mut sandboxes: HashMap<String, HashMap<String, String>> = HashMap::new();
        for intent in super::launch::all(home).into_iter().filter(|i| !i.sandbox.is_empty() && i.state == VmState::Running) {
            sandboxes.entry(intent.sandbox.clone()).or_default().insert(intent.r#box.clone(), intent.agent.clone());
        }
        self.last.retain(|s, _| sandboxes.contains_key(s));
        let every = Duration::from_secs(pack.poll_seconds);
        let mut drained = false;
        for (sandbox, boxes) in sandboxes {
            if self.last.get(&sandbox).is_some_and(|at| at.elapsed() < every) {
                continue;
            }
            let linked = links.linked(&sandbox);
            if !linked && provider.name() != "hooks" {
                continue;
            }
            self.last.insert(sandbox.clone(), Instant::now());
            // Through the link the satellite pushes the operations itself (`SatUp::Op`): history only.
            let drain = |request: &DrainRequest| if linked { links.drain(&sandbox, &DrainRequest { skip_ops: true, ..request.clone() }) } else { exec_drain(provider, &sandbox, request) };
            match poll(home, &exe, &drain, &sandbox, &boxes, log) {
                Ok(()) => {
                    if self.failing.remove(&sandbox).is_some() {
                        log(format!("VM {sandbox}: reachable again"));
                    }
                    drained = true;
                }
                Err(error) => {
                    let text = format!("{error:#}");
                    if self.failing.insert(sandbox.clone(), text.clone()).as_ref() != Some(&text) {
                        log(format!("VM {sandbox}: cannot drain it: {}", super::hooks::first_line(&text)));
                    }
                }
            }
        }
        drained
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::lite::Piece;

    /// The CLI inventory (plan §4), row by row: what an agent runs in a VM
    /// session's guest becomes, on this machine, the same command line it
    /// would run here (its files carried, its outputs written back), or is
    /// refused there with why; static text runs in the guest.
    #[test]
    fn every_row_of_the_cli_inventory_relays_as_the_same_command() {
        use super::super::lite::{Here, op_of, route};
        let dir = std::env::temp_dir().join(format!("atelier-inventory-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            path.display().to_string()
        };
        let (body, summary, results, notes, draft, md, html, import) = (file("body.md", "b"), file("summary.md", "s"), file("results.md", "r"), file("notes.md", "n"), file("draft.md", "d"), file("note.md", "m"), file("r.html", "h"), file("import.json", "[]"));
        let o = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        let f = |flags: &[&str]| flags.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // (words, options, flags) as typed in the guest; the command line here is the same with carried files.
        type Row<'a> = (Vec<&'a str>, Vec<(String, String)>, Vec<String>);
        let rows: Vec<Row> = vec![
            (vec!["events"], o(&[]), f(&[])),
            (vec!["emit", "deploy.done"], o(&[("data", "{\"ok\":true}"), ("to", "Reviewer"), ("group", "g1")]), f(&[])),
            (vec!["send", "Hello there"], o(&[("to", "Reviewer")]), f(&[])),
            (vec!["status", "ok", "All green"], o(&[]), f(&[])),
            (vec!["followup", "Check the build"], o(&[("in", "30m")]), f(&[])),
            (vec!["followup", "Standup"], o(&[("at", "09:00")]), f(&[])),
            (vec!["effect", "pulse"], o(&[("on", "Reviewer"), ("note", "look")]), f(&[])),
            (vec!["effect", "clear"], o(&[]), f(&[])),
            (vec!["report", "publish"], o(&[("markdown", &body), ("title", "Done"), ("kind", "summary"), ("pr", "https://example.com/pr/1")]), f(&[])),
            (vec!["report", "publish"], o(&[("html", &html)]), f(&[])),
            (vec!["report", "publish"], o(&[("message", "Short note")]), f(&[])),
            (vec!["report", "list"], o(&[("limit", "5")]), f(&["unread"])),
            (vec!["pr", "track", "https://example.com/contoso/app/pull/7"], o(&[("title", "Fix")]), f(&[])),
            (vec!["pr", "untrack", "https://example.com/contoso/app/pull/7"], o(&[]), f(&[])),
            (vec!["pr", "check"], o(&[]), f(&[])),
            (vec!["pr", "list"], o(&[("format", "json")]), f(&[])),
            (vec!["review", "handoff"], o(&[("pr", "https://example.com/pr/1"), ("author", "Ana"), ("results", &results), ("notes", &notes)]), f(&[])),
            (vec!["review", "message", "RV-1"], o(&[("draft", &draft)]), f(&[])),
            (vec!["review", "list"], o(&[]), f(&[])),
            (vec!["todo", "prime"], o(&[]), f(&[])),
            (vec!["todo", "boards"], o(&[]), f(&["tree"])),
            (vec!["todo", "list"], o(&[("board", "Canvas App")]), f(&["mine"])),
            (vec!["todo", "show", "WI-1"], o(&[]), f(&[])),
            (vec!["todo", "ready"], o(&[]), f(&[])),
            (vec!["todo", "add", "Fix it"], o(&[("type", "bug"), ("body", &body)]), f(&[])),
            (vec!["todo", "claim", "WI-1"], o(&[]), f(&[])),
            (vec!["todo", "submit", "WI-1"], o(&[("summary", &summary), ("pr", "https://example.com/pr/1")]), f(&[])),
            (vec!["todo", "note", "WI-1", "Halfway"], o(&[]), f(&[])),
            (vec!["todo", "import", &import], o(&[("board", "Canvas App")]), f(&[])),
            (vec!["todo", "export"], o(&[("canvas", "/home/agent/b.canvas")]), f(&[])),
            (vec!["todo", "mirror"], o(&[("out", "/home/agent/todos.md")]), f(&[])),
            (vec!["note", "new"], o(&[("title", "Plan"), ("md", &md), ("audience", "ai")]), f(&[])),
            (vec!["note", "list"], o(&[("format", "json")]), f(&["internal"])),
        ];
        for (words, options, flags) in rows {
            assert_eq!(route(&words, &options), Here::Relay, "{words:?}");
            let op = op_of(&words, &options, &flags, &mut std::io::empty()).unwrap_or_else(|e| panic!("{words:?}: {e}"));
            let files: BTreeMap<String, PathBuf> = op.inputs.iter().enumerate().map(|(n, b)| (b.name.clone(), PathBuf::from(format!("/hub/in-{n}")))).collect();
            let outputs: Vec<(String, PathBuf)> = super::super::lite::output_options(&op).into_iter().enumerate().map(|(n, name)| (name, PathBuf::from(format!("/hub/out-{n}")))).collect();
            let argv = argv_of(&op, &files, &outputs).unwrap_or_else(|e| panic!("{words:?}: {e}"));
            // The same words, options and flags, a carried file or output in place of each guest path.
            let mut expected: Vec<String> = words.iter().map(|w| files.get(*w).map(|p| p.display().to_string()).unwrap_or_else(|| w.to_string())).collect();
            for (key, value) in &options {
                let carried = files.get(value).or_else(|| outputs.iter().find(|(n, _)| n == value).map(|(_, p)| p)).filter(|_| key != "data" && key != "to" && key != "title" && key != "pr");
                expected.extend([format!("--{key}"), carried.map(|p| p.display().to_string()).unwrap_or_else(|| value.clone())]);
            }
            expected.extend(flags.iter().map(|f| format!("--{f}")));
            assert_eq!(argv, expected, "{words:?}");
            assert!(op.inputs.iter().all(|b| !b.bytes.is_empty()), "{words:?}: files carried with their content");
        }
        // Static text runs in the guest; commands of the canvas's machine are refused there with why.
        assert_eq!(route(&["effect"], &[]), Here::Normal);
        assert_eq!(route(&["effect", "list"], &[]), Here::Normal);
        let refused = |words: &[&str], options: &[(String, String)]| op_of(words, options, &[], &mut std::io::empty()).unwrap_err().to_string();
        for words in [&["service", "stop"][..], &["remote", "pair"], &["vm", "list"], &["automations", "list"], &["config", "vm.pack"], &["doctor"], &["record"], &["corpus", "check"], &["selftest"]] {
            assert!(refused(words, &[]).contains("not available inside a VM session"), "{words:?}");
        }
        assert!(refused(&["todo", "mirror"], &o(&[("set", "/home/agent/m.md")])).contains("set it from the canvas"));
        assert!(refused(&["todo", "add", "x"], &o(&[("body", "/no/such/file.md")])).contains("cannot read"));
        let _ = std::fs::remove_dir_all(dir);
    }

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
