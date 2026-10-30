//! Terminal sizes with several clients (tmux "latest").
//!
//! A hosted terminal has one size (its ConPTY), but each connected client may
//! want another: the desktop's tile, the iPhone's screen. Every client's wish
//! (its last `Resize`) is remembered; the terminal takes the wish of its
//! *owner*: the client that most recently typed in it or claimed it
//! (`ClientMsg::Claim`). Without an owner, or when the owner never asked for a
//! size (the iPhone's mirror mode), the newest wish wins, which is exactly the
//! old "last `Resize` wins" behaviour for a single client. When the owner
//! leaves, the newest remaining wish applies.

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
        self.owner = Some(client);
        self.target()
    }

    /// `client` disconnected (or stopped showing the terminal): its wish is
    /// forgotten and, if it owned the terminal, nobody owns it any more.
    /// Returns the size to apply now.
    pub fn leave(&mut self, client: u64) -> Option<(u16, u16)> {
        self.wishes.retain(|w| w.client != client);
        if self.owner == Some(client) {
            self.owner = None;
        }
        self.target()
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
    fn a_non_owner_leaving_changes_nothing() {
        let mut p = SizePolicy::default();
        p.wish(DESK, 140, 45);
        p.claim(DESK);
        p.wish(PHONE, 48, 30);
        assert_eq!(p.leave(PHONE), Some((140, 45)));
        assert_eq!(p.owner(), Some(DESK));
    }
}
