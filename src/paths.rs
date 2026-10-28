//! Locations, instance naming and build identity shared by client and service.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct Instance {
    pub name: String,
    pub home: PathBuf,
    pub sessions_dir: PathBuf,
}

impl Instance {
    pub fn new(name: Option<String>, home: Option<PathBuf>, sessions_dir: Option<PathBuf>) -> Result<Self> {
        let name = name.unwrap_or_else(|| "default".into());
        anyhow::ensure!(
            !name.is_empty() && name.len() <= 40 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "instance name must be 1-40 characters of [A-Za-z0-9_-]"
        );
        let home = match home.or_else(|| std::env::var_os("COPILOT_CANVAS_HOME").map(PathBuf::from)) {
            Some(path) => path,
            None => dirs::home_dir().context("cannot determine the home directory")?.join(".copilot-canvas"),
        };
        let sessions_dir = match sessions_dir {
            Some(path) => path,
            None => dirs::home_dir().context("cannot determine the home directory")?.join(".copilot").join("session-state"),
        };
        std::fs::create_dir_all(&home).with_context(|| format!("cannot create {}", home.display()))?;
        Ok(Self { name, home: absolute(&home), sessions_dir: absolute(&sessions_dir) })
    }

    pub fn pipe_name(&self) -> String {
        let user: String = std::env::var("USERNAME")
            .unwrap_or_else(|_| "user".into())
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(32)
            .collect();
        // Different data homes must never share a service.
        let mut hash = Sha256::new();
        hash.update(self.home.to_string_lossy().to_lowercase().as_bytes());
        let digest = hash.finalize();
        format!(r"\\.\pipe\copilot-canvas-{user}-{}-{}", self.name, hex(&digest[..6]))
    }

    pub fn token_path(&self) -> PathBuf {
        self.home.join(format!("service-{}.token", self.name))
    }

    pub fn log_path(&self) -> PathBuf {
        self.home.join(format!("service-{}.log", self.name))
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.home.join("bin")
    }
}

pub fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Content hash of the running executable; identifies compatible client/service pairs.
pub fn build_id() -> &'static str {
    static BUILD: OnceLock<String> = OnceLock::new();
    BUILD.get_or_init(|| {
        let digest = std::env::current_exe()
            .ok()
            .and_then(|exe| std::fs::read(exe).ok())
            .map(|bytes| Sha256::digest(&bytes).to_vec())
            .unwrap_or_default();
        format!("{}-{}", env!("CARGO_PKG_VERSION"), hex(&digest[..digest.len().min(8)]))
    })
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parses the RFC 3339 timestamps Copilot writes (e.g. `2026-10-20T20:00:00.123Z`).
/// UTC time as `2026-10-22T21:43:10.216Z` (the format Copilot writes).
pub fn rfc3339_now() -> String {
    format_rfc3339_ms(now_ms())
}

pub fn format_rfc3339_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, rem % 3600 / 60, rem % 60, ms % 1000)
}

pub fn parse_rfc3339_ms(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> { text.get(range)?.parse().ok() };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut index = 19;
    let mut millis = 0i64;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        let digits = &text[start..index];
        let padded: String = digits.chars().chain("000".chars()).take(3).collect();
        millis = padded.parse().ok()?;
    }
    let offset_minutes = match bytes.get(index)? {
        b'Z' | b'z' => 0,
        sign @ (b'+' | b'-') => {
            let hours: i64 = text.get(index + 1..index + 3)?.parse().ok()?;
            let minutes: i64 = text.get(index + 4..index + 6)?.parse().ok()?;
            let total = hours * 60 + minutes;
            if *sign == b'+' { total } else { -total }
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60;
    let total = seconds * 1000 + millis;
    (total >= 0).then_some(total as u64)
}

pub fn format_age(ms: u64, now: u64) -> String {
    let seconds = now.saturating_sub(ms) / 1000;
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_copilot_timestamps() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("2026-10-20T20:00:00.123Z"), Some(1_792_526_400_123));
        assert_eq!(parse_rfc3339_ms("2026-10-20T13:00:00-07:00"), Some(1_792_526_400_000));
        assert_eq!(parse_rfc3339_ms("2026-10-20T20:00:00.5Z"), Some(1_792_526_400_500));
        assert_eq!(parse_rfc3339_ms("not a time"), None);
        assert_eq!(parse_rfc3339_ms("2026-13-20T20:00:00Z"), None);
    }

    #[test]
    fn instance_names_are_validated_and_isolated() {
        let temp = std::env::temp_dir().join(format!("cc-paths-{}", uuid::Uuid::new_v4()));
        let a = Instance::new(Some("t1".into()), Some(temp.join("a")), Some(temp.clone())).unwrap();
        let b = Instance::new(Some("t1".into()), Some(temp.join("b")), Some(temp.clone())).unwrap();
        assert_ne!(a.pipe_name(), b.pipe_name());
        assert!(Instance::new(Some("bad name".into()), Some(temp.join("c")), None).is_err());
        std::fs::remove_dir_all(temp).unwrap();
    }
}
