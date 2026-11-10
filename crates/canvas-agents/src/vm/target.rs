//! Where an agent runs, for the command line that starts it (plan VM-26):
//! this machine, or a VM's guest, where nothing of this machine exists and
//! the canvas's files are the roots the hub syncs (`vm::sync`), named by
//! [`ROOTS`](super::link::ROOTS) until the satellite's folder is known.

use super::link::ROOTS;

/// Where an agent's command runs.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ExecTarget {
    #[default]
    Local,
    /// A VM's guest: the synced roots it may use.
    Vm(GuestRoots),
}

/// The synced roots a VM box's agent gets (the canvas's plugin always, when
/// the satellite syncs: the hooks lane has none).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuestRoots {
    /// The canvas's own plugin (`plugin`).
    pub plugin: bool,
    /// The user's profile bundle (`profile`), and its generated instructions.
    pub profile: bool,
    pub profile_instructions: bool,
    /// The group's slug (`notes/<group>`, `instructions/<group>`).
    pub group: Option<String>,
    /// The box's group skill plugin (`group-skills/<box>`).
    pub group_skills: bool,
    /// The box has an MCP preset: Claude Code's configuration is synced (`sync::CLAUDE_MCP`).
    pub mcp: bool,
}

/// A guest path under the roots: `%ROOTS%/<rel>`.
pub fn guest(rel: &str) -> String {
    format!("{ROOTS}/{rel}")
}

impl GuestRoots {
    /// The plugins the agent loads (`--plugin-dir`).
    pub fn plugins(&self, id: &str) -> Vec<String> {
        let mut out = Vec::new();
        if self.plugin {
            out.push(guest("plugin"));
        }
        if self.profile {
            out.push(guest("profile"));
        }
        if self.group_skills {
            out.push(guest(&format!("group-skills/{id}")));
        }
        out
    }

    /// The folders the agent may write besides its workspace (`--add-dir`): the group's notes.
    pub fn add_dirs(&self) -> Vec<String> {
        self.group.iter().flat_map(|g| [canvas_model::doc::AI_DOCS, canvas_model::doc::HUMAN_OUTPUTS].map(|d| guest(&format!("notes/{g}/{d}")))).collect()
    }

    /// Claude Code's instructions (`--append-system-prompt-file`): the group's.
    pub fn instructions_file(&self) -> Option<String> {
        self.group.as_ref().map(|g| guest(&format!("instructions/{g}/{}", super::sync::GROUP_INSTRUCTIONS)))
    }

    /// Copilot's instruction folders (`COPILOT_CUSTOM_INSTRUCTIONS_DIRS`).
    pub fn instructions(&self) -> Vec<String> {
        let mut out: Vec<String> = self.group.iter().map(|g| guest(&format!("instructions/{g}"))).collect();
        if self.profile_instructions {
            out.push(guest("profile/instructions"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vm_boxs_roots_are_guest_paths_under_the_roots() {
        let roots = GuestRoots { plugin: true, profile: true, profile_instructions: true, group: Some("design".into()), group_skills: true, mcp: false };
        assert_eq!(roots.plugins("b1"), ["%ROOTS%/plugin", "%ROOTS%/profile", "%ROOTS%/group-skills/b1"]);
        assert_eq!(roots.add_dirs(), ["%ROOTS%/notes/design/ai-docs", "%ROOTS%/notes/design/human-outputs"]);
        assert_eq!(roots.instructions(), ["%ROOTS%/instructions/design", "%ROOTS%/profile/instructions"]);
        assert!(GuestRoots::default().plugins("b1").is_empty() && GuestRoots::default().add_dirs().is_empty());
    }
}
