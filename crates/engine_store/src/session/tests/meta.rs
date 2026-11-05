//! A session's metadata read without its log, and the bounds on the parsed
//! read-only logs kept.

use std::time::{Duration, Instant, SystemTime};

use super::super::{ReadOnlyLogFingerprint, ReadOnlyLogs};
use super::*;

fn open_store(data: &Path) -> Arc<SessionStore> {
    SessionStore::open(data, &data.join("workspace")).expect("store")
}

fn attach(store: &SessionStore, id: SessionId) {
    store
        .append(
            id,
            None,
            fuzz_origin(),
            EventPayload::RuntimeAttached {
                name: cookie_agent_protocol::AttachmentName::new("box_api").unwrap(),
                runtime_id: cookie_agent_protocol::RuntimeId::new("box").unwrap(),
                capability: cookie_agent_protocol::CapabilityName::coding(),
                kind: cookie_agent_protocol::RuntimeKind::coding(),
                workspace: Some("/srv/api".into()),
            },
        )
        .unwrap();
}

/// A store's data directory holding one session, closed so the session's
/// `metadata` file is written, and that session.
fn closed_session(data: &Path, attached: bool) -> SessionId {
    let store = open_store(data);
    let id = persist_test_session(&store);
    if attached {
        attach(&store, id);
    }
    id
}

fn session_dir(store: &SessionStore, id: SessionId) -> std::path::PathBuf {
    store.resolve_dir(id).expect("the session's directory")
}

/// Rewrites the session's `metadata` file to say `status`. The sessions these
/// tests make have a run going; their metadata is otherwise as written.
fn say_status(store: &SessionStore, id: SessionId, status: &str) {
    let path = session_dir(store, id).join(SESSION_META_FILE);
    let mut meta: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("metadata")).expect("json");
    meta["status"] = serde_json::json!(status);
    fs::write(&path, serde_json::to_vec(&meta).expect("json")).expect("rewrite");
}

#[test]
fn a_settled_session_with_attachments_is_read_without_its_log() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let id = closed_session(&data, true);

    let cold = open_store(&data);
    say_status(&cold, id, "completed");
    let meta = cold.meta(id).expect("metadata");
    assert_eq!(meta.session_id, id);
    assert_eq!(meta.attachments.len(), 1);
    assert_eq!(cold.log_open_count(id), 0);
}

#[test]
fn metadata_that_lists_no_attachments_is_not_trusted_for_them() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let id = closed_session(&data, false);

    let cold = open_store(&data);
    say_status(&cold, id, "completed");
    assert_eq!(cold.meta(id).expect("metadata").session_id, id);
    assert!(cold.log_open_count(id) > 0);
}

#[test]
fn a_log_newer_than_its_metadata_is_read() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let id = closed_session(&data, true);

    let cold = open_store(&data);
    say_status(&cold, id, "completed");
    let metadata = fs::File::options()
        .write(true)
        .open(session_dir(&cold, id).join(SESSION_META_FILE))
        .expect("the metadata file");
    metadata
        .set_modified(SystemTime::now() - Duration::from_secs(3_600))
        .expect("an older time");
    assert_eq!(cold.meta(id).expect("metadata").attachments.len(), 1);
    assert!(cold.log_open_count(id) > 0);
}

#[test]
fn metadata_of_a_run_going_when_it_was_written_is_not_trusted() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let id = closed_session(&data, true);

    let cold = open_store(&data);
    say_status(&cold, id, "running");
    cold.meta(id).expect("metadata");
    assert!(cold.log_open_count(id) > 0);
}

fn fingerprint(len: u64) -> ReadOnlyLogFingerprint {
    ReadOnlyLogFingerprint {
        len,
        modified: None,
    }
}

/// `count` kept logs of `len` bytes each, newest last, and their IDs newest
/// first.
fn kept(store: &SessionStore, logs: &mut ReadOnlyLogs, lens: &[u64]) -> Vec<SessionId> {
    let now = Instant::now();
    let mut ids = Vec::new();
    for len in lens {
        let id = persist_test_session(store);
        let log =
            crate::events::EventLog::open_read_only(session_dir(store, id).join(EVENTS_FILE), id)
                .expect("the log");
        logs.0.push_front(super::super::ReadOnlyLog {
            id,
            fingerprint: fingerprint(*len),
            log,
            used: now,
        });
        ids.insert(0, id);
    }
    ids
}

fn kept_ids(logs: &ReadOnlyLogs) -> Vec<SessionId> {
    logs.0.iter().map(|entry| entry.id).collect()
}

#[test]
fn kept_logs_fit_a_byte_budget_least_recently_used_first() {
    let temporary = private_tempdir();
    let store = open_store(&temporary.path().join("data"));
    let mut logs = ReadOnlyLogs::default();
    let ids = kept(&store, &mut logs, &[10, 10, 10]);
    logs.fit(20);
    assert_eq!(kept_ids(&logs), ids[..2]);
}

#[test]
fn the_newest_log_stays_whatever_its_size() {
    let temporary = private_tempdir();
    let store = open_store(&temporary.path().join("data"));
    let mut logs = ReadOnlyLogs::default();
    let ids = kept(&store, &mut logs, &[10, 500]);
    logs.fit(20);
    assert_eq!(kept_ids(&logs), ids[..1]);
}

#[test]
fn kept_logs_nothing_read_for_a_while_go() {
    let temporary = private_tempdir();
    let store = open_store(&temporary.path().join("data"));
    let mut logs = ReadOnlyLogs::default();
    let ids = kept(&store, &mut logs, &[10, 10]);
    let later = Instant::now() + Duration::from_secs(60);
    logs.0
        .iter_mut()
        .find(|entry| entry.id == ids[0])
        .expect("the newest")
        .used = later;
    logs.prune(later, Duration::from_secs(30));
    assert_eq!(kept_ids(&logs), ids[..1]);
}
