//! The satellite's terminals (plan VM-23 `pty`): each one a local PTY with
//! the alacritty engine, which answers the program's queries here in the
//! guest (D6, D7). Its raw output goes into a [`Ring`] and, in order, onto
//! the link; a reconnect replays it from what the hub has, or sends the
//! screen when the ring no longer reaches back (a gap).
//!
//! One live generation per terminal id: a `Spawn` of a running terminal's
//! id with another generation is refused, the same generation again (a
//! repeat after a reconnect) is a no-op. Input is numbered by the hub
//! (`seq`, per generation) and applied once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use canvas_common::protocol::Input;
use canvas_common::sat::{ExitInfo, SAT_IO_CHUNK, SatUp, TermHeld};
use canvas_host::emulator::Engine;
use canvas_host::term::{Hooks, Spawn, Terminal};
use crossbeam_channel::{Sender, bounded, unbounded};

use super::ring::Ring;

/// Raw output kept per terminal for replays.
pub const RING: usize = 4 << 20;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The link as the terminals see it: terminal I/O goes on `io` (ahead of
/// everything else), the rest on `other`; `epoch` names this link.
#[derive(Clone)]
pub struct Lanes {
    pub io: Sender<SatUp>,
    pub other: Sender<SatUp>,
    pub epoch: u64,
}

/// What the hub asks to start.
pub struct SpawnRequest {
    pub term: String,
    pub generation: u64,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Default)]
pub struct Terms {
    terms: Mutex<HashMap<String, Arc<Term>>>,
    link: Mutex<Option<Lanes>>,
}

struct Term {
    id: String,
    generation: u64,
    terminal: Arc<Terminal>,
    state: Mutex<State>,
}

struct State {
    ring: Ring,
    /// The output offset sent up to on link `epoch` (`(epoch, offset)`):
    /// output goes out live only while it continues exactly from there.
    sent: Option<(u64, u64)>,
    input_acked: u64,
    /// The program's exit code and why (once it ended and its output was read).
    exit: Option<(u32, ExitInfo)>,
}

/// `+NAME` entries prepend to the variable (a `PATH` the hub composes);
/// the rest are passed on (`-NAME` with no value removes it: `Terminal::spawn`).
fn compose_env(env: &[(String, String)]) -> Vec<(String, String)> {
    let separator = if cfg!(windows) { ";" } else { ":" };
    env.iter()
        .map(|(key, value)| match key.strip_prefix('+') {
            Some(name) => {
                let old = std::env::var(name).unwrap_or_default();
                (name.to_owned(), if old.is_empty() { value.clone() } else { format!("{value}{separator}{old}") })
            }
            None => (key.clone(), value.clone()),
        })
        .collect()
}

fn send_output(lanes: &Lanes, term: &Term, seq: u64, bytes: &[u8]) {
    for (index, piece) in bytes.chunks(SAT_IO_CHUNK).enumerate() {
        let message = SatUp::Output { term: term.id.clone(), generation: term.generation, seq: seq + (index * SAT_IO_CHUNK) as u64, bytes: piece.to_vec() };
        if lanes.io.send(message).is_err() {
            return;
        }
    }
}

fn exit_message(term: &Term, code: u32, info: &ExitInfo) -> SatUp {
    SatUp::Exit { term: term.id.clone(), generation: term.generation, code, json: serde_json::to_string(info).unwrap_or_default() }
}

impl Terms {
    /// A new link: nothing goes out on it until the hub says what it has
    /// (its `Welcome`, a `Resync`).
    pub fn attach(&self, lanes: Lanes) {
        *lock(&self.link) = Some(lanes);
    }

    /// Link `epoch` ended.
    pub fn detach(&self, epoch: u64) {
        let mut link = lock(&self.link);
        if link.as_ref().is_some_and(|l| l.epoch == epoch) {
            *link = None;
        }
    }

    fn lanes(&self) -> Option<Lanes> {
        lock(&self.link).clone()
    }

    fn get(&self, id: &str, generation: u64) -> Option<Arc<Term>> {
        lock(&self.terms).get(id).filter(|t| t.generation == generation).cloned()
    }

    /// The terminals as `HelloInfo` lists them.
    pub fn held(&self) -> Vec<TermHeld> {
        let mut out: Vec<TermHeld> = lock(&self.terms)
            .values()
            .map(|t| {
                let state = lock(&t.state);
                TermHeld { term: t.id.clone(), generation: t.generation, end: state.ring.end(), alive: state.exit.is_none(), input_acked: state.input_acked }
            })
            .collect();
        out.sort_by(|a, b| a.term.cmp(&b.term));
        out
    }

    /// Starts a terminal (see the module for repeats and generations);
    /// `false`: that generation was started before (a repeat).
    pub fn spawn(self: &Arc<Self>, request: SpawnRequest) -> Result<bool> {
        let mut terms = lock(&self.terms);
        if let Some(existing) = terms.get(&request.term) {
            if existing.generation == request.generation {
                return Ok(false);
            }
            if lock(&existing.state).exit.is_none() {
                bail!("terminal {} runs (generation {}): it is stopped before another starts", request.term, existing.generation);
            }
        }
        let program = request.argv.first().context("nothing to run")?;
        // An agent's name follows `ATELIER_<AGENT>` when it is set (the self-tests' stand-in agents), as on the hub.
        let named = match program.as_str() {
            name @ ("copilot" | "opencode" | "claude") => canvas_common::env::var(&format!("ATELIER_{}", name.to_ascii_uppercase())).ok(),
            _ => None,
        };
        let program = named.as_ref().unwrap_or(program);
        let env = compose_env(&request.env);
        let (raw, raw_rx) = bounded::<Vec<u8>>(256);
        let (exited, exited_rx) = unbounded::<String>();
        let (pump, _) = bounded(1);
        let (clipboard, _) = bounded(1);
        let (changed, _) = bounded(1);
        let hooks = Hooks { pump, exited, clipboard, bytes: Default::default(), changed, notify: None, raw: Some(raw) };
        let spec = Spawn { id: &request.term, program, args: &request.argv[1..], env: &env, cwd: &request.cwd, cols: request.cols, rows: request.rows, copilot: false, engine: Engine::Alacritty };
        let terminal = Terminal::spawn(spec, hooks)?;
        let term = Arc::new(Term { id: request.term.clone(), generation: request.generation, terminal, state: Mutex::new(State { ring: Ring::new(RING), sent: None, input_acked: 0, exit: None }) });
        terms.insert(request.term, term.clone());
        drop(terms);
        let terms = self.clone();
        std::thread::Builder::new().name("sat-term".into()).spawn(move || {
            for chunk in raw_rx {
                terms.output(&term, &chunk);
            }
            // The output is all read: the exit code follows once the waiter has it.
            let _ = exited_rx.recv_timeout(Duration::from_secs(30));
            let code = lock(&term.terminal.raw_exit_code).unwrap_or(1);
            let error = lock(&term.terminal.error).clone().unwrap_or_default();
            terms.exited(&term, code, ExitInfo { error, unknown: false });
        })?;
        Ok(true)
    }

    /// A program that could not start: the hub hears it as an exit.
    pub fn refuse(&self, id: &str, generation: u64, error: String) {
        if let Some(lanes) = self.lanes() {
            let info = ExitInfo { error, unknown: false };
            let _ = lanes.io.send(SatUp::Exit { term: id.to_owned(), generation, code: 127, json: serde_json::to_string(&info).unwrap_or_default() });
        }
    }

    fn output(&self, term: &Term, chunk: &[u8]) {
        let mut state = lock(&term.state);
        let seq = state.ring.end();
        state.ring.push(chunk);
        if let Some(lanes) = self.lanes()
            && state.sent == Some((lanes.epoch, seq))
        {
            send_output(&lanes, term, seq, chunk);
            state.sent = Some((lanes.epoch, state.ring.end()));
        }
    }

    fn exited(&self, term: &Term, code: u32, info: ExitInfo) {
        let mut state = lock(&term.state);
        if let Some(lanes) = self.lanes()
            && state.sent == Some((lanes.epoch, state.ring.end()))
        {
            let _ = lanes.io.send(exit_message(term, code, &info));
        }
        state.exit = Some((code, info));
    }

    /// Sends what the hub lacks of a terminal from output offset `have`:
    /// the bytes when the ring holds them, else the screen (a gap) and what
    /// followed it; then its exit if it ended. An unknown terminal or
    /// generation is an exit marked `unknown`.
    pub fn resync(&self, id: &str, generation: u64, have: u64) {
        let Some(lanes) = self.lanes() else { return };
        let Some(term) = self.get(id, generation) else {
            let info = ExitInfo { error: String::new(), unknown: true };
            let _ = lanes.io.send(SatUp::Exit { term: id.to_owned(), generation, code: 0, json: serde_json::to_string(&info).unwrap_or_default() });
            return;
        };
        let mut state = lock(&term.state);
        match state.ring.since(have) {
            Some(bytes) => send_output(&lanes, &term, have, &bytes),
            None => {
                if let Some((screen, fed)) = term.terminal.snapshot_at() {
                    let vt = canvas_host::emulator::replay::repaint(&screen);
                    let _ = lanes.io.send(SatUp::Snapshot { term: term.id.clone(), generation, seq: fed, cols: screen.cols, rows: screen.rows, vt });
                    if let Some(rest) = state.ring.since(fed) {
                        send_output(&lanes, &term, fed, &rest);
                    }
                }
            }
        }
        state.sent = Some((lanes.epoch, state.ring.end()));
        if let Some((code, info)) = &state.exit {
            let _ = lanes.io.send(exit_message(&term, *code, info));
        }
    }

    /// Input `seq` for the program, applied once (a repeat is acknowledged again).
    pub fn input(&self, id: &str, generation: u64, seq: u64, bytes: Vec<u8>) {
        let Some(term) = self.get(id, generation) else { return };
        let mut state = lock(&term.state);
        if seq > state.input_acked {
            if term.terminal.send_input(&Input::Raw(bytes)).is_err() {
                return;
            }
            state.input_acked = seq;
        }
        if let Some(lanes) = self.lanes() {
            let _ = lanes.io.send(SatUp::InputAck { term: term.id.clone(), generation, seq: state.input_acked });
        }
    }

    pub fn resize(&self, id: &str, generation: u64, cols: u16, rows: u16) {
        if let Some(term) = self.get(id, generation) {
            let _ = term.terminal.resize(cols, rows);
        }
    }

    /// `SIG_INT` is a Ctrl+C typed into the terminal; the others end it.
    pub fn signal(&self, id: &str, generation: u64, signal: u8) {
        let Some(term) = self.get(id, generation) else { return };
        if signal == canvas_common::sat::SIG_INT {
            let _ = term.terminal.send_input(&Input::Raw(vec![3]));
        } else {
            term.terminal.force_kill();
        }
    }

    /// Ends the program (the hub's graceful stop types its keys first, then asks for this).
    pub fn stop(&self, id: &str, generation: u64) {
        if let Some(term) = self.get(id, generation) {
            term.terminal.force_kill();
        }
    }

    /// Ends every program (the satellite's credential was revoked).
    pub fn stop_all(&self) {
        for term in lock(&self.terms).values() {
            term.terminal.force_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::Receiver;
    use std::time::Instant;

    fn shell(script: &str) -> Vec<String> {
        if cfg!(windows) { vec!["cmd".into(), "/d".into(), "/c".into(), script.into()] } else { vec!["sh".into(), "-c".into(), script.into()] }
    }

    fn request(term: &str, generation: u64, argv: Vec<String>) -> SpawnRequest {
        SpawnRequest { term: term.into(), generation, argv, env: vec![("+ATELIER_TEST_PATH".into(), "first".into())], cwd: std::env::temp_dir().display().to_string(), cols: 60, rows: 10 }
    }

    fn link(terms: &Terms, epoch: u64) -> (Receiver<SatUp>, Receiver<SatUp>) {
        let (io, io_rx) = unbounded();
        let (other, other_rx) = unbounded();
        terms.attach(Lanes { io, other, epoch });
        (io_rx, other_rx)
    }

    /// The output the link carried for `term` (in order, checked contiguous) until its exit.
    fn collect(rx: &Receiver<SatUp>, term: &str, from: u64) -> (Vec<u8>, Option<(u32, ExitInfo)>) {
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(SatUp::Output { term: t, seq, bytes, .. }) if t == term => {
                    assert_eq!(seq, from + out.len() as u64, "contiguous output");
                    out.extend(bytes);
                }
                Ok(SatUp::Exit { term: t, code, json, .. }) if t == term => return (out, Some((code, serde_json::from_str(&json).unwrap()))),
                _ => {}
            }
        }
        (out, None)
    }

    #[test]
    fn a_terminal_runs_streams_replays_and_reports_its_exit() {
        let terms = Arc::new(Terms::default());
        let (io, _other) = link(&terms, 1);
        // Nothing goes out until the hub says what it has.
        terms.spawn(request("t1", 1, shell("echo one-two-three"))).unwrap();
        let held = || terms.held().into_iter().find(|h| h.term == "t1").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while held().alive {
            assert!(Instant::now() < deadline, "the program did not end");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(io.try_recv().is_err(), "nothing was sent before a resync");
        // A resync from 0 replays everything, then the exit.
        terms.resync("t1", 1, 0);
        let (out, exit) = collect(&io, "t1", 0);
        assert!(String::from_utf8_lossy(&out).contains("one-two-three"), "{:?}", String::from_utf8_lossy(&out));
        assert_eq!(exit.unwrap().0, 0);
        assert_eq!(held().end, out.len() as u64);
        // A repeat of the same generation is a no-op; another generation replaces an ended one.
        assert!(!terms.spawn(request("t1", 1, shell("echo again"))).unwrap());
        assert_eq!(held().generation, 1);
        // An unknown generation is answered as unknown.
        terms.resync("t1", 7, 0);
        let (_, exit) = collect(&io, "t1", 0);
        assert!(exit.unwrap().1.unknown);
    }

    #[test]
    fn live_output_follows_a_resync_and_input_is_applied_once() {
        let terms = Arc::new(Terms::default());
        let (io, _other) = link(&terms, 1);
        let argv = if cfg!(windows) { ["cmd", "/d", "/v:on", "/c", "set /p line= & echo got:!line!"].map(String::from).to_vec() } else { shell("printf 'path:%s\\n' \"$ATELIER_TEST_PATH\"; read line; echo got:$line") };
        terms.spawn(request("t2", 1, argv)).unwrap();
        terms.resync("t2", 1, 0);
        terms.input("t2", 1, 1, b"hello\r".to_vec());
        terms.input("t2", 1, 1, b"hello\r".to_vec());
        let (out, exit) = collect(&io, "t2", 0);
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("got:hello"), "{text:?}");
        assert!(!text.contains("got:hellohello"), "input applied once: {text:?}");
        if cfg!(unix) {
            assert!(text.contains("path:first"), "+NAME prepends: {text:?}");
        }
        assert!(exit.is_some());
        assert_eq!(terms.held()[0].input_acked, 1);
    }

    #[test]
    fn a_running_terminal_is_not_replaced_and_a_gap_sends_the_screen() {
        let terms = Arc::new(Terms::default());
        let (io, _other) = link(&terms, 1);
        let long = if cfg!(windows) { "echo screen-line & ping -n 30 127.0.0.1 > nul".to_owned() } else { "echo screen-line; sleep 30".to_owned() };
        terms.spawn(request("t3", 1, shell(&long))).unwrap();
        assert!(terms.spawn(request("t3", 2, shell("echo other"))).is_err(), "one live generation");
        terms.resync("t3", 1, 0);
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = Vec::new();
        while !String::from_utf8_lossy(&seen).contains("screen-line") {
            assert!(Instant::now() < deadline);
            if let Ok(SatUp::Output { bytes, .. }) = io.recv_timeout(Duration::from_millis(100)) {
                seen.extend(bytes);
            }
        }
        // The hub has more than the satellite ever sent: the ring cannot serve it, so the screen comes.
        terms.resync("t3", 1, u64::MAX / 2);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "no snapshot");
            if let Ok(SatUp::Snapshot { vt, seq, .. }) = io.recv_timeout(Duration::from_millis(100)) {
                assert!(String::from_utf8_lossy(&vt).contains("screen-line") && seq > 0);
                break;
            }
        }
        terms.stop("t3", 1);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !matches!(io.recv_timeout(Duration::from_millis(100)), Ok(SatUp::Exit { .. })) {
            assert!(Instant::now() < deadline, "a stop ends it");
        }
    }
}
