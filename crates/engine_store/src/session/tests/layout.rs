//! Session storage layout v3: month folders, the migration from v2
//! work-dir folders, and the activity journals
//! (`docs/site/specs/session-storage-v3.md` sections 4 and 5).

use std::collections::HashSet;

use jiff::Timestamp;

use crate::events::blobs;

use super::super::{
    ACTIVITY_DIR, LAYOUT_MARKER_FILE, LAYOUT_VERSION, ListWindow, Month, RUNTIME_REVISIONS_FILE,
    StoreLayout, read_layout, root_dir_in,
};
use super::*;

fn at(text: &str) -> Timestamp {
    text.parse().expect("test timestamp")
}

/// A UUIDv7 session id whose timestamp is `time`.
fn id_at(time: Timestamp) -> SessionId {
    let seconds = u64::try_from(time.as_second()).expect("a time after 1970");
    SessionId(uuid::Uuid::new_v7(uuid::Timestamp::from_unix(
        uuid::NoContext,
        seconds,
        0,
    )))
}

fn month(text: &str) -> Month {
    Month::parse(text).expect("test month")
}

fn open_store(data: &Path) -> Arc<SessionStore> {
    SessionStore::open(data, &data.join("workspace")).expect("session store")
}

fn sessions(data: &Path) -> std::path::PathBuf {
    data.join(SESSIONS_ROOT_DIR)
}

fn listed_ids(list: &[cookie_agent_protocol::SessionMeta]) -> Vec<SessionId> {
    let mut ids = list.iter().map(|meta| meta.session_id).collect::<Vec<_>>();
    ids.sort();
    ids
}

fn sorted(mut ids: Vec<SessionId>) -> Vec<SessionId> {
    ids.sort();
    ids
}

/// Lays the store under `data` out as a v2 store: each root of `keys` moved
/// into its work-dir folder with the files the v2 layout kept there, and the
/// v3 files removed.
fn demote_to_v2(data: &Path, keys: &[(&str, Vec<SessionId>)]) {
    let sessions = sessions(data);
    for (key, roots) in keys {
        let work_dir = sessions.join(key);
        create_private_test_dir_all(&work_dir);
        write_private_test_file(&work_dir.join("cwd"), format!("/home/test/{key}"));
        write_private_test_file(&work_dir.join(LAYOUT_MARKER_FILE), r#"{"version": 2}"#);
        for root in roots {
            fs::rename(
                root_dir_in(&sessions, *root),
                work_dir.join(root.to_string()),
            )
            .expect("move the root into its work-dir folder");
        }
    }
    let _ = fs::remove_file(sessions.join(LAYOUT_MARKER_FILE));
    let _ = fs::remove_file(sessions.join(RUNTIME_REVISIONS_FILE));
    let _ = fs::remove_dir_all(sessions.join(ACTIVITY_DIR));
    for entry in fs::read_dir(&sessions).expect("sessions").flatten() {
        if Month::parse(&entry.file_name().to_string_lossy()).is_some() {
            let _ = fs::remove_dir(entry.path());
        }
    }
}

fn revision_line(runtime: &str, model: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "protocol_version": "1.0.0",
            "runtime_revision": runtime,
            "model_runtime_revision": model,
        })
    )
}

#[test]
fn a_months_boundaries_are_utc() {
    assert_eq!(Month::of(at("2026-10-31T23:59:59Z")), month("2026-10"));
    assert_eq!(Month::of(at("2026-11-01T00:00:00Z")), month("2026-11"));
    // Still October in UTC, though November in UTC+1.
    assert_eq!(Month::of(at("2026-10-31T23:30:00Z")), month("2026-10"));
    assert_eq!(month("2026-01").previous(), month("2025-12"));
    assert_eq!(month("2026-03").start(), at("2026-03-01T00:00:00Z"));
    assert_eq!(month("2026-03").to_string(), "2026-03");
    for name in [
        "2026-13", "2026-00", "2026-1", "26-01", "2026-01x", "abcd-ef",
    ] {
        assert_eq!(Month::parse(name), None, "{name}");
    }
}

#[test]
fn a_roots_month_comes_from_its_uuid_v7_timestamp() {
    assert_eq!(
        Month::of_id(id_at(at("2026-08-31T23:59:59Z"))),
        Some(month("2026-08"))
    );
    assert_eq!(
        Month::of_id(id_at(at("2026-09-01T00:00:00Z"))),
        Some(month("2026-09"))
    );
    assert_eq!(
        Month::of_id(SessionId(uuid::Uuid::from_u128(0x1234))),
        None,
        "an id that is not a UUIDv7 names no month"
    );
}

#[test]
fn roots_are_filed_by_month_and_children_under_their_root() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let root = persist_test_session_with_id(
        &store,
        id_at(at("2026-08-15T12:00:00Z")),
        SessionOrigin::Root,
    );
    let child = persist_test_session_with_id(
        &store,
        id_at(at("2026-10-02T08:00:00Z")),
        delegated_origin(root, root, 1),
    );
    // Roots in later months, which a child's lookup passes over.
    for time in ["2026-09-03T00:00:00Z", "2026-10-01T00:00:00Z"] {
        persist_test_session_with_id(&store, id_at(at(time)), SessionOrigin::Root);
    }
    let root_dir = sessions(&data).join("2026-08").join(root.to_string());
    assert_eq!(store.session_dir(root), root_dir);
    assert_eq!(
        store.session_dir(child),
        root_dir.join(SUBAGENTS_DIR).join(child.to_string()),
        "a child stays under its root, whatever its own month"
    );
    drop(store);

    let cold = open_store(&data);
    assert_eq!(
        cold.resolve_dir(child).expect("the child is found"),
        root_dir.join(SUBAGENTS_DIR).join(child.to_string())
    );
    assert_eq!(cold.root_of(child).expect("its root"), root);
}

#[test]
fn a_newer_store_is_refused_naming_the_build_it_needs() {
    for (layout, named) in [
        (
            serde_json::json!({"version": LAYOUT_VERSION + 1, "history_format": 1, "build": "9.1.0"}),
            "cookie 9.1.0 or newer",
        ),
        (
            serde_json::json!({
                "version": LAYOUT_VERSION,
                "history_format": cookie_agent_protocol::HISTORY_FORMAT + 1,
            }),
            "a newer cookie build",
        ),
    ] {
        let temporary = private_tempdir();
        let data = temporary.path().join("data");
        create_private_test_dir_all(&sessions(&data));
        write_private_test_file(
            &sessions(&data).join(LAYOUT_MARKER_FILE),
            serde_json::to_vec(&layout).expect("layout"),
        );
        let error = SessionStore::open(&data, temporary.path()).expect_err("a newer store");
        assert!(
            matches!(error, SessionError::NewerStore { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains(named), "{error}");
        assert_eq!(
            fs::read(sessions(&data).join(LAYOUT_MARKER_FILE)).expect("layout"),
            serde_json::to_vec(&layout).expect("layout"),
            "the refused store is left as it was"
        );
    }
}

#[test]
fn the_migration_moves_every_work_dir_tree_into_its_month() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let first = persist_test_session_with_id(
        &store,
        id_at(at("2026-07-04T10:00:00Z")),
        SessionOrigin::Root,
    );
    let child = persist_test_session_with_origin(&store, delegated_origin(first, first, 1));
    let second = persist_test_session_with_id(
        &store,
        id_at(at("2026-09-20T10:00:00Z")),
        SessionOrigin::Root,
    );
    // Created now, but by an id that is not a UUIDv7: its month comes from
    // its log.
    let legacy = persist_test_session_with_id(
        &store,
        SessionId(uuid::Uuid::from_u128(0x0bad_cafe)),
        SessionOrigin::Root,
    );
    append_title(&store, second, "second");
    drop(store);
    demote_to_v2(
        &data,
        &[
            ("0000000000000001-home", vec![first, legacy]),
            ("0000000000000002-projects", vec![second]),
        ],
    );
    let sessions_dir = sessions(&data);
    write_private_test_file(
        &sessions_dir
            .join("0000000000000001-home")
            .join("runtime-revisions-v8.jsonl"),
        revision_line("r1", "m1") + &revision_line("r2", "m2"),
    );
    write_private_test_file(
        &sessions_dir
            .join("0000000000000002-projects")
            .join("runtime-revisions-v8.jsonl"),
        revision_line("r2", "m2") + &revision_line("r3", "m3") + &revision_line("r1", "other"),
    );

    let store = open_store(&data);
    assert_eq!(
        store.session_dir(first),
        sessions_dir.join("2026-07").join(first.to_string())
    );
    assert_eq!(
        store.session_dir(second),
        sessions_dir.join("2026-09").join(second.to_string())
    );
    let legacy_dir = sessions_dir
        .join(Month::of(Timestamp::now()).to_string())
        .join(legacy.to_string());
    assert!(legacy_dir.join(EVENTS_FILE).is_file());
    assert_eq!(
        listed_ids(&store.list_roots()),
        sorted(vec![first, second, legacy])
    );
    let listed_second = store
        .list_roots()
        .into_iter()
        .find(|meta| meta.session_id == second)
        .expect("second");
    assert_eq!(
        listed_second
            .title
            .as_ref()
            .map(|title| title.as_str().to_owned()),
        Some("second".to_owned()),
        "the entry comes from the root's metadata"
    );
    assert_eq!(store.root_of(child).expect("the child moved too"), first);
    store.get(legacy).expect("the legacy root opens");
    assert_eq!(
        read_layout(&sessions_dir).expect("layout"),
        Some(StoreLayout {
            version: LAYOUT_VERSION,
            history_format: cookie_agent_protocol::HISTORY_FORMAT,
            build: Some(env!("CARGO_PKG_VERSION").to_owned()),
            pending: Vec::new(),
        })
    );
    let revisions = fs::read_to_string(sessions_dir.join(RUNTIME_REVISIONS_FILE))
        .expect("the store's runtime revisions");
    let revisions = revisions
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("record"))
        .map(|record| {
            (
                record["runtime_revision"]
                    .as_str()
                    .expect("runtime")
                    .to_owned(),
                record["model_runtime_revision"]
                    .as_str()
                    .expect("model")
                    .to_owned(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        revisions,
        [("r1", "m1"), ("r2", "m2"), ("r3", "m3")]
            .map(|(runtime, model)| (runtime.to_owned(), model.to_owned()))
            .to_vec(),
        "every record once, the first mapping of a revision kept"
    );
    for key in ["0000000000000001-home", "0000000000000002-projects"] {
        assert!(
            !sessions_dir.join(key).exists(),
            "the emptied work-dir folder {key} is removed"
        );
    }
}

/// The migration reports on the daemon's status board: one blocking task
/// per step, counting every tree, and none left once the store is open.
#[test]
fn the_migration_reports_each_tree_on_the_status_board() {
    use crate::status::StatusBoard;
    use cookie_agent_protocol::DaemonPhase;

    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let roots = (1..=3)
        .map(|day| {
            persist_test_session_with_id(
                &store,
                id_at(at(&format!("2026-07-0{day}T10:00:00Z"))),
                SessionOrigin::Root,
            )
        })
        .collect::<Vec<_>>();
    drop(store);
    demote_to_v2(&data, &[("0000000000000001-home", roots)]);

    let board = StatusBoard::recording(DaemonPhase::Starting);
    let cwd = temporary.path().join("cwd");
    fs::create_dir_all(&cwd).expect("cwd");
    let _store = SessionStore::open_reporting(&data, &cwd, &board).expect("open");
    let history = board.history();
    let moving = history
        .iter()
        .flat_map(|status| &status.tasks)
        .filter(|task| task.label == "Moving sessions to the new storage layout")
        .collect::<Vec<_>>();
    assert!(moving.iter().all(|task| task.blocking));
    let counts = moving
        .iter()
        .filter_map(|task| task.progress.as_ref())
        .map(|progress| (progress.done, progress.total))
        .collect::<Vec<_>>();
    for done in 0..=3 {
        assert!(
            counts.contains(&(done, Some(3))),
            "{done} of 3 reported: {counts:?}"
        );
    }
    assert!(
        moving
            .iter()
            .any(|task| task.detail.as_deref() == Some("0000000000000001-home")),
        "the detail names the folder a tree moves from"
    );
    assert!(board.status().tasks.is_empty(), "every task ended");
}

#[test]
fn a_migrated_store_is_not_scanned_again() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let root = persist_test_session(&store);
    drop(store);
    demote_to_v2(&data, &[("0000000000000001-home", vec![root])]);
    drop(open_store(&data));

    // A root an older build filed under a work-dir key after the migration
    // is not looked for: the marker says the store is current.
    let late = SessionId::new_v7();
    let late_dir = sessions(&data)
        .join("0000000000000003-late")
        .join(late.to_string());
    create_private_test_dir_all(&late_dir);
    let store = open_store(&data);
    assert!(late_dir.is_dir(), "the second start moved nothing");
    assert_eq!(listed_ids(&store.list_roots()), vec![root]);
}

#[test]
fn the_migration_resumes_after_a_crash_between_trees() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let moved = persist_test_session(&store);
    let left = persist_test_session(&store);
    drop(store);
    demote_to_v2(&data, &[("0000000000000001-home", vec![moved, left])]);
    // A start that stopped after its first step: one tree is in its month,
    // the other still in the work-dir folder, no journal and no marker.
    let sessions_dir = sessions(&data);
    let month_dir = root_dir_in(&sessions_dir, moved);
    create_private_test_dir_all(month_dir.parent().expect("month"));
    fs::rename(
        sessions_dir
            .join("0000000000000001-home")
            .join(moved.to_string()),
        &month_dir,
    )
    .expect("the first step");

    let store = open_store(&data);
    assert_eq!(listed_ids(&store.list_roots()), sorted(vec![moved, left]));
    assert_eq!(store.session_dir(left), root_dir_in(&sessions_dir, left));
    assert!(
        read_layout(&sessions_dir)
            .expect("layout")
            .is_some_and(|layout| layout.pending.is_empty())
    );
    assert!(!sessions_dir.join("0000000000000001-home").exists());
}

#[test]
fn a_tree_that_cannot_move_stays_pending_and_is_retried() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let free = persist_test_session(&store);
    let held = persist_test_session(&store);
    drop(store);
    demote_to_v2(&data, &[("0000000000000001-home", vec![free, held])]);
    let sessions_dir = sessions(&data);
    let held_dir = sessions_dir
        .join("0000000000000001-home")
        .join(held.to_string());
    let lock = match crate::ownership::try_acquire(&held_dir).expect("lock") {
        crate::ownership::SessionOwnership::Owned(lock) => lock,
        crate::ownership::SessionOwnership::Foreign => panic!("the tree is free"),
    };

    let store = open_store(&data);
    let pending = store.pending_trees().to_vec();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].root_session_id, held);
    assert_eq!(
        sessions_dir.join(&pending[0].path),
        held_dir,
        "the tree stays where it was"
    );
    assert!(held_dir.join(EVENTS_FILE).is_file());
    assert_eq!(
        read_layout(&sessions_dir)
            .expect("layout")
            .expect("marker")
            .pending,
        pending
    );
    assert_eq!(
        listed_ids(&store.list_roots()),
        vec![free],
        "a pending tree is not listed"
    );
    drop(store);

    drop(lock);
    let store = open_store(&data);
    assert!(store.pending_trees().is_empty());
    assert_eq!(listed_ids(&store.list_roots()), sorted(vec![free, held]));
    assert_eq!(store.session_dir(held), root_dir_in(&sessions_dir, held));
    assert!(!sessions_dir.join("0000000000000001-home").exists());
}

#[test]
fn the_migration_converts_older_history_and_drops_the_originals() {
    let tree = format_zero_tree();
    demote_to_v2(&tree.data, &[("0000000000000001-home", vec![tree.root])]);
    let store = SessionStore::open(&tree.data, &tree.cwd).expect("migrated store");
    let root_dir = store.session_dir(tree.root);
    for dir in [
        root_dir.clone(),
        root_dir.join(SUBAGENTS_DIR).join(tree.child.to_string()),
    ] {
        let first = fs::read_to_string(dir.join(EVENTS_FILE))
            .expect("log")
            .lines()
            .next()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("line"))
            .expect("first line");
        assert_eq!(
            first["payload"]["history_format"],
            serde_json::json!(cookie_agent_protocol::HISTORY_FORMAT)
        );
        assert!(staged_files(&dir).is_empty(), "{:?}", staged_files(&dir));
    }
    assert!(
        !sessions(&tree.data)
            .join(super::super::layout::BACKUP_DIR)
            .exists()
    );
}

/// Writes `lines` as the journal of `month` under `data`.
fn write_journal(data: &Path, month: &str, lines: &[serde_json::Value]) {
    let dir = sessions(data).join(ACTIVITY_DIR);
    create_private_test_dir_all(&dir);
    let text = lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    fs::write(dir.join(format!("{month}.jsonl")), text).expect("journal");
}

/// The list entry of a fresh root, `title`d and last active at `time`.
fn entry(
    template: &cookie_agent_protocol::SessionMeta,
    title: &str,
    time: &str,
) -> serde_json::Value {
    let mut meta = template.clone();
    meta.session_id = SessionId::new_v7();
    meta.title = Some(SessionTitle::new(title).expect("title"));
    meta.last_activity = at(time);
    serde_json::to_value(meta).expect("entry")
}

fn retitled(entry: &serde_json::Value, title: &str, time: &str) -> serde_json::Value {
    let mut entry = entry.clone();
    entry["title"] = serde_json::json!(title);
    entry["last_activity"] = serde_json::json!(time);
    entry
}

fn tombstone(entry: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({"session_id": entry["session_id"], "deleted": true})
}

fn id_of(entry: &serde_json::Value) -> SessionId {
    entry["session_id"]
        .as_str()
        .expect("id")
        .parse()
        .expect("session id")
}

/// A current store with no sessions and no journal, and a list entry to
/// copy.
fn empty_store_and_template() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    cookie_agent_protocol::SessionMeta,
) {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let template = store
        .get(persist_test_session(&store))
        .expect("template")
        .meta
        .clone();
    drop(store);
    for entry in fs::read_dir(sessions(&data)).expect("sessions").flatten() {
        if entry.path().is_dir() {
            fs::remove_dir_all(entry.path()).expect("empty the store");
        }
    }
    (temporary, data, template)
}

#[test]
fn the_newest_entry_wins_and_tombstones_hide() {
    let (_temporary, data, template) = empty_store_and_template();
    let kept = entry(&template, "old title", "2025-01-10T00:00:00Z");
    let deleted = entry(&template, "deleted", "2025-01-11T00:00:00Z");
    let retitled_twice = retitled(&kept, "newer title", "2025-02-01T00:00:00Z");
    write_journal(&data, "2025-01", &[kept.clone(), deleted.clone()]);
    write_journal(
        &data,
        "2025-02",
        &[
            retitled_twice,
            retitled(&kept, "newest title", "2025-02-02T00:00:00Z"),
        ],
    );
    write_journal(&data, "2025-03", &[tombstone(&deleted)]);

    let store = open_store(&data);
    let list = store.list_roots();
    assert_eq!(listed_ids(&list), vec![id_of(&kept)]);
    assert_eq!(
        list[0]
            .title
            .as_ref()
            .map(|title| title.as_str().to_owned()),
        Some("newest title".to_owned())
    );
}

#[test]
fn a_window_lists_recent_roots_and_pages_older_by_month() {
    let (_temporary, data, template) = empty_store_and_template();
    let january = entry(&template, "january", "2025-01-15T00:00:00Z");
    let march = entry(&template, "march", "2025-03-10T00:00:00Z");
    let april = entry(&template, "april", "2025-04-20T00:00:00Z");
    // Active in April again, though first journaled in January: it pages
    // with April.
    let revived = entry(&template, "revived", "2025-01-02T00:00:00Z");
    write_journal(&data, "2025-01", &[january.clone(), revived.clone()]);
    write_journal(&data, "2025-03", std::slice::from_ref(&march));
    write_journal(
        &data,
        "2025-04",
        &[
            april.clone(),
            retitled(&revived, "revived", "2025-04-02T00:00:00Z"),
        ],
    );
    let store = open_store(&data);
    let none = HashSet::new();

    let recent = store.list_roots_in(
        ListWindow {
            active_since: Some(at("2025-04-01T00:00:00Z")),
            before: None,
        },
        &none,
    );
    // A window that starts mid-month pages the rest of that month, and
    // passes over a part of a month that holds nothing.
    let mid_april = store.list_roots_in(
        ListWindow {
            active_since: Some(at("2025-04-10T00:00:00Z")),
            before: None,
        },
        &none,
    );
    assert_eq!(listed_ids(&mid_april.sessions), vec![id_of(&april)]);
    let rest_of_april = store.list_roots_in(
        ListWindow {
            active_since: None,
            before: mid_april.older,
        },
        &none,
    );
    assert_eq!(listed_ids(&rest_of_april.sessions), vec![id_of(&revived)]);
    let past_an_empty_part = store.list_roots_in(
        ListWindow {
            active_since: None,
            before: Some(at("2025-03-02T00:00:00Z")),
        },
        &none,
    );
    assert_eq!(
        listed_ids(&past_an_empty_part.sessions),
        vec![id_of(&january)],
        "March before the 2nd holds nothing, nor does February"
    );
    assert_eq!(
        listed_ids(&recent.sessions),
        sorted(vec![id_of(&april), id_of(&revived)])
    );
    assert_eq!(recent.older, Some(at("2025-04-01T00:00:00Z")));

    let page = store.list_roots_in(
        ListWindow {
            active_since: None,
            before: recent.older,
        },
        &none,
    );
    assert_eq!(listed_ids(&page.sessions), vec![id_of(&march)]);
    assert_eq!(page.older, Some(at("2025-03-01T00:00:00Z")));

    // February has no journal: the next page is January's.
    let last = store.list_roots_in(
        ListWindow {
            active_since: None,
            before: page.older,
        },
        &none,
    );
    assert_eq!(listed_ids(&last.sessions), vec![id_of(&january)]);
    assert_eq!(last.older, None, "nothing is older");

    let everything = store.list_roots_in(ListWindow::default(), &none);
    assert_eq!(everything.sessions.len(), 4);
    assert_eq!(everything.older, None);
}

#[test]
fn unread_and_busy_roots_are_listed_outside_the_window() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let unread = persist_test_session(&store);
    let busy = persist_test_session(&store);
    let neither = persist_test_session(&store);
    assert_eq!(store.set_unread(&[unread], true), vec![unread]);
    drop(store);

    let cold = open_store(&data);
    // A window that starts next month reads no journal at all.
    let next_month = Timestamp::now()
        .checked_add(jiff::SignedDuration::from_hours(24 * 40))
        .expect("forty days on");
    let list = cold.list_roots_in(
        ListWindow {
            active_since: Some(next_month),
            before: None,
        },
        &HashSet::from([busy]),
    );
    assert_eq!(listed_ids(&list.sessions), sorted(vec![unread, busy]));
    assert!(!listed_ids(&list.sessions).contains(&neither));
    for id in [unread, busy] {
        assert_eq!(cold.log_open_count(id), 0, "read from its metadata alone");
    }
}

#[test]
fn compaction_keeps_each_sessions_last_line_once_a_new_month_starts() {
    let (_temporary, data, template) = empty_store_and_template();
    let previous = Month::of(Timestamp::now()).previous().to_string();
    let renamed = entry(&template, "first", "2025-01-01T00:00:00Z");
    let deleted = entry(&template, "deleted", "2025-01-01T00:00:00Z");
    let lines = [
        renamed.clone(),
        deleted.clone(),
        retitled(&renamed, "second", "2025-01-02T00:00:00Z"),
        tombstone(&deleted),
        retitled(&renamed, "third", "2025-01-03T00:00:00Z"),
    ];
    write_journal(&data, &previous, &lines);

    // The first entry of this month compacts the previous month's journal.
    let store = open_store(&data);
    persist_test_session(&store);
    let compacted = fs::read_to_string(
        sessions(&data)
            .join(ACTIVITY_DIR)
            .join(format!("{previous}.jsonl")),
    )
    .expect("compacted journal")
    .lines()
    .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("line"))
    .collect::<Vec<_>>();
    assert_eq!(compacted, vec![tombstone(&deleted), lines[4].clone()]);

    // Later entries of this month leave it alone.
    let before = fs::read(
        sessions(&data)
            .join(ACTIVITY_DIR)
            .join(format!("{previous}.jsonl")),
    )
    .expect("journal");
    persist_test_session(&store);
    assert_eq!(
        fs::read(
            sessions(&data)
                .join(ACTIVITY_DIR)
                .join(format!("{previous}.jsonl"))
        )
        .expect("journal"),
        before
    );
}

#[test]
fn a_deleted_root_leaves_the_listing_through_its_tombstone() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let kept = persist_test_session(&store);
    let deleted = persist_test_session(&store);
    store
        .begin_tree_deletion(deleted)
        .expect("begin")
        .commit()
        .expect("delete");
    assert_eq!(listed_ids(&store.list_roots()), vec![kept]);
    drop(store);
    assert_eq!(listed_ids(&open_store(&data).list_roots()), vec![kept]);
}

#[test]
fn a_root_leaving_memory_journals_the_sequence_a_delete_checks() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let root = persist_test_session(&store);
    // An event outside a run that changes nothing listed is not journaled
    // when it is appended.
    store
        .append(
            root,
            None,
            test_origin(),
            EventPayload::Diagnostic {
                source: cookie_agent_protocol::DiagnosticSource::engine("test").unwrap(),
                kind: cookie_agent_protocol::DiagnosticKind::new("note").unwrap(),
                message: "a note".into(),
                count: None,
                detail: None,
            },
        )
        .expect("diagnostic");
    let last_seq = store.get(root).expect("root").meta.last_event_seq;
    assert!(store.evict(root).expect("evict"));

    let cold = open_store(&data);
    let listed = cold
        .list_roots()
        .into_iter()
        .find(|meta| meta.session_id == root)
        .expect("listed");
    assert_eq!(listed.last_event_seq, last_seq);
}

#[test]
fn a_store_without_journals_or_a_readable_marker_is_checked_again() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let root = persist_test_session(&store);
    drop(store);

    fs::remove_dir_all(sessions(&data).join(ACTIVITY_DIR)).expect("drop the journals");
    assert_eq!(listed_ids(&open_store(&data).list_roots()), vec![root]);

    fs::write(sessions(&data).join(LAYOUT_MARKER_FILE), b"{not json").expect("garble");
    fs::remove_dir_all(sessions(&data).join(ACTIVITY_DIR)).expect("drop the journals");
    assert_eq!(listed_ids(&open_store(&data).list_roots()), vec![root]);
    assert!(
        read_layout(&sessions(&data))
            .expect("a readable marker again")
            .is_some_and(|layout| layout.version == LAYOUT_VERSION)
    );
}

/// Rewrites the tree at `root_dir` as history format 1 wrote it: whole
/// values in every log, the artifacts in `artifacts/`, no `blobs/`.
fn demote_to_format_one(root_dir: &Path) {
    use crate::events::blobs::{self, BLOBS_DIR, LEGACY_ARTIFACTS_DIR};
    let blob_dir = root_dir.join(BLOBS_DIR);
    let mut logs = vec![root_dir.join(EVENTS_FILE)];
    if let Ok(children) = fs::read_dir(root_dir.join(SUBAGENTS_DIR)) {
        logs.extend(
            children
                .flatten()
                .map(|child| child.path().join(EVENTS_FILE))
                .filter(|log| log.is_file()),
        );
    }
    for log in logs {
        let lines = fs::read_to_string(&log)
            .expect("read log")
            .lines()
            .map(|line| {
                let mut value = serde_json::from_str::<serde_json::Value>(line).expect("line");
                blobs::internalize(&mut value["payload"], &mut |digest| {
                    blobs::read(&blob_dir, digest)
                })
                .expect("whole");
                if value["payload"]["type"] == "session_created" {
                    value["payload"]["history_format"] = serde_json::json!(1);
                }
                serde_json::to_string(&value).expect("line") + "\n"
            })
            .collect::<String>();
        fs::write(&log, lines).expect("write format 1");
    }
    if blob_dir.is_dir() {
        fs::rename(&blob_dir, root_dir.join(LEGACY_ARTIFACTS_DIR)).expect("artifacts/");
    } else {
        create_private_test_dir_all(&root_dir.join(LEGACY_ARTIFACTS_DIR));
    }
}

fn first_line_format(log: &Path) -> serde_json::Value {
    let first = fs::read_to_string(log)
        .expect("log")
        .lines()
        .next()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("line"))
        .expect("first line");
    first["payload"]["history_format"].clone()
}

/// A store already in layout 3 with history format 1 converts every tree
/// once at startup, where it is: the marker records the new format only
/// after the last tree, a tree another process holds stays pending and is
/// retried, and the originals go once the marker is written.
#[test]
fn a_format_one_store_converts_every_tree_once_at_startup() {
    use crate::events::blobs::{self, BLOBS_DIR, LEGACY_ARTIFACTS_DIR};

    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let free = persist_test_session(&store);
    let held = persist_test_session(&store);
    let commands = (0..20)
        .map(|index| cookie_agent_protocol::AcpCommand {
            name: format!("command-{index}"),
            description: "a command the agent offers".into(),
            input_hint: None,
        })
        .collect::<Vec<_>>();
    for session in [free, held] {
        store
            .append(
                session,
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::AcpSessionUpdated {
                    update: cookie_agent_protocol::AcpSessionUpdate {
                        commands: Some(commands.clone()),
                        ..Default::default()
                    },
                },
            )
            .expect("commands");
    }
    drop(store);
    let sessions_dir = sessions(&data);
    let artifact = b"an artifact of format 1".to_vec();
    for root in [free, held] {
        let dir = root_dir_in(&sessions_dir, root);
        demote_to_format_one(&dir);
        fs::write(
            dir.join(LEGACY_ARTIFACTS_DIR)
                .join(blobs::digest(&artifact)),
            &artifact,
        )
        .expect("an artifact");
    }
    fs::write(
        sessions_dir.join(LAYOUT_MARKER_FILE),
        r#"{"version": 3, "history_format": 1, "pending": []}"#,
    )
    .expect("a format-1 marker");
    let held_dir = root_dir_in(&sessions_dir, held);
    let lock = match crate::ownership::try_acquire(&held_dir).expect("lock") {
        crate::ownership::SessionOwnership::Owned(lock) => lock,
        crate::ownership::SessionOwnership::Foreign => panic!("the tree is free"),
    };

    let store = open_store(&data);
    let free_dir = root_dir_in(&sessions_dir, free);
    assert_eq!(
        first_line_format(&free_dir.join(EVENTS_FILE)),
        serde_json::json!(cookie_agent_protocol::HISTORY_FORMAT)
    );
    assert!(
        fs::read_to_string(free_dir.join(EVENTS_FILE))
            .expect("log")
            .contains("\"commands_blob\":\"artifact://sha256/")
    );
    assert!(!free_dir.join(LEGACY_ARTIFACTS_DIR).exists());
    assert_eq!(
        fs::read(free_dir.join(BLOBS_DIR).join(blobs::digest(&artifact))).expect("moved"),
        artifact
    );
    assert!(
        !free_dir.join(format!("{EVENTS_FILE}.v1.bak")).exists(),
        "the original went to the backups, removed after the commit point"
    );
    assert!(!sessions_dir.join(super::super::layout::BACKUP_DIR).exists());
    let pending = store.pending_trees().to_vec();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].root_session_id, held);
    assert_eq!(sessions_dir.join(&pending[0].path), held_dir);
    assert_eq!(first_line_format(&held_dir.join(EVENTS_FILE)), 1);
    let layout = read_layout(&sessions_dir).expect("layout").expect("marker");
    assert_eq!(layout.history_format, cookie_agent_protocol::HISTORY_FORMAT);
    assert_eq!(layout.pending, pending);
    // The converted session reads whole.
    let read = store.get(free).expect("the converted root");
    assert!(read.log.event_snapshot().iter().any(|event| matches!(
        &event.payload,
        EventPayload::AcpSessionUpdated { update } if update.commands.as_ref() == Some(&commands)
    )));
    drop(read);
    drop(store);

    drop(lock);
    let store = open_store(&data);
    assert!(store.pending_trees().is_empty());
    assert_eq!(
        first_line_format(&held_dir.join(EVENTS_FILE)),
        serde_json::json!(cookie_agent_protocol::HISTORY_FORMAT)
    );
    assert!(!held_dir.join(LEGACY_ARTIFACTS_DIR).exists());
    assert_eq!(listed_ids(&store.list_roots()), sorted(vec![free, held]));
}

/// A conversion a crash cut short, its original still beside the converted
/// log, goes on at the next start, which backs up and removes the original.
#[test]
fn a_conversion_cut_short_by_a_crash_goes_on_at_the_next_start() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let root = persist_test_session(&store);
    drop(store);
    let sessions_dir = sessions(&data);
    let dir = root_dir_in(&sessions_dir, root);
    demote_to_format_one(&dir);
    let original = fs::read(dir.join(EVENTS_FILE)).expect("format 1");
    // The tree converted, but the marker was not written before the crash.
    super::super::convert::convert_tree(root, &dir).expect("convert");
    assert!(dir.join(format!("{EVENTS_FILE}.v1.bak")).is_file());
    fs::write(
        sessions_dir.join(LAYOUT_MARKER_FILE),
        r#"{"version": 3, "history_format": 1, "pending": []}"#,
    )
    .expect("a format-1 marker");
    let converted = fs::read(dir.join(EVENTS_FILE)).expect("format 2");
    assert_ne!(converted, original);

    drop(open_store(&data));
    assert_eq!(fs::read(dir.join(EVENTS_FILE)).expect("log"), converted);
    assert!(!dir.join(format!("{EVENTS_FILE}.v1.bak")).exists());
    assert_eq!(
        read_layout(&sessions_dir)
            .expect("layout")
            .expect("marker")
            .history_format,
        cookie_agent_protocol::HISTORY_FORMAT
    );
}

/// A format-2 tree whose log holds a tool result with an image in `raw_output`:
/// its log, the image's digest, and the digest of the blob holding the base64.
fn tree_with_a_stored_image(dir: &Path) -> (String, String) {
    use base64::Engine as _;
    let log = dir.join(EVENTS_FILE);
    let text = fs::read_to_string(&log).expect("log");
    let mut lines = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a line"))
        .collect::<Vec<_>>();
    lines[0]["payload"]["history_format"] = serde_json::json!(2);
    let image = vec![5_u8; 64 * 1024];
    let image_digest = blobs::digest(&image);
    let folder = dir.join(blobs::BLOBS_DIR);
    fs::create_dir_all(&folder).expect("blobs");
    fs::write(folder.join(&image_digest), &image).expect("the image");
    let mut payload = serde_json::json!({
        "type": "tool_call_terminated",
        "tool_call_id": "a-call",
        "result": {
            "title": "Read",
            "output": "",
            "display": [],
            "metadata": {"acp": {"raw_output": [{"source": {
                "data": base64::engine::general_purpose::STANDARD.encode(&image),
            }}]}},
        },
    });
    let mut json_blob = String::new();
    blobs::externalize(&mut payload, &mut |digest, bytes| {
        json_blob = digest.to_owned();
        fs::write(folder.join(digest), bytes).expect("the blob");
    });
    let mut record = lines.last().expect("a line").clone();
    record["seq"] = serde_json::json!(record["seq"].as_u64().expect("seq") + 1);
    record["payload"] = payload;
    lines.push(record);
    let written = lines
        .iter()
        .map(|line| serde_json::to_string(line).expect("line") + "\n")
        .collect::<String>();
    fs::write(&log, written).expect("format 2");
    (image_digest, json_blob)
}

fn format_two_marker(sessions_dir: &Path) {
    fs::write(
        sessions_dir.join(LAYOUT_MARKER_FILE),
        r#"{"version": 3, "history_format": 2, "pending": []}"#,
    )
    .expect("a format-2 marker");
}

#[test]
fn a_stores_conversion_deletes_the_old_blobs_when_the_backups_go() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let root = persist_test_session(&store);
    drop(store);
    let sessions_dir = sessions(&data);
    let dir = root_dir_in(&sessions_dir, root);
    let (image, json_blob) = tree_with_a_stored_image(&dir);
    format_two_marker(&sessions_dir);

    drop(open_store(&data));

    let folder = dir.join(blobs::BLOBS_DIR);
    assert!(folder.join(&image).is_file(), "the image stays");
    assert!(!folder.join(&json_blob).exists(), "its base64 goes");
    let log = fs::read_to_string(dir.join(EVENTS_FILE)).expect("log");
    assert!(log.contains(&format!("artifact://sha256/{image}")));
    assert!(!log.contains(&json_blob));
    assert!(
        !dir.join(super::super::convert::OBSOLETE_BLOBS_FILE)
            .exists()
    );
    assert!(!sessions_dir.join(super::super::layout::BACKUP_DIR).exists());
}

#[test]
fn old_blobs_stay_while_a_backup_beside_the_log_needs_them() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let root = persist_test_session(&store);
    drop(store);
    let dir = root_dir_in(&sessions(&data), root);
    let (_, json_blob) = tree_with_a_stored_image(&dir);

    super::super::convert::convert_tree(root, &dir).expect("convert");
    let backup = dir.join(format!("{EVENTS_FILE}.v2.bak"));
    assert!(backup.is_file());
    let folder = dir.join(blobs::BLOBS_DIR);
    super::super::convert::remove_obsolete_blobs(&dir).expect("removes nothing");
    assert!(folder.join(&json_blob).is_file(), "the backup names it");
    // The backup can still be read whole.
    assert!(
        fs::read_to_string(&backup)
            .expect("backup")
            .contains(&json_blob)
    );

    // Once the user deleted it, the blob can go.
    fs::remove_file(&backup).expect("the user deletes the backup");
    super::super::convert::remove_obsolete_blobs(&dir).expect("removes it");
    assert!(!folder.join(&json_blob).exists());
}

#[test]
fn an_old_blob_a_log_still_names_stays() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let root = persist_test_session(&store);
    drop(store);
    let dir = root_dir_in(&sessions(&data), root);
    let (_, json_blob) = tree_with_a_stored_image(&dir);
    super::super::convert::convert_tree(root, &dir).expect("convert");
    fs::remove_file(dir.join(format!("{EVENTS_FILE}.v2.bak"))).expect("no backup");
    // Something named it since the conversion, as a record or anything else.
    let log = dir.join(EVENTS_FILE);
    let mut text = fs::read_to_string(&log).expect("log");
    let last = text.lines().last().expect("a line").to_owned();
    let mut record = serde_json::from_str::<serde_json::Value>(&last).expect("line");
    record["seq"] = serde_json::json!(record["seq"].as_u64().expect("seq") + 1);
    record["payload"] = serde_json::json!({"type": "from_a_newer_build", "names": json_blob});
    text.push_str(&(serde_json::to_string(&record).expect("line") + "\n"));
    fs::write(&log, text).expect("append");

    super::super::convert::remove_obsolete_blobs(&dir).expect("removes nothing");

    assert!(dir.join(blobs::BLOBS_DIR).join(&json_blob).is_file());
}

#[test]
fn a_conversion_is_read_only_until_a_tree_is_taken_and_reads_the_same() {
    let temporary = private_tempdir();
    let data = temporary.path().join("data");
    let store = open_store(&data);
    let root = persist_test_session(&store);
    drop(store);
    let dir = root_dir_in(&sessions(&data), root);
    let (image, json_blob) = tree_with_a_stored_image(&dir);
    let before = fs::read(dir.join(EVENTS_FILE)).expect("log");

    let log = crate::events::EventLog::open_read_only(dir.join(EVENTS_FILE), root)
        .expect("a format-2 log opens read-only");

    assert!(log.event_snapshot().len() > 1);
    assert_eq!(fs::read(dir.join(EVENTS_FILE)).expect("log"), before);
    let folder = dir.join(blobs::BLOBS_DIR);
    assert!(folder.join(&image).is_file() && folder.join(&json_blob).is_file());
}
