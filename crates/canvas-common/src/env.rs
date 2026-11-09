// rename:keep-begin
//! Atelier's environment variables. Every one is named `ATELIER_<X>`; before
//! the rename it was `COPILOT_CANVAS_<X>` (deprecated). Reads go through
// rename:keep-end
//! [`var`] and [`var_os`], which fall back to the old name when the new one
//! is not set, so old scripts, scheduled tasks and the sessions an older
//! service started keep working. The variables the service gives agent
//! sessions are set under both names ([`both`]).

use std::ffi::OsString;

use crate::legacy;

/// The prefix of every Atelier variable.
pub const PREFIX: &str = "ATELIER_";

/// The deprecated name of an Atelier variable (`ATELIER_HOME` → the same
/// after [`legacy::ENV_PREFIX`]); `None` for other names.
pub fn legacy_name(name: &str) -> Option<String> {
    name.strip_prefix(PREFIX).map(|rest| format!("{}{rest}", legacy::ENV_PREFIX))
}

/// `name` (`ATELIER_X`), else its deprecated name.
pub fn var_os(name: &str) -> Option<OsString> {
    std::env::var_os(name).or_else(|| legacy_name(name).and_then(std::env::var_os))
}

/// `name` (`ATELIER_X`) as text, else its deprecated name.
pub fn var(name: &str) -> Result<String, std::env::VarError> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => match legacy_name(name) {
            Some(old) => std::env::var(old),
            None => Err(std::env::VarError::NotPresent),
        },
        other => other,
    }
}

/// `(name, value)` under the new and the deprecated name, for the
/// environment the service gives sessions (programs of the previous build,
/// and instructions written for it, read the old name).
pub fn both(name: &str, value: &str) -> Vec<(String, String)> {
    let mut out = vec![(name.to_owned(), value.to_owned())];
    out.extend(legacy_name(name).map(|old| (old, value.to_owned())));
    out
}

/// Whether `key` names the variable `name` (`ATELIER_X`) under either name,
/// in any case (Windows reads names without case).
pub fn is(key: &str, name: &str) -> bool {
    key.eq_ignore_ascii_case(name) || legacy_name(name).is_some_and(|old| key.eq_ignore_ascii_case(&old))
}

/// Whether `key` is one of Atelier's variables, under either prefix.
pub fn is_ours(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.starts_with(PREFIX) || upper.starts_with(legacy::ENV_PREFIX)
}

/// The deprecated variables set in this process ([`legacy::ENV_PREFIX`]), for
/// `doctor` and for sandboxes that must not inherit them.
pub fn legacy_vars() -> Vec<String> {
    std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()).filter(|k| k.to_ascii_uppercase().starts_with(legacy::ENV_PREFIX)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_names_win_and_old_names_are_read_when_new_ones_are_missing() {
        let unique = format!("ENVTEST_{}", std::process::id());
        let (new, old) = (format!("ATELIER_{unique}"), format!("COPILOT_CANVAS_{unique}")); // rename:keep
        assert_eq!(legacy_name(&new).as_deref(), Some(old.as_str()));
        assert_eq!(legacy_name("PATH"), None, "only Atelier's variables have an old name");
        // SAFETY: names no other test reads.
        unsafe { std::env::set_var(&old, "from the old name") };
        assert_eq!(var(&new).as_deref(), Ok("from the old name"), "an old script's variable is still read");
        assert_eq!(var_os(&new), Some(OsString::from("from the old name")));
        unsafe { std::env::set_var(&new, "from the new name") };
        assert_eq!(var(&new).as_deref(), Ok("from the new name"), "the new name wins");
        assert!(legacy_vars().contains(&old));
        unsafe {
            std::env::remove_var(&new);
            std::env::remove_var(&old);
        }
        assert_eq!(var(&new), Err(std::env::VarError::NotPresent));
        assert_eq!(var_os(&new), None);
    }

    #[test]
    fn sessions_get_both_names_and_either_name_is_recognised() {
        assert_eq!(both("ATELIER_SESSION", "s1"), [("ATELIER_SESSION".to_owned(), "s1".to_owned()), ("COPILOT_CANVAS_SESSION".to_owned(), "s1".to_owned())]); // rename:keep
        assert_eq!(both("PATH", "x"), [("PATH".to_owned(), "x".to_owned())]);
        assert!(is("atelier_session", "ATELIER_SESSION") && is("Copilot_Canvas_Session", "ATELIER_SESSION")); // rename:keep
        assert!(!is("ATELIER_SESSION_AGENT", "ATELIER_SESSION"));
        assert!(is_ours("ATELIER_HOME") && is_ours("copilot_canvas_home") && !is_ours("COPILOT_HOME")); // rename:keep
    }
}
