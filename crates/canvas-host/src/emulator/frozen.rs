//! What an exited terminal keeps once its engine is gone: its screen and
//! scrollback as protocol-8 rows, scrollable as the engine was, with the
//! count of output bytes the engine took. A live engine holds far more
//! (cells, the parser, image storage) and a thread of its own.

use canvas_common::protocol::Run8;

use super::{Emulator, Scroll, Snapshot};

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
        let mut frozen = Self { screen, history, offset: 0, fed };
        frozen.scroll(Scroll::Delta(offset as i32));
        frozen
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
