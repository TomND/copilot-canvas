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
