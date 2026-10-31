//! `linux-html-blitz doc IN OUT`: the page the GUI would load (the
//! sanitizer's document, canvas_model::report_html::safe_document).
//! `linux-html-blitz render DOC PNG WIDTH HEIGHT`: Blitz lays the page out and
//! paints it on the CPU; only `data:` URLs are served (everything else is
//! counted as blocked). Prints one JSON line with timings and memory.

use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use base64::Engine;
use blitz_dom::DocumentConfig;
use blitz_dom::net::Resource;
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::net::{BoxedHandler, Bytes, NetCallback, NetProvider, Request, SharedCallback};
use blitz_traits::shell::{ColorScheme, Viewport};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

struct DataOnly {
    callback: SharedCallback<Resource>,
    blocked: Mutex<Vec<String>>,
    served: AtomicUsize,
}

impl NetProvider<Resource> for DataOnly {
    fn fetch(&self, doc_id: usize, request: Request, handler: BoxedHandler<Resource>) {
        let url = request.url.as_str().to_string();
        if let Some(rest) = url.strip_prefix("data:")
            && let Some((meta, data)) = rest.split_once(',')
        {
            let bytes = if meta.ends_with(";base64") {
                base64::engine::general_purpose::STANDARD.decode(data.trim()).unwrap_or_default()
            } else {
                data.as_bytes().to_vec()
            };
            self.served.fetch_add(1, Ordering::SeqCst);
            handler.bytes(doc_id, Bytes::from(bytes), self.callback.clone());
            return;
        }
        self.blocked.lock().unwrap().push(url);
    }
}

struct Chan(Mutex<mpsc::Sender<Resource>>);
impl NetCallback<Resource> for Chan {
    fn call(&self, _doc_id: usize, result: Result<Resource, Option<String>>) {
        if let Ok(r) = result {
            let _ = self.0.lock().unwrap().send(r);
        }
    }
}

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmHWM:")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("doc") => {
            let html = std::fs::read_to_string(&args[2]).expect("read");
            std::fs::write(&args[3], canvas_model::report_html::safe_document(&html)).expect("write");
        }
        Some("render") => render(&args[2], &args[3], args[4].parse().unwrap(), args[5].parse().unwrap()),
        _ => eprintln!("usage: doc IN OUT | render DOC PNG WIDTH HEIGHT"),
    }
}

fn render(doc: &str, out: &str, width: u32, height: u32) {
    let t0 = Instant::now();
    let html = std::fs::read_to_string(doc).expect("read");
    let (tx, rx) = mpsc::channel();
    let net = Arc::new(DataOnly { callback: Arc::new(Chan(Mutex::new(tx))), blocked: Mutex::new(Vec::new()), served: AtomicUsize::new(0) });
    let mut document = HtmlDocument::from_html(
        &html,
        DocumentConfig { base_url: Some("about:blank".into()), net_provider: Some(net.clone() as _), ..Default::default() },
    );
    let parsed = t0.elapsed();
    document.as_mut().set_viewport(Viewport::new(width, height, 1.0, ColorScheme::Dark));
    while let Ok(res) = rx.try_recv() {
        document.as_mut().load_resource(res);
    }
    document.as_mut().resolve(0.0);
    let laid_out = t0.elapsed();
    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(|scene| paint_scene(scene, document.as_ref(), 1.0, width, height), width, height);
    let painted = t0.elapsed();
    let file = std::fs::File::create(out).expect("png");
    let mut enc = png::Encoder::new(file, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().unwrap();
    w.write_image_data(&buffer).unwrap();
    w.finish().unwrap();
    let doc_h = document.as_ref().root_element().final_layout.size.height;
    println!(
        "{{\"renderer\":\"blitz-0.2\",\"doc\":\"{doc}\",\"parse_ms\":{},\"layout_ms\":{},\"paint_ms\":{},\"total_ms\":{},\"peak_rss_kb\":{},\"doc_height\":{doc_h},\"data_urls\":{},\"blocked\":{:?}}}",
        parsed.as_millis(),
        (laid_out - parsed).as_millis(),
        (painted - laid_out).as_millis(),
        t0.elapsed().as_millis(),
        rss_kb(),
        net.served.load(Ordering::SeqCst),
        net.blocked.lock().unwrap()
    );
}
