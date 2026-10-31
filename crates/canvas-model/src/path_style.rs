//! Path semantics of the machine that runs the service (MAC-LINUX-PLAN
//! D10): link detection, the folder browser, remote paths, VS Code tunnel
//! URIs, drop quoting and upload names follow the *service's* style, so a
//! Mac client of a Windows service and a Windows client of a Linux service
//! both handle paths right.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PathStyle {
    /// `C:\dir\file`, `\\server\share`; case-insensitive.
    #[default]
    Windows,
    /// `/dir/file` (macOS, Linux); case-sensitive.
    Posix,
}

impl PathStyle {
    /// The style of a service platform (`BackendInfo::platform`: `windows`,
    /// `macos`, `linux`; empty from services older than the field: Windows).
    pub fn of_platform(platform: &str) -> Self {
        match platform {
            "" | "windows" => PathStyle::Windows,
            _ => PathStyle::Posix,
        }
    }

    /// This machine's style.
    pub fn local() -> Self {
        if cfg!(windows) { PathStyle::Windows } else { PathStyle::Posix }
    }

    pub fn separator(self) -> char {
        match self {
            PathStyle::Windows => '\\',
            PathStyle::Posix => '/',
        }
    }

    /// An absolute path of this style: a drive (`C:\`, `C:/`) or UNC path on
    /// Windows; `/…` on POSIX.
    pub fn is_absolute(self, path: &str) -> bool {
        match self {
            PathStyle::Windows => {
                let bytes = path.as_bytes();
                (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/')) || path.starts_with(r"\\")
            }
            PathStyle::Posix => path.starts_with('/'),
        }
    }

    /// `base` joined with `part` with this style's separator.
    pub fn join(self, base: &str, part: &str) -> String {
        let sep = self.separator();
        let trimmed = base.trim_end_matches(['/', sep]);
        if base.is_empty() {
            return part.to_owned();
        }
        if trimmed.is_empty() {
            return format!("{sep}{part}");
        }
        format!("{trimmed}{sep}{part}")
    }

    /// The path of a decoded `file://` URL's rest (after `file://`):
    /// `/C:/x` → `C:\x`, `server/share` → `\\server\share` (Windows);
    /// `/x/y` → `/x/y` (POSIX).
    pub fn from_file_url(self, rest: &str) -> String {
        match self {
            PathStyle::Windows => {
                let local = rest.strip_prefix('/').filter(|p| p.as_bytes().get(1) == Some(&b':')).map(str::to_owned).unwrap_or_else(|| format!("//{rest}"));
                local.replace('/', "\\")
            }
            PathStyle::Posix => {
                // `file://localhost/x` and `file:///x` both mean `/x`.
                let rest = rest.strip_prefix("localhost").unwrap_or(rest);
                if rest.starts_with('/') { rest.to_owned() } else { format!("/{rest}") }
            }
        }
    }

    /// Whether two paths name the same file (Windows compares without case
    /// and with either slash).
    pub fn same(self, a: &str, b: &str) -> bool {
        match self {
            PathStyle::Windows => a.replace('/', "\\").eq_ignore_ascii_case(&b.replace('/', "\\")),
            PathStyle::Posix => a == b,
        }
    }

    /// A path in this style with forward slashes, as VS Code URIs want it.
    pub fn to_uri_path(self, path: &str) -> String {
        match self {
            PathStyle::Windows => path.replace('\\', "/"),
            PathStyle::Posix => path.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PathStyle::{Posix, Windows};
    use super::*;

    #[test]
    fn both_styles_know_absolute_paths_joins_and_file_urls() {
        assert!(Windows.is_absolute(r"C:\x") && Windows.is_absolute("c:/x") && Windows.is_absolute(r"\\srv\share"));
        assert!(!Windows.is_absolute("/x") && !Windows.is_absolute(r"rel\x"));
        assert!(Posix.is_absolute("/Users/me") && !Posix.is_absolute(r"C:\x") && !Posix.is_absolute("rel/x"));
        assert_eq!(Windows.join(r"C:\h", "canvas-agents"), r"C:\h\canvas-agents");
        assert_eq!(Windows.join(r"C:\", "x"), r"C:\x");
        assert_eq!(Posix.join("/home/me/", "x"), "/home/me/x");
        assert_eq!(Posix.join("/", "x"), "/x");
        assert_eq!(Windows.from_file_url("/C:/a b/x.rs"), r"C:\a b\x.rs");
        assert_eq!(Windows.from_file_url("srv/share/x"), r"\\srv\share\x");
        assert_eq!(Posix.from_file_url("/Users/me/x.rs"), "/Users/me/x.rs");
        assert_eq!(Posix.from_file_url("localhost/tmp/x"), "/tmp/x");
        assert!(Windows.same(r"C:\A\b", "c:/a/B") && !Posix.same("/a/B", "/a/b"));
        assert_eq!(PathStyle::of_platform(""), Windows);
        assert_eq!(PathStyle::of_platform("macos"), Posix);
        assert_eq!(PathStyle::of_platform("linux"), Posix);
    }
}
