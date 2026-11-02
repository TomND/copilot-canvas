//! What the service should launch for a session box: shared by the desktop
//! clients and the iPhone client.

use canvas_common::protocol::AgentLaunch;
use serde::{Deserialize, Serialize};

use crate::agent::{AgentKind, agent_of};
use crate::doc::{Group, LaunchDefaults, Node};

/// The environment entry that carries a launch's skill folders to the
/// service, as JSON ([`GroupSkills`]). `AgentLaunch` travels postcard-encoded
/// (no field can be added without breaking older peers), its `env` already
/// does; the service takes the entry out before the agent starts, and an
/// older service passes it on to the agent, where it does nothing.
pub const GROUP_SKILLS_ENV: &str = "COPILOT_CANVAS_GROUP_SKILLS";

/// The skill folders a launch gets (`LaunchDefaults::skill_dirs` of the
/// canvas and the box's group, combined).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct GroupSkills {
    /// The group's id (empty: the box is in no group).
    #[serde(default)]
    pub group: String,
    pub dirs: Vec<String>,
}

/// Adds skill folders to a launch (nothing when there are none).
pub fn attach_group_skills(launch: &mut AgentLaunch, group: &str, dirs: &[String]) {
    let dirs = crate::doc::union_dirs(dirs, &[]);
    if dirs.is_empty() {
        return;
    }
    let value = GroupSkills { group: group.to_owned(), dirs };
    launch.env.retain(|(k, _)| !k.eq_ignore_ascii_case(GROUP_SKILLS_ENV));
    launch.env.push((GROUP_SKILLS_ENV.to_owned(), serde_json::to_string(&value).unwrap_or_default()));
}

/// Takes a launch's skill folders out of its environment (the last entry
/// counts; one that is not valid is dropped all the same). Names match in
/// any case, as Windows reads them.
pub fn take_group_skills(launch: &mut AgentLaunch) -> Option<GroupSkills> {
    let mut found = None;
    launch.env.retain(|(k, v)| {
        if !k.eq_ignore_ascii_case(GROUP_SKILLS_ENV) {
            return true;
        }
        found = serde_json::from_str::<GroupSkills>(v).ok().filter(|s| !s.dirs.is_empty());
        false
    });
    found
}

/// What to launch for an agent session box. The service adds the canvas
/// integration (skills plugin, identity, `copilot-canvas` on PATH) and turns
/// it into the agent's command line.
///
/// Launch defaults (canvas, overridden by the box's group) apply to new
/// conversations only, and only those the agent supports; permissions, skill
/// folders and the box's MCP choice apply to every run. The model: a new
/// conversation gets the box's own (chosen in the new-session dialog), else
/// the effective default ([`crate::models::effective`]); a resume or fork
/// carries the model recorded on the box (never the current default), which
/// the provider passes only where its agent does not keep it itself.
/// Canvas-wide agents get their own skills, never skill folders.
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
    let group_id = group.map(|g| g.id.clone()).unwrap_or_default();
    let group = group.filter(|g| g.folder.is_some());
    let mut launch = AgentLaunch {
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
    };
    if !node.is_canvas_agent() {
        attach_group_skills(&mut launch, &group_id, &defaults.skill_dirs);
    }
    launch
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(agent: &str) -> Node {
        let mut node: Node = serde_json::from_value(serde_json::json!({ "id": "12345678-1234-1234-1234-123456789abc", "kind": "session", "name": "Fix it", "x": 0, "y": 0 })).unwrap();
        node.agent = Some(agent.to_owned());
        node
    }

    #[test]
    fn every_launch_in_a_group_carries_the_canvas_and_group_skill_folders() {
        let canvas = LaunchDefaults { skill_dirs: vec![r"C:\all".into()], ..Default::default() };
        let group = Group { id: "g-1".into(), name: "Payments".into(), defaults: Some(LaunchDefaults { skill_dirs: vec![r"D:\pay".into(), r"c:\ALL".into()], ..Default::default() }), ..Default::default() };
        for kind in crate::agent::ALL {
            for (resume, fork) in [(false, None), (true, None), (false, Some("parent".to_owned()))] {
                let mut launch = agent_launch(&node(kind.id()), &canvas, Some(&group), None, fork, resume);
                assert_eq!(launch.group_folder, None, "a group without a folder still gets its skills");
                let skills = take_group_skills(&mut launch).expect("skill folders");
                assert_eq!(skills, GroupSkills { group: "g-1".into(), dirs: vec![r"C:\all".into(), r"D:\pay".into()] }, "{kind:?} resume={resume}");
                assert!(launch.env.is_empty(), "taken out before the agent starts");
                assert_eq!(take_group_skills(&mut launch), None);
            }
        }
        // No group: the canvas's own; none anywhere: nothing.
        let mut alone = agent_launch(&node("claude"), &canvas, None, None, None, false);
        assert_eq!(take_group_skills(&mut alone).map(|s| (s.group, s.dirs)), Some((String::new(), vec![r"C:\all".to_owned()])));
        assert!(agent_launch(&node("copilot"), &LaunchDefaults::default(), None, None, None, false).env.is_empty());
        // A canvas-wide agent's chat gets its own skills only.
        let mut wide = node("copilot");
        wide.passive = Some(crate::doc::Passive { scope: crate::doc::PassiveScope::Canvas, ..Default::default() });
        assert!(agent_launch(&wide, &canvas, Some(&group), None, None, true).env.is_empty());
    }

    #[test]
    fn the_skill_folder_entry_survives_the_wire_and_bad_entries_are_dropped() {
        let mut launch = AgentLaunch { agent: "opencode".into(), env: vec![("A".into(), "1".into())], ..Default::default() };
        attach_group_skills(&mut launch, "g", &[" ".into()]);
        assert_eq!(launch.env.len(), 1, "no folders: no entry");
        attach_group_skills(&mut launch, "g", &[r"C:\s".into()]);
        attach_group_skills(&mut launch, "g", &[r"C:\t".into()]);
        assert_eq!(launch.env.iter().filter(|(k, _)| k == GROUP_SKILLS_ENV).count(), 1, "set once");
        // AgentLaunch travels as postcard inside ClientMsg::Launch: its shape is unchanged.
        let message = canvas_common::protocol::ClientMsg::Launch { id: "t".into(), kind: canvas_common::protocol::LaunchKind::Agent(Box::new(launch.clone())), cwd: r"C:\w".into(), cols: 80, rows: 24 };
        let mut bytes = Vec::new();
        canvas_common::protocol::write_frame(&mut bytes, &message).unwrap();
        let back: canvas_common::protocol::ClientMsg = canvas_common::protocol::read_frame(&mut std::io::Cursor::new(bytes)).unwrap().unwrap();
        let canvas_common::protocol::ClientMsg::Launch { kind: canvas_common::protocol::LaunchKind::Agent(mut back), cwd, .. } = back else { panic!("a launch") };
        assert_eq!(cwd, r"C:\w");
        assert_eq!(take_group_skills(&mut back).map(|s| s.dirs), Some(vec![r"C:\t".to_owned()]));
        assert_eq!(back.env, [("A".to_owned(), "1".to_owned())]);
        let mut bad = AgentLaunch { env: vec![(GROUP_SKILLS_ENV.into(), "not json".into())], ..Default::default() };
        assert_eq!(take_group_skills(&mut bad), None);
        assert!(bad.env.is_empty(), "never reaches the agent");
        // Windows reads variable names in any case: such an entry never reaches the agent either.
        let mut cased = AgentLaunch { env: vec![("copilot_canvas_group_skills".into(), r#"{"dirs":["C:\\s"]}"#.into()), ("B".into(), "2".into())], ..Default::default() };
        assert_eq!(take_group_skills(&mut cased).map(|s| s.dirs), Some(vec![r"C:\s".to_owned()]));
        assert_eq!(cased.env, [("B".to_owned(), "2".to_owned())]);
        let mut twice = AgentLaunch { env: vec![("Copilot_Canvas_Group_Skills".into(), "x".into())], ..Default::default() };
        attach_group_skills(&mut twice, "g", &[r"C:\s".into()]);
        assert_eq!(twice.env.len(), 1, "replaced, whatever its case");
    }
}
