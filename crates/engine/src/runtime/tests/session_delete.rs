use cookie_agent_protocol::{SessionId, SessionsChangedNotification};

use super::delegation_revert::start_held_background_delegation;
use super::support::*;
use crate::runtime::EngineError;
use crate::session::SessionError;

/// A published root with one published delegated child, both holding an
/// artifact of the tree, and the root's last event seq.
fn published_tree(
    fixture: &Fixture,
    selection: cookie_agent_protocol::RunSelection,
) -> (SessionId, SessionId, u64) {
    let engine = &fixture.engine;
    let root = engine.create_session(selection).expect("root").session_id;
    engine
        .inner
        .store
        .persist_buffered_session(root)
        .expect("persist root");
    let child = create_buffered_delegated_child(engine, root);
    engine
        .inner
        .store
        .persist_buffered_session(child)
        .expect("persist child");
    engine
        .inner
        .artifacts
        .retain(root, b"root artifact")
        .expect("root artifact");
    engine
        .inner
        .artifacts
        .retain(child, b"child artifact")
        .expect("child artifact");
    let last = engine.get_session(root).expect("root meta").last_event_seq;
    (root, child, last)
}

fn workdir_entries(fixture: &Fixture) -> Vec<String> {
    let mut names = std::fs::read_dir(fixture.engine.inner.store.workdir_dir_path())
        .expect("work dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[tokio::test]
async fn deleting_a_tree_removes_it_from_disk_and_tells_every_listener() {
    let (fixture, selection) = custom_fixture();
    let (root, child, last) = published_tree(&fixture, selection);
    let engine = &fixture.engine;
    let root_dir = engine.inner.store.resolve_dir(root).expect("root dir");
    assert!(root_dir.join("artifacts").is_dir());
    assert!(root_dir.join("subagents").join(child.to_string()).is_dir());
    let before = workdir_entries(&fixture);
    let mut changes = engine.subscribe_session_changes();
    let (_, mut tail) = engine.subscribe(root, None).await.expect("root tail");
    for session in [root, child] {
        engine
            .inner
            .deferral_views
            .lock()
            .unwrap()
            .insert(session, std::sync::Arc::default());
    }

    let deleted = engine.delete_session(root, last).await.expect("delete");

    assert_eq!(deleted, vec![root, child]);
    assert!(
        engine.inner.deferral_views.lock().unwrap().is_empty(),
        "the tree's deferral views went with it"
    );
    assert!(!root_dir.exists(), "the tree's directory is gone");
    let after = workdir_entries(&fixture);
    assert_eq!(
        after,
        before
            .into_iter()
            // On Windows the tree's lock is a sidecar beside it, and goes too.
            .filter(|name| *name != root.to_string() && *name != format!("{root}.owner.lock"))
            .collect::<Vec<_>>(),
        "only the tree (and its lock) left the work dir, and no tombstone stayed"
    );
    assert!(
        engine
            .list_sessions()
            .iter()
            .all(|meta| meta.session_id != root)
    );
    for id in [root, child] {
        assert!(
            matches!(engine.get_session(id), Err(EngineError::Session(SessionError::Missing(missing))) if missing == id),
            "{id} is gone"
        );
        assert!(!engine.inner.store.is_owned(id));
    }
    let mut removed = Vec::new();
    while let Ok(change) = changes.try_recv() {
        if let SessionsChangedNotification::Removed { session_id } = change {
            removed.push(session_id);
        }
    }
    assert_eq!(removed, vec![root, child], "removed, the root first");
    let ended = tokio::time::timeout(test_timeout(5), async {
        while tail.recv().await.is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "the root's live tail ends");
    assert!(matches!(
        engine.delete_session(root, last).await,
        Err(EngineError::Session(SessionError::Missing(id))) if id == root
    ));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_delegated_session_is_deleted_only_with_its_root() {
    let (fixture, selection) = custom_fixture();
    let (root, child, last) = published_tree(&fixture, selection);
    let child_last = fixture
        .engine
        .get_session(child)
        .expect("child meta")
        .last_event_seq;
    assert!(matches!(
        fixture.engine.delete_session(child, child_last).await,
        Err(EngineError::NotARootSession { session_id, root_session_id })
            if session_id == child && root_session_id == root
    ));
    assert!(fixture.engine.get_session(child).is_ok());
    fixture
        .engine
        .delete_session(root, last)
        .await
        .expect("delete through the root");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_root_that_moved_since_it_was_shown_is_not_deleted() {
    let (fixture, selection) = custom_fixture();
    let (root, _, last) = published_tree(&fixture, selection);
    let root_dir = fixture
        .engine
        .inner
        .store
        .resolve_dir(root)
        .expect("root dir");
    assert!(matches!(
        fixture.engine.delete_session(root, last - 1).await,
        Err(EngineError::StaleDelete { session_id, current_last_seq })
            if session_id == root && current_last_seq == last
    ));
    assert!(
        root_dir.join("events.jsonl").is_file(),
        "nothing was removed"
    );
    assert!(fixture.engine.get_session(root).is_ok());
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_session_never_written_to_disk_is_forgotten() {
    let (fixture, selection) = custom_fixture();
    let meta = fixture.engine.create_session(selection).expect("root");
    let deleted = fixture
        .engine
        .delete_session(meta.session_id, meta.last_event_seq)
        .await
        .expect("delete a buffered root");
    assert_eq!(deleted, vec![meta.session_id]);
    assert!(
        fixture
            .engine
            .list_sessions()
            .iter()
            .all(|listed| listed.session_id != meta.session_id)
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_tree_another_process_holds_is_not_deleted_until_it_lets_go() {
    let (fixture, selection) = custom_fixture();
    let (root, child, last) = published_tree(&fixture, selection);
    let root_dir = fixture
        .engine
        .inner
        .store
        .resolve_dir(root)
        .expect("root dir");
    let other = reopen_engine(&fixture);
    assert!(matches!(
        other.delete_session(root, last).await,
        Err(EngineError::SessionOwnedByAnotherProcess(id)) if id == root
    ));
    assert!(root_dir.is_dir());

    fixture.engine.shutdown().await;
    // The tree is free now: the other daemon deletes it without adopting
    // it, so nothing is appended and the guard still matches.
    let deleted = other.delete_session(root, last).await.expect("delete");
    assert_eq!(deleted, vec![root, child]);
    assert!(!root_dir.exists());
    other.shutdown().await;
}

#[tokio::test]
async fn a_running_tree_is_not_deleted_until_it_finishes() {
    let (fixture, parent, release, server) = start_held_background_delegation(true).await;
    let engine = &fixture.engine;
    let child = engine
        .inner
        .store
        .children(parent)
        .expect("children")
        .first()
        .expect("the background child")
        .session_id;
    let last = engine.get_session(parent).expect("parent").last_event_seq;
    assert!(matches!(
        engine.delete_session(parent, last).await,
        Err(EngineError::SessionTreeRunning { root, busy }) if root == parent && busy == child
    ));
    // A guard that lags the run, as a listing does while a run streams, is
    // still refused as running rather than as changed.
    assert!(matches!(
        engine.delete_session(parent, last - 1).await,
        Err(EngineError::SessionTreeRunning { root, busy }) if root == parent && busy == child
    ));
    assert!(engine.get_session(child).is_ok(), "nothing was deleted");

    release.notify_waiters();
    server.await.expect("held delegation server");
    wait_for_session_not_running(engine, parent).await;
    wait_for_session_not_running(engine, child).await;
    let last = engine.get_session(parent).expect("parent").last_event_seq;
    let deleted = engine.delete_session(parent, last).await.expect("delete");
    assert_eq!(deleted, vec![parent, child]);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_tombstone_a_crash_left_behind_is_removed_at_the_next_start() {
    let (fixture, selection) = custom_fixture();
    let (root, _, _) = published_tree(&fixture, selection);
    let workdir = fixture.engine.inner.store.workdir_dir_path().to_path_buf();
    // What a deletion that stopped after its rename leaves: the tree under
    // a tombstone name, which no listing reads.
    let tombstone = workdir.join(format!(".deleted-{root}-crashed"));
    std::fs::rename(workdir.join(root.to_string()), &tombstone).expect("move the tree aside");
    assert!(tombstone.join("events.jsonl").is_file());
    fixture.engine.shutdown().await;

    let reopened = reopen_engine(&fixture);
    assert!(!tombstone.exists(), "the next start removed the tombstone");
    assert!(
        reopened
            .list_sessions()
            .iter()
            .all(|meta| meta.session_id != root)
    );
    reopened.shutdown().await;
}

/// A subagent report owed by a session that was deleted since can never be
/// delivered: its delivery gives up instead of retrying until shutdown.
#[tokio::test]
async fn a_report_owed_by_a_deleted_subagent_stops_retrying() {
    let (fixture, selection) = custom_fixture();
    let (root, child, last) = published_tree(&fixture, selection);
    let engine = &fixture.engine;
    let deleted = engine.delete_session(root, last).await.expect("delete");
    assert_eq!(deleted, vec![root, child]);

    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.deliver_subagent_report_until_done(child, cookie_agent_protocol::RunId::new_v7()),
    )
    .await
    .expect("the delivery gives up once the session is gone");
    fixture.engine.shutdown().await;
}
