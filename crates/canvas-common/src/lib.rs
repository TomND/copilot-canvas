//! Shared vocabulary of every layer: the wire protocol, the local transport,
//! instance paths and Windows process utilities.
//!
//! The protocol, framing, handshake, pairing and TCP modules are portable
//! (the iPhone client and the macOS mock service use them); the named pipe,
//! DPAPI, service token files and Win32 process utilities are Windows-only.

#[cfg(windows)]
pub mod auth;
pub mod clipimage;
#[cfg(windows)]
pub mod dpapi;
pub mod handshake;
pub mod pairing;
pub mod paths;
#[cfg(windows)]
pub mod pipe;
pub mod protocol;
pub mod remote;
pub mod settings;
pub mod sizing;
pub mod tcp;
#[cfg(windows)]
pub mod win;
pub mod wire;
pub mod ws;
