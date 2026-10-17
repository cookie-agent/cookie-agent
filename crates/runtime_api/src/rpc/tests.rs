use std::time::Duration;

use cookie_agent_protocol::{MessageFrame, Transport, in_process_pair};
use serde_json::json;
use tokio::io::AsyncWriteExt as _;

use super::{Incoming, LineTransport, Peer, RpcError};
use crate::protocol::ToolDeclarationExt as _;
use crate::protocol::{
    self, Empty, ExecuteOutcome, ExecuteResult, MAX_FRAME_BYTES, Outcome, PrepareOutcome,
    PrepareResult, RuntimeSettings, ToolDeclaration, WireToolError, error_code,
};
use crate::{ToolCollision, ToolError, ToolSpec};

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
        .fail(RpcError::remote(error_code::UNKNOWN_HANDLE, "no such call"))
        .await
        .unwrap();
    match ask.await.unwrap() {
        Err(RpcError::Remote { code, message, .. }) => {
            assert_eq!(code, error_code::UNKNOWN_HANDLE);
            assert_eq!(message, "no such call");
        }
        other => panic!("unexpected reply {other:?}"),
    }
}

#[tokio::test]
async fn unknown_params_fields_are_invalid_params() {
    let (peer, mut incoming, mut raw) = peer_and_raw();
    raw.send(MessageFrame::Text(
        r#"{"jsonrpc":"2.0","id":7,"method":"runtime/context","params":{"preset":null,"extra":1}}"#
            .into(),
    ))
    .await
    .unwrap();
    let Some(Incoming::Request(request)) = incoming.recv().await else {
        panic!("request expected");
    };
    let error = request
        .params::<protocol::ContextParams>()
        .expect_err("unknown field");
    let RpcError::Remote { code, .. } = &error else {
        panic!("remote error expected");
    };
    assert_eq!(*code, error_code::INVALID_PARAMS);
    request.fail(error).await.unwrap();
    let reply: serde_json::Value = serde_json::from_str(&recv_text(&mut raw).await).unwrap();
    assert_eq!(reply["id"], 7);
    assert_eq!(reply["error"]["code"], error_code::INVALID_PARAMS);
    assert!(!peer.is_closed());
}

#[tokio::test]
async fn a_malformed_envelope_ends_the_connection_and_fails_pending_requests() {
    for frame in [
        // Unknown envelope field.
        r#"{"jsonrpc":"2.0","id":1,"result":{},"extra":true}"#,
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
fn wire_types_reject_unknown_fields_including_inside_tagged_variants() {
    let prepared = json!({"results": [{
        "status": "failed",
        "error": {"kind": "failed", "message": "no"},
        "presentation": {"title": "bash", "primary_argument": null},
        "extra": 1
    }]});
    assert!(serde_json::from_value::<PrepareResult>(prepared).is_err());
    let failed = json!({"results": [{
        "status": "failed",
        "error": {"kind": "failed", "message": "no"},
        "presentation": {"title": "bash", "primary_argument": null}
    }]});
    let result: PrepareResult = serde_json::from_value(failed).unwrap();
    assert!(matches!(result.results[0], PrepareOutcome::Failed { .. }));

    let execute = json!({
        "outcome": {"status": "failed", "error": {"kind": "runtime_unavailable", "message": "gone"}},
        "media": [],
        "launched_jobs": [],
        "surprise": true
    });
    assert!(serde_json::from_value::<ExecuteResult>(execute).is_err());
    let execute = json!({
        "outcome": {"status": "failed", "error": {"kind": "runtime_unavailable", "message": "gone"}}
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
    assert!(serde_json::from_value::<RuntimeSettings>(settings).is_err());
    assert!(serde_json::from_value::<Outcome>(json!({"error": null, "x": 1})).is_err());
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
