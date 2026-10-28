// Links the pinned libghostty-vt static library.
//
// The library is built outside Cargo by `cargo xtask build-ghostty` (Zig
// builds take minutes and fetch sources, which a build script should not do
// implicitly). It is looked up in `<workspace>/target/ghostty/<commit>/lib`
// for the commit pinned in `vendor/CHECKSUMS.sha256`; its `BUILD-INFO.txt`
// must name that commit and the library's sha256. `GHOSTTY_VT_LIB_DIR`
// overrides the folder and skips these provenance checks (with a warning).
//
// Plain comments, not `//!`: xtask's tests `include!` this file (hence the
// `pub(crate)` checks).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const STATIC_LIB: &str = "ghostty-vt-static.lib";

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let checksums = manifest.join("vendor").join("CHECKSUMS.sha256");
    println!("cargo:rerun-if-changed={}", checksums.display());
    println!("cargo:rerun-if-env-changed=GHOSTTY_VT_LIB_DIR");

    let text = fs::read_to_string(&checksums).unwrap_or_else(|e| panic!("cannot read {}: {e}", checksums.display()));
    let pin = |key: &str| pin_value(&text, key).unwrap_or_else(|| panic!("{} has no `# {key}` line", checksums.display()));
    let commit = normalize_commit(&pin("ghostty-commit:")).unwrap_or_else(|error| panic!("{}: {error}", checksums.display()));
    let zig = pin("zig-version:");
    println!("cargo:rustc-env=GHOSTTY_VT_COMMIT={commit}");
    println!("cargo:rustc-env=GHOSTTY_VT_ZIG_VERSION={zig}");

    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("windows-msvc") {
        panic!("ghostty-vt-sys links the MSVC build of libghostty-vt; target {target} is not supported");
    }

    let (dir, from_env) = match env::var_os("GHOSTTY_VT_LIB_DIR") {
        Some(dir) => (PathBuf::from(dir), true),
        None => (manifest.ancestors().nth(2).expect("workspace root").join("target").join("ghostty").join(&commit).join("lib"), false),
    };
    let lib = dir.join(STATIC_LIB);
    if !lib.is_file() {
        let hint = if from_env {
            "GHOSTTY_VT_LIB_DIR does not contain it".to_owned()
        } else {
            format!("build it with `cargo xtask build-ghostty` (needs git and Zig {zig})")
        };
        panic!("\n\nlibghostty-vt {} not found: {}\n{hint}\n\n", short(&commit), lib.display());
    }
    println!("cargo:rerun-if-changed={}", lib.display());
    if from_env {
        println!("cargo:warning=GHOSTTY_VT_LIB_DIR is set: libghostty-vt provenance checks (pin, BUILD-INFO.txt, library sha256) are skipped");
    } else {
        let info = dir.parent().map(|p| p.join("BUILD-INFO.txt")).unwrap_or_default();
        println!("cargo:rerun-if-changed={}", info.display());
        if let Err(error) = check_build_info(&info, &commit, &lib) {
            panic!("\n\n{error}\nrun `cargo xtask build-ghostty --force`\n\n");
        }
    }
    println!("cargo:rustc-link-search=native={}", dir.display());
    // Zig installs the archive under this name so it does not collide with the
    // DLL import library `ghostty-vt.lib`; `+verbatim` links exactly this file.
    println!("cargo:rustc-link-lib=static:+verbatim={STATIC_LIB}");
    // The Zig standard library inside the archive calls NT and kernel32 APIs.
    println!("cargo:rustc-link-lib=dylib=ntdll");
    println!("cargo:rustc-link-lib=dylib=kernel32");
}

/// The value of a `# <key> <value>` header line.
fn pin_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| l.strip_prefix('#')?.trim().strip_prefix(key).map(|v| v.trim().to_owned()))
}

/// The pinned commit, lowercased (as xtask names its build folder): exactly 40 hex digits.
pub(crate) fn normalize_commit(commit: &str) -> Result<String, String> {
    let commit = commit.trim();
    if commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(commit.to_ascii_lowercase())
    } else {
        Err(format!("ghostty-commit {commit:?} is not a full 40-character hex hash"))
    }
}

pub(crate) fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
}

/// A library from `cargo xtask build-ghostty`: `BUILD-INFO.txt` names the
/// pinned commit and the sha256 of exactly this library file.
pub(crate) fn check_build_info(info: &Path, commit: &str, lib: &Path) -> Result<(), String> {
    let text = fs::read_to_string(info).map_err(|_| format!("{} is missing (interrupted build?)", info.display()))?;
    let field = |key: &str| text.lines().find_map(|l| l.strip_prefix(key)).map(|v| v.trim().to_ascii_lowercase());
    if field("ghostty-commit:").as_deref() != Some(commit) {
        return Err(format!("{} is not a build of {commit}", info.display()));
    }
    let recorded = field(&format!("{STATIC_LIB} sha256:")).ok_or_else(|| format!("{} records no {STATIC_LIB} sha256", info.display()))?;
    let actual = sha256_hex(lib)?;
    if recorded != actual {
        return Err(format!(
            "{} does not match {} (sha256 {actual}, recorded {recorded}): it was rebuilt or changed outside `cargo xtask build-ghostty`",
            lib.display(),
            info.display()
        ));
    }
    Ok(())
}
