//! Terminal sizes with several clients (tmux "latest").
//!
//! A hosted terminal has one size (its ConPTY), but each connected client may
//! want another: the desktop's tile, the iPhone's screen. Every client's wish
//! (its last `Resize`) is remembered; the terminal takes the wish of its
//! *owner*: the client that most recently typed in it or claimed it
//! (`ClientMsg::Claim`). Without an owner, or when the owner never asked for a
//! size (the iPhone's mirror mode), the newest wish wins, which is exactly the
//! old "last `Resize` wins" behaviour for a single client. When the owner
//! leaves, the newest remaining wish applies, after [`LEAVE_GRACE`]: a tile
//! hidden for a moment (a click on a stopped box shows its details instead)
//! or a connection its own reconnection replaces would otherwise resize the
//! terminal to another client's size and straight back.

use std::time::{Duration, Instant};

/// How long a leaving owner's wish still holds while other clients wish
/// another size.
pub const LEAVE_GRACE: Duration = Duration::from_secs(1);

/// One client's wanted size for a terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Wish {
    client: u64,
    cols: u16,
    rows: u16,
    /// Order of arrival (newest wins without an owner).
    seq: u64,
}

/// The size state of one terminal.
#[derive(Clone, Debug, Default)]
pub struct SizePolicy {
    wishes: Vec<Wish>,
    owner: Option<u64>,
    next: u64,
    /// The owner left: its wish holds until then ([`SizePolicy::leave_at`]).
    leaving: Option<(u64, Instant)>,
}

impl SizePolicy {
    /// The size the terminal should have now (`None`: nobody asked; keep it).
    pub fn target(&self) -> Option<(u16, u16)> {
        let owned = self.owner.and_then(|owner| self.wishes.iter().find(|w| w.client == owner));
        owned.or_else(|| self.wishes.iter().max_by_key(|w| w.seq)).map(|w| (w.cols, w.rows))
    }

    /// The client whose size the terminal follows, if any.
    pub fn owner(&self) -> Option<u64> {
        self.owner
    }

    /// `client` asked for `cols`×`rows` (`Resize`, or the size of its launch).
    /// Returns the size to apply now.
    pub fn wish(&mut self, client: u64, cols: u16, rows: u16) -> Option<(u16, u16)> {
        self.stay(client);
        self.next += 1;
        let seq = self.next;
        match self.wishes.iter_mut().find(|w| w.client == client) {
            Some(wish) => *wish = Wish { client, cols, rows, seq },
            None => self.wishes.push(Wish { client, cols, rows, seq }),
        }
        self.target()
    }

    /// `client` typed in the terminal or claimed it: it becomes the owner.
    /// Returns the size to apply now.
    pub fn claim(&mut self, client: u64) -> Option<(u16, u16)> {
        self.stay(client);
        // A leaving owner another client takes over from is gone at once.
        if let Some((leaver, _)) = self.leaving.take() {
            self.wishes.retain(|w| w.client != leaver);
        }
        self.owner = Some(client);
        self.target()
    }

    /// `client` disconnected (or stopped showing the terminal): its wish is
    /// forgotten and, if it owned the terminal, nobody owns it any more.
    /// Returns the size to apply now.
    pub fn leave(&mut self, client: u64) -> Option<(u16, u16)> {
        if self.leaving.is_some_and(|(leaver, _)| leaver == client) {
            self.leaving = None;
        }
        self.wishes.retain(|w| w.client != client);
        if self.owner == Some(client) {
            self.owner = None;
        }
        self.target()
    }

    /// [`SizePolicy::leave`] at `now`, except that an owner with a wish
    /// whose leaving would hand the terminal to another client's size keeps
    /// it for [`LEAVE_GRACE`] ([`SizePolicy::expire`] ends it; a wish or claim
    /// of its own meanwhile keeps it). Returns the size to apply now.
    pub fn leave_at(&mut self, client: u64, now: Instant) -> Option<(u16, u16)> {
        let owns_a_wish = self.owner == Some(client) && self.wishes.iter().any(|w| w.client == client);
        if owns_a_wish && self.wishes.iter().any(|w| w.client != client) {
            self.leaving = Some((client, now + LEAVE_GRACE));
            return self.target();
        }
        self.leave(client)
    }

    /// Ends a leaving owner's grace once it is over at `now`: the size to
    /// apply then (`Some(None)`: nobody asked any more), or `None` when
    /// nothing ended.
    pub fn expire(&mut self, now: Instant) -> Option<Option<(u16, u16)>> {
        let (client, until) = self.leaving?;
        (until <= now).then(|| self.leave(client))
    }

    /// A leaving owner's grace ends then.
    pub fn leaving_until(&self) -> Option<Instant> {
        self.leaving.map(|(_, until)| until)
    }

    /// `client` is back (a wish or a claim): it is not leaving any more.
    fn stay(&mut self, client: u64) {
        if self.leaving.is_some_and(|(leaver, _)| leaver == client) {
            self.leaving = None;
        }
    }

    /// Nothing is remembered (no wish, no owner).
    pub fn is_empty(&self) -> bool {
        self.wishes.is_empty() && self.owner.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESK: u64 = 1;
    const PHONE: u64 = 2;

    #[test]
    fn a_single_client_behaves_as_before_last_resize_wins() {
        let mut p = SizePolicy::default();
        assert_eq!(p.target(), None, "nobody asked");
        assert_eq!(p.wish(DESK, 120, 40), Some((120, 40)));
        assert_eq!(p.wish(DESK, 100, 30), Some((100, 30)));
        assert_eq!(p.claim(DESK), Some((100, 30)));
        assert_eq!(p.wish(DESK, 90, 30), Some((90, 30)), "the owner's own resizes apply");
    }

    #[test]
    fn the_client_that_typed_last_sets_the_size() {
        let mut p = SizePolicy::default();
        p.wish(DESK, 140, 45);
        p.claim(DESK);
        // The phone opens the terminal in "fit" mode: its wish is recorded,
        // but the desktop still owns the terminal until the phone claims it.
        assert_eq!(p.wish(PHONE, 48, 30), Some((140, 45)));
        assert_eq!(p.claim(PHONE), Some((48, 30)), "phone typed (or claimed)");
        // The desktop re-sends its size (layout change, reconnect): recorded only.
        assert_eq!(p.wish(DESK, 150, 45), Some((48, 30)));
        assert_eq!(p.claim(DESK), Some((150, 45)), "back at the desk");
    }

    #[test]
    fn an_owner_without_a_wish_leaves_the_newest_wish_in_place() {
        let mut p = SizePolicy::default();
        p.wish(DESK, 140, 45);
        // Mirror mode: the phone types without ever asking for a size.
        assert_eq!(p.claim(PHONE), Some((140, 45)));
        assert_eq!(p.wish(DESK, 120, 40), Some((120, 40)), "the desktop's resizes still apply");
    }

    #[test]
    fn when_the_owner_leaves_the_newest_remaining_wish_applies() {
        let mut p = SizePolicy::default();
        p.wish(DESK, 140, 45);
        p.wish(PHONE, 48, 30);
        p.claim(PHONE);
        assert_eq!(p.leave(PHONE), Some((140, 45)), "phone went to the background");
        assert_eq!(p.owner(), None);
        assert_eq!(p.leave(DESK), None, "nobody left: keep the size");
        assert!(p.is_empty());
    }

    #[test]
    fn a_leaving_owner_keeps_its_size_for_a_moment() {
        let now = Instant::now();
        let mut p = SizePolicy::default();
        p.wish(PHONE, 48, 30);
        p.wish(DESK, 140, 45);
        p.claim(DESK);
        // The desktop hides the tile for a moment (another box selected).
        assert_eq!(p.leave_at(DESK, now), Some((140, 45)), "no resize to the phone's size yet");
        assert_eq!(p.leaving_until(), Some(now + LEAVE_GRACE));
        assert_eq!(p.expire(now + LEAVE_GRACE / 2), None);
        // It shows the tile again within the grace: nothing ever changed.
        assert_eq!(p.wish(DESK, 140, 45), Some((140, 45)));
        assert_eq!(p.leaving_until(), None);
        assert_eq!(p.owner(), Some(DESK));
        // Gone for good: the phone's size once the grace is over.
        p.leave_at(DESK, now);
        assert_eq!(p.expire(now + LEAVE_GRACE), Some(Some((48, 30))));
        assert_eq!(p.owner(), None);
        assert_eq!(p.expire(now + LEAVE_GRACE * 2), None, "ended once");
    }

    #[test]
    fn another_client_claiming_ends_the_grace_at_once() {
        let now = Instant::now();
        let mut p = SizePolicy::default();
        p.wish(PHONE, 48, 30);
        p.wish(DESK, 140, 45);
        p.claim(DESK);
        p.leave_at(DESK, now);
        assert_eq!(p.claim(PHONE), Some((48, 30)), "the phone typed");
        assert_eq!(p.leaving_until(), None);
        assert_eq!(p.leave(PHONE), None, "the desktop's wish went with its leaving");
    }

    #[test]
    fn leaving_alone_or_as_a_non_owner_is_immediate() {
        let now = Instant::now();
        let mut p = SizePolicy::default();
        p.wish(DESK, 140, 45);
        p.claim(DESK);
        assert_eq!(p.leave_at(DESK, now), None, "nobody else: keep the size, nothing to wait for");
        assert!(p.is_empty() && p.leaving_until().is_none());
        p.wish(DESK, 140, 45);
        p.claim(DESK);
        p.wish(PHONE, 48, 30);
        assert_eq!(p.leave_at(PHONE, now), Some((140, 45)));
        assert!(p.leaving_until().is_none());
    }

    #[test]
    fn a_non_owner_leaving_changes_nothing() {
        let mut p = SizePolicy::default();
        p.wish(DESK, 140, 45);
        p.claim(DESK);
        p.wish(PHONE, 48, 30);
        assert_eq!(p.leave(PHONE), Some((140, 45)));
        assert_eq!(p.owner(), Some(DESK));
    }
}
