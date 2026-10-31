//! Linux: the session's clipboard through `wl-clipboard` (Wayland) or
//! `xclip` / `xsel` (X11). Without a display (a server, SSH) there is no
//! clipboard: the TUI uses OSC 52 and the service uploads images instead
//! (MAC-LINUX-PLAN §5.8, §5.9).

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use canvas_common::clipimage;

fn wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
}

fn x11() -> bool {
    std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
}

/// The tool commands that read the clipboard (as `mime`, or text).
fn readers(mime: Option<&str>) -> Vec<Vec<String>> {
    let mut found = Vec::new();
    if wayland() {
        let mut args = vec!["wl-paste".to_owned(), "--no-newline".to_owned()];
        if let Some(mime) = mime {
            args.extend(["--type".to_owned(), mime.to_owned()]);
        }
        found.push(args);
    }
    if x11() {
        found.push(["xclip", "-selection", "clipboard", "-o", "-t", mime.unwrap_or("UTF8_STRING")].map(str::to_owned).to_vec());
        if mime.is_none() {
            found.push(["xsel", "--clipboard", "--output"].map(str::to_owned).to_vec());
        }
    }
    found
}

fn read(mime: Option<&str>) -> Result<Vec<u8>> {
    for args in readers(mime) {
        if let Ok(out) = Command::new(&args[0]).args(&args[1..]).stdin(Stdio::null()).stderr(Stdio::null()).output()
            && out.status.success()
        {
            return Ok(out.stdout);
        }
    }
    bail!("no clipboard is available (install wl-clipboard or xclip)")
}

fn write(mime: &str, bytes: &[u8]) -> Result<()> {
    let mut tools: Vec<Vec<String>> = Vec::new();
    if wayland() {
        tools.push(["wl-copy", "--type", mime].map(str::to_owned).to_vec());
    }
    if x11() {
        tools.push(["xclip", "-selection", "clipboard", "-i", "-t", mime].map(str::to_owned).to_vec());
    }
    for args in tools {
        let Ok(mut child) = Command::new(&args[0]).args(&args[1..]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { continue };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(bytes).context("cannot write to the clipboard tool")?;
        }
        if child.wait().is_ok_and(|s| s.success()) {
            return Ok(());
        }
    }
    bail!("no clipboard is available (install wl-clipboard or xclip)")
}

fn offered() -> Vec<String> {
    let lists: Vec<Vec<String>> = [
        wayland().then(|| ["wl-paste", "--list-types"].map(str::to_owned).to_vec()),
        x11().then(|| ["xclip", "-selection", "clipboard", "-o", "-t", "TARGETS"].map(str::to_owned).to_vec()),
    ]
    .into_iter()
    .flatten()
    .collect();
    for args in lists {
        if let Ok(out) = Command::new(&args[0]).args(&args[1..]).stdin(Stdio::null()).stderr(Stdio::null()).output()
            && out.status.success()
        {
            return String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().to_owned()).collect();
        }
    }
    Vec::new()
}

pub fn set_text(text: &str) -> Result<()> {
    write("text/plain;charset=utf-8", text.as_bytes())
}

pub fn has_text() -> bool {
    offered().iter().any(|t| t.starts_with("text/plain") || t == "UTF8_STRING" || t == "STRING")
}

pub fn has_image() -> bool {
    offered().iter().any(|t| t == "image/png")
}

pub fn get_text() -> Result<String> {
    let bytes = read(None)?;
    ensure!(!bytes.is_empty(), "the clipboard has no text");
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn get_image_png() -> Result<Vec<u8>> {
    let png = read(Some("image/png"))?;
    ensure!(png.starts_with(b"\x89PNG"), "the clipboard has no image");
    ensure!(png.len() <= clipimage::MAX_PNG, "the image is larger than {} MB", clipimage::MAX_PNG >> 20);
    Ok(png)
}

pub fn set_image_png(png: &[u8]) -> Result<()> {
    ensure!(png.len() <= clipimage::MAX_PNG, "the image is larger than {} MB", clipimage::MAX_PNG >> 20);
    write("image/png", png)
}
