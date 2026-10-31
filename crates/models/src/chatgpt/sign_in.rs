//! The authorization-code flow with PKCE, the code exchange, and sign-out.
//!
//! The ID token arrives straight from the token endpoint over TLS, so its
//! issuer is authenticated by the connection (OpenID Connect Core 1.0,
//! section 3.1.3.7) and its signature is not checked separately; its issuer,
//! audience, expiry, and nonce are.

use std::time::Instant;

use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::{
    AGENT_NAME, ChatGptCredentials, ChatGptError, REGISTRATION_CLIENT_ID, SCOPE, SIGN_IN_TIMEOUT,
    TOKEN_SHARING_SCOPE, base64url,
    credentials::{Account, Host, TokenResponse, error_code},
    now_unix, random_token, valid_redirect_uri,
};

pub(super) struct PendingSignIn {
    verifier: String,
    state: String,
    nonce: String,
    redirect_uri: String,
    authorization_url: String,
    started: Instant,
}

/// A completed sign-in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignedIn {
    pub subject: String,
    pub email: Option<String>,
}

impl ChatGptCredentials {
    /// Starts a sign-in whose browser returns to `redirect_uri` and returns
    /// the authorization URL. The same redirect returns the same URL while
    /// the sign-in waits; another redirect replaces it.
    pub fn begin_sign_in(&self, redirect_uri: &str) -> Result<String, ChatGptError> {
        if !valid_redirect_uri(redirect_uri) {
            return Err(ChatGptError::InvalidRedirectUri);
        }
        if let Some(pending) = &self.lock().pending
            && pending.redirect_uri == redirect_uri
            && pending.started.elapsed() < SIGN_IN_TIMEOUT
        {
            return Ok(pending.authorization_url.clone());
        }
        let host = self.host()?;
        let verifier = random_token(32)?;
        let state = random_token(24)?;
        let nonce = random_token(24)?;
        let challenge = base64url::encode(&Sha256::digest(verifier.as_bytes()));
        let login_hint = self
            .lock()
            .file
            .account
            .as_ref()
            .and_then(|account| account.email.clone());
        let mut url = url::Url::parse(&self.endpoints.authorize)
            .map_err(|_| ChatGptError::Failed("the authorize endpoint is not a URL".into()))?;
        {
            let mut query = url.query_pairs_mut();
            match &host.client_id {
                Some(client_id) => query.append_pair("client_id", client_id),
                None => query
                    .append_pair("client_id", REGISTRATION_CLIENT_ID)
                    .append_pair("agent_name_hint", AGENT_NAME),
            };
            query
                .append_pair("ext_agent_host_id", &host.ext_agent_host_id)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("scope", SCOPE)
                .append_pair("resource", &self.endpoints.api)
                .append_pair("state", &state)
                .append_pair("nonce", &nonce)
                .append_pair("code_challenge_method", "S256")
                .append_pair("code_challenge", &challenge);
            if let Some(hint) = &login_hint {
                query.append_pair("login_hint", hint);
            }
        }
        let authorization_url = url.to_string();
        self.lock().pending = Some(PendingSignIn {
            verifier,
            state,
            nonce,
            redirect_uri: redirect_uri.to_owned(),
            authorization_url: authorization_url.clone(),
            started: Instant::now(),
        });
        Ok(authorization_url)
    }

    /// Drops a sign-in in progress.
    pub fn cancel_sign_in(&self) {
        self.lock().pending = None;
    }

    /// Whether `callback_url` is the redirect of the sign-in in progress,
    /// with its `state`. Anything else, such as a forged or stale request to
    /// the daemon's listener, leaves the sign-in waiting.
    #[must_use]
    pub fn callback_belongs_to_sign_in(&self, callback_url: &str) -> bool {
        let Ok(callback) = url::Url::parse(callback_url.trim()) else {
            return false;
        };
        let query = callback
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect::<std::collections::BTreeMap<_, _>>();
        self.lock()
            .pending
            .as_ref()
            .is_some_and(|pending| belongs_to(pending, &callback, &query).is_ok())
    }

    /// Finishes the sign-in with the address the browser landed on.
    pub async fn complete_sign_in(&self, callback_url: &str) -> Result<SignedIn, ChatGptError> {
        let callback = url::Url::parse(callback_url.trim())
            .map_err(|_| ChatGptError::InvalidCallback("the address is not a URL"))?;
        let query = callback
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let pending = {
            let mut state = self.lock();
            let pending = state.pending.as_ref().ok_or(ChatGptError::NotPending)?;
            if pending.started.elapsed() >= SIGN_IN_TIMEOUT {
                state.pending = None;
                return Err(ChatGptError::Expired);
            }
            belongs_to(pending, &callback, &query)?;
            state.pending.take().expect("checked above")
        };
        if let Some(error) = query.get("error") {
            return Err(if error == "access_denied" {
                ChatGptError::Denied
            } else {
                ChatGptError::Failed(format!("the authorization answered `{error}`"))
            });
        }
        let code = query
            .get("code")
            .ok_or(ChatGptError::InvalidCallback("the address carries no code"))?;
        let saved_client = self.host()?.client_id;
        // The callback carries the client OpenAI issued on a first sign-in.
        let client_id = query.get("client_id").cloned().or(saved_client).ok_or(
            ChatGptError::InvalidCallback("the address carries no client_id"),
        )?;
        let response = self
            .post_form(
                &self.endpoints.token,
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", &client_id),
                    ("code", code),
                    ("code_verifier", &pending.verifier),
                    ("redirect_uri", &pending.redirect_uri),
                    ("resource", &self.endpoints.api),
                ],
            )
            .await?;
        if !response.success {
            return Err(match error_code(&response.body).as_deref() {
                Some("invalid_client") => {
                    self.persist(|file| {
                        if let Some(host) = &mut file.host {
                            host.client_id = None;
                        }
                    })?;
                    ChatGptError::ClientRejected
                }
                Some("subscription_sharing_user_not_eligible") => ChatGptError::NotEligible,
                Some("access_denied") => ChatGptError::Denied,
                code => ChatGptError::Failed(format!(
                    "the code exchange answered HTTP {}{}",
                    response.status,
                    code.map(|code| format!(" `{code}`")).unwrap_or_default()
                )),
            });
        }
        let tokens: TokenResponse = serde_json::from_slice(&response.body)
            .map_err(|_| ChatGptError::Failed("the code exchange answer is not valid".into()))?;
        let scopes = tokens
            .scope
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if !scopes.iter().any(|scope| scope == TOKEN_SHARING_SCOPE) {
            return Err(ChatGptError::ScopeMissing);
        }
        let refresh_token = tokens
            .refresh_token
            .clone()
            .ok_or_else(|| ChatGptError::Failed("the sign-in issued no refresh token".into()))?;
        let claims = tokens
            .id_token
            .as_deref()
            .and_then(id_token_claims)
            .ok_or_else(|| ChatGptError::Failed("the sign-in issued no valid ID token".into()))?;
        let now = now_unix();
        let audience_matches = match claims.get("aud") {
            Some(Value::String(audience)) => audience == &client_id,
            Some(Value::Array(audiences)) => audiences.iter().any(|value| value == &client_id),
            _ => false,
        };
        if claims.get("iss").and_then(Value::as_str) != Some(self.endpoints.issuer.as_str())
            || !audience_matches
            || claims.get("nonce").and_then(Value::as_str) != Some(pending.nonce.as_str())
            || claims
                .get("exp")
                .and_then(Value::as_u64)
                .is_none_or(|expiry| expiry <= now)
        {
            return Err(ChatGptError::Failed(
                "the ID token does not belong to this sign-in".into(),
            ));
        }
        let subject = claims
            .get("sub")
            .and_then(Value::as_str)
            .filter(|subject| !subject.is_empty())
            .ok_or_else(|| ChatGptError::Failed("the ID token names no account".into()))?
            .to_owned();
        let email = claims
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let account = Account {
            issuer: self.endpoints.issuer.clone(),
            subject: subject.clone(),
            email: email.clone(),
            access_token: tokens.access_token,
            expires_at: now + tokens.expires_in.unwrap_or(3600),
            earliest_refresh_at: tokens.earliest_refresh_at,
            refresh_token,
            scopes,
            saved_at: now,
            needs_sign_in: false,
        };
        self.persist(|file| {
            if let Some(host) = &mut file.host {
                host.client_id = Some(client_id);
            }
            file.account = Some(account);
        })?;
        self.lock().rejected = None;
        self.notify();
        Ok(SignedIn { subject, email })
    }

    /// Revokes the refresh token and forgets the account. The host's client
    /// registration is kept for the next sign-in. A revocation OpenAI refuses
    /// or cannot be reached for still signs out.
    pub async fn sign_out(&self) -> Result<(), ChatGptError> {
        let (token, client_id) = {
            let state = self.lock();
            (
                state
                    .file
                    .account
                    .as_ref()
                    .map(|account| account.refresh_token.clone()),
                state
                    .file
                    .host
                    .as_ref()
                    .and_then(|host| host.client_id.clone()),
            )
        };
        if let (Some(token), Some(client_id)) = (token, client_id) {
            let _ = self
                .post_form(
                    &self.endpoints.revoke,
                    &[
                        ("token", &token),
                        ("token_type_hint", "refresh_token"),
                        ("client_id", &client_id),
                    ],
                )
                .await;
        }
        self.persist(|file| file.account = None)?;
        self.lock().rejected = None;
        self.notify();
        Ok(())
    }

    /// The host record, made and saved on first use.
    fn host(&self) -> Result<Host, ChatGptError> {
        if let Some(host) = self.lock().file.host.clone() {
            return Ok(host);
        }
        let mut bytes = [0_u8; 16];
        getrandom::getrandom(&mut bytes)
            .map_err(|_| ChatGptError::Failed("no system randomness".into()))?;
        let host = Host {
            ext_agent_host_id: format!(
                "urn:uuid:{}",
                uuid::Builder::from_random_bytes(bytes).into_uuid()
            ),
            client_id: None,
        };
        let created = host.clone();
        self.persist(|file| {
            if file.host.is_none() {
                file.host = Some(created);
            }
        })?;
        self.lock()
            .file
            .host
            .clone()
            .ok_or_else(|| ChatGptError::Failed("could not save the host record".into()))
    }
}

fn id_token_claims(token: &str) -> Option<serde_json::Map<String, Value>> {
    let payload = token.split('.').nth(1)?;
    match serde_json::from_slice(&base64url::decode(payload)?).ok()? {
        Value::Object(claims) => Some(claims),
        _ => None,
    }
}

/// Checks that `callback` is `pending`'s redirect and carries its `state`.
fn belongs_to(
    pending: &PendingSignIn,
    callback: &url::Url,
    query: &std::collections::BTreeMap<String, String>,
) -> Result<(), ChatGptError> {
    let expected =
        url::Url::parse(&pending.redirect_uri).map_err(|_| ChatGptError::InvalidRedirectUri)?;
    if callback.scheme() != expected.scheme()
        || callback.host_str() != expected.host_str()
        || callback.port() != expected.port()
        || callback.path() != expected.path()
    {
        return Err(ChatGptError::InvalidCallback(
            "the address is not this sign-in's redirect",
        ));
    }
    match query.get("state") {
        Some(state) if same_secret(state, &pending.state) => Ok(()),
        _ => Err(ChatGptError::StateMismatch),
    }
}

/// Compares two secrets in time that depends only on their lengths.
fn same_secret(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && std::hint::black_box(
            left.bytes()
                .zip(right.bytes())
                .fold(0_u8, |difference, (a, b)| difference | (a ^ b)),
        ) == 0
}
