//! User-facing status derived from history, live locks and owned terminals.

use ratatui::style::Color;

use crate::paint::{ATTENTION, ATTENTION_BG, DARK, FINISHED, FINISHED_BG, GRAY, NEGATIVE, PURPLE, SPINNER, TEXT};
use crate::protocol::{Activity, AgentStatus, SessionSummary, TermInfo};

pub const RECENT_MS: u64 = 120_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    Working,
    Waiting,
    JustFinished,
    Idle,
    Running,
    External,
    Stopped,
    Failed,
    Preparing,
    Unknown,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Working => "working",
            Status::Waiting => "needs you",
            Status::JustFinished => "done · review",
            Status::Idle => "idle",
            Status::Running => "running",
            Status::External => "open elsewhere",
            Status::Stopped => "stopped",
            Status::Failed => "failed",
            Status::Preparing => "preparing",
            Status::Unknown => "unknown",
        }
    }

    pub fn icon(self, tick: usize) -> &'static str {
        match self {
            Status::Working | Status::Preparing => SPINNER[tick % SPINNER.len()],
            Status::Waiting => "◆",
            Status::JustFinished => "✓",
            Status::Idle => "○",
            Status::Running => "●",
            Status::External => "↗",
            Status::Stopped => "■",
            Status::Failed => "✗",
            Status::Unknown => "·",
        }
    }

    pub fn color(self) -> Color {
        match self {
            Status::Working | Status::Running | Status::Preparing => TEXT,
            Status::Waiting => ATTENTION,
            Status::JustFinished => FINISHED,
            Status::Idle => GRAY,
            Status::External => PURPLE,
            Status::Stopped | Status::Unknown => DARK,
            Status::Failed => NEGATIVE,
        }
    }

    /// Border colour and compact-zoom background for states that want your eyes.
    pub fn attention(self) -> Option<(Color, Color)> {
        match self {
            Status::Waiting => Some((ATTENTION, ATTENTION_BG)),
            Status::JustFinished => Some((FINISHED, FINISHED_BG)),
            _ => None,
        }
    }

    pub fn animated(self) -> bool {
        matches!(self, Status::Working | Status::Preparing)
    }
}

/// `reviewed_ms`: when you last looked at the session; a finished turn stays
/// "done · review" until then (no time limit).
pub fn session_status(summary: Option<&SessionSummary>, term: Option<&TermInfo>, reviewed_ms: Option<u64>) -> Status {
    if let Some(term) = term
        && !term.alive
    {
        return if term.exit_code == Some(0) { Status::Stopped } else { Status::Failed };
    }
    let owned = term.is_some_and(|t| t.alive);
    let live = owned || summary.is_some_and(|s| s.live);
    if live && summary.is_some_and(|s| s.pending > 0) {
        return Status::Waiting;
    }
    // The program's own progress indicator (OSC 9;4) is immediate: Copilot sets
    // it when a turn starts and clears it when the turn ends.
    let progress = term.filter(|t| t.alive && t.progress_seen);
    if progress.is_some_and(|t| matches!(t.progress, 1 | 3)) {
        return Status::Working;
    }
    // Subagents still running after the main turn ended (background agents).
    if live && summary.is_some_and(|s| s.agents.iter().any(|a| a.status == AgentStatus::Working)) {
        return Status::Working;
    }
    let Some(summary) = summary else {
        return if owned { Status::Running } else { Status::Stopped };
    };
    if !owned && !summary.live {
        return if summary.activity == Activity::Failed { Status::Failed } else { Status::Stopped };
    }
    if summary.pending > 0 {
        return Status::Waiting;
    }
    // The progress report cleared after the newest history event: the turn has
    // ended even though the history has not settled yet.
    let (activity, finished_ms) = match progress {
        Some(t) if summary.activity == Activity::Working && t.progress == 0 && t.progress_ms + 2_000 >= summary.last_event_ms => {
            (Activity::Idle, Some(t.progress_ms))
        }
        _ => (summary.activity, summary.finished_ms),
    };
    match activity {
        Activity::Failed => Status::Failed,
        _ if finished_ms.is_some_and(|f| reviewed_ms.is_some_and(|r| f > r)) => Status::JustFinished,
        Activity::Idle => Status::Idle,
        Activity::Working => Status::Working,
        _ if owned => Status::Running,
        _ => Status::External,
    }
}

pub fn agent_status(status: AgentStatus, finished_ms: Option<u64>, parent_running: bool, now: u64) -> Status {
    match status {
        AgentStatus::Working if parent_running => Status::Working,
        AgentStatus::Working => Status::Stopped,
        AgentStatus::Finished if finished_ms.is_some_and(|f| now >= f && now - f < RECENT_MS) => Status::JustFinished,
        AgentStatus::Finished => Status::Idle,
        AgentStatus::Failed => Status::Failed,
        // Interrupted by a restart of its session.
        AgentStatus::Unknown => Status::Stopped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(activity: Activity, live: bool) -> SessionSummary {
        SessionSummary { activity, live, tracked: true, ..Default::default() }
    }

    fn term(alive: bool, code: Option<u32>) -> TermInfo {
        TermInfo { alive, exit_code: code, ..Default::default() }
    }

    #[test]
    fn owned_terminal_exit_overrides_history() {
        let s = summary(Activity::Working, true);
        assert_eq!(session_status(Some(&s), Some(&term(false, Some(0))), None), Status::Stopped);
        assert_eq!(session_status(Some(&s), Some(&term(false, Some(3))), None), Status::Failed);
    }

    #[test]
    fn not_running_is_stopped_regardless_of_stale_working_history() {
        assert_eq!(session_status(Some(&summary(Activity::Working, false)), None, None), Status::Stopped);
    }

    #[test]
    fn pending_and_recent_completion_win() {
        let mut s = summary(Activity::Working, true);
        s.pending = 1;
        assert_eq!(session_status(Some(&s), None, None), Status::Waiting);
        let mut s = summary(Activity::Idle, true);
        s.finished_ms = Some(1_000);
        assert_eq!(session_status(Some(&s), None, Some(500)), Status::JustFinished, "finished after you last looked");
        assert_eq!(session_status(Some(&s), None, Some(1_000)), Status::Idle, "reviewed");
        assert_eq!(session_status(Some(&s), None, None), Status::Idle, "no baseline yet: old history never lights up");
    }

    #[test]
    fn progress_report_means_working_unless_waiting() {
        let busy = TermInfo { alive: true, progress: 3, progress_seen: true, ..Default::default() };
        assert_eq!(session_status(Some(&summary(Activity::Idle, true)), Some(&busy), None), Status::Working);
        let mut s = summary(Activity::Working, true);
        s.pending = 1;
        assert_eq!(session_status(Some(&s), Some(&busy), None), Status::Waiting);
        assert_eq!(session_status(None, Some(&busy), None), Status::Working, "shells without history too");
        let done = TermInfo { alive: true, progress: 0, ..Default::default() };
        assert_eq!(session_status(Some(&summary(Activity::Idle, true)), Some(&done), None), Status::Idle);
    }

    #[test]
    fn progress_clear_finishes_at_once_and_agents_keep_a_session_working() {
        let mut s = summary(Activity::Working, true);
        s.last_event_ms = 10_000;
        let cleared = TermInfo { alive: true, progress_seen: true, progress: 0, progress_ms: 10_500, ..Default::default() };
        assert_eq!(session_status(Some(&s), Some(&cleared), Some(1_000)), Status::JustFinished, "turn ended per the progress report");
        assert_eq!(session_status(Some(&s), Some(&cleared), Some(20_000)), Status::Idle, "already looked at");
        let stale = TermInfo { progress_ms: 1_000, ..cleared.clone() };
        assert_eq!(session_status(Some(&s), Some(&stale), Some(0)), Status::Working, "newer history (a new prompt) wins");
        let unseen = TermInfo { alive: true, ..Default::default() };
        assert_eq!(session_status(Some(&s), Some(&unseen), Some(0)), Status::Working, "programs without progress reports use the history");
        let mut idle = summary(Activity::Idle, true);
        idle.agents.push(crate::protocol::AgentSummary { key: "a".into(), name: "Explorer".into(), description: String::new(), status: AgentStatus::Working, finished_ms: None });
        assert_eq!(session_status(Some(&idle), Some(&cleared), None), Status::Working, "background subagent still running");
        assert_eq!(agent_status(AgentStatus::Unknown, None, true, 0), Status::Stopped);
    }

    #[test]
    fn live_elsewhere_without_activity_is_external() {
        assert_eq!(session_status(Some(&summary(Activity::Unknown, true)), None, None), Status::External);
        assert_eq!(session_status(Some(&summary(Activity::Unknown, false)), Some(&term(true, None)), None), Status::Running);
    }

    #[test]
    fn agents_stop_with_their_parent() {
        assert_eq!(agent_status(AgentStatus::Working, None, false, 0), Status::Stopped);
        assert_eq!(agent_status(AgentStatus::Working, None, true, 0), Status::Working);
        assert_eq!(agent_status(AgentStatus::Finished, Some(10), true, 20), Status::JustFinished);
    }
}
