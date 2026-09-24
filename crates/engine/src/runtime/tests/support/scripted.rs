//! Scripted HTTP model servers and wire-body builders for the runtime tests.

use super::*;

pub(crate) async fn scripted_model_server() -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("scripted listener");
    let address = listener.local_addr().expect("listener address");
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("scripted accept");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            let read = socket.read(&mut buffer).await.expect("scripted read");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"scripted root complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("scripted response");
        String::from_utf8(request).expect("UTF-8 request")
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) async fn native_compaction_server(
    fail_native: bool,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("native compaction listener");
    let address = listener.local_addr().expect("listener address");
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        let count = if fail_native { 3 } else { 2 };
        for index in 0..count {
            let (mut socket, _) = listener.accept().await.expect("native compaction accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let read = socket
                    .read(&mut buffer)
                    .await
                    .expect("native compaction read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = if index == 0 {
                let body = concat!(
                    "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-test\"}}\n\n",
                    "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"initial complete\"}\n\n",
                    "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"initial complete\"}]}}\n\n",
                    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"initial complete\"}]}]}}\n\n"
                );
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else if fail_native && index == 1 {
                "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".into()
            } else if fail_native {
                let body = concat!(
                    "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_summary\",\"model\":\"gpt-test\"}}\n\n",
                    "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_summary\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"fallback summary\"}\n\n",
                    "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_summary\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"fallback summary\"}]}}\n\n",
                    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_summary\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"fallback summary\"}]}]}}\n\n"
                );
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                let body = serde_json::json!({
                    "id": "cmp_1",
                    "created_at": 1_754_000_000_u64,
                    "object": "response.compaction",
                    "output": [{
                        "type": "compaction",
                        "id": "cmp_item_1",
                        "encrypted_content": "opaque-compacted-state",
                        "created_by": "openai"
                    }],
                    "usage": {
                        "input_tokens": 120,
                        "input_tokens_details": {"cached_tokens": 20},
                        "output_tokens": 8,
                        "output_tokens_details": {"reasoning_tokens": 3},
                        "total_tokens": 128
                    }
                })
                .to_string();
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            socket
                .write_all(response.as_bytes())
                .await
                .expect("native compaction response");
            requests.push(String::from_utf8(request).expect("UTF-8 request"));
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) async fn scripted_zero_resource_tool_server()
-> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("zero-resource listener");
    let address = listener.local_addr().expect("listener address");
    let bodies = [
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"zero-resource-write\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"resource-free write rejected\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    ];
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut socket, _) = listener.accept().await.expect("zero-resource accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.expect("zero-resource read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("zero-resource response");
            requests.push(String::from_utf8(request).expect("UTF-8 request"));
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) async fn scripted_delegation_server() -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("delegation listener");
    let address = listener.local_addr().expect("listener address");
    let task = tokio::spawn(async move {
        let bodies = [
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"delegate-call\",\"type\":\"function\",\"function\":{\"name\":\"delegate_subagent\",\"arguments\":\"{\\\"agent_type\\\":\\\"worker\\\",\\\"description\\\":\\\"Write report\\\",\\\"prompt\\\":\\\"write report\\\"}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"delegated child report\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"parent accepted child report\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ];
        let mut requests = Vec::new();
        for body in bodies {
            let (mut socket, _) = listener.accept().await.expect("delegation accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.expect("delegation read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("delegation response");
            requests.push(String::from_utf8(request).expect("UTF-8 request"));
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) async fn scripted_staged_recovery_server() -> (
    String,
    tokio::sync::mpsc::UnboundedSender<MatchedScriptedResponse>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let (endpoint, responses, task) = scripted_channel_server(2).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_body(
                "staged-restart-call",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type":"worker",
                    "description":"Recover staged skill",
                    "prompt":"staged restart"
                }),
            ),
        ))
        .expect("parent response");
    (endpoint, responses, task)
}

pub(crate) async fn scripted_background_delegation_server()
-> (String, tokio::task::JoinHandle<Vec<String>>) {
    let (endpoint, responses, task) = scripted_channel_server(3).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_body(
                "background-delegate-call",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type":"worker",
                    "description":"Write report",
                    "prompt":"write report",
                    "background":true
                }),
            ),
        ))
        .expect("background tool response");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "write report",
            scripted_text_body("first line\nsecond line\nthird line"),
        ))
        .expect("background child response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("parent continued after admission"),
        ))
        .expect("background parent response");
    (endpoint, task)
}

pub(crate) async fn scripted_preset_switch_delegation_server()
-> (String, tokio::task::JoinHandle<Vec<String>>) {
    let (endpoint, responses, task) = scripted_channel_server(5).await;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "complete the shared run",
            scripted_text_body("shared run complete"),
        ))
        .expect("shared response");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "delegate after switching presets",
            scripted_tool_body(
                "preset-delegate-call",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type":"worker",
                    "description":"Preset child",
                    "prompt":"preset child task",
                    "background":true
                }),
            ),
        ))
        .expect("preset delegation response");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "preset child task",
            scripted_text_body("preset child complete"),
        ))
        .expect("preset child response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("preset parent complete"),
        ))
        .expect("preset parent response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_text_body("historical preset summary"),
        ))
        .expect("historical compaction response");
    (endpoint, task)
}

pub(crate) async fn read_scripted_http_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    use tokio::io::AsyncReadExt as _;

    let mut request = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut expected = None;
    loop {
        let read = socket
            .read(&mut buffer)
            .await
            .expect("scripted request read");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if expected.is_none()
            && let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            expected = Some(header_end + 4 + content_length);
        }
        if expected.is_some_and(|expected| request.len() >= expected) {
            break;
        }
    }
    request
}

pub(crate) async fn write_scripted_sse(socket: &mut tokio::net::TcpStream, body: &str) {
    use tokio::io::AsyncWriteExt as _;

    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .expect("scripted SSE response");
}

/// Streams one text delta and then holds the connection open, so an attempt
/// can be interrupted while it is genuinely mid-stream.
pub(crate) async fn scripted_stalled_stream_server(
    first_delta: &str,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::AsyncWriteExt as _;

    let first_delta = first_delta.to_owned();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("stalled listener");
    let address = listener.local_addr().expect("stalled listener address");
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        // Accepted sockets stay open for the whole test: the stream must not
        // reach EOF while the test interrupts it.
        let mut held = Vec::new();
        while let Ok((mut socket, _)) = listener.accept().await {
            requests.push(
                String::from_utf8(read_scripted_http_request(&mut socket).await)
                    .expect("UTF-8 stalled request"),
            );
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
            let body = format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[{"delta":{"content":first_delta},"finish_reason":null}]})
            );
            let chunk = format!("{:x}\r\n{body}\r\n", body.len());
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(chunk.as_bytes()).await;
            let _ = socket.flush().await;
            held.push(socket);
        }
        drop(held);
        requests
    });
    (format!("http://{address}/v1"), task)
}

#[derive(Clone, Copy)]
pub(crate) enum RetryModelResponse {
    Status(u16),
    /// Stream partial content, then fail with a retryable in-band error.
    PartialError,
    Success,
}

pub(crate) async fn retry_model_server(
    responses: Vec<RetryModelResponse>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::AsyncWriteExt as _;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("retry listener");
    let address = listener.local_addr().expect("retry listener address");
    let task = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(responses.len());
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("retry accept");
            requests.push(
                String::from_utf8(read_scripted_http_request(&mut socket).await)
                    .expect("UTF-8 retry request"),
            );
            match response {
                RetryModelResponse::Status(status) => {
                    let body = "{}";
                    let response = format!(
                        "HTTP/1.1 {status} Retryable Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("retry error response");
                }
                RetryModelResponse::PartialError => {
                    write_scripted_sse(&mut socket, &scripted_partial_error_body()).await;
                }
                RetryModelResponse::Success => {
                    write_scripted_sse(&mut socket, &scripted_text_body("fallback success")).await;
                }
            }
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) async fn assert_retry_budget_and_fallback(
    status: u16,
    expected_attempts_on_first: usize,
) {
    let retry = ModelRetryConfig {
        backoff_ceiling_ms: 1,
        ..ModelRetryConfig::default()
    };
    let mut responses = vec![RetryModelResponse::Status(status); expected_attempts_on_first];
    responses.push(RetryModelResponse::Success);
    let (endpoint, captured) = retry_model_server(responses).await;
    let (fixture, selection) = retry_fixture_with_endpoint(&endpoint, retry).await;
    fixture
        .engine
        .inner
        .test_hooks
        .model_retry_sleep_hook
        .set_mode(ModelRetrySleepMode::Immediate);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new(format!("retry-budget-{status}")).unwrap(),
                selection,
                input: "exercise retry budget".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    let projection = await_projection(
        &fixture.engine,
        session.session_id,
        "retry fallback completion",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;
    let delays = fixture
        .engine
        .inner
        .test_hooks
        .model_retry_sleep_hook
        .delays();
    assert_eq!(delays.len(), expected_attempts_on_first - 1);
    assert!(
        delays
            .iter()
            .all(|delay| *delay == std::time::Duration::from_millis(1))
    );

    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("retry requests");
    assert_eq!(requests.len(), expected_attempts_on_first + 1);
    assert!(
        requests[..expected_attempts_on_first]
            .iter()
            .all(|request| request_body(request)["model"] == "group/model")
    );
    assert_eq!(
        request_body(requests.last().unwrap())["model"],
        "group/fallback"
    );
    let events = projection.log.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::AttemptAbandoned { .. }))
            .count(),
        expected_attempts_on_first
    );
    for model_error in events.iter().filter_map(|event| match &event.payload {
        EventPayload::AttemptAbandoned { model_error, .. } => Some(model_error),
        _ => None,
    }) {
        let error = model_error
            .as_ref()
            .expect("abandoned attempt records its cause");
        assert_eq!(error.http_status, Some(status));
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.payload,
                EventPayload::ModelAttemptStarted {
                    fallback_index: 0,
                    ..
                }
            ))
            .count(),
        expected_attempts_on_first
    );
    assert!(events.iter().any(|event| matches!(
        event.payload,
        EventPayload::ModelFallback {
            attempts_on_from,
            ..
        } if attempts_on_from as usize == expected_attempts_on_first
    )));
    fixture.engine.shutdown().await;
}

pub(crate) async fn scripted_channel_server(
    expected_requests: usize,
) -> (
    String,
    tokio::sync::mpsc::UnboundedSender<MatchedScriptedResponse>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("channel script listener");
    let address = listener.local_addr().expect("listener address");
    let (responses, mut response_rx) =
        tokio::sync::mpsc::unbounded_channel::<MatchedScriptedResponse>();
    let task = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(expected_requests);
        let mut pending_responses = Vec::<MatchedScriptedResponse>::new();
        for _ in 0..expected_requests {
            let (mut socket, request) =
                accept_scripted_planned_request(&listener, "channel script accept").await;
            let body = loop {
                if let Some(index) = pending_responses
                    .iter()
                    .position(|response| response.matches(&request))
                {
                    break pending_responses.remove(index).body;
                }
                pending_responses.push(
                    response_rx
                        .recv()
                        .await
                        .expect("matching channel script response"),
                );
            };
            requests.push(String::from_utf8(request).expect("channel script request"));
            write_scripted_sse(&mut socket, &body).await;
        }
        spawn_scripted_auxiliary_tail(listener);
        requests
    });
    (format!("http://{address}/v1"), responses, task)
}

pub(crate) async fn parallel_delegate_server() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("parallel delegate listener");
    let address = listener.local_addr().expect("listener address");
    let (children_reached_tx, children_reached_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let task_release = Arc::clone(&release);
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        let (mut parent, _) = listener.accept().await.expect("parent request");
        requests.push(
            String::from_utf8(read_scripted_http_request(&mut parent).await)
                .expect("parent request UTF-8"),
        );
        write_scripted_sse(
            &mut parent,
            &scripted_tool_batch_body(&[
                (
                    "delegate-one",
                    "delegate_subagent",
                    serde_json::json!({
                        "agent_type":"worker",
                        "description":"First child",
                        "prompt":"parallel child one"
                    }),
                ),
                (
                    "delegate-two",
                    "delegate_subagent",
                    serde_json::json!({
                        "agent_type":"worker",
                        "description":"Second child",
                        "prompt":"parallel child two"
                    }),
                ),
            ]),
        )
        .await;

        let mut children = Vec::new();
        for _ in 0..2 {
            let (mut child, _) = listener.accept().await.expect("child request");
            requests.push(
                String::from_utf8(read_scripted_http_request(&mut child).await)
                    .expect("child request UTF-8"),
            );
            children.push(child);
        }
        let _ = children_reached_tx.send(());
        task_release.notified().await;
        for (index, child) in children.iter_mut().enumerate() {
            write_scripted_sse(
                child,
                &scripted_text_body(&format!("child {index} complete")),
            )
            .await;
        }

        let (mut parent_followup, _) = listener.accept().await.expect("parent follow-up");
        requests.push(
            String::from_utf8(read_scripted_http_request(&mut parent_followup).await)
                .expect("parent follow-up UTF-8"),
        );
        write_scripted_sse(
            &mut parent_followup,
            &scripted_text_body("parallel delegation complete"),
        )
        .await;
        requests
    });
    (
        format!("http://{address}/v1"),
        children_reached_rx,
        release,
        task,
    )
}

pub(crate) struct MatchedScriptedResponse {
    pub(crate) matcher: ScriptedRequestMatcher,
    pub(crate) body: String,
}

pub(crate) enum ScriptedRequestMatcher {
    LastMessageRole(String),
    LastMessageContains(String),
}

impl MatchedScriptedResponse {
    pub(crate) fn last_message_role(role: &str, body: String) -> Self {
        Self {
            matcher: ScriptedRequestMatcher::LastMessageRole(role.into()),
            body,
        }
    }

    pub(crate) fn last_message_contains(text: &str, body: String) -> Self {
        Self {
            matcher: ScriptedRequestMatcher::LastMessageContains(text.into()),
            body,
        }
    }

    pub(crate) fn matches(&self, request: &[u8]) -> bool {
        let Some(last_message) = scripted_effective_last_message(request) else {
            return false;
        };
        match &self.matcher {
            ScriptedRequestMatcher::LastMessageRole(role) => {
                last_message.get("role").and_then(serde_json::Value::as_str) == Some(role)
            }
            ScriptedRequestMatcher::LastMessageContains(text) => {
                last_message.to_string().contains(text)
            }
        }
    }
}

pub(crate) fn scripted_request_messages(request: &[u8]) -> Option<Vec<serde_json::Value>> {
    let body_start = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?
        + 4;
    serde_json::from_slice::<serde_json::Value>(&request[body_start..])
        .ok()?
        .get("messages")?
        .as_array()
        .cloned()
}

pub(crate) fn scripted_message_is_subagent_notification(message: &serde_json::Value) -> bool {
    if message.get("role").and_then(serde_json::Value::as_str) != Some("user") {
        return false;
    }
    let text = match message.get("content") {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
            .collect::<String>(),
        _ => return false,
    };
    let text = text.trim();
    text.starts_with("<subagent_notification>") && text.ends_with("</subagent_notification>")
}

pub(crate) fn scripted_effective_last_message(request: &[u8]) -> Option<serde_json::Value> {
    scripted_request_messages(request)?
        .into_iter()
        .rev()
        .find(|message| !scripted_message_is_subagent_notification(message))
}

pub(crate) fn scripted_is_auxiliary_subagent_notification(request: &[u8]) -> bool {
    let Some(messages) = scripted_request_messages(request) else {
        return false;
    };
    messages
        .last()
        .is_some_and(scripted_message_is_subagent_notification)
        && messages
            .iter()
            .rev()
            .find(|message| !scripted_message_is_subagent_notification(message))
            .and_then(|message| message.get("role"))
            .and_then(serde_json::Value::as_str)
            == Some("assistant")
}

pub(crate) fn scripted_text_body(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n",
        serde_json::json!({"choices":[{"delta":{"content":text},"finish_reason":null}]})
    )
}

pub(crate) fn scripted_partial_error_body() -> String {
    format!(
        "data: {}\n\ndata: {}\n\n",
        serde_json::json!({"choices":[{"delta":{"content":"partial "},"finish_reason":null}]}),
        serde_json::json!({"error":{"code":500,"message":"mid-stream provider failure"}})
    )
}

pub(crate) fn scripted_tool_body(id: &str, name: &str, arguments: serde_json::Value) -> String {
    let call = serde_json::json!({
        "index":0,
        "id":id,
        "type":"function",
        "function":{"name":name,"arguments":arguments.to_string()}
    });
    format!(
        "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n",
        serde_json::json!({"choices":[{"delta":{"tool_calls":[call]},"finish_reason":null}]})
    )
}

pub(crate) fn scripted_tool_batch_body(calls: &[(&str, &str, serde_json::Value)]) -> String {
    let calls = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            serde_json::json!({
                "index":index,
                "id":id,
                "type":"function",
                "function":{"name":name,"arguments":arguments.to_string()}
            })
        })
        .collect::<Vec<_>>();
    format!(
        "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n",
        serde_json::json!({"choices":[{"delta":{"tool_calls":calls},"finish_reason":null}]})
    )
}

pub(crate) fn scripted_text_usage_body(
    text: &str,
    input: u64,
    output: Option<u64>,
    cache_read: u64,
) -> String {
    let mut usage = serde_json::json!({
        "prompt_tokens": input,
        "prompt_tokens_details": {"cached_tokens": cache_read},
        "total_tokens": input + output.unwrap_or_default(),
    });
    if let Some(output) = output {
        usage["completion_tokens"] = serde_json::json!(output);
    }
    format!(
        "data: {}\n\ndata: {}\n\n",
        serde_json::json!({"choices":[{"delta":{"content":text},"finish_reason":null}]}),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":usage}),
    )
}

pub(crate) fn scripted_text_cache_write_usage_body(
    text: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
) -> String {
    format!(
        "data: {}\n\ndata: {}\n\n",
        serde_json::json!({"choices":[{"delta":{"content":text},"finish_reason":null}]}),
        serde_json::json!({
            "choices":[{"delta":{},"finish_reason":"stop"}],
            "usage": {
                "prompt_tokens": input,
                "prompt_tokens_details": {
                    "cached_tokens": cache_read,
                    "cache_write_tokens": cache_write,
                },
                "completion_tokens": output,
                "total_tokens": input + output,
            }
        }),
    )
}

pub(crate) fn scripted_qwen_usage_body(text: &str, cache_read: Option<u64>) -> String {
    let prompt_tokens_details = cache_read.map_or(
        serde_json::Value::Null,
        |cached_tokens| serde_json::json!({"cached_tokens": cached_tokens}),
    );
    format!(
        "data: {}\n\ndata: {}\n\n",
        serde_json::json!({"choices":[{"delta":{"content":text},"finish_reason":null}]}),
        serde_json::json!({
            "choices":[{"delta":{},"finish_reason":"stop"}],
            "usage": {
                "prompt_tokens": 1264,
                "total_tokens": 1294,
                "completion_tokens": 30,
                "prompt_tokens_details": prompt_tokens_details,
                "reasoning_tokens": 29,
            }
        }),
    )
}

pub(crate) fn scripted_tool_usage_body(
    id: &str,
    arguments: serde_json::Value,
    input: u64,
    output: u64,
    cache_read: u64,
) -> String {
    let call = serde_json::json!({
        "index": 0,
        "id": id,
        "type": "function",
        "function": {
            "name": "delegate_subagent",
            "arguments": arguments.to_string(),
        }
    });
    format!(
        "data: {}\n\ndata: {}\n\n",
        serde_json::json!({"choices":[{"delta":{"tool_calls":[call]},"finish_reason":null}]}),
        serde_json::json!({
            "choices":[{"delta":{},"finish_reason":"tool_calls"}],
            "usage": {
                "prompt_tokens": input,
                "prompt_tokens_details": {"cached_tokens": cache_read},
                "completion_tokens": output,
                "total_tokens": input + output,
            }
        }),
    )
}

pub(crate) async fn scripted_queued_delegation_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("queued delegation listener");
    let address = listener.local_addr().expect("listener address");
    let task = tokio::spawn(async move {
        let (mut initial, _) =
            accept_scripted_planned_request(&listener, "queued parent initial").await;
        let calls = (0..5)
            .map(|index| {
                serde_json::json!({
                    "index": index,
                    "id": format!("queued-delegate-{index}"),
                    "type": "function",
                    "function": {
                        "name": "delegate_subagent",
                        "arguments": serde_json::json!({
                            "agent_type":"worker",
                            "description":format!("Child {index}"),
                            "prompt":format!("queued child {index}"),
                            "background":true
                        }).to_string()
                    }
                })
            })
            .collect::<Vec<_>>();
        let initial_body = format!(
            "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n",
            serde_json::json!({"choices":[{"delta":{"tool_calls":calls},"finish_reason":null}]})
        );
        write_scripted_sse(&mut initial, &initial_body).await;

        let mut children = Vec::new();
        let mut parent_responded = false;
        while children.len() < 4 || !parent_responded {
            let (mut socket, request) =
                accept_scripted_planned_request(&listener, "queued concurrent request").await;
            if MatchedScriptedResponse::last_message_role("tool", String::new()).matches(&request) {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"parent admitted all children\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
                write_scripted_sse(&mut socket, body).await;
                parent_responded = true;
            } else {
                children.push(socket);
            }
        }
        for (index, child) in children.iter_mut().enumerate() {
            let body = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"child {index} done\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n"
            );
            write_scripted_sse(child, &body).await;
        }
        let (mut queued, request) =
            accept_scripted_planned_request(&listener, "queued child start").await;
        assert!(
            scripted_effective_last_message(&request)
                .is_some_and(|message| message.to_string().contains("queued child"))
        );
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"queued child done\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        write_scripted_sse(&mut queued, body).await;
        spawn_scripted_auxiliary_tail(listener);
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) async fn scripted_full_delegation_queue_server() -> (String, tokio::task::JoinHandle<()>)
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("full delegation queue listener");
    let address = listener.local_addr().expect("listener address");
    let task = tokio::spawn(async move {
        let (mut initial, _) = listener.accept().await.expect("full queue parent initial");
        let _ = read_scripted_http_request(&mut initial).await;
        let calls = (0..21)
            .map(|index| {
                serde_json::json!({
                    "index": index,
                    "id": format!("full-queue-delegate-{index}"),
                    "type": "function",
                    "function": {
                        "name": "delegate_subagent",
                        "arguments": serde_json::json!({
                            "agent_type":"worker",
                            "description":format!("Full queue child {index}"),
                            "prompt":format!("full queue child {index}"),
                            "background":true
                        }).to_string()
                    }
                })
            })
            .collect::<Vec<_>>();
        let initial_body = format!(
            "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n",
            serde_json::json!({"choices":[{"delta":{"tool_calls":calls},"finish_reason":null}]})
        );
        write_scripted_sse(&mut initial, &initial_body).await;

        let mut children = Vec::new();
        let mut parent_responded = false;
        while children.len() < 4 || !parent_responded {
            let (mut socket, _) = listener.accept().await.expect("full queue request");
            let request = read_scripted_http_request(&mut socket).await;
            if String::from_utf8_lossy(&request).contains("\"role\":\"tool\"") {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"queue full rejection observed\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
                write_scripted_sse(&mut socket, body).await;
                parent_responded = true;
            } else {
                children.push(socket);
            }
        }
        std::future::pending::<()>().await;
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) async fn scripted_startup_failure_delegation_server()
-> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("startup failure delegation listener");
    let address = listener.local_addr().expect("listener address");
    let task = tokio::spawn(async move {
        let (mut initial, _) = listener
            .accept()
            .await
            .expect("startup failure parent initial");
        let _ = read_scripted_http_request(&mut initial).await;
        let calls = (0..5)
            .map(|index| {
                serde_json::json!({
                    "index": index,
                    "id": format!("startup-failure-delegate-{index}"),
                    "type": "function",
                    "function": {
                        "name": "delegate_subagent",
                        "arguments": serde_json::json!({
                            "agent_type":"worker",
                            "description":format!("Startup child {index}"),
                            "prompt":format!("startup child {index}"),
                            "background":true
                        }).to_string()
                    }
                })
            })
            .collect::<Vec<_>>();
        let initial_body = format!(
            "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n",
            serde_json::json!({"choices":[{"delta":{"tool_calls":calls},"finish_reason":null}]})
        );
        write_scripted_sse(&mut initial, &initial_body).await;

        let mut children = Vec::new();
        let mut parent_responded = false;
        while children.len() < 4 || !parent_responded {
            let (mut socket, _) = listener.accept().await.expect("startup failure request");
            let request = read_scripted_http_request(&mut socket).await;
            if String::from_utf8_lossy(&request).contains("\"role\":\"tool\"") {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"parent observed startup failure\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
                write_scripted_sse(&mut socket, body).await;
                parent_responded = true;
            } else {
                children.push(socket);
            }
        }
        for (index, child) in children.iter_mut().enumerate() {
            let body = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"startup child {index} done\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n"
            );
            write_scripted_sse(child, &body).await;
        }
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) async fn scripted_cancellable_delegation_server() -> (String, tokio::task::JoinHandle<()>)
{
    use tokio::io::AsyncReadExt as _;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("cancellable delegation listener");
    let address = listener.local_addr().expect("listener address");
    let task = tokio::spawn(async move {
        let (mut initial, _) = listener.accept().await.expect("cancellable parent initial");
        let _ = read_scripted_http_request(&mut initial).await;
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"cancellable-delegate\",\"type\":\"function\",\"function\":{\"name\":\"delegate_subagent\",\"arguments\":\"{\\\"agent_type\\\":\\\"worker\\\",\\\"description\\\":\\\"Long task\\\",\\\"prompt\\\":\\\"long running child\\\",\\\"background\\\":true}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
        write_scripted_sse(&mut initial, body).await;

        let mut child = None;
        let mut parent_responded = false;
        while child.is_none() || !parent_responded {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("cancellable concurrent request");
            let request = read_scripted_http_request(&mut socket).await;
            if String::from_utf8_lossy(&request).contains("\"role\":\"tool\"") {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"parent continued\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
                write_scripted_sse(&mut socket, body).await;
                parent_responded = true;
            } else {
                child = Some(socket);
            }
        }
        let mut child = child.expect("child socket");
        let mut buffer = [0_u8; 256];
        while child.read(&mut buffer).await.unwrap_or(0) != 0 {}
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) async fn scripted_finished_wake_server() -> (String, tokio::task::JoinHandle<Vec<String>>)
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("finished wake listener");
    let address = listener.local_addr().expect("listener address");
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        let (mut parent, _) = listener.accept().await.expect("wake parent initial");
        requests.push(
            String::from_utf8(read_scripted_http_request(&mut parent).await)
                .expect("wake parent request"),
        );
        write_scripted_sse(
            &mut parent,
            &scripted_tool_body(
                "finished-wake-delegate",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type":"worker",
                    "description":"Wake child",
                    "prompt":"child turn one work",
                    "background":false
                }),
            ),
        )
        .await;

        let (mut child_one, _) = listener.accept().await.expect("child first turn");
        requests.push(
            String::from_utf8(read_scripted_http_request(&mut child_one).await)
                .expect("child first request"),
        );
        write_scripted_sse(&mut child_one, &scripted_text_body("child turn one")).await;

        let (mut parent_two, _) = listener.accept().await.expect("parent resumed");
        requests.push(
            String::from_utf8(read_scripted_http_request(&mut parent_two).await)
                .expect("parent resumed request"),
        );
        write_scripted_sse(
            &mut parent_two,
            &scripted_text_body("parent first run done"),
        )
        .await;

        let (mut child_two, _) = listener.accept().await.expect("child wake turn");
        requests.push(
            String::from_utf8(read_scripted_http_request(&mut child_two).await)
                .expect("child wake request"),
        );
        write_scripted_sse(&mut child_two, &scripted_text_body("child turn two")).await;
        requests
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) async fn scripted_running_steer_server() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("running steer listener");
    let address = listener.local_addr().expect("listener address");
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut initial, _) = listener.accept().await.expect("steer parent initial");
        let mut requests = vec![
            String::from_utf8(read_scripted_http_request(&mut initial).await)
                .expect("parent request"),
        ];
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"steer-delegate\",\"type\":\"function\",\"function\":{\"name\":\"delegate_subagent\",\"arguments\":\"{\\\"agent_type\\\":\\\"worker\\\",\\\"description\\\":\\\"Steer child\\\",\\\"prompt\\\":\\\"begin child work\\\",\\\"background\\\":true}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
        write_scripted_sse(&mut initial, body).await;

        let mut child = None;
        let mut reached_tx = Some(reached_tx);
        let mut parent_responded = false;
        while child.is_none() || !parent_responded {
            let (mut socket, _) = listener.accept().await.expect("steer concurrent request");
            let request = String::from_utf8(read_scripted_http_request(&mut socket).await)
                .expect("steer request");
            if request.contains("\"role\":\"tool\"") {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"parent continued\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
                write_scripted_sse(&mut socket, body).await;
                parent_responded = true;
            } else {
                requests.push(request);
                child = Some(socket);
                if let Some(reached_tx) = reached_tx.take() {
                    let _ = reached_tx.send(());
                }
            }
        }
        let _ = release_rx.await;
        let mut child = child.expect("running child socket");
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"initial child pass\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        write_scripted_sse(&mut child, body).await;

        let (mut steered, _) = listener.accept().await.expect("steered child request");
        requests.push(
            String::from_utf8(read_scripted_http_request(&mut steered).await)
                .expect("steered request"),
        );
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"steered child done\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        write_scripted_sse(&mut steered, body).await;
        requests
    });
    (format!("http://{address}/v1"), reached_rx, release_tx, task)
}

pub(crate) async fn scripted_running_resume_server() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<SessionId>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("running resume listener");
    let address = listener.local_addr().expect("listener address");
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut initial, _) = listener.accept().await.expect("resume parent initial");
        let mut requests = vec![
            String::from_utf8(read_scripted_http_request(&mut initial).await)
                .expect("resume parent request"),
        ];
        write_scripted_sse(
            &mut initial,
            &scripted_tool_body(
                "running-resume-fresh",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type":"worker",
                    "description":"Running resume child",
                    "prompt":"initial active prompt",
                    "background":true
                }),
            ),
        )
        .await;

        let mut child = None;
        let mut parent_responded = false;
        while child.is_none() || !parent_responded {
            let (mut socket, _) = listener.accept().await.expect("running resume request");
            let request = String::from_utf8(read_scripted_http_request(&mut socket).await)
                .expect("running resume request text");
            requests.push(request.clone());
            if request.contains("\"role\":\"tool\"") {
                write_scripted_sse(&mut socket, &scripted_text_body("parent first run done")).await;
                parent_responded = true;
            } else {
                child = Some(socket);
            }
        }
        let _ = ready_tx.send(());
        let resume_session_id = resume_rx.await.expect("resume session ID");
        let (mut second_parent, _) = listener.accept().await.expect("second resume parent");
        requests.push(
            String::from_utf8(read_scripted_http_request(&mut second_parent).await)
                .expect("second resume parent request"),
        );
        write_scripted_sse(
            &mut second_parent,
            &scripted_tool_body(
                "running-resume-existing",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type":"worker",
                    "description":"Continue running child",
                    "prompt":"resume active prompt",
                    "background":true,
                    "resume_session_id":resume_session_id
                }),
            ),
        )
        .await;

        let _ = release_rx.await;
        let mut child = child.expect("held child request");
        write_scripted_sse(&mut child, &scripted_text_body("initial child pass")).await;
        let mut parent_done = false;
        let mut child_done = false;
        while !parent_done || !child_done {
            let (mut socket, _) = listener.accept().await.expect("resumed completion request");
            let request = String::from_utf8(read_scripted_http_request(&mut socket).await)
                .expect("resumed completion request text");
            requests.push(request.clone());
            if request.contains("\"role\":\"tool\"") {
                write_scripted_sse(&mut socket, &scripted_text_body("parent resumed child")).await;
                parent_done = true;
            } else {
                write_scripted_sse(&mut socket, &scripted_text_body("resumed child done")).await;
                child_done = true;
            }
        }
        requests
    });
    (
        format!("http://{address}/v1"),
        ready_rx,
        resume_tx,
        release_tx,
        task,
    )
}

pub(crate) async fn scripted_queued_resume_server() -> (
    String,
    tokio::sync::oneshot::Sender<SessionId>,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("queued resume listener");
    let address = listener.local_addr().expect("listener address");
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let (queued_tx, queued_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        let (mut first_parent, _) = listener.accept().await.expect("queued resume first parent");
        requests.push(
            String::from_utf8(read_scripted_http_request(&mut first_parent).await)
                .expect("first parent request"),
        );
        write_scripted_sse(
            &mut first_parent,
            &scripted_tool_body(
                "queued-resume-origin",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type":"worker",
                    "description":"Queue resume identity",
                    "prompt":"create terminal resume target",
                    "background":true
                }),
            ),
        )
        .await;
        for text in ["terminal target complete", "first parent complete"] {
            let (mut socket, request) =
                accept_scripted_planned_request(&listener, "first resume phase request").await;
            requests.push(String::from_utf8(request).expect("first resume phase request text"));
            write_scripted_sse(&mut socket, &scripted_text_body(text)).await;
        }

        let mut resume_rx = resume_rx;
        let mut accepted_second_parent = None;
        let resume_session_id = loop {
            tokio::select! {
                resume_session_id = &mut resume_rx => {
                    break resume_session_id.expect("queued resume session ID");
                }
                accepted = listener.accept() => {
                    let (mut socket, _) = accepted.expect("queued resume continuation");
                    let request = read_scripted_http_request(&mut socket).await;
                    if scripted_is_auxiliary_subagent_notification(&request) {
                        write_scripted_sse(
                            &mut socket,
                            &scripted_text_body("auxiliary subagent notification accepted"),
                        )
                        .await;
                    } else {
                        accepted_second_parent = Some((socket, request));
                        break resume_rx.await.expect("queued resume session ID");
                    }
                }
            }
        };
        let (mut second_parent, request) = match accepted_second_parent {
            Some(request) => request,
            None => accept_scripted_planned_request(&listener, "queued resume second parent").await,
        };
        requests.push(String::from_utf8(request).expect("second parent request"));
        let calls = [
            serde_json::json!({
                "index":0,
                "id":"queued-resume-slot-holder",
                "type":"function",
                "function":{
                    "name":"delegate_subagent",
                    "arguments":serde_json::json!({
                        "agent_type":"worker",
                        "description":"Slot holder",
                        "prompt":"hold the only slot",
                        "background":true
                    }).to_string()
                }
            }),
            serde_json::json!({
                "index":1,
                "id":"queued-resume-target",
                "type":"function",
                "function":{
                    "name":"delegate_subagent",
                    "arguments":serde_json::json!({
                        "agent_type":"worker",
                        "description":"Queued resumed child",
                        "prompt":"resume after slot release",
                        "background":true,
                        "resume_session_id":resume_session_id
                    }).to_string()
                }
            }),
            serde_json::json!({
                "index":2,
                "id":"queued-resume-duplicate",
                "type":"function",
                "function":{
                    "name":"delegate_subagent",
                    "arguments":serde_json::json!({
                        "agent_type":"worker",
                        "description":"Duplicate queued resume",
                        "prompt":"must be rejected while resume is queued",
                        "background":true,
                        "resume_session_id":resume_session_id
                    }).to_string()
                }
            }),
        ];
        let mut slot_holder = None;
        // Parallel calls need not enter delegation admission in model order. Wait for
        // each tool result before requesting the resume and then its duplicate.
        for mut call in calls {
            call["index"] = serde_json::json!(0);
            let body = format!(
                "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[call]},"finish_reason":null}]})
            );
            write_scripted_sse(&mut second_parent, &body).await;
            let mut next_parent = None;
            while slot_holder.is_none() || next_parent.is_none() {
                let (socket, request) =
                    accept_scripted_planned_request(&listener, "queued resume admission request")
                        .await;
                let request =
                    String::from_utf8(request).expect("queued resume admission request text");
                requests.push(request.clone());
                if MatchedScriptedResponse::last_message_role("tool", String::new())
                    .matches(request.as_bytes())
                {
                    assert!(next_parent.replace(socket).is_none());
                } else {
                    assert!(
                        MatchedScriptedResponse::last_message_contains(
                            "hold the only slot",
                            String::new(),
                        )
                        .matches(request.as_bytes())
                    );
                    assert!(slot_holder.replace(socket).is_none());
                }
            }
            second_parent = next_parent.expect("parent continuation after delegation");
        }
        write_scripted_sse(
            &mut second_parent,
            &scripted_text_body("parent queued resume"),
        )
        .await;
        let _ = queued_tx.send(());
        let _ = release_rx.await;
        let mut slot_holder = slot_holder.expect("slot holder request");
        write_scripted_sse(&mut slot_holder, &scripted_text_body("slot holder done")).await;
        let (mut resumed, request) =
            accept_scripted_planned_request(&listener, "queued resumed child request").await;
        requests.push(String::from_utf8(request).expect("queued resumed child request text"));
        write_scripted_sse(&mut resumed, &scripted_text_body("queued resume done")).await;
        let (mut steered, request) =
            accept_scripted_planned_request(&listener, "queued resume steer request").await;
        requests.push(String::from_utf8(request).expect("queued resume steer request text"));
        write_scripted_sse(
            &mut steered,
            &scripted_text_body("queued resume correction done"),
        )
        .await;
        spawn_scripted_auxiliary_tail(listener);
        requests
    });
    (
        format!("http://{address}/v1"),
        resume_tx,
        queued_rx,
        release_tx,
        task,
    )
}

pub(crate) async fn scripted_queued_steer_recovery_server() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("queued steer listener");
    let address = listener.local_addr().expect("listener address");
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut initial, _) = listener
            .accept()
            .await
            .expect("queued steer parent initial");
        let _ = read_scripted_http_request(&mut initial).await;
        let calls = (0..5)
            .map(|index| {
                serde_json::json!({
                    "index": index,
                    "id": format!("queued-steer-delegate-{index}"),
                    "type": "function",
                    "function": {
                        "name": "delegate_subagent",
                        "arguments": serde_json::json!({
                            "agent_type":"worker",
                            "description":format!("Queued steer child {index}"),
                            "prompt":format!("queued steer child {index}"),
                            "background":true
                        }).to_string()
                    }
                })
            })
            .collect::<Vec<_>>();
        let body = format!(
            "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n",
            serde_json::json!({"choices":[{"delta":{"tool_calls":calls},"finish_reason":null}]})
        );
        write_scripted_sse(&mut initial, &body).await;

        let mut children = Vec::new();
        let mut parent_responded = false;
        while children.len() < 4 || !parent_responded {
            let (mut socket, _) = listener.accept().await.expect("queued steer request");
            let request = read_scripted_http_request(&mut socket).await;
            if String::from_utf8_lossy(&request).contains("\"role\":\"tool\"") {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"parent queued children\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
                write_scripted_sse(&mut socket, body).await;
                parent_responded = true;
            } else {
                children.push(socket);
            }
        }
        let _ = reached_tx.send(());
        let _ = release_rx.await;
        drop(children);

        let (mut queued, first) = loop {
            let (mut socket, _) = listener.accept().await.expect("recovered queued child");
            let request = String::from_utf8(read_scripted_http_request(&mut socket).await)
                .expect("queued initial request");
            if request.contains("Worker prompt.") {
                break (socket, request);
            }
            write_scripted_sse(
                &mut socket,
                &scripted_text_body("parent accepted recovered child completion"),
            )
            .await;
        };
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"queued initial pass\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        write_scripted_sse(&mut queued, body).await;

        let (mut steered, second) = loop {
            let (mut socket, _) = listener.accept().await.expect("recovered steer request");
            let request = String::from_utf8(read_scripted_http_request(&mut socket).await)
                .expect("queued steered request");
            if request.contains("Worker prompt.")
                && request.contains("apply this queued correction")
            {
                break (socket, request);
            }
            write_scripted_sse(
                &mut socket,
                &scripted_text_body("parent accepted recovered child completion"),
            )
            .await;
        };
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"queued steer done\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        write_scripted_sse(&mut steered, body).await;
        vec![first, second]
    });
    (format!("http://{address}/v1"), reached_rx, release_tx, task)
}

pub(crate) async fn scripted_approval_server(
    internal_output: &str,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("approval listener");
    let address = listener.local_addr().expect("listener address");
    let internal_delta = serde_json::json!({
        "choices": [{
            "delta": {"content": internal_output},
            "finish_reason": null
        }]
    });
    let bodies = [
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"write-call\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n"
            .to_owned(),
        format!(
            "data: {internal_delta}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n"
        ),
        "data: {\"choices\":[{\"delta\":{\"content\":\"approval flow complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
            .to_owned(),
    ];
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut socket, _) = listener.accept().await.expect("approval accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.expect("approval read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("approval response");
            requests.push(String::from_utf8(request).expect("UTF-8 request"));
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) async fn scripted_server_with_delayed_response(
    bodies: Vec<String>,
    delayed_index: usize,
) -> (
    String,
    tokio::task::JoinHandle<Vec<String>>,
    tokio::sync::oneshot::Receiver<()>,
    Arc<tokio::sync::Notify>,
) {
    scripted_server_with_status_and_delay(
        bodies.into_iter().map(|body| (200, body)).collect(),
        delayed_index,
    )
    .await
}

pub(crate) async fn scripted_server_with_status_and_delay(
    bodies: Vec<(u16, String)>,
    delayed_index: usize,
) -> (
    String,
    tokio::task::JoinHandle<Vec<String>>,
    tokio::sync::oneshot::Receiver<()>,
    Arc<tokio::sync::Notify>,
) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("delayed listener");
    let address = listener.local_addr().expect("listener address");
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let task_release = Arc::clone(&release);
    let task = tokio::spawn(async move {
        let mut reached_tx = Some(reached_tx);
        let mut requests = Vec::new();
        for (index, (status, body)) in bodies.into_iter().enumerate() {
            let (mut socket, _) = listener.accept().await.expect("delayed accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            let expected_len = loop {
                let read = socket.read(&mut buffer).await.expect("delayed read");
                if read == 0 {
                    break request.len();
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let header_end = header_end + 4;
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .or_else(|| line.strip_prefix("Content-Length: "))
                    })
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                break header_end + content_length;
            };
            while request.len() < expected_len {
                let read = socket.read(&mut buffer).await.expect("delayed body read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            if index == delayed_index {
                if let Some(reached_tx) = reached_tx.take() {
                    let _ = reached_tx.send(());
                }
                task_release.notified().await;
            }
            let content_type = if status == 200 {
                "text/event-stream"
            } else {
                "application/json"
            };
            let response = format!(
                "HTTP/1.1 {status} Response\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            requests.push(String::from_utf8(request).expect("UTF-8 request"));
        }
        requests
    });
    (format!("http://{address}/v1"), task, reached_rx, release)
}

pub(crate) async fn scripted_repeated_write_server(
    tool_calls: usize,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("repeated write listener");
    let address = listener.local_addr().expect("listener address");
    let mut bodies = (0..tool_calls)
        .map(|index| {
            format!(
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"write-call-{index}\",\"type\":\"function\",\"function\":{{\"name\":\"write\",\"arguments\":\"{{}}\"}}}}]}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n"
            )
        })
        .collect::<Vec<_>>();
    bodies.push(
        "data: {\"choices\":[{\"delta\":{\"content\":\"permission sequence complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
            .to_owned(),
    );
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut socket, _) = listener.accept().await.expect("repeated write accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.expect("repeated write read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("repeated write response");
            requests.push(String::from_utf8(request).expect("UTF-8 request"));
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) async fn scripted_two_evaluated_writes_server(
    internal_output: &str,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("persistent approval listener");
    let address = listener.local_addr().expect("listener address");
    let tool_call = |index| {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"persistent-write-{index}\",\"type\":\"function\",\"function\":{{\"name\":\"write\",\"arguments\":\"{{}}\"}}}}]}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n"
        )
    };
    let internal_delta = serde_json::json!({
        "choices": [{
            "delta": {"content": internal_output},
            "finish_reason": null
        }]
    });
    let approval = format!(
        "data: {internal_delta}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n"
    );
    let bodies = [
        tool_call(1),
        approval.clone(),
        tool_call(2),
        approval,
        "data: {\"choices\":[{\"delta\":{\"content\":\"persistent approvals complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
    ];
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut socket, _) = listener.accept().await.expect("persistent approval accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let read = socket
                    .read(&mut buffer)
                    .await
                    .expect("persistent approval read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("persistent approval response");
            requests.push(String::from_utf8(request).expect("UTF-8 request"));
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) fn responses_metadata_sse(events: Vec<serde_json::Value>) -> String {
    events
        .into_iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
}

pub(crate) fn responses_text_with_transport_fields(text: &str, phase: Option<&str>) -> String {
    let item = serde_json::json!({"type":"message","id":"message","role":"assistant","phase":phase,"content":[{"type":"output_text","text":text,"annotations":[],"logprobs":null}]});
    responses_metadata_sse(vec![
        serde_json::json!({"type":"response.created","response":{"id":"response","model":"group/model"}}),
        serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"message","role":"assistant","phase":phase,"content":[]}}),
        serde_json::json!({"type":"response.output_text.delta","item_id":"message","output_index":0,"content_index":0,"delta":text}),
        serde_json::json!({"type":"response.output_item.done","output_index":0,"item":item}),
        serde_json::json!({"type":"response.completed","response":{"id":"response","status":"completed","output":[item]}}),
    ])
}

pub(crate) const RESPONSES_REPLAY_CAPABILITIES: &str = "input = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = 8192\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = true\nnative_replay = \"optional\"\nmedia = {}";

pub(crate) fn anthropic_usage_body(
    text: &str,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
) -> String {
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":{input_tokens},\"cache_read_input_tokens\":{cache_read_tokens},\"cache_creation_input_tokens\":{cache_write_tokens}}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":1}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

pub(crate) fn anthropic_tool_body(id: &str, name: &str, arguments: serde_json::Value) -> String {
    [
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n".into(),
        format!(
            "event: content_block_start\ndata: {}\n\n",
            serde_json::json!({
                "type":"content_block_start",
                "index":0,
                "content_block":{"type":"tool_use","id":id,"name":name,"input":arguments}
            })
        ),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".into(),
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{}}\n\n".into(),
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into(),
    ]
    .concat()
}

pub(crate) fn anthropic_thinking_body(signature: Option<&str>) -> String {
    let signature = signature.map_or_else(String::new, |signature| {
        format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::json!({
                "type":"content_block_delta",
                "index":0,
                "delta":{"type":"signature_delta","signature":signature}
            })
        )
    });
    [
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n".into(),
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n".into(),
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reason\"}}\n\n".into(),
        signature,
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".into(),
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\"}}\n\n".into(),
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\n".into(),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n".into(),
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\n".into(),
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into(),
    ]
    .concat()
}

#[derive(Clone, Copy)]
pub(crate) enum AnthropicReplayResponse {
    Thinking(Option<&'static str>),
    Status400,
    Text(&'static str),
    TextWithInputTokens(&'static str, u64),
}

pub(crate) fn anthropic_request_has_unsigned_thinking(request: &str) -> bool {
    request_body(request)["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .any(|block| {
            block["type"] == "thinking"
                && block
                    .get("signature")
                    .is_none_or(|signature| signature.as_str() == Some(""))
        })
}

pub(crate) async fn anthropic_replay_server(
    responses: Vec<AnthropicReplayResponse>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::AsyncWriteExt as _;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Anthropic replay listener");
    let address = listener.local_addr().expect("Anthropic replay address");
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("Anthropic replay accept");
            let request = String::from_utf8(read_scripted_http_request(&mut socket).await)
                .expect("UTF-8 Anthropic replay request");
            let reject = matches!(response, AnthropicReplayResponse::Status400);
            requests.push(request);
            if reject {
                let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"invalid signature"}}"#;
                let wire = format!(
                    "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(wire.as_bytes())
                    .await
                    .expect("Anthropic replay rejection");
            } else {
                let body = match response {
                    AnthropicReplayResponse::Thinking(signature) => {
                        anthropic_thinking_body(signature)
                    }
                    AnthropicReplayResponse::Text(text) => anthropic_usage_body(text, 1, 0, 0),
                    AnthropicReplayResponse::TextWithInputTokens(text, input_tokens) => {
                        anthropic_usage_body(text, input_tokens, 0, 0)
                    }
                    AnthropicReplayResponse::Status400 => unreachable!(),
                };
                write_scripted_sse(&mut socket, &body).await;
            }
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

pub(crate) const ANTHROPIC_REPLAY_CAPABILITIES: &str = "input = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = 4096\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = true\ntemperature = true\ntop_p = true\nseed = false\nnative_replay = \"required\"\nmedia = {}";

pub(crate) fn request_body(request: &str) -> serde_json::Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

pub(crate) fn cache_marker_count(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(object) => {
            usize::from(object.contains_key("cache_control"))
                + object.values().map(cache_marker_count).sum::<usize>()
        }
        serde_json::Value::Array(values) => values.iter().map(cache_marker_count).sum(),
        _ => 0,
    }
}
