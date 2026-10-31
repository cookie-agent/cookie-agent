//! Sign in with ChatGPT: OpenAI's token sharing, which runs models on a
//! ChatGPT plan through the public Responses API.
//!
//! The sign-in is an OAuth authorization-code flow with PKCE against
//! `auth.openai.com`. The first sign-in registers a client for the user with
//! the shared `dynamic_agent_client` ID and keeps the client ID OpenAI issues
//! on the callback. Tokens live in the provider-store directory, outside the
//! provider store itself, so an hourly refresh never recompiles the model
//! runtime; the store holds only the signed-in account.

mod base64url;
pub mod catalog;
mod credentials;
mod sign_in;

use std::time::Duration;

pub use catalog::{ChatGptModel, ChatGptModelList, provider_entry};
pub use credentials::{AccountSummary, ChatGptCredentials, ChatGptEndpoints};
pub use sign_in::SignedIn;

/// The built-in provider's ID.
pub const PROVIDER_ID: &str = "chatgpt";
/// The built-in provider's display name, found under either product name.
pub const DISPLAY_NAME: &str = "Codex / ChatGPT";
/// The name OpenAI shows on the consent screen and in connected apps.
pub const AGENT_NAME: &str = "Cookie Agent";
/// The path every redirect URI must use.
pub const REDIRECT_PATH: &str = "/auth/callback";
/// The scope that grants plan usage.
pub const TOKEN_SHARING_SCOPE: &str = "chatgpt.tokens.use.direct";

const REGISTRATION_CLIENT_ID: &str = "dynamic_agent_client";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// How long a begun sign-in waits for its callback.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Refresh this long before the access token expires.
const REFRESH_AHEAD: Duration = Duration::from_secs(5 * 60);
/// The longest a sign-in goes without a refresh, which keeps the rotating
/// 30-day refresh token alive on an idle daemon.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Why a sign-in, a refresh, or a model listing failed. Each code reaches
/// clients unchanged.
#[derive(Debug, thiserror::Error)]
pub enum ChatGptError {
    #[error("sign_in_denied: the ChatGPT sign-in was declined")]
    Denied,
    #[error("sign_in_not_eligible: this ChatGPT account cannot share its plan with other apps")]
    NotEligible,
    #[error("sign_in_scope_missing: the sign-in did not grant plan usage")]
    ScopeMissing,
    #[error("sign_in_state_mismatch: the callback does not belong to this sign-in")]
    StateMismatch,
    #[error("sign_in_expired: the sign-in waited more than 10 minutes; start it again")]
    Expired,
    #[error("sign_in_not_pending: no ChatGPT sign-in is in progress")]
    NotPending,
    #[error(
        "sign_in_client_rejected: OpenAI refused cookie's saved client registration; sign in again to register a new one"
    )]
    ClientRejected,
    #[error("invalid_redirect_uri: the redirect must be http://127.0.0.1:<port>/auth/callback")]
    InvalidRedirectUri,
    #[error("invalid_callback: {0}")]
    InvalidCallback(&'static str),
    #[error("sign_in_required: sign in with ChatGPT again")]
    SignInRequired,
    #[error("sign_in_failed: {0}")]
    Failed(String),
    #[error("chatgpt_storage: {0}")]
    Storage(#[from] crate::secure_store::SecureStoreError),
}

impl ChatGptError {
    /// The stable code before the message.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Denied => "sign_in_denied",
            Self::NotEligible => "sign_in_not_eligible",
            Self::ScopeMissing => "sign_in_scope_missing",
            Self::StateMismatch => "sign_in_state_mismatch",
            Self::Expired => "sign_in_expired",
            Self::NotPending => "sign_in_not_pending",
            Self::ClientRejected => "sign_in_client_rejected",
            Self::InvalidRedirectUri => "invalid_redirect_uri",
            Self::InvalidCallback(_) => "invalid_callback",
            Self::SignInRequired => "sign_in_required",
            Self::Failed(_) => "sign_in_failed",
            Self::Storage(_) => "chatgpt_storage",
        }
    }
}

/// Whether `uri` is the only redirect token sharing accepts:
/// `http://127.0.0.1:<port>/auth/callback`, with nothing else.
#[must_use]
pub fn valid_redirect_uri(uri: &str) -> bool {
    let Some(rest) = uri.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    let Some(port) = rest.strip_suffix(REDIRECT_PATH) else {
        return false;
    };
    !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && !port.starts_with('0')
        && port.parse::<u16>().is_ok()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn random_token(bytes: usize) -> Result<String, ChatGptError> {
    let mut buffer = vec![0_u8; bytes];
    getrandom::getrandom(&mut buffer)
        .map_err(|_| ChatGptError::Failed("no system randomness".into()))?;
    Ok(base64url::encode(&buffer))
}

#[cfg(test)]
mod tests;
