use std::time::Duration;

use cookie_agent_protocol::{MessageFrame, Transport, in_process_pair};
use serde_json::json;
use tokio::io::AsyncWriteExt as _;

use super::{Incoming, LineTransport, Peer, RpcError};
use crate::protocol::ToolDeclarationExt as _;
use crate::protocol::{
    self, Empty, ExecuteOutcome, ExecuteResult, MAX_FRAME_BYTES, Outcome, PrepareOutcome,
    PrepareResult, RuntimeSettings, ToolDeclaration, WireToolError,
};
use crate::{ToolCollision, ToolError, ToolSpec};
use cookie_agent_protocol::errors;

const WAIT: Duration = Duration::from_secs(5);

/// A peer on one end and the raw transport of the other.
fn peer_and_raw() -> (Peer, tokio::sync::mpsc::Receiver<Incoming>, impl Transport) {
    let (left, right) = in_process_pair(8);
    let (peer, incoming) = Peer::start(Box::new(left));
    (peer, incoming, right)
}

async fn recv_text(transport: &mut impl Transport) -> String {
    let frame = tokio::time::timeout(WAIT, transport.recv())
        .await
        .expect("frame in time")
        .expect("transport")
        .expect("frame");
    frame.into_text().expect("text frame")
}

#[tokio::test]
async fn requests_and_notifications_round_trip_both_ways() {
    let (left, right) = in_process_pair(8);
    let (daemon, mut daemon_incoming) = Peer::start(Box::new(left));
    let (runtime, mut runtime_incoming) = Peer::start(Box::new(right));

    let answer = tokio::spawn(async move {
        let Some(Incoming::Request(request)) = runtime_incoming.recv().await else {
            panic!("request expected");
        };
        assert_eq!(request.method, "echo");
        let params: serde_json::Value = request.params().expect("params");
        request.respond(&json!({"echo": params})).await.unwrap();
        let Some(Incoming::Notification(notification)) = runtime_incoming.recv().await else {
            panic!("notification expected");
        };
        assert_eq!(notification.method, "note");
        let _: Empty = notification.params().expect("empty params");
        runtime
    });
    let reply: serde_json::Value = daemon.request("echo", &json!({"x": 1})).await.unwrap();
    assert_eq!(reply, json!({"echo": {"x": 1}}));
    daemon.notify("note", &Empty {}).await.unwrap();
    let runtime = answer.await.unwrap();

    // The runtime asks the daemon too, and gets an error answer.
    let ask = tokio::spawn(async move {
        runtime
            .request::<_, serde_json::Value>("tool/progress", &Empty {})
            .await
    });
    let Some(Incoming::Request(request)) = daemon_incoming.recv().await else {
        panic!("request expected");
    };
    request
        .fail(errors::UNKNOWN_HANDLE.error().detail("no such call").into())
        .await
        .unwrap();
    match ask.await.unwrap() {
        Err(error @ RpcError::Remote { code, .. }) => {
            assert_eq!(code, errors::APPLICATION_ERROR);
            assert_eq!(error.data_code(), Some("unknown_handle"));
            assert_eq!(error.detail(), Some("no such call"));
            assert_eq!(
                error.to_string(),
                "The tool call or workspace the request names is no longer open. (no such call)"
            );
        }
        other => panic!("unexpected reply {other:?}"),
    }
}

#[tokio::test]
async fn commands_refuse_what_they_do_not_know_and_information_ignores_it() {
    let (peer, mut incoming, mut raw) = peer_and_raw();
    let tool_call_id = cookie_agent_protocol::ToolCallId::new_v7();
    for (id, method, params, code, field) in [
        (
            7,
            "tool/progress",
            json!({"prepared_id": 1, "progress": {"tool_call_id": tool_call_id, "message": "m", "extra": 1}}),
            "unknown_field",
            "progress.extra",
        ),
        (
            8,
            "tool/progress",
            json!({"prepared_id": 1, "progress": {"tool_call_id": tool_call_id, "message": "m",
                "display": {"type": "rewind", "index": 0}}}),
            "unknown_variant",
            "progress.display.type",
        ),
    ] {
        raw.send(MessageFrame::Text(
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        ))
        .await
        .unwrap();
        let Some(Incoming::Request(request)) = incoming.recv().await else {
            panic!("request expected");
        };
        let error = request
            .command::<protocol::ToolProgressParams>()
            .expect_err("refused");
        assert_eq!(error.data_code(), Some(code));
        request.fail(error).await.unwrap();
        let reply: serde_json::Value = serde_json::from_str(&recv_text(&mut raw).await).unwrap();
        assert_eq!(reply["id"], id);
        assert_eq!(reply["error"]["code"], errors::INVALID_PARAMS);
        assert_eq!(reply["error"]["data"]["code"], code);
        assert_eq!(reply["error"]["data"]["field"], field, "{reply}");
    }
    // The same request read as information ignores the member.
    raw.send(MessageFrame::Text(
        r#"{"jsonrpc":"2.0","id":9,"method":"runtime/context","params":{"instance":"i","preset":null,"extra":1}}"#
            .into(),
    ))
    .await
    .unwrap();
    let Some(Incoming::Request(request)) = incoming.recv().await else {
        panic!("request expected");
    };
    let context: protocol::ContextParams = request.params().expect("information");
    assert_eq!(context.instance, "i");
    // Malformed params are `invalid_params`, with serde's message as detail.
    let error = request
        .params::<protocol::PreparedRef>()
        .expect_err("malformed");
    let RpcError::Remote { code, .. } = &error else {
        panic!("remote error expected");
    };
    assert_eq!(*code, errors::INVALID_PARAMS);
    assert_eq!(error.data_code(), Some("invalid_params"));
    assert!(
        error
            .detail()
            .is_some_and(|detail| detail.contains("prepared_id"))
    );
    assert!(!peer.is_closed());
}

#[tokio::test]
async fn the_envelope_ignores_unknown_members_and_takes_string_ids() {
    let (peer, mut incoming, mut raw) = peer_and_raw();
    let pending = tokio::spawn({
        let peer = peer.clone();
        async move { peer.request::<_, Empty>("runtime/context", &Empty {}).await }
    });
    let request: serde_json::Value = serde_json::from_str(&recv_text(&mut raw).await).unwrap();
    let id = request["id"].as_u64().expect("this peer sends integer IDs");
    raw.send(MessageFrame::Text(format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{{}},"extra":true}}"#
    )))
    .await
    .unwrap();
    assert!(matches!(
        tokio::time::timeout(WAIT, pending).await.unwrap().unwrap(),
        Ok(Empty {})
    ));
    for (id, text) in [(json!("call-1"), r#""call-1""#), (json!(-4), "-4")] {
        raw.send(MessageFrame::Text(format!(
            r#"{{"jsonrpc":"2.0","id":{text},"method":"echo","params":{{}},"trace":"x"}}"#
        )))
        .await
        .unwrap();
        let Some(Incoming::Request(request)) = incoming.recv().await else {
            panic!("request expected");
        };
        request.respond(&json!({"ok": true})).await.unwrap();
        let reply: serde_json::Value = serde_json::from_str(&recv_text(&mut raw).await).unwrap();
        assert_eq!(reply["id"], id);
        assert_eq!(reply["result"], json!({"ok": true}));
    }
    // A reply to an ID this peer never sent is dropped.
    raw.send(MessageFrame::Text(
        r#"{"jsonrpc":"2.0","id":"call-9","result":{}}"#.into(),
    ))
    .await
    .unwrap();
    raw.send(MessageFrame::Text(
        r#"{"jsonrpc":"2.0","method":"note","params":{},"extra":[1]}"#.into(),
    ))
    .await
    .unwrap();
    let Some(Incoming::Notification(notification)) = incoming.recv().await else {
        panic!("notification expected");
    };
    assert_eq!(notification.method, "note");
    assert!(!peer.is_closed());
}

#[tokio::test]
async fn a_malformed_envelope_ends_the_connection_and_fails_pending_requests() {
    for frame in [
        // Wrong JSON-RPC version.
        r#"{"jsonrpc":"1.0","id":1,"result":{}}"#,
        // A reply with both result and error.
        r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":1,"message":"x"}}"#,
        // Neither request, notification, nor reply.
        r#"{"jsonrpc":"2.0"}"#,
        "not json",
    ] {
        let (peer, _incoming, mut raw) = peer_and_raw();
        let pending = tokio::spawn({
            let peer = peer.clone();
            async move { peer.request::<_, Empty>("runtime/context", &Empty {}).await }
        });
        let _request = recv_text(&mut raw).await;
        raw.send(MessageFrame::Text(frame.into())).await.unwrap();
        let reply = tokio::time::timeout(WAIT, pending).await.unwrap().unwrap();
        assert!(
            matches!(reply, Err(RpcError::Closed(_))),
            "{frame}: {reply:?}"
        );
        let reason = tokio::time::timeout(WAIT, peer.closed()).await.unwrap();
        assert!(
            ["frame", "message", "reply"]
                .iter()
                .any(|word| reason.contains(word)),
            "{reason}"
        );
    }
}

#[tokio::test]
async fn oversize_frames_are_refused_both_ways() {
    let (peer, _incoming, mut raw) = peer_and_raw();
    // Sending: the request fails, the connection stays up.
    let oversize = "x".repeat(MAX_FRAME_BYTES);
    let error = peer
        .request::<_, Empty>("tool/stdin", &json!({"data": oversize}))
        .await
        .expect_err("oversize request");
    assert!(matches!(error, RpcError::Protocol(_)), "{error:?}");
    assert!(!peer.is_closed());

    // Receiving: the connection ends.
    let frame = format!(
        r#"{{"jsonrpc":"2.0","method":"x","params":"{}"}}"#,
        "y".repeat(MAX_FRAME_BYTES)
    );
    raw.send(MessageFrame::Text(frame)).await.unwrap();
    let reason = tokio::time::timeout(WAIT, peer.closed()).await.unwrap();
    assert!(reason.contains("exceeds"), "{reason}");
}

#[tokio::test]
async fn transport_loss_fails_in_flight_requests() {
    let (peer, _incoming, mut raw) = peer_and_raw();
    let pending = tokio::spawn({
        let peer = peer.clone();
        async move { peer.request::<_, Empty>("tool/execute", &Empty {}).await }
    });
    let _request = recv_text(&mut raw).await;
    drop(raw);
    let reply = tokio::time::timeout(WAIT, pending).await.unwrap().unwrap();
    assert!(matches!(reply, Err(RpcError::Closed(_))), "{reply:?}");
    // Later requests fail at once.
    let later = peer.request::<_, Empty>("tool/execute", &Empty {}).await;
    assert!(matches!(later, Err(RpcError::Closed(_))), "{later:?}");
}

#[tokio::test]
async fn line_transport_frames_lines_and_bounds_them() {
    let (client, server) = tokio::io::duplex(1024);
    let (client_read, client_write) = tokio::io::split(client);
    let (server_read, mut server_write) = tokio::io::split(server);
    let mut transport = LineTransport::new(client_read, client_write);
    transport
        .send(MessageFrame::Text(r#"{"a":1}"#.into()))
        .await
        .unwrap();
    let mut lines = LineTransport::new(server_read, tokio::io::sink());
    let frame = lines.recv().await.unwrap().unwrap();
    assert_eq!(frame.into_text().unwrap(), r#"{"a":1}"#);
    assert!(
        transport
            .send(MessageFrame::Text("a\nb".into()))
            .await
            .is_err()
    );

    server_write.write_all(b"{\"b\":2}\r\n").await.unwrap();
    let frame = transport.recv().await.unwrap().unwrap();
    assert_eq!(frame.into_text().unwrap(), r#"{"b":2}"#);
    let writer = tokio::spawn(async move {
        let line = vec![b'x'; MAX_FRAME_BYTES + 1];
        let _ = server_write.write_all(&line).await;
        let _ = server_write.write_all(b"\n").await;
    });
    let error = transport.recv().await.expect_err("overlong line");
    assert!(error.to_string().contains("exceeds"), "{error}");
    writer.abort();
}

#[test]
fn information_ignores_unknown_members_and_reads_open_enum_fallbacks() {
    let prepared = json!({"results": [{
        "status": "failed",
        "error": {"kind": "quota_exceeded", "message": "no", "retry_after": 3},
        "presentation": {"title": "bash", "primary_argument": null, "icon": "x"},
        "extra": 1
    }], "batch": 1});
    let result: PrepareResult = serde_json::from_value(prepared).unwrap();
    let PrepareOutcome::Failed { error, .. } = &result.results[0] else {
        panic!("failed outcome expected");
    };
    // An unknown error kind reads as a generic failure.
    assert!(
        matches!(ToolError::from(error.clone()), ToolError::Failed(message) if message == "no")
    );

    let execute = json!({
        "outcome": {"status": "failed", "error": {"kind": "runtime_unavailable", "message": "gone"}},
        "media": [],
        "launched_jobs": [],
        "surprise": true
    });
    let result: ExecuteResult = serde_json::from_value(execute).unwrap();
    let ExecuteOutcome::Failed { error } = result.outcome else {
        panic!("failed outcome expected");
    };
    assert!(matches!(
        ToolError::from(error),
        ToolError::RuntimeUnavailable(message) if message == "gone"
    ));

    let settings = json!({
        "tool_output": {"max_lines": 10, "max_bytes": 100, "head_lines": 5, "tail_lines": 5},
        "agent_md": {"max_bytes": 1000},
        "mcp_servers": [],
        "plugins": []
    });
    assert!(serde_json::from_value::<RuntimeSettings>(settings).is_ok());
    assert!(serde_json::from_value::<Outcome>(json!({"error": null, "x": 1})).is_ok());

    // Unknown declaration values read as their documented fallbacks.
    let declaration: ToolDeclaration = serde_json::from_value(json!({
        "name": "t", "description": "d", "parameters": {"type": "object"},
        "collision": "shadow", "concurrency": "batched", "result_truncation": "summarize",
        "icons": ["x"]
    }))
    .unwrap();
    assert_eq!(declaration.collision, ToolCollision::Prefix);
    assert_eq!(declaration.concurrency, crate::ToolConcurrency::Exclusive);
    assert_eq!(
        declaration.result_truncation,
        crate::ToolResultTruncationPolicy::Bounded
    );
    let report: protocol::McpServerReport =
        serde_json::from_value(json!({"name": "docs", "state": "needs_auth", "scopes": []}))
            .unwrap();
    assert_eq!(report.state, protocol::McpServerState::Disconnected);

    // Command types still deny unknown members themselves.
    assert!(
        serde_json::from_value::<protocol::DetachableParams>(json!({"prepared_id": 1, "x": 1}))
            .is_err()
    );
}

#[test]
fn tool_errors_and_declarations_survive_the_wire() {
    for error in [
        ToolError::ProgressSinkClosed,
        ToolError::execution("boom"),
        ToolError::operation_changed("changed"),
        ToolError::resource_limit("too big"),
        ToolError::runtime_unavailable("gone"),
        ToolError::Timeout("slow".into()),
    ] {
        let wire = WireToolError::from(&error);
        let text = serde_json::to_string(&wire).unwrap();
        let back = ToolError::from(serde_json::from_str::<WireToolError>(&text).unwrap());
        assert_eq!(back.to_string(), error.to_string());
    }

    let spec = ToolSpec {
        background: false,
        background_default: false,
        name: "webfetch".into(),
        collision: ToolCollision::Fallback,
        permission_name: Some("webfetch".into()),
        description: "Fetch".into(),
        parameters: json!({"type": "object"}),
        concurrency: crate::ToolConcurrency::Parallel,
        result_truncation: Default::default(),
        output: Default::default(),
    };
    let declaration = ToolDeclaration::from_spec(&spec).unwrap();
    assert_eq!(declaration.clone().into_spec(), spec);
    let text = serde_json::to_string(&declaration).unwrap();
    assert_eq!(
        serde_json::from_str::<ToolDeclaration>(&text).unwrap(),
        declaration
    );
    let plugin = ToolSpec {
        permission_name: Some("plugin:docs".into()),
        ..spec
    };
    assert!(ToolDeclaration::from_spec(&plugin).is_err());

    let key = [0_u8, 1, 0x7f, 0xff];
    assert_eq!(protocol::hex_encode(&key), "00017fff");
    assert_eq!(protocol::hex_decode("00017fff").unwrap(), key);
    assert!(protocol::hex_decode("0g").is_none());
    assert!(protocol::hex_decode("abc").is_none());
}
