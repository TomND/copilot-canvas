//! Review follow-ups in the service (WI-016): the routed canvas's records
//! (`reviews/book.json`, one writer: this service), requests from
//! `copilot-canvas review …` and windows, the events for the review
//! follow-up agent (`review.completed`, `review.message.approved`, the
//! hourly `review.check_due`), and the routing of authors' requests to the
//! review session that did the review: queued while it works, typed in when
//! it is idle (the mailbox), blocked while it cannot get them, one at a time
//! per pull request, every change reported in the inbox.

use std::path::Path;

use super::prs::{elsewhere, reach, running, who, wide};
use super::{Shared, lock, log};
use crate::bus::{BusState, Event, Mail, PassiveSpec, Routing, Target};
use crate::prs_cli::{Answer, write_answer};
use crate::reviews_cli::REQUEST;
use canvas_model::prs::{PrRef, excerpt};
use canvas_model::reviews::{self, Book, FollowRequest, Handoff, Link, MessageState, RequestState, Requested, Review, ReviewSettings, ReviewsView};

/// Seconds between checks instead of the agent's minutes (tests).
const CHECK_SECS_ENV: &str = "COPILOT_CANVAS_REVIEW_CHECK_SECS";

/// The routed canvas's records, held by the service, their only writer.
#[derive(Default)]
pub(super) struct Hub {
    canvas: String,
    book: Book,
    saved: String,
}

impl Hub {
    fn open(&mut self, home: &Path, canvas: &str) {
        if self.canvas == canvas {
            return;
        }
        self.save(home);
        self.canvas = canvas.to_owned();
        self.book = if crate::doc::valid_canvas_id(canvas) { crate::reviews_cli::load(home, canvas) } else { Book::default() };
        self.saved = serde_json::to_string_pretty(&self.book).unwrap_or_default();
    }

    fn save(&mut self, home: &Path) {
        if !crate::doc::valid_canvas_id(&self.canvas) {
            return;
        }
        let text = serde_json::to_string_pretty(&self.book).unwrap_or_default();
        if text != self.saved && crate::paths::write_atomic(&crate::reviews_cli::book_path(home, &self.canvas), text.as_bytes()).is_ok() {
            self.saved = text;
        }
    }
}

/// The canvas-wide agents with review follow-ups that run (routed: enabled; not paused).
pub(super) fn agents(routing: &Routing) -> Vec<&PassiveSpec> {
    routing.passive.iter().filter(|p| p.canvas_wide && !p.paused && p.reviews.is_some()).collect()
}

/// The agent that sends the messages and checks the conversations, with its settings.
fn agent(routing: &Routing) -> Option<(&PassiveSpec, &ReviewSettings)> {
    agents(routing).into_iter().find_map(|p| p.reviews.as_ref().map(|s| (p, s)))
}

fn interval_ms(settings: &ReviewSettings) -> u64 {
    if let Some(secs) = std::env::var(CHECK_SECS_ENV).ok().and_then(|s| s.parse::<u64>().ok()).filter(|s| *s > 0) {
        return secs * 1_000;
    }
    settings.check_minutes.max(1) as u64 * 60_000
}

/// The Reports thread of a pull request's review follow-ups.
fn thread(pr: &str) -> String {
    canvas_model::prs::thread_key(pr).replacen("pr:", "review:", 1)
}

pub(super) fn is_request(event: &Event) -> bool {
    event.kind == REQUEST
}

/// Requests only a window may make (`ClientMsg::Bus`): an approval. The bus
/// loop ignores them in the inbox, where any process (an agent too) can drop a file.
pub(super) fn window_only(event: &Event) -> bool {
    is_request(event) && event.data["op"] == "approve"
}

/// A report about a review follow-up (on behalf of its agent).
struct Note {
    kind: &'static str,
    title: String,
    body: String,
    pr: String,
    url: String,
    session: String,
}

fn report(shared: &Shared, routing: &Routing, note: Note, now: u64) {
    let agent = agent(routing).map(|(p, _)| p).or_else(|| routing.passive.iter().find(|p| p.canvas_wide && p.reviews.is_some()));
    let notice = crate::reports::Notice {
        title: note.title,
        body: note.body,
        kind: note.kind.into(),
        source: agent.map(|a| a.session_id.clone()).unwrap_or_default(),
        passive: agent.map(|a| a.key.clone()).unwrap_or_default(),
        source_name: "Review follow-ups".into(),
        session: note.session,
        pr: note.url,
        thread: thread(&note.pr),
        ..Default::default()
    };
    if let Err(error) = crate::reports::notice(&shared.instance.home, notice, now) {
        log(&shared.instance, &format!("cannot report a review follow-up: {error:#}"));
    }
}

/// Handles a request (`copilot-canvas review …`, a window's button); answers
/// it for a waiting command. Returns events to route.
pub(super) fn request(shared: &Shared, routing: &Routing, state: &mut BusState, event: &Event, now: u64) -> Vec<Event> {
    let mut out = Vec::new();
    let mut notes = Vec::new();
    let op = event.data["op"].as_str().unwrap_or_default().to_owned();
    let answer = handle(shared, routing, state, event, now, &mut out, &mut notes).unwrap_or_else(|why| Answer { ok: false, message: why, state: String::new() });
    log(&shared.instance, &format!("review {op} from {}: {}{}", who(routing, &event.source), if answer.ok { "" } else { "refused: " }, answer.message));
    write_answer(&shared.instance.home, event.data["request"].as_str().unwrap_or_default(), &answer);
    for note in notes {
        report(shared, routing, note, now);
    }
    out
}

fn handle(shared: &Shared, routing: &Routing, state: &mut BusState, event: &Event, now: u64, out: &mut Vec<Event>, notes: &mut Vec<Note>) -> Result<Answer, String> {
    if !crate::doc::valid_canvas_id(&routing.canvas) {
        return Err("no canvas with an id is open in Copilot Canvas (open the canvas with an up-to-date client)".into());
    }
    let source = event.source.as_str();
    if !source.is_empty() && !routing.names.contains_key(source) && routing.passive_by_session(source).is_none() {
        return Err("this session is not on the canvas Copilot Canvas has open (open its canvas first)".into());
    }
    let data = &event.data;
    let text = |key: &str| data[key].as_str().unwrap_or_default().trim().to_owned();
    let followed = agent(routing);
    // The agent's records: canvas-wide agents with review follow-ups, and you.
    let agent_or_you = || -> Result<Option<&PassiveSpec>, String> {
        if source.is_empty() {
            return Ok(None);
        }
        routing.passive_by_session(source).filter(|p| p.canvas_wide && p.reviews.is_some()).map(Some).ok_or_else(|| "only a canvas-wide agent with review follow-ups (or you) records messages, checks and requests".to_owned())
    };
    let op = data["op"].as_str().unwrap_or_default();
    // The pull request a hand-off names is also followed (its end), outside this module's lock.
    let mut reviewed: Option<(PrRef, String)> = None;
    let answer = {
        let mut hub = lock(&shared.reviews);
        hub.open(&shared.instance.home, &routing.canvas);
        let book = &mut hub.book;
        let answer = match op {
            "handoff" => {
                let pr = PrRef::parse(&text("url"))?;
                let named = text("session");
                let reviewer = if !named.is_empty() {
                    match routing.resolve_from(&named, source, "")? {
                        Target::Session(id) => id,
                        Target::Passive(_) => return Err(format!("{named:?} is a passive session or agent: reviews come from review sessions")),
                    }
                } else if routing.names.contains_key(source) && routing.passive_by_session(source).is_none() {
                    source.to_owned()
                } else {
                    return Err("name the review session with --session \"<name or id>\"".into());
                };
                let links: Vec<Link> = data["links"].as_array().into_iter().flatten().filter_map(|l| Some(Link { title: l["title"].as_str()?.to_owned(), url: l["url"].as_str()?.to_owned() })).take(20).collect();
                let handoff = Handoff {
                    pr: pr.clone(),
                    title: text("title"),
                    author: text("author"),
                    author_id: text("author_id"),
                    outcome: text("outcome"),
                    results: data["results"].as_str().unwrap_or_default().to_owned(),
                    notes: data["notes"].as_str().unwrap_or_default().to_owned(),
                    links,
                    reviewer: reviewer.clone(),
                    reviewer_name: routing.names.get(&reviewer).cloned().unwrap_or_default(),
                };
                let (id, new) = book.handoff(handoff, now)?;
                let review = book.review(&id).cloned().expect("recorded");
                if !new {
                    Answer { ok: true, message: format!("Review {id} of {} was handed off already; nothing new.", pr.short()), state: "duplicate".into() }
                } else {
                    reviewed = Some((pr.clone(), reviewer.clone()));
                    let missing = match followed {
                        None => Some("no canvas-wide agent has review follow-ups (◎ Agents › + New canvas agent › Review follow-up)".to_owned()),
                        Some((_, settings)) => settings.missing(),
                    };
                    match missing {
                        Some(why) => {
                            book.problem(&id, &why, true, now)?;
                            notes.push(Note { kind: "alert", title: format!("Review of PR {}: no message to {} — {why}", review.short, review.author), body: format!("The review is recorded ({id}); nothing sends it to the author until this is fixed.\n{}", review.url), pr: review.pr.clone(), url: review.url.clone(), session: reviewer.clone() });
                            Answer { ok: true, message: format!("Review {id} of {} recorded; it is not sent to {}: {why}.", pr.short(), review.author), state: "needs-you".into() }
                        }
                        None => {
                            // Nothing is missing: there is an agent with a channel.
                            let (spec, settings) = followed.expect("an agent");
                            out.push(completed_event(routing, &review, settings, event.hop));
                            let approval = if settings.approval { " (you approve it before it is sent)" } else { "" };
                            Answer { ok: true, message: format!("Review {id} of {} recorded for {}: {} drafts the message to the author{approval}.", pr.short(), review.author, spec.name), state: "recorded".into() }
                        }
                    }
                }
            }
            "draft" => {
                let spec = agent_or_you()?;
                let approval = spec.and_then(|s| s.reviews.as_ref()).or(followed.map(|(_, s)| s)).is_none_or(|s| s.approval);
                let id = text("review");
                let state = book.draft(&id, data["text"].as_str().unwrap_or_default(), &spec.map(|s| s.key.clone()).unwrap_or_default(), approval, now)?;
                let review = book.review(&id).cloned().expect("drafted");
                if state == MessageState::AwaitingApproval {
                    notes.push(Note {
                        kind: "warn",
                        title: format!("Approve the message to {} about PR {}", review.author, review.short),
                        body: format!("Review follow-ups drafted this message; it is sent only after you approve it in ◎ Agents › Review follow-ups.\n\n{}", review.message.draft),
                        pr: review.pr.clone(),
                        url: review.url.clone(),
                        session: review.reviewer.clone(),
                    });
                    Answer { ok: true, message: format!("Draft recorded for {}: awaiting the user's approval (◎ Agents › Review follow-ups); you get review.message.approved when it is approved. Do not send it before.", review.short), state: "awaiting-approval".into() }
                } else {
                    Answer { ok: true, message: format!("Draft recorded for {}: approved (no approval needed); send it now, then record it with --sent.", review.short), state: "approved".into() }
                }
            }
            "approve" => {
                if !source.is_empty() {
                    return Err("only you approve a message (◎ Agents › Review follow-ups)".into());
                }
                let id = text("review");
                book.approve(&id, now)?;
                let review = book.review(&id).cloned().expect("approved");
                let to = if review.message.agent.is_empty() { followed.map(|(p, _)| p.key.clone()).unwrap_or_default() } else { review.message.agent.clone() };
                let settings = routing.passive.iter().find(|p| p.key == to).and_then(|p| p.reviews.clone()).unwrap_or_default();
                out.push(Event {
                    kind: "review.message.approved".into(),
                    to,
                    canvas: routing.canvas.clone(),
                    data: serde_json::json!({
                        "review": review.id, "pr": review.pr, "url": review.url, "short": review.short, "author": review.author, "author_id": review.author_id,
                        "draft": review.message.draft, "channel": settings.channel, "tool": settings.tool, "recipients": settings.recipients,
                    }),
                    ..Default::default()
                });
                Answer { ok: true, message: format!("Approved: the message to {} is sent by the review follow-up agent.", review.author), state: "approved".into() }
            }
            "sent" => {
                let spec = agent_or_you()?;
                let id = text("review");
                let channel = spec.and_then(|s| s.reviews.as_ref()).or(followed.map(|(_, s)| s)).map(|s| s.channel.clone()).unwrap_or_default();
                book.sent(&id, &text("conversation"), &text("recipient"), &channel, now)?;
                let review = book.review(&id).cloned().expect("sent");
                notes.push(Note {
                    kind: "ok",
                    title: format!("Review results sent to {} (PR {})", review.message.recipient, review.short),
                    body: format!("{}\n\nSent through {} ({}). The conversation is checked for requests for another review.", review.message.draft, channel, review.message.conversation),
                    pr: review.pr.clone(),
                    url: review.url.clone(),
                    session: review.reviewer.clone(),
                });
                let every = followed.map(|(_, s)| interval_ms(s)).unwrap_or(60 * 60_000);
                Answer { ok: true, message: format!("Recorded as sent to {}; the canvas asks for a check of {} every {} min.", review.message.recipient, review.message.conversation, (every / 60_000).max(1)), state: "sent".into() }
            }
            "needs-you" | "failed" => {
                agent_or_you()?;
                let id = text("review");
                book.problem(&id, &text("reason"), op == "needs-you", now)?;
                let review = book.review(&id).cloned().expect("recorded");
                notes.push(Note {
                    kind: if op == "needs-you" { "alert" } else { "failed" },
                    title: format!("Review of PR {}: the message to {} {}", review.short, review.author, if op == "needs-you" { "needs you" } else { "failed" }),
                    body: format!("{}\n\n{}", review.message.reason, review.url),
                    pr: review.pr.clone(),
                    url: review.url.clone(),
                    session: review.reviewer.clone(),
                });
                Answer { ok: true, message: format!("Recorded: the user sees “{}”.", review.message.reason), state: op.into() }
            }
            "dismiss-message" => {
                if !source.is_empty() {
                    return Err("only you dismiss a message".into());
                }
                let id = text("review");
                let review = book.reviews.iter_mut().find(|r| r.id == id).ok_or("no such review")?;
                if review.message.state == MessageState::Sent {
                    return Err("it was sent already".into());
                }
                review.message.state = MessageState::Dismissed;
                review.message.updated_ms = now;
                Answer { ok: true, message: "Dismissed: nothing is sent.".into(), state: "dismissed".into() }
            }
            "checked" => {
                agent_or_you()?;
                let conversation = text("conversation");
                let failed = text("failed");
                let mark = text("mark");
                book.checked((!conversation.is_empty()).then_some(conversation.as_str()), (!mark.is_empty()).then_some(mark.as_str()), (!failed.is_empty()).then_some(failed.as_str()), now)?;
                if !failed.is_empty() && matches!(book.checks.failures, 1 | 3) {
                    notes.push(Note { kind: "warn", title: "Review follow-ups: checking the conversations failed".into(), body: format!("{failed}\n\nThe next check is still asked for on schedule."), pr: String::new(), url: String::new(), session: String::new() });
                }
                Answer { ok: true, message: if failed.is_empty() { "Check recorded.".into() } else { format!("Failure recorded ({} in a row).", book.checks.failures) }, state: "checked".into() }
            }
            "request" => {
                agent_or_you()?;
                let pr = Some(text("url")).filter(|u| !u.is_empty()).map(|u| PrRef::parse(&u)).transpose()?;
                match book.request(&text("conversation"), &text("message_id"), &text("kind"), data["text"].as_str().unwrap_or_default(), pr.as_ref(), now)? {
                    Requested::Duplicate(id, st) => Answer { ok: true, message: format!("This message was recorded before ({id}, {}); nothing new.", st.word()), state: "duplicate".into() },
                    Requested::New(id, st) => {
                        let q = book.requests.iter().find(|q| q.id == id).cloned().expect("recorded");
                        if st == RequestState::Ambiguous {
                            notes.push(Note { kind: "warn", title: format!("A review request needs clarification: {}", q.reason), body: format!("“{}” (conversation {})\nChoose its pull request in ◎ Agents › Review follow-ups, or dismiss it.", q.text, q.conversation), pr: String::new(), url: String::new(), session: String::new() });
                        }
                        let message = match st {
                            RequestState::Ambiguous => format!("Request {id} recorded; it needs the user's clarification: {}.", q.reason),
                            RequestState::Waiting => format!("Request {id} for {} recorded; it waits for the request in progress for the same pull request.", q.short),
                            _ => format!("Request {id} for {} recorded; it goes to “{}” when that session is idle.", q.short, q.to_name),
                        };
                        Answer { ok: true, message, state: st.word().into() }
                    }
                }
            }
            "done" | "dismiss" => {
                let id = text("request");
                let q = book.requests.iter().find(|q| q.id == id).cloned().ok_or_else(|| format!("no request {id:?}"))?;
                if !wide(routing, source) && source != q.to {
                    return Err("only the review session it went to, a canvas-wide agent or you finish a request".into());
                }
                state.mailbox.retain(|m| m.id != id);
                book.finish(&id, op == "dismiss", now)?;
                Answer { ok: true, message: format!("Request {id} {}.", if op == "dismiss" { "dismissed" } else { "done" }), state: op.into() }
            }
            "assign" => {
                if !wide(routing, source) {
                    return Err("only you and canvas-wide agents clarify a request".into());
                }
                book.assign(&text("request"), &text("review"), now)?;
                Answer { ok: true, message: "Assigned: it goes to that review's session when it is idle.".into(), state: "assigned".into() }
            }
            "unwatch" => {
                if !wide(routing, source) {
                    return Err("only you and canvas-wide agents stop watching a conversation".into());
                }
                let id = text("conversation");
                let c = book.conversations.iter_mut().find(|c| c.id == id).ok_or_else(|| format!("no conversation {id:?}"))?;
                c.watching = false;
                Answer { ok: true, message: format!("No longer watching {id}."), state: "unwatched".into() }
            }
            "check-now" => {
                if !wide(routing, source) {
                    return Err("only you and canvas-wide agents ask for a check".into());
                }
                book.checks.next_ms = now;
                Answer { ok: true, message: "A check is asked for now.".into(), state: "check".into() }
            }
            other => return Err(format!("unknown review request {other:?}")),
        };
        hub.save(&shared.instance.home);
        answer
    };
    if let Some((pr, reviewer)) = reviewed {
        super::prs::track_reviewed(shared, routing, &pr, &reviewer, now);
    }
    Ok(answer)
}

/// `review.completed` for the review follow-up agent(s): the public results
/// and the channel settings, never the private notes.
fn completed_event(routing: &Routing, review: &Review, settings: &ReviewSettings, hop: u32) -> Event {
    Event {
        kind: "review.completed".into(),
        source: review.reviewer.clone(),
        canvas: routing.canvas.clone(),
        data: serde_json::json!({
            "review": review.id, "pr": review.pr, "url": review.url, "short": review.short, "title": review.title,
            "author": review.author, "author_id": review.author_id, "outcome": review.outcome,
            "results": excerpt(&review.results, 4_000), "links": review.links.iter().map(|l| serde_json::json!({ "title": l.title, "url": l.url })).collect::<Vec<_>>(),
            "reviewer_name": review.reviewer_name, "channel": settings.channel, "tool": settings.tool, "recipients": settings.recipients, "approval": settings.approval,
        }),
        hop,
        ..Default::default()
    }
}

/// Moves requests along like follow-ups of pull requests: intended or
/// blocked ones whose review session can get them go into its mailbox;
/// queued ones whose session stopped are blocked; queued ones whose message
/// is gone failed. Answers the changed requests.
fn route(shared: &Shared, routing: &Routing, state: &mut BusState, book: &mut Book, now: u64) -> Vec<FollowRequest> {
    let mut changed = Vec::new();
    let reviews = book.reviews.clone();
    for q in book.requests.iter_mut().filter(|q| matches!(q.state, RequestState::Intended | RequestState::Queued | RequestState::Blocked)) {
        let is_running = running(shared, &q.to);
        let reach = reach(routing, &q.to, is_running, !is_running && elsewhere(shared, &q.to));
        let queued = state.mailbox.iter().any(|m| m.id == q.id);
        let (next, reason) = match (q.state, reach) {
            (RequestState::Queued, Ok(())) if queued => continue,
            (RequestState::Queued, Ok(())) => (RequestState::Failed, if now.saturating_sub(q.created_ms) >= 86_000_000 { "not delivered within a day".to_owned() } else { "its queued message was discarded".to_owned() }),
            (RequestState::Intended | RequestState::Blocked, Ok(())) => {
                let Some(review) = reviews.iter().find(|r| r.id == q.review) else { continue };
                state.mailbox.retain(|m| m.id != q.id);
                state.mailbox.push(Mail { id: q.id.clone(), to: q.to.clone(), from: "Review follow-ups".into(), text: reviews::request_text(q, review), ms: now });
                (RequestState::Queued, String::new())
            }
            (RequestState::Blocked, Err(why)) => {
                q.reason = why;
                continue;
            }
            (_, Err(why)) => {
                state.mailbox.retain(|m| m.id != q.id);
                (RequestState::Blocked, why)
            }
            _ => continue,
        };
        q.state = next;
        q.reason = reason;
        q.updated_ms = now;
        changed.push(q.clone());
    }
    changed
}

/// Reports request changes (thread `review:<pull request>`).
fn report_requests(shared: &Shared, routing: &Routing, changed: &[FollowRequest], book: &Book, now: u64) {
    for q in changed {
        log(&shared.instance, &format!("review request {} for {} to {}: {}{}", q.id, q.short, q.to_name, q.state.word(), if q.reason.is_empty() { String::new() } else { format!(" ({})", q.reason) }));
        let Some(kind) = q.state.report_kind() else { continue };
        let review = book.review(&q.review);
        let author = review.map(|r| r.author.clone()).unwrap_or_else(|| "The author".into());
        let title = match q.state {
            RequestState::Queued => format!("{author} asks for a {} of PR {}; routed to “{}” (queued)", q.kind, q.short, q.to_name),
            RequestState::Delivered => format!("{author}'s {} request for PR {} delivered to “{}”", q.kind, q.short, q.to_name),
            RequestState::Blocked => format!("{author}'s {} request for PR {} is blocked", q.kind, q.short),
            _ => format!("{author}'s {} request for PR {} failed", q.kind, q.short),
        };
        let what = match q.state {
            RequestState::Queued => "Queued: it is typed into the review session when that session is idle; delivered follows in this thread.".to_owned(),
            RequestState::Delivered => "Delivered: typed into the review session.".to_owned(),
            RequestState::Blocked => format!("Blocked: {}. ◎ Agents › Review follow-ups has Resume reviewer and Dismiss.", q.reason),
            _ => format!("Failed: {}.", q.reason),
        };
        report(shared, routing, Note { kind, title, body: format!("{what}\n\n“{}” (conversation {})", q.text, q.conversation), pr: q.pr.clone(), url: review.map(|r| r.url.clone()).unwrap_or_default(), session: q.to.clone() }, now);
    }
}

/// Each pass of the bus loop: quiet or ended conversations stop being
/// watched, requests move along, and the hourly check is asked for. Answers
/// events to route (`review.check_due`).
pub(super) fn tick(shared: &Shared, routing: &Routing, state: &mut BusState, now: u64) -> Vec<Event> {
    if !crate::doc::valid_canvas_id(&routing.canvas) {
        return Vec::new();
    }
    let ended = super::prs::ended(shared, routing);
    let mut out = Vec::new();
    let (changed, book) = {
        let mut hub = lock(&shared.reviews);
        hub.open(&shared.instance.home, &routing.canvas);
        for id in hub.book.retire(now, |k| ended.iter().any(|e| e == k)) {
            log(&shared.instance, &format!("review follow-ups: no longer watching {id} (its pull requests ended, or quiet for 14 days)"));
        }
        let changed = route(shared, routing, state, &mut hub.book, now);
        if let Some((spec, settings)) = agent(routing)
            && !settings.paused
            && settings.missing().is_none()
        {
            let every = interval_ms(settings);
            if hub.book.due(now, every) {
                let conversations: Vec<serde_json::Value> = hub
                    .book
                    .watched()
                    .iter()
                    .map(|c| {
                        let prs: Vec<&str> = hub.book.reviews.iter().filter(|r| c.reviews.contains(&r.id)).map(|r| r.url.as_str()).collect();
                        serde_json::json!({ "conversation": c.id, "channel": c.channel, "contact": c.contact, "contact_name": c.contact_name, "since": c.mark, "last_checked_ms": c.last_checked_ms, "prs": prs })
                    })
                    .collect();
                out.push(Event {
                    kind: "review.check_due".into(),
                    to: spec.key.clone(),
                    canvas: routing.canvas.clone(),
                    data: serde_json::json!({ "conversations": conversations, "every_minutes": settings.check_minutes, "channel": settings.channel, "tool": settings.tool }),
                    ..Default::default()
                });
                hub.book.asked(now, every);
                log(&shared.instance, &format!("review follow-ups: asked {} to check {} conversation(s)", spec.name, conversations.len()));
            }
        }
        hub.save(&shared.instance.home);
        let book = if changed.is_empty() { Book::default() } else { hub.book.clone() };
        (changed, book)
    };
    report_requests(shared, routing, &changed, &book, now);
    out
}

/// A message the bus loop typed in: a request is delivered.
pub(super) fn delivered(shared: &Shared, routing: &Routing, mail: &Mail, now: u64) {
    if !mail.id.starts_with("q-") {
        return;
    }
    let (changed, book) = {
        let mut hub = lock(&shared.reviews);
        let Some(q) = hub.book.requests.iter_mut().find(|q| q.id == mail.id) else { return };
        q.state = RequestState::Delivered;
        q.reason.clear();
        q.updated_ms = now;
        q.delivered_ms = now;
        let q = q.clone();
        hub.save(&shared.instance.home);
        (q, hub.book.clone())
    };
    report_requests(shared, routing, &[changed], &book, now);
}

/// The review follow-ups for clients (`None`: none recorded and no agent for them).
pub(super) fn view(shared: &Shared, routing: &Routing) -> Option<ReviewsView> {
    if !crate::doc::valid_canvas_id(&routing.canvas) {
        return None;
    }
    let hub = lock(&shared.reviews);
    let any = routing.passive.iter().find(|p| p.canvas_wide && p.reviews.is_some());
    if hub.canvas != routing.canvas || (hub.book.reviews.is_empty() && any.is_none()) {
        return None;
    }
    let running = agent(routing);
    let settings = running.map(|(_, s)| s.clone()).or_else(|| any.and_then(|p| p.reviews.clone()));
    let paused = match (running, any) {
        (None, Some(_)) => "its review follow-up agent is paused: resume it".to_owned(),
        (None, None) => "no canvas-wide agent has review follow-ups: ◎ Agents › + New canvas agent › Review follow-up".to_owned(),
        (Some((_, s)), _) if s.paused => "checks are paused (Resume checks)".to_owned(),
        (Some((_, s)), _) => s.missing().unwrap_or_default(),
    };
    let names = agents(routing).iter().map(|p| p.name.clone()).collect();
    Some(ReviewsView::of(&routing.canvas, &hub.book, names, settings, paused))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_follow_the_agents_settings_and_reports_have_their_own_thread() {
        let mut routing = Routing { canvas: "c1".into(), ..Default::default() };
        assert!(agent(&routing).is_none());
        routing.passive.push(PassiveSpec { key: "kf".into(), session_id: "f".into(), name: "Review follow-up".into(), canvas_wide: true, reviews: Some(ReviewSettings::parse("mcp teams every 2h").unwrap().unwrap()), ..Default::default() });
        let (spec, settings) = agent(&routing).unwrap();
        assert_eq!((spec.key.as_str(), interval_ms(settings)), ("kf", 2 * 3_600_000));
        routing.passive[0].paused = true;
        assert!(agent(&routing).is_none(), "a paused agent asks for no checks");
        assert_eq!(thread("github.com/o/r#7"), "review:github.com/o/r#7");
        // Approvals are a window's: the inbox (where any process can drop a file) never carries one.
        let approve = Event { kind: REQUEST.into(), data: serde_json::json!({ "op": "approve", "review": "rv-1" }), ..Default::default() };
        assert!(window_only(&approve));
        assert!(!window_only(&Event { data: serde_json::json!({ "op": "sent" }), ..approve }));
    }
}
