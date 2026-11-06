//! Whether the transcript follows its end while a reply streams in.
//!
//! The transcript's scroller is `column-reverse`, so its scroll position
//! counts from its end: rows arriving above the view move nothing. Following
//! needs a pin only when the view is off the end and should not be: at the
//! end, the browser keeps it there as the content grows. A stream event that
//! changes nothing on screen, such as a delta inside a folded thinking
//! block, moves nothing. Any scroll the user makes away from the end (wheel,
//! keys, a finger, the scrollbar, a flick's momentum) stops following at
//! once, and nothing but the user coming back to the end, or the
//! jump-to-latest button, starts it again.

/// Pixels short of the end that still count as at it: what a fractional
/// scroll position loses to rounding.
const END_SLACK: i32 = 1;

/// Where a scroller is: how far its end is below the bottom of its box, its
/// content's height, and its box's height, in CSS pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metrics {
    pub from_end: i32,
    pub height: i32,
    pub view: i32,
}

impl Metrics {
    const fn at_end(self) -> bool {
        self.from_end <= END_SLACK
    }

    pub const fn overflows(self) -> bool {
        self.height > self.view + END_SLACK
    }
}

/// The follow state of one transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Follow {
    following: bool,
    /// How far from the end, and over how much content, the last scroll
    /// reported.
    last_from_end: i32,
    last_height: i32,
    /// Whether a pin's scroll has yet to report.
    pinned: bool,
}

impl Default for Follow {
    /// A transcript opens at its end, following it.
    fn default() -> Self {
        Self {
            following: true,
            last_from_end: 0,
            last_height: 0,
            pinned: false,
        }
    }
}

impl Follow {
    pub const fn following(self) -> bool {
        self.following
    }

    /// The user moved toward older rows: a wheel or a key up, a finger
    /// dragging the page down. Following stops before the scroll it starts
    /// arrives, so no pin in between can take it back.
    pub fn back(&mut self) {
        self.following = false;
    }

    /// The user moved toward the end: a wheel or a key down, a finger
    /// dragging the page up. At the end, where no scroll follows to say
    /// so, following resumes.
    pub fn forward(&mut self, at: Metrics) {
        if at.at_end() {
            self.following = true;
        }
    }

    /// The scroller scrolled, by the user or by a pin. Reaching the end on
    /// the way down follows it; any move up short of the end stops
    /// following. Reaching the end as the content shrinks under the view
    /// changes nothing, the user having come back to nothing; so does the
    /// scroll of a pin, which may report after a wheel up stopped following.
    pub fn scrolled(&mut self, at: Metrics) {
        let up = at.from_end > self.last_from_end;
        let shrunk = at.height < self.last_height;
        self.last_from_end = at.from_end;
        self.last_height = at.height;
        if std::mem::take(&mut self.pinned) && at.at_end() {
            return;
        }
        if at.at_end() {
            if !up && !shrunk {
                self.following = true;
            }
        } else if up {
            self.following = false;
        }
    }

    /// Whether to put the view back at the end, now that the content or the
    /// box changed size: only while following, and only when the view is off
    /// the end. Content that fits the box has nothing to follow until it
    /// overflows, when it is followed.
    pub fn pin(&mut self, at: Metrics) -> bool {
        if !at.overflows() {
            self.following = true;
            return false;
        }
        if !self.following || at.at_end() {
            return false;
        }
        // A scroll that reports farther than the pin moved the view up from
        // it.
        self.last_from_end = 0;
        self.pinned = true;
        true
    }

    /// The jump-to-latest button, or a message the user sends: back to the
    /// end, following it.
    pub fn jump(&mut self) {
        self.following = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{Follow, Metrics};

    /// A 500 px box over 2,000 px of content, `from_end` above its end.
    const fn at(from_end: i32) -> Metrics {
        Metrics {
            from_end,
            height: 2000,
            view: 500,
        }
    }

    /// `follow` after its view scrolled to the end, as a pin leaves it.
    fn at_end() -> Follow {
        let mut follow = Follow::default();
        follow.scrolled(at(0));
        follow
    }

    #[test]
    fn a_view_off_the_end_is_pinned_and_one_at_it_is_not() {
        let mut follow = at_end();
        // The browser keeps a view at the end there as the content grows.
        assert!(!follow.pin(at(0)));
        assert!(!follow.pin(Metrics {
            height: 2040,
            ..at(1)
        }));
        // A view the browser left off the end.
        assert!(follow.pin(at(40)));
        assert!(follow.following());
    }

    #[test]
    fn any_scroll_up_stops_following_however_small() {
        // A trackpad's few pixels, momentum, the scrollbar: whatever moved
        // the view up away from the end.
        for from_end in [2, 100, 1500] {
            let mut follow = at_end();
            follow.scrolled(at(from_end));
            assert!(!follow.following(), "{from_end}");
            assert!(!follow.pin(at(from_end)), "{from_end}");
        }
    }

    #[test]
    fn a_wheel_up_stops_following_before_its_scroll_arrives() {
        let mut follow = at_end();
        assert!(follow.pin(at(40)));
        // The wheel comes before the pin's scroll reports the end.
        follow.back();
        follow.scrolled(at(0));
        assert!(!follow.following(), "the pin's scroll took the wheel back");
        // A one-pixel step stays within the slack, yet it stopped following.
        follow.scrolled(at(1));
        assert!(!follow.following());
        assert!(!follow.pin(at(1)));
    }

    #[test]
    fn a_scroll_up_that_lands_with_a_pin_still_stops_following() {
        let mut follow = at_end();
        assert!(follow.pin(at(40)));
        // The finger's scroll and the pin report as one, short of the end.
        follow.scrolled(at(30));
        assert!(!follow.following());
    }

    #[test]
    fn rows_arriving_above_a_scrolled_up_view_change_nothing() {
        let mut follow = at_end();
        follow.scrolled(at(900));
        // A page of older history: the content grew, no scroll reports it,
        // and the next scroll up is still a scroll up.
        let grown = Metrics {
            from_end: 950,
            height: 8000,
            view: 500,
        };
        assert!(!follow.pin(grown));
        follow.scrolled(grown);
        assert!(!follow.following());
    }

    #[test]
    fn coming_back_to_the_end_resumes_following() {
        let mut follow = at_end();
        follow.scrolled(at(600));
        follow.scrolled(at(300));
        assert!(!follow.following(), "on the way down, short of the end");
        follow.scrolled(at(0));
        assert!(follow.following());
    }

    #[test]
    fn content_shrinking_under_a_scrolled_up_view_does_not_resume_following() {
        let mut follow = at_end();
        follow.scrolled(at(100));
        // A block folded below: the view lands on the new end, which the
        // user did not come back to.
        let shrunk = Metrics {
            from_end: 0,
            height: 1800,
            view: 500,
        };
        follow.scrolled(shrunk);
        assert!(!follow.following());
        // Their wheel down at the end, where nothing can scroll, resumes it.
        follow.forward(shrunk);
        assert!(follow.following());
    }

    #[test]
    fn content_shrinking_under_a_following_view_keeps_following() {
        let mut follow = at_end();
        let shrunk = Metrics {
            from_end: 0,
            height: 1800,
            view: 500,
        };
        follow.scrolled(shrunk);
        assert!(follow.following());
    }

    #[test]
    fn a_move_down_short_of_the_end_does_not_resume_following() {
        let mut follow = at_end();
        follow.back();
        follow.forward(at(300));
        assert!(!follow.following());
        follow.forward(at(0));
        assert!(follow.following());
    }

    #[test]
    fn content_that_fits_follows_once_it_overflows() {
        let mut follow = at_end();
        follow.back();
        let fits = Metrics {
            from_end: 0,
            height: 400,
            view: 500,
        };
        assert!(!follow.pin(fits));
        assert!(follow.following());
    }

    #[test]
    fn a_message_sent_while_scrolled_up_follows_again() {
        let mut follow = at_end();
        follow.back();
        follow.scrolled(at(900));
        follow.jump();
        assert!(follow.pin(at(900)));
        follow.scrolled(at(0));
        assert!(follow.following());
    }

    #[test]
    fn the_jump_button_follows_again() {
        let mut follow = at_end();
        follow.scrolled(at(1400));
        follow.jump();
        assert!(follow.following());
        assert!(follow.pin(at(1400)));
    }
}
