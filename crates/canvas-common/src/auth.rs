//! Shared secrets: the per-start service token that local clients prove
//! knowledge of over the pipe (and over loopback TCP, from this user only).

use std::path::Path;

use anyhow::{Context, Result};

use crate::paths::hex;

/// Publishes the service token atomically, readable by this user only (a
/// protected DACL, whatever the folder's permissions are).
pub fn write_token(path: &Path, token: &[u8]) -> Result<()> {
    crate::win::write_private(path, hex(token).as_bytes()).context("cannot publish service token")
}

pub fn read_token(path: &Path) -> Result<Vec<u8>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let text = text.trim();
    anyhow::ensure!(text.len() == 64 && text.chars().all(|c| c.is_ascii_hexdigit()), "malformed service token");
    Ok((0..32).map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).expect("validated hex")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_written_privately_and_replaced_atomically() {
        let dir = std::env::temp_dir().join(format!("cc-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("service-x.token");
        write_token(&path, &[7; 32]).unwrap();
        write_token(&path, &[9; 32]).unwrap();
        assert_eq!(read_token(&path).unwrap(), vec![9; 32]);
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["service-x.token"], "no temporary file is left behind");
        // The DACL is protected and names only this user: icacls lists exactly one entry.
        let listing = std::process::Command::new("icacls").arg(&path).output().unwrap();
        let text = String::from_utf8_lossy(&listing.stdout);
        let entries = text.lines().filter(|l| l.contains(":(")).count();
        assert_eq!(entries, 1, "{text}");
        assert!(!text.contains("(I)"), "nothing is inherited from the folder: {text}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
