use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _, sync::Arc};

use cookie_agent_config::load_from_roots;
use cookie_agent_models::{
    Catalog, CredentialConnectRequest, CredentialStore, MODELS_DEV_ARTIFACT_SHA256, ModelSetManager,
};
use cookie_agent_protocol::{
    AgentId, AttemptId, ClientRunId, EventPayload as Event, EventSchemaVersion, InvocationId,
    ModelFinishReason, ModelSelection, PersistedAssistantPart, PersistedModelTurn, ProviderId,
    RunId, RunSelection, RunStartParams, SessionId, SessionOrigin, SessionStatus, StoredEvent,
    ToolCallId, Usage,
};
use tempfile::TempDir;

use crate::{
    Engine, EngineError, EngineOptions, cwd_identity, freeze_delegated_child_policy,
    model_history::wire_model,
    policy::{ResultLimits, freeze_agent_policy, resolve_agent},
    protocol_digest, session_meta, title_regeneration_target,
};

struct Fixture {
    _directory: TempDir,
    engine: Engine,
    manager: Arc<ModelSetManager>,
}

fn config() -> String {
    format!(
        r#"schema_version = 6
[providers.test]
source = "explicit"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = {{ type = "none" }}

[providers.test.models.alpha]
display_name = "Alpha"
[providers.test.models.alpha.capabilities]
input = ["text"]
output = ["text"]
context_tokens = 8192
output_tokens = 2048
tool_calling = true
parallel_tool_calls = false
structured_output = false
reasoning = false
temperature = true
top_p = true
seed = true
native_replay = "unsupported"
native_compaction = "unsupported"
cancellation = "local_only"
media = {{}}

[providers.test.models.beta]
display_name = "Beta"
[providers.test.models.beta.capabilities]
input = ["text"]
output = ["text"]
context_tokens = 8192
output_tokens = 2048
tool_calling = true
parallel_tool_calls = false
structured_output = false
reasoning = false
temperature = true
top_p = true
seed = true
native_replay = "unsupported"
native_compaction = "unsupported"
cancellation = "local_only"
media = {{}}

[providers.openai]
source = "models_dev"
catalog_revision = "sha256:{MODELS_DEV_ARTIFACT_SHA256}"
auth = {{ type = "credential_store" }}
[providers.openai.models."gpt-5.6-sol"]
"#
    )
}

fn write_agent(
    root: &std::path::Path,
    id: &str,
    mode: &str,
    enabled: bool,
    fallback: &str,
    delegation: &str,
) {
    fs::write(
        root.join("agents").join(format!("{id}.md")),
        format!(
            "---\nschema: 1\ndescription: {id} test agent\nmode: {mode}\nenabled: {enabled}\nmodel_fallback: {fallback}\ntools: []\npermissions: []\n{delegation}---\n{id} prompt.\n"
        ),
    )
    .expect("write agent");
}

fn fixture() -> Fixture {
    let directory = TempDir::new().expect("tempdir");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private tempdir");
    let root = directory.path().join(".cookie-agent");
    fs::create_dir_all(root.join("agents")).expect("agent directory");
    fs::write(root.join("config.toml"), config()).expect("config");
    write_agent(
        &root,
        "alpha",
        "primary",
        true,
        "[{ model: \"test/alpha\" }]",
        "delegation:\n  agents: [inheritor]\n  max_depth: 2\n",
    );
    write_agent(&root, "beta", "all", true, "[{ model: \"test/beta\" }]", "");
    write_agent(
        &root,
        "disabled",
        "primary",
        false,
        "[{ model: \"test/alpha\" }]",
        "",
    );
    write_agent(
        &root,
        "unavailable",
        "primary",
        true,
        "[{ model: \"openai/gpt-5.6-sol\" }]",
        "",
    );
    write_agent(
        &root,
        "child",
        "subagent",
        true,
        "[{ model: \"test/alpha\" }]",
        "",
    );
    write_agent(&root, "inheritor", "subagent", true, "[]", "");
    let loaded = load_from_roots(None, Some(&root)).expect("configuration");
    let catalog = Arc::new(Catalog::embedded().expect("catalog"));
    let manager = Arc::new(
        ModelSetManager::new(
            loaded.runtime.providers.clone(),
            catalog,
            CredentialStore::new(directory.path().join("credentials")),
        )
        .expect("model manager"),
    );
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config: loaded,
        model_manager: Arc::clone(&manager),
        tools: Vec::new(),
    })
    .expect("engine");
    Fixture {
        _directory: directory,
        engine,
        manager,
    }
}

fn selection(agent: &str, model: &str) -> RunSelection {
    RunSelection {
        agent: AgentId::new(agent).expect("agent id"),
        model: ModelSelection {
            model: model.parse().expect("model key"),
            variant: None,
        },
    }
}

fn connect_openai(fixture: &Fixture, id: &str, secret: &str) {
    fixture
        .manager
        .connect(&CredentialConnectRequest {
            client_connect_id: id.into(),
            provider_id: ProviderId::new("openai").expect("provider"),
            catalog_revision: format!("sha256:{MODELS_DEV_ARTIFACT_SHA256}"),
            credentials: BTreeMap::from([("OPENAI_API_KEY".into(), secret.into())]),
        })
        .expect("connect provider");
}

fn persisted_turn(text: &str) -> PersistedModelTurn {
    PersistedModelTurn {
        content: vec![PersistedAssistantPart::Text {
            text: text.into(),
            metadata: None,
        }],
        provider_options: BTreeMap::new(),
        finish_reason: ModelFinishReason::Stop,
        usage: Usage::default(),
        response_metadata: BTreeMap::new(),
        provider_metadata: BTreeMap::new(),
        native_replay: None,
    }
}

fn committed_turn_event(
    session: SessionId,
    run: RunId,
    seq: u64,
    binding: &cookie_agent_models::FrozenModelBinding,
    text: &str,
) -> StoredEvent {
    StoredEvent {
        event_schema_version: EventSchemaVersion::current(),
        session_id: session,
        run_id: Some(run),
        seq,
        timestamp: jiff::Timestamp::now(),
        payload: Event::ModelTurnCommitted {
            attempt_id: AttemptId::new_v7(),
            model_turn_seq: seq,
            resolved_model: wire_model(binding),
            input_through_seq: seq,
            turn: persisted_turn(text),
            warnings: Vec::new(),
        },
    }
}

async fn start(engine: &Engine, session_id: SessionId, agent: &str, model: &str) -> RunId {
    engine
        .start_run(RunStartParams {
            session_id,
            client_run_id: ClientRunId::new(format!("{agent}-{}", uuid::Uuid::now_v7()))
                .expect("client run id"),
            selection: selection(agent, model),
            input: format!("run {agent}"),
        })
        .await
        .expect("run start")
        .run_id
}

async fn stop(engine: &Engine, run_id: RunId, session_id: SessionId) {
    let _ = engine.cancel_run(run_id).await;
    for _ in 0..100 {
        if engine.get_session(session_id).expect("session").status != SessionStatus::Running {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("run did not become terminal");
}

#[tokio::test]
async fn root_session_switches_runnable_agents_and_preserves_the_first_run_snapshot() {
    let fixture = fixture();
    let session = fixture
        .engine
        .create_session(selection("alpha", "test/alpha"))
        .expect("root session");

    let first = start(&fixture.engine, session.session_id, "alpha", "test/alpha").await;
    let first_snapshot = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .runs[&first]
        .agent
        .clone();
    stop(&fixture.engine, first, session.session_id).await;

    let second = start(&fixture.engine, session.session_id, "beta", "test/beta").await;
    let projection = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection");
    assert_eq!(projection.runs[&first].agent, first_snapshot);
    assert_eq!(projection.runs[&first].agent.agent.as_str(), "alpha");
    assert_eq!(projection.runs[&second].agent.agent.as_str(), "beta");
    stop(&fixture.engine, second, session.session_id).await;

    let mismatch = fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: ClientRunId::new("beta-wrong-model").expect("client run id"),
            selection: selection("beta", "test/alpha"),
            input: "reject mismatched suffix".into(),
        })
        .await
        .expect_err("model outside beta fallback rejected");
    assert!(matches!(mismatch, EngineError::Model(_)));

    for (agent, model) in [
        ("disabled", "test/alpha"),
        ("unavailable", "openai/gpt-5.6-sol"),
    ] {
        let error = fixture
            .engine
            .start_run(RunStartParams {
                session_id: session.session_id,
                client_run_id: ClientRunId::new(format!("reject-{agent}")).expect("client run id"),
                selection: selection(agent, model),
                input: "reject".into(),
            })
            .await
            .expect_err("ineligible root agent");
        assert!(matches!(error, EngineError::IneligibleAgent(id) if id.as_str() == agent));
    }
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn root_title_reset_after_same_fingerprint_credential_rotation_uses_old_handle() {
    let fixture = fixture();
    connect_openai(&fixture, "first", "credential-one");
    let retained = fixture.manager.current();
    let session = fixture
        .engine
        .create_session(selection("alpha", "test/alpha"))
        .expect("session");
    let run = start(&fixture.engine, session.session_id, "alpha", "test/alpha").await;
    stop(&fixture.engine, run, session.session_id).await;
    connect_openai(&fixture, "rotate", "credential-two");
    assert_eq!(
        fixture.manager.current().model_set().fingerprint(),
        retained.model_set().fingerprint()
    );

    let projection = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection");
    let policy = fixture
        .engine
        .historical_title_policy(&projection.log.events(), run)
        .expect("historical title policy");
    assert!(Arc::ptr_eq(&policy.model_snapshot, &retained));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn root_title_reset_after_behavior_changing_publication_uses_old_snapshot() {
    let fixture = fixture();
    let retained = fixture.manager.current();
    let session = fixture
        .engine
        .create_session(selection("alpha", "test/alpha"))
        .expect("session");
    let run = start(&fixture.engine, session.session_id, "alpha", "test/alpha").await;
    stop(&fixture.engine, run, session.session_id).await;
    connect_openai(&fixture, "enable-openai", "credential");
    assert_ne!(
        fixture.manager.current().model_set().fingerprint(),
        retained.model_set().fingerprint()
    );

    let projection = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection");
    let policy = fixture
        .engine
        .historical_title_policy(&projection.log.events(), run)
        .expect("historical title policy");
    assert!(Arc::ptr_eq(&policy.model_snapshot, &retained));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn title_reset_with_multiple_runs_uses_snapshot_for_latest_committed_turn() {
    let fixture = fixture();
    let session = fixture
        .engine
        .create_session(selection("alpha", "test/alpha"))
        .expect("session");
    let first = start(&fixture.engine, session.session_id, "alpha", "test/alpha").await;
    stop(&fixture.engine, first, session.session_id).await;
    let first_snapshot = fixture
        .engine
        .historical_run_model_snapshot(first)
        .expect("first snapshot");

    connect_openai(&fixture, "between-runs", "credential");
    let second = start(&fixture.engine, session.session_id, "beta", "test/beta").await;
    stop(&fixture.engine, second, session.session_id).await;
    let second_snapshot = fixture
        .engine
        .historical_run_model_snapshot(second)
        .expect("second snapshot");
    assert!(!Arc::ptr_eq(&first_snapshot, &second_snapshot));

    let first_binding = first_snapshot
        .model_set()
        .freeze(&selection("alpha", "test/alpha").model)
        .expect("first binding");
    let second_binding = second_snapshot
        .model_set()
        .freeze(&selection("beta", "test/beta").model)
        .expect("second binding");
    let committed = vec![
        committed_turn_event(session.session_id, first, 1, &first_binding, "first"),
        committed_turn_event(session.session_id, second, 2, &second_binding, "second"),
    ];
    let (selected_run, _, _) = title_regeneration_target(&committed).expect("title target");
    assert_eq!(selected_run, second);

    let projection = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection");
    let policy = fixture
        .engine
        .historical_title_policy(&projection.log.events(), selected_run)
        .expect("selected title policy");
    assert!(Arc::ptr_eq(&policy.model_snapshot, &second_snapshot));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn delegated_title_reset_uses_exact_delegated_run_snapshot() {
    let fixture = fixture();
    let retained = fixture.manager.current();
    let agents = fixture
        .engine
        .materialize_agents(retained.model_set())
        .expect("agents");
    let child_selection = selection("child", "test/alpha");
    let child = resolve_agent(&agents, &child_selection.agent).expect("child agent");
    let policy = freeze_agent_policy(
        child,
        Arc::clone(&agents),
        Arc::clone(&retained),
        &child_selection.model,
        None,
        None,
        ResultLimits {
            tool_output_max_lines: 100,
            tool_output_max_bytes: 10_000,
        },
    )
    .expect("child policy");
    let session_id = SessionId::new_v7();
    let origin = SessionOrigin::Delegated {
        root_session_id: SessionId::new_v7(),
        parent_session_id: SessionId::new_v7(),
        parent_run_id: RunId::new_v7(),
        parent_tool_call_id: ToolCallId::new_v7(),
        invocation_id: InvocationId::new_v7(),
        depth: 1,
    };
    let cwd = cwd_identity(fixture.engine.inner.store.cwd()).expect("cwd");
    fixture
        .engine
        .inner
        .store
        .create(
            session_meta(
                session_id,
                origin.clone(),
                cwd.clone(),
                child_selection.clone(),
            ),
            Event::SessionCreated {
                origin,
                cwd_identity: cwd,
                creation_selection: child_selection,
                creation_agent: Box::new(policy.agent),
                model_snapshot_fingerprint: protocol_digest(
                    policy.model_snapshot.model_set().fingerprint(),
                )
                .expect("fingerprint"),
            },
        )
        .expect("delegated session");
    fixture
        .engine
        .inner
        .session_model_snapshots
        .lock()
        .expect("session snapshots")
        .insert(session_id, Arc::clone(&retained));
    fixture.engine.spawn_actor(session_id);
    connect_openai(&fixture, "after-child-admission", "credential");

    let run = start(&fixture.engine, session_id, "child", "test/alpha").await;
    stop(&fixture.engine, run, session_id).await;
    let projection = fixture
        .engine
        .inner
        .store
        .get(session_id)
        .expect("projection");
    let historical = fixture
        .engine
        .historical_title_policy(&projection.log.events(), run)
        .expect("delegated title policy");
    assert!(Arc::ptr_eq(&historical.model_snapshot, &retained));
    fixture.engine.shutdown().await;
}

#[test]
fn repeated_attempt_and_tool_loop_resolution_stays_on_the_run_snapshot_after_publication() {
    let fixture = fixture();
    let retained = fixture.manager.current();
    let agents = fixture
        .engine
        .materialize_agents(retained.model_set())
        .expect("agents");
    let agent = resolve_agent(&agents, &AgentId::new("alpha").expect("agent")).expect("alpha");
    let policy = freeze_agent_policy(
        agent,
        Arc::clone(&agents),
        Arc::clone(&retained),
        &selection("alpha", "test/alpha").model,
        None,
        None,
        ResultLimits {
            tool_output_max_lines: 100,
            tool_output_max_bytes: 10_000,
        },
    )
    .expect("policy");
    let binding = &policy.selected_suffix[0];
    let executable = policy.model_snapshot.resolve(binding).expect("executable");

    let published = fixture.manager.refresh().expect("publish same fingerprint");
    assert_eq!(
        published.model_set().fingerprint(),
        retained.model_set().fingerprint()
    );
    assert!(!Arc::ptr_eq(&published, &retained));
    for _ in 0..4 {
        let resolved = policy
            .model_snapshot
            .resolve(binding)
            .expect("retained resolve");
        assert!(Arc::ptr_eq(executable.model(), resolved.model()));
    }
}

#[test]
fn child_admission_freezes_from_the_invoking_parent_snapshot_after_provider_publication() {
    let fixture = fixture();
    let retained = fixture.manager.current();
    let agents = fixture
        .engine
        .materialize_agents(retained.model_set())
        .expect("agents");
    let parent = resolve_agent(&agents, &AgentId::new("alpha").expect("agent")).expect("alpha");
    let parent_policy = freeze_agent_policy(
        parent,
        Arc::clone(&agents),
        Arc::clone(&retained),
        &selection("alpha", "test/alpha").model,
        None,
        None,
        ResultLimits {
            tool_output_max_lines: 100,
            tool_output_max_bytes: 10_000,
        },
    )
    .expect("parent policy");
    let published = fixture.manager.refresh().expect("provider publication");
    assert!(!Arc::ptr_eq(&published, &retained));

    let child = resolve_agent(
        &parent_policy.registry,
        &AgentId::new("inheritor").expect("child agent"),
    )
    .expect("inheriting child");
    let inherited = parent_policy.active_suffix(0);
    let child_policy = freeze_delegated_child_policy(
        child,
        &parent_policy,
        &inherited[0].resolved.selection,
        inherited,
        2,
        ResultLimits {
            tool_output_max_lines: 100,
            tool_output_max_bytes: 10_000,
        },
    )
    .expect("child admission policy");
    assert!(Arc::ptr_eq(&child_policy.model_snapshot, &retained));
    assert_eq!(child_policy.selected_suffix, inherited);
}

#[tokio::test]
async fn delegated_session_cannot_switch_from_its_frozen_child_agent() {
    let fixture = fixture();
    let snapshot = fixture.engine.inner.model_manager.current();
    let agents = fixture
        .engine
        .materialize_agents(snapshot.model_set())
        .expect("agents");
    let child_selection = selection("child", "test/alpha");
    let child = resolve_agent(&agents, &child_selection.agent).expect("child agent");
    let policy = freeze_agent_policy(
        child,
        Arc::clone(&agents),
        Arc::clone(&snapshot),
        &child_selection.model,
        None,
        None,
        ResultLimits {
            tool_output_max_lines: 100,
            tool_output_max_bytes: 10_000,
        },
    )
    .expect("child policy");
    let session_id = SessionId::new_v7();
    let origin = SessionOrigin::Delegated {
        root_session_id: SessionId::new_v7(),
        parent_session_id: SessionId::new_v7(),
        parent_run_id: RunId::new_v7(),
        parent_tool_call_id: ToolCallId::new_v7(),
        invocation_id: InvocationId::new_v7(),
        depth: 1,
    };
    let cwd = cwd_identity(fixture.engine.inner.store.cwd()).expect("cwd");
    fixture
        .engine
        .inner
        .store
        .create(
            session_meta(
                session_id,
                origin.clone(),
                cwd.clone(),
                child_selection.clone(),
            ),
            Event::SessionCreated {
                origin,
                cwd_identity: cwd,
                creation_selection: child_selection,
                creation_agent: Box::new(policy.agent),
                model_snapshot_fingerprint: protocol_digest(
                    policy.model_snapshot.model_set().fingerprint(),
                )
                .expect("fingerprint"),
            },
        )
        .expect("delegated session");
    fixture.engine.spawn_actor(session_id);

    let error = fixture
        .engine
        .start_run(RunStartParams {
            session_id,
            client_run_id: ClientRunId::new("delegated-switch").expect("client run id"),
            selection: selection("beta", "test/beta"),
            input: "switch".into(),
        })
        .await
        .expect_err("delegated agent switch rejected");
    assert!(matches!(error, EngineError::Model(_)));
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(session_id)
            .expect("projection")
            .runs
            .is_empty()
    );
    fixture.engine.shutdown().await;
}
