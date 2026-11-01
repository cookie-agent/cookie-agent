//! Pinned root sessions (`docs/site/specs/session-pins-and-filters.md`):
//! pinned and unpinned through the engine, shown in client metadata,
//! announced, listed whatever the window, and forgotten with their
//! session; and roots a cron job will wake, listed the same way.

use cookie_agent_protocol::{
    CronCreateParams, CronSchedule, CronTarget, SessionId, SessionPinSetParams,
    SessionUnreadSkipReason, SessionsChangedNotification,
};

use super::support::*;
use crate::Engine;

fn listed_pinned(engine: &Engine, session: SessionId) -> bool {
    engine
        .list_sessions()
        .into_iter()
        .find(|meta| meta.session_id == session)
        .expect("listed root")
        .pinned
}

fn announced_pinned(
    changes: &mut tokio::sync::broadcast::Receiver<SessionsChangedNotification>,
    session: SessionId,
) -> Option<bool> {
    while let Ok(change) = changes.try_recv() {
        if let SessionsChangedNotification::Upserted { session: meta } = change
            && meta.session_id == session
        {
            return Some(meta.pinned);
        }
    }
    None
}

/// Roots listed by a window that starts after every session's activity:
/// only those listed whatever their age.
fn listed_out_of_window(engine: &Engine) -> Vec<SessionId> {
    let next_month = jiff::Timestamp::now()
        .checked_add(jiff::SignedDuration::from_hours(24 * 40))
        .expect("forty days on");
    let (sessions, _) = engine.list_sessions_in(crate::session::ListWindow {
        active_since: Some(next_month),
        before: None,
    });
    let mut listed = sessions
        .iter()
        .map(|meta| meta.session_id)
        .collect::<Vec<_>>();
    listed.sort();
    listed
}

#[tokio::test(flavor = "multi_thread")]
async fn roots_are_pinned_and_unpinned_and_forgotten_with_their_tree() {
    let (fixture, selection) = custom_fixture();
    let engine = &fixture.engine;
    let root = engine.create_session(selection).expect("root").session_id;
    let child = create_buffered_delegated_child(engine, root);
    let unknown = SessionId::new_v7();
    let mut changes = engine.inner.store.subscribe_index_changes();

    // Only an existing root is pinned; the rest are skipped, saying why.
    let result = engine.set_session_pinned(SessionPinSetParams {
        session_ids: vec![root, child, unknown],
        pinned: true,
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
    assert!(listed_pinned(engine, root));
    assert!(!engine.get_session(child).expect("child").pinned);
    assert_eq!(announced_pinned(&mut changes, root), Some(true));
    // Pinning it again changes nothing and announces nothing.
    let again = engine.set_session_pinned(SessionPinSetParams {
        session_ids: vec![root],
        pinned: true,
    });
    assert!(again.changed.is_empty() && again.skipped.is_empty());
    assert_eq!(announced_pinned(&mut changes, root), None);

    // A pin is not an unread mark, nor the other way round.
    engine.mark_tree_read(root);
    assert!(listed_pinned(engine, root));

    let unpinned = engine.set_session_pinned(SessionPinSetParams {
        session_ids: vec![root],
        pinned: false,
    });
    assert_eq!(unpinned.changed, [root]);
    assert!(!listed_pinned(engine, root));
    assert_eq!(announced_pinned(&mut changes, root), Some(false));

    // A deleted tree leaves the file.
    engine.set_session_pinned(SessionPinSetParams {
        session_ids: vec![root],
        pinned: true,
    });
    let last = engine.get_session(root).expect("root").last_event_seq;
    engine.delete_session(root, last).await.expect("deleted");
    let file = std::fs::read_to_string(engine.data_root().join("pinned.json")).expect("file");
    assert!(!file.contains(&root.to_string()), "{file}");
}

/// A session list's window leaves out older sessions, but never a pinned
/// one, nor one a cron job will wake; a disabled job wakes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn pinned_and_cron_woken_roots_are_listed_whatever_the_window() {
    let (fixture, selection) = custom_fixture();
    let engine = &fixture.engine;
    let idle = engine
        .create_session(selection.clone())
        .expect("idle")
        .session_id;
    let pinned = engine
        .create_session(selection.clone())
        .expect("pinned")
        .session_id;
    let woken = engine
        .create_session(selection.clone())
        .expect("woken")
        .session_id;
    let disabled = engine
        .create_session(selection)
        .expect("disabled")
        .session_id;
    engine.set_session_pinned(SessionPinSetParams {
        session_ids: vec![pinned],
        pinned: true,
    });
    let job = |target, enabled| CronCreateParams {
        name: None,
        prompt: "Wake up.".into(),
        schedule: CronSchedule::Cron("0 3 * * *".into()),
        max_runs: None,
        target: CronTarget::Session(target),
        delivery: None,
        permission_mode: None,
        max_consecutive_failures: None,
        enabled: Some(enabled),
    };
    engine.create_cron(job(woken, true)).await.expect("job");
    engine.create_cron(job(disabled, false)).await.expect("job");

    let mut expected = vec![pinned, woken];
    expected.sort();
    let listed = listed_out_of_window(engine);
    assert_eq!(listed, expected);
    assert!(!listed.contains(&idle) && !listed.contains(&disabled));

    fixture.engine.shutdown().await;
}
