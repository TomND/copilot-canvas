//! A stand-in Copilot CLI for the GUI parity suite: `copilot-canvas.exe`
//! copied as `copilot.exe` (or `opencode.exe` for the suite's OpenCode
//! boxes) and run with `COPILOT_CANVAS_FAKE_COPILOT=1`
//! (both are required). It answers the configuration listings the canvas
//! reads (two MCP servers, no plugins or skills) and, started as a session,
//! draws a Copilot-like screen (your message, a reply, an input line) that
//! echoes keys until Ctrl+D or Ctrl+C. Nothing is sent anywhere.

use std::io::Write;

use anyhow::Result;

/// The environment variable that turns a `copilot.exe` copy into the fake.
pub const ENV: &str = "COPILOT_CANVAS_FAKE_COPILOT";
/// The MCP servers it lists.
pub const MCP_SERVERS: [&str; 2] = ["github", "playwright"];
/// The models its `--help` offers (`opencode models` lists them with a provider).
pub const MODELS: [&str; 3] = ["claude-opus-5.5", "gpt-parity-5", "claude-sonnet-parity"];
/// Your message as the fake shows it.
pub const USER_MESSAGE: &str = "Parity check: tint my message";

/// This process is the fake (its name is `copilot`, or `opencode` / `claude`
/// for the parity suite's other agents, and the variable is set).
pub fn active() -> bool {
    std::env::var_os(ENV).is_some()
        && std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| ["copilot", "opencode", "claude"].iter().any(|n| s.to_string_lossy().eq_ignore_ascii_case(n))))
            .unwrap_or(false)
}

pub fn run(args: &[String]) -> Result<()> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = match words.as_slice() {
        ["mcp", "list", ..] => format!("{{\"mcpServers\":{{\"{}\":{{\"command\":\"gh-mcp\"}},\"{}\":{{\"command\":\"npx\",\"args\":[\"@playwright/mcp\"]}}}}}}", MCP_SERVERS[0], MCP_SERVERS[1]),
        ["plugin" | "skill" | "instruction", "list", ..] => "[]".into(),
        ["config", "--list", ..] => "{\"model\":\"fake-model\",\"mouse\":true}".into(),
        ["config", ..] | ["plugin" | "skill" | "mcp", ..] => String::new(),
        ["--version"] => "0.0.0 (canvas parity fake)".into(),
        ["--help"] => format!(
            "Usage: copilot [options]\n\nOptions:\n  --model <model>  Set the AI model to use (choices: {})\n  --allow-all      Allow all tools\n",
            MODELS.iter().map(|m| format!("\"{m}\"")).collect::<Vec<_>>().join(", ")
        ),
        ["models", ..] => MODELS.iter().map(|m| format!("github-copilot/{m}")).collect::<Vec<_>>().join("\n"),
        _ => return session(args),
    };
    println!("{out}");
    Ok(())
}

/// The model a session was started with (`--model` or `-m`), as the fake shows it.
pub fn model_line(args: &[String]) -> String {
    let model = args.windows(2).find(|w| w[0] == "--model" || w[0] == "-m").map(|w| w[1].as_str());
    format!("Model: {}", model.unwrap_or("none"))
}

/// A Copilot-like screen in mouse mode that echoes what you type and
/// reports mouse clicks (`MOUSE <report>`), reading raw VT input as Copilot does.
fn session(args: &[String]) -> Result<()> {
    use std::io::Read;
    console_modes();
    let mut out = std::io::stdout();
    let disabled: Vec<&str> = args.windows(2).filter(|w| w[0] == "--disable-mcp-server").map(|w| w[1].as_str()).collect();
    // Mouse reports (SGR), as Copilot asks for them.
    write!(out, "\x1b[?1000h\x1b[?1006h")?;
    write!(out, "Fake Copilot (canvas parity)\r\n")?;
    write!(out, "MCP off: {} · {}\r\n\r\n", if disabled.is_empty() { "none".to_owned() } else { disabled.join(",") }, model_line(args))?;
    write!(out, " ❯ {USER_MESSAGE}\r\n   and its second line\r\n\r\n")?;
    write!(out, " ● Fake reply: nothing was sent anywhere.\r\n\r\n")?;
    write!(out, "❯ ")?;
    out.flush()?;
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 4096];
    loop {
        let n = stdin.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        if text.contains('\u{4}') || text.contains('\u{3}') {
            break;
        }
        if let Some(at) = text.find("\x1b[<") {
            let report: String = text[at + 3..].chars().take_while(|c| c.is_ascii_digit() || *c == ';' || *c == 'M' || *c == 'm').collect();
            write!(out, "\r\nMOUSE {report}\r\n❯ ")?;
        } else if !text.starts_with('\x1b') {
            write!(out, "{}", text.replace('\r', "\r\n❯ "))?;
        }
        out.flush()?;
    }
    write!(out, "\x1b[?1000l\x1b[?1006l")?;
    out.flush()?;
    Ok(())
}

/// Raw VT input (keys and mouse reports as escape sequences), VT output.
fn console_modes() {
    use windows_sys::Win32::System::Console::{
        ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    // SAFETY: the process's own standard handles.
    unsafe {
        SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), ENABLE_VIRTUAL_TERMINAL_INPUT);
        let output = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(output, &mut mode) != 0 {
            SetConsoleMode(output, mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}
