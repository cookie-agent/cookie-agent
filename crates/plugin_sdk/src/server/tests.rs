use std::collections::BTreeMap;

use cookie_agent_protocol::{
    AdaptorId, AgentId, CancellationCapability, ExtensionToolBeforeCallAction, ExtensionToolOrigin,
    Modality, ModelCapabilities, ReplayCapability, runtime_protocol::AdapterFamily,
    runtime_protocol::WireTurnContext,
};
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::{sync::mpsc, task::JoinHandle};

use super::*;
use crate::allow;

/// A liveness bound, not a latency assertion.
const WAIT: Duration = Duration::from_secs(10);

fn declaration(name: &str) -> ToolDecl {
    serde_json::from_value(json!({
        "name": name,
        "description": "Echo text",
        "parameters": {"type": "object", "properties": {"text": {"type": "string"}}},
    }))
    .expect("tool declaration")
}

/// A fresh UUID-backed ID of any kind.
fn id<T: DeserializeOwned>() -> T {
    serde_json::from_value(serde_json::to_value(SessionId::new_v7()).expect("ID")).expect("ID")
}

/// The engine's end of a plugin connection.
struct Engine {
    peer: Peer,
    incoming: mpsc::Receiver<Incoming>,
    server: JoinHandle<Result<(), PluginError>>,
    initialized: ExtensionInitializeResult,
}

/// The initialize params the daemon sends.
fn initialize_params(producer_messaging: bool) -> ExtensionInitializeParams {
    ExtensionInitializeParams {
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        engine_version: "test".into(),
        features: producer_messaging
            .then(|| FEATURE_PRODUCER_MESSAGING.to_owned())
            .into_iter()
            .collect(),
        interception_timeout_ms: 2_000,
    }
}

/// Serves `server` over an in-memory stream; returns the daemon's end.
fn serve(
    server: PluginServer,
) -> (
    Peer,
    mpsc::Receiver<Incoming>,
    JoinHandle<Result<(), PluginError>>,
) {
    let (engine_io, plugin_io) = tokio::io::duplex(1 << 20);
    let (plugin_read, plugin_write) = tokio::io::split(plugin_io);
    let server = tokio::spawn(server.run_io(plugin_read, plugin_write));
    let (engine_read, engine_write) = tokio::io::split(engine_io);
    let (peer, incoming) = Peer::start(Box::new(LineTransport::new(engine_read, engine_write)));
    (peer, incoming, server)
}

impl Engine {
    async fn connect(server: PluginServer, producer_messaging: bool) -> Self {
        Self::connect_with(server, initialize_params(producer_messaging)).await
    }

    async fn connect_with(server: PluginServer, params: ExtensionInitializeParams) -> Self {
        let (peer, incoming, server) = serve(server);
        let initialized = peer
            .request(PLUGIN_INITIALIZE_METHOD, &params)
            .await
            .expect("initialize");
        Self {
            peer,
            incoming,
            server,
            initialized,
        }
    }

    async fn next(&mut self) -> Incoming {
        tokio::time::timeout(WAIT, self.incoming.recv())
            .await
            .expect("a message from the plugin")
            .expect("the plugin connection")
    }

    async fn next_request(&mut self, method: &str) -> IncomingRequest {
        match self.next().await {
            Incoming::Request(request) if request.method == method => request,
            Incoming::Request(request) => panic!("expected `{method}`, got `{}`", request.method),
            Incoming::Notification(notification) => {
                panic!("expected `{method}`, got `{}`", notification.method)
            }
        }
    }

    async fn next_notification(&mut self, method: &str) -> IncomingNotification {
        match self.next().await {
            Incoming::Notification(notification) if notification.method == method => notification,
            Incoming::Notification(notification) => {
                panic!("expected `{method}`, got `{}`", notification.method)
            }
            Incoming::Request(request) => panic!("expected `{method}`, got `{}`", request.method),
        }
    }

    async fn prepare(&self, session: SessionId, calls: Vec<WireToolCall>) -> Vec<PrepareOutcome> {
        let result: PrepareResult = self
            .peer
            .request(
                method::TOOL_PREPARE,
                &PrepareParams {
                    instance: None,
                    session,
                    run: RunId::new_v7(),
                    turn: turn(),
                    calls,
                },
            )
            .await
            .expect("prepare");
        result.results
    }

    async fn execute(&self, session: SessionId, prepared_id: u64) -> ExecuteResult {
        self.peer
            .request(
                method::TOOL_EXECUTE,
                &ExecuteParams {
                    prepared_id,
                    session,
                    run: RunId::new_v7(),
                    turn: turn(),
                    background: false,
                },
            )
            .await
            .expect("execute")
    }

    async fn shutdown(self) {
        self.peer
            .notify(PLUGIN_SHUTDOWN_METHOD, &json!({}))
            .await
            .expect("shutdown notification");
        tokio::time::timeout(WAIT, self.server)
            .await
            .expect("the server stops")
            .expect("server task")
            .expect("server result");
    }
}

fn turn() -> WireTurnContext {
    WireTurnContext {
        agent: AgentId::new("test").expect("agent ID"),
        model: "test/model".parse().expect("model key"),
        adapter: AdaptorId::OPENAI_CHAT,
        adapter_family: AdapterFamily::OpenaiChat,
        capabilities: ModelCapabilities {
            input: [Modality::Text].into_iter().collect(),
            output: [Modality::Text].into_iter().collect(),
            context_tokens: 8_192,
            output_tokens: 2_048,
            tool_calling: true,
            parallel_tool_calls: true,
            structured_output: false,
            reasoning: false,
            temperature: true,
            top_p: true,
            seed: false,
            native_replay: ReplayCapability::Optional,
            cancellation: CancellationCapability::LocalOnly,
            media: BTreeMap::new(),
        },
    }
}

fn call(name: &str, arguments: Value) -> WireToolCall {
    WireToolCall {
        id: ToolCallId::new_v7(),
        name: name.into(),
        arguments,
    }
}

fn prepared(outcome: &PrepareOutcome) -> &PreparedCall {
    match outcome {
        PrepareOutcome::Prepared(prepared) => prepared,
        PrepareOutcome::Failed { error, .. } => panic!("preparation failed: {}", error.message),
    }
}

fn completed_text(result: &ExecuteResult) -> (&str, bool) {
    match &result.outcome {
        ExecuteOutcome::Completed {
            output: ToolCompletionOutput::Single { text },
            failed,
            ..
        } => (text, *failed),
        other => panic!("expected a single-text completion, got {other:?}"),
    }
}

#[test]
fn capabilities_follow_the_registered_handlers() {
    let builder = PluginServer::builder("echo", "0.1.0")
        .tool(declaration("echo"), |_context, _arguments| async {
            Ok(ToolOutput::success("ok"))
        })
        .tool_after_result(|_context, _params| async { crate::replace("new") })
        .tool_before_call(|_context, _params| async { allow() })
        .on_event(|_context, _event| async {})
        .enable_bus_publishing();
    let capabilities = builder.capabilities();
    assert!(capabilities.subscribe_events);
    assert!(!capabilities.subscribe_bus);
    assert!(capabilities.publish_bus);
    assert!(!capabilities.producer_messaging);
    // Advertised in the hook table's order, whatever the registration order.
    assert_eq!(
        capabilities.intercept,
        [
            ExtensionInterceptionHook::ToolBeforeCall,
            ExtensionInterceptionHook::ToolAfterResult,
        ]
    );
}

#[test]
fn building_rejects_invalid_registrations() {
    let error = |builder: PluginServerBuilder| builder.build().err().expect("a build error");
    let handler = |_context: ToolContext, _arguments: Value| async { Ok(ToolOutput::success("")) };

    assert!(
        error(PluginServer::builder("producer", "1").on_recovery(|_context| async { Ok(()) }))
            .to_string()
            .contains("enable_producers")
    );
    assert!(
        PluginServer::builder("producer", "1")
            .enable_producers()
            .build()
            .is_ok()
    );

    let mut schema = declaration("echo");
    schema.parameters = json!({"type": 42});
    assert!(
        error(PluginServer::builder("echo", "1").tool(schema, handler))
            .to_string()
            .contains("invalid JSON Schema")
    );
    assert!(
        error(
            PluginServer::builder("echo", "1")
                .tool(declaration("echo"), handler)
                .tool(declaration("echo"), handler)
        )
        .to_string()
        .contains("duplicate tool name")
    );
    assert!(
        error(PluginServer::builder("echo", "1").tool(declaration("has space"), handler))
            .to_string()
            .contains("ASCII letters")
    );
    let mut background = declaration("echo");
    background.background_default = true;
    assert!(
        error(PluginServer::builder("echo", "1").tool(background, handler))
            .to_string()
            .contains("background_default")
    );
}

#[tokio::test]
async fn a_tool_is_declared_prepared_and_executed_over_the_runtime_protocol() {
    let mut guarded = declaration("read_issue");
    guarded.permission = Some(PermissionAction::new("issue_read").expect("action"));
    let server = PluginServer::builder("issues", "0.1.0")
        .tool(declaration("echo"), |context, arguments| async move {
            assert!(!context.background());
            Ok(ToolOutput::success(
                arguments["text"].as_str().unwrap_or_default(),
            ))
        })
        .tool_with_resource(
            guarded,
            |arguments| arguments["issue"].as_str().map(str::to_owned),
            |_context, _arguments| async { Ok(ToolOutput::error("closed issue")) },
        )
        .build()
        .expect("server");
    let engine = Engine::connect(server, false).await;
    assert_eq!(engine.initialized.protocol_version, PLUGIN_PROTOCOL_VERSION);
    assert_eq!(engine.initialized.name, "issues");
    assert_eq!(
        engine
            .initialized
            .tools
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["echo", "read_issue"]
    );

    let session = SessionId::new_v7();
    let outcomes = engine
        .prepare(
            session,
            vec![
                call("echo", json!({"text": "hello"})),
                call("read_issue", json!({"issue": "42"})),
                call("missing", json!({})),
            ],
        )
        .await;
    let echo = prepared(&outcomes[0]);
    assert_eq!(echo.permission, None);
    assert_eq!(echo.permission_resource, None);
    assert_eq!(echo.operation.capabilities()[0].action.as_str(), "echo");
    let guarded = prepared(&outcomes[1]);
    assert_eq!(
        guarded.permission.as_ref().map(PermissionAction::as_str),
        Some("issue_read")
    );
    assert_eq!(guarded.permission_resource.as_deref(), Some("42"));
    assert_eq!(guarded.policy_labels, [Some("42".to_owned())]);
    assert!(matches!(
        &outcomes[2],
        PrepareOutcome::Failed { error, .. } if error.message.contains("unknown tool")
    ));

    let result = engine.execute(session, echo.prepared_id).await;
    assert_eq!(completed_text(&result), ("hello", false));
    let result = engine.execute(session, guarded.prepared_id).await;
    assert_eq!(completed_text(&result), ("closed issue", true));
    // A prepared call runs once.
    let again = engine.execute(session, echo.prepared_id).await;
    assert!(matches!(
        again.outcome,
        ExecuteOutcome::Failed { error } if error.message.contains("unknown prepared call")
    ));
    engine.shutdown().await;
}

#[tokio::test]
async fn a_tool_reports_progress_and_stops_when_cancelled() {
    let server = PluginServer::builder("slow", "0.1.0")
        .tool(declaration("wait"), |context, _arguments| async move {
            context
                .progress("waiting", Some("partial".into()))
                .await
                .map_err(|error| ToolFailure::new(error.to_string()))?;
            context.cancelled().await;
            Err(ToolFailure::new("cancelled"))
        })
        .build()
        .expect("server");
    let mut engine = Engine::connect(server, false).await;
    let session = SessionId::new_v7();
    let outcomes = engine.prepare(session, vec![call("wait", json!({}))]).await;
    let prepared_id = prepared(&outcomes[0]).prepared_id;
    let peer = engine.peer.clone();
    let execution = tokio::spawn(async move {
        peer.request::<_, ExecuteResult>(
            method::TOOL_EXECUTE,
            &ExecuteParams {
                prepared_id,
                session,
                run: RunId::new_v7(),
                turn: turn(),
                background: false,
            },
        )
        .await
    });

    let progress = engine.next_request(method::TOOL_PROGRESS).await;
    let params: ToolProgressParams = progress.params().expect("progress params");
    assert_eq!(params.prepared_id, prepared_id);
    assert_eq!(params.progress.message, "waiting");
    assert_eq!(params.progress.output[0].text, "partial");
    progress
        .respond(&Outcome::default())
        .await
        .expect("answer progress");

    engine
        .peer
        .notify(method::TOOL_CANCEL, &PreparedRef { prepared_id })
        .await
        .expect("cancel");
    let result = tokio::time::timeout(WAIT, execution)
        .await
        .expect("the execution ends")
        .expect("execution task")
        .expect("execute");
    assert!(matches!(
        result.outcome,
        ExecuteOutcome::Failed { error } if error.message == "cancelled"
    ));
    engine.shutdown().await;
}

#[tokio::test]
async fn hooks_answer_their_registered_method_only() {
    let server = PluginServer::builder("guard", "0.1.0")
        .tool_before_call(
            |_context, params: ExtensionToolBeforeCallParams| async move {
                assert_eq!(params.origin.known(), Some(&ExtensionToolOrigin::Base));
                allow()
            },
        )
        .build()
        .expect("server");
    let engine = Engine::connect(server, false).await;
    let params = ExtensionToolBeforeCallParams {
        session_id: SessionId::new_v7(),
        context_id: "hook-context".into(),
        tool: "read".into(),
        origin: ExtensionToolOrigin::Base.into(),
        arguments: json!({}),
        permission_name: None,
        resource: None,
    };
    let result: ExtensionToolBeforeCallResult = engine
        .peer
        .request(PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD, &params)
        .await
        .expect("hook result");
    assert_eq!(result.action, ExtensionToolBeforeCallAction::Allow);

    let unregistered = engine
        .peer
        .request::<_, Value>(PLUGIN_INTERCEPT_TOOL_AFTER_RESULT_METHOD, &json!({}))
        .await
        .expect_err("an unregistered hook");
    assert!(matches!(
        unregistered,
        RpcError::Remote {
            code: errors::METHOD_NOT_FOUND,
            ..
        }
    ));
    let malformed = engine
        .peer
        .request::<_, Value>(
            PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD,
            &json!({"tool": 1}),
        )
        .await
        .expect_err("malformed params");
    assert!(matches!(
        malformed,
        RpcError::Remote {
            code: errors::INVALID_PARAMS,
            ..
        }
    ));
    engine.shutdown().await;
}

#[tokio::test]
async fn an_event_handler_emits_once_with_the_delivered_context() {
    let (outcomes, mut emitted) = mpsc::unbounded_channel();
    let server = PluginServer::builder("relay", "0.1.0")
        .enable_bus_publishing()
        .on_event(move |context, event| {
            let outcomes = outcomes.clone();
            async move {
                let first = context
                    .emit_bus(event.session_id, "relayed", json!({"seq": event.seq}))
                    .await;
                let second = context
                    .emit_bus(event.session_id, "relayed", json!({"seq": event.seq}))
                    .await;
                let _ = outcomes.send((first, second));
            }
        })
        .build()
        .expect("server");
    let mut engine = Engine::connect(server, false).await;
    assert!(engine.initialized.capabilities.subscribe_events);
    let session_id = SessionId::new_v7();
    engine
        .peer
        .notify(
            PLUGIN_EVENT_METHOD,
            &ExtensionEventParams {
                session_id,
                context_id: "delivered".into(),
                seq: 7,
                event: cookie_agent_protocol::EventPayload::Diagnostic {
                    source: cookie_agent_protocol::DiagnosticSource::plugin("engine")
                        .expect("source"),
                    kind: cookie_agent_protocol::PluginDiagnosticKind::HookBlocked.label(),
                    message: "event".into(),
                    count: None,
                    detail: None,
                }
                .into(),
                timestamp: jiff::Timestamp::now(),
            },
        )
        .await
        .expect("event");

    let emit: ExtensionEmitParams = engine
        .next_notification(PLUGIN_EMIT_METHOD)
        .await
        .params()
        .expect("emit params");
    assert_eq!(emit.context_id, "delivered");
    assert_eq!(emit.session_id, session_id);
    assert_eq!(emit.payload, json!({"seq": 7}));
    engine
        .peer
        .notify(
            PLUGIN_EMIT_RESULT_METHOD,
            &ExtensionEmitResultParams {
                name: "relayed".into(),
                bus: ExtensionEmitStatus::Published,
                reason: None,
            },
        )
        .await
        .expect("emit result");
    let (first, second) = tokio::time::timeout(WAIT, emitted.recv())
        .await
        .expect("handler finished")
        .expect("outcome channel");
    assert_eq!(first.expect("first emit"), ExtensionEmitStatus::Published);
    assert!(
        matches!(second, Err(PluginError::ContextUnavailable(session)) if session == session_id)
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn recovery_runs_producers_and_then_reports_ready() {
    let session_id = SessionId::new_v7();
    let server = PluginServer::builder("watcher", "0.1.0")
        .enable_producers()
        .on_recovery(move |context| async move {
            let producer = context.register_producer(session_id).await?;
            let message = producer
                .queue(
                    "build finished",
                    "Build watcher",
                    ProducerIdempotencyKey::new("build-1").expect("key"),
                )
                .await?;
            producer.discard(message).await?;
            producer.unregister().await?;
            Ok(())
        })
        .build()
        .expect("server");
    let mut engine = Engine::connect(server, true).await;
    assert!(engine.initialized.capabilities.producer_messaging);
    engine
        .peer
        .notify(PLUGIN_RECOVERY_START_METHOD, &json!({}))
        .await
        .expect("recovery start");

    let producer_id: ProducerId = id();
    let message_id: ProducerMessageId = id();
    let register = engine.next_request(PLUGIN_PRODUCER_REGISTER_METHOD).await;
    let params: ProducerRegisterParams = register.params().expect("register params");
    assert_eq!(params.session_id, session_id);
    assert_eq!(params.name, ProducerName::default_name());
    register
        .respond(&ProducerRegisterResult { producer_id })
        .await
        .expect("register reply");

    let send = engine.next_request(PLUGIN_PRODUCER_SEND_METHOD).await;
    let params: ProducerSendParams = send.params().expect("send params");
    assert_eq!(params.producer_id, producer_id);
    assert_eq!(params.mode, ProducerDeliveryMode::Queue);
    assert_eq!(params.body, "build finished");
    send.respond(&ProducerSendResult { message_id })
        .await
        .expect("send reply");

    let discard = engine.next_request(PLUGIN_PRODUCER_DISCARD_METHOD).await;
    let params: ProducerDiscardParams = discard.params().expect("discard params");
    assert_eq!(params.message_id, message_id);
    discard
        .respond(&ProducerDiscardResult {})
        .await
        .expect("discard reply");

    let unregister = engine.next_request(PLUGIN_PRODUCER_UNREGISTER_METHOD).await;
    unregister
        .respond(&ProducerUnregisterResult {})
        .await
        .expect("unregister reply");

    let complete = engine.next_request(PLUGIN_RECOVERY_COMPLETE_METHOD).await;
    let params: RecoveryCompleteParams = complete.params().expect("complete params");
    assert_eq!(params.outcome, RecoveryOutcome::Ready);
    complete
        .respond(&RecoveryCompleteResult {})
        .await
        .expect("complete reply");
    engine.shutdown().await;
}

#[tokio::test]
async fn producers_stay_closed_when_the_engine_does_not_enable_them() {
    let (outcome, mut registered) = mpsc::unbounded_channel();
    let server = PluginServer::builder("watcher", "0.1.0")
        .enable_producers()
        .tool(declaration("start"), move |context, _arguments| {
            let outcome = outcome.clone();
            async move {
                let result = context
                    .plugin()
                    .register_producer(context.session_id())
                    .await;
                let _ = outcome.send(matches!(
                    result,
                    Err(PluginError::ProducerMessagingNotEnabled)
                ));
                Ok(ToolOutput::success("started"))
            }
        })
        .build()
        .expect("server");
    let engine = Engine::connect(server, false).await;
    // Recovery is the engine's to start, and only for an open producer.
    engine
        .peer
        .notify(PLUGIN_RECOVERY_START_METHOD, &json!({}))
        .await
        .expect("recovery start");
    let session = SessionId::new_v7();
    let outcomes = engine
        .prepare(session, vec![call("start", json!({}))])
        .await;
    let result = engine
        .execute(session, prepared(&outcomes[0]).prepared_id)
        .await;
    assert_eq!(completed_text(&result), ("started", false));
    assert!(registered.recv().await.expect("tool outcome"));
    engine.shutdown().await;
}

#[tokio::test]
async fn a_daemon_of_another_version_is_refused_before_the_rest_is_read() {
    let server = PluginServer::builder("versions", "0.1.0")
        .build()
        .expect("server");
    let (peer, _incoming, _server) = serve(server);
    // Before initialize, requests are refused.
    let early = peer
        .request::<_, Value>(method::TOOL_REVALIDATE, &json!({"prepared_id": 1}))
        .await
        .expect_err("not initialized");
    assert_eq!(early.data_code(), Some("not_initialized"));
    let next_minor = SemVer::new(
        PLUGIN_PROTOCOL_VERSION.major(),
        PLUGIN_PROTOCOL_VERSION.minor() + 1,
        0,
    );
    for version in [json!(next_minor.to_string()), json!("9.0.0"), json!(2)] {
        // The rest of the params is a shape this build does not share.
        let error = peer
            .request::<_, ExtensionInitializeResult>(
                PLUGIN_INITIALIZE_METHOD,
                &json!({"protocol_version": version, "engine": {"name": "future"}}),
            )
            .await
            .expect_err("another version");
        assert_eq!(error.data_code(), Some("unsupported_protocol"), "{error}");
        assert_eq!(
            error.data_member("supported_version"),
            Some(PLUGIN_PROTOCOL_VERSION.to_string().as_str())
        );
    }
    let malformed = peer
        .request::<_, ExtensionInitializeResult>(PLUGIN_INITIALIZE_METHOD, &json!({}))
        .await
        .expect_err("no version");
    assert_eq!(malformed.data_code(), Some("invalid_params"));
    // A patch of the same minor connects.
    let mut params = serde_json::to_value(initialize_params(false)).unwrap();
    params["protocol_version"] = json!(
        SemVer::new(
            PLUGIN_PROTOCOL_VERSION.major(),
            PLUGIN_PROTOCOL_VERSION.minor(),
            PLUGIN_PROTOCOL_VERSION.patch() + 1
        )
        .to_string()
    );
    let initialized: ExtensionInitializeResult = peer
        .request(PLUGIN_INITIALIZE_METHOD, &params)
        .await
        .expect("a patch connects");
    assert_eq!(initialized.protocol_version, PLUGIN_PROTOCOL_VERSION);
    let again = peer
        .request::<_, ExtensionInitializeResult>(PLUGIN_INITIALIZE_METHOD, &params)
        .await
        .expect_err("initialized twice");
    assert_eq!(again.data_code(), Some("already_initialized"));
}

#[tokio::test]
async fn handlers_see_the_daemon_the_handshake_described() {
    let (seen, mut daemons) = mpsc::unbounded_channel();
    let server = PluginServer::builder("observer", "0.1.0")
        .tool(declaration("observe"), move |context, _arguments| {
            let seen = seen.clone();
            async move {
                let _ = seen.send(context.plugin().daemon().clone());
                Ok(ToolOutput::success("seen"))
            }
        })
        .build()
        .expect("server");
    let engine = Engine::connect(server, true).await;
    let session = SessionId::new_v7();
    let outcomes = engine
        .prepare(
            session,
            vec![WireToolCall {
                id: ToolCallId::new_v7(),
                name: "observe".into(),
                arguments: json!({}),
            }],
        )
        .await;
    let PrepareOutcome::Prepared(prepared) = &outcomes[0] else {
        panic!("prepared: {outcomes:?}");
    };
    engine.execute(session, prepared.prepared_id).await;
    let daemon = tokio::time::timeout(WAIT, daemons.recv())
        .await
        .expect("the tool ran")
        .expect("daemon info");
    assert_eq!(daemon.protocol_version(), PLUGIN_PROTOCOL_VERSION);
    assert_eq!(daemon.engine_version(), "test");
    assert!(daemon.has_feature(FEATURE_PRODUCER_MESSAGING));
    assert_eq!(daemon.features(), ["producer_messaging"]);
    assert_eq!(daemon.interception_timeout(), Duration::from_millis(2_000));
    engine.shutdown().await;
}

/// What a newer daemon sends still reaches the plugin: hook params and
/// events with members and types this SDK does not know, and notifications
/// it cannot read, which go to `on_unknown`.
#[tokio::test]
async fn hooks_events_and_notifications_from_a_newer_daemon_reach_the_plugin() {
    let (seen, mut observed) = mpsc::unbounded_channel();
    let (hook_seen, event_seen, unknown_seen) = (seen.clone(), seen.clone(), seen);
    let server = PluginServer::builder("observer", "0.1.0")
        .tool_before_call(move |_context, params: ExtensionToolBeforeCallParams| {
            let seen = hook_seen.clone();
            async move {
                let _ = seen.send(format!("hook {} {}", params.tool, params.origin.tag()));
                allow()
            }
        })
        .on_event(move |_context, event| {
            let seen = event_seen.clone();
            async move {
                let _ = seen.send(format!(
                    "event {} unknown={}",
                    event.event.tag(),
                    event.event.is_unknown()
                ));
            }
        })
        .on_bus_event(|_context, _event| async {})
        .on_unknown(move |_context, message| {
            let seen = unknown_seen.clone();
            async move {
                let _ = seen.send(format!("unknown {}", message.method));
            }
        })
        .build()
        .expect("server");
    let engine = Engine::connect(server, false).await;
    let session_id = SessionId::new_v7();
    let result: ExtensionToolBeforeCallResult = engine
        .peer
        .request(
            PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD,
            &json!({
                "session_id": session_id, "context_id": "c", "tool": "read",
                "origin": {"kind": "mcp", "server": "docs"},
                "arguments": {}, "permission_name": null, "resource": null,
                "turn": {"agent": "build"}
            }),
        )
        .await
        .expect("the hook runs");
    assert_eq!(result.action, ExtensionToolBeforeCallAction::Allow);
    let mut next = async || {
        tokio::time::timeout(WAIT, observed.recv())
            .await
            .expect("observed in time")
            .expect("observation")
    };
    assert_eq!(next().await, "hook read mcp");
    for (method, params) in [
        (
            PLUGIN_EVENT_METHOD,
            json!({"session_id": session_id, "context_id": "e", "seq": 3,
                "event": {"type": "weather_changed", "sky": "clear"},
                "timestamp": jiff::Timestamp::now(), "public": true}),
        ),
        // A bus event without its name cannot be read.
        (
            PLUGIN_BUS_EVENT_METHOD,
            json!({"session_id": session_id, "plugin": "other", "payload": {}}),
        ),
        ("plugin/weather", json!({"sky": "clear"})),
    ] {
        engine
            .peer
            .notify(method, &params)
            .await
            .expect("notification");
    }
    let mut seen = vec![next().await, next().await, next().await];
    seen.sort();
    assert_eq!(
        seen,
        [
            "event weather_changed unknown=true",
            "unknown plugin/bus_event",
            "unknown plugin/weather",
        ]
    );
    engine.shutdown().await;
}

/// An emit the daemon does not answer within the interception timeout
/// resolves as dropped, and its late answer is not taken for the next one's.
#[tokio::test]
async fn an_unanswered_emit_resolves_dropped_after_the_interception_timeout() {
    let (outcomes, mut emitted) = mpsc::unbounded_channel();
    let server = PluginServer::builder("relay", "0.1.0")
        .enable_bus_publishing()
        .on_event(move |context, event| {
            let outcomes = outcomes.clone();
            async move {
                let status = context
                    .emit_bus(event.session_id, "relayed", json!({"seq": event.seq}))
                    .await;
                let _ = outcomes.send((event.seq, status.expect("emit")));
            }
        })
        .build()
        .expect("server");
    let mut params = initialize_params(false);
    params.interception_timeout_ms = 50;
    let mut engine = Engine::connect_with(server, params).await;
    let session_id = SessionId::new_v7();
    let event = |seq: u64| ExtensionEventParams {
        session_id,
        context_id: format!("event-{seq}"),
        seq,
        event: cookie_agent_protocol::EventPayload::Diagnostic {
            source: cookie_agent_protocol::DiagnosticSource::plugin("engine").expect("source"),
            kind: cookie_agent_protocol::PluginDiagnosticKind::HookBlocked.label(),
            message: "event".into(),
            count: None,
            detail: None,
        }
        .into(),
        timestamp: jiff::Timestamp::now(),
    };
    let result = |bus: ExtensionEmitStatus| ExtensionEmitResultParams {
        name: "relayed".into(),
        bus,
        reason: None,
    };
    engine
        .peer
        .notify(PLUGIN_EVENT_METHOD, &event(1))
        .await
        .expect("event");
    engine.next_notification(PLUGIN_EMIT_METHOD).await;
    let first = tokio::time::timeout(WAIT, emitted.recv())
        .await
        .expect("the first emit resolves")
        .expect("outcome");
    assert_eq!(first, (1, ExtensionEmitStatus::Dropped));

    engine
        .peer
        .notify(PLUGIN_EVENT_METHOD, &event(2))
        .await
        .expect("event");
    engine.next_notification(PLUGIN_EMIT_METHOD).await;
    // The first emit's late answer, then the second's.
    for bus in [
        ExtensionEmitStatus::Published,
        ExtensionEmitStatus::Rejected,
    ] {
        engine
            .peer
            .notify(PLUGIN_EMIT_RESULT_METHOD, &result(bus))
            .await
            .expect("emit result");
    }
    let second = tokio::time::timeout(WAIT, emitted.recv())
        .await
        .expect("the second emit resolves")
        .expect("outcome");
    assert_eq!(second, (2, ExtensionEmitStatus::Rejected));
    engine.shutdown().await;
}
