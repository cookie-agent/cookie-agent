//! Webhook IDs and URL tokens (`docs/site/specs/webhooks.md` section 4).

use base64::Engine as _;

/// The characters of a webhook ID, as of a cron job's.
const ID_ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
/// A URL token's length: 64 bits in base64url without padding.
pub const URL_TOKEN_LEN: usize = 11;

/// The OS's random source failed.
#[derive(Debug, thiserror::Error)]
#[error("no random bytes: {0}")]
pub struct RandomError(String);

fn random<const N: usize>() -> Result<[u8; N], RandomError> {
    let mut bytes = [0_u8; N];
    getrandom::getrandom(&mut bytes).map_err(|error| RandomError(error.to_string()))?;
    Ok(bytes)
}

/// A new webhook ID: eight lowercase letters and digits that `taken`
/// refuses, from the OS's random source.
pub fn new_webhook_id(mut taken: impl FnMut(&str) -> bool) -> Result<String, RandomError> {
    loop {
        let bytes = random::<8>()?;
        let id = bytes
            .iter()
            .map(|byte| char::from(ID_ALPHABET[usize::from(*byte) % ID_ALPHABET.len()]))
            .collect::<String>();
        if !taken(&id) {
            return Ok(id);
        }
    }
}

/// Whether `id` has a webhook ID's shape.
#[must_use]
pub fn is_webhook_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|byte| ID_ALPHABET.contains(&byte))
}

/// A new URL token: 64 bits from the OS's random source, base64url.
pub fn new_url_token() -> Result<String, RandomError> {
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random::<8>()?))
}

/// Whether `token` has a URL token's shape, which the HTTP route checks
/// before looking it up.
#[must_use]
pub fn is_url_token(token: &str) -> bool {
    token.len() == URL_TOKEN_LEN
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        && base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token)
            .is_ok_and(|bytes| bytes.len() == 8)
}

pub(crate) fn random_secret_bytes() -> Result<[u8; 32], RandomError> {
    random::<32>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_tokens_have_their_shapes() {
        for _ in 0..64 {
            let id = new_webhook_id(|_| false).unwrap();
            assert!(is_webhook_id(&id), "{id}");
            let token = new_url_token().unwrap();
            assert!(is_url_token(&token), "{token}");
        }
    }

    #[test]
    fn a_taken_id_is_drawn_again() {
        let first = new_webhook_id(|_| false).unwrap();
        let mut refused = 0;
        let next = new_webhook_id(|id| {
            refused += 1;
            refused == 1 || id == first
        })
        .unwrap();
        assert!(is_webhook_id(&next));
        assert!(refused >= 2);
    }

    #[test]
    fn malformed_tokens_and_ids_are_refused() {
        for token in ["", "short", "AAAAAAAAAAAA", "AAAAAAAAA/A", "AAAAAAAAA=A"] {
            assert!(!is_url_token(token), "{token}");
        }
        // Eleven characters carry 66 bits; the last two must be zero.
        assert!(is_url_token("AAAAAAAAAAA"));
        for id in ["", "abcdefg", "abcdefghi", "ABCDEFGH", "abcd-fgh"] {
            assert!(!is_webhook_id(id), "{id}");
        }
    }
}
