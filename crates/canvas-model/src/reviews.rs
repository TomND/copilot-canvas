//! Review follow-ups (WI-016): after a review session finishes a review, it
//! hands the results to the canvas (`copilot-canvas review handoff`); a
//! canvas-wide agent with review follow-ups drafts a message to the pull
//! request's author, you approve it (by default), the agent sends it through
//! the messaging channel you configured and records where; then the canvas
//! asks the agent every hour to read those conversations (only those) for
//! requests for another review, and routes each request once to the review
//! session that did the review. The canvas keeps every record here
//! ([`Book`], `<home>/canvas-agents/<canvas id>/reviews/book.json`, one
//! writer: the service); sending and reading messages is the agent's.

use serde::{Deserialize, Serialize};

use crate::prs::{PrRef, excerpt, fnv};

/// Longest public results a hand-off carries (characters).
pub const MAX_RESULTS: usize = 8_000;
/// Longest private notes kept (characters).
pub const MAX_NOTES: usize = 8_000;
/// Longest message draft (characters).
pub const MAX_DRAFT: usize = 4_000;
/// Records kept at most (the newest).
pub const MAX_REVIEWS: usize = 300;
pub const MAX_REQUESTS: usize = 300;
/// Seen message ids kept per conversation.
pub const MAX_SEEN: usize = 500;
pub const DEFAULT_CHECK_MINUTES: u32 = 60;
/// A conversation is no longer watched this long after its last activity.
pub const WATCH_MS: u64 = 14 * 86_400_000;

/// The messaging channels a review follow-up agent can use.
pub const CHANNELS: [(&str, &str); 3] = [
    ("mcp", "a messaging MCP server or tool the agent has (Teams, Slack, mail): name it"),
    ("pr-comment", "a comment on the pull request itself (gh / az): naming this channel is the permission to post it"),
    ("outbox", "a folder: one file per conversation (your own bridge, and the tests' stand-in)"),
];

/// Review follow-ups of a canvas-wide agent.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ReviewSettings {
    /// [`CHANNELS`] (empty: not set yet; nothing is sent).
    #[serde(default)]
    pub channel: String,
    /// The MCP server or tool (`mcp`), or the folder (`outbox`).
    #[serde(default)]
    pub tool: String,
    /// How the agent finds the recipient and conversation.
    #[serde(default = "default_recipients")]
    pub recipients: String,
    /// You approve every message before it is sent (the default).
    #[serde(default = "yes")]
    pub approval: bool,
    /// Minutes between checks of the watched conversations.
    #[serde(default = "default_check")]
    pub check_minutes: u32,
    /// Checks are paused (records and messages still work).
    #[serde(default)]
    pub paused: bool,
}

fn default_recipients() -> String {
    "the pull request's author, by the account the pull request shows".into()
}

fn yes() -> bool {
    true
}

fn default_check() -> u32 {
    DEFAULT_CHECK_MINUTES
}

impl Default for ReviewSettings {
    fn default() -> Self {
        ReviewSettings { channel: String::new(), tool: String::new(), recipients: default_recipients(), approval: true, check_minutes: DEFAULT_CHECK_MINUTES, paused: false }
    }
}

impl ReviewSettings {
    /// `outbox C:\x approval every 60m recipients: …` (the settings prompt's text).
    pub fn text(&self) -> String {
        let channel = if self.channel.is_empty() { "unset".to_owned() } else if self.tool.is_empty() { self.channel.clone() } else { format!("{} {}", self.channel, self.tool) };
        format!(
            "{channel} {} every {}m{} recipients: {}",
            if self.approval { "approval" } else { "no-approval" },
            self.check_minutes,
            if self.paused { " paused" } else { "" },
            self.recipients
        )
    }

    /// Parses the settings prompt: `off`, or `CHANNEL [TOOL] [approval |
    /// no-approval] [every 60m] [paused] [recipients: RULE]`.
    pub fn parse(text: &str) -> Result<Option<ReviewSettings>, String> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("off") {
            return Ok(None);
        }
        let (head, rule) = match text.find("recipients:") {
            Some(at) => (&text[..at], Some(text[at + "recipients:".len()..].trim())),
            None => (text, None),
        };
        let mut settings = ReviewSettings::default();
        if let Some(rule) = rule.filter(|r| !r.is_empty()) {
            settings.recipients = excerpt(rule, 300);
        }
        let words: Vec<&str> = head.split_whitespace().collect();
        let mut i = 0;
        // The tool (a folder may have spaces) runs from the channel to the first keyword.
        let mut tool_open = false;
        while i < words.len() {
            let word = words[i];
            let lower = word.to_ascii_lowercase();
            let keyword = matches!(lower.as_str(), "approval" | "no-approval" | "paused" | "every");
            if keyword {
                tool_open = false;
            }
            match lower.as_str() {
                "approval" => settings.approval = true,
                "no-approval" => settings.approval = false,
                "paused" => settings.paused = true,
                "unset" if settings.channel.is_empty() => {}
                "every" => {
                    let next = words.get(i + 1).ok_or("every takes a duration such as 60m")?;
                    let ms = crate::bus::parse_duration(next).ok_or_else(|| format!("not a duration: {next} (for example 60m)"))?;
                    settings.check_minutes = ((ms / 60_000).max(1)) as u32;
                    i += 1;
                }
                _ if settings.channel.is_empty() && CHANNELS.iter().any(|(c, _)| *c == lower) => {
                    settings.channel = lower;
                    tool_open = true;
                }
                _ if tool_open => {
                    if !settings.tool.is_empty() {
                        settings.tool.push(' ');
                    }
                    settings.tool.push_str(word);
                }
                _ => return Err(format!("not understood: {word} (CHANNEL [TOOL] approval|no-approval every 60m recipients: RULE · channels: {})", CHANNELS.iter().map(|(c, _)| *c).collect::<Vec<_>>().join(", "))),
            }
            i += 1;
        }
        settings.tool = settings.tool.trim_matches('"').to_owned();
        if matches!(settings.channel.as_str(), "mcp" | "outbox") && settings.tool.is_empty() {
            return Err(format!("{} needs {}", settings.channel, if settings.channel == "mcp" { "the MCP server or tool's name" } else { "the folder" }));
        }
        Ok(Some(settings))
    }

    /// Why nothing can be sent with these settings (`None`: it can).
    pub fn missing(&self) -> Option<String> {
        self.channel.is_empty().then(|| "its messaging channel is not set yet (its panel's Follow-ups settings)".to_owned())
    }
}

/// Where a review's message to its author stands.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MessageState {
    /// Handed off: the agent drafts it.
    #[default]
    Pending,
    /// Drafted: it waits for your approval.
    AwaitingApproval,
    /// Approved (or no approval needed): the agent sends it.
    Approved,
    Sent,
    /// The agent could not send it without you (no access, an ambiguous recipient).
    NeedsYou,
    Failed,
    Dismissed,
}

impl MessageState {
    pub fn word(self) -> &'static str {
        match self {
            MessageState::Pending => "being drafted",
            MessageState::AwaitingApproval => "awaiting your approval",
            MessageState::Approved => "approved, being sent",
            MessageState::Sent => "sent",
            MessageState::NeedsYou => "needs you",
            MessageState::Failed => "failed",
            MessageState::Dismissed => "dismissed",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Message {
    pub state: MessageState,
    #[serde(default)]
    pub draft: String,
    #[serde(default)]
    pub channel: String,
    /// The conversation it went to (the channel's id for it).
    #[serde(default)]
    pub conversation: String,
    #[serde(default)]
    pub recipient: String,
    /// The agent that drafted it (its passive key).
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub updated_ms: u64,
    #[serde(default)]
    pub sent_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Link {
    pub title: String,
    pub url: String,
}

/// A completed review handed off by its review session.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Review {
    /// `rv-<ms>-<4 hex>`.
    pub id: String,
    /// The pull request's key, link and short name.
    pub pr: String,
    pub url: String,
    pub short: String,
    #[serde(default)]
    pub title: String,
    /// The author as people know them, and their account.
    pub author: String,
    #[serde(default)]
    pub author_id: String,
    /// `approved`, `changes-requested`, `commented`, … (what the review established).
    pub outcome: String,
    /// What may be told to the author.
    pub results: String,
    /// Private notes: never in an event, a draft or a message to the author.
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub links: Vec<Link>,
    /// The review session (its box) and its name.
    pub reviewer: String,
    #[serde(default)]
    pub reviewer_name: String,
    pub created_ms: u64,
    #[serde(default)]
    pub message: Message,
}

/// A conversation with an author the canvas watches for requests.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Conversation {
    pub id: String,
    pub channel: String,
    /// The author's account and name.
    pub contact: String,
    #[serde(default)]
    pub contact_name: String,
    /// Reviews sent in it (ids).
    #[serde(default)]
    pub reviews: Vec<String>,
    pub watching: bool,
    #[serde(default)]
    pub last_checked_ms: u64,
    /// Where the agent's last check ended (its own token: a message id, a time).
    #[serde(default)]
    pub mark: String,
    pub created_ms: u64,
    #[serde(default)]
    pub active_ms: u64,
    /// Message ids recorded as requests (never twice).
    #[serde(default)]
    pub seen: Vec<String>,
}

/// Where a request for another review stands.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RequestState {
    /// Which pull request it is about is not clear: you decide.
    Ambiguous,
    /// Another request for the same pull request is in progress.
    Waiting,
    /// Recorded; the service routes it at its next pass.
    #[default]
    Intended,
    /// In the review session's mailbox, typed in when it is idle.
    Queued,
    /// Typed into the review session (it reviews again).
    Delivered,
    /// The review session cannot get it (stopped, elsewhere, gone).
    Blocked,
    Failed,
    /// Reviewed again (a new hand-off), or marked done.
    Done,
    Dismissed,
}

impl RequestState {
    pub fn word(self) -> &'static str {
        match self {
            RequestState::Ambiguous => "needs clarification",
            RequestState::Waiting => "waiting",
            RequestState::Intended => "intended",
            RequestState::Queued => "queued",
            RequestState::Delivered => "delivered",
            RequestState::Blocked => "blocked",
            RequestState::Failed => "failed",
            RequestState::Done => "done",
            RequestState::Dismissed => "dismissed",
        }
    }

    /// In progress for its pull request (a second one waits).
    pub fn active(self) -> bool {
        matches!(self, RequestState::Intended | RequestState::Queued | RequestState::Delivered | RequestState::Blocked)
    }

    /// The Reports kind of a change to this state (`None`: no report).
    pub fn report_kind(self) -> Option<&'static str> {
        match self {
            RequestState::Ambiguous | RequestState::Blocked => Some("warn"),
            RequestState::Queued => Some("queued"),
            RequestState::Delivered => Some("delivered"),
            RequestState::Failed => Some("failed"),
            _ => None,
        }
    }
}

/// The request kinds that count.
pub const KINDS: [&str; 3] = ["re-review", "another-review", "follow-up"];

/// An author's request for another review, a re-review or review follow-up.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct FollowRequest {
    /// `q-<ms>-<4 hex>`; also its mailbox message's id.
    pub id: String,
    pub conversation: String,
    /// The channel's id of the author's message.
    pub message_id: String,
    pub kind: String,
    /// Their words (short; untrusted).
    pub text: String,
    /// The pull request and review it belongs to (empty while ambiguous).
    #[serde(default)]
    pub pr: String,
    #[serde(default)]
    pub short: String,
    #[serde(default)]
    pub review: String,
    pub state: RequestState,
    /// The review session it goes to, and its name.
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub to_name: String,
    /// Why it is ambiguous, waiting, blocked or failed.
    #[serde(default)]
    pub reason: String,
    pub created_ms: u64,
    #[serde(default)]
    pub updated_ms: u64,
    #[serde(default)]
    pub delivered_ms: u64,
}

/// The hourly check.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct CheckState {
    /// When the agent last reported a check.
    #[serde(default)]
    pub last_ms: u64,
    /// When the next check is asked for (0: once a conversation is watched).
    #[serde(default)]
    pub next_ms: u64,
    /// When the agent was last asked.
    #[serde(default)]
    pub asked_ms: u64,
    #[serde(default)]
    pub failures: u32,
    #[serde(default)]
    pub last_error: String,
}

/// The review follow-ups of one canvas.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Book {
    #[serde(default)]
    pub reviews: Vec<Review>,
    #[serde(default)]
    pub conversations: Vec<Conversation>,
    #[serde(default)]
    pub requests: Vec<FollowRequest>,
    #[serde(default)]
    pub checks: CheckState,
}

/// What a review session hands off.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Handoff {
    pub pr: PrRef,
    pub title: String,
    pub author: String,
    pub author_id: String,
    pub outcome: String,
    pub results: String,
    pub notes: String,
    pub links: Vec<Link>,
    pub reviewer: String,
    pub reviewer_name: String,
}

/// What a request did.
#[derive(Debug, PartialEq)]
pub enum Requested {
    /// A new request (its id and state).
    New(String, RequestState),
    /// This message was recorded before (its request's id and state).
    Duplicate(String, RequestState),
}

/// Phrases that claim an approval.
const APPROVAL_CLAIMS: [&str; 7] = ["approved it", "i approved", "i've approved", "i have approved", "lgtm", "ready to merge", "good to merge"];

fn new_id(prefix: &str, now: u64, salt: &str) -> String {
    format!("{prefix}-{now}-{:04x}", fnv(&format!("{salt}{now}")) & 0xffff)
}

impl Book {
    pub fn review(&self, id: &str) -> Option<&Review> {
        self.reviews.iter().find(|r| r.id == id)
    }

    fn review_mut(&mut self, id: &str) -> Result<&mut Review, String> {
        self.reviews.iter_mut().find(|r| r.id == id).ok_or_else(|| format!("no review {id:?} on this canvas (copilot-canvas review list)"))
    }

    pub fn conversation(&self, id: &str) -> Option<&Conversation> {
        self.conversations.iter().find(|c| c.id == id)
    }

    /// Records a completed review (answers its id and whether it is new).
    /// The same results from the same session for the same pull request are
    /// one review; a new review of a pull request completes the requests for it.
    pub fn handoff(&mut self, h: Handoff, now: u64) -> Result<(String, bool), String> {
        let results = h.results.trim();
        if results.is_empty() {
            return Err("the review results are empty (--results FILE: what may be told to the author)".into());
        }
        if results.chars().count() > MAX_RESULTS {
            return Err(format!("the review results have more than {MAX_RESULTS} characters: summarize them"));
        }
        if h.author.trim().is_empty() {
            return Err("name the pull request's author with --author".into());
        }
        let outcome = h.outcome.trim().to_ascii_lowercase().replace([' ', '_'], "-");
        let outcome = if outcome.is_empty() { "commented".to_owned() } else { outcome };
        if outcome.len() > 40 || !outcome.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err("--outcome is one word, for example approved, changes-requested or commented".into());
        }
        let key = h.pr.key();
        let hash = fnv(results);
        if let Some(existing) = self.reviews.iter().find(|r| r.pr == key && r.reviewer == h.reviewer && fnv(&r.results) == hash) {
            return Ok((existing.id.clone(), false));
        }
        let id = new_id("rv", now, &format!("{key}{}", h.reviewer));
        for request in self.requests.iter_mut().filter(|q| q.pr == key && (q.state.active() || q.state == RequestState::Waiting)) {
            request.state = RequestState::Done;
            request.reason = "reviewed again (a new review was handed off)".into();
            request.updated_ms = now;
        }
        self.reviews.push(Review {
            id: id.clone(),
            pr: key,
            url: h.pr.url(),
            short: h.pr.short(),
            title: excerpt(&h.title, 200),
            author: excerpt(&h.author, 120),
            author_id: excerpt(&h.author_id, 200),
            outcome,
            results: results.to_owned(),
            notes: h.notes.trim().chars().take(MAX_NOTES).collect(),
            links: h.links,
            reviewer: h.reviewer,
            reviewer_name: h.reviewer_name,
            created_ms: now,
            message: Message { updated_ms: now, ..Default::default() },
        });
        let excess = self.reviews.len().saturating_sub(MAX_REVIEWS);
        self.reviews.drain(..excess);
        Ok((id, true))
    }

    /// Records the agent's draft. It must link the pull request, leave the
    /// private notes out and claim no approval the review did not give;
    /// answers what happens next (approval or sending).
    pub fn draft(&mut self, id: &str, text: &str, agent: &str, approval: bool, now: u64) -> Result<MessageState, String> {
        let review = self.review_mut(id)?;
        if matches!(review.message.state, MessageState::Sent | MessageState::Dismissed) {
            return Err(format!("the message for {} was already {}: one message per review", review.short, review.message.state.word()));
        }
        let text = text.trim();
        if text.is_empty() {
            return Err("the draft is empty".into());
        }
        if text.chars().count() > MAX_DRAFT {
            return Err(format!("the draft has more than {MAX_DRAFT} characters: keep it concise"));
        }
        if !text.contains(&review.url) {
            return Err(format!("link the pull request in the message: {}", review.url));
        }
        let lower = text.to_lowercase();
        if let Some(line) = review.notes.split(['\n', '.', ';', '!', '?', ',']).map(str::trim).filter(|l| l.chars().count() >= 16).find(|l| lower.contains(&l.to_lowercase())) {
            return Err(format!("the draft quotes the private review notes ({:?}): leave them out", excerpt(line, 60)));
        }
        if review.outcome != "approved" && let Some(claim) = APPROVAL_CLAIMS.iter().find(|c| lower.contains(*c)) {
            return Err(format!("the review's outcome is {}, not an approval: do not say “{claim}”", review.outcome));
        }
        review.message.draft = text.to_owned();
        review.message.agent = agent.to_owned();
        review.message.reason.clear();
        review.message.state = if approval { MessageState::AwaitingApproval } else { MessageState::Approved };
        review.message.updated_ms = now;
        Ok(review.message.state)
    }

    /// You approve a draft.
    pub fn approve(&mut self, id: &str, now: u64) -> Result<(), String> {
        let review = self.review_mut(id)?;
        if review.message.state != MessageState::AwaitingApproval {
            return Err(format!("the message for {} is {}, not awaiting approval", review.short, review.message.state.word()));
        }
        review.message.state = MessageState::Approved;
        review.message.updated_ms = now;
        Ok(())
    }

    /// The agent sent the approved message: recorded once, and its
    /// conversation is watched from now on.
    pub fn sent(&mut self, id: &str, conversation: &str, recipient: &str, channel: &str, now: u64) -> Result<(), String> {
        let conversation = conversation.trim();
        if conversation.is_empty() || conversation.chars().count() > 200 {
            return Err("--conversation names where it was sent (the channel's id, at most 200 characters)".into());
        }
        let review = self.review_mut(id)?;
        match review.message.state {
            MessageState::Sent => return Err(format!("the message for {} was sent already: one message per review", review.short)),
            MessageState::Approved => {}
            MessageState::AwaitingApproval => return Err(format!("the message for {} is not approved yet: the user approves it in ◎ Agents › Review follow-ups", review.short)),
            other => return Err(format!("the message for {} is {}: record an approved draft first (review message ID --draft FILE)", review.short, other.word())),
        }
        review.message.state = MessageState::Sent;
        review.message.conversation = conversation.to_owned();
        review.message.recipient = excerpt(if recipient.trim().is_empty() { &review.author } else { recipient }, 120);
        review.message.channel = channel.to_owned();
        review.message.sent_ms = now;
        review.message.updated_ms = now;
        let (contact, name, review_id) = (if review.author_id.is_empty() { review.author.clone() } else { review.author_id.clone() }, review.author.clone(), review.id.clone());
        match self.conversations.iter_mut().find(|c| c.id == conversation) {
            Some(c) => {
                if !c.reviews.contains(&review_id) {
                    c.reviews.push(review_id);
                }
                c.watching = true;
                c.active_ms = now;
            }
            None => self.conversations.push(Conversation { id: conversation.to_owned(), channel: channel.to_owned(), contact, contact_name: name, reviews: vec![review_id], watching: true, created_ms: now, active_ms: now, ..Default::default() }),
        }
        Ok(())
    }

    /// The agent could not send: it needs you (`needs_you`) or it failed.
    pub fn problem(&mut self, id: &str, reason: &str, needs_you: bool, now: u64) -> Result<(), String> {
        let review = self.review_mut(id)?;
        if review.message.state == MessageState::Sent {
            return Err(format!("the message for {} was sent already", review.short));
        }
        review.message.state = if needs_you { MessageState::NeedsYou } else { MessageState::Failed };
        review.message.reason = excerpt(reason, 300);
        review.message.updated_ms = now;
        Ok(())
    }

    /// A check of the watched conversations (one, or all): its end mark, or why it failed.
    pub fn checked(&mut self, conversation: Option<&str>, mark: Option<&str>, failed: Option<&str>, now: u64) -> Result<(), String> {
        if let Some(id) = conversation
            && self.conversation(id).is_none()
        {
            return Err(format!("no conversation {id:?} is watched on this canvas"));
        }
        if let Some(why) = failed {
            self.checks.failures += 1;
            self.checks.last_error = excerpt(why, 300);
            return Ok(());
        }
        self.checks.last_ms = now;
        self.checks.failures = 0;
        self.checks.last_error.clear();
        for c in self.conversations.iter_mut().filter(|c| conversation.is_none_or(|id| c.id == id)) {
            c.last_checked_ms = now;
            if let Some(mark) = mark {
                c.mark = excerpt(mark, 200);
            }
        }
        Ok(())
    }

    /// Records an author's request from a watched conversation, once per
    /// message, with its pull request and review when that is clear.
    pub fn request(&mut self, conversation: &str, message_id: &str, kind: &str, text: &str, pr: Option<&PrRef>, now: u64) -> Result<Requested, String> {
        let kind = kind.trim().to_ascii_lowercase();
        if !KINDS.contains(&kind.as_str()) {
            return Err(format!("--kind is one of {} (other messages are not review requests: leave them)", KINDS.join(", ")));
        }
        let message_id = message_id.trim();
        if message_id.is_empty() || message_id.chars().count() > 200 {
            return Err("--message-id names the author's message (the channel's id)".into());
        }
        let conv = self.conversations.iter().find(|c| c.id == conversation && c.watching).ok_or_else(|| format!("{conversation:?} is not a conversation the canvas watches (only conversations a review was sent in)"))?.clone();
        if let Some(existing) = self.requests.iter().find(|q| q.conversation == conversation && q.message_id == message_id) {
            return Ok(Requested::Duplicate(existing.id.clone(), existing.state));
        }
        // The reviews this author got in this conversation, then (a named pull request) any of theirs.
        let mine: Vec<&Review> = self.reviews.iter().filter(|r| conv.reviews.contains(&r.id)).collect();
        let candidates: Vec<&Review> = match pr {
            Some(pr) => {
                let key = pr.key();
                let named: Vec<&Review> = mine.iter().copied().filter(|r| r.pr == key).collect();
                if named.is_empty() { self.reviews.iter().filter(|r| r.pr == key && (r.author_id == conv.contact || r.author == conv.contact_name)).collect() } else { named }
            }
            None => mine,
        };
        let mut prs: Vec<&str> = candidates.iter().map(|r| r.pr.as_str()).collect();
        prs.sort();
        prs.dedup();
        let id = new_id("q", now, &format!("{conversation}{message_id}"));
        let mut request = FollowRequest {
            id: id.clone(),
            conversation: conversation.to_owned(),
            message_id: message_id.to_owned(),
            kind,
            text: excerpt(text, 500),
            created_ms: now,
            updated_ms: now,
            ..Default::default()
        };
        match prs.as_slice() {
            [key] => {
                let review = candidates.iter().filter(|r| r.pr == *key).max_by_key(|r| r.created_ms).expect("one");
                request.pr = review.pr.clone();
                request.short = review.short.clone();
                request.review = review.id.clone();
                request.to = review.reviewer.clone();
                request.to_name = review.reviewer_name.clone();
                let busy = self.requests.iter().any(|q| q.pr == request.pr && q.state.active());
                request.state = if busy { RequestState::Waiting } else { RequestState::Intended };
                if busy {
                    request.reason = "another request for this pull request is in progress".into();
                }
            }
            [] => {
                request.state = RequestState::Ambiguous;
                request.reason = match pr {
                    Some(pr) => format!("no review of {} was sent to {}", pr.short(), conv.contact_name),
                    None => "no review was sent in this conversation".into(),
                };
            }
            several => {
                request.state = RequestState::Ambiguous;
                let shorts: Vec<String> = several.iter().filter_map(|k| self.reviews.iter().find(|r| r.pr == *k).map(|r| r.short.clone())).collect();
                request.reason = format!("which pull request? {} were reviewed for {}", shorts.join(" and "), conv.contact_name);
            }
        }
        let state = request.state;
        self.requests.push(request);
        if let Some(c) = self.conversations.iter_mut().find(|c| c.id == conversation) {
            c.active_ms = now;
            c.seen.push(message_id.to_owned());
            let excess = c.seen.len().saturating_sub(MAX_SEEN);
            c.seen.drain(..excess);
        }
        let excess = self.requests.len().saturating_sub(MAX_REQUESTS);
        self.requests.drain(..excess);
        Ok(Requested::New(id, state))
    }

    /// You settle an ambiguous request with the review it is about.
    pub fn assign(&mut self, request: &str, review: &str, now: u64) -> Result<(), String> {
        let r = self.review(review).cloned().ok_or_else(|| format!("no review {review:?}"))?;
        let busy = self.requests.iter().any(|q| q.id != request && q.pr == r.pr && q.state.active());
        let q = self.requests.iter_mut().find(|q| q.id == request).ok_or_else(|| format!("no request {request:?}"))?;
        if q.state != RequestState::Ambiguous {
            return Err(format!("the request is {}, not waiting for clarification", q.state.word()));
        }
        q.pr = r.pr;
        q.short = r.short;
        q.review = r.id;
        q.to = r.reviewer;
        q.to_name = r.reviewer_name;
        q.state = if busy { RequestState::Waiting } else { RequestState::Intended };
        q.reason = if busy { "another request for this pull request is in progress".into() } else { String::new() };
        q.updated_ms = now;
        Ok(())
    }

    /// Finishes a request (done or dismissed); the next one waiting for its
    /// pull request goes on its way.
    pub fn finish(&mut self, request: &str, dismissed: bool, now: u64) -> Result<(), String> {
        let q = self.requests.iter_mut().find(|q| q.id == request).ok_or_else(|| format!("no request {request:?}"))?;
        q.state = if dismissed { RequestState::Dismissed } else { RequestState::Done };
        q.updated_ms = now;
        let pr = q.pr.clone();
        self.release(&pr, now);
        Ok(())
    }

    /// The oldest waiting request of a pull request goes on when none is active.
    pub fn release(&mut self, pr: &str, now: u64) {
        if pr.is_empty() || self.requests.iter().any(|q| q.pr == pr && q.state.active()) {
            return;
        }
        if let Some(next) = self.requests.iter_mut().find(|q| q.pr == pr && q.state == RequestState::Waiting) {
            next.state = RequestState::Intended;
            next.reason.clear();
            next.updated_ms = now;
        }
    }

    /// Conversations watched now (the agent reads only these).
    pub fn watched(&self) -> Vec<&Conversation> {
        self.conversations.iter().filter(|c| c.watching).collect()
    }

    /// Stops watching quiet conversations ([`WATCH_MS`] without activity)
    /// and those whose pull requests all ended (`ended`: by key).
    pub fn retire(&mut self, now: u64, ended: impl Fn(&str) -> bool) -> Vec<String> {
        let reviews = self.reviews.clone();
        let mut stopped = Vec::new();
        for c in self.conversations.iter_mut().filter(|c| c.watching) {
            let prs: Vec<&str> = reviews.iter().filter(|r| c.reviews.contains(&r.id)).map(|r| r.pr.as_str()).collect();
            let quiet = now.saturating_sub(c.active_ms.max(c.created_ms)) > WATCH_MS;
            if quiet || (!prs.is_empty() && prs.iter().all(|k| ended(k))) {
                c.watching = false;
                stopped.push(c.id.clone());
            }
        }
        stopped
    }

    /// Whether to ask for a check now: something is watched and its time came.
    /// The first check is one interval after the first message went out.
    pub fn due(&mut self, now: u64, interval_ms: u64) -> bool {
        if self.watched().is_empty() {
            return false;
        }
        if self.checks.next_ms == 0 {
            let first = self.watched().iter().map(|c| c.created_ms).min().unwrap_or(now);
            self.checks.next_ms = first + interval_ms;
        }
        now >= self.checks.next_ms
    }

    /// The check was asked for: the next one an interval later.
    pub fn asked(&mut self, now: u64, interval_ms: u64) {
        self.checks.asked_ms = now;
        self.checks.next_ms = now + interval_ms;
    }
}

/// The text typed into the review session for a request: who asks for
/// what, the pull request, the earlier review (with its private notes: they
/// stay inside the canvas) and how to hand the new review off.
pub fn request_text(request: &FollowRequest, review: &Review) -> String {
    let notes = if review.notes.is_empty() { String::new() } else { format!(" Your private notes: {}.", excerpt(&review.notes, 300)) };
    let what = match request.kind.as_str() {
        "re-review" => "a re-review",
        "another-review" => "another review",
        _ => "a follow-up on your review",
    };
    format!(
        "{} asks for {what} of PR {} ({}): \"{}\". Your earlier review ({}): {}.{notes} When you have reviewed it again, hand it off: copilot-canvas review handoff --pr {} --author \"{}\" --results <file>. [Canvas: request {}; the review follow-up agent answers the author, not you.]",
        review.author,
        review.short,
        review.url,
        request.text,
        review.outcome,
        excerpt(&review.results, 300),
        review.url,
        review.author,
        request.id
    )
}

// ---------- what clients see ----------

/// A review without its private notes (clients never get them).
fn public(review: &Review) -> Review {
    let mut r = review.clone();
    r.notes.clear();
    r.results = excerpt(&r.results, 400);
    r
}

/// The review follow-ups of the routed canvas (`bus::Snapshot::reviews`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ReviewsView {
    pub canvas: String,
    /// The agents with review follow-ups (enabled and not paused) and their settings.
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub settings: Option<ReviewSettings>,
    /// Why no check is asked for now (empty: checks run).
    #[serde(default)]
    pub paused: String,
    #[serde(default)]
    pub checks: CheckState,
    /// Newest first, without private notes.
    #[serde(default)]
    pub reviews: Vec<Review>,
    #[serde(default)]
    pub conversations: Vec<Conversation>,
    #[serde(default)]
    pub requests: Vec<FollowRequest>,
}

impl ReviewsView {
    pub fn of(canvas: &str, book: &Book, agents: Vec<String>, settings: Option<ReviewSettings>, paused: String) -> Self {
        let mut reviews: Vec<Review> = book.reviews.iter().rev().take(20).map(public).collect();
        reviews.sort_by_key(|r| (!matches!(r.message.state, MessageState::AwaitingApproval | MessageState::NeedsYou | MessageState::Failed), std::cmp::Reverse(r.created_ms)));
        let mut requests: Vec<FollowRequest> = book.requests.iter().rev().take(30).cloned().collect();
        requests.sort_by_key(|q| (!matches!(q.state, RequestState::Ambiguous | RequestState::Blocked | RequestState::Failed), std::cmp::Reverse(q.created_ms)));
        let conversations = book.conversations.iter().rev().take(20).map(|c| Conversation { seen: Vec::new(), ..c.clone() }).collect();
        ReviewsView { canvas: canvas.to_owned(), agents, settings, paused, checks: book.checks.clone(), reviews, conversations, requests }
    }

    /// Things that need you: drafts to approve, messages that could not be sent, unclear or blocked requests.
    pub fn needing_you(&self) -> usize {
        self.reviews.iter().filter(|r| matches!(r.message.state, MessageState::AwaitingApproval | MessageState::NeedsYou | MessageState::Failed)).count()
            + self.requests.iter().filter(|q| matches!(q.state, RequestState::Ambiguous | RequestState::Blocked | RequestState::Failed)).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff(url: &str, results: &str) -> Handoff {
        Handoff {
            pr: PrRef::parse(url).unwrap(),
            title: "Retry uploads".into(),
            author: "Ana Silva".into(),
            author_id: "ana".into(),
            outcome: "changes requested".into(),
            results: results.into(),
            notes: "The retry loop smells like the 2024 outage; check with Ben before saying so".into(),
            links: vec![Link { title: "comment".into(), url: "https://github.com/o/r/pull/7#discussion_r1".into() }],
            reviewer: "rev".into(),
            reviewer_name: "Reviewer".into(),
        }
    }

    const URL: &str = "https://github.com/o/r/pull/7";

    /// A review handed off, drafted, approved and sent to `outbox:ana`.
    fn sent_book() -> (Book, String) {
        let mut book = Book::default();
        let (id, new) = book.handoff(handoff(URL, "Two blocking issues: retries never stop; the 429 path is untested."), 10).unwrap();
        assert!(new);
        book.draft(&id, &format!("Hi Ana, I reviewed {URL}: changes requested. Retries never stop; the 429 path needs a test."), "kf", true, 11).unwrap();
        book.approve(&id, 12).unwrap();
        book.sent(&id, "outbox:ana", "Ana Silva", "outbox", 13).unwrap();
        (book, id)
    }

    #[test]
    fn a_review_is_recorded_once_and_its_message_needs_your_approval() {
        let mut book = Book::default();
        let (id, new) = book.handoff(handoff(URL, "Two issues."), 10).unwrap();
        assert!(new && book.review(&id).unwrap().outcome == "changes-requested");
        assert_eq!(book.handoff(handoff(URL, "Two issues."), 20).unwrap(), (id.clone(), false), "the same hand-off twice is one review");
        assert!(book.handoff(handoff(URL, "  "), 20).unwrap_err().contains("empty"));
        assert!(book.handoff(Handoff { outcome: "lgtm!".into(), ..handoff(URL, "x") }, 20).unwrap_err().contains("--outcome"));
        // Drafts: the link, no private notes, no approval the review did not give.
        assert!(book.draft(&id, "Hi Ana, two issues.", "kf", true, 11).unwrap_err().contains("link the pull request"));
        assert!(book.draft(&id, &format!("Hi Ana, {URL}: the retry loop smells like the 2024 outage"), "kf", true, 11).unwrap_err().contains("private review notes"));
        assert!(book.draft(&id, &format!("Hi Ana, {URL}: LGTM after two fixes"), "kf", true, 11).unwrap_err().contains("not an approval"));
        assert_eq!(book.draft(&id, &format!("Hi Ana, I reviewed {URL}: changes requested."), "kf", true, 11), Ok(MessageState::AwaitingApproval));
        assert!(book.sent(&id, "outbox:ana", "Ana", "outbox", 12).unwrap_err().contains("not approved yet"), "never sent before you approve");
        book.approve(&id, 12).unwrap();
        assert!(book.approve(&id, 12).is_err());
        book.sent(&id, "outbox:ana", "Ana Silva", "outbox", 13).unwrap();
        assert!(book.sent(&id, "outbox:ana", "Ana Silva", "outbox", 14).unwrap_err().contains("sent already"), "no duplicate notification");
        assert!(book.draft(&id, &format!("again {URL}"), "kf", true, 15).unwrap_err().contains("one message per review"));
        let conversation = book.conversation("outbox:ana").unwrap();
        assert!(conversation.watching && conversation.reviews == [id.clone()] && conversation.contact == "ana");
        // Without approval a draft is approved at once; problems are recorded for you.
        let (other, _) = book.handoff(handoff("https://github.com/o/r/pull/8", "One nit."), 20).unwrap();
        assert_eq!(book.draft(&other, "Hi Ana, https://github.com/o/r/pull/8: one nit.", "kf", false, 21), Ok(MessageState::Approved));
        book.problem(&other, "two people named Ana Silva in Teams", true, 22).unwrap();
        assert_eq!(book.review(&other).unwrap().message.state, MessageState::NeedsYou);
        // It survives a restart.
        let again: Book = serde_json::from_str(&serde_json::to_string(&book).unwrap()).unwrap();
        assert_eq!(again, book);
    }

    #[test]
    fn requests_are_associated_once_and_ambiguity_is_surfaced() {
        let (mut book, review) = sent_book();
        let pr = PrRef::parse(URL).unwrap();
        assert!(book.request("outbox:bob", "m1", "re-review", "please", None, 20).unwrap_err().contains("not a conversation the canvas watches"));
        assert!(book.request("outbox:ana", "m1", "lunch", "lunch?", None, 20).unwrap_err().contains("--kind"));
        let Requested::New(first, state) = book.request("outbox:ana", "m1", "re-review", "Pushed fixes, could you re-review?", None, 20).unwrap() else { panic!() };
        assert_eq!(state, RequestState::Intended);
        let q = book.requests.iter().find(|q| q.id == first).unwrap();
        assert_eq!((q.pr.as_str(), q.review.as_str(), q.to.as_str(), q.to_name.as_str()), (pr.key().as_str(), review.as_str(), "rev", "Reviewer"));
        assert_eq!(book.request("outbox:ana", "m1", "re-review", "same message", None, 30).unwrap(), Requested::Duplicate(first.clone(), RequestState::Intended), "a message is processed once");
        // Another request for the same pull request waits: never two reviews of it at once.
        let Requested::New(second, state) = book.request("outbox:ana", "m2", "follow-up", "and one more thing", Some(&pr), 31).unwrap() else { panic!() };
        assert_eq!(state, RequestState::Waiting);
        book.finish(&first, false, 40).unwrap();
        assert_eq!(book.requests.iter().find(|q| q.id == second).unwrap().state, RequestState::Intended, "the next one goes on");
        // A second reviewed pull request in the same conversation: a request without one is ambiguous.
        let (other, _) = book.handoff(handoff("https://github.com/o/r/pull/9", "Fine."), 50).unwrap();
        book.draft(&other, "Hi Ana, https://github.com/o/r/pull/9 looks fine to me apart from naming.", "kf", false, 51).unwrap();
        book.sent(&other, "outbox:ana", "Ana Silva", "outbox", 52).unwrap();
        let Requested::New(unclear, state) = book.request("outbox:ana", "m3", "another-review", "can you look again?", None, 53).unwrap() else { panic!() };
        assert_eq!(state, RequestState::Ambiguous);
        assert!(book.requests.iter().find(|q| q.id == unclear).unwrap().reason.contains("which pull request? o/r#7 and o/r#9 were reviewed for Ana Silva"));
        book.assign(&unclear, &other, 54).unwrap();
        assert_eq!(book.requests.iter().find(|q| q.id == unclear).unwrap().to, "rev");
        // A pull request never reviewed for her is not guessed.
        let stranger = PrRef::parse("https://github.com/o/r/pull/99").unwrap();
        assert!(matches!(book.request("outbox:ana", "m4", "re-review", "?", Some(&stranger), 55).unwrap(), Requested::New(_, RequestState::Ambiguous)));
        // A new review of the pull request completes its requests.
        book.handoff(handoff(URL, "Re-reviewed: all fixed."), 60).unwrap();
        assert!(book.requests.iter().filter(|q| q.pr == pr.key()).all(|q| q.state == RequestState::Done));
    }

    #[test]
    fn checks_are_hourly_while_something_is_watched_and_stop_with_it() {
        let hour = 3_600_000;
        assert!(!Book::default().due(10 * hour, hour), "nothing watched: no checks");
        let (mut book, _) = sent_book();
        assert!(!book.due(13 + hour - 1, hour), "the first check is an hour after the message");
        assert!(book.due(13 + hour, hour));
        book.asked(13 + hour, hour);
        assert!(!book.due(13 + hour + 5, hour));
        book.checked(Some("outbox:ana"), Some("m7"), None, 13 + hour + 60_000).unwrap();
        assert_eq!((book.checks.last_ms, book.conversation("outbox:ana").unwrap().mark.as_str()), (13 + hour + 60_000, "m7"));
        book.checked(None, None, Some("Teams MCP: 401"), 13 + hour + 70_000).unwrap();
        assert_eq!((book.checks.failures, book.checks.last_error.as_str()), (1, "Teams MCP: 401"));
        assert!(book.checked(Some("nope"), None, None, 0).is_err());
        assert!(book.due(13 + 2 * hour, hour));
        // Its pull request merged: no longer watched, no more checks.
        let key = PrRef::parse(URL).unwrap().key();
        assert_eq!(book.retire(13 + 2 * hour, |k| k == key), ["outbox:ana"]);
        assert!(!book.due(13 + 3 * hour, hour));
    }

    #[test]
    fn settings_parse_and_clients_never_see_private_notes() {
        assert_eq!(ReviewSettings::parse("off"), Ok(None));
        let s = ReviewSettings::parse(r"outbox C:\outbox no-approval every 2h recipients: the author's work mail").unwrap().unwrap();
        assert_eq!((s.channel.as_str(), s.tool.as_str(), s.approval, s.check_minutes, s.recipients.as_str()), ("outbox", r"C:\outbox", false, 120, "the author's work mail"));
        assert_eq!(ReviewSettings::parse(&s.text()).unwrap().unwrap(), s, "the prompt shows what it parses");
        let teams = ReviewSettings::parse("mcp teams").unwrap().unwrap();
        assert!(teams.approval && teams.check_minutes == 60, "approval by default, hourly");
        let spaced = ReviewSettings::parse(r"outbox C:\my review outbox approval every 30m").unwrap().unwrap();
        assert_eq!((spaced.tool.as_str(), spaced.check_minutes), (r"C:\my review outbox", 30), "a folder with spaces");
        assert_eq!(ReviewSettings::parse(&spaced.text()).unwrap().unwrap(), spaced);
        assert!(ReviewSettings::parse("mcp").unwrap_err().contains("MCP server"));
        assert!(ReviewSettings::parse("pigeon").unwrap_err().contains("channel"));
        assert_eq!(ReviewSettings::default().missing().as_deref(), Some("its messaging channel is not set yet (its panel's Follow-ups settings)"));
        let (book, _) = sent_book();
        let view = ReviewsView::of("c1", &book, vec!["Review follow-up".into()], Some(teams), String::new());
        assert!(view.reviews.iter().all(|r| r.notes.is_empty()) && !serde_json::to_string(&view).unwrap().contains("2024 outage"));
        let q = FollowRequest { kind: "re-review".into(), text: "please".into(), id: "q-1".into(), ..Default::default() };
        let text = request_text(&q, &book.reviews[0]);
        assert!(text.starts_with("Ana Silva asks for a re-review of PR o/r#7 (https://github.com/o/r/pull/7)") && text.contains("Your private notes:") && text.contains("review handoff --pr"), "{text}");
    }
}
