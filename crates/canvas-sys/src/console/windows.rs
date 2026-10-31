//! Windows: console modes and a raw console reader (unchanged code from the CLI).

/// Standard input as the console delivers it: unlike `std::io::stdin`,
/// which ends a console's input at Ctrl+Z (0x1A), Ctrl+Z arrives as a key
/// (`KEY \x1a`), as it does in the agent CLIs. Text comes as UTF-8.
pub struct RawStdin(windows_sys::Win32::Foundation::HANDLE);

impl RawStdin {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        // SAFETY: the process's own standard input handle.
        Self(unsafe { windows_sys::Win32::System::Console::GetStdHandle(windows_sys::Win32::System::Console::STD_INPUT_HANDLE) })
    }

    /// The next input (empty at its end).
    pub fn read_some(&mut self) -> std::io::Result<Vec<u8>> {
        let mut wide = [0u16; 4096];
        let mut chars = 0u32;
        // SAFETY: `wide` is valid for its length for the whole (synchronous) call; no read control.
        let console = unsafe { windows_sys::Win32::System::Console::ReadConsoleW(self.0, wide.as_mut_ptr().cast(), wide.len() as u32, &mut chars, std::ptr::null()) };
        if console != 0 {
            return Ok(String::from_utf16_lossy(&wide[..chars as usize]).into_bytes());
        }
        // Not a console (a pipe): its bytes.
        let mut buf = vec![0u8; 8192];
        let mut read = 0u32;
        // SAFETY: as above.
        let ok = unsafe { windows_sys::Win32::Storage::FileSystem::ReadFile(self.0, buf.as_mut_ptr(), buf.len() as u32, &mut read, std::ptr::null_mut()) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        buf.truncate(read as usize);
        Ok(buf)
    }
}


/// Raw VT input (no line editing, echo or Ctrl+C handling), VT output.
pub fn raw_vt_modes() {
    use windows_sys::Win32::System::Console::{
        ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    // SAFETY: the process's own standard handles; failures leave the modes as they were.
    unsafe {
        SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), ENABLE_VIRTUAL_TERMINAL_INPUT);
        let output = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(output, &mut mode) != 0 {
            SetConsoleMode(output, mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}


/// Lets the console pass escape sequences through to the pseudoconsole.
pub fn vt_output() {
    use windows_sys::Win32::System::Console::{ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleMode};
    // SAFETY: the process's own standard output handle; failures leave the mode as it was.
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) != 0 {
            SetConsoleMode(handle, mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}
