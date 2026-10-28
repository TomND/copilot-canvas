//! Builds the pinned libghostty-vt with Zig and vendors its headers.
//!
//! The pin (Ghostty commit and Zig version) lives in the header of
//! `crates/ghostty-vt-sys/vendor/CHECKSUMS.sha256`, the one file this task,
//! the sys crate's build script and its checksum test all read.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

const REPO: &str = "https://github.com/ghostty-org/ghostty";
/// Flags of the static build; recorded in BUILD-INFO.txt so a change rebuilds.
/// `-Dcpu=baseline` keeps the library runnable on any x86-64 machine (SIMD
/// paths are still selected at run time). The target defaults to the MSVC ABI.
const ZIG_FLAGS: &[&str] = &["-Demit-lib-vt=true", "-Doptimize=ReleaseFast", "-Dcpu=baseline"];
/// Where the pinned Zig is installed on the development machines.
const DEFAULT_ZIG: &str = r"C:\tools\zig-x86_64-windows-0.17.0\zig.exe";
pub const STATIC_LIB: &str = "ghostty-vt-static.lib";

/// The pinned engine version.
pub struct Pin {
    pub commit: String,
    pub zig: String,
}

impl Pin {
    fn short(&self) -> &str {
        &self.commit[..12]
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).expect("crates/xtask lives two levels below the workspace root").to_path_buf()
}

fn sys_crate(root: &Path) -> PathBuf {
    root.join("crates").join("ghostty-vt-sys")
}

fn checksums_file(root: &Path) -> PathBuf {
    sys_crate(root).join("vendor").join("CHECKSUMS.sha256")
}

fn ghostty_dir(root: &Path) -> PathBuf {
    root.join("target").join("ghostty")
}

fn source_dir(root: &Path, pin: &Pin) -> PathBuf {
    ghostty_dir(root).join(format!("src-{}", pin.short()))
}

/// Reads `# ghostty-commit:` and `# zig-version:` from the checksum file.
pub fn read_pin(path: &Path) -> Result<Pin> {
    let text = fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let value = |key: &str| {
        text.lines().find_map(|line| line.strip_prefix('#')?.trim().strip_prefix(key).map(|v| v.trim().to_owned()))
    };
    let commit = value("ghostty-commit:").context("CHECKSUMS.sha256 has no `# ghostty-commit:` line")?;
    let zig = value("zig-version:").context("CHECKSUMS.sha256 has no `# zig-version:` line")?;
    ensure!(commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()), "ghostty-commit must be a full 40-character hash");
    Ok(Pin { commit: commit.to_ascii_lowercase(), zig })
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
}

fn run(command: &mut Command, what: &str) -> Result<()> {
    let status = command.status().with_context(|| format!("cannot start {what}"))?;
    ensure!(status.success(), "{what} failed ({status})");
    Ok(())
}

/// `cargo xtask build-ghostty`: returns the install prefix.
pub fn build(force: bool) -> Result<PathBuf> {
    let root = workspace_root();
    let pin = read_pin(&checksums_file(&root))?;
    let out = ghostty_dir(&root).join(&pin.commit);
    let lib = out.join("lib").join(STATIC_LIB);
    let info_path = out.join("BUILD-INFO.txt");
    if !force && up_to_date(&info_path, &lib, &pin) {
        println!("libghostty-vt {} is up to date: {}", pin.short(), lib.display());
        return Ok(out);
    }

    let src = fetch(&root, &pin)?;
    let zig = find_zig(&pin)?;
    // BUILD-INFO.txt is written last, so an interrupted build is never taken as complete.
    if out.exists() {
        fs::remove_dir_all(&out).with_context(|| format!("cannot clean {}", out.display()))?;
    }
    println!("building libghostty-vt {} with {} {}", pin.short(), zig.display(), ZIG_FLAGS.join(" "));
    let started = SystemTime::now();
    run(
        Command::new(&zig)
            .arg("build")
            .args(ZIG_FLAGS)
            .arg("--prefix")
            .arg(&out)
            .arg("--cache-dir")
            .arg(ghostty_dir(&root).join("zig-cache"))
            .current_dir(&src),
        "zig build",
    )?;

    ensure!(lib.metadata().map(|m| m.len() > 0).unwrap_or(false), "zig build did not produce {}", lib.display());
    ensure!(out.join("include").join("ghostty").join("vt.h").is_file(), "zig build did not install include/ghostty/vt.h");
    check_headers(&root, &out)?;

    let lib_hash = sha256_file(&lib)?;
    let seconds = started.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let info = format!(
        "ghostty-commit: {}\nzig-version: {}\nzig-flags: {}\nsource: {REPO}\n{STATIC_LIB} sha256: {lib_hash}\nbuilt-unix: {now}\nbuild-seconds: {seconds}\n",
        pin.commit,
        pin.zig,
        ZIG_FLAGS.join(" "),
    );
    fs::write(&info_path, info).with_context(|| format!("cannot write {}", info_path.display()))?;
    println!("built {} ({seconds} s)\n{STATIC_LIB} sha256 {lib_hash}", lib.display());
    Ok(out)
}

/// A finished build of the same pin and flags whose library is unchanged.
fn up_to_date(info_path: &Path, lib: &Path, pin: &Pin) -> bool {
    let Ok(info) = fs::read_to_string(info_path) else { return false };
    let field = |key: &str| info.lines().find_map(|l| l.strip_prefix(key)).map(str::trim);
    field("ghostty-commit:") == Some(pin.commit.as_str())
        && field("zig-version:") == Some(pin.zig.as_str())
        && field("zig-flags:") == Some(ZIG_FLAGS.join(" ").as_str())
        && field(&format!("{STATIC_LIB} sha256:")).is_some_and(|hash| sha256_file(lib).is_ok_and(|h| h == hash))
}

/// The pinned source under target/ghostty/src-<short>: a git checkout, or the
/// GitHub source archive when git is unavailable.
fn fetch(root: &Path, pin: &Pin) -> Result<PathBuf> {
    let src = source_dir(root, pin);
    let stamp = src.join(".ghostty-commit");
    if fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == pin.commit) && src.join("build.zig").is_file() {
        return Ok(src);
    }
    if src.exists() {
        fs::remove_dir_all(&src).with_context(|| format!("cannot clean {}", src.display()))?;
    }
    fs::create_dir_all(ghostty_dir(root))?;
    if let Err(error) = git_checkout(&src, &pin.commit) {
        println!("git checkout failed ({error:#}); downloading the source archive");
        let _ = fs::remove_dir_all(&src);
        download(&src, &pin.commit)?;
    }
    fs::write(&stamp, &pin.commit)?;
    Ok(src)
}

fn git_checkout(src: &Path, commit: &str) -> Result<()> {
    println!("cloning {REPO} into {}", src.display());
    // Byte-exact upstream files (headers are vendored and checksummed), and
    // long paths for the fuzz corpus.
    run(
        Command::new("git")
            .args(["clone", "--quiet", "--filter=blob:none", "--no-checkout"])
            .args(["--config", "core.autocrlf=false", "--config", "core.longpaths=true"])
            .arg(REPO)
            .arg(src),
        "git clone",
    )?;
    run(Command::new("git").arg("-C").arg(src).args(["checkout", "--quiet", "--detach", commit]), "git checkout")?;
    let head = Command::new("git").arg("-C").arg(src).args(["rev-parse", "HEAD"]).output().context("git rev-parse")?;
    let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    ensure!(head == commit, "checked out {head}, expected {commit}");
    Ok(())
}

fn download(src: &Path, commit: &str) -> Result<()> {
    let parent = src.parent().context("source directory has a parent")?;
    let archive = parent.join(format!("ghostty-{commit}.tar.gz"));
    let url = format!("{REPO}/archive/{commit}.tar.gz");
    println!("downloading {url}");
    run(Command::new("curl.exe").args(["-fsSL", "-o"]).arg(&archive).arg(&url), "curl")?;
    run(Command::new("tar.exe").arg("-xzf").arg(&archive).arg("-C").arg(parent), "tar")?;
    fs::rename(parent.join(format!("ghostty-{commit}")), src).context("cannot move the extracted source")?;
    let _ = fs::remove_file(&archive);
    Ok(())
}

/// `ZIG`, then the default install, then `zig` on PATH; must be the pinned version.
fn find_zig(pin: &Pin) -> Result<PathBuf> {
    let zig = match std::env::var_os("ZIG") {
        Some(path) => PathBuf::from(path),
        None if Path::new(DEFAULT_ZIG).is_file() => PathBuf::from(DEFAULT_ZIG),
        None => PathBuf::from("zig"),
    };
    let output = Command::new(&zig)
        .arg("version")
        .output()
        .with_context(|| format!("cannot run `{} version`: install Zig {} or set ZIG to its zig.exe", zig.display(), pin.zig))?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if version != pin.zig {
        bail!("{} is Zig {version:?} but the pin needs Zig {}; set ZIG to a Zig {} zig.exe", zig.display(), pin.zig, pin.zig);
    }
    Ok(zig)
}

/// Parses `<sha256>  <path>` lines.
fn read_entries(path: &Path) -> Result<BTreeMap<String, String>> {
    let text = fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| l.split_once("  ").map(|(hash, file)| (file.trim().to_owned(), hash.trim().to_owned())))
        .collect())
}

/// The built headers must be the vendored ones (the bindings were checked against them).
fn check_headers(root: &Path, out: &Path) -> Result<()> {
    let vendored = read_entries(&checksums_file(root))?;
    let built = header_files(&out.join("include"))?;
    if !vendored.keys().any(|k| k.starts_with("vendor/include/")) {
        println!("note: no vendored headers yet; run `cargo xtask vendor-ghostty`");
        return Ok(());
    }
    let mut differences = Vec::new();
    for (relative, path) in &built {
        let key = format!("vendor/include/{relative}");
        match vendored.get(&key) {
            Some(hash) if *hash == sha256_file(path)? => {}
            Some(_) => differences.push(format!("changed: {relative}")),
            None => differences.push(format!("new: {relative}")),
        }
    }
    for key in vendored.keys().filter_map(|k| k.strip_prefix("vendor/include/")) {
        if !built.contains_key(key) {
            differences.push(format!("removed: {key}"));
        }
    }
    if !differences.is_empty() {
        bail!(
            "the built headers differ from crates/ghostty-vt-sys/vendor ({}); after a pin bump run \
             `cargo xtask vendor-ghostty`, review the header diff and update src/bindings.rs",
            differences.join(", ")
        );
    }
    println!("headers match vendor/CHECKSUMS.sha256 ({} files)", built.len());
    Ok(())
}

/// `*.h` files below `dir`, keyed by their `/`-separated relative path.
fn header_files(dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).with_context(|| format!("cannot list {}", current.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| e == "h") {
                let relative = path.strip_prefix(dir)?.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
                files.insert(relative, path);
            }
        }
    }
    Ok(files)
}

/// `cargo xtask vendor-ghostty`: refreshes vendor/ from the build and rewrites the checksums.
pub fn vendor(checksums_only: bool) -> Result<()> {
    let root = workspace_root();
    let sys = sys_crate(&root);
    let checksums = checksums_file(&root);
    let pin = read_pin(&checksums)?;
    if !checksums_only {
        let out = ghostty_dir(&root).join(&pin.commit);
        ensure!(out.join("BUILD-INFO.txt").is_file(), "no build of {}; run `cargo xtask build-ghostty` first", pin.short());
        let include = sys.join("vendor").join("include");
        if include.exists() {
            fs::remove_dir_all(&include)?;
        }
        for (relative, path) in header_files(&out.join("include"))? {
            let dest = include.join(&relative);
            fs::create_dir_all(dest.parent().context("header has a parent")?)?;
            fs::copy(&path, &dest).with_context(|| format!("cannot copy {relative}"))?;
        }
        let license = source_dir(&root, &pin).join("LICENSE");
        fs::copy(&license, sys.join("vendor").join("LICENSE")).with_context(|| format!("cannot copy {}", license.display()))?;
    }

    let old = read_entries(&checksums)?;
    let mut files: Vec<(String, PathBuf)> = vec![
        ("src/bindings.rs".to_owned(), sys.join("src").join("bindings.rs")),
        ("vendor/LICENSE".to_owned(), sys.join("vendor").join("LICENSE")),
    ];
    for (relative, path) in header_files(&sys.join("vendor").join("include"))? {
        files.push((format!("vendor/include/{relative}"), path));
    }
    let header: String = fs::read_to_string(&checksums)?.lines().take_while(|l| l.starts_with('#')).map(|l| format!("{l}\n")).collect();
    let mut text = header;
    let mut changed = 0;
    for (relative, path) in &files {
        let hash = sha256_file(path)?;
        if old.get(relative) != Some(&hash) {
            changed += 1;
            println!("updated: {relative}");
        }
        text.push_str(&format!("{hash}  {relative}\n"));
    }
    fs::write(&checksums, text)?;
    println!("wrote {} ({} files, {changed} changed)", checksums.display(), files.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pin_is_read_from_the_checksum_file() {
        let pin = read_pin(&checksums_file(&workspace_root())).expect("pin");
        assert_eq!((pin.commit.len(), pin.short().len()), (40, 12));
        assert!(!pin.zig.is_empty());
        let entries = read_entries(&checksums_file(&workspace_root())).unwrap();
        assert!(entries.contains_key("src/bindings.rs") && entries.contains_key("vendor/include/ghostty/vt.h"));
    }

    #[test]
    fn malformed_pins_are_rejected() {
        let dir = std::env::temp_dir().join(format!("xtask-pin-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("CHECKSUMS.sha256");
        for text in ["# zig-version: 0.17.0\n", "# ghostty-commit: 521cf2c5\n# zig-version: 0.17.0\n", "# ghostty-commit: 521cf2c5d30bd48028ca61546c64e7913764073d\n"] {
            fs::write(&file, text).unwrap();
            assert!(read_pin(&file).is_err(), "{text:?}");
        }
        fs::write(&file, "# ghostty-commit: 521CF2C5D30BD48028CA61546C64E7913764073D\n# zig-version: 0.17.0\n").unwrap();
        assert_eq!(read_pin(&file).unwrap().commit, "521cf2c5d30bd48028ca61546c64e7913764073d");
        let _ = fs::remove_dir_all(&dir);
    }
}
