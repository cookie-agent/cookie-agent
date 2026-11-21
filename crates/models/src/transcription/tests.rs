use std::{collections::BTreeMap, time::Duration};

use cookie_agent_identity::{AuthFieldName, AuthMethodId, AuthParameterId};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};

use super::*;
use crate::{SafeAuthParameterValue, SecretString};

/// What the fake service received: the request head (lowercase) and body.
struct Received {
    head: String,
    body: Vec<u8>,
}

/// A one-request HTTP server answering `status` with `body`, after `delay`.
async fn serve(
    status: u16,
    body: &'static str,
    delay: Duration,
) -> (EndpointUrl, oneshot::Receiver<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut data = Vec::new();
        let mut buffer = [0u8; 8192];
        let (head_end, length) = loop {
            let read = socket.read(&mut buffer).await.unwrap();
            data.extend_from_slice(&buffer[..read]);
            if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                break (end + 4, length);
            }
        };
        while data.len() < head_end + length {
            let read = socket.read(&mut buffer).await.unwrap();
            data.extend_from_slice(&buffer[..read]);
        }
        let head = String::from_utf8_lossy(&data[..head_end]).to_ascii_lowercase();
        let _ = sender.send(Received {
            head,
            body: data[head_end..head_end + length].to_vec(),
        });
        tokio::time::sleep(delay).await;
        let answer = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(answer.as_bytes()).await;
    });
    let endpoint = EndpointUrl::new(format!("http://127.0.0.1:{port}/v1/")).unwrap();
    (endpoint, receiver)
}

fn auth(method: &str, values: &[(&str, &str)], parameters: &[(&str, &str)]) -> AuthDefinition {
    AuthDefinition {
        method: AuthMethodId::new(method).unwrap(),
        parameters: parameters
            .iter()
            .map(|(id, value)| {
                (
                    AuthParameterId::new(*id).unwrap(),
                    SafeAuthParameterValue::new(*value).unwrap(),
                )
            })
            .collect(),
        values: values
            .iter()
            .map(|(field, value)| {
                (
                    AuthFieldName::new(*field).unwrap(),
                    SecretString::new(*value).unwrap(),
                )
            })
            .collect(),
    }
}

fn headers(pairs: &[(&str, &str)]) -> BTreeMap<HeaderName, SafeStaticHeaderValue> {
    pairs
        .iter()
        .map(|(name, value)| {
            (
                HeaderName::new(*name).unwrap(),
                SafeStaticHeaderValue::new(*value).unwrap(),
            )
        })
        .collect()
}

fn request(audio: &[u8]) -> TranscriptionRequest<'_> {
    TranscriptionRequest {
        audio,
        media_type: "audio/webm",
        filename: "voice.webm",
        model: "whisper-large-v3",
        language: Some("en"),
        prompt: Some("UniEnv, UCMA"),
    }
}

#[tokio::test]
async fn a_transcription_posts_the_recording_with_its_fields_and_credentials() {
    let (endpoint, received) = serve(200, r#"{"text":"  hello there \n"}"#, Duration::ZERO).await;
    let target = TranscriptionTarget::endpoint(
        &endpoint,
        Some(&auth("bearer-api-key-v1", &[("api_key", "sk-test")], &[])),
        &headers(&[("x-global", "g"), ("x-both", "global")]),
        &headers(&[("x-backend", "b"), ("x-both", "backend")]),
    )
    .unwrap();
    let audio = b"\x1a\x45\xdf\xa3 not really opus".to_vec();
    let transcript = transcribe(&target, request(&audio), None).await.unwrap();
    assert_eq!(transcript.text, "hello there");

    let received = received.await.unwrap();
    assert!(received.head.starts_with("post /v1/audio/transcriptions "));
    assert!(received.head.contains("authorization: bearer sk-test"));
    assert!(received.head.contains("x-global: g"));
    assert!(received.head.contains("x-backend: b"));
    assert!(received.head.contains("x-both: backend"));
    let boundary = received
        .head
        .lines()
        .find_map(|line| line.strip_prefix("content-type: multipart/form-data; boundary="))
        .unwrap()
        .to_owned();
    let body = String::from_utf8_lossy(&received.body).into_owned();
    for (name, value) in [
        ("model", "whisper-large-v3"),
        ("response_format", "json"),
        ("language", "en"),
        ("prompt", "UniEnv, UCMA"),
    ] {
        assert!(
            body.contains(&format!(
                "Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )),
            "{name}"
        );
    }
    assert!(
        body.contains("name=\"file\"; filename=\"voice.webm\"\r\nContent-Type: audio/webm\r\n\r\n")
    );
    let start = received
        .body
        .windows(audio.len())
        .position(|window| window == audio.as_slice())
        .unwrap();
    assert!(
        received.body[start + audio.len()..]
            .starts_with(format!("\r\n--{boundary}--\r\n").as_bytes())
    );
}

#[tokio::test]
async fn an_api_key_header_goes_in_the_named_header() {
    let (endpoint, received) = serve(200, r#"{"text":"ok"}"#, Duration::ZERO).await;
    let target = TranscriptionTarget::endpoint(
        &endpoint,
        Some(&auth(
            "api-key-header-v1",
            &[("api_key", "k-1")],
            &[("header_name", "x-api-key")],
        )),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    transcribe(&target, request(b"abc"), None).await.unwrap();
    let head = received.await.unwrap().head;
    assert!(head.contains("x-api-key: k-1"));
    assert!(!head.contains("authorization"));
}

#[tokio::test]
async fn no_auth_sends_no_credentials() {
    let (endpoint, received) = serve(200, r#"{"text":""}"#, Duration::ZERO).await;
    let target =
        TranscriptionTarget::endpoint(&endpoint, None, &BTreeMap::new(), &BTreeMap::new()).unwrap();
    let transcript = transcribe(&target, request(b"abc"), None).await.unwrap();
    assert_eq!(transcript.text, "");
    assert!(!received.await.unwrap().head.contains("authorization"));
}

#[tokio::test]
async fn a_refusal_says_its_status_and_message() {
    let (endpoint, _received) = serve(
        401,
        r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error"}}"#,
        Duration::ZERO,
    )
    .await;
    let target =
        TranscriptionTarget::endpoint(&endpoint, None, &BTreeMap::new(), &BTreeMap::new()).unwrap();
    let error = transcribe(&target, request(b"abc"), None)
        .await
        .unwrap_err();
    assert_eq!(
        error,
        TranscriptionError::Status {
            status: 401,
            message: Some("Incorrect API key provided".to_owned())
        }
    );
    assert_eq!(
        error.to_string(),
        "returned 401 (Incorrect API key provided)"
    );
}

#[tokio::test]
async fn an_answer_without_text_is_refused() {
    let (endpoint, _received) = serve(200, r#"{"words":[]}"#, Duration::ZERO).await;
    let target =
        TranscriptionTarget::endpoint(&endpoint, None, &BTreeMap::new(), &BTreeMap::new()).unwrap();
    let error = transcribe(&target, request(b"abc"), None)
        .await
        .unwrap_err();
    assert!(
        matches!(error, TranscriptionError::BadAnswer(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_slow_backend_times_out() {
    let (endpoint, _received) = serve(200, r#"{"text":"late"}"#, Duration::from_secs(5)).await;
    let target =
        TranscriptionTarget::endpoint(&endpoint, None, &BTreeMap::new(), &BTreeMap::new()).unwrap();
    let error = transcribe(&target, request(b"abc"), Some(Duration::from_millis(200)))
        .await
        .unwrap_err();
    assert_eq!(error, TranscriptionError::Timeout);
}

#[tokio::test]
async fn a_closed_port_is_unreachable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let endpoint = EndpointUrl::new(format!("http://127.0.0.1:{port}/v1")).unwrap();
    let target =
        TranscriptionTarget::endpoint(&endpoint, None, &BTreeMap::new(), &BTreeMap::new()).unwrap();
    let error = transcribe(&target, request(b"abc"), None)
        .await
        .unwrap_err();
    assert!(
        matches!(error, TranscriptionError::Unreachable(_)),
        "{error:?}"
    );
}

#[test]
fn methods_without_a_plain_header_are_refused() {
    let endpoint = EndpointUrl::new("https://bedrock.example/v1").unwrap();
    let error = TranscriptionTarget::endpoint(
        &endpoint,
        Some(&auth(
            "aws-sigv4-credentials-v1",
            &[("access_key_id", "a"), ("secret_access_key", "s")],
            &[],
        )),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert_eq!(
        error,
        TranscriptionError::UnsupportedAuth("aws-sigv4-credentials-v1".to_owned())
    );
}

#[test]
fn a_target_formats_without_its_credentials() {
    let endpoint = EndpointUrl::new("https://api.example/v1").unwrap();
    let target = TranscriptionTarget::endpoint(
        &endpoint,
        Some(&auth(
            "bearer-api-key-v1",
            &[("api_key", "sk-secret-value")],
            &[],
        )),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    let shown = format!("{target:?}");
    assert!(!shown.contains("sk-secret-value"), "{shown}");
    assert_eq!(target.url(), "https://api.example/v1/audio/transcriptions");
}
