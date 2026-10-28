//! Shared secrets: the per-start service token that local clients prove
//! knowledge of over the pipe.

use std::path::Path;

use anyhow::{Context, Result};

use crate::paths::hex;

/// Publishes the service token atomically (owner-only home directory).
pub fn write_token(path: &Path, token: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temp, hex(token))?;
    std::fs::rename(&temp, path).context("cannot publish service token")?;
    Ok(())
}

pub fn read_token(path: &Path) -> Result<Vec<u8>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let text = text.trim();
    anyhow::ensure!(text.len() == 64 && text.chars().all(|c| c.is_ascii_hexdigit()), "malformed service token");
    Ok((0..32).map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).expect("validated hex")).collect())
}
