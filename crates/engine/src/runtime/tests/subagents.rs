use std::{fs, sync::Arc};

use cookie_agent_protocol::{
    AgentId, ClientRenameId, ClientRunId, EventPayload, InternalAgentKind, PermissionMode,
    RunStartParams, SessionStatus, SessionTitle, SessionTitleChange,
};

use crate::EngineHistoryView;

use super::support::*;

#[tokio::test]
async fn delegated_child_uses_description_title_without_title_agent() {
    let bodies = vec![
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"titled-delegate\",\"type\":\"function\",\"function\":{\"name\":\"delegate_subagent\",\"arguments\":\"{\\\"agent_type\\\":\\\"worker\\\",\\\"description\\\":\\\"Write report\\\",\\\"prompt\\\":\\\"write report\\\"}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"Parent delegation title\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"delegated child report\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"parent accepted child report\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
    ];
    let (endpoint, captured, _reached, _release) =
        scripted_server_with_delayed_response(bodies, usize::MAX).await;
    let (mut fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        &endpoint,
        "---\ndescription: Titled delegation parent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  delegate:\n    worker: allow\n---\nTest delegated titles.\n",
        None,
        None,
        true,
    );
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("titled parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("titled-delegation").expect("run ID"),
                selection,
                content: "delegate a titled child".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted titled delegation");
    await_session_change(
        &fixture.engine,
        parent.session_id,
        "titled child completion",
        || {
            (fixture
                .engine
                .children(parent.session_id)
                .expect("children")
                .first()
                .is_some_and(|child| child.status == SessionStatus::Completed)
                && fixture
                    .engine
                    .get_session(parent.session_id)
                    .is_ok_and(|parent| parent.status == SessionStatus::Completed))
            .then_some(())
        },
    )
    .await;
    let child_id = fixture
        .engine
        .children(parent.session_id)
        .expect("children")[0]
        .session_id;
    let child = fixture
        .engine
        .inner
        .store
        .get(child_id)
        .expect("titled child projection");
    assert_eq!(
        child.meta.title.as_ref().map(SessionTitle::as_str),
        Some("Write report")
    );
    // Creation writes the inherited `global` and `local` attachments first.
    assert!(matches!(
        child.log.events()[3].payload,
        EventPayload::SessionTitleCommitted {
            change: SessionTitleChange::DelegatedSet { .. },
            input_through_seq: 0,
        }
    ));
    assert!(!child.log.events().iter().any(|event| matches!(
        event.payload,
        EventPayload::InternalAgentStarted {
            kind: InternalAgentKind::SessionTitle,
            ..
        }
    )));
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(parent.session_id)
            .expect("titled parent projection")
            .log
            .events()
            .iter()
            .any(|event| matches!(
                event.payload,
                EventPayload::InternalAgentStarted {
                    kind: InternalAgentKind::SessionTitle,
                    ..
                }
            ))
    );
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("titled delegation server")
            .len(),
        4
    );
    fixture
        .engine
        .rename_session(
            cookie_agent_protocol::SessionRenameParams {
                session_id: child_id,
                client_rename_id: ClientRenameId::new("delegated-user-title").expect("rename ID"),
                change: cookie_agent_protocol::SessionRenameChange::Set {
                    title: SessionTitle::new("User title").expect("user title"),
                },
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("rename delegated child");
    let child_dir = fixture.engine.inner.store.session_dir(child_id);
    fixture.engine.shutdown().await;
    drop(fixture.engine);

    let reopened = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    assert_eq!(
        reopened
            .get_session(child_id)
            .expect("user-renamed delegated child")
            .title
            .as_ref()
            .map(SessionTitle::as_str),
        Some("User title")
    );
    reopened.shutdown().await;
    drop(reopened);

    let event_path = child_dir.join("events.jsonl");
    let mut events = fs::read_to_string(&event_path)
        .expect("child events")
        .lines()
        .map(|line| {
            serde_json::from_str::<cookie_agent_protocol::StoredEvent>(line).expect("event")
        })
        .collect::<Vec<_>>();
    let removed_seq = events
        .iter()
        .find(|event| {
            matches!(
                event.payload,
                EventPayload::SessionTitleCommitted {
                    change: SessionTitleChange::DelegatedSet { .. },
                    ..
                }
            )
        })
        .expect("delegated title event")
        .seq;
    events.retain(|event| {
        !matches!(
            event.payload,
            EventPayload::SessionTitleCommitted {
                change: SessionTitleChange::DelegatedSet { .. },
                ..
            }
        )
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
    fs::write(&event_path, rewritten).expect("remove delegated title crash window");
    fixture.config.runtime.session_title.max_chars = 4;

    let reopened = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    reopened
        .resume(child_id)
        .await
        .expect("adopt recovered child");
    let recovered_child = reopened
        .inner
        .store
        .get(child_id)
        .expect("recovered titled child");
    assert_eq!(
        recovered_child
            .meta
            .title
            .as_ref()
            .map(SessionTitle::as_str),
        Some("User title")
    );
    assert!(recovered_child.log.events().iter().any(|event| {
        matches!(
            &event.payload,
            EventPayload::SessionTitleCommitted {
                change: SessionTitleChange::DelegatedSet { title, .. },
                ..
            } if title.as_str() == "Write report"
        )
    }));
    reopened.shutdown().await;
}

#[tokio::test]
async fn background_delegate_returns_session_then_notifies_and_paginates() {
    let (endpoint, captured) = scripted_background_delegation_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    write_private_test_file(
        &fixture._directory.path().join("AGENTS.md"),
        "root-only AGENTS.md context",
    );
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("background parent session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("background-delegation").expect("run ID"),
                selection: selection.clone(),
                content: "delegate in the background".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted background parent run");

    let child_session_id = await_session_change(
        &fixture.engine,
        parent.session_id,
        "immediate background session result",
        || {
            let projection = fixture
                .engine
                .inner
                .store
                .get(parent.session_id)
                .expect("background parent projection");
            projection.log.events().iter().find_map(|event| {
                let EventPayload::ToolCallTerminated { termination } = &event.payload else {
                    return None;
                };
                termination
                    .result
                    .as_ref()?
                    .metadata
                    .get("session_id")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
            })
        },
    )
    .await;

    await_event(
        &fixture.engine,
        parent.session_id,
        "background completion notification",
        |event| {
            matches!(
                &event.payload,
                EventPayload::DelegationFinished { child_session_id: session_id, status: SessionStatus::Completed, preview, total_lines: 3, .. } if *session_id == child_session_id
                    && preview == "first line\nsecond line\nthird line"
            )
        },
    )
    .await;
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(parent.session_id)
            .unwrap()
            .log
            .events()
            .iter()
            .any(|event| matches!(event.payload, EventPayload::AgentMdLoaded { .. }))
    );
    assert!(
        !fixture
            .engine
            .inner
            .store
            .get(child_session_id)
            .unwrap()
            .log
            .events()
            .iter()
            .any(|event| matches!(event.payload, EventPayload::AgentMdLoaded { .. }))
    );

    let page = fixture
        .engine
        .get_subagent_result(
            parent.session_id,
            child_session_id,
            false,
            1,
            1,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("paginated subagent result");
    assert_eq!(
        page.output,
        "<status>completed</status>\n<content>\n2: second line\n</content>"
    );
    assert_eq!(page.metadata["offset"], 1);
    assert_eq!(page.metadata["limit"], 1);
    assert_eq!(page.metadata["total_lines"], 3);

    let foreign = fixture
        .engine
        .create_session(selection)
        .expect("foreign session");
    assert!(
        fixture
            .engine
            .get_subagent_result(
                foreign.session_id,
                child_session_id,
                false,
                0,
                1,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .is_err()
    );
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("background server")
            .len(),
        3
    );
    fixture.engine.shutdown().await;
}

/// A fork of a session whose background child's report arrived as a
/// producer message leaves that child behind: the fork's model history is
/// the source's, it has no delegations, and it delegates again to a child of
/// its own while the source and its child stay as they were.
#[tokio::test]
async fn a_fork_leaves_its_source_subagents_behind_and_delegates_its_own() {
    // More requests than either branch makes: the source may take a wake run
    // for the report, and the server is never awaited.
    let (endpoint, responses, _captured) = scripted_channel_server(16).await;
    for response in [
        MatchedScriptedResponse::last_message_contains(
            "delegate in the background",
            scripted_tool_body(
                "background-delegate-call",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type": "worker",
                    "description": "Write report",
                    "prompt": "write report",
                    "background": true
                }),
            ),
        ),
        MatchedScriptedResponse::last_message_contains(
            "write report",
            scripted_text_body("first line\nsecond line"),
        ),
        MatchedScriptedResponse::last_message_contains(
            "background-delegate-call",
            scripted_text_body("parent continued after admission"),
        ),
        MatchedScriptedResponse::last_message_contains(
            "parent continued after admission",
            scripted_text_body("parent read the report"),
        ),
        MatchedScriptedResponse::last_message_contains(
            "delegate in the fork",
            scripted_tool_body(
                "fork-delegate-call",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type": "worker",
                    "description": "Fork task",
                    "prompt": "fork child task"
                }),
            ),
        ),
        MatchedScriptedResponse::last_message_contains(
            "fork child task",
            scripted_text_body("fork child report"),
        ),
        MatchedScriptedResponse::last_message_contains(
            "fork-delegate-call",
            scripted_text_body("fork accepted child report"),
        ),
    ] {
        responses.send(response).expect("scripted response");
    }
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
    let source = fixture
        .engine
        .create_session(selection.clone())
        .expect("source session")
        .session_id;
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: source,
                client_run_id: ClientRunId::new("source-delegation").expect("run ID"),
                selection: selection.clone(),
                content: "delegate in the background".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("source run");
    let source_tip = await_projection(
        &fixture.engine,
        source,
        "the report consumed and the source idle",
        |projection| {
            projection.status != SessionStatus::Running
                && crate::goal_projection::GoalProducerProjection::from_events(
                    &projection.log.event_snapshot(),
                )
                .messages
                .iter()
                .any(|message| {
                    message.consumed
                        && matches!(
                            message.producer_owner,
                            cookie_agent_protocol::ProducerOwner::Delegation { .. }
                        )
                })
        },
    )
    .await
    .meta
    .last_event_seq;
    let source_children = fixture.engine.children(source).expect("source children");
    assert_eq!(source_children.len(), 1);
    let source_child = source_children[0].session_id;
    let source_events = fixture.engine.inner.store.log(source).unwrap().events();
    let source_child_events = fixture
        .engine
        .inner
        .store
        .log(source_child)
        .unwrap()
        .events();
    let delegation_seqs = source_events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                EventPayload::DelegationReserved { .. }
                    | EventPayload::DelegationProgressed { .. }
                    | EventPayload::DelegationFinished { .. }
            )
        })
        .map(|event| event.seq)
        .collect::<Vec<_>>();
    assert!(delegation_seqs.len() >= 3, "{delegation_seqs:?}");

    let fork = fixture
        .engine
        .fork_session(
            source,
            source_tip,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            None,
        )
        .await
        .expect("fork a session that delegated")
        .session_id;
    let fork_events = fixture.engine.inner.store.log(fork).unwrap().events();
    let omitted_seqs = fork_events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::RecordOmitted { .. }))
        .map(|event| event.seq)
        .collect::<Vec<_>>();
    assert_eq!(omitted_seqs, delegation_seqs);
    for view in [EngineHistoryView::Full, EngineHistoryView::Assembled] {
        assert_eq!(
            serde_json::to_value(fixture.engine.get_history(fork, view).await.unwrap()).unwrap(),
            serde_json::to_value(fixture.engine.get_history(source, view).await.unwrap()).unwrap(),
        );
    }
    assert!(fixture.engine.children(fork).unwrap().is_empty());

    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: fork,
                client_run_id: ClientRunId::new("fork-delegation").expect("run ID"),
                selection,
                content: "delegate in the fork".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("fork run");
    await_projection(&fixture.engine, fork, "the fork's run", |projection| {
        projection.status == SessionStatus::Completed
    })
    .await;
    let fork_children = fixture.engine.children(fork).expect("fork children");
    assert_eq!(fork_children.len(), 1);
    let fork_child = fork_children[0].session_id;
    assert_ne!(fork_child, source_child);
    assert!(matches!(
        fixture.engine.get_session(fork_child).unwrap().origin,
        cookie_agent_protocol::SessionOrigin::Delegated { parent_session_id, .. }
            if parent_session_id == fork
    ));
    let entries = fixture.engine.inner.delegation_events.entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.reservation.parent_session_id == fork
                && entry.reservation.child_session_id == fork_child)
            .count(),
        1
    );
    assert!(
        fixture
            .engine
            .inner
            .store
            .log(fork)
            .unwrap()
            .events()
            .iter()
            .any(|event| matches!(&event.payload, EventPayload::DelegationFinished { child_session_id, status: SessionStatus::Completed, .. } if *child_session_id == fork_child))
    );
    assert_eq!(
        fixture.engine.inner.store.log(source).unwrap().events(),
        source_events
    );
    assert_eq!(
        fixture
            .engine
            .inner
            .store
            .log(source_child)
            .unwrap()
            .events(),
        source_child_events
    );
    assert_eq!(fixture.engine.children(source).unwrap().len(), 1);
    fixture.engine.shutdown().await;

    let reopened = reopen_engine(&fixture);
    reopened.resume(fork).await.expect("adopt the fork");
    let children = reopened.children(fork).expect("reopened fork children");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].session_id, fork_child);
    assert_eq!(
        reopened.children(source).unwrap()[0].session_id,
        source_child
    );
    assert_eq!(reopened.inner.delegation_events.entries().len(), 2);
    // A prefix that ends while the report waits to be read keeps it waiting:
    // a producer message whose delegation the fork does not hold reads as any
    // other.
    let accepted = source_events
        .iter()
        .find(|event| {
            matches!(
                event.payload,
                EventPayload::ProducerMessageAccepted {
                    producer_owner: cookie_agent_protocol::ProducerOwner::Delegation { .. },
                    ..
                }
            )
        })
        .expect("the report's acceptance")
        .seq;
    let waiting = reopened
        .inner
        .store
        .fork_with(
            source,
            accepted,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            &|_| Ok(()),
        )
        .expect("fork while the report waits");
    let mail = crate::goal_projection::GoalProducerProjection::from_events(
        &reopened.inner.store.log(waiting).unwrap().event_snapshot(),
    )
    .messages;
    assert_eq!(mail.len(), 1);
    assert!(!mail[0].consumed && !mail[0].discarded);
    assert!(reopened.children(waiting).unwrap().is_empty());
    reopened.shutdown().await;
}

#[tokio::test]
async fn delegation_completion_triggers_configured_subagent_eviction_after_teaser() {
    let (endpoint, captured) = scripted_background_delegation_server().await;
    let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    reopen_fixture_with_residency(&mut fixture, 0, std::time::Duration::ZERO).await;
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("automatic paging parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("automatic-subagent-paging").expect("run ID"),
                selection,
                content: "delegate in the background".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("automatic paging parent run");

    // Residency eviction has no durable event after the store transition, so
    // this test intentionally polls the residency cache itself.
    let child_session_id = tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), async {
        loop {
            if let Some(child) = fixture
                .engine
                .children(parent.session_id)
                .expect("children")
                .into_iter()
                .find(|child| child.status == SessionStatus::Completed)
                && !fixture.engine.inner.store.is_resident(child.session_id)
            {
                break child.session_id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("completion-triggered subagent eviction");
    assert!(fixture.engine.inner.store.is_resident(parent.session_id));
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(parent.session_id)
            .expect("automatic paging parent projection")
            .log
            .events()
            .iter()
            .any(|event| matches!(
                event.payload,
                EventPayload::DelegationFinished { child_session_id: session_id, .. }
                    if session_id == child_session_id
            ))
    );
    let result = fixture
        .engine
        .get_subagent_result(
            parent.session_id,
            child_session_id,
            false,
            0,
            20,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("automatic paging result reopen");
    assert!(result.output.contains("first line"));
    assert!(fixture.engine.inner.store.is_resident(child_session_id));
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("automatic paging server")
            .len(),
        3
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn delegated_restart_after_agent_removal_falls_back_to_the_default_agent() {
    let (endpoint, responses, server) = scripted_channel_server(4).await;
    let (mut fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
            &endpoint,
            "---\ndescription: Capped worker parent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  delegate:\n    worker: allow\n---\nDelegate capped work.\n",
            None,
            None,
            false,
            None,
            None,
            4_096,
            Some(
                "---\ndescription: Capped worker\nmode: subagent\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\nlimits: { max_output_tokens: 128 }\npermissions: {}\n---\nKeep delegated responses bounded.\n",
            ),
        );
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("capped worker parent");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "create capped child",
            scripted_tool_body(
                "create-capped-child",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type":"worker",
                    "description":"Capped child",
                    "prompt":"first capped child task",
                    "background":true
                }),
            ),
        ))
        .expect("capped child tool response");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "first capped child task",
            scripted_text_body("first capped child result"),
        ))
        .expect("first capped child response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("parent observed capped child"),
        ))
        .expect("capped parent continuation");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("create-capped-child").expect("run ID"),
                selection,
                content: "create capped child".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("create capped child run");
    wait_for_session_not_running(&fixture.engine, parent.session_id).await;
    let child = fixture
        .engine
        .children(parent.session_id)
        .expect("children")[0]
        .session_id;
    wait_for_session_not_running(&fixture.engine, child).await;
    let child_selection = fixture
        .engine
        .get_session(child)
        .expect("capped child metadata")
        .creation_selection;

    fixture.engine.shutdown().await;
    fixture
        .config
        .agents
        .remove(&AgentId::new("worker").expect("worker agent ID"));
    drop(fixture.engine);
    let engine = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "second capped child task",
            scripted_text_body("second capped child result"),
        ))
        .expect("resumed capped child response");
    engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: child,
                client_run_id: ClientRunId::new("resume-capped-child").expect("run ID"),
                selection: child_selection,
                content: "second capped child task".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("resume child after restart");
    wait_for_session_not_running(&engine, child).await;
    // The removed `worker` no longer governs the child: its next run resolves
    // best-effort to the default agent, which carries no 128-token cap.
    let resumed_run = engine
        .inner
        .store
        .get(child)
        .expect("resumed child projection")
        .runs
        .values()
        .find(|run| run.client_run_id.as_str() == "resume-capped-child")
        .expect("resumed child run")
        .clone();
    assert_eq!(resumed_run.agent.agent.as_str(), "primary");
    assert_eq!(resumed_run.selection.agent.as_str(), "primary");

    let requests = with_watchdog("server fixture completion", server)
        .await
        .expect("capped restart server");
    let resumed = requests
        .iter()
        .find(|request| request.contains("second capped child task"))
        .expect("resumed child request");
    let body = resumed
        .split_once("\r\n\r\n")
        .expect("resumed HTTP request body")
        .1;
    let request: serde_json::Value = serde_json::from_str(body).expect("resumed request JSON");
    assert_ne!(
        request
            .get("max_tokens")
            .and_then(serde_json::Value::as_u64),
        Some(128)
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn inherited_context_is_event_backed_and_deterministic_after_restart() {
    let (endpoint, responses, server) = scripted_channel_server(5).await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Context delegation parent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: allow\n  delegate:\n    worker: allow\n---\nTest inherited context.\n",
    );
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Local,
        Arc::new(TestWriteProvider {
            executed: Arc::new(TestFlag::default()),
        }),
    );
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("context parent");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "parent history input",
            scripted_tool_body("context-write", "write", serde_json::json!({})),
        ))
        .expect("write response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("parent assistant context"),
        ))
        .expect("parent context response");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("inherit-context-history").expect("run ID"),
                selection: selection.clone(),
                content: "parent history input".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("context history run");
    wait_for_session_not_running(&fixture.engine, parent.session_id).await;

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "delegate using assembled history",
            scripted_tool_body(
                "context-delegate",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type":"worker",
                    "description":"Inherited context child",
                    "prompt":"inherited child task",
                    "background":true,
                    "inherit_context":true
                }),
            ),
        ))
        .expect("inherit delegation response");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "inherited child task",
            scripted_text_body("inherited child done"),
        ))
        .expect("inherited child response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("parent after inherited delegation"),
        ))
        .expect("inherit parent response");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("inherit-context-delegate").expect("run ID"),
                selection,
                content: "delegate using assembled history".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("inherit delegation run");
    let child_session_id = await_child(
        &fixture.engine,
        parent.session_id,
        "inherited child completion",
        |child| child.status == SessionStatus::Completed,
    )
    .await
    .session_id;
    let child = fixture
        .engine
        .inner
        .store
        .get(child_session_id)
        .expect("inherited child projection");
    let seed = child
        .log
        .events()
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::DelegatedContextSeeded { turns, .. } => Some(turns),
            _ => None,
        })
        .expect("durable inherited context seed");
    let seed_text = seed
        .iter()
        .map(|turn| turn.text.as_str())
        .collect::<String>();
    assert!(seed_text.contains("parent history input"));
    assert!(seed_text.contains("parent assistant context"));
    assert!(!seed_text.contains("executed"));
    assert!(seed.iter().map(|turn| turn.text.len()).sum::<usize>() <= 64 * 1024);
    let reservation_entry = fixture
        .engine
        .inner
        .delegation_events
        .entries()
        .into_iter()
        .find(|entry| entry.reservation.child_session_id == child_session_id)
        .expect("context reservation event");
    assert!(reservation_entry.request.inherit_context);
    assert_eq!(reservation_entry.request.seeded_context, seed);
    let before_restart = serde_json::to_value(
        fixture
            .engine
            .get_history(child_session_id, EngineHistoryView::Assembled)
            .await
            .expect("child assembled history"),
    )
    .expect("serialize child history");
    let requests = with_watchdog("server fixture completion", server)
        .await
        .expect("inherited context server");
    let child_request = requests
        .iter()
        .find(|request| {
            request.contains("inherited child task") && !request.contains("\"role\":\"tool\"")
        })
        .expect("inherited child model request");
    assert!(child_request.contains("parent history input"));
    assert!(child_request.contains("parent assistant context"));
    assert!(!child_request.contains("executed"));
    fixture.engine.shutdown().await;

    let reopened = reopen_engine(&fixture);
    let after_restart = serde_json::to_value(
        reopened
            .get_history(child_session_id, EngineHistoryView::Assembled)
            .await
            .expect("reopened child assembled history"),
    )
    .expect("serialize reopened child history");
    assert_eq!(after_restart, before_restart);
    reopened.shutdown().await;
}

#[tokio::test]
async fn background_delegate_permission_approval_gates_child_admission() {
    let (endpoint, captured) = scripted_background_delegation_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Approval-gated delegation agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  delegate:\n    worker: ask\n---\nTest approval-gated delegation.\n",
    );
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("approval-gated parent");
    fixture
        .engine
        .set_permission_mode(
            parent.session_id,
            PermissionMode::Ask,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .expect("ask mode");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("approval-gated-background").expect("run ID"),
                selection,
                content: "delegate only after approval".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted approval-gated run");

    let approval = wait_for_escalated_approval(&fixture.engine, parent.session_id).await;
    assert!(
        fixture
            .engine
            .children(parent.session_id)
            .expect("children")
            .is_empty()
    );
    approve_once(&fixture.engine, &approval, "background-delegate-approval").await;
    await_child(
        &fixture.engine,
        parent.session_id,
        "approved background child completion",
        |child| child.status == SessionStatus::Completed,
    )
    .await;
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("approval-gated server")
            .len(),
        3
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn tree_permission_mode_gates_child_and_survives_child_eviction() {
    let (endpoint, responses, captured) = scripted_channel_server(4).await;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "delegate under tree mode",
            scripted_tool_body(
                "tree-mode-delegate",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type":"worker",
                    "description":"Exercise child permission mode",
                    "prompt":"write under inherited mode"
                }),
            ),
        ))
        .expect("parent delegation response");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "write under inherited mode",
            scripted_tool_body(
                "tree-mode-write",
                "write",
                serde_json::json!({"value":"blocked"}),
            ),
        ))
        .expect("child write response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("child handled rejected write"),
        ))
        .expect("child completion response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("parent handled child result"),
        ))
        .expect("parent completion response");

    let primary = "---\ndescription: Tree mode parent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  delegate:\n    worker: allow\n---\nDelegate work.\n";
    let worker = "---\ndescription: Tree mode worker\nmode: subagent\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: ask\n---\nWrite work.\n";
    let (fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
            &endpoint,
            primary,
            None,
            None,
            false,
            None,
            None,
            4_096,
            Some(worker),
        );
    let executed = Arc::new(TestFlag::default());
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Local,
        Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }),
    );
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("tree-mode parent");
    fixture
        .engine
        .set_permission_mode(
            parent.session_id,
            PermissionMode::Ask,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .expect("tree ask mode");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("tree-mode-delegation").expect("run ID"),
                selection,
                content: "delegate under tree mode".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("tree-mode run");

    let child = await_child(
        &fixture.engine,
        parent.session_id,
        "tree-mode child",
        |_| true,
    )
    .await;
    assert_eq!(
        fixture
            .engine
            .get_session_permissions(child.session_id)
            .expect("child permission query")
            .current_mode,
        Some(PermissionMode::Ask)
    );
    let approval =
        wait_for_tree_escalated_approval(&fixture.engine, parent.session_id, child.session_id)
            .await;
    assert_eq!(approval.session_id, child.session_id);
    let child_events = fixture
        .engine
        .inner
        .store
        .get(child.session_id)
        .expect("child projection")
        .log
        .events();
    assert!(!child_events.iter().any(|event| matches!(
        event.payload,
        EventPayload::InternalAgentStarted {
            kind: InternalAgentKind::Approval,
            ..
        }
    )));

    fixture
        .engine
        .set_permission_mode(
            child.session_id,
            PermissionMode::Yolo,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .expect("child-addressed tree mode update");
    for session_id in [parent.session_id, child.session_id] {
        assert_eq!(
            fixture
                .engine
                .get_session_permissions(session_id)
                .expect("shared tree mode query")
                .current_mode,
            Some(PermissionMode::Yolo)
        );
    }
    reject_approval(&fixture.engine, &approval, "tree-mode-child-rejection").await;
    await_projection(
        &fixture.engine,
        parent.session_id,
        "tree-mode delegation completion",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;
    assert!(!executed.is_set());
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("tree-mode requests")
            .len(),
        4
    );

    let evicted = fixture
        .engine
        .evict_idle_subagents_for_test(0, std::time::Duration::ZERO)
        .await
        .expect("evict tree-mode child");
    assert!(evicted.contains(&child.session_id));
    assert_eq!(
        fixture
            .engine
            .get_session_permissions(child.session_id)
            .expect("reopened child permission query")
            .current_mode,
        Some(PermissionMode::Yolo)
    );
    fixture.engine.shutdown().await;
}
