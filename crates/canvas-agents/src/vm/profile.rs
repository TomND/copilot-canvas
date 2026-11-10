//! The profile bundle and the path map (plan VM-35): what of the user's own
//! agent setup a VM session gets, and where folders of this machine are in
//! the guest.
//!
//! - `profile.yaml` names what to bundle (`include`, globs under the user's
//!   home such as `~/.copilot/skills/*`) and what never to (`exclude`). The
//!   bundle is a plugin the guest's agent loads (`--plugin-dir`): the skills
//!   found under `…/skills/<name>/` become its `skills/<name>/`, anything else
//!   is kept under `files/`. Whatever is named like a credential is never
//!   bundled, whatever the pack says, and neither is a skill whose front
//!   matter says `requires: host`. Bounded: 1 MiB a file, 16 MiB, 2,000 files.
//! - `paths.yaml` maps folders of this machine to guest paths
//!   (`ro-snapshot`: a copy kept up to date; `rw-notes`: edits come back;
//!   `none`: only named); the guest's agent gets an instruction listing them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::pack::Pack;

pub const MAX_FILE: u64 = 1 << 20;
pub const MAX_TOTAL: u64 = 16 << 20;
pub const MAX_FILES: usize = 2000;

/// Names never bundled (any component of the path).
const SECRET_NAMES: [&str; 13] = ["*token*", "*secret*", "*credential*", "*password*", "*passwd*", ".env", ".env.*", "*.pem", "*.key", "*.p12", "*.pfx", "id_rsa*", "id_ed25519*"];

/// The plugin manifest of the bundle (Copilot's `plugin.json`, Claude Code's `.claude-plugin/plugin.json`).
const PLUGIN: &str = r#"{ "name": "atelier-profile", "description": "Your own skills, bundled from the canvas's machine for this VM session.", "version": "1.0.0", "skills": "./skills/" }"#;

/// The bundle: its files by name (`skills/<name>/…`, `files/…`, the
/// manifests) and where each comes from, or its text; and why some were left out.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bundle {
    pub files: BTreeMap<String, Source>,
    pub skipped: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    File(PathBuf),
    Text(String),
}

fn secret_like(rel: &str) -> bool {
    rel.split('/').any(|part| {
        let part = part.to_ascii_lowercase();
        SECRET_NAMES.iter().any(|pattern| canvas_model::vm::glob_match(pattern, &part))
    })
}

fn excluded(pack: &Pack, rel: &str) -> bool {
    pack.profile.exclude.iter().any(|pattern| {
        let pattern = pattern.trim().trim_start_matches("~/");
        canvas_model::vm::glob_match(pattern, rel) || canvas_model::vm::glob_match(&format!("**/{pattern}"), rel)
    })
}

/// Whether a skill's `SKILL.md` says it needs this machine (`requires: host`).
fn needs_host(skill_md: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(skill_md) else { return false };
    let Some(front) = text.strip_prefix("---").and_then(|rest| rest.split("\n---").next()) else { return false };
    front.lines().any(|l| l.split_once(':').is_some_and(|(k, v)| k.trim() == "requires" && v.trim().trim_matches(['"', '\'']) == "host"))
}

/// Files under `dir` (links are never followed), as paths relative to `base`.
fn walk(dir: &Path, base: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 12 || out.len() > MAX_FILES * 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(base) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if kind.is_dir() {
            out.push(format!("{rel}/"));
            walk(&path, base, depth + 1, out);
        } else if kind.is_file() {
            out.push(rel);
        }
    }
}

/// Where a bundled file goes in the plugin: a skill's files under
/// `skills/<name>/`, anything else under `files/`.
fn place(rel: &str) -> String {
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.iter().position(|p| *p == "skills") {
        Some(i) if i + 2 < parts.len() => format!("skills/{}", parts[i + 1..].join("/")),
        _ => format!("files/{rel}"),
    }
}

/// The profile bundle for the user whose home is `home`.
pub fn bundle(pack: &Pack, home: &Path) -> Bundle {
    let mut bundle = Bundle::default();
    let mut candidates = Vec::new();
    for pattern in &pack.profile.include {
        let Some(pattern) = pattern.trim().strip_prefix("~/").map(|p| p.trim_end_matches('/').to_owned()) else {
            bundle.skipped.push(format!("{pattern}: only paths under ~/ are bundled"));
            continue;
        };
        // From the pattern's literal folders, then matched entry by entry (a matched folder brings its files).
        let literal: Vec<&str> = pattern.split('/').take_while(|p| !p.contains(['*', '?'])).collect();
        let start = home.join(literal.join("/"));
        if start.is_file() {
            candidates.push(literal.join("/"));
            continue;
        }
        let mut found = Vec::new();
        walk(&start, home, 0, &mut found);
        let mut matched_dirs: Vec<String> = Vec::new();
        for rel in found {
            let (path, is_dir) = match rel.strip_suffix('/') {
                Some(dir) => (dir.to_owned(), true),
                None => (rel.clone(), false),
            };
            let inside = matched_dirs.iter().any(|d| path.starts_with(&format!("{d}/")));
            if canvas_model::vm::glob_match(&pattern, &path) {
                if is_dir {
                    matched_dirs.push(path);
                } else {
                    candidates.push(path);
                }
            } else if inside && !is_dir {
                candidates.push(path);
            }
        }
    }
    let mut total = 0u64;
    let mut host_skills: Vec<String> = Vec::new();
    for rel in candidates {
        let name = place(&rel);
        if bundle.files.contains_key(&name) {
            continue;
        }
        if secret_like(&rel) || excluded(pack, &rel) {
            bundle.skipped.push(format!("{rel}: excluded"));
            continue;
        }
        if let Some(skill) = name.strip_prefix("skills/").and_then(|s| s.split('/').next()) {
            let dir = home.join(&rel[..rel.find(&format!("/{skill}/")).map(|i| i + skill.len() + 1).unwrap_or(rel.len())]);
            if host_skills.iter().any(|s| s == skill) {
                continue;
            }
            if needs_host(&dir.join("SKILL.md")) {
                host_skills.push(skill.to_owned());
                bundle.skipped.push(format!("skill {skill}: it requires this machine (requires: host)"));
                continue;
            }
        }
        if !canvas_common::sat::name_ok(&name) {
            bundle.skipped.push(format!("{rel}: a name the guest cannot keep"));
            continue;
        }
        let path = home.join(&rel);
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if meta.len() > MAX_FILE {
            bundle.skipped.push(format!("{rel}: larger than {} KiB", MAX_FILE >> 10));
            continue;
        }
        if bundle.files.len() >= MAX_FILES || total + meta.len() > MAX_TOTAL {
            bundle.skipped.push(format!("{rel}: the bundle is full"));
            continue;
        }
        total += meta.len();
        bundle.files.insert(name, Source::File(path));
    }
    bundle.files.insert("plugin.json".into(), Source::Text(PLUGIN.into()));
    bundle.files.insert(".claude-plugin/plugin.json".into(), Source::Text(PLUGIN.into()));
    if let Some(text) = paths_instruction(pack) {
        bundle.files.insert(format!("instructions/{PATHS_INSTRUCTION}"), Source::Text(text));
    }
    bundle
}

/// The generated instruction's file name (in the bundle's `instructions/`).
pub const PATHS_INSTRUCTION: &str = "vm-paths.instructions.md";

/// The instruction telling the guest's agent where this machine's folders
/// are (`None`: the pack maps none).
pub fn paths_instruction(pack: &Pack) -> Option<String> {
    if pack.paths.is_empty() {
        return None;
    }
    let mut text = String::from("---\napplyTo: '**'\n---\n# Folders of the canvas's machine\n\nThis session runs on a VM. Paths of the canvas's machine that you may meet in notes, todos or messages are here:\n\n");
    for map in &pack.paths {
        let how = match map.mode.as_str() {
            "rw-notes" => "a copy; what you change there goes back",
            "none" => "not available here",
            _ => "a read-only copy, kept up to date",
        };
        text.push_str(&format!("- `{}` → `{}` ({how})\n", map.host, map.guest));
    }
    text.push_str("\nAny other path of the canvas's machine is not available here.\n");
    Some(text)
}

fn normal(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_owned()
}

/// A path of this machine as the guest has it (`None`: not mapped, or mapped `none`).
pub fn guest_path(pack: &Pack, host: &str) -> Option<String> {
    let host = normal(host);
    let fold = |s: &str| if cfg!(windows) { s.to_ascii_lowercase() } else { s.to_owned() };
    pack.paths.iter().filter(|m| m.mode != "none").find_map(|map| {
        let prefix = normal(&map.host);
        let rest = fold(&host).strip_prefix(&fold(&prefix)).map(|r| r.len())?;
        let tail = &host[host.len() - rest..];
        (tail.is_empty() || tail.starts_with('/')).then(|| format!("{}{tail}", map.guest.trim_end_matches('/')))
    })
}

/// A guest path as this machine has it (`None`: not inside a mapped folder).
pub fn host_path(pack: &Pack, guest: &str) -> Option<String> {
    let guest = normal(guest);
    pack.paths.iter().filter(|m| m.mode != "none").find_map(|map| {
        let prefix = normal(&map.guest);
        let tail = guest.strip_prefix(&prefix)?;
        (tail.is_empty() || tail.starts_with('/')).then(|| format!("{}{tail}", map.host.trim_end_matches(['/', '\\'])))
    })
}

#[cfg(test)]
mod tests {
    use super::super::pack::{PathMap, Profile};
    use super::*;

    fn write(home: &Path, rel: &str, text: &str) {
        let path = home.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_bundle_takes_the_allowed_skills_and_never_a_credential() {
        let home = std::env::temp_dir().join(format!("atelier-profile-{}", uuid::Uuid::new_v4()));
        write(&home, ".copilot/skills/review/SKILL.md", "---\nname: review\n---\nReview well.\n");
        write(&home, ".copilot/skills/review/notes/checklist.md", "- tests\n");
        write(&home, ".copilot/skills/review/api-token.txt", "s3cret");
        write(&home, ".copilot/skills/review/.env", "KEY=1");
        write(&home, ".copilot/skills/deploy/SKILL.md", "---\nname: deploy\nrequires: host\n---\nDeploys from this machine.\n");
        write(&home, ".claude/skills/tidy/SKILL.md", "Tidy.\n");
        write(&home, ".claude/skills/tidy/big.bin", &"x".repeat((MAX_FILE + 1) as usize));
        write(&home, ".copilot/config.json", "{\"token\": \"no\"}");
        write(&home, ".copilot/skills/review/drafts/private.md", "private");
        let pack = Pack {
            profile: Profile { include: vec!["~/.copilot/skills/*".into(), "~/.claude/skills/*".into(), "/etc/passwd".into()], exclude: vec!["**/drafts/**".into()], config_keys: Vec::new() },
            paths: vec![PathMap { host: "/Users/contoso/notes".into(), guest: "/home/agent/notes".into(), mode: "rw-notes".into() }],
            ..Default::default()
        };
        let bundle = bundle(&pack, &home);
        let names: Vec<&str> = bundle.files.keys().map(String::as_str).collect();
        assert_eq!(names, [".claude-plugin/plugin.json", "instructions/vm-paths.instructions.md", "plugin.json", "skills/review/SKILL.md", "skills/review/notes/checklist.md", "skills/tidy/SKILL.md"]);
        let skipped = bundle.skipped.join("\n");
        for why in ["api-token.txt: excluded", ".env: excluded", "drafts/private.md: excluded", "skill deploy: it requires this machine", "big.bin: larger than", "/etc/passwd: only paths under ~/"] {
            assert!(skipped.contains(why), "{why} in:\n{skipped}");
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn mapped_paths_go_both_ways_and_the_instruction_lists_them() {
        let pack = Pack {
            paths: vec![
                PathMap { host: "/Users/contoso/notes".into(), guest: "/home/agent/notes".into(), mode: "rw-notes".into() },
                PathMap { host: "C:\\work\\specs".into(), guest: "/home/agent/specs".into(), mode: "ro-snapshot".into() },
                PathMap { host: "/Users/contoso/private".into(), guest: "/home/agent/private".into(), mode: "none".into() },
            ],
            ..Default::default()
        };
        assert_eq!(guest_path(&pack, "/Users/contoso/notes/a/b.md").as_deref(), Some("/home/agent/notes/a/b.md"));
        assert_eq!(guest_path(&pack, "/Users/contoso/notes").as_deref(), Some("/home/agent/notes"));
        assert_eq!(guest_path(&pack, "/Users/contoso/notes-old/x.md"), None, "a prefix of a name is not a folder");
        assert_eq!(guest_path(&pack, "C:\\work\\specs\\api.md").as_deref(), Some("/home/agent/specs/api.md"));
        assert_eq!(guest_path(&pack, "/Users/contoso/private/x"), None, "none: not there");
        assert_eq!(host_path(&pack, "/home/agent/notes/a.md").as_deref(), Some("/Users/contoso/notes/a.md"));
        assert_eq!(host_path(&pack, "/home/agent/elsewhere/a.md"), None);
        let text = paths_instruction(&pack).unwrap();
        assert!(text.contains("`/Users/contoso/notes` → `/home/agent/notes` (a copy; what you change there goes back)") && text.contains("(not available here)"));
        assert_eq!(paths_instruction(&Pack::default()), None);
    }
}
