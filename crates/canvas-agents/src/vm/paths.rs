//! Paths between a VM box's guest and this machine, for the canvas commands
//! relayed from it (plan §4): a path the guest names (`todo link --note
//! PATH`) is this machine's when it is inside the group's notes as synced
//! there or a folder `paths.yaml` maps, else the command is refused; this
//! machine's paths in a command's answer (`note new` prints where the note
//! went) become the guest's.

use std::path::Path;

use canvas_model::vm::CliOp;

use super::launch::Intent;

/// The pairs of (this machine's folder, the guest's) for one box.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Places {
    pairs: Vec<(String, String)>,
}

/// Options whose value is a path of the machine the command runs on.
const PATH_OPTIONS: [(&str, &str); 2] = [("todo", "note"), ("todo", "file")];

fn normal(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_owned()
}

impl Places {
    /// The box's group notes (its folder here, `<roots>/notes/<group>` there) and the pack's mapped folders.
    pub fn of(home: &Path, intent: &Intent, guest_roots: &str) -> Places {
        let mut pairs = Vec::new();
        if !intent.group_folder.is_empty() && !guest_roots.is_empty() {
            pairs.push((normal(&intent.group_folder), format!("{}/notes/{}", guest_roots.trim_end_matches('/'), super::sync::group_slug(&intent.group_name))));
        }
        if let Some(folder) = super::pack::configured(home) {
            let (pack, _) = super::pack::load(&folder);
            pairs.extend(pack.paths.iter().filter(|m| m.mode != "none").map(|m| (normal(&m.host), normal(&m.guest))));
        }
        Places { pairs }
    }

    #[cfg(test)]
    fn new(pairs: &[(&str, &str)]) -> Places {
        Places { pairs: pairs.iter().map(|(h, g)| (normal(h), normal(g))).collect() }
    }

    /// The guest's `path` as this machine has it (`None`: only in the guest).
    pub fn to_host(&self, path: &str) -> Option<String> {
        let path = normal(path);
        if path.split('/').any(|p| p == "..") {
            return None;
        }
        self.pairs.iter().find_map(|(host, guest)| {
            let tail = path.strip_prefix(guest.as_str())?;
            (tail.is_empty() || tail.starts_with('/')).then(|| format!("{host}{tail}"))
        })
    }

    /// The command's path options in this machine's terms; `Err`: a path only the guest has.
    pub fn command_in(&self, op: &mut CliOp) -> Result<(), String> {
        let verb = op.words.first().cloned().unwrap_or_default();
        for (key, value) in op.options.iter_mut() {
            if !PATH_OPTIONS.iter().any(|(v, k)| *v == verb && k == key) {
                continue;
            }
            match self.to_host(value) {
                Some(host) => *value = host,
                None => return Err(format!("{value} is only on the VM: write it as a note with `atelier note new` (it is filed on the canvas's machine), then link that")),
            }
        }
        Ok(())
    }

    /// An answer's text with this machine's folders as the guest has them.
    pub fn text_out(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for (host, guest) in &self.pairs {
            for variant in [host.clone(), host.replace('/', "\\")] {
                out = replace_path(&out, &variant, guest);
            }
        }
        out
    }
}

/// Replaces each `from` in `text` that starts a path with `to`, the rest of
/// that path with `/` separators.
fn replace_path(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(from) {
        out.push_str(&rest[..at]);
        let after = &rest[at + from.len()..];
        // Only a whole folder: what follows ends the path or goes on into it.
        if !(after.is_empty() || after.starts_with(['/', '\\']) || after.starts_with(|c: char| c.is_whitespace() || "`'\")".contains(c))) {
            out.push_str(from);
            rest = after;
            continue;
        }
        out.push_str(to);
        let end = after.find(|c: char| c.is_whitespace() || "`'\")".contains(c)).unwrap_or(after.len());
        out.push_str(&after[..end].replace('\\', "/"));
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_paths_become_this_machines_and_back() {
        let places = Places::new(&[("/Users/c/groups/design", "/home/agent/.atelier/vm/roots/notes/design"), ("C:\\work\\specs", "/home/agent/specs")]);
        assert_eq!(places.to_host("/home/agent/.atelier/vm/roots/notes/design/ai-docs/plan.md").as_deref(), Some("/Users/c/groups/design/ai-docs/plan.md"));
        assert_eq!(places.to_host("/home/agent/specs/api.md").as_deref(), Some("C:/work/specs/api.md"));
        assert_eq!(places.to_host("/home/agent/work/app/README.md"), None);
        assert_eq!(places.to_host("/home/agent/specs/../../etc/passwd"), None);
        let mut op = CliOp { words: vec!["todo".into(), "link".into(), "WI-1".into()], options: vec![("note".into(), "/home/agent/specs/api.md".into()), ("title".into(), "/home/agent/x".into())], ..Default::default() };
        places.command_in(&mut op).unwrap();
        assert_eq!(op.options, [("note".to_owned(), "C:/work/specs/api.md".to_owned()), ("title".to_owned(), "/home/agent/x".to_owned())], "path options only");
        let mut op = CliOp { words: vec!["todo".into(), "link".into(), "WI-1".into()], options: vec![("file".into(), "/home/agent/work/app/src.rs".into())], ..Default::default() };
        assert!(places.command_in(&mut op).unwrap_err().contains("only on the VM"));
        let text = "Wrote /Users/c/groups/design/ai-docs/plan.md and `C:\\work\\specs\\api.md`; /Users/c/groups/designer/x stays.";
        assert_eq!(places.text_out(text), "Wrote /home/agent/.atelier/vm/roots/notes/design/ai-docs/plan.md and `/home/agent/specs/api.md`; /Users/c/groups/designer/x stays.");
    }
}
