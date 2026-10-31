//! How the page says what went wrong. A daemon error is drawn from its
//! fields (its message, its hint, and its detail), never from the
//! error's display text, which already joins the message and the hint:
//! a page that showed that text and then the hint said the hint twice.
//! Each part is sanitized as the protocol's own display text is.

use cookie_agent_protocol::diagnostics;
use cookie_agent_protocol_transport::ClientError;

/// A failure in parts, for a status line or an error box.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ErrorText {
    pub message: String,
    /// What to do about it, when the daemon says and the message does not.
    pub hint: Option<String>,
    /// Technical detail, for an error box.
    pub detail: Option<String>,
}

impl ErrorText {
    /// The parts of `error`: a daemon error's fields, else its text.
    pub fn of(error: &ClientError) -> Self {
        let ClientError::Rpc(rpc) = error else {
            return Self {
                message: error.to_string(),
                hint: None,
                detail: None,
            };
        };
        let clean = |text: &str| diagnostics::detail(text.trim()).to_string();
        let message = clean(&rpc.message);
        let data = error.error_data().unwrap_or_default();
        let hint = data
            .hint
            .map(|hint| clean(&hint))
            .filter(|hint| !hint.is_empty() && !message.contains(hint.as_str()));
        let detail = error
            .detail()
            .map(|detail| clean(&detail))
            .filter(|detail| {
                !detail.is_empty() && *detail != message && hint.as_deref() != Some(detail.as_str())
            });
        Self {
            message,
            hint,
            detail,
        }
    }

    /// The message and the hint, on one line.
    pub fn line(&self) -> String {
        match &self.hint {
            Some(hint) => format!("{} {hint}", self.message),
            None => self.message.clone(),
        }
    }

    /// The message and the hint, then the detail on its own line.
    pub fn full(&self) -> String {
        match &self.detail {
            Some(detail) => format!("{}\n{detail}", self.line()),
            None => self.line(),
        }
    }
}

/// `error` on one line, for the status line.
pub fn describe(error: &ClientError) -> String {
    ErrorText::of(error).line()
}

/// `error` with its detail, for an error box.
pub fn describe_full(error: &ClientError) -> String {
    ErrorText::of(error).full()
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::JsonRpcError;
    use cookie_agent_protocol_transport::ClientError;
    use serde_json::json;

    use super::{ErrorText, describe, describe_full};

    fn rpc(message: &str, data: serde_json::Value) -> ClientError {
        ClientError::Rpc(JsonRpcError {
            code: -32000,
            message: message.into(),
            data: Some(data),
        })
    }

    #[test]
    fn a_daemon_error_says_its_hint_once() {
        let error = rpc(
            "The session changed after it was shown.",
            json!({"code": "session_changed", "hint": "Look at the session again."}),
        );
        // The display text joins them already; the page builds its own.
        assert_eq!(
            error.to_string(),
            "The session changed after it was shown. Look at the session again."
        );
        assert_eq!(
            describe(&error),
            "The session changed after it was shown. Look at the session again."
        );
        // A message that already says its hint does not repeat it.
        let repeated = rpc(
            "Not deleted. Look at the session again.",
            json!({"code": "session_changed", "hint": "Look at the session again."}),
        );
        assert_eq!(
            describe(&repeated),
            "Not deleted. Look at the session again."
        );
    }

    #[test]
    fn the_detail_goes_on_its_own_line_and_only_in_an_error_box() {
        let error = rpc(
            "The configuration was not changed.",
            json!({"code": "config_invalid", "hint": "Fix it and apply again.", "detail": "unknown field `colour`"}),
        );
        assert_eq!(
            ErrorText::of(&error),
            ErrorText {
                message: "The configuration was not changed.".into(),
                hint: Some("Fix it and apply again.".into()),
                detail: Some("unknown field `colour`".into()),
            }
        );
        assert_eq!(
            describe(&error),
            "The configuration was not changed. Fix it and apply again."
        );
        assert_eq!(
            describe_full(&error),
            "The configuration was not changed. Fix it and apply again.\nunknown field `colour`"
        );
        // Without registry data, and for a transport failure, the text is
        // the message.
        let bare = ClientError::Rpc(JsonRpcError {
            code: -32601,
            message: "Method not found".into(),
            data: None,
        });
        assert_eq!(describe_full(&bare), "Method not found");
        assert_eq!(describe(&ClientError::Closed), "transport closed");
    }
}
