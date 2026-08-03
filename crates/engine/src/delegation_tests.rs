use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::Arc,
};

use cookie_agent_config::{
    AgentType as ConfigAgentType, DelegationPolicy, DepthLimit as ConfigDepthLimit, PolicySnapshot,
    ProfileSnapshot as ConfigProfileSnapshot, ResolvedPermissions, ResultLimits,
};
use cookie_agent_models::{Catalog, CredentialStore, ModelSetManager};
use cookie_agent_protocol::{
    AgentType, Event, InvocationId, ModelFinishReason, ModelRef, PersistedAssistantPart,
    PersistedModelTurn, ProfileIdentity, ReplayDecision, ReplayDisposition, RunId, SessionId,
    SessionOrigin, SessionStatus, ToolCallId, ToolResult, Usage,
};
use uuid::Uuid;

use crate::{
    DelegateHandle, Engine, EngineOptions, completed_delegate_result, invocation_id, session_meta,
    wire_profile,
};

const FINAL_REPORT: &str = "valid child final report";
const CHILD_WARNING: &str = "child replay warning";
const PARENT_WARNING: &str = "parent-owned warning";
const REPLAY_DIAGNOSTIC: &str = "child replay diagnostic";

#[derive(Clone, Copy)]
struct ParentLink {
    session: SessionId,
    run: RunId,
    call: ToolCallId,
    invocation: InvocationId,
}

impl ParentLink {
    fn new(session: SessionId, run: RunId, call: ToolCallId) -> Self {
        Self {
            session,
            run,
            call,
            invocation: invocation_id(session, run, call),
        }
    }
}

fn session_id(value: u128) -> SessionId {
    SessionId(Uuid::from_u128(value))
}

fn run_id(value: u128) -> RunId {
    RunId(Uuid::from_u128(value))
}

fn tool_call_id(value: u128) -> ToolCallId {
    ToolCallId(Uuid::from_u128(value))
}

fn policy(name: &str, agent_type: ConfigAgentType, delegates: bool) -> PolicySnapshot {
    PolicySnapshot {
        profile: ConfigProfileSnapshot {
            name: name.into(),
            r#type: agent_type,
        },
        models: Vec::new(),
        tools: BTreeSet::new(),
        permissions: ResolvedPermissions { rules: Vec::new() },
        delegation: DelegationPolicy {
            enabled: delegates,
            allowed_profiles: if delegates {
                BTreeSet::from(["worker".into()])
            } else {
                BTreeSet::new()
            },
            depth_limit: if delegates {
                ConfigDepthLimit::Finite(1)
            } else {
                ConfigDepthLimit::Finite(0)
            },
        },
        result_limits: ResultLimits::default(),
    }
}

fn model(name: &str) -> ModelRef {
    ModelRef {
        name: name.into(),
        provider_id: format!("{name}.provider"),
        model_id: format!("{name}.model"),
        adapter_id: format!("{name}.adapter"),
    }
}

fn turn(content: PersistedAssistantPart, warning: &str) -> PersistedModelTurn {
    PersistedModelTurn {
        content: vec![content],
        provider_options: BTreeMap::new(),
        finish_reason: ModelFinishReason::Stop,
        usage: Usage::default(),
        response_metadata: BTreeMap::new(),
        provider_metadata: BTreeMap::new(),
        warnings: vec![warning.into()],
        native_replay: None,
    }
}

fn test_engine(root: &Path) -> Engine {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).expect("private test root");
    }
    let manager = ModelSetManager::new(
        BTreeMap::new(),
        Arc::new(Catalog::embedded().expect("catalog")),
        CredentialStore::new(root.join("credentials")),
    )
    .expect("model manager");
    Engine::open(EngineOptions {
        data_dir: root.join("data"),
        cwd: root.to_owned(),
        config: cookie_agent_config::Config::default(),
        model_manager: Arc::new(manager),
        tools: Vec::new(),
    })
    .expect("engine")
}

fn install_parent(
    engine: &Engine,
    root: &Path,
    parent_session: SessionId,
    parent_run: RunId,
    call: ToolCallId,
) {
    let parent_policy = policy("parent", ConfigAgentType::Primary, true);
    engine
        .inner
        .store
        .create(
            session_meta(parent_session, SessionOrigin::Root, root, &parent_policy),
            parent_policy.clone(),
        )
        .expect("parent session");
    engine.spawn_actor(parent_session);
    engine
        .append_direct(
            parent_session,
            Some(parent_run),
            Event::RunStarted {
                client_run_id: "parent-run".into(),
                input: "delegate work".into(),
                profile: wire_profile(&parent_policy),
                current_profile: ProfileIdentity {
                    name: "parent".into(),
                    agent_type: AgentType::Primary,
                },
            },
        )
        .expect("parent run");
    engine
        .append_direct(
            parent_session,
            Some(parent_run),
            Event::ModelTurnCommitted {
                model: model("parent-model"),
                input_through_seq: 1,
                turn: turn(
                    PersistedAssistantPart::ToolCall {
                        id: "delegate-model-call".into(),
                        provider_item_id: None,
                        name: "delegate".into(),
                        input: serde_json::json!({"profile":"worker","task":"report"}),
                        raw_input: None,
                        metadata: None,
                    },
                    PARENT_WARNING,
                ),
            },
        )
        .expect("parent model turn");
    engine
        .append_direct(
            parent_session,
            Some(parent_run),
            Event::ToolCallStarted {
                tool_call_id: call,
                model_call_id: "delegate-model-call".into(),
                provider_item_id: None,
                tool: "delegate".into(),
                arguments: serde_json::json!({"profile":"worker","task":"report"}),
            },
        )
        .expect("delegate call");
}

fn install_completed_child(
    engine: &Engine,
    root: &Path,
    parent: ParentLink,
    child_session: SessionId,
    child_run: RunId,
) {
    let child_policy = policy("worker", ConfigAgentType::Subagent, false);
    engine
        .inner
        .store
        .create(
            session_meta(
                child_session,
                SessionOrigin::Delegated {
                    root_session_id: parent.session,
                    parent_session_id: parent.session,
                    parent_run_id: parent.run,
                    parent_tool_call_id: parent.call,
                    invocation_id: parent.invocation,
                    depth: 1,
                },
                root,
                &child_policy,
            ),
            child_policy.clone(),
        )
        .expect("child session");
    engine.spawn_actor(child_session);
    engine
        .append_direct(
            child_session,
            Some(child_run),
            Event::RunStarted {
                client_run_id: format!("delegate:{}", parent.invocation),
                input: "produce report".into(),
                profile: wire_profile(&child_policy),
                current_profile: ProfileIdentity {
                    name: "worker".into(),
                    agent_type: AgentType::SubAgent,
                },
            },
        )
        .expect("child run");
    engine
        .append_direct(
            child_session,
            Some(child_run),
            Event::ModelReplayEvaluated {
                model: model("child-model"),
                decisions: vec![ReplayDecision {
                    history_index: 0,
                    disposition: ReplayDisposition::DiscardedInvalidPayload {
                        reason: REPLAY_DIAGNOSTIC.into(),
                    },
                }],
            },
        )
        .expect("child replay decision");
    engine
        .append_direct(
            child_session,
            Some(child_run),
            Event::ModelTurnCommitted {
                model: model("child-model"),
                input_through_seq: 1,
                turn: turn(
                    PersistedAssistantPart::Text {
                        text: FINAL_REPORT.into(),
                        metadata: None,
                    },
                    CHILD_WARNING,
                ),
            },
        )
        .expect("child model turn");
    engine
        .append_direct(
            child_session,
            Some(child_run),
            Event::RunCompleted {
                final_text: Some(FINAL_REPORT.into()),
            },
        )
        .expect("child completion");
}

fn completed_result(engine: &Engine, parent_session: SessionId, call: ToolCallId) -> ToolResult {
    engine
        .inner
        .store
        .get(parent_session)
        .expect("parent projection")
        .log
        .events()
        .into_iter()
        .find_map(|event| match event.event {
            Event::ToolCallCompleted {
                tool_call_id,
                result,
            } if tool_call_id == call => Some(result),
            _ => None,
        })
        .expect("completed delegate result")
}

fn assert_session_local_ownership(
    engine: &Engine,
    parent_session: SessionId,
    child_session: SessionId,
    call: ToolCallId,
) -> ToolResult {
    assert_ne!(parent_session, child_session);
    let parent = engine
        .inner
        .store
        .get(parent_session)
        .expect("parent projection");
    let child = engine
        .inner
        .store
        .get(child_session)
        .expect("child projection");
    assert!(
        parent
            .log
            .events()
            .iter()
            .all(|event| event.session_id == parent_session)
    );
    assert!(
        child
            .log
            .events()
            .iter()
            .all(|event| event.session_id == child_session)
    );
    assert_eq!(parent.meta.profile.name, "parent");
    assert_eq!(child.meta.profile.name, "worker");
    assert!(matches!(
        child.meta.origin,
        SessionOrigin::Delegated {
            parent_session_id: found_parent,
            parent_tool_call_id: found_call,
            ..
        } if found_parent == parent_session && found_call == call
    ));
    assert!(parent.log.events().iter().any(|event| matches!(
        &event.event,
        Event::RunStarted {
            profile,
            current_profile,
            ..
        } if profile.name == "parent"
            && current_profile.name == "parent"
            && current_profile.agent_type == AgentType::Primary
    )));
    assert!(child.log.events().iter().any(|event| matches!(
        &event.event,
        Event::RunStarted {
            profile,
            current_profile,
            ..
        } if profile.name == "worker"
            && current_profile.name == "worker"
            && current_profile.agent_type == AgentType::SubAgent
    )));

    let parent_turns = parent
        .log
        .events()
        .into_iter()
        .filter_map(|event| match event.event {
            Event::ModelTurnCommitted { model, turn, .. } => Some((model, turn.warnings)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        parent_turns,
        vec![(model("parent-model"), vec![PARENT_WARNING.into()])]
    );

    let child_turns = child
        .log
        .events()
        .into_iter()
        .filter_map(|event| match event.event {
            Event::ModelTurnCommitted { model, turn, .. } => Some((model, turn.warnings)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        child_turns,
        vec![(model("child-model"), vec![CHILD_WARNING.into()])]
    );
    assert!(child.log.events().iter().any(|event| matches!(
        &event.event,
        Event::ModelReplayEvaluated { model: replay_model, decisions }
            if replay_model == &model("child-model")
                && matches!(decisions.as_slice(), [ReplayDecision {
                    history_index: 0,
                    disposition: ReplayDisposition::DiscardedInvalidPayload { reason },
                }] if reason == REPLAY_DIAGNOSTIC)
    )));

    let result = completed_result(engine, parent_session, call);
    assert_eq!(result.title, "Delegate report");
    assert_eq!(result.output, FINAL_REPORT);
    assert_eq!(
        result.metadata,
        serde_json::json!({
            "status": "completed",
            "child_session_id": child_session,
        })
    );
    assert_eq!(result.truncation, None);
    assert!(result.attachments.is_empty());
    assert_eq!(child.status, SessionStatus::Completed);
    result
}

#[tokio::test]
async fn live_delegate_result_keeps_child_warning_and_replay_diagnostics_child_local() {
    let root = tempfile::tempdir().expect("root");
    let engine = test_engine(root.path());
    let parent_session = session_id(1);
    let parent_run = run_id(2);
    let call = tool_call_id(3);
    let child_session = session_id(4);
    let child_run = run_id(5);
    let parent = ParentLink::new(parent_session, parent_run, call);
    install_parent(&engine, root.path(), parent_session, parent_run, call);
    install_completed_child(&engine, root.path(), parent, child_session, child_run);

    let result = engine
        .await_delegate(DelegateHandle {
            invocation_id: parent.invocation,
            child_session_id: child_session,
            child_run_id: child_run,
        })
        .await
        .expect("delegate result");
    engine
        .submit_tool_result(parent_session, parent_run, call, Ok(result))
        .await
        .expect("commit parent result");

    assert_session_local_ownership(&engine, parent_session, child_session, call);
    engine.shutdown().await;
}

#[tokio::test]
async fn restart_recovery_uses_the_same_session_local_completed_delegate_result() {
    let root = tempfile::tempdir().expect("root");
    let engine = test_engine(root.path());
    let parent_session = session_id(11);
    let parent_run = run_id(12);
    let call = tool_call_id(13);
    let child_run = run_id(15);
    let parent = ParentLink::new(parent_session, parent_run, call);
    install_parent(&engine, root.path(), parent_session, parent_run, call);
    let child_policy = policy("worker", ConfigAgentType::Subagent, false);
    let entry = engine
        .inner
        .journal
        .reserve(
            parent.invocation,
            parent_session,
            parent_run,
            call,
            child_policy,
            "restart-fixture".into(),
            crate::journal::DelegateRequestPayload {
                task: "report".into(),
                ..Default::default()
            },
        )
        .expect("journal reservation");
    let child_session = entry.reservation.child_session_id;
    install_completed_child(&engine, root.path(), parent, child_session, child_run);
    engine
        .ensure_parent_link_blocking(parent_session, parent_run, call, child_session)
        .expect("parent link");
    engine
        .inner
        .journal
        .mark_linked(parent.invocation)
        .expect("linked journal");
    engine
        .inner
        .journal
        .mark_run_started(parent.invocation, child_run)
        .expect("run journal");
    engine.shutdown().await;
    drop(engine);

    let reopened = test_engine(root.path());
    reopened
        .resume(parent_session)
        .await
        .expect("resume parent");
    let recovered = assert_session_local_ownership(&reopened, parent_session, child_session, call);
    let direct = completed_delegate_result(
        &reopened
            .inner
            .store
            .get(child_session)
            .expect("child projection"),
        Some(child_run),
        &reopened.inner.artifacts,
    )
    .expect("direct completed result");
    assert_eq!(recovered, direct);
    reopened.shutdown().await;
}
