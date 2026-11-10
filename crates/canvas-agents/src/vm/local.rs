//! `vm::local`: sandboxes as folders and local processes, for tests and CI
//! (never a real VM). Each sandbox is `<root>/sandboxes/<id>/` with its own
//! `home` (`HOME`, `ATELIER_HOME`, `COPILOT_HOME`, … all point inside it, so
//! nothing reaches the real user's folders) and `work/<repo>` (a clone when
//! the repository's URL is a local repository, else an empty folder).
//!
//! In Phase M it backs `atelier vm-hook-standin`, which answers the pack's
//! hooks the way real ones would, so the whole thin lane runs on any machine.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::paths::now_ms;

/// A sandbox's record (`state.json`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Sandbox {
    pub id: String,
    pub r#box: String,
    pub repo: String,
    pub branch: String,
    pub cwd: String,
    pub created_ms: u64,
    pub suspended: bool,
}

pub struct Local {
    pub root: PathBuf,
}

/// A sandbox id: `sbx-` and 12 hex digits.
pub fn valid_id(id: &str) -> bool {
    id.len() == 16 && id.starts_with("sbx-") && id[4..].chars().all(|c| c.is_ascii_hexdigit())
}

impl Local {
    pub fn new(root: PathBuf) -> Self {
        Local { root }
    }

    fn dir(&self, id: &str) -> Result<PathBuf> {
        anyhow::ensure!(valid_id(id), "not a sandbox id: {id:?}");
        let dir = self.root.join("sandboxes").join(id);
        anyhow::ensure!(dir.join("state.json").is_file(), "no sandbox {id}");
        Ok(dir)
    }

    pub fn get(&self, id: &str) -> Result<Sandbox> {
        let dir = self.dir(id)?;
        Ok(serde_json::from_slice(&std::fs::read(dir.join("state.json"))?)?)
    }

    fn save(&self, sandbox: &Sandbox) -> Result<()> {
        let path = self.root.join("sandboxes").join(&sandbox.id).join("state.json");
        crate::paths::write_atomic(&path, &serde_json::to_vec_pretty(sandbox)?)?;
        Ok(())
    }

    pub fn list(&self) -> Vec<Sandbox> {
        let Ok(entries) = std::fs::read_dir(self.root.join("sandboxes")) else { return Vec::new() };
        let mut out: Vec<Sandbox> = entries.flatten().filter_map(|e| std::fs::read(e.path().join("state.json")).ok()).filter_map(|b| serde_json::from_slice(&b).ok()).collect();
        out.sort_by(|a: &Sandbox, b: &Sandbox| a.id.cmp(&b.id));
        out
    }

    /// A new sandbox for `box` with `repo` cloned from `url` (when it is a
    /// local repository) on `branch` from `base` (tracking the remote branch
    /// when it exists).
    pub fn claim(&self, r#box: &str, repo: &str, url: &str, branch: &str, base: &str) -> Result<Sandbox> {
        anyhow::ensure!(canvas_model::vm::sync_name_ok(repo) && !repo.contains('/'), "not a repository key: {repo:?}");
        let id = format!("sbx-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let dir = self.root.join("sandboxes").join(&id);
        for sub in ["home/.atelier", "home/.copilot", "work"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        let work = dir.join("work").join(repo);
        let local_repo = !url.contains("://") && Path::new(url).exists();
        if local_repo {
            let git = |args: &[&str], cwd: &Path| -> Result<bool> {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(cwd).env("GIT_TERMINAL_PROMPT", "0");
                canvas_sys::spawn::CommandExt::no_window(&mut cmd);
                Ok(cmd.output().context("git is not installed")?.status.success())
            };
            if !git(&["clone", "--quiet", url, &work.display().to_string()], &dir)? {
                bail!("cannot clone {repo} into the sandbox");
            }
            let tracked = git(&["checkout", "--quiet", "-b", branch, "--track", &format!("origin/{branch}")], &work)?;
            if !tracked && !git(&["checkout", "--quiet", "-b", branch, &format!("origin/{base}")], &work)? && !git(&["checkout", "--quiet", "-b", branch], &work)? {
                bail!("cannot make the branch {branch}");
            }
        } else {
            std::fs::create_dir_all(&work)?;
        }
        let sandbox = Sandbox { id, r#box: r#box.to_owned(), repo: repo.to_owned(), branch: branch.to_owned(), cwd: work.display().to_string(), created_ms: now_ms(), suspended: false };
        self.save(&sandbox)?;
        Ok(sandbox)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let dir = self.dir(id)?;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    pub fn set_suspended(&self, id: &str, suspended: bool) -> Result<()> {
        let mut sandbox = self.get(id)?;
        sandbox.suspended = suspended;
        self.save(&sandbox)
    }

    /// The environment a process in the sandbox gets: its own home for
    /// everything (also on Windows, whose user folders do not follow `HOME`).
    pub fn env(&self, id: &str) -> Result<Vec<(String, String)>> {
        let home = self.dir(id)?.join("home");
        let h = |sub: &str| home.join(sub).display().to_string();
        let exe_dir = std::env::current_exe()?.parent().map(Path::to_path_buf).unwrap_or_default();
        let path = std::env::join_paths(std::iter::once(exe_dir).chain(std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default()))?;
        Ok(vec![
            ("HOME".into(), home.display().to_string()),
            ("USERPROFILE".into(), home.display().to_string()),
            ("ATELIER_HOME".into(), h(".atelier")),
            ("COPILOT_HOME".into(), h(".copilot")),
            ("XDG_DATA_HOME".into(), h(".local/share")),
            ("XDG_CONFIG_HOME".into(), h(".config")),
            ("CLAUDE_CONFIG_DIR".into(), h(".claude")),
            ("PATH".into(), path.to_string_lossy().into_owned()),
        ])
    }

    /// A command in the sandbox: its program (`atelier` is this program; an
    /// agent's name follows `ATELIER_<AGENT>` when it is set, as the
    /// self-tests' stand-in agents do), with the sandbox's environment.
    pub fn command(&self, id: &str, argv: &[String], cwd: Option<&str>, env: &[(String, String)]) -> Result<Command> {
        let program = argv.first().context("nothing to run")?;
        let program = match program.as_str() {
            "atelier" => std::env::current_exe()?.display().to_string(),
            name @ ("copilot" | "opencode" | "claude") => canvas_common::env::var(&format!("ATELIER_{}", name.to_ascii_uppercase())).unwrap_or_else(|_| name.to_owned()),
            other => other.to_owned(),
        };
        let mut cmd = Command::new(program);
        cmd.args(&argv[1..]);
        let sandbox = self.get(id)?;
        cmd.current_dir(cwd.filter(|c| Path::new(c).is_dir()).unwrap_or(&sandbox.cwd));
        // Nothing of the hub's own service or pack reaches the guest.
        for key in ["ATELIER_VM_PACK", "ATELIER_LISTEN", "ATELIER_INSTANCE", "ATELIER_SESSION", "ATELIER_PASSIVE", "ATELIER_REMOTE"] {
            cmd.env_remove(key);
            if let Some(old) = canvas_common::env::legacy_name(key) {
                cmd.env_remove(old);
            }
        }
        for (k, v) in self.env(id)?.into_iter().chain(env.iter().cloned()) {
            cmd.env(k, v);
        }
        Ok(cmd)
    }

    /// Runs a command in the sandbox to its end: (exit code, stdout).
    pub fn exec(&self, id: &str, argv: &[String], stdin: &[u8]) -> Result<(i64, Vec<u8>, String)> {
        use std::io::Write;
        let mut cmd = self.command(id, argv, None, &[])?;
        cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        canvas_sys::spawn::CommandExt::no_window(&mut cmd);
        let mut child = cmd.spawn().with_context(|| format!("cannot run {} in the sandbox", argv[0]))?;
        if let Some(mut input) = child.stdin.take() {
            let bytes = stdin.to_vec();
            std::thread::spawn(move || {
                let _ = input.write_all(&bytes);
            });
        }
        let out = child.wait_with_output()?;
        Ok((out.status.code().unwrap_or(-1) as i64, out.stdout, String::from_utf8_lossy(&out.stderr).into_owned()))
    }
}

/// `atelier vm-hook-standin HOOK`: answers one hook of the pack named by
/// `ATELIER_VM_PACK` as a real hook would, with `vm::local` sandboxes
/// (`local.root` of the pack). Reads the request on standard input, prints
/// the answer.
pub fn standin(hook: &str) -> Result<()> {
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let request: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let answer = match standin_answer(hook, &request) {
        Ok(answer) => answer,
        Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
    };
    println!("{answer}");
    Ok(())
}

fn standin_local() -> Result<Local> {
    let folder = canvas_common::env::var_os("ATELIER_VM_PACK").context("the stand-in needs ATELIER_VM_PACK")?;
    let (pack, _) = super::pack::load(Path::new(&folder));
    Ok(Local::new(pack.local_root.context("the stand-in needs local.root in vm.yaml")?))
}

fn standin_answer(hook: &str, request: &Value) -> Result<Value> {
    let local = standin_local()?;
    let s = |key: &str| request[key].as_str().unwrap_or_default().to_owned();
    let strings = |value: &Value| -> Vec<String> { value.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default() };
    Ok(match hook {
        "claim" => {
            let sandbox = local.claim(&s("box"), &s("repo"), &s("url"), &s("branch"), &s("base"))?;
            json!({ "ok": true, "sandbox": sandbox.id, "cwd": sandbox.cwd })
        }
        "shell-command" => {
            let id = s("sandbox");
            local.get(&id)?;
            let exe = std::env::current_exe()?.display().to_string();
            let mut argv = vec![exe, "vm-hook-standin".into(), "run".into(), "--sandbox".into(), id];
            if let Some(env) = request["env"].as_object() {
                for (k, v) in env {
                    argv.extend(["--env".into(), format!("{k}={}", v.as_str().unwrap_or_default())]);
                }
            }
            argv.push("--".into());
            argv.extend(strings(&request["argv"]));
            json!({ "ok": true, "argv": argv })
        }
        "exec" => {
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD;
            let stdin = b64.decode(request["stdin"].as_str().unwrap_or_default()).unwrap_or_default();
            let (code, stdout, stderr) = local.exec(&s("sandbox"), &strings(&request["argv"]), &stdin)?;
            json!({ "ok": true, "code": code, "stdout": b64.encode(stdout), "stderr": crate::vm::hooks::first_line(&stderr) })
        }
        "suspend" => {
            local.set_suspended(&s("sandbox"), true)?;
            json!({ "ok": true })
        }
        "resume" => {
            local.set_suspended(&s("sandbox"), false)?;
            json!({ "ok": true })
        }
        "delete" => {
            local.delete(&s("sandbox"))?;
            json!({ "ok": true })
        }
        "image" => json!({ "ok": true, "image": format!("local-{}", s("repo")) }),
        other => bail!("the stand-in has no {other} hook"),
    })
}

/// `atelier vm-hook-standin run --sandbox ID [--env K=V]… -- PROGRAM ARGS…`:
/// the "shell into the guest" the stand-in's `shell-command` answers: runs
/// the program in the sandbox with this terminal (exits with its code).
pub fn standin_run(id: &str, env: &[String], argv: &[String]) -> Result<()> {
    let local = standin_local()?;
    let env: Vec<(String, String)> = env.iter().filter_map(|e| e.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned()))).collect();
    let mut cmd = local.command(id, argv, None, &env)?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = cmd.exec();
        bail!("cannot start {}: {error}", argv[0]);
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().with_context(|| format!("cannot start {}", argv[0]))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandboxes_are_folders_with_a_home_of_their_own() {
        let root = std::env::temp_dir().join(format!("atelier-local-{}", uuid::Uuid::new_v4()));
        let local = Local::new(root.clone());
        let sandbox = local.claim("box-1", "contoso-app", "https://example.com/app.git", "atelier/x", "main").unwrap();
        assert!(valid_id(&sandbox.id) && Path::new(&sandbox.cwd).is_dir());
        assert_eq!(local.list(), vec![sandbox.clone()]);
        let env = local.env(&sandbox.id).unwrap();
        let home = root.join("sandboxes").join(&sandbox.id).join("home");
        for key in ["HOME", "USERPROFILE", "ATELIER_HOME", "COPILOT_HOME", "CLAUDE_CONFIG_DIR"] {
            let value = &env.iter().find(|(k, _)| k == key).unwrap().1;
            assert!(Path::new(value).starts_with(&home), "{key} = {value}");
        }
        assert!(local.claim("b", "../x", "", "b", "main").is_err(), "a repository key is one plain name");
        assert!(local.get("sbx-../../etc").is_err() && local.get("sbx-000000000000").is_err());
        local.set_suspended(&sandbox.id, true).unwrap();
        assert!(local.get(&sandbox.id).unwrap().suspended);
        local.delete(&sandbox.id).unwrap();
        assert!(local.list().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
