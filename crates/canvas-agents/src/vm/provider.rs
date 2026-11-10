//! `vm::Provider` (docs/VM-SESSIONS.md §4.4): what the manager asks of a
//! sandbox platform. `vm::aca` implements it with Azure Container Apps
//! Sandboxes' data-plane REST API, `vm::local` with local folders and
//! processes (tests and CI), `vm::hooks` with the pack's hooks (the thin
//! lane). Every call is blocking and bounded; the manager calls them on its
//! own thread, never on a client's.
//!
//! Secrets cross this boundary only as [`Secret`] (zeroized when dropped,
//! never printed), and only to `secret_set`.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Labels on a sandbox (`atelier.hub`, `atelier.box`, `atelier.repo`, `atelier.role`).
pub type Labels = BTreeMap<String, String>;

/// What a sandbox is made from and how large it is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SandboxSpec {
    /// The disk image (provider id), or `None` for the provider's stock image.
    pub image: Option<String>,
    pub cpu_milli: u32,
    pub memory_mi: u32,
    pub disk_gi: u32,
    pub labels: Labels,
    /// Plain environment (never a secret: secrets reach a guest only
    /// through egress injection or files written after a claim).
    pub env: Vec<(String, String)>,
    /// The egress policy at creation (deny by default).
    pub egress: Option<EgressPolicy>,
}

/// A sandbox as the provider reports it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Sandbox {
    pub id: String,
    /// `Creating`, `Running`, `Stopping`, `Stopped`, `Suspended`, `Resuming`, `Deleting`, `Failed`.
    pub state: String,
    pub labels: Labels,
}

impl Sandbox {
    pub fn running(&self) -> bool {
        self.state.eq_ignore_ascii_case("running")
    }

    pub fn suspended(&self) -> bool {
        matches!(self.state.to_ascii_lowercase().as_str(), "stopped" | "suspended")
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecResult {
    pub code: i64,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// An egress policy (rendered from the pack's `egress/<profile>.yaml`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct EgressPolicy {
    /// `Allow` or `Deny`.
    pub default_action: String,
    pub rules: Vec<EgressRule>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct EgressRule {
    pub name: String,
    pub host: String,
    pub path: Option<String>,
    pub methods: Vec<String>,
    /// `Allow`, `Deny` or `Transform`.
    pub action: String,
    /// Headers a `Transform` sets: (header, secret name, key in the secret, format with `{}` for the value).
    pub headers: Vec<HeaderFromSecret>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct HeaderFromSecret {
    pub name: String,
    pub secret: String,
    pub key: String,
    /// `Bearer {}`: the value's place in the header.
    pub format: String,
}

/// A refused or allowed outbound call the platform recorded.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct EgressDecision {
    pub ms: u64,
    pub host: String,
    pub method: String,
    pub path: String,
    pub allowed: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ImageRef {
    pub id: String,
    pub name: String,
    pub created_ms: u64,
}

/// A secret value: zeroized when dropped, never shown.
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Self {
        Secret(bytes)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        for byte in self.0.iter_mut() {
            // A volatile write the optimizer cannot drop.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(… {} bytes)", self.0.len())
    }
}

/// A sandbox platform.
pub trait Provider: Send + Sync {
    /// `aca`, `local`, `hooks`.
    fn name(&self) -> &'static str;
    /// A new sandbox, running when this returns (or an error).
    fn create(&self, spec: &SandboxSpec) -> Result<Sandbox>;
    fn get(&self, id: &str) -> Result<Option<Sandbox>>;
    /// Sandboxes whose labels include every one of `labels`.
    fn list(&self, labels: &Labels) -> Result<Vec<Sandbox>>;
    /// Runs a command (`argv`, no shell) to its end.
    fn exec(&self, id: &str, argv: &[String], stdin: &[u8]) -> Result<ExecResult>;
    fn put_file(&self, id: &str, path: &str, bytes: &[u8]) -> Result<()>;
    fn get_file(&self, id: &str, path: &str, max: u64) -> Result<Vec<u8>>;
    /// Suspends with memory (the platform's own idle suspend stays off: §5.9 of the plan).
    fn suspend(&self, id: &str) -> Result<()>;
    fn resume(&self, id: &str) -> Result<()>;
    fn delete(&self, id: &str) -> Result<()>;
    fn set_egress(&self, id: &str, policy: &EgressPolicy) -> Result<()>;
    fn egress_decisions(&self, id: &str) -> Result<Vec<EgressDecision>>;
    /// An image of `base` (an OCI image or the stock one).
    fn image_build(&self, base: &str, name: &str) -> Result<ImageRef>;
    /// An image of a running sandbox's disk.
    fn image_commit(&self, id: &str, name: &str) -> Result<ImageRef>;
    fn image_delete(&self, image: &ImageRef) -> Result<()>;
    fn images(&self) -> Result<Vec<ImageRef>>;
    /// Sets a group secret's key (for egress injection).
    fn secret_set(&self, name: &str, key: &str, value: &Secret) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_show() {
        let secret = Secret::new(b"hunter2".to_vec());
        assert_eq!(format!("{secret:?}"), "Secret(… 7 bytes)");
        assert_eq!(secret.expose(), b"hunter2");
        let sandbox = Sandbox { id: "s".into(), state: "Suspended".into(), labels: Labels::new() };
        assert!(sandbox.suspended() && !sandbox.running());
    }
}
