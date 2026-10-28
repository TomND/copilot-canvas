//! Links the pinned libghostty-vt static library.
//!
//! The library is built outside Cargo by `cargo xtask build-ghostty` (Zig
//! builds take minutes and fetch sources, which a build script should not do
//! implicitly). It is looked up in `GHOSTTY_VT_LIB_DIR`, else in
//! `<workspace>/target/ghostty/<commit>/lib` for the commit pinned in
//! `vendor/CHECKSUMS.sha256`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const STATIC_LIB: &str = "ghostty-vt-static.lib";

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let checksums = manifest.join("vendor").join("CHECKSUMS.sha256");
    println!("cargo:rerun-if-changed={}", checksums.display());
    println!("cargo:rerun-if-env-changed=GHOSTTY_VT_LIB_DIR");

    let text = fs::read_to_string(&checksums).unwrap_or_else(|e| panic!("cannot read {}: {e}", checksums.display()));
    let pin = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix('#')?.trim().strip_prefix(key).map(|v| v.trim().to_owned()))
            .unwrap_or_else(|| panic!("{} has no `# {key}` line", checksums.display()))
    };
    let commit = pin("ghostty-commit:");
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
        panic!("\n\nlibghostty-vt {} not found: {}\n{hint}\n\n", &commit[..12], lib.display());
    }
    if !from_env {
        check_build_info(&dir, &commit);
    }
    println!("cargo:rerun-if-changed={}", lib.display());
    println!("cargo:rustc-link-search=native={}", dir.display());
    // Zig installs the archive under this name so it does not collide with the
    // DLL import library `ghostty-vt.lib`; `+verbatim` links exactly this file.
    println!("cargo:rustc-link-lib=static:+verbatim={STATIC_LIB}");
    // The Zig standard library inside the archive calls NT and kernel32 APIs.
    println!("cargo:rustc-link-lib=dylib=ntdll");
    println!("cargo:rustc-link-lib=dylib=kernel32");
}

/// A library from `cargo xtask build-ghostty` records the commit it was built from.
fn check_build_info(lib_dir: &Path, commit: &str) {
    let info = lib_dir.parent().map(|p| p.join("BUILD-INFO.txt")).unwrap_or_default();
    println!("cargo:rerun-if-changed={}", info.display());
    match fs::read_to_string(&info) {
        Ok(text) if text.lines().any(|l| l.trim() == format!("ghostty-commit: {commit}")) => {}
        Ok(_) => panic!("{} is not a build of {commit}; run `cargo xtask build-ghostty --force`", info.display()),
        Err(_) => panic!("{} is missing (interrupted build?); run `cargo xtask build-ghostty --force`", info.display()),
    }
}
