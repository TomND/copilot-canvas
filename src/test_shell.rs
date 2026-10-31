//! Commands the self-tests type into demo shells (`canvas_model::doc::demo_shell`):
//! PowerShell on Windows, zsh (macOS) or bash (Linux) elsewhere. Every
//! command prints its marker from two parts, so the typed command line never
//! contains the text a check waits for.

/// Prints `a` and `b` joined.
pub fn print(a: &str, b: &str) -> String {
    if cfg!(windows) { format!("Write-Output ('{a}' + '{b}')\r") } else { format!("echo '{a}''{b}'\r") }
}

/// Clears the screen, then prints `a` and `b` joined.
pub fn clear_then_print(a: &str, b: &str) -> String {
    if cfg!(windows) { format!("Clear-Host; Write-Output ('{a}' + '{b}')\r") } else { format!("clear; echo '{a}''{b}'\r") }
}

/// Prints `prefix1` … `prefix<count>`, one per line.
pub fn print_numbered(prefix: &str, count: u32) -> String {
    if cfg!(windows) { format!("1..{count} | % {{ \"{prefix}$_\" }}\r") } else { format!("for i in $(seq 1 {count}); do echo {prefix}$i; done\r") }
}

/// Clears the screen.
pub fn clear() -> &'static str {
    if cfg!(windows) { "Clear-Host\r" } else { "clear\r" }
}

/// Writes `sequence` (escape sequences written `\e` for ESC and `\a` for BEL)
/// to the terminal without a line break.
pub fn emit(sequence: &str) -> String {
    if cfg!(windows) {
        let body = sequence.replace("\\e", "$e").replace("\\a", "$b");
        format!("$e=[char]27; $b=[char]7; Write-Host -NoNewline \"{body}\"\r")
    } else {
        format!("printf '{}'\r", sequence.replace("\\e", "\\033").replace("\\a", "\\007"))
    }
}

/// Runs this program with `args` (a path that may contain spaces).
pub fn run_self(exe: &std::path::Path, args: &str) -> String {
    if cfg!(windows) { format!("& '{}' {args}\r", exe.display()) } else { format!("'{}' {args}\r", exe.display()) }
}

/// Discards the shell's input line (PSReadLine: Esc; zsh and bash: Ctrl+C,
/// which runs nothing; sent twice, as the first one copies a terminal
/// selection still showing from a copy check, as in Windows Terminal).
pub fn discard_input() -> &'static [u8] {
    if cfg!(windows) { b"\x1b" } else { b"\x03\x03" }
}

/// Discards the shell's input line through `send`: PSReadLine's Esc alone; on
/// Unix the two interrupts of [`discard_input`] one at a time, each given time
/// to land (a second one that arrives while the shell redraws after the first
/// is held until the next key typed, which it then discards with its line).
pub fn discard(mut send: impl FnMut(&[u8]) -> anyhow::Result<()>) -> anyhow::Result<()> {
    if cfg!(windows) {
        return send(discard_input());
    }
    for interrupt in discard_input().chunks(1) {
        send(interrupt)?;
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    Ok(())
}

/// An absolute path that does not exist, for link checks.
pub fn missing_path() -> &'static str {
    if cfg!(windows) { r"C:\canvas-link-probe\missing.txt" } else { "/canvas-link-probe/missing.txt" }
}

/// The clipboard's line break for copied text.
pub fn newline() -> &'static str {
    if cfg!(windows) { "\r\n" } else { "\n" }
}

#[cfg(test)]
mod tests {
    #[test]
    fn typed_commands_never_contain_their_marker() {
        for command in [super::print("CANVAS_", "TERMINAL_OK"), super::clear_then_print("CANVAS_", "TERMINAL_OK")] {
            assert!(!command.contains("CANVAS_TERMINAL_OK"), "{command}");
        }
        assert!(super::emit("\\e]0;T\\a").ends_with('\r'));
    }
}
