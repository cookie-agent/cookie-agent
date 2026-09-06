use super::*;

use cookie_agent_protocol::GoalReminderKind;

#[tokio::test]
async fn consumed_goal_reminder_stays_continuation_across_pause_restart_and_resume() {
    let (endpoint, responses, captured) = scripted_channel_server(4).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection)
        .expect("continuation restart session");
    let session_id = session.session_id;
    let blocker_authority = producer_authority();
    let blocker = fixture
        .engine
        .register_producer(session_id, blocker_authority.clone())
        .await
        .expect("continuation restart blocker");
    let goal = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Keep reminder identity durable across restart".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("continuation restart goal")
        .goal;
    fixture
        .engine
        .unregister_producer(session_id, blocker_authority, blocker)
        .await
        .expect("release first reminder");

    let first_admission = await_event(&fixture.engine, session_id, "started reminder", |event| {
        matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id }
        if producer_projection(&fixture.engine, session_id).messages.iter().any(|message| {
            message.message_id == message_id
                && message.reminder.is_some_and(|identity| {
                    identity.goal_id == goal.goal_id
                        && identity.kind == GoalReminderKind::Started
                })
        }))
    })
    .await;
    let first_run = first_admission.run_id.expect("started reminder run");
    let EventPayload::ProducerMessageAdmitted {
        message_id: first_id,
    } = first_admission.payload
    else {
        unreachable!()
    };
    await_event(
        &fixture.engine,
        session_id,
        "started reminder request",
        |event| {
            event.run_id == Some(first_run)
                && matches!(event.payload, EventPayload::ModelRequestPrepared { .. })
        },
    )
    .await;
    let (before_pause_claim, release_pause_claim) =
        fixture.engine.install_prompt_before_claim_hook_for_test();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            &goal.objective,
            scripted_text_body("started reminder committed"),
        ))
        .expect("started reminder response");
    await_event(
        &fixture.engine,
        session_id,
        "started reminder consumed",
        |event| {
            matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, run_id }
                if message_id == first_id && run_id == first_run)
        },
    )
    .await;
    tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), before_pause_claim)
        .await
        .expect("pre-pause continuation gate timeout")
        .expect("pre-pause continuation reached gate");

    let pending_continuation = producer_projection(&fixture.engine, session_id)
        .messages
        .into_iter()
        .find(|message| {
            message.message_id != first_id
                && message.reminder.is_some_and(|identity| {
                    identity.goal_id == goal.goal_id
                        && identity.kind == GoalReminderKind::Continuation
                })
                && !message.consumed
        })
        .expect("unclaimed continuation before pause");
    assert!(pending_continuation.claims.is_empty());
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
            client_origin(),
        )
        .await
        .expect("pause consumed goal")
        .goal;
    let paused_projection = producer_projection(&fixture.engine, session_id);
    assert!(
        paused_projection
            .messages
            .iter()
            .find(|message| message.message_id == pending_continuation.message_id)
            .expect("discarded pre-pause continuation")
            .discarded
    );
    let first_control = paused_projection
        .messages
        .iter()
        .rev()
        .find(|message| matches!(&message.producer_owner, ProducerOwner::GoalControl { .. }))
        .expect("first pause control")
        .message_id;
    release_pause_claim.notify_one();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "paused by the user",
            scripted_text_body("first pause acknowledged"),
        ))
        .expect("first pause response");
    await_event(
        &fixture.engine,
        session_id,
        "first pause consumed",
        |event| {
            matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, .. }
            if message_id == first_control)
        },
    )
    .await;
    wait_for_session_not_running(&fixture.engine, session_id).await;

    fixture.engine.shutdown().await;
    let reopened = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    let resumed = reopened
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: goal.goal_id,
                expected_revision: paused.revision,
                action: GoalLifecycleAction::Resume,
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("resume reopened goal")
        .goal;
    let resumed_admission = await_event(&reopened, session_id, "resumed continuation", |event| {
        matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id }
        if producer_projection(&reopened, session_id).messages.iter().any(|message| {
            message.message_id == message_id
                && message.reminder == Some(cookie_agent_protocol::GoalReminderIdentity {
                    goal_id: goal.goal_id,
                    revision: resumed.revision,
                    kind: GoalReminderKind::Continuation,
                })
        }))
    })
    .await;
    let resumed_run = resumed_admission.run_id.expect("resumed continuation run");
    let EventPayload::ProducerMessageAdmitted {
        message_id: resumed_id,
    } = resumed_admission.payload
    else {
        unreachable!()
    };
    await_event(
        &reopened,
        session_id,
        "resumed continuation request",
        |event| {
            event.run_id == Some(resumed_run)
                && matches!(event.payload, EventPayload::ModelRequestPrepared { .. })
        },
    )
    .await;
    let (before_final_pause, release_final_pause) =
        reopened.install_prompt_before_claim_hook_for_test();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            &goal.objective,
            scripted_text_body("resumed continuation committed"),
        ))
        .expect("resumed continuation response");
    await_event(
        &reopened,
        session_id,
        "resumed continuation consumed",
        |event| {
            matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, run_id }
            if message_id == resumed_id && run_id == resumed_run)
        },
    )
    .await;
    tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), before_final_pause)
        .await
        .expect("final continuation gate timeout")
        .expect("final continuation reached gate");
    let final_paused = reopened
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: goal.goal_id,
                expected_revision: resumed.revision,
                action: GoalLifecycleAction::Pause,
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("final pause")
        .goal;
    assert_eq!(final_paused.status, GoalStatus::Paused);
    let final_control = producer_projection(&reopened, session_id)
        .messages
        .iter()
        .rev()
        .find(|message| matches!(&message.producer_owner, ProducerOwner::GoalControl { .. }))
        .expect("final pause control")
        .message_id;
    release_final_pause.notify_one();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "paused by the user",
            scripted_text_body("final pause acknowledged"),
        ))
        .expect("final pause response");
    await_event(&reopened, session_id, "final pause consumed", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, .. }
            if message_id == final_control)
    })
    .await;
    wait_for_session_not_running(&reopened, session_id).await;

    let requests = captured.await.expect("restart continuation requests");
    assert_eq!(requests.len(), 4);
    let requests = requests
        .iter()
        .map(|request| {
            scripted_effective_last_message(request.as_bytes())
                .expect("model request message")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert!(requests[0].contains("Goal started. Pursue the new root objective below."));
    assert!(requests[1].contains("paused by the user"));
    assert!(requests[2].contains("Continue the root goal."));
    assert!(!requests[2].contains("Goal started. Pursue the new root objective below."));
    assert!(requests[3].contains("paused by the user"));
    reopened.shutdown().await;
}

#[tokio::test]
async fn discarded_initial_reminder_and_fresh_replacement_are_both_started() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection)
        .expect("discarded start session");
    let session_id = session.session_id;
    let blocker_authority = producer_authority();
    let blocker = fixture
        .engine
        .register_producer(session_id, blocker_authority.clone())
        .await
        .expect("discarded start blocker");
    let goal = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Bootstrap after an invalidated admission".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("discarded start goal")
        .goal;
    let (before_claim, release_claim) = fixture.engine.install_prompt_before_claim_hook_for_test();
    fixture
        .engine
        .unregister_producer(session_id, blocker_authority, blocker)
        .await
        .expect("release discard candidate");
    let stale_admission = await_event(
        &fixture.engine,
        session_id,
        "initial reminder admission",
        |event| {
            matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id }
            if producer_projection(&fixture.engine, session_id).messages.iter().any(|message| {
                message.message_id == message_id
                    && message.reminder == Some(cookie_agent_protocol::GoalReminderIdentity {
                        goal_id: goal.goal_id,
                        revision: goal.revision,
                        kind: GoalReminderKind::Started,
                    })
            }))
        },
    )
    .await;
    let EventPayload::ProducerMessageAdmitted {
        message_id: stale_id,
    } = stale_admission.payload
    else {
        unreachable!()
    };
    tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), before_claim)
        .await
        .expect("initial reminder gate timeout")
        .expect("initial reminder reached gate");
    let stale_record = producer_projection(&fixture.engine, session_id)
        .messages
        .into_iter()
        .find(|message| message.message_id == stale_id)
        .expect("initial reminder record");
    assert!(stale_record.claims.is_empty());
    assert!(
        stale_record
            .body
            .contains("Establish the checklist with goal_update, then pursue its unfinished work.")
    );

    let revised = fixture
        .engine
        .goal_update(
            session_id,
            cookie_agent_protocol::GoalUpdateParams { items: Vec::new() },
        )
        .await
        .expect("revise before claim")
        .goal;
    let stale_record = producer_projection(&fixture.engine, session_id)
        .messages
        .into_iter()
        .find(|message| message.message_id == stale_id)
        .expect("discarded initial reminder");
    assert!(stale_record.discarded);
    assert!(!stale_record.consumed);
    let boundary_authority = producer_authority();
    let boundary_registration = fixture
        .engine
        .register_producer(session_id, boundary_authority.clone())
        .await
        .expect("revision boundary producer");
    let boundary_id = fixture
        .engine
        .send_producer_message(
            session_id,
            boundary_authority.clone(),
            boundary_registration,
            ProducerDeliveryMode::Steer,
            producer_key("discarded-reminder-revision-boundary"),
            "finish the run that lost its stale reminder".into(),
        )
        .await
        .expect("revision boundary input");
    fixture
        .engine
        .unregister_producer(session_id, boundary_authority, boundary_registration)
        .await
        .expect("close revision boundary producer");
    release_claim.notify_one();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "finish the run that lost its stale reminder",
            scripted_text_body("revision boundary committed"),
        ))
        .expect("revision boundary response");
    await_event(
        &fixture.engine,
        session_id,
        "revision boundary consumed",
        |event| {
            matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, .. }
            if message_id == boundary_id)
        },
    )
    .await;

    let revised_admission = await_event(
        &fixture.engine,
        session_id,
        "revised started reminder",
        |event| {
            matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id }
            if message_id != stale_id
                && producer_projection(&fixture.engine, session_id).messages.iter().any(|message| {
                    message.message_id == message_id
                        && message.reminder == Some(cookie_agent_protocol::GoalReminderIdentity {
                            goal_id: goal.goal_id,
                            revision: revised.revision,
                            kind: GoalReminderKind::Started,
                        })
                }))
        },
    )
    .await;
    let revised_run = revised_admission.run_id.expect("revised started run");
    let EventPayload::ProducerMessageAdmitted {
        message_id: revised_id,
    } = revised_admission.payload
    else {
        unreachable!()
    };
    await_event(
        &fixture.engine,
        session_id,
        "revised reminder request",
        |event| {
            event.run_id == Some(revised_run)
                && matches!(event.payload, EventPayload::ModelRequestPrepared { .. })
        },
    )
    .await;
    let revised_record = producer_projection(&fixture.engine, session_id)
        .messages
        .into_iter()
        .find(|message| message.message_id == revised_id)
        .expect("revised reminder record");
    assert!(revised_record.body.contains("Goal started."));
    assert!(
        revised_record
            .body
            .contains("Establish the checklist with goal_update, then pursue its unfinished work.")
    );
    fixture
        .engine
        .goal_update(
            session_id,
            cookie_agent_protocol::GoalUpdateParams {
                items: vec![GoalItem {
                    description: "Finish the first goal".into(),
                    finished: true,
                }],
            },
        )
        .await
        .expect("complete first goal while reminder is claimed");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            &goal.objective,
            scripted_text_body("revised started reminder committed"),
        ))
        .expect("revised started response");
    await_event(
        &fixture.engine,
        session_id,
        "revised reminder consumed",
        |event| {
            matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, run_id }
            if message_id == revised_id && run_id == revised_run)
        },
    )
    .await;
    wait_for_session_not_running(&fixture.engine, session_id).await;

    let replacement_blocker_authority = producer_authority();
    let replacement_blocker = fixture
        .engine
        .register_producer(session_id, replacement_blocker_authority.clone())
        .await
        .expect("replacement blocker");
    let replacement = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Introduce a distinct replacement goal".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("replacement goal")
        .goal;
    assert_ne!(replacement.goal_id, goal.goal_id);
    fixture
        .engine
        .unregister_producer(
            session_id,
            replacement_blocker_authority,
            replacement_blocker,
        )
        .await
        .expect("release replacement reminder");
    let replacement_admission = await_event(
        &fixture.engine,
        session_id,
        "replacement started reminder",
        |event| {
            matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id }
            if producer_projection(&fixture.engine, session_id).messages.iter().any(|message| {
                message.message_id == message_id
                    && message.reminder == Some(cookie_agent_protocol::GoalReminderIdentity {
                        goal_id: replacement.goal_id,
                        revision: replacement.revision,
                        kind: GoalReminderKind::Started,
                    })
            }))
        },
    )
    .await;
    let replacement_run = replacement_admission
        .run_id
        .expect("replacement started run");
    let EventPayload::ProducerMessageAdmitted {
        message_id: replacement_id,
    } = replacement_admission.payload
    else {
        unreachable!()
    };
    await_event(
        &fixture.engine,
        session_id,
        "replacement reminder request",
        |event| {
            event.run_id == Some(replacement_run)
                && matches!(event.payload, EventPayload::ModelRequestPrepared { .. })
        },
    )
    .await;
    fixture
        .engine
        .goal_update(
            session_id,
            cookie_agent_protocol::GoalUpdateParams {
                items: vec![GoalItem {
                    description: "Finish the replacement goal".into(),
                    finished: true,
                }],
            },
        )
        .await
        .expect("complete replacement while reminder is claimed");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            &replacement.objective,
            scripted_text_body("replacement started reminder committed"),
        ))
        .expect("replacement started response");
    await_event(
        &fixture.engine,
        session_id,
        "replacement reminder consumed",
        |event| {
            matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, run_id }
                if message_id == replacement_id && run_id == replacement_run)
        },
    )
    .await;
    wait_for_session_not_running(&fixture.engine, session_id).await;

    let requests = captured.await.expect("started reminder requests");
    assert_eq!(requests.len(), 3);
    let requests = requests
        .iter()
        .map(|request| {
            scripted_effective_last_message(request.as_bytes())
                .expect("model request message")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert!(!requests[0].contains("Goal started. Pursue the new root objective below."));
    for request in &requests[1..] {
        assert!(request.contains("Goal started. Pursue the new root objective below."));
        assert!(request.contains(
            "Establish the checklist with goal_update, then pursue its unfinished work."
        ));
        assert!(!request.contains("Continue the root goal."));
    }
    assert!(requests[1].contains(&goal.goal_id.to_string()));
    assert!(requests[2].contains(&replacement.goal_id.to_string()));
    fixture.engine.shutdown().await;
}
