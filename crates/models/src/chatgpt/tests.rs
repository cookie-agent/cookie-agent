use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use oven_sdk_openai::OpenAiTokenSource as _;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;
use crate::secure_store::SecureDirectory;

#[test]
fn redirect_uris_are_loopback_ip_with_the_callback_path_only() {
    assert!(valid_redirect_uri("http://127.0.0.1:1455/auth/callback"));
    assert!(valid_redirect_uri("http://127.0.0.1:65535/auth/callback"));
    for refused in [
        "http://localhost:1455/auth/callback",
        "https://127.0.0.1:1455/auth/callback",
        "http://127.0.0.1/auth/callback",
        "http://127.0.0.1:0/auth/callback",
        "http://127.0.0.1:65536/auth/callback",
        "http://127.0.0.1:01455/auth/callback",
        "http://127.0.0.1:1455/callback",
        "http://127.0.0.1:1455/auth/callback?x=1",
        "http://127.0.0.1:1455/auth/callback#x",
    ] {
        assert!(!valid_redirect_uri(refused), "{refused}");
    }
}

#[derive(Clone, Debug)]
struct Received {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: String,
}

impl Received {
    fn form(&self) -> BTreeMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .into_owned()
            .collect()
    }
}

type Responder = dyn Fn(&Received) -> (u16, String) + Send + Sync;

/// A one-route-table HTTP/1.1 server for the token, revoke, and models
/// endpoints.
struct FakeOpenAi {
    base: String,
    received: Arc<Mutex<Vec<Received>>>,
    responder: Arc<Mutex<Arc<Responder>>>,
    /// How long each answer waits after the request is read.
    delay: Arc<Mutex<std::time::Duration>>,
}

impl FakeOpenAi {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let responder: Arc<Mutex<Arc<Responder>>> =
            Arc::new(Mutex::new(Arc::new(|_: &Received| (404, "{}".to_owned()))));
        let delay = Arc::new(Mutex::new(std::time::Duration::ZERO));
        let (log, route, wait) = (
            Arc::clone(&received),
            Arc::clone(&responder),
            Arc::clone(&delay),
        );
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let (log, route, wait) = (Arc::clone(&log), Arc::clone(&route), Arc::clone(&wait));
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    let request = loop {
                        let read = stream.read(&mut chunk).await.unwrap();
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
                        else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&buffer[..end]).to_string();
                        let mut lines = head.lines();
                        let mut first = lines.next().unwrap().split(' ');
                        let method = first.next().unwrap().to_owned();
                        let path = first.next().unwrap().to_owned();
                        let headers = lines
                            .filter_map(|line| line.split_once(':'))
                            .map(|(name, value)| {
                                (name.trim().to_lowercase(), value.trim().to_owned())
                            })
                            .collect::<BTreeMap<_, _>>();
                        let length = headers
                            .get("content-length")
                            .and_then(|value| value.parse::<usize>().ok())
                            .unwrap_or(0);
                        if buffer.len() < end + 4 + length {
                            continue;
                        }
                        break Received {
                            method,
                            path,
                            headers,
                            body: String::from_utf8_lossy(&buffer[end + 4..end + 4 + length])
                                .to_string(),
                        };
                    };
                    let pause = *wait.lock().unwrap();
                    tokio::time::sleep(pause).await;
                    let respond = Arc::clone(&route.lock().unwrap());
                    let (status, body) = respond(&request);
                    log.lock().unwrap().push(request);
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
        Self {
            base,
            received,
            responder,
            delay,
        }
    }

    fn delay(&self, delay: std::time::Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    fn respond(&self, responder: impl Fn(&Received) -> (u16, String) + Send + Sync + 'static) {
        *self.responder.lock().unwrap() = Arc::new(responder);
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    fn endpoints(&self) -> ChatGptEndpoints {
        ChatGptEndpoints {
            issuer: "https://auth.openai.com".into(),
            authorize: "https://auth.openai.com/api/accounts/authorize".into(),
            token: format!("{}/token", self.base),
            revoke: format!("{}/revoke", self.base),
            api: format!("{}/v1", self.base),
        }
    }
}

fn id_token(claims: Value) -> String {
    format!(
        "{}.{}.signature",
        base64url::encode(br#"{"alg":"RS256"}"#),
        base64url::encode(claims.to_string().as_bytes())
    )
}

fn query(url: &str) -> BTreeMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

fn open(directory: &tempfile::TempDir, server: &FakeOpenAi) -> Arc<ChatGptCredentials> {
    ChatGptCredentials::open(
        SecureDirectory::open(directory.path()).unwrap(),
        server.endpoints(),
        reqwest::Client::new(),
    )
    .unwrap()
}

const REDIRECT: &str = "http://127.0.0.1:1455/auth/callback";

/// Answers the code exchange as OpenAI does for a first sign-in.
fn exchange_answer(nonce: String, scope: &'static str) -> impl Fn(&Received) -> (u16, String) {
    move |_| {
        (
            200,
            json!({
                "access_token": "access-1",
                "refresh_token": "refresh-1",
                "id_token": id_token(json!({
                    "iss": "https://auth.openai.com",
                    "aud": "oaiapp_issued",
                    "exp": now_unix() + 3600,
                    "nonce": nonce,
                    "sub": "user-123",
                    "email": "me@example.com",
                })),
                "token_type": "Bearer",
                "expires_in": 3600,
                "earliest_refresh_at": now_unix() + 3240,
                "scope": scope,
            })
            .to_string(),
        )
    }
}

async fn signed_in(directory: &tempfile::TempDir, server: &FakeOpenAi) -> Arc<ChatGptCredentials> {
    let credentials = open(directory, server);
    let url = credentials.begin_sign_in(REDIRECT).unwrap();
    let params = query(&url);
    server.respond(exchange_answer(
        params["nonce"].clone(),
        "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
    ));
    credentials
        .complete_sign_in(&format!(
            "{REDIRECT}?code=the-code&state={}&client_id=oaiapp_issued&scope=x",
            params["state"]
        ))
        .await
        .unwrap();
    credentials
}

#[tokio::test]
async fn a_first_sign_in_registers_and_a_later_one_reuses_the_issued_client() {
    let directory = tempfile::tempdir().unwrap();
    let server = FakeOpenAi::start().await;
    let credentials = open(&directory, &server);
    let url = credentials.begin_sign_in(REDIRECT).unwrap();
    assert!(url.starts_with("https://auth.openai.com/api/accounts/authorize?"));
    let params = query(&url);
    assert_eq!(params["client_id"], "dynamic_agent_client");
    assert_eq!(params["agent_name_hint"], "Cookie Agent");
    assert!(params["ext_agent_host_id"].starts_with("urn:uuid:"));
    assert_eq!(params["redirect_uri"], REDIRECT);
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["code_challenge_method"], "S256");
    assert!(
        params["scope"]
            .split(' ')
            .any(|scope| scope == TOKEN_SHARING_SCOPE)
    );
    assert_eq!(params["resource"], server.endpoints().api);
    // The same redirect while waiting gives the same URL.
    assert_eq!(credentials.begin_sign_in(REDIRECT).unwrap(), url);

    server.respond(exchange_answer(
        params["nonce"].clone(),
        "openid email offline_access resource.invoke chatgpt.tokens.use.direct",
    ));
    let signed = credentials
        .complete_sign_in(&format!(
            "{REDIRECT}?code=the-code&state={}&client_id=oaiapp_issued",
            params["state"]
        ))
        .await
        .unwrap();
    assert_eq!(signed.subject, "user-123");
    assert_eq!(signed.email.as_deref(), Some("me@example.com"));
    let exchange = server.received().pop().unwrap();
    assert_eq!(exchange.path, "/token");
    let form = exchange.form();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["client_id"], "oaiapp_issued");
    assert_eq!(form["code"], "the-code");
    assert_eq!(form["redirect_uri"], REDIRECT);
    let challenge = base64url::encode(&<sha2::Sha256 as sha2::Digest>::digest(
        form["code_verifier"].as_bytes(),
    ));
    assert_eq!(challenge, params["code_challenge"]);
    assert_eq!(
        credentials.account(),
        Some(AccountSummary {
            subject: "user-123".into(),
            email: Some("me@example.com".into()),
            needs_sign_in: false,
        })
    );
    assert_eq!(
        credentials.access_token().await.unwrap().expose_secret(),
        "access-1"
    );

    // The sign-in survives a restart, and the next one uses the issued client.
    let reopened = open(&directory, &server);
    assert_eq!(reopened.account().unwrap().subject, "user-123");
    let again = query(
        &reopened
            .begin_sign_in("http://127.0.0.1:2000/auth/callback")
            .unwrap(),
    );
    assert_eq!(again["client_id"], "oaiapp_issued");
    assert!(!again.contains_key("agent_name_hint"));
    assert_eq!(again["ext_agent_host_id"], params["ext_agent_host_id"]);
    assert_eq!(again["login_hint"], "me@example.com");
}

#[tokio::test]
async fn callbacks_that_do_not_belong_to_the_sign_in_are_refused() {
    let directory = tempfile::tempdir().unwrap();
    let server = FakeOpenAi::start().await;
    let credentials = open(&directory, &server);
    assert!(matches!(
        credentials.complete_sign_in(REDIRECT).await,
        Err(ChatGptError::NotPending)
    ));
    assert!(matches!(
        credentials.begin_sign_in("http://localhost:1455/auth/callback"),
        Err(ChatGptError::InvalidRedirectUri)
    ));
    let params = query(&credentials.begin_sign_in(REDIRECT).unwrap());
    assert!(matches!(
        credentials
            .complete_sign_in(&format!("{REDIRECT}?code=c&state=forged"))
            .await,
        Err(ChatGptError::StateMismatch)
    ));
    assert!(matches!(
        credentials
            .complete_sign_in(&format!(
                "http://127.0.0.1:9999/auth/callback?code=c&state={}",
                params["state"]
            ))
            .await,
        Err(ChatGptError::InvalidCallback(_))
    ));
    // Refusals of a forged callback keep the sign-in waiting.
    assert!(matches!(
        credentials
            .complete_sign_in(&format!(
                "{REDIRECT}?error=access_denied&state={}",
                params["state"]
            ))
            .await,
        Err(ChatGptError::Denied)
    ));
    assert!(server.received().is_empty());
    assert!(credentials.account().is_none());
}

#[tokio::test]
async fn a_sign_in_without_plan_usage_or_for_another_nonce_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let server = FakeOpenAi::start().await;
    let credentials = open(&directory, &server);
    let params = query(&credentials.begin_sign_in(REDIRECT).unwrap());
    server.respond(exchange_answer(params["nonce"].clone(), "openid email"));
    let callback = format!(
        "{REDIRECT}?code=c&state={}&client_id=oaiapp_issued",
        params["state"]
    );
    assert!(matches!(
        credentials.complete_sign_in(&callback).await,
        Err(ChatGptError::ScopeMissing)
    ));
    let params = query(&credentials.begin_sign_in(REDIRECT).unwrap());
    server.respond(exchange_answer(
        "another-nonce".into(),
        "openid chatgpt.tokens.use.direct",
    ));
    assert!(matches!(
        credentials
            .complete_sign_in(&format!(
                "{REDIRECT}?code=c&state={}&client_id=oaiapp_issued",
                params["state"]
            ))
            .await,
        Err(ChatGptError::Failed(_))
    ));
    assert!(credentials.account().is_none());
}

fn expire(directory: &tempfile::TempDir) {
    let path = directory.path().join("chatgpt-sign-in.json");
    let mut file: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    file["account"]["expires_at"] = json!(now_unix() + 60);
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
}

#[tokio::test]
async fn an_expiring_token_refreshes_and_rotates_the_refresh_token() {
    let directory = tempfile::tempdir().unwrap();
    let server = FakeOpenAi::start().await;
    signed_in(&directory, &server).await;
    expire(&directory);
    let credentials = open(&directory, &server);
    server.respond(|request| {
        assert_eq!(request.form()["refresh_token"], "refresh-1");
        (
            200,
            json!({"access_token": "access-2", "refresh_token": "refresh-2", "expires_in": 3600})
                .to_string(),
        )
    });
    assert_eq!(
        credentials.access_token().await.unwrap().expose_secret(),
        "access-2"
    );
    let refresh = server.received().pop().unwrap();
    assert_eq!(refresh.method, "POST");
    let form = refresh.form();
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["client_id"], "oaiapp_issued");
    assert_eq!(form["resource"], server.endpoints().api);
    assert!(!form.contains_key("scope"));
    // Still fresh: no second refresh.
    assert_eq!(
        credentials.token().await.unwrap().expose_secret(),
        "access-2"
    );
    assert_eq!(server.received().len(), 2);
    let stored = std::fs::read_to_string(directory.path().join("chatgpt-sign-in.json")).unwrap();
    assert!(stored.contains("refresh-2"));
}

#[tokio::test]
async fn a_refused_token_is_refreshed_even_before_it_expires() {
    let directory = tempfile::tempdir().unwrap();
    let server = FakeOpenAi::start().await;
    let credentials = signed_in(&directory, &server).await;
    server.respond(|_| {
        (
            200,
            json!({"access_token": "access-2", "expires_in": 3600}).to_string(),
        )
    });
    let first = credentials.token().await.unwrap();
    credentials.rejected(&first);
    assert_eq!(
        credentials.token().await.unwrap().expose_secret(),
        "access-2"
    );
    // A refresh without a new refresh token keeps the current one.
    let stored = std::fs::read_to_string(directory.path().join("chatgpt-sign-in.json")).unwrap();
    assert!(stored.contains("refresh-1"));
}

#[tokio::test]
async fn a_refused_refresh_token_needs_a_new_sign_in() {
    let directory = tempfile::tempdir().unwrap();
    let server = FakeOpenAi::start().await;
    signed_in(&directory, &server).await;
    expire(&directory);
    let credentials = open(&directory, &server);
    let changes = credentials.subscribe();
    server.respond(|_| {
        (
            400,
            json!({"error": "refresh_token_reused", "error_description": "no"}).to_string(),
        )
    });
    // The token still works for a minute, but a refused refresh token is final.
    let error = credentials.token().await.unwrap_err();
    assert_eq!(error.kind, oven_sdk::ModelErrorKind::Auth);
    assert_eq!(
        error.diagnostics.vendor_code.as_deref(),
        Some("provider_sign_in_required")
    );
    assert!(credentials.account().unwrap().needs_sign_in);
    assert!(changes.has_changed().unwrap());
    let requests = server.received().len();
    assert!(matches!(
        credentials.access_token().await,
        Err(ChatGptError::SignInRequired)
    ));
    assert_eq!(server.received().len(), requests);
    assert!(open(&directory, &server).account().unwrap().needs_sign_in);
}

#[tokio::test]
async fn a_refresh_whose_caller_gives_up_still_saves_the_rotated_token() {
    let directory = tempfile::tempdir().unwrap();
    let server = FakeOpenAi::start().await;
    signed_in(&directory, &server).await;
    expire(&directory);
    let credentials = open(&directory, &server);
    server.delay(std::time::Duration::from_millis(300));
    server.respond(|_| {
        (
            200,
            json!({"access_token": "access-2", "refresh_token": "refresh-2", "expires_in": 3600})
                .to_string(),
        )
    });
    // The caller's future is dropped while OpenAI is still answering.
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            credentials.access_token()
        )
        .await
        .is_err()
    );
    let path = directory.path().join("chatgpt-sign-in.json");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !std::fs::read_to_string(&path)
        .unwrap()
        .contains("refresh-2")
    {
        assert!(tokio::time::Instant::now() < deadline, "never saved");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    server.delay(std::time::Duration::ZERO);
    assert_eq!(
        credentials.access_token().await.unwrap().expose_secret(),
        "access-2"
    );
    assert_eq!(server.received().len(), 2);
}

#[tokio::test]
async fn a_refresh_refused_after_another_process_rotated_keeps_the_sign_in() {
    let directory = tempfile::tempdir().unwrap();
    let server = FakeOpenAi::start().await;
    signed_in(&directory, &server).await;
    expire(&directory);
    let credentials = open(&directory, &server);
    let path = directory.path().join("chatgpt-sign-in.json");
    let other = path.clone();
    // Another process refreshes with the same token first and saves the
    // rotated one while this process's request is refused as reused.
    server.respond(move |request| {
        assert_eq!(request.form()["refresh_token"], "refresh-1");
        let mut file: Value = serde_json::from_slice(&std::fs::read(&other).unwrap()).unwrap();
        file["account"]["access_token"] = json!("access-other");
        file["account"]["refresh_token"] = json!("refresh-other");
        file["account"]["expires_at"] = json!(now_unix() + 3600);
        std::fs::write(&other, serde_json::to_vec(&file).unwrap()).unwrap();
        (400, json!({"error": "refresh_token_reused"}).to_string())
    });
    assert_eq!(
        credentials.access_token().await.unwrap().expose_secret(),
        "access-other"
    );
    assert!(!credentials.account().unwrap().needs_sign_in);
    let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored["account"]["needs_sign_in"], json!(false));
    assert_eq!(stored["account"]["refresh_token"], json!("refresh-other"));
}

#[tokio::test]
async fn a_transient_refresh_failure_keeps_the_valid_token() {
    let directory = tempfile::tempdir().unwrap();
    let server = FakeOpenAi::start().await;
    signed_in(&directory, &server).await;
    expire(&directory);
    let credentials = open(&directory, &server);
    server.respond(|_| (503, "{}".to_owned()));
    assert_eq!(
        credentials.access_token().await.unwrap().expose_secret(),
        "access-1"
    );
    assert!(!credentials.account().unwrap().needs_sign_in);
}

#[tokio::test]
async fn signing_out_revokes_and_keeps_the_client_registration() {
    let directory = tempfile::tempdir().unwrap();
    let server = FakeOpenAi::start().await;
    let credentials = signed_in(&directory, &server).await;
    server.respond(|_| (200, String::new()));
    credentials.sign_out().await.unwrap();
    let revoke = server.received().pop().unwrap();
    assert_eq!(revoke.path, "/revoke");
    let form = revoke.form();
    assert_eq!(form["token"], "refresh-1");
    assert_eq!(form["token_type_hint"], "refresh_token");
    assert_eq!(form["client_id"], "oaiapp_issued");
    assert!(credentials.account().is_none());
    assert!(matches!(
        credentials.access_token().await,
        Err(ChatGptError::SignInRequired)
    ));
    let again = query(&credentials.begin_sign_in(REDIRECT).unwrap());
    assert_eq!(again["client_id"], "oaiapp_issued");
}

const LISTING: &str = r#"{"models": [
    {"slug": "gpt-6-luna", "display_name": "GPT-6-Luna", "description": "Small.", "visibility": "list", "supported_in_api": true, "context_window": 272000, "input_modalities": ["text", "image"], "supported_reasoning_levels": [{"effort": "low"}, {"effort": "max"}], "priority": 5},
    {"slug": "gpt-6.1-sol", "display_name": "GPT-6.1-Sol", "description": "Big.", "visibility": "list", "supported_in_api": true, "context_window": 500000, "input_modalities": ["text", "image"], "supported_reasoning_levels": [{"effort": "high"}, {"effort": "ultra"}], "priority": 1, "service_tiers": [{"id": "priority"}]},
    {"slug": "gpt-reserve", "display_name": "Hidden", "visibility": "hide", "supported_in_api": true, "priority": 4},
    {"slug": "no-api", "display_name": "No API", "visibility": "list", "supported_in_api": false, "priority": 2}
]}"#;

#[test]
fn the_listing_keeps_listed_api_models_in_codex_order_without_ultra() {
    let models = catalog::parse_models_for_test(LISTING.as_bytes()).unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.slug.as_str())
            .collect::<Vec<_>>(),
        ["gpt-6.1-sol", "gpt-6-luna"]
    );
    assert_eq!(models[0].efforts, ["high"]);
    assert_eq!(models[1].efforts, ["low", "max"]);
    let entry = provider_entry(&models, None);
    let record = entry.record.unwrap();
    assert_eq!(record.name, DISPLAY_NAME);
    assert_eq!(record.npm, crate::recipes::CHATGPT_NPM);
    let sol = record.models[&cookie_agent_identity::ProviderModelId::new("gpt-6.1-sol").unwrap()]
        .record
        .clone()
        .unwrap();
    assert_eq!(sol.limits.input, Some(500_000));
    assert_eq!(sol.limits.context, 500_000 + sol.limits.output);
    assert!(sol.tool_call && sol.reasoning && sol.attachment);
    assert_eq!(sol.dynamically_loaded_tools, Some(true));
}

#[tokio::test]
async fn the_model_list_is_fetched_with_the_token_and_cached_per_account() {
    let directory = tempfile::tempdir().unwrap();
    let server = FakeOpenAi::start().await;
    let credentials = signed_in(&directory, &server).await;
    assert!(credentials.cached_models().is_empty());
    server.respond(|request| {
        assert_eq!(request.path, "/v1/models");
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer access-1")
        );
        (200, LISTING.to_owned())
    });
    let models = credentials.fetch_models().await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(open(&directory, &server).cached_models(), models);
    server.respond(|_| (200, String::new()));
    credentials.sign_out().await.unwrap();
    assert!(credentials.cached_models().is_empty());
}
