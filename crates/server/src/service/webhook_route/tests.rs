//! Webhooks through a running daemon: `POST /hooks/<token>` on the main
//! listener and on the public one, and the answers the route gives itself.

use std::{sync::Arc, time::Duration};

use cookie_agent_protocol::{WebhookCreateParams, WebhookTarget};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
};

const WAIT: Duration = Duration::from_secs(10);

struct Answer {
    status: u16,
    head: String,
    body: String,
}

/// `method path` on `port` with `body`, read to the end. The body is
/// written beside the read, since the daemon may answer before it has all
/// of it.
async fn send(port: u16, method: &str, path: &str, body: &str) -> Answer {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (mut reader, mut writer) = stream.into_split();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let writing = tokio::spawn(async move {
        let _ = writer.write_all(request.as_bytes()).await;
        writer
    });
    let mut response = Vec::new();
    tokio::time::timeout(WAIT, reader.read_to_end(&mut response))
        .await
        .expect("an answer")
        .unwrap();
    drop(writing.await);
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a header block");
    let head = String::from_utf8_lossy(&response[..split]).to_ascii_lowercase();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    Answer {
        status,
        head,
        body: String::from_utf8_lossy(&response[split + 4..]).into_owned(),
    }
}

/// A harness whose OpenAI provider answers nowhere.
fn harness() -> crate::tests::Harness {
    let mut catalog = (*crate::tests::openai_catalog()).clone();
    Arc::make_mut(&mut catalog.providers)
        .values_mut()
        .next()
        .expect("OpenAI provider")
        .record
        .as_mut()
        .expect("OpenAI provider record")
        .api = Some("http://127.0.0.1:9/v1".into());
    crate::tests::harness_with_catalog(Arc::new(catalog), |_| {})
}

/// A webhook into a session; its events are accepted whether or not the
/// session can run.
async fn webhook_path(harness: &crate::tests::Harness) -> String {
    let revision = harness.engine.runtime_snapshot().unwrap().snapshot.revision;
    harness
        .engine
        .connect_provider(
            serde_json::from_value(serde_json::json!({
                "provider_id": "openai",
                "expected_revision": revision,
                "setup_values": {},
                "auth_method": "bearer-api-key-v1",
                "auth_values": { "api_key": "test-secret" },
                "client_connect_id": "webhook-tests",
            }))
            .unwrap(),
        )
        .expect("connected");
    let session = harness
        .engine
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "default",
                "model": { "model": "openai/gpt-5-mini", "variant": null },
            }))
            .unwrap(),
        )
        .expect("a session")
        .session_id;
    harness
        .engine
        .create_webhook(WebhookCreateParams::new(WebhookTarget::Session(session)))
        .await
        .expect("created")
        .webhook
        .path
}

#[tokio::test]
async fn the_main_listener_takes_posts_and_answers_the_rest_itself() {
    let harness = harness();
    let running = Arc::clone(&harness.server)
        .serve_on(
            std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            crate::generate_token().unwrap(),
            None,
        )
        .await
        .unwrap();
    let port = running.address().port();
    let path = webhook_path(&harness).await;

    let accepted = send(port, "POST", &path, "{}").await;
    assert_eq!(accepted.status, 202, "{}", accepted.body);
    assert!(accepted.head.contains("cache-control: no-store"));
    let reply: serde_json::Value = serde_json::from_str(&accepted.body).unwrap();
    assert!(reply["event"].is_string());

    let got = send(port, "GET", &path, "").await;
    assert_eq!(got.status, 405);
    assert!(got.head.contains("allow: post"));
    assert_eq!(
        send(port, "POST", "/hooks/AAAAAAAAAAA", "{}").await.status,
        404
    );
    assert_eq!(send(port, "POST", "/hooks/short", "{}").await.status, 404);
    let limit = usize::try_from(harness.engine.webhook_body_limit()).unwrap();
    let large = format!(r#"{{"x":"{}"}}"#, "y".repeat(limit));
    assert_eq!(send(port, "POST", &path, &large).await.status, 413);
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn the_public_listener_serves_webhooks_once_the_daemon_is_ready() {
    let listener = crate::DaemonListener::new(cookie_agent_engine::status::StatusBoard::new(
        cookie_agent_protocol::DaemonPhase::Starting,
    ));
    let public = listener
        .serve_public_on(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let port = public.address().port();
    let harness = harness();
    let path = webhook_path(&harness).await;
    assert_eq!(send(port, "POST", &path, "{}").await.status, 404);
    listener.hand_over(Arc::clone(&harness.server));
    assert_eq!(send(port, "POST", &path, "{}").await.status, 202);
    listener.shutdown();
    public.wait().await;
    harness.engine.shutdown().await;
}
