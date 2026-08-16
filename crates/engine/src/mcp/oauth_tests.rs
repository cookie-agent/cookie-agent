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
    sync::Barrier,
};
use tokio_util::sync::CancellationToken;

use super::{
    McpRegistry, McpServerState, OAUTH_CALLBACK_TIMEOUT, canonical_oauth_resource_url,
    oauth_credential_key,
};

struct OAuthFixtureState {
    refreshes: AtomicUsize,
    authorizations: AtomicUsize,
    reject_access_one: AtomicBool,
    reject_all_access: AtomicBool,
    reject_refresh: AtomicBool,
    transient_refresh_failure: AtomicBool,
    reject_code_exchange: AtomicBool,
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
            reject_code_exchange: AtomicBool::new(false),
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
                return json_response(
                    &mut stream,
                    "200 OK",
                    json!({
                        "access_token": "access-2",
                        "token_type": "Bearer",
                        "expires_in": state.token_expires_in.load(Ordering::SeqCst),
                        "refresh_token": "refresh-1",
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
            if state.reject_code_exchange.load(Ordering::SeqCst) {
                return json_response(
                    &mut stream,
                    "400 Bad Request",
                    json!({"error":"invalid_grant","error_description":"oauth-token-sentinel"}),
                )
                .await;
            }
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
        !status
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
async fn token_exchange_errors_are_redacted() {
    let fixture = OAuthFixture::start().await;
    fixture
        .state
        .reject_code_exchange
        .store(true, Ordering::SeqCst);
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
    let message = registry.statuses()[0].message.clone().unwrap_or_default();
    assert!(message.contains("authorization failed"));
    assert!(!message.contains("oauth-token-sentinel"));
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
