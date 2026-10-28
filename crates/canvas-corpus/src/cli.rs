//! `copilot-canvas record …` and `copilot-canvas corpus …`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use canvas_host::emulator::Engine;

use crate::record::{self, Spec};
use crate::replay::{Expected, compare, replay};
use crate::store::{Blessed, Corpus, Meta, Outcome};
use crate::vtrec::Recording;
use crate::{engines, synth};

/// The service's recording tap writes here (`COPILOT_CANVAS_RECORD`).
pub use canvas_host::term::RECORD_ENV;

type Options<'a> = &'a [(String, String)];

fn option<'a>(options: Options<'a>, name: &str) -> Option<&'a str> {
    options.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn all_options<'a>(options: Options<'a>, name: &str) -> Vec<&'a str> {
    options.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect()
}

fn number<T: std::str::FromStr>(options: Options, name: &str, default: T) -> Result<T> {
    match option(options, name) {
        Some(value) => value.parse().map_err(|_| anyhow::anyhow!("--{name} needs a number, not {value:?}")),
        None => Ok(default),
    }
}

/// `corpus list|check|bless|synth|show|diff`.
pub fn corpus(words: &[&str], options: Options) -> Result<()> {
    let corpus = Corpus::locate(option(options, "corpus").map(Path::new))?;
    let engine = option(options, "engine");
    match words {
        [] | ["list"] => list(&corpus),
        ["check", names @ ..] => check(&corpus, names, engine),
        ["bless", names @ ..] => bless(&corpus, names, engine),
        ["synth", names @ ..] => synthesize(&corpus, names),
        ["show", name] => show(&corpus, name, engine, option(options, "mark")),
        ["diff", names @ ..] => diff(&corpus, names, engine.unwrap_or("alacritty"), option(options, "against").unwrap_or("ghostty")),
        other => bail!("unknown corpus command {:?} (list, check, bless, synth, show, diff)", other.join(" ")),
    }
}

fn list(corpus: &Corpus) -> Result<()> {
    println!("corpus {}", corpus.dir.display());
    println!("{:<24} {:<10} {:>8} {:>7} {:>8} {:>5}  {:<18} TAGS", "NAME", "SOURCE", "SIZE", "CHUNKS", "BYTES", "MARKS", "EXPECTED");
    for name in corpus.names()? {
        let recording = corpus.recording(&name)?;
        let meta = corpus.meta(&name).unwrap_or_default();
        let expected: Vec<&str> = crate::ENGINES.iter().filter(|e| corpus.expected_path(&name, **e).is_file()).map(|e| e.name()).collect();
        println!(
            "{:<24} {:<10} {:>8} {:>7} {:>8} {:>5}  {:<18} {}",
            name,
            meta.source,
            format!("{}x{}", recording.header.cols, recording.header.rows),
            recording.chunks(),
            recording.output_bytes(),
            recording.marks().len(),
            expected.join(","),
            meta.tags.join(",")
        );
    }
    Ok(())
}

fn check(corpus: &Corpus, names: &[&str], engine: Option<&str>) -> Result<()> {
    let engines = engines(engine)?;
    let names = corpus.select(names)?;
    let mut failed = 0usize;
    let mut checked = 0usize;
    for name in &names {
        for &engine in &engines {
            checked += 1;
            match corpus.check(name, engine)? {
                Outcome::Match { screens } => println!("ok    {:<9} {name} ({screens} screen{})", engine.name(), if screens == 1 { "" } else { "s" }),
                Outcome::Mismatch(diff) => {
                    failed += 1;
                    println!("FAIL  {:<9} {name}", engine.name());
                    for line in diff {
                        println!("      {line}");
                    }
                }
                Outcome::Missing => {
                    failed += 1;
                    println!(
                        "MISS  {:<9} {name}: no expected.{0}.json (review, then `copilot-canvas corpus bless --engine {0} {name}`)",
                        engine.name()
                    );
                }
            }
        }
        for other in crate::ENGINES.into_iter().filter(|e| !e.available()) {
            if corpus.expected_path(name, other).is_file() {
                println!("skip  {:<9} {name} (engine not in this build)", other.name());
            }
        }
    }
    let engine_names: Vec<&str> = engines.iter().map(|e| e.name()).collect();
    println!("{checked} checked ({}), {failed} failed", engine_names.join(", "));
    if failed > 0 {
        bail!("{failed} corpus check(s) failed");
    }
    Ok(())
}

fn bless(corpus: &Corpus, names: &[&str], engine: Option<&str>) -> Result<()> {
    let engines = engines(engine)?;
    for name in corpus.select(names)? {
        for &engine in &engines {
            match corpus.bless(&name, engine)? {
                Blessed::New => println!("new        {:<9} {name}", engine.name()),
                Blessed::Unchanged => println!("unchanged  {:<9} {name}", engine.name()),
                Blessed::Updated(diff) => {
                    println!("updated    {:<9} {name} (review the change; previous expectation vs now):", engine.name());
                    for line in diff.iter().take(40) {
                        println!("           {line}");
                    }
                    if diff.len() > 40 {
                        println!("           … {} more", diff.len() - 40);
                    }
                }
            }
        }
    }
    Ok(())
}

fn synthesize(corpus: &Corpus, names: &[&str]) -> Result<()> {
    let all = synth::all();
    let selected: Vec<_> = all.iter().filter(|s| names.is_empty() || names.contains(&s.name)).collect();
    for name in names {
        if !all.iter().any(|s| s.name == *name) {
            bail!("no synthetic entry {name:?}");
        }
    }
    for entry in selected {
        let recording = entry.recording();
        corpus.write_entry(entry.name, &recording, &entry.meta(), None)?;
        println!("wrote {} ({} chunks, {} bytes)", corpus.stream_path(entry.name).display(), recording.chunks(), recording.output_bytes());
    }
    println!("Next: `copilot-canvas corpus bless` and review the expected files.");
    Ok(())
}

fn print_expected(expected: &Expected, mark: Option<&str>) {
    for screen in expected.screens.iter().filter(|s| mark.is_none_or(|m| m == s.mark)) {
        println!(
            "== [{}] {}x{} cursor {:?} shape {}{} history {} modes {:?}",
            screen.mark,
            screen.cols,
            screen.rows,
            screen.cursor,
            screen.cursor_shape,
            if screen.cursor_blink { " blinking" } else { "" },
            screen.history,
            screen.mode_names
        );
        for line in &screen.text {
            println!("|{line}");
        }
        if !screen.links.is_empty() {
            println!("links: {:?}", screen.links);
        }
    }
    println!("events: {}", serde_json::to_string(&expected.events).unwrap_or_default());
}

fn show(corpus: &Corpus, name: &str, engine: Option<&str>, mark: Option<&str>) -> Result<()> {
    let name = corpus.select(&[name])?.remove(0);
    let engine = engines(engine)?[0];
    let meta = corpus.meta(&name).unwrap_or_default();
    println!("{name}: {} ({})", meta.description, engine.name());
    print_expected(&replay(engine, &corpus.recording(&name)?)?, mark);
    Ok(())
}

/// Differences between two engines' expected files (input for `reviewed-differences.md`).
fn diff(corpus: &Corpus, names: &[&str], engine: &str, against: &str) -> Result<()> {
    let parse = |name: &str| Engine::parse(name).with_context(|| format!("unknown engine {name:?}"));
    let (a, b) = (parse(engine)?, parse(against)?);
    for name in corpus.select(names)? {
        match (corpus.expected(&name, a)?, corpus.expected(&name, b)?) {
            (Some(x), Some(y)) => {
                let diff = compare(&x, &y);
                if diff.is_empty() {
                    println!("same  {name}");
                } else {
                    println!("diff  {name} (expected = {}, actual = {})", a.name(), b.name());
                    for line in diff {
                        println!("      {line}");
                    }
                }
            }
            _ => println!("skip  {name} (needs expected.{}.json and expected.{}.json)", a.name(), b.name()),
        }
    }
    Ok(())
}

/// The Windows version ConPTY behaviour depends on (`ver`).
fn windows_version() -> String {
    std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "ver"])
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// Writes a recording to `--out` and/or a corpus entry (`--entry NAME`).
fn save(recording: &Recording, options: Options, mut meta: Meta, script: Option<&str>) -> Result<()> {
    let out = option(options, "out").map(PathBuf::from);
    let entry = option(options, "entry");
    if out.is_none() && entry.is_none() {
        bail!("pass --out FILE or --entry NAME");
    }
    if let Some(out) = &out {
        recording.write(out)?;
        println!("wrote {}", out.display());
    }
    if let Some(name) = entry {
        let corpus = Corpus::locate(option(options, "corpus").map(Path::new))?;
        if let Some(description) = option(options, "description") {
            meta.description = description.to_owned();
        }
        if let Some(tags) = option(options, "tags") {
            meta.tags = tags.split(',').map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()).collect();
        }
        if let Some(notes) = option(options, "notes") {
            meta.notes = notes.to_owned();
        }
        corpus.write_entry(name, recording, &meta, script)?;
        println!("wrote {} (next: review, then `copilot-canvas corpus bless {name}`)", corpus.entry_dir(name).display());
    }
    let preview = record::preview(recording)?;
    let end = preview.screens.last().expect("the end screen");
    println!(
        "{} chunks, {} bytes, {:.1} s, marks {:?}; final screen ({}x{}):",
        recording.chunks(),
        recording.output_bytes(),
        recording.duration_ms() as f64 / 1000.0,
        recording.marks(),
        end.cols,
        end.rows
    );
    for line in record::screen_text(end).lines() {
        println!("|{line}");
    }
    Ok(())
}

/// `record [--cols N] [--rows M] [--input SCRIPT] [--seconds S] [--env K=V]… (--out FILE | --entry NAME) -- PROGRAM [ARGS…]`
/// and `record --session ID (--out FILE | --entry NAME) [--dir DIR]`.
pub fn record(home: &Path, words: &[&str], options: Options, cwd: Option<&Path>) -> Result<()> {
    if let Some(id) = option(options, "session") {
        return session(home, id, options);
    }
    let command: Vec<String> = match words {
        ["--", rest @ ..] | rest => rest.iter().map(|s| (*s).to_owned()).collect(),
    };
    let Some((program, args)) = command.split_first() else {
        bail!("usage: copilot-canvas record --out FILE [--cols N] [--rows M] [--input SCRIPT] [--seconds S] -- PROGRAM [ARGS…]");
    };
    let script_text = match option(options, "input") {
        Some(path) => Some(std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?),
        None => None,
    };
    let script = record::parse_script(script_text.as_deref().unwrap_or_default())?;
    let mut env = Vec::new();
    for pair in all_options(options, "env") {
        let (key, value) = pair.split_once('=').with_context(|| format!("--env needs NAME=value, not {pair:?}"))?;
        env.push((key.to_owned(), value.to_owned()));
    }
    let spec = Spec {
        program: program.clone(),
        args: args.to_vec(),
        cwd: cwd.map(Path::to_path_buf),
        env: env.clone(),
        cols: number(options, "cols", 120)?,
        rows: number(options, "rows", 40)?,
        limit: Duration::from_secs_f64(number(options, "seconds", 30.0)?),
        script,
        description: option(options, "description").unwrap_or_default().to_owned(),
        verbose: true,
    };
    let recording = record::record(&spec)?;
    let meta = Meta {
        description: spec.description.clone(),
        source: "recorded".into(),
        cols: spec.cols,
        rows: spec.rows,
        command,
        env: env.iter().map(|(k, v)| format!("{k}={v}")).collect(),
        recorded_on: windows_version(),
        recorded_at: recording.header.recorded_at.clone(),
        ..Meta::default()
    };
    save(&recording, options, meta, script_text.as_deref())
}

/// Copies (and normalizes) the service tap's recording of one terminal.
fn session(home: &Path, id: &str, options: Options) -> Result<()> {
    let dir = match option(options, "dir") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::var_os(RECORD_ENV).filter(|d| !d.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join("recordings")),
    };
    let exact = canvas_host::term::recording_path(&dir, id);
    let path = if exact.is_file() {
        Some(exact)
    } else {
        let stem = exact.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let matches: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "vtrec"))
                    .filter(|p| p.file_stem().is_some_and(|s| s.to_string_lossy().starts_with(&stem)))
                    .collect()
            })
            .unwrap_or_default();
        if matches.len() > 1 {
            bail!("{id:?} matches several recordings in {}: {matches:?}", dir.display());
        }
        matches.into_iter().next()
    };
    let Some(path) = path else {
        let known: Vec<String> = std::fs::read_dir(&dir)
            .map(|entries| entries.flatten().filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned())).collect())
            .unwrap_or_default();
        println!("No recording of terminal {id:?} in {}.", dir.display());
        if !known.is_empty() {
            println!("Recorded terminals there: {}", known.join(", "));
        }
        println!("The service records terminal output only when it starts with {RECORD_ENV} set:");
        println!("  $env:{RECORD_ENV} = \"{}\"", dir.display());
        println!("  copilot-canvas service stop      # ends the running sessions");
        println!("  copilot-canvas                   # reopen; the service restarts and records every terminal");
        println!("Then use the session and run this command again.");
        bail!("nothing recorded for {id:?} yet");
    };
    let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut segments = Recording::segments(&text).with_context(|| format!("in {}", path.display()))?;
    if segments.len() > 1 {
        println!("{} holds {} runs of this terminal; using the last.", path.display(), segments.len());
    }
    let mut recording = segments.pop().expect("at least one segment");
    if let Some(description) = option(options, "description") {
        recording.header.description = description.to_owned();
    }
    let meta = Meta {
        description: recording.header.description.clone(),
        source: "session".into(),
        cols: recording.header.cols,
        rows: recording.header.rows,
        command: vec!["copilot-canvas".into(), "record".into(), "--session".into(), id.to_owned()],
        recorded_on: windows_version(),
        recorded_at: recording.header.recorded_at.clone(),
        ..Meta::default()
    };
    save(&recording, options, meta, None)
}
