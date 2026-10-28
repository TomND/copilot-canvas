//! Client-side frame statistics: frame cost, present time, keystroke to
//! present (through the service's echo), service round trip; shown by
//! `--perf` and returned by the `perf` test hook.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const KEEP: usize = 512;

#[derive(Default)]
pub struct Samples(VecDeque<f32>);

impl Samples {
    pub fn push(&mut self, ms: f32) {
        if self.0.len() == KEEP {
            self.0.pop_front();
        }
        self.0.push_back(ms);
    }

    pub fn percentile(&self, p: f32) -> f32 {
        if self.0.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f32> = self.0.iter().copied().collect();
        sorted.sort_by(f32::total_cmp);
        sorted[((sorted.len() - 1) as f32 * p / 100.0).round() as usize]
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({ "n": self.len(), "p50": round(self.percentile(50.0)), "p95": round(self.percentile(95.0)), "p99": round(self.percentile(99.0)), "max": round(self.percentile(100.0)) })
    }
}

fn round(v: f32) -> f32 {
    (v * 100.0).round() / 100.0
}

#[derive(Default)]
pub struct Perf {
    /// View + scene + encode on the UI thread.
    pub frame_ms: Samples,
    /// Building the view (core) only.
    pub view_ms: Samples,
    pub scene_ms: Samples,
    pub encode_ms: Samples,
    pub acquire_ms: Samples,
    pub present_ms: Samples,
    /// Input arrival → its frame presented.
    pub input_ms: Samples,
    /// Key typed into a terminal → the frame showing the terminal's echo presented.
    pub key_ms: Samples,
    /// Service frame arrival → presented.
    pub output_ms: Samples,
    /// `Stats` request → answer.
    pub rtt_ms: Samples,
    frames: VecDeque<Instant>,
    /// Oldest input not yet on screen.
    pub input_at: Option<Instant>,
    /// Oldest key typed into a terminal whose echo is not on screen yet.
    pub key_at: Option<Instant>,
    /// The echo arrived (a frame of the focused terminal after the key).
    pub echoed: bool,
    pub output_at: Option<Instant>,
    pub presented: u64,
    pub skipped: u64,
}

impl Perf {
    pub fn presented(&mut self, done: Instant) {
        self.presented += 1;
        self.frames.push_back(done);
        while self.frames.front().is_some_and(|t| done.duration_since(*t) > Duration::from_secs(1)) {
            self.frames.pop_front();
        }
        if let Some(at) = self.input_at.take() {
            self.input_ms.push(ms(done - at));
        }
        if let Some(at) = self.output_at.take() {
            self.output_ms.push(ms(done - at));
        }
        if self.echoed
            && let Some(at) = self.key_at.take()
        {
            self.key_ms.push(ms(done - at));
            self.echoed = false;
        }
    }

    pub fn fps(&self) -> usize {
        self.frames.iter().filter(|t| t.elapsed() <= Duration::from_secs(1)).count()
    }

    /// The overlay's lines.
    pub fn lines(&self, remote: bool) -> Vec<String> {
        let mut lines = vec![
            format!("{} fps · frame {:.2} ms p95 (view {:.2} · scene {:.2} · encode {:.2})", self.fps(), self.frame_ms.percentile(95.0), self.view_ms.percentile(95.0), self.scene_ms.percentile(95.0), self.encode_ms.percentile(95.0)),
            format!("present {:.2} ms · acquire {:.2} ms · input→present {:.1} ms p95", self.present_ms.percentile(95.0), self.acquire_ms.percentile(95.0), self.input_ms.percentile(95.0)),
            format!("key→echo→present p50 {:.1} / p99 {:.1} ms · output→present {:.1} ms p95", self.key_ms.percentile(50.0), self.key_ms.percentile(99.0), self.output_ms.percentile(95.0)),
        ];
        if remote || self.rtt_ms.len() > 0 {
            lines.push(format!("service round trip {:.1} ms (p50 {:.1})", self.rtt_ms.percentile(95.0).max(0.0), self.rtt_ms.percentile(50.0)));
        }
        lines
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "fps": self.fps(),
            "presented": self.presented,
            "skipped": self.skipped,
            "frame_ms": self.frame_ms.json(),
            "view_ms": self.view_ms.json(),
            "scene_ms": self.scene_ms.json(),
            "encode_ms": self.encode_ms.json(),
            "acquire_ms": self.acquire_ms.json(),
            "present_ms": self.present_ms.json(),
            "input_ms": self.input_ms.json(),
            "key_ms": self.key_ms.json(),
            "output_ms": self.output_ms.json(),
            "rtt_ms": self.rtt_ms.json(),
        })
    }

    /// Starts a measurement from scratch.
    pub fn reset(&mut self) {
        *self = Perf { presented: self.presented, ..Perf::default() };
    }
}

pub fn ms(d: Duration) -> f32 {
    d.as_secs_f32() * 1000.0
}

/// Process CPU time and private memory (Win32).
pub fn process() -> (Duration, u64) {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    let mut memory: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    memory.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    // SAFETY: the current process's pseudo handle and properly sized out-structs.
    unsafe {
        GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user);
        GetProcessMemoryInfo(GetCurrentProcess(), &mut memory as *mut _ as *mut _, memory.cb);
    }
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    (Duration::from_nanos((ticks(kernel) + ticks(user)) * 100), memory.PrivateUsage as u64)
}

/// Time since this process was created.
pub fn since_start() -> Duration {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user, mut now) = (zero, zero, zero, zero, zero);
    // SAFETY: as above.
    unsafe {
        GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user);
        GetSystemTimeAsFileTime(&mut now);
    }
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    Duration::from_nanos(ticks(now).saturating_sub(ticks(created)) * 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystroke_latency_counts_once_the_echo_is_on_screen() {
        let mut perf = Perf::default();
        let t0 = Instant::now();
        perf.key_at = Some(t0);
        perf.presented(t0 + Duration::from_millis(3));
        assert_eq!(perf.key_ms.len(), 0, "no echo yet");
        perf.echoed = true;
        perf.presented(t0 + Duration::from_millis(9));
        assert_eq!(perf.key_ms.len(), 1);
        assert!((perf.key_ms.percentile(50.0) - 9.0).abs() < 0.01);
        let mut s = Samples::default();
        for v in 1..=100 {
            s.push(v as f32);
        }
        assert_eq!((s.percentile(50.0), s.percentile(99.0)), (51.0, 99.0));
        assert!(process().1 > 0 && since_start() > Duration::ZERO);
    }
}
