//! The approval reviewer's own log, a hidden child session
//! (`docs/site/specs/forever-sessions.md` section 5.5).

use cookie_agent_protocol::{
    ApprovalId, ReviewerCursor, ReviewerEntry, ReviewerEpoch, ReviewerTurn,
};

use super::super::{REVIEWER_DIR, STATE_FILE};
use super::*;

/// A session with `messages` more user inputs after its first, so its log
/// reaches past `messages + 3`.
fn reviewed_session(store: &SessionStore, messages: usize) -> SessionId {
    let session = persist_test_session(store);
    let run = store
        .get(session)
        .expect("the session")
        .log
        .last_run_started()
        .map(|(_, run, _)| run);
    for index in 0..messages {
        store
            .append(
                session,
                run,
                test_origin(),
                EventPayload::UserInputSubmitted {
                    content: format!("message {index}").into(),
                },
            )
            .expect("an input");
    }
    session
}

/// A reviewer turn for the review cut at `cut`, its verdict at `cut + 1`,
/// starting an epoch when `epoch`.
fn turn(cut: u64, epoch: bool) -> EventPayload {
    let entry = ReviewerEntry {
        seq: cut,
        text: format!("[user] at {cut}"),
        message: true,
    };
    EventPayload::ReviewerTurnRecorded {
        turn: Box::new(ReviewerTurn {
            approval_id: ApprovalId::new_v7(),
            cut_seq: cut,
            evaluated_seq: cut + 1,
            tool: "bash".into(),
            request: format!("<context_delta>\n{}\n</context_delta>", entry.text),
            verdict: r#"{"decision":"allow"}"#.into(),
            entries: if epoch {
                Vec::new()
            } else {
                vec![entry.clone()]
            },
            epoch: epoch.then(|| ReviewerEpoch {
                messages: vec![entry],
                ..ReviewerEpoch::default()
            }),
            cursor: ReviewerCursor {
                runs: 1,
                ..ReviewerCursor::default()
            },
        }),
    }
}

/// The cuts of the reviewer turns `log` holds, from the file, in order.
fn cuts(log: &crate::events::EventLog) -> Vec<u64> {
    log.history()
        .expect("a pass")
        .filter_map(|event| match &event.expect("a record").payload {
            EventPayload::ReviewerTurnRecorded { turn } => Some(turn.cut_seq),
            _ => None,
        })
        .collect()
}

/// The reviewer log is filed in its session's folder, where no listing looks:
/// the tree, the children, and the store's sessions leave it out. Each epoch
/// moves its window to itself and writes its `state.json`, and once its
/// session left memory it opens again from that file.
#[test]
fn a_reviewer_log_is_hidden_narrows_at_each_epoch_and_reopens_from_its_file() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let store = SessionStore::open(&data, &cwd).expect("store");
    let session = reviewed_session(&store, 12);
    assert!(store.reviewer_log(session).expect("a lookup").is_none());

    let log = store
        .create_reviewer_log(session, vec![turn(3, true)])
        .expect("a reviewer log");
    let reviewer = log.session_id();
    assert_ne!(reviewer, session);
    let directory = store
        .resolve_dir(session)
        .expect("a folder")
        .join(REVIEWER_DIR);
    assert_eq!(log.path(), directory.join(EVENTS_FILE));
    assert!(directory.join(STATE_FILE).is_file());
    for cut in [5, 7] {
        store
            .append_reviewer(session, turn(cut, false))
            .expect("a turn");
    }
    assert_eq!(log.storage_len_for_test(), 3, "an epoch's turns are held");
    let epoch = store
        .append_reviewer(session, turn(9, true))
        .expect("an epoch");
    store
        .append_reviewer(session, turn(11, false))
        .expect("a turn");
    assert_eq!(log.window_for_test(), Some(epoch.seq));
    assert_eq!(
        log.storage_len_for_test(),
        2,
        "the window starts at the epoch"
    );
    assert_eq!(cuts(&log), [3, 5, 7, 9, 11]);

    // Hidden: nothing that lists sessions or walks a tree finds it.
    assert!(store.resolve_dir(reviewer).is_err());
    assert!(!store.session_exists(reviewer));
    assert!(
        store
            .all()
            .iter()
            .all(|session| session.meta.session_id != reviewer)
    );
    assert!(store.children(session).expect("children").is_empty());
    let tree = store.tree(session).expect("a tree");
    assert_eq!(tree.session.session_id, session);
    assert!(tree.children.is_empty());
    assert!(store.tree_summaries(session).expect("summaries").len() == 1);

    // It leaves memory with its session and opens again from its file.
    assert!(store.evict(session).expect("an eviction"));
    assert!(!store.reviewer_log_is_open_for_test(session));
    let reopened = store
        .reviewer_log(session)
        .expect("a lookup")
        .expect("its log");
    assert!(reopened.opened_from_state_for_test());
    assert_eq!(reopened.window_for_test(), Some(epoch.seq));
    assert_eq!(cuts(&reopened), [3, 5, 7, 9, 11]);
    drop(store);

    // A file that cannot be read starts over.
    let store = SessionStore::open(&data, &cwd).expect("store");
    store.open_for_write(session).expect("an adoption");
    fs::write(directory.join(EVENTS_FILE), b"not a log\n").expect("a write");
    assert!(store.reviewer_log(session).expect("a lookup").is_none());
    assert!(!directory.exists());
}

/// A revert of the reviewed session keeps the turns whose verdicts stay and
/// the epoch they are in, and a fork copies them from that epoch on. A delete
/// takes the log with its folder.
#[test]
fn a_revert_cuts_the_reviewer_log_a_fork_copies_it_and_a_delete_takes_it() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let store = SessionStore::open(&temporary.path().join("data"), &cwd).expect("store");
    let session = reviewed_session(&store, 16);
    store
        .create_reviewer_log(session, vec![turn(3, true)])
        .expect("a reviewer log");
    for (cut, epoch) in [(5, false), (7, true), (9, false), (11, false), (13, true)] {
        store
            .append_reviewer(session, turn(cut, epoch))
            .expect("a turn");
    }

    // Verdicts at 4, 6, 8, and 10 stay; the one at 12 goes, and with the
    // epoch at 13 the window moves back to the one at 7.
    let fork = store.fork(session, 10, test_origin()).expect("a fork");
    let forked = store
        .reviewer_log(fork)
        .expect("a lookup")
        .expect("the fork's reviewer log");
    assert_eq!(cuts(&forked), [7, 9]);
    assert_ne!(
        forked.session_id(),
        store.reviewer_log(session).unwrap().unwrap().session_id()
    );
    // A fork before every verdict has none.
    let early = store.fork(session, 3, test_origin()).expect("a fork");
    assert!(store.reviewer_log(early).expect("a lookup").is_none());

    store.truncate(session, 11).expect("a revert");
    let log = store
        .reviewer_log(session)
        .expect("a lookup")
        .expect("its log");
    assert_eq!(cuts(&log), [3, 5, 7, 9]);
    assert_eq!(
        log.window_for_test(),
        Some(4),
        "the epoch at 7 is the fourth record"
    );
    store
        .append_reviewer(session, turn(11, false))
        .expect("a turn after the revert");
    assert_eq!(cuts(&log), [3, 5, 7, 9, 11]);

    // A revert before every verdict keeps the creation alone.
    store.truncate(session, 3).expect("a revert");
    assert!(cuts(&store.reviewer_log(session).unwrap().unwrap()).is_empty());

    let directory = store
        .resolve_dir(fork)
        .expect("a folder")
        .join(REVIEWER_DIR);
    assert!(directory.is_dir());
    store
        .begin_tree_deletion(fork)
        .expect("a deletion")
        .commit()
        .expect("a deletion");
    assert!(!directory.exists());
    assert!(!store.reviewer_log_is_open_for_test(fork));
}

/// A session that does not leave memory keeps its reviewer log. An eviction that
/// evicted nothing let go of it all the same, so a review's turn found no log to
/// go to, and the next lookup opened a second log on the same file while the first
/// could still be held (review, 2026-11-08).
#[test]
fn an_eviction_that_evicts_nothing_keeps_the_reviewer_log() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let store = SessionStore::open(&temporary.path().join("data"), &cwd).expect("store");
    let session = reviewed_session(&store, 4);
    store
        .create_reviewer_log(session, vec![turn(3, true)])
        .expect("a reviewer log");
    assert!(store.evict(session).expect("an eviction"));
    assert!(
        !store.reviewer_log_is_open_for_test(session),
        "it leaves memory with its session"
    );

    // Opened again while its session is out of memory: an eviction evicts nothing.
    let open = store
        .reviewer_log(session)
        .expect("a lookup")
        .expect("its log");
    assert!(
        !store.evict(session).expect("an eviction"),
        "nothing evicted"
    );
    assert!(
        store.reviewer_log_is_open_for_test(session),
        "the log stays open"
    );
    store
        .append_reviewer(session, turn(5, false))
        .expect("a turn through the log it holds");
    let again = store
        .reviewer_log(session)
        .expect("a lookup")
        .expect("its log");
    assert!(Arc::ptr_eq(&open, &again), "one log on its file");
    assert_eq!(cuts(&open), [3, 5]);
}
