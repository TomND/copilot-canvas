//! Workspace automation, run as `cargo xtask <command>`.

use std::process::ExitCode;

mod ghostty;

const USAGE: &str = "usage: cargo xtask <command>

commands:
  build-ghostty [--force]          build the pinned libghostty-vt static library into
                                   target/ghostty/<commit>/ (skipped when up to date)
  vendor-ghostty [--checksums-only] copy the built headers and LICENSE into
                                   crates/ghostty-vt-sys/vendor and rewrite
                                   vendor/CHECKSUMS.sha256 (after a pin bump or a
                                   bindings.rs edit)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().skip(1).any(|a| a == name);
    let result = match args.first().map(String::as_str) {
        Some("build-ghostty") => ghostty::build(flag("--force")).map(|_| ()),
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
