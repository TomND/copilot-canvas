//! The crash log (`<home>\crash.log`): panics on any thread (with the build,
//! the number of windows, the GPU adapter and the last test-hook command)
//! and GPU or driver events (device creation failures, lost devices and
//! surfaces, uncaptured device errors) with the adapter's details.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use canvas_core::paths;

static PATH: OnceLock<PathBuf> = OnceLock::new();
static WINDOWS: AtomicUsize = AtomicUsize::new(0);
static LAST_HOOK: Mutex<Option<String>> = Mutex::new(None);
static ADAPTER: Mutex<Option<String>> = Mutex::new(None);
/// When each event text was last written (the same text at most every 10 s).
static RECENT: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn set_windows(count: usize) {
    WINDOWS.store(count, Ordering::Relaxed);
}

/// The test-hook command being handled (reported if it crashes the process).
pub fn set_hook(command: &str) {
    *lock(&LAST_HOOK) = Some(command.chars().take(300).collect());
}

pub fn set_adapter(description: String) {
    *lock(&ADAPTER) = Some(description);
}

fn append(text: &str) {
    if let Some(path) = PATH.get()
        && let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = file.write_all(text.as_bytes());
    }
}

/// Logs a GPU or driver event (rate-limited per text).
pub fn event(kind: &str, text: &str) {
    {
        let mut recent = lock(RECENT.get_or_init(Default::default));
        let now = Instant::now();
        if recent.get(text).is_some_and(|t| now.duration_since(*t) < Duration::from_secs(10)) {
            return;
        }
        recent.insert(text.to_owned(), now);
    }
    let adapter = lock(&ADAPTER).clone().unwrap_or_else(|| "no adapter yet".into());
    let line = format!("{} gui build {} {kind}: {text} [adapter: {adapter}]\n", paths::now_ms(), paths::build_id());
    if std::env::var_os("CANVAS_GUI_TRACE").is_some() {
        eprint!("{line}");
    }
    append(&line);
}

/// Panics (on any thread) are written to the log; the process exits (the
/// sessions keep running in the service).
pub fn install(path: PathBuf) {
    let _ = PATH.set(path.clone());
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let report = format!(
            "{} gui build {} thread {:?} windows {} adapter {} last hook {}\n{info}\n{}\n\n",
            paths::now_ms(),
            paths::build_id(),
            thread.name().unwrap_or("?"),
            WINDOWS.load(Ordering::Relaxed),
            lock(&ADAPTER).clone().unwrap_or_else(|| "none".into()),
            lock(&LAST_HOOK).clone().unwrap_or_else(|| "none".into()),
            std::backtrace::Backtrace::force_capture()
        );
        append(&report);
        previous(info);
        eprintln!("Copilot Canvas crashed; details saved to {}. Your sessions keep running: start it again to reattach.", path.display());
        std::process::exit(101);
    }));
}
