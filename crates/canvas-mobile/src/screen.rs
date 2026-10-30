//! A terminal's screen for the app: the rows that changed since the app last
//! drew it (or all of them), as grapheme cells with their style.

use std::collections::BTreeSet;

use canvas_common::protocol::{Frame8, TermInfo};
use canvas_termview::{TermView, user_message_rows};

use crate::model::{Screen, ScreenRow, ScreenRun};

#[derive(Default)]
pub struct ViewState {
    pub view: TermView,
    /// Rows changed since the screen was last taken.
    dirty: BTreeSet<u16>,
    /// Everything changed (a full frame or a new size).
    full: bool,
}

impl ViewState {
    pub fn apply(&mut self, frame: Frame8) {
        let resized = frame.cols != self.view.cols || frame.rows != self.view.rows;
        if frame.full || resized {
            self.full = true;
            self.dirty.clear();
        } else if !self.full {
            self.dirty.extend(frame.changed.iter().map(|(y, _)| *y));
        }
        self.view.apply8(frame);
    }

    /// The next `take` returns every row.
    pub fn force_full(&mut self) {
        self.full = true;
        self.dirty.clear();
    }

    /// What changed since the last call (`full`: every row). `copilot`: tint
    /// your own prompts the way the desktop does.
    pub fn take(&mut self, id: &str, term: Option<&TermInfo>, copilot: bool, full: bool) -> Screen {
        let full = full || std::mem::take(&mut self.full);
        let rows: Vec<u16> = if full { (0..self.view.rows).collect() } else { std::mem::take(&mut self.dirty).into_iter().collect() };
        if full {
            self.dirty.clear();
        }
        let view = &self.view;
        let changed = rows
            .into_iter()
            .filter_map(|y| {
                let runs = view.rows8.get(y as usize)?;
                Some(ScreenRow {
                    y,
                    runs: runs
                        .iter()
                        .map(|run| ScreenRun {
                            col: run.col,
                            cells: run.cells().into_iter().map(str::to_owned).collect(),
                            wide: run.wide,
                            fg: run.fg,
                            bg: run.bg,
                            flags: run.flags,
                            underline: run.underline,
                            link: run.link,
                        })
                        .collect(),
                })
            })
            .collect();
        let user_rows = if copilot { user_message_rows(&view.lines) } else { vec![false; view.rows as usize] };
        // The service leaves the cursor out when it is hidden or scrolled away.
        let cursor = view.cursor;
        Screen {
            id: id.to_owned(),
            cols: view.cols,
            rows: view.rows,
            full,
            changed,
            user_rows,
            cursor_visible: cursor.is_some(),
            cursor_col: cursor.map_or(0, |c| c.0),
            cursor_row: cursor.map_or(0, |c| c.1),
            cursor_shape: view.cursor_shape,
            cursor_blink: view.cursor_blink,
            modes: view.modes,
            display_offset: view.display_offset,
            history: view.history,
            links: view.links.clone(),
            title: term.map(|t| t.title.clone()).unwrap_or_default(),
            alive: term.is_some_and(|t| t.alive),
        }
    }
}
