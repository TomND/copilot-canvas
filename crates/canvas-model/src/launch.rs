//! What the service should launch for a session box: shared by the desktop
//! clients and the iPhone client.

use canvas_common::protocol::AgentLaunch;

use crate::agent::{AgentKind, agent_of};
use crate::doc::{Group, LaunchDefaults, Node};

/// What to launch for an agent session box. The service adds the canvas
/// integration (skills plugin, identity, `copilot-canvas` on PATH) and turns
/// it into the agent's command line.
///
/// Launch defaults (canvas, overridden by the box's group) apply to new
/// conversations only, and only those the agent supports; permissions and the
/// box's MCP choice apply to every run. The model: a new conversation gets the
/// box's own (chosen in the new-session dialog), else the effective default
/// ([`crate::models::effective`]); a resume or fork carries the model
/// recorded on the box (never the current default), which the provider passes
/// only where its agent does not keep it itself.
pub fn agent_launch(
    node: &Node,
    canvas: &LaunchDefaults,
    group: Option<&Group>,
    prompt: Option<String>,
    fork_from: Option<String>,
    resume: bool,
) -> AgentLaunch {
    let kind = agent_of(node);
    let caps = kind.caps();
    let defaults = canvas.merged(group.and_then(|g| g.defaults.as_ref()));
    let fresh = |value: &Option<String>, supported: bool| value.clone().filter(|v| !resume && supported && !v.trim().is_empty());
    let model = if resume || fork_from.is_some() {
        node.model.clone()
    } else {
        node.model.clone().or_else(|| Some(crate::models::effective(kind, canvas, group).model))
    };
    let group = group.filter(|g| g.folder.is_some());
    AgentLaunch {
        agent: kind.id().to_owned(),
        native_id: node.agent_session.clone().filter(|s| !s.is_empty()),
        resume,
        fork_from,
        name: Some(node.name.clone()).filter(|n| !n.trim().is_empty()),
        prompt: prompt.filter(|p| !p.trim().is_empty()),
        model: model.map(|m| m.trim().to_owned()).filter(|m| !m.is_empty()),
        effort: fresh(&defaults.reasoning_effort, caps.effort),
        mode: fresh(&defaults.mode, caps.modes),
        custom_agent: fresh(&defaults.agent, true),
        context: fresh(&defaults.context, kind == AgentKind::Copilot),
        allow_all: defaults.permissions.as_deref() != Some("ask"),
        add_dirs: if resume { Vec::new() } else { defaults.add_dirs.iter().filter(|d| !d.trim().is_empty()).cloned().collect() },
        mcp_off: node.mcp_off.clone().unwrap_or_default(),
        group_folder: group.and_then(|g| g.folder.clone()),
        group_name: group.map(|g| g.name.clone()),
        headless_prompt: None,
        env: Vec::new(),
    }
}
