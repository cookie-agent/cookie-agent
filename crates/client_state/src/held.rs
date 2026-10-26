//! Held sessions in the clients (`docs/site/specs/held-sessions.md`): why a
//! session's automatic runs stopped, what waits, and the Resume that clears
//! the hold.

use cookie_agent_protocol::{HeldReason, Open};

use crate::SessionState;

/// Why a hold happened, for its transcript row and the composer's line:
/// `stopped by you`, `failed 4 times in a row: <error>`.
pub fn reason_label(reason: &Open<HeldReason>) -> String {
    match reason {
        Open::Known(HeldReason::Stopped {}) => "stopped by you".to_owned(),
        Open::Known(HeldReason::Failing { failures, error }) => match error {
            Some(error) => format!("failed {failures} times in a row: {error}"),
            None => format!("failed {failures} times in a row"),
        },
        _ => "held".to_owned(),
    }
}

/// The transcript row a hold adds.
pub fn held_row(reason: &Open<HeldReason>) -> String {
    format!(
        "Automatic runs held: {}. Nothing runs on its own until you resume.",
        reason_label(reason)
    )
}

/// The producer messages waiting in a session: what a Resume sends.
pub fn waiting_count(state: &SessionState) -> usize {
    state
        .transcript
        .iter()
        .filter(|item| {
            matches!(
                item,
                crate::TranscriptItem::ProducerMessage {
                    status: crate::ProducerMessageStatus::Pending
                        | crate::ProducerMessageStatus::Admitted,
                    ..
                }
            )
        })
        .count()
}

/// The composer's line while `state` is held: `Held — stopped by you · 2
/// messages waiting`; `None` when it is not held.
pub fn composer_line(state: &SessionState) -> Option<String> {
    let reason = state.held.as_ref()?;
    let waiting = match waiting_count(state) {
        0 => String::new(),
        1 => " · 1 message waiting".to_owned(),
        count => format!(" · {count} messages waiting"),
    };
    Some(format!("Held — {}{waiting}", reason_label(reason)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_labels_say_why_and_what_waits() {
        let stopped = Open::Known(HeldReason::Stopped {});
        assert_eq!(reason_label(&stopped), "stopped by you");
        let failing = Open::Known(HeldReason::Failing {
            failures: 4,
            error: Some(
                cookie_agent_protocol::SafeErrorMessage::new("model failure: invalid_request")
                    .unwrap(),
            ),
        });
        assert_eq!(
            reason_label(&failing),
            "failed 4 times in a row: model failure: invalid_request"
        );
        let mut state = SessionState::default();
        assert_eq!(composer_line(&state), None);
        state.held = Some(stopped);
        assert_eq!(
            composer_line(&state).as_deref(),
            Some("Held — stopped by you")
        );
    }
}
