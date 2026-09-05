use super::*;

use cookie_agent_protocol::{
    EventOrigin, GoalItem, GoalLifecycleAction, ProducerDeliveryMode, ProducerIdempotencyKey,
    ProducerOwner, SessionGoalLifecycleParams, SessionGoalSetParams,
};

use crate::runtime::producers::ProducerAuthority;

const PRESET: &str = "goal-choice";

fn origin() -> EventOrigin {
    EventOrigin::new("client:goal-selection-test").expect("event origin")
}

fn authority() -> ProducerAuthority {
    ProducerAuthority {
        owner: ProducerOwner::Delegation {
            invocation_id: InvocationId::new_v7(),
        },
        connection_epoch: None,
    }
}

fn events(engine: &Engine, session_id: SessionId) -> Vec<cookie_agent_protocol::StoredEvent> {
    engine
        .inner
        .store
        .get(session_id)
        .expect("goal selection projection")
        .log
        .events()
}

fn run_selection_for(
    engine: &Engine,
    session_id: SessionId,
    run_id: cookie_agent_protocol::RunId,
) -> RunSelection {
    events(engine, session_id)
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::RunStarted { selection, .. } if event.run_id == Some(run_id) => {
                Some(selection)
            }
            _ => None,
        })
        .expect("run_started selection")
}

fn assert_wire_selection(request: &str, selection: &RunSelection) {
    let body = request_body(request);
    assert_eq!(body["model"], selection.model.model.model_id().as_str());
    if selection.model.variant.as_ref().map(VariantId::as_str) == Some("precise") {
        assert_eq!(body["temperature"], 0.25);
        assert!(request.contains("Goal choice preset prompt."));
    } else {
        assert_ne!(body["temperature"], 0.25);
        assert!(!request.contains("Goal choice preset prompt."));
    }
}

async fn selection_fixture(endpoint: &str) -> (Fixture, RunSelection, RunSelection) {
    let mut fixture = synthetic_default_fixture_with_config(
        Some(
            "---\ndescription: Goal selection primary\nmode: primary\nenabled: true\nmodels:\n  - { model: \"custom.test/z-model\", variant: base }\n  - { model: \"custom.test/a-model\", variant: precise }\npermissions: {}\n---\nShared goal selection prompt.\n",
        ),
        endpoint,
        "",
    );
    fixture.engine.shutdown().await;

    let primary = AgentId::new("primary").expect("primary agent ID");
    let mut preset_agents = fixture.config.agents.clone();
    preset_agents
        .get_mut(&primary)
        .expect("preset primary")
        .body = "Goal choice preset prompt.\n".into();
    fixture
        .config
        .agent_presets
        .insert(PRESET.into(), preset_agents);
    fixture.config.runtime.session_title.generate_on_first_turn = false;
    fixture.engine = reopen_engine(&fixture);

    let selection_a = RunSelection {
        agent: primary.clone(),
        model: ModelSelection {
            model: "custom.test/z-model".parse().expect("selection A model"),
            variant: None,
        },
        preset: None,
    };
    let selection_b = RunSelection {
        agent: primary,
        model: ModelSelection {
            model: "custom.test/a-model".parse().expect("selection B model"),
            variant: Some(VariantId::new("precise").expect("selection B variant")),
        },
        preset: Some(PRESET.into()),
    };
    (fixture, selection_a, selection_b)
}

async fn held_channel_server(
    expected_requests: usize,
) -> (
    String,
    tokio::sync::mpsc::UnboundedSender<MatchedScriptedResponse>,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("goal selection listener");
    let address = listener.local_addr().expect("goal selection address");
    let (responses, mut response_rx) =
        tokio::sync::mpsc::unbounded_channel::<MatchedScriptedResponse>();
    let (seen_tx, seen) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(expected_requests);
        let mut pending_responses = Vec::<MatchedScriptedResponse>::new();
        for _ in 0..expected_requests {
            let (mut socket, request) =
                accept_scripted_planned_request(&listener, "goal selection request").await;
            let request = String::from_utf8(request).expect("goal selection request UTF-8");
            seen_tx
                .send(request.clone())
                .expect("report held goal selection request");
            let body = loop {
                if let Some(index) = pending_responses
                    .iter()
                    .position(|response| response.matches(request.as_bytes()))
                {
                    break pending_responses.remove(index).body;
                }
                pending_responses.push(
                    response_rx
                        .recv()
                        .await
                        .expect("matching goal selection response"),
                );
            };
            requests.push(request);
            write_scripted_sse(&mut socket, &body).await;
        }
        spawn_scripted_auxiliary_tail(listener);
        requests
    });
    (format!("http://{address}/v1"), responses, seen, task)
}

async fn next_request(seen: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(test_timeout(5), seen.recv())
        .await
        .expect("goal selection HTTP request timeout")
        .expect("goal selection HTTP request")
}

async fn complete_goal(engine: &Engine, session_id: SessionId) {
    engine
        .goal_update(
            session_id,
            cookie_agent_protocol::GoalUpdateParams {
                items: vec![GoalItem {
                    description: "Selection regression is covered".into(),
                    finished: true,
                }],
            },
        )
        .await
        .expect("complete goal");
}

async fn wait_for_run_started(
    engine: &Engine,
    session_id: SessionId,
    excluded: Option<cookie_agent_protocol::RunId>,
) -> cookie_agent_protocol::RunId {
    await_event(engine, session_id, "selected automatic run", |event| {
        event.run_id != excluded && matches!(event.payload, EventPayload::RunStarted { .. })
    })
    .await
    .run_id
    .expect("automatic run ID")
}

#[tokio::test]
async fn explicit_goal_selection_is_durable_and_drives_reopened_goal_run() {
    let (endpoint, responses, mut seen, captured) = held_channel_server(1).await;
    let (fixture, selection_a, selection_b) = selection_fixture(&endpoint).await;
    let session = fixture
        .engine
        .create_session(selection_a)
        .expect("goal selection session");
    let session_id = session.session_id;
    let blocker_authority = authority();
    fixture
        .engine
        .register_producer(session_id, blocker_authority)
        .await
        .expect("other producer blocks goal wake");

    fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Resume with the explicitly selected model".into(),
                selection: Some(selection_b.clone()),
            },
            origin(),
        )
        .await
        .expect("set selected goal");
    let activated = events(&fixture.engine, session_id)
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::GoalActivated { selection, .. } => Some(selection),
            _ => None,
        })
        .expect("durable goal activation");
    assert_eq!(activated, Some(selection_b.clone()));
    assert!(
        !events(&fixture.engine, session_id)
            .iter()
            .any(|event| matches!(event.payload, EventPayload::RunStarted { .. }))
    );

    fixture.engine.shutdown().await;
    drop(fixture.engine);
    let engine = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    engine
        .resume(session_id)
        .await
        .expect("adopt selected goal after restart");
    let run_id = wait_for_run_started(&engine, session_id, None).await;
    assert_eq!(run_selection_for(&engine, session_id, run_id), selection_b);
    await_event(&engine, session_id, "reopened model request", |event| {
        event.run_id == Some(run_id)
            && matches!(event.payload, EventPayload::ModelAttemptStarted { .. })
    })
    .await;
    let held = next_request(&mut seen).await;
    assert_wire_selection(&held, &selection_b);
    complete_goal(&engine, session_id).await;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "Resume with the explicitly selected model",
            scripted_text_body("completed selected goal"),
        ))
        .expect("selected goal response");
    wait_for_session_not_running(&engine, session_id).await;

    let requests = captured.await.expect("selected goal request");
    assert_eq!(requests.len(), 1);
    assert_wire_selection(&requests[0], &selection_b);
    assert_eq!(
        events(&engine, session_id)
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::RunStarted { .. }))
            .count(),
        1
    );
    assert!(!events(&engine, session_id).iter().any(|event| {
        matches!(
            event.payload,
            EventPayload::ProducerMessageAccepted {
                producer_owner: ProducerOwner::GoalControl { .. },
                ..
            }
        )
    }));
    engine.shutdown().await;
}

#[derive(Clone, Copy)]
enum SelectionChange {
    Set,
    Resume,
}

async fn assert_active_run_keeps_selection(change: SelectionChange) {
    let (endpoint, responses, mut seen, captured) = held_channel_server(2).await;
    let (fixture, selection_a, selection_b) = selection_fixture(&endpoint).await;
    let session = fixture
        .engine
        .create_session(selection_a.clone())
        .expect("active attribution session");
    let session_id = session.session_id;
    let blocker_authority = authority();
    let blocker = fixture
        .engine
        .register_producer(session_id, blocker_authority.clone())
        .await
        .expect("goal wake blocker");

    let paused = if matches!(change, SelectionChange::Resume) {
        let goal = fixture
            .engine
            .set_session_goal(
                SessionGoalSetParams {
                    session_id,
                    objective: "Resume on selection B".into(),
                    selection: None,
                },
                origin(),
            )
            .await
            .expect("set resumable goal")
            .goal;
        Some(
            fixture
                .engine
                .change_session_goal_lifecycle(
                    SessionGoalLifecycleParams {
                        session_id,
                        goal_id: goal.goal_id,
                        expected_revision: goal.revision,
                        action: GoalLifecycleAction::Pause,
                        selection: None,
                    },
                    origin(),
                )
                .await
                .expect("pause goal")
                .goal,
        )
    } else {
        None
    };

    let active_run = fixture
        .engine
        .start_run(
            RunStartParams {
                session_id,
                client_run_id: ClientRunId::new(match change {
                    SelectionChange::Set => "active-during-goal-set",
                    SelectionChange::Resume => "active-during-goal-resume",
                })
                .expect("active run client ID"),
                selection: selection_a.clone(),
                input: "hold selection A while changing the goal".into(),
            },
            origin(),
        )
        .await
        .expect("active selection A run")
        .run_id;
    await_event(
        &fixture.engine,
        session_id,
        "active A model request",
        |event| {
            event.run_id == Some(active_run)
                && matches!(event.payload, EventPayload::ModelAttemptStarted { .. })
        },
    )
    .await;
    let held_a = next_request(&mut seen).await;
    assert_wire_selection(&held_a, &selection_a);

    match paused {
        None => {
            fixture
                .engine
                .set_session_goal(
                    SessionGoalSetParams {
                        session_id,
                        objective: "Set selection B during an active A run".into(),
                        selection: Some(selection_b.clone()),
                    },
                    origin(),
                )
                .await
                .expect("set goal during active run");
        }
        Some(paused) => {
            fixture
                .engine
                .change_session_goal_lifecycle(
                    SessionGoalLifecycleParams {
                        session_id,
                        goal_id: paused.goal_id,
                        expected_revision: paused.revision,
                        action: GoalLifecycleAction::Resume,
                        selection: Some(selection_b.clone()),
                    },
                    origin(),
                )
                .await
                .expect("resume goal during active run");
        }
    }
    assert_eq!(
        run_selection_for(&fixture.engine, session_id, active_run),
        selection_a
    );

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "hold selection A",
            scripted_text_body("selection A run finished"),
        ))
        .expect("active A response");
    wait_for_session_not_running(&fixture.engine, session_id).await;
    fixture
        .engine
        .unregister_producer(session_id, blocker_authority, blocker)
        .await
        .expect("release goal wake");
    let goal_run = wait_for_run_started(&fixture.engine, session_id, Some(active_run)).await;
    assert_eq!(
        run_selection_for(&fixture.engine, session_id, goal_run),
        selection_b
    );
    await_event(
        &fixture.engine,
        session_id,
        "selection B model request",
        |event| {
            event.run_id == Some(goal_run)
                && matches!(event.payload, EventPayload::ModelAttemptStarted { .. })
        },
    )
    .await;
    let held_b = next_request(&mut seen).await;
    assert_wire_selection(&held_b, &selection_b);
    complete_goal(&fixture.engine, session_id).await;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "selection B",
            scripted_text_body("selection B goal finished"),
        ))
        .expect("goal B response");
    wait_for_session_not_running(&fixture.engine, session_id).await;

    let requests = captured.await.expect("active attribution requests");
    assert_eq!(requests.len(), 2);
    assert_wire_selection(&requests[0], &selection_a);
    assert_wire_selection(&requests[1], &selection_b);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn goal_set_and_resume_selection_do_not_reattribute_an_active_run() {
    for change in [SelectionChange::Set, SelectionChange::Resume] {
        assert_active_run_keeps_selection(change).await;
    }
}

#[tokio::test]
async fn none_and_terminal_goal_choices_preserve_ordinary_selection_fallbacks() {
    let (endpoint, responses, mut seen, captured) = held_channel_server(3).await;
    let (fixture, selection_a, selection_b) = selection_fixture(&endpoint).await;

    let none_goal = fixture
        .engine
        .create_session(selection_a.clone())
        .expect("None goal session");
    let none_authority = authority();
    let none_blocker = fixture
        .engine
        .register_producer(none_goal.session_id, none_authority.clone())
        .await
        .expect("None goal blocker");
    fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id: none_goal.session_id,
                objective: "Use the legacy selection fallback".into(),
                selection: None,
            },
            origin(),
        )
        .await
        .expect("set legacy goal");
    fixture
        .engine
        .unregister_producer(none_goal.session_id, none_authority, none_blocker)
        .await
        .expect("release legacy goal");
    let none_run = wait_for_run_started(&fixture.engine, none_goal.session_id, None).await;
    assert_eq!(
        run_selection_for(&fixture.engine, none_goal.session_id, none_run),
        selection_a
    );
    let none_request = next_request(&mut seen).await;
    assert_wire_selection(&none_request, &selection_a);
    complete_goal(&fixture.engine, none_goal.session_id).await;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "Use the legacy selection fallback",
            scripted_text_body("legacy goal complete"),
        ))
        .expect("legacy goal response");
    wait_for_session_not_running(&fixture.engine, none_goal.session_id).await;

    let producer_only = fixture
        .engine
        .create_session(selection_a.clone())
        .expect("producer-only session");
    let producer_authority = authority();
    let producer = fixture
        .engine
        .register_producer(producer_only.session_id, producer_authority.clone())
        .await
        .expect("producer-only registration");
    fixture
        .engine
        .send_producer_message(
            producer_only.session_id,
            producer_authority.clone(),
            producer,
            ProducerDeliveryMode::Queue,
            ProducerIdempotencyKey::new("producer-only-selection").expect("producer key"),
            "ordinary producer-only wake".into(),
        )
        .await
        .expect("producer-only message");
    fixture
        .engine
        .unregister_producer(producer_only.session_id, producer_authority, producer)
        .await
        .expect("release producer-only wake");
    let producer_run = wait_for_run_started(&fixture.engine, producer_only.session_id, None).await;
    assert_eq!(
        run_selection_for(&fixture.engine, producer_only.session_id, producer_run),
        selection_a
    );
    let producer_request = next_request(&mut seen).await;
    assert_wire_selection(&producer_request, &selection_a);
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "ordinary producer-only wake",
            scripted_text_body("producer-only complete"),
        ))
        .expect("producer-only response");
    wait_for_session_not_running(&fixture.engine, producer_only.session_id).await;

    let terminal = fixture
        .engine
        .create_session(selection_a.clone())
        .expect("terminal goal session");
    let terminal_authority = authority();
    let terminal_producer = fixture
        .engine
        .register_producer(terminal.session_id, terminal_authority.clone())
        .await
        .expect("terminal generic producer");
    fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id: terminal.session_id,
                objective: "Do not leak this choice to generic work".into(),
                selection: Some(selection_b),
            },
            origin(),
        )
        .await
        .expect("set terminal selected goal");
    fixture
        .engine
        .send_producer_message(
            terminal.session_id,
            terminal_authority.clone(),
            terminal_producer,
            ProducerDeliveryMode::Queue,
            ProducerIdempotencyKey::new("terminal-generic-selection").expect("producer key"),
            "generic work after selected goal completes".into(),
        )
        .await
        .expect("terminal generic message");
    complete_goal(&fixture.engine, terminal.session_id).await;
    fixture
        .engine
        .unregister_producer(terminal.session_id, terminal_authority, terminal_producer)
        .await
        .expect("release terminal generic wake");
    let terminal_run = wait_for_run_started(&fixture.engine, terminal.session_id, None).await;
    assert_eq!(
        run_selection_for(&fixture.engine, terminal.session_id, terminal_run),
        selection_a
    );
    let terminal_request = next_request(&mut seen).await;
    assert_wire_selection(&terminal_request, &selection_a);
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "generic work after selected goal completes",
            scripted_text_body("terminal generic work complete"),
        ))
        .expect("terminal generic response");
    wait_for_session_not_running(&fixture.engine, terminal.session_id).await;

    let requests = captured.await.expect("fallback selection requests");
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_wire_selection(request, &selection_a);
    }
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn invalid_and_non_resume_selections_are_rejected_before_goal_events() {
    let (fixture, selection_a, selection_b) = selection_fixture("http://127.0.0.1:9/v1").await;
    let session = fixture
        .engine
        .create_session(selection_a.clone())
        .expect("validation session");
    let session_id = session.session_id;
    fixture
        .engine
        .register_producer(session_id, authority())
        .await
        .expect("validation wake blocker");

    let mut invalid_model = selection_b.clone();
    invalid_model.model.model = "custom.test/missing".parse().expect("invalid model key");
    let mut invalid_variant = selection_b.clone();
    invalid_variant.model.variant = Some(VariantId::new("missing").expect("invalid variant ID"));
    let mut invalid_preset = selection_b.clone();
    invalid_preset.preset = Some("missing".into());
    for (name, selection) in [
        ("model", invalid_model.clone()),
        ("variant", invalid_variant.clone()),
        ("preset", invalid_preset.clone()),
    ] {
        fixture
            .engine
            .set_session_goal(
                SessionGoalSetParams {
                    session_id,
                    objective: format!("Reject invalid {name}"),
                    selection: Some(selection),
                },
                origin(),
            )
            .await
            .expect_err("invalid goal selection must fail");
    }
    assert!(events(&fixture.engine, session_id).iter().all(|event| {
        !matches!(
            event.payload,
            EventPayload::GoalActivated { .. } | EventPayload::GoalLifecycleChanged { .. }
        )
    }));

    let goal = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Validate lifecycle selection".into(),
                selection: None,
            },
            origin(),
        )
        .await
        .expect("valid goal")
        .goal;
    for action in [GoalLifecycleAction::Pause, GoalLifecycleAction::Cancel] {
        fixture
            .engine
            .change_session_goal_lifecycle(
                SessionGoalLifecycleParams {
                    session_id,
                    goal_id: goal.goal_id,
                    expected_revision: goal.revision,
                    action,
                    selection: Some(selection_b.clone()),
                },
                origin(),
            )
            .await
            .expect_err("selection is accepted only on resume");
    }
    assert_eq!(
        events(&fixture.engine, session_id)
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::GoalLifecycleChanged { .. }))
            .count(),
        0
    );

    let paused = fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: goal.goal_id,
                expected_revision: goal.revision,
                action: GoalLifecycleAction::Pause,
                selection: None,
            },
            origin(),
        )
        .await
        .expect("valid pause")
        .goal;
    for selection in [invalid_model, invalid_variant, invalid_preset] {
        fixture
            .engine
            .change_session_goal_lifecycle(
                SessionGoalLifecycleParams {
                    session_id,
                    goal_id: paused.goal_id,
                    expected_revision: paused.revision,
                    action: GoalLifecycleAction::Resume,
                    selection: Some(selection),
                },
                origin(),
            )
            .await
            .expect_err("invalid resume selection must fail");
    }
    let lifecycle_events = events(&fixture.engine, session_id)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::GoalLifecycleChanged { selection, .. } => Some(selection),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(lifecycle_events, vec![None]);

    let resumed_with_b = fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: paused.goal_id,
                expected_revision: paused.revision,
                action: GoalLifecycleAction::Resume,
                selection: Some(selection_b.clone()),
            },
            origin(),
        )
        .await
        .expect("valid selected resume")
        .goal;
    let paused_again = fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: resumed_with_b.goal_id,
                expected_revision: resumed_with_b.revision,
                action: GoalLifecycleAction::Pause,
                selection: None,
            },
            origin(),
        )
        .await
        .expect("pause selected goal")
        .goal;
    fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: paused_again.goal_id,
                expected_revision: paused_again.revision,
                action: GoalLifecycleAction::Resume,
                selection: None,
            },
            origin(),
        )
        .await
        .expect("resume without replacing stored selection");
    assert_eq!(
        crate::goal_projection::GoalProducerProjection::from_events(&events(
            &fixture.engine,
            session_id
        ))
        .selection,
        Some(selection_b)
    );
    fixture.engine.shutdown().await;
}
