use std::sync::Arc;

use cookie_agent_protocol::{
    ApprovalId, ClientRenameId, ClientRunId, EventPayload, RunStartParams, SessionStatus,
    SessionTitle, SessionTitleChange,
};

use super::support::*;

#[tokio::test]
async fn foreground_delegate_and_its_fork_page_after_delayed_compaction_releases() {
    let (endpoint, responses, captured) = scripted_channel_server(4).await;
    for response in [
        MatchedScriptedResponse::last_message_contains(
            "delegate this task",
            scripted_tool_body(
                "delegate-call",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type": "worker", "description": "Write report", "prompt": "write report"
                }),
            ),
        ),
        MatchedScriptedResponse::last_message_contains(
            "write report",
            scripted_text_body("delegated child report"),
        ),
        MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("parent accepted child report"),
        ),
        MatchedScriptedResponse::last_message_contains(
            crate::runtime::compaction::COMPACTION_INSTRUCTION,
            scripted_text_body("Compacted child report"),
        ),
    ] {
        responses.send(response).expect("scripted response");
    }
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("parent session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("scripted-delegation")
                    .expect("run ID"),
                selection,
                input: "delegate this task".to_owned(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted parent run");
    await_projection(
        &fixture.engine,
        parent.session_id,
        "delegation completion",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;
    let children = fixture
        .engine
        .children(parent.session_id)
        .expect("children");
    assert_eq!(children.len(), 1);
    let child = fixture
        .engine
        .get_session(children[0].session_id)
        .expect("child session");
    assert_eq!(
        child.status,
        cookie_agent_protocol::SessionStatus::Completed
    );
    assert!(
        fixture
            .engine
            .delegation_finished_for_test(child.session_id),
        "foreground state: {}",
        fixture.engine.delegation_state_for_test(child.session_id)
    );
    assert!(
        fixture
            .engine
            .subagent_eviction_eligible_for_test(child.session_id, std::time::Duration::ZERO)
    );
    let (janitor_reached, janitor_release) = fixture
        .engine
        .install_janitor_before_barrier_hook_for_test();
    let janitor_engine = fixture.engine.clone();
    let janitor = tokio::spawn(async move {
        janitor_engine
            .evict_idle_subagents_for_test(0, std::time::Duration::ZERO)
            .await
    });
    tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), janitor_reached)
        .await
        .expect("janitor pre-barrier hook timeout")
        .expect("janitor reached pre-barrier hook");
    let (compaction_reached, compaction_release) =
        fixture.engine.install_compaction_execution_hook_for_test();
    let compaction = fixture
        .engine
        .enqueue_compact_without_residency_for_test(child.session_id)
        .await
        .expect("queue compaction ahead of eviction barrier");
    tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), compaction_reached)
        .await
        .expect("compaction execution hook timeout")
        .expect("detached compaction reached delay hook");
    janitor_release.notify_waiters();
    assert!(
        janitor
            .await
            .expect("janitor task")
            .expect("janitor result")
            .is_empty()
    );
    assert!(fixture.engine.inner.store.is_resident(child.session_id));
    assert!(
        fixture
            .engine
            .compaction_reserved_for_test(child.session_id)
    );
    compaction_release.notify_waiters();
    let compacted = with_watchdog("compaction reply", compaction)
        .await
        .expect("compaction task")
        .expect("compaction succeeds");
    assert!(compacted.compacted);
    let requests = with_watchdog("delegation and compaction server", captured)
        .await
        .expect("scripted server");
    assert_eq!(requests.len(), 4);
    assert!(
        !fixture
            .engine
            .compaction_reserved_for_test(child.session_id)
    );
    let child_tip = fixture
        .engine
        .inner
        .store
        .get(child.session_id)
        .expect("child before fork")
        .meta
        .last_event_seq;
    let fork = fixture
        .engine
        .fork_session(
            child.session_id,
            child_tip,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("fork terminal delegated child");
    assert!(fixture.engine.inner.store.is_resident(fork.session_id));
    assert!(!fixture.engine.actor_resident_for_test(fork.session_id));
    assert!(matches!(
        fixture
            .engine
            .inner
            .store
            .get(fork.session_id)
            .expect("delegated fork projection")
            .meta
            .origin,
        cookie_agent_protocol::SessionOrigin::Delegated { .. }
    ));
    let evicted = fixture
        .engine
        .evict_idle_subagents_for_test(0, std::time::Duration::ZERO)
        .await
        .expect("foreground child and delegated fork paging");
    assert_eq!(evicted.len(), 2);
    assert!(evicted.contains(&child.session_id));
    assert!(evicted.contains(&fork.session_id));
    assert!(!fixture.engine.inner.store.is_resident(child.session_id));
    assert!(!fixture.engine.inner.store.is_resident(fork.session_id));
    assert!(!fixture.engine.actor_resident_for_test(fork.session_id));
    assert!(fixture.engine.inner.store.is_resident(parent.session_id));
    let entries = fixture.engine.inner.delegation_events.entries();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].started);
    assert!(entries[0].child_run_id.is_some());
    fixture.engine.shutdown().await;

    let reopened = reopen_engine(&fixture);
    assert_eq!(
        reopened
            .get_session(parent.session_id)
            .expect("reopened parent")
            .status,
        cookie_agent_protocol::SessionStatus::Completed
    );
    assert_eq!(
        reopened
            .get_session(child.session_id)
            .expect("reopened child")
            .status,
        cookie_agent_protocol::SessionStatus::Completed
    );
    assert_eq!(reopened.inner.delegation_events.entries().len(), 1);
    reopened.shutdown().await;
}

#[tokio::test]
async fn subagent_residency_pages_oldest_idle_and_reopens_transparently() {
    let (endpoint, responses, server) = scripted_channel_server(12).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("paging parent");
    let mut children = Vec::new();

    for index in 0..3 {
        let parent_input = format!("create paging child {index}");
        let child_prompt = format!("paging child task {index}");
        await_projection(
            &fixture.engine,
            parent.session_id,
            "parent producer deliveries settled before next paging run",
            |parent| {
                parent.status != SessionStatus::Running
                    && crate::goal_projection::GoalProducerProjection::from_events(
                        &parent.log.event_snapshot(),
                    )
                    .messages
                    .iter()
                    .all(|message| message.consumed || message.discarded)
            },
        )
        .await;
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                &parent_input,
                scripted_tool_body(
                    &format!("paging-delegate-{index}"),
                    "delegate_subagent",
                    serde_json::json!({
                        "agent_type":"worker",
                        "description":format!("Paging child {index}"),
                        "prompt":child_prompt,
                        "background":true
                    }),
                ),
            ))
            .expect("paging tool response");
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                &child_prompt,
                scripted_text_body(&format!("paging child result {index}")),
            ))
            .expect("paging child response");
        responses
            .send(MatchedScriptedResponse::last_message_role(
                "tool",
                scripted_text_body(&format!("parent after paging child {index}")),
            ))
            .expect("paging parent response");
        fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: parent.session_id,
                    client_run_id: ClientRunId::new(format!("paging-parent-{index}"))
                        .expect("run ID"),
                    selection: selection.clone(),
                    input: parent_input,
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect("paging parent run");
        let child_id = await_session_change(
            &fixture.engine,
            parent.session_id,
            "paging child completion and teaser",
            || {
                let known = fixture
                    .engine
                    .children(parent.session_id)
                    .expect("children");
                if let Some(child) = known.iter().find(|child| {
                    child.status == SessionStatus::Completed
                        && !children.contains(&child.session_id)
                }) && fixture
                    .engine
                    .get_session(parent.session_id)
                    .is_ok_and(|parent| parent.status == SessionStatus::Completed)
                {
                    let notified = fixture
                        .engine
                        .inner
                        .store
                        .get(parent.session_id)
                        .expect("paging parent projection")
                        .log
                        .events()
                        .iter()
                        .any(|event| {
                            matches!(
                                event.payload,
                                EventPayload::DelegateFinishedV2 { session_id, .. }
                                    if session_id == child.session_id
                            )
                        });
                    if notified {
                        return Some(child.session_id);
                    }
                }
                None
            },
        )
        .await;
        children.push(child_id);
    }

    assert_eq!(fixture.engine.inner.store.resident_subagent_count(), 3);
    assert!(
        fixture
            .engine
            .evict_idle_subagents_for_test(0, std::time::Duration::from_secs(60 * 60))
            .await
            .expect("recent soft-cap pass")
            .is_empty()
    );
    assert!(
        fixture
            .engine
            .evict_idle_subagents_for_test(3, std::time::Duration::ZERO)
            .await
            .expect("under-cap pass")
            .is_empty()
    );
    let (transition_reached, release_transition) = fixture
        .engine
        .inner
        .store
        .install_eviction_transition_hook_for_test();
    let janitor_engine = fixture.engine.clone();
    let runtime = tokio::runtime::Handle::current();
    let janitor = tokio::task::spawn_blocking(move || {
        runtime.block_on(janitor_engine.evict_idle_subagents_for_test(1, std::time::Duration::ZERO))
    });
    let transitioning = tokio::time::timeout(std::time::Duration::from_secs(1), transition_reached)
        .await
        .expect("eviction transition hook timeout")
        .expect("eviction transition reached hook");

    let parent_session_id = parent.session_id;
    let (list_ready, list_started) = tokio::sync::oneshot::channel();
    let list_engine = fixture.engine.clone();
    let list_task = tokio::task::spawn_blocking(move || {
        let _ = list_ready.send(());
        list_engine.list_sessions()
    });
    let (children_ready, children_started) = tokio::sync::oneshot::channel();
    let children_engine = fixture.engine.clone();
    let children_task = tokio::task::spawn_blocking(move || {
        let _ = children_ready.send(());
        children_engine
            .children(parent_session_id)
            .expect("children")
    });
    let (tree_ready, tree_started) = tokio::sync::oneshot::channel();
    let tree_engine = fixture.engine.clone();
    let tree_task = tokio::task::spawn_blocking(move || {
        let _ = tree_ready.send(());
        tree_engine.tree(parent_session_id)
    });
    let (list_started, children_started, tree_started) =
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(list_started, children_started, tree_started)
        })
        .await
        .expect("listing calls did not start");
    list_started.expect("list call started");
    children_started.expect("children call started");
    tree_started.expect("tree call started");
    // A short real-time window is intentional: these are negative assertions
    // that the synchronous readers remain blocked by the transition lock.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(!list_task.is_finished());
    assert!(!children_task.is_finished());
    assert!(!tree_task.is_finished());
    release_transition
        .send(())
        .expect("release eviction transition");

    let evicted = tokio::time::timeout(std::time::Duration::from_secs(2), janitor)
        .await
        .expect("transition janitor timeout")
        .expect("transition janitor task")
        .expect("oldest-idle paging");
    let listed = tokio::time::timeout(std::time::Duration::from_secs(2), list_task)
        .await
        .expect("list task timeout")
        .expect("list task");
    let listed_children = tokio::time::timeout(std::time::Duration::from_secs(2), children_task)
        .await
        .expect("children task timeout")
        .expect("children task");
    let listed_tree = tokio::time::timeout(std::time::Duration::from_secs(2), tree_task)
        .await
        .expect("tree task timeout")
        .expect("tree task")
        .expect("session tree");
    assert!(
        listed
            .iter()
            .any(|session| session.session_id == parent_session_id)
    );
    assert!(
        listed
            .iter()
            .all(|session| session.session_id != transitioning)
    );
    assert!(
        listed_children
            .iter()
            .any(|child| child.session_id == transitioning)
    );
    assert!(
        listed_tree
            .children
            .iter()
            .any(|child| child.session.session_id == transitioning)
    );
    assert_eq!(evicted, children[..2]);
    assert!(!fixture.engine.inner.store.is_resident(children[0]));
    assert!(!fixture.engine.inner.store.is_resident(children[1]));
    assert!(fixture.engine.inner.store.is_resident(children[2]));
    assert!(!fixture.engine.actor_resident_for_test(children[0]));
    assert!(fixture.engine.inner.store.is_resident(parent.session_id));
    assert_eq!(fixture.engine.list_sessions().len(), 1);
    assert_eq!(
        fixture
            .engine
            .children(parent.session_id)
            .expect("children")
            .len(),
        3
    );

    let result = fixture
        .engine
        .get_subagent_result(
            parent.session_id,
            children[0],
            false,
            0,
            20,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("get reopens evicted child");
    assert!(result.output.contains("paging child result 0"));
    assert!(fixture.engine.inner.store.is_resident(children[0]));
    assert!(!fixture.engine.actor_resident_for_test(children[0]));

    assert_eq!(
        fixture
            .engine
            .evict_idle_subagents_for_test(1, std::time::Duration::ZERO)
            .await
            .expect("evict before synchronized read-only reopen"),
        [children[0]]
    );
    let (read_reopened, release_read) = fixture.engine.install_read_only_reopen_hook_for_test();
    let read_engine = fixture.engine.clone();
    let read_child = children[0];
    let read = tokio::task::spawn_blocking(move || read_engine.get_session(read_child));
    tokio::time::timeout(std::time::Duration::from_secs(1), read_reopened)
        .await
        .expect("read-only reopen hook timeout")
        .expect("read-only reopen reached hook");
    assert!(fixture.engine.inner.store.is_resident(children[0]));
    assert_eq!(
        fixture
            .engine
            .evict_idle_subagents_for_test(1, std::time::Duration::ZERO)
            .await
            .expect("evict during synchronized read-only reopen"),
        [children[0]]
    );
    assert!(!fixture.engine.inner.store.is_resident(children[0]));
    release_read.send(()).expect("release read-only reopen");
    read.await
        .expect("concurrent read task")
        .expect("concurrent read-only reopen");
    assert!(!fixture.engine.actor_resident_for_test(children[0]));
    assert!(!fixture.engine.inner.store.is_resident(children[0]));
    fixture
        .engine
        .get_subagent_result(
            parent.session_id,
            children[0],
            false,
            0,
            20,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("restore child after concurrent read and janitor");
    assert!(fixture.engine.inner.store.is_resident(children[0]));
    assert!(!fixture.engine.actor_resident_for_test(children[0]));

    let subscription_cursor = fixture
        .engine
        .inner
        .store
        .get(children[0])
        .expect("subscribed child projection")
        .meta
        .last_event_seq;
    let (initial, mut eviction_events) = fixture
        .engine
        .subscribe(children[0], Some(subscription_cursor))
        .await
        .expect("subscribe before paging");
    assert!(initial.events.is_empty());
    fixture
        .engine
        .append(
            children[0],
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionTitleCommitted {
                input_through_seq: subscription_cursor,
                change: SessionTitleChange::UserSet {
                    title: SessionTitle::new("Buffered before paging").expect("buffered title"),
                    client_rename_id: ClientRenameId::new("buffered-before-paging")
                        .expect("buffered rename ID"),
                },
            },
        )
        .await
        .expect("queue unread event before paging");
    let buffered_seq = fixture
        .engine
        .inner
        .store
        .get(children[0])
        .expect("buffered child projection")
        .meta
        .last_event_seq;
    assert_eq!(
        fixture
            .engine
            .evict_idle_subagents_for_test(1, std::time::Duration::ZERO)
            .await
            .expect("subscribed child paging"),
        [children[0]]
    );
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), eviction_events.recv())
            .await
            .expect("buffered event timeout")
            .expect("buffered event"),
        cookie_agent_protocol::EventSubscriptionMessage::Event { event }
            if event.seq == buffered_seq
                && matches!(
                    &event.payload,
                    EventPayload::SessionTitleCommitted {
                        change: SessionTitleChange::UserSet { title, .. },
                        ..
                    } if title.as_str() == "Buffered before paging"
                )
    ));
    let gap_cursor =
        match tokio::time::timeout(std::time::Duration::from_secs(1), eviction_events.recv())
            .await
            .expect("eviction gap timeout")
            .expect("eviction gap")
        {
            cookie_agent_protocol::EventSubscriptionMessage::Gap {
                session_id,
                last_delivered_seq,
            } => {
                assert_eq!(session_id, children[0]);
                assert_eq!(last_delivered_seq, buffered_seq);
                last_delivered_seq
            }
            message => panic!("expected eviction gap, got {message:?}"),
        };
    assert!(!fixture.engine.actor_resident_for_test(children[0]));
    fixture
        .engine
        .append(
            children[0],
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionTitleCommitted {
                input_through_seq: gap_cursor,
                change: SessionTitleChange::UserSet {
                    title: SessionTitle::new("Replay after paging").expect("replay title"),
                    client_rename_id: ClientRenameId::new("replay-after-paging")
                        .expect("replay rename ID"),
                },
            },
        )
        .await
        .expect("append after paging");
    let (replay, mut reopened_events) = fixture
        .engine
        .subscribe(children[0], Some(gap_cursor))
        .await
        .expect("resubscribe after paging");
    let replay_sequences = replay
        .events
        .iter()
        .map(|event| event.seq)
        .collect::<Vec<_>>();
    assert_eq!(replay_sequences, [gap_cursor + 1]);
    assert!(matches!(
        &replay.events[0].payload,
        EventPayload::SessionTitleCommitted {
            change: SessionTitleChange::UserSet { title, .. },
            ..
        } if title.as_str() == "Replay after paging"
    ));
    let replay_seq = replay.events[0].seq;
    fixture
        .engine
        .append(
            children[0],
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionTitleCommitted {
                input_through_seq: replay_seq,
                change: SessionTitleChange::UserSet {
                    title: SessionTitle::new("Live after paging").expect("live title"),
                    client_rename_id: ClientRenameId::new("live-after-paging")
                        .expect("live rename ID"),
                },
            },
        )
        .await
        .expect("live append after paging");
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), reopened_events.recv())
            .await
            .expect("live event timeout")
            .expect("live event"),
        cookie_agent_protocol::EventSubscriptionMessage::Event { event }
            if event.seq == replay_seq + 1
                && matches!(
                    &event.payload,
                    EventPayload::SessionTitleCommitted {
                        change: SessionTitleChange::UserSet { title, .. },
                        ..
                    } if title.as_str() == "Live after paging"
                )
    ));
    assert!(matches!(
        reopened_events.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));

    fixture
        .engine
        .append(
            children[0],
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::UserInputAdmitted {
                input: "queued steer must stay resident".into(),
            },
        )
        .await
        .expect("pending steer marker");
    let pending_evictions = fixture
        .engine
        .evict_idle_subagents_for_test(0, std::time::Duration::ZERO)
        .await
        .expect("pending-input exclusion");
    assert!(!pending_evictions.contains(&children[0]));
    assert!(fixture.engine.inner.store.is_resident(children[0]));
    fixture
        .engine
        .append(
            children[0],
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::UserInputRecalled {
                input: "queued steer must stay resident".into(),
            },
        )
        .await
        .expect("clear pending steer marker");

    fixture
        .engine
        .set_delegation_queued_for_test(children[0], true);
    assert!(
        fixture
            .engine
            .evict_idle_subagents_for_test(0, std::time::Duration::ZERO)
            .await
            .expect("queued exclusion")
            .is_empty()
    );
    fixture
        .engine
        .set_delegation_queued_for_test(children[0], false);
    fixture
        .engine
        .set_notification_sent_for_test(children[0], false);
    assert!(
        fixture
            .engine
            .evict_idle_subagents_for_test(0, std::time::Duration::ZERO)
            .await
            .expect("teaser exclusion")
            .is_empty()
    );
    fixture
        .engine
        .set_notification_sent_for_test(children[0], true);

    let (approval_sender, _approval_receiver) = tokio::sync::oneshot::channel();
    let pending_approval_id = ApprovalId::new_v7();
    fixture
        .engine
        .inner
        .pending_approvals
        .lock()
        .expect("pending approval lock")
        .insert(
            (children[0], pending_approval_id),
            crate::runtime::PendingApproval {
                sender: approval_sender,
                executor: Arc::new(tokio::sync::Mutex::new(None)),
                permission_overlay_epoch: 0,
            },
        );
    assert!(
        fixture
            .engine
            .evict_idle_subagents_for_test(0, std::time::Duration::ZERO)
            .await
            .expect("approval exclusion")
            .is_empty()
    );
    fixture
        .engine
        .inner
        .pending_approvals
        .lock()
        .expect("pending approval lock")
        .remove(&(children[0], pending_approval_id));

    assert_eq!(
        fixture
            .engine
            .evict_idle_subagents_for_test(0, std::time::Duration::ZERO)
            .await
            .expect("evict before steer"),
        [children[0]]
    );
    fixture
        .engine
        .steer_subagent(
            parent.session_id,
            children[0],
            "cannot steer a terminal child".into(),
        )
        .await
        .expect_err("terminal steer is rejected after transparent reopen");
    assert!(fixture.engine.inner.store.is_resident(children[0]));
    fixture
        .engine
        .evict_idle_subagents_for_test(0, std::time::Duration::ZERO)
        .await
        .expect("evict before resume");

    await_projection(
        &fixture.engine,
        parent.session_id,
        "parent producer deliveries settled before explicit resume",
        |parent| {
            parent.status != SessionStatus::Running
                && crate::goal_projection::GoalProducerProjection::from_events(
                    &parent.log.event_snapshot(),
                )
                .messages
                .iter()
                .all(|message| message.consumed || message.discarded)
        },
    )
    .await;

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "resume the evicted paging child",
            scripted_tool_body(
                "paging-resume",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type":"worker",
                    "description":"Resume paged child",
                    "prompt":"resumed paging task",
                    "background":true,
                    "resume_session_id":children[0]
                }),
            ),
        ))
        .expect("paging resume response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("parent after paging resume"),
        ))
        .expect("paging resume parent response");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("paging-resume-parent").expect("run ID"),
                selection,
                input: "resume the evicted paging child".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("paging resume parent run");
    await_projection(
        &fixture.engine,
        children[0],
        "resumed child running",
        |child| {
            child.runs.len() == 2
                && child
                    .runs
                    .values()
                    .any(|run| run.status == SessionStatus::Running)
        },
    )
    .await;
    assert!(
        fixture
            .engine
            .evict_idle_subagents_for_test(0, std::time::Duration::ZERO)
            .await
            .expect("running exclusion")
            .is_empty()
    );
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "resumed paging task",
            scripted_text_body("resumed paging result"),
        ))
        .expect("release resumed child");
    await_projection(
        &fixture.engine,
        children[0],
        "resumed paging child completion",
        |child| child.runs.len() == 2 && child.status == SessionStatus::Completed,
    )
    .await;
    assert_eq!(
        with_watchdog("server fixture completion", server)
            .await
            .expect("paging server")
            .len(),
        12
    );
    fixture.engine.shutdown().await;
}
