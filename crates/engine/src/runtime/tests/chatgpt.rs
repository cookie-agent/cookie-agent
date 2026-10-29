//! The built-in ChatGPT provider's sign-in through the engine: begin,
//! complete, the connection and model list it publishes, refusals, sign-out,
//! and the daemon's own redirect listener.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cookie_agent_models::{
    ModelManager,
    chatgpt::{ChatGptCredentials, ChatGptEndpoints},
    manager::ChatGptManagerInputs,
    provider_store::ProviderStore,
    secure_store::SecureDirectory,
};
use cookie_agent_protocol::{
    AuthMethodId, ClientConnectId, ProviderAuthBeginParams, ProviderAuthCancelParams,
    ProviderAuthCompleteParams, ProviderConnectParams, ProviderCredentialValues,
    ProviderDescriptor, ProviderId,
};
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::support::*;
use crate::{Engine, EngineError, EngineOptions};

/// Answers the token, revoke, and models endpoints as OpenAI does, and
/// records each request's path.
struct FakeOpenAi {
    base: String,
    paths: Arc<Mutex<Vec<String>>>,
    nonce: Arc<Mutex<String>>,
}

impl FakeOpenAi {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let paths = Arc::new(Mutex::new(Vec::new()));
        let nonce = Arc::new(Mutex::new(String::new()));
        let (log, current_nonce) = (Arc::clone(&paths), Arc::clone(&nonce));
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (log, current_nonce) = (Arc::clone(&log), Arc::clone(&current_nonce));
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    let path = loop {
                        let read = stream.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
                        else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&buffer[..end]).to_string();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buffer.len() < end + 4 + length {
                            continue;
                        }
                        break head.split(' ').nth(1).unwrap_or_default().to_owned();
                    };
                    log.lock().unwrap().push(path.clone());
                    let body = match path.as_str() {
                        "/token" => json!({
                            "access_token": "access-1",
                            "refresh_token": "refresh-1",
                            "id_token": format!(
                                "e30.{}.signature",
                                URL_SAFE_NO_PAD.encode(json!({
                                    "iss": "https://auth.openai.com",
                                    "aud": "oaiapp_issued",
                                    "exp": 4_000_000_000_u64,
                                    "nonce": current_nonce.lock().unwrap().clone(),
                                    "sub": "user-123",
                                    "email": "me@example.com",
                                }).to_string())
                            ),
                            "expires_in": 3600,
                            "scope": "openid email offline_access resource.invoke chatgpt.tokens.use.direct",
                        })
                        .to_string(),
                        "/v1/models" => json!({"models": [{
                            "slug": "gpt-6-luna",
                            "display_name": "GPT-6-Luna",
                            "visibility": "list",
                            "supported_in_api": true,
                            "context_window": 272000,
                            "input_modalities": ["text"],
                            "supported_reasoning_levels": [{"effort": "low"}],
                            "priority": 5,
                        }]})
                        .to_string(),
                        _ => String::new(),
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        Self { base, paths, nonce }
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

    fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }

    /// Takes the nonce and state of an authorization URL, as the browser
    /// would carry them.
    fn authorized(&self, authorization_url: &str) -> String {
        let query = authorization_url
            .split_once('?')
            .unwrap()
            .1
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect::<BTreeMap<_, _>>();
        *self.nonce.lock().unwrap() = query["nonce"].clone();
        query["state"].clone()
    }
}

struct ChatGptFixture {
    _directory: PanicResistantTempDir,
    engine: Engine,
    server: FakeOpenAi,
}

async fn chatgpt_fixture() -> ChatGptFixture {
    let directory = private_tempdir();
    let store_path = directory.path().join("provider-store");
    create_private_test_dir(&store_path);
    let store = ProviderStore::open(&store_path).expect("provider store");
    let server = FakeOpenAi::start().await;
    let credentials = ChatGptCredentials::open(
        SecureDirectory::open(store.directory_path()).unwrap(),
        server.endpoints(),
        reqwest::Client::new(),
    )
    .unwrap();
    let manager = Arc::new(
        ModelManager::new_with_chatgpt(
            BTreeMap::new(),
            BTreeMap::new(),
            empty_catalog(),
            store,
            Some(ChatGptManagerInputs {
                credentials,
                models: Vec::new(),
            }),
        )
        .unwrap(),
    );
    let engine = Engine::open_async(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config: empty_configuration(),
        model_manager: manager,
        runtimes: cookie_agent_runtime_api::RuntimeOptions::new(directory.path()),
    })
    .await
    .unwrap();
    ChatGptFixture {
        _directory: directory,
        engine,
        server,
    }
}

fn chatgpt() -> ProviderId {
    ProviderId::new("chatgpt").unwrap()
}

fn method() -> AuthMethodId {
    AuthMethodId::new("chatgpt-sign-in-v1").unwrap()
}

fn descriptor(engine: &Engine) -> ProviderDescriptor {
    engine
        .runtime_snapshot()
        .unwrap()
        .snapshot
        .providers
        .into_iter()
        .find(|provider| provider.id == chatgpt())
        .unwrap()
}

async fn wait_for(
    engine: &Engine,
    ready: impl Fn(&ProviderDescriptor) -> bool,
) -> ProviderDescriptor {
    for _ in 0..200 {
        let provider = descriptor(engine);
        if ready(&provider) {
            return provider;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "the ChatGPT provider never reached the expected state: {:?}",
        descriptor(engine)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_in_connects_the_account_lists_its_plan_and_signs_out() {
    let fixture = chatgpt_fixture().await;
    let engine = &fixture.engine;
    let before = descriptor(engine);
    assert!(before.auth_methods.is_empty());
    assert_eq!(before.sign_in_methods.len(), 1);
    assert_eq!(
        before.sign_in_methods[0].display_name.as_str(),
        "Continue with ChatGPT"
    );
    assert_eq!(
        before.sign_in_methods[0].redirect_path.as_str(),
        "/auth/callback"
    );
    assert!(before.sign_in_account.is_none());
    assert!(before.durable_connection.is_none());

    let redirect = "http://127.0.0.1:1455/auth/callback".to_owned();
    let began = engine
        .begin_provider_auth(ProviderAuthBeginParams {
            provider_id: chatgpt(),
            method: method(),
            redirect_uri: Some(redirect.clone()),
        })
        .await
        .unwrap();
    assert_eq!(began.redirect_uri, redirect);
    let state = fixture.server.authorized(&began.authorization_url);
    let completed = engine
        .complete_provider_auth(ProviderAuthCompleteParams {
            provider_id: chatgpt(),
            callback_url: format!("{redirect}?code=c&state={state}&client_id=oaiapp_issued"),
        })
        .await
        .unwrap();
    assert_eq!(
        completed.account.email.as_ref().map(|email| email.as_str()),
        Some("me@example.com")
    );

    let connected = wait_for(engine, |provider| provider.model_counts.available > 0).await;
    assert!(connected.durable_connection.is_some());
    let account = connected.sign_in_account.unwrap();
    assert_eq!(account.email.unwrap().as_str(), "me@example.com");
    assert!(!account.sign_in_required);
    assert!(
        engine
            .runtime_snapshot()
            .unwrap()
            .snapshot
            .models
            .iter()
            .any(|model| model.key.to_string() == "chatgpt/gpt-6-luna")
    );
    assert!(fixture.server.paths().contains(&"/v1/models".to_owned()));

    // Only the sign-in connects a sign-in method.
    let error = engine
        .connect_provider(ProviderConnectParams {
            provider_id: chatgpt(),
            expected_revision: engine.runtime_snapshot().unwrap().snapshot.revision,
            setup_values: BTreeMap::new(),
            auth_method: method(),
            auth_values: serde_json::from_value::<ProviderCredentialValues>(
                json!({"subject": "forged"}),
            )
            .unwrap(),
            client_connect_id: ClientConnectId::new("forged").unwrap(),
        })
        .unwrap_err();
    assert!(
        matches!(error, EngineError::ProviderSignIn { .. }),
        "{error}"
    );

    engine.sign_out_chatgpt().await.unwrap();
    assert!(fixture.server.paths().contains(&"/revoke".to_owned()));
    let signed_out = wait_for(engine, |provider| provider.sign_in_account.is_none()).await;
    assert!(signed_out.sign_in_account.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_sign_in_requests_name_their_reason() {
    let fixture = chatgpt_fixture().await;
    let engine = &fixture.engine;
    let reason = |error: EngineError| match error {
        EngineError::ProviderSignIn { reason, .. } => reason,
        other => panic!("unexpected error {other}"),
    };
    let wrong_redirect = engine
        .begin_provider_auth(ProviderAuthBeginParams {
            provider_id: chatgpt(),
            method: method(),
            redirect_uri: Some("http://localhost:1455/auth/callback".into()),
        })
        .await
        .unwrap_err();
    assert!(reason(wrong_redirect).starts_with("invalid_redirect_uri"));
    let other_provider = engine
        .begin_provider_auth(ProviderAuthBeginParams {
            provider_id: ProviderId::new("openai").unwrap(),
            method: method(),
            redirect_uri: None,
        })
        .await
        .unwrap_err();
    assert!(reason(other_provider).starts_with("sign_in_unsupported"));
    let not_pending = engine
        .complete_provider_auth(ProviderAuthCompleteParams {
            provider_id: chatgpt(),
            callback_url: "http://127.0.0.1:1455/auth/callback?code=c&state=s".into(),
        })
        .await
        .unwrap_err();
    assert!(reason(not_pending).starts_with("sign_in_not_pending"));
    engine
        .cancel_provider_auth(ProviderAuthCancelParams {
            provider_id: chatgpt(),
        })
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_catches_the_redirect_itself_and_announces_the_outcome() {
    let fixture = chatgpt_fixture().await;
    let engine = &fixture.engine;
    let mut completed = engine.subscribe_provider_auth();
    let began = engine
        .begin_provider_auth(ProviderAuthBeginParams {
            provider_id: chatgpt(),
            method: method(),
            redirect_uri: None,
        })
        .await
        .unwrap();
    assert!(began.redirect_uri.starts_with("http://127.0.0.1:"));
    assert!(began.redirect_uri.ends_with("/auth/callback"));
    let state = fixture.server.authorized(&began.authorization_url);
    // The browser's redirect, as a plain GET.
    let address = began
        .redirect_uri
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap()
        .to_owned();
    let mut browser = tokio::net::TcpStream::connect(&address).await.unwrap();
    browser
        .write_all(
            format!(
                "GET /auth/callback?code=c&state={state}&client_id=oaiapp_issued HTTP/1.1\r\nhost: {address}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut page = String::new();
    browser.read_to_string(&mut page).await.unwrap();
    assert!(page.contains("Signed in to ChatGPT"), "{page}");
    let announced = tokio::time::timeout(Duration::from_secs(5), completed.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(announced.provider_id, chatgpt());
    assert!(announced.error.is_none());
    assert_eq!(
        announced.account.unwrap().email.unwrap().as_str(),
        "me@example.com"
    );
    wait_for(engine, |provider| provider.durable_connection.is_some()).await;
}
