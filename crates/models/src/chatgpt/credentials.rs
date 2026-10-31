//! The stored sign-in, its refresh, and the token source Oven awaits.

use std::{
    fmt,
    future::Future,
    sync::{Arc, Mutex, MutexGuard, Weak},
};

use oven_sdk::{BoxFuture, ModelError, ModelErrorKind, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ChatGptError, REFRESH_AHEAD, now_unix};
use crate::secure_store::SecureDirectory;

const FILE: &str = "chatgpt-sign-in.json";
const LOCK: &str = "chatgpt-sign-in.lock";
const FILE_LIMIT: u64 = 256 * 1024;
const FILE_VERSION: u32 = 1;
/// Token-endpoint and listing bodies are small; anything larger is refused.
const RESPONSE_LIMIT: usize = 1024 * 1024;

/// OpenAI's endpoints for token sharing; tests point them at a local server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatGptEndpoints {
    /// The ID token's issuer.
    pub issuer: String,
    pub authorize: String,
    pub token: String,
    pub revoke: String,
    /// The Responses API base, which also lists the plan's models.
    pub api: String,
}

impl Default for ChatGptEndpoints {
    fn default() -> Self {
        Self {
            issuer: "https://auth.openai.com".into(),
            authorize: "https://auth.openai.com/api/accounts/authorize".into(),
            token: "https://auth.openai.com/api/accounts/oauth/token".into(),
            revoke: "https://auth.openai.com/api/accounts/oauth/revoke".into(),
            api: "https://api.openai.com/v1".into(),
        }
    }
}

/// `chatgpt-sign-in.json`. The host survives sign-outs: OpenAI binds the
/// issued client ID to the user and `ext_agent_host_id` to this daemon.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredFile {
    pub(super) version: u32,
    pub(super) host: Option<Host>,
    pub(super) account: Option<Account>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Host {
    pub(super) ext_agent_host_id: String,
    pub(super) client_id: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Account {
    pub(super) issuer: String,
    pub(super) subject: String,
    pub(super) email: Option<String>,
    pub(super) access_token: String,
    /// Unix seconds.
    pub(super) expires_at: u64,
    /// Unix seconds; OpenAI's advice, honored except for a refused token.
    pub(super) earliest_refresh_at: Option<u64>,
    pub(super) refresh_token: String,
    pub(super) scopes: Vec<String>,
    /// Unix seconds of the sign-in or last refresh.
    pub(super) saved_at: u64,
    /// Set when the refresh token was refused for good.
    pub(super) needs_sign_in: bool,
}

impl fmt::Debug for Account {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Account")
            .field("subject", &self.subject)
            .field("expires_at", &self.expires_at)
            .field("needs_sign_in", &self.needs_sign_in)
            .finish_non_exhaustive()
    }
}

/// The signed-in account, without secrets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountSummary {
    pub subject: String,
    pub email: Option<String>,
    /// The sign-in can no longer refresh; the user must sign in again.
    pub needs_sign_in: bool,
}

pub(super) struct State {
    pub(super) file: StoredFile,
    /// An access token the API refused, so it is not handed out again.
    pub(super) rejected: Option<String>,
    pub(super) pending: Option<super::sign_in::PendingSignIn>,
}

/// One daemon's Sign in with ChatGPT: the stored account, its refresh, and the
/// sign-in flow. It is the token source of every ChatGPT model.
pub struct ChatGptCredentials {
    pub(super) directory: SecureDirectory,
    pub(super) endpoints: ChatGptEndpoints,
    pub(super) http: reqwest::Client,
    pub(super) state: Mutex<State>,
    refresh: tokio::sync::Mutex<()>,
    changes: tokio::sync::watch::Sender<u64>,
    /// Refreshes run as their own tasks through this handle, so a caller
    /// that gives up cannot drop a rotated refresh token before it is saved.
    me: Weak<Self>,
}

impl fmt::Debug for ChatGptCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatGptCredentials")
            .field("directory", &self.directory.path())
            .field("account", &self.account())
            .finish_non_exhaustive()
    }
}

impl ChatGptCredentials {
    /// Opens the sign-in kept in `directory`, the provider store's directory.
    pub fn open(
        directory: SecureDirectory,
        endpoints: ChatGptEndpoints,
        http: reqwest::Client,
    ) -> Result<Arc<Self>, ChatGptError> {
        let file = read_file(&directory)?;
        Ok(Arc::new_cyclic(|me| Self {
            directory,
            endpoints,
            http,
            state: Mutex::new(State {
                file,
                rejected: None,
                pending: None,
            }),
            refresh: tokio::sync::Mutex::new(()),
            changes: tokio::sync::watch::channel(0).0,
            me: Weak::clone(me),
        }))
    }

    /// Opens the sign-in kept beside `store`, against OpenAI's endpoints.
    pub fn open_beside(
        store: &crate::provider_store::ProviderStore,
    ) -> Result<Arc<Self>, ChatGptError> {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|_| ChatGptError::Failed("could not build an HTTP client".into()))?;
        Self::open(
            SecureDirectory::open(store.directory_path())?,
            ChatGptEndpoints::default(),
            http,
        )
    }

    /// The signed-in account, if any.
    #[must_use]
    pub fn account(&self) -> Option<AccountSummary> {
        self.lock()
            .file
            .account
            .as_ref()
            .map(|account| AccountSummary {
                subject: account.subject.clone(),
                email: account.email.clone(),
                needs_sign_in: account.needs_sign_in,
            })
    }

    /// Changes whenever the account is signed in, signed out, or needs a new
    /// sign-in.
    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// The token source of the signed-in account's models.
    #[must_use]
    pub fn token_source(self: &Arc<Self>) -> crate::DynTokenSource {
        Arc::clone(self) as crate::DynTokenSource
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn notify(&self) {
        self.changes.send_modify(|generation| *generation += 1);
    }

    /// Rewrites the file under its lock from what is on disk now, and caches
    /// the result.
    pub(super) fn persist(&self, change: impl FnOnce(&mut StoredFile)) -> Result<(), ChatGptError> {
        let lock = self.directory.lock(LOCK)?;
        let mut file = match lock.read(FILE, FILE_LIMIT)? {
            Some(bytes) => parse_file(&bytes)?,
            None => StoredFile::default(),
        };
        change(&mut file);
        file.version = FILE_VERSION;
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|_| ChatGptError::Failed("could not encode the sign-in".into()))?;
        lock.atomic_replace(FILE, &bytes)?;
        self.lock().file = file;
        Ok(())
    }

    /// A Bearer token for the signed-in account, refreshed when it is about to
    /// expire or was refused.
    pub async fn access_token(&self) -> Result<SecretString, ChatGptError> {
        if let Some(token) = self.usable_token() {
            return Ok(token);
        }
        self.detached(|this| async move {
            let _refreshing = this.refresh.lock().await;
            if let Some(token) = this.usable_token() {
                return Ok(token);
            }
            this.refresh_locked(true).await
        })
        .await
    }

    /// Refreshes a sign-in that has not refreshed for `max_age`, so the
    /// rotating refresh token stays valid while the daemon is idle.
    pub async fn keep_alive(&self, max_age: std::time::Duration) -> Result<(), ChatGptError> {
        let due = self.lock().file.account.as_ref().is_some_and(|account| {
            !account.needs_sign_in && account.saved_at + max_age.as_secs() <= now_unix()
        });
        if due {
            self.detached(|this| async move {
                let _refreshing = this.refresh.lock().await;
                this.refresh_locked(false).await
            })
            .await?;
        }
        Ok(())
    }

    /// Runs `work` as its own task and waits for it. OpenAI rotates the
    /// refresh token when it answers, so a refresh must reach `persist` even
    /// when the caller's future is dropped (a timeout, a cancelled run).
    async fn detached<T, Work>(
        &self,
        work: impl FnOnce(Arc<Self>) -> Work,
    ) -> Result<T, ChatGptError>
    where
        T: Send + 'static,
        Work: Future<Output = Result<T, ChatGptError>> + Send + 'static,
    {
        let this = self
            .me
            .upgrade()
            .ok_or_else(|| ChatGptError::Failed("the sign-in was closed".into()))?;
        tokio::spawn(work(this))
            .await
            .map_err(|_| ChatGptError::Failed("the token refresh stopped".into()))?
    }

    fn usable_token(&self) -> Option<SecretString> {
        let state = self.lock();
        let account = state.file.account.as_ref()?;
        let refused = state.rejected.as_deref() == Some(account.access_token.as_str());
        (!account.needs_sign_in
            && !refused
            && account.expires_at > now_unix() + REFRESH_AHEAD.as_secs())
        .then(|| SecretString::new(account.access_token.clone()))
    }

    /// Refreshes under the refresh lock the caller holds. With
    /// `fallback_to_current`, a transient failure still hands out the current
    /// token while it is valid.
    async fn refresh_locked(
        &self,
        fallback_to_current: bool,
    ) -> Result<SecretString, ChatGptError> {
        // Another process may have refreshed; the file is the truth.
        let on_disk = read_file(&self.directory)?;
        self.lock().file = on_disk;
        let (refresh_token, client_id, current) = {
            let state = self.lock();
            let account = state
                .file
                .account
                .as_ref()
                .filter(|account| !account.needs_sign_in)
                .ok_or(ChatGptError::SignInRequired)?;
            let client_id = state
                .file
                .host
                .as_ref()
                .and_then(|host| host.client_id.clone())
                .ok_or(ChatGptError::SignInRequired)?;
            let refused = state.rejected.as_deref() == Some(account.access_token.as_str());
            let current = (!refused && account.expires_at > now_unix())
                .then(|| SecretString::new(account.access_token.clone()));
            (account.refresh_token.clone(), client_id, current)
        };
        let fallback = |error: ChatGptError| match (&current, fallback_to_current) {
            (Some(token), true) => Ok(token.clone()),
            _ => Err(error),
        };
        let response = match self
            .post_form(
                &self.endpoints.token,
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", &client_id),
                    ("refresh_token", &refresh_token),
                    ("resource", &self.endpoints.api),
                ],
            )
            .await
        {
            Ok(response) => response,
            Err(error) => return fallback(error),
        };
        if !response.success {
            let code = error_code(&response.body);
            if response.status == 401 || permanent_refresh_failure(code.as_deref()) {
                let invalid_client = code.as_deref() == Some("invalid_client");
                // The refresh lock is this process's; another process may
                // have rotated the token while this request was refused.
                // Only the refresh token that failed is flagged.
                let mut still_current = false;
                self.persist(|file| {
                    still_current = file
                        .account
                        .as_ref()
                        .is_some_and(|account| account.refresh_token == refresh_token);
                    if !still_current {
                        return;
                    }
                    if let Some(account) = &mut file.account {
                        account.needs_sign_in = true;
                    }
                    if invalid_client && let Some(host) = &mut file.host {
                        host.client_id = None;
                    }
                })?;
                if !still_current {
                    if self.lock().file.account.is_none() {
                        return Err(ChatGptError::SignInRequired);
                    }
                    return match self.usable_token() {
                        Some(token) => Ok(token),
                        None => fallback(ChatGptError::Failed(
                            "another process refreshed the sign-in; try again".into(),
                        )),
                    };
                }
                self.notify();
                return Err(ChatGptError::SignInRequired);
            }
            return fallback(ChatGptError::Failed(format!(
                "token refresh answered HTTP {}",
                response.status
            )));
        }
        let tokens: TokenResponse = serde_json::from_slice(&response.body)
            .map_err(|_| ChatGptError::Failed("the token refresh answer is not valid".into()))?;
        let now = now_unix();
        self.persist(|file| {
            if let Some(account) = &mut file.account {
                account.access_token = tokens.access_token.clone();
                if let Some(refresh) = &tokens.refresh_token {
                    account.refresh_token = refresh.clone();
                }
                account.expires_at = now + tokens.expires_in.unwrap_or(3600);
                account.earliest_refresh_at = tokens.earliest_refresh_at;
                if let Some(scope) = &tokens.scope {
                    account.scopes = scope.split_whitespace().map(str::to_owned).collect();
                }
                account.saved_at = now;
                account.needs_sign_in = false;
            }
        })?;
        self.lock().rejected = None;
        Ok(SecretString::new(tokens.access_token))
    }

    pub(super) async fn post_form(
        &self,
        url: &str,
        fields: &[(&str, &str)],
    ) -> Result<HttpAnswer, ChatGptError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        let response = self
            .http
            .post(url)
            .header("user-agent", user_agent())
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|_| ChatGptError::Failed("could not reach auth.openai.com".into()))?;
        HttpAnswer::read(response).await
    }
}

impl oven_sdk_openai::OpenAiTokenSource for ChatGptCredentials {
    fn token(&self) -> BoxFuture<'_, Result<SecretString, ModelError>> {
        Box::pin(async move {
            self.access_token().await.map_err(|error| match error {
                ChatGptError::SignInRequired => ModelError::new(
                    ModelErrorKind::Auth,
                    "the ChatGPT sign-in can no longer refresh; sign in again",
                )
                .with_vendor_code("provider_sign_in_required"),
                other => {
                    ModelError::transport(format!("could not refresh the ChatGPT sign-in: {other}"))
                        .with_retryable(true)
                }
            })
        })
    }

    fn rejected(&self, token: &SecretString) {
        let mut state = self.lock();
        if state
            .file
            .account
            .as_ref()
            .is_some_and(|account| account.access_token == token.expose_secret())
        {
            state.rejected = Some(token.expose_secret().to_owned());
        }
    }
}

pub(super) struct HttpAnswer {
    pub(super) status: u16,
    pub(super) success: bool,
    pub(super) body: Vec<u8>,
}

impl HttpAnswer {
    pub(super) async fn read(response: reqwest::Response) -> Result<Self, ChatGptError> {
        let status = response.status();
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        use futures_util::StreamExt as _;
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| ChatGptError::Failed("the OpenAI answer was cut off".into()))?;
            if body.len() + chunk.len() > RESPONSE_LIMIT {
                return Err(ChatGptError::Failed(
                    "the OpenAI answer is too large".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Self {
            status: status.as_u16(),
            success: status.is_success(),
            body,
        })
    }
}

#[derive(Deserialize)]
pub(super) struct TokenResponse {
    pub(super) access_token: String,
    #[serde(default)]
    pub(super) refresh_token: Option<String>,
    #[serde(default)]
    pub(super) id_token: Option<String>,
    #[serde(default)]
    pub(super) expires_in: Option<u64>,
    #[serde(default)]
    pub(super) earliest_refresh_at: Option<u64>,
    #[serde(default)]
    pub(super) scope: Option<String>,
}

/// The error code of an OAuth (`{"error": "code"}`) or API
/// (`{"error": {"code": ...}}`) error body.
pub(super) fn error_code(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/error/code").and_then(Value::as_str))
        .or_else(|| value.get("code").and_then(Value::as_str))
        .map(str::to_owned)
}

fn permanent_refresh_failure(code: Option<&str>) -> bool {
    matches!(
        code,
        Some(
            "invalid_grant"
                | "invalid_refresh_token"
                | "token_expired"
                | "refresh_token_expired"
                | "refresh_token_invalidated"
                | "refresh_token_reused"
                | "invalid_client"
        )
    )
}

pub(super) fn user_agent() -> String {
    concat!("cookie-agent/", env!("CARGO_PKG_VERSION")).to_owned()
}

fn read_file(directory: &SecureDirectory) -> Result<StoredFile, ChatGptError> {
    match directory.read(FILE, FILE_LIMIT)? {
        Some(bytes) => parse_file(&bytes),
        None => Ok(StoredFile::default()),
    }
}

fn parse_file(bytes: &[u8]) -> Result<StoredFile, ChatGptError> {
    let file: StoredFile = serde_json::from_slice(bytes)
        .map_err(|_| ChatGptError::Failed(format!("{FILE} is not a valid sign-in record")))?;
    if file.version != FILE_VERSION {
        return Err(ChatGptError::Failed(format!(
            "{FILE} has version {}, which this build does not read",
            file.version
        )));
    }
    Ok(file)
}
