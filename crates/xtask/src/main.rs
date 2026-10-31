//! Workspace automation, run as `cargo xtask <command>`.

use std::process::ExitCode;

mod ghostty;

const USAGE: &str = "usage: cargo xtask <command>

commands:
  build-ghostty [--force] [--target TRIPLE]
                                   build the pinned libghostty-vt static library into
                                   target/ghostty/<commit>/ (Windows) or
                                   target/ghostty/<commit>/<triple>/ (macOS, Linux:
                                   aarch64/x86_64-apple-darwin, x86_64/aarch64-unknown-linux-gnu);
                                   skipped when up to date; default: this machine
  vendor-ghostty [--checksums-only] copy the built headers and LICENSE into
                                   crates/ghostty-vt-sys/vendor and rewrite
                                   vendor/CHECKSUMS.sha256 (after a pin bump or a
                                   bindings.rs edit)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().skip(1).any(|a| a == name);
    let result = match args.first().map(String::as_str) {
        Some("build-ghostty") => {
            let target = match args.iter().position(|a| a == "--target").map(|i| args.get(i + 1)) {
                Some(Some(triple)) => ghostty::Target::of(triple).ok_or_else(|| anyhow::anyhow!("libghostty-vt is not built for {triple}")),
                Some(None) => Err(anyhow::anyhow!("--target needs a target triple")),
                None => ghostty::Target::host(),
            };
            target.and_then(|t| ghostty::build(flag("--force"), &t)).map(|_| ())
        }
        Some("vendor-ghostty") => ghostty::vendor(flag("--checksums-only")),
        Some("help" | "-h" | "--help") | None => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("unknown command `{other}`\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
