//! Unix: termios raw mode on the terminal (no line editing, echo or signal
//! keys: Ctrl+C, Ctrl+Z and Ctrl+D arrive as bytes, as in the agent CLIs).

/// Raw input on standard input when it is a terminal; output is VT already.
pub fn raw_vt_modes() {
    // SAFETY: termios calls on the process's own standard input.
    unsafe {
        if libc::isatty(0) == 0 {
            return;
        }
        let mut mode: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut mode) == 0 {
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
