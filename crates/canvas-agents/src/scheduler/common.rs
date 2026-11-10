//! What the backends that run the built-in launcher share (macOS and Linux):
//! its command line and task file, schedules as calendar slots, drift between
//! a rendered and an installed definition, the next run, quoting.

use std::path::{Path, PathBuf};

use crate::factory::{Defaults, Manifest, Trigger};

/// The program a schedule starts: the AppImage itself when running from
/// one (its mount point disappears with it), else this executable.
pub fn launcher_exe() -> PathBuf {
    if let Some(image) = std::env::var_os("APPIMAGE").filter(|p| !p.is_empty()) {
        return PathBuf::from(image);
    }
    std::env::current_exe().map(|exe| exe.canonicalize().unwrap_or(exe)).unwrap_or_else(|_| PathBuf::from("atelier"))
}

/// What a schedule runs for a manifest: the folder's own launcher (`python
/// launcher task.json`) when `automations.yaml` names one, else the built-in
/// launcher (`atelier automations launch task.json`).
pub fn action(m: &Manifest, d: &Defaults) -> Vec<String> {
    let task = d.task_json(&m.name).display().to_string();
    if !d.launcher.is_empty() && !d.python.is_empty() {
        vec![d.python.clone(), d.launcher.clone(), task]
    } else {
        vec![launcher_exe().display().to_string(), "automations".into(), "launch".into(), task]
    }
}

/// The launcher's task file: the Python launcher's fields plus what the
/// built-in launcher enforces itself.
pub fn task_json(m: &Manifest, d: &Defaults) -> String {
    let mut json: serde_json::Value = serde_json::from_str(&crate::factory::task_json(m, d)).unwrap_or_default();
    json["timeLimitMs"] = m.time_limit_ms.into();
    json["overlap"] = m.overlap.clone().into();
    json["restartCount"] = m.restart_count.into();
    json["restartEveryMs"] = m.restart_every_ms.into();
    json["successCodes"] = m.success_codes.clone().into();
    json["startWhenAvailable"] = m.start_when_available.into();
    serde_json::to_string_pretty(&json).unwrap_or_default()
}

/// Fills a manifest from a task file (import); returns the file's state folder.
pub fn read_task_json(path: &Path, m: &mut Manifest) -> Option<PathBuf> {
    let bytes = std::fs::read(path).ok()?;
    let json: serde_json::Value = serde_json::from_slice(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes)).ok()?;
    let text = |k: &str| json[k].as_str().unwrap_or_default().to_owned();
    m.command = text("executable");
    m.args = text("arguments");
    m.cwd = text("workingDirectory");
    m.report = text("reportCommand");
    if !text("manifest").is_empty() {
        m.path = PathBuf::from(text("manifest"));
    }
    m.time_limit_ms = json["timeLimitMs"].as_u64().unwrap_or(m.time_limit_ms);
    if let Some(overlap) = json["overlap"].as_str() {
        m.overlap = overlap.to_owned();
    }
    m.restart_count = json["restartCount"].as_u64().unwrap_or(0) as u32;
    m.restart_every_ms = json["restartEveryMs"].as_u64().unwrap_or(0);
    if let Some(codes) = json["successCodes"].as_array() {
        m.success_codes = codes.iter().filter_map(serde_json::Value::as_i64).collect();
    }
    m.start_when_available = json["startWhenAvailable"].as_bool().unwrap_or(true);
    super::state_of(path).map(Path::to_path_buf)
}

/// The fields of the launcher's file that differ from what `apply` would
/// write (`what: installed → manifest`).
pub fn task_json_drift(m: &Manifest, d: &Defaults) -> Vec<String> {
    let path = d.task_json(&m.name);
    let Ok(text) = std::fs::read_to_string(&path) else { return vec![format!("launcher settings: missing → {}", path.display())] };
    let have: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    let want: serde_json::Value = serde_json::from_str(&task_json(m, d)).unwrap_or_default();
    let labels = [
        ("executable", "command"),
        ("arguments", "arguments"),
        ("workingDirectory", "folder"),
        ("reportCommand", "report"),
        ("timeLimitMs", "time limit"),
        ("overlap", "overlap"),
        ("restartCount", "restart on failure"),
        ("restartEveryMs", "restart interval"),
        ("successCodes", "success codes"),
        ("startWhenAvailable", "start when available"),
        ("logPath", "log"),
        ("statusPath", "status file"),
        ("manifest", "manifest"),
    ];
    let show = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "—".into(),
        other => other.to_string(),
    };
    labels.iter().filter(|(key, _)| have[*key] != want[*key]).map(|(key, label)| format!("{label}: {} → {}", show(&have[*key]), show(&want[*key]))).collect()
}

/// When a trigger fires, as calendar slots in local time.
#[derive(Clone, Debug, PartialEq)]
pub enum Slots {
    /// At `minute` past each of `hours`, on `days` (0 = Monday; empty: every day).
    At { days: Vec<usize>, hours: Vec<u32>, minute: u32 },
    /// Every `step` minutes from `first` past each hour (`step` divides 60).
    Minutes { first: u32, step: u32 },
    /// An interval that does not divide a day: first at `first` (local
    /// minutes), then every `ms` (systemd keeps the pace; cron restarts it
    /// each day at `first`).
    Interval { first: u32, ms: u64 },
    /// At logon (systemd: when the user manager starts; cron: `@reboot`).
    Logon,
}

/// A trigger as slots (one or more).
pub fn slots(trigger: &Trigger) -> Vec<Slots> {
    match trigger {
        Trigger::Logon => vec![Slots::Logon],
        Trigger::Daily(at) => vec![Slots::At { days: Vec::new(), hours: vec![at / 60], minute: at % 60 }],
        Trigger::Weekly(days, at) => {
            let days = days.iter().filter_map(|d| crate::factory::DAYS.iter().position(|x| x == d)).collect();
            vec![Slots::At { days, hours: vec![at / 60], minute: at % 60 }]
        }
        Trigger::Every(ms, from) => {
            let step = (*ms / 60_000).max(1) as u32;
            if step < 60 && 60 % step == 0 {
                return vec![Slots::Minutes { first: from % step, step }];
            }
            if step > 1440 || 1440 % step != 0 {
                return vec![Slots::Interval { first: from % 1440, ms: *ms }];
            }
            // Every time of day the interval lands on, grouped by minute.
            let mut by_minute: Vec<(u32, Vec<u32>)> = Vec::new();
            let phase = from % step;
            let mut t = phase;
            while t < 1440 {
                match by_minute.iter_mut().find(|(m, _)| *m == t % 60) {
                    Some((_, hours)) => hours.push(t / 60),
                    None => by_minute.push((t % 60, vec![t / 60])),
                }
                t += step;
            }
            by_minute.into_iter().map(|(minute, hours)| Slots::At { days: Vec::new(), hours, minute }).collect()
        }
    }
}

/// Lines of `rendered` and `installed` compared per key (`Key=value` lines,
/// in order; other lines by themselves): `what: installed → rendered`.
pub fn definition_drift(installed: &str, rendered: &str) -> Vec<String> {
    let entries = |text: &str| -> Vec<(String, String)> {
        let mut section = String::new();
        let mut out = Vec::new();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if line.starts_with('[') {
                section = line.to_owned();
                continue;
            }
            match line.split_once('=').filter(|(k, _)| !k.contains(' ') && !line.starts_with('#')) {
                Some((key, value)) => out.push((format!("{section}{key}"), value.to_owned())),
                None => out.push((line.to_owned(), String::new())),
            }
        }
        out
    };
    let (have, want) = (entries(installed), entries(rendered));
    let mut keys: Vec<&String> = have.iter().chain(&want).map(|(k, _)| k).collect();
    keys.dedup();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for key in keys {
        if !seen.insert(key) {
            continue;
        }
        let values = |list: &[(String, String)]| list.iter().filter(|(k, _)| k == key).map(|(_, v)| v.clone()).collect::<Vec<_>>();
        let (old, new) = (values(&have), values(&want));
        if old == new {
            continue;
        }
        let show = |v: &[String], key: &str| if v.is_empty() { "—".to_owned() } else if v.iter().all(String::is_empty) { key.to_owned() } else { v.join(" · ") };
        let label = key.rsplit(']').next().unwrap_or(key);
        if old.iter().chain(&new).all(String::is_empty) {
            out.push(if old.is_empty() { format!("adds {label}") } else { format!("removes {label}") });
        } else {
            out.push(format!("{label}: {} → {}", show(&old, label), show(&new, label)));
        }
    }
    out
}

/// Local calendar fields of a Unix time (at today's UTC offset).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalTime {
    pub days: i64,
    /// 0 = Monday.
    pub weekday: usize,
    pub minute_of_day: u32,
}

pub fn local_time(unix_ms: u64, offset_minutes: i32) -> LocalTime {
    let minutes = (unix_ms / 60_000) as i64 + offset_minutes as i64;
    let days = minutes.div_euclid(1440);
    // 1970-01-01 was a Thursday (3 when Monday is 0).
    LocalTime { days, weekday: (days + 3).rem_euclid(7) as usize, minute_of_day: minutes.rem_euclid(1440) as u32 }
}

/// The next time (Unix ms, after `now_ms`) the slots fire, within 8 days.
pub fn next_run(slots: &[Slots], now_ms: u64, offset_minutes: i32) -> Option<u64> {
    let start = now_ms / 60_000 + 1;
    (start..start + 8 * 1440).find(|minute| {
        let t = local_time(minute * 60_000, offset_minutes);
        slots.iter().any(|s| match s {
            Slots::At { days, hours, minute } => (days.is_empty() || days.contains(&t.weekday)) && hours.contains(&(t.minute_of_day / 60)) && t.minute_of_day % 60 == *minute,
            Slots::Minutes { first, step } => (t.minute_of_day % 60) % step == *first,
            Slots::Interval { first, ms } => {
                let step = (*ms / 60_000).max(1);
                t.minute_of_day >= *first && ((t.minute_of_day - first) as u64).is_multiple_of(step)
            }
            Slots::Logon => false,
        })
    })
    .map(|minute| minute * 60_000)
}

/// The next run (Unix ms; 0: none) of a schedule.
pub fn next_from_schedule(schedule: &[Trigger], now_ms: u64, offset_minutes: i32) -> u64 {
    let all: Vec<Slots> = schedule.iter().flat_map(slots).collect();
    next_run(&all, now_ms, offset_minutes).unwrap_or(0)
}

/// A local timestamp with its offset, `2026-10-26T07:18:36-07:00` (the
/// launcher logs' format).
pub fn local_stamp(unix_ms: u64, offset_minutes: i32) -> String {
    let utc = canvas_common::paths::format_rfc3339_ms((unix_ms as i64 + offset_minutes as i64 * 60_000).max(0) as u64);
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    format!("{}{sign}{:02}:{:02}", &utc[..19], offset_minutes.abs() / 60, offset_minutes.abs() % 60)
}

/// `s` as one word for `/bin/sh`.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:,@".contains(c)) {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Splits a command line into words the way a POSIX shell would for plain
/// quoting (`'…'`, `"…"` with `\"`, backslash escapes); no expansions.
pub fn split_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                started = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                started = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => word.push(chars.next().unwrap_or('\\')),
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                started = true;
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            c if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_become_calendar_slots() {
        assert_eq!(slots(&Trigger::Daily(6 * 60 + 45)), vec![Slots::At { days: vec![], hours: vec![6], minute: 45 }]);
        assert_eq!(slots(&Trigger::Weekly(vec!["mon".into(), "fri".into()], 960)), vec![Slots::At { days: vec![0, 4], hours: vec![16], minute: 0 }]);
        assert_eq!(slots(&Trigger::Every(7_200_000, 6 * 60 + 10)), vec![Slots::At { days: vec![], hours: (0..24).step_by(2).collect(), minute: 10 }]);
        assert_eq!(slots(&Trigger::Every(900_000, 5)), vec![Slots::Minutes { first: 5, step: 15 }]);
        let ninety = slots(&Trigger::Every(5_400_000, 0));
        assert_eq!(ninety.len(), 2, "on the hour and at half past: {ninety:?}");
        assert_eq!(slots(&Trigger::Every(7 * 3_600_000, 60)), vec![Slots::Interval { first: 60, ms: 7 * 3_600_000 }]);
    }

    #[test]
    fn next_runs_and_local_time() {
        // 2026-10-31 (a Saturday) 12:00 UTC; local UTC-7 is 05:00.
        let now = 1_793_448_000_000;
        assert_eq!(local_time(now, -420), LocalTime { days: local_time(now, -420).days, weekday: 5, minute_of_day: 300 });
        let daily = slots(&Trigger::Daily(6 * 60 + 45));
        assert_eq!(next_run(&daily, now, -420), Some(now + (105 * 60_000)));
        let monday = slots(&Trigger::Weekly(vec!["mon".into()], 60));
        assert_eq!(next_run(&monday, now, -420), Some(now + (2 * 1440 - 240) * 60_000));
        assert_eq!(next_run(&[Slots::Minutes { first: 5, step: 15 }], now, 0), Some(now + 5 * 60_000));
        assert_eq!(next_run(&[Slots::Logon], now, 0), None);
        assert_eq!(local_stamp(now, -420), "2026-10-31T05:00:00-07:00");
        assert_eq!(local_stamp(now, 330), "2026-10-31T17:30:00+05:30");
    }

    #[test]
    fn drift_compares_keys_and_lines() {
        let installed = "[Timer]\nOnCalendar=*-*-* 06:45:00\nPersistent=true\n# enabled: true\n";
        assert!(definition_drift(installed, installed).is_empty());
        let rendered = "[Timer]\nOnCalendar=*-*-* 07:00:00\nOnCalendar=Mon *-*-* 08:00:00\nPersistent=true\n# enabled: false\n";
        assert_eq!(
            definition_drift(installed, rendered),
            vec!["OnCalendar: *-*-* 06:45:00 → *-*-* 07:00:00 · Mon *-*-* 08:00:00", "removes # enabled: true", "adds # enabled: false"]
        );
    }

    #[test]
    fn words_and_quotes() {
        assert_eq!(split_words(r#"-File "C:\x y\run.ps1" -Kind 'a b' c\ d"#), vec!["-File", r"C:\x y\run.ps1", "-Kind", "a b", "c d"]);
        assert_eq!(split_words(r#"say "a \"b\"" ''"#), vec!["say", "a \"b\"", ""]);
        assert!(split_words("   ").is_empty());
        assert_eq!(sh_quote("/usr/bin/x"), "/usr/bin/x");
        assert_eq!(sh_quote("it's here"), "'it'\\''s here'");
    }
}
