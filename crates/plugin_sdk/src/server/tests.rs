use cookie_agent_protocol::{
    AdaptorId, AgentId, ExtensionToolBeforeCallAction, ExtensionToolOrigin, PermissionAction,
    ToolCompletionOutput,
    runtime_protocol::{ToolResult, ToolTurn, ToolTurnModel},
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

fn turn() -> ToolTurn {
    ToolTurn {
        agent: AgentId::new("test").expect("agent ID"),
        model: ToolTurnModel {
            model: "test/model".parse().expect("model key"),
            adapter: AdaptorId::OPENAI_CHAT,
            input_media: Vec::new(),
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
        ExecuteOutcome::Completed(ToolResult {
            output: ToolCompletionOutput::Single { text },
            failed,
            ..
        }) => (text, *failed),
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
        .on_hook_error(
            ExtensionInterceptionHook::ToolBeforeCall,
            ExtensionHookOnError::Block,
        )
        .on_event(|_context, _event| async {})
        .event_patterns(["run_*", "diagnostic"])
        .enable_bus_publishing();
    let capabilities = builder.capabilities();
    assert_eq!(
        serde_json::to_value(&capabilities.subscribe_events).expect("patterns JSON"),
        json!(["run_*", "diagnostic"])
    );
    assert!(!capabilities.subscribe_bus);
    assert!(capabilities.publish_bus);
    assert!(!capabilities.producer_messaging);
    // Advertised in the hook table's order, whatever the registration order,
    // each with its failure policy.
    assert_eq!(
        serde_json::to_value(&capabilities.intercept).expect("intercept JSON"),
        json!([
            {"hook": "tool_before_call", "on_error": "block"},
            {"hook": "tool_after_result", "on_error": "allow"},
        ])
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
    assert!(
        error(PluginServer::builder("guard", "1").on_hook_error(
            ExtensionInterceptionHook::ToolBeforeCall,
            ExtensionHookOnError::Block,
        ))
        .to_string()
        .contains("`tool_before_call`, which has no handler")
    );
    assert!(
        error(PluginServer::builder("watcher", "1").event_patterns(["run_*"]))
            .to_string()
            .contains("event_patterns requires on_event")
    );
    assert!(
        error(
            PluginServer::builder("watcher", "1")
                .on_event(|_context, _event| async {})
                .event_patterns(["run*_x"])
        )
        .to_string()
        .contains("invalid event pattern `run*_x`")
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
    assert_eq!(echo.presentation.title.as_str(), "echo");
    let guarded = prepared(&outcomes[1]);
    assert_eq!(
        guarded.permission.as_ref().map(PermissionAction::as_str),
        Some("issue_read")
    );
    assert_eq!(guarded.permission_resource.as_deref(), Some("42"));
    assert_eq!(
        guarded
            .presentation
            .primary_argument
            .as_ref()
            .map(|argument| argument.as_str()),
        Some("42")
    );
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

/// A handler that ignores its cancellation is aborted once the grace runs
/// out, so the call still ends.
#[tokio::test]
async fn a_tool_that_ignores_its_cancellation_is_aborted_after_the_grace() {
    let dropped = std::sync::Arc::new(tokio::sync::Notify::new());
    let on_drop = std::sync::Arc::clone(&dropped);
    let server = PluginServer::builder("wedged", "0.1.0")
        .tool(declaration("wedge"), move |_context, _arguments| {
            let on_drop = std::sync::Arc::clone(&on_drop);
            async move {
                struct Signal(std::sync::Arc<tokio::sync::Notify>);
                impl Drop for Signal {
                    fn drop(&mut self) {
                        self.0.notify_one();
                    }
                }
                let _signal = Signal(on_drop);
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                Ok(ToolOutput::success("woke up"))
            }
        })
        .build()
        .expect("server");
    let engine = Engine::connect(server, false).await;
    let session = SessionId::new_v7();
    let outcomes = engine
        .prepare(session, vec![call("wedge", json!({}))])
        .await;
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
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let cancelled = std::time::Instant::now();
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
    assert!(cancelled.elapsed() < std::time::Duration::from_secs(4));
    assert!(matches!(
        result.outcome,
        ExecuteOutcome::Failed { error } if error.message.contains("did not stop within 2 s")
    ));
    tokio::time::timeout(WAIT, dropped.notified())
        .await
        .expect("the handler is dropped");
    engine.shutdown().await;
}

/// A tool's presentation, display, progress display, metadata, and
/// attachments cross the protocol as the builders made them.
#[tokio::test]
async fn a_tool_presents_itself_and_returns_what_its_output_builder_holds() {
    use crate::{DisplayBlock, DisplayDelta, Presentation};

    let mut guarded = declaration("close_issue");
    guarded.permission = Some(PermissionAction::new("issue_write").expect("action"));
    let server = PluginServer::builder("issues", "0.1.0")
        .tool_with_resource(
            guarded,
            |arguments| arguments["issue"].as_str().map(str::to_owned),
            |context, _arguments| async move {
                context
                    .progress_display(
                        "closing",
                        DisplayDelta::Replace {
                            blocks: vec![cookie_agent_protocol::Open::Known(DisplayBlock::text(
                                "closing…",
                            ))],
                        },
                    )
                    .await
                    .map_err(|error| ToolFailure::new(error.to_string()))?;
                Ok(ToolOutput::success("closed #12")
                    .title("Closed issue #12")
                    .display(DisplayBlock::text("#12 is closed"))
                    .metadata(json!({"issue": 12}))
                    .attachment("image/png", Some("badge.png".into()), b"png bytes".to_vec()))
            },
        )
        .prepare_presentation("close_issue", |call| {
            Presentation::new(format!("Close issue {}", call.resource.unwrap_or("?")))
                .hint("generic")
                .primary_argument(call.resource.unwrap_or_default())
                .preview(DisplayBlock::text("The issue is closed as completed."))
        })
        .tool(declaration("echo"), |_context, arguments| async move {
            Ok(ToolOutput::error(arguments["text"].as_str().unwrap_or_default()).failed(false))
        })
        .build()
        .expect("server");
    let mut engine = Engine::connect(server, false).await;
    let session = SessionId::new_v7();
    let outcomes = engine
        .prepare(
            session,
            vec![
                call("close_issue", json!({"issue": "12"})),
                call("echo", json!({"text": "hi"})),
            ],
        )
        .await;
    let close = prepared(&outcomes[0]);
    assert_eq!(
        serde_json::to_value(&close.presentation).unwrap(),
        json!({
            "title": "Close issue 12",
            "hint": "generic",
            "primary_argument": "12",
            "preview": [{"type": "text", "text": "The issue is closed as completed."}],
        })
    );
    // Without a hook the call shows the tool's name and its resource.
    let echo = prepared(&outcomes[1]);
    assert_eq!(echo.presentation.title.as_str(), "echo");
    assert!(echo.presentation.primary_argument.is_none());

    let prepared_id = close.prepared_id;
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
    assert_eq!(
        serde_json::to_value(&params.progress.display).unwrap(),
        json!({"type": "replace", "blocks": [{"type": "text", "text": "closing…"}]})
    );
    progress
        .respond(&Outcome::default())
        .await
        .expect("answer progress");
    let result = tokio::time::timeout(WAIT, execution)
        .await
        .expect("the execution ends")
        .expect("execution task")
        .expect("execute");
    let ExecuteOutcome::Completed(completed) = &result.outcome else {
        panic!("the call completes: {:?}", result.outcome);
    };
    assert_eq!(
        completed.output,
        ToolCompletionOutput::Single {
            text: "closed #12".into()
        }
    );
    assert_eq!(
        completed.title.as_ref().map(|title| title.as_str()),
        Some("Closed issue #12")
    );
    assert_eq!(
        serde_json::to_value(&completed.display).unwrap(),
        json!([{"type": "text", "text": "#12 is closed"}])
    );
    assert_eq!(completed.metadata, Some(json!({"issue": 12})));
    assert!(!completed.failed);
    let attachment = &completed.attachments[0];
    assert_eq!(attachment.mime_type.as_str(), "image/png");
    assert_eq!(attachment.byte_length, 9);
    assert_eq!(result.media[0].sha256, attachment.sha256.as_str());
    assert_eq!(
        attachment.reference,
        cookie_agent_protocol::runtime_protocol::artifact_reference(attachment.sha256.as_str())
    );

    let result = engine.execute(session, echo.prepared_id).await;
    assert_eq!(completed_text(&result), ("hi", false));
    engine.shutdown().await;
}

#[test]
fn a_presentation_for_an_unregistered_tool_fails_the_build() {
    let result = PluginServer::builder("issues", "0.1.0")
        .tool(declaration("echo"), |_context, _arguments| async {
            Ok(ToolOutput::success("ok"))
        })
        .prepare_presentation("missing", crate::Presentation::of_call)
        .build();
    assert!(
        result
            .err()
            .expect("a build error")
            .to_string()
            .contains("prepare_presentation names `missing`")
    );
}

#[test]
fn hook_content_helpers_rewrite_text_and_keep_everything_else() {
    use cookie_agent_protocol::{
        ArtifactRef, ExtensionMessageRole, HookAssistantPart, HookContentPart, HookHistoryMessage,
        Open, UserContent, UserContentPart,
    };

    let file: ArtifactRef = serde_json::from_value(json!({
        "sha256": "a".repeat(64), "byte_length": 3, "media_type": "image/png",
    }))
    .unwrap();
    let input = UserContent::new(vec![
        Open::Known(UserContentPart::Text {
            text: "hello".into(),
        }),
        Open::Known(UserContentPart::File {
            artifact: file.clone(),
        }),
    ])
    .unwrap();
    let result = crate::rewrite_input_text(&input, str::to_uppercase).expect("content");
    assert_eq!(
        result.action,
        cookie_agent_protocol::ExtensionUserBeforeInputAction::Transform
    );
    assert_eq!(
        serde_json::to_value(result.content.unwrap()).unwrap(),
        json!([
            {"type": "text", "text": "HELLO"},
            {"type": "file", "artifact": file},
        ])
    );

    let unchanged = HookHistoryMessage {
        role: ExtensionMessageRole::Tool,
        content: vec![Open::Known(HookContentPart::ToolResult {
            tool_call_id: "c1".into(),
            content: "secret".into(),
            is_error: false,
        })],
    };
    let changed = HookHistoryMessage {
        role: ExtensionMessageRole::User,
        content: vec![Open::Known(HookContentPart::Text {
            text: "a secret".into(),
        })],
    };
    let result = crate::rewrite_history_text(&[unchanged.clone(), changed], |text| {
        text.replace("secret", "[redacted]")
    });
    let messages = result.messages.expect("messages");
    assert_eq!(messages[0], unchanged);
    assert_eq!(
        messages[1].content,
        [Open::Known(HookContentPart::Text {
            text: "a [redacted]".into()
        })]
    );

    let unknown: Open<HookAssistantPart> =
        serde_json::from_value(json!({"type": "hologram"})).unwrap();
    let result = crate::rewrite_message_text(
        &[
            Open::Known(HookAssistantPart::Text { text: "hi".into() }),
            unknown,
        ],
        |text| format!("{text}!"),
    );
    assert_eq!(
        result.content.unwrap(),
        [Open::Known(HookAssistantPart::Text { text: "hi!".into() })]
    );
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
        .request(
            cookie_agent_protocol::PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD,
            &params,
        )
        .await
        .expect("hook result");
    assert_eq!(result.action, ExtensionToolBeforeCallAction::Allow);

    let unregistered = engine
        .peer
        .request::<_, Value>(
            cookie_agent_protocol::PLUGIN_INTERCEPT_TOOL_AFTER_RESULT_METHOD,
            &json!({}),
        )
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
            cookie_agent_protocol::PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD,
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
    // An event handler alone subscribes to every event type.
    assert_eq!(
        serde_json::to_value(&engine.initialized.capabilities.subscribe_events)
            .expect("patterns JSON"),
        json!(["*"])
    );
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
    for version in [json!("0.5.0"), json!("9.0.0"), json!(2)] {
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
    // Another minor and patch of the same major connects.
    let mut params = serde_json::to_value(initialize_params(false)).unwrap();
    params["protocol_version"] = json!(
        SemVer::new(
            PLUGIN_PROTOCOL_VERSION.major(),
            PLUGIN_PROTOCOL_VERSION.minor() + 2,
            PLUGIN_PROTOCOL_VERSION.patch() + 1
        )
        .to_string()
    );
    let initialized: ExtensionInitializeResult = peer
        .request(PLUGIN_INITIALIZE_METHOD, &params)
        .await
        .expect("another minor connects");
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
