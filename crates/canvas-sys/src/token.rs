//! The per-start service token that local clients prove knowledge of, in a
//! file only this user can read.

#[cfg(windows)]
pub use canvas_common::auth::{read_token, write_token};

#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
mod unix {
    use std::path::Path;

    use anyhow::{Context, Result};
    use canvas_common::paths::hex;

    /// Publishes the service token atomically, readable by this user only (0600).
    pub fn write_token(path: &Path, token: &[u8]) -> Result<()> {
        crate::fs::write_private(path, hex(token).as_bytes()).context("cannot publish service token")
    }

    pub fn read_token(path: &Path) -> Result<Vec<u8>> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let text = text.trim();
        anyhow::ensure!(text.len() == 64 && text.chars().all(|c| c.is_ascii_hexdigit()), "malformed service token");
        Ok((0..32).map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).expect("validated hex")).collect())
    }
}
