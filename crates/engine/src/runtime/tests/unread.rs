//! Unread root sessions (`docs/site/specs/unread-sessions.md`): marked and
//! read through the engine, shown in client metadata, announced, and
//! forgotten with their session.

use cookie_agent_protocol::{
    SessionId, SessionUnreadSetParams, SessionUnreadSkipReason, SessionsChangedNotification,
};

use super::support::*;
use crate::Engine;

fn listed_unread(engine: &Engine, session: SessionId) -> bool {
    engine
        .list_sessions()
        .into_iter()
        .find(|meta| meta.session_id == session)
        .expect("listed root")
        .unread
}

fn announced_unread(
    changes: &mut tokio::sync::broadcast::Receiver<SessionsChangedNotification>,
    session: SessionId,
) -> Option<bool> {
    while let Ok(change) = changes.try_recv() {
        if let SessionsChangedNotification::Upserted { session: meta } = change
            && meta.session_id == session
        {
            return Some(meta.unread);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn roots_are_marked_unread_and_read_and_forgotten_with_their_tree() {
    let (fixture, selection) = custom_fixture();
    let engine = &fixture.engine;
    let root = engine.create_session(selection).expect("root").session_id;
    let child = create_buffered_delegated_child(engine, root);
    let unknown = SessionId::new_v7();
    let mut changes = engine.inner.store.subscribe_index_changes();

    // Only an existing root is marked; the rest are skipped, saying why.
    let result = engine.set_session_unread(SessionUnreadSetParams {
        session_ids: vec![root, child, unknown],
        unread: true,
    });
    assert_eq!(result.changed, [root]);
    let skipped = result
        .skipped
        .iter()
        .map(|skip| (skip.session_id, skip.reason, skip.root_session_id))
        .collect::<Vec<_>>();
    assert_eq!(
        skipped,
        [
            (child, SessionUnreadSkipReason::NotARootSession, Some(root)),
            (unknown, SessionUnreadSkipReason::SessionNotFound, None),
        ]
    );
    assert!(listed_unread(engine, root));
    assert_eq!(announced_unread(&mut changes, root), Some(true));
    // Marking it again changes nothing and announces nothing.
    let again = engine.set_session_unread(SessionUnreadSetParams {
        session_ids: vec![root],
        unread: true,
    });
    assert!(again.changed.is_empty() && again.skipped.is_empty());
    assert_eq!(announced_unread(&mut changes, root), None);

    // Presence on a subagent reads the whole tree.
    engine.mark_tree_read(child);
    assert!(!listed_unread(engine, root));
    assert_eq!(announced_unread(&mut changes, root), Some(false));

    // A deleted tree leaves the file.
    engine.mark_unread(root);
    assert!(listed_unread(engine, root));
    let last = engine.get_session(root).expect("root").last_event_seq;
    engine.delete_session(root, last).await.expect("deleted");
    let file = std::fs::read_to_string(engine.data_root().join("unread.json")).expect("file");
    assert!(!file.contains(&root.to_string()), "{file}");
}
