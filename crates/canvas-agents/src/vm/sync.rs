//! What the hub keeps in sync in a VM's guest (plan VM-34): the roots of the
//! boxes it runs, each a folder of files the hub decides.
//!
//! | root | from | the guest's edits |
//! |---|---|---|
//! | `plugin` | the canvas's plugin (`<home>/plugin`) | — |
//! | `canvas` | the routing and the todo book, laid out as a home (`bus/routing.json`, `canvas-agents/<canvas>/todos/book.json`): the guest's reads when the hub is away | — |
//! | `profile` | the pack's profile bundle (`vm::profile`) | — |
//! | `group-skills/<box>` | the box's group skill plugin | — |
//! | `instructions/<group>` | the group's instructions, with the guest's paths | — |
//! | `notes/<group>` | the group's `ai-docs/` and `human-outputs/` | written back |
//! | `paths/<n>` | a folder of `paths.yaml` (at its guest path) | written back when `rw-notes` |
//!
//! [`Root`]s say what a root holds; `vm::link` sends them and writes the
//! guest's edits with [`write`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use canvas_common::sat::{NoteAnswer, name_ok, rev_of};

use super::launch::Intent;
use super::pack::Pack;

/// At most per root: files, bytes, bytes a file.
const MAX_FILES: usize = 2000;
const MAX_BYTES: u64 = 64 << 20;
const MAX_FILE: u64 = 8 << 20;

/// Where a file's content comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    File(PathBuf),
    Bytes(Vec<u8>),
}

impl Source {
    pub fn read(&self) -> Option<Vec<u8>> {
        match self {
            Source::File(path) => std::fs::read(path).ok(),
            Source::Bytes(bytes) => Some(bytes.clone()),
        }
    }
}

/// A root as the hub has it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Root {
    pub id: String,
    /// An absolute guest path (empty: under the guest's roots).
    pub target: String,
    pub files: BTreeMap<String, Source>,
    /// Where the guest's edits are written (`None`: read-only).
    pub writes: Option<Writes>,
    /// Folders it always has in the guest.
    pub folders: Vec<String>,
}

/// A writable root's folder on the hub and the names the guest may write.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Writes {
    pub dir: PathBuf,
    /// Name prefixes the guest may write (`ai-docs/`, …; empty: any).
    pub prefixes: Vec<String>,
}

impl Writes {
    fn allows(&self, name: &str) -> bool {
        name_ok(name) && (self.prefixes.is_empty() || self.prefixes.iter().any(|p| name.starts_with(p.as_str())))
    }
}

/// The files of `dir` whose names start with one of `prefixes` (empty:
/// all): no links, names the guest can keep, within the root's bounds.
pub fn folder(dir: &Path, prefixes: &[&str]) -> BTreeMap<String, Source> {
    let mut out = BTreeMap::new();
    let mut total = 0u64;
    walk(dir, dir, 0, prefixes, &mut out, &mut total);
    out
}

fn walk(dir: &Path, base: &Path, depth: usize, prefixes: &[&str], out: &mut BTreeMap<String, Source>, total: &mut u64) {
    if depth > 12 || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(base) else { continue };
        let name = rel.to_string_lossy().replace('\\', "/");
        let wanted = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p) || p.starts_with(&format!("{name}/")));
        if meta.is_dir() {
            if wanted(&name) {
                walk(&path, base, depth + 1, prefixes, out, total);
            }
        } else if meta.is_file() && wanted(&name) && name_ok(&name) && meta.len() <= MAX_FILE && *total + meta.len() <= MAX_BYTES && out.len() < MAX_FILES {
            *total += meta.len();
            out.insert(name, Source::File(path));
        }
    }
}

/// What the hub knows of a guest from its satellite's `Hello`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Guest {
    /// Where the roots are (`HelloInfo::roots`).
    pub roots: String,
    /// The guest user's home.
    pub home: String,
    /// The guest's MCP proxy (0: none).
    pub mcp_port: u16,
}

/// The guest's Copilot MCP configuration (`mcp-config.json`): each server of
/// the box's preset that has a policy, at the guest's MCP proxy, for this box
/// (`None`: no preset, no proxy, or no server).
/// The group's instructions file in its `instructions/<group>` root.
pub const GROUP_INSTRUCTIONS: &str = "canvas-group.instructions.md";

/// Claude Code's MCP configuration in the guest (under the roots): the
/// box's preset at the satellite's proxy (`--mcp-config`).
pub const CLAUDE_MCP: &str = "agent-mcp/claude.json";

/// [`mcp_config`] for Claude Code (`type: http` with a URL, no tool list).
pub fn claude_mcp_config(pack: &Pack, intent: &Intent, mcp_port: u16) -> Option<String> {
    let copilot: serde_json::Value = serde_json::from_str(&mcp_config(pack, intent, mcp_port)?).ok()?;
    let servers: serde_json::Map<String, serde_json::Value> = copilot["mcpServers"].as_object()?.iter().map(|(name, server)| (name.clone(), serde_json::json!({ "type": "http", "url": server["url"] }))).collect();
    serde_json::to_string_pretty(&serde_json::json!({ "mcpServers": servers })).ok()
}

pub fn mcp_config(pack: &Pack, intent: &Intent, mcp_port: u16) -> Option<String> {
    let preset = intent.placement.mcp.as_deref().filter(|p| !p.is_empty())?;
    let servers: Vec<&String> = pack.mcp.presets.get(preset)?.iter().filter(|s| pack.mcp.servers.contains_key(*s)).collect();
    if servers.is_empty() || mcp_port == 0 || !super::launch::valid_box(&intent.r#box) {
        return None;
    }
    let entries: serde_json::Map<String, serde_json::Value> = servers
        .into_iter()
        .map(|server| (server.clone(), serde_json::json!({ "type": "http", "url": format!("http://127.0.0.1:{mcp_port}/mcp/{}/{server}", intent.r#box), "tools": ["*"] })))
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({ "mcpServers": entries })).ok()
}

/// A group's name as one path component.
pub fn group_slug(name: &str) -> String {
    let slug: String = name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let slug = slug.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    let slug: String = slug.chars().take(40).collect();
    if slug.is_empty() { "group".into() } else { slug }
}

/// The roots of a sandbox whose guest keeps them under `guest_roots`, for
/// the boxes it runs (`intents`), the canvas's `home` and the user's home.
pub fn roots_for(home: &Path, pack: &Pack, user_home: &Path, guest: &Guest, intents: &[Intent]) -> Vec<Root> {
    let guest_roots = guest.roots.as_str();
    let mut roots = Vec::new();
    // The guest's Copilot MCP configuration: its home's `.copilot/mcp-config.json` (the VM is the
    // box's; with boxes sharing it, the first one's servers).
    if !guest.home.is_empty()
        && let Some(config) = intents.iter().find_map(|i| mcp_config(pack, i, guest.mcp_port))
    {
        let target = format!("{}/.copilot", guest.home.trim_end_matches(['/', '\\']));
        roots.push(Root { id: "mcp".into(), target, files: [("mcp-config.json".to_owned(), Source::Bytes(config.into_bytes()))].into(), ..Default::default() });
    }
    // Claude Code's: a file under the roots that its command line names.
    if let Some(config) = intents.iter().find_map(|i| claude_mcp_config(pack, i, guest.mcp_port)) {
        roots.push(Root { id: "agent-mcp".into(), files: [("claude.json".to_owned(), Source::Bytes(config.into_bytes()))].into(), ..Default::default() });
    }
    let plugin = folder(&home.join("plugin"), &[]);
    if !plugin.is_empty() {
        roots.push(Root { id: "plugin".into(), files: plugin, ..Default::default() });
    }
    // The canvas's routing and todo book, laid out as a home: the guest's reads when the hub is away.
    let routing = home.join("bus").join("routing.json");
    if routing.is_file() {
        let mut files: BTreeMap<String, Source> = [("bus/routing.json".to_owned(), Source::File(routing.clone()))].into();
        let canvas = std::fs::read(&routing).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()).and_then(|v| v["canvas"].as_str().map(str::to_owned)).filter(|c| canvas_model::doc::valid_canvas_id(c));
        if let Some(canvas) = canvas {
            let book = crate::todos_cli::book_path(home, &canvas);
            if book.is_file() {
                files.insert(format!("canvas-agents/{canvas}/todos/book.json"), Source::File(book));
            }
        }
        roots.push(Root { id: "canvas".into(), files, ..Default::default() });
    }
    if !pack.profile.include.is_empty() || !pack.paths.is_empty() {
        let bundle = super::profile::bundle(pack, user_home);
        let files = bundle
            .files
            .into_iter()
            .map(|(name, source)| {
                let source = match source {
                    super::profile::Source::File(path) => Source::File(path),
                    super::profile::Source::Text(text) => Source::Bytes(text.into_bytes()),
                };
                (name, source)
            })
            .collect();
        roots.push(Root { id: "profile".into(), files, ..Default::default() });
    }
    let mut groups_done = Vec::new();
    for intent in intents {
        if !intent.group_plugin.is_empty() && super::launch::valid_box(&intent.r#box) {
            roots.push(Root { id: format!("group-skills/{}", intent.r#box), files: folder(Path::new(&intent.group_plugin), &[]), ..Default::default() });
        }
        if intent.group_folder.is_empty() || groups_done.contains(&intent.group_folder) {
            continue;
        }
        groups_done.push(intent.group_folder.clone());
        let slug = group_slug(&intent.group_name);
        let folder_path = PathBuf::from(&intent.group_folder);
        let (docs, outputs) = (canvas_model::doc::AI_DOCS, canvas_model::doc::HUMAN_OUTPUTS);
        let notes = folder(&folder_path, &[&format!("{docs}/"), &format!("{outputs}/")]);
        let writes = Writes { dir: folder_path, prefixes: vec![format!("{docs}/"), format!("{outputs}/")] };
        roots.push(Root { id: format!("notes/{slug}"), files: notes, writes: Some(writes), folders: vec![docs.to_owned(), outputs.to_owned()], ..Default::default() });
        let guest_notes = format!("{}/notes/{slug}", guest_roots.trim_end_matches('/'));
        let text = canvas_model::doc::group_instructions_text(&intent.group_name, &format!("{guest_notes}/{docs}"), &format!("{guest_notes}/{outputs}"));
        roots.push(Root { id: format!("instructions/{slug}"), files: [(GROUP_INSTRUCTIONS.to_owned(), Source::Bytes(text.into_bytes()))].into(), ..Default::default() });
    }
    for (n, map) in pack.paths.iter().enumerate().filter(|(_, m)| m.mode != "none") {
        let dir = PathBuf::from(&map.host);
        let writes = (map.mode == "rw-notes").then(|| Writes { dir: dir.clone(), prefixes: Vec::new() });
        roots.push(Root { id: format!("paths/{n}"), target: map.guest.clone(), files: folder(&dir, &[]), writes, folders: Vec::new() });
    }
    roots
}

/// The hub's file `name` of a writable root (`None`: not one the guest may write).
fn writable_file(writes: &Writes, name: &str, create: bool) -> Option<PathBuf> {
    if !writes.allows(name) {
        return None;
    }
    let parts: Vec<&str> = name.split('/').collect();
    let mut at = writes.dir.clone();
    for part in &parts[..parts.len() - 1] {
        at = at.join(part);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            Err(_) if create => std::fs::create_dir(&at).ok()?,
            _ => return None,
        }
    }
    let file = at.join(parts[parts.len() - 1]);
    if std::fs::symlink_metadata(&file).is_ok_and(|m| !m.is_file()) {
        return None;
    }
    Some(file)
}

/// Writes a guest's edit of a writable root: only when the hub's file is
/// still the one the edit started from (`base_rev`; `None`: it must not
/// exist), else a conflict (nothing written).
pub fn write(writes: &Writes, name: &str, base_rev: Option<&str>, bytes: &[u8]) -> NoteAnswer {
    let refused = |error: &str| NoteAnswer { error: error.into(), ..Default::default() };
    if !writes.dir.is_dir() {
        return refused("the folder is not there on the canvas's machine");
    }
    let Some(file) = writable_file(writes, name, true) else { return refused("not a name this root takes") };
    let current = std::fs::read(&file).ok().map(|b| rev_of(&b));
    if current.as_deref() != base_rev {
        return NoteAnswer { conflict: true, rev: current.unwrap_or_default(), ..Default::default() };
    }
    match crate::paths::write_atomic(&file, bytes) {
        Ok(()) => NoteAnswer { ok: true, rev: rev_of(bytes), ..Default::default() },
        Err(error) => refused(&format!("cannot write it: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boxs_roots_come_from_the_canvas_its_group_and_the_pack() {
        let home = std::env::temp_dir().join(format!("atelier-sync-{}", uuid::Uuid::new_v4()));
        let group = home.join("groups").join("design");
        for (rel, text) in [("plugin/plugin.json", "{}"), ("plugin/skills/canvas-todos/SKILL.md", "todos"), ("groups/design/ai-docs/GOALS.md", "goals"), ("groups/design/human-outputs/report.html", "<p>"), ("groups/design/src/main.rs", "code"), ("specs/api.md", "api")] {
            let path = home.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let pack = Pack {
            paths: vec![
                super::super::pack::PathMap { host: home.join("specs").display().to_string(), guest: "/home/agent/specs".into(), mode: "ro-snapshot".into() },
                super::super::pack::PathMap { host: "/nowhere".into(), guest: "/home/agent/x".into(), mode: "none".into() },
            ],
            ..Default::default()
        };
        let intent = Intent { r#box: "0b1e5ca1-0000-4000-8000-0000000051c0".into(), group_name: "Design & Docs".into(), group_folder: group.display().to_string(), ..Default::default() };
        let roots = roots_for(&home, &pack, &home.join("user"), &Guest { roots: "/home/agent/.atelier/vm/roots".into(), ..Default::default() }, &[intent.clone(), intent]);
        let ids: Vec<&str> = roots.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["plugin", "profile", "notes/design-docs", "instructions/design-docs", "paths/0"], "one group once; `none` paths are not synced");
        // With a routed canvas: its routing and todo book as a home.
        let canvas = "c0ffee00-0000-4000-8000-00000000ca5e";
        std::fs::create_dir_all(home.join("bus")).unwrap();
        std::fs::write(home.join("bus/routing.json"), format!("{{\"canvas\": \"{canvas}\"}}")).unwrap();
        let book = crate::todos_cli::book_path(&home, canvas);
        std::fs::create_dir_all(book.parent().unwrap()).unwrap();
        std::fs::write(&book, "{}").unwrap();
        let routed = roots_for(&home, &Pack::default(), &home.join("user"), &Guest { roots: "/r".into(), ..Default::default() }, &[]);
        let names: Vec<&str> = routed[1].files.keys().map(String::as_str).collect();
        assert_eq!((routed[1].id.as_str(), names), ("canvas", vec!["bus/routing.json", &format!("canvas-agents/{canvas}/todos/book.json")[..]]));
        let notes = &roots[2];
        assert_eq!(notes.files.keys().collect::<Vec<_>>(), ["ai-docs/GOALS.md", "human-outputs/report.html"], "the group's notes only, not its code");
        assert!(notes.writes.is_some() && roots[0].writes.is_none() && roots[4].writes.is_none(), "ro-snapshot is read-only");
        let Source::Bytes(text) = &roots[3].files["canvas-group.instructions.md"] else { panic!("generated") };
        let text = String::from_utf8_lossy(text);
        assert!(text.contains("`/home/agent/.atelier/vm/roots/notes/design-docs/ai-docs`") && !text.contains(&home.display().to_string()), "guest paths only: {text}");
        assert_eq!(roots[4].target, "/home/agent/specs");
        let profile = &roots[1];
        assert!(profile.files.contains_key("instructions/vm-paths.instructions.md") && profile.files.contains_key("plugin.json"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_guest_edit_is_written_only_over_what_it_started_from() {
        let dir = std::env::temp_dir().join(format!("atelier-sync-w-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("ai-docs")).unwrap();
        let writes = Writes { dir: dir.clone(), prefixes: vec!["ai-docs/".into(), "human-outputs/".into()] };
        let made = write(&writes, "ai-docs/plan.md", None, b"v1");
        assert!(made.ok && made.rev == rev_of(b"v1"));
        assert_eq!(std::fs::read(dir.join("ai-docs/plan.md")).unwrap(), b"v1");
        let again = write(&writes, "ai-docs/plan.md", None, b"other");
        assert!(again.conflict && again.rev == rev_of(b"v1"), "it exists now");
        assert!(write(&writes, "ai-docs/plan.md", Some(&rev_of(b"v1")), b"v2").ok);
        assert!(write(&writes, "ai-docs/plan.md", Some(&rev_of(b"v1")), b"v3").conflict, "the hub's is v2 now");
        assert!(write(&writes, "human-outputs/new/deep.md", None, b"deep").ok, "folders are made");
        for bad in ["src/main.rs", "../x.md", "ai-docs/../../x.md", "/etc/x", "nul.md"] {
            let answer = write(&writes, bad, None, b"x");
            assert!(!answer.ok && !answer.conflict, "{bad:?}");
        }
        #[cfg(unix)]
        {
            let outside = dir.join("..").join(format!("outside-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, dir.join("ai-docs").join("out")).unwrap();
            assert!(!write(&writes, "ai-docs/out/x.md", None, b"x").ok, "never through a link");
            assert!(!outside.join("x.md").exists());
            let _ = std::fs::remove_dir_all(outside);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_guests_mcp_configuration_names_the_presets_servers_at_its_proxy() {
        use super::super::pack::{McpPolicy, McpServerPolicy};
        let pack = Pack {
            mcp: McpPolicy { presets: [("tools".to_owned(), vec!["forge".to_owned(), "no-policy".to_owned()])].into(), servers: [("forge".to_owned(), McpServerPolicy::default())].into() },
            ..Default::default()
        };
        let mut intent = Intent { r#box: "0b1e5ca1-0000-4000-8000-0000000051c0".into(), ..Default::default() };
        assert_eq!(mcp_config(&pack, &intent, 48200), None, "no preset: no servers");
        intent.placement.mcp = Some("tools".into());
        let config: serde_json::Value = serde_json::from_str(&mcp_config(&pack, &intent, 48200).unwrap()).unwrap();
        assert_eq!(config["mcpServers"].as_object().unwrap().keys().collect::<Vec<_>>(), ["forge"], "a server without a policy is not offered");
        assert_eq!(config["mcpServers"]["forge"]["url"], "http://127.0.0.1:48200/mcp/0b1e5ca1-0000-4000-8000-0000000051c0/forge");
        assert_eq!(mcp_config(&pack, &intent, 0), None, "no proxy in the guest");
        // Claude Code's: the same servers, `type: http` and a URL only, a file under the roots.
        let claude: serde_json::Value = serde_json::from_str(&claude_mcp_config(&pack, &intent, 48200).unwrap()).unwrap();
        assert_eq!(claude["mcpServers"]["forge"], serde_json::json!({ "type": "http", "url": "http://127.0.0.1:48200/mcp/0b1e5ca1-0000-4000-8000-0000000051c0/forge" }));
        let roots = roots_for(Path::new("/nowhere"), &pack, Path::new("/nowhere"), &Guest { roots: "/home/agent/.atelier/vm/roots".into(), home: "/home/agent".into(), mcp_port: 48200 }, &[intent]);
        assert_eq!((roots[0].id.as_str(), roots[0].target.as_str()), ("mcp", "/home/agent/.copilot"));
        assert_eq!((roots[1].id.as_str(), roots[1].target.as_str()), ("agent-mcp", ""), "under the roots");
        assert!(roots[1].files.contains_key("claude.json") && CLAUDE_MCP == "agent-mcp/claude.json");
    }
}
