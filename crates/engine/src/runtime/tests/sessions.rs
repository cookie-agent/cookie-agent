use std::{collections::BTreeSet, fs, sync::Arc};

use cookie_agent_protocol::{
    AgentId, ClientRenameId, ClientRunId, EventPayload, EventSubscriptionMessage, ModelSelection,
    PermissionMode, RunSelection, RunStartParams, SessionTitle, SessionTitleChange,
};

use crate::EngineError;

use super::support::*;

#[tokio::test]
async fn direct_store_appends_share_the_subscription_handoff() {
    let (fixture, selection) = custom_fixture();
    let session = fixture.engine.create_session(selection).expect("session");
    let session_id = session.session_id;
    let cursor = session.last_event_seq;
    let gate = Arc::new(std::sync::Barrier::new(2));
    let writer_gate = Arc::clone(&gate);
    let writer_store = Arc::clone(&fixture.engine.inner.store);
    let writer = tokio::task::spawn_blocking(move || {
        writer_gate.wait();
        for index in 0..32 {
            writer_store
                .append(
                    session_id,
                    None,
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                    EventPayload::UserInputAdmitted {
                        input: format!("input {index}"),
                    },
                )
                .expect("direct append");
        }
    });
    let store = Arc::clone(&fixture.engine.inner.store);
    let subscriber = tokio::task::spawn_blocking(move || {
        gate.wait();
        store
            .subscribe_events(session_id, Some(cursor))
            .expect("subscribe")
    });
    let (snapshot, mut live) = with_watchdog("subscription handoff", subscriber)
        .await
        .unwrap();
    with_watchdog("direct writer", writer).await.unwrap();
    let mut events = snapshot.events;
    with_watchdog("direct append delivery", async {
        while events.len() < 32 {
            match live.recv().await.expect("live subscription") {
                EventSubscriptionMessage::Event { event } => events.push(*event),
                EventSubscriptionMessage::Gap { .. } => panic!("unexpected gap"),
            }
        }
    })
    .await;
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.seq, cursor + index as u64 + 1);
        assert!(
            matches!(&event.payload, EventPayload::UserInputAdmitted { input }
            if input == &format!("input {index}"))
        );
    }
    // Actor appends use the same publication path and must arrive exactly once.
    fixture
        .engine
        .append(
            session_id,
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::UserInputAdmitted {
                input: "actor append".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(with_watchdog("actor append delivery", live.recv()).await,
        Some(EventSubscriptionMessage::Event { event }) if event.seq == cursor + 33)
    );
    assert!(matches!(
        live.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn shutdown_is_idempotent_and_blocks_ownership_reacquisition() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .inner
        .store
        .persist_buffered_session(session.session_id)
        .expect("persist session");
    let retained = fixture.engine.clone();

    fixture.engine.shutdown().await;
    retained.shutdown().await;

    assert!(matches!(
        retained.ensure_session_owned(session.session_id),
        Err(EngineError::ActorStopped)
    ));
    assert!(matches!(
        retained.spawn_actor(session.session_id),
        Err(EngineError::ActorStopped)
    ));
    assert!(matches!(
        retained.create_session(selection),
        Err(EngineError::Session(
            crate::session::SessionError::StoreClosed
        ))
    ));
    assert!(!retained.inner.store.is_owned(session.session_id));
    assert!(!retained.actor_resident_for_test(session.session_id));

    let reopened = reopen_engine(&fixture);
    reopened
        .ensure_session_owned(session.session_id)
        .expect("new engine adopts released session");
    reopened.shutdown().await;
}

/// Ownership is the tree's, not the session's: while one engine holds a tree,
/// a second engine may write nothing in it — root or delegated child. Once the
/// holder is gone, adopting the child takes the tree, and the root then becomes
/// writable through its own adoption, without a second lock.
#[tokio::test]
async fn a_second_engine_writes_no_session_of_a_held_tree() {
    let (fixture, selection) = custom_fixture();
    let root = fixture
        .engine
        .create_session(selection)
        .expect("root session");
    let root_id = root.session_id;
    fixture
        .engine
        .inner
        .store
        .persist_buffered_session(root_id)
        .expect("persist root");
    let child = create_buffered_delegated_child(&fixture.engine, root_id);
    fixture
        .engine
        .inner
        .store
        .persist_buffered_session(child)
        .expect("persist child");

    let engine_b = reopen_engine(&fixture);
    for id in [child, root_id] {
        assert!(matches!(
            engine_b.ensure_session_owned(id),
            Err(EngineError::SessionOwnedByAnotherProcess(locked)) if locked == id
        ));
    }

    fixture.engine.shutdown().await;

    engine_b
        .ensure_session_owned(child)
        .expect("adopt the child of a released tree");
    assert!(engine_b.inner.store.is_owned(child));
    assert!(
        !engine_b.inner.store.is_owned(root_id),
        "taking the tree through a child does not adopt the root"
    );
    engine_b
        .ensure_session_owned(root_id)
        .expect("adopt the root of a tree this engine already holds");
    assert!(engine_b.inner.store.is_owned(root_id));
    engine_b.shutdown().await;
}

#[tokio::test]
async fn two_engines_share_a_data_dir_without_sharing_session_writers() {
    let (fixture, selection) = custom_fixture();
    let session_a = fixture
        .engine
        .create_session(selection.clone())
        .expect("engine A session");
    fixture
        .engine
        .inner
        .store
        .persist_buffered_session(session_a.session_id)
        .expect("persist engine A session");

    let engine_b = reopen_engine(&fixture);
    assert_eq!(
        engine_b
            .get_session(session_a.session_id)
            .expect("foreign snapshot")
            .session_id,
        session_a.session_id
    );
    assert!(matches!(
        engine_b.subscribe(session_a.session_id, None).await,
        Err(EngineError::SessionOwnedByAnotherProcess(id)) if id == session_a.session_id
    ));
    assert!(matches!(
        engine_b.resume(session_a.session_id).await,
        Err(EngineError::SessionOwnedByAnotherProcess(id)) if id == session_a.session_id
    ));

    let session_b = engine_b
        .create_session(selection)
        .expect("engine B session");
    engine_b
        .inner
        .store
        .persist_buffered_session(session_b.session_id)
        .expect("persist engine B session");
    assert!(matches!(
        fixture.engine.resume(session_b.session_id).await,
        Err(EngineError::SessionOwnedByAnotherProcess(id)) if id == session_b.session_id
    ));

    let barrier = Arc::new(std::sync::Barrier::new(3));
    let writers = [
        (fixture.engine.clone(), session_a.session_id, "engine-a"),
        (engine_b.clone(), session_b.session_id, "engine-b"),
    ]
    .map(|(engine, session_id, message)| {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            engine.inner.store.append(
                session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::PluginDiagnostic {
                    plugin: "ownership-test".into(),
                    kind: cookie_agent_protocol::PluginDiagnosticKind::HookBlocked,
                    message: message.into(),
                    count: 1,
                },
            )
        })
    });
    barrier.wait();
    for writer in writers {
        writer
            .join()
            .expect("ownership writer thread")
            .expect("owned append");
    }
    for (engine, session_id) in [
        (&fixture.engine, session_a.session_id),
        (&engine_b, session_b.session_id),
    ] {
        let events = engine
            .inner
            .store
            .get(session_id)
            .expect("owned projection")
            .log
            .all_events();
        assert!(events.iter().all(|event| event.session_id == session_id));
        assert!(
            events
                .windows(2)
                .all(|events| events[1].seq == events[0].seq + 1)
        );
    }

    engine_b.shutdown().await;
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn session_metadata_tracks_log_tail_for_create_get_list_tree_and_append() {
    let (fixture, selection) = custom_fixture();
    let created = fixture
        .engine
        .create_session(selection.clone())
        .expect("create session");
    let creation_event = fixture
        .engine
        .inner
        .store
        .get(created.session_id)
        .expect("created projection")
        .log
        .last_event()
        .expect("creation event");
    assert_eq!(created.last_activity, creation_event.timestamp);
    assert_eq!(
        fixture
            .engine
            .get_session(created.session_id)
            .expect("get session")
            .last_activity,
        creation_event.timestamp
    );
    assert_eq!(
        fixture
            .engine
            .list_sessions()
            .into_iter()
            .find(|session| session.session_id == created.session_id)
            .expect("listed session")
            .last_activity,
        creation_event.timestamp
    );
    assert_eq!(
        fixture
            .engine
            .tree(created.session_id)
            .expect("session tree")
            .session
            .last_activity,
        creation_event.timestamp
    );

    fixture
        .engine
        .append_direct(
            created.session_id,
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionTitleCommitted {
                input_through_seq: creation_event.seq,
                change: SessionTitleChange::UserSet {
                    title: SessionTitle::new("Latest activity").expect("title"),
                    client_rename_id: ClientRenameId::new("latest-activity").expect("rename ID"),
                },
            },
        )
        .expect("append event");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: created.session_id,
                client_run_id: ClientRunId::new("metadata-persist").expect("client run ID"),
                selection,
                input: "persist session".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("persist session");
    wait_for_session_not_running(&fixture.engine, created.session_id).await;
    let latest_event = fixture
        .engine
        .inner
        .store
        .get(created.session_id)
        .expect("updated projection")
        .log
        .last_event()
        .expect("latest event");
    assert_eq!(
        fixture
            .engine
            .get_session(created.session_id)
            .expect("updated session")
            .last_activity,
        latest_event.timestamp
    );
    assert_eq!(
        fixture
            .engine
            .list_sessions()
            .into_iter()
            .find(|session| session.session_id == created.session_id)
            .expect("updated listed session")
            .last_activity,
        latest_event.timestamp
    );
    assert_eq!(
        fixture
            .engine
            .tree(created.session_id)
            .expect("updated tree")
            .session
            .last_activity,
        latest_event.timestamp
    );

    let reopened = reopen_engine(&fixture);
    assert_eq!(
        reopened
            .get_session(created.session_id)
            .expect("replayed session")
            .last_activity,
        latest_event.timestamp
    );
}

#[tokio::test]
async fn unreadable_session_metadata_cache_is_rebuilt_from_events() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("create session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("metadata-cache-persist").expect("client run ID"),
                selection,
                input: "persist session".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("persist session");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let path = fixture.engine.inner.store.session_dir(session.session_id);
    let path = crate::session::meta_path(&path);
    let expected = fixture
        .engine
        .get_session(session.session_id)
        .expect("projected metadata");
    fs::write(&path, b"not a metadata cache").expect("write unreadable metadata cache");

    let reopened = crate::session::SessionStore::open(
        &fixture._directory.path().join("data"),
        fixture._directory.path(),
    )
    .expect("unreadable cache is rebuildable");
    let rebuilt = reopened
        .get(session.session_id)
        .expect("rebuilt session metadata")
        .metadata();
    assert_eq!(rebuilt.session_id, expected.session_id);
    assert_eq!(rebuilt.last_event_seq, expected.last_event_seq);
    assert_eq!(rebuilt.status, expected.status);
}

#[test]
fn empty_session_is_live_only_and_disappears_on_restart_without_artifacts() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection)
        .expect("create session");
    let session_dir = fixture.engine.inner.store.session_dir(session.session_id);

    assert!(!session_dir.exists());
    assert!(
        fixture
            .engine
            .list_sessions()
            .iter()
            .any(|listed| listed.session_id == session.session_id)
    );
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Ask)
        .expect("set memory-only permission mode");
    fixture
        .engine
        .append_direct(
            session.session_id,
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionTitleCommitted {
                input_through_seq: 1,
                change: SessionTitleChange::UserSet {
                    title: SessionTitle::new("Memory-only title").expect("title"),
                    client_rename_id: ClientRenameId::new("memory-only-title").expect("rename ID"),
                },
            },
        )
        .expect("append memory-only title");
    assert!(!session_dir.exists());

    let reopened = reopen_engine(&fixture);
    assert!(
        !reopened
            .list_sessions()
            .iter()
            .any(|listed| listed.session_id == session.session_id)
    );
    assert!(reopened.get_session(session.session_id).is_err());
    assert!(!session_dir.exists());
}

#[tokio::test]
async fn list_sessions_returns_only_root_sessions() {
    let (fixture, selection) = custom_fixture();
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("root session");
    let sibling = fixture
        .engine
        .create_session(selection)
        .expect("second root session");
    let child = create_buffered_delegated_child(&fixture.engine, parent.session_id);

    assert_eq!(
        fixture
            .engine
            .list_sessions()
            .into_iter()
            .map(|session| session.session_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([parent.session_id, sibling.session_id])
    );
    assert!(
        fixture
            .engine
            .inner
            .store
            .tree_summaries(parent.session_id)
            .expect("parent tree")
            .iter()
            .any(|summary| summary.meta.session_id == child)
    );

    // Filtering happens at the listing boundary only: the delegated child
    // stays resident and addressable through the per-session APIs.
    assert!(fixture.engine.inner.store.is_resident(child));
    let fetched = fixture
        .engine
        .get_session(child)
        .expect("delegated child stays addressable");
    assert_eq!(fetched.session_id, child);
    assert!(matches!(
        fetched.origin,
        cookie_agent_protocol::SessionOrigin::Delegated { .. }
    ));
    assert!(fixture.engine.tree(parent.session_id).is_ok());
}

#[tokio::test]
async fn first_user_message_flushes_complete_ordered_buffer_and_replays_exactly() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("create session");
    let session_dir = fixture.engine.inner.store.session_dir(session.session_id);
    assert!(!session_dir.exists());

    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("first-persist").expect("client run ID"),
                selection,
                input: "first user message".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("start run");

    assert!(crate::session::meta_path(&session_dir).is_file());
    assert!(session_dir.join("events.jsonl").is_file());
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let memory_events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("live session")
        .log
        .events();
    let disk_events = crate::events::load_jsonl::<cookie_agent_protocol::StoredEvent>(
        &session_dir.join("events.jsonl"),
    )
    .expect("disk events");
    assert_eq!(disk_events, memory_events);
    assert!(matches!(
        disk_events[0].payload,
        EventPayload::SessionCreated { .. }
    ));
    assert!(matches!(
        disk_events[1].payload,
        EventPayload::RunStarted { .. }
    ));
    assert!(matches!(
        disk_events[2].payload,
        EventPayload::UserInputSubmitted { .. }
    ));
    assert!(
        disk_events
            .iter()
            .enumerate()
            .all(|(index, event)| event.seq == index as u64 + 1)
    );

    let reopened = reopen_engine(&fixture);
    assert_eq!(
        reopened
            .inner
            .store
            .get(session.session_id)
            .expect("replayed session")
            .log
            .events(),
        memory_events
    );
}

#[test]
fn empty_startup_is_coherent_and_rejects_fabricated_sessions() {
    let fixture = fixture();
    let snapshot = fixture
        .engine
        .runtime_snapshot()
        .expect("runtime snapshot")
        .snapshot;
    assert!(snapshot.providers.is_empty());
    assert!(snapshot.models.is_empty());
    assert_eq!(
        snapshot
            .agents
            .iter()
            .filter(|agent| agent.mode == cookie_agent_protocol::AgentMode::Internal)
            .count(),
        3
    );
    assert!(!snapshot.agents.iter().any(|agent| agent.runnable_as_root));
    let selection = RunSelection {
        agent: AgentId::new("primary").expect("agent ID"),
        model: ModelSelection {
            model: "openai/model".parse().expect("model key"),
            variant: None,
        },
        preset: None,
    };
    assert!(matches!(
        fixture.engine.create_session(selection),
        Err(EngineError::NoRunnableModel)
    ));
}

#[tokio::test]
async fn revert_and_fork_preserve_prefix_context_replay_and_independence() {
    let response = |text: &str| {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n"
        )
    };
    let (endpoint, captured, second_request_reached, release_second) =
        scripted_server_with_delayed_response(
            vec![
                response("first answer"),
                response("second answer"),
                response("branch answer"),
            ],
            1,
        )
        .await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("source session");

    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("revert-first").expect("client run ID"),
                selection: selection.clone(),
                input: "first input".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("first run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let through_seq = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("first projection")
        .log
        .all_events()
        .last()
        .expect("first tip")
        .seq;

    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("revert-second").expect("client run ID"),
                selection: selection.clone(),
                input: "second input must disappear".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("second run");
    second_request_reached
        .await
        .expect("second request reached");
    assert!(matches!(
        fixture
            .engine
            .revert_session(session.session_id, through_seq, cookie_agent_protocol::EventOrigin::new("client:test").unwrap())
            .await,
        Err(EngineError::SessionRunning(id)) if id == session.session_id
    ));
    let fork = fixture
        .engine
        .fork_session(
            session.session_id,
            through_seq,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("fork active source");
    let (artifact, digest) = fixture
        .engine
        .inner
        .artifacts
        .retain(session.session_id, b"fork-shared-artifact")
        .expect("retain shared artifact");
    assert_eq!(artifact.uri, format!("artifact://sha256/{digest}"));
    assert!(
        fixture
            .engine
            .inner
            .artifacts
            .open_existing(session.session_id, &digest)
            .expect("resolve shared artifact")
            .is_some()
    );
    let source_prefix = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("source prefix")
        .log
        .all_events()
        .into_iter()
        .filter(|event| event.seq <= through_seq)
        .collect::<Vec<_>>();
    let fork_physical = fixture
        .engine
        .inner
        .store
        .get(fork.session_id)
        .expect("fork projection")
        .log
        .all_events();
    assert_eq!(fork_physical.len(), source_prefix.len() + 2);
    for (source_event, fork_event) in source_prefix.iter().zip(&fork_physical) {
        assert_eq!(fork_event.session_id, fork.session_id);
        assert_eq!(fork_event.engine_version, source_event.engine_version);
        assert_eq!(fork_event.run_id, source_event.run_id);
        assert_eq!(fork_event.seq, source_event.seq);
        assert_eq!(fork_event.timestamp, source_event.timestamp);
        assert_eq!(fork_event.payload, source_event.payload);
    }
    assert!(matches!(
        fork_physical[source_prefix.len()].payload,
        EventPayload::SessionReverted { through_seq: target } if target == through_seq
    ));
    assert!(matches!(
        fork_physical[source_prefix.len() + 1].payload,
        EventPayload::SessionTitleCommitted { .. }
    ));
    release_second.notify_one();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let reverted = fixture
        .engine
        .revert_session(
            session.session_id,
            through_seq,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("revert completed source");
    let first_revert_event = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("reverted source")
        .log
        .all_events()
        .last()
        .expect("revert tip")
        .clone();
    assert!(matches!(
        first_revert_event.payload,
        EventPayload::SessionReverted { through_seq: target } if target == through_seq
    ));
    assert_eq!(reverted.session.last_event_seq, first_revert_event.seq);
    assert_eq!(reverted.session.last_activity, first_revert_event.timestamp);
    let first_revert_tip = first_revert_event.seq;
    fixture
        .engine
        .rename_session(
            cookie_agent_protocol::SessionRenameParams {
                session_id: session.session_id,
                client_rename_id: ClientRenameId::new("branch-title").expect("rename ID"),
                change: cookie_agent_protocol::SessionRenameChange::Set {
                    title: SessionTitle::new("temporary branch").expect("title"),
                },
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("branch title");
    fixture
        .engine
        .revert_session(
            session.session_id,
            first_revert_tip,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("stacked revert");
    let fork_after_revert = fixture
        .engine
        .fork_session(
            session.session_id,
            through_seq,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("fork reverted source at original boundary");
    let first_fork_prefix = fixture
        .engine
        .inner
        .store
        .get(fork.session_id)
        .expect("first fork")
        .log
        .all_events()
        .into_iter()
        .filter(|event| event.seq <= through_seq)
        .collect::<Vec<_>>();
    let reverted_fork_prefix = fixture
        .engine
        .inner
        .store
        .get(fork_after_revert.session_id)
        .expect("fork after revert")
        .log
        .all_events()
        .into_iter()
        .filter(|event| event.seq <= through_seq)
        .collect::<Vec<_>>();
    assert_eq!(first_fork_prefix.len(), reverted_fork_prefix.len());
    for (first, second) in first_fork_prefix.iter().zip(&reverted_fork_prefix) {
        assert_eq!(first.engine_version, second.engine_version);
        assert_eq!(first.run_id, second.run_id);
        assert_eq!(first.seq, second.seq);
        assert_eq!(first.timestamp, second.timestamp);
        assert_eq!(first.payload, second.payload);
    }
    let visible = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("stacked projection")
        .log
        .events();
    assert!(visible.iter().all(|event| !matches!(
        &event.payload,
        EventPayload::UserInputSubmitted { input } if input == "second input must disappear"
    )));

    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("revert-branch").expect("client run ID"),
                selection,
                input: "branch input".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("branch run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("scripted requests");
    assert_eq!(requests.len(), 3);
    assert!(requests[2].contains("first input"));
    assert!(requests[2].contains("branch input"));
    assert!(!requests[2].contains("second input must disappear"));

    let source_tip_before_fork_rename = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("source")
        .log
        .all_events()
        .len();
    let fork_meta = fixture
        .engine
        .get_session(fork.session_id)
        .expect("fork meta");
    assert!(
        fork_meta
            .title
            .is_some_and(|title| title.as_str().ends_with(" (fork)"))
    );
    fixture
        .engine
        .rename_session(
            cookie_agent_protocol::SessionRenameParams {
                session_id: fork.session_id,
                client_rename_id: ClientRenameId::new("fork-independent").expect("rename ID"),
                change: cookie_agent_protocol::SessionRenameChange::Set {
                    title: SessionTitle::new("independent fork").expect("title"),
                },
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("rename fork");
    assert_eq!(
        fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .expect("unchanged source")
            .log
            .all_events()
            .len(),
        source_tip_before_fork_rename
    );

    fixture.engine.shutdown().await;
    let reopened = reopen_engine(&fixture);
    assert!(
        reopened
            .inner
            .artifacts
            .open_existing(session.session_id, &digest)
            .expect("resolve shared artifact after restart")
            .is_some()
    );
    let reopened_visible = reopened
        .inner
        .store
        .get(session.session_id)
        .expect("reopened source")
        .log
        .events();
    let reopened_physical_tip = reopened
        .inner
        .store
        .get(session.session_id)
        .expect("reopened physical source")
        .log
        .all_events()
        .last()
        .expect("reopened physical tip")
        .clone();
    let reopened_meta = reopened
        .get_session(session.session_id)
        .expect("reopened source metadata");
    assert_eq!(reopened_meta.last_event_seq, reopened_physical_tip.seq);
    assert_eq!(reopened_meta.last_activity, reopened_physical_tip.timestamp);
    assert!(reopened_visible.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputSubmitted { input } if input == "branch input"
    )));
    assert!(reopened_visible.iter().all(|event| !matches!(
        &event.payload,
        EventPayload::UserInputSubmitted { input } if input == "second input must disappear"
    )));
    assert_eq!(
        reopened
            .get_session(fork.session_id)
            .expect("reopened fork")
            .title
            .expect("fork title")
            .as_str(),
        "independent fork"
    );
    reopened.shutdown().await;
}
