use std::sync::Arc;

use cookie_agent_protocol::{
    AgentId, ClientRunId, EventPayload, InternalAgentKind, ModelSelection, PermissionAction,
    PermissionEffect, PermissionRule, RunSelection, RunStartParams, SessionPermissionOverlay,
    SessionStatus, WildcardPattern,
};

use crate::{EngineError, EngineHistoryView, ToolProvider};

use super::support::*;

#[test]
fn workspace_internal_agent_replaces_builtin_document_and_limits() {
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        "http://127.0.0.1:9/v1",
        "---\ndescription: Primary test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nPrimary.\n",
        Some((
            "approval.md",
            "---\ndescription: Workspace approval\nmode: internal\nenabled: true\nmodels: [{ model: \"${parent_model}\" }]\nlimits: { timeout_ms: 1234, max_output_tokens: 345 }\npermissions: {}\n---\nWorkspace approval prompt.\n",
        )),
        None,
        false,
    );
    let owner = frozen_root_policy(&fixture, &selection);
    let policy = fixture
        .engine
        .internal_agent_policy(
            InternalAgentKind::Approval,
            &owner,
            owner.selected_suffix.first(),
        )
        .expect("workspace approval policy");

    assert_eq!(
        policy.agent.document_source,
        cookie_agent_protocol::AgentDocumentSource::Workspace
    );
    assert_eq!(policy.agent.composed_prompt, "Workspace approval prompt.\n");
    assert_eq!(policy.limits.timeout_ms, 1234);
    assert_eq!(policy.limits.max_output_tokens, 345);
    assert!(policy.agent.permissions.is_empty());
}

#[test]
fn available_models_synthesize_default_agent_and_admit_sessions() {
    let fixture = synthetic_default_fixture(None);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestToolDefinitionProvider));
    let snapshot = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    assert_eq!(snapshot.models.len(), 2);
    assert_eq!(snapshot.agents.len(), 4);
    let agent = snapshot
        .agents
        .iter()
        .find(|agent| agent.id.as_str() == "default")
        .expect("default agent");
    assert_eq!(agent.id.as_str(), "default");
    assert!(agent.runnable_as_root);
    assert_eq!(agent.resolved_fallback.len(), 1);
    assert_eq!(
        agent.resolved_fallback[0].model.to_string(),
        "custom.test/a-model"
    );
    assert_eq!(
        agent.resolved_fallback[0]
            .variant
            .as_ref()
            .map(|variant| variant.as_str()),
        Some("precise")
    );
    let selection = RunSelection {
        agent: agent.id.clone(),
        model: agent.resolved_fallback[0].clone(),
        preset: None,
    };
    let policy = frozen_root_policy(&fixture, &selection);
    let session = fixture
        .engine
        .create_session(selection)
        .expect("synthetic-agent session");
    let frozen = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("stored session")
        .creation_agent;
    assert_eq!(
        frozen.document_source,
        cookie_agent_protocol::AgentDocumentSource::BuiltIn
    );
    assert!(frozen.delegation.is_none());
    let tool_names = fixture
        .engine
        .tool_definitions(session.session_id, &policy)
        .expect("built-in default tool definitions")
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert_eq!(tool_names, ["bash", "edit", "read", "write"]);
    assert!(frozen.permissions.iter().any(|rule| {
        rule.action == PermissionAction::Read
            && rule.resource.as_str() == "store-v3.json"
            && rule.effect == cookie_agent_protocol::PermissionEffect::Deny
    }));
    assert!(frozen.permissions.iter().any(|rule| {
        rule.action == PermissionAction::Write
            && rule.resource.as_str() == "*"
            && rule.effect == cookie_agent_protocol::PermissionEffect::Ask
    }));
    for (action, resource, expected) in [
        (
            PermissionAction::Read,
            ".env",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            "nested/.env.local",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            ".env.example",
            cookie_agent_protocol::PermissionEffect::Allow,
        ),
        (
            PermissionAction::Read,
            "nested/.env.example",
            cookie_agent_protocol::PermissionEffect::Allow,
        ),
        (
            PermissionAction::Read,
            "store-v3.json",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            "nested/store-v3.json",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            "id_ed25519",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            ".netrc",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            "application_default_credentials.json",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            "src/lib.rs",
            cookie_agent_protocol::PermissionEffect::Allow,
        ),
        (
            PermissionAction::Write,
            "src/lib.rs",
            cookie_agent_protocol::PermissionEffect::Ask,
        ),
        (
            PermissionAction::Bash,
            "cargo test",
            cookie_agent_protocol::PermissionEffect::Ask,
        ),
        (
            PermissionAction::Delegate,
            "worker",
            cookie_agent_protocol::PermissionEffect::Ask,
        ),
    ] {
        assert_eq!(
            crate::permissions::effective_permission(
                &frozen,
                action,
                resource,
                fixture.engine.inner.store.cwd(),
            )
            .0,
            expected,
            "{action:?} {resource}"
        );
    }
}

#[test]
fn tool_definitions_enforce_sparse_permissions_and_delegate_structure() {
    let sparse_agent = "---\ndescription: Sparse tool test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/a-model\", variant: null }]\npermissions:\n  read: allow\n---\nTest sparse tool visibility.\n";
    let mut fixture = synthetic_default_fixture(Some(sparse_agent));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestToolDefinitionProvider));
    let snapshot = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    let agent = snapshot
        .agents
        .iter()
        .find(|agent| agent.id.as_str() == "primary")
        .expect("sparse primary agent");
    let selection = RunSelection {
        agent: agent.id.clone(),
        model: agent.resolved_fallback[0].clone(),
        preset: None,
    };
    let mut policy = frozen_root_policy(&fixture, &selection);
    let session = fixture
        .engine
        .create_session(selection)
        .expect("sparse-agent session");

    let definitions = fixture
        .engine
        .tool_definitions(session.session_id, &policy)
        .expect("structurally gated sparse tool definitions");
    assert_eq!(
        definitions
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read"]
    );
    assert!(
        !serde_json::to_string(&definitions)
            .expect("serialize provider tool definitions")
            .contains("result_truncation")
    );

    let worker = "---\ndescription: Worker tool target\nmode: subagent\nenabled: true\nmodels: []\npermissions: {}\n---\nTest worker.\n";
    fixture = synthetic_default_fixture(Some(worker));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestToolDefinitionProvider));
    let snapshot = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    let default = snapshot
        .agents
        .iter()
        .find(|agent| agent.id.as_str() == "default")
        .expect("built-in default agent");
    let selection = RunSelection {
        agent: default.id.clone(),
        model: default.resolved_fallback[0].clone(),
        preset: None,
    };
    policy = frozen_root_policy(&fixture, &selection);
    // The default document has delegate permission but no named target; supply
    // valid frozen target metadata to exercise the structural gate's open path.
    policy.agent.delegation = Some(cookie_agent_protocol::FrozenDelegationPolicy {
        targets: vec![AgentId::new("primary").expect("worker agent ID")],
        effective_depth_ceiling: 3,
    });
    let session = fixture
        .engine
        .create_session(selection)
        .expect("default-agent session");
    assert_eq!(
        fixture
            .engine
            .tool_definitions(session.session_id, &policy)
            .expect("delegate-enabled default tool definitions")
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>(),
        ["bash", "delegate", "edit", "read", "write"]
    );
}

#[test]
fn published_tool_order_is_stable_across_registration_and_overlay_order() {
    fn definitions(reversed: bool) -> Vec<oven_sdk::ToolDefinition> {
        let agent = "---\ndescription: Tool ordering agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/a-model\", variant: null }]\npermissions: {}\n---\nTest stable tool ordering.\n";
        let fixture = synthetic_default_fixture(Some(agent));
        let providers: Vec<Arc<dyn ToolProvider>> = if reversed {
            vec![
                Arc::new(OrderedToolDefinitionProvider {
                    id: "test.ordered_definition.middle",
                    tools: vec![("middle", "bash")],
                }),
                Arc::new(OrderedToolDefinitionProvider {
                    id: "test.ordered_definition.edges",
                    tools: vec![("zeta", "read"), ("alpha", "write")],
                }),
            ]
        } else {
            vec![
                Arc::new(OrderedToolDefinitionProvider {
                    id: "test.ordered_definition.edges",
                    tools: vec![("alpha", "write"), ("zeta", "read")],
                }),
                Arc::new(OrderedToolDefinitionProvider {
                    id: "test.ordered_definition.middle",
                    tools: vec![("middle", "bash")],
                }),
            ]
        };
        for provider in providers {
            fixture.engine.register_tool_provider(provider);
        }
        let snapshot = fixture.engine.runtime_snapshot().unwrap().snapshot;
        let selected = snapshot
            .agents
            .iter()
            .find(|agent| agent.id.as_str() == "primary")
            .unwrap();
        let selection = RunSelection {
            agent: selected.id.clone(),
            model: selected.resolved_fallback[0].clone(),
            preset: None,
        };
        let policy = frozen_root_policy(&fixture, &selection);
        let session = fixture.engine.create_session(selection).unwrap();
        let mut rules = vec![
            PermissionRule {
                action: PermissionAction::Read,
                resource: WildcardPattern::new("*").unwrap(),
                effect: PermissionEffect::Allow,
            },
            PermissionRule {
                action: PermissionAction::Write,
                resource: WildcardPattern::new("*").unwrap(),
                effect: PermissionEffect::Allow,
            },
            PermissionRule {
                action: PermissionAction::Bash,
                resource: WildcardPattern::new("*").unwrap(),
                effect: PermissionEffect::Allow,
            },
        ];
        if reversed {
            rules.reverse();
        }
        fixture
            .engine
            .append_blocking(
                session.session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionPermissionOverlaySet {
                    overlay: SessionPermissionOverlay { rules },
                },
            )
            .unwrap();
        fixture
            .engine
            .tool_definitions(session.session_id, &policy)
            .unwrap()
    }

    let forward = definitions(false);
    let reversed = definitions(true);
    assert_eq!(
        forward
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "middle", "zeta"]
    );
    assert_eq!(
        serde_json::to_value(forward).unwrap(),
        serde_json::to_value(reversed).unwrap()
    );
}

#[test]
fn runtime_snapshot_model_descriptor_preserves_compiled_variant_order() {
    let fixture = synthetic_default_fixture(None);
    let snapshot = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    let descriptor = snapshot
        .models
        .iter()
        .find(|model| model.key.to_string() == "custom.test/a-model")
        .expect("runtime model descriptor");
    let runtime = fixture.manager.current();
    let compiled = runtime
        .models()
        .get(&descriptor.key)
        .expect("compiled runtime model");

    assert_eq!(descriptor.variant_order, compiled.model.variant_order);
}

#[test]
fn synthetic_default_replaces_no_authored_agent_and_unknown_models_are_diagnostic() {
    let fixture = synthetic_default_fixture(Some(
        "---\ndescription: Unknown primary\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/missing\", variant: base }]\npermissions: {}\n---\nUnknown prompt.\n",
    ));
    let error = fixture
        .engine
        .create_session(RunSelection {
            agent: AgentId::new("primary").expect("primary agent ID"),
            model: ModelSelection {
                model: "custom.test/missing".parse().expect("unknown model"),
                variant: None,
            },
            preset: None,
        })
        .expect_err("unknown model should reject explicit session selection");
    assert!(matches!(error, EngineError::UnknownAgentModel { .. }));
    drop(fixture);

    let (runnable, _) = custom_fixture();
    let snapshot = runnable
        .engine
        .runtime_snapshot()
        .expect("runtime")
        .snapshot;
    assert!(
        snapshot
            .agents
            .iter()
            .any(|agent| agent.id.as_str() == "primary")
    );
    assert!(
        !snapshot
            .agents
            .iter()
            .any(|agent| agent.id.as_str() == "default")
    );
}

#[tokio::test]
async fn agent_presets_materialize_effective_registries_and_persist_selection() {
    let (mut fixture, shared_selection) = custom_fixture();
    fixture.engine.shutdown().await;

    let primary_id = AgentId::new("primary").expect("primary agent ID");
    let mut python_agents = fixture.config.agents.clone();
    let mut python_primary = python_agents[&primary_id].clone();
    python_primary.frontmatter.description = "Python preset primary".into();
    python_primary.body = "Use Python for this task.\n".into();
    python_agents.insert(primary_id.clone(), python_primary);
    let reviewer_id = AgentId::new("reviewer").expect("reviewer agent ID");
    let mut reviewer = fixture.config.agents[&primary_id].clone();
    reviewer.id = reviewer_id.clone();
    reviewer.frontmatter.description = "Python-only reviewer".into();
    reviewer.body = "Review Python code.\n".into();
    python_agents.insert(reviewer_id.clone(), reviewer);
    fixture
        .config
        .agent_presets
        .insert("python".into(), python_agents);

    let mut no_root_agents = fixture.config.agents.clone();
    no_root_agents
        .get_mut(&primary_id)
        .expect("primary agent")
        .frontmatter
        .enabled = false;
    fixture
        .config
        .agent_presets
        .insert("no-root".into(), no_root_agents);
    fixture.engine = reopen_engine(&fixture);

    let snapshot = &fixture.engine.current_runtime().result.snapshot;
    let shared_primary = snapshot
        .agents
        .iter()
        .find(|agent| agent.preset.is_none() && agent.id == primary_id)
        .expect("shared primary descriptor");
    assert_eq!(shared_primary.description, "Primary test agent");
    let python_primary = snapshot
        .agents
        .iter()
        .find(|agent| agent.preset.as_deref() == Some("python") && agent.id == primary_id)
        .expect("Python primary descriptor");
    assert_eq!(python_primary.description, "Python preset primary");
    assert!(
        snapshot
            .agents
            .iter()
            .any(|agent| { agent.preset.as_deref() == Some("python") && agent.id == reviewer_id })
    );
    assert!(snapshot.agents.iter().any(|agent| {
        agent.preset.as_deref() == Some("no-root") && agent.id.as_str() == "default"
    }));
    assert!(snapshot.agents.iter().any(|agent| {
        agent.preset.as_deref() == Some("python") && agent.id.as_str() == "approval"
    }));

    let preset_selection = RunSelection {
        agent: reviewer_id,
        model: shared_selection.model.clone(),
        preset: Some("python".into()),
    };
    let created = fixture
        .engine
        .create_session(preset_selection.clone())
        .expect("preset session");
    assert_eq!(created.creation_selection, preset_selection);
    assert_eq!(
        fixture
            .engine
            .inner
            .store
            .get(created.session_id)
            .expect("preset projection")
            .creation_agent
            .description,
        "Python-only reviewer"
    );
    let unavailable = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: created.session_id,
                client_run_id: ClientRunId::new("unavailable-preset-agent").expect("run ID"),
                selection: RunSelection {
                    preset: Some("no-root".into()),
                    ..preset_selection.clone()
                },
                input: "agent is absent in this preset".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect_err("missing preset agent is rejected");
    assert!(matches!(unavailable, EngineError::InvalidRuntimeAgent(_)));
    assert!(matches!(
        fixture.engine.create_session(RunSelection {
            preset: Some("missing".into()),
            ..shared_selection
        }),
        Err(EngineError::UnknownAgentPreset(name)) if name == "missing"
    ));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn root_run_preset_switch_freezes_replay_and_delegation_inheritance() {
    let (endpoint, server) = scripted_preset_switch_delegation_server().await;
    let (mut fixture, shared_selection) = custom_fixture_with_endpoint(&endpoint);
    fixture.engine.shutdown().await;
    let primary_id = AgentId::new("primary").expect("primary ID");
    let worker_id = AgentId::new("worker").expect("worker ID");
    let mut python_agents = fixture.config.agents.clone();
    python_agents
        .get_mut(&primary_id)
        .expect("preset primary")
        .frontmatter
        .description = "Python preset primary".into();
    python_agents
        .get_mut(&worker_id)
        .expect("preset worker")
        .frontmatter
        .description = "Python preset worker".into();
    let mut compaction = fixture.config.agents[&primary_id].clone();
    compaction.id = AgentId::new("compaction").expect("compaction ID");
    compaction.frontmatter.description = "Python preset compaction".into();
    compaction.frontmatter.mode = cookie_agent_config::AgentMode::Internal;
    compaction.frontmatter.models = vec![cookie_agent_config::AgentModelFallback {
        model: cookie_agent_config::AgentModelRef::ParentModel,
        variant: None,
        cache: None,
    }];
    compaction.frontmatter.limits = cookie_agent_config::AgentLimits {
        timeout_ms: 30_000,
        max_output_tokens: 2_048,
    };
    compaction.body = "Python preset compaction prompt.\n".into();
    python_agents.insert(compaction.id.clone(), compaction);
    fixture
        .config
        .agent_presets
        .insert("python".into(), python_agents);
    fixture.engine = reopen_engine(&fixture);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));

    let parent = fixture
        .engine
        .create_session(shared_selection.clone())
        .expect("shared parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("shared-before-preset").expect("run ID"),
                selection: shared_selection.clone(),
                input: "complete the shared run".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("shared run");
    wait_for_session_not_running(&fixture.engine, parent.session_id).await;

    let preset_selection = RunSelection {
        preset: Some("python".into()),
        ..shared_selection
    };
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("preset-delegation-run").expect("run ID"),
                selection: preset_selection,
                input: "delegate after switching presets".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("preset run");
    let child = await_child(
        &fixture.engine,
        parent.session_id,
        "preset child completion",
        |child| child.status == SessionStatus::Completed,
    )
    .await;
    wait_for_session_not_running(&fixture.engine, parent.session_id).await;

    let parent_projection = fixture
        .engine
        .inner
        .store
        .get(parent.session_id)
        .expect("parent projection");
    let shared_run = parent_projection
        .runs
        .values()
        .find(|run| run.client_run_id.as_str() == "shared-before-preset")
        .expect("shared run projection");
    let preset_run = parent_projection
        .runs
        .values()
        .find(|run| run.client_run_id.as_str() == "preset-delegation-run")
        .expect("preset run projection");
    assert_eq!(shared_run.selection.preset, None);
    assert_eq!(shared_run.agent.description, "Primary test agent");
    assert_eq!(preset_run.selection.preset.as_deref(), Some("python"));
    assert_eq!(preset_run.agent.description, "Python preset primary");
    let mut legacy_events = parent_projection.log.events();
    for event in &mut legacy_events {
        if event.run_id == Some(preset_run.id)
            && let EventPayload::RunStarted {
                internal_agents, ..
            } = &mut event.payload
        {
            internal_agents.clear();
        }
    }
    let legacy_policy = fixture
        .engine
        .historical_title_policy(&legacy_events, preset_run.id)
        .expect("legacy preset policy");
    let current_runtime = fixture.engine.current_runtime();
    assert!(legacy_policy.internal_agents.is_empty());
    assert!(!legacy_policy.historical_delegation);
    assert!(Arc::ptr_eq(
        &legacy_policy.registry,
        current_runtime
            .agent_presets
            .get("python")
            .expect("live python preset registry")
    ));

    let child_projection = fixture
        .engine
        .inner
        .store
        .get(child.session_id)
        .expect("preset child projection");
    assert_eq!(
        child_projection.meta.creation_selection.preset.as_deref(),
        Some("python")
    );
    assert_eq!(
        child_projection.creation_agent.description,
        "Python preset worker"
    );
    let mut switched_child = child_projection.meta.creation_selection.clone();
    switched_child.preset = None;
    assert!(matches!(
        fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: child.session_id,
                    client_run_id: ClientRunId::new("delegated-preset-switch").expect("run ID"),
                    selection: switched_child,
                    input: "must remain pinned".into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap()
            )
            .await,
        Err(EngineError::NoRunnableModel)
    ));

    fixture.engine.shutdown().await;
    fixture.config.agent_presets.clear();
    drop(fixture.engine);
    let reopened = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    let replayed = reopened
        .inner
        .store
        .get(parent.session_id)
        .expect("replayed parent");
    let replayed_run = replayed
        .runs
        .values()
        .find(|run| run.client_run_id.as_str() == "preset-delegation-run")
        .expect("replayed preset run");
    assert_eq!(replayed_run.selection.preset.as_deref(), Some("python"));
    assert_eq!(replayed_run.agent.description, "Python preset primary");
    let replayed_events = replayed.log.events();
    let frozen_run_started = replayed_events
        .iter()
        .find(|event| {
            event.run_id == Some(replayed_run.id)
                && matches!(event.payload, EventPayload::RunStarted { .. })
        })
        .expect("frozen preset run start");
    let EventPayload::RunStarted {
        internal_agents, ..
    } = &frozen_run_started.payload
    else {
        unreachable!("matched run start")
    };
    let frozen_compaction = internal_agents
        .iter()
        .find(|definition| definition.kind == InternalAgentKind::ContextCompaction)
        .expect("frozen preset compaction definition");
    assert_eq!(
        frozen_compaction.composed_prompt,
        "Python preset compaction prompt.\n"
    );
    for partial_len in [1, 2] {
        let mut partial = frozen_run_started.clone();
        let EventPayload::RunStarted {
            internal_agents, ..
        } = &mut partial.payload
        else {
            unreachable!("cloned run start")
        };
        internal_agents.truncate(partial_len);
        assert!(partial.validate().is_err());
        let encoded = serde_json::to_value(partial).expect("serialize partial run start");
        assert!(serde_json::from_value::<cookie_agent_protocol::StoredEvent>(encoded).is_err());
    }
    assert!(
        !reopened
            .get_history(parent.session_id, EngineHistoryView::Assembled)
            .await
            .expect("historical assembled history")
            .is_empty()
    );
    assert!(
        reopened
            .compact_session(
                parent.session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap()
            )
            .await
            .expect("historical manual compaction")
    );
    let requests = with_watchdog("server fixture completion", server)
        .await
        .expect("preset switch server");
    assert_eq!(requests.len(), 5);
    assert!(
        requests[4].contains("Python preset compaction prompt"),
        "{}",
        requests[4]
    );
    assert!(
        !requests[4]
            .contains("Summarize conversation context faithfully within the supplied bounds"),
        "{}",
        requests[4]
    );
    reopened.shutdown().await;
}
