//! OAuth for Streamable HTTP MCP servers: the daemon's credential store
//! (`mcp-oauth.json`), the reactive authorization flow, and token refresh.
//!
//! Only the daemon's global runtime uses it. A registry opened without a
//! credential store never takes these paths.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

#[cfg(unix)]
use std::{
    fs::{self, OpenOptions},
    io::{Read as _, Write as _},
};

use async_trait::async_trait;
use cookie_agent_config::{LoadedMcpServer, McpOAuthSettings, McpServerConfig};
#[cfg(windows)]
use cookie_agent_models::secure_store::{DEFAULT_LOCK_BUDGET, SecureDirectory};
#[cfg(unix)]
use cookie_agent_models::secure_store::{DEFAULT_LOCK_BUDGET, lock_within_file, unlock};
use cookie_agent_runtime_api::ToolError;
use futures_util::StreamExt as _;
use rmcp::transport::{
    AuthClient, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
    OAuthHttpClient, OAuthHttpClientFuture, OAuthHttpRedirectPolicy, StoredCredentials,
    StreamableHttpClientTransport,
    auth::{AuthorizationCallback, OAuthClientConfig, OAuthTokenResponse},
    streamable_http_client::StreamableHttpClientTransportConfig,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use super::{McpRegistry, McpServerState, ServerRuntime};

#[cfg(not(test))]
pub(crate) const OAUTH_CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
// Long enough that a loaded CI runner finishes the discovery, registration,
// and authorization round trips of a test flow before it expires.
#[cfg(test)]
pub(crate) const OAUTH_CALLBACK_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(any(unix, test))]
// Only unix stores credentials in this file; the tests that read it are
// unix-only too.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) const OAUTH_STORE_FILE: &str = "mcp-oauth.json";
const OAUTH_STORE_LOCK_FILE: &str = "mcp-oauth.lock";
const OAUTH_STORE_MAX_BYTES: u64 = 1024 * 1024;
const REDACTED_AUTHORIZATION_CODE: &str = "cookie-agent-redacted-authorization-code";

/// The OAuth state of one server.
#[derive(Default)]
pub(crate) struct ServerAuth {
    /// The `WWW-Authenticate` challenge of the last authorization failure.
    challenge: Mutex<Option<String>>,
    response_error: Arc<Mutex<Option<String>>>,
    flow: Mutex<Option<OAuthFlowState>>,
    generation: AtomicU64,
    /// Serializes credential changes and token exchanges.
    lock: AsyncMutex<()>,
}

struct OAuthFlowState {
    generation: u64,
    authorization_url: String,
    /// The client's loopback callback the authorization URL redirects to.
    redirect_uri: url::Url,
    cancellation: CancellationToken,
    /// Taken by the one completion allowed to exchange its code.
    exchange: Option<(AuthorizationSession, OAuthExchangeState)>,
}

#[derive(Clone)]
pub(crate) struct OAuthCredentialFile {
    inner: Arc<OAuthCredentialFileInner>,
}

struct OAuthCredentialFileInner {
    path: PathBuf,
    transaction: Mutex<()>,
}

#[cfg(unix)]
struct OAuthStoreLock {
    file: fs::File,
}

#[derive(Clone)]
struct ServerCredentialStore {
    file: OAuthCredentialFile,
    key: String,
    binding: OAuthCredentialBinding,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOAuthCredential {
    binding: OAuthCredentialBinding,
    credentials: StrictStoredCredentials,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct OAuthCredentialBinding {
    resource_url: String,
    configured_client_id: Option<String>,
    client_metadata_url: Option<String>,
    client_secret_sha256: Option<String>,
    scopes: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StrictStoredCredentials {
    client_id: String,
    token_response: Option<OAuthTokenResponse>,
    granted_scopes: Vec<String>,
    token_received_at: Option<u64>,
    issuer: Option<String>,
}

struct McpOAuthHttpClient {
    follow_redirects: reqwest::Client,
    stop_redirects: reqwest::Client,
    exchange: OAuthExchangeState,
    store: ServerCredentialStore,
}

#[derive(Clone, Default)]
struct OAuthExchangeState {
    code: Arc<Mutex<Option<zeroize::Zeroizing<String>>>>,
    response_error: Arc<Mutex<Option<String>>>,
}

#[derive(Debug)]
struct OAuthHttpFailure;

impl std::fmt::Display for OAuthHttpFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OAuth HTTP operation failed")
    }
}

impl std::error::Error for OAuthHttpFailure {}

pub(crate) fn oauth_enabled_config(config: &McpServerConfig) -> bool {
    config.url.is_some()
        && config.oauth.enabled()
        && !config
            .headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("authorization"))
}

pub(crate) fn auth_challenge(error: &(dyn std::error::Error + 'static)) -> Option<String> {
    use rmcp::transport::streamable_http_client::{AuthRequiredError, InsufficientScopeError};

    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(required) = current.downcast_ref::<AuthRequiredError>() {
            return Some(required.www_authenticate_header.clone());
        }
        if let Some(scope) = current.downcast_ref::<InsufficientScopeError>() {
            return Some(scope.www_authenticate_header.clone());
        }
        source = current.source();
    }
    None
}

fn oauth_requires_auth(error: &(dyn std::error::Error + 'static)) -> bool {
    if auth_challenge(error).is_some() {
        return true;
    }
    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(auth) = current.downcast_ref::<rmcp::transport::AuthError>()
            && matches!(
                auth,
                rmcp::transport::AuthError::AuthorizationRequired
                    | rmcp::transport::AuthError::TokenExpired
                    | rmcp::transport::AuthError::TokenRefreshFailed(_)
                    | rmcp::transport::AuthError::TokenRefreshRejected(_)
            )
        {
            return true;
        }
        source = current.source();
    }
    false
}

pub(crate) fn initialize_error_requires_auth(error: &rmcp::service::ClientInitializeError) -> bool {
    error.is_authorization_required()
        || match error {
            rmcp::service::ClientInitializeError::TransportError { error, .. } => {
                oauth_requires_auth(error.error.as_ref())
            }
            _ => false,
        }
}

pub(crate) fn service_error_requires_auth(error: &rmcp::service::ServiceError) -> bool {
    match error {
        rmcp::service::ServiceError::TransportSend(error) => {
            oauth_requires_auth(error.error.as_ref())
        }
        _ => oauth_requires_auth(error),
    }
}

impl OAuthCredentialFile {
    pub(crate) fn open(path: PathBuf) -> Result<Self, ToolError> {
        #[cfg(windows)]
        {
            load_oauth_store_windows(&path).map_err(|()| oauth_store_startup_error(&path))?;
            Ok(Self {
                inner: Arc::new(OAuthCredentialFileInner {
                    path,
                    transaction: Mutex::new(()),
                }),
            })
        }
        #[cfg(unix)]
        {
            load_oauth_store(&path).map_err(|()| oauth_store_startup_error(&path))?;
            Ok(Self {
                inner: Arc::new(OAuthCredentialFileInner {
                    path,
                    transaction: Mutex::new(()),
                }),
            })
        }
    }

    fn scoped(&self, server: &str, binding: OAuthCredentialBinding) -> ServerCredentialStore {
        ServerCredentialStore {
            file: self.clone(),
            key: oauth_credential_key(server, &binding.resource_url),
            binding,
        }
    }

    fn remove(&self, server: &str, binding: &OAuthCredentialBinding) -> Result<(), ToolError> {
        let key = oauth_credential_key(server, &binding.resource_url);
        self.update(|credentials| {
            credentials.remove(&key);
        })
        .map_err(|()| oauth_store_runtime_error(&self.inner.path))
    }

    fn update(
        &self,
        update: impl FnOnce(&mut BTreeMap<String, PersistedOAuthCredential>),
    ) -> Result<(), ()> {
        #[cfg(windows)]
        {
            let _transaction = self
                .inner
                .transaction
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let (directory, name) = oauth_store_directory_windows(&self.inner.path)?;
            let lock = directory
                .lock_within(OAUTH_STORE_LOCK_FILE, DEFAULT_LOCK_BUDGET)
                .map_err(|_| ())?;
            let mut candidate = load_oauth_store_from_lock_windows(&lock, &name)?;
            update(&mut candidate);
            let bytes = serde_json::to_vec(&candidate).map_err(|_| ())?;
            lock.atomic_replace(&name, &bytes).map_err(|_| ())
        }
        #[cfg(unix)]
        {
            let _transaction = self
                .inner
                .transaction
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _file_lock = OAuthStoreLock::acquire(&self.inner.path)?;
            let mut candidate = load_oauth_store(&self.inner.path)?;
            update(&mut candidate);
            persist_oauth_store(&self.inner.path, &candidate)?;
            Ok(())
        }
    }

    /// Lock-free read of one credential entry (D6).
    ///
    /// The store is only ever mutated via temp-file plus atomic replace, so a
    /// reader always observes a complete old-or-new document; there is no torn
    /// state to guard. On Windows the general data reader opens with
    /// `FILE_SHARE_DELETE`, so a lock-free reader cannot block the writer's
    /// replace.
    fn get(&self, key: &str) -> Result<Option<PersistedOAuthCredential>, ()> {
        #[cfg(windows)]
        {
            Ok(load_oauth_store_windows(&self.inner.path)?
                .get(key)
                .cloned())
        }
        #[cfg(unix)]
        {
            Ok(load_oauth_store(&self.inner.path)?.get(key).cloned())
        }
    }

    /// Bounded-lock read used by the refresh-claim check (D6-correctness).
    ///
    /// Unlike [`Self::get`] this takes the exclusive lock so the pre-flight
    /// compare against disk is coherent; any error (including contention) is
    /// reported to the caller, which skips the optimization and proceeds with
    /// the live request.
    fn get_for_claim(&self, key: &str) -> Result<Option<PersistedOAuthCredential>, ()> {
        #[cfg(windows)]
        {
            let _transaction = self
                .inner
                .transaction
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let (directory, name) = oauth_store_directory_windows(&self.inner.path)?;
            let lock = directory
                .lock_within(OAUTH_STORE_LOCK_FILE, DEFAULT_LOCK_BUDGET)
                .map_err(|_| ())?;
            Ok(load_oauth_store_from_lock_windows(&lock, &name)?
                .get(key)
                .cloned())
        }
        #[cfg(unix)]
        {
            let _transaction = self
                .inner
                .transaction
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _file_lock = OAuthStoreLock::acquire(&self.inner.path)?;
            Ok(load_oauth_store(&self.inner.path)?.get(key).cloned())
        }
    }
}

#[async_trait]
impl CredentialStore for ServerCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, rmcp::transport::AuthError> {
        let stored = self
            .file
            .get(&self.key)
            .map_err(|()| oauth_store_auth_error())?;
        let Some(stored) = stored else {
            return Ok(None);
        };
        if stored.binding != self.binding {
            self.clear().await?;
            return Ok(None);
        }
        Ok(Some(stored.credentials.into_rmcp()))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), rmcp::transport::AuthError> {
        let stored = PersistedOAuthCredential {
            binding: self.binding.clone(),
            credentials: StrictStoredCredentials::from_rmcp(credentials),
        };
        self.file
            .update(|all| {
                if let Some(existing) = all.get(&self.key)
                    && credential_superseded(&existing.credentials, &stored.credentials)
                {
                    // Compare-before-write CAS (D6-correctness): the on-disk
                    // entry is newer (or a same-timestamp rotation), so the
                    // caller's credentials are superseded. The DISK wins; skip
                    // the write without error.
                    return;
                }
                all.insert(self.key.clone(), stored);
            })
            .map_err(|()| oauth_store_auth_error())
    }

    async fn clear(&self) -> Result<(), rmcp::transport::AuthError> {
        self.file
            .update(|all| {
                all.remove(&self.key);
            })
            .map_err(|()| oauth_store_auth_error())
    }
}

impl ServerCredentialStore {
    /// Bounded-lock disk read for the refresh-claim check.
    ///
    /// Any error (including lock contention) yields `None`: the optimization is
    /// skipped and the live request proceeds. This is never a failure mode.
    fn claim_credential(&self) -> Option<PersistedOAuthCredential> {
        self.file.get_for_claim(&self.key).ok().flatten()
    }
}

/// Compare-before-write CAS predicate (D6-correctness).
///
/// Returns `true` when the freshly re-read on-disk entry supersedes the
/// candidate being saved, in which case the write is skipped. The disk wins
/// when it is strictly newer by `token_received_at`, or carries the same
/// timestamp with a different refresh token. Equal-refresh-token idempotent
/// writes and `token_response: None` administrative downgrades are allowed.
fn credential_superseded(
    existing: &StrictStoredCredentials,
    candidate: &StrictStoredCredentials,
) -> bool {
    if candidate.token_response.is_none() {
        return false;
    }
    let strictly_newer = existing.token_received_at > candidate.token_received_at;
    let same_timestamp = existing.token_received_at == candidate.token_received_at;
    strictly_newer || (same_timestamp && existing.refresh_token() != candidate.refresh_token())
}

fn refresh_token_of(response: &OAuthTokenResponse) -> Option<String> {
    serde_json::to_value(response)
        .ok()?
        .get("refresh_token")?
        .as_str()
        .map(str::to_owned)
}

impl OAuthCredentialBinding {
    fn from_config(config: &McpServerConfig) -> Option<Self> {
        let resource_url = canonical_oauth_resource_url(config.url.as_deref()?);
        let settings = config.oauth.settings();
        let client_secret_sha256 = settings.client_secret.as_ref().map(|secret| {
            let digest = Sha256::digest(secret.as_bytes());
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        });
        Some(Self {
            resource_url,
            configured_client_id: settings.client_id,
            client_metadata_url: settings.client_metadata_url,
            client_secret_sha256,
            scopes: settings.scopes,
        })
    }
}

fn canonical_oauth_resource_url(resource_url: &str) -> String {
    let Ok(mut url) = url::Url::parse(resource_url) else {
        // Invalid URLs stay fully distinct instead of falling back to a broader key.
        return resource_url.to_owned();
    };
    if matches!(
        (url.scheme(), url.port()),
        ("http", Some(80)) | ("https", Some(443))
    ) {
        let _ = url.set_port(None);
    }
    url.into()
}

fn oauth_credential_key(server: &str, canonical_resource_url: &str) -> String {
    let digest = Sha256::digest(canonical_resource_url.as_bytes());
    let hash = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{server}:{hash}")
}

impl StrictStoredCredentials {
    fn from_rmcp(credentials: StoredCredentials) -> Self {
        Self {
            client_id: credentials.client_id,
            token_response: credentials.token_response,
            granted_scopes: credentials.granted_scopes,
            token_received_at: credentials.token_received_at,
            issuer: credentials.issuer,
        }
    }

    fn into_rmcp(self) -> StoredCredentials {
        StoredCredentials::new(
            self.client_id,
            self.token_response,
            self.granted_scopes,
            self.token_received_at,
        )
        .with_issuer(self.issuer)
    }

    fn refresh_token(&self) -> Option<String> {
        self.token_response.as_ref().and_then(refresh_token_of)
    }
}

impl McpOAuthHttpClient {
    fn new(exchange: OAuthExchangeState, store: ServerCredentialStore) -> Result<Self, ToolError> {
        let base = || {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .pool_max_idle_per_host(0)
        };
        let follow_redirects = base()
            .build()
            .map_err(|_| ToolError::execution("MCP OAuth HTTP client setup failed"))?;
        let stop_redirects = base()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ToolError::execution("MCP OAuth HTTP client setup failed"))?;
        Ok(Self {
            follow_redirects,
            stop_redirects,
            exchange,
            store,
        })
    }

    /// Pre-flight refresh claim (D6-correctness).
    ///
    /// Compares the refresh token about to be presented to the IdP against the
    /// on-disk credential under a bounded lock. When disk has rotated ahead,
    /// another process already refreshed: synthesize a 200 adopting the disk
    /// credentials instead of making a doomed IdP call. Any read error or
    /// contention skips the check so the live request proceeds.
    fn adopted_success(&self, requested: Option<&str>) -> Option<http::Response<Vec<u8>>> {
        let requested = requested?;
        let disk = self.store.claim_credential()?;
        if disk.binding != self.store.binding {
            return None;
        }
        let token_response = disk.credentials.token_response.as_ref()?;
        let disk_refresh = refresh_token_of(token_response)?;
        if disk_refresh == requested {
            return None;
        }
        let body = serde_json::to_vec(token_response).ok()?;
        http::Response::builder()
            .status(http::StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(body)
            .ok()
    }
}

/// Extracts the refresh-token grant's token from an OAuth form body.
///
/// Returns `Some(token)` only for `grant_type=refresh_token` requests; `None`
/// otherwise. This sniffing is the fragile part of the refresh claim and is
/// covered by a debug test that fails loudly if rmcp stops routing refreshes
/// through this client.
fn refresh_grant_token(body: &[u8]) -> Option<String> {
    let mut grant_type = None;
    let mut refresh_token = None;
    for (name, value) in url::form_urlencoded::parse(body) {
        match name.as_ref() {
            "grant_type" => grant_type = Some(value.into_owned()),
            "refresh_token" => refresh_token = Some(value.into_owned()),
            _ => {}
        }
    }
    if grant_type.as_deref() != Some("refresh_token") {
        return None;
    }
    refresh_token
}

/// Whether a token-endpoint response is the `invalid_grant` rejection.
fn response_is_invalid_grant(response: &http::Response<Vec<u8>>) -> bool {
    if response.status() != http::StatusCode::BAD_REQUEST {
        return false;
    }
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(response.body())
        && value.get("error").and_then(serde_json::Value::as_str) == Some("invalid_grant")
    {
        return true;
    }
    response
        .body()
        .windows(b"invalid_grant".len())
        .any(|window| window == b"invalid_grant")
}

impl OAuthExchangeState {
    fn diagnostic(&self, error: &(dyn std::error::Error + 'static)) -> String {
        self.response_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .unwrap_or_else(|| cookie_agent_protocol::diagnostics::error_chain(error))
    }

    fn install(&self, code: String) {
        *self
            .code
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(zeroize::Zeroizing::new(code));
    }

    fn restore_in_request(&self, request: &mut http::Request<Vec<u8>>) {
        let parameters = url::form_urlencoded::parse(request.body())
            .into_owned()
            .collect::<Vec<_>>();
        if !parameters
            .iter()
            .any(|(name, value)| name == "code" && value == REDACTED_AUTHORIZATION_CODE)
        {
            return;
        }
        let Some(code) = self
            .code
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        else {
            return;
        };
        let mut encoded = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in parameters {
            encoded.append_pair(
                &name,
                if name == "code" && value == REDACTED_AUTHORIZATION_CODE {
                    code.as_str()
                } else {
                    &value
                },
            );
        }
        *request.body_mut() = encoded.finish().into_bytes();
    }
}

impl OAuthHttpClient for McpOAuthHttpClient {
    fn execute(&self, operation: rmcp::transport::OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let client = match operation.redirect_policy {
                OAuthHttpRedirectPolicy::Follow => &self.follow_redirects,
                OAuthHttpRedirectPolicy::Stop => &self.stop_redirects,
                _ => &self.stop_redirects,
            };
            let timeout = operation.timeout;
            let mut oauth_request = operation.request;
            self.exchange.restore_in_request(&mut oauth_request);
            // D6-correctness pre-flight: for a refresh grant, adopt a rotation
            // that already landed on disk instead of making a doomed IdP call.
            let refresh_token = refresh_grant_token(oauth_request.body());
            if let Some(response) = self.adopted_success(refresh_token.as_deref()) {
                return Ok(response);
            }
            let mut request = reqwest::Request::try_from(oauth_request)
                .map_err(|_| Box::new(OAuthHttpFailure) as rmcp::transport::OAuthHttpClientError)?;
            *request.timeout_mut() = timeout;
            let response = client
                .execute(request)
                .await
                .map_err(|_| Box::new(OAuthHttpFailure) as rmcp::transport::OAuthHttpClientError)?;
            let status = response.status();
            let version = response.version();
            let headers = response.headers().clone();
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| {
                    Box::new(OAuthHttpFailure) as rmcp::transport::OAuthHttpClientError
                })?;
                if chunk.len() > OAUTH_STORE_MAX_BYTES as usize - body.len() {
                    return Err(Box::new(OAuthHttpFailure) as rmcp::transport::OAuthHttpClientError);
                }
                body.extend_from_slice(&chunk);
            }
            // Keep the original response for OAuth error classification. Retain a bounded
            // display copy because rmcp can discard the body on parse failures.
            *self
                .exchange
                .response_error
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = (!status.is_success()).then(|| {
                cookie_agent_protocol::diagnostics::detail(&format!(
                    "OAuth HTTP {status}\nResponse body:\n{}",
                    String::from_utf8_lossy(&body)
                ))
                .to_string()
            });
            let mut response = http::Response::builder().status(status).version(version);
            for (name, value) in &headers {
                response = response.header(name, value);
            }
            let response = response
                .body(body)
                .map_err(|_| Box::new(OAuthHttpFailure) as rmcp::transport::OAuthHttpClientError)?;
            // D6-correctness recovery: a refresh the IdP rejected with
            // `invalid_grant` may have lost a cross-process race. Re-check disk
            // once; if a sibling landed a rotation, adopt it instead of
            // surfacing re-login.
            if refresh_token.is_some()
                && response_is_invalid_grant(&response)
                && let Some(adopted) = self.adopted_success(refresh_token.as_deref())
            {
                return Ok(adopted);
            }
            Ok(response)
        })
    }
}

/// Accepts only a client loopback callback (RFC 8252 §7.3): plain HTTP to a
/// loopback IP literal with an explicit port, no credentials, query, or
/// fragment.
fn loopback_redirect_uri(value: &str) -> Result<url::Url, ToolError> {
    url::Url::parse(value)
        .ok()
        .filter(|url| {
            url.scheme() == "http"
                && match url.host() {
                    Some(url::Host::Ipv4(address)) => address.is_loopback(),
                    Some(url::Host::Ipv6(address)) => address.is_loopback(),
                    _ => false,
                }
                && url.port().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
        .ok_or_else(|| {
            ToolError::execution(
                "OAuth redirect URI must be the client's loopback callback, such as http://127.0.0.1:<port>/callback",
            )
        })
}

/// Whether `callback` is a redirect to `redirect_uri` carrying an
/// authorization response (a code or an error).
fn is_callback_for(callback: &url::Url, redirect_uri: &url::Url) -> bool {
    callback.scheme() == redirect_uri.scheme()
        && callback.host() == redirect_uri.host()
        && callback.port() == redirect_uri.port()
        && callback.path() == redirect_uri.path()
        && callback
            .query_pairs()
            .any(|(key, _)| matches!(key.as_ref(), "code" | "error"))
}

fn parse_oauth_callback(url: &str) -> Result<AuthorizationCallback, String> {
    AuthorizationCallback::from_redirect_url(url).map_err(|error| {
        let mut message = error.to_string();
        if let Ok(url) = url::Url::parse(url) {
            for (key, value) in url
                .query_pairs()
                .filter(|(key, _)| matches!(key.as_ref(), "error" | "error_description"))
            {
                message.push_str(&format!("\n{key}: {value}"));
            }
        }
        cookie_agent_protocol::diagnostics::detail(&message).to_string()
    })
}

fn oauth_store_startup_error(path: &Path) -> ToolError {
    ToolError::execution(format!(
        "invalid MCP OAuth credential store `{}`; fix its contents or remove the file to reset user MCP OAuth credentials",
        path.display()
    ))
}

#[cfg(windows)]
fn oauth_store_directory_windows(path: &Path) -> Result<(SecureDirectory, String), ()> {
    let parent = path.parent().ok_or(())?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(())?
        .to_owned();
    let directory = SecureDirectory::open(parent).map_err(|_| ())?;
    Ok((directory, name))
}

#[cfg(windows)]
fn load_oauth_store_windows(path: &Path) -> Result<BTreeMap<String, PersistedOAuthCredential>, ()> {
    // D6: reads are lock-free on Windows too. The general data reader opens
    // with `FILE_SHARE_DELETE`, so this cannot block a concurrent atomic
    // replace, and the replace is atomic (old-or-new bytes, never torn).
    let (directory, name) = oauth_store_directory_windows(path)?;
    let Some(bytes) = directory
        .read(&name, OAUTH_STORE_MAX_BYTES)
        .map_err(|_| ())?
    else {
        return Ok(BTreeMap::new());
    };
    serde_json::from_slice(&bytes).map_err(|_| ())
}

#[cfg(windows)]
fn load_oauth_store_from_lock_windows(
    lock: &cookie_agent_models::secure_store::SecureDirectoryLock<'_>,
    name: &str,
) -> Result<BTreeMap<String, PersistedOAuthCredential>, ()> {
    let Some(bytes) = lock.read(name, OAUTH_STORE_MAX_BYTES).map_err(|_| ())? else {
        return Ok(BTreeMap::new());
    };
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn oauth_store_runtime_error(path: &Path) -> ToolError {
    ToolError::execution(format!(
        "MCP OAuth credential storage failed at `{}`",
        path.display()
    ))
}

fn oauth_store_auth_error() -> rmcp::transport::AuthError {
    rmcp::transport::AuthError::InternalError("OAuth credential storage failed".into())
}

#[cfg(unix)]
impl OAuthStoreLock {
    fn acquire(store_path: &Path) -> Result<Self, ()> {
        let parent = store_path.parent().ok_or(())?;
        if !parent.exists() {
            use std::os::unix::fs::DirBuilderExt as _;

            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(parent).map_err(|_| ())?;
        }
        let path = parent.join(OAUTH_STORE_LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_CLOEXEC);
        }
        let file = options.open(&path).map_err(|_| ())?;
        if !lock_within_file(&file, DEFAULT_LOCK_BUDGET).map_err(|_| ())? {
            return Err(());
        }
        Ok(Self { file })
    }
}

#[cfg(unix)]
impl Drop for OAuthStoreLock {
    fn drop(&mut self) {
        let _ = unlock(&self.file);
    }
}

#[cfg(unix)]
fn load_oauth_store(path: &Path) -> Result<BTreeMap<String, PersistedOAuthCredential>, ()> {
    let Some(mut file) = open_oauth_store(path)? else {
        return Ok(BTreeMap::new());
    };
    let mut bytes = Vec::new();
    (&mut file)
        .take(OAUTH_STORE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > OAUTH_STORE_MAX_BYTES {
        return Err(());
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

#[cfg(unix)]
fn open_oauth_store(path: &Path) -> Result<Option<fs::File>, ()> {
    match OpenOptions::new().read(true).open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

#[cfg(unix)]
fn persist_oauth_store(
    path: &Path,
    credentials: &BTreeMap<String, PersistedOAuthCredential>,
) -> Result<(), ()> {
    let parent = path.parent().ok_or(())?;
    if !parent.exists() {
        use std::os::unix::fs::DirBuilderExt as _;

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(parent).map_err(|_| ())?;
    }
    let bytes = serde_json::to_vec(credentials).map_err(|_| ())?;
    let temporary = parent.join(format!(".{OAUTH_STORE_FILE}.{}.tmp", uuid::Uuid::now_v7()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&temporary).map_err(|_| ())?;
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| ())?;
    }
    let result = (|| {
        file.write_all(&bytes).map_err(|_| ())?;
        file.sync_all().map_err(|_| ())?;
        drop(file);
        fs::rename(&temporary, path).map_err(|_| ())?;
        sync_oauth_directory(parent).map_err(|_| ())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(unix)]
fn sync_oauth_directory(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

fn apply_oauth_settings(
    mut request: AuthorizationRequest,
    settings: &McpOAuthSettings,
) -> AuthorizationRequest {
    if !settings.scopes.is_empty() {
        request = request.with_scopes(settings.scopes.clone());
    }
    if let Some(client_id) = &settings.client_id {
        request = request.with_preregistered_client(client_id);
    }
    if let Some(client_secret) = &settings.client_secret {
        request = request.with_client_secret(client_secret);
    }
    if let Some(client_metadata_url) = &settings.client_metadata_url {
        request = request.with_client_metadata_url(client_metadata_url);
    }
    request
}

impl McpRegistry {
    /// Begins replacing `previous` with `loaded`: holds `previous`'s
    /// credential lock, forgets its credential when the OAuth binding
    /// changes, and supersedes it. Returns the lock and whether the
    /// replacement must authenticate before it connects.
    pub(crate) async fn replace_credentials<'a>(
        &self,
        previous: Option<&'a Arc<ServerRuntime>>,
        name: &str,
        loaded: &LoadedMcpServer,
    ) -> Result<(Option<tokio::sync::MutexGuard<'a, ()>>, bool), ToolError> {
        let Some(previous) = previous else {
            return Ok((None, false));
        };
        let previous_binding = OAuthCredentialBinding::from_config(&previous.loaded.config);
        let binding_changed =
            previous_binding != OAuthCredentialBinding::from_config(&loaded.config);
        let auth = previous.auth.lock.lock().await;
        if binding_changed
            && let Some(binding) = &previous_binding
            && let Some(credentials) = &self.inner.oauth_credentials
        {
            credentials.remove(name, binding)?;
        }
        previous.supersede();
        Ok((
            Some(auth),
            binding_changed
                && self.inner.oauth_credentials.is_some()
                && oauth_enabled_config(&loaded.config),
        ))
    }

    /// Holds `server`'s credential lock and forgets its credential, as when
    /// it is removed.
    pub(crate) async fn forget_credentials<'a>(
        &self,
        server: &'a Arc<ServerRuntime>,
    ) -> Result<tokio::sync::MutexGuard<'a, ()>, ToolError> {
        let auth = server.auth.lock.lock().await;
        if let Some(binding) = OAuthCredentialBinding::from_config(&server.loaded.config)
            && let Some(credentials) = &self.inner.oauth_credentials
        {
            credentials.remove(&server.name, &binding)?;
        }
        Ok(auth)
    }

    pub async fn begin_auth(&self, name: &str, redirect_uri: &str) -> Result<String, ToolError> {
        let server = self.server(name)?;
        server.begin_auth(redirect_uri).await
    }

    pub async fn complete_auth(&self, name: &str, callback_url: &str) -> Result<(), ToolError> {
        let server = self.server(name)?;
        server.complete_auth(callback_url).await
    }

    pub fn cancel_auth(&self, name: &str) -> Result<(), ToolError> {
        let server = self.server(name)?;
        let flow = server
            .auth
            .flow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .ok_or_else(|| {
                ToolError::execution(format!(
                    "MCP server `{name}` has no OAuth authorization in progress"
                ))
            })?;
        flow.cancellation.cancel();
        server.set_auth_in_progress(false);
        server.set_status(
            McpServerState::NeedsAuth,
            Some("OAuth authorization cancelled; authenticate to connect".into()),
        );
        Ok(())
    }
}

impl ServerRuntime {
    /// Cancels an authorization flow in progress.
    pub(crate) fn end_auth_flow(&self) {
        if let Some(flow) = self
            .auth
            .flow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            flow.cancellation.cancel();
        }
        self.set_auth_in_progress(false);
    }

    fn set_auth_in_progress(&self, active: bool) {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .auth_in_progress = active;
        self.notify_changed();
    }

    /// The Streamable HTTP transport of an OAuth server, with its stored
    /// credentials.
    pub(crate) async fn oauth_transport(
        &self,
        config: StreamableHttpClientTransportConfig,
    ) -> Result<StreamableHttpClientTransport<AuthClient<reqwest::Client>>, ToolError> {
        let (mut manager, _authorization_code) = self.authorization_manager().await?;
        let restored = manager
            .initialize_from_store()
            .await
            .map_err(|_| ToolError::execution("MCP OAuth credential loading failed"))?;
        let settings = self.loaded.config.oauth.settings();
        if restored && let Some(client_id) = settings.client_id {
            let mut client =
                OAuthClientConfig::new(client_id, "http://127.0.0.1").with_scopes(settings.scopes);
            if let Some(client_secret) = settings.client_secret {
                client = client.with_client_secret(client_secret);
            }
            manager
                .configure_client(client)
                .map_err(|_| ToolError::execution("MCP OAuth stored-client setup failed"))?;
        }
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ToolError::execution("MCP HTTP client setup failed"))?;
        Ok(StreamableHttpClientTransport::with_client(
            AuthClient::new(client, manager),
            config,
        ))
    }

    pub(crate) fn mark_needs_auth(&self, challenge: Option<String>, reason: &str) {
        if self.superseded.is_cancelled() {
            return;
        }
        *self
            .auth
            .challenge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = challenge;
        self.clear_claims();
        self.tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let response_error = self
            .auth
            .response_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let reason = response_error.as_deref().unwrap_or(reason);
        self.set_status(
            McpServerState::NeedsAuth,
            Some(
                cookie_agent_protocol::diagnostics::detail(&format!(
                    "OAuth authorization required; authenticate to connect\n{reason}"
                ))
                .to_string(),
            ),
        );
    }

    fn oauth_binding(&self) -> Result<OAuthCredentialBinding, ToolError> {
        OAuthCredentialBinding::from_config(&self.loaded.config)
            .ok_or_else(|| ToolError::execution("OAuth is available only for remote MCP servers"))
    }

    fn oauth_store(&self) -> Result<ServerCredentialStore, ToolError> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| ToolError::execution("MCP registry is unavailable"))?;
        let credentials = registry.oauth_credentials.as_ref().ok_or_else(|| {
            ToolError::execution("MCP OAuth is available only in the daemon's global runtime")
        })?;
        Ok(credentials.scoped(&self.name, self.oauth_binding()?))
    }

    async fn authorization_manager(
        &self,
    ) -> Result<(AuthorizationManager, OAuthExchangeState), ToolError> {
        let url = self.loaded.config.url.as_ref().ok_or_else(|| {
            ToolError::execution("OAuth is available only for remote MCP servers")
        })?;
        *self
            .auth
            .response_error
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
        let authorization_code = OAuthExchangeState {
            response_error: self.auth.response_error.clone(),
            ..Default::default()
        };
        // The HTTP client needs the same credential store the manager uses so
        // the refresh claim can compare the outgoing refresh token against
        // disk (D6-correctness).
        let store = self.oauth_store()?;
        let http_client = McpOAuthHttpClient::new(authorization_code.clone(), store.clone())?;
        let mut manager =
            AuthorizationManager::new_with_oauth_http_client(url, Arc::new(http_client))
                .await
                .map_err(|error| ToolError::execution(authorization_code.diagnostic(&error)))?;
        manager.set_credential_store(store);
        Ok((manager, authorization_code))
    }

    async fn begin_auth(self: &Arc<Self>, redirect_uri: &str) -> Result<String, ToolError> {
        let redirect_uri = loopback_redirect_uri(redirect_uri)?;
        if !self.oauth_enabled() {
            return Err(ToolError::execution(format!(
                "MCP server `{}` does not use OAuth (stdio, oauth=false, or a static Authorization header takes precedence)",
                self.name
            )));
        }
        if self.current_state() != McpServerState::NeedsAuth {
            return Err(ToolError::execution(format!(
                "MCP server `{}` does not currently require OAuth authorization",
                self.name
            )));
        }
        if let Some(url) = self.pending_authorization_url(&redirect_uri)? {
            return Ok(url);
        }

        let store = self.oauth_store()?;
        store
            .clear()
            .await
            .map_err(|_| ToolError::execution("MCP OAuth credential reset failed"))?;
        let (mut manager, authorization_code) = self.authorization_manager().await?;
        let settings = self.loaded.config.oauth.settings();
        let mut request = AuthorizationRequest::new(redirect_uri.as_str())
            .with_client_name("Cookie Agent")
            .with_application_type("native");
        let challenge = self
            .auth
            .challenge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(challenge) = &challenge {
            request = request.with_challenge(challenge);
        }
        let metadata = manager
            .resolve_metadata_from_challenge(challenge.as_deref())
            .await
            .map_err(|error| ToolError::execution(authorization_code.diagnostic(&error)))?;
        manager.set_metadata(metadata.metadata);
        request = apply_oauth_settings(request, &settings);
        let session = AuthorizationSession::new(manager, request)
            .await
            .map_err(|(_, error)| ToolError::execution(authorization_code.diagnostic(&error)))?;
        let authorization_url = session.get_authorization_url().to_owned();
        let cancellation = CancellationToken::new();
        let generation = self.auth.generation.fetch_add(1, Ordering::AcqRel) + 1;
        {
            let mut flow = self
                .auth
                .flow
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(existing) = flow.as_ref() {
                if existing.redirect_uri == redirect_uri {
                    return Ok(existing.authorization_url.clone());
                }
                if existing.exchange.is_none() {
                    return Err(self.completing_error());
                }
                existing.cancellation.cancel();
            }
            *flow = Some(OAuthFlowState {
                generation,
                authorization_url: authorization_url.clone(),
                redirect_uri,
                cancellation: cancellation.clone(),
                exchange: Some((session, authorization_code)),
            });
        }
        self.set_auth_in_progress(true);
        self.set_status(
            McpServerState::NeedsAuth,
            Some("waiting for OAuth browser callback".into()),
        );

        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| ToolError::execution("MCP registry is unavailable"))?;
        let server = Arc::clone(self);
        let task_registry = Arc::clone(&registry);
        let task = tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(OAUTH_CALLBACK_TIMEOUT) => {}
                () = cancellation.cancelled() => return,
                () = task_registry.shutdown.cancelled() => return,
                () = server.superseded.cancelled() => return,
            }
            if server.expire_auth_flow(generation) {
                server.set_status(
                    McpServerState::NeedsAuth,
                    Some("OAuth authorization timed out; authenticate to try again".into()),
                );
            }
        });
        registry
            .connection_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(task);
        Ok(authorization_url)
    }

    /// The URL of a flow already waiting on `redirect_uri`, so a client that
    /// asks again resumes it. A flow waiting on another client's callback is
    /// replaced once the new one is built, unless its code is already being
    /// exchanged.
    fn pending_authorization_url(
        &self,
        redirect_uri: &url::Url,
    ) -> Result<Option<String>, ToolError> {
        let flow = self
            .auth
            .flow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match flow.as_ref() {
            Some(existing) if existing.redirect_uri == *redirect_uri => {
                Ok(Some(existing.authorization_url.clone()))
            }
            Some(existing) if existing.exchange.is_none() => Err(self.completing_error()),
            _ => Ok(None),
        }
    }

    fn completing_error(&self) -> ToolError {
        ToolError::execution(format!(
            "MCP server `{}` is already completing OAuth authorization",
            self.name
        ))
    }

    /// Exchanges the code in the browser's redirect, which the client caught
    /// on its own loopback. A URL that is not this flow's callback is
    /// rejected without ending the flow, so a mistaken paste can be retried;
    /// anything that reaches the exchange, including a denial or a state
    /// mismatch, ends it.
    async fn complete_auth(self: &Arc<Self>, callback_url: &str) -> Result<(), ToolError> {
        let callback = url::Url::parse(callback_url.trim()).ok();
        let registry = self
            .registry
            .upgrade()
            .filter(|registry| !registry.shutdown.is_cancelled())
            .ok_or_else(|| ToolError::execution("MCP registry is unavailable"))?;
        let (generation, session, authorization_code, cancellation) = {
            let mut flow = self
                .auth
                .flow
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let state = flow.as_mut().ok_or_else(|| {
                ToolError::execution(format!(
                    "MCP server `{}` has no OAuth authorization in progress",
                    self.name
                ))
            })?;
            if !callback
                .as_ref()
                .is_some_and(|callback| is_callback_for(callback, &state.redirect_uri))
            {
                return Err(ToolError::execution(format!(
                    "not the OAuth callback for MCP server `{}`: expected the browser's redirect to {}",
                    self.name, state.redirect_uri
                )));
            }
            let (session, authorization_code) = state
                .exchange
                .take()
                .ok_or_else(|| self.completing_error())?;
            (
                state.generation,
                session,
                authorization_code,
                state.cancellation.clone(),
            )
        };
        let callback = callback.expect("validated callback URL");
        let cancelled = || ToolError::execution("OAuth authorization was cancelled");
        let auth_guard = tokio::select! {
            lock = self.auth.lock.lock() => lock,
            () = cancellation.cancelled() => return Err(cancelled()),
            () = registry.shutdown.cancelled() => return Err(cancelled()),
        };
        let exchanged = match parse_oauth_callback(callback.as_str()) {
            Ok(callback) => {
                authorization_code.install(callback.code);
                tokio::select! {
                    result = session.handle_callback_with_issuer(
                        REDACTED_AUTHORIZATION_CODE,
                        &callback.csrf_token,
                        callback.issuer.as_deref(),
                    ) => result.map(drop).map_err(|error| authorization_code.diagnostic(&error)),
                    () = cancellation.cancelled() => return Err(cancelled()),
                    () = registry.shutdown.cancelled() => return Err(cancelled()),
                }
            }
            Err(error) => Err(error),
        };
        drop(auth_guard);
        if !self.finish_auth_flow(generation) {
            return Err(cancelled());
        }
        if let Err(message) = exchanged {
            let message = cookie_agent_protocol::diagnostics::detail(&format!(
                "OAuth authorization failed; authenticate to try again\n{message}"
            ))
            .to_string();
            self.set_status(McpServerState::NeedsAuth, Some(message.clone()));
            return Err(ToolError::execution(message));
        }
        *self
            .auth
            .challenge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.cancel_tool_refresh();
        if let Some(mut service) = self.service.lock().await.take() {
            let _ = service.close_with_timeout(Duration::from_secs(4)).await;
        }
        self.set_status(McpServerState::Disconnected, None);
        let server = Arc::clone(self);
        let task = tokio::spawn(async move {
            if let Err(error) = server.connect().await {
                server.fail(error.to_string());
            }
        });
        registry
            .connection_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(task);
        Ok(())
    }

    /// Ends a flow nobody completed in time. A flow whose code is being
    /// exchanged is left to finish.
    fn expire_auth_flow(&self, generation: u64) -> bool {
        let mut flow = self
            .auth
            .flow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !flow
            .as_ref()
            .is_some_and(|flow| flow.generation == generation && flow.exchange.is_some())
        {
            return false;
        }
        *flow = None;
        drop(flow);
        self.set_auth_in_progress(false);
        true
    }

    fn finish_auth_flow(&self, generation: u64) -> bool {
        let mut flow = self
            .auth
            .flow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let finished = if flow
            .as_ref()
            .is_some_and(|flow| flow.generation == generation)
        {
            if let Some(flow) = flow.take() {
                flow.cancellation.cancel();
            }
            true
        } else {
            false
        };
        drop(flow);
        if finished {
            self.set_auth_in_progress(false);
        }
        finished
    }
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, windows))]
mod windows_tests {
    use super::OAuthCredentialFile;

    #[test]
    fn oauth_store_is_acl_protected_on_windows() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let path = temporary.path().join("oauth").join("mcp-oauth.json");
        let store = OAuthCredentialFile::open(path.clone()).expect("OAuth store");
        store.update(|_| {}).expect("persist empty store");
        cookie_agent_models::secure_store::verify_windows_private_creation(&path)
            .expect("store ACL");
        cookie_agent_models::secure_store::verify_windows_private_creation(
            &path.parent().unwrap().join("mcp-oauth.lock"),
        )
        .expect("lock ACL");
    }
}
