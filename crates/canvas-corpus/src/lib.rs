//! Golden PTY corpus (PLAN.md §5 Phase 0, §10).
//!
//! Recorded and synthetic terminal output streams ([`vtrec`]) are replayed
//! into every available terminal engine ([`replay`]) and compared cell by
//! cell with reviewed expectations kept next to them ([`store`]). The
//! recorder ([`record`]) captures real programs in a ConPTY; [`synth`]
//! generates escape-sequence coverage streams. Layout, commands and the
//! review policy: `corpus/README.md`.

use canvas_host::emulator::Engine;

pub mod cli;
pub mod record;
pub mod replay;
pub mod store;
pub mod synth;
pub mod vtrec;

/// Every engine the corpus knows; [`available_engines`] filters by build.
pub const ENGINES: [Engine; 2] = [Engine::Alacritty, Engine::Ghostty];

/// The engines this build includes (`ghostty` needs the cargo feature).
pub fn available_engines() -> Vec<Engine> {
    ENGINES.into_iter().filter(|engine| engine.available()).collect()
}

/// Parses `--engine NAME`, or every available engine when `None`.
pub fn engines(name: Option<&str>) -> anyhow::Result<Vec<Engine>> {
    let Some(name) = name else {
        return Ok(available_engines());
    };
    let engine = Engine::parse(name).ok_or_else(|| anyhow::anyhow!("unknown engine {name:?} (alacritty, ghostty)"))?;
    anyhow::ensure!(engine.available(), "the {} engine is not in this build (cargo feature `ghostty`)", engine.name());
    Ok(vec![engine])
}
