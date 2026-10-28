//! The backend of a service reached over the connection: requests go out as
//! `ClientMsg::Rpc`, answers come back as `ServiceMsg::RpcReply` and are
//! routed to the waiting caller by id on the connection's reader thread (so
//! a caller may even be the UI thread). A dropped connection fails the calls
//! waiting on it; the next connection carries new calls.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use crossbeam_channel::{Sender, bounded};

use crate::protocol::ClientMsg;
use crate::rpc::{Backend, Request, Response};

#[derive(Default)]
struct State {
    /// The current connection: (generation, its outgoing queue).
    link: Option<(u64, Sender<ClientMsg>)>,
    generation: u64,
    /// Calls waiting for an answer: id → (connection generation, reply slot).
    pending: HashMap<u64, (u64, Sender<Result<String, String>>)>,
}

#[derive(Default)]
pub struct RemoteBackend {
    next_id: AtomicU64,
    state: Mutex<State>,
}

impl RemoteBackend {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A connection is up: calls go through `tx` from now on. Returns its generation.
    pub fn attach(&self, tx: Sender<ClientMsg>) -> u64 {
        let mut state = self.state();
        state.generation += 1;
        let generation = state.generation;
        state.link = Some((generation, tx));
        generation
    }

    /// The connection of `generation` ended: the calls waiting on it fail.
    pub fn detach(&self, generation: u64) {
        let mut state = self.state();
        if state.link.as_ref().is_some_and(|(g, _)| *g == generation) {
            state.link = None;
        }
        let failed: Vec<u64> = state.pending.iter().filter(|(_, (g, _))| *g == generation).map(|(id, _)| *id).collect();
        for id in failed {
            if let Some((_, slot)) = state.pending.remove(&id) {
                let _ = slot.send(Err("the connection to the service was lost".into()));
            }
        }
    }

    /// An answer arrived (late answers to abandoned calls are dropped).
    pub fn deliver(&self, id: u64, json: String) {
        if let Some((_, slot)) = self.state().pending.remove(&id) {
            let _ = slot.send(Ok(json));
        }
    }

    /// Calls waiting for an answer.
    pub fn pending(&self) -> usize {
        self.state().pending.len()
    }
}

impl Backend for RemoteBackend {
    fn call(&self, request: Request) -> Result<Response> {
        let (name, timeout) = (request.name(), request.timeout());
        let json = serde_json::to_string(&request)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (slot, answer) = bounded(1);
        {
            let mut state = self.state();
            let Some((generation, link)) = state.link.clone() else { bail!("not connected to the service") };
            state.pending.insert(id, (generation, slot));
            if link.try_send(ClientMsg::Rpc { id, json }).is_err() {
                state.pending.remove(&id);
                bail!("not connected to the service");
            }
        }
        match answer.recv_timeout(timeout) {
            Ok(Ok(json)) => serde_json::from_str(&json).map_err(|e| anyhow!("unreadable answer to {name}: {e}")),
            Ok(Err(error)) => bail!(error),
            Err(_) => {
                self.state().pending.remove(&id);
                bail!("the service did not answer {name} in time")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::BackendInfo;
    use crossbeam_channel::unbounded;

    #[test]
    fn answers_reach_their_caller_and_a_lost_connection_fails_pending_calls() {
        let backend = RemoteBackend::new();
        let offline: &dyn Backend = &*backend;
        assert!(offline.info().unwrap_err().to_string().contains("not connected"));
        let (tx, rx) = unbounded();
        let generation = backend.attach(tx);
        // A fake service answering every request with its own id in the info.
        let service = {
            let backend = backend.clone();
            std::thread::spawn(move || {
                for message in rx {
                    if let ClientMsg::Rpc { id, json } = message {
                        let request: Request = serde_json::from_str(&json).unwrap();
                        let reply = match request {
                            Request::Info => Response::Info(BackendInfo { home: format!("call {id}"), ..Default::default() }),
                            _ => Response::Error("nope".into()),
                        };
                        backend.deliver(id, serde_json::to_string(&reply).unwrap());
                    }
                }
            })
        };
        let callers: Vec<_> = (0..8)
            .map(|_| {
                let backend = backend.clone();
                std::thread::spawn(move || {
                    let backend: &dyn Backend = &*backend;
                    backend.info().unwrap().home
                })
            })
            .collect();
        let mut homes: Vec<String> = callers.into_iter().map(|c| c.join().unwrap()).collect();
        homes.sort();
        homes.dedup();
        assert_eq!(homes.len(), 8, "each caller got its own answer");
        let remote: &dyn Backend = &*backend;
        assert_eq!(remote.list_dir(None).unwrap_err().to_string(), "nope");
        // A connection that never answers: its drop fails the call.
        let (silent, _keep) = unbounded();
        let generation2 = backend.attach(silent);
        backend.detach(generation);
        let waiting = {
            let backend = backend.clone();
            std::thread::spawn(move || {
                let backend: &dyn Backend = &*backend;
                backend.info()
            })
        };
        while backend.pending() == 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        backend.detach(generation2);
        assert!(waiting.join().unwrap().unwrap_err().to_string().contains("lost"));
        assert_eq!(backend.pending(), 0);
        drop(service);
    }
}
