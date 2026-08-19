//! Ratatui frontend for cookie agent's versioned JSON-RPC protocol.

pub mod client;
pub mod config;
pub mod markdown;
pub mod state;
pub mod terminal_detect;
pub mod theme;
pub mod ui;

pub use client::{
    Client, ClientDelivery, ClientError, ClientProtocol, read_daemon_token, validate_websocket_url,
};
pub use ui::{run_with_client, run_with_new_session};

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, sync::Arc};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use cookie_agent_config::{
        AgentDocument, AgentDocumentSource, AgentFrontmatter, AgentMode, ApprovalConfig,
        ContextCompactionConfig, LoadedConfiguration, RuntimeConfig, ServerConfig,
        SessionTitleConfig, ToolOutputConfig,
    };
    use cookie_agent_engine::{Engine, EngineOptions};
    use cookie_agent_models::{ModelManager, ProviderDefinition};
    use cookie_agent_models::{
        catalog::{
            CatalogAgeState, CatalogAvailability, CatalogRuntimeState, CatalogSnapshot,
            CatalogSource,
        },
        provider_store::ProviderStore,
    };
    use cookie_agent_protocol::{
        AgentId, ApprovalBoundary, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation,
        ApprovalId, ApprovalRequest, ApprovalResourceSource, ApprovalTrigger, DecisionTrace,
        EventPayload, EventSubscriptionMessage, MatchedPermissionRule, ModelKey, ModelSelection,
        OutputDelta, OutputSnapshot, OutputSnapshotEnvelope, OutputStream, PermissionAction,
        PermissionEffect, PreparedApprovalResource, PreparedBindingLifetime,
        PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
        PreparedResourceIdentity, ProviderId, RunSelection, SafeCode, SessionCreateParams,
        SessionId, Sha256Digest, StoredEvent, ToolCallId,
    };
    use cookie_agent_server::Server;
    use jiff::Timestamp;
    use ratatui::{Terminal, backend::TestBackend};

    use crate::{
        Client,
        state::{OrderedOutput, StateStore},
        ui::App,
    };

    pub(crate) fn test_run_selection() -> RunSelection {
        RunSelection {
            agent: AgentId::new("primary").expect("agent id"),
            model: ModelSelection {
                model: "custom.gateway/arbitrary-model"
                    .parse::<ModelKey>()
                    .expect("model key"),
                variant: None,
            },
        }
    }

    fn test_providers() -> BTreeMap<ProviderId, ProviderDefinition> {
        let definition: ProviderDefinition = serde_json::from_value(serde_json::json!({
            "source": "custom",
            "endpoint": "https://example.test/v1",
            "adaptor": "openai-compatible",
            "auth": {"method": "no-auth-v1", "values": {}},
            "models": {
                "arbitrary-model": {
                    "display_name": "Arbitrary Model",
                    "capabilities": {
                        "input": ["text"],
                        "output": ["text"],
                        "context_tokens": 8192,
                        "output_tokens": 2048,
                        "tool_calling": false,
                        "parallel_tool_calls": false,
                        "structured_output": false,
                        "reasoning": false,
                        "temperature": false,
                        "top_p": false,
                        "seed": false,
                        "native_replay": "unsupported",
                        "cancellation": "local_only",
                        "media": {}
                    }
                }
            }
        }))
        .expect("test provider");
        BTreeMap::from([(
            ProviderId::new("custom.gateway").expect("provider id"),
            definition,
        )])
    }

    fn test_digest(value: &str) -> cookie_agent_models::Sha256Digest {
        // A deterministic 64-hex stand-in; config fingerprints are opaque to
        // the TUI and only carried through to the engine.
        let base = value.to_owned();
        let mut hex = String::with_capacity(64);
        while hex.len() < 64 {
            hex.push_str(
                &base
                    .bytes()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
            );
        }
        hex.truncate(64);
        let hex = hex
            .chars()
            .map(|character| {
                if character.is_ascii_hexdigit() {
                    character
                } else {
                    'a'
                }
            })
            .collect::<String>()
            .to_lowercase();
        cookie_agent_models::Sha256Digest::new(hex).expect("test digest")
    }

    fn test_agent_document(id: &AgentId) -> AgentDocument {
        let body = "You are the primary test agent.
";
        AgentDocument {
            id: id.clone(),
            frontmatter: AgentFrontmatter {
                description: "Test primary agent".into(),
                mode: AgentMode::Primary,
                enabled: true,
                models: vec![cookie_agent_config::AgentModelFallback {
                    model: cookie_agent_config::AgentModelRef::Model(
                        "custom.gateway/arbitrary-model"
                            .parse::<ModelKey>()
                            .expect("model key"),
                    ),
                    variant: None,
                }],
                limits: cookie_agent_config::AgentLimits::default(),
                permissions: Default::default(),
            },
            body: body.to_owned(),
            source: AgentDocumentSource::Workspace,
            document_fingerprint: test_digest("primary document"),
            prompt_fingerprint: test_digest(body),
        }
    }

    fn in_process_server() -> (tempfile::TempDir, Arc<Server>) {
        in_process_server_with_skills(false)
    }

    fn in_process_server_with_skills(with_skills: bool) -> (tempfile::TempDir, Arc<Server>) {
        let directory = tempfile::tempdir().expect("temporary data directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .expect("private test root");
        }
        let providers = test_providers();
        let provider_store = directory.path().join("provider-store");
        fs::create_dir(&provider_store).expect("provider store directory");
        #[cfg(unix)]
        fs::set_permissions(&provider_store, fs::Permissions::from_mode(0o700))
            .expect("private provider store");
        let revision =
            cookie_agent_protocol::CatalogRevision::new(format!("sha256:{}", "0".repeat(64)))
                .expect("catalog revision");
        let now = Timestamp::now();
        let catalog = Arc::new(CatalogSnapshot {
            revision,
            source: CatalogSource::Bootstrap,
            state: CatalogRuntimeState {
                availability: CatalogAvailability::Bootstrap,
                age: CatalogAgeState::Current,
                last_error: None,
            },
            validated_at: now,
            last_checked_at: now,
            etag: None,
            providers: BTreeMap::new(),
            canonical_models: BTreeMap::new(),
            quarantine: Vec::new(),
        });
        let model_manager = Arc::new(
            ModelManager::new(
                providers.clone(),
                catalog,
                ProviderStore::open(&provider_store).expect("provider store"),
            )
            .expect("model manager"),
        );
        let agent_id = AgentId::new("primary").expect("agent id");
        let skills = if with_skills {
            let root = directory.path().join("skills");
            fs::create_dir_all(root.join("fresh-skill")).expect("skill directory");
            fs::write(
                root.join("fresh-skill/SKILL.md"),
                "---\nname: fresh-skill\ndescription: Fresh session skill\nargument-hint: <value>\n---\nFresh $ARGUMENTS.\n",
            )
            .expect("skill document");
            cookie_agent_config::load_skill_roots(None, &[root]).expect("skill registry")
        } else {
            cookie_agent_config::SkillRegistry::default()
        };
        let agent = test_agent_document(&agent_id);
        let config = LoadedConfiguration {
            runtime: RuntimeConfig {
                server: ServerConfig::default(),
                tool_output: ToolOutputConfig::default(),
                approval: ApprovalConfig::default(),
                context_compaction: ContextCompactionConfig::default(),
                prompt_caching: cookie_agent_config::PromptCachingConfig::default(),
                session_title: SessionTitleConfig::default(),
                delegation: cookie_agent_config::DelegationConfig::default(),
                pricing: cookie_agent_config::PricingConfig::default(),
                providers,
            },
            agents: BTreeMap::from([(agent_id.clone(), agent)]),
            mcp_servers: BTreeMap::new(),
            user_mcp_servers: BTreeMap::new(),
            workspace_mcp_servers: BTreeMap::new(),
            plugins: Default::default(),
            config_paths: cookie_agent_config::ConfigLayerPaths::default(),
            skills,
        };
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config,
            model_manager: Arc::clone(&model_manager),
            tools: Vec::new(),
        })
        .expect("open engine");
        if with_skills {
            engine
                .try_register_tool_provider(Arc::new(cookie_agent_tools::skill::SkillTool::new(
                    engine.clone(),
                )))
                .expect("skill tool");
        }
        (directory, Arc::new(Server::new(engine)))
    }

    fn call_id() -> ToolCallId {
        ToolCallId::new_v7()
    }

    fn approval_request() -> ApprovalRequest {
        let resource = PreparedApprovalResource {
            capability: PermissionAction::Bash,
            canonical: PreparedResourceIdentity::new("command:git-status")
                .expect("prepared resource identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"git status"),
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: ApprovalBoundary::CommandPrefix {
                prefix: "git status".into(),
            },
            source: ApprovalResourceSource::ModelRequest,
        };
        let resource_digest = resource.binding_digest.clone();
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"normalized arguments"),
            vec![ApprovalCapability {
                action: PermissionAction::Bash,
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
                effect: PermissionEffect::Ask,
                trace: DecisionTrace {
                    action: PermissionAction::Bash,
                    normalized_resource: "git status".into(),
                    candidates: vec![MatchedPermissionRule {
                        source_layer: SafeCode::new("test").expect("safe code"),
                        action: PermissionAction::Bash,
                        resource: cookie_agent_protocol::WildcardPattern::new("git status")
                            .expect("wildcard"),
                        effect: PermissionEffect::Ask,
                    }],
                    effect: PermissionEffect::Ask,
                    precedence_reason: "test".into(),
                },
            }],
            ApprovalConstraints {
                allow_once: true,
                allow_tree_grant: false,
                cancellable: true,
                expires_at: None,
            },
        )
        .expect("approval request")
    }

    fn text_event(
        session_id: SessionId,
        seq: u64,
        run_id: cookie_agent_protocol::RunId,
        attempt_id: cookie_agent_protocol::AttemptId,
        text: &str,
    ) -> StoredEvent {
        StoredEvent {
            engine_version: None,
            session_id,
            run_id: Some(run_id),
            seq,
            timestamp: Timestamp::now(),
            payload: EventPayload::TextDelta {
                attempt_id,
                text: text.into(),
            },
        }
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
    fn terminal_run_expires_pending_approvals() {
        let session = SessionId::new_v7();
        let run_id = cookie_agent_protocol::RunId::new_v7();
        let mut store = StateStore::default();
        store.apply_event(StoredEvent {
            engine_version: None,
            session_id: session,
            run_id: Some(run_id),
            seq: 1,
            timestamp: Timestamp::now(),
            payload: EventPayload::ApprovalRequested {
                request: approval_request(),
            },
        });
        store.apply_event(StoredEvent {
            engine_version: None,
            session_id: session,
            run_id: Some(run_id),
            seq: 2,
            timestamp: Timestamp::now(),
            payload: EventPayload::RunCompleted { final_text: None },
        });
        assert!(store.sessions[&session].approvals.is_empty());
    }

    #[tokio::test]
    async fn app_draws_conversation_approval_tree_and_input() {
        let (_directory, server) = in_process_server();
        let client = Client::connect_in_process(server);
        client.handshake().await.expect("handshake");
        let models = client.runtime_snapshot().await.expect("runtime snapshot");
        assert_eq!(
            models.snapshot.models[0].key.as_str(),
            "custom.gateway/arbitrary-model"
        );
        let session = client
            .create_session(SessionCreateParams {
                selection: test_run_selection(),
            })
            .await
            .expect("session")
            .session;
        let mut app = App::new(client).await.expect("TUI config");
        let request = approval_request();
        let approval_id = request.approval_id();
        app.store.apply_event(StoredEvent {
            engine_version: None,
            session_id: session.session_id,
            run_id: Some(cookie_agent_protocol::RunId::new_v7()),
            seq: 2,
            timestamp: Timestamp::now(),
            payload: EventPayload::ApprovalRequested { request },
        });
        app.store.apply_event(StoredEvent {
            engine_version: None,
            session_id: session.session_id,
            run_id: Some(cookie_agent_protocol::RunId::new_v7()),
            seq: 3,
            timestamp: Timestamp::now(),
            payload: EventPayload::ApprovalEscalated {
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
                assert!(rendered.contains("git status"));
                assert!(rendered.contains("primary • custom.gateway/arbitrary-model[base]"));
            }
        }
    }

    #[tokio::test]
    async fn client_round_trips_handshake_create_and_event_replay_in_process() {
        let (_directory, server) = in_process_server();
        let client = Client::connect_in_process(server);
        client.handshake().await.expect("handshake");
        let session = client
            .create_session(SessionCreateParams {
                selection: test_run_selection(),
            })
            .await
            .expect("create session")
            .session;
        let mut deliveries = client.subscribe_deliveries().expect("delivery receiver");
        client
            .subscribe_events(session.session_id, None)
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
        assert!(
            replayed
                .iter()
                .all(|event| event.session_id == session.session_id)
        );
    }

    #[tokio::test]
    async fn fresh_session_selection_discovers_skill_slash_commands() {
        let (_directory, server) = in_process_server_with_skills(true);
        let setup = Client::connect_in_process(server.clone());
        setup.handshake().await.expect("setup handshake");
        let session = setup
            .create_session(SessionCreateParams {
                selection: test_run_selection(),
            })
            .await
            .expect("session")
            .session;
        setup
            .set_permission_mode(cookie_agent_protocol::SessionSetPermissionModeParams {
                session_id: session.session_id,
                mode: cookie_agent_protocol::PermissionMode::Ask,
            })
            .await
            .expect("ask mode");

        let observer = Client::connect_in_process(server.clone());
        observer.handshake().await.expect("observer handshake");
        let mut deliveries = observer
            .subscribe_deliveries()
            .expect("observer deliveries");
        observer
            .subscribe_events(session.session_id, Some(1))
            .await
            .expect("observe session");

        let client = Client::connect_in_process(server);
        client.handshake().await.expect("handshake");
        let mut app = App::new(client).await.expect("app");
        app.wait_for_skill_refresh_for_test().await;
        assert_eq!(app.skill_names_for_test(), ["fresh-skill"]);
        assert_eq!(app.skill_visible_for_test("fresh-skill"), Some(false));
        assert!(
            app.skill_palette_labels_for_test()
                .iter()
                .any(|label| label.contains("/fresh-skill <value>"))
        );
        app.submit_text_for_test("/fresh-skill value").await;
        let loaded = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut run_id = None;
            let mut approval = None;
            loop {
                let event = match deliveries.recv().await.expect("delivery") {
                    crate::ClientDelivery::Live { message, .. } => match message.as_ref() {
                        cookie_agent_protocol::EventSubscriptionMessage::Event { event } => {
                            Some(event.clone())
                        }
                        _ => None,
                    },
                    crate::ClientDelivery::ReplayEvent { event, .. } => Some(event),
                    _ => None,
                };
                let Some(event) = event else { continue };
                if matches!(event.payload, EventPayload::RunStarted { .. }) {
                    run_id = event.run_id;
                }
                if let EventPayload::ApprovalRequested { request } = &event.payload {
                    approval = Some((event.session_id, request.clone()));
                }
                if matches!(event.payload, EventPayload::ApprovalEscalated { .. }) {
                    let (session_id, request) = approval.take().expect("approval request");
                    let request_revision = serde_json::to_value(&request)
                        .expect("approval JSON")["revision"]
                        .as_u64()
                        .expect("approval revision");
                    observer
                        .respond_approval(cookie_agent_protocol::ApprovalRespondParams {
                            session_id,
                            approval_id: request.approval_id(),
                            request_revision,
                            operation_fingerprint: request.operation_fingerprint().clone(),
                            client_response_id: cookie_agent_protocol::ClientResponseId::new(
                                uuid::Uuid::now_v7().to_string(),
                            )
                            .expect("response ID"),
                            decision: cookie_agent_protocol::ApprovalUserDecision::ApproveOnce,
                            feedback: None,
                        })
                        .await
                        .expect("approve direct skill");
                }
                if matches!(event.payload, EventPayload::SkillLoaded { ref name, .. } if name == "fresh-skill") {
                    break run_id;
                }
            }
        })
        .await;
        let run_id = loaded
            .expect("fresh skill command timed out")
            .expect("skill run ID");
        observer
            .cancel_run(cookie_agent_protocol::RunCancelParams { run_id })
            .await
            .expect("cancel test run");
    }

    #[tokio::test]
    async fn local_startup_creates_a_fresh_root_while_attach_keeps_existing_selection() {
        let (_directory, server) = in_process_server();
        let setup = Client::connect_in_process(server.clone());
        setup.handshake().await.expect("setup handshake");
        let existing = setup
            .create_session(SessionCreateParams {
                selection: test_run_selection(),
            })
            .await
            .expect("existing session")
            .session;

        let attached = Client::connect_in_process(server.clone());
        attached.handshake().await.expect("attach handshake");
        let attached_app = App::new(attached).await.expect("attached TUI config");
        assert_eq!(attached_app.store.sessions.len(), 1);
        assert!(
            attached_app
                .store
                .sessions
                .contains_key(&existing.session_id)
        );

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
        assert_ne!(local_session, existing.session_id);
    }

    #[test]
    fn state_store_reduces_events_and_surfaces_gap_cursor() {
        let session = SessionId::new_v7();
        let run_id = cookie_agent_protocol::RunId::new_v7();
        let attempt_id = cookie_agent_protocol::AttemptId::new_v7();
        let mut store = StateStore::default();
        store.apply_event(text_event(session, 1, run_id, attempt_id, "hello"));
        // Replayed duplicate is ignored, preserving the projection.
        store.apply_event(text_event(session, 1, run_id, attempt_id, "duplicate"));
        assert_eq!(store.sessions[&session].last_seq, 1);
        assert_eq!(
            store.apply_subscription(EventSubscriptionMessage::Gap {
                session_id: session,
                last_delivered_seq: 1,
            }),
            Some(1)
        );
    }
}
