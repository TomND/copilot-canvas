//! Egress (plan VM-36; worker P-egress, docs/VM-WORKERS.md): the pack's
//! `egress/<name>.yaml` profiles rendered into a box's [`EgressPolicy`] at its
//! claim, the rule engine every egress point applies, and the stand-in
//! forward proxy that enforces a policy for `vm::local` sandboxes (the
//! platform enforces it for `vm::aca`).
//!
//! This is the interface the lead wires: [`render()`] (the profile's format:
//! [`render`](mod@render)), [`decide`] (the engine: [`rules`]) and [`serve`]
//! (the forward proxy: module `proxy`), and the stand-in of a `vm::local`
//! sandbox ([`standin()`], [`Standin`], [`proxy_env`]).

use std::net::TcpListener;
use std::sync::Arc;

use anyhow::Result;

use super::pack::Pack;
use super::provider::{EgressDecision, EgressPolicy, HeaderFromSecret, Secret};

mod proxy;
pub mod render;
pub mod rules;
pub mod standin;

pub use render::hub_host;
pub use standin::{Standin, proxy_env, standin};

/// What a profile's placeholders are filled with at a claim: `{{box.branch}}`,
/// `{{box.repo}}` (the pack's repository key), `{{box.id}}`, `{{sandbox}}`,
/// `{{hub.host}}` (the host of the pack's `tunnel.url`). Header values come
/// only from `{{secret:NAME}}` (the pack's `secrets.yaml`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Vars {
    pub branch: String,
    pub repo: String,
    pub r#box: String,
    pub sandbox: String,
    pub hub_host: String,
}

/// Renders profile `name` of the pack for a box: its placeholders filled
/// from `vars` (an empty `hub_host`: [`hub_host`] of the pack), header values
/// as references to secrets. Errors name the profile, the rule and the
/// placeholder, never a value.
pub fn render(pack: &Pack, name: &str, vars: &Vars) -> Result<EgressPolicy> {
    render::render(pack, name, vars)
}

/// An outbound request as an egress point sees it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Request {
    pub host: String,
    pub port: u16,
    /// `GET`, `POST`, …; `CONNECT` for a tunnel (then there is no path).
    pub method: String,
    pub path: Option<String>,
}

/// A policy's answer for a request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decision {
    pub allowed: bool,
    /// The rule that decided (`None`: the default action).
    pub rule: Option<String>,
    /// Headers a `Transform` rule sets (values from secrets the egress point holds).
    pub headers: Vec<HeaderFromSecret>,
}

/// The first rule that matches decides; none: the policy's default action
/// (how hosts, paths and methods match, and what a `CONNECT` matches: [`rules`]).
pub fn decide(policy: &EgressPolicy, request: &Request) -> Decision {
    rules::decide(policy, request)
}

/// The secrets an egress point injects (never the guest's).
pub trait Secrets: Send + Sync {
    fn value(&self, name: &str, key: &str) -> Option<Secret>;
}

/// Where an egress point records each decision (`Provider::egress_decisions` reads them).
pub trait Recorder: Send + Sync {
    fn record(&self, decision: EgressDecision);
}

/// Serves a forward proxy on `listener` that applies `policy()` (read for
/// every request, so a changed policy applies at once): absolute-form
/// requests and `Upgrade`s by host, path and method with `Transform`
/// headers injected; `CONNECT` by host only.
/// One thread per connection; it returns only when the listener keeps
/// failing (the module `proxy` says what it relays and how it is bounded).
pub fn serve(listener: TcpListener, policy: Arc<dyn Fn() -> Option<EgressPolicy> + Send + Sync>, secrets: Arc<dyn Secrets>, recorder: Arc<dyn Recorder>) -> Result<()> {
    proxy::serve(listener, policy, secrets, recorder)
}

