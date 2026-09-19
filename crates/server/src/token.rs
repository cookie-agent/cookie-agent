//! Per-run localhost daemon bearer token and its stdout handoff frame.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Serialize;
use zeroize::Zeroizing;

/// Random bytes encoded into each daemon token.
const TOKEN_BYTES: usize = 32;
/// Length in characters of a base64url-encoded 32-byte token.
pub(crate) const TOKEN_ENCODED_BYTES: usize = 43;
/// Literal prefix that marks the single machine-readable startup frame.
pub const READY_LINE_PREFIX: &str = "daemon-ready ";

/// Errors returned while preparing the per-run daemon authentication token.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// The operating system could not supply random bytes.
    #[error("could not obtain randomness for the daemon authentication token")]
    Random(#[source] getrandom::Error),
}

/// Generates a fresh random 32-byte daemon bearer token as 43 base64url
/// characters. The token is kept only in memory and dies with the process.
pub fn generate_token() -> Result<Zeroizing<String>, TokenError> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::getrandom(&mut bytes).map_err(TokenError::Random)?;
    Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(bytes)))
}

/// Formats the single machine-readable startup frame a supervisor parses.
///
/// The daemon writes this as its first stdout line; every other stdout line is
/// human logging that parsers must ignore. Parsers match [`READY_LINE_PREFIX`].
pub fn ready_line(url: &str, token: &str) -> String {
    #[derive(Serialize)]
    struct ReadyFrame<'a> {
        url: &'a str,
        token: &'a str,
    }

    let frame = serde_json::to_string(&ReadyFrame { url, token })
        .expect("serializing a two-string ready frame cannot fail");
    format!("{READY_LINE_PREFIX}{frame}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn generated_tokens_are_43_char_base64url_and_distinct() {
        let mut seen = HashSet::new();
        for _ in 0..64 {
            let token = generate_token().expect("token");
            assert_eq!(token.len(), TOKEN_ENCODED_BYTES);
            assert!(
                token
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
                "{}",
                token.as_str()
            );
            assert!(seen.insert(token.to_string()));
        }
    }

    #[test]
    fn ready_line_is_one_json_frame_with_the_documented_prefix() {
        let token = "A".repeat(TOKEN_ENCODED_BYTES);
        let line = ready_line("ws://127.0.0.1:7419/ws", &token);
        let payload = line
            .strip_prefix(READY_LINE_PREFIX)
            .expect("daemon-ready prefix");
        let value: serde_json::Value = serde_json::from_str(payload).expect("json frame");
        assert_eq!(value["url"].as_str(), Some("ws://127.0.0.1:7419/ws"));
        assert_eq!(value["token"].as_str(), Some(token.as_str()));
        assert_eq!(
            payload,
            format!(r#"{{"url":"ws://127.0.0.1:7419/ws","token":"{token}"}}"#)
        );
    }
}
