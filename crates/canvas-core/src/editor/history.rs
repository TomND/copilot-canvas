//! Undo and redo: steps of edits, grouping of typing and deleting, and the
//! saved revision for dirty tracking.

use super::Pos;
use super::text::end_of;

/// Steps kept (older ones are dropped).
const MAX_STEPS: usize = 2000;

/// What started a step: typing and deleting group with the previous step of
/// the same kind; everything else is a step of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Type,
    Delete,
    Other,
}

/// One replacement: `removed` (at `at`) was replaced by `inserted`.
#[derive(Clone, Debug)]
pub(super) struct Edit {
    pub at: Pos,
    pub removed: String,
    pub inserted: String,
}

/// The cursor and the selection anchor.
pub(super) type Sel = (Pos, Option<Pos>);

#[derive(Clone, Debug)]
pub(super) struct Step {
    pub edits: Vec<Edit>,
    pub before: Sel,
    pub after: Sel,
    kind: Kind,
    /// The revision this step leads to (unique while the editor lives).
    id: u64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct History {
    undo: Vec<Step>,
    redo: Vec<Step>,
    next: u64,
    saved: u64,
    /// The revision under the oldest kept step (0: the loaded text).
    base: u64,
    /// The next change starts a new step even if it could group.
    sealed: bool,
    /// While a change is open: whether its step is new, and edits recorded.
    open: Option<(bool, usize)>,
}

impl History {
    /// Forgets every step; the current text is the saved one.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.saved = 0;
        self.base = 0;
        self.sealed = true;
        self.open = None;
    }

    fn revision(&self) -> u64 {
        self.undo.last().map_or(self.base, |s| s.id)
    }

    pub fn dirty(&self) -> bool {
        self.revision() != self.saved
    }

    pub fn mark_saved(&mut self) {
        self.saved = self.revision();
        self.sealed = true;
    }

    /// The caret moved (or similar): typing after this starts a new step.
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Opens a change of `kind` made with the cursor and anchor `sel`.
    pub fn begin(&mut self, kind: Kind, sel: Sel) {
        let group = kind != Kind::Other && !self.sealed && self.redo.is_empty() && self.undo.last().is_some_and(|s| s.kind == kind && s.after == sel);
        if group {
            self.open = Some((false, 0));
        } else {
            self.undo.push(Step { edits: Vec::new(), before: sel, after: sel, kind, id: 0 });
            self.open = Some((true, 0));
        }
    }

    /// Reopens the last step for an edit that belongs to it (the app's,
    /// right after it); false when there is none or undone steps wait.
    pub fn reopen(&mut self) -> bool {
        if self.undo.is_empty() || !self.redo.is_empty() {
            return false;
        }
        self.open = Some((false, 0));
        true
    }

    /// Records an edit of the open change.
    pub fn record(&mut self, edit: Edit) {
        let (Some((_, count)), Some(step)) = (self.open.as_mut(), self.undo.last_mut()) else { return };
        *count += 1;
        // Typing at the end of the previous insertion extends it.
        if step.kind == Kind::Type
            && edit.removed.is_empty()
            && let Some(last) = step.edits.last_mut()
            && last.removed.is_empty()
            && end_of(last.at, &last.inserted) == edit.at
        {
            last.inserted.push_str(&edit.inserted);
            return;
        }
        step.edits.push(edit);
    }

    /// Closes the open change; answers whether anything was recorded.
    pub fn finish(&mut self, sel: Sel) -> bool {
        let Some((new, count)) = self.open.take() else { return false };
        if count == 0 {
            if new {
                self.undo.pop();
            }
            return false;
        }
        self.next += 1;
        let Some(step) = self.undo.last_mut() else { return false };
        step.id = self.next;
        step.after = sel;
        self.sealed = step.kind == Kind::Other;
        self.redo.clear();
        if self.undo.len() > MAX_STEPS {
            self.base = self.undo.remove(0).id;
        }
        true
    }

    /// The step to take back (moved to the redo stack by [`Self::undone`]).
    pub fn pop_undo(&mut self) -> Option<Step> {
        self.sealed = true;
        self.undo.pop()
    }

    pub fn undone(&mut self, step: Step) {
        self.redo.push(step);
    }

    pub fn pop_redo(&mut self) -> Option<Step> {
        self.sealed = true;
        self.redo.pop()
    }

    pub fn redone(&mut self, step: Step) {
        self.undo.push(step);
    }
}
