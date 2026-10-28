//! `--measure` mode shared by both spikes: process memory/CPU (Win32), frame
//! statistics, and a phase driver that tells the app what to show next.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use canvas_common::protocol::Frame;
use serde_json::{Map, Value, json};

use crate::term::Layout;

pub fn memory_mb() -> (f64, f64) {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters as *mut _ as *mut _, counters.cb) };
    (counters.PrivateUsage as f64 / 1_048_576.0, counters.WorkingSetSize as f64 / 1_048_576.0)
}

fn filetime_ms(ft: windows_sys::Win32::Foundation::FILETIME) -> f64 {
    ((ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64) as f64 / 10_000.0
}

fn process_times() -> (f64, f64) {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
    (filetime_ms(created), filetime_ms(kernel) + filetime_ms(user))
}

/// CPU time (user + kernel, all threads) of this process in ms.
pub fn cpu_ms() -> f64 {
    process_times().1
}

/// Milliseconds since the process was created (includes loader time).
pub fn process_age_ms() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    let mut now = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    unsafe { GetSystemTimePreciseAsFileTime(&mut now) };
    filetime_ms(now) - process_times().0
}

pub fn percentile(samples: &[f64], p: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    round2(sorted[index])
}

pub fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[derive(Clone, Debug)]
pub enum Action {
    /// Show these terminals in tiles 0..3 (and subscribe to them).
    Tiles(Vec<String>),
    Layout(Layout),
    /// Continuous programmatic pan + zoom of the canvas.
    Animate(bool),
    /// Inject a key press into the focused tile through the OS message queue.
    Key(char),
    /// Print the report and exit.
    Done(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Boot,
    Stream,
    TilesOnly,
    Pan,
    Idle,
    Latency,
    Done,
}

/// (phase, length, sampling window start, sampling window end) in seconds.
fn schedule(phase: Phase) -> (f64, f64, f64) {
    match phase {
        Phase::Boot => (60.0, 0.0, 0.0),
        Phase::Stream => (10.0, 2.0, 10.0),
        Phase::TilesOnly => (7.0, 2.0, 7.0),
        Phase::Pan => (6.0, 1.0, 6.0),
        Phase::Idle => (12.0, 1.5, 11.5),
        Phase::Latency => (30.0, 1.5, 30.0),
        Phase::Done => (0.0, 0.0, 0.0),
    }
}

fn key_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Stream => "streaming_tiles",
        Phase::TilesOnly => "tiles_only_full_grid",
        Phase::Pan => "pan_zoom",
        Phase::Idle => "idle",
        Phase::Latency => "latency_typing",
        _ => "boot",
    }
}

pub struct Ids {
    pub noise: Vec<String>,
    pub shell: Vec<String>,
    pub echo: String,
}

pub struct Driver {
    stack: &'static str,
    ids: Ids,
    phase: Phase,
    phase_start: Instant,
    pub frames_started: u64,
    frames_presented: u64,
    first_paint_ms: Option<f64>,
    ready: bool,
    sampling: bool,
    samples: Vec<f64>,
    acquire: Vec<f64>,
    window_start: Option<(Instant, f64, u64)>,
    drawn: (f64, f64, u64),
    app_ms: (f64, f64),
    report: Map<String, Value>,
    frame_ms: Map<String, Value>,
    cpu: Map<String, Value>,
    drawn_report: Map<String, Value>,
    // keystroke → present
    next_key_at: Option<Instant>,
    sent: Option<(Instant, u64, char)>,
    seen: Option<Instant>,
    keys_sent: u64,
    latencies: Vec<f64>,
    to_frame: Vec<f64>,
    to_forward: Vec<f64>,
    frame_to_present: Vec<f64>,
    forwarded_at: Option<Instant>,
    missed: u32,
    pending: Vec<Action>,
}

const KEYS: usize = 60;
const KEY_TIMEOUT: Duration = Duration::from_secs(1);
const KEY_GAP: Duration = Duration::from_millis(40);

impl Driver {
    pub fn new(stack: &'static str, ids: Ids) -> Self {
        let mut d = Driver {
            stack,
            ids,
            phase: Phase::Boot,
            phase_start: Instant::now(),
            frames_started: 0,
            frames_presented: 0,
            first_paint_ms: None,
            ready: false,
            sampling: false,
            samples: Vec::new(),
            acquire: Vec::new(),
            window_start: None,
            drawn: (0.0, 0.0, 0),
            app_ms: (0.0, 0.0),
            report: Map::new(),
            frame_ms: Map::new(),
            cpu: Map::new(),
            drawn_report: Map::new(),
            next_key_at: None,
            sent: None,
            seen: None,
            keys_sent: 0,
            latencies: Vec::new(),
            to_frame: Vec::new(),
            to_forward: Vec::new(),
            frame_to_present: Vec::new(),
            forwarded_at: None,
            missed: 0,
            pending: Vec::new(),
        };
        d.pending.push(Action::Tiles(d.ids.noise.clone()));
        d.pending.push(Action::Layout(Layout::Normal));
        d
    }

    pub fn info(&mut self, key: &str, value: Value) {
        self.report.insert(key.to_owned(), value);
    }

    /// Records this process's private memory now under `private_mb_<name>`.
    pub fn memory_checkpoint(&mut self, name: &str) {
        let (private, _) = memory_mb();
        self.report.insert(format!("private_mb_{name}"), json!(round2(private)));
        self.report.insert(format!("startup_ms_{name}"), json!(round2(process_age_ms())));
    }

    pub fn take_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.pending)
    }

    /// The app has every noise terminal streaming into its tiles.
    pub fn set_ready(&mut self) {
        self.ready = true;
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn frame_started(&mut self) {
        self.frames_started += 1;
    }

    /// A frame was presented; `frame_ms` is its CPU-side cost (build + submit +
    /// present call), `acquire_ms` time blocked waiting for a back buffer.
    pub fn presented(&mut self, frame_ms: f64, acquire_ms: f64, now: Instant) {
        self.frames_presented += 1;
        if self.first_paint_ms.is_none() {
            self.first_paint_ms = Some(process_age_ms());
        }
        if self.sampling {
            self.samples.push(frame_ms);
            self.acquire.push(acquire_ms);
        }
        if let Some((sent, _, _)) = self.sent
            && let Some(seen_at) = self.seen
            && now.checked_sub(Duration::from_secs_f64(frame_ms / 1000.0)).is_some_and(|start| start >= seen_at)
        {
            // First frame that started after the echo arrived (frames drawn
            // outside the measured path, e.g. during key dispatch, are skipped).
            self.latencies.push(now.duration_since(sent).as_secs_f64() * 1000.0);
            self.to_frame.push(seen_at.duration_since(sent).as_secs_f64() * 1000.0);
            self.frame_to_present.push(now.duration_since(seen_at).as_secs_f64() * 1000.0);
            self.sent = None;
            self.forwarded_at = None;
            self.seen = None;
            self.next_key_at = Some(now + KEY_GAP);
        }
    }

    /// Per-frame counts and the app's own paint/prepare time split
    /// (canvas, tiles), excluding the framework's scene/render/present work.
    pub fn drawn(&mut self, boxes: usize, cells: usize, canvas_ms: f64, tiles_ms: f64) {
        if self.sampling {
            self.drawn.0 += boxes as f64;
            self.drawn.1 += cells as f64;
            self.drawn.2 += 1;
            self.app_ms.0 += canvas_ms;
            self.app_ms.1 += tiles_ms;
        }
    }

    /// The app is forwarding typed input to the service (after its key/char handling).
    pub fn forwarded(&mut self) {
        if let Some((sent, _, _)) = self.sent
            && self.forwarded_at.is_none()
        {
            let now = Instant::now();
            self.forwarded_at = Some(now);
            self.to_forward.push(now.duration_since(sent).as_secs_f64() * 1000.0);
        }
    }

    /// Service frame about to be applied: completes a pending keystroke when
    /// the echo terminal shows `<n>:<c>`.
    pub fn service_frame(&mut self, frame: &Frame) {
        let Some((_, n, c)) = self.sent else { return };
        if self.seen.is_some() || frame.id != self.ids.echo {
            return;
        }
        let want = format!(":{c}");
        let hit = frame.changed.iter().any(|(_, runs)| {
            let text: String = runs.iter().map(|r| r.text.as_str()).collect();
            let text = text.trim();
            text.ends_with(&want) && text[..text.len() - want.len()].parse::<u64>().is_ok_and(|k| k >= n)
        });
        if hit {
            self.seen = Some(Instant::now());
        }
    }

    fn enter(&mut self, phase: Phase, now: Instant) {
        self.phase = phase;
        self.phase_start = now;
        self.sampling = false;
        self.window_start = None;
        match phase {
            Phase::Stream => {
                self.pending.push(Action::Tiles(self.ids.noise.clone()));
                self.pending.push(Action::Layout(Layout::Normal));
            }
            Phase::TilesOnly => self.pending.push(Action::Layout(Layout::TilesOnly)),
            Phase::Pan => {
                self.pending.push(Action::Layout(Layout::Normal));
                self.pending.push(Action::Tiles(self.ids.shell.clone()));
                self.pending.push(Action::Animate(true));
            }
            Phase::Idle => self.pending.push(Action::Animate(false)),
            Phase::Latency => {
                let mut tiles = vec![self.ids.echo.clone()];
                tiles.extend(self.ids.noise.iter().skip(1).cloned());
                self.pending.push(Action::Tiles(tiles));
            }
            Phase::Done => {
                let report = self.finish();
                self.pending.push(Action::Done(report));
            }
            Phase::Boot => {}
        }
    }

    fn close_window(&mut self, now: Instant) {
        let Some((t0, cpu0, frames0)) = self.window_start.take() else { return };
        let secs = now.duration_since(t0).as_secs_f64();
        let cpu = cpu_ms() - cpu0;
        let frames = self.frames_presented - frames0;
        let name = key_name(self.phase);
        self.cpu.insert(name.into(), json!(round2(cpu / (secs * 10.0))));
        if self.phase != Phase::Idle && self.phase != Phase::Latency {
            let mean = if self.samples.is_empty() { 0.0 } else { self.samples.iter().sum::<f64>() / self.samples.len() as f64 };
            let mut stats = json!({
                "p50": percentile(&self.samples, 0.5),
                "p95": percentile(&self.samples, 0.95),
                "max": percentile(&self.samples, 1.0),
                "mean": round2(mean),
                "frames": frames,
                "fps": round2(frames as f64 / secs),
                "process_cpu_ms_per_frame": round2(if frames > 0 { cpu / frames as f64 } else { 0.0 }),
            });
            if self.acquire.iter().any(|a| *a > 0.0) {
                stats["acquire_wait_p50"] = json!(percentile(&self.acquire, 0.5));
            }
            self.frame_ms.insert(name.into(), stats);
            if self.drawn.2 > 0 {
                let n = self.drawn.2 as f64;
                self.drawn_report.insert(
                    name.into(),
                    json!({
                        "boxes_avg": (self.drawn.0 / n).round(),
                        "tile_cells_avg": (self.drawn.1 / n).round(),
                        "app_canvas_ms_avg": round2(self.app_ms.0 / n),
                        "app_tiles_ms_avg": round2(self.app_ms.1 / n),
                    }),
                );
            }
        }
        if self.phase == Phase::Idle {
            self.report.insert("idle_cpu_percent".into(), json!(round2(cpu / (secs * 10.0))));
            self.report.insert("idle_frames_presented".into(), json!(frames));
            self.report.insert("idle_seconds".into(), json!(round2(secs)));
        }
        self.samples.clear();
        self.acquire.clear();
        self.drawn = (0.0, 0.0, 0);
        self.app_ms = (0.0, 0.0);
    }

    /// Advances the timeline; returns when it next needs to run.
    pub fn tick(&mut self, now: Instant) -> Instant {
        let t = now.duration_since(self.phase_start).as_secs_f64();
        let (length, w0, w1) = schedule(self.phase);
        match self.phase {
            Phase::Boot => {
                if self.ready && self.first_paint_ms.is_some() {
                    self.report.insert("first_paint_ms".into(), json!(round2(self.first_paint_ms.unwrap_or(0.0))));
                    self.report.insert("boot_to_streaming_s".into(), json!(round2(t)));
                    self.enter(Phase::Stream, now);
                    return now;
                }
                if t > length {
                    self.report.insert("error".into(), json!("terminals never became ready"));
                    self.enter(Phase::Done, now);
                    return now;
                }
                return now + Duration::from_millis(50);
            }
            Phase::Done => return now + Duration::from_secs(3600),
            _ => {}
        }
        if self.window_start.is_none() && !self.sampling && t >= w0 && t < w1 {
            self.window_start = Some((now, cpu_ms(), self.frames_presented));
            self.sampling = true;
        }
        if self.phase == Phase::Latency && self.sampling {
            self.latency_tick(now);
        }
        if self.phase == Phase::Stream && t >= w1 - 0.05 && !self.report.contains_key("private_mb_after_10s_streaming") {
            let (private, ws) = memory_mb();
            self.report.insert("private_mb_after_10s_streaming".into(), json!(round2(private)));
            self.report.insert("working_set_mb_after_10s_streaming".into(), json!(round2(ws)));
        }
        let latency_done = self.phase == Phase::Latency && self.keys_sent as usize >= KEYS && self.sent.is_none();
        if (self.sampling && t >= w1) || latency_done {
            self.close_window(now);
            self.sampling = false;
        }
        if t >= length || latency_done {
            self.close_window(now);
            let next = match self.phase {
                Phase::Stream => Phase::TilesOnly,
                Phase::TilesOnly => Phase::Pan,
                Phase::Pan => Phase::Idle,
                Phase::Idle => Phase::Latency,
                _ => Phase::Done,
            };
            self.enter(next, now);
            return now;
        }
        let mut next = self.phase_start + Duration::from_secs_f64(if t < w0 { w0 } else if t < w1 { w1 } else { length });
        if self.phase == Phase::Stream && t < w1 - 0.05 {
            next = next.min(self.phase_start + Duration::from_secs_f64(w1 - 0.05));
        }
        if self.phase == Phase::Latency {
            next = next.min(now + Duration::from_millis(2));
        }
        next.max(now + Duration::from_millis(1))
    }

    fn latency_tick(&mut self, now: Instant) {
        if let Some((sent, _, _)) = self.sent {
            if now.duration_since(sent) > KEY_TIMEOUT {
                self.missed += 1;
                self.sent = None;
                self.forwarded_at = None;
                self.seen = None;
                self.next_key_at = Some(now + KEY_GAP);
            }
            return;
        }
        if self.keys_sent as usize >= KEYS || self.next_key_at.is_some_and(|at| now < at) {
            return;
        }
        self.keys_sent += 1;
        let c = (b'a' + ((self.keys_sent - 1) % 26) as u8) as char;
        self.sent = Some((now, self.keys_sent, c));
        self.pending.push(Action::Key(c));
    }

    fn finish(&mut self) -> String {
        let mut report = Map::new();
        report.insert("stack".into(), json!(self.stack));
        for (k, v) in std::mem::take(&mut self.report) {
            report.insert(k, v);
        }
        report.insert("frame_ms".into(), Value::Object(std::mem::take(&mut self.frame_ms)));
        report.insert("process_cpu_percent".into(), Value::Object(std::mem::take(&mut self.cpu)));
        report.insert("drawn".into(), Value::Object(std::mem::take(&mut self.drawn_report)));
        report.insert(
            "keystroke_to_present_ms".into(),
            json!({
                "p50": percentile(&self.latencies, 0.5),
                "p95": percentile(&self.latencies, 0.95),
                "max": percentile(&self.latencies, 1.0),
                "samples": self.latencies.len(),
                "missed": self.missed,
                "service_frame_p50": percentile(&self.to_frame, 0.5),
                "key_handled_p50": percentile(&self.to_forward, 0.5),
                "key_handled_p95": percentile(&self.to_forward, 0.95),
                "service_frame_to_present_p50": percentile(&self.frame_to_present, 0.5),
                "service_frame_to_present_p95": percentile(&self.frame_to_present, 0.95),
                "path": "PostMessage WM_KEYDOWN to the window -> framework key/char handling -> ClientMsg::Input -> service -> echo -> Frame -> present",
            }),
        );
        let (private, ws) = memory_mb();
        report.insert("private_mb_at_end".into(), json!(round2(private)));
        report.insert("working_set_mb_at_end".into(), json!(round2(ws)));
        serde_json::to_string_pretty(&Value::Object(report)).unwrap_or_default()
    }
}
