use std::{
    fs,
    sync::{Arc, atomic::Ordering},
};

use cookie_agent_protocol::{
    ClientRunId, EventPayload, InvocationId, RunStartParams, SessionStatus, SessionTitle,
    ToolCallId,
};

use crate::{Engine, EngineError, EngineOptions};

use super::support::*;

#[test]
fn recovery_snapshot_preserves_published_artifacts_but_not_live_captures() {
    let source = private_tempdir();
    let target = private_tempdir();
    let snapshot = target.path().join("snapshot");
    let artifacts = source.path().join("artifacts");
    let store = crate::ArtifactStore::open(artifacts.clone()).unwrap();
    let (_, digest) = store.retain(b"published output").unwrap();
    let name = format!(".capture-{}-output0.tmp", uuid::Uuid::now_v7());
    let capture = store.create_capture_file(&name).unwrap();
    write_private_test_file(&source.path().join("events.jsonl"), b"durable events");

    copy_private_test_tree(source.path(), &snapshot);

    assert!(artifacts.join(&name).exists());
    assert!(!snapshot.join("artifacts").join(&name).exists());
    assert_eq!(
        fs::read(snapshot.join("artifacts").join(digest)).unwrap(),
        b"published output"
    );
    assert_eq!(
        fs::read(snapshot.join("events.jsonl")).unwrap(),
        b"durable events"
    );
    drop(capture);
}

#[tokio::test]
async fn adopting_a_session_reconciles_its_dead_run_before_resume() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    let projection = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("created projection");
    let run_id = cookie_agent_protocol::RunId::new_v7();
    let selected_suffix = projection.creation_agent.fallback_chain.clone();
    fixture
        .engine
        .inner
        .store
        .append(
            session.session_id,
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::RunStarted {
                client_run_id: ClientRunId::new("dead-owner-run").expect("client run ID"),
                selection,
                agent: Box::new(projection.creation_agent),
                runtime_revision: projection.meta.runtime_revision.clone(),
                catalog_revision: projection.meta.catalog_revision.clone(),
                provider_state_revision: projection.meta.provider_state_revision.clone(),
                model_revision: projection.meta.model_revision.clone(),
                agent_revision: projection.meta.agent_revision.clone(),
                recipe_registry_revision: projection.meta.recipe_registry_revision.clone(),
                manifest_revision: projection.meta.manifest_revision.clone(),
                selected_suffix,
                internal_agents: Vec::new(),
                input_through_seq: 1,
            },
        )
        .expect("start abandoned run");
    fixture
        .engine
        .inner
        .store
        .append(
            session.session_id,
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::UserInputSubmitted {
                input: "persist abandoned run".into(),
            },
        )
        .expect("persist abandoned run");

    fixture.engine.shutdown().await;
    drop(fixture.engine);
    let reopened = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    reopened
        .inner
        .test_hooks
        .adoption_reconcile_failures
        .store(1, Ordering::Release);
    assert!(matches!(
        reopened.resume(session.session_id).await,
        Err(EngineError::ActorStopped)
    ));
    assert!(!reopened.inner.store.is_owned(session.session_id));
    assert!(!reopened.actor_resident_for_test(session.session_id));
    let partially_recovered = reopened
        .inner
        .store
        .get(session.session_id)
        .expect("partial recovery snapshot")
        .log
        .events();
    assert_eq!(
        partially_recovered
            .iter()
            .filter(|event| {
                event.run_id == Some(run_id)
                    && matches!(event.payload, EventPayload::RunInterrupted { .. })
            })
            .count(),
        1
    );
    reopened
        .resume(session.session_id)
        .await
        .expect("adopt session");
    let adopted = reopened
        .inner
        .store
        .get(session.session_id)
        .expect("adopted projection");
    assert_eq!(adopted.runs[&run_id].status, SessionStatus::Interrupted);
    assert_eq!(
        adopted
            .log
            .events()
            .iter()
            .filter(|event| {
                event.run_id == Some(run_id)
                    && matches!(event.payload, EventPayload::RunInterrupted { .. })
            })
            .count(),
        1
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn setup_append_terminal_failure_retains_active_tombstone_until_retry() {
    for inject_message in [false, true] {
        let (mut fixture, selection) = custom_fixture_with_endpoint("http://127.0.0.1:9/v1");
        if inject_message {
            let capabilities = r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["agent_before_start"]}"#;
            reopen_with_interception_plugins(
                &mut fixture,
                vec![(
                    "inject".into(),
                    interception_plugin(
                        "inject",
                        &[
                            ("FIXTURE_CAPABILITIES", capabilities.into()),
                            (
                                "FIXTURE_AGENT_BEFORE_RESULT",
                                r#"{"inject_message":{"role":"user","content":"injected"}}"#.into(),
                            ),
                        ],
                    ),
                )],
            )
            .await;
        }
        let session = fixture.engine.create_session(selection.clone()).unwrap();
        fixture
            .engine
            .inner
            .test_hooks
            .run_setup_append_failures
            .store(2, Ordering::Release);
        let error = fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new(format!("setup-failure-{inject_message}"))
                        .unwrap(),
                    selection,
                    input: "setup input".into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect_err("terminal append failure is propagated");
        assert!(
            error
                .to_string()
                .contains("injected run failed append failure")
        );
        let projection = fixture.engine.inner.store.get(session.session_id).unwrap();
        assert_eq!(projection.status, SessionStatus::Running);
        let run_id = *projection.runs.keys().next().expect("durable started run");
        assert!(fixture.engine.has_active_run_for_test(run_id));

        fixture
            .engine
            .retry_run_setup_terminalization_for_test(run_id)
            .await
            .expect("terminal append retry");
        assert!(!fixture.engine.has_active_run_for_test(run_id));
        assert_eq!(
            fixture
                .engine
                .get_session(session.session_id)
                .unwrap()
                .status,
            SessionStatus::Failed
        );
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn delegation_reservation_reopens_from_parent_events_and_rejects_tampering() {
    let primary = "---\ndescription: Reservation owner\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nReservation owner prompt.\n";
    let (fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
            "http://127.0.0.1:9/v1",
            primary,
            None,
            None,
            false,
            None,
            None,
            4_096,
            None,
            "openai-chat",
        );
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("event-reservation-reopen").expect("run ID"),
                selection,
                input: "scripted root input".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("root run");
    let parent = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("parent projection");
    let agent = parent.creation_agent.clone();
    let runtime = fixture.engine.current_runtime();
    let revisions = crate::delegation_events::DelegationRuntimeRevisions {
        manifest_revision: agent.fallback_chain[0].manifest_revision.clone(),
        runtime_revision: runtime.result.snapshot.runtime_revision.clone(),
        catalog_revision: runtime.result.snapshot.catalog_revision.clone(),
        provider_state_revision: runtime.result.snapshot.provider_state_revision.clone(),
        model_revision: runtime.result.snapshot.model_revision.clone(),
        agent_revision: runtime.result.snapshot.agent_revision.clone(),
        recipe_registry_revision: runtime.result.snapshot.recipe_registry_revision.clone(),
    };
    let request = cookie_agent_protocol::DelegateRequestPayload {
        description: "Scripted delegation".into(),
        prompt: "scripted delegated task".into(),
        title: SessionTitle::new("Scripted delegation").expect("title"),
        resume_session_id: None,
        inherit_context: false,
        seeded_context: Vec::new(),
        background: false,
        staged_skill: None,
    };
    let cache_strategies = vec![
        Some(cookie_agent_protocol::FrozenCacheStrategy::OpenAi {
            prompt_cache_key: Some("persisted-${session_id}".into()),
            prompt_cache_retention: None,
            mode: None,
            ttl: None,
            system: None,
            rolling: None,
        });
        agent.fallback_chain.len()
    ];
    let fingerprint = crate::delegation_events::delegation_request_fingerprint(
        &agent,
        &agent.fallback_chain,
        &cache_strategies,
        &request,
    )
    .expect("request fingerprint");
    let invocation_id = InvocationId::new_v7();
    fixture
        .engine
        .inner
        .delegation_events
        .reserve(
            invocation_id,
            session.session_id,
            run.run_id,
            ToolCallId::new_v7(),
            agent.clone(),
            revisions,
            agent.fallback_chain.clone(),
            cache_strategies.clone(),
            fingerprint,
            request,
        )
        .expect("reservation event");
    let event_path = parent.log.path().to_owned();
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .expect("updated parent")
            .log
            .events()
            .iter()
            .any(|event| matches!(event.payload, EventPayload::DelegationReserved { .. }))
    );
    fixture.engine.shutdown().await;

    let reopened = reopen_engine(&fixture);
    assert_eq!(
        reopened
            .inner
            .delegation_events
            .get(invocation_id)
            .expect("reopened reservation")
            .cache_strategies,
        cache_strategies
    );
    reopened.shutdown().await;

    let source = fs::read_to_string(&event_path).expect("parent events");
    let tampered = source
        .lines()
        .map(|line| {
            let mut value: serde_json::Value = serde_json::from_str(line).expect("event JSON");
            if value["payload"]["type"] == "delegation_reserved" {
                value["payload"]["request"]["description"] = serde_json::json!("tampered");
            }
            serde_json::to_string(&value).expect("tampered event")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(event_path, tampered).expect("tamper parent event");
    let rejected = Engine::open(EngineOptions {
        data_dir: fixture._directory.path().join("data"),
        cwd: fixture._directory.path().to_owned(),
        config: fixture.config,
        model_manager: Arc::clone(&fixture.manager),
        tools: Vec::new(),
    });
    assert!(matches!(
        rejected,
        Err(EngineError::DelegationEvents(
            crate::delegation_events::DelegationEventError::Corrupt(id)
        )) if id == invocation_id
    ));
}

#[tokio::test]
async fn corrupt_delegation_event_is_skipped_without_blocking_other_recovery() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("best-effort-delegations").expect("run ID"),
                selection,
                input: "scripted root input".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("root run");
    let parent = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("parent projection");
    let agent = parent.creation_agent.clone();
    let runtime = fixture.engine.current_runtime();
    let revisions = crate::delegation_events::DelegationRuntimeRevisions {
        manifest_revision: agent.fallback_chain[0].manifest_revision.clone(),
        runtime_revision: runtime.result.snapshot.runtime_revision.clone(),
        catalog_revision: runtime.result.snapshot.catalog_revision.clone(),
        provider_state_revision: runtime.result.snapshot.provider_state_revision.clone(),
        model_revision: runtime.result.snapshot.model_revision.clone(),
        agent_revision: runtime.result.snapshot.agent_revision.clone(),
        recipe_registry_revision: runtime.result.snapshot.recipe_registry_revision.clone(),
    };
    let first_id = InvocationId::new_v7();
    let second_id = InvocationId::new_v7();
    let intact_id = InvocationId::new_v7();
    for (invocation_id, description) in [
        (first_id, "skipped run start"),
        (second_id, "skipped finish"),
        (intact_id, "intact reservation"),
    ] {
        let request = cookie_agent_protocol::DelegateRequestPayload {
            description: description.into(),
            prompt: format!("{description} delegated task"),
            title: SessionTitle::new(description).expect("title"),
            resume_session_id: None,
            inherit_context: false,
            seeded_context: Vec::new(),
            background: false,
            staged_skill: None,
        };
        let cache_strategies = vec![None; agent.fallback_chain.len()];
        let fingerprint = crate::delegation_events::delegation_request_fingerprint(
            &agent,
            &agent.fallback_chain,
            &cache_strategies,
            &request,
        )
        .expect("fingerprint");
        fixture
            .engine
            .inner
            .delegation_events
            .reserve(
                invocation_id,
                session.session_id,
                run.run_id,
                ToolCallId::new_v7(),
                agent.clone(),
                revisions.clone(),
                agent.fallback_chain.clone(),
                cache_strategies,
                fingerprint,
                request,
            )
            .expect("reservation event");
    }
    let first_child = fixture
        .engine
        .inner
        .delegation_events
        .get(first_id)
        .expect("first reservation")
        .reservation
        .child_session_id;
    let second_child = fixture
        .engine
        .inner
        .delegation_events
        .get(second_id)
        .expect("second reservation")
        .reservation
        .child_session_id;
    let first_run = cookie_agent_protocol::RunId::new_v7();
    fixture
        .engine
        .inner
        .delegation_events
        .mark_started(first_id)
        .expect("first start");
    fixture
        .engine
        .inner
        .delegation_events
        .mark_run_started(first_id, first_run)
        .expect("first run start");
    fixture
        .engine
        .inner
        .delegation_events
        .mark_finished(first_id, SessionStatus::Completed)
        .expect("first finish");
    fixture
        .engine
        .inner
        .delegation_events
        .mark_started(second_id)
        .expect("second start");
    fixture
        .engine
        .inner
        .delegation_events
        .mark_finished(second_id, SessionStatus::Failed)
        .expect("second finish");
    let event_path = parent.log.path().to_owned();
    fixture.engine.shutdown().await;
    drop(fixture.engine);

    let source = fs::read_to_string(&event_path).expect("parent events");
    let source = source
        .lines()
        .map(|line| {
            let mut value: serde_json::Value = serde_json::from_str(line).expect("event JSON");
            if value["payload"]["type"] == "delegation_run_started"
                && value["payload"]["invocation_id"] == serde_json::json!(first_id)
            {
                value["payload"]["child_run_id"] = serde_json::json!(42);
            }
            if value["payload"]["type"] == "delegation_finished"
                && value["payload"]["invocation_id"] == serde_json::json!(second_id)
            {
                value["payload"]["status"] = serde_json::json!(42);
            }
            serde_json::to_string(&value).expect("corrupt event")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(event_path, source).expect("write corrupt event");

    let reopened = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    reopened
        .resume(session.session_id)
        .await
        .expect("adopt parent for recovery");
    let parent = reopened
        .inner
        .store
        .get(session.session_id)
        .expect("best-effort parent");
    assert_eq!(parent.meta.skipped_events.len(), 2);
    for (invocation_id, child_session_id) in [
        (first_id, first_child),
        (second_id, second_child),
        (
            intact_id,
            reopened
                .inner
                .delegation_events
                .get(intact_id)
                .expect("intact recovered reservation")
                .reservation
                .child_session_id,
        ),
    ] {
        let recovered = reopened
            .inner
            .delegation_events
            .get(invocation_id)
            .expect("recovered delegation");
        assert_eq!(recovered.reservation.child_session_id, child_session_id);
        assert_eq!(recovered.terminal_status, Some(SessionStatus::Failed));
        assert!(
            recovered
                .terminal_reason
                .as_ref()
                .is_some_and(|reason| reason.as_str().contains("child_missing"))
        );
    }
    reopened.shutdown().await;
    drop(reopened);

    let reopened_again =
        reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    for invocation_id in [first_id, second_id, intact_id] {
        assert_eq!(
            reopened_again
                .inner
                .delegation_events
                .get(invocation_id)
                .expect("terminal repair survives another reopen")
                .terminal_status,
            Some(SessionStatus::Failed)
        );
    }
    reopened_again.shutdown().await;
}

#[tokio::test]
async fn a_runtime_revision_index_from_an_older_protocol_still_opens() {
    let fixture = fixture();
    let index = fixture
        .engine
        .inner
        .store
        .workdir_dir_path()
        .join("runtime-revisions-v8.jsonl");
    fixture.engine.shutdown().await;
    let older = fs::read_to_string(&index)
        .expect("runtime revision index")
        .lines()
        .map(|line| {
            let mut record: serde_json::Value = serde_json::from_str(line).expect("record");
            record["protocol_version"] =
                serde_json::json!(cookie_agent_protocol::PROTOCOL_VERSION - 1);
            serde_json::to_string(&record).expect("record JSON") + "\n"
        })
        .collect::<String>();
    fs::write(&index, older).expect("rewrite index as an older protocol wrote it");
    let reopened = reopen_engine(&fixture);
    reopened.shutdown().await;
}
