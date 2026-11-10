//! The thin lane's guest side (`ATELIER_REMOTE=satellite-lite`): canvas
//! commands an agent runs in the guest are relayed to the hub as
//! [`CliOp`]s through an outbox the hub polls (`atelier satellite --lite`
//! over the pack's `exec` hook), and the guest's agent history is shipped
//! to the hub's mirror in the same poll. Nothing here touches the hub: the
//! hub reads what the drain prints and writes back answers.
//!
//! Writes never claim success they did not get: a command waits a few
//! seconds for the hub's answer and otherwise says it is queued.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use canvas_model::vm::{Blob, CliOp, OpAnswer};
use serde::{Deserialize, Serialize};

use crate::paths::now_ms;

/// The verbs relayed to the hub (everything else is refused in a VM session).
pub const RELAYED: [&str; 10] = ["emit", "send", "status", "followup", "effect", "report", "pr", "review", "todo", "note"];
/// Options whose value is a file the command reads (`-`: standard input).
pub const FILE_OPTIONS: [&str; 8] = ["body", "summary", "results", "notes", "draft", "markdown", "html", "md"];
/// Largest file a relayed command may carry.
pub const MAX_INPUT: u64 = 4 << 20;
/// How long a relayed command waits for the hub's answer.
const WAIT: Duration = Duration::from_secs(5);

/// The relay mode of this process (`ATELIER_REMOTE`).
pub fn mode() -> Option<String> {
    std::env::var("ATELIER_REMOTE").ok().filter(|m| m.starts_with("satellite"))
}

/// A relayed operation in the guest's outbox.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Queued {
    pub id: String,
    /// The box the command ran in (its `ATELIER_SESSION`; the hub checks it
    /// belongs to this VM).
    pub term: String,
    pub op: CliOp,
    pub ms: u64,
}

pub fn outbox_dir(home: &Path) -> PathBuf {
    home.join("vm").join("outbox")
}

pub fn answers_dir(home: &Path) -> PathBuf {
    home.join("vm").join("answers")
}

/// Whether an op id is safe as a file name.
pub fn valid_op_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The operation a guest command asks for: its words, options and flags, and
/// the files it reads inlined (`stdin` for `-`). Refused when the verb is not
/// one the hub runs for VM sessions.
pub fn op_of(words: &[&str], options: &[(String, String)], flags: &[String], stdin: &mut dyn Read) -> Result<CliOp> {
    let verb = words.first().copied().unwrap_or_default();
    if !RELAYED.contains(&verb) || words.get(..2) == Some(&["effect", "list"]) || words == ["effect"] {
        bail!("`atelier {verb}` is not available inside a VM session (it acts on the canvas's machine)");
    }
    if words.get(..2) == Some(&["note", "path"]) {
        bail!("`atelier note path` names folders of the canvas's machine; write notes with `atelier note new … --md FILE` (they are filed on the canvas's machine)");
    }
    let mut inputs = Vec::new();
    let mut read = |name: &str| -> Result<()> {
        if inputs.iter().any(|b: &Blob| b.name == name) {
            return Ok(());
        }
        let mut bytes = Vec::new();
        if name == "-" {
            (&mut *stdin).take(MAX_INPUT + 1).read_to_end(&mut bytes)?;
        } else {
            std::fs::File::open(name).and_then(|f| f.take(MAX_INPUT + 1).read_to_end(&mut bytes)).with_context(|| format!("cannot read {name}"))?;
        }
        anyhow::ensure!(bytes.len() as u64 <= MAX_INPUT, "{name} is larger than {} MB", MAX_INPUT >> 20);
        inputs.push(Blob { name: name.to_owned(), bytes });
        Ok(())
    };
    for (key, value) in options {
        if FILE_OPTIONS.contains(&key.as_str()) {
            read(value)?;
        }
    }
    if words.get(..2) == Some(&["todo", "import"])
        && let Some(file) = words.get(2)
    {
        read(file)?;
    }
    Ok(CliOp { words: words.iter().map(|w| w.to_string()).collect(), options: options.to_vec(), flags: flags.to_vec(), inputs })
}

/// The files a command writes where it runs (`todo export --canvas FILE`, `todo mirror --out FILE`).
pub fn output_options(op: &CliOp) -> Vec<String> {
    let todo = op.words.first().map(String::as_str) == Some("todo");
    op.options.iter().filter(|(k, _)| todo && (k == "canvas" || k == "out")).map(|(_, v)| v.clone()).collect()
}

/// Whether the guest's satellite is linked to the hub, and since when it is
/// not (`None`: no satellite here, the hooks lane, which polls).
pub fn link_state(home: &Path) -> Option<(bool, u64)> {
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(home.join("vm").join("link.json")).ok()?).ok()?;
    Some((value["linked"] == true, value["since_ms"].as_u64().unwrap_or(0)))
}

/// The satellite says whether it is linked (for the guest's commands).
pub fn set_link_state(home: &Path, linked: bool) {
    let _ = crate::paths::write_atomic(&home.join("vm").join("link.json"), serde_json::json!({ "linked": linked, "since_ms": now_ms() }).to_string().as_bytes());
}

/// Commands that only read the canvas: when the hub is away they read its
/// last copy in the guest (the `canvas` root) instead.
pub fn is_read(words: &[&str], options: &[(String, String)]) -> bool {
    match words {
        ["todo", "prime" | "boards" | "list" | "show" | "ready" | "export", ..] => true,
        ["todo", "mirror", ..] => !options.iter().any(|(k, _)| k == "set"),
        _ => false,
    }
}

/// `HH:MM UTC` of a time.
fn clock(ms: u64) -> String {
    canvas_common::paths::format_rfc3339_ms(ms).get(11..16).map(|t| format!("{t} UTC")).unwrap_or_default()
}

/// A read run here on the canvas's last copy (`<home>/vm/roots/canvas`, a
/// home of its own), said to be one.
fn read_copy(home: &Path, words: &[&str], options: &[(String, String)], flags: &[String], since: u64) -> Result<String> {
    let copy = home.join("vm").join("roots").join("canvas");
    anyhow::ensure!(copy.join("bus").join("routing.json").is_file(), "the canvas is not reachable and its todos have not reached this VM yet");
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(words);
    for (key, value) in options {
        cmd.args([format!("--{key}"), value.clone()]);
    }
    for flag in flags {
        cmd.arg(format!("--{flag}"));
    }
    cmd.env("ATELIER_HOME", &copy).env_remove("ATELIER_REMOTE").stdin(std::process::Stdio::null());
    canvas_sys::spawn::CommandExt::no_window(&mut cmd);
    let out = cmd.output().context("cannot read the canvas's copy")?;
    let note = if since > 0 { format!("(the canvas is not reachable since {}: this is its last copy)", clock(since)) } else { "(the canvas is not reachable: this is its last copy)".to_owned() };
    if !out.status.success() {
        bail!("{}\n{note}", String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("atelier: "));
    }
    Ok(format!("{}\n{note}", String::from_utf8_lossy(&out.stdout).trim_end()))
}

/// Relays a command from the guest: queues it, waits for the hub's answer,
/// writes the files it produced here; what to print (`Err`: a refusal).
/// With a satellite that is not linked, a read answers from the canvas's last
/// copy and a write says at once that it is queued.
pub fn relay(home: &Path, words: &[&str], options: &[(String, String)], flags: &[String]) -> Result<String> {
    let offline = link_state(home).filter(|(linked, _)| !linked).map(|(_, since)| since);
    let read = is_read(words, options);
    if let Some(since) = offline
        && read
    {
        return read_copy(home, words, options, flags, since);
    }
    let op = op_of(words, options, flags, &mut std::io::stdin().lock())?;
    let term = canvas_common::env::var("ATELIER_SESSION").unwrap_or_default();
    let id = uuid::Uuid::new_v4().to_string();
    let queued = Queued { id: id.clone(), term, op: op.clone(), ms: now_ms() };
    crate::paths::write_atomic(&outbox_dir(home).join(format!("{id}.json")), &serde_json::to_vec(&queued)?)?;
    let answer_path = answers_dir(home).join(format!("{id}.json"));
    if let Some(since) = offline {
        return Ok(format!("Queued (the canvas is not reachable since {}): `atelier {}` is applied when it is back; its answer is not known yet.", clock(since), words.join(" ")));
    }
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if let Ok(bytes) = std::fs::read(&answer_path) {
            let _ = std::fs::remove_file(&answer_path);
            let answer: OpAnswer = serde_json::from_slice(&bytes).context("unreadable answer from the canvas")?;
            for (name, out) in output_options(&op).iter().zip(&answer.outputs) {
                crate::paths::write_atomic(Path::new(name), &out.bytes).with_context(|| format!("cannot write {name}"))?;
            }
            if answer.ok {
                return Ok(answer.text.trim_end().to_owned());
            }
            bail!("{}", answer.text.trim_end());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if read && link_state(home).is_some() {
        // Asked of a hub that did not answer in time: the copy, said to be one (the request is dropped).
        let _ = std::fs::remove_file(outbox_dir(home).join(format!("{id}.json")));
        return read_copy(home, words, options, flags, 0);
    }
    Ok(format!("Queued (not applied yet): the canvas applies `atelier {}` when it next reaches this VM; its answer is not known yet.", words.join(" ")))
}

/// A history file's identity and how much of it the hub has.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct FileState {
    pub ident: u64,
    pub offset: u64,
}

/// What the hub sends with each drain.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct DrainRequest {
    /// `copilot/<session>/<file>` → what the hub has of it.
    pub files: std::collections::BTreeMap<String, FileState>,
    /// Answers to earlier operations.
    pub answers: Vec<(String, OpAnswer)>,
    pub max_bytes: u64,
    /// The operations reach the hub another way (the satellite pushes them): history only.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub skip_ops: bool,
}

/// A piece of a history file.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Piece {
    pub file: String,
    pub ident: u64,
    pub offset: u64,
    /// The whole file (rewritten files such as `workspace.yaml`), not an append.
    pub whole: bool,
    #[serde(with = "b64")]
    pub bytes: Vec<u8>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct DrainResponse {
    pub version: u32,
    pub history: Vec<Piece>,
    pub ops: Vec<Queued>,
}

mod b64 {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        base64::engine::general_purpose::STANDARD.decode(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// The guest's Copilot history folder (`COPILOT_HOME`, else `~/.copilot`).
pub fn copilot_root() -> PathBuf {
    std::env::var_os("COPILOT_HOME").map(PathBuf::from).unwrap_or_else(|| crate::agents::home().join(".copilot")).join("session-state")
}

/// A file's identity: its inode where there is one, else its creation time.
fn ident(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
    }
    #[cfg(not(unix))]
    {
        meta.created().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64)
    }
}

/// `atelier satellite --lite`: stores the hub's answers, then reports what
/// the hub does not have yet of the agents' history and the queued
/// operations (at most `max_bytes` of history a drain).
pub fn drain(home: &Path, history: &Path, request: &DrainRequest) -> Result<DrainResponse> {
    for (id, answer) in &request.answers {
        if valid_op_id(id) {
            answered(home, id, answer)?;
        }
    }
    let budget = if request.max_bytes == 0 { 4 << 20 } else { request.max_bytes.min(16 << 20) };
    let mut used = 0u64;
    let mut pieces = Vec::new();
    let mut sessions: Vec<PathBuf> = std::fs::read_dir(history).map(|e| e.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default();
    sessions.sort();
    'sessions: for dir in sessions {
        let Some(session) = dir.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|s| crate::index::is_session_id(s)) else { continue };
        for (name, whole) in [("workspace.yaml", true), ("events.jsonl", false)] {
            let path = dir.join(name);
            let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
            if !meta.is_file() {
                continue;
            }
            let file = format!("copilot/{session}/{name}");
            let have = request.files.get(&file).copied().unwrap_or_default();
            let size = meta.len();
            let (ident, start) = if whole {
                // Rewritten in place: its modification time and size say whether the hub has it.
                let stamp = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64);
                if have.ident == stamp && have.offset == size {
                    continue;
                }
                (stamp, 0)
            } else {
                let id = ident(&meta);
                if have.ident == id && have.offset >= size {
                    continue;
                }
                (id, if have.ident == id { have.offset } else { 0 })
            };
            let left = budget.saturating_sub(used);
            if left == 0 {
                break 'sessions;
            }
            let take = if whole { size } else { (size - start).min(left) };
            if whole && size > left && used > 0 {
                break 'sessions;
            }
            let mut bytes = Vec::with_capacity(take as usize);
            let mut f = std::fs::File::open(&path)?;
            use std::io::Seek;
            f.seek(std::io::SeekFrom::Start(start))?;
            f.take(take).read_to_end(&mut bytes)?;
            used += bytes.len() as u64;
            pieces.push(Piece { file, ident, offset: start, whole, bytes });
        }
    }
    let ops = if request.skip_ops { Vec::new() } else { queued(home, 64) };
    Ok(DrainResponse { version: 1, history: pieces, ops })
}

/// The guest's outbox, oldest first (at most `max`).
pub fn queued(home: &Path, max: usize) -> Vec<Queued> {
    let mut ops: Vec<Queued> = std::fs::read_dir(outbox_dir(home))
        .map(|e| e.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).filter_map(|e| std::fs::read(e.path()).ok()).filter_map(|b| serde_json::from_slice::<Queued>(&b).ok()).collect())
        .unwrap_or_default();
    ops.retain(|q| valid_op_id(&q.id));
    ops.sort_by_key(|q| q.ms);
    ops.truncate(max);
    ops
}

/// The hub's answer to operation `id`: kept for the command that waits for
/// it, the operation taken out of the outbox.
pub fn answered(home: &Path, id: &str, answer: &OpAnswer) -> Result<()> {
    anyhow::ensure!(valid_op_id(id), "not an operation id");
    crate::paths::write_atomic(&answers_dir(home).join(format!("{id}.json")), &serde_json::to_vec(answer)?)?;
    let _ = std::fs::remove_file(outbox_dir(home).join(format!("{id}.json")));
    Ok(())
}

/// Answers nobody read (their command stopped waiting), a day after they came.
pub fn tidy_answers(home: &Path) {
    let Ok(entries) = std::fs::read_dir(answers_dir(home)) else { return };
    let day = std::time::SystemTime::now() - Duration::from_secs(86_400);
    for entry in entries.flatten() {
        if entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < day) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relayed_commands_carry_their_files_and_other_verbs_are_refused() {
        let dir = std::env::temp_dir().join(format!("atelier-lite-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let summary = dir.join("summary.md");
        std::fs::write(&summary, "done").unwrap();
        let options = vec![("pr".to_owned(), "https://github.com/o/r/pull/1".to_owned()), ("summary".to_owned(), summary.display().to_string()), ("body".to_owned(), "-".to_owned())];
        let op = op_of(&["todo", "submit", "DEC-1"], &options, &[], &mut &b"from stdin"[..]).unwrap();
        assert_eq!(op.inputs.iter().map(|b| (b.name.clone(), b.bytes.clone())).collect::<Vec<_>>(), [(summary.display().to_string(), b"done".to_vec()), ("-".to_owned(), b"from stdin".to_vec())]);
        for words in [&["service", "stop"][..], &["remote", "pair"], &["vm", "pack", "check"], &["effect", "list"], &["note", "path"], &["config"]] {
            assert!(op_of(words, &[], &[], &mut &b""[..]).is_err(), "{words:?}");
        }
        assert!(op_of(&["todo", "add", "x"], &[("body".into(), dir.join("missing").display().to_string())], &[], &mut &b""[..]).is_err());
        let export = CliOp { words: vec!["todo".into(), "export".into()], options: vec![("canvas".into(), "/w/b.canvas".into())], ..Default::default() };
        assert_eq!(output_options(&export), ["/w/b.canvas"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn drains_ship_what_the_hub_lacks_and_take_answers() {
        let root = std::env::temp_dir().join(format!("atelier-drain-{}", uuid::Uuid::new_v4()));
        let (home, history) = (root.join("home"), root.join("history"));
        let id = "12345678-1234-1234-1234-123456789abc";
        std::fs::create_dir_all(history.join(id)).unwrap();
        std::fs::create_dir_all(history.join("not-a-session")).unwrap();
        std::fs::write(history.join(id).join("events.jsonl"), "{\"type\":\"user.message\"}\n").unwrap();
        std::fs::write(history.join(id).join("workspace.yaml"), "cwd: /w\n").unwrap();
        std::fs::write(history.join(id).join("inuse.7.lock"), "").unwrap();
        let queued = Queued { id: "op-1".into(), term: id.into(), op: CliOp { words: vec!["emit".into(), "x".into()], ..Default::default() }, ms: 1 };
        crate::paths::write_atomic(&outbox_dir(&home).join("op-1.json"), &serde_json::to_vec(&queued).unwrap()).unwrap();
        let first = drain(&home, &history, &DrainRequest::default()).unwrap();
        assert_eq!(first.history.iter().map(|p| p.file.as_str()).collect::<Vec<_>>(), [format!("copilot/{id}/workspace.yaml"), format!("copilot/{id}/events.jsonl")], "never lock files");
        assert_eq!(first.ops, vec![queued]);
        // The hub has it all: nothing again; an append ships only the new part.
        let mut have = DrainRequest::default();
        for p in &first.history {
            have.files.insert(p.file.clone(), FileState { ident: p.ident, offset: p.offset + p.bytes.len() as u64 });
        }
        have.answers.push(("op-1".into(), OpAnswer { ok: true, state: "applied".into(), text: "Sent x".into(), ..Default::default() }));
        assert!(drain(&home, &history, &have).unwrap().history.is_empty());
        assert!(!outbox_dir(&home).join("op-1.json").exists() && answers_dir(&home).join("op-1.json").is_file(), "answered: out of the outbox");
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(history.join(id).join("events.jsonl")).unwrap().write_all(b"{\"type\":\"session.idle\"}\n").unwrap();
        let next = drain(&home, &history, &have).unwrap();
        assert_eq!(next.history.len(), 1);
        assert_eq!((next.history[0].offset, next.history[0].bytes.as_slice()), (24, &b"{\"type\":\"session.idle\"}\n"[..]));
        // A budget splits large appends across drains.
        let small = DrainRequest { max_bytes: 5, ..Default::default() };
        let pieces = drain(&home, &history, &small).unwrap().history;
        assert!(pieces.iter().map(|p| p.bytes.len()).sum::<usize>() <= 8 + 5, "{pieces:?}");
        let _ = std::fs::remove_dir_all(root);
    }
}
