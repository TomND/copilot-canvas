//! `--test-hooks`: a local, owner-only named pipe
//! (`\\.\pipe\copilot-canvas-gui-test-<pid>`) taking one JSON command per
//! line and answering one JSON line each. Commands inject input (`key`,
//! `text`, `paste`, `mouse`, `wheel`, `drop`), read the UI (`screen`, `tree`,
//! `size`, `perf`, `platform`), render offscreen (`png`), manage windows
//! (`new_window`, `windows`, `close_window`; others take `"window": n`), set
//! the stand-in clipboard (`clipboard`) and `quit`. See `docs/GUI.md` §3.

use std::io::{BufRead, BufReader, Write};

use anyhow::Result;
use canvas_core::view::View;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use serde_json::{Value, json};
use winit::event_loop::EventLoopProxy;

use crate::shell::UserEvent;

pub struct Request {
    pub command: Value,
    pub reply: crossbeam_channel::Sender<Value>,
}

pub fn pipe_name() -> String {
    format!(r"\\.\pipe\copilot-canvas-gui-test-{}", std::process::id())
}

/// Serves the pipe on background threads; commands run on the UI thread.
pub fn serve(name: &str, proxy: EventLoopProxy<UserEvent>) -> Result<()> {
    let mut server = canvas_common::pipe::PipeServer::bind(name)?;
    std::thread::Builder::new().name("test-hooks".into()).spawn(move || {
        while let Ok(pipe) = server.accept() {
            let proxy = proxy.clone();
            let _ = std::thread::Builder::new().name("test-hook-client".into()).spawn(move || {
                let Ok((reader, mut writer, _closer)) = pipe.split() else { return };
                for line in BufReader::new(reader).lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    let reply = match serde_json::from_str::<Value>(&line) {
                        Ok(command) => {
                            let (tx, rx) = crossbeam_channel::bounded(1);
                            if proxy.send_event(UserEvent::Hook(Request { command, reply: tx })).is_err() {
                                break;
                            }
                            rx.recv_timeout(std::time::Duration::from_secs(120)).unwrap_or_else(|_| json!({ "ok": false, "error": "no answer" }))
                        }
                        Err(error) => json!({ "ok": false, "error": format!("bad command: {error}") }),
                    };
                    if writeln!(writer, "{reply}").and_then(|_| writer.flush()).is_err() {
                        break;
                    }
                }
            });
        }
    })?;
    Ok(())
}

/// The screen as text, one string per row: chrome cells, the canvas drawn as
/// characters at the cells its shapes cover, and the terminals' text.
pub fn screen(app: &canvas_core::app::App, view: &View, chrome: &Buffer, overlay: &Buffer) -> Vec<String> {
    let area = chrome.area;
    let mut grid = Buffer::empty(area);
    if let Some(model) = &view.canvas {
        canvas_core::ui_canvas::paint_model(model, &mut grid);
    }
    let content = |x: u16, y: u16| {
        let inside = |r: &Rect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        view.canvas.as_ref().is_some_and(|c| inside(&c.area)) || view.tiles.iter().any(|t| inside(&t.inner))
    };
    for tile in &view.tiles {
        if let Some(term) = app.views.get(&tile.id) {
            let options = canvas_termview::tui::Options { tint: None, selection: None, cursor: false };
            canvas_termview::tui::draw(term, &mut grid, tile.inner, &options);
        }
    }
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = &chrome[(x, y)];
            let blank = cell.symbol() == " " && cell.bg == ratatui::style::Color::Reset;
            if !content(x, y) || !blank {
                grid[(x, y)] = cell.clone();
            }
        }
    }
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = &overlay[(x, y)];
            if *cell != Cell::EMPTY {
                grid[(x, y)] = cell.clone();
            }
        }
    }
    (area.y..area.bottom()).map(|y| (area.x..area.right()).map(|x| grid[(x, y)].symbol()).collect::<String>()).collect()
}

fn rect(r: Rect) -> Value {
    json!([r.x, r.y, r.width, r.height])
}

/// The automation tree: click targets, canvas boxes and groups, tiles, focus.
pub fn tree(app: &canvas_core::app::App, view: &View, chrome: &Buffer) -> Value {
    let text_in = |r: Rect| -> String {
        (r.x..r.right().min(chrome.area.right())).filter_map(|x| chrome.cell((x, r.y)).map(|c| c.symbol().to_owned())).collect::<String>().trim().to_owned()
    };
    let hits: Vec<Value> = app.layout.hits.iter().map(|(r, hit)| json!({ "rect": rect(*r), "hit": format!("{hit:?}"), "text": text_in(*r) })).collect();
    let canvas = view.canvas.as_ref().map(|model| {
        let screen = |r: canvas_core::view::WRect| {
            let s = model.rect_to_screen(r);
            json!([s.x, s.y, s.w, s.h])
        };
        json!({
            "area": rect(model.area),
            "scale": model.scale,
            "origin": [model.origin.0, model.origin.1],
            "boxes": model.boxes.iter().map(|b| json!({
                "id": b.id, "name": b.name, "prefix": b.prefix, "status": b.status.label(), "line": b.line, "rect": screen(b.rect),
                "selected": b.selected, "attention": b.attention.is_some(), "border": format!("{:?}", b.border), "loop": b.every,
            })).collect::<Vec<_>>(),
            "groups": model.groups.iter().map(|g| json!({ "id": g.id, "name": g.name, "rect": screen(g.rect), "selected": g.selected, "members": g.members })).collect::<Vec<_>>(),
            "edges": model.edges.len(),
            "halo": model.halo.map(|(r, typing)| json!({ "rect": screen(r), "typing": typing })),
            "loops": model.loops.len(),
        })
    });
    json!({
        "cols": chrome.area.width,
        "rows": chrome.area.height,
        "focus": format!("{:?}", app.focus),
        "focused_term": app.focused_term,
        "selection": app.selection,
        "modal": view.modal.map(rect),
        "modals": app.modals.len(),
        "cursor": view.cursor.map(|(x, y)| json!([x, y])),
        "banner": view.banner,
        "connected": app.connected,
        "hits": hits,
        "canvas": canvas,
        "tiles": view.tiles.iter().map(|t| {
            // Kitty images as screen cell rects (x, y, columns, rows).
            let images: Vec<Value> = app.views.get(&t.id).map(|v| v.placements.iter().map(|p| json!({
                "id": p.image_id,
                "rect": [t.inner.x as i32 + p.col, t.inner.y as i32 + p.row, p.cols, p.rows],
                "z": p.z,
                "loaded": v.images.contains_key(&p.image_id),
            })).collect()).unwrap_or_default();
            json!({ "id": t.id, "outer": rect(t.outer), "inner": rect(t.inner), "focused": t.focused, "alive": t.alive, "title": t.title, "tint": t.tint, "images": images })
        }).collect::<Vec<_>>(),
        "term_selection": app.term_select.as_ref().map(|s| json!({ "id": s.id, "anchor": [s.anchor.0, s.anchor.1], "head": [s.head.0, s.head.1] })),
        "notice": app.notice.as_ref().map(|n| json!({ "text": n.text, "error": n.error })),
        "links": app.layout.links.iter().map(|l| json!({ "target": l.target, "terminal": l.terminal, "rects": l.rects.iter().map(|r| rect(*r)).collect::<Vec<_>>() })).collect::<Vec<_>>(),
    })
}
