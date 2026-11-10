//! Egress (plan VM-36; worker P-egress, docs/VM-WORKERS.md): the pack's
//! `egress/<name>.yaml` profiles rendered into a box's [`EgressPolicy`] at its
//! claim, the rule engine every egress point applies, and the stand-in
//! forward proxy that enforces a policy for `vm::local` sandboxes (the
//! platform enforces it for `vm::aca`).
//!
//! This is the interface the lead wires; until the worker's package lands,
//! [`decide`] denies everything and [`render`] and [`serve`] refuse.

use std::net::TcpListener;
use std::sync::Arc;

use anyhow::{Result, bail};

use super::pack::Pack;
use super::provider::{EgressDecision, EgressPolicy, HeaderFromSecret, Secret};

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

/// Renders profile `name` of the pack for a box.
pub fn render(pack: &Pack, name: &str, vars: &Vars) -> Result<EgressPolicy> {
    let _ = (pack, vars);
    bail!("egress profile {name}: rendering is not available yet (Phase 3)")
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

/// The first rule that matches decides; none: the policy's default action.
pub fn decide(policy: &EgressPolicy, request: &Request) -> Decision {
    let _ = (policy, request);
    Decision::default()
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
pub fn serve(listener: TcpListener, policy: Arc<dyn Fn() -> Option<EgressPolicy> + Send + Sync>, secrets: Arc<dyn Secrets>, recorder: Arc<dyn Recorder>) -> Result<()> {
    let _ = (listener, policy, secrets, recorder);
    bail!("the egress stand-in is not available yet (Phase 3)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn until_the_engine_lands_everything_is_denied() {
        let policy = EgressPolicy { default_action: "Allow".into(), rules: Vec::new() };
        assert!(!decide(&policy, &Request { host: "example.com".into(), port: 443, method: "GET".into(), path: Some("/".into()) }).allowed);
    }
}
