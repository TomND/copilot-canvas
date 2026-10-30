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
mod reports;
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

/// Opens a renewed tunnel access token from a silent notification (`cc.n`,
/// `cc.t`), sealed by the service with this phone's pairing secret.
#[uniffi::export]
pub fn open_tunnel_access(secret: Vec<u8>, nonce: String, sealed: String) -> Result<TunnelAccessInfo, CoreError> {
    let (url, token, expires_ms) = canvas_common::remote::open_access(&secret, &nonce, &sealed)?;
    Ok(TunnelAccessInfo { url, token, expires_ms })
}

/// Whether a link from a report may open (`http(s)://host…` or `mailto:`).
#[uniffi::export]
pub fn report_link_allowed(url: String) -> bool {
    canvas_model::reports::check_url(&url).is_ok()
}

/// The page a report body is shown as (what `Client::read_report` gives
/// for a report of the service): sanitized HTML with the phone's look.
#[uniffi::export]
pub fn report_page_html(format: ReportFormat, body: String) -> String {
    let format = match format {
        ReportFormat::Message => canvas_model::reports::Format::Message,
        ReportFormat::Markdown => canvas_model::reports::Format::Markdown,
        ReportFormat::Html => canvas_model::reports::Format::Html,
    };
    reports::page(&canvas_model::reports::Content { id: String::new(), format, body }).html
}

/// The core's version (the service logs it as the client's build).
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}
