//! Ratatui frontend for cookie agent's versioned JSON-RPC protocol.

pub mod client;
pub mod config;
pub mod markdown;
pub mod state;
pub mod theme;
pub mod ui;

pub use client::{Client, ClientDelivery, ClientError, read_daemon_token, validate_websocket_url};
pub use ui::{run_with_client, run_with_new_session};

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use cookie_agent_config::{Config, load_layered};
    use cookie_agent_engine::{Engine, EngineOptions};
    use cookie_agent_models::{Catalog, CredentialStore, ModelSetManager};
    use cookie_agent_protocol::{
        ActionKind, ApprovalBoundary, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation,
        ApprovalId, ApprovalRequest, ApprovalResourceSource, ApprovalTrigger, DecisionTrace,
        Effect, Event, EventEnvelope, EventSchemaVersion, EventSubscriptionMessage,
        MatchedPermissionRule, ModelListParams, OutputDelta, OutputSnapshot,
        OutputSnapshotEnvelope, OutputStream, PreparedApprovalResource, PreparedBindingLifetime,
        PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
        PreparedResourceIdentity, SessionCreateParams, SessionId, Sha256Digest, ToolCallId,
    };
    use cookie_agent_server::Server;
    use jiff::Timestamp;
    use ratatui::{Terminal, backend::TestBackend};

    use crate::{
        Client,
        state::{OrderedOutput, StateStore},
        ui::App,
    };

    fn in_process_server() -> (tempfile::TempDir, Arc<Server>) {
        let directory = tempfile::tempdir().expect("temporary data directory");
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"
[models.scripted]
provider_id = "test"
model_id = "scripted"
endpoint = "https://example.test/v1"
adaptor = "openai-responses"

[models.scripted.auth]
type = "openai"
api_key = "test-secret"

[models.scripted.capabilities]
features = ["max_output_tokens", "tool_calling"]
cancellation = "local_only"
compaction = "unsupported"

[models.scripted.capabilities.limits]
context = 4096

[models.scripted.capabilities.modalities]
input = ["text"]
output = ["text"]

[models.scripted.capabilities.media]
input = {}

[models.scripted.capabilities.replay]
policy = "never"
capability = "unsupported"
reasoning = false

[models.scripted.settings]

[agents.primary]
type = "primary"
models = ["scripted"]
"#,
        )
        .expect("write test config");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .expect("private test root");
        }
        let config: Config = load_layered(None, Some(&path)).expect("load test config");
        let catalog = Arc::new(Catalog::embedded().expect("embedded catalog"));
        let model_manager = Arc::new(
            ModelSetManager::new(
                config.models.clone(),
                Arc::clone(&catalog),
                CredentialStore::new(directory.path().join("credentials")),
            )
            .expect("model manager"),
        );
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config: config.clone(),
            model_manager: Arc::clone(&model_manager),
            tools: Vec::new(),
        })
        .expect("open engine");
        (
            directory,
            Arc::new(Server::new(engine, model_manager, catalog)),
        )
    }

    fn call_id() -> ToolCallId {
        ToolCallId::new_v7()
    }

    fn approval_request() -> ApprovalRequest {
        let resource = PreparedApprovalResource {
            capability: ActionKind::Bash,
            canonical: PreparedResourceIdentity::new("command:git-status")
                .expect("prepared resource identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"git status"),
            binding_lifetime: PreparedBindingLifetime::RestartStable,
            boundary: ApprovalBoundary::CommandPrefix {
                prefix: "git status".into(),
            },
            source: ApprovalResourceSource::PrimaryOperation,
        };
        let resource_digest = resource.binding_digest.clone();
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"normalized arguments"),
            vec![ApprovalCapability {
                action: ActionKind::Bash,
                operation: PreparedCapabilityOperation::new("execute")
                    .expect("prepared capability operation"),
            }],
            vec![resource],
            Sha256Digest::of_bytes(b"execution context"),
        )
        .expect("prepared operation");
        ApprovalRequest::new(
            ApprovalId::new_v7(),
            7,
            ApprovalTrigger::PermissionPolicy,
            operation,
            vec![ApprovalEvaluation {
                resource_digest,
                effect: Effect::Ask,
                trace: DecisionTrace {
                    action: ActionKind::Bash,
                    normalized_resource: "git status".into(),
                    candidates: vec![MatchedPermissionRule {
                        rule_id: None,
                        source_layer: "test".into(),
                        effect: Effect::Ask,
                    }],
                    effect: Effect::Ask,
                    precedence_reason: "test".into(),
                },
            }],
            ApprovalConstraints {
                allow_once: true,
                allow_tree_grant: true,
                cancellable: true,
                expires_at: None,
            },
        )
        .expect("approval request")
    }

    #[test]
    fn output_snapshot_handoff_and_out_of_order_deltas_are_lossless() {
        let mut output = OrderedOutput::default();
        output.replace_snapshot(
            0,
            3,
            vec![OutputDelta {
                call_id: call_id(),
                stream: OutputStream::Stdout,
                byte_offset: 0,
                data: STANDARD.encode(b"one"),
            }],
        );
        output.push(OutputDelta {
            call_id: call_id(),
            stream: OutputStream::Stdout,
            byte_offset: 6,
            data: STANDARD.encode(b"six"),
        });
        output.push(OutputDelta {
            call_id: call_id(),
            stream: OutputStream::Stdout,
            byte_offset: 3,
            data: STANDARD.encode(b"two"),
        });
        assert_eq!(output.text(), "onetwosix");
        assert_eq!(output.next_offset, 9);
    }

    #[test]
    fn empty_snapshot_keeps_its_explicit_stream_identity() {
        let envelope = OutputSnapshotEnvelope {
            stream: OutputStream::Stderr,
            snapshot: OutputSnapshot {
                call_id: call_id(),
                start_offset: 0,
                end_offset: 0,
                chunks: Vec::new(),
            },
        };
        assert_eq!(envelope.stream, OutputStream::Stderr);
    }

    #[test]
    fn early_output_snapshot_is_applied_after_tool_replay() {
        let session = SessionId::new_v7();
        let call = call_id();
        let mut store = StateStore::default();
        store.apply_snapshot(OutputSnapshotEnvelope {
            stream: OutputStream::Stdout,
            snapshot: OutputSnapshot {
                call_id: call,
                start_offset: 0,
                end_offset: 4,
                chunks: vec![OutputDelta {
                    call_id: call,
                    stream: OutputStream::Stdout,
                    byte_offset: 0,
                    data: STANDARD.encode(b"live"),
                }],
            },
        });
        store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::ToolCallStarted {
                tool_call_id: call,
                model_call_id: "model-call".into(),
                provider_item_id: None,
                tool: "bash".into(),
                arguments: serde_json::json!({}),
            },
        });
        assert_eq!(
            store.sessions[&session].output[&(call, false)].text(),
            "live"
        );
    }

    #[test]
    fn terminal_run_expires_pending_approvals() {
        let session = SessionId::new_v7();
        let mut store = StateStore::default();
        store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::ApprovalRequested {
                request: approval_request(),
            },
        });
        store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: None,
            seq: 2,
            timestamp: Timestamp::now(),
            event: Event::RunCompleted { final_text: None },
        });
        assert!(store.sessions[&session].approvals.is_empty());
    }

    #[test]
    fn unknown_output_overflow_is_rendered_as_a_gap() {
        let session = SessionId::new_v7();
        let call = call_id();
        let mut store = StateStore::default();
        for offset in 0..129 {
            store.apply_output_delta(OutputDelta {
                call_id: call,
                stream: OutputStream::Stderr,
                byte_offset: offset,
                data: STANDARD.encode(b"x"),
            });
        }
        store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::ToolCallStarted {
                tool_call_id: call,
                model_call_id: "model-call".into(),
                provider_item_id: None,
                tool: "bash".into(),
                arguments: serde_json::json!({}),
            },
        });
        assert!(store.sessions[&session].output[&(call, true)].has_gap);
        assert!(!store.sessions[&session].output.contains_key(&(call, false)));
    }

    #[tokio::test]
    async fn app_draws_conversation_approval_tree_and_input() {
        let (directory, server) = in_process_server();
        let client = Client::connect_in_process(server);
        client.handshake().await.expect("handshake");
        let models = client
            .list_models(ModelListParams::default())
            .await
            .expect("list models");
        assert_eq!(models.models[0].name, "scripted");
        let session = client
            .create_session(SessionCreateParams {
                cwd: directory.path().display().to_string(),
                profile: "primary".into(),
            })
            .await
            .expect("session")
            .session;
        let mut app = App::new(client).await.expect("TUI config");
        app.store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session.id,
            run_id: None,
            seq: 2,
            timestamp: Timestamp::now(),
            event: Event::TextDelta {
                text: "streamed assistant text".into(),
            },
        });
        let request = approval_request();
        let approval_id = request.approval_id();
        app.store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session.id,
            run_id: None,
            seq: 3,
            timestamp: Timestamp::now(),
            event: Event::ApprovalRequested { request },
        });
        app.store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session.id,
            run_id: None,
            seq: 4,
            timestamp: Timestamp::now(),
            event: Event::ApprovalEscalated {
                approval_id,
                reason_code: cookie_agent_protocol::ApprovalReasonCode::Escalated,
            },
        });
        for (width, height) in [(40, 12), (80, 24), (160, 50)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal
                .draw(|frame| app.draw_for_test(frame))
                .expect("app render");
            let buffer = terminal.backend().buffer();
            let rendered = (0..buffer.area.height)
                .flat_map(|y| {
                    (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned())
                })
                .collect::<String>();
            assert!(rendered.contains("Approval"));
            assert!(rendered.contains("Agents"));
            if width >= 80 {
                assert!(rendered.contains("Conversation"));
            }
            if (width, height) == (80, 24) {
                assert!(rendered.contains("ASSISTANT"));
                assert!(rendered.contains("git status"));
            }
        }
    }

    #[tokio::test]
    async fn client_round_trips_handshake_create_and_event_replay_in_process() {
        let (directory, server) = in_process_server();
        let client = Client::connect_in_process(server);
        client.handshake().await.expect("handshake");
        let session = client
            .create_session(SessionCreateParams {
                cwd: directory.path().display().to_string(),
                profile: "primary".into(),
            })
            .await
            .expect("create session")
            .session;
        let mut deliveries = client.subscribe_deliveries().expect("delivery receiver");
        client
            .subscribe_events(session.id, None)
            .await
            .expect("subscribe events");
        let mut replayed = Vec::new();
        loop {
            match deliveries.recv().await.expect("delivery") {
                crate::ClientDelivery::ReplayEvent { event, .. } => replayed.push(*event),
                crate::ClientDelivery::ReplayEnd { .. } => break,
                _ => {}
            }
        }
        assert!(!replayed.is_empty());
        assert!(replayed.iter().all(|event| event.session_id == session.id));
    }

    #[tokio::test]
    async fn local_startup_creates_a_fresh_root_while_attach_keeps_existing_selection() {
        let (directory, server) = in_process_server();
        let setup = Client::connect_in_process(server.clone());
        setup.handshake().await.expect("setup handshake");
        let existing = setup
            .create_session(SessionCreateParams {
                cwd: directory.path().display().to_string(),
                profile: "primary".into(),
            })
            .await
            .expect("existing session")
            .session;

        let attached = Client::connect_in_process(server.clone());
        attached.handshake().await.expect("attach handshake");
        let attached_app = App::new(attached).await.expect("attached TUI config");
        assert_eq!(attached_app.store.sessions.len(), 1);
        assert!(attached_app.store.sessions.contains_key(&existing.id));

        let local = Client::connect_in_process(server);
        local.handshake().await.expect("local handshake");
        let local_app = App::new_with_new_session(local)
            .await
            .expect("local TUI config");
        assert_eq!(local_app.store.sessions.len(), 1);
        let local_session = local_app
            .store
            .sessions
            .keys()
            .next()
            .copied()
            .expect("new local root session");
        assert_ne!(local_session, existing.id);
    }

    #[test]
    fn state_store_reduces_events_and_surfaces_gap_cursor() {
        let session = SessionId::new_v7();
        let mut store = StateStore::default();
        store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::TextDelta {
                text: "hello".into(),
            },
        });
        // Replayed duplicate is ignored, preserving the projection.
        store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::TextDelta {
                text: "duplicate".into(),
            },
        });
        assert_eq!(store.sessions[&session].last_seq, 1);
        assert_eq!(
            store.apply_subscription(EventSubscriptionMessage::Gap {
                session_id: session,
                last_delivered_seq: 1,
            }),
            Some(1)
        );
    }

    #[test]
    fn missing_replay_end_keeps_the_visible_projection() {
        let session = SessionId::new_v7();
        let call = call_id();
        let mut store = StateStore::default();
        store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::TextDelta { text: "old".into() },
        });
        store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: None,
            seq: 2,
            timestamp: Timestamp::now(),
            event: Event::ToolCallStarted {
                tool_call_id: call,
                model_call_id: "model-call".into(),
                provider_item_id: None,
                tool: "bash".into(),
                arguments: serde_json::json!({}),
            },
        });
        assert_eq!(store.sessions[&session].generation, 0);
        assert!(matches!(
            store.apply_delivery(crate::ClientDelivery::ReplayStart {
                session_id: session,
                generation: 1,
                final_seq: 2,
                rebuild: true,
            }),
            crate::state::DeliveryOutcome::Applied
        ));
        assert!(matches!(
            store.apply_delivery(crate::ClientDelivery::ReplayEvent {
                session_id: session,
                generation: 1,
                final_seq: 2,
                event: Box::new(EventEnvelope {
                    schema_version: EventSchemaVersion::current(),
                    session_id: session,
                    run_id: None,
                    seq: 2,
                    timestamp: Timestamp::now(),
                    event: Event::ToolCallStarted {
                        tool_call_id: call,
                        model_call_id: "model-call".into(),
                        provider_item_id: None,
                        tool: "bash".into(),
                        arguments: serde_json::json!({}),
                    },
                }),
            }),
            crate::state::DeliveryOutcome::ReplayFailed { session_id } if session_id == session
        ));
        assert_eq!(store.sessions[&session].generation, 0);
        assert_eq!(store.sessions[&session].last_seq, 2);
        assert!(store.abandon_replays().is_empty());
        assert_eq!(store.sessions[&session].generation, 0);
        assert!(matches!(
            store.apply_delivery(crate::ClientDelivery::OutputSnapshot(OutputSnapshotEnvelope {
                stream: OutputStream::Stdout,
                snapshot: OutputSnapshot {
                    call_id: call,
                    start_offset: 0,
                    end_offset: 0,
                    chunks: Vec::new(),
                },
            })),
            crate::state::DeliveryOutcome::ReplayFailed { session_id } if session_id == session
        ));
        assert!(!store.sessions[&session].output.contains_key(&(call, false)));
    }

    #[test]
    fn buffered_output_overflow_during_replay_never_marks_visible_state() {
        let session = SessionId::new_v7();
        let call = call_id();
        let mut store = StateStore::default();
        store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::TextDelta {
                text: "visible".into(),
            },
        });
        let _ = store.apply_delivery(crate::ClientDelivery::ReplayStart {
            session_id: session,
            generation: 1,
            final_seq: 1,
            rebuild: true,
        });
        for offset in 0..129 {
            let _ = store.apply_delivery(crate::ClientDelivery::OutputDelta(OutputDelta {
                call_id: call,
                stream: OutputStream::Stdout,
                byte_offset: offset,
                data: STANDARD.encode(b"x"),
            }));
        }
        let _ = store.apply_delivery(crate::ClientDelivery::ReplayEvent {
            session_id: session,
            generation: 1,
            final_seq: 1,
            event: Box::new(EventEnvelope {
                schema_version: EventSchemaVersion::current(),
                session_id: session,
                run_id: None,
                seq: 1,
                timestamp: Timestamp::now(),
                event: Event::ToolCallStarted {
                    tool_call_id: call,
                    model_call_id: "model-call".into(),
                    provider_item_id: None,
                    tool: "bash".into(),
                    arguments: serde_json::json!({}),
                },
            }),
        });
        assert_eq!(store.sessions[&session].generation, 0);
        assert!(!store.sessions[&session].output.contains_key(&(call, false)));
    }

    #[test]
    fn validated_rereplay_clears_session_output_quarantine() {
        let session = SessionId::new_v7();
        let call = call_id();
        let mut store = StateStore::default();
        store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::ToolCallStarted {
                tool_call_id: call,
                model_call_id: "model-call".into(),
                provider_item_id: None,
                tool: "bash".into(),
                arguments: serde_json::json!({}),
            },
        });
        let _ = store.apply_delivery(crate::ClientDelivery::ReplayStart {
            session_id: session,
            generation: 1,
            final_seq: 2,
            rebuild: true,
        });
        let _ = store.apply_delivery(crate::ClientDelivery::ReplayEvent {
            session_id: session,
            generation: 1,
            final_seq: 2,
            event: Box::new(EventEnvelope {
                schema_version: EventSchemaVersion::current(),
                session_id: session,
                run_id: None,
                seq: 2,
                timestamp: Timestamp::now(),
                event: Event::ToolCallStarted {
                    tool_call_id: call,
                    model_call_id: "model-call".into(),
                    provider_item_id: None,
                    tool: "bash".into(),
                    arguments: serde_json::json!({}),
                },
            }),
        });
        let _ = store.apply_delivery(crate::ClientDelivery::ReplayStart {
            session_id: session,
            generation: 2,
            final_seq: 1,
            rebuild: true,
        });
        let _ = store.apply_delivery(crate::ClientDelivery::ReplayEvent {
            session_id: session,
            generation: 2,
            final_seq: 1,
            event: Box::new(EventEnvelope {
                schema_version: EventSchemaVersion::current(),
                session_id: session,
                run_id: None,
                seq: 1,
                timestamp: Timestamp::now(),
                event: Event::ToolCallStarted {
                    tool_call_id: call,
                    model_call_id: "model-call".into(),
                    provider_item_id: None,
                    tool: "bash".into(),
                    arguments: serde_json::json!({}),
                },
            }),
        });
        assert!(matches!(
            store.apply_delivery(crate::ClientDelivery::ReplayEnd {
                session_id: session,
                generation: 2,
                final_seq: 1,
            }),
            crate::state::DeliveryOutcome::Applied
        ));
        assert!(matches!(
            store.apply_delivery(crate::ClientDelivery::OutputSnapshot(
                OutputSnapshotEnvelope {
                    stream: OutputStream::Stdout,
                    snapshot: OutputSnapshot {
                        call_id: call,
                        start_offset: 0,
                        end_offset: 0,
                        chunks: Vec::new(),
                    },
                }
            )),
            crate::state::DeliveryOutcome::Applied
        ));
        assert!(store.sessions[&session].output.contains_key(&(call, false)));
    }

    #[test]
    fn rebuild_refuses_quarantined_session_without_erasing_visible_state() {
        let session = SessionId::new_v7();
        let mut store = StateStore::default();
        store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::TextDelta {
                text: "visible".into(),
            },
        });
        let _ = store.apply_delivery(crate::ClientDelivery::ReplayStart {
            session_id: session,
            generation: 1,
            final_seq: 2,
            rebuild: true,
        });
        let failed = store.apply_delivery(crate::ClientDelivery::ReplayEvent {
            session_id: session,
            generation: 1,
            final_seq: 2,
            event: Box::new(EventEnvelope {
                schema_version: EventSchemaVersion::current(),
                session_id: session,
                run_id: None,
                seq: 2,
                timestamp: Timestamp::now(),
                event: Event::TextDelta {
                    text: "invalid".into(),
                },
            }),
        });
        assert!(matches!(
            failed,
            crate::state::DeliveryOutcome::ReplayFailed { session_id } if session_id == session
        ));
        assert!(!store.rebuild_session(
            session,
            2,
            vec![EventEnvelope {
                schema_version: EventSchemaVersion::current(),
                session_id: session,
                run_id: None,
                seq: 1,
                timestamp: Timestamp::now(),
                event: Event::TextDelta {
                    text: "replacement".into()
                },
            }]
        ));
        assert_eq!(store.sessions[&session].generation, 0);
        assert_eq!(store.sessions[&session].last_seq, 1);
    }

    #[test]
    fn rebuild_non_quarantined_session_still_replaces_projection() {
        let session = SessionId::new_v7();
        let mut store = StateStore::default();
        store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::TextDelta { text: "old".into() },
        });
        assert!(store.rebuild_session(
            session,
            1,
            vec![EventEnvelope {
                schema_version: EventSchemaVersion::current(),
                session_id: session,
                run_id: None,
                seq: 1,
                timestamp: Timestamp::now(),
                event: Event::TextDelta { text: "new".into() },
            }]
        ));
        assert_eq!(store.sessions[&session].generation, 1);
        assert_eq!(store.sessions[&session].last_seq, 1);
    }
}
