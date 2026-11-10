//! The service's environment on macOS and Linux (MAC-LINUX-PLAN §5.2): a
//! service started by the app, launchd or systemd gets a minimal `PATH` (no
//! Homebrew, nvm, volta, asdf, mise, `~/.local/bin`) and often no `LANG`, so
//! agents look "not installed". At start the service reads the user's login
//! environment once (`$SHELL -l -i -c env`, 5 s at most, as VS Code does)
//! and merges it under its own variables. Windows needs none of this.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Delimits the environment in the shell's output (rc files may print).
const MARK: &str = "__ATELIER_ENV__";

/// Variables of the login shell that describe that shell, not the user.
const SKIP: &[&str] = &["_", "PWD", "OLDPWD", "SHLVL", "PS1", "PS2", "PROMPT", "TERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "TERM_SESSION_ID", "TMUX", "COLUMNS", "LINES"];

/// What changed: the variables set from the login shell, and the resolved `PATH`.
#[derive(Debug, Default, Clone)]
pub struct Resolved {
    pub added: Vec<String>,
    pub path: String,
    /// Why nothing was read (no shell, timeout, …).
    pub error: Option<String>,
}

/// The variables a login environment adds to `current`: every variable
/// `current` lacks (the service's own win; `ATELIER_*`, and the deprecated
/// names before the rename, are never taken, so a canvas behaves the same however it was started), `PATH` as the
/// login shell's entries followed by the current ones it lacks, and a UTF-8
/// `LANG` when neither has one.
pub fn merge(current: &BTreeMap<String, String>, login: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (key, value) in login {
        if key == "PATH" || SKIP.contains(&key.as_str()) || canvas_common::env::is_ours(key) || current.contains_key(key) {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    if let Some(login_path) = login.get("PATH") {
        let mut entries: Vec<&str> = login_path.split(':').filter(|e| !e.is_empty()).collect();
        for entry in current.get("PATH").map(String::as_str).unwrap_or_default().split(':').filter(|e| !e.is_empty()) {
            if !entries.contains(&entry) {
                entries.push(entry);
            }
        }
        let path = entries.join(":");
        if current.get("PATH") != Some(&path) {
            out.insert("PATH".into(), path);
        }
    }
    let has = |key: &str| current.contains_key(key) || out.contains_key(key);
    if !has("LANG") && !has("LC_ALL") && !has("LC_CTYPE") {
        out.insert("LANG".into(), "en_US.UTF-8".into());
    }
    out
}

/// Parses `env -0` output between the marks.
fn parse(output: &[u8]) -> Option<BTreeMap<String, String>> {
    let text = String::from_utf8_lossy(output);
    let start = text.find(MARK)? + MARK.len();
    let end = start + text[start..].find(MARK)?;
    Some(
        text[start..end]
            .split('\0')
            .filter_map(|entry| entry.split_once('='))
            .filter(|(key, _)| !key.is_empty() && !key.contains(char::is_whitespace))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
    )
}

/// The user's login shell's environment (`$SHELL`, else the account's shell).
pub fn login_environment(timeout: Duration) -> Result<BTreeMap<String, String>, String> {
    use std::io::Read;
    let shell = std::env::var("SHELL").ok().filter(|s| s.starts_with('/')).unwrap_or_else(|| "/bin/sh".into());
    let script = format!("printf '%s' {MARK}; env -0; printf '%s' {MARK}");
    let mut child = std::process::Command::new(&shell)
        .args(["-l", "-i", "-c", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .env_remove("ATELIER_SELFTEST")
        .env_remove(canvas_common::env::legacy_name("ATELIER_SELFTEST").unwrap_or_default())
        .spawn()
        .map_err(|e| format!("cannot run {shell}: {e}"))?;
    let mut stdout = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = crate::process::kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{shell} -l -i did not finish within {} s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = reader.join().unwrap_or_default();
    parse(&output).ok_or_else(|| format!("{shell} -l -i printed no environment"))
}

/// Merges the login environment into this process's (call once, at the
/// service's start, before it starts threads). Skipped with
/// `ATELIER_LOGIN_ENV=0` and in self-tests (`ATELIER_SELFTEST=1`),
/// which keep their environment as given.
pub fn adopt_login_environment() -> Resolved {
    let current: BTreeMap<String, String> = std::env::vars().collect();
    let skipped = |key: &str, off: &str| current.get(key).or_else(|| canvas_common::env::legacy_name(key).and_then(|old| current.get(&old))).is_some_and(|v| v.trim() == off);
    if cfg!(windows) || skipped("ATELIER_LOGIN_ENV", "0") || skipped("ATELIER_SELFTEST", "1") {
        return Resolved { path: current.get("PATH").cloned().unwrap_or_default(), ..Default::default() };
    }
    let (login, error) = match login_environment(Duration::from_secs(5)) {
        Ok(login) => (login, None),
        Err(error) => (BTreeMap::new(), Some(error)),
    };
    let added = merge(&current, &login);
    for (key, value) in &added {
        // SAFETY: called at the service's start, before it starts threads.
        unsafe { std::env::set_var(key, value) };
    }
    Resolved { added: added.keys().cloned().collect(), path: std::env::var("PATH").unwrap_or_default(), error }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn the_login_environment_fills_in_and_extends_path() {
        let current = map(&[("PATH", "/usr/bin:/bin"), ("HOME", "/Users/me"), ("ATELIER_SELFTEST", "1")]);
        let login = map(&[
            ("PATH", "/opt/homebrew/bin:/Users/me/.local/bin:/usr/bin"),
            ("HOME", "/elsewhere"),
            ("NVM_DIR", "/Users/me/.nvm"),
            ("SHLVL", "2"),
            ("ATELIER_HOME", "/tmp/x"),
            ("COPILOT_CANVAS_SESSION", "s1"), // rename:keep
            ("LC_CTYPE", "UTF-8"),
        ]);
        let added = merge(&current, &login);
        assert_eq!(added.get("PATH").map(String::as_str), Some("/opt/homebrew/bin:/Users/me/.local/bin:/usr/bin:/bin"));
        assert_eq!(added.get("NVM_DIR").map(String::as_str), Some("/Users/me/.nvm"));
        assert!(!added.contains_key("HOME") && !added.contains_key("SHLVL") && !added.contains_key("ATELIER_HOME") && added.keys().all(|k| !canvas_common::env::is_ours(k)));
        assert!(!added.contains_key("LANG"), "LC_CTYPE came from the login shell");
        assert_eq!(merge(&map(&[("PATH", "/bin")]), &BTreeMap::new()).get("LANG").map(String::as_str), Some("en_US.UTF-8"));
    }

    #[test]
    fn env_output_is_read_between_the_marks() {
        let output = format!("motd text\n{MARK}A=1\0B=two=2\0\0{MARK}bye");
        assert_eq!(parse(output.as_bytes()), Some(map(&[("A", "1"), ("B", "two=2")])));
        assert_eq!(parse(b"no marks"), None);
    }

    #[test]
    fn a_login_shell_gives_its_environment() {
        let env = login_environment(Duration::from_secs(10)).unwrap();
        assert!(env.get("PATH").is_some_and(|p| !p.is_empty()), "{env:?}");
    }
}
