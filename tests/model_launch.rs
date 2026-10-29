//! The model a launch carries, end to end: the canvas's launch of a box (a new
//! conversation gets the box's choice or the effective default; a resume the
//! model recorded on the box) through each agent's provider into its command
//! line. A resume never gets the current default.

use canvas_agents::agents::claude::Claude;
use canvas_agents::agents::copilot::Copilot;
use canvas_agents::agents::opencode::OpenCode;
use canvas_agents::agents::{Integration, Provider};
use canvas_core::side::agent_launch;
use canvas_model::agent::{ALL, AgentKind};
use canvas_model::doc::{LaunchDefaults, Node};

const ID: &str = "12345678-1234-1234-1234-123456789abc";
const SES: &str = "ses_e7fbbd911ffeOynBIm3IrWFsUl";

fn node(kind: AgentKind, model: Option<&str>) -> Node {
    let mut node: Node = serde_json::from_value(serde_json::json!({ "id": ID, "kind": "session", "name": "Model check", "x": 0, "y": 0, "agent": kind.id() })).unwrap();
    node.model = model.map(str::to_owned);
    if kind == AgentKind::OpenCode {
        node.agent_session = Some(SES.into());
    }
    node
}

/// The model flag's value on the command line of `node`'s launch.
fn launched_model(node: &Node, canvas: &LaunchDefaults, resume: bool) -> Option<String> {
    // SAFETY: this test binary only reads these variables.
    unsafe {
        std::env::set_var("COPILOT_CANVAS_COPILOT", r"C:\bin\copilot.exe");
        std::env::set_var("COPILOT_CANVAS_OPENCODE", r"C:\bin\opencode.exe");
        std::env::set_var("COPILOT_CANVAS_CLAUDE", r"C:\bin\claude.exe");
    }
    let launch = agent_launch(node, canvas, None, None, None, resume);
    let integration = Integration::default();
    let kind = AgentKind::parse(&launch.agent).unwrap();
    let args = match kind {
        AgentKind::Copilot => Copilot.command(ID, &launch, &integration).unwrap().args,
        AgentKind::OpenCode => OpenCode.command_with(ID, &launch, &integration, false).unwrap().args,
        AgentKind::Claude => Claude.command(ID, &launch, &integration).unwrap().args,
    };
    let flag = if kind == AgentKind::OpenCode { "-m" } else { "--model" };
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

#[test]
fn new_sessions_start_with_the_default_and_resumes_keep_their_model() {
    let nothing = LaunchDefaults::default();
    let mut later = LaunchDefaults::default();
    for kind in ALL {
        later.set_model(kind, Some(&format!("later-{}", kind.id())));
    }
    for kind in ALL {
        let initial = canvas_model::models::initial(kind);
        // New: the initial default (Opus 5.5 in the agent's own id), the remembered one, or the box's choice.
        assert_eq!(launched_model(&node(kind, None), &nothing, false).as_deref(), Some(initial), "{kind:?} new");
        assert_eq!(launched_model(&node(kind, None), &later, false), Some(format!("later-{}", kind.id())), "{kind:?} new, remembered default");
        assert_eq!(launched_model(&node(kind, Some("chosen")), &later, false).as_deref(), Some("chosen"), "{kind:?} new, chosen in the dialog");
        // Resume: never the current default. Claude Code is given its recorded model; Copilot and OpenCode keep theirs themselves.
        let resumed = launched_model(&node(kind, Some(initial)), &later, true);
        match kind {
            AgentKind::Claude => assert_eq!(resumed.as_deref(), Some(initial), "Claude Code resumes with its recorded model"),
            AgentKind::Copilot | AgentKind::OpenCode => assert_eq!(resumed, None, "{kind:?} keeps the session's model itself"),
        }
        assert_eq!(launched_model(&node(kind, None), &later, true), None, "{kind:?}: not recorded, no model flag on resume");
    }
}
