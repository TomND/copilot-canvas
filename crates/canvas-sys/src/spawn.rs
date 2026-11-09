//! How child processes start: without a console window, detached from this
//! process's console or session, in a new process group. The flag values are
//! Windows' process creation flags; on Unix `DETACHED` and `NEW_CONSOLE` start
//! a new session (`setsid`), `NEW_PROCESS_GROUP` a new process group, and the
//! window and job flags mean nothing.

use std::process::Command;

/// No console window for a console program (`CREATE_NO_WINDOW`).
pub const NO_WINDOW: u32 = 0x0800_0000;
/// No console at all (`DETACHED_PROCESS`); Unix: a new session.
pub const DETACHED: u32 = 0x0000_0008;
/// A new process group (`CREATE_NEW_PROCESS_GROUP`).
pub const NEW_PROCESS_GROUP: u32 = 0x0000_0200;
/// Out of this process's job (`CREATE_BREAKAWAY_FROM_JOB`).
pub const BREAKAWAY: u32 = 0x0100_0000;
/// A console window of its own (`CREATE_NEW_CONSOLE`); Unix: a new session.
pub const NEW_CONSOLE: u32 = 0x0000_0010;

pub trait CommandExt {
    /// Sets how the process starts (the constants above, or-ed).
    fn spawn_flags(&mut self, flags: u32) -> &mut Self;

    /// A console program without a window (no-op on Unix).
    fn no_window(&mut self) -> &mut Self {
        self.spawn_flags(NO_WINDOW)
    }

    /// Linux: the child gets `SIGTERM` when this process dies
    /// (`PR_SET_PDEATHSIG`); start it with [`spawn_tied`], since the signal
    /// follows the *thread* that forked it. Windows uses jobs and macOS the
    /// service's own sweep instead (no-op there).
    fn die_with_parent(&mut self) -> &mut Self;
}

/// Starts `command` (set up with [`CommandExt::die_with_parent`]) so it is
/// tied to this process, not to the calling thread: on Linux the parent-death
/// signal fires when the forking thread ends, so every tied child is forked
/// by one thread that lives as long as the process. Elsewhere a plain spawn.
pub fn spawn_tied(command: Command) -> std::io::Result<std::process::Child> {
    #[cfg(target_os = "linux")]
    {
        use std::sync::mpsc::{Sender, channel};
        use std::sync::{Mutex, OnceLock};
        type Job = (Command, Sender<std::io::Result<std::process::Child>>);
        static SPAWNER: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
        let spawner = SPAWNER.get_or_init(|| {
            let (tx, rx) = channel::<Job>();
            let _ = std::thread::Builder::new().name("tied-spawner".into()).spawn(move || {
                for (mut command, reply) in rx {
                    let _ = reply.send(command.spawn());
                }
            });
            Mutex::new(tx)
        });
        let (reply, answer) = channel();
        let sent = spawner.lock().unwrap_or_else(|e| e.into_inner()).send((command, reply));
        if let Err(std::sync::mpsc::SendError((mut command, _))) = sent {
            return command.spawn();
        }
        answer.recv().unwrap_or_else(|_| Err(std::io::Error::other("the spawner thread ended")))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut command = command;
        command.spawn()
    }
}

#[cfg(windows)]
impl CommandExt for Command {
    fn spawn_flags(&mut self, flags: u32) -> &mut Self {
        std::os::windows::process::CommandExt::creation_flags(self, flags)
    }

    fn die_with_parent(&mut self) -> &mut Self {
        self
    }
}

#[cfg(unix)]
impl CommandExt for Command {
    fn spawn_flags(&mut self, flags: u32) -> &mut Self {
        use std::os::unix::process::CommandExt as _;
        if flags & (DETACHED | NEW_CONSOLE) != 0 {
            // SAFETY: setsid is async-signal-safe.
            unsafe {
                self.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
            }
        } else if flags & NEW_PROCESS_GROUP != 0 {
            self.process_group(0)
        } else {
            self
        }
    }

    fn die_with_parent(&mut self) -> &mut Self {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt as _;
            // SAFETY: prctl is async-signal-safe.
            unsafe {
                self.pre_exec(|| {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        self
    }
}

/// Starts `program args` as a transient systemd user service named `unit`
/// when a user manager runs (Linux; MAC-LINUX-PLAN D5): the service then has
/// its own cgroup, out of the scope of the terminal or desktop session that
/// started it, and systemd ends what it leaves behind when it stops. Its
/// environment is this process's plus `env`, handed over in a 0600 file in
/// `private_dir` (removed again once the unit started). Answers `Ok(false)`
/// where this does not apply (no user manager, no `systemd-run`, another OS,
/// `ATELIER_SYSTEMD_RUN=0`); the caller then starts it itself.
pub fn start_user_unit(unit: &str, program: &std::path::Path, args: &[std::ffi::OsString], cwd: &std::path::Path, env: &[(&str, &str)], private_dir: &std::path::Path) -> std::io::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        linux_unit::start(unit, program, args, cwd, env, private_dir)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (unit, program, args, cwd, env, private_dir);
        Ok(false)
    }
}

#[cfg(target_os = "linux")]
mod linux_unit {
    use std::io::Write;
    use std::path::Path;
    use std::process::{Command, Stdio};

    /// A user manager answers on its private socket and `systemd-run` exists.
    fn available() -> bool {
        if canvas_common::env::var("ATELIER_SYSTEMD_RUN").is_ok_and(|v| v.trim() == "0") {
            return false;
        }
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
        let manager = runtime.is_some_and(|dir| dir.join("systemd").join("private").exists());
        let tool = std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("systemd-run").is_file()));
        manager && tool
    }

    /// `KEY="value"` for an `EnvironmentFile=` (values with line breaks or
    /// NULs cannot be written and are left out).
    fn line(key: &str, value: &str) -> Option<String> {
        let key_ok = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !key.starts_with(|c: char| c.is_ascii_digit());
        if !key_ok || value.contains(['\n', '\r', '\0']) {
            return None;
        }
        Some(format!("{key}=\"{}\"\n", value.replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$").replace('`', "\\`")))
    }

    pub fn start(unit: &str, program: &Path, args: &[std::ffi::OsString], cwd: &Path, env: &[(&str, &str)], private_dir: &Path) -> std::io::Result<bool> {
        if !available() {
            return Ok(false);
        }
        crate::fs::private_dir(private_dir)?;
        let file = private_dir.join(format!("{unit}.env"));
        let _ = std::fs::remove_file(&file);
        {
            let mut out = crate::fs::create_private(&file)?;
            let mut vars: std::collections::BTreeMap<String, String> = std::env::vars().collect();
            for (key, value) in env {
                vars.insert((*key).to_owned(), (*value).to_owned());
            }
            for (key, value) in &vars {
                if let Some(line) = line(key, value) {
                    out.write_all(line.as_bytes())?;
                }
            }
        }
        let status = Command::new("systemd-run")
            .args(["--user", "--collect", "--quiet", "--no-block"])
            .arg(format!("--unit={unit}"))
            .arg(format!("--working-directory={}", cwd.display()))
            .arg(format!("--property=EnvironmentFile={}", file.display()))
            .arg("--property=KillMode=control-group")
            .arg("--")
            .arg(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output();
        // The manager reads the file when the unit starts: removed a little later.
        let cleanup = file.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(20));
            let _ = std::fs::remove_file(cleanup);
        });
        match status {
            Ok(out) if out.status.success() => Ok(true),
            // A unit of this name still exists (an old service stopping): the caller's own start.
            _ => {
                let _ = std::fs::remove_file(&file);
                Ok(false)
            }
        }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn environment_lines_are_quoted_for_systemd() {
            assert_eq!(super::line("PATH", "/a b:/c").as_deref(), Some("PATH=\"/a b:/c\"\n"));
            assert_eq!(super::line("X", r#"say "hi" $HOME \n `x`"#).as_deref(), Some("X=\"say \\\"hi\\\" \\$HOME \\\\n \\`x\\`\"\n"));
            assert_eq!(super::line("MULTI", "a\nb"), None);
            assert_eq!(super::line("BASH_FUNC_x%%", "() {}"), None);
            assert_eq!(super::line("1BAD", "x"), None);
        }
    }
}
