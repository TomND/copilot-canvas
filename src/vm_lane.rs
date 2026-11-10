//! A VM lane for the client suites (the self-test's VM step, the GUI parity
//! rows 57–62): a VM pack whose provider is `vm::local` (sandboxes as local
//! folders, each with its satellite), a git repository of its own to clone,
//! and the service started with what the lane needs (the satellites'
//! listener on loopback, the stand-in agent's history). No cloud, no model.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::client::conn;
use crate::paths::Instance;

/// The lane's repository key (the pack's catalog).
pub(crate) const REPO: &str = "vm-app";

pub(crate) struct Lane {
    pub(crate) pack: PathBuf,
    sandboxes: PathBuf,
    port: u16,
}

impl Lane {
    /// A lane under `root` whose repository is found from folders named
    /// `folder` (`None`: git is not installed here).
    pub(crate) fn new(root: &Path, folder: &str) -> Result<Option<Lane>> {
        std::fs::create_dir_all(root)?;
        let Some(url) = crate::vm_probe::bare_repo(root) else { return Ok(None) };
        let port = crate::vm_probe::free_port()?;
        let (pack, sandboxes) = (root.join("vm-pack"), root.join("vm-sandboxes"));
        std::fs::create_dir_all(pack.join("egress"))?;
        let json = |p: &Path| serde_json::to_string(&p.display().to_string());
        std::fs::write(
            pack.join("vm.yaml"),
            format!(
                "pack_version: 1\nname: lane\nprovider: local\npolicy_ack: \"2026-11-09\"\npoll_seconds: 1\nbudget:\n  max_running: 2\nsizes:\n  - name: S\n    cpu: 1000m\n    memory: 2048Mi\n    disk: 20Gi\nimages:\n  keep: 1\ntunnel:\n  url: ws://127.0.0.1:{port}\nlocal:\n  root: {}\n",
                json(&sandboxes)?
            ),
        )?;
        std::fs::write(pack.join("repos.yaml"), format!("repos:\n  {REPO}:\n    url: {}\n    default_branch: main\n    egress: lane\n    folders: [\"**/{folder}\"]\n", json(&url)?))?;
        std::fs::write(pack.join("egress").join("lane.yaml"), "default: deny\nrules:\n  - host: example.com\n    action: allow\n")?;
        Ok(Some(Lane { pack, sandboxes, port }))
    }

    /// What the service needs for the lane (added to the suite's own).
    pub(crate) fn env(&self) -> Vec<(String, String)> {
        vec![
            ("ATELIER_VM_PACK".into(), self.pack.display().to_string()),
            ("ATELIER_LISTEN".into(), format!("127.0.0.1:{}", self.port)),
            ("ATELIER_FAKE_HISTORY".into(), "1".into()),
            ("ATELIER_FAKE_TURN_MS".into(), "1500".into()),
            ("ATELIER_VM_STATS_SECS".into(), "2".into()),
        ]
    }

    /// Starts `instance`'s service with `env` and the lane's (the one running
    /// is stopped first) and waits for it.
    pub(crate) fn start_service(&self, exe: &Path, instance: &Instance, env: &[(String, String)]) -> Result<()> {
        crate::parity::stop_service(instance);
        let mut all: Vec<(String, String)> = env.to_vec();
        all.extend(self.env());
        let refs: Vec<(&str, &str)> = all.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        conn::spawn_service_exe(exe, instance, &refs)?;
        let deadline = Instant::now() + Duration::from_secs(20);
        while conn::try_connect(instance).is_err() {
            if Instant::now() > deadline {
                bail!("the service with the VM lane did not start");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    }

    /// The lane's sandboxes (`vm::local`).
    pub(crate) fn local(&self) -> canvas_agents::vm::local::Local {
        canvas_agents::vm::local::Local::new(self.sandboxes.clone())
    }

    /// Ends every sandbox's processes (whatever happened).
    pub(crate) fn clean(&self) {
        let local = self.local();
        for sandbox in local.list() {
            let _ = local.delete(&sandbox.id);
        }
    }

    /// Runs `atelier ARGS` against `instance` with the lane's pack (a hub
    /// command); its success and output.
    pub(crate) fn cli(&self, exe: &Path, instance: &Instance, args: &[&str]) -> Result<(bool, String)> {
        let env = [
            ("ATELIER_HOME".to_owned(), instance.home.display().to_string()),
            ("ATELIER_VM_PACK".to_owned(), self.pack.display().to_string()),
            (crate::vm_probe::PROBE_INSTANCE.to_owned(), instance.name.clone()),
        ];
        crate::vm_probe::cli(exe, args, &env)
    }
}

/// The intent of the canvas's VM box (the first one) in `instance`.
pub(crate) fn vm_box(instance: &Instance) -> Option<canvas_agents::vm::launch::Intent> {
    canvas_agents::vm::launch::all(&instance.home).into_iter().find(|i| i.state != canvas_model::vm::VmState::Deleted)
}
