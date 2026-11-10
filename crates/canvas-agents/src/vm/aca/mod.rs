//! `vm::aca`: Azure Container Apps Sandboxes as a provider, through the data
//! plane's REST API (docs/VM-SESSIONS-PLAN.md Appendix A) with a cached
//! bearer token ([`token`]); no CLI on the hot path.

pub mod model;
pub mod rest;
pub mod token;

pub use self::token::{Cache, CommandSource, Token, TokenSource};
