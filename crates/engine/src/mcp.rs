#![allow(deprecated)]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[cfg(unix)]
use std::{
    fs::{self, OpenOptions},
    io::{Read as _, Write as _},
};

use async_trait::async_trait;
use base64::Engine as _;
use cookie_agent_config::{LoadedMcpServer, McpOAuthSettings, McpServerConfig};
#[cfg(windows)]
use cookie_agent_models::secure_store::{DEFAULT_LOCK_BUDGET, SecureDirectory};
#[cfg(unix)]
use cookie_agent_models::secure_store::{DEFAULT_LOCK_BUDGET, lock_within_file, unlock};
use cookie_agent_protocol::{
    ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, PermissionAction,
    PersistedToolResult as ToolResult, PreparedApprovalResource, PreparedBindingLifetime,
    PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
    PreparedResourceIdentity, SafeDisplayText, Sha256Digest, ToolAttachment, ToolCallId,
    ToolEmittedContent, ToolEmittedMessage, ToolEmittedMessageRole,
};
use futures_util::StreamExt as _;
use process_wrap::tokio::CommandWrap;
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use rmcp::transport::auth::{AuthorizationCallback, OAuthClientConfig, OAuthTokenResponse};
use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt, Peer,
    model::{
        CallToolRequestParams, ContentBlock, CreateMessageRequestParams, CreateMessageResult,
        ElicitRequestParams, ElicitResult, ErrorCode, ErrorData, ListRootsResult,
        ProgressNotificationParam, ProtocolVersion, ResourceContents, Tool,
    },
    service::{
        NotificationContext, RequestContext, RoleClient, RunningService, RxJsonRpcMessage,
        TxJsonRpcMessage,
    },
    transport::{
        AuthClient, AuthorizationManager, AuthorizationRequest, AuthorizationSession,
        CredentialStore, OAuthHttpClient, OAuthHttpClientFuture, OAuthHttpRedirectPolicy,
        StoredCredentials, StreamableHttpClientTransport, TokioChildProcess, Transport,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use tokio::sync::Notify;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::Mutex as AsyncMutex,
};
use tokio_util::sync::CancellationToken;

use crate::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolConcurrency, ToolError,
    ToolExecutionContext, ToolPreparationContext, ToolProgress, ToolProvider, ToolSpec,
};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const TOOL_LIST_DEBOUNCE: Duration = Duration::from_millis(750);
#[cfg(not(test))]
const OAUTH_CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
#[cfg(test)]
const OAUTH_CALLBACK_TIMEOUT: Duration = Duration::from_millis(200);
#[cfg(any(unix, test))]
const OAUTH_STORE_FILE: &str = "mcp-oauth.json";
const OAUTH_STORE_LOCK_FILE: &str = "mcp-oauth.lock";
const OAUTH_CALLBACK_MAX_BYTES: usize = 16 * 1024;
const OAUTH_STORE_MAX_BYTES: u64 = 1024 * 1024;
const REDACTED_AUTHORIZATION_CODE: &str = "cookie-agent-redacted-authorization-code";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    Disabled,
    Disconnected,
    Connecting,
    Connected,
    NeedsAuth,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct McpServerStatus {
    pub server: String,
    pub state: McpServerState,
    pub message: Option<String>,
    pub tools: Vec<String>,
    pub auth_in_progress: bool,
}

#[derive(Clone)]
struct CachedTool {
    raw_name: String,
    spec: ToolSpec,
}

type ClientService = RunningService<RoleClient, McpClientHandler>;

struct ServerRuntime {
    name: String,
    sanitized_name: String,
    loaded: LoadedMcpServer,
    status: Mutex<McpServerStatus>,
    tools: Mutex<Vec<CachedTool>>,
    list_generation: AtomicU64,
    publication: Mutex<()>,
    tool_refresh: Mutex<ToolRefreshDebounce>,
    connect_lock: AsyncMutex<()>,
    service: AsyncMutex<Option<ClientService>>,
    superseded: CancellationToken,
    registry: Weak<RegistryInner>,
    progress: AsyncMutex<HashMap<ToolCallId, crate::ProgressSink>>,
    auth_challenge: Mutex<Option<String>>,
    oauth_response_error: Arc<Mutex<Option<String>>>,
    auth_flow: Mutex<Option<OAuthFlowState>>,
    auth_generation: AtomicU64,
    auth_lock: AsyncMutex<()>,
}

struct ToolRefreshDebounce {
    peer: Option<Peer<RoleClient>>,
    cancellation: CancellationToken,
    notify: Arc<Notify>,
    worker: Option<tokio::task::JoinHandle<()>>,
    #[cfg(test)]
    armed: Arc<AtomicU64>,
}

impl Default for ToolRefreshDebounce {
    fn default() -> Self {
        Self {
            peer: None,
            cancellation: CancellationToken::new(),
            notify: Arc::new(Notify::new()),
            worker: None,
            #[cfg(test)]
            armed: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl ToolRefreshDebounce {
    fn cancel(&mut self) {
        self.peer = None;
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

impl Drop for ServerRuntime {
    fn drop(&mut self) {
        self.tool_refresh
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .cancel();
    }
}

struct RegistryInner {
    servers: Mutex<BTreeMap<String, Arc<ServerRuntime>>>,
    reserved_names: Mutex<HashSet<String>>,
    claimed_names: Mutex<HashMap<String, String>>,
    plugin_collision_handler: Mutex<Option<PluginCollisionHandler>>,
    oauth_credentials: OAuthCredentialFile,
    shutdown: CancellationToken,
    connection_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    active_connects: AtomicUsize,
    connects_idle: Notify,
    eager_pending: AtomicUsize,
    eager_ready: Notify,
}

type PluginCollisionHandler = Arc<dyn Fn(&str, &str, Option<&str>) + Send + Sync>;

struct OAuthFlowState {
    generation: u64,
    authorization_url: String,
    cancellation: CancellationToken,
}

#[derive(Clone)]
struct OAuthCredentialFile {
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

#[derive(Clone)]
pub struct McpRegistry {
    inner: Arc<RegistryInner>,
}

impl std::fmt::Debug for McpRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpRegistry")
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct McpClientHandler {
    server: Weak<ServerRuntime>,
}

struct ActiveConnect(Arc<RegistryInner>);

impl ActiveConnect {
    fn new(registry: Arc<RegistryInner>) -> Self {
        registry.active_connects.fetch_add(1, Ordering::AcqRel);
        Self(registry)
    }
}

impl Drop for ActiveConnect {
    fn drop(&mut self) {
        if self.0.active_connects.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.connects_idle.notify_waiters();
        }
    }
}

struct EagerConnect(Arc<RegistryInner>);

impl Drop for EagerConnect {
    fn drop(&mut self) {
        if self.0.eager_pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.eager_ready.notify_waiters();
        }
    }
}

#[derive(Clone)]
struct ChildCleanup(Arc<ChildCleanupInner>);

struct ChildCleanupInner {
    done: AtomicBool,
    notify: Notify,
}

impl ChildCleanup {
    fn new() -> Self {
        Self(Arc::new(ChildCleanupInner {
            done: AtomicBool::new(false),
            notify: Notify::new(),
        }))
    }

    fn finish(&self) {
        self.0.done.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }

    async fn wait(&self) {
        while !self.0.done.load(Ordering::Acquire) {
            let notified = self.0.notify.notified();
            if self.0.done.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
    }
}

struct ReapingChildTransport {
    inner: Option<TokioChildProcess>,
    cleanup: ChildCleanup,
}

impl ReapingChildTransport {
    fn new(inner: TokioChildProcess, cleanup: ChildCleanup) -> Self {
        Self {
            inner: Some(inner),
            cleanup,
        }
    }
}

impl Transport<RoleClient> for ReapingChildTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner
            .as_mut()
            .expect("open child transport")
            .send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.inner.as_mut().expect("open child transport").receive()
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let inner = self.inner.take();
        let cleanup = self.cleanup.clone();
        async move {
            let result = match inner {
                Some(mut inner) => inner.graceful_shutdown().await,
                None => Ok(()),
            };
            cleanup.finish();
            result
        }
    }
}

impl Drop for ReapingChildTransport {
    fn drop(&mut self) {
        let Some(mut inner) = self.inner.take() else {
            return;
        };
        let cleanup = self.cleanup.clone();
        tokio::spawn(async move {
            let _ = inner.graceful_shutdown().await;
            cleanup.finish();
        });
    }
}

impl ClientHandler for McpClientHandler {
    fn create_message(
        &self,
        _params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<CreateMessageResult, ErrorData>> + Send + '_ {
        std::future::ready(Err(unsupported_server_request("sampling/createMessage")))
    }

    fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<ListRootsResult, ErrorData>> + Send + '_ {
        std::future::ready(Err(unsupported_server_request("roots/list")))
    }

    fn create_elicitation(
        &self,
        _request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<ElicitResult, ErrorData>> + Send + '_ {
        std::future::ready(Err(unsupported_server_request("elicitation/create")))
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let Some(server) = self.server.upgrade() else {
            return;
        };
        let message = params.message.unwrap_or_else(|| match params.total {
            Some(total) => format!("MCP progress: {} / {total}", params.progress),
            None => format!("MCP progress: {}", params.progress),
        });
        let sinks = server
            .progress
            .lock()
            .await
            .iter()
            .map(|(id, sink)| (*id, sink.clone()))
            .collect::<Vec<_>>();
        for (tool_call_id, sink) in sinks {
            let _ = sink
                .send(ToolProgress {
                    output: Vec::new(),
                    tool_call_id,
                    message: message.clone(),
                    display: Some(safe_title(&message).to_string()),
                })
                .await;
        }
    }

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        let Some(server) = self.server.upgrade() else {
            return;
        };
        server.schedule_tool_refresh(context.peer);
    }
}

fn unsupported_server_request(method: &str) -> ErrorData {
    ErrorData::new(
        ErrorCode::METHOD_NOT_FOUND,
        format!("unsupported MCP server request `{method}`"),
        None,
    )
}

fn oauth_enabled_config(config: &McpServerConfig) -> bool {
    config.url.is_some()
        && config.oauth.enabled()
        && !config
            .headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("authorization"))
}

fn auth_challenge(error: &(dyn std::error::Error + 'static)) -> Option<String> {
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

fn initialize_error_requires_auth(error: &rmcp::service::ClientInitializeError) -> bool {
    error.is_authorization_required()
        || match error {
            rmcp::service::ClientInitializeError::TransportError { error, .. } => {
                oauth_requires_auth(error.error.as_ref())
            }
            _ => false,
        }
}

fn service_error_requires_auth(error: &rmcp::service::ServiceError) -> bool {
    match error {
        rmcp::service::ServiceError::TransportSend(error) => {
            oauth_requires_auth(error.error.as_ref())
        }
        _ => oauth_requires_auth(error),
    }
}

impl OAuthCredentialFile {
    fn open(path: PathBuf) -> Result<Self, ToolError> {
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

async fn receive_oauth_callback(listener: TcpListener) -> Result<(TcpStream, String), ()> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|_| ())?;
        let mut request = Vec::new();
        loop {
            let mut buffer = [0_u8; 1024];
            let read = stream.read(&mut buffer).await.map_err(|_| ())?;
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            if request.len() > OAUTH_CALLBACK_MAX_BYTES {
                break;
            }
        }
        if request.len() > OAUTH_CALLBACK_MAX_BYTES {
            let _ = write_oauth_browser_response(&mut stream, false).await;
            continue;
        }
        let Ok(request) = std::str::from_utf8(&request) else {
            let _ = write_oauth_browser_response(&mut stream, false).await;
            continue;
        };
        let Some(target) = request
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("GET "))
            .and_then(|line| line.split_once(' '))
            .map(|(target, _)| target)
        else {
            let _ = write_oauth_browser_response(&mut stream, false).await;
            continue;
        };
        let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
            let _ = write_oauth_browser_response(&mut stream, false).await;
            continue;
        };
        if url.path() != "/callback" {
            let _ = write_oauth_browser_response(&mut stream, false).await;
            continue;
        }
        return Ok((stream, url.to_string()));
    }
}

async fn write_oauth_browser_response(
    stream: &mut TcpStream,
    success: bool,
) -> std::io::Result<()> {
    let (status, body) = if success {
        (
            "200 OK",
            "Authorization complete. You can close this window.",
        )
    } else {
        (
            "400 Bad Request",
            "Authorization failed. Return to Cookie Agent and try again.",
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

impl McpRegistry {
    pub(crate) fn set_plugin_collision_handler(&self, handler: PluginCollisionHandler) {
        *self
            .inner
            .plugin_collision_handler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handler);
    }

    pub(crate) fn claim_plugin_tools(
        &self,
        plugin: &str,
        names: &[String],
    ) -> Result<(), ToolError> {
        let reserved = self
            .inner
            .reserved_names
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut claimed = self
            .inner
            .claimed_names
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let owner = format!("plugin:{plugin}");
        let mut local = HashSet::new();
        let mut displaced = Vec::new();
        for name in names {
            if reserved.contains(name) || !local.insert(name) {
                return Err(ToolError::execution(format!(
                    "plugin `{plugin}` declared colliding tool name `{name}`"
                )));
            }
            if let Some(current) = claimed.get(name) {
                if let Some(previous) = current.strip_prefix("plugin:") {
                    if previous != plugin {
                        displaced.push((previous.to_owned(), name.clone()));
                    }
                } else {
                    return Err(ToolError::execution(format!(
                        "plugin `{plugin}` declared colliding tool name `{name}`"
                    )));
                }
            }
        }
        for name in names {
            claimed.insert(name.clone(), owner.clone());
        }
        drop(claimed);
        drop(reserved);
        let collision_handler = self
            .inner
            .plugin_collision_handler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(handler) = collision_handler {
            for (previous, tool) in displaced {
                handler(&previous, &tool, Some(plugin));
            }
        }
        Ok(())
    }

    pub(crate) fn plugin_owns_tool(&self, plugin: &str, tool: &str) -> bool {
        self.inner
            .claimed_names
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(tool)
            .is_some_and(|owner| owner == &format!("plugin:{plugin}"))
    }

    pub(crate) fn release_plugin_tools(&self, plugin: &str) {
        let owner = format!("plugin:{plugin}");
        self.inner
            .claimed_names
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, current| current != &owner);
    }

    pub(crate) fn new(
        servers: BTreeMap<String, LoadedMcpServer>,
        oauth_path: std::path::PathBuf,
    ) -> Result<Self, ToolError> {
        let oauth_credentials = OAuthCredentialFile::open(oauth_path)?;
        let inner = Arc::new(RegistryInner {
            servers: Mutex::new(BTreeMap::new()),
            reserved_names: Mutex::new(HashSet::from([
                "read".into(),
                "write".into(),
                "edit".into(),
                "bash".into(),
                "delegate_subagent".into(),
                "get_subagent_result".into(),
                "cancel_subagent".into(),
                "skill".into(),
            ])),
            claimed_names: Mutex::new(HashMap::new()),
            plugin_collision_handler: Mutex::new(None),
            oauth_credentials,
            shutdown: CancellationToken::new(),
            connection_tasks: Mutex::new(Vec::new()),
            active_connects: AtomicUsize::new(0),
            connects_idle: Notify::new(),
            eager_pending: AtomicUsize::new(0),
            eager_ready: Notify::new(),
        });
        let mut sanitized_servers = HashMap::new();
        let mut runtimes = BTreeMap::new();
        for (name, loaded) in servers {
            let sanitized_name = sanitize_name(&name);
            if let Some(existing) = sanitized_servers.insert(sanitized_name.clone(), name.clone()) {
                return Err(ToolError::execution(format!(
                    "MCP server `{name}` collides with server `{existing}` after name sanitization"
                )));
            }
            let state = if !loaded.config.enabled {
                McpServerState::Disabled
            } else {
                McpServerState::Disconnected
            };
            let runtime = Arc::new(ServerRuntime {
                name: name.clone(),
                sanitized_name,
                loaded,
                status: Mutex::new(McpServerStatus {
                    server: name.clone(),
                    state,
                    message: None,
                    tools: Vec::new(),
                    auth_in_progress: false,
                }),
                tools: Mutex::new(Vec::new()),
                list_generation: AtomicU64::new(0),
                publication: Mutex::new(()),
                tool_refresh: Mutex::new(ToolRefreshDebounce::default()),
                connect_lock: AsyncMutex::new(()),
                service: AsyncMutex::new(None),
                superseded: CancellationToken::new(),
                registry: Arc::downgrade(&inner),
                progress: AsyncMutex::new(HashMap::new()),
                auth_challenge: Mutex::new(None),
                oauth_response_error: Arc::default(),
                auth_flow: Mutex::new(None),
                auth_generation: AtomicU64::new(0),
                auth_lock: AsyncMutex::new(()),
            });
            runtimes.insert(name, runtime);
        }
        *inner.servers.lock().unwrap_or_else(|p| p.into_inner()) = runtimes;
        Ok(Self { inner })
    }

    pub(crate) fn reserve_provider(&self, provider: &dyn ToolProvider) -> Result<(), ToolError> {
        let Ok(tools) = provider.tools_for_session(&SessionToolContext::new(
            cookie_agent_protocol::SessionId::new_v7(),
        )) else {
            // Session-dependent providers are checked when they publish tools.
            return Ok(());
        };
        let mut reserved = self
            .inner
            .reserved_names
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let claimed = self
            .inner
            .claimed_names
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for tool in tools {
            if let Some(server) = claimed.get(&tool.name) {
                return Err(ToolError::execution(format!(
                    "MCP server `{server}` generated tool name `{}` which collides with a registered tool",
                    tool.name
                )));
            }
            reserved.insert(tool.name);
        }
        Ok(())
    }

    pub(crate) fn start_eager(&self, runtime: &tokio::runtime::Handle) {
        let servers = self
            .servers()
            .into_iter()
            .filter(|server| {
                server.loaded.config.enabled
                    && !server.loaded.config.lazy
                    && server.current_state() == McpServerState::Disconnected
            })
            .collect::<Vec<_>>();
        self.inner
            .eager_pending
            .store(servers.len(), Ordering::Release);
        for server in servers {
            let registry = Arc::clone(&self.inner);
            let task_registry = Arc::clone(&registry);
            let eager = EagerConnect(Arc::clone(&task_registry));
            let task = runtime.spawn(async move {
                let _eager = eager;
                if let Err(error) = server.connect().await
                    && !task_registry.shutdown.is_cancelled()
                {
                    server.fail(error.to_string());
                }
            });
            registry
                .connection_tasks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(task);
        }
    }

    pub(crate) async fn await_eager_ready(&self) {
        while self.inner.eager_pending.load(Ordering::Acquire) != 0 {
            let notified = self.inner.eager_ready.notified();
            if self.inner.eager_pending.load(Ordering::Acquire) == 0 {
                break;
            }
            notified.await;
        }
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        for server in self.servers() {
            server.cancel_tool_refresh();
        }
        let tasks = self
            .inner
            .connection_tasks
            .lock()
            .map(|mut tasks| tasks.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for mut task in tasks {
            if tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        for server in self.servers() {
            let mut service = server.service.lock().await;
            if let Some(service) = service.as_mut() {
                let _ = service.close_with_timeout(Duration::from_secs(4)).await;
            }
            *service = None;
        }
        while self.inner.active_connects.load(Ordering::Acquire) != 0 {
            let notified = self.inner.connects_idle.notified();
            if self.inner.active_connects.load(Ordering::Acquire) == 0 {
                break;
            }
            notified.await;
        }
    }

    #[must_use]
    pub fn statuses(&self) -> Vec<McpServerStatus> {
        self.servers()
            .into_iter()
            .map(|server| {
                server
                    .status
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
            })
            .collect()
    }

    pub(crate) async fn upsert_server(
        &self,
        name: String,
        loaded: LoadedMcpServer,
    ) -> Result<(), ToolError> {
        let sanitized_name = sanitize_name(&name);
        let previous = {
            let servers = self
                .inner
                .servers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(existing) = servers
                .values()
                .find(|server| server.name != name && server.sanitized_name == sanitized_name)
            {
                return Err(ToolError::execution(format!(
                    "MCP server `{name}` collides with server `{}` after name sanitization",
                    existing.name
                )));
            }
            servers.get(&name).cloned()
        };
        let previous_binding = previous
            .as_ref()
            .and_then(|server| OAuthCredentialBinding::from_config(&server.loaded.config));
        let new_binding = OAuthCredentialBinding::from_config(&loaded.config);
        let binding_changed = previous.is_some() && previous_binding != new_binding;
        let _previous_auth = if let Some(previous) = &previous {
            let auth = previous.auth_lock.lock().await;
            if binding_changed && let Some(binding) = &previous_binding {
                self.inner.oauth_credentials.remove(&name, binding)?;
            }
            previous.supersede();
            Some(auth)
        } else {
            None
        };
        let needs_auth_after_edit = binding_changed && oauth_enabled_config(&loaded.config);
        let state = if !loaded.config.enabled {
            McpServerState::Disabled
        } else if needs_auth_after_edit {
            McpServerState::NeedsAuth
        } else {
            McpServerState::Disconnected
        };
        let message = (state == McpServerState::NeedsAuth).then(|| {
            "OAuth credentials invalidated by MCP server configuration change; authenticate to connect".into()
        });
        let runtime = Arc::new(ServerRuntime {
            name: name.clone(),
            sanitized_name,
            loaded,
            status: Mutex::new(McpServerStatus {
                server: name.clone(),
                state,
                message,
                tools: Vec::new(),
                auth_in_progress: false,
            }),
            tools: Mutex::new(Vec::new()),
            list_generation: AtomicU64::new(0),
            publication: Mutex::new(()),
            tool_refresh: Mutex::new(ToolRefreshDebounce::default()),
            connect_lock: AsyncMutex::new(()),
            service: AsyncMutex::new(None),
            superseded: CancellationToken::new(),
            registry: Arc::downgrade(&self.inner),
            progress: AsyncMutex::new(HashMap::new()),
            auth_challenge: Mutex::new(None),
            oauth_response_error: Arc::default(),
            auth_flow: Mutex::new(None),
            auth_generation: AtomicU64::new(0),
            auth_lock: AsyncMutex::new(()),
        });
        {
            let mut servers = self
                .inner
                .servers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            servers.insert(name, Arc::clone(&runtime));
        }
        drop(_previous_auth);
        if let Some(previous) = previous
            && let Some(mut service) = previous.service.lock().await.take()
        {
            let _ = service.close_with_timeout(Duration::from_secs(4)).await;
        }
        if runtime.loaded.config.enabled
            && !runtime.loaded.config.lazy
            && runtime.current_state() == McpServerState::Disconnected
        {
            let handle = tokio::runtime::Handle::try_current()
                .map_err(|_| ToolError::execution("MCP runtime is unavailable"))?;
            self.spawn_connection(runtime, &handle, None)?;
        }
        Ok(())
    }

    pub(crate) async fn remove_server(&self, name: &str) -> Result<(), ToolError> {
        let server = self.server(name)?;
        let _auth = server.auth_lock.lock().await;
        if let Some(binding) = OAuthCredentialBinding::from_config(&server.loaded.config) {
            self.inner.oauth_credentials.remove(name, &binding)?;
        }
        server.supersede();
        let removed = {
            let mut servers = self
                .inner
                .servers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let removed = servers
                .get(name)
                .cloned()
                .ok_or_else(|| ToolError::execution(format!("unknown MCP server `{name}`")))?;
            servers.remove(name);
            removed
        };
        if let Some(mut service) = removed.service.lock().await.take() {
            let _ = service.close_with_timeout(Duration::from_secs(4)).await;
        }
        if let Some(flow) = removed
            .auth_flow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            flow.cancellation.cancel();
        }
        Ok(())
    }

    pub(crate) async fn reconnect_server(&self, name: &str) -> Result<(), ToolError> {
        let server = self.server(name)?;
        if !server.loaded.config.enabled {
            return Err(ToolError::execution(format!(
                "MCP server `{name}` is disabled"
            )));
        }
        server.cancel_tool_refresh();
        server.clear_claims();
        server
            .tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        if let Some(mut service) = server.service.lock().await.take() {
            let _ = service.close_with_timeout(Duration::from_secs(4)).await;
        }
        server.set_status(McpServerState::Disconnected, None);
        match server.connect().await {
            Ok(()) => Ok(()),
            Err(error) => {
                server.fail(error.to_string());
                Err(error)
            }
        }
    }

    pub(crate) async fn begin_auth(&self, name: &str) -> Result<String, ToolError> {
        let server = self.server(name)?;
        server.begin_auth().await
    }

    pub(crate) fn cancel_auth(&self, name: &str) -> Result<(), ToolError> {
        let server = self.server(name)?;
        let flow = server
            .auth_flow
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

    fn spawn_connection(
        &self,
        server: Arc<ServerRuntime>,
        runtime: &tokio::runtime::Handle,
        readiness: Option<EagerConnect>,
    ) -> Result<(), ToolError> {
        let registry = Arc::clone(&self.inner);
        let mut tasks = registry
            .connection_tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if registry.shutdown.is_cancelled() {
            return Err(ToolError::execution("MCP registry is shutting down"));
        }
        let task_registry = Arc::clone(&registry);
        let task = runtime.spawn(async move {
            let _readiness = readiness;
            if let Err(error) = server.connect().await
                && !task_registry.shutdown.is_cancelled()
            {
                server.fail(error.to_string());
            }
        });
        tasks.push(task);
        Ok(())
    }

    fn servers(&self) -> Vec<Arc<ServerRuntime>> {
        self.inner
            .servers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect()
    }

    fn server(&self, name: &str) -> Result<Arc<ServerRuntime>, ToolError> {
        self.inner
            .servers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::execution(format!("unknown MCP server `{name}`")))
    }

    async fn resolve_tool(
        &self,
        tool_name: &str,
    ) -> Result<(Arc<ServerRuntime>, String), ToolError> {
        for server in self.servers() {
            if let Some(raw) = server.raw_tool_name(tool_name) {
                return Ok((server, raw));
            }
        }
        for server in self.servers() {
            if server.loaded.config.lazy
                && tool_name.starts_with(&format!("{}_", server.sanitized_name))
            {
                server.connect().await?;
                if let Some(raw) = server.raw_tool_name(tool_name) {
                    return Ok((server, raw));
                }
            }
        }
        Err(ToolError::execution(format!(
            "unknown MCP tool `{tool_name}`"
        )))
    }
}

impl ServerRuntime {
    fn timeout(&self) -> Duration {
        Duration::from_millis(self.loaded.config.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS))
    }

    fn current_state(&self) -> McpServerState {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state
            .clone()
    }

    fn set_status(&self, state: McpServerState, message: Option<String>) {
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        status.state = state;
        status.message = message;
        status.tools = self
            .tools
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|tool| tool.spec.name.clone())
            .collect();
    }

    fn set_auth_in_progress(&self, active: bool) {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .auth_in_progress = active;
    }

    fn fail(&self, message: String) {
        let message = cookie_agent_protocol::diagnostics::sanitize(&message, 4096);
        if self.superseded.is_cancelled() {
            return;
        }
        if self.current_state() == McpServerState::NeedsAuth {
            return;
        }
        self.clear_claims();
        self.tools.lock().unwrap_or_else(|p| p.into_inner()).clear();
        self.set_status(McpServerState::Failed, Some(message));
    }

    fn mark_needs_auth(&self, challenge: Option<String>, reason: &str) {
        if self.superseded.is_cancelled() {
            return;
        }
        *self
            .auth_challenge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = challenge;
        self.clear_claims();
        self.tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let response_error = self
            .oauth_response_error
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

    fn supersede(&self) {
        self.superseded.cancel();
        self.cancel_tool_refresh_task();
        if let Some(flow) = self
            .auth_flow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            flow.cancellation.cancel();
        }
        self.set_auth_in_progress(false);
        self.next_list_generation();
        self.clear_claims();
    }

    fn clear_claims(&self) {
        if let Some(registry) = self.registry.upgrade() {
            registry
                .claimed_names
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|_, owner| owner != &self.name);
        }
    }

    fn raw_tool_name(&self, generated: &str) -> Option<String> {
        self.tools
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|tool| tool.spec.name == generated)
            .map(|tool| tool.raw_name.clone())
    }

    fn oauth_enabled(&self) -> bool {
        oauth_enabled_config(&self.loaded.config)
    }

    fn oauth_binding(&self) -> Result<OAuthCredentialBinding, ToolError> {
        OAuthCredentialBinding::from_config(&self.loaded.config)
            .ok_or_else(|| ToolError::execution("OAuth is available only for remote MCP servers"))
    }

    fn oauth_store(&self) -> Result<ServerCredentialStore, ToolError> {
        self.registry
            .upgrade()
            .map(|registry| {
                self.oauth_binding()
                    .map(|binding| registry.oauth_credentials.scoped(&self.name, binding))
            })
            .transpose()?
            .ok_or_else(|| ToolError::execution("MCP registry is unavailable"))
    }

    async fn authorization_manager(
        &self,
    ) -> Result<(AuthorizationManager, OAuthExchangeState), ToolError> {
        let url = self.loaded.config.url.as_ref().ok_or_else(|| {
            ToolError::execution("OAuth is available only for remote MCP servers")
        })?;
        *self
            .oauth_response_error
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
        let authorization_code = OAuthExchangeState {
            response_error: self.oauth_response_error.clone(),
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

    async fn begin_auth(self: &Arc<Self>) -> Result<String, ToolError> {
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
        if let Some(flow) = self
            .auth_flow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            return Ok(flow.authorization_url.clone());
        }

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| ToolError::execution("MCP OAuth callback listener failed"))?;
        let port = listener
            .local_addr()
            .map_err(|_| ToolError::execution("MCP OAuth callback listener failed"))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        let store = self.oauth_store()?;
        store
            .clear()
            .await
            .map_err(|_| ToolError::execution("MCP OAuth credential reset failed"))?;
        let (mut manager, authorization_code) = self.authorization_manager().await?;
        let settings = self.loaded.config.oauth.settings();
        let mut request = AuthorizationRequest::new(&redirect_uri)
            .with_client_name("Cookie Agent")
            .with_application_type("native");
        let challenge = self
            .auth_challenge
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
        let generation = self.auth_generation.fetch_add(1, Ordering::AcqRel) + 1;
        {
            let mut flow = self
                .auth_flow
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(existing) = flow.as_ref() {
                return Ok(existing.authorization_url.clone());
            }
            *flow = Some(OAuthFlowState {
                generation,
                authorization_url: authorization_url.clone(),
                cancellation: cancellation.clone(),
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
            let callback = tokio::select! {
                result = tokio::time::timeout(OAUTH_CALLBACK_TIMEOUT, receive_oauth_callback(listener)) => {
                    match result {
                        Ok(result) => result,
                        Err(_) => {
                            if server.finish_auth_flow(generation) {
                                server.set_status(
                                    McpServerState::NeedsAuth,
                                    Some("OAuth authorization timed out; authenticate to try again".into()),
                                );
                            }
                            return;
                        }
                    }
                }
                () = cancellation.cancelled() => return,
                () = task_registry.shutdown.cancelled() => return,
                () = server.superseded.cancelled() => return,
            };
            let Ok((mut browser, callback_url)) = callback else {
                if server.finish_auth_flow(generation) {
                    server.set_status(
                        McpServerState::NeedsAuth,
                        Some("invalid OAuth callback; authenticate to try again".into()),
                    );
                }
                return;
            };
            let auth_guard = tokio::select! {
                lock = server.auth_lock.lock() => lock,
                () = cancellation.cancelled() => return,
                () = task_registry.shutdown.cancelled() => return,
                () = server.superseded.cancelled() => return,
            };
            let exchanged = match parse_oauth_callback(&callback_url) {
                Ok(callback) => {
                    authorization_code.install(callback.code);
                    tokio::select! {
                        result = session.handle_callback_with_issuer(
                            REDACTED_AUTHORIZATION_CODE,
                            &callback.csrf_token,
                            callback.issuer.as_deref(),
                        ) => result.map_err(|error| authorization_code.diagnostic(&error)),
                        () = cancellation.cancelled() => return,
                        () = task_registry.shutdown.cancelled() => return,
                        () = server.superseded.cancelled() => return,
                    }
                }
                Err(error) => Err(error),
            };
            drop(auth_guard);
            let _ = write_oauth_browser_response(&mut browser, exchanged.is_ok()).await;
            if !server.finish_auth_flow(generation) {
                return;
            }
            if let Err(message) = exchanged {
                server.set_status(
                    McpServerState::NeedsAuth,
                    Some(
                        cookie_agent_protocol::diagnostics::detail(&format!(
                            "OAuth authorization failed; authenticate to try again\n{message}"
                        ))
                        .to_string(),
                    ),
                );
                return;
            }
            *server
                .auth_challenge
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            server.cancel_tool_refresh();
            if let Some(mut service) = server.service.lock().await.take() {
                let _ = service.close_with_timeout(Duration::from_secs(4)).await;
            }
            server.set_status(McpServerState::Disconnected, None);
            if let Err(error) = server.connect().await {
                server.fail(error.to_string());
            }
        });
        registry
            .connection_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(task);
        Ok(authorization_url)
    }

    fn finish_auth_flow(&self, generation: u64) -> bool {
        let mut flow = self
            .auth_flow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let finished = if flow
            .as_ref()
            .is_some_and(|flow| flow.generation == generation)
        {
            *flow = None;
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

    async fn connect(self: &Arc<Self>) -> Result<(), ToolError> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| ToolError::execution("MCP registry is unavailable"))?;
        if registry.shutdown.is_cancelled() {
            return Err(ToolError::execution("MCP registry is shutting down"));
        }
        if self.superseded.is_cancelled() {
            return Err(self.superseded_error());
        }
        let _active = ActiveConnect::new(Arc::clone(&registry));
        let _connect = tokio::select! {
            lock = self.connect_lock.lock() => lock,
            () = registry.shutdown.cancelled() => {
                return Err(ToolError::execution("MCP registry is shutting down"));
            }
            () = self.superseded.cancelled() => {
                return Err(self.superseded_error());
            }
        };
        if registry.shutdown.is_cancelled() {
            return Err(ToolError::execution("MCP registry is shutting down"));
        }
        if self.superseded.is_cancelled() {
            return Err(self.superseded_error());
        }
        match self.current_state() {
            McpServerState::Connected => return Ok(()),
            McpServerState::Disabled => {
                return Err(ToolError::execution(format!(
                    "MCP server `{}` is disabled",
                    self.name
                )));
            }
            McpServerState::NeedsAuth => {
                return Err(ToolError::execution(format!(
                    "MCP server `{}` requires OAuth authorization",
                    self.name
                )));
            }
            McpServerState::Disconnected | McpServerState::Connecting | McpServerState::Failed => {}
        }
        self.reset_tool_refresh();
        self.set_status(McpServerState::Connecting, None);
        let handler = McpClientHandler {
            server: Arc::downgrade(self),
        };
        let lifecycle = ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        };
        let timeout = self.timeout();
        let mut service = if let Some(command) = &self.loaded.config.command {
            let mut process = tokio::process::Command::new(command);
            process
                .args(&self.loaded.config.args)
                .envs(&self.loaded.config.env);
            if let Some(cwd) = &self.loaded.config.cwd {
                process.current_dir(cwd);
            }
            let mut wrapped = CommandWrap::from(process);
            #[cfg(unix)]
            wrapped.wrap(ProcessGroup::leader());
            #[cfg(windows)]
            wrapped.wrap(JobObject);
            let transport = TokioChildProcess::new(wrapped).map_err(|error| {
                ToolError::execution(format!(
                    "MCP server `{}` failed to spawn command `{command}`: {error}",
                    self.name,
                ))
            })?;
            let cleanup = ChildCleanup::new();
            let transport = ReapingChildTransport::new(transport, cleanup.clone());
            let outcome = tokio::select! {
                result = tokio::time::timeout(timeout, handler.serve_with_lifecycle(transport, lifecycle)) => Some(result),
                () = registry.shutdown.cancelled() => None,
                () = self.superseded.cancelled() => None,
            };
            match outcome {
                Some(Ok(Ok(service))) => service,
                Some(Ok(Err(error))) => {
                    cleanup.wait().await;
                    return Err(ToolError::execution(format!(
                        "MCP server `{}` connect failed: {error}",
                        self.name
                    )));
                }
                Some(Err(_)) => {
                    cleanup.wait().await;
                    return Err(ToolError::execution(format!(
                        "MCP server `{}` connect timed out",
                        self.name
                    )));
                }
                None => {
                    cleanup.wait().await;
                    return if self.superseded.is_cancelled() {
                        Err(self.superseded_error())
                    } else {
                        Err(ToolError::execution("MCP registry is shutting down"))
                    };
                }
            }
        } else {
            let url = self
                .loaded
                .config
                .url
                .as_ref()
                .expect("validated MCP transport");
            let mut headers = HashMap::new();
            for (name, value) in &self.loaded.config.headers {
                headers.insert(
                    http::HeaderName::try_from(name)
                        .map_err(|error| ToolError::execution(error.to_string()))?,
                    http::HeaderValue::try_from(value)
                        .map_err(|error| ToolError::execution(error.to_string()))?,
                );
            }
            let config = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.clone())
                .custom_headers(headers);
            let result = if self.oauth_enabled() {
                let (mut manager, _authorization_code) = self.authorization_manager().await?;
                let restored = manager
                    .initialize_from_store()
                    .await
                    .map_err(|_| ToolError::execution("MCP OAuth credential loading failed"))?;
                let settings = self.loaded.config.oauth.settings();
                if restored && let Some(client_id) = settings.client_id {
                    let mut client = OAuthClientConfig::new(client_id, "http://127.0.0.1")
                        .with_scopes(settings.scopes);
                    if let Some(client_secret) = settings.client_secret {
                        client = client.with_client_secret(client_secret);
                    }
                    manager.configure_client(client).map_err(|_| {
                        ToolError::execution("MCP OAuth stored-client setup failed")
                    })?;
                }
                let client = reqwest::Client::builder()
                    .pool_max_idle_per_host(0)
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|_| ToolError::execution("MCP HTTP client setup failed"))?;
                let transport = StreamableHttpClientTransport::with_client(
                    AuthClient::new(client, manager),
                    config,
                );
                tokio::select! {
                    result = tokio::time::timeout(timeout, handler.serve_with_lifecycle(transport, lifecycle)) => result,
                    () = registry.shutdown.cancelled() => {
                        return Err(ToolError::execution("MCP registry is shutting down"));
                    }
                    () = self.superseded.cancelled() => {
                        return Err(self.superseded_error());
                    }
                }
            } else {
                let transport = StreamableHttpClientTransport::from_config(config);
                tokio::select! {
                    result = tokio::time::timeout(timeout, handler.serve_with_lifecycle(transport, lifecycle)) => result,
                    () = registry.shutdown.cancelled() => {
                        return Err(ToolError::execution("MCP registry is shutting down"));
                    }
                    () = self.superseded.cancelled() => {
                        return Err(self.superseded_error());
                    }
                }
            };
            match result {
                Err(_) => {
                    return Err(ToolError::execution(format!(
                        "MCP server `{}` connect timed out",
                        self.name
                    )));
                }
                Ok(Err(error))
                    if self.oauth_enabled() && initialize_error_requires_auth(&error) =>
                {
                    self.mark_needs_auth(
                        error.auth_challenge().map(str::to_owned),
                        &cookie_agent_protocol::diagnostics::error_chain(&error),
                    );
                    return Err(ToolError::execution(format!(
                        "MCP server `{}` requires OAuth authorization",
                        self.name
                    )));
                }
                Ok(Err(error)) => {
                    return Err(ToolError::execution(format!(
                        "MCP server `{}` connect failed: {error}",
                        self.name
                    )));
                }
                Ok(Ok(service)) => service,
            }
        };
        let generation = self.next_list_generation();
        let tools = tokio::select! {
            result = tokio::time::timeout(timeout, service.peer().list_all_tools()) => result,
            () = registry.shutdown.cancelled() => {
                let _ = service.close_with_timeout(Duration::from_secs(4)).await;
                return Err(ToolError::execution("MCP registry is shutting down"));
            }
            () = self.superseded.cancelled() => {
                let _ = service.close_with_timeout(Duration::from_secs(4)).await;
                return Err(self.superseded_error());
            }
        }
        .map_err(|_| {
            ToolError::execution(format!("MCP server `{}` tools/list timed out", self.name))
        })?;
        let tools = match tools {
            Ok(tools) => tools,
            Err(error) if self.oauth_enabled() && service_error_requires_auth(&error) => {
                self.mark_needs_auth(
                    auth_challenge(&error),
                    &cookie_agent_protocol::diagnostics::error_chain(&error),
                );
                let _ = service.close_with_timeout(Duration::from_secs(4)).await;
                return Err(ToolError::execution(format!(
                    "MCP server `{}` requires OAuth authorization",
                    self.name
                )));
            }
            Err(error) => {
                return Err(ToolError::execution(format!(
                    "MCP server `{}` tools/list failed: {error}",
                    self.name
                )));
            }
        };
        self.publish_tools(generation, tools);
        if self.current_state() == McpServerState::Failed {
            return Err(ToolError::execution(
                self.status
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .message
                    .clone()
                    .unwrap_or_default(),
            ));
        }
        let mut installed = self.service.lock().await;
        if registry.shutdown.is_cancelled() || self.superseded.is_cancelled() {
            drop(installed);
            let _ = service.close_with_timeout(Duration::from_secs(4)).await;
            return if self.superseded.is_cancelled() {
                Err(self.superseded_error())
            } else {
                Err(ToolError::execution("MCP registry is shutting down"))
            };
        }
        *installed = Some(service);
        drop(installed);
        self.set_status(McpServerState::Connected, None);
        Ok(())
    }

    fn next_list_generation(&self) -> u64 {
        let _publication = self.publication.lock().unwrap_or_else(|p| p.into_inner());
        self.list_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn schedule_tool_refresh(self: &Arc<Self>, peer: Peer<RoleClient>) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let mut refresh = self
            .tool_refresh
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if refresh.cancellation.is_cancelled() {
            return;
        }
        refresh.peer = Some(peer);
        if refresh.worker.is_none() {
            let server = Arc::downgrade(self);
            let notify = Arc::clone(&refresh.notify);
            let cancellation = refresh.cancellation.clone();
            let shutdown = registry.shutdown.clone();
            let superseded = self.superseded.clone();
            #[cfg(test)]
            let armed = Arc::clone(&refresh.armed);
            refresh.worker = Some(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        () = notify.notified() => {}
                        () = cancellation.cancelled() => return,
                        () = shutdown.cancelled() => return,
                        () = superseded.cancelled() => return,
                    }
                    loop {
                        #[cfg(test)]
                        armed.fetch_add(1, Ordering::Release);
                        tokio::select! {
                            () = tokio::time::sleep(TOOL_LIST_DEBOUNCE) => break,
                            () = notify.notified() => {}
                            () = cancellation.cancelled() => return,
                            () = shutdown.cancelled() => return,
                            () = superseded.cancelled() => return,
                        }
                    }

                    let Some(server) = server.upgrade() else {
                        return;
                    };
                    if cancellation.is_cancelled() {
                        return;
                    }
                    let peer = {
                        let refresh = server
                            .tool_refresh
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if !Arc::ptr_eq(&refresh.notify, &notify) {
                            return;
                        }
                        refresh.peer.clone()
                    };
                    if let Some(peer) = peer
                        && !peer.is_transport_closed()
                    {
                        server.refresh_tools(peer, &cancellation, &shutdown).await;
                    }
                }
            }));
        }
        refresh.notify.notify_one();
    }

    async fn refresh_tools(
        &self,
        peer: Peer<RoleClient>,
        cancellation: &CancellationToken,
        shutdown: &CancellationToken,
    ) {
        let generation = self.next_list_generation();
        let timeout = self.timeout();
        let result = tokio::select! {
            result = tokio::time::timeout(timeout, peer.list_all_tools()) => result,
            () = cancellation.cancelled() => return,
            () = shutdown.cancelled() => return,
            () = self.superseded.cancelled() => return,
        };
        if cancellation.is_cancelled() || shutdown.is_cancelled() {
            return;
        }
        match result {
            Ok(Ok(tools)) => self.publish_tools(generation, tools),
            Ok(Err(error)) if self.oauth_enabled() && service_error_requires_auth(&error) => {
                self.mark_needs_auth(
                    auth_challenge(&error),
                    &cookie_agent_protocol::diagnostics::error_chain(&error),
                );
            }
            Ok(Err(error)) => self
                .publish_refresh_failure(generation, format!("tools/list refresh failed: {error}")),
            Err(_) => {
                self.publish_refresh_failure(generation, "tools/list refresh timed out".into());
            }
        }
    }

    fn reset_tool_refresh(&self) {
        let mut refresh = self
            .tool_refresh
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        refresh.cancel();
        *refresh = ToolRefreshDebounce::default();
    }

    fn cancel_tool_refresh_task(&self) {
        let mut refresh = self
            .tool_refresh
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        refresh.cancel();
    }

    fn cancel_tool_refresh(&self) {
        self.cancel_tool_refresh_task();
        self.next_list_generation();
    }

    fn superseded_error(&self) -> ToolError {
        ToolError::execution(format!("MCP server `{}` was superseded", self.name))
    }

    fn publish_tools(&self, generation: u64, tools: Vec<Tool>) {
        self.publish_tools_before_commit(generation, tools, || {});
    }

    fn publish_refresh_failure(&self, generation: u64, message: String) {
        self.publish_refresh_failure_before_commit(generation, message, || {});
    }

    fn publish_refresh_failure_before_commit(
        &self,
        generation: u64,
        message: String,
        before_commit: impl FnOnce(),
    ) {
        before_commit();
        let _publication = self.publication.lock().unwrap_or_else(|p| p.into_inner());
        if self.list_generation.load(Ordering::Acquire) == generation {
            self.fail(message);
        }
    }

    fn publish_tools_before_commit(
        &self,
        generation: u64,
        tools: Vec<Tool>,
        before_commit: impl FnOnce(),
    ) {
        let converted = tools
            .into_iter()
            .map(|tool| convert_tool(&self.name, &self.sanitized_name, tool))
            .collect::<Result<Vec<_>, _>>();
        before_commit();
        let publication = self.publication.lock().unwrap_or_else(|p| p.into_inner());
        if self.superseded.is_cancelled()
            || self.list_generation.load(Ordering::Acquire) != generation
        {
            return;
        }
        let converted = match converted {
            Ok(tools) => tools,
            Err(error) => {
                self.fail(error.to_string());
                return;
            }
        };
        let unchanged = {
            let current = self
                .tools
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            current.len() == converted.len()
                && current.iter().zip(&converted).all(|(current, refreshed)| {
                    current.raw_name == refreshed.raw_name
                        && current.spec.name == refreshed.spec.name
                        && current.spec.description == refreshed.spec.description
                        && current.spec.parameters == refreshed.spec.parameters
                })
        };
        if unchanged {
            return;
        }
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let reserved = registry
            .reserved_names
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut claimed = registry
            .claimed_names
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        claimed.retain(|_, owner| owner != &self.name);
        let mut local = HashSet::new();
        let mut preempted_plugins = Vec::new();
        for tool in &converted {
            let claimed_by_plugin = claimed
                .get(&tool.spec.name)
                .and_then(|owner| owner.strip_prefix("plugin:"));
            if reserved.contains(&tool.spec.name) || !local.insert(tool.spec.name.clone()) {
                drop(claimed);
                drop(reserved);
                self.fail(format!(
                    "MCP server `{}` generated colliding tool name `{}`",
                    self.name, tool.spec.name
                ));
                return;
            }
            if let Some(plugin) = claimed_by_plugin {
                preempted_plugins.push((plugin.to_owned(), tool.spec.name.clone()));
            } else if claimed.contains_key(&tool.spec.name) {
                drop(claimed);
                drop(reserved);
                self.fail(format!(
                    "MCP server `{}` generated colliding tool name `{}`",
                    self.name, tool.spec.name
                ));
                return;
            }
        }
        for tool in &converted {
            claimed.insert(tool.spec.name.clone(), self.name.clone());
        }
        drop(claimed);
        drop(reserved);
        let collision_handler = registry
            .plugin_collision_handler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(handler) = collision_handler {
            for (plugin, tool) in preempted_plugins {
                handler(&plugin, &tool, None);
            }
        }
        *self.tools.lock().unwrap_or_else(|p| p.into_inner()) = converted;
        drop(publication);
        if self.current_state() == McpServerState::Connected {
            self.set_status(McpServerState::Connected, None);
        }
    }
}

#[async_trait]
impl ToolProvider for McpRegistry {
    fn provider_id(&self) -> &'static str {
        "mcp"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(self
            .servers()
            .into_iter()
            .flat_map(|server| {
                server
                    .tools
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .iter()
                    .map(|tool| tool.spec.clone())
                    .collect::<Vec<_>>()
            })
            .collect())
    }

    fn get_permission_name(_tool_name: &str) -> Result<&'static str, ToolError> {
        Ok("mcp")
    }

    fn permission_for_unlisted_tool(
        &self,
        tool_name: &str,
    ) -> Result<Option<&'static str>, ToolError> {
        Ok(self
            .servers()
            .into_iter()
            .any(|server| tool_name.starts_with(&format!("{}_", server.sanitized_name)))
            .then_some("mcp"))
    }

    fn get_permission_resource(
        &self,
        tool_name: &str,
        _arguments: &Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let owned = self
            .servers()
            .into_iter()
            .any(|server| tool_name.starts_with(&format!("{}_", server.sanitized_name)));
        if !owned {
            return Err(ToolError::execution(format!(
                "unknown MCP tool `{tool_name}`"
            )));
        }
        Ok(("mcp", Some(tool_name.to_owned())))
    }

    fn get_display_argument(&self, name: &str, _arguments: &Value) -> Result<String, ToolError> {
        Ok(name.to_owned())
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let (server, raw_name) = self.resolve_tool(&call.name).await?;
        let arguments = call
            .arguments
            .as_object()
            .cloned()
            .ok_or_else(|| ToolError::execution("MCP tool arguments must be an object"))?;
        let normalized = Value::Object(arguments.clone());
        let argument_bytes = serde_json::to_vec(&normalized)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let label = call.name.clone();
        let resource = PreparedApprovalResource {
            capability: PermissionAction::Mcp,
            canonical: PreparedResourceIdentity::new(format!(
                "mcp-tool:{}",
                Sha256Digest::of_bytes(label.as_bytes())
            ))
            .map_err(|error| ToolError::execution(error.to_string()))?,
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(label.as_bytes()),
            binding_lifetime: PreparedBindingLifetime::RestartStable,
            boundary: ApprovalBoundary::Exact,
            source: ApprovalResourceSource::PrimaryOperation,
        };
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(&argument_bytes),
            vec![ApprovalCapability {
                action: PermissionAction::Mcp,
                operation: PreparedCapabilityOperation::new(format!("{}:call", call.name))
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            }],
            vec![resource],
            Sha256Digest::of_bytes(ctx.workspace_root.to_string_lossy().as_bytes()),
        )
        .map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            normalized,
            None,
            Box::new(McpExecutor {
                server,
                generated_name: call.name,
                raw_name,
                arguments,
                call_id: call.id,
            }),
        )
    }
}

struct McpExecutor {
    server: Arc<ServerRuntime>,
    generated_name: String,
    raw_name: String,
    arguments: Map<String, Value>,
    call_id: ToolCallId,
}

#[async_trait]
impl PreparedExecutor for McpExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        if self.server.raw_tool_name(&self.generated_name).as_deref() != Some(&self.raw_name) {
            return Err(ToolError::operation_changed(
                "MCP tool list changed after approval",
            ));
        }
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<crate::ToolCompletion, ToolError> {
        let result: Result<ToolResult, ToolError> = async move {
        self.revalidate().await?;
        let params = CallToolRequestParams::new(self.raw_name.clone())
            .with_arguments(self.arguments.clone());
        let timeout = self.server.timeout();
        let service = self.server.service.lock().await;
        let Some(service) = service.as_ref() else {
            return Err(ToolError::execution("MCP server is not connected"));
        };
        self.server
            .progress
            .lock()
            .await
            .insert(self.call_id, context.progress.clone());
        let call = service.call_tool(params);
        tokio::pin!(call);
        let result = tokio::select! {
            result = &mut call => match result {
                Err(error)
                    if self.server.oauth_enabled() && service_error_requires_auth(&error) =>
                {
                    self.server.mark_needs_auth(auth_challenge(&error), &cookie_agent_protocol::diagnostics::error_chain(&error));
                    Err(ToolError::execution(format!(
                        "MCP server `{}` requires OAuth authorization",
                        self.server.name
                    )))
                }
                Err(error) => Err(ToolError::execution(error.to_string())),
                Ok(result) => Ok(result),
            },
            _ = context.cancellation.cancelled() => Err(ToolError::execution("MCP tool call cancelled")),
            _ = tokio::time::sleep(timeout) => Err(ToolError::execution("MCP tool call timed out")),
        };
        self.server.progress.lock().await.remove(&self.call_id);
        map_tool_result(&context, &self.generated_name, result?)
}.await;
        result.map(|result| {
            let failed = result.metadata["mcp"]["is_error"] == true;
            let mut completion = crate::ToolCompletion::single(result);
            completion.failed = failed;
            completion
        })
    }
}

fn convert_tool(server: &str, sanitized_server: &str, tool: Tool) -> Result<CachedTool, ToolError> {
    let generated = format!("{}_{}", sanitized_server, sanitize_name(tool.name.as_ref()));
    let description = format!(
        "[{server}] {} (untrusted MCP output)",
        tool.description.as_deref().unwrap_or("MCP tool")
    );
    let mut schema = tool.input_schema.as_ref().clone();
    schema.insert("type".into(), Value::String("object".into()));
    schema.insert("additionalProperties".into(), Value::Bool(false));
    Ok(CachedTool {
        raw_name: tool.name.into_owned(),
        spec: ToolSpec {
            output: Default::default(),
            concurrency: ToolConcurrency::Parallel,
            // External opt-out requires a future extension-protocol capability.
            result_truncation: Default::default(),
            name: generated,
            permission_name: "mcp".into(),
            description,
            parameters: Value::Object(schema),
        },
    })
}

fn map_tool_result(
    context: &ToolExecutionContext,
    tool_name: &str,
    result: rmcp::model::CallToolResult,
) -> Result<ToolResult, ToolError> {
    let mut output = Vec::new();
    let mut attachments = Vec::new();
    let mut emitted_attachments = Vec::new();
    for block in result.content {
        match block {
            ContentBlock::Text(text) => output.push(text.text),
            ContentBlock::Image(image) => {
                if !mcp_attachment_budget_remaining(&attachments, &emitted_attachments) {
                    output.push(
                        "[MCP image attachment omitted: combined attachment limit reached]".into(),
                    );
                    continue;
                }
                match retain_base64_attachment(context, image.mime_type, &image.data) {
                    Ok(retained) => {
                        output.push(
                            if route_mcp_attachment(
                                retained,
                                &mut attachments,
                                &mut emitted_attachments,
                            ) {
                                "[MCP image attachment]".into()
                            } else {
                                "[MCP image attachment omitted: combined attachment limit reached]"
                                    .into()
                            },
                        );
                    }
                    Err(McpAttachmentError::Invalid(error)) => {
                        output.push(format!("[Invalid MCP image content: {error}]"));
                    }
                    Err(McpAttachmentError::Gated(error)) => {
                        return Err(ToolError::Failed(error));
                    }
                }
            }
            ContentBlock::Audio(audio) => {
                if !mcp_attachment_budget_remaining(&attachments, &emitted_attachments) {
                    output.push(
                        "[MCP audio attachment omitted: combined attachment limit reached]".into(),
                    );
                    continue;
                }
                match retain_base64_attachment(context, audio.mime_type, &audio.data) {
                    Ok(retained) => {
                        output.push(
                            if route_mcp_attachment(
                                retained,
                                &mut attachments,
                                &mut emitted_attachments,
                            ) {
                                "[MCP audio attachment]".into()
                            } else {
                                "[MCP audio attachment omitted: combined attachment limit reached]"
                                    .into()
                            },
                        );
                    }
                    Err(McpAttachmentError::Invalid(error)) => {
                        output.push(format!("[Invalid MCP audio content: {error}]"));
                    }
                    Err(McpAttachmentError::Gated(error)) => {
                        return Err(ToolError::Failed(error));
                    }
                }
            }
            ContentBlock::Resource(resource) => match resource.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    output.push(format!("[{uri}]\n{text}"));
                }
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => {
                    if !mcp_attachment_budget_remaining(&attachments, &emitted_attachments) {
                        output.push(format!(
                            "[MCP embedded resource unavailable: {uri}: combined attachment limit reached]"
                        ));
                        continue;
                    }
                    match retain_base64_attachment(
                        context,
                        mime_type.unwrap_or_else(|| "application/octet-stream".into()),
                        &blob,
                    ) {
                        Ok(retained) => {
                            let routed = route_mcp_attachment(
                                retained,
                                &mut attachments,
                                &mut emitted_attachments,
                            );
                            output.push(if routed {
                                format!("[MCP embedded resource attachment: {uri}]")
                            } else {
                                format!(
                                    "[MCP embedded resource unavailable: {uri}: combined attachment limit reached]"
                                )
                            });
                        }
                        Err(McpAttachmentError::Invalid(error)) => output.push(format!(
                            "[MCP embedded resource unavailable: {uri}: {error}]"
                        )),
                        Err(McpAttachmentError::Gated(error)) => {
                            return Err(ToolError::Failed(error));
                        }
                    }
                }
                _ => output.push("[Unsupported MCP embedded resource]".into()),
            },
            ContentBlock::ResourceLink(link) => {
                output.push(format!("[MCP resource link: {} ({})]", link.name, link.uri))
            }
            _ => output.push("[Unsupported MCP content block]".into()),
        }
    }
    let text_output = output.join("\n");
    let structured_content = result.structured_content.filter(|structured| {
        !output
            .iter()
            .chain(std::iter::once(&text_output))
            .any(|text| serde_json::from_str::<Value>(text).ok().as_ref() == Some(structured))
    });
    let mut metadata = serde_json::json!({
        "mcp": {
            "is_error": result.is_error.unwrap_or(false),
        }
    });
    if let Some(structured) = structured_content {
        metadata["mcp"]["structured_content"] = structured;
    }
    let additional_messages = if emitted_attachments.is_empty() {
        Vec::new()
    } else {
        vec![
            ToolEmittedMessage::new(
                ToolEmittedMessageRole::User,
                emitted_attachments
                    .into_iter()
                    .map(ToolEmittedContent::File)
                    .collect(),
            )
            .map_err(|error| ToolError::execution(error.to_string()))?,
        ]
    };
    Ok(ToolResult {
        display: None,
        retained_output: None,
        title: safe_title(tool_name),
        output: text_output,
        metadata,
        truncation: None,
        attachments,
        additional_messages,
    })
}

#[derive(Debug)]
enum McpAttachmentError {
    /// Malformed or undecodable content; reported inline, the call still succeeds.
    Invalid(String),
    /// Rejected by the media gate; fails the tool result like `read` does.
    Gated(String),
}

#[derive(Debug)]
struct RetainedMcpAttachment {
    attachment: ToolAttachment,
    delivery: crate::media::AttachmentGate,
}

fn route_mcp_attachment(
    retained: RetainedMcpAttachment,
    attachments: &mut Vec<ToolAttachment>,
    emitted_attachments: &mut Vec<ToolAttachment>,
) -> bool {
    if !mcp_attachment_budget_remaining(attachments, emitted_attachments) {
        return false;
    }
    match retained.delivery {
        crate::media::AttachmentGate::AttachToolResult => attachments.push(retained.attachment),
        crate::media::AttachmentGate::DeliverViaUserTurn => {
            emitted_attachments.push(retained.attachment);
        }
        crate::media::AttachmentGate::RejectUnsupportedModel
        | crate::media::AttachmentGate::RejectUnsupportedFamily
        | crate::media::AttachmentGate::RejectTooLarge { .. } => {
            unreachable!("rejected MCP attachment gates return an error")
        }
    }
    true
}

fn mcp_attachment_budget_remaining(
    attachments: &[ToolAttachment],
    emitted_attachments: &[ToolAttachment],
) -> bool {
    attachments.len().saturating_add(emitted_attachments.len()) < ToolResult::MAX_ATTACHMENTS
}

fn retain_base64_attachment(
    context: &ToolExecutionContext,
    mime_type: String,
    data: &str,
) -> Result<RetainedMcpAttachment, McpAttachmentError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|error| McpAttachmentError::Invalid(error.to_string()))?;
    let mut delivery = crate::media::AttachmentGate::AttachToolResult;
    let sniffed = crate::media::approved_media_type(std::path::Path::new(""), &bytes)
        .map_err(|error| McpAttachmentError::Invalid(error.to_string()))?;
    let retained_mime = if mime_type.is_empty() || mime_type == "application/octet-stream" {
        sniffed.unwrap_or(mime_type.as_str())
    } else {
        mime_type.as_str()
    };
    let attachment = if let Some(mime) = sniffed {
        if crate::media::canonical_video_mime_type(mime)
            != crate::media::canonical_video_mime_type(retained_mime)
        {
            return Err(McpAttachmentError::Invalid(format!(
                "attachment MIME mismatch: declared {retained_mime}, validated {mime}"
            )));
        }
        let gate = crate::media::gate_attachment(
            context.turn_context.adapter_family,
            &context.turn_context.capabilities,
            mime,
            &bytes,
        );
        if let Some(error) = crate::media::attachment_gate_error(
            gate,
            mime,
            &context.turn_context.model,
            context.turn_context.adapter,
        ) {
            return Err(McpAttachmentError::Gated(error));
        }
        delivery = gate;
        context.retain_validated_attachment(retained_mime, None, &bytes)
    } else {
        context.retain_attachment(retained_mime, None, &bytes)
    }
    .map_err(|error| McpAttachmentError::Invalid(error.to_string()))?;
    Ok(RetainedMcpAttachment {
        attachment,
        delivery,
    })
}

fn sanitize_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn safe_title(value: &str) -> SafeDisplayText {
    let mut title = String::new();
    for character in value.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if title.len() + character.len_utf8() > SafeDisplayText::MAX_BYTES {
            break;
        }
        title.push(character);
    }
    SafeDisplayText::new(title).expect("sanitized MCP title")
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[cfg(unix)]
#[path = "mcp/oauth_tests.rs"]
mod oauth_tests;

#[cfg(all(test, windows))]
mod windows_oauth_tests {
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
