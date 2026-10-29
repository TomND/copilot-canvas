//! The model a new session starts with. Model ids differ per agent CLI, so
//! the canvas keeps them per agent ([`LaunchDefaults::models`]): a group's
//! override wins, else the canvas's remembered choice, else the initial
//! default, Claude Opus 5.5 ([`initial`]). A session records the model it
//! started with (`Node::model`); resumes keep it.

use crate::agent::AgentKind;
use crate::doc::{Group, LaunchDefaults};

/// What the initial default is, for people.
pub const INITIAL_LABEL: &str = "Claude Opus 5.5";

/// Claude Opus 5.5 as each agent CLI names it: the default until you choose
/// another. Copilot CLI uses GitHub's model ids (dotted versions, as
/// OpenCode's `github-copilot` provider lists them); OpenCode needs
/// `provider/model` (`opencode models`); Claude Code takes the full model name
/// (its alias `opus` follows the newest Opus instead).
pub fn initial(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Copilot => "claude-opus-5.5",
        AgentKind::OpenCode => "github-copilot/claude-opus-5.5",
        AgentKind::Claude => "claude-opus-5-5",
    }
}

/// Where a new session's model comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The launch defaults of the group the session is in (its name).
    Group(String),
    /// Chosen before (new-session dialog or Settings) and kept on the canvas.
    Remembered,
    /// Nothing chosen yet.
    Initial,
}

/// The model a new session gets and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effective {
    /// Model id; empty: the agent's own default (no model flag).
    pub model: String,
    pub source: Source,
}

impl Effective {
    /// The id to pass on the command line (`None`: the agent's own default).
    pub fn flag(&self) -> Option<String> {
        Some(self.model.trim().to_owned()).filter(|m| !m.is_empty())
    }
}

/// The model a new session of `kind` gets in `group` (`None`: no group): the
/// group's override, else the canvas's remembered default, else [`initial`].
pub fn effective(kind: AgentKind, canvas: &LaunchDefaults, group: Option<&Group>) -> Effective {
    if let Some(group) = group
        && let Some(model) = group.defaults.as_ref().and_then(|d| d.model(kind))
    {
        return Effective { model: model.trim().to_owned(), source: Source::Group(group.name.clone()) };
    }
    match canvas.model(kind) {
        Some(model) => Effective { model: model.trim().to_owned(), source: Source::Remembered },
        None => Effective { model: initial(kind).to_owned(), source: Source::Initial },
    }
}

/// A model id the canvas passes on: one word that is not an option.
pub fn valid_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty() && id.len() <= 200 && !id.starts_with('-') && !id.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The agents a canvas's single model (from before models were kept per
/// agent) applied to: `provider/model` ids only fit OpenCode; other ids stay
/// with Copilot and Claude Code, which both got it before.
pub fn legacy_agents(model: &str) -> &'static [AgentKind] {
    if model.contains('/') { &[AgentKind::OpenCode] } else { &[AgentKind::Copilot, AgentKind::Claude] }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults(pairs: &[(AgentKind, &str)]) -> LaunchDefaults {
        let mut d = LaunchDefaults::default();
        for (kind, model) in pairs {
            d.set_model(*kind, Some(model));
        }
        d
    }

    #[test]
    fn nothing_chosen_means_opus_5_5_in_each_agents_own_id() {
        let canvas = LaunchDefaults::default();
        for (kind, id) in [(AgentKind::Copilot, "claude-opus-5.5"), (AgentKind::OpenCode, "github-copilot/claude-opus-5.5"), (AgentKind::Claude, "claude-opus-5-5")] {
            let e = effective(kind, &canvas, None);
            assert_eq!((e.model.as_str(), &e.source, e.flag().as_deref()), (id, &Source::Initial, Some(id)), "{kind:?}");
            assert!(valid_id(id));
        }
    }

    #[test]
    fn a_remembered_choice_wins_per_agent_and_a_group_override_wins_over_it() {
        let canvas = defaults(&[(AgentKind::OpenCode, "github-copilot/gpt-6-sol"), (AgentKind::Claude, "")]);
        assert_eq!(effective(AgentKind::OpenCode, &canvas, None), Effective { model: "github-copilot/gpt-6-sol".into(), source: Source::Remembered });
        let own = effective(AgentKind::Claude, &canvas, None);
        assert_eq!((&own.source, own.flag()), (&Source::Remembered, None), "an empty choice is the agent's own default");
        assert_eq!(effective(AgentKind::Copilot, &canvas, None).source, Source::Initial, "other agents keep the initial default");
        let group = Group { name: "Team".into(), defaults: Some(defaults(&[(AgentKind::OpenCode, "opencode/big-pickle")])), ..Default::default() };
        assert_eq!(effective(AgentKind::OpenCode, &canvas, Some(&group)), Effective { model: "opencode/big-pickle".into(), source: Source::Group("Team".into()) });
        assert_eq!(effective(AgentKind::Claude, &canvas, Some(&group)).source, Source::Remembered, "a group without an override for that agent inherits");
        let plain = Group { name: "Plain".into(), ..Default::default() };
        assert_eq!(effective(AgentKind::OpenCode, &canvas, Some(&plain)).source, Source::Remembered);
    }

    #[test]
    fn ids_must_be_one_word_and_never_an_option() {
        for good in ["opus", "github-copilot/claude-opus-5.5", "claude-opus-5-5[1m]"] {
            assert!(valid_id(good), "{good}");
        }
        for bad in ["", "  ", "-m", "--model", "two words", "a\u{7}b"] {
            assert!(!valid_id(bad), "{bad:?}");
        }
        assert_eq!(legacy_agents("opencode/big-pickle"), [AgentKind::OpenCode]);
        assert_eq!(legacy_agents("claude-opus-4.5"), [AgentKind::Copilot, AgentKind::Claude]);
    }
}
