//! The report's HTML view on macOS and Linux comes with Phase 4 (WKWebView;
//! WebKit offscreen on Linux). Until then a shown report reports the view as
//! unavailable once, and the canvas shows the report as formatted text, as
//! on Windows without the WebView2 runtime.

use canvas_core::view::HtmlPane;
use serde_json::{Value, json};

const UNAVAILABLE: &str = "the HTML view is not available on this platform yet";

/// What happened in the view that the window acts on (the native views of
/// Phase 4 send all of them; this stand-in only fails).
#[allow(dead_code)]
pub enum WebEvent {
    Ready,
    Failed(String),
    /// The user clicked a link (the canvas asks before opening it).
    Link(String),
    /// Esc in the view: the report closes.
    Escape,
}

pub struct ReportWeb {
    events: Vec<WebEvent>,
    failed: bool,
}

impl ReportWeb {
    pub fn new(_home: &std::path::Path) -> Self {
        ReportWeb { events: Vec::new(), failed: false }
    }

    pub fn take_events(&mut self) -> Vec<WebEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn sync(&mut self, _window: isize, pane: Option<&HtmlPane>, _cell_w: f32, _cell_h: f32) {
        if pane.is_some() && !self.failed {
            self.failed = true;
            self.events.push(WebEvent::Failed(UNAVAILABLE.into()));
            crate::shell::wake_web();
        }
    }

    pub fn json(&self) -> Value {
        json!({ "ok": true, "phase": if self.failed { format!("failed: {UNAVAILABLE}") } else { "idle".into() }, "visible": false })
    }

    pub fn eval(&self, _script: &str, reply: crossbeam_channel::Sender<Value>) {
        let _ = reply.send(json!({ "ok": false, "error": "no HTML view" }));
    }

    pub fn probe_raw(&mut self, _html: &str) -> Value {
        json!({ "ok": false, "error": "no HTML view" })
    }

    pub fn capture(&self, _path: std::path::PathBuf, reply: crossbeam_channel::Sender<Value>) {
        let _ = reply.send(json!({ "ok": false, "error": "no HTML view" }));
    }
}

/// The composed window as a PNG (the offscreen capture covers it elsewhere).
pub fn capture_window(_window: isize, _path: &std::path::Path) -> anyhow::Result<(u32, u32)> {
    anyhow::bail!("window capture is Windows-only; use the offscreen capture")
}
