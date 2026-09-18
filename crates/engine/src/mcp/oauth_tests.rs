use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cookie_agent_config::{LoadedMcpServer, McpOAuthConfig, McpServerConfig, McpServerSource};
use rmcp::transport::{CredentialStore as _, StoredCredentials};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::{Barrier, Notify},
};
use tokio_util::sync::CancellationToken;

use super::{
    McpOAuthHttpClient, McpRegistry, McpServerState, OAUTH_CALLBACK_TIMEOUT, OAUTH_STORE_FILE,
    OAuthCredentialFile, OAuthExchangeState, PersistedOAuthCredential, ServerCredentialStore,
    StrictStoredCredentials, canonical_oauth_resource_url, credential_superseded,
    oauth_credential_key, refresh_grant_token,
};

struct OAuthFixtureState {
    refreshes: AtomicUsize,
    authorizations: AtomicUsize,
    reject_access_one: AtomicBool,
    reject_all_access: AtomicBool,
    reject_refresh: AtomicBool,
    transient_refresh_failure: AtomicBool,
    rotate_refreshes: AtomicBool,
    refresh_sequence: AtomicU64,
    valid_refresh: Mutex<Option<String>>,
    refresh_gate: AtomicBool,
    refresh_seen: Notify,
    refresh_release: Notify,
    code_exchange_error: Mutex<Option<String>>,
    token_expires_in: AtomicU64,
    mcp2_bearer_requests: AtomicUsize,
    pkce: Mutex<Vec<(String, String)>>,
    code_verifiers: Mutex<Vec<String>>,
    exchanged_codes: Mutex<Vec<String>>,
}

struct OAuthFixture {
    base_url: String,
    state: Arc<OAuthFixtureState>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl OAuthFixture {
    async fn start() -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("OAuth fixture listener");
        let base_url = format!("http://{}", listener.local_addr().expect("fixture address"));
        let state = Arc::new(OAuthFixtureState {
            refreshes: AtomicUsize::new(0),
            authorizations: AtomicUsize::new(0),
            reject_access_one: AtomicBool::new(false),
            reject_all_access: AtomicBool::new(false),
            reject_refresh: AtomicBool::new(false),
            transient_refresh_failure: AtomicBool::new(false),
            rotate_refreshes: AtomicBool::new(false),
            refresh_sequence: AtomicU64::new(2),
            valid_refresh: Mutex::new(None),
            refresh_gate: AtomicBool::new(false),
            refresh_seen: Notify::new(),
            refresh_release: Notify::new(),
            code_exchange_error: Mutex::new(None),
            token_expires_in: AtomicU64::new(3600),
            mcp2_bearer_requests: AtomicUsize::new(0),
            pkce: Mutex::new(Vec::new()),
            code_verifiers: Mutex::new(Vec::new()),
            exchanged_codes: Mutex::new(Vec::new()),
        });
        let shutdown = CancellationToken::new();
        let task_state = Arc::clone(&state);
        let task_base = base_url.clone();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    accepted = listener.accept() => accepted,
                    () = task_shutdown.cancelled() => break,
                };
                let Ok((stream, _)) = accepted else {
                    break;
                };
                let state = Arc::clone(&task_state);
                let base_url = task_base.clone();
                tokio::spawn(async move {
                    let _ = handle_request(stream, &base_url, &state).await;
                });
            }
        });
        Self {
            base_url,
            state,
            shutdown,
            task,
        }
    }

    fn mcp_url(&self) -> String {
        format!("{}/mcp", self.base_url)
    }

    fn replacement_mcp_url(&self) -> String {
        format!("{}/mcp2", self.base_url)
    }

    async fn stop(self) {
        self.shutdown.cancel();
        let _ = self.task.await;
    }
}

struct HttpRequest {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<HttpRequest> {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0_u8; 2048];
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
    let mut lines = headers.lines();
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let target = request_line.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let mut buffer = vec![0_u8; header_end + content_length - bytes.len()];
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(HttpRequest {
        method,
        target,
        headers,
        body: bytes[header_end..].to_vec(),
    })
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    headers: &[(&str, String)],
    body: &str,
) -> std::io::Result<()> {
    let extra = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

async fn json_response(stream: &mut TcpStream, status: &str, value: Value) -> std::io::Result<()> {
    respond(stream, status, "application/json", &[], &value.to_string()).await
}

async fn handle_request(
    mut stream: TcpStream,
    base_url: &str,
    state: &OAuthFixtureState,
) -> std::io::Result<()> {
    let request = read_request(&mut stream).await?;
    let url = url::Url::parse(&format!("{base_url}{}", request.target))
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
    match (request.method.as_str(), url.path()) {
        ("GET", "/resource") | ("GET", "/.well-known/oauth-protected-resource/mcp") => {
            json_response(
                &mut stream,
                "200 OK",
                json!({
                    "resource": format!("{base_url}/mcp"),
                    "authorization_servers": [base_url],
                    "scopes_supported": ["mcp"]
                }),
            )
            .await
        }
        ("GET", "/resource2") | ("GET", "/.well-known/oauth-protected-resource/mcp2") => {
            json_response(
                &mut stream,
                "200 OK",
                json!({
                    "resource": format!("{base_url}/mcp2"),
                    "authorization_servers": [base_url],
                    "scopes_supported": ["mcp"]
                }),
            )
            .await
        }
        ("GET", "/.well-known/oauth-authorization-server")
        | ("GET", "/.well-known/openid-configuration") => {
            json_response(
                &mut stream,
                "200 OK",
                json!({
                    "issuer": base_url,
                    "authorization_endpoint": format!("{base_url}/authorize"),
                    "token_endpoint": format!("{base_url}/token"),
                    "registration_endpoint": format!("{base_url}/register"),
                    "response_types_supported": ["code"],
                    "grant_types_supported": ["authorization_code", "refresh_token"],
                    "code_challenge_methods_supported": ["S256"],
                    "scopes_supported": ["mcp", "offline_access"]
                }),
            )
            .await
        }
        ("POST", "/register") => {
            let registration: Value = serde_json::from_slice(&request.body).unwrap_or_default();
            json_response(
                &mut stream,
                "201 Created",
                json!({
                    "client_id": "cookie-test-client",
                    "client_name": "Cookie Agent",
                    "redirect_uris": registration["redirect_uris"]
                }),
            )
            .await
        }
        ("GET", "/authorize") => {
            state.authorizations.fetch_add(1, Ordering::SeqCst);
            let params = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
            state.pkce.lock().expect("PKCE capture").push((
                params.get("code_challenge").cloned().unwrap_or_default(),
                params
                    .get("code_challenge_method")
                    .cloned()
                    .unwrap_or_default(),
            ));
            let mut redirect =
                url::Url::parse(&params["redirect_uri"]).expect("fixture redirect URI");
            redirect
                .query_pairs_mut()
                .append_pair("code", "authorization-code")
                .append_pair("state", &params["state"])
                .append_pair("iss", base_url);
            respond(
                &mut stream,
                "302 Found",
                "text/plain",
                &[("Location", redirect.to_string())],
                "redirecting",
            )
            .await
        }
        ("POST", "/token") => {
            let body = String::from_utf8_lossy(&request.body);
            let params = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect::<BTreeMap<_, _>>();
            if params.get("grant_type").map(String::as_str) == Some("refresh_token") {
                state.refreshes.fetch_add(1, Ordering::SeqCst);
                if state.refresh_gate.load(Ordering::SeqCst) {
                    state.refresh_seen.notify_one();
                    state.refresh_release.notified().await;
                }
                if state.transient_refresh_failure.load(Ordering::SeqCst) {
                    return json_response(
                        &mut stream,
                        "500 Internal Server Error",
                        json!({"error":"server_error","error_description":"temporary"}),
                    )
                    .await;
                }
                if state.reject_refresh.load(Ordering::SeqCst) {
                    return json_response(
                        &mut stream,
                        "400 Bad Request",
                        json!({"error":"invalid_grant","error_description":"oauth-token-sentinel"}),
                    )
                    .await;
                }
                let issued_refresh = if state.rotate_refreshes.load(Ordering::SeqCst) {
                    // Single-use rotating refresh tokens: presenting a consumed
                    // token is `invalid_grant`.
                    let presented = params.get("refresh_token").cloned().unwrap_or_default();
                    let matched = {
                        let valid = state.valid_refresh.lock().expect("valid refresh");
                        valid.as_deref() == Some(presented.as_str())
                    };
                    if !matched {
                        return json_response(
                            &mut stream,
                            "400 Bad Request",
                            json!({"error":"invalid_grant","error_description":"oauth-token-sentinel"}),
                        )
                        .await;
                    }
                    let next = format!(
                        "refresh-{}",
                        state.refresh_sequence.fetch_add(1, Ordering::SeqCst)
                    );
                    *state.valid_refresh.lock().expect("valid refresh") = Some(next.clone());
                    next
                } else {
                    "refresh-1".to_owned()
                };
                return json_response(
                    &mut stream,
                    "200 OK",
                    json!({
                        "access_token": "access-2",
                        "token_type": "Bearer",
                        "expires_in": state.token_expires_in.load(Ordering::SeqCst),
                        "refresh_token": issued_refresh,
                        "scope": "mcp offline_access"
                    }),
                )
                .await;
            }
            state
                .code_verifiers
                .lock()
                .expect("verifier capture")
                .push(params.get("code_verifier").cloned().unwrap_or_default());
            state
                .exchanged_codes
                .lock()
                .expect("code capture")
                .push(params.get("code").cloned().unwrap_or_default());
            let error_body = state.code_exchange_error.lock().unwrap().clone();
            if let Some(body) = error_body {
                return respond(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    &[],
                    &body,
                )
                .await;
            }
            *state.valid_refresh.lock().expect("valid refresh") = Some("refresh-1".to_owned());
            json_response(
                &mut stream,
                "200 OK",
                json!({
                    "access_token": "access-1",
                    "token_type": "Bearer",
                    "expires_in": state.token_expires_in.load(Ordering::SeqCst),
                    "refresh_token": "refresh-1",
                    "scope": "mcp offline_access"
                }),
            )
            .await
        }
        ("POST", "/mcp") | ("POST", "/mcp2") => {
            let token = request
                .headers
                .get("authorization")
                .and_then(|value| value.strip_prefix("Bearer "));
            if url.path() == "/mcp2" && token.is_some() {
                state.mcp2_bearer_requests.fetch_add(1, Ordering::SeqCst);
            }
            let valid = !state.reject_all_access.load(Ordering::SeqCst)
                && matches!(token, Some("access-2"))
                || !state.reject_all_access.load(Ordering::SeqCst)
                    && !state.reject_access_one.load(Ordering::SeqCst)
                    && matches!(token, Some("access-1"));
            if !valid {
                return respond(
                    &mut stream,
                    "401 Unauthorized",
                    "text/plain",
                    &[(("WWW-Authenticate"), format!(
                        "Bearer resource_metadata=\"{base_url}/{}\", error=\"invalid_token\", scope=\"mcp\"",
                        if url.path() == "/mcp2" { "resource2" } else { "resource" }
                    ))],
                    "authorization required",
                )
                .await;
            }
            let message: Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            match message.get("method").and_then(Value::as_str) {
                Some("server/discover") => {
                    json_response(
                        &mut stream,
                        "200 OK",
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"legacy fixture"}}),
                    )
                    .await
                }
                Some("initialize") => {
                    json_response(
                        &mut stream,
                        "200 OK",
                        json!({
                            "jsonrpc":"2.0",
                            "id":id,
                            "result":{
                                "protocolVersion":"2025-11-25",
                                "capabilities":{"tools":{}},
                                "serverInfo":{"name":"oauth-fixture","version":"1.0.0"}
                            }
                        }),
                    )
                    .await
                }
                Some("tools/list") => {
                    json_response(
                        &mut stream,
                        "200 OK",
                        json!({"jsonrpc":"2.0","id":id,"result":{"tools":[]}}),
                    )
                    .await
                }
                _ if message.get("id").is_none() => {
                    respond(&mut stream, "202 Accepted", "text/plain", &[], "").await
                }
                _ => {
                    json_response(
                        &mut stream,
                        "200 OK",
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"unsupported"}}),
                    )
                    .await
                }
            }
        }
        ("GET", "/mcp" | "/mcp2") => {
            respond(&mut stream, "405 Method Not Allowed", "text/plain", &[], "").await
        }
        ("DELETE", "/mcp" | "/mcp2") => respond(&mut stream, "200 OK", "text/plain", &[], "").await,
        _ => respond(&mut stream, "404 Not Found", "text/plain", &[], "").await,
    }
}

fn remote_config(url: String) -> McpServerConfig {
    McpServerConfig {
        command: None,
        args: Vec::new(),
        env: BTreeMap::new(),
        cwd: None,
        url: Some(url),
        headers: BTreeMap::new(),
        oauth: Default::default(),
        enabled: true,
        lazy: true,
        timeout_ms: Some(5_000),
    }
}

fn oauth_registry(directory: &tempfile::TempDir, url: String, name: &str) -> McpRegistry {
    McpRegistry::new(
        BTreeMap::from([(
            name.to_owned(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config: remote_config(url),
            },
        )]),
        directory.path().join("mcp-oauth.json"),
    )
    .expect("OAuth registry")
}

fn project_oauth_registry(
    _project: &tempfile::TempDir,
    oauth_path: &std::path::Path,
    url: String,
    name: &str,
) -> McpRegistry {
    McpRegistry::new(
        BTreeMap::from([(
            name.to_owned(),
            LoadedMcpServer {
                source: McpServerSource::WorkspaceFile,
                config: remote_config(url),
            },
        )]),
        oauth_path.to_owned(),
    )
    .expect("project OAuth registry")
}

fn credential_store_registry(oauth_path: &std::path::Path, url: &str, name: &str) -> McpRegistry {
    McpRegistry::new(
        BTreeMap::from([(
            name.to_owned(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config: remote_config(url.to_owned()),
            },
        )]),
        oauth_path.to_owned(),
    )
    .expect("credential store registry")
}

fn test_credentials(client_id: &str) -> StoredCredentials {
    StoredCredentials::new(client_id.to_owned(), None, Vec::new(), None)
}

async fn wait_for_state(registry: &McpRegistry, expected: McpServerState) {
    wait_for_named_state(registry, &registry.statuses()[0].server, expected).await;
}

async fn wait_for_named_state(registry: &McpRegistry, server: &str, expected: McpServerState) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if registry
                .statuses()
                .into_iter()
                .any(|status| status.server == server && status.state == expected)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("MCP state transition");
}

async fn authorize(registry: &McpRegistry, server: &str) {
    let authorization_url = registry.begin_auth(server).await.expect("begin OAuth");
    let response = reqwest::Client::new()
        .get(authorization_url)
        .send()
        .await
        .expect("follow OAuth authorization redirect");
    assert!(response.status().is_success());
    wait_for_named_state(registry, server, McpServerState::Connected).await;
}

fn authorization_parameters(authorization_url: &str) -> BTreeMap<String, String> {
    url::Url::parse(authorization_url)
        .expect("authorization URL")
        .query_pairs()
        .into_owned()
        .collect()
}

async fn wait_for_auth_flow(registry: &McpRegistry, active: bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if registry.statuses()[0].auth_in_progress == active {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("OAuth flow state");
}

async fn assert_callback_port_released(redirect_uri: &str) {
    let address = url::Url::parse(redirect_uri)
        .expect("redirect URI")
        .socket_addrs(|| None)
        .expect("callback address")[0];
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match TcpListener::bind(address).await {
                Ok(listener) => break drop(listener),
                Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("callback port release");
}

#[tokio::test]
async fn oauth_challenge_callback_persistence_refresh_and_revocation() {
    let fixture = OAuthFixture::start().await;
    let directory = tempfile::tempdir().expect("project data");
    let registry = oauth_registry(&directory, fixture.mcp_url(), "remote");
    registry
        .server("remote")
        .expect("remote")
        .connect()
        .await
        .expect_err("unauthorized connection");
    assert_eq!(registry.statuses()[0].state, McpServerState::NeedsAuth);

    authorize(&registry, "remote").await;
    let (challenge, method) = fixture.state.pkce.lock().expect("PKCE")[0].clone();
    let verifier = fixture.state.code_verifiers.lock().expect("verifier")[0].clone();
    assert_eq!(method, "S256");
    assert_eq!(
        challenge,
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    );
    assert_eq!(
        fixture.state.exchanged_codes.lock().expect("codes")[0],
        "authorization-code"
    );
    assert_eq!(fixture.state.authorizations.load(Ordering::SeqCst), 1);
    let credential_path = directory.path().join("mcp-oauth.json");
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(&credential_path)
            .expect("OAuth credentials")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    registry.shutdown().await;

    let restarted = oauth_registry(&directory, fixture.mcp_url(), "remote");
    restarted
        .server("remote")
        .expect("remote")
        .connect()
        .await
        .expect("stored token reconnect");
    assert_eq!(fixture.state.authorizations.load(Ordering::SeqCst), 1);

    fixture
        .state
        .reject_access_one
        .store(true, Ordering::SeqCst);
    restarted
        .reconnect_server("remote")
        .await
        .expect("refresh reconnect");
    assert_eq!(restarted.statuses()[0].state, McpServerState::Connected);
    assert_eq!(fixture.state.refreshes.load(Ordering::SeqCst), 1);
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(&credential_path)
            .expect("rewritten OAuth credentials")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(directory.path().join("mcp-oauth.lock"))
            .expect("OAuth credential lock")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    fixture
        .state
        .reject_all_access
        .store(true, Ordering::SeqCst);
    fixture.state.reject_refresh.store(true, Ordering::SeqCst);
    restarted
        .reconnect_server("remote")
        .await
        .expect_err("revoked credentials require authorization");
    let status = restarted.statuses().remove(0);
    assert_eq!(status.state, McpServerState::NeedsAuth);
    assert!(
        status
            .message
            .unwrap_or_default()
            .contains("oauth-token-sentinel")
    );
    restarted
        .remove_server("remote")
        .await
        .expect("remove OAuth server");
    let stored: Value = serde_json::from_slice(
        &std::fs::read(&credential_path).expect("credential store after removal"),
    )
    .expect("strict credential JSON");
    assert!(stored.as_object().is_some_and(serde_json::Map::is_empty));
    restarted.shutdown().await;
    fixture.stop().await;
}

#[tokio::test]
async fn user_credentials_are_shared_across_projects() {
    let fixture = OAuthFixture::start().await;
    let user_data = tempfile::tempdir().expect("user data");
    let oauth_path = user_data.path().join("mcp-oauth.json");
    let first_project = tempfile::tempdir().expect("first project");
    let first = project_oauth_registry(&first_project, &oauth_path, fixture.mcp_url(), "remote");
    assert_eq!(first.statuses()[0].state, McpServerState::Disconnected);
    first
        .server("remote")
        .expect("remote")
        .connect()
        .await
        .expect_err("authorization challenge");
    authorize(&first, "remote").await;
    first.shutdown().await;

    let second_project = tempfile::tempdir().expect("second project");
    let second = project_oauth_registry(&second_project, &oauth_path, fixture.mcp_url(), "remote");
    assert_eq!(second.statuses()[0].state, McpServerState::Disconnected);
    second
        .server("remote")
        .expect("remote")
        .connect()
        .await
        .expect("shared user credential");
    assert_eq!(fixture.state.authorizations.load(Ordering::SeqCst), 1);
    second.shutdown().await;
    fixture.stop().await;
}

#[tokio::test]
async fn same_name_different_endpoint_never_presents_stored_token() {
    let fixture = OAuthFixture::start().await;
    let user_data = tempfile::tempdir().expect("user data");
    let oauth_path = user_data.path().join("mcp-oauth.json");
    let first_project = tempfile::tempdir().expect("first project");
    let first = project_oauth_registry(&first_project, &oauth_path, fixture.mcp_url(), "remote");
    first
        .server("remote")
        .expect("remote")
        .connect()
        .await
        .expect_err("authorization challenge");
    authorize(&first, "remote").await;
    first.shutdown().await;

    let second_project = tempfile::tempdir().expect("second project");
    let second = project_oauth_registry(
        &second_project,
        &oauth_path,
        fixture.replacement_mcp_url(),
        "remote",
    );
    second
        .server("remote")
        .expect("remote")
        .connect()
        .await
        .expect_err("different endpoint requires authorization");
    assert_eq!(second.statuses()[0].state, McpServerState::NeedsAuth);
    assert_eq!(fixture.state.mcp2_bearer_requests.load(Ordering::SeqCst), 0);
    second.shutdown().await;
    fixture.stop().await;
}

#[test]
fn oauth_credential_keys_use_minimal_fail_safe_url_canonicalization() {
    let canonical = canonical_oauth_resource_url("HTTPS://Example.COM:443/a/../mcp/");
    assert_eq!(canonical, "https://example.com/mcp/");
    assert_eq!(
        canonical,
        canonical_oauth_resource_url("https://example.com/mcp/")
    );
    assert_eq!(
        oauth_credential_key("remote", &canonical),
        oauth_credential_key(
            "remote",
            &canonical_oauth_resource_url("https://EXAMPLE.com:443/mcp/")
        )
    );
    assert_eq!(
        canonical_oauth_resource_url("http://Example.com:80/mcp/"),
        "http://example.com/mcp/"
    );

    let key = |url: &str| oauth_credential_key("remote", &canonical_oauth_resource_url(url));
    assert_ne!(
        key("https://example.com/mcp"),
        key("https://example.com/mcp/")
    );
    assert_ne!(
        key("https://example.com/mcp?tenant=one"),
        key("https://example.com/mcp/?tenant=one")
    );
    assert_ne!(
        key("https://example.com/mcp"),
        key("http://example.com/mcp")
    );
    assert_ne!(
        key("https://example.com/mcp"),
        key("https://other.example.com/mcp")
    );
    assert_ne!(
        key("https://example.com/mcp"),
        key("https://example.com:444/mcp")
    );
    assert_ne!(
        key("https://example.com/mcp"),
        key("https://example.com/mcp/subpath")
    );
    assert_ne!(
        key("https://example.com/mcp?tenant=one"),
        key("https://example.com/mcp?tenant=two")
    );
    assert_ne!(
        key("https://example.com/mcp?a=1&b=2"),
        key("https://example.com/mcp?b=2&a=1")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_engine_stores_merge_distinct_credentials() {
    let user_data = tempfile::tempdir().expect("user data");
    let oauth_path = user_data.path().join("mcp-oauth.json");
    let first = credential_store_registry(&oauth_path, "https://one.example/mcp", "one");
    let second = credential_store_registry(&oauth_path, "https://two.example/mcp", "two");
    let first_store = first
        .server("one")
        .expect("one")
        .oauth_store()
        .expect("store");
    let second_store = second
        .server("two")
        .expect("two")
        .oauth_store()
        .expect("store");
    let barrier = Arc::new(Barrier::new(2));
    let first_barrier = Arc::clone(&barrier);
    let first_write = tokio::spawn(async move {
        first_barrier.wait().await;
        first_store
            .save(test_credentials("client-one"))
            .await
            .expect("first credential write");
    });
    let second_write = tokio::spawn(async move {
        barrier.wait().await;
        second_store
            .save(test_credentials("client-two"))
            .await
            .expect("second credential write");
    });
    let (first_result, second_result) = tokio::join!(first_write, second_write);
    first_result.expect("first writer");
    second_result.expect("second writer");

    let stored: Value = serde_json::from_slice(&std::fs::read(oauth_path).expect("credentials"))
        .expect("credential JSON");
    assert_eq!(stored.as_object().expect("credential map").len(), 2);
}

#[tokio::test]
async fn stale_engine_snapshot_cannot_overwrite_a_new_credential() {
    let user_data = tempfile::tempdir().expect("user data");
    let oauth_path = user_data.path().join("mcp-oauth.json");
    let first = credential_store_registry(&oauth_path, "https://one.example/mcp", "one");
    let stale = credential_store_registry(&oauth_path, "https://two.example/mcp", "two");
    let first_store = first
        .server("one")
        .expect("one")
        .oauth_store()
        .expect("store");
    let stale_store = stale
        .server("two")
        .expect("two")
        .oauth_store()
        .expect("store");

    first_store
        .save(test_credentials("client-one"))
        .await
        .expect("first credential write");
    stale_store
        .save(test_credentials("client-two"))
        .await
        .expect("stale instance credential write");

    let stored: Value = serde_json::from_slice(&std::fs::read(oauth_path).expect("credentials"))
        .expect("credential JSON");
    assert_eq!(stored.as_object().expect("credential map").len(), 2);
}

#[tokio::test]
async fn oauth_callback_timeout_returns_to_needs_auth() {
    let fixture = OAuthFixture::start().await;
    let directory = tempfile::tempdir().expect("project data");
    let registry = oauth_registry(&directory, fixture.mcp_url(), "remote");
    registry
        .server("remote")
        .expect("remote")
        .connect()
        .await
        .expect_err("unauthorized connection");
    registry.begin_auth("remote").await.expect("begin OAuth");
    tokio::time::sleep(OAUTH_CALLBACK_TIMEOUT + Duration::from_millis(50)).await;
    wait_for_state(&registry, McpServerState::NeedsAuth).await;
    assert!(
        registry.statuses()[0]
            .message
            .as_deref()
            .is_some_and(|message| message.contains("timed out"))
    );
    assert!(!registry.statuses()[0].auth_in_progress);
    registry
        .begin_auth("remote")
        .await
        .expect("fresh flow after timeout");
    registry.cancel_auth("remote").expect("cancel retry");
    registry.shutdown().await;
    fixture.stop().await;
}

#[tokio::test]
async fn state_mismatch_is_rejected_and_cancel_releases_callback_port() {
    let fixture = OAuthFixture::start().await;
    let directory = tempfile::tempdir().expect("project data");
    let registry = oauth_registry(&directory, fixture.mcp_url(), "remote");
    registry
        .server("remote")
        .expect("remote")
        .connect()
        .await
        .expect_err("authorization challenge");

    let authorization_url = registry.begin_auth("remote").await.expect("begin OAuth");
    let parameters = authorization_parameters(&authorization_url);
    let redirect_uri = parameters["redirect_uri"].clone();
    let mut callback = url::Url::parse(&redirect_uri).expect("callback URL");
    callback
        .query_pairs_mut()
        .append_pair("code", "authorization-code")
        .append_pair("state", "wrong-state")
        .append_pair("iss", &fixture.base_url);
    let response = reqwest::get(callback)
        .await
        .expect("state mismatch callback");
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    wait_for_auth_flow(&registry, false).await;
    assert!(
        fixture
            .state
            .exchanged_codes
            .lock()
            .expect("codes")
            .is_empty()
    );

    let authorization_url = registry.begin_auth("remote").await.expect("retry OAuth");
    let redirect_uri = authorization_parameters(&authorization_url)["redirect_uri"].clone();
    registry.cancel_auth("remote").expect("cancel OAuth");
    wait_for_auth_flow(&registry, false).await;
    assert_callback_port_released(&redirect_uri).await;
    assert!(
        registry.statuses()[0]
            .message
            .as_deref()
            .is_some_and(|message| message.contains("cancelled"))
    );
    let retry_url = registry.begin_auth("remote").await.expect("fresh flow");
    assert_ne!(authorization_url, retry_url);
    registry.cancel_auth("remote").expect("cancel retry");
    registry.shutdown().await;
    fixture.stop().await;
}

#[tokio::test]
async fn concurrent_servers_authorize_independently() {
    let fixture = OAuthFixture::start().await;
    let directory = tempfile::tempdir().expect("project data");
    let registry = McpRegistry::new(
        BTreeMap::from([
            (
                "one".into(),
                LoadedMcpServer {
                    source: McpServerSource::UserFile,
                    config: remote_config(fixture.mcp_url()),
                },
            ),
            (
                "two".into(),
                LoadedMcpServer {
                    source: McpServerSource::UserFile,
                    config: remote_config(fixture.mcp_url()),
                },
            ),
        ]),
        directory.path().join("mcp-oauth.json"),
    )
    .expect("concurrent registry");
    let one = registry.server("one").expect("one");
    let two = registry.server("two").expect("two");
    let (one_result, two_result) = tokio::join!(one.connect(), two.connect());
    one_result.expect_err("one challenge");
    two_result.expect_err("two challenge");
    let ((), ()) = tokio::join!(authorize(&registry, "one"), authorize(&registry, "two"));
    assert!(
        registry
            .statuses()
            .iter()
            .all(|status| status.state == McpServerState::Connected)
    );
    let stored: Value = serde_json::from_slice(
        &std::fs::read(directory.path().join("mcp-oauth.json")).expect("credentials"),
    )
    .expect("credential JSON");
    assert_eq!(stored.as_object().expect("credential map").len(), 2);
    registry.shutdown().await;
    fixture.stop().await;
}

#[tokio::test]
async fn static_authorization_header_precedes_oauth_and_disabled_oauth_stays_failed() {
    let fixture = OAuthFixture::start().await;
    let static_directory = tempfile::tempdir().expect("static project data");
    let mut static_config = remote_config(fixture.mcp_url());
    static_config
        .headers
        .insert("Authorization".into(), "Bearer access-1".into());
    let static_registry = McpRegistry::new(
        BTreeMap::from([(
            "static".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config: static_config,
            },
        )]),
        static_directory.path().join("mcp-oauth.json"),
    )
    .expect("static registry");
    static_registry
        .server("static")
        .expect("static server")
        .connect()
        .await
        .expect("static authorization connection");
    assert_eq!(
        static_registry.statuses()[0].state,
        McpServerState::Connected
    );
    assert_eq!(fixture.state.authorizations.load(Ordering::SeqCst), 0);
    static_registry.shutdown().await;

    let disabled_directory = tempfile::tempdir().expect("disabled project data");
    let mut disabled_config = remote_config(fixture.mcp_url());
    disabled_config.oauth = McpOAuthConfig::Disabled;
    let disabled = McpRegistry::new(
        BTreeMap::from([(
            "disabled".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config: disabled_config,
            },
        )]),
        disabled_directory.path().join("mcp-oauth.json"),
    )
    .expect("disabled OAuth registry");
    disabled
        .reconnect_server("disabled")
        .await
        .expect_err("disabled OAuth leaves 401 as connection failure");
    assert_eq!(disabled.statuses()[0].state, McpServerState::Failed);
    disabled.shutdown().await;
    fixture.stop().await;
}

#[tokio::test]
async fn transient_refresh_failure_returns_to_needs_auth() {
    let fixture = OAuthFixture::start().await;
    fixture.state.token_expires_in.store(1, Ordering::SeqCst);
    let directory = tempfile::tempdir().expect("project data");
    let registry = oauth_registry(&directory, fixture.mcp_url(), "remote");
    registry
        .server("remote")
        .expect("remote")
        .connect()
        .await
        .expect_err("authorization challenge");
    authorize(&registry, "remote").await;
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    fixture
        .state
        .transient_refresh_failure
        .store(true, Ordering::SeqCst);
    let error = registry
        .reconnect_server("remote")
        .await
        .expect_err("temporary refresh failure requires fresh authorization");
    let status = registry.statuses().remove(0);
    assert_eq!(
        status.state,
        McpServerState::NeedsAuth,
        "error={error}; message={:?}",
        status.message
    );
    assert!(fixture.state.refreshes.load(Ordering::SeqCst) >= 1);
    registry.shutdown().await;
    fixture.stop().await;
}

#[tokio::test]
async fn endpoint_replacement_invalidates_before_any_bearer_request() {
    let fixture = OAuthFixture::start().await;
    let directory = tempfile::tempdir().expect("project data");
    let registry = oauth_registry(&directory, fixture.mcp_url(), "remote");
    registry
        .server("remote")
        .expect("remote")
        .connect()
        .await
        .expect_err("authorization challenge");
    authorize(&registry, "remote").await;

    registry
        .upsert_server(
            "remote".into(),
            LoadedMcpServer {
                source: McpServerSource::Runtime,
                config: remote_config(fixture.replacement_mcp_url()),
            },
        )
        .await
        .expect("replace endpoint");
    let status = registry.statuses().remove(0);
    assert_eq!(status.state, McpServerState::NeedsAuth);
    assert!(!status.auth_in_progress);
    assert_eq!(fixture.state.mcp2_bearer_requests.load(Ordering::SeqCst), 0);
    let stored: Value = serde_json::from_slice(
        &std::fs::read(directory.path().join("mcp-oauth.json")).expect("credentials"),
    )
    .expect("credential JSON");
    assert!(stored.as_object().is_some_and(serde_json::Map::is_empty));
    registry.shutdown().await;
    fixture.stop().await;
}

#[tokio::test]
async fn shutdown_and_supersede_release_inflight_callback_listeners() {
    let fixture = OAuthFixture::start().await;
    let shutdown_directory = tempfile::tempdir().expect("shutdown project");
    let shutdown_registry = oauth_registry(&shutdown_directory, fixture.mcp_url(), "remote");
    shutdown_registry
        .server("remote")
        .expect("remote")
        .connect()
        .await
        .expect_err("authorization challenge");
    let url = shutdown_registry
        .begin_auth("remote")
        .await
        .expect("shutdown flow");
    let redirect = authorization_parameters(&url)["redirect_uri"].clone();
    shutdown_registry.shutdown().await;
    assert_callback_port_released(&redirect).await;

    let supersede_directory = tempfile::tempdir().expect("supersede project");
    let supersede_registry = oauth_registry(&supersede_directory, fixture.mcp_url(), "remote");
    supersede_registry
        .server("remote")
        .expect("remote")
        .connect()
        .await
        .expect_err("authorization challenge");
    let url = supersede_registry
        .begin_auth("remote")
        .await
        .expect("superseded flow");
    let redirect = authorization_parameters(&url)["redirect_uri"].clone();
    supersede_registry
        .upsert_server(
            "remote".into(),
            LoadedMcpServer {
                source: McpServerSource::Runtime,
                config: remote_config(fixture.mcp_url()),
            },
        )
        .await
        .expect("supersede server");
    assert_callback_port_released(&redirect).await;
    supersede_registry.shutdown().await;
    fixture.stop().await;
}

#[tokio::test]
async fn eager_readiness_finishes_at_needs_auth_without_waiting_for_browser_flow() {
    let fixture = OAuthFixture::start().await;
    let directory = tempfile::tempdir().expect("project data");
    let mut config = remote_config(fixture.mcp_url());
    config.lazy = false;
    let registry = McpRegistry::new(
        BTreeMap::from([(
            "remote".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config,
            },
        )]),
        directory.path().join("mcp-oauth.json"),
    )
    .expect("eager registry");
    registry.start_eager(&tokio::runtime::Handle::current());
    tokio::time::timeout(Duration::from_secs(1), registry.await_eager_ready())
        .await
        .expect("eager authorization challenge is ready");
    let status = registry.statuses().remove(0);
    assert_eq!(status.state, McpServerState::NeedsAuth);
    assert!(!status.auth_in_progress);
    registry.shutdown().await;
    fixture.stop().await;
}

#[tokio::test]
async fn token_exchange_errors_preserve_bounded_response_bodies() {
    for body in [
        json!({"error":"custom_rejection","error_description":"oauth-token-sentinel", "password":"visible-password"}).to_string(),
        format!("oauth-token-sentinel visible-password\u{1b}\u{202e}\n{}", "é".repeat(4096)),
    ] {
    let fixture = OAuthFixture::start().await;
    *fixture.state.code_exchange_error.lock().unwrap() = Some(body.clone());
    let directory = tempfile::tempdir().expect("project data");
    let registry = oauth_registry(&directory, fixture.mcp_url(), "remote");
    registry
        .server("remote")
        .expect("remote")
        .connect()
        .await
        .expect_err("unauthorized connection");
    let authorization_url = registry.begin_auth("remote").await.expect("begin OAuth");
    let response = reqwest::Client::new()
        .get(authorization_url)
        .send()
        .await
        .expect("OAuth callback response");
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    wait_for_state(&registry, McpServerState::NeedsAuth).await;
    wait_for_auth_flow(&registry, false).await;
    let message = registry.statuses()[0].message.clone().unwrap_or_default();
    assert!(message.contains("authorization failed"));
    assert!(message.contains("oauth-token-sentinel"), "{message}");
    assert!(message.contains("visible-password"), "{message}");
    assert!(message.contains("HTTP 400"), "{message}");
    assert!(!message.contains(['\u{1b}', '\u{202e}']));
    assert!(message.len() <= cookie_agent_protocol::DiagnosticText::MAX_BYTES);
    if body.len() > 4096 { assert!(message.ends_with("[diagnostic truncated]")); }
    registry.shutdown().await;
    fixture.stop().await;
    }
}

#[tokio::test]
async fn oauth_callback_denial_preserves_error_description() {
    let fixture = OAuthFixture::start().await;
    let directory = tempfile::tempdir().unwrap();
    let registry = oauth_registry(&directory, fixture.mcp_url(), "remote");
    registry
        .server("remote")
        .unwrap()
        .connect()
        .await
        .expect_err("authorization required");
    let authorization_url = registry.begin_auth("remote").await.unwrap();
    let params = authorization_parameters(&authorization_url);
    let mut callback = url::Url::parse(&params["redirect_uri"]).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("error", "access_denied")
        .append_pair(
            "error_description",
            "password=visible; user declined\u{1b}\u{202e}",
        );
    let response = reqwest::get(callback).await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    wait_for_auth_flow(&registry, false).await;
    let message = registry.statuses()[0].message.clone().unwrap();
    assert!(message.contains("access_denied"), "{message}");
    assert!(
        message.contains("password=visible; user declined"),
        "{message}"
    );
    assert!(!message.contains(['\u{1b}', '\u{202e}']));
    registry.shutdown().await;
    fixture.stop().await;
}

#[test]
fn malformed_oauth_store_is_strict_and_redacted() {
    let directory = tempfile::tempdir().expect("project data");
    let path = directory.path().join("mcp-oauth.json");
    std::fs::write(
        &path,
        r#"{
            "remote": {
                "binding": {
                    "resource_url": "https://example.test/mcp",
                    "configured_client_id": null,
                    "client_metadata_url": null,
                    "client_secret_sha256": null,
                    "scopes": []
                },
                "credentials": {
                    "client_id": "client",
                    "token_response": null,
                    "granted_scopes": [],
                    "token_received_at": null,
                    "issuer": null,
                    "unknown": "oauth-token-sentinel"
                }
            }
        }"#,
    )
    .expect("unknown-field OAuth store");
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("private OAuth store");
    let error = McpRegistry::new(BTreeMap::new(), path.clone())
        .expect_err("unknown OAuth credential fields must fail startup")
        .to_string();
    assert!(error.contains(&path.display().to_string()));
    assert!(error.contains("remove the file"));
    assert!(!error.contains("oauth-token-sentinel"));
}

// --- D6: lock-free credential reads -----------------------------------------

#[cfg(unix)]
const CHILD_LOCK_ENV: &str = "COOKIE_AGENT_OAUTH_LOCK_CHILD";

fn token_response_json(access: &str, refresh: &str) -> rmcp::transport::auth::OAuthTokenResponse {
    serde_json::from_value(json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": 3600,
        "refresh_token": refresh,
        "scope": "mcp offline_access"
    }))
    .expect("OAuth token response")
}

fn strict_credentials(access: &str, refresh: &str, received_at: u64) -> StrictStoredCredentials {
    StrictStoredCredentials {
        client_id: "cookie-test-client".to_owned(),
        token_response: Some(token_response_json(access, refresh)),
        granted_scopes: vec!["mcp".to_owned()],
        token_received_at: Some(received_at),
        issuer: Some("issuer".to_owned()),
    }
}

fn stored_credentials(access: &str, refresh: &str, received_at: u64) -> StoredCredentials {
    StoredCredentials::new(
        "cookie-test-client".to_owned(),
        Some(token_response_json(access, refresh)),
        vec!["mcp".to_owned()],
        Some(received_at),
    )
    .with_issuer(Some("issuer".to_owned()))
}

fn token_field(
    response: &rmcp::transport::auth::OAuthTokenResponse,
    field: &str,
) -> Option<String> {
    serde_json::to_value(response)
        .ok()?
        .get(field)?
        .as_str()
        .map(str::to_owned)
}

fn oauth_binding(url: &str) -> super::OAuthCredentialBinding {
    super::OAuthCredentialBinding::from_config(&remote_config(url.to_owned())).expect("binding")
}

fn seed_credential(
    path: &std::path::Path,
    server: &str,
    url: &str,
    access: &str,
    refresh: &str,
    received_at: u64,
) -> OAuthCredentialFile {
    let file = OAuthCredentialFile::open(path.to_owned()).expect("OAuth file");
    let binding = oauth_binding(url);
    let key = oauth_credential_key(server, &binding.resource_url);
    file.update(|all| {
        all.insert(
            key,
            PersistedOAuthCredential {
                binding,
                credentials: strict_credentials(access, refresh, received_at),
            },
        );
    })
    .expect("seed credential");
    file
}

async fn refresh_via_http_client(
    url: String,
    store: ServerCredentialStore,
    stale: StoredCredentials,
) -> Result<rmcp::transport::auth::OAuthTokenResponse, rmcp::transport::AuthError> {
    let exchange = OAuthExchangeState::default();
    let http_client = McpOAuthHttpClient::new(exchange, store).expect("OAuth HTTP client");
    let mut manager = rmcp::transport::AuthorizationManager::new_with_oauth_http_client(
        url,
        Arc::new(http_client),
    )
    .await
    .expect("authorization manager");
    let resolution = manager.resolve_metadata().await.expect("metadata");
    manager.set_metadata(resolution.metadata);
    manager
        .configure_client(rmcp::transport::auth::OAuthClientConfig::new(
            "cookie-test-client".to_owned(),
            "http://127.0.0.1".to_owned(),
        ))
        .expect("configure client");
    let credentials = rmcp::transport::auth::InMemoryCredentialStore::new();
    credentials
        .save(stale)
        .await
        .expect("seed stale credentials");
    manager.set_credential_store(credentials);
    manager.refresh_token().await
}

#[test]
fn refresh_grant_sniffing_matches_rmcp_refresh_shape() {
    // Locks the wire shape rmcp's refresh uses so a future rmcp that stops
    // routing refreshes through our client fails loudly instead of silently
    // bypassing the claim check.
    assert_eq!(
        refresh_grant_token(b"grant_type=refresh_token&refresh_token=abc&resource=x"),
        Some("abc".to_owned())
    );
    assert_eq!(
        refresh_grant_token(b"grant_type=authorization_code&code=abc"),
        None
    );
    assert_eq!(refresh_grant_token(b""), None);
}

#[test]
fn cas_predicate_matches_spec() {
    let existing = strict_credentials("a", "r1", 100);
    assert!(credential_superseded(
        &existing,
        &strict_credentials("b", "r2", 90)
    ));
    assert!(credential_superseded(
        &existing,
        &strict_credentials("b", "r2", 100)
    ));
    assert!(!credential_superseded(
        &existing,
        &strict_credentials("b", "r1", 100)
    ));
    assert!(!credential_superseded(
        &existing,
        &strict_credentials("b", "r2", 200)
    ));
    let downgrade = StrictStoredCredentials {
        token_response: None,
        ..strict_credentials("b", "r2", 50)
    };
    assert!(!credential_superseded(&existing, &downgrade));
}

#[tokio::test]
async fn credential_save_cas_skips_superseded_and_lands_fresh() {
    let directory = tempfile::tempdir().expect("profile data");
    let path = directory.path().join(OAUTH_STORE_FILE);
    let url = "https://example.test/mcp";
    let file = OAuthCredentialFile::open(path).expect("OAuth file");
    let binding = oauth_binding(url);
    let key = oauth_credential_key("remote", &binding.resource_url);
    file.update(|all| {
        all.insert(
            key.clone(),
            PersistedOAuthCredential {
                binding: binding.clone(),
                credentials: strict_credentials("access-1", "refresh-1", 100),
            },
        );
    })
    .expect("seed disk");
    let store = file.scoped("remote", binding);

    // Fresh (newer) write lands.
    store
        .save(stored_credentials("access-2", "refresh-2", 200))
        .await
        .expect("fresh save");
    let disk = file.get(&key).expect("read").expect("entry");
    assert_eq!(
        token_field(
            disk.credentials.token_response.as_ref().unwrap(),
            "access_token"
        ),
        Some("access-2".to_owned())
    );

    // Older write is skipped and reports success; disk is unchanged.
    store
        .save(stored_credentials("access-stale", "refresh-stale", 150))
        .await
        .expect("stale save returns Ok");
    let disk = file.get(&key).expect("read").expect("entry");
    assert_eq!(
        token_field(
            disk.credentials.token_response.as_ref().unwrap(),
            "access_token"
        ),
        Some("access-2".to_owned())
    );

    // Same timestamp, different refresh token: skipped.
    store
        .save(stored_credentials("access-rotated", "refresh-rotated", 200))
        .await
        .expect("rotation save returns Ok");
    let disk = file.get(&key).expect("read").expect("entry");
    assert_eq!(
        token_field(
            disk.credentials.token_response.as_ref().unwrap(),
            "access_token"
        ),
        Some("access-2".to_owned())
    );

    // Same timestamp, same refresh token: idempotent write lands.
    store
        .save(stored_credentials("access-2b", "refresh-2", 200))
        .await
        .expect("idempotent save");
    let disk = file.get(&key).expect("read").expect("entry");
    assert_eq!(
        token_field(
            disk.credentials.token_response.as_ref().unwrap(),
            "access_token"
        ),
        Some("access-2b".to_owned())
    );

    // `token_response: None` administrative downgrade is allowed.
    store
        .save(StoredCredentials::new(
            "cookie-test-client".to_owned(),
            None,
            Vec::new(),
            Some(400),
        ))
        .await
        .expect("downgrade save");
    let disk = file.get(&key).expect("read").expect("entry");
    assert!(disk.credentials.token_response.is_none());
}

#[tokio::test]
async fn refresh_claim_adopts_rotated_disk_credential_without_idp_hit() {
    let fixture = OAuthFixture::start().await;
    fixture.state.rotate_refreshes.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().expect("profile data");
    let path = directory.path().join(OAUTH_STORE_FILE);
    let url = fixture.mcp_url();
    // A sibling process already rotated disk to these credentials.
    let file = seed_credential(
        &path,
        "remote",
        &url,
        "access-rotated",
        "refresh-rotated",
        200,
    );
    *fixture.state.valid_refresh.lock().unwrap() = Some("refresh-rotated".to_owned());
    let store = file.scoped("remote", oauth_binding(&url));
    let result = refresh_via_http_client(
        url,
        store,
        stored_credentials("access-old", "refresh-old", 100),
    )
    .await
    .expect("adopted disk credentials");
    assert_eq!(
        token_field(&result, "access_token").as_deref(),
        Some("access-rotated")
    );
    assert_eq!(
        fixture.state.refreshes.load(Ordering::SeqCst),
        0,
        "pre-flight claim must avoid the IdP call"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn refresh_claim_recovers_from_invalid_grant_by_adopting_disk() {
    let fixture = OAuthFixture::start().await;
    fixture.state.reject_refresh.store(true, Ordering::SeqCst);
    fixture.state.refresh_gate.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().expect("profile data");
    let path = directory.path().join(OAUTH_STORE_FILE);
    let url = fixture.mcp_url();
    let file = seed_credential(&path, "remote", &url, "access-old", "refresh-old", 100);
    let store = file.scoped("remote", oauth_binding(&url));
    let rotate_file = file.clone();
    let rotate_binding = oauth_binding(&url);
    let rotate_key = oauth_credential_key("remote", &rotate_binding.resource_url);
    let refresh_url = url.clone();
    let task = tokio::spawn(async move {
        refresh_via_http_client(
            refresh_url,
            store,
            stored_credentials("access-old", "refresh-old", 100),
        )
        .await
    });

    // The IdP has the request in hand; a sibling lands a rotation before the
    // IdP rejects the stale token.
    fixture.state.refresh_seen.notified().await;
    rotate_file
        .update(|all| {
            all.insert(
                rotate_key,
                PersistedOAuthCredential {
                    binding: rotate_binding,
                    credentials: strict_credentials("access-new", "refresh-new", 200),
                },
            );
        })
        .expect("rotate disk");
    fixture.state.refresh_release.notify_one();
    let result = task
        .await
        .expect("refresh task")
        .expect("invalid_grant recovered by disk adoption");
    assert_eq!(
        token_field(&result, "access_token").as_deref(),
        Some("access-new")
    );
    assert_eq!(fixture.state.refreshes.load(Ordering::SeqCst), 1);
    fixture.stop().await;
}

#[tokio::test]
async fn refresh_claim_read_error_falls_back_to_live_idp_call() {
    let fixture = OAuthFixture::start().await;
    fixture.state.rotate_refreshes.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().expect("profile data");
    let path = directory.path().join(OAUTH_STORE_FILE);
    let url = fixture.mcp_url();
    let file = seed_credential(&path, "remote", &url, "access-old", "refresh-old", 100);
    // Corrupt the store so the pre-flight claim read errors; the live refresh
    // must still proceed (the optimization is never a failure mode).
    std::fs::write(&path, b"{ not valid json").expect("corrupt store");
    *fixture.state.valid_refresh.lock().unwrap() = Some("refresh-old".to_owned());
    let store = file.scoped("remote", oauth_binding(&url));
    let result = refresh_via_http_client(
        url,
        store,
        stored_credentials("access-old", "refresh-old", 100),
    )
    .await
    .expect("live refresh");
    assert_eq!(fixture.state.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        token_field(&result, "access_token").as_deref(),
        Some("access-2")
    );
    fixture.stop().await;
}

#[cfg(unix)]
#[test]
fn credential_read_is_lock_free_while_lock_held_by_child_process() {
    let Some(path) = std::env::var_os(CHILD_LOCK_ENV) else {
        run_lock_free_read_parent();
        return;
    };
    // Child: hold the OAuth store's exclusive lock, announce readiness, and
    // wait for the parent to release us.
    use std::io::Write as _;
    let path = std::path::PathBuf::from(path);
    let lock = super::OAuthStoreLock::acquire(&path).expect("child acquires store lock");
    println!("OAUTH-LOCK-CHILD-HOLDING");
    std::io::stdout().flush().expect("flush marker");
    let mut line = String::new();
    let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line);
    drop(lock);
}

#[cfg(unix)]
fn run_lock_free_read_parent() {
    use std::{
        io::{BufRead as _, BufReader, Write as _},
        process::{Command, Stdio},
        time::Instant,
    };

    let directory = tempfile::tempdir().expect("profile data");
    let path = directory.path().join(OAUTH_STORE_FILE);
    let url = "https://example.test/mcp";
    let binding = oauth_binding(url);
    let key = oauth_credential_key("remote", &binding.resource_url);
    let file = OAuthCredentialFile::open(path.clone()).expect("OAuth file");
    file.update(|all| {
        all.insert(
            key.clone(),
            PersistedOAuthCredential {
                binding: binding.clone(),
                credentials: strict_credentials("access-1", "refresh-1", 100),
            },
        );
    })
    .expect("seed disk");

    let exe = std::env::current_exe().expect("test executable");
    let mut child = Command::new(exe)
        .args([
            "--exact",
            "mcp::oauth_tests::credential_read_is_lock_free_while_lock_held_by_child_process",
            "--nocapture",
        ])
        .env(CHILD_LOCK_ENV, &path)
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn child process");
    let stdout = child.stdout.take().expect("child stdout");
    let mut lines = BufReader::new(stdout).lines();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "child never reported holding the lock"
        );
        match lines.next() {
            Some(Ok(line)) if line.contains("OAUTH-LOCK-CHILD-HOLDING") => break,
            Some(Ok(_)) => {}
            Some(Err(error)) => panic!("child stdout error: {error}"),
            None => panic!("child exited before holding the lock"),
        }
    }

    // The child holds the exclusive store lock. A lock-taking read would block
    // for the 5 s budget; the D6 read must complete promptly and observe the
    // seeded credential.
    let started = Instant::now();
    let stored = file.get(&key).expect("lock-free read").expect("credential");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "read waited on the child's lock"
    );
    assert_eq!(
        stored.credentials.refresh_token().as_deref(),
        Some("refresh-1")
    );

    child
        .stdin
        .as_mut()
        .expect("child stdin")
        .write_all(b"release\n")
        .expect("release child");
    let status = child.wait().expect("wait for child");
    assert!(status.success());
}
