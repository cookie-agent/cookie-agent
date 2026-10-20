//! What a key does in the composer, as in the TUI: Enter steers, Alt+Enter
//! queues, Shift+Enter, Ctrl+Enter, and Ctrl+J insert a newline, Escape
//! cancels, and Up in an empty composer recalls this client's newest
//! pending or queued message.

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

#[cfg(test)]
mod tests {
    use super::{ComposerAction, KeyPress, composer_action};

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
