//! Connection to a running copilot-canvas service instance and the spike's
//! terminals. Reuses `canvas_core::conn::try_connect` (pipe + HMAC handshake,
//! reader/writer threads); never `connect(start = true)`, which would copy
//! this executable as the service.

#![allow(dead_code)]

use std::path::PathBuf;

use anyhow::{Context, Result};
use canvas_common::paths::Instance;
use canvas_common::protocol::{ClientMsg, Frame, Input, LaunchKind, ServiceMsg, TermInfo};
use crossbeam_channel::Sender;

use crate::term::{COLS, ROWS};

pub enum SvcEvent {
    Frame(Frame),
    Terminals(Vec<TermInfo>),
    Error(String),
    Closed,
}

pub struct Ids {
    pub noise: [String; 4],
    pub shell: [String; 4],
    pub echo: String,
}

impl Ids {
    pub fn new() -> Self {
        let pid = std::process::id();
        Ids {
            noise: std::array::from_fn(|i| format!("spike{pid}-noise-{i}")),
            shell: std::array::from_fn(|i| format!("spike{pid}-shell-{i}")),
            echo: format!("spike{pid}-echo"),
        }
    }
    pub fn all(&self) -> Vec<String> {
        self.noise.iter().chain(self.shell.iter()).chain(std::iter::once(&self.echo)).cloned().collect()
    }
}

pub struct Service {
    tx: Sender<ClientMsg>,
    pub ids: Ids,
    pub build: String,
}

/// The copilot-canvas executable providing `noise` and `echo` (CANVAS_EXE, or
/// `target\release\copilot-canvas.exe` in an ancestor of this executable).
pub fn canvas_exe() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CANVAS_EXE") {
        return Ok(PathBuf::from(path));
    }
    let exe = std::env::current_exe()?;
    for dir in exe.ancestors().skip(1) {
        let candidate = dir.join("target").join("release").join("copilot-canvas.exe");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("copilot-canvas.exe not found; build the root package in release mode or set CANVAS_EXE")
}

fn powershell() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    format!(r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe")
}

impl Service {
    /// Connects to the instance named by SPIKE_INSTANCE (default "spike") and
    /// forwards its messages to `on_event` from a reader thread.
    pub fn connect(mut on_event: impl FnMut(SvcEvent) + Send + 'static) -> Result<Self> {
        let name = std::env::var("SPIKE_INSTANCE").unwrap_or_else(|_| "spike".into());
        let instance = Instance::new(Some(name.clone()), None, None)?;
        let conn = canvas_core::conn::try_connect(&instance)
            .with_context(|| format!("no service instance '{name}'; start it with `copilot-canvas service start --instance {name}`"))?;
        let rx = conn.rx;
        std::thread::Builder::new().name("spike-service".into()).spawn(move || {
            for message in rx {
                match message {
                    ServiceMsg::Frame(frame) => on_event(SvcEvent::Frame(frame)),
                    ServiceMsg::Frame8(frame) => on_event(SvcEvent::Frame(frame.to_v7())),
                    ServiceMsg::Terminals(infos) => on_event(SvcEvent::Terminals(infos)),
                    ServiceMsg::Error { message, .. } => on_event(SvcEvent::Error(message)),
                    _ => {}
                }
            }
            on_event(SvcEvent::Closed);
        })?;
        Ok(Service { tx: conn.tx, ids: Ids::new(), build: conn.service_build })
    }

    /// Launches 4 `noise` generators, 4 PowerShell prompts and an `echo`
    /// terminal, all 200×60.
    pub fn launch_all(&self) -> Result<()> {
        let exe = canvas_exe()?.to_string_lossy().into_owned();
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let launch = |id: &str, program: &str, args: &[&str]| {
            let kind = LaunchKind::Command { program: program.to_owned(), args: args.iter().map(|s| s.to_string()).collect() };
            self.tx.send(ClientMsg::Launch { id: id.to_owned(), kind, cwd: cwd.clone(), cols: COLS, rows: ROWS })
        };
        for id in &self.ids.noise {
            launch(id, &exe, &["noise", "--rate", "20"])?;
        }
        let ps = powershell();
        for id in &self.ids.shell {
            launch(id, &ps, &["-NoLogo", "-NoProfile"])?;
        }
        launch(&self.ids.echo, &exe, &["echo"])?;
        Ok(())
    }

    pub fn subscribe(&self, ids: &[String]) {
        let _ = self.tx.send(ClientMsg::Subscribe { ids: ids.to_vec() });
    }

    pub fn input(&self, id: &str, input: Input) {
        let _ = self.tx.send(ClientMsg::Input { id: id.to_owned(), input });
    }

    /// Stops this process's terminals (the service keeps running).
    pub fn stop_all(&self) {
        let _ = self.tx.send(ClientMsg::Subscribe { ids: Vec::new() });
        for id in self.ids.all() {
            let _ = self.tx.send(ClientMsg::Stop { id, force: true });
        }
    }
}
