//! The VM pack (docs/VM-SESSIONS.md §5): a folder of configuration outside
//! any repository that says which provider, repositories, sizes, budgets,
//! egress, secrets, MCP policy, profile and path map VM sessions use. The
//! service reads it at run time; nothing of it is ever copied into the
//! repository, the canvas document, logs or reports.
//!
//! [`load`] reads every file and keeps going after a problem: each one
//! becomes a [`Finding`] with its file and line, never with a value of the
//! pack. `Debug` of a [`Pack`] names only its repositories and sizes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use canvas_model::vm::{Finding, Level, PackStatus, PackSummary, RepoSummary};
use yaml_rust2::{Yaml, YamlLoader};

/// The pack format this build reads (`pack_version`).
pub const PACK_VERSION: i64 = 1;
/// The hooks a pack may provide (`hooks/<name>[.sh|.ps1|.cmd|.exe]`).
pub const HOOKS: [&str; 7] = ["claim", "shell-command", "exec", "suspend", "resume", "delete", "image"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ProviderKind {
    /// Azure Container Apps Sandboxes (`vm::aca`, REST).
    #[default]
    Aca,
    /// The pack's `hooks/` (the thin lane).
    Hooks,
    /// Sandboxes as local processes in folders (`vm::local`): tests and CI.
    Local,
}

#[derive(Clone, Default, PartialEq)]
pub struct Pack {
    pub folder: PathBuf,
    pub name: String,
    pub provider: ProviderKind,
    /// S6: when the user's data-handling policy for VM sessions was acknowledged.
    pub policy_ack: String,
    pub aca: Aca,
    pub auth: Auth,
    pub sizes: Vec<Size>,
    pub pool: Pool,
    pub images: Images,
    pub budget: Budget,
    pub suspend_after_ms: u64,
    /// How often the thin lane polls a guest (seconds).
    pub poll_seconds: u64,
    pub tunnel: Tunnel,
    pub git: GitIdentity,
    pub satellite: SatelliteBinary,
    pub ring_mb: u32,
    /// `vm::local`'s folder for its sandboxes (`None`: under the service's home).
    pub local_root: Option<PathBuf>,
    pub repos: Vec<Repo>,
    /// Egress profiles by name (`egress/<name>.yaml`): their text, rendered at claim.
    pub egress: BTreeMap<String, String>,
    pub secrets: Vec<SecretSpec>,
    pub mcp: McpPolicy,
    pub profile: Profile,
    pub paths: Vec<PathMap>,
    /// Hook name → its file.
    pub hooks: BTreeMap<String, PathBuf>,
}

impl std::fmt::Debug for Pack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never a value of the pack: names only.
        f.debug_struct("Pack")
            .field("provider", &self.provider)
            .field("repos", &self.repos.iter().map(|r| r.key.as_str()).collect::<Vec<_>>())
            .field("sizes", &self.sizes.iter().map(|s| s.name.as_str()).collect::<Vec<_>>())
            .field("hooks", &self.hooks.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Default, PartialEq)]
pub struct Aca {
    pub subscription: String,
    pub resource_group: String,
    pub sandbox_group: String,
    pub region: String,
    pub api_version: String,
    /// The data plane's base URL (`None`: `https://management.<region>.azuredevcompute.io`).
    pub endpoint: Option<String>,
}

#[derive(Clone, Default, PartialEq)]
pub struct Auth {
    /// The token audience (the SDK's by default).
    pub audience: String,
    /// A command that prints a bearer token (`None`: the Azure CLI).
    pub command: Option<String>,
}

pub const DEFAULT_AUDIENCE: &str = "https://dynamicsessions.io";
pub const DEFAULT_API_VERSION: &str = "2026-02-01-preview";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Size {
    pub name: String,
    pub cpu_milli: u32,
    pub memory_mi: u32,
    pub disk_gi: u32,
    /// Estimated cost per running hour, in cents.
    pub cents_per_hour: u32,
}

impl Size {
    pub fn cores(&self) -> f32 {
        self.cpu_milli as f32 / 1000.0
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pool {
    pub max_age_ms: u64,
    /// Repository key → members kept ready.
    pub min: BTreeMap<String, u32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Images {
    /// `daily HH:MM`, `every Nh`, or `off`.
    pub schedule: String,
    pub keep: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Budget {
    pub max_running: u32,
    pub max_cores: f32,
    pub daily_cents: u64,
}

#[derive(Clone, Default, PartialEq)]
pub struct Tunnel {
    /// The hub's remote port through the tunnel's web forwarding (`wss://…`).
    pub url: String,
    /// The group secret holding the tunnel's connect token (injected by egress).
    pub secret: String,
}

#[derive(Clone, Default, PartialEq)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Default, PartialEq)]
pub struct SatelliteBinary {
    pub path: Option<PathBuf>,
    pub sha256: Option<String>,
}

#[derive(Clone, Default, PartialEq)]
pub struct Repo {
    pub key: String,
    pub url: String,
    pub default_branch: String,
    /// `full`, `blobless` or `single_branch`.
    pub clone_mode: String,
    pub sparse: Vec<String>,
    pub image_base: String,
    pub image_disk_gi: u32,
    /// The egress profile (`egress/<name>.yaml`).
    pub egress: String,
    /// Globs of this machine's folders that hold the repository.
    pub folders: Vec<String>,
}

#[derive(Clone, Default, PartialEq)]
pub struct SecretSpec {
    pub name: String,
    /// Run as the user; its standard output is the value (never logged or stored).
    pub command: String,
    pub key: String,
    pub every_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct McpPolicy {
    /// Preset name → the servers it allows.
    pub presets: BTreeMap<String, Vec<String>>,
    pub servers: BTreeMap<String, McpServerPolicy>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct McpServerPolicy {
    /// `per_session` (the default) or `shared`.
    pub mode: String,
    /// The protocol version the gateway pins (empty: the server's).
    pub protocol: String,
    pub rules: Vec<McpRule>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct McpRule {
    /// `allow` or `deny`.
    pub action: String,
    pub tool: String,
    /// Argument name → the value it must have (placeholders such as `{{box.branch}}`).
    pub when: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Profile {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub config_keys: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PathMap {
    pub host: String,
    pub guest: String,
    /// `ro-snapshot`, `rw-notes` or `none`.
    pub mode: String,
}

impl Pack {
    pub fn repo(&self, key: &str) -> Option<&Repo> {
        self.repos.iter().find(|r| r.key == key)
    }

    pub fn size(&self, name: Option<&str>) -> Option<&Size> {
        match name.filter(|n| !n.is_empty()) {
            Some(name) => self.sizes.iter().find(|s| s.name.eq_ignore_ascii_case(name)),
            None => self.sizes.first(),
        }
    }

    /// What clients may know of the pack (names, never values).
    pub fn summary(&self, errors: usize) -> PackSummary {
        PackSummary {
            name: self.name.clone(),
            provider: match self.provider {
                ProviderKind::Aca => "aca",
                ProviderKind::Hooks => "hooks",
                ProviderKind::Local => "local",
            }
            .into(),
            repos: self.repos.iter().map(|r| RepoSummary { key: r.key.clone(), default_branch: r.default_branch.clone(), folders: r.folders.clone() }).collect(),
            sizes: self.sizes.iter().map(|s| s.name.clone()).collect(),
            presets: self.mcp.presets.keys().cloned().collect(),
            errors: errors as u32,
        }
    }
}

/// One file of the pack being read: its parsed YAML and its text (for line numbers).
struct File<'a> {
    name: String,
    text: String,
    yaml: Yaml,
    findings: &'a mut Vec<Finding>,
}

impl File<'_> {
    /// The 1-based line where `key:` first appears (0: not found).
    fn line(&self, key: &str) -> u32 {
        let needle = format!("{key}:");
        self.text.lines().position(|l| l.trim_start().trim_start_matches("- ").starts_with(&needle)).map_or(0, |i| i as u32 + 1)
    }

    fn find(&mut self, level: Level, key: &str, text: impl Into<String>) {
        let line = self.line(key);
        self.findings.push(Finding { level, file: self.name.clone(), line, text: text.into() });
    }
}

fn text(y: &Yaml) -> Option<String> {
    match y {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Real(r) => Some(r.clone()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

fn texts(y: &Yaml) -> Vec<String> {
    match y {
        Yaml::Array(items) => items.iter().filter_map(text).collect(),
        other => text(other).into_iter().collect(),
    }
}

fn number(y: &Yaml) -> Option<f64> {
    match y {
        Yaml::Integer(i) => Some(*i as f64),
        Yaml::Real(r) => r.parse().ok(),
        Yaml::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `2000m` or `2` cores in millicores.
fn milli(text: &str) -> Option<u32> {
    let text = text.trim();
    match text.strip_suffix('m') {
        Some(m) => m.parse().ok(),
        None => text.parse::<f64>().ok().map(|c| (c * 1000.0) as u32),
    }
}

/// `4096Mi`, `4Gi`, `20Gi` in the unit asked for (`Mi` or `Gi`).
fn bytes_in(text: &str, unit: &str) -> Option<u32> {
    let text = text.trim();
    let (n, factor) = if let Some(n) = text.strip_suffix("Gi") {
        (n, if unit == "Mi" { 1024.0 } else { 1.0 })
    } else {
        (text.strip_suffix("Mi")?, if unit == "Mi" { 1.0 } else { 1.0 / 1024.0 })
    };
    n.trim().parse::<f64>().ok().map(|v| (v * factor).ceil() as u32)
}

fn read(folder: &Path, name: &str, required: bool, findings: &mut Vec<Finding>) -> Option<(String, Yaml)> {
    let path = folder.join(name);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if required {
                findings.push(Finding { level: Level::Error, file: name.into(), line: 0, text: "missing".into() });
            }
            return None;
        }
        Err(error) => {
            findings.push(Finding { level: Level::Error, file: name.into(), line: 0, text: format!("cannot be read: {}", error.kind()) });
            return None;
        }
    };
    match YamlLoader::load_from_str(&text) {
        Ok(mut docs) if !docs.is_empty() => Some((text, docs.remove(0))),
        Ok(_) => Some((text, Yaml::Hash(Default::default()))),
        Err(error) => {
            let line = error.marker().line() as u32 + 1;
            findings.push(Finding { level: Level::Error, file: name.into(), line, text: format!("not YAML: {}", error.info()) });
            None
        }
    }
}

/// Reads the pack in `folder`: the pack (as much as could be read) and what
/// was found. A pack with errors is shown but starts no VM.
pub fn load(folder: &Path) -> (Pack, PackStatus) {
    let mut findings = Vec::new();
    let mut pack = Pack { folder: folder.to_path_buf(), suspend_after_ms: 30 * 60_000, poll_seconds: 10, ring_mb: 4, ..Default::default() };
    if !folder.is_dir() {
        findings.push(Finding { level: Level::Error, file: String::new(), line: 0, text: "the pack's folder does not exist".into() });
        return (pack, PackStatus { folder: Some(folder.display().to_string()), findings });
    }
    if let Some((text, yaml)) = read(folder, "vm.yaml", true, &mut findings) {
        vm_yaml(&mut pack, &mut File { name: "vm.yaml".into(), text, yaml, findings: &mut findings });
    }
    if let Some((text, yaml)) = read(folder, "repos.yaml", true, &mut findings) {
        repos_yaml(&mut pack, &mut File { name: "repos.yaml".into(), text, yaml, findings: &mut findings });
    }
    if let Ok(entries) = std::fs::read_dir(folder.join("egress")) {
        let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "yaml" || e == "yml")).collect();
        names.sort();
        for path in names {
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let name = format!("egress/{}", path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
            if let Some((text, yaml)) = read(folder, &name, false, &mut findings) {
                egress_yaml(&mut File { name: name.clone(), text: text.clone(), yaml, findings: &mut findings });
                pack.egress.insert(stem, text);
            }
        }
    }
    if let Some((text, yaml)) = read(folder, "secrets.yaml", false, &mut findings) {
        secrets_yaml(&mut pack, &mut File { name: "secrets.yaml".into(), text, yaml, findings: &mut findings });
    }
    if let Some((text, yaml)) = read(folder, "mcp-policy.yaml", false, &mut findings) {
        mcp_yaml(&mut pack, &mut File { name: "mcp-policy.yaml".into(), text, yaml, findings: &mut findings });
    }
    if let Some((text, yaml)) = read(folder, "profile.yaml", false, &mut findings) {
        let mut file = File { name: "profile.yaml".into(), text, yaml, findings: &mut findings };
        pack.profile = Profile { include: texts(&file.yaml["include"]), exclude: texts(&file.yaml["exclude"]), config_keys: texts(&file.yaml["config_keys"]) };
        if pack.profile.include.iter().any(|p| p.contains("..")) {
            file.find(Level::Error, "include", "paths may not contain ..");
        }
    }
    if let Some((text, yaml)) = read(folder, "paths.yaml", false, &mut findings) {
        paths_yaml(&mut pack, &mut File { name: "paths.yaml".into(), text, yaml, findings: &mut findings });
    }
    hooks(&mut pack, &mut findings);
    checks(&mut pack, &mut findings);
    (pack, PackStatus { folder: Some(folder.display().to_string()), findings })
}

fn vm_yaml(pack: &mut Pack, file: &mut File) {
    let y = file.yaml.clone();
    match y["pack_version"].as_i64() {
        Some(PACK_VERSION) => {}
        Some(other) => file.find(Level::Error, "pack_version", format!("pack_version {other} is not one this build reads ({PACK_VERSION})")),
        None => file.find(Level::Error, "pack_version", format!("pack_version is missing (this build reads {PACK_VERSION})")),
    }
    pack.name = text(&y["name"]).unwrap_or_default();
    pack.provider = match text(&y["provider"]).as_deref() {
        Some("aca") | None => ProviderKind::Aca,
        Some("hooks") => ProviderKind::Hooks,
        Some("local") => ProviderKind::Local,
        Some(other) => {
            file.find(Level::Error, "provider", format!("unknown provider {other:?} (aca, hooks or local)"));
            ProviderKind::Aca
        }
    };
    pack.policy_ack = text(&y["policy_ack"]).unwrap_or_default();
    if pack.policy_ack.trim().is_empty() && pack.provider != ProviderKind::Local {
        file.find(Level::Warning, "policy_ack", "policy_ack is empty: acknowledge your data-handling policy for VM sessions (what may leave this machine) before using them (docs/VM-STATUS.md, S6)");
    }
    let aca = &y["aca"];
    pack.aca = Aca {
        subscription: text(&aca["subscription"]).unwrap_or_default(),
        resource_group: text(&aca["resource_group"]).unwrap_or_default(),
        sandbox_group: text(&aca["sandbox_group"]).unwrap_or_default(),
        region: text(&aca["region"]).unwrap_or_default(),
        api_version: text(&aca["api_version"]).unwrap_or_else(|| DEFAULT_API_VERSION.into()),
        endpoint: text(&aca["endpoint"]),
    };
    if pack.provider == ProviderKind::Aca {
        for (key, value) in [("subscription", &pack.aca.subscription), ("resource_group", &pack.aca.resource_group), ("sandbox_group", &pack.aca.sandbox_group), ("region", &pack.aca.region)] {
            if value.trim().is_empty() {
                file.find(Level::Error, "aca", format!("aca.{key} is missing"));
            } else if value.contains(['/', '?', '#']) || value.contains("..") {
                file.find(Level::Error, key, format!("aca.{key} may not contain '/', '?', '#' or '..'"));
            }
        }
        if pack.aca.region.chars().any(|c| !c.is_ascii_alphanumeric()) {
            file.find(Level::Error, "region", "aca.region is a region name such as eastus2");
        }
    }
    pack.auth = Auth { audience: text(&y["auth"]["audience"]).unwrap_or_else(|| DEFAULT_AUDIENCE.into()), command: text(&y["auth"]["command"]).filter(|c| !c.trim().is_empty()) };
    if let Some(sizes) = y["sizes"].as_vec() {
        for item in sizes {
            let name = text(&item["name"]).unwrap_or_default();
            let size = Size {
                cpu_milli: text(&item["cpu"]).as_deref().and_then(milli).unwrap_or(0),
                memory_mi: text(&item["memory"]).as_deref().and_then(|m| bytes_in(m, "Mi")).unwrap_or(0),
                disk_gi: text(&item["disk"]).as_deref().and_then(|d| bytes_in(d, "Gi")).unwrap_or(0),
                cents_per_hour: number(&item["cents_per_hour"]).unwrap_or(0.0) as u32,
                name,
            };
            if size.name.is_empty() || size.cpu_milli == 0 || size.memory_mi == 0 {
                file.find(Level::Error, "sizes", "every size needs a name, cpu (2000m) and memory (4096Mi)");
            } else {
                pack.sizes.push(size);
            }
        }
    }
    if let Some(map) = y["sizes"].as_hash() {
        // The earlier draft of docs/VM-SESSIONS.md keyed sizes by name.
        let names: Vec<String> = map.keys().filter_map(text).collect();
        file.find(
            Level::Error,
            "sizes",
            format!("sizes is a list now, not a map keyed by name (the earlier draft's format): write `sizes:` then `- name: {}` with its cpu, memory and disk under it, one item per size", names.first().map(String::as_str).unwrap_or("small")),
        );
    } else if pack.sizes.is_empty() {
        file.find(Level::Error, "sizes", "no sizes: list at least one (name, cpu, memory, disk)");
    }
    let duration = |file: &mut File, key: &str, value: &Yaml, default: u64| -> u64 {
        match text(value) {
            None => default,
            Some(t) => canvas_model::bus::parse_duration(&t).unwrap_or_else(|| {
                file.find(Level::Error, key, format!("{key} takes a duration such as 30m or 12h"));
                default
            }),
        }
    };
    pack.pool.max_age_ms = duration(file, "max_age", &y["pool"]["max_age"], 12 * 3_600_000);
    if let Some(min) = y["pool"]["min"].as_hash() {
        for (k, v) in min {
            if let (Some(k), Some(n)) = (text(k), v.as_i64()) {
                pack.pool.min.insert(k, n.clamp(0, 50) as u32);
            }
        }
    }
    pack.images = Images { schedule: text(&y["images"]["schedule"]).unwrap_or_else(|| "daily 02:00".into()), keep: y["images"]["keep"].as_i64().unwrap_or(3).clamp(1, 20) as u32 };
    pack.budget = Budget {
        max_running: y["budget"]["max_running"].as_i64().unwrap_or(4).clamp(0, 1000) as u32,
        max_cores: number(&y["budget"]["max_cores"]).unwrap_or(16.0) as f32,
        daily_cents: (number(&y["budget"]["daily_soft"]).unwrap_or(0.0) * 100.0) as u64,
    };
    pack.suspend_after_ms = duration(file, "suspend_after", &y["suspend_after"], 30 * 60_000);
    pack.poll_seconds = y["poll_seconds"].as_i64().unwrap_or(10).clamp(1, 600) as u64;
    pack.tunnel = Tunnel { url: text(&y["tunnel"]["url"]).unwrap_or_default(), secret: text(&y["tunnel"]["secret"]).unwrap_or_default() };
    if !pack.tunnel.url.is_empty() && !(pack.tunnel.url.starts_with("wss://") || pack.tunnel.url.starts_with("ws://")) {
        file.find(Level::Error, "url", "tunnel.url is the hub's WebSocket address (wss://…)");
    }
    pack.git = GitIdentity { name: text(&y["git"]["name"]).unwrap_or_default(), email: text(&y["git"]["email"]).unwrap_or_default() };
    pack.satellite = SatelliteBinary { path: text(&y["satellite"]["path"]).filter(|p| !p.is_empty()).map(PathBuf::from), sha256: text(&y["satellite"]["sha256"]).filter(|s| !s.is_empty()) };
    pack.ring_mb = y["link"]["ring_mb"].as_i64().unwrap_or(4).clamp(1, 16) as u32;
    pack.local_root = text(&y["local"]["root"]).filter(|p| !p.is_empty()).map(PathBuf::from);
}

fn repos_yaml(pack: &mut Pack, file: &mut File) {
    let y = file.yaml.clone();
    let Some(repos) = y["repos"].as_hash() else {
        file.find(Level::Error, "repos", "repos: is missing (a map of repository keys)");
        return;
    };
    for (key, r) in repos {
        let key = text(key).unwrap_or_default();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') || key.len() > 60 {
            file.find(Level::Error, &key, format!("repository key {key:?}: letters, digits, '-' and '_' only"));
            continue;
        }
        let repo = Repo {
            url: text(&r["url"]).unwrap_or_default(),
            default_branch: text(&r["default_branch"]).unwrap_or_else(|| "main".into()),
            clone_mode: text(&r["clone"]["mode"]).unwrap_or_else(|| "blobless".into()),
            sparse: texts(&r["clone"]["sparse"]),
            image_base: text(&r["image"]["base"]).unwrap_or_else(|| "ubuntu".into()),
            image_disk_gi: text(&r["image"]["disk"]).as_deref().and_then(|d| bytes_in(d, "Gi")).unwrap_or(0),
            egress: text(&r["egress"]).unwrap_or_default(),
            folders: texts(&r["folders"]),
            key: key.clone(),
        };
        if repo.url.is_empty() {
            file.find(Level::Error, &key, format!("{key}: url is missing"));
        }
        if !canvas_model::vm::branch_ok(&repo.default_branch) {
            file.find(Level::Error, "default_branch", format!("{key}: default_branch is not a branch name"));
        }
        if !matches!(repo.clone_mode.as_str(), "full" | "blobless" | "single_branch") {
            file.find(Level::Error, "mode", format!("{key}: clone.mode is full, blobless or single_branch"));
        }
        pack.repos.push(repo);
    }
    if pack.repos.is_empty() {
        file.find(Level::Error, "repos", "no repositories");
    }
}

/// Keys of the earlier draft of docs/VM-SESSIONS.md that a rule may carry
/// instead of `host`, with what to write now.
const DRAFT_RULE_KEYS: [(&str, &str); 7] = [
    ("domain", "`host: NAME`"),
    ("fqdn", "`host: NAME`"),
    ("hostname", "`host: NAME`"),
    ("destination", "`host: NAME`"),
    ("url", "`host: NAME` (the host alone, no scheme or path)"),
    ("hosts", "one rule per host (`- host: NAME`)"),
    ("domains", "one rule per host (`- host: NAME`)"),
];

fn egress_yaml(file: &mut File) {
    let y = file.yaml.clone();
    // The earlier draft's names: say what they are now.
    for old in ["defaultAction", "default_action", "defaultaction"] {
        if !y[old].is_badvalue() {
            let value = text(&y[old]).unwrap_or_else(|| "deny".into()).to_lowercase();
            file.find(Level::Error, old, format!("{old} is the earlier draft's key: write `default: {value}` and list the hosts the guest may reach under `rules:` (`- host: NAME`, action allow, deny or transform)"));
        }
    }
    match text(&y["default"]).as_deref() {
        Some("deny") | Some("Deny") | None => {}
        Some("allow") | Some("Allow") => file.find(Level::Warning, "default", "default: allow lets the guest reach any host; VM sessions should deny by default"),
        Some(other) => file.find(Level::Error, "default", format!("default is deny or allow, not {other:?}")),
    }
    for rule in y["rules"].as_vec().into_iter().flatten() {
        if text(&rule["host"]).is_none_or(|h| h.trim().is_empty()) {
            match DRAFT_RULE_KEYS.iter().find(|(key, _)| !rule[*key].is_badvalue()) {
                Some((key, now)) => file.find(Level::Error, "rules", format!("every rule names a host: this one has `{key}` (the earlier draft's key); write {now}")),
                None => file.find(Level::Error, "rules", "every rule names a host"),
            }
        }
        let action = text(&rule["action"]).unwrap_or_else(|| "allow".into());
        if !matches!(action.as_str(), "allow" | "deny" | "transform") {
            file.find(Level::Error, "action", format!("action is allow, deny or transform, not {action:?}"));
        }
        for header in rule["headers"].as_vec().into_iter().flatten() {
            if text(&header["value"]).is_some_and(|value| !value.contains("{{secret:")) {
                file.find(Level::Error, "headers", "header values come from {{secret:NAME}} (a value written in the pack would reach the provider's policy in the clear)");
            }
        }
    }
}

fn secrets_yaml(pack: &mut Pack, file: &mut File) {
    let y = file.yaml.clone();
    let Some(secrets) = y["secrets"].as_hash() else { return };
    for (name, s) in secrets {
        let name = text(name).unwrap_or_default();
        let spec = SecretSpec {
            command: text(&s["command"]).unwrap_or_default(),
            key: text(&s["key"]).unwrap_or_else(|| "value".into()),
            every_ms: text(&s["every"]).and_then(|e| canvas_model::bus::parse_duration(&e)).unwrap_or(50 * 60_000),
            name: name.clone(),
        };
        if spec.command.trim().is_empty() {
            file.find(Level::Error, &name, format!("{name}: command is missing (its standard output becomes the secret)"));
            continue;
        }
        file.find(Level::Info, &name, format!("secret {name} is refreshed every {} min by running, as you: {}", spec.every_ms / 60_000, spec.command));
        pack.secrets.push(spec);
    }
}

fn mcp_yaml(pack: &mut Pack, file: &mut File) {
    let y = file.yaml.clone();
    if let Some(presets) = y["presets"].as_hash() {
        for (name, servers) in presets {
            pack.mcp.presets.insert(text(name).unwrap_or_default(), texts(servers));
        }
    }
    let Some(servers) = y["servers"].as_hash() else { return };
    for (name, s) in servers {
        let name = text(name).unwrap_or_default();
        let mode = text(&s["mode"]).unwrap_or_else(|| "per_session".into());
        if !matches!(mode.as_str(), "per_session" | "shared") {
            file.find(Level::Error, "mode", format!("{name}: mode is per_session or shared"));
        }
        let mut rules = Vec::new();
        for rule in s["rules"].as_vec().into_iter().flatten() {
            let (action, tool) = match (text(&rule["allow"]), text(&rule["deny"])) {
                (Some(tool), None) => ("allow", tool),
                (None, Some(tool)) => ("deny", tool),
                _ => {
                    file.find(Level::Error, "rules", format!("{name}: every rule is `allow: TOOL` or `deny: TOOL`"));
                    continue;
                }
            };
            let mut when = BTreeMap::new();
            if let Some(map) = rule["when"].as_hash() {
                for (k, v) in map {
                    if let (Some(k), Some(v)) = (text(k), text(v)) {
                        when.insert(k, v);
                    }
                }
            }
            rules.push(McpRule { action: action.into(), tool, when });
        }
        pack.mcp.servers.insert(name, McpServerPolicy { mode, protocol: text(&s["protocol"]).unwrap_or_default(), rules });
    }
}

fn paths_yaml(pack: &mut Pack, file: &mut File) {
    let y = file.yaml.clone();
    for item in y["paths"].as_vec().into_iter().flatten() {
        let map = PathMap { host: text(&item["host"]).unwrap_or_default(), guest: text(&item["guest"]).unwrap_or_default(), mode: text(&item["mode"]).unwrap_or_else(|| "ro-snapshot".into()) };
        if map.host.is_empty() || !map.guest.starts_with('/') || !matches!(map.mode.as_str(), "ro-snapshot" | "rw-notes" | "none") {
            file.find(Level::Error, "paths", "every path maps a host folder to an absolute guest path (/…) with mode ro-snapshot, rw-notes or none");
            continue;
        }
        pack.paths.push(map);
    }
}

fn hooks(pack: &mut Pack, findings: &mut Vec<Finding>) {
    let dir = pack.folder.join("hooks");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        if pack.provider == ProviderKind::Hooks {
            findings.push(Finding { level: Level::Error, file: "hooks/".into(), line: 0, text: "provider: hooks needs a hooks/ folder".into() });
        }
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if HOOKS.contains(&stem.as_str()) && path.is_file() && hook_runs_here(&path) {
            pack.hooks.insert(stem, path);
        }
    }
    for name in pack.hooks.keys() {
        findings.push(Finding { level: Level::Info, file: format!("hooks/{name}"), line: 0, text: "runs as you when VM sessions need it".into() });
    }
    if pack.provider == ProviderKind::Hooks {
        for name in ["claim", "shell-command", "exec", "delete"] {
            if !pack.hooks.contains_key(name) {
                findings.push(Finding { level: Level::Error, file: format!("hooks/{name}"), line: 0, text: "missing (provider: hooks needs claim, shell-command, exec and delete)".into() });
            }
        }
    }
}

/// A hook file this machine can run: PowerShell, a command script or an
/// executable on Windows; a shell script or an executable elsewhere.
fn hook_runs_here(path: &Path) -> bool {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase());
    if cfg!(windows) {
        matches!(ext.as_deref(), Some("ps1" | "cmd" | "bat" | "exe"))
    } else {
        matches!(ext.as_deref(), Some("sh") | None)
    }
}

fn checks(pack: &mut Pack, findings: &mut Vec<Finding>) {
    let error = |findings: &mut Vec<Finding>, file: &str, text: String| findings.push(Finding { level: Level::Error, file: file.into(), line: 0, text });
    for repo in &pack.repos {
        if !repo.egress.is_empty() && !pack.egress.contains_key(&repo.egress) {
            error(findings, "repos.yaml", format!("{}: egress profile {:?} has no egress/{}.yaml", repo.key, repo.egress, repo.egress));
        } else if !repo.egress.is_empty() {
            // Rendered as at a claim (a box's values made up): what a claim would refuse, `pack check` says now.
            let vars = super::egress::Vars { branch: "atelier/check".into(), repo: repo.key.clone(), r#box: "check".into(), sandbox: "check".into(), hub_host: "hub.example.com".into() };
            if let Err(why) = super::egress::render(pack, &repo.egress, &vars) {
                error(findings, &format!("egress/{}.yaml", repo.egress), format!("{why:#}"));
            }
        }
        if repo.image_disk_gi != 0 && pack.sizes.iter().all(|s| s.disk_gi != 0 && s.disk_gi < repo.image_disk_gi) {
            error(findings, "repos.yaml", format!("{}: its image disk is larger than every size's disk", repo.key));
        }
    }
    for key in pack.pool.min.keys() {
        if pack.repo(key).is_none() {
            error(findings, "vm.yaml", format!("pool.min names {key:?}, which repos.yaml does not have"));
        }
    }
    for (name, text) in &pack.egress {
        for secret in placeholders(text, "secret:") {
            if !pack.secrets.iter().any(|s| s.name == secret) && secret != pack.tunnel.secret {
                error(findings, &format!("egress/{name}.yaml"), format!("{{{{secret:{secret}}}}} is not defined in secrets.yaml"));
            }
        }
    }
    for (preset, servers) in &pack.mcp.presets {
        for server in servers {
            if !pack.mcp.servers.contains_key(server) {
                error(findings, "mcp-policy.yaml", format!("preset {preset}: server {server:?} has no policy"));
            }
        }
    }
    match (&pack.satellite.path, &pack.satellite.sha256) {
        (Some(path), Some(sha)) => match std::fs::read(path) {
            Ok(bytes) => {
                use sha2::Digest;
                let digest = sha2::Sha256::digest(&bytes);
                let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
                if !hex.eq_ignore_ascii_case(sha.trim()) {
                    error(findings, "vm.yaml", "satellite.sha256 does not match satellite.path (the Linux build of atelier)".into());
                }
            }
            Err(_) => error(findings, "vm.yaml", "satellite.path cannot be read".into()),
        },
        (Some(_), None) => error(findings, "vm.yaml", "satellite.sha256 is missing: the satellite is pinned by its SHA-256".into()),
        _ if pack.provider == ProviderKind::Aca => findings.push(Finding { level: Level::Warning, file: "vm.yaml".into(), line: 0, text: "satellite.path is not set: images and sandboxes get no satellite (the thin lane still works)".into() }),
        _ => {}
    }
    if let Some(remote) = git_remote_of(&pack.folder) {
        findings.push(Finding { level: Level::Warning, file: String::new(), line: 0, text: format!("the pack is inside a git work tree with a remote ({remote}): keep packs out of repositories") });
    }
}

/// `{{prefix:NAME}}` placeholders in `text`.
pub fn placeholders(text: &str, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let open = format!("{{{{{prefix}");
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find("}}") else { break };
        out.push(after[..end].trim().to_owned());
        rest = &after[end + 2..];
    }
    out
}

/// The kind of remote of the git work tree `folder` is in (never its URL), if any.
fn git_remote_of(folder: &Path) -> Option<String> {
    let mut dir = Some(folder);
    while let Some(d) = dir {
        let config = d.join(".git").join("config");
        if let Ok(text) = std::fs::read_to_string(&config) {
            let url = text.lines().find_map(|l| l.trim().strip_prefix("url = "))?;
            let host = url.split("://").nth(1).unwrap_or(url).split(['/', ':']).next().unwrap_or_default().rsplit('@').next().unwrap_or_default().to_ascii_lowercase();
            return Some(if matches!(host.as_str(), "github.com" | "gitlab.com" | "bitbucket.org") { format!("{host}, possibly public") } else { "a remote".into() });
        }
        if d.join(".git").exists() {
            return None;
        }
        dir = d.parent();
    }
    None
}

/// The configured pack: `ATELIER_VM_PACK`, else settings.json `vm.pack`.
pub fn configured(home: &Path) -> Option<PathBuf> {
    canvas_common::settings::Settings::load(home).vm_pack()
}

/// The text `atelier vm pack check` prints.
pub fn report(status: &PackStatus) -> String {
    let Some(folder) = &status.folder else {
        return "No VM pack is configured (settings.json vm.pack or ATELIER_VM_PACK): VM sessions are off, and sessions run on this machine.".into();
    };
    let mut out = vec![format!("VM pack {folder}")];
    let mut findings = status.findings.clone();
    findings.sort_by(|a, b| b.level.cmp(&a.level).then(a.file.cmp(&b.file)).then(a.line.cmp(&b.line)));
    for f in &findings {
        let level = match f.level {
            Level::Error => "error",
            Level::Warning => "warning",
            Level::Info => "info",
        };
        let place = match (f.file.is_empty(), f.line) {
            (true, _) => String::new(),
            (false, 0) => format!("{}: ", f.file),
            (false, line) => format!("{}:{line}: ", f.file),
        };
        out.push(format!("  {level:<7} {place}{}", f.text));
    }
    let errors = status.errors();
    out.push(if errors == 0 { "The pack is usable.".into() } else { format!("{errors} error(s): VM sessions do not start until they are fixed.") });
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/vm-pack")
    }

    #[test]
    fn the_example_pack_reads_without_errors_and_shows_no_values() {
        let (pack, status) = load(&example());
        let errors: Vec<&Finding> = status.findings.iter().filter(|f| f.level == Level::Error).collect();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(pack.provider, ProviderKind::Aca);
        assert!(pack.repo("contoso-app").is_some_and(|r| r.egress == "forge" && r.folders == ["**/contoso-app"]));
        assert_eq!(pack.size(None).map(|s| s.name.as_str()), Some("S"), "the first size is the default");
        assert_eq!(pack.size(Some("m")).map(|s| (s.cpu_milli, s.memory_mi, s.disk_gi)), Some((2000, 4096, 40)));
        assert!(pack.egress.contains_key("forge") && pack.secrets.iter().any(|s| s.name == "forge-token"));
        assert!(pack.mcp.servers["forge"].rules.iter().any(|r| r.action == "allow" && r.tool == "create_pull_request" && r.when.get("head").map(String::as_str) == Some("{{box.branch}}")));
        assert_eq!(pack.paths.len(), 1);
        assert_eq!(pack.auth.audience, DEFAULT_AUDIENCE);
        // Debug and the summary carry names, never the pack's values.
        let debug = format!("{pack:?}");
        assert!(!debug.contains("contoso-rg") && !debug.contains("example.com") && debug.contains("contoso-app"), "{debug}");
        let summary = serde_json::to_string(&pack.summary(0)).unwrap();
        assert!(!summary.contains("example.com/contoso") && !summary.contains("00000000-0000"), "{summary}");
        // The commands a pack runs are shown by the check.
        assert!(status.findings.iter().any(|f| f.file == "secrets.yaml" && f.text.contains("runs") || f.text.contains("running, as you")));
        assert!(report(&status).ends_with("The pack is usable."), "{}", report(&status));
    }

    #[test]
    fn problems_name_their_file_and_line() {
        let dir = std::env::temp_dir().join(format!("atelier-pack-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("egress")).unwrap();
        std::fs::write(dir.join("vm.yaml"), "pack_version: 2\nprovider: aca\naca:\n  region: east us\nsizes:\n  - name: S\n").unwrap();
        std::fs::write(dir.join("repos.yaml"), "repos:\n  bad key:\n    url: x\n  app:\n    default_branch: -x\n    egress: missing\n").unwrap();
        std::fs::write(dir.join("egress/open.yaml"), "default: allow\nrules:\n  - host: example.com\n    action: transform\n    headers:\n      - name: Authorization\n        value: Bearer plain\n").unwrap();
        std::fs::write(dir.join("mcp-policy.yaml"), "servers: [").unwrap();
        let (pack, status) = load(&dir);
        let find = |file: &str, text: &str| status.findings.iter().find(|f| f.file == file && f.text.contains(text)).unwrap_or_else(|| panic!("{file}: {text} in {:#?}", status.findings));
        assert_eq!(find("vm.yaml", "pack_version 2").line, 1);
        assert_eq!(find("vm.yaml", "region name").line, 4);
        assert_eq!(find("vm.yaml", "every size needs").line, 5);
        find("vm.yaml", "aca.subscription is missing");
        find("repos.yaml", "letters, digits");
        assert_eq!(find("repos.yaml", "default_branch is not").line, 5);
        find("repos.yaml", "url is missing");
        find("repos.yaml", "no egress/missing.yaml");
        find("egress/open.yaml", "default: allow");
        find("egress/open.yaml", "{{secret:NAME}}");
        assert!(find("mcp-policy.yaml", "not YAML").line >= 1);
        assert!(status.errors() > 5 && report(&status).contains("VM sessions do not start"));
        assert!(!report(&status).contains("Bearer plain"), "never a value");
        assert_eq!(pack.repos.len(), 1);
        let (_, missing) = load(&dir.join("nope"));
        assert!(missing.errors() == 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_pack_in_the_earlier_drafts_format_is_told_the_new_key_names() {
        let dir = std::env::temp_dir().join(format!("atelier-pack-draft-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("egress")).unwrap();
        std::fs::write(dir.join("vm.yaml"), "pack_version: 1\nprovider: aca\nsizes:\n  small:\n    cpu: 2000m\n    memory: 4096Mi\n").unwrap();
        std::fs::write(dir.join("egress/forge.yaml"), "defaultAction: Deny\nrules:\n  - domain: example.com\n  - hosts: [a.example.com, b.example.com]\n  - action: allow\n").unwrap();
        let (_, status) = load(&dir);
        let find = |file: &str, text: &str| status.findings.iter().find(|f| f.file == file && f.text.contains(text)).unwrap_or_else(|| panic!("{file}: {text} in {:#?}", status.findings));
        assert_eq!(find("vm.yaml", "sizes is a list now").line, 3);
        find("vm.yaml", "`- name: small`");
        assert_eq!(find("egress/forge.yaml", "defaultAction is the earlier draft's key: write `default: deny`").line, 1);
        find("egress/forge.yaml", "this one has `domain` (the earlier draft's key); write `host: NAME`");
        find("egress/forge.yaml", "this one has `hosts` (the earlier draft's key); write one rule per host");
        assert!(status.findings.iter().any(|f| f.text == "every rule names a host"), "a rule with neither keeps the plain message");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn units_and_placeholders() {
        assert_eq!((milli("2000m"), milli("2"), milli("0.5"), milli("x")), (Some(2000), Some(2000), Some(500), None));
        assert_eq!((bytes_in("4Gi", "Mi"), bytes_in("4096Mi", "Mi"), bytes_in("20Gi", "Gi"), bytes_in("512Mi", "Gi"), bytes_in("4G", "Gi")), (Some(4096), Some(4096), Some(20), Some(1), None));
        assert_eq!(placeholders("a {{secret:x}} b {{ secret:y }} {{repo.path}}", "secret:"), ["x"]);
        assert_eq!(placeholders("{{secret:x}}{{secret:y}}", "secret:"), ["x", "y"]);
        assert_eq!(report(&PackStatus::default()), "No VM pack is configured (settings.json vm.pack or ATELIER_VM_PACK): VM sessions are off, and sessions run on this machine.");
    }
}
