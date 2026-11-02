//! Whether the transcript follows its end while a reply streams in.
//!
//! Following pins the view to the end, but only when the end moves away:
//! the content grew (a new row, a longer reply) or the box shrank. A
//! stream event that changes nothing on screen, such as a delta inside a
//! folded thinking block, moves nothing. Any scroll the user makes away
//! from the end (wheel, keys, a finger, the scrollbar, a flick's momentum)
//! stops following at once, and nothing but the user coming back to the
//! end, or the jump-to-latest button, starts it again.

/// Pixels short of the end that still count as at it: what a fractional
/// scroll position loses to rounding.
const END_SLACK: i32 = 1;

/// Where a scroller is: its scroll position, its content's height, and its
/// box's height, in CSS pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metrics {
    pub top: i32,
    pub height: i32,
    pub view: i32,
}

impl Metrics {
    /// How far the end is below the bottom of the box.
    const fn gap(self) -> i32 {
        self.height - self.top - self.view
    }

    const fn at_end(self) -> bool {
        self.gap() <= END_SLACK
    }

    const fn overflows(self) -> bool {
        self.height > self.view + END_SLACK
    }
}

/// The follow state of one transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Follow {
    following: bool,
    /// The scroll position the last scroll reported.
    last_top: i32,
    /// Where the last pin put the view, until its scroll reports it.
    pinned: Option<i32>,
}

impl Default for Follow {
    /// A transcript opens at its end, following it.
    fn default() -> Self {
        Self {
            following: true,
            last_top: 0,
            pinned: None,
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
    /// following. A move up that ends at the end is content shrinking
    /// under the view (or the box growing), which changes nothing; so is
    /// the scroll of a pin, which may report after a wheel up stopped
    /// following.
    pub fn scrolled(&mut self, at: Metrics) {
        let up = at.top < self.last_top;
        self.last_top = at.top;
        if let Some(pinned) = self.pinned.take()
            && (at.top - pinned).abs() <= END_SLACK
        {
            return;
        }
        if at.at_end() {
            if !up {
                self.following = true;
            }
        } else if up {
            self.following = false;
        }
    }

    /// Where to scroll the view, now that the content or the box changed
    /// size: to the end, only while following, and only when the end moved
    /// away. Content that fits the box has nothing to follow until it
    /// overflows, when it is followed.
    pub fn pin(&mut self, at: Metrics) -> Option<i32> {
        if !at.overflows() {
            self.following = true;
            return None;
        }
        if !self.following || at.at_end() {
            return None;
        }
        let end = at.height - at.view;
        // A scroll that reports less than the pin moved the view up from it.
        self.last_top = end;
        self.pinned = Some(end);
        Some(end)
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

    /// A 500 px box over 2,000 px of content, scrolled to `top`.
    const fn at(top: i32) -> Metrics {
        Metrics {
            top,
            height: 2000,
            view: 500,
        }
    }

    /// `follow` after its view scrolled to the end, as a pin leaves it.
    fn at_end() -> Follow {
        let mut follow = Follow::default();
        follow.scrolled(at(1500));
        follow
    }

    #[test]
    fn a_growing_end_is_pinned_and_a_still_one_is_not() {
        let mut follow = at_end();
        // A delta inside a folded block: nothing grew, nothing to pin.
        assert_eq!(follow.pin(at(1500)), None);
        // A new row: the end moved 40 px down.
        let grown = Metrics {
            top: 1500,
            height: 2040,
            view: 500,
        };
        assert_eq!(follow.pin(grown), Some(1540));
        assert!(follow.following());
    }

    #[test]
    fn any_scroll_up_stops_following_however_small() {
        // A trackpad's single pixel, momentum, the scrollbar: whatever
        // moved the view up away from the end.
        for top in [1498, 1400, 0] {
            let mut follow = at_end();
            follow.scrolled(at(top));
            assert!(!follow.following(), "{top}");
            let grown = Metrics {
                top,
                height: 2400,
                view: 500,
            };
            assert_eq!(follow.pin(grown), None, "{top}");
        }
    }

    #[test]
    fn a_wheel_up_stops_following_before_its_scroll_arrives() {
        let mut follow = at_end();
        let grown = Metrics {
            top: 1500,
            height: 2040,
            view: 500,
        };
        assert_eq!(follow.pin(grown), Some(1540));
        // The wheel comes before the pin's scroll reports the end.
        follow.back();
        follow.scrolled(Metrics { top: 1540, ..grown });
        assert!(!follow.following(), "the pin's scroll took the wheel back");
        // A one-pixel step stays within the slack, yet it stopped following.
        follow.scrolled(Metrics { top: 1539, ..grown });
        assert!(!follow.following());
        let grown = Metrics {
            top: 1539,
            height: 2100,
            view: 500,
        };
        assert_eq!(follow.pin(grown), None);
    }

    #[test]
    fn a_scroll_up_that_lands_with_a_pin_still_stops_following() {
        let mut follow = at_end();
        let grown = Metrics {
            top: 1500,
            height: 2040,
            view: 500,
        };
        assert_eq!(follow.pin(grown), Some(1540));
        // The finger's scroll and the pin report as one, short of the end.
        follow.scrolled(Metrics { top: 1460, ..grown });
        assert!(!follow.following());
    }

    #[test]
    fn a_small_scroll_up_after_a_pin_stops_following_before_the_pin_reports() {
        let mut follow = at_end();
        // Content grew between scroll reports and was pinned; the user's
        // 30 px up lands before the pin's scroll reports, and the one
        // scroll that does is still below the last one reported.
        let grown = Metrics {
            top: 1500,
            height: 2100,
            view: 500,
        };
        assert_eq!(follow.pin(grown), Some(1600));
        follow.scrolled(Metrics { top: 1570, ..grown });
        assert!(!follow.following());
    }

    #[test]
    fn coming_back_to_the_end_resumes_following() {
        let mut follow = at_end();
        follow.scrolled(at(900));
        follow.scrolled(at(1200));
        assert!(!follow.following(), "on the way down, short of the end");
        follow.scrolled(at(1500));
        assert!(follow.following());
    }

    #[test]
    fn content_shrinking_under_a_scrolled_up_view_does_not_resume_following() {
        let mut follow = at_end();
        follow.scrolled(at(1400));
        // A block folded below: the browser moves the view up to the new
        // end, which the user did not come back to.
        let shrunk = Metrics {
            top: 1300,
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
            top: 1300,
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
        follow.forward(at(1200));
        assert!(!follow.following());
        follow.forward(at(1500));
        assert!(follow.following());
    }

    #[test]
    fn content_that_fits_follows_once_it_overflows() {
        let mut follow = at_end();
        follow.back();
        let fits = Metrics {
            top: 0,
            height: 400,
            view: 500,
        };
        assert_eq!(follow.pin(fits), None);
        assert!(follow.following());
        let overflows = Metrics {
            top: 0,
            height: 700,
            view: 500,
        };
        assert_eq!(follow.pin(overflows), Some(200));
    }

    #[test]
    fn a_message_sent_while_scrolled_up_follows_again_as_its_row_arrives() {
        let mut follow = at_end();
        follow.back();
        follow.scrolled(at(600));
        follow.jump();
        assert_eq!(follow.pin(at(600)), Some(1500));
        follow.scrolled(at(1500));
        // The message's row, then the reply, grow the end: followed.
        let grown = Metrics {
            top: 1500,
            height: 2080,
            view: 500,
        };
        assert_eq!(follow.pin(grown), Some(1580));
        assert!(follow.following());
    }

    #[test]
    fn the_jump_button_follows_again() {
        let mut follow = at_end();
        follow.scrolled(at(100));
        follow.jump();
        assert!(follow.following());
        assert_eq!(follow.pin(at(100)), Some(1500));
    }
}
