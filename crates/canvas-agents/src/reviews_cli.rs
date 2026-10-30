//! `copilot-canvas review handoff|message|checked|request|done|list`
//! (WI-016): a review session hands a finished review off, the canvas-wide
//! review follow-up agent records its message to the author (draft, sent,
//! a problem), its hourly checks and the requests it finds. Each command
//! checks what it can and posts a request for the service (bus inbox, like
//! `copilot-canvas pr`), then waits for its answer while the service runs.
//! The service owns the records (`<home>/canvas-agents/<canvas id>/reviews/book.json`).

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::prs_cli::{Answer, ask};
use canvas_model::prs::PrRef;
use canvas_model::reviews::{Book, KINDS, MessageState, RequestState};

/// The bus event of a review request (handled by the service).
pub const REQUEST: &str = "canvas.review";
/// Longest text file a command reads.
const MAX_FILE: u64 = 64 * 1024;

/// Reads a text file, or standard input for `-`.
fn read_text(source: &str, what: &str) -> Result<String> {
    let mut bytes = Vec::new();
    if source == "-" {
        std::io::stdin().take(MAX_FILE + 1).read_to_end(&mut bytes).with_context(|| format!("cannot read the {what} from standard input"))?;
    } else {
        std::fs::File::open(source).and_then(|f| f.take(MAX_FILE + 1).read_to_end(&mut bytes)).with_context(|| format!("cannot read the {what} file {source}"))?;
    }
    if bytes.len() as u64 > MAX_FILE {
        bail!("the {what} is larger than {} KB: summarize it", MAX_FILE / 1024);
    }
    let text = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("the {what} is not UTF-8 text"))?;
    Ok(text.trim_start_matches('\u{feff}').to_owned())
}

fn answered(answer: Option<Answer>, pending: &str) -> Result<()> {
    match answer {
        Some(answer) if answer.ok => {
            println!("{}", answer.message);
            Ok(())
        }
        Some(answer) => bail!("Copilot Canvas refused: {}", answer.message),
        None => {
            println!("{pending}");
            Ok(())
        }
    }
}

/// `review handoff` options.
#[derive(Clone, Debug, Default)]
pub struct HandoffArgs {
    pub pr: Option<String>,
    pub author: Option<String>,
    pub author_id: Option<String>,
    /// What the author may read: a file, or `-`.
    pub results: Option<String>,
    /// Private notes (never sent to the author): a file.
    pub notes: Option<String>,
    pub outcome: Option<String>,
    pub title: Option<String>,
    /// `TITLE=URL` each.
    pub links: Vec<String>,
    pub session: Option<String>,
}

/// `review handoff --pr URL --author NAME (--results FILE|-) …`.
pub fn handoff(home: &Path, args: HandoffArgs) -> Result<()> {
    let usage = "usage: copilot-canvas review handoff --pr URL --author NAME [--author-id ID] --results FILE|- [--notes FILE] [--outcome approved|changes-requested|commented] [--link TITLE=URL]… [--session NAME|ID]";
    let pr = PrRef::parse(args.pr.as_deref().context(usage)?).map_err(|e| anyhow::anyhow!(e))?;
    let author = args.author.filter(|a| !a.trim().is_empty()).context(usage)?;
    let results = read_text(args.results.as_deref().context(usage)?, "results")?;
    if results.trim().is_empty() {
        bail!("the results are empty: write what the author may read (the outcome and the actionable findings)");
    }
    let notes = args.notes.as_deref().map(|n| read_text(n, "notes")).transpose()?.unwrap_or_default();
    let links: Vec<serde_json::Value> = args
        .links
        .iter()
        .map(|l| canvas_model::reports::parse_link(l).map(|l| serde_json::json!({ "title": l.title, "url": l.url })))
        .collect::<Result<_>>()?;
    if std::env::var("COPILOT_CANVAS_SESSION").unwrap_or_default().is_empty() && args.session.as_deref().is_none_or(|s| s.trim().is_empty()) {
        bail!("this is not a Copilot Canvas session: name the review session with --session \"<name or id>\"");
    }
    let data = serde_json::json!({
        "op": "handoff", "url": pr.url(), "author": author, "author_id": args.author_id.unwrap_or_default(), "results": results, "notes": notes,
        "outcome": args.outcome.unwrap_or_default(), "title": args.title.unwrap_or_default(), "links": links, "session": args.session.unwrap_or_default(),
    });
    answered(ask(home, REQUEST, data)?, &format!("Review of {} handed to Copilot Canvas; the service is not running (or busy): it is recorded when the service takes it.", pr.short()))
}

/// `review message` options: exactly one of a draft, sent, a problem.
#[derive(Clone, Debug, Default)]
pub struct MessageArgs {
    /// The draft: a file, or `-`.
    pub draft: Option<String>,
    pub sent: bool,
    pub conversation: Option<String>,
    pub recipient: Option<String>,
    pub needs_you: Option<String>,
    pub failed: Option<String>,
}

/// `review message REVIEW (--draft FILE|- | --sent --conversation ID [--recipient NAME] | --needs-you WHY | --failed WHY)`.
pub fn message(home: &Path, review: &str, args: MessageArgs) -> Result<()> {
    let chosen = [args.draft.is_some(), args.sent, args.needs_you.is_some(), args.failed.is_some()].iter().filter(|b| **b).count();
    if chosen != 1 || review.trim().is_empty() {
        bail!("usage: copilot-canvas review message REVIEW (--draft FILE|- | --sent --conversation ID [--recipient NAME] | --needs-you \"WHY\" | --failed \"WHY\")");
    }
    let data = if let Some(file) = &args.draft {
        serde_json::json!({ "op": "draft", "review": review, "text": read_text(file, "draft")? })
    } else if args.sent {
        let conversation = args.conversation.filter(|c| !c.trim().is_empty()).context("--sent needs --conversation ID (where the message went, as the channel names it)")?;
        serde_json::json!({ "op": "sent", "review": review, "conversation": conversation, "recipient": args.recipient.unwrap_or_default() })
    } else if let Some(why) = args.needs_you {
        serde_json::json!({ "op": "needs-you", "review": review, "reason": why })
    } else {
        serde_json::json!({ "op": "failed", "review": review, "reason": args.failed.unwrap_or_default() })
    };
    answered(ask(home, REQUEST, data)?, "Handed to Copilot Canvas; the service is not running (or busy): it is recorded when the service takes it.")
}

/// `review checked [--conversation ID] [--mark TOKEN] [--failed WHY]`.
pub fn checked(home: &Path, conversation: Option<String>, mark: Option<String>, failed: Option<String>) -> Result<()> {
    let data = serde_json::json!({ "op": "checked", "conversation": conversation.unwrap_or_default(), "mark": mark.unwrap_or_default(), "failed": failed.unwrap_or_default() });
    answered(ask(home, REQUEST, data)?, "Check handed to Copilot Canvas (the service is not running or busy).")
}

/// `review request --conversation ID --message-id ID --kind KIND --text TEXT [--pr URL]`.
pub fn request(home: &Path, conversation: Option<String>, message_id: Option<String>, kind: Option<String>, text: Option<String>, pr: Option<String>) -> Result<()> {
    let usage = format!("usage: copilot-canvas review request --conversation ID --message-id ID --kind {} --text \"<their words>\" [--pr URL]", KINDS.join("|"));
    let conversation = conversation.filter(|c| !c.trim().is_empty()).context(usage.clone())?;
    let message_id = message_id.filter(|c| !c.trim().is_empty()).context(usage.clone())?;
    let kind = kind.context(usage.clone())?;
    if !KINDS.contains(&kind.trim().to_ascii_lowercase().as_str()) {
        bail!("--kind is one of {} (other messages are not review requests: leave them)", KINDS.join(", "));
    }
    let pr = pr.map(|p| PrRef::parse(&p).map_err(|e| anyhow::anyhow!(e))).transpose()?.map(|p| p.url()).unwrap_or_default();
    let data = serde_json::json!({ "op": "request", "conversation": conversation, "message_id": message_id, "kind": kind, "text": text.unwrap_or_default(), "url": pr });
    answered(ask(home, REQUEST, data)?, "Request handed to Copilot Canvas; the service is not running (or busy): it is recorded when the service takes it.")
}

/// `review done REQUEST`: the review session finished it without a new hand-off.
pub fn done(home: &Path, request: &str) -> Result<()> {
    answered(ask(home, REQUEST, serde_json::json!({ "op": "done", "request": request }))?, "Handed to Copilot Canvas (the service is not running or busy).")
}

pub fn book_path(home: &Path, canvas: &str) -> PathBuf {
    crate::canvas_wide::canvas_dir(home, canvas).join("reviews").join("book.json")
}

/// The records as stored (read only; empty when missing).
pub fn load(home: &Path, canvas: &str) -> Book {
    std::fs::read(book_path(home, canvas)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// `review list [--format json]` (private notes are never printed).
pub fn list(home: &Path, json: bool) -> Result<String> {
    let canvas = crate::cli::routing(home).canvas;
    if !crate::doc::valid_canvas_id(&canvas) {
        bail!("no canvas with an id is open in Copilot Canvas (open it with an up-to-date client)");
    }
    let mut book = load(home, &canvas);
    for review in &mut book.reviews {
        review.notes.clear();
    }
    if json {
        return Ok(serde_json::to_string_pretty(&book)?);
    }
    let now = crate::paths::now_ms();
    let age = |ms: u64| if ms == 0 { "never".to_owned() } else if ms <= now { format!("{} ago", crate::paths::format_age(ms, now)) } else { format!("in {}", crate::paths::format_age(now, ms)) };
    let watched = book.watched().len();
    let mut out = vec![format!(
        "Copilot Canvas review follow-ups of canvas {canvas}: {} review(s), {watched} watched conversation(s) · last check {} · next {}{}",
        book.reviews.len(),
        age(book.checks.last_ms),
        if watched == 0 { "when a message was sent".to_owned() } else { age(book.checks.next_ms) },
        if book.checks.last_error.is_empty() { String::new() } else { format!(" · last failure: {}", book.checks.last_error) }
    )];
    for r in book.reviews.iter().rev() {
        let mark = match r.message.state {
            MessageState::Sent => "✓",
            MessageState::AwaitingApproval | MessageState::NeedsYou | MessageState::Failed => "⚠",
            _ => "◌",
        };
        out.push(format!("  {mark} {} {} · {} → {} · message {}{}  ({})", r.short, r.outcome, r.reviewer_name, r.author, r.message.state.word(), if r.message.reason.is_empty() { String::new() } else { format!(": {}", r.message.reason) }, r.id));
    }
    for q in book.requests.iter().rev().take(20) {
        let mark = match q.state {
            RequestState::Delivered | RequestState::Done => "✓",
            RequestState::Ambiguous | RequestState::Blocked | RequestState::Failed => "⚠",
            _ => "◌",
        };
        out.push(format!("  {mark} request {} {} {} → {}: \"{}\"{}  ({})", q.kind, q.state.word(), q.short, q.to_name, q.text, if q.reason.is_empty() { String::new() } else { format!(" ({})", q.reason) }, q.id));
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{self, Event};

    #[test]
    fn hand_offs_and_agent_records_are_checked_then_posted() {
        let home = std::env::temp_dir().join(format!("cc-reviews-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let results = home.join("results.md");
        std::fs::write(&results, "Changes requested: retries never stop.").unwrap();
        let notes = home.join("notes.md");
        std::fs::write(&notes, "private").unwrap();
        let base = HandoffArgs { pr: Some("https://github.com/o/r/pull/7".into()), author: Some("Ana".into()), results: Some(results.display().to_string()), session: Some("Reviewer".into()), ..Default::default() };
        assert!(handoff(&home, HandoffArgs { pr: Some("nope".into()), ..base.clone() }).unwrap_err().to_string().contains("not a pull request link"));
        assert!(handoff(&home, HandoffArgs { author: None, ..base.clone() }).unwrap_err().to_string().contains("usage"));
        assert!(handoff(&home, HandoffArgs { links: vec!["x=file:///c".into()], ..base.clone() }).is_err());
        handoff(&home, HandoffArgs { notes: Some(notes.display().to_string()), outcome: Some("changes-requested".into()), links: vec!["comment=https://github.com/o/r/pull/7#discussion_r1".into()], ..base }).unwrap();
        assert!(message(&home, "rv-1", MessageArgs { sent: true, ..Default::default() }).unwrap_err().to_string().contains("--conversation"));
        assert!(message(&home, "rv-1", MessageArgs { sent: true, failed: Some("x".into()), ..Default::default() }).unwrap_err().to_string().contains("usage"));
        message(&home, "rv-1", MessageArgs { sent: true, conversation: Some("outbox:ana".into()), recipient: Some("Ana".into()), ..Default::default() }).unwrap();
        assert!(request(&home, Some("outbox:ana".into()), Some("m1".into()), Some("lunch".into()), Some("?".into()), None).unwrap_err().to_string().contains("--kind is one of"));
        request(&home, Some("outbox:ana".into()), Some("m1".into()), Some("re-review".into()), Some("please re-review".into()), None).unwrap();
        let events: Vec<Event> = std::fs::read_dir(bus::inbox_dir(&home)).unwrap().flatten().map(|f| serde_json::from_slice(&std::fs::read(f.path()).unwrap()).unwrap()).collect();
        assert_eq!(events.len(), 3, "refused ones are never posted");
        let handed = events.iter().find(|e| e.data["op"] == "handoff").unwrap();
        assert_eq!((handed.kind.as_str(), handed.data["notes"].as_str(), handed.data["links"][0]["title"].as_str()), (REQUEST, Some("private"), Some("comment")));
        assert!(events.iter().any(|e| e.data["op"] == "sent" && e.data["conversation"] == "outbox:ana"));
        assert!(events.iter().any(|e| e.data["op"] == "request" && e.data["kind"] == "re-review"));
        let _ = std::fs::remove_dir_all(home);
    }
}
