//! A provider in memory for the manager's and the launcher's tests.

use std::collections::BTreeMap;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use super::provider::*;

/// A sandbox: its labels, state and files.
type Fake = (Labels, String, BTreeMap<String, Vec<u8>>);

#[derive(Default)]
struct State {
    sandboxes: BTreeMap<String, Fake>,
    calls: Vec<String>,
    fail: BTreeMap<String, u32>,
    next: u32,
    images: Vec<ImageRef>,
}

pub struct FakeProvider {
    name: &'static str,
    state: Mutex<State>,
    on_create: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl FakeProvider {
    pub fn new(name: &'static str) -> Self {
        FakeProvider { name, state: Mutex::default(), on_create: Mutex::new(None) }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// The next `times` calls of `op` fail.
    pub fn fail(&self, op: &str, times: u32) {
        self.state().fail.insert(op.into(), times);
    }

    pub fn on_create(&self, hook: Box<dyn Fn() + Send>) {
        *self.on_create.lock().unwrap() = Some(hook);
    }

    pub fn add(&self, id: &str, labels: Labels) {
        self.state().sandboxes.insert(id.into(), (labels, "Running".into(), BTreeMap::new()));
    }

    pub fn exists(&self, id: &str) -> bool {
        self.state().sandboxes.contains_key(id)
    }

    pub fn live(&self) -> Vec<String> {
        self.state().sandboxes.keys().cloned().collect()
    }

    pub fn calls(&self) -> Vec<String> {
        self.state().calls.clone()
    }

    pub fn files(&self, id: &str) -> Vec<String> {
        self.state().sandboxes.get(id).map(|s| s.2.keys().cloned().collect()).unwrap_or_default()
    }

    fn call(&self, what: String) -> Result<()> {
        let mut state = self.state();
        let op = what.split(' ').next().unwrap_or_default().to_owned();
        state.calls.push(what);
        if let Some(left) = state.fail.get_mut(&op).filter(|n| **n > 0) {
            *left -= 1;
            bail!("{op} failed (injected)");
        }
        Ok(())
    }

    fn with<T>(&self, id: &str, f: impl FnOnce(&mut Fake) -> T) -> Result<T> {
        let mut state = self.state();
        match state.sandboxes.get_mut(id) {
            Some(sandbox) => Ok(f(sandbox)),
            None => bail!("no sandbox {id}"),
        }
    }
}

impl Provider for FakeProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn create(&self, spec: &SandboxSpec) -> Result<Sandbox> {
        self.call(format!("create {:?}", spec.image))?;
        let id = {
            let mut state = self.state();
            state.next += 1;
            let id = format!("sbx-{:04}", state.next);
            state.sandboxes.insert(id.clone(), (spec.labels.clone(), "Running".into(), BTreeMap::new()));
            id
        };
        if let Some(hook) = self.on_create.lock().unwrap().as_ref() {
            hook();
        }
        Ok(Sandbox { id, state: "Running".into(), labels: spec.labels.clone() })
    }

    fn get(&self, id: &str) -> Result<Option<Sandbox>> {
        Ok(self.state().sandboxes.get(id).map(|(labels, state, _)| Sandbox { id: id.into(), state: state.clone(), labels: labels.clone() }))
    }

    fn list(&self, labels: &Labels) -> Result<Vec<Sandbox>> {
        Ok(self.state().sandboxes.iter().filter(|(_, (l, _, _))| labels.iter().all(|(k, v)| l.get(k) == Some(v))).map(|(id, (l, s, _))| Sandbox { id: id.clone(), state: s.clone(), labels: l.clone() }).collect())
    }

    fn exec(&self, id: &str, argv: &[String], _stdin: &[u8]) -> Result<ExecResult> {
        self.call(format!("exec {id} {}", argv.join(" ")))?;
        self.with(id, |_| ())?;
        let stdout = if argv.iter().any(|a| a == "--prepare") {
            let repo = argv.windows(2).find(|w| w[0] == "--repo").map(|w| w[1].clone()).unwrap_or_default();
            format!(r#"{{"ok": true, "path": "/home/agent/work/{repo}"}}"#).into_bytes()
        } else {
            b"{\"ok\": true}".to_vec()
        };
        Ok(ExecResult { code: 0, stdout, stderr: String::new() })
    }

    fn put_file(&self, id: &str, path: &str, bytes: &[u8], _mode: u32) -> Result<()> {
        self.call(format!("put_file {id} {path}"))?;
        self.with(id, |s| {
            s.2.insert(path.into(), bytes.to_vec());
        })
    }

    fn get_file(&self, id: &str, path: &str, max: u64) -> Result<Vec<u8>> {
        let bytes = self.with(id, |s| s.2.get(path).cloned())?.with_context(|| format!("no {path} in {id}"))?;
        anyhow::ensure!(bytes.len() as u64 <= max, "{path} in {id} is larger than {max} bytes");
        Ok(bytes)
    }

    fn suspend(&self, id: &str) -> Result<()> {
        self.call(format!("suspend {id}"))?;
        self.with(id, |s| s.1 = "Suspended".into())
    }

    fn resume(&self, id: &str) -> Result<()> {
        self.call(format!("resume {id}"))?;
        self.with(id, |s| s.1 = "Running".into())
    }

    fn delete(&self, id: &str) -> Result<()> {
        self.call(format!("delete {id}"))?;
        self.state().sandboxes.remove(id);
        Ok(())
    }

    fn set_egress(&self, id: &str, _policy: &EgressPolicy) -> Result<()> {
        self.call(format!("set_egress {id}"))
    }

    fn egress_decisions(&self, _id: &str) -> Result<Vec<EgressDecision>> {
        Ok(Vec::new())
    }

    fn image_build(&self, base: &str, name: &str) -> Result<ImageRef> {
        self.call(format!("image_build {base} {name}"))?;
        let image = ImageRef { id: format!("img-{name}"), name: name.into(), created_ms: 1 };
        self.state().images.push(image.clone());
        Ok(image)
    }

    fn image_commit(&self, id: &str, name: &str) -> Result<ImageRef> {
        self.call(format!("image_commit {id} {name}"))?;
        let image = ImageRef { id: format!("img-{name}"), name: name.into(), created_ms: 2 };
        self.state().images.push(image.clone());
        Ok(image)
    }

    fn image_delete(&self, image: &ImageRef) -> Result<()> {
        self.call(format!("image_delete {}", image.id))?;
        self.state().images.retain(|i| i.id != image.id);
        Ok(())
    }

    fn images(&self) -> Result<Vec<ImageRef>> {
        Ok(self.state().images.clone())
    }

    fn secret_set(&self, name: &str, _key: &str, _value: &Secret) -> Result<()> {
        self.call(format!("secret_set {name}"))
    }
}
