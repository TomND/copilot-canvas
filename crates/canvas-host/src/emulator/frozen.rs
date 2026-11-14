//! What an exited terminal keeps once its engine is gone: its screen and
//! scrollback as protocol-8 rows, scrollable as the engine was, with the
//! count of output bytes the engine took, and the images on its screen as
//! PNG files. A live engine holds far more (cells, the parser, image
//! storage) and a thread of its own. Only the [`KEPT_HISTORIES`] most
//! recently exited or scrolled keep their scrollback ([`Recent`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use canvas_common::protocol::Run8;

use super::{Emulator, Scroll, Snapshot};

/// Exited terminals that keep their scrollback: the most recently exited
/// or scrolled ones; the others keep their last screen (R2-B-06: each kept
/// up to 5000 rows, megabytes, until its box was removed).
pub const KEPT_HISTORIES: usize = 20;

/// An image of the frozen screen, encoded while the engine still had its pixels.
#[derive(Clone, Debug)]
pub struct FrozenImage {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub png: Arc<[u8]>,
}

#[derive(Clone, Debug, Default)]
pub struct Frozen {
    /// The live screen as the engine left it (cursor, modes and images
    /// included); its `links` are those of every row, history included.
    screen: Snapshot,
    /// The scrollback, oldest row first.
    history: Vec<Vec<Run8>>,
    /// Rows scrolled back from the live screen.
    offset: u32,
    /// Output bytes the engine took (`Terminal::snapshot_at`).
    pub fed: u64,
    /// The images placed on the screen, by image id ([`Frozen::keep_images`]).
    pub images: HashMap<u32, FrozenImage>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The frozen screens that keep their scrollback, most recent first; the
/// one pushed out of the first `cap` drops its scrollback.
pub struct Recent {
    cap: usize,
    list: Mutex<VecDeque<Weak<Mutex<Frozen>>>>,
}

impl Recent {
    pub const fn new(cap: usize) -> Self {
        Self { cap, list: Mutex::new(VecDeque::new()) }
    }

    /// `frozen` was just made or scrolled: it keeps its scrollback; the
    /// least recent beyond the cap keep their screen only. Takes the list's
    /// lock, then the frozen screens' (never the other way).
    pub fn touch(&self, frozen: &Arc<Mutex<Frozen>>) {
        let mut list = lock(&self.list);
        list.retain(|kept| kept.strong_count() > 0 && !std::ptr::eq(kept.as_ptr(), Arc::as_ptr(frozen)));
        list.push_front(Arc::downgrade(frozen));
        while list.len() > self.cap {
            if let Some(old) = list.pop_back().and_then(|old| old.upgrade()) {
                lock(&old).drop_history();
            }
        }
    }
}

/// The service's [`Recent`].
pub fn recent() -> &'static Recent {
    static RECENT: Recent = Recent::new(KEPT_HISTORIES);
    &RECENT
}

/// `row`'s links as indexes into `links` (added when new), from those of `from`.
fn relink(row: &mut [Run8], from: &[String], links: &mut Vec<String>) {
    for run in row.iter_mut().filter(|run| run.link != 0) {
        run.link = match from.get(run.link as usize - 1) {
            Some(target) => match links.iter().position(|l| l == target) {
                Some(at) => at as u32 + 1,
                None => {
                    links.push(target.clone());
                    links.len() as u32
                }
            },
            None => 0,
        };
    }
}

impl Frozen {
    /// The engine's screen and whole scrollback, read page by page (the
    /// engine is scrolled; it is about to be dropped).
    pub fn capture(emu: &mut dyn Emulator, fed: u64) -> Self {
        let offset = emu.display_offset();
        emu.scroll(Scroll::Bottom);
        let mut screen = emu.snapshot();
        let mut links = Vec::new();
        for row in &mut screen.lines {
            relink(row, &screen.links, &mut links);
        }
        let total = screen.history as usize;
        let rows = (screen.rows as usize).max(1);
        let mut history: Vec<Vec<Run8>> = Vec::with_capacity(total);
        if total > 0 {
            emu.scroll(Scroll::Top);
            // Each page adds the rows it shows that are not kept yet; an
            // engine that skips rows ends the history there.
            for _ in 0..=total / rows + 1 {
                let page = emu.snapshot();
                let top = total.saturating_sub(page.display_offset as usize);
                for (i, mut row) in page.lines.into_iter().enumerate() {
                    if top + i == history.len() && history.len() < total {
                        relink(&mut row, &page.links, &mut links);
                        history.push(row);
                    }
                }
                if history.len() >= total || page.display_offset == 0 {
                    break;
                }
                emu.scroll(Scroll::Delta(-(rows as i32)));
            }
            emu.scroll(Scroll::Bottom);
        }
        screen.links = links;
        screen.history = history.len() as u32;
        screen.display_offset = 0;
        let mut frozen = Self { screen, history, offset: 0, fed, images: HashMap::new() };
        frozen.scroll(Scroll::Delta(offset as i32));
        frozen
    }

    /// Encodes the images placed on the screen as PNG files of at most
    /// `max` bytes while the engine still has their pixels: a client that
    /// shows the terminal later (a reconnect) still gets them (R2-B-06).
    pub fn keep_images(&mut self, emu: &mut dyn Emulator, max: usize) {
        let ids: HashSet<u32> = self.screen.images.iter().map(|image| image.placement.image_id).collect();
        for id in ids {
            let Some(image) = emu.image(id).filter(|image| super::image_size_ok(image.width, image.height)) else { continue };
            if let Some(png) = super::encode_png(image.width, image.height, &image.rgba).filter(|png| png.len() <= max) {
                self.images.insert(id, FrozenImage { generation: image.generation, width: image.width, height: image.height, png: png.into() });
            }
        }
    }

    /// Keeps the screen only ([`Recent`]).
    fn drop_history(&mut self) {
        self.history = Vec::new();
        self.offset = 0;
        self.screen.history = 0;
    }

    /// Scrollback rows kept.
    pub fn history_rows(&self) -> usize {
        self.history.len()
    }

    /// A blank screen (an engine that failed before anything was kept).
    pub fn blank(cols: u16, rows: u16) -> Self {
        Self { screen: Snapshot { cols, rows, lines: vec![Vec::new(); rows as usize], ..Snapshot::default() }, ..Self::default() }
    }

    /// Scrolls like the engine did: towards history (positive), or to an end.
    pub fn scroll(&mut self, scroll: Scroll) {
        let most = self.history.len() as i64;
        self.offset = match scroll {
            Scroll::Top => most,
            Scroll::Bottom => 0,
            Scroll::Delta(delta) => (self.offset as i64 + delta as i64).clamp(0, most),
        } as u32;
    }

    /// The viewport now.
    pub fn snapshot(&self) -> Snapshot {
        if self.offset == 0 {
            return self.screen.clone();
        }
        let rows = self.screen.lines.len();
        let all = self.history.len() + rows;
        let start = all.saturating_sub(rows + self.offset as usize);
        let lines = (start..start + rows).map(|i| if i < self.history.len() { self.history[i].clone() } else { self.screen.lines[i - self.history.len()].clone() }).collect();
        let mut images = self.screen.images.clone();
        for image in &mut images {
            image.placement.row += self.offset as i32;
        }
        Snapshot { lines, cursor: None, display_offset: self.offset, images, ..self.screen.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::{Config, alacritty::AlacrittyEmulator, feed_collect};

    #[test]
    fn the_screen_and_scrollback_survive_the_engine() {
        let mut emu = AlacrittyEmulator::new(&Config { cols: 30, rows: 5, scrollback: 100, kitty_keyboard: false, pull_scrollback: true });
        let lines: Vec<String> = (1..=23).map(|i| format!("line {i}")).collect();
        feed_collect(&mut emu, lines.join("\r\n").as_bytes());
        feed_collect(&mut emu, b"\r\n\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ end");
        let live = emu.snapshot();
        emu.scroll(Scroll::Delta(2));
        let live_back = emu.snapshot();
        let mut frozen = Frozen::capture(&mut emu, 1234);
        drop(emu);
        assert_eq!(frozen.fed, 1234);
        assert_eq!(frozen.snapshot().text(), live_back.text(), "scrolled back as the engine was");
        frozen.scroll(Scroll::Bottom);
        let screen = frozen.snapshot();
        assert_eq!((screen.text(), screen.cursor, screen.history), (live.text(), live.cursor, live.history));
        let link = screen.lines[4].iter().find(|run| run.text == "link").expect("the link row");
        assert_eq!(screen.links[link.link as usize - 1], "https://example.com");
        frozen.scroll(Scroll::Top);
        let top = frozen.snapshot();
        assert_eq!(top.text().lines().next(), Some("line 1"), "the whole history: {}", top.text());
        assert_eq!((top.display_offset, top.cursor), (live.history, None));
        frozen.scroll(Scroll::Delta(-3));
        assert_eq!(frozen.snapshot().text().lines().next(), Some("line 4"));
        frozen.scroll(Scroll::Delta(-1000));
        assert_eq!(frozen.snapshot(), screen);
    }

    /// R2-B-06: exited terminals keep their scrollback while they are among
    /// the most recently exited or scrolled; the others keep their screen.
    #[test]
    fn only_the_most_recent_frozen_screens_keep_their_scrollback() {
        let recent = Recent::new(3);
        let make = |rows: usize| {
            let screen = Snapshot { rows: 5, lines: vec![Vec::new(); 5], history: rows as u32, ..Snapshot::default() };
            Arc::new(Mutex::new(Frozen { screen, history: vec![vec![Run8 { text: "x".repeat(80), ..Run8::default() }]; rows], ..Frozen::default() }))
        };
        let all: Vec<_> = (0..5).map(|_| make(100)).collect();
        for frozen in &all[..4] {
            recent.touch(frozen);
        }
        assert_eq!(lock(&all[0]).history_rows(), 0, "the least recent of four keeps its screen only");
        assert_eq!(lock(&all[0]).snapshot().history, 0);
        assert!(all[1..4].iter().all(|f| lock(f).history_rows() == 100));
        // One scrolled is the most recent again; the next to exit pushes out another.
        recent.touch(&all[1]);
        recent.touch(&all[4]);
        assert_eq!(lock(&all[2]).history_rows(), 0);
        assert!([1, 3, 4].iter().all(|&i| lock(&all[i]).history_rows() == 100));
        // A soak: 200 exits keep at most the cap's histories.
        let recent = Recent::new(KEPT_HISTORIES);
        let many: Vec<_> = (0..200).map(|_| make(1000)).collect();
        for frozen in &many {
            recent.touch(frozen);
        }
        let kept: usize = many.iter().map(|f| lock(f).history_rows()).sum();
        assert_eq!(kept, KEPT_HISTORIES * 1000);
        drop(many);
        assert!(lock(&recent.list).iter().all(|kept| kept.strong_count() == 0), "dropped screens are not kept alive");
        recent.touch(&all[0]);
        assert_eq!(lock(&recent.list).len(), 1, "and leave the list");
    }

    #[test]
    fn a_screen_without_history() {
        let mut emu = AlacrittyEmulator::new(&Config { cols: 20, rows: 4, scrollback: 100, kitty_keyboard: false, pull_scrollback: true });
        feed_collect(&mut emu, b"only\r\nthis");
        let mut frozen = Frozen::capture(&mut emu, 10);
        frozen.scroll(Scroll::Top);
        assert_eq!(frozen.snapshot().text().lines().take(2).collect::<Vec<_>>(), ["only", "this"]);
        assert_eq!(Frozen::blank(20, 4).snapshot().lines.len(), 4);
    }
}
