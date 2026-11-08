//! `state.json` as the tree load and a viewer read it
//! (`docs/site/specs/forever-sessions.md` section 4.5).

use std::path::Path;

use cookie_agent_protocol::EventsView;

use super::super::{STATE_FILE, SessionLocation};
use super::*;

const ACP_ROOT: &str = "01a1b46f-0f55-7ba3-86e8-f8d942e26916";
const ACP_CHILD: &str = "01a1b46f-1fef-7713-852e-7d1231b0";

fn copy_directory(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("a directory");
    for entry in fs::read_dir(from).expect("a directory") {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("a copy");
        }
    }
}

fn store_in(temporary: &tempfile::TempDir) -> Arc<SessionStore> {
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    SessionStore::open(&temporary.path().join("data"), &cwd).expect("store")
}

/// A store holding a copy of the golden ACP tree, with its root and child.
fn golden_acp_tree(temporary: &tempfile::TempDir) -> (SessionId, SessionId) {
    let store = store_in(temporary);
    let root: SessionId = ACP_ROOT.parse().expect("a session");
    let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../engine/tests/fixtures/history")
        .join(cookie_agent_protocol::HISTORY_FORMAT.to_string())
        .join("acp")
        .join(ACP_ROOT);
    let target = store.path_for(SessionLocation::Root, root);
    copy_directory(&golden, &target);
    let child = fs::read_dir(target.join(crate::session::SUBAGENTS_DIR))
        .expect("the tree's subagents")
        .map(|entry| entry.expect("a directory entry").file_name())
        .find_map(|name| {
            name.to_str()
                .filter(|name| name.starts_with(ACP_CHILD))?
                .parse()
                .ok()
        })
        .expect("the golden ACP scenario's child");
    (root, child)
}

fn parses() -> usize {
    crate::events::EVENT_LOG_PARSES.with(std::cell::Cell::get)
}

/// Adopts `ids` and sends them out of memory, which writes their state files.
fn leave_memory(store: &SessionStore, ids: &[SessionId]) {
    for id in ids {
        store.open_for_write(*id).expect("an adoption");
    }
    for id in ids {
        assert!(store.evict(*id).expect("an eviction"));
        let dir = store.resolve_dir(*id).expect("a session");
        assert!(dir.join(STATE_FILE).is_file(), "leaving memory writes it");
    }
}

/// With a file for the root and each child, a tree load parses none of them, and
/// a viewer of the root parses nothing on top.
#[test]
fn a_tree_load_and_a_viewer_parse_no_log_that_has_a_state_file() {
    let temporary = private_tempdir();
    let (root, child) = golden_acp_tree(&temporary);
    leave_memory(&store_in(&temporary), &[root, child]);

    let store = store_in(&temporary);
    let before = parses();
    store.load_tree(root).expect("a tree load");
    assert_eq!(parses(), before, "the tree load parsed no log");
    assert!(store.is_tree_loaded(root));
    assert!(
        store
            .snapshot_tail(root, EventsView::Display, 64 * 1024)
            .is_ok()
    );
    assert!(store.get(root).is_ok());
    assert_eq!(parses(), before, "the view parsed no log");
}

/// What a tree load hands over is the same from the files as from the logs.
#[test]
fn a_tree_load_harvests_the_same_from_state_files_as_from_the_logs() {
    let temporary = private_tempdir();
    let (root, child) = golden_acp_tree(&temporary);
    let parsed = store_in(&temporary).fold_tree(root).expect("a tree load");
    leave_memory(&store_in(&temporary), &[root, child]);
    let store = store_in(&temporary);
    let before = parses();
    let read = store.fold_tree(root).expect("a tree load");
    assert_eq!(parses(), before, "the tree load parsed no log");
    assert_eq!(read.grants, parsed.grants);
    assert_eq!(read.delegations, parsed.delegations);
    assert_eq!(read.producer_sessions, parsed.producer_sessions);
    assert_eq!(read.producer_projections, parsed.producer_projections);
    let by_session = |summaries: &[SessionSummary]| {
        summaries
            .iter()
            .map(|summary| {
                (
                    summary.meta.session_id,
                    serde_json::to_value(summary).expect("a summary serializes"),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    assert_eq!(by_session(&read.summaries), by_session(&parsed.summaries));
}

/// A child with no file parses as it did, and the others still do not.
#[test]
fn a_tree_load_parses_only_the_logs_without_a_state_file() {
    let temporary = private_tempdir();
    let (root, child) = golden_acp_tree(&temporary);
    leave_memory(&store_in(&temporary), &[root]);
    let store = store_in(&temporary);
    let before = parses();
    store.load_tree(root).expect("a tree load");
    assert_eq!(parses(), before + 1, "the child only");
    assert!(store.get_log_only(child).is_ok());
}

/// Only the owner writes the file, and a session this process only reads gets none.
#[test]
fn a_store_that_only_reads_a_session_writes_no_state_file() {
    let temporary = private_tempdir();
    let (root, _) = golden_acp_tree(&temporary);
    let store = store_in(&temporary);
    store.load_tree(root).expect("a tree load");
    store.get(root).expect("a read");
    store.evict(root).expect("an eviction");
    let dir = store.resolve_dir(root).expect("a session");
    assert!(!dir.join(STATE_FILE).exists());
}

/// A log shorter than the least that has a state file leaves memory without
/// writing one, and opens by folding; one as long writes it.
#[test]
fn a_log_shorter_than_the_least_has_no_state_file() {
    let temporary = private_tempdir();
    let (root, _) = golden_acp_tree(&temporary);
    let store = store_in(&temporary);
    let dir = store.resolve_dir(root).expect("a session");
    let length = fs::metadata(dir.join(crate::session::EVENTS_FILE))
        .expect("the log")
        .len();

    crate::events::state_file::set_state_min_log_bytes_for_test(length + 1);
    store.open_for_write(root).expect("an adoption");
    assert!(store.evict(root).expect("an eviction"));
    assert!(
        !dir.join(STATE_FILE).exists(),
        "a log one byte short has none"
    );
    let log = crate::events::EventLog::open_read_only(dir.join(crate::session::EVENTS_FILE), root)
        .expect("the log opens");
    assert!(!log.opened_from_state_for_test());

    crate::events::state_file::set_state_min_log_bytes_for_test(length);
    store.open_for_write(root).expect("an adoption");
    assert!(store.evict(root).expect("an eviction"));
    crate::events::state_file::set_state_min_log_bytes_for_test(0);
    assert!(
        dir.join(STATE_FILE).is_file(),
        "a log as long as the least has one"
    );
}

/// A revert between taking a state file from the log and writing it wins: the
/// write is left out, so the file from before the cut does not come back and the
/// log is not noted as written as far as it then was, which held writes off until
/// the log grew past that again (review, 2026-11-08).
#[test]
fn a_state_write_racing_a_revert_leaves_neither_its_file_nor_its_note() {
    let temporary = private_tempdir();
    let (root, _) = golden_acp_tree(&temporary);
    let store = store_in(&temporary);
    store.open_for_write(root).expect("an adoption");
    let log = Arc::clone(&store.get_resident(root).expect("resident").log);
    let through = log.physical_tip_seq() / 2;
    let reverting = Arc::downgrade(&store);
    store.install_state_write_hook_for_test(move |id| {
        reverting
            .upgrade()
            .expect("the store")
            .truncate(id, through)
            .expect("a revert");
    });
    store.refresh_state(root, |_| true);
    assert_eq!(log.physical_tip_seq(), through, "the revert");
    let dir = store.resolve_dir(root).expect("a session");
    assert!(
        !dir.join(STATE_FILE).exists(),
        "no file from before the cut"
    );
    let length = fs::metadata(dir.join(crate::session::EVENTS_FILE))
        .expect("the log")
        .len();
    assert_eq!(
        log.state_behind(),
        Some(length),
        "nothing of the log noted as written"
    );
}

/// An owned open removes a `state.json` that does not match its log, and one beside
/// a log shorter than the least that has one, though it matches: nothing else
/// would, and every open read it and threw it away (review, 2026-11-08). One that
/// matches a log long enough stays.
#[test]
fn an_owned_open_removes_a_state_file_that_does_not_match_or_is_not_wanted() {
    let temporary = private_tempdir();
    let (root, _) = golden_acp_tree(&temporary);
    let store = store_in(&temporary);
    leave_memory(&store, &[root]);
    let dir = store.resolve_dir(root).expect("a session");
    let state = dir.join(STATE_FILE);

    store.open_for_write(root).expect("an adoption");
    assert!(
        store
            .get_resident(root)
            .expect("resident")
            .log
            .opened_from_state_for_test()
    );
    assert!(state.is_file(), "a file that matches stays");
    assert!(store.evict(root).expect("an eviction"));

    fs::write(&state, b"not json").expect("a damaged file");
    store.open_for_write(root).expect("an adoption");
    assert!(!state.exists(), "a file that does not match goes");
    assert!(store.evict(root).expect("an eviction"));
    assert!(state.is_file(), "leaving memory writes it again");

    let length = fs::metadata(dir.join(crate::session::EVENTS_FILE))
        .expect("the log")
        .len();
    crate::events::state_file::set_state_min_log_bytes_for_test(length + 1);
    store.open_for_write(root).expect("an adoption");
    crate::events::state_file::set_state_min_log_bytes_for_test(0);
    assert!(!state.exists(), "a log shorter than the least has none");
}
