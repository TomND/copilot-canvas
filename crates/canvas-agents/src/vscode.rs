//! Read-only import of VS Code agent sessions and their workspace groupings.
//!
//! VS Code (and Insiders) keep a per-workspace `agentSessions.model.cache` list in
//! `workspaceStorage/<id>/state.vscdb`. Copilot CLI entries reference the same
//! session UUIDs stored under `~/.copilot/session-state`.
//!
//! The Agents window keeps its own session per `agentSessionData/<id>/session.db`.
//! That id is VS Code's; the Copilot session behind its main chat is
//! `defaultChatProviderData.sdkSessionId`, and each extra chat in `peerChats`
//! (a sub-session) is another Copilot session.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use percent_encoding::percent_decode_str;
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

pub use canvas_model::vscode::{SyncCandidate, VsSession};

#[derive(Clone, Debug, PartialEq)]
pub struct VsWorkspace {
    pub product: String,
    pub storage_id: String,
    pub label: String,
    pub folder: Option<String>,
    pub sessions: Vec<VsSession>,
    /// Entries from other providers (Claude, local chat) that cannot open as Copilot terminals.
    pub other_providers: usize,
}

#[derive(Deserialize)]
struct CacheEntry {
    #[serde(rename = "providerType", default)]
    provider_type: String,
    #[serde(default)]
    resource: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    timing: Option<Timing>,
    #[serde(default)]
    metadata: Option<Metadata>,
}

#[derive(Deserialize)]
struct Timing {
    #[serde(default)]
    created: Option<f64>,
    #[serde(rename = "lastRequestStarted", default)]
    last_request_started: Option<f64>,
    #[serde(rename = "lastRequestEnded", default)]
    last_request_ended: Option<f64>,
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = "workingDirectoryPath", default)]
    working_directory_path: Option<String>,
}

#[derive(Deserialize)]
struct WorkspaceJson {
    folder: Option<String>,
    workspace: Option<String>,
}

pub fn default_roots() -> Vec<(String, PathBuf)> {
    let Some(appdata) = dirs::config_dir() else { return Vec::new() };
    ["Code", "Code - Insiders"]
        .into_iter()
        .map(|product| (product.to_owned(), appdata.join(product).join("User").join("workspaceStorage")))
        .filter(|(_, path)| path.is_dir())
        .collect()
}

/// First hyphenated UUID in a resource such as `agent-host-copilotcli:/<uuid>`.
pub fn session_uuid(resource: &str) -> Option<String> {
    let decoded = percent_decode_str(resource).decode_utf8_lossy();
    let bytes = decoded.as_bytes();
    (0..bytes.len().saturating_sub(35)).find_map(|start| {
        let candidate = decoded.get(start..start + 36)?;
        crate::index::is_session_id(candidate).then(|| candidate.to_lowercase())
    })
}

fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode_str(rest).decode_utf8_lossy().into_owned();
    // file:///c:/x -> c:/x ; file://server/share -> \\server\share
    let path = match decoded.strip_prefix('/') {
        Some(local) if local.as_bytes().get(1) == Some(&b':') => local.to_owned(),
        Some(_) | None => format!("//{decoded}"),
    };
    Some(path.replace('/', "\\"))
}

fn label_for(folder: Option<&str>, workspace_file: Option<&str>) -> String {
    if let Some(folder) = folder {
        let parts: Vec<&str> = folder.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
        return match parts.as_slice() {
            [] => folder.to_owned(),
            [only] => (*only).to_owned(),
            [.., parent, last] => format!("{parent}\\{last}"),
        };
    }
    if let Some(file) = workspace_file {
        return Path::new(file).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| file.to_owned());
    }
    EMPTY_WINDOW.to_owned()
}

const EMPTY_WINDOW: &str = "Empty window";

fn is_copilot_cli(provider: &str, resource: &str) -> bool {
    provider.contains("copilotcli") || resource.split(':').next().is_some_and(|scheme| scheme.contains("copilotcli"))
}

pub fn parse_cache(json: &str) -> Result<(Vec<VsSession>, usize)> {
    let entries: Vec<CacheEntry> = serde_json::from_str(json).context("unexpected agent session cache format")?;
    let mut sessions = Vec::new();
    let mut other = 0;
    for entry in entries {
        if !is_copilot_cli(&entry.provider_type, &entry.resource) {
            other += 1;
            continue;
        }
        let Some(session_id) = session_uuid(&entry.resource) else { continue };
        if sessions.iter().any(|s: &VsSession| s.session_id == session_id) {
            continue;
        }
        sessions.push(VsSession {
            session_id,
            label: entry.label.trim().to_owned(),
            archived: entry.archived,
            cwd: entry.metadata.and_then(|m| m.working_directory_path).filter(|p| !p.is_empty()),
            created_ms: entry.timing.as_ref().and_then(|t| t.created).filter(|c| *c > 0.0).map(|c| c as u64),
            active_ms: entry
                .timing
                .as_ref()
                .map(|t| [t.created, t.last_request_started, t.last_request_ended].into_iter().flatten().fold(0.0f64, f64::max))
                .filter(|v| *v > 0.0)
                .map(|v| v as u64),
        });
    }
    Ok((sessions, other))
}

fn read_cache(db: &Path) -> Result<Option<String>> {
    let uri = format!("file:{}?mode=ro", db.to_string_lossy().replace('\\', "/"));
    let connection = Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;
    let mut statement = connection.prepare("SELECT value FROM ItemTable WHERE key = 'agentSessions.model.cache'")?;
    let mut rows = statement.query([])?;
    let Some(row) = rows.next()? else { return Ok(None) };
    let value: rusqlite::types::Value = row.get(0)?;
    Ok(match value {
        rusqlite::types::Value::Text(text) => Some(text),
        rusqlite::types::Value::Blob(blob) => Some(String::from_utf8_lossy(&blob).into_owned()),
        _ => None,
    })
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    let norm = |s: &str| s.to_lowercase().replace('/', "\\");
    let (a, b) = (norm(a), norm(b));
    let parts_a: Vec<&str> = a.split('\\').filter(|p| !p.is_empty()).collect();
    let parts_b: Vec<&str> = b.split('\\').filter(|p| !p.is_empty()).collect();
    parts_a.iter().zip(&parts_b).take_while(|(x, y)| x == y).count()
}

/// VS Code lists the same agent session in several workspace caches. Assign each
/// session to exactly one workspace: the one whose folder shares the longest path
/// prefix with the session's working directory, else one with a folder, else the first.
pub fn assign_unique(workspaces: &mut Vec<VsWorkspace>) {
    use std::collections::HashMap;
    let mut owner: HashMap<String, usize> = HashMap::new();
    for (index, workspace) in workspaces.iter().enumerate() {
        for session in &workspace.sessions {
            let score = |w: &VsWorkspace| -> (usize, bool) {
                let prefix = match (&w.folder, &session.cwd) {
                    (Some(folder), Some(cwd)) => common_prefix_len(folder, cwd),
                    _ => 0,
                };
                (prefix, w.folder.is_some())
            };
            match owner.get(&session.session_id) {
                Some(&current) if score(&workspaces[current]) >= score(workspace) => {}
                _ => {
                    owner.insert(session.session_id.clone(), index);
                }
            }
        }
    }
    for (index, workspace) in workspaces.iter_mut().enumerate() {
        workspace.sessions.retain(|s| owner.get(&s.session_id) == Some(&index));
    }
    workspaces.retain(|w| !w.sessions.is_empty() || w.other_providers > 0);
}

/// A user-created group in the VS Code Agents window.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentGroup {
    pub product: String,
    pub id: String,
    pub name: String,
    pub sessions: Vec<String>,
}

#[derive(Deserialize)]
struct GroupsState {
    #[serde(default)]
    groups: Vec<GroupDef>,
    #[serde(default)]
    membership: std::collections::HashMap<String, String>,
}

#[derive(Deserialize)]
struct GroupDef {
    id: String,
    #[serde(default)]
    name: String,
}

pub fn parse_agent_groups(product: &str, json: &str) -> Result<Vec<AgentGroup>> {
    let state: GroupsState = serde_json::from_str(json).context("unexpected Agents-window group format")?;
    let mut groups: Vec<AgentGroup> = state
        .groups
        .into_iter()
        .map(|g| AgentGroup { product: product.to_owned(), name: if g.name.trim().is_empty() { "Untitled group".into() } else { g.name.trim().to_owned() }, id: g.id, sessions: Vec::new() })
        .collect();
    for (resource, group_id) in state.membership {
        if let (Some(group), Some(session)) = (groups.iter_mut().find(|g| g.id == group_id), session_uuid(&resource)) {
            // Only local Copilot CLI sessions; remote tunnel sessions live on another machine.
            if resource.contains("remote-tunnel") || !resource.contains("copilotcli") {
                continue;
            }
            if !group.sessions.contains(&session) {
                group.sessions.push(session);
            }
        }
    }
    Ok(groups)
}

pub fn default_user_dirs() -> Vec<(String, PathBuf)> {
    let Some(appdata) = dirs::config_dir() else { return Vec::new() };
    ["Code", "Code - Insiders"]
        .into_iter()
        .map(|product| (product.to_owned(), appdata.join(product).join("User")))
        .filter(|(_, path)| path.is_dir())
        .collect()
}

/// Reads Agents-window groups from each product's built-in "agents" profile. Never writes.
pub fn agent_groups(user_dirs: &[(String, PathBuf)]) -> (Vec<AgentGroup>, Vec<String>) {
    let mut groups = Vec::new();
    let mut warnings = Vec::new();
    for (product, user) in user_dirs {
        let db = user.join("profiles").join("builtin").join("agents").join("globalStorage").join("state.vscdb");
        if !db.is_file() {
            continue;
        }
        let read = (|| -> Result<Option<String>> {
            let uri = format!("file:{}?mode=ro", db.to_string_lossy().replace('\\', "/"));
            let connection = Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
            connection.busy_timeout(std::time::Duration::from_secs(2))?;
            let mut statement = connection.prepare("SELECT value FROM ItemTable WHERE key = 'sessionsListControl.groups'")?;
            let mut rows = statement.query([])?;
            Ok(match rows.next()? {
                Some(row) => match row.get::<_, rusqlite::types::Value>(0)? {
                    rusqlite::types::Value::Text(t) => Some(t),
                    rusqlite::types::Value::Blob(b) => Some(String::from_utf8_lossy(&b).into_owned()),
                    _ => None,
                },
                None => None,
            })
        })();
        match read.and_then(|json| json.map(|j| parse_agent_groups(product, &j)).transpose()) {
            Ok(Some(found)) => groups.extend(found),
            Ok(None) => {}
            Err(error) => warnings.push(format!("{product} Agents window: {error:#}")),
        }
    }
    (groups, warnings)
}

/// A session of the VS Code Agents window (`agentSessionData/<id>/session.db`).
#[derive(Clone, Debug, PartialEq)]
pub struct VsContainer {
    pub product: String,
    /// VS Code's id (used by Agents-window groups).
    pub id: String,
    pub title: String,
    /// Copilot session of the main chat.
    pub main: String,
    /// Sub-sessions: (Copilot session id, chat title).
    pub peers: Vec<(String, String)>,
    pub archived: bool,
    pub cwd: Option<String>,
    pub from_vscode: bool,
    pub modified_ms: u64,
}

fn sdk_session(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let id = value.get("sdkSessionId")?.as_str()?;
    crate::index::is_session_id(id).then(|| id.to_lowercase())
}

/// Builds a container from its `session_metadata` rows.
pub fn parse_container(product: &str, id: &str, metadata: &std::collections::HashMap<String, String>, modified_ms: u64) -> VsContainer {
    let get = |key: &str| metadata.get(key).map(String::as_str).unwrap_or_default();
    let provider = metadata.get("defaultChatProviderData").or_else(|| metadata.get("agentHost.chatProviderData"));
    let main = provider.and_then(|p| sdk_session(p)).unwrap_or_else(|| id.to_lowercase());
    let mut peers = Vec::new();
    if let Ok(serde_json::Value::Array(chats)) = serde_json::from_str::<serde_json::Value>(get("peerChats")) {
        for chat in chats {
            let uri = chat.get("uri").and_then(|u| u.as_str()).unwrap_or_default();
            let Some(sdk) = chat.get("providerData").and_then(|d| d.as_str()).and_then(sdk_session) else { continue };
            if sdk != main && !peers.iter().any(|(p, _): &(String, String)| *p == sdk) {
                peers.push((sdk, get(&format!("customChatTitle:{uri}")).trim().to_owned()));
            }
        }
    }
    let initiator: Option<String> = serde_json::from_str::<serde_json::Value>(get("vscode.session.initiator"))
        .ok()
        .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_owned));
    let from_vscode = match initiator {
        Some(name) => name.starts_with("vscode"),
        None => provider.is_some(),
    };
    VsContainer {
        product: product.to_owned(),
        id: id.to_lowercase(),
        title: get("customTitle").trim().to_owned(),
        main,
        peers,
        archived: get("isArchived") == "true",
        cwd: uri_to_path(get("copilot.workingDirectory")),
        from_vscode,
        modified_ms,
    }
}

pub fn default_container_roots() -> Vec<(String, PathBuf)> {
    let Some(appdata) = dirs::config_dir() else { return Vec::new() };
    ["Code", "Code - Insiders"]
        .into_iter()
        .map(|product| (product.to_owned(), appdata.join(product).join("agentSessionData")))
        .filter(|(_, path)| path.is_dir())
        .collect()
}

/// Reads Agents-window sessions changed since `since_ms`, plus the ids in
/// `always` (members of Agents-window groups). Read-only.
pub fn read_containers(roots: &[(String, PathBuf)], since_ms: u64, always: &std::collections::HashSet<String>) -> (Vec<VsContainer>, Vec<String>) {
    let mut out = Vec::new();
    let mut failed = 0;
    for (product, root) in roots {
        let Ok(entries) = std::fs::read_dir(root) else { continue };
        for entry in entries.filter_map(|e| e.ok()) {
            let id = entry.file_name().to_string_lossy().to_lowercase();
            let db = entry.path().join("session.db");
            let Ok(modified) = std::fs::metadata(&db).and_then(|m| m.modified()) else { continue };
            let modified_ms = modified.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
            if modified_ms < since_ms && !always.contains(&id) {
                continue;
            }
            let read = (|| -> Result<std::collections::HashMap<String, String>> {
                let uri = format!("file:{}?mode=ro", db.to_string_lossy().replace('\\', "/"));
                let connection = Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
                connection.busy_timeout(std::time::Duration::from_millis(500))?;
                let mut statement = connection.prepare("SELECT key, value FROM session_metadata")?;
                let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
                Ok(rows.filter_map(|r| r.ok()).collect())
            })();
            match read {
                Ok(metadata) => out.push(parse_container(product, &id, &metadata, modified_ms)),
                Err(_) => failed += 1,
            }
        }
    }
    let warnings = if failed > 0 { vec![format!("{failed} VS Code session database(s) could not be read")] } else { Vec::new() };
    // Code and Insiders can both hold a session; the most recently used copy wins.
    out.sort_by_key(|c| std::cmp::Reverse(c.modified_ms));
    (out, warnings)
}

/// Agents-window group first; otherwise the session's VS Code workspace, or its
/// folder when VS Code has no workspace for it.
///
/// `containers`: Agents-window sessions with their sub-sessions (most accurate).
/// `created_in_vscode`: sessions Copilot recorded as created by VS Code, for any
/// the other sources miss.
pub fn sync_candidates(
    workspaces: &[VsWorkspace],
    groups: &[AgentGroup],
    containers: &[VsContainer],
    created_in_vscode: &[VsSession],
) -> Vec<SyncCandidate> {
    // Deterministic: stable VS Code's groups are listed before Insiders', so a
    // session filed in both always maps to the same group.
    let group_for = |ids: &[&str]| groups.iter().find(|g| ids.iter().any(|id| g.sessions.iter().any(|s| s == id)));
    let agent_group = |g: &AgentGroup| (format!("vscode-group:{}:{}", g.product, g.id), g.name.clone());
    let mut out: Vec<SyncCandidate> = Vec::new();
    let has = |out: &Vec<SyncCandidate>, id: &str| out.iter().any(|c| c.session.session_id == id);
    let peer_ids: std::collections::HashSet<&str> = containers.iter().flat_map(|c| c.peers.iter().map(|(p, _)| p.as_str())).collect();
    for container in containers {
        // A chat that is some other session's sub-session is listed under that parent.
        if peer_ids.contains(container.main.as_str()) || has(&out, &container.main) {
            continue;
        }
        let (group_key, group_name) = match group_for(&[&container.id, &container.main]) {
            Some(group) => agent_group(group),
            None => cwd_group(container.cwd.as_deref()),
        };
        let session = |id: &str, label: &str| VsSession {
            session_id: id.to_owned(),
            label: label.to_owned(),
            archived: container.archived,
            cwd: container.cwd.clone(),
            created_ms: None,
            active_ms: Some(container.modified_ms),
        };
        out.push(SyncCandidate {
            session: session(&container.main, &container.title),
            group_key: group_key.clone(),
            group_name: group_name.clone(),
            parent: None,
            from_vscode: container.from_vscode,
        });
        for (peer, title) in &container.peers {
            if has(&out, peer) {
                continue;
            }
            out.push(SyncCandidate {
                session: session(peer, title),
                group_key: group_key.clone(),
                group_name: group_name.clone(),
                parent: Some(container.main.clone()),
                from_vscode: container.from_vscode,
            });
        }
    }
    // VS Code caches a session in several windows; pick its best workspace first.
    let mut owned = workspaces.to_vec();
    assign_unique(&mut owned);
    for workspace in &owned {
        for session in &workspace.sessions {
            if has(&out, &session.session_id) || peer_ids.contains(session.session_id.as_str()) {
                continue;
            }
            let suffix = if workspace.product.contains("Insiders") { " (Insiders)" } else { "" };
            let empty_window = workspace.folder.is_none() && workspace.label == EMPTY_WINDOW;
            let (group_key, group_name) = match group_for(&[&session.session_id]) {
                Some(group) => agent_group(group),
                None if empty_window => cwd_group(session.cwd.as_deref()),
                None => (format!("vscode-ws:{}:{}", workspace.product, workspace.storage_id), format!("VS Code · {}{suffix}", workspace.label)),
            };
            out.push(SyncCandidate { session: session.clone(), group_key, group_name, parent: None, from_vscode: false });
        }
    }
    for session in created_in_vscode {
        if has(&out, &session.session_id) || peer_ids.contains(session.session_id.as_str()) {
            continue;
        }
        let (group_key, group_name) = match group_for(&[&session.session_id]) {
            Some(group) => agent_group(group),
            None => cwd_group(session.cwd.as_deref()),
        };
        out.push(SyncCandidate { session: session.clone(), group_key, group_name, parent: None, from_vscode: true });
    }
    out
}

/// Fallback grouping by the session's working folder.
fn cwd_group(cwd: Option<&str>) -> (String, String) {
    let cwd = cwd.unwrap_or_default();
    let label = if cwd.is_empty() { "no folder".to_owned() } else { label_for(Some(cwd), None) };
    (format!("vscode-cwd:{}", cwd.to_lowercase()), format!("VS Code · {label}"))
}

/// Scans VS Code workspaces with Copilot CLI agent sessions. Never writes.
pub fn scan(roots: &[(String, PathBuf)]) -> (Vec<VsWorkspace>, Vec<String>) {
    let mut workspaces = Vec::new();
    let mut warnings = Vec::new();
    for (product, root) in roots {
        let Ok(entries) = std::fs::read_dir(root) else { continue };
        for entry in entries.filter_map(|e| e.ok()) {
            let dir = entry.path();
            let db = dir.join("state.vscdb");
            if !db.is_file() {
                continue;
            }
            let json = match read_cache(&db) {
                Ok(Some(json)) => json,
                Ok(None) => continue,
                Err(error) => {
                    warnings.push(format!("{product} {}: {error}", entry.file_name().to_string_lossy()));
                    continue;
                }
            };
            let (sessions, other_providers) = match parse_cache(&json) {
                Ok(parsed) => parsed,
                Err(error) => {
                    warnings.push(format!("{product} {}: {error}", entry.file_name().to_string_lossy()));
                    continue;
                }
            };
            if sessions.is_empty() && other_providers == 0 {
                continue;
            }
            let meta: Option<WorkspaceJson> =
                std::fs::read_to_string(dir.join("workspace.json")).ok().and_then(|t| serde_json::from_str(&t).ok());
            let folder = meta.as_ref().and_then(|m| m.folder.as_deref()).and_then(uri_to_path);
            let workspace_file = meta.as_ref().and_then(|m| m.workspace.as_deref()).and_then(uri_to_path);
            let label = label_for(folder.as_deref(), workspace_file.as_deref());
            workspaces.push(VsWorkspace {
                product: product.clone(),
                storage_id: entry.file_name().to_string_lossy().into_owned(),
                label,
                folder,
                sessions,
                other_providers,
            });
        }
    }
    assign_unique(&mut workspaces);
    workspaces.sort_by(|a, b| b.sessions.len().cmp(&a.sessions.len()).then(a.label.cmp(&b.label)));
    (workspaces, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "11111111-2222-3333-4444-555555555555";

    #[test]
    fn extracts_session_ids_from_resources() {
        assert_eq!(session_uuid(&format!("agent-host-copilotcli:/{A}")).as_deref(), Some(A));
        assert_eq!(session_uuid(&format!("agent-host-copilotcli:/{A}#99999999-2222-3333-4444-555555555555")).as_deref(), Some(A));
        assert_eq!(session_uuid("vscode-chat-session://local/MzFjZjQ2"), None);
    }

    #[test]
    fn parses_cache_entries_and_counts_other_providers() {
        let json = format!(
            r#"[{{"providerType":"agent-host-copilotcli","resource":"agent-host-copilotcli:/{A}","label":" Fix build ","archived":true,
                 "timing":{{"created":1792526400000}},"metadata":{{"workingDirectoryPath":"C:\\wt\\x"}}}},
                {{"providerType":"agent-host-copilotcli","resource":"agent-host-copilotcli:/{A}","label":"dup"}},
                {{"providerType":"claude-code","resource":"claude-code:/{A}","label":"claude"}},
                {{"providerType":"local","resource":"vscode-chat-session://local/abc","label":"chat"}}]"#
        );
        let (sessions, other) = parse_cache(&json).unwrap();
        assert_eq!(other, 2);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].label, "Fix build");
        assert!(sessions[0].archived);
        assert_eq!(sessions[0].cwd.as_deref(), Some(r"C:\wt\x"));
        assert_eq!(sessions[0].created_ms, Some(1_792_526_400_000));
    }

    #[test]
    fn workspace_labels_from_uris() {
        assert_eq!(uri_to_path("file:///q%3A/contoso/src").as_deref(), Some(r"q:\contoso\src"));
        assert_eq!(label_for(Some(r"q:\contoso\src"), None), r"contoso\src");
        assert_eq!(label_for(None, Some(r"C:\w\team.code-workspace")), "team");
        assert_eq!(label_for(None, None), "Empty window");
    }

    #[test]
    fn agents_window_groups_take_precedence_over_workspaces() {
        let json = format!(
            r#"{{"groups": [{{"id": "g1", "name": "Release work", "createdAt": 1}}, {{"id": "g2", "name": " ", "createdAt": 2}}],
                "membership": {{"local-agent-host:agent-host-copilotcli:/{A}": "g1",
                               "agenthost-tunnel__x:remote-tunnel__x-copilotcli:/99999999-2222-3333-4444-555555555555": "g1",
                               "local-agent-host:agent-host-copilotcli:/bad": "g2"}},
                "explicitlyUngroupedSessionIds": []}}"#
        );
        let groups = parse_agent_groups("Code", &json).unwrap();
        assert_eq!(groups[0].sessions, vec![A.to_owned()], "remote tunnel sessions are skipped");
        assert_eq!(groups[1].name, "Untitled group");
        let session = |id: &str| VsSession { session_id: id.into(), label: String::new(), archived: false, cwd: None, created_ms: None, active_ms: None };
        let workspaces = vec![VsWorkspace {
            product: "Code".into(),
            storage_id: "ws1".into(),
            label: r"src\app".into(),
            folder: None,
            sessions: vec![session(A), session("11111111-2222-3333-4444-666666666666")],
            other_providers: 0,
        }];
        let candidates = sync_candidates(&workspaces, &groups, &[], &[]);
        assert_eq!((candidates[0].group_key.as_str(), candidates[0].group_name.as_str()), ("vscode-group:Code:g1", "Release work"));
        assert_eq!((candidates[1].group_key.as_str(), candidates[1].group_name.as_str()), ("vscode-ws:Code:ws1", r"VS Code · src\app"));
    }

    #[test]
    fn sync_prefers_a_real_workspace_groups_the_rest_by_folder_and_adds_vscode_created_sessions() {
        let session = |id: &str, cwd: &str| VsSession { session_id: id.into(), label: id.into(), archived: false, cwd: Some(cwd.into()), created_ms: None, active_ms: None };
        let workspace = |id: &str, folder: Option<&str>, sessions: Vec<VsSession>| VsWorkspace {
            product: "Code".into(),
            storage_id: id.into(),
            label: label_for(folder, None),
            folder: folder.map(str::to_owned),
            sessions,
            other_providers: 0,
        };
        // The empty window is read first, but "a" belongs to the app workspace.
        let workspaces = vec![
            workspace("empty", None, vec![session("a", r"c:\src\app"), session("b", r"q:\pipeline\worker")]),
            workspace("app", Some(r"c:\src\app"), vec![session("a", r"c:\src\app")]),
        ];
        let created = vec![session("c", r"q:\contoso\src"), session("a", r"c:\src\app")];
        let candidates = sync_candidates(&workspaces, &[], &[], &created);
        assert_eq!(candidates.len(), 3, "no duplicates");
        let c = candidates.iter().find(|c| c.session.session_id == "c").unwrap();
        assert_eq!((c.group_key.as_str(), c.group_name.as_str()), (r"vscode-cwd:q:\contoso\src", r"VS Code · contoso\src"));
        let a = candidates.iter().find(|c| c.session.session_id == "a").unwrap();
        assert_eq!(a.group_key, "vscode-ws:Code:app");
        let b = candidates.iter().find(|c| c.session.session_id == "b").unwrap();
        assert_eq!((b.group_key.as_str(), b.group_name.as_str()), (r"vscode-cwd:q:\pipeline\worker", r"VS Code · pipeline\worker"));
    }

    #[test]
    fn sessions_listed_in_several_workspaces_get_one_owner() {
        let session = |id: &str, cwd: Option<&str>| VsSession { session_id: id.into(), label: id.into(), archived: false, cwd: cwd.map(str::to_owned), created_ms: None, active_ms: None };
        let workspace = |label: &str, folder: Option<&str>, sessions: Vec<VsSession>| VsWorkspace {
            product: "Code".into(),
            storage_id: label.into(),
            label: label.into(),
            folder: folder.map(str::to_owned),
            sessions,
            other_providers: 0,
        };
        let mut workspaces = vec![
            workspace("empty", None, vec![session("a", Some(r"C:\src\app\sub")), session("b", None)]),
            workspace("app", Some(r"c:\src\app"), vec![session("a", Some(r"C:\src\app\sub")), session("b", None)]),
            workspace("other", Some(r"c:\src\other"), vec![session("a", Some(r"C:\src\app\sub"))]),
        ];
        assign_unique(&mut workspaces);
        assert_eq!(workspaces.len(), 1, "empty owners are dropped");
        assert_eq!(workspaces[0].label, "app");
        assert_eq!(workspaces[0].sessions.len(), 2, "a by path, b by having a folder");
    }

    #[test]
    fn scans_a_synthetic_workspace_storage_read_only() {
        let root = std::env::temp_dir().join(format!("cc-vs-{}", uuid::Uuid::new_v4()));
        let ws = root.join("abc123");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("workspace.json"), r#"{"folder":"file:///c%3A/src/app"}"#).unwrap();
        {
            let db = Connection::open(ws.join("state.vscdb")).unwrap();
            db.execute_batch("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);").unwrap();
            let value = format!(r#"[{{"providerType":"agent-host-copilotcli","resource":"agent-host-copilotcli:/{A}","label":"One"}}]"#);
            db.execute("INSERT INTO ItemTable VALUES ('agentSessions.model.cache', ?1)", [value]).unwrap();
        }
        let before = std::fs::read(ws.join("state.vscdb")).unwrap();
        let (workspaces, warnings) = scan(&[("Code".into(), root.clone())]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].label, r"src\app");
        assert_eq!(workspaces[0].sessions[0].session_id, A);
        assert_eq!(std::fs::read(ws.join("state.vscdb")).unwrap(), before, "database untouched");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agents_window_sessions_map_to_copilot_sessions_with_sub_sessions_and_groups() {
        const MAIN: &str = "f8276756-c2e0-46ed-af12-dcae55e00b1b";
        const PEER: &str = "941cc8bd-23f0-4d45-aa61-3db2d9cfe73c";
        const VS_ID: &str = "0f2e0a39-766d-45ee-b1ba-175df741d067";
        let peer_uri = "ahp-chat://1673759b-a933-45a2-8da2-a20fb064c43f/Y29w";
        let metadata: std::collections::HashMap<String, String> = [
            ("defaultChatProviderData", format!(r#"{{"sdkSessionId":"{MAIN}"}}"#)),
            ("peerChats", format!(r#"[{{"uri":"{peer_uri}","isRead":true,"providerData":"{{\"sdkSessionId\":\"{PEER}\"}}"}}]"#)),
            (&format!("customChatTitle:{peer_uri}"), "Parser set 3 \u{2014} producer".to_owned()),
            ("customTitle", "Renderer changes".into()),
            ("vscode.session.initiator", r#"{"name":"vscode-agents-window","title":"VS Code Agents Window"}"#.into()),
            ("copilot.workingDirectory", "file:///q%3A/src/MediaService".into()),
            ("isArchived", String::new()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let container = parse_container("Code", VS_ID, &metadata, 5);
        assert_eq!((container.main.as_str(), container.title.as_str()), (MAIN, "Renderer changes"));
        assert_eq!(container.peers, vec![(PEER.to_owned(), "Parser set 3 \u{2014} producer".to_owned())]);
        assert!(container.from_vscode && !container.archived);
        assert_eq!(container.cwd.as_deref(), Some(r"q:\src\MediaService"));

        // A CLI-started session VS Code merely tracks: same id, no provider data.
        let cli: std::collections::HashMap<String, String> =
            [("vscode.session.initiator".to_owned(), r#"{"name":"github/cli"}"#.to_owned()), ("isArchived".to_owned(), "true".to_owned())].into();
        let tracked = parse_container("Code", A, &cli, 1);
        assert_eq!(tracked.main, A);
        assert!(!tracked.from_vscode && tracked.archived);

        // Groups reference VS Code's id; the peer also appears as its own container.
        let groups = vec![AgentGroup { product: "Code".into(), id: "g1".into(), name: "Quality Boost PRs".into(), sessions: vec![VS_ID.to_owned()] }];
        let peer_container = parse_container("Code", PEER, &std::collections::HashMap::new(), 5);
        let candidates = sync_candidates(&[], &groups, &[peer_container, container], &[]);
        assert_eq!(candidates.len(), 2, "{candidates:?}");
        assert_eq!((candidates[0].session.session_id.as_str(), candidates[0].group_name.as_str(), candidates[0].parent.as_deref()), (MAIN, "Quality Boost PRs", None));
        assert_eq!((candidates[1].session.session_id.as_str(), candidates[1].group_key.as_str(), candidates[1].parent.as_deref()), (PEER, "vscode-group:Code:g1", Some(MAIN)));
        assert_eq!(candidates[1].session.label, "Parser set 3 \u{2014} producer");
    }
}
