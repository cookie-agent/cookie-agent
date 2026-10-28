//! Unpadded base64url, as PKCE and JWTs use it.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

pub(super) fn encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(super) fn decode(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')).ok()
}
