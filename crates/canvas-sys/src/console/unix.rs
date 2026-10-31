//! Unix: termios raw mode on the terminal (no line editing, echo or signal
//! keys: Ctrl+C, Ctrl+Z and Ctrl+D arrive as bytes, as in the agent CLIs).

/// The terminal's modes before [`raw_vt_modes`], put back at exit.
static SAVED: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();

extern "C" fn restore_modes() {
    if let Some(mode) = SAVED.get() {
        // SAFETY: the modes read from this terminal at start; TCSADRAIN lets
        // the last output go out in the mode it was written in.
        unsafe { libc::tcsetattr(0, libc::TCSADRAIN, mode) };
    }
}

/// Raw input on standard input when it is a terminal; output is VT already.
/// The terminal's modes come back when the process exits: bash keeps the
/// modes a finished command leaves (PSReadLine sets the console's again at
/// every prompt, zsh resets its own), and with the paste probe's raw modes
/// left behind bash echoed nothing and a pasted path never reached it
/// (parity rows G3 and 33 on Linux).
pub fn raw_vt_modes() {
    // SAFETY: termios calls on the process's own standard input; `atexit`
    // gets a plain function that only reads a static.
    unsafe {
        if libc::isatty(0) == 0 {
            return;
        }
        let mut mode: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut mode) == 0 {
            if SAVED.set(mode).is_ok() {
                libc::atexit(restore_modes);
            }
            libc::cfmakeraw(&mut mode);
            // Keep output processing (\n → \r\n), as a console's VT output does.
            mode.c_oflag |= libc::OPOST | libc::ONLCR;
            libc::tcsetattr(0, libc::TCSANOW, &mode);
        }
    }
}

/// Terminals take escape sequences as they are.
pub fn vt_output() {}

/// Standard input, every byte as it comes.
pub struct RawStdin;

impl RawStdin {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self
    }

    /// The next input (empty at its end).
    pub fn read_some(&mut self) -> std::io::Result<Vec<u8>> {
        let mut buf = vec![0u8; 8192];
        loop {
            // SAFETY: `buf` is valid for its length.
            let read = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if read >= 0 {
                buf.truncate(read as usize);
                return Ok(buf);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// A process that turned raw modes on leaves its terminal as it found it.
    #[test]
    fn raw_modes_come_back_at_exit() {
        use std::os::fd::FromRawFd;
        if std::env::var_os("CANVAS_SYS_RAW_CHILD").is_some() {
            super::raw_vt_modes();
            return;
        }
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty fills two descriptors it opened; tcgetattr reads the
        // slave's modes into a zeroed struct.
        let modes = |fd| unsafe {
            let mut mode: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(fd, &mut mode), 0);
            // PENDIN is the kernel's own (set when raw input goes canonical again).
            (mode.c_iflag, mode.c_oflag, mode.c_lflag & !libc::PENDIN)
        };
        // SAFETY: as above; null name, termios and window size are allowed.
        assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) }, 0);
        let before = modes(slave);
        // SAFETY: a duplicate of the slave, owned by the child's stdin from here.
        let stdin = unsafe { std::process::Stdio::from(std::os::fd::OwnedFd::from_raw_fd(libc::dup(slave))) };
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "console::unix::tests::raw_modes_come_back_at_exit", "--test-threads=1", "--quiet"])
            .env("CANVAS_SYS_RAW_CHILD", "1")
            .stdin(stdin)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("the child test");
        assert!(status.success());
        assert_eq!(modes(slave), before, "modes restored after the child's raw mode");
        // SAFETY: closing the two descriptors openpty opened.
        unsafe {
            libc::close(slave);
            libc::close(master);
        }
    }
}
