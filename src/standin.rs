//! `copilot-canvas devtunnel-standin connect TUNNEL` (hidden): stands in for
//! `devtunnel connect` where the Dev Tunnels CLI cannot sign in (tests). It
//! forwards a local port to `COPILOT_CANVAS_STANDIN_TARGET` (`HOST:PORT`, the
//! service's loopback port) and prints what `devtunnel connect` prints.
//! `COPILOT_CANVAS_STANDIN_PORT` picks the local port (default: a free one,
//! like `devtunnel connect` when the tunnel's port is taken). With
//! `COPILOT_CANVAS_STANDIN_CONTROL=DIR` it writes `DIR\standin.pid` and drops
//! every connection when a `DIR\drop` file appears (and deletes it).
//!
//! Clients use it through `COPILOT_CANVAS_DEVTUNNEL="<copilot-canvas.exe>|devtunnel-standin"`.

use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const TARGET_ENV: &str = "COPILOT_CANVAS_STANDIN_TARGET";
pub const PORT_ENV: &str = "COPILOT_CANVAS_STANDIN_PORT";
pub const CONTROL_ENV: &str = "COPILOT_CANVAS_STANDIN_CONTROL";

type Links = Arc<Mutex<Vec<TcpStream>>>;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Copies one direction, then ends both.
fn pipe(mut from: TcpStream, mut to: TcpStream) {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut from, &mut to);
        let _ = from.shutdown(Shutdown::Both);
        let _ = to.shutdown(Shutdown::Both);
    });
}

pub fn run(args: &[&str]) -> Result<()> {
    let [command, tunnel, ..] = args else { bail!("usage: copilot-canvas devtunnel-standin connect TUNNEL") };
    anyhow::ensure!(*command == "connect", "the stand-in only does `connect`");
    let target = std::env::var(TARGET_ENV).with_context(|| format!("set {TARGET_ENV}=HOST:PORT (where the tunnel's port goes)"))?;
    let host_port: u16 = target.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).context("the target is HOST:PORT")?;
    let local: u16 = std::env::var(PORT_ENV).ok().and_then(|p| p.parse().ok()).unwrap_or(0);
    let listener = TcpListener::bind(("127.0.0.1", local)).context("cannot listen")?;
    let port = listener.local_addr()?.port();
    let links: Links = Arc::default();
    if let Some(control) = std::env::var_os(CONTROL_ENV).map(PathBuf::from) {
        std::fs::create_dir_all(&control)?;
        std::fs::write(control.join("standin.pid"), std::process::id().to_string())?;
        let links = links.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                if std::fs::remove_file(control.join("drop")).is_ok() {
                    for link in lock(&links).drain(..) {
                        let _ = link.shutdown(Shutdown::Both);
                    }
                }
            }
        });
    }
    // What `devtunnel connect` 1.0.2115 prints once it forwards (its binary's strings).
    println!("Connected to tunnel: {tunnel}.standin");
    println!("Forwarding from 127.0.0.1:{port} to host port {host_port}.");
    for client in listener.incoming().flatten() {
        let Ok(upstream) = TcpStream::connect(&target) else { continue };
        let _ = (client.set_nodelay(true), upstream.set_nodelay(true));
        let (Ok(c2), Ok(u2), Ok(c3), Ok(u3)) = (client.try_clone(), upstream.try_clone(), client.try_clone(), upstream.try_clone()) else { continue };
        lock(&links).extend([c3, u3]);
        pipe(client, upstream);
        pipe(u2, c2);
    }
    Ok(())
}
