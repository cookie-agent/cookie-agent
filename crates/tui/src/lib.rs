//! Ratatui frontend for cookie agent's versioned JSON-RPC protocol.

pub mod client;
pub mod state;
pub mod ui;

pub use client::{Client, ClientDelivery, ClientError};
pub use ui::run_with_client;

/// Kept for the composition root's pre-Phase-5 entrypoint. Embedders must
/// construct a [`Client`] (in-process or WebSocket), handshake it, then call
/// [`run_with_client`].
pub fn run() -> anyhow::Result<()> {
    Err(anyhow::anyhow!(
        "the TUI needs a connected client; use cookie_agent_tui::run_with_client"
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap},
        sync::Arc,
    };

    use async_trait::async_trait;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use cookie_agent_config::{AgentProfile, Config, ModelConfig, ProviderConfig, ProviderType};
    use cookie_agent_engine::{Engine, EngineOptions};
    use cookie_agent_protocol::{
        ActionKind, ApprovalResource, DecisionTrace, Effect, Event, EventEnvelope,
        EventSubscriptionMessage, MatchedPermissionRule, OutputDelta, OutputSnapshot,
        OutputSnapshotEnvelope, OutputStream, SessionCreateParams, SessionId, ToolCallId,
    };
    use cookie_agent_providers::{
        ModelId, NormalizedEvent, Provider, ProviderCapabilities, ProviderError, ProviderRequest,
    };
    use cookie_agent_server::Server;
    use futures_util::{StreamExt, stream};
    use jiff::Timestamp;
    use ratatui::{Terminal, backend::TestBackend};

    use crate::{
        Client,
        state::{OrderedOutput, StateStore},
        ui::App,
    };

    struct ScriptedFakeProvider;

    #[async_trait]
    impl Provider for ScriptedFakeProvider {
        fn capabilities(&self, _: &ModelId) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn stream(
            &self,
            _: ProviderRequest,
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
            ProviderError,
        > {
            Ok(stream::pending().boxed())
        }
    }

    fn in_process_server() -> (tempfile::TempDir, Arc<Server>) {
        let directory = tempfile::tempdir().expect("temporary data directory");
        let mut config = Config::default();
        config.providers.insert(
            "fake".into(),
            ProviderConfig {
                kind: ProviderType::OpenAi,
                api_key_env: None,
                base_url: None,
                api: None,
            },
        );
        config.agents = BTreeMap::from([(
            "primary".into(),
            AgentProfile {
                r#type: cookie_agent_config::AgentType::Primary,
                models: vec![ModelConfig {
                    provider: "fake".into(),
                    model: "scripted".into(),
                }],
                ..AgentProfile::default()
            },
        )]);
        let provider: Arc<dyn Provider> = Arc::new(ScriptedFakeProvider);
        let providers = HashMap::from([("fake".into(), provider)]);
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config: config.clone(),
            providers: providers.clone(),
            tools: Vec::new(),
        })
        .expect("open engine");
        (directory, Arc::new(Server::new(engine, config, providers)))
    }

    fn call_id() -> ToolCallId {
        ToolCallId::new_v7()
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
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::ToolCallStarted {
                tool_call_id: call,
                tool: "bash".into(),
                arguments: serde_json::json!({}),
                provider_tool_call_id: None,
                provider_protocol: None,
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
        let trace = DecisionTrace {
            action: ActionKind::Bash,
            normalized_resource: "git status".into(),
            candidates: Vec::new(),
            effect: Effect::Ask,
            precedence_reason: "test".into(),
        };
        store.apply_event(EventEnvelope {
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::ApprovalRequested {
                approval_id: "approval".into(),
                action: ActionKind::Bash,
                resource: "git status".into(),
                suggested_pattern: "git status *".into(),
                resources: vec![ApprovalResource {
                    action: ActionKind::Bash,
                    resource: "git status".into(),
                    suggested_pattern: "git status *".into(),
                }],
                decision_trace: trace,
            },
        });
        store.apply_event(EventEnvelope {
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
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::ToolCallStarted {
                tool_call_id: call,
                tool: "bash".into(),
                arguments: serde_json::json!({}),
                provider_tool_call_id: None,
                provider_protocol: None,
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
        let session = client
            .create_session(SessionCreateParams {
                cwd: directory.path().display().to_string(),
                profile: "primary".into(),
            })
            .await
            .expect("session")
            .session;
        let mut app = App::new(client).await;
        app.store.apply_event(EventEnvelope {
            session_id: session.id,
            run_id: None,
            seq: 2,
            timestamp: Timestamp::now(),
            event: Event::TextDelta {
                text: "streamed assistant text".into(),
            },
        });
        app.store.apply_event(EventEnvelope {
            session_id: session.id,
            run_id: None,
            seq: 3,
            timestamp: Timestamp::now(),
            event: Event::ApprovalRequested {
                approval_id: "approval".into(),
                action: ActionKind::Bash,
                resource: "git status".into(),
                suggested_pattern: "git status *".into(),
                resources: vec![ApprovalResource {
                    action: ActionKind::Bash,
                    resource: "git status".into(),
                    suggested_pattern: "git status *".into(),
                }],
                decision_trace: DecisionTrace {
                    action: ActionKind::Bash,
                    normalized_resource: "git status".into(),
                    candidates: vec![MatchedPermissionRule {
                        rule_id: None,
                        source_layer: "test".into(),
                        effect: Effect::Ask,
                        hard: false,
                    }],
                    effect: Effect::Ask,
                    precedence_reason: "test".into(),
                },
            },
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("app render");
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect::<String>();
        assert!(rendered.contains("Conversation"));
        assert!(rendered.contains("Approval"));
        assert!(rendered.contains("streamed assistant text"));
        assert!(rendered.contains("git status"));
        assert!(rendered.contains("Session tree"));
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

    #[test]
    fn state_store_reduces_events_and_surfaces_gap_cursor() {
        let session = SessionId::new_v7();
        let mut store = StateStore::default();
        store.apply_event(EventEnvelope {
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
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::TextDelta { text: "old".into() },
        });
        store.apply_event(EventEnvelope {
            session_id: session,
            run_id: None,
            seq: 2,
            timestamp: Timestamp::now(),
            event: Event::ToolCallStarted {
                tool_call_id: call,
                tool: "bash".into(),
                arguments: serde_json::json!({}),
                provider_tool_call_id: None,
                provider_protocol: None,
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
                    session_id: session,
                    run_id: None,
                    seq: 2,
                    timestamp: Timestamp::now(),
                    event: Event::ToolCallStarted {
                        tool_call_id: call,
                        tool: "bash".into(),
                        arguments: serde_json::json!({}),
                        provider_tool_call_id: None,
                        provider_protocol: None,
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
                session_id: session,
                run_id: None,
                seq: 1,
                timestamp: Timestamp::now(),
                event: Event::ToolCallStarted {
                    tool_call_id: call,
                    tool: "bash".into(),
                    arguments: serde_json::json!({}),
                    provider_tool_call_id: None,
                    provider_protocol: None,
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
            session_id: session,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::ToolCallStarted {
                tool_call_id: call,
                tool: "bash".into(),
                arguments: serde_json::json!({}),
                provider_tool_call_id: None,
                provider_protocol: None,
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
                session_id: session,
                run_id: None,
                seq: 2,
                timestamp: Timestamp::now(),
                event: Event::ToolCallStarted {
                    tool_call_id: call,
                    tool: "bash".into(),
                    arguments: serde_json::json!({}),
                    provider_tool_call_id: None,
                    provider_protocol: None,
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
                session_id: session,
                run_id: None,
                seq: 1,
                timestamp: Timestamp::now(),
                event: Event::ToolCallStarted {
                    tool_call_id: call,
                    tool: "bash".into(),
                    arguments: serde_json::json!({}),
                    provider_tool_call_id: None,
                    provider_protocol: None,
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
