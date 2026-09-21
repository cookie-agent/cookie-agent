use cookie_agent_protocol::{
    ExtensionEngineCapabilities, ExtensionToolAfterResultAction, ExtensionToolBeforeCallAction,
    ToolCallId, extension_initialize_request, extension_shutdown_notification,
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt as _, BufReader};

use super::*;
use crate::{allow, replace};

fn declaration() -> ToolDecl {
    ToolDecl {
        output: Default::default(),
        name: "echo".into(),
        description: "Echo text".into(),
        parameters: json!({
            "type": "object",
            "properties": {"text": {"type": "string"}}
        }),
        permission_name: "echo".into(),
        primary_resource_param: None,
    }
}

#[test]
fn derives_capabilities_from_handlers() {
    let server = PluginServer::builder("echo", "0.1.0")
        .tool(declaration(), |_ctx, _request| async {
            Ok(ToolOutput::success("ok"))
        })
        .tool_before_call(|_ctx, _request| async { allow() })
        .tool_after_result(|_ctx, _request| async { replace("new") })
        .on_event(|_ctx, _event| async {})
        .on_bus_event(|_ctx, _event| async {})
        .enable_bus_publishing()
        .build()
        .unwrap();
    let capabilities = server.handlers.capabilities();
    assert!(capabilities.tools);
    assert!(capabilities.subscribe_events);
    assert!(capabilities.subscribe_bus);
    assert!(capabilities.publish_bus);
    assert!(!capabilities.publish_session_events);
    assert!(!capabilities.producer_messaging);
    assert_eq!(
        capabilities.intercept,
        [
            ExtensionInterceptionHook::ToolBeforeCall,
            ExtensionInterceptionHook::ToolAfterResult,
        ]
    );
}

#[test]
fn publishing_capabilities_require_explicit_opt_in() {
    let server = PluginServer::builder("echo", "0.1.0")
        .tool(declaration(), |_ctx, _request| async {
            Ok(ToolOutput::success("ok"))
        })
        .build()
        .unwrap();
    let capabilities = server.handlers.capabilities();
    assert!(!capabilities.publish_bus);
    assert!(!capabilities.publish_session_events);
    assert!(!capabilities.producer_messaging);
}

#[test]
fn recovery_handler_requires_producer_opt_in_but_producers_need_no_handler() {
    assert!(
        PluginServer::builder("producer", "0.1.0")
            .on_recovery(|_context| async { Ok(()) })
            .build()
            .is_err()
    );
    let server = PluginServer::builder("producer", "0.1.0")
        .enable_producers()
        .build()
        .unwrap();
    assert!(server.handlers.capabilities().producer_messaging);
}

#[test]
fn validates_schema_during_building() {
    let mut invalid = declaration();
    invalid.parameters = json!({"type": 42});
    let error = PluginServer::builder("echo", "0.1.0")
        .tool(invalid, |_ctx, _request| async {
            Ok(ToolOutput::success("unused"))
        })
        .build()
        .err()
        .unwrap();
    assert!(error.to_string().contains("JSON Schema"));
}

#[tokio::test]
async fn loopback_handshake_tool_ping_and_shutdown() {
    let server = PluginServer::builder("echo", "0.1.0")
        .tool(declaration(), |_ctx, request| async move {
            Ok(ToolOutput::success(
                request.arguments["text"].as_str().unwrap(),
            ))
        })
        .build()
        .unwrap();
    let (engine_side, plugin_side) = tokio::io::duplex(64 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
    let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
    let (engine_read, mut engine_write) = tokio::io::split(engine_side);
    let mut engine_read = BufReader::new(engine_read);

    write_wire(&mut engine_write, &extension_initialize_request("test")).await;
    let initialize = read_wire(&mut engine_read).await;
    assert_eq!(
        initialize["result"]["protocol_version"],
        crate::EXTENSION_PROTOCOL_VERSION
    );
    assert_eq!(initialize["result"]["name"], "echo");
    assert_eq!(initialize["result"]["capabilities"]["tools"], true);

    let session_id = SessionId::new_v7();
    let invocation_id = ToolCallId::new_v7();
    let call = Request::new(
        JsonRpcId::Number(2),
        PLUGIN_TOOLS_CALL_METHOD,
        Some(
            serde_json::to_value(ExtensionToolCallParams {
                tool: "echo".into(),
                session_id,
                context_id: "context".into(),
                invocation_id,
                arguments: json!({"text": "hello"}),
                resource: None,
                cancellation_token: None,
            })
            .unwrap(),
        ),
    );
    write_wire(&mut engine_write, &call).await;
    let response = read_wire(&mut engine_read).await;
    assert_eq!(
        response["result"],
        json!({"output": {"kind":"single", "text":"hello"}, "display":"hello", "is_error": false})
    );

    let ping = Request::new(JsonRpcId::Number(3), PLUGIN_PING_METHOD, Some(json!({})));
    write_wire(&mut engine_write, &ping).await;
    assert_eq!(read_wire(&mut engine_read).await["result"], json!({}));

    write_wire(&mut engine_write, &extension_shutdown_notification()).await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_dispatches_every_hook() {
    let server = PluginServer::builder("hooks", "0.1.0")
        .agent_before_start(|_ctx, _| async { crate::append_system_prompt("agent") })
        .session_before_compact(|_ctx, _| async { crate::cancel_compaction("compact") })
        .user_before_input(|_ctx, _| async {
            ExtensionUserBeforeInputResult {
                action: cookie_agent_protocol::ExtensionUserBeforeInputAction::Transform,
                new_text: Some("changed".into()),
                reason: None,
            }
        })
        .model_before_request(|_ctx, _| async {
            ExtensionModelBeforeRequestResult {
                action: cookie_agent_protocol::ExtensionModelBeforeRequestAction::Keep,
                messages: None,
                params_adjustments: None,
            }
        })
        .provider_before_headers(|_ctx, _| async {
            ExtensionProviderBeforeHeadersResult {
                set: [("x-test".into(), "yes".into())].into(),
                delete: vec!["x-old".into()],
            }
        })
        .provider_before_request(|_ctx, _| async {
            ExtensionProviderBeforeRequestResult {
                action: cookie_agent_protocol::ExtensionProviderBeforeRequestAction::Keep,
                payload: None,
            }
        })
        .provider_after_response(|_ctx, _| async { ExtensionProviderAfterResponseResult {} })
        .message_end(|_ctx, _| async {
            ExtensionMessageEndResult {
                action: cookie_agent_protocol::ExtensionMessageEndAction::Keep,
                content: None,
            }
        })
        .model_before_select(|_ctx, _| async {
            ExtensionAllowBlockResult {
                action: cookie_agent_protocol::ExtensionAllowBlockAction::Allow,
                reason: None,
            }
        })
        .session_before_fork(|_ctx, _| async {
            ExtensionAllowBlockResult {
                action: cookie_agent_protocol::ExtensionAllowBlockAction::Allow,
                reason: None,
            }
        })
        .session_before_revert(|_ctx, _| async {
            ExtensionSessionBeforeRevertResult {
                action: cookie_agent_protocol::ExtensionSessionBeforeRevertAction::Override,
                reason: None,
                instructions_override: Some("revert".into()),
            }
        })
        .build()
        .unwrap();
    assert_eq!(server.handlers.capabilities().intercept.len(), 11);

    let (engine_side, plugin_side) = tokio::io::duplex(256 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
    let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
    let (engine_read, mut engine_write) = tokio::io::split(engine_side);
    let mut engine_read = BufReader::new(engine_read);
    write_wire(&mut engine_write, &extension_initialize_request("test")).await;
    let initialize = read_wire(&mut engine_read).await;
    assert_eq!(
        initialize["result"]["capabilities"]["intercept"]
            .as_array()
            .unwrap()
            .len(),
        11
    );

    let session = SessionId::new_v7();
    let attempt = cookie_agent_protocol::AttemptId::new_v7();
    let model_selection = json!({"model": "custom.test/model", "variant": null});
    let resolved_model = json!({
        "selection": model_selection.clone(),
        "provider_id": "custom.test",
        "model_id": "model",
        "adapter_id": "openai-responses",
        "selection_fingerprint": "0".repeat(64),
    });
    let calls = [
        (
            PLUGIN_INTERCEPT_AGENT_BEFORE_START_METHOD,
            json!({"session_id":session,"context_id":"hook","agent_path":"test","prompt_context":{}}),
            json!("agent"),
        ),
        (
            PLUGIN_INTERCEPT_SESSION_BEFORE_COMPACT_METHOD,
            json!({"session_id":session,"context_id":"hook","checkpoint_id":"one","additions":[]}),
            json!(true),
        ),
        (
            PLUGIN_INTERCEPT_USER_BEFORE_INPUT_METHOD,
            json!({"session_id":session,"context_id":"hook","text":"old"}),
            json!("changed"),
        ),
        (
            PLUGIN_INTERCEPT_MODEL_BEFORE_REQUEST_METHOD,
            json!({"session_id":session,"context_id":"hook","attempt_id":attempt,"messages":[],"model":resolved_model,"params":{}}),
            json!("keep"),
        ),
        (
            PLUGIN_INTERCEPT_PROVIDER_BEFORE_HEADERS_METHOD,
            json!({"session_id":session,"context_id":"hook","attempt_id":attempt,"headers":{}}),
            json!("yes"),
        ),
        (
            PLUGIN_INTERCEPT_PROVIDER_BEFORE_REQUEST_METHOD,
            json!({"session_id":session,"context_id":"hook","attempt_id":attempt,"payload":{}}),
            json!("keep"),
        ),
        (
            PLUGIN_INTERCEPT_PROVIDER_AFTER_RESPONSE_METHOD,
            json!({"session_id":session,"context_id":"hook","attempt_id":attempt,"status":200,"headers":{}}),
            Value::Null,
        ),
        (
            PLUGIN_INTERCEPT_MESSAGE_END_METHOD,
            json!({"session_id":session,"context_id":"hook","attempt_id":attempt,"role":"assistant","content":[]}),
            json!("keep"),
        ),
        (
            PLUGIN_INTERCEPT_MODEL_BEFORE_SELECT_METHOD,
            json!({"session_id":session,"context_id":"hook","from":null,"to":model_selection,"source":"user"}),
            json!("allow"),
        ),
        (
            PLUGIN_INTERCEPT_SESSION_BEFORE_FORK_METHOD,
            json!({"session_id":session,"context_id":"hook","through_seq":1}),
            json!("allow"),
        ),
        (
            PLUGIN_INTERCEPT_SESSION_BEFORE_REVERT_METHOD,
            json!({"session_id":session,"context_id":"hook","through_seq":1}),
            json!("revert"),
        ),
    ];
    for (index, (method, params, expected)) in calls.into_iter().enumerate() {
        write_wire(
            &mut engine_write,
            &Request::new(JsonRpcId::Number(index as i64 + 2), method, Some(params)),
        )
        .await;
        let response = read_wire(&mut engine_read).await;
        let result = response["result"].clone();
        if expected.is_null() {
            assert_eq!(result, json!({}), "{method}");
        } else {
            assert!(
                result.to_string().contains(&expected.to_string()),
                "{method}: {response}"
            );
        }
    }
    write_wire(&mut engine_write, &extension_shutdown_notification()).await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_emit_uses_delivered_context_and_returns_selected_status() {
    let (status_tx, mut status_rx) = mpsc::channel(1);
    let server = PluginServer::builder("emitter", "0.1.0")
        .on_bus_event(move |context, event| {
            let status_tx = status_tx.clone();
            async move {
                let status = context
                    .emit_bus(event.session_id, "echoed", event.payload)
                    .await
                    .unwrap();
                status_tx.send(status).await.unwrap();
            }
        })
        .enable_bus_publishing()
        .build()
        .unwrap();
    let (engine_side, plugin_side) = tokio::io::duplex(64 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
    let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
    let (engine_read, mut engine_write) = tokio::io::split(engine_side);
    let mut engine_read = BufReader::new(engine_read);

    write_wire(&mut engine_write, &extension_initialize_request("test")).await;
    let _ = read_wire(&mut engine_read).await;
    let session_id = SessionId::new_v7();
    write_wire(
        &mut engine_write,
        &Notification::new(
            PLUGIN_BUS_EVENT_METHOD,
            Some(
                serde_json::to_value(ExtensionBusEventParams {
                    session_id,
                    context_id: Some("emit-context".into()),
                    plugin: "source".into(),
                    name: "incoming".into(),
                    payload: json!({"value": 1}),
                })
                .unwrap(),
            ),
        ),
    )
    .await;
    let emit = read_wire(&mut engine_read).await;
    assert_eq!(emit["method"], PLUGIN_EMIT_METHOD);
    assert_eq!(emit["params"]["session_id"], session_id.to_string());
    assert_eq!(emit["params"]["context_id"], "emit-context");
    assert_eq!(emit["params"]["name"], "echoed");
    write_wire(
        &mut engine_write,
        &Notification::new(
            PLUGIN_EMIT_RESULT_METHOD,
            Some(json!({
                "name": "echoed",
                "bus": "published",
                "durable": "rejected",
                "reason": "durable publishing disabled"
            })),
        ),
    )
    .await;
    assert_eq!(status_rx.recv().await, Some(ExtensionEmitStatus::Published));

    write_wire(&mut engine_write, &extension_shutdown_notification()).await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn recovery_services_correlated_producer_requests_until_explicit_completion() {
    let session_id = SessionId::new_v7();
    let (finished_tx, mut finished_rx) = mpsc::channel(1);
    let server = PluginServer::builder("producer", "0.1.0")
            .enable_producers()
            .on_recovery(move |context| {
                let finished_tx = finished_tx.clone();
                async move {
                    let producer = context.register_producer(session_id).await?;
                    for description in ["", "   ", "bad\ntext", &"x".repeat(1025)] {
                        assert!(matches!(
                            producer.steer("body", description, ProducerIdempotencyKey::new("invalid").unwrap()).await,
                            Err(PluginError::Protocol(_))
                        ));
                    }
                    let (steer, queue) = tokio::join!(
                        producer
                            .steer("steered", "Steered result", ProducerIdempotencyKey::new("steer-key").unwrap()),
                        producer.queue("queued", "Queued result", ProducerIdempotencyKey::new("queue-key").unwrap())
                    );
                    let receipts = (steer?, queue?);
                    assert!(matches!(
                        producer.unregister().await,
                        Err(PluginError::EngineRequest { code: -32001, .. })
                    ));
                    producer.unregister().await?;

                    producer.discard(receipts.1).await?;
                    context.discard_producer_message(session_id, receipts.1).await?;
                    let error = context.discard_producer_message(session_id, receipts.0).await.unwrap_err();
                    assert!(matches!(error, PluginError::EngineRequest { code: -32000, message, .. } if message == "message already consumed or in flight"));

                    let zero_send = context.register_producer(session_id).await?;
                    zero_send.unregister().await?;
                    finished_tx.send(receipts).await.unwrap();
                    Ok(())
                }
            })
            .build()
            .unwrap();
    let (engine_side, plugin_side) = tokio::io::duplex(64 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
    let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
    let (engine_read, mut engine_write) = tokio::io::split(engine_side);
    let mut engine_read = BufReader::new(engine_read);

    write_wire(
        &mut engine_write,
        &Request::new(
            JsonRpcId::Number(1),
            PLUGIN_INITIALIZE_METHOD,
            Some(serde_json::to_value(initialize_params(true)).unwrap()),
        ),
    )
    .await;
    let initialize = read_wire(&mut engine_read).await;
    assert_eq!(
        initialize["result"]["capabilities"]["producer_messaging"],
        true
    );
    write_wire(
        &mut engine_write,
        &Notification::new(PLUGIN_RECOVERY_START_METHOD, Some(json!({}))),
    )
    .await;

    let register = read_wire(&mut engine_read).await;
    assert_eq!(register["method"], PLUGIN_PRODUCER_REGISTER_METHOD);
    let first_producer = ProducerId::new_v7();
    reply_success(
        &mut engine_write,
        &register,
        json!({"producer_id": first_producer}),
    )
    .await;

    let first_send = read_wire(&mut engine_read).await;
    let second_send = read_wire(&mut engine_read).await;
    assert_eq!(first_send["method"], PLUGIN_PRODUCER_SEND_METHOD);
    assert_eq!(second_send["method"], PLUGIN_PRODUCER_SEND_METHOD);
    for request in [&first_send, &second_send] {
        let expected = if request["params"]["mode"] == "steer" {
            "Steered result"
        } else {
            "Queued result"
        };
        assert_eq!(request["params"]["description"], expected);
        serde_json::from_value::<ExtensionProducerSendParams>(request["params"].clone()).unwrap();
    }
    let receipt_for = |request: &Value| match request["params"]["mode"].as_str().unwrap() {
        "steer" => ProducerMessageId::new_v7(),
        "queue" => ProducerMessageId::new_v7(),
        mode => panic!("unexpected producer mode {mode}"),
    };
    let first_receipt = receipt_for(&first_send);
    let second_receipt = receipt_for(&second_send);
    reply_success(
        &mut engine_write,
        &second_send,
        json!({"message_id": second_receipt}),
    )
    .await;
    reply_success(
        &mut engine_write,
        &first_send,
        json!({"message_id": first_receipt}),
    )
    .await;

    let unregister = read_wire(&mut engine_read).await;
    assert_eq!(unregister["method"], PLUGIN_PRODUCER_UNREGISTER_METHOD);
    reply_error(&mut engine_write, &unregister, -32001, "transient overload").await;
    let retry_unregister = read_wire(&mut engine_read).await;
    assert_eq!(
        retry_unregister["method"],
        PLUGIN_PRODUCER_UNREGISTER_METHOD
    );
    assert_eq!(retry_unregister["params"], unregister["params"]);
    reply_success(&mut engine_write, &retry_unregister, json!({})).await;

    let (steer_receipt, queue_receipt) = match first_send["params"]["mode"].as_str().unwrap() {
        "steer" => (first_receipt, second_receipt),
        "queue" => (second_receipt, first_receipt),
        _ => unreachable!(),
    };
    let discard = read_wire(&mut engine_read).await;
    assert_eq!(discard["method"], "plugin/producer/discard");
    assert_eq!(
        discard["params"],
        json!({"session_id": session_id, "message_id": queue_receipt})
    );
    reply_success(&mut engine_write, &discard, json!({})).await;
    let repeated_discard = read_wire(&mut engine_read).await;
    assert_eq!(repeated_discard["method"], "plugin/producer/discard");
    assert_eq!(repeated_discard["params"], discard["params"]);
    reply_success(&mut engine_write, &repeated_discard, json!({})).await;
    let consumed_discard = read_wire(&mut engine_read).await;
    assert_eq!(consumed_discard["method"], "plugin/producer/discard");
    assert_eq!(
        consumed_discard["params"],
        json!({"session_id": session_id, "message_id": steer_receipt})
    );
    reply_error(
        &mut engine_write,
        &consumed_discard,
        -32000,
        "message already consumed or in flight",
    )
    .await;

    let zero_register = read_wire(&mut engine_read).await;
    assert_eq!(zero_register["method"], PLUGIN_PRODUCER_REGISTER_METHOD);
    reply_success(
        &mut engine_write,
        &zero_register,
        json!({"producer_id": ProducerId::new_v7()}),
    )
    .await;
    let zero_unregister = read_wire(&mut engine_read).await;
    assert_eq!(zero_unregister["method"], PLUGIN_PRODUCER_UNREGISTER_METHOD);
    reply_success(&mut engine_write, &zero_unregister, json!({})).await;

    let expected = match first_send["params"]["mode"].as_str().unwrap() {
        "steer" => (first_receipt, second_receipt),
        "queue" => (second_receipt, first_receipt),
        _ => unreachable!(),
    };
    assert_eq!(finished_rx.recv().await, Some(expected));
    let complete = read_wire(&mut engine_read).await;
    assert_eq!(complete["method"], PLUGIN_RECOVERY_COMPLETE_METHOD);
    assert_eq!(complete["params"]["outcome"], json!({"status":"ready"}));
    reply_success(&mut engine_write, &complete, json!({})).await;

    write_wire(&mut engine_write, &extension_shutdown_notification()).await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn recovery_without_handler_sends_explicit_ready_completion() {
    let server = PluginServer::builder("producer", "0.1.0")
        .enable_producers()
        .build()
        .unwrap();
    let (engine_side, plugin_side) = tokio::io::duplex(64 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
    let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
    let (engine_read, mut engine_write) = tokio::io::split(engine_side);
    let mut engine_read = BufReader::new(engine_read);

    write_wire(
        &mut engine_write,
        &Request::new(
            JsonRpcId::Number(1),
            PLUGIN_INITIALIZE_METHOD,
            Some(serde_json::to_value(initialize_params(true)).unwrap()),
        ),
    )
    .await;
    let initialize = read_wire(&mut engine_read).await;
    assert_eq!(
        initialize["result"]["capabilities"]["producer_messaging"],
        true
    );

    write_wire(
        &mut engine_write,
        &Notification::new(PLUGIN_RECOVERY_START_METHOD, Some(json!({}))),
    )
    .await;
    let complete = read_wire(&mut engine_read).await;
    assert_eq!(complete["method"], PLUGIN_RECOVERY_COMPLETE_METHOD);
    assert_eq!(complete["params"]["outcome"], json!({"status":"ready"}));
    reply_success(&mut engine_write, &complete, json!({})).await;

    write_wire(&mut engine_write, &extension_shutdown_notification()).await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn recovery_discards_saved_receipt_without_registering() {
    let session_id = SessionId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let server = PluginServer::builder("producer", "0.1.0")
        .enable_producers()
        .on_recovery(move |context| async move {
            context
                .discard_producer_message(session_id, message_id)
                .await?;
            Ok(())
        })
        .build()
        .unwrap();
    let (engine_side, plugin_side) = tokio::io::duplex(64 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
    let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
    let (engine_read, mut engine_write) = tokio::io::split(engine_side);
    let mut engine_read = BufReader::new(engine_read);
    write_wire(&mut engine_write, &extension_initialize_request("test")).await;
    let _ = read_wire(&mut engine_read).await;
    write_wire(
        &mut engine_write,
        &Notification::new(PLUGIN_RECOVERY_START_METHOD, Some(json!({}))),
    )
    .await;
    let discard = read_wire(&mut engine_read).await;
    assert_eq!(discard["method"], PLUGIN_PRODUCER_DISCARD_METHOD);
    assert_eq!(
        discard["params"],
        json!({"session_id": session_id, "message_id": message_id})
    );
    reply_success(&mut engine_write, &discard, json!({})).await;
    let complete = read_wire(&mut engine_read).await;
    assert_eq!(complete["method"], PLUGIN_RECOVERY_COMPLETE_METHOD);
    assert_eq!(complete["params"]["outcome"], json!({"status": "ready"}));
    reply_success(&mut engine_write, &complete, json!({})).await;
    write_wire(&mut engine_write, &extension_shutdown_notification()).await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn queued_and_writer_blocked_requests_report_disconnect() {
    let (context_tx, mut context_rx) = mpsc::channel(1);
    let server = PluginServer::builder("producer", "0.1.0")
        .enable_producers()
        .on_recovery(move |context| {
            let context_tx = context_tx.clone();
            async move {
                context_tx.send(context).await.unwrap();
                std::future::pending::<RecoveryResult>().await
            }
        })
        .build()
        .unwrap();
    let (engine_side, plugin_side) = tokio::io::duplex(256);
    let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
    let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
    let (engine_read, mut engine_write) = tokio::io::split(engine_side);
    let mut engine_read = BufReader::new(engine_read);
    write_wire(&mut engine_write, &extension_initialize_request("test")).await;
    let _ = read_wire(&mut engine_read).await;
    write_wire(
        &mut engine_write,
        &Notification::new(PLUGIN_RECOVERY_START_METHOD, Some(json!({}))),
    )
    .await;
    let context = context_rx.recv().await.unwrap();
    let mut calls = Vec::new();
    for index in 0..MAX_PENDING_REQUESTS {
        let context = context.clone();
        calls.push(tokio::spawn(async move {
            if index % 2 == 0 {
                context
                    .register_producer(SessionId::new_v7())
                    .await
                    .map(|_| ())
            } else {
                context
                    .discard_producer_message(SessionId::new_v7(), ProducerMessageId::new_v7())
                    .await
            }
        }));
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while context.state.pending_slots.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all external calls should enter the bounded writer pipeline");
    drop(engine_write);
    drop(engine_read);
    for call in calls {
        let result = tokio::time::timeout(Duration::from_secs(1), call)
            .await
            .expect("queued request must not hang on disconnect")
            .unwrap();
        assert!(matches!(result, Err(PluginError::TransportClosed)));
    }
    let _ = server_task.await.unwrap();
}

#[tokio::test]
async fn pending_request_capacity_is_bounded() {
    let (outbound, mut receiver) = mpsc::channel(MAX_PENDING_REQUESTS);
    let context = PluginContext {
        state: Arc::new(ContextState {
            grants: Mutex::new(HashMap::new()),
            pending_emits: Arc::new(Mutex::new(HashMap::new())),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            outbound,
            publishing: PublishingCapabilities { bus: false },
            producers: true,
            request_ids: AtomicI64::new(1),
            pending_slots: Arc::new(Semaphore::new(MAX_PENDING_REQUESTS)),
            connected: AtomicBool::new(true),
        }),
        grant: None,
    };
    let mut tasks = Vec::new();
    for _ in 0..MAX_PENDING_REQUESTS {
        let context = context.clone();
        tasks.push(tokio::spawn(async move {
            context_request::<_, ExtensionRecoveryCompleteResult>(
                &context,
                PLUGIN_RECOVERY_COMPLETE_METHOD,
                ExtensionRecoveryCompleteParams {
                    outcome: ExtensionRecoveryOutcome::Ready,
                },
            )
            .await
        }));
    }
    let mut held = Vec::new();
    for _ in 0..MAX_PENDING_REQUESTS {
        held.push(receiver.recv().await.unwrap());
    }
    let error = context_request::<_, ExtensionRecoveryCompleteResult>(
        &context,
        PLUGIN_RECOVERY_COMPLETE_METHOD,
        ExtensionRecoveryCompleteParams {
            outcome: ExtensionRecoveryOutcome::Ready,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PluginError::TooManyPendingRequests));
    drop(held);
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn handler_capacity_returns_explicit_busy_error() {
    let slots = Arc::new(Semaphore::new(MAX_CONCURRENT_HANDLERS));
    let mut permits = Vec::new();
    for _ in 0..MAX_CONCURRENT_HANDLERS {
        permits.push(Arc::clone(&slots).try_acquire_owned().unwrap());
    }
    let (outbound, mut receiver) = mpsc::channel(1);
    let context = PluginContext {
        state: Arc::new(ContextState {
            grants: Mutex::new(HashMap::new()),
            pending_emits: Arc::new(Mutex::new(HashMap::new())),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            outbound,
            publishing: PublishingCapabilities { bus: false },
            producers: false,
            request_ids: AtomicI64::new(1),
            pending_slots: Arc::new(Semaphore::new(MAX_PENDING_REQUESTS)),
            connected: AtomicBool::new(true),
        }),
        grant: None,
    };
    let request = Request::new(JsonRpcId::Number(42), PLUGIN_TOOLS_CALL_METHOD, None);
    assert!(
        try_handler_slot(&context, &request, &slots)
            .await
            .unwrap()
            .is_none()
    );
    let Outbound::Message(response) = receiver.recv().await.unwrap() else {
        panic!("handler saturation must send a JSON-RPC error");
    };
    assert_eq!(response["id"], 42);
    assert_eq!(response["error"]["code"], SERVER_BUSY);
    drop(permits);
}

#[tokio::test]
async fn concurrent_same_session_calls_emit_with_their_own_contexts() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let server = PluginServer::builder("concurrent", "0.1.0")
        .tool(declaration(), move |context, request| {
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.wait().await;
                let name = request.arguments["text"].as_str().unwrap().to_owned();
                let status = context
                    .emit_bus(request.session_id, name.clone(), json!({"call": name}))
                    .await
                    .unwrap();
                assert_eq!(status, ExtensionEmitStatus::Published);
                Ok(ToolOutput::success(name))
            }
        })
        .enable_bus_publishing()
        .build()
        .unwrap();
    let (engine_side, plugin_side) = tokio::io::duplex(64 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
    let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
    let (engine_read, mut engine_write) = tokio::io::split(engine_side);
    let mut engine_read = BufReader::new(engine_read);

    write_wire(&mut engine_write, &extension_initialize_request("test")).await;
    let _ = read_wire(&mut engine_read).await;
    let session_id = SessionId::new_v7();
    write_wire(
        &mut engine_write,
        &tool_call_request(2, session_id, "context-first", "first"),
    )
    .await;
    write_wire(
        &mut engine_write,
        &tool_call_request(3, session_id, "context-second", "second"),
    )
    .await;

    let emits = [
        read_wire(&mut engine_read).await,
        read_wire(&mut engine_read).await,
    ];
    for emit in &emits {
        let name = emit["params"]["name"].as_str().unwrap();
        let expected_context = match name {
            "first" => "context-first",
            "second" => "context-second",
            other => panic!("unexpected emit name {other}"),
        };
        assert_eq!(emit["params"]["session_id"], session_id.to_string());
        assert_eq!(emit["params"]["context_id"], expected_context);
    }
    for emit in &emits {
        write_wire(
            &mut engine_write,
            &Notification::new(
                PLUGIN_EMIT_RESULT_METHOD,
                Some(json!({
                    "name": emit["params"]["name"],
                    "bus": "published",
                    "durable": "rejected"
                })),
            ),
        )
        .await;
    }

    let responses = [
        read_wire(&mut engine_read).await,
        read_wire(&mut engine_read).await,
    ];
    let mut outputs = responses
        .iter()
        .map(|response| response["result"]["output"]["text"].as_str().unwrap())
        .collect::<Vec<_>>();
    outputs.sort_unstable();
    assert_eq!(outputs, ["first", "second"]);

    write_wire(&mut engine_write, &extension_shutdown_notification()).await;
    server_task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn emit_grants_are_one_shot_expire_and_reject_unknown_sessions() {
    let (outbound, mut receiver) = mpsc::channel(4);
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let context = PluginContext {
        state: Arc::new(ContextState {
            grants: Mutex::new(HashMap::new()),
            pending_emits: pending,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            outbound,
            publishing: PublishingCapabilities { bus: true },
            producers: false,
            request_ids: AtomicI64::new(1),
            pending_slots: Arc::new(Semaphore::new(MAX_PENDING_REQUESTS)),
            connected: AtomicBool::new(true),
        }),
        grant: None,
    };
    let session = SessionId::new_v7();
    let unknown = SessionId::new_v7();
    let first = context.register_notification(session, "first".into(), Instant::now());
    assert!(matches!(
        first.consume(unknown),
        Err(PluginError::ContextUnavailable(id)) if id == unknown
    ));
    assert_eq!(first.consume(session).unwrap(), "first");
    assert!(matches!(
        first.consume(session),
        Err(PluginError::ContextUnavailable(id)) if id == session
    ));

    let received_at = Instant::now();
    tokio::time::advance(NOTIFICATION_CONTEXT_LIFETIME).await;
    let expiring = context.register_notification(session, "expiring".into(), received_at);
    assert!(matches!(
        expiring.consume(session),
        Err(PluginError::ContextUnavailable(id)) if id == session
    ));
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn emit_requires_target_capability_without_consuming_the_grant() {
    let (outbound, mut receiver) = mpsc::channel(1);
    let context = PluginContext {
        state: Arc::new(ContextState {
            grants: Mutex::new(HashMap::new()),
            pending_emits: Arc::new(Mutex::new(HashMap::new())),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            outbound,
            publishing: PublishingCapabilities { bus: false },
            producers: false,
            request_ids: AtomicI64::new(1),
            pending_slots: Arc::new(Semaphore::new(MAX_PENDING_REQUESTS)),
            connected: AtomicBool::new(true),
        }),
        grant: None,
    };
    let session = SessionId::new_v7();
    assert!(matches!(
        context.register_producer(session).await,
        Err(PluginError::ProducerMessagingNotEnabled)
    ));
    assert!(matches!(
        context
            .discard_producer_message(session, ProducerMessageId::new_v7())
            .await,
        Err(PluginError::ProducerMessagingNotEnabled)
    ));
    let scoped = context.register_request(session, "gated".into());
    assert!(matches!(
        scoped.emit_bus(session, "event", json!({})).await,
        Err(PluginError::PublishingNotEnabled("bus"))
    ));
    assert_eq!(scoped.consume(session).unwrap(), "gated");
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn synchronously_panicking_tool_and_intercept_handlers_return_internal_errors() {
    let server = PluginServer::builder("panic", "0.1.0")
        .tool(declaration(), synchronously_panicking_tool)
        .tool_before_call(synchronously_panicking_intercept)
        .build()
        .unwrap();
    let (engine_side, plugin_side) = tokio::io::duplex(64 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
    let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
    let (engine_read, mut engine_write) = tokio::io::split(engine_side);
    let mut engine_read = BufReader::new(engine_read);
    write_wire(&mut engine_write, &extension_initialize_request("test")).await;
    let _ = read_wire(&mut engine_read).await;
    let call = Request::new(
        JsonRpcId::Number(2),
        PLUGIN_TOOLS_CALL_METHOD,
        Some(json!({
            "tool": "echo",
            "session_id": SessionId::new_v7(),
            "context_id": "panic-context",
            "invocation_id": ToolCallId::new_v7(),
            "arguments": {},
            "resource": null
        })),
    );
    write_wire(&mut engine_write, &call).await;
    let response = read_wire(&mut engine_read).await;
    assert_eq!(response["error"]["code"], INTERNAL_ERROR);

    let intercept = Request::new(
        JsonRpcId::Number(3),
        PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD,
        Some(json!({
            "session_id": SessionId::new_v7(),
            "context_id": "intercept-panic-context",
            "tool": "echo",
            "arguments": {},
            "permission_name": "echo",
            "resource": null
        })),
    );
    write_wire(&mut engine_write, &intercept).await;
    let response = read_wire(&mut engine_read).await;
    assert_eq!(response["error"]["code"], INTERNAL_ERROR);
    write_wire(&mut engine_write, &extension_shutdown_notification()).await;
    server_task.await.unwrap().unwrap();
}

fn synchronously_panicking_tool(
    _context: PluginContext,
    _request: ExtensionToolCallParams,
) -> std::future::Ready<Result<ToolOutput, ToolFailure>> {
    panic!("synchronous tool panic")
}

fn synchronously_panicking_intercept(
    _context: PluginContext,
    _request: ExtensionToolBeforeCallParams,
) -> std::future::Ready<ExtensionToolBeforeCallResult> {
    panic!("synchronous intercept panic")
}

#[test]
fn helper_results_have_protocol_actions() {
    assert_eq!(allow().action, ExtensionToolBeforeCallAction::Allow);
    assert_eq!(
        replace("new").action,
        ExtensionToolAfterResultAction::Replace
    );
}

async fn write_wire<W: AsyncWrite + Unpin>(writer: &mut W, value: &impl Serialize) {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    writer.write_all(&bytes).await.unwrap();
    writer.flush().await.unwrap();
}

async fn read_wire<R: tokio::io::AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

async fn reply_success<W: AsyncWrite + Unpin>(writer: &mut W, request: &Value, result: Value) {
    write_wire(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": result,
        }),
    )
    .await;
}

async fn reply_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    request: &Value,
    code: i32,
    message: &str,
) {
    write_wire(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": {"code": code, "message": message},
        }),
    )
    .await;
}

fn tool_call_request(id: i64, session_id: SessionId, context_id: &str, text: &str) -> Request {
    Request::new(
        JsonRpcId::Number(id),
        PLUGIN_TOOLS_CALL_METHOD,
        Some(
            serde_json::to_value(ExtensionToolCallParams {
                tool: "echo".into(),
                session_id,
                context_id: context_id.into(),
                invocation_id: ToolCallId::new_v7(),
                arguments: json!({"text": text}),
                resource: None,
                cancellation_token: None,
            })
            .unwrap(),
        ),
    )
}

#[allow(dead_code)]
fn initialize_params(producer_messaging: bool) -> ExtensionInitializeParams {
    ExtensionInitializeParams {
        protocol_version: ExtensionProtocolVersion::current(),
        engine_version: "test".into(),
        capabilities: ExtensionEngineCapabilities {
            producer_messaging,
            ping: true,
            shutdown: true,
            tools: true,
            event_streaming: true,
            event_publishing: true,
            interception: true,
        },
    }
}
