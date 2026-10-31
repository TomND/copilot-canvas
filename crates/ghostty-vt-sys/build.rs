// Links the pinned libghostty-vt static library.
//
// The library is built outside Cargo by `cargo xtask build-ghostty` (Zig
// builds take minutes and fetch sources, which a build script should not do
// implicitly). It is looked up in `<workspace>/target/ghostty/<commit>/lib`
// for the commit pinned in `vendor/CHECKSUMS.sha256` (Windows MSVC), or in
// `<workspace>/target/ghostty/<commit>/<target triple>/lib` for macOS and
// Linux (`cargo xtask build-ghostty --target TRIPLE`); its `BUILD-INFO.txt`
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
const STATIC_LIB_UNIX: &str = "libghostty-vt.a";

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
    let windows = target == "x86_64-pc-windows-msvc";
    let unix = matches!(target.as_str(), "aarch64-apple-darwin" | "x86_64-apple-darwin" | "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu");
    if !windows && !unix {
        panic!("ghostty-vt-sys links libghostty-vt built for x86_64 Windows (MSVC), macOS or Linux (glibc); target {target} is not supported");
    }
    let lib_name = if windows { STATIC_LIB } else { STATIC_LIB_UNIX };

    let (dir, from_env) = match env::var_os("GHOSTTY_VT_LIB_DIR") {
        Some(dir) => (PathBuf::from(dir), true),
        None => {
            let base = manifest.ancestors().nth(2).expect("workspace root").join("target").join("ghostty").join(&commit);
            (if windows { base } else { base.join(&target) }.join("lib"), false)
        }
    };
    let lib = dir.join(lib_name);
    if !lib.is_file() {
        let hint = if from_env {
            "GHOSTTY_VT_LIB_DIR does not contain it".to_owned()
        } else {
            format!("build it with `cargo xtask build-ghostty --target {target}` (needs git and Zig {zig})")
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
            panic!("\n\n{error}\nrun `cargo xtask build-ghostty --force --target {target}`\n\n");
        }
    }
    if windows {
        println!("cargo:rustc-link-search=native={}", dir.display());
        // Zig installs the archive under this name so it does not collide with the
        // DLL import library `ghostty-vt.lib`; `+verbatim` links exactly this file.
        println!("cargo:rustc-link-lib=static:+verbatim={STATIC_LIB}");
        // The Zig standard library inside the archive calls NT and kernel32 APIs.
        println!("cargo:rustc-link-lib=dylib=ntdll");
        println!("cargo:rustc-link-lib=dylib=kernel32");
    } else {
        // The install folder also holds the shared library, which a Unix
        // linker would prefer for `-lghostty-vt`: link from a folder that
        // holds only the archive.
        let only = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("ghostty-static");
        fs::create_dir_all(&only).unwrap_or_else(|e| panic!("cannot create {}: {e}", only.display()));
        fs::copy(&lib, only.join(STATIC_LIB_UNIX)).unwrap_or_else(|e| panic!("cannot copy {}: {e}", lib.display()));
        println!("cargo:rustc-link-search=native={}", only.display());
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }
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
    let lib_name = lib.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let text = fs::read_to_string(info).map_err(|_| format!("{} is missing (interrupted build?)", info.display()))?;
    let field = |key: &str| text.lines().find_map(|l| l.strip_prefix(key)).map(|v| v.trim().to_ascii_lowercase());
    if field("ghostty-commit:").as_deref() != Some(commit) {
        return Err(format!("{} is not a build of {commit}", info.display()));
    }
    let recorded = field(&format!("{lib_name} sha256:")).ok_or_else(|| format!("{} records no {lib_name} sha256", info.display()))?;
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
