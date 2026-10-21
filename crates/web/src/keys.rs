//! What a key does in the composer, as in the TUI: Enter steers, Alt+Enter
//! queues, Shift+Enter, Ctrl+Enter, and Ctrl+J insert a newline, Escape
//! cancels, and Up in an empty composer recalls this client's newest
//! pending or queued message. Also which buttons the composer shows, and
//! how tall its text grows.

/// One key press in the composer: the `KeyboardEvent.key` and modifiers.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyPress<'a> {
    pub key: &'a str,
    pub alt: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub meta: bool,
    /// The composer holds no text.
    pub empty: bool,
}

/// What the composer does with a key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComposerAction {
    /// Submit, steering a running run.
    Steer,
    /// Submit to the queue behind a running run.
    Queue,
    /// Insert a newline.
    Newline,
    /// Cancel the running run.
    Cancel,
    /// Take back this client's newest pending or queued message.
    Recall,
    /// The browser handles the key.
    Type,
}

/// The composer's action for `press`.
pub fn composer_action(press: KeyPress<'_>) -> ComposerAction {
    match press.key {
        "Enter" if press.alt => ComposerAction::Queue,
        "Enter" if press.shift || press.ctrl || press.meta => ComposerAction::Newline,
        "Enter" => ComposerAction::Steer,
        "j" | "J" if press.ctrl && !press.alt && !press.meta => ComposerAction::Newline,
        "Escape" => ComposerAction::Cancel,
        "ArrowUp" if press.empty && !press.alt && !press.shift && !press.ctrl => {
            ComposerAction::Recall
        }
        _ => ComposerAction::Type,
    }
}

/// The buttons at the composer's bottom right, by whether the session runs
/// (a run or a compaction) and whether there is anything to send.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComposerButtons {
    /// Send (Enter): starts a run; disabled with nothing to send.
    Send { enabled: bool },
    /// Stop (Escape): cancels the run or the compaction.
    Stop,
    /// Steer (Enter) and Queue (Alt+Enter), side by side.
    SteerAndQueue,
}

/// The composer's buttons for a session that is `running` and a draft that
/// `has_input` (text or attached files).
pub const fn composer_buttons(running: bool, has_input: bool) -> ComposerButtons {
    match (running, has_input) {
        (false, has_input) => ComposerButtons::Send { enabled: has_input },
        (true, false) => ComposerButtons::Stop,
        (true, true) => ComposerButtons::SteerAndQueue,
    }
}

/// The most lines the composer grows to before it scrolls.
pub const COMPOSER_MAX_LINES: u32 = 5;

/// The composer's height for text whose content is `content` px tall
/// (`scrollHeight` with the box at its smallest), with lines of `line` px
/// and `padding` px above and below them together: one line at least,
/// [`COMPOSER_MAX_LINES`] at most. The flag says the text scrolls inside.
pub fn composer_height(content: f64, line: f64, padding: f64) -> (f64, bool) {
    let smallest = line + padding;
    let largest = line * f64::from(COMPOSER_MAX_LINES) + padding;
    (content.clamp(smallest, largest), content > largest + 0.5)
}

#[cfg(test)]
mod tests {
    use super::{
        COMPOSER_MAX_LINES, ComposerAction, ComposerButtons, KeyPress, composer_action,
        composer_buttons, composer_height,
    };

    #[test]
    fn the_buttons_follow_the_run_and_the_draft() {
        assert_eq!(
            composer_buttons(false, false),
            ComposerButtons::Send { enabled: false }
        );
        assert_eq!(
            composer_buttons(false, true),
            ComposerButtons::Send { enabled: true }
        );
        assert_eq!(composer_buttons(true, false), ComposerButtons::Stop);
        assert_eq!(composer_buttons(true, true), ComposerButtons::SteerAndQueue);
    }

    #[test]
    fn the_composer_grows_from_one_line_to_five_then_scrolls() {
        let (line, padding) = (20.0, 24.0);
        assert_eq!(composer_height(0.0, line, padding), (44.0, false));
        assert_eq!(composer_height(44.0, line, padding), (44.0, false));
        assert_eq!(composer_height(84.0, line, padding), (84.0, false));
        let five = line * f64::from(COMPOSER_MAX_LINES) + padding;
        assert_eq!(composer_height(five, line, padding), (five, false));
        assert_eq!(composer_height(five + 20.0, line, padding), (five, true));
    }

    fn press(key: &str) -> KeyPress<'_> {
        KeyPress {
            key,
            ..KeyPress::default()
        }
    }

    #[test]
    fn keys_map_as_in_the_tui() {
        assert_eq!(composer_action(press("Enter")), ComposerAction::Steer);
        assert_eq!(
            composer_action(KeyPress {
                alt: true,
                ..press("Enter")
            }),
            ComposerAction::Queue
        );
        for newline in [
            KeyPress {
                shift: true,
                ..press("Enter")
            },
            KeyPress {
                ctrl: true,
                ..press("Enter")
            },
            KeyPress {
                ctrl: true,
                ..press("j")
            },
        ] {
            assert_eq!(composer_action(newline), ComposerAction::Newline);
        }
        assert_eq!(composer_action(press("Escape")), ComposerAction::Cancel);
        assert_eq!(
            composer_action(KeyPress {
                empty: true,
                ..press("ArrowUp")
            }),
            ComposerAction::Recall
        );
        // Up in a draft moves the caret.
        assert_eq!(composer_action(press("ArrowUp")), ComposerAction::Type);
        assert_eq!(composer_action(press("j")), ComposerAction::Type);
        assert_eq!(composer_action(press("a")), ComposerAction::Type);
    }
}
