//! The iPhone client's core (docs/IOS-PLAN.md D1): everything that is not
//! drawing or the WebSocket itself. The app (Swift) opens a WebSocket to the
//! service's Dev Tunnels web-forwarding address and hands it to a [`Client`]:
//! the core authenticates as the paired phone, keeps the shared canvas,
//! session summaries and terminal screens up to date, derives the statuses
//! the desktop canvas shows, and turns taps and keys into protocol messages.
//!
//! It reuses the desktop clients' code: the wire protocol, framing and
//! handshake (`canvas-common`), the canvas replica, status rules and launch
//! builder (`canvas-model`) and the terminal view (`canvas-termview`).

uniffi::setup_scaffolding!();

mod client;
mod link;
mod model;
mod screen;
mod state;

pub use client::{Client, ClientConfig, Listener, Pairing};
pub use link::{Inbox, InboxReader, Transport, TransportWriter};
pub use model::*;
pub use state::{Effect, State};

/// Reads a scanned `ccanvas://pair?…` link.
#[uniffi::export]
pub fn parse_pair_link(uri: String) -> Result<PairLinkInfo, CoreError> {
    let link = canvas_common::remote::PairLink::parse(&uri)?;
    Ok(PairLinkInfo { name: link.name, url: link.url, code: link.code, token: link.token, token_expires_ms: link.token_expires_ms })
}

/// A typed pairing code in canonical form (`None`: not a code).
#[uniffi::export]
pub fn normalize_pairing_code(code: String) -> Option<String> {
    canvas_common::pairing::normalize_code(&code)
}

/// When a Dev Tunnels access token expires (ms since the epoch; 0: unknown).
#[uniffi::export]
pub fn token_expiry_ms(token: String) -> u64 {
    canvas_common::remote::jwt_claims(&token).and_then(|c| c.get("exp").and_then(|e| e.as_u64())).map_or(0, |s| s * 1000)
}

/// The core's version (the service logs it as the client's build).
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}
