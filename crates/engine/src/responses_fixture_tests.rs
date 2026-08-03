use std::collections::BTreeMap;

use oven_sdk::{AbortSignal, HistoryTurn, InputPart, Request, TextPart, UserMessage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn completion_stream() -> String {
    concat!(
        "data: {\"type\":\"response.created\",\"response\":{}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_fixture\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"pinned completion\"}]}],\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}}\n\n"
    )
    .into()
}

#[tokio::test]
async fn responses_fixture_captures_exact_request_and_pinned_completion() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture server");
    let address = listener.local_addr().expect("fixture address");
    let (request_tx, request_rx) = tokio::sync::oneshot::channel();
    let response_body = completion_stream();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        let mut request = Vec::new();
        let header_end = loop {
            let mut buffer = [0_u8; 4096];
            let count = socket.read(&mut buffer).await.expect("read request");
            assert_ne!(count, 0, "request ended before headers");
            request.extend_from_slice(&buffer[..count]);
            if let Some(offset) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break offset + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .expect("content length");
        while request.len() < header_end + content_length {
            let mut buffer = [0_u8; 4096];
            let count = socket.read(&mut buffer).await.expect("read body");
            assert_ne!(count, 0, "request ended before body");
            request.extend_from_slice(&buffer[..count]);
        }
        request_tx.send(request).expect("capture request");
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nx-request-id: req_fixture\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });

    let configured: cookie_agent_models::ConfiguredModel =
        serde_json::from_value(serde_json::json!({
            "provider_id":"openai",
            "model_id":"fixture/model",
            "endpoint":format!("http://{address}/v1"),
            "auth":{"type":"openai","api_key":"fixture-secret"},
            "capabilities":{
                "features":[],
                "limits":{},
                "modalities":{"input":["text"],"output":["text"]},
                "media":{"input":{}},
                "cancellation":"local_only",
                "compaction":"unsupported",
                "replay":{"policy":"never","capability":"unsupported","reasoning":false}
            },
            "adaptor":"openai-responses",
            "settings":{}
        }))
        .expect("configured model");
    let model = configured.build("fixture").expect("model");
    let completion = model
        .model()
        .complete(
            Request::new(vec![HistoryTurn::user(UserMessage::new(vec![
                InputPart::Text(TextPart::new("fixture prompt")),
            ]))]),
            AbortSignal::default(),
        )
        .await
        .expect("completion");
    server.await.expect("fixture server");
    let request = request_rx.await.expect("captured request");
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("header terminator")
        + 4;
    let headers = String::from_utf8(request[..header_end].to_vec()).expect("headers");
    let mut lines = headers.lines();
    assert_eq!(lines.next(), Some("POST /v1/responses HTTP/1.1"));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer fixture-secret")
    );
    assert_eq!(
        headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
    let body: serde_json::Value = serde_json::from_slice(&request[header_end..]).expect("body");
    assert_eq!(
        body,
        serde_json::json!({
            "include":["reasoning.encrypted_content"],
            "input":[{
                "content":[{"text":"fixture prompt","type":"input_text"}],
                "role":"user",
                "type":"message"
            }],
            "model":"fixture/model",
            "store":false,
            "stream":true,
            "tool_choice":"auto"
        })
    );
    let text = completion
        .turn
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            oven_sdk::AssistantPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, "pinned completion");
    assert_eq!(
        completion.response.request_id.as_deref(),
        Some("req_fixture")
    );
}
