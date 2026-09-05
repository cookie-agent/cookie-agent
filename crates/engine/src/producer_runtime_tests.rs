use super::*;

use cookie_agent_protocol::{
    EventOrigin, GoalItem, GoalLifecycleAction, GoalStatus, ProducerDeliveryMode,
    ProducerIdempotencyKey, ProducerOwner, SessionGoalGetParams, SessionGoalLifecycleParams,
    SessionGoalSetParams, SessionProducersParams,
};

use crate::runtime::producers::ProducerAuthority;

fn producer_authority() -> ProducerAuthority {
    ProducerAuthority {
        owner: ProducerOwner::Delegation {
            invocation_id: InvocationId::new_v7(),
        },
        connection_epoch: None,
    }
}

fn producer_key(value: &str) -> ProducerIdempotencyKey {
    ProducerIdempotencyKey::new(value).expect("producer idempotency key")
}

fn client_origin() -> EventOrigin {
    EventOrigin::new("client:producer-runtime-test").expect("event origin")
}

fn producer_events(
    engine: &Engine,
    session_id: SessionId,
) -> Vec<cookie_agent_protocol::StoredEvent> {
    engine
        .inner
        .store
        .get(session_id)
        .expect("producer session projection")
        .log
        .events()
}

fn producer_projection(
    engine: &Engine,
    session_id: SessionId,
) -> crate::goal_projection::GoalProducerProjection {
    crate::goal_projection::GoalProducerProjection::from_events(&producer_events(
        engine, session_id,
    ))
}

struct GoalVisibilityProvider;

#[async_trait]
impl ToolProvider for GoalVisibilityProvider {
    fn provider_id(&self) -> &'static str {
        "test.goal_visibility"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok([
            (
                "goal_get",
                "read",
                "Read the current goal",
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {}
                }),
            ),
            (
                "goal_update",
                "write",
                "Update the current goal",
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["items"],
                    "properties": {
                        "items": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["description", "finished"],
                                "properties": {
                                    "description": { "type": "string" },
                                    "finished": { "type": "boolean" }
                                }
                            }
                        }
                    }
                }),
            ),
        ]
        .into_iter()
        .map(
            |(name, permission_name, description, parameters)| ToolSpec {
                name: name.into(),
                permission_name: permission_name.into(),
                description: description.into(),
                parameters,
                concurrency: Default::default(),
                result_truncation: Default::default(),
            },
        )
        .collect())
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "goal_get" => Ok("read"),
            "goal_update" => Ok("write"),
            _ => Err(ToolError::execution("unexpected goal visibility tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        Ok((Self::get_permission_name(name)?, Some("goal:test".into())))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        self.get_permission_resource(name, arguments)?
            .1
            .ok_or_else(|| ToolError::execution("missing goal resource"))
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        _call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Err(ToolError::execution(
            "goal visibility test tools must not execute",
        ))
    }
}

fn request_tool_names(request: &str) -> Vec<String> {
    request_body(request)["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
        .collect()
}

fn request_tool_parameters(request: &str, name: &str) -> serde_json::Value {
    request_body(request)["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|tool| tool["function"]["name"].as_str() == Some(name))
        .map(|tool| tool["function"]["parameters"].clone())
        .expect("request tool parameters")
}

fn assert_no_notification_run_abort(engine: &Engine, session_id: SessionId) {
    assert!(!producer_events(engine, session_id).iter().any(|event| {
        matches!(
            event.payload,
            EventPayload::RunCancelled { .. } | EventPayload::RunInterrupted { .. }
        )
    }));
    assert!(!producer_events(engine, session_id).iter().any(|event| {
        matches!(
            event.payload,
            EventPayload::DelegationFinished {
                status: SessionStatus::Cancelled | SessionStatus::Interrupted,
                ..
            }
        )
    }));
}

pub(super) async fn cancelled_request_boundary_server() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("cancel boundary listener");
    let address = listener.local_addr().expect("cancel boundary address");
    let (first_seen_tx, first_seen_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let task_release = Arc::clone(&release);
    let captured = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt as _;

        let (mut first, first_request) =
            accept_scripted_planned_request(&listener, "cancel boundary first request").await;
        let mut requests = vec![String::from_utf8(first_request).expect("first request UTF-8")];
        let _ = first_seen_tx.send(());
        task_release.notified().await;
        let body = scripted_text_body("late cancelled-run response");
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = first.write_all(response.as_bytes()).await;

        let (mut second, second_request) =
            accept_scripted_planned_request(&listener, "cancel boundary replacement request").await;
        requests.push(String::from_utf8(second_request).expect("second request UTF-8"));
        write_scripted_sse(
            &mut second,
            &scripted_text_body("replacement run consumed pause"),
        )
        .await;
        spawn_scripted_auxiliary_tail(listener);
        requests
    });
    (
        format!("http://{address}/v1"),
        first_seen_rx,
        release,
        captured,
    )
}

async fn hold_compaction(
    engine: &Engine,
    session_id: SessionId,
) -> (
    tokio::sync::oneshot::Receiver<
        Result<cookie_agent_protocol::SessionCompactResult, EngineError>,
    >,
    Arc<tokio::sync::Notify>,
) {
    let (reached, release) = engine.install_compaction_execution_hook_for_test();
    let completion = engine
        .enqueue_compact_without_residency_for_test(session_id)
        .await
        .expect("enqueue held compaction");
    tokio::time::timeout(test_timeout(2), reached)
        .await
        .expect("held compaction hook timeout")
        .expect("held compaction reached hook");
    (completion, release)
}

async fn release_compaction(
    completion: tokio::sync::oneshot::Receiver<
        Result<cookie_agent_protocol::SessionCompactResult, EngineError>,
    >,
    release: Arc<tokio::sync::Notify>,
) {
    release.notify_waiters();
    let _ = tokio::time::timeout(test_timeout(2), completion)
        .await
        .expect("held compaction completion timeout");
}

#[tokio::test]
async fn goal_state_machine_is_durable_stale_safe_and_quiescent_when_terminal() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection)
        .expect("goal session");
    let session_id = session.session_id;
    let (compaction, compaction_release) = hold_compaction(&fixture.engine, session_id).await;

    assert_eq!(
        fixture
            .engine
            .get_session_goal(SessionGoalGetParams { session_id })
            .await
            .expect("empty goal")
            .goal,
        None
    );
    assert!(
        fixture
            .engine
            .set_session_goal(
                SessionGoalSetParams {
                    session_id,
                    objective: "   ".into(),
                    selection: None,
                },
                client_origin(),
            )
            .await
            .expect_err("blank goal must fail")
            .to_string()
            .contains("objective must not be blank")
    );

    let activated = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Ship phase two".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("activate goal")
        .goal;
    assert_eq!(activated.status, GoalStatus::Active);
    assert!(activated.items.is_empty());
    assert_eq!(activated.revision, 0);

    let registrations = fixture
        .engine
        .session_producers(SessionProducersParams { session_id })
        .await
        .expect("producer registrations")
        .producers;
    assert_eq!(registrations.len(), 1);
    assert!(registrations.iter().any(|entry| {
        entry.producer_owner
            == ProducerOwner::Goal {
                goal_id: activated.goal_id,
            }
    }));
    assert!(
        !producer_events(&fixture.engine, session_id)
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ProducerMessageAccepted { .. }))
    );

    let bootstrapped = fixture
        .engine
        .goal_update(
            session_id,
            cookie_agent_protocol::GoalUpdateParams { items: Vec::new() },
        )
        .await
        .expect("empty checklist bootstrap")
        .goal;
    assert_eq!(bootstrapped.status, GoalStatus::Active);
    assert_eq!(bootstrapped.revision, 1);
    assert!(
        fixture
            .engine
            .change_session_goal_lifecycle(
                SessionGoalLifecycleParams {
                    session_id,
                    goal_id: activated.goal_id,
                    expected_revision: 0,
                    action: GoalLifecycleAction::Pause,
                    selection: None,
                },
                client_origin(),
            )
            .await
            .expect_err("stale lifecycle change")
            .to_string()
            .contains("stale goal")
    );

    let paused = fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: activated.goal_id,
                expected_revision: 1,
                action: GoalLifecycleAction::Pause,
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("pause goal")
        .goal;
    assert_eq!(paused.status, GoalStatus::Paused);
    assert_eq!(paused.revision, 2);
    assert!(
        fixture
            .engine
            .session_producers(SessionProducersParams { session_id })
            .await
            .expect("paused registrations")
            .producers
            .is_empty()
    );

    let resumed = fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: activated.goal_id,
                expected_revision: 2,
                action: GoalLifecycleAction::Resume,
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("resume goal")
        .goal;
    assert_eq!(resumed.status, GoalStatus::Active);
    let paused_again = fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: activated.goal_id,
                expected_revision: resumed.revision,
                action: GoalLifecycleAction::Pause,
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("pause resumed goal")
        .goal;

    let completed = fixture
        .engine
        .goal_update(
            session_id,
            cookie_agent_protocol::GoalUpdateParams {
                items: vec![GoalItem {
                    description: "Integration tests pass".into(),
                    finished: true,
                }],
            },
        )
        .await
        .expect("finish paused goal")
        .goal;
    assert_eq!(completed.status, GoalStatus::Completed);
    assert_eq!(completed.revision, paused_again.revision + 2);
    assert!(
        fixture
            .engine
            .change_session_goal_lifecycle(
                SessionGoalLifecycleParams {
                    session_id,
                    goal_id: activated.goal_id,
                    expected_revision: completed.revision,
                    action: GoalLifecycleAction::Resume,
                    selection: None,
                },
                client_origin(),
            )
            .await
            .expect_err("terminal lifecycle change")
            .to_string()
            .contains("terminal goals cannot be changed")
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_ne!(
        fixture
            .engine
            .get_session(session_id)
            .expect("completed quiet session")
            .status,
        SessionStatus::Running
    );
    assert!(
        !producer_events(&fixture.engine, session_id)
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ProducerMessageAccepted { .. }))
    );

    let replacement = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Verify replacement".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("replace terminal goal")
        .goal;
    assert_ne!(replacement.goal_id, completed.goal_id);
    assert_eq!(replacement.revision, 0);
    let cancelled = fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: replacement.goal_id,
                expected_revision: 0,
                action: GoalLifecycleAction::Cancel,
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("cancel replacement")
        .goal;
    release_compaction(compaction, compaction_release).await;

    fixture.engine.shutdown().await;
    let reopened = reopen_engine(&fixture);
    let replayed = reopened
        .get_session_goal(SessionGoalGetParams { session_id })
        .await
        .expect("replayed goal")
        .goal
        .expect("durable goal");
    assert_eq!(replayed, cancelled);
    assert!(
        reopened
            .session_producers(SessionProducersParams { session_id })
            .await
            .expect("reopened registrations")
            .producers
            .is_empty()
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn explicit_goal_selection_is_durable_and_changes_with_lifecycle_atomically() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("explicit-selection goal session");
    let session_id = session.session_id;
    let (compaction, compaction_release) = hold_compaction(&fixture.engine, session_id).await;

    let goal = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Keep the selected goal model durable".into(),
                selection: Some(selection.clone()),
            },
            client_origin(),
        )
        .await
        .expect("explicit-selection goal")
        .goal;
    let projection = producer_projection(&fixture.engine, session_id);
    assert_eq!(projection.selection.as_ref(), Some(&selection));
    assert!(
        producer_events(&fixture.engine, session_id)
            .iter()
            .any(|event| {
                matches!(
                    &event.payload,
                    EventPayload::GoalActivated {
                        goal_id,
                        selection: Some(stored),
                        ..
                    } if *goal_id == goal.goal_id && stored == &selection
                )
            })
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
            client_origin(),
        )
        .await
        .expect("pause preserves explicit selection")
        .goal;
    let projection = producer_projection(&fixture.engine, session_id);
    assert_eq!(projection.goal, Some(paused.clone()));
    assert_eq!(projection.selection.as_ref(), Some(&selection));
    assert!(
        producer_events(&fixture.engine, session_id)
            .iter()
            .any(|event| {
                matches!(
                    &event.payload,
                    EventPayload::GoalLifecycleChanged {
                        goal_id,
                        revision,
                        selection: None,
                        ..
                    } if *goal_id == paused.goal_id && *revision == paused.revision
                )
            })
    );

    release_compaction(compaction, compaction_release).await;
    fixture.engine.shutdown().await;
    let reopened = reopen_engine(&fixture);
    let replayed = producer_projection(&reopened, session_id);
    assert_eq!(replayed.goal, Some(paused));
    assert_eq!(replayed.selection, Some(selection));
    reopened.shutdown().await;
}

#[tokio::test]
async fn shared_view_goal_updates_apply_in_actor_order_and_allow_duplicate_descriptions() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection)
        .expect("shared-view goal session");
    let session_id = session.session_id;
    let (compaction, compaction_release) = hold_compaction(&fixture.engine, session_id).await;
    assert!(
        fixture
            .engine
            .goal_update(
                session_id,
                cookie_agent_protocol::GoalUpdateParams { items: Vec::new() },
            )
            .await
            .expect_err("update without a goal")
            .to_string()
            .contains("no goal is set")
    );
    let goal = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Apply serialized model updates".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("shared-view goal")
        .goal;

    let first_items = vec![
        GoalItem {
            description: "Inspect shared evidence".into(),
            finished: false,
        },
        GoalItem {
            description: "Inspect shared evidence".into(),
            finished: false,
        },
    ];
    let second_items = vec![GoalItem {
        description: "Use the later replacement".into(),
        finished: false,
    }];
    let first = cookie_agent_protocol::GoalUpdateParams {
        items: first_items.clone(),
    };
    let second = cookie_agent_protocol::GoalUpdateParams {
        items: second_items.clone(),
    };

    let first_result = fixture
        .engine
        .goal_update(session_id, first)
        .await
        .expect("first shared-view update")
        .goal;
    assert_eq!(first_result.revision, 1);
    assert_eq!(first_result.items, first_items);

    let second_result = fixture
        .engine
        .goal_update(session_id, second)
        .await
        .expect("second shared-view update")
        .goal;
    assert_eq!(second_result.revision, 2);
    assert_eq!(second_result.items, second_items);

    let update_built_for_goal_a = cookie_agent_protocol::GoalUpdateParams {
        items: vec![GoalItem {
            description: "Apply to whichever goal is current".into(),
            finished: false,
        }],
    };
    fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: goal.goal_id,
                expected_revision: second_result.revision,
                action: GoalLifecycleAction::Cancel,
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("cancel goal A");
    let goal_b = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Receive the previously built model update".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("activate goal B")
        .goal;
    let updated_goal_b = fixture
        .engine
        .goal_update(session_id, update_built_for_goal_a)
        .await
        .expect("old model update targets current goal B")
        .goal;
    assert_eq!(updated_goal_b.goal_id, goal_b.goal_id);
    assert_eq!(updated_goal_b.revision, 1);
    assert_eq!(
        updated_goal_b.items[0].description,
        "Apply to whichever goal is current"
    );

    assert!(
        fixture
            .engine
            .goal_update(
                session_id,
                cookie_agent_protocol::GoalUpdateParams {
                    items: vec![GoalItem {
                        description: "  ".into(),
                        finished: false,
                    }],
                },
            )
            .await
            .expect_err("blank item description")
            .to_string()
            .contains("items require nonblank descriptions")
    );
    assert_eq!(
        fixture
            .engine
            .goal_get(session_id)
            .await
            .expect("goal after rejected blank update")
            .goal,
        Some(updated_goal_b.clone())
    );
    fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: goal_b.goal_id,
                expected_revision: updated_goal_b.revision,
                action: GoalLifecycleAction::Cancel,
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("finish current-goal binding fixture");
    release_compaction(compaction, compaction_release).await;
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn completing_shared_view_update_rejects_the_following_update() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection)
        .expect("terminal shared-view goal session");
    let session_id = session.session_id;
    fixture
        .engine
        .register_producer(session_id, producer_authority())
        .await
        .expect("terminal shared-view wake blocker");
    fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Complete in actor order".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("terminal shared-view goal");
    let completing = cookie_agent_protocol::GoalUpdateParams {
        items: vec![GoalItem {
            description: "Finish first".into(),
            finished: true,
        }],
    };
    let following = cookie_agent_protocol::GoalUpdateParams {
        items: vec![GoalItem {
            description: "Replace after completion".into(),
            finished: false,
        }],
    };

    let completed = fixture
        .engine
        .goal_update(session_id, completing)
        .await
        .expect("completing shared-view update")
        .goal;
    assert_eq!(completed.status, GoalStatus::Completed);
    assert_eq!(completed.revision, 2);
    assert!(
        fixture
            .engine
            .goal_update(session_id, following)
            .await
            .expect_err("update after serialized completion")
            .to_string()
            .contains("terminal goals cannot be changed")
    );
    assert_eq!(
        fixture
            .engine
            .goal_get(session_id)
            .await
            .expect("terminal goal state")
            .goal,
        Some(completed)
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn active_steer_and_queue_preserve_authority_dedup_and_model_run_boundaries() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    let session_id = session.session_id;
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                session_id,
                client_run_id: ClientRunId::new("producer-active-run").expect("client run ID"),
                selection: selection.clone(),
                input: "initial producer request".into(),
            },
            client_origin(),
        )
        .await
        .expect("initial run")
        .run_id;
    await_event(
        &fixture.engine,
        session_id,
        "initial model attempt",
        |event| {
            event.run_id == Some(run)
                && matches!(event.payload, EventPayload::ModelAttemptStarted { .. })
        },
    )
    .await;

    let authority = producer_authority();
    let first_registration = fixture
        .engine
        .register_producer(session_id, authority.clone())
        .await
        .expect("first registration");
    let second_registration = fixture
        .engine
        .register_producer(session_id, authority.clone())
        .await
        .expect("second registration");
    let steer_key = producer_key("stable-owner-key");
    let steer_id = fixture
        .engine
        .send_producer_message(
            session_id,
            authority.clone(),
            first_registration,
            ProducerDeliveryMode::Steer,
            steer_key.clone(),
            "steer into active run".into(),
        )
        .await
        .expect("steer send");
    assert_eq!(
        fixture
            .engine
            .send_producer_message(
                session_id,
                authority.clone(),
                second_registration,
                ProducerDeliveryMode::Steer,
                steer_key.clone(),
                "steer into active run".into(),
            )
            .await
            .expect("stable owner dedup"),
        steer_id
    );
    for (mode, body) in [
        (ProducerDeliveryMode::Queue, "steer into active run"),
        (ProducerDeliveryMode::Steer, "different payload"),
    ] {
        assert!(
            fixture
                .engine
                .send_producer_message(
                    session_id,
                    authority.clone(),
                    second_registration,
                    mode,
                    steer_key.clone(),
                    body.into(),
                )
                .await
                .expect_err("conflicting dedup send")
                .to_string()
                .contains("idempotency key already accepted with different payload")
        );
    }

    let foreign = producer_authority();
    assert!(
        fixture
            .engine
            .send_producer_message(
                session_id,
                foreign,
                first_registration,
                ProducerDeliveryMode::Steer,
                producer_key("foreign-owner"),
                "foreign".into(),
            )
            .await
            .expect_err("wrong owner")
            .to_string()
            .contains("closed, foreign, or wrong-session registration")
    );
    let other = fixture
        .engine
        .create_session(selection)
        .expect("other session");
    assert!(
        fixture
            .engine
            .send_producer_message(
                other.session_id,
                authority.clone(),
                first_registration,
                ProducerDeliveryMode::Steer,
                producer_key("wrong-session"),
                "wrong session".into(),
            )
            .await
            .expect_err("wrong session")
            .to_string()
            .contains("closed, foreign, or wrong-session registration")
    );

    let queue_id = fixture
        .engine
        .send_producer_message(
            session_id,
            authority.clone(),
            second_registration,
            ProducerDeliveryMode::Queue,
            producer_key("queued-follow-up"),
            "queue for subsequent run".into(),
        )
        .await
        .expect("queue send");
    fixture
        .engine
        .unregister_producer(session_id, authority.clone(), first_registration)
        .await
        .expect("unregister first producer");
    fixture
        .engine
        .unregister_producer(session_id, authority.clone(), second_registration)
        .await
        .expect("unregister with accepted messages pending");
    assert!(
        fixture
            .engine
            .send_producer_message(
                session_id,
                authority,
                second_registration,
                ProducerDeliveryMode::Queue,
                producer_key("closed-registration"),
                "closed".into(),
            )
            .await
            .expect_err("closed registration")
            .to_string()
            .contains("closed, foreign, or wrong-session registration")
    );

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "initial producer request",
            scripted_text_body("initial response committed"),
        ))
        .expect("initial response");
    let steer_admission = await_event(&fixture.engine, session_id, "steer admission", |event| {
        event.run_id == Some(run)
            && matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id } if message_id == steer_id)
    })
    .await;
    let before_steer_response = producer_projection(&fixture.engine, session_id);
    assert!(
        !before_steer_response
            .messages
            .iter()
            .find(|message| message.message_id == steer_id)
            .expect("steer record")
            .consumed
    );

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "steer into active run",
            scripted_text_body("steered response committed"),
        ))
        .expect("steer response");
    await_event(&fixture.engine, session_id, "steer consumption", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, run_id } if message_id == steer_id && run_id == run)
    })
    .await;

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "queue for subsequent run",
            scripted_text_body("queued response committed"),
        ))
        .expect("queue response");
    let queue_consumption = await_event(&fixture.engine, session_id, "queue consumption", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, .. } if message_id == queue_id)
    })
    .await;
    assert_ne!(queue_consumption.run_id, Some(run));
    assert_ne!(steer_admission.run_id, queue_consumption.run_id);
    assert!(
        !producer_events(&fixture.engine, session_id)
            .iter()
            .any(|event| {
                event.run_id.is_none()
                    && matches!(event.payload, EventPayload::UserInputAdmitted { .. })
            })
    );

    let requests = captured.await.expect("producer model requests");
    assert_eq!(requests.len(), 3);
    assert!(requests[1].contains("steer into active run"));
    assert!(!requests[1].contains("queue for subsequent run"));
    assert!(requests[2].contains("queue for subsequent run"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn idle_send_auto_runs_and_recovery_restores_missing_consumption_marker() {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection)
        .expect("idle session");
    let session_id = session.session_id;
    let authority = producer_authority();
    let registration = fixture
        .engine
        .register_producer(session_id, authority.clone())
        .await
        .expect("idle producer registration");
    let message_id = fixture
        .engine
        .send_producer_message(
            session_id,
            authority.clone(),
            registration,
            ProducerDeliveryMode::Queue,
            producer_key("idle-auto-run"),
            "idle producer model input".into(),
        )
        .await
        .expect("idle producer send");
    fixture
        .engine
        .unregister_producer(session_id, authority, registration)
        .await
        .expect("unregister idle producer");

    let admission = await_event(&fixture.engine, session_id, "automatic producer admission", |event| {
        matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id: admitted } if admitted == message_id)
    })
    .await;
    let auto_run = admission.run_id.expect("automatic producer run ID");
    let pending = producer_projection(&fixture.engine, session_id);
    assert!(!pending.messages[0].consumed);

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "idle producer model input",
            scripted_text_body("idle producer complete"),
        ))
        .expect("idle response");
    await_event(&fixture.engine, session_id, "idle producer consumed", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id: consumed, run_id } if consumed == message_id && run_id == auto_run)
    })
    .await;
    wait_for_session_not_running(&fixture.engine, session_id).await;
    let requests = captured.await.expect("idle model request");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("idle producer model input"));

    let event_path = fixture
        .engine
        .inner
        .store
        .session_dir(session_id)
        .join("events.jsonl");
    fixture.engine.shutdown().await;
    let mut events = fs::read_to_string(&event_path)
        .expect("producer events")
        .lines()
        .map(|line| {
            serde_json::from_str::<cookie_agent_protocol::StoredEvent>(line).expect("event")
        })
        .collect::<Vec<_>>();
    let removed_seq = events
        .iter()
        .find(|event| {
            matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id: consumed, .. } if consumed == message_id)
        })
        .expect("consumption marker")
        .seq;
    events.retain(|event| {
        !matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id: consumed, .. } if consumed == message_id)
    });
    for event in &mut events {
        if event.seq > removed_seq {
            event.seq -= 1;
        }
        match &mut event.payload {
            EventPayload::RunStarted {
                input_through_seq, ..
            }
            | EventPayload::ModelTurnCommitted {
                input_through_seq, ..
            } if *input_through_seq > removed_seq => *input_through_seq -= 1,
            EventPayload::UserInputApplied { user_input_seq } if *user_input_seq > removed_seq => {
                *user_input_seq -= 1;
            }
            _ => {}
        }
    }
    let rewritten = events
        .iter()
        .map(|event| serde_json::to_string(event).expect("serialize event"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    write_private_test_file(&event_path, rewritten);

    drop(fixture.engine);
    let reopened = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    reopened
        .resume(session_id)
        .await
        .expect("adopt producer session after restart");
    let recovered = await_event(&reopened, session_id, "recovered consumption marker", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id: consumed, run_id } if consumed == message_id && run_id == auto_run)
    })
    .await;
    assert_eq!(
        recovered.origin.as_ref().map(EventOrigin::as_str),
        Some("engine:producer")
    );
    let projection = producer_projection(&reopened, session_id);
    assert!(projection.messages[0].consumed);
    assert!(projection.messages[0].consumption_recorded);
    assert!(
        reopened
            .session_producers(SessionProducersParams { session_id })
            .await
            .expect("reopened producer registrations")
            .producers
            .is_empty()
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn goal_reminders_include_full_state_repeat_with_fresh_ids_and_pause_quiesces() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection)
        .expect("goal reminder session");
    let session_id = session.session_id;
    let blocker_authority = producer_authority();
    let blocker = fixture
        .engine
        .register_producer(session_id, blocker_authority.clone())
        .await
        .expect("goal reminder blocker");
    fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Deliver the complete release candidate".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("set reminder goal");
    let goal = fixture
        .engine
        .goal_update(
            session_id,
            cookie_agent_protocol::GoalUpdateParams {
                items: vec![
                    GoalItem {
                        description: "Build release artifacts".into(),
                        finished: true,
                    },
                    GoalItem {
                        description: "Verify production smoke tests".into(),
                        finished: false,
                    },
                ],
            },
        )
        .await
        .expect("set reminder checklist")
        .goal;
    fixture
        .engine
        .unregister_producer(session_id, blocker_authority, blocker)
        .await
        .expect("release goal reminder");

    let first_admission = await_event(&fixture.engine, session_id, "first goal reminder", |event| {
        matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id } if producer_projection(&fixture.engine, session_id).messages.iter().any(|message| message.message_id == message_id && message.reminder.is_some()))
    })
    .await;
    let first_run = first_admission.run_id.expect("first reminder run");
    let EventPayload::ProducerMessageAdmitted {
        message_id: first_id,
    } = first_admission.payload
    else {
        unreachable!()
    };
    let first_record = producer_projection(&fixture.engine, session_id)
        .messages
        .into_iter()
        .find(|message| message.message_id == first_id)
        .expect("first reminder record");
    assert_eq!(
        first_record.reminder,
        Some(cookie_agent_protocol::GoalReminderIdentity {
            goal_id: goal.goal_id,
            revision: goal.revision,
        })
    );
    let reminder_json: serde_json::Value = serde_json::from_str(
        first_record
            .body
            .lines()
            .last()
            .expect("serialized goal reminder"),
    )
    .expect("goal reminder JSON");
    assert_eq!(reminder_json, serde_json::to_value(&goal).unwrap());
    assert!(first_record.body.contains(&goal.objective));
    assert!(first_record.body.contains("Build release artifacts"));
    assert!(first_record.body.contains("Verify production smoke tests"));
    assert!(first_record.body.contains(&goal.goal_id.to_string()));
    assert!(
        first_record
            .body
            .contains(&format!("\"revision\":{}", goal.revision))
    );

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "Deliver the complete release candidate",
            scripted_text_body("first unchanged goal turn"),
        ))
        .expect("first goal response");
    await_event(&fixture.engine, session_id, "first goal reminder consumed", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, run_id } if message_id == first_id && run_id == first_run)
    })
    .await;
    let second_admission = await_event(
        &fixture.engine,
        session_id,
        "fresh unchanged-revision reminder",
        |event| {
            matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id } if message_id != first_id && producer_projection(&fixture.engine, session_id).messages.iter().any(|message| message.message_id == message_id && message.reminder == first_record.reminder))
        },
    )
    .await;
    let second_run = second_admission.run_id.expect("second reminder run");
    let EventPayload::ProducerMessageAdmitted {
        message_id: second_id,
    } = second_admission.payload
    else {
        unreachable!()
    };
    assert_ne!(second_id, first_id);
    assert_ne!(second_run, first_run);
    assert_eq!(
        producer_projection(&fixture.engine, session_id)
            .messages
            .iter()
            .find(|message| message.message_id == second_id)
            .expect("second reminder record")
            .body,
        first_record.body
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
            client_origin(),
        )
        .await
        .expect("pause in-flight continuation")
        .goal;
    assert_eq!(paused.status, GoalStatus::Paused);
    assert_eq!(
        fixture
            .engine
            .get_session(session_id)
            .expect("in-flight reminder session")
            .status,
        SessionStatus::Running
    );
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "Deliver the complete release candidate",
            scripted_text_body("paused in-flight goal turn completed"),
        ))
        .expect("second goal response");
    await_event(&fixture.engine, session_id, "second goal reminder consumed", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, run_id } if message_id == second_id && run_id == second_run)
    })
    .await;
    let control = producer_projection(&fixture.engine, session_id)
        .messages
        .into_iter()
        .find(|message| {
            message.producer_owner
                == ProducerOwner::GoalControl {
                    goal_id: goal.goal_id,
                }
        })
        .expect("accepted pause control");
    assert_eq!(control.mode, ProducerDeliveryMode::Steer);
    assert_eq!(control.reminder, None);
    assert!(control.body.contains("paused by the user"));
    assert!(control.body.contains(&format!("\"{}\"", goal.objective)));
    assert!(control.body.contains("Stop pursuing it autonomously"));
    assert!(control.body.contains("Wrap up current work"));
    assert!(control.body.contains("report status"));
    assert!(control.body.contains("follow new user directions"));
    let accepted = producer_events(&fixture.engine, session_id)
        .into_iter()
        .find(|event| {
            matches!(event.payload, EventPayload::ProducerMessageAccepted { message_id, .. } if message_id == control.message_id)
        })
        .expect("engine-authored pause acceptance");
    assert_eq!(accepted.run_id, None);
    assert_eq!(
        accepted.origin.as_ref().map(EventOrigin::as_str),
        Some("engine:producer")
    );
    let control_admission = await_event(
        &fixture.engine,
        session_id,
        "pause control admitted",
        |event| {
            event.run_id == Some(second_run)
                && matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id } if message_id == control.message_id)
        },
    )
    .await;
    assert_eq!(control_admission.run_id, Some(second_run));
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "paused by the user",
            scripted_text_body("pause acknowledged at safe request boundary"),
        ))
        .expect("pause control response");
    await_event(&fixture.engine, session_id, "pause control consumed", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, run_id } if message_id == control.message_id && run_id == second_run)
    })
    .await;
    wait_for_session_not_running(&fixture.engine, session_id).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        producer_projection(&fixture.engine, session_id)
            .messages
            .iter()
            .filter(|message| message.reminder.is_some())
            .count(),
        2
    );
    let controls = producer_projection(&fixture.engine, session_id)
        .messages
        .into_iter()
        .filter(|message| {
            message.producer_owner
                == ProducerOwner::GoalControl {
                    goal_id: goal.goal_id,
                }
        })
        .collect::<Vec<_>>();
    assert_eq!(controls.len(), 1);
    assert!(controls[0].consumed);
    assert!(controls[0].consumption_recorded);
    assert!(!controls[0].discarded);
    assert_eq!(
        producer_events(&fixture.engine, session_id)
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::RunStarted { .. }))
            .count(),
        2
    );
    assert!(
        !producer_events(&fixture.engine, session_id)
            .iter()
            .any(|event| {
                event.run_id.is_none()
                    && matches!(event.payload, EventPayload::UserInputAdmitted { .. })
            })
    );
    let requests = captured.await.expect("goal reminder requests");
    assert_eq!(requests.len(), 3);
    for request in &requests[..2] {
        assert!(request.contains(&goal.objective));
        assert!(request.contains("Build release artifacts"));
        assert!(request.contains("Verify production smoke tests"));
        assert!(request.contains(&goal.goal_id.to_string()));
    }
    assert!(!requests[1].contains("paused by the user"));
    assert!(requests[2].contains("paused by the user"));
    assert_no_notification_run_abort(&fixture.engine, session_id);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn active_goal_cancel_notifies_once_and_preserves_other_producer_work() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("active cancel session");
    let session_id = session.session_id;
    let authority = producer_authority();
    let registration = fixture
        .engine
        .register_producer(session_id, authority.clone())
        .await
        .expect("ordinary producer registration");
    let goal = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Publish the signed candidate".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("active cancel goal")
        .goal;
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                session_id,
                client_run_id: ClientRunId::new("active-goal-cancel").expect("cancel run ID"),
                selection,
                input: "begin signed candidate work".into(),
            },
            client_origin(),
        )
        .await
        .expect("start active cancel run")
        .run_id;
    await_event(
        &fixture.engine,
        session_id,
        "cancel run model attempt",
        |event| {
            event.run_id == Some(run)
                && matches!(event.payload, EventPayload::ModelAttemptStarted { .. })
        },
    )
    .await;
    let queued = fixture
        .engine
        .send_producer_message(
            session_id,
            authority.clone(),
            registration,
            ProducerDeliveryMode::Queue,
            producer_key("work-after-cancel-notice"),
            "ordinary queued producer work".into(),
        )
        .await
        .expect("queue ordinary producer work");
    let cancelled = fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: goal.goal_id,
                expected_revision: goal.revision,
                action: GoalLifecycleAction::Cancel,
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("cancel active goal")
        .goal;
    assert_eq!(cancelled.status, GoalStatus::Cancelled);

    let projection = producer_projection(&fixture.engine, session_id);
    let control = projection
        .messages
        .iter()
        .find(|message| {
            message.producer_owner
                == ProducerOwner::GoalControl {
                    goal_id: goal.goal_id,
                }
        })
        .expect("accepted cancel control")
        .clone();
    assert_eq!(control.mode, ProducerDeliveryMode::Steer);
    assert_eq!(control.reminder, None);
    assert!(control.body.contains("cancelled by the user"));
    assert!(control.body.contains(&format!("\"{}\"", goal.objective)));
    assert!(control.body.contains("Stop pursuing this cancelled goal"));
    assert!(control.body.contains("do not call goal_update"));
    assert!(control.body.contains("Follow new user directions"));
    let accepted = producer_events(&fixture.engine, session_id)
        .into_iter()
        .find(|event| {
            matches!(event.payload, EventPayload::ProducerMessageAccepted { message_id, .. } if message_id == control.message_id)
        })
        .expect("engine-authored cancel acceptance");
    assert_eq!(accepted.run_id, None);
    assert_eq!(
        accepted.origin.as_ref().map(EventOrigin::as_str),
        Some("engine:producer")
    );
    let queued_record = projection
        .messages
        .iter()
        .find(|message| message.message_id == queued)
        .expect("ordinary queued producer record");
    assert!(!queued_record.consumed);
    assert!(!queued_record.discarded);
    let registrations = fixture
        .engine
        .session_producers(SessionProducersParams { session_id })
        .await
        .expect("registrations after cancel")
        .producers;
    assert!(
        registrations
            .iter()
            .any(|entry| entry.producer_id == registration)
    );
    assert!(!registrations.iter().any(|entry| matches!(
        &entry.producer_owner,
        ProducerOwner::Goal { .. } | ProducerOwner::GoalControl { .. }
    )));

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "begin signed candidate work",
            scripted_text_body("old in-flight response completed"),
        ))
        .expect("old cancel response");
    let admission = await_event(&fixture.engine, session_id, "cancel control admission", |event| {
        matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id } if message_id == control.message_id)
    })
    .await;
    assert_eq!(admission.run_id, Some(run));
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "cancelled by the user",
            scripted_text_body("cancel control acknowledged"),
        ))
        .expect("cancel control response");
    await_event(&fixture.engine, session_id, "cancel control consumed", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, run_id } if message_id == control.message_id && run_id == run)
    })
    .await;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "ordinary queued producer work",
            scripted_text_body("ordinary queue completed"),
        ))
        .expect("ordinary queued response");
    let queue_consumption = await_event(
        &fixture.engine,
        session_id,
        "ordinary queued producer consumed",
        |event| {
            matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, .. } if message_id == queued)
        },
    )
    .await;
    assert_ne!(queue_consumption.run_id, Some(run));
    wait_for_session_not_running(&fixture.engine, session_id).await;

    let controls = producer_projection(&fixture.engine, session_id)
        .messages
        .into_iter()
        .filter(|message| {
            message.producer_owner
                == ProducerOwner::GoalControl {
                    goal_id: goal.goal_id,
                }
        })
        .collect::<Vec<_>>();
    assert_eq!(controls.len(), 1);
    assert!(controls[0].consumed);
    assert!(controls[0].consumption_recorded);
    assert!(!controls[0].discarded);
    let requests = captured.await.expect("active cancel requests");
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].contains("cancelled by the user"));
    assert!(requests[1].contains("cancelled by the user"));
    assert!(!requests[1].contains("ordinary queued producer work"));
    assert!(requests[2].contains("ordinary queued producer work"));
    assert_no_notification_run_abort(&fixture.engine, session_id);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn idle_goal_pause_and_cancel_notify_live_producer_before_its_later_result() {
    for (index, action) in [GoalLifecycleAction::Pause, GoalLifecycleAction::Cancel]
        .into_iter()
        .enumerate()
    {
        let (endpoint, responses, captured) = scripted_channel_server(2).await;
        let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
        let session = fixture
            .engine
            .create_session(selection)
            .expect("idle lifecycle session");
        let session_id = session.session_id;
        let authority = producer_authority();
        let registration = fixture
            .engine
            .register_producer(session_id, authority.clone())
            .await
            .expect("idle ordinary registration");
        let goal = fixture
            .engine
            .set_session_goal(
                SessionGoalSetParams {
                    session_id,
                    objective: format!("Idle lifecycle objective {index}"),
                    selection: None,
                },
                client_origin(),
            )
            .await
            .expect("idle lifecycle goal")
            .goal;
        fixture
            .engine
            .change_session_goal_lifecycle(
                SessionGoalLifecycleParams {
                    session_id,
                    goal_id: goal.goal_id,
                    expected_revision: goal.revision,
                    action,
                    selection: None,
                },
                client_origin(),
            )
            .await
            .expect("idle lifecycle decision");

        let control = producer_projection(&fixture.engine, session_id)
            .messages
            .into_iter()
            .find(|message| {
                message.producer_owner
                    == ProducerOwner::GoalControl {
                        goal_id: goal.goal_id,
                    }
            })
            .expect("idle lifecycle control");
        assert_eq!(control.mode, ProducerDeliveryMode::Steer);
        assert_eq!(control.reminder, None);
        let registrations = fixture
            .engine
            .session_producers(SessionProducersParams { session_id })
            .await
            .expect("idle lifecycle registrations")
            .producers;
        assert_eq!(registrations.len(), 1);
        assert_eq!(registrations[0].producer_id, registration);
        let control_admission = await_event(
            &fixture.engine,
            session_id,
            "idle lifecycle control admission",
            |event| {
                matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id } if message_id == control.message_id)
            },
        )
        .await;
        let control_run = control_admission.run_id.expect("idle control run");
        let notice = match action {
            GoalLifecycleAction::Pause => "paused by the user",
            GoalLifecycleAction::Cancel => "cancelled by the user",
            GoalLifecycleAction::Resume => unreachable!(),
        };
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                notice,
                scripted_text_body("idle lifecycle control consumed"),
            ))
            .expect("idle lifecycle control response");
        await_event(&fixture.engine, session_id, "idle control consumed", |event| {
            matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, run_id } if message_id == control.message_id && run_id == control_run)
        })
        .await;
        wait_for_session_not_running(&fixture.engine, session_id).await;

        let later_body = format!("later ordinary producer result {index}");
        let later = fixture
            .engine
            .send_producer_message(
                session_id,
                authority,
                registration,
                ProducerDeliveryMode::Steer,
                producer_key(&format!("later-idle-result-{index}")),
                later_body.clone(),
            )
            .await
            .expect("send after lifecycle control");
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                &later_body,
                scripted_text_body("later ordinary producer result consumed"),
            ))
            .expect("later producer response");
        await_event(&fixture.engine, session_id, "later producer consumed", |event| {
            matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, .. } if message_id == later)
        })
        .await;
        wait_for_session_not_running(&fixture.engine, session_id).await;

        let projection = producer_projection(&fixture.engine, session_id);
        let controls = projection
            .messages
            .iter()
            .filter(|message| matches!(&message.producer_owner, ProducerOwner::GoalControl { .. }))
            .collect::<Vec<_>>();
        assert_eq!(controls.len(), 1);
        assert!(controls[0].consumed);
        assert!(controls[0].consumption_recorded);
        assert_eq!(
            projection.goal.as_ref().expect("lifecycle goal").status,
            match action {
                GoalLifecycleAction::Pause => GoalStatus::Paused,
                GoalLifecycleAction::Cancel => GoalStatus::Cancelled,
                GoalLifecycleAction::Resume => unreachable!(),
            }
        );
        assert_eq!(
            projection
                .messages
                .iter()
                .filter(|message| message.reminder.is_some())
                .count(),
            0
        );
        let requests = captured.await.expect("idle audience requests");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains(notice));
        assert!(!requests[0].contains(&later_body));
        assert!(requests[1].contains(&later_body));
        assert_no_notification_run_abort(&fixture.engine, session_id);
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn fully_idle_goal_pause_and_cancel_emit_no_control_or_run() {
    for (index, action) in [GoalLifecycleAction::Pause, GoalLifecycleAction::Cancel]
        .into_iter()
        .enumerate()
    {
        let (fixture, selection) = custom_fixture();
        let session = fixture
            .engine
            .create_session(selection)
            .expect("fully idle lifecycle session");
        let session_id = session.session_id;
        let (compaction, compaction_release) = hold_compaction(&fixture.engine, session_id).await;
        let goal = fixture
            .engine
            .set_session_goal(
                SessionGoalSetParams {
                    session_id,
                    objective: format!("Fully idle lifecycle objective {index}"),
                    selection: None,
                },
                client_origin(),
            )
            .await
            .expect("fully idle lifecycle goal")
            .goal;
        fixture
            .engine
            .change_session_goal_lifecycle(
                SessionGoalLifecycleParams {
                    session_id,
                    goal_id: goal.goal_id,
                    expected_revision: goal.revision,
                    action,
                    selection: None,
                },
                client_origin(),
            )
            .await
            .expect("fully idle lifecycle decision");

        assert!(
            !producer_projection(&fixture.engine, session_id)
                .messages
                .iter()
                .any(|message| matches!(
                    &message.producer_owner,
                    ProducerOwner::GoalControl { .. }
                ))
        );
        assert!(
            fixture
                .engine
                .session_producers(SessionProducersParams { session_id })
                .await
                .expect("fully idle registrations")
                .producers
                .is_empty()
        );
        release_compaction(compaction, compaction_release).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !producer_events(&fixture.engine, session_id)
                .iter()
                .any(|event| matches!(event.payload, EventPayload::RunStarted { .. }))
        );
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn pending_message_without_registration_is_goal_control_audience() {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection)
        .expect("pending-only audience session");
    let session_id = session.session_id;
    let (compaction, compaction_release) = hold_compaction(&fixture.engine, session_id).await;
    let authority = producer_authority();
    let registration = fixture
        .engine
        .register_producer(session_id, authority.clone())
        .await
        .expect("pending-only producer registration");
    let goal = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Notify pending work without its registration".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("pending-only goal")
        .goal;
    let pending = fixture
        .engine
        .send_producer_message(
            session_id,
            authority.clone(),
            registration,
            ProducerDeliveryMode::Queue,
            producer_key("pending-only-audience"),
            "producer work accepted before pause".into(),
        )
        .await
        .expect("pending-only message");
    fixture
        .engine
        .unregister_producer(session_id, authority, registration)
        .await
        .expect("remove pending message registration");
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
            client_origin(),
        )
        .await
        .expect("pause with pending-only audience");
    let projection = producer_projection(&fixture.engine, session_id);
    let control = projection
        .messages
        .iter()
        .find(|message| matches!(&message.producer_owner, ProducerOwner::GoalControl { .. }))
        .expect("pending-only goal control")
        .clone();
    assert_eq!(control.reminder, None);
    assert!(
        fixture
            .engine
            .session_producers(SessionProducersParams { session_id })
            .await
            .expect("pending-only registrations")
            .producers
            .is_empty()
    );

    release_compaction(compaction, compaction_release).await;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "paused by the user",
            scripted_text_body("pending-only work and control consumed"),
        ))
        .expect("pending-only model response");
    await_event(&fixture.engine, session_id, "pending-only message consumed", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, .. } if message_id == pending)
    })
    .await;
    await_event(&fixture.engine, session_id, "pending-only control consumed", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, .. } if message_id == control.message_id)
    })
    .await;
    wait_for_session_not_running(&fixture.engine, session_id).await;
    let requests = captured.await.expect("pending-only request");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("producer work accepted before pause"));
    assert!(requests[0].contains("paused by the user"));
    assert_eq!(
        producer_projection(&fixture.engine, session_id)
            .goal
            .expect("pending-only paused goal")
            .status,
        GoalStatus::Paused
    );
    assert_eq!(
        producer_projection(&fixture.engine, session_id)
            .messages
            .iter()
            .filter(|message| message.reminder.is_some())
            .count(),
        0
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn accepted_pause_control_survives_cancelled_run_and_wakes_without_registration() {
    let (endpoint, first_seen, release_first, captured) = cancelled_request_boundary_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("pause cancellation boundary session");
    let session_id = session.session_id;
    let blocker_authority = producer_authority();
    let blocker = fixture
        .engine
        .register_producer(session_id, blocker_authority.clone())
        .await
        .expect("pause cancellation blocker");
    let goal = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Retain control across run cancellation".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("pause cancellation goal")
        .goal;
    let old_run = fixture
        .engine
        .start_run(
            RunStartParams {
                session_id,
                client_run_id: ClientRunId::new("pause-old-run").expect("old run ID"),
                selection,
                input: "hold the old run in flight".into(),
            },
            client_origin(),
        )
        .await
        .expect("start old run")
        .run_id;
    await_event(
        &fixture.engine,
        session_id,
        "old run model attempt",
        |event| {
            event.run_id == Some(old_run)
                && matches!(event.payload, EventPayload::ModelAttemptStarted { .. })
        },
    )
    .await;
    first_seen.await.expect("old request reached server");
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
            client_origin(),
        )
        .await
        .expect("accept pause before old run cancellation");
    let control = producer_projection(&fixture.engine, session_id)
        .messages
        .into_iter()
        .find(|message| matches!(&message.producer_owner, ProducerOwner::GoalControl { .. }))
        .expect("pause control accepted before cancellation");
    fixture
        .engine
        .unregister_producer(session_id, blocker_authority, blocker)
        .await
        .expect("remove final ordinary registration");
    fixture
        .engine
        .cancel_run(old_run)
        .await
        .expect("cancel old run before control promotion");
    release_first.notify_one();

    let next_admission = await_event(
        &fixture.engine,
        session_id,
        "pause control admitted to replacement run",
        |event| {
            matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id } if message_id == control.message_id)
                && event.run_id != Some(old_run)
        },
    )
    .await;
    let next_run = next_admission.run_id.expect("replacement producer run");
    await_event(&fixture.engine, session_id, "replacement pause consumed", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, run_id } if message_id == control.message_id && run_id == next_run)
    })
    .await;
    wait_for_session_not_running(&fixture.engine, session_id).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        fixture
            .engine
            .session_producers(SessionProducersParams { session_id })
            .await
            .expect("post-control registrations")
            .producers
            .is_empty()
    );
    assert_eq!(
        producer_events(&fixture.engine, session_id)
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::RunStarted { .. }))
            .count(),
        2
    );
    assert_eq!(
        producer_events(&fixture.engine, session_id)
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::RunCancelled { .. }))
            .count(),
        1
    );
    assert!(
        !producer_events(&fixture.engine, session_id)
            .iter()
            .any(|event| matches!(event.payload, EventPayload::RunInterrupted { .. }))
    );
    let requests = captured.await.expect("run-boundary pause requests");
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].contains("paused by the user"));
    assert!(requests[1].contains("paused by the user"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn goal_tool_visibility_is_frozen_at_run_admission_across_lifecycle_changes() {
    let (endpoint, responses, captured) = scripted_channel_server(4).await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Goal visibility test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n  write: allow\n---\nExercise goal tool visibility.\n",
    );
    fixture
        .engine
        .register_tool_provider(Arc::new(GoalVisibilityProvider));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("goal tool session");
    let session_id = session.session_id;
    let blocker_authority = producer_authority();
    let blocker = fixture
        .engine
        .register_producer(session_id, blocker_authority.clone())
        .await
        .expect("goal tool blocker");

    let no_goal_run = fixture
        .engine
        .start_run(
            RunStartParams {
                session_id,
                client_run_id: ClientRunId::new("goal-tools-none").expect("no-goal run ID"),
                selection: selection.clone(),
                input: "request before goal activation".into(),
            },
            client_origin(),
        )
        .await
        .expect("no-goal run")
        .run_id;
    await_event(
        &fixture.engine,
        session_id,
        "no-goal model attempt",
        |event| {
            event.run_id == Some(no_goal_run)
                && matches!(event.payload, EventPayload::ModelAttemptStarted { .. })
        },
    )
    .await;
    let goal = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Exercise frozen goal tools".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("activate goal during run")
        .goal;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "request before goal activation",
            scripted_text_body("no-goal snapshot complete"),
        ))
        .expect("no-goal response");
    wait_for_session_not_running(&fixture.engine, session_id).await;

    fixture
        .engine
        .start_run(
            RunStartParams {
                session_id,
                client_run_id: ClientRunId::new("goal-tools-active").expect("active run ID"),
                selection: selection.clone(),
                input: "request admitted with active goal".into(),
            },
            client_origin(),
        )
        .await
        .expect("active goal run");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "request admitted with active goal",
            scripted_text_body("active goal snapshot complete"),
        ))
        .expect("active response");
    wait_for_session_not_running(&fixture.engine, session_id).await;
    fixture
        .engine
        .unregister_producer(session_id, blocker_authority.clone(), blocker)
        .await
        .expect("remove blocker before idle pause");

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
        .expect("pause goal before admission")
        .goal;
    let blocker = fixture
        .engine
        .register_producer(session_id, blocker_authority.clone())
        .await
        .expect("register frozen-run producer");
    let paused_run = fixture
        .engine
        .start_run(
            RunStartParams {
                session_id,
                client_run_id: ClientRunId::new("goal-tools-paused").expect("paused run ID"),
                selection,
                input: "request admitted with paused goal".into(),
            },
            client_origin(),
        )
        .await
        .expect("paused goal run")
        .run_id;
    await_event(
        &fixture.engine,
        session_id,
        "paused-goal model attempt",
        |event| {
            event.run_id == Some(paused_run)
                && matches!(event.payload, EventPayload::ModelAttemptStarted { .. })
        },
    )
    .await;
    fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: goal.goal_id,
                expected_revision: paused.revision,
                action: GoalLifecycleAction::Cancel,
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("cancel goal during frozen run");
    assert!(
        fixture
            .engine
            .goal_update(
                session_id,
                cookie_agent_protocol::GoalUpdateParams { items: Vec::new() },
            )
            .await
            .expect_err("terminal update must fail")
            .to_string()
            .contains("terminal goals cannot be changed")
    );
    let steer_id = fixture
        .engine
        .send_producer_message(
            session_id,
            blocker_authority.clone(),
            blocker,
            ProducerDeliveryMode::Steer,
            producer_key("frozen-terminal-steer"),
            "continue the already admitted frozen run".into(),
        )
        .await
        .expect("steer frozen run");
    fixture
        .engine
        .unregister_producer(session_id, blocker_authority, blocker)
        .await
        .expect("close frozen-run producer");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "request admitted with paused goal",
            scripted_text_body("paused request complete"),
        ))
        .expect("paused response");
    await_event(&fixture.engine, session_id, "frozen run steer admission", |event| {
        event.run_id == Some(paused_run)
            && matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id } if message_id == steer_id)
    })
    .await;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "continue the already admitted frozen run",
            scripted_text_body("terminal frozen snapshot complete"),
        ))
        .expect("frozen follow-up response");
    wait_for_session_not_running(&fixture.engine, session_id).await;

    let requests = captured.await.expect("goal tool visibility requests");
    assert_eq!(requests.len(), 4);
    assert!(request_tool_names(&requests[0]).is_empty());
    for request in &requests[1..] {
        assert_eq!(request_tool_names(request), ["goal_get", "goal_update"]);
        let parameters = request_tool_parameters(request, "goal_update");
        assert_eq!(parameters["required"], serde_json::json!(["items"]));
        assert_eq!(
            parameters["properties"]
                .as_object()
                .expect("goal_update properties")
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["items"]
        );
    }
    assert!(!requests[2].contains("cancelled by the user"));
    assert!(requests[3].contains("cancelled by the user"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn paused_goal_survives_fork_and_revert_but_not_delegated_session_boundaries() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let root = fixture
        .engine
        .create_session(selection.clone())
        .expect("goal boundary root");
    let root_id = root.session_id;
    let (compaction, compaction_release) = hold_compaction(&fixture.engine, root_id).await;
    fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id: root_id,
                objective: "Preserve branch checklist".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("branch goal");
    let revised = fixture
        .engine
        .goal_update(
            root_id,
            cookie_agent_protocol::GoalUpdateParams {
                items: vec![GoalItem {
                    description: "Retain this checklist item".into(),
                    finished: false,
                }],
            },
        )
        .await
        .expect("branch checklist")
        .goal;
    let paused = fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id: root_id,
                goal_id: revised.goal_id,
                expected_revision: revised.revision,
                action: GoalLifecycleAction::Pause,
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("pause branch goal")
        .goal;
    release_compaction(compaction, compaction_release).await;
    let blocker_authority = producer_authority();
    let blocker = fixture
        .engine
        .register_producer(root_id, blocker_authority.clone())
        .await
        .expect("goal boundary producer");
    let boundary_run = fixture
        .engine
        .start_run(
            RunStartParams {
                session_id: root_id,
                client_run_id: ClientRunId::new("paused-goal-fork-boundary")
                    .expect("fork boundary run ID"),
                selection,
                input: "commit paused goal fork boundary".into(),
            },
            client_origin(),
        )
        .await
        .expect("start fork boundary run")
        .run_id;
    await_event(
        &fixture.engine,
        root_id,
        "fork boundary model attempt",
        |event| {
            event.run_id == Some(boundary_run)
                && matches!(event.payload, EventPayload::ModelAttemptStarted { .. })
        },
    )
    .await;
    let branch_message = fixture
        .engine
        .send_producer_message(
            root_id,
            blocker_authority.clone(),
            blocker,
            ProducerDeliveryMode::Steer,
            producer_key("forked-branch-message"),
            "retain this producer message across branches".into(),
        )
        .await
        .expect("branch producer message");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "commit paused goal fork boundary",
            scripted_text_body("paused goal boundary first turn"),
        ))
        .expect("fork boundary first response");
    await_event(&fixture.engine, root_id, "branch message admission", |event| {
        event.run_id == Some(boundary_run)
            && matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id } if message_id == branch_message)
    })
    .await;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "retain this producer message across branches",
            scripted_text_body("paused goal boundary committed"),
        ))
        .expect("fork boundary producer response");
    wait_for_session_not_running(&fixture.engine, root_id).await;
    let boundary_projection = producer_projection(&fixture.engine, root_id);
    let boundary_message = boundary_projection
        .messages
        .iter()
        .find(|message| message.message_id == branch_message)
        .expect("committed branch message");
    assert!(boundary_message.consumed);
    assert!(boundary_message.consumption_recorded);
    let boundary = producer_events(&fixture.engine, root_id)
        .last()
        .expect("paused boundary")
        .seq;
    let fork = fixture
        .engine
        .fork_session(root_id, boundary, client_origin())
        .await
        .expect("fork paused goal");
    assert_eq!(
        producer_projection(&fixture.engine, fork.session_id).goal,
        Some(paused.clone())
    );
    assert_eq!(
        producer_projection(&fixture.engine, fork.session_id).messages,
        boundary_projection.messages
    );
    assert_eq!(
        fixture
            .engine
            .session_producers(SessionProducersParams {
                session_id: root_id,
            })
            .await
            .expect("source registry")
            .producers
            .len(),
        1
    );
    assert!(
        fixture
            .engine
            .session_producers(SessionProducersParams {
                session_id: fork.session_id,
            })
            .await
            .expect("fork registry")
            .producers
            .is_empty()
    );

    fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id: root_id,
                goal_id: paused.goal_id,
                expected_revision: paused.revision,
                action: GoalLifecycleAction::Resume,
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("resume source after fork");
    fixture
        .engine
        .goal_update(
            root_id,
            cookie_agent_protocol::GoalUpdateParams {
                items: vec![GoalItem {
                    description: "This branch is reverted".into(),
                    finished: false,
                }],
            },
        )
        .await
        .expect("temporary branch update");
    fixture
        .engine
        .revert_session(root_id, boundary, client_origin())
        .await
        .expect("revert goal branch");
    assert_eq!(
        producer_projection(&fixture.engine, root_id).goal,
        Some(paused)
    );
    assert_eq!(
        producer_projection(&fixture.engine, root_id).messages,
        boundary_projection.messages
    );
    assert!(
        fixture
            .engine
            .session_producers(SessionProducersParams {
                session_id: root_id,
            })
            .await
            .expect("reverted source registry")
            .producers
            .is_empty()
    );
    assert!(
        fixture
            .engine
            .unregister_producer(root_id, blocker_authority, blocker)
            .await
            .expect_err("revert invalidates stale registration")
            .to_string()
            .contains("closed, foreign, or wrong-session registration")
    );
    assert_eq!(captured.await.expect("fork boundary requests").len(), 2);
    fixture.engine.shutdown().await;

    let (endpoint, captured) = scripted_delegation_server().await;
    let (delegation_fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    delegation_fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: delegation_fixture.engine.clone(),
        }));
    let parent = delegation_fixture
        .engine
        .create_session(selection.clone())
        .expect("delegated boundary parent");
    let parent_blocker_authority = producer_authority();
    let parent_blocker = delegation_fixture
        .engine
        .register_producer(parent.session_id, parent_blocker_authority.clone())
        .await
        .expect("delegated boundary blocker");
    delegation_fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id: parent.session_id,
                objective: "Parent-only goal".into(),
                selection: None,
            },
            client_origin(),
        )
        .await
        .expect("parent-only goal");
    delegation_fixture
        .engine
        .start_run(
            RunStartParams {
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("delegated-goal-boundary")
                    .expect("delegation run ID"),
                selection,
                input: "delegate a child for goal boundary coverage".into(),
            },
            client_origin(),
        )
        .await
        .expect("delegation boundary run");
    wait_for_session_not_running(&delegation_fixture.engine, parent.session_id).await;
    let child_id = delegation_fixture
        .engine
        .children(parent.session_id)
        .first()
        .expect("delegated child")
        .session_id;
    assert_eq!(
        producer_projection(&delegation_fixture.engine, child_id).goal,
        None
    );
    assert!(
        delegation_fixture
            .engine
            .get_session_goal(SessionGoalGetParams {
                session_id: child_id,
            })
            .await
            .expect_err("delegated goal get")
            .to_string()
            .contains("goals are available only in root sessions")
    );
    assert!(
        delegation_fixture
            .engine
            .set_session_goal(
                SessionGoalSetParams {
                    session_id: child_id,
                    objective: "Child goal must fail".into(),
                    selection: None,
                },
                client_origin(),
            )
            .await
            .expect_err("delegated goal set")
            .to_string()
            .contains("goals are available only in root sessions")
    );
    assert!(
        delegation_fixture
            .engine
            .goal_update(
                child_id,
                cookie_agent_protocol::GoalUpdateParams { items: Vec::new() },
            )
            .await
            .expect_err("delegated goal update")
            .to_string()
            .contains("goals are available only in root sessions")
    );
    delegation_fixture
        .engine
        .unregister_producer(parent.session_id, parent_blocker_authority, parent_blocker)
        .await
        .expect("remove delegated boundary blocker");
    assert_eq!(
        captured.await.expect("delegated boundary requests").len(),
        3
    );
    delegation_fixture.engine.shutdown().await;
}
